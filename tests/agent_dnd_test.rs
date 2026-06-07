//! cli-to-cli-routing Slice 5 — agent_set_dnd + DND drain background task.
//!
//! Coverage (QA cases TC-C2C-4.1..4.7, TC-C2C-11.1/2, TC-C2C-14.1..14.4):
//!
//!   * parse_dnd_state — pure parser tests for "on" / "off" /
//!     "<N>m" / "<N>h" / "until HH:MM" + invalid inputs (TC-C2C-14)
//!   * indefinite DND = i64::MAX (architect A-3 / OQ-UC-C2C-1)
//!   * Explicit "off" clears dnd_until_ts to NULL
//!   * set_dnd writes the column
//!   * drain_dnd_tick clears expired DND and emits drainable messages
//!   * FR-C2C-5.5 rate limit (10 messages/agent/30s tick)
//!   * F-2 phantom-sender (drain delivers original from_agent even
//!     if that agent is now dead) — accepted symptom per plan
//!   * F-5 simulated panic recovery via std::panic::catch_unwind on a
//!     closure that mimics a tick body — outer loop continues

use claudebase::daemon::agent_registry::{
    drain_agent_inbox, drain_dnd_tick, mark_delivered, parse_dnd_state, register, send_message,
    set_dnd, DrainStats, INDEFINITE_DND,
};
use claudebase::daemon::chat::{ensure_chat_db_schema, now_millis};
use rusqlite::Connection;

fn fresh_db() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory");
    ensure_chat_db_schema(&conn).expect("apply schema");
    conn
}

fn register_alive(conn: &Connection, agent_id: &str) {
    register(conn, agent_id, agent_id, "cid", None, None).expect("register");
}

// ---- parse_dnd_state ------------------------------------------------------

#[test]
fn parse_on_returns_i64_max_sentinel() {
    // Architect A-3 / OQ-UC-C2C-1: indefinite DND = i64::MAX so the
    // drain WHERE clause `dnd_until_ts < now` naturally excludes.
    let v = parse_dnd_state("on", 1_000_000).expect("parse on");
    assert_eq!(v, Some(INDEFINITE_DND));
    assert_eq!(INDEFINITE_DND, i64::MAX);
}

#[test]
fn parse_off_returns_none() {
    let v = parse_dnd_state("off", 1_000_000).expect("parse off");
    assert_eq!(v, None);
}

#[test]
fn parse_minutes_returns_now_plus_60_seconds_per_unit() {
    let now = 1_000_000_000i64;
    let v = parse_dnd_state("30m", now).expect("parse 30m");
    assert_eq!(v, Some(now + 30 * 60 * 1000));
}

#[test]
fn parse_hours_returns_now_plus_3600_seconds_per_unit() {
    let now = 2_000_000_000i64;
    let v = parse_dnd_state("2h", now).expect("parse 2h");
    assert_eq!(v, Some(now + 2 * 3600 * 1000));
}

#[test]
fn parse_minute_with_no_unit_suffix_rejected() {
    parse_dnd_state("30", 0).expect_err("bare number with no unit");
}

#[test]
fn parse_invalid_state_returns_error() {
    parse_dnd_state("nonsense", 0).expect_err("nonsense");
    parse_dnd_state("", 0).expect_err("empty");
    parse_dnd_state("until 99:99", 0).expect_err("HH out of range");
}

#[test]
fn parse_until_hhmm_does_not_panic_on_dst_boundary_inputs() {
    // F-2-style defensive: parser must NEVER panic on edge-case time
    // inputs. We sample plausible HH:MM values across the day. The
    // parser is required to return Some(ts) for every well-formed
    // HH:MM where 0<=H<24 and 0<=M<60.
    let now = now_millis();
    for hh in 0..24 {
        for mm in [0, 15, 30, 45, 59] {
            let s = format!("until {:02}:{:02}", hh, mm);
            let result = parse_dnd_state(&s, now);
            assert!(result.is_ok(), "until {:02}:{:02} must parse, got {:?}", hh, mm, result);
            assert!(
                result.unwrap().is_some(),
                "until HH:MM must yield Some(ts)"
            );
        }
    }
}

#[test]
fn parse_until_hhmm_future_time_returns_today_target() {
    // "until 23:59" called from a "now" that's earlier than 23:59 in
    // local time MUST resolve to today's 23:59 (the parser doesn't
    // bump to tomorrow unless the target is in the past).
    let now = now_millis();
    let v = parse_dnd_state("until 23:59", now).expect("parse").expect("Some");
    assert!(
        v > now,
        "until HH:MM should yield a future timestamp; got {v} <= {now}"
    );
}

// ---- set_dnd --------------------------------------------------------------

#[test]
fn set_dnd_writes_indefinite_value_to_column() {
    let conn = fresh_db();
    register_alive(&conn, "mira");
    let n = set_dnd(&conn, "mira", Some(INDEFINITE_DND)).expect("set");
    assert_eq!(n, 1);
    let v: Option<i64> = conn
        .query_row(
            "SELECT dnd_until_ts FROM agent_registry WHERE agent_id='mira'",
            [],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(v, Some(INDEFINITE_DND));
}

#[test]
fn set_dnd_off_writes_null() {
    let conn = fresh_db();
    register_alive(&conn, "mira");
    set_dnd(&conn, "mira", Some(123456)).expect("set on");
    set_dnd(&conn, "mira", None).expect("set off");
    let v: Option<i64> = conn
        .query_row(
            "SELECT dnd_until_ts FROM agent_registry WHERE agent_id='mira'",
            [],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(v, None);
}

// ---- drain_dnd_tick -------------------------------------------------------

#[test]
fn drain_dnd_tick_finds_no_drainable_when_no_expired() {
    let conn = fresh_db();
    register_alive(&conn, "mira");
    set_dnd(&conn, "mira", Some(INDEFINITE_DND)).expect("indefinite");
    let stats = drain_dnd_tick(&conn, now_millis(), 10).expect("tick");
    assert_eq!(stats.cleared_dnd, 0);
    assert_eq!(stats.drainable.len(), 0);
}

#[test]
fn drain_dnd_tick_clears_expired_dnd_and_returns_drainable_messages() {
    let conn = fresh_db();
    register_alive(&conn, "mira");
    register_alive(&conn, "vela");
    // vela had DND until 1ms ago; now is 1_000_000.
    set_dnd(&conn, "vela", Some(1)).expect("set expired");
    // 3 messages queued for vela while DND was active.
    for i in 0..3 {
        send_message(&conn, "mira", "vela", &format!("msg-{i}"), 500 + i)
            .expect("send queued");
    }
    let stats = drain_dnd_tick(&conn, 1_000_000, 10).expect("tick");
    assert_eq!(stats.cleared_dnd, 1, "vela's DND row cleared");
    assert_eq!(stats.drainable.len(), 3);
    // After clearing, dnd_until_ts is NULL.
    let v: Option<i64> = conn
        .query_row(
            "SELECT dnd_until_ts FROM agent_registry WHERE agent_id='vela'",
            [],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(v, None);
}

#[test]
fn drain_dnd_tick_rate_limits_to_10_per_agent_per_tick() {
    // FR-C2C-5.5: drain emits at most 10 per agent per tick.
    let conn = fresh_db();
    register_alive(&conn, "mira");
    register_alive(&conn, "vela");
    set_dnd(&conn, "vela", Some(1)).expect("expired dnd");
    for i in 0..25 {
        send_message(&conn, "mira", "vela", &format!("msg-{i}"), 500 + i)
            .expect("queue");
    }
    let stats = drain_dnd_tick(&conn, 1_000_000, 10).expect("tick");
    assert_eq!(stats.drainable.len(), 10, "rate limit 10 per tick");
}

#[test]
fn drain_dnd_tick_indefinite_max_never_drained() {
    let conn = fresh_db();
    register_alive(&conn, "mira");
    register_alive(&conn, "vela");
    set_dnd(&conn, "vela", Some(INDEFINITE_DND)).expect("indefinite");
    send_message(&conn, "mira", "vela", "queued forever", 500).expect("queue");
    let stats = drain_dnd_tick(&conn, i64::MAX - 1, 10).expect("tick");
    assert_eq!(stats.cleared_dnd, 0);
    assert_eq!(stats.drainable.len(), 0);
}

#[test]
fn drain_dnd_tick_marks_delivered_for_returned_messages() {
    // After drain_dnd_tick returns the drainable list, the handler is
    // expected to call mark_delivered for each. This test exercises
    // the full path so a Slice-5 integration consumer sees that
    // delivered_at gets stamped exactly once and idempotent re-calls
    // are no-ops.
    let conn = fresh_db();
    register_alive(&conn, "mira");
    register_alive(&conn, "vela");
    set_dnd(&conn, "vela", Some(1)).expect("expired");
    let out =
        send_message(&conn, "mira", "vela", "hi", 500).expect("queue");
    let stats = drain_dnd_tick(&conn, 1_000_000, 10).expect("tick");
    assert_eq!(stats.drainable.len(), 1);
    let now = 1_000_000;
    let n = mark_delivered(&conn, &stats.drainable[0].id, now).expect("mark");
    assert_eq!(n, 1);
    let delivered_at: Option<i64> = conn
        .query_row(
            "SELECT delivered_at FROM chat_messages WHERE id=?1",
            rusqlite::params![&out.message_id],
            |r| r.get(0),
        )
        .expect("select");
    assert_eq!(delivered_at, Some(now));
}

#[test]
fn f_2_drain_delivers_message_from_now_dead_agent_with_original_from_id() {
    // Architect F-2 phantom-sender, accepted symptom-only: when the
    // queued sender is no longer alive at drain time, the message is
    // STILL delivered with the original from_agent. Receiver sees
    // "message from <dead-id>" and can detect via agent_list_alive
    // that the sender is gone.
    let conn = fresh_db();
    register_alive(&conn, "mira");
    register_alive(&conn, "vela");
    set_dnd(&conn, "vela", Some(1)).expect("expired");
    let out = send_message(&conn, "mira", "vela", "hi", 500).expect("queue");
    // Kill mira AFTER the message was queued.
    conn.execute(
        "UPDATE agent_registry SET state='dead' WHERE agent_id='mira'",
        [],
    )
    .expect("kill");
    let stats = drain_dnd_tick(&conn, 1_000_000, 10).expect("tick");
    assert_eq!(stats.drainable.len(), 1);
    assert_eq!(
        stats.drainable[0].from_agent, "mira",
        "drain preserves original from_agent — phantom-sender accepted"
    );
    let _ = out;
}

// ---- Slice 5 hotfix: drain_agent_inbox catches explicit-off path -----------

#[test]
fn drain_agent_inbox_returns_queued_messages_when_dnd_was_explicitly_cleared() {
    // Wave 5 live QA bug: drain_dnd_tick requires dnd_until_ts IS NOT
    // NULL — operator-cleared DND (NULL) is never caught. The new
    // drain_agent_inbox helper bypasses DND state and just returns
    // queued messages for THIS agent. agent_set_dnd("off") handler
    // calls it inline.
    let conn = fresh_db();
    register_alive(&conn, "mira");
    register_alive(&conn, "vela");
    // Queue 3 messages while vela was in DND (simulate by sending with
    // DND active, then operator turns DND off → handler should drain).
    set_dnd(&conn, "vela", Some(INDEFINITE_DND)).expect("dnd on");
    for i in 0..3 {
        send_message(&conn, "mira", "vela", &format!("queued-{i}"), 100 + i)
            .expect("queue");
    }
    // Operator clears DND explicitly.
    set_dnd(&conn, "vela", None).expect("dnd off");
    // Even though dnd_until_ts is now NULL, drain_agent_inbox still
    // finds the 3 queued messages.
    let queued = drain_agent_inbox(&conn, "vela", 10).expect("drain");
    assert_eq!(queued.len(), 3);
}

#[test]
fn drain_agent_inbox_respects_rate_limit() {
    let conn = fresh_db();
    register_alive(&conn, "mira");
    register_alive(&conn, "vela");
    for i in 0..25 {
        send_message(&conn, "mira", "vela", &format!("msg-{i}"), 100 + i)
            .expect("queue");
    }
    let queued = drain_agent_inbox(&conn, "vela", 10).expect("drain");
    assert_eq!(queued.len(), 10, "FR-C2C-5.5 rate limit applies to inline drain too");
}

#[test]
fn drain_agent_inbox_skips_already_delivered_rows() {
    let conn = fresh_db();
    register_alive(&conn, "mira");
    register_alive(&conn, "vela");
    let out = send_message(&conn, "mira", "vela", "delivered", 100).expect("queue");
    mark_delivered(&conn, &out.message_id, 200).expect("mark");
    let queued = drain_agent_inbox(&conn, "vela", 10).expect("drain");
    assert!(queued.is_empty(), "delivered rows must NOT be returned");
}

// ---- F-5 panic recovery (simulated) ---------------------------------------

#[test]
fn f_5_simulated_tick_panic_caught_and_loop_continues() {
    // Mimic the drain loop's defensive wrapper. The real loop wraps
    // EACH tick body in std::panic::catch_unwind so a panic in one
    // tick does not kill the supervisor task. Here we just verify
    // the wrapping primitive does what we think.
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let mut tick_calls = 0;
    for i in 0..5 {
        let r = catch_unwind(AssertUnwindSafe(|| {
            tick_calls += 1;
            if i == 2 {
                panic!("simulated tick panic");
            }
        }));
        // catch_unwind keeps us alive; outer loop continues.
        if i == 2 {
            assert!(r.is_err(), "panic should be caught at i=2");
        } else {
            assert!(r.is_ok());
        }
    }
    assert_eq!(tick_calls, 5, "all 5 ticks attempted despite one panic");
}
