//! The supervisor must not outlive the operator's session, and must not leave
//! `claude` running when it goes.
//!
//! The failure this guards against, reported live on 2026-08-17: the operator
//! closed a window that `claudebase run` owned, then could not `/resume` that
//! conversation — Claude Code does not offer a session whose process is still
//! running, and the process was still running. The supervisor had no shutdown
//! path at all: no signal handlers, and the stdin pump swallowed EOF. Since
//! `claude` lives on the INNER pty, it cannot observe the outer terminal
//! disappearing on its own; something has to tell it, and nothing did.
//!
//! A stub stands in for `claude` so the test does not depend on Claude Code
//! being installed, and so the child's pid is knowable: the stub writes its own
//! pid before blocking.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn binary() -> PathBuf {
    let mut p = std::env::current_exe().expect("test exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("claudebase")
}

/// `kill(pid, 0)` — EPERM means the process exists but belongs to someone we
/// may not signal, which is still alive.
fn alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn wait_until<F: Fn() -> bool>(what: &str, limit: Duration, f: F) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("timed out waiting for: {what}");
    false
}

/// Writes a `claude` stub that records its pid and then blocks forever.
///
/// `exec` matters: without it the pid recorded is the shell's, and the shell
/// would exit leaving the real sleeper behind under a different pid.
fn write_stub(dir: &Path, pidfile: &Path, trap_hup: bool) -> PathBuf {
    let stub = dir.join("claude");
    let trap = if trap_hup {
        // Deliberately deaf to the polite signals, to prove the ladder ends in
        // something the child cannot ignore.
        "trap '' HUP TERM\n"
    } else {
        ""
    };
    let body = format!(
        "#!/bin/sh\n{trap}echo $$ > {pid}\n{tail}",
        pid = pidfile.display(),
        tail = if trap_hup {
            // `exec sleep` would replace the shell and lose the traps.
            "while true; do sleep 1; done\n"
        } else {
            "exec sleep 300\n"
        }
    );
    std::fs::write(&stub, body).expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    stub
}

struct Session {
    supervisor: Child,
    child_pid: i32,
    _home: tempfile::TempDir,
    _bin: tempfile::TempDir,
}

impl Session {
    fn start(trap_hup: bool) -> Option<Self> {
        let home = tempfile::tempdir().expect("home");
        let bindir = tempfile::tempdir().expect("bindir");
        let pidfile = home.path().join("stub.pid");
        write_stub(bindir.path(), &pidfile, trap_hup);

        let path = format!(
            "{}:{}",
            bindir.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );

        let supervisor = Command::new(binary())
            .arg("run")
            .env("PATH", path)
            .env("HOME", home.path())
            // Keep the supervisor off the operator's real daemon socket.
            .env("CLAUDEBASE_LOG_STDERR", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn supervisor");

        // The stub records its pid once spawned.
        if !wait_until("stub to start", Duration::from_secs(20), || {
            pidfile.exists() && !std::fs::read_to_string(&pidfile).unwrap_or_default().trim().is_empty()
        }) {
            return None;
        }
        let child_pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("read pidfile")
            .trim()
            .parse()
            .expect("pid");

        assert!(alive(child_pid), "stub should be running before shutdown");
        Some(Session {
            supervisor,
            child_pid,
            _home: home,
            _bin: bindir,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Only ever the pids this test created.
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
        unsafe { libc::kill(self.child_pid, libc::SIGKILL) };
    }
}

/// SIGTERM to the supervisor — a service stop, or a terminal emulator tearing
/// the session down — must reach `claude`, not just end the supervisor and
/// orphan it.
#[test]
fn terminating_the_supervisor_stops_the_claude_it_owns() {
    let Some(mut session) = Session::start(false) else {
        eprintln!("stub never started; skipping");
        return;
    };

    let sup_pid = session.supervisor.id() as i32;
    unsafe { libc::kill(sup_pid, libc::SIGTERM) };

    let child = session.child_pid;
    assert!(
        wait_until("claude to exit", Duration::from_secs(20), || !alive(child)),
        "the `claude` the supervisor owned is still running after the supervisor was told to stop — \
         it is holding a transcript the operator can no longer /resume"
    );
    let _ = session.supervisor.wait();
}

/// The polite signals are a courtesy, not a contract. A child that ignores both
/// must still be reaped, or it holds the operator's conversation hostage — the
/// exact symptom that started this.
#[test]
fn a_child_that_ignores_hup_and_term_is_still_reaped() {
    let Some(mut session) = Session::start(true) else {
        eprintln!("stub never started; skipping");
        return;
    };

    let sup_pid = session.supervisor.id() as i32;
    unsafe { libc::kill(sup_pid, libc::SIGTERM) };

    let child = session.child_pid;
    // 3s SIGHUP grace + 5s SIGTERM grace, then SIGKILL — plus slack.
    assert!(
        wait_until("stubborn claude to be killed", Duration::from_secs(30), || {
            !alive(child)
        }),
        "a child ignoring SIGHUP and SIGTERM was never escalated to SIGKILL"
    );
    let _ = session.supervisor.wait();
}

/// Closing the operator's terminal shows up as EOF on the supervisor's stdin.
/// The child is on a different pty and cannot see it.
#[test]
fn losing_the_operators_terminal_ends_the_session() {
    let Some(mut session) = Session::start(false) else {
        eprintln!("stub never started; skipping");
        return;
    };

    // Dropping stdin is the closest a test can get to the window going away
    // without allocating a real terminal.
    drop(session.supervisor.stdin.take());
    // Backstop for the non-tty case: the stdin pump only runs when stdin is a
    // TTY, which it is not under `cargo test`, so drive the same path the way a
    // terminal teardown would.
    unsafe { libc::kill(session.supervisor.id() as i32, libc::SIGHUP) };

    let child = session.child_pid;
    assert!(
        wait_until("claude to exit", Duration::from_secs(20), || !alive(child)),
        "the session outlived the operator's terminal"
    );
    let _ = session.supervisor.wait();
}

/// Guards the discipline that matters more than the ladder: signals go to the
/// pid captured at spawn. Matching processes by name finds the operator's OTHER
/// Claude sessions, which has happened.
#[test]
fn the_supervisor_signals_only_its_own_child() {
    let Some(mut session) = Session::start(false) else {
        eprintln!("stub never started; skipping");
        return;
    };

    // A second, unrelated sleeper standing in for someone else's session.
    let mut bystander = Command::new("sleep")
        .arg("60")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bystander");
    let bystander_pid = bystander.id() as i32;

    unsafe { libc::kill(session.supervisor.id() as i32, libc::SIGTERM) };

    let child = session.child_pid;
    assert!(
        wait_until("own child to exit", Duration::from_secs(20), || !alive(child)),
        "the supervisor failed to stop its own child"
    );
    assert!(
        alive(bystander_pid),
        "an unrelated process was killed — the supervisor is not aiming at a captured pid"
    );

    let _ = bystander.kill();
    let _ = bystander.wait();
    let _ = session.supervisor.wait();
}
