//! Long-lived daemon client for the PTY supervisor.
//!
//! Sibling of `client.rs`, deliberately separate: `client.rs` is a one-shot
//! request/response used by short CLI commands, this one holds a connection
//! open for the whole session, keeps a subscription, and streams notifications.
//! Merging them would give one type two lifecycles.
//!
//! This replaces the notification half of `src/plugin/bridge.rs` — same daemon
//! wire protocol, but the destination is a PTY instead of Claude Code's MCP
//! stdio.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use crate::daemon::ipc::{read_frame, write_frame};
use crate::daemon::server::socket_path;
use crate::supervisor::{
    inject::{render, Source},
    InboundMessage,
};

/// Timeout for a single tool call made during setup (register / subscribe).
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the pump re-scans for threads that appeared after this connection
/// subscribed.
///
/// Subscriptions used to be computed once per connection, which broke the very
/// first run: a chat only becomes a thread when its first message is accepted,
/// so a session started before pairing never learned about the operator's own
/// chat and silently received nothing until it was restarted. The re-scan is
/// what makes "pair, then message me" work without a restart.
const RESCAN_INTERVAL: Duration = Duration::from_secs(15);

pub struct SubscribeClient {
    read: tokio::io::ReadHalf<interprocess::local_socket::tokio::Stream>,
    write: tokio::io::WriteHalf<interprocess::local_socket::tokio::Stream>,
    next_id: u64,
    subscribed: std::collections::HashSet<String>,
    /// Request-id -> thread for subscribes issued by the re-scan, so their
    /// responses can be recognised in the pump and their BACKLOG delivered.
    pending_subscribes: std::collections::HashMap<u64, String>,
}

impl SubscribeClient {
    pub async fn connect() -> Result<Self> {
        use interprocess::local_socket::tokio::prelude::*;
        use interprocess::local_socket::tokio::Stream;
        use interprocess::local_socket::{GenericFilePath, ToFsName};

        let socket = socket_path().context("compute daemon socket path")?;
        let name = socket
            .clone()
            .to_fs_name::<GenericFilePath>()
            .context("socket name")?;
        let stream = Stream::connect(name)
            .await
            .with_context(|| format!("connect {}", socket.display()))?;
        let (read, write) = tokio::io::split(stream);
        Ok(Self {
            read,
            write,
            next_id: 1,
            subscribed: std::collections::HashSet::new(),
            pending_subscribes: std::collections::HashMap::new(),
        })
    }

    /// Setup-time tool call. Notifications arriving in the middle are ignored
    /// here — the subscription is not live until `pump` starts, and the daemon
    /// only broadcasts to threads we have already subscribed to.
    pub async fn call(&mut self, tool: &str, arguments: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        }))?;
        write_frame(&mut self.write, &body).await?;

        let deadline = tokio::time::Instant::now() + CALL_TIMEOUT;
        loop {
            let frame = tokio::time::timeout_at(deadline, read_frame(&mut self.read))
                .await
                .with_context(|| format!("daemon did not answer `{tool}`"))??;
            let value: Value = serde_json::from_slice(&frame)?;
            if value.get("id").and_then(|v| v.as_u64()) != Some(id) {
                continue;
            }
            if let Some(err) = value.get("error") {
                bail!(
                    "daemon rejected `{tool}`: {}",
                    err.get("message").and_then(|m| m.as_str()).unwrap_or("?")
                );
            }
            return Ok(value);
        }
    }

    /// Subscribe to every thread this session should hear about, skipping the
    /// ones this connection already holds.
    ///
    /// Startup and re-scan take the SAME path on purpose. An earlier version
    /// used a blocking `call()` here, which returned the subscribe response —
    /// and with it the thread's backlog — straight into `let _ = …`. The
    /// backlog is not decoration: it carries the message that created the
    /// thread, which in the real flow is the first message after pairing.
    /// Routing both paths through `pump` means one place delivers it.
    pub async fn subscribe_all(
        &mut self,
        identity: &crate::supervisor::AgentIdentity,
        extra_threads: &[String],
    ) {
        self.subscribe_new_threads(identity, extra_threads).await;
    }

    /// Issue subscribes for threads this connection does not hold yet.
    ///
    /// Fire-and-forget: the responses land on the same socket and `pump`
    /// recognises them by request id, so it can deliver their backlog. Waiting
    /// here would stall a message already in flight. Failures are logged and
    /// retried on the next tick — a transient error must not tear down a
    /// working connection.
    async fn subscribe_new_threads(
        &mut self,
        identity: &crate::supervisor::AgentIdentity,
        extra_threads: &[String],
    ) {
        let wanted = crate::supervisor::threads_to_subscribe(identity, extra_threads);
        let fresh: Vec<String> = wanted
            .into_iter()
            .filter(|t| !self.subscribed.contains(t))
            .collect();
        if fresh.is_empty() {
            return;
        }
        for thread in fresh {
            let id = self.next_id;
            self.next_id += 1;
            let body = match serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": "chat_subscribe", "arguments": { "thread": thread } },
            })) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "rescan: serialise failed");
                    continue;
                }
            };
            // Fire-and-forget: the response arrives on the same socket and the
            // pump's notification filter skips it. Waiting here would stall
            // delivery of a message already in flight.
            if let Err(e) = write_frame(&mut self.write, &body).await {
                tracing::warn!(error = %e, %thread, "rescan: subscribe write failed");
                return;
            }
            self.pending_subscribes.insert(id, thread.clone());
            self.subscribed.insert(thread.clone());
            tracing::info!(%thread, "subscribed (new thread appeared)");
        }
    }

    /// Inject the messages a `chat_subscribe` response handed back.
    ///
    /// Messages the session itself sent are skipped for the same reason live
    /// ones are (F-7 echo loop): an agent must not read its own words as input.
    fn deliver_backlog(
        &self,
        response: &Value,
        thread: &str,
        self_agent_id: &str,
        tx: &Sender<InboundMessage>,
    ) {
        // The chat may have been `/switch`-ed to another session between our
        // subscribe and its answer. Injecting then would put the operator's
        // conversation into a session they did not choose.
        if !crate::supervisor::thread_belongs_to(self_agent_id, thread) {
            tracing::info!(%thread, "backlog skipped — chat is bound to another session");
            return;
        }

        let messages = backlog_messages(response);
        if messages.is_empty() {
            return;
        }
        let (messages, suppressed) = bound_backlog(messages, crate::daemon::chat::now_millis());
        if suppressed > 0 {
            tracing::warn!(
                %thread,
                suppressed,
                "backlog trimmed — older/excess queued messages were NOT injected"
            );
        }
        if messages.is_empty() {
            return;
        }

        let source = if thread.starts_with("telegram:") {
            Source::Telegram
        } else {
            Source::Agent {
                nick: "peer".to_string(),
            }
        };

        let mut delivered = 0usize;
        for msg in messages {
            let (message_id, content) = (msg.id, msg.content);
            let body = render(source.clone(), &content);
            if tx
                .send(InboundMessage { message_id, body, priority: false })
                .is_err()
            {
                return;
            }
            delivered += 1;
        }
        if delivered > 0 {
            tracing::info!(%thread, delivered, "delivered backlog from subscribe");
        }
    }

    /// Stream notifications until the connection ends.
    ///
    /// Filtering mirrors `bridge.rs::should_relay_channel_notification`: a
    /// frame addressed to a specific agent (`meta.target_agent_id`) is only for
    /// that agent; everything else is broadcast and belongs to us.
    pub async fn pump(
        &mut self,
        identity: &crate::supervisor::AgentIdentity,
        extra_threads: &[String],
        tx: &Sender<InboundMessage>,
        done: &Arc<AtomicBool>,
    ) -> Result<()> {
        let self_agent_id = identity.agent_id.as_str();
        let mut last_scan = tokio::time::Instant::now();
        loop {
            if done.load(Ordering::SeqCst) {
                return Ok(());
            }
            if last_scan.elapsed() >= RESCAN_INTERVAL {
                last_scan = tokio::time::Instant::now();
                self.subscribe_new_threads(identity, extra_threads).await;
            }
            // Bounded wait so session shutdown is noticed even on a silent
            // socket.
            let frame = match tokio::time::timeout(
                Duration::from_millis(500),
                read_frame(&mut self.read),
            )
            .await
            {
                Ok(r) => r.context("daemon connection closed")?,
                Err(_) => continue,
            };

            let value: Value = match serde_json::from_slice(&frame) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "daemon sent a non-JSON frame; ignoring");
                    continue;
                }
            };

            // A response to a re-scan subscribe carries the thread's BACKLOG.
            // Dropping it loses exactly the message that created the thread —
            // in the real flow, the first message after pairing, which is the
            // one the operator is watching for.
            if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                if let Some(thread) = self.pending_subscribes.remove(&id) {
                    self.deliver_backlog(&value, &thread, self_agent_id, tx);
                }
                continue;
            }

            if value.get("method").and_then(|m| m.as_str())
                != Some("notifications/claude/channel")
            {
                continue;
            }
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let meta = params.get("meta").cloned().unwrap_or(Value::Null);

            // EVERY message names one recipient, and only that recipient is
            // delivered to. An absent recipient is not "send to whoever
            // subscribed" — it is a message with nobody to receive it, and it is
            // dropped.
            //
            // The old rule enforced the target only when it happened to be
            // present, so anything published without one fell through to
            // delivery-by-subscription. That is how sessions read each other's
            // replies to the operator: publishing to a thread was treated as
            // permission to send to everyone listening on it. Being subscribed
            // is not the same as being addressed.
            let target = meta.get("target_agent_id").and_then(|v| v.as_str());
            match target {
                Some(t) if t == self_agent_id => {}
                Some(t) => {
                    tracing::debug!(target = %t, "addressed to another agent; skipping");
                    continue;
                }
                None => {
                    tracing::debug!(
                        thread = %meta.get("thread").and_then(|v| v.as_str()).unwrap_or("?"),
                        "no recipient named; not delivering to anyone"
                    );
                    continue;
                }
            }

            // F-7 (live e2e 2026-08-16): the daemon broadcasts an outbound
            // `chat_reply` to the SAME thread the agent just answered on, so
            // without this filter the agent's own reply is injected back into
            // its own input — an echo loop where each turn re-triggers the
            // next. Observed on the first end-to-end run: the reply "понял"
            // came back as a `<channel …>` block seconds after being sent.
            //
            // Filtering is by exact sender identity, NOT by "is it from an
            // agent": messages from OTHER agents (`claudebase agent chat`) are
            // legitimate inbound traffic and must still arrive.
            if meta.get("from_agent").and_then(|v| v.as_str()) == Some(self_agent_id) {
                tracing::debug!("own outbound message echoed back by the bus; skipping");
                continue;
            }

            let raw_content = params
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or_default();
            if raw_content.is_empty() {
                continue;
            }
            let (content, sender_hint) = split_agent_preamble(raw_content);

            // The operator's "continue" button. Stated explicitly in the meta,
            // never inferred: it changes how the message is DELIVERED (past the
            // gate, promoted out of the queue), so guessing it from content
            // would let any message claim that power.
            let is_control = meta.get("control").and_then(|c| c.as_str()) == Some("continue");

            let (thread, source) = classify(&meta, sender_hint.clone());

            // On a TELEGRAM thread only the operator is inbound. Every other
            // sender there is an agent whose reply the daemon published to the
            // same thread — an OUTBOUND message to the operator, not something
            // addressed to us.
            //
            // The filter above catches only our OWN id, which was right for
            // `agent:` threads (a peer's message there is legitimate inbound)
            // and wrong for telegram ones. Observed live on 2026-08-18: a
            // sibling session answered the operator, its reply arrived here
            // marked `[telegram_message]`, and both sessions then answered each
            // other through the operator's chat while the operator watched.
            // `chat_messages` shows eight distinct agent ids that have written
            // to that thread, so every one of them was visible to whichever
            // session held the binding.
            if thread.starts_with("telegram:") {
                if let Some(from) = meta.get("from_agent").and_then(|v| v.as_str()) {
                    if !from.starts_with("telegram:") {
                        tracing::debug!(
                            %thread,
                            from,
                            "agent outbound on a telegram thread; not inbound for us"
                        );
                        continue;
                    }
                }
            }

            let message_id = meta
                .get("message_id")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();

            // Same binding check as the backlog path: a subscription outlives
            // a `/switch` (there is no unsubscribe), so ownership is re-checked
            // per message rather than once at subscribe time.
            if !crate::supervisor::thread_belongs_to(self_agent_id, &thread) {
                tracing::debug!(thread = %thread, "message belongs to another session; skipping");
                continue;
            }

            let body = render(source, &content);

            if tx
                .send(InboundMessage {
                    message_id,
                    body,
                    priority: is_control,
                })
                .is_err()
            {
                // Injector is gone — the session is ending.
                return Ok(());
            }
        }
    }
}


/// Decide which channel a notification came from, and its thread id.
///
/// The daemon has THREE notification shapes and they disagree about where the
/// thread lives — which is how a Telegram message arrived labelled
/// `[agent-to-agent:codefath]`, the first 8 characters of the sender's Telegram
/// username, on 2026-08-17:
///
/// * Telegram inbound (`build_channel_notification_telegram`) puts the NUMERIC
///   chat id in `meta.chat_id`, as a string — no `telegram:` prefix, no
///   `meta.thread`.
/// * Agent-to-agent (`build_channel_notification_agent_to_agent`) reuses
///   `meta.chat_id` for the thread NAME, `agent:<id>`.
/// * Posts and replies (`build_channel_notification`) use `meta.thread`, which
///   does carry the `telegram:` / `agent:` prefix.
///
/// Classifying on a `telegram:` prefix alone therefore misread every real
/// Telegram message. Order matters: an explicit prefix wins, then the
/// `agent:` shape, then a bare numeric chat id — which only Telegram produces.
fn classify(meta: &Value, sender_hint: Option<String>) -> (String, Source) {
    let field = |key: &str| meta.get(key).and_then(|v| v.as_str()).map(|s| s.to_string());
    let thread_field = field("thread");
    let chat_field = field("chat_id");

    let agent_source = |meta: &Value| {
        let sender = sender_hint
            .clone()
            .or_else(|| field("user"))
            .or_else(|| field("from_agent"))
            .unwrap_or_default();
        let _ = meta;
        Source::Agent {
            nick: nick_for(&sender),
        }
    };

    // An explicit source beats every inference below. The callback builder
    // states it outright precisely because inferring the source from the shape
    // of some other field is what produced F-14.
    // An explicit `source` is authoritative for EVERY value, not just callback.
    // The control frame states `source: "telegram"` because it carries the
    // operator's own words, but it rides an `agent:<id>` thread to reach one
    // specific session — so reading the thread prefix instead labelled it
    // `[agent-to-agent:unknown]`. Setting a field and then not reading it is the
    // same mistake as F-14, one field later.
    if field("source").as_deref() == Some("telegram") {
        let thread = thread_field
            .clone()
            .or_else(|| chat_field.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let is_voice = meta.get("voice").and_then(|v| v.as_bool()).unwrap_or(false);
        return (
            thread,
            if is_voice {
                Source::TelegramVoice
            } else {
                Source::Telegram
            },
        );
    }

    if field("source").as_deref() == Some("callback") {
        let label = field("label").filter(|l| !l.is_empty());
        let thread = thread_field
            .clone()
            .or_else(|| chat_field.clone())
            .unwrap_or_else(|| "unknown".to_string());
        return (thread, Source::Callback { label });
    }

    // Explicit, never inferred: the transcript travels the ordinary text path,
    // so only this flag distinguishes a dictated note from a typed one.
    let is_voice = meta.get("voice").and_then(|v| v.as_bool()).unwrap_or(false);
    let telegram_source = || {
        if is_voice {
            Source::TelegramVoice
        } else {
            Source::Telegram
        }
    };

    if let Some(thread) = thread_field {
        if thread.starts_with("telegram:") {
            return (thread, telegram_source());
        }
        if thread.starts_with("agent:") {
            return (thread.clone(), agent_source(meta));
        }
    }
    if let Some(chat) = chat_field {
        if chat.starts_with("agent:") {
            return (chat.clone(), agent_source(meta));
        }
        if chat.starts_with("telegram:") {
            return (chat, telegram_source());
        }
        // A bare numeric id is a Telegram chat; normalise it to the thread name
        // the rest of the supervisor uses (`thread_belongs_to`, subscriptions).
        if chat.parse::<i64>().is_ok() {
            return (format!("telegram:{chat}"), telegram_source());
        }
    }
    ("unknown".to_string(), agent_source(meta))
}

/// Resolve an `agent_id` to its registered nick, falling back to the id when
/// the registry has no row (a peer that unregistered between send and
/// delivery) — a truncated uuid is still better than an empty sender slot.
fn nick_for(agent_id: &str) -> String {
    if agent_id.is_empty() {
        return "unknown".to_string();
    }

    // Read-only: a name lookup must not open a write transaction (see
    // `open_chat_db_readonly`), or it competes with the daemon storing the very
    // message being labelled.
    let looked_up = crate::daemon::chat::open_chat_db_readonly()
        .and_then(|conn| {
            conn.query_row(
                "SELECT agent_name FROM agent_registry WHERE agent_id = ?1",
                [agent_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(anyhow::Error::from)
        })
        .unwrap_or_else(|e| {
            // Distinguishable in the log from a genuinely unknown sender, which
            // returns Ok(None) and says nothing.
            tracing::warn!(agent_id, error = %e, "could not read agent nick; falling back to id");
            None
        });

    // The fallback is the FULL id on purpose. `agent_registry::resolve_target`
    // accepts a whole agent_id but never a prefix, so an 8-character stub was a
    // label the recipient could not reply to — the same defect that made
    // `codefath` undeliverable in F-14.
    looked_up.unwrap_or_else(|| agent_id.to_string())
}

/// Agent-to-agent frames carry a one-line JSON preamble followed by a blank
/// line and then the human message (`build_channel_notification_agent_to_agent`
/// in chat.rs). Split it so the pasted block shows the message, not the
/// bookkeeping, and surface the sender id for the envelope.
///
/// Anything that does not match that exact shape is passed through untouched —
/// a Telegram message that merely happens to start with `{` must not be
/// mangled.
fn split_agent_preamble(raw: &str) -> (String, Option<String>) {
    let Some((head, rest)) = raw.split_once("\n\n") else {
        return (raw.to_string(), None);
    };
    let Ok(parsed) = serde_json::from_str::<Value>(head) else {
        return (raw.to_string(), None);
    };
    let Some(a2a) = parsed.get("agent_to_agent") else {
        return (raw.to_string(), None);
    };
    let from = a2a
        .get("from_agent_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (rest.to_string(), from)
}

/// Extract `(message_id, content)` pairs from a `chat_subscribe` response.
///
/// Split out from the delivery path because this parsing is where the original
/// bug lived: the backlog is a JSON document nested as TEXT inside the MCP
/// result envelope, and the earlier code simply dropped the whole response. It
/// matters because `chat_subscribe` MARKS the backlog delivered daemon-side —
/// a client that discards it loses those messages permanently.
fn backlog_messages(response: &Value) -> Vec<BacklogMessage> {
    let Some(text) = response.pointer("/result/content/0/text").and_then(|t| t.as_str()) else {
        return Vec::new();
    };
    let Ok(payload) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(messages) = payload.get("messages").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    messages
        .iter()
        .filter_map(|m| {
            let content = m.get("content").and_then(|c| c.as_str())?;
            if content.is_empty() {
                return None;
            }
            Some(BacklogMessage {
                id: m
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string(),
                content: content.to_string(),
                created_at: m.get("created_at").and_then(|t| t.as_i64()).unwrap_or(0),
            })
        })
        .collect()
}

/// One message handed back by `chat_subscribe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklogMessage {
    pub id: String,
    pub content: String,
    /// UNIX millis. `0` when the daemon did not report one — treated as old.
    pub created_at: i64,
}

/// Backlog older than this is history, not a pending conversation. Replaying it
/// into a fresh session is worse than dropping it: the operator sees an old
/// exchange arrive as if it were new, and the agent answers questions that were
/// settled hours ago.
const BACKLOG_MAX_AGE_MS: i64 = 10 * 60 * 1000;

/// Hard cap on how many queued messages a single subscribe may inject.
///
/// Without one, `chat_subscribe` hands back EVERY undelivered message for the
/// thread (`chat.rs::drain_backlog` has no LIMIT), so a thread that accumulated
/// while delivery was broken dumps its whole history into the next session that
/// subscribes — observed live 2026-08-16.
const BACKLOG_MAX_MESSAGES: usize = 5;

/// Keep only what a session should actually be told about on connect.
///
/// Returns the messages to inject and how many were suppressed, so the caller
/// can say so rather than silently swallowing them (they are already marked
/// delivered daemon-side by the drain, so "silent" would mean "lost").
pub fn bound_backlog(
    messages: Vec<BacklogMessage>,
    now_ms: i64,
) -> (Vec<BacklogMessage>, usize) {
    let total = messages.len();
    let mut fresh: Vec<BacklogMessage> = messages
        .into_iter()
        .filter(|m| m.created_at > 0 && now_ms.saturating_sub(m.created_at) <= BACKLOG_MAX_AGE_MS)
        .collect();
    // Keep the NEWEST ones when over the cap: the tail of a conversation is
    // what a joining session needs, not its beginning.
    if fresh.len() > BACKLOG_MAX_MESSAGES {
        fresh = fresh.split_off(fresh.len() - BACKLOG_MAX_MESSAGES);
    }
    let suppressed = total - fresh.len();
    (fresh, suppressed)
}

#[cfg(test)]
mod tests {
    use super::{backlog_messages, split_agent_preamble};
    use serde_json::json;

    /// Shape a real daemon returns: payload as a JSON STRING inside
    /// `result.content[0].text`.
    fn subscribe_response(messages: serde_json::Value) -> serde_json::Value {
        let payload = json!({ "thread": "telegram:42", "messages": messages }).to_string();
        json!({ "jsonrpc": "2.0", "id": 7, "result": { "content": [{ "type": "text", "text": payload }] } })
    }

    #[test]
    fn extracts_the_backlog_from_the_nested_text_payload() {
        let resp = subscribe_response(json!([
            { "id": "m1", "content": "первое" },
            { "id": "m2", "content": "второе" },
        ]));
        let got = backlog_messages(&resp);
        assert_eq!(
            got.iter().map(|m| (m.id.as_str(), m.content.as_str())).collect::<Vec<_>>(),
            vec![("m1", "первое"), ("m2", "второе")]
        );
    }

    #[test]
    fn an_empty_backlog_yields_nothing() {
        assert!(backlog_messages(&subscribe_response(json!([]))).is_empty());
    }

    #[test]
    fn a_response_without_a_backlog_is_not_a_panic() {
        for resp in [
            json!({ "jsonrpc": "2.0", "id": 1, "result": {} }),
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32603, "message": "boom" } }),
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "text": "not json" }] } }),
        ] {
            assert!(backlog_messages(&resp).is_empty(), "unexpected parse of {resp}");
        }
    }

    #[test]
    fn messages_with_empty_content_are_skipped() {
        let resp = subscribe_response(json!([
            { "id": "m1", "content": "" },
            { "id": "m2", "content": "ok" },
        ]));
        let got = backlog_messages(&resp);
        assert_eq!(
            got.iter().map(|m| (m.id.as_str(), m.content.as_str())).collect::<Vec<_>>(),
            vec![("m2", "ok")]
        );
    }

    #[test]
    fn strips_the_agent_to_agent_preamble_and_extracts_the_sender() {
        let raw = "{\"agent_to_agent\":{\"from_agent_id\":\"peer-1\"}}\n\nпривет";
        let (body, from) = split_agent_preamble(raw);
        assert_eq!(body, "привет");
        assert_eq!(from.as_deref(), Some("peer-1"));
    }

    #[test]
    fn leaves_ordinary_messages_alone() {
        let raw = "первая строка\n\nвторая строка";
        let (body, from) = split_agent_preamble(raw);
        assert_eq!(body, raw, "a normal two-paragraph message must survive intact");
        assert!(from.is_none());
    }

    #[test]
    fn json_that_is_not_a_preamble_is_not_stripped() {
        let raw = "{\"some\":\"payload\"}\n\nтекст";
        let (body, _) = split_agent_preamble(raw);
        assert_eq!(body, raw);
    }
}

#[cfg(test)]
mod backlog_bounds_tests {
    use super::{bound_backlog, BacklogMessage};

    fn msg(id: &str, age_ms: i64, now: i64) -> BacklogMessage {
        BacklogMessage {
            id: id.to_string(),
            content: format!("body-{id}"),
            created_at: now - age_ms,
        }
    }

    #[test]
    fn old_messages_are_not_replayed() {
        let now = 1_000_000_000;
        let (kept, suppressed) = bound_backlog(
            vec![msg("old", 60 * 60 * 1000, now), msg("fresh", 1_000, now)],
            now,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "fresh");
        assert_eq!(suppressed, 1, "the hour-old message must be reported, not silently dropped");
    }

    #[test]
    fn a_burst_is_capped_and_keeps_the_newest() {
        let now = 1_000_000_000;
        let msgs: Vec<_> = (0..20).map(|i| msg(&format!("m{i}"), 20 - i, now)).collect();
        let (kept, suppressed) = bound_backlog(msgs, now);
        assert_eq!(kept.len(), 5, "cap the dump — a whole history is not context");
        assert_eq!(kept.last().unwrap().id, "m19", "keep the tail of the conversation");
        assert_eq!(suppressed, 15);
    }

    #[test]
    fn messages_without_a_timestamp_are_treated_as_history() {
        let now = 1_000_000_000;
        let (kept, suppressed) = bound_backlog(
            vec![BacklogMessage { id: "x".into(), content: "c".into(), created_at: 0 }],
            now,
        );
        assert!(kept.is_empty(), "unknown age must not be replayed");
        assert_eq!(suppressed, 1);
    }

    #[test]
    fn a_normal_small_backlog_passes_through() {
        let now = 1_000_000_000;
        let (kept, suppressed) = bound_backlog(vec![msg("a", 500, now), msg("b", 100, now)], now);
        assert_eq!(kept.len(), 2);
        assert_eq!(suppressed, 0);
    }
}

#[cfg(test)]
mod classify_tests {
    use super::{classify, Source};
    use crate::daemon::chat::{
        build_channel_notification_agent_to_agent, build_channel_notification_telegram,
        TelegramMessageMeta,
    };

    fn meta_of(frame: &serde_json::Value) -> serde_json::Value {
        frame.pointer("/params/meta").cloned().expect("meta")
    }

    /// Built from the REAL producer, not a hand-written fixture: the bug was
    /// exactly a mismatch between what the daemon emits and what this module
    /// assumed it emits.
    #[test]
    fn a_real_telegram_notification_is_telegram() {
        let tg = TelegramMessageMeta {
            chat_id: 434566766,
            message_id_str: "42".to_string(),
            user: "codefather_dev".to_string(),
            user_id: "434566766".to_string(),
            ts_iso8601: "2026-08-17T09:00:00.000Z".to_string(),
            is_voice: false,
            thread_id: None,
        };
        let frame = build_channel_notification_telegram("привет", &tg, None);
        let (thread, source) = classify(&meta_of(&frame), None);

        assert_eq!(source, Source::Telegram, "a Telegram message must not be labelled agent-to-agent");
        assert_eq!(
            thread, "telegram:434566766",
            "the bare numeric chat id must be normalised to the thread name the rest of the supervisor uses"
        );
    }

    /// Built with the REAL builder: a transcript is an ordinary Telegram text
    /// message in every respect except one flag, so a fixture asserting the flag
    /// by hand would prove nothing about what the daemon emits.
    #[test]
    fn a_real_voice_notification_is_marked_as_voice() {
        let tg = crate::daemon::chat::TelegramMessageMeta {
            chat_id: 434566766,
            message_id_str: "43".to_string(),
            user: "codefather_dev".to_string(),
            user_id: "434566766".to_string(),
            ts_iso8601: "2026-08-18T14:42:13.000Z".to_string(),
            thread_id: None,
            is_voice: true,
        };
        let frame = crate::daemon::chat::build_channel_notification_telegram(
            "1,2,3,4 тест голоса",
            &tg,
            None,
        );
        let (thread, source) = classify(&meta_of(&frame), None);
        assert_eq!(thread, "telegram:434566766");
        assert_eq!(source, Source::TelegramVoice);
    }

    /// And typed text must NOT acquire the marker — the flag is emitted only
    /// for voice so the text meta keeps its baseline shape.
    #[test]
    fn typed_text_is_not_marked_as_voice() {
        let tg = crate::daemon::chat::TelegramMessageMeta {
            chat_id: 434566766,
            message_id_str: "44".to_string(),
            user: "codefather_dev".to_string(),
            user_id: "434566766".to_string(),
            ts_iso8601: "2026-08-18T14:42:14.000Z".to_string(),
            thread_id: None,
            is_voice: false,
        };
        let frame = crate::daemon::chat::build_channel_notification_telegram("печатал", &tg, None);
        let meta = meta_of(&frame);
        assert!(meta.get("voice").is_none(), "text meta must stay unchanged");
        assert_eq!(classify(&meta, None).1, Source::Telegram);
    }

    /// A sibling session answering the operator must not land in OUR input.
    ///
    /// The daemon publishes an agent's outbound reply to the same telegram
    /// thread it was sent on. The old filter dropped only our own id, so every
    /// other session's replies arrived here marked `[telegram_message]` — and on
    /// 2026-08-18 two sessions spent several turns answering each other through
    /// the operator's chat, each believing the other's words were the operator's.
    #[test]
    fn another_agents_reply_to_the_operator_is_not_our_inbound() {
        let msg = crate::daemon::chat::ChatMessage {
            id: "m1".to_string(),
            thread_id: "telegram:434566766".to_string(),
            from_agent: "404f985e-4aa7-4242-8524-e9285401becb".to_string(),
            content: "answer meant for the operator".to_string(),
            reply_to: None,
            created_at: 0,
        };
        let frame = crate::daemon::chat::build_channel_notification(&msg);
        let meta = meta_of(&frame);
        let from = meta.get("from_agent").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            !from.starts_with("telegram:"),
            "an agent id is what marks this as outbound; the guard keys on that"
        );
    }

    /// And the operator's own message on the same thread IS inbound — the guard
    /// must not swallow the traffic it exists to carry.
    #[test]
    fn the_operator_on_a_telegram_thread_is_still_inbound() {
        let msg = crate::daemon::chat::ChatMessage {
            id: "m2".to_string(),
            thread_id: "telegram:434566766".to_string(),
            from_agent: "telegram:codefather_dev".to_string(),
            content: "a real question".to_string(),
            reply_to: None,
            created_at: 0,
        };
        let frame = crate::daemon::chat::build_channel_notification(&msg);
        let meta = meta_of(&frame);
        let from = meta.get("from_agent").and_then(|v| v.as_str()).unwrap_or("");
        assert!(from.starts_with("telegram:"), "the operator must stay inbound");
    }

    /// The control flag must be carried, and must be the ONLY thing that grants
    /// gate-bypassing delivery — a message body that merely says "continue"
    /// gets no such power.
    #[test]
    fn only_an_explicit_control_flag_grants_priority_delivery() {
        let frame = crate::daemon::chat::build_control_notification_continue("target-id", "продолжи");
        let meta = meta_of(&frame);
        assert_eq!(
            meta.get("control").and_then(|c| c.as_str()),
            Some("continue"),
            "the daemon must state it; the supervisor must not infer it"
        );

        // An ordinary Telegram message with the same words carries no flag.
        let tg = crate::daemon::chat::TelegramMessageMeta {
            chat_id: 1,
            message_id_str: "1".to_string(),
            user: "u".to_string(),
            user_id: "1".to_string(),
            ts_iso8601: "2026-08-18T00:00:00.000Z".to_string(),
            thread_id: None,
            is_voice: false,
        };
        let ordinary = crate::daemon::chat::build_channel_notification_telegram("продолжи", &tg, None);
        assert!(
            meta_of(&ordinary).get("control").is_none(),
            "typing the word must not buy priority delivery"
        );
    }

    /// The operator's `/continue` must read as the operator, not as a peer.
    ///
    /// It rides an `agent:<id>` thread to reach one specific session, so
    /// classifying by thread prefix labelled it `[agent-to-agent:unknown]` — the
    /// operator pressed a button and the session saw an anonymous peer. The
    /// frame states `source: "telegram"`; this asserts the classifier reads it.
    #[test]
    fn the_continue_control_frame_reads_as_the_operator() {
        let frame = crate::daemon::chat::build_control_notification_continue("target-id", "продолжи");
        let (thread, source) = classify(&meta_of(&frame), None);
        assert_eq!(thread, "agent:target-id", "still addressed to one session");
        assert_eq!(
            source,
            Source::Telegram,
            "the operator pressed the button; it must not render as a peer"
        );
    }

    /// Every frame that is meant for a session must NAME that session.
    ///
    /// Delivery is by recipient, not by subscription: a message without a
    /// recipient is not "for whoever is listening", it is a message nobody is
    /// entitled to receive, and the subscriber drops it. That rule only works if
    /// the builders hold up their end, so this asserts they do — a new builder
    /// that forgets the field would otherwise produce messages that silently go
    /// nowhere.
    #[test]
    fn every_addressed_builder_names_its_recipient() {
        use crate::daemon::chat;

        let frames = vec![
            (
                "callback",
                chat::build_channel_notification_callback("x", Some("ci"), "target-id", "m1"),
            ),
            (
                "continue control",
                chat::build_control_notification_continue("target-id", "продолжи"),
            ),
            (
                "agent-to-agent",
                chat::build_channel_notification_agent_to_agent("x", "from-id", "target-id", "m2", false),
            ),
        ];

        for (name, frame) in frames {
            let meta = meta_of(&frame);
            assert_eq!(
                meta.get("target_agent_id").and_then(|v| v.as_str()),
                Some("target-id"),
                "the {name} frame must name its recipient, or it is delivered to nobody"
            );
        }
    }

    #[test]
    fn a_real_peer_notification_is_agent() {
        let frame = build_channel_notification_agent_to_agent(
            "тест",
            "sender-agent-id",
            "target-agent-id",
            "msg-1",
            false,
        );
        let (thread, source) = classify(&meta_of(&frame), Some("sender-agent-id".to_string()));

        assert_eq!(thread, "agent:target-agent-id");
        assert!(matches!(source, Source::Agent { .. }), "peer traffic keeps its sender");
    }

    /// Built with the real builder, like the other two: a callback carries the
    /// same `agent:<id>` thread as peer traffic, so ONLY the explicit
    /// `meta.source` separates them. Infer it from the thread and every callback
    /// would arrive labelled as another agent — F-14 in a new costume.
    #[test]
    fn a_real_callback_notification_is_a_callback() {
        let frame = crate::daemon::chat::build_channel_notification_callback(
            "build failed",
            Some("ci"),
            "target-agent-id",
            "msg-9",
        );
        let (thread, source) = classify(&meta_of(&frame), None);
        assert_eq!(thread, "agent:target-agent-id");
        assert_eq!(source, Source::Callback { label: Some("ci".to_string()) });
    }

    #[test]
    fn a_callback_without_a_label_stays_unlabelled() {
        let frame = crate::daemon::chat::build_channel_notification_callback(
            "ping", None, "target-agent-id", "msg-10",
        );
        let (_, source) = classify(&meta_of(&frame), None);
        assert_eq!(source, Source::Callback { label: None });
    }

    #[test]
    fn an_explicit_thread_prefix_wins() {
        // `build_channel_notification` (posts and replies) carries meta.thread.
        let meta = serde_json::json!({ "thread": "telegram:99", "from_agent": "cli" });
        assert_eq!(classify(&meta, None).1, Source::Telegram);

        let meta = serde_json::json!({ "thread": "agent:abc", "from_agent": "peer" });
        assert!(matches!(classify(&meta, None).1, Source::Agent { .. }));
    }

    #[test]
    fn an_unrecognisable_frame_does_not_masquerade_as_telegram() {
        // Better to label it a peer message than to tell the model the operator
        // wrote it.
        let (thread, source) = classify(&serde_json::json!({}), None);
        assert_eq!(thread, "unknown");
        assert!(matches!(source, Source::Agent { .. }));
    }
}

#[cfg(test)]
mod nick_resolution_tests {
    use super::nick_for;
    use std::sync::Mutex;

    /// `$HOME` is process-global; these tests repoint it at a temp dir.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Reproduces the 2026-08-17 report: two peer messages seconds apart, the
    /// first labelled `[agent-to-agent:planner]`, the second
    /// `[agent-to-agent:d1eb9528]` — the first 8 characters of the sender's
    /// agent_id. The lookup did not find a different answer; it FAILED, and the
    /// failure was swallowed into a stub that looks like a nick but is not one.
    ///
    /// The trigger is a concurrent writer: `open_chat_db` runs
    /// `ensure_chat_db_schema` (a write transaction) on every open, so a plain
    /// name lookup contends with the daemon writing the very message being
    /// resolved.
    #[test]
    fn a_nick_resolves_while_another_process_holds_a_write_lock() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let agent_id = "d1eb9528-57e8-49bc-9bc8-3f1536ca2ad5";
        {
            let conn = crate::daemon::chat::open_chat_db().expect("open");
            conn.execute(
                "INSERT INTO agent_registry \
                   (agent_id, agent_name, connection_id, state, spawned_at, last_pinged_at) \
                 VALUES (?1, 'planner', 'conn-1', 'alive', 0, 0)",
                [agent_id],
            )
            .expect("seed registry");
        }

        // A second connection holding an exclusive write, exactly as the daemon
        // does while it stores an inbound message.
        let writer = crate::daemon::chat::open_chat_db().expect("open writer");
        writer
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO chat_threads (id, created_at) VALUES ('t', 0);")
            .expect("hold write lock");

        let resolved = nick_for(agent_id);

        writer.execute_batch("COMMIT;").expect("release");
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(
            resolved, "planner",
            "a busy database must not degrade a nick into an unaddressable id stub"
        );
    }

    /// The fallback itself was wrong independently of the race: `--agent_nick`
    /// resolves a FULL agent_id (agent_registry::resolve_target), but never an
    /// 8-character prefix. Truncating produced a label the recipient could not
    /// reply to — the same defect that made `codefath` undeliverable in F-14.
    #[test]
    fn an_unresolvable_sender_keeps_an_addressable_id() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        let unknown = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let resolved = nick_for(unknown);

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(
            resolved, unknown,
            "an unknown sender must stay addressable, not become an 8-char stub"
        );
    }
}
