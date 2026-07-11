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
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatusView {
    pub source: String,
    pub connected: bool,
    pub detail: String,
}

fn to_view(stored: almanac_core::db::StoredBriefing) -> BriefingView {
    BriefingView {
        briefing_date: stored.briefing_date.to_string(),
        backend_id: stored.backend_id,
        rationale: stored.rationale,
        created_at: stored.created_at,
        items: stored
            .items
            .into_iter()
            .map(|item| BriefingItemView {
                position: item.position,
                kind: item.kind.as_str().to_string(),
                summary: item.summary,
                occurred_at: item.occurred_at.to_rfc3339(),
                source: item.provenance.source.to_string(),
                native_id: item.provenance.native_id,
                deep_link: item.provenance.deep_link,
            })
            .collect(),
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

/// Full live chain (adapters → extraction → synthesis → persisted briefing),
/// entirely inside the core. Long-running: model inference is CPU-bound, so
/// it runs on the blocking pool, not a runtime worker.
#[tauri::command]
async fn run_live_briefing() -> Result<BriefingView, String> {
    tauri::async_runtime::spawn_blocking(|| {
        tauri::async_runtime::block_on(async {
            almanac_core::live_briefing().await.map(to_view).map_err(|e| format!("{e:#}"))
        })
    })
    .await
    .map_err(|e| format!("briefing task panicked: {e}"))?
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
            run_live_briefing
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
