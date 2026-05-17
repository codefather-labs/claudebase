//! TDD integration tests for Slice 1b: MCP `initialize` handshake + fallback
//!
//! Coverage:
//! - TC-1.4: plugin returns valid MCP `initialize` response with capabilities.tools.listChanged=true
//! - TC-1.5: plugin rejects unsupported protocol versions
//! - TC-1.6: plugin completes `initialize` even when daemon is down, with fallback tools
//!
//! These tests spawn both `claudebase daemon serve` and `claudebase plugin serve` as subprocesses,
//! piping JSON-RPC 2.0 MCP frames over STDIO. The plugin is expected to fail initially (missing
//! bridge implementation) — that's the red phase of TDD.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::time::timeout;

/// Per-pid BufReader registry — keeps the BufReader alive across calls
/// so `read_mcp_line` is reentrant. Without this, taking `plugin.stdout`
/// once consumes it and subsequent reads fail.
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
/// The plugin connects to daemon via UDS/named pipe in the given runtime directory.
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

/// Poll for the socket file to appear at the expected path. Times out after the given duration.
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
/// Stores the BufReader in a per-pid registry so subsequent reads work.
async fn read_mcp_line(plugin: &mut Child, timeout_dur: Duration) -> Result<String> {
    let pid = plugin.id();
    // Register stdout on first use.
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

/// Helper to send MCP request and read response line.
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

/// Test: daemon running, plugin returns valid initialize response with listChanged capability.
/// Maps to: TC-1.4 — happy path
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_plugin_initialize_returns_valid_mcp_response() {
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

    // Send initialize request
    let init_params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {
            "name": "test-client",
            "version": "0.1.0"
        }
    });

    let response = send_mcp_request(&mut plugin, "initialize", init_params, 1)
        .await
        .expect("initialize request succeeded");

    // Verify response structure
    assert_eq!(
        response.get("jsonrpc"),
        Some(&json!("2.0")),
        "response should be JSON-RPC 2.0"
    );
    assert_eq!(response.get("id"), Some(&json!(1)), "response id should match");

    let result = response
        .get("result")
        .expect("response should have 'result' field");

    assert_eq!(
        result.get("protocolVersion"),
        Some(&json!("2024-11-05")),
        "protocolVersion should match"
    );

    let server_info = result
        .get("serverInfo")
        .expect("result should have serverInfo");
    assert_eq!(
        server_info.get("name"),
        Some(&json!("claudebase")),
        "serverInfo.name should be 'claudebase'"
    );

    let capabilities = result
        .get("capabilities")
        .expect("result should have capabilities");
    let tools_cap = capabilities
        .get("tools")
        .expect("capabilities should have tools");
    assert_eq!(
        tools_cap.get("listChanged"),
        Some(&json!(true)),
        "capabilities.tools.listChanged MUST be true per FR-ACD-3.7"
    );

    // Send notifications/initialized (no response expected)
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    send_mcp_line(&mut plugin, &notif.to_string())
        .expect("notifications/initialized sent");

    // Close stdin to signal end of stream
    drop(plugin.stdin.take());

    // Wait for plugin to exit gracefully
    let timeout_dur = Duration::from_secs(2);
    tokio::time::sleep(timeout_dur).await;
    let _ = plugin.kill();

    // Kill daemon
    let _ = daemon.kill();
}

/// Test: unsupported protocol version is rejected with JSON-RPC error.
/// Maps to: TC-1.5 — error case
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_plugin_initialize_rejects_unsupported_version() {
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

    // Send initialize with unsupported version
    let init_params = json!({
        "protocolVersion": "9999-99-99",
        "capabilities": {},
        "clientInfo": {
            "name": "test-client",
            "version": "0.1.0"
        }
    });

    let response = send_mcp_request(&mut plugin, "initialize", init_params, 1)
        .await
        .expect("request succeeded");

    // Should have error field, not result
    assert!(
        response.get("error").is_some(),
        "response should contain error for unsupported version"
    );

    let error = response.get("error").unwrap();
    assert_eq!(
        error.get("code"),
        Some(&json!(-32602)),
        "error code should be -32602 (Invalid params)"
    );

    // Message should mention supported versions
    let message = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        message.len() > 0,
        "error message should explain the version mismatch"
    );

    // Kill processes
    let _ = plugin.kill();
    let _ = daemon.kill();
}

/// Test: plugin completes initialize even when daemon is not running (fallback mode).
/// Maps to: TC-1.6 — fallback path
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_plugin_initialize_completes_when_daemon_down() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let runtime_dir = tmpdir.path();

    // Do NOT spawn daemon — test daemon-down fallback

    // Spawn plugin
    let mut plugin = spawn_plugin_with_runtime_dir(runtime_dir)
        .expect("plugin subprocess spawned");

    // Send initialize request
    let init_params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {
            "name": "test-client",
            "version": "0.1.0"
        }
    });

    let response = send_mcp_request(&mut plugin, "initialize", init_params, 1)
        .await
        .expect("initialize succeeded even with daemon down");

    // Should have result (not error)
    assert!(
        response.get("result").is_some(),
        "plugin should complete initialize handshake even when daemon is down"
    );

    // Now send tools/list — should return only the sentinel tool
    let tools_list_response = send_mcp_request(&mut plugin, "tools/list", json!({}), 2)
        .await
        .expect("tools/list request succeeded");

    let result = tools_list_response
        .get("result")
        .expect("tools/list should have result");
    let tools = result
        .get("tools")
        .expect("result should have tools array")
        .as_array()
        .expect("tools should be an array");

    assert_eq!(
        tools.len(),
        1,
        "should have exactly one sentinel tool when daemon is down"
    );

    let sentinel = &tools[0];
    assert_eq!(
        sentinel.get("name"),
        Some(&json!("claudebase_daemon_status")),
        "sentinel tool should be named 'claudebase_daemon_status'"
    );

    // Schema should be empty per FR-ACD-10.1
    let schema = sentinel
        .get("inputSchema")
        .expect("tool should have inputSchema");
    assert_eq!(schema, &json!({}), "sentinel tool schema should be empty");

    // Send tools/call to the sentinel tool
    let call_params = json!({
        "name": "claudebase_daemon_status",
        "arguments": {}
    });

    let call_response = send_mcp_request(&mut plugin, "tools/call", call_params, 3)
        .await
        .expect("tools/call request succeeded");

    let call_result = call_response
        .get("result")
        .expect("tools/call should have result");
    let content = call_result
        .get("content")
        .and_then(|v| v.as_array())
        .expect("content should be an array");

    assert!(!content.is_empty(), "sentinel tool should return content");

    // Message should contain the literal daemon-down text per FR-ACD-10.1
    let message_text = content[0]
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        message_text.contains("claudebase daemon is not running") || message_text.contains("daemon"),
        "daemon-down message should be informative"
    );

    // Clean up
    let _ = plugin.kill();
}
