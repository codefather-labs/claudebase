//! End-to-end for mind maps (journal#92, #93, #94, #95), against a real
//! daemon: two sessions in one project; the owner builds a map that points at
//! journal entries; the other session may read it by id and nothing more;
//! each session's start-of-session hook names its own open maps only; the
//! daemon serves the live page by the map's view token.

mod common;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use common::{payload, socket_under, stop, wait_for_socket, Client};

const BIN: &str = env!("CARGO_BIN_EXE_claudebase");

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    l.local_addr().expect("addr").port()
}

/// Stops the daemon however the test ends. A panicking assert skips any
/// `stop()` written after it, and the orphaned daemon keeps the test's output
/// pipe open, so a `cargo test | grep` waits forever (journal#45).
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        stop(&mut self.0);
    }
}

fn spawn_daemon(home: &Path) -> Result<std::process::Child> {
    let runtime = home.join("run");
    std::fs::create_dir_all(&runtime)?;
    Ok(std::process::Command::new(BIN)
        .args(["daemon", "serve"])
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .spawn()?)
}

async fn register(c: &mut Client, agent_id: &str, nick: &str, cwd: &Path) -> Result<()> {
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
            1,
        )
        .await?;
    assert!(resp.get("error").is_none(), "register failed: {resp}");
    Ok(())
}

fn session(agent_id: &str, extra: Value) -> Value {
    let mut args = json!({ "agent_id": agent_id, "session_token": format!("tok-{agent_id}") });
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    args
}

async fn ok(c: &mut Client, tool: &str, args: Value) -> Value {
    let resp = c.call(tool, args, 7).await.expect("call");
    payload(&resp).unwrap_or_else(|| panic!("{tool} failed: {resp}"))
}

async fn refused(c: &mut Client, tool: &str, args: Value) -> String {
    let resp = c.call(tool, args, 8).await.expect("call");
    assert!(payload(&resp).is_none(), "{tool} should have been refused: {resp}");
    resp.to_string()
}

/// The SessionStart hook as a session would run it: its identity in the env,
/// the claudebase under test first on PATH.
fn hook_output(home: &Path, agent_id: &str) -> String {
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let link = bin_dir.join("claudebase");
    if !link.exists() {
        std::os::unix::fs::symlink(BIN, &link).unwrap();
    }
    let hook = Path::new(env!("CARGO_MANIFEST_DIR")).join("hooks/claudebase-journal-reminder.sh");
    let out = std::process::Command::new("bash")
        .arg(&hook)
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("PATH", format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default()))
        .env("CLAUDEBASE_AGENT_ID", agent_id)
        .env("CLAUDEBASE_SESSION_TOKEN", format!("tok-{agent_id}"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "hook failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn http_get(port: u16, path: &str) -> String {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[tokio::test]
async fn a_map_is_its_sessions_alone_and_readable_by_id() -> Result<()> {
    let home = tempfile::tempdir()?;
    let project = tempfile::tempdir()?;
    let port = free_port();
    let enable = std::process::Command::new(BIN)
        .args(["daemon", "callback", "enable", "--bind"])
        .arg(format!("127.0.0.1:{port}"))
        .env("HOME", home.path())
        .output()?;
    assert!(enable.status.success(), "{enable:?}");
    let _daemon = Daemon(spawn_daemon(home.path())?);
    let socket = socket_under(home.path());
    wait_for_socket(&socket, Duration::from_secs(30)).await?;
    {
        let mut mira = Client::connect(&socket).await?;
        register(&mut mira, "mira-1", "mira", project.path()).await?;
        let mut atlas = Client::connect(&socket).await?;
        register(&mut atlas, "atlas-1", "atlas", project.path()).await?;

        let e1 = ok(&mut mira, "journal_add", session("mira-1", json!({ "body": "the pool queue grows when the bus saturates", "category": "finding", "tags": ["pool"] }))).await["id"].as_i64().unwrap();
        let e2 = ok(&mut mira, "journal_add", session("mira-1", json!({ "body": "the bus has a single writer task", "category": "finding", "tags": ["pool"] }))).await["id"].as_i64().unwrap();

        let made = ok(&mut mira, "journal_mindmap_new", session("mira-1", json!({ "title": "why the pool is slow" }))).await;
        let map = made["map"].as_i64().unwrap();
        let a = ok(&mut mira, "journal_mindmap_add", session("mira-1", json!({ "map": map, "text": "queue grows", "kind": "fact", "refs": [e1] }))).await["node"].as_i64().unwrap();
        let b = ok(&mut mira, "journal_mindmap_add", session("mira-1", json!({ "map": map, "text": "single writer", "kind": "hypothesis", "parent": a }))).await["node"].as_i64().unwrap();
        ok(&mut mira, "journal_mindmap_link", session("mira-1", json!({ "map": map, "from": b, "to": a, "label": "explains", "refs": [e2] }))).await;
        ok(&mut mira, "journal_mindmap_chain", session("mira-1", json!({ "map": map, "name": "cause", "entries": [e1, e2] }))).await;

        // The other session: every write refused, the read by id allowed.
        for (tool, args) in [
            ("journal_mindmap_add", json!({ "map": map, "text": "x", "kind": "idea" })),
            ("journal_mindmap_edit", json!({ "map": map, "node": a, "text": "x" })),
            ("journal_mindmap_state", json!({ "map": map, "node": a, "state": "done" })),
            ("journal_mindmap_remove", json!({ "map": map, "node": b })),
            ("journal_mindmap_link", json!({ "map": map, "from": a, "to": b })),
            ("journal_mindmap_ref", json!({ "map": map, "node": a, "entries": [e2] })),
            ("journal_mindmap_chain", json!({ "map": map, "name": "other", "entries": [e2, e1] })),
            ("journal_mindmap_close", json!({ "map": map })),
            ("journal_mindmap_delete", json!({ "map": map })),
        ] {
            let err = refused(&mut atlas, tool, session("atlas-1", args)).await;
            assert!(err.contains("belongs to session"), "{tool}: {err}");
        }
        let seen = ok(&mut atlas, "journal_mindmap_show", session("atlas-1", json!({ "map": map }))).await;
        assert_eq!(seen["read_only"], json!(true));
        let outline = seen["outline"].as_str().unwrap();
        assert!(outline.contains("single writer") && outline.contains(&format!("journal#{e1}")), "{outline}");
        assert!(ok(&mut atlas, "journal_mindmap_list", session("atlas-1", json!({}))).await["maps"].as_array().unwrap().is_empty());
        let mine = ok(&mut mira, "journal_mindmap_list", session("mira-1", json!({}))).await;
        assert_eq!(mine["maps"][0]["id"], json!(map));

        // The start-of-session hook: the owner is told, the other session is not.
        let told = hook_output(home.path(), "mira-1");
        assert!(told.contains("Your open mind maps") && told.contains("why the pool is slow"), "{told}");
        let other = hook_output(home.path(), "atlas-1");
        assert!(!other.contains("why the pool is slow") && !other.contains("Your open mind maps"), "{other}");

        // The drawing, and the live page by view token.
        let svg = ok(&mut atlas, "journal_mindmap_svg", session("atlas-1", json!({ "map": map, "theme": "dark" }))).await;
        assert!(svg["svg"].as_str().unwrap().starts_with("<svg"));
        let url = ok(&mut mira, "journal_mindmap_url", session("mira-1", json!({ "map": map }))).await["url"].as_str().unwrap().to_string();
        let path = url.split_once(&format!("127.0.0.1:{port}")).map(|(_, p)| p.to_string()).expect("url on the listener");
        let page = http_get(port, &path);
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("text/html") && page.contains("<svg"), "{page}");
        let image = http_get(port, &format!("{path}.svg"));
        assert!(image.starts_with("HTTP/1.1 200") && image.contains("image/svg+xml"), "{image}");
        let wrong = http_get(port, "/mindmap/0123456789abcdef0123456789abcdef");
        assert!(wrong.starts_with("HTTP/1.1 404"), "{wrong}");
        let post = {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(s, "POST {path} HTTP/1.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        assert!(post.starts_with("HTTP/1.1 405"), "{post}");

        // Closed: no longer named by the hook, still readable.
        ok(&mut mira, "journal_mindmap_close", session("mira-1", json!({ "map": map }))).await;
        let after = hook_output(home.path(), "mira-1");
        assert!(!after.contains("Your open mind maps"), "{after}");
        assert!(ok(&mut atlas, "journal_mindmap_show", session("atlas-1", json!({ "map": map }))).await["outline"].as_str().unwrap().contains("closed"));
        // Deleted by its owner: gone for everyone.
        ok(&mut mira, "journal_mindmap_delete", session("mira-1", json!({ "map": map }))).await;
        refused(&mut atlas, "journal_mindmap_show", session("atlas-1", json!({ "map": map }))).await;
    }
    Ok(())
}
