//! e5-multilingual-small encoder (Slice 5 of vector-retrieval-backend).
//!
//! Wraps fastembed-rs's `TextEmbedding` with the e5 prefix-discipline
//! contract: passages are prepended `"passage: "`, queries `"query: "`. The
//! model card at https://huggingface.co/intfloat/multilingual-e5-small
//! mandates this prefix; forgetting it silently degrades retrieval 5–10%
//! (Risk R7 in the plan). fastembed v5 does NOT auto-prepend (verified via
//! the crate README example showing manual `"passage: ..."` prefixes), so
//! THIS wrapper is the canonical place that adds them.
//!
//! The encoder is a process-wide singleton loaded lazily on first use —
//! same Mutex<Option<T>> pattern as `crate::store::SQLITE_VEC_INIT` and
//! `crate::pdf::PDFIUM`. Cache dir pinned to
//! `~/.claude/tools/claudebase/models/e5-small/` so install.sh
//! (Slice 11) can pre-populate the model files alongside pdfium.
//!
//! Degraded-mode contract: if the model cannot be loaded (offline + no
//! cached files; corrupt model; ONNX runtime missing), the public API
//! returns `EncoderError::Load`. Callers (Slice 6 OCR, Slice 7 hybrid
//! search, ingest pipeline) catch and degrade to BM25-only behavior with
//! a logged warning.

use std::path::PathBuf;
use std::sync::Mutex;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EncoderError {
    #[error("encoder model load failed: {0}")]
    Load(String),
    #[error("encode failed: {0}")]
    Encode(String),
}

/// Process-wide singleton. fastembed's `TextEmbedding` is `Send`/`Sync` so
/// holding it behind a `Mutex` for serialized access is safe; our CLI
/// invocations are sequential anyway.
static ENCODER: Mutex<Option<TextEmbedding>> = Mutex::new(None);

/// Resolve the model cache directory: `~/.claude/tools/claudebase/models`.
/// Honors `HOME` (Unix) and `USERPROFILE` (Windows fallback) — same pattern
/// as `crate::pdf::resolve_pdfium_lib_dir` (security-auditor HIGH #1).
fn model_cache_dir() -> Result<PathBuf, EncoderError> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            EncoderError::Load(
                "HOME (Unix) / USERPROFILE (Windows) env var unset; cannot resolve encoder model cache"
                    .to_string(),
            )
        })?;
    if home.is_empty() {
        return Err(EncoderError::Load(
            "HOME / USERPROFILE env var empty".to_string(),
        ));
    }
    Ok(PathBuf::from(home).join(".claude/tools/claudebase/models"))
}

/// Whether the model files are already on disk. Loading the encoder without
/// them makes fastembed download ~450 MB, which a long-running process (the
/// daemon) must never start on its own: the installers fetch the model with
/// `claudebase warmup`, and until then callers stay lexical.
pub fn model_cached() -> bool {
    let Ok(dir) = model_cache_dir() else { return false };
    std::fs::read_dir(dir.join(MODEL_DIR).join("snapshots"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

/// fastembed's cache folder for the model.
const MODEL_DIR: &str = "models--intfloat--multilingual-e5-small";
/// What the download comes to (the ONNX graph, its data, the tokenizer).
const MODEL_BYTES: u64 = 487_000_000;

/// Bytes under `dir`, following nothing: the download lands in `blobs/`.
fn dir_bytes(dir: &std::path::Path) -> u64 {
    let Ok(read) = std::fs::read_dir(dir) else { return 0 };
    read.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Progress of the model download, on stderr, until `done` is set. A ~490 MB
/// download that prints nothing looks hung; fastembed's own bar is drawn only
/// on a terminal, so an installer run over plain ssh or by an agent showed a
/// silent "Pre-loading ..." for minutes. On a terminal: one line rewritten in
/// place. Otherwise: a line every 10%, and every 30 s while it does not move,
/// so a stalled download reads as stalled rather than as silence.
fn report_download(dir: std::path::PathBuf, done: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::io::{IsTerminal, Write};
    use std::sync::atomic::Ordering;
    let tty = std::io::stderr().is_terminal();
    let mut last_step = u64::MAX;
    let mut last_line = std::time::Instant::now();
    let mut shown = false;
    while !done.load(Ordering::Relaxed) {
        let got = dir_bytes(&dir);
        let pct = (got * 100 / MODEL_BYTES).min(99);
        let line = format!("encoder model: {} / ~{} MB ({pct}%)", got / 1_000_000, MODEL_BYTES / 1_000_000);
        if tty {
            eprint!("\r  {line}   ");
            let _ = std::io::stderr().flush();
            shown = true;
        } else if pct / 10 != last_step || last_line.elapsed() >= std::time::Duration::from_secs(30) {
            last_step = pct / 10;
            last_line = std::time::Instant::now();
            eprintln!("  {line}");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if tty && shown {
        eprintln!("\r  encoder model: downloaded                         ");
    }
}

/// Compose the e5 passage-prefixed input. **Critical**: exactly ONE
/// `"passage: "` prefix per call. Plan Risk R7 + architect AI-4: the
/// encoder MUST add the prefix; double-prefix silently degrades quality.
pub fn prefix_passage(text: &str) -> String {
    format!("passage: {text}")
}

/// Compose the e5 query-prefixed input. Symmetric to [`prefix_passage`].
pub fn prefix_query(text: &str) -> String {
    format!("query: {text}")
}

/// Lazy-load the singleton. Idempotent across calls; only the first call
/// triggers model load. Subsequent calls return immediately on cache-hit.
fn ensure_loaded() -> Result<(), EncoderError> {
    let mut guard = ENCODER
        .lock()
        .map_err(|_| EncoderError::Load("encoder mutex poisoned".to_string()))?;
    if guard.is_some() {
        return Ok(());
    }
    let cache_dir = model_cache_dir()?;
    // Our own progress instead of fastembed's (see `report_download`).
    let progress = (!model_cached()).then(|| {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (dir, flag) = (cache_dir.join(MODEL_DIR), done.clone());
        (done, std::thread::spawn(move || report_download(dir, flag)))
    });
    let opts = InitOptions::new(EmbeddingModel::MultilingualE5Small)
        .with_cache_dir(cache_dir)
        .with_show_download_progress(false);
    let model = TextEmbedding::try_new(opts).map_err(|e| EncoderError::Load(format!("{e}")));
    if let Some((done, reporter)) = progress {
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = reporter.join();
    }
    *guard = Some(model?);
    Ok(())
}

/// Encode passages (chunk text) into 384-dim f32 embeddings. Each input
/// `&str` is internally prepended with `"passage: "` per the e5 contract.
/// Returns one `Vec<f32>` per input, in the same order.
pub fn encode_passages(passages: &[&str]) -> Result<Vec<Vec<f32>>, EncoderError> {
    ensure_loaded()?;
    let prefixed: Vec<String> = passages.iter().map(|p| prefix_passage(p)).collect();
    let mut guard = ENCODER
        .lock()
        .map_err(|_| EncoderError::Encode("encoder mutex poisoned".to_string()))?;
    let model = guard
        .as_mut()
        .expect("encoder loaded above by ensure_loaded");
    model
        .embed(&prefixed, Some(32))
        .map_err(|e| EncoderError::Encode(format!("{e}")))
}

/// Encode a single query into a 384-dim f32 embedding. The input is
/// internally prepended with `"query: "` per the e5 contract.
pub fn encode_query(query: &str) -> Result<Vec<f32>, EncoderError> {
    ensure_loaded()?;
    let prefixed = prefix_query(query);
    let mut guard = ENCODER
        .lock()
        .map_err(|_| EncoderError::Encode("encoder mutex poisoned".to_string()))?;
    let model = guard
        .as_mut()
        .expect("encoder loaded above by ensure_loaded");
    let mut out = model
        .embed(&[prefixed], Some(1))
        .map_err(|e| EncoderError::Encode(format!("{e}")))?;
    out.pop()
        .ok_or_else(|| EncoderError::Encode("empty embedding result".to_string()))
}
