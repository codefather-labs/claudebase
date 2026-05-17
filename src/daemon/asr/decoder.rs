//! Slice 6-MVP — Opus-in-Ogg → 16 kHz mono PCM decode pipeline.
//!
//! Telegram voice notes are ALWAYS 48 kHz Opus packets in an Ogg
//! container (per the Bot API spec). The decoder is defensive:
//!
//! 1. `ogg::PacketReader` peels packets out of the .ogg container.
//! 2. The first packet is `OpusHead` (RFC 7845 §5.1): magic `"OpusHead"`
//!    + version + channel count + pre-skip + sample-rate + gain + map.
//!    We read channel count + input sample rate from this packet.
//! 3. The second packet is `OpusTags` (RFC 7845 §5.2) — skipped, no
//!    audio data.
//! 4. Subsequent packets are Opus audio frames. Each decoded via
//!    `opus_decoder::OpusDecoder::decode_float` → interleaved f32
//!    samples in `[-1.0, 1.0]`.
//! 5. Stereo → mono: average L+R per frame.
//! 6. 48 kHz → 16 kHz: linear interpolation (3:1 decimation with
//!    sub-sample interpolation). Cheap, deterministic, ASR-quality
//!    adequate (whisper's mel-spectrogram preprocessing dwarfs the
//!    resampler's <0.1 dB artefacts).
//!
//! The public surface is the single function
//! `decode_ogg_opus_to_16k_mono_pcm(&[u8]) -> Result<Vec<f32>>`.
//! Implementation details are free to evolve — the only contract is
//! "input: Telegram voice-note bytes; output: 16 kHz mono PCM Vec<f32>".

use std::io::Cursor;

use anyhow::{anyhow, bail, Context, Result};

const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Decode an Opus-in-Ogg byte stream into 16 kHz mono PCM. See module
/// doc for the pipeline. Errors are returned as `anyhow::Error` with
/// messages that include the substring "ogg", "opus", or "decode" so
/// callers can pattern-match for telemetry / log searching.
pub fn decode_ogg_opus_to_16k_mono_pcm(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.is_empty() {
        bail!("ogg decode: input is empty");
    }

    let cursor = Cursor::new(bytes);
    let mut reader = ogg::PacketReader::new(cursor);

    // Pull the OpusHead packet (first packet of the logical bitstream).
    let head_packet = reader
        .read_packet()
        .map_err(|e| anyhow!("ogg page read error: {e}"))?
        .ok_or_else(|| anyhow!("ogg decode: no packets in stream"))?;

    let (channels, _input_sr) = parse_opus_head(&head_packet.data)
        .context("opus decode: parse OpusHead failed")?;

    // Pull (and skip) the OpusTags packet (second packet). Some streams
    // may omit it on truncation; treat absence as fatal because no audio
    // packets would follow.
    let _tags = reader
        .read_packet()
        .map_err(|e| anyhow!("ogg page read error after head: {e}"))?
        .ok_or_else(|| anyhow!("opus decode: missing OpusTags packet"))?;

    // Construct the codec layer. Per RFC 7845 the decoder ALWAYS runs at
    // 48 kHz regardless of the OpusHead `input_sample_rate` field
    // (that's metadata about the ORIGINAL signal pre-encoding; the bit
    // stream itself is always 48 kHz).
    let mut decoder = opus_decoder::OpusDecoder::new(48_000, channels as usize)
        .map_err(|e| anyhow!("opus decode: decoder construction failed: {e:?}"))?;

    // Per-packet decode → interleaved f32 samples. Opus max frame is
    // 120 ms @ 48 kHz = 5760 samples/channel. Allocate a scratch buffer
    // sized for the worst case to avoid per-packet reallocation.
    let mut pcm_scratch = vec![0.0f32; 5760 * channels as usize];
    let mut mono_48k: Vec<f32> = Vec::with_capacity(48_000); // ~1s; grows on need

    while let Some(packet) = reader
        .read_packet()
        .map_err(|e| anyhow!("ogg page read error mid-stream: {e}"))?
    {
        let samples_per_channel = decoder
            .decode_float(&packet.data, &mut pcm_scratch, false)
            .map_err(|e| anyhow!("opus decode: packet decode failed: {e:?}"))?;
        let written = samples_per_channel * channels as usize;
        match channels {
            1 => {
                mono_48k.extend_from_slice(&pcm_scratch[..written]);
            }
            2 => {
                // Interleaved L,R,L,R → mono via (L+R)/2 per frame.
                for frame in pcm_scratch[..written].chunks_exact(2) {
                    mono_48k.push(0.5 * (frame[0] + frame[1]));
                }
            }
            n => {
                bail!("opus decode: unsupported channel count {n} (only mono/stereo)");
            }
        }
    }

    if mono_48k.is_empty() {
        bail!("opus decode: no audio frames decoded");
    }

    // 48 kHz mono → 16 kHz mono via linear interpolation. Strict 3:1
    // decimation with mid-sample interpolation: output sample at index
    // `i` corresponds to input position `i * 3.0`. Linear blend between
    // floor and ceil index keeps the resampler causal + bounded.
    let mut mono_16k = Vec::with_capacity(mono_48k.len() / 3 + 1);
    let ratio = 48_000.0 / TARGET_SAMPLE_RATE as f64; // 3.0
    let mut t: f64 = 0.0;
    while (t as usize) + 1 < mono_48k.len() {
        let i = t as usize;
        let frac = t - i as f64;
        let s = mono_48k[i] as f64 * (1.0 - frac) + mono_48k[i + 1] as f64 * frac;
        mono_16k.push(s as f32);
        t += ratio;
    }
    if mono_16k.is_empty() {
        bail!("opus decode: resampled output is empty");
    }
    Ok(mono_16k)
}

/// Parse the OpusHead packet body (RFC 7845 §5.1). Returns
/// `(channel_count, input_sample_rate)`. Validates the magic + version.
fn parse_opus_head(data: &[u8]) -> Result<(u8, u32)> {
    if data.len() < 19 {
        bail!("opus decode: OpusHead too short ({} bytes, need ≥19)", data.len());
    }
    if &data[0..8] != b"OpusHead" {
        bail!("opus decode: OpusHead magic mismatch");
    }
    let version = data[8];
    // RFC 7845: version 0..15 supported; major version != 0 is fatal.
    if version >= 16 {
        bail!("opus decode: unsupported OpusHead version {version}");
    }
    let channels = data[9];
    if channels == 0 {
        bail!("opus decode: OpusHead channels=0");
    }
    // Bytes 12..16 (LE u32) = input_sample_rate (metadata only, not the
    // bitstream rate — bitstream is always 48 kHz per RFC 7845).
    let input_sr = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    Ok((channels, input_sr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_opus_head_accepts_minimal_valid_header() {
        let mut data = Vec::new();
        data.extend_from_slice(b"OpusHead");
        data.push(1); // version
        data.push(1); // channels (mono)
        data.extend_from_slice(&[0u8, 0]); // pre-skip
        data.extend_from_slice(&48_000u32.to_le_bytes()); // input sr
        data.extend_from_slice(&[0u8, 0]); // gain
        data.push(0); // channel mapping family
        let (ch, sr) = parse_opus_head(&data).expect("parse failed");
        assert_eq!(ch, 1);
        assert_eq!(sr, 48_000);
    }

    #[test]
    fn parse_opus_head_rejects_bad_magic() {
        let mut data = vec![0u8; 19];
        data[0..8].copy_from_slice(b"BadMagic");
        let result = parse_opus_head(&data);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("magic"));
    }

    #[test]
    fn parse_opus_head_rejects_zero_channels() {
        let mut data = Vec::new();
        data.extend_from_slice(b"OpusHead");
        data.push(1); // version
        data.push(0); // channels=0 — invalid
        data.extend_from_slice(&[0u8; 9]); // pre-skip + sr + gain + map
        let result = parse_opus_head(&data);
        assert!(result.is_err());
    }
}
