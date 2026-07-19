-- 0012_item_outcomes (Phase 3.5): close the loop — record when work we acted on
-- actually resolved, with the closing evidence linked.
--
-- One row per resolved plan item: after a proposal for it was EXECUTED, a later
-- source refresh shows the underlying work reached a terminal state (a Jira issue
-- whose statusCategory is "done"). Detection is read-only w.r.t. external
-- services (it reads already-stored source objects); recording the close is an
-- audited local write (L1). The evidence composite FK makes E2 hold at rest —
-- the closing artifact MUST be a real stored source object.
CREATE TABLE item_outcomes (
    item_key           TEXT PRIMARY KEY,
    resolved_at        TEXT NOT NULL,
    evidence_source    TEXT NOT NULL,
    evidence_native_id TEXT NOT NULL,
    detail             TEXT NOT NULL,
    FOREIGN KEY (evidence_source, evidence_native_id)
        REFERENCES source_objects (source, native_id)
);
