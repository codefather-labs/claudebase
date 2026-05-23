//! telegram-plugin-rs — Rust port of the official Anthropic Telegram
//! channel plugin for Claude Code.
//!
//! Wire format: JSON-RPC 2.0 + MCP method conventions. Stdout is RESERVED
//! for protocol frames — NEVER print!() / println!() / eprintln to stdout.
//! All logging goes to stderr via `tracing`.

mod access;
mod mcp;
mod state;
mod telegram;
mod whisper;

use frankenstein::client_reqwest::Bot;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "telegram-plugin-rs starting"
    );

    state::load_env_file();

    let (notif_tx, notif_rx) = mpsc::unbounded_channel();
    let pending_permissions = mcp::permission::PendingPermissions::new();

    // Construct Bot once if token is set; clone for polling task + server
    // tool dispatcher. Frankenstein's Bot is cheap to clone (Arc-internal).
    let bot: Option<Bot> = match state::bot_token() {
        Ok(token) => {
            let bot = Bot::new(&token);
            let bot_for_polling = bot.clone();
            let tx = notif_tx.clone();
            let pending_for_polling = pending_permissions.clone();
            tokio::spawn(async move {
                telegram::bot::run(bot_for_polling, tx, pending_for_polling).await;
                tracing::warn!("TG polling loop exited");
            });
            Some(bot)
        }
        Err(_) => {
            tracing::warn!(
                "TELEGRAM_BOT_TOKEN not set — TG polling disabled. \
                 Run /telegram:configure <token> in Claude Code to set it."
            );
            None
        }
    };

    drop(notif_tx);

    let result = mcp::server::run(notif_rx, bot, pending_permissions).await;
    tracing::info!("MCP server loop exited");
    result
}
