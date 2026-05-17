//! Slice 1c — tracing JSON-line stderr smoke test.
//!
//! The slice's `Verify:` clause: `RUST_LOG=debug claudebase daemon serve`
//! emits JSON-structured log lines. This test spawns the daemon binary,
//! reads stderr, and asserts at least one line parses as a JSON object
//! containing the canonical `tracing-subscriber` fmt-json fields.
//!
//! It is deliberately lenient on which exact keys are present beyond the
//! minimum set so subscriber layer evolution does not break the test.

use anyhow::Result;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn spawn_daemon_capturing_stderr(tempdir: &Path) -> Result<std::process::Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "serve"]);
    // Force info-level minimum so accept-loop bind line shows up regardless
    // of the user's local RUST_LOG. The subscriber init MUST honour RUST_LOG
    // when set and default to info when unset.
    cmd.env("RUST_LOG", "info");

    #[cfg(unix)]
    {
        cmd.env("XDG_RUNTIME_DIR", tempdir);
    }
    #[cfg(windows)]
    {
        cmd.env("LOCALAPPDATA", tempdir);
    }

    cmd.stderr(Stdio::piped());
    cmd.stdout(Stdio::null());

    let child = cmd.spawn()?;
    Ok(child)
}

/// Wait until at least one stderr line parses as a JSON object containing
/// `level` (tracing emits this verbatim under fmt::json()) OR returns the
/// collected stderr after `max_wait` for diagnostic output.
fn first_json_stderr_line(
    child: &mut std::process::Child,
    max_wait: Duration,
) -> Result<Option<serde_json::Value>> {
    let stderr = child.stderr.take().expect("piped stderr");
    let reader = BufReader::new(stderr);
    let start = Instant::now();

    let mut all_lines = Vec::new();
    for line_res in reader.lines() {
        if start.elapsed() > max_wait {
            break;
        }
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        all_lines.push(line.clone());
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&line)
        {
            // tracing-subscriber's fmt::json() emits `level`, `target`,
            // `fields` (with the message inside) at minimum. We assert
            // `level` is present so we catch non-tracing JSON noise that
            // might leak from other sources.
            if map.contains_key("level") {
                return Ok(Some(serde_json::Value::Object(map)));
            }
        }
    }
    eprintln!(
        "no JSON line with `level` found within {:?}. stderr captured:\n{}",
        max_wait,
        all_lines.join("\n")
    );
    Ok(None)
}

#[test]
fn daemon_stderr_emits_at_least_one_tracing_json_line() {
    let tmpdir = tempfile::tempdir().expect("tempdir created");
    let mut child = spawn_daemon_capturing_stderr(tmpdir.path()).expect("spawn daemon");

    // Give the daemon up to 8s to bind + emit its first log line. The
    // accept-loop bind line is unconditional (info-level), so this should
    // arrive within ~100 ms typically.
    let json = first_json_stderr_line(&mut child, Duration::from_secs(8))
        .expect("first_json_stderr_line completed");

    // Kill the daemon before asserting so it cannot leak past the test.
    let _ = child.kill();
    let _ = child.wait();

    let json = json.expect("at least one JSON line with `level` key in daemon stderr");
    let obj = json.as_object().expect("top-level JSON object");
    assert!(
        obj.contains_key("level"),
        "expected `level` field in tracing-subscriber fmt-json output, got {obj:?}"
    );
    // Either `target` or `fields` indicates fmt-json. The tracing-subscriber
    // default is `target` + `fields`; we accept either.
    assert!(
        obj.contains_key("target") || obj.contains_key("fields"),
        "expected `target` or `fields` key in tracing-subscriber fmt-json output, got {obj:?}"
    );
}
