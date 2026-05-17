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
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::cli::DaemonServeArgs;
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

        tokio::spawn(async move {
            // Rule 3 / Rule 5 from ASYNC_INVARIANTS.md: panic-safe spawned
            // task body — propagate via Result, surface via tracing::error.
            if let Err(e) = handle_connection(stream, connection_id, permit).await {
                tracing::error!(%connection_id, error = %e, "connection handler error");
            }
        });
    }
}

/// Handle one accepted connection: loop reading frames, echo each as
/// `{"pong": <ping>}`. Returns cleanly on EOF or read error.
///
/// `_permit` owns the semaphore slot for this connection — it is held
/// for the entire task lifetime and released on Drop, freeing the slot
/// for the next accept.
async fn handle_connection(
    mut stream: Stream,
    connection_id: Uuid,
    _permit: OwnedSemaphorePermit,
) -> anyhow::Result<()> {
    loop {
        let body = match read_frame(&mut stream).await {
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
        // Error envelope on malformed input (SEC-3 from Vault pre-review)
        // — the prior Slice 1a `{"error": "malformed JSON: ..."}` shape
        // leaked `serde_json::Error::to_string()` and was NOT JSON-RPC
        // compliant. The new shape uses `id: null` and a generic
        // "Parse error" string per the JSON-RPC 2.0 spec.
        let inbound: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                // Note: do NOT include the serde error in the response —
                // leaking parse-error internals is a data-leak risk and
                // breaks the spec's "generic Parse error" guidance.
                tracing::warn!(%connection_id, "malformed JSON frame (sending Parse Error)");
                let err_resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::Value::Null,
                    "error": {
                        "code": -32700,
                        "message": "Parse error"
                    }
                });
                let err_bytes = serde_json::to_vec(&err_resp)?;
                write_frame(&mut stream, &err_bytes).await?;
                continue;
            }
        };

        // Slice 1b: minimal MCP-shaped dispatch added so the plugin
        // bridge can proxy `tools/list` and `tools/call
        // claudebase_daemon_status` to the daemon when up. Everything
        // else still falls through to the legacy ping/pong echo so
        // existing Slice 1a smoke tests continue to pass.
        let echo_id = inbound.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = inbound
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("");

        if method == "tools/list" {
            // Daemon-up `tools/list` is empty until Slice 3 lands the
            // chat tools. The plugin's daemon-down code path returns
            // the sentinel tool independently.
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": echo_id,
                "result": { "tools": [] }
            });
            let resp_bytes = serde_json::to_vec(&resp)?;
            write_frame(&mut stream, &resp_bytes).await?;
            continue;
        }

        if method == "tools/call" {
            let tool_name = inbound
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            if tool_name == "claudebase_daemon_status" {
                // Daemon-up status: report "up" with a short message.
                // The verbatim daemon-down literal lives in the plugin
                // (SEC-8); daemon-up has freedom.
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
                let resp_bytes = serde_json::to_vec(&resp)?;
                write_frame(&mut stream, &resp_bytes).await?;
                continue;
            }
            // Unknown tool — JSON-RPC Method not found.
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": echo_id,
                "error": {
                    "code": -32601,
                    "message": "Method not found"
                }
            });
            let resp_bytes = serde_json::to_vec(&resp)?;
            write_frame(&mut stream, &resp_bytes).await?;
            continue;
        }

        // Legacy Slice 1a ping/pong echo for any non-MCP frame. The
        // smoke tests in `tests/daemon_smoke.rs` rely on this path.
        let ping_value = inbound
            .get("ping")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let response = serde_json::json!({ "pong": ping_value });
        let response_bytes = serde_json::to_vec(&response)?;
        write_frame(&mut stream, &response_bytes).await?;
    }
}
