//! The public API on a real daemon: a token, no session, one journal shared with sessions by project key.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_claudebase");
const PROJECT: &str = "github.com/acme/app";

struct Daemon {
    child: Child,
    port: u16,
    token: String,
    _home: TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn cli(home: &Path, args: &[&str]) -> String {
    let out = Command::new(BIN).args(args).env("HOME", home).env("XDG_RUNTIME_DIR", home.join("run")).output().unwrap();
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn request(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth = token.map(|t| format!("X-Api-Token: {t}\r\n")).unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).unwrap();
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let json = text.split_once("\r\n\r\n").map(|(_, b)| serde_json::from_str(b).unwrap_or(Value::Null)).unwrap();
    (status, json)
}

fn start() -> Daemon {
    let home = TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join("run")).unwrap();
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    cli(home.path(), &["daemon", "callback", "enable", "--bind", &format!("127.0.0.1:{port}")]);
    let issued = cli(home.path(), &["daemon", "api", "issue", "orchestrator"]);
    let token = issued.split_whitespace().find(|w| w.starts_with("cba_")).expect("a token").to_owned();
    let child = Command::new(BIN).args(["daemon", "serve"]).env("HOME", home.path()).env("XDG_RUNTIME_DIR", home.path().join("run")).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "the listener did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    Daemon { child, port, token, _home: home }
}

impl Daemon {
    fn post(&self, path: &str, body: Value) -> (u16, Value) {
        request(self.port, "POST", path, Some(&self.token), &body.to_string())
    }
    fn body(&self, extra: Value) -> Value {
        let mut b = json!({ "project_id": PROJECT, "author": "backend-developer" });
        b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        b
    }
}

#[test]
fn the_journal_and_maps_work_with_a_token_and_no_session() {
    let d = start();
    assert_eq!(request(d.port, "GET", "/v1/version", None, "").0, 401);
    assert_eq!(request(d.port, "GET", "/v1/version", Some("cba_wrong"), "").0, 401);
    let (status, version) = request(d.port, "GET", "/v1/version", Some(&d.token), "");
    assert_eq!((status, version["api"].clone()), (200, json!(1)));

    // A todo with a linked invariant, as an orchestrator writes them.
    let (status, todo) = d.post("/v1/journal/add", d.body(json!({ "body": "Add a server", "flags": ["todo"], "tags": ["t"] })));
    assert_eq!(status, 200, "{todo}");
    let (status, rule) = d.post("/v1/journal/add", d.body(json!({ "body": "INV-1: totals are never negative", "flags": ["invariant"], "tags": ["t"] })));
    assert_eq!(status, 200, "{rule}");
    let (todo_id, rule_id) = (todo["id"].as_i64().unwrap(), rule["id"].as_i64().unwrap());
    let (status, linked) = d.post("/v1/journal/link", d.body(json!({ "id": todo_id, "to": rule_id })));
    assert_eq!(status, 200, "{linked}");

    let (_, got) = d.post("/v1/journal/get", d.body(json!({ "id": rule_id })));
    assert_eq!(got["entry"]["agent"].as_str().or(got["agent"].as_str()), Some("backend-developer"), "{got}");
    let (_, agenda) = d.post("/v1/journal/agenda", d.body(json!({})));
    assert!(agenda.to_string().contains("INV-1"), "an invariant is on the agenda: {agenda}");

    // The closing rule holds here as for a session.
    let (status, refused) = d.post("/v1/journal/close", d.body(json!({ "id": todo_id })));
    assert_eq!(status, 400, "{refused}");
    assert!(refused["message"].as_str().unwrap().contains("verified"), "{refused}");

    // A map, with a reference on a node, private to its author.
    let (status, map) = d.post("/v1/mindmap/new", d.body(json!({ "title": "Architecture" })));
    assert_eq!(status, 200, "{map}");
    let id = map["map"].clone();
    let (status, node) = d.post("/v1/mindmap/add", d.body(json!({ "map": id, "text": "API", "refs": [rule_id] })));
    assert_eq!(status, 200, "{node}");
    let foreign = json!({ "project_id": PROJECT, "author": "tester", "map": id, "text": "x" });
    assert_eq!(d.post("/v1/mindmap/add", foreign).0, 400, "a private map takes writes from its owner only");

    let (status, projects) = request(d.port, "GET", "/v1/projects", Some(&d.token), "");
    assert_eq!(status, 200);
    assert!(projects.to_string().contains(PROJECT), "{projects}");
}

#[test]
fn projects_are_apart_and_bad_requests_are_refused() {
    let d = start();
    d.post("/v1/journal/add", d.body(json!({ "body": "only in app", "tags": ["t"], "category": "finding" })));
    let other = json!({ "project_id": "github.com/acme/other", "author": "a" });
    let (_, list) = d.post("/v1/journal/list", other);
    assert!(!list.to_string().contains("only in app"), "{list}");

    assert_eq!(d.post("/v1/journal/list", json!({ "author": "a" })).0, 400);
    assert_eq!(d.post("/v1/journal/list", json!({ "project_id": PROJECT })).0, 400);
    assert_eq!(d.post("/v1/journal/list", json!({ "project_id": "local:abc", "author": "a" })).0, 400);
    assert_eq!(request(d.port, "POST", "/v1/journal/list", Some(&d.token), "not json").0, 400);
    for denied in ["/v1/journal/delete", "/v1/journal/share", "/v1/journal/gc", "/v1/mindmap/send", "/v1/nothing"] {
        assert_eq!(d.post(denied, d.body(json!({}))).0, 404, "{denied}");
    }
}

#[test]
fn a_client_field_cannot_impersonate_or_reach_other_journals() {
    let d = start();
    // Sent straight to a tool, the internal field must not count: there is no session here.
    let (_, wrote) = d.post("/v1/journal/add", d.body(json!({ "body": "x", "tags": ["t"], "category": "finding", "agent": "someone-else", "cwd": "/etc" })));
    let (_, got) = d.post("/v1/journal/get", d.body(json!({ "id": wrote["id"] })));
    assert!(!got.to_string().contains("someone-else"), "{got}");
}

/// The policy is the operator's: a token reads it and cannot change it, as a session cannot.
#[test]
fn a_token_cannot_change_the_policy() {
    let d = start();
    d.post("/v1/journal/add", d.body(json!({ "body": "x", "tags": ["t"], "category": "finding" })));
    let (status, read) = d.post("/v1/journal/policy", d.body(json!({})));
    assert_eq!(status, 200, "{read}");
    let held = read["evidence_first"].as_bool().expect("the policy is told");
    let (status, refused) = d.post("/v1/journal/policy", d.body(json!({ "evidence_first": !held })));
    assert_eq!((status, refused["error"].as_str()), (400, Some("refused")), "{refused}");
    assert_eq!(d.post("/v1/journal/policy", d.body(json!({}))).1["evidence_first"], json!(held));
}

fn journals(home: &Path) -> usize {
    let dir = home.join(".claude/claudebase/journals");
    std::fs::read_dir(dir).map_or(0, |files| files.flatten().filter(|f| f.path().extension().is_some_and(|e| e == "db")).count())
}

/// A misspelt project is told, and leaves nothing behind; only a write starts a journal.
#[test]
fn reading_an_unknown_project_is_404_and_creates_nothing() {
    let d = start();
    let before = journals(d._home.path());
    for read in ["/v1/journal/list", "/v1/journal/agenda", "/v1/journal/search", "/v1/journal/policy", "/v1/mindmap/list"] {
        let (status, answer) = d.post(read, json!({ "project_id": "github.com/acme/typo", "author": "a", "query": "x" }));
        assert_eq!((status, answer["error"].as_str()), (404, Some("unknown_project")), "{read}: {answer}");
    }
    assert_eq!(journals(d._home.path()), before);
    let (status, _) = d.post("/v1/journal/add", d.body(json!({ "body": "the first", "tags": ["t"], "category": "finding" })));
    assert_eq!(status, 200);
    assert_eq!(journals(d._home.path()), before + 1);
    let (status, _) = d.post("/v1/mindmap/new", json!({ "project_id": "github.com/acme/maps", "author": "a", "title": "t" }));
    assert_eq!(status, 200, "a map starts a journal as an entry does");
}

/// Any spelling of a repository reaches the one journal its sessions use; what is no repository is refused.
#[test]
fn a_project_id_is_normalised_as_a_session_s_is() {
    let d = start();
    d.post("/v1/journal/add", d.body(json!({ "body": "spelt one way", "tags": ["t"], "category": "finding" })));
    for spelling in ["GitHub.com/Acme/App", "https://github.com/acme/app.git", "git@github.com:acme/app.git"] {
        let (status, list) = d.post("/v1/journal/list", json!({ "project_id": spelling, "author": "a" }));
        assert_eq!(status, 200, "{spelling}: {list}");
        assert!(list.to_string().contains("spelt one way"), "{spelling}: {list}");
    }
    for bad in ["app", "acme/app", "github.com/../app"] {
        assert_eq!(d.post("/v1/journal/list", json!({ "project_id": bad, "author": "a" })).0, 400, "{bad}");
    }
}

/// A bad token is answered from the head: the body is not waited for.
#[test]
fn a_bad_token_is_refused_before_the_body() {
    let d = start();
    let mut s = TcpStream::connect(("127.0.0.1", d.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "POST /v1/journal/list HTTP/1.1\r\nHost: x\r\nX-Api-Token: wrong\r\nContent-Length: 1000\r\n\r\n").unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).expect("answered without the body");
    assert!(text.starts_with("HTTP/1.1 401"), "{text}");
}

/// `--invariant` binds as `--constraint` does: in the agenda, and past retention.
#[test]
fn an_invariant_is_on_the_agenda() {
    let d = start();
    d.post("/v1/journal/add", d.body(json!({ "body": "the count never goes below zero", "tags": ["t"], "category": "finding", "flags": ["invariant"], "salience": "low" })));
    let (status, agenda) = d.post("/v1/journal/agenda", d.body(json!({})));
    assert_eq!(status, 200);
    assert!(agenda.to_string().contains("the count never goes below zero") && agenda.to_string().contains("invariant"), "{agenda}");
}

/// Reading is not narrowed to the reader's own entries; `agent` filters only when asked, and a write cannot borrow a name.
#[test]
fn an_author_reads_what_others_wrote() {
    let d = start();
    d.post("/v1/journal/add", json!({ "project_id": PROJECT, "author": "writer", "agent": "someone-else", "body": "written by the writer", "tags": ["t"], "category": "finding" }));
    let reader = |extra: Value| { let mut b = json!({ "project_id": PROJECT, "author": "reader" }); b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone()); b };
    for (route, extra) in [("/v1/journal/list", json!({})), ("/v1/journal/search", json!({ "query": "writer" }))] {
        let (_, found) = d.post(route, reader(extra));
        assert!(found.to_string().contains("written by the writer"), "{route}: {found}");
    }
    assert!(d.post("/v1/journal/list", reader(json!({ "agent": "writer" }))).1.to_string().contains("written by the writer"));
    assert!(!d.post("/v1/journal/list", reader(json!({ "agent": "someone-else" }))).1.to_string().contains("written by the writer"));
}
