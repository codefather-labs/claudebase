//! SEC-2-2 — installing the daemon service as root is warned about, no longer
//! refused: on a server where every session runs as root the refusal left the
//! daemon with no service at all (see `root_install_warning`).

#![cfg(unix)]

use claudebase::daemon::service::root_install_warning;

#[test]
fn root_gets_a_warning_that_names_the_risk() {
    let w = root_install_warning(0).expect("root is warned");
    assert!(w.contains("as root") && w.contains("terminals"), "{w}");
}

#[test]
fn an_ordinary_user_gets_no_warning() {
    assert!(root_install_warning(1000).is_none());
    assert!(root_install_warning(501).is_none());
}
