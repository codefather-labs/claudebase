//! cli-to-cli-routing Slice 3 — agent_describe + register-time identity tests.
//!
//! Coverage (QA cases TC-C2C-2.1..2.5, TC-C2C-7.1..7.3, TC-C2C-16.1, 16.2):
//!
//! - Register-time identity capture: handler stores project_id, branch,
//!   working_dir on the v6 columns when the bridge passes a `cwd` arg.
//! - Backward-compat: register without cwd leaves new columns NULL.
//! - agent_describe round-trip writes feature_description.
//! - agent_describe overwrites previous description (second call wins).
//! - agent_describe with optional `branch` updates branch via COALESCE
//!   pattern — None leaves the existing value untouched.
//! - lookup_agent_id_by_connection resolves the alive row bound to a
//!   given connection_id (the security primitive Slice 4 FR-C2C-4.6
//!   builds on for sender identity binding).
//! - capture_identity is idempotent on a re-register from the same
//!   connection (UPDATE … WHERE agent_id matches the new row).

use claudebase::daemon::agent_registry::{
    capture_identity, describe, lookup_agent_id_by_connection, register,
};
use claudebase::daemon::chat::ensure_chat_db_schema;
use rusqlite::Connection;

fn fresh_db() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    conn
}

#[test]
fn tc_c2c_16_1_capture_identity_persists_project_id_branch_working_dir() {
    let conn = fresh_db();
    register(&conn, "mira-cc1", "mira", "cid-0", None, None).expect("register");
    let updated = capture_identity(
        &conn,
        "mira-cc1",
        Some("github.com/codefather-labs/claudebase"),
        Some("feat/multi-agent-on-v0.6"),
        Some("C:\\Users\\madwh\\Documents\\claudebase"),
    )
    .expect("capture_identity");
    assert_eq!(updated, 1, "exactly one alive row updated");

    let (pid, br, wd): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT project_id, branch, working_dir \
             FROM agent_registry WHERE agent_id = 'mira-cc1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("select after capture");
    assert_eq!(pid.as_deref(), Some("github.com/codefather-labs/claudebase"));
    assert_eq!(br.as_deref(), Some("feat/multi-agent-on-v0.6"));
    assert_eq!(
        wd.as_deref(),
        Some("C:\\Users\\madwh\\Documents\\claudebase")
    );
}

#[test]
fn register_without_capture_identity_leaves_new_cols_null() {
    // Backward compat — older bridges that don't pass cwd register
    // without project_id/branch/working_dir.
    let conn = fresh_db();
    register(&conn, "mira-cc1", "mira", "cid-0", None, None).expect("register");
    let (pid, br, wd): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT project_id, branch, working_dir FROM agent_registry WHERE agent_id='mira-cc1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("select");
    assert_eq!(pid, None);
    assert_eq!(br, None);
    assert_eq!(wd, None);
}

#[test]
fn capture_identity_coalesce_preserves_existing_on_none() {
    // Partial updates via COALESCE — passing None for one field MUST
    // NOT clobber the existing value. Defends against rename / re-
    // register paths that may not re-emit every identity field.
    let conn = fresh_db();
    register(&conn, "mira-cc1", "mira", "cid-0", None, None).expect("register");
    capture_identity(
        &conn,
        "mira-cc1",
        Some("github.com/foo/bar"),
        Some("feat/a"),
        Some("/tmp/a"),
    )
    .expect("first capture");
    capture_identity(&conn, "mira-cc1", None, Some("feat/b"), None).expect("second capture");

    let (pid, br, wd): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT project_id, branch, working_dir FROM agent_registry WHERE agent_id='mira-cc1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("select");
    assert_eq!(pid.as_deref(), Some("github.com/foo/bar"), "preserved");
    assert_eq!(br.as_deref(), Some("feat/b"), "updated to new branch");
    assert_eq!(wd.as_deref(), Some("/tmp/a"), "preserved");
}

#[test]
fn tc_c2c_2_1_describe_roundtrip_updates_feature_description() {
    let conn = fresh_db();
    register(&conn, "mira-cc1", "mira", "cid-0", None, None).expect("register");
    let updated =
        describe(&conn, "mira-cc1", "Slice 3 — agent_describe handler", None).expect("describe");
    assert_eq!(updated, 1);
    let fd: Option<String> = conn
        .query_row(
            "SELECT feature_description FROM agent_registry WHERE agent_id='mira-cc1'",
            [],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(fd.as_deref(), Some("Slice 3 — agent_describe handler"));
}

#[test]
fn tc_c2c_2_2_describe_overwrites_previous_description() {
    let conn = fresh_db();
    register(&conn, "mira-cc1", "mira", "cid-0", None, None).expect("register");
    describe(&conn, "mira-cc1", "first", None).expect("first describe");
    describe(&conn, "mira-cc1", "second", None).expect("second describe");
    let fd: Option<String> = conn
        .query_row(
            "SELECT feature_description FROM agent_registry WHERE agent_id='mira-cc1'",
            [],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(fd.as_deref(), Some("second"));
}

#[test]
fn describe_with_branch_updates_via_coalesce() {
    let conn = fresh_db();
    register(&conn, "mira-cc1", "mira", "cid-0", None, None).expect("register");
    capture_identity(&conn, "mira-cc1", None, Some("feat/initial"), None).expect("seed branch");
    describe(&conn, "mira-cc1", "desc-1", Some("feat/updated")).expect("describe with branch");
    let (fd, br): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT feature_description, branch FROM agent_registry WHERE agent_id='mira-cc1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("select");
    assert_eq!(fd.as_deref(), Some("desc-1"));
    assert_eq!(br.as_deref(), Some("feat/updated"), "branch updated");
}

#[test]
fn describe_with_no_branch_arg_preserves_existing_branch() {
    let conn = fresh_db();
    register(&conn, "mira-cc1", "mira", "cid-0", None, None).expect("register");
    capture_identity(&conn, "mira-cc1", None, Some("feat/initial"), None).expect("seed branch");
    describe(&conn, "mira-cc1", "desc-1", None).expect("describe without branch");
    let br: Option<String> = conn
        .query_row(
            "SELECT branch FROM agent_registry WHERE agent_id='mira-cc1'",
            [],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(
        br.as_deref(),
        Some("feat/initial"),
        "branch preserved by COALESCE"
    );
}

#[test]
fn tc_c2c_7_1_lookup_agent_id_by_connection_finds_alive_row() {
    // The security primitive for FR-C2C-4.6 sender identity binding
    // (Slice 4): given a connection_id, return the agent_id of the
    // alive row bound to that connection. agent_describe uses this
    // pattern to refuse to update a row the caller doesn't own.
    let conn = fresh_db();
    register(&conn, "mira-cc1", "mira", "cid-aaa", None, None).expect("register A");
    register(&conn, "vela-cc2", "vela", "cid-bbb", None, None).expect("register B");
    let a = lookup_agent_id_by_connection(&conn, "cid-aaa").expect("lookup A");
    let b = lookup_agent_id_by_connection(&conn, "cid-bbb").expect("lookup B");
    let none = lookup_agent_id_by_connection(&conn, "cid-zzz").expect("lookup missing");
    assert_eq!(a.as_deref(), Some("mira-cc1"));
    assert_eq!(b.as_deref(), Some("vela-cc2"));
    assert_eq!(none, None);
}

#[test]
fn tc_c2c_2_e1_describe_missing_agent_returns_zero_rows_updated() {
    // Caller asks to describe a non-existent agent_id. The handler
    // surface (Slice 3 server.rs) maps this to a JSON-RPC error.
    // The DB-level primitive simply returns 0 rows updated.
    let conn = fresh_db();
    let updated = describe(&conn, "ghost-agent", "anything", None).expect("describe ghost");
    assert_eq!(updated, 0);
}
