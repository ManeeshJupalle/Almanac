//! Phase 2.1 Jira read-path tests: adapter parsing (against the captured
//! fixtures), the J9 non-RFC3339 timestamp, deep links, error-envelope
//! reading, and status-category extraction. All offline.

use std::path::{Path, PathBuf};

use almanac_core::adapters::jira;
use almanac_core::extract::{Extractor, ItemKind};
use almanac_core::types::{SourceId, SourceObject};
use chrono::{DateTime, Utc};
use serde_json::Value;

const BASE: &str = "https://redacted.atlassian.net";

fn fixture(rel: &str) -> Value {
    let path: PathBuf =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("fixtures").join(rel);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn first_issue() -> Value {
    fixture("jira/search_jql.json").get("issues").and_then(Value::as_array).unwrap()[0].clone()
}

// --------------------------------------------------------------- parsing ----

#[test]
fn jira_issue_parses_to_grounded_source_object() {
    let obj = jira::issue_to_source_object(BASE, &first_issue()).unwrap();
    assert_eq!(obj.provenance.source, SourceId::Jira);
    // J10: the KEY (not the numeric id) is the native id + deep-link anchor.
    assert_eq!(obj.provenance.native_id, "ALM-1");
    assert_eq!(obj.provenance.deep_link, "https://redacted.atlassian.net/browse/ALM-1");
    assert!(obj.provenance.deep_link.starts_with("https://"));
    // Raw payload retained locally.
    assert_eq!(obj.raw.as_json().get("key").and_then(Value::as_str), Some("ALM-1"));
}

#[test]
fn jira_deep_link_uses_the_issue_key() {
    assert_eq!(jira::deep_link(BASE, "ALM-42"), "https://redacted.atlassian.net/browse/ALM-42");
    // Trailing slash on the base is tolerated.
    assert_eq!(jira::deep_link("https://x.atlassian.net/", "ALM-1"), "https://x.atlassian.net/browse/ALM-1");
}

#[test]
fn jira_timestamp_is_not_rfc3339_and_needs_the_explicit_parser() {
    // J9: offset WITHOUT a colon — real value from the comment fixture.
    let s = "2026-07-14T22:26:03.672-0500";
    // The naive RFC3339 parser REJECTS it (this is why J9 exists).
    assert!(DateTime::parse_from_rfc3339(s).is_err(), "expected rfc3339 to reject {s}");
    // Our explicit parser accepts it and yields the right UTC instant
    // (22:26:03.672 at -05:00 == 03:26:03.672Z next day).
    let utc: DateTime<Utc> = jira::jira_time_to_utc(s).unwrap();
    assert_eq!(utc, "2026-07-15T03:26:03.672Z".parse::<DateTime<Utc>>().unwrap());
}

#[test]
fn jira_error_reads_both_envelope_shapes() {
    // J4: 410 uses errorMessages[]; 400 validation uses errors{}.
    let gone = r#"{"errorMessages":["The requested API has been removed."],"errors":{}}"#;
    assert!(jira::jira_error(gone).contains("has been removed"));

    let bad = r#"{"errorMessages":[],"errors":{"comment":"Comment body is not valid!"}}"#;
    let msg = jira::jira_error(bad);
    assert!(msg.contains("comment"), "{msg}");
    assert!(msg.contains("not valid"), "{msg}");
}

// ------------------------------------------------------------ extraction ----

fn issue_objects() -> Vec<SourceObject> {
    fixture("jira/search_jql.json")
        .get("issues")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|i| jira::issue_to_source_object(BASE, i).unwrap())
        .collect()
}

#[test]
fn jira_issues_classify_by_status_category() {
    // Rules-only (offline): open issues → ActionNeeded, Done → Noise. The
    // captured project has ALM-1..4 open (To Do / In Progress) and ALM-5 Done.
    let objects = issue_objects();
    let items = Extractor::rules_only().extract(&objects).unwrap();
    assert_eq!(items.len(), objects.len(), "one item per issue, nothing dropped");

    for (obj, item) in objects.iter().zip(&items) {
        let category = obj
            .raw
            .as_json()
            .pointer("/fields/status/statusCategory/key")
            .and_then(Value::as_str)
            .unwrap();
        match category {
            "done" => {
                assert_eq!(item.kind(), ItemKind::Noise, "{} done → noise", obj.provenance.native_id);
                assert!(item.signals().rule_hits.iter().any(|h| h == "jira:status-done"));
            }
            "new" | "indeterminate" => {
                assert_eq!(
                    item.kind(),
                    ItemKind::ActionNeeded,
                    "{} open → action",
                    obj.provenance.native_id
                );
                assert_eq!(item.signals().decided_by, "rule");
            }
            other => panic!("unexpected status category {other}"),
        }
        // Provenance passes through unchanged (grounding).
        assert_eq!(item.provenance(), &obj.provenance);
    }

    // Sanity: the mixed-state project yielded both classes.
    assert!(items.iter().any(|i| i.kind() == ItemKind::ActionNeeded));
    assert!(items.iter().any(|i| i.kind() == ItemKind::Noise));
}

#[test]
fn jira_summary_becomes_the_item_summary() {
    let obj = jira::issue_to_source_object(BASE, &first_issue()).unwrap();
    let item = Extractor::rules_only().extract_one(&obj).unwrap();
    assert_eq!(item.summary(), "Fix audit chain verification failing on empty DB");
}
