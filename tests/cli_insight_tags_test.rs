//! Slice 4 (insights-hybrid-corpus) — `insight tags` subcommand tests.
//!
//! Coverage (QA TC-IHC-5.x / 6.x):
//! - TC-IHC-5.1 — merged default: project + general tags both appear, sorted
//!   by count descending, `{tag,count}` JSON shape, exit 0
//! - TC-IHC-5.2 — merged count for a tag present in both dbs is the SUM
//!   (`tokio` count==3 across local×2 + global×1)
//! - TC-IHC-5.3 — AC-IHC-8 shape: array len>=1, elem0 has tag (string) +
//!   count (integer)
//! - TC-IHC-5.4 — no `--json`: human-readable `<tag>  <count>` table, exit 0
//! - TC-IHC-5.5 — global db absent → local-only tags, no error
//! - TC-IHC-5.6 — local db absent → global-only tags, exit 0
//! - TC-IHC-5.7 — both dbs empty/absent → `[]`, exit 0
//! - TC-IHC-6.1 — `--category general` → only global-db tags
//! - TC-IHC-6.2 — `--project <slug>` registry lookup → that project's tags
//! - TC-IHC-6.3 — `--project nonexistent` (not in registry) → exit 1 +
//!   `not found in registry`
//!
//! HERMETICITY: every command points `$HOME` (and `USERPROFILE`) at a
//! per-test tempdir via `cmd.env(...)`, so the operator's real
//! `~/.claude/knowledge/insights.db` and `~/.claude/knowledge/projects.json`
//! are never touched.

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn bin() -> Command {
    Command::cargo_bin("claudebase").expect("binary built")
}

/// A hermetic sandbox: a project dir (cwd) and a separate HOME dir, kept
/// alive together so neither tempdir is dropped mid-test.
struct Sandbox {
    project: tempfile::TempDir,
    home: tempfile::TempDir,
}

fn sandbox() -> Sandbox {
    let project = tempfile::tempdir().expect("project tempdir");
    fs::create_dir_all(project.path().join(".claude/knowledge")).expect("mkdir project knowledge");
    let home = tempfile::tempdir().expect("home tempdir");
    fs::create_dir_all(home.path().join(".claude/knowledge")).expect("mkdir home knowledge");
    Sandbox { project, home }
}

impl Sandbox {
    /// A `claudebase` command rooted in the project dir with `$HOME` /
    /// `USERPROFILE` pinned to the sandbox home so the global insights db +
    /// registry resolve under the tempdir, not the operator's real home.
    fn cmd(&self) -> Command {
        let mut c = bin();
        c.current_dir(self.project.path())
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path());
        c
    }

    fn global_db(&self) -> PathBuf {
        self.home.path().join(".claude/knowledge/insights.db")
    }

    /// Seed one project (cwd-local) insight with the given tags.
    fn seed_project(&self, body: &str, tags: &[&str]) {
        let mut args: Vec<String> = vec![
            "insight".into(),
            "create".into(),
            body.into(),
            "--type".into(),
            "agent-learned".into(),
            "--agent".into(),
            "tester".into(),
            "--category".into(),
            "project".into(),
        ];
        for t in tags {
            args.push("--tags".into());
            args.push((*t).into());
        }
        self.cmd().args(&args).assert().success();
    }

    /// Seed one general (global) insight with the given tags.
    fn seed_general(&self, body: &str, tags: &[&str]) {
        let mut args: Vec<String> = vec![
            "insight".into(),
            "create".into(),
            body.into(),
            "--type".into(),
            "agent-learned".into(),
            "--agent".into(),
            "tester".into(),
            "--category".into(),
            "general".into(),
        ];
        for t in tags {
            args.push("--tags".into());
            args.push((*t).into());
        }
        self.cmd().args(&args).assert().success();
    }
}

fn parse_json_array(bytes: &[u8]) -> Vec<Value> {
    let s = String::from_utf8_lossy(bytes);
    let v: Value = serde_json::from_str(s.trim())
        .unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}\nstdout was:\n{s}"));
    v.as_array().cloned().unwrap_or_else(|| panic!("JSON is not an array; got:\n{s}"))
}

fn count_for(arr: &[Value], tag: &str) -> Option<i64> {
    arr.iter()
        .find(|o| o.get("tag").and_then(|t| t.as_str()) == Some(tag))
        .and_then(|o| o.get("count").and_then(|c| c.as_i64()))
}

// ---------------------------------------------------------------------------
// TC-IHC-5.1 — merged default: project + general tags both appear.
// ---------------------------------------------------------------------------

#[test]
fn tags_merged_default_includes_both_dbs_sorted_desc() {
    let sb = sandbox();
    sb.seed_project("project insight about tokio", &["tokio"]);
    sb.seed_general("general insight about nginx", &["nginx"]);

    let out = sb
        .cmd()
        .args(["insight", "tags", "--json"])
        .assert()
        .success();
    let arr = parse_json_array(&out.get_output().stdout);

    assert_eq!(count_for(&arr, "tokio"), Some(1), "tokio from project db; arr={arr:?}");
    assert_eq!(count_for(&arr, "nginx"), Some(1), "nginx from global db; arr={arr:?}");

    // sorted by count descending
    let counts: Vec<i64> = arr
        .iter()
        .map(|o| o.get("count").and_then(|c| c.as_i64()).unwrap())
        .collect();
    let mut sorted = counts.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(counts, sorted, "counts must be descending; arr={arr:?}");
}

// ---------------------------------------------------------------------------
// TC-IHC-5.2 — merged count for a tag in BOTH dbs is the SUM.
// ---------------------------------------------------------------------------

#[test]
fn tags_merged_count_is_summed_across_dbs() {
    let sb = sandbox();
    // Three insights all tagged `tokio`. The bodies are deliberately
    // SEMANTICALLY DISTINCT (different subject matter) so the cosine>0.92
    // semantic-dedup gate in `insight create` does NOT collapse them — we need
    // all three rows to land so the merged count can be 3.
    sb.seed_project("the borrow checker rejected a mutable alias in slice four", &["tokio"]);
    sb.seed_project("postgres connection pooling exhausted under load", &["tokio"]);
    // one general insight also tagged tokio (separate db → never dedup'd vs local)
    sb.seed_general("kubernetes ingress misrouted traffic to the wrong pod", &["tokio"]);

    let out = sb
        .cmd()
        .args(["insight", "tags", "--json"])
        .assert()
        .success();
    let arr = parse_json_array(&out.get_output().stdout);

    assert_eq!(count_for(&arr, "tokio"), Some(3), "summed local(2)+global(1); arr={arr:?}");
}

// ---------------------------------------------------------------------------
// TC-IHC-5.3 — AC-IHC-8 shape: len>=1, elem0 has tag(string)+count(integer).
// ---------------------------------------------------------------------------

#[test]
fn tags_json_shape_ac_ihc_8() {
    let sb = sandbox();
    sb.seed_project("project insight", &["slice4"]);
    sb.seed_general("general insight", &["nginx"]);

    let out = sb
        .cmd()
        .args(["insight", "tags", "--json"])
        .assert()
        .success();
    let arr = parse_json_array(&out.get_output().stdout);

    assert!(!arr.is_empty(), "array length >= 1; arr={arr:?}");
    let tag = arr[0].get("tag").and_then(|t| t.as_str());
    let count = arr[0].get("count").and_then(|c| c.as_i64());
    assert!(tag.is_some() && !tag.unwrap().is_empty(), "elem0.tag non-empty string; arr={arr:?}");
    assert!(count.is_some(), "elem0.count integer; arr={arr:?}");
}

// ---------------------------------------------------------------------------
// TC-IHC-5.4 — no --json: human-readable `<tag>  <count>` table, exit 0.
// ---------------------------------------------------------------------------

#[test]
fn tags_human_table_when_no_json() {
    let sb = sandbox();
    sb.seed_project("project insight", &["tokio"]);

    let out = sb.cmd().args(["insight", "tags"]).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();

    assert!(!stdout.trim().is_empty(), "human output is non-empty");
    // human output is NOT a JSON array (sanity)
    assert!(
        serde_json::from_str::<Value>(stdout.trim())
            .ok()
            .and_then(|v| v.as_array().map(|_| ()))
            .is_none(),
        "human output should NOT parse as a JSON array; got:\n{stdout}"
    );
    // at least one line has a tag-like token, whitespace, then a digit
    let has_table_line = stdout.lines().any(|l| {
        let t = l.trim();
        t.contains("tokio") && t.chars().any(|c| c.is_ascii_digit())
    });
    assert!(has_table_line, "expected a `tokio  <count>` line; got:\n{stdout}");
}

// ---------------------------------------------------------------------------
// TC-IHC-5.5 — global db absent → local-only tags, no error.
// ---------------------------------------------------------------------------

#[test]
fn tags_global_absent_returns_local_only_no_error() {
    let sb = sandbox();
    sb.seed_project("project insight", &["tokio"]);
    // ensure global db does not exist
    let _ = fs::remove_file(sb.global_db());
    assert!(!sb.global_db().exists(), "global db must be absent for this test");

    let out = sb
        .cmd()
        .args(["insight", "tags", "--json"])
        .assert()
        .success();
    let arr = parse_json_array(&out.get_output().stdout);
    assert!(!arr.is_empty(), "local-only tags returned; arr={arr:?}");
    assert_eq!(count_for(&arr, "tokio"), Some(1));
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(!stderr.contains("error:"), "no error about missing global db; stderr:\n{stderr}");
}

// ---------------------------------------------------------------------------
// TC-IHC-5.6 — local db absent → global-only tags, exit 0.
// ---------------------------------------------------------------------------

#[test]
fn tags_local_absent_returns_global_only() {
    let sb = sandbox();
    sb.seed_general("general insight", &["nginx"]);
    // local db must not exist (we never seeded a project insight)
    assert!(
        !sb.project.path().join(".claude/knowledge/insights.db").exists(),
        "local db must be absent for this test"
    );

    let out = sb
        .cmd()
        .args(["insight", "tags", "--json"])
        .assert()
        .success();
    let arr = parse_json_array(&out.get_output().stdout);
    assert!(!arr.is_empty(), "global-only tags returned; arr={arr:?}");
    assert_eq!(count_for(&arr, "nginx"), Some(1));
}

// ---------------------------------------------------------------------------
// TC-IHC-5.7 — both dbs empty/absent → `[]`, exit 0.
// ---------------------------------------------------------------------------

#[test]
fn tags_both_empty_returns_empty_array() {
    let sb = sandbox();
    // no seeding at all
    let out = sb
        .cmd()
        .args(["insight", "tags", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert_eq!(stdout.trim(), "[]", "both-empty must emit exactly []; got:\n{stdout}");
}

// ---------------------------------------------------------------------------
// TC-IHC-6.1 — --category general → only global-db tags.
// ---------------------------------------------------------------------------

#[test]
fn tags_category_general_only_global() {
    let sb = sandbox();
    sb.seed_project("project insight", &["tokio"]);
    sb.seed_general("general insight", &["nginx"]);

    let out = sb
        .cmd()
        .args(["insight", "tags", "--category", "general", "--json"])
        .assert()
        .success();
    let arr = parse_json_array(&out.get_output().stdout);

    assert_eq!(count_for(&arr, "nginx"), Some(1), "nginx present; arr={arr:?}");
    assert_eq!(count_for(&arr, "tokio"), None, "tokio (project) must be absent; arr={arr:?}");
}

// ---------------------------------------------------------------------------
// TC-IHC-6.1b — --category project → only local-db tags (symmetry check).
// ---------------------------------------------------------------------------

#[test]
fn tags_category_project_only_local() {
    let sb = sandbox();
    sb.seed_project("project insight", &["tokio"]);
    sb.seed_general("general insight", &["nginx"]);

    let out = sb
        .cmd()
        .args(["insight", "tags", "--category", "project", "--json"])
        .assert()
        .success();
    let arr = parse_json_array(&out.get_output().stdout);

    assert_eq!(count_for(&arr, "tokio"), Some(1), "tokio present; arr={arr:?}");
    assert_eq!(count_for(&arr, "nginx"), None, "nginx (general) must be absent; arr={arr:?}");
}

// ---------------------------------------------------------------------------
// TC-IHC-6.2 — --project <slug> registry lookup → that project's tags.
// ---------------------------------------------------------------------------

#[test]
fn tags_project_registry_lookup() {
    // Build a SECOND project dir with its own insight, register it in
    // projects.json under HOME, then query `--project <slug>` from an
    // unrelated cwd. The named project's local tags must surface.
    let home = tempfile::tempdir().expect("home tempdir");
    fs::create_dir_all(home.path().join(".claude/knowledge")).expect("mkdir home knowledge");

    let named = tempfile::tempdir().expect("named project tempdir");
    fs::create_dir_all(named.path().join(".claude/knowledge")).expect("mkdir named knowledge");

    // seed the named project's local db with tag `slice3`
    bin()
        .current_dir(named.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .args([
            "insight", "create", "named project insight",
            "--type", "agent-learned", "--agent", "tester",
            "--category", "project", "--tags", "slice3",
        ])
        .assert()
        .success();

    // write the registry mapping slug -> named project path
    let canonical = fs::canonicalize(named.path()).expect("canonicalize named");
    let registry = serde_json::json!([
        { "name": "namedproj", "path": canonical.to_str().unwrap(), "last_seen": 1748376015u64 }
    ]);
    fs::write(
        home.path().join(".claude/knowledge/projects.json"),
        serde_json::to_string_pretty(&registry).unwrap(),
    )
    .expect("write registry");

    // query from an unrelated cwd
    let other = tempfile::tempdir().expect("other cwd tempdir");
    let out = bin()
        .current_dir(other.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .args(["insight", "tags", "--project", "namedproj", "--json"])
        .assert()
        .success();
    let arr = parse_json_array(&out.get_output().stdout);
    assert_eq!(count_for(&arr, "slice3"), Some(1), "named project tag surfaced; arr={arr:?}");
}

// ---------------------------------------------------------------------------
// TC-IHC-6.3 — --project nonexistent (not in registry) → exit 1.
// ---------------------------------------------------------------------------

#[test]
fn tags_project_not_in_registry_exit_1() {
    let sb = sandbox();
    // no registry file at all
    let assert = sb
        .cmd()
        .args(["insight", "tags", "--project", "nonexistentproject", "--json"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("not found in registry"),
        "stderr must say not found in registry; got:\n{stderr}"
    );
    assert!(
        stderr.contains("nonexistentproject"),
        "stderr should name the slug; got:\n{stderr}"
    );
}
