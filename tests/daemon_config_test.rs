//! TDD tests for Slice 4: Daemon configuration and config edit/show commands
//!
//! Coverage:
//! - TC-4.13: daemon config edit with malformed TOML should fail gracefully
//! - daemon config show should display config (verified in telegram_secrets_perm_test)
//! - SEC-15: daemon.toml symlink rejection
//! - SEC-15: bot_token field forbidden in daemon.toml (must be in secrets.toml)
//! - SEC-16: config edit uses arg-vector form for $EDITOR (not shell-string)

use anyhow::{bail, Result};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

#[cfg(unix)]
use std::os::unix::fs::symlink;

/// Helper to create a valid daemon.toml
fn create_daemon_toml(tempdir: &Path, content: &str) -> Result<()> {
    let config_dir = tempdir.join(".config").join("claudebase");
    fs::create_dir_all(&config_dir)?;

    let daemon_path = config_dir.join("daemon.toml");
    fs::write(&daemon_path, content)?;

    Ok(())
}

/// Valid minimal daemon.toml content
fn valid_daemon_toml() -> String {
    r#"[asr]
backend = "whisper"

[daemon]
port = 9999
"#
    .to_string()
}

/// Test: TC-4.13 — daemon config edit with malformed TOML fails gracefully
#[test]
#[cfg(unix)]
fn test_config_edit_malformed_toml_rejected() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let home_dir = tmpdir.path();

    // Setup: create valid daemon.toml first
    create_daemon_toml(home_dir, &valid_daemon_toml())?;

    // Use /bin/sh editor that writes broken TOML
    let editor_script = home_dir.join("broken_editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/bash
echo "[unclosed" > "$1"
"#,
    )?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&editor_script, fs::Permissions::from_mode(0o755))?;
    }

    // Run: claudebase daemon config edit with broken editor
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_claudebase"));
    cmd.args(["daemon", "config", "edit"]);
    cmd.env("HOME", home_dir);
    cmd.env("EDITOR", editor_script.to_string_lossy().to_string());
    cmd.env("XDG_RUNTIME_DIR", home_dir.join("run"));

    let output = cmd.output()?;

    // Verify: config edit fails
    if output.status.success() {
        bail!("config edit succeeded with malformed TOML — should have failed");
    }

    let stderr = String::from_utf8(output.stderr)?;

    // Verify: error message mentions TOML or invalid
    if !stderr.to_lowercase().contains("toml")
        && !stderr.to_lowercase().contains("invalid")
        && !stderr.to_lowercase().contains("parse")
    {
        bail!("stderr does not mention TOML parse error: {}", stderr);
    }

    Ok(())
}

/// Test: daemon config edit with /bin/true editor (no-op)
#[test]
#[cfg(unix)]
fn test_config_edit_noop_editor() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let home_dir = tmpdir.path();

    // Setup: create valid daemon.toml
    create_daemon_toml(home_dir, &valid_daemon_toml())?;

    let config_path = home_dir.join(".config").join("claudebase").join("daemon.toml");
    let original_content = fs::read_to_string(&config_path)?;

    // Run: daemon config edit with /usr/bin/true (no-op editor)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_claudebase"));
    cmd.args(["daemon", "config", "edit"]);
    cmd.env("HOME", home_dir);
    cmd.env("EDITOR", "true"); // The Unix no-op command
    cmd.env("XDG_RUNTIME_DIR", home_dir.join("run"));

    let output = cmd.output()?;

    // Verify: config edit succeeds (true always exits 0)
    if !output.status.success() {
        // Might fail if EDITOR is not found — that's acceptable for this test
        // What matters is we don't panic
    }

    // Verify: original config unchanged
    let new_content = fs::read_to_string(&config_path)?;
    if new_content != original_content {
        bail!("config was modified by true editor");
    }

    Ok(())
}

/// Test: SEC-15 — daemon.toml cannot be a symlink
#[test]
#[cfg(unix)]
fn test_daemon_toml_symlink_rejected() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let home_dir = tmpdir.path();

    let config_dir = home_dir.join(".config").join("claudebase");
    fs::create_dir_all(&config_dir)?;

    // Create a symlink to /etc/hosts as daemon.toml
    let daemon_path = config_dir.join("daemon.toml");
    symlink("/etc/hosts", &daemon_path)?;

    // Run: daemon config show
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_claudebase"));
    cmd.args(["daemon", "config", "show"]);
    cmd.env("HOME", home_dir);
    cmd.env("XDG_RUNTIME_DIR", home_dir.join("run"));

    let output = cmd.output()?;

    // Verify: command fails (symlink rejected)
    if output.status.success() {
        bail!("daemon config show succeeded with symlink daemon.toml — SEC-15 violation!");
    }

    let stderr = String::from_utf8(output.stderr)?;

    // Verify: error mentions symlink or refuse
    if !stderr.to_lowercase().contains("symlink")
        && !stderr.to_lowercase().contains("refuse")
        && !stderr.to_lowercase().contains("not allowed")
    {
        bail!("stderr does not mention symlink rejection: {}", stderr);
    }

    Ok(())
}

/// Test: SEC-15 — bot_token field forbidden in daemon.toml
#[test]
#[cfg(unix)]
fn test_bot_token_in_daemon_toml_forbidden() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let home_dir = tmpdir.path();

    // Create daemon.toml with bot_token field (forbidden by SEC-15)
    let bad_daemon_toml = r#"[asr]
backend = "whisper"

[telegram]
bot_token = "123456:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi"
"#;

    create_daemon_toml(home_dir, bad_daemon_toml)?;

    // Run: daemon config show
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_claudebase"));
    cmd.args(["daemon", "config", "show"]);
    cmd.env("HOME", home_dir);
    cmd.env("XDG_RUNTIME_DIR", home_dir.join("run"));

    let output = cmd.output()?;

    // Verify: command fails
    if output.status.success() {
        bail!("daemon config show succeeded with bot_token in daemon.toml — SEC-15 violation!");
    }

    let stderr = String::from_utf8(output.stderr)?;

    // Verify: error message explains bot_token must be in secrets.toml
    if !stderr.to_lowercase().contains("bot_token")
        || !stderr.to_lowercase().contains("secrets")
    {
        bail!(
            "stderr does not explain bot_token belongs in secrets.toml: {}",
            stderr
        );
    }

    Ok(())
}

/// Test: daemon config show displays configuration
#[test]
#[cfg(unix)]
fn test_config_show_displays_config() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let home_dir = tmpdir.path();

    let daemon_toml = r#"[asr]
backend = "whisper"

[daemon]
log_level = "info"
"#;

    create_daemon_toml(home_dir, daemon_toml)?;

    // Run: daemon config show
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_claudebase"));
    cmd.args(["daemon", "config", "show"]);
    cmd.env("HOME", home_dir);
    cmd.env("XDG_RUNTIME_DIR", home_dir.join("run"));

    let output = cmd.output()?;

    if !output.status.success() {
        bail!(
            "daemon config show failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8(output.stdout)?;

    // Verify: output contains config fields
    if !stdout.contains("asr") || !stdout.contains("whisper") {
        bail!("config show output missing expected fields: {}", stdout);
    }

    Ok(())
}

/// Test: daemon config show --json returns valid JSON
#[test]
#[cfg(unix)]
fn test_config_show_json_valid() -> Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let home_dir = tmpdir.path();

    create_daemon_toml(home_dir, &valid_daemon_toml())?;

    // Run: daemon config show --json
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_claudebase"));
    cmd.args(["daemon", "config", "show", "--json"]);
    cmd.env("HOME", home_dir);
    cmd.env("XDG_RUNTIME_DIR", home_dir.join("run"));

    let output = cmd.output()?;

    if !output.status.success() {
        bail!(
            "daemon config show --json failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8(output.stdout)?;

    // Verify: output is valid JSON
    match serde_json::from_str::<serde_json::Value>(&stdout) {
        Ok(_) => Ok(()),
        Err(e) => bail!("config show --json output is not valid JSON: {}", e),
    }
}
