//! Access control — schema-equivalent to TSX `server.ts:108-298`.
//!
//! All access state lives in `~/.claude/channels/telegram/access.json`.
//! Re-read on every inbound message so changes via `/telegram:access` skill
//! take effect without restart.

pub mod gate;
pub mod state;
