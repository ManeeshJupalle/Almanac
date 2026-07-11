//! Google Calendar adapter — modeled against fixtures/gcal/*, not docs.
//! Corrections handled here (PAYLOAD_CORRECTIONS.md): C1, C4, C5, C6, C10, C12.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;

use crate::auth::GoogleAuth;
use crate::types::{ProvenanceRef, RawContent, SourceId, SourceObject, TimeWindow};

const MAX_PAGES: usize = 4;

pub struct CalendarAdapter {
    auth: GoogleAuth,
    http: reqwest::Client,
}

impl CalendarAdapter {
    pub fn new(auth: GoogleAuth) -> Self {
        Self { auth, http: reqwest::Client::new() }
    }

    pub fn from_env() -> Result<Self> {
        Ok(Self::new(GoogleAuth::from_env()?))
    }
}

#[async_trait]
impl super::SourceAdapter for CalendarAdapter {
    fn source_id(&self) -> SourceId {
        SourceId::GoogleCalendar
    }

    async fn authenticate(&mut self) -> Result<()> {
        self.auth.ensure_authenticated()
    }

    async fn refresh_token(&mut self) -> Result<()> {
        self.auth.force_refresh().await.map(|_| ())
    }

    async fn fetch_window(&self, window: TimeWindow) -> Result<Vec<SourceObject>> {
        let mut objects = Vec::new();
        let mut page_token: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let mut url = url::Url::parse(
                "https://www.googleapis.com/calendar/v3/calendars/primary/events",
            )?;
            url.query_pairs_mut()
                .append_pair("timeMin", &window.start.to_rfc3339())
                .append_pair("timeMax", &window.end.to_rfc3339())
                .append_pair("singleEvents", "true")
                .append_pair("maxResults", "50");
            if let Some(token) = &page_token {
                url.query_pairs_mut().append_pair("pageToken", token);
            }

            let token = self.auth.access_token().await?;
            let resp = self.http.get(url.clone()).bearer_auth(token).send().await?;
            let status = resp.status();
            let body = resp.text().await?;
            if !status.is_success() {
                bail!("GET {} failed ({status})", url.path());
            }
            let list: Value = serde_json::from_str(&body)?;

            // C1: the response's top-level `summary` is the account's e-mail
            // address (PII). It is deliberately never read, logged, or copied
            // out of the raw payload here — only `items` is consumed.
            objects.extend(events_to_source_objects(&list)?);

            // C4: nextPageToken and nextSyncToken are mutually exclusive; the
            // final page carries only nextSyncToken. Absent page token = done.
            page_token = list.get("nextPageToken").and_then(Value::as_str).map(String::from);
            if page_token.is_none() {
                break;
            }
        }
        Ok(objects)
    }
}

// ------------------------------------------------------ parse functions ----

/// events.list response → SourceObjects (consumes `items` only; see C1).
pub fn events_to_source_objects(list: &Value) -> Result<Vec<SourceObject>> {
    // C10/C12: unknown fields (extendedProperties.shared vendor keys, absent
    // optional fields) never error — items are consumed as generic JSON.
    list.get("items")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(event_to_source_object).collect())
        .unwrap_or_else(|| Ok(Vec::new()))
}

pub fn event_to_source_object(event: &Value) -> Result<SourceObject> {
    let id = event
        .get("id")
        .and_then(Value::as_str)
        .context("calendar event missing id")?
        .to_string();
    // htmlLink is the real deep link, present on every event in practice.
    let deep_link = event
        .get("htmlLink")
        .and_then(Value::as_str)
        .context("calendar event missing htmlLink")?
        .to_string();
    let occurred_at = event_start_utc(event)?;

    Ok(SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::GoogleCalendar,
            native_id: id,
            deep_link,
        },
        raw: RawContent::new(event.clone()),
        occurred_at,
    })
}

/// C5 + C6: `start` is a union — timed events carry `dateTime` (RFC3339 with
/// offset OR Z+millis; both flavors coexist), all-day events carry `date`
/// (yyyy-mm-dd). Event timeZone may differ from the calendar's; the offset in
/// `dateTime` is authoritative — never inherit the calendar tz.
pub fn event_start_utc(event: &Value) -> Result<DateTime<Utc>> {
    let start = event.get("start").context("calendar event missing start")?;
    if let Some(dt) = start.get("dateTime").and_then(Value::as_str) {
        return Ok(DateTime::parse_from_rfc3339(dt)
            .with_context(|| format!("unparseable event dateTime: {dt}"))?
            .with_timezone(&Utc));
    }
    if let Some(d) = start.get("date").and_then(Value::as_str) {
        let date: NaiveDate = d.parse().with_context(|| format!("unparseable event date: {d}"))?;
        let midnight = date.and_hms_opt(0, 0, 0).context("invalid midnight")?;
        return Ok(DateTime::from_naive_utc_and_offset(midnight, Utc));
    }
    bail!("calendar event start has neither dateTime nor date")
}
