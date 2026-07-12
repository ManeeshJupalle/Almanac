-- 0003_briefings: persisted briefings (Phase 5).
--
-- Grounding at rest, again: briefing_items carries a composite FOREIGN KEY
-- to source_objects, so an ungrounded planned item cannot be persisted, and
-- reads INNER-JOIN source_objects, so an unresolvable item cannot even be
-- loaded for display.

CREATE TABLE briefings (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    briefing_date TEXT NOT NULL,  -- local day briefed, YYYY-MM-DD
    backend_id    TEXT NOT NULL,
    rationale     TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE briefing_items (
    briefing_id INTEGER NOT NULL REFERENCES briefings (id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    source      TEXT NOT NULL,
    native_id   TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('commitment', 'event', 'action_needed', 'noise')),
    summary     TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    PRIMARY KEY (briefing_id, position),
    FOREIGN KEY (source, native_id) REFERENCES source_objects (source, native_id)
);
