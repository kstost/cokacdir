use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Mutex;
use teloxide::prelude::*;

use crate::services::claude::CancelToken;
use crate::ui::ai_screen::HistoryItem;

mod commands;
mod file_ops;
mod message;
mod storage;
mod streaming;
mod tools;

pub use storage::cleanup_stale_sessions;
pub use storage::resolve_token_by_hash;
pub use storage::token_hash;

/// Global debug log flag for Telegram API calls
pub(crate) static TG_DEBUG: AtomicBool = AtomicBool::new(false);

/// Log Telegram API call result to ~/.cokacdir/debug/ file
pub(crate) fn tg_debug<T, E: std::fmt::Display>(name: &str, result: &Result<T, E>) {
    if !TG_DEBUG.load(Ordering::Relaxed) {
        return;
    }
    let Some(debug_dir) = dirs::home_dir().map(|h| h.join(".cokacdir").join("debug")) else {
        return;
    };
    let _ = std::fs::create_dir_all(&debug_dir);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let log_path = debug_dir.join(format!("{}.log", date));
    let ts = chrono::Local::now().format("%H:%M:%S%.3f");
    let status = match result {
        Ok(_) => "✓".to_string(),
        Err(e) => format!("✗ {e}"),
    };
    let line = format!("[{ts}] {name}: {status}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

/// Wrap a Telegram API call to log its result in debug mode
macro_rules! tg {
    ($name:expr, $fut:expr) => {{
        let r = $fut;
        $crate::services::telegram::tg_debug($name, &r);
        r
    }};
}
pub(crate) use tg;

/// Per-chat session state
pub(crate) struct ChatSession {
    pub(crate) session_id: Option<String>,
    pub(crate) current_path: Option<String>,
    pub(crate) history: Vec<HistoryItem>,
    /// File upload records not yet sent to Claude AI.
    /// Drained and prepended to the next user prompt so Claude knows about uploaded files.
    pub(crate) pending_uploads: Vec<String>,
    /// Set to true by /clear to prevent a racing polling loop from re-populating history.
    pub(crate) cleared: bool,
}

/// Bot-level settings persisted to disk
#[derive(Clone)]
pub(crate) struct BotSettings {
    pub(crate) allowed_tools: HashMap<String, Vec<String>>,
    /// chat_id (string) → last working directory path
    pub(crate) last_sessions: HashMap<String, String>,
    /// Telegram user ID of the registered owner (imprinting auth)
    pub(crate) owner_user_id: Option<u64>,
    /// chat_id (string) → true if group chat is public (non-owner users allowed)
    pub(crate) as_public_for_group_chat: HashMap<String, bool>,
}

impl Default for BotSettings {
    fn default() -> Self {
        Self {
            allowed_tools: HashMap::new(),
            last_sessions: HashMap::new(),
            owner_user_id: None,
            as_public_for_group_chat: HashMap::new(),
        }
    }
}

/// Shared state: per-chat sessions + bot settings
pub(crate) struct SharedData {
    pub(crate) sessions: HashMap<ChatId, ChatSession>,
    pub(crate) settings: BotSettings,
    /// Per-chat cancel tokens for stopping in-progress AI requests
    pub(crate) cancel_tokens: HashMap<ChatId, Arc<CancelToken>>,
    /// Message ID of the "Stopping..." message sent by /stop, so the polling loop can update it
    pub(crate) stop_message_ids: HashMap<ChatId, teloxide::types::MessageId>,
    /// Per-chat timestamp of the last Telegram API call (for rate limiting)
    pub(crate) api_timestamps: HashMap<ChatId, tokio::time::Instant>,
    /// Telegram API polling interval in milliseconds (shared across all bots)
    pub(crate) polling_time_ms: u64,
}

pub(crate) type SharedState = Arc<Mutex<SharedData>>;

/// Telegram message length limit
pub(crate) const TELEGRAM_MSG_LIMIT: usize = 4096;

/// Entry point: start the Telegram bot with long polling
pub async fn run_bot(token: &str) {
    let bot = Bot::new(token);
    let bot_settings = storage::load_bot_settings(token);

    // Clean up session files older than 30 days on startup
    cleanup_stale_sessions(30);

    // Register bot commands for autocomplete
    let commands = vec![
        teloxide::types::BotCommand::new("help", "Show help"),
        teloxide::types::BotCommand::new("start", "Start session at directory"),
        teloxide::types::BotCommand::new("pwd", "Show current working directory"),
        teloxide::types::BotCommand::new("clear", "Clear AI conversation history"),
        teloxide::types::BotCommand::new("stop", "Stop current AI request"),
        teloxide::types::BotCommand::new("down", "Download file from server"),
        teloxide::types::BotCommand::new("public", "Toggle public access (group only)"),
        teloxide::types::BotCommand::new("availabletools", "List all available tools"),
        teloxide::types::BotCommand::new("allowedtools", "Show currently allowed tools"),
        teloxide::types::BotCommand::new("allowed", "Add/remove tool (+name / -name)"),
        teloxide::types::BotCommand::new("setpollingtime", "Set API polling interval (ms)"),
        teloxide::types::BotCommand::new("debug", "Toggle API debug logging"),
    ];
    if let Err(e) = tg!("set_my_commands", bot.set_my_commands(commands).await) {
        println!("  ⚠ Failed to set bot commands: {e}");
    }

    match bot_settings.owner_user_id {
        Some(owner_id) => println!("  ✓ Owner: {owner_id}"),
        None => println!("  ⚠ No owner registered — first user will be registered as owner"),
    }

    let app_settings = crate::config::Settings::load();
    let polling_time_ms = app_settings.telegram_polling_time.max(2500);

    let state: SharedState = Arc::new(Mutex::new(SharedData {
        sessions: HashMap::new(),
        settings: bot_settings,
        cancel_tokens: HashMap::new(),
        stop_message_ids: HashMap::new(),
        api_timestamps: HashMap::new(),
        polling_time_ms,
    }));

    println!("  ✓ Bot connected — Listening for messages");

    let shared_state = state.clone();
    let token_owned = token.to_string();
    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let state = shared_state.clone();
        let token = token_owned.clone();
        async move {
            message::handle_message(bot, msg, state, &token).await
        }
    })
    .await;
}
