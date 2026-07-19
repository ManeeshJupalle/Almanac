//! Tauri shell — a thin IPC client of the headless core. No engine logic
//! here: classification, synthesis, and grounding all live in almanac-core.
//!
//! IPC boundary discipline: only these string-field DTOs cross to the UI.
//! `RawContent` has no Serialize impl (compile-time guarantee, unchanged) and
//! is never touched here; `ProvenanceRef`'s three non-sensitive fields are
//! copied into the DTO so the UI can deep-link back to sources.

use serde::Serialize;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BriefingItemView {
    pub position: i64,
    pub kind: String,
    pub summary: String,
    pub occurred_at: String,
    pub source: String,
    pub native_id: String,
    pub deep_link: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BriefingView {
    pub briefing_date: String,
    pub backend_id: String,
    pub rationale: String,
    pub created_at: String,
    pub items: Vec<BriefingItemView>,
    /// Tomorrow's events, shown in a separate "Coming up" preview.
    pub preview: Vec<BriefingItemView>,
    /// The local date the preview covers (briefed day + 1), YYYY-MM-DD.
    pub preview_date: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatusView {
    pub source: String,
    pub connected: bool,
    pub detail: String,
}

fn item_to_view(item: almanac_core::db::StoredBriefingItem) -> BriefingItemView {
    BriefingItemView {
        position: item.position,
        kind: item.kind.as_str().to_string(),
        summary: item.summary,
        occurred_at: item.occurred_at.to_rfc3339(),
        source: item.provenance.source.to_string(),
        native_id: item.provenance.native_id,
        deep_link: item.provenance.deep_link,
    }
}

fn to_view(stored: almanac_core::db::StoredBriefing) -> BriefingView {
    let preview_date = stored
        .briefing_date
        .succ_opt()
        .map(|d| d.to_string())
        .unwrap_or_default();
    BriefingView {
        briefing_date: stored.briefing_date.to_string(),
        backend_id: stored.backend_id,
        rationale: stored.rationale,
        created_at: stored.created_at,
        items: stored.items.into_iter().map(item_to_view).collect(),
        preview: stored.preview.into_iter().map(item_to_view).collect(),
        preview_date,
    }
}

/// Latest stored briefing from local SQLite (None if none generated yet).
#[tauri::command]
fn get_briefing() -> Result<Option<BriefingView>, String> {
    let inner = || -> anyhow::Result<Option<BriefingView>> {
        let path = almanac_core::init_default_db()?;
        let conn = almanac_core::db::open(&path)?;
        Ok(almanac_core::db::latest_briefing(&conn)?.map(to_view))
    };
    inner().map_err(|e| format!("{e:#}"))
}

/// Real connection state: token present at the configured path AND
/// decryptable by this OS user. Scope list is read from the stored token.
#[tauri::command]
fn get_connection_status() -> Vec<SourceStatusView> {
    let google = almanac_core::auth::GoogleAuth::from_env()
        .and_then(|a| almanac_core::auth::load_token(&a.token_path));
    let (g_ok, g_detail) = match &google {
        Ok(token) => {
            let scopes = token.get("scope").and_then(|v| v.as_str()).unwrap_or("");
            let has = |s: &str| scopes.contains(s);
            // Testing-mode consent expires refresh tokens after ~7 days; if
            // the last successful (re)issue is older, warn before compose
            // fails. Honest hint, not a live probe.
            let age_days = token
                .get("obtained_at_unix")
                .and_then(|v| v.as_i64())
                .map(|t| (chrono::Utc::now().timestamp() - t) / 86_400);
            let expiry_hint = match age_days {
                Some(d) if d >= 7 => " · token >7 days old — likely expired (testing mode); re-auth if composing fails",
                _ => "",
            };
            (true, format!(
                "token on file · scopes: {}{}{}",
                if has("gmail.readonly") { "gmail.readonly " } else { "" },
                if has("calendar.readonly") { "calendar.readonly" } else { "" },
                expiry_hint,
            ))
        }
        Err(e) => (false, format!("not connected — {e:#}")),
    };
    let slack = almanac_core::auth::SlackAuth::from_env()
        .and_then(|a| almanac_core::auth::load_token(&a.token_path));
    let (s_ok, s_detail) = match &slack {
        Ok(token) => (
            true,
            format!(
                "token on file · scopes: {}",
                token.pointer("/authed_user/scope").and_then(|v| v.as_str()).unwrap_or("?")
            ),
        ),
        Err(e) => (false, format!("not connected — {e:#}")),
    };

    vec![
        SourceStatusView { source: "gmail".into(), connected: g_ok, detail: g_detail.clone() },
        SourceStatusView { source: "gcal".into(), connected: g_ok, detail: g_detail },
        SourceStatusView { source: "slack".into(), connected: s_ok, detail: s_detail },
    ]
}

/// Guards against overlapping composes (audit F-10): the UI disables its
/// button, but this serializes at the command layer too, so a double-invoke
/// over IPC can't run two live chains against the same DB at once.
static COMPOSING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Full live chain (adapters → extraction → synthesis → persisted briefing),
/// entirely inside the core. Long-running: model inference is CPU-bound, so
/// it runs on the blocking pool, not a runtime worker.
#[tauri::command]
async fn run_live_briefing() -> Result<BriefingView, String> {
    use std::sync::atomic::Ordering;
    if COMPOSING.swap(true, Ordering::SeqCst) {
        return Err("A briefing is already being composed — please wait.".into());
    }
    let result = tauri::async_runtime::spawn_blocking(|| {
        tauri::async_runtime::block_on(async {
            almanac_core::live_briefing().await.map(to_view).map_err(|e| format!("{e:#}"))
        })
    })
    .await
    .map_err(|e| format!("briefing task panicked: {e}"));
    COMPOSING.store(false, Ordering::SeqCst);
    result?
}

// ------------------------------------------- action layer (Phase 2.0) ------
//
// IPC discipline unchanged: string-field DTOs only. The `dryRun` string shown
// in the queue IS the exact payload the executor will send (Gmail: the full
// MIME; Slack: the exact JSON body) — no raw third-party content crosses here.

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceView {
    pub tier: String,
    pub kind: String,
    pub source: String,
    pub native_id: String,
    /// The artifact's stored deep link (from the E2-enforcing INNER JOIN).
    pub deep_link: String,
    pub observed_at: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProposalView {
    pub id: i64,
    pub kind: String,
    pub state: String,
    pub subject: Option<String>,
    pub body: String,
    pub asserts_work_done: bool,
    pub backend_id: String,
    pub created_at: String,
    pub expires_at: String,
    /// Exact bytes-to-be-sent, rendered for human approval.
    pub dry_run: String,
    pub receipt: Option<String>,
    /// Templated correlation basis + confidence (Phase 2.2), if any.
    pub correlation_rationale: Option<String>,
    pub evidence: Vec<EvidenceView>,
}

fn proposal_to_view(
    conn: &almanac_core::db::Connection,
    p: almanac_core::act::StoredProposal,
) -> ProposalView {
    use almanac_core::act::executors::{
        render_gmail_reply, render_jira_comment, render_jira_transition, render_slack_post,
    };
    use almanac_core::act::ActionKind;
    // dry_run is the exact send payload; if it can't render (e.g. target row
    // missing) show the reason rather than a misleading blank. Jira renders
    // need the site base URL (from JiraAuth config, no token access here).
    let jira_base = || almanac_core::auth::JiraAuth::from_env().map(|a| a.base_url().to_string());
    let dry_run = match p.kind {
        ActionKind::GmailReply => render_gmail_reply(conn, &p),
        ActionKind::SlackPost => render_slack_post(&p),
        ActionKind::JiraTransition => jira_base().and_then(|b| render_jira_transition(&b, &p)),
        ActionKind::JiraComment => jira_base().and_then(|b| render_jira_comment(&b, &p)),
    }
    .map(|r| r.display)
    .unwrap_or_else(|e| format!("[dry-run unavailable: {e:#}]"));

    ProposalView {
        id: p.id,
        kind: p.kind.as_str().to_string(),
        state: p.state.as_str().to_string(),
        subject: p.draft_subject,
        body: p.draft_body,
        asserts_work_done: p.asserts_work_done,
        backend_id: p.backend_id,
        created_at: p.created_at,
        expires_at: p.expires_at,
        dry_run,
        receipt: p.receipt_json,
        correlation_rationale: p.correlation_rationale,
        evidence: p
            .evidence
            .into_iter()
            .map(|e| EvidenceView {
                tier: e.tier.as_str().to_string(),
                kind: e.kind.as_str().to_string(),
                source: e.source.to_string(),
                native_id: e.native_id,
                deep_link: e.deep_link.unwrap_or(e.artifact_deep_link),
                observed_at: e.observed_at,
            })
            .collect(),
    }
}

/// The approval queue (TTL-swept), newest first.
#[tauri::command]
fn list_proposals() -> Result<Vec<ProposalView>, String> {
    let inner = || -> anyhow::Result<Vec<ProposalView>> {
        let path = almanac_core::init_default_db()?;
        let mut conn = almanac_core::db::open(&path)?;
        let proposals = almanac_core::act::list_proposals(&mut conn)?;
        Ok(proposals.into_iter().map(|p| proposal_to_view(&conn, p)).collect())
    };
    inner().map_err(|e| format!("{e:#}"))
}

/// THE user-approval event (A1). Returns the new state after approval.
#[tauri::command]
fn approve_proposal(id: i64) -> Result<String, String> {
    let inner = || -> anyhow::Result<String> {
        let path = almanac_core::init_default_db()?;
        let mut conn = almanac_core::db::open(&path)?;
        // "user" names the human at the approval UI — the only actor that can
        // move a proposal to Approved (A1).
        almanac_core::act::approve(&mut conn, id, "user")?;
        // Capture the decision + factor snapshot for the learning loop (3.3).
        // Best-effort: a capture failure must not undo the approval.
        let _ = almanac_core::plan::record_proposal_decision(&conn, id, "user", "approved", chrono::Utc::now());
        Ok("approved".to_string())
    };
    inner().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn reject_proposal(id: i64) -> Result<String, String> {
    let inner = || -> anyhow::Result<String> {
        let path = almanac_core::init_default_db()?;
        let mut conn = almanac_core::db::open(&path)?;
        almanac_core::act::reject(&mut conn, id, "user")?;
        let _ = almanac_core::plan::record_proposal_decision(&conn, id, "user", "rejected", chrono::Utc::now());
        Ok("rejected".to_string())
    };
    inner().map_err(|e| format!("{e:#}"))
}

/// Execute an APPROVED proposal (A2: dispatches to the executor that holds the
/// write scope). Long-running (network); runs on the blocking pool.
#[tauri::command]
async fn execute_proposal(id: i64) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        tauri::async_runtime::block_on(async move {
            use almanac_core::act::executors::{
                ActionExecutor, GmailReplyExecutor, JiraCommentExecutor, JiraTransitionExecutor,
                SlackPostExecutor,
            };
            use almanac_core::act::ActionKind;
            let path = almanac_core::init_default_db().map_err(|e| format!("{e:#}"))?;
            let mut conn = almanac_core::db::open(&path).map_err(|e| format!("{e:#}"))?;
            // Read the kind to pick the executor; the executor re-validates
            // state == Approved itself (A1/A2 defense in depth).
            let proposal =
                almanac_core::act::load_proposal(&conn, id).map_err(|e| format!("{e:#}"))?;
            let receipt = match proposal.kind {
                ActionKind::GmailReply => {
                    let ex = GmailReplyExecutor::from_env().map_err(|e| format!("{e:#}"))?;
                    ex.execute(&mut conn, id).await
                }
                ActionKind::SlackPost => {
                    let ex = SlackPostExecutor::from_env().map_err(|e| format!("{e:#}"))?;
                    ex.execute(&mut conn, id).await
                }
                ActionKind::JiraTransition => {
                    let ex = JiraTransitionExecutor::from_env().map_err(|e| format!("{e:#}"))?;
                    ex.execute(&mut conn, id).await
                }
                ActionKind::JiraComment => {
                    let ex = JiraCommentExecutor::from_env().map_err(|e| format!("{e:#}"))?;
                    ex.execute(&mut conn, id).await
                }
            }
            .map_err(|e| format!("{e:#}"))?;
            // Capture the executed decision + factor snapshot (3.3). Best-effort.
            let _ = almanac_core::plan::record_proposal_decision(&conn, id, "user", "executed", chrono::Utc::now());
            Ok(format!(
                "executed (http {}, audit seq {}..{})",
                receipt.http_status, receipt.started_seq, receipt.final_seq
            ))
        })
    })
    .await
    .map_err(|e| format!("execute task panicked: {e}"))?
}

/// Audit-chain verification for the UI footer (never fails the app; returns a
/// human string either way).
#[tauri::command]
fn verify_audit_chain() -> Result<String, String> {
    let inner = || -> anyhow::Result<String> {
        let path = almanac_core::init_default_db()?;
        let conn = almanac_core::db::open(&path)?;
        let r = almanac_core::act::audit::verify_chain(&conn)?;
        Ok(format!("audit chain intact — {} records (head seq {})", r.records, r.head_seq))
    };
    inner().map_err(|e| format!("{e:#}"))
}

// -------------------------------------------- audit-log viewer (3.6.1) -----

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AuditRecordView {
    pub seq: i64,
    pub ts: String,
    pub actor: String,
    pub event: String,
    pub proposal_id: Option<i64>,
    /// Short fingerprint of the chained record hash (browse, not verify).
    pub record_hash: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AuditLogView {
    /// Whether the chain recomputes intact right now.
    pub verified: bool,
    pub status: String,
    /// Most-recent-first records.
    pub records: Vec<AuditRecordView>,
}

/// Browse the hash-chained audit log (3.6.1) — READ ONLY. Surfaces the live
/// chain-intact status plus the most recent records, so the log can be inspected,
/// not just verified.
#[tauri::command]
fn get_audit_log(limit: Option<usize>) -> Result<AuditLogView, String> {
    let inner = || -> anyhow::Result<AuditLogView> {
        let path = almanac_core::init_default_db()?;
        let conn = almanac_core::db::open(&path)?;
        let (verified, status) = match almanac_core::act::audit::verify_chain(&conn) {
            Ok(r) => (true, format!("chain intact — {} records (head seq {})", r.records, r.head_seq)),
            Err(e) => (false, format!("CHAIN BROKEN — {e:#}")),
        };
        let records = almanac_core::act::audit::tail(&conn, limit.unwrap_or(100))?
            .into_iter()
            .map(|r| AuditRecordView {
                seq: r.seq,
                ts: r.ts,
                actor: r.actor,
                event: r.event,
                proposal_id: r.proposal_id,
                record_hash: r.record_hash.chars().take(12).collect(),
            })
            .collect();
        Ok(AuditLogView { verified, status, records })
    };
    inner().map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------- prioritized plan (2.3) -----

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FactorView {
    pub name: String,
    pub points: i32,
    pub reason: String,
}

/// A proposal queued for a plan item (Phase 3.1) — lets the plan surface + act on
/// it inline, through the same approve/reject/execute commands as the queue.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProposalRefView {
    pub id: i64,
    pub kind: String,
    pub state: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlanItemView {
    pub rank: usize,
    /// Stable plan-item identity (Phase 3.2) — used to snooze/dismiss/complete it.
    pub item_key: String,
    pub title: String,
    pub kind: String,
    pub section: String,
    pub score: i32,
    /// Templated "why this rank" (typed factor slots only).
    pub rationale: String,
    pub deep_link: String,
    pub factors: Vec<FactorView>,
    /// Proposal(s) queued for this item, if any (Phase 3.1).
    pub proposals: Vec<ProposalRefView>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlanView {
    pub summary: String,
    pub generated_at: String,
    pub items: Vec<PlanItemView>,
}

fn candidate_kind_str(k: almanac_core::plan::CandidateKind) -> &'static str {
    use almanac_core::plan::CandidateKind::*;
    match k {
        WorkThreadFull => "work thread (ready)",
        WorkThreadPartial => "work thread (possible)",
        CalendarEvent => "event",
        Ask => "ask",
    }
}

fn plan_to_view(
    p: almanac_core::plan::Plan,
    links: &std::collections::HashMap<String, Vec<almanac_core::plan::LinkedProposal>>,
) -> PlanView {
    PlanView {
        summary: p.summary,
        generated_at: p.generated_at.to_rfc3339(),
        items: p
            .items
            .into_iter()
            .map(|i| {
                let proposals = links
                    .get(&i.candidate.key)
                    .map(|ls| {
                        ls.iter()
                            .map(|l| ProposalRefView {
                                id: l.id,
                                kind: l.kind.clone(),
                                state: l.state.as_str().to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                PlanItemView {
                    rank: i.rank,
                    item_key: i.candidate.key.clone(),
                    title: i.candidate.title,
                    kind: candidate_kind_str(i.candidate.kind).to_string(),
                    section: i.section.as_str().to_string(),
                    score: i.score,
                    rationale: i.rationale,
                    deep_link: i.candidate.deep_link,
                    factors: i
                        .factors
                        .into_iter()
                        .map(|f| FactorView { name: f.name.to_string(), points: f.points, reason: f.reason })
                        .collect(),
                    proposals,
                }
            })
            .collect(),
    }
}

/// The current prioritized plan — READ ONLY (no queueing, no audit). The passive
/// display the UI opens with.
#[tauri::command]
fn get_plan() -> Result<PlanView, String> {
    let inner = || -> anyhow::Result<PlanView> {
        let path = almanac_core::init_default_db()?;
        let conn = almanac_core::db::open(&path)?;
        let now = chrono::Utc::now();
        let candidates = almanac_core::plan::load_candidates(&conn, now)?;
        let states = almanac_core::plan::load_item_states(&conn)?;
        let active = almanac_core::plan::active_candidates(candidates, &states, now);
        let config = almanac_core::plan::effective_config(&conn)?;
        let plan = almanac_core::plan::prioritize(&active, &config, now);
        let links = almanac_core::plan::link_proposals(&conn, &plan)?;
        Ok(plan_to_view(plan, &links))
    };
    inner().map_err(|e| format!("{e:#}"))
}

/// Set a plan item's state (Phase 3.2): snooze / done / dismiss / reopen. Audited
/// (actor + reason), no external side-effect. Returns the fresh, filtered plan.
/// `status`: "snoozed" | "done" | "dismissed" | "open"; `snoozeHours` applies
/// only to "snoozed" (default 24h).
#[tauri::command]
fn set_plan_item_state(
    item_key: String,
    status: String,
    snooze_hours: Option<i64>,
) -> Result<PlanView, String> {
    use almanac_core::plan::ItemStatus;
    let inner = || -> anyhow::Result<PlanView> {
        let path = almanac_core::init_default_db()?;
        let mut conn = almanac_core::db::open(&path)?;
        let now = chrono::Utc::now();
        let (st, until) = match status.as_str() {
            "open" => (None, None),
            "snoozed" => {
                let hrs = snooze_hours.unwrap_or(24).clamp(1, 24 * 30);
                (Some(ItemStatus::Snoozed), Some(now + chrono::Duration::hours(hrs)))
            }
            "done" => (Some(ItemStatus::Done), None),
            "dismissed" => (Some(ItemStatus::Dismissed), None),
            other => anyhow::bail!("unknown plan item status '{other}'"),
        };
        almanac_core::plan::set_item_state(&mut conn, &item_key, st, until, "user", &status, now)?;
        // Capture the decision + factor snapshot for the learning loop (3.3).
        let decision = if status == "open" { "reopened" } else { status.as_str() };
        let _ = almanac_core::plan::record_item_decision(&conn, &item_key, "user", decision, now);

        // Fresh, filtered plan so the UI reflects the change in one round trip.
        let candidates = almanac_core::plan::load_candidates(&conn, now)?;
        let states = almanac_core::plan::load_item_states(&conn)?;
        let active = almanac_core::plan::active_candidates(candidates, &states, now);
        let config = almanac_core::plan::effective_config(&conn)?;
        let plan = almanac_core::plan::prioritize(&active, &config, now);
        let links = almanac_core::plan::link_proposals(&conn, &plan)?;
        Ok(plan_to_view(plan, &links))
    };
    inner().map_err(|e| format!("{e:#}"))
}

/// A triggered re-plan cycle (the "Refresh plan" button): idempotently queue
/// proposals + re-rank + append a traceable audit record (actor = the human who
/// clicked). Offline (no external writes). J15 self-comment exclusion uses the
/// accountId cached by the last online Jira step (`app_meta`); if none has been
/// cached yet, it degrades to no exclusion — same as before that step ran.
#[tauri::command]
fn replan(reason: Option<String>) -> Result<PlanView, String> {
    let inner = || -> anyhow::Result<PlanView> {
        let path = almanac_core::init_default_db()?;
        let mut conn = almanac_core::db::open(&path)?;
        let self_id = almanac_core::db::get_meta(&conn, almanac_core::db::JIRA_SELF_ACCOUNT_ID)?;
        let config = almanac_core::plan::effective_config(&conn)?;
        let reason = reason.unwrap_or_else(|| "manual refresh".to_string());
        let report = almanac_core::plan::replan_cycle(
            &mut conn,
            &config,
            self_id,
            "user",
            &reason,
            chrono::Utc::now(),
        )?;
        let links = almanac_core::plan::link_proposals(&conn, &report.plan)?;
        Ok(plan_to_view(report.plan, &links))
    };
    inner().map_err(|e| format!("{e:#}"))
}

// ------------------------------------------------ learning flywheel (3.4) --

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LearnedWeightView {
    pub factor: String,
    pub multiplier: f64,
    pub positives: usize,
    pub negatives: usize,
    pub rationale: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LearningView {
    pub enabled: bool,
    pub adjustments: Vec<LearnedWeightView>,
}

fn learning_view(conn: &almanac_core::db::Connection) -> anyhow::Result<LearningView> {
    let enabled = almanac_core::plan::learning_enabled(conn)?;
    // Always surface what learning WOULD do, even when off, so the UI can explain
    // it before you enable it.
    let adjustments = almanac_core::plan::learn_weights(conn)?
        .into_iter()
        .map(|w| LearnedWeightView {
            factor: w.factor,
            multiplier: w.multiplier,
            positives: w.positives,
            negatives: w.negatives,
            rationale: w.rationale,
        })
        .collect();
    Ok(LearningView { enabled, adjustments })
}

/// What Almanac has learned from your decisions (3.4) — read-only.
#[tauri::command]
fn get_learning() -> Result<LearningView, String> {
    let inner = || -> anyhow::Result<LearningView> {
        let path = almanac_core::init_default_db()?;
        let conn = almanac_core::db::open(&path)?;
        learning_view(&conn)
    };
    inner().map_err(|e| format!("{e:#}"))
}

/// Turn learned weighting on/off — the "reset to defaults" control (3.4).
/// Returns the updated learning view; the caller re-fetches the plan.
#[tauri::command]
fn set_learning_enabled(enabled: bool) -> Result<LearningView, String> {
    let inner = || -> anyhow::Result<LearningView> {
        let path = almanac_core::init_default_db()?;
        let conn = almanac_core::db::open(&path)?;
        almanac_core::plan::set_learning_enabled(&conn, enabled)?;
        learning_view(&conn)
    };
    inner().map_err(|e| format!("{e:#}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // .env lives at the repo root; tauri dev runs with cwd=src-tauri and a
    // bundled exe runs from target/release (or wherever it was copied).
    // Anchor the process at the .env directory — search the cwd's ancestors
    // first (dev), then the executable's ancestors (built exe) — so the
    // relative paths inside .env (credentials, tokens) and the models/
    // walk-up resolve identically everywhere.
    let anchored = dotenvy::dotenv().ok().or_else(|| {
        let mut dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        loop {
            let candidate = dir.join(".env");
            if candidate.is_file() {
                dotenvy::from_path(&candidate).ok()?;
                return Some(candidate);
            }
            if !dir.pop() {
                return None;
            }
        }
    });
    if let Some(env_path) = anchored {
        if let Some(root) = env_path.parent() {
            let _ = std::env::set_current_dir(root);
        }
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|_app| {
            let db_path = almanac_core::init_default_db()?;
            println!("almanac: db ready at {}", db_path.display());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_briefing,
            get_connection_status,
            run_live_briefing,
            list_proposals,
            approve_proposal,
            reject_proposal,
            execute_proposal,
            verify_audit_chain,
            get_plan,
            replan,
            set_plan_item_state,
            get_learning,
            set_learning_enabled,
            get_audit_log
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
