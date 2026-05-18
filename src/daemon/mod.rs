//! claudebase daemon — persistent background process that owns the
//! local IPC surface (UDS on Unix, named pipe on Windows) for Claude
//! Code MCP plugin bridges (Slice 1b) and future agent-chat (Slice 5+).
//!
//! Architecture (Slice 1a baseline):
//! - `serve()` (server.rs) — bind listener, accept loop, per-connection
//!   tokio tasks, length-prefixed JSON echo.
//! - `ipc.rs` — wire codec: 4-byte big-endian length + UTF-8 JSON body,
//!   16 MiB cap.
//!
//! INVARIANT (async discipline): no `.await` in any tokio task may
//! execute while a `std::sync::Mutex` guard (PDFIUM / ENCODER /
//! OCR_ENGINE) is held — see the module-level invariant in
//! `src/main.rs` for the full rule and the spawn_blocking escape hatch.
//! Slice 1a daemon code does NOT touch those mutexes; future slices
//! that DO must wrap the lock-and-use site in
//! `tokio::task::spawn_blocking`.

pub mod agent_registry;
pub mod asr;
pub mod channel_state;
pub mod chat;
pub mod config;
pub mod ipc;
pub mod permissions;
pub mod server;
pub mod service;
pub mod telegram;

use std::future::Future;

/// Construct a fresh tokio runtime and run the supplied future to
/// completion. This is the ONLY tokio entry point in the binary — every
/// other subcommand stays on the sync dispatch path so we don't pay the
/// runtime startup cost for ingest / search / list / status / etc.
///
/// Used from the `Command::Daemon` and `Command::Plugin` match arms in
/// `src/main.rs`.
pub fn run_tokio<F: Future<Output = R>, R>(future: F) -> R {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime build (should never fail on a healthy system)");
    rt.block_on(future)
}
