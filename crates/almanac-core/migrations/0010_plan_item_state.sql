-- 0010_plan_item_state (Phase 3.2): give plan items memory across days.
--
-- A row exists only for an item the user has ACTED on; its absence means "open"
-- (the default). `status` is one of snoozed / done / dismissed. `snooze_until`
-- (RFC3339) is set only for snoozed items — the prioritizer suppresses the item
-- until then, after which it is treated as open again. Every transition is also
-- appended to the hash-chained audit log (L1), so this table is the current
-- state and the audit log is the history — no silent mutation.
--
-- `item_key` is the plan candidate key (`thread:<ISSUE>` or `<source>:<native>`),
-- stable across re-plans. Local-only; no FK (candidate keys are derived, not
-- rows). Plain table, no rebuild.
CREATE TABLE plan_item_state (
    item_key     TEXT PRIMARY KEY,
    status       TEXT NOT NULL CHECK (status IN ('snoozed', 'done', 'dismissed')),
    snooze_until TEXT,
    updated_at   TEXT NOT NULL
);
