//! Slice 2 TDD tests — content-equality idempotency (SEC-2-4 / SEC-2-8).
//!
//! Exercises the pure-Rust `check_idempotency` predicate that the
//! `install` / `write_mcp_descriptor` flows depend on. We never spawn
//! `systemctl` here — that lives in the platform-specific integration
//! tests gated behind `#[ignore]`.

use claudebase::daemon::service::{check_idempotency, IdempotencyDecision};
use std::fs;
use tempfile::tempdir;

#[test]
fn test_install_twice_same_content_is_noop() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("claudebase.service");
    let content = b"[Unit]\nDescription=test\n";
    fs::write(&path, content).unwrap();
    assert_eq!(
        check_idempotency(&path, content),
        IdempotencyDecision::AlreadyInstalled
    );
}

#[test]
fn test_install_with_different_content_signals_differs() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("claudebase.service");
    fs::write(&path, b"[Unit]\nDescription=old\n").unwrap();
    let decision = check_idempotency(&path, b"[Unit]\nDescription=new\n");
    assert_eq!(decision, IdempotencyDecision::Differs);
}

#[test]
fn test_install_on_missing_file_signals_fresh() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("missing.service");
    let decision = check_idempotency(&path, b"[Unit]\nDescription=x\n");
    assert_eq!(decision, IdempotencyDecision::Fresh);
}
