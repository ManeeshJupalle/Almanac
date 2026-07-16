//! Per-source derivation of classification text and a short on-device
//! summary from raw payloads. Rule-based only — no model, no synthesis.

use serde_json::Value;

use crate::adapters::gmail::header_values;
use crate::types::SourceId;

pub struct SourceText {
    /// Short human-readable line (local use only).
    pub summary: String,
    /// Text fed to the classifier.
    pub body: String,
}

pub fn classification_text(source: SourceId, raw: &Value) -> SourceText {
    match source {
        SourceId::Gmail => gmail_text(raw),
        SourceId::GoogleCalendar => gcal_text(raw),
        SourceId::Slack => slack_text(raw),
        SourceId::Jira => jira_text(raw),
        // Git commits are evidence, not extracted; provide their message text
        // for completeness (this path isn't taken by the pipeline).
        SourceId::Git => {
            let subject = raw.get("subject").and_then(Value::as_str).unwrap_or("");
            let message = raw.get("message").and_then(Value::as_str).unwrap_or(subject);
            SourceText {
                summary: truncate(subject, 120, "(no subject)"),
                body: message.to_string(),
            }
        }
    }
}

/// A Jira issue's classification text: summary + description prose. The
/// description is ADF; pull its flat text (paragraph/text leaves) for the
/// classifier without dragging the whole tree in.
fn jira_text(raw: &Value) -> SourceText {
    let fields = raw.get("fields").cloned().unwrap_or(Value::Null);
    let summary = fields.get("summary").and_then(Value::as_str).unwrap_or("").to_string();
    let description = adf_plain_text(fields.get("description"));
    SourceText {
        summary: truncate(&summary, 120, "(untitled issue)"),
        body: format!("{summary}\n{description}").trim().to_string(),
    }
}

/// Flatten an ADF document to its concatenated `text` leaves (best-effort).
fn adf_plain_text(adf: Option<&Value>) -> String {
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = map.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                        out.push(' ');
                    }
                }
                if let Some(content) = map.get("content") {
                    walk(content, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    if let Some(v) = adf {
        walk(v, &mut out);
    }
    out.trim().to_string()
}

fn gmail_text(raw: &Value) -> SourceText {
    let payload = raw.get("payload").cloned().unwrap_or(Value::Null);
    let subject = header_values(&payload, "Subject").first().copied().unwrap_or("").to_string();
    let snippet = raw.get("snippet").and_then(Value::as_str).unwrap_or("");
    let summary = if subject.trim().is_empty() {
        truncate(snippet, 120, "(no subject)")
    } else {
        truncate(&subject, 120, "(no subject)")
    };
    SourceText { summary, body: format!("{subject}\n{snippet}").trim().to_string() }
}

fn gcal_text(raw: &Value) -> SourceText {
    let title = raw.get("summary").and_then(Value::as_str).unwrap_or("");
    let description = raw.get("description").and_then(Value::as_str).unwrap_or("");
    let location = raw.get("location").and_then(Value::as_str).unwrap_or("");
    SourceText {
        summary: truncate(title, 120, "(untitled event)"),
        body: format!("{title}\n{description}\n{location}").trim().to_string(),
    }
}

fn slack_text(raw: &Value) -> SourceText {
    let text = raw.get("text").and_then(Value::as_str).unwrap_or("");
    let summary = if let Some(subtype) = raw.get("subtype").and_then(Value::as_str) {
        format!("[system: {subtype}]")
    } else {
        truncate(text, 120, "(empty message)")
    };
    SourceText { summary, body: text.to_string() }
}

/// Char-boundary-safe truncation with a fallback for empty input.
fn truncate(s: &str, max_chars: usize, fallback: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    match trimmed.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", &trimmed[..idx]),
        None => trimmed.to_string(),
    }
}
