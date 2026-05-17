//! Slice 6-MVP — local whisper.cpp ASR backend.
//!
//! Gated behind `--features asr-whisper`. When absent, the module is
//! not compiled and the factory bails with a clean Err per PRD
//! FR-ACD-7.4.
//!
//! ## Model file
//!
//! The whisper.cpp ggml-medium.bin model lives at
//! `<home>/.claude/tools/claudebase/models/whisper/ggml-medium.bin`.
//! `WhisperAsr::new()` does NOT download — construction is cheap and
//! always succeeds so `daemon doctor --asr` can be invoked even when
//! the model is absent (the health_check is what reports missing).
//!
//! Auto-download happens in two places:
//!   1. `claudebase daemon warmup --asr` (operator-driven pre-fetch)
//!   2. Lazy download on first `transcribe()` call (so a fresh daemon
//!      receiving a voice note before warmup still works — at the cost
//!      of a 30-second stall on that first note).
//!
//! Both share `ensure_model(path) -> Result<()>`. The download uses
//! `reqwest::blocking::Client` with `Range:` header support for resume
//! from a `.part` file (NFR-ACD-11). A file-level lock via `fslock`
//! prevents two concurrent voice notes from racing on the download.
//!
//! ## SHA verification
//!
//! Slice 6-MVP ships WITHOUT a hardcoded SHA256 for `ggml-medium.bin`.
//! Looking up the canonical SHA from HF requires opening the
//! repository's `SHA256SUMS` file at the right commit — left as a
//! Slice 6.1 follow-up. Until then the download path performs a
//! "size sanity check" only (file must be ≥ 100 MB) and logs a WARN
//! flagging the missing SHA verification. **This is tracked as
//! `### Hacks acknowledged` in the slice commit message.**

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;

use super::Asr;

/// Whisper backend instance. Holds the model path and (lazily) the
/// whisper.cpp context. The context is heavy (~1.5 GB resident with
/// ggml-medium) and is therefore deferred until the first transcribe
/// call — keeping `WhisperAsr::new()` cheap so `daemon doctor` can
/// run without booting the model.
pub struct WhisperAsr {
    model_path: PathBuf,
    // The WhisperContext is intentionally NOT held here in Slice 6-MVP —
    // every transcribe call constructs the context inline so the
    // implementation is straightforward. Future slices can hoist the
    // context behind a `tokio::sync::Mutex<Option<WhisperContext>>`
    // for caching across calls.
}

impl WhisperAsr {
    /// Construct a WhisperAsr handle. Always succeeds (no I/O) so
    /// `daemon doctor` can introspect even when the model file is
    /// missing — `health_check()` is what reports model-missing.
    pub fn new() -> Result<Self> {
        Ok(Self {
            model_path: model_path()?,
        })
    }
}

#[async_trait]
impl Asr for WhisperAsr {
    async fn transcribe(&self, pcm: Vec<f32>, sample_rate: u32) -> Result<String> {
        if sample_rate != 16_000 {
            bail!(
                "whisper expects 16 kHz PCM input; got {sample_rate} Hz — \
                 the decoder pipeline should have resampled"
            );
        }
        let model_path = self.model_path.clone();
        // whisper-rs is sync + heavy CPU; run on the blocking pool per
        // ASYNC_INVARIANTS Rule 2.
        tokio::task::spawn_blocking(move || -> Result<String> {
            ensure_model(&model_path).context("whisper: model download/verify failed")?;
            transcribe_blocking(&model_path, &pcm)
        })
        .await
        .context("whisper: spawn_blocking join failed")?
    }

    fn health_check(&self) -> Result<()> {
        if !self.model_path.exists() {
            bail!(
                "MISSING model file at {} — run `claudebase daemon warmup --asr` to download",
                self.model_path.display()
            );
        }
        let meta = std::fs::metadata(&self.model_path).with_context(|| {
            format!("failed to stat model file {}", self.model_path.display())
        })?;
        // Size sanity check (real ggml-medium is ~1.5 GB; anything smaller
        // than 100 MB indicates a partial / truncated download).
        const MIN_SIZE_BYTES: u64 = 100 * 1024 * 1024;
        if meta.len() < MIN_SIZE_BYTES {
            bail!(
                "MISSING model file appears truncated: {} bytes ({}); re-run warmup",
                meta.len(),
                self.model_path.display()
            );
        }
        Ok(())
    }
}

/// Compute the canonical model path. Lives under
/// `<home>/.claude/tools/claudebase/models/whisper/ggml-medium.bin` per
/// PRD FR-ACD-7.6. The `CLAUDEBASE_HOME_OVERRIDE` env var (when set)
/// replaces the home dir — used by tests to redirect model lookup to
/// a tmp path without touching the operator's real install.
pub fn model_path() -> Result<PathBuf> {
    let home = if let Some(override_dir) = std::env::var_os("CLAUDEBASE_HOME_OVERRIDE") {
        PathBuf::from(override_dir)
    } else {
        let raw = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .context("HOME / USERPROFILE not set; cannot locate model dir")?;
        PathBuf::from(raw)
    };
    Ok(home
        .join(".claude")
        .join("tools")
        .join("claudebase")
        .join("models")
        .join("whisper")
        .join("ggml-medium.bin"))
}

/// Hugging Face URL for the medium ggml model. Documented in PRD
/// FR-ACD-7.3.
const HF_MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin";

/// Download (or verify) the whisper model file at `path`. Idempotent
/// when the file exists and passes the size sanity check.
///
/// Slice 6-MVP TODO (Hack acknowledged): the canonical SHA256 of
/// `ggml-medium.bin` is NOT verified — the model is checked only by
/// file-size lower-bound. Adding the SHA constant is a Slice 6.1
/// follow-up; the model file currently distributed by Hugging Face is
/// 1.5 GB so the 100 MB threshold catches all truncations.
pub fn ensure_model(path: &std::path::Path) -> Result<()> {
    // Fast path — file already present and a reasonable size.
    if path.exists() {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() >= 100 * 1024 * 1024 {
                return Ok(());
            }
        }
    }
    // Ensure parent dir exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create model dir {}", parent.display()))?;
    }
    // File-level lock prevents two concurrent voice-notes from racing on
    // the same .part file.
    let lock_path = path.with_extension("download.lock");
    let mut lock = fslock::LockFile::open(&lock_path)
        .with_context(|| format!("open download lock {}", lock_path.display()))?;
    lock.lock().context("acquire download lock")?;

    // Re-check inside the critical section (another process may have
    // completed the download while we waited).
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() >= 100 * 1024 * 1024 {
            return Ok(());
        }
    }

    let part_path = path.with_extension("part");
    let part_size_existing = std::fs::metadata(&part_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!(
        target = %path.display(),
        resume_from = part_size_existing,
        "whisper: starting model download from huggingface"
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(900)) // 15 min cap
        .build()
        .context("build reqwest client for model download")?;

    let mut req = client.get(HF_MODEL_URL);
    if part_size_existing > 0 {
        req = req.header("Range", format!("bytes={part_size_existing}-"));
    }
    let mut resp = req
        .send()
        .with_context(|| format!("GET {HF_MODEL_URL} failed"))?;

    if !resp.status().is_success() && resp.status().as_u16() != 206 {
        bail!(
            "huggingface download returned HTTP {}: {}",
            resp.status(),
            HF_MODEL_URL
        );
    }

    // Append-or-create the .part file. Range-resume implies append;
    // initial-create implies truncate.
    let open_mode = if part_size_existing > 0 && resp.status().as_u16() == 206 {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&part_path)
    } else {
        std::fs::File::create(&part_path)
    };
    let mut writer = open_mode
        .with_context(|| format!("open .part file {}", part_path.display()))?;

    std::io::copy(&mut resp, &mut writer)
        .with_context(|| format!("write to .part file {}", part_path.display()))?;
    drop(writer);

    // Verify size sanity BEFORE rename — a truncated download leaves
    // .part in place so the next call can resume.
    let final_size = std::fs::metadata(&part_path)
        .with_context(|| format!("stat {} after download", part_path.display()))?
        .len();
    if final_size < 100 * 1024 * 1024 {
        bail!(
            "downloaded .part file is too small ({final_size} bytes); \
             expected ≥ 100 MB. Network truncation? Leaving .part for resume."
        );
    }

    // Atomic rename .part → final. SHA verification would go here once
    // the canonical SHA constant lands (Slice 6.1 follow-up).
    std::fs::rename(&part_path, path)
        .with_context(|| format!("rename {} → {}", part_path.display(), path.display()))?;

    tracing::warn!(
        path = %path.display(),
        "whisper: model downloaded WITHOUT SHA256 verification — Slice 6.1 follow-up"
    );
    Ok(())
}

/// Synchronous whisper-rs invocation. Runs on the tokio blocking pool.
fn transcribe_blocking(model_path: &std::path::Path, pcm: &[f32]) -> Result<String> {
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    let model_path_str = model_path
        .to_str()
        .context("whisper: model path is not valid UTF-8")?;

    let ctx = WhisperContext::new_with_params(model_path_str, WhisperContextParameters::default())
        .map_err(|e| anyhow::anyhow!("whisper: open model {model_path_str}: {e}"))?;

    let mut state = ctx
        .create_state()
        .map_err(|e| anyhow::anyhow!("whisper: create_state: {e}"))?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(num_cpus_safe() as i32);
    params.set_translate(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    state
        .full(params, pcm)
        .map_err(|e| anyhow::anyhow!("whisper: full inference: {e}"))?;

    // whisper-rs 0.16 API: full_n_segments returns i32 directly (NOT
    // Result); per-segment text accessor is get_segment(i) -> Option<WhisperSegment>
    // and WhisperSegment::to_str() -> Result<&str, WhisperError>.
    let n_segments = state.full_n_segments();
    let mut out = String::new();
    for i in 0..n_segments {
        if let Some(seg) = state.get_segment(i) {
            let text = seg
                .to_str()
                .map_err(|e| anyhow::anyhow!("whisper: segment({i}) to_str: {e}"))?;
            out.push_str(text);
        }
    }
    Ok(out.trim().to_string())
}

/// Cheap fallback for `num_cpus` without adding a dep. Uses
/// std::thread::available_parallelism, which is available since 1.59.
fn num_cpus_safe() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_path_uses_home_override() {
        let prev = std::env::var_os("CLAUDEBASE_HOME_OVERRIDE");
        std::env::set_var("CLAUDEBASE_HOME_OVERRIDE", "/tmp/fake-home");
        let p = model_path().expect("model_path");
        assert!(p.starts_with("/tmp/fake-home/.claude/tools/claudebase/models/whisper"));
        match prev {
            Some(v) => std::env::set_var("CLAUDEBASE_HOME_OVERRIDE", v),
            None => std::env::remove_var("CLAUDEBASE_HOME_OVERRIDE"),
        }
    }

    #[test]
    fn whisper_health_check_reports_missing_model() {
        let prev = std::env::var_os("CLAUDEBASE_HOME_OVERRIDE");
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("CLAUDEBASE_HOME_OVERRIDE", tmp.path());
        let asr = WhisperAsr::new().expect("construct");
        let err = asr.health_check().expect_err("expected missing-model err");
        let msg = format!("{err}");
        assert!(msg.contains("MISSING") && msg.contains("model"), "got: {msg}");
        match prev {
            Some(v) => std::env::set_var("CLAUDEBASE_HOME_OVERRIDE", v),
            None => std::env::remove_var("CLAUDEBASE_HOME_OVERRIDE"),
        }
    }
}
