//! Slice 4 — daemon.toml + secrets.toml loaders with SEC-9 / SEC-10 / SEC-15.
//!
//! ## File layout
//!
//! Two files under `$HOME/.config/claudebase/`:
//!
//! - `daemon.toml` (mode irrelevant — no secrets) — user-editable config:
//!   `[asr] backend = "..."`, `[telegram] dmPolicy = "...", poll_interval_secs = N`.
//!   MUST NOT contain a `bot_token` field (SEC-15 — forbidden; secret material
//!   belongs in secrets.toml only).
//! - `secrets.toml` (MUST be 0600, lstat-checked) — contains:
//!   `[telegram] bot_token = "..."`. Symlinks REFUSED (SEC-9). The token
//!   value is wrapped in `RedactedToken` so any accidental Display / Debug
//!   formatting emits `"***"` instead of the literal token (SEC-10).
//!
//! ## Security backbone
//!
//! Both loaders run `symlink_metadata` (lstat) BEFORE any `read_to_string`.
//! Symlinks are refused outright — a symlink whose target is a 0600 file
//! would lstat as a symlink (mode permissions on the link itself, not the
//! target), so the permission check alone is insufficient. The symlink-refuse
//! is the load-bearing TOCTOU mitigation against `ln -s /etc/whatever
//! ~/.config/claudebase/secrets.toml` confusion attacks.
//!
//! On Unix, secrets.toml MUST also satisfy `mode & 0o077 == 0` (no group /
//! other bits set). On Windows the perm check is skipped — NTFS ACLs are
//! not directly comparable; Slice 2's service installer is responsible for
//! ACL-locking the file under the running user.

use std::fmt;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Newtype wrapping a Telegram bot token. The wrapped string is only
/// accessible via `as_str()`; `Display` and `Debug` BOTH emit the literal
/// `"***"` (three asterisks) so accidental `tracing::*` or `format!`
/// invocations cannot leak the token into logs (SEC-10).
#[derive(Clone)]
pub struct RedactedToken(String);

impl RedactedToken {
    /// Wrap a raw token string. Caller MUST have loaded the string from a
    /// secrets-grade source (perm-checked secrets.toml — SEC-9).
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Reveal the inner token. Use ONLY at the teloxide `Bot::new` boundary —
    /// every other site MUST work with the wrapper to keep the redaction
    /// invariant intact.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RedactedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Debug for RedactedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl Serialize for RedactedToken {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Emit "***" if anyone ever feeds a RedactedToken through serde
        // (e.g. `config show --json`). The unredacted form is intentionally
        // not reachable via any Serialize impl.
        serializer.serialize_str("***")
    }
}

/// Top-level daemon.toml schema. Currently only `telegram` and `asr` blocks
/// are surfaced; unknown keys are tolerated (TOML keeps the original Value
/// tree in `extra` so `config show` can echo them).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub asr: AsrConfig,
    #[serde(default)]
    pub daemon: DaemonSection,
}

/// `[telegram]` block. All fields optional with sensible defaults so
/// missing-or-partial daemon.toml does not crash the parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Whether the telegram long-poll loop should start at all.
    #[serde(default = "default_telegram_enabled")]
    pub enabled: bool,
    /// Poll interval seconds (teloxide getUpdates timeout). Default 25s
    /// matches Telegram's long-poll recommended ceiling.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u32,
    /// DM policy. Mirrors the access.json `dmPolicy` field; daemon.toml
    /// only sets the default for fresh installs — runtime authority lives
    /// in access.json (which Slice 4 `access pair` mutates).
    #[serde(default)]
    pub dm_policy: DmPolicy,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: default_telegram_enabled(),
            poll_interval_secs: default_poll_interval(),
            dm_policy: DmPolicy::default(),
        }
    }
}

fn default_telegram_enabled() -> bool {
    true
}

fn default_poll_interval() -> u32 {
    25
}

/// DM policy enum mirrored from access.json — UC-6-B semantics:
///
/// - `Pairing` (default) — `/start` from unknown user → bot replies with
///   a pairing code; user must run `claudebase daemon access pair <code>`
///   to be added to `allowFrom`. Messages from non-allow-listed users are
///   discarded silently per TC-4.3.
/// - `Allowlist` — same enforcement as Pairing but no auto-generated codes;
///   the operator adds user IDs out-of-band.
/// - `Disabled` — bot accepts ALL inbound messages (SEC-12: the brief
///   originally said "silent drop", UC-6-B overrode it to "accept all" —
///   "disabled" means the policy filter is disabled, not that DMs are
///   disabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    Pairing,
    Allowlist,
    Disabled,
}

impl Default for DmPolicy {
    fn default() -> Self {
        DmPolicy::Pairing
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsrConfig {
    /// Slice 6-MVP populates this with `"whisper"`. v1 default empty so the
    /// daemon doesn't crash if ASR config is absent.
    #[serde(default)]
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonSection {
    #[serde(default)]
    pub log_level: Option<String>,
    #[serde(default)]
    pub port: Option<u32>,
}

/// `secrets.toml` schema — must be 0600, must not be a symlink.
#[derive(Debug, Clone)]
pub struct Secrets {
    pub telegram: TelegramSecrets,
}

#[derive(Debug, Clone)]
pub struct TelegramSecrets {
    pub bot_token: RedactedToken,
}

/// Internal TOML-parse-only shape; we never store `String` for the token,
/// we move it into `RedactedToken` immediately on load.
#[derive(Debug, Deserialize)]
struct SecretsRaw {
    telegram: TelegramSecretsRaw,
}

#[derive(Debug, Deserialize)]
struct TelegramSecretsRaw {
    bot_token: String,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Return the canonical config directory: `$HOME/.config/claudebase/` on
/// Unix, `$USERPROFILE\AppData\Roaming\claudebase\` on Windows (XDG-spec
/// fallback path on Unix when XDG_CONFIG_HOME is not set).
pub fn user_level_config_dir() -> PathBuf {
    #[cfg(unix)]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("claudebase");
            }
        }
        let home = std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
        PathBuf::from(home).join(".config").join("claudebase")
    }
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .unwrap_or_else(|| std::ffi::OsString::from("C:\\"));
        PathBuf::from(appdata).join("claudebase")
    }
}

pub fn user_level_daemon_toml_path() -> PathBuf {
    user_level_config_dir().join("daemon.toml")
}

pub fn user_level_secrets_toml_path() -> PathBuf {
    user_level_config_dir().join("secrets.toml")
}

// ---------------------------------------------------------------------------
// daemon.toml loader (SEC-15)
// ---------------------------------------------------------------------------

/// Load `daemon.toml` from `path`. SEC-15 enforcement:
///
/// 1. `symlink_metadata` BEFORE any read — refuse if path is a symlink.
/// 2. After parsing, refuse if a `bot_token` key exists anywhere in the
///    top-level table OR in the `[telegram]` block (case-sensitive). The
///    error message contains the literal `"secrets.toml"` so users know
///    where the token belongs.
///
/// Returns `Ok(Config)` only when both checks pass and the TOML is valid.
pub fn load_daemon_toml(path: &Path) -> Result<Config> {
    // SEC-15 step 1: symlink refuse via lstat (symlink_metadata reads the
    // link itself, not the target).
    let meta = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "failed to stat daemon.toml at {} — check parent dir exists",
            path.display()
        )
    })?;
    if meta.file_type().is_symlink() {
        bail!("refuse to read symlink: {}", path.display());
    }

    let body = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read daemon.toml at {}", path.display()))?;

    // SEC-15 step 2: parse generically first, scan for bot_token, THEN
    // re-parse into Config. The generic parse lets us walk the original
    // Value tree to detect bot_token regardless of which section it lives
    // in.
    let raw: toml::Value = toml::from_str(&body).map_err(|e| {
        // The literal token "TOML parse error" surfaces here so TC-4.13
        // can pattern-match on "toml" / "parse" / "invalid".
        anyhow::anyhow!("TOML parse error in daemon.toml: {e}")
    })?;

    if contains_bot_token(&raw) {
        bail!("bot_token must live in secrets.toml, not daemon.toml");
    }

    let config: Config = raw
        .try_into()
        .with_context(|| "daemon.toml schema mismatch (after bot_token check)")?;
    Ok(config)
}

/// Recursive scan for a `bot_token` key anywhere in a TOML `Value` tree.
/// Returns true if found (any case-sensitive match) — TC-4.SEC-15 pre-creates
/// `[telegram] bot_token = "..."` so the bot_token lives one level deep.
fn contains_bot_token(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(tbl) => {
            if tbl.contains_key("bot_token") {
                return true;
            }
            tbl.values().any(contains_bot_token)
        }
        toml::Value::Array(arr) => arr.iter().any(contains_bot_token),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// secrets.toml loader (SEC-9 / SEC-10)
// ---------------------------------------------------------------------------

/// Load `secrets.toml` from `path`. SEC-9 enforcement:
///
/// 1. `symlink_metadata` BEFORE any read — refuse if path is a symlink.
/// 2. On Unix, mode MUST satisfy `mode & 0o077 == 0` — no group / other bits.
///    The literal error string `"must have permissions 0600"` is required by
///    TC-4.14.
/// 3. Only then is the file opened and parsed.
///
/// The bot_token is moved into `RedactedToken` so it cannot be accidentally
/// Display-formatted later (SEC-10 — the type system enforces the redaction).
pub fn load_secrets_toml(path: &Path) -> Result<Secrets> {
    // SEC-9 step 1: lstat for symlink check.
    let meta = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "failed to stat secrets.toml at {} — file may be missing",
            path.display()
        )
    })?;
    if meta.file_type().is_symlink() {
        bail!("refuse to read symlink: {}", path.display());
    }

    // SEC-9 step 2: Unix permission check. The mask 0o077 covers group +
    // other bits — any of them set means the file is too permissive.
    #[cfg(unix)]
    {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            // Literal "must have permissions 0600" required by TC-4.14.
            bail!(
                "secrets.toml must have permissions 0600 — current: {:#o}",
                mode
            );
        }
    }

    let body = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read secrets.toml at {}", path.display()))?;

    let raw: SecretsRaw = toml::from_str(&body)
        .map_err(|e| anyhow::anyhow!("TOML parse error in secrets.toml: {e}"))?;

    Ok(Secrets {
        telegram: TelegramSecrets {
            bot_token: RedactedToken::new(raw.telegram.bot_token),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_token_display_emits_three_asterisks() {
        let t = RedactedToken::new("REAL_TOKEN_VALUE".to_string());
        assert_eq!(format!("{t}"), "***");
        assert_eq!(format!("{t:?}"), "***");
    }

    #[test]
    fn redacted_token_as_str_reveals_value() {
        let t = RedactedToken::new("revealed".to_string());
        assert_eq!(t.as_str(), "revealed");
    }

    #[test]
    fn redacted_token_serde_emits_mask() {
        let t = RedactedToken::new("xxxx".to_string());
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"***\"");
    }

    #[test]
    fn dm_policy_serde_roundtrip() {
        for variant in [DmPolicy::Pairing, DmPolicy::Allowlist, DmPolicy::Disabled] {
            let s = toml::to_string(&Config {
                telegram: TelegramConfig {
                    dm_policy: variant,
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
            let back: Config = toml::from_str(&s).unwrap();
            assert_eq!(back.telegram.dm_policy, variant);
        }
    }

    #[test]
    fn contains_bot_token_detects_nested() {
        let v: toml::Value = toml::from_str(
            r#"[telegram]
bot_token = "x"
"#,
        )
        .unwrap();
        assert!(contains_bot_token(&v));
    }

    #[test]
    fn contains_bot_token_detects_top_level() {
        let v: toml::Value = toml::from_str(r#"bot_token = "x""#).unwrap();
        assert!(contains_bot_token(&v));
    }

    #[test]
    fn contains_bot_token_returns_false_when_absent() {
        let v: toml::Value = toml::from_str(
            r#"[asr]
backend = "whisper"
"#,
        )
        .unwrap();
        assert!(!contains_bot_token(&v));
    }
}
