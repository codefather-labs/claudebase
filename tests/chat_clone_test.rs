//! `claudebase chat clone` — copy a Claude Code conversation under a new id
//! and name, addressed either by id or by the name `/resume` shows.

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const SRC: &str = "11111111-1111-4111-8111-111111111111";
const OTHER: &str = "22222222-2222-4222-8222-222222222222";

struct Env {
    _tmp: tempfile::TempDir,
    claude: PathBuf,
    cwd: PathBuf,
    folder: PathBuf,
}

fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let claude = tmp.path().join("claude");
    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.canonicalize().unwrap();
    let folder = claude
        .join("projects")
        .join(claudebase::chat_clone::project_slug(&cwd));
    fs::create_dir_all(&folder).unwrap();

    let transcript = format!(
        "{{\"type\":\"user\",\"uuid\":\"m1\",\"parentUuid\":null,\"sessionId\":\"{SRC}\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n\
         {{\"type\":\"ai-title\",\"aiTitle\":\"auto title\",\"sessionId\":\"{SRC}\"}}\n\
         {{\"type\":\"custom-title\",\"customTitle\":\"original\",\"sessionId\":\"{SRC}\"}}\n\
         {{\"type\":\"agent-name\",\"agentName\":\"original\",\"sessionId\":\"{SRC}\"}}\n\
         {{\"type\":\"bridge-session\",\"bridgeSessionId\":\"cse_x\",\"sessionId\":\"{SRC}\"}}\n\
         {{\"type\":\"assistant\",\"uuid\":\"m2\",\"parentUuid\":\"m1\",\"sessionId\":\"{SRC}\",\"message\":{{\"role\":\"assistant\",\"content\":\"hi\"}}}}\n"
    );
    fs::write(folder.join(format!("{SRC}.jsonl")), transcript).unwrap();

    let sub = folder.join(SRC).join("subagents");
    fs::create_dir_all(&sub).unwrap();
    fs::write(
        sub.join("agent-a1.jsonl"),
        format!("{{\"type\":\"user\",\"isSidechain\":true,\"sessionId\":\"{SRC}\"}}\n"),
    )
    .unwrap();
    fs::write(sub.join("agent-a1.meta.json"), "{\"agentType\":\"x\"}").unwrap();
    let fh = claude.join("file-history").join(SRC);
    fs::create_dir_all(&fh).unwrap();
    fs::write(fh.join("abc@v1"), "backup").unwrap();

    Env { _tmp: tmp, claude, cwd, folder }
}

fn clone(env: &Env, source: &str, name: &str) -> assert_cmd::assert::Assert {
    Command::cargo_bin("claudebase")
        .unwrap()
        .current_dir(&env.cwd)
        .env("CLAUDE_CONFIG_DIR", &env.claude)
        .args(["chat", "clone", source, name])
        .assert()
}

fn new_id(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .find_map(|l| l.strip_prefix("new id: "))
        .expect("new id line")
        .trim()
        .to_owned()
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn clone_by_id_copies_context_under_new_id_and_name() {
    let env = setup();
    let out = clone(&env, SRC, "my copy").success().get_output().stdout.clone();
    let id = new_id(&out);
    assert_ne!(id, SRC);

    let copy = lines(&env.folder.join(format!("{id}.jsonl")));
    // Every remaining line now belongs to the clone.
    assert!(copy.iter().all(|v| v["sessionId"] == id.as_str()));
    // Messages and their chain are preserved.
    assert_eq!(copy[0]["uuid"], "m1");
    assert!(copy.iter().any(|v| v["uuid"] == "m2" && v["parentUuid"] == "m1"));
    // The source's name and Remote Control binding are not carried over.
    assert!(!copy.iter().any(|v| v["type"] == "bridge-session"));
    let titles: Vec<&Value> = copy.iter().filter(|v| v["type"] == "custom-title").collect();
    assert_eq!(titles.len(), 1);
    assert_eq!(titles[0]["customTitle"], "my copy");
    let names: Vec<&Value> = copy.iter().filter(|v| v["type"] == "agent-name").collect();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0]["agentName"], "my copy");

    // Side folders come along, subagent transcripts rewritten.
    let sub = lines(&env.folder.join(&id).join("subagents").join("agent-a1.jsonl"));
    assert_eq!(sub[0]["sessionId"], id.as_str());
    assert!(env.folder.join(&id).join("subagents/agent-a1.meta.json").is_file());
    assert_eq!(
        fs::read_to_string(env.claude.join("file-history").join(&id).join("abc@v1")).unwrap(),
        "backup"
    );

    // The original is untouched.
    let orig = lines(&env.folder.join(format!("{SRC}.jsonl")));
    assert_eq!(orig.len(), 6);
    assert!(orig.iter().all(|v| v["sessionId"] == SRC));
}

#[test]
fn clone_by_resume_name() {
    let env = setup();
    let out = clone(&env, "original", "second").success().get_output().stdout.clone();
    let id = new_id(&out);
    assert!(String::from_utf8_lossy(&out).contains(SRC));

    // The clone is now findable by its own name, and the source by its own.
    let out2 = clone(&env, "second", "third").success().get_output().stdout.clone();
    assert!(String::from_utf8_lossy(&out2).contains(&id));
}

#[test]
fn ai_title_is_used_when_there_is_no_custom_title() {
    let env = setup();
    fs::write(
        env.folder.join(format!("{OTHER}.jsonl")),
        format!("{{\"type\":\"ai-title\",\"aiTitle\":\"only auto\",\"sessionId\":\"{OTHER}\"}}\n"),
    )
    .unwrap();
    let out = clone(&env, "only auto", "x").success().get_output().stdout.clone();
    assert!(String::from_utf8_lossy(&out).contains(OTHER));
}

#[test]
fn ambiguous_name_is_refused() {
    let env = setup();
    fs::write(
        env.folder.join(format!("{OTHER}.jsonl")),
        format!("{{\"type\":\"custom-title\",\"customTitle\":\"original\",\"sessionId\":\"{OTHER}\"}}\n"),
    )
    .unwrap();
    clone(&env, "original", "x")
        .failure()
        .stderr(predicates::str::contains("2 conversations are named `original`"));
}

#[test]
fn unknown_source_and_empty_name_are_refused() {
    let env = setup();
    clone(&env, "no such thing", "x")
        .failure()
        .stderr(predicates::str::contains("no conversation named"));
    clone(&env, OTHER, "x")
        .failure()
        .stderr(predicates::str::contains("no conversation"));
    clone(&env, SRC, "   ").failure().stderr(predicates::str::contains("non-empty name"));
}
