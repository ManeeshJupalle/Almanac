//! Action executors (ARCHITECTURE_V2 §5, §7). **A2: this module is the ONLY
//! code in the workspace that touches write endpoints** (`messages/send`,
//! `chat.postMessage`) — nothing else may construct a write call, and the
//! write-scoped tokens are read only here (from the same encrypted store as
//! the read path; scopes: Google `gmail.send`, Slack `chat:write`).
//!
//! Discipline per executor:
//! 1. `execute()` re-validates `state == approved` from the database FIRST —
//!    defense in depth behind the state machine (A1), so a UI bug cannot
//!    reach the network.
//! 2. `dry_run()` renders the EXACT final payload — full MIME for Gmail, the
//!    exact JSON body for Slack — and `execute()` sends `dry_run()`'s bytes
//!    verbatim (byte-identity by construction, pinned by tests).
//! 3. L1: the `execution_started` audit record (hashing those exact bytes)
//!    commits BEFORE the send; the receipt or failure is audited after. An
//!    action that cannot be audited does not execute.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use rusqlite::Connection;
use serde_json::Value;

use super::draft::{reply_subject, sanitize_header_value};
use super::{audit, ActionKind, ActionTarget, ProposalState, StoredProposal};
use crate::auth::{GoogleAuth, SlackAuth};

/// The exact action as it will hit the wire. `api_body` is byte-identical to
/// what `execute()` sends; `display` is what the approval UI must show
/// verbatim (for Gmail the full MIME that becomes the email; for Slack it is
/// the same bytes as `api_body`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedAction {
    pub endpoint: &'static str,
    pub content_type: &'static str,
    pub api_body: Vec<u8>,
    pub display: String,
}

/// Proof of an execution attempt, with the audit seqs that bracket it.
#[derive(Debug)]
pub struct ExecutionReceipt {
    pub proposal_id: i64,
    pub endpoint: &'static str,
    pub http_status: u16,
    /// Full response body (send/post metadata about OUR message — not
    /// third-party raw content).
    pub response_body: String,
    pub started_seq: i64,
    pub final_seq: i64,
}

#[async_trait]
pub trait ActionExecutor {
    fn action_kind(&self) -> ActionKind;
    /// Render the exact final payload without sending. Local-only: needs no
    /// token and performs no I/O beyond reading the local store.
    fn dry_run(&self, conn: &Connection, proposal: &StoredProposal) -> Result<RenderedAction>;
    /// Execute an APPROVED proposal. Re-validates state from the DB, audits
    /// before sending (L1), sends `dry_run`'s exact bytes, audits the result.
    async fn execute(&self, conn: &mut Connection, proposal_id: i64) -> Result<ExecutionReceipt>;
}

/// Shared gate: load + re-validate state/kind (A1/A2 defense in depth).
fn load_approved(
    conn: &Connection,
    proposal_id: i64,
    expected_kind: ActionKind,
) -> Result<StoredProposal> {
    let proposal = super::load_proposal(conn, proposal_id)?;
    if proposal.kind != expected_kind {
        bail!(
            "executor for {} refuses proposal {proposal_id} of kind {}",
            expected_kind.as_str(),
            proposal.kind.as_str()
        );
    }
    if proposal.state != ProposalState::Approved {
        bail!(
            "REFUSING to execute proposal {proposal_id}: state is '{}', not 'approved' — \
             execution requires an explicit user approval event (A1)",
            proposal.state.as_str()
        );
    }
    Ok(proposal)
}

/// Shared post-render pipeline: claim+audit, send, finalize+audit.
async fn send_gated(
    conn: &mut Connection,
    proposal_id: i64,
    rendered: &RenderedAction,
    bearer_token: &str,
    success: impl Fn(u16, &str) -> Result<(), String>,
) -> Result<ExecutionReceipt> {
    // L1: audited (with the hash of the exact bytes) BEFORE the side effect.
    let started_seq =
        super::claim_execution(conn, proposal_id, &audit::sha256_hex(&rendered.api_body))?;

    let sent = reqwest::Client::new()
        .post(rendered.endpoint)
        .header(reqwest::header::CONTENT_TYPE, rendered.content_type)
        .bearer_auth(bearer_token)
        .body(rendered.api_body.clone())
        .send()
        .await;

    match sent {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            match success(status, &body) {
                Ok(()) => {
                    let final_seq =
                        super::finalize_execution(conn, proposal_id, Some(body.trim()), None)?;
                    Ok(ExecutionReceipt {
                        proposal_id,
                        endpoint: rendered.endpoint,
                        http_status: status,
                        response_body: body.trim().to_string(),
                        started_seq,
                        final_seq,
                    })
                }
                Err(why) => {
                    // Error bodies carry status/error codes, never tokens.
                    let msg = format!("send failed ({status}): {why}");
                    let final_seq =
                        super::finalize_execution(conn, proposal_id, None, Some(&msg))?;
                    bail!("{msg} (audited as execution_failed, seq {final_seq})");
                }
            }
        }
        Err(e) => {
            let msg = format!("send failed before a response: {e}");
            let final_seq = super::finalize_execution(conn, proposal_id, None, Some(&msg))?;
            bail!("{msg} (audited as execution_failed, seq {final_seq})");
        }
    }
}

// ------------------------------------------------------------- Gmail ------

const GMAIL_SEND_ENDPOINT: &str = "https://gmail.googleapis.com/gmail/v1/users/me/messages/send";

/// Thread-aware Gmail reply. Requires scope `gmail.send` on the stored token
/// (re-consent; see README). Holds the ONLY Gmail write path (A2).
pub struct GmailReplyExecutor {
    auth: GoogleAuth,
}

impl GmailReplyExecutor {
    pub fn new(auth: GoogleAuth) -> Self {
        Self { auth }
    }
    pub fn from_env() -> Result<Self> {
        Ok(Self::new(GoogleAuth::from_env()?))
    }
}

/// Pure render: proposal + the stored original message → exact send payload.
/// Threading is modeled against the Phase-1 fixtured full message: header
/// names are wire-cased (`Message-Id`, G8) so lookups are case-insensitive;
/// `threadId` comes from the stored message itself.
pub fn render_gmail_reply(conn: &Connection, proposal: &StoredProposal) -> Result<RenderedAction> {
    let ActionTarget::GmailThread { message_native_id } = &proposal.target else {
        bail!("proposal {} is not a gmail reply", proposal.id);
    };
    let raw = crate::db::source_raw_json(conn, crate::types::SourceId::Gmail, message_native_id)?
        .with_context(|| {
            format!("reply target gmail:{message_native_id} is not a stored source object")
        })?;
    let thread_id = raw
        .get("threadId")
        .and_then(Value::as_str)
        .context("stored message has no threadId — cannot thread a reply")?
        .to_string();
    let payload = raw.get("payload").cloned().unwrap_or(Value::Null);
    let headers = |name: &str| -> Option<String> {
        crate::adapters::gmail::header_values(&payload, name)
            .first()
            .map(|v| sanitize_header_value(v))
            .filter(|v| !v.is_empty())
    };

    // Reply addressing per RFC 5322 conventions: Reply-To wins over From.
    let to = headers("Reply-To")
        .or_else(|| headers("From"))
        .context("original message has neither Reply-To nor From — cannot address a reply")?;
    // Subject comes from the proposal draft (which used the same reply_subject
    // rule at drafting time); fall back to deriving it here so the MIME can
    // never disagree with the draft.
    let subject = proposal
        .draft_subject
        .clone()
        .map(|s| sanitize_header_value(&s))
        .unwrap_or_else(|| reply_subject(&headers("Subject").unwrap_or_default()));
    let orig_message_id = headers("Message-Id"); // wire-cased in real payloads (G8)
    let references = match (headers("References"), &orig_message_id) {
        (Some(refs), Some(mid)) => Some(format!("{refs} {mid}")),
        (None, Some(mid)) => Some(mid.clone()),
        (existing, None) => existing,
    };

    // Deterministic MIME: no Date/From/Message-ID — Gmail sets them at send
    // time. CRLF line endings; single text/plain part (no boundary → no
    // nondeterminism).
    let mut mime = String::new();
    mime.push_str(&format!("To: {to}\r\n"));
    mime.push_str(&format!("Subject: {subject}\r\n"));
    if let Some(mid) = &orig_message_id {
        mime.push_str(&format!("In-Reply-To: {mid}\r\n"));
    }
    if let Some(refs) = &references {
        mime.push_str(&format!("References: {refs}\r\n"));
    }
    mime.push_str("MIME-Version: 1.0\r\n");
    mime.push_str("Content-Type: text/plain; charset=\"utf-8\"\r\n");
    mime.push_str("Content-Transfer-Encoding: 8bit\r\n");
    mime.push_str("\r\n");
    mime.push_str(&proposal.draft_body);

    // G9 discipline: Gmail's raw field is base64url.
    let raw_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mime.as_bytes());
    let api_body = serde_json::to_vec(&serde_json::json!({
        "raw": raw_b64,
        "threadId": thread_id,
    }))?;

    Ok(RenderedAction {
        endpoint: GMAIL_SEND_ENDPOINT,
        content_type: "application/json",
        api_body,
        display: mime,
    })
}

#[async_trait]
impl ActionExecutor for GmailReplyExecutor {
    fn action_kind(&self) -> ActionKind {
        ActionKind::GmailReply
    }

    fn dry_run(&self, conn: &Connection, proposal: &StoredProposal) -> Result<RenderedAction> {
        render_gmail_reply(conn, proposal)
    }

    async fn execute(&self, conn: &mut Connection, proposal_id: i64) -> Result<ExecutionReceipt> {
        // A1/A2 gate first: nothing below runs for a non-approved proposal.
        let proposal = load_approved(conn, proposal_id, ActionKind::GmailReply)?;
        let rendered = render_gmail_reply(conn, &proposal)?;
        // Token AFTER the gate, BEFORE the claim: a token failure must not
        // leave a claimed-but-unsent proposal.
        let token = self.auth.access_token().await?;
        send_gated(conn, proposal_id, &rendered, &token, |status, _| {
            if (200..300).contains(&status) {
                Ok(())
            } else if status == 403 {
                Err("missing scope? gmail.send requires re-consent \
                     (`cargo run -p fixture-capture -- google-auth`)"
                    .to_string())
            } else {
                Err("gmail send rejected".to_string())
            }
        })
        .await
    }
}

// ------------------------------------------------------------- Slack ------

const SLACK_POST_ENDPOINT: &str = "https://slack.com/api/chat.postMessage";

/// Slack channel post. Requires user scope `chat:write` (app config +
/// re-consent; see README). Holds the ONLY Slack write path (A2).
pub struct SlackPostExecutor {
    auth: SlackAuth,
}

impl SlackPostExecutor {
    pub fn new(auth: SlackAuth) -> Self {
        Self { auth }
    }
    pub fn from_env() -> Result<Self> {
        Ok(Self::new(SlackAuth::from_env()?))
    }
}

/// Pure render: the exact chat.postMessage JSON body.
pub fn render_slack_post(proposal: &StoredProposal) -> Result<RenderedAction> {
    let ActionTarget::SlackChannel { channel_id } = &proposal.target else {
        bail!("proposal {} is not a slack post", proposal.id);
    };
    let api_body = serde_json::to_vec(&serde_json::json!({
        "channel": channel_id,
        "text": proposal.draft_body,
    }))?;
    let display = String::from_utf8(api_body.clone()).expect("json is utf-8");
    Ok(RenderedAction {
        endpoint: SLACK_POST_ENDPOINT,
        content_type: "application/json; charset=utf-8",
        api_body,
        display,
    })
}

#[async_trait]
impl ActionExecutor for SlackPostExecutor {
    fn action_kind(&self) -> ActionKind {
        ActionKind::SlackPost
    }

    fn dry_run(&self, _conn: &Connection, proposal: &StoredProposal) -> Result<RenderedAction> {
        render_slack_post(proposal)
    }

    async fn execute(&self, conn: &mut Connection, proposal_id: i64) -> Result<ExecutionReceipt> {
        let proposal = load_approved(conn, proposal_id, ActionKind::SlackPost)?;
        let rendered = render_slack_post(&proposal)?;
        let token = self.auth.user_token()?;
        send_gated(conn, proposal_id, &rendered, &token, |status, body| {
            // Slack signals errors as HTTP 200 + ok=false (S-corrections).
            let ok = serde_json::from_str::<Value>(body)
                .ok()
                .and_then(|v| v.get("ok").and_then(Value::as_bool))
                == Some(true);
            if (200..300).contains(&status) && ok {
                Ok(())
            } else {
                let err = serde_json::from_str::<Value>(body)
                    .ok()
                    .and_then(|v| {
                        v.get("error").and_then(Value::as_str).map(str::to_string)
                    })
                    .unwrap_or_else(|| "unknown_error".to_string());
                if err == "missing_scope" {
                    Err("missing_scope — add the chat:write USER scope on the Slack app \
                         config, reinstall the app, and re-run \
                         `cargo run -p fixture-capture -- slack-auth`"
                        .to_string())
                } else {
                    Err(err)
                }
            }
        })
        .await
    }
}
