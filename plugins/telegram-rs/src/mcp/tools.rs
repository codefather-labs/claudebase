//! MCP tool schemas + dispatch. Schemas mirror TSX `server.ts:445-518`
//! verbatim — Claude Code's tools/list parser is permissive but the agent
//! reads descriptions when deciding when to call a tool, so we keep them
//! intact.

use serde_json::{json, Value};

/// Build the `tools` array for the `tools/list` response. Each tool is a
/// `{name, description, inputSchema}` object. Schemas mirror TSX
/// `server.ts:445-518` so Claude Code's prompt sees identical tool
/// surfaces whichever backend runs.
pub fn tools_list() -> Value {
    json!([
        {
            "name": "reply",
            "description": "Reply on Telegram. Pass chat_id from the inbound message. Optionally pass reply_to (message_id) for threading, and files (absolute paths) to attach images or documents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": { "type": "string" },
                    "text": { "type": "string" },
                    "reply_to": {
                        "type": "string",
                        "description": "Message ID to thread under. Use message_id from the inbound <channel> block."
                    },
                    "files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Absolute file paths to attach. Images (jpg/jpeg/png/gif/webp) send as photos (inline preview); other types as documents. Max 50MB each."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["text", "markdownv2"],
                        "description": "Rendering mode. 'markdownv2' enables Telegram formatting (bold, italic, code, links). Caller must escape special chars per MarkdownV2 rules. Default: 'text' (plain, no escaping needed)."
                    }
                },
                "required": ["chat_id", "text"]
            }
        },
        {
            "name": "react",
            "description": "Add an emoji reaction to a Telegram message. Telegram only accepts a fixed whitelist (👍 👎 ❤ 🔥 👀 🎉 etc) — non-whitelisted emoji will be rejected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": { "type": "string" },
                    "message_id": { "type": "string" },
                    "emoji": { "type": "string" }
                },
                "required": ["chat_id", "message_id", "emoji"]
            }
        },
        {
            "name": "edit_message",
            "description": "Edit a message the bot previously sent. Useful for interim progress updates. Edits don't trigger push notifications — send a new reply when a long task completes so the user's device pings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": { "type": "string" },
                    "message_id": { "type": "string" },
                    "text": { "type": "string" },
                    "format": {
                        "type": "string",
                        "enum": ["text", "markdownv2"],
                        "description": "Rendering mode. 'markdownv2' enables Telegram formatting (bold, italic, code, links). Caller must escape special chars per MarkdownV2 rules. Default: 'text' (plain, no escaping needed)."
                    }
                },
                "required": ["chat_id", "message_id", "text"]
            }
        },
        {
            "name": "download_attachment",
            "description": "Download a file attachment from a Telegram message to the local inbox. Use when the inbound <channel> meta shows attachment_file_id. Returns the local file path ready to Read. Telegram caps bot downloads at 20MB.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_id": { "type": "string", "description": "The attachment_file_id from inbound meta" }
                },
                "required": ["file_id"]
            }
        }
    ])
}

/// Maximum Telegram text-message chunk (per Bot API).
pub const MAX_CHUNK_LIMIT: usize = 4096;

/// Split `text` into chunks of at most `MAX_CHUNK_LIMIT` chars, preferring
/// newline boundaries when present near the boundary, falling back to hard
/// char-count split. Matches TSX `server.ts:357-379`.
pub fn chunk_text(text: &str) -> Vec<String> {
    if text.chars().count() <= MAX_CHUNK_LIMIT {
        return vec![text.to_string()];
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor < chars.len() {
        let end = (cursor + MAX_CHUNK_LIMIT).min(chars.len());
        // Prefer cut on a newline within the last 256 chars of this chunk.
        let slice = &chars[cursor..end];
        let actual_end = if end < chars.len() {
            // Search backward for newline.
            let search_start = slice.len().saturating_sub(256);
            slice[search_start..]
                .iter()
                .rposition(|&c| c == '\n')
                .map(|p| cursor + search_start + p + 1)
                .unwrap_or(end)
        } else {
            end
        };
        let chunk: String = chars[cursor..actual_end].iter().collect();
        out.push(chunk);
        cursor = actual_end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_returns_single_chunk() {
        let v = chunk_text("hello");
        assert_eq!(v, vec!["hello".to_string()]);
    }

    #[test]
    fn long_text_splits_at_limit() {
        let s = "a".repeat(5000);
        let v = chunk_text(&s);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].chars().count(), MAX_CHUNK_LIMIT);
        assert_eq!(v[1].chars().count(), 5000 - MAX_CHUNK_LIMIT);
    }

    #[test]
    fn long_text_prefers_newline_boundary() {
        let mut s = "a".repeat(MAX_CHUNK_LIMIT - 100);
        s.push('\n');
        s.push_str(&"b".repeat(500));
        let v = chunk_text(&s);
        assert!(v[0].ends_with('\n'));
        assert!(v[1].starts_with('b'));
    }

    #[test]
    fn unicode_safe_char_boundary() {
        // 4096 ASCII chars + 100 emoji (each 4 bytes) — chunk by CHAR count, not bytes.
        let s: String = std::iter::repeat('a').take(4096).collect::<String>()
            + &"🦀".repeat(100);
        let v = chunk_text(&s);
        // All chunks must be valid UTF-8 (no panic on String collect).
        for chunk in &v {
            assert!(chunk.chars().count() <= MAX_CHUNK_LIMIT);
        }
    }
}
