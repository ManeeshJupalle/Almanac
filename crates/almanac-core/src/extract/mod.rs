//! On-device extraction engine (Phase 3): raw `SourceObject`s become
//! classified, grounded `ExtractedItem`s. Fully offline — no network I/O
//! exists anywhere in this module tree; embeddings run from local files.
//!
//! GROUNDING INVARIANT (ARCHITECTURE.md §6.1): no `ExtractedItem` exists
//! without a resolving `ProvenanceRef`. Enforced here by construction —
//! fields are private and `ExtractedItem::new` is the only door, validating
//! provenance — and again at rest by a SQLite foreign key.

mod classify;
mod embed;
mod rules;
mod text;

pub use embed::EmbeddingModel;

use anyhow::{ensure, Result};

use crate::types::{ProvenanceRef, SourceObject};

/// Classification of an extracted item, exactly as ARCHITECTURE.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Commitment,
    Event,
    ActionNeeded,
    Noise,
}

impl ItemKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemKind::Commitment => "commitment",
            ItemKind::Event => "event",
            ItemKind::ActionNeeded => "action_needed",
            ItemKind::Noise => "noise",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "commitment" => ItemKind::Commitment,
            "event" => ItemKind::Event,
            "action_needed" => ItemKind::ActionNeeded,
            "noise" => ItemKind::Noise,
            _ => return None,
        })
    }
}

impl std::fmt::Display for ItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Cosine similarity of the item text against each class centroid.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EmbeddingScores {
    pub commitment: f32,
    pub event: f32,
    pub action_needed: f32,
    pub noise: f32,
}

/// Auditable record of WHY an item was classified the way it was.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExtractionSignals {
    pub rule_hits: Vec<String>,
    pub embedding_scores: Option<EmbeddingScores>,
    /// "rule" | "embedding" | "fallback_low_confidence" | "fallback_no_model"
    pub decided_by: String,
}

/// A classified item that provably traces back to a source object.
/// Fields are private ON PURPOSE: `new()` is the only constructor and it
/// rejects unresolvable provenance. Do not add public field access that
/// would allow constructing or mutating an orphan item.
#[derive(Debug, Clone)]
pub struct ExtractedItem {
    kind: ItemKind,
    summary: String,
    provenance: ProvenanceRef,
    signals: ExtractionSignals,
}

impl ExtractedItem {
    pub fn new(
        kind: ItemKind,
        summary: String,
        provenance: ProvenanceRef,
        signals: ExtractionSignals,
    ) -> Result<Self> {
        ensure!(
            !provenance.native_id.trim().is_empty(),
            "grounding violation: empty native_id — an ExtractedItem must resolve to a source object"
        );
        ensure!(
            provenance.deep_link.starts_with("https://"),
            "grounding violation: deep_link '{}' is not an https URL",
            provenance.deep_link
        );
        Ok(Self { kind, summary, provenance, signals })
    }

    pub fn kind(&self) -> ItemKind {
        self.kind
    }
    pub fn summary(&self) -> &str {
        &self.summary
    }
    pub fn provenance(&self) -> &ProvenanceRef {
        &self.provenance
    }
    pub fn signals(&self) -> &ExtractionSignals {
        &self.signals
    }
}

/// The extraction pipeline: rules first, embeddings for the undecided rest.
pub struct Extractor {
    embedder: Option<classify::PrototypeClassifier>,
}

impl Extractor {
    /// Full hybrid extractor. `model_dir` must contain model.onnx +
    /// tokenizer.json (local files; nothing is fetched).
    pub fn with_model(model_dir: &std::path::Path) -> Result<Self> {
        let model = EmbeddingModel::load(model_dir)?;
        Ok(Self { embedder: Some(classify::PrototypeClassifier::new(model)?) })
    }

    /// Rules + fallback only. For tests and model-less environments; items
    /// rules can't decide become flagged Noise (retained, auditable).
    pub fn rules_only() -> Self {
        Self { embedder: None }
    }

    pub fn has_model(&self) -> bool {
        self.embedder.is_some()
    }

    /// Classify one source object. Never drops anything: every input yields
    /// exactly one ExtractedItem (Noise is retained and flagged).
    pub fn extract_one(&self, obj: &SourceObject) -> Result<ExtractedItem> {
        let source = obj.provenance.source;
        let raw = obj.raw.as_json();
        let text = text::classification_text(source, raw);

        let (rule_kind, rule_hits) = rules::apply(source, raw, &text.body);

        let (kind, signals) = match rule_kind {
            Some(kind) => (
                kind,
                ExtractionSignals {
                    rule_hits,
                    embedding_scores: None,
                    decided_by: "rule".to_string(),
                },
            ),
            None => match &self.embedder {
                Some(classifier) => {
                    let (kind, scores, confident) = classifier.classify(&text.body)?;
                    (
                        kind,
                        ExtractionSignals {
                            rule_hits,
                            embedding_scores: Some(scores),
                            decided_by: if confident {
                                "embedding".to_string()
                            } else {
                                "fallback_low_confidence".to_string()
                            },
                        },
                    )
                }
                None => (
                    ItemKind::Noise,
                    ExtractionSignals {
                        rule_hits,
                        embedding_scores: None,
                        decided_by: "fallback_no_model".to_string(),
                    },
                ),
            },
        };

        // Provenance passes through UNCHANGED from the source object.
        ExtractedItem::new(kind, text.summary, obj.provenance.clone(), signals)
    }

    /// Extract a batch. Output length always equals input length — noise is
    /// classified, never filtered.
    pub fn extract(&self, objects: &[SourceObject]) -> Result<Vec<ExtractedItem>> {
        objects.iter().map(|o| self.extract_one(o)).collect()
    }
}
