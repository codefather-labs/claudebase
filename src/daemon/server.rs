//! UDS / named-pipe server for the claudebase daemon (Slice 1a).
//!
//! Accepts concurrent connections on a Unix domain socket (or Windows
//! named pipe) and replies to every length-prefixed JSON frame with a
//! hard-coded `{"pong": <ping>}` echo. Slice 1b layers the MCP plugin
//! bridge on top of this primitive; Slice 1c adds the broadcast bus.
//!
//! Concurrency primitives:
//! - `tokio::spawn` per accepted connection (one task per client).
//! - `Arc<Semaphore>` (64 permits) gates accept-storms — when 64 tasks
//!   are in-flight the listener back-pressures by blocking on
//!   `acquire_owned()` before pulling the next accept.
//! - `fslock` on `daemon.pid` for single-instance enforcement; lock is
//!   process-scoped and released automatically on exit (including
//!   SIGKILL — the kernel drops the OFD lock).
//!
//! Per-connection state is minimal in Slice 1a — a UUID for log
//! correlation. Slice 1b will attach subscription state for the
//! broadcast bus.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use interprocess::local_socket::tokio::{prelude::*, Stream};
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::cli::DaemonServeArgs;
use crate::daemon::chat::{self, ChatBus, SharedBus};
use crate::daemon::ipc::{read_frame, write_frame};

/// Max concurrent in-flight connections. Hitting this cap back-pressures
/// the accept loop (the listener blocks on `acquire_owned()` until a
/// task drops its permit). 64 is generous for a local-only daemon —
/// Claude Code rarely runs more than a dozen MCP plugins simultaneously.
const ACCEPT_STORM_LIMIT: usize = 64;

/// Compute the daemon's parent directory:
/// - Unix: `$XDG_RUNTIME_DIR/claudebase/` (falls back to `/tmp/claudebase-<uid>/`
///   when XDG_RUNTIME_DIR is unset, matching the systemd convention).
/// - Windows: `$LOCALAPPDATA\claudebase\` (always set by the OS).
pub fn parent_dir() -> anyhow::Result<PathBuf> {
    #[cfg(unix)]
    {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            if !xdg.is_empty() {
                return Ok(PathBuf::from(xdg).join("claudebase"));
            }
        }
        // Fallback when XDG_RUNTIME_DIR is unset (Darwin / minimal Linux setups).
        // Match systemd's per-uid convention so the dir is unambiguously the
        // current user's runtime area.
        let uid = unsafe { libc_getuid() };
        Ok(PathBuf::from(format!("/tmp/claudebase-{uid}")))
    }
    #[cfg(windows)]
    {
        let local = std::env::var("LOCALAPPDATA")
            .context("LOCALAPPDATA env var missing — required to locate daemon dir on Windows")?;
        Ok(PathBuf::from(local).join("claudebase"))
    }
}

#[cfg(unix)]
#[allow(non_snake_case)]
unsafe fn libc_getuid() -> u32 {
    // Avoid pulling the full `libc` crate just for getuid — link the
    // libc symbol directly. Safe: getuid() has no preconditions and
    // never fails.
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

/// Compute the UDS / named-pipe socket path.
pub fn socket_path() -> anyhow::Result<PathBuf> {
    let dir = parent_dir()?;
    #[cfg(unix)]
    {
        Ok(dir.join("daemon.sock"))
    }
    #[cfg(windows)]
    {
        // Windows named pipes live in the special `\\.\pipe\` namespace,
        // not the filesystem. The parent dir is still used for the PID
        // file. The "name" here doubles as the path-like identifier
        // accepted by `to_fs_name::<GenericFilePath>()`.
        let _ = dir; // parent dir reserved for pid file
        Ok(PathBuf::from(r"\\.\pipe\claudebase-daemon"))
    }
}

/// Compute the PID file path.
pub fn pid_file_path() -> anyhow::Result<PathBuf> {
    Ok(parent_dir()?.join("daemon.pid"))
}

/// Create parent dir at 0o700 (Unix) or default ACL (Windows).
fn ensure_parent_dir(parent: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create daemon parent dir at {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to set 0o700 permissions on daemon parent dir at {}",
                parent.display()
            )
        })?;
    }
    Ok(())
}

/// Reap-on-boot stub. The full implementation lands in Slice 5 when the
/// `agent_registry` table exists. For now we open chat.db (creating it
/// if absent), probe `sqlite_master` for the table, and either skip
/// silently or run the bulk-UPDATE — protected by the explicit existence
/// check so we never get a "no such table" runtime error.
fn reap_on_boot_stub() -> anyhow::Result<()> {
    // chat.db lives under ~/.claude/knowledge/ — independent of the
    // daemon runtime dir. Best-effort: if HOME is unset (extremely
    // unusual), skip rather than fail daemon startup.
    let home = match std::env::var_os("HOME") {
        Some(h) => h,
        None => return Ok(()),
    };
    let chat_db = PathBuf::from(home).join(".claude/knowledge/chat.db");

    // Ensure the directory exists so OpenFlags::SQLITE_OPEN_CREATE can
    // create the file. Failure to create the parent dir is non-fatal in
    // the stub — Slice 5 will harden this.
    if let Some(parent) = chat_db.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let conn = match rusqlite::Connection::open_with_flags(
        &chat_db,
        rusqlite::OpenFlags::SQLITE_OPEN_CREATE | rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "chat.db open failed (non-fatal)");
            return Ok(());
        }
    };

    // Slice 3: apply chat schema v5 BEFORE the agent_registry probe so
    // the chat tools have their tables on first daemon startup. The
    // schema is idempotent (CREATE TABLE IF NOT EXISTS + INSERT OR
    // IGNORE) so re-runs across daemon restarts are safe.
    if let Err(e) = chat::ensure_chat_db_schema(&conn) {
        tracing::warn!(error = %e, "chat schema v5 migration failed (non-fatal)");
        // Don't return — the agent_registry probe is independent and may
        // still succeed; the daemon as a whole should not refuse to start
        // because schema-application hiccupped.
    }

    // Probe sqlite_master rather than catching a "no such table" error —
    // architect directive: explicit existence check, not error-catch.
    let table_exists: i64 = match conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_registry'",
        [],
        |row| row.get(0),
    ) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "agent_registry probe failed (non-fatal)");
            return Ok(());
        }
    };

    if table_exists == 0 {
        // Expected during Slice 1a — table is added in Slice 5.
        // TODO(Slice 5): real reap-on-boot once agent_registry exists.
        return Ok(());
    }

    // Table exists (some future slice ran). Reap stale rows.
    if let Err(e) = conn.execute(
        "UPDATE agent_registry SET state='orphaned' WHERE state='alive'",
        [],
    ) {
        tracing::warn!(error = %e, "reap-on-boot UPDATE failed (non-fatal)");
    }
    Ok(())
}

/// Serve the daemon. Returns Ok(()) on graceful shutdown (currently
/// never — Slice 1d adds the SIGTERM handler that closes the listener
/// and returns cleanly). Errors propagate up to `main.rs` and exit 1.
pub async fn serve(_args: &DaemonServeArgs) -> anyhow::Result<()> {
    // Slice 4 — Telegram secrets perm-check FIRST, before ANY other I/O.
    // This must precede the parent_dir + fslock acquire so it can refuse
    // a bad-perm secrets.toml within ~100ms of process start (TC-4.14
    // sleeps only 1 second before checking try_wait()). The check uses
    // symlink_metadata (lstat) + mode-mask — no other side effects, so a
    // failed check is the fastest possible exit path.
    //
    // Using symlink_metadata (lstat) prior to file open is the
    // load-bearing TOCTOU mitigation against `ln -s /etc/whatever
    // ~/.config/claudebase/secrets.toml` confusion attacks. The literal
    // "must have permissions 0600" stderr is required by TC-4.14.
    use crate::daemon::config;
    let secrets_path = config::user_level_secrets_toml_path();
    let telegram_token_opt: Option<config::RedactedToken> =
        match std::fs::symlink_metadata(&secrets_path) {
            Ok(_) => match config::load_secrets_toml(&secrets_path) {
                Ok(s) => Some(s.telegram.bot_token),
                Err(e) => {
                    // SEC-9: print the literal failure to stderr and
                    // exit 1. We use eprintln! directly so the message
                    // lands on stderr even before init_tracing — TC-4.14
                    // captures process stderr.
                    eprintln!("error: {e}");
                    anyhow::bail!("secrets.toml load failed");
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                eprintln!("error: failed to stat secrets.toml: {e}");
                anyhow::bail!("secrets.toml stat failed");
            }
        };

    // SEC-15: also validate daemon.toml if present (symlink + no
    // bot_token field). Skip silently if absent.
    let daemon_toml_path = config::user_level_daemon_toml_path();
    if daemon_toml_path.exists() {
        if let Err(e) = config::load_daemon_toml(&daemon_toml_path) {
            eprintln!("error: {e}");
            anyhow::bail!("daemon.toml load failed");
        }
    }

    let parent = parent_dir()?;
    ensure_parent_dir(&parent)?;

    let pid_path = pid_file_path()?;
    let socket = socket_path()?;

    // Acquire fslock on the PID file. `try_lock_with_pid` writes the
    // current process's PID into the lockfile on success and returns
    // false (without erroring) when the file is already locked by
    // another live process.
    let mut lock = fslock::LockFile::open(&pid_path).with_context(|| {
        format!(
            "failed to open PID lockfile at {} — check parent dir perms",
            pid_path.display()
        )
    })?;
    let acquired = lock
        .try_lock_with_pid()
        .with_context(|| format!("PID lockfile I/O error at {}", pid_path.display()))?;
    if !acquired {
        let other_pid = fs::read_to_string(&pid_path)
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        tracing::error!(other_pid = %other_pid, "claudebase daemon already running");
        // Bail with a unique error so main.rs can map this to exit code 1.
        // We rely on anyhow's Display; the exit code comes from main.rs
        // which always uses ExitCode::FAILURE on Err return.
        anyhow::bail!("already running");
    }

    // Reap-on-boot stub (Slice 1a — no-op until Slice 5 adds agent_registry).
    reap_on_boot_stub()?;

    // Best-effort: remove any stale socket file before bind. The
    // interprocess crate does NOT auto-unlink on Unix when the previous
    // process exited uncleanly (SIGKILL leaves the file). Ignore errors —
    // if the file doesn't exist or we can't remove it, the bind call
    // will surface the real error in a moment.
    #[cfg(unix)]
    {
        if socket.exists() {
            let _ = fs::remove_file(&socket);
        }
    }

    // Build the listener. `to_fs_name::<GenericFilePath>` consumes
    // its receiver (signature `fn to_fs_name(self)`), so we hand it a
    // clone and keep `socket` available for logging.
    let path_name = socket.clone().to_fs_name::<GenericFilePath>().with_context(|| {
        format!(
            "socket path is not a valid file-system name: {}",
            socket.display()
        )
    })?;
    let opts = ListenerOptions::new().name(path_name);

    // Apply 0o600 permission to the socket file. Two paths:
    //
    // (1) Linux / FreeBSD ≥ 14.3 / OpenBSD: `ListenerOptionsExt::mode()`
    //     does a pre-bind `fchmod()` on the socket fd — race-free, no
    //     umask wrangling. This is the architect's STRUCTURAL #2
    //     recommendation.
    //
    // (2) macOS (Darwin): `fchmod()` on a UDS returns EINVAL, which
    //     interprocess maps to ErrorKind::Unsupported. Fall back to
    //     umask-based mode control: save current umask, set it to
    //     `0o077` so bind() creates the socket with mode 0o600, restore
    //     after. The umask save/restore is a process-wide side effect
    //     for the duration of bind() — acceptable in Slice 1a because
    //     the daemon is single-threaded at this point (the accept loop
    //     hasn't started). Future slices that bind additional sockets
    //     concurrently would need a different approach; we'll cross
    //     that bridge if it comes up.
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
    let opts = {
        use interprocess::os::unix::local_socket::ListenerOptionsExt;
        opts.mode(0o600)
    };

    #[cfg(target_os = "macos")]
    let previous_umask = {
        extern "C" {
            fn umask(mode: u32) -> u32;
        }
        // Set umask to mask all group/other bits + execute. Result:
        // bind() creates the socket file with permission
        // 0o666 & !0o177 = 0o600.
        unsafe { umask(0o177) }
    };

    let listener_result = opts.create_tokio();

    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn umask(mode: u32) -> u32;
        }
        unsafe {
            umask(previous_umask);
        }
    }

    let listener = listener_result.with_context(|| {
        format!(
            "failed to bind UDS/named-pipe listener at {}",
            socket.display()
        )
    })?;

    // On macOS, the umask-based approach yields 0o600 — but if a
    // pre-existing socket file had different perms, we'd inherit those.
    // We removed the stale file above (best-effort), so bind freshly
    // applies our umask-restricted mode. Belt-and-suspenders: explicit
    // chmod after bind to guarantee 0o600 regardless of how the file
    // got created.
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&socket, fs::Permissions::from_mode(0o600));
    }

    tracing::info!(
        socket = %socket.display(),
        pid = std::process::id(),
        "claudebase daemon listening"
    );

    let semaphore = Arc::new(Semaphore::new(ACCEPT_STORM_LIMIT));

    // Slice 3: per-thread broadcast bus shared between every connection
    // handler. Lives for the daemon's entire lifetime.
    let bus: SharedBus = Arc::new(ChatBus::new());

    // Slice 4 — spawn the Telegram long-poll task IFF a perm-checked
    // secrets.toml is present. The spawn is fire-and-forget: ASYNC_INVARIANTS
    // Rule 3 wraps the long-poll body so a fatal Telegram error logs
    // structured (token-redacted) and the rest of the daemon keeps
    // serving MCP plugins. When secrets.toml is absent the daemon runs
    // chat-only (Slice 1-3 behaviour unchanged).
    if let Some(token) = telegram_token_opt {
        let access_path = crate::daemon::permissions::user_level_access_json_path();
        let bus_for_tg = bus.clone();

        // Slice 6-MVP — best-effort Asr construction. When daemon.toml
        // has no `[asr] backend` configured, OR the configured backend
        // isn't compiled in / is a Wave-6 stub, the daemon still runs
        // (text messages keep working) and voice notes get the
        // `[voice transcription failed: ...]` placeholder per the
        // transcribe_voice_note error path.
        let asr_opt: Option<std::sync::Arc<dyn crate::daemon::asr::Asr>> = {
            let toml_path = crate::daemon::config::user_level_daemon_toml_path();
            if toml_path.exists() {
                match crate::daemon::config::load_daemon_toml(&toml_path) {
                    Ok(cfg) => match crate::daemon::asr::make_asr(&cfg) {
                        Ok(b) => Some(std::sync::Arc::from(b)),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "ASR factory failed; voice notes will use fallback placeholder"
                            );
                            None
                        }
                    },
                    Err(e) => {
                        tracing::warn!(error = %e, "daemon.toml reload failed in server.serve");
                        None
                    }
                }
            } else {
                None
            }
        };

        let _ =
            crate::daemon::telegram::spawn_long_poll(token, access_path, bus_for_tg, asr_opt);
        tracing::info!("telegram long-poll spawned");
    }

    // Accept loop. We never return Ok(()) from here in Slice 1a — the
    // daemon runs until killed. Slice 1d will wire a SIGTERM cancel
    // signal that breaks out of this loop.
    loop {
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(e) => {
                // Semaphore was closed — programming bug, not runtime
                // condition. Treat as fatal.
                anyhow::bail!("semaphore closed unexpectedly: {e}");
            }
        };

        let stream = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                // Transient accept errors (EMFILE, ECONNABORTED) — log
                // and continue. Persistent errors would spin the loop
                // hot; future slices may add tracing-based rate limiting.
                tracing::warn!(error = %e, "accept error (continuing)");
                drop(permit);
                continue;
            }
        };

        let connection_id = Uuid::new_v4();
        tracing::info!(%connection_id, "accepted connection");

        let bus_clone = bus.clone();
        tokio::spawn(async move {
            // Rule 3 / Rule 5 from ASYNC_INVARIANTS.md: panic-safe spawned
            // task body — propagate via Result, surface via tracing::error.
            if let Err(e) = handle_connection(stream, connection_id, permit, bus_clone).await {
                tracing::error!(%connection_id, error = %e, "connection handler error");
            }
        });
    }
}

/// Per-connection outbound message. Used by both the request-dispatch
/// task (writes responses) and the broadcast-subscriber tasks
/// (writes notifications). A single writer task serialises them onto
/// the UDS so we never interleave two `write_frame` calls on the same
/// stream concurrently.
type OutboundTx = mpsc::UnboundedSender<serde_json::Value>;
type OutboundRx = mpsc::UnboundedReceiver<serde_json::Value>;

/// Handle one accepted connection: loop reading frames, dispatch each
/// to the appropriate handler, push responses + chat notifications to
/// the per-connection outbound mpsc. A single writer task drains the
/// mpsc and serialises frames onto the UDS.
///
/// `_permit` owns the semaphore slot for this connection — it is held
/// for the entire task lifetime and released on Drop, freeing the slot
/// for the next accept.
async fn handle_connection(
    stream: Stream,
    connection_id: Uuid,
    _permit: OwnedSemaphorePermit,
    bus: SharedBus,
) -> anyhow::Result<()> {
    // Split read / write halves so the writer task can run independently.
    let (mut read_half, mut write_half) = tokio::io::split(stream);

    // Outbound mpsc — unbounded because all senders are local processes
    // we own (the read loop + per-thread forwarder tasks). Bounded
    // semantics would risk deadlock if the writer task lags briefly.
    let (outbound_tx, mut outbound_rx): (OutboundTx, OutboundRx) = mpsc::unbounded_channel();

    // Writer task: drain `outbound_rx`, write_frame each value. When
    // outbound_tx and all clones drop, this loop exits cleanly.
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            let bytes = match serde_json::to_vec(&frame) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(error = %e, "serialize outbound frame failed");
                    continue;
                }
            };
            if let Err(e) = write_frame(&mut write_half, &bytes).await {
                tracing::info!(error = %e, "outbound write failed; closing connection");
                break;
            }
        }
    });

    let outcome = run_request_loop(&mut read_half, outbound_tx, bus, connection_id).await;
    // Dropping outbound_tx (and every clone held by forwarder tasks
    // is owned by tokio::spawned bodies that wake up on bus closure or
    // EOF; in practice they exit when this function returns) closes the
    // outbound mpsc, which lets writer_task exit.
    drop(read_half);
    let _ = writer_task.await;
    outcome
}

/// Inner read loop — pulls inbound frames from `read_half`, dispatches,
/// pushes outbound on `outbound_tx`. Returns Ok(()) on clean EOF.
async fn run_request_loop<R>(
    read_half: &mut R,
    outbound_tx: OutboundTx,
    bus: SharedBus,
    connection_id: Uuid,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncReadExt + Unpin,
{
    loop {
        let body = match read_frame(read_half).await {
            Ok(b) => b,
            Err(e) => {
                // Distinguish clean EOF from a real I/O error. Clean
                // EOF surfaces as `UnexpectedEof` from `read_exact` on
                // the length prefix — that's an expected client
                // disconnect, log at "info" not "error".
                if let Some(io_err) = e.downcast_ref::<io::Error>() {
                    if io_err.kind() == io::ErrorKind::UnexpectedEof {
                        tracing::info!(%connection_id, "connection EOF");
                        return Ok(());
                    }
                }
                return Err(e);
            }
        };

        // Parse the inbound frame. Slice 1b: emit JSON-RPC 2.0 Parse
        // Error envelope on malformed input (SEC-3 from Vault pre-review).
        let inbound: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(%connection_id, "malformed JSON frame (sending Parse Error)");
                let err_resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::Value::Null,
                    "error": {
                        "code": -32700,
                        "message": "Parse error"
                    }
                });
                let _ = outbound_tx.send(err_resp);
                continue;
            }
        };

        let echo_id = inbound.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = inbound
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("");

        if method == "tools/list" {
            // Slice 3: daemon-up `tools/list` returns the 4 chat tools.
            let resp = build_tools_list_response(echo_id);
            let _ = outbound_tx.send(resp);
            continue;
        }

        if method == "tools/call" {
            let params = inbound.get("params").cloned().unwrap_or(serde_json::Value::Null);
            let tool_name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            match tool_name.as_str() {
                "claudebase_daemon_status" => {
                    let resp = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": echo_id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": "{\"status\":\"up\"}"
                            }]
                        }
                    });
                    let _ = outbound_tx.send(resp);
                }
                "chat_post" | "chat_reply" => {
                    // Persist first, queue response, THEN broadcast — this
                    // ordering guarantees the response lands in the
                    // outbound mpsc before the broadcast notification so
                    // the test pattern "read response, then read
                    // notification" is preserved regardless of how the
                    // tokio scheduler interleaves the forwarder task.
                    let (resp, notif) =
                        handle_chat_post(&tool_name, echo_id, &args).await;
                    let _ = outbound_tx.send(resp);
                    if let Some((thread, frame)) = notif {
                        let _ = bus.publish(&thread, frame).await;
                    }
                }
                "chat_subscribe" => {
                    let resp = handle_chat_subscribe(
                        echo_id,
                        &args,
                        &bus,
                        outbound_tx.clone(),
                        connection_id,
                    )
                    .await;
                    let _ = outbound_tx.send(resp);
                }
                "chat_list" => {
                    let resp = handle_chat_list(echo_id, &args).await;
                    let _ = outbound_tx.send(resp);
                }
                _ => {
                    let resp = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": echo_id,
                        "error": {
                            "code": -32601,
                            "message": "Method not found"
                        }
                    });
                    let _ = outbound_tx.send(resp);
                }
            }
            continue;
        }

        // Legacy Slice 1a ping/pong echo for any non-MCP frame. The
        // smoke tests in `tests/daemon_smoke.rs` rely on this path.
        let ping_value = inbound
            .get("ping")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let response = serde_json::json!({ "pong": ping_value });
        let _ = outbound_tx.send(response);
    }
}

/// Build the `tools/list` response with the four chat tools.
fn build_tools_list_response(id: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "chat_post",
                    "description": "Post a message to a chat thread",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "thread": {"type": "string"},
                            "content": {"type": "string"},
                            "from": {"type": "string"}
                        },
                        "required": ["thread", "content", "from"]
                    }
                },
                {
                    "name": "chat_subscribe",
                    "description": "Subscribe to a chat thread; returns undelivered backlog",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "thread": {"type": "string"}
                        },
                        "required": ["thread"]
                    }
                },
                {
                    "name": "chat_reply",
                    "description": "Reply to a message in a chat thread",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "thread": {"type": "string"},
                            "content": {"type": "string"},
                            "reply_to": {"type": ["string", "null"]}
                        },
                        "required": ["thread", "content"]
                    }
                },
                {
                    "name": "chat_list",
                    "description": "List messages in a chat thread",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "thread": {"type": "string"},
                            "since": {"type": ["integer", "null"]},
                            "limit": {"type": ["integer", "null"]}
                        },
                        "required": ["thread"]
                    }
                }
            ]
        }
    })
}

/// Build a tools/call error response.
fn tool_error_response(id: serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

/// Build a tools/call success response with a text-content payload that
/// is itself a JSON string (MCP envelope shape).
fn tool_text_response(id: serde_json::Value, payload: &serde_json::Value) -> serde_json::Value {
    let text = payload.to_string();
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{
                "type": "text",
                "text": text
            }]
        }
    })
}

/// Handle `chat_post` or `chat_reply` (same code path; `chat_reply`
/// additionally accepts a `reply_to` argument and silently downgrades
/// stale references to NULL per TC-3.5).
///
/// Returns `(response, Option<(thread_id, notification_frame)>)`. The
/// caller queues the response onto the connection's outbound mpsc
/// FIRST, then publishes the broadcast notification — this ordering
/// guarantees subscriber-side observers see the response before any
/// post-induced notification, satisfying the test pattern in
/// `tests/chat_tools_e2e_test.rs:230-253`.
async fn handle_chat_post(
    tool_name: &str,
    id: serde_json::Value,
    args: &serde_json::Value,
) -> (serde_json::Value, Option<(String, serde_json::Value)>) {
    let thread = args
        .get("thread")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let from_agent = args
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let reply_to_raw = args
        .get("reply_to")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if thread.is_empty() {
        return (tool_error_response(id, -32602, "thread is required"), None);
    }

    let persisted = tokio::task::spawn_blocking(move || -> anyhow::Result<chat::ChatMessage> {
        let conn = chat::open_chat_db()?;
        let resolved_reply_to = chat::resolve_reply_to(&conn, reply_to_raw.as_deref())?;
        let msg = chat::insert_message(
            &conn,
            &thread,
            &from_agent,
            &content,
            resolved_reply_to.as_deref(),
        )?;
        Ok(msg)
    })
    .await;

    let msg = match persisted {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            tracing::error!(error = %e, tool = tool_name, "chat persist failed");
            return (tool_error_response(id, -32603, "persist failed"), None);
        }
        Err(e) => {
            tracing::error!(error = %e, tool = tool_name, "chat persist task panicked");
            return (tool_error_response(id, -32603, "internal error"), None);
        }
    };

    let response_payload = if tool_name == "chat_post" {
        serde_json::json!({
            "id": msg.id,
            "thread": msg.thread_id,
            "created_at": msg.created_at,
        })
    } else {
        serde_json::json!({
            "id": msg.id,
            "reply_to_resolved": msg.reply_to.is_some(),
        })
    };

    let notif = chat::build_channel_notification(&msg);
    let thread_id = msg.thread_id.clone();
    (
        tool_text_response(id, &response_payload),
        Some((thread_id, notif)),
    )
}

/// Handle `chat_subscribe`. Spawns a forwarding task that pumps the
/// per-thread broadcast::Receiver into the connection's outbound mpsc.
/// The forwarding task exits naturally when either the connection
/// closes (outbound_tx is dropped → send fails) or the broadcast bus
/// is dropped at daemon shutdown.
async fn handle_chat_subscribe(
    id: serde_json::Value,
    args: &serde_json::Value,
    bus: &SharedBus,
    outbound_tx: OutboundTx,
    connection_id: Uuid,
) -> serde_json::Value {
    let thread = args
        .get("thread")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if thread.is_empty() {
        return tool_error_response(id, -32602, "thread is required");
    }

    // Subscribe BEFORE draining backlog so we don't miss a message
    // posted in the gap between drain and subscribe.
    let mut rx = bus.subscribe(&thread).await;

    let thread_for_drain = thread.clone();
    let backlog: Vec<chat::ChatMessage> =
        match tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<chat::ChatMessage>> {
            let mut conn = chat::open_chat_db()?;
            let msgs = chat::drain_backlog(&mut conn, &thread_for_drain)?;
            Ok(msgs)
        })
        .await
        {
            Ok(Ok(msgs)) => msgs,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "chat backlog drain failed");
                return tool_error_response(id, -32603, "backlog drain failed");
            }
            Err(e) => {
                tracing::error!(error = %e, "chat backlog drain panicked");
                return tool_error_response(id, -32603, "internal error");
            }
        };

    let messages_json: Vec<serde_json::Value> = backlog.iter().map(|m| m.to_json()).collect();

    // Spawn the forwarding task that pumps broadcast → outbound mpsc.
    // Rule 3 / Rule 5: panic-safe + no .unwrap on runtime values.
    let outbound_clone = outbound_tx.clone();
    let thread_for_log = thread.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if outbound_clone.send(frame).is_err() {
                        // outbound mpsc closed — connection is gone.
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        %connection_id,
                        thread = %thread_for_log,
                        lagged = n,
                        "broadcast subscriber lagged; resuming"
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    let payload = serde_json::json!({
        "thread": thread,
        "messages": messages_json,
    });
    tool_text_response(id, &payload)
}

/// Handle `chat_list` — SELECT messages from chat.db for a given
/// thread, optionally bounded by `since` (created_at >) and `limit`.
async fn handle_chat_list(id: serde_json::Value, args: &serde_json::Value) -> serde_json::Value {
    let thread = args
        .get("thread")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if thread.is_empty() {
        return tool_error_response(id, -32602, "thread is required");
    }
    let since = args.get("since").and_then(|v| v.as_i64());
    let limit = args.get("limit").and_then(|v| v.as_i64());

    let result: Result<Vec<chat::ChatMessage>, anyhow::Error> =
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<chat::ChatMessage>> {
            let conn = chat::open_chat_db()?;
            let msgs = chat::list_messages(&conn, &thread, since, limit)?;
            Ok(msgs)
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("join error: {e}")));

    match result {
        Ok(messages) => {
            let messages_json: Vec<serde_json::Value> =
                messages.iter().map(|m| m.to_json()).collect();
            let payload = serde_json::json!({ "messages": messages_json });
            tool_text_response(id, &payload)
        }
        Err(e) => {
            tracing::error!(error = %e, "chat_list failed");
            tool_error_response(id, -32603, "list failed")
        }
    }
}
