//! Journal reminders: an entry written with `journal add --every <dur>` comes
//! back into the input of the session that wrote it, as `[reminder #N]: ...`,
//! every interval until that entry's `reminder` flag is closed.
//!
//! Operator decisions journal#72 / journal#73: the reminder goes to its author
//! and to no other session; it is addressed by nick, which belongs to the
//! conversation and survives a resume (the agent id is new on every
//! `claudebase run`); nothing is sent while the author is offline or in DND,
//! and a reminder that came due meanwhile is delivered on the first tick after
//! it is back. Unlike a cron of the session, it outlives the session.

use std::path::Path;

use rusqlite::{params, OptionalExtension};

use super::chat;
use super::chat::SharedBus;
use crate::journal;

/// How often due reminders are looked for — also the longest a returning
/// session waits for one that came due while it was away.
const TICK_SECS: u64 = 30;

/// `CLAUDEBASE_REMINDER_TICK_SECS` shortens the tick for the end-to-end test,
/// which otherwise waits half a minute per step.
fn tick_interval() -> std::time::Duration {
    let secs = std::env::var("CLAUDEBASE_REMINDER_TICK_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(TICK_SECS);
    std::time::Duration::from_secs(secs)
}

pub fn spawn_reminder_task(bus: SharedBus) {
    let every = tick_interval();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let bus_for_tick = bus.clone();
            // A panic in one tick must not end the task (same guard as the DND drain).
            if let Err(e) = tokio::spawn(async move { tick(&bus_for_tick).await }).await {
                tracing::error!(error = %e, "reminder tick failed — next tick continues");
            }
        }
    });
}

/// One due reminder, with the live session it goes to.
struct Delivery {
    project: String,
    reminder: journal::DueReminder,
    agent_id: String,
}

async fn tick(bus: &SharedBus) {
    let now = journal::now_secs();
    let found = tokio::task::spawn_blocking(move || collect(now)).await;
    let deliveries = match found {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "reminder tick: could not read the journals");
            return;
        }
        Err(e) => {
            tracing::error!(error = %e, "reminder tick: join error");
            return;
        }
    };
    for d in deliveries {
        deliver(bus, d, now).await;
    }
}

/// Every due reminder whose author is online and not in DND. The others stay
/// due and are looked at again next tick.
fn collect(now: i64) -> anyhow::Result<Vec<Delivery>> {
    let dir = journal::journals_dir();
    let registry = chat::open_chat_db()?;
    let mut out = Vec::new();
    for info in journal::list_journals(&dir)? {
        let Some(conn) = journal::open(&dir, Path::new(&info.project), false)? else { continue };
        for reminder in journal::due_reminders(&conn, now)? {
            if let Some(agent_id) = online_author(&registry, &reminder, now)? {
                out.push(Delivery { project: info.project.clone(), reminder, agent_id });
            }
        }
    }
    Ok(out)
}

fn online_author(
    registry: &rusqlite::Connection,
    r: &journal::DueReminder,
    now_secs: i64,
) -> anyhow::Result<Option<String>> {
    Ok(registry
        .query_row(
            "SELECT agent_id FROM agent_registry \
             WHERE agent_name = ?1 AND state = 'alive' AND (?2 IS NULL OR host = ?2) \
               AND (dnd_until_ts IS NULL OR dnd_until_ts <= ?3) \
             ORDER BY last_pinged_at DESC LIMIT 1",
            params![r.nick, r.host, now_secs * 1000],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn render(r: &journal::DueReminder) -> String {
    format!(
        "{}\n\n(journal#{id}, every {} min until you close it: claudebase journal close {id} --resolution \"<how>\")",
        r.body,
        r.every_secs / 60,
        id = r.id
    )
}

async fn deliver(bus: &SharedBus, d: Delivery, now: i64) {
    let content = render(&d.reminder);
    let (agent_for_send, text) = (d.agent_id.clone(), content.clone());
    let from = format!("reminder-{}", d.reminder.id);
    let stored = tokio::task::spawn_blocking(move || {
        chat::with_writer(|conn| {
            super::agent_registry::send_message(conn, &from, &agent_for_send, &text, chat::now_millis())
                .map(|o| o.message_id)
        })
    })
    .await;
    let message_id = match stored {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => {
            tracing::warn!(entry = d.reminder.id, error = %e, "reminder not stored — retried next tick");
            return;
        }
        Err(e) => {
            tracing::error!(error = %e, "reminder store join error");
            return;
        }
    };
    let thread = format!("agent:{}", d.agent_id);
    let frame = chat::build_channel_notification_reminder(&content, d.reminder.id, &d.agent_id, &message_id);
    let subscribers = bus.publish(&thread, frame).await;
    // The journal, not chat.db, decides whether a reminder is sent again. A
    // row left undelivered would be replayed by the subscribe backlog as
    // `[agent-to-agent:peer]` next to the next tick's own delivery.
    let mid = message_id.clone();
    let _ = tokio::task::spawn_blocking(move || {
        chat::with_writer(|conn| super::agent_registry::mark_delivered(conn, &mid, chat::now_millis()))
    })
    .await;
    if subscribers == 0 {
        // Registered but not listening (a bridge between reconnects): try again.
        tracing::info!(entry = d.reminder.id, nick = %d.reminder.nick, "reminder: no subscriber — retried next tick");
        return;
    }
    let (project, id) = (d.project.clone(), d.reminder.id);
    let marked = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let dir = journal::journals_dir();
        if let Some(conn) = journal::open(&dir, Path::new(&project), false)? {
            journal::reminder_sent(&conn, id, now)?;
        }
        Ok(())
    })
    .await;
    if let Ok(Err(e)) = marked {
        tracing::error!(entry = id, error = %e, "reminder delivered but not rescheduled");
    }
    tracing::info!(entry = id, nick = %d.reminder.nick, %message_id, "reminder delivered");
}
