-- 0004_briefing_sections: split a persisted briefing into the day's plan and
-- a forward-looking preview (tomorrow's events), without weakening grounding.
--
-- 'today'   = the sequenced, model-ordered plan for the briefed day.
-- 'preview' = non-model, chronological events for the day AFTER the briefed
--             day (surfaced separately in the UI, never mixed into the plan).
-- The existing composite FK to source_objects still applies to every row, so
-- preview items are grounded exactly like plan items.

ALTER TABLE briefing_items
    ADD COLUMN section TEXT NOT NULL DEFAULT 'today'
        CHECK (section IN ('today', 'preview'));
