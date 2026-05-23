//! Inbox — local persistence for inbound attachments (photos primarily).
//! Mirrors TSX `server.ts:792-816` (photo download path).

use frankenstein::client_reqwest::Bot;
use frankenstein::methods::GetFileParams;
use frankenstein::AsyncTelegramApi;

/// Download a Telegram file by `file_id` into `inbox/` and return the
/// local path. Returns None on any error (logged) — caller decides
/// whether to drop the notification or send it without `image_path`.
pub async fn download_photo(
    bot: &Bot,
    token: &str,
    file_id: &str,
    file_unique_id: &str,
) -> Option<String> {
    let inbox = crate::state::inbox_dir();
    if let Err(e) = std::fs::create_dir_all(&inbox) {
        tracing::warn!(error = %e, "could not create inbox dir");
        return None;
    }

    let params = GetFileParams::builder().file_id(file_id.to_string()).build();
    let file = match bot.get_file(&params).await {
        Ok(r) => r.result,
        Err(e) => {
            tracing::warn!(error = ?e, file_id = %file_id, "get_file failed");
            return None;
        }
    };
    let Some(file_path) = file.file_path else {
        tracing::warn!(file_id = %file_id, "no file_path in get_file response");
        return None;
    };

    let url = format!("https://api.telegram.org/file/bot{}/{}", token, file_path);
    let bytes = match reqwest::get(&url).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "photo body read failed");
                return None;
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "photo fetch failed");
            return None;
        }
    };

    let ext = file_path.rsplit('.').next().unwrap_or("jpg");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let local_name = format!("{}-{}.{}", now_ms, file_unique_id, ext);
    let path = inbox.join(&local_name);

    if let Err(e) = std::fs::write(&path, &bytes) {
        tracing::warn!(error = %e, ?path, "photo write failed");
        return None;
    }
    tracing::info!(
        bytes = bytes.len(),
        ?path,
        "photo saved to inbox"
    );
    Some(path.to_string_lossy().to_string())
}
