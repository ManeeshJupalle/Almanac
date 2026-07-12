//! SQLite connection + embedded migration runner.
//!
//! Migrations are compiled into the binary (`include_str!`) and applied in
//! order inside transactions, tracked in a `schema_migrations` table.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

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
    Migration {
        version: 3,
        name: "briefings",
        sql: include_str!("../migrations/0003_briefings.sql"),
    },
    Migration {
        version: 4,
        name: "briefing_sections",
        sql: include_str!("../migrations/0004_briefing_sections.sql"),
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

/// Choose which day to brief, given the user's local `today` (post-audit F-1
/// fix). Rule: brief TODAY when today has a non-noise item; otherwise fall
/// back to the most recent PRIOR local day that has one; never a future day
/// (a standing event tomorrow must not hijack the briefing). When there is no
/// content today or earlier, still return `today` — the day briefs empty and
/// the tomorrow-preview can populate.
///
/// Run-scoped (F-11): only the latest extraction per source object counts, so
/// an item reclassified to noise no longer nominates its day.
pub fn pick_briefing_day(conn: &Connection, today: chrono::NaiveDate) -> Result<chrono::NaiveDate> {
    let mut stmt = conn.prepare(
        "SELECT so.occurred_at
         FROM extracted_items ei
         JOIN source_objects so
           ON so.source = ei.source AND so.native_id = ei.native_id
         WHERE ei.kind != 'noise'
           AND ei.id = (SELECT MAX(id) FROM extracted_items
                        WHERE source = ei.source AND native_id = ei.native_id)",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;

    let mut best_prior: Option<chrono::NaiveDate> = None;
    for row in rows {
        let local_date = chrono::DateTime::parse_from_rfc3339(&row?)?
            .with_timezone(&chrono::Local)
            .date_naive();
        if local_date == today {
            return Ok(today); // today has content — brief today
        }
        if local_date < today {
            best_prior = Some(best_prior.map_or(local_date, |b| b.max(local_date)));
        }
        // future days are ignored for day-selection (preview handles them)
    }
    Ok(best_prior.unwrap_or(today))
}

/// UTC bounds of a calendar day in an arbitrary timezone (Phase 6: briefing
/// days are the USER'S local day, not UTC). Generic over TimeZone so tests
/// can pin fixed offsets and real DST-observing zones.
pub fn day_bounds<Tz: chrono::TimeZone>(
    date: chrono::NaiveDate,
    tz: &Tz,
) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    let midnight = |d: chrono::NaiveDate| -> Result<chrono::DateTime<chrono::Utc>> {
        let naive = d.and_hms_opt(0, 0, 0).context("invalid midnight")?;
        // DST can make local midnight ambiguous or nonexistent; earliest()
        // picks the first valid instant, matching user intuition of "the
        // start of the day".
        let local = tz
            .from_local_datetime(&naive)
            .earliest()
            .with_context(|| format!("no valid local midnight for {d}"))?;
        Ok(local.with_timezone(&chrono::Utc))
    };
    Ok((midnight(date)?, midnight(date.succ_opt().context("date overflow")?)?))
}

/// Briefing inputs for a half-open UTC range [start, end) — the run-scoping
/// rule (Phase 4): for every (source, native_id) only the LATEST extraction
/// (max rowid) counts, so re-running extraction never yields stale or
/// duplicate items. Ordered by occurrence time.
pub fn briefing_inputs_range(
    conn: &Connection,
    start_utc: chrono::DateTime<chrono::Utc>,
    end_utc: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<crate::extract::ExtractedItem>> {
    let mut stmt = conn.prepare(
        "SELECT ei.source, ei.native_id, ei.kind, ei.summary, ei.signals_json,
                so.deep_link, so.occurred_at
         FROM extracted_items ei
         JOIN source_objects so
           ON so.source = ei.source AND so.native_id = ei.native_id
         WHERE datetime(so.occurred_at) >= datetime(?1)
           AND datetime(so.occurred_at) < datetime(?2)
           AND ei.id = (SELECT MAX(id) FROM extracted_items
                        WHERE source = ei.source AND native_id = ei.native_id)
         ORDER BY so.occurred_at ASC, ei.native_id ASC",
    )?;
    let rows = stmt.query_map([start_utc.to_rfc3339(), end_utc.to_rfc3339()], |row| {
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

/// Raw payload of a stored source object (LOCAL read only — used by the
/// cross-source dedup to inspect calendar-notification emails on-device).
pub fn source_raw_json(
    conn: &Connection,
    source: crate::types::SourceId,
    native_id: &str,
) -> Result<Option<serde_json::Value>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT raw_json FROM source_objects WHERE source = ?1 AND native_id = ?2",
            (source.to_string(), native_id),
            |row| row.get(0),
        )
        .optional()?;
    Ok(match raw {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    })
}

/// A briefing as read back from the local store. Deep links come from an
/// INNER JOIN on source_objects — an item that does not resolve cannot be
/// loaded, so the UI can never render ungrounded provenance.
#[derive(Debug)]
pub struct StoredBriefing {
    pub id: i64,
    pub briefing_date: chrono::NaiveDate,
    pub backend_id: String,
    pub rationale: String,
    pub created_at: String,
    /// The briefed day's sequenced plan.
    pub items: Vec<StoredBriefingItem>,
    /// Tomorrow's events (chronological), shown in a separate preview.
    pub preview: Vec<StoredBriefingItem>,
}

#[derive(Debug)]
pub struct StoredBriefingItem {
    pub position: i64,
    pub kind: crate::extract::ItemKind,
    pub summary: String,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub provenance: crate::types::ProvenanceRef,
}

/// Persist a validated briefing (transactional; FK enforces grounding).
pub fn insert_briefing(
    conn: &mut Connection,
    briefing_date: chrono::NaiveDate,
    backend_id: &str,
    briefing: &crate::synth::Briefing,
) -> Result<i64> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO briefings (briefing_date, backend_id, rationale) VALUES (?1, ?2, ?3)",
        (briefing_date.to_string(), backend_id, &briefing.rationale),
    )?;
    let briefing_id = tx.last_insert_rowid();
    // `position` is the per-briefing PK; preview rows continue after the plan
    // so they stay unique, and the `section` column keeps them distinct on read.
    let base = briefing.sequence.len();
    let plan = briefing.sequence.iter().enumerate().map(|(i, it)| (i, it, "today"));
    let preview = briefing.preview.iter().enumerate().map(|(i, it)| (base + i, it, "preview"));
    for (position, item, section) in plan.chain(preview) {
        tx.execute(
            "INSERT INTO briefing_items
                 (briefing_id, position, source, native_id, kind, summary, occurred_at, section)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                briefing_id,
                position as i64,
                item.provenance.source.to_string(),
                &item.provenance.native_id,
                item.kind.as_str(),
                &item.summary,
                item.occurred_at.to_rfc3339(),
                section,
            ),
        )
        .context("persisting briefing item (FK failure here means ungrounded provenance)")?;
    }
    tx.commit()?;
    Ok(briefing_id)
}

/// Most recently created briefing, if any.
pub fn latest_briefing(conn: &Connection) -> Result<Option<StoredBriefing>> {
    let head = conn
        .query_row(
            "SELECT id, briefing_date, backend_id, rationale, created_at
             FROM briefings ORDER BY id DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((id, date, backend_id, rationale, created_at)) = head else {
        return Ok(None);
    };

    let mut stmt = conn.prepare(
        "SELECT bi.position, bi.kind, bi.summary, bi.occurred_at,
                bi.source, bi.native_id, so.deep_link, bi.section
         FROM briefing_items bi
         JOIN source_objects so
           ON so.source = bi.source AND so.native_id = bi.native_id
         WHERE bi.briefing_id = ?1
         ORDER BY bi.position ASC",
    )?;
    let rows = stmt.query_map([id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;

    let mut items = Vec::new();
    let mut preview = Vec::new();
    for row in rows {
        let (position, kind, summary, occurred_at, source, native_id, deep_link, section) = row?;
        let item = StoredBriefingItem {
            position,
            kind: crate::extract::ItemKind::parse(&kind)
                .with_context(|| format!("unknown kind '{kind}' in briefing_items"))?,
            summary,
            occurred_at: chrono::DateTime::parse_from_rfc3339(&occurred_at)?
                .with_timezone(&chrono::Utc),
            provenance: crate::types::ProvenanceRef {
                source: crate::types::SourceId::parse(&source)
                    .with_context(|| format!("unknown source '{source}' in briefing_items"))?,
                native_id,
                deep_link,
            },
        };
        if section == "preview" {
            preview.push(item);
        } else {
            items.push(item);
        }
    }
    Ok(Some(StoredBriefing {
        id,
        briefing_date: date.parse()?,
        backend_id,
        rationale,
        created_at,
        items,
        preview,
    }))
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
                "briefing_items".to_string(),
                "briefings".to_string(),
                "extracted_items".to_string(),
                "schema_migrations".to_string(),
                "source_objects".to_string(),
            ]
        );
    }
}
