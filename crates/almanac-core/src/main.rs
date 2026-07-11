use std::process::ExitCode;

use almanac_core::adapters::{
    gcal::CalendarAdapter, gmail::GmailAdapter, slack::SlackAdapter, SourceAdapter,
};
use almanac_core::auth::{GoogleAuth, SlackAuth};
use almanac_core::types::TimeWindow;
use anyhow::Result;
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
        _ => {
            eprintln!(
                "usage: almanac-core <--self-check|smoke-adapters|refresh-google|refresh-slack|extract-fixtures|extract-live>"
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

    let model_dir = std::path::Path::new("models").join("minilm");
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

/// Live smoke: adapters → extraction → SQLite. Not the test basis.
async fn extract_live() -> Result<()> {
    let window = TimeWindow { start: Utc::now() - Duration::days(7), end: Utc::now() };
    let extractor =
        almanac_core::extract::Extractor::with_model(&std::path::Path::new("models").join("minilm"))?;

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
