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
//! ## State machine
//!
//! Each plugin invocation runs ONE bridge loop:
//!
//! 1. Try to connect to the daemon UDS. If the first connection fails,
//!    we enter daemon-down mode and serve `tools/list` sentinel +
//!    `claudebase_daemon_status` locally.
//! 2. `tokio::select!` over (stdin line, optional daemon read, optional
//!    reconnect timer). Each branch dispatches to the appropriate
//!    handler.
//! 3. On stdin EOF: drop pending requests, close UDS, exit 0 within
//!    100 ms (SEC-6).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

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

/// Entry point for the plugin bridge — runs until stdin EOF or fatal
/// I/O failure on stdout. Daemon UDS failures are non-fatal and drop
/// us into daemon-down mode.
pub async fn run() -> anyhow::Result<()> {
    let socket = socket_path().context("compute daemon socket path")?;

    // Attempt initial connection. We do 3 quick retries (FR-ACD-3.3)
    // before falling back to daemon-down mode.
    let mut uds = connect_with_retries(&socket, INITIAL_RETRY_COUNT, INITIAL_RETRY_DELAY_MS).await;

    // Pending requests map: id → oneshot::Sender for the response.
    // Slice 1b: we don't actually use the oneshot channel — each
    // request blocks the bridge loop until reply arrives because we
    // multiplex stdin and daemon reads in `tokio::select!`. The map is
    // kept for shape parity with the SEC-4 cap discipline and Slice 5
    // routing.
    let pending: HashMap<Value, oneshot::Sender<Value>> = HashMap::new();

    // stdin: newline-delimited per STRUCTURAL-A1.
    let stdin = tokio::io::stdin();
    let mut stdin_reader = BufReader::new(stdin).lines();

    let mut stdout = tokio::io::stdout();

    // Track whether we believe the daemon is reachable. Used to gate
    // tools/list_changed emission on down → up transition.
    let mut daemon_down = uds.is_none();

    loop {
        // Branch 1: read a line from stdin (newline-delimited).
        // Branch 2: in deliberate future revisions we'd also select over
        // a daemon UDS read for unsolicited notifications (Slice 3).
        // Slice 1b keeps the loop stdin-driven — every daemon read is
        // immediately after a daemon write (request/response pairing).
        let line_result = stdin_reader.next_line().await;

        let line = match line_result {
            Ok(Some(l)) => l,
            Ok(None) => {
                // Stdin EOF — clean shutdown (SEC-6). Drop pending map
                // to close all oneshot senders. UDS drops when `uds`
                // goes out of scope.
                drop(pending);
                drop(uds);
                return Ok(());
            }
            Err(e) => {
                eprintln!("claudebase plugin: stdin read error: {e}");
                drop(pending);
                drop(uds);
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
                        // No capabilities / no other fields. Keeps PII / fingerprint
                        // surface minimal until Slice 1c brings tracing.
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
                        eprintln!(
                            "claudebase plugin: initialize from '{}' v'{}'",
                            client_name, client_ver
                        );

                        write_mcp_line(&mut stdout, &initialize_response(id)).await?;
                    }
                    "tools/list" => {
                        if let Some(stream) = uds.as_mut() {
                            // Daemon-up: forward.
                            match forward_to_daemon(stream, &parsed).await {
                                Ok(response) => {
                                    write_mcp_line(&mut stdout, &response).await?;
                                }
                                Err(_) => {
                                    // UDS failure mid-flight — drop the
                                    // socket, fall back to daemon-down,
                                    // and serve the sentinel locally.
                                    uds = None;
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
                            uds = try_reconnect(&socket).await;
                            if let Some(stream) = uds.as_mut() {
                                // Came back up. Per mcp-protocol-expert
                                // invariant #5: emit notifications/tools/list_changed
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
                                match forward_to_daemon(stream, &parsed).await {
                                    Ok(response) => {
                                        write_mcp_line(&mut stdout, &response).await?;
                                    }
                                    Err(_) => {
                                        uds = None;
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
                        if uds.is_none() {
                            // Try a quick reconnect.
                            uds = try_reconnect(&socket).await;
                            if uds.is_some() && daemon_down {
                                write_mcp_line(
                                    &mut stdout,
                                    &tools_list_changed_notification(),
                                )
                                .await?;
                                daemon_down = false;
                            }
                        }

                        if uds.is_none() {
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
                        let stream = uds.as_mut().expect("uds checked Some above");
                        match forward_to_daemon(stream, &parsed).await {
                            Ok(response) => {
                                write_mcp_line(&mut stdout, &response).await?;
                            }
                            Err(_) => {
                                uds = None;
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
) -> Option<interprocess::local_socket::tokio::Stream> {
    for attempt in 0..retries {
        if let Some(stream) = try_connect(socket).await {
            return Some(stream);
        }
        if attempt + 1 < retries {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    None
}

/// Single-attempt connect, no retry.
async fn try_connect(
    socket: &std::path::Path,
) -> Option<interprocess::local_socket::tokio::Stream> {
    use interprocess::local_socket::tokio::{prelude::*, Stream};
    use interprocess::local_socket::{GenericFilePath, ToFsName};

    let path_name = socket.to_path_buf().to_fs_name::<GenericFilePath>().ok()?;
    Stream::connect(path_name).await.ok()
}

/// Re-attempt connection (single try, fast). Used opportunistically
/// when we're in daemon-down mode and a request arrives — gives the
/// daemon a chance to have come back up.
async fn try_reconnect(
    socket: &std::path::Path,
) -> Option<interprocess::local_socket::tokio::Stream> {
    try_connect(socket).await
}

/// Forward one MCP request to the daemon via UDS and read the response.
/// Returns Err on UDS I/O failure so the caller can drop the socket and
/// fall back to daemon-down. SEC-1 frame cap applies to the daemon
/// response body.
async fn forward_to_daemon(
    stream: &mut interprocess::local_socket::tokio::Stream,
    request: &Value,
) -> anyhow::Result<Value> {
    let body = serde_json::to_vec(request).context("serialize MCP request")?;
    write_frame(stream, &body).await.context("write daemon frame")?;
    let response_body = read_frame(stream).await.context("read daemon frame")?;
    if response_body.len() > MAX_MCP_FRAME_SIZE {
        anyhow::bail!(
            "daemon response exceeds MCP cap: {} > {}",
            response_body.len(),
            MAX_MCP_FRAME_SIZE
        );
    }
    let parsed: Value =
        serde_json::from_slice(&response_body).context("parse daemon response JSON")?;
    Ok(parsed)
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
