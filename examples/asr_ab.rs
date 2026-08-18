//! Runs whisper and parakeet over the SAME audio and prints what each cost and
//! what each heard.
//!
//! This is Slice 0 of `docs/plans/claudebase-v0.10-parakeet-asr.md`, and it
//! exists because the decision it feeds cannot be made from a benchmark someone
//! else ran: the claim "much faster than whisper" comes with a GPU attached,
//! and this machine has no NVIDIA card. Two numbers decide whether Parakeet
//! becomes the default — wall time here, and whether the Russian is right.
//!
//! ```text
//! cargo run --release --features asr-whisper,asr-sherpa --example asr_ab -- <file.f32|synth>
//! ```
//!
//! The input is raw 16 kHz mono `f32` PCM — the same buffer the daemon hands
//! the backend, so nothing about decoding or resampling is in the measurement.
//! Produce one from any audio file with:
//!
//! ```text
//! ffmpeg -v error -i note.ogg -ac 1 -ar 16000 -f f32le note.f32
//! ```
use std::time::Instant;

fn synth(seconds: usize) -> Vec<f32> {
    let sr = 16_000usize;
    (0..sr * seconds)
        .map(|i| {
            let t = i as f32 / sr as f32;
            let env = 0.5 * (1.0 + (2.0 * std::f32::consts::PI * 4.0 * t).sin());
            env * 0.2
                * ((2.0 * std::f32::consts::PI * 140.0 * t).sin()
                    + 0.5 * (2.0 * std::f32::consts::PI * 700.0 * t).sin())
        })
        .collect()
}

#[tokio::main]
async fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "synth".to_string());
    let pcm: Vec<f32> = if arg == "synth" {
        synth(4)
    } else {
        let bytes = std::fs::read(&arg).unwrap_or_else(|e| panic!("read {arg}: {e}"));
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    };
    println!(
        "audio: {} ({:.1}s at 16 kHz)\nload: {}",
        arg,
        pcm.len() as f32 / 16_000.0,
        std::fs::read_to_string("/proc/loadavg")
            .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    );

    use claudebase::daemon::asr::Asr;

    // Two runs each: the first pays for loading the model, the second is what a
    // running daemon actually costs per note.
    #[cfg(feature = "asr-whisper")]
    {
        let asr = claudebase::daemon::asr::whisper::WhisperAsr::new(None).expect("whisper");
        run("whisper", &asr, &pcm).await;
    }
    #[cfg(feature = "asr-sherpa")]
    {
        let asr = claudebase::daemon::asr::parakeet::ParakeetAsr::new(None).expect("parakeet");
        run("parakeet", &asr, &pcm).await;
    }
}

async fn run(name: &str, asr: &dyn claudebase::daemon::asr::Asr, pcm: &[f32]) {
    for pass in ["cold (model load included)", "warm"] {
        let t = Instant::now();
        match asr.transcribe(pcm.to_vec(), 16_000).await {
            Ok(text) => println!("\n{name:9} {pass:28} {:>8.1?}\n  -> {text}", t.elapsed()),
            Err(e) => {
                println!("\n{name:9} {pass:28} FAILED: {e:#}");
                return;
            }
        }
    }
}
