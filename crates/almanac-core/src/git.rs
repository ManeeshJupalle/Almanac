//! GitWatcher (Phase 2.2): a local, network-free observer that turns commits in
//! configured repositories into Tier-Hard `SourceObject`s for the correlation
//! engine (ARCHITECTURE_V2; BUILD_PHASES_V2 Phase 2.2).
//!
//! **A2 scope isolation:** this module takes ONLY filesystem paths and shells
//! out to `git`. It reads no token, holds no `*Auth`, and opens no network
//! socket — it cannot reach Gmail/Slack/Jira credentials, and they cannot reach
//! it. The only thing it can do is *read* local history.
//!
//! **Payload-first (see PAYLOAD_CORRECTIONS.md, G-series):** commits are parsed
//! from a real `git log` byte stream, not modeled from docs. The exact command
//! and its collision-proof delimiters are defined by [`LOG_FORMAT`] /
//! [`log_args`]; fixtures in `fixtures/git_log/` are captured with the SAME
//! format via `fixture-capture git`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};

use crate::types::{ProvenanceRef, RawContent, SourceId, SourceObject};

/// `git log` pretty-format producing one record per commit with US-separated
/// fields and the raw message LAST. Field order (7 fields):
/// `full-sha ⟂ short-sha ⟂ author-name ⟂ author-email ⟂ author-date-iso ⟂
///  parent-shas ⟂ raw-message`.
///
/// **Delimiters (G-1): why they cannot collide with message content.**
/// - Records are separated by NUL (`-z`, [`log_args`]). Git forbids a NUL byte
///   inside a commit object, so a NUL can NEVER appear inside a record — record
///   boundaries are collision-*proof*, not merely unlikely.
/// - Fields are separated by US = `0x1f` (`%x1f`). The one free-text field (the
///   raw message `%B`) is LAST and is parsed with `splitn`, so any delimiter or
///   newline inside the message is absorbed into that final field and cannot
///   shift a boundary. The only residual risk is a raw `0x1f` inside an author
///   NAME/EMAIL or an ISO date — none of which git identities or dates contain.
pub const LOG_FORMAT: &str = "%H\u{1f}%h\u{1f}%an\u{1f}%ae\u{1f}%aI\u{1f}%P\u{1f}%B";

const FIELD_SEP: char = '\u{1f}';
const RECORD_SEP: char = '\0';

/// Args for a windowed, branch-scoped, colorless, NUL-separated `git log`.
/// `since` bounds the window (commit date); `revision` is a branch/ref name.
pub fn log_args<'a>(revision: &'a str, since: &'a str) -> Vec<String> {
    vec![
        "log".into(),
        revision.into(),
        "-z".into(),
        "--no-color".into(),
        format!("--since={since}"),
        format!("--pretty=format:{LOG_FORMAT}"),
    ]
}

/// One parsed commit. `branches`, `repo_*`, and `deep_link` are context filled
/// by [`GitWatcher::collect`]; [`parse_commits`] leaves them empty.
#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub short_sha: String,
    pub author_name: String,
    pub author_email: String,
    /// Author date (`%aI`, strict ISO-8601 WITH a colon in the offset — unlike
    /// Jira's colon-less timestamps, so `parse_from_rfc3339` accepts it, G-4).
    pub committed_at: DateTime<Utc>,
    /// Parent shas; two or more ⇒ a merge commit.
    pub parents: Vec<String>,
    /// First line of the message.
    pub subject: String,
    /// Message minus the subject line (may be multi-line; empty if none).
    pub body: String,
    /// Full raw message (`%B`), trimmed — what the correlator scans for keys.
    pub message: String,
    /// Local branches that contain this commit (union across the scan).
    pub branches: Vec<String>,
    pub repo_name: String,
    pub repo_path: String,
    /// Evidence ref: an `https://…/commit/<sha>` web URL when the repo has a
    /// github.com https remote, else the local form `git-local://<repo>/commit/
    /// <sha>` (G-6). Empty until `collect` fills it.
    pub deep_link: String,
}

impl Commit {
    pub fn is_merge(&self) -> bool {
        self.parents.len() >= 2
    }
}

/// Parse a raw `git log -z --pretty=format:LOG_FORMAT` byte stream into commits.
/// Branch/repo/deep_link are left empty (context the watcher attaches). Pure and
/// offline — the unit of the parsing tests.
pub fn parse_commits(blob: &[u8]) -> Result<Vec<Commit>> {
    // Git output is UTF-8 (author names/messages are stored as bytes; git emits
    // them verbatim — the scratch fixture carries é/ü/☕ to prove it, G-3).
    let text = std::str::from_utf8(blob).context("git log output was not valid utf-8")?;
    let mut commits = Vec::new();
    for record in text.split(RECORD_SEP) {
        // `-z` separates records with NUL; the last record has no trailing NUL,
        // so a split never yields a spurious empty tail — but guard anyway.
        if record.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.splitn(7, FIELD_SEP).collect();
        ensure!(
            fields.len() == 7,
            "git record has {} field(s), expected 7 — LOG_FORMAT drift?",
            fields.len()
        );
        let sha = fields[0].trim().to_string();
        ensure!(
            sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()),
            "git record sha '{sha}' is not a 40-char hex hash"
        );
        let committed_at = DateTime::parse_from_rfc3339(fields[4].trim())
            .with_context(|| format!("unparseable git author date '{}'", fields[4]))?
            .with_timezone(&Utc);
        let parents =
            fields[5].split_whitespace().map(str::to_string).collect::<Vec<String>>();
        // Trim only trailing whitespace/newlines git appends before the NUL.
        let message = fields[6].trim_end().to_string();
        let (subject, body) = split_subject_body(&message);
        commits.push(Commit {
            sha,
            short_sha: fields[1].trim().to_string(),
            author_name: fields[2].to_string(),
            author_email: fields[3].to_string(),
            committed_at,
            parents,
            subject,
            body,
            message,
            branches: Vec::new(),
            repo_name: String::new(),
            repo_path: String::new(),
            deep_link: String::new(),
        });
    }
    Ok(commits)
}

/// Split a raw message into (subject, body). Git normalizes message line
/// endings to LF in `log` output even when authored with CRLF on Windows
/// (G-2), but we still strip a stray CR defensively.
fn split_subject_body(message: &str) -> (String, String) {
    let mut parts = message.splitn(2, '\n');
    let subject = parts.next().unwrap_or("").trim_end_matches('\r').trim().to_string();
    let body = parts.next().unwrap_or("").trim().to_string();
    (subject, body)
}

/// A git commit as a grounded source object: `source = git`, `native_id` is the
/// full sha, `deep_link` is the web/local evidence ref, `raw` carries the parsed
/// fields (local-only). Feeds the same `source_objects` store + composite FK as
/// every other source, so commit evidence resolves at rest exactly like Jira/
/// Gmail (E2) — no new artifact-store shape.
pub fn to_source_object(commit: &Commit) -> SourceObject {
    let raw = serde_json::json!({
        "sha": commit.sha,
        "short_sha": commit.short_sha,
        "subject": commit.subject,
        "body": commit.body,
        "message": commit.message,
        "author_name": commit.author_name,
        "author_email": commit.author_email,
        "committed_at": commit.committed_at.to_rfc3339(),
        "parents": commit.parents,
        "is_merge": commit.is_merge(),
        "branches": commit.branches,
        "repo": commit.repo_name,
        "repo_path": commit.repo_path,
    });
    SourceObject {
        provenance: ProvenanceRef {
            source: SourceId::Git,
            native_id: commit.sha.clone(),
            deep_link: commit.deep_link.clone(),
        },
        raw: RawContent::new(raw),
        occurred_at: commit.committed_at,
    }
}

/// Watches configured local repositories. Network-free, credential-free (A2).
pub struct GitWatcher {
    repos: Vec<PathBuf>,
}

impl GitWatcher {
    pub fn new(repos: Vec<PathBuf>) -> Self {
        Self { repos }
    }

    /// Repos from `ALMANAC_GIT_REPOS` (`;`-separated absolute paths). No token
    /// or credential is read here — filesystem paths only (A2).
    pub fn from_env() -> Result<Self> {
        let raw = std::env::var("ALMANAC_GIT_REPOS")
            .context("ALMANAC_GIT_REPOS not set — add it to .env (see .env.example)")?;
        let repos: Vec<PathBuf> = raw
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        ensure!(!repos.is_empty(), "ALMANAC_GIT_REPOS is empty");
        Ok(Self::new(repos))
    }

    pub fn repos(&self) -> &[PathBuf] {
        &self.repos
    }

    /// Collect commits from every configured repo, tagged with the branches that
    /// contain them, since `since`. Deduped by sha (branch sets unioned). Pure
    /// local reads — one `git` process per branch, no network.
    pub fn collect(&self, since: DateTime<Utc>) -> Result<Vec<Commit>> {
        let since = since.to_rfc3339();
        let mut by_sha: BTreeMap<String, Commit> = BTreeMap::new();
        for repo in &self.repos {
            ensure!(repo.join(".git").exists() || is_git_dir(repo)?, "{} is not a git repository", repo.display());
            let repo_name = repo
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| repo.display().to_string());
            let web_base = github_web_base(repo);
            for branch in list_branches(repo)? {
                let blob = run_git_bytes(repo, &log_args(&branch, &since))?;
                for mut commit in parse_commits(&blob)? {
                    match by_sha.get_mut(&commit.sha) {
                        Some(existing) => {
                            if !existing.branches.contains(&branch) {
                                existing.branches.push(branch.clone());
                            }
                        }
                        None => {
                            commit.branches = vec![branch.clone()];
                            commit.repo_name = repo_name.clone();
                            commit.repo_path = repo.display().to_string();
                            commit.deep_link = deep_link(web_base.as_deref(), &repo_name, &commit.sha);
                            by_sha.insert(commit.sha.clone(), commit);
                        }
                    }
                }
            }
        }
        Ok(by_sha.into_values().collect())
    }
}

/// Evidence ref for a commit: a real github web URL when derivable, else the
/// documented local form (G-6). The full sha is the authoritative identifier
/// (also the source-object native_id); `git -C <repo> show <sha>` verifies it.
fn deep_link(web_base: Option<&str>, repo_name: &str, sha: &str) -> String {
    match web_base {
        Some(base) => format!("{base}/commit/{sha}"),
        None => format!("git-local://{repo_name}/commit/{sha}"),
    }
}

/// Conservative web-base derivation: ONLY github.com https remotes, whose
/// `/commit/<sha>` path we can build correctly. Anything else (ssh, other hosts,
/// no remote) returns None → local ref, so we never emit a *wrong* clickable
/// link (a broken link would be worse than an honest local ref).
fn github_web_base(repo: &Path) -> Option<String> {
    let url = run_git_string(repo, &["config", "--get", "remote.origin.url"]).ok()?;
    github_web_base_from_url(url.trim())
}

/// Pure derivation half (testable without a repo).
fn github_web_base_from_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://github.com/")?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    Some(format!("https://github.com/{}", rest.trim_end_matches('/')))
}

fn list_branches(repo: &Path) -> Result<Vec<String>> {
    let out = run_git_string(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
    Ok(out.lines().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
}

fn is_git_dir(repo: &Path) -> Result<bool> {
    Ok(run_git_string(repo, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false))
}

fn run_git_bytes(repo: &Path, args: &[String]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("running `git` in {} (is git on PATH?)", repo.display()))?;
    ensure!(
        out.status.success(),
        "git {:?} failed in {}: {}",
        args,
        repo.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}

fn run_git_string(repo: &Path, args: &[&str]) -> Result<String> {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    Ok(String::from_utf8_lossy(&run_git_bytes(repo, &owned)?).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw `-z` blob from records so tests mirror real git bytes without
    /// shelling out (US between fields, NUL between records).
    fn blob(records: &[String]) -> Vec<u8> {
        records.join("\0").into_bytes()
    }
    fn rec(sha40: &str, short: &str, name: &str, email: &str, date: &str, parents: &str, msg: &str) -> String {
        format!("{sha40}\u{1f}{short}\u{1f}{name}\u{1f}{email}\u{1f}{date}\u{1f}{parents}\u{1f}{msg}")
    }

    const SHA_A: &str = "097d3daa91555f704bbf0bd1b2eb2d41e94c0961";
    const SHA_B: &str = "4f46607cff70a287282997b37d0927a163210605";

    #[test]
    fn parses_subject_body_date_and_short_sha() {
        let b = blob(&[rec(
            SHA_A,
            "097d3da",
            "Alma Nemo",
            "alma@example.test",
            "2026-07-11T03:00:00-05:00",
            "4f46607cff70a287282997b37d0927a163210605",
            "ALM-1: initial skeleton\n\nBody line one.\nBody line two.\n",
        )]);
        let commits = parse_commits(&b).unwrap();
        assert_eq!(commits.len(), 1);
        let c = &commits[0];
        assert_eq!(c.sha, SHA_A);
        assert_eq!(c.short_sha, "097d3da");
        assert_eq!(c.subject, "ALM-1: initial skeleton");
        assert_eq!(c.body, "Body line one.\nBody line two.");
        assert_eq!(c.committed_at.to_rfc3339(), "2026-07-11T08:00:00+00:00");
        assert!(!c.is_merge());
    }

    #[test]
    fn detects_merge_via_two_parents() {
        let b = blob(&[rec(
            SHA_B,
            "4f46607",
            "Alma Nemo",
            "alma@example.test",
            "2026-07-11T00:00:00-05:00",
            "913a880ce42b2cb47c3a5cef5d545ae80c704b6c 16acb1a2f7e3268dfd8ef327eec29fced4cca51d",
            "Merge branch 'feature/ALM-2' for ALM-4 rollup\n",
        )]);
        let c = &parse_commits(&b).unwrap()[0];
        assert!(c.is_merge(), "two parents ⇒ merge");
        assert_eq!(c.parents.len(), 2);
        assert_eq!(c.subject, "Merge branch 'feature/ALM-2' for ALM-4 rollup");
    }

    #[test]
    fn preserves_non_ascii_author_and_message() {
        let b = blob(&[rec(
            SHA_A,
            "097d3da",
            "Renée Müller",
            "renee.muller@example.test",
            "2026-07-10T21:00:00-05:00",
            "bb1efe3aa11223344556677889900aabbccddeeff",
            "Café ☕ ünïcode cleanup — smörgåsbord\n",
        )]);
        let c = &parse_commits(&b).unwrap()[0];
        assert_eq!(c.author_name, "Renée Müller");
        assert_eq!(c.subject, "Café ☕ ünïcode cleanup — smörgåsbord");
    }

    #[test]
    fn empty_parents_is_root_not_merge() {
        let b = blob(&[rec(
            SHA_A, "097d3da", "Alma Nemo", "alma@example.test",
            "2026-07-10T09:00:00-05:00", "", "ALM-1: root commit\n",
        )]);
        let c = &parse_commits(&b).unwrap()[0];
        assert!(c.parents.is_empty());
        assert!(!c.is_merge());
    }

    #[test]
    fn field_separator_inside_message_does_not_shift_boundaries() {
        // A pathological US byte INSIDE the message is absorbed by the final
        // splitn field — the sha/date/etc. still parse correctly.
        let msg = "Subject with \u{1f} embedded\n\nbody";
        let b = blob(&[rec(
            SHA_A, "097d3da", "Alma Nemo", "alma@example.test",
            "2026-07-10T09:00:00-05:00", "", msg,
        )]);
        let c = &parse_commits(&b).unwrap()[0];
        assert_eq!(c.sha, SHA_A);
        assert_eq!(c.committed_at.to_rfc3339(), "2026-07-10T14:00:00+00:00");
    }

    #[test]
    fn deep_link_prefers_github_then_local() {
        assert_eq!(
            deep_link(Some("https://github.com/o/r"), "r", "abc"),
            "https://github.com/o/r/commit/abc"
        );
        assert_eq!(deep_link(None, "alm-scratch", "abc"), "git-local://alm-scratch/commit/abc");
    }

    #[test]
    fn github_web_base_only_https_github() {
        // github https → web base (with/without .git suffix).
        assert_eq!(
            github_web_base_from_url("https://github.com/ManeeshJupalle/Almanac.git"),
            Some("https://github.com/ManeeshJupalle/Almanac".to_string())
        );
        assert_eq!(
            github_web_base_from_url("https://github.com/o/r"),
            Some("https://github.com/o/r".to_string())
        );
        // ssh / other hosts → None (fall back to local ref, never a wrong link).
        assert_eq!(github_web_base_from_url("git@github.com:o/r.git"), None);
        assert_eq!(github_web_base_from_url("https://gitlab.com/o/r.git"), None);
    }
}
