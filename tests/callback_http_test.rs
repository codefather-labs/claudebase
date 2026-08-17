//! End-to-end for the HTTP callback endpoint: a real TCP request to a real
//! daemon, landing as a real channel notification on a subscribed session.
//!
//! The unit tests in `src/daemon/callback.rs` cover parsing and the token store.
//! What they cannot show is the part that actually matters to an operator:
//! `curl` on one side, `[callback:…]` in a session's input on the other. That
//! whole path — auth, nick resolution, storage, bus publish, subscriber — is
//! only exercised here.

mod common;

use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use common::{payload, socket_under, spawn_daemon_with_home, stop, wait_for_socket, Client};

/// A port nobody else is using, obtained by binding one and letting it go.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = l.local_addr().expect("addr").port();
    drop(l);
    port
}

/// Minimal HTTP client. Using `curl` would make the test depend on it being
/// installed; using reqwest would pull the whole client stack into a test that
/// needs one request.
fn post(port: u16, path: &str, token: Option<&str>, body: &str) -> Result<String> {
    use std::io::{Read, Write};

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let auth = match token {
        Some(t) => format!("X-Api-Token: {t}\r\n"),
        None => String::new(),
    };
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

fn body_json(response: &str) -> Value {
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or(response);
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON: {e}\n{response}"))
}

fn status_line(response: &str) -> &str {
    response.lines().next().unwrap_or_default()
}

struct Fixture {
    home: tempfile::TempDir,
    daemon: std::process::Child,
    port: u16,
    token: String,
    /// Held for the fixture's lifetime on purpose: the daemon ties an agent's
    /// registration to the connection that made it and marks the agent orphaned
    /// on EOF. Dropping this would un-register the very session under test —
    /// which is what the first run of these tests did, and the resulting
    /// "no alive agent matches `mira`" was the daemon being right.
    session: Client,
}

impl Fixture {
    async fn start(nick: &str) -> Result<Fixture> {
        let home = tempfile::tempdir()?;
        let port = free_port();

        // Configure the bind BEFORE the daemon starts: the listener is spawned
        // during startup, which is also the behaviour an operator gets after
        // `callback enable` + `daemon restart`.
        let bin = env!("CARGO_BIN_EXE_claudebase");
        let out = std::process::Command::new(bin)
            .args(["daemon", "callback", "enable", "--bind"])
            .arg(format!("127.0.0.1:{port}"))
            .env("HOME", home.path())
            .output()?;
        assert!(out.status.success(), "enable failed: {out:?}");

        let mut daemon = spawn_daemon_with_home(home.path())?;
        let socket = socket_under(home.path());
        if wait_for_socket(&socket, Duration::from_secs(30)).await.is_err() {
            stop(&mut daemon);
            anyhow::bail!("daemon never came up");
        }

        // Registering mints the token — the operator never asks for one.
        let mut client = Client::connect(&socket).await?;
        let resp = client
            .call(
                "agent_register",
                json!({
                    "agent_id": "agent-under-test",
                    "name": nick,
                    "session_token": "tok",
                    "cwd": "/tmp/project",
                    "host": "testhost",
                    "pid": std::process::id(),
                }),
                1,
            )
            .await?;
        assert!(resp.get("error").is_none(), "register failed: {resp}");

        let read = std::process::Command::new(bin)
            .args(["daemon", "callback", "token", nick, "--reveal"])
            .env("HOME", home.path())
            .output()?;
        assert!(read.status.success(), "token read failed: {read:?}");
        let token = String::from_utf8_lossy(&read.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        assert_eq!(token.len(), 64, "expected a 32-byte hex token, got {token:?}");

        Ok(Fixture {
            home,
            daemon,
            port,
            token,
            session: client,
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        stop(&mut self.daemon);
    }
}

/// The whole point: something outside POSTs, and the session receives it as a
/// channel notification tagged as a callback.
#[tokio::test]
async fn a_posted_callback_reaches_a_subscribed_session() -> Result<()> {
    let mut fx = Fixture::start("mira").await?;

    // Subscribe on the SAME connection that registered — that is how the PTY
    // supervisor does it, and it keeps the agent alive.
    let sub = fx
        .session
        .call("chat_subscribe", json!({"thread": "agent:agent-under-test"}), 10)
        .await?;
    assert!(sub.get("error").is_none(), "subscribe failed: {sub}");

    let response = post(
        fx.port,
        "/callback/mira?label=ci",
        Some(&fx.token),
        "build failed on lint",
    )?;

    assert!(status_line(&response).contains("200"), "always 200: {response}");
    let body = body_json(&response);
    assert_eq!(body["ok"], json!(true), "callback rejected: {body}");
    assert_eq!(body["target"], json!("mira"));

    let note = fx.session.next_notification(Duration::from_secs(10)).await?;
    assert_eq!(
        note.pointer("/params/content").and_then(|v| v.as_str()),
        Some("build failed on lint")
    );
    assert_eq!(
        note.pointer("/params/meta/source").and_then(|v| v.as_str()),
        Some("callback"),
        "the source must be stated, not inferred — inferring it is what produced F-14"
    );
    assert_eq!(
        note.pointer("/params/meta/label").and_then(|v| v.as_str()),
        Some("ci")
    );
    Ok(())
}

/// Without the right token the endpoint is an open door into a session running
/// with permissions skipped.
#[tokio::test]
async fn a_wrong_or_missing_token_is_refused() -> Result<()> {
    let fx = Fixture::start("atlas").await?;

    let no_token = body_json(&post(fx.port, "/callback/atlas", None, "hello")?);
    assert_eq!(no_token["ok"], json!(false), "a tokenless call must not deliver");

    let wrong = body_json(&post(fx.port, "/callback/atlas", Some(&"a".repeat(64)), "hello")?);
    assert_eq!(wrong["ok"], json!(false), "a wrong token must not deliver");
    assert_eq!(wrong["error"], json!("unauthorized"));
    Ok(())
}

/// A token addresses ONE session. If it opened any of them, a single leaked
/// debugging script would expose every session on the machine.
#[tokio::test]
async fn a_token_does_not_open_another_session() -> Result<()> {
    let mut fx = Fixture::start("mira").await?;

    let resp = fx
        .session
        .call(
            "agent_register",
            json!({
                "agent_id": "second-agent",
                "name": "atlas",
                "session_token": "tok2",
                "cwd": "/tmp/other",
                "host": "testhost",
                "pid": std::process::id(),
            }),
            2,
        )
        .await?;
    assert!(resp.get("error").is_none(), "second register failed: {resp}");

    let crossed = body_json(&post(fx.port, "/callback/atlas", Some(&fx.token), "hello")?);
    assert_eq!(
        crossed["ok"],
        json!(false),
        "mira's token opened atlas — one leaked script would expose every session"
    );
    Ok(())
}

/// Always 200, but the body has to distinguish "delivered" from "you typed the
/// nick wrong" — otherwise a debugging session spends its time chasing a
/// delivery that never started.
#[tokio::test]
async fn an_unknown_nick_answers_200_but_says_it_failed() -> Result<()> {
    let fx = Fixture::start("mira").await?;

    let response = post(fx.port, "/callback/mria", Some(&fx.token), "typo")?;
    assert!(status_line(&response).contains("200"), "still 200: {response}");
    let body = body_json(&response);
    assert_eq!(body["ok"], json!(false));
    assert!(
        body["error"].as_str().unwrap_or_default().contains("mria"),
        "the error must name what was not found: {body}"
    );
    Ok(())
}

/// The body is pasted into a pty. An escape sequence in it would drive the
/// operator's terminal past the model entirely.
#[tokio::test]
async fn control_sequences_never_reach_the_session() -> Result<()> {
    let mut fx = Fixture::start("mira").await?;

    fx.session
        .call("chat_subscribe", json!({"thread": "agent:agent-under-test"}), 10)
        .await?;

    let hostile = "before\x1b[2J\x1b[200~after";
    let body = body_json(&post(fx.port, "/callback/mira", Some(&fx.token), hostile)?);
    assert_eq!(body["ok"], json!(true));

    let note = fx.session.next_notification(Duration::from_secs(10)).await?;
    let content = note
        .pointer("/params/content")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(!content.contains('\x1b'), "ESC reached the session: {content:?}");
    assert!(content.contains("before") && content.contains("after"));
    Ok(())
}

/// `--reveal` is the only way to see a token; the default must not print it,
/// because whatever captured the output keeps it.
#[test]
fn the_default_token_command_prints_a_fingerprint_not_the_secret() {
    let home = tempfile::tempdir().expect("home");
    let bin = env!("CARGO_BIN_EXE_claudebase");

    // Mint a token without a daemon by rotating into a fresh database.
    let rotated = std::process::Command::new(bin)
        .args(["daemon", "callback", "rotate", "solo", "--reveal"])
        .env("HOME", home.path())
        .output()
        .expect("rotate");
    assert!(rotated.status.success(), "rotate failed: {rotated:?}");
    let token = String::from_utf8_lossy(&rotated.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    assert_eq!(token.len(), 64);

    let shown = std::process::Command::new(bin)
        .args(["daemon", "callback", "token", "solo"])
        .env("HOME", home.path())
        .output()
        .expect("token");
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(
        !text.contains(&token),
        "the default output leaked the token:\n{text}"
    );
    assert!(text.contains("fingerprint:"), "expected a fingerprint:\n{text}");
}
