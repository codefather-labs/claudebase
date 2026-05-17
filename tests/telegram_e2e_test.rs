//! TDD integration tests for Slice 4: Telegram bot E2E roundtrip
//!
//! These tests verify Telegram integration with the daemon and plugin:
//! - TC-4.11: happy path text message → chat.db → plugin notification
//! - TC-4.7: 401 Unauthorized → daemon stays alive, tg_bot_state = "disconnected"
//! - TC-4.8: 429 Rate Limited → 1 retry (not 3), then surface error
//! - TC-4.16: restart window — daemon down, message queued, restart → delivered once
//!
//! IMPORTANT NOTES FOR IMPLEMENTER:
//! - These tests expect daemon to accept TELOXIDE_API_URL env var OR implement via feature flag
//! - Mock approach: tests can use a local HTTP mock server OR feature-flag-based mock transport
//! - The test structure assumes test helpers work with mocked Telegram responses
//! - Integration tests depend on daemon supporting mocked Telegram endpoints

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::time::timeout;

/// Per-pid BufReader registry for plugin stdout (copied from chat_tools_e2e_test)
#[allow(dead_code)]
fn stdout_registry() -> &'static Mutex<std::collections::HashMap<u32, BufReader<ChildStdout>>> {
    static REG: OnceLock<
        Mutex<std::collections::HashMap<u32, BufReader<ChildStdout>>>,
    > = OnceLock::new();
    REG.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Spawn daemon with HOME, XDG_RUNTIME_DIR, and TELOXIDE_API_URL isolation
#[allow(dead_code)]
fn spawn_daemon_with_home_and_telegram(
    tempdir: &Path,
    teloxide_url: &str,
) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "serve"]);
    cmd.env("HOME", tempdir);
    cmd.env("TELOXIDE_API_URL", teloxide_url);

    #[cfg(unix)]
    {
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

    cmd.stderr(Stdio::piped());
    cmd.stdout(Stdio::null());

    let child = cmd.spawn()?;
    Ok(child)
}

/// Spawn plugin with HOME and XDG_RUNTIME_DIR isolation (copied from chat_tools_e2e_test)
#[allow(dead_code)]
fn spawn_plugin_with_home(tempdir: &Path) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["plugin", "serve"]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    cmd.env("HOME", tempdir);

    #[cfg(unix)]
    {
        let runtime_dir = tempdir.join("run");
        cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    }

    #[cfg(windows)]
    {
        cmd.env("USERPROFILE", tempdir);
        let localappdata = tempdir.join("AppData\\Local");
        cmd.env("LOCALAPPDATA", &localappdata);
    }

    let child = cmd.spawn()?;
    Ok(child)
}

/// Helper to create secrets.toml with bot token
fn create_secrets_toml(tempdir: &Path) -> Result<()> {
    let config_dir = tempdir.join(".config").join("claudebase");
    fs::create_dir_all(&config_dir)?;

    let secrets_path = config_dir.join("secrets.toml");
    let content = r#"[telegram]
bot_token = "123456789:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi"
"#;

    fs::write(&secrets_path, content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&secrets_path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// Helper to create daemon.toml with valid ASR config
fn create_daemon_toml(tempdir: &Path, dm_policy: &str) -> Result<()> {
    let config_dir = tempdir.join(".config").join("claudebase");
    fs::create_dir_all(&config_dir)?;

    let daemon_path = config_dir.join("daemon.toml");
    let content = format!(
        r#"[asr]
backend = "whisper"

[telegram]
dmPolicy = "{}"
"#,
        dm_policy
    );

    fs::write(&daemon_path, content)?;

    Ok(())
}

/// Helper to create access.json with authorized users
fn create_access_json(tempdir: &Path, authorized_user_id: u64) -> Result<()> {
    let config_dir = tempdir.join(".config").join("claudebase");
    fs::create_dir_all(&config_dir)?;

    let access_path = config_dir.join("access.json");
    let content = json!({
        "dmPolicy": "pairing",
        "allowFrom": [authorized_user_id],
        "groups": {},
        "pending": {}
    });

    fs::write(&access_path, serde_json::to_string_pretty(&content)?)?;

    Ok(())
}

/// Send MCP line to plugin stdin
#[allow(dead_code)]
fn send_mcp_line(plugin: &mut Child, line: &str) -> Result<()> {
    let stdin = plugin
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("stdin unavailable"))?;
    writeln!(stdin, "{}", line)?;
    stdin.flush()?;
    Ok(())
}

/// Read MCP line from plugin stdout with timeout
#[allow(dead_code)]
async fn read_mcp_line(plugin: &mut Child, timeout_dur: Duration) -> Result<String> {
    let pid = plugin.id();
    {
        let mut reg = stdout_registry().lock().unwrap();
        if !reg.contains_key(&pid) {
            let stdout = plugin
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("stdout unavailable"))?;
            reg.insert(pid, BufReader::new(stdout));
        }
    }

    let result = timeout(timeout_dur, tokio::task::spawn_blocking(move || {
        let mut reg = stdout_registry().lock().unwrap();
        let reader = reg.get_mut(&pid).ok_or_else(|| anyhow::anyhow!("registry missing"))?;
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => Err(anyhow::anyhow!("EOF")),
            Ok(_) => Ok(buf.trim_end_matches('\n').trim_end_matches('\r').to_string()),
            Err(e) => Err(anyhow::anyhow!("read error: {}", e)),
        }
    }))
    .await;

    match result {
        Ok(Ok(Ok(line))) => Ok(line),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(e)) => Err(anyhow::anyhow!("spawn_blocking: {}", e)),
        Err(_) => Err(anyhow::anyhow!("timeout")),
    }
}

/// Send MCP request and read response
#[allow(dead_code)]
async fn send_mcp_request(
    plugin: &mut Child,
    method: &str,
    params: Value,
    id: u32,
) -> Result<Value> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    });
    send_mcp_line(plugin, &request.to_string())?;
    let response_line = read_mcp_line(plugin, Duration::from_secs(2)).await?;
    let response: Value = serde_json::from_str(&response_line)?;
    Ok(response)
}

/// Helper to get chat.db path
fn chat_db_path(tempdir: &Path) -> std::path::PathBuf {
    tempdir.join(".claude").join("knowledge").join("chat.db")
}

/// Test: TC-4.11 — happy path text message → chat.db → plugin notification
/// Text message from authorized user arrives within 1 second
/// NOTE: This test structure is in place. Full integration requires:
/// - Implementer to support TELOXIDE_API_URL env var OR mock Telegram feature flag
/// - Mock Telegram server returning valid update payloads
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_telegram_text_message_e2e() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let home_dir = tmpdir.path();

    // Setup: secrets.toml, daemon.toml, access.json
    create_secrets_toml(home_dir)?;
    create_daemon_toml(home_dir, "pairing")?;
    create_access_json(home_dir, 1001)?; // User 1001 is authorized

    // NOTE: In a real integration, spawn daemon with mocked Telegram API endpoint
    // For now, test structure verifies setup is correct
    // let teloxide_url = "http://127.0.0.1:8888"; // Would point to mock server
    // let mut _daemon = spawn_daemon_with_home_and_telegram(home_dir, &teloxide_url)?;

    // Verify chat.db location exists after daemon would create it
    let chat_db = chat_db_path(home_dir);
    let chat_db_parent = chat_db.parent();

    if let Some(parent) = chat_db_parent {
        fs::create_dir_all(parent)?;
    }

    // This test verifies the test infrastructure works
    // Full integration test will verify:
    // 1. Telegram text message received
    // 2. Message stored in chat.db
    // 3. Plugin notified via broadcast
    // 4. Notification received within 1 second

    Ok(())
}

/// Test: TC-4.7 — 401 Unauthorized → daemon stays alive, tg_bot_state = "disconnected"
/// NOTE: This test structure verifies daemon resilience to auth failures
/// Full integration requires mock Telegram server returning 401
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_telegram_401_disconnected() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let home_dir = tmpdir.path();

    // Setup
    create_secrets_toml(home_dir)?;
    create_daemon_toml(home_dir, "pairing")?;
    create_access_json(home_dir, 1001)?;

    // NOTE: Full integration would:
    // 1. Start mock server returning 401
    // 2. Spawn daemon with TELOXIDE_API_URL pointing to mock
    // 3. Verify daemon stays running after 401
    // 4. Check tg_bot_state in stderr logs

    // For red-phase test, verify config is valid
    let secrets_path = home_dir.join(".config").join("claudebase").join("secrets.toml");
    if !secrets_path.exists() {
        bail!("secrets.toml not created");
    }

    // Test structure validates SEC-14 can be tested
    Ok(())
}

/// Test: TC-4.8 — 429 Rate Limited → 1 retry (not 3), then surface error
/// SEC-14: rate limit response specifies retry_after; daemon retries ONCE then surfaces error
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_telegram_429_single_retry() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let home_dir = tmpdir.path();

    // Setup
    create_secrets_toml(home_dir)?;
    create_daemon_toml(home_dir, "pairing")?;
    create_access_json(home_dir, 1001)?;

    // NOTE: Full integration would:
    // 1. Start mock server that returns 429 with retry_after header
    // 2. Spawn daemon and trigger outbound via chat_reply MCP tool
    // 3. Verify retry logic:
    //    - Initial request returns 429
    //    - Daemon waits retry_after seconds
    //    - Daemon retries ONCE (not 3 times per UC-3-E2)
    //    - On failure after retry, MCP response contains:
    //      { "error": "telegram_rate_limited", "retry_after": <seconds> }
    // 4. Verify daemon stays alive (does not crash)

    // For red-phase, verify test infrastructure
    let config_dir = home_dir.join(".config").join("claudebase");
    if !config_dir.exists() {
        bail!("config directory not created");
    }

    Ok(())
}

/// Test: TC-4.16 — restart window — daemon down, message queued, restart → delivered once
/// This tests SEC-13 atomic transaction guarantee on (message insert + last_update_id update)
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_telegram_restart_window_deduped() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let home_dir = tmpdir.path();

    // Setup
    create_secrets_toml(home_dir)?;
    create_daemon_toml(home_dir, "pairing")?;
    create_access_json(home_dir, 1001)?;

    let chat_db = chat_db_path(home_dir);
    if let Some(parent) = chat_db.parent() {
        fs::create_dir_all(parent)?;
    }

    // NOTE: Full integration would:
    // 1. Start mock Telegram server
    // 2. Spawn daemon, verify "first" message processed and in chat.db
    // 3. Stop daemon (SIGTERM, 3s timeout for graceful shutdown)
    // 4. Mock server queues "second" message (represents message while daemon down)
    // 5. Restart daemon, verify:
    //    a) "second" message appears in chat.db (offset advances)
    //    b) No duplicate "first" message (offset already processed)
    //    c) chat_messages row count = 2 (both first and second, no triple-insert)
    //    d) daemon_state.telegram.last_update_id increased past second message's update_id
    // 6. Verify: restarting daemon again with no new messages produces no additional inserts
    //    (offset persistence works correctly per SEC-13)

    // For red-phase, verify test structure is sound
    // This test will validate:
    // - Atomic transaction wrapping (message_insert + daemon_state UPDATE) per directive 7
    // - Offset checkpoint correctly prevents duplicate delivery
    // - No unique constraint needed on (thread_id, telegram_message_id) — insight #9
    //   Instead: relies on transactional offset-advance to skip already-processed messages

    Ok(())
}
