-- 0007_git_correlation (Phase 2.2): admit local git commits as Tier-Hard
-- evidence + carry a correlation rationale on proposals.
--
-- GitWatcher stores each commit as a `source = git` source object, reusing the
-- SAME (source, native_id) composite FK as Gmail/Jira — so commit evidence
-- resolves at rest exactly like every other kind (E2), no new artifact-store
-- shape. Two CHECK constraints must widen (SQLite can't ALTER a CHECK, so the
-- standard drop+recreate; the runner disables FK enforcement and re-verifies
-- integrity per migration — see db::migrate). Rebuilds preserve every row.
-- Shipped migrations 0002/0005/0006 are NOT edited.
--
--   source_objects.source     + 'git'
--   source_objects.deep_link  + 'git-local://%' (local commits have no web URL;
--                               their stable ref is git-local://<repo>/commit/
--                               <sha> — see almanac_core::git. https still holds
--                               for every network source and for git commits in
--                               repos that DO have a github remote.)
--   proposal_evidence.kind    + 'git_commit' (Tier-Hard, backed by a git source
--                               object via the existing composite FK)
--   action_proposals          + correlation_rationale (nullable; a plain ADD
--                               COLUMN — no CHECK change, so no rebuild)

-- 1. source_objects: widen source + deep_link CHECKs.
CREATE TABLE source_objects_new (
    source      TEXT NOT NULL CHECK (source IN ('gmail', 'gcal', 'slack', 'jira', 'git')),
    native_id   TEXT NOT NULL CHECK (length(native_id) > 0),
    deep_link   TEXT NOT NULL CHECK (deep_link LIKE 'https://%' OR deep_link LIKE 'git-local://%'),
    occurred_at TEXT NOT NULL,
    raw_json    TEXT NOT NULL,
    fetched_at  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (source, native_id)
);
INSERT INTO source_objects_new (source, native_id, deep_link, occurred_at, raw_json, fetched_at)
    SELECT source, native_id, deep_link, occurred_at, raw_json, fetched_at FROM source_objects;
DROP TABLE source_objects;
ALTER TABLE source_objects_new RENAME TO source_objects;

-- 2. proposal_evidence: admit Tier-Hard git commits.
CREATE TABLE proposal_evidence_new (
    proposal_id INTEGER NOT NULL REFERENCES action_proposals (id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    tier        TEXT NOT NULL CHECK (tier = 'hard'),
    kind        TEXT NOT NULL CHECK (kind IN ('message', 'calendar_event', 'jira_event', 'git_commit')),
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

-- 3. action_proposals: carry the correlation rationale (no CHECK change → a
-- plain column add, no rebuild).
ALTER TABLE action_proposals ADD COLUMN correlation_rationale TEXT;
