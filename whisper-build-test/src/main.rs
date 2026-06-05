// Slice 0b spike — whisper-rs cross-platform link verification.
//
// This binary exists only to force the linker to resolve a symbol from
// `whisper_rs`. Successful `cargo build --release` on linux-x64,
// macos-arm64, and windows-x64 proves that:
//   1. whisper-rs's build.rs invokes whisper.cpp's CMake build cleanly,
//   2. the resulting static lib links against the host C++ runtime, and
//   3. the Rust binding's public API surface is visible to consumers.
//
// We do NOT run any inference — that needs a model file and would slow CI.
// The reference to WhisperContextParameters is enough to drag the symbol
// across the linker boundary.

fn main() {
    // Reference a type from whisper_rs to force linking; do NOT run inference.
    let _: Option<whisper_rs::WhisperContextParameters> = None;
    println!("whisper-rs links cleanly");
}
