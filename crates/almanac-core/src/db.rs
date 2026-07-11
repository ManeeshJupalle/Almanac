//! SQLite connection + embedded migration runner.
//!
//! Migrations are compiled into the binary (`include_str!`) and applied in
//! order inside transactions, tracked in a `schema_migrations` table.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// A single schema migration, embedded at compile time.
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Ordered, append-only list of all migrations.
pub const MIGRATIONS: &[Migration] = &[
    Migration { version: 1, name: "init", sql: include_str!("../migrations/0001_init.sql") },
    Migration {
        version: 2,
        name: "extraction",
        sql: include_str!("../migrations/0002_extraction.sql"),
    },
];

/// Open the database at `path`, creating parent directories and the file on
/// first run.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("opening database at {}", path.display()))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(conn)
}

/// Apply all pending migrations. Returns how many were applied.
pub fn migrate(conn: &mut Connection) -> Result<usize> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )?;
    let already_applied = applied_versions(conn)?;
    let mut applied_now = 0;
    for m in MIGRATIONS {
        if already_applied.contains(&m.version) {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(m.sql)
            .with_context(|| format!("applying migration {} ({})", m.version, m.name))?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name) VALUES (?1, ?2)",
            (m.version, m.name),
        )?;
        tx.commit()?;
        applied_now += 1;
    }
    Ok(applied_now)
}

/// Store a fetched source object locally (raw stays on this machine — the
/// serialization here targets the local SQLite file only, via the deliberate
/// `RawContent::as_json()` accessor).
pub fn insert_source_object(conn: &Connection, obj: &crate::types::SourceObject) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO source_objects
             (source, native_id, deep_link, occurred_at, raw_json)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            obj.provenance.source.to_string(),
            &obj.provenance.native_id,
            &obj.provenance.deep_link,
            obj.occurred_at.to_rfc3339(),
            serde_json::to_string(obj.raw.as_json())?,
        ),
    )?;
    Ok(())
}

/// Persist an extracted item. The composite FOREIGN KEY to source_objects
/// makes this FAIL LOUDLY for any item whose provenance does not resolve to
/// a stored source object — the grounding invariant, enforced at rest.
pub fn insert_extracted_item(
    conn: &Connection,
    item: &crate::extract::ExtractedItem,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO extracted_items (source, native_id, kind, summary, signals_json)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            item.provenance().source.to_string(),
            &item.provenance().native_id,
            item.kind().as_str(),
            item.summary(),
            serde_json::to_string(item.signals())?,
        ),
    )
    .context("persisting extracted item (a foreign-key failure here means ungrounded provenance)")?;
    Ok(conn.last_insert_rowid())
}

/// Briefing inputs for a UTC day — the run-scoping rule (Phase 4):
/// for every (source, native_id) only the LATEST extraction (max rowid)
/// counts, so re-running extraction never yields stale or duplicate items;
/// selection is restricted to source objects whose occurred_at falls on the
/// given UTC date. Ordered by occurrence time.
pub fn briefing_inputs(
    conn: &Connection,
    day_utc: chrono::NaiveDate,
) -> Result<Vec<crate::extract::ExtractedItem>> {
    let mut stmt = conn.prepare(
        "SELECT ei.source, ei.native_id, ei.kind, ei.summary, ei.signals_json,
                so.deep_link, so.occurred_at
         FROM extracted_items ei
         JOIN source_objects so
           ON so.source = ei.source AND so.native_id = ei.native_id
         WHERE date(so.occurred_at) = ?1
           AND ei.id = (SELECT MAX(id) FROM extracted_items
                        WHERE source = ei.source AND native_id = ei.native_id)
         ORDER BY so.occurred_at ASC, ei.native_id ASC",
    )?;
    let rows = stmt.query_map([day_utc.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;

    let mut items = Vec::new();
    for row in rows {
        let (source, native_id, kind, summary, signals_json, deep_link, occurred_at) = row?;
        let source = crate::types::SourceId::parse(&source)
            .with_context(|| format!("unknown source '{source}' in extracted_items"))?;
        let kind = crate::extract::ItemKind::parse(&kind)
            .with_context(|| format!("unknown kind '{kind}' in extracted_items"))?;
        let signals: crate::extract::ExtractionSignals = serde_json::from_str(&signals_json)?;
        let occurred_at = chrono::DateTime::parse_from_rfc3339(&occurred_at)
            .with_context(|| format!("bad occurred_at '{occurred_at}' in source_objects"))?
            .with_timezone(&chrono::Utc);
        items.push(crate::extract::ExtractedItem::new(
            kind,
            summary,
            crate::types::ProvenanceRef { source, native_id, deep_link },
            signals,
            occurred_at,
        )?);
    }
    Ok(items)
}

/// (kind, count) rows for reporting.
pub fn count_items_by_kind(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn
        .prepare("SELECT kind, COUNT(*) FROM extracted_items GROUP BY kind ORDER BY kind")?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(String, i64)>>>()?;
    Ok(rows)
}

/// Versions recorded in `schema_migrations`, ascending.
pub fn applied_versions(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version")?;
    let versions = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(versions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_creates_db_and_applies_all_migrations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("almanac.db");
        assert!(!path.exists());

        let mut conn = open(&path).expect("open");
        let applied = migrate(&mut conn).expect("migrate");

        assert!(path.exists(), "db file should exist after first run");
        assert_eq!(applied, MIGRATIONS.len());
        assert_eq!(
            applied_versions(&conn).expect("applied_versions").len(),
            MIGRATIONS.len()
        );
    }

    #[test]
    fn migrate_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut conn = open(&dir.path().join("almanac.db")).expect("open");

        assert_eq!(migrate(&mut conn).expect("first run"), MIGRATIONS.len());
        assert_eq!(migrate(&mut conn).expect("second run"), 0);
    }

    #[test]
    fn schema_matches_applied_migrations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut conn = open(&dir.path().join("almanac.db")).expect("open");
        migrate(&mut conn).expect("migrate");

        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .expect("prepare");
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");

        assert_eq!(
            tables,
            vec![
                "extracted_items".to_string(),
                "schema_migrations".to_string(),
                "source_objects".to_string(),
            ]
        );
    }
}
