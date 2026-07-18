//! Prioritization + live re-planning (Phase 2.3).
//!
//! A **deterministic, explainable** ranker over the day's actionable units. Every
//! rank carries a templated "why this rank" rationale built from TYPED factor
//! slots — no synthesized prose, and the ordering is never delegated to a model
//! (v1 rationale discipline). Same inputs → same order (total tie-break); adding
//! or removing one item never reshuffles the others (each item's sort key
//! depends only on its own data).
//!
//! **Two relevance systems, stated (Phase 2.2 note).** Candidates come from BOTH
//! (a) the correlation engine — work threads (ask ↔ Jira issue ↔ commit), which
//! carry evidence/completeness signal — and (b) the v1 classifier — non-noise
//! extracted items (action-needed / commitment / calendar event) NOT already
//! represented by a thread's ask, which carry the classifier-kind signal. They
//! COMBINE additively in the score; correlation is NOT re-gated on the classifier
//! label (that regression is exactly what the 2.2 live gate caught).
//!
//! **Re-planning is explicit and audited.** `prioritize` and `load_candidates`
//! are pure reads (the app's plan view). `replan_cycle` is the triggered refresh:
//! it idempotently queues proposals (no duplicates, no resurrected rejections)
//! and appends a traceable audit record (actor + reason) — no silent mutation.

use std::collections::HashSet;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;
use serde_json::Value;

use crate::act::draft::TemplatedDraftingBackend;
use crate::correlate::{self, CorrelationEngine, QueueOutcome, WorkThread};
use crate::extract::ItemKind;
use crate::types::SourceId;

// ------------------------------------------------------- scoring model -----
//
// Fixed, documented weights (the report lists these). Deterministic integers so
// scores compare exactly and ties are explicit.

const W_THREAD_FULL: i32 = 50; // ask + item + commit — ready to act now
const W_THREAD_PARTIAL: i32 = 20; // possible match — needs your confirmation
const W_ASK: i32 = 25; // an ask awaiting a reply (no work item)
const W_EVENT: i32 = 10; // a scheduled event (the deadline factor dominates)

const W_DEADLINE_IMMINENT: i32 = 40; // upcoming within 4h
const W_DEADLINE_TODAY: i32 = 30;
const W_DEADLINE_SOON: i32 = 15; // within 3 days
const W_DEADLINE_LATER: i32 = 5;
const W_DEADLINE_PAST: i32 = 0; // a calendar event that already happened is done

const W_HARD_EVIDENCE: i32 = 15; // a commit backs the work (Tier-Hard)
const W_KIND_ACTION: i32 = 10; // v1 classifier: action-needed
const W_KIND_COMMITMENT: i32 = 8; // v1 classifier: a commitment you made
const W_STALENESS_PER_DAY: i32 = 1; // +1/day unaddressed …
const W_STALENESS_CAP: i32 = 15; // … capped
const W_ASKER: i32 = 20; // from a configured priority sender

const SECTION_DO_NOW: i32 = 65;
const SECTION_BY_EOD: i32 = 40;

// -------------------------------------------------------------- types ------

pub type PlanKey = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    WorkThreadFull,
    WorkThreadPartial,
    CalendarEvent,
    Ask,
}

/// One normalized, rankable unit. Loaders build these from work threads and
/// extracted items; the prioritizer scores them without touching the DB.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub key: PlanKey,
    pub title: String,
    pub kind: CandidateKind,
    pub has_hard_evidence: bool,
    pub classifier_kind: Option<ItemKind>,
    /// Event start / due time (drives the deadline factor); None for plain asks.
    pub deadline: Option<DateTime<Utc>>,
    /// When the underlying ask/event occurred (staleness + tie-break). For a
    /// multi-ask thread this is the OLDEST ask — the one waiting longest.
    pub occurred_at: DateTime<Utc>,
    /// Every asker on the candidate (a thread may bind several asks). The
    /// asker-weight fires if ANY of them is a configured priority sender.
    pub askers: Vec<String>,
    pub deep_link: String,
}

/// One weighted contribution to a candidate's score, with a templated reason.
#[derive(Debug, Clone)]
pub struct Factor {
    pub name: &'static str,
    pub points: i32,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    DoNow,
    ByEod,
    CanWait,
}

impl Section {
    pub fn as_str(&self) -> &'static str {
        match self {
            Section::DoNow => "do now",
            Section::ByEod => "by EOD",
            Section::CanWait => "can wait",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RankedItem {
    pub candidate: Candidate,
    pub score: i32,
    pub factors: Vec<Factor>,
    pub section: Section,
    /// Templated "why this rank" (typed factor slots only).
    pub rationale: String,
    /// 1-based position in the plan.
    pub rank: usize,
}

#[derive(Debug, Clone, Default)]
pub struct PriorityConfig {
    /// Case-insensitive substrings; an asker containing one gets the asker bonus.
    pub important_senders: Vec<String>,
}

impl PriorityConfig {
    /// Priority senders from `ALMANAC_PRIORITY_SENDERS` (comma-separated).
    pub fn from_env() -> Self {
        let important_senders = std::env::var("ALMANAC_PRIORITY_SENDERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Self { important_senders }
    }

    fn is_important(&self, asker: &str) -> bool {
        let a = asker.to_ascii_lowercase();
        self.important_senders.iter().any(|s| !s.is_empty() && a.contains(&s.to_ascii_lowercase()))
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub items: Vec<RankedItem>,
    /// Templated one-line "expected EOD" summary.
    pub summary: String,
    pub generated_at: DateTime<Utc>,
}

// ---------------------------------------------------------- prioritize -----

/// Rank candidates deterministically. Pure — no DB, no clock beyond `now`.
pub fn prioritize(candidates: &[Candidate], config: &PriorityConfig, now: DateTime<Utc>) -> Plan {
    let mut items: Vec<RankedItem> = candidates
        .iter()
        .map(|c| {
            let (score, factors) = score_candidate(c, config, now);
            let section = section_of(c, score, now);
            RankedItem { candidate: c.clone(), score, factors, section, rationale: String::new(), rank: 0 }
        })
        .collect();

    // Total, deterministic order: score desc, then earlier deadline, then older
    // (staler) first, then key — so equal scores never depend on input order and
    // adding one item cannot reshuffle the rest.
    items.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| deadline_key(&a.candidate).cmp(&deadline_key(&b.candidate)))
            .then_with(|| a.candidate.occurred_at.cmp(&b.candidate.occurred_at))
            .then_with(|| a.candidate.key.cmp(&b.candidate.key))
    });

    for (i, item) in items.iter_mut().enumerate() {
        item.rank = i + 1;
        item.rationale = render_rationale(item);
    }

    let count = |s: Section| items.iter().filter(|i| i.section == s).count();
    let (n_now, n_eod, n_wait) = (count(Section::DoNow), count(Section::ByEod), count(Section::CanWait));
    let top = items
        .first()
        .map(|i| format!(" Start with: {}.", i.candidate.title))
        .unwrap_or_default();
    let summary = format!(
        "Plan: {n_now} to do now, {n_eod} by end of day, {n_wait} can wait.{top}"
    );

    Plan { items, summary, generated_at: now }
}

fn deadline_key(c: &Candidate) -> i64 {
    c.deadline.map(|d| d.timestamp()).unwrap_or(i64::MAX)
}

fn score_candidate(c: &Candidate, config: &PriorityConfig, now: DateTime<Utc>) -> (i32, Vec<Factor>) {
    let mut factors = Vec::new();

    let (act_pts, act_reason) = match c.kind {
        CandidateKind::WorkThreadFull => {
            (W_THREAD_FULL, "ready to act — ask, work item, and commit evidence all present")
        }
        CandidateKind::WorkThreadPartial => {
            (W_THREAD_PARTIAL, "possible match — needs your confirmation")
        }
        CandidateKind::Ask => (W_ASK, "an ask awaiting a reply"),
        CandidateKind::CalendarEvent => (W_EVENT, "a scheduled event"),
    };
    factors.push(Factor { name: "actionability", points: act_pts, reason: act_reason.into() });

    if let Some(dl) = c.deadline {
        // A calendar event is a point in time: once it's PAST it's done, not
        // urgent (unlike a task "due-by", which we don't model). Only UPCOMING
        // deadlines earn urgency.
        let (pts, reason) = if dl < now {
            (W_DEADLINE_PAST, "already happened".to_string())
        } else if dl <= now + Duration::hours(4) {
            (W_DEADLINE_IMMINENT, "starts within the hour".to_string())
        } else if same_local_day(dl, now) {
            (W_DEADLINE_TODAY, "later today".to_string())
        } else if dl <= now + Duration::days(3) {
            (W_DEADLINE_SOON, "in the next few days".to_string())
        } else {
            (W_DEADLINE_LATER, "scheduled later".to_string())
        };
        factors.push(Factor { name: "deadline", points: pts, reason });
    }

    if c.has_hard_evidence {
        factors.push(Factor {
            name: "evidence",
            points: W_HARD_EVIDENCE,
            reason: "backed by a commit (Tier-Hard)".into(),
        });
    }

    match c.classifier_kind {
        Some(ItemKind::ActionNeeded) => factors.push(Factor {
            name: "classifier",
            points: W_KIND_ACTION,
            reason: "classified as action-needed".into(),
        }),
        Some(ItemKind::Commitment) => factors.push(Factor {
            name: "classifier",
            points: W_KIND_COMMITMENT,
            reason: "a commitment you made".into(),
        }),
        _ => {}
    }

    // Staleness applies to asks/threads (things awaiting YOUR reply), not events.
    if !matches!(c.kind, CandidateKind::CalendarEvent) {
        let age_days = ((now - c.occurred_at).num_hours() / 24).max(0) as i32;
        if age_days > 0 {
            let pts = (age_days * W_STALENESS_PER_DAY).min(W_STALENESS_CAP);
            factors.push(Factor {
                name: "staleness",
                points: pts,
                reason: format!("unaddressed for {age_days} day(s)"),
            });
        }
    }

    if c.askers.iter().any(|a| config.is_important(a)) {
        factors.push(Factor {
            name: "asker",
            points: W_ASKER,
            reason: "from a priority sender".into(),
        });
    }

    let score = factors.iter().map(|f| f.points).sum();
    (score, factors)
}

fn section_of(c: &Candidate, score: i32, now: DateTime<Utc>) -> Section {
    if let Some(dl) = c.deadline {
        if dl >= now && dl <= now + Duration::hours(4) {
            return Section::DoNow; // an UPCOMING imminent event is always do-now
        }
        if dl >= now && same_local_day(dl, now) {
            return Section::ByEod; // still to come today
        }
        // A past event falls through to the score-based sections (score is low).
    }
    if score >= SECTION_DO_NOW {
        Section::DoNow
    } else if score >= SECTION_BY_EOD {
        Section::ByEod
    } else {
        Section::CanWait
    }
}

fn same_local_day(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    a.with_timezone(&chrono::Local).date_naive() == b.with_timezone(&chrono::Local).date_naive()
}

fn render_rationale(item: &RankedItem) -> String {
    let factors = item
        .factors
        .iter()
        .map(|f| format!("{} +{} ({})", f.name, f.points, f.reason))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "#{} · {} · score {} = {}",
        item.rank,
        item.section.as_str(),
        item.score,
        factors
    )
}

// -------------------------------------------------------- db loaders -------

/// Build candidates from already-computed threads + the non-noise extracted
/// items not already represented by a thread's ask (so nothing is double-ranked).
pub fn build_candidates(
    conn: &Connection,
    threads: &[WorkThread],
    _now: DateTime<Utc>,
) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    let mut thread_ask_ids: HashSet<(String, String)> = HashSet::new();

    for t in threads {
        let kind = if t.is_full() {
            CandidateKind::WorkThreadFull
        } else {
            CandidateKind::WorkThreadPartial
        };
        // Aggregate across ALL bound asks: any asker can trip the priority-sender
        // weight, and staleness is measured from the OLDEST ask (waiting longest).
        let askers: Vec<String> = t.asks.iter().filter_map(|a| a.asker.clone()).collect();
        let occurred_at = t
            .asks
            .iter()
            .map(|a| a.occurred_at)
            .min()
            .unwrap_or(t.item.occurred_at);
        for a in &t.asks {
            thread_ask_ids.insert((a.provenance.source.to_string(), a.provenance.native_id.clone()));
        }
        candidates.push(Candidate {
            key: format!("thread:{}", t.item.key),
            title: format!("{}: {}", t.item.key, t.item.summary),
            kind,
            has_hard_evidence: !t.evidence.is_empty(),
            classifier_kind: None,
            deadline: None,
            occurred_at,
            askers,
            deep_link: t.item.deep_link.clone(),
        });
    }

    let mut stmt = conn.prepare(
        "SELECT ei.source, ei.native_id, ei.kind, ei.summary, so.deep_link, so.occurred_at, so.raw_json
         FROM extracted_items ei
         JOIN source_objects so ON so.source = ei.source AND so.native_id = ei.native_id
         WHERE ei.source IN ('gmail', 'slack', 'gcal') AND ei.kind != 'noise'
           AND ei.id = (SELECT MAX(id) FROM extracted_items
                        WHERE source = ei.source AND native_id = ei.native_id)",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
        ))
    })?;
    for row in rows {
        let (source, native_id, kind, summary, deep_link, occurred_at, raw_json) = row?;
        if thread_ask_ids.contains(&(source.clone(), native_id.clone())) {
            continue; // already ranked as a work thread
        }
        let src = SourceId::parse(&source).with_context(|| format!("unknown source '{source}'"))?;
        let item_kind = ItemKind::parse(&kind).with_context(|| format!("unknown kind '{kind}'"))?;
        let occ = parse_utc(&occurred_at)?;
        // Only a real CALENDAR event (source = gcal) carries a start-time
        // deadline. A Gmail/Slack message the classifier merely labeled "event"
        // (e.g. a "1 event happening tomorrow" notification email) is not a
        // calendar entry — its occurred_at is a receipt time, not a deadline —
        // so it is treated as a plain ask with no deadline factor.
        let is_calendar_event = item_kind == ItemKind::Event && src == SourceId::GoogleCalendar;
        let (cand_kind, deadline) = if is_calendar_event {
            (CandidateKind::CalendarEvent, Some(occ))
        } else {
            (CandidateKind::Ask, None)
        };
        let askers = asker_of(src, &raw_json).into_iter().collect();
        candidates.push(Candidate {
            key: format!("{source}:{native_id}"),
            title: summary,
            kind: cand_kind,
            has_hard_evidence: false,
            classifier_kind: Some(item_kind),
            deadline,
            occurred_at: occ,
            askers,
            deep_link,
        });
    }
    Ok(candidates)
}

/// Read-only plan view: correlate, then rank. No queueing, no audit (the app's
/// passive display uses this; `replan_cycle` is the explicit refresh). J15
/// self-comment exclusion uses the accountId cached by the last online Jira step
/// so the displayed correlation matches what a re-plan would queue.
pub fn load_candidates(conn: &Connection, now: DateTime<Utc>) -> Result<Vec<Candidate>> {
    let asks = correlate::load_asks(conn)?;
    let items = correlate::load_work_items(conn)?;
    let commits = correlate::load_commits(conn)?;
    let self_id = crate::db::get_meta(conn, crate::db::JIRA_SELF_ACCOUNT_ID)?;
    let threads = CorrelationEngine::new(self_id).correlate(&asks, &items, &commits);
    build_candidates(conn, &threads, now)
}

fn asker_of(source: SourceId, raw_json: &str) -> Option<String> {
    let raw: Value = serde_json::from_str(raw_json).ok()?;
    match source {
        SourceId::Gmail => {
            let payload = raw.get("payload").cloned().unwrap_or(Value::Null);
            crate::adapters::gmail::header_values(&payload, "From").first().map(|s| s.to_string())
        }
        SourceId::Slack => raw.get("user").and_then(Value::as_str).map(String::from),
        _ => None,
    }
}

fn parse_utc(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("bad timestamp '{s}'"))?
        .with_timezone(&Utc))
}

// -------------------------------------------------------- re-planning ------

#[derive(Debug)]
pub struct ReplanReport {
    pub outcome: QueueOutcome,
    pub plan: Plan,
    pub summary: String,
    pub audit_seq: i64,
}

/// One triggered re-plan cycle: idempotently queue proposals for the current
/// threads, rank the plan, and append a traceable audit record (actor + reason).
/// Read-only w.r.t. external services — the only writes are local queue rows a
/// human still approves. Uses ONE correlation pass for both queueing and ranking.
pub fn replan_cycle(
    conn: &mut Connection,
    config: &PriorityConfig,
    self_account_id: Option<String>,
    actor: &str,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<ReplanReport> {
    let asks = correlate::load_asks(conn)?;
    let items = correlate::load_work_items(conn)?;
    let commits = correlate::load_commits(conn)?;
    // J15 self-exclusion carries through the re-plan's correlation, same as the
    // standalone `correlate` path.
    let threads = CorrelationEngine::new(self_account_id).correlate(&asks, &items, &commits);

    let outcome = correlate::queue_proposals(
        conn,
        &threads,
        &TemplatedDraftingBackend,
        "correlation",
        Duration::hours(24),
    )?;

    let candidates = build_candidates(conn, &threads, now)?;
    let plan = prioritize(&candidates, config, now);

    let summary = format!(
        "reason={reason}; queued={}; skipped_dup={}; ranked={}; top={}",
        outcome.queued,
        outcome.skipped,
        plan.items.len(),
        plan.items.first().map(|i| i.candidate.key.as_str()).unwrap_or("-"),
    );
    // Every triggered cycle is on the record (actor + reason) — no silent mutation.
    let audit_seq = crate::act::record_replan(conn, actor, &summary)?;

    Ok(ReplanReport { outcome, plan, summary, audit_seq })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(key: &str, kind: CandidateKind) -> Candidate {
        Candidate {
            key: key.into(),
            title: key.into(),
            kind,
            has_hard_evidence: false,
            classifier_kind: None,
            deadline: None,
            occurred_at: DateTime::parse_from_rfc3339("2026-07-16T09:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            askers: vec![],
            deep_link: "https://example.com/x".into(),
        }
    }
    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-16T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn deterministic_same_inputs_same_order() {
        let cs = vec![
            base("a", CandidateKind::Ask),
            base("b", CandidateKind::WorkThreadFull),
            base("c", CandidateKind::CalendarEvent),
        ];
        let cfg = PriorityConfig::default();
        let p1 = prioritize(&cs, &cfg, now());
        let p2 = prioritize(&cs, &cfg, now());
        let order = |p: &Plan| p.items.iter().map(|i| i.candidate.key.clone()).collect::<Vec<_>>();
        let scores = |p: &Plan| p.items.iter().map(|i| i.score).collect::<Vec<_>>();
        assert_eq!(order(&p1), order(&p2));
        assert_eq!(scores(&p1), scores(&p2));
    }

    #[test]
    fn stability_adding_item_does_not_reshuffle_others() {
        let cfg = PriorityConfig::default();
        let a = base("a", CandidateKind::Ask);
        let b = base("b", CandidateKind::WorkThreadPartial);
        let before = prioritize(&[a.clone(), b.clone()], &cfg, now());
        let before_order: Vec<_> = before.items.iter().map(|i| i.candidate.key.clone()).collect();

        // Inject a new, higher-priority item.
        let urgent = {
            let mut c = base("urgent", CandidateKind::WorkThreadFull);
            c.has_hard_evidence = true;
            c
        };
        let after = prioritize(&[a, b, urgent], &cfg, now());
        let after_order: Vec<_> = after
            .items
            .iter()
            .map(|i| i.candidate.key.clone())
            .filter(|k| k != "urgent")
            .collect();
        assert_eq!(before_order, after_order, "existing items keep their relative order");
        assert_eq!(after.items[0].candidate.key, "urgent", "the urgent item leads");
    }

    #[test]
    fn deadline_inversion_earlier_deadline_wins() {
        let cfg = PriorityConfig::default();
        let mut today = base("today", CandidateKind::CalendarEvent);
        today.deadline = Some(now() + Duration::hours(6)); // today, not imminent
        let mut next_week = base("next_week", CandidateKind::CalendarEvent);
        next_week.deadline = Some(now() + Duration::days(7));
        let plan = prioritize(&[next_week, today], &cfg, now());
        assert_eq!(plan.items[0].candidate.key, "today", "due-today event outranks next-week");
        assert!(plan.items[0].score > plan.items[1].score);
    }

    #[test]
    fn asker_weight_promotes_priority_sender() {
        let cfg = PriorityConfig { important_senders: vec!["boss@corp.com".into()] };
        let mut vip = base("vip", CandidateKind::Ask);
        // Priority sender is the SECOND asker — the fix must scan all of them.
        vip.askers = vec!["someone@corp.com".into(), "Big Boss <boss@corp.com>".into()];
        let plain = base("plain", CandidateKind::Ask); // same kind, no asker weight
        let plan = prioritize(&[plain, vip], &cfg, now());
        assert_eq!(plan.items[0].candidate.key, "vip");
        assert!(
            plan.items[0].factors.iter().any(|f| f.name == "asker" && f.points == W_ASKER),
            "asker factor present"
        );
    }

    #[test]
    fn tie_break_is_deterministic_by_occurred_then_key() {
        let cfg = PriorityConfig::default();
        // Two identical-kind asks, no deadline → equal score → tie-break decides.
        let mut older = base("z_older", CandidateKind::Ask);
        older.occurred_at =
            DateTime::parse_from_rfc3339("2026-07-10T00:00:00Z").unwrap().with_timezone(&Utc);
        let mut newer = base("a_newer", CandidateKind::Ask);
        newer.occurred_at =
            DateTime::parse_from_rfc3339("2026-07-15T00:00:00Z").unwrap().with_timezone(&Utc);
        // Staleness differs by age, so make them equal-score by clearing staleness:
        // both older-than-now, but the older one is staler → higher score. To test
        // the KEY tie-break specifically, give them the SAME occurred_at.
        newer.occurred_at = older.occurred_at;
        let plan = prioritize(&[newer, older], &cfg, now());
        // Equal score + equal occurred_at → ordered by key ascending.
        assert_eq!(plan.items[0].candidate.key, "a_newer");
        assert_eq!(plan.items[1].candidate.key, "z_older");
    }

    #[test]
    fn rationale_is_templated_factor_slots() {
        let cfg = PriorityConfig::default();
        let mut c = base("t", CandidateKind::WorkThreadFull);
        c.has_hard_evidence = true;
        let plan = prioritize(&[c], &cfg, now());
        let r = &plan.items[0].rationale;
        assert!(r.starts_with("#1 · do now · score "));
        assert!(r.contains("actionability +50"));
        assert!(r.contains("evidence +15"));
    }

    #[test]
    fn past_calendar_event_is_not_do_now() {
        let cfg = PriorityConfig::default();
        let mut past = base("past", CandidateKind::CalendarEvent);
        past.deadline = Some(now() - Duration::days(1)); // already happened
        let plan = prioritize(&[past], &cfg, now());
        let item = &plan.items[0];
        assert_ne!(item.section, Section::DoNow, "a past event is not do-now");
        assert!(
            item.factors.iter().any(|f| f.name == "deadline" && f.points == 0),
            "a past deadline earns no urgency"
        );
    }

    #[test]
    fn upcoming_imminent_event_is_do_now() {
        let cfg = PriorityConfig::default();
        let mut soon = base("soon", CandidateKind::CalendarEvent);
        soon.deadline = Some(now() + Duration::hours(2));
        let plan = prioritize(&[soon], &cfg, now());
        assert_eq!(plan.items[0].section, Section::DoNow);
    }

    #[test]
    fn sections_split_by_deadline_and_score() {
        let cfg = PriorityConfig::default();
        let mut imminent = base("imminent", CandidateKind::CalendarEvent);
        imminent.deadline = Some(now() + Duration::hours(1));
        let low = base("low", CandidateKind::WorkThreadPartial); // score 20 → can wait
        let plan = prioritize(&[imminent, low], &cfg, now());
        let sec = |k: &str| plan.items.iter().find(|i| i.candidate.key == k).unwrap().section;
        assert_eq!(sec("imminent"), Section::DoNow);
        assert_eq!(sec("low"), Section::CanWait);
    }
}
