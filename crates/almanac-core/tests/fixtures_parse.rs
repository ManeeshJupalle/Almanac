//! Phase 1 gate: every captured fixture exists and parses as valid JSON.
//!
//! Deliberately uses only generic `serde_json::Value` — typed models are
//! Phase 2 work and must be written against these fixtures, not docs.

use std::path::{Path, PathBuf};

const REQUIRED: &[&str] = &[
    "gmail/messages_list.json",
    "gmail/message_get_full.json",
    "gcal/events_list_day.json",
    "slack/conversations_list.json",
    "slack/conversations_history.json",
];

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("fixtures")
}

#[test]
fn all_required_fixtures_exist_and_parse_as_json() {
    for rel in REQUIRED {
        let path = fixtures_root().join(rel);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing required fixture {}: {e}", path.display()));
        let value: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("fixture {} is not valid JSON: {e}", path.display()));
        assert!(
            value.is_object(),
            "fixture {} should be a top-level JSON object",
            path.display()
        );
    }
}

#[test]
fn slack_fixtures_have_ok_true_envelope() {
    // Slack signals errors as HTTP 200 + ok=false; a fixture captured that way
    // would be an error payload, not a real response.
    for rel in ["slack/conversations_list.json", "slack/conversations_history.json"] {
        let path = fixtures_root().join(rel);
        let raw = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            value.get("ok").and_then(serde_json::Value::as_bool),
            Some(true),
            "{} is not an ok=true Slack response",
            path.display()
        );
    }
}
