//! TDD integration tests for Slice 3: Chat CLI Introspection
//!
//! Coverage:
//! - TC-3.9: `claudebase chat list --thread X` lists messages for thread X
//! - TC-3.10: `claudebase chat threads` lists all thread ids
//!
//! These tests pre-populate chat.db with rusqlite, then invoke CLI subcommands
//! directly (NO daemon needed for these tests).

use anyhow::Result;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Prepare chat.db with schema and test data
fn prepare_chat_db(home_dir: &Path) -> Result<()> {
    let chat_db_path = home_dir.join(".claude").join("knowledge").join("chat.db");
    fs::create_dir_all(chat_db_path.parent().unwrap())?;

    use rusqlite::Connection;
    let conn = Connection::open(&chat_db_path)?;

    // Create schema
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chat_threads (
            id TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS chat_messages (
             id TEXT PRIMARY KEY,
             thread_id TEXT NOT NULL,
             from_agent TEXT NOT NULL,
             content TEXT NOT NULL,
             reply_to TEXT,
             created_at INTEGER NOT NULL,
             delivered_at INTEGER
         );
         CREATE INDEX IF NOT EXISTS chat_messages_thread_time_idx
             ON chat_messages(thread_id, created_at);"
    )?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;

    // Insert thread 1
    conn.execute(
        "INSERT INTO chat_threads (id, created_at) VALUES (?, ?)",
        [&"telegram:12345".to_string(), &now.to_string()],
    )?;

    // Insert messages for thread 1
    conn.execute(
        "INSERT INTO chat_messages (id, thread_id, from_agent, content, created_at) VALUES (?, ?, ?, ?, ?)",
        [&"msg-1".to_string(), &"telegram:12345".to_string(), &"agent-1".to_string(), &"first message".to_string(), &now.to_string()],
    )?;

    conn.execute(
        "INSERT INTO chat_messages (id, thread_id, from_agent, content, created_at) VALUES (?, ?, ?, ?, ?)",
        [&"msg-2".to_string(), &"telegram:12345".to_string(), &"agent-2".to_string(), &"second message".to_string(), &(now + 1000).to_string()],
    )?;

    conn.execute(
        "INSERT INTO chat_messages (id, thread_id, from_agent, content, created_at) VALUES (?, ?, ?, ?, ?)",
        [&"msg-3".to_string(), &"telegram:12345".to_string(), &"agent-1".to_string(), &"third message".to_string(), &(now + 2000).to_string()],
    )?;

    // Insert thread 2
    conn.execute(
        "INSERT INTO chat_threads (id, created_at) VALUES (?, ?)",
        [&"telegram:67890".to_string(), &(now + 500).to_string()],
    )?;

    // Insert messages for thread 2
    conn.execute(
        "INSERT INTO chat_messages (id, thread_id, from_agent, content, created_at) VALUES (?, ?, ?, ?, ?)",
        [&"msg-4".to_string(), &"telegram:67890".to_string(), &"agent-3".to_string(), &"thread2 message1".to_string(), &(now + 500).to_string()],
    )?;

    conn.execute(
        "INSERT INTO chat_messages (id, thread_id, from_agent, content, created_at) VALUES (?, ?, ?, ?, ?)",
        [&"msg-5".to_string(), &"telegram:67890".to_string(), &"agent-4".to_string(), &"thread2 message2".to_string(), &(now + 1500).to_string()],
    )?;

    Ok(())
}

/// Test: `claudebase chat list --thread X` lists messages
/// Maps to: TC-3.9
#[tokio::test]
async fn test_chat_list_cli_for_thread() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let home_dir = tmpdir.path();

    // Prepare chat.db
    prepare_chat_db(home_dir).expect("chat.db prepared");

    // Run `claudebase chat list --thread telegram:12345`
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let output = Command::new(bin)
        .args(&["chat", "list", "--thread", "telegram:12345"])
        .env("HOME", home_dir)
        .output()
        .expect("chat list command executed");

    // Verify exit code is 0
    assert_eq!(
        output.status.code(),
        Some(0),
        "chat list should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify stdout contains messages from the thread
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("first message"),
        "output should contain 'first message', got: {}",
        stdout
    );

    assert!(
        stdout.contains("second message"),
        "output should contain 'second message', got: {}",
        stdout
    );

    assert!(
        stdout.contains("third message"),
        "output should contain 'third message', got: {}",
        stdout
    );

    // Verify messages appear in chronological order (first < second < third)
    let first_pos = stdout.find("first message").expect("first message found");
    let second_pos = stdout.find("second message").expect("second message found");
    let third_pos = stdout.find("third message").expect("third message found");

    assert!(
        first_pos < second_pos && second_pos < third_pos,
        "messages should appear in chronological order"
    );
}

/// Test: `claudebase chat threads` lists all thread ids
/// Maps to: TC-3.10
#[tokio::test]
async fn test_chat_threads_cli_lists_all() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let home_dir = tmpdir.path();

    // Prepare chat.db
    prepare_chat_db(home_dir).expect("chat.db prepared");

    // Run `claudebase chat threads`
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let output = Command::new(bin)
        .args(&["chat", "threads"])
        .env("HOME", home_dir)
        .output()
        .expect("chat threads command executed");

    // Verify exit code is 0
    assert_eq!(
        output.status.code(),
        Some(0),
        "chat threads should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify stdout contains both thread ids
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("telegram:12345"),
        "output should contain 'telegram:12345', got: {}",
        stdout
    );

    assert!(
        stdout.contains("telegram:67890"),
        "output should contain 'telegram:67890', got: {}",
        stdout
    );
}

/// Test: `claudebase chat list --thread` with non-existent thread returns empty/error gracefully
/// Edge case coverage
#[tokio::test]
async fn test_chat_list_nonexistent_thread() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let home_dir = tmpdir.path();

    // Prepare chat.db with some threads
    prepare_chat_db(home_dir).expect("chat.db prepared");

    // Run for a thread that doesn't exist
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let output = Command::new(bin)
        .args(&["chat", "list", "--thread", "telegram:nonexistent"])
        .env("HOME", home_dir)
        .output()
        .expect("chat list command executed");

    // Exit code should be 0 (graceful empty result) or consistent error code
    // Implementation choice: likely exit 0 with empty output, or exit 1 with error message
    let exit_code = output.status.code();
    assert!(
        exit_code == Some(0) || exit_code == Some(1),
        "chat list should exit 0 or 1, got: {:?}",
        exit_code
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Either empty output or a clear message about no messages found
    assert!(
        stdout.is_empty() || stdout.to_lowercase().contains("no message") || stdout.to_lowercase().contains("not found"),
        "output should be empty or contain 'no message' for non-existent thread, got: {}",
        stdout
    );
}

/// Test: `claudebase chat threads` with empty database
/// Edge case coverage
#[tokio::test]
async fn test_chat_threads_empty_database() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let home_dir = tmpdir.path();

    // Create empty chat.db with schema only (no threads)
    let chat_db_path = home_dir.join(".claude").join("knowledge").join("chat.db");
    fs::create_dir_all(chat_db_path.parent().unwrap()).expect("dir created");

    use rusqlite::Connection;
    let conn = Connection::open(&chat_db_path).expect("chat.db created");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chat_threads (
            id TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS chat_messages (
             id TEXT PRIMARY KEY,
             thread_id TEXT NOT NULL,
             from_agent TEXT NOT NULL,
             content TEXT NOT NULL,
             reply_to TEXT,
             created_at INTEGER NOT NULL,
             delivered_at INTEGER
         );"
    ).expect("schema created");
    drop(conn);

    // Run `claudebase chat threads`
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let output = Command::new(bin)
        .args(&["chat", "threads"])
        .env("HOME", home_dir)
        .output()
        .expect("chat threads command executed");

    // Should exit 0 with empty or "no threads" message
    let exit_code = output.status.code();
    assert_eq!(
        exit_code,
        Some(0),
        "chat threads should exit 0 for empty database"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output should be empty or indicate no threads
    assert!(
        stdout.is_empty() || stdout.to_lowercase().contains("thread"),
        "output should be empty or mention threads, got: {}",
        stdout
    );
}
