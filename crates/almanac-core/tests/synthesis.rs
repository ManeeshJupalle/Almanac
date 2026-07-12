//! Phase 4 hard gates: grounding rejection (validator AND pipeline),
//! run-scoping/dedup, and model-gated end-to-end synthesis.
//! Model-dependent tests skip loudly when models/ is absent (e.g. CI).

use std::path::{Path, PathBuf};

use almanac_core::extract::{ExtractedItem, ExtractionSignals, ItemKind};
use almanac_core::synth::{
    generate_briefing, validate_briefing, Briefing, DayContext, PlannedItem, SynthesisBackend,
};
use almanac_core::types::{ProvenanceRef, RawContent, SourceId, SourceObject};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::Connection;
use serde_json::json;

fn test_db() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = almanac_core::db::open(&dir.path().join("t.db")).unwrap();
    almanac_core::db::migrate(&mut conn).unwrap();
    (dir, conn)
}

fn provenance(native_id: &str) -> ProvenanceRef {
    ProvenanceRef {
        source: SourceId::Slack,
        native_id: native_id.to_string(),
        deep_link: format!("https://example.slack.com/archives/C1/p{native_id}"),
    }
}

fn source_object(native_id: &str, occurred_at: DateTime<Utc>) -> SourceObject {
    SourceObject {
        provenance: provenance(native_id),
        raw: RawContent::new(json!({"type": "message", "text": "local", "ts": native_id})),
        occurred_at,
    }
}

fn item(native_id: &str, kind: ItemKind, occurred_at: DateTime<Utc>) -> ExtractedItem {
    ExtractedItem::new(
        kind,
        format!("summary for {native_id}"),
        provenance(native_id),
        ExtractionSignals { rule_hits: vec![], embedding_scores: None, decided_by: "test".into() },
        occurred_at,
    )
    .unwrap()
}

fn planned(native_id: &str, occurred_at: DateTime<Utc>) -> PlannedItem {
    PlannedItem {
        provenance: provenance(native_id),
        kind: ItemKind::ActionNeeded,
        summary: format!("summary for {native_id}"),
        occurred_at,
    }
}

fn ts(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn day(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

fn store(conn: &Connection, obj: &SourceObject, extracted: &ExtractedItem) {
    almanac_core::db::insert_source_object(conn, obj).unwrap();
    almanac_core::db::insert_extracted_item(conn, extracted).unwrap();
}

// ------------------------------------------------- grounding rejection ----

#[test]
fn validator_rejects_a_deliberately_ungrounded_item() {
    let (_dir, conn) = test_db();
    let when = ts("2026-07-11T10:00:00Z");

    // A real, stored item…
    let obj = source_object("1783745960.543929", when);
    let real = item("1783745960.543929", ItemKind::ActionNeeded, when);
    store(&conn, &obj, &real);

    // …and a briefing that also claims a FABRICATED item.
    let briefing = Briefing {
        sequence: vec![
            planned("1783745960.543929", when),
            planned("9999999999.000000", when), // never fetched, never stored
        ],
        rationale: "made up".into(),
        preview: vec![],
    };
    let err = validate_briefing(&conn, &briefing, &[real]).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("grounding violation"), "unexpected error: {msg}");
}

/// A hostile backend that emits an item outside its inputs — the PIPELINE
/// must reject its briefing. This also proves the trait swaps cleanly
/// (second backend implemented in a test with no model coupling).
struct HallucinatingBackend;

#[async_trait]
impl SynthesisBackend for HallucinatingBackend {
    fn backend_id(&self) -> &str {
        "test-hallucinating"
    }
    async fn synthesize(&self, _items: Vec<ExtractedItem>, _ctx: DayContext) -> Result<Briefing> {
        Ok(Briefing {
            sequence: vec![planned("6666666666.000000", ts("2026-07-11T09:00:00Z"))],
            rationale: "trust me".into(),
            preview: vec![],
        })
    }
}

#[tokio::test]
async fn pipeline_hard_rejects_a_backend_that_hallucinates_items() {
    let (_dir, conn) = test_db();
    let when = ts("2026-07-11T10:00:00Z");
    let obj = source_object("1783745960.543929", when);
    let real = item("1783745960.543929", ItemKind::ActionNeeded, when);
    store(&conn, &obj, &real);

    let ctx = DayContext { date: day("2026-07-11"), now: Utc::now() };
    let err = generate_briefing(&conn, &HallucinatingBackend, ctx).await.unwrap_err();
    assert!(format!("{err:#}").contains("grounding violation"));
}

#[test]
fn validator_rejects_duplicate_planned_items() {
    let (_dir, conn) = test_db();
    let when = ts("2026-07-11T10:00:00Z");
    let obj = source_object("1.1", when);
    let real = item("1.1", ItemKind::ActionNeeded, when);
    store(&conn, &obj, &real);

    let briefing = Briefing {
        sequence: vec![planned("1.1", when), planned("1.1", when)],
        rationale: "twice".into(),
        preview: vec![],
    };
    assert!(validate_briefing(&conn, &briefing, &[real]).is_err());
}

// ------------------------------------------------- run-scoping / dedup ----

#[test]
fn run_scoping_latest_extraction_wins_no_duplicates() {
    let (_dir, conn) = test_db();
    let when = ts("2026-07-11T10:00:00Z");
    let obj = source_object("1.1", when);
    almanac_core::db::insert_source_object(&conn, &obj).unwrap();

    // Two extraction runs disagree; the later one must win.
    let run1 = item("1.1", ItemKind::Noise, when);
    let run2 = item("1.1", ItemKind::ActionNeeded, when);
    almanac_core::db::insert_extracted_item(&conn, &run1).unwrap();
    almanac_core::db::insert_extracted_item(&conn, &run2).unwrap();

    let inputs = almanac_core::db::briefing_inputs_range(
        &conn,
        ts("2026-07-11T00:00:00Z"),
        ts("2026-07-12T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(inputs.len(), 1, "dedup must collapse re-extractions");
    assert_eq!(inputs[0].kind(), ItemKind::ActionNeeded, "latest run wins");
}

#[test]
fn run_scoping_selects_only_the_requested_range() {
    let (_dir, conn) = test_db();
    let d10 = ts("2026-07-10T23:00:00Z");
    let d11 = ts("2026-07-11T01:00:00Z");
    store(&conn, &source_object("10.1", d10), &item("10.1", ItemKind::ActionNeeded, d10));
    store(&conn, &source_object("11.1", d11), &item("11.1", ItemKind::ActionNeeded, d11));

    let inputs = almanac_core::db::briefing_inputs_range(
        &conn,
        ts("2026-07-11T00:00:00Z"),
        ts("2026-07-12T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].provenance().native_id, "11.1");
}

// --------------------------------------------- local-tz day boundaries ----

#[test]
fn local_day_bounds_catch_late_evening_items_across_utc_midnight() {
    // 22:30 on July 11 in UTC-5 is 03:30 UTC on July 12. Under UTC day
    // selection this item briefs on the wrong day; under local-day bounds it
    // belongs to July 11.
    let tz = chrono::FixedOffset::west_opt(5 * 3600).unwrap();
    let (start, end) = almanac_core::db::day_bounds(day("2026-07-11"), &tz).unwrap();
    assert_eq!(start, ts("2026-07-11T05:00:00Z"));
    assert_eq!(end, ts("2026-07-12T05:00:00Z"));

    let (_dir, conn) = test_db();
    let late_evening = ts("2026-07-12T03:30:00Z"); // 22:30 local, July 11
    store(
        &conn,
        &source_object("late.1", late_evening),
        &item("late.1", ItemKind::ActionNeeded, late_evening),
    );
    let inputs = almanac_core::db::briefing_inputs_range(&conn, start, end).unwrap();
    assert_eq!(inputs.len(), 1, "late-evening local item must brief on its local day");

    // And it must NOT brief on local July 12.
    let (start12, end12) = almanac_core::db::day_bounds(day("2026-07-12"), &tz).unwrap();
    let inputs12 = almanac_core::db::briefing_inputs_range(&conn, start12, end12).unwrap();
    assert!(inputs12.is_empty());
}

#[test]
fn dst_transition_day_has_23_hours_and_correct_bounds() {
    // America/Chicago springs forward on 2026-03-08 (02:00 CST → 03:00 CDT):
    // the local day is 23 hours long and the bounds must reflect the offset
    // change (CST -6 at midnight, CDT -5 by the next midnight).
    let tz: chrono_tz::Tz = "America/Chicago".parse().unwrap();
    let (start, end) = almanac_core::db::day_bounds(day("2026-03-08"), &tz).unwrap();
    assert_eq!(start, ts("2026-03-08T06:00:00Z"));
    assert_eq!(end, ts("2026-03-09T05:00:00Z"));
    assert_eq!((end - start).num_hours(), 23);

    // Fall back (2026-11-01): 25-hour local day.
    let (start, end) = almanac_core::db::day_bounds(day("2026-11-01"), &tz).unwrap();
    assert_eq!((end - start).num_hours(), 25);
}

// ------------------------------------------------- cross-source dedup ----

fn calendar_email_object(
    native_id: &str,
    when: DateTime<Utc>,
    subject: &str,
    body_html: &str,
) -> (SourceObject, ExtractedItem) {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body_html);
    let raw = json!({
        "id": native_id,
        "payload": {
            "headers": [
                { "name": "From", "value": "Google Calendar <calendar-notification@google.com>" },
                { "name": "Subject", "value": subject }
            ],
            "mimeType": "text/html",
            "body": { "size": body_html.len(), "data": data }
        },
        "internalDate": when.timestamp_millis().to_string()
    });
    let provenance = ProvenanceRef {
        source: SourceId::Gmail,
        native_id: native_id.to_string(),
        deep_link: format!("https://mail.google.com/mail/#all/{native_id}"),
    };
    let obj = SourceObject {
        provenance: provenance.clone(),
        raw: RawContent::new(raw),
        occurred_at: when,
    };
    let it = ExtractedItem::new(
        ItemKind::Event,
        subject.to_string(),
        provenance,
        ExtractionSignals { rule_hits: vec![], embedding_scores: None, decided_by: "test".into() },
        when,
    )
    .unwrap();
    (obj, it)
}

fn gcal_event_object(event_id: &str, when: DateTime<Utc>) -> (SourceObject, ExtractedItem) {
    let provenance = ProvenanceRef {
        source: SourceId::GoogleCalendar,
        native_id: event_id.to_string(),
        deep_link: "https://www.google.com/calendar/event?eid=abc".to_string(),
    };
    let obj = SourceObject {
        provenance: provenance.clone(),
        raw: RawContent::new(json!({ "id": event_id, "summary": "Meeting" })),
        occurred_at: when,
    };
    let it = ExtractedItem::new(
        ItemKind::Event,
        "Meeting".to_string(),
        provenance,
        ExtractionSignals { rule_hits: vec![], embedding_scores: None, decided_by: "test".into() },
        when,
    )
    .unwrap();
    (obj, it)
}

#[test]
fn dedup_suppresses_calendar_email_that_references_its_briefed_event() {
    use base64::Engine as _;
    let (_dir, conn) = test_db();
    let when = ts("2026-07-11T15:00:00Z");
    let event_id = "vdgh2ak4uln8e5tbi2scj57444"; // 26 chars, like real ids

    let (event_obj, event_item) = gcal_event_object(event_id, when);
    store(&conn, &event_obj, &event_item);

    // The email body carries the real calendar link: eid = b64("<id> <cal>").
    let eid = base64::engine::general_purpose::STANDARD
        .encode(format!("{event_id} user@example.com"));
    let body = format!("<a href=\"https://www.google.com/calendar/event?eid={eid}\">View</a>");
    let (mail_obj, mail_item) =
        calendar_email_object("mailA", when, "Invitation: Meeting", &body);
    store(&conn, &mail_obj, &mail_item);

    let (kept, suppressed) = almanac_core::synth::dedup_calendar_email_duplicates(
        &conn,
        vec![event_item.clone(), mail_item],
    )
    .unwrap();
    assert_eq!(suppressed, 1, "email referencing the briefed event must be suppressed");
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].provenance().source, SourceId::GoogleCalendar);

    // Both source objects and extracted items REMAIN stored (selection-only).
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM source_objects WHERE source = 'gmail'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn dedup_suppresses_agenda_digest_when_calendar_items_present() {
    let (_dir, conn) = test_db();
    let when = ts("2026-07-11T15:00:00Z");
    let (event_obj, event_item) = gcal_event_object("someevent1234567890abcdefg", when);
    store(&conn, &event_obj, &event_item);
    let (mail_obj, mail_item) =
        calendar_email_object("mailB", when, "1 event happening tomorrow", "agenda digest body");
    store(&conn, &mail_obj, &mail_item);

    let (kept, suppressed) = almanac_core::synth::dedup_calendar_email_duplicates(
        &conn,
        vec![event_item, mail_item],
    )
    .unwrap();
    assert_eq!(suppressed, 1);
    assert_eq!(kept.len(), 1);
}

#[test]
fn dedup_is_conservative_no_gcal_items_or_no_match_keeps_everything() {
    let (_dir, conn) = test_db();
    let when = ts("2026-07-11T15:00:00Z");

    // Calendar email with NO calendar item in the selection → kept.
    let (mail_obj, mail_item) =
        calendar_email_object("mailC", when, "1 event happening tomorrow", "body");
    store(&conn, &mail_obj, &mail_item);
    let (kept, suppressed) =
        almanac_core::synth::dedup_calendar_email_duplicates(&conn, vec![mail_item.clone()])
            .unwrap();
    assert_eq!(suppressed, 0, "no calendar items present → nothing suppressed");
    assert_eq!(kept.len(), 1);

    // Calendar-sender email that neither matches an eid nor is a digest → kept.
    let (event_obj, event_item) = gcal_event_object("otherevent123456789012345z", when);
    store(&conn, &event_obj, &event_item);
    let (mail2_obj, mail2_item) = calendar_email_object(
        "mailD",
        when,
        "Notification: calendar settings changed",
        "no event link here",
    );
    store(&conn, &mail2_obj, &mail2_item);
    let (kept, suppressed) = almanac_core::synth::dedup_calendar_email_duplicates(
        &conn,
        vec![event_item, mail2_item],
    )
    .unwrap();
    assert_eq!(suppressed, 0, "non-matching calendar email must be kept (high precision)");
    assert_eq!(kept.len(), 2);

    // Items from other sources are never touched.
    let ordinary = item("ord.1", ItemKind::ActionNeeded, when);
    let ordinary_obj = source_object("ord.1", when);
    store(&conn, &ordinary_obj, &ordinary);
    let (kept, suppressed) =
        almanac_core::synth::dedup_calendar_email_duplicates(&conn, vec![ordinary]).unwrap();
    assert_eq!(suppressed, 0);
    assert_eq!(kept.len(), 1);
}

// -------------------------------- today-vs-tomorrow (F-1) + preview ----

/// Orders items in input order (no model) so pipeline behavior can be tested.
struct IdentityBackend;

#[async_trait]
impl SynthesisBackend for IdentityBackend {
    fn backend_id(&self) -> &str {
        "test-identity"
    }
    async fn synthesize(&self, items: Vec<ExtractedItem>, _ctx: DayContext) -> Result<Briefing> {
        let sequence = items
            .iter()
            .map(|i| PlannedItem {
                provenance: i.provenance().clone(),
                kind: i.kind(),
                summary: i.summary().to_string(),
                occurred_at: i.occurred_at(),
            })
            .collect();
        Ok(Briefing { sequence, rationale: "identity".into(), preview: vec![] })
    }
}

/// A UTC instant that is local noon on `date` (avoids DST-midnight edges).
fn local_noon(date: NaiveDate) -> DateTime<Utc> {
    use chrono::TimeZone;
    chrono::Local
        .from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn pick_briefing_day_prefers_today_and_never_a_future_day() {
    let (_dir, conn) = test_db();
    let today = chrono::Local::now().date_naive();
    let tomorrow = today.succ_opt().unwrap();

    // Today's action + tomorrow's event (the F-1 hijack scenario).
    store(&conn, &source_object("t.1", local_noon(today)),
        &item("t.1", ItemKind::ActionNeeded, local_noon(today)));
    store(&conn, &source_object("m.1", local_noon(tomorrow)),
        &item("m.1", ItemKind::Event, local_noon(tomorrow)));

    // Must be TODAY, not tomorrow.
    assert_eq!(almanac_core::db::pick_briefing_day(&conn, today).unwrap(), today);
}

#[test]
fn pick_briefing_day_falls_back_to_prior_never_future() {
    let (_dir, conn) = test_db();
    let today = chrono::Local::now().date_naive();
    let tomorrow = today.succ_opt().unwrap();
    let two_days_ago = today.pred_opt().unwrap().pred_opt().unwrap();

    // Nothing today; a future event and a prior action.
    store(&conn, &source_object("future.1", local_noon(tomorrow)),
        &item("future.1", ItemKind::Event, local_noon(tomorrow)));
    store(&conn, &source_object("past.1", local_noon(two_days_ago)),
        &item("past.1", ItemKind::ActionNeeded, local_noon(two_days_ago)));

    // The prior day, never the future one.
    assert_eq!(almanac_core::db::pick_briefing_day(&conn, today).unwrap(), two_days_ago);
}

#[test]
fn pick_briefing_day_ignores_items_reclassified_to_noise() {
    // F-11: an item first classified action, later re-extracted as noise,
    // must not nominate its day.
    let (_dir, conn) = test_db();
    let today = chrono::Local::now().date_naive();
    let when = local_noon(today);
    almanac_core::db::insert_source_object(&conn, &source_object("r.1", when)).unwrap();
    almanac_core::db::insert_extracted_item(&conn, &item("r.1", ItemKind::ActionNeeded, when)).unwrap();
    almanac_core::db::insert_extracted_item(&conn, &item("r.1", ItemKind::Noise, when)).unwrap();

    // Today now has no non-noise item → today is not nominated by content;
    // with nothing else present, the picker still returns today (empty brief).
    assert_eq!(almanac_core::db::pick_briefing_day(&conn, today).unwrap(), today);
    // And run-scoped selection yields nothing for today.
    let (s, e) = almanac_core::db::day_bounds(today, &chrono::Local).unwrap();
    let inputs = almanac_core::db::briefing_inputs_range(&conn, s, e).unwrap();
    assert!(inputs.iter().all(|i| i.kind() == ItemKind::Noise));
}

#[tokio::test]
async fn tomorrows_events_go_to_preview_not_todays_plan() {
    let (_dir, conn) = test_db();
    let today = chrono::Local::now().date_naive();
    let tomorrow = today.succ_opt().unwrap();

    store(&conn, &source_object("today.act", local_noon(today)),
        &item("today.act", ItemKind::ActionNeeded, local_noon(today)));
    store(&conn, &source_object("tom.evt", local_noon(tomorrow)),
        &item("tom.evt", ItemKind::Event, local_noon(tomorrow)));

    let ctx = DayContext { date: today, now: Utc::now() };
    let briefing = generate_briefing(&conn, &IdentityBackend, ctx).await.unwrap();

    // Today's plan contains ONLY today's item.
    assert_eq!(briefing.sequence.len(), 1);
    assert_eq!(briefing.sequence[0].provenance.native_id, "today.act");
    // Tomorrow's event is in the preview, not the plan.
    assert_eq!(briefing.preview.len(), 1);
    assert_eq!(briefing.preview[0].provenance.native_id, "tom.evt");
    assert_eq!(briefing.preview[0].kind, ItemKind::Event);
}

// --------------------------------------------------------- empty day ----

/// Backend that must never be called.
struct UnreachableBackend;

#[async_trait]
impl SynthesisBackend for UnreachableBackend {
    fn backend_id(&self) -> &str {
        "test-unreachable"
    }
    async fn synthesize(&self, _items: Vec<ExtractedItem>, _ctx: DayContext) -> Result<Briefing> {
        panic!("backend must not be invoked for an empty day")
    }
}

#[tokio::test]
async fn empty_day_briefs_without_invoking_the_backend() {
    let (_dir, conn) = test_db();
    let ctx = DayContext { date: day("2026-01-01"), now: Utc::now() };
    let briefing = generate_briefing(&conn, &UnreachableBackend, ctx).await.unwrap();
    assert!(briefing.sequence.is_empty());
    assert!(!briefing.rationale.is_empty());
}

#[tokio::test]
async fn noise_only_day_briefs_empty_but_counts_noise() {
    let (_dir, conn) = test_db();
    let when = ts("2026-07-11T10:00:00Z");
    store(&conn, &source_object("n.1", when), &item("n.1", ItemKind::Noise, when));

    let ctx = DayContext { date: day("2026-07-11"), now: Utc::now() };
    let briefing = generate_briefing(&conn, &UnreachableBackend, ctx).await.unwrap();
    assert!(briefing.sequence.is_empty());
    assert!(briefing.rationale.contains("noise"), "rationale: {}", briefing.rationale);
}

// ------------------------------------------------ model-gated e2e ----

fn model_dir(name: &str) -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("models").join(name);
    if dir.exists() {
        Some(dir)
    } else {
        eprintln!("SKIP: models/{name} not present — synthesis model tests not run here");
        None
    }
}

#[tokio::test]
async fn local_llm_synthesizes_a_grounded_briefing_for_a_realistic_day() {
    let Some(dir) = model_dir("qwen2.5-0.5b-instruct") else { return };
    let backend = almanac_core::synth::local_llm::LocalLlmBackend::load(&dir).unwrap();

    let (_tmp, conn) = test_db();
    let d = "2026-07-11";
    let mk = |id: &str, kind, summary: &str, at: &str| {
        let when = ts(at);
        let obj = SourceObject {
            provenance: provenance(id),
            raw: RawContent::new(json!({"text": summary, "ts": id})),
            occurred_at: when,
        };
        let it = ExtractedItem::new(
            kind,
            summary.to_string(),
            provenance(id),
            ExtractionSignals {
                rule_hits: vec![],
                embedding_scores: None,
                decided_by: "test".into(),
            },
            when,
        )
        .unwrap();
        store(&conn, &obj, &it);
    };
    mk("1.1", ItemKind::Event, "Standup with platform team", &format!("{d}T09:30:00Z"));
    mk("1.2", ItemKind::Event, "Interview debrief with recruiter", &format!("{d}T16:00:00Z"));
    mk("1.3", ItemKind::ActionNeeded, "Contract awaiting your signature before it expires", &format!("{d}T08:10:00Z"));
    mk("1.4", ItemKind::ActionNeeded, "Reviewer feedback blocking the release", &format!("{d}T11:00:00Z"));
    mk("1.5", ItemKind::Commitment, "Send the updated deck to the client", &format!("{d}T07:45:00Z"));
    mk("1.6", ItemKind::Noise, "Weekly newsletter digest", &format!("{d}T06:00:00Z"));

    let ctx = DayContext { date: day(d), now: Utc::now() };
    let started = std::time::Instant::now();
    let briefing = generate_briefing(&conn, &backend, ctx).await.unwrap();
    let elapsed = started.elapsed();

    // All five actionable items, each exactly once, all grounded (validated
    // inside generate_briefing); noise excluded from sequence but counted.
    assert_eq!(briefing.sequence.len(), 5);
    assert!(briefing.rationale.len() > 40, "rationale too thin: {}", briefing.rationale);
    assert!(briefing.rationale.contains("noise"));

    eprintln!("sample briefing (synthesis took {elapsed:?}):");
    for (i, it) in briefing.sequence.iter().enumerate() {
        eprintln!("  {}. [{}] {}", i + 1, it.kind, it.summary);
    }
    eprintln!("  why: {}", briefing.rationale);
}
