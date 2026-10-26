//! A session whose connection closes is no longer `alive` — however it closed.
//! A close with frames still unread on the session's side reaches the daemon as
//! `ConnectionReset`, not a clean EOF, and used to leave the row `alive`
//! forever (found by the pool dispatch test: a lost worker kept its job).

mod common;
use std::time::Duration;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_connection_orphans_its_session_even_with_unread_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let mut d = common::spawn_daemon_with_home(&home).unwrap();
    common::wait_for_socket(&common::socket_under(&home), Duration::from_secs(10)).await.unwrap();
    for subscribe in [false, true] {
        let id = format!("p-{subscribe}");
        let mut c = common::Client::connect(&common::socket_under(&home)).await.unwrap();
        c.call("agent_register", json!({"agent_id": id, "name": id, "session_token": "t"}), 1).await.unwrap();
        if subscribe {
            c.call("chat_subscribe", json!({"thread": format!("agent:{id}")}), 2).await.unwrap();
            // A message it never reads.
            let mut o = common::Client::connect(&common::socket_under(&home)).await.unwrap();
            o.call("agent_register", json!({"agent_id": "other", "name": "other", "session_token": "o"}), 1).await.unwrap();
            o.call("agent_send", json!({"from_agent_id": "other", "session_token": "o", "to_agent_id": id, "content": "unread"}), 2).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            std::mem::forget(o);
        }
        drop(c);
        tokio::time::sleep(Duration::from_millis(800)).await;
        let conn = rusqlite::Connection::open(home.join(".claude/knowledge/chat.db")).unwrap();
        let st: String = conn.query_row("SELECT state FROM agent_registry WHERE agent_id=?1", [&id], |r| r.get(0)).unwrap();
        assert_eq!(st, "orphaned", "subscribe={subscribe}");
    }
    common::stop(&mut d);
}
