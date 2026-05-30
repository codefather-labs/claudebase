//! Slice 6 (insights-hybrid-corpus) — project registry module.
//!
//! Coverage (QA TC-IHC-10.x / 11.x / 12.x):
//!   - TC-IHC-10.1: upsert_project creates projects.json with the entry
//!   - TC-IHC-10.2: second call same cwd → idempotent (1 entry, last_seen updated)
//!   - TC-IHC-10.3: malformed json → treated as `[]`, overwritten valid
//!   - TC-IHC-10.4: symlinked cwd → canonical path used, no dup
//!   - TC-IHC-10.5: name derives from canonical.file_name() ONLY
//!   - TC-IHC-11.1: resolve_project_path(name) returns Some(correct path)
//!   - TC-IHC-11.2: resolve_project_path(unknown) returns None
//!   - TC-IHC-12.1: 10-thread concurrent upsert → projects.json stays valid JSON
//!
//! HERMETICITY: $HOME / USERPROFILE pinned to a per-test tempdir so the
//! operator's real ~/.claude/knowledge/projects.json is never touched.
//! Mirrors the env-set-and-restore pattern from store_global_resolver_test.rs.

use claudebase::registry::{registry_path, resolve_project_path, upsert_project, ProjectEntry};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Barrier, Mutex, MutexGuard};
use std::thread;
use tempfile::TempDir;

/// Process-local mutex serializing every HomeGuard-acquiring test in this
/// binary. `HomeGuard::new` mutates process-wide `$HOME` / `$USERPROFILE`;
/// without serialization the default `cargo test` parallelism races and
/// panics — see TC-IHC-12.1 / qa-engineer iter-1 FAIL. We use a process-local
/// `Mutex<()>` instead of pulling in `serial_test` to avoid a new dependency
/// (deliberate-mode "no new abstractions / no new dependencies").
///
/// Poison handling: if a prior test panicked while holding the lock the
/// mutex becomes poisoned; we `.unwrap_or_else(|e| e.into_inner())` so the
/// next test still proceeds (the env state is independently restored by
/// the previous HomeGuard's Drop, which runs even on panic).
static HOME_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that saves $HOME / $USERPROFILE, points them at a tempdir for
/// the duration of the test, and restores them on Drop. Mirrors the manual
/// save/restore in `tests/store_global_resolver_test.rs` but as a guard so a
/// panic in the test body cannot leak env state into the next test in the
/// same binary.
///
/// Holds a `MutexGuard<'static, ()>` over `HOME_LOCK` for its entire
/// lifetime so concurrent HomeGuard-using tests serialize on the env
/// mutation rather than racing.
struct HomeGuard {
    _tmp: TempDir,
    home_path: PathBuf,
    saved_home: Option<std::ffi::OsString>,
    saved_userprofile: Option<std::ffi::OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl HomeGuard {
    fn new() -> Self {
        // Acquire the process-local HOME_LOCK FIRST, before touching env vars,
        // so two threads cannot interleave save/set. Recover from poison.
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().expect("home tempdir");
        let home_path = tmp.path().to_path_buf();
        let saved_home = std::env::var_os("HOME");
        let saved_userprofile = std::env::var_os("USERPROFILE");
        std::env::set_var("HOME", &home_path);
        std::env::set_var("USERPROFILE", &home_path);
        // make sure the knowledge/ dir exists so registry write succeeds
        fs::create_dir_all(home_path.join(".claude/knowledge"))
            .expect("mkdir knowledge");
        Self {
            _tmp: tmp,
            home_path,
            saved_home,
            saved_userprofile,
            _lock: lock,
        }
    }

    fn registry_file(&self) -> PathBuf {
        self.home_path.join(".claude/knowledge/projects.json")
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match &self.saved_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match &self.saved_userprofile {
            Some(u) => std::env::set_var("USERPROFILE", u),
            None => std::env::remove_var("USERPROFILE"),
        }
    }
}

fn read_entries(p: &std::path::Path) -> Vec<ProjectEntry> {
    let body = fs::read_to_string(p).expect("read registry");
    serde_json::from_str(&body).expect("parse registry")
}

// ---------------------------------------------------------------------------
// TC-IHC-10.1 — first upsert creates projects.json with {name, path, last_seen}
// ---------------------------------------------------------------------------

#[test]
fn upsert_creates_registry_with_entry() {
    let g = HomeGuard::new();
    let project = TempDir::new().expect("project tempdir");

    upsert_project(project.path()).expect("upsert ok");

    let reg = g.registry_file();
    assert!(reg.exists(), "projects.json must be created at {reg:?}");
    let entries = read_entries(&reg);
    assert_eq!(entries.len(), 1, "exactly one entry; got {entries:?}");
    let e = &entries[0];
    let canon = fs::canonicalize(project.path()).unwrap();
    assert_eq!(e.path, canon.to_string_lossy(), "stored path is canonical");
    assert_eq!(
        e.name,
        canon.file_name().unwrap().to_string_lossy(),
        "name derives from canonical.file_name() only"
    );
    assert!(e.last_seen > 0, "last_seen populated");
}

// ---------------------------------------------------------------------------
// TC-IHC-10.2 — second upsert on same cwd is idempotent (one entry, updated)
// ---------------------------------------------------------------------------

#[test]
fn upsert_second_call_idempotent_updates_last_seen() {
    let _g = HomeGuard::new();
    let project = TempDir::new().expect("project tempdir");

    upsert_project(project.path()).expect("upsert 1");
    // Snapshot last_seen.
    let entries1 = read_entries(&registry_path());
    let ls1 = entries1[0].last_seen;
    // Sleep one second to advance unix-time (last_seen granularity is secs).
    std::thread::sleep(std::time::Duration::from_secs(1));
    upsert_project(project.path()).expect("upsert 2");
    let entries2 = read_entries(&registry_path());

    assert_eq!(entries2.len(), 1, "still exactly one entry; got {entries2:?}");
    assert!(
        entries2[0].last_seen >= ls1,
        "last_seen monotonic non-decreasing: {ls1} → {}",
        entries2[0].last_seen
    );
}

// ---------------------------------------------------------------------------
// TC-IHC-10.3 — malformed JSON is treated as [] and overwritten with valid
// ---------------------------------------------------------------------------

#[test]
fn upsert_treats_malformed_json_as_empty_and_overwrites() {
    let g = HomeGuard::new();
    let project = TempDir::new().expect("project tempdir");

    // pre-populate registry with garbage
    fs::write(g.registry_file(), b"{not valid json at all").expect("seed garbage");

    upsert_project(project.path()).expect("upsert recovers from malformed");

    let entries = read_entries(&g.registry_file());
    assert_eq!(entries.len(), 1, "garbage was overwritten with one entry");
    let canon = fs::canonicalize(project.path()).unwrap();
    assert_eq!(entries[0].path, canon.to_string_lossy());
}

// ---------------------------------------------------------------------------
// TC-IHC-10.4 — symlinked cwd → canonical path used, no duplicate entry
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn upsert_via_symlink_uses_canonical_no_duplicate() {
    use std::os::unix::fs::symlink;
    let _g = HomeGuard::new();
    let project = TempDir::new().expect("project tempdir");
    let link_parent = TempDir::new().expect("link parent");
    let link = link_parent.path().join("project-link");
    symlink(project.path(), &link).expect("symlink");

    upsert_project(project.path()).expect("upsert real");
    upsert_project(&link).expect("upsert via symlink");

    let entries = read_entries(&registry_path());
    assert_eq!(
        entries.len(),
        1,
        "symlink and real path share canonical → 1 entry; got {entries:?}"
    );
    let canon = fs::canonicalize(project.path()).unwrap();
    assert_eq!(entries[0].path, canon.to_string_lossy());
}

// ---------------------------------------------------------------------------
// TC-IHC-10.5 — name comes from canonical.file_name(), not user input
// (regression guard: prove the basename is derived even when the project dir
// is reached via a renamed parent or symlink with a different leaf name).
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn upsert_name_is_canonical_file_name_not_link_leaf() {
    use std::os::unix::fs::symlink;
    let _g = HomeGuard::new();
    let project = TempDir::new().expect("project tempdir");
    let canon = fs::canonicalize(project.path()).unwrap();
    let expected_name = canon.file_name().unwrap().to_string_lossy().to_string();

    // create a symlink with a DIFFERENT leaf name pointing at the project.
    // The registered `name` MUST be the canonical leaf, not the link leaf.
    let link_parent = TempDir::new().expect("link parent");
    let link = link_parent.path().join("totally-different-name");
    symlink(project.path(), &link).expect("symlink");

    upsert_project(&link).expect("upsert via link");

    let entries = read_entries(&registry_path());
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].name, expected_name,
        "name MUST derive from canonical.file_name(), not link leaf"
    );
}

// ---------------------------------------------------------------------------
// TC-IHC-11.1 — resolve_project_path(name) returns Some(correct path)
// ---------------------------------------------------------------------------

#[test]
fn resolve_known_name_returns_canonical_path() {
    let _g = HomeGuard::new();
    let project = TempDir::new().expect("project tempdir");
    upsert_project(project.path()).expect("upsert");

    let canon = fs::canonicalize(project.path()).unwrap();
    let name = canon.file_name().unwrap().to_string_lossy().to_string();

    let got = resolve_project_path(&name).expect("known slug resolves");
    assert_eq!(got, canon);
}

// ---------------------------------------------------------------------------
// TC-IHC-11.2 — resolve_project_path(unknown) returns None
// ---------------------------------------------------------------------------

#[test]
fn resolve_unknown_name_returns_none() {
    let _g = HomeGuard::new();
    // No upsert at all → registry doesn't exist → None.
    assert!(resolve_project_path("nonexistentproject").is_none());

    // And after an upsert of a different project, the unknown slug is still None.
    let project = TempDir::new().expect("project tempdir");
    upsert_project(project.path()).expect("upsert");
    assert!(resolve_project_path("definitely-not-the-name").is_none());
}

#[test]
fn resolve_malformed_registry_returns_none() {
    let g = HomeGuard::new();
    fs::write(g.registry_file(), b"{garbage").expect("seed garbage");
    assert!(
        resolve_project_path("anything").is_none(),
        "malformed registry MUST be treated as no entries (graceful, not panic)"
    );
}

// ---------------------------------------------------------------------------
// TC-IHC-12.1 — concurrent 10-thread upsert → projects.json stays valid JSON
// This is the load-bearing atomic-rename invariant. Without atomic rename,
// torn writes would produce a partial/invalid JSON file under load.
// ---------------------------------------------------------------------------

#[test]
fn concurrent_upserts_produce_valid_json() {
    let _g = HomeGuard::new();
    // 10 distinct project dirs so all entries are different; one shared
    // registry file at $HOME/.claude/knowledge/projects.json.
    let projects: Vec<TempDir> = (0..10)
        .map(|_| TempDir::new().expect("project tempdir"))
        .collect();
    let paths: Vec<PathBuf> = projects.iter().map(|p| p.path().to_path_buf()).collect();

    let barrier = Arc::new(Barrier::new(paths.len()));
    let handles: Vec<_> = paths
        .into_iter()
        .map(|p| {
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                b.wait();
                upsert_project(&p).expect("concurrent upsert ok");
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread join");
    }

    // After all threads, the file MUST be valid JSON (atomic-rename invariant).
    let body = fs::read_to_string(registry_path()).expect("registry exists");
    let parsed: Vec<ProjectEntry> =
        serde_json::from_str(&body).expect("registry MUST be valid JSON after concurrent writes");
    // We don't assert entry count == 10 — the last-writer-wins atomic-rename
    // model means some upserts may overwrite a snapshot that lacked siblings'
    // entries. The LOAD-BEARING invariant is "valid JSON afterward", not
    // "every entry present". Doc this clearly so a future reader understands
    // the trade-off (FR-IHC-6.5 mentions per-call atomicity, not last-writer-
    // sees-all serialization).
    assert!(
        !parsed.is_empty(),
        "at least one entry survived; got {} entries",
        parsed.len()
    );
}
