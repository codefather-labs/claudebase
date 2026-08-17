//! Shared harness for daemon integration tests.
//!
//! Before v0.10 each of these tests spawned `claudebase plugin serve` and spoke
//! newline-delimited MCP over its stdio. That bridge is gone, so the tests now
//! connect to the daemon's framed UDS surface directly — which is also what
//! every real caller does (`src/daemon/client.rs` for one-shot commands,
//! `src/daemon/subscribe_client.rs` for the PTY supervisor).
//!
//! Not `claudebase::daemon::client::DaemonClient`: that resolves the socket
//! from the process environment, and these tests run daemons on temp HOMEs.
//! Connecting to an explicit path keeps them independent of env mutation, which
//! is unsound under a parallel test runner.

#![allow(dead_code)] // each test file uses a subset

use std::fs;
use std::path::Path;
use std::process::Child;
use std::time::Duration;

use anyhow::{bail, Result};
use claudebase::daemon::ipc::{read_frame, write_frame};
use serde_json::{json, Value};
use tokio::io::{ReadHalf, WriteHalf};

pub type Stream = interprocess::local_socket::tokio::Stream;

/// Spawn a daemon rooted at `tempdir` (its own HOME and XDG_RUNTIME_DIR, so it
/// never touches the operator's real chat.db or socket).
pub fn spawn_daemon_with_home(tempdir: &Path) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["daemon", "serve"]);
    cmd.env("HOME", tempdir);
    let runtime_dir = tempdir.join("run");
    fs::create_dir_all(&runtime_dir)?;
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    Ok(cmd.spawn()?)
}

pub fn socket_under(home: &Path) -> std::path::PathBuf {
    home.join("run").join("claudebase").join("daemon.sock")
}

pub async fn wait_for_socket(socket_path: &Path, max_wait: Duration) -> Result<()> {
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

/// Stop a test daemon **by pid**.
///
/// The previous harness ran `pkill -f "daemon serve"`, which on a developer
/// machine also killed the operator's real daemon — observed live 2026-08-16,
/// where systemd then restarted it under a new pid mid-session. Never match
/// daemons by command line.
pub fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Request/response + notification client over the daemon's framed socket.
pub struct Client {
    read: ReadHalf<Stream>,
    write: WriteHalf<Stream>,
}

impl Client {
    pub async fn connect(socket: &Path) -> Result<Self> {
        use interprocess::local_socket::tokio::prelude::*;
        use interprocess::local_socket::{GenericFilePath, ToFsName};

        let name = socket.to_path_buf().to_fs_name::<GenericFilePath>()?;
        let stream = Stream::connect(name).await?;
        let (read, write) = tokio::io::split(stream);
        Ok(Self { read, write })
    }

    /// Call a tool and wait for the matching response, skipping any broadcast
    /// notification that arrives in between.
    pub async fn call(&mut self, tool: &str, arguments: Value, id: u32) -> Result<Value> {
        self.send(tool, arguments, id).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let frame = tokio::time::timeout_at(deadline, read_frame(&mut self.read)).await??;
            let value: Value = serde_json::from_slice(&frame)?;
            if value.get("id").and_then(|v| v.as_u64()) == Some(id as u64) {
                return Ok(value);
            }
        }
    }

    /// Write a request without waiting for its response — for the cases where
    /// the notification it triggers is what the test is watching for.
    pub async fn send(&mut self, tool: &str, arguments: Value, id: u32) -> Result<()> {
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        }))?;
        write_frame(&mut self.write, &body).await?;
        Ok(())
    }

    /// Wait for the next `notifications/claude/channel` frame, ignoring
    /// responses to earlier requests.
    pub async fn next_notification(&mut self, within: Duration) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let frame = tokio::time::timeout_at(deadline, read_frame(&mut self.read)).await??;
            let value: Value = serde_json::from_slice(&frame)?;
            if value.get("method").and_then(|m| m.as_str()) == Some("notifications/claude/channel") {
                return Ok(value);
            }
        }
    }
}

/// Unwrap the JSON payload carried as text inside an MCP `tools/call` result.
pub fn payload(response: &Value) -> Option<Value> {
    let text = response.pointer("/result/content/0/text")?.as_str()?;
    serde_json::from_str(text).ok()
}
