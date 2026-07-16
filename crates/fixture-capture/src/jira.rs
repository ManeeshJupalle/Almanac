//! Jira Cloud (Atlassian) auth + payload-first capture (Phase 2.1).
//!
//! `jira-auth` validates the API token (GET /myself) and writes it
//! DPAPI-encrypted to JIRA_TOKEN_PATH, like the other connectors. `capture`
//! then exercises the read + write endpoints and saves the REAL responses to
//! /fixtures/jira{,_transition,_comment}/ (redacted) before any struct is
//! modeled. Responses are handled as generic JSON only.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::Value;

fn base_url() -> Result<String> {
    Ok(crate::config::required("JIRA_BASE_URL")?.trim_end_matches('/').to_string())
}

/// Basic header from the raw env token — used ONLY by `jira-auth` before the
/// encrypted store exists.
fn basic_header_from_env() -> Result<String> {
    let email = crate::config::required("JIRA_EMAIL")?;
    let token = crate::config::required("JIRA_API_TOKEN")?;
    Ok(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"))))
}

/// Basic header from the encrypted store — used by `capture` and the app.
fn basic_header_from_store() -> Result<String> {
    almanac_core::auth::JiraAuth::from_env()?.basic_auth_header()
}

/// GET, returning (status, body). Never logs the auth header.
async fn get(header: &str, url: &str) -> Result<(u16, String)> {
    let resp = reqwest::Client::new()
        .get(url)
        .header("authorization", header)
        .header("accept", "application/json")
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.text().await?))
}

/// POST JSON, returning (status, body).
async fn post_json(header: &str, url: &str, body: &Value) -> Result<(u16, String)> {
    let resp = reqwest::Client::new()
        .post(url)
        .header("authorization", header)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(serde_json::to_vec(body)?)
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.text().await?))
}

// ------------------------------------------------------------- jira-auth ----

pub async fn authorize() -> Result<()> {
    let header = basic_header_from_env()?;
    let base = base_url()?;
    let (status, body) = get(&header, &format!("{base}/rest/api/3/myself")).await?;
    if status != 200 {
        // Error bodies carry no secret; safe to surface for diagnosis.
        bail!(
            "Jira token validation failed (GET /myself → {status}). Check JIRA_EMAIL / \
             JIRA_API_TOKEN / JIRA_BASE_URL. Body: {body}"
        );
    }
    let me: Value = serde_json::from_str(&body)?;
    // Store email + token (NOT logged) DPAPI-encrypted, like other connectors.
    let email = crate::config::required("JIRA_EMAIL")?;
    let token = crate::config::required("JIRA_API_TOKEN")?;
    let stored = serde_json::json!({
        "email": email,
        "api_token": token,
        "base_url": base,
        "obtained_at_unix": chrono::Utc::now().timestamp(),
    });
    let path = crate::store::save_token("JIRA_TOKEN_PATH", &stored)?;
    // Print only the non-secret account identity + a token fingerprint.
    println!(
        "Jira token stored (DPAPI-encrypted) at {}\nValidated account: {} (accountId {})\ntoken fingerprint (sha256/12): {}",
        path.display(),
        me.get("displayName").and_then(Value::as_str).unwrap_or("(unknown)"),
        me.get("accountId").and_then(Value::as_str).unwrap_or("(unknown)"),
        almanac_core::auth::fingerprint(&token),
    );
    Ok(())
}

// --------------------------------------------------------------- capture ----

/// (key, status name) for an issue in the search result.
fn issue_rows(search: &Value) -> Vec<(String, String)> {
    search
        .get("issues")
        .and_then(Value::as_array)
        .map(|issues| {
            issues
                .iter()
                .filter_map(|i| {
                    let key = i.get("key").and_then(Value::as_str)?.to_string();
                    let status = i
                        .pointer("/fields/status/name")
                        .and_then(Value::as_str)
                        .unwrap_or("(unknown)")
                        .to_string();
                    Some((key, status))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub async fn capture() -> Result<()> {
    let header = basic_header_from_store()
        .context("run `fixture-capture jira-auth` first (need the encrypted token)")?;
    let base = base_url()?;

    // 1. Issue search. Atlassian migrated /rest/api/3/search (startAt) →
    //    /rest/api/3/search/jql (nextPageToken). Capture what the site serves
    //    on BOTH so the corrections table can document the live one.
    let jql = "project = ALM ORDER BY key ASC";
    let mut enhanced = url::Url::parse(&format!("{base}/rest/api/3/search/jql"))?;
    enhanced
        .query_pairs_mut()
        .append_pair("jql", jql)
        .append_pair("maxResults", "50")
        .append_pair("fields", "summary,status,issuetype,priority");
    let (s_status, s_body) = get(&header, enhanced.as_str()).await?;
    let search = if s_status == 200 {
        crate::store::save_fixture("jira", "search_jql.json", &s_body)?
    } else {
        bail!("enhanced search /search/jql → {s_status}: {s_body}");
    };
    println!("jira: /search/jql → 200 ({} issues)", issue_rows(&search).len());

    // Probe the legacy endpoint to document its fate (deprecation / 410 / etc.).
    let mut legacy = url::Url::parse(&format!("{base}/rest/api/3/search"))?;
    legacy
        .query_pairs_mut()
        .append_pair("jql", jql)
        .append_pair("maxResults", "5")
        .append_pair("fields", "summary,status");
    let (l_status, l_body) = get(&header, legacy.as_str()).await?;
    println!("jira: legacy /search → {l_status}");
    if serde_json::from_str::<Value>(&l_body).is_ok() {
        crate::store::save_fixture("jira", "search_legacy_probe.json", &l_body)?;
    } else {
        println!("   (legacy /search body was not JSON; status {l_status} documented in corrections)");
    }

    // Choose two issues in DIFFERENT statuses (transitions are state-dependent).
    let rows = issue_rows(&search);
    if rows.is_empty() {
        bail!("no ALM issues returned — seed the project first");
    }
    let (key_a, status_a) = rows[0].clone();
    let (key_b, status_b) = rows
        .iter()
        .find(|(_, st)| *st != status_a)
        .cloned()
        .unwrap_or_else(|| rows.get(1).cloned().unwrap_or_else(|| rows[0].clone()));

    // 2. GET issue detail.
    let (i_status, i_body) = get(&header, &format!("{base}/rest/api/3/issue/{key_a}")).await?;
    if i_status != 200 {
        bail!("GET issue/{key_a} → {i_status}: {i_body}");
    }
    crate::store::save_fixture("jira", "issue_get.json", &i_body)?;
    println!("jira: GET issue/{key_a} → 200");

    // 3. Transitions for two different statuses.
    let (ta_status, ta_body) =
        get(&header, &format!("{base}/rest/api/3/issue/{key_a}/transitions")).await?;
    if ta_status != 200 {
        bail!("GET transitions/{key_a} → {ta_status}: {ta_body}");
    }
    let transitions_a = crate::store::save_fixture("jira", "transitions_status_a.json", &ta_body)?;
    let (tb_status, tb_body) =
        get(&header, &format!("{base}/rest/api/3/issue/{key_b}/transitions")).await?;
    if tb_status == 200 {
        crate::store::save_fixture("jira", "transitions_status_b.json", &tb_body)?;
    }
    println!(
        "jira: transitions for {key_a} (status '{status_a}') and {key_b} (status '{status_b}')"
    );

    // 4. Real transition (write) on issue A: take the first offered transition.
    let transition = transitions_a
        .get("transitions")
        .and_then(Value::as_array)
        .and_then(|t| t.first())
        .context("issue A offers no transitions to exercise the write path")?;
    let transition_id = transition
        .get("id")
        .and_then(Value::as_str)
        .context("transition has no id")?
        .to_string();
    let target_status = transition
        .pointer("/to/name")
        .and_then(Value::as_str)
        .unwrap_or("(unknown)")
        .to_string();
    let transition_req = serde_json::json!({ "transition": { "id": transition_id } });
    crate::store::save_fixture(
        "jira_transition",
        "transition_request.json",
        &transition_req.to_string(),
    )?;
    let (tr_status, tr_body) =
        post_json(&header, &format!("{base}/rest/api/3/issue/{key_a}/transitions"), &transition_req)
            .await?;
    println!("jira: POST transition {transition_id} on {key_a} → {tr_status} (→ '{target_status}')");
    if !tr_body.trim().is_empty() && serde_json::from_str::<Value>(&tr_body).is_ok() {
        crate::store::save_fixture("jira_transition", "transition_response.json", &tr_body)?;
    } else {
        println!("   (transition response body is empty — HTTP {tr_status}; documented in corrections)");
    }
    if !(200..300).contains(&tr_status) {
        bail!("transition POST failed ({tr_status}): {tr_body}");
    }
    // Follow-up GET shows the changed status.
    let (_, after_body) = get(&header, &format!("{base}/rest/api/3/issue/{key_a}")).await?;
    crate::store::save_fixture("jira_transition", "issue_after_transition.json", &after_body)?;

    // 5. Comment (write) — ADF body. Also probe a PLAIN-TEXT body to capture
    //    the v3 rejection (docs say v3 requires ADF; confirm/refute).
    let adf = serde_json::json!({
        "body": {
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "paragraph",
                "content": [{
                    "type": "text",
                    "text": "Almanac Phase 2.1 payload capture — this comment fixtures the \
                             ADF request/response shape. Posted once per capture run."
                }]
            }]
        }
    });
    crate::store::save_fixture("jira_comment", "comment_request.json", &adf.to_string())?;
    let (c_status, c_body) =
        post_json(&header, &format!("{base}/rest/api/3/issue/{key_a}/comment"), &adf).await?;
    println!("jira: POST comment (ADF) on {key_a} → {c_status}");
    if serde_json::from_str::<Value>(&c_body).is_ok() {
        crate::store::save_fixture("jira_comment", "comment_response.json", &c_body)?;
    }
    if !(200..300).contains(&c_status) {
        bail!("comment POST failed ({c_status}): {c_body}");
    }

    let plain = serde_json::json!({ "body": "plain text, no ADF" });
    let (p_status, p_body) =
        post_json(&header, &format!("{base}/rest/api/3/issue/{key_a}/comment"), &plain).await?;
    println!("jira: POST comment (plain text) on {key_a} → {p_status} (expected a 4xx)");
    if serde_json::from_str::<Value>(&p_body).is_ok() {
        crate::store::save_fixture("jira_comment", "comment_plaintext_error.json", &p_body)?;
    }

    println!("jira capture complete — fixtures written under fixtures/jira*, review before committing.");
    Ok(())
}
