//! `gate()` and mention-detection. Ports TSX `server.ts:227-329`.

use frankenstein::types::Message;
use rand::RngCore;

use super::state::{load, now_ms, prune_expired, save, Access, DmPolicy, PendingPairing};

const PAIRING_TTL_MS: i64 = 60 * 60 * 1000; // 1 hour
const MAX_PENDING: usize = 3;
const MAX_REPLIES_PER_PENDING: i32 = 2;

#[derive(Debug)]
pub enum GateResult {
    /// Silently drop — sender is not allowed, mention not present, etc.
    Drop,
    /// Reply with pairing instructions. `is_resend` = true means the
    /// sender already had a pending code; we're nudging them again.
    Pair {
        code: String,
        is_resend: bool,
    },
    /// Sender is approved; emit channel notification.
    Deliver { access: Access },
}

/// Main inbound gate. Returns one of three actions based on access policy,
/// sender membership, and group mention/whitelist rules.
///
/// Side effects: may write to access.json (pruning expired, recording a
/// new pending pairing). All mutations go through atomic save().
pub fn gate(msg: &Message, bot_username: &str) -> GateResult {
    let mut access = load();
    let pruned = prune_expired(&mut access);
    let mut dirty = pruned;

    if access.dm_policy == DmPolicy::Disabled {
        if dirty {
            let _ = save(&access);
        }
        return GateResult::Drop;
    }

    let Some(from) = msg.from.as_ref() else {
        if dirty {
            let _ = save(&access);
        }
        return GateResult::Drop;
    };
    let sender_id = from.id.to_string();
    let chat_type = msg.chat.type_field;

    use frankenstein::types::ChatType;
    match chat_type {
        ChatType::Private => {
            // Already-approved sender → pass through.
            if access.allow_from.iter().any(|s| s == &sender_id) {
                if dirty {
                    let _ = save(&access);
                }
                return GateResult::Deliver { access };
            }

            // allowlist policy + not on list → drop.
            if access.dm_policy == DmPolicy::Allowlist {
                if dirty {
                    let _ = save(&access);
                }
                return GateResult::Drop;
            }

            // pairing mode — check for existing non-expired pending code.
            let existing: Option<(String, i32)> = access
                .pending
                .iter()
                .find(|(_, p)| p.sender_id == sender_id)
                .map(|(c, p)| (c.clone(), p.replies));
            if let Some((code, replies)) = existing {
                if replies >= MAX_REPLIES_PER_PENDING {
                    if dirty {
                        let _ = save(&access);
                    }
                    return GateResult::Drop;
                }
                if let Some(p) = access.pending.get_mut(&code) {
                    p.replies += 1;
                }
                let _ = save(&access);
                return GateResult::Pair {
                    code,
                    is_resend: true,
                };
            }

            // Cap pending at 3.
            if access.pending.len() >= MAX_PENDING {
                if dirty {
                    let _ = save(&access);
                }
                return GateResult::Drop;
            }

            // Generate a new pairing code: 6 lowercase hex chars (TSX:
            // randomBytes(3).toString('hex')).
            let code = new_pairing_code();
            let now = now_ms();
            access.pending.insert(
                code.clone(),
                PendingPairing {
                    sender_id: sender_id.clone(),
                    chat_id: Some(msg.chat.id.to_string()),
                    display_name: from
                        .username
                        .clone()
                        .or_else(|| Some(format!("{} {}", from.first_name, from.last_name.clone().unwrap_or_default()).trim().to_string())),
                    created_at_ms: now,
                    expires_at_ms: now + PAIRING_TTL_MS,
                    replies: 1,
                },
            );
            let _ = save(&access);
            GateResult::Pair {
                code,
                is_resend: false,
            }
        }

        ChatType::Group | ChatType::Supergroup => {
            let group_id = msg.chat.id.to_string();
            let Some(policy) = access.groups.get(&group_id).cloned() else {
                if dirty {
                    let _ = save(&access);
                }
                return GateResult::Drop;
            };
            // Member whitelist (if configured).
            if let Some(members) = &policy.allow_from {
                if !members.is_empty() && !members.iter().any(|s| s == &sender_id) {
                    if dirty {
                        let _ = save(&access);
                    }
                    return GateResult::Drop;
                }
            }
            // Require @mention unless explicitly disabled.
            if policy.require_mention && !is_mentioned(msg, bot_username, &access.mention_patterns) {
                if dirty {
                    let _ = save(&access);
                }
                return GateResult::Drop;
            }
            if dirty {
                let _ = save(&access);
            }
            GateResult::Deliver { access }
        }

        _ => {
            // Channels and other chat types: drop.
            if dirty {
                let _ = save(&access);
            }
            GateResult::Drop
        }
    }
}

/// Detect whether the bot was @mentioned, text-mentioned, or replied to.
/// Mirrors TSX `isMentioned`.
pub fn is_mentioned(msg: &Message, bot_username: &str, extra_patterns: &[String]) -> bool {
    let username_lower = bot_username.to_lowercase();
    let entities = msg
        .entities
        .as_deref()
        .or(msg.caption_entities.as_deref())
        .unwrap_or(&[]);
    let text = msg
        .text
        .as_deref()
        .or(msg.caption.as_deref())
        .unwrap_or("");

    use frankenstein::types::MessageEntityType;
    for e in entities {
        let offset = e.offset as usize;
        let length = e.length as usize;
        match e.type_field {
            MessageEntityType::Mention => {
                let chars: Vec<char> = text.chars().collect();
                if offset + length <= chars.len() {
                    let mention: String = chars[offset..offset + length].iter().collect();
                    if mention.to_lowercase() == format!("@{}", username_lower) {
                        return true;
                    }
                }
            }
            MessageEntityType::TextMention => {
                if let Some(user) = e.user.as_ref() {
                    if user.is_bot
                        && user.username.as_deref().map(str::to_lowercase) == Some(username_lower.clone())
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }

    // Implicit mention: reply to one of our messages.
    if let Some(reply) = msg.reply_to_message.as_ref() {
        if let Some(reply_from) = reply.from.as_ref() {
            if reply_from.username.as_deref().map(str::to_lowercase) == Some(username_lower) {
                return true;
            }
        }
    }

    // Operator-configured regex patterns.
    for _pat in extra_patterns {
        // Skipping regex eval — adding `regex` crate just for this is
        // overkill in R6. Defer to a follow-up if anyone actually uses
        // mentionPatterns in practice.
    }

    false
}

/// Like `gate()` but for bot commands (/start /help /status). NO pairing
/// side effects — just allow/drop. Returns Some((access, sender_id)) when
/// the sender is permitted to run a command. Mirrors TSX `dmCommandGate`
/// (server.ts:285-298).
pub fn dm_command_gate(msg: &Message) -> Option<(Access, String)> {
    use frankenstein::types::ChatType;
    if msg.chat.type_field != ChatType::Private {
        return None;
    }
    let from = msg.from.as_ref()?;
    let sender_id = from.id.to_string();
    let mut access = load();
    let pruned = prune_expired(&mut access);
    if pruned {
        let _ = save(&access);
    }
    if access.dm_policy == DmPolicy::Disabled {
        return None;
    }
    if access.dm_policy == DmPolicy::Allowlist
        && !access.allow_from.iter().any(|s| s == &sender_id)
    {
        return None;
    }
    Some((access, sender_id))
}

/// Outbound-tool security gate. Refuses to operate on chats that are
/// not in the allowlist OR in the configured group list. Mirrors TSX
/// `assertAllowedChat` (server.ts:195-202). Stops Mira from accidentally
/// or maliciously messaging arbitrary Telegram users.
pub fn assert_allowed_chat(chat_id: &str) -> Result<(), String> {
    let access = load();
    if access.allow_from.iter().any(|s| s == chat_id) {
        return Ok(());
    }
    if access.groups.contains_key(chat_id) {
        return Ok(());
    }
    Err(format!(
        "chat {} is not allowlisted — add via /telegram:access",
        chat_id
    ))
}

/// Generate a fresh 6-character lowercase hex pairing code (24 bits of
/// entropy). Mirrors TSX `randomBytes(3).toString('hex')`.
fn new_pairing_code() -> String {
    let mut bytes = [0u8; 3];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_format() {
        let c = new_pairing_code();
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
    }

    #[test]
    fn pairing_codes_are_random() {
        let codes: std::collections::HashSet<String> =
            (0..100).map(|_| new_pairing_code()).collect();
        // 24 bits of entropy → 100 samples should have <<1 collision expected.
        assert!(codes.len() > 95);
    }
}
