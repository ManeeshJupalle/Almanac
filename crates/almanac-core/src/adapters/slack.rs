//! Slack adapter — modeled against fixtures/slack/*, not docs.
//! Corrections handled here (PAYLOAD_CORRECTIONS.md): S1–S4, S7, S8.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::auth::SlackAuth;
use crate::types::{ProvenanceRef, RawContent, SourceId, SourceObject, TimeWindow};

const MAX_PAGES: usize = 4;

pub struct SlackAdapter {
    auth: SlackAuth,
    http: reqwest::Client,
    /// Workspace base URL from auth.test (e.g. https://team.slack.com/),
    /// resolved in authenticate(); needed for real message deep links.
    workspace_url: Option<String>,
}

impl SlackAdapter {
    pub fn new(auth: SlackAuth) -> Self {
        Self { auth, http: reqwest::Client::new(), workspace_url: None }
    }

    pub fn from_env() -> Result<Self> {
        Ok(Self::new(SlackAuth::from_env()?))
    }

    async fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value> {
        let token = self.auth.user_token()?;
        let mut url = url::Url::parse(&format!("https://slack.com/api/{method}"))?;
        for (k, v) in params {
            url.query_pairs_mut().append_pair(k, v);
        }
        let resp = self.http.get(url).bearer_auth(token).send().await?;
        let body = resp.text().await?;
        let v: Value =
            serde_json::from_str(&body).with_context(|| format!("{method} returned non-JSON"))?;
        // Slack signals errors as HTTP 200 + ok=false.
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = v.get("error").and_then(Value::as_str).unwrap_or("unknown_error");
            bail!("{method} returned ok=false: {err}");
        }
        Ok(v)
    }
}

#[async_trait]
impl super::SourceAdapter for SlackAdapter {
    fn source_id(&self) -> SourceId {
        SourceId::Slack
    }

    async fn authenticate(&mut self) -> Result<()> {
        self.auth.ensure_authenticated()?;
        self.workspace_url = Some(self.auth.workspace_url().await?);
        Ok(())
    }

    async fn refresh_token(&mut self) -> Result<()> {
        self.auth.force_refresh().await.map(|_| ())
    }

    async fn fetch_window(&self, window: TimeWindow) -> Result<Vec<SourceObject>> {
        let workspace_url = self
            .workspace_url
            .clone()
            .context("SlackAdapter::authenticate must run before fetch_window")?;

        // 1. All public channels, paginated. S1: conversations.list ends with
        //    next_cursor == "" (empty string), handled by next_cursor().
        let mut channels: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut params = vec![("limit", "100"), ("types", "public_channel")];
            let cursor_val;
            if let Some(c) = &cursor {
                cursor_val = c.clone();
                params.push(("cursor", &cursor_val));
            }
            let list = self.call("conversations.list", &params).await?;
            channels
                .extend(list.get("channels").and_then(Value::as_array).cloned().unwrap_or_default());
            cursor = next_cursor(&list);
            if cursor.is_none() {
                break;
            }
        }

        // 2. History per member channel within the window. Slack's oldest/
        //    latest take ts-style second strings.
        let oldest = format!("{}.000000", window.start.timestamp());
        let latest = format!("{}.000000", window.end.timestamp());
        let mut objects = Vec::new();

        for channel in &channels {
            if channel.get("is_member").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let channel_id = channel
                .get("id")
                .and_then(Value::as_str)
                .context("channel object missing id")?;

            let mut cursor: Option<String> = None;
            for _ in 0..MAX_PAGES {
                let mut params = vec![
                    ("channel", channel_id),
                    ("limit", "100"),
                    ("oldest", oldest.as_str()),
                    ("latest", latest.as_str()),
                ];
                let cursor_val;
                if let Some(c) = &cursor {
                    cursor_val = c.clone();
                    params.push(("cursor", &cursor_val));
                }
                let history = self.call("conversations.history", &params).await?;

                for msg in history.get("messages").and_then(Value::as_array).into_iter().flatten()
                {
                    // S8: system messages (subtype channel_join etc.) are
                    // returned as-is — classification/noise is Phase 3.
                    objects.push(message_to_source_object(&workspace_url, channel_id, msg)?);
                }

                // S2: conversations.history OMITS response_metadata entirely
                // on the last page (unlike list's empty string). has_more is
                // the primary signal; next_cursor() handles both shapes.
                let has_more =
                    history.get("has_more").and_then(Value::as_bool).unwrap_or(false);
                cursor = next_cursor(&history);
                if !has_more || cursor.is_none() {
                    break;
                }
            }
        }
        Ok(objects)
    }
}

// ------------------------------------------------------ parse functions ----

/// S1 + S2: pagination cursor, handling BOTH termination shapes — empty
/// string (conversations.list) and absent response_metadata
/// (conversations.history). Returns None when pagination is finished.
pub fn next_cursor(response: &Value) -> Option<String> {
    response
        .pointer("/response_metadata/next_cursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(String::from)
}

/// Message JSON + channel context → SourceObject.
pub fn message_to_source_object(
    workspace_url: &str,
    channel_id: &str,
    msg: &Value,
) -> Result<SourceObject> {
    // S7: ts is a STRING ("seconds.micros") that doubles as the message's
    // unique ID per channel. Kept verbatim in the native id; parsed to a
    // timestamp WITHOUT going through a float.
    let ts = msg.get("ts").and_then(Value::as_str).context("slack message missing ts")?;
    Ok(SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Slack,
            native_id: format!("{channel_id}:{ts}"),
            deep_link: deep_link(workspace_url, channel_id, ts),
        },
        raw: RawContent::new(msg.clone()),
        occurred_at: ts_to_utc(ts)?,
    })
}

/// Real Slack archives permalink: https://<ws>/archives/<channel>/p<ts-no-dot>
pub fn deep_link(workspace_url: &str, channel_id: &str, ts: &str) -> String {
    let base = workspace_url.trim_end_matches('/');
    let ts_compact = ts.replace('.', "");
    format!("{base}/archives/{channel_id}/p{ts_compact}")
}

/// S7: parse "1783745960.543929" exactly — integer seconds + microseconds,
/// never a float (floats lose precision and uniqueness).
pub fn ts_to_utc(ts: &str) -> Result<DateTime<Utc>> {
    let (secs_str, frac_str) = ts.split_once('.').unwrap_or((ts, "0"));
    let secs: i64 = secs_str.parse().with_context(|| format!("bad ts seconds: {ts}"))?;
    // Fraction is microseconds in practice (6 digits); normalize defensively.
    let micros: u32 = format!("{frac_str:0<6}")[..6]
        .parse()
        .with_context(|| format!("bad ts fraction: {ts}"))?;
    DateTime::from_timestamp(secs, micros * 1000).with_context(|| format!("ts out of range: {ts}"))
}

/// S3: channel `created` is epoch SECONDS…
pub fn channel_created_utc(channel: &Value) -> Result<DateTime<Utc>> {
    let secs = channel
        .get("created")
        .and_then(Value::as_i64)
        .context("channel missing created")?;
    DateTime::from_timestamp(secs, 0).context("channel created out of range")
}

/// …while `updated` in the SAME object is epoch MILLISECONDS. Each converted
/// explicitly; consumers must use these instead of guessing units.
pub fn channel_updated_utc(channel: &Value) -> Result<DateTime<Utc>> {
    let millis = channel
        .get("updated")
        .and_then(Value::as_i64)
        .context("channel missing updated")?;
    DateTime::from_timestamp_millis(millis).context("channel updated out of range")
}
