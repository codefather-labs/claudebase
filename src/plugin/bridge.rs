//! STDIO ↔ daemon-UDS bridge for the Claude Code MCP plugin.
//!
//! Connection-id discipline: the daemon owns connection_id (UUID v4
//! generated at accept, see `src/daemon/server.rs`). The plugin does
//! NOT learn its connection_id from any handshake frame — the daemon
//! tracks the mapping internally for Slice 5 routing. Plugin is
//! connection-id-opaque.
//!
//! ## Wire-format invariant (recap)
//!
//! - STDIO side (Claude Code ↔ plugin): newline-delimited UTF-8 JSON.
//!   Read with `BufReader::lines()`. Write with
//!   `stdout.write_all(serialized + "\n")` then `flush()`.
//! - UDS side (plugin ↔ daemon): length-prefixed via
//!   `crate::daemon::ipc::{read_frame, write_frame}`.
//!
//! The dispatcher in this file is the ONLY place these two protocols
//! meet — calling `ipc::read_frame` on stdin OR doing newline-framing
//! on the UDS socket is a wire-format violation.
//!
//! ## Slice 3 — UDS reader task + notification relay
//!
//! Daemon-pushed `notifications/claude/channel` frames are
//! asynchronous: they can arrive at any time while the plugin is
//! waiting on stdin. To relay them without blocking on stdin, we split
//! the UDS stream into read/write halves:
//!   - The read half is owned by a spawned task that pumps every UDS
//!     frame to an `mpsc::UnboundedSender<Value>`.
//!   - The write half stays with the bridge main loop and is used for
//!     outbound requests.
//! The main loop's `tokio::select!` polls stdin AND the uds-mpsc; any
//! notification observed while idle is relayed to stdout immediately.
//! When the bridge is mid-request (after `write_frame` to UDS), it
//! drains the same uds-mpsc until the response is observed — relaying
//! notifications encountered along the way.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::daemon::ipc::{read_frame, write_frame};
use crate::daemon::server::socket_path;
use crate::plugin::mcp::{
    self, classify, daemon_status_down_call_response, error_response, initialize_response,
    parse_error_response, tools_list_changed_notification, tools_list_daemon_down_response,
    validate_tool_name, Inbound, ERROR_INTERNAL, ERROR_INVALID_PARAMS, ERROR_METHOD_NOT_FOUND,
    MAX_MCP_FRAME_SIZE, SUPPORTED_PROTOCOL_VERSION,
};

/// Cap on in-flight requests waiting for a daemon reply. Per SEC-4 from
/// Vault — beyond this we refuse new requests with `-32603` instead of
/// growing the map unboundedly.
const MAX_PENDING_REQUESTS: usize = 1024;

/// Initial connect / reconnect attempts before declaring daemon-down.
/// Per FR-ACD-3.3 — 250 ms × 3 — then exponential after that for
/// reconnect attempts during a running session.
const INITIAL_RETRY_COUNT: u32 = 3;
const INITIAL_RETRY_DELAY_MS: u64 = 250;

/// Handle to the daemon side of the bridge: the write half of the
/// split UDS stream + the mpsc::Receiver carrying inbound UDS frames
/// (responses + notifications). When `None`, the daemon is down.
struct DaemonChannel {
    write_half: Arc<Mutex<tokio::io::WriteHalf<interprocess::local_socket::tokio::Stream>>>,
    inbound_rx: mpsc::UnboundedReceiver<Value>,
    _reader_task: tokio::task::JoinHandle<()>,
}

/// Entry point for the plugin bridge — runs until stdin EOF or fatal
/// I/O failure on stdout. Daemon UDS failures are non-fatal and drop
/// us into daemon-down mode.
pub async fn run() -> anyhow::Result<()> {
    let socket = socket_path().context("compute daemon socket path")?;

    // Attempt initial connection. We do 3 quick retries (FR-ACD-3.3)
    // before falling back to daemon-down mode.
    let mut daemon = connect_with_retries(&socket, INITIAL_RETRY_COUNT, INITIAL_RETRY_DELAY_MS).await;

    // Pending requests map: id → oneshot::Sender for the response.
    // Held only for SEC-4 cap discipline — Slice 3 still uses the
    // drain-until-id-match pattern inside `forward_to_daemon`, so the
    // map is structurally present but never populated. Keep the cap
    // gate in `tools/call` so we can refuse oversubscription cleanly.
    let pending: HashMap<Value, oneshot::Sender<Value>> = HashMap::new();

    // stdin: newline-delimited per STRUCTURAL-A1.
    let stdin = tokio::io::stdin();
    let mut stdin_reader = BufReader::new(stdin).lines();

    let mut stdout = tokio::io::stdout();

    // Track whether we believe the daemon is reachable. Used to gate
    // tools/list_changed emission on down → up transition.
    let mut daemon_down = daemon.is_none();

    loop {
        // tokio::select! across (stdin, idle UDS notification drain).
        // Slice 3: when the daemon pushes a notification while we're
        // idle (between requests), we relay it onto stdout via the
        // uds-mpsc branch.
        //
        // Both branches use cancellation-safe futures (Rule 4 from
        // ASYNC_INVARIANTS.md):
        //   - stdin_reader.next_line() — documented cancellation-safe
        //   - mpsc::Receiver::recv()   — documented cancellation-safe
        let line_opt = if let Some(d) = daemon.as_mut() {
            tokio::select! {
                line = stdin_reader.next_line() => Some(line),
                frame = d.inbound_rx.recv() => {
                    match frame {
                        Some(f) => {
                            if f.get("id").is_none() {
                                // Notification — relay to stdout.
                                write_mcp_line(&mut stdout, &f).await?;
                            } else {
                                // Unexpected response while idle —
                                // log and drop. This shouldn't happen
                                // with the current request/response
                                // pairing.
                                let resp_id = f.get("id").cloned().unwrap_or(Value::Null);
                                tracing::warn!(
                                    response_id = %resp_id,
                                    "unexpected daemon response while idle (dropping)"
                                );
                            }
                            None
                        }
                        None => {
                            // UDS reader task ended — daemon link is gone.
                            daemon = None;
                            daemon_down = true;
                            None
                        }
                    }
                }
            }
        } else {
            Some(stdin_reader.next_line().await)
        };

        let line_result = match line_opt {
            Some(r) => r,
            None => continue,
        };

        let line = match line_result {
            Ok(Some(l)) => l,
            Ok(None) => {
                // Stdin EOF — clean shutdown (SEC-6). Drop pending map
                // to close all oneshot senders. UDS drops when `daemon`
                // goes out of scope.
                drop(pending);
                drop(daemon);
                return Ok(());
            }
            Err(e) => {
                tracing::error!(error = %e, "stdin read error");
                drop(pending);
                drop(daemon);
                return Ok(());
            }
        };

        // SEC-1 frame-size cap on stdin line.
        if line.len() > MAX_MCP_FRAME_SIZE {
            // Send Parse Error and keep going — the spec lets us reject
            // oversized frames as malformed.
            write_mcp_line(&mut stdout, &parse_error_response()).await?;
            continue;
        }

        // Try to parse JSON. On failure → Parse Error response (SEC-3).
        let parsed: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                write_mcp_line(&mut stdout, &parse_error_response()).await?;
                continue;
            }
        };

        match classify(&parsed) {
            Inbound::Notification { method, .. } => {
                // Notifications: discard or relay.
                if method == "notifications/initialized" {
                    // Discard per MCP spec — purely a handshake-complete
                    // signal from the client. Slice 1b does not track
                    // post-init state; future slices may.
                    continue;
                }
                // Unknown notification — silently discard per JSON-RPC
                // notifications convention.
                continue;
            }
            Inbound::Invalid => {
                // Has neither `id` (would-be-notification) nor `method`,
                // OR malformed in some other way. Respond with -32600.
                let id = parsed.get("id").cloned().unwrap_or(Value::Null);
                write_mcp_line(
                    &mut stdout,
                    &error_response(id, mcp::ERROR_INVALID_REQUEST, "Invalid Request"),
                )
                .await?;
                continue;
            }
            Inbound::Request { id, method, params } => {
                match method.as_str() {
                    "initialize" => {
                        // Validate protocolVersion.
                        let client_version = params
                            .get("protocolVersion")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if client_version != SUPPORTED_PROTOCOL_VERSION {
                            let msg = format!(
                                "unsupported protocolVersion: client requested '{}', server supports '{}'",
                                client_version, SUPPORTED_PROTOCOL_VERSION
                            );
                            write_mcp_line(
                                &mut stdout,
                                &error_response(id, ERROR_INVALID_PARAMS, &msg),
                            )
                            .await?;
                            continue;
                        }

                        // SEC-5: log only clientInfo.name (≤64) + version (≤32).
                        let client_name = params
                            .get("clientInfo")
                            .and_then(|c| c.get("name"))
                            .and_then(|n| n.as_str())
                            .map(|s| truncate(s, 64))
                            .unwrap_or_default();
                        let client_ver = params
                            .get("clientInfo")
                            .and_then(|c| c.get("version"))
                            .and_then(|v| v.as_str())
                            .map(|s| truncate(s, 32))
                            .unwrap_or_default();
                        tracing::info!(
                            client = %client_name,
                            version = %client_ver,
                            "plugin initialize"
                        );

                        write_mcp_line(&mut stdout, &initialize_response(id)).await?;
                    }
                    "tools/list" => {
                        if let Some(d) = daemon.as_mut() {
                            // Daemon-up: forward.
                            match forward_to_daemon(d, &parsed, &mut stdout).await {
                                Ok(response) => {
                                    write_mcp_line(&mut stdout, &response).await?;
                                }
                                Err(_) => {
                                    // UDS failure mid-flight — drop the
                                    // socket, fall back to daemon-down,
                                    // and serve the sentinel locally.
                                    daemon = None;
                                    daemon_down = true;
                                    write_mcp_line(
                                        &mut stdout,
                                        &tools_list_daemon_down_response(id),
                                    )
                                    .await?;
                                }
                            }
                        } else {
                            // Daemon-down: try a quick reconnect first.
                            daemon = try_reconnect(&socket).await;
                            if let Some(d) = daemon.as_mut() {
                                // Came back up. Emit notifications/tools/list_changed
                                // BEFORE sending the new tools/list response so
                                // Claude Code re-fetches.
                                if daemon_down {
                                    write_mcp_line(
                                        &mut stdout,
                                        &tools_list_changed_notification(),
                                    )
                                    .await?;
                                    daemon_down = false;
                                }
                                match forward_to_daemon(d, &parsed, &mut stdout).await {
                                    Ok(response) => {
                                        write_mcp_line(&mut stdout, &response).await?;
                                    }
                                    Err(_) => {
                                        daemon = None;
                                        daemon_down = true;
                                        write_mcp_line(
                                            &mut stdout,
                                            &tools_list_daemon_down_response(id),
                                        )
                                        .await?;
                                    }
                                }
                            } else {
                                write_mcp_line(&mut stdout, &tools_list_daemon_down_response(id))
                                    .await?;
                            }
                        }
                    }
                    "tools/call" => {
                        let tool_name = params
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("");

                        // SEC-7 whitelist enforcement BEFORE any UDS frame
                        // is sent.
                        if !validate_tool_name(tool_name) {
                            write_mcp_line(
                                &mut stdout,
                                &error_response(
                                    id,
                                    ERROR_METHOD_NOT_FOUND,
                                    "Method not found",
                                ),
                            )
                            .await?;
                            continue;
                        }

                        // Daemon-down: only claudebase_daemon_status is
                        // valid; everything else → -32601.
                        if daemon.is_none() {
                            // Try a quick reconnect.
                            daemon = try_reconnect(&socket).await;
                            if daemon.is_some() && daemon_down {
                                write_mcp_line(
                                    &mut stdout,
                                    &tools_list_changed_notification(),
                                )
                                .await?;
                                daemon_down = false;
                            }
                        }

                        if daemon.is_none() {
                            if tool_name == "claudebase_daemon_status" {
                                // Verbatim FR-ACD-10.1 message (SEC-8).
                                write_mcp_line(
                                    &mut stdout,
                                    &daemon_status_down_call_response(id),
                                )
                                .await?;
                            } else {
                                write_mcp_line(
                                    &mut stdout,
                                    &error_response(
                                        id,
                                        ERROR_METHOD_NOT_FOUND,
                                        "Method not found",
                                    ),
                                )
                                .await?;
                            }
                            continue;
                        }

                        // Daemon-up: forward.
                        if pending.len() >= MAX_PENDING_REQUESTS {
                            write_mcp_line(
                                &mut stdout,
                                &error_response(
                                    id,
                                    ERROR_INTERNAL,
                                    "too many in-flight requests",
                                ),
                            )
                            .await?;
                            continue;
                        }
                        let d = daemon.as_mut().expect("daemon checked Some above");
                        match forward_to_daemon(d, &parsed, &mut stdout).await {
                            Ok(response) => {
                                write_mcp_line(&mut stdout, &response).await?;
                            }
                            Err(_) => {
                                daemon = None;
                                daemon_down = true;
                                if tool_name == "claudebase_daemon_status" {
                                    write_mcp_line(
                                        &mut stdout,
                                        &daemon_status_down_call_response(id),
                                    )
                                    .await?;
                                } else {
                                    write_mcp_line(
                                        &mut stdout,
                                        &error_response(
                                            id,
                                            ERROR_METHOD_NOT_FOUND,
                                            "Method not found",
                                        ),
                                    )
                                    .await?;
                                }
                            }
                        }
                    }
                    _ => {
                        // Unknown method.
                        write_mcp_line(
                            &mut stdout,
                            &error_response(id, ERROR_METHOD_NOT_FOUND, "Method not found"),
                        )
                        .await?;
                    }
                }
            }
        }
    }
}

/// Attempt to connect to the daemon UDS, retrying up to `retries`
/// times with `delay_ms` ms between attempts. Returns `None` after
/// exhaustion — caller falls back to daemon-down mode.
async fn connect_with_retries(
    socket: &std::path::Path,
    retries: u32,
    delay_ms: u64,
) -> Option<DaemonChannel> {
    for attempt in 0..retries {
        if let Some(stream) = try_connect(socket).await {
            return Some(spawn_daemon_channel(stream));
        }
        if attempt + 1 < retries {
            tracing::warn!(
                attempt = attempt + 1,
                of = retries,
                delay_ms = delay_ms,
                socket = %socket.display(),
                "daemon connect failed; retrying"
            );
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    tracing::warn!(
        retries = retries,
        socket = %socket.display(),
        "daemon connect retries exhausted; entering daemon-down mode"
    );
    None
}

/// Single-attempt connect, no retry.
async fn try_connect(
    socket: &std::path::Path,
) -> Option<interprocess::local_socket::tokio::Stream> {
    use interprocess::local_socket::tokio::prelude::*;
    use interprocess::local_socket::tokio::Stream;
    use interprocess::local_socket::{GenericFilePath, ToFsName};

    let path_name = socket.to_path_buf().to_fs_name::<GenericFilePath>().ok()?;
    Stream::connect(path_name).await.ok()
}

/// Re-attempt connection (single try, fast). Used opportunistically
/// when we're in daemon-down mode and a request arrives — gives the
/// daemon a chance to have come back up.
async fn try_reconnect(socket: &std::path::Path) -> Option<DaemonChannel> {
    let stream = try_connect(socket).await?;
    Some(spawn_daemon_channel(stream))
}

/// Split the UDS stream, spawn the inbound reader task, return the
/// composite handle the main loop uses.
fn spawn_daemon_channel(stream: interprocess::local_socket::tokio::Stream) -> DaemonChannel {
    let (mut read_half, write_half) = tokio::io::split(stream);
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    // Reader task: read length-prefixed frames forever; forward each
    // parsed JSON Value to the mpsc. Exits on EOF or parse failure.
    let reader_task = tokio::spawn(async move {
        loop {
            let body = match read_frame(&mut read_half).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::info!(error = %e, "daemon UDS read ended");
                    return;
                }
            };
            if body.len() > MAX_MCP_FRAME_SIZE {
                tracing::warn!(len = body.len(), "daemon frame exceeds MCP cap; dropping");
                continue;
            }
            let parsed: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "daemon frame JSON parse failed; dropping");
                    continue;
                }
            };
            if tx.send(parsed).is_err() {
                // Main loop dropped the receiver — bridge is shutting down.
                return;
            }
        }
    });
    DaemonChannel {
        write_half: Arc::new(Mutex::new(write_half)),
        inbound_rx: rx,
        _reader_task: reader_task,
    }
}

/// Forward one MCP request to the daemon via UDS and read the response,
/// relaying any notifications encountered along the way to stdout.
///
/// Returns Err on UDS write failure OR when the inbound_rx is closed
/// (daemon link gone) so the caller can drop the socket and fall back
/// to daemon-down. SEC-1 frame cap applies to the daemon response body.
async fn forward_to_daemon(
    daemon: &mut DaemonChannel,
    request: &Value,
    stdout: &mut tokio::io::Stdout,
) -> anyhow::Result<Value> {
    let body = serde_json::to_vec(request).context("serialize MCP request")?;
    {
        let mut wh = daemon.write_half.lock().await;
        write_frame(&mut *wh, &body)
            .await
            .context("write daemon frame")?;
    }
    let expected_id = request.get("id").cloned();
    loop {
        let frame = match daemon.inbound_rx.recv().await {
            Some(v) => v,
            None => anyhow::bail!("daemon inbound channel closed"),
        };
        // Notifications (no `id`) → relay to stdout, keep reading.
        if frame.get("id").is_none() {
            write_mcp_line(stdout, &frame).await?;
            continue;
        }
        if let Some(exp) = expected_id.as_ref() {
            if frame.get("id") != Some(exp) {
                let got = frame.get("id").cloned().unwrap_or(Value::Null);
                tracing::warn!(
                    expected = %exp,
                    got = %got,
                    "daemon response id mismatch (returning anyway)"
                );
            }
        }
        return Ok(frame);
    }
}

/// Write one JSON value as a newline-terminated UTF-8 line to stdout
/// and flush. The newline is the wire-format delimiter on the MCP leg.
async fn write_mcp_line(stdout: &mut tokio::io::Stdout, value: &Value) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(value).context("serialize MCP response")?;
    line.push(b'\n');
    stdout.write_all(&line).await.context("write stdout")?;
    stdout.flush().await.context("flush stdout")?;
    Ok(())
}

/// Truncate `s` to at most `max` bytes on a UTF-8 character boundary.
/// Used for SEC-5 clientInfo log redaction.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Walk back to a char boundary.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
