//! The `SourceAdapter` trait (ARCHITECTURE.md §5) and its three
//! implementations. Adapters return `SourceObject`s only — no extraction or
//! classification here (Phase 3). Raw content is populated but never sent
//! across a network boundary.

pub mod gcal;
pub mod gmail;
pub mod slack;

use anyhow::Result;
use async_trait::async_trait;

use crate::types::{SourceId, SourceObject, TimeWindow};

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
