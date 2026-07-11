# Almanac — Architecture

> Local-first daily-briefing desktop agent. Opens to a synthesized plan for your
> day, pulled from your real sources, with every suggested action grounded back
> to the message or event it came from. Raw content never leaves the machine.

---

## 1. What it is (one paragraph)

Almanac is a Rust + Tauri desktop application that connects to a user's Gmail,
Google Calendar, and Slack, extracts the day's commitments/threads/events
on-device, and produces a synthesized "here is your day" briefing: what needs
action, what's noise, a suggested sequence, and *why* — with each item
click-through-grounded to its source. Extraction and synthesis both run
on-device (local model); raw message content is never transmitted off the
machine. A `SynthesisBackend` trait keeps the model swappable so a
local-vs-alternative quality delta can be measured and documented honestly.

## 2. Defensible-gap statement

Existing "AI inbox" tools are cloud-first (your mail is uploaded) and produce
ungrounded summaries (no click-back to source; hallucinated tasks). Almanac's
gap: **local-first + grounded**. Every asserted task carries a provenance handle
to the exact source object. Nothing is surfaced that can't be traced back. This
is the Synapse privacy thesis applied to the daily-briefing problem, and the
grounding requirement is the anti-hallucination signal recruiters read as rigor.

Distinct from **Starling** (multi-agent orchestration): Almanac is *single-agent
synthesis + grounding*, not orchestration. Keep the framing clean so the two
projects read as different domains.

## 3. Stack

| Layer | Choice | Rationale |
|---|---|---|
| Core engine | Rust (Tokio async) | Systems signal; matches Auricle/FluxFS pattern |
| Desktop shell | Tauri v2 | Ground-up cross-platform desktop (the legit Rust/Tauri credential, not Electron) |
| UI | React + Vite, embedded in binary | Auricle pattern: thin shell over headless engine |
| Local store | SQLite (`sqlx` or `rusqlite`) | Fixtures, extracted entities, run history, token store |
| On-device extraction | ONNX embeddings (all-MiniLM-L6-v2), rule + embedding hybrid | Proven Synapse stack |
| On-device synthesis | Local quantized LLM behind `SynthesisBackend` trait | User decision: fully local. Trait keeps it swappable for honest delta measurement |
| Integrations | Gmail, Google Calendar, Slack — all live OAuth | User decision: all three live |

## 4. Component architecture

```
+-------------------------------------------------------------+
|  Tauri Shell (React/Vite UI)                                |
|   - Briefing view (grounded task list, sequence, "why")     |
|   - Source connection / OAuth status                        |
|   - Provenance click-through (task -> source object)        |
+---------------------------|---------------------------------+
                            | Tauri IPC (commands/events)
+---------------------------v---------------------------------+
|  Rust Core (headless, independently runnable)               |
|                                                             |
|  +-----------------+   +-----------------+   +-----------+   |
|  | Source Adapters |   | Extraction      |   | Synthesis |   |
|  |  (trait)        |-->| Engine          |-->| Backend   |   |
|  |  - GmailAdapter |   | - ONNX embed    |   |  (trait)  |   |
|  |  - CalAdapter   |   | - commitment/   |   | - Local   |   |
|  |  - SlackAdapter |   |   action detect |   |   LLM     |   |
|  +-----------------+   | - grounding IDs |   +-----------+   |
|         |              +-----------------+                   |
|         v                      |                             |
|  +-----------------+           v                             |
|  | Auth/Token Store|   +-----------------+                   |
|  | (encrypted)     |   | Grounding Index |                   |
|  +-----------------+   | (item -> source)|                   |
|                        +-----------------+                   |
|                                                             |
|  +-------------------------------------------------------+   |
|  | SQLite: fixtures | entities | briefings | tokens      |   |
|  +-------------------------------------------------------+   |
+-------------------------------------------------------------+
```

## 5. Key interfaces (the traits that make it defensible)

```rust
/// Every source (Gmail, Calendar, Slack) implements this.
/// Adding a source = adding one impl. This is the "pluggable" story.
#[async_trait]
pub trait SourceAdapter {
    fn source_id(&self) -> SourceId;
    async fn authenticate(&mut self) -> Result<()>;
    async fn refresh_token(&mut self) -> Result<()>;
    /// Returns raw source objects for the window. NEVER leaves the machine.
    async fn fetch_window(&self, window: TimeWindow) -> Result<Vec<SourceObject>>;
}

/// A source object always carries a stable provenance handle.
pub struct SourceObject {
    pub provenance: ProvenanceRef, // {source, native_id, deep_link}
    pub raw: RawContent,           // stays local
    pub occurred_at: DateTime<Utc>,
}

/// Extraction turns raw objects into candidate items, each still grounded.
pub struct ExtractedItem {
    pub kind: ItemKind,            // Commitment | Event | ActionNeeded | Noise
    pub summary: String,           // on-device generated
    pub provenance: ProvenanceRef, // MUST trace back; no orphan items
    pub signals: ExtractionSignals,
}

/// Synthesis is swappable so local-vs-alt delta can be measured honestly.
#[async_trait]
pub trait SynthesisBackend {
    fn backend_id(&self) -> &str;      // "local-<model>" | "alt-..."
    async fn synthesize(&self, items: Vec<ExtractedItem>, ctx: DayContext)
        -> Result<Briefing>;
}

pub struct Briefing {
    pub sequence: Vec<PlannedItem>,    // ordered
    pub rationale: String,             // the "why"
    /// INVARIANT: every PlannedItem.provenance resolves to a real SourceObject.
    /// A briefing with an ungrounded item is a hard failure, not a warning.
}
```

## 6. Hard invariants (these are the project's identity — never relax)

1. **Grounding is total.** Every item in a briefing resolves to a real source
   object via `ProvenanceRef`. An ungrounded/hallucinated item is a test
   failure, not a display glitch. This is the anti-hallucination signal.
2. **Raw content is local-only.** No `RawContent` is ever serialized to a
   network call. Enforced by keeping the synthesis backend on-device and by a
   test that asserts no raw field crosses an adapter/network boundary.
3. **Payload-first.** No code is written against a source API until a real
   response from that API is captured as a fixture in `/fixtures`. Applies to
   all three integrations.
4. **Headless-runnable core.** The Rust core runs and is testable without the
   Tauri shell (Auricle pattern). The UI is a client, not the engine.
5. **Honest README.** Local-vs-alternative synthesis quality delta is measured
   and documented, misses and all (CashPulse discipline).

## 7. Data flow (a morning)

1. App opens → core checks token validity for all 3 sources (refresh if needed).
2. Each `SourceAdapter.fetch_window(today)` pulls raw objects → local SQLite.
3. Extraction engine: embed + rule-detect → `ExtractedItem`s, each carrying
   provenance. Noise filtered but retained (auditable).
4. `SynthesisBackend.synthesize(items)` → `Briefing` (sequence + rationale).
5. Grounding validation pass: assert every `PlannedItem` resolves. Fail loud.
6. UI renders briefing; each item click-through opens its source via deep link.

## 8. Security / privacy notes

- OAuth tokens encrypted at rest in the local store (OS keychain where available
  via Tauri, else encrypted SQLite field).
- Three separate OAuth lifecycles (Gmail, Google Cal — can share a Google
  project/scopes; Slack separate). Token refresh handled per-adapter.
- Privacy claim wording for README (must stay literally true): *"Raw message and
  event content is processed entirely on-device and is never transmitted. Only
  you see your data."* Do not overclaim beyond what code enforces.

## 9. Out of scope for v1 (say it, don't build it)

- System-tray / launch-on-startup daemon polish (can *describe* as "designed to
  run as a persistent local service"; don't sink the week into it).
- Write-back actions (sending replies, creating events). v1 is read + brief +
  ground only. Write-back is a later phase and a bigger safety surface.
- Mobile.
