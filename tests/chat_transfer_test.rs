//! `claudebase chat dump` / `chat load` — a conversation carried from one
//! machine to another. Each "machine" is its own CLAUDE_CONFIG_DIR and its own
//! project directory, at different paths — and its own HOME, so a test never
//! reaches this machine's daemon or `chat.db`.

mod common;

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const ID: &str = "33333333-3333-4333-8333-333333333333";

struct Machine {
    home: PathBuf,
    claude: PathBuf,
    project: PathBuf,
}

impl Machine {
    fn new(root: &Path, name: &str) -> Machine {
        Machine::with_project(root, name, "proj")
    }

    fn with_project(root: &Path, name: &str, project: &str) -> Machine {
        let home = root.join(name).join("home");
        let claude = root.join(name).join("claude");
        let project = root.join(name).join("work").join(project);
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(home.join("run")).unwrap();
        Machine { home: home.canonicalize().unwrap(), claude, project: project.canonicalize().unwrap() }
    }

    fn folder(&self) -> PathBuf {
        self.claude.join("projects").join(claudebase::chat_clone::project_slug(&self.project))
    }

    fn run(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        Command::cargo_bin("claudebase")
            .unwrap()
            .current_dir(&self.project)
            .env("CLAUDE_CONFIG_DIR", &self.claude)
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", self.home.join("run"))
            .env_remove("CLAUDEBASE_AGENT_ID")
            .env_remove("CLAUDEBASE_SESSION_TOKEN")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .args(args)
            .assert()
    }
}

/// A conversation on machine `m`, with a subagent, a spilled tool result and a
/// /rewind backup, recorded at `m`'s project path.
fn seed(m: &Machine) {
    let cwd = m.project.to_str().unwrap();
    let folder = m.folder();
    fs::create_dir_all(&folder).unwrap();
    let lines = [
        serde_json::json!({"type":"user","uuid":"m1","parentUuid":null,"sessionId":ID,"cwd":cwd,
            "message":{"role":"user","content":format!("work in {cwd}")}}),
        serde_json::json!({"type":"assistant","uuid":"m2","parentUuid":"m1","sessionId":ID,
            "cwd":format!("{cwd}/sub"),"message":{"role":"assistant","content":"ok"}}),
        serde_json::json!({"type":"custom-title","customTitle":"socks5 routing","sessionId":ID}),
        serde_json::json!({"type":"bridge-session","bridgeSessionId":"cse_x","sessionId":ID}),
        serde_json::json!({"type":"user","uuid":"m3","parentUuid":"m2","sessionId":ID,"cwd":"/elsewhere",
            "message":{"role":"user","content":"outside the project"}}),
    ];
    let mut text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    text.push_str("{\"type\":\"user\",\"torn"); // being written right now
    fs::write(folder.join(format!("{ID}.jsonl")), text).unwrap();

    let sub = folder.join(ID).join("subagents");
    fs::create_dir_all(&sub).unwrap();
    fs::write(
        sub.join("agent-a1.jsonl"),
        format!("{}\n", serde_json::json!({"type":"user","isSidechain":true,"sessionId":ID,"cwd":cwd})),
    )
    .unwrap();
    let results = folder.join(ID).join("tool-results");
    fs::create_dir_all(&results).unwrap();
    fs::write(results.join("r1.txt"), "big tool output").unwrap();
    let fh = m.claude.join("file-history").join(ID);
    fs::create_dir_all(&fh).unwrap();
    fs::write(fh.join("abc@v1"), "backup bytes").unwrap();
}

fn read_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn archive_path(stdout: &[u8]) -> PathBuf {
    let text = String::from_utf8_lossy(stdout);
    PathBuf::from(text.lines().find_map(|l| l.strip_prefix("archive: ")).expect("archive line").trim())
}

#[test]
fn a_conversation_moves_to_another_machine_and_resumes_there() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Machine::new(tmp.path(), "a");
    let b = Machine::new(tmp.path(), "b-other-user");
    seed(&a);

    // Dump by the name /resume shows.
    let out = a.run(&["chat", "dump", "socks5 routing"]).success().get_output().stdout.clone();
    let archive = archive_path(&out);
    assert_eq!(archive, a.project.join("socks5-routing-33333333.zip"));

    // Carry it over: it sits in the other machine's project directory.
    let carried = b.project.join("project.zip");
    fs::copy(&archive, &carried).unwrap();
    let out = b.run(&["chat", "load", "./project.zip"]).success().get_output().stdout.clone();
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains(&format!("claude --resume {ID}")), "{text}");

    // Native on b: same id, filed under b's project, findable by its name.
    let transcript = b.folder().join(format!("{ID}.jsonl"));
    let lines = read_lines(&transcript);
    assert!(lines.iter().all(|v| v["sessionId"] == ID));
    let (a_cwd, b_cwd) = (a.project.to_str().unwrap(), b.project.to_str().unwrap());
    assert_eq!(lines[0]["cwd"], b_cwd);
    assert_eq!(lines[1]["cwd"], format!("{b_cwd}/sub"));
    // Paths outside the project, and free text, are history: left as written.
    assert_eq!(lines[3]["cwd"], "/elsewhere");
    assert_eq!(lines[0]["message"]["content"], format!("work in {a_cwd}"));
    assert!(!lines.iter().any(|v| v["type"] == "bridge-session"));
    assert!(lines.iter().any(|v| v["customTitle"] == "socks5 routing"));
    // The torn line never travelled.
    assert_eq!(lines.len(), 4);

    let sub = read_lines(&b.folder().join(ID).join("subagents/agent-a1.jsonl"));
    assert_eq!(sub[0]["cwd"], b_cwd);
    assert_eq!(
        fs::read_to_string(b.folder().join(ID).join("tool-results/r1.txt")).unwrap(),
        "big tool output"
    );
    assert_eq!(
        fs::read_to_string(b.claude.join("file-history").join(ID).join("abc@v1")).unwrap(),
        "backup bytes"
    );
    // No staging leftovers.
    let leftovers: Vec<_> = fs::read_dir(b.folder())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".claudebase-load"))
        .collect();
    assert!(leftovers.is_empty());

    // Once there, it is a conversation like any other: clone finds it by name.
    b.run(&["chat", "clone", "socks5 routing", "branch"]).success();
}

#[test]
fn loading_twice_is_refused_and_new_id_makes_a_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Machine::new(tmp.path(), "a");
    seed(&a);
    let out = a.run(&["chat", "dump", ID, "--out", "x.zip"]).success().get_output().stdout.clone();
    assert_eq!(archive_path(&out), PathBuf::from("x.zip"));

    // Loading back onto the machine it came from: the conversation is there.
    a.run(&["chat", "load", "x.zip"])
        .failure()
        .stderr(predicates::str::contains("already on this machine"));
    let out = a
        .run(&["chat", "load", "x.zip", "--new-id", "--name", "imported"])
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains(&format!("a copy of {ID}")), "{text}");
    let new_id = text.split_whitespace().nth(1).unwrap().to_owned();
    let lines = read_lines(&a.folder().join(format!("{new_id}.jsonl")));
    assert!(lines.iter().all(|v| v["sessionId"] == new_id.as_str()));
    assert_eq!(lines.last().unwrap()["agentName"], "imported");

    // Same machine, another project directory: the id is still taken.
    let elsewhere = a.project.parent().unwrap().join("other");
    fs::create_dir_all(&elsewhere).unwrap();
    Command::cargo_bin("claudebase")
        .unwrap()
        .current_dir(&elsewhere)
        .env("CLAUDE_CONFIG_DIR", &a.claude)
        .args(["chat", "load", a.project.join("x.zip").to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already on this machine"));

    // An existing archive is never overwritten.
    a.run(&["chat", "dump", ID, "--out", "x.zip"])
        .failure()
        .stderr(predicates::str::contains("x.zip"));
}

#[test]
fn without_a_source_dump_takes_the_session_it_runs_in() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Machine::new(tmp.path(), "a");
    seed(&a);
    let out = Command::cargo_bin("claudebase")
        .unwrap()
        .current_dir(&a.project)
        .env("CLAUDE_CONFIG_DIR", &a.claude)
        .env("CLAUDE_CODE_SESSION_ID", ID)
        .args(["chat", "dump"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&out).contains(&format!("dumped {ID}")));

    Command::cargo_bin("claudebase")
        .unwrap()
        .current_dir(&a.project)
        .env("CLAUDE_CONFIG_DIR", &a.claude)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .args(["chat", "dump"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("CLAUDE_CODE_SESSION_ID is not set"));
}

fn hostile_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    for (name, body) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(body).unwrap();
    }
    zip.finish().unwrap();
}

fn manifest() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "format":"claudebase-chat-dump","version":1,"session_id":ID,"title":null,
        "source_cwd":"/x","created_at":"2026-01-01T00:00:00Z","claudebase_version":"0"
    }))
    .unwrap()
}

#[test]
fn hostile_or_foreign_archives_are_refused_and_leave_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let b = Machine::new(tmp.path(), "b");
    let m = manifest();
    let line = format!("{{\"type\":\"user\",\"sessionId\":\"{ID}\"}}\n");

    for (file, entries, expect) in [
        (
            "slip.zip",
            vec![("manifest.json", m.as_slice()), ("transcript.jsonl", line.as_bytes()),
                 ("session/../../../../evil.txt", b"x".as_slice())],
            "unsafe path",
        ),
        (
            "odd.zip",
            vec![("manifest.json", m.as_slice()), ("transcript.jsonl", line.as_bytes()),
                 ("bin/run.sh", b"x".as_slice())],
            "unexpected entry",
        ),
        ("plain.zip", vec![("notes.txt", b"hi".as_slice())], "no manifest.json"),
        ("empty.zip", vec![("manifest.json", m.as_slice())], "no transcript.jsonl"),
    ] {
        hostile_zip(&b.project.join(file), &entries);
        b.run(&["chat", "load", file]).failure().stderr(predicates::str::contains(expect));
        assert!(!b.folder().join(format!("{ID}.jsonl")).exists(), "{file}");
        let left: Vec<_> = fs::read_dir(b.folder()).map(|d| d.flatten().collect()).unwrap_or_default();
        assert!(left.is_empty(), "{file} left {left:?}");
    }
    assert!(!tmp.path().join("evil.txt").exists());

    let future = serde_json::to_vec(&serde_json::json!({
        "format":"claudebase-chat-dump","version":99,"session_id":ID,
        "created_at":"x","claudebase_version":"9.9.9"
    }))
    .unwrap();
    hostile_zip(&b.project.join("future.zip"), &[("manifest.json", &future)]);
    b.run(&["chat", "load", "future.zip"])
        .failure()
        .stderr(predicates::str::contains("update it first"));
}

struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        common::stop(&mut self.0);
    }
}

async fn daemon_for(m: &Machine) -> Daemon {
    let d = Daemon(common::spawn_daemon_with_home(&m.home).expect("daemon spawned"));
    common::wait_for_socket(&common::socket_under(&m.home), std::time::Duration::from_secs(10))
        .await
        .expect("socket");
    d
}

/// The journal and the nick travel with the conversation — into a directory
/// with another name, at another path, on a machine with its own daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_journal_and_the_nick_travel_with_the_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Machine::new(tmp.path(), "a");
    let b = Machine::with_project(tmp.path(), "b", "renamed.on-b");
    seed(&a);

    let _da = daemon_for(&a).await;
    let a_dir = a.project.to_str().unwrap().to_owned();
    let mut client = common::Client::connect(&common::socket_under(&a.home)).await.unwrap();
    let entries = serde_json::json!([
        {"body": "Chose WAL for the journal", "category": "decision", "tags": ["sqlite"], "salience": "high",
         "created_at": 1_700_000_000, "agent": "planner", "session_id": ID,
         "source_artifact": format!("{a_dir}/src/journal.rs#L160")},
        {"body": "bumped zip to 2.x", "category": "update", "tags": ["deps"], "created_at": 1_700_000_100,
         "source_artifact": "cd3f1a1"},
    ]);
    let r = client
        .call("journal_import", serde_json::json!({"cwd": a_dir, "entries": entries}), 1)
        .await
        .unwrap();
    assert_eq!(common::payload(&r).unwrap()["written"], 2, "{r}");
    let chat = rusqlite::Connection::open(a.home.join(".claude/knowledge/chat.db")).unwrap();
    claudebase::daemon::agent_registry::remember_conversation_nick(&chat, ID, "tt-backend", 1).unwrap();
    drop(chat);

    let (a2, b2) = (a, b);
    let (a, b, archive) = tokio::task::spawn_blocking(move || {
        let out = a2.run(&["chat", "dump", ID]).success().get_output().stdout.clone();
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.contains("journal: 2 entries") && text.contains("nick: tt-backend"), "{text}");
        let archive = archive_path(&out);
        (a2, b2, archive)
    })
    .await
    .unwrap();

    let _db = daemon_for(&b).await;
    let carried = b.project.join("carried.zip");
    fs::copy(&archive, &carried).unwrap();
    tokio::task::spawn_blocking(move || {
        let out = b.run(&["chat", "load", "carried.zip"]).success().get_output().stdout.clone();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("journal: 2 entries written, 0 already there"), "{text}");
        assert!(text.contains("nick: `tt-backend`"), "{text}");

        // b's journal is the journal of b's directory, whatever it is called.
        let out = b.run(&["journal", "list", "--json"]).success().get_output().stdout.clone();
        let listed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(listed["project"], b.project.to_str().unwrap());
        assert_eq!(listed["total"], 2);
        let wal = listed["entries"].as_array().unwrap().iter().find(|e| e["category"] == "decision").unwrap();
        assert_eq!(wal["created_at"], 1_700_000_000, "the original creation time is kept");
        assert_eq!((wal["agent"].as_str(), wal["salience"].as_str()), (Some("planner"), Some("high")));
        assert_eq!(wal["session_id"], ID);
        assert_eq!(wal["source_artifact"], format!("{}/src/journal.rs#L160", b.project.display()));
        assert!(wal.get("agent_id").is_none());
        let bump = listed["entries"].as_array().unwrap().iter().find(|e| e["category"] == "update").unwrap();
        assert_eq!(bump["source_artifact"], "cd3f1a1", "not a path: left alone");

        let chat = rusqlite::Connection::open(b.home.join(".claude/knowledge/chat.db")).unwrap();
        assert_eq!(
            claudebase::daemon::agent_registry::recall_conversation_nick(&chat, ID).unwrap().as_deref(),
            Some("tt-backend")
        );

        // Importing the same archive again writes nothing new.
        let out = b.run(&["journal", "import", "carried.zip"]).success().get_output().stdout.clone();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("0 entries written, 2 already there"), "{text}");
    })
    .await
    .unwrap();
}

/// Without a daemon the conversation still moves, and the operator is told
/// how to bring the journal in afterwards.
#[test]
fn without_a_daemon_the_conversation_loads_and_the_journal_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let b = Machine::new(tmp.path(), "b");
    let archive = b.project.join("dump.zip");
    let manifest = serde_json::to_vec(&serde_json::json!({
        "format":"claudebase-chat-dump","version":2,"session_id":ID,"title":null,
        "source_cwd":"/x","created_at":"2026-01-01T00:00:00Z","claudebase_version":"0",
        "journal_entries":1
    }))
    .unwrap();
    let line = format!("{{\"type\":\"user\",\"sessionId\":\"{ID}\"}}\n");
    let journal = br#"{"body":"x","category":"decision","tags":["t"],"created_at":5}
"#;
    hostile_zip(&archive, &[("manifest.json", &manifest), ("transcript.jsonl", line.as_bytes()), ("journal.jsonl", journal)]);
    let out = b.run(&["chat", "load", "dump.zip"]).success().get_output().stdout.clone();
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("the journal was NOT imported") && text.contains("claudebase journal import"), "{text}");
    assert!(b.folder().join(format!("{ID}.jsonl")).is_file());
}

async fn ok(c: &mut common::Client, tool: &str, args: Value) -> Value {
    let r = c.call(tool, args, 9).await.unwrap();
    common::payload(&r).unwrap_or_else(|| panic!("{tool} failed: {r}"))
}

/// journal#120: the links between entries and the project's mind maps travel
/// too — renumbered when the target journal already has entries of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn links_and_mind_maps_travel_with_the_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Machine::new(tmp.path(), "a");
    let b = Machine::with_project(tmp.path(), "b", "elsewhere");
    seed(&a);

    let _da = daemon_for(&a).await;
    let a_dir = a.project.to_str().unwrap().to_owned();
    let mut client = common::Client::connect(&common::socket_under(&a.home)).await.unwrap();
    let reg = client
        .call(
            "agent_register",
            serde_json::json!({"agent_id": "mira-1", "name": "mira", "session_token": "tok", "cwd": a_dir, "host": "host-a", "pid": std::process::id()}),
            1,
        )
        .await
        .unwrap();
    assert!(reg.get("error").is_none(), "{reg}");
    let s = |extra: Value| {
        let mut v = serde_json::json!({"agent_id": "mira-1", "session_token": "tok"});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    let entries = serde_json::json!([
        {"body": "the pool queue grows under load", "category": "finding", "tags": ["pool"], "created_at": 1_700_000_000},
        {"body": "measured: queue flat after the shard", "category": "verification", "tags": ["pool"], "created_at": 1_700_000_100},
        {"body": "shard the writer task", "category": "decision", "tags": ["pool"], "created_at": 1_700_000_200},
    ]);
    let r = ok(&mut client, "journal_import", serde_json::json!({"cwd": a_dir, "entries": entries})).await;
    let ids: Vec<i64> = r["ids"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
    let (finding, proof, decision) = (ids[0], ids[1], ids[2]);
    ok(&mut client, "journal_import_links", serde_json::json!({"cwd": a_dir, "links": [[proof, finding, "verifies"], [decision, finding, "cites"]]})).await;
    // A shared mind map and a private flowchart, pointing at the entries.
    let m = ok(&mut client, "journal_mindmap_new", s(serde_json::json!({"title": "pool", "shared": true}))).await["map"].as_i64().unwrap();
    let n1 = ok(&mut client, "journal_mindmap_add", s(serde_json::json!({"map": m, "text": "queue grows", "kind": "fact", "refs": [finding]}))).await["node"].as_i64().unwrap();
    let n2 = ok(&mut client, "journal_mindmap_add", s(serde_json::json!({"map": m, "text": "shard it", "kind": "decision", "refs": [decision]}))).await["node"].as_i64().unwrap();
    ok(&mut client, "journal_mindmap_link", s(serde_json::json!({"map": m, "from": n2, "to": n1, "label": "fixes", "refs": [proof]}))).await;
    ok(&mut client, "journal_mindmap_chain", s(serde_json::json!({"map": m, "name": "story", "entries": [finding, decision, proof]}))).await;
    let f = ok(&mut client, "journal_mindmap_new", s(serde_json::json!({"title": "request path", "layout": "flowchart", "direction": "right"}))).await["map"].as_i64().unwrap();
    let start = ok(&mut client, "journal_mindmap_add", s(serde_json::json!({"map": f, "text": "request", "kind": "start"}))).await["node"].as_i64().unwrap();
    ok(&mut client, "journal_mindmap_add", s(serde_json::json!({"map": f, "text": "queue", "kind": "data", "after": start, "label": "enqueue", "refs": [finding]}))).await;
    let chat = rusqlite::Connection::open(a.home.join(".claude/knowledge/chat.db")).unwrap();
    claudebase::daemon::agent_registry::remember_conversation_nick(&chat, ID, "mira", 1).unwrap();
    drop(chat);

    let (a2, b2) = (a, b);
    let (_a, b, archive) = tokio::task::spawn_blocking(move || {
        let out = a2.run(&["chat", "dump", ID]).success().get_output().stdout.clone();
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.contains("journal: 3 entries") && text.contains("mind maps: 2"), "{text}");
        let archive = archive_path(&out);
        (a2, b2, archive)
    })
    .await
    .unwrap();

    // b's journal already has an entry: everything from a is renumbered.
    let _db = daemon_for(&b).await;
    let b_dir = b.project.to_str().unwrap().to_owned();
    let mut bc = common::Client::connect(&common::socket_under(&b.home)).await.unwrap();
    ok(&mut bc, "journal_import", serde_json::json!({"cwd": b_dir, "entries": [{"body": "b was here first", "category": "finding", "tags": ["b"], "created_at": 1_600_000_000}]})).await;
    let carried = b.project.join("carried.zip");
    fs::copy(&archive, &carried).unwrap();
    let b = tokio::task::spawn_blocking(move || {
        let out = b.run(&["chat", "load", "carried.zip"]).success().get_output().stdout.clone();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("3 entries written") && text.contains("2 links restored") && text.contains("2 mind maps imported"), "{text}");
        b
    })
    .await
    .unwrap();

    let listed = ok(&mut bc, "journal_list", serde_json::json!({"cwd": b_dir})).await;
    let by_body = |body: &str| listed["entries"].as_array().unwrap().iter().find(|e| e["body"] == body).unwrap().clone();
    let (nf, np, nd) = (by_body("the pool queue grows under load"), by_body("measured: queue flat after the shard"), by_body("shard the writer task"));
    let (nf_id, np_id, nd_id) = (nf["id"].as_i64().unwrap(), np["id"].as_i64().unwrap(), nd["id"].as_i64().unwrap());
    assert_ne!(nf_id, finding, "renumbered behind b's own entry");
    let links_of = |e: &Value| e["links"].as_array().cloned().unwrap_or_default();
    assert!(links_of(&np).iter().any(|l| l["kind"] == "verifies" && l["dir"] == "out" && l["id"] == nf_id), "{np}");
    assert!(links_of(&nd).iter().any(|l| l["kind"] == "cites" && l["dir"] == "out" && l["id"] == nf_id), "{nd}");
    assert_eq!(links_of(&nf).len(), 2, "both shown on the cited entry: {nf}");

    let maps = ok(&mut bc, "journal_mindmap_list", serde_json::json!({"cwd": b_dir, "closed": true})).await;
    let maps = maps["maps"].as_array().unwrap().clone();
    assert_eq!(maps.len(), 2, "{maps:?}");
    let pool = maps.iter().find(|m| m["title"] == "pool").unwrap();
    assert_eq!((pool["owner_nick"].as_str(), pool["shared"].as_bool()), (Some("mira"), Some(true)));
    assert_eq!(pool["owner_host"].as_str(), Some(claudebase::daemon::agent_registry::this_host().as_str()), "it lives here now");
    let shown = ok(&mut bc, "journal_mindmap_show", serde_json::json!({"cwd": b_dir, "map": pool["id"]})).await;
    let pm = &shown["map"];
    let refs: Vec<i64> = pm["nodes"].as_array().unwrap().iter().flat_map(|n| n["refs"].as_array().unwrap().iter().map(|r| r.as_i64().unwrap())).collect();
    assert!(refs.contains(&nf_id) && refs.contains(&nd_id), "{refs:?}");
    assert_eq!(pm["edges"][0]["label"], "fixes");
    assert_eq!(pm["edges"][0]["refs"], serde_json::json!([np_id]));
    assert_eq!(pm["chains"][0]["entries"], serde_json::json!([nf_id, nd_id, np_id]));
    let flow = maps.iter().find(|m| m["title"] == "request path").unwrap();
    assert_eq!((flow["layout"].as_str(), flow["direction"].as_str()), (Some("flowchart"), Some("right")));
    let fm = ok(&mut bc, "journal_mindmap_show", serde_json::json!({"cwd": b_dir, "map": flow["id"]})).await;
    assert_eq!(fm["map"]["edges"][0]["label"], "enqueue");

    // Loading again adds nothing.
    tokio::task::spawn_blocking(move || {
        let out = b.run(&["journal", "import", "carried.zip"]).success().get_output().stdout.clone();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("0 entries written, 3 already there") && text.contains("0 links restored, 2 already there") && text.contains("0 mind maps imported, 2 already there"), "{text}");
    })
    .await
    .unwrap();
}
