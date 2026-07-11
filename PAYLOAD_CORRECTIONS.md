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
