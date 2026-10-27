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

/// Flags through the CLI: set on add and later, filtered open and closed,
/// closed and reopened by a session, gathered by `agenda`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entries_carry_work_items_a_session_closes_and_reopens() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let s = Session { id: "agent-gamma", token: "tok-gamma", dir: home.join("work/gamma") };
    std::fs::create_dir_all(&s.dir).unwrap();
    let _daemon = Daemon(spawn_daemon_with_home(&home).expect("daemon spawned"));
    let socket = socket_under(&home);
    wait_for_socket(&socket, Duration::from_secs(10)).await.expect("socket");
    let mut client = Client::connect(&socket).await.expect("connect");
    let r = client
        .call("agent_register", json!({ "agent_id": s.id, "name": "gamma", "session_token": s.token, "cwd": s.dir.to_str().unwrap() }), 1)
        .await
        .unwrap();
    assert!(payload(&r).is_some(), "{r}");

    let h = home.clone();
    tokio::task::spawn_blocking(move || {
        let home = h;
        let j = |args: &[&str]| json_out(journal(&home, &s.dir, Some(&s), args));
        let todo = j(&["add", "run the benchmark", "--todo", "--tag", "perf", "--json"])["id"].as_i64().unwrap();
        let note = j(&["add", "the bus drops frames under load", "--category", "finding", "--tag", "bus", "--json"])["id"].as_i64().unwrap();
        let marked = j(&["mark", &note.to_string(), "--problem", "--important", "--json"]);
        assert_eq!(marked["changed"], json!(["problem", "important"]));

        let got = j(&["get", &todo.to_string(), "--json"]);
        assert_eq!((got["entry"]["category"].as_str(), got["entry"]["flags"][0]["open"].as_bool()), (Some("todo"), Some(true)));
        let open = j(&["list", "--todo", "--problem", "--open", "--json"]);
        assert_eq!(open["total"], 2, "{open}");

        // A todo closes only on evidence: refused first, then a verified entry
        // in its branch lets it close (with the branch).
        journal(&home, &s.dir, Some(&s), &["close", &todo.to_string()])
            .failure()
            .stderr(predicates::str::contains("--verified"));
        let proof = j(&["add", &format!("Verifies journal#{todo}: benchmark ran, 3x faster"), "--category", "verification", "--verified", "--tag", "perf", "--json"])["id"].as_i64().unwrap();
        let tree = journal(&home, &s.dir, Some(&s), &["branch", &proof.to_string()]).success().get_output().stdout.clone();
        let tree = String::from_utf8_lossy(&tree);
        assert!(tree.contains(&format!("branch of todo #{todo} — verified (verified in #{proof})")) && tree.contains(&format!("verifies #{proof}")), "{tree}");
        let closed = j(&["close", &todo.to_string(), "--resolution", "done in 7935425", "--json"]);
        assert_eq!(closed["changed"], json!(["todo"]));
        let f = &closed["entry"]["flags"][0];
        assert_eq!((f["open"].as_bool(), f["closed_by"].as_str(), f["resolution"].as_str()), (Some(false), Some("gamma"), Some("done in 7935425")));
        assert_eq!(j(&["list", "--todo", "--closed", "--json"])["total"], 1);
        assert_eq!(j(&["list", "--todo", "--open", "--json"])["total"], 0);
        journal(&home, &s.dir, Some(&s), &["close", &note.to_string(), "--important"])
            .failure()
            .stderr(predicates::str::contains("unmark"));

        let agenda = j(&["agenda", "--json"]);
        let flags: Vec<&str> = agenda["agenda"].as_array().unwrap().iter().map(|g| g["flag"].as_str().unwrap()).collect();
        assert_eq!(flags, vec!["problem", "important"], "{agenda}");

        // An agent's suggestion for the harness is a work item; a deprecated
        // entry is history, not something open.
        j(&["add", "pool workers should not load the operator's persona", "--ai-feedback", "--tag", "pool", "--json"]);
        j(&["mark", &note.to_string(), "--deprecated", "--json"]);
        let agenda = j(&["agenda", "--json"]);
        let flags: Vec<&str> = agenda["agenda"].as_array().unwrap().iter().map(|g| g["flag"].as_str().unwrap()).collect();
        assert_eq!(flags, vec!["problem", "ai-feedback", "important"], "{agenda}");
        j(&["unmark", &note.to_string(), "--deprecated", "--json"]);

        // The graph: a reference in the text is a link both ends show; an
        // explicit one can be added and removed; `get` lists the neighbours.
        let fix = j(&["add", &format!("Fixes journal#{note}: frames are buffered now"), "--category", "update", "--tag", "bus", "--json"])["id"].as_i64().unwrap();
        let got = j(&["get", &note.to_string(), "--json"]);
        assert_eq!(got["entry"]["links"], json!([{ "kind": "fixes", "dir": "in", "id": fix }]));
        assert_eq!(got["related"]["linked"][0][1]["id"], fix, "{got}");
        assert_eq!(j(&["link", &todo.to_string(), &note.to_string(), "--json"])["added"], true);
        let human = journal(&home, &s.dir, Some(&s), &["get", &note.to_string()]).success().get_output().stdout.clone();
        let human = String::from_utf8_lossy(&human);
        assert!(human.contains("related:") && human.contains(&format!("fixed by #{fix}")) && human.contains(&format!("cited by #{todo}")), "{human}");
        let listed = journal(&home, &s.dir, Some(&s), &["list", "--tag", "bus"]).success().get_output().stdout.clone();
        assert!(String::from_utf8_lossy(&listed).contains(&format!("↳ fixed by #{fix} · cited by #{todo}")), "{}", String::from_utf8_lossy(&listed));
        assert_eq!(j(&["unlink", &todo.to_string(), &note.to_string(), "--json"])["removed"], 1);
        journal(&home, &s.dir, Some(&s), &["unlink", &todo.to_string(), &note.to_string()]).failure();

        j(&["reopen", &todo.to_string(), "--json"]);
        assert_eq!(j(&["list", "--todo", "--open", "--json"])["total"], 1);
        // Reading needs no session; closing does.
        journal(&home, &s.dir, None, &["agenda"]).success().stdout(predicates::str::contains("run the benchmark"));
        journal(&home, &s.dir, None, &["close", &todo.to_string()]).failure().stderr(predicates::str::contains("claudebase run"));
        let human = journal(&home, &s.dir, Some(&s), &["list", "--problem"]).success().get_output().stdout.clone();
        assert!(String::from_utf8_lossy(&human).contains("· problem · important"), "{}", String::from_utf8_lossy(&human));
    })
    .await
    .unwrap();

    // Flags travel with `chat dump` / `chat load`: an imported entry keeps
    // them, closed ones closed with how.
    let target = home.join("work/loaded");
    std::fs::create_dir_all(&target).unwrap();
    let flags = json!([{ "flag": "question", "open": false, "set_at": 5, "closed_at": 6, "closed_by": "mira", "resolution": "yes" }, { "flag": "todo", "open": true, "set_at": 5 }]);
    let r = client
        .call("journal_import", json!({ "cwd": target.to_str().unwrap(), "entries": [{ "body": "carried over", "category": "finding", "tags": ["x"], "created_at": 5, "flags": flags }] }), 2)
        .await
        .unwrap();
    assert_eq!(payload(&r).unwrap()["written"], 1, "{r}");
    let r = client.call("journal_list", json!({ "cwd": target.to_str().unwrap() }), 3).await.unwrap();
    let f = &payload(&r).unwrap()["entries"][0]["flags"];
    assert_eq!(f, &json!([{ "flag": "todo", "open": true, "set_at": 5 }, { "flag": "question", "open": false, "set_at": 5, "closed_at": 6, "closed_by": "mira", "resolution": "yes" }]), "{f}");
    drop(client);
}

/// `journal share` / `unshare` / `shares` and the operator's `feedback`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_shares_a_journal_live_and_leaves_with_a_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let a = Session { id: "agent-a", token: "tok-a", dir: home.join("work/a") };
    let b = Session { id: "agent-b", token: "tok-b", dir: home.join("work/b-worktree") };
    for s in [&a, &b] {
        std::fs::create_dir_all(&s.dir).unwrap();
    }
    let _daemon = Daemon(spawn_daemon_with_home(&home).expect("daemon spawned"));
    let socket = socket_under(&home);
    wait_for_socket(&socket, Duration::from_secs(10)).await.expect("socket");
    let mut connections = Vec::new();
    for (n, s) in [&a, &b].iter().enumerate() {
        let mut c = Client::connect(&socket).await.expect("connect");
        let r = c
            .call("agent_register", json!({ "agent_id": s.id, "name": s.id, "session_token": s.token, "cwd": s.dir.to_str().unwrap() }), n as u32 + 1)
            .await
            .unwrap();
        assert!(payload(&r).is_some(), "{r}");
        connections.push(c);
    }

    let h = home.clone();
    tokio::task::spawn_blocking(move || {
        let home = h;
        journal(&home, &a.dir, Some(&a), &["add", "a knows this", "--category", "finding", "--tag", "x"]).success();
        journal(&home, &b.dir, Some(&b), &["add", "b found this first", "--category", "finding", "--tag", "x"]).success();
        journal(&home, &b.dir, Some(&b), &["add", "an idea for claudebase", "--ai-feedback", "--tag", "x"]).success();

        // Shared from a's folder: b's entries come over, b's file moves aside.
        let shared = json_out(journal(&home, &a.dir, Some(&a), &["share", b.dir.to_str().unwrap(), "--json"]));
        assert_eq!((shared["written"].as_u64(), shared["project"].as_str()), (Some(2), a.dir.to_str()), "{shared}");
        assert!(shared["moved_aside"].is_string());
        // From now on b's session writes into a's journal, and reads it.
        journal(&home, &b.dir, Some(&b), &["add", "written from b after sharing", "--category", "finding", "--tag", "x"]).success();
        let seen = json_out(journal(&home, &a.dir, Some(&a), &["list", "--json"]));
        assert_eq!(seen["total"], 4, "{seen}");
        let from_b = json_out(journal(&home, &b.dir, None, &["list", "--json"]));
        assert_eq!((from_b["project"].as_str(), from_b["total"].as_i64()), (a.dir.to_str(), Some(4)));
        let all = json_out(journal(&home, &a.dir, None, &["shares", "--json"]));
        assert_eq!(all["shares"].as_array().unwrap().len(), 1);
        // Sharing again is a no-op; the repository case is refused elsewhere.
        assert_eq!(json_out(journal(&home, &a.dir, None, &["share", b.dir.to_str().unwrap(), "--json"]))["already"], true);

        // The operator's cross-project view; a session may not read it.
        journal(&home, &a.dir, Some(&a), &["feedback"]).failure().stderr(predicates::str::contains("operator"));
        let fb = json_out(journal(&home, &a.dir, None, &["feedback", "--json"]));
        assert_eq!(fb["projects"][0]["entries"][0]["body"], "an idea for claudebase", "{fb}");

        // Unshared: b keeps a copy as its own, and the two diverge.
        let un = json_out(journal(&home, &a.dir, None, &["unshare", b.dir.to_str().unwrap(), "--json"]));
        assert!(un["forked"].is_string(), "{un}");
        journal(&home, &b.dir, Some(&b), &["add", "only in b's own journal now", "--category", "finding", "--tag", "x"]).success();
        let bl = json_out(journal(&home, &b.dir, Some(&b), &["list", "--json"]));
        assert_eq!((bl["project"].as_str(), bl["total"].as_i64()), (b.dir.to_str(), Some(5)));
        assert_eq!(json_out(journal(&home, &a.dir, Some(&a), &["list", "--json"]))["total"], 4);
        journal(&home, &a.dir, None, &["unshare", b.dir.to_str().unwrap()]).failure().stderr(predicates::str::contains("not shared"));
    })
    .await
    .unwrap();
    drop(connections);
}
