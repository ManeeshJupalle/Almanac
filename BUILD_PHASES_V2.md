# Almanac v2 — Phased Build Prompts

> One phase per Claude Code session. Verify the acceptance gate, commit, push,
> then start the next. Reasoning: xhigh default; max for 2.2 (correlation) and
> 2.5 (vision) if the first attempt is shaky.
>
> **Global hard constraints (every phase):**
> - Scope is exactly this phase. No forward-bleeding.
> - Payload-first for every NEW external API (Jira, gmail.send, chat.postMessage):
>   real response fixtured before any struct is modeled.
> - No v1 invariant weakened: grounding (memory/wire/disk/render), RawContent
>   non-Serialize, offline extraction+synthesis, encrypted tokens.
> - New invariants E1/E2 (evidence tiering + resolution), A1/A2 (approval-only
>   path, executor-held scopes), L1 (audit-before-acknowledge), R1 (raw-capture
>   retention) are never relaxed once introduced.
> - Report at end of phase: built / verified / deferred-and-why.
>
> **PREREQUISITE (before Phase 2.0):** triage AUDIT.md. All Critical and High
> security findings must be fixed (as their own gated fix session) BEFORE any
> write scope is added to this codebase. Adding send/post/transition capability
> on top of known critical issues is how audits become incidents.

---

## Phase 2.0 — Action layer: proposals, approval queue, audit chain, executors (Gmail + Slack)

**Scope:**
- `EvidenceRef`/`EvidenceTier` types; `ActionProposal` with private fields and a
  validating `new()` enforcing E1 (hard-evidence requirement for factual claims)
  and E2 (resolvable evidence, FK at rest). Migration for proposals, evidence,
  audit_records.
- Full state machine Proposed→Approved/Rejected/Expired→Executed/Failed with L1
  audit-before-acknowledge and the hash-chained audit log + `verify-chain` CLI.
- Two executors: `GmailReplyExecutor` (thread-aware reply), `SlackPostExecutor`.
  Both implement `dry_run()` (exact final payload) and `execute()` (asserts
  Approved). Write scopes obtained (gmail.send, chat:write) — consent re-run.
- PAYLOAD-FIRST: fixture a REAL sent email (to yourself) and a REAL Slack post
  (to a private test channel) — capture the send/post API responses to
  /fixtures. Corrections table for both.
- Templated `DraftingBackend` (evidence-slot templates only; no model drafting
  this phase).
- Minimal approval UI in the Tauri shell: queue list, dry-run display, evidence
  chain display, Approve/Reject. (Polish later; function now.)

**Hard constraints:** No code path from Proposed to an executor without a user
approval event (A1) — write a test that tries. Executors alone hold write scopes
(A2). No correlation, no observers, no Jira (blocked).

**Acceptance gates:**
- [ ] Proposal constructor REJECTS a factual-claim draft with only soft/no
      evidence (test), and rejects unresolvable evidence (test)
- [ ] State machine: illegal transitions impossible (tests incl. execute-on-
      Proposed and execute-on-Rejected)
- [ ] Audit chain: records appended on every transition; `verify-chain` passes;
      a manually corrupted record FAILS verification (test)
- [ ] Dry-run output is byte-identical to what execute() sends (test on Gmail
      MIME + Slack payload)
- [ ] LIVE: one real approved Gmail reply (to yourself) and one real approved
      Slack post (test channel) executed, receipts + audit records shown
- [ ] Send/post fixtures + corrections table committed
- [ ] Full suite green; no v1 test regressed

**Report:** the A1 bypass-attempt test, chain-corruption test output, live
receipts, corrections tables, scope list now held and by which module.

---

## Phase 2.1 — Jira integration (source + action target), payload-first

**Scope:** Free Atlassian cloud dev site as target. Fixture FIRST: issue GET,
search (JQL) response, transitions list, a real transition POST response, a real
comment POST response. Then: `JiraAdapter` (SourceAdapter — issues/events as
Tier-H evidence + source objects with deep links) and `JiraTransitionExecutor` +
`JiraCommentExecutor` (dry-run + execute, same A1/A2/L1 discipline). OAuth or
API-token auth stored encrypted like the others.

**Hard constraints:** payload-first (structs after fixtures). Executors validate
the transition is legal for the issue's current state via the transitions
endpoint — never guess transition IDs (they vary per project/workflow).

**Acceptance gates:**
- [ ] Jira fixtures + corrections table (expect: ADF rich-text bodies, per-
      workflow transition IDs, custom fields)
- [ ] JiraAdapter yields grounded SourceObjects w/ working deep links (verified)
- [ ] LIVE: one approved transition + one approved comment on a test issue,
      audited, receipts shown; dry-run byte-match test
- [ ] Illegal-transition attempt fails loudly BEFORE proposal reaches queue
- [ ] Full suite green; scopes documented

**Report:** corrections table, live receipts + audit seqs, deep-link samples.

---

## Phase 2.2 — Hard-evidence watchers + Correlation engine

**Scope:**
- `GitWatcher` ObserverSource (Tier-H): watches configured local repos (config
  list), emits commit observations {sha, message, time, repo, branch} — local
  read, no network. Optional CI evidence via existing fixtures pattern if a CI
  API is configured; otherwise defer CI to config-documented.
- `CorrelationEngine`: binds asks (v1 ExtractedItems) ↔ work items (Jira
  issues) ↔ evidence (commits, CI, Jira events) into `WorkThread`s with a
  confidence score. Deterministic signals first (issue keys in commit messages/
  branch names, explicit links), embedding similarity as tiebreaker (existing
  MiniLM), threshold below which NO thread is formed (no guessing).
- Proposer: from a resolved WorkThread, emit the proposal set (reply/transition/
  post) through the Phase 2.0 machinery.

**Hard constraints:** Correlation NEVER fabricates a link — below-threshold means
no proposal, surfaced as "possible match, needs your confirmation" at most. E1
enforced end-to-end: the manager-email scenario proposal must cite the commit/
Jira evidence. Confidence must be displayed in the approval UI.

**Acceptance gates:**
- [ ] GitWatcher emits Tier-H observations from a real local repo (live)
- [ ] Correlator binds a seeded end-to-end scenario: email ask + Jira issue +
      commit → one WorkThread (deterministic test on fixtures)
- [ ] Negative test: ambiguous/similar-but-wrong commit does NOT bind (below
      threshold → no proposal)
- [ ] Full scenario LIVE: seeded manager-style email → proposals generated with
      correct evidence chains → approved → executed → audited
- [ ] Full suite green

**Report:** the live scenario walkthrough with evidence chain, correlation
thresholds chosen and why, false-positive test evidence.

---

## Phase 2.3 — Prioritization & live re-planning

**Scope:** Prioritizer scoring open items by urgency (deadlines, asker,
staleness), importance (thread signals, participants), complexity (evidence of
scope); the briefing becomes live: re-ranks as new evidence/asks arrive; "do
now / by EOD / can wait" sections; expected-EOD summary. Scoring is
DETERMINISTIC and explainable (v1 rationale lesson) — every rank shows its
factors; the model may order within ties (existing SynthesisBackend), never
override explainable factors silently.

**Hard constraints:** No new external APIs. Explainability: every priority
decision renders its factor breakdown in UI. No black-box rank.

**Acceptance gates:**
- [ ] Deterministic scoring w/ factor breakdown (tests over fixture scenarios,
      incl. deadline-inversion and asker-weight cases)
- [ ] Live re-rank demonstrated: injecting a new urgent ask reorders the day
- [ ] Factor breakdown visible per item in UI
- [ ] Full suite green; no v1 regression

**Report:** scoring model factors + weights, a before/after re-rank capture,
honest note on where scoring is naive (feeds README).

---

## Phase 2.4 — Observer daemon: active-window source (Tier-S)

**Scope:** `ActiveWindowObserver` (Windows first: Win32 foreground-window +
process name; event-driven on focus change + idle/active transitions). Emits
Tier-S observations {app, window title, span}. Per-app allowlist/blocklist
config, global pause, private-mode toggle (UI). Observations feed prioritizer
context ONLY (Tier-S — E1 keeps them out of outward claims; add the test).

**Hard constraints:** Event-driven (no polling loop tighter than idle checks);
measured steady-state overhead reported. Window titles are sensitive: display
minimally, never cross IPC beyond what the UI shows, never enter drafts.
Off by default; explicit enable in UI with the employer-policy note shown.

**Acceptance gates:**
- [ ] Focus-change events captured live; spans correct across a scripted
      app-switch sequence
- [ ] Test: a Tier-S-only proposal for a factual claim is REJECTED (E1 holds)
- [ ] Blocklisted app produces NO observations (test); pause + private mode work
- [ ] Overhead measured and reported (CPU % steady-state)
- [ ] Off-by-default verified on fresh profile

**Report:** overhead numbers, the E1 rejection test, consent/config UX shots.

---

## Phase 2.5 — Screen capture + on-device OCR/vision (Tier-S) + retention

**Scope:** Event-driven screen capture (on focus change into allowlisted apps),
on-device OCR (choose pure-Rust-friendly OCR; justify choice Phase-4-style
BEFORE building); optional on-device vision model ONLY when OCR is empty
(justify model + runtime, cross-platform, candle-style). `raw_captures` store
with mandatory `expires_at` (default 24h) + reaper (start + hourly) — R1.
Derived ScreenExtract observations persist; raw pixels expire. Non-Serialize
extended to capture/extract raw types; local-only inspector view in UI.

**Hard constraints:** STEP 0 — propose OCR + vision choices with justification
first (my veto point). All inference offline (network-disabled proof, like
Phases 3/4). R1 enforced with a test (expired rows actually deleted). Captures
never cross IPC; blocklist/private-mode apply to capture identically.

**Acceptance gates:**
- [ ] OCR/vision choices proposed + justified; offline run proven
- [ ] Capture only on allowlisted focus events (test w/ blocklisted app)
- [ ] Reaper test: expired raw captures deleted; derived observations retained
- [ ] Extracted text improves a prioritization/context case (demonstrated) while
      E1 rejection still holds for Tier-S-only claims (regression test)
- [ ] Compile-time non-Serialize on capture types; IPC audit shows no capture
      egress
- [ ] Overhead measured (capture+OCR cost per event); full suite green

**Report:** chosen OCR/vision stack + why, overhead, retention proof, honest
note on OCR quality misses (README material).

---

## Phase 2.6 — Hardening, README v2, ship

**Scope:** End-to-end hardening (token-expiry pauses proposals + surfaces
re-auth; partial-source degradation; approval-queue TTL/expiry sweep); fresh-
clone smoke test incl. Jira setup + write-scope consent, documented honestly;
security pass on the new surface (scopes audit, injection review on templates,
IPC audit); README v2: the evidence-tier/approval/audit story, the manager-
email scenario as the hero walkthrough, honest-miss section (correlation
precision, OCR quality, scoring naivety, prompt-injection posture, audit-chain
threat-model limits, anything open), employer-policy note; demo GIF of the FULL
scenario (seeded/innocuous data — redaction check is mandatory: approval UI
shows real drafts); tag v2.0.0, GitHub release.

**Acceptance gates:**
- [ ] Expired-token path: proposals pause + re-auth surfaced (exercised)
- [ ] Fresh-clone smoke from docs alone
- [ ] Every README claim verified against the running system; privacy + audit
      claims exactly as enforced, no overclaiming
- [ ] Demo GIF: ask → correlation → proposal w/ evidence → approve → executed →
      audit record; redaction-checked
- [ ] All tests + CI green; v2.0.0 tagged + released

**Report:** claim-verification checklist, known misses, release link.

---

## Commit points

One phase per session; gate passes → commit + push → next. A failed gate is
fixed within the phase, never carried forward.
