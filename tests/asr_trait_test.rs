//! Slice 6-MVP — `Asr` trait + `make_asr` factory contract tests.
//!
//! These tests pin the trait surface (object-safe, Send+Sync+'static) and
//! the factory's runtime-error contract: requesting a backend that is NOT
//! compiled in returns a clean `anyhow::Error` (NOT a panic) so the
//! daemon doesn't crash when the operator points `daemon.toml` at an
//! unimplemented backend.
//!
//! Per PRD FR-ACD-7.4: sherpa-nemo / nim backends are always runtime-Err
//! in v1 (Slice 6-MVP) regardless of compile-time feature state — they
//! reserve namespace for Wave-6 implementations. The whisper backend is
//! feature-gated; without `--features asr-whisper`, `make_asr("whisper")`
//! ALSO returns Err (clean message), but the test for that variant lives
//! in this file as well so a regression of the cfg gate gets caught.

use claudebase::daemon::asr::{make_asr, Asr};
use claudebase::daemon::config::{AsrConfig, Config};

fn config_with_backend(name: &str) -> Config {
    let mut cfg = Config::default();
    cfg.asr = AsrConfig {
        backend: Some(name.to_string()),
    };
    cfg
}

/// Sanity: when `[asr] backend = "whisper"` and the `asr-whisper` feature
/// is NOT compiled in (the default build for this repo per Cargo.toml
/// `[features] default = []`), the factory returns a clean Err with a
/// message that names the missing feature. NEVER a panic.
#[cfg(not(feature = "asr-whisper"))]
#[test]
fn make_asr_whisper_without_feature_returns_clear_err() {
    let cfg = config_with_backend("whisper");
    let result = make_asr(&cfg);
    assert!(
        result.is_err(),
        "make_asr(whisper) without feature should be Err"
    );
    let msg = format!("{}", result.err().expect("expected Err"));
    assert!(
        msg.contains("asr-whisper") && msg.contains("feature"),
        "error message should name the missing feature; got: {msg}"
    );
}

/// When `--features asr-whisper` IS enabled, the factory returns an Ok
/// boxed trait object (regardless of whether the whisper model file is
/// present — model presence is checked by `health_check()` and by the
/// transcribe path, not by construction).
#[cfg(feature = "asr-whisper")]
#[test]
fn make_asr_whisper_with_feature_returns_instance() {
    let cfg = config_with_backend("whisper");
    let result = make_asr(&cfg);
    // Construction must succeed even if the model file is missing —
    // the doctor surface is what reports missing-model.
    assert!(result.is_ok(), "make_asr(whisper) construction failed");
}

/// PRD FR-ACD-7.4 — sherpa-nemo is a Wave-6 stub. Calling `make_asr`
/// against it returns Err with a message containing `not implemented`
/// AND `v1` so the operator immediately understands the gap. Calling
/// `transcribe()` on this Err path would never happen — construction
/// fails first — but if a future change wires sherpa-nemo construction
/// to a stub trait impl, the regression test would need to add a
/// transcribe-returns-err case here.
#[test]
fn make_asr_sherpa_returns_unimplemented_err() {
    let cfg = config_with_backend("sherpa-nemo");
    let result = make_asr(&cfg);
    assert!(result.is_err(), "sherpa-nemo should be Err in v1");
    let msg = format!("{}", result.err().expect("expected Err"));
    assert!(
        msg.contains("not implemented") && msg.contains("v1"),
        "error message should mark sherpa-nemo as v1-unimplemented; got: {msg}"
    );
}

/// PRD FR-ACD-7.4 — nim is a Wave-6 stub (same contract as sherpa-nemo).
#[test]
fn make_asr_nim_returns_unimplemented_err() {
    let cfg = config_with_backend("nim");
    let result = make_asr(&cfg);
    assert!(result.is_err(), "nim should be Err in v1");
    let msg = format!("{}", result.err().expect("expected Err"));
    assert!(
        msg.contains("not implemented") && msg.contains("v1"),
        "error message should mark nim as v1-unimplemented; got: {msg}"
    );
}

/// Unknown backend name → clean Err with the offending name surfaced
/// so the operator can spot the typo in `daemon.toml`.
#[test]
fn make_asr_unknown_backend_returns_err() {
    let cfg = config_with_backend("totally-not-a-real-backend");
    let result = make_asr(&cfg);
    assert!(result.is_err());
    let msg = format!("{}", result.err().expect("expected Err"));
    assert!(
        msg.contains("totally-not-a-real-backend"),
        "error message should echo the offending name; got: {msg}"
    );
}

/// Missing `[asr] backend` entirely → clean Err so the daemon's startup
/// path can fail explicitly rather than picking a silent default.
#[test]
fn make_asr_no_backend_configured_returns_err() {
    let cfg = Config::default();
    let result = make_asr(&cfg);
    assert!(result.is_err(), "no backend configured should be Err");
    let msg = format!("{}", result.err().expect("expected Err"));
    assert!(
        msg.contains("backend") || msg.contains("ASR"),
        "error message should mention backend config; got: {msg}"
    );
}

/// Object-safety + auto-trait check. The `Asr` trait MUST be usable as
/// `Box<dyn Asr>` AND must carry `Send + Sync + 'static` bounds (the
/// `Arc<dyn Asr>` instance is shared across the tokio long-poll loop and
/// the spawn_blocking transcribe call site).
///
/// This is a compile-time check disguised as a runtime test — if the
/// trait ever loses an auto-trait bound the file fails to compile and
/// the regression surfaces at `cargo build` time.
#[test]
fn asr_trait_is_object_safe_send_sync_static() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn assert_static<T: 'static>() {}

    assert_send::<Box<dyn Asr>>();
    assert_sync::<Box<dyn Asr>>();
    assert_static::<Box<dyn Asr>>();
}
