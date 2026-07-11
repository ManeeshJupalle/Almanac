//! High-precision rules — the "rule" half of the hybrid classifier.
//! A rule verdict short-circuits the embedding path; when no rule fires the
//! item flows to the semantic classifier. Every hit is recorded in
//! ExtractionSignals for auditability.

use serde_json::Value;

use super::ItemKind;
use crate::adapters::gmail::header_values;
use crate::types::SourceId;

/// Returns (verdict, hits). `verdict == None` means "rules are not sure".
pub fn apply(source: SourceId, raw: &Value, text: &str) -> (Option<ItemKind>, Vec<String>) {
    let mut hits = Vec::new();
    let verdict = match source {
        SourceId::GoogleCalendar => gcal_rules(raw, &mut hits),
        SourceId::Slack => slack_rules(raw, text, &mut hits),
        SourceId::Gmail => gmail_rules(raw, text, &mut hits),
    };
    (verdict, hits)
}

fn gcal_rules(raw: &Value, hits: &mut Vec<String>) -> Option<ItemKind> {
    if raw.get("status").and_then(Value::as_str) == Some("cancelled") {
        hits.push("gcal:cancelled".into());
        return Some(ItemKind::Noise);
    }
    // A calendar item in the window IS an event — the strongest rule we have.
    hits.push("gcal:event".into());
    Some(ItemKind::Event)
}

fn slack_rules(raw: &Value, text: &str, hits: &mut Vec<String>) -> Option<ItemKind> {
    // S8: system messages carry a subtype (channel_join, channel_topic, …).
    if let Some(subtype) = raw.get("subtype").and_then(Value::as_str) {
        hits.push(format!("slack:subtype:{subtype}"));
        return Some(ItemKind::Noise);
    }
    if raw.get("bot_id").and_then(Value::as_str).is_some() {
        hits.push("slack:bot_message".into());
        return Some(ItemKind::Noise);
    }
    text_pattern_rules(text, hits)
}

fn gmail_rules(raw: &Value, text: &str, hits: &mut Vec<String>) -> Option<ItemKind> {
    let payload = raw.get("payload").cloned().unwrap_or(Value::Null);

    // Bulk/automated mail markers — strong Noise signals. These win over
    // text patterns: a newsletter asking "can you spare 2 minutes?" is noise.
    if !header_values(&payload, "List-Unsubscribe").is_empty() {
        hits.push("gmail:list-unsubscribe".into());
        return Some(ItemKind::Noise);
    }
    if header_values(&payload, "Precedence")
        .iter()
        .any(|v| v.eq_ignore_ascii_case("bulk") || v.eq_ignore_ascii_case("list"))
    {
        hits.push("gmail:precedence-bulk".into());
        return Some(ItemKind::Noise);
    }
    if !header_values(&payload, "Auto-Submitted").is_empty() {
        hits.push("gmail:auto-submitted".into());
        return Some(ItemKind::Noise);
    }
    // G10: CATEGORY_* system labels. Promotions/social/forums are reliably
    // bulk; CATEGORY_UPDATES alone is only a weak hint (bills and alerts land
    // there), so it is recorded but does not decide.
    if let Some(labels) = raw.get("labelIds").and_then(Value::as_array) {
        let has = |l: &str| labels.iter().any(|v| v.as_str() == Some(l));
        if has("CATEGORY_PROMOTIONS") || has("CATEGORY_SOCIAL") || has("CATEGORY_FORUMS") {
            hits.push("gmail:category-bulk".into());
            return Some(ItemKind::Noise);
        }
        if has("CATEGORY_UPDATES") {
            hits.push("gmail:category-updates(weak)".into());
        }
    }

    text_pattern_rules(text, hits)
}

/// Explicit, high-precision text patterns shared by Gmail and Slack.
fn text_pattern_rules(text: &str, hits: &mut Vec<String>) -> Option<ItemKind> {
    if action_pattern().is_match(text) {
        hits.push("text:action-pattern".into());
        return Some(ItemKind::ActionNeeded);
    }
    if commitment_pattern().is_match(text) {
        hits.push("text:commitment-pattern".into());
        return Some(ItemKind::Commitment);
    }
    None
}

fn action_pattern() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(action required|please (review|approve|confirm|respond|reply|rsvp|sign|complete|update|send)|can you\b|could you\b|would you mind|need(s)? your (approval|review|input|feedback|sign-?off)|waiting on you|due (by|on|today|tomorrow)|deadline is|by (eod|end of day|cob))\b",
        )
        .expect("action regex compiles")
    })
}

fn commitment_pattern() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(i'?ll (send|get|have|take|handle|review|follow|do|share|schedule|set up|update|finish|deliver)|i will (send|get|have|take|handle|review|follow|do|share|schedule|set up|update|finish|deliver)|will do\b|consider it done|i'?m on it|on it!|i commit to)\b",
        )
        .expect("commitment regex compiles")
    })
}
