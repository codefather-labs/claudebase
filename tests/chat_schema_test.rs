//! TDD integration tests for Slice 3: Chat Backend Schema Migration
//!
//! Coverage:
//! - TC-3.1-schema: Schema v5 migration creates chat_threads, chat_messages, daemon_state
//!
//! This test spawns the daemon with a temporary HOME directory to isolate
//! chat.db at ~/.claude/knowledge/chat.db, lets it initialize the schema,
//! then stops the daemon and inspects the database directly.

use anyhow::{bail, Result};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Spawn the claudebase daemon with HOME scoped to the given temp directory.
/// The daemon will create chat.db at $HOME/.claude/knowledge/chat.db.
fn spawn_daemon_with_home(tempdir: &Path) -> Result<std::process::Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "serve"]);
    cmd.env("HOME", tempdir);

    #[cfg(unix)]
    {
        // Also set XDG_RUNTIME_DIR to tempdir so socket is isolated
        let runtime_dir = tempdir.join("run");
        fs::create_dir_all(&runtime_dir)?;
        cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    }

    #[cfg(windows)]
    {
        cmd.env("USERPROFILE", tempdir);
        let localappdata = tempdir.join("AppData\\Local");
        fs::create_dir_all(&localappdata)?;
        cmd.env("LOCALAPPDATA", &localappdata);
    }

    let child = cmd.spawn()?;
    Ok(child)
}

/// Poll for the socket file to appear. Times out after the given duration.
async fn wait_for_socket(socket_path: &Path, max_wait: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        if socket_path.exists() {
            return Ok(());
        }
        if start.elapsed() > max_wait {
            bail!("socket file not found after {:?}: {:?}", max_wait, socket_path);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Test: daemon startup creates chat.db with schema v5 tables
/// Maps to: TC-3.1-schema
#[tokio::test(flavor = "multi_thread")]
async fn test_chat_schema_v5_migration_on_daemon_startup() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let home_dir = tmpdir.path();

    // Determine socket path based on platform
    #[cfg(unix)]
    let socket_path = home_dir.join("run").join("claudebase").join("daemon.sock");
    #[cfg(windows)]
    let socket_path = home_dir.join("AppData\\Local\\claudebase\\daemon.sock");

    // Spawn daemon with home isolation
    let mut daemon = spawn_daemon_with_home(home_dir)
        .expect("daemon subprocess spawned");

    // Wait for socket (gives daemon time to initialize)
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    // Give daemon a moment to create chat.db
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Kill daemon gracefully via SIGTERM
    let daemon_pid = daemon.id();
    #[cfg(unix)]
    let _ = std::process::Command::new("kill")
        .arg(daemon_pid.to_string())
        .output();
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(&["/PID", &daemon_pid.to_string(), "/T", "/F"])
        .output();

    let _ = daemon.wait();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Now inspect chat.db at ~/.claude/knowledge/chat.db
    let chat_db_path = home_dir.join(".claude").join("knowledge").join("chat.db");

    assert!(
        chat_db_path.exists(),
        "chat.db should exist at {:?}",
        chat_db_path
    );

    // Open database with rusqlite and verify tables exist
    use rusqlite::Connection;

    let conn = Connection::open(&chat_db_path)
        .expect("chat.db opened successfully");

    // Verify chat_threads table exists
    let result: Result<i32, _> = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='chat_threads'",
        [],
        |row| row.get(0),
    );
    assert_eq!(
        result.expect("query succeeded"),
        1,
        "chat_threads table should exist"
    );

    // Verify chat_messages table exists
    let result: Result<i32, _> = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='chat_messages'",
        [],
        |row| row.get(0),
    );
    assert_eq!(
        result.expect("query succeeded"),
        1,
        "chat_messages table should exist"
    );

    // Verify chat_messages_thread_time_idx index exists
    let result: Result<i32, _> = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='chat_messages_thread_time_idx'",
        [],
        |row| row.get(0),
    );
    assert_eq!(
        result.expect("query succeeded"),
        1,
        "chat_messages_thread_time_idx index should exist"
    );

    // Verify daemon_state table exists
    let result: Result<i32, _> = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='daemon_state'",
        [],
        |row| row.get(0),
    );
    assert_eq!(
        result.expect("query succeeded"),
        1,
        "daemon_state table should exist"
    );

    // Verify daemon_state bootstrap row exists (telegram.last_update_id = '0')
    let result: Result<String, _> = conn.query_row(
        "SELECT value FROM daemon_state WHERE key='telegram.last_update_id'",
        [],
        |row| row.get(0),
    );
    match result {
        Ok(value) => {
            assert_eq!(
                value, "0",
                "telegram.last_update_id should be '0' initially, got '{}'",
                value
            );
        }
        Err(e) => {
            panic!("daemon_state should contain telegram.last_update_id row: {}", e);
        }
    }
}
