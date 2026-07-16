//! Jira Cloud adapter — modeled against fixtures/jira/*, not docs.
//! Corrections handled here (PAYLOAD_CORRECTIONS.md): J1, J2, J9, J10, J11.
//!
//! Read path only: issues become `SourceObject`s (source = jira) that feed the
//! same extraction pipeline as every other source. No write access here —
//! transitions/comments live in `act::executors` (A2).

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::auth::JiraAuth;
use crate::types::{ProvenanceRef, RawContent, SourceId, SourceObject, TimeWindow};

const MAX_PAGES: usize = 5;

pub struct JiraAdapter {
    auth: JiraAuth,
    http: reqwest::Client,
    truncated: std::sync::atomic::AtomicBool,
}

impl JiraAdapter {
    pub fn new(auth: JiraAuth) -> Self {
        Self { auth, http: reqwest::Client::new(), truncated: Default::default() }
    }

    pub fn from_env() -> Result<Self> {
        Ok(Self::new(JiraAuth::from_env()?))
    }

    /// True if the last fetch_window hit the page cap with more to fetch (F-6).
    pub fn was_truncated(&self) -> bool {
        self.truncated.load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn get(&self, url: &str) -> Result<Value> {
        let header = self.auth.basic_auth_header()?;
        let resp = self
            .http
            .get(url)
            .header("authorization", header)
            .header("accept", "application/json")
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            // J4: errors split between errorMessages[] and errors{}; surface both.
            bail!("Jira GET failed ({status}): {}", jira_error(&body));
        }
        serde_json::from_str(&body).context("Jira response was not JSON")
    }
}

#[async_trait]
impl super::SourceAdapter for JiraAdapter {
    fn source_id(&self) -> SourceId {
        SourceId::Jira
    }

    async fn authenticate(&mut self) -> Result<()> {
        self.auth.ensure_authenticated()
    }

    /// Jira API tokens do not rotate like OAuth; validate liveness via /myself
    /// (mirrors Slack's non-refreshing path).
    async fn refresh_token(&mut self) -> Result<()> {
        self.get(&format!("{}/rest/api/3/myself", self.auth.base_url())).await.map(|_| ())
    }

    async fn fetch_window(&self, window: TimeWindow) -> Result<Vec<SourceObject>> {
        let base = self.auth.base_url().to_string();
        // J1: only /search/jql serves results (legacy /search is 410 Gone).
        // Date-only bound keeps the JQL unambiguous across site timezones.
        let since = window.start.format("%Y-%m-%d").to_string();
        let mut jql = format!("updated >= \"{since}\" ORDER BY updated ASC");
        if let Ok(project) = std::env::var("JIRA_PROJECT") {
            if !project.trim().is_empty() {
                jql = format!("project = \"{}\" AND {jql}", project.trim());
            }
        }

        let mut objects = Vec::new();
        let mut page_token: Option<String> = None;
        let mut hit_cap_with_more = false;
        for page in 0..MAX_PAGES {
            let mut url = url::Url::parse(&format!("{base}/rest/api/3/search/jql"))?;
            url.query_pairs_mut()
                .append_pair("jql", &jql)
                .append_pair("maxResults", "100")
                .append_pair(
                    "fields",
                    // `comment` (Phase 2.2): correlation reads issue comments for
                    // the explicit-link signal, excluding Almanac's own (J15).
                    "summary,status,description,updated,created,issuetype,priority,assignee,reporter,comment",
                );
            if let Some(token) = &page_token {
                url.query_pairs_mut().append_pair("nextPageToken", token);
            }
            let list = self.get(url.as_str()).await?;

            for issue in list.get("issues").and_then(Value::as_array).into_iter().flatten() {
                objects.push(issue_to_source_object(&base, issue)?);
            }

            // J2: terminate on isLast; carry the opaque nextPageToken verbatim.
            let is_last = list.get("isLast").and_then(Value::as_bool).unwrap_or(true);
            page_token = list.get("nextPageToken").and_then(Value::as_str).map(String::from);
            if is_last || page_token.is_none() {
                break;
            }
            if page == MAX_PAGES - 1 {
                hit_cap_with_more = true;
            }
        }
        self.truncated.store(hit_cap_with_more, std::sync::atomic::Ordering::Relaxed);
        Ok(objects)
    }
}

// ------------------------------------------------------ parse functions ----

/// One issue → SourceObject. J10: `key` (ALM-1) is the stable native id and
/// the deep-link anchor; the numeric `id` is not used for either.
pub fn issue_to_source_object(base_url: &str, issue: &Value) -> Result<SourceObject> {
    let key = issue
        .get("key")
        .and_then(Value::as_str)
        .context("jira issue missing key")?
        .to_string();
    let occurred_at = issue
        .pointer("/fields/updated")
        .and_then(Value::as_str)
        .or_else(|| issue.pointer("/fields/created").and_then(Value::as_str))
        .map(jira_time_to_utc)
        .transpose()?
        .unwrap_or_else(Utc::now);

    Ok(SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Jira,
            native_id: key.clone(),
            deep_link: deep_link(base_url, &key),
        },
        raw: RawContent::new(issue.clone()),
        occurred_at,
    })
}

/// Real Jira issue deep link: `{site}/browse/ALM-1`.
pub fn deep_link(base_url: &str, key: &str) -> String {
    format!("{}/browse/{key}", base_url.trim_end_matches('/'))
}

/// One workflow transition currently offered for an issue.
#[derive(Debug, Clone)]
pub struct TransitionOption {
    pub id: String,
    pub name: String,
    pub to_name: String,
    /// J8: a screen-required transition needs extra field data — Phase 2.1
    /// only proposes `has_screen == false`.
    pub has_screen: bool,
}

/// Live GET of the transitions offered for `issue_key` (read path). Shared by
/// the executor's J6 re-validation and the dev seed, so both see the same set.
pub async fn fetch_transitions(
    auth: &JiraAuth,
    issue_key: &str,
) -> Result<Vec<TransitionOption>> {
    let header = auth.basic_auth_header()?;
    let url = format!("{}/rest/api/3/issue/{issue_key}/transitions", auth.base_url());
    let resp = reqwest::Client::new()
        .get(&url)
        .header("authorization", header)
        .header("accept", "application/json")
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("GET transitions failed ({status}): {}", jira_error(&body));
    }
    let v: Value = serde_json::from_str(&body)?;
    Ok(v.get("transitions")
        .and_then(Value::as_array)
        .map(|ts| {
            ts.iter()
                .filter_map(|t| {
                    Some(TransitionOption {
                        id: t.get("id").and_then(Value::as_str)?.to_string(),
                        name: t.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        to_name: t
                            .pointer("/to/name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        has_screen: t.get("hasScreen").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// GET `/myself` → the token account's `accountId`. Used by the correlation
/// engine to exclude Almanac's OWN Jira comments from the evidence signal (J15).
/// A read GET — no write. Best-effort at correlate time; the engine also works
/// with `None` (it just cannot self-exclude).
pub async fn fetch_self_account_id(auth: &JiraAuth) -> Result<String> {
    let header = auth.basic_auth_header()?;
    let url = format!("{}/rest/api/3/myself", auth.base_url());
    let resp = reqwest::Client::new()
        .get(&url)
        .header("authorization", header)
        .header("accept", "application/json")
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("GET /myself failed ({status}): {}", jira_error(&body));
    }
    serde_json::from_str::<Value>(&body)?
        .get("accountId")
        .and_then(Value::as_str)
        .map(String::from)
        .context("/myself response missing accountId")
}

/// J9: Jira timestamps are `2026-07-14T22:26:03.672-0500` — millis + an offset
/// WITHOUT a colon, which is NOT valid RFC 3339. Parse with an explicit format
/// (`%z` accepts `-0500`); never `parse_from_rfc3339` (it rejects this).
pub fn jira_time_to_utc(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%z")
        .with_context(|| format!("unparseable jira timestamp: {s}"))?
        .with_timezone(&Utc))
}

/// J4: collect a human error from BOTH shapes — `errorMessages[]` (array) and
/// every value of `errors{}` (object) — since either alone can be empty.
pub fn jira_error(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.chars().take(200).collect();
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(msgs) = v.get("errorMessages").and_then(Value::as_array) {
        parts.extend(msgs.iter().filter_map(|m| m.as_str().map(String::from)));
    }
    if let Some(errs) = v.get("errors").and_then(Value::as_object) {
        parts.extend(errs.iter().map(|(k, val)| {
            format!("{k}: {}", val.as_str().unwrap_or_default())
        }));
    }
    if parts.is_empty() {
        body.chars().take(200).collect()
    } else {
        parts.join("; ")
    }
}
