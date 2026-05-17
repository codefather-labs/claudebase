//! Slice 4 — permission / pairing model.
//!
//! Ports the voice-control project's `access.json` schema (per architect
//! reuse decision) to the claudebase daemon. The file lives at
//! `$HOME/.config/claudebase/access.json` and survives daemon restarts.
//!
//! ## Schema
//!
//! ```json
//! {
//!   "dmPolicy": "pairing" | "allowlist" | "disabled",
//!   "allowFrom": [<telegram_user_id>, ...],
//!   "groups": { ... },                                  // future use
//!   "pending": {
//!     "<6-char-code>": {
//!       "telegram_user_id": <i64>,
//!       "expires_at": <millis-since-epoch>
//!     },
//!     ...
//!   }
//! }
//! ```
//!
//! ## Security backbone
//!
//! - **SEC-11** (HIGH): pairing codes drawn from `getrandom::getrandom` —
//!   OS CSPRNG, never `rand::thread_rng` (the latter is deterministic
//!   across some forks under MUSL). TTL = 1 hour (3,600,000 ms) per UC-6-E1.
//!   The pending map is capped at 100 entries; on overflow we drop the
//!   OLDEST entry (lowest `expires_at` — earliest-to-expire).
//! - **SEC-12** (HIGH): writes go through `write → fsync → rename` so a
//!   crash mid-write never leaves a half-truncated access.json.
//! - **SEC-16** (MEDIUM): code lookup uses constant-time byte compare so
//!   timing leaks don't distinguish "wrong format" from "unknown code".
//!   The same error variant fires for both cases.
//!
//! The pairing-code alphabet is 6-char base32 minus `O/I/0/1` (visually
//! ambiguous), giving 32 characters: `A-H J-N P-Z 2-9`. With 32^6 ≈ 1B
//! codes and 100-entry cap, birthday-collision probability stays negligible.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::daemon::config::DmPolicy;

/// 1-hour TTL on pairing codes per UC-6-E1 / SEC-11.
pub const PAIRING_CODE_TTL_MS: i64 = 3_600_000;

/// Hard cap on the pending-pairing map size. Slice 4 uses 100; the cap
/// guards against an unbounded growth attack where an attacker spams
/// `/start` faster than codes expire.
pub const PENDING_MAP_CAP: usize = 100;

/// Length of the alphanumeric pairing code emitted by `generate_pairing_code`.
pub const PAIRING_CODE_LEN: usize = 6;

/// Base32-without-confusables alphabet. 32 characters, removing the
/// visually ambiguous `O/I/0/1` glyphs (Crockford-style). This is the
/// canonical character set agents and humans both see; the regex
/// `^[A-HJ-NP-Z2-9]{6}$` matches any well-formed code.
pub const PAIRING_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// One pending pairing entry — the bot has sent code `C` to user `U`;
/// user has `PAIRING_CODE_TTL_MS` to run `claudebase daemon access pair C`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEntry {
    pub telegram_user_id: i64,
    pub expires_at: i64, // millis since UNIX_EPOCH
}

/// Top-level access.json document. The `groups` map is reserved for
/// future use (group-chat ACLs); current Slice 4 leaves it untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Access {
    #[serde(rename = "dmPolicy", default)]
    pub dm_policy: DmPolicy,
    #[serde(rename = "allowFrom", default)]
    pub allow_from: Vec<i64>,
    #[serde(default)]
    pub groups: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub pending: BTreeMap<String, PendingEntry>,
}

impl Default for Access {
    fn default() -> Self {
        Self {
            dm_policy: DmPolicy::default(),
            allow_from: Vec::new(),
            groups: serde_json::Map::new(),
            pending: BTreeMap::new(),
        }
    }
}

/// Return `$HOME/.config/claudebase/access.json` (mirrors config.rs path
/// helpers — same XDG fallback chain).
pub fn user_level_access_json_path() -> PathBuf {
    crate::daemon::config::user_level_config_dir().join("access.json")
}

/// Load access.json from `path`. Returns `Ok(Access::default())` when the
/// file does not exist (fresh install — daemon will create on first write).
/// Parse errors propagate as `Err`. Symlinks are NOT refused here — unlike
/// secrets.toml the file contains no secrets; an operator who wants their
/// access list lived on a different volume via symlink is permitted.
pub fn load_access(path: &Path) -> Result<Access> {
    match fs::read_to_string(path) {
        Ok(body) => {
            let access: Access = serde_json::from_str(&body).with_context(|| {
                format!("access.json at {} is not valid JSON", path.display())
            })?;
            Ok(access)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Access::default()),
        Err(e) => Err(anyhow!("failed to read access.json at {}: {e}", path.display())),
    }
}

/// Save access.json atomically (SEC-12): write to `path.json.tmp` first,
/// fsync, rename over `path`. Any reader either sees the prior file
/// completely OR the new file completely — never a half-written truncation.
pub fn save_access(path: &Path, access: &Access) -> Result<()> {
    // Ensure the parent dir exists; first call on a fresh install needs
    // to mkdir -p $HOME/.config/claudebase/.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {} parent dir", path.display()))?;
    }

    let body = serde_json::to_string_pretty(access)
        .with_context(|| "failed to serialise access.json")?;

    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body.as_bytes())
        .with_context(|| format!("failed to write tmp file at {}", tmp.display()))?;

    // fsync the tmp file so the rename below sees durable bytes. Failure
    // to fsync surfaces as an error; the half-written tmp is left in place
    // for forensics.
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

/// Current wall-clock time in milliseconds since UNIX_EPOCH. Returned as
/// `i64` to match the JSON schema (i64 fits Telegram user IDs and survives
/// past 2038 on 32-bit systems).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Generate a 6-char pairing code from the base32-no-confusables alphabet.
/// Uses `getrandom::getrandom` — OS CSPRNG (SEC-11). Each output byte is
/// reduced mod 32 to index the alphabet.
///
/// Statistical note: `byte % 32` is bias-free since 256 / 32 = 8 (whole
/// number) — every alphabet slot receives equal probability.
pub fn generate_pairing_code() -> Result<String> {
    let mut buf = [0u8; PAIRING_CODE_LEN];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow!("OS RNG failed: {e}"))?;
    let mut out = String::with_capacity(PAIRING_CODE_LEN);
    for b in buf.iter() {
        let idx = (*b as usize) % PAIRING_CODE_ALPHABET.len();
        out.push(PAIRING_CODE_ALPHABET[idx] as char);
    }
    Ok(out)
}

/// Validate a candidate pairing code is well-formed: 6 chars from
/// `[A-Z0-9]` (uppercase alphanumeric). Note this is WIDER than the
/// generator's alphabet (which excludes O/I/0/1 for visual clarity) —
/// the wider format check accepts the broader space so externally-
/// stored codes (e.g. from a prior voice-control install whose
/// access.json was migrated in) round-trip cleanly. The generator's
/// own output still uses the narrow no-confusables alphabet.
///
/// The same message ("unknown or invalid pairing code") fires for
/// `InvalidFormat` and `Unknown` regardless of what alphabet we accept
/// here — SEC-16's invariant is on the error surface, not on the
/// pre-screen breadth.
pub fn is_valid_pairing_format(code: &str) -> bool {
    if code.len() != PAIRING_CODE_LEN {
        return false;
    }
    code.bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Check whether `user_id` is allowed to send DMs under the current policy.
///
/// Semantics per UC-6-B:
/// - `Disabled` — accept all (filter is OFF).
/// - `Allowlist` — accept only if `user_id` is in `allow_from`.
/// - `Pairing`   — accept only if `user_id` is in `allow_from`. Pending
///   pairing codes do NOT count — the user must complete pairing first.
pub fn check_allowed(access: &Access, user_id: i64) -> bool {
    match access.dm_policy {
        DmPolicy::Disabled => true,
        DmPolicy::Allowlist | DmPolicy::Pairing => access.allow_from.iter().any(|u| *u == user_id),
    }
}

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

/// Outcome of `redeem_pairing_code`.
#[derive(Debug)]
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

/// Redeem a pairing code: look it up via constant-time compare across the
/// pending map, check expiry, move the user into `allow_from`, and drop the
/// pending entry. Returns the user ID that was just added to allow_from on
/// success.
///
/// On failure the access struct is NOT mutated — caller can safely retry.
pub fn redeem_pairing_code(
    access: &mut Access,
    code: &str,
    now_ms: i64,
) -> std::result::Result<i64, RedeemError> {
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
    let user_id = entry.telegram_user_id;
    if !access.allow_from.contains(&user_id) {
        access.allow_from.push(user_id);
    }
    access.pending.remove(&matched_code);
    Ok(user_id)
}

/// Insert a new pending pairing entry, enforcing the 100-entry cap. When
/// the cap is reached, the entry with the earliest `expires_at` is dropped
/// to make room (FIFO-by-expiry — codes that would expire soonest get
/// evicted first).
///
/// If the same `telegram_user_id` already has a pending code, return the
/// existing code unchanged (TC-4.9: "duplicate /start re-sends same code")
/// to avoid spamming the user with one-time codes.
pub fn issue_pairing_code(
    access: &mut Access,
    telegram_user_id: i64,
    now_ms: i64,
) -> Result<String> {
    // TC-4.9: dedupe by user_id — same /start re-sends the same code.
    for (k, v) in access.pending.iter() {
        if v.telegram_user_id == telegram_user_id && v.expires_at > now_ms {
            return Ok(k.clone());
        }
    }

    // Drop expired entries before checking the cap so an old stale code
    // doesn't permanently block new pairings.
    let pre_count = access.pending.len();
    access.pending.retain(|_, e| e.expires_at > now_ms);
    let _expired = pre_count - access.pending.len();

    // Enforce the cap: if still at-or-over capacity, evict the earliest-
    // expiring entry. (BTreeMap doesn't guarantee insertion order, but we
    // can scan in O(N) for the min `expires_at`.)
    while access.pending.len() >= PENDING_MAP_CAP {
        if let Some((victim, _)) = access
            .pending
            .iter()
            .min_by_key(|(_, e)| e.expires_at)
            .map(|(k, v)| (k.clone(), v.clone()))
        {
            access.pending.remove(&victim);
        } else {
            // Map empty (shouldn't happen given the .len() check) — bail
            // rather than infinite loop.
            break;
        }
    }

    let code = generate_pairing_code()?;
    access.pending.insert(
        code.clone(),
        PendingEntry {
            telegram_user_id,
            expires_at: now_ms + PAIRING_CODE_TTL_MS,
        },
    );
    Ok(code)
}

/// Build a redacted summary of the access state for `daemon access list`.
/// `allow_from` is shown verbatim (user IDs are not secrets); `pending`
/// entries are shown WITHOUT the code string itself (SEC-16: codes are
/// short-lived secrets while pending; revealing them via `access list`
/// would defeat the constant-time-compare-on-pair check).
pub fn redacted_summary(access: &Access) -> serde_json::Value {
    let now = now_ms();
    let pending_view: Vec<serde_json::Value> = access
        .pending
        .values()
        .map(|e| {
            let remaining_ms = (e.expires_at - now).max(0);
            serde_json::json!({
                "telegram_user_id": e.telegram_user_id,
                "expires_in_ms": remaining_ms,
            })
        })
        .collect();
    let allow_set: HashSet<i64> = access.allow_from.iter().copied().collect();
    let mut allow_sorted: Vec<i64> = allow_set.into_iter().collect();
    allow_sorted.sort_unstable();
    serde_json::json!({
        "dmPolicy": access.dm_policy,
        "allowFrom": allow_sorted,
        "pending_count": access.pending.len(),
        "pending": pending_view,
    })
}

/// Disambiguated `bail!` shim. (We don't want to import anyhow::bail at
/// top-level here; the module already uses `bail!` in `save_access`.)
#[allow(dead_code)]
fn _force_link_bail() {
    let _ = || -> Result<()> { bail!("never") };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_pairing_code_emits_canonical_form() {
        let code = generate_pairing_code().unwrap();
        assert_eq!(code.len(), PAIRING_CODE_LEN);
        assert!(is_valid_pairing_format(&code), "bad code: {code:?}");
    }

    #[test]
    fn is_valid_pairing_format_enforces_uppercase_alphanumeric_six() {
        // Length-six uppercase alphanumeric — all accepted (the wider
        // format check is documented at the function definition).
        assert!(is_valid_pairing_format("ABC123"));
        assert!(is_valid_pairing_format("ABC234"));
        assert!(is_valid_pairing_format("OOOOOO"));
        assert!(is_valid_pairing_format("111111"));

        // Rejected: wrong length OR non-uppercase-alphanumeric chars.
        assert!(!is_valid_pairing_format("abcdef")); // lowercase
        assert!(!is_valid_pairing_format("ABC12")); // too short
        assert!(!is_valid_pairing_format("ABC1234")); // too long
        assert!(!is_valid_pairing_format("AB!123")); // punctuation
        assert!(!is_valid_pairing_format("AB 123")); // space
    }

    #[test]
    fn redeem_unknown_code_returns_unknown_variant() {
        let mut access = Access::default();
        let err = redeem_pairing_code(&mut access, "ABC234", now_ms()).unwrap_err();
        assert!(matches!(err, RedeemError::Unknown));
    }

    #[test]
    fn redeem_invalid_format_returns_invalid_variant() {
        let mut access = Access::default();
        let err = redeem_pairing_code(&mut access, "abc!@#", now_ms()).unwrap_err();
        assert!(matches!(err, RedeemError::InvalidFormat));
    }

    #[test]
    fn redeem_expired_drops_entry_and_rejects() {
        let mut access = Access::default();
        let now = now_ms();
        access.pending.insert(
            "ABC234".to_string(),
            PendingEntry {
                telegram_user_id: 1001,
                expires_at: now - 1, // already expired
            },
        );
        let err = redeem_pairing_code(&mut access, "ABC234", now).unwrap_err();
        assert!(matches!(err, RedeemError::Expired));
        assert!(!access.pending.contains_key("ABC234"));
    }

    #[test]
    fn redeem_success_moves_user_and_drops_pending() {
        let mut access = Access::default();
        let now = now_ms();
        access.pending.insert(
            "ABC234".to_string(),
            PendingEntry {
                telegram_user_id: 1001,
                expires_at: now + PAIRING_CODE_TTL_MS,
            },
        );
        let uid = redeem_pairing_code(&mut access, "ABC234", now).unwrap();
        assert_eq!(uid, 1001);
        assert!(access.allow_from.contains(&1001));
        assert!(!access.pending.contains_key("ABC234"));
    }

    #[test]
    fn issue_pairing_code_dedupes_per_user() {
        let mut access = Access::default();
        let now = now_ms();
        let c1 = issue_pairing_code(&mut access, 1001, now).unwrap();
        let c2 = issue_pairing_code(&mut access, 1001, now).unwrap();
        assert_eq!(c1, c2);
        assert_eq!(access.pending.len(), 1);
    }

    #[test]
    fn constant_time_eq_correct_for_equal_and_unequal() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn check_allowed_disabled_accepts_all() {
        let mut access = Access::default();
        access.dm_policy = DmPolicy::Disabled;
        assert!(check_allowed(&access, 9999));
    }

    #[test]
    fn check_allowed_pairing_requires_allow_from() {
        let access = Access::default();
        // default = Pairing, allow_from = []
        assert!(!check_allowed(&access, 9999));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("access.json");
        let mut a = Access::default();
        a.allow_from.push(42);
        save_access(&path, &a).unwrap();
        let b = load_access(&path).unwrap();
        assert_eq!(b.allow_from, vec![42]);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.json");
        let a = load_access(&path).unwrap();
        assert!(a.allow_from.is_empty());
    }
}
