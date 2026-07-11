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
        _ => {
            eprintln!(
                "usage: almanac-core <--self-check|smoke-adapters|refresh-google|refresh-slack>"
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
