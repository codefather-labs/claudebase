//! `chat_subscribe` + `chat_post` end to end against a live daemon: the tool
//! answers, the subscriber is notified, and the row lands in `chat.db`.
//!
//! Ordering matters and is asserted: the daemon queues the tool RESPONSE before
//! publishing the broadcast, so a client that subscribed to the thread it posts
//! on sees its own response first. Reversing that would break every caller that
//! reads a reply before handling notifications.
//!
//! Rewritten in v0.10: the client used to be `claudebase plugin serve` speaking
//! MCP over stdio. That bridge was removed with the plugin transport, so the
//! test now drives the daemon's UDS surface directly.

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{payload, socket_under, spawn_daemon_with_home, stop, wait_for_socket, Client};
use rusqlite::Connection;
use serde_json::json;

const THREAD: &str = "telegram:99999";

#[tokio::test(flavor = "multi_thread")]
async fn post_then_subscribe_delivers_and_persists() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let home = tmpdir.path();
    let socket = socket_under(home);

    let mut daemon = spawn_daemon_with_home(home).expect("daemon spawned");
    wait_for_socket(&socket, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    let mut client = Client::connect(&socket).await.expect("connect");

    let sub = client
        .call("chat_subscribe", json!({ "thread": THREAD }), 1)
        .await
        .expect("subscribe");
    assert!(sub.get("error").is_none(), "subscribe errored: {sub}");
    let sub_payload = payload(&sub).expect("subscribe payload");
    // The response echoes the thread and hands back its backlog — that backlog
    // is how a reconnecting supervisor catches up on messages it missed.
    assert_eq!(sub_payload.get("thread"), Some(&json!(THREAD)));
    assert!(
        sub_payload.get("messages").and_then(|m| m.as_array()).is_some(),
        "subscribe must return a backlog array, got: {sub_payload}"
    );

    let post = client
        .call(
            "chat_post",
            json!({ "thread": THREAD, "content": "hello world", "from": "mira" }),
            2,
        )
        .await
        .expect("post");
    assert!(post.get("error").is_none(), "post errored: {post}");
    let post_payload = payload(&post).expect("post payload");
    assert!(
        post_payload.get("id").and_then(|v| v.as_str()).is_some(),
        "post response must carry the message id: {post_payload}"
    );

    // `call` returned the response, so the response came first; the broadcast
    // must follow on the same connection.
    let notif = client
        .next_notification(Duration::from_secs(3))
        .await
        .expect("broadcast after the response");
    assert_eq!(notif.pointer("/params/content"), Some(&json!("hello world")));
    assert_eq!(notif.pointer("/params/meta/thread"), Some(&json!(THREAD)));
    assert_eq!(notif.pointer("/params/meta/from_agent"), Some(&json!("mira")));

    let chat_db = home.join(".claude").join("knowledge").join("chat.db");
    assert!(chat_db.exists(), "chat.db should exist at {chat_db:?}");
    let conn = Connection::open(&chat_db).expect("open chat.db");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chat_messages WHERE thread_id = ?1 AND content = 'hello world'",
            [THREAD],
            |r| r.get(0),
        )
        .expect("count rows");
    assert_eq!(rows, 1, "the posted message should be persisted exactly once");

    stop(&mut daemon);
}

#[tokio::test(flavor = "multi_thread")]
async fn posting_without_a_thread_is_rejected() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let home = tmpdir.path();
    let socket = socket_under(home);

    let mut daemon = spawn_daemon_with_home(home).expect("daemon spawned");
    wait_for_socket(&socket, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    let mut client = Client::connect(&socket).await.expect("connect");
    let resp = client
        .call("chat_post", json!({ "content": "orphan" }), 1)
        .await
        .expect("call completes");

    // A message with no destination must fail loudly rather than land in some
    // default thread — misrouting an operator's text is not recoverable.
    assert!(
        resp.get("error").is_some(),
        "a post with no thread must be rejected, got: {resp}"
    );

    stop(&mut daemon);
}
