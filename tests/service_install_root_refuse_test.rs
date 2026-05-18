//! Slice 2 TDD test — SEC-2-2 root-refuse with the testable refactor.
//!
//! The production `refuse_root_install()` reads `geteuid()` from libc;
//! impossible to mock from cargo test. We expose
//! `refuse_root_install_with_euid(euid)` instead and exercise both
//! branches deterministically.

#![cfg(unix)]

use claudebase::daemon::service::refuse_root_install_with_euid;

#[test]
fn test_install_as_root_refuses_with_specific_message() {
    let err = refuse_root_install_with_euid(0)
        .expect_err("euid==0 must be refused");
    let s = format!("{err}");
    assert!(
        s.contains("do not run 'daemon install' as root"),
        "stderr text mismatch: got `{s}`"
    );
}

#[test]
fn test_install_as_normal_user_proceeds() {
    refuse_root_install_with_euid(1000).expect("non-root euid must succeed");
    refuse_root_install_with_euid(501).expect("non-root euid must succeed");
}
