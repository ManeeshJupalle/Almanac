//! Structure-preserving PII redaction for committed fixtures.
//!
//! Rules NEVER add, remove, or rename keys, never change array lengths, and
//! never change a value's JSON type — they only replace the *values* of
//! PII-bearing string fields with placeholders. IDs, timestamps, cursors,
//! etags, and enum-ish fields are kept verbatim because their formats are a
//! Phase 1 deliverable (see PAYLOAD_CORRECTIONS.md).

use base64::Engine as _;
use serde_json::Value;

pub fn redact(source: &str, v: &mut Value) {
    match source {
        "gmail" => walk(v, &gmail_rule),
        "gcal" => walk(v, &gcal_rule),
        "slack" => walk(v, &slack_rule),
        _ => {}
    }
    scrub_residual_emails(v, "");
}

fn walk(v: &mut Value, rule: &dyn Fn(&str, &mut Value)) {
    match v {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                rule(key, val);
                walk(val, rule);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                walk(item, rule);
            }
        }
        _ => {}
    }
}

fn set_str(v: &mut Value, s: &str) {
    if v.is_string() {
        *v = Value::String(s.to_string());
    }
}

// ---------------------------------------------------------------- gmail ----

fn gmail_rule(key: &str, val: &mut Value) {
    match key {
        // headers: [{name, value}] — keep names (structure), redact values
        // except a small allowlist of structural/format-bearing headers.
        "headers" => {
            if let Value::Array(headers) = val {
                for h in headers.iter_mut() {
                    let name = h
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if let Some(value) = h.get_mut("value") {
                        match name.as_str() {
                            "date" | "content-type" | "mime-version"
                            | "content-transfer-encoding" => {} // keep: format-bearing, no PII
                            "from" | "to" | "cc" | "bcc" | "reply-to" | "sender"
                            | "delivered-to" | "return-path" | "x-original-to" => {
                                set_str(value, "Redacted Sender <redacted@example.com>")
                            }
                            "subject" => set_str(value, "[redacted subject]"),
                            "message-id" | "in-reply-to" | "references" => {
                                set_str(value, "<redacted@mail.example.com>")
                            }
                            _ => set_str(value, "[redacted]"),
                        }
                    }
                }
            }
        }
        "snippet" => set_str(val, "[redacted]"),
        // body data stays valid base64url so decoding code paths still work
        "data" | "raw" => {
            if val.is_string() {
                *val = Value::String(
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("[redacted]"),
                );
            }
        }
        "filename" => {
            if val.as_str().is_some_and(|s| !s.is_empty()) {
                set_str(val, "redacted.bin");
            }
        }
        _ => {}
    }
}

// ----------------------------------------------------------------- gcal ----

fn gcal_rule(key: &str, val: &mut Value) {
    match key {
        "summary" | "description" | "location" => set_str(val, "[redacted]"),
        "email" => set_str(val, "redacted@example.com"),
        "displayName" => set_str(val, "Redacted Name"),
        "htmlLink" => {
            if let Some(s) = val.as_str() {
                if let Some(i) = s.find("eid=") {
                    let redacted = format!("{}REDACTED", &s[..i + 4]);
                    *val = Value::String(redacted);
                }
            }
        }
        "hangoutLink" => set_str(val, "https://meet.google.com/redacted"),
        "uri" => set_str(val, "https://redacted.example.com"),
        "label" | "meetingCode" | "passcode" | "pin" => set_str(val, "[redacted]"),
        "conferenceId" => set_str(val, "redacted"),
        _ => {}
    }
}

// ---------------------------------------------------------------- slack ----

fn slack_rule(key: &str, val: &mut Value) {
    match key {
        "name" | "name_normalized" => set_str(val, "redacted-channel"),
        "previous_names" => {
            if let Value::Array(names) = val {
                for n in names.iter_mut() {
                    set_str(n, "redacted");
                }
            }
        }
        // topic/purpose bodies, message text, block/button values
        "value" | "text" | "fallback" | "pretext" | "title" => set_str(val, "[redacted]"),
        "username" | "real_name" | "display_name" => set_str(val, "redacted"),
        _ => {
            if val
                .as_str()
                .is_some_and(|s| s.starts_with("https://files.slack.com/"))
            {
                set_str(val, "https://redacted.example.com/file");
            }
        }
    }
}

// ------------------------------------------------------------ safety net ----

/// Last pass: replace any e-mail-looking substring that survived the field
/// rules. Skips keys whose values are format-bearing identifiers.
fn scrub_residual_emails(v: &mut Value, key: &str) {
    const KEEP_KEYS: &[&str] = &["data", "raw", "iCalUID", "next_cursor", "cursor"];
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                scrub_residual_emails(val, k);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                scrub_residual_emails(item, key);
            }
        }
        Value::String(s) => {
            if !KEEP_KEYS.contains(&key) {
                let re = email_regex();
                if re.is_match(s) {
                    *s = re.replace_all(s, "redacted@example.com").into_owned();
                }
            }
        }
        _ => {}
    }
}

fn email_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").unwrap()
    })
}
