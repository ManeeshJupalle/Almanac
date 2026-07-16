# Payload-vs-Doc Corrections (Phase 1)

Every place the **real captured payloads** (2026-07-11, see `/fixtures`) differ
from what the official API docs imply. Typed models in Phase 2 must be written
against these fixtures, not against the docs.

Sources compared: Gmail API reference (`users.messages.list` / `users.messages.get`),
Calendar API reference (`events.list` / Events resource), Slack docs
(`conversations.list` / `conversations.history`), all fetched 2026-07-11.

Live run: Gmail — 25 message refs + 1 full message; Calendar — 1 event
(2026-07-08) + genuine empty day (2026-07-11); Slack — 3 channels + 1 message.

---

## Gmail

Fixtures: `fixtures/gmail/messages_list.json`, `fixtures/gmail/message_get_full.json`
(GET used `format=full`).

| # | Field / area | Docs imply | Real payload | Consequence for Phase 2 |
|---|---|---|---|---|
| G1 | `messages[]` in list | Partial `Message` (id + threadId only) | Confirmed: exactly `{id, threadId}` | List models must not expect any other field |
| G2 | `nextPageToken` | "A token"; no format documented | 20-digit decimal string **with a leading zero**: `"08914656770475527830"` | Opaque string only. Parsing as int silently corrupts it (leading zero lost) |
| G3 | `resultSizeEstimate` | "Estimated total number of results" | `201` while the page holds 25 refs — mailbox-wide estimate, unrelated to page size, not exact | Never use as a count of anything |
| G4 | `internalDate` | string (int64 format), epoch ms | Confirmed a **JSON string** of epoch **milliseconds**: `"1783746307000"` | Parse string→i64→ms. Treating it as seconds gives dates in year 58497 |
| G5 | `historyId` | string | Confirmed string of digits (`"12075797"`) | Keep as string; it can exceed common int sizes over time |
| G6 | `payload.body` on multipart root | `MessagePartBody` has `attachmentId`, `size`, `data` | Root body is `{"size": 0}` — `data` and `attachmentId` **absent**, not null/empty | All `MessagePartBody` fields except `size` must be optional |
| G7 | `headers[]` | "List of headers" | Duplicate names are real: 3× `Received`, 2× `DKIM-Signature` in one message | Headers are a multiset — a `HashMap<name, value>` loses data |
| G8 | Header name casing | Canonical names (`MIME-Version`, `Message-ID`) | Wire casing preserved: `Mime-Version`, `Message-Id`, `X-Mailgun-*` | Header lookups must be case-insensitive |
| G9 | `body.data` encoding | "base64-encoded" (alphabet unspecified for parts) | **base64url** alphabet (`-` and `_` present; `+` and `/` absent) | Decode with URL_SAFE, not standard base64 |
| G10 | `labelIds` | Label IDs | System category labels mixed in: `CATEGORY_UPDATES` alongside `INBOX`, `UNREAD` | Don't assume user labels only; `CATEGORY_*` is noise-classification signal |
| G11 | `partId` / `filename` | Documented as identifiers | Present-but-**empty strings** on root/non-attachment parts (`"partId": ""`, `"filename": ""`) | Empty string ≠ absent; both cases occur |
| G12 | `raw` field | Only with `format=RAW` | Confirmed absent with `format=full` | — |

## Google Calendar

Fixtures: `fixtures/gcal/events_list_day.json` (single day with 1 event),
`fixtures/gcal/events_list_day_empty.json` (genuine zero-event day).

| # | Field / area | Docs imply | Real payload | Consequence for Phase 2 |
|---|---|---|---|---|
| C1 | Top-level `summary` | "Title of the calendar" | For the primary calendar the "title" is the **account e-mail address** | PII in a field named like a label; must be treated as sensitive (redacted in fixture) |
| C2 | Top-level `description` | "Description of the calendar" | Present but **empty string** — not omitted | Present-but-empty is a real state |
| C3 | `etag` (collection + event) | "ETag" | Value contains **literal escaped quotes**: `"\"3566224136349662\""` | Compare byte-for-byte; don't strip/add quotes |
| C4 | `nextPageToken` / `nextSyncToken` | Mutually exclusive | Confirmed: final page has only `nextSyncToken`; empty-day response also carries `nextSyncToken` | Sync-token flow works even for empty windows |
| C5 | `start`/`end` | `date` (all-day) vs `dateTime`, `timeZone` "required for recurring" | Timed event: `dateTime` **and** `timeZone` both present, `date` absent. Event tz (`America/New_York`) **differs** from calendar tz (`America/Chicago`) in the same payload | Models need the date/dateTime union; never inherit event tz from the calendar |
| C6 | Timestamp formats | "RFC3339" everywhere | Two flavors in one payload: `created`/`updated` = `2026-07-03T20:54:28.174Z` (UTC+millis), event times = `2026-07-08T13:00:00-05:00` (offset, no millis) | Parser must accept both RFC3339 shapes |
| C7 | `creator` / `organizer` | Same subfield set (`id`, `email`, `displayName`, `self`) | Same event: `creator` = `{email}` only; `organizer` = `{email, displayName}` | Every subfield optional, per-object |
| C8 | `attendees[]` | 11 documented subfields | Only `{email, self, responseStatus}` present | All other subfields optional |
| C9 | `conferenceData.entryPoints[]` | Uniform entry-point objects | **Heterogeneous array**: video=`{entryPointType,uri,label}`, more=`{entryPointType,uri,pin}`, phone=`{regionCode,entryPointType,uri,label,pin}` | One struct with everything optional; `regionCode` appears only on phone entries |
| C10 | `extendedProperties.shared` | App key-value store | Contains **third-party vendor keys** (`ashbyInterviewScheduleId`, `isAshbyInvite` — written by an ATS) | Treat as open `Map<String,String>`; other apps pollute it |
| C11 | `reminders` | `useDefault` + `overrides[]` | `{"useDefault": true}` with `overrides` absent | `overrides` optional |
| C12 | Event optional fields | Long field list | `colorId`, `transparency`, `visibility`, `recurringEventId`, `sequence`>0 … simply absent | Absent, not null — every field beyond the core set is `Option` |

## Slack

Fixtures: `fixtures/slack/conversations_list.json`, `fixtures/slack/conversations_history.json`.
Reminder: Slack errors are **HTTP 200 + `"ok": false`** — status codes are useless.

| # | Field / area | Docs imply | Real payload | Consequence for Phase 2 |
|---|---|---|---|---|
| S1 | `response_metadata.next_cursor` (list) | "Responses will include … a next_cursor value"; empty-case undocumented | Last page = **empty string** `""`, not absent, not null | Pagination termination check is `cursor == ""` |
| S2 | `response_metadata` (history) | Shown in every documented success example | **Entirely absent** when `has_more: false` | Termination detection differs per method: empty-string cursor (list) vs missing object (history) |
| S3 | `created` vs `updated` (channel) | Both timestamps | `created` = epoch **seconds** (`1783745960`), `updated` = epoch **milliseconds** (`1783745972551`) — same object | Two different time units two lines apart; unit is per-field, not per-API |
| S4 | Channel object fields | Documented example set | Undocumented extras: `context_team_id`, `properties` (with `use_case`, `tabs`, **and `tabz`** — an internal duplicate key), `parent_conversation: null`, `pending_shared`, `pending_connected_team_ids`, `unlinked`, `shared_team_ids`, `is_pending_ext_shared` | Models must ignore unknown fields; do not fail on them |
| S5 | `topic`/`purpose` when never set | — | Zero values, not null: `creator: ""`, `last_set: 0` | Empty string / 0 are the "unset" sentinels |
| S6 | `parent_conversation` | Not in documented example | Present and explicitly `null` | The one null-valued field — needs nullable handling |
| S7 | Message `ts` | "A sortable Unix timestamp value" | String `"1783745960.543929"` — seconds + 6-digit suffix; doubles as the message's unique ID per channel | MUST stay a string. Parsing to float destroys uniqueness/precision; it's an ID that happens to sort |
| S8 | `messages[]` content | Example shows plain user messages | First message is `subtype: "channel_join"` — system events are mixed into history | Filter/classify by `subtype`; absent `subtype` = ordinary message |
| S9 | `channel_actions_ts` / `channel_actions_count` | Not documented | Present: `null` and `0` | More undocumented top-level fields; ignore-unknown again |
| S10 | Nested `ts` in metadata | — | `properties.tabs[].data.shared_ts` = ts-string deep inside channel metadata | ts-strings appear in arbitrary nesting, not just messages |

---

## Redaction notes (structure preserved, values blanked)

- Fixtures under `/fixtures` are the real responses with PII **values** replaced
  (`[redacted]`, `redacted@example.com`, base64url of `[redacted]` for Gmail
  body data). No keys added/removed/renamed, no array lengths changed, no type
  changes. IDs, timestamps, cursors, etags kept verbatim — their formats are
  deliverables (G2, G4, S7, C3…).
- Two value-format notes: raw `Delivered-To` was a bare address but the
  placeholder uses name-addr form (`Redacted Sender <redacted@example.com>`);
  raw `Message-Id` host part was a real domain, placeholder is `example.com`.
  `payload.body.size` values are the original (pre-redaction) byte counts.
- Unredacted originals exist only in gitignored `.fixtures-raw/` on this
  machine (raw-content-local invariant).

## Scope / permission notes

- Google: one Desktop OAuth client covered both APIs; consent granted
  `gmail.readonly` + `calendar.readonly` on the first attempt. No surprises.
- Slack: first authorization attempt failed because the redirect URL had not
  been saved on the app config (`http://localhost:8080/callback`); after
  registering it, user-token flow granted `channels:read,channels:history`
  (note: response reports them in reversed order — treat scope list as a set).

---

# Write-API corrections (Phase 2.0)

Every place the **real captured write-path payloads** (2026-07-12, see
`/fixtures/gmail_send` and `/fixtures/slack_post`) differ from what the docs
imply. Captured by sending ONE real email to self and posting ONE real message
to a private test channel (payload-first: these fixtures existed before any
send/post response was interpreted). New scopes exercised: Google `gmail.send`
(re-consent), Slack `chat:write` user scope (app config + reinstall).

## Gmail `users.messages.send`

Fixtures: `fixtures/gmail_send/send_response.json` (the send response),
`fixtures/gmail_send/sent_message_get.json` (a follow-up `messages.get?format=full`
of the sent id, showing what Gmail fills in server-side).

| # | Field / area | Docs imply | Real payload | Consequence for the executor |
|---|---|---|---|---|
| GS1 | send response body | Returns a `Message` resource | **Partial** `Message`: only `{id, threadId, labelIds}` — no `payload`, `snippet`, `internalDate`, `sizeEstimate` | Never read headers/body from the send response; GET the id separately if you need them (as the capture does) |
| GS2 | `threadId` vs `id` | Independent identifiers | On a NEW thread they are **equal** (`19f58d74d1bca1f2` == `19f58d74d1bca1f2`) | id==threadId only when starting a thread; a real reply threads to the *original's* threadId, so they differ — don't infer "reply" from equality |
| GS3 | `labelIds` on a sent message | `SENT` | Message sent to **yourself** carries `["UNREAD","SENT","INBOX"]` at once | A self-addressed send is simultaneously SENT and delivered to INBOX+UNREAD; don't assume a lone `SENT` |
| GS4 | `Message-Id` header | Caller may set it | **Server-assigned**: absent from what we sent, present on the GET (`<…@…>`) | Correct to OMIT `Message-ID` in the outbound MIME and let Gmail mint it (the executor does; the GET proves it) |
| GS5 | `Date` header | Caller may set it | **Server-assigned** (`Sun, 12 Jul 2026 17:18:52 -0700`) — not in our MIME | Omitting `Date` is correct and required for a deterministic dry-run (the byte-identity test depends on it) |
| GS6 | `From` header | Caller may set it | **Server-assigned** to the account address | Omitting `From` is correct; Gmail fills the authenticated identity |
| GS7 | round-trip of a single-part reply | multipart shapes (per G6) | Our single `text/plain` MIME returns as a **flat** `payload` (no `parts[]`; `body.data` present directly) | Confirms the flat text/plain reply is well-formed; `body.data` is base64url, no padding (`W3JlZGFjdGVkXQ`, decodes to `[redacted]`) — consistent with G9 |
| GS8 | per-message size field | (list uses `resultSizeEstimate`, G3) | GET carries `sizeEstimate` (camelCase, `734`) — a distinct per-message field | Don't confuse with the mailbox-wide `resultSizeEstimate` (G3); different key, different meaning |
| GS9 | `internalDate` on the sent message | — | JSON **string** of epoch **ms** (`"1783901932000"`) | Same shape as G4; parse string→i64→ms |

## Slack `chat.postMessage`

Fixture: `fixtures/slack_post/post_response.json`.
Reminder: errors are still **HTTP 200 + `"ok": false`** (as elsewhere in Slack).

| # | Field / area | Docs imply | Real payload | Consequence |
|---|---|---|---|---|
| SP1 | echoed `message` identity | A user message (we posted with a **user** token, `chat:write`) | Echoed message carries `bot_id`, `app_id`, and a full `bot_profile` — it is **app-attributed**, not a bare user message | Our own posts are identifiable as app-posted; a re-ingest path must not treat them as human messages (relevant to future dedup / not replying to ourselves) |
| SP2 | `ts` location | one value | Present **both** top-level (`"1783901949.342459"`) and nested in `message.ts`, identical | Use the top-level `ts` as the post's id / deep-link anchor (the per-channel unique id, S7 pattern) |
| SP3 | plain-text post round-trip | `text` echoes back | Slack **auto-generates** a `blocks[]` (`rich_text` → `rich_text_section` → `text`) from our plain `text` | The echoed message has richer structure than what we sent; `message.text` is not the only representation |
| SP4 | server-assigned `block_id` | — | The auto-generated block carries a `block_id` (`"Fn1Y9"`) we never sent | Echoed blocks get server-assigned ids; don't expect to control or match them |
| SP5 | `bot_profile.updated` unit | (channel `updated` = ms, per S3) | Here `updated` = epoch **seconds** (`1783747526`, 10 digits) | Extends S3: the SAME field name `updated` is ms on a channel but seconds on a bot_profile — the unit is per-object, verify each; never guess |
| SP6 | response envelope | `response_metadata`/warnings shown in examples | Simple `ok:true` post has **no** `response_metadata` and no `warning` | Absent metadata is the norm on a clean post (mirrors S2: metadata is not always present) |

## Redaction notes (write-path fixtures)

- Same structure-preserving rules as the read path (redactor routes by source
  prefix, so `gmail_send`/`slack_post` reuse the `gmail`/`slack` rules). The
  Gmail body `data` still decodes to base64url of `[redacted]`; the Slack
  `message.text` and `bot_profile.name` are blanked; ids, `ts`, `app_id`,
  `team`, `block_id`, and timestamps are kept verbatim (they are the deliverable
  formats above). `bot_profile.icons` point at `a.slack-edge.com` (not
  `files.slack.com`), so they are left as-is — public asset URLs, no PII.

## Scope / permission notes (write path)

- Google: `gmail.send` required fresh testing-mode consent (re-run of
  `google-auth`); no other surprises — the same Desktop client covers it.
- Slack: `chat:write` had to be added under **User Token Scopes**, the app
  **reinstalled**, then `slack-auth` re-run. The post is attributed to the app
  identity (`app_id`/`bot_id`) despite using the user token — see SP1.

---

# Jira (Atlassian Cloud) corrections (Phase 2.1)

Every place the **real captured Jira payloads** (2026-07-14, site
`almanacnemo.atlassian.net`, project `ALM`, see `/fixtures/jira`,
`/fixtures/jira_transition`, `/fixtures/jira_comment`) differ from what the docs
imply. Auth is HTTP Basic over `base64(email:api_token)`. These fixtures existed
before any Jira struct was modeled (payload-first). Captured: enhanced + legacy
issue search, issue GET, transitions for two statuses, a real transition POST
(+follow-up GET), and a real ADF comment POST (+plain-text rejection).

| # | Field / area | Docs imply | Real payload | Consequence for Phase 2.1 |
|---|---|---|---|---|
| J1 | Issue search endpoint | Many guides still show `GET /rest/api/3/search` (`startAt`) | Legacy `/rest/api/3/search` is **HTTP 410 Gone**: `errorMessages:["The requested API has been removed. Please migrate to the /rest/api/3/search/jql API…"]`. Only `/rest/api/3/search/jql` serves results | Model the read path against `/search/jql` ONLY; the legacy endpoint is dead on this site |
| J2 | Search pagination | `startAt` / `maxResults` / `total` offset paging | `/search/jql` returns `{"issues":[…], "isLast": true}` — **no** `startAt`, `total`, or echoed `maxResults`. Continuation is a `nextPageToken` (opaque) + `isLast` | Terminate on `isLast == true`; carry `nextPageToken` verbatim (opaque, never parsed) — a different paging model from every v1 source |
| J3 | Comment body format | Some docs show plain-string `body` | v3 **requires ADF**: a plain-string body is **HTTP 400** `errors:{"comment":"Comment body is not valid!"}`. ADF (`{type:"doc",version:1,content:[{type:"paragraph",content:[{type:"text",text:…}]}]}`) succeeds with 201 | The comment executor MUST emit ADF; the drafting/template layer wraps text into the ADF tree |
| J4 | Error envelope shape | One error shape | **Two** shapes, per failure class: 410 uses `errorMessages:[…]` (array, `errors:{}` empty); 400 validation uses `errors:{<field>:<msg>}` (object) with `errorMessages:[]` empty | Read BOTH: collect `errorMessages[]` and every value of `errors{}` — neither alone is complete |
| J5 | Transition id | id | **String** (`"11"`, `"21"`, `"31"`, `"41"`), and it is the *workflow transition* id — **distinct from** the target status id (`to.id` = `"10005"`). Never conflate | Keep transition ids as opaque strings; never derive them from status ids or hardcode them |
| J6 | Transition availability | "available transitions depend on current status" | On THIS project every transition is `isGlobal: true`, so the offered set was identical for a `To Do` and an `In Progress` issue — but the ids are still workflow-specific and unknowable a priori | Do NOT assume the set is state-independent in general (other workflows gate transitions); always GET `/transitions` for the specific issue at propose time — the Phase-2.1 re-validate-at-execute hazard |
| J7 | Transition POST response | a body | **HTTP 204 No Content, empty body** on success | Success = 2xx status, not a parsed body; confirm the effect via a follow-up GET (the `issue_after_transition` fixture shows the changed status) |
| J8 | Transition object fields | `id`, `name`, `to` | Also `hasScreen`, `isGlobal`, `isInitial`, `isAvailable`, `isConditional`, `isLooped` | `hasScreen: true` means the transition needs a field screen (extra payload) — Phase 2.1 only proposes `hasScreen: false` transitions; document the limit |
| J9 | Timestamp format | "ISO 8601" | `2026-07-14T22:26:03.672-0500` — millis **and** a timezone offset **without a colon** (`-0500`, not `-05:00`). This is NOT valid RFC 3339 | `chrono::DateTime::parse_from_rfc3339` REJECTS it; parse with an explicit format (`%Y-%m-%dT%H:%M:%S%.3f%z`, which accepts `-0500`) |
| J10 | Issue id vs key | one identifier | `id` is a numeric **string** (`"10005"`); `key` is the human ref (`"ALM-1"`); `self` links use the numeric id (`/issue/10005`) while the browse/deep link uses the key (`/browse/ALM-1`) | Use `key` as the SourceObject `native_id` and for the deep link (`{base}/browse/{key}`); keep `id` too, but they are not interchangeable |
| J11 | Nullable fields | field list | `description`, `assignee`, `resolution`, `duedate`, `timespent`, `resolutiondate`, … are explicitly `null` (not absent) on a fresh issue | Every non-core field is `Option`; `null` is a real state (as with the Google/Slack "present-but-empty" quirks) |
| J12 | Custom fields | — | `customfield_10015`, `customfield_10019` (`"0|i0001v:"` — a Lexorank), `customfield_10021`, `customfield_10001` appear inline in `fields` | Ignore unknown `customfield_*` keys; never fail on them (open-map discipline, like Slack's undocumented extras) |
| J13 | Mixed id types | ids | `status.id`/`issuetype.id`/`priority.id` are **strings**; `statusCategory.id` is a **number** (`2`, `3`, `4`) in the SAME payload | Don't assume all ids share a type; `statusCategory.id` is numeric |
| J14 | Comment sub-resource paging | (search moved off startAt, J2) | The `fields.comment` container STILL uses old-style `{comments:[], startAt, maxResults, total}` | Pagination model is per-resource: `/search/jql` is token-based, embedded comments are offset-based — don't unify them |
| J15 | **Self-authored attribution** (SP1 analog) | — | A comment we POST comes back (201) with `author.accountId` = **our own** authenticated account (the one `GET /myself` returns). Same for a transition recorded in history | To exclude Almanac's own comments/transitions on re-read (a later-phase dedup, as with Slack SP1), compare `author.accountId` to the stored `/myself` accountId — capture makes this identity concrete |

## Redaction notes (Jira fixtures)

- `redact.rs` gained a `jira` rule set + a residual pass **before** any fixture
  was written. Blanked: `accountId` → `redacted-account-id`, `emailAddress` →
  `redacted@example.com`, `displayName` → `Redacted User`, avatar URLs (the
  `16x16/24x24/32x32/48x48` leaves) → a placeholder, ADF leaf `text` →
  `[redacted]`, the site host in every URL → `redacted.atlassian.net` (path
  kept — the `self`/REST-route shapes are the deliverable), and `accountId=`
  query-param values in `self` user links. **Kept verbatim** (structure /
  deliverable, not PII): issue `key`/`id`, status/transition/issue-type/priority
  `name`s ("To Do", "In Progress", …), `statusCategory`, transition ids and
  flags, timestamps (format is J9), `customfield_*` keys. Verified post-capture:
  zero occurrences of the real site host, account id, display name, or external
  avatar hosts in `/fixtures/jira*`; the only email present is the placeholder.

## Scope / permission notes (Jira)

- Auth is a personal **API token** (Basic auth), not OAuth — no browser flow.
  Validated once via `GET /rest/api/3/myself`, then stored DPAPI-encrypted at
  `JIRA_TOKEN_PATH` (`tokens/`, gitignored). **A2 isolation:** `JiraAuth` reads
  only `JIRA_*` config + its own token file; it cannot reach Gmail/Slack
  credentials, nor they Jira's.
- The token's account is a **project admin** on the dev site; a token with only
  browse/transition/comment permission on `ALM` would suffice for the app.
  Write permission is exercised only by the executors (A2).

# Git log (local commits) corrections (Phase 2.2)

Payload = `git log` byte output, not an HTTP API. GitWatcher is local + network-
free (A2). The exact command (single source of truth: `almanac_core::git::
LOG_FORMAT` + `log_args`, also used by `fixture-capture git`):

```
git -C <repo> log <ref> -z --no-color \
    --pretty=format:'%H<US>%h<US>%an<US>%ae<US>%aI<US>%P<US>%B'
```
`<US>` = `%x1f` (ASCII Unit Separator, 0x1f). Fixtures: `fixtures/git_log/`.

| # | Area | Doc/assumption | Reality (observed in captured bytes) | Handling |
|---|------|----------------|--------------------------------------|----------|
| G1 | **Delimiter collision** | "pick a separator unlikely to appear" | A commit message can contain ANY printable char, so no printable delimiter is safe | Records: NUL via `-z` — git FORBIDS NUL inside a commit object, so record boundaries are collision-**proof**. Fields: US (0x1f) with the one free-text field (`%B`) LAST + parsed via `splitn`, so any delimiter/newline inside a message is absorbed, never shifting a boundary. Residual risk (documented): a raw 0x1f in an author name/email or ISO date — not producible by git identities/dates |
| G2 | **CRLF in messages** (Windows) | "Windows commits may carry CRLF; parser must strip `\r`" | Authoring a message with CRLF (`git commit -F` of a CRLF file) → `git log` output has the message with **LF only**; git normalizes message line endings | Parser sees LF regardless; we still strip a stray trailing `\r` on the subject line defensively. The CRLF concern is real only for the fixture FILE we write/read — we write/read exact bytes (no newline translation) |
| G3 | **Non-ASCII author/message** | assume ASCII | `%an`/`%B` are emitted as raw UTF-8 (`Renée Müller`, `Café ☕ … smörgåsbord`) | Decode the whole blob as UTF-8; identities/messages survive round-trip. (The non-ASCII scratch author is under `example.test`, so redaction keeps it — it IS the test payload) |
| G4 | **Timestamp format** | (contrast with Jira J9) | `%aI` = strict ISO-8601 WITH a colon in the offset: `2026-07-11T03:00:00-05:00` | `DateTime::parse_from_rfc3339` accepts it directly — unlike Jira's colon-less `-0500` (J9), which needs an explicit format. Two sources, two timestamp grammars |
| G5 | **Merge detection** | "a flag says it's a merge" | No flag; `%P` is a space-separated parent list — **2+ parents ⇒ merge** (the merge commit shows two 40-hex parents; the root shows an empty `%P`) | `is_merge = parents.len() >= 2`; empty `%P` = root (not a merge). No `--first-parent` (we want every commit) |
| G6 | **Evidence ref for a LOCAL commit** | "hard evidence links out over https" | A commit in a repo with no remote (the scratch repo) has NO web URL — only (repo, sha) | Ref form: `git-local://<repo>/commit/<full-sha>`; the authoritative id is the 40-char SHA (also the source-object `native_id`), verified offline with `git -C <repo> show <sha>`. When the repo HAS a github.com https remote (the Almanac repo), we derive the real `https://github.com/<owner>/<repo>/commit/<sha>` instead — conservatively (github https only; any other host/ssh falls back to the local ref, so we never emit a *wrong* clickable link). `ActionProposal::new` accepts either form for `git_commit` hard evidence; every network source still requires https |
| G7 | **Branch of a commit** | "the log line says its branch" | `%D` (decorations) only names refs at a commit's TIP, not for its ancestors; a commit's branch membership is not in the per-commit line | GitWatcher runs `git log <branch>` per local branch (`for-each-ref refs/heads`) and tags each commit with the branch it was listed under, unioning across branches and deduping by sha. "Key in branch name" correlation uses that tag |

## Redaction notes (git fixtures)

- `redact.rs` gained `redact_git` **before** any git fixture was written. It
  scrubs REAL author identities (name → `Redacted Author`, email →
  `redacted@example.com`, incl. `Co-authored-by:`/trailer addresses) but KEEPS
  reserved-example identities verbatim (RFC 2606/6761: the `example.{com,net,
  org}` domains and the `.example`/`.test`/`.invalid`/`.localhost` TLDs), because
  the scratch fixture's invented authors — including the non-ASCII one (G3) — ARE
  the payload the parser is tested against. Structure-preserving: only identity
  *values* change; delimiters, field counts, and record counts are untouched.
- Verified post-capture: `fixtures/git_log/almanac_all.txt` (a real capture of
  this repo) contains ZERO occurrences of the real author name, real email,
  `@gmail.com`, or `anthropic.com`; the only address present is the placeholder.
  `scratch_all.txt` keeps `Renée Müller` / `Café ☕` / `example.test` intact.

## Correlation notes (Phase 2.2)

- **Binding is deterministic only.** A commit is evidence for an issue iff the
  issue key appears (word-boundary safe: `ALM-1` matches; `PSALM-1`, `ALM-10` do
  NOT) in the commit subject, body, or a branch name, OR a non-Almanac Jira
  comment on the issue names the commit sha (explicit link). Below signal ⇒ no
  thread. The MiniLM embedding "tiebreaker" sketched in BUILD_PHASES_V2 is
  **deliberately deferred**: a semantic fuzzy-match is the one thing that could
  fabricate a link, which the phase's hard constraint forbids, and the ALM
  scenario is fully deterministic. Recorded as an explicit deferral, not a
  silent omission.
- **What counts as an ask.** An ask is a Gmail/Slack source object whose text
  (email subject + snippet, or Slack text) **contains an issue key** — matched by
  the same word-boundary `issue_keys`. It is NOT gated on the v1 noise/action
  classification: the general-purpose topic classifier never knew about issue
  keys and falls back to `noise` on low confidence (observed live — it labeled a
  self-sent email titled "ALM-3" as noise, hiding it from correlation). The exact
  key-match is the ask signal, and stronger than the topic classifier; a message
  with no key is not an ask (precision preserved). Binding still requires the key
  to resolve to a real fetched issue (E2), a commit (E1), and user approval.
- **Confidence / eligibility.** *Full* = ask + item + ≥1 commit all present →
  eligible to propose. Anything less = *possible (needs confirmation)* → surfaced
  only, never auto-proposed. The proposal carries a templated `correlation_
  rationale` (no synthesized prose) shown in the approval UI.
- **J15 applied.** The engine takes the `/myself` accountId and drops Almanac's
  own comments from the explicit-link signal, so it never corroborates its own
  prior output. Offline-testable: the accountId is a parameter, not a live call.
