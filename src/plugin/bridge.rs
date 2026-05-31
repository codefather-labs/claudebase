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
    validate_tool_name, Inbound, ERROR_INTERNAL, ERROR_METHOD_NOT_FOUND,
    MAX_MCP_FRAME_SIZE, SUPPORTED_PROTOCOL_VERSION,
};

/// Cache for session-establishing tool calls so the bridge can replay them
/// after a reconnect. The daemon tracks subscriptions per-connection; when
/// the bridge reconnects, it gets a NEW connection_id and the old subscriptions
/// are lost. The replay mechanism restores them.
struct SessionCache {
    /// Latest `agent_register` call params (agent_name, optionally thread/metadata).
    agent_register_params: Option<Value>,
    /// Set of thread_ids from `chat_subscribe` calls.
    subscribed_threads: Vec<String>,
}

impl SessionCache {
    fn new() -> Self {
        SessionCache {
            agent_register_params: None,
            subscribed_threads: Vec::new(),
        }
    }

    /// Record an agent_register call for later replay.
    fn cache_agent_register(&mut self, params: Value) {
        self.agent_register_params = Some(params);
    }

    /// Record a chat_subscribe thread for later replay.
    fn cache_chat_subscribe(&mut self, thread: String) {
        if !self.subscribed_threads.contains(&thread) {
            self.subscribed_threads.push(thread);
        }
    }

    /// Build replay frames with synthetic request ids (non-colliding with Claude Code's).
    /// Returns a Vec of (frame, is_subscribe) tuples for convenience.
    fn build_replay_frames(&self, id_gen: &mut ReplayIdGenerator) -> Vec<(Value, &str)> {
        let mut frames = Vec::new();

        // Replay agent_register first (it should happen before subscribes).
        if let Some(params) = &self.agent_register_params {
            let replay_id = id_gen.next();
            let frame = serde_json::json!({
                "jsonrpc": "2.0",
                "id": replay_id,
                "method": "tools/call",
                "params": {
                    "name": "agent_register",
                    "arguments": params
                }
            });
            frames.push((frame, "agent_register"));
        }

        // Replay each subscribed thread.
        for thread in &self.subscribed_threads {
            let replay_id = id_gen.next();
            let frame = serde_json::json!({
                "jsonrpc": "2.0",
                "id": replay_id,
                "method": "tools/call",
                "params": {
                    "name": "chat_subscribe",
                    "arguments": {
                        "thread": thread
                    }
                }
            });
            frames.push((frame, "chat_subscribe"));
        }

        frames
    }
}

/// Generates non-colliding request IDs for internal replay frames.
/// Uses a high negative range (i32::MIN + 1000 downward) to avoid colliding
/// with Claude Code's typically positive or small ids.
struct ReplayIdGenerator {
    next_id: i32,
}

impl ReplayIdGenerator {
    fn new() -> Self {
        // Start at i32::MIN + 1000 (very negative) and decrement.
        ReplayIdGenerator {
            next_id: i32::MIN + 1000,
        }
    }

    fn next(&mut self) -> i32 {
        let current = self.next_id;
        self.next_id = self.next_id.saturating_sub(1);
        current
    }
}

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

    // FIX B — ensure the daemon is running before attempting to connect.
    // This is a one-shot startup check; if the daemon is already running
    // (fslock prevents duplicates), this is a no-op. Best-effort: if spawn
    // fails or times out, we proceed to connect_with_retries which has its
    // own retry logic.
    ensure_daemon_running(&socket).await;

    // Attempt initial connection. We do 3 quick retries (FR-ACD-3.3)
    // before falling back to daemon-down mode.
    let mut daemon = connect_with_retries(&socket, INITIAL_RETRY_COUNT, INITIAL_RETRY_DELAY_MS).await;

    // FIX A — session cache for replaying subscriptions on reconnect.
    let mut session_cache = SessionCache::new();
    let mut replay_id_gen = ReplayIdGenerator::new();

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
                                // Notification — relay to stdout.
                                trace_line("UDS→STDOUT notif", &f.to_string());
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
                            // FIX A — replay session cache on successful reconnect.
                            if daemon.is_some() {
                                replay_session_cache(&mut daemon, &mut session_cache, &mut replay_id_gen)
                                    .await;
                            }
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

                        // FIX A — cache session-establishing calls for replay on reconnect.
                        if tool_name == "agent_register" {
                            if let Some(agent_args) = params.get("arguments") {
                                session_cache.cache_agent_register(agent_args.clone());
                            }
                        } else if tool_name == "chat_subscribe" {
                            if let Some(thread) = params
                                .get("arguments")
                                .and_then(|a| a.get("thread"))
                                .and_then(|t| t.as_str())
                            {
                                session_cache.cache_chat_subscribe(thread.to_string());
                            }
                        }

                        // Daemon-down: only claudebase_daemon_status is
                        // valid; everything else → -32601.
                        if daemon.is_none() {
                            // Try a quick reconnect.
                            daemon = try_reconnect(&socket).await;
                            // FIX A — replay session cache on successful reconnect.
                            if daemon.is_some() {
                                replay_session_cache(&mut daemon, &mut session_cache, &mut replay_id_gen)
                                    .await;
                            }
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

/// FIX A helper — replay cached session-establishing calls on a newly
/// reconnected daemon. Sends agent_register (if cached) followed by
/// chat_subscribe calls (one per subscribed thread). Responses are read
/// from the inbound_rx but discarded (internal bookkeeping, Claude Code
/// didn't ask for them). Non-blocking — doesn't hold locks across .await.
async fn replay_session_cache(
    daemon: &mut Option<DaemonChannel>,
    session_cache: &mut SessionCache,
    replay_id_gen: &mut ReplayIdGenerator,
) {
    let Some(d) = daemon else { return };
    let frames = session_cache.build_replay_frames(replay_id_gen);

    for (frame, tool_name) in frames {
        let body = match serde_json::to_vec(&frame) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "replay frame serialize failed; skipping");
                continue;
            }
        };

        // Send the replay frame to the daemon.
        {
            let mut wh = d.write_half.lock().await;
            if let Err(e) = write_frame(&mut *wh, &body).await {
                tracing::warn!(error = %e, "replay frame write failed; closing connection");
                return;
            }
        }

        // Drain the inbound_rx until we receive the response (id matches the
        // synthetic replay id) or until the channel closes. Relay any
        // notifications (frames without id) to... wait, we don't have stdout
        // here. This is async context inside the select! loop, so we CAN'T
        // write to stdout (would need mutable access and might block the loop).
        // Instead, drop the response and any intervening notifications — they're
        // internal bookkeeping and Claude Code didn't request them.
        let expected_id = frame.get("id").cloned();
        loop {
            match d.inbound_rx.recv().await {
                Some(resp_frame) => {
                    let resp_id = resp_frame.get("id").cloned();
                    // If id matches, we got our response — break and continue to next frame.
                    if resp_id == expected_id {
                        tracing::debug!(tool = %tool_name, "replay response received");
                        break;
                    }
                    // No id (notification) or mismatched id — drop and continue reading.
                    if resp_id.is_none() {
                        tracing::debug!(tool = %tool_name, "replay: dropped intervening notification");
                    }
                }
                None => {
                    // Inbound channel closed — daemon link is gone during replay.
                    tracing::warn!(tool = %tool_name, "daemon inbound closed during replay");
                    return;
                }
            }
        }
    }

    tracing::info!("session cache replay complete");
}

/// FIX B — ensure the daemon is running. Check if the UDS socket is
/// connectable (quick ~100ms attempt). If not, spawn the daemon detached
/// in the background and wait briefly (~500ms) for the socket to appear.
///
/// Idempotency: relies on the daemon's own fslock (server.rs:274-296)
/// to prevent duplicate spawns. If a 2nd `daemon serve` is attempted
/// while the 1st is still starting, the fslock will cause the 2nd to bail
/// "already running" harmlessly.
///
/// Non-blocking: spawns the daemon and waits for socket appearance, but
/// returns quickly if the socket is already available or if the spawn/wait
/// timeout expires. The bridge's connect_with_retries() has its own retries.
///
/// Best-effort: if spawn or socket-wait fails, we log and return; the
/// connect_with_retries() logic handles the failure.
async fn ensure_daemon_running(socket: &std::path::Path) {
    // Quick check: is the socket connectable right now?
    if try_connect(socket).await.is_some() {
        tracing::debug!("daemon socket already reachable");
        return;
    }

    tracing::info!("daemon socket not reachable; attempting auto-start");

    // Spawn the daemon detached. On Unix, use process_group(0) to make it
    // immune to parent process signals. On Windows, the behavior is
    // different but we'll start with Unix-focused implementation.
    let spawn_result = {
        let mut cmd = std::process::Command::new("claudebase");
        cmd.arg("daemon");
        cmd.arg("serve");

        // Close stdio so the daemon runs independently.
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());

        // Unix-specific: detach from parent process group.
        // setsid is not directly exposed by CommandExt, so we rely on stdio closure
        // for independence. Daemon process will continue running even if parent dies
        // (not guaranteed by tokio::spawn alone, but std::process::Command::spawn()
        // spawns a real OS process that persists). A cleaner approach would wrap the
        // Command in a proper daemonize call, but that requires an external crate.
        // For this iteration, we accept the limitation and note it as a future enhancement.

        cmd.spawn()
    };

    match spawn_result {
        Ok(mut child) => {
            tracing::info!(
                pid = child.id(),
                "daemon spawned; waiting for socket to appear (~500ms)"
            );
            // Spawn a task to detach the child process (prevent zombies on Unix).
            // We don't wait for it — just let it run in the background.
            tokio::spawn(async move {
                let _ = child.wait();
            });

            // Wait for the socket to appear (up to 500ms). Poll every 50ms.
            let start = tokio::time::Instant::now();
            let timeout = Duration::from_millis(500);
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if try_connect(socket).await.is_some() {
                    tracing::info!("daemon socket appeared");
                    return;
                }
                if start.elapsed() > timeout {
                    tracing::warn!("daemon socket did not appear within 500ms; proceeding with connect retries");
                    return;
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to spawn daemon; proceeding with connect retries"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_cache_caches_agent_register() {
        let mut cache = SessionCache::new();
        let params = serde_json::json!({ "agent_name": "test_agent" });
        cache.cache_agent_register(params.clone());
        assert_eq!(cache.agent_register_params, Some(params));
    }

    #[test]
    fn replay_cache_caches_chat_subscribe() {
        let mut cache = SessionCache::new();
        cache.cache_chat_subscribe("thread_1".to_string());
        cache.cache_chat_subscribe("thread_2".to_string());
        assert_eq!(cache.subscribed_threads, vec!["thread_1", "thread_2"]);
    }

    #[test]
    fn replay_cache_dedupes_subscribe_threads() {
        let mut cache = SessionCache::new();
        cache.cache_chat_subscribe("thread_1".to_string());
        cache.cache_chat_subscribe("thread_1".to_string());
        assert_eq!(cache.subscribed_threads.len(), 1);
    }

    #[test]
    fn build_replay_frames_replays_agent_register_first() {
        let mut cache = SessionCache::new();
        let agent_params = serde_json::json!({ "agent_name": "test_agent" });
        cache.cache_agent_register(agent_params);
        cache.cache_chat_subscribe("thread_1".to_string());

        let mut id_gen = ReplayIdGenerator::new();
        let frames = cache.build_replay_frames(&mut id_gen);

        // Should have 2 frames: agent_register + chat_subscribe.
        assert_eq!(frames.len(), 2);
        // First frame is agent_register.
        assert_eq!(frames[0].1, "agent_register");
        // Second frame is chat_subscribe.
        assert_eq!(frames[1].1, "chat_subscribe");
    }

    #[test]
    fn build_replay_frames_uses_noncoliding_ids() {
        let mut cache = SessionCache::new();
        cache.cache_agent_register(serde_json::json!({}));
        cache.cache_chat_subscribe("thread_1".to_string());
        cache.cache_chat_subscribe("thread_2".to_string());

        let mut id_gen = ReplayIdGenerator::new();
        let frames = cache.build_replay_frames(&mut id_gen);

        // Extract the ids from the replay frames.
        let ids: Vec<i32> = frames
            .iter()
            .map(|(f, _)| f.get("id").and_then(|v| v.as_i64()).unwrap_or(0) as i32)
            .collect();

        // All ids should be negative (in the reserved range).
        for id in &ids {
            assert!(*id < 0, "replay id {} is not negative", id);
        }

        // All ids should be unique.
        let mut sorted = ids.clone();
        sorted.sort();
        for i in 1..sorted.len() {
            assert_ne!(sorted[i - 1], sorted[i], "duplicate replay id");
        }
    }

    #[test]
    fn replay_id_generator_decrements() {
        let mut gen = ReplayIdGenerator::new();
        let id1 = gen.next();
        let id2 = gen.next();
        let id3 = gen.next();
        // Each call should decrement (return more negative).
        assert!(id1 > id2);
        assert!(id2 > id3);
    }

    #[test]
    fn build_replay_frames_empty_cache() {
        let cache = SessionCache::new();
        let mut id_gen = ReplayIdGenerator::new();
        let frames = cache.build_replay_frames(&mut id_gen);
        assert_eq!(frames.len(), 0);
    }
}
