//! TDD integration test for Slice 1b: MCP parse error response (JSON-RPC 2.0 compliant)
//!
//! Coverage:
//! - TC-security-mitigation-#3: parse error returns spec-compliant JSON-RPC response,
//!   no serde error string leakage, plugin survives and accepts subsequent valid frames
//!
//! Tests that the plugin correctly handles malformed JSON per MCP spec: when invalid JSON
//! is sent, the response MUST be JSON-RPC 2.0 compliant with code -32700 and a generic
//! "Parse error" message, NOT a Rust serde panic or error details.

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

/// Send a raw line to the plugin's stdin (may not be valid JSON).
fn send_raw_line(plugin: &mut Child, line: &str) -> Result<()> {
    let stdin = plugin
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("plugin stdin not available"))?;
    writeln!(stdin, "{}", line)?;
    stdin.flush()?;
    Ok(())
}

/// Read one newline-terminated line from the plugin's stdout with timeout.
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

/// Send valid MCP request and read response.
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
    send_raw_line(plugin, &request.to_string())?;
    let response_line = read_mcp_line(plugin, Duration::from_secs(2)).await?;
    let response: Value = serde_json::from_str(&response_line)?;
    Ok(response)
}

/// Test: parse error returns JSON-RPC 2.0 compliant error response, no serde details leak.
/// Maps to: TC-security-mitigation-#3
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_plugin_parse_error_returns_spec_compliant_response() {
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

    // Initialize first
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

    // Send invalid JSON to trigger parse error
    send_raw_line(&mut plugin, "{this is not json}")
        .expect("invalid JSON sent");

    // Read error response
    let error_line = read_mcp_line(&mut plugin, Duration::from_secs(2))
        .await
        .expect("plugin returned error response");

    // Parse the error response
    let error_response: Value = serde_json::from_str(&error_line)
        .expect("error response should be valid JSON");

    // Verify JSON-RPC 2.0 compliance
    assert_eq!(
        error_response.get("jsonrpc"),
        Some(&json!("2.0")),
        "error response must have jsonrpc: 2.0"
    );

    assert_eq!(
        error_response.get("id"),
        Some(&json!(null)),
        "parse error should have id: null per JSON-RPC 2.0 spec"
    );

    // Verify error structure
    let error_obj = error_response
        .get("error")
        .expect("error response must have error field");

    assert_eq!(
        error_obj.get("code"),
        Some(&json!(-32700)),
        "parse error code must be -32700"
    );

    let message = error_obj
        .get("message")
        .and_then(|v| v.as_str())
        .expect("error must have message");

    assert_eq!(
        message, "Parse error",
        "error message must be exactly 'Parse error' per JSON-RPC 2.0"
    );

    // Verify no serde error details leaked
    let full_response_str = error_line.to_lowercase();
    assert!(
        !full_response_str.contains("serde"),
        "error response must NOT contain 'serde' (no error details should leak)"
    );
    assert!(
        !full_response_str.contains("line "),
        "error response must NOT contain line numbers or internal details"
    );
    assert!(
        !full_response_str.contains("expected"),
        "error response must NOT contain detailed parse failure info"
    );

    // Verify plugin is still alive and can handle subsequent valid requests
    let second_init = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {
            "name": "test-client-2",
            "version": "0.1.0"
        }
    });

    let recovery_response = send_mcp_request(&mut plugin, "initialize", second_init, 2)
        .await
        .expect("plugin recovered from parse error and accepted valid request");

    assert!(
        recovery_response.get("result").is_some(),
        "plugin should successfully process valid request after parse error"
    );

    // Clean up
    let _ = plugin.kill();
    let _ = daemon.kill();
}
