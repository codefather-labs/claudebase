//! Channel access gate, driven through the CLI that operators actually run.
//!
//! History worth knowing: these tests were written against
//! `claudebase daemon access pair …`, a subcommand that was never implemented —
//! the functionality shipped as an LLM-driven skill instead, so four of them had
//! been failing since the day they landed and the rest only wrote a JSON file
//! and read it back, asserting nothing about claudebase.
//!
//! v0.10 made access management a real command (`claudebase telegram
//! pair|access|policy|allow|revoke`), specifically so that granting access is
//! not a decision made by a model whose context contains messages from the
//! channel being gated. These tests now exercise that command against the real
//! `~/.claude/channels/claudebase/access.json` and the real `PendingEntry`
//! shape.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::json;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn access_path(home: &Path) -> PathBuf {
    home.join(".claude")
        .join("channels")
        .join("claudebase")
        .join("access.json")
}

/// Write an `access.json` in the shape `channel_state::Access` deserialises:
/// `pending` is keyed by CODE and each entry carries `senderId` / `chatId` /
/// `expiresAt` — not the `{telegram_user_id, expires_at}` shape the old fixture
/// invented.
fn write_access(
    home: &Path,
    dm_policy: &str,
    allow_from: &[&str],
    pending: &[(&str, &str, i64)],
) -> Result<()> {
    let path = access_path(home);
    fs::create_dir_all(path.parent().expect("parent"))?;

    let mut pending_map = serde_json::Map::new();
    for (code, sender, expires_at) in pending {
        pending_map.insert(
            (*code).to_string(),
            json!({
                "senderId": sender,
                "chatId": sender,
                "createdAt": now_ms(),
                "expiresAt": expires_at,
                "replies": 1,
            }),
        );
    }

    fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "dmPolicy": dm_policy,
            "allowFrom": allow_from,
            "groups": {},
            "pending": pending_map,
            "mentionPatterns": [],
        }))?,
    )?;
    Ok(())
}

fn read_access(home: &Path) -> Result<serde_json::Value> {
    Ok(serde_json::from_str(&fs::read_to_string(access_path(home))?)?)
}

fn run(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_claudebase"));
    cmd.args(args);
    cmd.env("HOME", home);
    cmd.env("XDG_RUNTIME_DIR", home.join("run"));
    cmd.output().expect("run claudebase")
}

fn allowed(access: &serde_json::Value) -> Vec<String> {
    access["allowFrom"]
        .as_array()
        .expect("allowFrom array")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn pairing_a_valid_code_moves_the_sender_into_the_allowlist() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path();
    write_access(home, "pairing", &[], &[("abc123", "1001", now_ms() + 3_600_000)])?;

    let out = run(home, &["telegram", "pair", "abc123"]);
    assert!(
        out.status.success(),
        "pair should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let access = read_access(home)?;
    assert_eq!(allowed(&access), vec!["1001"]);
    assert!(
        access["pending"].as_object().expect("pending").is_empty(),
        "the code must be consumed once used"
    );

    // The daemon's approved-dir watcher turns this marker into the "Paired!"
    // confirmation the sender sees in Telegram; without it the operator
    // approves and the sender is never told.
    let marker = home
        .join(".claude")
        .join("channels")
        .join("claudebase")
        .join("approved")
        .join("1001");
    assert!(marker.exists(), "approval marker missing at {marker:?}");
    assert_eq!(fs::read_to_string(marker)?.trim(), "1001");
    Ok(())
}

#[test]
fn an_expired_code_grants_nothing() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path();
    write_access(home, "pairing", &[], &[("exp123", "1001", now_ms() - 1_000)])?;

    let out = run(home, &["telegram", "pair", "exp123"]);
    assert!(!out.status.success(), "an expired code must be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("expired") || stderr.contains("not pending"),
        "the error should explain why: {stderr}"
    );
    assert!(allowed(&read_access(home)?).is_empty());
    Ok(())
}

#[test]
fn an_unknown_code_grants_nothing_and_leaves_the_real_one_pending() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path();
    write_access(home, "pairing", &[], &[("abc123", "1001", now_ms() + 3_600_000)])?;

    let out = run(home, &["telegram", "pair", "xxxxxxxx"]);
    assert!(!out.status.success(), "an unknown code must be rejected");

    let access = read_access(home)?;
    assert!(allowed(&access).is_empty());
    assert!(
        access["pending"].get("abc123").is_some(),
        "a wrong guess must not consume the pending code"
    );
    Ok(())
}

#[test]
fn access_lists_policy_allowlist_and_pending() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path();
    write_access(
        home,
        "pairing",
        &["1001", "2002"],
        &[("abc123", "3003", now_ms() + 600_000)],
    )?;

    let out = run(home, &["telegram", "access"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for expected in ["pairing", "1001", "2002", "abc123", "3003"] {
        assert!(stdout.contains(expected), "`access` output missing {expected:?}: {stdout}");
    }
    Ok(())
}

#[test]
fn policy_switches_and_takes_effect_in_the_file() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path();
    write_access(home, "pairing", &["1001"], &[])?;

    let out = run(home, &["telegram", "policy", "allowlist"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(read_access(home)?["dmPolicy"], json!("allowlist"));

    let out = run(home, &["telegram", "policy", "disabled"]);
    assert!(out.status.success());
    assert_eq!(read_access(home)?["dmPolicy"], json!("disabled"));
    Ok(())
}

#[test]
fn switching_to_allowlist_with_nobody_allowed_is_refused() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path();
    write_access(home, "pairing", &[], &[])?;

    let out = run(home, &["telegram", "policy", "allowlist"]);
    // Allowing this would lock the operator out of their own bot, recoverable
    // only by hand-editing JSON.
    assert!(!out.status.success(), "empty-allowlist lockout must be refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("lock out"));
    assert_eq!(read_access(home)?["dmPolicy"], json!("pairing"));
    Ok(())
}

#[test]
fn allow_and_revoke_edit_the_allowlist() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path();
    write_access(home, "pairing", &[], &[])?;

    assert!(run(home, &["telegram", "allow", "4004"]).status.success());
    assert_eq!(allowed(&read_access(home)?), vec!["4004"]);

    // Idempotent: adding twice must not duplicate the entry.
    assert!(run(home, &["telegram", "allow", "4004"]).status.success());
    assert_eq!(allowed(&read_access(home)?), vec!["4004"]);

    assert!(run(home, &["telegram", "revoke", "4004"]).status.success());
    assert!(allowed(&read_access(home)?).is_empty());

    // Revoking someone who was never allowed is an error, not a silent no-op —
    // otherwise a typo reads as success.
    assert!(!run(home, &["telegram", "revoke", "4004"]).status.success());
    Ok(())
}

#[test]
fn a_non_numeric_sender_id_is_refused() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path();
    write_access(home, "pairing", &[], &[])?;

    let out = run(home, &["telegram", "allow", "@username"]);
    assert!(
        !out.status.success(),
        "Telegram ids are numeric; accepting a @handle would silently allow nobody"
    );
    assert!(allowed(&read_access(home)?).is_empty());
    Ok(())
}
