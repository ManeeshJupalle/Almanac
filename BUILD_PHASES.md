# Almanac — Phased Build Prompts

> One phase per Claude Code session. Verify the acceptance gate and commit to git
> before starting the next phase. Reasoning: xhigh default; max only if xhigh's
> first attempt on a hard phase is shaky. No ultracode/workflow mode.
>
> Every phase prompt below includes: scope, hard constraints, payload-vs-doc
> correction report requirement (where APIs are touched), acceptance gates, and
> reporting requirements. Hard components get vitest/`cargo test` coverage.
>
> **Global hard constraints (apply to every phase):**
> - Scope is exactly this phase. Do NOT build ahead. Future-phase features are blocked.
> - No code against any source API before its real payload is fixtured (Phase 1).
> - Grounding invariant and raw-content-local invariant are never relaxed.
> - Report at end of every phase: what was built, what was verified, what was skipped and why.

---

## Phase 0 — Scaffold & headless core skeleton

**Scope:** Tauri v2 project; Rust core as a separate crate/module that builds and
runs headless (a `cargo run --bin almanac-core -- --self-check` prints "core ok").
SQLite wired (empty schema migration runner). React/Vite shell renders a static
"Almanac" shell with an empty briefing view. No sources, no model yet.

**Hard constraints:** Core must build and pass `--self-check` WITHOUT the Tauri
shell. If the core only works via Tauri, the phase fails.

**Acceptance gates:**
- [ ] `cargo build` clean; `cargo run --bin almanac-core -- --self-check` prints ok
- [ ] `npm run tauri dev` launches, shows empty briefing view
- [ ] SQLite migration runner creates an empty DB on first run
- [ ] One `cargo test` exists and passes (even trivial), CI-ready layout

**Report:** directory tree, how core runs headless, DB path.

---

## Phase 1 — PAYLOAD-FIRST fixture capture (all 3 sources)

**Scope:** Stand up minimal OAuth for Gmail, Google Calendar, Slack — just enough
to make ONE real authenticated call each and **save the raw JSON response to
`/fixtures/{gmail,gcal,slack}/*.json`**. This phase's deliverable is *fixtures +
a correction report*, not features.

**Hard constraints:**
- Do NOT model any struct against API docs. Fetch real responses first.
- Capture at least: Gmail message list + one full message; Calendar events list
  for a day; Slack conversations + messages for a channel.
- Redact nothing structurally (keep shapes); may blank literal PII values.

**Payload-vs-doc correction report (REQUIRED):** For each source, a table of
every place the real payload differs from what the official docs imply (missing
fields, unexpected nulls, nesting, pagination cursors, date formats). This
report is a primary deliverable.

**Acceptance gates:**
- [ ] Real fixtures saved for all 3 sources under `/fixtures`
- [ ] `PAYLOAD_CORRECTIONS.md` written with the doc-vs-reality deltas
- [ ] Tokens stored encrypted (not plaintext) in local store
- [ ] A test loads each fixture and asserts it parses

**Report:** the corrections table, token-storage approach, any scope/permission
surprises per provider.

---

## Phase 2 — SourceAdapter trait + three adapters (fixture-backed, then live)

**Scope:** Implement `SourceAdapter` trait exactly as in ARCHITECTURE.md.
Implement Gmail, Calendar, Slack adapters. Each `fetch_window` first works
against the Phase-1 fixtures (deterministic tests), then against live APIs behind
the same interface. Every returned `SourceObject` carries a valid `ProvenanceRef`
(source + native_id + deep_link).

**Hard constraints:** Adapters return `SourceObject`s only; no extraction logic
here. Raw content populated but never sent anywhere. Deep links must be real
(constructable from native IDs — verify one manually per source).

**Acceptance gates:**
- [ ] All 3 adapters parse their fixtures into `SourceObject`s (unit tests)
- [ ] `fetch_window` works live for all 3 (manual smoke, documented)
- [ ] Every `SourceObject` has a resolvable `ProvenanceRef`; test asserts non-null
- [ ] Token refresh path exercised for at least one Google + Slack

**Report:** per-source object counts from a live run, one verified deep link each.

---

## Phase 3 — On-device extraction engine

**Scope:** ONNX all-MiniLM-L6-v2 embeddings loaded locally. Hybrid
rule+embedding classifier producing `ExtractedItem`s (Commitment | Event |
ActionNeeded | Noise), each carrying forward the source's `ProvenanceRef`. Noise
retained (flagged), not dropped.

**Hard constraints:** Runs fully offline (unplug network to verify). No item is
emitted without provenance — enforce in code + test. This is a hard component →
full `cargo test` coverage on the classifier boundaries.

**Acceptance gates:**
- [ ] Embeddings run offline; model bundled/loaded locally
- [ ] Extraction over fixtures yields items with correct `ItemKind` on a labeled mini-set
- [ ] Test: zero `ExtractedItem`s exist without a resolving `ProvenanceRef`
- [ ] Documented accuracy on the labeled mini-set (honest number)

**Report:** classifier accuracy on the mini-set, offline-run confirmation.

---

## Phase 4 — SynthesisBackend trait + local model

**Scope:** `SynthesisBackend` trait; a `LocalLlmBackend` running a quantized
model on-device that turns `ExtractedItem`s into a `Briefing` (ordered sequence +
rationale). Grounding validation pass: every `PlannedItem` MUST resolve to a real
source object or the briefing is rejected (hard failure).

**Hard constraints:** Fully on-device — network unplugged during synthesis test.
The grounding validation is not advisory: an ungrounded item fails the pipeline.
Keep the trait clean enough that a second backend could be dropped in later for
delta measurement.

**Acceptance gates:**
- [ ] Local synthesis produces a `Briefing` offline
- [ ] Grounding validator rejects a deliberately-ungrounded item (test proves it)
- [ ] End-to-end: fixtures → extraction → briefing, all offline, all grounded
- [ ] Briefing includes a human-readable rationale

**Report:** a sample generated briefing (redacted), synthesis latency, grounding-
validator test output.

---

## Phase 5 — Tauri UI: grounded briefing view

**Scope:** React briefing view — ordered task list, the "why" rationale, and
per-item **click-through to source** via deep link. Source-connection/OAuth
status panel. This is a UI phase → **screenshot-and-self-critique step required.**

**Hard constraints:** No new engine logic in the UI; it consumes core via Tauri
IPC only. Click-through must open the real source object.

**Acceptance gates:**
- [ ] Briefing renders from a live core run
- [ ] Each item click opens its real source (verified for all 3 sources)
- [ ] OAuth status panel reflects real connection state
- [ ] **Screenshot review done**; issues found + fixed are listed in the report

**Report:** before/after screenshots, the self-critique findings (empty states,
flat presentation, etc.), what was fixed.

---

## Phase 6 — Hardening, honest README, ship

**Scope:** Encrypted token store finalized; error/empty/expired-token states;
fresh-clone smoke test; demo GIF; README with verified claims and the
**local-synthesis honest-miss section** (where the local model produces weak
plans — document it, CashPulse-style). Optional: measure delta vs. an alternative
backend and report it.

**Hard constraints:** Every README claim verified against the running system. The
privacy sentence must stay literally true to what the code enforces. No
overclaiming.

**Acceptance gates:**
- [ ] Fresh-clone smoke test passes on a clean machine/dir
- [ ] Demo GIF recorded (hero: open app → grounded briefing → click to source)
- [ ] README: honest-miss section present; privacy claim matches enforcement
- [ ] All tests green; `cargo test` + any vitest pass
- [ ] Tag a GitHub release; (crates.io optional if you split a core crate)

**Report:** final claim-verification checklist, known misses, release link.

---

## Suggested commit points

Commit after each phase's gate passes. Never carry two phases in one session.
If a phase's gate fails, fix within the same phase scope — do not proceed.
