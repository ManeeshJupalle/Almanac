//! Phase 2.0 hard gates: E1 (hard-evidence requirement), E2 (evidence
//! resolution in memory AND at rest), A1 (no path to an executor without an
//! approval event), the state machine, the hash-chained audit log (including
//! deliberate corruption), and dry-run/execute byte identity for both
//! executors. Everything here is offline; no test touches the network.

use std::path::{Path, PathBuf};

use almanac_core::act::draft::{Draft, DraftingBackend, DraftRequest, TemplatedDraftingBackend};
use almanac_core::act::executors::{
    render_gmail_reply, render_slack_post, ActionExecutor, GmailReplyExecutor, SlackPostExecutor,
};
use almanac_core::act::{
    audit, ActionKind, ActionProposal, ActionTarget, EvidenceKind, EvidenceRef, EvidenceTier,
    ProposalState, ProposalViolation,
};
use almanac_core::adapters::{gmail, slack};
use almanac_core::auth::{GoogleAuth, SlackAuth};
use almanac_core::types::{SourceId, SourceObject};
use base64::Engine as _;
use rusqlite::Connection;
use serde_json::{json, Value};

// -------------------------------------------------------------- helpers ----

fn test_db() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = almanac_core::db::open(&dir.path().join("t.db")).unwrap();
    almanac_core::db::migrate(&mut conn).unwrap();
    (dir, conn)
}

fn fixture(rel: &str) -> Value {
    let path: PathBuf =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("fixtures").join(rel);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// The Phase-1 fixtured full Gmail message, stored as a source object — the
/// real payload the reply threading is modeled against.
fn store_fixture_gmail_message(conn: &Connection) -> SourceObject {
    let obj = gmail::message_to_source_object(&fixture("gmail/message_get_full.json")).unwrap();
    almanac_core::db::insert_source_object(conn, &obj).unwrap();
    obj
}

fn store_slack_message(conn: &Connection, ts: &str) -> SourceObject {
    let msg = json!({ "type": "message", "user": "U0BGKB60LH3", "text": "local", "ts": ts });
    let obj =
        slack::message_to_source_object("https://example.slack.com/", "C0BG797NE8P", &msg).unwrap();
    almanac_core::db::insert_source_object(conn, &obj).unwrap();
    obj
}

fn hard_evidence_for(obj: &SourceObject) -> EvidenceRef {
    EvidenceRef {
        kind: match obj.provenance.source {
            SourceId::GoogleCalendar => EvidenceKind::CalendarEvent,
            _ => EvidenceKind::Message,
        },
        source: obj.provenance.source,
        native_id: obj.provenance.native_id.clone(),
        deep_link: Some(obj.provenance.deep_link.clone()),
        observed_at: obj.occurred_at,
    }
}

fn soft_evidence() -> EvidenceRef {
    EvidenceRef {
        kind: EvidenceKind::WindowObservation,
        source: SourceId::Slack, // arbitrary; soft kinds have no store yet
        native_id: "obs-123".into(),
        deep_link: None,
        observed_at: chrono::Utc::now(),
    }
}

fn workdone_draft() -> Draft {
    TemplatedDraftingBackend
        .draft(&DraftRequest::WorkDoneReply {
            original_subject: "did you fix the bug?".into(),
            commit_short: "a1b2c3d".into(),
            completed_at: "2026-07-12T15:02:00Z".parse().unwrap(),
            ci_link: Some("https://ci.example.com/runs/42".into()),
            ticket: Some("ALM-42".into()),
        })
        .unwrap()
}

fn ack_draft() -> Draft {
    TemplatedDraftingBackend
        .draft(&DraftRequest::AckReply { original_subject: "hello".into() })
        .unwrap()
}

fn gmail_target(obj: &SourceObject) -> ActionTarget {
    ActionTarget::GmailThread { message_native_id: obj.provenance.native_id.clone() }
}

fn dummy_gmail_executor() -> GmailReplyExecutor {
    // Paths are never read on refusal paths: the A1 gate fires before any
    // token access.
    GmailReplyExecutor::new(GoogleAuth {
        credentials_path: "does-not-exist.json".into(),
        token_path: "does-not-exist.json".into(),
    })
}

fn audit_events(conn: &Connection) -> Vec<String> {
    let mut stmt =
        conn.prepare("SELECT event FROM audit_records ORDER BY seq ASC").unwrap();
    stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

// ------------------------------------------------------------------ E1 ----

#[test]
fn e1_factual_claim_with_soft_only_evidence_is_refused_at_construction() {
    let (_d, conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);

    // Soft-only factual claim → refused.
    let err = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        workdone_draft(),
        vec![soft_evidence()],
        "templated-v1",
    )
    .unwrap_err();
    assert!(
        matches!(err, ProposalViolation::SoftOnlyFactualClaim),
        "expected E1 refusal, got: {err}"
    );
    assert!(err.to_string().contains("E1"), "violation must name the invariant: {err}");

    // The SAME draft with one Tier-Hard ref constructs.
    let ok = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        workdone_draft(),
        vec![hard_evidence_for(&obj)],
        "templated-v1",
    );
    assert!(ok.is_ok(), "hard-evidence factual claim must construct: {:?}", ok.err());

    // Soft evidence may ACCOMPANY hard evidence (§4), it just can't be alone.
    let mixed = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        workdone_draft(),
        vec![hard_evidence_for(&obj), soft_evidence()],
        "templated-v1",
    );
    assert!(mixed.is_ok());

    // A non-factual draft is fine with any evidence tier mix.
    let ack = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![hard_evidence_for(&obj)],
        "templated-v1",
    );
    assert!(ack.is_ok());
}

// ------------------------------------------------------------------ E2 ----

#[test]
fn e2_malformed_evidence_is_refused_in_memory() {
    let (_d, conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);

    // Empty native_id.
    let mut empty_id = hard_evidence_for(&obj);
    empty_id.native_id = "  ".into();
    let err = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![empty_id],
        "templated-v1",
    )
    .unwrap_err();
    assert!(matches!(err, ProposalViolation::MalformedEvidence(_)), "{err}");

    // Hard evidence without an https deep link.
    let mut no_link = hard_evidence_for(&obj);
    no_link.deep_link = None;
    assert!(ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![no_link],
        "templated-v1",
    )
    .is_err());

    // No evidence at all → orphan proposal, refused.
    let err = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![],
        "templated-v1",
    )
    .unwrap_err();
    assert!(matches!(err, ProposalViolation::NoEvidence));

    // kind/source incoherence: a calendar_event ref claiming to come from gmail.
    let mut wrong = hard_evidence_for(&obj);
    wrong.kind = EvidenceKind::CalendarEvent; // but source is Gmail
    assert!(ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![wrong],
        "templated-v1",
    )
    .is_err());
}

#[test]
fn e2_unresolvable_evidence_is_uninsertable_at_rest() {
    let (_d, mut conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);

    // Well-formed in memory, but the referenced artifact was never stored.
    let phantom = EvidenceRef {
        kind: EvidenceKind::Message,
        source: SourceId::Slack,
        native_id: "C999:9999999999.000001".into(),
        deep_link: Some("https://example.slack.com/archives/C999/p9999999999000001".into()),
        observed_at: chrono::Utc::now(),
    };
    let proposal = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![phantom],
        "templated-v1",
    )
    .unwrap();
    let err = almanac_core::act::insert_proposal(
        &mut conn,
        &proposal,
        "test",
        chrono::Duration::hours(24),
    )
    .unwrap_err();
    let msg = format!("{err:#}").to_lowercase();
    assert!(msg.contains("foreign key"), "expected FK failure (E2 at rest), got: {msg}");

    // Same proposal with the artifact actually stored → persists.
    let slack_obj = store_slack_message(&conn, "1783745960.543929");
    let proposal = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![hard_evidence_for(&slack_obj)],
        "templated-v1",
    )
    .unwrap();
    almanac_core::act::insert_proposal(&mut conn, &proposal, "test", chrono::Duration::hours(24))
        .unwrap();
}

#[test]
fn e2_reply_target_must_be_a_stored_message() {
    let (_d, mut conn) = test_db();
    let slack_obj = store_slack_message(&conn, "1783745960.543929");

    // Target message never fetched → the proposals table's own FK refuses it.
    let proposal = ActionProposal::new(
        ActionKind::GmailReply,
        ActionTarget::GmailThread { message_native_id: "deadbeefdeadbeef".into() },
        ack_draft(),
        vec![hard_evidence_for(&slack_obj)],
        "templated-v1",
    )
    .unwrap();
    let err = almanac_core::act::insert_proposal(
        &mut conn,
        &proposal,
        "test",
        chrono::Duration::hours(24),
    )
    .unwrap_err();
    assert!(format!("{err:#}").to_lowercase().contains("foreign key"));
}

#[test]
fn soft_evidence_kinds_have_no_store_in_phase_2_0_and_cannot_persist() {
    let (_d, mut conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);

    // Constructs fine (E1 satisfied by the hard ref)...
    let proposal = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![hard_evidence_for(&obj), soft_evidence()],
        "templated-v1",
    )
    .unwrap();
    // ...but cannot persist until an artifact store for soft kinds exists.
    let err = almanac_core::act::insert_proposal(
        &mut conn,
        &proposal,
        "test",
        chrono::Duration::hours(24),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("no artifact store"), "{err:#}");
}

#[test]
fn tampered_stored_proposal_is_refused_on_load() {
    let (_d, mut conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);
    let proposal = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![hard_evidence_for(&obj)],
        "templated-v1",
    )
    .unwrap();
    let id = almanac_core::act::insert_proposal(
        &mut conn,
        &proposal,
        "test",
        chrono::Duration::hours(24),
    )
    .unwrap();

    // Rip out the evidence rows behind the pipeline's back (raw SQL).
    conn.execute("DELETE FROM proposal_evidence WHERE proposal_id = ?1", [id]).unwrap();
    let err = almanac_core::act::load_proposal(&conn, id).unwrap_err();
    assert!(format!("{err:#}").contains("E2"), "evidence-less proposal must not load: {err:#}");
}

// ------------------------------------------------------------------ A1 ----

#[tokio::test]
async fn a1_executor_refuses_proposed_and_rejected_proposals() {
    let (_d, mut conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);
    let proposal = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![hard_evidence_for(&obj)],
        "templated-v1",
    )
    .unwrap();
    let id = almanac_core::act::insert_proposal(
        &mut conn,
        &proposal,
        "test",
        chrono::Duration::hours(24),
    )
    .unwrap();
    let executor = dummy_gmail_executor();

    // Bypass attempt 1: execute while still 'proposed'.
    let err = executor.execute(&mut conn, id).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("REFUSING") && msg.contains("approval"),
        "executor must refuse a proposed proposal: {msg}"
    );
    // State unchanged; nothing audited as execution.
    let p = almanac_core::act::load_proposal(&conn, id).unwrap();
    assert_eq!(p.state, ProposalState::Proposed);
    assert!(
        !audit_events(&conn).iter().any(|e| e.starts_with("execution")),
        "a refused bypass must leave no execution audit records"
    );

    // Bypass attempt 2: execute after an explicit rejection.
    almanac_core::act::reject(&mut conn, id, "user").unwrap();
    let err = executor.execute(&mut conn, id).await.unwrap_err();
    assert!(format!("{err:#}").contains("REFUSING"));
    let p = almanac_core::act::load_proposal(&conn, id).unwrap();
    assert_eq!(p.state, ProposalState::Rejected);
    assert!(!audit_events(&conn).iter().any(|e| e.starts_with("execution")));
}

#[tokio::test]
async fn a1_slack_executor_enforces_the_same_gate() {
    let (_d, mut conn) = test_db();
    let slack_obj = store_slack_message(&conn, "1783745960.543929");
    let proposal = ActionProposal::new(
        ActionKind::SlackPost,
        ActionTarget::SlackChannel { channel_id: "C0BG797NE8P".into() },
        TemplatedDraftingBackend.draft(&DraftRequest::SlackCheckInPost).unwrap(),
        vec![hard_evidence_for(&slack_obj)],
        "templated-v1",
    )
    .unwrap();
    let id = almanac_core::act::insert_proposal(
        &mut conn,
        &proposal,
        "test",
        chrono::Duration::hours(24),
    )
    .unwrap();

    let executor = SlackPostExecutor::new(SlackAuth { token_path: "does-not-exist.json".into() });
    let err = executor.execute(&mut conn, id).await.unwrap_err();
    assert!(format!("{err:#}").contains("REFUSING"));
}

// --------------------------------------------------------- state machine ----

#[test]
fn state_machine_hard_rejects_illegal_transitions() {
    let (_d, mut conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);
    let mk = |conn: &mut Connection| {
        let p = ActionProposal::new(
            ActionKind::GmailReply,
            gmail_target(&obj),
            ack_draft(),
            vec![hard_evidence_for(&obj)],
            "templated-v1",
        )
        .unwrap();
        almanac_core::act::insert_proposal(conn, &p, "test", chrono::Duration::hours(24)).unwrap()
    };

    // approve is single-shot.
    let a = mk(&mut conn);
    almanac_core::act::approve(&mut conn, a, "user").unwrap();
    let err = almanac_core::act::approve(&mut conn, a, "user").unwrap_err();
    assert!(format!("{err:#}").contains("illegal transition"));
    // reject after approve is illegal.
    assert!(almanac_core::act::reject(&mut conn, a, "user").is_err());

    // approve after reject is illegal.
    let b = mk(&mut conn);
    almanac_core::act::reject(&mut conn, b, "user").unwrap();
    assert!(almanac_core::act::approve(&mut conn, b, "user").is_err());

    // unknown id is loud.
    assert!(almanac_core::act::approve(&mut conn, 99_999, "user").is_err());
}

#[test]
fn ttl_expiry_sweeps_proposed_rows_and_audits_them() {
    let (_d, mut conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);
    let p = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![hard_evidence_for(&obj)],
        "templated-v1",
    )
    .unwrap();
    // Already past its TTL.
    let id =
        almanac_core::act::insert_proposal(&mut conn, &p, "test", chrono::Duration::hours(-1))
            .unwrap();

    let swept = almanac_core::act::sweep_expired(&mut conn).unwrap();
    assert_eq!(swept, 1);
    let stored = almanac_core::act::load_proposal(&conn, id).unwrap();
    assert_eq!(stored.state, ProposalState::Expired);
    assert!(audit_events(&conn).contains(&"expired".to_string()));
    // Expired is terminal for approval.
    assert!(almanac_core::act::approve(&mut conn, id, "user").is_err());
}

// ------------------------------------------------------------ audit chain ----

#[test]
fn audit_chain_verifies_and_detects_tampering() {
    let (_d, mut conn) = test_db();

    // Fresh DB: genesis only, verifies.
    let report = audit::verify_chain(&conn).unwrap();
    assert_eq!(report.records, 1);

    // Drive a few transitions.
    let obj = store_fixture_gmail_message(&conn);
    let p = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![hard_evidence_for(&obj)],
        "templated-v1",
    )
    .unwrap();
    let id1 =
        almanac_core::act::insert_proposal(&mut conn, &p, "test", chrono::Duration::hours(24))
            .unwrap();
    let id2 =
        almanac_core::act::insert_proposal(&mut conn, &p, "test", chrono::Duration::hours(24))
            .unwrap();
    almanac_core::act::approve(&mut conn, id1, "user").unwrap();
    almanac_core::act::reject(&mut conn, id2, "user").unwrap();

    let report = audit::verify_chain(&conn).unwrap();
    assert_eq!(report.records, 5, "genesis + 2 proposed + approved + rejected");
    assert_eq!(audit_events(&conn), vec!["genesis", "proposed", "proposed", "approved", "rejected"]);

    // Tamper 1: edit a record's content → its hash no longer matches.
    conn.execute("UPDATE audit_records SET event = 'approved' WHERE seq = 4", []).unwrap();
    let err = audit::verify_chain(&conn).unwrap_err();
    assert!(format!("{err:#}").contains("seq 4"), "must name the broken record: {err:#}");
    conn.execute("UPDATE audit_records SET event = 'rejected' WHERE seq = 4", []).unwrap();
    audit::verify_chain(&conn).unwrap();

    // Tamper 2: break the link to a neighbor by swapping in a different (but
    // well-formed) prev_hash. Use a fixed all-zeros digest so the corruption
    // is deterministic regardless of the real hash's leading digit.
    let zeros = "0".repeat(64);
    let changed = conn
        .execute("UPDATE audit_records SET prev_hash = ?2 WHERE seq = 3", (3i64, &zeros))
        .unwrap();
    assert_eq!(changed, 1);
    assert!(audit::verify_chain(&conn).is_err(), "a broken prev_hash link must fail verification");
}

#[test]
fn audit_chain_detects_deleted_and_forged_genesis_records() {
    let (_d, mut conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);
    let p = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![hard_evidence_for(&obj)],
        "templated-v1",
    )
    .unwrap();
    for _ in 0..3 {
        almanac_core::act::insert_proposal(&mut conn, &p, "test", chrono::Duration::hours(24))
            .unwrap();
    }
    audit::verify_chain(&conn).unwrap();

    // Delete a middle record → contiguity break.
    conn.execute("DELETE FROM audit_records WHERE seq = 2", []).unwrap();
    let err = audit::verify_chain(&conn).unwrap_err();
    assert!(format!("{err:#}").contains("not contiguous"), "{err:#}");

    // Forged genesis on a separate DB → refused.
    let (_d2, conn2) = test_db();
    conn2
        .execute("UPDATE audit_records SET actor = 'not-genesis' WHERE seq = 0", [])
        .unwrap();
    let err = audit::verify_chain(&conn2).unwrap_err();
    assert!(format!("{err:#}").contains("seq 0"));
}

// ------------------------------------------------- dry-run byte identity ----

#[test]
fn gmail_dry_run_is_deterministic_thread_aware_and_byte_identical_to_the_send_body() {
    let (_d, mut conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);
    let raw = fixture("gmail/message_get_full.json");
    let fixture_thread_id = raw.get("threadId").and_then(Value::as_str).unwrap();

    let p = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        ack_draft(),
        vec![hard_evidence_for(&obj)],
        "templated-v1",
    )
    .unwrap();
    let id =
        almanac_core::act::insert_proposal(&mut conn, &p, "test", chrono::Duration::hours(24))
            .unwrap();
    let stored = almanac_core::act::load_proposal(&conn, id).unwrap();

    // Deterministic: two renders are byte-identical (this is what execute()
    // sends — it calls the same render).
    let r1 = render_gmail_reply(&conn, &stored).unwrap();
    let r2 = render_gmail_reply(&conn, &stored).unwrap();
    assert_eq!(r1, r2, "dry-run must be deterministic");
    let executor = dummy_gmail_executor();
    assert_eq!(executor.dry_run(&conn, &stored).unwrap(), r1);

    // Threading headers come from the REAL fixtured message (G7/G8 wire-cased
    // names, matched case-insensitively) — read the expected values from the
    // fixture itself rather than hardcoding redaction artifacts.
    let payload = raw.get("payload").unwrap();
    let orig_msg_id = gmail::header_values(payload, "Message-Id")[0];
    let orig_from = gmail::header_values(payload, "From")[0];
    let mime = &r1.display;
    assert!(mime.contains(&format!("To: {orig_from}\r\n")), "{mime}");
    assert!(mime.contains(&format!("In-Reply-To: {orig_msg_id}\r\n")), "{mime}");
    assert!(mime.contains(&format!("References: {orig_msg_id}")), "{mime}");
    assert!(mime.contains("Subject: Re: "), "{mime}");
    assert!(!mime.contains("Date:"), "Gmail sets Date — determinism requires we don't");

    // The API body wraps EXACTLY the displayed MIME (byte identity through
    // the base64url wrapper) and threads to the fixtured threadId.
    let body: Value = serde_json::from_slice(&r1.api_body).unwrap();
    assert_eq!(body.get("threadId").and_then(Value::as_str), Some(fixture_thread_id));
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body.get("raw").and_then(Value::as_str).unwrap())
        .unwrap();
    assert_eq!(decoded, mime.as_bytes(), "raw field must decode to the displayed MIME exactly");
}

#[test]
fn slack_dry_run_is_the_exact_json_body() {
    let (_d, mut conn) = test_db();
    let slack_obj = store_slack_message(&conn, "1783745960.543929");
    let p = ActionProposal::new(
        ActionKind::SlackPost,
        ActionTarget::SlackChannel { channel_id: "C0BG797NE8P".into() },
        TemplatedDraftingBackend.draft(&DraftRequest::SlackCheckInPost).unwrap(),
        vec![hard_evidence_for(&slack_obj)],
        "templated-v1",
    )
    .unwrap();
    let id =
        almanac_core::act::insert_proposal(&mut conn, &p, "test", chrono::Duration::hours(24))
            .unwrap();
    let stored = almanac_core::act::load_proposal(&conn, id).unwrap();

    let r1 = render_slack_post(&stored).unwrap();
    let r2 = render_slack_post(&stored).unwrap();
    assert_eq!(r1, r2);
    // display IS the api body, byte for byte.
    assert_eq!(r1.display.as_bytes(), &r1.api_body[..]);
    let body: Value = serde_json::from_slice(&r1.api_body).unwrap();
    assert_eq!(body.get("channel").and_then(Value::as_str), Some("C0BG797NE8P"));
    assert!(body.get("text").and_then(Value::as_str).unwrap().contains("approved"));
}

// ------------------------------------------------- drafting discipline ----

#[test]
fn templates_validate_typed_slots_and_never_take_free_body_text() {
    let backend = TemplatedDraftingBackend;

    // Bad slot values are refused, not interpolated.
    assert!(backend
        .draft(&DraftRequest::WorkDoneReply {
            original_subject: "s".into(),
            commit_short: "not-a-sha".into(),
            completed_at: chrono::Utc::now(),
            ci_link: None,
            ticket: None,
        })
        .is_err());
    assert!(backend
        .draft(&DraftRequest::SlackWorkDonePost {
            ticket: "not a ticket".into(),
            commit_short: "a1b2c3d".into(),
            completed_at: chrono::Utc::now(),
            ci_link: None,
        })
        .is_err());
    assert!(backend
        .draft(&DraftRequest::WorkDoneReply {
            original_subject: "s".into(),
            commit_short: "a1b2c3d".into(),
            completed_at: chrono::Utc::now(),
            ci_link: Some("http://insecure.example.com".into()),
            ticket: None,
        })
        .is_err());

    // Untrusted subject text cannot inject MIME headers.
    let evil = backend
        .draft(&DraftRequest::AckReply {
            original_subject: "urgent\r\nBcc: attacker@example.com".into(),
        })
        .unwrap();
    let subject = evil.subject.unwrap();
    assert!(!subject.contains('\r') && !subject.contains('\n'), "{subject}");
    assert!(subject.contains("Bcc") == false || !subject.contains('\n'));
}

#[test]
fn header_injection_is_neutralized_end_to_end_in_the_mime() {
    let (_d, mut conn) = test_db();
    let obj = store_fixture_gmail_message(&conn);
    // Bypass the templates deliberately (worst case) — the render layer must
    // still sanitize.
    let draft = Draft::raw_for_tests(
        Some("evil\r\nBcc: attacker@example.com".into()),
        "body".into(),
        false,
    );
    let p = ActionProposal::new(
        ActionKind::GmailReply,
        gmail_target(&obj),
        draft,
        vec![hard_evidence_for(&obj)],
        "templated-v1",
    )
    .unwrap();
    let id =
        almanac_core::act::insert_proposal(&mut conn, &p, "test", chrono::Duration::hours(24))
            .unwrap();
    let stored = almanac_core::act::load_proposal(&conn, id).unwrap();
    let rendered = render_gmail_reply(&conn, &stored).unwrap();
    // The headers section (before the blank line) must not contain a Bcc line.
    let headers_part = rendered.display.split("\r\n\r\n").next().unwrap();
    assert!(
        !headers_part.lines().any(|l| l.starts_with("Bcc:")),
        "header injection survived sanitization:\n{headers_part}"
    );
}
