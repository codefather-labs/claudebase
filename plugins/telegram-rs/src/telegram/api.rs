//! Outbound Telegram Bot API calls — wraps frankenstein methods with our
//! domain-specific input/output types so the MCP tool dispatcher doesn't
//! need to know about frankenstein internals.

use frankenstein::client_reqwest::Bot;
use frankenstein::input_file::{FileUpload, InputFile};
use frankenstein::methods::{
    AnswerCallbackQueryParams, EditMessageTextParams, GetFileParams, SendDocumentParams,
    SendMessageParams, SendPhotoParams, SetMessageReactionParams,
};
use frankenstein::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, ReactionType, ReactionTypeEmoji, ReplyMarkup,
    ReplyParameters,
};
use frankenstein::AsyncTelegramApi;
use std::path::PathBuf;

use crate::mcp::tools::chunk_text;

const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;
const PHOTO_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

/// Result of a `reply` call: list of message IDs sent (one per chunk).
#[derive(Debug)]
pub struct ReplyResult {
    pub message_ids: Vec<i32>,
}

/// Send a text reply, chunking if `text` exceeds Telegram's 4096-char
/// limit. `reply_to_message_id` is applied only to the FIRST text chunk
/// AND to each file. Each file sends as a separate message: photos for
/// jpg/jpeg/png/gif/webp, documents for everything else.
pub async fn reply(
    bot: &Bot,
    chat_id: i64,
    text: &str,
    reply_to_message_id: Option<i32>,
    files: &[String],
) -> Result<ReplyResult, String> {
    // Validate files before sending — fail fast.
    for f in files {
        let path = std::path::Path::new(f);
        if !path.is_absolute() {
            return Err(format!("file path must be absolute: {}", f));
        }
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("file stat failed: {} ({})", f, e))?;
        if !meta.is_file() {
            return Err(format!("not a regular file: {}", f));
        }
        if meta.len() > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "file too large: {} ({} MB, max 50 MB)",
                f,
                meta.len() / 1024 / 1024
            ));
        }
    }

    let chunks = chunk_text(text);
    let mut message_ids = Vec::with_capacity(chunks.len() + files.len());

    // --- text chunks ---
    for (idx, chunk) in chunks.iter().enumerate() {
        let params = if idx == 0 && reply_to_message_id.is_some() {
            SendMessageParams::builder()
                .chat_id(chat_id)
                .text(chunk.clone())
                .reply_parameters(
                    ReplyParameters::builder()
                        .message_id(reply_to_message_id.unwrap())
                        .build(),
                )
                .build()
        } else {
            SendMessageParams::builder()
                .chat_id(chat_id)
                .text(chunk.clone())
                .build()
        };

        match bot.send_message(&params).await {
            Ok(response) => message_ids.push(response.result.message_id),
            Err(e) => {
                return Err(format!(
                    "send_message failed at chunk {}/{}: {:?}",
                    idx + 1,
                    chunks.len(),
                    e
                ));
            }
        }
    }

    // --- files (each as separate message) ---
    for f in files {
        let path = PathBuf::from(f);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        let is_photo = PHOTO_EXTS.iter().any(|p| *p == ext);

        let reply_params = reply_to_message_id.map(|rid| {
            ReplyParameters::builder().message_id(rid).build()
        });

        let mid = if is_photo {
            let params = match reply_params {
                Some(rp) => SendPhotoParams::builder()
                    .chat_id(chat_id)
                    .photo(FileUpload::InputFile(InputFile { path: path.clone() }))
                    .reply_parameters(rp)
                    .build(),
                None => SendPhotoParams::builder()
                    .chat_id(chat_id)
                    .photo(FileUpload::InputFile(InputFile { path: path.clone() }))
                    .build(),
            };
            bot.send_photo(&params)
                .await
                .map(|r| r.result.message_id)
                .map_err(|e| format!("send_photo failed for {}: {:?}", f, e))?
        } else {
            let params = match reply_params {
                Some(rp) => SendDocumentParams::builder()
                    .chat_id(chat_id)
                    .document(FileUpload::InputFile(InputFile { path: path.clone() }))
                    .reply_parameters(rp)
                    .build(),
                None => SendDocumentParams::builder()
                    .chat_id(chat_id)
                    .document(FileUpload::InputFile(InputFile { path: path.clone() }))
                    .build(),
            };
            bot.send_document(&params)
                .await
                .map(|r| r.result.message_id)
                .map_err(|e| format!("send_document failed for {}: {:?}", f, e))?
        };
        message_ids.push(mid);
    }

    Ok(ReplyResult { message_ids })
}

/// Add an emoji reaction to a message. Telegram silently drops non-
/// whitelisted emoji (it returns 200 OK but the reaction never appears).
pub async fn react(
    bot: &Bot,
    chat_id: i64,
    message_id: i32,
    emoji: &str,
) -> Result<(), String> {
    let reaction = ReactionType::Emoji(ReactionTypeEmoji {
        emoji: emoji.to_string(),
    });
    let params = SetMessageReactionParams::builder()
        .chat_id(chat_id)
        .message_id(message_id)
        .reaction(vec![reaction])
        .build();
    bot.set_message_reaction(&params)
        .await
        .map(|_| ())
        .map_err(|e| format!("set_message_reaction failed: {:?}", e))
}

/// Edit text of a message the bot previously sent.
pub async fn edit_message(
    bot: &Bot,
    chat_id: i64,
    message_id: i32,
    text: &str,
) -> Result<i32, String> {
    let params = EditMessageTextParams::builder()
        .chat_id(chat_id)
        .message_id(message_id)
        .text(text.to_string())
        .build();
    match bot.edit_message_text(&params).await {
        Ok(resp) => {
            // edit_message_text returns either a Message or true (bool) per
            // Telegram spec. frankenstein wraps both in MessageOrBool.
            use frankenstein::response::MessageOrBool;
            match resp.result {
                MessageOrBool::Message(m) => Ok(m.message_id),
                MessageOrBool::Bool(_) => Ok(message_id),
            }
        }
        Err(e) => Err(format!("edit_message_text failed: {:?}", e)),
    }
}

/// Build the 3-button inline keyboard for a permission_request prompt
/// (See more / Allow / Deny). Callback data is `perm:<action>:<request_id>`.
fn permission_keyboard(request_id: &str, include_more: bool) -> InlineKeyboardMarkup {
    let mut row = Vec::with_capacity(3);
    if include_more {
        row.push(button("See more", &format!("perm:more:{}", request_id)));
    }
    row.push(button("✅ Allow", &format!("perm:allow:{}", request_id)));
    row.push(button("❌ Deny", &format!("perm:deny:{}", request_id)));
    InlineKeyboardMarkup::builder().inline_keyboard(vec![row]).build()
}

fn button(text: &str, callback_data: &str) -> InlineKeyboardButton {
    InlineKeyboardButton::builder()
        .text(text.to_string())
        .callback_data(callback_data.to_string())
        .build()
}

/// Send the initial permission_request prompt with 3-button keyboard.
pub async fn send_permission_prompt(
    bot: &Bot,
    chat_id: i64,
    text: &str,
    request_id: &str,
) -> Result<(), String> {
    let kb = permission_keyboard(request_id, true);
    let params = SendMessageParams::builder()
        .chat_id(chat_id)
        .text(text.to_string())
        .reply_markup(ReplyMarkup::InlineKeyboardMarkup(kb))
        .build();
    bot.send_message(&params)
        .await
        .map(|_| ())
        .map_err(|e| format!("send_message failed: {:?}", e))
}

/// On "See more" press — edit message to show full details, re-show
/// Allow/Deny buttons (no See more again).
pub async fn edit_to_expanded_permission(
    bot: &Bot,
    chat_id: i64,
    message_id: i32,
    expanded_text: &str,
    request_id: &str,
) -> Result<(), String> {
    let kb = permission_keyboard(request_id, false);
    let params = EditMessageTextParams::builder()
        .chat_id(chat_id)
        .message_id(message_id)
        .text(expanded_text.to_string())
        .reply_markup(kb)
        .build();
    bot.edit_message_text(&params)
        .await
        .map(|_| ())
        .map_err(|e| format!("edit_message_text failed: {:?}", e))
}

/// On allow/deny — edit message to append the outcome label, dropping
/// the inline keyboard (replaced by plain text).
pub async fn edit_to_decided_permission(
    bot: &Bot,
    chat_id: i64,
    message_id: i32,
    final_text: &str,
) -> Result<(), String> {
    let params = EditMessageTextParams::builder()
        .chat_id(chat_id)
        .message_id(message_id)
        .text(final_text.to_string())
        .build();
    bot.edit_message_text(&params)
        .await
        .map(|_| ())
        .map_err(|e| format!("edit_message_text failed: {:?}", e))
}

/// Reply to a callback_query (the small pop-up shown to the tapper).
pub async fn answer_callback_query(
    bot: &Bot,
    callback_query_id: &str,
    text: Option<&str>,
) -> Result<(), String> {
    let params = match text {
        Some(t) => AnswerCallbackQueryParams::builder()
            .callback_query_id(callback_query_id.to_string())
            .text(t.to_string())
            .build(),
        None => AnswerCallbackQueryParams::builder()
            .callback_query_id(callback_query_id.to_string())
            .build(),
    };
    bot.answer_callback_query(&params)
        .await
        .map(|_| ())
        .map_err(|e| format!("answer_callback_query failed: {:?}", e))
}

/// Download a file by `file_id` from Telegram CDN to the inbox dir.
/// Returns the local absolute path. Used by the `download_attachment`
/// MCP tool (Mira calls this when she sees `attachment_file_id` in
/// inbound channel meta and wants the file content).
pub async fn download_attachment(
    bot: &Bot,
    token: &str,
    file_id: &str,
) -> Result<String, String> {
    let inbox = crate::state::inbox_dir();
    std::fs::create_dir_all(&inbox)
        .map_err(|e| format!("create inbox dir failed: {}", e))?;

    let params = GetFileParams::builder().file_id(file_id.to_string()).build();
    let file = bot
        .get_file(&params)
        .await
        .map_err(|e| format!("get_file failed: {:?}", e))?
        .result;
    let file_path = file
        .file_path
        .ok_or_else(|| "Telegram returned no file_path — file may have expired".to_string())?;

    let url = format!("https://api.telegram.org/file/bot{}/{}", token, file_path);
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("download fetch failed: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("download body read failed: {}", e))?;

    // Sanitize extension to alphanumeric only.
    let raw_ext = file_path.rsplit('.').next().unwrap_or("bin");
    let ext: String = raw_ext.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let ext = if ext.is_empty() { "bin".to_string() } else { ext };

    let unique_id: String = file
        .file_unique_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    let unique_id = if unique_id.is_empty() {
        "dl".to_string()
    } else {
        unique_id
    };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = inbox.join(format!("{}-{}.{}", ts, unique_id, ext));
    std::fs::write(&path, &bytes)
        .map_err(|e| format!("write to inbox failed: {}", e))?;
    Ok(path.to_string_lossy().to_string())
}
