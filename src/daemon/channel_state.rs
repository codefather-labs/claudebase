//! Channel state model — 1:1 port of the official Anthropic telegram plugin.
//!
//! Lives at `~/.claude/channels/claudebase/` so any user-facing tooling
//! (CLI subcommands, external operators, future skills) can read/write
//! it without going through a daemon API. Schema mirrors
//! `claude-plugins-official/external_plugins/telegram/server.ts` verbatim:
//!
//! ```json
//! {
//!   "dmPolicy": "pairing" | "allowlist" | "disabled",
//!   "allowFrom": ["<senderId>", ...],
//!   "groups": { "<groupId>": { "requireMention": true, "allowFrom": [] } },
//!   "pending": {
//!     "<6-char-code>": {
//!       "senderId": "...", "chatId": "...",
//!       "createdAt": <ms>, "expiresAt": <ms>,
//!       "replies": 1
//!     }
//!   },
//!   "mentionPatterns": ["@mybot"]
//! }
//! ```
//!
//! ## Key invariants
//!
//! - Senderr / chat IDs are STRINGS (Telegram numeric IDs serialized as
//!   strings). This matches the official skill which treats them as opaque
//!   strings — easier hand-editing, no JS Number-precision issues for IDs
//!   beyond 2^53.
//! - Pending codes are 6 hex chars (`randomBytes(3).toString('hex')` in
//!   server.ts:256). Port uses the same — `getrandom::getrandom(3 bytes)`
//!   formatted lowercase hex.
//! - Pending cap = 3 (server.ts:254). Extra DMs while saturated are
//!   silently dropped — the official decision to throttle pairing-code
//!   spam.
//! - TTL = 60 minutes (server.ts:262).
//! - On resend of same sender's existing non-expired code, increment
//!   `replies` counter. Max 2 replies — third inbound DM in pair-pending
//!   state is dropped silently (server.ts:247-249).
//! - Approved-dir = `<channel_state_dir>/approved/`. After
//!   `/claudebase:access pair <code>` succeeds the skill writes
//!   `approved/<senderId>` (contents = chatId). A 5s polling task in
//!   `daemon/telegram.rs` reads each file, sends "Paired! Say hi to
//!   Claude." to chatId, then unlinks the file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// 60-minute TTL on pairing codes (server.ts:262 — `60 * 60 * 1000` ms).
pub const PAIRING_CODE_TTL_MS: i64 = 60 * 60 * 1000;

/// Pending cap (server.ts:254 — extra attempts silently dropped).
pub const PENDING_CAP: usize = 3;

/// Max replies on a re-DM with same pending code (server.ts:247 —
/// initial reply + one reminder; third inbound silently dropped).
pub const MAX_PAIRING_REPLIES: u8 = 2;

/// 6 hex chars from 3 random bytes (server.ts:256).
pub const PAIRING_CODE_HEX_BYTES: usize = 3;

/// DM policy enum matching server.ts:103 verbatim.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEntry {
    #[serde(rename = "senderId")]
    pub sender_id: String,
    #[serde(rename = "chatId")]
    pub chat_id: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
    #[serde(default = "default_replies")]
    pub replies: u8,
}

fn default_replies() -> u8 {
    1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupPolicy {
    #[serde(rename = "requireMention", default = "default_require_mention")]
    pub require_mention: bool,
    #[serde(rename = "allowFrom", default)]
    pub allow_from: Vec<String>,
}

fn default_require_mention() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Access {
    #[serde(rename = "dmPolicy", default)]
    pub dm_policy: DmPolicy,
    #[serde(rename = "allowFrom", default)]
    pub allow_from: Vec<String>,
    #[serde(default)]
    pub groups: BTreeMap<String, GroupPolicy>,
    #[serde(default)]
    pub pending: BTreeMap<String, PendingEntry>,
    #[serde(rename = "mentionPatterns", default)]
    pub mention_patterns: Vec<String>,
}

impl Default for Access {
    fn default() -> Self {
        Self {
            dm_policy: DmPolicy::default(),
            allow_from: Vec::new(),
            groups: BTreeMap::new(),
            pending: BTreeMap::new(),
            mention_patterns: Vec::new(),
        }
    }
}

/// `~/.claude/channels/claudebase/` — root of channel state (matches
/// `~/.claude/channels/telegram/` convention from the official plugin).
pub fn channel_state_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    PathBuf::from(home)
        .join(".claude")
        .join("channels")
        .join("claudebase")
}

pub fn access_json_path() -> PathBuf {
    channel_state_dir().join("access.json")
}

pub fn approved_dir() -> PathBuf {
    channel_state_dir().join("approved")
}

pub fn env_file_path() -> PathBuf {
    channel_state_dir().join(".env")
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Load access.json (missing file → defaults — matches server.ts:166).
pub fn load_access(path: &Path) -> Result<Access> {
    match fs::read_to_string(path) {
        Ok(body) => {
            let access: Access = serde_json::from_str(&body).with_context(|| {
                format!("access.json at {} is not valid JSON", path.display())
            })?;
            Ok(access)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Access::default()),
        Err(e) => Err(anyhow!(
            "failed to read access.json at {}: {e}",
            path.display()
        )),
    }
}

/// Save access.json atomically (write → fsync → rename).
pub fn save_access(path: &Path, access: &Access) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {} parent dir", path.display()))?;
    }

    let body = serde_json::to_string_pretty(access)
        .with_context(|| "failed to serialise access.json")?;

    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body.as_bytes())
        .with_context(|| format!("failed to write tmp file at {}", tmp.display()))?;

    let f = fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .with_context(|| format!("failed to open tmp for fsync at {}", tmp.display()))?;
    f.sync_all()
        .with_context(|| format!("fsync failed on {}", tmp.display()))?;
    drop(f);

    fs::rename(&tmp, path).with_context(|| {
        format!(
            "atomic rename {} -> {} failed",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Drop expired entries from `pending`. Returns true if any were removed
/// (caller saves access.json in that case).
pub fn prune_expired(access: &mut Access, now: i64) -> bool {
    let before = access.pending.len();
    access.pending.retain(|_, e| e.expires_at > now);
    before != access.pending.len()
}

/// Generate a 6-char hex pairing code from 3 random bytes (matches
/// server.ts:256 `randomBytes(3).toString('hex')`).
pub fn generate_pairing_code() -> Result<String> {
    let mut buf = [0u8; PAIRING_CODE_HEX_BYTES];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow!("OS RNG failed: {e}"))?;
    Ok(hex::encode(buf))
}

/// Outcome of `gate_dm` — what to do with a Telegram DM under current
/// access policy. Mirrors `gate(ctx)` in server.ts:225-267.
#[derive(Debug, Clone)]
pub enum GateAction {
    /// Sender is allowed — deliver the message to claude (insert + broadcast).
    Deliver,
    /// Drop silently — policy=disabled, policy=allowlist with unknown sender,
    /// pending-cap saturated, or 3rd+ DM in pair-pending state.
    Drop,
    /// Reply to the sender with a pairing code. `is_resend` distinguishes
    /// initial code emission ("Pairing required ...") from re-send
    /// ("Still pending ..."). `code` is the 6-hex code.
    Pair { code: String, is_resend: bool },
}

/// Read-only allowlist predicate for inline-keyboard `callback_query` taps
/// (telegram-multi-cli Slice 5, security defense-in-depth).
///
/// Unlike the inbound MESSAGE path, a button tap must NEVER emit a pairing
/// code or mutate `access.pending` — a callback is an answer to a question
/// the operator was already asked, not a fresh contact attempt. So this is a
/// pure, side-effect-free predicate that returns `true` only for the exact
/// senders `gate_dm` would `Deliver`:
///
/// - `Disabled` → `false` (the message path drops Disabled DMs at
///   `gate_dm` line `Disabled → Drop`; the callback path mirrors it).
/// - `allow_from` hit → `true` (the message path's `Deliver` arm).
/// - otherwise (`Allowlist`/`Pairing` without an allow_from hit) → `false`
///   (the message path would `Drop`/`Pair` — neither is a route).
///
/// This is the callback-side sibling of `permissions::check_allowed`, scoped
/// to the `channel_state::Access` type the production update loop carries.
pub fn is_callback_allowed(access: &Access, sender_id: &str) -> bool {
    if access.dm_policy == DmPolicy::Disabled {
        return false;
    }
    access.allow_from.iter().any(|id| id == sender_id)
}

/// Outcome of `redeem_pairing_code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeemError {
    /// Code format invalid (length / charset). Same surface treatment as
    /// `Unknown` to avoid leaking which check failed.
    InvalidFormat,
    /// Code not present in `pending`. Note: caller's error message MUST
    /// NOT distinguish this from `InvalidFormat` per SEC-16.
    Unknown,
    /// Code present but `expires_at <= now`.
    Expired,
}

impl std::fmt::Display for RedeemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RedeemError::Expired => write!(f, "pairing code expired"),
            // SEC-16: same message for unknown / invalid format so timing
            // and string-content do not distinguish them.
            RedeemError::Unknown | RedeemError::InvalidFormat => {
                write!(f, "unknown or invalid pairing code")
            }
        }
    }
}

impl std::error::Error for RedeemError {}

/// Constant-time byte comparison. Equivalent to `a == b` semantically but
/// runs in O(len) regardless of where the first mismatch is — defeats
/// timing-side-channel attacks on the pending-code lookup (SEC-16).
///
/// Returns `true` iff both slices have the same length AND all bytes match.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Validate a candidate pairing code is well-formed: 6 lowercase hex chars.
/// Matches channel_state's `generate_pairing_code` format.
pub fn is_valid_pairing_format(code: &str) -> bool {
    if code.len() != 6 {
        return false;
    }
    code.bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Redeem a pairing code: look it up via constant-time compare across the
/// pending map, check expiry, move the sender into `allow_from`, and drop the
/// pending entry. Returns the sender_id (String) that was just added to allow_from on
/// success.
///
/// On failure the access struct is NOT mutated — caller can safely retry.
pub fn redeem_pairing_code(
    access: &mut Access,
    code: &str,
    now_ms: i64,
) -> std::result::Result<String, RedeemError> {
    if !is_valid_pairing_format(code) {
        return Err(RedeemError::InvalidFormat);
    }

    // Constant-time scan over every pending key. We collect the match in a
    // separate variable rather than short-circuiting on first hit so the
    // running time depends only on the total number of pending entries,
    // not on which entry matches.
    let code_bytes = code.as_bytes();
    let mut matched: Option<(String, PendingEntry)> = None;
    for (k, v) in access.pending.iter() {
        if constant_time_eq(k.as_bytes(), code_bytes) {
            matched = Some((k.clone(), v.clone()));
            // Do NOT break — keep scanning in constant time.
        }
    }

    let (matched_code, entry) = matched.ok_or(RedeemError::Unknown)?;

    if entry.expires_at <= now_ms {
        // Drop the expired entry as a courtesy so it doesn't pollute future
        // lookups, but reject the redeem.
        access.pending.remove(&matched_code);
        return Err(RedeemError::Expired);
    }

    // Move to allow_from. De-dup so re-pairing an already-allowed user
    // doesn't accumulate duplicate ids.
    let sender_id = entry.sender_id.clone();
    if !access.allow_from.contains(&sender_id) {
        access.allow_from.push(sender_id.clone());
    }
    access.pending.remove(&matched_code);
    Ok(sender_id)
}

/// Evaluate a Telegram DM under the current access policy. Mutates
/// `access.pending` when a new pairing code is issued (caller MUST save
/// access.json afterward to persist).
///
/// Mirrors `gate(ctx)` in server.ts:225-267 byte-for-byte semantically.
/// Group-chat handling is out of scope for the MVP port — group messages
/// route to `gate_group()` (TODO).
pub fn gate_dm(access: &mut Access, sender_id: &str, chat_id: &str, now: i64) -> GateAction {
    // server.ts:232: dmPolicy === 'disabled' → drop
    if access.dm_policy == DmPolicy::Disabled {
        return GateAction::Drop;
    }
    // server.ts:240: allowFrom hit → deliver
    if access.allow_from.iter().any(|id| id == sender_id) {
        return GateAction::Deliver;
    }
    // server.ts:241: dmPolicy === 'allowlist' AND not allowed → drop
    if access.dm_policy == DmPolicy::Allowlist {
        return GateAction::Drop;
    }
    // pairing mode below.
    // server.ts:244: existing pending code for this sender → resend (max 2 replies)
    for (code, p) in access.pending.iter_mut() {
        if p.sender_id == sender_id {
            if p.replies >= MAX_PAIRING_REPLIES {
                return GateAction::Drop;
            }
            p.replies += 1;
            return GateAction::Pair {
                code: code.clone(),
                is_resend: true,
            };
        }
    }
    // server.ts:254: cap at 3 — extra attempts silently dropped
    if access.pending.len() >= PENDING_CAP {
        return GateAction::Drop;
    }
    // server.ts:256: generate new code
    let code = match generate_pairing_code() {
        Ok(c) => c,
        Err(_) => return GateAction::Drop, // RNG failure — bail
    };
    access.pending.insert(
        code.clone(),
        PendingEntry {
            sender_id: sender_id.to_string(),
            chat_id: chat_id.to_string(),
            created_at: now,
            expires_at: now + PAIRING_CODE_TTL_MS,
            replies: 1,
        },
    );
    GateAction::Pair {
        code,
        is_resend: false,
    }
}

/// Format the pairing reply text matching server.ts:911-914 verbatim
/// (skill name swapped to `/claudebase:access pair` per port).
pub fn format_pair_reply(code: &str, is_resend: bool) -> String {
    let lead = if is_resend {
        "Still pending"
    } else {
        "Pairing required"
    };
    format!(
        "{lead} — run in Claude Code:\n\n/claudebase:access pair {code}"
    )
}

/// Load `.env` style file and return the value of `TELEGRAM_BOT_TOKEN` if
/// present. Trims whitespace + strips surrounding quotes. Returns
/// `Ok(None)` if the file doesn't exist OR the key is absent (caller
/// falls back to other sources). Lines starting with `#` are comments.
pub fn load_bot_token_from_env(path: &Path) -> Result<Option<String>> {
    let body = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow!(
                "failed to read .env at {}: {e}",
                path.display()
            ))
        }
    };
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("TELEGRAM_BOT_TOKEN=") {
            let value = rest.trim();
            let unquoted = value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(value);
            if unquoted.is_empty() {
                return Ok(None);
            }
            return Ok(Some(unquoted.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_is_six_hex() {
        let c = generate_pairing_code().expect("RNG works");
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|ch| ch.is_ascii_hexdigit()), "code={c}");
        assert!(c.chars().all(|ch| !ch.is_ascii_uppercase()), "code={c}");
    }

    #[test]
    fn gate_dm_emits_pair_for_unknown_sender() {
        let mut access = Access::default();
        let out = gate_dm(&mut access, "12345", "12345", 1000);
        match out {
            GateAction::Pair { code, is_resend } => {
                assert_eq!(code.len(), 6);
                assert!(!is_resend);
                assert!(access.pending.contains_key(&code));
            }
            other => panic!("expected Pair, got {other:?}"),
        }
    }

    #[test]
    fn gate_dm_resends_existing_code() {
        let mut access = Access::default();
        let first = gate_dm(&mut access, "12345", "12345", 1000);
        let code1 = match first {
            GateAction::Pair { code, .. } => code,
            other => panic!("expected Pair, got {other:?}"),
        };
        let second = gate_dm(&mut access, "12345", "12345", 2000);
        match second {
            GateAction::Pair { code, is_resend } => {
                assert_eq!(code, code1);
                assert!(is_resend);
            }
            other => panic!("expected Pair (resend), got {other:?}"),
        }
        // 3rd DM hits the MAX_PAIRING_REPLIES cap.
        let third = gate_dm(&mut access, "12345", "12345", 3000);
        matches!(third, GateAction::Drop);
    }

    #[test]
    fn gate_dm_drops_in_allowlist_mode_for_unknown() {
        let mut access = Access {
            dm_policy: DmPolicy::Allowlist,
            ..Default::default()
        };
        let out = gate_dm(&mut access, "99999", "99999", 1000);
        matches!(out, GateAction::Drop);
    }

    #[test]
    fn gate_dm_delivers_for_allowed_sender() {
        let mut access = Access {
            dm_policy: DmPolicy::Allowlist,
            allow_from: vec!["12345".to_string()],
            ..Default::default()
        };
        let out = gate_dm(&mut access, "12345", "12345", 1000);
        matches!(out, GateAction::Deliver);
    }

    #[test]
    fn gate_dm_drops_disabled_policy_completely() {
        let mut access = Access {
            dm_policy: DmPolicy::Disabled,
            allow_from: vec!["12345".to_string()],
            ..Default::default()
        };
        let out = gate_dm(&mut access, "12345", "12345", 1000);
        matches!(out, GateAction::Drop);
    }

    #[test]
    fn pending_cap_blocks_4th_unknown_sender() {
        let mut access = Access::default();
        for i in 0..PENDING_CAP {
            let id = format!("user{i}");
            let out = gate_dm(&mut access, &id, &id, 1000);
            matches!(out, GateAction::Pair { .. });
        }
        let extra = gate_dm(&mut access, "overflow", "overflow", 1000);
        matches!(extra, GateAction::Drop);
    }

    #[test]
    fn prune_expired_removes_only_expired() {
        let mut access = Access::default();
        access.pending.insert(
            "abc123".into(),
            PendingEntry {
                sender_id: "1".into(),
                chat_id: "1".into(),
                created_at: 0,
                expires_at: 500,
                replies: 1,
            },
        );
        access.pending.insert(
            "def456".into(),
            PendingEntry {
                sender_id: "2".into(),
                chat_id: "2".into(),
                created_at: 0,
                expires_at: 2000,
                replies: 1,
            },
        );
        let changed = prune_expired(&mut access, 1000);
        assert!(changed);
        assert_eq!(access.pending.len(), 1);
        assert!(access.pending.contains_key("def456"));
    }

    #[test]
    fn pair_reply_text_matches_official_format() {
        let s = format_pair_reply("abc123", false);
        assert_eq!(
            s,
            "Pairing required — run in Claude Code:\n\n/claudebase:access pair abc123"
        );
        let s = format_pair_reply("abc123", true);
        assert_eq!(
            s,
            "Still pending — run in Claude Code:\n\n/claudebase:access pair abc123"
        );
    }

    #[test]
    fn load_env_extracts_bot_token() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env");
        std::fs::write(&p, "TELEGRAM_BOT_TOKEN=12345:abcdef\n").unwrap();
        let tok = load_bot_token_from_env(&p).unwrap().unwrap();
        assert_eq!(tok, "12345:abcdef");
    }

    #[test]
    fn load_env_handles_quoted_values_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env");
        std::fs::write(
            &p,
            "# leading comment\nOTHER=ignored\nTELEGRAM_BOT_TOKEN=\"99:xyz\"\n",
        )
        .unwrap();
        let tok = load_bot_token_from_env(&p).unwrap().unwrap();
        assert_eq!(tok, "99:xyz");
    }

    #[test]
    fn load_env_returns_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nonexistent.env");
        assert!(load_bot_token_from_env(&p).unwrap().is_none());
    }

    #[test]
    fn access_roundtrips_via_json_with_string_ids() {
        let access = Access {
            dm_policy: DmPolicy::Pairing,
            allow_from: vec!["434566766".to_string()],
            groups: BTreeMap::new(),
            pending: {
                let mut m = BTreeMap::new();
                m.insert(
                    "101b06".to_string(),
                    PendingEntry {
                        sender_id: "434566766".into(),
                        chat_id: "434566766".into(),
                        created_at: 1779130319512,
                        expires_at: 1779133919512,
                        replies: 1,
                    },
                );
                m
            },
            mention_patterns: vec![],
        };
        let s = serde_json::to_string_pretty(&access).unwrap();
        // Skill-compatible: senderId/chatId/createdAt/expiresAt camelCase
        assert!(s.contains("\"dmPolicy\""));
        assert!(s.contains("\"allowFrom\""));
        assert!(s.contains("\"434566766\""));
        assert!(s.contains("\"senderId\""));
        assert!(s.contains("\"chatId\""));
        assert!(s.contains("\"createdAt\""));
        assert!(s.contains("\"expiresAt\""));
        let parsed: Access = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.allow_from, vec!["434566766".to_string()]);
        assert_eq!(parsed.pending.get("101b06").unwrap().sender_id, "434566766");
    }

    #[test]
    fn redeem_pairing_code_valid_code_succeeds() {
        let mut access = Access::default();
        let now = now_ms();
        access.pending.insert(
            "abc123".to_string(),
            PendingEntry {
                sender_id: "12345".to_string(),
                chat_id: "12345".to_string(),
                created_at: now,
                expires_at: now + PAIRING_CODE_TTL_MS,
                replies: 1,
            },
        );
        let sender_id = redeem_pairing_code(&mut access, "abc123", now).unwrap();
        assert_eq!(sender_id, "12345");
        assert!(access.allow_from.contains(&"12345".to_string()));
        assert!(!access.pending.contains_key("abc123"));
    }

    #[test]
    fn redeem_pairing_code_expired_rejects() {
        let mut access = Access::default();
        let now = now_ms();
        access.pending.insert(
            "abc123".to_string(),
            PendingEntry {
                sender_id: "12345".to_string(),
                chat_id: "12345".to_string(),
                created_at: now - PAIRING_CODE_TTL_MS - 1000,
                expires_at: now - 1,
                replies: 1,
            },
        );
        let err = redeem_pairing_code(&mut access, "abc123", now).unwrap_err();
        assert_eq!(err, RedeemError::Expired);
        assert!(!access.pending.contains_key("abc123"));
    }

    #[test]
    fn redeem_pairing_code_unknown_rejects() {
        let mut access = Access::default();
        let now = now_ms();
        let err = redeem_pairing_code(&mut access, "ffffff", now).unwrap_err();
        assert_eq!(err, RedeemError::Unknown);
    }

    #[test]
    fn redeem_pairing_code_invalid_format_rejects() {
        let mut access = Access::default();
        let now = now_ms();
        let err = redeem_pairing_code(&mut access, "BADCODE", now).unwrap_err();
        assert_eq!(err, RedeemError::InvalidFormat);
    }

    #[test]
    fn redeem_pairing_code_sec16_constant_time_display() {
        // SEC-16: Unknown and InvalidFormat return the same error message
        let unknown_err = RedeemError::Unknown;
        let invalid_err = RedeemError::InvalidFormat;
        assert_eq!(unknown_err.to_string(), invalid_err.to_string());
        assert_eq!(unknown_err.to_string(), "unknown or invalid pairing code");
        assert_ne!(unknown_err.to_string(), RedeemError::Expired.to_string());
    }

    #[test]
    fn redeem_pairing_code_deduplicates() {
        let mut access = Access {
            allow_from: vec!["12345".to_string()],
            ..Default::default()
        };
        let now = now_ms();
        access.pending.insert(
            "abc123".to_string(),
            PendingEntry {
                sender_id: "12345".to_string(),
                chat_id: "12345".to_string(),
                created_at: now,
                expires_at: now + PAIRING_CODE_TTL_MS,
                replies: 1,
            },
        );
        let sender_id = redeem_pairing_code(&mut access, "abc123", now).unwrap();
        assert_eq!(sender_id, "12345");
        // Should still be 1 (no duplicate added)
        assert_eq!(access.allow_from.iter().filter(|id| *id == &"12345".to_string()).count(), 1);
    }
}
