//! Phase 2.2 payload capture: real `git log` output → redacted text fixtures.
//!
//! Git output is raw TEXT (records NUL-separated, fields US-separated — see
//! `almanac_core::git::LOG_FORMAT`), so it does not go through the JSON fixture
//! path. We save the exact bytes to gitignored `.fixtures-raw/` and a redacted
//! copy (author identities scrubbed, `redact::redact_git`) to
//! `FIXTURES_DIR/git_log/`. Uses the SAME format constant the live GitWatcher
//! parses, so fixtures cannot drift from the code that consumes them.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{ensure, Context, Result};

use crate::redact;

fn run_git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("running git in {} (is git on PATH?)", repo.display()))?;
    ensure!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}

fn write_fixture(label: &str, name: &str, bytes: &[u8], redacted: &str) -> Result<()> {
    let raw_dir = PathBuf::from(".fixtures-raw").join("git_log");
    std::fs::create_dir_all(&raw_dir)?;
    std::fs::write(raw_dir.join(format!("{label}_{name}")), bytes)?;

    let root = std::env::var("FIXTURES_DIR").unwrap_or_else(|_| "./fixtures".to_string());
    let dir = PathBuf::from(root).join("git_log");
    std::fs::create_dir_all(&dir)?;
    // Write bytes exactly (no newline translation) so a message's LF stays LF.
    std::fs::write(dir.join(format!("{label}_{name}")), redacted.as_bytes())?;
    println!("saved fixture git_log/{label}_{name} ({} bytes redacted)", redacted.len());
    Ok(())
}

/// Capture `git log --all` (all branches' commits) + the branch list from
/// `repo`, redact author PII, and write both fixtures under label `label`.
pub fn capture(repo: Option<&String>, label: Option<&String>) -> Result<()> {
    let repo = repo.context("usage: fixture-capture git <repo-path> <label>")?;
    let label = label.context("usage: fixture-capture git <repo-path> <label>")?;
    let repo = PathBuf::from(repo);
    ensure!(repo.exists(), "{} does not exist", repo.display());

    let fmt = format!("--pretty=format:{}", almanac_core::git::LOG_FORMAT);
    // All commits from all branches, colorless, NUL-separated records.
    let log = run_git(&repo, &["log", "--all", "-z", "--no-color", &fmt])?;
    let redacted = redact::redact_git(&String::from_utf8_lossy(&log));
    write_fixture(label, "all.txt", &log, &redacted)?;

    let branches = run_git(&repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
    let branches_str = String::from_utf8_lossy(&branches).into_owned();
    write_fixture(label, "branches.txt", &branches, &branches_str)?;

    Ok(())
}
