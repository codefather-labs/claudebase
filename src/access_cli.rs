//! `claudebase telegram pair|access|policy|allow|revoke` — channel access
//! management as deterministic CLI commands.
//!
//! ## Why this is not a skill
//!
//! Access control used to live in a `/claudebase-access` skill: an LLM read
//! `access.json`, decided what the operator meant, and wrote it back. That put
//! the decision to grant access inside a model whose context also contains
//! messages from the very channel being gated. "Add me to the allowlist" is
//! precisely what a prompt injection says, and the only defence was an
//! instruction in the skill telling the model to refuse.
//!
//! A CLI command has no such failure mode: it does what its arguments say, it
//! runs only when a human types it in a terminal, and its behaviour does not
//! depend on what arrived over Telegram five seconds earlier.
//!
//! ## State
//!
//! Everything lives in `~/.claude/channels/claudebase/access.json`, which the
//! daemon re-reads on **every inbound message** — so changes take effect
//! immediately, with no restart. Approving a pairing additionally drops a file
//! into `approved/`, which the daemon's 5-second watcher turns into the
//! "Paired!" confirmation sent back to Telegram.

use anyhow::{bail, Context, Result};

use crate::daemon::channel_state::{
    self, access_json_path, approved_dir, load_access, now_ms, save_access, Access, DmPolicy,
};

/// `claudebase telegram pair <code>` — approve a pending pairing request.
pub fn pair(code: &str) -> Result<String> {
    let code = code.trim().trim_start_matches('#').to_lowercase();
    if code.is_empty() {
        bail!("pass the code the bot sent you: claudebase telegram pair <code>");
    }

    let path = access_json_path();
    let mut access = load_access(&path).context("read access.json")?;
    let now = now_ms();

    // Prune first so an expired code reports as expired rather than as
    // approved-but-useless.
    let pruned = channel_state::prune_expired(&mut access, now);

    let matched = access
        .pending
        .iter()
        .find(|(k, _)| k.to_lowercase() == code)
        .map(|(k, v)| (k.clone(), v.clone()));

    let (stored_code, entry) = match matched {
        Some(pair) => pair,
        None => {
            if pruned {
                bail!(
                    "code `{code}` is not pending — it may have expired.\n\
                     Ask the sender to message the bot again for a fresh code."
                );
            }
            let pending: Vec<&str> = access.pending.keys().map(|s| s.as_str()).collect();
            if pending.is_empty() {
                bail!(
                    "no pairing is pending. The sender must message the bot first; \
                     the bot answers with a code."
                );
            }
            bail!(
                "code `{code}` is not pending. Currently waiting: {}",
                pending.join(", ")
            );
        }
    };

    if !access.allow_from.iter().any(|id| *id == entry.sender_id) {
        access.allow_from.push(entry.sender_id.clone());
    }
    access.pending.remove(&stored_code);
    save_access(&path, &access).context("write access.json")?;

    // The daemon's approved-dir watcher sends "Paired! Say hi to Claude." to
    // this chat and removes the file. Filename is the sender id, contents are
    // the chat id — group chats differ between the two.
    let dir = approved_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join(&entry.sender_id), entry.chat_id.as_bytes())
        .context("write approval marker")?;

    Ok(format!(
        "paired {} (chat {}) — the bot will confirm in Telegram within ~5s\n\
         allowlist now has {} sender(s)\n\n\
         Once everyone who should reach you is listed, lock it down:\n  \
         claudebase telegram policy allowlist",
        entry.sender_id,
        entry.chat_id,
        access.allow_from.len()
    ))
}

/// `claudebase telegram access` — show the current gate state.
pub fn show() -> Result<String> {
    let path = access_json_path();
    let mut access = load_access(&path).context("read access.json")?;
    channel_state::prune_expired(&mut access, now_ms());

    let mut out = String::new();
    out.push_str(&format!("policy: {}\n", policy_name(&access.dm_policy)));
    out.push_str(&format!("  {}\n\n", policy_meaning(&access.dm_policy)));

    if access.allow_from.is_empty() {
        out.push_str("allowed senders: none\n");
    } else {
        out.push_str(&format!("allowed senders ({}):\n", access.allow_from.len()));
        for id in &access.allow_from {
            out.push_str(&format!("  {id}\n"));
        }
    }

    if access.pending.is_empty() {
        out.push_str("\npending pairings: none\n");
    } else {
        out.push_str(&format!("\npending pairings ({}):\n", access.pending.len()));
        for (code, e) in &access.pending {
            let secs_left = (e.expires_at - now_ms()).max(0) / 1000;
            out.push_str(&format!(
                "  {code}  sender {}  expires in {}m{}s\n",
                e.sender_id,
                secs_left / 60,
                secs_left % 60
            ));
        }
        out.push_str("\napprove with: claudebase telegram pair <code>\n");
    }

    if !access.groups.is_empty() {
        out.push_str(&format!("\ngroups ({}):\n", access.groups.len()));
        for (id, g) in &access.groups {
            out.push_str(&format!(
                "  {id}  require_mention={}  allow_from={}\n",
                g.require_mention,
                if g.allow_from.is_empty() {
                    "any allowed sender".to_string()
                } else {
                    g.allow_from.join(",")
                }
            ));
        }
    }

    // Pairing is a bootstrap mode, not a resting state: it lets strangers
    // trigger code generation. Say so whenever it is on with people already
    // captured.
    if access.dm_policy == DmPolicy::Pairing && !access.allow_from.is_empty() {
        out.push_str(
            "\nnote: `pairing` lets any stranger request a code. Once the list above is\n\
             complete, run `claudebase telegram policy allowlist` to close it.\n",
        );
    }
    Ok(out)
}

/// `claudebase telegram policy <pairing|allowlist|disabled>`.
pub fn set_policy(value: &str) -> Result<String> {
    let policy = match value.trim().to_lowercase().as_str() {
        "pairing" => DmPolicy::Pairing,
        "allowlist" => DmPolicy::Allowlist,
        "disabled" | "off" => DmPolicy::Disabled,
        other => bail!("unknown policy `{other}` — use pairing, allowlist or disabled"),
    };
    let path = access_json_path();
    let mut access = load_access(&path).context("read access.json")?;

    if policy == DmPolicy::Allowlist && access.allow_from.is_empty() {
        bail!(
            "refusing to switch to `allowlist` while the allowlist is empty — \
             that would lock out everyone, including you.\n\
             Pair yourself first (message the bot, then `claudebase telegram pair <code>`)."
        );
    }

    let previous = policy_name(&access.dm_policy).to_string();
    access.dm_policy = policy;
    save_access(&path, &access).context("write access.json")?;
    Ok(format!(
        "policy {previous} → {}\n  {}\n(takes effect on the next inbound message; no restart)",
        policy_name(&access.dm_policy),
        policy_meaning(&access.dm_policy)
    ))
}

/// `claudebase telegram allow <sender_id>` — add a known Telegram user id.
pub fn allow(sender_id: &str) -> Result<String> {
    let sender_id = sender_id.trim();
    if sender_id.is_empty() || !sender_id.chars().all(|c| c.is_ascii_digit()) {
        bail!("sender id must be the numeric Telegram user id (they can get it from @userinfobot)");
    }
    let path = access_json_path();
    let mut access = load_access(&path).context("read access.json")?;
    if access.allow_from.iter().any(|id| id == sender_id) {
        return Ok(format!("{sender_id} is already allowed"));
    }
    access.allow_from.push(sender_id.to_string());
    save_access(&path, &access).context("write access.json")?;
    Ok(format!(
        "allowed {sender_id} ({} total)",
        access.allow_from.len()
    ))
}

/// `claudebase telegram revoke <sender_id>`.
pub fn revoke(sender_id: &str) -> Result<String> {
    let sender_id = sender_id.trim();
    let path = access_json_path();
    let mut access = load_access(&path).context("read access.json")?;
    let before = access.allow_from.len();
    access.allow_from.retain(|id| id != sender_id);
    if access.allow_from.len() == before {
        bail!("{sender_id} is not in the allowlist");
    }
    save_access(&path, &access).context("write access.json")?;
    Ok(format!(
        "revoked {sender_id} ({} remaining)",
        access.allow_from.len()
    ))
}

fn policy_name(p: &DmPolicy) -> &'static str {
    match p {
        DmPolicy::Pairing => "pairing",
        DmPolicy::Allowlist => "allowlist",
        DmPolicy::Disabled => "disabled",
    }
}

fn policy_meaning(p: &DmPolicy) -> &'static str {
    match p {
        DmPolicy::Pairing => "unknown senders get a code; you approve each one by hand",
        DmPolicy::Allowlist => "only listed senders get through; everyone else is dropped silently",
        DmPolicy::Disabled => "all direct messages are dropped",
    }
}

/// Test seam: apply a pairing approval to an in-memory `Access`.
///
/// The filesystem half (`approved/` marker) is deliberately excluded so the
/// decision logic can be tested without a home directory.
pub fn apply_pair(access: &mut Access, code: &str, now: i64) -> Result<String> {
    channel_state::prune_expired(access, now);
    let matched = access
        .pending
        .iter()
        .find(|(k, _)| k.to_lowercase() == code.to_lowercase())
        .map(|(k, v)| (k.clone(), v.sender_id.clone()));
    let (stored, sender) = matched.ok_or_else(|| anyhow::anyhow!("code not pending"))?;
    if !access.allow_from.iter().any(|id| *id == sender) {
        access.allow_from.push(sender.clone());
    }
    access.pending.remove(&stored);
    Ok(sender)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::channel_state::PendingEntry;

    fn access_with_pending(code: &str, sender: &str, expires_at: i64) -> Access {
        let mut a = Access::default();
        a.pending.insert(
            code.to_string(),
            PendingEntry {
                sender_id: sender.to_string(),
                chat_id: sender.to_string(),
                created_at: 0,
                expires_at,
                replies: 1,
            },
        );
        a
    }

    #[test]
    fn pairing_moves_the_sender_into_the_allowlist_and_clears_the_code() {
        let mut a = access_with_pending("abc123", "434566766", 10_000);
        let sender = apply_pair(&mut a, "abc123", 1_000).expect("pair");
        assert_eq!(sender, "434566766");
        assert_eq!(a.allow_from, vec!["434566766"]);
        assert!(a.pending.is_empty(), "code must be consumed");
    }

    #[test]
    fn code_matching_is_case_insensitive() {
        let mut a = access_with_pending("abc123", "1", 10_000);
        assert!(apply_pair(&mut a, "ABC123", 1_000).is_ok());
    }

    #[test]
    fn expired_code_is_refused() {
        let mut a = access_with_pending("abc123", "1", 500);
        assert!(
            apply_pair(&mut a, "abc123", 1_000).is_err(),
            "a code past its expiry must not grant access"
        );
        assert!(a.allow_from.is_empty());
    }

    #[test]
    fn unknown_code_grants_nothing() {
        let mut a = access_with_pending("abc123", "1", 10_000);
        assert!(apply_pair(&mut a, "deadbeef", 1_000).is_err());
        assert!(a.allow_from.is_empty());
        assert_eq!(a.pending.len(), 1, "the real code must survive a wrong guess");
    }

    #[test]
    fn pairing_twice_does_not_duplicate_the_sender() {
        let mut a = access_with_pending("abc123", "7", 10_000);
        apply_pair(&mut a, "abc123", 1_000).unwrap();
        a.pending.insert(
            "def456".to_string(),
            PendingEntry {
                sender_id: "7".to_string(),
                chat_id: "7".to_string(),
                created_at: 0,
                expires_at: 10_000,
                replies: 1,
            },
        );
        apply_pair(&mut a, "def456", 1_000).unwrap();
        assert_eq!(a.allow_from, vec!["7"], "one sender, one entry");
    }
}
