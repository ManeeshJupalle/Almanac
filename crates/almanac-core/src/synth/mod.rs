//! Synthesis (Phase 4): grounded `ExtractedItem`s become a `Briefing` —
//! an ordered sequence plus a human-readable rationale.
//!
//! Design for a small local model: the LLM only ever refers to items by
//! index into a numbered digest. `PlannedItem`s and their `ProvenanceRef`s
//! are constructed in code from the real items, so the model cannot invent
//! provenance. The grounding validation pass is still the hard final gate:
//! a briefing containing ANY unresolvable item is rejected, not repaired.
//!
//! Raw-content discipline: prompts are built exclusively from item summaries
//! (local distillates); raw payloads never enter a prompt. Inference is
//! in-process — nothing here can perform network I/O.

pub mod local_llm;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Timelike, Utc};
use rusqlite::Connection;

use crate::extract::{ExtractedItem, ItemKind};
use crate::types::ProvenanceRef;

/// Context the backend gets about the day being planned.
#[derive(Debug, Clone, Copy)]
pub struct DayContext {
    pub date: NaiveDate,
    pub now: DateTime<Utc>,
}

/// One step of the plan. Provenance is copied verbatim from the extracted
/// item it was built from — never synthesized.
#[derive(Debug, Clone)]
pub struct PlannedItem {
    pub provenance: ProvenanceRef,
    pub kind: ItemKind,
    pub summary: String,
    pub occurred_at: DateTime<Utc>,
}

/// INVARIANT: every PlannedItem.provenance resolves to a real SourceObject.
/// A briefing with an ungrounded item is a hard failure, not a warning.
#[derive(Debug, Clone)]
pub struct Briefing {
    pub sequence: Vec<PlannedItem>,
    pub rationale: String,
}

/// Synthesis is swappable so local-vs-alt delta can be measured honestly.
#[async_trait]
pub trait SynthesisBackend {
    fn backend_id(&self) -> &str; // "local-<model>" | "alt-..."
    async fn synthesize(&self, items: Vec<ExtractedItem>, ctx: DayContext) -> Result<Briefing>;
}

// ----------------------------------------------------------- pipeline ------

/// Generate a validated briefing for `ctx.date`, interpreted as the USER'S
/// LOCAL calendar day (Phase 6).
///
/// Run-scoping: inputs come from `db::briefing_inputs_range` — for each
/// (source, native_id) only the LATEST extraction (max rowid) is selected,
/// restricted to the local day's UTC bounds. Noise items are excluded from
/// synthesis but counted in the rationale; cross-source duplicates
/// (calendar-notification emails shadowing their own events) are suppressed
/// from selection only — everything stays stored and auditable.
pub async fn generate_briefing(
    conn: &Connection,
    backend: &dyn SynthesisBackend,
    ctx: DayContext,
) -> Result<Briefing> {
    let (start, end) = crate::db::day_bounds(ctx.date, &chrono::Local)?;
    let inputs = crate::db::briefing_inputs_range(conn, start, end)?;
    let (noise, actionable): (Vec<_>, Vec<_>) =
        inputs.into_iter().partition(|i| i.kind() == ItemKind::Noise);
    let (actionable, suppressed) = dedup_calendar_email_duplicates(conn, actionable)?;

    let mut briefing = if actionable.is_empty() {
        Briefing {
            sequence: Vec::new(),
            rationale: format!(
                "Nothing needs your attention for {} — no commitments, events, or action items surfaced.",
                ctx.date
            ),
        }
    } else {
        let briefing = backend.synthesize(actionable.clone(), ctx).await?;
        validate_briefing(conn, &briefing, &actionable)?;
        briefing
    };

    if suppressed > 0 {
        briefing.rationale.push_str(&format!(
            " ({suppressed} calendar-notification email{} suppressed as duplicate{} of briefed events.)",
            if suppressed == 1 { "" } else { "s" },
            if suppressed == 1 { "" } else { "s" },
        ));
    }
    if !noise.is_empty() {
        briefing.rationale.push_str(&format!(
            " ({} low-signal item{} classified as noise — retained in the local store, not briefed.)",
            noise.len(),
            if noise.len() == 1 { " was" } else { "s were" }
        ));
    }
    Ok(briefing)
}

/// Phase 6 cross-source dedup, deliberately HIGH-PRECISION (a wrong merge is
/// worse than a duplicate). A Gmail item is suppressed from the selection
/// only when BOTH hold:
///   1. its From header is Google Calendar's notification sender, and
///   2. its body references a briefed calendar event's id (the base64 `eid`
///      token embedded in calendar-email links), or it is a pure agenda
///      digest ("N events happening today/tomorrow") while calendar items
///      are present in the same selection.
/// Suppression affects briefing selection ONLY; both source objects and both
/// extracted items stay stored. Returns (kept, suppressed_count).
pub fn dedup_calendar_email_duplicates(
    conn: &Connection,
    items: Vec<ExtractedItem>,
) -> Result<(Vec<ExtractedItem>, usize)> {
    use base64::Engine as _;

    let gcal_ids: Vec<&str> = items
        .iter()
        .filter(|i| i.provenance().source == crate::types::SourceId::GoogleCalendar)
        .map(|i| i.provenance().native_id.as_str())
        .collect();
    if gcal_ids.is_empty() {
        return Ok((items, 0));
    }
    // The `eid` in calendar-email links is base64("<event_id> <calendar>").
    // The prefix that encodes "<event_id> " is a stable, searchable token.
    let eid_tokens: Vec<String> = gcal_ids
        .iter()
        .map(|id| {
            let bytes = format!("{id} ");
            let full = base64::engine::general_purpose::STANDARD.encode(bytes.as_bytes());
            full[..(bytes.len() / 3) * 4].to_string()
        })
        .collect();

    let digest_re = regex::Regex::new(r"(?i)^\s*\d+\s+events?\s+happening\s+(today|tomorrow)")
        .expect("digest regex compiles");

    let mut kept = Vec::with_capacity(items.len());
    let mut suppressed = 0usize;
    for item in items {
        let mut is_duplicate = false;
        if item.provenance().source == crate::types::SourceId::Gmail {
            if let Some(raw) =
                crate::db::source_raw_json(conn, item.provenance().source, &item.provenance().native_id)?
            {
                let payload = raw.get("payload").cloned().unwrap_or(serde_json::Value::Null);
                let from_calendar = crate::adapters::gmail::header_values(&payload, "From")
                    .iter()
                    .any(|v| v.contains("calendar-notification@google.com"));
                if from_calendar {
                    let body_text: String = crate::adapters::gmail::collect_body_parts(&payload)
                        .into_iter()
                        .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
                        .collect();
                    let references_briefed_event =
                        eid_tokens.iter().any(|t| body_text.contains(t.as_str()));
                    let is_agenda_digest = digest_re.is_match(item.summary());
                    if references_briefed_event || is_agenda_digest {
                        is_duplicate = true;
                    }
                }
            }
        }
        if is_duplicate {
            suppressed += 1;
        } else {
            kept.push(item);
        }
    }
    Ok((kept, suppressed))
}

/// The grounding validation pass (NOT advisory). Rejects the briefing if any
/// planned item (a) is not among the allowed inputs for this run, (b) does
/// not resolve to a stored source object, or (c) appears more than once.
pub fn validate_briefing(
    conn: &Connection,
    briefing: &Briefing,
    allowed: &[ExtractedItem],
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for planned in &briefing.sequence {
        let key = (planned.provenance.source, planned.provenance.native_id.clone());
        ensure!(
            seen.insert(key.clone()),
            "grounding violation: item {}:{} appears more than once in the sequence",
            planned.provenance.source,
            planned.provenance.native_id
        );
        ensure!(
            allowed.iter().any(|a| a.provenance() == &planned.provenance),
            "grounding violation: planned item {}:{} is not among this run's inputs",
            planned.provenance.source,
            planned.provenance.native_id
        );
        let resolves: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM source_objects WHERE source = ?1 AND native_id = ?2)",
                (planned.provenance.source.to_string(), &planned.provenance.native_id),
                |row| row.get(0),
            )
            .context("querying source_objects for grounding validation")?;
        ensure!(
            resolves,
            "grounding violation: planned item {}:{} does not resolve to a stored source object",
            planned.provenance.source,
            planned.provenance.native_id
        );
    }
    Ok(())
}

// ------------------------------------------------- digest + plan parsing ----

/// Numbered, kind-tagged digest the model plans over. Summaries only — no
/// raw content, no IDs (the model cannot leak or invent what it never sees).
pub fn build_digest(items: &[ExtractedItem]) -> String {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let tag = match item.kind() {
                ItemKind::Event => {
                    let t = item.occurred_at();
                    format!("event at {:02}:{:02} UTC", t.hour(), t.minute())
                }
                ItemKind::ActionNeeded => "action needed".to_string(),
                ItemKind::Commitment => "commitment".to_string(),
                ItemKind::Noise => "noise".to_string(), // not expected in digests
            };
            format!("{}. [{}] {}", i + 1, tag, item.summary())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strict parser for the model's ORDER reply. Anything that is not a
/// permutation of 1..=n is an error — the caller may retry once and must
/// otherwise fail the synthesis.
///
/// Small local models sometimes echo digest text after the numbers
/// ("ORDER: 1. [event at 18:00 UTC] …"). Only tokens that parse fully as
/// integers are kept — safety is unchanged because the permutation check
/// still requires exactly 1..=n, each exactly once.
pub fn parse_order(reply: &str, n_items: usize) -> Result<Vec<usize>> {
    let order_line = reply
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("ORDER:").or_else(|| l.strip_prefix("Order:")))
        .context("reply has no ORDER: line")?;

    let order: Vec<usize> = order_line
        .split([',', ' '])
        .map(|t| t.trim().trim_end_matches(['.', ')', ':']))
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect();
    ensure!(
        order.len() == n_items,
        "ORDER must list all {n_items} items exactly once, got {} entries",
        order.len()
    );
    let mut seen = vec![false; n_items];
    for &idx in &order {
        ensure!((1..=n_items).contains(&idx), "ORDER references item {idx}, which does not exist");
        ensure!(!seen[idx - 1], "ORDER lists item {idx} more than once");
        seen[idx - 1] = true;
    }
    Ok(order)
}

/// Deterministic, legible rationale built from REAL signals of the chosen
/// sequence (Phase 5 decision: the model picks the order; the "why" text is
/// templated — no model prose, no garble).
pub fn templated_rationale(sequence: &[PlannedItem]) -> String {
    let events: Vec<&PlannedItem> =
        sequence.iter().filter(|i| i.kind == ItemKind::Event).collect();
    let actions = sequence.iter().filter(|i| i.kind == ItemKind::ActionNeeded).count();
    let commitments = sequence.iter().filter(|i| i.kind == ItemKind::Commitment).count();

    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut composition = Vec::new();
    if !events.is_empty() {
        composition.push(format!("{} timed event{}", events.len(), plural(events.len())));
    }
    if actions > 0 {
        composition.push(format!("{actions} action item{}", plural(actions)));
    }
    if commitments > 0 {
        composition.push(format!("{commitments} commitment{}", plural(commitments)));
    }

    let mut sentences = vec![format!(
        "{} item{} briefed: {}.",
        sequence.len(),
        plural(sequence.len()),
        composition.join(", ")
    )];
    if !events.is_empty() {
        let times = events
            .iter()
            .map(|e| format!("{:02}:{:02}", e.occurred_at.hour(), e.occurred_at.minute()))
            .collect::<Vec<_>>()
            .join(", ");
        sentences.push(format!("Timed events hold their scheduled slots ({times} UTC)."));
    }
    if let Some(first_event_pos) = sequence.iter().position(|i| i.kind == ItemKind::Event) {
        let early = sequence[..first_event_pos]
            .iter()
            .filter(|i| i.kind == ItemKind::ActionNeeded || i.kind == ItemKind::Commitment)
            .count();
        if early > 0 {
            sentences.push(format!(
                "{early} loose item{} front-loaded before the first event.",
                plural(early)
            ));
        }
    }
    sentences
        .push("Sequence chosen by the on-device model; every item links to its source.".into());
    sentences.join(" ")
}

/// Assemble the briefing from OUR items in the model's chosen order —
/// provenance is copied from the extracted items, never model-generated;
/// the rationale is templated from the resulting sequence.
pub fn briefing_from_order(items: &[ExtractedItem], order: Vec<usize>) -> Result<Briefing> {
    let mut sequence = Vec::with_capacity(order.len());
    for idx in order {
        let item = items.get(idx - 1).context("plan index out of range")?;
        sequence.push(PlannedItem {
            provenance: item.provenance().clone(),
            kind: item.kind(),
            summary: item.summary().to_string(),
            occurred_at: item.occurred_at(),
        });
    }
    let rationale = templated_rationale(&sequence);
    Ok(Briefing { sequence, rationale })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_order_accepts_a_valid_reply() {
        assert_eq!(parse_order("ORDER: 2, 1, 3", 3).unwrap(), vec![2, 1, 3]);
    }

    #[test]
    fn parse_order_rejects_duplicates_missing_and_invented_items() {
        assert!(parse_order("ORDER: 1, 1, 2", 3).is_err(), "duplicate");
        assert!(parse_order("ORDER: 1, 2", 3).is_err(), "missing item");
        assert!(parse_order("ORDER: 1, 2, 4", 3).is_err(), "invented item");
        assert!(parse_order("no order here", 3).is_err(), "no order line");
    }

    #[test]
    fn parse_order_survives_digest_echo_without_weakening_the_permutation_check() {
        // Seen live: single-item day, model echoed the digest line.
        let echo = "ORDER: 1. [event at 18:00 UTC] Interview loop";
        assert_eq!(parse_order(echo, 1).unwrap(), vec![1]);

        // Echoed prose still cannot smuggle in a bad plan.
        assert!(parse_order(echo, 2).is_err(), "echo with missing item still rejected");
    }

    #[test]
    fn templated_rationale_states_real_signals_only() {
        let mk = |kind, at: &str| PlannedItem {
            provenance: crate::types::ProvenanceRef {
                source: crate::types::SourceId::Slack,
                native_id: "1.1".into(),
                deep_link: "https://example.slack.com/archives/C1/p11".into(),
            },
            kind,
            summary: "x".into(),
            occurred_at: at.parse().unwrap(),
        };
        let seq = vec![
            mk(ItemKind::ActionNeeded, "2026-07-11T08:00:00Z"),
            mk(ItemKind::Event, "2026-07-11T09:30:00Z"),
            mk(ItemKind::Commitment, "2026-07-11T07:45:00Z"),
            mk(ItemKind::Event, "2026-07-11T16:00:00Z"),
        ];
        let r = templated_rationale(&seq);
        assert!(r.contains("4 items briefed"), "{r}");
        assert!(r.contains("2 timed events"), "{r}");
        assert!(r.contains("09:30, 16:00 UTC"), "{r}");
        assert!(r.contains("1 loose item front-loaded"), "{r}");
        assert!(r.contains("on-device model"), "{r}");
    }
}
