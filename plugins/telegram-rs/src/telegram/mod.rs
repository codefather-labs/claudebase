//! Telegram bot — long-polling loop, message dispatch, outbound API calls.
//!
//! Slice R3 scope: long-polling skeleton + access.json read on each message,
//! log to stderr only. R4 wires the channel notification emitter.

pub mod api;
pub mod bot;
pub mod inbox;
