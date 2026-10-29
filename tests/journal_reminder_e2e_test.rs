//! End-to-end for journal reminders (journal#72, journal#73): a session writes
//! an entry with `--every`, and the daemon brings it back into THAT session's
//! input as a `reminder` frame until the entry is closed — holding it while
//! the session is offline and delivering it when the nick registers again.

mod common;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use common::{payload, socket_under, stop, wait_for_socket, Client};

/// The daemon looks for due reminders every second instead of every 30.
fn spawn_daemon(home: &Path) -> Result<std::process::Child> {
    let runtime = home.join("run");
    std::fs::create_dir_all(&runtime)?;
    Ok(std::process::Command::new(env!("CARGO_BIN_EXE_claudebase"))
        .args(["daemon", "serve"])
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("CLAUDEBASE_REMINDER_TICK_SECS", "1")
        .spawn()?)
}

async fn register(c: &mut Client, agent_id: &str, nick: &str, cwd: &Path, id: u32) -> Result<()> {
    let resp = c
        .call(
            "agent_register",
            json!({
                "agent_id": agent_id,
                "name": nick,
                "session_token": format!("tok-{agent_id}"),
                "cwd": cwd.to_string_lossy(),
                "host": "testhost",
                "pid": std::process::id(),
            }),
            id,
        )
        .await?;
    assert!(resp.get("error").is_none(), "register failed: {resp}");
    let sub = c.call("chat_subscribe", json!({ "thread": format!("agent:{agent_id}") }), id + 1).await?;
    assert!(sub.get("error").is_none(), "subscribe failed: {sub}");
    Ok(())
}

fn session(agent_id: &str, extra: Value) -> Value {
    let mut args = json!({ "agent_id": agent_id, "session_token": format!("tok-{agent_id}") });
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    args
}

/// Make every reminder of the project's journal due now, instead of waiting
/// out the one-minute minimum interval.
fn make_due(home: &Path) -> Result<()> {
    let dir = home.join(".claude/claudebase/journals");
    for f in std::fs::read_dir(&dir)? {
        let path = f?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("db") {
            let conn = rusqlite::Connection::open(&path)?;
            conn.busy_timeout(Duration::from_secs(5))?;
            conn.execute("UPDATE entry_reminders SET next_at = 0", [])?;
        }
    }
    Ok(())
}

async fn nothing_for(c: &mut Client, within: Duration) -> bool {
    c.next_notification(within).await.is_err()
}

#[tokio::test]
async fn a_reminder_returns_to_its_author_only_while_online_until_closed() -> Result<()> {
    let home = tempfile::tempdir()?;
    let project = tempfile::tempdir()?;
    let mut daemon = spawn_daemon(home.path())?;
    let socket = socket_under(home.path());
    if wait_for_socket(&socket, Duration::from_secs(30)).await.is_err() {
        stop(&mut daemon);
        anyhow::bail!("daemon never came up");
    }
    let result = async {
        let mut mira = Client::connect(&socket).await?;
        register(&mut mira, "mira-1", "mira", project.path(), 1).await?;
        // A peer in the same project: it must never get mira's reminder.
        let mut atlas = Client::connect(&socket).await?;
        register(&mut atlas, "atlas-1", "atlas", project.path(), 1).await?;

        let added = mira
            .call(
                "journal_add",
                session(
                    "mira-1",
                    json!({ "body": "rerun the flaky pool test", "category": "todo", "tags": ["pool"], "every_secs": 600 }),
                ),
                10,
            )
            .await?;
        let id = payload(&added).and_then(|p| p["id"].as_i64()).unwrap_or_else(|| panic!("add failed: {added}"));

        // Not due yet: nothing arrives.
        assert!(nothing_for(&mut mira, Duration::from_secs(3)).await, "delivered before its time");

        make_due(home.path())?;
        let note = mira.next_notification(Duration::from_secs(10)).await?;
        assert_eq!(note.pointer("/params/meta/source").and_then(Value::as_str), Some("reminder"));
        assert_eq!(note.pointer("/params/meta/entry_id").and_then(Value::as_str), Some(id.to_string().as_str()));
        let content = note.pointer("/params/content").and_then(Value::as_str).unwrap_or_default();
        assert!(content.starts_with("rerun the flaky pool test"), "{content}");
        assert!(content.contains(&format!("claudebase journal close {id}")), "{content}");
        // Rescheduled an interval ahead: no second one right away, and none to the peer.
        assert!(nothing_for(&mut mira, Duration::from_secs(3)).await, "sent again at once");
        assert!(nothing_for(&mut atlas, Duration::from_secs(1)).await, "a peer got another session's reminder");

        // Offline: the session's connection closes; a due reminder waits.
        drop(mira);
        make_due(home.path())?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(nothing_for(&mut atlas, Duration::from_secs(1)).await, "an offline author's reminder went to a peer");

        // Back under the same nick (a resume: new agent id), it arrives.
        let mut back = Client::connect(&socket).await?;
        register(&mut back, "mira-2", "mira", project.path(), 1).await?;
        let note = back.next_notification(Duration::from_secs(10)).await?;
        assert_eq!(note.pointer("/params/meta/source").and_then(Value::as_str), Some("reminder"));
        assert_eq!(note.pointer("/params/meta/target_agent_id").and_then(Value::as_str), Some("mira-2"));

        // Closed: silent for good.
        let closed = back
            .call("journal_close", session("mira-2", json!({ "id": id, "flags": ["reminder"], "resolution": "rerun green" })), 20)
            .await?;
        assert!(closed.get("error").is_none(), "close failed: {closed}");
        make_due(home.path())?;
        assert!(nothing_for(&mut back, Duration::from_secs(3)).await, "a closed reminder came back");
        Ok::<(), anyhow::Error>(())
    }
    .await;
    stop(&mut daemon);
    result
}
