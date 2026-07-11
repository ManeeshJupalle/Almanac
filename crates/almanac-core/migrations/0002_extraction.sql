-- 0002_extraction: source objects + extracted items (Phase 3).
--
-- Grounding invariant at rest: extracted_items carries a composite FOREIGN
-- KEY to source_objects, so an item that does not resolve to a stored source
-- object cannot be persisted (foreign_keys pragma is always ON — see db::open).
-- raw_json is local-only content; it never leaves this machine.

CREATE TABLE source_objects (
    source      TEXT NOT NULL CHECK (source IN ('gmail', 'gcal', 'slack')),
    native_id   TEXT NOT NULL CHECK (length(native_id) > 0),
    deep_link   TEXT NOT NULL CHECK (deep_link LIKE 'https://%'),
    occurred_at TEXT NOT NULL, -- RFC3339 UTC
    raw_json    TEXT NOT NULL, -- raw payload, stays local
    fetched_at  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (source, native_id)
);

CREATE TABLE extracted_items (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    source       TEXT NOT NULL,
    native_id    TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('commitment', 'event', 'action_needed', 'noise')),
    summary      TEXT NOT NULL,
    signals_json TEXT NOT NULL,
    extracted_at TEXT NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (source, native_id) REFERENCES source_objects (source, native_id)
);

CREATE INDEX idx_extracted_items_kind ON extracted_items (kind);
CREATE INDEX idx_extracted_items_source ON extracted_items (source, native_id);
