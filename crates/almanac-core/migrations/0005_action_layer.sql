-- 0005_action_layer (Phase 2.0): ActionProposals, their evidence chain, and
-- the hash-chained audit log (ARCHITECTURE_V2 §4-§6).
--
-- E2 at rest: proposal_evidence carries a composite FOREIGN KEY to
-- source_objects — exactly the v1 grounding pattern — so an evidence ref that
-- does not resolve to a stored artifact is unpersistable. Phase 2.0's only
-- artifact store is source_objects, so only source-object-backed evidence
-- kinds ('message', 'calendar_event' — both Tier-Hard by definition) are
-- persistable here. Git/CI/Jira kinds arrive with their own stores + FKs in
-- Phase 2.1/2.2, soft observation kinds with theirs in Phase 2.4/2.5 — all
-- via NEW migrations (never by editing this one).
--
-- A1 note: 'approved' is reachable only through the approval event API
-- (act::approve), which writes an audit record in the same transaction.

CREATE TABLE action_proposals (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    kind                 TEXT NOT NULL CHECK (kind IN ('gmail_reply', 'slack_post')),
    state                TEXT NOT NULL DEFAULT 'proposed'
                         CHECK (state IN ('proposed', 'approved', 'rejected', 'expired',
                                          'executed', 'execution_failed')),
    -- gmail_reply target: the stored message being replied to. The composite
    -- FK means we can only propose replying to a message we actually fetched
    -- and grounded. NULL for slack_post (SQLite skips composite FKs with any
    -- NULL column, by design here).
    target_source        TEXT,
    target_native_id     TEXT,
    -- slack_post target: the destination channel id. Channels are not source
    -- objects; the grounding requirement lives on the evidence rows.
    target_channel       TEXT,
    draft_subject        TEXT,
    draft_body           TEXT NOT NULL,
    asserts_work_done    INTEGER NOT NULL CHECK (asserts_work_done IN (0, 1)),
    backend_id           TEXT NOT NULL, -- drafting backend that produced the draft
    created_at           TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at           TEXT NOT NULL,
    -- Set exactly once, in the same transaction as the 'execution_started'
    -- audit record, BEFORE the network send (double-execute guard + L1).
    execution_claimed_at TEXT,
    -- Send/post response metadata (ids, ts) — never raw third-party content.
    receipt_json         TEXT,
    FOREIGN KEY (target_source, target_native_id) REFERENCES source_objects (source, native_id)
);

CREATE TABLE proposal_evidence (
    proposal_id INTEGER NOT NULL REFERENCES action_proposals (id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    -- Phase 2.0: only Tier-Hard kinds have an artifact store (see header).
    tier        TEXT NOT NULL CHECK (tier = 'hard'),
    kind        TEXT NOT NULL CHECK (kind IN ('message', 'calendar_event')),
    source      TEXT NOT NULL,
    native_id   TEXT NOT NULL,
    deep_link   TEXT,
    observed_at TEXT NOT NULL,
    PRIMARY KEY (proposal_id, position),
    FOREIGN KEY (source, native_id) REFERENCES source_objects (source, native_id)
);

-- Append-only, hash-chained audit log (ARCHITECTURE_V2 §6):
--   record_hash = SHA-256(seq \n ts \n actor \n event \n proposal_id \n
--                         payload_hash \n prev_hash)
-- with proposal_id rendered as its decimal string, or empty when NULL.
-- No FK on proposal_id: audit records must outlive anything they describe.
CREATE TABLE audit_records (
    seq          INTEGER PRIMARY KEY CHECK (seq >= 0),
    ts           TEXT NOT NULL,
    actor        TEXT NOT NULL,
    event        TEXT NOT NULL,
    proposal_id  INTEGER, -- NULL for genesis
    payload_hash TEXT NOT NULL CHECK (length(payload_hash) = 64),
    prev_hash    TEXT NOT NULL CHECK (length(prev_hash) = 64),
    record_hash  TEXT NOT NULL CHECK (length(record_hash) = 64)
);

-- Fixed genesis record. payload_hash = SHA-256(""), prev_hash = 64 zeros,
-- record_hash = SHA-256 of the canonical string above. A test recomputes this
-- from the constants in act::audit so the literal below cannot silently drift.
INSERT INTO audit_records (seq, ts, actor, event, proposal_id, payload_hash, prev_hash, record_hash)
VALUES (
    0,
    '2026-01-01T00:00:00Z',
    'genesis',
    'genesis',
    NULL,
    'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
    '0000000000000000000000000000000000000000000000000000000000000000',
    '34b6463e473a193a5cf6572a1142c932cc0a32394067c0f9207ef76eb090690a'
);
