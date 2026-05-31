//! E2E routing integration tests — Slice 7 (TC-TMC-22.x, TC-TMC-19.1).
//!
//! ## Coverage
//!
//! | TC | Use case | What is tested |
//! |----|----------|---------------|
//! | TC-TMC-22.1 | UC-TMC-22 / FR-TMC-7.1 | Cross-chat routing: chat 111 → CLI-1 only; `tg_message_map` row inserted; reply-quote resolves to CLI-1 |
//! | TC-TMC-22.2 | UC-TMC-22-EC1 / AC-TMC-4 | Isolation: chat 111 → CLI-1 ONLY, chat 222 → CLI-2 ONLY, no cross-contamination |
//! | TC-TMC-22.3 | UC-TMC-22-A1 | SQLite durability: binding + message-map rows survive `open_chat_db()` reopen |
//! | TC-TMC-19.1 | UC-TMC-19 / NFR-TMC-7 | Daemon-not-running: plugin returns error to CLI-1 on `chat_ask`, does not crash |
//!
//! ## Design rationale
//!
//! TC-TMC-22.1/22.2/22.3 are in-process tests: they call `process_batch`
//! (pub) directly with a temporary SQLite DB, avoiding the need for a live
//! Telegram HTTP mock server. The routing decision tree is exercised through
//! the same code path the production long-poll loop uses. The ChatBus
//! subscribe-and-receive path is NOT exercised here (that requires the async
//! daemon + plugin process — covered by `chat_broadcast_test.rs` which
//! already verifies bus delivery). Instead, we assert on `BatchOutcome.notifications`
//! which is built by process_batch before publication to the bus: this
//! faithfully represents what the bus *would* publish (TC-TMC-22.2 isolation)
//! and what the `tg_message_map` row records (TC-TMC-22.1 tg_message_map
//! + reply-quote).
//!
//! TC-TMC-19.1 spawns only the plugin subprocess (no daemon), sends
//! `chat_ask` via the MCP bridge, and asserts the response is an error.
//!
//! ## Not covered here (manual / qa-cycle)
//!
//! - ChatBus notification delivery END-TO-END (plugin receives the
//!   `notifications/claude/channel` frame after `process_batch` publishes to
//!   the bus): this is what TC-TMC-22.1's step (a) actually tests in the QA
//!   doc ("bridge received notification"). We do NOT mock the ChatBus receiver
//!   here because that path is already covered by `chat_broadcast_test.rs`
//!   (TC-3.3). Adding a full daemon+plugin spawn here is possible but
//!   duplicates coverage of daemon bus wiring that `chat_broadcast_test.rs`
//!   owns. Flagged as manual / qa-cycle.
//! - `tg_message_map` insertion via `chat_reply` (requires `enqueue_outbound_tg_with_sender`
//!   to actually be called from within the async TG task): the ROUTING half
//!   (reply-quote lookups) is tested here via direct SQL seeding.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::time::timeout;

use claudebase::daemon::agent_registry::register;
use claudebase::daemon::chat::{ensure_chat_db_schema, ChatBus};
use claudebase::daemon::channel_state::{Access, DmPolicy};
use claudebase::daemon::telegram::{process_batch, Message, Update, User, Chat};

// ── in-process helpers ──────────────────────────────────────────────────────

/// Open an in-memory SQLite connection with full v7 schema applied.
fn open_test_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory db");
    ensure_chat_db_schema(&conn).expect("schema applied");
    conn
}

/// Open a file-backed SQLite connection with full v7 schema applied.
fn open_file_conn(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("file db open");
    ensure_chat_db_schema(&conn).expect("schema applied");
    conn
}

/// Seed an alive agent row (chat_thread_id = NULL so multiple CLIs coexist).
fn seed_agent(conn: &Connection, agent_id: &str, name: &str) {
    register(conn, agent_id, name, "conn-test", None, None).unwrap();
}

/// Seed an `active_cli_per_chat` binding row.
fn seed_binding(conn: &Connection, chat_id: i64, cli_name: &str, agent_id: &str) {
    conn.execute(
        "INSERT OR REPLACE INTO active_cli_per_chat \
         (chat_id, active_cli_name, active_agent_id, set_at, set_by) \
         VALUES (?1, ?2, ?3, 0, 'test')",
        params![chat_id, cli_name, agent_id],
    )
    .unwrap();
}

/// Seed a `tg_message_map` reply-quote row.
fn seed_msg_map(conn: &Connection, tg_msg_id: i64, chat_id: i64, sender_agent_id: &str) {
    conn.execute(
        "INSERT OR REPLACE INTO tg_message_map \
         (tg_msg_id, chat_id, sender_agent_id, sent_at) \
         VALUES (?1, ?2, ?3, 0)",
        params![tg_msg_id, chat_id, sender_agent_id],
    )
    .unwrap();
}

/// Build a free-text `Update` (no reply-quote).
fn free_text_update(update_id: i64, chat_id: i64, user_id: i64, text: &str) -> Update {
    Update {
        update_id,
        message: Some(Message {
            date: 0,
            message_id: 1000 + update_id,
            from: Some(User { id: user_id, username: Some("op".into()) }),
            chat: Chat { id: chat_id },
            text: Some(text.into()),
            voice: None,
            reply_to_message: None,
        }),
        callback_query: None,
    }
}

/// Build a reply-quote `Update`.
fn reply_update(update_id: i64, chat_id: i64, user_id: i64, reply_to: i64) -> Update {
    use claudebase::daemon::telegram::ReplyToMessage;
    Update {
        update_id,
        message: Some(Message {
            date: 0,
            message_id: 2000 + update_id,
            from: Some(User { id: user_id, username: None }),
            chat: Chat { id: chat_id },
            text: Some("reply".into()),
            voice: None,
            reply_to_message: Some(ReplyToMessage { message_id: reply_to }),
        }),
        callback_query: None,
    }
}

/// Access that allows all users (DmPolicy::Disabled = no allowlist gate).
fn allow_all() -> Access {
    let mut a = Access::default();
    a.dm_policy = DmPolicy::Disabled;
    a
}

/// Extract `meta.target_agent_id` from the first notification in a
/// `BatchOutcome.notifications` list.
fn first_target(notifications: &[(String, Value)]) -> Option<String> {
    notifications
        .first()
        .and_then(|(_t, f)| f.pointer("/params/meta/target_agent_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

// ── TC-TMC-22.1: full round-trip routing ────────────────────────────────────

/// TC-TMC-22.1 — chat 111 bound to CLI-1; free-text in chat 111 routes to
/// CLI-1 ONLY. Then a tg_message_map row for msg 9001 is seeded (simulating
/// CLI-1 having sent a reply that was recorded). A reply-quote for msg 9001
/// also routes to CLI-1.
///
/// Covers: UC-TMC-22 / FR-TMC-7.1
#[test]
fn tc_tmc_22_1_routing_and_reply_quote() {
    let mut conn = open_test_conn();

    // Seed two agents; bind chat 111 to CLI-1.
    seed_agent(&conn, "cli-1-id", "mira");
    seed_agent(&conn, "cli-2-id", "worker");
    seed_binding(&conn, 111, "mira", "cli-1-id");

    let access = allow_all();
    let bus = Arc::new(ChatBus::new());

    // Step 1: free-text message on chat 111 → CLI-1.
    let batch_1 = vec![free_text_update(1, 111, 7, "hello")];
    let outcome_1 = process_batch(&mut conn, &access, Some(&bus), &batch_1).unwrap();
    assert_eq!(outcome_1.messages_inserted, 1, "TC-TMC-22.1: one message inserted");
    assert_eq!(
        first_target(&outcome_1.notifications).as_deref(),
        Some("cli-1-id"),
        "TC-TMC-22.1: free-text on chat 111 must route to cli-1-id"
    );
    // CLI-2 must NOT be the target.
    assert_ne!(
        first_target(&outcome_1.notifications).as_deref(),
        Some("cli-2-id"),
        "TC-TMC-22.1: cli-2 must not receive chat-111 message"
    );

    // Step 2: simulate CLI-1 having sent message_id 9001 (tg_message_map row).
    seed_msg_map(&conn, 9001, 111, "cli-1-id");

    // Verify the row exists.
    let sender: String = conn
        .query_row(
            "SELECT sender_agent_id FROM tg_message_map WHERE chat_id=111 AND tg_msg_id=9001",
            [],
            |r| r.get(0),
        )
        .expect("TC-TMC-22.1: tg_message_map row must exist after seed");
    assert_eq!(sender, "cli-1-id", "TC-TMC-22.1: tg_message_map.sender_agent_id = cli-1-id");

    // Step 3: reply-quote referencing msg 9001 → CLI-1 (routing step 2).
    let batch_2 = vec![reply_update(2, 111, 7, 9001)];
    let outcome_2 = process_batch(&mut conn, &access, Some(&bus), &batch_2).unwrap();
    assert_eq!(
        first_target(&outcome_2.notifications).as_deref(),
        Some("cli-1-id"),
        "TC-TMC-22.1: reply-quote for msg 9001 must resolve to cli-1-id"
    );
}

// ── TC-TMC-22.2: cross-chat isolation ────────────────────────────────────────

/// TC-TMC-22.2 — two CLIs alive; chat 111 → CLI-1, chat 222 → CLI-2. A
/// message on chat 111 must NEVER reach CLI-2, and vice versa. A third chat
/// (chat 333) with NO binding verifies its notification doesn't bleed into
/// 111 or 222.
///
/// Covers: UC-TMC-22-EC1 / AC-TMC-4
#[test]
fn tc_tmc_22_2_cross_chat_isolation() {
    let mut conn = open_test_conn();

    // Seed agents and bindings.
    seed_agent(&conn, "cli-1-id", "mira");
    seed_agent(&conn, "cli-2-id", "worker");
    seed_agent(&conn, "orch-id", "orchestrator-main");
    seed_binding(&conn, 111, "mira", "cli-1-id");
    seed_binding(&conn, 222, "worker", "cli-2-id");
    // chat 333 has no binding; will fall to first_alive (orch-id).

    let access = allow_all();
    let bus = Arc::new(ChatBus::new());

    // Chat 111 → CLI-1 only.
    let o_111 = process_batch(&mut conn, &access, Some(&bus), &[free_text_update(1, 111, 7, "hi")])
        .unwrap();
    assert_eq!(
        first_target(&o_111.notifications).as_deref(),
        Some("cli-1-id"),
        "TC-TMC-22.2: chat 111 must route to cli-1-id"
    );
    assert_ne!(
        first_target(&o_111.notifications).as_deref(),
        Some("cli-2-id"),
        "TC-TMC-22.2: cli-2 must NOT be the target for chat 111"
    );

    // Chat 222 → CLI-2 only.
    let o_222 = process_batch(&mut conn, &access, Some(&bus), &[free_text_update(2, 222, 7, "hi")])
        .unwrap();
    assert_eq!(
        first_target(&o_222.notifications).as_deref(),
        Some("cli-2-id"),
        "TC-TMC-22.2: chat 222 must route to cli-2-id"
    );
    assert_ne!(
        first_target(&o_222.notifications).as_deref(),
        Some("cli-1-id"),
        "TC-TMC-22.2: cli-1 must NOT be the target for chat 222"
    );

    // Chat 333 (no binding) → orch-id (first_alive orchestrator).
    // This also verifies chat-C binding never bleeds into A/B.
    let o_333 = process_batch(&mut conn, &access, Some(&bus), &[free_text_update(3, 333, 7, "hi")])
        .unwrap();
    let target_333 = first_target(&o_333.notifications);
    assert_eq!(
        target_333.as_deref(),
        Some("orch-id"),
        "TC-TMC-22.2: chat 333 (unbound) must fall to first_alive orchestrator"
    );
    // Planted chat-C binding must NEVER appear in chat-A or chat-B results.
    assert_ne!(
        target_333.as_deref(),
        Some("cli-1-id"),
        "TC-TMC-22.2: chat-333 fallback must not contaminate chat-111"
    );
    assert_ne!(
        target_333.as_deref(),
        Some("cli-2-id"),
        "TC-TMC-22.2: chat-333 fallback must not contaminate chat-222"
    );
}

// ── TC-TMC-22.3: restart survives (SQLite durability) ───────────────────────

/// TC-TMC-22.3 — write `active_cli_per_chat` and `tg_message_map` rows to a
/// file-backed SQLite db; close the connection; reopen with `open_chat_db()`
/// (schema is applied again — idempotent); assert both rows are still present.
///
/// This exercises the SQLite durability contract: the daemon can restart and
/// both the chat binding and the message-map survive. Routing after restart
/// resolves to CLI-1 via the persisted binding and the persisted message-map.
///
/// Covers: UC-TMC-22-A1
#[test]
fn tc_tmc_22_3_routing_survives_db_reopen() {
    let tmpdir = tempfile::tempdir().expect("tmpdir");
    let db_path = tmpdir.path().join("chat.db");

    // Write rows via first connection.
    {
        let conn = open_file_conn(&db_path);
        seed_agent(&conn, "cli-1-id", "mira");
        seed_binding(&conn, 111, "mira", "cli-1-id");
        seed_msg_map(&conn, 9001, 111, "cli-1-id");
    } // connection dropped and flushed here

    // Reopen — simulates daemon restart.
    let conn2 = open_file_conn(&db_path);

    // Verify active_cli_per_chat binding is still present.
    let active_agent: String = conn2
        .query_row(
            "SELECT active_agent_id FROM active_cli_per_chat WHERE chat_id=111",
            [],
            |r| r.get(0),
        )
        .expect("TC-TMC-22.3: active_cli_per_chat row must survive reopen");
    assert_eq!(active_agent, "cli-1-id", "TC-TMC-22.3: binding persisted correctly");

    // Verify tg_message_map row is still present.
    let sender: String = conn2
        .query_row(
            "SELECT sender_agent_id FROM tg_message_map WHERE chat_id=111 AND tg_msg_id=9001",
            [],
            |r| r.get(0),
        )
        .expect("TC-TMC-22.3: tg_message_map row must survive reopen");
    assert_eq!(sender, "cli-1-id", "TC-TMC-22.3: message-map persisted correctly");

    // Routing after restart — the binding is alive again once we re-register
    // the agent (simulating the CLI reconnecting after daemon restart).
    // We seed the agent again (register is idempotent) and verify routing.
    let mut conn3 = open_file_conn(&db_path);
    seed_agent(&conn3, "cli-1-id", "mira"); // re-register CLI-1 (it's reconnected)
    let access = allow_all();
    let bus = Arc::new(ChatBus::new());

    // Free-text after restart → routes via persisted active_cli_per_chat.
    let o_free = process_batch(
        &mut conn3,
        &access,
        Some(&bus),
        &[free_text_update(10, 111, 7, "post-restart message")],
    )
    .unwrap();
    assert_eq!(
        first_target(&o_free.notifications).as_deref(),
        Some("cli-1-id"),
        "TC-TMC-22.3: after restart free-text routes to cli-1-id via persisted binding"
    );

    // Reply-quote after restart → resolves via persisted tg_message_map.
    let o_reply = process_batch(
        &mut conn3,
        &access,
        Some(&bus),
        &[reply_update(11, 111, 7, 9001)],
    )
    .unwrap();
    assert_eq!(
        first_target(&o_reply.notifications).as_deref(),
        Some("cli-1-id"),
        "TC-TMC-22.3: after restart reply-quote for msg 9001 routes to cli-1-id via persisted message-map"
    );
}

// ── TC-TMC-19.1: daemon-not-running error path ─────────────────────────────

/// Per-pid BufReader registry for plugin stdout.
fn stdout_registry() -> &'static Mutex<HashMap<u32, BufReader<ChildStdout>>> {
    static REG: OnceLock<Mutex<HashMap<u32, BufReader<ChildStdout>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn spawn_plugin_no_daemon(tmpdir: &Path) -> Result<Child> {
    let bin = env!("CARGO_BIN_EXE_claudebase");
    let mut cmd = Command::new(bin);
    cmd.args(["plugin", "serve"]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    // Point HOME + XDG_RUNTIME_DIR to a dir where NO daemon socket exists.
    cmd.env("HOME", tmpdir);
    #[cfg(unix)]
    {
        let runtime_dir = tmpdir.join("no-daemon-run");
        std::fs::create_dir_all(&runtime_dir)?;
        cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    }
    #[cfg(windows)]
    {
        cmd.env("USERPROFILE", tmpdir);
        let localappdata = tmpdir.join("AppData\\Local");
        std::fs::create_dir_all(&localappdata)?;
        cmd.env("LOCALAPPDATA", &localappdata);
    }
    Ok(cmd.spawn()?)
}

fn send_mcp_line(plugin: &mut Child, line: &str) -> Result<()> {
    let stdin = plugin
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("stdin unavailable"))?;
    writeln!(stdin, "{}", line)?;
    stdin.flush()?;
    Ok(())
}

async fn read_mcp_line(plugin: &mut Child, timeout_dur: Duration) -> Result<String> {
    let pid = plugin.id();
    {
        let mut reg = stdout_registry().lock().unwrap();
        if !reg.contains_key(&pid) {
            let stdout = plugin
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("stdout unavailable"))?;
            reg.insert(pid, BufReader::new(stdout));
        }
    }
    let result = timeout(
        timeout_dur,
        tokio::task::spawn_blocking(move || {
            let mut reg = stdout_registry().lock().unwrap();
            let reader = reg
                .get_mut(&pid)
                .ok_or_else(|| anyhow::anyhow!("registry missing"))?;
            let mut buf = String::new();
            match reader.read_line(&mut buf) {
                Ok(0) => Err(anyhow::anyhow!("EOF")),
                Ok(_) => Ok(buf.trim_end_matches('\n').trim_end_matches('\r').to_string()),
                Err(e) => Err(anyhow::anyhow!("read error: {}", e)),
            }
        }),
    )
    .await;
    match result {
        Ok(Ok(Ok(line))) => Ok(line),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(e)) => Err(anyhow::anyhow!("spawn_blocking: {}", e)),
        Err(_) => Err(anyhow::anyhow!("timeout waiting for plugin stdout")),
    }
}

async fn send_mcp_request(
    plugin: &mut Child,
    method: &str,
    params: Value,
    id: u32,
) -> Result<Value> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    send_mcp_line(plugin, &request.to_string())?;
    let line = read_mcp_line(plugin, Duration::from_secs(5)).await?;
    Ok(serde_json::from_str(&line)?)
}

/// TC-TMC-19.1 — the plugin bridge with NO daemon running returns an error
/// response for `chat_ask`; CLI-1 does not crash (plugin process keeps
/// responding to subsequent requests).
///
/// The TOOL_WHITELIST includes "chat_ask" (TC-TMC-18.1 verifies this), so
/// the SEC-7 gate passes and the bridge proceeds to the daemon-down path.
/// On daemon-down the bridge returns `-32601 Method not found` for any tool
/// other than `claudebase_daemon_status` (bridge.rs:388-407). This is a
/// well-defined error response — the plugin does NOT hang or crash.
///
/// Covers: UC-TMC-19 / NFR-TMC-7
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn tc_tmc_19_1_daemon_not_running_chat_ask_returns_error() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let mut plugin = spawn_plugin_no_daemon(tmpdir.path()).expect("plugin spawn");

    // Allow time for initial connect-with-retries (3 × 250 ms = 750 ms) to
    // exhaust and the plugin to enter daemon-down mode. We send initialize
    // immediately; the init handler does not require daemon contact.
    let init_params = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": { "name": "test-tc-19-1", "version": "0.1" },
    });
    let init_resp = send_mcp_request(&mut plugin, "initialize", init_params, 1)
        .await
        .expect("initialize should respond even without daemon");
    assert!(
        init_resp.get("error").is_none(),
        "TC-TMC-19.1: initialize should succeed without daemon"
    );

    // Wait out the initial 3-retry window (750 ms headroom + buffer).
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // Call chat_ask — daemon is down, plugin must return an error response.
    // The bridge returns -32601 Method not found for non-daemon_status tools
    // when daemon is down (bridge.rs handle path for daemon.is_none()).
    let chat_ask_params = json!({
        "name": "chat_ask",
        "arguments": {
            "thread": "telegram:111",
            "question": "Which agent?",
            "options": ["mira", "worker"],
        },
    });
    let resp = send_mcp_request(&mut plugin, "tools/call", chat_ask_params, 2)
        .await
        .expect("TC-TMC-19.1: plugin should respond to chat_ask (not hang)");

    // The response MUST be an error (not a result).
    assert!(
        resp.get("error").is_some(),
        "TC-TMC-19.1: chat_ask with daemon down must return an error response; got: {resp}"
    );

    // CLI-1 did not crash — plugin still responds to a follow-up request.
    let ping = json!({ "jsonrpc": "2.0", "id": 3, "method": "ping", "params": {} });
    send_mcp_line(&mut plugin, &ping.to_string()).expect("send ping");
    let ping_resp_line = read_mcp_line(&mut plugin, Duration::from_secs(3)).await;
    assert!(
        ping_resp_line.is_ok(),
        "TC-TMC-19.1: plugin must still respond after daemon-down chat_ask (no crash); ping error: {:?}",
        ping_resp_line.err()
    );
    let ping_resp: Value = serde_json::from_str(&ping_resp_line.unwrap()).unwrap();
    assert!(
        ping_resp.get("error").is_none(),
        "TC-TMC-19.1: ping after daemon-down chat_ask must succeed"
    );

    let _ = plugin.kill();
}

// ── Infeasibility notes ──────────────────────────────────────────────────────
//
// The following QA cases are NOT testable as pure Rust integration tests
// without a live Telegram mock HTTP server or complex async daemon orchestration
// beyond what the integration test harness provides. They remain manual or
// qa-cycle targets:
//
// • TC-TMC-22.1 step (a) — ChatBus notification delivery end-to-end:
//   verifying the subscribed plugin subprocess receives the
//   `notifications/claude/channel` frame after process_batch publishes
//   requires spawning daemon + two plugin processes and reading their
//   stdout asynchronously. The ROUTING half (which CLI gets the target)
//   is covered by this file's in-process assertions. The BUS DELIVERY
//   half is covered by `chat_broadcast_test.rs::test_chat_broadcast_to_two_subscribers`
//   (TC-3.3) which exercises the bus path end-to-end.
//
// • TC-TMC-15.1 (409 Conflict mock): requires a mock HTTP server returning
//   HTTP 409 to the Telegram getUpdates call. No mock server fixture exists.
//   The `is_conflict_error` predicate that parses the 409 Display string is
//   already unit-tested in `telegram.rs::tests::is_conflict_error_matches_teloxide_409_display`.
//
// • TC-TMC-13.x / TC-TMC-14.x (`chat_ask` inline keyboard + callback_query):
//   require a mock Telegram sendMessage + answerCallbackQuery HTTP server.
//   No mock server fixture exists.
//
// • TC-TMC-22.1 full round-trip with `chat_reply` → `tg_message_map` insert:
//   the `enqueue_outbound_tg_with_sender` path requires the async TG send
//   task running inside a live daemon. Tested via end-to-end qa-cycle against
//   the running system.
