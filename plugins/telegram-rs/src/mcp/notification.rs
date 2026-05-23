//! Channel notification builders. Wire format mirrors TSX `server.ts:889-918`
//! byte-for-byte so Claude Code's channel surface parses our output the
//! same way it parses TSX's.

use serde_json::{json, Value};

/// Optional attachment metadata (voice / photo / document / etc).
#[derive(Debug, Clone, Default)]
pub struct AttachmentMeta {
    pub kind: String,
    pub file_id: String,
    pub size: Option<i64>,
    pub mime: Option<String>,
    pub name: Option<String>,
}

/// Build a `notifications/claude/channel` notification matching the TSX
/// envelope exactly. Returns a `serde_json::Value` ready to push through
/// the notification channel to the stdout writer.
///
/// All numeric IDs are serialized AS STRINGS per TSX behavior — this is
/// what Claude Code's channel-surface parser expects.
#[allow(clippy::too_many_arguments)]
pub fn channel_message(
    content: &str,
    chat_id: i64,
    message_id: Option<i64>,
    user: &str,
    user_id: i64,
    ts_unix_seconds: i64,
    image_path: Option<&str>,
    attachment: Option<&AttachmentMeta>,
) -> Value {
    let mut meta = serde_json::Map::new();
    meta.insert("chat_id".into(), Value::String(chat_id.to_string()));
    if let Some(mid) = message_id {
        meta.insert("message_id".into(), Value::String(mid.to_string()));
    }
    meta.insert("user".into(), Value::String(user.to_string()));
    meta.insert("user_id".into(), Value::String(user_id.to_string()));
    meta.insert("ts".into(), Value::String(to_iso8601(ts_unix_seconds)));
    if let Some(p) = image_path {
        meta.insert("image_path".into(), Value::String(p.to_string()));
    }
    if let Some(a) = attachment {
        meta.insert("attachment_kind".into(), Value::String(a.kind.clone()));
        meta.insert("attachment_file_id".into(), Value::String(a.file_id.clone()));
        if let Some(s) = a.size {
            meta.insert("attachment_size".into(), Value::String(s.to_string()));
        }
        if let Some(m) = &a.mime {
            meta.insert("attachment_mime".into(), Value::String(m.clone()));
        }
        if let Some(n) = &a.name {
            meta.insert("attachment_name".into(), Value::String(n.clone()));
        }
    }
    crate::mcp::protocol::notification(
        "notifications/claude/channel",
        json!({ "content": content, "meta": Value::Object(meta) }),
    )
}

/// Format a Unix timestamp (seconds) as ISO 8601 with millisecond precision
/// and 'Z' suffix — matches `new Date(ms).toISOString()` from TSX.
fn to_iso8601(unix_seconds: i64) -> String {
    let unix_ms = (unix_seconds as i128) * 1000;
    let secs = unix_ms.div_euclid(1000) as i64;
    let ms = unix_ms.rem_euclid(1000) as u32;
    // Build manually without chrono. Compute Y/M/D/H/M/S from epoch.
    let (y, mo, d, h, mi, s) = epoch_to_utc(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, h, mi, s, ms
    )
}

/// Convert Unix epoch seconds → UTC (Y, M, D, h, m, s). Algorithm from
/// the Gregorian calendar handbook (no leap-second handling — matches
/// JavaScript Date semantics).
fn epoch_to_utc(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let h = (time_of_day / 3600) as u32;
    let mi = ((time_of_day % 3600) / 60) as u32;
    let s = (time_of_day % 60) as u32;

    // Days since 1970-01-01 → Y/M/D using Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_message_shape_matches_tsx() {
        let v = channel_message(
            "hello", 434566766, Some(419), "codefather_dev", 434566766,
            1748013463, None, None,
        );
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "notifications/claude/channel");
        assert_eq!(v["params"]["content"], "hello");
        assert_eq!(v["params"]["meta"]["chat_id"], "434566766");
        assert_eq!(v["params"]["meta"]["message_id"], "419");
        assert_eq!(v["params"]["meta"]["user"], "codefather_dev");
        assert_eq!(v["params"]["meta"]["user_id"], "434566766");
        // ts must be ISO 8601 with Z suffix and millisecond precision
        let ts = v["params"]["meta"]["ts"].as_str().unwrap();
        assert!(ts.ends_with('Z'), "ts={}", ts);
        assert!(ts.contains('T'));
        assert_eq!(ts.len(), 24); // YYYY-MM-DDTHH:MM:SS.mmmZ
    }

    #[test]
    fn iso8601_epoch_zero() {
        // 1970-01-01T00:00:00.000Z
        assert_eq!(to_iso8601(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn iso8601_known_date() {
        // 2025-05-23T15:47:43Z (pure algorithmic test — any fixed point works
        // to verify epoch_to_utc is correct end-to-end).
        let s = to_iso8601(1748015263);
        assert_eq!(s, "2025-05-23T15:47:43.000Z");
    }

    #[test]
    fn iso8601_leap_year_feb29() {
        // 2024-02-29T12:34:56Z = unix 1709210096 — exercises the leap day branch.
        let s = to_iso8601(1709210096);
        assert_eq!(s, "2024-02-29T12:34:56.000Z");
    }

    #[test]
    fn attachment_meta_included_when_present() {
        let att = AttachmentMeta {
            kind: "voice".into(),
            file_id: "AwACAgIAA...".into(),
            size: Some(8125),
            mime: Some("audio/ogg".into()),
            name: None,
        };
        let v = channel_message(
            "(voice)", 434566766, Some(435), "codefather_dev", 434566766,
            1748014110, None, Some(&att),
        );
        assert_eq!(v["params"]["meta"]["attachment_kind"], "voice");
        assert_eq!(v["params"]["meta"]["attachment_file_id"], "AwACAgIAA...");
        assert_eq!(v["params"]["meta"]["attachment_size"], "8125");
        assert_eq!(v["params"]["meta"]["attachment_mime"], "audio/ogg");
        assert!(v["params"]["meta"].get("attachment_name").is_none());
    }
}
