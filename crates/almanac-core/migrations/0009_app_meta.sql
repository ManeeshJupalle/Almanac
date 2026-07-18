-- 0009_app_meta (Phase 2.3 review fix): a tiny local key/value store.
--
-- Holds small, non-sensitive app facts that must survive across runs. The first
-- key is `jira_self_account_id`: the accountId resolved from Jira `/myself`
-- during an online step (fetch-jira / correlate / replan). Persisting it lets
-- the OFFLINE UI "Refresh plan" apply the same J15 self-comment exclusion the
-- online CLI path applies — otherwise the app would (mis)count Almanac's own
-- Jira comments as evidence when queueing proposals.
--
-- Local-only, no PII. A plain table (no FK, no rebuild).
CREATE TABLE app_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
