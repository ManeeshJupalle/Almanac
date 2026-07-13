//! Phase 1 fixture-capture tool.
//!
//! Stands up the minimum OAuth needed to make one real authenticated call per
//! source and saves the raw JSON responses as fixtures. Deliberately contains
//! NO typed API models and NO adapters — those are Phase 2, and per the
//! payload-first invariant they must be written against these fixtures.

mod config;
mod google;
mod loopback;
mod redact;
mod slack;
mod store;

use anyhow::{bail, Result};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("google-auth") => google::authorize().await,
        Some("slack-auth") => slack::authorize().await,
        Some("capture") => match args.get(1).map(String::as_str) {
            Some("gmail") => google::capture_gmail().await,
            Some("gcal") => google::capture_gcal().await,
            Some("slack") => slack::capture().await,
            // Phase 2.0 write-path captures (payload-first for send/post).
            Some("gmail-send") => google::capture_gmail_send().await,
            Some("slack-post") => slack::capture_post().await,
            Some("all") => {
                google::capture_gmail().await?;
                google::capture_gcal().await?;
                slack::capture().await
            }
            _ => bail!("usage: fixture-capture capture <gmail|gcal|slack|gmail-send|slack-post|all>"),
        },
        _ => bail!(
            "usage: fixture-capture <google-auth|slack-auth|capture <gmail|gcal|slack|gmail-send|slack-post|all>>"
        ),
    }
}
