//! Permission-request flow — shared state + handlers.
//!
//! Claude Code emits `notifications/claude/channel/permission_request` to
//! ask the operator (through any external channel) whether a sensitive
//! tool call should be allowed. We forward those to every allowlisted
//! Telegram chat as a message with inline yes/no/more buttons; the
//! reply (button press or "yes/no XXXXX" text) comes back through the
//! polling loop and we emit `notifications/claude/channel/permission`
//! to CC.
//!
//! Mirrors TSX `server.ts:412-444` (request handler) +
//! `server.ts:731-786` (callback handler) +
//! `server.ts:84` (text-reply regex).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub struct PermissionDetails {
    pub tool_name: String,
    pub description: String,
    pub input_preview: String,
}

/// Shared store keyed by request_id. Cloned freely (Arc) — the inner
/// RwLock is taken for the brief duration of insert/get/remove.
#[derive(Clone, Default)]
pub struct PendingPermissions {
    inner: Arc<RwLock<HashMap<String, PermissionDetails>>>,
}

impl PendingPermissions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, request_id: String, details: PermissionDetails) {
        if let Ok(mut g) = self.inner.write() {
            g.insert(request_id, details);
        }
    }

    pub fn get(&self, request_id: &str) -> Option<PermissionDetails> {
        self.inner
            .read()
            .ok()
            .and_then(|g| g.get(request_id).cloned())
    }

    pub fn remove(&self, request_id: &str) -> Option<PermissionDetails> {
        self.inner
            .write()
            .ok()
            .and_then(|mut g| g.remove(request_id))
    }
}

/// Validates the text-reply form "yes XXXXX" / "no XXXXX" where XXXXX
/// is a request_id matching `[a-km-z]{5}`. Returns (behavior, request_id).
/// Behavior is `"allow"` or `"deny"`. Returns None when text doesn't
/// match the strict pattern. Mirrors TSX PERMISSION_REPLY_RE.
pub fn parse_permission_reply(text: &str) -> Option<(&'static str, String)> {
    let t = text.trim();
    // case-insensitive prefix
    let lower = t.to_lowercase();
    let (behavior, after): (&str, &str) = if let Some(rest) = lower.strip_prefix("yes ") {
        ("allow", &t[t.len() - rest.len()..])
    } else if let Some(rest) = lower.strip_prefix("y ") {
        ("allow", &t[t.len() - rest.len()..])
    } else if let Some(rest) = lower.strip_prefix("no ") {
        ("deny", &t[t.len() - rest.len()..])
    } else if let Some(rest) = lower.strip_prefix("n ") {
        ("deny", &t[t.len() - rest.len()..])
    } else {
        return None;
    };

    let code = after.trim();
    if code.len() != 5 {
        return None;
    }
    if !code
        .chars()
        .all(|c| matches!(c.to_ascii_lowercase(), 'a'..='k' | 'm'..='z'))
    {
        return None;
    }
    Some((behavior, code.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_yes_short() {
        assert_eq!(parse_permission_reply("y abcde"), Some(("allow", "abcde".into())));
    }

    #[test]
    fn parse_yes_long() {
        assert_eq!(parse_permission_reply("Yes abcde"), Some(("allow", "abcde".into())));
    }

    #[test]
    fn parse_no() {
        assert_eq!(parse_permission_reply("no abcde"), Some(("deny", "abcde".into())));
    }

    #[test]
    fn parse_rejects_chatter() {
        assert!(parse_permission_reply("yes please abcde").is_none());
        assert!(parse_permission_reply("yes abcde and more").is_none());
        assert!(parse_permission_reply("yes").is_none());
        assert!(parse_permission_reply("abcde").is_none());
    }

    #[test]
    fn parse_rejects_wrong_length() {
        assert!(parse_permission_reply("yes abcd").is_none());
        assert!(parse_permission_reply("yes abcdef").is_none());
    }

    #[test]
    fn parse_rejects_invalid_chars() {
        // 'l' is excluded from the alphabet to avoid 1/l confusion.
        assert!(parse_permission_reply("yes lbcde").is_none());
        assert!(parse_permission_reply("yes a8cde").is_none());
    }
}
