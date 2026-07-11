//! Gmail adapter — modeled against fixtures/gmail/*, not docs.
//! Corrections handled here (PAYLOAD_CORRECTIONS.md): G1, G2, G4, G6–G11.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::auth::GoogleAuth;
use crate::types::{ProvenanceRef, RawContent, SourceId, SourceObject, TimeWindow};

/// Pages of message refs to follow at most per window (25 refs per page).
const MAX_PAGES: usize = 4;

pub struct GmailAdapter {
    auth: GoogleAuth,
    http: reqwest::Client,
}

impl GmailAdapter {
    pub fn new(auth: GoogleAuth) -> Self {
        Self { auth, http: reqwest::Client::new() }
    }

    pub fn from_env() -> Result<Self> {
        Ok(Self::new(GoogleAuth::from_env()?))
    }

    async fn get(&self, url: url::Url) -> Result<Value> {
        let token = self.auth.access_token().await?;
        let resp = self.http.get(url.clone()).bearer_auth(token).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("GET {} failed ({status})", url.path());
        }
        Ok(serde_json::from_str(&body)?)
    }
}

#[async_trait]
impl super::SourceAdapter for GmailAdapter {
    fn source_id(&self) -> SourceId {
        SourceId::Gmail
    }

    async fn authenticate(&mut self) -> Result<()> {
        self.auth.ensure_authenticated()
    }

    async fn refresh_token(&mut self) -> Result<()> {
        self.auth.force_refresh().await.map(|_| ())
    }

    async fn fetch_window(&self, window: TimeWindow) -> Result<Vec<SourceObject>> {
        // Gmail search operators take epoch seconds.
        let query = format!("after:{} before:{}", window.start.timestamp(), window.end.timestamp());
        let mut objects = Vec::new();
        // G2: nextPageToken is an opaque STRING (real value has a leading
        // zero) — carried verbatim, never parsed.
        let mut page_token: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let mut url =
                url::Url::parse("https://gmail.googleapis.com/gmail/v1/users/me/messages")?;
            url.query_pairs_mut()
                .append_pair("q", &query)
                .append_pair("maxResults", "25");
            if let Some(token) = &page_token {
                url.query_pairs_mut().append_pair("pageToken", token);
            }
            let list = self.get(url).await?;

            // G1: list refs carry only {id, threadId}.
            let refs = list.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
            for msg_ref in &refs {
                let id = msg_ref
                    .get("id")
                    .and_then(Value::as_str)
                    .context("message ref missing id")?;
                let msg_url = url::Url::parse(&format!(
                    "https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}?format=full"
                ))?;
                let full = self.get(msg_url).await?;
                objects.push(message_to_source_object(&full)?);
            }

            page_token = list.get("nextPageToken").and_then(Value::as_str).map(String::from);
            if page_token.is_none() {
                break;
            }
        }
        Ok(objects)
    }
}

// ------------------------------------------------------ parse functions ----
// Pure and fixture-testable; the live path above uses exactly these.

/// Full message JSON (format=full) → SourceObject.
pub fn message_to_source_object(msg: &Value) -> Result<SourceObject> {
    let id = msg
        .get("id")
        .and_then(Value::as_str)
        .context("gmail message missing id")?
        .to_string();
    // G4: internalDate is a JSON string of epoch MILLISECONDS.
    let internal_ms: i64 = msg
        .get("internalDate")
        .and_then(Value::as_str)
        .context("gmail message missing internalDate (need format=full)")?
        .parse()
        .context("internalDate was not an integer string")?;
    let occurred_at: DateTime<Utc> = DateTime::from_timestamp_millis(internal_ms)
        .context("internalDate out of range")?;

    Ok(SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Gmail,
            native_id: id.clone(),
            deep_link: deep_link(&id),
        },
        raw: RawContent::new(msg.clone()),
        occurred_at,
    })
}

/// Real Gmail web-UI link, constructable from the message id.
pub fn deep_link(message_id: &str) -> String {
    format!("https://mail.google.com/mail/#all/{message_id}")
}

/// G7 + G8: headers are a MULTISET with wire-cased names — return ALL values
/// for a name, matched case-insensitively.
pub fn header_values<'v>(payload: &'v Value, name: &str) -> Vec<&'v str> {
    payload
        .get("headers")
        .and_then(Value::as_array)
        .map(|headers| {
            headers
                .iter()
                .filter(|h| {
                    h.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.eq_ignore_ascii_case(name))
                })
                .filter_map(|h| h.get("value").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

/// G9: message body `data` is base64url (URL_SAFE alphabet), not standard
/// base64. Gmail pads inconsistently, so decode ignoring padding.
pub fn decode_body_data(data: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(data.trim_end_matches('='))
        .context("body data was not valid base64url")
}

/// G6 + multipart: collect (mimeType, decoded bytes) for every part that has
/// body.data, walking nested parts. Root body may be `{"size": 0}` with no
/// data key at all — that is a normal multipart shape, not an error.
pub fn collect_body_parts(payload: &Value) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    collect_into(payload, &mut out);
    out
}

fn collect_into(part: &Value, out: &mut Vec<(String, Vec<u8>)>) {
    let mime = part.get("mimeType").and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(data) = part.pointer("/body/data").and_then(Value::as_str) {
        if let Ok(bytes) = decode_body_data(data) {
            out.push((mime, bytes));
        }
    }
    if let Some(parts) = part.get("parts").and_then(Value::as_array) {
        for p in parts {
            collect_into(p, out);
        }
    }
}
