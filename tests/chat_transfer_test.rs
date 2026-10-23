//! `claudebase chat dump` / `chat load` — a conversation carried from one
//! machine to another. Each "machine" is its own CLAUDE_CONFIG_DIR and its own
//! project directory, at different paths.

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const ID: &str = "33333333-3333-4333-8333-333333333333";

struct Machine {
    claude: PathBuf,
    project: PathBuf,
}

impl Machine {
    fn new(root: &Path, name: &str) -> Machine {
        let claude = root.join(name).join("claude");
        let project = root.join(name).join("work").join("proj");
        fs::create_dir_all(&project).unwrap();
        Machine { claude, project: project.canonicalize().unwrap() }
    }

    fn folder(&self) -> PathBuf {
        self.claude.join("projects").join(claudebase::chat_clone::project_slug(&self.project))
    }

    fn run(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        Command::cargo_bin("claudebase")
            .unwrap()
            .current_dir(&self.project)
            .env("CLAUDE_CONFIG_DIR", &self.claude)
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
