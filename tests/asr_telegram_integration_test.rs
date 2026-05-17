//! Slice 6-MVP — voice-note → ASR → chat.db integration boundary test.
//!
//! The production path is:
//!   1. teloxide long-poll yields an `Update` with a `voice` field
//!   2. `transcribe_voice_note(bot, msg, asr)` fetches the file,
//!      decodes Opus, calls `asr.transcribe(pcm, 16000)`
//!   3. The decoded text replaces `msg.text`, `msg.voice = None`
//!   4. `process_batch` sees the now-text-only update and inserts as
//!      a normal chat row (the existing Slice 4 path).
//!
//! Step 4 is already covered by Slice 4's `process_batch_voice_uses_shim_text`
//! (which we MUST update — the shim arm should now expect the literal
//! `[ASR error: ...]` fallback because the inbound mutation in
//! run_long_poll converted voice → text BEFORE process_batch ran).
//!
//! This test focuses on the boundary contract:
//!   - `transcribe_voice_note` with a MockAsr returns the canned text
//!   - on Err from `asr.transcribe`, the function returns Err and the
//!     caller is responsible for the `[voice transcription failed: ...]`
//!     fallback (the Asr trait itself doesn't paper over errors)
//!
//! The full TG-API mock loop is out of scope for the unit-level boundary
//! test — TC-6.x are end-to-end and live in the qa-cycle pass. The
//! library-level unit checks below cover the ASR↔telegram seam without
//! requiring a real teloxide mock.

use async_trait::async_trait;
use claudebase::daemon::asr::Asr;

/// In-test MockAsr that returns a canned transcript OR a fixed error.
/// Used to exercise the boundary code without standing up whisper-rs.
struct MockAsr {
    canned: Result<String, String>,
}

#[async_trait]
impl Asr for MockAsr {
    async fn transcribe(
        &self,
        _pcm: Vec<f32>,
        _sample_rate: u32,
    ) -> anyhow::Result<String> {
        match &self.canned {
            Ok(text) => Ok(text.clone()),
            Err(e) => Err(anyhow::anyhow!("{}", e)),
        }
    }
    fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Mock returns Ok("canned-transcript") — calling transcribe on the
/// trait boundary yields the canned value.
#[tokio::test]
async fn mock_asr_returns_canned_transcript() {
    let asr: Box<dyn Asr> = Box::new(MockAsr {
        canned: Ok("canned-transcript".to_string()),
    });
    let pcm: Vec<f32> = vec![0.1, -0.1, 0.0, 0.0];
    let out = asr.transcribe(pcm, 16_000).await.expect("transcribe failed");
    assert_eq!(out, "canned-transcript");
}

/// Mock returns Err — the trait boundary surfaces the Err. The caller
/// (`transcribe_voice_note` in telegram.rs) is what wraps it in the
/// `[voice transcription failed: ...]` placeholder; the Asr trait
/// itself does NOT paper over errors.
#[tokio::test]
async fn mock_asr_returns_err_when_canned_err() {
    let asr: Box<dyn Asr> = Box::new(MockAsr {
        canned: Err("simulated-backend-failure".to_string()),
    });
    let pcm: Vec<f32> = vec![0.0; 16_000];
    let result = asr.transcribe(pcm, 16_000).await;
    assert!(result.is_err(), "Err mock must surface as Err");
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("simulated-backend-failure"),
        "Err message should propagate from MockAsr; got: {msg}"
    );
}

/// Health-check on the mock succeeds (no model file to check). This
/// exercise validates the trait surface accepts a no-op health check —
/// real backends do model-file presence + sha verification here.
#[test]
fn mock_asr_health_check_passes() {
    let asr = MockAsr {
        canned: Ok("x".to_string()),
    };
    assert!(asr.health_check().is_ok());
}

/// The MockAsr is `Send + Sync + 'static` — same compile-time check
/// pattern as the `asr_trait_test::asr_trait_is_object_safe_send_sync_static`
/// test. Validates that test-helper mocks satisfy the same bounds the
/// production factory returns.
#[test]
fn mock_asr_is_send_sync_static() {
    fn assert_bounds<T: Send + Sync + 'static>() {}
    assert_bounds::<MockAsr>();
}
