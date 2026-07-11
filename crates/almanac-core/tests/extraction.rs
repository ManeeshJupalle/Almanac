//! Phase 3 hard-component tests: grounding invariant, noise retention, rule
//! boundaries, and the labeled mini-set. Everything here is offline.
//!
//! Tests that need the ONNX model skip (with a loud note) when
//! models/minilm/ is absent — e.g. in CI. Run locally with the model for the
//! full suite; accuracy numbers are printed with --nocapture.

use std::path::{Path, PathBuf};

use almanac_core::adapters::{gcal, gmail, slack};
use almanac_core::extract::{ExtractedItem, ExtractionSignals, Extractor, ItemKind};
use almanac_core::types::{ProvenanceRef, SourceId, SourceObject};
use serde_json::{json, Value};

fn fixture(rel: &str) -> Value {
    let path: PathBuf =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("fixtures").join(rel);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn fixture_source_objects() -> Vec<SourceObject> {
    let mut objects = Vec::new();
    objects.push(gmail::message_to_source_object(&fixture("gmail/message_get_full.json")).unwrap());
    objects.extend(gcal::events_to_source_objects(&fixture("gcal/events_list_day.json")).unwrap());
    let history = fixture("slack/conversations_history.json");
    for msg in history.get("messages").and_then(Value::as_array).unwrap() {
        objects.push(
            slack::message_to_source_object("https://example.slack.com/", "C0BG797NE8P", msg)
                .unwrap(),
        );
    }
    objects
}

fn model_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("models").join("minilm");
    if dir.join("model.onnx").exists() && dir.join("tokenizer.json").exists() {
        Some(dir)
    } else {
        eprintln!("SKIP: models/minilm not present — embedding tests not run here");
        None
    }
}

fn slack_object(text: &str, ts: &str) -> SourceObject {
    let msg = json!({ "type": "message", "user": "U0BGKB60LH3", "text": text, "ts": ts });
    slack::message_to_source_object("https://example.slack.com/", "C0BG797NE8P", &msg).unwrap()
}

fn signals() -> ExtractionSignals {
    ExtractionSignals { rule_hits: vec![], embedding_scores: None, decided_by: "test".into() }
}

// ------------------------------------------------------------- grounding ----

#[test]
fn grounding_orphan_item_cannot_be_constructed() {
    // Empty native_id → refused.
    let orphan = ProvenanceRef {
        source: SourceId::Gmail,
        native_id: "".into(),
        deep_link: "https://mail.google.com/mail/#all/x".into(),
    };
    let now = chrono::Utc::now();
    assert!(ExtractedItem::new(ItemKind::Noise, "x".into(), orphan, signals(), now).is_err());

    // Non-https deep link → refused.
    let bad_link = ProvenanceRef {
        source: SourceId::Slack,
        native_id: "C1:1.2".into(),
        deep_link: "notaurl".into(),
    };
    assert!(ExtractedItem::new(ItemKind::Noise, "x".into(), bad_link, signals(), now).is_err());

    // Resolvable provenance → accepted.
    let good = ProvenanceRef {
        source: SourceId::Slack,
        native_id: "C1:1.2".into(),
        deep_link: "https://example.slack.com/archives/C1/p12".into(),
    };
    assert!(ExtractedItem::new(ItemKind::Noise, "x".into(), good, signals(), now).is_ok());
}

#[test]
fn grounding_db_rejects_items_without_a_source_object() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = almanac_core::db::open(&dir.path().join("t.db")).unwrap();
    almanac_core::db::migrate(&mut conn).unwrap();

    let objects = fixture_source_objects();
    let items = Extractor::rules_only().extract(&objects).unwrap();

    // Item WITHOUT its source object stored → foreign key failure, loud.
    let err = almanac_core::db::insert_extracted_item(&conn, &items[0]);
    assert!(err.is_err(), "ungrounded item must not persist");
    assert!(format!("{:#}", err.unwrap_err()).to_lowercase().contains("foreign key"));

    // Same item WITH its source object stored → persists fine.
    almanac_core::db::insert_source_object(&conn, &objects[0]).unwrap();
    almanac_core::db::insert_extracted_item(&conn, &items[0]).unwrap();
}

#[test]
fn provenance_passes_through_unchanged() {
    let objects = fixture_source_objects();
    let items = Extractor::rules_only().extract(&objects).unwrap();
    assert_eq!(objects.len(), items.len());
    for (obj, item) in objects.iter().zip(&items) {
        assert_eq!(&obj.provenance, item.provenance());
    }
}

// ------------------------------------------------------- noise retention ----

#[test]
fn noise_is_retained_and_flagged_never_dropped() {
    let objects = fixture_source_objects();
    let items = Extractor::rules_only().extract(&objects).unwrap();

    // Output length equals input length — nothing filtered out.
    assert_eq!(items.len(), objects.len());

    let noise: Vec<_> = items.iter().filter(|i| i.kind() == ItemKind::Noise).collect();
    assert!(!noise.is_empty(), "fixtures contain noise; it must be retained");
    for item in &noise {
        assert!(
            !item.signals().rule_hits.is_empty() || item.signals().decided_by.starts_with("fallback"),
            "noise must carry an auditable reason"
        );
    }
}

// -------------------------------------------------------- rule boundaries ----

#[test]
fn rule_gmail_list_unsubscribe_is_noise() {
    let obj = gmail::message_to_source_object(&fixture("gmail/message_get_full.json")).unwrap();
    let item = Extractor::rules_only().extract_one(&obj).unwrap();
    assert_eq!(item.kind(), ItemKind::Noise);
    assert_eq!(item.signals().decided_by, "rule");
    assert!(item.signals().rule_hits.iter().any(|h| h == "gmail:list-unsubscribe"));
}

#[test]
fn rule_gcal_event_is_event_and_cancelled_is_noise() {
    let objs = gcal::events_to_source_objects(&fixture("gcal/events_list_day.json")).unwrap();
    let item = Extractor::rules_only().extract_one(&objs[0]).unwrap();
    assert_eq!(item.kind(), ItemKind::Event);
    assert_eq!(item.signals().decided_by, "rule");

    let mut cancelled = objs[0].raw.as_json().clone();
    cancelled["status"] = json!("cancelled");
    let obj = SourceObject {
        provenance: objs[0].provenance.clone(),
        raw: almanac_core::types::RawContent::new(cancelled),
        occurred_at: objs[0].occurred_at,
    };
    let item = Extractor::rules_only().extract_one(&obj).unwrap();
    assert_eq!(item.kind(), ItemKind::Noise);
    assert!(item.signals().rule_hits.iter().any(|h| h == "gcal:cancelled"));
}

#[test]
fn rule_slack_system_and_bot_messages_are_noise() {
    let history = fixture("slack/conversations_history.json");
    let join = &history["messages"][0];
    let obj =
        slack::message_to_source_object("https://example.slack.com/", "C0BG797NE8P", join).unwrap();
    let item = Extractor::rules_only().extract_one(&obj).unwrap();
    assert_eq!(item.kind(), ItemKind::Noise);
    assert!(item.signals().rule_hits.iter().any(|h| h == "slack:subtype:channel_join"));

    let bot = json!({ "type": "message", "bot_id": "B123", "text": "deploy finished", "ts": "1783745961.000100" });
    let obj =
        slack::message_to_source_object("https://example.slack.com/", "C0BG797NE8P", &bot).unwrap();
    let item = Extractor::rules_only().extract_one(&obj).unwrap();
    assert_eq!(item.kind(), ItemKind::Noise);
    assert!(item.signals().rule_hits.iter().any(|h| h == "slack:bot_message"));
}

#[test]
fn rule_text_patterns_catch_explicit_asks_and_promises() {
    let ask = slack_object("Can you review the migration PR by EOD?", "1783745962.000001");
    let item = Extractor::rules_only().extract_one(&ask).unwrap();
    assert_eq!(item.kind(), ItemKind::ActionNeeded);
    assert_eq!(item.signals().decided_by, "rule");

    let promise = slack_object("I'll send the updated deck tomorrow morning.", "1783745963.000002");
    let item = Extractor::rules_only().extract_one(&promise).unwrap();
    assert_eq!(item.kind(), ItemKind::Commitment);
    assert_eq!(item.signals().decided_by, "rule");
}

// ------------------------------------- labeled mini-set: fixture items ----

#[test]
fn labeled_fixture_miniset_classifies_correctly() {
    // Hand labels for every content-bearing fixture item:
    //   gmail full message  → Noise (Mailgun bulk mail: List-Unsubscribe)
    //   gcal event          → Event
    //   slack channel_join  → Noise (system message)
    let objects = fixture_source_objects();
    let expected = [ItemKind::Noise, ItemKind::Event, ItemKind::Noise];
    let items = Extractor::rules_only().extract(&objects).unwrap();
    assert_eq!(items.len(), expected.len());
    for (item, want) in items.iter().zip(expected) {
        assert_eq!(item.kind(), want, "mislabeled: {}", item.summary());
    }
}

// ------------------------------ labeled mini-set: embedding classifier ----

/// 16 hand-labeled texts phrased to AVOID every rule pattern, forcing the
/// embedding path. Accuracy is printed honestly; the assertion floor is a
/// regression guard, not the headline number (see the phase report).
const EMBEDDING_MINISET: &[(&str, ItemKind)] = &[
    ("Leaving it with me — the deck goes out to the client before Thursday.", ItemKind::Commitment),
    ("As promised, the budget summary is being prepared and lands with you Monday.", ItemKind::Commitment),
    ("The migration is my responsibility now; expect it finished this sprint.", ItemKind::Commitment),
    ("Count on me for the quarterly numbers by end of week.", ItemKind::Commitment),
    ("Quarterly planning session, Room 4B, Wednesday 2pm.", ItemKind::Event),
    ("The design review starts at 11:30 in the main conference room.", ItemKind::Event),
    ("Coffee catch-up with Priya on Friday morning.", ItemKind::Event),
    ("Board meeting rescheduled to next Tuesday afternoon.", ItemKind::Event),
    ("The contract is waiting for your signature before it expires on Friday.", ItemKind::ActionNeeded),
    ("Reviewer feedback is blocking the release — someone has to respond today.", ItemKind::ActionNeeded),
    ("Your expense report was rejected; it has to be resubmitted before month end.", ItemKind::ActionNeeded),
    ("The security training is overdue and must be completed this week.", ItemKind::ActionNeeded),
    ("Introducing our redesigned mobile app — see what's new this month.", ItemKind::Noise),
    ("Your monthly statement is now available for viewing.", ItemKind::Noise),
    ("Someone mentioned you in a comment: tap to see the conversation.", ItemKind::Noise),
    ("Thanks for subscribing! Here's what to expect from our updates.", ItemKind::Noise),
];

#[test]
fn embedding_miniset_accuracy_reported_honestly() {
    let Some(dir) = model_dir() else { return };
    let extractor = Extractor::with_model(&dir).unwrap();

    let mut correct = 0;
    let mut lines = Vec::new();
    for (i, (text, want)) in EMBEDDING_MINISET.iter().enumerate() {
        let obj = slack_object(text, &format!("1783746000.{i:06}"));
        let item = extractor.extract_one(&obj).unwrap();
        assert!(
            item.signals().embedding_scores.is_some(),
            "mini-set text must take the embedding path, got rule hit for: {text}"
        );
        let got = item.kind();
        if got == *want {
            correct += 1;
        }
        lines.push(format!(
            "  {} want={:<13} got={:<13} decided_by={} | {}",
            if got == *want { "OK  " } else { "MISS" },
            want.as_str(),
            got.as_str(),
            item.signals().decided_by,
            text
        ));
    }
    let accuracy = correct as f32 / EMBEDDING_MINISET.len() as f32;
    eprintln!("embedding mini-set accuracy: {correct}/{} = {accuracy:.2}", EMBEDDING_MINISET.len());
    for l in &lines {
        eprintln!("{l}");
    }
    // Regression floor — the honest measured number lives in the report.
    assert!(accuracy >= 0.6, "embedding accuracy regressed below 0.6: {accuracy}");
}

#[test]
fn embedding_path_used_when_rules_are_silent() {
    let Some(dir) = model_dir() else { return };
    let extractor = Extractor::with_model(&dir).unwrap();
    let obj = slack_object("The roadmap draft is nearly finished and looks solid.", "1783746100.000001");
    let item = extractor.extract_one(&obj).unwrap();
    assert!(item.signals().embedding_scores.is_some());
    assert!(
        item.signals().decided_by == "embedding"
            || item.signals().decided_by == "fallback_low_confidence"
    );
}

#[test]
fn embedding_is_deterministic_and_normalized() {
    let Some(dir) = model_dir() else { return };
    let model = almanac_core::extract::EmbeddingModel::load(&dir).unwrap();
    let a = model.embed("standup meeting at ten").unwrap();
    let b = model.embed("standup meeting at ten").unwrap();
    assert_eq!(a, b, "same text must embed identically");
    let norm: f32 = a.iter().map(|v| v * v).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-4, "embedding must be L2-normalized, norm={norm}");
}
