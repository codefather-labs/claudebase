//! Short-lived request/response client for the daemon's UDS (named-pipe on
//! Windows) surface — Slice 1 of the pty-transport feature.
//!
//! ## Why this exists
//!
//! Until now the ONLY client of the daemon socket was `src/plugin/bridge.rs`,
//! a long-lived MCP bridge whose connect/retry/correlate logic is entangled
//! with stdio pumping and daemon-down fallbacks. The new transport needs the
//! opposite shape: a process that starts, sends ONE tool call, reads ONE
//! response, prints it, and exits — `claudebase telegram send`,
//! `claudebase agent chat`.
//!
//! ## Wire contract (verified against `src/daemon/server.rs` dispatch)
//!
//! - framing: `crate::daemon::ipc::{read_frame, write_frame}` (4-byte BE
//!   length + JSON body);
//! - the daemon does NOT require an `initialize` handshake — it dispatches on
//!   `method` directly, so a client may send `tools/call` as its first frame;
//! - a tool response carries its payload as a JSON **string** inside
//!   `result.content[0].text` (see `tool_text_response`), so callers get one
//!   more `serde_json::from_str` than they expect;
//! - errors come back as JSON-RPC `error { code, message }`.
//!
//! ## Frames that are not ours
//!
//! The daemon multiplexes broadcast notifications onto the same connection.
//! A short-lived client can receive one before its own response (it is
//! subscribed to nothing, but the daemon may still push connection-scoped
//! frames), so `call_tool` skips any frame whose `id` does not match the
//! request instead of assuming strict request/response alternation.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::daemon::ipc::{read_frame, write_frame};
use crate::daemon::server::socket_path;

/// Connect attempts before giving up. The daemon is a user service that may
/// be mid-restart when a CLI call lands; three quick tries costs 500 ms in
/// the worst case and avoids a spurious failure in the common one.
const CONNECT_RETRIES: u32 = 3;
const CONNECT_RETRY_DELAY_MS: u64 = 250;

/// How long to wait for the daemon's answer to a single tool call before
/// giving up. Generous: `chat_reply` does a SQLite write plus an outbound
/// Telegram enqueue. Short enough that a wedged daemon does not hang an
/// agent's Bash call forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

pub struct DaemonClient {
    read: tokio::io::ReadHalf<interprocess::local_socket::tokio::Stream>,
    write: tokio::io::WriteHalf<interprocess::local_socket::tokio::Stream>,
    next_id: u64,
}

impl DaemonClient {
    /// Connect to the running daemon, retrying briefly.
    ///
    /// The error text is operator-facing: a CLI call that fails here almost
    /// always means the daemon is not running, and the message says so
    /// instead of leaking a bare ENOENT.
    pub async fn connect() -> Result<Self> {
        let socket = socket_path().context("compute daemon socket path")?;
        for attempt in 0..CONNECT_RETRIES {
            match try_connect(&socket).await {
                Some(stream) => {
                    let (read, write) = tokio::io::split(stream);
                    return Ok(Self {
                        read,
                        write,
                        next_id: 1,
                    });
                }
                None if attempt + 1 < CONNECT_RETRIES => {
                    tokio::time::sleep(Duration::from_millis(CONNECT_RETRY_DELAY_MS)).await;
                }
                None => {}
            }
        }
        bail!(
            "claudebase daemon is not reachable at {}\n\
             start it with:  claudebase daemon start",
            socket.display()
        )
    }

    /// Send `tools/call` and return the tool's payload, already unwrapped
    /// from the MCP `content[0].text` envelope and JSON-parsed.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        });
        let body = serde_json::to_vec(&request).context("serialize daemon request")?;
        write_frame(&mut self.write, &body)
            .await
            .context("write request frame to daemon")?;

        let response = tokio::time::timeout(CALL_TIMEOUT, self.read_response_for(id))
            .await
            .with_context(|| format!("daemon did not answer `{name}` within {CALL_TIMEOUT:?}"))??;

        if let Some(err) = response.get("error") {
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown daemon error");
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            bail!("daemon rejected `{name}` ({code}): {message}");
        }

        // Unwrap `result.content[0].text`, which is itself a JSON document.
        // A tool that ever returns a non-JSON string still yields something
        // usable rather than an error, hence the fallback to Value::String.
        let text = response
            .pointer("/result/content/0/text")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("daemon response for `{name}` had no text payload"))?;
        Ok(serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string())))
    }

    /// Read frames until one echoes `id`, skipping broadcast notifications
    /// and any out-of-band frame the daemon multiplexes onto the socket.
    async fn read_response_for(&mut self, id: u64) -> Result<Value> {
        loop {
            let body = read_frame(&mut self.read)
                .await
                .context("read response frame from daemon (connection closed?)")?;
            let frame: Value = serde_json::from_slice(&body)
                .context("daemon sent a frame that is not valid JSON")?;
            match frame.get("id").and_then(|v| v.as_u64()) {
                Some(got) if got == id => return Ok(frame),
                // A notification (`id` absent) or another request's response.
                _ => continue,
            }
        }
    }
}

async fn try_connect(socket: &Path) -> Option<interprocess::local_socket::tokio::Stream> {
    use interprocess::local_socket::tokio::prelude::*;
    use interprocess::local_socket::tokio::Stream;
    use interprocess::local_socket::{GenericFilePath, ToFsName};

    let name = socket.to_path_buf().to_fs_name::<GenericFilePath>().ok()?;
    Stream::connect(name).await.ok()
}
