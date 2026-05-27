//! Slice 3 (insights-hybrid-corpus) — `insight create` mandatory
//! category/tags + dual-db routing tests.
//!
//! Coverage (QA TC-IHC-1.x / 2.x / 3.x / 4.x / 20.x):
//! - `--category` is clap-required → exit 2 with `--category` in stderr
//! - invalid / empty `--category` value → clap exit 2
//! - missing / all-empty `--tags` → business-logic exit 2 with exact stderr,
//!   fired BEFORE any db open (no write)
//! - `--category project` lands in the cwd-local db, NOT the global db
//! - `--category general` lands in the global db, NOT the cwd-local db
//! - tags normalized (`#` stripped, lowercased, deduped) into insight_tags
//! - `--project myproj` (project) → project_slug='myproj'
//! - `--project x --category general` → project_slug IS NULL (ignored)
//! - default project_slug = cwd-basename when `--project` absent
//! - exact-sha dedup still fires per-db (project + general)
//!
//! HERMETICITY: every test that may touch the global db points `$HOME`
//! (and `USERPROFILE`) at a per-test tempdir via `cmd.env(...)`, so the
//! operator's real `~/.claude/knowledge/insights.db` is never written.

use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};

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
    Sandbox { project, home }
}

impl Sandbox {
    /// A `claudebase` command rooted in the project dir with `$HOME` /
    /// `USERPROFILE` pinned to the sandbox home so the global insights db
    /// resolves under the tempdir, not the operator's real home.
    fn cmd(&self) -> Command {
        let mut c = bin();
        c.current_dir(self.project.path())
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path());
        c
    }

    fn local_db(&self) -> PathBuf {
        self.project.path().join(".claude/knowledge/insights.db")
    }

    fn global_db(&self) -> PathBuf {
        self.home.path().join(".claude/knowledge/insights.db")
    }
}

fn open(db: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db).expect("open db")
}

fn doc_count(db: &Path) -> i64 {
    if !db.exists() {
        return 0;
    }
    open(db)
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// TC-IHC-4.x — `--category` is mandatory at the clap layer.
// ---------------------------------------------------------------------------

#[test]
fn missing_category_clap_exit_2_mentions_category() {
    // TC-IHC-4.1
    let sb = sandbox();
    let assert = sb
        .cmd()
        .args([
            "insight", "create", "body",
            "--type", "agent-learned", "--agent", "x",
            "--tags", "foo",
        ])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("--category"),
        "clap error should name --category; got:\n{stderr}"
    );
}

#[test]
fn invalid_category_value_clap_exit_2() {
    // TC-IHC-4.2
    let sb = sandbox();
    let assert = sb
        .cmd()
        .args([
            "insight", "create", "body",
            "--type", "agent-learned", "--agent", "x",
            "--tags", "foo", "--category", "team",
        ])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(stderr.contains("team"), "stderr should echo the bad value; got:\n{stderr}");
    assert!(
        stderr.contains("general") || stderr.contains("project"),
        "stderr should list valid options; got:\n{stderr}"
    );
}

#[test]
fn empty_category_value_clap_exit_2() {
    // TC-IHC-4.3
    let sb = sandbox();
    sb.cmd()
        .args([
            "insight", "create", "body",
            "--type", "agent-learned", "--agent", "x",
            "--tags", "foo", "--category", "",
        ])
        .assert()
        .failure()
        .code(2);
}

// ---------------------------------------------------------------------------
// TC-IHC-3.x — `--tags` mandatory (business logic, exact stderr, no write).
// ---------------------------------------------------------------------------

#[test]
fn missing_tags_exit_2_exact_stderr_no_write() {
    // TC-IHC-3.1
    let sb = sandbox();
    let assert = sb
        .cmd()
        .args([
            "insight", "create", "body",
            "--type", "agent-learned", "--agent", "x",
            "--category", "project",
        ])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("error: insight create requires at least one --tag"),
        "expected exact tag-required stderr; got:\n{stderr}"
    );
    assert_eq!(doc_count(&sb.local_db()), 0, "tagless create must not write");
}

#[test]
fn all_tags_reduce_to_empty_exit_2() {
    // TC-IHC-3.2 — sole tag is `#` which strips to empty.
    let sb = sandbox();
    let assert = sb
        .cmd()
        .args([
            "insight", "create", "body",
            "--type", "agent-learned", "--agent", "x",
            "--category", "project", "--tags", "#",
        ])
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("error: insight create requires at least one --tag"),
        "got:\n{stderr}"
    );
}

#[test]
fn piped_body_without_tags_exit_2_before_open() {
    // TC-IHC-3.3 — body via stdin, no --tags: exit 2 before any db open.
    let sb = sandbox();
    let assert = sb
        .cmd()
        .args([
            "insight", "create",
            "--type", "agent-learned", "--agent", "x",
            "--category", "project",
        ])
        .write_stdin("a valid body from stdin")
        .assert()
        .failure()
        .code(2);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("error: insight create requires at least one --tag"),
        "got:\n{stderr}"
    );
    assert_eq!(doc_count(&sb.local_db()), 0, "no write on tagless piped body");
}

// ---------------------------------------------------------------------------
// TC-IHC-1.x — `--category project` routes to the cwd-local db.
// ---------------------------------------------------------------------------

#[test]
fn project_category_writes_local_not_global() {
    // TC-IHC-1.1
    let sb = sandbox();
    sb.cmd()
        .args([
            "insight", "create", "Tokio mutex held across await point",
            "--type", "agent-learned", "--agent", "planner",
            "--feature", "insights-hybrid-corpus", "--salience", "high",
            "--category", "project", "--tags", "tokio", "--tags", "mutex",
        ])
        .assert()
        .success();

    // (b) local row with category='project' + project_slug=cwd-basename.
    let local = open(&sb.local_db());
    let (cat, slug): (String, String) = local
        .query_row(
            "SELECT category, project_slug FROM documents ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cat, "project");
    let expected_basename = sb
        .project
        .path()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap()
        .to_string();
    assert_eq!(slug, expected_basename, "project_slug defaults to cwd basename");

    // (c) insight_tags has tokio + mutex for the new doc.
    let tag_count: i64 = local
        .query_row(
            "SELECT COUNT(*) FROM insight_tags WHERE doc_id=(SELECT MAX(id) FROM documents)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tag_count, 2);
    let tags: Vec<String> = local
        .prepare("SELECT tag FROM insight_tags WHERE doc_id=(SELECT MAX(id) FROM documents) ORDER BY tag")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(tags, vec!["mutex".to_string(), "tokio".to_string()]);

    // (d) global db NOT created.
    assert_eq!(doc_count(&sb.global_db()), 0, "project create must not touch global db");
}

#[test]
fn project_explicit_project_slug_overrides_basename() {
    // TC-IHC-1.5
    let sb = sandbox();
    sb.cmd()
        .args([
            "insight", "create", "explicit slug body",
            "--type", "agent-learned", "--agent", "x",
            "--category", "project", "--project", "myproject", "--tags", "sometag",
        ])
        .assert()
        .success();
    let slug: String = open(&sb.local_db())
        .query_row(
            "SELECT project_slug FROM documents ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(slug, "myproject");
}

#[test]
fn project_tags_strip_leading_hash() {
    // TC-IHC-1.6
    let sb = sandbox();
    sb.cmd()
        .args([
            "insight", "create", "hash strip body",
            "--type", "agent-learned", "--agent", "x",
            "--category", "project", "--tags", "#tokio", "--tags", "#mutex",
        ])
        .assert()
        .success();
    let tags: Vec<String> = open(&sb.local_db())
        .prepare("SELECT tag FROM insight_tags WHERE doc_id=(SELECT MAX(id) FROM documents) ORDER BY tag")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(tags, vec!["mutex".to_string(), "tokio".to_string()]);
    assert!(
        tags.iter().all(|t| !t.starts_with('#')),
        "no stored tag should keep its leading #"
    );
}

#[test]
fn project_duplicate_tags_deduped() {
    // TC-IHC-1.7
    let sb = sandbox();
    sb.cmd()
        .args([
            "insight", "create", "dup tags body",
            "--type", "agent-learned", "--agent", "x",
            "--category", "project",
            "--tags", "tokio", "--tags", "tokio", "--tags", "mutex",
        ])
        .assert()
        .success();
    let n: i64 = open(&sb.local_db())
        .query_row(
            "SELECT COUNT(*) FROM insight_tags WHERE doc_id=(SELECT MAX(id) FROM documents)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2, "duplicate tokio must collapse to one row");
}

#[test]
fn project_create_on_fresh_db_succeeds() {
    // TC-IHC-1.8 — no prior insights.db in the project.
    let sb = sandbox();
    assert!(!sb.local_db().exists(), "precondition: fresh project");
    sb.cmd()
        .args([
            "insight", "create", "fresh db body",
            "--type", "agent-learned", "--agent", "x",
            "--category", "project", "--tags", "foo",
        ])
        .assert()
        .success();
    assert!(sb.local_db().exists(), "local insights.db created on first write");
    // Schema version lives in the `schema_version` TABLE (not PRAGMA
    // user_version) — the codebase convention since v1; Slice 1 stamps 5 here.
    let ver: i64 = open(&sb.local_db())
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 5, "fresh db stamped at schema v5");
    assert_eq!(doc_count(&sb.local_db()), 1);
}

// ---------------------------------------------------------------------------
// TC-IHC-2.x — `--category general` routes to the global db.
// ---------------------------------------------------------------------------

#[test]
fn general_category_writes_global_not_local() {
    // TC-IHC-2.1
    let sb = sandbox();
    sb.cmd()
        .args([
            "insight", "create", "nginx reload sends SIGHUP",
            "--type", "agent-learned", "--agent", "ba-analyst",
            "--feature", "insights-hybrid-corpus", "--salience", "medium",
            "--category", "general", "--tags", "nginx", "--tags", "infrastructure",
        ])
        .assert()
        .success();

    // (b) global row, category='general', project_slug IS NULL.
    let global = open(&sb.global_db());
    let (cat, slug): (String, Option<String>) = global
        .query_row(
            "SELECT category, project_slug FROM documents ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cat, "general");
    assert_eq!(slug, None, "general insight has NULL project_slug");

    // (c) tags persisted in global db.
    let tags: Vec<String> = global
        .prepare("SELECT tag FROM insight_tags WHERE doc_id=(SELECT MAX(id) FROM documents) ORDER BY tag")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(tags, vec!["infrastructure".to_string(), "nginx".to_string()]);

    // (d) cwd-local db NOT created.
    assert_eq!(doc_count(&sb.local_db()), 0, "general create must not touch local db");
}

#[test]
fn general_creates_global_dir_when_absent() {
    // TC-IHC-14.1 — global ~/.claude/knowledge does not exist yet.
    let sb = sandbox();
    let knowledge_dir = sb.home.path().join(".claude/knowledge");
    assert!(!knowledge_dir.exists(), "precondition: global knowledge dir absent");
    sb.cmd()
        .args([
            "insight", "create", "global lesson",
            "--type", "agent-learned", "--agent", "prd-writer", "--salience", "medium",
            "--category", "general", "--tags", "general-knowledge",
        ])
        .assert()
        .success();
    assert!(knowledge_dir.exists(), "global knowledge dir auto-created");
    assert!(sb.global_db().exists(), "global insights.db created");
    let ver: i64 = open(&sb.global_db())
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 5);
    assert_eq!(doc_count(&sb.local_db()), 0, "cwd-local db not created");
}

#[test]
fn general_ignores_project_slug() {
    // TC-IHC-2.3 — --project is silently ignored for general (slug NULL).
    let sb = sandbox();
    sb.cmd()
        .args([
            "insight", "create", "ignored slug body",
            "--type", "agent-learned", "--agent", "x", "--salience", "low",
            "--category", "general", "--project", "myproject", "--tags", "infra",
        ])
        .assert()
        .success();
    let slug: Option<String> = open(&sb.global_db())
        .query_row(
            "SELECT project_slug FROM documents ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(slug, None, "--project must be ignored for --category general");
}

// ---------------------------------------------------------------------------
// TC-IHC-20.x — exact-sha dedup still fires per-db with the new flags.
// ---------------------------------------------------------------------------

#[test]
fn dedup_fires_per_db_project() {
    // TC-IHC-20.1
    let sb = sandbox();
    for _ in 0..2 {
        sb.cmd()
            .args([
                "insight", "create", "identical project body for dedup",
                "--type", "agent-learned", "--agent", "dedupagent",
                "--category", "project", "--tags", "foo", "--json",
            ])
            .assert()
            .success();
    }
    assert_eq!(doc_count(&sb.local_db()), 1, "exact-sha dedup keeps one doc");
}

#[test]
fn dedup_fires_per_db_general() {
    // TC-IHC-20.2
    let sb = sandbox();
    let mut second_stdout = String::new();
    for i in 0..2 {
        let assert = sb
            .cmd()
            .args([
                "insight", "create", "identical general body for dedup",
                "--type", "agent-learned", "--agent", "dedupagent",
                "--category", "general", "--tags", "bar", "--json",
            ])
            .assert()
            .success();
        if i == 1 {
            second_stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
        }
    }
    assert!(
        second_stdout.contains("\"status\": \"deduped\"")
            || second_stdout.contains("\"status\":\"deduped\""),
        "second general write should dedup; got:\n{second_stdout}"
    );
    assert_eq!(doc_count(&sb.global_db()), 1, "global dedup keeps one doc");
    assert_eq!(doc_count(&sb.local_db()), 0, "general writes never touch local db");
}

#[test]
fn cross_agent_general_not_deduped() {
    // TC-IHC-2.7 — same body, two different agents → two global rows.
    let sb = sandbox();
    for agent in ["agentone", "agenttwo"] {
        sb.cmd()
            .args([
                "insight", "create", "same body different agents general",
                "--type", "agent-learned", "--agent", agent,
                "--category", "general", "--tags", "baz",
            ])
            .assert()
            .success();
    }
    let n: i64 = open(&sb.global_db())
        .query_row(
            "SELECT COUNT(*) FROM documents WHERE source_path LIKE 'agent:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2, "cross-agent agreement is NOT deduped");
}
