//! Drafting (Phase 2.0: templated ONLY — no model drafting).
//!
//! v1 lesson applied (ARCHITECTURE_V2 §7): a small model orders well but
//! garbles prose, and garbled prose never ships under the user's name. The
//! default backend renders fixed templates whose slots are TYPED fields
//! (commit shas, links, times, ticket keys) validated before interpolation.
//! Free text from untrusted input (email/Slack bodies, subjects) is never
//! interpolated into a draft body. The one untrusted value that must appear —
//! the original Subject on a reply, required for threading — goes only into
//! the Subject header, sanitized, and is fully visible in the dry-run the
//! user approves.
//!
//! Prompt-injection posture: because slots are typed and validated, text like
//! "ignore previous instructions" arriving in an email cannot flow into an
//! outward draft through this backend.

use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};

/// A produced draft. `asserts_work_done` is declared PER TEMPLATE by the
/// backend (a template that states work happened sets it true) and drives the
/// E1 hard-evidence requirement in `ActionProposal::new`.
#[derive(Debug, Clone)]
pub struct Draft {
    /// Subject for email replies (None for Slack posts).
    pub subject: Option<String>,
    pub body: String,
    pub asserts_work_done: bool,
}

/// What to draft. Every field is typed; there is deliberately no
/// free-text-body variant in Phase 2.0.
#[derive(Debug, Clone)]
pub enum DraftRequest {
    /// Reply asserting completed work — E1: requires Tier-Hard evidence.
    WorkDoneReply {
        /// Untrusted; used ONLY for the "Re:" subject header (threading).
        original_subject: String,
        commit_short: String,
        completed_at: DateTime<Utc>,
        ci_link: Option<String>,
        ticket: Option<String>,
    },
    /// Acknowledgement reply — makes no claim about work done.
    AckReply {
        /// Untrusted; used ONLY for the "Re:" subject header (threading).
        original_subject: String,
    },
    /// Slack post asserting completed work — E1: requires Tier-Hard evidence.
    SlackWorkDonePost {
        ticket: String,
        commit_short: String,
        completed_at: DateTime<Utc>,
        ci_link: Option<String>,
    },
    /// Fixed-text Slack check-in post — makes no claim about work done.
    /// (Used by the Phase 2.0 live gate and the dev seed path.)
    SlackCheckInPost,
}

/// Same shape as `SynthesisBackend`: swappable so a model backend can be
/// slotted in later and delta-measured honestly (Phase 2.x, not now).
pub trait DraftingBackend {
    fn backend_id(&self) -> &str;
    fn draft(&self, req: &DraftRequest) -> Result<Draft>;
}

/// Strip CR/LF and other control characters so an untrusted subject cannot
/// inject MIME headers, then trim. Public so the executor can apply the same
/// rule to address headers.
pub fn sanitize_header_value(v: &str) -> String {
    v.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()
}

/// "Re: <subject>" without stacking prefixes.
pub fn reply_subject(original_subject: &str) -> String {
    let s = sanitize_header_value(original_subject);
    if s.to_ascii_lowercase().starts_with("re:") {
        s
    } else if s.is_empty() {
        "Re: (no subject)".to_string()
    } else {
        format!("Re: {s}")
    }
}

fn validate_commit_short(sha: &str) -> Result<()> {
    ensure!(
        (7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit()),
        "commit_short '{sha}' is not a git sha prefix (7-40 hex chars)"
    );
    Ok(())
}

fn validate_ticket(ticket: &str) -> Result<()> {
    let ok = ticket.split_once('-').is_some_and(|(proj, num)| {
        !proj.is_empty()
            && proj.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            && proj.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && !num.is_empty()
            && num.chars().all(|c| c.is_ascii_digit())
    });
    ensure!(ok, "ticket '{ticket}' is not a ticket key (e.g. ALM-42)");
    Ok(())
}

fn validate_link(link: &str) -> Result<()> {
    ensure!(
        link.starts_with("https://") && !link.chars().any(|c| c.is_control() || c == ' '),
        "link '{link}' must be a single https URL"
    );
    Ok(())
}

/// Local-time human rendering of a typed timestamp (we format it — callers
/// never pass preformatted time strings).
fn format_time(at: DateTime<Utc>) -> String {
    at.with_timezone(&chrono::Local).format("%H:%M on %b %-d").to_string()
}

/// The default (and only, in Phase 2.0) drafting backend.
pub struct TemplatedDraftingBackend;

impl DraftingBackend for TemplatedDraftingBackend {
    fn backend_id(&self) -> &str {
        "templated-v1"
    }

    fn draft(&self, req: &DraftRequest) -> Result<Draft> {
        match req {
            DraftRequest::WorkDoneReply {
                original_subject,
                commit_short,
                completed_at,
                ci_link,
                ticket,
            } => {
                validate_commit_short(commit_short)?;
                if let Some(t) = ticket {
                    validate_ticket(t)?;
                }
                if let Some(l) = ci_link {
                    validate_link(l)?;
                }
                let mut body =
                    format!("Done — fixed in {commit_short} at {}.", format_time(*completed_at));
                if let Some(l) = ci_link {
                    body.push_str(&format!(" CI is green: {l}."));
                }
                if let Some(t) = ticket {
                    body.push_str(&format!(" Closing {t}."));
                }
                Ok(Draft {
                    subject: Some(reply_subject(original_subject)),
                    body,
                    asserts_work_done: true,
                })
            }
            DraftRequest::AckReply { original_subject } => Ok(Draft {
                subject: Some(reply_subject(original_subject)),
                body: "Got it — I'll take a look and follow up shortly.".to_string(),
                asserts_work_done: false,
            }),
            DraftRequest::SlackWorkDonePost { ticket, commit_short, completed_at, ci_link } => {
                validate_ticket(ticket)?;
                validate_commit_short(commit_short)?;
                if let Some(l) = ci_link {
                    validate_link(l)?;
                }
                let mut body = format!(
                    "{ticket} is done — fix landed in {commit_short} at {}.",
                    format_time(*completed_at)
                );
                if let Some(l) = ci_link {
                    body.push_str(&format!(" CI: {l}"));
                }
                Ok(Draft { subject: None, body, asserts_work_done: true })
            }
            DraftRequest::SlackCheckInPost => Ok(Draft {
                subject: None,
                body: "Almanac action-layer live check — this post was proposed, shown \
                       verbatim in a dry-run, and explicitly approved before sending."
                    .to_string(),
                asserts_work_done: false,
            }),
        }
    }
}

impl Draft {
    /// Test/dev constructor for arbitrary drafts (bypasses the templates but
    /// NOT the invariants — E1 still applies at proposal construction).
    pub fn raw_for_tests(subject: Option<String>, body: String, asserts_work_done: bool) -> Self {
        Self { subject, body, asserts_work_done }
    }
}
