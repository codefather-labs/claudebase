//! `claudebase chat clone <id-or-name> <name>` — copy a Claude Code
//! conversation under a new id and a new name, so `/resume` (or
//! `claude --resume <new-id>`) can continue from the same context while the
//! original stays untouched.
//!
//! ## What a conversation is on disk
//!
//! Claude Code keeps one conversation as
//!
//! ```text
//! <claude-dir>/projects/<cwd-slug>/<id>.jsonl   the transcript, one JSON object per line
//! <claude-dir>/projects/<cwd-slug>/<id>/        subagent transcripts, spilled tool results
//! <claude-dir>/file-history/<id>/               file backups behind /rewind
//! ```
//!
//! Almost every transcript line names its conversation in `sessionId` (a few
//! in `session_id` too). The `/resume` picker takes the name from
//! `custom-title` / `agent-name` lines, which is what `/rename` writes.
//!
//! ## What the clone changes
//!
//! - `sessionId` / `session_id` equal to the source id become the new id —
//!   in the transcript and in every subagent transcript.
//! - The source's `custom-title` and `agent-name` lines are dropped and the new
//!   name is appended, so whichever line the picker reads, it reads ours.
//! - `bridge-session` lines are dropped: they tie a conversation to a Remote
//!   Control session, and two conversations sharing one would fight over it.
//! - Lines that are not JSON (Claude Code occasionally leaves a NUL-filled
//!   line behind) are copied byte for byte: the clone is as readable as the
//!   source, no more and no less. A torn last line — the source is being
//!   written right now — is left out.
//!
//! Message uuids are kept: they chain messages within a transcript, and the
//! transcript is a separate file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Line types that belong to the source conversation and are not copied.
const DROPPED_TYPES: &[&str] = &["custom-title", "agent-name", "bridge-session"];

/// What `clone_conversation` produced.
#[derive(Debug)]
pub struct CloneReport {
    pub source_id: String,
    pub new_id: String,
    pub transcript: PathBuf,
    pub lines_copied: usize,
    pub lines_dropped: usize,
}

/// Claude Code's configuration directory: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .context("neither HOME nor USERPROFILE is set — cannot find Claude Code's directory")?;
    Ok(PathBuf::from(home).join(".claude"))
}

/// The directory name Claude Code files a working directory's conversations
/// under: every character outside `[A-Za-z0-9]` becomes `-`.
pub fn project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Find `<id>.jsonl`: first under the working directory's project folder, then
/// anywhere under `projects/`. More than one match outside the cwd is refused
/// rather than guessed at.
pub fn find_transcript(claude_dir: &Path, cwd: &Path, id: &str) -> Result<PathBuf> {
    let file = format!("{id}.jsonl");
    let mut found = Vec::new();
    for folder in search_folders(claude_dir, cwd) {
        let candidate = folder.join(&file);
        if candidate.is_file() {
            found.push(candidate);
        }
    }
    match found.len() {
        0 => bail!(
            "no conversation {id} under {} — check the id (the transcript file names are the ids)",
            claude_dir.join("projects").display()
        ),
        1 => Ok(found.remove(0)),
        _ => bail!(
            "conversation {id} exists in several project folders: {}",
            found.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Find a conversation by the name `/resume` lists it under: the last
/// `custom-title` (what `/rename` sets) or, failing that, the last `ai-title`.
/// The working directory's project folder is searched first; other folders
/// only when it has no match. Several matches are refused with their ids.
pub fn find_transcript_by_name(claude_dir: &Path, cwd: &Path, name: &str) -> Result<PathBuf> {
    let here = claude_dir.join("projects").join(project_slug(cwd));
    let mut matches = matches_in(&here, name);
    if matches.is_empty() {
        for folder in search_folders(claude_dir, cwd) {
            if folder != here {
                matches.extend(matches_in(&folder, name));
            }
        }
    }
    match matches.len() {
        0 => bail!("no conversation named `{name}` — pass its id instead, or check the name in /resume"),
        1 => Ok(matches.remove(0)),
        _ => bail!(
            "{} conversations are named `{name}`; pass one of their ids instead:\n{}",
            matches.len(),
            matches.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n")
        ),
    }
}

/// The working directory's project folder, then every other one.
fn search_folders(claude_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let projects = claude_dir.join("projects");
    let here = projects.join(project_slug(cwd));
    let mut folders = vec![here.clone()];
    if let Ok(entries) = fs::read_dir(&projects) {
        let mut rest: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && *p != here)
            .collect();
        rest.sort();
        folders.extend(rest);
    }
    folders
}

fn matches_in(folder: &Path, name: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(folder) else { return Vec::new() };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter(|p| transcript_title(p).as_deref() == Some(name))
        .collect();
    out.sort();
    out
}

/// The name a transcript is listed under, as described on
/// [`find_transcript_by_name`].
pub fn transcript_title(path: &Path) -> Option<String> {
    let raw = fs::read(path).ok()?;
    let (mut custom, mut ai) = (None, None);
    for line in raw.split(|&b| b == b'\n') {
        // Cheap pre-filter: transcripts run to tens of megabytes and only the
        // title lines are of interest.
        if !contains(line, b"\"custom-title\"") && !contains(line, b"\"ai-title\"") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        match v.get("type").and_then(Value::as_str) {
            Some("custom-title") => {
                custom = v.get("customTitle").and_then(Value::as_str).map(str::to_owned)
            }
            Some("ai-title") => ai = v.get("aiTitle").and_then(Value::as_str).map(str::to_owned),
            _ => {}
        }
    }
    custom.or(ai)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Resolve what the operator typed: a UUID is an id, anything else a name.
pub fn resolve_conversation(claude_dir: &Path, cwd: &Path, key: &str) -> Result<(String, PathBuf)> {
    let key = key.trim();
    let path = if uuid::Uuid::parse_str(key).is_ok() {
        find_transcript(claude_dir, cwd, key)?
    } else {
        find_transcript_by_name(claude_dir, cwd, key)?
    };
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("transcript file name is not UTF-8")?
        .to_owned();
    Ok((id, path))
}

/// Clone the conversation `source` (its id, or the name `/resume` lists it
/// under) as a new conversation titled `name`.
pub fn clone_conversation(
    claude_dir: &Path,
    cwd: &Path,
    source: &str,
    name: &str,
) -> Result<CloneReport> {
    let name = name.trim();
    if name.is_empty() {
        bail!("the new conversation needs a non-empty name");
    }
    if name.contains(['\n', '\r']) {
        bail!("the conversation name must be a single line");
    }

    let (source_id, source) = resolve_conversation(claude_dir, cwd, source)?;
    let source_id = source_id.as_str();
    let folder = source.parent().context("transcript has no parent folder")?;
    let new_id = uuid::Uuid::new_v4().to_string();
    let target = folder.join(format!("{new_id}.jsonl"));

    let raw = fs::read(&source).with_context(|| format!("read {}", source.display()))?;
    let mut out = Vec::with_capacity(raw.len() + 256);
    let (lines_copied, lines_dropped) = rewrite_transcript(&raw, source_id, &new_id, &mut out)?;
    for (kind, field) in [("custom-title", "customTitle"), ("agent-name", "agentName")] {
        let line = serde_json::json!({ "type": kind, field: name, "sessionId": new_id });
        out.extend_from_slice(line.to_string().as_bytes());
        out.push(b'\n');
    }

    // Side folders first, transcript last: the transcript is what makes the
    // clone visible to /resume, so it appears only once everything it points
    // at is in place. Any failure removes what was written.
    let side_dirs = [
        (folder.join(source_id), folder.join(&new_id)),
        (
            claude_dir.join("file-history").join(source_id),
            claude_dir.join("file-history").join(&new_id),
        ),
    ];
    let result = (|| -> Result<()> {
        for (from, to) in &side_dirs {
            if from.is_dir() {
                copy_dir(from, to, source_id, &new_id)
                    .with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
            }
        }
        write_new_file(&target, &out)
    })();
    if let Err(e) = result {
        for (_, to) in &side_dirs {
            let _ = fs::remove_dir_all(to);
        }
        let _ = fs::remove_file(&target);
        return Err(e);
    }

    Ok(CloneReport { source_id: source_id.to_owned(), new_id, transcript: target, lines_copied, lines_dropped })
}

/// Rewrite a transcript (or a subagent transcript) into `out`. Returns
/// `(lines copied, lines dropped)`.
fn rewrite_transcript(
    raw: &[u8],
    source_id: &str,
    new_id: &str,
    out: &mut Vec<u8>,
) -> Result<(usize, usize)> {
    // A missing final newline means the writer is mid-line: leave that out.
    let complete = match raw.iter().rposition(|&b| b == b'\n') {
        Some(i) => &raw[..=i],
        None => &raw[..0],
    };
    let (mut copied, mut dropped) = (0, 0);
    for line in complete.split(|&b| b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(mut value) = serde_json::from_slice::<Value>(line) else {
            out.extend_from_slice(line);
            out.push(b'\n');
            copied += 1;
            continue;
        };
        let Some(obj) = value.as_object_mut() else {
            out.extend_from_slice(line);
            out.push(b'\n');
            copied += 1;
            continue;
        };
        if obj
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| DROPPED_TYPES.contains(&t))
        {
            dropped += 1;
            continue;
        }
        for key in ["sessionId", "session_id"] {
            if obj.get(key).and_then(Value::as_str) == Some(source_id) {
                obj.insert(key.to_owned(), Value::String(new_id.to_owned()));
            }
        }
        serde_json::to_writer(&mut *out, &value).context("serialize transcript line")?;
        out.push(b'\n');
        copied += 1;
    }
    Ok((copied, dropped))
}

/// Copy a conversation's side folder. `.jsonl` files get their conversation id
/// rewritten; everything else is copied as is. Symlinks are not followed.
fn copy_dir(from: &Path, to: &Path, source_id: &str, new_id: &str) -> Result<()> {
    fs::create_dir(to).with_context(|| format!("create {}", to.display()))?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&src, &dst, source_id, new_id)?;
        } else if kind.is_file() {
            if src.extension().is_some_and(|e| e == "jsonl") {
                let raw = fs::read(&src)?;
                let mut out = Vec::with_capacity(raw.len());
                rewrite_transcript(&raw, source_id, new_id, &mut out)?;
                write_new_file(&dst, &out)?;
            } else {
                fs::copy(&src, &dst)?;
            }
        }
    }
    Ok(())
}

/// Write a file that must not exist yet, owner-only like Claude Code's own.
fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| format!("create {}", path.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "11111111-1111-4111-8111-111111111111";

    #[test]
    fn slug_replaces_every_non_alphanumeric() {
        assert_eq!(
            project_slug(Path::new("/home/u/.config/my_proj")),
            "-home-u--config-my-proj"
        );
    }

    #[test]
    fn rewrite_swaps_ids_drops_identity_and_keeps_junk() {
        let raw = format!(
            "{{\"type\":\"user\",\"sessionId\":\"{SRC}\",\"session_id\":\"{SRC}\",\"message\":\"mentions {SRC}\"}}\n\
             {{\"type\":\"custom-title\",\"customTitle\":\"old\",\"sessionId\":\"{SRC}\"}}\n\
             {{\"type\":\"bridge-session\",\"sessionId\":\"{SRC}\"}}\n\
             \0\0\0\n\
             {{\"type\":\"file-history-snapshot\"}}\n\
             {{\"type\":\"user\",\"sessionId\""
        );
        let mut out = Vec::new();
        let (copied, dropped) = rewrite_transcript(raw.as_bytes(), SRC, "NEW", &mut out).unwrap();
        assert_eq!((copied, dropped), (3, 2));
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["sessionId"], "NEW");
        assert_eq!(first["session_id"], "NEW");
        // Free text is never rewritten.
        assert_eq!(first["message"], format!("mentions {SRC}"));
        assert_eq!(lines[1], "\0\0\0");
        assert_eq!(lines[2], "{\"type\":\"file-history-snapshot\"}");
    }
}
