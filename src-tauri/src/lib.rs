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

// ---------------------------------------------- prioritized plan (2.3) -----

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FactorView {
    pub name: String,
    pub points: i32,
    pub reason: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlanItemView {
    pub rank: usize,
    pub title: String,
    pub kind: String,
    pub section: String,
    pub score: i32,
    /// Templated "why this rank" (typed factor slots only).
    pub rationale: String,
    pub deep_link: String,
    pub factors: Vec<FactorView>,
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

fn plan_to_view(p: almanac_core::plan::Plan) -> PlanView {
    PlanView {
        summary: p.summary,
        generated_at: p.generated_at.to_rfc3339(),
        items: p
            .items
            .into_iter()
            .map(|i| PlanItemView {
                rank: i.rank,
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
        let config = almanac_core::plan::PriorityConfig::from_env();
        Ok(plan_to_view(almanac_core::plan::prioritize(&candidates, &config, now)))
    };
    inner().map_err(|e| format!("{e:#}"))
}

/// A triggered re-plan cycle (the "Refresh plan" button): idempotently queue
/// proposals + re-rank + append a traceable audit record (actor = the human who
/// clicked). Offline (no external writes; J15 handled at correlate time and by
/// the idempotency key).
#[tauri::command]
fn replan(reason: Option<String>) -> Result<PlanView, String> {
    let inner = || -> anyhow::Result<PlanView> {
        let path = almanac_core::init_default_db()?;
        let mut conn = almanac_core::db::open(&path)?;
        let config = almanac_core::plan::PriorityConfig::from_env();
        let reason = reason.unwrap_or_else(|| "manual refresh".to_string());
        let report = almanac_core::plan::replan_cycle(
            &mut conn,
            &config,
            None,
            "user",
            &reason,
            chrono::Utc::now(),
        )?;
        Ok(plan_to_view(report.plan))
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
            replan
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
