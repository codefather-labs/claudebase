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
use std::io::{BufRead, BufReader, BufWriter, Write};
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
        1 => Ok(matches.remove(0).path),
        _ => {
            // Newest first: the usual question is "which one am I in right
            // now", and a conversation continued after its context ran out is
            // a new file carrying the old one's title.
            matches.sort_by(|a, b| b.modified.cmp(&a.modified));
            let list = matches
                .iter()
                .enumerate()
                .map(|(i, m)| describe_match(m, i == 0))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "{} conversations are named `{name}` (a conversation continued after its context \
                 ran out keeps the old title); pass one of these ids instead, newest first:\n{list}",
                matches.len()
            )
        }
    }
}

/// A transcript whose title matched, with what tells it apart from the others.
struct Match {
    path: PathBuf,
    modified: Option<std::time::SystemTime>,
    last_prompt: Option<String>,
}

fn describe_match(m: &Match, newest: bool) -> String {
    let id = m.path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default();
    let when = m
        .modified
        .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let mut prompt: String = m.last_prompt.as_deref().unwrap_or("").replace(['\n', '\r'], " ");
    if prompt.chars().count() > 80 {
        prompt = prompt.chars().take(79).collect::<String>() + "…";
    }
    format!(
        "  {id}  last active {when}{}\n      last prompt: {}",
        if newest { "  (most recent)" } else { "" },
        if prompt.is_empty() { "—".to_owned() } else { format!("\"{prompt}\"") }
    )
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

fn matches_in(folder: &Path, name: &str) -> Vec<Match> {
    let Ok(entries) = fs::read_dir(folder) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|path| {
            let (title, last_prompt) = transcript_meta(&path)?;
            (title.as_deref() == Some(name)).then(|| Match {
                modified: fs::metadata(&path).and_then(|m| m.modified()).ok(),
                path,
                last_prompt,
            })
        })
        .collect()
}

/// The name a transcript is listed under, as described on
/// [`find_transcript_by_name`], and its last prompt — one pass over the file.
pub(crate) fn transcript_meta(path: &Path) -> Option<(Option<String>, Option<String>)> {
    let mut input = BufReader::new(fs::File::open(path).ok()?);
    let (mut custom, mut ai, mut last_prompt) = (None, None, None);
    let mut line = Vec::new();
    loop {
        line.clear();
        match input.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        // Cheap pre-filters: transcripts run to hundreds of megabytes, almost
        // all of it in long message lines, and only these few short lines are
        // of interest. A title or last-prompt line past the cap is a pasted
        // wall of text, not something worth matching on.
        if line.len() > META_LINE_CAP {
            continue;
        }
        if !contains(line, b"\"custom-title\"")
            && !contains(line, b"\"ai-title\"")
            && !contains(line, b"\"last-prompt\"")
        {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        match v.get("type").and_then(Value::as_str) {
            Some("custom-title") => custom = field("customTitle"),
            Some("ai-title") => ai = field("aiTitle"),
            Some("last-prompt") => last_prompt = field("lastPrompt"),
            _ => {}
        }
    }
    Some((custom.or(ai), last_prompt))
}

/// Longest transcript line [`transcript_meta`] looks at.
const META_LINE_CAP: usize = 64 * 1024;

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

/// A conversation name as `/resume` can show it: trimmed, non-empty, one line.
pub(crate) fn check_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("the new conversation needs a non-empty name");
    }
    if name.contains(['\n', '\r']) {
        bail!("the conversation name must be a single line");
    }
    Ok(name)
}

/// Clone the conversation `source` (its id, or the name `/resume` lists it
/// under) as a new conversation titled `name`.
pub fn clone_conversation(
    claude_dir: &Path,
    cwd: &Path,
    source: &str,
    name: &str,
) -> Result<CloneReport> {
    let name = check_name(name)?;

    let (source_id, source) = resolve_conversation(claude_dir, cwd, source)?;
    let source_id = source_id.as_str();
    let folder = source.parent().context("transcript has no parent folder")?;
    let new_id = uuid::Uuid::new_v4().to_string();
    let target = folder.join(format!("{new_id}.jsonl"));

    let rw = Rewrite { source_id, new_id: &new_id, drop_types: DROPPED_TYPES, cwd: None };

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
    let result = (|| -> Result<(usize, usize)> {
        for (from, to) in &side_dirs {
            if from.is_dir() {
                copy_dir(from, to, &rw)
                    .with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
            }
        }
        let input = BufReader::new(
            fs::File::open(&source).with_context(|| format!("open {}", source.display()))?,
        );
        let mut out = BufWriter::new(create_new_file(&target)?);
        let counts = rewrite_lines(input, &mut out, &rw)?;
        append_title(&mut out, &new_id, name)?;
        finish_file(out)?;
        Ok(counts)
    })();
    let (lines_copied, lines_dropped) = match result {
        Ok(counts) => counts,
        Err(e) => {
            for (_, to) in &side_dirs {
                let _ = fs::remove_dir_all(to);
            }
            let _ = fs::remove_file(&target);
            return Err(e);
        }
    };

    Ok(CloneReport { source_id: source_id.to_owned(), new_id, transcript: target, lines_copied, lines_dropped })
}

/// Append the lines `/rename` writes, so `/resume` lists the conversation as
/// `name` whichever of the two it reads.
pub(crate) fn append_title(out: &mut impl Write, id: &str, name: &str) -> Result<()> {
    for (kind, field) in [("custom-title", "customTitle"), ("agent-name", "agentName")] {
        let line = serde_json::json!({ "type": kind, field: name, "sessionId": id });
        out.write_all(line.to_string().as_bytes())?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// How [`rewrite_lines`] changes a transcript on its way through.
pub(crate) struct Rewrite<'a> {
    /// `sessionId` / `session_id` equal to this…
    pub source_id: &'a str,
    /// …become this (may be the same id).
    pub new_id: &'a str,
    /// Line `type`s left out.
    pub drop_types: &'a [&'a str],
    /// `cwd` fields at or under `.0` are moved under `.1` — a conversation
    /// carried to a machine where the project lives somewhere else.
    pub cwd: Option<(&'a str, &'a str)>,
}

/// Stream a transcript (or a subagent transcript) from `input` to `out`,
/// line by line — transcripts run to hundreds of megabytes. Returns
/// `(lines copied, lines dropped)`.
pub(crate) fn rewrite_lines(
    input: impl BufRead,
    out: &mut impl Write,
    rw: &Rewrite,
) -> Result<(usize, usize)> {
    let mut input = input;
    let (mut copied, mut dropped) = (0, 0);
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        // A missing final newline means the writer is mid-line: leave that out.
        if line.last() != Some(&b'\n') {
            break;
        }
        let body = &line[..line.len() - 1];
        if body.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match rewrite_line(body, rw)? {
            Some(bytes) => {
                out.write_all(&bytes)?;
                out.write_all(b"\n")?;
                copied += 1;
            }
            None => dropped += 1,
        }
    }
    Ok((copied, dropped))
}

/// One transcript line rewritten, or `None` when it is dropped. Lines that are
/// not JSON objects pass through byte for byte.
fn rewrite_line(body: &[u8], rw: &Rewrite) -> Result<Option<Vec<u8>>> {
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return Ok(Some(body.to_vec()));
    };
    let Some(obj) = value.as_object_mut() else {
        return Ok(Some(body.to_vec()));
    };
    if obj
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| rw.drop_types.contains(&t))
    {
        return Ok(None);
    }
    let mut changed = false;
    if rw.source_id != rw.new_id {
        for key in ["sessionId", "session_id"] {
            if obj.get(key).and_then(Value::as_str) == Some(rw.source_id) {
                obj.insert(key.to_owned(), Value::String(rw.new_id.to_owned()));
                changed = true;
            }
        }
    }
    if let Some((from, to)) = rw.cwd {
        if let Some(moved) = obj.get("cwd").and_then(Value::as_str).and_then(|c| move_path(c, from, to)) {
            obj.insert("cwd".to_owned(), Value::String(moved));
            changed = true;
        }
    }
    // Untouched lines keep their exact bytes: re-serialising would reorder keys
    // for nothing.
    if !changed {
        return Ok(Some(body.to_vec()));
    }
    Ok(Some(serde_json::to_vec(&value).context("serialize transcript line")?))
}

/// `path` moved from under `from` to under `to`, or `None` when it is not
/// `from` itself or inside it. Either separator counts, so a conversation can
/// move between Windows and Unix.
pub(crate) fn move_path(path: &str, from: &str, to: &str) -> Option<String> {
    let from = from.trim_end_matches(['/', '\\']);
    let rest = path.strip_prefix(from)?;
    if !(rest.is_empty() || rest.starts_with(['/', '\\'])) {
        return None;
    }
    let to = to.trim_end_matches(['/', '\\']);
    let sep = if to.contains('\\') && !to.contains('/') { '\\' } else { '/' };
    Some(format!("{to}{}", rest.replace(['/', '\\'], &sep.to_string())))
}

/// Rewrite one transcript file into a new file.
pub(crate) fn rewrite_file(src: &Path, dst: &Path, rw: &Rewrite) -> Result<(usize, usize)> {
    let input = BufReader::new(fs::File::open(src).with_context(|| format!("open {}", src.display()))?);
    let mut out = BufWriter::new(create_new_file(dst)?);
    let counts = rewrite_lines(input, &mut out, rw)?;
    finish_file(out)?;
    Ok(counts)
}

/// Copy a conversation's side folder. `.jsonl` files go through `rw`;
/// everything else is copied as is. Symlinks are not followed.
fn copy_dir(from: &Path, to: &Path, rw: &Rewrite) -> Result<()> {
    fs::create_dir(to).with_context(|| format!("create {}", to.display()))?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&src, &dst, rw)?;
        } else if kind.is_file() {
            if src.extension().is_some_and(|e| e == "jsonl") {
                rewrite_file(&src, &dst, rw)?;
            } else {
                fs::copy(&src, &dst)?;
            }
        }
    }
    Ok(())
}

/// Create a file that must not exist yet, owner-only like Claude Code's own.
pub(crate) fn create_new_file(path: &Path) -> Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).with_context(|| format!("create {}", path.display()))
}

/// Flush and fsync a file written through a buffer.
pub(crate) fn finish_file(out: BufWriter<fs::File>) -> Result<()> {
    let f = out.into_inner().map_err(|e| e.into_error())?;
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
        let rw = Rewrite { source_id: SRC, new_id: "NEW", drop_types: DROPPED_TYPES, cwd: None };
        let (copied, dropped) = rewrite_lines(raw.as_bytes(), &mut out, &rw).unwrap();
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

    #[test]
    fn cwd_moves_only_at_or_under_the_old_root() {
        let to = "/home/b/proj";
        assert_eq!(move_path("/home/a/proj", "/home/a/proj", to).as_deref(), Some(to));
        assert_eq!(
            move_path("/home/a/proj/src/x", "/home/a/proj/", to).as_deref(),
            Some("/home/b/proj/src/x")
        );
        // A sibling that merely shares the prefix is not inside the project.
        assert_eq!(move_path("/home/a/project2", "/home/a/proj", to), None);
        assert_eq!(move_path("/tmp", "/home/a/proj", to), None);
        // Windows to Unix: the separators follow the destination.
        assert_eq!(
            move_path(r"C:\Users\a\proj\src", r"C:\Users\a\proj", to).as_deref(),
            Some("/home/b/proj/src")
        );
        assert_eq!(
            move_path("/home/a/proj/src", "/home/a/proj", r"C:\w\proj").as_deref(),
            Some(r"C:\w\proj\src")
        );
    }

    #[test]
    fn untouched_lines_keep_their_bytes_and_cwd_is_moved() {
        let raw = "{\"type\":\"summary\",\"b\":1,\"a\":2}\n\
                   {\"type\":\"user\",\"cwd\":\"/home/a/proj/sub\",\"sessionId\":\"S\"}\n";
        let rw = Rewrite {
            source_id: "S",
            new_id: "S",
            drop_types: &["bridge-session"],
            cwd: Some(("/home/a/proj", "/home/b/p")),
        };
        let mut out = Vec::new();
        rewrite_lines(raw.as_bytes(), &mut out, &rw).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "{\"type\":\"summary\",\"b\":1,\"a\":2}");
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["cwd"], "/home/b/p/sub");
        assert_eq!(second["sessionId"], "S");
    }
}
