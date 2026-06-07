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

    // Capture this bridge's own agent_id at startup so we can filter
    // routing-key broadcasts on the way OUT to the CC. Daemon publishes
    // every `notifications/claude/channel` frame to every subscriber of
    // the thread; without this filter every CC subscribed to the chat
    // would see every message and operators would see "the bot sent
    // to both CCs even though /switch is bound to one of them"
    // (operator report 2026-06-04 multi-CC test).
    //
    // The id is read from .claudebase/config.json via derive_identity
    // and is updated in-place when the user renames via the
    // `agent_register` MCP tool — same persistence layer that
    // persist_rename_if_changed already touches, kept in lock-step.
    // Empty agent_id (unrecoverable identity) degrades to "relay all"
    // so a misconfigured bridge never silently swallows messages.
    let mut self_agent_id: String = derive_identity().agent_id;

    // Bridge self-bootstrap on initial daemon connect. Also fires on
    // every successful try_reconnect — see the tools/list / tools/call
    // reconnect paths below.
    if let Some(d) = daemon.as_ref() {
        bootstrap_after_connect(d).await;
    }

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

    // Slice 7.x diagnostic helper — appends a one-line entry to
    // /tmp/claudebase-plugin-trace.log when CLAUDEBASE_PLUGIN_TRACE=1.
    // Best-effort: file open / write errors are swallowed so the
    // diagnostic NEVER affects production semantics. The marker is a
    // simple compact line that captures direction (IN/OUT) + body so
    // the operator can grep the wire shape from outside Claude Code's
    // MCP log sandbox.
    fn trace_line(tag: &str, body: &str) {
        if std::env::var("CLAUDEBASE_PLUGIN_TRACE").as_deref() != Ok("1") {
            return;
        }
        use std::io::Write;
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/claudebase-plugin-trace.log")
        {
            let _ = writeln!(
                log,
                "[{}] pid={} {} bytes={} body={}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0),
                std::process::id(),
                tag,
                body.len(),
                body
            );
        }
    }

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
                                // Notification — relay to stdout, but
                                // filter `notifications/claude/channel`
                                // frames whose `meta.target_agent_id`
                                // names a DIFFERENT CLI. Daemon-side
                                // fanout is unfiltered (publish-to-all-
                                // subscribers); this bridge owns the
                                // last-mile addressee gate so the
                                // operator's /switch routing surfaces
                                // ONLY in the CC the binding names.
                                if should_relay_channel_notification(&f, &self_agent_id) {
                                    trace_line("UDS→STDOUT notif", &f.to_string());
                                    write_mcp_line(&mut stdout, &f).await?;
                                } else {
                                    trace_line(
                                        "UDS→DROP notif (target_agent_id mismatch)",
                                        &f.to_string(),
                                    );
                                }
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
                trace_line("STDIN→PLUGIN EOF", "<eof>");
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

        // Slice 7.x diagnostic — every stdin line from Claude Code.
        trace_line("STDIN→PLUGIN", &line);

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
                        // MCP spec version negotiation: client sends its
                        // preferred protocolVersion; server responds with
                        // ITS preferred version (may differ). Client
                        // decides whether it can downgrade. So we MUST
                        // NOT reject on mismatch — just log and respond
                        // with our supported version.
                        //
                        // Live-test discovery (2026-05-18): real Claude
                        // Code sends a newer protocolVersion (e.g.
                        // "2025-03-26" or "2025-06-18"); the original
                        // Slice 1b strict-equality check returned
                        // -32602 Invalid Params and the plugin failed
                        // to connect. Relaxed to log-only.
                        let client_version = params
                            .get("protocolVersion")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if client_version != SUPPORTED_PROTOCOL_VERSION {
                            tracing::info!(
                                client_version = %client_version,
                                server_version = SUPPORTED_PROTOCOL_VERSION,
                                "protocolVersion mismatch; responding with server's version per MCP negotiation contract"
                            );
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
                            match forward_to_daemon(d, &parsed, &mut stdout, &self_agent_id).await {
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
                                // Came back up. Re-fire the bridge self-
                                // bootstrap so the new daemon connection
                                // gets agent_register + chat_subscribe — a
                                // daemon bounce otherwise silently drops
                                // subscription state. Operator request
                                // 2026-06-04, follow-up to the original
                                // bootstrap-at-initial-connect patch.
                                bootstrap_after_connect(d).await;
                                // Emit notifications/tools/list_changed
                                // BEFORE sending the new tools/list response
                                // so Claude Code re-fetches.
                                if daemon_down {
                                    write_mcp_line(
                                        &mut stdout,
                                        &tools_list_changed_notification(),
                                    )
                                    .await?;
                                    daemon_down = false;
                                }
                                match forward_to_daemon(d, &parsed, &mut stdout, &self_agent_id).await {
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
                            if let Some(d) = daemon.as_ref() {
                                // Re-fire bridge self-bootstrap on the
                                // new daemon connection so subscription
                                // state survives daemon bounce (see the
                                // matching block in tools/list above).
                                bootstrap_after_connect(d).await;
                                if daemon_down {
                                    write_mcp_line(
                                        &mut stdout,
                                        &tools_list_changed_notification(),
                                    )
                                    .await?;
                                    daemon_down = false;
                                }
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
                        // Capture rename intent BEFORE forwarding so we can
                        // persist `.claudebase/config.json` after the daemon
                        // confirms the new id landed. Only matters when the
                        // tool call is `agent_register`; for every other tool
                        // these locals stay None and the persist hook is a
                        // no-op.
                        let rename_target: Option<(String, Option<String>)> = if tool_name
                            == "agent_register"
                        {
                            params.get("arguments").and_then(|a| {
                                let new_id = a
                                    .get("agent_id")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())?;
                                let new_name = a
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                Some((new_id, new_name))
                            })
                        } else {
                            None
                        };

                        let d = daemon.as_mut().expect("daemon checked Some above");
                        match forward_to_daemon(d, &parsed, &mut stdout, &self_agent_id).await {
                            Ok(response) => {
                                // Daemon-confirmed success path. We persist
                                // ONLY when the response carries no `error`
                                // field — a failed agent_register (e.g.
                                // UNIQUE-constraint clash) must NOT rewrite
                                // the on-disk config or the file and daemon
                                // diverge silently.
                                if response.get("error").is_none() {
                                    if let Some((new_id, new_name)) = rename_target {
                                        persist_rename_if_changed(&new_id, new_name.as_deref());
                                        // Slice 4 of cli-to-cli-routing (architect F-1
                                        // + security pre-review SEC-5): re-route the
                                        // bridge's `agent:<id>` inbox subscription
                                        // when the id changes. Order matters —
                                        // SEC-5 mandates subscribe(new) BEFORE
                                        // unsubscribe(old) so there is never a
                                        // zero-subscription window during which
                                        // another agent's `agent_send(to=new_id)`
                                        // would publish to no subscribers. Worst
                                        // case is a transient double-subscription
                                        // (harmless — old_id's row was just
                                        // renamed in agent_registry, so peer
                                        // senders targeting old_id fail the
                                        // alive-check anyway).
                                        let old_id = self_agent_id.clone();
                                        if !new_id.trim().is_empty() && new_id != old_id {
                                            if let Some(d) = daemon.as_ref() {
                                                rename_resubscribe_agent_inbox(
                                                    d, &old_id, &new_id,
                                                )
                                                .await;
                                            }
                                            self_agent_id = new_id;
                                        }
                                    }
                                }
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
                    "prompts/list" => {
                        // H3 — Claude Code 2.1.144 may probe prompts/list
                        // after init. We don't ship prompts; return empty.
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "prompts": [] }
                        });
                        write_mcp_line(&mut stdout, &resp).await?;
                    }
                    "resources/list" => {
                        // H3 — same idea, return empty resources list.
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "resources": [] }
                        });
                        write_mcp_line(&mut stdout, &resp).await?;
                    }
                    "resources/templates/list" => {
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "resourceTemplates": [] }
                        });
                        write_mcp_line(&mut stdout, &resp).await?;
                    }
                    "logging/setLevel" => {
                        // Accept any level; we don't actually adjust our
                        // tracing filter based on this. Just ack.
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {}
                        });
                        write_mcp_line(&mut stdout, &resp).await?;
                    }
                    "ping" => {
                        // MCP ping: empty result.
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {}
                        });
                        write_mcp_line(&mut stdout, &resp).await?;
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

/// Identity used by `autoregister_self` and by post-rename persistence.
/// `agent_id` is the daemon-side registry key (used by `/switch <id>` and
/// `agent_register`); `name` is the human-friendly display field.
struct AgentIdentity {
    agent_id: String,
    name: String,
}

/// Derive this CLI's identity at bridge startup. Priority order:
///
///   1. `<cwd>/.claudebase/config.json` if it exists — operator-vision
///      per-project persistence (2026-06-03/04). The file is written by
///      `claudebase run`'s `ensure_project_config` AND rewritten by the
///      `tools/call` rename interception below when the user renames
///      via the `agent_register` MCP tool. Bridge never CREATES this
///      file — only reads it — so a CC session launched from a random
///      cwd (e.g. desktop shortcut) does not pollute that cwd.
///   2. cwd basename — stable across CC restarts from the same cwd
///      even without a config file (matches the 2026-06-03 initial
///      bridge self-bootstrap behaviour).
///   3. UUID v4 — last-resort fallback when cwd is undetermined.
fn derive_identity() -> AgentIdentity {
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(cfg) = crate::project_config::load(&cwd) {
            return AgentIdentity {
                agent_id: cfg.session_id,
                name: cfg.name,
            };
        }
        if let Some(name) = cwd.file_name() {
            let name_str = name.to_string_lossy().trim().to_string();
            if !name_str.is_empty() {
                return AgentIdentity {
                    agent_id: name_str.clone(),
                    name: name_str,
                };
            }
        }
    }
    let uuid = uuid::Uuid::new_v4().to_string();
    AgentIdentity {
        agent_id: uuid.clone(),
        name: uuid,
    }
}

/// Run BOTH self-bootstrap steps in their canonical order:
///
///   1. `autoregister_self` — `agent_register(agent_id, name)` so this
///      CLI shows up in `/agents` and `/switch <id>` can bind chats to
///      it without the operator calling agent_register by hand.
///   2. `autosubscribe_from_access` — `chat_subscribe` per Telegram
///      thread paired in access.json so notifications flow without a
///      manual chat_subscribe call.
///
/// Register first: by the time daemon broadcasts a frame after a
/// /switch we already have a stamped alive row. Called both at INITIAL
/// connect AND on every successful `try_reconnect` — without the
/// reconnect re-fire, a daemon bounce silently drops subscription state
/// because the new connection has no chat_subscribe registrations
/// (operator request 2026-06-04, closes the bridge-bootstrap-on-
/// reconnect followup parked end of 2026-06-03 session).
async fn bootstrap_after_connect(daemon: &DaemonChannel) {
    autoregister_self(daemon).await;
    autosubscribe_from_access(daemon).await;
    // Slice 4 of cli-to-cli-routing (architect F-1) — subscribe this
    // bridge to its own `agent:<my-id>` inbox so notifications from
    // peer `agent_send(to=<my-id>)` calls reach this CC's channel
    // surface. Same identity source as `autoregister_self` above
    // (`derive_identity()`), same fire-and-forget pattern as
    // `autosubscribe_from_access`. Daemon's `chat_subscribe` handler
    // is open-subscription (see security pre-review SEC-4 — accepted
    // under single-box single-user trust); authorization for the
    // inbound traffic still lives in the bridge filter at
    // `should_relay_channel_notification` via target_agent_id match.
    autosubscribe_agent_inbox(daemon).await;
}

/// Slice 4 of cli-to-cli-routing (security pre-review SEC-5) —
/// re-route the bridge's `agent:<id>` inbox subscription when the
/// `self_agent_id` changes via `agent_register` rename. Order is
/// load-bearing: subscribe(new) MUST land at the daemon BEFORE
/// unsubscribe(old) so no zero-subscription window opens for an
/// inbound `agent_send` targeting `new_id`. Daemon processes frames
/// from one connection in send-order, so writing them in this order
/// guarantees the daemon-side dispatch order too.
async fn rename_resubscribe_agent_inbox(daemon: &DaemonChannel, old_id: &str, new_id: &str) {
    let new_thread = format!("agent:{new_id}");
    let old_thread = format!("agent:{old_id}");
    let sub_frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "bridge-rename-subscribe",
        "method": "tools/call",
        "params": {
            "name": "chat_subscribe",
            "arguments": { "thread": new_thread }
        }
    });
    let unsub_frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "bridge-rename-unsubscribe",
        "method": "tools/call",
        "params": {
            "name": "chat_unsubscribe",
            "arguments": { "thread": old_thread }
        }
    });
    let sub_body = match serde_json::to_vec(&sub_frame) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "rename-resubscribe: subscribe serialize failed");
            return;
        }
    };
    let unsub_body = match serde_json::to_vec(&unsub_frame) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "rename-resubscribe: unsubscribe serialize failed");
            return;
        }
    };
    let mut wh = daemon.write_half.lock().await;
    if let Err(e) = write_frame(&mut *wh, &sub_body).await {
        tracing::warn!(error = %e, %new_id, "rename-resubscribe: subscribe(new) failed");
        return;
    }
    if let Err(e) = write_frame(&mut *wh, &unsub_body).await {
        tracing::warn!(error = %e, %old_id, "rename-resubscribe: unsubscribe(old) failed");
        return;
    }
    tracing::info!(%old_id, %new_id, "rename-resubscribe: subscribe(new) then unsubscribe(old)");
}

async fn autosubscribe_agent_inbox(daemon: &DaemonChannel) {
    let id = derive_identity();
    if id.agent_id.trim().is_empty() {
        // Degraded mode — bridge filter falls back to "relay all" so
        // there's no inbox to subscribe to either. Caller logs.
        return;
    }
    let thread = format!("agent:{}", id.agent_id);
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "bridge-autosubscribe-agent",
        "method": "tools/call",
        "params": {
            "name": "chat_subscribe",
            "arguments": { "thread": thread }
        }
    });
    let body = match serde_json::to_vec(&frame) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "autosubscribe_agent_inbox: serialize failed");
            return;
        }
    };
    let mut wh = daemon.write_half.lock().await;
    if let Err(e) = write_frame(&mut *wh, &body).await {
        tracing::warn!(error = %e, agent_id = %id.agent_id, "autosubscribe_agent_inbox: write_frame failed");
        return;
    }
    tracing::info!(agent_id = %id.agent_id, thread = %thread, "subscribed to agent inbox");
}

/// Fire-and-forget auto-register: announce this CLI as alive in the
/// daemon's agent_registry so the operator's `/switch <agent_id>` and
/// `/agents` commands can find it. Without this, every CC session would
/// require a manual `agent_register` tool call before the routing-key
/// flow works — the gap operator hit on 2026-06-03 when /agents reported
/// "No CLIs are bound" until manual re-registration. Fire-and-forget —
/// response dropped by the idle drain (matches `autosubscribe_from_access`
/// discipline).
async fn autoregister_self(daemon: &DaemonChannel) {
    let id = derive_identity();
    // Slice 3 of cli-to-cli-routing — pass the bridge's cwd to the
    // daemon so register-time identity capture (project_id / branch /
    // working_dir) can resolve git context on the daemon side. When
    // the bridge's cwd is unavailable (rare; e.g. process spawned
    // without a cwd), the arg is omitted and the daemon leaves the v6
    // columns NULL (backward compat).
    let cwd_arg = std::env::current_dir()
        .ok()
        .map(|p| serde_json::Value::String(p.to_string_lossy().into_owned()))
        .unwrap_or(serde_json::Value::Null);
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "bridge-autoregister",
        "method": "tools/call",
        "params": {
            "name": "agent_register",
            "arguments": {
                "agent_id": id.agent_id,
                "name": id.name,
                "cwd": cwd_arg,
            }
        }
    });
    let body = match serde_json::to_vec(&frame) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "autoregister: serialize failed");
            return;
        }
    };
    let mut wh = daemon.write_half.lock().await;
    if let Err(e) = write_frame(&mut *wh, &body).await {
        tracing::warn!(error = %e, agent_id = %id.agent_id, "autoregister: write_frame failed");
        return;
    }
    tracing::info!(agent_id = %id.agent_id, name = %id.name, "autoregister: sent agent_register");
}

/// Decide whether a daemon-pushed notification frame should be relayed
/// to the CC stdout. Filters `notifications/claude/channel` frames
/// whose `meta.target_agent_id` names a DIFFERENT CLI — those are
/// addressed to another bridge subscribed to the same thread and
/// must NOT surface in this CC (operator report 2026-06-04: "should
/// have gone only to fbscout, not both"). All other frame methods
/// (e.g. `tools/list_changed`, `chat_subscribe` acks) pass through
/// unfiltered.
///
/// Degraded-mode discipline: when `self_agent_id` is empty (e.g. the
/// bridge could not derive its own identity at startup) the filter
/// is bypassed and every frame is relayed. The trade-off is "loud
/// over silent" — better to surface possibly-mis-addressed events
/// than to silently swallow operator's traffic.
///
/// When `meta.target_agent_id` is ABSENT, the frame is treated as
/// broadcast-to-all (no addressee filter) and relayed unconditionally.
fn should_relay_channel_notification(frame: &Value, self_agent_id: &str) -> bool {
    // Non-channel notifications pass through unchanged.
    let is_channel = frame
        .get("method")
        .and_then(|v| v.as_str())
        .map(|m| m == "notifications/claude/channel")
        .unwrap_or(false);
    if !is_channel {
        return true;
    }
    // Empty self-id ⇒ degraded mode (relay everything).
    if self_agent_id.trim().is_empty() {
        return true;
    }
    // Frame is a channel notification — inspect addressee.
    let target = frame
        .pointer("/params/meta/target_agent_id")
        .and_then(|v| v.as_str());
    match target {
        None => true,                       // unaddressed broadcast
        Some(t) => t == self_agent_id,      // addressed → relay iff it's us
    }
}

/// Persist an `agent_register` rename back to `<cwd>/.claudebase/config.json`
/// so the next CC restart from the same dir reuses the new id. Called from
/// the `tools/call` handler after the daemon has confirmed the rename
/// landed (we never rewrite the file on a failed rename, otherwise the
/// file and the daemon would diverge — the named drift this slice is
/// designed to prevent). Best-effort: errors are logged and swallowed,
/// they never break the user-facing tool response.
fn persist_rename_if_changed(new_agent_id: &str, new_name: Option<&str>) {
    if new_agent_id.trim().is_empty() {
        return;
    }
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "persist_rename: cwd unavailable");
            return;
        }
    };
    // Only rewrite when the file ALREADY exists. The bridge does not
    // create the file from random cwds — that contract is owned by
    // `claudebase run`. If the file isn't there, the user is in a
    // CC session that was launched outside `claudebase run` and the
    // rename is daemon-only (next CC restart will get a fresh id).
    if crate::project_config::load(&cwd).is_none() {
        return;
    }
    if let Err(e) = crate::project_config::write_session_id(&cwd, new_agent_id, new_name) {
        tracing::warn!(
            error = %e,
            new_agent_id = %new_agent_id,
            "persist_rename: write_session_id failed"
        );
        return;
    }
    tracing::info!(
        new_agent_id = %new_agent_id,
        new_name = ?new_name,
        "persist_rename: wrote new session_id to .claudebase/config.json"
    );
}

/// Fire-and-forget auto-subscribe: after the daemon link is up, read
/// `access.json` and issue a `chat_subscribe` for every Telegram thread
/// the operator has paired (DM senders + configured group IDs). This
/// closes the gap where notifications are dropped because no subscriber
/// exists for a thread until Mira explicitly calls chat_subscribe.
///
/// "Fire-and-forget" — we don't wait for responses. Each call carries
/// a unique `bridge-autosub-<n>` id; the main loop's idle drain catches
/// the daemon's response and discards it with a warn. The subscription
/// registration happens daemon-side immediately on receipt, so backlog
/// drain + bus pump start before this helper returns. If the access
/// file is missing or empty, the helper is a no-op.
async fn autosubscribe_from_access(daemon: &DaemonChannel) {
    use crate::daemon::channel_state::{access_json_path, load_access};

    let path = access_json_path();
    let access = match load_access(&path) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "autosubscribe: load_access failed; skipping");
            return;
        }
    };

    let mut threads: Vec<String> = Vec::new();
    for sender_id in &access.allow_from {
        threads.push(format!("telegram:{}", sender_id));
    }
    for group_id in access.groups.keys() {
        threads.push(format!("telegram:{}", group_id));
    }

    if threads.is_empty() {
        tracing::info!("autosubscribe: access.json has no paired senders / groups; skipping");
        return;
    }

    for (idx, thread) in threads.iter().enumerate() {
        let id = format!("bridge-autosub-{}", idx);
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "chat_subscribe",
                "arguments": { "thread": thread },
            }
        });
        let body = match serde_json::to_vec(&frame) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "autosubscribe: serialize failed");
                continue;
            }
        };
        let mut wh = daemon.write_half.lock().await;
        if let Err(e) = write_frame(&mut *wh, &body).await {
            tracing::warn!(error = %e, thread = %thread, "autosubscribe: write_frame failed");
            return;
        }
        tracing::info!(thread = %thread, "autosubscribe: sent chat_subscribe");
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
    self_agent_id: &str,
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
        // Notifications (no `id`) → relay to stdout subject to the
        // target_agent_id filter (mirrors the idle-drain branch in
        // `run()`), keep reading.
        if frame.get("id").is_none() {
            if should_relay_channel_notification(&frame, self_agent_id) {
                write_mcp_line(stdout, &frame).await?;
            }
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
    // Slice 7.x diagnostic — every stdout line plugin sends to Claude Code.
    // Env-gated via CLAUDEBASE_PLUGIN_TRACE=1. trace_line is defined locally
    // in run_bridge; we inline a minimal copy here since this helper is
    // module-level and doesn't have access to the closure.
    if std::env::var("CLAUDEBASE_PLUGIN_TRACE").as_deref() == Ok("1") {
        use std::io::Write as _;
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/claudebase-plugin-trace.log")
        {
            let body = String::from_utf8_lossy(&line);
            let _ = writeln!(
                log,
                "[{}] pid={} PLUGIN→STDOUT bytes={} body={}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0),
                std::process::id(),
                line.len(),
                body
            );
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn channel_frame(target: Option<&str>) -> Value {
        let mut meta = serde_json::Map::new();
        meta.insert("chat_id".into(), json!("8791871989"));
        if let Some(t) = target {
            meta.insert("target_agent_id".into(), json!(t));
        }
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {"content": "hi", "meta": meta}
        })
    }

    #[test]
    fn relay_channel_with_matching_target_id() {
        let frame = channel_frame(Some("mira"));
        assert!(should_relay_channel_notification(&frame, "mira"));
    }

    #[test]
    fn drop_channel_when_target_id_is_different() {
        let frame = channel_frame(Some("fbscout"));
        assert!(!should_relay_channel_notification(&frame, "mira"));
    }

    #[test]
    fn relay_channel_when_target_id_absent() {
        let frame = channel_frame(None);
        assert!(should_relay_channel_notification(&frame, "mira"));
    }

    #[test]
    fn relay_non_channel_notifications_unconditionally() {
        let other = json!({
            "jsonrpc": "2.0",
            "method": "notifications/tools/list_changed"
        });
        assert!(should_relay_channel_notification(&other, "mira"));
        assert!(should_relay_channel_notification(&other, "fbscout"));
        assert!(should_relay_channel_notification(&other, ""));
    }

    #[test]
    fn degraded_mode_relays_when_self_agent_id_is_empty() {
        // Empty self-id: degrade to "relay all" so a misconfigured
        // bridge never silently swallows operator traffic.
        let addressed = channel_frame(Some("mira"));
        assert!(should_relay_channel_notification(&addressed, ""));
        let addressed_other = channel_frame(Some("fbscout"));
        assert!(should_relay_channel_notification(&addressed_other, ""));
        let unaddressed = channel_frame(None);
        assert!(should_relay_channel_notification(&unaddressed, ""));
    }

    #[test]
    fn whitespace_self_id_is_treated_as_empty() {
        let addressed = channel_frame(Some("mira"));
        // Whitespace-only id treated same as empty per is_empty check
        // on trimmed value — degraded "relay all" branch.
        assert!(should_relay_channel_notification(&addressed, "   "));
    }
}
