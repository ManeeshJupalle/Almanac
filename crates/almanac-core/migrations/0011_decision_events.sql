-- 0011_decision_events (Phase 3.3): capture what the user DECIDED, with context.
--
-- One append-only row per decision — approve / reject / execute a proposal, or
-- snooze / done / dismiss / reopen a plan item — stamped with the candidate's
-- FACTOR VECTOR at decision time (`factors_json`). That snapshot is the raw
-- signal Phase 3.4 learns from: "what does this user do with stale action-needed
-- asks from sender X". Local only; nothing leaves the device.
--
-- `item_key` is the plan candidate key (`thread:<ISSUE>` / `<source>:<native>`),
-- or `proposal:<id>` when a decided proposal has no correlation identity. The
-- hash-chained audit log already records the authoritative action; this table is
-- a denormalized projection for analysis, so it is not chained. Append-only:
-- rows are only ever inserted.
CREATE TABLE decision_events (
    id           INTEGER PRIMARY KEY,
    occurred_at  TEXT NOT NULL,
    actor        TEXT NOT NULL,
    item_key     TEXT NOT NULL,
    decision     TEXT NOT NULL,
    factors_json TEXT NOT NULL
);

CREATE INDEX idx_decision_events_item ON decision_events (item_key);
