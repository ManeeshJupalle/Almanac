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
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "init",
    sql: include_str!("../migrations/0001_init.sql"),
}];

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
    fn phase0_schema_has_no_domain_tables() {
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

        assert_eq!(tables, vec!["schema_migrations".to_string()]);
    }
}
