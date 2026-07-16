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
    // Prefix match so write-path captures (gmail_send/, slack_post/,
    // jira_transition/, jira_comment/) get the same rules as their read source.
    match source {
        s if s.starts_with("gmail") => walk(v, &gmail_rule),
        s if s.starts_with("gcal") => walk(v, &gcal_rule),
        s if s.starts_with("slack") => walk(v, &slack_rule),
        s if s.starts_with("jira") => walk(v, &jira_rule),
        _ => {}
    }
    scrub_residual_emails(v, "");
    // Jira: site hostname + account-id query params can appear in ANY string
    // (self links, avatar URLs, nested arrays) — scrub every leaf, not just
    // keyed values. Harmless for other sources (no atlassian.net hosts there).
    scrub_atlassian_identifiers(v);
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

// ----------------------------------------------------------------- jira ----
//
// Redact: account IDs, display names, e-mail addresses, avatar URLs. Keep as
// STRUCTURE (never blanked) the fields whose shapes are the corrections-table
// deliverable: issue `key`/`id`, `name` (status/transition/priority/issue-type
// names like "In Progress" are enum-ish, NOT PII), transition ids, `created`/
// `updated` timestamps, status categories, ADF node `type`s. ADF leaf `text`
// IS blanked (it can carry issue/comment prose) but the tree shape survives.
// Site hostname + accountId-in-URLs are handled by scrub_atlassian_identifiers.

fn jira_rule(key: &str, val: &mut Value) {
    match key {
        // `author`/`updateAuthor`/`reporter`/`assignee` are user OBJECTS —
        // left to the recursive walk, which redacts their accountId/name/email.
        "accountId" => set_str(val, "redacted-account-id"),
        "emailAddress" => set_str(val, "redacted@example.com"),
        "displayName" => set_str(val, "Redacted User"),
        // avatarUrls is an object keyed by size; blank each URL leaf.
        "16x16" | "24x24" | "32x32" | "48x48" => {
            set_str(val, "https://redacted.example.com/avatar")
        }
        // ADF leaf text (doc → paragraph → text). Keep the tree, blank prose.
        "text" => set_str(val, "[redacted]"),
        _ => {}
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

/// Jira residual pass: in EVERY string leaf (including array elements the
/// field walk skips), replace the site hostname with a placeholder — keeping
/// the URL PATH intact, which is the deliverable (`self`-link and REST-route
/// shapes) — and blank any `accountId=` query-param value. No-op for strings
/// that contain neither, so it is safe across all sources.
fn scrub_atlassian_identifiers(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (_k, val) in map.iter_mut() {
                scrub_atlassian_identifiers(val);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                scrub_atlassian_identifiers(item);
            }
        }
        Value::String(s) => {
            if atlassian_host_regex().is_match(s) {
                *s = atlassian_host_regex()
                    .replace_all(s, "https://redacted.atlassian.net")
                    .into_owned();
            }
            if account_query_regex().is_match(s) {
                *s = account_query_regex()
                    .replace_all(s, "accountId=redacted-account-id")
                    .into_owned();
            }
        }
        _ => {}
    }
}

fn atlassian_host_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"https://[A-Za-z0-9][A-Za-z0-9-]*\.atlassian\.net").unwrap()
    })
}

// ------------------------------------------------------------------ git ----
//
// Git-log fixtures are RAW TEXT, not JSON (records NUL-separated, fields US
// (0x1f)-separated — see almanac_core::git::LOG_FORMAT). The only PII is the
// author identity: name + email in the author fields, and in Co-authored-by /
// Signed-off-by trailers inside message bodies.
//
// We scrub REAL identities but deliberately KEEP reserved-example identities
// verbatim (RFC 2606 / 6761: the `example.{com,net,org}` domains and the
// `.example`/`.test`/`.invalid`/`.localhost` TLDs). The scratch fixture's
// invented authors — including a non-ASCII one — live under `example.test` and
// ARE the payload the parser is tested against, so blanking them would destroy
// the very cases we captured. Real-domain identities (a live capture of the
// Almanac repo carries them) are replaced. Structure-preserving: only identity
// *values* change; delimiters, field counts, and record counts are untouched.

const GIT_FIELD_SEP: char = '\u{1f}';
const GIT_RECORD_SEP: char = '\0';

fn is_reserved_example_email(email: &str) -> bool {
    let domain = email.rsplit('@').next().unwrap_or_default().to_ascii_lowercase();
    matches!(domain.as_str(), "example.com" | "example.net" | "example.org")
        || domain.ends_with(".example")
        || domain.ends_with(".test")
        || domain.ends_with(".invalid")
        || domain.ends_with(".localhost")
}

/// Redact real author identities from a raw `git log -z` blob.
pub fn redact_git(blob: &str) -> String {
    // Collect real (non-example) author names/emails from the identity fields
    // (index 2 = %an, 3 = %ae). splitn keeps the free-text message (last field)
    // from being mistaken for a field.
    let mut real_names: Vec<String> = Vec::new();
    let mut real_emails: Vec<String> = Vec::new();
    for record in blob.split(GIT_RECORD_SEP) {
        let fields: Vec<&str> = record.splitn(7, GIT_FIELD_SEP).collect();
        if fields.len() >= 4 {
            let (name, email) = (fields[2], fields[3]);
            if !email.is_empty() && !is_reserved_example_email(email) {
                if !name.is_empty() && !real_names.iter().any(|n| n == name) {
                    real_names.push(name.to_string());
                }
                if !real_emails.iter().any(|e| e == email) {
                    real_emails.push(email.to_string());
                }
            }
        }
    }
    // Replace longest-first so one identity isn't a substring-clobber of another.
    real_names.sort_by_key(|s| std::cmp::Reverse(s.len()));
    real_emails.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = blob.to_string();
    for email in &real_emails {
        out = out.replace(email.as_str(), "redacted@example.com");
    }
    for name in &real_names {
        out = out.replace(name.as_str(), "Redacted Author");
    }
    // Safety net: any real-domain email that never appeared as an author field
    // (e.g. a trailer address) — but keep reserved-example addresses verbatim.
    email_regex()
        .replace_all(&out, |caps: &regex::Captures| {
            if is_reserved_example_email(&caps[0]) {
                caps[0].to_string()
            } else {
                "redacted@example.com".to_string()
            }
        })
        .into_owned()
}

fn account_query_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#"accountId=[^&"'\s]+"#).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: char = '\u{1f}';

    fn rec(name: &str, email: &str, msg: &str) -> String {
        format!(
            "{sha}{US}{short}{US}{name}{US}{email}{US}2026-07-11T00:00:00-05:00{US}{parents}{US}{msg}",
            sha = "0".repeat(40),
            short = "0000000",
            parents = "",
        )
    }

    #[test]
    fn redact_git_scrubs_real_identities_keeps_example() {
        // A real identity (gmail) + trailer email, plus a reserved-example one.
        let real = rec("Jane Realperson", "jane.real@company.com", "ALM-1 fix\n\nCo-authored-by: Bob <bob@corp.io>\n");
        let fake = rec("Renée Müller", "renee@example.test", "Café ünïcode ☕\n");
        let blob = format!("{real}\0{fake}");
        let out = redact_git(&blob);

        // Real identity gone.
        assert!(!out.contains("Jane Realperson"), "real name scrubbed");
        assert!(!out.contains("jane.real@company.com"), "real email scrubbed");
        assert!(!out.contains("bob@corp.io"), "trailer email scrubbed");
        assert!(out.contains("Redacted Author") && out.contains("redacted@example.com"));

        // Reserved-example identity preserved (it is the deliberate test payload,
        // including the non-ASCII author + message).
        assert!(out.contains("Renée Müller"), "example author kept");
        assert!(out.contains("renee@example.test"), "example email kept");
        assert!(out.contains("Café ünïcode ☕"), "non-ASCII message kept");

        // Structure preserved: still 2 records, field counts unchanged.
        assert_eq!(out.split('\0').count(), 2);
        for r in out.split('\0') {
            assert_eq!(r.splitn(7, US).count(), 7);
        }
    }
}
