//! Almanac core — the headless engine crate.
//!
//! Per ARCHITECTURE.md ("Headless-runnable core"), this crate builds, runs,
//! and is testable without the Tauri shell. The shell is a client of this
//! crate, never the engine.

pub mod adapters;
pub mod auth;
pub mod db;
pub mod extract;
pub mod synth;
pub mod types;

use std::path::PathBuf;

use anyhow::{ensure, Context, Result};

/// Default on-disk location of the Almanac database:
/// `<platform data dir>/Almanac/almanac.db`
/// (Windows: `%APPDATA%\Almanac\almanac.db`).
pub fn default_db_path() -> Result<PathBuf> {
    let base = dirs::data_dir().context("no platform data directory available")?;
    Ok(base.join("Almanac").join("almanac.db"))
}

/// Open the default database — creating it on first run — and bring its
/// schema up to date. Returns the path actually used.
pub fn init_default_db() -> Result<PathBuf> {
    let path = default_db_path()?;
    let mut conn = db::open(&path)?;
    db::migrate(&mut conn)?;
    Ok(path)
}

/// Locate the models/ directory: ALMANAC_MODELS_DIR override, else walk up
/// from the current directory (the Tauri shell runs with cwd=src-tauri, the
/// CLI from the repo root — both find the same repo-root models/).
pub fn models_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("ALMANAC_MODELS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let mut dir = std::env::current_dir()?;
    loop {
        let candidate = dir.join("models");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        if !dir.pop() {
            anyhow::bail!(
                "models/ directory not found in any ancestor of the working directory \
                 (set ALMANAC_MODELS_DIR or fetch models per README)"
            );
        }
    }
}

/// The full live chain (Phase 5): adapters → extraction → synthesis →
/// persisted briefing. Returns the stored briefing read back from SQLite.
/// Engine logic lives HERE — the UI shell only calls this via IPC.
pub async fn live_briefing() -> Result<db::StoredBriefing> {
    use adapters::SourceAdapter;
    // The window reaches one day FORWARD: a daily briefing must include
    // today's upcoming calendar events, not just what already happened.
    let window = types::TimeWindow {
        start: chrono::Utc::now() - chrono::Duration::days(7),
        end: chrono::Utc::now() + chrono::Duration::days(1),
    };

    let mut objects = Vec::new();
    let mut gmail = adapters::gmail::GmailAdapter::from_env()?;
    gmail.authenticate().await?;
    objects.extend(gmail.fetch_window(window).await?);
    let mut gcal = adapters::gcal::CalendarAdapter::from_env()?;
    gcal.authenticate().await?;
    objects.extend(gcal.fetch_window(window).await?);
    let mut slack = adapters::slack::SlackAdapter::from_env()?;
    slack.authenticate().await?;
    objects.extend(slack.fetch_window(window).await?);

    let models = models_dir()?;
    let extractor = extract::Extractor::with_model(&models.join("minilm"))?;
    let items = extractor.extract(&objects)?;

    let db_path = init_default_db()?;
    let mut conn = db::open(&db_path)?;
    for (obj, item) in objects.iter().zip(&items) {
        db::insert_source_object(&conn, obj)?;
        db::insert_extracted_item(&conn, item)?;
    }

    // Brief the most recent LOCAL day that has a non-noise item (Phase 6:
    // day boundaries follow the user's timezone, not UTC).
    let latest: Option<String> = {
        use rusqlite::OptionalExtension;
        conn.query_row(
            "SELECT so.occurred_at
             FROM extracted_items ei
             JOIN source_objects so
               ON so.source = ei.source AND so.native_id = ei.native_id
             WHERE ei.kind != 'noise'
             ORDER BY datetime(so.occurred_at) DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?
    };
    let latest = latest.context("no non-noise items found in the last 7 days — nothing to brief")?;
    let date = chrono::DateTime::parse_from_rfc3339(&latest)?
        .with_timezone(&chrono::Local)
        .date_naive();

    let backend = synth::local_llm::LocalLlmBackend::load(&models.join("qwen2.5-0.5b-instruct"))?;
    let ctx = synth::DayContext { date, now: chrono::Utc::now() };
    let briefing = synth::generate_briefing(&conn, &backend, ctx).await?;
    db::insert_briefing(&mut conn, date, synth::SynthesisBackend::backend_id(&backend), &briefing)?;
    db::latest_briefing(&conn)?.context("briefing vanished after insert")
}

/// Headless sanity check backing `almanac-core --self-check`: initialize the
/// database and verify every embedded migration is recorded as applied.
pub fn self_check() -> Result<PathBuf> {
    let path = init_default_db()?;
    let conn = db::open(&path)?;
    let applied = db::applied_versions(&conn)?;
    ensure!(
        applied.len() == db::MIGRATIONS.len(),
        "expected {} applied migrations, found {}",
        db::MIGRATIONS.len(),
        applied.len()
    );
    Ok(path)
}
