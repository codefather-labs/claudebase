//! A session on one machine joining the daemon of another
//! (docs/design/remote-daemon.md). Two "machines" are two HOMEs on this host:
//! `prod` runs the daemon with its TLS listener on loopback, `laptop` has only a
//! profile pointing at it. Everything the laptop does goes through the CLI with
//! `CLAUDEBASE_DAEMON=prod`, exactly as a session started with
//! `claudebase run --daemon prod` would.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command;
use claudebase::daemon::ipc::{read_frame, write_frame};
use claudebase::daemon::remote::{self, BoxRead, BoxWrite, Endpoint, Remote};
use serde_json::{json, Value};

fn cli(home: &Path, daemon: Option<&str>, session: Option<(&str, &str)>, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("claudebase").unwrap();
    cmd.current_dir(home)
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env_remove("CLAUDEBASE_AGENT_ID")
        .env_remove("CLAUDEBASE_SESSION_TOKEN")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove(remote::DAEMON_ENV)
        .args(args);
    if let Some(d) = daemon {
        cmd.env(remote::DAEMON_ENV, d);
    }
    if let Some((id, token)) = session {
        cmd.env("CLAUDEBASE_AGENT_ID", id).env("CLAUDEBASE_SESSION_TOKEN", token);
    }
    cmd
}

fn stdout(mut cmd: Command) -> String {
    let out = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8_lossy(&out).into_owned()
}

/// The framed protocol over TLS, as the supervisor speaks it.
struct TlsClient {
    read: BoxRead,
    write: BoxWrite,
}

impl TlsClient {
    async fn connect(remote: &Remote) -> anyhow::Result<TlsClient> {
        let (read, write) =
            remote::connect(&Endpoint::Remote { profile: "prod".into(), remote: remote.clone() }).await?;
        Ok(TlsClient { read, write })
    }

    async fn call(&mut self, tool: &str, arguments: Value, id: u64) -> Value {
        let body = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":arguments}});
        write_frame(&mut self.write, &serde_json::to_vec(&body).unwrap()).await.unwrap();
        loop {
            let v: Value = serde_json::from_slice(&read_frame(&mut self.read).await.unwrap()).unwrap();
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                return v;
            }
        }
    }

    async fn next_notification(&mut self, within: Duration) -> Value {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let frame = tokio::time::timeout_at(deadline, read_frame(&mut self.read)).await.expect("no notification").unwrap();
            let v: Value = serde_json::from_slice(&frame).unwrap();
            if v.get("method").and_then(Value::as_str) == Some("notifications/claude/channel") {
                return v;
            }
        }
    }
}

struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        common::stop(&mut self.0);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_laptop_session_joins_the_prod_daemon_and_talks_to_its_agents() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (prod, laptop): (PathBuf, PathBuf) = (root.join("prod"), root.join("laptop"));
    for h in [&prod, &laptop] {
        std::fs::create_dir_all(h.join("run")).unwrap();
    }
    // Two checkouts of one repository, at different paths on the two machines.
    let laptop_project = laptop.join("work/proj");
    let prod_project = prod.join("srv/proj-checkout");
    for dir in [&laptop_project, &prod_project] {
        std::fs::create_dir_all(dir).unwrap();
        for args in [vec!["init", "-q"], vec!["remote", "add", "origin", "git@github.com:acme/shop.git"]] {
            assert!(std::process::Command::new("git").arg("-C").arg(dir).args(&args).status().unwrap().success());
        }
    }
    let port = free_port();

    // prod: enable, issue a token for the laptop, start the daemon.
    let (p, l) = (prod.clone(), laptop.clone());
    let url = tokio::task::spawn_blocking(move || {
        stdout(cli(&p, None, None, &["daemon", "remote", "enable", "--bind", &format!("127.0.0.1:{port}")]));
        let issued = stdout(cli(&p, None, None, &["daemon", "remote", "issue", "laptop", "--host", "127.0.0.1"]));
        let url = issued.lines().find(|l| l.starts_with("claudebase://")).expect("connection string").to_owned();
        stdout(cli(&l, None, None, &["daemon", "remote", "add", "prod", &url]));
        url
    })
    .await
    .unwrap();
    let remote_cfg = Remote::parse(&url).unwrap();

    let _d = Daemon(common::spawn_daemon_with_home(&prod).expect("daemon"));
    common::wait_for_socket(&common::socket_under(&prod), Duration::from_secs(10)).await.unwrap();
    // The TLS listener starts right after the socket.
    let mut tls = None;
    for _ in 0..50 {
        if let Ok(c) = TlsClient::connect(&remote_cfg).await {
            tls = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut laptop_session = tls.expect("TLS listener up");

    // An agent running on prod, on prod's own socket.
    let mut worker = common::Client::connect(&common::socket_under(&prod)).await.unwrap();
    let r = worker
        .call("agent_register", json!({"agent_id":"prod-agent","name":"worker","session_token":"tok-p","cwd":prod_project.to_str().unwrap()}), 1)
        .await
        .unwrap();
    assert!(common::payload(&r).is_some(), "{r}");
    worker.call("chat_subscribe", json!({"thread":"agent:prod-agent"}), 2).await.unwrap();

    // The laptop session, over TLS — what `claudebase run --daemon prod` registers.
    let r = laptop_session
        .call(
            "agent_register",
            json!({"agent_id":"lap-agent","name":"laptop","session_token":"tok-l","cwd":laptop_project.to_str().unwrap(),"host":"laptop-host",
                   "project_id": claudebase::project_id::resolve_project_id(&laptop_project)}),
            1,
        )
        .await;
    assert!(r.get("error").is_none(), "{r}");
    laptop_session.call("chat_subscribe", json!({"thread":"agent:lap-agent"}), 2).await;

    // Administration is not reachable over the network.
    let refused = laptop_session.call("agent_reap", json!({}), 3).await;
    assert_eq!(refused["error"]["code"], -32601, "{refused}");

    let (l, p) = (laptop.clone(), prod.clone());
    let lp = laptop_project.clone();
    let pp = prod_project.clone();
    tokio::task::spawn_blocking(move || {
        let lap = Some(("lap-agent", "tok-l"));
        // laptop -> prod agent, by nick, resolved on prod.
        let out = stdout(cli(&l, Some("prod"), lap, &["agent", "send", "hello from the laptop", "--agent_nick", "worker"]));
        assert!(!out.is_empty());
        // prod agent -> laptop, from prod's own terminal.
        stdout(cli(&p, None, Some(("prod-agent", "tok-p")), &["agent", "send", "hello from prod", "--agent_nick", "laptop"]));

        // The laptop sees prod's sessions, and knows where it is.
        let list = stdout(cli(&l, Some("prod"), lap, &["agent", "list"]));
        assert!(list.contains("worker") && list.contains("laptop"), "{list}");
        let who = stdout(cli(&l, Some("prod"), lap, &["agent", "whoami"]));
        assert!(who.contains("nick: laptop") && who.contains("daemon: prod (127.0.0.1"), "{who}");

        // The journal lives on prod.
        let mut add = cli(&l, Some("prod"), lap, &["journal", "add", "decided on prod", "--category", "decision", "--tag", "remote"]);
        add.current_dir(&lp);
        add.assert().success();
        let journals = |h: &Path| std::fs::read_dir(h.join(".claude/claudebase/journals")).map(|d| d.count()).unwrap_or(0);
        assert!(journals(&p) >= 1, "the journal is on prod");
        assert_eq!(journals(&l), 0, "nothing on the laptop");
        // …and it is the repository's journal: the prod agent, in its own
        // checkout at another path, reads what the laptop wrote.
        let mut read = cli(&p, None, Some(("prod-agent", "tok-p")), &["journal", "list", "--json"]);
        read.current_dir(&pp);
        let listed: Value = serde_json::from_str(&stdout(read)).unwrap();
        assert_eq!(listed["project"], "git:github.com/acme/shop", "{listed}");
        assert_eq!(listed["entries"][0]["body"], "decided on prod", "{listed}");

        // Inspection of a local chat.db says it is local-only, not silently wrong.
        cli(&l, Some("prod"), None, &["chat", "list"]).assert().code(2);
        // An unknown profile never falls back to the local daemon.
        cli(&l, Some("staging"), lap, &["agent", "list"]).assert().failure();
    })
    .await
    .unwrap();

    let got = worker.next_notification(Duration::from_secs(5)).await.unwrap();
    assert!(got.to_string().contains("hello from the laptop"), "{got}");
    let got = laptop_session.next_notification(Duration::from_secs(5)).await;
    assert!(got.to_string().contains("hello from prod"), "{got}");

    // The laptop hands prod a Claude token; prod applies it, never gives it back.
    let (l, p) = (laptop.clone(), prod.clone());
    tokio::task::spawn_blocking(move || {
        let token = "sk-ant-oat01-TESTTOKENabcdefghijklmnopqrstuvwxyz0123456789";
        let mut set = cli(&l, Some("prod"), None, &["auth", "setuptoken"]);
        set.write_stdin(format!("{token}\n"));
        let out = stdout(set);
        assert!(out.contains("…6789") && !out.contains(token), "{out}");
        let stored = std::fs::read_to_string(p.join(".claude/claudebase/auth.toml")).unwrap();
        assert!(stored.contains(token), "stored on prod");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(p.join(".claude/claudebase/auth.toml")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let profile = std::fs::read_to_string(p.join(".profile")).unwrap();
            assert!(profile.contains("claudebase auth") && !profile.contains(token), "{profile}");
            let env = std::fs::read_to_string(p.join(".claude/claudebase/auth.env")).unwrap();
            assert!(env.contains("CLAUDE_CODE_OAUTH_TOKEN='sk-ant-oat01-TESTTOKEN"), "{env}");
        }
        assert!(!l.join(".claude/claudebase/auth.toml").exists(), "nothing stored on the laptop");
        let st = stdout(cli(&l, Some("prod"), None, &["auth", "status"]));
        assert!(st.contains("…6789") && st.contains("remote client `laptop`") && !st.contains(token), "{st}");
        let cleared = stdout(cli(&l, Some("prod"), None, &["auth", "clear-token"]));
        assert!(cleared.contains("removed"), "{cleared}");
        assert!(!p.join(".claude/claudebase/auth.toml").exists());
        #[cfg(unix)]
        assert!(!std::fs::read_to_string(p.join(".profile")).unwrap().contains("claudebase auth"));
    })
    .await
    .unwrap();

    // Revoking the laptop: a new connection is refused.
    let (l, p) = (laptop.clone(), prod.clone());
    tokio::task::spawn_blocking(move || {
        stdout(cli(&p, None, None, &["daemon", "remote", "revoke", "laptop"]));
        cli(&l, None, None, &["daemon", "remote", "test", "prod"])
            .assert()
            .failure()
            .stderr(predicates::str::contains("unknown or revoked token"));
    })
    .await
    .unwrap();
}
