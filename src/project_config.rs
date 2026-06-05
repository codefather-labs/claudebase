//! Per-project session configuration persisted at
//! `<project-root>/.claudebase/config.json`.
//!
//! Operator vision (2026-06-03 / 2026-06-04):
//!
//! - `claudebase run` creates `<cwd>/.claudebase/config.json` on first
//!   invocation in a project directory. The file stores a stable
//!   `session_id` (UUID v4 by default) and a human-friendly `name`
//!   (cwd basename by default).
//! - `claudebase run` exports `CLAUDEBASE_SESSION_ID` and
//!   `CLAUDEBASE_SESSION_NAME` env vars to the spawned `claude` process.
//!   The plugin bridge inherits these and uses `session_id` as the
//!   `agent_id` on its `agent_register` self-bootstrap so the daemon
//!   has the SAME identifier the file does.
//! - When the user asks Mira to rename the session (e.g. "register as
//!   mira"), Mira calls `agent_register` via the MCP tool — the
//!   bridge persists the new `agent_id` back into this file via
//!   `write_session_id` so the next CC restart from the same cwd
//!   reuses the renamed id. The daemon and the file stay in sync.
//!
//! The directory is **per-project** so a future slice can stash other
//! per-project hints alongside `session_id` (e.g. routing preferences,
//! per-project log paths, etc.) without revisiting this contract.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const CONFIG_DIRNAME: &str = ".claudebase";
const CONFIG_FILENAME: &str = "config.json";

/// Persisted per-project session config. Forward-compatible: unknown
/// fields are ignored on read so a newer claudebase build can land
/// extra keys without breaking older readers (`#[serde(default)]` on
/// every existing field, `flatten` open for additions).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectConfig {
    /// Stable session identifier. Used as `agent_id` in the bridge's
    /// auto-register call so the daemon row matches the on-disk file.
    /// Defaults to a fresh UUID v4 on first `load_or_create`. Renames
    /// rewrite this field via `write_session_id`.
    pub session_id: String,
    /// Human-friendly name passed as `name` in `agent_register`.
    /// Defaults to the project-root basename. May diverge from
    /// `session_id` (e.g. after a rename the id can be `mira` while
    /// the name stays the original basename, or both can move).
    pub name: String,
}

/// Compute the on-disk config-file path for a given project root.
pub fn config_path(project_root: &Path) -> PathBuf {
    project_root.join(CONFIG_DIRNAME).join(CONFIG_FILENAME)
}

/// Compute the parent `.claudebase/` directory path.
pub fn config_dir(project_root: &Path) -> PathBuf {
    project_root.join(CONFIG_DIRNAME)
}

/// Read-only load. Returns `Some(cfg)` when the file exists and parses
/// to a non-empty `session_id`, `None` otherwise. Used by the plugin
/// bridge so a CC session started OUTSIDE `claudebase run` (e.g. via a
/// desktop shortcut from a random cwd) never accidentally pollutes
/// that cwd with a `.claudebase/` directory.
pub fn load(project_root: &Path) -> Option<ProjectConfig> {
    let path = config_path(project_root);
    if !path.exists() {
        return None;
    }
    let body = std::fs::read_to_string(&path).ok()?;
    let cfg: ProjectConfig = serde_json::from_str(&body).ok()?;
    if cfg.session_id.trim().is_empty() {
        return None;
    }
    Some(cfg)
}

/// Load existing config; if missing OR malformed, create a fresh one
/// with a UUID v4 `session_id` and cwd-basename `name`, persist it,
/// and return it. Malformed-on-disk is handled by rewriting with a
/// fresh default rather than bubbling a parse error to the caller —
/// the file is a cache hint, not a fail-closed gate.
///
/// Caller MUST pass an absolute (or at least caller-canonicalised)
/// `project_root` — this function does NOT resolve relative paths.
pub fn load_or_create(project_root: &Path) -> Result<ProjectConfig> {
    let path = config_path(project_root);
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(body) => match serde_json::from_str::<ProjectConfig>(&body) {
                Ok(cfg) if !cfg.session_id.trim().is_empty() => return Ok(cfg),
                Ok(_) => {
                    // Empty session_id — treat as corrupt, fall through to rewrite.
                }
                Err(_) => {
                    // Malformed JSON — treat as corrupt, fall through to rewrite.
                }
            },
            Err(_) => {
                // Read error — fall through to rewrite (best-effort).
            }
        }
    }
    let cfg = default_config(project_root);
    persist(&path, &cfg)?;
    Ok(cfg)
}

/// Rewrite `session_id` (and optionally `name`) in the on-disk config.
/// Used by the bridge when the user renames the session via
/// `agent_register` MCP tool. If the file does not exist, this
/// function creates it with default `name` (project basename).
pub fn write_session_id(
    project_root: &Path,
    new_session_id: &str,
    new_name: Option<&str>,
) -> Result<()> {
    if new_session_id.trim().is_empty() {
        anyhow::bail!("write_session_id: new_session_id must be non-empty");
    }
    let path = config_path(project_root);
    let mut cfg = if path.exists() {
        match std::fs::read_to_string(&path).ok().and_then(|b| {
            serde_json::from_str::<ProjectConfig>(&b).ok()
        }) {
            Some(c) => c,
            None => default_config(project_root),
        }
    } else {
        default_config(project_root)
    };
    cfg.session_id = new_session_id.trim().to_string();
    if let Some(n) = new_name {
        let trimmed = n.trim();
        if !trimmed.is_empty() {
            cfg.name = trimmed.to_string();
        }
    }
    persist(&path, &cfg)?;
    Ok(())
}

fn default_config(project_root: &Path) -> ProjectConfig {
    let name = project_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "claudebase-cli".to_string());
    ProjectConfig {
        session_id: uuid::Uuid::new_v4().to_string(),
        name,
    }
}

fn persist(path: &Path, cfg: &ProjectConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create .claudebase dir at {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(cfg)
        .context("serialize ProjectConfig")?;
    std::fs::write(path, body)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmpdir")
    }

    #[test]
    fn load_or_create_makes_file_with_uuid_and_basename_on_first_call() {
        let tmp = tmpdir();
        let cfg = load_or_create(tmp.path()).expect("load_or_create");
        assert!(
            !cfg.session_id.trim().is_empty(),
            "session_id must be non-empty"
        );
        // UUID v4 string is 36 chars with 4 dashes.
        assert_eq!(cfg.session_id.len(), 36, "UUID should be 36 chars");
        assert_eq!(cfg.session_id.matches('-').count(), 4);
        // name should default to the tempdir basename, which is non-empty.
        assert!(!cfg.name.trim().is_empty(), "name must be non-empty");
        // file exists on disk
        let path = config_path(tmp.path());
        assert!(path.exists(), "config file must be created on disk");
    }

    #[test]
    fn load_or_create_reuses_existing_session_id_on_second_call() {
        let tmp = tmpdir();
        let first = load_or_create(tmp.path()).unwrap();
        let second = load_or_create(tmp.path()).unwrap();
        assert_eq!(
            first.session_id, second.session_id,
            "session_id must persist across load_or_create calls"
        );
        assert_eq!(first.name, second.name);
    }

    #[test]
    fn write_session_id_rewrites_file_and_load_returns_new_value() {
        let tmp = tmpdir();
        let initial = load_or_create(tmp.path()).unwrap();
        write_session_id(tmp.path(), "mira", None).unwrap();
        let reloaded = load_or_create(tmp.path()).unwrap();
        assert_eq!(reloaded.session_id, "mira");
        // name should be preserved when not passed
        assert_eq!(reloaded.name, initial.name);
    }

    #[test]
    fn write_session_id_also_updates_name_when_provided() {
        let tmp = tmpdir();
        load_or_create(tmp.path()).unwrap();
        write_session_id(tmp.path(), "mira", Some("Mira the Agent")).unwrap();
        let reloaded = load_or_create(tmp.path()).unwrap();
        assert_eq!(reloaded.session_id, "mira");
        assert_eq!(reloaded.name, "Mira the Agent");
    }

    #[test]
    fn write_session_id_rejects_empty_id() {
        let tmp = tmpdir();
        let err = write_session_id(tmp.path(), "   ", None).unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn write_session_id_creates_file_when_absent() {
        let tmp = tmpdir();
        // no load_or_create first
        write_session_id(tmp.path(), "mira", Some("Mira")).unwrap();
        let cfg = load_or_create(tmp.path()).unwrap();
        assert_eq!(cfg.session_id, "mira");
        assert_eq!(cfg.name, "Mira");
    }

    #[test]
    fn load_or_create_rewrites_malformed_json() {
        let tmp = tmpdir();
        let path = config_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{this is not valid json").unwrap();
        let cfg = load_or_create(tmp.path()).unwrap();
        // should be fresh — UUID, non-empty
        assert_eq!(cfg.session_id.len(), 36);
    }

    #[test]
    fn load_or_create_rewrites_empty_session_id() {
        let tmp = tmpdir();
        let path = config_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"session_id":"","name":"foo"}"#).unwrap();
        let cfg = load_or_create(tmp.path()).unwrap();
        assert_ne!(cfg.session_id, "");
        assert_eq!(cfg.session_id.len(), 36);
    }

    #[test]
    fn load_returns_none_when_absent() {
        let tmp = tmpdir();
        assert!(
            load(tmp.path()).is_none(),
            "load must return None when file is absent (no auto-create)"
        );
        // and MUST NOT create the file as a side effect
        assert!(!config_path(tmp.path()).exists());
    }

    #[test]
    fn load_returns_some_when_file_present_and_valid() {
        let tmp = tmpdir();
        load_or_create(tmp.path()).unwrap();
        let loaded = load(tmp.path()).expect("Some after load_or_create");
        assert_eq!(loaded.session_id.len(), 36);
    }

    #[test]
    fn load_returns_none_on_malformed_file() {
        let tmp = tmpdir();
        let path = config_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(tmp.path()).is_none());
    }
}
