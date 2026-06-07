//! cli-to-cli-routing Slice 1 — schema v5→v6 migration tests.
//!
//! Coverage (QA cases TC-C2C-17.1, TC-C2C-17.2 + UC-C2C-1-EC3 backfill):
//! - v6 migration adds 5 C2C columns (project_id, branch, working_dir,
//!   feature_description, dnd_until_ts) to agent_registry
//! - v6 migration creates agent_registry_project_id_idx index
//! - v6 migration is idempotent (second run is a no-op)
//! - all 9 base v5 columns + 6 routing-migration columns survive (regression)
//! - legacy rows inserted via base-column INSERT come through with NULL
//!   in the 5 new C2C columns (UC-C2C-1-EC3 backfill semantics)
//! - PRAGMA integrity_check returns "ok" post-migration
//!
//! Fixtures are hermetic — in-memory rusqlite::Connection per test, never
//! touching the on-disk chat.db. The migration function under test is
//! `claudebase::daemon::chat::ensure_chat_db_schema`, which chains
//! v5 base + apply_routing_migration + apply_pending_asks_migration +
//! apply_agent_registry_c2c_migration (Slice 1 of cli-to-cli-routing).

use claudebase::daemon::chat::ensure_chat_db_schema;
use rusqlite::Connection;

fn col_exists(conn: &Connection, table: &str, col: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, col],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

fn index_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
        rusqlite::params![name],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

// ----------------------------------------------------------------------------
// TC-C2C-17.1 — fresh DB through ensure_chat_db_schema gains all 5 C2C columns
// ----------------------------------------------------------------------------
#[test]
fn tc_c2c_17_1_v6_migration_adds_five_c2c_columns() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    assert!(
        col_exists(&conn, "agent_registry", "project_id"),
        "project_id column"
    );
    assert!(col_exists(&conn, "agent_registry", "branch"), "branch column");
    assert!(
        col_exists(&conn, "agent_registry", "working_dir"),
        "working_dir column"
    );
    assert!(
        col_exists(&conn, "agent_registry", "feature_description"),
        "feature_description column"
    );
    assert!(
        col_exists(&conn, "agent_registry", "dnd_until_ts"),
        "dnd_until_ts column"
    );
}

// ----------------------------------------------------------------------------
// TC-C2C-17.1 — agent_registry_project_id_idx exists for --project filter
// ----------------------------------------------------------------------------
#[test]
fn tc_c2c_17_1_project_id_index_exists() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    assert!(
        index_exists(&conn, "agent_registry_project_id_idx"),
        "agent_registry_project_id_idx index"
    );
}

// ----------------------------------------------------------------------------
// Regression — v6 migration MUST NOT drop or rename v5 columns
// ----------------------------------------------------------------------------
#[test]
fn v6_migration_preserves_all_v5_columns() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    // 9 base columns from CREATE TABLE agent_registry (chat.rs:443-453)
    for col in &[
        "agent_id",
        "agent_name",
        "connection_id",
        "chat_thread_id",
        "permission_relayer",
        "spawned_at",
        "last_pinged_at",
        "state",
        "metadata",
    ] {
        assert!(
            col_exists(&conn, "agent_registry", col),
            "base column {col} missing"
        );
    }
    // 6 routing-migration columns from apply_routing_migration (chat.rs:508-518)
    for col in &[
        "routing_chat_id",
        "routing_thread_id",
        "last_user_id",
        "host",
        "cwd",
        "pid",
    ] {
        assert!(
            col_exists(&conn, "agent_registry", col),
            "routing column {col} missing"
        );
    }
}

// ----------------------------------------------------------------------------
// TC-C2C-17.2 — migration is idempotent on the same connection
// ----------------------------------------------------------------------------
#[test]
fn tc_c2c_17_2_migration_is_idempotent() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("first apply");
    ensure_chat_db_schema(&conn).expect("second apply must not error");
    // C2C columns still exist (not dropped or duplicated)
    assert!(col_exists(&conn, "agent_registry", "project_id"));
    assert!(col_exists(&conn, "agent_registry", "dnd_until_ts"));
    // Index still exists
    assert!(index_exists(&conn, "agent_registry_project_id_idx"));
}

// ----------------------------------------------------------------------------
// UC-C2C-1-EC3 — legacy rows (inserted via base-column INSERT) have NULL in
// the 5 new C2C columns. Backfill semantics: existing rows surface in the
// daemon with NULL until Slice 3's agent_describe UPDATE populates them.
// ----------------------------------------------------------------------------
#[test]
fn uc_c2c_1_ec3_legacy_row_has_null_in_c2c_columns() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    // Insert the way pre-Slice-1 code would (only base columns).
    conn.execute(
        "INSERT INTO agent_registry \
         (agent_id, agent_name, connection_id, chat_thread_id, \
          permission_relayer, spawned_at, last_pinged_at, state, metadata) \
         VALUES ('legacy', 'mira', 'cid-0', NULL, NULL, 1, 1, 'alive', NULL)",
        [],
    )
    .expect("insert legacy row");
    let (pid, br, wd, fd, dnd): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) = conn
        .query_row(
            "SELECT project_id, branch, working_dir, feature_description, dnd_until_ts \
             FROM agent_registry WHERE agent_id = 'legacy'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("select c2c cols");
    assert_eq!(pid, None);
    assert_eq!(br, None);
    assert_eq!(wd, None);
    assert_eq!(fd, None);
    assert_eq!(dnd, None);
}

// ----------------------------------------------------------------------------
// Database integrity post-migration
// ----------------------------------------------------------------------------
#[test]
fn pragma_integrity_check_ok_after_migration() {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("integrity_check");
    assert_eq!(result, "ok");
}
