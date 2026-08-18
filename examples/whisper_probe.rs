//! Measures where voice-note latency actually goes: model load vs inference,
//! and how inference scales with thread count on THIS machine under ITS
//! current load. Run with `--features asr-whisper`.
//!
//! The audio is synthetic on purpose — the question is cost, not accuracy.
use std::time::Instant;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

fn synth(seconds: usize) -> Vec<f32> {
    // Amplitude-modulated harmonic stack: ambiguous enough to exercise the
    // decoder rather than short-circuiting on digital silence.
    let sr = 16_000usize;
    (0..sr * seconds)
        .map(|i| {
            let t = i as f32 / sr as f32;
            let env = 0.5 * (1.0 + (2.0 * std::f32::consts::PI * 4.0 * t).sin());
            env * 0.2
                * ((2.0 * std::f32::consts::PI * 140.0 * t).sin()
                    + 0.5 * (2.0 * std::f32::consts::PI * 700.0 * t).sin()
                    + 0.3 * (2.0 * std::f32::consts::PI * 1900.0 * t).sin())
        })
        .collect()
}

fn main() {
    let model = std::env::args().nth(1).expect("usage: whisper_probe <model.bin>");
    let pcm = match std::env::args().nth(2) {
        Some(raw) => {
            let bytes = std::fs::read(&raw).expect("read pcm");
            bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
        }
        None => synth(4),
    };
    let secs = pcm.len() / 16_000;

    let t0 = Instant::now();
    let ctx = WhisperContext::new_with_params(&model, WhisperContextParameters::default())
        .expect("open model");
    println!("model load          : {:?}", t0.elapsed());

    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    for threads in [cores, cores / 2, cores / 4] {
        if threads == 0 { continue }
        for (label, temp_fallback) in [("defaults", true), ("no temp-fallback", false)] {
            let mut state = ctx.create_state().expect("state");
            let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            p.set_n_threads(threads as i32);
            p.set_language(Some("auto"));
            p.set_print_progress(false);
            p.set_print_realtime(false);
            p.set_print_timestamps(false);
            if !temp_fallback {
                p.set_temperature_inc(0.0);
                p.set_no_context(true);
            }
            let t = Instant::now();
            state.full(p, &pcm).expect("full");
            println!("{secs}s audio | {threads:2} threads | {label:16} : {:?}", t.elapsed());
        }
    }
}
