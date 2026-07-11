//! Token storage (delegated to almanac-core's encrypted store, at the
//! gitignored paths from .env) and fixture writing (raw copy to gitignored
//! .fixtures-raw/, redacted structure-preserving copy to FIXTURES_DIR).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

pub fn save_token(path_env: &str, token_json: &Value) -> Result<PathBuf> {
    let path = PathBuf::from(crate::config::required(path_env)?);
    almanac_core::auth::save_token(&path, token_json)?;
    Ok(path)
}

pub fn load_token(path_env: &str) -> Result<Value> {
    let path = PathBuf::from(crate::config::required(path_env)?);
    almanac_core::auth::load_token(&path)
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
