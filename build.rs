//! Build script — exists solely to make the Parakeet backend loadable.
//!
//! `asr-sherpa` links sherpa-onnx as a SHARED library. That is not a
//! preference: the crate's default is static, and a static sherpa-onnx bundles
//! its own ONNX Runtime plus the onnx protobuf helpers, which collide at link
//! time with the copy `fastembed` already pulls in for the e5 retrieval
//! encoder — `duplicate symbol: onnx::ParseData<double>`, defined in both
//! archives. One shared library in the process instead of two static copies in
//! the archive is what resolves it.
//!
//! Shared linking costs a runtime search path. Without an rpath the binary dies
//! before `main` with `libsherpa-onnx-c-api.so: cannot open shared object
//! file`, which is a miserable way to learn that a feature flag has a
//! deployment consequence. So two rpaths are baked in:
//!
//! * `$ORIGIN/lib` — where an installed claudebase keeps them, next to the
//!   binary under `~/.claude/tools/claudebase/`. This is the one that matters
//!   for a release.
//! * the absolute prebuilt directory under `target/` — so a `cargo run` or
//!   `cargo test` in this checkout works without anyone exporting
//!   `LD_LIBRARY_PATH` first.
//!
//! Both are emitted only when the feature is on, so a default build is
//! untouched and keeps its single-file property.

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_ASR_SHERPA");
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");

    if std::env::var_os("CARGO_FEATURE_ASR_SHERPA").is_none() {
        return;
    }
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Windows has no rpath; the loader searches the executable's own directory,
    // which is exactly where an installed claudebase keeps its DLLs anyway.
    if target_os == "windows" {
        return;
    }
    let origin = if target_os == "macos" {
        "@loader_path/lib"
    } else {
        "$ORIGIN/lib"
    };
    println!("cargo:rustc-link-arg=-Wl,-rpath,{origin}");

    // Development convenience: point at wherever the sherpa build script
    // unpacked the prebuilt archive, so the binary in target/release runs.
    if let Some(dir) = sherpa_lib_dir() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }
}

/// Where the sherpa-onnx prebuilt shared libraries ended up in this checkout.
///
/// `SHERPA_ONNX_LIB_DIR` wins when the operator set it — that is the documented
/// escape hatch for supplying their own build. Otherwise the sys crate's build
/// script unpacks under `target/sherpa-onnx-prebuilt/`, and the directory name
/// carries the version, so it is discovered rather than hardcoded.
fn sherpa_lib_dir() -> Option<String> {
    if let Some(dir) = std::env::var_os("SHERPA_ONNX_LIB_DIR") {
        return Some(dir.to_string_lossy().into_owned());
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let root = std::path::Path::new(&manifest).join("target/sherpa-onnx-prebuilt");
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(&root).ok()? {
        let entry = entry.ok()?;
        let lib = entry.path().join("lib");
        if !lib.is_dir() {
            continue;
        }
        let when = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let path = lib.to_string_lossy().into_owned();
        if newest.as_ref().is_none_or(|(t, _)| when > *t) {
            newest = Some((when, path));
        }
    }
    newest.map(|(_, p)| p)
}
