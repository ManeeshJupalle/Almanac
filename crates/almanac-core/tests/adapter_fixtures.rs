//! Phase 2 gate: all three adapters parse their Phase-1 fixtures into
//! SourceObjects, deterministically, with every parsing quirk from
//! PAYLOAD_CORRECTIONS.md exercised. No network involved.

use std::path::{Path, PathBuf};

use almanac_core::adapters::{gcal, gmail, slack};
use almanac_core::types::SourceId;
use chrono::{DateTime, Datelike, Utc};
use serde_json::Value;

fn fixture(rel: &str) -> Value {
    let path: PathBuf =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("fixtures").join(rel);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap()
}

// ---------------------------------------------------------------- gmail ----

#[test]
fn gmail_full_message_parses_to_source_object() {
    let msg = fixture("gmail/message_get_full.json");
    let obj = gmail::message_to_source_object(&msg).unwrap();

    assert_eq!(obj.provenance.source, SourceId::Gmail);
    assert_eq!(obj.provenance.native_id, "19f4f90ab07d8041");
    assert_eq!(obj.provenance.deep_link, "https://mail.google.com/mail/#all/19f4f90ab07d8041");
    // G4: internalDate "1783746307000" (string, epoch ms) == 2026-07-11T05:05:07Z,
    // matching the message's Date header.
    assert_eq!(obj.occurred_at, "2026-07-11T05:05:07Z".parse::<DateTime<Utc>>().unwrap());
    assert!(obj.raw.as_json().get("payload").is_some(), "raw payload retained locally");
}

#[test]
fn gmail_next_page_token_stays_an_opaque_string() {
    // G2: the real token has a leading zero — numeric handling would corrupt it.
    let list = fixture("gmail/messages_list.json");
    let token = list.get("nextPageToken").and_then(Value::as_str).unwrap();
    assert_eq!(token, "08914656770475527830");
    assert!(token.starts_with('0'));
}

#[test]
fn gmail_headers_are_a_multiset_with_wire_cased_names() {
    // G7 + G8: three Received headers; names matched case-insensitively.
    let msg = fixture("gmail/message_get_full.json");
    let payload = msg.get("payload").unwrap();
    assert_eq!(gmail::header_values(payload, "Received").len(), 3);
    assert_eq!(gmail::header_values(payload, "RECEIVED").len(), 3);
    assert_eq!(gmail::header_values(payload, "dkim-signature").len(), 2);
    // Wire casing "Mime-Version" resolves for the canonical name too.
    assert_eq!(gmail::header_values(payload, "MIME-Version"), vec!["1.0"]);
}

#[test]
fn gmail_body_data_decodes_as_base64url_and_multipart_walk_works() {
    // G6 + G9: root body has no data ({"size": 0}); the two MIME parts carry
    // base64url data (redacted placeholder decodes to "[redacted]").
    let msg = fixture("gmail/message_get_full.json");
    let payload = msg.get("payload").unwrap();
    assert!(payload.pointer("/body/data").is_none(), "multipart root has no body data");

    let parts = gmail::collect_body_parts(payload);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].0, "text/plain");
    assert_eq!(parts[1].0, "text/html");
    assert_eq!(parts[0].1, b"[redacted]");
}

// ----------------------------------------------------------------- gcal ----

#[test]
fn gcal_event_parses_to_source_object() {
    let list = fixture("gcal/events_list_day.json");
    let objs = gcal::events_to_source_objects(&list).unwrap();
    assert_eq!(objs.len(), 1);

    let obj = &objs[0];
    assert_eq!(obj.provenance.source, SourceId::GoogleCalendar);
    assert_eq!(obj.provenance.native_id, "vdgh2ak4uln8e5tbi2scj57444");
    assert!(obj.provenance.deep_link.starts_with("https://www.google.com/calendar/event?eid="));
    // C5/C6: start 2026-07-08T13:00:00-05:00 == 18:00Z; the offset in the
    // payload wins even though the calendar tz (America/Chicago) differs from
    // the event tz (America/New_York).
    assert_eq!(obj.occurred_at, "2026-07-08T18:00:00Z".parse::<DateTime<Utc>>().unwrap());
}

#[test]
fn gcal_empty_day_yields_no_objects_without_error() {
    let list = fixture("gcal/events_list_day_empty.json");
    let objs = gcal::events_to_source_objects(&list).unwrap();
    assert!(objs.is_empty());
}

#[test]
fn gcal_both_rfc3339_flavors_parse() {
    // C6: UTC+millis (created/updated) and offset-no-millis (event times)
    // coexist in one payload — both taken from the real fixture.
    for flavor in ["2026-07-03T20:54:28.174Z", "2026-07-08T13:00:00-05:00"] {
        DateTime::parse_from_rfc3339(flavor).unwrap();
    }
}

#[test]
fn gcal_all_day_events_anchor_to_local_day_not_utc_midnight() {
    // C5: all-day events carry start.date instead of start.dateTime.
    // F-2 fix: the instant must fall on the event's date in the USER'S LOCAL
    // timezone (UTC midnight would land on the previous day in the Americas).
    use chrono::Local;
    let event = serde_json::json!({ "start": { "date": "2026-07-08" } });
    let ts = gcal::event_start_utc(&event).unwrap();
    let local_date = ts.with_timezone(&Local).date_naive();
    assert_eq!(local_date, "2026-07-08".parse::<chrono::NaiveDate>().unwrap());
}

// ---------------------------------------------------------------- slack ----

#[test]
fn slack_history_message_parses_to_source_object() {
    let history = fixture("slack/conversations_history.json");
    let msg = &history.get("messages").and_then(Value::as_array).unwrap()[0];
    let obj =
        slack::message_to_source_object("https://example.slack.com/", "C0BG797NE8P", msg).unwrap();

    assert_eq!(obj.provenance.source, SourceId::Slack);
    // S7: ts string kept verbatim inside the native id.
    assert_eq!(obj.provenance.native_id, "C0BG797NE8P:1783745960.543929");
    assert_eq!(
        obj.provenance.deep_link,
        "https://example.slack.com/archives/C0BG797NE8P/p1783745960543929"
    );
    // S7: exact microsecond parse, no float involved.
    let expected = DateTime::from_timestamp(1_783_745_960, 543_929_000).unwrap();
    assert_eq!(obj.occurred_at, expected);
    // S8: system messages (subtype channel_join) pass through unclassified.
    assert_eq!(obj.raw.as_json().get("subtype").and_then(Value::as_str), Some("channel_join"));
}

#[test]
fn slack_pagination_terminates_both_ways() {
    // S1: conversations.list last page → next_cursor is EMPTY STRING.
    let list = fixture("slack/conversations_list.json");
    assert_eq!(list.pointer("/response_metadata/next_cursor").and_then(Value::as_str), Some(""));
    assert_eq!(slack::next_cursor(&list), None);

    // S2: conversations.history last page → response_metadata ABSENT.
    let history = fixture("slack/conversations_history.json");
    assert!(history.get("response_metadata").is_none());
    assert_eq!(slack::next_cursor(&history), None);

    // A real continuation cursor still flows through.
    let more = serde_json::json!({ "response_metadata": { "next_cursor": "dGVhbTpD" } });
    assert_eq!(slack::next_cursor(&more).as_deref(), Some("dGVhbTpD"));
}

#[test]
fn slack_channel_created_and_updated_use_different_epoch_units() {
    // S3: created = seconds, updated = milliseconds, same object. If either
    // unit were guessed wrong the results would be millennia apart.
    let list = fixture("slack/conversations_list.json");
    let channel = &list.get("channels").and_then(Value::as_array).unwrap()[0];

    let created = slack::channel_created_utc(channel).unwrap();
    let updated = slack::channel_updated_utc(channel).unwrap();
    assert_eq!(created.year(), 2026);
    assert_eq!(updated.year(), 2026);
    let drift = (updated - created).num_seconds();
    assert!((0..3600).contains(&drift), "created/updated should be seconds apart, got {drift}s");
}

// ------------------------------------------------------------ provenance ----

#[test]
fn every_fixture_source_object_has_resolvable_provenance() {
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

    assert!(!objects.is_empty());
    for obj in &objects {
        assert!(!obj.provenance.native_id.is_empty(), "native_id must be non-empty");
        assert!(
            obj.provenance.deep_link.starts_with("https://"),
            "deep_link must be a real URL, got {}",
            obj.provenance.deep_link
        );
    }
}
