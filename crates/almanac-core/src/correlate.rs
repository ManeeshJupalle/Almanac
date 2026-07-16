//! Correlation engine (Phase 2.2): bind **asks** (Gmail/Slack messages that
//! reference a work item by issue key) ↔ **work items** (Jira issues) ↔
//! **evidence** (git commits) into `WorkThread`s, then turn the resolved ones
//! into `ActionProposal`s through the Phase 2.0 machinery (E1/E2 enforced there).
//!
//! **Deterministic, never fabricated.** Binding is by EXACT issue-key match
//! (word-boundary safe: `ALM-1` matches; `PSALM-1` and `ALM-10` do NOT) plus an
//! explicit commit-sha reference inside a Jira comment. There is no fuzzy /
//! embedding match in this phase — a below-signal item forms no thread (a
//! semantic tiebreaker is the one thing that could invent a link, which the
//! hard constraint forbids; see PAYLOAD_CORRECTIONS.md "Correlation" note).
//!
//! **Confidence / eligibility (explicit).** A commit counts as evidence FOR an
//! item iff the item's key appears in the commit subject, body, or a branch
//! name, OR a NON-Almanac Jira comment on the item references the commit sha
//! (J15: Almanac's own comments — `accountId == /myself` — are excluded so it
//! never corroborates its own prior output). A `WorkThread` is *Full* (surface-
//! able, eligible to propose) only when ask + item + ≥1 commit are ALL present;
//! anything less is *PartialNeedsConfirmation* and produces NO proposal.
//!
//! **A2 / read-only.** Nothing here writes to Gmail/Slack/Jira/git. The Proposer
//! only builds local queue rows; a human still approves each one.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::Value;

use crate::act::draft::{DraftRequest, DraftingBackend};
use crate::act::{ActionKind, ActionProposal, ActionTarget, EvidenceKind, EvidenceRef};
use crate::types::{ProvenanceRef, SourceId};

// --------------------------------------------------------- issue keys ------

/// Extract Jira-style issue keys from free text, word-boundary safe.
///
/// `\b[A-Z][A-Z0-9]+-\d+\b`: the project part is greedy, so `PSALM-1` extracts
/// as `PSALM-1` (never `ALM-1`), and `ALM-10` as `ALM-10` (never `ALM-1`).
/// Membership is then exact — `keys.contains("ALM-1")`.
pub fn issue_keys(text: &str) -> BTreeSet<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\b[A-Z][A-Z0-9]+-[0-9]+\b").unwrap());
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

fn mentions_key(text: &str, key: &str) -> bool {
    issue_keys(text).contains(key)
}

// ------------------------------------------------------------- inputs ------

/// An ask: something a person asked, from Gmail/Slack. `text` is scanned for
/// keys; `subject` (Gmail only) threads the reply; `asker` (Gmail From / Slack
/// user) feeds the prioritizer's asker-weight factor (Phase 2.3).
#[derive(Debug, Clone)]
pub struct AskInput {
    pub provenance: ProvenanceRef,
    pub subject: Option<String>,
    pub text: String,
    pub asker: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

/// A commit surfaced by the GitWatcher (Tier-Hard evidence candidate).
#[derive(Debug, Clone)]
pub struct CommitInput {
    pub sha: String,
    pub short_sha: String,
    pub subject: String,
    pub body: String,
    pub branches: Vec<String>,
    pub deep_link: String,
    pub committed_at: DateTime<Utc>,
}

/// One Jira comment (for the explicit-link + J15 exclusion path).
#[derive(Debug, Clone)]
pub struct IssueComment {
    pub author_account_id: String,
    pub body_text: String,
}

/// A work item: a Jira issue.
#[derive(Debug, Clone)]
pub struct WorkItemInput {
    pub key: String,
    pub summary: String,
    pub deep_link: String,
    pub occurred_at: DateTime<Utc>,
    pub comments: Vec<IssueComment>,
}

// ------------------------------------------------------------ outputs ------

/// Where an issue key (or sha reference) was found — drives the rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchSite {
    Subject,
    Body,
    Branch,
    Comment,
}

impl MatchSite {
    fn as_str(&self) -> &'static str {
        match self {
            MatchSite::Subject => "commit subject",
            MatchSite::Body => "commit body",
            MatchSite::Branch => "branch name",
            MatchSite::Comment => "a Jira comment",
        }
    }
}

/// A commit bound to a work item, with the site(s) that justified the binding.
#[derive(Debug, Clone)]
pub struct EvidenceCommit {
    pub commit: CommitInput,
    pub sites: Vec<MatchSite>,
}

/// Eligibility of a correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// ask + item + ≥1 commit — surface-able, eligible to propose.
    Full,
    /// only some legs present — surfaced as "possible match, needs your
    /// confirmation" at most; never auto-proposed.
    PartialNeedsConfirmation,
}

impl Confidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Confidence::Full => "high",
            Confidence::PartialNeedsConfirmation => "possible (needs confirmation)",
        }
    }
}

/// One correlated unit of work.
#[derive(Debug, Clone)]
pub struct WorkThread {
    pub item: WorkItemInput,
    pub asks: Vec<AskInput>,
    pub evidence: Vec<EvidenceCommit>,
    pub confidence: Confidence,
}

impl WorkThread {
    pub fn is_full(&self) -> bool {
        matches!(self.confidence, Confidence::Full)
    }

    /// Distinct match sites across all evidence commits (for the rationale).
    fn site_summary(&self) -> String {
        let mut seen: Vec<MatchSite> = Vec::new();
        for ec in &self.evidence {
            for s in &ec.sites {
                if !seen.contains(s) {
                    seen.push(*s);
                }
            }
        }
        seen.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
    }
}

// ------------------------------------------------------------- engine ------

pub struct CorrelationEngine {
    /// Almanac's own Jira accountId (`/myself`), used to exclude its own
    /// comments from the explicit-link signal (J15). `None` disables that one
    /// signal (key-in-commit binding is unaffected).
    self_account_id: Option<String>,
}

impl CorrelationEngine {
    pub fn new(self_account_id: Option<String>) -> Self {
        Self { self_account_id }
    }

    /// Correlate asks/items/commits into threads. Deterministic and pure — the
    /// unit under the fixture tests.
    pub fn correlate(
        &self,
        asks: &[AskInput],
        items: &[WorkItemInput],
        commits: &[CommitInput],
    ) -> Vec<WorkThread> {
        let mut threads = Vec::new();
        for item in items {
            let key = item.key.as_str();

            let bound_asks: Vec<AskInput> =
                asks.iter().filter(|a| mentions_key(&a.text, key)).cloned().collect();

            // J15: exclude Almanac's own comments from the explicit-link signal.
            let human_comments: Vec<&IssueComment> = item
                .comments
                .iter()
                .filter(|c| self.self_account_id.as_deref() != Some(c.author_account_id.as_str()))
                .collect();

            let mut evidence: Vec<EvidenceCommit> = Vec::new();
            for commit in commits {
                let mut sites = Vec::new();
                if mentions_key(&commit.subject, key) {
                    sites.push(MatchSite::Subject);
                }
                if mentions_key(&commit.body, key) {
                    sites.push(MatchSite::Body);
                }
                if commit.branches.iter().any(|b| mentions_key(b, key)) {
                    sites.push(MatchSite::Branch);
                }
                if human_comments.iter().any(|c| comment_references_commit(&c.body_text, commit)) {
                    sites.push(MatchSite::Comment);
                }
                if !sites.is_empty() {
                    evidence.push(EvidenceCommit { commit: commit.clone(), sites });
                }
            }

            if bound_asks.is_empty() && evidence.is_empty() {
                continue; // nothing to correlate — no thread (no guessing)
            }
            let confidence = if !bound_asks.is_empty() && !evidence.is_empty() {
                Confidence::Full
            } else {
                Confidence::PartialNeedsConfirmation
            };
            threads.push(WorkThread { item: item.clone(), asks: bound_asks, evidence, confidence });
        }
        threads
    }
}

/// A non-Almanac comment that names a commit's sha explicitly links it to the
/// issue. Require a ≥7-char short-sha (or the full sha) to avoid coincidences.
fn comment_references_commit(comment_text: &str, commit: &CommitInput) -> bool {
    (commit.short_sha.len() >= 7 && comment_text.contains(&commit.short_sha))
        || (commit.sha.len() >= 7 && comment_text.contains(&commit.sha))
}

// ----------------------------------------------------------- proposer ------

/// Turn a resolved WorkThread into proposals through the Phase 2.0 machinery.
/// Only *Full* threads propose; each bound ask yields one reply/post asserting
/// the work is done, citing every bound commit (Tier-Hard) + the Jira issue as
/// evidence. E1/E2 are enforced by `ActionProposal::new`. Returns an empty vec
/// for partial threads (surfaced elsewhere, never auto-proposed).
pub fn propose_from_thread(
    thread: &WorkThread,
    backend: &dyn DraftingBackend,
) -> Result<Vec<ActionProposal>> {
    if !thread.is_full() {
        return Ok(Vec::new());
    }
    // Representative commit = most recent bound commit (drives the draft slots).
    let rep = thread
        .evidence
        .iter()
        .map(|ec| &ec.commit)
        .max_by_key(|c| c.committed_at)
        .context("full thread has no evidence commit")?;

    // Evidence chain: every bound commit (Tier-Hard) + the work item itself.
    let mut evidence: Vec<EvidenceRef> = thread
        .evidence
        .iter()
        .map(|ec| EvidenceRef {
            kind: EvidenceKind::GitCommit,
            source: SourceId::Git,
            native_id: ec.commit.sha.clone(),
            deep_link: Some(ec.commit.deep_link.clone()),
            observed_at: ec.commit.committed_at,
        })
        .collect();
    evidence.push(EvidenceRef {
        kind: EvidenceKind::JiraEvent,
        source: SourceId::Jira,
        native_id: thread.item.key.clone(),
        deep_link: Some(thread.item.deep_link.clone()),
        observed_at: thread.item.occurred_at,
    });

    let sites = thread.site_summary();
    let mut proposals = Vec::new();
    for ask in &thread.asks {
        let (kind, target, req) = match ask.provenance.source {
            SourceId::Gmail => (
                ActionKind::GmailReply,
                ActionTarget::GmailThread {
                    message_native_id: ask.provenance.native_id.clone(),
                },
                DraftRequest::WorkDoneReply {
                    original_subject: ask.subject.clone().unwrap_or_default(),
                    commit_short: rep.short_sha.clone(),
                    completed_at: rep.committed_at,
                    ci_link: None,
                    ticket: Some(thread.item.key.clone()),
                },
            ),
            SourceId::Slack => {
                // Slack native_id is "channel:ts" — the channel is the post target.
                let channel = ask.provenance.native_id.split(':').next().unwrap_or("").to_string();
                (
                    ActionKind::SlackPost,
                    ActionTarget::SlackChannel { channel_id: channel },
                    DraftRequest::SlackWorkDonePost {
                        ticket: thread.item.key.clone(),
                        commit_short: rep.short_sha.clone(),
                        completed_at: rep.committed_at,
                        ci_link: None,
                    },
                )
            }
            // Only Gmail/Slack asks produce a reply this phase.
            _ => continue,
        };

        let rationale = format!(
            "Correlation {key} · confidence {conf} (deterministic issue-key match): \
             reply to {src} ask ↔ Jira {key} ↔ {n} commit(s) [{short} …] found in {sites}. \
             {n} commit(s) cited as Tier-Hard evidence; no semantic guessing.",
            key = thread.item.key,
            conf = thread.confidence.as_str(),
            src = ask.provenance.source,
            n = thread.evidence.len(),
            short = rep.short_sha,
            sites = sites,
        );

        // Idempotency key (Phase 2.3): the (ask, work item, evidence) triple —
        // the reply-target message, the issue key, and the sorted commit shas.
        let mut shas: Vec<String> =
            thread.evidence.iter().map(|ec| ec.commit.sha.clone()).collect();
        shas.sort();
        let correlation_key = format!(
            "{}:{}|{}|{}",
            ask.provenance.source,
            ask.provenance.native_id,
            thread.item.key,
            shas.join(",")
        );

        let draft = backend.draft(&req)?;
        let proposal = ActionProposal::new(kind, target, draft, evidence.clone(), backend.backend_id())?
            .with_rationale(rationale)
            .with_correlation_key(correlation_key);
        proposals.push(proposal);
    }
    Ok(proposals)
}

/// Outcome of an idempotent queueing pass.
#[derive(Debug, Clone, Default)]
pub struct QueueOutcome {
    pub queued: usize,
    /// Skipped as duplicates (an equivalent proposal already exists open/decided).
    pub skipped: usize,
    pub ids: Vec<i64>,
}

/// Queue proposals for the given threads IDEMPOTENTLY (Phase 2.3): a proposal
/// whose (ask, work item, evidence) key already exists in a decided-or-open
/// state is skipped, so repeated correlation/re-plan runs never duplicate and a
/// rejected proposal is never resurrected. Only *Full* threads produce proposals.
pub fn queue_proposals(
    conn: &mut Connection,
    threads: &[WorkThread],
    backend: &dyn DraftingBackend,
    actor: &str,
    ttl: chrono::Duration,
) -> Result<QueueOutcome> {
    let mut outcome = QueueOutcome::default();
    for thread in threads {
        for proposal in propose_from_thread(thread, backend)? {
            if let Some(key) = proposal.correlation_key() {
                if crate::act::correlation_key_blocking(conn, key)? {
                    outcome.skipped += 1;
                    continue;
                }
            }
            let id = crate::act::insert_proposal(conn, &proposal, actor, ttl)?;
            outcome.ids.push(id);
            outcome.queued += 1;
        }
    }
    Ok(outcome)
}

// -------------------------------------------------------- db loaders -------

/// Git commits stored as `source = git` source objects → CommitInputs.
pub fn load_commits(conn: &Connection) -> Result<Vec<CommitInput>> {
    let mut stmt = conn.prepare(
        "SELECT native_id, deep_link, occurred_at, raw_json
         FROM source_objects WHERE source = 'git'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (sha, deep_link, occurred_at, raw_json) = row?;
        let raw: Value = serde_json::from_str(&raw_json)?;
        let branches = raw
            .get("branches")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        out.push(CommitInput {
            sha,
            short_sha: raw.get("short_sha").and_then(Value::as_str).unwrap_or("").to_string(),
            subject: raw.get("subject").and_then(Value::as_str).unwrap_or("").to_string(),
            body: raw.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
            branches,
            deep_link,
            committed_at: parse_utc(&occurred_at)?,
        });
    }
    Ok(out)
}

/// Jira issues stored as source objects → WorkItemInputs (with comments).
pub fn load_work_items(conn: &Connection) -> Result<Vec<WorkItemInput>> {
    let mut stmt = conn.prepare(
        "SELECT native_id, deep_link, occurred_at, raw_json
         FROM source_objects WHERE source = 'jira'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (key, deep_link, occurred_at, raw_json) = row?;
        let raw: Value = serde_json::from_str(&raw_json)?;
        let summary =
            raw.pointer("/fields/summary").and_then(Value::as_str).unwrap_or("").to_string();
        let comments = raw
            .pointer("/fields/comment/comments")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|c| IssueComment {
                        author_account_id: c
                            .pointer("/author/accountId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        body_text: adf_plain_text(c.get("body")),
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(WorkItemInput {
            key,
            summary,
            deep_link,
            occurred_at: parse_utc(&occurred_at)?,
            comments,
        });
    }
    Ok(out)
}

/// Asks = Gmail/Slack source objects whose text **references a work item** (its
/// subject/snippet or Slack text contains an issue key).
///
/// We deliberately do NOT gate on the v1 noise/action-needed classification: the
/// general-purpose topic classifier never knew about issue keys and falls back
/// to `noise` on low confidence — it did exactly that to a self-sent email whose
/// subject was literally "ALM-3". The exact word-boundary key-match IS the ask
/// signal here, and a stronger, more precise one; binding still requires the key
/// to resolve to a real fetched issue (per E2) plus a commit (E1) plus the
/// user's approval. Word-boundary matching is unchanged; no fuzzy matching is
/// introduced — this only removes a filter that was discarding valid asks.
pub fn load_asks(conn: &Connection) -> Result<Vec<AskInput>> {
    let mut stmt = conn.prepare(
        "SELECT source, native_id, deep_link, occurred_at, raw_json
         FROM source_objects WHERE source IN ('gmail', 'slack')",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (source, native_id, deep_link, occurred_at, raw_json) = row?;
        let source = SourceId::parse(&source)
            .with_context(|| format!("unknown ask source '{source}'"))?;
        let raw: Value = serde_json::from_str(&raw_json)?;
        let (subject, text, asker) = ask_text(source, &raw);
        // Precision gate: only messages that reference a work item are asks.
        if issue_keys(&text).is_empty() {
            continue;
        }
        out.push(AskInput {
            provenance: ProvenanceRef { source, native_id, deep_link },
            subject,
            text,
            asker,
            occurred_at: parse_utc(&occurred_at)?,
        });
    }
    Ok(out)
}

/// (subject, key-scan text, asker) for an ask from its stored raw payload.
fn ask_text(source: SourceId, raw: &Value) -> (Option<String>, String, Option<String>) {
    match source {
        SourceId::Gmail => {
            let payload = raw.get("payload").cloned().unwrap_or(Value::Null);
            let header = |name: &str| {
                crate::adapters::gmail::header_values(&payload, name)
                    .first()
                    .map(|s| s.to_string())
            };
            let subject = header("Subject").unwrap_or_default();
            let snippet = raw.get("snippet").and_then(Value::as_str).unwrap_or("");
            let text = format!("{subject}\n{snippet}");
            (Some(subject), text, header("From"))
        }
        SourceId::Slack => {
            let text = raw.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            let asker = raw.get("user").and_then(Value::as_str).map(String::from);
            (None, text, asker)
        }
        _ => (None, String::new(), None),
    }
}

/// Flatten an ADF body to its concatenated text leaves (comment bodies are ADF).
fn adf_plain_text(adf: Option<&Value>) -> String {
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = map.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                        out.push(' ');
                    }
                }
                if let Some(content) = map.get("content") {
                    walk(content, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    if let Some(v) = adf {
        walk(v, &mut out);
    }
    out.trim().to_string()
}

fn parse_utc(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("bad timestamp '{s}'"))?
        .with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_keys_are_word_boundary_safe() {
        // ALM-1 matches.
        assert!(issue_keys("please fix ALM-1 today").contains("ALM-1"));
        assert!(issue_keys("(ALM-1)").contains("ALM-1"));
        assert!(issue_keys("done: ALM-1.").contains("ALM-1"));
        // PSALM-1 must NOT be read as ALM-1.
        let psalm = issue_keys("see PSALM-1 reference");
        assert!(psalm.contains("PSALM-1"));
        assert!(!psalm.contains("ALM-1"));
        // ALM-10 must NOT be read as ALM-1.
        let ten = issue_keys("resolves ALM-10");
        assert!(ten.contains("ALM-10"));
        assert!(!ten.contains("ALM-1"));
        // lowercase + trailing letter are not keys.
        assert!(issue_keys("alm-1").is_empty());
        assert!(!issue_keys("ALM-1a").contains("ALM-1"));
        // multiple keys.
        let two = issue_keys("ALM-1 and ALM-2");
        assert!(two.contains("ALM-1") && two.contains("ALM-2"));
    }

    fn commit(sha: &str, subject: &str, body: &str, branches: &[&str]) -> CommitInput {
        CommitInput {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            subject: subject.to_string(),
            body: body.to_string(),
            branches: branches.iter().map(|s| s.to_string()).collect(),
            deep_link: format!("git-local://scratch/commit/{sha}"),
            committed_at: DateTime::parse_from_rfc3339("2026-07-11T08:00:00+00:00")
                .unwrap()
                .with_timezone(&Utc),
        }
    }
    fn ask(source: SourceId, native: &str, text: &str) -> AskInput {
        AskInput {
            provenance: ProvenanceRef {
                source,
                native_id: native.to_string(),
                deep_link: "https://example.com/x".to_string(),
            },
            subject: Some("Status?".to_string()),
            text: text.to_string(),
            asker: None,
            occurred_at: Utc::now(),
        }
    }
    fn item(key: &str, comments: Vec<IssueComment>) -> WorkItemInput {
        WorkItemInput {
            key: key.to_string(),
            summary: "an issue".to_string(),
            deep_link: format!("https://x.atlassian.net/browse/{key}"),
            occurred_at: Utc::now(),
            comments,
        }
    }

    #[test]
    fn full_triple_binds_into_one_thread() {
        let engine = CorrelationEngine::new(None);
        let asks = vec![ask(SourceId::Gmail, "m1", "Any update on ALM-3?")];
        let items = vec![item("ALM-3", vec![])];
        let commits = vec![
            commit("a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1", "ALM-3: fix tz bug", "", &["main"]),
            commit("b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2", "unrelated work", "", &["main"]),
        ];
        let threads = engine.correlate(&asks, &items, &commits);
        assert_eq!(threads.len(), 1);
        let t = &threads[0];
        assert!(t.is_full());
        assert_eq!(t.asks.len(), 1);
        assert_eq!(t.evidence.len(), 1, "only the ALM-3 commit binds");
        assert_eq!(t.evidence[0].sites, vec![MatchSite::Subject]);
    }

    #[test]
    fn key_in_branch_or_body_binds() {
        let engine = CorrelationEngine::new(None);
        let items = vec![item("ALM-2", vec![])];
        let commits = vec![
            commit("c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3", "Add parser", "Refs ALM-2.", &["feature/ALM-2"]),
        ];
        let threads = engine.correlate(&[], &items, &commits);
        assert_eq!(threads.len(), 1);
        let sites = &threads[0].evidence[0].sites;
        assert!(sites.contains(&MatchSite::Body));
        assert!(sites.contains(&MatchSite::Branch));
        // No ask → partial, not eligible to propose.
        assert!(!threads[0].is_full());
    }

    #[test]
    fn ambiguous_commit_does_not_bind() {
        // A textually similar but wrong-key (or no-key) commit must NOT bind —
        // below signal ⇒ no thread. (Negative test: no fabrication.)
        let engine = CorrelationEngine::new(None);
        let asks = vec![ask(SourceId::Gmail, "m1", "update on ALM-1?")];
        let items = vec![item("ALM-1", vec![])];
        let commits = vec![
            commit("d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4", "PSALM-1 hymn tweak", "touches ALM-10 too", &["main"]),
        ];
        let threads = engine.correlate(&asks, &items, &commits);
        // The item has an ask but NO commit (PSALM-1/ALM-10 ≠ ALM-1) → partial.
        assert_eq!(threads.len(), 1);
        assert!(threads[0].evidence.is_empty(), "no commit binds to ALM-1");
        assert!(!threads[0].is_full());
    }

    #[test]
    fn missing_leg_yields_partial_not_proposal() {
        let backend = crate::act::draft::TemplatedDraftingBackend;
        let engine = CorrelationEngine::new(None);
        // item + commit but NO ask → partial → no proposal.
        let items = vec![item("ALM-3", vec![])];
        let commits =
            vec![commit("e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5", "ALM-3: done", "", &["main"])];
        let threads = engine.correlate(&[], &items, &commits);
        assert_eq!(threads.len(), 1);
        assert!(!threads[0].is_full());
        assert!(propose_from_thread(&threads[0], &backend).unwrap().is_empty());
    }

    #[test]
    fn j15_excludes_almanac_own_comment_as_evidence() {
        // A commit with NO key anywhere, bound ONLY via a Jira comment sha ref.
        let commit_sha = "f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6";
        let c = commit(commit_sha, "misc cleanup", "no key here", &["main"]);
        let items_self = vec![item(
            "ALM-4",
            vec![IssueComment {
                author_account_id: "almanac-bot".to_string(),
                body_text: format!("Done — fixed in {} .", &commit_sha[..7]),
            }],
        )];
        // With self = almanac-bot, the self comment is EXCLUDED → no binding.
        let engine_self = CorrelationEngine::new(Some("almanac-bot".to_string()));
        let t = engine_self.correlate(&[], &items_self, std::slice::from_ref(&c));
        assert!(t.is_empty() || t[0].evidence.is_empty(), "J15: own comment must not corroborate");

        // A HUMAN comment with the same sha DOES bind.
        let items_human = vec![item(
            "ALM-4",
            vec![IssueComment {
                author_account_id: "human-123".to_string(),
                body_text: format!("thanks, see {} .", &commit_sha[..7]),
            }],
        )];
        let t2 = engine_self.correlate(&[], &items_human, std::slice::from_ref(&c));
        assert_eq!(t2.len(), 1);
        assert_eq!(t2[0].evidence.len(), 1);
        assert_eq!(t2[0].evidence[0].sites, vec![MatchSite::Comment]);
    }

    #[test]
    fn proposer_builds_workdone_reply_with_commit_evidence() {
        let backend = crate::act::draft::TemplatedDraftingBackend;
        let engine = CorrelationEngine::new(None);
        let asks = vec![ask(SourceId::Gmail, "m1", "Is ALM-3 fixed yet?")];
        let items = vec![item("ALM-3", vec![])];
        let commits =
            vec![commit("a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1", "ALM-3: fix tz", "", &["main"])];
        let threads = engine.correlate(&asks, &items, &commits);
        let proposals = propose_from_thread(&threads[0], &backend).unwrap();
        assert_eq!(proposals.len(), 1);
        let p = &proposals[0];
        assert_eq!(p.kind(), ActionKind::GmailReply);
        assert!(p.draft().asserts_work_done, "reply asserts work done (E1 in play)");
        // Evidence: the git commit (hard) + the jira issue.
        assert!(p.evidence().iter().any(|e| e.kind == EvidenceKind::GitCommit));
        assert!(p.evidence().iter().any(|e| e.kind == EvidenceKind::JiraEvent));
        assert!(p.correlation_rationale().unwrap().contains("ALM-3"));
    }
}
