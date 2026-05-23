//! State directory layout — mirrors TSX `server.ts:26-29, 53-54` so both
//! implementations read/write the SAME files. This is critical: an
//! operator can toggle between TSX and Rust mid-session and the
//! allowlist / token / inbox persists.

use std::path::PathBuf;

/// Resolved state directory. Honors `TELEGRAM_STATE_DIR` env var; defaults
/// to `~/.claude/channels/telegram`.
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TELEGRAM_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".claude/channels/telegram")
}

pub fn access_file() -> PathBuf {
    state_dir().join("access.json")
}

pub fn approved_dir() -> PathBuf {
    state_dir().join("approved")
}

pub fn env_file() -> PathBuf {
    state_dir().join(".env")
}

pub fn inbox_dir() -> PathBuf {
    state_dir().join("inbox")
}

pub fn pid_file() -> PathBuf {
    state_dir().join("bot.pid")
}

/// Load the `~/.claude/channels/telegram/.env` file into process env.
/// Real env vars win (don't overwrite). Mirrors TSX `server.ts:31-39`.
pub fn load_env_file() {
    let path = env_file();
    let Ok(content) = std::fs::read_to_string(&path) else {
        tracing::debug!(?path, "no .env file to load");
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if std::env::var(key).is_err() {
            std::env::set_var(key, value);
            tracing::debug!(key = %key, "loaded env var from .env");
        }
    }
}

/// Read `TELEGRAM_BOT_TOKEN` from env (must be set; .env loaded first).
pub fn bot_token() -> Result<String, &'static str> {
    std::env::var("TELEGRAM_BOT_TOKEN").map_err(|_| "TELEGRAM_BOT_TOKEN not set")
}
