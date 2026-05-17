//! Slice 6-MVP — ASR (Automatic Speech Recognition) trait + factory.
//!
//! ## Design
//!
//! The `Asr` trait is the single abstraction every backend implements:
//!
//! - `transcribe(pcm, sample_rate) -> Result<String>` — async, returns
//!   the decoded transcript text.
//! - `health_check() -> Result<()>` — synchronous, no I/O beyond local
//!   filesystem checks (model file presence + sha). Powers
//!   `daemon doctor --asr`.
//!
//! The trait uses `#[async_trait]` (architect insight #11): native
//! `async fn` in traits creates Send-bound friction at `dyn Asr`
//! boundaries — the macro returns `Pin<Box<dyn Future + Send>>` which
//! sidesteps the issue. The one-heap-alloc-per-call cost is acceptable
//! because `transcribe` runs once per voice note (~30s interval).
//!
//! The `make_asr(config) -> Box<dyn Asr>` factory dispatches by the
//! `[asr] backend = "..."` value in `daemon.toml`. Backends are
//! feature-gated at COMPILE time AND runtime-error-fallback at the
//! factory:
//!
//! - `"whisper"` — local whisper.cpp via whisper-rs. Gated behind
//!   `--features asr-whisper`. Feature OFF → factory returns clean
//!   `anyhow::Error`, NEVER panics (PRD FR-ACD-7.4).
//! - `"sherpa-nemo"` — ALWAYS returns Err in v1; Wave-6 implementation.
//! - `"nim"` — ALWAYS returns Err in v1; Wave-6 implementation.
//! - unknown name → Err with the offending name surfaced.
//!
//! ## Why the runtime-error fallback (architect plan-bug context)
//!
//! Slice 6-MVP ships ONLY whisper. sherpa-nemo / nim are reserved
//! namespace for Wave-6 backends — operators that point `daemon.toml`
//! at them MUST get a clean Err the daemon can surface in
//! `daemon doctor --asr`, NOT a panic that kills the process. The
//! factory is the central point that enforces this: every "not yet
//! implemented" path takes the `anyhow::bail!("...not implemented in
//! v1...")` route.

use anyhow::{bail, Result};
use async_trait::async_trait;

use crate::daemon::config::Config;

pub mod decoder;

#[cfg(feature = "asr-whisper")]
pub mod whisper;

/// Speech-recognition trait — one method per pipeline stage that the
/// downstream needs to drive.
///
/// `Send + Sync + 'static` bounds let the daemon stash an `Arc<dyn Asr>`
/// in the Telegram long-poll task. The `'static` bound is what allows
/// the Arc to live for the daemon's lifetime; `Send + Sync` make the
/// Arc safe to clone across tokio worker threads.
#[async_trait]
pub trait Asr: Send + Sync + 'static {
    /// Decode `pcm` (16 kHz mono `f32`, range `[-1.0, 1.0]`) into a
    /// transcript string. `sample_rate` is passed for forward-compat —
    /// v1 backends assert 16 000 and treat anything else as an error.
    async fn transcribe(&self, pcm: Vec<f32>, sample_rate: u32) -> Result<String>;

    /// Synchronous health check for `daemon doctor --asr`. Backends
    /// verify their own preconditions:
    ///   - whisper: model file present + sha valid
    ///   - sherpa: ONNX files configured + readable
    ///   - nim: NVIDIA_API_KEY env var set
    /// Returns Ok(()) when healthy, Err with a clear message otherwise.
    fn health_check(&self) -> Result<()>;
}

/// Construct an `Asr` instance from `daemon.toml` config.
///
/// Dispatch:
/// - `Some("whisper")` + feature ON  → Box::new(WhisperAsr::new(...))
/// - `Some("whisper")` + feature OFF → Err("asr-whisper feature not compiled in")
/// - `Some("sherpa-nemo")`           → Err("backend 'sherpa-nemo' not implemented in v1 — see Wave 6")
/// - `Some("nim")`                   → Err("backend 'nim' not implemented in v1 — see Wave 6")
/// - `Some(other)`                   → Err("unknown asr backend: <other>")
/// - `None`                          → Err("no ASR backend configured in daemon.toml")
///
/// Errors are returned as `anyhow::Error` so the caller (daemon doctor
/// or the telegram long-poll bootstrapper) can format and route them
/// uniformly.
pub fn make_asr(config: &Config) -> Result<Box<dyn Asr>> {
    let backend = match config.asr.backend.as_deref() {
        Some(b) => b,
        None => bail!("no ASR backend configured in daemon.toml [asr] section"),
    };
    match backend {
        "whisper" => make_whisper(),
        // Wave-6 stubs. ALWAYS runtime-Err in v1 regardless of feature
        // state — preserves the namespace without crashing the daemon
        // when an operator points daemon.toml at them.
        "sherpa-nemo" => {
            bail!("backend 'sherpa-nemo' not implemented in v1 — see Wave 6")
        }
        "nim" => {
            bail!("backend 'nim' not implemented in v1 — see Wave 6")
        }
        other => bail!("unknown asr backend: {other}"),
    }
}

#[cfg(feature = "asr-whisper")]
fn make_whisper() -> Result<Box<dyn Asr>> {
    Ok(Box::new(whisper::WhisperAsr::new()?))
}

#[cfg(not(feature = "asr-whisper"))]
fn make_whisper() -> Result<Box<dyn Asr>> {
    bail!("backend 'whisper' selected but asr-whisper feature not compiled in — rebuild with `cargo build --features asr-whisper`")
}
