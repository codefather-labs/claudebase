//! NVIDIA Parakeet ASR backend, run locally through sherpa-onnx.
//!
//! Gated behind `--features asr-sherpa`. Selected with `[asr] backend =
//! "parakeet"` in daemon.toml; whisper remains the default until this one has
//! been measured on real Russian audio (see
//! `docs/plans/claudebase-v0.10-parakeet-asr.md`, Slice 0).
//!
//! ## The model, exactly
//!
//! **`nvidia/parakeet-tdt-0.6b-v3`** — the MULTILINGUAL one. This matters more
//! than a version number usually does: `parakeet-tdt-0.6b-v2` is
//! English-only, and feeding it Russian does not fail, it silently returns
//! English-shaped nonsense. That is the exact bug v0.8.1 fixed for whisper by
//! setting `language = "auto"`, and issue 007 parked this whole backend on the
//! risk of re-introducing it. v3 covers 25 European languages including Russian
//! and detects the language itself, with no prompting.
//!
//! We consume the sherpa-onnx conversion of it, which ships as one archive:
//!
//! ```text
//! sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2   (~640 MB)
//!   encoder.int8.onnx   622 MB
//!   decoder.int8.onnx    12 MB
//!   joiner.int8.onnx    6.1 MB
//!   tokens.txt           92 KB
//! ```
//!
//! ## Why sherpa-onnx rather than NeMo
//!
//! NeMo is PyTorch and Python. This daemon is one Rust binary, and the point of
//! transcribing locally is that nothing else has to be installed to do it.
//! sherpa-onnx runs the ONNX export through ONNX Runtime on CPU and exposes a C
//! API the `sherpa-onnx` crate wraps. NVIDIA's hosted NIM endpoint would have
//! been less work still, and would have sent the operator's dictation to
//! someone else's machine — voice notes are the most private thing this system
//! carries.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;

use super::Asr;

/// The one archive that carries every file this backend needs.
///
/// Pinned to the v3 (multilingual) build on purpose — see the module docs for
/// what picking v2 by accident would do to Russian.
const MODEL_ARCHIVE_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2";

/// The directory the archive expands into, inside our model directory.
const MODEL_ARCHIVE_ROOT: &str = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8";

/// Anything smaller than this is a truncated download, not a model.
const MIN_ENCODER_BYTES: u64 = 100 * 1024 * 1024;

/// Parakeet backend handle.
///
/// Construction does no I/O, so `daemon doctor --asr` can introspect a machine
/// where the model has never been fetched — `health_check` is what reports
/// that. The recognizer is built on first use and kept, for the same reason
/// whisper's context is: re-reading hundreds of megabytes per voice note is
/// pure waste, and it evicts everyone else's page cache on the way through.
pub struct ParakeetAsr {
    model_dir: PathBuf,
    n_threads: usize,
    /// Held across the whole inference, deliberately. Two notes decoded at once
    /// on a busy machine finish later than the same two run back to back — the
    /// same effect measured for whisper's thread count, for the same reason.
    recognizer: Arc<Mutex<Option<Arc<sherpa_onnx::OfflineRecognizer>>>>,
}

impl ParakeetAsr {
    pub fn new(n_threads: Option<usize>) -> Result<Self> {
        Ok(Self {
            model_dir: model_dir()?,
            n_threads: resolve_threads(n_threads),
            recognizer: Default::default(),
        })
    }

    fn encoder(&self) -> PathBuf {
        self.model_dir.join("encoder.int8.onnx")
    }
    fn decoder(&self) -> PathBuf {
        self.model_dir.join("decoder.int8.onnx")
    }
    fn joiner(&self) -> PathBuf {
        self.model_dir.join("joiner.int8.onnx")
    }
    fn tokens(&self) -> PathBuf {
        self.model_dir.join("tokens.txt")
    }

    fn files(&self) -> [PathBuf; 4] {
        [self.encoder(), self.decoder(), self.joiner(), self.tokens()]
    }
}

/// Threads for ONNX Runtime, and not every core.
///
/// Same default and same reasoning as the whisper backend: measured on this
/// project's 16-core machine, giving whisper every core made a 4-second note
/// 2.8x SLOWER than giving it four, because the workers join at a barrier and
/// one descheduled worker holds all the others. ONNX Runtime parallelises the
/// same way. `[asr] n_threads` raises it where the cores are genuinely free.
fn resolve_threads(configured: Option<usize>) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    match configured {
        Some(n) if n > 0 => n.min(cores.max(1)),
        _ => 4.min(cores.max(1)),
    }
}

/// `<home>/.claude/tools/claudebase/models/parakeet/`, beside the whisper model.
pub fn model_dir() -> Result<PathBuf> {
    // Same resolution as the whisper backend, deliberately: two ways of
    // deciding where models live is how a documented path ends up wrong.
    let home = if let Some(override_dir) = std::env::var_os("CLAUDEBASE_HOME_OVERRIDE") {
        PathBuf::from(override_dir)
    } else {
        let raw = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .context("cannot resolve home directory (HOME / USERPROFILE unset)")?;
        PathBuf::from(raw)
    };
    Ok(home
        .join(".claude")
        .join("tools")
        .join("claudebase")
        .join("models")
        .join("parakeet"))
}

#[async_trait]
impl Asr for ParakeetAsr {
    async fn transcribe(&self, pcm: Vec<f32>, sample_rate: u32) -> Result<String> {
        if sample_rate != 16_000 {
            bail!(
                "parakeet expects 16 kHz PCM input; got {sample_rate} Hz — \
                 the decoder pipeline should have resampled"
            );
        }
        let files = self.files();
        let dir = self.model_dir.clone();
        let n_threads = self.n_threads;
        let cache = self.recognizer.clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            ensure_model(&dir, &files).context("parakeet: model download/verify failed")?;
            transcribe_blocking(&files, n_threads, &cache, &pcm)
        })
        .await
        .context("parakeet: spawn_blocking join failed")?
    }

    fn name(&self) -> &'static str {
        "parakeet"
    }

    fn warmup(&self) -> Result<()> {
        ensure_model(&self.model_dir, &self.files())
    }

    fn health_check(&self) -> Result<()> {
        for f in self.files() {
            if !f.exists() {
                bail!(
                    "MISSING model file at {} — run `claudebase daemon warmup --asr` to download",
                    f.display()
                );
            }
        }
        let enc = std::fs::metadata(self.encoder())
            .with_context(|| format!("failed to stat {}", self.encoder().display()))?;
        if enc.len() < MIN_ENCODER_BYTES {
            bail!(
                "MISSING model appears truncated: encoder is {} bytes at {}; re-run warmup",
                enc.len(),
                self.encoder().display()
            );
        }
        Ok(())
    }
}

fn transcribe_blocking(
    files: &[PathBuf; 4],
    n_threads: usize,
    cache: &Mutex<Option<Arc<sherpa_onnx::OfflineRecognizer>>>,
    pcm: &[f32],
) -> Result<String> {
    use sherpa_onnx::{
        OfflineModelConfig, OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig,
    };

    let mut guard = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("parakeet: recognizer cache poisoned by an earlier panic"))?;

    let recognizer = match guard.as_ref() {
        Some(r) => r.clone(),
        None => {
            let path = |p: &PathBuf| -> Result<String> {
                p.to_str()
                    .map(|s| s.to_string())
                    .with_context(|| format!("parakeet: path is not valid UTF-8: {}", p.display()))
            };
            let config = OfflineRecognizerConfig {
                model_config: OfflineModelConfig {
                    transducer: OfflineTransducerModelConfig {
                        encoder: Some(path(&files[0])?),
                        decoder: Some(path(&files[1])?),
                        joiner: Some(path(&files[2])?),
                    },
                    tokens: Some(path(&files[3])?),
                    num_threads: n_threads as i32,
                    debug: false,
                    ..Default::default()
                },
                ..Default::default()
            };
            // `create` returns None rather than an error, so the message has to
            // be built here — and it is the message someone reads at 2am when a
            // voice note stops arriving.
            let built = OfflineRecognizer::create(&config).ok_or_else(|| {
                anyhow::anyhow!(
                    "parakeet: sherpa-onnx refused to build a recognizer from {}. \
                     The four model files are present but one may be truncated or from a \
                     different export; re-run `claudebase daemon warmup --asr`.",
                    files[0].parent().map(Path::to_path_buf).unwrap_or_default().display()
                )
            })?;
            let built = Arc::new(built);
            *guard = Some(built.clone());
            built
        }
    };

    let stream = recognizer.create_stream();
    stream.accept_waveform(16_000, pcm);
    recognizer.decode(&stream);
    let text = stream
        .get_result()
        .map(|r| r.text)
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok(text)
}

/// Fetch and unpack the model archive, unless it is already unpacked.
///
/// One archive, four files, and a lock so two voice notes arriving together
/// cannot both start a 640 MB download — the same shape as the whisper
/// backend's fetch, which learned that lesson first.
fn ensure_model(dir: &Path, files: &[PathBuf; 4]) -> Result<()> {
    if files.iter().all(|f| f.exists()) {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    let lock_path = dir.join("parakeet.download.lock");
    let mut lock = fslock::LockFile::open(&lock_path)
        .with_context(|| format!("open download lock {}", lock_path.display()))?;
    lock.lock().context("acquire parakeet download lock")?;

    // Another process may have finished while we waited for the lock.
    if files.iter().all(|f| f.exists()) {
        return Ok(());
    }

    let archive = dir.join("parakeet-v3-int8.tar.bz2");
    tracing::warn!(
        url = MODEL_ARCHIVE_URL,
        dest = %dir.display(),
        "parakeet: downloading the model archive (~640 MB); the first voice note will wait for it"
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(None)
        .build()
        .context("parakeet: build http client")?;
    let mut resp = client
        .get(MODEL_ARCHIVE_URL)
        .send()
        .context("parakeet: model download request failed")?;
    if !resp.status().is_success() {
        bail!(
            "parakeet: model download returned HTTP {}: {MODEL_ARCHIVE_URL}",
            resp.status()
        );
    }
    let mut out = std::fs::File::create(&archive)
        .with_context(|| format!("create {}", archive.display()))?;
    std::io::copy(&mut resp, &mut out)
        .with_context(|| format!("write {}", archive.display()))?;
    drop(out);

    let size = std::fs::metadata(&archive)?.len();
    if size < MIN_ENCODER_BYTES {
        bail!(
            "parakeet: downloaded archive is only {size} bytes; expected ~640 MB. \
             Leaving it in place at {} for inspection.",
            archive.display()
        );
    }

    unpack(&archive, dir)?;
    let _ = std::fs::remove_file(&archive);

    // The archive expands into its own directory; lift the four files up so the
    // paths this module hands to sherpa-onnx do not depend on the archive's
    // internal layout.
    let inner = dir.join(MODEL_ARCHIVE_ROOT);
    if inner.is_dir() {
        for name in ["encoder.int8.onnx", "decoder.int8.onnx", "joiner.int8.onnx", "tokens.txt"] {
            let from = inner.join(name);
            let to = dir.join(name);
            if from.exists() && !to.exists() {
                std::fs::rename(&from, &to)
                    .with_context(|| format!("move {} -> {}", from.display(), to.display()))?;
            }
        }
        let _ = std::fs::remove_dir_all(&inner);
    }

    for f in files {
        if !f.exists() {
            bail!(
                "parakeet: the archive unpacked but {} is missing — the export layout may have \
                 changed upstream",
                f.display()
            );
        }
    }
    Ok(())
}

/// Unpack a `.tar.bz2` using the system `tar`.
///
/// Shelling out rather than linking a tar and a bzip2 crate: this runs once per
/// machine, and `tar` is present on every platform claudebase ships to —
/// including Windows 10 and later, which bundles bsdtar as `tar.exe`. Both GNU
/// tar and bsdtar detect bzip2 from the file itself, so no `-j` is passed.
///
/// If it is somehow absent, the error names the exact command to run by hand,
/// because a 640 MB download that has already succeeded should not have to be
/// repeated.
fn unpack(archive: &Path, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => bail!(
            "parakeet: `tar -xf {} -C {}` exited with {s}",
            archive.display(),
            dest.display()
        ),
        Err(e) => bail!(
            "parakeet: could not run `tar` ({e}). The archive is downloaded; unpack it by hand:\n  \
             tar -xf {} -C {}",
            archive.display(),
            dest.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The multilingual build, and nothing else.
    ///
    /// `parakeet-tdt-0.6b-v2` is English-only and does not fail on Russian — it
    /// returns English-shaped nonsense, which is the failure mode v0.8.1 fixed
    /// for whisper and issue 007 parked this backend over. A version slip here
    /// would be silent in every test that does not read the URL.
    #[test]
    fn the_pinned_model_is_the_multilingual_one() {
        assert!(
            MODEL_ARCHIVE_URL.contains("parakeet-tdt-0.6b-v3"),
            "the archive must be the v3 (multilingual) export, got: {MODEL_ARCHIVE_URL}"
        );
        assert!(
            !MODEL_ARCHIVE_URL.contains("v2"),
            "v2 is English-only and fails silently on Russian: {MODEL_ARCHIVE_URL}"
        );
        assert!(MODEL_ARCHIVE_ROOT.contains("v3"));
    }

    /// Same discipline as whisper: never take every core.
    #[test]
    fn the_default_thread_count_does_not_take_every_core() {
        let picked = resolve_threads(None);
        assert!(picked <= 4 && picked >= 1, "default {picked} threads is too greedy");
        assert!(resolve_threads(Some(0)) >= 1, "0 threads must not reach onnxruntime");
    }

    /// The model lives beside the whisper one, not in a directory this module
    /// invented — two conventions for "where models live" is how a documented
    /// path ends up wrong exactly where someone is debugging.
    #[test]
    fn the_model_sits_beside_the_other_models() {
        let dir = model_dir().expect("model dir");
        assert_eq!(dir.file_name().unwrap(), "parakeet");
        assert_eq!(dir.parent().unwrap().file_name().unwrap(), "models");
    }
}
