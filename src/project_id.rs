//! Slice 2 of cli-to-cli-routing — project identity resolver.
//!
//! Resolves a STABLE, cross-clone `project_id` for the agent_registry
//! v6 column added in Slice 1. Two clones (or git worktrees) of the
//! SAME upstream repo MUST hash to the SAME `project_id` so they
//! appear together under `claudebase agent list-alive --project current`
//! (Slice 6). Two unrelated cwds MUST hash to different `project_id`s.
//!
//! Three-step fallback chain:
//!
//! 1. **git remote origin URL** — `git -C <cwd> config --get
//!    remote.origin.url` normalized to `host/owner/repo` (lowercase,
//!    `.git` suffix stripped, SSH/HTTPS/scheme prefixes folded into the
//!    same shape). Covers the 99% case.
//! 2. **`<cwd>/.claudebase/config.json::project_id`** — manual operator
//!    override for forks-treated-as-upstream, monorepo splits, or
//!    non-git projects. Fires when Step 1 returns nothing (no git
//!    binary, no origin remote, no command success).
//! 3. **`local:<sha256(canonical_path)[..16]>`** — path-hash fallback
//!    for non-git folders with no manual override. Different cwds get
//!    different `project_id`s; same cwd is stable across daemon
//!    restarts. Returns `local:unknown` if canonicalization fails (rare
//!    — virtual fs, deleted path mid-call).

use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

/// Resolve the stable `project_id` for the given working directory.
/// See module docstring for the fallback chain.
///
/// Never panics. The function is pure relative to its inputs except
/// for: (a) shelling out to `git` (filesystem side effect None;
/// network side effect None — `git config --get remote.origin.url`
/// reads `.git/config` locally), (b) reading
/// `<cwd>/.claudebase/config.json` (filesystem read), (c) calling
/// `std::path::Path::canonicalize` (filesystem read for symlink
/// resolution). All three are intentional per the contract.
pub fn resolve_project_id(cwd: &Path) -> String {
    if let Some(url) = git_remote_origin_url(cwd) {
        if let Some(normalized) = normalize_remote_url(&url) {
            return normalized;
        }
    }
    if let Some(pid) = read_project_id_from_config(cwd) {
        return pid;
    }
    sha_local_id(cwd)
}

/// Normalize a remote URL to the `host/owner/repo` canonical shape.
/// Returns `None` for empty/whitespace input.
///
/// Handled prefixes (folded away):
///   - `https://`, `http://`
///   - `ssh://`, `ssh://git@`
///   - `git@host:owner/repo.git` (SSH alias form — `git@` prefix and
///     `:` host/path separator)
///
/// Handled suffixes (stripped):
///   - `.git`
///   - trailing `/`
///
/// Then the entire string is lowercased so the SAME repo cloned via
/// different syntaxes (e.g. `https://GitHub.com/Foo/Bar.git` vs
/// `git@github.com:foo/bar.git`) resolves to the SAME `project_id`.
///
/// `pub` (not `pub(crate)`) so the dedicated test file
/// `tests/project_id_test.rs` can exercise the function directly
/// without the cost of spinning up a real git repo.
pub fn normalize_remote_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Lowercase FIRST so scheme prefix matches are case-insensitive
    // (`HTTPS://`, `Ssh://Git@`, etc. all fold to the same shape).
    // Path components (owner/repo) get lowercased too, which is the
    // intent — GitHub treats `Foo/Bar` and `foo/bar` as the same repo
    // on URL but stores the canonical casing in the API; lowercase
    // is the safe lossy choice for cross-clone equivalence.
    let mut s = trimmed.to_lowercase();

    // SSH alias form FIRST — must be checked before bare prefix strips
    // because the prefix is `git@` not a scheme.
    if let Some(rest) = s.strip_prefix("git@") {
        if let Some(colon) = rest.find(':') {
            let (host, path) = rest.split_at(colon);
            // path still includes the leading ':'
            let path = &path[1..];
            s = format!("{host}/{path}");
        }
    } else if let Some(rest) = s.strip_prefix("ssh://git@") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("ssh://") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("https://") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("http://") {
        s = rest.to_string();
    }

    if let Some(prefix) = s.strip_suffix(".git") {
        s = prefix.to_string();
    }
    s = s.trim_end_matches('/').to_string();

    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn git_remote_origin_url(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn read_project_id_from_config(cwd: &Path) -> Option<String> {
    let path = cwd.join(".claudebase").join("config.json");
    let bytes = std::fs::read(&path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let pid = value.get("project_id")?.as_str()?;
    let trimmed = pid.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn sha_local_id(cwd: &Path) -> String {
    let canonical = match cwd.canonicalize() {
        Ok(p) => p,
        Err(_) => return "local:unknown".to_string(),
    };
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    // 8 bytes → 16 hex chars. Stable across daemon restarts because
    // the canonical path resolves the same.
    let hex: String = digest.iter().take(8).map(|b| format!("{:02x}", b)).collect();
    format!("local:{hex}")
}
