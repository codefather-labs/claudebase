//! TDD integration tests for Slice 3: Chat Broadcast to Multiple Subscribers
//!
//! Coverage:
//! - TC-3.3: broadcast to 2 subscribers, both receive notifications
//!
//! This test spawns two separate plugin processes, both subscribed to the same thread,
//! posts a message from plugin A, and verifies both plugin A and B receive the notification.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::time::timeout;

/// Per-pid BufReader registry
fn stdout_registry() -> &'static Mutex<HashMap<u32, BufReader<ChildStdout>>> {
    static REG: OnceLock<Mutex<HashMap<u32, BufReader<ChildStdout>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Spawn daemon with HOME and XDG_RUNTIME_DIR isolation
fn spawn_daemon_with_home(tempdir: &Path) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "serve"]);
    cmd.env("HOME", tempdir);

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

    let child = cmd.spawn()?;
    Ok(child)
}

/// Spawn plugin with HOME and XDG_RUNTIME_DIR isolation
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

/// Poll for socket existence
async fn wait_for_socket(socket_path: &Path, max_wait: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        if socket_path.exists() {
            return Ok(());
        }
        if start.elapsed() > max_wait {
            bail!("socket not found");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Send MCP line to plugin stdin
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

/// Test: two plugins both receive broadcast notification
/// Maps to: TC-3.3
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_chat_broadcast_to_two_subscribers() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let home_dir = tmpdir.path();
    let socket_path = home_dir.join("run").join("claudebase").join("daemon.sock");

    // Spawn daemon
    let mut _daemon = spawn_daemon_with_home(home_dir).expect("daemon spawned");
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Spawn plugin A
    let mut plugin_a = spawn_plugin_with_home(home_dir).expect("plugin A spawned");

    // Initialize plugin A
    let init_params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": { "name": "test-a", "version": "0.1.0" }
    });
    let _init_a = send_mcp_request(&mut plugin_a, "initialize", init_params.clone(), 1)
        .await
        .expect("plugin A initialize");

    // Subscribe plugin A to thread
    let _sub_a = send_mcp_request(&mut plugin_a, "tools/call", json!({
        "name": "chat_subscribe",
        "arguments": json!({ "thread": "telegram:99999" })
    }), 2)
        .await
        .expect("plugin A subscribe");

    // Spawn plugin B
    let mut plugin_b = spawn_plugin_with_home(home_dir).expect("plugin B spawned");

    // Initialize plugin B
    let _init_b = send_mcp_request(&mut plugin_b, "initialize", init_params, 3)
        .await
        .expect("plugin B initialize");

    // Subscribe plugin B to same thread
    let _sub_b = send_mcp_request(&mut plugin_b, "tools/call", json!({
        "name": "chat_subscribe",
        "arguments": json!({ "thread": "telegram:99999" })
    }), 4)
        .await
        .expect("plugin B subscribe");

    // Small delay to ensure both subscriptions are registered
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Post message from plugin A
    let post_resp = send_mcp_request(&mut plugin_a, "tools/call", json!({
        "name": "chat_post",
        "arguments": json!({
            "thread": "telegram:99999",
            "content": "broadcast-test-message",
            "from": "mira"
        })
    }), 5)
        .await
        .expect("post request");

    assert!(
        post_resp.get("error").is_none(),
        "post should not error"
    );

    // Plugin A should receive notification
    let notif_a_line = read_mcp_line(&mut plugin_a, Duration::from_secs(2))
        .await
        .expect("plugin A notification");
    let notif_a: Value = serde_json::from_str(&notif_a_line)
        .expect("plugin A notification JSON");

    assert_eq!(
        notif_a.get("method"),
        Some(&json!("notifications/claude/channel")),
        "plugin A should receive notifications/claude/channel"
    );

    let params_a = notif_a.get("params").expect("plugin A notification params");
    assert_eq!(
        params_a.get("content"),
        Some(&json!("broadcast-test-message")),
        "plugin A notification content should match"
    );

    // Plugin B should also receive notification
    let notif_b_line = read_mcp_line(&mut plugin_b, Duration::from_secs(2))
        .await
        .expect("plugin B notification");
    let notif_b: Value = serde_json::from_str(&notif_b_line)
        .expect("plugin B notification JSON");

    assert_eq!(
        notif_b.get("method"),
        Some(&json!("notifications/claude/channel")),
        "plugin B should receive notifications/claude/channel"
    );

    let params_b = notif_b.get("params").expect("plugin B notification params");
    assert_eq!(
        params_b.get("content"),
        Some(&json!("broadcast-test-message")),
        "plugin B notification content should match"
    );

    // NOTE: TC-3.3 also checks subscriber_count in daemon status, but that endpoint
    // is implemented in Slice 2 (daemon status subcommand). This test focuses on
    // the broadcast itself and defers the status assertion to TC-2.x in Slice 2.
    // See Inbound validation note in the task description.

    // Kill processes
    let _ = std::process::Command::new("pkill")
        .arg("-f")
        .arg("daemon serve")
        .output();
    let _ = std::process::Command::new("pkill")
        .arg("-f")
        .arg("plugin serve")
        .output();
}
