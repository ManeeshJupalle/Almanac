//! Synthesis (Phase 4): grounded `ExtractedItem`s become a `Briefing` —
//! an ordered sequence plus a human-readable rationale.
//!
//! Design for a small local model: the LLM only ever refers to items by
//! index into a numbered digest. `PlannedItem`s and their `ProvenanceRef`s
//! are constructed in code from the real items, so the model cannot invent
//! provenance. The grounding validation pass is still the hard final gate:
//! a briefing containing ANY unresolvable item is rejected, not repaired.
//!
//! Raw-content discipline: prompts are built exclusively from item summaries
//! (local distillates); raw payloads never enter a prompt. Inference is
//! in-process — nothing here can perform network I/O.

pub mod local_llm;

use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Timelike, Utc};
use rusqlite::Connection;

use crate::extract::{ExtractedItem, ItemKind};
use crate::types::ProvenanceRef;

/// Context the backend gets about the day being planned.
#[derive(Debug, Clone, Copy)]
pub struct DayContext {
    pub date: NaiveDate,
    pub now: DateTime<Utc>,
}

/// One step of the plan. Provenance is copied verbatim from the extracted
/// item it was built from — never synthesized.
#[derive(Debug, Clone)]
pub struct PlannedItem {
    pub provenance: ProvenanceRef,
    pub kind: ItemKind,
    pub summary: String,
    pub occurred_at: DateTime<Utc>,
}

/// INVARIANT: every PlannedItem.provenance resolves to a real SourceObject.
/// A briefing with an ungrounded item is a hard failure, not a warning.
#[derive(Debug, Clone)]
pub struct Briefing {
    pub sequence: Vec<PlannedItem>,
    pub rationale: String,
}

/// Synthesis is swappable so local-vs-alt delta can be measured honestly.
#[async_trait]
pub trait SynthesisBackend {
    fn backend_id(&self) -> &str; // "local-<model>" | "alt-..."
    async fn synthesize(&self, items: Vec<ExtractedItem>, ctx: DayContext) -> Result<Briefing>;
}

// ----------------------------------------------------------- pipeline ------

/// Generate a validated briefing for `ctx.date`.
///
/// Run-scoping: inputs come from `db::briefing_inputs` — for each
/// (source, native_id) only the LATEST extraction (max rowid) is selected,
/// restricted to source objects whose occurred_at falls on the UTC day.
/// Noise items are excluded from synthesis but counted in the rationale.
pub async fn generate_briefing(
    conn: &Connection,
    backend: &dyn SynthesisBackend,
    ctx: DayContext,
) -> Result<Briefing> {
    let inputs = crate::db::briefing_inputs(conn, ctx.date)?;
    let (noise, actionable): (Vec<_>, Vec<_>) =
        inputs.into_iter().partition(|i| i.kind() == ItemKind::Noise);

    let mut briefing = if actionable.is_empty() {
        Briefing {
            sequence: Vec::new(),
            rationale: format!(
                "Nothing needs your attention for {} — no commitments, events, or action items surfaced.",
                ctx.date
            ),
        }
    } else {
        let briefing = backend.synthesize(actionable.clone(), ctx).await?;
        validate_briefing(conn, &briefing, &actionable)?;
        briefing
    };

    if !noise.is_empty() {
        briefing.rationale.push_str(&format!(
            " ({} low-signal item{} classified as noise — retained in the local store, not briefed.)",
            noise.len(),
            if noise.len() == 1 { " was" } else { "s were" }
        ));
    }
    Ok(briefing)
}

/// The grounding validation pass (NOT advisory). Rejects the briefing if any
/// planned item (a) is not among the allowed inputs for this run, (b) does
/// not resolve to a stored source object, or (c) appears more than once.
pub fn validate_briefing(
    conn: &Connection,
    briefing: &Briefing,
    allowed: &[ExtractedItem],
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for planned in &briefing.sequence {
        let key = (planned.provenance.source, planned.provenance.native_id.clone());
        ensure!(
            seen.insert(key.clone()),
            "grounding violation: item {}:{} appears more than once in the sequence",
            planned.provenance.source,
            planned.provenance.native_id
        );
        ensure!(
            allowed.iter().any(|a| a.provenance() == &planned.provenance),
            "grounding violation: planned item {}:{} is not among this run's inputs",
            planned.provenance.source,
            planned.provenance.native_id
        );
        let resolves: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM source_objects WHERE source = ?1 AND native_id = ?2)",
                (planned.provenance.source.to_string(), &planned.provenance.native_id),
                |row| row.get(0),
            )
            .context("querying source_objects for grounding validation")?;
        ensure!(
            resolves,
            "grounding violation: planned item {}:{} does not resolve to a stored source object",
            planned.provenance.source,
            planned.provenance.native_id
        );
    }
    Ok(())
}

// ------------------------------------------------- digest + plan parsing ----

/// Numbered, kind-tagged digest the model plans over. Summaries only — no
/// raw content, no IDs (the model cannot leak or invent what it never sees).
pub fn build_digest(items: &[ExtractedItem]) -> String {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let tag = match item.kind() {
                ItemKind::Event => {
                    let t = item.occurred_at();
                    format!("event at {:02}:{:02} UTC", t.hour(), t.minute())
                }
                ItemKind::ActionNeeded => "action needed".to_string(),
                ItemKind::Commitment => "commitment".to_string(),
                ItemKind::Noise => "noise".to_string(), // not expected in digests
            };
            format!("{}. [{}] {}", i + 1, tag, item.summary())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, PartialEq)]
pub struct ParsedPlan {
    /// 1-based digest indexes, a permutation of 1..=n.
    pub order: Vec<usize>,
    pub rationale: String,
}

/// Strict parser for the model's reply. Anything that is not a permutation
/// of 1..=n with a non-empty rationale is an error — the caller may retry
/// once and must otherwise fail the synthesis.
pub fn parse_plan(reply: &str, n_items: usize) -> Result<ParsedPlan> {
    let order_line = reply
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("ORDER:").or_else(|| l.strip_prefix("Order:")))
        .context("reply has no ORDER: line")?;

    // Small local models sometimes echo digest text after the numbers
    // ("ORDER: 1. [event at 18:00 UTC] …"). Keep only tokens that parse
    // fully as integers — safety is unchanged because the permutation check
    // below still requires exactly 1..=n, each exactly once.
    let order: Vec<usize> = order_line
        .split([',', ' '])
        .map(|t| t.trim().trim_end_matches(['.', ')', ':']))
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect();
    ensure!(
        order.len() == n_items,
        "ORDER must list all {n_items} items exactly once, got {} entries",
        order.len()
    );
    let mut seen = vec![false; n_items];
    for &idx in &order {
        ensure!((1..=n_items).contains(&idx), "ORDER references item {idx}, which does not exist");
        ensure!(!seen[idx - 1], "ORDER lists item {idx} more than once");
        seen[idx - 1] = true;
    }

    let rationale = match reply.split_once("RATIONALE:").or_else(|| reply.split_once("Rationale:")) {
        Some((_, r)) => r.trim().to_string(),
        None => bail!("reply has no RATIONALE: line"),
    };
    ensure!(!rationale.is_empty(), "RATIONALE is empty");

    Ok(ParsedPlan { order, rationale })
}

/// Assemble the briefing from OUR items in the model's chosen order —
/// provenance is copied from the extracted items, never model-generated.
pub fn briefing_from_plan(items: &[ExtractedItem], plan: ParsedPlan) -> Result<Briefing> {
    let mut sequence = Vec::with_capacity(plan.order.len());
    for idx in plan.order {
        let item = items.get(idx - 1).context("plan index out of range")?;
        sequence.push(PlannedItem {
            provenance: item.provenance().clone(),
            kind: item.kind(),
            summary: item.summary().to_string(),
            occurred_at: item.occurred_at(),
        });
    }
    Ok(Briefing { sequence, rationale: plan.rationale })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plan_accepts_a_valid_reply() {
        let reply = "ORDER: 2, 1, 3\nRATIONALE: Meetings first, then the follow-ups.";
        let plan = parse_plan(reply, 3).unwrap();
        assert_eq!(plan.order, vec![2, 1, 3]);
        assert!(plan.rationale.starts_with("Meetings"));
    }

    #[test]
    fn parse_plan_rejects_duplicates_missing_and_invented_items() {
        assert!(parse_plan("ORDER: 1, 1, 2\nRATIONALE: x", 3).is_err(), "duplicate");
        assert!(parse_plan("ORDER: 1, 2\nRATIONALE: x", 3).is_err(), "missing item");
        assert!(parse_plan("ORDER: 1, 2, 4\nRATIONALE: x", 3).is_err(), "invented item");
        assert!(parse_plan("ORDER: 1, 2, 3", 3).is_err(), "no rationale");
        assert!(parse_plan("RATIONALE: x", 3).is_err(), "no order");
    }

    #[test]
    fn parse_plan_survives_digest_echo_without_weakening_the_permutation_check() {
        // Seen live: single-item day, model echoed the digest line.
        let echo = "ORDER: 1. [event at 18:00 UTC] Interview loop\nRATIONALE: Only one item today.";
        assert_eq!(parse_plan(echo, 1).unwrap().order, vec![1]);

        // Echoed prose still cannot smuggle in a bad plan.
        let bad = "ORDER: 1. [event at 18:00 UTC] x\nRATIONALE: y";
        assert!(parse_plan(bad, 2).is_err(), "echo with missing item still rejected");
    }
}
