use std::process::ExitCode;

use almanac_core::adapters::{
    gcal::CalendarAdapter, gmail::GmailAdapter, slack::SlackAdapter, SourceAdapter,
};
use almanac_core::auth::{GoogleAuth, SlackAuth};
use almanac_core::types::TimeWindow;
use anyhow::{Context, Result};
use chrono::{Duration, Utc};

fn main() -> ExitCode {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match args.first().map(String::as_str) {
        Some("--self-check") => self_check(),
        Some("smoke-adapters") => block_on(smoke_adapters()),
        Some("refresh-google") => block_on(refresh_google()),
        Some("refresh-slack") => block_on(refresh_slack()),
        Some("extract-fixtures") => extract_fixtures(),
        Some("extract-live") => block_on(extract_live()),
        Some("synthesize") => block_on(synthesize(args.get(1).cloned())),
        Some("e2e-fixtures") => block_on(e2e_fixtures()),
        Some("db-dump") => db_dump(),
        // Test utility: makes the stored Google refresh token invalid so the
        // expired-token UX (Google's 7-day testing-mode expiry) can be
        // reproduced on demand. Back up tokens/ first.
        Some("debug-expire-google-token") => (|| {
            let auth = GoogleAuth::from_env()?;
            let mut token = almanac_core::auth::load_token(&auth.token_path)?;
            token["refresh_token"] = serde_json::Value::String("invalid-for-testing".into());
            token["obtained_at_unix"] = serde_json::Value::from(0); // force refresh path
            almanac_core::auth::save_token(&auth.token_path, &token)?;
            println!("google token invalidated (refresh will now fail like an expired token)");
            Ok(())
        })(),
        Some("slack-permalink") => block_on(slack_permalink(args.get(1).cloned(), args.get(2).cloned())),
        Some("verify-chain") => verify_chain(),
        Some("list-proposals") => list_proposals(),
        // Dev-only seed path (Phase 2.0): builds a test proposal from a REAL
        // stored source object. Correlation (Phase 2.2) is the production
        // proposer; nothing outside tests/dev calls this.
        Some("debug-seed-proposal") => seed_proposal(args.get(1).cloned(), args.get(2).cloned()),
        Some("live-briefing") => block_on(async {
            let stored = almanac_core::live_briefing().await?;
            println!(
                "live briefing for {} ({} items) persisted; backend {}",
                stored.briefing_date,
                stored.items.len(),
                stored.backend_id
            );
            for item in &stored.items {
                println!(
                    "  {}. [{}] {} -> {}",
                    item.position + 1,
                    item.kind.as_str(),
                    item.summary,
                    item.provenance.deep_link
                );
            }
            println!("  why: {}", stored.rationale);
            Ok(())
        }),
        _ => {
            eprintln!(
                "usage: almanac-core <command>\n\
                 \n\
                 App / pipeline:\n\
                 \x20 --self-check              db + migrations sanity (prints \"core ok\")\n\
                 \x20 extract-fixtures         offline: fixtures -> classified items\n\
                 \x20 e2e-fixtures             offline: fixtures -> extraction -> briefing\n\
                 \x20 synthesize [YYYY-MM-DD]  synthesize a briefing for a local day\n\
                 \x20 live-briefing            full live chain (adapters -> UI-shaped briefing)\n\
                 \x20 extract-live             live smoke: adapters -> extraction -> SQLite\n\
                 \x20 smoke-adapters           live smoke: fetch a window from all 3 sources\n\
                 \n\
                 Auth:\n\
                 \x20 refresh-google           forced Google token refresh (fingerprint evidence)\n\
                 \x20 refresh-slack            Slack token refresh / live validation\n\
                 \n\
                 Action layer (Phase 2.0):\n\
                 \x20 verify-chain             walk + verify the hash-chained audit log\n\
                 \x20 list-proposals           approval queue + audit tail (local console)\n\
                 \x20 debug-seed-proposal <gmail-ack|slack-check> [native_id]\n\
                 \x20                          DEV-ONLY: seed a test proposal from a stored\n\
                 \x20                          source object (correlation arrives in 2.2)\n\
                 \n\
                 Diagnostics:\n\
                 \x20 db-dump                  recent extracted items (local console)\n\
                 \x20 slack-permalink <ch> <ts>  compare our deep link vs chat.getPermalink\n\
                 \x20 debug-expire-google-token  invalidate the stored token (test utility)"
            );
            return ExitCode::from(2);
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn block_on<F: std::future::Future<Output = Result<()>>>(fut: F) -> Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(fut)
}

fn self_check() -> Result<()> {
    let path = almanac_core::self_check()?;
    eprintln!("db: {}", path.display());
    println!("core ok");
    Ok(())
}

/// Live smoke: authenticate all three adapters and fetch the last 7 days.
/// Prints counts and one deep link per source — never tokens or raw content.
async fn smoke_adapters() -> Result<()> {
    let window = TimeWindow { start: Utc::now() - Duration::days(7), end: Utc::now() };
    println!("window: {} .. {}", window.start.to_rfc3339(), window.end.to_rfc3339());

    let mut gmail = GmailAdapter::from_env()?;
    gmail.authenticate().await?;
    report("gmail", &gmail.fetch_window(window).await?);

    let mut gcal = CalendarAdapter::from_env()?;
    gcal.authenticate().await?;
    report("gcal", &gcal.fetch_window(window).await?);

    let mut slack = SlackAdapter::from_env()?;
    slack.authenticate().await?;
    report("slack", &slack.fetch_window(window).await?);

    Ok(())
}

fn report(label: &str, objects: &[almanac_core::types::SourceObject]) {
    println!("{label}: {} source objects", objects.len());
    if let Some(first) = objects.first() {
        println!("  first.native_id: {}", first.provenance.native_id);
        println!("  first.occurred_at: {}", first.occurred_at.to_rfc3339());
        println!("  first.deep_link: {}", first.provenance.deep_link);
    }
}

async fn refresh_google() -> Result<()> {
    let evidence = GoogleAuth::from_env()?.force_refresh().await?;
    println!("google token refresh:\n{evidence}");
    Ok(())
}

async fn refresh_slack() -> Result<()> {
    let evidence = SlackAuth::from_env()?.force_refresh().await?;
    println!("slack token refresh:\n{evidence}");
    Ok(())
}

/// Probe whether the network is reachable (used to PROVE offline runs).
fn network_reachable() -> bool {
    use std::net::{SocketAddr, TcpStream};
    let probes: [SocketAddr; 2] =
        ["8.8.8.8:53".parse().unwrap(), "1.1.1.1:443".parse().unwrap()];
    probes.iter().any(|addr| {
        TcpStream::connect_timeout(addr, std::time::Duration::from_secs(2)).is_ok()
    })
}

/// Fixtures → SourceObjects (same parse functions as the live adapters).
fn fixture_source_objects() -> Result<Vec<almanac_core::types::SourceObject>> {
    use almanac_core::adapters::{gcal, gmail, slack};
    let read = |rel: &str| -> Result<serde_json::Value> {
        let raw = std::fs::read_to_string(std::path::Path::new("fixtures").join(rel))?;
        Ok(serde_json::from_str(&raw)?)
    };
    let mut objects = Vec::new();
    objects.push(gmail::message_to_source_object(&read("gmail/message_get_full.json")?)?);
    objects.extend(gcal::events_to_source_objects(&read("gcal/events_list_day.json")?)?);
    let history = read("slack/conversations_history.json")?;
    for msg in history.get("messages").and_then(serde_json::Value::as_array).into_iter().flatten()
    {
        objects.push(slack::message_to_source_object(
            "https://example.slack.com/",
            "C0BG797NE8P",
            msg,
        )?);
    }
    Ok(objects)
}

/// Offline extraction over the Phase-1 fixtures: loads the local ONNX model,
/// classifies every fixture source object, persists to SQLite. Reports the
/// network state first so an offline run is provable.
fn extract_fixtures() -> Result<()> {
    if network_reachable() {
        println!("network: REACHABLE — rerun with networking disabled to prove the offline gate");
    } else {
        println!("network: UNREACHABLE — offline run confirmed");
    }

    let model_dir = almanac_core::models_dir()?.join("minilm");
    let started = std::time::Instant::now();
    let extractor = almanac_core::extract::Extractor::with_model(&model_dir)?;
    println!("model loaded from {} in {:?} (local files only)", model_dir.display(), started.elapsed());

    let objects = fixture_source_objects()?;
    let items = extractor.extract(&objects)?;

    let db_path = almanac_core::init_default_db()?;
    let conn = almanac_core::db::open(&db_path)?;
    for (obj, item) in objects.iter().zip(&items) {
        almanac_core::db::insert_source_object(&conn, obj)?;
        almanac_core::db::insert_extracted_item(&conn, item)?;
    }

    println!("extracted {} items from {} source objects (nothing dropped):", items.len(), objects.len());
    for item in &items {
        println!(
            "  [{}] {} — decided_by={} hits={:?} (source {}:{})",
            item.kind(),
            item.summary(),
            item.signals().decided_by,
            item.signals().rule_hits,
            item.provenance().source,
            item.provenance().native_id,
        );
    }
    println!("persisted to {}; kind counts: {:?}", db_path.display(),
        almanac_core::db::count_items_by_kind(&conn)?);
    Ok(())
}

/// Diagnostic: compare our constructed Slack deep link against Slack's own
/// canonical permalink from chat.getPermalink (grounding verification).
async fn slack_permalink(channel: Option<String>, ts: Option<String>) -> Result<()> {
    let (channel, ts) = (
        channel.context("usage: slack-permalink <channel_id> <ts>")?,
        ts.context("usage: slack-permalink <channel_id> <ts>")?,
    );
    let auth = SlackAuth::from_env()?;
    let token = auth.user_token()?;
    let ours = almanac_core::adapters::slack::deep_link(&auth.workspace_url().await?, &channel, &ts);

    let mut url = url::Url::parse("https://slack.com/api/chat.getPermalink")?;
    url.query_pairs_mut().append_pair("channel", &channel).append_pair("message_ts", &ts);
    let resp = reqwest::Client::new().get(url).bearer_auth(&token).send().await?;
    let v: serde_json::Value = serde_json::from_str(&resp.text().await?)?;
    if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        anyhow::bail!(
            "chat.getPermalink failed: {}",
            v.get("error").and_then(serde_json::Value::as_str).unwrap_or("unknown_error")
        );
    }
    let canonical = v.get("permalink").and_then(serde_json::Value::as_str).unwrap_or("");
    println!("constructed: {ours}");
    println!("canonical:   {canonical}");
    println!("match: {}", ours == canonical);
    Ok(())
}

/// Diagnostic: recent extracted items and stored briefings (local console
/// only; summaries are local distillates and never leave the machine).
fn db_dump() -> Result<()> {
    let conn = almanac_core::db::open(&almanac_core::init_default_db()?)?;
    let mut stmt = conn.prepare(
        "SELECT ei.source, ei.native_id, ei.kind, ei.summary, so.occurred_at, ei.signals_json
         FROM extracted_items ei
         JOIN source_objects so ON so.source = ei.source AND so.native_id = ei.native_id
         WHERE ei.id = (SELECT MAX(id) FROM extracted_items
                        WHERE source = ei.source AND native_id = ei.native_id)
         ORDER BY so.occurred_at DESC LIMIT 40",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    println!("latest extraction per source object (newest first, max 40):");
    for row in rows {
        let (source, native_id, kind, summary, occurred_at, signals) = row?;
        let mut from = String::new();
        if source == "gmail" {
            if let Some(raw) = almanac_core::db::source_raw_json(
                &conn,
                almanac_core::types::SourceId::Gmail,
                &native_id,
            )? {
                let payload = raw.get("payload").cloned().unwrap_or(serde_json::Value::Null);
                from = almanac_core::adapters::gmail::header_values(&payload, "From")
                    .first()
                    .map(|s| format!(" | from: {s}"))
                    .unwrap_or_default();
            }
        }
        println!("  {occurred_at} [{kind:>13}] {source}:{native_id} — {summary}{from} | {signals}");
    }
    Ok(())
}

fn print_briefing(briefing: &almanac_core::synth::Briefing, elapsed: std::time::Duration) {
    println!("briefing (synthesis took {elapsed:?}):");
    for (i, item) in briefing.sequence.iter().enumerate() {
        println!(
            "  {}. [{}] {} ({}:{})",
            i + 1,
            item.kind,
            item.summary,
            item.provenance.source,
            item.provenance.native_id
        );
    }
    println!("  why: {}", briefing.rationale);
}

/// Synthesize a validated briefing for a UTC date (default: today).
async fn synthesize(date_arg: Option<String>) -> Result<()> {
    if network_reachable() {
        println!("network: REACHABLE — rerun with networking disabled to prove the offline gate");
    } else {
        println!("network: UNREACHABLE — offline run confirmed");
    }
    let date = match date_arg {
        Some(d) => d.parse().map_err(|e| anyhow::anyhow!("bad date '{d}': {e}"))?,
        // Briefing days are LOCAL calendar days (Phase 6).
        None => chrono::Local::now().date_naive(),
    };
    let backend = almanac_core::synth::local_llm::LocalLlmBackend::load(
        &almanac_core::models_dir()?.join("qwen2.5-0.5b-instruct"),
    )?;
    println!("backend: {}", almanac_core::synth::SynthesisBackend::backend_id(&backend));

    let db_path = almanac_core::init_default_db()?;
    let mut conn = almanac_core::db::open(&db_path)?;
    let ctx = almanac_core::synth::DayContext { date, now: Utc::now() };
    let started = std::time::Instant::now();
    let briefing = almanac_core::synth::generate_briefing(&conn, &backend, ctx).await?;
    almanac_core::db::insert_briefing(
        &mut conn,
        date,
        almanac_core::synth::SynthesisBackend::backend_id(&backend),
        &briefing,
    )?;
    print_briefing(&briefing, started.elapsed());
    println!("briefing persisted to SQLite");
    Ok(())
}

/// Fully offline end-to-end: fixtures → extraction → synthesis → validated
/// briefing, using the day (from the fixture data) that has non-noise items.
async fn e2e_fixtures() -> Result<()> {
    if network_reachable() {
        println!("network: REACHABLE — rerun with networking disabled to prove the offline gate");
    } else {
        println!("network: UNREACHABLE — offline run confirmed");
    }

    // Extraction (MiniLM, local files).
    let models = almanac_core::models_dir()?;
    let extractor = almanac_core::extract::Extractor::with_model(&models.join("minilm"))?;
    let objects = fixture_source_objects()?;
    let items = extractor.extract(&objects)?;
    let db_path = almanac_core::init_default_db()?;
    let mut conn = almanac_core::db::open(&db_path)?;
    for (obj, item) in objects.iter().zip(&items) {
        almanac_core::db::insert_source_object(&conn, obj)?;
        almanac_core::db::insert_extracted_item(&conn, item)?;
    }
    println!("extraction: {} fixture source objects classified + persisted", items.len());

    // Pick the most recent fixture day that has a non-noise item.
    let date: String = conn.query_row(
        "SELECT date(so.occurred_at)
         FROM extracted_items ei
         JOIN source_objects so ON so.source = ei.source AND so.native_id = ei.native_id
         WHERE ei.kind != 'noise'
         ORDER BY so.occurred_at DESC LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    let date: chrono::NaiveDate = date.parse()?;
    println!("briefing day selected from data: {date}");

    // Synthesis (Qwen, local files) + grounding validation.
    let backend = almanac_core::synth::local_llm::LocalLlmBackend::load(
        &models.join("qwen2.5-0.5b-instruct"),
    )?;
    let ctx = almanac_core::synth::DayContext { date, now: Utc::now() };
    let started = std::time::Instant::now();
    let briefing = almanac_core::synth::generate_briefing(&conn, &backend, ctx).await?;
    almanac_core::db::insert_briefing(
        &mut conn,
        date,
        almanac_core::synth::SynthesisBackend::backend_id(&backend),
        &briefing,
    )?;
    print_briefing(&briefing, started.elapsed());
    println!("grounding validation: PASSED (briefing was accepted); persisted to SQLite");
    Ok(())
}

/// Live smoke: adapters → extraction → SQLite. Not the test basis.
async fn extract_live() -> Result<()> {
    let window = TimeWindow { start: Utc::now() - Duration::days(7), end: Utc::now() };
    let extractor = almanac_core::extract::Extractor::with_model(
        &almanac_core::models_dir()?.join("minilm"),
    )?;

    let mut objects = Vec::new();
    let mut gmail = GmailAdapter::from_env()?;
    gmail.authenticate().await?;
    objects.extend(gmail.fetch_window(window).await?);
    let mut gcal = CalendarAdapter::from_env()?;
    gcal.authenticate().await?;
    objects.extend(gcal.fetch_window(window).await?);
    let mut slack = SlackAdapter::from_env()?;
    slack.authenticate().await?;
    objects.extend(slack.fetch_window(window).await?);

    let items = extractor.extract(&objects)?;
    let db_path = almanac_core::init_default_db()?;
    let conn = almanac_core::db::open(&db_path)?;
    for (obj, item) in objects.iter().zip(&items) {
        almanac_core::db::insert_source_object(&conn, obj)?;
        almanac_core::db::insert_extracted_item(&conn, item)?;
    }
    let mut by_kind = std::collections::BTreeMap::new();
    for item in &items {
        *by_kind.entry(item.kind().as_str()).or_insert(0usize) += 1;
    }
    println!("live extraction: {} source objects → {} items; kinds: {:?}", objects.len(), items.len(), by_kind);
    Ok(())
}

// ------------------------------------------------ action layer (2.0) ------

/// Walk and verify the hash-chained audit log. Any break fails loudly.
fn verify_chain() -> Result<()> {
    let conn = almanac_core::db::open(&almanac_core::init_default_db()?)?;
    let report = almanac_core::act::audit::verify_chain(&conn)?;
    println!(
        "audit chain OK — {} record(s) verified (genesis .. seq {}), head hash {}",
        report.records, report.head_seq, report.head_hash
    );
    Ok(())
}

/// Approval queue + audit tail (local console; drafts are our own text).
fn list_proposals() -> Result<()> {
    let mut conn = almanac_core::db::open(&almanac_core::init_default_db()?)?;
    let proposals = almanac_core::act::list_proposals(&mut conn)?;
    println!("proposals ({}):", proposals.len());
    for p in &proposals {
        println!(
            "  #{} [{}] {} — {} evidence ref(s), expires {}",
            p.id,
            p.state.as_str(),
            p.kind.as_str(),
            p.evidence.len(),
            p.expires_at
        );
        if let Some(s) = &p.draft_subject {
            println!("     subject: {s}");
        }
        println!("     body: {}", p.draft_body);
        for e in &p.evidence {
            println!(
                "     evidence[{}]: {}:{} -> {}",
                e.tier.as_str(),
                e.source,
                e.native_id,
                e.artifact_deep_link
            );
        }
    }
    println!("audit tail (newest first):");
    for r in almanac_core::act::audit::tail(&conn, 10)? {
        println!(
            "  seq {:>4}  {}  {:<10} {:<18} proposal={}",
            r.seq,
            r.ts,
            r.actor,
            r.event,
            r.proposal_id.map(|i| i.to_string()).unwrap_or_else(|| "-".into())
        );
    }
    Ok(())
}

/// DEV-ONLY seed path (clearly marked; see dispatch comment). Builds a test
/// proposal from a REAL stored source object so E2 holds — run
/// `live-briefing` or `extract-live` first to populate the store.
fn seed_proposal(which: Option<String>, native_id: Option<String>) -> Result<()> {
    use almanac_core::act::draft::{DraftingBackend, DraftRequest, TemplatedDraftingBackend};
    use almanac_core::act::{ActionKind, ActionProposal, ActionTarget, EvidenceKind, EvidenceRef};
    use almanac_core::types::SourceId;

    let mut conn = almanac_core::db::open(&almanac_core::init_default_db()?)?;
    let backend = TemplatedDraftingBackend;

    // Most recent stored source object of `source`, or the one named by id.
    let pick = |conn: &rusqlite::Connection, source: &str, id: Option<&str>| -> Result<(String, String, String)> {
        let (sql, param): (&str, &str) = match id {
            Some(id) => (
                "SELECT native_id, deep_link, occurred_at FROM source_objects
                 WHERE source = ?1 AND native_id = ?2",
                id,
            ),
            None => (
                "SELECT native_id, deep_link, occurred_at FROM source_objects
                 WHERE source = ?1 ORDER BY datetime(occurred_at) DESC LIMIT 1",
                "",
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let row = if param.is_empty() {
            stmt.query_row([source], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        } else {
            stmt.query_row([source, param], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        };
        row.with_context(|| format!("no stored {source} source object — fetch first (extract-live)"))
    };

    let (kind_label, id) = match which.as_deref() {
        Some("gmail-ack") => {
            let (native_id, deep_link, occurred_at) =
                pick(&conn, "gmail", native_id.as_deref())?;
            let raw = almanac_core::db::source_raw_json(&conn, SourceId::Gmail, &native_id)?
                .context("stored gmail object has no raw payload")?;
            let payload = raw.get("payload").cloned().unwrap_or(serde_json::Value::Null);
            let subject = almanac_core::adapters::gmail::header_values(&payload, "Subject")
                .first()
                .map(|s| s.to_string())
                .unwrap_or_default();
            let draft = backend.draft(&DraftRequest::AckReply { original_subject: subject })?;
            let evidence = vec![EvidenceRef {
                kind: EvidenceKind::Message,
                source: SourceId::Gmail,
                native_id: native_id.clone(),
                deep_link: Some(deep_link),
                observed_at: chrono::DateTime::parse_from_rfc3339(&occurred_at)?
                    .with_timezone(&chrono::Utc),
            }];
            let proposal = ActionProposal::new(
                ActionKind::GmailReply,
                ActionTarget::GmailThread { message_native_id: native_id },
                draft,
                evidence,
                backend.backend_id(),
            )?;
            let id = almanac_core::act::insert_proposal(
                &mut conn,
                &proposal,
                "dev-seed",
                chrono::Duration::hours(24),
            )?;
            ("gmail-ack reply", id)
        }
        Some("slack-check") => {
            let channel = std::env::var("SLACK_TEST_CHANNEL").context(
                "SLACK_TEST_CHANNEL is not set — add it to .env (the channel id of a \
                 private test channel you are a member of)",
            )?;
            let (native_id, deep_link, occurred_at) =
                pick(&conn, "slack", native_id.as_deref())?;
            let draft = backend.draft(&DraftRequest::SlackCheckInPost)?;
            let evidence = vec![EvidenceRef {
                kind: EvidenceKind::Message,
                source: SourceId::Slack,
                native_id,
                deep_link: Some(deep_link),
                observed_at: chrono::DateTime::parse_from_rfc3339(&occurred_at)?
                    .with_timezone(&chrono::Utc),
            }];
            let proposal = ActionProposal::new(
                ActionKind::SlackPost,
                ActionTarget::SlackChannel { channel_id: channel },
                draft,
                evidence,
                backend.backend_id(),
            )?;
            let id = almanac_core::act::insert_proposal(
                &mut conn,
                &proposal,
                "dev-seed",
                chrono::Duration::hours(24),
            )?;
            ("slack check-in post", id)
        }
        _ => anyhow::bail!("usage: debug-seed-proposal <gmail-ack|slack-check> [native_id]"),
    };

    let stored = almanac_core::act::load_proposal(&conn, id)?;
    let rendered = match stored.kind {
        ActionKind::GmailReply => {
            almanac_core::act::executors::render_gmail_reply(&conn, &stored)?
        }
        ActionKind::SlackPost => almanac_core::act::executors::render_slack_post(&stored)?,
    };
    println!("seeded {kind_label} as proposal #{id} (state: proposed)");
    println!("dry-run ({} bytes to {}):", rendered.api_body.len(), rendered.endpoint);
    println!("{}", rendered.display);
    println!("\napprove it in the app's approval queue (or reject it).");
    Ok(())
}
