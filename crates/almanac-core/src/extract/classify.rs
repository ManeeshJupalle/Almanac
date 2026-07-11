//! Semantic half of the hybrid classifier: prototype-centroid similarity.
//! Each ItemKind gets a handful of prototype sentences; an item is assigned
//! the kind whose (L2-normalized) centroid is most similar to the item's
//! embedding. Low similarity → flagged low-confidence Noise (retained).

use anyhow::Result;

use super::embed::{cosine, EmbeddingModel};
use super::{EmbeddingScores, ItemKind};

/// Below this max-similarity the classifier refuses to guess and the item is
/// retained as low-confidence Noise. Chosen against the labeled mini-set.
const CONFIDENCE_THRESHOLD: f32 = 0.30;

const COMMITMENT_PROTOTYPES: &[&str] = &[
    "I'll send you the report by tomorrow.",
    "I will take care of scheduling the meeting.",
    "I'll follow up with the team and get back to you.",
    "Consider it done, I'll handle it this week.",
    "I said I would review the document tonight.",
    "I'll get the fix deployed before Friday.",
];

const EVENT_PROTOTYPES: &[&str] = &[
    "Team standup meeting at 10am tomorrow.",
    "Calendar invitation: project review on Wednesday.",
    "Your appointment is scheduled for July 8 at 1pm.",
    "Reminder: dentist appointment this afternoon.",
    "The all-hands meeting starts in one hour.",
    "Lunch with Sarah at noon on Thursday.",
];

const ACTION_PROTOTYPES: &[&str] = &[
    "Can you review this pull request by end of day?",
    "Please approve the budget request before Friday.",
    "Could you send me the updated slides?",
    "We need your feedback on the proposal before we ship.",
    "Please respond to confirm your attendance.",
    "Your input is required on the design decision.",
];

const NOISE_PROTOTYPES: &[&str] = &[
    "Weekly newsletter: top stories in tech this week.",
    "Your order has shipped and is on its way.",
    "New sign-in to your account from Chrome on Windows.",
    "Flash sale: 40% off everything this weekend only.",
    "You have three new followers on LinkedIn.",
    "Release notes for version 2.4.1 are now available.",
];

pub struct PrototypeClassifier {
    model: EmbeddingModel,
    centroids: [(ItemKind, Vec<f32>); 4],
}

impl PrototypeClassifier {
    pub fn new(model: EmbeddingModel) -> Result<Self> {
        let centroid = |texts: &[&str]| -> Result<Vec<f32>> {
            let mut sum = vec![0f32; 0];
            for t in texts {
                let v = model.embed(t)?;
                if sum.is_empty() {
                    sum = vec![0f32; v.len()];
                }
                for (s, x) in sum.iter_mut().zip(&v) {
                    *s += x;
                }
            }
            let norm = sum.iter().map(|v| v * v).sum::<f32>().sqrt();
            Ok(sum.iter().map(|v| v / norm).collect())
        };
        let centroids = [
            (ItemKind::Commitment, centroid(COMMITMENT_PROTOTYPES)?),
            (ItemKind::Event, centroid(EVENT_PROTOTYPES)?),
            (ItemKind::ActionNeeded, centroid(ACTION_PROTOTYPES)?),
            (ItemKind::Noise, centroid(NOISE_PROTOTYPES)?),
        ];
        Ok(Self { model, centroids })
    }

    /// Returns (kind, all scores, confident). When `confident` is false the
    /// caller records the item as low-confidence Noise — never drops it.
    pub fn classify(&self, text: &str) -> Result<(ItemKind, EmbeddingScores, bool)> {
        let v = self.model.embed(text)?;
        let sims: Vec<(ItemKind, f32)> =
            self.centroids.iter().map(|(k, c)| (*k, cosine(&v, c))).collect();

        let scores = EmbeddingScores {
            commitment: sims[0].1,
            event: sims[1].1,
            action_needed: sims[2].1,
            noise: sims[3].1,
        };
        let (best_kind, best_sim) =
            sims.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1)).unwrap();

        if best_sim < CONFIDENCE_THRESHOLD {
            return Ok((ItemKind::Noise, scores, false));
        }
        Ok((best_kind, scores, true))
    }
}
