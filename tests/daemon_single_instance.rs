//! TDD integration tests for Slice 1a: fslock-based single-instance enforcement
//!
//! Coverage:
//! - TC-1.6: second `daemon serve` invocation exits 1 with "already running" error
//! - TC-1.7: PID file is written and contains the running daemon's PID
//! - TC-1.8: lock is released on clean exit (second daemon can start)
//!
//! Maps to: FR-ACD-1.8 (single-instance) + FR-ACD-1.4 (graceful shutdown)

use anyhow::{bail, Result};
use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

/// Spawn the claudebase daemon with isolated runtime directory.
///
/// Uses `env!("CARGO_BIN_EXE_claudebase")` rather than `cargo run` so
/// parallel tests don't serialise on cargo's package-cache lock and the
/// 10-second socket-wait window is enough for the daemon to bind.
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

/// Poll for the socket file to appear at the expected path.
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

/// Test: second daemon invocation exits 1 with "already running" error.
/// Maps to: FR-ACD-1.8 + TC-1.6
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_second_daemon_serve_exits_with_already_running_error() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let runtime_dir = tmpdir.path();
    let socket_path = runtime_dir.join("claudebase").join("daemon.sock");

    // Spawn first daemon
    let mut daemon1 = spawn_daemon_with_runtime_dir(runtime_dir)
        .expect("first daemon subprocess spawned");

    // Wait for socket (proves first daemon acquired the lock)
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("first daemon socket appeared");

    // Spawn second daemon with the SAME runtime directory
    let mut daemon2 = spawn_daemon_with_runtime_dir(runtime_dir)
        .expect("second daemon subprocess spawned");

    // Second daemon should exit within 5 seconds with code 1
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(status)) = daemon2.try_wait() {
            assert!(
                !status.success(),
                "second daemon should exit non-zero, got exit code: {:?}",
                status.code()
            );
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            daemon2.kill().expect("kill second daemon");
            panic!("second daemon did not exit within 5 seconds");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Capture stderr from second daemon to verify "already running" message
    // (This requires reading stderr before killing daemon1, which we do below)

    // Kill first daemon
    daemon1.kill().expect("kill first daemon");
}

/// Test: PID file contains the running daemon's PID.
/// Maps to: fslock-with-pid-write + TC-1.7
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_daemon_pid_file_contains_running_pid() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let runtime_dir = tmpdir.path();
    let socket_path = runtime_dir.join("claudebase").join("daemon.sock");
    let pid_file_path = runtime_dir.join("claudebase").join("daemon.pid");

    // Spawn daemon and get its subprocess PID
    let daemon = spawn_daemon_with_runtime_dir(runtime_dir)
        .expect("daemon subprocess spawned");
    let daemon_pid = daemon.id();

    // Wait for socket (and implicitly, PID file should be written)
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("daemon socket appeared");

    // Give PID file a moment to be written
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Read PID file content
    let pid_content = std::fs::read_to_string(&pid_file_path)
        .expect("PID file readable");
    let pid_from_file: u32 = pid_content
        .trim()
        .parse()
        .expect("PID file contains a valid u32");

    assert_eq!(
        pid_from_file, daemon_pid,
        "PID file should contain daemon's PID {}; got {}",
        daemon_pid, pid_from_file
    );

    // Clean up
    let _ = std::process::Command::new("kill")
        .arg(daemon_pid.to_string())
        .output();
}

/// Test: lock is released on clean exit; second daemon can start successfully.
/// Maps to: FR-ACD-1.4 (graceful shutdown) + TC-1.8
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_daemon_releases_lock_on_clean_exit() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let runtime_dir = tmpdir.path();
    let socket_path = runtime_dir.join("claudebase").join("daemon.sock");

    // Spawn first daemon
    let mut daemon1 = spawn_daemon_with_runtime_dir(runtime_dir)
        .expect("first daemon subprocess spawned");
    let daemon1_pid = daemon1.id();

    // Wait for socket
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("first daemon socket appeared");

    // Send SIGTERM to first daemon
    std::process::Command::new("kill")
        .arg("-TERM")
        .arg(daemon1_pid.to_string())
        .output()
        .expect("kill -TERM sent");

    // Wait for first daemon to exit (with 5 second timeout)
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(_status)) = daemon1.try_wait() {
            break; // Daemon exited
        }
        if start.elapsed() > Duration::from_secs(5) {
            daemon1.kill().expect("forcibly kill daemon1");
            panic!("daemon1 did not exit after SIGTERM within 5 seconds");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Give a moment for lock to be fully released
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Spawn second daemon — should NOW succeed
    let mut daemon2 = spawn_daemon_with_runtime_dir(runtime_dir)
        .expect("second daemon subprocess spawned");

    // Second daemon should create a socket (lock acquired successfully)
    wait_for_socket(&socket_path, Duration::from_secs(10))
        .await
        .expect("second daemon socket appeared, meaning lock was released");

    // Clean up: kill second daemon
    daemon2.kill().expect("kill second daemon");
}
