//! Slice 2 (insights-hybrid-corpus): global insights resolver + the
//! open->migrate->validate chain that `open_and_validate_at` runs on an
//! absolute path. Covers FR-IHC-2.1..2.4 and TC-IHC-14.1 / 14.2 / 2.6.
//!
//! `open_and_validate_at` itself lives in the binary crate (`main.rs`) and is
//! not reachable from an integration test; its body is a thin wrapper over the
//! lib functions exercised here (`open_or_init_v2` + `validate_schema`), so
//! this file proves the chain on an absolute global-db path. The wrapper is
//! exercised end-to-end by Slice 3's CLI tests once it is wired in.

use claudebase::store::{
    global_insights_db_path_from_home, open_or_init_v2, resolve_global_insights_db,
    validate_schema,
};
use std::ffi::OsString;
use tempfile::TempDir;

/// FR-IHC-2.1 / TC-IHC-14.1: the path is `<home>/.claude/knowledge/insights.db`.
#[test]
fn path_from_home_builds_global_insights_path() {
    let tmp = TempDir::new().unwrap();
    let home: OsString = tmp.path().as_os_str().to_owned();
    let p = global_insights_db_path_from_home(Some(home)).unwrap();
    assert_eq!(
        p,
        tmp.path().join(".claude").join("knowledge").join("insights.db")
    );
}

/// FR-IHC-2.2 / TC-IHC-2.6: HOME unset -> exact operator-facing error.
#[test]
fn path_from_home_none_errors_with_home_not_set() {
    let err = global_insights_db_path_from_home(None).unwrap_err();
    assert!(err.contains("$HOME not set"), "got: {err}");
}

/// FR-IHC-2.3 / TC-IHC-14.1(c): a freshly-created global db on an absolute
/// path opens, stamps schema v5, and passes the corruption gate (which Slice 1
/// bumped to accept 1..=5). This is the exact chain `open_and_validate_at` runs.
#[test]
fn fresh_global_db_opens_at_schema_v5_and_validates() {
    let tmp = TempDir::new().unwrap();
    let path =
        global_insights_db_path_from_home(Some(tmp.path().as_os_str().to_owned())).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    let conn = open_or_init_v2(&path).expect("open_or_init_v2 on absolute global path");
    validate_schema(&conn).expect("fresh global db passes schema validation (1..=5 gate)");
    let v: i64 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 5, "fresh global db stamped at schema v5");
}

/// FR-IHC-2.1 / 2.4 / TC-IHC-14.1(a)(b): the live resolver reads HOME and
/// creates the parent directory. Mutates process HOME, so it saves/restores
/// it; separate test binaries are separate processes, so this does not race
/// other test files.
#[test]
fn resolve_global_insights_db_uses_home_and_creates_parent() {
    let tmp = TempDir::new().unwrap();
    let saved_home = std::env::var_os("HOME");
    let saved_userprofile = std::env::var_os("USERPROFILE");

    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("USERPROFILE"); // ensure HOME is the source on all platforms

    let got = resolve_global_insights_db();

    // Restore env before any assertion can panic and leak state.
    match saved_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
    match saved_userprofile {
        Some(u) => std::env::set_var("USERPROFILE", u),
        None => std::env::remove_var("USERPROFILE"),
    }

    let got = got.expect("resolver succeeds when HOME is set");
    let expected = tmp.path().join(".claude").join("knowledge").join("insights.db");
    assert_eq!(got, expected);
    assert!(
        got.parent().unwrap().exists(),
        "resolver created the parent dir <home>/.claude/knowledge/"
    );
}
