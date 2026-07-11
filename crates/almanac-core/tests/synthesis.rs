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

    let inputs = almanac_core::db::briefing_inputs(&conn, day("2026-07-11")).unwrap();
    assert_eq!(inputs.len(), 1, "dedup must collapse re-extractions");
    assert_eq!(inputs[0].kind(), ItemKind::ActionNeeded, "latest run wins");
}

#[test]
fn run_scoping_selects_only_the_requested_day() {
    let (_dir, conn) = test_db();
    let d10 = ts("2026-07-10T23:00:00Z");
    let d11 = ts("2026-07-11T01:00:00Z");
    store(&conn, &source_object("10.1", d10), &item("10.1", ItemKind::ActionNeeded, d10));
    store(&conn, &source_object("11.1", d11), &item("11.1", ItemKind::ActionNeeded, d11));

    let inputs = almanac_core::db::briefing_inputs(&conn, day("2026-07-11")).unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].provenance().native_id, "11.1");
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
