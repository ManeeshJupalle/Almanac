//! Phase 2.0 action layer: ActionProposals → approval queue → executors →
//! audit chain (ARCHITECTURE_V2 §4-§7). Propose-then-approve, ALWAYS.
//!
//! Invariants owned by this module tree:
//! - **E1** — a draft asserting a fact about work done carries ≥1 Tier-Hard
//!   evidence ref. Enforced in `ActionProposal::new` (soft-only factual
//!   proposals are refused at construction) and re-checked when a stored
//!   proposal is loaded for the queue.
//! - **E2** — every evidence ref resolves to a stored artifact: well-formed
//!   at construction, composite FK to `source_objects` at rest (v1 grounding
//!   pattern), INNER JOIN on read.
//! - **A1** — the ONLY path to state `approved` is `approve()`, which records
//!   an explicit user approval event in the audit chain in the same
//!   transaction. There is no auto-approve flag, config, or code path.
//! - **A2** — executors (`executors` submodule) are the only code that calls
//!   write endpoints, and they independently re-validate `state == approved`
//!   at execution time.
//! - **L1** — every state transition appends its audit record in the SAME
//!   transaction; executors append `execution_started` (hash of the exact
//!   bytes to send) in a COMMITTED transaction before any network send.

pub mod audit;
pub mod draft;
pub mod executors;

use anyhow::{bail, ensure, Context, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::types::SourceId;
use draft::Draft;

// ------------------------------------------------------------ evidence ----

/// Evidence tier (ARCHITECTURE_V2 §4). Derived from the kind — a source
/// cannot lie about its own tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceTier {
    Hard,
    Soft,
}

impl EvidenceTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvidenceTier::Hard => "hard",
            EvidenceTier::Soft => "soft",
        }
    }
}

/// What kind of artifact backs an evidence ref (ARCHITECTURE_V2 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    GitCommit,
    CiRun,
    JiraEvent,
    Message,
    CalendarEvent,
    WindowObservation,
    ScreenExtract,
}

impl EvidenceKind {
    /// §4 tiering, fixed: hard artifacts are verifiable; observations are not.
    pub fn tier(&self) -> EvidenceTier {
        match self {
            EvidenceKind::GitCommit
            | EvidenceKind::CiRun
            | EvidenceKind::JiraEvent
            | EvidenceKind::Message
            | EvidenceKind::CalendarEvent => EvidenceTier::Hard,
            EvidenceKind::WindowObservation | EvidenceKind::ScreenExtract => EvidenceTier::Soft,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EvidenceKind::GitCommit => "git_commit",
            EvidenceKind::CiRun => "ci_run",
            EvidenceKind::JiraEvent => "jira_event",
            EvidenceKind::Message => "message",
            EvidenceKind::CalendarEvent => "calendar_event",
            EvidenceKind::WindowObservation => "window_observation",
            EvidenceKind::ScreenExtract => "screen_extract",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "git_commit" => EvidenceKind::GitCommit,
            "ci_run" => EvidenceKind::CiRun,
            "jira_event" => EvidenceKind::JiraEvent,
            "message" => EvidenceKind::Message,
            "calendar_event" => EvidenceKind::CalendarEvent,
            "window_observation" => EvidenceKind::WindowObservation,
            "screen_extract" => EvidenceKind::ScreenExtract,
            _ => return None,
        })
    }

    /// Only source-object-backed kinds have an artifact store, so only they
    /// can be persisted (FK-enforced). Phase 2.1 adds Jira issues, Phase 2.2
    /// adds git commits (GitWatcher stores them as `source = git` source
    /// objects). CI/observation stores arrive in later phases.
    pub fn artifact_store_available(&self) -> bool {
        matches!(
            self,
            EvidenceKind::Message
                | EvidenceKind::CalendarEvent
                | EvidenceKind::JiraEvent
                | EvidenceKind::GitCommit
        )
    }
}

/// A reference to a stored artifact backing a proposal (ARCHITECTURE_V2 §7).
/// Deviation from the sketch there: carries `source` because our artifact
/// store (`source_objects`) is keyed by (source, native_id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRef {
    pub kind: EvidenceKind,
    pub source: SourceId,
    pub native_id: String,
    /// Hard evidence links out; soft may not (§7).
    pub deep_link: Option<String>,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

impl EvidenceRef {
    pub fn tier(&self) -> EvidenceTier {
        self.kind.tier()
    }
}

// ----------------------------------------------------------- proposals ----

/// Action kinds. Phase 2.0: gmail_reply, slack_post. Phase 2.1 adds the two
/// Jira write actions (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    GmailReply,
    SlackPost,
    JiraTransition,
    JiraComment,
}

impl ActionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionKind::GmailReply => "gmail_reply",
            ActionKind::SlackPost => "slack_post",
            ActionKind::JiraTransition => "jira_transition",
            ActionKind::JiraComment => "jira_comment",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "gmail_reply" => ActionKind::GmailReply,
            "slack_post" => ActionKind::SlackPost,
            "jira_transition" => ActionKind::JiraTransition,
            "jira_comment" => ActionKind::JiraComment,
            _ => return None,
        })
    }
}

/// Where the action lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionTarget {
    /// Reply to a stored Gmail message (must resolve — FK at rest).
    GmailThread { message_native_id: String },
    /// Post to a Slack channel by id (channels are not source objects; the
    /// grounding requirement lives on the evidence).
    SlackChannel { channel_id: String },
    /// Transition a stored Jira issue via a workflow transition id. The issue
    /// (by key) must resolve to a stored source object (FK at rest); the
    /// transition id is re-validated live at execute time (J6 hazard).
    JiraTransition { issue_key: String, transition_id: String, transition_name: String },
    /// Comment on a stored Jira issue (by key; FK at rest).
    JiraComment { issue_key: String },
}

/// Proposal lifecycle (ARCHITECTURE_V2 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalState {
    Proposed,
    Approved,
    Rejected,
    Expired,
    Executed,
    ExecutionFailed,
}

impl ProposalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProposalState::Proposed => "proposed",
            ProposalState::Approved => "approved",
            ProposalState::Rejected => "rejected",
            ProposalState::Expired => "expired",
            ProposalState::Executed => "executed",
            ProposalState::ExecutionFailed => "execution_failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "proposed" => ProposalState::Proposed,
            "approved" => ProposalState::Approved,
            "rejected" => ProposalState::Rejected,
            "expired" => ProposalState::Expired,
            "executed" => ProposalState::Executed,
            "execution_failed" => ProposalState::ExecutionFailed,
            _ => return None,
        })
    }
}

/// Constructor-time violations. E1/E2 refusals are values, not panics, so the
/// pipeline can surface WHY a proposal was refused.
#[derive(Debug)]
pub enum ProposalViolation {
    /// E1: factual work-done claim without any Tier-Hard evidence.
    SoftOnlyFactualClaim,
    /// E2 (form): evidence ref that cannot possibly resolve.
    MalformedEvidence(String),
    /// A proposal with no evidence at all is an orphan (nothing explains why
    /// it exists) — refused.
    NoEvidence,
    /// kind/target don't belong together.
    TargetMismatch(String),
}

impl std::fmt::Display for ProposalViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposalViolation::SoftOnlyFactualClaim => write!(
                f,
                "E1 violation: draft asserts work was done but no Tier-Hard evidence supports \
                 it — soft observations alone may never back an outward factual claim"
            ),
            ProposalViolation::MalformedEvidence(why) => {
                write!(f, "E2 violation: {why}")
            }
            ProposalViolation::NoEvidence => {
                write!(f, "E2 violation: a proposal must carry at least one evidence ref")
            }
            ProposalViolation::TargetMismatch(why) => write!(f, "invalid proposal: {why}"),
        }
    }
}

impl std::error::Error for ProposalViolation {}

/// A validated, not-yet-persisted proposal. PRIVATE fields: `new()` is the
/// only door, and it enforces E1 + evidence well-formedness (E2's in-memory
/// half; the FK enforces resolution at rest).
#[derive(Debug, Clone)]
pub struct ActionProposal {
    kind: ActionKind,
    target: ActionTarget,
    draft: Draft,
    evidence: Vec<EvidenceRef>,
    backend_id: String,
    /// Templated, human-readable explanation of the correlation that produced
    /// this proposal (Phase 2.2). `None` for dev-seeded proposals. Shown in the
    /// approval UI so the confidence/basis is visible before approval.
    correlation_rationale: Option<String>,
    /// Idempotency key (Phase 2.3): the (ask, work item, evidence) triple. A
    /// re-plan cycle skips queueing a proposal whose key already exists in a
    /// decided-or-open state, so repeated correlation runs never duplicate.
    correlation_key: Option<String>,
}

impl ActionProposal {
    pub fn new(
        kind: ActionKind,
        target: ActionTarget,
        draft: Draft,
        evidence: Vec<EvidenceRef>,
        backend_id: &str,
    ) -> Result<Self, ProposalViolation> {
        match (&kind, &target) {
            (ActionKind::GmailReply, ActionTarget::GmailThread { message_native_id }) => {
                if message_native_id.trim().is_empty() {
                    return Err(ProposalViolation::TargetMismatch(
                        "gmail reply target message id is empty".into(),
                    ));
                }
            }
            (ActionKind::SlackPost, ActionTarget::SlackChannel { channel_id }) => {
                if channel_id.trim().is_empty() {
                    return Err(ProposalViolation::TargetMismatch(
                        "slack post channel id is empty".into(),
                    ));
                }
            }
            (
                ActionKind::JiraTransition,
                ActionTarget::JiraTransition { issue_key, transition_id, .. },
            ) => {
                if issue_key.trim().is_empty() || transition_id.trim().is_empty() {
                    return Err(ProposalViolation::TargetMismatch(
                        "jira transition needs a non-empty issue key and transition id".into(),
                    ));
                }
            }
            (ActionKind::JiraComment, ActionTarget::JiraComment { issue_key }) => {
                if issue_key.trim().is_empty() {
                    return Err(ProposalViolation::TargetMismatch(
                        "jira comment target issue key is empty".into(),
                    ));
                }
            }
            _ => {
                return Err(ProposalViolation::TargetMismatch(format!(
                    "action kind {} does not match its target",
                    kind.as_str()
                )))
            }
        }

        if evidence.is_empty() {
            return Err(ProposalViolation::NoEvidence);
        }
        for e in &evidence {
            if e.native_id.trim().is_empty() {
                return Err(ProposalViolation::MalformedEvidence(
                    "evidence native_id is empty — cannot resolve to a stored artifact".into(),
                ));
            }
            match e.tier() {
                EvidenceTier::Hard => {
                    // A local git commit has no web URL — its stable ref is the
                    // documented `git-local://` form (or an https web URL when
                    // the repo has a remote). Every OTHER hard kind links out
                    // over https, unchanged. This admits a NEW ref form for a
                    // new kind; it does not relax the requirement for any
                    // existing source.
                    let link_ok = match &e.deep_link {
                        Some(l) if e.kind == EvidenceKind::GitCommit => {
                            l.starts_with("https://") || l.starts_with("git-local://")
                        }
                        Some(l) => l.starts_with("https://"),
                        None => false,
                    };
                    if !link_ok {
                        return Err(ProposalViolation::MalformedEvidence(format!(
                            "hard evidence {}:{} must carry a resolvable deep link",
                            e.kind.as_str(),
                            e.native_id
                        )));
                    }
                }
                EvidenceTier::Soft => {}
            }
            // kind ↔ source coherence for source-object-backed kinds.
            let coherent = match e.kind {
                EvidenceKind::Message => {
                    matches!(e.source, SourceId::Gmail | SourceId::Slack)
                }
                EvidenceKind::CalendarEvent => matches!(e.source, SourceId::GoogleCalendar),
                EvidenceKind::JiraEvent => matches!(e.source, SourceId::Jira),
                EvidenceKind::GitCommit => matches!(e.source, SourceId::Git),
                _ => true,
            };
            if !coherent {
                return Err(ProposalViolation::MalformedEvidence(format!(
                    "evidence kind {} cannot come from source {}",
                    e.kind.as_str(),
                    e.source
                )));
            }
        }

        // E1 — the invariant this layer exists for.
        if draft.asserts_work_done && !evidence.iter().any(|e| e.tier() == EvidenceTier::Hard) {
            return Err(ProposalViolation::SoftOnlyFactualClaim);
        }

        Ok(Self {
            kind,
            target,
            draft,
            evidence,
            backend_id: backend_id.to_string(),
            correlation_rationale: None,
            correlation_key: None,
        })
    }

    /// Attach a templated correlation rationale (the correlation Proposer sets
    /// this). Purely additive metadata — does not affect E1/E2 validation.
    pub fn with_rationale(mut self, rationale: impl Into<String>) -> Self {
        self.correlation_rationale = Some(rationale.into());
        self
    }

    /// Attach the idempotency key (the correlation Proposer sets this).
    pub fn with_correlation_key(mut self, key: impl Into<String>) -> Self {
        self.correlation_key = Some(key.into());
        self
    }

    pub fn correlation_key(&self) -> Option<&str> {
        self.correlation_key.as_deref()
    }

    pub fn kind(&self) -> ActionKind {
        self.kind
    }
    pub fn target(&self) -> &ActionTarget {
        &self.target
    }
    pub fn draft(&self) -> &Draft {
        &self.draft
    }
    pub fn evidence(&self) -> &[EvidenceRef] {
        &self.evidence
    }
    pub fn correlation_rationale(&self) -> Option<&str> {
        self.correlation_rationale.as_deref()
    }
}

// -------------------------------------------------------- persistence -----

/// Evidence as read back (with the artifact's stored deep link from the
/// E2-enforcing INNER JOIN, for display).
#[derive(Debug, Clone)]
pub struct StoredEvidence {
    pub tier: EvidenceTier,
    pub kind: EvidenceKind,
    pub source: SourceId,
    pub native_id: String,
    pub deep_link: Option<String>,
    pub observed_at: String,
    /// The referenced artifact's own deep link (from source_objects).
    pub artifact_deep_link: String,
}

/// A proposal as read back from the store.
#[derive(Debug, Clone)]
pub struct StoredProposal {
    pub id: i64,
    pub kind: ActionKind,
    pub state: ProposalState,
    pub target: ActionTarget,
    pub draft_subject: Option<String>,
    pub draft_body: String,
    pub asserts_work_done: bool,
    pub backend_id: String,
    pub created_at: String,
    pub expires_at: String,
    pub receipt_json: Option<String>,
    /// Templated correlation basis/confidence (Phase 2.2), if this proposal came
    /// from the correlation engine. Shown in the approval UI.
    pub correlation_rationale: Option<String>,
    pub evidence: Vec<StoredEvidence>,
}

/// Persist a validated proposal + its evidence and audit it, atomically.
/// E2 at rest: the composite FKs make an unresolvable target or evidence ref
/// FAIL LOUDLY here. Returns the new proposal id.
pub fn insert_proposal(
    conn: &mut Connection,
    proposal: &ActionProposal,
    actor: &str,
    ttl: chrono::Duration,
) -> Result<i64> {
    for e in proposal.evidence() {
        ensure!(
            e.kind.artifact_store_available(),
            "evidence kind {} has no artifact store yet and cannot be persisted \
             (its store + FK arrive with the phase that introduces its observer)",
            e.kind.as_str()
        );
    }

    // Target mapping. Gmail message + Jira issue targets both resolve through
    // the composite (target_source, target_native_id) FK to source_objects, so
    // an unstored reply/issue target FAILS LOUDLY here (E2).
    let (target_source, target_native_id, target_channel, transition_id, transition_name) =
        match proposal.target() {
            ActionTarget::GmailThread { message_native_id } => {
                (Some("gmail"), Some(message_native_id.as_str()), None, None, None)
            }
            ActionTarget::SlackChannel { channel_id } => {
                (None, None, Some(channel_id.as_str()), None, None)
            }
            ActionTarget::JiraTransition { issue_key, transition_id, transition_name } => (
                Some("jira"),
                Some(issue_key.as_str()),
                None,
                Some(transition_id.as_str()),
                Some(transition_name.as_str()),
            ),
            ActionTarget::JiraComment { issue_key } => {
                (Some("jira"), Some(issue_key.as_str()), None, None, None)
            }
        };
    let expires_at = (chrono::Utc::now() + ttl).to_rfc3339();

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO action_proposals
             (kind, state, target_source, target_native_id, target_channel,
              target_transition_id, target_transition_name,
              draft_subject, draft_body, asserts_work_done, backend_id, expires_at,
              correlation_rationale, correlation_key)
         VALUES (?1, 'proposed', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        (
            proposal.kind().as_str(),
            target_source,
            target_native_id,
            target_channel,
            transition_id,
            transition_name,
            proposal.draft().subject.as_deref(),
            &proposal.draft().body,
            proposal.draft().asserts_work_done as i64,
            &proposal.backend_id,
            &expires_at,
            proposal.correlation_rationale(),
            proposal.correlation_key(),
        ),
    )
    .context("persisting proposal (an FK failure here means the reply/issue target is not a stored source object)")?;
    let id = tx.last_insert_rowid();

    for (position, e) in proposal.evidence().iter().enumerate() {
        tx.execute(
            "INSERT INTO proposal_evidence
                 (proposal_id, position, tier, kind, source, native_id, deep_link, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                id,
                position as i64,
                e.tier().as_str(),
                e.kind.as_str(),
                e.source.to_string(),
                &e.native_id,
                e.deep_link.as_deref(),
                e.observed_at.to_rfc3339(),
            ),
        )
        .context("persisting evidence (an FK failure here means unresolvable evidence — E2)")?;
    }

    audit::append(
        &tx,
        actor,
        "proposed",
        Some(id),
        &audit::sha256_hex(proposal.draft().body.as_bytes()),
    )?;
    tx.commit()?;
    Ok(id)
}

/// Load one proposal with its evidence (INNER JOIN on source_objects — the
/// render-layer half of E2) and re-check E1 before it may be shown/queued.
pub fn load_proposal(conn: &Connection, id: i64) -> Result<StoredProposal> {
    let row = conn
        .query_row(
            "SELECT kind, state, target_source, target_native_id, target_channel,
                    target_transition_id, target_transition_name,
                    draft_subject, draft_body, asserts_work_done, backend_id,
                    created_at, expires_at, receipt_json, correlation_rationale
             FROM action_proposals WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, Option<String>>(14)?,
                ))
            },
        )
        .optional()?
        .with_context(|| format!("no proposal with id {id}"))?;

    let (
        kind,
        state,
        _target_source,
        target_native_id,
        target_channel,
        target_transition_id,
        target_transition_name,
        draft_subject,
        draft_body,
        asserts_work_done,
        backend_id,
        created_at,
        expires_at,
        receipt_json,
        correlation_rationale,
    ) = row;

    let kind = ActionKind::parse(&kind).with_context(|| format!("unknown action kind '{kind}'"))?;
    let state =
        ProposalState::parse(&state).with_context(|| format!("unknown proposal state '{state}'"))?;
    let target = match kind {
        ActionKind::GmailReply => ActionTarget::GmailThread {
            message_native_id: target_native_id
                .context("gmail_reply proposal missing target_native_id")?,
        },
        ActionKind::SlackPost => ActionTarget::SlackChannel {
            channel_id: target_channel.context("slack_post proposal missing target_channel")?,
        },
        ActionKind::JiraTransition => ActionTarget::JiraTransition {
            issue_key: target_native_id
                .context("jira_transition proposal missing target issue key")?,
            transition_id: target_transition_id
                .context("jira_transition proposal missing target_transition_id")?,
            transition_name: target_transition_name.unwrap_or_default(),
        },
        ActionKind::JiraComment => ActionTarget::JiraComment {
            issue_key: target_native_id
                .context("jira_comment proposal missing target issue key")?,
        },
    };

    let mut stmt = conn.prepare(
        "SELECT pe.tier, pe.kind, pe.source, pe.native_id, pe.deep_link, pe.observed_at,
                so.deep_link
         FROM proposal_evidence pe
         JOIN source_objects so ON so.source = pe.source AND so.native_id = pe.native_id
         WHERE pe.proposal_id = ?1
         ORDER BY pe.position ASC",
    )?;
    let evidence: Vec<StoredEvidence> = stmt
        .query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(tier, kind, source, native_id, deep_link, observed_at, artifact_deep_link)| {
            let kind = EvidenceKind::parse(&kind)
                .with_context(|| format!("unknown evidence kind '{kind}'"))?;
            ensure!(
                tier == kind.tier().as_str(),
                "stored evidence tier '{tier}' contradicts kind '{}' — tampered row?",
                kind.as_str()
            );
            Ok(StoredEvidence {
                tier: kind.tier(),
                kind,
                source: SourceId::parse(&source)
                    .with_context(|| format!("unknown evidence source '{source}'"))?,
                native_id,
                deep_link,
                observed_at,
                artifact_deep_link,
            })
        })
        .collect::<Result<_>>()?;

    let proposal = StoredProposal {
        id,
        kind,
        state,
        target,
        draft_subject,
        draft_body,
        asserts_work_done: asserts_work_done != 0,
        backend_id,
        created_at,
        expires_at,
        receipt_json,
        correlation_rationale,
        evidence,
    };

    // Pipeline re-check (E1 + E2) before anything downstream may use it.
    ensure!(
        !proposal.evidence.is_empty(),
        "proposal {id} has no resolvable evidence — refusing to load it (E2)"
    );
    ensure!(
        !proposal.asserts_work_done
            || proposal.evidence.iter().any(|e| e.tier == EvidenceTier::Hard),
        "proposal {id} asserts work done without hard evidence — refusing to load it (E1)"
    );
    Ok(proposal)
}

/// All proposals, newest first (expiry-swept first so the queue never shows a
/// stale 'proposed' row).
pub fn list_proposals(conn: &mut Connection) -> Result<Vec<StoredProposal>> {
    sweep_expired(conn)?;
    let ids: Vec<i64> = {
        let mut stmt =
            conn.prepare("SELECT id FROM action_proposals ORDER BY id DESC LIMIT 100")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        ids
    };
    ids.into_iter().map(|id| load_proposal(conn, id)).collect()
}

// ------------------------------------------------------ state machine -----

/// Compare-and-swap state transition + audit record, one transaction (L1).
/// The CAS makes illegal transitions hard errors — there is no way to move a
/// proposal whose current state is not `from`.
fn transition(
    conn: &mut Connection,
    id: i64,
    from: ProposalState,
    to: ProposalState,
    actor: &str,
    event: &str,
    payload_hash: &str,
) -> Result<i64> {
    let tx = conn.transaction()?;
    let n = tx.execute(
        "UPDATE action_proposals SET state = ?3 WHERE id = ?1 AND state = ?2",
        (id, from.as_str(), to.as_str()),
    )?;
    if n != 1 {
        let current: Option<String> = tx
            .query_row("SELECT state FROM action_proposals WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        match current {
            Some(s) => bail!(
                "illegal transition for proposal {id}: {} -> {} requires state '{}' but it is '{s}'",
                from.as_str(),
                to.as_str(),
                from.as_str()
            ),
            None => bail!("no proposal with id {id}"),
        }
    }
    let seq = audit::append(&tx, actor, event, Some(id), payload_hash)?;
    tx.commit()?;
    Ok(seq)
}

/// THE user approval event (A1). `actor` names who clicked (always a human
/// surface — the UI passes "user"; tests simulate the same event).
pub fn approve(conn: &mut Connection, id: i64, actor: &str) -> Result<i64> {
    transition(
        conn,
        id,
        ProposalState::Proposed,
        ProposalState::Approved,
        actor,
        "approved",
        &audit::sha256_hex(b""),
    )
}

pub fn reject(conn: &mut Connection, id: i64, actor: &str) -> Result<i64> {
    transition(
        conn,
        id,
        ProposalState::Proposed,
        ProposalState::Rejected,
        actor,
        "rejected",
        &audit::sha256_hex(b""),
    )
}

/// TTL sweep: 'proposed' rows past expires_at → 'expired', each audited.
pub fn sweep_expired(conn: &mut Connection) -> Result<usize> {
    let now = chrono::Utc::now().to_rfc3339();
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM action_proposals
             WHERE state = 'proposed' AND datetime(expires_at) <= datetime(?1)",
        )?;
        let ids = stmt
            .query_map([&now], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        ids
    };
    let mut swept = 0;
    for id in ids {
        transition(
            conn,
            id,
            ProposalState::Proposed,
            ProposalState::Expired,
            "system",
            "expired",
            &audit::sha256_hex(b""),
        )?;
        swept += 1;
    }
    Ok(swept)
}

/// Execution claim: called by executors INSIDE the gated execute path, after
/// re-validating state == approved. Sets `execution_claimed_at` exactly once
/// (double-execute guard) and appends the `execution_started` record — the
/// L1 "audited before the side effect" step. Committed before any send.
pub(crate) fn claim_execution(
    conn: &mut Connection,
    id: i64,
    api_body_hash: &str,
) -> Result<i64> {
    let tx = conn.transaction()?;
    let n = tx.execute(
        "UPDATE action_proposals
         SET execution_claimed_at = datetime('now')
         WHERE id = ?1 AND state = 'approved' AND execution_claimed_at IS NULL",
        [id],
    )?;
    ensure!(
        n == 1,
        "refusing to execute proposal {id}: not in state 'approved' or already claimed \
         by a previous execution attempt"
    );
    let seq = audit::append(&tx, "executor", "execution_started", Some(id), api_body_hash)?;
    tx.commit()?;
    Ok(seq)
}

/// Finalize an execution attempt (executor-only): approved → executed /
/// execution_failed, with the receipt or error audited in the same
/// transaction.
pub(crate) fn finalize_execution(
    conn: &mut Connection,
    id: i64,
    receipt_json: Option<&str>,
    error: Option<&str>,
) -> Result<i64> {
    let (to, event, payload) = match (receipt_json, error) {
        (Some(r), None) => (ProposalState::Executed, "executed", r.as_bytes().to_vec()),
        (None, Some(e)) => {
            (ProposalState::ExecutionFailed, "execution_failed", e.as_bytes().to_vec())
        }
        _ => bail!("finalize_execution needs exactly one of receipt or error"),
    };
    let tx = conn.transaction()?;
    let n = tx.execute(
        "UPDATE action_proposals SET state = ?2, receipt_json = ?3 WHERE id = ?1 AND state = 'approved'",
        (id, to.as_str(), receipt_json),
    )?;
    ensure!(n == 1, "cannot finalize execution of proposal {id}: not in state 'approved'");
    let seq =
        audit::append(&tx, "executor", event, Some(id), &audit::sha256_hex(&payload))?;
    tx.commit()?;
    Ok(seq)
}

// ------------------------------------------------ idempotency (2.3) -------

/// States in which an existing proposal BLOCKS re-queueing the same correlation
/// (its `correlation_key`). Expired is deliberately absent — a lapsed proposal
/// may be re-surfaced by a later re-plan; `rejected` is present so a human's
/// rejection is never resurrected.
const BLOCKING_STATES: &str = "'proposed', 'approved', 'executed', 'execution_failed', 'rejected'";

/// True if a proposal with this correlation key already exists in a
/// decided-or-open state (Phase 2.3 idempotency). Re-planning uses this to
/// avoid duplicating a proposal it (or the user) already acted on.
pub fn correlation_key_blocking(conn: &Connection, correlation_key: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM action_proposals
             WHERE correlation_key = ?1 AND state IN ({BLOCKING_STATES})"
        ),
        [correlation_key],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Audit a re-plan cycle (Phase 2.3): a traceable, actor-attributed record that
/// the plan was regenerated — no silent background mutation. `summary` names the
/// reason and outcome; its hash is chained (the same discipline as every other
/// audited action). Not tied to a single proposal (`proposal_id` is NULL).
pub fn record_replan(conn: &mut Connection, actor: &str, summary: &str) -> Result<i64> {
    let tx = conn.transaction()?;
    let seq = audit::append(&tx, actor, "replanned", None, &audit::sha256_hex(summary.as_bytes()))?;
    tx.commit()?;
    Ok(seq)
}

#[cfg(test)]
mod tests {
    use super::draft::Draft;
    use super::*;
    use crate::types::{ProvenanceRef, RawContent, SourceObject};

    fn test_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        crate::db::migrate(&mut conn).unwrap();
        (dir, conn)
    }

    fn seeded_approved_proposal(conn: &mut Connection) -> i64 {
        let obj = SourceObject {
            provenance: ProvenanceRef {
                source: SourceId::Slack,
                native_id: "C1:1.100".into(),
                deep_link: "https://example.slack.com/archives/C1/p1100".into(),
            },
            raw: RawContent::new(serde_json::json!({"type": "message", "ts": "1.100"})),
            occurred_at: chrono::Utc::now(),
        };
        crate::db::insert_source_object(conn, &obj).unwrap();
        let proposal = ActionProposal::new(
            ActionKind::SlackPost,
            ActionTarget::SlackChannel { channel_id: "C1".into() },
            Draft::raw_for_tests(None, "status check".into(), false),
            vec![EvidenceRef {
                kind: EvidenceKind::Message,
                source: SourceId::Slack,
                native_id: "C1:1.100".into(),
                deep_link: Some("https://example.slack.com/archives/C1/p1100".into()),
                observed_at: chrono::Utc::now(),
            }],
            "templated-v1",
        )
        .unwrap();
        let id = insert_proposal(conn, &proposal, "test", chrono::Duration::hours(24)).unwrap();
        approve(conn, id, "user").unwrap();
        id
    }

    /// L1: the execution claim writes the `execution_started` audit record —
    /// hashing the exact bytes that will be sent — in a COMMITTED transaction
    /// before any network side effect, and it is single-shot (double-execute
    /// guard). This is the only path executors have to the network.
    #[test]
    fn l1_execution_claim_audits_before_send_and_is_single_shot() {
        let (_d, mut conn) = test_db();
        let id = seeded_approved_proposal(&mut conn);
        let body_hash = audit::sha256_hex(b"exact-api-bytes");

        let seq = claim_execution(&mut conn, id, &body_hash).unwrap();
        let tail = audit::tail(&conn, 1).unwrap();
        assert_eq!(tail[0].seq, seq);
        assert_eq!(tail[0].event, "execution_started");
        assert_eq!(tail[0].payload_hash, body_hash, "the audited hash IS the send-bytes hash");

        // Second claim (double execution) is refused.
        let err = claim_execution(&mut conn, id, &body_hash).unwrap_err();
        assert!(format!("{err:#}").contains("already claimed"), "{err:#}");

        // Finalize as executed; the receipt is audited in the same transaction.
        let final_seq = finalize_execution(&mut conn, id, Some("{\"ok\":true}"), None).unwrap();
        assert_eq!(final_seq, seq + 1);
        let p = load_proposal(&conn, id).unwrap();
        assert_eq!(p.state, ProposalState::Executed);
        assert_eq!(p.receipt_json.as_deref(), Some("{\"ok\":true}"));
        // Terminal: cannot finalize twice.
        assert!(finalize_execution(&mut conn, id, Some("{}"), None).is_err());
        audit::verify_chain(&conn).unwrap();
    }

    /// Claim on a non-approved proposal is refused (belt to the executors'
    /// braces — even crate-internal code cannot claim without approval).
    #[test]
    fn execution_claim_requires_approved_state() {
        let (_d, mut conn) = test_db();
        let obj = SourceObject {
            provenance: ProvenanceRef {
                source: SourceId::Slack,
                native_id: "C1:1.200".into(),
                deep_link: "https://example.slack.com/archives/C1/p1200".into(),
            },
            raw: RawContent::new(serde_json::json!({"type": "message", "ts": "1.200"})),
            occurred_at: chrono::Utc::now(),
        };
        crate::db::insert_source_object(&conn, &obj).unwrap();
        let proposal = ActionProposal::new(
            ActionKind::SlackPost,
            ActionTarget::SlackChannel { channel_id: "C1".into() },
            Draft::raw_for_tests(None, "x".into(), false),
            vec![EvidenceRef {
                kind: EvidenceKind::Message,
                source: SourceId::Slack,
                native_id: "C1:1.200".into(),
                deep_link: Some("https://example.slack.com/archives/C1/p1200".into()),
                observed_at: chrono::Utc::now(),
            }],
            "templated-v1",
        )
        .unwrap();
        let id =
            insert_proposal(&mut conn, &proposal, "test", chrono::Duration::hours(24)).unwrap();
        // Still 'proposed' — no claim possible.
        assert!(claim_execution(&mut conn, id, &audit::sha256_hex(b"x")).is_err());
    }

    /// Phase 2.1 hazard (J6): when a transition is no longer valid at execute
    /// time, the executor finalizes execution_failed and applies NOTHING — no
    /// `execution_started` claim (so no write was attempted), state is
    /// execution_failed, and the failure is audited. This exercises the exact
    /// finalize path the executor takes on its invalid-transition branch.
    #[test]
    fn transition_invalidated_at_execute_fails_safely_without_applying() {
        let (_d, mut conn) = test_db();
        let obj = SourceObject {
            provenance: ProvenanceRef {
                source: SourceId::Jira,
                native_id: "ALM-1".into(),
                deep_link: "https://x.atlassian.net/browse/ALM-1".into(),
            },
            raw: RawContent::new(serde_json::json!({"key": "ALM-1"})),
            occurred_at: chrono::Utc::now(),
        };
        crate::db::insert_source_object(&conn, &obj).unwrap();
        let proposal = ActionProposal::new(
            ActionKind::JiraTransition,
            ActionTarget::JiraTransition {
                issue_key: "ALM-1".into(),
                transition_id: "999".into(),
                transition_name: "Done".into(),
            },
            Draft::raw_for_tests(None, "Move ALM-1 to Done.".into(), false),
            vec![EvidenceRef {
                kind: EvidenceKind::JiraEvent,
                source: SourceId::Jira,
                native_id: "ALM-1".into(),
                deep_link: Some("https://x.atlassian.net/browse/ALM-1".into()),
                observed_at: chrono::Utc::now(),
            }],
            "templated-v1",
        )
        .unwrap();
        let id =
            insert_proposal(&mut conn, &proposal, "test", chrono::Duration::hours(24)).unwrap();
        approve(&mut conn, id, "user").unwrap();

        // The live-offered set (999 is absent) — this is the decision the
        // executor makes before any write.
        let offered = ["11".to_string(), "21".to_string()];
        assert!(!offered.contains(&"999".to_string()), "precondition: 999 not offered");

        // Executor's invalid-transition branch: finalize_execution(error) with
        // NO preceding claim_execution.
        let seq = finalize_execution(&mut conn, id, None, "transition 999 no longer valid".into())
            .unwrap();

        let stored = load_proposal(&conn, id).unwrap();
        assert_eq!(stored.state, ProposalState::ExecutionFailed);

        // No write was attempted: execution_claimed_at is still NULL.
        let claimed: Option<String> = conn
            .query_row(
                "SELECT execution_claimed_at FROM action_proposals WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(claimed.is_none(), "no execution should have been claimed (nothing applied)");

        // Audited as execution_failed, with NO execution_started record.
        let events = audit_events(&conn);
        assert!(events.contains(&"execution_failed".to_string()));
        assert!(
            !events.contains(&"execution_started".to_string()),
            "a failed re-validation must not emit execution_started (no partial apply)"
        );
        // Chain still verifies after all of this.
        audit::verify_chain(&conn).unwrap();
        let _ = seq;
    }

    fn audit_events(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("SELECT event FROM audit_records ORDER BY seq ASC").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }
}
