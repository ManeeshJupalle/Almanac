//! Minimal loopback HTTP listener that waits for a single OAuth redirect and
//! returns its query parameters.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn accept_any(listeners: &[TcpListener]) -> std::io::Result<TcpStream> {
    match listeners {
        [only] => only.accept().await.map(|(s, _)| s),
        [a, b, ..] => tokio::select! {
            r = a.accept() => r.map(|(s, _)| s),
            r = b.accept() => r.map(|(s, _)| s),
        },
        [] => Err(std::io::Error::other("no listeners")),
    }
}

pub async fn wait_for_redirect(
    listeners: Vec<TcpListener>,
    expected_path: &str,
    timeout_secs: u64,
) -> Result<HashMap<String, String>> {
    let fut = async {
        loop {
            let mut stream = accept_any(&listeners).await?;
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await?;
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let target = request
                .lines()
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let (path, query) = match target.split_once('?') {
                Some((p, q)) => (p.to_string(), q.to_string()),
                None => (target, String::new()),
            };
            if path != expected_path {
                let _ = stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue; // favicon requests etc.
            }
            let params: HashMap<String, String> =
                url::form_urlencoded::parse(query.as_bytes())
                    .into_owned()
                    .collect();
            let body =
                "<html><body><h2>Almanac: authorization received.</h2><p>You can close this tab.</p></body></html>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            return Ok::<_, anyhow::Error>(params);
        }
    };
    tokio::time::timeout(Duration::from_secs(timeout_secs), fut)
        .await
        .context("timed out waiting for the OAuth redirect (consent not completed?)")?
}
