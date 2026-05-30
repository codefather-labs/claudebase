//! Slice 5 — agent_registry table + state transitions.
//!
//! Schema v6 (defined in `chat.rs::ensure_chat_db_schema`) carries the
//! `agent_registry` table + 2 indexes (1 partial UNIQUE on
//! `(chat_thread_id, agent_name) WHERE state='alive' AND chat_thread_id IS NOT NULL`
//! per F-5.1 red-team finding, 1 routing index for Slice 7
//! `target_agent_id` resolution).
//!
//! State machine (STRUCTURAL-5-2):
//!   alive    → orphaned   (on connection EOF — bulk-UPDATE by connection_id)
//!   alive    → alive      (idempotent re-register from SAME connection_id)
//!   orphaned → alive      (re-register from NEW connection_id, same agent_id)
//!   alive    → dead       (agent_unregister)
//!   orphaned → dead       (agent_unregister OR agent_reap)
//!
//! Reconciliation of UC-5-EC-2 (idempotent same agent_id) and UC-5-EC-3
//! (different agent_id same name in alive thread) is a single
//! `INSERT...ON CONFLICT(agent_id) DO UPDATE` statement (STRUCTURAL-5-3):
//! ON CONFLICT on the agent_id primary key handles UC-5-EC-2;
//! the partial UNIQUE index on `(thread, name)` fires SQLITE_CONSTRAINT_UNIQUE
//! on UC-5-EC-3 BEFORE the ON CONFLICT clause runs, surfacing the
//! literal "UNIQUE constraint failed" string the implementer catches and
//! maps to the friendly error TC-5.9 expects.
//!
//! `connection_id` rendering invariant (STRUCTURAL-5-8): every INSERT
//! uses `connection_id.to_string()` (where `connection_id` is the
//! `Uuid::new_v4()` value generated per-connection in
//! `src/daemon/server.rs`). Every EOF UPDATE uses the SAME
//! `to_string()` render so the WHERE clause matches.

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::daemon::chat::now_millis;

/// The three states an agent can occupy. The DB CHECK constraint on
/// `state` enforces the same vocabulary; the Rust enum is the authority
/// for transition legality (transitions other than the ones listed in
/// the module docstring are forbidden — the caller MUST NOT bypass
/// these helpers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Alive,
    Orphaned,
    Dead,
}

impl AgentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentState::Alive => "alive",
            AgentState::Orphaned => "orphaned",
            AgentState::Dead => "dead",
        }
    }
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "alive" => Ok(AgentState::Alive),
            "orphaned" => Ok(AgentState::Orphaned),
            "dead" => Ok(AgentState::Dead),
            other => anyhow::bail!("unknown agent state: {other}"),
        }
    }
}

/// Outcome of a successful `agent_register` call.
#[derive(Debug, Clone)]
pub struct RegisterOutcome {
    pub spawned_at: i64,
}

/// Outcome of a successful `agent_unregister` call.
#[derive(Debug, Clone)]
pub struct UnregisterOutcome {
    pub previous_state: String,
}

/// One row of the agent_registry table returned by `list_alive`.
#[derive(Debug, Clone)]
pub struct AgentRow {
    pub agent_id: String,
    pub agent_name: String,
    pub chat_thread_id: Option<String>,
    pub spawned_at: i64,
    pub last_pinged_at: i64,
}

/// Outcome of a successful `agent_reap` call. The wire shape exposed
/// by the MCP tool is `{"reaped_count": N, "remaining_orphaned": N}` —
/// `reaped_count` (NOT `reaped`) per the TC-5.4 jq path
/// `.result.reaped_count`.
#[derive(Debug, Clone)]
pub struct ReapOutcome {
    pub reaped_count: usize,
    pub remaining_orphaned: usize,
}

/// Validate `agent_name` against the 1-64-char `[A-Za-z0-9_-]` charset
/// per STRUCTURAL-5-9. Prevents Unicode / control-char injection into
/// the unique-index lookup and PII leakage in tracing output.
pub fn validate_agent_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("agent_name must be 1-64 chars of [a-zA-Z0-9_-]");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("agent_name must be 1-64 chars of [a-zA-Z0-9_-]");
    }
    Ok(())
}

/// Register an agent OR re-register an existing agent_id (idempotent
/// per UC-5-EC-2). On `(chat_thread_id, agent_name)` collision with a
/// DIFFERENT agent_id the partial UNIQUE index fires
/// SQLITE_CONSTRAINT_UNIQUE; we map the rusqlite error string to the
/// friendly TC-5.9 error.
///
/// The `metadata` JSON value (if any) is serialised to TEXT for storage;
/// SQLite's JSON1 hint type is intentionally not used (TEXT + serde
/// round-trip is the canonical pattern).
pub fn register(
    conn: &Connection,
    agent_id: &str,
    agent_name: &str,
    connection_id: &str,
    chat_thread_id: Option<&str>,
    metadata: Option<&Value>,
) -> anyhow::Result<RegisterOutcome> {
    validate_agent_name(agent_name).context("agent_register: validate name")?;
    let now = now_millis();
    let metadata_text = metadata.map(|v| v.to_string());
    let result = conn.execute(
        "INSERT INTO agent_registry \
         (agent_id, agent_name, connection_id, chat_thread_id, \
          permission_relayer, spawned_at, last_pinged_at, state, metadata) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5, 'alive', ?6) \
         ON CONFLICT(agent_id) DO UPDATE SET \
           agent_name = excluded.agent_name, \
           connection_id = excluded.connection_id, \
           chat_thread_id = excluded.chat_thread_id, \
           last_pinged_at = excluded.last_pinged_at, \
           state = 'alive', \
           metadata = excluded.metadata",
        params![agent_id, agent_name, connection_id, chat_thread_id, now, metadata_text],
    );
    match result {
        Ok(_) => Ok(RegisterOutcome { spawned_at: now }),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("UNIQUE constraint failed") {
                anyhow::bail!(
                    "agent_name already alive in thread; unregister first or use different name"
                )
            }
            Err(e.into())
        }
    }
}

/// Mark an agent as `dead` (terminal state). Returns the previous
/// state so the caller can report "was alive" vs "was orphaned" vs
/// "absent" (idempotent — no error when the row never existed, per
/// UC-5-A).
///
/// Race: a concurrent unregister between our SELECT and UPDATE would
/// land both transitions to 'dead'; the result is the same end state.
/// `previous_state` may lie under that race (both callers report the
/// pre-pre-state) — acceptable for v1 since the end-state is correct
/// and SQLite serialises writes at the WAL boundary.
pub fn unregister(conn: &Connection, agent_id: &str) -> anyhow::Result<UnregisterOutcome> {
    let previous: Option<String> = conn
        .query_row(
            "SELECT state FROM agent_registry WHERE agent_id=?1",
            params![agent_id],
            |row| row.get(0),
        )
        .optional()?;
    match previous {
        Some(prev_state) => {
            conn.execute(
                "UPDATE agent_registry SET state='dead' WHERE agent_id=?1",
                params![agent_id],
            )?;
            Ok(UnregisterOutcome {
                previous_state: prev_state,
            })
        }
        None => Ok(UnregisterOutcome {
            previous_state: "absent".to_string(),
        }),
    }
}

/// List all rows where state='alive', optionally filtered by thread.
/// Ordered newest-pinged first so Slice 7's routing can prefer
/// recently-active agents on race.
pub fn list_alive(conn: &Connection, thread: Option<&str>) -> anyhow::Result<Vec<AgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, agent_name, chat_thread_id, spawned_at, last_pinged_at \
         FROM agent_registry \
         WHERE state='alive' \
           AND (?1 IS NULL OR chat_thread_id = ?1) \
         ORDER BY last_pinged_at DESC \
         LIMIT 1000",
    )?;
    let rows: rusqlite::Result<Vec<AgentRow>> = stmt
        .query_map(params![thread], |row| {
            Ok(AgentRow {
                agent_id: row.get(0)?,
                agent_name: row.get(1)?,
                chat_thread_id: row.get(2)?,
                spawned_at: row.get(3)?,
                last_pinged_at: row.get(4)?,
            })
        })?
        .collect();
    Ok(rows?)
}

/// Return `true` iff `agent_id` names an alive (state='alive') row.
///
/// Parameterised query — `agent_id` is bound, never interpolated, so an
/// empty string or a SQL-injection payload (UC-TMC-2-EC1) is matched as
/// a literal value and simply finds no row (returns `false`) rather than
/// altering the table. Orphaned and dead rows return `false` because the
/// WHERE clause pins `state='alive'`.
pub fn is_alive(conn: &Connection, agent_id: &str) -> anyhow::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM agent_registry WHERE agent_id=?1 AND state='alive' LIMIT 1",
            params![agent_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Pick a single alive agent to route to when a chat has no explicit
/// binding (Slice 2 routing-tree step 4 fallback). Two-pass:
///
/// 1. **prefer_role pass** — if `prefer_role` is `Some(role)`, return the
///    first alive row whose `agent_name` contains `role` as a substring.
///    The `agent_registry` table has no dedicated `role` column (the
///    `AgentRow` struct exposes `agent_id, agent_name, chat_thread_id,
///    spawned_at, last_pinged_at` only); the role is carried in the
///    `agent_name` by convention (e.g. "orchestrator-main"), which the QA
///    cases TC-TMC-3.1/3.2/3.4 assert against directly. We therefore match
///    `prefer_role` against `agent_name` via a parameterised `LIKE
///    '%'||?||'%'` predicate. The LIKE wildcards `%`/`_` inside `role`
///    are NOT escaped — `prefer_role` is an internal caller-supplied
///    constant ("orchestrator"), never user input, so wildcard injection
///    is not a threat surface here.
/// 2. **any-alive fallback** — if the prefer_role pass finds nothing (or
///    `prefer_role` is `None`), return the first alive row regardless of
///    name.
///
/// Both passes order by `agent_id ASC` so the tiebreak between two
/// equally-eligible agents is **deterministic** (UC-TMC-3-EC1 /
/// TC-TMC-3.4 require two consecutive calls to return the same row).
/// `agent_id` is the primary key, hence total-ordered and stable across
/// calls; `spawned_at` was rejected as the sort key because two agents
/// can share a millisecond timestamp, reintroducing nondeterminism.
///
/// Returns `None` when no alive row exists (UC-TMC-3-A2 / TC-TMC-3.3).
/// `thread`, when `Some`, scopes both passes to that `chat_thread_id`.
pub fn first_alive(
    conn: &Connection,
    thread: Option<&str>,
    prefer_role: Option<&str>,
) -> anyhow::Result<Option<AgentRow>> {
    let map_row = |row: &rusqlite::Row| -> rusqlite::Result<AgentRow> {
        Ok(AgentRow {
            agent_id: row.get(0)?,
            agent_name: row.get(1)?,
            chat_thread_id: row.get(2)?,
            spawned_at: row.get(3)?,
            last_pinged_at: row.get(4)?,
        })
    };

    // Pass 1 — prefer_role substring match on agent_name.
    if let Some(role) = prefer_role {
        let like = format!("%{role}%");
        let hit: Option<AgentRow> = conn
            .query_row(
                "SELECT agent_id, agent_name, chat_thread_id, spawned_at, last_pinged_at \
                 FROM agent_registry \
                 WHERE state='alive' \
                   AND (?1 IS NULL OR chat_thread_id = ?1) \
                   AND agent_name LIKE ?2 \
                 ORDER BY agent_id ASC \
                 LIMIT 1",
                params![thread, like],
                map_row,
            )
            .optional()?;
        if hit.is_some() {
            return Ok(hit);
        }
    }

    // Pass 2 — any alive row, deterministic by agent_id.
    let any: Option<AgentRow> = conn
        .query_row(
            "SELECT agent_id, agent_name, chat_thread_id, spawned_at, last_pinged_at \
             FROM agent_registry \
             WHERE state='alive' \
               AND (?1 IS NULL OR chat_thread_id = ?1) \
             ORDER BY agent_id ASC \
             LIMIT 1",
            params![thread],
            map_row,
        )
        .optional()?;
    Ok(any)
}

/// Reap orphaned rows older than `older_than_secs` seconds. Default
/// 86400 (24 hours). Returns `reaped_count` (the number of rows
/// transitioned orphaned→dead) and `remaining_orphaned` (rows still in
/// state='orphaned' after the reap).
///
/// **Unit note (insight #12)**: the wire param is in SECONDS per
/// FR-ACD-5.4 PRD spec; `last_pinged_at` is stored in MILLISECONDS
/// (`now_millis()` convention). The cutoff arithmetic multiplies by
/// 1000 here so the WHERE clause is unit-coherent.
pub fn reap(conn: &Connection, older_than_secs: Option<i64>) -> anyhow::Result<ReapOutcome> {
    let secs = older_than_secs.unwrap_or(86_400);
    let cutoff_ms = now_millis().saturating_sub(secs.saturating_mul(1000));
    let reaped = conn.execute(
        "UPDATE agent_registry SET state='dead' \
         WHERE state='orphaned' AND last_pinged_at < ?1",
        params![cutoff_ms],
    )?;
    let remaining: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agent_registry WHERE state='orphaned'",
        [],
        |row| row.get(0),
    )?;
    Ok(ReapOutcome {
        reaped_count: reaped,
        remaining_orphaned: remaining as usize,
    })
}

/// Bulk-UPDATE all rows where connection_id matches and state='alive'
/// to state='orphaned'. Called from the connection-EOF hook in
/// `server.rs::handle_connection`. Single SQL statement so SQLite's
/// auto-commit provides atomicity.
pub fn mark_connection_orphaned(conn: &Connection, connection_id: &str) -> anyhow::Result<usize> {
    let updated = conn.execute(
        "UPDATE agent_registry SET state='orphaned' \
         WHERE connection_id=?1 AND state='alive'",
        params![connection_id],
    )?;
    Ok(updated)
}

/// Reap-on-boot: mark every state='alive' row as 'orphaned'. Run at
/// daemon startup because all connections that existed before restart
/// are gone. Slice 1b's `reap_on_boot_stub` body becomes this real path
/// now that schema v6 ensures the table exists.
pub fn reap_on_boot(conn: &Connection) -> anyhow::Result<usize> {
    let updated = conn.execute(
        "UPDATE agent_registry SET state='orphaned' WHERE state='alive'",
        [],
    )?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        crate::daemon::chat::ensure_chat_db_schema(&conn).expect("schema applied");
        conn
    }

    #[test]
    fn agent_state_round_trip() {
        for s in [AgentState::Alive, AgentState::Orphaned, AgentState::Dead] {
            assert_eq!(AgentState::parse(s.as_str()).unwrap(), s);
        }
        assert!(AgentState::parse("bogus").is_err());
    }

    #[test]
    fn validate_agent_name_accepts_valid() {
        for n in ["planner", "PLANNER", "x", "_a", "-z", "a-b_c-1"] {
            assert!(validate_agent_name(n).is_ok(), "should accept '{n}'");
        }
        let long_ok = "a".repeat(64);
        assert!(validate_agent_name(&long_ok).is_ok());
    }

    #[test]
    fn validate_agent_name_rejects_invalid() {
        for n in ["", "with space", "слово", "name!", "n.dot"] {
            assert!(validate_agent_name(n).is_err(), "should reject '{n}'");
        }
        let too_long = "a".repeat(65);
        assert!(validate_agent_name(&too_long).is_err());
    }

    #[test]
    fn register_creates_alive_row() {
        let conn = open_test_conn();
        let out = register(
            &conn,
            "planner-abc123",
            "planner",
            "conn-xyz",
            Some("telegram:1001"),
            Some(&json!({"role": "tactical"})),
        )
        .unwrap();
        assert!(out.spawned_at > 0);
        let state: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='planner-abc123'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "alive");
    }

    #[test]
    fn register_idempotent_same_agent_id() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "conn-x", Some("t-1"), None).unwrap();
        // Re-register from a DIFFERENT connection — UC-5-EC-2 expects
        // a successful re-bind (orphaned → alive flow if state was
        // orphaned; alive → alive otherwise).
        register(&conn, "a-1", "planner", "conn-y", Some("t-1"), None).unwrap();
        let cid: String = conn
            .query_row(
                "SELECT connection_id FROM agent_registry WHERE agent_id='a-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cid, "conn-y", "re-register should update connection_id");
    }

    #[test]
    fn register_rejects_conflict_different_agent_id_same_name_same_thread() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "conn-x", Some("t-1"), None).unwrap();
        // Different agent_id, same (thread, name) — partial UNIQUE index
        // fires; UC-5-EC-3, TC-5.9.
        let err = register(&conn, "a-2", "planner", "conn-y", Some("t-1"), None).unwrap_err();
        assert!(
            err.to_string()
                .contains("agent_name already alive in thread"),
            "expected friendly TC-5.9 error, got: {err}"
        );
    }

    #[test]
    fn register_allows_same_name_different_thread() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "conn-x", Some("t-1"), None).unwrap();
        // Different thread — UNIQUE INDEX scope is per-thread.
        register(&conn, "a-2", "planner", "conn-x", Some("t-2"), None).unwrap();
    }

    #[test]
    fn unregister_marks_dead_and_releases_index_slot() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "conn-x", Some("t-1"), None).unwrap();
        unregister(&conn, "a-1").unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='a-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "dead");
        // After dead, (thread, name) slot is released — a fresh
        // register with a different agent_id succeeds.
        register(&conn, "a-2", "planner", "conn-y", Some("t-1"), None).unwrap();
    }

    #[test]
    fn list_alive_filters_by_thread() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "c-x", Some("t-1"), None).unwrap();
        register(&conn, "a-2", "reflection", "c-x", Some("t-2"), None).unwrap();
        let t1 = list_alive(&conn, Some("t-1")).unwrap();
        assert_eq!(t1.len(), 1);
        assert_eq!(t1[0].agent_id, "a-1");
        let all = list_alive(&conn, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn reap_drops_orphaned_older_than_cutoff() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "c-x", Some("t-1"), None).unwrap();
        // Backdate last_pinged_at to 25 hours ago AND set state to orphaned.
        let two_days_ago_ms = now_millis() - 25 * 60 * 60 * 1000;
        conn.execute(
            "UPDATE agent_registry SET state='orphaned', last_pinged_at=?1 WHERE agent_id='a-1'",
            params![two_days_ago_ms],
        )
        .unwrap();
        // Reap with default 24h cutoff.
        let out = reap(&conn, None).unwrap();
        assert_eq!(out.reaped_count, 1);
        assert_eq!(out.remaining_orphaned, 0);
    }

    #[test]
    fn reap_preserves_fresh_orphans() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "c-x", Some("t-1"), None).unwrap();
        // Mark orphaned but keep last_pinged_at recent.
        conn.execute(
            "UPDATE agent_registry SET state='orphaned' WHERE agent_id='a-1'",
            [],
        )
        .unwrap();
        let out = reap(&conn, Some(86400)).unwrap();
        assert_eq!(out.reaped_count, 0);
        assert_eq!(out.remaining_orphaned, 1);
    }

    #[test]
    fn mark_connection_orphaned_only_touches_matching_connection() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "conn-a", Some("t-1"), None).unwrap();
        register(&conn, "a-2", "reflection", "conn-b", Some("t-2"), None).unwrap();
        let updated = mark_connection_orphaned(&conn, "conn-a").unwrap();
        assert_eq!(updated, 1);
        let s1: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='a-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let s2: String = conn
            .query_row(
                "SELECT state FROM agent_registry WHERE agent_id='a-2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(s1, "orphaned");
        assert_eq!(s2, "alive");
    }

    #[test]
    fn reap_on_boot_marks_all_alive_as_orphaned() {
        let conn = open_test_conn();
        register(&conn, "a-1", "planner", "c-x", Some("t-1"), None).unwrap();
        register(&conn, "a-2", "reflection", "c-y", Some("t-2"), None).unwrap();
        let updated = reap_on_boot(&conn).unwrap();
        assert_eq!(updated, 2);
        for id in ["a-1", "a-2"] {
            let s: String = conn
                .query_row(
                    "SELECT state FROM agent_registry WHERE agent_id=?1",
                    params![id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(s, "orphaned");
        }
    }

    #[test]
    fn state_check_rejects_unknown_value() {
        let conn = open_test_conn();
        let err = conn
            .execute(
                "INSERT INTO agent_registry \
                 (agent_id, agent_name, connection_id, chat_thread_id, \
                  permission_relayer, spawned_at, last_pinged_at, state, metadata) \
                 VALUES ('x', 'planner', 'c', NULL, NULL, 0, 0, 'bogus', NULL)",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("CHECK constraint failed"),
            "expected DB-layer CHECK rejection, got: {err}"
        );
    }

    // ---- telegram-multi-cli Slice 1: is_alive + first_alive ----

    /// TC-TMC-2.1: is_alive returns true for a registered (alive) agent.
    #[test]
    fn is_alive_returns_true_for_registered() {
        let conn = open_test_conn();
        register(&conn, "agent-id-abc", "planner", "c-x", Some("t-1"), None).unwrap();
        assert!(is_alive(&conn, "agent-id-abc").unwrap());
    }

    /// TC-TMC-2.2: is_alive returns false for an unknown agent_id.
    #[test]
    fn is_alive_returns_false_for_unknown() {
        let conn = open_test_conn();
        assert!(!is_alive(&conn, "nonexistent-id").unwrap());
    }

    /// TC-TMC-2.3: is_alive returns false for an orphaned agent.
    #[test]
    fn is_alive_returns_false_for_orphaned() {
        let conn = open_test_conn();
        register(&conn, "agent-id-orphaned", "planner", "c-x", Some("t-1"), None).unwrap();
        mark_connection_orphaned(&conn, "c-x").unwrap();
        assert!(!is_alive(&conn, "agent-id-orphaned").unwrap());
    }

    /// TC-TMC-2.4: is_alive on an empty string and a SQL-injection payload
    /// both return false WITHOUT modifying agent_registry (parameterised
    /// query). UC-TMC-2-EC1.
    #[test]
    fn is_alive_rejects_malformed_ids() {
        let conn = open_test_conn();
        register(&conn, "real-agent", "planner", "c-x", Some("t-1"), None).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_registry", [], |r| r.get(0))
            .unwrap();
        assert!(!is_alive(&conn, "").unwrap());
        assert!(!is_alive(&conn, "'; DROP TABLE agent_registry;--").unwrap());
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_registry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "table must be untouched by malformed ids");
        // The legitimate row is still alive — table survived.
        assert!(is_alive(&conn, "real-agent").unwrap());
    }

    /// TC-TMC-3.1: first_alive prefers the prefer_role substring match.
    #[test]
    fn first_alive_prefers_role() {
        let conn = open_test_conn();
        register(&conn, "id-orch", "orchestrator-main", "c-x", Some("t-1"), None).unwrap();
        register(&conn, "id-work", "worker", "c-y", Some("t-2"), None).unwrap();
        let hit = first_alive(&conn, None, Some("orchestrator")).unwrap().unwrap();
        assert!(
            hit.agent_name.contains("orchestrator"),
            "expected an orchestrator row, got {}",
            hit.agent_name
        );
        assert_eq!(hit.agent_id, "id-orch");
    }

    /// TC-TMC-3.2: first_alive falls back to any alive agent when no
    /// prefer_role match exists.
    #[test]
    fn first_alive_fallback_no_role_match() {
        let conn = open_test_conn();
        register(&conn, "id-work", "worker", "c-y", Some("t-2"), None).unwrap();
        let hit = first_alive(&conn, None, Some("orchestrator")).unwrap().unwrap();
        assert_eq!(hit.agent_name, "worker");
        assert_eq!(hit.agent_id, "id-work");
    }

    /// TC-TMC-3.3: first_alive returns None when there are zero alive rows.
    #[test]
    fn first_alive_returns_none_when_empty() {
        let conn = open_test_conn();
        assert!(first_alive(&conn, None, Some("orchestrator")).unwrap().is_none());
        // Also None when the only rows are orphaned.
        register(&conn, "id-x", "worker", "c-x", Some("t-1"), None).unwrap();
        mark_connection_orphaned(&conn, "c-x").unwrap();
        assert!(first_alive(&conn, None, Some("orchestrator")).unwrap().is_none());
    }

    /// TC-TMC-3.4: with two equally-preferred agents, two consecutive
    /// first_alive calls return the SAME row (deterministic agent_id sort).
    #[test]
    fn first_alive_deterministic_tiebreak() {
        let conn = open_test_conn();
        register(&conn, "id-orch-b", "orchestrator-b", "c-x", Some("t-1"), None).unwrap();
        register(&conn, "id-orch-a", "orchestrator-a", "c-y", Some("t-2"), None).unwrap();
        let first = first_alive(&conn, None, Some("orchestrator")).unwrap().unwrap();
        let second = first_alive(&conn, None, Some("orchestrator")).unwrap().unwrap();
        assert_eq!(first.agent_id, second.agent_id);
        // ORDER BY agent_id ASC → "id-orch-a" wins over "id-orch-b".
        assert_eq!(first.agent_id, "id-orch-a");
    }

    /// first_alive with prefer_role=None returns the first alive agent by
    /// deterministic agent_id sort.
    #[test]
    fn first_alive_no_prefer_role_returns_first_alive() {
        let conn = open_test_conn();
        register(&conn, "id-b", "worker-b", "c-x", Some("t-1"), None).unwrap();
        register(&conn, "id-a", "worker-a", "c-y", Some("t-2"), None).unwrap();
        let hit = first_alive(&conn, None, None).unwrap().unwrap();
        assert_eq!(hit.agent_id, "id-a");
    }

    /// first_alive scopes to a thread when `thread` is Some.
    #[test]
    fn first_alive_scopes_to_thread() {
        let conn = open_test_conn();
        register(&conn, "id-t1", "orchestrator-x", "c-x", Some("t-1"), None).unwrap();
        register(&conn, "id-t2", "orchestrator-y", "c-y", Some("t-2"), None).unwrap();
        let hit = first_alive(&conn, Some("t-2"), Some("orchestrator"))
            .unwrap()
            .unwrap();
        assert_eq!(hit.agent_id, "id-t2");
    }
}
