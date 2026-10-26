//! Pool dispatch end to end (docs/design/pool-dispatch.md): a live daemon, two
//! worker sessions registered with `pool: funnel`, and a "gateway" speaking
//! HTTP to `/pool/*` with its own token. Workers answer through the CLI, as a
//! `claudebase run --pool funnel` session would.

mod common;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command;
use base64::Engine;
use serde_json::{json, Value};

fn cli(home: &Path, session: Option<(&str, &str)>, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("claudebase").unwrap();
    cmd.current_dir(home)
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env_remove("CLAUDEBASE_AGENT_ID")
        .env_remove("CLAUDEBASE_SESSION_TOKEN")
        .env_remove("CLAUDEBASE_DAEMON")
        .args(args);
    if let Some((id, token)) = session {
        cmd.env("CLAUDEBASE_AGENT_ID", id).env("CLAUDEBASE_SESSION_TOKEN", token);
    }
    cmd
}

fn out(mut cmd: Command) -> String {
    String::from_utf8_lossy(&cmd.assert().success().get_output().stdout).into_owned()
}

/// One HTTP/1.1 request; the server closes after answering.
fn http(port: u16, method: &str, path: &str, token: &str, body: Option<&Value>) -> (u16, Value) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(40))).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\nX-Api-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status: u16 = resp.split_whitespace().nth(1).unwrap().parse().unwrap();
    let json = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, serde_json::from_str(json).unwrap_or(Value::Null))
}

struct Daemon(std::process::Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        common::stop(&mut self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gateway_dispatches_to_free_workers_and_gets_their_answers() {
    let tmp = tempfile::tempdir().unwrap();
    let home: PathBuf = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(home.join("run")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();

    let h = home.clone();
    let token = tokio::task::spawn_blocking(move || {
        out(cli(&h, None, &["daemon", "callback", "enable", "--bind", &format!("127.0.0.1:{port}")]));
        let issued = out(cli(&h, None, &["pool", "token", "issue", "gateway"]));
        issued.lines().find(|l| l.starts_with("cbp_")).expect("token").trim().to_owned()
    })
    .await
    .unwrap();

    // A short suspect cooldown, inherited by the daemon (8 minutes in real use).
    std::env::set_var("CLAUDEBASE_POOL_SUSPECT_COOLDOWN_MS", "6000");
    let _d = Daemon(common::spawn_daemon_with_home(&home).unwrap());
    common::wait_for_socket(&common::socket_under(&home), Duration::from_secs(10)).await.unwrap();
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Two workers, as `claudebase run --pool funnel` registers them.
    let mut clients = std::collections::HashMap::new();
    for (n, (id, nick)) in [("w-a-id", "w-a"), ("w-b-id", "w-b")].into_iter().enumerate() {
        let mut c = common::Client::connect(&common::socket_under(&home)).await.unwrap();
        let r = c
            .call("agent_register", json!({"agent_id": id, "name": nick, "session_token": format!("tok-{nick}"), "pool": "funnel"}), 1)
            .await
            .unwrap();
        assert!(common::payload(&r).is_some(), "{r}");
        c.call("chat_subscribe", json!({"thread": format!("agent:{id}")}), 2 + n as u32).await.unwrap();
        clients.insert(nick.to_string(), c);
    }
    let sess = |nick: &str| -> (String, String) { (format!("{nick}-id"), format!("tok-{nick}")) };

    let tk = token.clone();
    let g = move |method: &'static str, path: String, body: Option<Value>| {
        let tk = tk.clone();
        async move { tokio::task::spawn_blocking(move || http(port, method, &path, &tk, body.as_ref())).await.unwrap() }
    };

    // Unknown token, unknown pool.
    let (st, _) = tokio::task::spawn_blocking(move || http(port, "GET", "/pool/funnel/workers", "cbp_nope", None)).await.unwrap();
    assert_eq!(st, 401);
    assert_eq!(g("GET", "/pool/nopool/workers".into(), None).await.0, 404);

    let (st, w) = g("GET", "/pool/funnel/workers".into(), None).await;
    assert_eq!((st, w["free"].as_u64()), (200, Some(2)), "{w}");

    // Dispatch r-1 with a jpeg; the worker it went to sees the job line.
    let jpeg = vec![0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
    let job = |id: &str, timeout: i64| {
        json!({"req_id": id, "timeout_ms": timeout,
               "payload": {"model": "m", "messages": [{"role": "user", "content": "fill the form"}],
                           "meta": {"callType": "FORM_FILL"}},
               "images": [{"base64": b64, "mime": "image/jpeg"}]})
    };
    let (st, d1) = g("POST", "/pool/funnel/dispatch".into(), Some(job("r-1", 60_000))).await;
    assert_eq!((st, d1["state"].as_str()), (202, Some("leased")), "{d1}");
    let w1 = d1["worker"].as_str().unwrap().to_owned();
    let w2 = if w1 == "w-a" { "w-b" } else { "w-a" }.to_owned();
    let note = clients.get_mut(&w1).unwrap().next_notification(Duration::from_secs(5)).await.unwrap();
    assert_eq!(note["params"]["meta"]["source"], "pool");
    assert_eq!(note["params"]["meta"]["call_type"], "FORM_FILL");
    assert!(note["params"]["content"].as_str().unwrap().contains("claudebase pool job r-1"), "{note}");

    // The same req_id again: no second worker.
    let (st, again) = g("POST", "/pool/funnel/dispatch".into(), Some(job("r-1", 60_000))).await;
    assert_eq!((st, again["worker"].as_str()), (200, Some(w1.as_str())));
    let (st, d2) = g("POST", "/pool/funnel/dispatch".into(), Some(job("r-2", 60_000))).await;
    assert_eq!((st, d2["worker"].as_str()), (202, Some(w2.as_str())), "{d2}");
    let (st, full) = g("POST", "/pool/funnel/dispatch".into(), Some(job("r-3", 60_000))).await;
    assert_eq!((st, full["error"].as_str()), (503, Some("no_free_worker")), "{full}");

    // The worker fetches its job; the other one cannot answer it.
    let (h, s1, s2) = (home.clone(), sess(&w1), sess(&w2));
    let jpeg2 = jpeg.clone();
    tokio::task::spawn_blocking(move || {
        let fetched = out(cli(&h, Some((&s1.0, &s1.1)), &["pool", "job", "r-1"]));
        assert!(fetched.contains("(FORM_FILL)"), "{fetched}");
        let dir = h.join(".claude/claudebase/pool-jobs/r-1");
        assert_eq!(std::fs::read(dir.join("image-01.jpg")).unwrap(), jpeg2);
        let payload: Value = serde_json::from_slice(&std::fs::read(dir.join("payload.json")).unwrap()).unwrap();
        assert_eq!(payload["meta"]["callType"], "FORM_FILL");
        cli(&h, Some((&s2.0, &s2.1)), &["pool", "reply", "r-1", "--content", "x"])
            .assert()
            .failure()
            .stderr(predicates::str::contains("another session"));
        // As PowerShell writes a file: UTF-16 LE with a BOM.
        let answer = r#"{"content": "{\"fields\":[]}"}"#;
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain(answer.encode_utf16().flat_map(u16::to_le_bytes)).collect();
        let file = h.join("answer.json");
        std::fs::write(&file, utf16).unwrap();
        out(cli(&h, Some((&s1.0, &s1.1)), &["pool", "reply", "r-1", "--file", file.to_str().unwrap()]));
    })
    .await
    .unwrap();
    let (_, r1) = g("GET", "/pool/jobs/r-1?wait_ms=2000".into(), None).await;
    assert_eq!(r1["state"], "done", "{r1}");
    assert_eq!(r1["result"]["content"], "{\"fields\":[]}");

    // A long-poll returns as soon as the answer lands; tool_calls pass through.
    let waiting = tokio::spawn(g("GET", "/pool/jobs/r-2?wait_ms=20000".into(), None));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (h, s2) = (home.clone(), sess(&w2));
    tokio::task::spawn_blocking(move || {
        let mut reply = cli(&h, Some((&s2.0, &s2.1)), &["pool", "reply", "r-2"]);
        // As PowerShell pipes it: a UTF-8 BOM first.
        let mut piped = vec![0xEF, 0xBB, 0xBF];
        piped.extend_from_slice(br#"{"tool_calls":[{"function":{"name":"click","arguments":"{\"x\":1}"}}]}"#);
        reply.write_stdin(piped);
        reply.assert().success();
    })
    .await
    .unwrap();
    let started = std::time::Instant::now();
    let (_, r2) = waiting.await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(r2["state"], "done");
    assert_eq!(r2["result"]["tool_calls"][0]["function"]["name"], "click");

    // A lease that runs out: expired, and the worker gets nothing until ready.
    let (_, d5) = g("POST", "/pool/funnel/dispatch".into(), Some(job("r-5", 1_000))).await;
    let w5 = d5["worker"].as_str().unwrap().to_owned();
    let (_, r5) = g("GET", "/pool/jobs/r-5?wait_ms=6000".into(), None).await;
    assert_eq!(r5["state"], "expired", "{r5}");
    let (_, w) = g("GET", "/pool/funnel/workers".into(), None).await;
    let state_of = |w: &Value, nick: &str| {
        w["workers"].as_array().unwrap().iter().find(|x| x["nick"] == nick).unwrap()["state"].as_str().unwrap().to_owned()
    };
    assert_eq!(state_of(&w, &w5), "suspect", "{w}");
    let (h, s5) = (home.clone(), sess(&w5));
    tokio::task::spawn_blocking(move || out(cli(&h, Some((&s5.0, &s5.1)), &["pool", "ready"]))).await.unwrap();
    let (_, w) = g("GET", "/pool/funnel/workers".into(), None).await;
    assert_eq!(w["free"], 2, "{w}");

    // Without `pool ready`, a suspect worker is trusted again after the cooldown.
    let (_, d5b) = g("POST", "/pool/funnel/dispatch".into(), Some(job("r-5b", 1_000))).await;
    assert_eq!(d5b["state"], "leased", "{d5b}");
    let (_, r5b) = g("GET", "/pool/jobs/r-5b?wait_ms=6000".into(), None).await;
    assert_eq!(r5b["state"], "expired", "{r5b}");
    let (_, w) = g("GET", "/pool/funnel/workers".into(), None).await;
    assert_eq!(w["free"], 1, "one worker suspect right after expiry: {w}");
    let mut recovered = false;
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let (_, w) = g("GET", "/pool/funnel/workers".into(), None).await;
        if w["free"] == 2 {
            recovered = true;
            break;
        }
    }
    assert!(recovered, "the suspect worker never came back by itself");

    // Cancel: the job is cancelled and its worker told.
    let (_, d6) = g("POST", "/pool/funnel/dispatch".into(), Some(job("r-6", 60_000))).await;
    let w6 = d6["worker"].as_str().unwrap().to_owned();
    let (st, c6) = g("POST", "/pool/jobs/r-6/cancel".into(), None).await;
    assert_eq!((st, c6["state"].as_str()), (200, Some("cancelled")), "{c6}");

    // A worker that vanishes: its job fails as worker_lost after the grace.
    let (_, d7) = g("POST", "/pool/funnel/dispatch".into(), Some(job("r-7", 120_000))).await;
    let w7 = d7["worker"].as_str().unwrap().to_owned();
    drop(clients.remove(&w7));
    let mut lost = Value::Null;
    for _ in 0..3 {
        let (_, r7) = g("GET", "/pool/jobs/r-7?wait_ms=10000".into(), None).await;
        lost = r7;
        if lost["state"] == "failed" {
            break;
        }
    }
    assert_eq!((lost["state"].as_str(), lost["error"].as_str()), (Some("failed"), Some("worker_lost")), "{lost}");
    let _ = w6;
}
