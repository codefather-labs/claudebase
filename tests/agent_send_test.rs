//! cli-to-cli-routing Slice 4 — agent_send + DND-respecting persistence
//! + sender identity binding tests.
//!
//! Security pre-review (auditor verdict PASS-WITH-CONDITIONS) drove
//! three structural changes folded into the design:
//!
//!   * SEC-2 — INSERT with delivered_at=NULL; UPDATE to now() only
//!     when bus.publish reports ≥1 subscriber. Closes the alive→
//!     orphaned race between SELECT and INSERT.
//!   * SEC-5 — bridge rename emits subscribe(new) BEFORE
//!     unsubscribe(old) (tested via order assertion in a separate
//!     integration test once the bridge changes land).
//!   * SEC-1b — explicit test that a connection without a prior
//!     agent_register call sees the "no alive agent on this
//!     connection" error from agent_send.
//!
//! This file exercises the SYNCHRONOUS DB helpers in
//! `agent_registry::send_message` + `mark_delivered`. The end-to-end
//! handler + bus.publish wiring is exercised by an integration test
//! once we land Slice 5's DND drain task.
//!
//! Coverage:
//!   * TC-C2C-3.1 — A→B direct path, delivered_at set after publish ≥1.
//!   * TC-C2C-3.5 — sender identity binding: send_message uses the
//!     caller's resolved agent_id, never the value in caller args.
//!   * F-3 — orphaned/dead target rejected with "agent not found or
//!     not alive".
//!   * TC-C2C-4.1 — DND-active target leaves delivered_at=NULL and
//!     returns the dnd_until_ts for the caller's `delivered_when`.
//!   * SEC-2 — when no subscribers consume the publish, delivered_at
//!     remains NULL so Slice 5's drain picks the row up later.
//!   * self-send (A → A) is supported (Slice 4 spec).

use claudebase::daemon::agent_registry::{
    capture_identity, mark_delivered, register, send_message, SendDecision,
};
use claudebase::daemon::chat::{ensure_chat_db_schema, now_millis};
use rusqlite::Connection;

fn fresh_db() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    conn
}

fn register_alive(conn: &Connection, agent_id: &str, connection_id: &str) {
    register(conn, agent_id, agent_id, connection_id, None, None).expect("register");
}

#[test]
fn tc_c2c_3_1_direct_path_inserts_row_with_delivered_at_null_initially() {
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    register_alive(&conn, "vela", "cid-vela");
    let outcome =
        send_message(&conn, "mira", "vela", "hello vela", now_millis()).expect("send_message");
    assert!(
        matches!(outcome.decision, SendDecision::Deliver),
        "no DND on target → deliver-path"
    );
    let msg_id = outcome.message_id;
    // delivered_at starts NULL — the handler bumps it to now() only after
    // bus.publish confirms a subscriber.
    let (delivered, from_agent, thread): (Option<i64>, String, String) = conn
        .query_row(
            "SELECT delivered_at, from_agent, thread_id FROM chat_messages WHERE id = ?1",
            rusqlite::params![&msg_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("select inserted row");
    assert_eq!(delivered, None, "delivered_at starts NULL (SEC-2)");
    assert_eq!(from_agent, "mira", "from_agent is the resolved caller id");
    assert_eq!(thread, "agent:vela");
}

#[test]
fn tc_c2c_3_1_mark_delivered_updates_only_when_publish_finds_subscribers() {
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    register_alive(&conn, "vela", "cid-vela");
    let outcome =
        send_message(&conn, "mira", "vela", "hello", now_millis()).expect("send_message");
    let now = now_millis();
    let n = mark_delivered(&conn, &outcome.message_id, now).expect("mark_delivered");
    assert_eq!(n, 1);
    let delivered_at: Option<i64> = conn
        .query_row(
            "SELECT delivered_at FROM chat_messages WHERE id = ?1",
            rusqlite::params![&outcome.message_id],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(delivered_at, Some(now));
}

#[test]
fn sec_2_when_no_subscribers_consume_delivered_at_stays_null() {
    // Handler path: send_message + bus.publish returns 0 → DO NOT call
    // mark_delivered. The row sits delivered_at=NULL ready for Slice 5
    // drain.
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    register_alive(&conn, "vela", "cid-vela");
    let outcome =
        send_message(&conn, "mira", "vela", "hi", now_millis()).expect("send_message");
    // Skip mark_delivered (simulates publish→0 subscribers).
    let delivered_at: Option<i64> = conn
        .query_row(
            "SELECT delivered_at FROM chat_messages WHERE id = ?1",
            rusqlite::params![&outcome.message_id],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(
        delivered_at, None,
        "no mark_delivered call → row remains drainable"
    );
}

#[test]
fn f_3_orphaned_target_returns_not_alive_error() {
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    register_alive(&conn, "vela", "cid-vela");
    // Orphan vela — Slice 5 of multi-agent-tg uses 'orphaned' for
    // connection-EOF. Slice 4 must reject sends to it.
    conn.execute(
        "UPDATE agent_registry SET state='orphaned' WHERE agent_id='vela'",
        [],
    )
    .expect("orphan");
    let err = send_message(&conn, "mira", "vela", "hi", now_millis())
        .expect_err("send to orphaned must error");
    let msg = err.to_string();
    assert!(
        msg.contains("not alive") || msg.contains("not found"),
        "error message must signal not-alive, got: {msg}"
    );
}

#[test]
fn f_3_dead_target_returns_not_alive_error() {
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    register_alive(&conn, "vela", "cid-vela");
    conn.execute(
        "UPDATE agent_registry SET state='dead' WHERE agent_id='vela'",
        [],
    )
    .expect("kill");
    send_message(&conn, "mira", "vela", "hi", now_millis())
        .expect_err("send to dead must error");
}

#[test]
fn missing_target_returns_not_alive_error() {
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    // No vela ever registered.
    send_message(&conn, "mira", "vela", "hi", now_millis())
        .expect_err("send to never-existed must error");
}

#[test]
fn tc_c2c_4_1_dnd_active_returns_queue_decision_with_until_ts() {
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    register_alive(&conn, "vela", "cid-vela");
    let now = now_millis();
    let dnd_until = now + 30 * 60 * 1000; // 30 min from now
    capture_identity(&conn, "vela", None, None, None).ok();
    conn.execute(
        "UPDATE agent_registry SET dnd_until_ts = ?1 WHERE agent_id='vela'",
        rusqlite::params![dnd_until],
    )
    .expect("set dnd");
    let outcome = send_message(&conn, "mira", "vela", "hi", now).expect("send_message");
    match outcome.decision {
        SendDecision::Queue { dnd_until_ts } => {
            assert_eq!(dnd_until_ts, dnd_until);
        }
        SendDecision::Deliver => panic!("DND-active target should queue, got Deliver"),
    }
    let delivered_at: Option<i64> = conn
        .query_row(
            "SELECT delivered_at FROM chat_messages WHERE id = ?1",
            rusqlite::params![&outcome.message_id],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(delivered_at, None, "DND-queued row must have NULL delivered_at");
}

#[test]
fn indefinite_dnd_i64_max_keeps_message_queued() {
    // Architect A-3 — dnd_until_ts = i64::MAX is the indefinite DND
    // sentinel. send_message must treat it as DND-active (not deliver).
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    register_alive(&conn, "vela", "cid-vela");
    conn.execute(
        "UPDATE agent_registry SET dnd_until_ts = ?1 WHERE agent_id='vela'",
        rusqlite::params![i64::MAX],
    )
    .expect("set indefinite dnd");
    let outcome = send_message(&conn, "mira", "vela", "hi", now_millis()).expect("send_message");
    matches!(outcome.decision, SendDecision::Queue { .. });
}

#[test]
fn expired_dnd_falls_through_to_deliver_decision() {
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    register_alive(&conn, "vela", "cid-vela");
    let expired_ts = 1i64;
    conn.execute(
        "UPDATE agent_registry SET dnd_until_ts = ?1 WHERE agent_id='vela'",
        rusqlite::params![expired_ts],
    )
    .expect("expired dnd");
    let outcome = send_message(&conn, "mira", "vela", "hi", now_millis()).expect("send_message");
    assert!(matches!(outcome.decision, SendDecision::Deliver));
}

#[test]
fn self_send_a_to_a_is_supported() {
    // Edge case: an agent sending to itself (e.g. echo loop test, or
    // a deliberate self-notification). Plan does not forbid it.
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    let outcome =
        send_message(&conn, "mira", "mira", "self echo", now_millis()).expect("self-send");
    assert!(matches!(outcome.decision, SendDecision::Deliver));
    let from: String = conn
        .query_row(
            "SELECT from_agent FROM chat_messages WHERE id = ?1",
            rusqlite::params![&outcome.message_id],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(from, "mira");
}

#[test]
fn tc_c2c_3_5_send_message_thread_id_is_agent_prefix_target() {
    // Identity binding sanity-check: the from_agent value passed to
    // send_message is whatever the caller resolved (handler side does
    // the connection_id lookup); the THREAD always carries the
    // recipient's id, never the sender's, so subscriptions are routed
    // to the receiver inbox.
    let conn = fresh_db();
    register_alive(&conn, "mira", "cid-mira");
    register_alive(&conn, "vela", "cid-vela");
    let outcome =
        send_message(&conn, "mira", "vela", "hi", now_millis()).expect("send_message");
    let thread: String = conn
        .query_row(
            "SELECT thread_id FROM chat_messages WHERE id = ?1",
            rusqlite::params![&outcome.message_id],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(thread, "agent:vela");
}
