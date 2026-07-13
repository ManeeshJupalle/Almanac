# Almanac v2 — Architecture (Agentic Layer)

> v1 shipped a local-first, grounded daily briefing: read → extract → synthesize
> → render, with every item traceable to a real source object.
> v2 extends it into an agent: **observe** what you actually do, **reason** about
> what it means (correlate asks to work to evidence, prioritize the day), and
> **act** on your behalf — with every action proposed first, approved by you,
> and recorded in a tamper-evident audit chain.

---

## 1. What v2 is (one paragraph)

Almanac v2 is a local-first agentic layer on the v1 engine. It watches verifiable
work signals (git commits, CI, Jira transitions) and ambient activity (active
window, then screen capture + on-device OCR/vision), correlates them with the
asks arriving in Gmail/Slack/Calendar ("did you fix the bug?" ↔ ALM-42 ↔ commit
`a1b2c3` at 10:02), continuously re-prioritizes the day by urgency, importance,
and complexity, and **proposes** outward actions — reply to the manager, close
the Jira ticket, post to the Slack channel — which execute only after explicit
human approval, each carrying its full evidence chain into a hash-chained,
tamper-evident audit log. Raw content (including screen captures, the most
sensitive class of data the app now touches) never leaves the machine.

## 2. Defensible-gap statement (v2)

Agentic assistants today either (a) act autonomously and hallucinate claims into
your outbox, or (b) observe nothing and can only paraphrase your inbox back at
you. Almanac v2's gap: **evidence-tiered agency under human approval**. No
outward claim without a hard, verifiable artifact behind it; no action without a
human click; no action without a tamper-evident audit record. "AI that watches
your screen" is a gimmick without this — the approval + evidence + audit triad
is the product.

Portfolio framing: v2 ports the ProofPilot pattern (approval gates, hash-chained
flight recorder, simulation mode) into an agent that Almanac v1 already grounds.
Distinct from Starling (orchestration of many agents): this is ONE agent, deep,
with provable claims.

## 3. The three layers

```
┌──────────────────────────────────────────────────────────────────┐
│ LAYER 1 — OBSERVE                                                │
│  Hard-evidence sources: GitWatcher, CI, Jira events              │
│  Soft-evidence sources: ActiveWindowObserver, ScreenObserver     │
│                         (event-driven, on-device OCR/vision)     │
├──────────────────────────────────────────────────────────────────┤
│ LAYER 2 — REASON                                                 │
│  CorrelationEngine: ask ↔ work-item ↔ evidence                   │
│  Prioritizer: urgency × importance × complexity → re-planned day │
│  (feeds the existing v1 briefing; briefing becomes live, not     │
│   just morning-static)                                           │
├──────────────────────────────────────────────────────────────────┤
│ LAYER 3 — ACT (propose-then-approve, always)                     │
│  ActionProposals → Approval Queue (UI) → ActionExecutors         │
│  (GmailSend, SlackPost, JiraTransition) → AuditChain             │
└──────────────────────────────────────────────────────────────────┘
```

## 4. Evidence tiering — the new core invariant

Every piece of evidence carries a tier:

| Tier | Sources | May be used for |
|---|---|---|
| **H (hard)** | git commit, CI run, Jira transition/comment, calendar event, a message actually sent/received | Backing outward-facing claims; justifying proposed actions; prioritization |
| **S (soft)** | active-window observations, screen OCR/vision extracts | Prioritization, context, drafting hints — **never the sole basis for an outward claim** |

**INVARIANT E1:** An `ActionProposal` whose draft asserts a fact about work done
("fixed at 10:02", "PR merged", "ticket resolved") MUST cite ≥1 Tier-H
`EvidenceRef` supporting that fact. Enforced in the proposal constructor (same
private-field/validating-`new()` pattern as `ExtractedItem`) and re-checked by
the pipeline before an action may enter the approval queue. Soft evidence may
appear in the chain only alongside hard evidence, clearly labeled.

**INVARIANT E2 (grounding, extended):** every `EvidenceRef` resolves to a real
stored artifact (FK-enforced, as v1 does for `ProvenanceRef`). No orphan
evidence, no orphan proposals.

## 5. Action lifecycle — propose-then-approve, always

```
Proposed ──(user approves)──► Approved ──(executor runs)──► Executed
   │                             │                             │
   ├──(user rejects)──► Rejected └──(API fails)──► ExecutionFailed
   └──(TTL passes)───► Expired
Every transition (including Expired) appends an AuditRecord.
```

**INVARIANT A1:** There is no code path from `Proposed` to an executor that does
not pass through an explicit user approval event. No auto-approve config flag
exists in v2. (Tiered autonomy is a deliberate non-goal for v2 — revisit only
after the agent has an accuracy track record to justify it.)

**INVARIANT A2:** Executors are the ONLY code that holds write scopes, and each
executor validates the proposal state is `Approved` at execution time (defense
in depth against UI bugs).

**Simulation mode:** every executor implements `dry_run()` rendering exactly
what would be sent (final MIME/payload), byte-for-byte, without sending. The
approval UI shows the dry-run output — the user approves what will actually go
out, not a summary of it.

## 6. Audit chain (ProofPilot pattern, ported)

Append-only SQLite table `audit_records`:
`{seq, ts, actor, event, proposal_id, payload_hash, prev_hash, record_hash}`
where `record_hash = SHA-256(seq ‖ ts ‖ actor ‖ event ‖ proposal_id ‖
payload_hash ‖ prev_hash)`. Genesis record fixed. A `verify-chain` CLI command
walks and validates the chain; a broken link fails loudly.

**INVARIANT L1:** every proposal state transition and every executed action
appends a record BEFORE the side effect is acknowledged as complete. An action
that cannot be audited does not execute.

## 7. Key interfaces (new; v1 traits unchanged)

```rust
pub enum EvidenceTier { Hard, Soft }

pub struct EvidenceRef {
    pub tier: EvidenceTier,
    pub kind: EvidenceKind,        // GitCommit | CiRun | JiraEvent | Message |
                                   // CalendarEvent | WindowObservation | ScreenExtract
    pub native_id: String,         // resolvable, FK-enforced at rest
    pub deep_link: Option<String>, // hard evidence links out; soft may not
    pub observed_at: DateTime<Utc>,
}

/// Layer 1 — anything that produces evidence implements this.
#[async_trait]
pub trait ObserverSource {
    fn observer_id(&self) -> &str;
    fn tier(&self) -> EvidenceTier;            // a source is honest about itself
    async fn poll(&mut self) -> Result<Vec<Observation>>; // event-driven where possible
}

/// Layer 2 — correlation output.
pub struct WorkThread {
    pub ask: Option<ProvenanceRef>,        // the email/Slack ask, if any
    pub work_item: Option<EvidenceRef>,    // e.g. the Jira ticket
    pub evidence: Vec<EvidenceRef>,        // commits, CI, transitions…
    pub confidence: f32,                   // correlation confidence, surfaced in UI
}

/// Layer 3 — proposals. Private fields; validating constructor enforces E1/E2.
pub struct ActionProposal { /* private */ }
impl ActionProposal {
    pub fn new(kind: ActionKind, target: ActionTarget, draft: DraftContent,
               evidence: Vec<EvidenceRef>) -> Result<Self, ProposalViolation>;
}

pub enum ActionKind { GmailReply, GmailSend, SlackPost, JiraTransition, JiraComment }

#[async_trait]
pub trait ActionExecutor {
    fn action_kind(&self) -> ActionKind;
    async fn dry_run(&self, p: &ActionProposal) -> Result<RenderedAction>; // exact bytes
    async fn execute(&self, p: &ActionProposal) -> Result<ExecutionReceipt>; // asserts Approved
}
```

**Drafting:** `DraftingBackend` trait, same shape as `SynthesisBackend`. v1
lesson applies (0.5B sequences well, garbles prose): the DEFAULT drafting
backend is **templated-with-evidence-slots** ("Fixed in <commit short-sha> at
<time>; CI green: <link>. Closing <ticket>."). A model-generated drafting
backend can be slotted in and delta-measured honestly, but garbled prose never
ships as the default in something signed with the user's name.

## 8. Observation pipeline (Layer 1 soft sources)

- **Event-driven, not sampled:** capture on window-switch and idle↔active
  transitions, not per-second. Target steady-state CPU cost ≈ 0 when nothing
  changes.
- **OCR-first, vision-on-demand:** on-device OCR extracts text from captures;
  a vision model runs only when OCR yields nothing useful. All on-device.
- **Retention (INVARIANT R1):** raw captures live in a `raw_captures` store with
  a mandatory `expires_at` (default 24h, max 7d, user-configurable). A reaper
  deletes expired rows on every app start and hourly. Derived `Observation`s
  (window title, extracted text spans, embeddings) persist; raw pixels do not.
- **Consent & control:** per-app allowlist/blocklist for observation; a global
  pause; a "private mode" hotkey that drops all observation until resumed.
  README must carry an employer-policy note: screen observation of a work
  machine may require employer consent regardless of local-only storage —
  deployment-level decision, surfaced in onboarding, off by default.

## 9. Raw-content-local, extended

Screen captures and OCR extracts are `RawContent`-class data — the most
sensitive the app touches. The v1 compile-time guarantee extends to them:
capture and extract types implement no `Serialize`; nothing derived from a
capture crosses IPC except minimal display fields (observation kind, app name,
time); raw pixels never cross IPC at all. The approval UI's evidence display for
soft evidence shows labels/derived text, never the capture itself, unless the
user explicitly opens a local-only inspector view.

## 10. Data flow (the manager-email scenario, end to end)

1. Gmail adapter ingests manager's "did you fix ALM-42?" → extraction →
   ActionNeeded item (v1 path, unchanged).
2. GitWatcher observed commit `a1b2c3` "fix: ALM-42 …" at 10:02 (Tier-H);
   Jira source shows ALM-42 in review; CI run green (Tier-H). ActiveWindow saw
   the IDE on the repo 9:40–10:05 (Tier-S, context only).
3. CorrelationEngine binds ask ↔ ALM-42 ↔ {commit, CI} into a `WorkThread`
   (confidence surfaced).
4. Proposer emits three `ActionProposal`s: Gmail reply (templated draft citing
   commit + CI), Jira transition to Done, Slack post to the team channel. Each
   passes E1 (hard evidence present) or is refused at construction.
5. Approval queue renders all three with dry-run output + evidence chains.
   User approves reply + Jira, rejects the Slack post.
6. Executors run the two approved actions; receipts + audit records appended;
   the rejected proposal is audited as rejected.
7. The briefing updates: the ActionNeeded item resolves, the day re-prioritizes.

## 11. Security notes (delta from v1)

- **Write scopes** (`gmail.send`, Slack `chat:write`, Jira write) are held only
  by executors; consent re-run required; scopes documented in README.
- **Jira** is a new payload-first integration (fixtures before structs, as
  always). Dev target: a free Atlassian cloud site.
- **Token UX is now a feature:** an expired token (Google 7-day testing mode)
  must pause proposals for that target and surface re-auth — an agent that
  silently stops acting is worse than a briefing that goes stale.
- **Prompt-injection surface:** email/Slack/Jira text and OCR'd screen text are
  UNTRUSTED input to the correlator and drafter. Drafting templates interpolate
  only typed fields (sha, links, times) — never free text from untrusted input
  into an outward draft without it being visibly quoted in the approval UI.
- Threat model addition: the audit chain protects against silent tampering by
  malware or a curious co-user; it is not a cryptographic notary (no external
  anchor) — README states this honestly.

## 12. Out of scope for v2 (say it, don't build it)

- Any autonomy tier / auto-approve (explicit non-goal; revisit with data).
- Write-back beyond the three executors (no calendar writes, no file edits).
- Multi-user / team features; mobile; cloud sync.
- Keystroke logging and microphone/audio observation — never in scope.
