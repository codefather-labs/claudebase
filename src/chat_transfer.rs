//! `claudebase chat dump <id-or-name>` / `claudebase chat load <archive.zip>` —
//! carry a Claude Code conversation to another machine.
//!
//! `dump` packs everything Claude Code keeps for one conversation (see
//! [`crate::chat_clone`] for the on-disk layout) into one zip:
//!
//! ```text
//! manifest.json        format, version, id, name, the project path it came from
//! transcript.jsonl     <claude-dir>/projects/<slug>/<id>.jsonl
//! session/...          <claude-dir>/projects/<slug>/<id>/      subagents, tool results
//! file-history/...     <claude-dir>/file-history/<id>/         /rewind backups
//! ```
//!
//! `load` unpacks it under the working directory's project folder, so `/resume`
//! run from there lists it as a native conversation. On the way in:
//!
//! - the id is kept — it is the same conversation, moved — unless `--new-id`
//!   asks for a separate copy; a conversation already present is refused
//!   rather than overwritten;
//! - `cwd` fields at or under the source project path are moved under the
//!   working directory, since the project rarely lives at the same path on
//!   the other machine;
//! - `bridge-session` lines are dropped: they bind the conversation to the
//!   source machine's Remote Control session;
//! - free text (messages, tool calls) is left as written — it is history.
//!
//! The archive comes from another machine and is treated as untrusted: only
//! the entries above are extracted, by their sanitised name, regular files
//! only; nothing outside the destination folders can be written, and nothing
//! existing is overwritten. Everything lands in staging folders first and the
//! transcript is put in place last, so a failed load leaves no half
//! conversation visible to `/resume`.

use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chat_clone::{
    append_title, check_name, create_new_file, finish_file, project_slug, resolve_conversation,
    rewrite_lines, transcript_meta, Rewrite,
};

/// `manifest.json`'s `format` — what tells a conversation dump from any zip.
const FORMAT: &str = "claudebase-chat-dump";
/// Bumped when a reader of the previous version could misread the archive.
const VERSION: u32 = 1;

const MANIFEST: &str = "manifest.json";
const TRANSCRIPT: &str = "transcript.jsonl";
const SESSION_DIR: &str = "session";
const FILE_HISTORY_DIR: &str = "file-history";

/// Lines that belong to the machine the conversation came from.
const DROPPED_ON_LOAD: &[&str] = &["bridge-session"];

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    session_id: String,
    title: Option<String>,
    /// The project directory the conversation was filed under on the source
    /// machine — what `cwd` fields are moved away from on load.
    source_cwd: Option<String>,
    created_at: String,
    claudebase_version: String,
}

/// What [`dump_conversation`] produced.
#[derive(Debug)]
pub struct DumpReport {
    pub session_id: String,
    pub title: Option<String>,
    pub archive: PathBuf,
    pub files: usize,
    pub bytes_in: u64,
}

/// What [`load_conversation`] produced.
#[derive(Debug)]
pub struct LoadReport {
    pub session_id: String,
    pub source_id: String,
    pub title: Option<String>,
    pub transcript: PathBuf,
    pub source_cwd: Option<String>,
    pub lines: usize,
}

// ---------------------------------------------------------------------------
// dump
// ---------------------------------------------------------------------------

/// Pack conversation `key` (its id, or the name `/resume` lists it under) into
/// a zip at `out`, or at `<cwd>/<name>.zip` when `out` is `None`.
pub fn dump_conversation(
    claude_dir: &Path,
    cwd: &Path,
    key: &str,
    out: Option<&Path>,
) -> Result<DumpReport> {
    let (id, transcript) = resolve_conversation(claude_dir, cwd, key)?;
    let folder = transcript.parent().context("transcript has no parent folder")?;
    let title = transcript_meta(&transcript).and_then(|(title, _)| title);
    let source_cwd = project_root_of(&transcript, folder);

    let archive = match out {
        Some(p) => p.to_path_buf(),
        None => cwd.join(format!("{}.zip", file_stem_for(title.as_deref(), &id))),
    };
    // Never overwrite: the file may be someone's earlier dump.
    let file = create_new_file(&archive)?;
    let result = (|| -> Result<(usize, u64)> {
        let mut zip = zip::ZipWriter::new(BufWriter::new(file));
        let manifest = Manifest {
            format: FORMAT.to_owned(),
            version: VERSION,
            session_id: id.clone(),
            title: title.clone(),
            source_cwd: source_cwd.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            claudebase_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        zip.start_file(MANIFEST, options(0))?;
        zip.write_all(&serde_json::to_vec_pretty(&manifest)?)?;

        let (mut files, mut bytes) = (1, 0);
        // The transcript goes through the same line filter as everything
        // else that reads one, so a torn last line — the conversation being
        // written right now — stays out of the archive.
        let size = fs::metadata(&transcript)?.len();
        zip.start_file(TRANSCRIPT, options(size))?;
        let input = BufReader::new(
            fs::File::open(&transcript)
                .with_context(|| format!("open {}", transcript.display()))?,
        );
        let keep_all = Rewrite { source_id: &id, new_id: &id, drop_types: &[], cwd: None };
        rewrite_lines(input, &mut zip, &keep_all)?;
        files += 1;
        bytes += size;

        for (dir, prefix) in [
            (folder.join(&id), SESSION_DIR),
            (claude_dir.join("file-history").join(&id), FILE_HISTORY_DIR),
        ] {
            if dir.is_dir() {
                let (f, b) = add_dir(&mut zip, &dir, prefix)?;
                files += f;
                bytes += b;
            }
        }
        zip.finish()?.flush()?;
        Ok((files, bytes))
    })();
    match result {
        Ok((files, bytes_in)) => Ok(DumpReport { session_id: id, title, archive, files, bytes_in }),
        Err(e) => {
            let _ = fs::remove_file(&archive);
            Err(e)
        }
    }
}

fn options(size: u64) -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600)
        // ZIP64 only where a member could need it; everything else stays
        // readable by the oldest unzip.
        .large_file(size >= 0xFFFF_0000)
}

/// Add a folder's regular files under `prefix/`. Symlinks are skipped.
fn add_dir<W: Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir: &Path,
    prefix: &str,
) -> Result<(usize, u64)> {
    let (mut files, mut bytes) = (0, 0);
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let kind = entry.file_type()?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            bail!("non-UTF-8 file name under {}", dir.display());
        };
        let path = format!("{prefix}/{name}");
        if kind.is_dir() {
            let (f, b) = add_dir(zip, &entry.path(), &path)?;
            files += f;
            bytes += b;
        } else if kind.is_file() {
            let size = entry.metadata()?.len();
            zip.start_file(path, options(size))?;
            std::io::copy(&mut fs::File::open(entry.path())?, zip)?;
            files += 1;
            bytes += size;
        }
    }
    Ok((files, bytes))
}

/// The project directory a transcript belongs to: the first `cwd` whose slug
/// is the transcript's folder name (the directory Claude Code filed it
/// under), else the first `cwd` at all.
fn project_root_of(transcript: &Path, folder: &Path) -> Option<String> {
    let slug = folder.file_name()?.to_str()?;
    let input = BufReader::new(fs::File::open(transcript).ok()?);
    let mut first = None;
    for line in std::io::BufRead::split(input, b'\n').take(10_000) {
        let Ok(line) = line else { break };
        // `cwd` sits on message lines, which can be long; but only the field is
        // needed, and every one of the first few thousand lines carries it.
        let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
        let Some(cwd) = v.get("cwd").and_then(Value::as_str) else { continue };
        if project_slug(Path::new(cwd)) == slug {
            return Some(cwd.to_owned());
        }
        first.get_or_insert_with(|| cwd.to_owned());
    }
    first
}

/// A file name for the archive: the conversation's name made safe for every
/// filesystem, else its id.
fn file_stem_for(title: Option<&str>, id: &str) -> String {
    let safe: String = title
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    let safe = safe.trim_matches(['-', '.']).chars().take(80).collect::<String>();
    if safe.is_empty() {
        id.to_owned()
    } else {
        format!("{safe}-{}", &id[..8.min(id.len())])
    }
}

// ---------------------------------------------------------------------------
// load
// ---------------------------------------------------------------------------

/// Unpack a dump into the working directory's project folder.
pub fn load_conversation(
    claude_dir: &Path,
    cwd: &Path,
    archive_path: &Path,
    new_id: bool,
    name: Option<&str>,
) -> Result<LoadReport> {
    let name = name.map(check_name).transpose()?;
    let file = fs::File::open(archive_path)
        .with_context(|| format!("open {}", archive_path.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("{} is not a zip archive", archive_path.display()))?;

    let manifest: Manifest = {
        let mut entry = zip
            .by_name(MANIFEST)
            .context("not a claudebase conversation dump (no manifest.json)")?;
        let mut buf = Vec::new();
        entry.by_ref().take(1 << 20).read_to_end(&mut buf)?;
        serde_json::from_slice(&buf).context("manifest.json is not valid")?
    };
    if manifest.format != FORMAT {
        bail!("not a claudebase conversation dump (format `{}`)", manifest.format);
    }
    if manifest.version > VERSION {
        bail!(
            "this dump is format version {}, made by claudebase {}; this claudebase reads up to \
             version {VERSION} — update it first",
            manifest.version,
            manifest.claudebase_version
        );
    }
    let source_id = manifest.session_id.clone();
    uuid::Uuid::parse_str(&source_id).context("manifest.json names no valid conversation id")?;
    let id = if new_id { uuid::Uuid::new_v4().to_string() } else { source_id.clone() };

    let folder = claude_dir.join("projects").join(project_slug(cwd));
    let history_root = claude_dir.join("file-history");
    let transcript = folder.join(format!("{id}.jsonl"));
    let session_dir = folder.join(&id);
    let history_dir = history_root.join(&id);
    // The id is global to the machine, not to a project: the same id filed
    // under two directories makes `claude --resume <id>` ambiguous.
    let elsewhere = fs::read_dir(claude_dir.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join(format!("{id}.jsonl")))
        .filter(|p| p.is_file());
    for existing in [transcript.clone(), session_dir.clone(), history_dir.clone()]
        .into_iter()
        .chain(elsewhere)
    {
        if existing.exists() {
            bail!(
                "conversation {id} is already on this machine ({}); pass --new-id to load it as a \
                 separate copy",
                existing.display()
            );
        }
    }
    fs::create_dir_all(&folder).with_context(|| format!("create {}", folder.display()))?;
    fs::create_dir_all(&history_root)?;

    // Staging folders sit next to their destinations so the final step is a
    // rename on one filesystem, never a copy.
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let stage_session = folder.join(format!(".claudebase-load-{tag}"));
    let stage_history = history_root.join(format!(".claudebase-load-{tag}"));
    let stage_transcript = folder.join(format!(".claudebase-load-{tag}.jsonl"));

    let cwd_str = cwd.to_string_lossy().into_owned();
    let rw = Rewrite {
        source_id: &source_id,
        new_id: &id,
        drop_types: DROPPED_ON_LOAD,
        cwd: manifest.source_cwd.as_deref().map(|from| (from, cwd_str.as_str())),
    };

    let result = (|| -> Result<usize> {
        let mut lines = None;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            if entry.is_dir() {
                continue;
            }
            let Some(rel) = entry.enclosed_name() else {
                bail!("unsafe path in archive: {:?}", entry.name());
            };
            let mut parts = rel.components().map(|c| match c {
                Component::Normal(p) => p.to_str().map(str::to_owned),
                _ => None,
            });
            let top = parts.next().flatten();
            let rest: Option<Vec<String>> = parts.collect();
            let Some(rest) = rest else {
                bail!("unsafe path in archive: {}", rel.display());
            };
            let dest_root = match top.as_deref() {
                Some(MANIFEST) if rest.is_empty() => continue,
                Some(TRANSCRIPT) if rest.is_empty() => None,
                Some(SESSION_DIR) if !rest.is_empty() => Some(&stage_session),
                Some(FILE_HISTORY_DIR) if !rest.is_empty() => Some(&stage_history),
                _ => bail!("unexpected entry in archive: {}", rel.display()),
            };
            if !entry.is_file() {
                bail!("archive entry {} is not a regular file", rel.display());
            }
            let Some(root) = dest_root else {
                let mut out = BufWriter::new(create_new_file(&stage_transcript)?);
                let (copied, _) = rewrite_lines(BufReader::new(&mut entry), &mut out, &rw)?;
                if let Some(name) = name {
                    append_title(&mut out, &id, name)?;
                }
                finish_file(out)?;
                lines = Some(copied);
                continue;
            };
            let dest = rest.iter().fold(root.clone(), |p, part| p.join(part));
            fs::create_dir_all(dest.parent().context("archive entry has no parent")?)?;
            let mut out = BufWriter::new(create_new_file(&dest)?);
            // Subagent transcripts name the conversation too; backups and spilled
            // tool output are opaque bytes.
            if top.as_deref() == Some(SESSION_DIR) && dest.extension().is_some_and(|e| e == "jsonl") {
                rewrite_lines(BufReader::new(&mut entry), &mut out, &rw)?;
            } else {
                std::io::copy(&mut entry, &mut out)?;
            }
            finish_file(out)?;
        }
        let lines = lines.context("archive has no transcript.jsonl")?;

        if stage_session.is_dir() {
            fs::rename(&stage_session, &session_dir)?;
        }
        if stage_history.is_dir() {
            fs::rename(&stage_history, &history_dir)?;
        }
        // Last: this is what makes the conversation appear in /resume.
        fs::rename(&stage_transcript, &transcript)?;
        Ok(lines)
    })();

    match result {
        Ok(lines) => Ok(LoadReport {
            session_id: id,
            source_id,
            title: name.map(str::to_owned).or(manifest.title),
            transcript,
            source_cwd: manifest.source_cwd,
            lines,
        }),
        Err(e) => {
            for dir in [&stage_session, &stage_history] {
                let _ = fs::remove_dir_all(dir);
            }
            let _ = fs::remove_file(&stage_transcript);
            // Renamed into place before the failure — only ever by this call,
            // since an existing destination was refused above.
            if !transcript.exists() {
                let _ = fs::remove_dir_all(&session_dir);
                let _ = fs::remove_dir_all(&history_dir);
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_name_is_safe_and_falls_back_to_the_id() {
        let id = "88d0e3e5-519e-44e8-8ad6-833a259371eb";
        assert_eq!(file_stem_for(Some("socks5 / proxy: v2"), id), "socks5---proxy--v2-88d0e3e5");
        assert_eq!(file_stem_for(Some("../.."), id), id);
        assert_eq!(file_stem_for(None, id), id);
    }
}
