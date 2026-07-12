//! Google OAuth (Desktop / installed-app PKCE loopback flow) and the Gmail +
//! Calendar capture calls. Responses are handled as generic JSON only.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use chrono::TimeZone;
use serde_json::Value;
use sha2::{Digest, Sha256};

const SCOPES: &str = "https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/calendar.readonly";

/// (client_id, client_secret, auth_uri, token_uri) from the Desktop client
/// JSON at GOOGLE_CREDENTIALS_PATH. Read as generic JSON — no typed models.
fn installed_client() -> Result<(String, String, String, String)> {
    let path = crate::config::required("GOOGLE_CREDENTIALS_PATH")?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    let v: Value = serde_json::from_str(&raw)?;
    let installed = v
        .get("installed")
        .context("credentials.json has no 'installed' key — need a Desktop OAuth client")?;
    let get = |k: &str| -> Result<String> {
        Ok(installed
            .get(k)
            .and_then(Value::as_str)
            .with_context(|| format!("credentials.json missing installed.{k}"))?
            .to_string())
    };
    Ok((get("client_id")?, get("client_secret")?, get("auth_uri")?, get("token_uri")?))
}

fn random_urlsafe(n: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub async fn authorize() -> Result<()> {
    let (client_id, client_secret, auth_uri, token_uri) = installed_client()?;

    // Loopback redirect on an ephemeral port — always permitted for Desktop
    // (installed) OAuth clients.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/");

    let verifier = random_urlsafe(32);
    let challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(16);

    let mut url = url::Url::parse(&auth_uri)?;
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("state", &state);

    println!("Opening browser for Google consent…");
    println!("If no browser opens, visit:\n{url}");
    let _ = webbrowser::open(url.as_str());

    let params = crate::loopback::wait_for_redirect(vec![listener], "/", 300).await?;
    if params.get("state").map(String::as_str) != Some(state.as_str()) {
        bail!("OAuth state mismatch — aborting");
    }
    if let Some(err) = params.get("error") {
        bail!("Google authorization was denied or failed: {err}");
    }
    let code = params.get("code").context("redirect had no ?code")?;

    let resp = reqwest::Client::new()
        .post(&token_uri)
        .form(&[
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri.as_str()),
        ])
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        // Error bodies carry no tokens; safe to surface.
        bail!("Google token exchange failed ({status}): {body}");
    }
    let mut token: Value = serde_json::from_str(&body)?;
    token["obtained_at_unix"] = chrono::Utc::now().timestamp().into();
    let path = crate::store::save_token("GOOGLE_TOKEN_PATH", &token)?;
    println!(
        "Google token stored (DPAPI-encrypted) at {}\nGranted scopes: {}",
        path.display(),
        token.get("scope").and_then(Value::as_str).unwrap_or("(none reported)")
    );
    Ok(())
}

/// Current access token, refreshing when near expiry. Delegates to the SINGLE
/// implementation in almanac-core (audit F-20: this used to be a second,
/// drifting copy of the refresh logic).
async fn access_token() -> Result<String> {
    almanac_core::auth::GoogleAuth::from_env()?.access_token().await
}

async fn get_json(url: url::Url, bearer: &str) -> Result<String> {
    let resp = reqwest::Client::new().get(url.clone()).bearer_auth(bearer).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        // Google error bodies carry no tokens. 403 insufficient scope shows here.
        bail!("GET {} failed ({status}): {body}", url.path());
    }
    Ok(body)
}

pub async fn capture_gmail() -> Result<()> {
    let tok = access_token().await?;

    let list_url =
        url::Url::parse("https://gmail.googleapis.com/gmail/v1/users/me/messages?maxResults=25")?;
    let list_body = get_json(list_url, &tok).await?;
    let list = crate::store::save_fixture("gmail", "messages_list.json", &list_body)?;

    let first_id = list
        .get("messages")
        .and_then(|m| m.get(0))
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .context("mailbox returned no messages to GET")?;
    let msg_url = url::Url::parse(&format!(
        "https://gmail.googleapis.com/gmail/v1/users/me/messages/{first_id}?format=full"
    ))?;
    let msg_body = get_json(msg_url, &tok).await?;
    crate::store::save_fixture("gmail", "message_get_full.json", &msg_body)?;

    let count = list.get("messages").and_then(Value::as_array).map_or(0, Vec::len);
    println!("gmail: captured messages_list ({count} refs) + 1 full message");
    Ok(())
}

async fn fetch_day(tok: &str, day_start: chrono::DateTime<chrono::Local>) -> Result<String> {
    let day_end = day_start + chrono::Duration::days(1);
    let mut url =
        url::Url::parse("https://www.googleapis.com/calendar/v3/calendars/primary/events")?;
    url.query_pairs_mut()
        .append_pair("timeMin", &day_start.to_rfc3339())
        .append_pair("timeMax", &day_end.to_rfc3339())
        .append_pair("singleEvents", "true")
        .append_pair("maxResults", "50");
    get_json(url, tok).await
}

pub async fn capture_gcal() -> Result<()> {
    let tok = access_token().await?;

    let today = chrono::Local
        .from_local_datetime(&chrono::Local::now().date_naive().and_hms_opt(0, 0, 0).unwrap())
        .single()
        .context("could not construct local midnight")?;

    // A day with zero events makes a useless primary fixture (no event object
    // shape). Scan today, then ±1, ±2, … ±14 days, and keep the first
    // single-day response that contains events. The genuine empty-day
    // response is preserved as its own fixture.
    let mut offsets = vec![0i64];
    for d in 1..=14 {
        offsets.push(d);
        offsets.push(-d);
    }
    for offset in offsets {
        let day_start = today + chrono::Duration::days(offset);
        let body = fetch_day(&tok, day_start).await?;
        let parsed: Value = serde_json::from_str(&body)?;
        let count = parsed.get("items").and_then(Value::as_array).map_or(0, Vec::len);
        if count > 0 {
            crate::store::save_fixture("gcal", "events_list_day.json", &body)?;
            println!("gcal: captured events_list_day ({count} events for {})", day_start.date_naive());
            return Ok(());
        }
        if offset == 0 {
            crate::store::save_fixture("gcal", "events_list_day_empty.json", &body)?;
            println!("gcal: today ({}) has 0 events — saved as events_list_day_empty.json, scanning nearby days…", day_start.date_naive());
        }
    }
    bail!(
        "no calendar events found within ±14 days — add one event to the primary calendar and re-run `capture gcal`"
    );
}
