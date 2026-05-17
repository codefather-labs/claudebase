//! Slice 6-MVP — pure-Rust Opus-in-Ogg → 16kHz mono PCM decode contract.
//!
//! The decoder lives in `src/daemon/asr/decoder.rs` and is the input
//! pipeline for every ASR backend (whisper today; sherpa/nim in Wave 6).
//! Telegram voice notes are always 48 kHz Opus-in-Ogg per the Bot API
//! spec, but the decoder is defensive — reads OpusHead for the actual
//! sample-rate + channels, downmixes stereo if present, resamples
//! 48 k → 16 k via `rubato::SincFixedIn`.
//!
//! Architect insight #10 (the v1 plan-bug): `symphonia` does NOT have an
//! `opus` codec feature in 0.6.x. The container layer is handled by the
//! `ogg` crate; the codec layer by `opus-decoder`. This test pins the
//! contract: `decode_ogg_opus_to_16k_mono_pcm(&bytes) -> Result<Vec<f32>>`
//! is the ONLY public surface — implementation details (which crate
//! handles which step) are free to evolve.
//!
//! Fixture status: a real 1-second Opus-in-Ogg fixture would be ideal
//! but generating one requires `opusenc` or `ffmpeg` (not available in
//! this build environment). The happy-path decode test is marked
//! `#[ignore]` with a removal path: the implementer of Slice 6.1 should
//! commit `tests/fixtures/voice_sample.ogg` and unignore the test. The
//! error-path tests (invalid bytes, empty, non-Opus) run unconditionally
//! and cover the failure modes the production loop hits when Telegram
//! sends a degraded file.

use claudebase::daemon::asr::decoder::decode_ogg_opus_to_16k_mono_pcm;

/// Random non-Ogg bytes — the decoder must return Err with a message
/// that names "ogg" or "opus" so the operator can pattern-match in
/// logs / telemetry.
#[test]
fn decode_invalid_bytes_returns_err() {
    let bytes: Vec<u8> = (0..=255u8).cycle().take(2048).collect();
    let result = decode_ogg_opus_to_16k_mono_pcm(&bytes);
    assert!(result.is_err(), "decode of arbitrary bytes should fail");
    let msg = format!("{}", result.unwrap_err()).to_lowercase();
    assert!(
        msg.contains("ogg") || msg.contains("opus") || msg.contains("decode"),
        "error message should reference the codec/container; got: {msg}"
    );
}

/// Empty input — the decoder must reject without panicking.
#[test]
fn decode_empty_bytes_returns_err() {
    let bytes: Vec<u8> = Vec::new();
    let result = decode_ogg_opus_to_16k_mono_pcm(&bytes);
    assert!(result.is_err(), "decode of empty bytes should fail");
}

/// Bytes that LOOK like an Ogg header (magic "OggS") but contain no
/// valid Opus stream — the decoder must reject, not return a zero-PCM
/// success. This catches the regression where the container layer
/// returns OK and the codec layer is never reached.
#[test]
fn decode_ogg_magic_but_no_opus_returns_err() {
    // Minimal Ogg page header that ogg::PacketReader will accept as a
    // page header but whose payload contains zero Opus packets. The
    // codec layer (opus-decoder) must see no packets and surface Err.
    //
    // Header layout per RFC 3533:
    //   bytes  0..4 : "OggS" magic
    //   byte   4    : stream structure version (0)
    //   byte   5    : header type flag (0x02 = first page)
    //   bytes  6..14: granule position (8 bytes, zero)
    //   bytes 14..18: bitstream serial number (4 bytes)
    //   bytes 18..22: page sequence number (4 bytes)
    //   bytes 22..26: page checksum (4 bytes, will fail CRC but ogg crate
    //                                         may still surface a clear err)
    //   byte  26    : number of page segments (0 — no payload)
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"OggS");
    bytes.push(0); // version
    bytes.push(0x02); // first-page flag
    bytes.extend_from_slice(&[0u8; 8]); // granule pos
    bytes.extend_from_slice(&[1u8; 4]); // serial
    bytes.extend_from_slice(&[0u8; 4]); // page seq
    bytes.extend_from_slice(&[0u8; 4]); // checksum (intentionally wrong)
    bytes.push(0); // num segments
    let result = decode_ogg_opus_to_16k_mono_pcm(&bytes);
    assert!(result.is_err(), "Ogg-shaped-but-no-Opus should be Err");
}

/// Happy-path decode against a real Opus-in-Ogg fixture. The fixture
/// must be a ~1-second 48 kHz mono voice clip (matches Telegram's voice
/// note format). The decoder returns ~16 000 f32 samples (1 s × 16 kHz)
/// with values in [-1.0, 1.0] and at least some non-zero content.
///
/// Marked `#[ignore]` until the fixture lands — see file-level comment.
/// To unignore: `cargo test --test decoder_ogg_opus_test -- --ignored`.
#[test]
#[ignore = "fixture tests/fixtures/voice_sample.ogg not yet committed; Slice 6.1 follow-up"]
fn decode_sample_returns_16k_mono_pcm() {
    let bytes = include_bytes!("fixtures/voice_sample.ogg");
    let pcm = decode_ogg_opus_to_16k_mono_pcm(bytes).expect("decode failed");
    let n = pcm.len();
    assert!(
        (15_000..=17_000).contains(&n),
        "expected ~16k samples for 1s @ 16kHz; got {n}"
    );
    for sample in &pcm {
        assert!(
            (-1.0..=1.0).contains(sample),
            "pcm value {sample} outside [-1, 1]"
        );
    }
    let nonzero = pcm.iter().any(|s| s.abs() > 0.001);
    assert!(nonzero, "decoded PCM has no non-zero content");
}
