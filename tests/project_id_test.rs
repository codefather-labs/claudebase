//! cli-to-cli-routing Slice 2 — project_id resolver tests.
//!
//! Coverage (QA cases TC-C2C-6.1..6.5, TC-C2C-8.1/2, TC-C2C-12.1/2,
//! TC-C2C-13.1/2 + UC-C2C-6/8/12/13):
//!
//! - HTTPS URL normalization → host/owner/repo (TC-C2C-6.1)
//! - SSH URL (git@host:owner/repo.git) normalization (TC-C2C-6.2)
//! - .git suffix stripping (TC-C2C-6.3)
//! - Case normalization to lowercase (TC-C2C-6.4)
//! - Trailing slash strip
//! - Non-git cwd falls through to local:<sha-prefix> (TC-C2C-6.5 / UC-C2C-13)
//! - .claudebase/config.json::project_id overrides Step 1 only when Step 1
//!   fails. Actual plan: Step 1 (git) > Step 2 (config) > Step 3 (path hash).
//!   The config override is Step 2 — it fires when git remote is unavailable.
//! - HTTPS and SSH for the SAME repo normalize to the SAME project_id
//!   (UC-C2C-12 — two clones with different remote URL syntax)
//! - Fork with different origin → different project_id (UC-C2C-8)
//! - Integration: real git repo in tempdir with mocked remote URL.

use claudebase::project_id::{normalize_remote_url, resolve_project_id};
use std::fs;
use tempfile::TempDir;

// ----------------------------------------------------------------------------
// normalize_remote_url — pure function tests
// ----------------------------------------------------------------------------

#[test]
fn tc_c2c_6_1_https_url_normalizes_to_host_path() {
    let r = normalize_remote_url("https://github.com/foo/bar").unwrap();
    assert_eq!(r, "github.com/foo/bar");
}

#[test]
fn tc_c2c_6_1_https_url_with_dot_git_strips_suffix() {
    let r = normalize_remote_url("https://github.com/foo/bar.git").unwrap();
    assert_eq!(r, "github.com/foo/bar");
}

#[test]
fn tc_c2c_6_2_ssh_git_at_url_normalizes() {
    let r = normalize_remote_url("git@github.com:foo/bar.git").unwrap();
    assert_eq!(r, "github.com/foo/bar");
}

#[test]
fn tc_c2c_6_2_ssh_scheme_url_normalizes() {
    let r = normalize_remote_url("ssh://git@github.com/foo/bar.git").unwrap();
    assert_eq!(r, "github.com/foo/bar");
}

#[test]
fn tc_c2c_6_4_case_normalization_lowercases() {
    let r = normalize_remote_url("https://GitHub.com/Foo/Bar.git").unwrap();
    assert_eq!(r, "github.com/foo/bar");
}

#[test]
fn trailing_slash_is_stripped() {
    let r = normalize_remote_url("https://github.com/foo/bar/").unwrap();
    assert_eq!(r, "github.com/foo/bar");
}

#[test]
fn tc_c2c_12_https_and_ssh_normalize_to_same_id() {
    // UC-C2C-12: two clones of the SAME repo via different URL syntax
    // MUST normalize to the SAME project_id so the two CCs see each
    // other under `claudebase agent list-alive --project current`.
    let https = normalize_remote_url("https://github.com/foo/bar.git").unwrap();
    let ssh = normalize_remote_url("git@github.com:foo/bar.git").unwrap();
    let ssh_scheme = normalize_remote_url("ssh://git@github.com/foo/bar.git").unwrap();
    let uppercase = normalize_remote_url("HTTPS://GitHub.COM/foo/bar").unwrap();
    assert_eq!(https, ssh);
    assert_eq!(https, ssh_scheme);
    assert_eq!(https, uppercase);
}

#[test]
fn tc_c2c_8_fork_with_different_origin_gets_different_id() {
    // UC-C2C-8: a fork has a different remote URL → it lives in a
    // different project_id namespace. Operator wanting the fork to
    // be considered "same project" uses .claudebase/config.json
    // override (Step 2) per R-C2C-1 mitigation.
    let upstream = normalize_remote_url("https://github.com/orig/bar.git").unwrap();
    let fork = normalize_remote_url("https://github.com/fork/bar.git").unwrap();
    assert_ne!(upstream, fork);
}

#[test]
fn empty_url_returns_none() {
    assert_eq!(normalize_remote_url(""), None);
    assert_eq!(normalize_remote_url("   "), None);
}

// ----------------------------------------------------------------------------
// resolve_project_id — full resolver tests (Step 2 / Step 3 fallbacks)
// ----------------------------------------------------------------------------

#[test]
fn tc_c2c_6_5_no_git_no_config_returns_local_hash() {
    // UC-C2C-13 / TC-C2C-6.5: cwd is a plain non-git directory with no
    // .claudebase/config.json → resolver falls through to Step 3
    // (sha256 of canonical path). Result must start with `local:`.
    let tmp = TempDir::new().expect("tempdir");
    let id = resolve_project_id(tmp.path());
    assert!(
        id.starts_with("local:"),
        "expected local: prefix, got {id}"
    );
    assert_eq!(id.len(), "local:".len() + 16, "16 hex chars after prefix");
}

#[test]
fn step_2_config_override_when_no_git() {
    // .claudebase/config.json::project_id is honoured when git remote is
    // absent. Step 1 fails (no git) → Step 2 fires.
    let tmp = TempDir::new().expect("tempdir");
    let cdir = tmp.path().join(".claudebase");
    fs::create_dir_all(&cdir).expect("mkdir .claudebase");
    fs::write(
        cdir.join("config.json"),
        r#"{"session_id":"x","name":"y","project_id":"github.com/manual/override"}"#,
    )
    .expect("write config");
    let id = resolve_project_id(tmp.path());
    assert_eq!(id, "github.com/manual/override");
}

#[test]
fn step_2_skipped_when_project_id_field_absent() {
    // Config file exists but project_id field is absent → Step 2 does
    // NOT fire, fall through to Step 3 (path hash).
    let tmp = TempDir::new().expect("tempdir");
    let cdir = tmp.path().join(".claudebase");
    fs::create_dir_all(&cdir).expect("mkdir .claudebase");
    fs::write(
        cdir.join("config.json"),
        r#"{"session_id":"x","name":"y"}"#,
    )
    .expect("write config");
    let id = resolve_project_id(tmp.path());
    assert!(
        id.starts_with("local:"),
        "no project_id field → must fall to Step 3, got {id}"
    );
}

#[test]
fn step_2_skipped_when_project_id_empty_string() {
    let tmp = TempDir::new().expect("tempdir");
    let cdir = tmp.path().join(".claudebase");
    fs::create_dir_all(&cdir).expect("mkdir .claudebase");
    fs::write(
        cdir.join("config.json"),
        r#"{"session_id":"x","name":"y","project_id":""}"#,
    )
    .expect("write config");
    let id = resolve_project_id(tmp.path());
    assert!(id.starts_with("local:"), "empty project_id → Step 3");
}

#[test]
fn malformed_config_json_falls_through_to_step_3() {
    // Defensive: a broken config.json must not panic; resolver falls
    // through to Step 3 just as if the file didn't exist.
    let tmp = TempDir::new().expect("tempdir");
    let cdir = tmp.path().join(".claudebase");
    fs::create_dir_all(&cdir).expect("mkdir .claudebase");
    fs::write(cdir.join("config.json"), "{this is not json").expect("write malformed");
    let id = resolve_project_id(tmp.path());
    assert!(id.starts_with("local:"));
}

// ----------------------------------------------------------------------------
// Integration — real git repo via `git init` + mocked remote URL in .git/config
// ----------------------------------------------------------------------------

#[test]
fn integration_real_git_repo_resolves_to_normalized_remote() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = tmp.path();
    // Initialise a bare git repo so `git -C <cwd> config --get` works.
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["init", "--quiet"])
        .status();
    let init = match init {
        Ok(s) if s.success() => s,
        _ => {
            eprintln!("git not available — skipping integration test");
            return;
        }
    };
    let _ = init;
    // Wire up an origin remote without contacting the network.
    let add = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "remote",
            "add",
            "origin",
            "https://GitHub.com/IntegrationTest/Repo.git",
        ])
        .status()
        .expect("git remote add");
    assert!(add.success(), "git remote add failed");
    let id = resolve_project_id(repo);
    assert_eq!(id, "github.com/integrationtest/repo");
}

#[test]
fn integration_git_repo_ssh_url_normalizes_same_as_https() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = tmp.path();
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["init", "--quiet"])
        .status();
    if !matches!(&init, Ok(s) if s.success()) {
        eprintln!("git not available — skipping integration test");
        return;
    }
    let add = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["remote", "add", "origin", "git@github.com:foo/bar.git"])
        .status()
        .expect("git remote add");
    assert!(add.success());
    let id = resolve_project_id(repo);
    assert_eq!(id, "github.com/foo/bar");
}

#[test]
fn integration_step_1_wins_over_step_2_config() {
    // Both git remote AND .claudebase/config.json::project_id present.
    // Step 1 (git) MUST win — the config override is the FALLBACK when
    // git is unavailable, not an overrider.
    let tmp = TempDir::new().expect("tempdir");
    let repo = tmp.path();
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["init", "--quiet"])
        .status();
    if !matches!(&init, Ok(s) if s.success()) {
        eprintln!("git not available — skipping integration test");
        return;
    }
    let add = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["remote", "add", "origin", "https://github.com/from/git.git"])
        .status()
        .expect("git remote add");
    assert!(add.success());
    let cdir = repo.join(".claudebase");
    fs::create_dir_all(&cdir).expect("mkdir .claudebase");
    fs::write(
        cdir.join("config.json"),
        r#"{"session_id":"x","name":"y","project_id":"from/config"}"#,
    )
    .expect("write config");
    let id = resolve_project_id(repo);
    assert_eq!(id, "github.com/from/git", "Step 1 (git) wins over Step 2 (config)");
}
