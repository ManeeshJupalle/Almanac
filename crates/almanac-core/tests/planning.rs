//! Phase 2.3 gates: idempotent re-queueing (no duplicates, no resurrected
//! rejections, expired-may-return), the deterministic plan over DB state, and
//! an audited re-plan cycle whose chain still verifies. Offline; no network.

use almanac_core::act::draft::TemplatedDraftingBackend;
use almanac_core::act::{self, audit, ProposalState};
use almanac_core::correlate::{self, CorrelationEngine, QueueOutcome};
use almanac_core::plan::{self, PriorityConfig};
use almanac_core::types::{ProvenanceRef, RawContent, SourceId, SourceObject};
use chrono::{Duration, Utc};
use rusqlite::Connection;
use serde_json::json;

fn test_db() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = almanac_core::db::open(&dir.path().join("t.db")).unwrap();
    almanac_core::db::migrate(&mut conn).unwrap();
    (dir, conn)
}

fn store_git_commit(conn: &Connection, sha: &str, subject: &str) {
    let commit = almanac_core::git::Commit {
        sha: sha.to_string(),
        short_sha: sha.chars().take(7).collect(),
        author_name: "Alma Nemo".into(),
        author_email: "alma@example.test".into(),
        committed_at: Utc::now() - Duration::hours(2),
        parents: vec![],
        subject: subject.to_string(),
        body: String::new(),
        message: subject.to_string(),
        branches: vec!["main".into()],
        repo_name: "alm-scratch".into(),
        repo_path: "D:/alm-scratch".into(),
        deep_link: format!("git-local://alm-scratch/commit/{sha}"),
    };
    almanac_core::db::insert_source_object(conn, &almanac_core::git::to_source_object(&commit)).unwrap();
}

fn store_jira_issue(conn: &Connection, key: &str) {
    let obj = SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Jira,
            native_id: key.to_string(),
            deep_link: format!("https://x.atlassian.net/browse/{key}"),
        },
        raw: RawContent::new(json!({
            "key": key,
            "fields": { "summary": "Timezone bug", "comment": { "comments": [] } }
        })),
        occurred_at: Utc::now() - Duration::hours(3),
    };
    almanac_core::db::insert_source_object(conn, &obj).unwrap();
}

/// A Jira issue with an explicit status category ("done" / "indeterminate" / …).
fn store_jira_issue_with_status(conn: &Connection, key: &str, status_category: &str) {
    let obj = SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Jira,
            native_id: key.to_string(),
            deep_link: format!("https://x.atlassian.net/browse/{key}"),
        },
        raw: RawContent::new(json!({
            "key": key,
            "fields": {
                "summary": "Timezone bug",
                "status": { "statusCategory": { "key": status_category } },
                "comment": { "comments": [] }
            }
        })),
        occurred_at: Utc::now() - Duration::hours(3),
    };
    almanac_core::db::insert_source_object(conn, &obj).unwrap();
}

/// Full triple whose Jira issue carries the given status category, and mark its
/// queued proposal executed (simulating that we already replied). Returns the key.
fn hex_sha(key: &str) -> String {
    let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{hex:0<40}").chars().take(40).collect() // 40 hex chars, unique per key
}

fn seed_executed_triple(conn: &mut Connection, key: &str, status_category: &str) {
    let sha = hex_sha(key);
    store_git_commit(conn, &sha, &format!("{key}: fix"));
    store_jira_issue_with_status(conn, key, status_category);
    store_gmail_ask(conn, &format!("m-{key}"), key);
    let pid = queue(conn).ids[0];
    conn.execute("UPDATE action_proposals SET state = 'executed' WHERE id = ?1", [pid]).unwrap();
}

fn store_gmail_ask(conn: &Connection, nid: &str, subject: &str) {
    let obj = SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Gmail,
            native_id: nid.to_string(),
            deep_link: format!("https://mail.google.com/mail/#all/{nid}"),
        },
        raw: RawContent::new(json!({
            "id": nid, "threadId": format!("t-{nid}"),
            "snippet": format!("did you fix {subject}?"),
            "payload": { "headers": [
                { "name": "Subject", "value": subject },
                { "name": "From", "value": "Manager <manager@example.com>" },
                { "name": "Message-Id", "value": format!("<{nid}@mail.example.com>") }
            ]}
        })),
        occurred_at: Utc::now() - Duration::days(2),
    };
    almanac_core::db::insert_source_object(conn, &obj).unwrap();
}

/// Seed a full triple (ALM-3 email + Jira issue + commit) so correlation yields
/// one Full thread that queues one proposal.
fn seed_full_triple(conn: &Connection) {
    store_git_commit(conn, "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1", "ALM-3: fix tz");
    store_jira_issue(conn, "ALM-3");
    store_gmail_ask(conn, "m-alm3", "ALM-3");
}

fn threads(conn: &Connection) -> Vec<correlate::WorkThread> {
    let asks = correlate::load_asks(conn).unwrap();
    let items = correlate::load_work_items(conn).unwrap();
    let commits = correlate::load_commits(conn).unwrap();
    CorrelationEngine::new(None).correlate(&asks, &items, &commits)
}

fn queue(conn: &mut Connection) -> QueueOutcome {
    let t = threads(conn);
    correlate::queue_proposals(conn, &t, &TemplatedDraftingBackend, "correlation", Duration::hours(24))
        .unwrap()
}

fn proposal_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM action_proposals", [], |r| r.get(0)).unwrap()
}

// --------------------------------------------------------- idempotency -----

#[test]
fn correlate_double_run_is_idempotent() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn);

    let first = queue(&mut conn);
    assert_eq!(first.queued, 1, "first run queues the proposal");
    assert_eq!(proposal_count(&conn), 1);

    let second = queue(&mut conn);
    assert_eq!(second.queued, 0, "second run queues nothing new");
    assert_eq!(second.skipped, 1, "the identical proposal is skipped");
    assert_eq!(proposal_count(&conn), 1, "no duplicate row");
}

#[test]
fn rejected_proposal_is_not_resurrected() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn);

    let first = queue(&mut conn);
    let id = first.ids[0];
    act::reject(&mut conn, id, "user").unwrap();
    assert_eq!(act::load_proposal(&conn, id).unwrap().state, ProposalState::Rejected);

    let second = queue(&mut conn);
    assert_eq!(second.queued, 0, "a rejected correlation is never re-queued");
    assert_eq!(second.skipped, 1);
    assert_eq!(proposal_count(&conn), 1);
    // And it STAYS rejected.
    assert_eq!(act::load_proposal(&conn, id).unwrap().state, ProposalState::Rejected);
}

#[test]
fn expired_proposal_may_be_requeued() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn);

    let first = queue(&mut conn);
    let id = first.ids[0];
    // Force it past its TTL and sweep → 'expired'.
    conn.execute(
        "UPDATE action_proposals SET expires_at = '2000-01-01T00:00:00+00:00' WHERE id = ?1",
        [id],
    )
    .unwrap();
    act::sweep_expired(&mut conn).unwrap();
    assert_eq!(act::load_proposal(&conn, id).unwrap().state, ProposalState::Expired);

    // Expired does NOT block — re-planning legitimately re-surfaces the work.
    let second = queue(&mut conn);
    assert_eq!(second.queued, 1, "an expired (undecided) proposal may return");
    assert_eq!(proposal_count(&conn), 2);
}

#[test]
fn new_commit_on_same_ask_issue_does_not_duplicate() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn); // ALM-3 email + issue + commit a1…

    let first = queue(&mut conn);
    assert_eq!(first.queued, 1);
    assert_eq!(proposal_count(&conn), 1);

    // A NEW commit also references ALM-3 — more Tier-Hard evidence for the SAME
    // (ask, issue). The identity is the ask+issue pair, so this must not mint a
    // second, duplicate-looking proposal.
    store_git_commit(&conn, "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3", "ALM-3: follow-up");

    let second = queue(&mut conn);
    assert_eq!(second.queued, 0, "extra evidence must not duplicate the proposal");
    assert_eq!(second.skipped, 1);
    assert_eq!(proposal_count(&conn), 1, "still one proposal for the ask+issue");
}

// ----------------------------------------------------- re-plan + audit -----

#[test]
fn replan_cycle_audits_every_cycle_and_chain_verifies() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn);
    let cfg = PriorityConfig::default();

    let r1 = plan::replan_cycle(&mut conn, &cfg, None, "user", "manual refresh", Utc::now()).unwrap();
    assert_eq!(r1.outcome.queued, 1);
    assert!(r1.plan.items.iter().any(|i| i.candidate.key == "thread:ALM-3"));

    let r2 = plan::replan_cycle(&mut conn, &cfg, None, "user", "manual refresh", Utc::now()).unwrap();
    assert_eq!(r2.outcome.queued, 0, "idempotent across re-plan cycles");
    assert_eq!(r2.outcome.skipped, 1);

    // Both cycles are on the record (actor + reason), and the chain still verifies.
    let replans = audit_events(&conn).into_iter().filter(|e| e == "replanned").count();
    assert_eq!(replans, 2, "every triggered cycle is audited — no silent mutation");
    audit::verify_chain(&conn).unwrap();
}

#[test]
fn plan_ranks_thread_above_a_low_priority_partial() {
    let (_d, conn) = test_db();
    seed_full_triple(&conn); // full thread: ALM-3 (ready to act, hard evidence)
    // A partial: a Jira issue + commit but no ask.
    store_git_commit(&conn, "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2", "ALM-9: groundwork");
    store_jira_issue(&conn, "ALM-9");

    let now = Utc::now();
    let candidates = plan::load_candidates(&conn, now).unwrap();
    let p = plan::prioritize(&candidates, &PriorityConfig::default(), now);

    let full = p.items.iter().position(|i| i.candidate.key == "thread:ALM-3").unwrap();
    let partial = p.items.iter().position(|i| i.candidate.key == "thread:ALM-9").unwrap();
    assert!(full < partial, "the ready-to-act full thread outranks the partial");
    assert!(p.items[full].rationale.contains("evidence +15"));
    assert!(p.summary.starts_with("Plan: "));
}

/// Phase 3.1: a plan thread item links to the proposal queued for it, and that
/// link reflects state changes — proving the plan's inline actions and the queue
/// act on the SAME proposal (one A1 path, no duplicate execution route).
#[test]
fn plan_item_links_to_its_queued_proposal_and_tracks_state() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn); // ALM-3 thread → one queued proposal
    let outcome = queue(&mut conn);
    let pid = outcome.ids[0];

    let now = Utc::now();
    let p = plan::prioritize(&plan::load_candidates(&conn, now).unwrap(), &PriorityConfig::default(), now);

    let linked = plan::link_proposals(&conn, &p).unwrap();
    let for_thread = linked.get("thread:ALM-3").expect("thread item links to its proposal");
    assert_eq!(for_thread.len(), 1);
    assert_eq!(for_thread[0].id, pid);
    assert_eq!(for_thread[0].state, ProposalState::Proposed);

    // Rejecting through the shared act path is reflected in the same link.
    act::reject(&mut conn, pid, "user").unwrap();
    let relinked = plan::link_proposals(&conn, &p).unwrap();
    assert_eq!(relinked.get("thread:ALM-3").unwrap()[0].state, ProposalState::Rejected);
}

fn store_with_extraction(conn: &Connection, obj: &SourceObject, kind: almanac_core::extract::ItemKind) {
    almanac_core::db::insert_source_object(conn, obj).unwrap();
    let item = almanac_core::extract::ExtractedItem::new(
        kind,
        "summary".into(),
        obj.provenance.clone(),
        almanac_core::extract::ExtractionSignals {
            rule_hits: vec![],
            embedding_scores: None,
            decided_by: "rule".into(),
        },
        obj.occurred_at,
    )
    .unwrap();
    almanac_core::db::insert_extracted_item(conn, &item).unwrap();
}

/// A Gmail notification the v1 classifier labels "event" must NOT become an
/// overdue calendar deadline; only a real gcal event carries a deadline.
#[test]
fn gmail_event_notification_is_not_a_calendar_deadline() {
    let (_d, conn) = test_db();

    // Gmail notification classified "event", received in the past (no key).
    let gmail = SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Gmail,
            native_id: "m-notif".into(),
            deep_link: "https://mail.google.com/mail/#all/m-notif".into(),
        },
        raw: RawContent::new(json!({
            "snippet": "1 event happening tomorrow",
            "payload": { "headers": [{ "name": "Subject", "value": "1 event happening tomorrow" }] }
        })),
        occurred_at: Utc::now() - Duration::days(1),
    };
    store_with_extraction(&conn, &gmail, almanac_core::extract::ItemKind::Event);

    // A real upcoming calendar event (source = gcal).
    let gcal = SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::GoogleCalendar,
            native_id: "evt-1".into(),
            deep_link: "https://www.google.com/calendar/event?eid=evt1".into(),
        },
        raw: RawContent::new(json!({ "summary": "Standup" })),
        occurred_at: Utc::now() + Duration::hours(2),
    };
    store_with_extraction(&conn, &gcal, almanac_core::extract::ItemKind::Event);

    let cands = plan::load_candidates(&conn, Utc::now()).unwrap();
    let gmail_c = cands.iter().find(|c| c.key == "gmail:m-notif").unwrap();
    let gcal_c = cands.iter().find(|c| c.key == "gcal:evt-1").unwrap();

    assert!(gmail_c.deadline.is_none(), "a gmail 'event' notification has no calendar deadline");
    assert_eq!(gmail_c.kind, plan::CandidateKind::Ask);
    assert!(gcal_c.deadline.is_some(), "a real gcal event carries a deadline");
    assert_eq!(gcal_c.kind, plan::CandidateKind::CalendarEvent);

    // And the plan puts the upcoming real event in do-now, not the stale notif.
    let p = plan::prioritize(&cands, &PriorityConfig::default(), Utc::now());
    let gcal_item = p.items.iter().find(|i| i.candidate.key == "gcal:evt-1").unwrap();
    assert_eq!(gcal_item.section, plan::Section::DoNow);
}

// -------------------------------------------------- item state (3.2) -------

/// The active (state-filtered) candidate set at `now`.
fn active(conn: &Connection, now: chrono::DateTime<Utc>) -> Vec<plan::Candidate> {
    let candidates = plan::load_candidates(conn, now).unwrap();
    let states = plan::load_item_states(conn).unwrap();
    plan::active_candidates(candidates, &states, now)
}

fn in_plan(conn: &Connection, now: chrono::DateTime<Utc>, key: &str) -> bool {
    plan::prioritize(&active(conn, now), &PriorityConfig::default(), now)
        .items
        .iter()
        .any(|i| i.candidate.key == key)
}

#[test]
fn dismissed_item_leaves_the_plan_and_audits() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn);
    let now = Utc::now();
    assert!(in_plan(&conn, now, "thread:ALM-3"), "starts in the plan");

    plan::set_item_state(&mut conn, "thread:ALM-3", Some(plan::ItemStatus::Dismissed), None, "user", "not now", now)
        .unwrap();

    assert!(!in_plan(&conn, now, "thread:ALM-3"), "dismissed item is gone");
    assert!(audit_events(&conn).iter().any(|e| e == "plan_item_dismissed"), "the dismissal is audited");
    audit::verify_chain(&conn).unwrap();
}

#[test]
fn snoozed_item_returns_when_the_snooze_elapses() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn);
    let now = Utc::now();

    plan::set_item_state(
        &mut conn,
        "thread:ALM-3",
        Some(plan::ItemStatus::Snoozed),
        Some(now + Duration::hours(2)),
        "user",
        "later",
        now,
    )
    .unwrap();
    assert!(!in_plan(&conn, now, "thread:ALM-3"), "snoozed item is hidden until due");

    let later = now + Duration::hours(3);
    assert!(in_plan(&conn, later, "thread:ALM-3"), "snoozed item returns once the snooze elapses");
}

#[test]
fn reopen_clears_state_and_shows_the_item_again() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn);
    let now = Utc::now();

    plan::set_item_state(&mut conn, "thread:ALM-3", Some(plan::ItemStatus::Done), None, "user", "done", now)
        .unwrap();
    assert!(!in_plan(&conn, now, "thread:ALM-3"), "done item leaves the plan");

    plan::set_item_state(&mut conn, "thread:ALM-3", None, None, "user", "reopen", now).unwrap();
    assert!(in_plan(&conn, now, "thread:ALM-3"), "reopened item is back");
    audit::verify_chain(&conn).unwrap();
}

// --------------------------------------------- decision capture (3.3) ------

/// True if the factor snapshot contains a factor named `name` (order-robust).
fn has_factor(factors_json: &str, name: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(factors_json)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f.get("name").and_then(|v| v.as_str()) == Some(name))
}

#[test]
fn item_decision_captures_the_factor_vector() {
    let (_d, conn) = test_db();
    seed_full_triple(&conn);

    plan::record_item_decision(&conn, "thread:ALM-3", "user", "dismissed", Utc::now()).unwrap();

    let decisions = plan::load_decisions(&conn).unwrap();
    assert_eq!(decisions.len(), 1);
    let d = &decisions[0];
    assert_eq!(d.item_key, "thread:ALM-3");
    assert_eq!(d.decision, "dismissed");
    // Full thread → actionability + hard-evidence factors are snapshotted.
    assert!(has_factor(&d.factors_json, "actionability"));
    assert!(has_factor(&d.factors_json, "evidence"));
}

#[test]
fn proposal_decision_maps_to_its_thread_item() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn);
    let pid = queue(&mut conn).ids[0];

    plan::record_proposal_decision(&conn, pid, "user", "approved", Utc::now()).unwrap();

    let d = &plan::load_decisions(&conn).unwrap()[0];
    assert_eq!(d.item_key, "thread:ALM-3", "the proposal's correlation identity maps it to its plan item");
    assert_eq!(d.decision, "approved");
    assert!(has_factor(&d.factors_json, "actionability"));
}

#[test]
fn decisions_are_append_only_and_queryable_by_factor() {
    let (_d, conn) = test_db();
    seed_full_triple(&conn);
    let now = Utc::now();

    plan::record_item_decision(&conn, "thread:ALM-3", "user", "snoozed", now).unwrap();
    plan::record_item_decision(&conn, "thread:ALM-3", "user", "dismissed", now).unwrap();

    let all = plan::load_decisions(&conn).unwrap();
    assert_eq!(all.len(), 2, "append-only: repeated decisions are all kept");
    // "What did the user do with hard-evidence items?" — slice by the snapshot.
    let on_evidence = all.iter().filter(|d| has_factor(&d.factors_json, "evidence")).count();
    assert_eq!(on_evidence, 2, "both decisions were on an item that had hard evidence");
}

// ------------------------------------------------ learning flywheel (3.4) --

/// Insert a decision carrying exactly one factor, for controlled learning tests.
fn seed_decision(conn: &Connection, decision: &str, factor: &str) {
    let factors_json = format!("[{{\"name\":\"{factor}\",\"points\":10,\"reason\":\"x\"}}]");
    conn.execute(
        "INSERT INTO decision_events (occurred_at, actor, item_key, decision, factors_json)
         VALUES ('2026-07-18T00:00:00+00:00', 'user', 'k', ?1, ?2)",
        (decision, factors_json.as_str()),
    )
    .unwrap();
}

#[test]
fn repeated_dismissal_downweights_a_factor_and_explains() {
    let (_d, conn) = test_db();
    for _ in 0..4 {
        seed_decision(&conn, "dismissed", "classifier");
    }

    let learned = plan::learn_weights(&conn).unwrap();
    let w = learned.iter().find(|w| w.factor == "classifier").expect("classifier is tuned");
    assert!(w.multiplier < 1.0, "consistent dismissal downweights the factor");
    assert_eq!(w.negatives, 4);
    assert!(w.rationale.contains("dismiss"), "the adjustment explains itself: {}", w.rationale);

    // It flows into the config the ranker actually uses.
    let cfg = plan::effective_config(&conn).unwrap();
    assert!(cfg.weights.get("classifier").copied().unwrap() < 1.0);
}

#[test]
fn below_min_samples_no_adjustment() {
    let (_d, conn) = test_db();
    // Only two signals — under MIN_SAMPLES; must not tune anything.
    seed_decision(&conn, "dismissed", "asker");
    seed_decision(&conn, "dismissed", "asker");
    assert!(plan::learn_weights(&conn).unwrap().is_empty(), "too little signal → no adjustment");
}

#[test]
fn learning_reset_restores_default_weights() {
    let (_d, conn) = test_db();
    for _ in 0..4 {
        seed_decision(&conn, "dismissed", "classifier");
    }
    // Learning is ON by default → weights present.
    assert!(!plan::effective_config(&conn).unwrap().weights.is_empty());

    // Reset (disable) → base weights.
    plan::set_learning_enabled(&conn, false).unwrap();
    assert!(plan::effective_config(&conn).unwrap().weights.is_empty(), "reset restores defaults");

    // Re-enable → learned weights return.
    plan::set_learning_enabled(&conn, true).unwrap();
    assert!(!plan::effective_config(&conn).unwrap().weights.is_empty());
}

#[test]
fn learned_downweight_lowers_the_score_but_stays_deterministic() {
    let (_d, conn) = test_db();
    seed_full_triple(&conn);
    let now = Utc::now();

    let base = plan::prioritize(&plan::load_candidates(&conn, now).unwrap(), &PriorityConfig::default(), now);
    let base_score = base.items.iter().find(|i| i.candidate.key == "thread:ALM-3").unwrap().score;

    for _ in 0..4 {
        seed_decision(&conn, "dismissed", "actionability");
    }
    let cfg = plan::effective_config(&conn).unwrap();
    let s = |c: &plan::PriorityConfig| {
        plan::prioritize(&plan::load_candidates(&conn, now).unwrap(), c, now)
            .items
            .iter()
            .find(|i| i.candidate.key == "thread:ALM-3")
            .unwrap()
            .score
    };
    let s1 = s(&cfg);
    let s2 = s(&cfg);
    assert!(s1 < base_score, "downweighting actionability lowers the score ({s1} < {base_score})");
    assert_eq!(s1, s2, "same weights + inputs → same score (deterministic)");
}

// ------------------------------------------------ outcome tracking (3.5) ---

#[test]
fn executed_action_whose_issue_resolves_closes_the_loop() {
    let (_d, mut conn) = test_db();
    seed_executed_triple(&mut conn, "ALM-3", "done");

    let closed = plan::detect_and_record_outcomes(&mut conn, "system", Utc::now()).unwrap();
    assert_eq!(closed, vec!["thread:ALM-3".to_string()], "the loop closes");

    let outcomes = plan::load_outcomes(&conn).unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].item_key, "thread:ALM-3");
    assert_eq!(outcomes[0].evidence_source, "jira");
    assert_eq!(outcomes[0].evidence_native_id, "ALM-3", "the closing issue is the evidence");

    // Audited (L1) + chain intact.
    assert!(audit_events(&conn).iter().any(|e| e == "item_resolved"));
    audit::verify_chain(&conn).unwrap();

    // Retired from the active plan.
    assert!(!in_plan(&conn, Utc::now(), "thread:ALM-3"), "a resolved item leaves the plan");
}

#[test]
fn outcome_detection_is_idempotent() {
    let (_d, mut conn) = test_db();
    seed_executed_triple(&mut conn, "ALM-3", "done");

    assert_eq!(plan::detect_and_record_outcomes(&mut conn, "system", Utc::now()).unwrap().len(), 1);
    let second = plan::detect_and_record_outcomes(&mut conn, "system", Utc::now()).unwrap();
    assert!(second.is_empty(), "an already-closed loop is not re-closed");
    assert_eq!(plan::load_outcomes(&conn).unwrap().len(), 1);
}

#[test]
fn unacted_item_does_not_auto_close() {
    let (_d, mut conn) = test_db();
    // Issue is Done, but the proposal was never executed (still 'proposed').
    store_git_commit(&conn, &hex_sha("ALM-3"), "ALM-3: fix");
    store_jira_issue_with_status(&conn, "ALM-3", "done");
    store_gmail_ask(&conn, "m-ALM-3", "ALM-3");
    queue(&mut conn);

    let closed = plan::detect_and_record_outcomes(&mut conn, "system", Utc::now()).unwrap();
    assert!(closed.is_empty(), "an item we never acted on does not auto-close");
}

#[test]
fn executed_but_unresolved_item_does_not_close() {
    let (_d, mut conn) = test_db();
    seed_executed_triple(&mut conn, "ALM-4", "indeterminate"); // still in progress

    let closed = plan::detect_and_record_outcomes(&mut conn, "system", Utc::now()).unwrap();
    assert!(closed.is_empty(), "an executed action whose issue is still open does not close");
}

#[test]
fn outcome_evidence_must_be_a_stored_source_object() {
    let (_d, conn) = test_db();
    // Directly inserting an outcome whose evidence is not a stored source object
    // must fail — E2 enforced at rest by the composite FK.
    let r = conn.execute(
        "INSERT INTO item_outcomes (item_key, resolved_at, evidence_source, evidence_native_id, detail)
         VALUES ('thread:ALM-9', '2026-07-18T00:00:00+00:00', 'jira', 'ALM-9', 'x')",
        [],
    );
    assert!(r.is_err(), "E2: closing evidence must FK-resolve to a stored source object");
}

// ------------------------------------------------- audit viewer (3.6.1) ----

#[test]
fn audit_tail_surfaces_recent_events_newest_first() {
    let (_d, mut conn) = test_db();
    seed_full_triple(&conn);
    plan::set_item_state(&mut conn, "thread:ALM-3", Some(plan::ItemStatus::Dismissed), None, "user", "x", Utc::now())
        .unwrap();
    plan::replan_cycle(&mut conn, &PriorityConfig::default(), None, "user", "manual", Utc::now()).unwrap();

    let tail = audit::tail(&conn, 50).unwrap();
    let events: Vec<&str> = tail.iter().map(|r| r.event.as_str()).collect();
    assert!(events.contains(&"plan_item_dismissed"), "the dismissal is browsable");
    assert!(events.contains(&"replanned"), "the re-plan cycle is browsable");
    assert!(tail.windows(2).all(|w| w[0].seq > w[1].seq), "records are newest-first");
}

fn audit_events(conn: &Connection) -> Vec<String> {
    let mut stmt = conn.prepare("SELECT event FROM audit_records ORDER BY seq ASC").unwrap();
    stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

/// A Jira issue whose only link to a commit is one comment (author + sha ref),
/// so J15 self-exclusion decides whether that commit becomes evidence.
fn store_jira_issue_with_comment(conn: &Connection, key: &str, author_acct: &str, sha_ref: &str) {
    let obj = SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Jira,
            native_id: key.to_string(),
            deep_link: format!("https://x.atlassian.net/browse/{key}"),
        },
        raw: RawContent::new(json!({
            "key": key,
            "fields": {
                "summary": "Comment-linked issue",
                "comment": { "comments": [ {
                    "author": { "accountId": author_acct },
                    "body": { "type": "doc", "content": [ {
                        "type": "paragraph",
                        "content": [ { "type": "text", "text": format!("done in {sha_ref} ") } ]
                    } ] }
                } ] }
            }
        })),
        occurred_at: Utc::now() - Duration::hours(3),
    };
    almanac_core::db::insert_source_object(conn, &obj).unwrap();
}

/// The J15 fix (#2): the accountId cached in `app_meta` by an online Jira step
/// must make the OFFLINE plan path exclude Almanac's own Jira comment, so its
/// own comment never binds a commit as "evidence".
#[test]
fn cached_self_account_id_excludes_self_comment_evidence() {
    let (_d, conn) = test_db();
    // Commit whose ONLY tie to ALM-7 is a Jira comment (keyless subject/body/branch).
    store_git_commit(&conn, "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4", "chore: tidy up");
    store_jira_issue_with_comment(&conn, "ALM-7", "self-acct", "d4d4d4d");
    store_gmail_ask(&conn, "m-alm7", "ALM-7");

    // No cached self id → the (self-authored) comment binds the commit → Full.
    let c1 = plan::load_candidates(&conn, Utc::now()).unwrap();
    let t1 = c1.iter().find(|c| c.key == "thread:ALM-7").unwrap();
    assert!(t1.has_hard_evidence, "without J15 the self-comment binds evidence");

    // Cache the self accountId (what an online correlate/replan persists) →
    // J15 excludes Almanac's own comment → the commit is no longer evidence.
    almanac_core::db::set_meta(&conn, almanac_core::db::JIRA_SELF_ACCOUNT_ID, "self-acct").unwrap();
    let c2 = plan::load_candidates(&conn, Utc::now()).unwrap();
    let t2 = c2.iter().find(|c| c.key == "thread:ALM-7").unwrap();
    assert!(!t2.has_hard_evidence, "the cached self id excludes Almanac's own comment");
}
