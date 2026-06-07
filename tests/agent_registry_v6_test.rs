//! cli-to-cli-routing Slice 1 — agent_registry struct extension tests.
//!
//! Coverage:
//! - AgentRow carries 5 new Option fields introduced by schema v6
//!   (project_id, branch, working_dir, feature_description, dnd_until_ts).
//! - list_alive returns rows with new fields populated as None — the
//!   extended SELECT lands in Slice 6 (claudebase agent list-alive CLI).
//! - Direct UPDATE / SELECT round-trip validates the v6 schema is wired
//!   for Slice 3's agent_describe and Slice 5's agent_set_dnd writes.
//! - Architect A-3 / OQ-UC-C2C-1: indefinite DND = dnd_until_ts = i64::MAX
//!   is naturally excluded by the drain predicate `dnd_until_ts < now()`.

use claudebase::daemon::agent_registry::{list_alive, register, AgentRow};
use claudebase::daemon::chat::ensure_chat_db_schema;
use rusqlite::Connection;

#[test]
fn agent_row_struct_accepts_five_new_optional_fields() {
    // Compile-time check the struct has the 5 new C2C fields.
    let row = AgentRow {
        agent_id: "a".to_string(),
        agent_name: "mira".to_string(),
        chat_thread_id: None,
        spawned_at: 1,
        last_pinged_at: 1,
        project_id: None,
        branch: None,
        working_dir: None,
        feature_description: None,
        dnd_until_ts: None,
    };
    assert_eq!(row.project_id, None);
    assert_eq!(row.branch, None);
    assert_eq!(row.working_dir, None);
    assert_eq!(row.feature_description, None);
    assert_eq!(row.dnd_until_ts, None);
}

#[test]
fn list_alive_returns_none_in_new_fields_until_slice_6() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    register(&conn, "a-id", "mira", "cid-0", None, None).expect("register");
    let rows = list_alive(&conn, None).expect("list_alive");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.agent_id, "a-id");
    // Slice 1 minimal extension — list_alive's SELECT does not yet pull
    // the new columns. Slice 6 (CLI list-alive surface) extends the
    // SELECT; until then the values are None at the struct boundary.
    assert_eq!(row.project_id, None);
    assert_eq!(row.branch, None);
    assert_eq!(row.working_dir, None);
    assert_eq!(row.feature_description, None);
    assert_eq!(row.dnd_until_ts, None);
}

#[test]
fn schema_v6_columns_accept_direct_writes() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    register(&conn, "a-id", "mira", "cid-0", None, None).expect("register");
    conn.execute(
        "UPDATE agent_registry SET \
           project_id = 'github.com/foo/bar', \
           branch = 'feat/x', \
           working_dir = '/tmp/x', \
           feature_description = 'cli-to-cli routing', \
           dnd_until_ts = ?1 \
         WHERE agent_id = 'a-id'",
        rusqlite::params![1_700_000_000_000i64],
    )
    .expect("update v6 columns");
    let (pid, br, wd, fd, dnd): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) = conn
        .query_row(
            "SELECT project_id, branch, working_dir, feature_description, dnd_until_ts \
             FROM agent_registry WHERE agent_id = 'a-id'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("select c2c cols");
    assert_eq!(pid.as_deref(), Some("github.com/foo/bar"));
    assert_eq!(br.as_deref(), Some("feat/x"));
    assert_eq!(wd.as_deref(), Some("/tmp/x"));
    assert_eq!(fd.as_deref(), Some("cli-to-cli routing"));
    assert_eq!(dnd, Some(1_700_000_000_000i64));
}

// Architect A-3 / OQ-UC-C2C-1 resolution: indefinite DND = i64::MAX.
// The drain task uses `dnd_until_ts < now()`; i64::MAX < any plausible now()
// is false, so indefinite-DND rows are naturally excluded without a
// special-case branch in the drain query.
#[test]
fn indefinite_dnd_sentinel_excluded_by_drain_predicate() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    register(&conn, "a-id", "mira", "cid-0", None, None).expect("register");
    conn.execute(
        "UPDATE agent_registry SET dnd_until_ts = ?1 WHERE agent_id = 'a-id'",
        rusqlite::params![i64::MAX],
    )
    .expect("set indefinite dnd");
    // Pick a "now" that is plausibly far in the future but still < i64::MAX.
    let now_year_2100_ms = 4_102_444_800_000i64;
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent_registry \
             WHERE dnd_until_ts < ?1 AND dnd_until_ts IS NOT NULL",
            rusqlite::params![now_year_2100_ms],
            |r| r.get(0),
        )
        .expect("drain count");
    assert_eq!(count, 0, "indefinite-DND row (i64::MAX) must be excluded");
}

#[test]
fn expired_dnd_row_selected_by_drain_predicate() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    register(&conn, "a-id", "mira", "cid-0", None, None).expect("register");
    let expired_ts = 1i64;
    conn.execute(
        "UPDATE agent_registry SET dnd_until_ts = ?1 WHERE agent_id = 'a-id'",
        rusqlite::params![expired_ts],
    )
    .expect("set expired dnd");
    let now = 1_000_000i64;
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent_registry \
             WHERE dnd_until_ts < ?1 AND dnd_until_ts IS NOT NULL",
            rusqlite::params![now],
            |r| r.get(0),
        )
        .expect("drain count");
    assert_eq!(count, 1, "expired DND row must be drain-selected");
}

#[test]
fn null_dnd_until_ts_excluded_from_drain() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    register(&conn, "a-id", "mira", "cid-0", None, None).expect("register");
    // Default state after register: dnd_until_ts IS NULL — agent is not
    // in DND. Drain MUST NOT touch these rows.
    let now = 1_000_000i64;
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent_registry \
             WHERE dnd_until_ts < ?1 AND dnd_until_ts IS NOT NULL",
            rusqlite::params![now],
            |r| r.get(0),
        )
        .expect("drain count");
    assert_eq!(
        count, 0,
        "NULL dnd_until_ts (no-DND) must NOT be drain-selected"
    );
}
