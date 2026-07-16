//! Phase 2.2 gates: parse REAL captured git-log fixtures, and drive the full
//! correlation scenario end to end offline — email ask + Jira issue + commit →
//! one Full WorkThread → a GmailReply proposal citing the commit (Tier-Hard) +
//! the issue, persisted through the Phase 2.0 machinery (E1/E2/L1), approved,
//! with the audit chain still verifying. No test here touches the network.

use std::path::{Path, PathBuf};

use almanac_core::act::draft::TemplatedDraftingBackend;
use almanac_core::act::{self, audit, ActionKind, EvidenceKind, ProposalState};
use almanac_core::correlate::{self, CorrelationEngine};
use almanac_core::git;
use almanac_core::types::{ProvenanceRef, RawContent, SourceId, SourceObject};
use rusqlite::Connection;
use serde_json::json;

fn fixture_bytes(rel: &str) -> Vec<u8> {
    let path: PathBuf =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("fixtures").join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn test_db() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = almanac_core::db::open(&dir.path().join("t.db")).unwrap();
    almanac_core::db::migrate(&mut conn).unwrap();
    (dir, conn)
}

// ---------------------------------------------- real-fixture parsing -------

/// The captured scratch fixture exercises every required case: key in subject,
/// key in body only, a merge commit (2 parents), a multiline body, a non-ASCII
/// author + message, and a no-key commit.
#[test]
fn parses_real_scratch_fixture_all_cases() {
    let commits = git::parse_commits(&fixture_bytes("git_log/scratch_all.txt")).unwrap();
    assert_eq!(commits.len(), 7, "7 commits captured");

    // Keys reachable from subjects/bodies (word-boundary safe extraction).
    let mut keys = std::collections::BTreeSet::new();
    for c in &commits {
        keys.extend(correlate::issue_keys(&c.subject));
        keys.extend(correlate::issue_keys(&c.body));
    }
    for expected in ["ALM-1", "ALM-2", "ALM-3", "ALM-4", "ALM-5"] {
        assert!(keys.contains(expected), "expected {expected} somewhere in the fixture");
    }

    // Exactly one merge commit, and its subject carries ALM-4.
    let merges: Vec<_> = commits.iter().filter(|c| c.is_merge()).collect();
    assert_eq!(merges.len(), 1, "one merge commit");
    assert!(merges[0].subject.contains("ALM-4"));
    assert_eq!(merges[0].parents.len(), 2);

    // Non-ASCII author + message survived capture + redaction (example.test kept).
    assert!(
        commits.iter().any(|c| c.subject.contains("Café") && c.author_name.contains("Müller")),
        "non-ASCII author + message preserved"
    );

    // The no-key commit yields no issue keys.
    let no_key = commits
        .iter()
        .find(|c| c.subject.starts_with("Tidy up formatting"))
        .expect("the no-key commit");
    assert!(correlate::issue_keys(&no_key.subject).is_empty());
    assert!(correlate::issue_keys(&no_key.body).is_empty());

    // A body-only key: the "Add JQL parser" commit mentions ALM-2 in its body,
    // never its subject.
    let parser = commits.iter().find(|c| c.subject == "Add JQL parser").expect("parser commit");
    assert!(!correlate::issue_keys(&parser.subject).contains("ALM-2"));
    assert!(correlate::issue_keys(&parser.body).contains("ALM-2"));
}

/// The Almanac-repo capture (real history) parses and its author identities are
/// redacted (no real name/email leaks into the committed fixture).
#[test]
fn parses_real_almanac_fixture_and_is_redacted() {
    let bytes = fixture_bytes("git_log/almanac_all.txt");
    let commits = git::parse_commits(&bytes).unwrap();
    assert!(!commits.is_empty(), "almanac repo has commits");
    for c in &commits {
        assert_eq!(c.sha.len(), 40);
        assert!(!c.author_name.contains("Maneesh"), "real author name must be redacted");
    }
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("@gmail.com"), "real email must be redacted");
    assert!(text.contains("Redacted Author"), "redaction placeholder present");
}

// ------------------------------------------ full end-to-end scenario -------

/// Build the ALM-3 commit from the scratch fixture, ready to store as evidence.
fn alm3_commit() -> git::Commit {
    let commits = git::parse_commits(&fixture_bytes("git_log/scratch_all.txt")).unwrap();
    let mut c = commits
        .into_iter()
        .find(|c| c.subject.starts_with("ALM-3"))
        .expect("the ALM-3 commit");
    c.repo_name = "alm-scratch".to_string();
    c.repo_path = "D:/alm-scratch".to_string();
    c.branches = vec!["main".to_string()];
    c.deep_link = format!("git-local://alm-scratch/commit/{}", c.sha);
    c
}

fn store_manager_email(conn: &Connection, key: &str) {
    let native_id = "m-manager-1";
    let raw = json!({
        "id": native_id,
        "threadId": "t-1",
        "snippet": format!("Quick one — is {key} fixed and shippable?"),
        "payload": { "headers": [
            { "name": "Subject", "value": format!("Status on {key}?") },
            { "name": "From", "value": "Manager <manager@example.com>" },
            { "name": "Message-Id", "value": "<manager-1@mail.example.com>" }
        ]}
    });
    let obj = SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Gmail,
            native_id: native_id.to_string(),
            deep_link: format!("https://mail.google.com/mail/#all/{native_id}"),
        },
        raw: RawContent::new(raw),
        occurred_at: chrono::Utc::now(),
    };
    almanac_core::db::insert_source_object(conn, &obj).unwrap();
    // The v1 "ask": an ActionNeeded extracted item grounding the same message.
    let item = almanac_core::extract::ExtractedItem::new(
        almanac_core::extract::ItemKind::ActionNeeded,
        format!("Status on {key}?"),
        obj.provenance.clone(),
        almanac_core::extract::ExtractionSignals {
            rule_hits: vec![],
            embedding_scores: None,
            decided_by: "rule".to_string(),
        },
        obj.occurred_at,
    )
    .unwrap();
    almanac_core::db::insert_extracted_item(conn, &item).unwrap();
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
            "fields": { "summary": "Digest timezone bug", "comment": { "comments": [] } }
        })),
        occurred_at: chrono::Utc::now(),
    };
    almanac_core::db::insert_source_object(conn, &obj).unwrap();
}

/// A Gmail source object with a given subject + snippet (no extraction implied).
fn gmail_msg(nid: &str, subject: &str, snippet: &str) -> SourceObject {
    SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Gmail,
            native_id: nid.to_string(),
            deep_link: format!("https://mail.google.com/mail/#all/{nid}"),
        },
        raw: RawContent::new(json!({
            "id": nid,
            "threadId": format!("t-{nid}"),
            "snippet": snippet,
            "payload": { "headers": [
                { "name": "Subject", "value": subject },
                { "name": "From", "value": "Someone <someone@example.com>" },
                { "name": "Message-Id", "value": format!("<{nid}@mail.example.com>") }
            ]}
        })),
        occurred_at: chrono::Utc::now(),
    }
}

/// Regression for the live-gate defect: the v1 classifier labeled a self-sent
/// email whose subject was literally "ALM-3" as NOISE (low-confidence fallback).
/// `load_asks` must STILL treat it as an ask — the exact key-match is the signal,
/// not the noise label — so it correlates into a Full thread and a proposal.
#[test]
fn noise_classified_email_with_key_is_still_an_ask() {
    let (_d, conn) = test_db();
    almanac_core::db::insert_source_object(&conn, &git::to_source_object(&alm3_commit())).unwrap();
    store_jira_issue(&conn, "ALM-3");

    let email = gmail_msg("m-alm3-noise", "ALM-3", "Hey, did you fix ALM-3? Need it for the demo");
    almanac_core::db::insert_source_object(&conn, &email).unwrap();
    // Classify it NOISE via the low-confidence fallback, exactly as live.
    let noise = almanac_core::extract::ExtractedItem::new(
        almanac_core::extract::ItemKind::Noise,
        "ALM-3".into(),
        email.provenance.clone(),
        almanac_core::extract::ExtractionSignals {
            rule_hits: vec![],
            embedding_scores: None,
            decided_by: "fallback_low_confidence".to_string(),
        },
        email.occurred_at,
    )
    .unwrap();
    almanac_core::db::insert_extracted_item(&conn, &noise).unwrap();

    let asks = correlate::load_asks(&conn).unwrap();
    assert_eq!(asks.len(), 1, "noise-labeled but key-bearing email is STILL an ask");

    let threads = CorrelationEngine::new(None).correlate(
        &asks,
        &correlate::load_work_items(&conn).unwrap(),
        &correlate::load_commits(&conn).unwrap(),
    );
    assert_eq!(threads.len(), 1);
    assert!(threads[0].is_full(), "ask + item + commit ⇒ Full, despite the noise label");
    let proposals =
        correlate::propose_from_thread(&threads[0], &TemplatedDraftingBackend).unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].kind(), ActionKind::GmailReply);
}

/// Precision gate preserved: a message with NO issue key is NOT an ask, so the
/// ask set stays about work items rather than all inbox traffic.
#[test]
fn message_without_key_is_not_an_ask() {
    let (_d, conn) = test_db();
    almanac_core::db::insert_source_object(
        &conn,
        &gmail_msg("m-plain", "Lunch tomorrow?", "Are you free around noon? No tickets here."),
    )
    .unwrap();
    assert!(correlate::load_asks(&conn).unwrap().is_empty(), "no issue key ⇒ not an ask");
}

/// The hero scenario: manager email about ALM-3 + Jira ALM-3 + the ALM-3 commit
/// → one Full thread → a GmailReply proposal whose evidence chain is the commit
/// (Tier-Hard) + the issue. Persisted (E2 FKs resolve), E1 satisfied by the
/// commit, approved, chain still verifies.
#[test]
fn full_scenario_email_issue_commit_produces_grounded_proposal() {
    let (_d, mut conn) = test_db();

    // Store the three legs as grounded source objects.
    almanac_core::db::insert_source_object(&conn, &git::to_source_object(&alm3_commit())).unwrap();
    store_jira_issue(&conn, "ALM-3");
    store_manager_email(&conn, "ALM-3");

    // Correlate from the store.
    let asks = correlate::load_asks(&conn).unwrap();
    let items = correlate::load_work_items(&conn).unwrap();
    let commits = correlate::load_commits(&conn).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(items.len(), 1);
    assert_eq!(commits.len(), 1);

    let engine = CorrelationEngine::new(None);
    let threads = engine.correlate(&asks, &items, &commits);
    assert_eq!(threads.len(), 1);
    assert!(threads[0].is_full(), "ask + item + commit ⇒ Full");

    // Propose through the Phase 2.0 machinery and persist.
    let backend = TemplatedDraftingBackend;
    let proposals = correlate::propose_from_thread(&threads[0], &backend).unwrap();
    assert_eq!(proposals.len(), 1);
    let id = act::insert_proposal(&mut conn, &proposals[0], "correlation", chrono::Duration::hours(24))
        .unwrap();

    // Read it back: E1/E2 re-check on load passes; evidence chain is commit+issue.
    let stored = act::load_proposal(&conn, id).unwrap();
    assert_eq!(stored.kind, ActionKind::GmailReply);
    assert!(stored.asserts_work_done, "work-done claim (E1 required hard evidence)");
    assert!(stored.evidence.iter().any(|e| e.kind == EvidenceKind::GitCommit && e.source == SourceId::Git));
    assert!(stored.evidence.iter().any(|e| e.kind == EvidenceKind::JiraEvent));
    let rationale = stored.correlation_rationale.expect("a correlation rationale");
    assert!(rationale.contains("ALM-3") && rationale.contains("high"));

    // Approve (the A1 event) and confirm the hash-chained log still verifies.
    act::approve(&mut conn, id, "user").unwrap();
    assert_eq!(act::load_proposal(&conn, id).unwrap().state, ProposalState::Approved);
    audit::verify_chain(&conn).unwrap();
}

/// A textually-plausible but wrong-key commit (PSALM-1 / ALM-10) does NOT become
/// evidence for ALM-1, so no work-done proposal is produced (no fabrication).
#[test]
fn wrong_key_commit_produces_no_proposal() {
    let (_d, mut conn) = test_db();

    // A commit that mentions PSALM-1 and ALM-10 but never ALM-1.
    let mut c = alm3_commit();
    c.sha = "1234567890abcdef1234567890abcdef12345678".to_string();
    c.short_sha = "1234567".to_string();
    c.subject = "PSALM-1 hymn + ALM-10 note".to_string();
    c.body = String::new();
    c.deep_link = format!("git-local://alm-scratch/commit/{}", c.sha);
    almanac_core::db::insert_source_object(&conn, &git::to_source_object(&c)).unwrap();
    store_jira_issue(&conn, "ALM-1");
    store_manager_email(&conn, "ALM-1");

    let asks = correlate::load_asks(&conn).unwrap();
    let items = correlate::load_work_items(&conn).unwrap();
    let commits = correlate::load_commits(&conn).unwrap();
    let threads = CorrelationEngine::new(None).correlate(&asks, &items, &commits);

    // Ask binds to the issue, but NO commit does → partial, no proposal.
    assert_eq!(threads.len(), 1);
    assert!(!threads[0].is_full());
    let backend = TemplatedDraftingBackend;
    assert!(correlate::propose_from_thread(&threads[0], &backend).unwrap().is_empty());
    let _ = &mut conn;
}
