//! cli-to-cli-routing Slice 6 — `claudebase agent list-alive` +
//! `agent inspect` CLI subcommand tests.
//!
//! Coverage (QA cases TC-C2C-1.1..1.8 + TC-C2C-AC1 + red-team F-4
//! observability surface):
//!
//!   * Project scope filter: `current` (resolves cwd), `all`, literal
//!     slug. UC-C2C-1, UC-C2C-1-EC3 (legacy NULL-project_id excluded).
//!   * `inspect` returns registry snapshot + undelivered_count + DND
//!     state. Exit 1 on unknown agent.
//!
//! The CLI subcommands spawn `claudebase` binary as a subprocess with
//! HOME pointed at a TempDir so chat.db is fully isolated.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_claudebase")
}

fn run_cli(home: &Path, args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new(bin());
    cmd.args(args);
    cmd.env("HOME", home);
    #[cfg(unix)]
    {
        let runtime = home.join("run");
        std::fs::create_dir_all(&runtime).ok();
        cmd.env("XDG_RUNTIME_DIR", &runtime);
    }
    #[cfg(windows)]
    {
        cmd.env("USERPROFILE", home);
        let localappdata = home.join("AppData\\Local");
        std::fs::create_dir_all(&localappdata).ok();
        cmd.env("LOCALAPPDATA", &localappdata);
    }
    Ok(cmd.output().context("spawn claudebase CLI")?)
}

/// Build a chat.db at the HOME scope with the given agent rows pre-
/// inserted. Each row is inserted alive with the supplied project_id
/// (or NULL if None). Uses the in-process rusqlite (NOT subprocess)
/// so tests don't depend on a `claudebase seed` subcommand that
/// doesn't exist.
fn seed_chat_db(home: &Path, rows: &[(&str, Option<&str>)]) -> Result<()> {
    let path = home.join(".claude").join("knowledge").join("chat.db");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let conn = rusqlite::Connection::open(&path)?;
    claudebase::daemon::chat::ensure_chat_db_schema(&conn)?;
    for (agent_id, pid) in rows {
        conn.execute(
            "INSERT INTO agent_registry \
             (agent_id, agent_name, connection_id, chat_thread_id, \
              permission_relayer, spawned_at, last_pinged_at, state, \
              metadata, project_id, branch, working_dir, \
              feature_description, dnd_until_ts) \
             VALUES (?1, ?1, 'cid', NULL, NULL, 1, 1, 'alive', NULL, \
                     ?2, NULL, NULL, NULL, NULL)",
            rusqlite::params![agent_id, pid],
        )?;
    }
    Ok(())
}

#[test]
fn list_alive_project_all_includes_legacy_null_project_id() {
    let tmp = TempDir::new().expect("tempdir");
    seed_chat_db(
        tmp.path(),
        &[
            ("alice", Some("github.com/foo/bar")),
            ("bob", None),
            ("carol", Some("github.com/other/repo")),
        ],
    )
    .expect("seed");
    let out = run_cli(tmp.path(), &["agent", "list-alive", "--project", "all", "--json"])
        .expect("cli");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"alice\""));
    assert!(stdout.contains("\"bob\""), "bob (NULL project) appears under --all");
    assert!(stdout.contains("\"carol\""));
}

#[test]
fn list_alive_literal_slug_filters_to_one_project() {
    let tmp = TempDir::new().expect("tempdir");
    seed_chat_db(
        tmp.path(),
        &[
            ("alice", Some("github.com/foo/bar")),
            ("bob", None),
            ("carol", Some("github.com/other/repo")),
        ],
    )
    .expect("seed");
    let out = run_cli(
        tmp.path(),
        &[
            "agent",
            "list-alive",
            "--project",
            "github.com/foo/bar",
            "--json",
        ],
    )
    .expect("cli");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"alice\""));
    assert!(!stdout.contains("\"bob\""));
    assert!(!stdout.contains("\"carol\""));
}

#[test]
fn list_alive_literal_slug_excludes_null_project_id_rows() {
    // UC-C2C-1-EC3 — legacy rows with NULL project_id MUST NOT appear
    // when a project filter is applied.
    let tmp = TempDir::new().expect("tempdir");
    seed_chat_db(
        tmp.path(),
        &[("alice", Some("github.com/foo/bar")), ("bob", None)],
    )
    .expect("seed");
    let out = run_cli(
        tmp.path(),
        &[
            "agent",
            "list-alive",
            "--project",
            "github.com/foo/bar",
            "--json",
        ],
    )
    .expect("cli");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("\"bob\""), "NULL project_id excluded by literal slug filter");
}

#[test]
fn list_alive_empty_result_exits_zero_with_friendly_message() {
    let tmp = TempDir::new().expect("tempdir");
    seed_chat_db(tmp.path(), &[]).expect("seed empty");
    let out = run_cli(
        tmp.path(),
        &["agent", "list-alive", "--project", "all"],
    )
    .expect("cli");
    assert!(out.status.success(), "empty list must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no agents alive"));
}

#[test]
fn list_alive_json_flag_emits_pretty_json_array() {
    let tmp = TempDir::new().expect("tempdir");
    seed_chat_db(tmp.path(), &[("alice", Some("p1"))]).expect("seed");
    let out = run_cli(
        tmp.path(),
        &["agent", "list-alive", "--project", "all", "--json"],
    )
    .expect("cli");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json must emit valid JSON");
    let arr = parsed.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["agent_id"], "alice");
}

#[test]
fn inspect_unknown_agent_exits_one() {
    let tmp = TempDir::new().expect("tempdir");
    seed_chat_db(tmp.path(), &[]).expect("seed empty");
    let out = run_cli(tmp.path(), &["agent", "inspect", "ghost"])
        .expect("cli");
    assert!(!out.status.success(), "unknown agent_id must exit 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not found"));
}

#[test]
fn inspect_known_agent_returns_registry_snapshot() {
    let tmp = TempDir::new().expect("tempdir");
    seed_chat_db(tmp.path(), &[("alice", Some("github.com/foo/bar"))]).expect("seed");
    let out = run_cli(tmp.path(), &["agent", "inspect", "alice", "--json"])
        .expect("cli");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(parsed["agent_id"], "alice");
    assert_eq!(parsed["project_id"], "github.com/foo/bar");
    assert_eq!(parsed["state"], "alive");
    assert_eq!(parsed["dnd_state"], "off");
    assert_eq!(parsed["undelivered_count"], 0);
}

#[test]
fn inspect_reports_indefinite_dnd_when_sentinel_set() {
    let tmp = TempDir::new().expect("tempdir");
    seed_chat_db(tmp.path(), &[("alice", Some("p1"))]).expect("seed");
    // Set indefinite DND directly on the DB row.
    let path = tmp
        .path()
        .join(".claude")
        .join("knowledge")
        .join("chat.db");
    let conn = rusqlite::Connection::open(&path).expect("open");
    conn.execute(
        "UPDATE agent_registry SET dnd_until_ts = ?1 WHERE agent_id='alice'",
        rusqlite::params![i64::MAX],
    )
    .expect("set indefinite");
    drop(conn);
    let out = run_cli(tmp.path(), &["agent", "inspect", "alice", "--json"])
        .expect("cli");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(parsed["dnd_state"], "indefinite");
}
