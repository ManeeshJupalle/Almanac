-- 0006_jira (Phase 2.1): admit Jira as a source + action target.
--
-- Three CHECK constraints from earlier migrations enumerate allowed values and
-- would reject Jira; SQLite cannot ALTER a CHECK, so each table is rebuilt via
-- the standard drop+recreate (the migration runner disables FK enforcement and
-- re-verifies integrity per migration — see db::migrate). Rebuilds preserve
-- every existing row; only the CHECK sets widen and one nullable column is
-- added. Shipped migrations 0002/0005 are NOT edited.
--
--   source_objects.source        + 'jira'   (JiraAdapter yields SourceObjects)
--   action_proposals.kind        + 'jira_transition', 'jira_comment'
--   action_proposals.(new col)   + target_transition_id / _name
--   proposal_evidence.kind       + 'jira_event' (Tier-Hard, backed by a jira
--                                   source_object via the existing composite FK)
--
-- Jira issue evidence + the jira reply/transition target reuse the SAME
-- (source, native_id) composite FK to source_objects — no new FK shape — so
-- E2 grounding holds for Jira exactly as for Gmail: an unstored issue is
-- unproposable and its evidence unpersistable.

-- 1. source_objects: widen the source CHECK.
CREATE TABLE source_objects_new (
    source      TEXT NOT NULL CHECK (source IN ('gmail', 'gcal', 'slack', 'jira')),
    native_id   TEXT NOT NULL CHECK (length(native_id) > 0),
    deep_link   TEXT NOT NULL CHECK (deep_link LIKE 'https://%'),
    occurred_at TEXT NOT NULL,
    raw_json    TEXT NOT NULL,
    fetched_at  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (source, native_id)
);
INSERT INTO source_objects_new (source, native_id, deep_link, occurred_at, raw_json, fetched_at)
    SELECT source, native_id, deep_link, occurred_at, raw_json, fetched_at FROM source_objects;
DROP TABLE source_objects;
ALTER TABLE source_objects_new RENAME TO source_objects;

-- 2. action_proposals: widen the kind CHECK + add the transition target cols.
CREATE TABLE action_proposals_new (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    kind                  TEXT NOT NULL CHECK (kind IN
                              ('gmail_reply', 'slack_post', 'jira_transition', 'jira_comment')),
    state                 TEXT NOT NULL DEFAULT 'proposed'
                          CHECK (state IN ('proposed', 'approved', 'rejected', 'expired',
                                           'executed', 'execution_failed')),
    target_source         TEXT,
    target_native_id      TEXT,
    target_channel        TEXT,
    -- jira_transition target: the workflow transition id (opaque string) and
    -- its human name (for the dry-run display + re-validation at execute time).
    -- NULL for every other kind.
    target_transition_id  TEXT,
    target_transition_name TEXT,
    draft_subject         TEXT,
    draft_body            TEXT NOT NULL,
    asserts_work_done     INTEGER NOT NULL CHECK (asserts_work_done IN (0, 1)),
    backend_id            TEXT NOT NULL,
    created_at            TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at            TEXT NOT NULL,
    execution_claimed_at  TEXT,
    receipt_json          TEXT,
    -- Gmail message + Jira issue targets both resolve here (source, native_id);
    -- slack_post leaves them NULL (SQLite skips composite FKs with a NULL col).
    FOREIGN KEY (target_source, target_native_id) REFERENCES source_objects (source, native_id)
);
INSERT INTO action_proposals_new
    (id, kind, state, target_source, target_native_id, target_channel,
     target_transition_id, target_transition_name,
     draft_subject, draft_body, asserts_work_done, backend_id,
     created_at, expires_at, execution_claimed_at, receipt_json)
    SELECT id, kind, state, target_source, target_native_id, target_channel,
           NULL, NULL,
           draft_subject, draft_body, asserts_work_done, backend_id,
           created_at, expires_at, execution_claimed_at, receipt_json
    FROM action_proposals;
DROP TABLE action_proposals;
ALTER TABLE action_proposals_new RENAME TO action_proposals;

-- 3. proposal_evidence: widen the kind CHECK to admit Tier-Hard jira issues.
CREATE TABLE proposal_evidence_new (
    proposal_id INTEGER NOT NULL REFERENCES action_proposals (id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    tier        TEXT NOT NULL CHECK (tier = 'hard'),
    kind        TEXT NOT NULL CHECK (kind IN ('message', 'calendar_event', 'jira_event')),
    source      TEXT NOT NULL,
    native_id   TEXT NOT NULL,
    deep_link   TEXT,
    observed_at TEXT NOT NULL,
    PRIMARY KEY (proposal_id, position),
    FOREIGN KEY (source, native_id) REFERENCES source_objects (source, native_id)
);
INSERT INTO proposal_evidence_new
    (proposal_id, position, tier, kind, source, native_id, deep_link, observed_at)
    SELECT proposal_id, position, tier, kind, source, native_id, deep_link, observed_at
    FROM proposal_evidence;
DROP TABLE proposal_evidence;
ALTER TABLE proposal_evidence_new RENAME TO proposal_evidence;
