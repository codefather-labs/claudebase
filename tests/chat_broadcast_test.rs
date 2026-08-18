//! Broadcast delivery — one post reaches every subscriber of a thread.
//!
//! This is the mechanism the whole transport rests on: the daemon publishes an
//! inbound Telegram message to a thread, and each subscribed PTY supervisor
//! injects it into its session. If fan-out breaks, messages vanish silently,
//! which is exactly the failure documented in `docs/issues/006`.
//!
//! Rewritten in v0.10: the subscribers used to be `claudebase plugin serve`
//! processes speaking MCP over stdio. That bridge was removed with the plugin
//! transport, so the test now subscribes over the daemon's UDS surface — the
//! same path `src/daemon/subscribe_client.rs` uses in production.

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{payload, socket_under, spawn_daemon_with_home, stop, wait_for_socket, Client};
use serde_json::json;

/// A peer thread, not a Telegram one, and that distinction is the point.
///
/// On an `agent:<id>` thread the broadcast IS the delivery — it is how a peer
/// session receives a message. On a `telegram:` thread an agent's post is
/// deliberately NOT broadcast: the text has already reached the operator over
/// the Bot API, and the only remaining audience is whichever other sessions
/// happen to subscribe to that chat, who would read one agent's reply to the
/// operator as if the operator had written it to them. That suppression is
/// asserted by its own test below.
const THREAD: &str = "agent:99999";

#[tokio::test(flavor = "multi_thread")]
async fn a_post_reaches_every_subscriber_of_the_thread() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let home = tmpdir.path();
    let socket = socket_under(home);

    let mut daemon = spawn_daemon_with_home(home).expect("daemon spawned");
    wait_for_socket(&socket, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    // Two independent connections, as two live sessions would be.
    let mut a = Client::connect(&socket).await.expect("connect A");
    let mut b = Client::connect(&socket).await.expect("connect B");

    a.call("chat_subscribe", json!({ "thread": THREAD }), 1)
        .await
        .expect("A subscribes");
    b.call("chat_subscribe", json!({ "thread": THREAD }), 2)
        .await
        .expect("B subscribes");

    let post = a
        .call(
            "chat_post",
            json!({
                "thread": THREAD,
                "content": "broadcast-test-message",
                "from": "mira",
            }),
            3,
        )
        .await
        .expect("post");
    assert!(post.get("error").is_none(), "post should not error: {post}");
    assert!(payload(&post).and_then(|p| p.get("id").cloned()).is_some());

    for (name, client) in [("A", &mut a), ("B", &mut b)] {
        let notif = client
            .next_notification(Duration::from_secs(3))
            .await
            .unwrap_or_else(|e| panic!("subscriber {name} received no broadcast: {e}"));
        assert_eq!(
            notif.pointer("/params/content"),
            Some(&json!("broadcast-test-message")),
            "subscriber {name} got the wrong content: {notif}"
        );
        assert_eq!(
            notif.pointer("/params/meta/thread"),
            Some(&json!(THREAD)),
            "subscriber {name} got the wrong thread: {notif}"
        );
    }

    stop(&mut daemon);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_post_does_not_reach_subscribers_of_other_threads() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let home = tmpdir.path();
    let socket = socket_under(home);

    let mut daemon = spawn_daemon_with_home(home).expect("daemon spawned");
    wait_for_socket(&socket, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    let mut listener = Client::connect(&socket).await.expect("connect");
    listener
        .call("chat_subscribe", json!({ "thread": "telegram:11111" }), 1)
        .await
        .expect("subscribe to a different thread");

    let mut poster = Client::connect(&socket).await.expect("connect poster");
    poster
        .call(
            "chat_post",
            json!({ "thread": THREAD, "content": "not-for-you", "from": "mira" }),
            2,
        )
        .await
        .expect("post");

    // Cross-thread leakage would mean an operator's message landing in an
    // unrelated session — worth asserting explicitly rather than assuming.
    let leaked = listener.next_notification(Duration::from_millis(800)).await;
    assert!(
        leaked.is_err(),
        "subscriber of another thread must not receive this post, got: {leaked:?}"
    );

    stop(&mut daemon);
}

/// An agent's post on a TELEGRAM thread must not fan out to that chat's other
/// subscribers.
///
/// This is the shape of a real incident: with several sessions subscribed to
/// one operator chat, every reply any of them sent to the operator was
/// delivered to all the others as inbound, and each read it as something the
/// operator had said to them. The suppression lives at the source rather than
/// only in the subscriber, because a session runs the binary it started with
/// for hours — filtering it daemon-side protects the ones already running.
#[tokio::test(flavor = "multi_thread")]
async fn an_agents_post_on_a_telegram_thread_is_not_broadcast() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let home = tmpdir.path();
    let socket = socket_under(home);

    let mut daemon = spawn_daemon_with_home(home).expect("daemon spawned");
    wait_for_socket(&socket, Duration::from_secs(10))
        .await
        .expect("socket appeared");

    let tg_thread = "telegram:99999";
    let mut listener = Client::connect(&socket).await.expect("connect listener");
    listener
        .call("chat_subscribe", json!({ "thread": tg_thread }), 1)
        .await
        .expect("subscribe");

    let mut poster = Client::connect(&socket).await.expect("connect poster");
    let post = poster
        .call(
            "chat_post",
            json!({ "thread": tg_thread, "content": "an answer to the operator", "from": "mira" }),
            2,
        )
        .await
        .expect("post");
    assert!(post.get("error").is_none(), "the post itself must still succeed: {post}");

    let leaked = listener.next_notification(Duration::from_millis(800)).await;
    assert!(
        leaked.is_err(),
        "another agent's reply to the operator reached this session as inbound: {leaked:?}"
    );

    stop(&mut daemon);
}
