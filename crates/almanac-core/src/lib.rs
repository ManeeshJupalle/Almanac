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
