//! `claudebase journal …` end to end: the CLI talks to a live daemon on a temp
//! HOME, the daemon decides the project from the session, and journals never
//! mix. Two "sessions" are registered the way `claudebase run` registers them
//! (agent id, session token, the directory they started in).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command;
use common::{payload, socket_under, spawn_daemon_with_home, stop, wait_for_socket, Client};
use serde_json::{json, Value};

struct Session {
    id: &'static str,
    token: &'static str,
    dir: PathBuf,
}

fn journal(home: &Path, cwd: &Path, session: Option<&Session>, args: &[&str]) -> assert_cmd::assert::Assert {
    journal_cmd(home, cwd, session, args).assert()
}

fn journal_cmd(home: &Path, cwd: &Path, session: Option<&Session>, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("claudebase").unwrap();
    cmd.current_dir(cwd)
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env_remove("CLAUDEBASE_AGENT_ID")
        .env_remove("CLAUDEBASE_SESSION_TOKEN")
        .env("CLAUDE_CODE_SESSION_ID", "conv-1")
        .arg("journal")
        .args(args);
    if let Some(s) = session {
        cmd.env("CLAUDEBASE_AGENT_ID", s.id).env("CLAUDEBASE_SESSION_TOKEN", s.token);
    }
    cmd
}

fn json_out(a: assert_cmd::assert::Assert) -> Value {
    let out = a.success().get_output().stdout.clone();
    serde_json::from_slice(&out).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journals_belong_to_the_directory_a_session_started_in() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let alpha = Session { id: "agent-alpha", token: "tok-alpha", dir: home.join("work/alpha") };
    let beta = Session { id: "agent-beta", token: "tok-beta", dir: home.join("work/beta") };
    for s in [&alpha, &beta] {
        std::fs::create_dir_all(s.dir.join("src/deep")).unwrap();
    }

    // alpha had insights before the journal existed; they come across.
    let legacy = alpha.dir.join(".claude/knowledge/insights.db");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let old = rusqlite::Connection::open(&legacy).unwrap();
    old.execute_batch(
        "CREATE TABLE documents (id INTEGER PRIMARY KEY, ingested_at INTEGER, source_type TEXT, agent_name TEXT, \
           session_id TEXT, feature_slug TEXT, salience TEXT, parent_artifact TEXT);
         CREATE TABLE chunks (id INTEGER PRIMARY KEY, doc_id INTEGER, ord INTEGER, text TEXT);
         CREATE TABLE insight_tags (doc_id INTEGER, tag TEXT);
         INSERT INTO documents VALUES (1, 100, 'agent-learned', 'planner', NULL, NULL, 'high', NULL);
         INSERT INTO chunks VALUES (1, 1, 0, 'the old lesson about migrations');
         INSERT INTO insight_tags VALUES (1, 'migrations');",
    )
    .unwrap();
    drop(old);

    // Stopped on drop, so a failed assertion cannot leave the daemon running
    // (and the test binary waiting on it).
    let _daemon = Daemon(spawn_daemon_with_home(&home).expect("daemon spawned"));
    let socket = socket_under(&home);
    wait_for_socket(&socket, Duration::from_secs(10)).await.expect("socket");
    // One connection per session, kept open: the daemon holds one agent per
    // connection, and a session is alive while its connection is.
    let mut connections = Vec::new();
    for (n, s) in [&alpha, &beta].iter().enumerate() {
        let mut client = Client::connect(&socket).await.expect("connect");
        let r = client
            .call(
                "agent_register",
                json!({ "agent_id": s.id, "name": s.id, "session_token": s.token, "cwd": s.dir.to_str().unwrap() }),
                n as u32 + 1,
            )
            .await
            .unwrap();
        assert!(payload(&r).is_some(), "{r}");
        connections.push(client);
    }

    let home2 = home.clone();
    let (a, b) = (alpha, beta);
    tokio::task::spawn_blocking(move || {
        let home = home2;
        // Written from a subdirectory, filed under the session's project.
        let out = json_out(journal(&home, &a.dir.join("src/deep"), Some(&a), &[
            "add", "Chose WAL mode for the journal db", "--category", "Decision", "--tag", "sqlite", "--tag", "#Locking", "--json",
        ]));
        assert_eq!(out["status"], "written");
        assert_eq!(out["project"], a.dir.to_str().unwrap());
        let id = out["id"].as_i64().unwrap();
        // The same note again is not a second entry.
        let again = json_out(journal(&home, &a.dir, Some(&a), &["add", "Chose WAL mode for the journal db", "--category", "decision", "--tag", "sqlite", "--json"]));
        assert_eq!((again["status"].as_str(), again["id"].as_i64()), (Some("deduped"), Some(id)));
        journal_cmd(&home, &a.dir, Some(&a), &["add", "-", "--category", "finding", "--tag", "fts5", "--salience", "low"])
            .write_stdin("FTS5 chokes on codefather.dev unless quoted")
            .assert()
            .success();
        journal(&home, &b.dir, Some(&b), &["add", "beta uses postgres", "--category", "decision", "--tag", "postgres"]).success();

        // Categories and tags are per project.
        let cats = json_out(journal(&home, &a.dir, Some(&a), &["categories", "--json"]));
        let names: Vec<&str> = cats["categories"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 3, "{cats}");
        assert!(names.contains(&"decision") && names.contains(&"finding") && names.contains(&"agent-learned"));
        let tags = json_out(journal(&home, &a.dir, Some(&a), &["tags", "--json"]));
        let tags: Vec<&str> = tags["tags"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(tags.contains(&"locking") && tags.contains(&"migrations"), "{tags:?}");
        assert!(!tags.contains(&"postgres"), "beta's tags leaked into alpha: {tags:?}");
        let btags = json_out(journal(&home, &b.dir, Some(&b), &["tags", "--json"]));
        assert_eq!(btags["tags"].as_array().unwrap().len(), 1);

        // Search and list stay inside the project, and filter.
        let hits = json_out(journal(&home, &a.dir, Some(&a), &["search", "postgres", "--json"]));
        assert!(hits["hits"].as_array().unwrap().is_empty(), "{hits}");
        let hits = json_out(journal(&home, &a.dir, Some(&a), &["search", "codefather.dev", "--json"]));
        assert_eq!(hits["hits"][0]["category"], "finding");
        assert_eq!(hits["hits"][0]["session_id"], "conv-1");
        let listed = json_out(journal(&home, &a.dir, Some(&a), &["list", "--category", "decision", "--tag", "locking", "--json"]));
        assert_eq!(listed["total"], 1);
        assert_eq!(listed["entries"][0]["agent"], "agent-alpha", "the nick stands in for --agent");

        // A plain terminal reads the journal of the project it is inside.
        let plain = json_out(journal(&home, &a.dir.join("src"), None, &["list", "--json"]));
        assert_eq!((plain["project"].as_str(), plain["total"].as_i64()), (a.dir.to_str(), Some(3)));
        // …but cannot write.
        journal(&home, &a.dir, None, &["add", "x", "--category", "decision", "--tag", "t"])
            .failure()
            .stderr(predicates::str::contains("claudebase run"));
        // A directory without a journal reads as empty and gets no file.
        let elsewhere = home.join("work");
        let empty = json_out(journal(&home, &elsewhere, None, &["categories", "--json"]));
        assert!(empty["categories"].as_array().unwrap().is_empty());

        // Every journal the daemon holds.
        let all = json_out(journal(&home, &elsewhere, None, &["projects", "--json"]));
        assert_eq!(all["journals"].as_array().unwrap().len(), 2, "{all}");
        let files: Vec<PathBuf> = std::fs::read_dir(home.join(".claude/claudebase/journals"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "db"))
            .collect();
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(!a.dir.join(".claude/journal.db").exists(), "nothing is written into the project");

        // Get, delete (sessions only), and entries the other project cannot touch.
        let got = json_out(journal(&home, &a.dir, Some(&a), &["get", &id.to_string(), "--json"]));
        assert_eq!(got["entry"]["tags"], json!(["sqlite", "locking"]));
        journal(&home, &b.dir, Some(&b), &["delete", &id.to_string()]).failure();
        journal(&home, &a.dir, Some(&a), &["delete", &id.to_string()]).success();
        journal(&home, &a.dir, Some(&a), &["get", &id.to_string()]).failure();

        // Validation is the daemon's, and its message reaches the operator.
        journal(&home, &a.dir, Some(&a), &["add", "no tags", "--category", "decision"])
            .failure()
            .stderr(predicates::str::contains("--tag"));
        // A wrong token is not a session.
        let forged = Session { id: "agent-alpha", token: "nope", dir: a.dir.clone() };
        journal(&home, &a.dir, Some(&forged), &["add", "x", "--category", "decision", "--tag", "t"])
            .failure()
            .stderr(predicates::str::contains("not registered"));

        let human = journal(&home, &a.dir, Some(&a), &["list"]).success().get_output().stdout.clone();
        let human = String::from_utf8_lossy(&human);
        assert!(human.contains("finding [fts5] low"), "{human}");
    })
    .await
    .unwrap();
    drop(connections);
}

struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        stop(&mut self.0);
    }
}
