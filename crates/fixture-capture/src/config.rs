use anyhow::{Context, Result};

/// Read a required env var (populated from .env by dotenvy in main).
pub fn required(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing env var {name} (see .env.example)"))
}
