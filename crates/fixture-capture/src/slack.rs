//! Slack OAuth v2 (user token) and the conversations capture calls.
//! Responses are handled as generic JSON only.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::Value;

// Phase 2.0 adds the WRITE user scope chat:write (executors only — see
// almanac-core::act::executors for the A2 discipline). The scope must ALSO be
// added under "User Token Scopes" on the Slack app config (api.slack.com →
// OAuth & Permissions), then the app reinstalled, then `slack-auth` re-run.
const USER_SCOPES: &str = "channels:read,channels:history,chat:write";

pub async fn authorize() -> Result<()> {
    let client_id = crate::config::required("SLACK_CLIENT_ID")?;
    let client_secret = crate::config::required("SLACK_CLIENT_SECRET")?;
    let redirect_uri = crate::config::required("SLACK_REDIRECT_URI")?;

    let parsed = url::Url::parse(&redirect_uri)?;
    let port = parsed.port().context("SLACK_REDIRECT_URI must include a port")?;
    let path = parsed.path().to_string();

    // `localhost` can resolve to ::1 on Windows — listen on both stacks.
    let mut listeners = Vec::new();
    if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        listeners.push(l);
    }
    if let Ok(l) = tokio::net::TcpListener::bind(("::1", port)).await {
        listeners.push(l);
    }
    if listeners.is_empty() {
        bail!("could not bind loopback port {port} (something else listening?)");
    }

    let state = {
        use rand::RngCore;
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut bytes);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    };
    let mut url = url::Url::parse("https://slack.com/oauth/v2/authorize")?;
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("user_scope", USER_SCOPES)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", &state);

    println!("Opening browser for Slack consent…");
    println!("If no browser opens, visit:\n{url}");
    let _ = webbrowser::open(url.as_str());

    let params = crate::loopback::wait_for_redirect(listeners, &path, 300).await?;
    if params.get("state").map(String::as_str) != Some(state.as_str()) {
        bail!("OAuth state mismatch — aborting");
    }
    if let Some(err) = params.get("error") {
        bail!("Slack authorization was denied or failed: {err}");
    }
    let code = params.get("code").context("redirect had no ?code")?;

    let resp = reqwest::Client::new()
        .post("https://slack.com/api/oauth.v2.access")
        .form(&[
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
        ])
        .send()
        .await?;
    let body = resp.text().await?;
    let v: Value = serde_json::from_str(&body).context("oauth.v2.access returned non-JSON")?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        // Print only the error code — the success body would contain tokens.
        bail!(
            "Slack oauth.v2.access failed: {}",
            v.get("error").and_then(Value::as_str).unwrap_or("unknown_error")
        );
    }
    let mut token = v;
    token["obtained_at_unix"] = chrono::Utc::now().timestamp().into();
    let path = crate::store::save_token("SLACK_TOKEN_PATH", &token)?;
    println!(
        "Slack token stored (DPAPI-encrypted) at {}\nGranted user scopes: {}",
        path.display(),
        token
            .pointer("/authed_user/scope")
            .and_then(Value::as_str)
            .unwrap_or("(none reported)")
    );
    Ok(())
}

fn user_token() -> Result<String> {
    let t = crate::store::load_token("SLACK_TOKEN_PATH")
        .context("run `fixture-capture slack-auth` first")?;
    Ok(t.pointer("/authed_user/access_token")
        .and_then(Value::as_str)
        .context("stored Slack token missing authed_user.access_token")?
        .to_string())
}

async fn slack_get(method: &str, params: &[(&str, &str)], token: &str) -> Result<String> {
    let mut url = url::Url::parse(&format!("https://slack.com/api/{method}"))?;
    for (k, v) in params {
        url.query_pairs_mut().append_pair(k, v);
    }
    let resp = reqwest::Client::new().get(url).bearer_auth(token).send().await?;
    let body = resp.text().await?;
    let v: Value =
        serde_json::from_str(&body).with_context(|| format!("{method} returned non-JSON"))?;
    // Slack returns HTTP 200 with ok=false on errors — check the envelope.
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        let err = v.get("error").and_then(Value::as_str).unwrap_or("unknown_error");
        let needed = v.get("needed").and_then(Value::as_str).unwrap_or("");
        if needed.is_empty() {
            bail!("{method} returned ok=false: {err}");
        }
        bail!("{method} returned ok=false: {err} — missing scope: {needed}");
    }
    Ok(body)
}

/// Phase 2.0 payload-first capture for the WRITE path: post ONE real message
/// to the configured private test channel and save the raw chat.postMessage
/// response. Run BEFORE modeling any post-response struct.
pub async fn capture_post() -> Result<()> {
    let token = user_token()?;
    let channel = crate::config::required("SLACK_TEST_CHANNEL").context(
        "SLACK_TEST_CHANNEL is not set — add the channel id of a private test channel \
         you are a member of to .env",
    )?;

    let body = serde_json::json!({
        "channel": channel,
        "text": "Almanac Phase 2.0 payload capture — fixturing the chat.postMessage \
                 response. Expected exactly once per capture run.",
    });
    let resp = reqwest::Client::new()
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(&token)
        .header("content-type", "application/json; charset=utf-8")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await?;
    let text = resp.text().await?;
    let v: Value = serde_json::from_str(&text).context("chat.postMessage returned non-JSON")?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        let err = v.get("error").and_then(Value::as_str).unwrap_or("unknown_error");
        if err == "missing_scope" {
            bail!(
                "chat.postMessage failed: missing_scope — add chat:write under \"User Token \
                 Scopes\" on the Slack app config (api.slack.com → OAuth & Permissions), \
                 reinstall the app, then re-run `fixture-capture slack-auth`"
            );
        }
        bail!("chat.postMessage failed: {err}");
    }
    crate::store::save_fixture("slack_post", "post_response.json", &text)?;
    println!("slack_post: captured post_response for channel {channel}");
    Ok(())
}

pub async fn capture() -> Result<()> {
    let token = user_token()?;

    let list_body = slack_get(
        "conversations.list",
        &[("limit", "100"), ("types", "public_channel")],
        &token,
    )
    .await?;
    let list = crate::store::save_fixture("slack", "conversations_list.json", &list_body)?;

    let channels = list.get("channels").and_then(Value::as_array).cloned().unwrap_or_default();
    let channel = channels
        .iter()
        .find(|c| c.get("is_member").and_then(Value::as_bool) == Some(true))
        .or_else(|| channels.first())
        .context("no channels visible to this user")?;
    let channel_id = channel
        .get("id")
        .and_then(Value::as_str)
        .context("channel object missing id")?;

    let history_body = slack_get(
        "conversations.history",
        &[("channel", channel_id), ("limit", "50")],
        &token,
    )
    .await?;
    let history = crate::store::save_fixture("slack", "conversations_history.json", &history_body)?;

    println!(
        "slack: captured conversations_list ({} channels) + history for {} ({} messages)",
        channels.len(),
        channel_id,
        history.get("messages").and_then(Value::as_array).map_or(0, Vec::len)
    );
    Ok(())
}
