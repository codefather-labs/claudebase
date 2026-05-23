//! `access.json` read/write + expiry pruning. Schema-equivalent to TSX
//! `server.ts:108-225` so both implementations can share the file.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level access state. Stored as `access.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Access {
    #[serde(rename = "dmPolicy")]
    pub dm_policy: DmPolicy,
    #[serde(rename = "allowFrom", default)]
    pub allow_from: Vec<String>,
    #[serde(default)]
    pub groups: HashMap<String, GroupConfig>,
    #[serde(default)]
    pub pending: HashMap<String, PendingPairing>,
    /// Optional ack emoji to send as reaction on each accepted inbound msg.
    /// Must be on Telegram's whitelist or it's silently dropped.
    #[serde(rename = "ackReaction", default, skip_serializing_if = "Option::is_none")]
    pub ack_reaction: Option<String>,
    /// Additional regex patterns that count as a bot mention in groups
    /// (TSX server.ts:300-329). Optional; defaults to empty.
    #[serde(rename = "mentionPatterns", default, skip_serializing_if = "Vec::is_empty")]
    pub mention_patterns: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    Pairing,
    Allowlist,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupConfig {
    #[serde(default = "default_true")]
    #[serde(rename = "requireMention")]
    pub require_mention: bool,
    /// Restrict which group members can trigger the bot. Empty / None = anyone.
    #[serde(rename = "allowFrom", default, skip_serializing_if = "Option::is_none")]
    pub allow_from: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPairing {
    #[serde(rename = "senderId")]
    pub sender_id: String,
    #[serde(rename = "chatId", default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(rename = "displayName", default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(rename = "createdAtMs", alias = "createdAt", default)]
    pub created_at_ms: i64,
    /// Unix ms timestamp when the pairing code expires.
    #[serde(rename = "expiresAtMs", alias = "expiresAt", default)]
    pub expires_at_ms: i64,
    /// How many reminder replies the bot has sent for this pending code.
    /// Cap at 2 so a stuck sender can't spam the bot's outbound queue.
    #[serde(default = "default_one")]
    pub replies: i32,
}

fn default_true() -> bool {
    true
}

fn default_one() -> i32 {
    1
}

impl Default for Access {
    /// Mirror TSX `defaultAccess()` — `pairing` policy, empty allowlist.
    fn default() -> Self {
        Access {
            dm_policy: DmPolicy::Pairing,
            allow_from: Vec::new(),
            groups: HashMap::new(),
            pending: HashMap::new(),
            ack_reaction: None,
            mention_patterns: Vec::new(),
        }
    }
}

/// Read `access.json`. Missing file or parse failure → defaults.
pub fn load() -> Access {
    let path = crate::state::access_file();
    let Ok(content) = std::fs::read_to_string(&path) else {
        tracing::debug!(?path, "access.json missing — using defaults");
        return Access::default();
    };
    match serde_json::from_str::<Access>(&content) {
        Ok(mut a) => {
            prune_expired(&mut a);
            a
        }
        Err(e) => {
            tracing::warn!(?path, error = %e, "access.json parse failed — using defaults");
            Access::default()
        }
    }
}

/// Atomic write via tempfile + rename. Mirrors TSX `saveAccess`.
pub fn save(access: &Access) -> std::io::Result<()> {
    let path = crate::state::access_file();
    let dir = crate::state::state_dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("access.json.{}.tmp", std::process::id()));
    let json = serde_json::to_string_pretty(access).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Drop expired pending pairings. Returns `true` if any were pruned.
pub fn prune_expired(access: &mut Access) -> bool {
    let now = now_ms();
    let before = access.pending.len();
    access.pending.retain(|_, p| p.expires_at_ms > now);
    access.pending.len() != before
}

/// Unix epoch milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_pairing_with_empty_allowlist() {
        let a = Access::default();
        assert_eq!(a.dm_policy, DmPolicy::Pairing);
        assert!(a.allow_from.is_empty());
        assert!(a.groups.is_empty());
        assert!(a.pending.is_empty());
        assert!(a.ack_reaction.is_none());
        assert!(a.mention_patterns.is_empty());
    }

    #[test]
    fn parses_real_access_json() {
        // Matches the actual shape of the operator's access.json this session.
        let json = r#"{
            "dmPolicy": "pairing",
            "allowFrom": ["434566766"],
            "groups": {},
            "pending": {}
        }"#;
        let a: Access = serde_json::from_str(json).unwrap();
        assert_eq!(a.dm_policy, DmPolicy::Pairing);
        assert_eq!(a.allow_from, vec!["434566766"]);
    }

    #[test]
    fn parses_pending_with_legacy_alias() {
        // TSX uses `createdAt` / `expiresAt`. Our canonical names are `*Ms`
        // but the alias must accept both so an existing access.json works.
        let json = r#"{
            "dmPolicy": "pairing",
            "allowFrom": [],
            "groups": {},
            "pending": {
                "abc123": {
                    "senderId": "111",
                    "chatId": "111",
                    "createdAt": 1000000000,
                    "expiresAt": 9999999999999,
                    "replies": 2
                }
            }
        }"#;
        let a: Access = serde_json::from_str(json).unwrap();
        let p = a.pending.get("abc123").unwrap();
        assert_eq!(p.sender_id, "111");
        assert_eq!(p.created_at_ms, 1000000000);
        assert_eq!(p.expires_at_ms, 9999999999999);
        assert_eq!(p.replies, 2);
    }

    #[test]
    fn parses_group_config_with_allow_from() {
        let json = r#"{
            "dmPolicy": "allowlist",
            "allowFrom": [],
            "groups": {
                "-1001234567890": {
                    "requireMention": false,
                    "allowFrom": ["111", "222"]
                }
            },
            "pending": {}
        }"#;
        let a: Access = serde_json::from_str(json).unwrap();
        let g = a.groups.get("-1001234567890").unwrap();
        assert!(!g.require_mention);
        assert_eq!(g.allow_from.as_deref(), Some(&["111".to_string(), "222".to_string()][..]));
    }

    #[test]
    fn prune_drops_expired() {
        let mut a = Access::default();
        a.pending.insert(
            "ABC123".to_string(),
            PendingPairing {
                sender_id: "111".to_string(),
                chat_id: None,
                display_name: None,
                created_at_ms: 0,
                expires_at_ms: 0, // long expired
                replies: 1,
            },
        );
        assert!(prune_expired(&mut a));
        assert!(a.pending.is_empty());
    }
}
