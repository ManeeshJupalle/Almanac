//! Encrypted token store + OAuth token lifecycle (ARCHITECTURE.md:
//! "Auth/Token Store (encrypted)").
//!
//! Tokens are stored as DPAPI-encrypted envelopes at the gitignored paths
//! configured in .env. Nothing in this module ever logs or returns a token to
//! a caller that prints it; refresh evidence uses SHA-256 fingerprints.

#[cfg(windows)]
mod dpapi;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[cfg(not(windows))]
mod dpapi {
    use anyhow::{bail, Result};
    // Encrypted-at-rest tokens are currently implemented with Windows DPAPI.
    // Other platforms must grow a keychain backend before storing tokens —
    // failing is safer than falling back to plaintext.
    pub fn protect(_data: &[u8]) -> Result<Vec<u8>> {
        bail!("token encryption is only implemented on Windows (DPAPI)")
    }
    pub fn unprotect(_data: &[u8]) -> Result<Vec<u8>> {
        bail!("token decryption is only implemented on Windows (DPAPI)")
    }
}

/// Short SHA-256 fingerprint of a secret — safe to print as change evidence.
pub fn fingerprint(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn save_token(path: &Path, token_json: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let plaintext = serde_json::to_vec(token_json)?;
    let blob = dpapi::protect(&plaintext)?;
    let envelope = serde_json::json!({
        "format": "dpapi-v1",
        "note": "DPAPI-encrypted OAuth token bound to this Windows user. Not plaintext.",
        "ciphertext_b64": base64::engine::general_purpose::STANDARD.encode(&blob),
    });
    std::fs::write(path, serde_json::to_vec_pretty(&envelope)?)?;
    Ok(())
}

pub fn load_token(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("no token at {} — run the auth flow first", path.display()))?;
    let envelope: Value = serde_json::from_str(&raw)?;
    let b64 = envelope
        .get("ciphertext_b64")
        .and_then(Value::as_str)
        .context("token file is not a dpapi-v1 envelope")?;
    let blob = base64::engine::general_purpose::STANDARD.decode(b64)?;
    let plaintext = dpapi::unprotect(&blob)?;
    Ok(serde_json::from_slice(&plaintext)?)
}

// ------------------------------------------------------------- Google ------

/// Google OAuth config: Desktop client JSON + encrypted token path.
#[derive(Debug, Clone)]
pub struct GoogleAuth {
    pub credentials_path: PathBuf,
    pub token_path: PathBuf,
}

impl GoogleAuth {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            credentials_path: std::env::var("GOOGLE_CREDENTIALS_PATH")
                .context("missing GOOGLE_CREDENTIALS_PATH (see .env.example)")?
                .into(),
            token_path: std::env::var("GOOGLE_TOKEN_PATH")
                .context("missing GOOGLE_TOKEN_PATH (see .env.example)")?
                .into(),
        })
    }

    fn client(&self) -> Result<(String, String, String)> {
        let raw = std::fs::read_to_string(&self.credentials_path)
            .with_context(|| format!("reading {}", self.credentials_path.display()))?;
        let v: Value = serde_json::from_str(&raw)?;
        let installed = v
            .get("installed")
            .context("credentials.json has no 'installed' key (need a Desktop OAuth client)")?;
        let get = |k: &str| -> Result<String> {
            Ok(installed
                .get(k)
                .and_then(Value::as_str)
                .with_context(|| format!("credentials.json missing installed.{k}"))?
                .to_string())
        };
        Ok((get("client_id")?, get("client_secret")?, get("token_uri")?))
    }

    /// Ensure a stored token exists (interactive consent is done once via the
    /// fixture-capture CLI; in-app OAuth UI is a later phase).
    pub fn ensure_authenticated(&self) -> Result<()> {
        load_token(&self.token_path).map(|_| ()).context(
            "no Google token — run `cargo run -p fixture-capture -- google-auth` once",
        )
    }

    /// Valid access token, refreshing via refresh_token when near expiry.
    pub async fn access_token(&self) -> Result<String> {
        let token = load_token(&self.token_path)?;
        let obtained = token.get("obtained_at_unix").and_then(Value::as_i64).unwrap_or(0);
        let expires_in = token.get("expires_in").and_then(Value::as_i64).unwrap_or(0);
        if chrono::Utc::now().timestamp() - obtained < expires_in - 120 {
            return Ok(token
                .get("access_token")
                .and_then(Value::as_str)
                .context("stored token missing access_token")?
                .to_string());
        }
        self.force_refresh().await?;
        let fresh = load_token(&self.token_path)?;
        Ok(fresh
            .get("access_token")
            .and_then(Value::as_str)
            .context("refreshed token missing access_token")?
            .to_string())
    }

    /// Unconditionally exercise the refresh_token grant and persist the new
    /// token. Returns printable (non-secret) evidence of the rotation.
    pub async fn force_refresh(&self) -> Result<RefreshEvidence> {
        let token = load_token(&self.token_path)?;
        let old_access = token
            .get("access_token")
            .and_then(Value::as_str)
            .context("stored token missing access_token")?
            .to_string();
        let old_obtained = token.get("obtained_at_unix").and_then(Value::as_i64).unwrap_or(0);
        let refresh = token
            .get("refresh_token")
            .and_then(Value::as_str)
            .context("stored token has no refresh_token — re-run google-auth")?
            .to_string();

        let (client_id, client_secret, token_uri) = self.client()?;
        let resp = reqwest::Client::new()
            .post(&token_uri)
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("refresh_token", refresh.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            // Testing-mode OAuth consent expires refresh tokens after 7 days;
            // surface an actionable message instead of a bare status code.
            if body.contains("invalid_grant") {
                bail!(
                    "Google token expired or revoked (testing-mode consent expires after \
                     7 days) — reconnect with `cargo run -p fixture-capture -- google-auth`"
                );
            }
            bail!("Google token refresh failed ({status})");
        }
        let mut fresh: Value = serde_json::from_str(&body)?;
        if fresh.get("refresh_token").is_none() {
            fresh["refresh_token"] = Value::String(refresh);
        }
        let now = chrono::Utc::now().timestamp();
        fresh["obtained_at_unix"] = now.into();
        save_token(&self.token_path, &fresh)?;

        let new_access = fresh
            .get("access_token")
            .and_then(Value::as_str)
            .context("refresh response missing access_token")?;
        Ok(RefreshEvidence {
            old_fingerprint: fingerprint(&old_access),
            new_fingerprint: fingerprint(new_access),
            token_changed: old_access != new_access,
            old_obtained_at_unix: old_obtained,
            new_obtained_at_unix: now,
            new_expires_in: fresh.get("expires_in").and_then(Value::as_i64).unwrap_or(0),
            detail: "grant_type=refresh_token against Google token endpoint".to_string(),
        })
    }
}

// -------------------------------------------------------------- Slack ------

#[derive(Debug, Clone)]
pub struct SlackAuth {
    pub token_path: PathBuf,
}

impl SlackAuth {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            token_path: std::env::var("SLACK_TOKEN_PATH")
                .context("missing SLACK_TOKEN_PATH (see .env.example)")?
                .into(),
        })
    }

    pub fn ensure_authenticated(&self) -> Result<()> {
        load_token(&self.token_path).map(|_| ()).context(
            "no Slack token — run `cargo run -p fixture-capture -- slack-auth` once",
        )
    }

    pub fn user_token(&self) -> Result<String> {
        let t = load_token(&self.token_path)?;
        Ok(t.pointer("/authed_user/access_token")
            .and_then(Value::as_str)
            .context("stored Slack token missing authed_user.access_token")?
            .to_string())
    }

    /// Slack refresh: user tokens only rotate when the app has opted into
    /// token rotation (then the stored token carries a refresh_token). With
    /// rotation, exercise oauth.v2.access grant_type=refresh_token; without,
    /// validate the non-expiring token live via auth.test and say so.
    pub async fn force_refresh(&self) -> Result<RefreshEvidence> {
        let token = load_token(&self.token_path)?;
        let old_access = token
            .pointer("/authed_user/access_token")
            .and_then(Value::as_str)
            .context("stored Slack token missing authed_user.access_token")?
            .to_string();
        let refresh = token
            .pointer("/authed_user/refresh_token")
            .or_else(|| token.get("refresh_token"))
            .and_then(Value::as_str);

        match refresh {
            Some(refresh_token) => {
                let client_id = std::env::var("SLACK_CLIENT_ID")
                    .context("missing SLACK_CLIENT_ID (needed for token rotation)")?;
                let client_secret = std::env::var("SLACK_CLIENT_SECRET")
                    .context("missing SLACK_CLIENT_SECRET (needed for token rotation)")?;
                let resp = reqwest::Client::new()
                    .post("https://slack.com/api/oauth.v2.access")
                    .form(&[
                        ("client_id", client_id.as_str()),
                        ("client_secret", client_secret.as_str()),
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh_token),
                    ])
                    .send()
                    .await?;
                let body = resp.text().await?;
                let v: Value = serde_json::from_str(&body)?;
                if v.get("ok").and_then(Value::as_bool) != Some(true) {
                    bail!(
                        "Slack token rotation failed: {}",
                        v.get("error").and_then(Value::as_str).unwrap_or("unknown_error")
                    );
                }
                let mut fresh = v;
                fresh["obtained_at_unix"] = chrono::Utc::now().timestamp().into();
                save_token(&self.token_path, &fresh)?;
                let new_access = fresh
                    .pointer("/authed_user/access_token")
                    .or_else(|| fresh.get("access_token"))
                    .and_then(Value::as_str)
                    .context("rotation response missing access token")?;
                Ok(RefreshEvidence {
                    old_fingerprint: fingerprint(&old_access),
                    new_fingerprint: fingerprint(new_access),
                    token_changed: old_access != new_access,
                    old_obtained_at_unix: token
                        .get("obtained_at_unix")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    new_obtained_at_unix: chrono::Utc::now().timestamp(),
                    new_expires_in: fresh
                        .pointer("/authed_user/expires_in")
                        .or_else(|| fresh.get("expires_in"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    detail: "oauth.v2.access grant_type=refresh_token (rotation enabled)"
                        .to_string(),
                })
            }
            None => {
                // No rotation: the token is non-expiring by Slack's design.
                // Prove it is live-valid right now via auth.test.
                let resp = reqwest::Client::new()
                    .get("https://slack.com/api/auth.test")
                    .bearer_auth(&old_access)
                    .send()
                    .await?;
                let v: Value = serde_json::from_str(&resp.text().await?)?;
                if v.get("ok").and_then(Value::as_bool) != Some(true) {
                    bail!(
                        "Slack auth.test failed: {}",
                        v.get("error").and_then(Value::as_str).unwrap_or("unknown_error")
                    );
                }
                Ok(RefreshEvidence {
                    old_fingerprint: fingerprint(&old_access),
                    new_fingerprint: fingerprint(&old_access),
                    token_changed: false,
                    old_obtained_at_unix: token
                        .get("obtained_at_unix")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    new_obtained_at_unix: chrono::Utc::now().timestamp(),
                    new_expires_in: 0,
                    detail: "no refresh_token stored: app has not opted into Slack token \
                             rotation, so the user token is non-expiring; validated live via \
                             auth.test instead"
                        .to_string(),
                })
            }
        }
    }

    /// Workspace base URL (e.g. https://myteam.slack.com/) via auth.test —
    /// needed to construct real message deep links.
    pub async fn workspace_url(&self) -> Result<String> {
        let token = self.user_token()?;
        let resp = reqwest::Client::new()
            .get("https://slack.com/api/auth.test")
            .bearer_auth(&token)
            .send()
            .await?;
        let v: Value = serde_json::from_str(&resp.text().await?)?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            bail!(
                "Slack auth.test failed: {}",
                v.get("error").and_then(Value::as_str).unwrap_or("unknown_error")
            );
        }
        Ok(v.get("url")
            .and_then(Value::as_str)
            .context("auth.test response missing url")?
            .to_string())
    }
}

// --------------------------------------------------------------- Jira ------

/// Jira Cloud auth (Phase 2.1): HTTP Basic over `base64(email:api_token)`.
///
/// A2 scope isolation: this struct reads ONLY `JIRA_*` config and its own
/// encrypted token file — it holds no reference to `GoogleAuth`/`SlackAuth`
/// and cannot reach their tokens (and they cannot reach Jira's). The
/// email+token pair is stored DPAPI-encrypted at `JIRA_TOKEN_PATH`, like the
/// other connectors; the raw `JIRA_API_TOKEN` env var is only the one-time
/// bootstrap consumed by `fixture-capture jira-auth`.
#[derive(Debug, Clone)]
pub struct JiraAuth {
    /// Site base, no trailing slash (e.g. https://yoursite.atlassian.net).
    pub base_url: String,
    pub token_path: PathBuf,
}

impl JiraAuth {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            base_url: std::env::var("JIRA_BASE_URL")
                .context("missing JIRA_BASE_URL (see .env.example)")?
                .trim_end_matches('/')
                .to_string(),
            token_path: std::env::var("JIRA_TOKEN_PATH")
                .context("missing JIRA_TOKEN_PATH (see .env.example)")?
                .into(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn ensure_authenticated(&self) -> Result<()> {
        load_token(&self.token_path).map(|_| ()).context(
            "no Jira token — run `cargo run -p fixture-capture -- jira-auth` once",
        )
    }

    /// `Authorization: Basic …` header value, built from the encrypted store.
    /// Never logged; callers pass it straight to reqwest.
    pub fn basic_auth_header(&self) -> Result<String> {
        let token = load_token(&self.token_path)?;
        let email = token
            .get("email")
            .and_then(Value::as_str)
            .context("stored Jira token missing email")?;
        let api_token = token
            .get("api_token")
            .and_then(Value::as_str)
            .context("stored Jira token missing api_token")?;
        let raw = format!("{email}:{api_token}");
        Ok(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw)))
    }
}

/// Printable, secret-free proof that a refresh actually happened.
#[derive(Debug)]
pub struct RefreshEvidence {
    pub old_fingerprint: String,
    pub new_fingerprint: String,
    pub token_changed: bool,
    pub old_obtained_at_unix: i64,
    pub new_obtained_at_unix: i64,
    pub new_expires_in: i64,
    pub detail: String,
}

impl std::fmt::Display for RefreshEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "  token changed: {}", self.token_changed)?;
        writeln!(f, "  old fingerprint (sha256/12): {}", self.old_fingerprint)?;
        writeln!(f, "  new fingerprint (sha256/12): {}", self.new_fingerprint)?;
        writeln!(f, "  old obtained_at: {}", self.old_obtained_at_unix)?;
        writeln!(f, "  new obtained_at: {}", self.new_obtained_at_unix)?;
        writeln!(f, "  new expires_in: {}s", self.new_expires_in)?;
        write!(f, "  path: {}", self.detail)
    }
}
