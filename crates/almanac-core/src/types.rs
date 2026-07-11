//! Core domain types shared by all source adapters (ARCHITECTURE.md §5).
//!
//! INVARIANT (raw-content-local): `RawContent` and `SourceObject` deliberately
//! do NOT implement `serde::Serialize`. Every outbound network call in this
//! codebase goes through serde-based clients, so raw content cannot cross a
//! network boundary without a loud, intentional escape hatch. A test in
//! `tests/raw_content_local.rs` statically asserts this stays true.

use chrono::{DateTime, Utc};

/// Identifies which source an object came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceId {
    Gmail,
    GoogleCalendar,
    Slack,
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SourceId::Gmail => "gmail",
            SourceId::GoogleCalendar => "gcal",
            SourceId::Slack => "slack",
        })
    }
}

/// Stable provenance handle: every surfaced item must trace back to one of
/// these (grounding invariant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceRef {
    pub source: SourceId,
    /// The source's own identifier (Gmail message id, Calendar event id,
    /// Slack `channel:ts`). Opaque — never parsed as a number.
    pub native_id: String,
    /// A real, openable URL to the source object.
    pub deep_link: String,
}

/// Raw payload from a source. Stays on this machine — no `Serialize` impl,
/// on purpose. Local consumers (extraction, local SQLite) use `as_json()`.
#[derive(Debug, Clone)]
pub struct RawContent(serde_json::Value);

impl RawContent {
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// Local, read-only access for on-device processing.
    pub fn as_json(&self) -> &serde_json::Value {
        &self.0
    }
}

/// A source object always carries a stable provenance handle (ARCHITECTURE.md).
#[derive(Debug, Clone)]
pub struct SourceObject {
    pub provenance: ProvenanceRef,
    pub raw: RawContent, // stays local
    pub occurred_at: DateTime<Utc>,
}

/// Half-open time window [start, end) that adapters fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}
