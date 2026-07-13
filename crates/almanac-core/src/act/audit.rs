//! Hash-chained, append-only audit log (ARCHITECTURE_V2 §6, the ProofPilot
//! pattern). Every proposal state transition and every executed action
//! appends a record; `verify_chain` walks the whole chain and fails loudly on
//! any break.
//!
//! INVARIANT L1 (audit-before-acknowledge) is enforced by the CALLERS in
//! `act`: transitions append their record in the same SQLite transaction as
//! the state change, and executors append `execution_started` in a committed
//! transaction BEFORE the network side effect is attempted.
//!
//! Threat model (stated honestly, per ARCHITECTURE_V2 §11): the chain
//! protects against silent tampering by malware or a curious co-user editing
//! the SQLite file; it is not a cryptographic notary — there is no external
//! anchor.

use anyhow::{bail, ensure, Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

/// Fixed genesis record (also inserted by migration 0005 — a test asserts the
/// two never drift apart).
pub const GENESIS_TS: &str = "2026-01-01T00:00:00Z";
pub const GENESIS_ACTOR: &str = "genesis";
pub const GENESIS_EVENT: &str = "genesis";
/// SHA-256 of the empty string.
pub const GENESIS_PAYLOAD_HASH: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Canonical serialization hashed into `record_hash`. Newline-delimited so no
/// two distinct records can serialize to the same string (none of the fields
/// may contain a newline; `append` enforces that).
pub fn canonical_record_string(
    seq: i64,
    ts: &str,
    actor: &str,
    event: &str,
    proposal_id: Option<i64>,
    payload_hash: &str,
    prev_hash: &str,
) -> String {
    let pid = proposal_id.map(|i| i.to_string()).unwrap_or_default();
    format!("{seq}\n{ts}\n{actor}\n{event}\n{pid}\n{payload_hash}\n{prev_hash}")
}

pub fn record_hash(
    seq: i64,
    ts: &str,
    actor: &str,
    event: &str,
    proposal_id: Option<i64>,
    payload_hash: &str,
    prev_hash: &str,
) -> String {
    sha256_hex(
        canonical_record_string(seq, ts, actor, event, proposal_id, payload_hash, prev_hash)
            .as_bytes(),
    )
}

/// One row of the chain, as read back.
#[derive(Debug, Clone)]
pub struct AuditRecord {
    pub seq: i64,
    pub ts: String,
    pub actor: String,
    pub event: String,
    pub proposal_id: Option<i64>,
    pub payload_hash: String,
    pub prev_hash: String,
    pub record_hash: String,
}

/// Append one record to the chain. Call INSIDE the transaction that performs
/// the state change it describes (L1) — `&Transaction` derefs to
/// `&Connection`, so both work here. Returns the appended record's seq.
pub fn append(
    conn: &Connection,
    actor: &str,
    event: &str,
    proposal_id: Option<i64>,
    payload_hash: &str,
) -> Result<i64> {
    for (name, v) in [("actor", actor), ("event", event), ("payload_hash", payload_hash)] {
        ensure!(
            !v.contains('\n') && !v.contains('\r'),
            "audit {name} must not contain newlines (canonical hash input)"
        );
    }
    ensure!(
        payload_hash.len() == 64 && payload_hash.chars().all(|c| c.is_ascii_hexdigit()),
        "payload_hash must be a sha256 hex digest"
    );

    let (prev_seq, prev_hash): (i64, String) = conn
        .query_row(
            "SELECT seq, record_hash FROM audit_records ORDER BY seq DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .context("audit chain has no genesis record — database not migrated?")?;

    let seq = prev_seq + 1;
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let hash = record_hash(seq, &ts, actor, event, proposal_id, payload_hash, &prev_hash);
    conn.execute(
        "INSERT INTO audit_records (seq, ts, actor, event, proposal_id, payload_hash, prev_hash, record_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (seq, &ts, actor, event, proposal_id, payload_hash, &prev_hash, &hash),
    )
    .context("appending audit record")?;
    Ok(seq)
}

/// Report from a successful chain verification.
#[derive(Debug)]
pub struct ChainReport {
    pub records: usize,
    pub head_seq: i64,
    pub head_hash: String,
}

/// Walk the entire chain: genesis must match the fixed constants, seqs must
/// be contiguous from 0, every prev_hash must equal the previous record_hash,
/// and every record_hash must recompute. Any break is a hard error naming the
/// exact seq.
pub fn verify_chain(conn: &Connection) -> Result<ChainReport> {
    let mut stmt = conn.prepare(
        "SELECT seq, ts, actor, event, proposal_id, payload_hash, prev_hash, record_hash
         FROM audit_records ORDER BY seq ASC",
    )?;
    let rows: Vec<AuditRecord> = stmt
        .query_map([], |row| {
            Ok(AuditRecord {
                seq: row.get(0)?,
                ts: row.get(1)?,
                actor: row.get(2)?,
                event: row.get(3)?,
                proposal_id: row.get(4)?,
                payload_hash: row.get(5)?,
                prev_hash: row.get(6)?,
                record_hash: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let Some(genesis) = rows.first() else {
        bail!("audit chain is empty — no genesis record (database not migrated?)");
    };
    ensure!(
        genesis.seq == 0
            && genesis.ts == GENESIS_TS
            && genesis.actor == GENESIS_ACTOR
            && genesis.event == GENESIS_EVENT
            && genesis.proposal_id.is_none()
            && genesis.payload_hash == GENESIS_PAYLOAD_HASH
            && genesis.prev_hash == GENESIS_PREV_HASH,
        "audit chain BROKEN at seq 0: genesis record does not match the fixed genesis"
    );

    let mut prev_hash = String::new();
    for (i, r) in rows.iter().enumerate() {
        ensure!(
            r.seq == i as i64,
            "audit chain BROKEN: seq not contiguous — expected {} but found {} (record missing or reordered)",
            i,
            r.seq
        );
        if r.seq > 0 {
            ensure!(
                r.prev_hash == prev_hash,
                "audit chain BROKEN at seq {}: prev_hash does not match record {}'s hash",
                r.seq,
                r.seq - 1
            );
        }
        let recomputed = record_hash(
            r.seq,
            &r.ts,
            &r.actor,
            &r.event,
            r.proposal_id,
            &r.payload_hash,
            &r.prev_hash,
        );
        ensure!(
            recomputed == r.record_hash,
            "audit chain BROKEN at seq {}: record content does not match its hash (tampered?)",
            r.seq
        );
        prev_hash = r.record_hash.clone();
    }

    Ok(ChainReport {
        records: rows.len(),
        head_seq: rows.last().map(|r| r.seq).unwrap_or(0),
        head_hash: prev_hash,
    })
}

/// Most recent `limit` records, newest first (diagnostics / UI receipts).
pub fn tail(conn: &Connection, limit: usize) -> Result<Vec<AuditRecord>> {
    let mut stmt = conn.prepare(
        "SELECT seq, ts, actor, event, proposal_id, payload_hash, prev_hash, record_hash
         FROM audit_records ORDER BY seq DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([limit as i64], |row| {
            Ok(AuditRecord {
                seq: row.get(0)?,
                ts: row.get(1)?,
                actor: row.get(2)?,
                event: row.get(3)?,
                proposal_id: row.get(4)?,
                payload_hash: row.get(5)?,
                prev_hash: row.get(6)?,
                record_hash: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The genesis record_hash hardcoded in migration 0005 must equal the
    /// hash recomputed from the constants here — neither can silently drift.
    #[test]
    fn genesis_constants_recompute_to_the_migrated_hash() {
        let computed = record_hash(
            0,
            GENESIS_TS,
            GENESIS_ACTOR,
            GENESIS_EVENT,
            None,
            GENESIS_PAYLOAD_HASH,
            GENESIS_PREV_HASH,
        );
        assert_eq!(
            computed,
            "34b6463e473a193a5cf6572a1142c932cc0a32394067c0f9207ef76eb090690a",
            "genesis hash drifted from the value in 0005_action_layer.sql"
        );
        assert_eq!(GENESIS_PAYLOAD_HASH, sha256_hex(b""), "genesis payload must be sha256 of empty");
    }
}
