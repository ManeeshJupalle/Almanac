# Almanac

**A local-first daily-briefing and action desktop agent.** Almanac connects
to your Gmail, Google Calendar, and Slack, classifies the day's items
on-device, and opens to a synthesized "here is your day" briefing — what
needs action, what's noise, in what order, and *why* — with **every item
click-through-grounded to the exact message or event it came from**.

On top of the briefing core, Almanac also **acts — carefully**. It reads Jira
and your local git commits, correlates asks ↔ issues ↔ commits into work
threads, drafts evidence-backed replies and Jira updates, and executes them
**only after your explicit approval**, with every step recorded on a
hash-chained audit log. It plans your day deterministically, tracks what you
snooze, finish, or dismiss, learns bounded ranking weights from those
decisions, and closes loops automatically when the evidence says the work is
done. See [the action layer](#the-action-layer-propose--approve--execute)
below.

![Almanac demo](docs/demo.gif)

## Why this exists

AI inbox tools are cloud-first (your mail gets uploaded) and produce ungrounded
summaries (no click-back to source; hallucinated tasks). Almanac's bet is the
opposite corner: **local-first + grounded**.

- **Local-first**: raw message and event content is processed entirely
  on-device and is never transmitted. Only you see your data.
- **Grounded**: every briefed item carries a provenance handle to the exact
  source object. Nothing is surfaced that can't be traced back — an ungrounded
  item is a *hard pipeline failure*, not a display glitch.

That privacy sentence is enforced, not aspirational — see below.

## How the privacy claim is enforced

- `RawContent` (and `SourceObject`) **do not implement `serde::Serialize`** —
  a compile-time guarantee, pinned by `static_assertions` in
  [`tests/raw_content_local.rs`](crates/almanac-core/tests/raw_content_local.rs).
  Outbound request *bodies* are built through serde, so raw content cannot be
  placed into one. (One honest scope note: URL query strings are built as
  plain strings, a channel the type system does not police — every adapter
  call site only ever sends window timestamps, API-issued cursors/ids, and
  channel ids, verified by enumeration, but the guarantee is narrower than
  "no outbound call can carry it".) `RawContent`/`SourceObject` also carry a
  manual redacted `Debug`, so a stray `{:?}` can't leak bodies to logs. The UI
  receives a string-field DTO (summary, kind, provenance) over Tauri IPC —
  never raw payloads.
- **Extraction runs on-device**: all-MiniLM-L6-v2 (ONNX, via pure-Rust
  tract-onnx). Verified with the network physically disabled: the pipeline's
  own probe reported `network: UNREACHABLE — offline run confirmed`, then
  classified and persisted every fixture item.
- **Synthesis runs on-device**: Qwen2.5-0.5B-Instruct (Q4_K_M GGUF, via
  pure-Rust candle). Same network-disabled verification: a full
  fixtures → extraction → briefing run completed offline.
- OAuth tokens are stored **DPAPI-encrypted** (Windows, bound to your OS
  user), never plaintext, at gitignored paths. Non-Windows platforms fail
  loudly rather than fall back to plaintext.

What *does* leave the machine: OAuth requests to Google/Slack and the API
calls that fetch your data (read-only scopes). Nothing else.

**At-rest posture (honest):** OAuth *tokens* are DPAPI-encrypted, but the
fetched message/event content is stored **unencrypted** in the local SQLite DB
(`%APPDATA%\Almanac\almanac.db`). The privacy claim is about *transmission*
(nothing is sent off-device), not at-rest encryption — the DB is as protected
as your OS user account. Encrypting `raw_json` (via the existing DPAPI helpers
or SQLCipher) is a known, deliberate follow-up, not a shipped feature.

## The grounding story

Grounding is enforced at four layers — memory, wire, disk, render:

1. **Memory**: `ExtractedItem` has private fields; its only constructor
   rejects empty native ids and non-https deep links. The synthesis model
   never sees or emits IDs — it orders a numbered digest, and planned items
   are built in code from the real items.
2. **Wire**: the model's reply must be an exact permutation of the digest
   indexes (1..=n, each exactly once). Anything else is rejected and retried
   once, then the pipeline fails.
3. **Disk**: SQLite composite foreign keys (`extracted_items` and
   `briefing_items` → `source_objects`) make an unresolvable item
   unpersistable.
4. **Render**: the UI reads briefings via an INNER JOIN on `source_objects` —
   an item that doesn't resolve cannot even be loaded for display.

**This fired for real.** On the first network-disabled end-to-end run, the
local model echoed digest text into its ORDER line instead of bare numbers.
The parser rejected it, the retry also failed, and the pipeline **refused to
produce a briefing** — a hard failure instead of silent garbage. The prompt
and parser were then hardened (the permutation check is unchanged), and the
incident is kept here as evidence the validator is not advisory.

Deep links are real: Gmail (`mail.google.com/mail/#all/<id>`), Calendar (the
event's own `htmlLink`), Slack (workspace archives permalink — verified
**byte-identical** to Slack's canonical `chat.getPermalink` output).

## The action layer (propose → approve → execute)

Almanac can *do* things — reply in Gmail, post in Slack, comment on or
transition Jira issues. The same posture as grounding applies: the safety
properties are enforced, not aspirational, and each has a named invariant
(spelled out in [`ARCHITECTURE_V2.md`](ARCHITECTURE_V2.md)):

- **A1 — propose-then-approve, always.** The only code path to an `approved`
  proposal is an explicit user approval, audited in the same transaction.
  There is no auto-approve flag, config, or route.
- **A2 — one write chokepoint.** A single executors module is the only code
  in the workspace that touches write endpoints (`messages/send`,
  `chat.postMessage`, Jira transition/comment); it independently re-validates
  `state == approved` from the database at execution time, so a UI bug cannot
  reach the network. Credentials are scope-isolated: Jira credentials never
  reach Gmail/Slack code and vice versa, and the GitWatcher holds no network
  credential at all.
- **What you approve is what is sent.** `dry_run()` renders the exact final
  payload — full MIME for Gmail, the exact JSON body for Slack/Jira — and
  `execute()` sends those bytes verbatim (byte-identity by construction,
  pinned by tests, and verified against the live endpoints).
- **E1/E2 — evidence-backed drafts.** A draft asserting work was done must
  carry at least one Tier-Hard evidence reference (a real commit or Jira
  issue) — soft-only factual proposals are refused at construction — and
  every evidence ref must resolve to a stored source object (composite FK +
  INNER JOIN: the v1 grounding pattern, reused).
- **L1 — audit-before-acknowledge.** Every state transition appends to a
  hash-chained, append-only audit log in the same SQLite transaction, and an
  `execution_started` record hashing the exact outbound bytes commits
  *before* any network send. An action that cannot be audited does not
  execute. Honest threat model: the chain makes tampering with the local DB
  *evident* (verified end-to-end by `verify-chain`); it is not a
  cryptographic notary — there is no external anchor.
- **Drafts are templated, not generated.** Applying the v1 lesson (a small
  model orders well but garbles prose), drafts are fixed templates whose
  slots are typed, validated fields (commit SHAs, links, ticket keys). Free
  text from email/Slack bodies is never interpolated into a draft body — so
  "ignore previous instructions" arriving in an email has no path into an
  outward message through this backend.

### Correlation: asks ↔ issues ↔ commits

The GitWatcher reads configured local repos (no network, no credential) and
stores commits as Tier-Hard evidence. A deterministic correlator binds
Gmail/Slack asks that mention an issue key ↔ the Jira issue ↔ the commits
into **work threads**, and queues "work done" reply proposals through the
approval machinery, showing its confidence and basis in the queue. Matching
is word-boundary-exact (`ALM-1` matches; `PSALM-1` and `ALM-10` don't), and
Almanac's own Jira comments are excluded from evidence so it can't cite
itself.

### Planning, learning, outcomes

- **Deterministic prioritization.** The plan ("do now / by EOD / can wait")
  is an integer-weighted score over named factors (actionability, deadline
  proximity, hard evidence, classifier kind, staleness, priority sender)
  with a templated per-item rationale and a total stable sort — same inputs,
  same plan. Re-planning is idempotent (a rejected proposal is never
  resurrected; new evidence on the same ask + issue doesn't mint a
  duplicate) and each re-plan cycle is itself an audited event.
- **Plan state across days.** Snooze / done / dismiss per item, each an
  audited transition; snoozed items return when due; state is applied at
  read time, so viewing the plan never writes.
- **Decision capture → learning flywheel.** Every approve / reject / execute
  / snooze / done / dismiss is logged with the item's factor vector at
  decision time. From that log the ranker derives per-factor multipliers
  that are **bounded** (0.5×–1.5×; deadlines are never scaled),
  **explainable** (each adjustment carries a plain-English rationale, shown
  in the UI), gated on a minimum sample count, and **resettable** (one
  toggle restores base weights). Weights are derived on demand from the log
  — deterministic, no hidden model state.
- **Outcome tracking.** When an item Almanac acted on reaches a done status
  in Jira, the loop closes with that issue recorded as the closing evidence
  — audited, idempotent, and read-only toward external services.

### Seeing what it did

Transparency panels back all of the above: an audit-log viewer (chain-intact
badge + recent records, every event type surfaced), a completed-and-dismissed
view whose Reopen button is a normal audited transition (undo goes through
the same single path, on the chain), and a data & privacy inventory showing
exactly what is stored locally (per-source counts, DB size) with a one-click
JSON export.

## Architecture

```
+--------------------------------------------------------------+
|  Tauri shell (React/Vite) — briefing, plan, approval queue,   |
|  audit log, learning & data panels; string DTOs via IPC only  |
+---------------------------|----------------------------------+
                            | Tauri IPC
+---------------------------v----------------------------------+
|  almanac-core (headless Rust; runs & tests without the UI)    |
|                                                               |
|  SourceAdapter trait          Extraction        Synthesis     |
|   - GmailAdapter        -->    rules +     -->  Backend       |
|   - CalendarAdapter            MiniLM           trait         |
|   - SlackAdapter               embeddings       (local Qwen)  |
|   - JiraAdapter                                               |
|  GitWatcher (local commits — network- and credential-free)    |
|        |                                                      |
|        v                                                      |
|  Correlator ---> Prioritizer ---> ActionProposals ---> act    |
|  (asks<->issues  (deterministic,  (approval queue)  executors |
|   <->commits)     learned weights)         hash-chained audit |
|        |                                                      |
|        v                                                      |
|  encrypted token store (DPAPI)                                |
|  SQLite (FK-grounded): source_objects | extracted_items |     |
|  briefings | proposals | audit_log | plan state | decisions | |
|  outcomes                                                     |
+---------------------------------------------------------------+
```

- **Payload-first**: adapters were modeled against captured real API
  responses (committed, redacted, in `/fixtures`), not docs —
  [`PAYLOAD_CORRECTIONS.md`](PAYLOAD_CORRECTIONS.md) documents 56 places the
  real payloads differ from what the docs imply (34 across Gmail, Calendar,
  and Slack; 15 more for Jira; 7 for `git log` itself).
- **Hybrid classifier**: high-precision rules first (calendar events,
  bulk-mail headers, Slack system messages, explicit ask/promise patterns),
  MiniLM prototype-similarity for the undecided rest; low-confidence items are
  flagged noise, retained, and auditable — never dropped.
- **Swappable synthesis**: the `SynthesisBackend` trait keeps the local model
  replaceable; the ordering is the model's, the rationale text is templated
  from real signals of the chosen sequence (see honest misses for why).
- **Run-scoping**: briefings select the latest extraction per source object
  over the user's **local** calendar day (DST-aware). Tomorrow's events surface
  in a separate "Coming up" preview rather than hijacking today's plan.
  Google calendar-notification emails that duplicate a briefed event are
  suppressed from selection — matched either by the event id embedded in the
  email's link (high precision) or, for pure agenda-digest emails, by the
  Google-Calendar sender when calendar items are already present that day.
  Everything stays stored; only the *selection* is dedup'd.

## Honest misses

Things that don't work as well as the demo suggests, measured, not vibes:

- **Classifier accuracy is 81% on the embedding path** (13/16 on a hand-labeled
  set that deliberately avoids every rule; rules handle the easy majority
  first). The three misses, named: a commitment read as an action-needed
  ("As promised, the budget summary … lands with you Monday" — recipient-directed
  phrasing), an informal event lost to low-confidence noise ("Coffee catch-up
  with Priya on Friday morning"), and an action read as noise ("The contract is
  waiting for your signature before it expires"). The structural limit:
  zero-shot text similarity can't reliably tell **who owes whom** — my promise
  vs. your ask needs sender/recipient modeling it doesn't have.
- **A 0.5B model can order better than it can explain.** Its sequences were
  consistently reasonable (events in time order, urgent work early), but its
  prose rationales were repetitive and semi-garbled. So the rationale you see
  is **deterministic and templated from real signals** (counts, event slots,
  what was front-loaded, what was suppressed); the model contributes ordering
  only. That's a capability boundary, documented rather than hidden.
- **Synthesis takes ~20–40 s** of CPU per briefing (0.5B Q4 on candle, greedy).
  Fine for once-a-morning; wrong for interactive use. Debug builds are ~20×
  slower — the workspace compiles dependencies optimized even in dev for this
  reason.
- **Real inboxes are mostly noise** — a live run briefed 4 items and filed 43
  as noise (correctly: newsletters, CI failures, sign-in alerts). Borderline
  content cuts both ways: an email with subject "test" squeaked into the
  briefing as action-needed at 0.34 similarity, while one-word Slack messages
  ("hi") were filed as low-confidence noise.
- **Slack tokens don't refresh** unless the Slack app opts into token
  rotation, which is irreversible — so the refresh path for Slack validates
  the non-expiring token live (`auth.test`) and says so, while the Google path
  does a real `refresh_token` rotation (verified by token fingerprint change).
- **Third-party web apps occasionally mis-handle a correct deep link.** Slack's
  browser interstitial can drop the message anchor (lands on the channel, not
  scrolled to the message) — though the link is byte-identical to Slack's own
  `chat.getPermalink`. Google Calendar sometimes shows "Could not find the
  requested event" on first load of an `eid` link and resolves on refresh —
  and the link is Google's own `htmlLink`, stored verbatim (the decoded `eid`
  matches the stored event id). Both are downstream web-app quirks on links
  Almanac constructs correctly, not grounding failures.
- **Google testing-mode consent expires every 7 days.** Composing degrades
  gracefully — an expired Gmail token still yields a Slack+Calendar briefing
  whose rationale notes "Gmail was unavailable this run"; a compose fails
  entirely only when *every* source fails. The status chip also warns when the
  token is older than 7 days.
- **Fetch is capped and the cap is now reported.** Each source pages up to a
  fixed bound (100 Gmail messages, 200 Calendar events, 400 channels/400
  msgs-per-channel per window). On a busy week the briefing rationale says so
  ("Gmail results were truncated at the fetch cap") rather than silently
  dropping recall.
- **Gmail deep links assume the browser's default Google account.** The
  `#all/<id>` form opens `/u/0`; a multi-account user whose connected account
  isn't the default lands in the wrong mailbox. Assessed during the audit and
  left as-is rather than shipping an `authuser=` form I couldn't verify against
  multiple accounts without risking the verified single-account behavior.
- **Windows-only token encryption** (DPAPI). Other platforms need a keychain
  backend and currently refuse to store tokens rather than write plaintext.
- Fixed after a full principal-engineer audit (all had regression tests added):
  a briefing-day *hijack* (a standing event tomorrow could date the briefing
  tomorrow and drop today), all-day events anchored to UTC instead of local
  midnight, UTC clock times misleading the model's ordering, a DST-at-midnight
  day that hard-failed, `csp: null` and an over-broad URL opener on the
  webview, and a `Debug` impl that could have logged raw content. Earlier
  hardening also fixed UTC day boundaries, the fetch window missing future
  events, and added the conservative cross-source dedup — whose caution proved
  itself when a third-party email titled "1 event happening tomorrow" (which
  *looks* like a Google agenda digest) was correctly left briefed rather than
  wrong-merged.

## Setup (honest version)

Windows-first (token encryption is DPAPI). Prereqs: Rust stable **1.91 or
newer** (the MSVC toolchain on Windows; `tract` sets the floor, and `Cargo.lock`
pins a dependency needing the 2024 edition), Node 18+.

```powershell
git clone https://github.com/ManeeshJupalle/Almanac.git; cd Almanac
npm install

# one-time model downloads (~580 MB total; inference never touches the network)
mkdir models\minilm, models\qwen2.5-0.5b-instruct
curl.exe -L -o models\minilm\model.onnx https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx
curl.exe -L -o models\minilm\tokenizer.json https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json
curl.exe -L -o models\qwen2.5-0.5b-instruct\model.gguf https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf
curl.exe -L -o models\qwen2.5-0.5b-instruct\tokenizer.json https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct/resolve/main/tokenizer.json
```

(On macOS/Linux use `mkdir -p models/minilm models/qwen2.5-0.5b-instruct` and
`curl` — but note token encryption is Windows-only for now; see honest misses.)

On a bare Linux box the engine needs a C++ toolchain whose `libstdc++` headers
match the GCC installation your `c++` driver selects (`esaxx-rs`, pulled in
under `tract`, fails on `<cstdint>` otherwise), plus the Tauri system deps if
you build the shell. [`.cursor/install.sh`](.cursor/install.sh) is the
executable version of that list — it provisions cloud agents and works as a
setup script for any Ubuntu checkout.

Credentials (bring your own — nothing is provisioned for you):

1. **Google**: create a Google Cloud project, enable the Gmail and Calendar
   APIs, configure the OAuth consent screen (External + *Testing* is fine —
   add yourself as a test user; note that **testing-mode refresh tokens
   expire after 7 days** and you'll re-consent weekly), create a **Desktop
   app** OAuth client, download it as `credentials.json` into the repo root.
2. **Slack**: create an app at api.slack.com, add
   `http://localhost:8080/callback` as a redirect URL, copy the client
   id/secret.
3. `Copy-Item .env.example .env` (PowerShell; `cp` on macOS/Linux) and fill in
   the Slack values.
4. **Jira** (optional — feeds the action layer): create an API token at
   id.atlassian.com and fill `JIRA_BASE_URL` / `JIRA_EMAIL` /
   `JIRA_API_TOKEN` in `.env`. `jira-auth` (below) validates it live and
   stores it DPAPI-encrypted; the plaintext `.env` value can be blanked
   afterward.
5. **GitWatcher** (optional — commit evidence): set `ALMANAC_GIT_REPOS` in
   `.env` to semicolon-separated local repo paths. It shells out to local
   `git` only and holds no network credential.

First run:

```sh
cargo run -p fixture-capture -- google-auth   # one-time browser consent
cargo run -p fixture-capture -- slack-auth    # one-time browser consent
cargo run -p fixture-capture -- jira-auth     # optional: validate + encrypt the Jira token
cargo run -p almanac-core -- --self-check     # prints "core ok"
cargo test -p almanac-core                    # model tests self-skip if models absent
npm run tauri dev                             # open the app → Compose today's briefing
```

The SQLite store lives at `%APPDATA%\Almanac\almanac.db`. Tokens are
DPAPI-encrypted under `tokens/` (gitignored). The first `Compose` fetches a
7-day window (plus tomorrow, for calendar), classifies on-device, sequences
with the local model (~1 min total), and persists the briefing — relaunching
the app renders it from SQLite instantly.

### Packaging status (honest)

`tauri build` produces a working release binary that resolves `.env`, tokens,
and models by searching upward from the executable — running it from a
checkout works. A distributable installer (config/keychain migration away
from repo-relative paths) is future work; v0.1.0 is a run-from-checkout
release by design.

## CLI (headless core)

The engine runs and is tested without the UI:

```sh
# briefing pipeline
cargo run -p almanac-core -- --self-check       # db + migrations sanity
cargo run -p almanac-core -- extract-fixtures   # offline: fixtures → classified items (prints network probe)
cargo run -p almanac-core -- e2e-fixtures       # offline: fixtures → extraction → validated briefing
cargo run -p almanac-core -- live-briefing      # full live chain, prints the briefing
cargo run -p almanac-core -- refresh-google     # forced token refresh with fingerprint evidence

# action layer
cargo run -p almanac-core -- fetch-jira         # fetch + store recent Jira issues as source objects
cargo run -p almanac-core -- git-watch          # local commits → Tier-Hard evidence (offline)
cargo run -p almanac-core -- correlate          # bind asks↔issues↔commits, queue proposals (read-only)
cargo run -p almanac-core -- list-proposals     # approval queue + audit tail
cargo run -p almanac-core -- plan               # prioritized plan (do now / by EOD / can wait)
cargo run -p almanac-core -- replan             # one audited, idempotent re-plan cycle
cargo run -p almanac-core -- decisions          # decision log, with factors at decision time
cargo run -p almanac-core -- learning           # what the ranker learned from you, and why
cargo run -p almanac-core -- outcomes           # reconcile + list resolved loops (closing evidence)
cargo run -p almanac-core -- closed             # done / dismissed / resolved items
cargo run -p almanac-core -- audit-log          # browse the hash chain (chain-intact + records)
cargo run -p almanac-core -- verify-chain       # walk + verify the full audit chain
cargo run -p almanac-core -- data export        # local data counts → JSON inventory export
```

## Stack

| Layer | Choice |
|---|---|
| Core engine | Rust, headless crate (`almanac-core`), 142 tests |
| Desktop shell | Tauri v2 (thin IPC client, no engine logic; strict CSP, scoped opener) |
| UI | React + Vite, hand-rolled CSS |
| Store | SQLite (rusqlite, embedded migrations, FK-grounded) |
| Extraction | all-MiniLM-L6-v2 ONNX via tract-onnx (pure Rust) |
| Synthesis | Qwen2.5-0.5B-Instruct GGUF via candle (pure Rust) |
| Actions | propose → approve → execute; SHA-256 hash-chained audit; templated drafts |
| Tokens | Windows DPAPI, encrypted at rest |
| CI | Linux + Windows (DPAPI) test jobs, workspace build, `cargo`/`npm audit` |

Built in six phases (scaffold → payload-first fixtures → adapters →
extraction → synthesis → UI → ship), hardened against a full
principal-engineer audit, then extended with the action layer (propose →
approve → execute over a hash-chained audit log, Jira + git correlation,
deterministic planning, a bounded learning loop, and outcome tracking).
`PAYLOAD_CORRECTIONS.md` and the honest misses above are part of the
deliverable, not an apology.
