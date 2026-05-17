//! Slice 6-MVP — `claudebase daemon doctor --asr` CLI contract tests.
//!
//! Per PRD FR-ACD-7.8 and TC-6.16 / TC-6.17, `daemon doctor --asr`:
//!   * exits 0 when the configured backend is healthy (whisper: model
//!     file present AND sha matches — sha verification is best-effort
//!     until the canonical SHA lands per Slice 6.1).
//!   * exits 1 with stderr containing a substring identifying the
//!     failure (e.g. `MISSING` for missing whisper model, `not implemented
//!     in v1` for sherpa-nemo / nim).
//!
//! These tests drive the CLI surface via `assert_cmd` so they exercise
//! the actual `run_daemon_doctor` dispatch path in `src/main.rs`. They
//! point the daemon at a tmp config dir via `XDG_CONFIG_HOME` so they
//! never read the operator's real `~/.config/claudebase/daemon.toml`.

use assert_cmd::Command;
use std::fs;
use tempfile::TempDir;

fn write_daemon_toml(dir: &TempDir, body: &str) {
    let cfg_dir = dir.path().join("claudebase");
    fs::create_dir_all(&cfg_dir).expect("create cfg dir");
    fs::write(cfg_dir.join("daemon.toml"), body).expect("write daemon.toml");
}

/// `backend = "sherpa-nemo"` → doctor exits 1 with `not implemented in v1`
/// per FR-ACD-7.4. The test asserts both exit code and substring.
#[test]
fn daemon_doctor_asr_sherpa_returns_not_implemented() {
    let tmp = TempDir::new().expect("tempdir");
    write_daemon_toml(
        &tmp,
        r#"[asr]
backend = "sherpa-nemo"
"#,
    );

    let assert = Command::cargo_bin("claudebase")
        .expect("bin")
        .env("XDG_CONFIG_HOME", tmp.path())
        // HOME must NOT carry the real user's config; point it at tmp too.
        .env("HOME", tmp.path())
        .args(["daemon", "doctor", "--asr"])
        .assert()
        .failure();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("not implemented") && combined.contains("v1"),
        "expected 'not implemented' + 'v1' in output; got: {combined}"
    );
}

/// `backend = "nim"` → doctor exits 1 with `not implemented in v1`.
#[test]
fn daemon_doctor_asr_nim_returns_not_implemented() {
    let tmp = TempDir::new().expect("tempdir");
    write_daemon_toml(
        &tmp,
        r#"[asr]
backend = "nim"
"#,
    );

    let assert = Command::cargo_bin("claudebase")
        .expect("bin")
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("HOME", tmp.path())
        .args(["daemon", "doctor", "--asr"])
        .assert()
        .failure();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("not implemented") && combined.contains("v1"),
        "expected 'not implemented' + 'v1' in output; got: {combined}"
    );
}

/// `backend = "whisper"` with `asr-whisper` feature OFF → doctor exits 1
/// because the factory can't construct WhisperAsr. The stderr message
/// names the missing feature (`asr-whisper`) so the operator can fix.
#[cfg(not(feature = "asr-whisper"))]
#[test]
fn daemon_doctor_asr_whisper_without_feature_returns_err() {
    let tmp = TempDir::new().expect("tempdir");
    write_daemon_toml(
        &tmp,
        r#"[asr]
backend = "whisper"
"#,
    );

    let assert = Command::cargo_bin("claudebase")
        .expect("bin")
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("HOME", tmp.path())
        .args(["daemon", "doctor", "--asr"])
        .assert()
        .failure();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("asr-whisper") || combined.contains("feature"),
        "expected feature-mention in output; got: {combined}"
    );
}

/// `backend = "whisper"` + feature ON + model file ABSENT → doctor exits
/// 1 with MISSING-model wording per TC-6.17. This test only runs under
/// `--features asr-whisper` because the factory has to succeed for the
/// health_check() call to even attempt to look for the model file.
#[cfg(feature = "asr-whisper")]
#[test]
fn daemon_doctor_asr_whisper_no_model_returns_err() {
    let tmp = TempDir::new().expect("tempdir");
    write_daemon_toml(
        &tmp,
        r#"[asr]
backend = "whisper"
"#,
    );

    let assert = Command::cargo_bin("claudebase")
        .expect("bin")
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("HOME", tmp.path())
        // Point the whisper model directory at tmp so the doctor's
        // health_check observes "model missing" rather than reading the
        // operator's real model.
        .env("CLAUDEBASE_HOME_OVERRIDE", tmp.path())
        .args(["daemon", "doctor", "--asr"])
        .assert()
        .failure();
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let lower = combined.to_lowercase();
    assert!(
        lower.contains("missing") && lower.contains("model"),
        "expected MISSING + model wording; got: {combined}"
    );
}

/// `daemon doctor --asr` with NO `daemon.toml` (fresh install) → exits 1
/// with a message that surfaces the missing config rather than a panic.
#[test]
fn daemon_doctor_asr_no_config_returns_err_not_panic() {
    let tmp = TempDir::new().expect("tempdir");
    // intentionally do NOT write daemon.toml

    let assert = Command::cargo_bin("claudebase")
        .expect("bin")
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("HOME", tmp.path())
        .args(["daemon", "doctor", "--asr"])
        .assert()
        .failure();
    let output = assert.get_output();
    // Failure is fine; the assertion is "no panic" (status code is
    // 1, NOT 101 which is rust's panic exit). 101 means abort.
    let code = output.status.code().unwrap_or(-1);
    assert_ne!(
        code, 101,
        "doctor must not panic; got panic exit 101. output:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
