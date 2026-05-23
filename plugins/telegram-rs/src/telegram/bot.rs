//! Long-polling loop using `frankenstein` + per-content-type message
//! handlers. Mirrors TSX `server.ts:787-895` for the dispatch matrix and
//! `server.ts:994-1037` for the retry-with-backoff polling loop.

use frankenstein::client_reqwest::Bot;
use frankenstein::methods::{GetUpdatesParams, SendMessageParams};
use frankenstein::types::Message;
use frankenstein::AsyncTelegramApi;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::access::gate::{dm_command_gate, gate, GateResult};
use crate::mcp::notification::{channel_message, AttachmentMeta};
use crate::mcp::permission::{parse_permission_reply, PendingPermissions};
use crate::mcp::protocol::notification as build_notification;

pub async fn run(
    bot: Bot,
    notification_tx: mpsc::UnboundedSender<Value>,
    pending_permissions: PendingPermissions,
) {
    if let Err(e) = write_pid_file() {
        tracing::warn!(error = %e, "could not write pid file (continuing)");
    }

    let bot_username: Arc<String> = match bot.get_me().await {
        Ok(resp) => {
            let name = resp.result.username.unwrap_or_default();
            tracing::info!(bot_username = %name, "polling started");
            Arc::new(name)
        }
        Err(e) => {
            tracing::warn!(error = ?e, "get_me failed — group mention detection disabled");
            Arc::new(String::new())
        }
    };

    // Token snapshot — needed for photo download URLs. Captured once;
    // a token rotation would require restart anyway.
    let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();

    let mut offset: i64 = 0;
    let mut attempt: u32 = 1;

    loop {
        let params = GetUpdatesParams::builder()
            .offset(offset)
            .timeout(30_u32)
            .build();

        match bot.get_updates(&params).await {
            Ok(response) => {
                attempt = 1;
                let updates = response.result;
                for update in updates {
                    offset = (update.update_id as i64) + 1;
                    handle_update(
                        &update,
                        &bot,
                        &bot_username,
                        &token,
                        &notification_tx,
                        &pending_permissions,
                    )
                    .await;
                }
            }
            Err(e) => {
                let err_str = format!("{:?}", e);
                let is_409 = err_str.contains("409") || err_str.contains("Conflict");
                if is_409 && attempt >= 8 {
                    tracing::error!(
                        "409 Conflict persists after {} attempts — another poller \
                         is holding the bot token. Exiting polling loop.",
                        attempt
                    );
                    return;
                }
                let delay = std::cmp::min(1000_u64 * attempt as u64, 15_000);
                let detail = if is_409 && attempt == 1 {
                    " — another instance is polling (zombie session, or a second Claude Code running?)"
                } else {
                    ""
                };
                let kind = if is_409 { "409 Conflict" } else { "polling error" };
                tracing::warn!(
                    "{}{}, retrying in {}ms (attempt {}): {}",
                    kind, detail, delay, attempt, err_str
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                attempt += 1;
            }
        }
    }
}

async fn handle_update(
    update: &frankenstein::updates::Update,
    bot: &Bot,
    bot_username: &str,
    token: &str,
    notification_tx: &mpsc::UnboundedSender<Value>,
    pending: &PendingPermissions,
) {
    use frankenstein::updates::UpdateContent;
    let msg = match &update.content {
        UpdateContent::Message(m) => m,
        UpdateContent::CallbackQuery(cb) => {
            handle_callback_query(cb, bot, pending, notification_tx).await;
            return;
        }
        other => {
            tracing::debug!("non-message update content: {:?}", other);
            return;
        }
    };

    // Bot commands (/start /help /status) — DM only, intercepted before
    // gate so non-allowlisted senders can run /start to see pairing flow.
    // Mirrors TSX server.ts:684-731.
    if let Some(text) = msg.text.as_deref() {
        let cmd = text.split_whitespace().next().unwrap_or("");
        // Strip optional @botname suffix per Telegram convention.
        let cmd_name = cmd.split('@').next().unwrap_or("");
        match cmd_name {
            "/start" | "/help" | "/status" => {
                if let Some((access, sender_id)) = dm_command_gate(msg) {
                    handle_bot_command(cmd_name, &access, &sender_id, msg, bot).await;
                }
                return;
            }
            _ => {}
        }
    }

    // Permission-reply text intercept (TSX server.ts:766-784). If the
    // sender is allowlisted AND types "yes XXXXX" / "no XXXXX" where
    // XXXXX is a pending request_id, emit the permission notification
    // and skip the normal channel-notification path.
    if let Some(text) = msg.text.as_deref() {
        if let Some((behavior, req_id)) = parse_permission_reply(text) {
            if let Some(from) = &msg.from {
                let sender_id = from.id.to_string();
                let access = crate::access::state::load();
                if access.allow_from.iter().any(|s| s == &sender_id) {
                    if pending.remove(&req_id).is_some() {
                        emit_permission_decision(&req_id, behavior, notification_tx);
                        // React to acknowledge.
                        let emoji = if behavior == "allow" { "✅" } else { "❌" };
                        let _ = crate::telegram::api::react(bot, msg.chat.id, msg.message_id, emoji).await;
                        return;
                    } else {
                        tracing::debug!(
                            req_id = %req_id,
                            "permission-reply text matched but request_id not in pending — passing through to channel"
                        );
                    }
                }
            }
        }
    }

    // Extract (text, attachment, photo_to_download) based on content type.
    let dispatched = dispatch(msg);
    let Some(d) = dispatched else {
        tracing::debug!(update_id = update.update_id, "no dispatchable content type");
        return;
    };

    let gate_result = gate(msg, bot_username);
    match gate_result {
        GateResult::Drop => {
            tracing::info!(
                update_id = update.update_id,
                from = msg.from.as_ref().map(|f| f.id).unwrap_or(0),
                chat = msg.chat.id,
                kind = %d.kind,
                "dropped by gate"
            );
        }
        GateResult::Pair { code, is_resend } => {
            let lead = if is_resend { "Still pending" } else { "Pairing required" };
            let reply = format!(
                "{} — run in Claude Code:\n\n/telegram:access pair {}",
                lead, code
            );
            tracing::info!(code = %code, is_resend = is_resend, chat = msg.chat.id, "pairing reply");
            send_simple_reply(bot, msg.chat.id, &reply).await;
        }
        GateResult::Deliver { access: _ } => {
            // For photos: download to inbox AFTER gate approval (TSX comment:
            // "any user can send photos, and we don't want to burn API quota
            // or fill the inbox for dropped messages").
            let image_path: Option<String> = if let Some(photo) = &d.photo_to_download {
                crate::telegram::inbox::download_photo(
                    bot,
                    token,
                    &photo.file_id,
                    &photo.file_unique_id,
                )
                .await
            } else {
                None
            };

            // For voice without caption: transcribe AFTER gate approval (same
            // "don't burn CPU on dropped" rationale as photo download). On
            // success, override d.text with "[voice transcription] X"; on
            // failure, the placeholder "(voice message)" from d.text stands.
            let mut effective_text = d.text.clone();
            if let Some(file_id) = &d.voice_to_transcribe {
                if let Some(transcript) =
                    crate::whisper::transcribe_voice(bot, token, file_id).await
                {
                    effective_text = format!("[voice transcription] {}", transcript);
                }
            }

            emit_notification(
                msg,
                &d,
                &effective_text,
                image_path.as_deref(),
                notification_tx,
            );
        }
    }
}

/// Result of dispatching a message by content type.
struct Dispatched {
    /// Content text — caption, placeholder, or actual text.
    text: String,
    /// Kind label for logging.
    kind: &'static str,
    /// Optional attachment metadata for non-text messages.
    attachment: Option<AttachmentMeta>,
    /// Set ONLY for photos — caller should call inbox::download_photo to
    /// fetch the bytes after the gate approves the sender.
    photo_to_download: Option<PhotoRef>,
    /// Set ONLY for voice messages WITHOUT a caption. After gate
    /// approval, caller transcribes via whisper-cli subprocess; on
    /// success the channel content becomes `[voice transcription] <text>`,
    /// on failure it stays as the placeholder already set in `text`.
    voice_to_transcribe: Option<String>,
}

struct PhotoRef {
    file_id: String,
    file_unique_id: String,
}

fn dispatch(msg: &Message) -> Option<Dispatched> {
    if let Some(text) = msg.text.as_deref() {
        return Some(Dispatched {
            text: text.to_string(),
            kind: "text",
            attachment: None,
            photo_to_download: None,
            voice_to_transcribe: None,
        });
    }

    if let Some(photos) = msg.photo.as_deref() {
        // Largest size is last in the array per Telegram convention.
        let best = photos.last()?;
        let caption = msg.caption.clone().unwrap_or_else(|| "(photo)".to_string());
        return Some(Dispatched {
            text: caption,
            kind: "photo",
            // Photos don't get attachment_kind/file_id in TSX — they use
            // image_path instead. Match that behavior.
            attachment: None,
            photo_to_download: Some(PhotoRef {
                file_id: best.file_id.clone(),
                file_unique_id: best.file_unique_id.clone(),
            }),
            voice_to_transcribe: None,
        });
    }

    if let Some(doc) = msg.document.as_ref() {
        let name = doc.file_name.as_deref().map(safe_name);
        let text = msg.caption.clone().unwrap_or_else(|| {
            format!("(document: {})", name.as_deref().unwrap_or("file"))
        });
        return Some(Dispatched {
            text,
            kind: "document",
            attachment: Some(AttachmentMeta {
                kind: "document".into(),
                file_id: doc.file_id.clone(),
                size: doc.file_size.map(|s| s as i64),
                mime: doc.mime_type.clone(),
                name,
            }),
            photo_to_download: None,
            voice_to_transcribe: None,
        });
    }

    if let Some(voice) = msg.voice.as_ref() {
        let has_caption = msg.caption.is_some();
        let text = msg.caption.clone().unwrap_or_else(|| "(voice message)".into());
        return Some(Dispatched {
            text,
            kind: "voice",
            attachment: Some(AttachmentMeta {
                kind: "voice".into(),
                file_id: voice.file_id.clone(),
                size: voice.file_size.map(|s| s as i64),
                mime: voice.mime_type.clone(),
                name: None,
            }),
            photo_to_download: None,
            // Only transcribe when no caption — caption-first, like TSX.
            voice_to_transcribe: if has_caption {
                None
            } else {
                Some(voice.file_id.clone())
            },
        });
    }

    if let Some(audio) = msg.audio.as_ref() {
        let name = audio.file_name.as_deref().map(safe_name);
        let title = audio.title.as_deref().map(safe_name);
        let text = msg.caption.clone().unwrap_or_else(|| {
            format!(
                "(audio: {})",
                title.as_deref().or(name.as_deref()).unwrap_or("audio")
            )
        });
        return Some(Dispatched {
            text,
            kind: "audio",
            attachment: Some(AttachmentMeta {
                kind: "audio".into(),
                file_id: audio.file_id.clone(),
                size: audio.file_size.map(|s| s as i64),
                mime: audio.mime_type.clone(),
                name,
            }),
            photo_to_download: None,
            voice_to_transcribe: None,
        });
    }

    if let Some(video) = msg.video.as_ref() {
        let text = msg.caption.clone().unwrap_or_else(|| "(video)".into());
        return Some(Dispatched {
            text,
            kind: "video",
            attachment: Some(AttachmentMeta {
                kind: "video".into(),
                file_id: video.file_id.clone(),
                size: video.file_size.map(|s| s as i64),
                mime: video.mime_type.clone(),
                name: video.file_name.as_deref().map(safe_name),
            }),
            photo_to_download: None,
            voice_to_transcribe: None,
        });
    }

    if let Some(vn) = msg.video_note.as_ref() {
        return Some(Dispatched {
            text: "(video note)".into(),
            kind: "video_note",
            attachment: Some(AttachmentMeta {
                kind: "video_note".into(),
                file_id: vn.file_id.clone(),
                size: vn.file_size.map(|s| s as i64),
                mime: None,
                name: None,
            }),
            photo_to_download: None,
            voice_to_transcribe: None,
        });
    }

    if let Some(sticker) = msg.sticker.as_ref() {
        let emoji_suffix = sticker
            .emoji
            .as_deref()
            .map(|e| format!(" {}", e))
            .unwrap_or_default();
        return Some(Dispatched {
            text: format!("(sticker{})", emoji_suffix),
            kind: "sticker",
            attachment: Some(AttachmentMeta {
                kind: "sticker".into(),
                file_id: sticker.file_id.clone(),
                size: sticker.file_size.map(|s| s as i64),
                mime: None,
                name: None,
            }),
            photo_to_download: None,
            voice_to_transcribe: None,
        });
    }

    None
}

fn emit_notification(
    msg: &Message,
    d: &Dispatched,
    text: &str,
    image_path: Option<&str>,
    notification_tx: &mpsc::UnboundedSender<Value>,
) {
    let Some(from) = &msg.from else { return };
    let chat_id = msg.chat.id;
    let user_id = from.id as i64;
    let user_label = from
        .username
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| user_id.to_string());
    let msg_id = msg.message_id as i64;
    let ts = msg.date as i64;

    tracing::info!(
        from = %user_label,
        user_id = user_id,
        chat_id = chat_id,
        msg_id = msg_id,
        kind = %d.kind,
        image_path = ?image_path,
        content_chars = text.chars().count(),
        "emitting channel notification"
    );

    let notif = channel_message(
        text,
        chat_id,
        Some(msg_id),
        &user_label,
        user_id,
        ts,
        image_path,
        d.attachment.as_ref(),
    );
    if let Err(e) = notification_tx.send(notif) {
        tracing::error!(error = %e, "failed to enqueue channel notification");
    }
}

/// Handle a callback_query (inline keyboard button press). Pattern:
/// `perm:<allow|deny|more>:<request_id>`. Mirrors TSX server.ts:731-786.
async fn handle_callback_query(
    cb: &frankenstein::types::CallbackQuery,
    bot: &Bot,
    pending: &PendingPermissions,
    notification_tx: &mpsc::UnboundedSender<Value>,
) {
    let Some(data) = cb.data.as_deref() else {
        let _ = crate::telegram::api::answer_callback_query(bot, &cb.id, None).await;
        return;
    };

    // Parse perm:<action>:<request_id>
    let Some(rest) = data.strip_prefix("perm:") else {
        let _ = crate::telegram::api::answer_callback_query(bot, &cb.id, None).await;
        return;
    };
    let Some((behavior, request_id)) = rest.split_once(':') else {
        let _ = crate::telegram::api::answer_callback_query(bot, &cb.id, None).await;
        return;
    };
    if !matches!(behavior, "allow" | "deny" | "more") {
        let _ = crate::telegram::api::answer_callback_query(bot, &cb.id, None).await;
        return;
    }

    // Authz: only allowlisted users can decide.
    let sender_id = cb.from.id.to_string();
    let access = crate::access::state::load();
    if !access.allow_from.iter().any(|s| s == &sender_id) {
        tracing::warn!(
            sender_id = %sender_id,
            request_id = %request_id,
            "callback from non-allowlisted user — ignored"
        );
        let _ = crate::telegram::api::answer_callback_query(
            bot, &cb.id, Some("Not authorized."),
        )
        .await;
        return;
    }

    let request_id = request_id.to_string();
    let chat_id = cb.message.as_ref().and_then(|m| match m {
        frankenstein::types::MaybeInaccessibleMessage::Message(m) => Some(m.chat.id),
        _ => None,
    });
    let message_id = cb.message.as_ref().and_then(|m| match m {
        frankenstein::types::MaybeInaccessibleMessage::Message(m) => Some(m.message_id),
        _ => None,
    });

    if behavior == "more" {
        let Some(details) = pending.get(&request_id) else {
            let _ = crate::telegram::api::answer_callback_query(
                bot, &cb.id, Some("Details no longer available."),
            )
            .await;
            return;
        };
        let pretty_input = serde_json::from_str::<Value>(&details.input_preview)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| details.input_preview.clone());
        let expanded = format!(
            "🔐 Permission: {}\n\ntool_name: {}\ndescription: {}\ninput_preview:\n{}",
            details.tool_name, details.tool_name, details.description, pretty_input
        );
        if let (Some(c), Some(m)) = (chat_id, message_id) {
            let _ = crate::telegram::api::edit_to_expanded_permission(
                bot, c, m, &expanded, &request_id,
            )
            .await;
        }
        let _ = crate::telegram::api::answer_callback_query(bot, &cb.id, None).await;
        return;
    }

    // allow / deny
    emit_permission_decision(&request_id, behavior, notification_tx);
    pending.remove(&request_id);
    let label = if behavior == "allow" { "✅ Allowed" } else { "❌ Denied" };
    let _ = crate::telegram::api::answer_callback_query(bot, &cb.id, Some(label)).await;

    // Replace the message text with the outcome so the same request can't
    // be answered twice and the chat history shows what was chosen.
    if let (Some(c), Some(m)) = (chat_id, message_id) {
        let original = cb.message.as_ref().and_then(|mb| match mb {
            frankenstein::types::MaybeInaccessibleMessage::Message(msg) => {
                msg.text.clone()
            }
            _ => None,
        });
        let final_text = match original {
            Some(t) => format!("{}\n\n{}", t, label),
            None => label.to_string(),
        };
        let _ = crate::telegram::api::edit_to_decided_permission(bot, c, m, &final_text).await;
    }
}

/// Emit `notifications/claude/channel/permission` to CC via the same
/// notification_tx channel as channel messages. Mirrors TSX server.ts:772.
fn emit_permission_decision(
    request_id: &str,
    behavior: &str,
    notification_tx: &mpsc::UnboundedSender<Value>,
) {
    tracing::info!(
        request_id = %request_id,
        behavior = %behavior,
        "emitting permission decision to CC"
    );
    let notif = build_notification(
        "notifications/claude/channel/permission",
        json!({ "request_id": request_id, "behavior": behavior }),
    );
    if let Err(e) = notification_tx.send(notif) {
        tracing::error!(error = %e, "failed to enqueue permission notification");
    }
}

/// Handle /start /help /status bot commands. Mirrors TSX server.ts:684-731.
async fn handle_bot_command(
    cmd: &str,
    access: &crate::access::state::Access,
    sender_id: &str,
    msg: &Message,
    bot: &Bot,
) {
    let reply_text = match cmd {
        "/start" => "This bot bridges Telegram to a Claude Code session.\n\n\
                     To pair:\n\
                     1. DM me anything — you'll get a 6-char code\n\
                     2. In Claude Code: /telegram:access pair <code>\n\n\
                     After that, DMs here reach that session."
            .to_string(),
        "/help" => "Messages you send here route to a paired Claude Code session. \
                    Text and photos are forwarded; replies and reactions come back.\n\n\
                    /start — pairing instructions\n\
                    /status — check your pairing state"
            .to_string(),
        "/status" => {
            if access.allow_from.iter().any(|s| s == sender_id) {
                let name = msg
                    .from
                    .as_ref()
                    .and_then(|f| f.username.as_deref())
                    .map(|u| format!("@{}", u))
                    .unwrap_or_else(|| sender_id.to_string());
                format!("Paired as {}.", name)
            } else if let Some((code, _)) = access
                .pending
                .iter()
                .find(|(_, p)| p.sender_id == sender_id)
            {
                format!(
                    "Pending pairing — run in Claude Code:\n\n/telegram:access pair {}",
                    code
                )
            } else {
                "Not paired. Send me a message to get a pairing code.".to_string()
            }
        }
        _ => return,
    };
    send_simple_reply(bot, msg.chat.id, &reply_text).await;
}

/// Strip uploader-controlled chars that would let the user break out of the
/// `<channel>` notification XML envelope. Mirrors TSX `safeName`.
pub fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '<' | '>' | '[' | ']' | '\r' | '\n' | ';' => '_',
            other => other,
        })
        .collect()
}

async fn send_simple_reply(bot: &Bot, chat_id: i64, text: &str) {
    let params = SendMessageParams::builder()
        .chat_id(chat_id)
        .text(text.to_string())
        .build();
    if let Err(e) = bot.send_message(&params).await {
        tracing::warn!(error = ?e, chat_id = chat_id, "send_simple_reply failed");
    }
}

fn write_pid_file() -> std::io::Result<()> {
    let path = crate::state::pid_file();
    let dir = crate::state::state_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_name_strips_delimiters() {
        assert_eq!(safe_name("file<name>.txt"), "file_name_.txt");
        assert_eq!(safe_name("a;b"), "a_b");
        assert_eq!(safe_name("multi\nline\rname"), "multi_line_name");
        assert_eq!(safe_name("normal.txt"), "normal.txt");
    }
}
