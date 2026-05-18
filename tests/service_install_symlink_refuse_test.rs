//! Slice 2 TDD tests — SEC-2-3 / SEC-2-9 write-side symlink-refusal.
//!
//! These exercises confirm that the `write_refusing_symlink` and
//! `ensure_install_parent` helpers refuse to follow operator-controlled
//! symlinks into world-writable locations. This is the load-bearing
//! defence against the "/tmp swap symlink" attack family.

#![cfg(unix)]

use claudebase::daemon::service::{ensure_install_parent, write_refusing_symlink};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

#[test]
fn test_install_refuses_symlinked_unit_file() {
    let dir = tempdir().unwrap();
    let sentinel = dir.path().join("sentinel.txt");
    fs::write(&sentinel, b"untouched").unwrap();

    let target = dir.path().join("claudebase.service");
    std::os::unix::fs::symlink(&sentinel, &target)
        .expect("symlink for the refuse test");

    let err = write_refusing_symlink(&target, b"[Unit]\nX=1\n", 0o644)
        .expect_err("refuse_symlink must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("refuse to write through symlink"),
        "wrong error text: {msg}"
    );

    let body = fs::read_to_string(&sentinel).unwrap();
    assert_eq!(body, "untouched", "sentinel file must not be modified");
}

#[test]
fn test_install_refuses_symlinked_parent_dir() {
    let dir = tempdir().unwrap();
    let real_target = dir.path().join("real_systemd_dir");
    fs::create_dir(&real_target).unwrap();
    fs::set_permissions(&real_target, fs::Permissions::from_mode(0o755)).unwrap();

    let symlinked = dir.path().join("user");
    std::os::unix::fs::symlink(&real_target, &symlinked)
        .expect("symlink for the refuse test");

    let err = ensure_install_parent(&symlinked, 0o755)
        .expect_err("symlinked parent must be refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("refuse to install into symlinked directory"),
        "wrong error text: {msg}"
    );
}

#[test]
fn test_ensure_install_parent_refuses_world_writable_existing_dir() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("loose_dir");
    fs::create_dir(&target).unwrap();
    // 0o777 has bits outside the required 0o700 — must refuse.
    fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).unwrap();

    let err = ensure_install_parent(&target, 0o700)
        .expect_err("loose-permission existing dir must be refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("refuse to install into"),
        "wrong error text: {msg}"
    );
}

#[test]
fn test_write_refusing_symlink_succeeds_on_fresh_path() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("fresh.service");
    write_refusing_symlink(&path, b"hello world\n", 0o644)
        .expect("fresh write must succeed");
    let body = fs::read_to_string(&path).unwrap();
    assert_eq!(body, "hello world\n");
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "SEC-2-12: post-write chmod must force 0o644");
}
