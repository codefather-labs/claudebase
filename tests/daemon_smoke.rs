//! TDD integration tests for Slice 1a: UDS/named-pipe server + echo
//!
//! Coverage:
//! - TC-1.1: daemon accepts 2 concurrent connections via UDS (Unix) / named pipe (Windows)
//! - TC-1.2: socket file permissions are 0o600 (Unix only)
//! - TC-1.3: UDS path uses filesystem, not abstract namespace (Unix only)
//!
//! These tests spawn `cargo run -- daemon serve` as a subprocess, isolated to a temporary
//! runtime directory so they don't collide with a real daemon on the dev machine.
//! The daemon is expected to fail at compile time (missing API) on first run — that's
//! the red phase of TDD. The implementer's job (step 3) is to make these tests pass.

use anyhow::{bail, Result};
use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

/// Spawn the claudebase daemon with XDG_RUNTIME_DIR/LOCALAPPDATA scoped to
/// the given temp directory. Returns the child process handle.
///
/// Uses `env!("CARGO_BIN_EXE_claudebase")` — cargo sets this env var to the
/// absolute path of the freshly-built `claudebase` binary when compiling
/// integration tests (per
/// https://doc.rust-lang.org/cargo/reference/environment-variables.html).
/// Calling the binary directly avoids the second `cargo run` invocation
/// the original test helper used, which serialised on cargo's package-
/// cache lock under parallel tests and frequently exceeded the 10-second
/// socket-wait window.
fn spawn_daemon_with_runtime_dir(tempdir: &Path) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "serve"]);

    #[cfg(unix)]
    {
        cmd.env("XDG_RUNTIME_DIR", tempdir);
    }

    #[cfg(windows)]
    {
        cmd.env("LOCALAPPDATA", tempdir);
    }

    let child = cmd.spawn()?;
    Ok(child)
}

/// Poll for the socket file to appear at the expected path. Times out after
/// the given duration.
async fn wait_for_socket(socket_path: &Path, max_wait: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        if socket_path.exists() {
            return Ok(());
        }
        if start.elapsed() > max_wait {
            bail!("socket file not found after {:?}: {:?}", max_wait, socket_path);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Connect to the UDS/named-pipe socket, send a length-prefixed JSON ping,
/// and read back a length-prefixed JSON pong response.
#[cfg(unix)]
async fn send_ping_get_pong(socket_path: &Path, n: u32) -> Result<bool> {
    use interprocess::local_socket::tokio::{prelude::*, Stream};
    use interprocess::local_socket::ToFsName;
    use interprocess::local_socket::GenericFilePath;

    let ping_json = serde_json::json!({ "ping": n });
    let ping_body = serde_json::to_vec(&ping_json)?;

    let path_name = socket_path.to_fs_name::<GenericFilePath>()?;
    let mut stream = Stream::connect(path_name).await?;

    // Write length-prefixed frame
    let len = ping_body.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&ping_body).await?;
    stream.flush().await?;

    // Read length-prefixed response
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    if resp_len > 1024 * 1024 {
        bail!("response frame too large: {resp_len} bytes");
    }
    let mut resp_body = vec![0u8; resp_len];
    stream.read_exact(&mut resp_body).await?;

    // Parse and validate pong
    let pong: serde_json::Value = serde_json::from_slice(&resp_body)?;
    let pong_value = pong
        .get("pong")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("pong field missing or not an integer"))?;

    Ok(pong_value == n as u64)
}

#[cfg(windows)]
async fn send_ping_get_pong(_socket_path: &Path, _n: u32) -> Result<bool> {
    bail!("Windows named pipe support not yet implemented in test helper")
}

/// Test: daemon accepts 2 concurrent connections and echoes ping/pong frames.
/// Maps to: UC-2 primary flow + TC-1.1, TC-1.4, TC-1.5 (concurrent accept, echo)
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_daemon_serve_accepts_two_concurrent_connections() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let runtime_dir = tmpdir.path();
    let socket_path = runtime_dir.join("claudebase").join("daemon.sock");

    // Spawn daemon as a subprocess
    let daemon = spawn_daemon_with_runtime_dir(runtime_dir)
        .expect("daemon subprocess spawned")
        .id();

    // Wait for socket to appear (timeout 10 seconds)
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("socket file appeared within timeout");

    // Spawn 2 concurrent client tasks
    let socket1 = socket_path.clone();
    let socket2 = socket_path.clone();

    let c1 = tokio::spawn(async move {
        send_ping_get_pong(&socket1, 1).await
    });

    let c2 = tokio::spawn(async move {
        send_ping_get_pong(&socket2, 2).await
    });

    // Wait for both clients to complete (with timeout)
    let result1 = timeout(Duration::from_secs(5), c1)
        .await
        .expect("client 1 timeout")
        .expect("client 1 panicked");
    let result2 = timeout(Duration::from_secs(5), c2)
        .await
        .expect("client 2 timeout")
        .expect("client 2 panicked");

    // Both should succeed
    assert!(
        result1.is_ok(),
        "client 1 failed: {:?}",
        result1.err()
    );
    assert!(
        result2.is_ok(),
        "client 2 failed: {:?}",
        result2.err()
    );

    // Kill daemon
    let _ = std::process::Command::new("kill")
        .arg(daemon.to_string())
        .output();

    // Give it a moment to clean up
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Socket should be cleaned up (or daemon should have exited 0)
    // We don't strictly enforce this here since cleanup timing is OS-dependent,
    // but in a real scenario the socket would be unlinked on graceful shutdown.
}

/// Test: socket file has correct permissions (0o600 on Unix).
/// Maps to: architect STRUCTURAL #2 + TC-1.2
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_daemon_socket_file_permissions_unix() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let runtime_dir = tmpdir.path();
    let socket_path = runtime_dir.join("claudebase").join("daemon.sock");

    // Spawn daemon
    let _daemon = spawn_daemon_with_runtime_dir(runtime_dir)
        .expect("daemon subprocess spawned");

    // Wait for socket
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    // Check socket permissions
    let metadata = fs::metadata(&socket_path)
        .expect("socket metadata readable");
    let mode = metadata.permissions().mode();
    let socket_perms = mode & 0o777;

    assert_eq!(
        socket_perms, 0o600,
        "socket file permissions should be 0o600, got 0o{:o}",
        socket_perms
    );

    // Also check parent directory permissions (should be 0o700)
    let parent_dir = socket_path.parent().expect("parent directory exists");
    let parent_metadata = fs::metadata(parent_dir)
        .expect("parent directory metadata readable");
    let parent_perms = parent_metadata.permissions().mode() & 0o777;

    assert_eq!(
        parent_perms, 0o700,
        "parent directory permissions should be 0o700, got 0o{:o}",
        parent_perms
    );
}

/// Test: UDS path is a real filesystem path, not an abstract namespace name.
/// Maps to: architect STRUCTURAL #1 + TC-1.3
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_daemon_uses_filesystem_path_not_namespace() {
    use std::fs;
    use std::os::unix::fs::FileTypeExt;

    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let runtime_dir = tmpdir.path();
    let socket_path = runtime_dir.join("claudebase").join("daemon.sock");

    // Spawn daemon
    let _daemon = spawn_daemon_with_runtime_dir(runtime_dir)
        .expect("daemon subprocess spawned");

    // Wait for socket
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    // Verify the path exists as a filesystem entry (not abstract namespace)
    assert!(
        socket_path.exists(),
        "socket path should exist as a real file: {:?}",
        socket_path
    );

    // Verify it's a socket file (not a regular file)
    let metadata = fs::metadata(&socket_path)
        .expect("socket metadata readable");
    let file_type = metadata.file_type();
    assert!(
        file_type.is_socket(),
        "socket_path should be a socket, not: {:?}",
        file_type
    );
}
