//! Project registry — maps a project slug (the canonical-path basename) to
//! the project's canonical filesystem root. Lives at
//! `~/.claude/knowledge/projects.json`, alongside `insights.db`.
//!
//! Written by `claudebase run` (see `run_claude_with_preset` in `main.rs`)
//! on every invocation: `upsert_project(cwd)` records the canonical cwd so
//! subsequent insight reads can resolve `--project <slug>` to the right
//! per-project insights.db without the operator having to type a path.
//!
//! # SECURITY
//!
//! Two invariants jointly contain the attack surface to "nothing":
//!
//! 1. **`name` derives ONLY from `canonical.file_name()`.** The slug is
//!    extracted from the *canonicalized* project path's leaf segment via
//!    [`std::path::Path::file_name`]. It is NEVER derived from a CLI
//!    argument, environment variable, or any other user-influenced input.
//!    An attacker who controls the cwd basename (e.g. via a symlink with a
//!    deceptive leaf) cannot inject path separators or `..` — canonicalize
//!    has already resolved those, and `file_name()` returns a single OS
//!    component with no separators by definition.
//!
//! 2. **The registry file path is a fixed HOME-rooted constant.** It is
//!    always `$HOME/.claude/knowledge/projects.json`, computed once from
//!    `std::env::var_os("HOME")` and never joined with user input.
//!    Same security posture as `store::resolve_global_insights_db` —
//!    no user-input segment ever reaches the path, so the
//!    `cli::resolve_project_root` cwd-containment gate is unnecessary.
//!
//! # Atomicity (FR-IHC-6.5)
//!
//! [`upsert_project`] writes the new JSON snapshot to a sibling temp file
//! `projects.json.tmp.<pid>` then `fs::rename`s it to the final name.
//! POSIX guarantees `rename` is atomic when source and target are on the
//! same filesystem; same-directory siblings always are. On Windows the
//! semantics are near-atomic via `MoveFileEx(MOVEFILE_REPLACE_EXISTING)`
//! — torn writes are still impossible, but a concurrent writer may see
//! one write briefly disappear before the next replacement lands. The
//! TC-IHC-12.1 concurrency test asserts the load-bearing invariant
//! (registry stays valid JSON under concurrent load), not last-writer-
//! sees-all serialization.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process monotonic counter that disambiguates concurrent
/// `upsert_project` calls from the same PID. Bare `std::process::id()` on
/// the temp-file name is insufficient because multiple threads inside the
/// same process share a PID — without the counter, two concurrent calls
/// would race for the same `projects.json.tmp.<pid>` filename and one
/// rename would fail with ENOENT (TC-IHC-12.1 caught this).
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One row of the registry. The on-disk shape is a JSON array of these
/// objects at `$HOME/.claude/knowledge/projects.json`.
///
/// Field semantics:
/// - `name`: the basename of the canonicalized project root. The slug
///   `--project <name>` resolves against this field. NEVER user-supplied.
/// - `path`: the canonicalized absolute project root (UTF-8 lossy string
///   of `std::path::Path`). Symlink-following, `..` resolution and OS
///   case-folding (on case-preserving FS) have all happened by the time
///   the value reaches disk.
/// - `last_seen`: Unix epoch seconds at the moment of the upsert. Used by
///   future garbage collection / "recently active" displays; the only
///   invariant today is that it monotonically updates on each upsert.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    pub name: String,
    pub path: String,
    pub last_seen: u64,
}

/// Return the registry file path: `$HOME/.claude/knowledge/projects.json`.
///
/// Mirrors `store::resolve_global_insights_db`'s HOME-rooted-constant
/// pattern (insights-base doc#22 caller-trust contract): no user input
/// reaches this path, so the cwd-containment gate is unnecessary and the
/// helper itself is infallible — it falls back to `/tmp` when `HOME` and
/// `USERPROFILE` are both unset, matching `user_level_chat_db_path()`.
pub fn registry_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    PathBuf::from(home)
        .join(".claude")
        .join("knowledge")
        .join("projects.json")
}

/// Upsert the project rooted at `root` into the registry.
///
/// Steps:
/// 1. Canonicalize `root` (resolves symlinks + `..`).
/// 2. Derive `name` from `canonical.file_name()` — error if the canonical
///    path has no file_name (root-of-filesystem inputs only).
/// 3. Read the registry; treat MISSING file OR MALFORMED JSON as `[]` so
///    the function never fails open on a stale-corrupted registry.
/// 4. Find the entry by canonical-path string equality; update `last_seen`
///    if present, append `{name, path, last_seen}` if not.
/// 5. Serialize the new snapshot; write to `projects.json.tmp.<pid>` in
///    the SAME directory; `fs::rename` to the final name (POSIX-atomic,
///    Windows-near-atomic).
///
/// Returns `Err(String)` on any I/O / canonicalization failure. The CLI
/// caller in `main.rs::run_claude_with_preset` logs the error to stderr
/// non-fatally and proceeds to `exec()` — the registry is a convenience,
/// not a correctness requirement for `claudebase run` itself.
pub fn upsert_project(root: &Path) -> Result<(), String> {
    // Step 1: canonicalize.
    let canonical = std::fs::canonicalize(root)
        .map_err(|e| format!("canonicalize {}: {e}", root.display()))?;

    // Step 2: derive slug from canonical leaf. No user input contributes.
    let name = canonical
        .file_name()
        .ok_or_else(|| {
            format!(
                "registry: canonical path {} has no file_name component (root of filesystem?)",
                canonical.display()
            )
        })?
        .to_string_lossy()
        .into_owned();

    let canonical_str = canonical.to_string_lossy().into_owned();
    let now = now_unix_secs();

    let reg = registry_path();
    // Ensure parent dir exists — mirrors resolve_global_insights_db pattern.
    if let Some(parent) = reg.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create registry parent {}: {e}", parent.display()))?;
    }

    // Step 3: read existing snapshot, graceful on missing/malformed.
    let mut entries: Vec<ProjectEntry> = match std::fs::read_to_string(&reg) {
        Ok(body) => serde_json::from_str(&body).unwrap_or_else(|_| Vec::new()),
        Err(_) => Vec::new(),
    };

    // Step 4: upsert by canonical-path equality.
    if let Some(existing) = entries.iter_mut().find(|e| e.path == canonical_str) {
        existing.last_seen = now;
        // Refresh `name` too in case the canonical leaf changed since the
        // last upsert (rare but possible on rename). This keeps the slug in
        // sync with reality without inventing migration logic.
        existing.name = name;
    } else {
        entries.push(ProjectEntry {
            name,
            path: canonical_str,
            last_seen: now,
        });
    }

    // Step 5: atomic write via sibling-temp rename.
    let body = serde_json::to_vec_pretty(&entries)
        .map_err(|e| format!("serialize registry: {e}"))?;
    // Per-call unique temp name: <pid>.<monotonic-counter>. PID alone races
    // when multiple threads of the same process call upsert concurrently.
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = reg.with_extension(format!("json.tmp.{}.{}", std::process::id(), seq));
    std::fs::write(&tmp, &body)
        .map_err(|e| format!("write temp registry {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &reg).map_err(|e| {
        // Best-effort cleanup so failed renames don't leave stale .tmp files.
        let _ = std::fs::remove_file(&tmp);
        format!("rename {} → {}: {e}", tmp.display(), reg.display())
    })?;

    Ok(())
}

/// Resolve a project slug to its canonical filesystem root, looking up the
/// registry written by [`upsert_project`].
///
/// Returns:
/// - `Some(PathBuf)` when an entry whose `name == slug` exists.
/// - `None` when the registry file is missing, malformed, or contains no
///   matching entry. ALL three "not found" cases collapse to `None` so
///   callers can use a single match arm.
///
/// Callers in `main.rs` (the insight read path, specifically
/// `resolve_registry_project_db`) turn `None` into the operator-facing
/// `error: project '<slug>' not found in registry` + exit 1.
pub fn resolve_project_path(slug: &str) -> Option<PathBuf> {
    let body = std::fs::read_to_string(registry_path()).ok()?;
    let entries: Vec<ProjectEntry> = serde_json::from_str(&body).ok()?;
    entries
        .into_iter()
        .find(|e| e.name == slug)
        .map(|e| PathBuf::from(e.path))
}

/// Unix epoch seconds — small helper so the call site stays readable.
/// SystemTime::now() can fail if the system clock is before UNIX_EPOCH
/// (e.g., a clock that has rolled back during NTP correction). We fall
/// back to 0 in that pathological case rather than failing the upsert.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
