//! Token storage (DPAPI-encrypted, at the gitignored paths from .env) and
//! fixture writing (raw copy to gitignored .fixtures-raw/, redacted
//! structure-preserving copy to FIXTURES_DIR).

use std::path::PathBuf;

use anyhow::{Context, Result};
use base64::Engine as _;
use serde_json::Value;

use crate::dpapi;

pub fn save_token(path_env: &str, token_json: &Value) -> Result<PathBuf> {
    let path = PathBuf::from(crate::config::required(path_env)?);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let plaintext = serde_json::to_vec(token_json)?;
    let blob = dpapi::protect(&plaintext)?;
    let envelope = serde_json::json!({
        "format": "dpapi-v1",
        "note": "DPAPI-encrypted OAuth token bound to this Windows user. Not plaintext.",
        "ciphertext_b64": base64::engine::general_purpose::STANDARD.encode(&blob),
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&envelope)?)?;
    Ok(path)
}

pub fn load_token(path_env: &str) -> Result<Value> {
    let path = PathBuf::from(crate::config::required(path_env)?);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("no token at {} — run the auth step first", path.display()))?;
    let envelope: Value = serde_json::from_str(&raw)?;
    let b64 = envelope
        .get("ciphertext_b64")
        .and_then(Value::as_str)
        .context("token file is not a dpapi-v1 envelope")?;
    let blob = base64::engine::general_purpose::STANDARD.decode(b64)?;
    let plaintext = dpapi::unprotect(&blob)?;
    Ok(serde_json::from_slice(&plaintext)?)
}

/// Save a captured response: exact raw bytes to .fixtures-raw/ (gitignored,
/// local only) and a redacted, structure-preserving copy to FIXTURES_DIR.
/// Returns the parsed (unredacted) value for follow-up calls.
pub fn save_fixture(source: &str, name: &str, raw_body: &str) -> Result<Value> {
    let value: Value =
        serde_json::from_str(raw_body).context("response body was not valid JSON")?;

    let raw_dir = PathBuf::from(".fixtures-raw").join(source);
    std::fs::create_dir_all(&raw_dir)?;
    std::fs::write(raw_dir.join(name), raw_body)?;

    let fixtures_root =
        std::env::var("FIXTURES_DIR").unwrap_or_else(|_| "./fixtures".to_string());
    let fixtures_dir = PathBuf::from(fixtures_root).join(source);
    std::fs::create_dir_all(&fixtures_dir)?;
    let mut redacted = value.clone();
    crate::redact::redact(source, &mut redacted);
    std::fs::write(
        fixtures_dir.join(name),
        serde_json::to_string_pretty(&redacted)?,
    )?;

    println!("saved fixture {source}/{name}");
    Ok(value)
}
