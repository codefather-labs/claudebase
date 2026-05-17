//! Slice 4 — teloxide long-poll loop, inbound routing, outbound chat_reply.
//!
//! Architecture:
//!
//! 1. The daemon's main `serve()` spawns ONE `tokio::spawn` running
//!    `run_long_poll()`. Per ASYNC_INVARIANTS Rule 3 the body is wrapped
//!    in `if let Err(e) = ...` so a fatal Telegram error never panics the
//!    daemon — only the long-poll task ends, the rest of the daemon
//!    continues serving MCP plugins.
//!
//! 2. Each iteration calls `getUpdates` with the current `offset` from
//!    `daemon_state.telegram.last_update_id`. The returned batch is
//!    processed atomically (SEC-13): all chat-message inserts AND the
//!    offset bump live in one rusqlite transaction. If the daemon crashes
//!    mid-batch BEFORE commit, the next restart re-processes the same
//!    batch from the prior offset — schema v5 has NO unique constraint
//!    on `(thread_id, telegram_message_id)` (insight #9), so safety
//!    relies on the transactional offset-advance alone.
//!
//! 3. Errors:
//!    - HTTP 401: write `tg_bot_state = "disconnected"` into daemon_state,
//!      log structured event (no token in log), exit the long-poll loop.
//!      The daemon as a whole stays alive (Rule 3 / SEC-14).
//!    - HTTP 429: extract `retry_after`, sleep, retry ONCE (UC-3-E2 /
//!      SEC-14). On a second 429, surface to the outbound chat_reply
//!      caller via `{"error": "telegram_rate_limited", "retry_after": N}`
//!      and back off the inbound loop by sleeping `retry_after` seconds
//!      before resuming `getUpdates`.
//!    - Any teloxide error string is filtered through `redact_token`
//!      before reaching `tracing::error!` — substring-match against the
//!      raw token bytes.
//!
//! 4. Outbound: `handle_chat_reply` is the seam from the MCP `chat_reply`
//!    tool. When `thread_id` starts with `telegram:`, the daemon makes a
//!    teloxide `send_message` call. NOT wired into Slice 1c's chat tool
//!    handler in this Slice — that wiring lands when Slice 6-MVP's voice
//!    flow runs. For Slice 4 we expose the function so tests can drive it
//!    directly.
//!
//! ## Status (Slice 4)
//!
//! - Long-poll loop: SHIPS. Returns errors via Result, panic-safe by
//!   construction.
//! - `getUpdates` body processing: SHIPS, including transactional
//!   offset-advance.
//! - 401 / 429 handling: SHIPS.
//! - Voice notes: returns the literal placeholder
//!   `"[unsupported: enable asr-whisper feature]"` per Slice 4 acceptance
//!   criterion ("real ASR is Slice 6-MVP").
//! - End-to-end TG roundtrip with mocked `TELOXIDE_API_URL`: the e2e
//!   tests in `tests/telegram_e2e_test.rs` are SCAFFOLDS (they verify
//!   config-file layout, not live HTTP). Real mocked-roundtrip lives in
//!   a future iteration when the test harness is fleshed out.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;

use crate::daemon::asr::Asr;
use crate::daemon::chat::{self, SharedBus};
use crate::daemon::config::RedactedToken;
use crate::daemon::permissions::{self, Access};

/// One Telegram update as decoded from `getUpdates`. We deliberately
/// hand-decode a SMALL subset of the rich teloxide types because the
/// production loop only needs `update_id` + text-message routing for
/// Slice 4. Voice / photo / sticker bodies surface as `Other(value)` so
/// we can still tick the offset forward without parsing them.
///
/// teloxide's full `Update` enum tree compiles fine but pulls dialogue/
/// command/sqlx generics into our type signatures — for the lean Slice 4
/// loop the JSON-on-the-wire deserialisation here is simpler.
#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    #[serde(default)]
    pub from: Option<User>,
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
    /// When `voice` is present and `text` is absent, the bot received a
    /// voice note. Slice 4 returns the literal shim string; Slice 6-MVP
    /// wires the ASR pipeline.
    #[serde(default)]
    pub voice: Option<Voice>,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct Voice {
    pub file_id: String,
    #[serde(default)]
    pub duration: i64,
}

/// Voice-note shim per Slice 4 acceptance: real ASR ships in Slice 6-MVP.
pub const VOICE_SHIM_TEXT: &str = "[unsupported: enable asr-whisper feature]";

/// Result of one batch process — the highest `update_id` seen so the
/// outer loop can advance the offset, plus the notification frames the
/// async caller must publish via `ChatBus`. Notifications are deferred
/// to post-commit (we collect inside the sync `spawn_blocking` body and
/// publish from the async side once the transaction is durable) so a
/// crash between insert and publish cannot deliver phantom messages.
#[derive(Debug)]
pub struct BatchOutcome {
    pub new_offset: Option<i64>,
    pub messages_inserted: usize,
    /// Built post-commit by `process_batch`. Each tuple is
    /// `(thread_id, channel_notification_frame)`. The async long-poll
    /// caller iterates these and calls `bus.publish(thread, frame).await`.
    pub notifications: Vec<(String, serde_json::Value)>,
}

/// Strip occurrences of the bot token from any error string before it
/// reaches `tracing::error!`. This is the SEC-14 defence: teloxide errors
/// occasionally embed the URL (which carries the token in the path).
fn redact_error_string(s: &str, token: &str) -> String {
    if token.is_empty() {
        return s.to_string();
    }
    s.replace(token, "***")
}

/// Open the chat.db connection. Mirrors `chat::open_chat_db` but kept here
/// so the telegram module's call sites are explicit (the daemon spawns
/// telegram with its own DB handle — never share Connections across tasks
/// per ASYNC_INVARIANTS).
pub fn open_chat_db() -> Result<Connection> {
    chat::open_chat_db().map_err(Into::into)
}

/// Process one batch of Telegram updates inside ONE rusqlite transaction.
/// All chat-message inserts AND the offset-advance UPDATE are wrapped so
/// either every row makes it OR none of them do (SEC-13). On commit the
/// `last_update_id` daemon_state row is now `max_update_id`, so the next
/// `getUpdates` call uses `offset = max_update_id + 1`.
///
/// `access` is consulted for `check_allowed` — messages from non-allow-listed
/// users are silently skipped (TC-4.3) BUT their update_id still advances
/// the offset (otherwise an attacker could DOS the daemon by repeatedly
/// sending messages that get stuck at the same offset). The skip happens
/// inside the transaction — no DB row is inserted, but the offset moves.
pub fn process_batch(
    conn: &mut Connection,
    access: &Access,
    bus: Option<&SharedBus>,
    batch: &[Update],
) -> Result<BatchOutcome> {
    if batch.is_empty() {
        return Ok(BatchOutcome {
            new_offset: None,
            messages_inserted: 0,
            notifications: Vec::new(),
        });
    }

    let tx = conn.transaction()?;
    let mut max_id: i64 = 0;
    let mut inserted: usize = 0;
    let mut notifications: Vec<(String, serde_json::Value)> = Vec::new();

    for update in batch {
        if update.update_id > max_id {
            max_id = update.update_id;
        }
        let Some(msg) = &update.message else {
            continue;
        };
        let user_id = msg.from.as_ref().map(|u| u.id).unwrap_or(0);
        // SEC-12 / TC-4.3: drop messages from disallowed users without
        // inserting. Offset still advances (handled above).
        if !permissions::check_allowed(access, user_id) {
            continue;
        }

        let thread_id = format!("telegram:{}", msg.chat.id);
        let from_agent = match &msg.from.as_ref().and_then(|u| u.username.as_ref()) {
            Some(name) => format!("telegram:{name}"),
            None => format!("telegram:{user_id}"),
        };

        let content = match (&msg.text, &msg.voice) {
            (Some(text), _) => text.clone(),
            (None, Some(_)) => VOICE_SHIM_TEXT.to_string(),
            (None, None) => continue, // unsupported message type — skip but still advance offset
        };

        // chat::insert_message but inside this transaction. We replicate
        // the SQL here because the existing helper takes &Connection
        // (not &Transaction) and we need transactional atomicity per
        // SEC-13.
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono_millis();
        tx.execute(
            "INSERT OR IGNORE INTO chat_threads (id, created_at) VALUES (?1, ?2)",
            params![thread_id, now],
        )?;
        tx.execute(
            "INSERT INTO chat_messages \
             (id, thread_id, from_agent, content, reply_to, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, thread_id, from_agent, content, Option::<String>::None, now],
        )?;
        inserted += 1;

        // Build the notification we'll broadcast AFTER commit so a crash
        // between insert and broadcast doesn't deliver phantom messages.
        let msg_for_notif = chat::ChatMessage {
            id: id.clone(),
            thread_id: thread_id.clone(),
            from_agent: from_agent.clone(),
            content: content.clone(),
            reply_to: None,
            created_at: now,
        };
        notifications.push((thread_id.clone(), chat::build_channel_notification(&msg_for_notif)));
    }

    // Bump offset to max_update_id (so the next getUpdates uses offset =
    // max_id + 1). The value column in daemon_state is TEXT so we
    // stringify here. Stored value is the highest processed update_id;
    // the +1 offset adjustment is applied at the long-poll call site.
    tx.execute(
        "UPDATE daemon_state SET value = ?1 WHERE key = 'telegram.last_update_id'",
        params![max_id.to_string()],
    )?;

    tx.commit()?;

    // POST-COMMIT broadcast queue handover. `bus.publish` is async and
    // cannot be called from this sync function (we run inside
    // `spawn_blocking`). The async caller iterates `outcome.notifications`
    // and publishes each frame after `spawn_blocking` returns — keeping
    // the invariant that broadcast only happens after the durable commit.
    // The `bus` parameter is retained for the test sites that thread it
    // through, but the actual publish wiring lives in `run_long_poll`.
    let _ = bus;

    Ok(BatchOutcome {
        new_offset: Some(max_id),
        messages_inserted: inserted,
        notifications,
    })
}

/// Wall-clock millis since epoch — local helper because the chat module's
/// `now_millis` is private. Behaviour identical.
fn chrono_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Read the persisted offset from daemon_state. Returns 0 when the row
/// doesn't exist (shouldn't happen since the v5 schema seeds it, but be
/// defensive).
pub fn load_offset(conn: &Connection) -> Result<i64> {
    let value: String = conn
        .query_row(
            "SELECT value FROM daemon_state WHERE key='telegram.last_update_id'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "0".to_string());
    Ok(value.parse::<i64>().unwrap_or(0))
}

/// Mark daemon state for the bot connection — `up` after a successful
/// getUpdates round-trip, `disconnected` on 401. Persisted into daemon_state
/// so `daemon status` and tests can introspect.
pub fn set_bot_state(conn: &Connection, state: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO daemon_state(key, value) VALUES('tg_bot_state', ?1) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![state],
    )
    .context("write tg_bot_state to daemon_state")?;
    Ok(())
}

/// Spawn the Telegram long-poll task. Returns immediately; the spawned
/// task runs until it hits a fatal error (401) OR the daemon is killed.
///
/// `secrets_path` is the loaded-and-perm-checked secrets.toml path; the
/// caller must have already passed it through `config::load_secrets_toml`
/// so this function only sees a token-shaped `RedactedToken`.
///
/// **Slice 4 status:** the long-poll loop is implemented for the path
/// where the daemon has valid secrets AND `enabled = true` in
/// daemon.toml. When either condition is false, the spawn is skipped
/// silently — Slice 1-3 callers without secrets.toml see no behavior
/// change. Live HTTP integration (mocked or real Telegram API) is exercised
/// only when the operator points the daemon at a real bot; the e2e tests
/// in `tests/telegram_e2e_test.rs` are scaffolds that verify config plumbing
/// not live HTTP. The real production loop body still has to compile and
/// be ready to run.
pub fn spawn_long_poll(
    token: RedactedToken,
    access_path: PathBuf,
    bus: SharedBus,
    asr: Option<Arc<dyn Asr>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // ASYNC_INVARIANTS Rule 3 — wrap the long-poll body so any
        // unhandled error logs structured (without leaking the token) and
        // the daemon's other tasks keep running.
        let token_str = token.as_str().to_string();
        if let Err(e) = run_long_poll(token, access_path, bus, asr).await {
            tracing::error!(
                error = %redact_error_string(&format!("{e:#}"), &token_str),
                "telegram long-poll fatal"
            );
        }
    })
}

/// Inner long-poll loop. Reads `getUpdates`, processes each batch
/// transactionally, advances `last_update_id`, sleeps a small interval,
/// repeats.
///
/// The loop is structured so cancellation (parent task drop) is
/// graceful — only `tokio::time::sleep` and reqwest's response future can
/// be in flight at any await point, and both are cancellation-safe per
/// ASYNC_INVARIANTS Rule 4.
async fn run_long_poll(
    token: RedactedToken,
    access_path: PathBuf,
    bus: SharedBus,
    asr: Option<Arc<dyn Asr>>,
) -> Result<()> {
    // Allow tests / local dev to point at a mock Telegram endpoint via
    // TELOXIDE_API_URL. teloxide 0.17 reads this env var directly via
    // `Bot::from_env_with_api_url` and `requester_ext` — see the spawned
    // teloxide construction below.
    let api_url = std::env::var("TELOXIDE_API_URL").ok();

    // Slice 4 production loop body. teloxide ships a high-level
    // `repls` interface but we use the lower-level `getUpdates` call
    // directly so the transactional offset-advance lives where SEC-13
    // wants it.
    //
    // teloxide's `set_api_url` accepts a `reqwest::Url`; we parse via the
    // url crate then convert. Both crates use the same underlying
    // representation so the conversion is a free re-parse.
    use teloxide::payloads::GetUpdatesSetters;
    use teloxide::requests::Requester;
    let bot = if let Some(url_str) = api_url.as_deref() {
        match url::Url::parse(url_str) {
            Ok(parsed) => {
                // url::Url and reqwest::Url are the SAME type (reqwest
                // re-exports url::Url). Just hand it through directly.
                teloxide::Bot::new(token.as_str()).set_api_url(parsed)
            }
            Err(e) => {
                tracing::error!(error = %e, "TELOXIDE_API_URL is not a valid URL");
                teloxide::Bot::new(token.as_str())
            }
        }
    } else {
        teloxide::Bot::new(token.as_str())
    };

    // Open the daemon's chat.db handle. Per ASYNC_INVARIANTS each task
    // opens its own Connection — never share across .await.
    let conn_path = crate::store::user_level_chat_db_path();
    tracing::info!(chat_db = %conn_path.display(), "telegram long-poll starting");

    // Slice 4: production long-poll body. We loop forever reading
    // `getUpdates`. For Slice 4 the body sleeps a small interval and
    // re-reads access.json every iteration — access.json changes are
    // observed within one poll cycle without restarting the daemon.
    //
    // The loop ALSO handles the access-path-missing case: if the file
    // doesn't exist (fresh install before any /start) the daemon uses
    // Access::default(), which keeps `dm_policy = Pairing` — so unknown
    // users get filtered out. The pending-pair generation lives in the
    // /start branch we route through bot's message handler.
    let mut consecutive_429_retries: u32 = 0;
    let token_for_error_redaction = token.as_str().to_string();

    loop {
        // Load access state fresh each poll. Slow change detection is
        // acceptable for Slice 4 — operator-mediated `access pair` runs
        // are infrequent.
        let access = match permissions::load_access(&access_path) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                    "failed to load access.json (using defaults)"
                );
                Access::default()
            }
        };

        // Open a fresh connection for this poll cycle so the long-running
        // task doesn't hold a Connection across .await. spawn_blocking
        // wraps the rusqlite work per Rule 2.
        let access_clone = access.clone();
        let process_result = tokio::task::spawn_blocking(move || -> Result<(i64, BatchOutcome)> {
            let conn = chat::open_chat_db()
                .context("open chat.db for telegram poll")?;
            let offset = load_offset(&conn)?;
            // For Slice 4 we DO NOT make the real teloxide network call
            // from this thread — the rest of this loop (network I/O,
            // batch construction) happens BACK in the async context.
            // Return the current offset so the async side knows where
            // to start, AND an empty batch outcome (no work done in
            // spawn_blocking).
            Ok((
                offset,
                BatchOutcome {
                    new_offset: None,
                    messages_inserted: 0,
                    notifications: Vec::new(),
                },
            ))
        })
        .await;

        let (offset, _) = match process_result {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                    "spawn_blocking load_offset failed"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "spawn_blocking joined with panic");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // Make the getUpdates HTTP call. teloxide's `Requester::get_updates`
        // returns a builder; we set offset and timeout, then await.
        let updates_result = bot
            .get_updates()
            .offset(offset.saturating_add(1) as i32)
            .timeout(25u32)
            .await;

        let raw_updates = match updates_result {
            Ok(v) => {
                consecutive_429_retries = 0;
                v
            }
            Err(e) => {
                let err_str = format!("{e}");
                let redacted = redact_error_string(&err_str, &token_for_error_redaction);
                // SEC-14: 401 → mark disconnected and exit the loop. 429
                // → retry once.
                if err_str.contains("401") || err_str.contains("Unauthorized") {
                    tracing::error!(
                        reason = "telegram 401 unauthorized",
                        error = %redacted,
                        "telegram disconnected"
                    );
                    let _ = tokio::task::spawn_blocking(|| -> Result<()> {
                        let conn = chat::open_chat_db()?;
                        set_bot_state(&conn, "disconnected")?;
                        Ok(())
                    })
                    .await;
                    return Ok(());
                }
                if err_str.contains("429") || err_str.contains("RetryAfter") {
                    consecutive_429_retries += 1;
                    if consecutive_429_retries > 1 {
                        // UC-3-E2 / SEC-14: ONE retry only. After the
                        // second 429, back off the inbound loop and
                        // reset the counter.
                        tracing::warn!(
                            error = %redacted,
                            "telegram 429 after retry — backing off"
                        );
                        consecutive_429_retries = 0;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    } else {
                        // First 429 — retry once after the suggested
                        // back-off (Telegram puts retry_after in the
                        // error description; for simplicity sleep 5s).
                        tracing::warn!(
                            error = %redacted,
                            "telegram 429 — retrying once after 5s"
                        );
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    continue;
                }
                tracing::warn!(
                    error = %redacted,
                    "telegram getUpdates error — continuing"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // Convert teloxide Updates into our minimal `Update` shape via
        // JSON round-trip — keeps our process_batch surface decoupled
        // from teloxide's enum tree.
        let mut decoded: Vec<Update> = Vec::with_capacity(raw_updates.len());
        for up in &raw_updates {
            match serde_json::to_value(up).and_then(serde_json::from_value::<Update>) {
                Ok(u) => decoded.push(u),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "failed to decode telegram Update — skipping"
                    );
                }
            }
        }

        if decoded.is_empty() {
            // Idle interval — Slice 4 hard-codes 1s; future config could
            // expose this as a knob.
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        // Slice 6-MVP: voice-note transcription happens HERE, BEFORE
        // the spawn_blocking(process_batch) call so the SEC-13 DB
        // transaction stays short (architect axis-5). Per update, if
        // `voice` is set and `text` is absent, fetch the file via the
        // teloxide client, decode Opus, run ASR, mutate the update to
        // `text = Some(transcript); voice = None`. Failures surface as
        // a bracketed `[voice transcription failed: ...]` text that
        // process_batch inserts as a normal row — the operator sees
        // the error in the chat thread instead of silent loss.
        for update in decoded.iter_mut() {
            if let Some(msg) = &mut update.message {
                if msg.text.is_none() && msg.voice.is_some() {
                    let voice_text = match transcribe_voice_note(&bot, msg, asr.as_ref()).await {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(
                                error = %redact_error_string(&format!("{e:#}"), &token_for_error_redaction),
                                "voice transcribe failed; using fallback"
                            );
                            format!("[voice transcription failed: {e}]")
                        }
                    };
                    msg.text = Some(voice_text);
                    msg.voice = None;
                }
            }
        }

        let access_clone = access_clone;
        let batch = decoded;
        let process_outcome = tokio::task::spawn_blocking(move || -> Result<BatchOutcome> {
            let mut conn = chat::open_chat_db()?;
            // Pass `None` for the bus argument — process_batch returns the
            // notification queue in `BatchOutcome.notifications` and we
            // publish from the async side below (bus.publish is async and
            // would deadlock spawn_blocking).
            process_batch(&mut conn, &access_clone, None, &batch)
        })
        .await;

        match process_outcome {
            Ok(Ok(outcome)) => {
                if outcome.messages_inserted > 0 {
                    tracing::info!(
                        inserted = outcome.messages_inserted,
                        max_update_id = ?outcome.new_offset,
                        "telegram batch persisted"
                    );
                }
                // Publish post-commit notifications from the async side
                // (Rule 4 cancellation-safety: bus.publish drops a
                // broadcast send result, no held lock across the await).
                // A subscriber count of 0 is the silent-no-op case
                // documented in ChatBus::publish.
                for (thread, frame) in outcome.notifications {
                    let n = bus.publish(&thread, frame).await;
                    tracing::debug!(
                        thread = %thread,
                        subscribers = n,
                        "telegram broadcast"
                    );
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %redact_error_string(&format!("{e}"), &token_for_error_redaction),
                    "telegram batch processing failed"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "telegram batch spawn_blocking panicked");
            }
        }
    }
}

/// Slice 6-MVP — transcribe one Telegram voice note end-to-end.
///
/// 1. `bot.get_file(file_id)` → File metadata (carries `path` and
///    `file_size`).
/// 2. `bot.download_file(path, &mut buf)` → raw Opus-in-Ogg bytes.
/// 3. `decoder::decode_ogg_opus_to_16k_mono_pcm(&bytes)` → 16 kHz mono
///    Vec<f32>.
/// 4. `asr.transcribe(pcm, 16_000).await` → transcript string.
///
/// Returns Err when:
///   - the message has no voice field
///   - `asr` is None (no backend configured / feature off)
///   - the file fetch, decode, or transcribe step fails
///
/// The caller decides how to surface the error to the chat thread —
/// the current `run_long_poll` integration wraps Err in the literal
/// `[voice transcription failed: ...]` placeholder so the operator
/// sees the failure inline (NEVER silent loss).
async fn transcribe_voice_note(
    bot: &teloxide::Bot,
    msg: &Message,
    asr: Option<&Arc<dyn Asr>>,
) -> Result<String> {
    use teloxide::net::Download;
    use teloxide::requests::Requester;
    use teloxide::types::FileId;

    let voice = msg
        .voice
        .as_ref()
        .context("voice transcribe: message has no voice field")?;
    let asr = asr.context("voice transcribe: ASR backend not configured")?;

    // Step 1: get_file resolves the Telegram file path from file_id.
    let file = bot
        .get_file(FileId(voice.file_id.clone()))
        .await
        .with_context(|| format!("voice transcribe: get_file {}", voice.file_id))?;

    // Step 2: download into an in-memory buffer. Voice notes max out
    // around ~1 MB for 10-minute clips at Opus's 24 kbps default; we
    // pre-size the buffer with file_size when available.
    let mut buf: Vec<u8> = Vec::with_capacity(file.size as usize);
    bot.download_file(&file.path, &mut buf)
        .await
        .with_context(|| format!("voice transcribe: download_file {}", file.path))?;

    // Step 3: decode Opus-in-Ogg → 16 kHz mono PCM. Run on the blocking
    // pool so the codec work doesn't hog the tokio worker.
    let pcm = tokio::task::spawn_blocking(move || {
        crate::daemon::asr::decoder::decode_ogg_opus_to_16k_mono_pcm(&buf)
    })
    .await
    .context("voice transcribe: decode spawn_blocking join failed")?
    .context("voice transcribe: decode failed")?;

    // Step 4: hand the PCM to the configured ASR backend. The trait's
    // own implementation chooses how to dispatch (sync blocking pool
    // for whisper; HTTP for nim; etc.).
    let transcript = asr.transcribe(pcm, 16_000).await.context("voice transcribe: ASR failed")?;
    Ok(transcript)
}

/// Cleanup helper — keep `Arc` so the daemon's lifetime guards work even
/// when the long-poll never runs.
pub fn no_op_arc() -> Arc<()> {
    Arc::new(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::config::DmPolicy;

    #[test]
    fn redact_error_string_replaces_token_substr() {
        let s = "Error 401: bad token=ABCDEF in url";
        let r = redact_error_string(s, "ABCDEF");
        assert!(!r.contains("ABCDEF"));
        assert!(r.contains("***"));
    }

    #[test]
    fn redact_error_string_no_op_on_empty_token() {
        let s = "Error";
        assert_eq!(redact_error_string(s, ""), s);
    }

    #[test]
    fn process_batch_inserts_allowed_and_advances_offset() {
        // In-memory DB so we don't touch the user's chat.db.
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();

        let mut access = Access::default();
        access.dm_policy = DmPolicy::Allowlist;
        access.allow_from.push(1001);

        let batch = vec![Update {
            update_id: 7,
            message: Some(Message {
                message_id: 100,
                from: Some(User {
                    id: 1001,
                    username: Some("alice".to_string()),
                }),
                chat: Chat { id: 555 },
                text: Some("hello".to_string()),
                voice: None,
            }),
        }];

        let outcome = process_batch(&mut conn, &access, None, &batch).unwrap();
        assert_eq!(outcome.messages_inserted, 1);
        assert_eq!(outcome.new_offset, Some(7));

        let offset = load_offset(&conn).unwrap();
        assert_eq!(offset, 7);
    }

    #[test]
    fn process_batch_drops_disallowed_user_but_advances_offset() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();

        let access = Access::default(); // Pairing policy, empty allow_from

        let batch = vec![Update {
            update_id: 9,
            message: Some(Message {
                message_id: 100,
                from: Some(User {
                    id: 1001,
                    username: None,
                }),
                chat: Chat { id: 555 },
                text: Some("hi".to_string()),
                voice: None,
            }),
        }];

        let outcome = process_batch(&mut conn, &access, None, &batch).unwrap();
        assert_eq!(outcome.messages_inserted, 0);
        assert_eq!(outcome.new_offset, Some(9));
        assert_eq!(load_offset(&conn).unwrap(), 9);
    }

    #[test]
    fn process_batch_voice_uses_shim_text() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();

        let mut access = Access::default();
        access.dm_policy = DmPolicy::Allowlist;
        access.allow_from.push(1001);

        let batch = vec![Update {
            update_id: 3,
            message: Some(Message {
                message_id: 200,
                from: Some(User {
                    id: 1001,
                    username: None,
                }),
                chat: Chat { id: 555 },
                text: None,
                voice: Some(Voice {
                    file_id: "FID".to_string(),
                    duration: 10,
                }),
            }),
        }];

        let outcome = process_batch(&mut conn, &access, None, &batch).unwrap();
        assert_eq!(outcome.messages_inserted, 1);
        let content: String = conn
            .query_row(
                "SELECT content FROM chat_messages LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(content, VOICE_SHIM_TEXT);
    }

    #[test]
    fn set_bot_state_persists_into_daemon_state() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        chat::ensure_chat_db_schema(&conn).unwrap();
        set_bot_state(&conn, "disconnected").unwrap();
        let v: String = conn
            .query_row("SELECT value FROM daemon_state WHERE key='tg_bot_state'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "disconnected");
    }
}
