//! `agent_registry` wire-contract integration test.
//!
//! Exercises `agent_register` → `agent_list_alive` → `agent_unregister` →
//! `agent_reap` against a real daemon over its UDS surface, confirming the
//! response shapes callers depend on (notably `reaped_count`, whose name a
//! prior regression got wrong — insight #12 / TC-5.4).
//!
//! Formerly `agent_registry_mcp_test.rs`, which drove these calls through
//! `claudebase plugin serve` — the MCP stdio bridge. That bridge was removed in
//! v0.10 along with the plugin transport, so the test now speaks to the daemon
//! directly, which is also how every caller reaches it today
//! (`src/daemon/client.rs`, `src/daemon/subscribe_client.rs`).
//!
//! DB-layer state machine, uniqueness and CHECK constraints are covered by the
//! unit tests in `src/daemon/agent_registry.rs`; this layer only validates the
//! dispatcher's wiring and envelopes.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Child;
use std::time::Duration;

use anyhow::{bail, Result};
use claudebase::daemon::ipc::{read_frame, write_frame};
use serde_json::{json, Value};
use tokio::io::{ReadHalf, WriteHalf};

type Stream = interprocess::local_socket::tokio::Stream;

fn spawn_daemon_with_home(tempdir: &Path) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["daemon", "serve"]);
    cmd.env("HOME", tempdir);
    let runtime_dir = tempdir.join("run");
    fs::create_dir_all(&runtime_dir)?;
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
            bail!("socket not found: {socket_path:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Minimal request/response client over the daemon's framed UDS surface.
///
/// Deliberately NOT `claudebase::daemon::client::DaemonClient`: that resolves
/// the socket from the process environment, and this test drives a daemon on a
/// temp HOME. Talking to an explicit path keeps the test independent of env
/// mutation, which is unsound with a parallel test runner.
struct Client {
    read: ReadHalf<Stream>,
    write: WriteHalf<Stream>,
}

impl Client {
    async fn connect(socket: &Path) -> Result<Self> {
        use interprocess::local_socket::tokio::prelude::*;
        use interprocess::local_socket::{GenericFilePath, ToFsName};

        let name = socket.to_path_buf().to_fs_name::<GenericFilePath>()?;
        let stream = Stream::connect(name).await?;
        let (read, write) = tokio::io::split(stream);
        Ok(Self { read, write })
    }

    async fn call(&mut self, tool: &str, arguments: Value, id: u32) -> Result<Value> {
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        }))?;
        write_frame(&mut self.write, &body).await?;

        // Skip frames that are not this request's answer — the daemon
        // multiplexes broadcast notifications onto the same connection.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let frame = tokio::time::timeout_at(deadline, read_frame(&mut self.read)).await??;
            let value: Value = serde_json::from_slice(&frame)?;
            if value.get("id").and_then(|v| v.as_u64()) == Some(id as u64) {
                return Ok(value);
            }
        }
    }
}

/// Unwrap the JSON payload carried as text inside an MCP `tools/call` result.
fn payload(response: &Value) -> Option<Value> {
    let text = response.pointer("/result/content/0/text")?.as_str()?;
    serde_json::from_str(text).ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_lifecycle_over_the_daemon_socket() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let home = tmpdir.path();
    let socket = home.join("run").join("claudebase").join("daemon.sock");

    let mut daemon = spawn_daemon_with_home(home).expect("daemon spawn");
    wait_for_socket(&socket, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    let mut client = Client::connect(&socket).await.expect("connect");

    // --- register ---
    let reg = client
        .call(
            "agent_register",
            json!({
                "agent_id": "planner-int-1",
                "name": "planner",
                "thread": "telegram:99999",
                "metadata": {"role": "tactical"},
            }),
            2,
        )
        .await
        .expect("agent_register call");
    assert!(
        reg.get("error").is_none(),
        "agent_register should succeed, got: {:?}",
        reg.get("error")
    );
    let reg_payload = payload(&reg).expect("register payload");
    assert_eq!(reg_payload.get("registered"), Some(&json!(true)));
    assert!(reg_payload
        .get("spawned_at")
        .and_then(|v| v.as_i64())
        .is_some());

    // --- list_alive, filtered by thread ---
    let list = client
        .call("agent_list_alive", json!({ "thread": "telegram:99999" }), 3)
        .await
        .expect("agent_list_alive call");
    assert!(list.get("error").is_none());
    let list_payload = payload(&list).expect("list payload");
    let agents = list_payload
        .get("agents")
        .and_then(|v| v.as_array())
        .expect("agents array");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].get("agent_id"), Some(&json!("planner-int-1")));
    assert_eq!(agents[0].get("agent_name"), Some(&json!("planner")));

    // --- name conflict in the same thread → friendly error (TC-5.9) ---
    //
    // The conflicting register MUST come from a SECOND connection. On the same
    // connection `register()` runs its rename-as-cleanup sweep instead: an
    // alive row with a different agent_id on the same (connection, thread) is
    // a prior self-registration being renamed, so it is buried rather than
    // treated as a rival. The conflict this asserts is the real-world one —
    // two different sessions claiming one name in one thread.
    let mut rival = Client::connect(&socket).await.expect("second connection");
    let conflict = rival
        .call(
            "agent_register",
            json!({
                "agent_id": "planner-int-2",
                "name": "planner",
                "thread": "telegram:99999",
            }),
            4,
        )
        .await
        .expect("conflict call");
    let err_msg = conflict
        .pointer("/error/message")
        .and_then(|m| m.as_str())
        .unwrap_or("");
    assert!(
        err_msg.contains("agent_name already alive in thread"),
        "expected the friendly TC-5.9 error, got: {err_msg}"
    );

    // --- unregister ---
    let unreg = client
        .call(
            "agent_unregister",
            json!({ "agent_id": "planner-int-1" }),
            5,
        )
        .await
        .expect("unregister call");
    assert!(unreg.get("error").is_none());
    let unreg_payload = payload(&unreg).expect("unregister payload");
    assert_eq!(unreg_payload.get("unregistered"), Some(&json!(true)));
    assert_eq!(unreg_payload.get("previous_state"), Some(&json!("alive")));

    // --- reap: the field is `reaped_count`, never `reaped` (insight #12) ---
    let reap = client
        .call("agent_reap", json!({ "older_than_secs": 0 }), 6)
        .await
        .expect("reap call");
    assert!(reap.get("error").is_none());
    let reap_payload = payload(&reap).expect("reap payload");
    assert!(
        reap_payload.get("reaped_count").is_some(),
        "reap response MUST carry `reaped_count` (TC-5.4 jq path), got: {reap_payload}"
    );
    assert!(
        reap_payload.get("reaped").is_none(),
        "reap response MUST NOT use `reaped` (insight #12)"
    );
    assert!(reap_payload.get("remaining_orphaned").is_some());

    // Kill only OUR daemon by pid. The previous version ran `pkill -f "daemon
    // serve"`, which also killed the operator's real daemon when the suite ran
    // on a working machine.
    let _ = daemon.kill();
    let _ = daemon.wait();
}
