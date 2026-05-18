//! Slice 5 — agent_registry MCP integration test.
//!
//! Exercises the full daemon→plugin path for `agent_register` +
//! `agent_list_alive` + `agent_unregister` + `agent_reap` to confirm the
//! wire-shape contract (especially the `reaped_count` field name per
//! TC-5.4 jq path).
//!
//! The DB-layer state machine + uniqueness semantics + CHECK constraint
//! are covered by the 14 inline unit tests in
//! `src/daemon/agent_registry.rs::tests`. This integration test layer
//! validates that the MCP dispatcher correctly wires those primitives
//! and that the tool_text_response envelope round-trips through the
//! plugin bridge's UDS reader task.

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

fn stdout_registry() -> &'static Mutex<HashMap<u32, BufReader<ChildStdout>>> {
    static REG: OnceLock<Mutex<HashMap<u32, BufReader<ChildStdout>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn spawn_daemon_with_home(tempdir: &Path) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "serve"]);
    cmd.env("HOME", tempdir);
    let runtime_dir = tempdir.join("run");
    fs::create_dir_all(&runtime_dir)?;
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    Ok(cmd.spawn()?)
}

fn spawn_plugin_with_home(tempdir: &Path) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["plugin", "serve"]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    cmd.env("HOME", tempdir);
    let runtime_dir = tempdir.join("run");
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    Ok(cmd.spawn()?)
}

async fn wait_for_socket(socket_path: &Path, max_wait: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        if socket_path.exists() {
            return Ok(());
        }
        if start.elapsed() > max_wait {
            bail!("socket not found: {:?}", socket_path);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn send_mcp_line(plugin: &mut Child, line: &str) -> Result<()> {
    let stdin = plugin
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("stdin unavailable"))?;
    writeln!(stdin, "{}", line)?;
    stdin.flush()?;
    Ok(())
}

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
    let result = timeout(
        timeout_dur,
        tokio::task::spawn_blocking(move || {
            let mut reg = stdout_registry().lock().unwrap();
            let reader = reg
                .get_mut(&pid)
                .ok_or_else(|| anyhow::anyhow!("registry missing"))?;
            let mut buf = String::new();
            match reader.read_line(&mut buf) {
                Ok(0) => Err(anyhow::anyhow!("EOF")),
                Ok(_) => Ok(buf.trim_end_matches('\n').trim_end_matches('\r').to_string()),
                Err(e) => Err(anyhow::anyhow!("read error: {}", e)),
            }
        }),
    )
    .await;
    match result {
        Ok(Ok(Ok(line))) => Ok(line),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(e)) => Err(anyhow::anyhow!("spawn_blocking: {}", e)),
        Err(_) => Err(anyhow::anyhow!("timeout")),
    }
}

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
        "params": params,
    });
    send_mcp_line(plugin, &request.to_string())?;
    let response_line = read_mcp_line(plugin, Duration::from_secs(2)).await?;
    Ok(serde_json::from_str(&response_line)?)
}

/// Extract the inner JSON payload from an MCP `tools/call` text-content
/// response. Returns None if the response shape doesn't match.
fn extract_tool_text_payload(response: &Value) -> Option<Value> {
    let text = response
        .get("result")?
        .get("content")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()?;
    serde_json::from_str(text).ok()
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_agent_register_then_list_alive_via_mcp() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let home = tmpdir.path();
    let socket = home.join("run").join("claudebase").join("daemon.sock");

    let mut _daemon = spawn_daemon_with_home(home).expect("daemon spawn");
    wait_for_socket(&socket, Duration::from_secs(10))
        .await
        .expect("socket appeared");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut plugin = spawn_plugin_with_home(home).expect("plugin spawn");

    // MCP initialize handshake.
    let init = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "0.1.0" },
    });
    let init_resp = send_mcp_request(&mut plugin, "initialize", init, 1)
        .await
        .expect("initialize");
    assert!(init_resp.get("error").is_none());

    // agent_register
    let reg_params = json!({
        "name": "agent_register",
        "arguments": {
            "agent_id": "planner-int-1",
            "name": "planner",
            "thread": "telegram:99999",
            "metadata": {"role": "tactical"},
        },
    });
    let reg_resp = send_mcp_request(&mut plugin, "tools/call", reg_params, 2)
        .await
        .expect("agent_register call");
    assert!(
        reg_resp.get("error").is_none(),
        "agent_register should succeed, got error: {:?}",
        reg_resp.get("error")
    );
    let reg_payload = extract_tool_text_payload(&reg_resp).expect("register payload");
    assert_eq!(reg_payload.get("registered"), Some(&json!(true)));
    assert!(reg_payload.get("spawned_at").and_then(|v| v.as_i64()).is_some());

    // agent_list_alive (filtered by thread)
    let list_params = json!({
        "name": "agent_list_alive",
        "arguments": { "thread": "telegram:99999" },
    });
    let list_resp = send_mcp_request(&mut plugin, "tools/call", list_params, 3)
        .await
        .expect("agent_list_alive call");
    assert!(list_resp.get("error").is_none());
    let list_payload = extract_tool_text_payload(&list_resp).expect("list payload");
    let agents = list_payload
        .get("agents")
        .and_then(|v| v.as_array())
        .expect("agents array");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].get("agent_id"), Some(&json!("planner-int-1")));
    assert_eq!(agents[0].get("agent_name"), Some(&json!("planner")));

    // agent_register conflict (different agent_id, same thread+name) → friendly error
    let conflict_params = json!({
        "name": "agent_register",
        "arguments": {
            "agent_id": "planner-int-2",
            "name": "planner",
            "thread": "telegram:99999",
        },
    });
    let conflict_resp = send_mcp_request(&mut plugin, "tools/call", conflict_params, 4)
        .await
        .expect("conflict call");
    let err_msg = conflict_resp
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    assert!(
        err_msg.contains("agent_name already alive in thread"),
        "expected friendly TC-5.9 error, got: {err_msg}"
    );

    // agent_unregister
    let unreg_params = json!({
        "name": "agent_unregister",
        "arguments": { "agent_id": "planner-int-1" },
    });
    let unreg_resp = send_mcp_request(&mut plugin, "tools/call", unreg_params, 5)
        .await
        .expect("unregister call");
    assert!(unreg_resp.get("error").is_none());
    let unreg_payload = extract_tool_text_payload(&unreg_resp).expect("unregister payload");
    assert_eq!(unreg_payload.get("unregistered"), Some(&json!(true)));
    assert_eq!(unreg_payload.get("previous_state"), Some(&json!("alive")));

    // agent_reap — wire-shape: must be `reaped_count`, NOT `reaped` (insight #12)
    let reap_params = json!({
        "name": "agent_reap",
        "arguments": { "older_than_secs": 0 },
    });
    let reap_resp = send_mcp_request(&mut plugin, "tools/call", reap_params, 6)
        .await
        .expect("reap call");
    assert!(reap_resp.get("error").is_none());
    let reap_payload = extract_tool_text_payload(&reap_resp).expect("reap payload");
    assert!(
        reap_payload.get("reaped_count").is_some(),
        "reap response MUST have `reaped_count` field (TC-5.4 jq path), got: {reap_payload}"
    );
    assert!(
        reap_payload.get("reaped").is_none(),
        "reap response MUST NOT use `reaped` field (insight #12 — TC-5.4 expects `reaped_count`)"
    );
    assert!(reap_payload.get("remaining_orphaned").is_some());

    // Clean shutdown.
    let _ = std::process::Command::new("pkill")
        .arg("-f")
        .arg("daemon serve")
        .output();
    let _ = std::process::Command::new("pkill")
        .arg("-f")
        .arg("plugin serve")
        .output();
}
