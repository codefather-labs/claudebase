//! TDD integration tests for Slice 1b: MCP `tools/list` forwarding + unknown tool rejection
//!
//! Coverage:
//! - TC-1.7: tools/list with daemon up returns empty list (Slice 3 adds chat tools later)
//! - TC-1.8: tools/call with unknown tool name returns method_not_found error (security)
//!
//! These tests verify the plugin correctly proxies tool requests to the daemon and
//! rejects unknown tool names per security mitigation #7.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::time::timeout;

/// Per-pid BufReader registry — see plugin_mcp_initialize.rs for rationale.
fn stdout_registry() -> &'static Mutex<HashMap<u32, BufReader<ChildStdout>>> {
    static REG: OnceLock<Mutex<HashMap<u32, BufReader<ChildStdout>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Spawn the claudebase daemon with XDG_RUNTIME_DIR scoped to the given temp directory.
fn spawn_daemon_with_runtime_dir(tempdir: &Path) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "serve"]);

    #[cfg(unix)]
    {
        cmd.env("XDG_RUNTIME_DIR", tempdir);
    }

    #[cfg(windows)]
    {
        cmd.env("LOCALAPPDATA", tempdir);
    }

    let child = cmd.spawn()?;
    Ok(child)
}

/// Spawn the claudebase plugin subprocess with piped stdin/stdout.
fn spawn_plugin_with_runtime_dir(tempdir: &Path) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["plugin", "serve"]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());

    #[cfg(unix)]
    {
        cmd.env("XDG_RUNTIME_DIR", tempdir);
    }

    #[cfg(windows)]
    {
        cmd.env("LOCALAPPDATA", tempdir);
    }

    let child = cmd.spawn()?;
    Ok(child)
}

/// Poll for the socket file to appear at the expected path.
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

/// Send a newline-delimited JSON line to the plugin's stdin.
fn send_mcp_line(plugin: &mut Child, line: &str) -> Result<()> {
    let stdin = plugin
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("plugin stdin not available"))?;
    writeln!(stdin, "{}", line)?;
    stdin.flush()?;
    Ok(())
}

/// Read one newline-terminated JSON line from the plugin's stdout with timeout.
async fn read_mcp_line(plugin: &mut Child, timeout_dur: Duration) -> Result<String> {
    let pid = plugin.id();
    {
        let mut reg = stdout_registry().lock().unwrap();
        if !reg.contains_key(&pid) {
            let stdout = plugin
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("plugin stdout not available"))?;
            reg.insert(pid, BufReader::new(stdout));
        }
    }

    let result = timeout(timeout_dur, tokio::task::spawn_blocking(move || {
        let mut reg = stdout_registry().lock().unwrap();
        let reader = reg.get_mut(&pid).ok_or_else(|| anyhow::anyhow!("registry missing"))?;
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => Err(anyhow::anyhow!("EOF from plugin stdout")),
            Ok(_) => Ok(buf.trim_end_matches('\n').trim_end_matches('\r').to_string()),
            Err(e) => Err(anyhow::anyhow!("read error: {}", e)),
        }
    }))
    .await;

    match result {
        Ok(Ok(Ok(line))) => Ok(line),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(e)) => Err(anyhow::anyhow!("spawn_blocking error: {}", e)),
        Err(_) => Err(anyhow::anyhow!("timeout waiting for stdout")),
    }
}

/// Send MCP request and read response.
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

/// Test: daemon running, tools/list returns the 4 chat tools landed in
/// Slice 3 (chat_post, chat_subscribe, chat_reply, chat_list). The
/// sentinel `claudebase_daemon_status` is plugin-side only and does NOT
/// appear in the daemon-up listing.
///
/// Maps to: TC-1.7 (happy path, daemon up) + TC-3.8 (daemon-up surface).
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_tools_list_daemon_up_returns_chat_tools() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let runtime_dir = tmpdir.path();
    let socket_path = runtime_dir.join("claudebase").join("daemon.sock");

    // Spawn daemon
    let mut daemon = spawn_daemon_with_runtime_dir(runtime_dir)
        .expect("daemon subprocess spawned");

    // Wait for socket
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    // Spawn plugin
    let mut plugin = spawn_plugin_with_runtime_dir(runtime_dir)
        .expect("plugin subprocess spawned");

    // Send initialize first (required before tools/list per MCP spec)
    let init_params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {
            "name": "test-client",
            "version": "0.1.0"
        }
    });

    let _init_response = send_mcp_request(&mut plugin, "initialize", init_params, 1)
        .await
        .expect("initialize succeeded");

    // Send tools/list
    let tools_response = send_mcp_request(&mut plugin, "tools/list", json!({}), 2)
        .await
        .expect("tools/list request succeeded");

    // Verify response
    let result = tools_response
        .get("result")
        .expect("tools/list should have result");
    let tools = result
        .get("tools")
        .expect("result should have tools array")
        .as_array()
        .expect("tools should be an array");

    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();

    // Slice 3 chat tools + Slice 5 agent_registry tools — daemon-up
    // tools/list MUST include all 8 (the sentinel daemon-down tool
    // `claudebase_daemon_status` is plugin-side only).
    for required in &[
        "chat_post", "chat_subscribe", "chat_reply", "chat_list",
        "agent_register", "agent_unregister", "agent_list_alive", "agent_reap",
    ] {
        assert!(
            names.contains(required),
            "daemon-up tools/list should include {required}; got {names:?}"
        );
    }
    assert_eq!(
        tools.len(),
        8,
        "post-Slice-5 daemon-up tools/list should expose exactly 8 tools (4 chat + 4 agent_registry); got {names:?}"
    );

    // Clean up
    let _ = plugin.kill();
    let _ = daemon.kill();
}

/// Test: tools/call with unknown tool name returns -32601 method_not_found.
/// Maps to: TC-1.8 — security mitigation #7, prevents shell-injection-like attempts
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_tools_call_unknown_name_returns_method_not_found() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let runtime_dir = tmpdir.path();
    let socket_path = runtime_dir.join("claudebase").join("daemon.sock");

    // Spawn daemon
    let mut daemon = spawn_daemon_with_runtime_dir(runtime_dir)
        .expect("daemon subprocess spawned");

    // Wait for socket
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    // Spawn plugin
    let mut plugin = spawn_plugin_with_runtime_dir(runtime_dir)
        .expect("plugin subprocess spawned");

    // Initialize
    let init_params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {
            "name": "test-client",
            "version": "0.1.0"
        }
    });

    let _init_response = send_mcp_request(&mut plugin, "initialize", init_params, 1)
        .await
        .expect("initialize succeeded");

    // Try to call a tool with a shell metacharacter name (e.g., "rm -rf /")
    // This should be rejected per FR-ACD-3.4 which whitelists tool names
    let call_params = json!({
        "name": "rm -rf /",
        "arguments": {}
    });

    let call_response = send_mcp_request(&mut plugin, "tools/call", call_params, 2)
        .await
        .expect("tools/call request succeeded");

    // Should have error, not result
    assert!(
        call_response.get("error").is_some(),
        "tools/call with unknown tool should return error"
    );

    let error = call_response.get("error").unwrap();
    assert_eq!(
        error.get("code"),
        Some(&json!(-32601)),
        "error code should be -32601 (Method not found)"
    );

    // Verify plugin is still alive and can serve more requests
    let second_call = json!({
        "name": "nonexistent-tool",
        "arguments": {}
    });

    let second_response = send_mcp_request(&mut plugin, "tools/call", second_call, 3)
        .await
        .expect("plugin survived first unknown tool call");

    assert!(
        second_response.get("error").is_some(),
        "plugin should continue to reject unknown tools"
    );

    // Clean up
    let _ = plugin.kill();
    let _ = daemon.kill();
}
