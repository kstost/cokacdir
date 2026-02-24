use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::Path;
use std::fs;

use tokio::sync::Mutex;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use sha2::{Sha256, Digest};

use crate::services::audit;
use crate::services::claude::{self, CancelToken, StreamMessage, DEFAULT_ALLOWED_TOOLS};
use crate::ui::ai_screen::{self, HistoryItem, HistoryType, SessionData};

/// Global debug log flag for Telegram API calls
static TG_DEBUG: AtomicBool = AtomicBool::new(false);

/// Log Telegram API call result to ~/.cokacdir/debug/ file
fn tg_debug<T, E: std::fmt::Display>(name: &str, result: &Result<T, E>) {
    if !TG_DEBUG.load(Ordering::Relaxed) {
        return;
    }
    let Some(debug_dir) = dirs::home_dir().map(|h| h.join(".cokacdir").join("debug")) else {
        return;
    };
    let _ = fs::create_dir_all(&debug_dir);
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
        tg_debug($name, &r);
        r
    }};
}

/// Per-chat session state
struct ChatSession {
    session_id: Option<String>,
    current_path: Option<String>,
    history: Vec<HistoryItem>,
    /// File upload records not yet sent to Claude AI.
    /// Drained and prepended to the next user prompt so Claude knows about uploaded files.
    pending_uploads: Vec<String>,
    /// Set to true by /clear to prevent a racing polling loop from re-populating history.
    cleared: bool,
}

/// Bot-level settings persisted to disk
#[derive(Clone)]
struct BotSettings {
    allowed_tools: HashMap<String, Vec<String>>,
    /// chat_id (string) → last working directory path
    last_sessions: HashMap<String, String>,
    /// Telegram user ID of the registered owner (imprinting auth)
    owner_user_id: Option<u64>,
    /// chat_id (string) → true if group chat is public (non-owner users allowed)
    as_public_for_group_chat: HashMap<String, bool>,
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

/// Get allowed tools for a specific chat_id.
/// Returns the chat-specific list if configured, otherwise DEFAULT_ALLOWED_TOOLS.
fn get_allowed_tools(settings: &BotSettings, chat_id: ChatId) -> Vec<String> {
    let key = chat_id.0.to_string();
    settings.allowed_tools.get(&key)
        .cloned()
        .unwrap_or_else(|| DEFAULT_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect())
}

/// Shared state: per-chat sessions + bot settings
struct SharedData {
    sessions: HashMap<ChatId, ChatSession>,
    settings: BotSettings,
    /// Per-chat cancel tokens for stopping in-progress AI requests
    cancel_tokens: HashMap<ChatId, Arc<CancelToken>>,
    /// Message ID of the "Stopping..." message sent by /stop, so the polling loop can update it
    stop_message_ids: HashMap<ChatId, teloxide::types::MessageId>,
    /// Per-chat timestamp of the last Telegram API call (for rate limiting)
    api_timestamps: HashMap<ChatId, tokio::time::Instant>,
    /// Telegram API polling interval in milliseconds (shared across all bots)
    polling_time_ms: u64,
}

type SharedState = Arc<Mutex<SharedData>>;

/// Telegram message length limit
const TELEGRAM_MSG_LIMIT: usize = 4096;

/// Compute a short hash key from the bot token (first 16 chars of SHA-256 hex)
pub fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..8]) // 16 hex chars
}

/// Path to bot settings file: ~/.cokacdir/bot_settings.json
fn bot_settings_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".cokacdir").join("bot_settings.json"))
}

/// Load bot settings from bot_settings.json
fn load_bot_settings(token: &str) -> BotSettings {
    let Some(path) = bot_settings_path() else {
        return BotSettings::default();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return BotSettings::default();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return BotSettings::default();
    };
    let key = token_hash(token);
    let Some(entry) = json.get(&key) else {
        return BotSettings::default();
    };
    let owner_user_id = entry.get("owner_user_id").and_then(|v| v.as_u64());
    let last_sessions: HashMap<String, String> = entry.get("last_sessions")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let allowed_tools = match entry.get("allowed_tools") {
        Some(serde_json::Value::Array(arr)) => {
            // Legacy migration: array → per-chat HashMap
            let tool_list: Vec<String> = arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if tool_list.is_empty() {
                HashMap::new()
            } else {
                let mut map = HashMap::new();
                for chat_id_str in last_sessions.keys() {
                    map.insert(chat_id_str.clone(), tool_list.clone());
                }
                map
            }
        }
        Some(serde_json::Value::Object(obj)) => {
            // New format: object with chat_id keys
            obj.iter()
                .filter_map(|(k, v)| {
                    v.as_array().map(|arr| {
                        let tools: Vec<String> = arr.iter()
                            .filter_map(|t| t.as_str().map(String::from))
                            .collect();
                        (k.clone(), tools)
                    })
                })
                .collect()
        }
        _ => HashMap::new(),
    };

    let as_public_for_group_chat: HashMap<String, bool> = entry.get("as_public_for_group_chat")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                .collect()
        })
        .unwrap_or_default();

    BotSettings { allowed_tools, last_sessions, owner_user_id, as_public_for_group_chat }
}

/// Save bot settings to bot_settings.json
fn save_bot_settings(token: &str, settings: &BotSettings) {
    let Some(path) = bot_settings_path() else { return };
    // Ensure directory exists
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Load existing JSON or start fresh
    let mut json: serde_json::Value = if let Ok(content) = fs::read_to_string(&path) {
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    let key = token_hash(token);
    let mut entry = serde_json::json!({
        "token": token,
        "allowed_tools": settings.allowed_tools,
        "last_sessions": settings.last_sessions,
        "as_public_for_group_chat": settings.as_public_for_group_chat,
    });
    if let Some(owner_id) = settings.owner_user_id {
        entry["owner_user_id"] = serde_json::json!(owner_id);
    }
    json[key] = entry;
    if let Ok(s) = serde_json::to_string_pretty(&json) {
        let _ = fs::write(&path, s);
    }
}

/// Resolve a bot token from its hash by searching bot_settings.json
pub fn resolve_token_by_hash(hash: &str) -> Option<String> {
    let path = bot_settings_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let obj = json.as_object()?;
    let entry = obj.get(hash)?;
    entry.get("token").and_then(|v| v.as_str()).map(String::from)
}

/// Normalize tool name: first letter uppercase, rest lowercase
fn normalize_tool_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// All available tools with (description, is_destructive)
const ALL_TOOLS: &[(&str, &str, bool)] = &[
    ("Bash",            "Execute shell commands",                          true),
    ("Read",            "Read file contents from the filesystem",          false),
    ("Edit",            "Perform find-and-replace edits in files",         true),
    ("Write",           "Create or overwrite files",                       true),
    ("Glob",            "Find files by name pattern",                      false),
    ("Grep",            "Search file contents with regex",                 false),
    ("Task",            "Launch autonomous sub-agents for complex tasks",  true),
    ("TaskOutput",      "Retrieve output from background tasks",           false),
    ("TaskStop",        "Stop a running background task",                  false),
    ("WebFetch",        "Fetch and process web page content",              true),
    ("WebSearch",       "Search the web for up-to-date information",       true),
    ("NotebookEdit",    "Edit Jupyter notebook cells",                     true),
    ("Skill",           "Invoke slash-command skills",                     false),
    ("TaskCreate",      "Create a structured task in the task list",       false),
    ("TaskGet",         "Retrieve task details by ID",                     false),
    ("TaskUpdate",      "Update task status or details",                   false),
    ("TaskList",        "List all tasks and their status",                 false),
    ("AskUserQuestion", "Ask the user a question (interactive)",           false),
    ("EnterPlanMode",   "Enter planning mode (interactive)",               false),
    ("ExitPlanMode",    "Exit planning mode (interactive)",                false),
];

/// Tool info: (description, is_destructive)
fn tool_info(name: &str) -> (&'static str, bool) {
    ALL_TOOLS.iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, desc, destr)| (*desc, *destr))
        .unwrap_or(("Custom tool", false))
}

/// Format a risk badge for display
fn risk_badge(destructive: bool) -> &'static str {
    if destructive { "!!!" } else { "" }
}

/// Entry point: start the Telegram bot with long polling
pub async fn run_bot(token: &str) {
    let bot = Bot::new(token);
    let bot_settings = load_bot_settings(token);

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
            handle_message(bot, msg, state, &token).await
        }
    })
    .await;
}

/// Route incoming messages to appropriate handlers
async fn handle_message(
    bot: Bot,
    msg: Message,
    state: SharedState,
    token: &str,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    let raw_user_name = msg.from.as_ref()
        .map(|u| u.first_name.as_str())
        .unwrap_or("unknown");
    let timestamp = chrono::Local::now().format("%H:%M:%S");
    let user_id = msg.from.as_ref().map(|u| u.id.0);

    // Auth check (imprinting)
    let Some(uid) = user_id else {
        // No user info (e.g. channel post) → reject
        return Ok(());
    };
    let is_group_chat = matches!(msg.chat.kind, teloxide::types::ChatKind::Public(_));
    let imprinted = {
        let mut data = state.lock().await;
        match data.settings.owner_user_id {
            None => {
                // Imprint: register first user as owner
                data.settings.owner_user_id = Some(uid);
                save_bot_settings(token, &data.settings);
                println!("  [{timestamp}] ★ Owner registered: {raw_user_name} (id:{uid})");
                true
            }
            Some(owner_id) => {
                if uid != owner_id {
                    // Check if this is a public group chat
                    let chat_key = chat_id.0.to_string();
                    let is_public = is_group_chat
                        && data.settings.as_public_for_group_chat.get(&chat_key).copied().unwrap_or(false);
                    if !is_public {
                        // Unregistered user → reject silently (log only)
                        println!("  [{timestamp}] ✗ Rejected: {raw_user_name} (id:{uid})");
                        return Ok(());
                    }
                    // Public group chat: allow non-owner user
                    println!("  [{timestamp}] ○ [{raw_user_name}(id:{uid})] Public group access");
                }
                false
            }
        }
    };
    if imprinted {
        // Owner registration is logged to server console only
        // No response sent to the user
    }

    let is_owner = {
        let data = state.lock().await;
        data.settings.owner_user_id == Some(uid)
    };

    let user_name = format!("{}({uid})", raw_user_name);

    // Handle file/photo uploads
    if msg.document().is_some() || msg.photo().is_some() {
        // In group chats, only process uploads whose caption starts with ';'
        if is_group_chat {
            let caption = msg.caption().unwrap_or("");
            if !caption.starts_with(';') {
                return Ok(());
            }
        }
        let file_hint = if msg.document().is_some() { "document" } else { "photo" };
        println!("  [{timestamp}] ◀ [{user_name}] Upload: {file_hint}");
        handle_file_upload(&bot, chat_id, &msg, &state).await?;
        println!("  [{timestamp}] ▶ [{user_name}] Upload complete");
        // If caption contains text after ';', send it to AI as a follow-up message
        if let Some(caption) = msg.caption() {
            let text_part = if is_group_chat {
                // Group chat: extract text after ';'
                caption.find(';').map(|pos| caption[pos + 1..].trim())
            } else {
                // DM: use entire caption as-is
                let trimmed = caption.trim();
                if trimmed.is_empty() { None } else { Some(trimmed) }
            };
            if let Some(text) = text_part {
                if !text.is_empty() {
                    // Block if an AI request is already in progress
                    let ai_busy = {
                        let data = state.lock().await;
                        data.cancel_tokens.contains_key(&chat_id)
                    };
                    if ai_busy {
                        shared_rate_limit_wait(&state, chat_id).await;
                        tg!("send_message", bot.send_message(chat_id, "AI request in progress. Use /stop to cancel.")
                            .await)?;
                    } else {
                        handle_text_message(&bot, chat_id, text, &state).await?;
                    }
                }
            }
        }
        return Ok(());
    }

    let Some(raw_text) = msg.text() else {
        return Ok(());
    };

    // Strip @botname suffix from commands (e.g. "/pwd@mybot" → "/pwd")
    let text = if raw_text.starts_with('/') {
        if let Some(space_pos) = raw_text.find(' ') {
            // "/cmd@bot args" → "/cmd args"
            let cmd_part = &raw_text[..space_pos];
            let args_part = &raw_text[space_pos..];
            if let Some(at_pos) = cmd_part.find('@') {
                format!("{}{}", &cmd_part[..at_pos], args_part)
            } else {
                raw_text.to_string()
            }
        } else {
            // "/cmd@bot" (no args) → "/cmd"
            if let Some(at_pos) = raw_text.find('@') {
                raw_text[..at_pos].to_string()
            } else {
                raw_text.to_string()
            }
        }
    } else {
        raw_text.to_string()
    };
    let preview = truncate_str(&text, 60);

    // Auto-restore session from bot_settings.json if not in memory
    if !text.starts_with("/start") {
        let mut data = state.lock().await;
        if !data.sessions.contains_key(&chat_id) {
            if let Some(last_path) = data.settings.last_sessions.get(&chat_id.0.to_string()).cloned() {
                if Path::new(&last_path).is_dir() {
                    let existing = load_existing_session(&last_path);
                    let session = data.sessions.entry(chat_id).or_insert_with(|| ChatSession {
                        session_id: None,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                    });
                    session.current_path = Some(last_path.clone());
                    if let Some((session_data, _)) = existing {
                        session.session_id = Some(session_data.session_id.clone());
                        session.history = session_data.history.clone();
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ↻ [{user_name}] Auto-restored session: {last_path}");
                }
            }
        }
    }

    // In group chats, ignore plain text (only /, !, ; prefixed messages are processed)
    if is_group_chat && !text.starts_with('/') && !text.starts_with('!') && !text.starts_with(';') {
        return Ok(());
    }

    // Block all messages except /stop while an AI request is in progress
    if !text.starts_with("/stop") {
        let data = state.lock().await;
        if data.cancel_tokens.contains_key(&chat_id) {
            drop(data);
            shared_rate_limit_wait(&state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "AI request in progress. Use /stop to cancel.")
                .await)?;
            return Ok(());
        }
    }

    // Audit log: record every command with risk classification
    {
        let risk_str = if text.starts_with("!") {
            "Dangerous"
        } else if text.starts_with("/allowed") || text.starts_with("/public") {
            "Dangerous"
        } else if text.starts_with("/start") || text.starts_with("/cd") || text.starts_with("/down") {
            "Elevated"
        } else {
            "Safe"
        };
        audit::log_command(uid, chat_id.0, risk_str, &text);
    }


    if text.starts_with("/stop") {
        println!("  [{timestamp}] ◀ [{user_name}] /stop");
        handle_stop_command(&bot, chat_id, &state).await?;
    } else if text.starts_with("/help") {
        println!("  [{timestamp}] ◀ [{user_name}] /help");
        handle_help_command(&bot, chat_id, &state).await?;
    } else if text.starts_with("/start") {
        println!("  [{timestamp}] ◀ [{user_name}] /start");
        handle_start_command(&bot, chat_id, &text, &state, token).await?;
    } else if text.starts_with("/clear") {
        println!("  [{timestamp}] ◀ [{user_name}] /clear");
        handle_clear_command(&bot, chat_id, &state).await?;
        println!("  [{timestamp}] ▶ [{user_name}] Session cleared");
    } else if text.starts_with("/pwd") {
        println!("  [{timestamp}] ◀ [{user_name}] /pwd");
        handle_pwd_command(&bot, chat_id, &state).await?;
    } else if text.starts_with("/down") {
        println!("  [{timestamp}] ◀ [{user_name}] /down {}", text.strip_prefix("/down").unwrap_or("").trim());
        handle_down_command(&bot, chat_id, &text, &state).await?;
    } else if text.starts_with("/public") {
        println!("  [{timestamp}] ◀ [{user_name}] /public {}", text.strip_prefix("/public").unwrap_or("").trim());
        handle_public_command(&bot, chat_id, &text, &state, token, is_group_chat, is_owner).await?;
    } else if text.starts_with("/availabletools") {
        println!("  [{timestamp}] ◀ [{user_name}] /availabletools");
        handle_availabletools_command(&bot, chat_id, &state).await?;
    } else if text.starts_with("/allowedtools") {
        println!("  [{timestamp}] ◀ [{user_name}] /allowedtools");
        handle_allowedtools_command(&bot, chat_id, &state).await?;
    } else if text.starts_with("/setpollingtime") {
        println!("  [{timestamp}] ◀ [{user_name}] /setpollingtime {}", text.strip_prefix("/setpollingtime").unwrap_or("").trim());
        handle_setpollingtime_command(&bot, chat_id, &text, &state).await?;
    } else if text.starts_with("/debug") {
        println!("  [{timestamp}] ◀ [{user_name}] /debug");
        handle_debug_command(&bot, chat_id, &state).await?;
    } else if text.starts_with("/allowed") {
        println!("  [{timestamp}] ◀ [{user_name}] /allowed {}", text.strip_prefix("/allowed").unwrap_or("").trim());
        handle_allowed_command(&bot, chat_id, &text, &state, token).await?;
    } else if text.starts_with('!') {
        println!("  [{timestamp}] ◀ [{user_name}] Shell: {preview}");
        handle_shell_command(&bot, chat_id, &text, &state).await?;
    } else if text.starts_with(';') {
        let stripped = text.strip_prefix(';').unwrap_or(&text).trim().to_string();
        if stripped.is_empty() {
            return Ok(());
        }
        let preview = truncate_str(&stripped, 60);
        println!("  [{timestamp}] ◀ [{user_name}] {preview}");
        handle_text_message(&bot, chat_id, &stripped, &state).await?;
    } else {
        println!("  [{timestamp}] ◀ [{user_name}] {preview}");
        handle_text_message(&bot, chat_id, &text, &state).await?;
    }

    Ok(())
}

/// Handle /help command
async fn handle_help_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let help = "\
<b>cokacdir Telegram Bot</b>
Manage server files &amp; chat with Claude AI.

<b>Session</b>
<code>/start &lt;path&gt;</code> — Start session at directory
<code>/start</code> — Start with auto-generated workspace
<code>/pwd</code> — Show current working directory
<code>/clear</code> — Clear AI conversation history
<code>/stop</code> — Stop current AI request

<b>File Transfer</b>
<code>/down &lt;file&gt;</code> — Download file from server
Send a file/photo — Upload to session directory

<b>Shell</b>
<code>!&lt;command&gt;</code> — Run shell command directly
  e.g. <code>!ls -la</code>, <code>!git status</code>

<b>AI Chat</b>
Any other message is sent to Claude AI.
AI can read, edit, and run commands in your session.

<b>Tool Management</b>
<code>/availabletools</code> — List all available tools
<code>/allowedtools</code> — Show currently allowed tools
<code>/allowed +name</code> — Add tool (e.g. <code>/allowed +Bash</code>)
<code>/allowed -name</code> — Remove tool
<code>/allowed +a -b +c</code> — Multiple at once

<b>Group Chat</b>
<code>;</code><i>message</i> — Send message to AI
<code>;</code><i>caption</i> — Upload file with AI prompt
<code>/public on</code> — Allow all members to use bot
<code>/public off</code> — Owner only (default)

<b>Settings</b>
<code>/setpollingtime &lt;ms&gt;</code> — Set API polling interval
  Too low may cause Telegram API rate limits.
  Minimum 2500ms, recommended 3000ms+.
<code>/debug</code> — Toggle API debug logging

<code>/help</code> — Show this help";

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, help)
        .parse_mode(ParseMode::Html)
        .await)?;

    Ok(())
}

/// Handle /start <path> command
async fn handle_start_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    // Extract path from "/start <path>"
    let path_str = text.strip_prefix("/start").unwrap_or("").trim();

    let canonical_path = if path_str.is_empty() {
        // Create random workspace directory
        let Some(home) = dirs::home_dir() else {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "Error: cannot determine home directory.")
                .await)?;
            return Ok(());
        };
        let workspace_dir = home.join(".cokacdir").join("workspace");
        use rand::Rng;
        let random_name: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(8)
            .map(|b| (b as char).to_ascii_lowercase())
            .collect();
        let new_dir = workspace_dir.join(&random_name);
        if let Err(e) = fs::create_dir_all(&new_dir) {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, format!("Error: failed to create workspace: {}", e))
                .await)?;
            return Ok(());
        }
        new_dir.display().to_string()
    } else {
        // Expand ~ to home directory
        let expanded = if path_str.starts_with("~/") || path_str == "~" {
            if let Some(home) = dirs::home_dir() {
                home.join(path_str.strip_prefix("~/").unwrap_or("")).display().to_string()
            } else {
                path_str.to_string()
            }
        } else {
            path_str.to_string()
        };
        // Validate path exists
        let path = Path::new(&expanded);
        if !path.exists() || !path.is_dir() {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, format!("Error: '{}' is not a valid directory.", expanded))
                .await)?;
            return Ok(());
        }
        path.canonicalize()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| expanded)
    };

    // Try to load existing session for this path
    let existing = load_existing_session(&canonical_path);

    let mut response_lines = Vec::new();

    {
        let mut data = state.lock().await;
        let session = data.sessions.entry(chat_id).or_insert_with(|| ChatSession {
            session_id: None,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
        });

        if let Some((session_data, _)) = &existing {
            session.session_id = Some(session_data.session_id.clone());
            session.current_path = Some(canonical_path.clone());
            session.history = session_data.history.clone();

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Session restored: {canonical_path}");
            response_lines.push(format!("Session restored at `{}`.", canonical_path));
            response_lines.push(String::new());

            // Show last 5 conversation items
            let history_len = session_data.history.len();
            let start_idx = if history_len > 5 { history_len - 5 } else { 0 };
            for item in &session_data.history[start_idx..] {
                let prefix = match item.item_type {
                    HistoryType::User => "You",
                    HistoryType::Assistant => "AI",
                    HistoryType::Error => "Error",
                    HistoryType::System => "System",
                    HistoryType::ToolUse => "Tool",
                    HistoryType::ToolResult => "Result",
                };
                // Truncate long items for display
                let content: String = item.content.chars().take(200).collect();
                let truncated = if item.content.chars().count() > 200 { "..." } else { "" };
                response_lines.push(format!("[{}] {}{}", prefix, content, truncated));
            }
        } else {
            session.session_id = None;
            session.current_path = Some(canonical_path.clone());
            session.history.clear();

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Session started: {canonical_path}");
            response_lines.push(format!("Session started at `{}`.", canonical_path));
        }
    }

    // Persist chat_id → path mapping for auto-restore after restart
    {
        let mut data = state.lock().await;
        data.settings.last_sessions.insert(chat_id.0.to_string(), canonical_path);
        save_bot_settings(token, &data.settings);
    }

    let response_text = response_lines.join("\n");
    send_long_message(bot, chat_id, &response_text, None, state).await?;

    Ok(())
}

/// Handle /clear command
async fn handle_clear_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    // Cancel in-progress AI request if any
    let cancel_token = {
        let data = state.lock().await;
        data.cancel_tokens.get(&chat_id).cloned()
    };
    if let Some(token) = cancel_token {
        token.cancelled.store(true, Ordering::Relaxed);
        if let Ok(guard) = token.child_pid.lock() {
            if let Some(pid) = *guard {
                #[cfg(unix)]
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
            }
        }
    }

    {
        let mut data = state.lock().await;
        if let Some(session) = data.sessions.get_mut(&chat_id) {
            session.session_id = None;
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
        data.cancel_tokens.remove(&chat_id);
        data.stop_message_ids.remove(&chat_id);
    }

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, "Session cleared.")
        .await)?;

    Ok(())
}

/// Handle /pwd command - show current session path
async fn handle_pwd_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let current_path = {
        let data = state.lock().await;
        data.sessions.get(&chat_id).and_then(|s| s.current_path.clone())
    };

    shared_rate_limit_wait(state, chat_id).await;
    match current_path {
        Some(path) => tg!("send_message", bot.send_message(chat_id, &path).await)?,
        None => tg!("send_message", bot.send_message(chat_id, "No active session. Use /start <path> first.").await)?,
    };

    Ok(())
}

/// Handle /stop command - cancel in-progress AI request
async fn handle_stop_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let token = {
        let data = state.lock().await;
        data.cancel_tokens.get(&chat_id).cloned()
    };

    match token {
        Some(token) => {
            // Ignore duplicate /stop if already cancelled
            if token.cancelled.load(Ordering::Relaxed) {
                return Ok(());
            }

            // Send immediate feedback to user
            shared_rate_limit_wait(state, chat_id).await;
            let stop_msg = tg!("send_message", bot.send_message(chat_id, "Stopping...").await)?;

            // Store the stop message ID so the polling loop can update it later
            {
                let mut data = state.lock().await;
                data.stop_message_ids.insert(chat_id, stop_msg.id);
            }

            // Set cancellation flag
            token.cancelled.store(true, Ordering::Relaxed);

            // Kill child process directly to unblock reader.lines()
            // When the child dies, its stdout pipe closes → reader returns EOF → blocking thread exits
            if let Ok(guard) = token.child_pid.lock() {
                if let Some(pid) = *guard {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                }
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Cancel signal sent");
        }
        None => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "No active request to stop.")
                .await)?;
        }
    }

    Ok(())
}

/// Handle /down <filepath> - send file to user
async fn handle_down_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
) -> ResponseResult<()> {
    let file_path = text.strip_prefix("/down").unwrap_or("").trim();

    if file_path.is_empty() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Usage: /down <filepath>\nExample: /down /home/kst/file.txt")
            .await)?;
        return Ok(());
    }

    // Resolve relative path using current session path
    let resolved_path = if Path::new(file_path).is_absolute() {
        file_path.to_string()
    } else {
        let current_path = {
            let data = state.lock().await;
            data.sessions.get(&chat_id).and_then(|s| s.current_path.clone())
        };
        match current_path {
            Some(base) => format!("{}/{}", base.trim_end_matches('/'), file_path),
            None => {
                shared_rate_limit_wait(state, chat_id).await;
                tg!("send_message", bot.send_message(chat_id, "No active session. Use absolute path or /start <path> first.")
                    .await)?;
                return Ok(());
            }
        }
    };

    let path = Path::new(&resolved_path);
    if !path.exists() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, &format!("File not found: {}", resolved_path)).await)?;
        return Ok(());
    }
    if !path.is_file() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, &format!("Not a file: {}", resolved_path)).await)?;
        return Ok(());
    }

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_document", bot.send_document(chat_id, teloxide::types::InputFile::file(path))
        .await)?;

    Ok(())
}

/// Handle file/photo upload - save to current session path
async fn handle_file_upload(
    bot: &Bot,
    chat_id: ChatId,
    msg: &Message,
    state: &SharedState,
) -> ResponseResult<()> {
    // Get current session path
    let current_path = {
        let data = state.lock().await;
        data.sessions.get(&chat_id).and_then(|s| s.current_path.clone())
    };

    let Some(save_dir) = current_path else {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "No active session. Use /start <path> first.")
            .await)?;
        return Ok(());
    };

    // Get file_id and file_name
    let (file_id, file_name) = if let Some(doc) = msg.document() {
        let name = doc.file_name.clone().unwrap_or_else(|| "uploaded_file".to_string());
        (doc.file.id.clone(), name)
    } else if let Some(photos) = msg.photo() {
        // Get the largest photo
        if let Some(photo) = photos.last() {
            let name = format!("photo_{}.jpg", photo.file.unique_id);
            (photo.file.id.clone(), name)
        } else {
            return Ok(());
        }
    } else {
        return Ok(());
    };

    // Download file from Telegram via HTTP
    shared_rate_limit_wait(state, chat_id).await;
    let file = tg!("get_file", bot.get_file(&file_id).await)?;
    let url = format!("https://api.telegram.org/file/bot{}/{}", bot.token(), file.path);
    let buf = match reqwest::get(&url).await {
        Ok(resp) => match resp.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                shared_rate_limit_wait(state, chat_id).await;
                tg!("send_message", bot.send_message(chat_id, &format!("Download failed: {}", e)).await)?;
                return Ok(());
            }
        },
        Err(e) => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, &format!("Download failed: {}", e)).await)?;
            return Ok(());
        }
    };

    // Save to session path (sanitize file_name to prevent path traversal)
    let safe_name = Path::new(&file_name)
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("uploaded_file"));
    let dest = Path::new(&save_dir).join(safe_name);
    let file_size = buf.len();
    match fs::write(&dest, &buf) {
        Ok(_) => {
            let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, &msg_text).await)?;
        }
        Err(e) => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, &format!("Failed to save file: {}", e)).await)?;
            return Ok(());
        }
    }

    // Record upload in session history and pending queue for Claude
    let upload_record = format!(
        "[File uploaded] {} → {} ({} bytes)",
        file_name, dest.display(), file_size
    );
    {
        let mut data = state.lock().await;
        if let Some(session) = data.sessions.get_mut(&chat_id) {
            session.history.push(HistoryItem {
                item_type: HistoryType::User,
                content: upload_record.clone(),
            });
            session.pending_uploads.push(upload_record);
            save_session_to_file(session, &save_dir);
        }
    }

    Ok(())
}

/// Shell command output message type
enum ShellOutput {
    Line(String),
    Done { exit_code: i32 },
    Error(String),
}

/// Handle !command - execute shell command directly with lock/stop/streaming support
async fn handle_shell_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
) -> ResponseResult<()> {
    let cmd_str = text.strip_prefix('!').unwrap_or("").trim();

    if cmd_str.is_empty() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Usage: !<command>\nExample: !mkdir /home/kst/testcode")
            .await)?;
        return Ok(());
    }

    // Get current_path for working directory (default to home directory)
    let working_dir = {
        let data = state.lock().await;
        data.sessions.get(&chat_id)
            .and_then(|s| s.current_path.clone())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or_else(|| "/".to_string())
            })
    };

    // Send placeholder message
    let cmd_display = cmd_str.to_string();
    shared_rate_limit_wait(state, chat_id).await;
    let placeholder = tg!("send_message", bot.send_message(chat_id, format!("!{}에 대해서 처리중입니다.", &cmd_display)).await)?;
    let placeholder_msg_id = placeholder.id;

    // Register cancel token (lock) — must be AFTER placeholder send succeeds,
    // otherwise a failed send leaves the chat permanently locked.
    let cancel_token = Arc::new(CancelToken::new());
    {
        let mut data = state.lock().await;
        data.cancel_tokens.insert(chat_id, cancel_token.clone());
    }

    // Create channel
    let (tx, rx) = mpsc::channel();

    let cmd_owned = cmd_str.to_string();
    let working_dir_clone = working_dir.clone();
    let cancel_token_clone = cancel_token.clone();

    // Spawn blocking thread for shell command execution
    tokio::task::spawn_blocking(move || {
        let child = std::process::Command::new("bash")
            .args(["-c", &cmd_owned])
            .current_dir(&working_dir_clone)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(ShellOutput::Error(format!("Failed to execute: {}", e)));
                return;
            }
        };

        // Store PID for /stop kill
        if let Ok(mut guard) = cancel_token_clone.child_pid.lock() {
            *guard = Some(child.id());
        }

        // Read stderr in a separate thread
        let stderr_handle = child.stderr.take();
        let stderr_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(se) = stderr_handle {
                use std::io::BufRead;
                for line in std::io::BufReader::new(se).lines().flatten() {
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
            buf
        });

        // Read stdout line by line with cancel checks
        if let Some(stdout) = child.stdout.take() {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines().flatten() {
                if cancel_token_clone.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                let _ = tx.send(ShellOutput::Line(line));
            }
        }

        let stderr_output = stderr_thread.join().unwrap_or_default();
        if !stderr_output.is_empty() {
            let _ = tx.send(ShellOutput::Line(format!("[stderr]\n{}", stderr_output.trim_end())));
        }

        let status = child.wait();
        let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        let _ = tx.send(ShellOutput::Done { exit_code });
    });

    // Spawn polling loop (same pattern as AI streaming)
    let bot_owned = bot.clone();
    let state_owned = state.clone();
    let cmd_display_owned = cmd_display.clone();
    tokio::spawn(async move {
        const SPINNER: &[&str] = &[
            "🕐 P",           "🕑 Pr",          "🕒 Pro",
            "🕓 Proc",        "🕔 Proce",       "🕕 Proces",
            "🕖 Process",     "🕗 Processi",    "🕘 Processin",
            "🕙 Processing",  "🕚 Processing.", "🕛 Processing..",
        ];
        let mut full_output = String::new();
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut spin_idx: usize = 0;
        let mut exit_code: i32 = -1;
        let mut spawn_error: Option<String> = None;

        let polling_time_ms = {
            let data = state_owned.lock().await;
            data.polling_time_ms
        };
        let mut queue_done = false;
        let mut response_rendered = false;
        while !done || !queue_done {
            // Check cancel
            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done { cancelled = true; }
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(polling_time_ms)).await;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done { cancelled = true; }
                break;
            }

            // Drain channel
            if !done {
                loop {
                    match rx.try_recv() {
                        Ok(msg) => match msg {
                            ShellOutput::Line(line) => {
                                if !full_output.is_empty() {
                                    full_output.push('\n');
                                }
                                full_output.push_str(&line);
                            }
                            ShellOutput::Done { exit_code: code } => {
                                exit_code = code;
                                done = true;
                            }
                            ShellOutput::Error(e) => {
                                spawn_error = Some(e);
                                done = true;
                            }
                        },
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            done = true;
                            break;
                        }
                    }
                }

                // Update placeholder with spinner
                if !done {
                    let indicator = SPINNER[spin_idx % SPINNER.len()];
                    spin_idx += 1;

                    let display_text = format!("!{}에 대해서 처리중입니다.\n\n{}", cmd_display_owned, indicator);

                    if display_text != last_edit_text {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &display_text).await);
                        last_edit_text = display_text;
                    } else {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("send_chat_action", bot_owned.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await);
                    }
                }
            }

            // Render final result once
            if done && !response_rendered {
                response_rendered = true;

                if let Some(err) = &spawn_error {
                    // Spawn error - just show error message
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, err).await);
                } else {
                    if !full_output.trim().is_empty() {
                        let file_content = format!("$ {}\n\n{}", cmd_display_owned, full_output);
                        let content_bytes = file_content.len();

                        if content_bytes <= 4000 {
                            // Short output: update placeholder with completion + result in one call
                            let combined = format!("!{} 완료 (exit code: {})\n\n<pre>$ {}\n\n{}</pre>",
                                cmd_display_owned, exit_code,
                                html_escape(&cmd_display_owned), html_escape(full_output.trim()));
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            if let Err(_) = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &combined)
                                .parse_mode(ParseMode::Html)
                                .await)
                            {
                                let fallback = format!("!{} 완료 (exit code: {})\n\n$ {}\n\n{}",
                                    cmd_display_owned, exit_code, cmd_display_owned, full_output.trim());
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &fallback).await);
                            }
                        } else {
                            // Long output: update placeholder + send as .txt file
                            let final_msg = format!("!{} 완료 (exit code: {})", cmd_display_owned, exit_code);
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &final_msg).await);

                            let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
                            let tmp_path = format!("/tmp/cokacdir_shell_{}_{}.txt", chat_id.0, timestamp);
                            if std::fs::write(&tmp_path, &file_content).is_ok() {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!("send_document", bot_owned.send_document(
                                    chat_id,
                                    teloxide::types::InputFile::file(std::path::Path::new(&tmp_path)),
                                ).await);
                                let _ = std::fs::remove_file(&tmp_path);
                            }
                        }
                    } else {
                        // No output
                        let final_msg = format!("!{} 완료 (exit code: {})", cmd_display_owned, exit_code);
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &final_msg).await);
                    }
                }

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ▶ Shell command completed: !{}", cmd_display_owned);
            }

            // Queue processing
            let queued = process_upload_queue(&bot_owned, chat_id, &state_owned).await;
            if done {
                queue_done = !queued;
            }
        }

        // Post-loop: cancel handling
        if cancelled {
            if let Ok(guard) = cancel_token.child_pid.lock() {
                if let Some(pid) = *guard {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                }
            }

            shared_rate_limit_wait(&state_owned, chat_id).await;
            let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, "[Stopped]").await);

            let stop_msg_id = {
                let data = state_owned.lock().await;
                data.stop_message_ids.get(&chat_id).cloned()
            };
            if let Some(msg_id) = stop_msg_id {
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Shell command stopped: !{}", cmd_display_owned);

            let mut data = state_owned.lock().await;
            data.cancel_tokens.remove(&chat_id);
            data.stop_message_ids.remove(&chat_id);
            return;
        }

        // Clean up stop message if /stop raced with completion
        {
            let mut data = state_owned.lock().await;
            if let Some(msg_id) = data.stop_message_ids.remove(&chat_id) {
                drop(data);
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
            }
        }

        // Release lock
        {
            let mut data = state_owned.lock().await;
            data.cancel_tokens.remove(&chat_id);
        }
    });

    Ok(())
}

/// Handle /availabletools command - show all available tools
async fn handle_availabletools_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let mut msg = String::from("<b>Available Tools</b>\n\n");

    for &(name, desc, destructive) in ALL_TOOLS {
        let badge = risk_badge(destructive);
        if badge.is_empty() {
            msg.push_str(&format!("<code>{}</code> — {}\n", html_escape(name), html_escape(desc)));
        } else {
            msg.push_str(&format!("<code>{}</code> {} — {}\n", html_escape(name), badge, html_escape(desc)));
        }
    }
    msg.push_str(&format!("\n{} = destructive\nTotal: {}", risk_badge(true), ALL_TOOLS.len()));

    send_long_message(bot, chat_id, &msg, Some(ParseMode::Html), state).await?;

    Ok(())
}

/// Handle /allowedtools command - show current allowed tools list
async fn handle_allowedtools_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let tools = {
        let data = state.lock().await;
        get_allowed_tools(&data.settings, chat_id)
    };

    let mut msg = String::from("<b>Allowed Tools</b>\n\n");
    for tool in &tools {
        let (desc, destructive) = tool_info(tool);
        let badge = risk_badge(destructive);
        if badge.is_empty() {
            msg.push_str(&format!("<code>{}</code> — {}\n", html_escape(tool), html_escape(desc)));
        } else {
            msg.push_str(&format!("<code>{}</code> {} — {}\n", html_escape(tool), badge, html_escape(desc)));
        }
    }
    msg.push_str(&format!("\n{} = destructive\nTotal: {}", risk_badge(true), tools.len()));

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, &msg)
        .parse_mode(ParseMode::Html)
        .await)?;

    Ok(())
}

/// Handle /setpollingtime command - set Telegram API polling interval
async fn handle_setpollingtime_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
) -> ResponseResult<()> {
    let arg = text.strip_prefix("/setpollingtime").unwrap_or("").trim();

    if arg.is_empty() {
        let current = {
            let data = state.lock().await;
            data.polling_time_ms
        };
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, format!("Current polling time: {}ms\nUsage: /setpollingtime <ms>\nMinimum: 2500ms", current))
            .await)?;
        return Ok(());
    }

    let value: u64 = match arg.parse() {
        Ok(v) => v,
        Err(_) => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "Invalid number. Usage: /setpollingtime <ms>\nExample: /setpollingtime 3000")
                .await)?;
            return Ok(());
        }
    };

    if value < 2500 {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Minimum polling time is 2500ms.")
            .await)?;
        return Ok(());
    }

    // Update in-memory state
    {
        let mut data = state.lock().await;
        data.polling_time_ms = value;
    }

    // Save to settings.json
    if let Ok(mut app_settings) = crate::config::Settings::load_with_error() {
        app_settings.telegram_polling_time = value;
        let _ = app_settings.save();
    }

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, format!("✅ Polling time set to {}ms", value))
        .await)?;

    Ok(())
}

/// Handle /debug command - toggle API debug logging
async fn handle_debug_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let prev = TG_DEBUG.load(Ordering::Relaxed);
    let next = !prev;
    TG_DEBUG.store(next, Ordering::Relaxed);
    let status = if next { "ON" } else { "OFF" };
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, format!("🔍 Debug logging: {status}"))
        .await)?;
    Ok(())
}

/// Handle /allowed command - add/remove tools
/// Usage: /allowed +toolname  (add)
///        /allowed -toolname  (remove)
///        /allowed +tool1 -tool2 +tool3  (multiple)
async fn handle_allowed_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let arg = text.strip_prefix("/allowed").unwrap_or("").trim();

    if arg.is_empty() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Usage:\n/allowed +toolname — Add a tool\n/allowed -toolname — Remove a tool\n/allowed +tool1 -tool2 — Multiple at once\n/allowedtools — Show current list")
            .await)?;
        return Ok(());
    }

    // Skip if argument starts with "tools" (that's /allowedtools handled separately)
    if arg.starts_with("tools") {
        // This shouldn't happen due to routing order, but just in case
        return handle_allowedtools_command(bot, chat_id, state).await;
    }

    // Parse multiple +name / -name tokens
    let mut operations: Vec<(char, String)> = Vec::new();
    for token_str in arg.split_whitespace() {
        if let Some(name) = token_str.strip_prefix('+') {
            let name = name.trim();
            if !name.is_empty() {
                operations.push(('+', normalize_tool_name(name)));
            }
        } else if let Some(name) = token_str.strip_prefix('-') {
            let name = name.trim();
            if !name.is_empty() {
                operations.push(('-', normalize_tool_name(name)));
            }
        }
    }

    if operations.is_empty() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Use +toolname to add or -toolname to remove.\nExample: /allowed +Bash -Edit")
            .await)?;
        return Ok(());
    }

    let response_msg = {
        let mut data = state.lock().await;
        let chat_key = chat_id.0.to_string();
        // Ensure this chat has its own tool list (initialize from defaults if missing)
        if !data.settings.allowed_tools.contains_key(&chat_key) {
            let defaults: Vec<String> = DEFAULT_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect();
            data.settings.allowed_tools.insert(chat_key.clone(), defaults);
        }
        let tools = data.settings.allowed_tools.get_mut(&chat_key).unwrap();
        let mut results: Vec<String> = Vec::new();
        let mut changed = false;
        for (op, tool_name) in &operations {
            match op {
                '+' => {
                    if tools.iter().any(|t| t == tool_name) {
                        results.push(format!("<code>{}</code> already in list", html_escape(tool_name)));
                    } else {
                        tools.push(tool_name.clone());
                        changed = true;
                        results.push(format!("✅ <code>{}</code>", html_escape(tool_name)));
                    }
                }
                '-' => {
                    let before_len = tools.len();
                    tools.retain(|t| t != tool_name);
                    if tools.len() < before_len {
                        changed = true;
                        results.push(format!("❌ <code>{}</code>", html_escape(tool_name)));
                    } else {
                        results.push(format!("<code>{}</code> not in list", html_escape(tool_name)));
                    }
                }
                _ => unreachable!(),
            }
        }
        if changed {
            save_bot_settings(token, &data.settings);
        }
        results.join("\n")
    };

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, &response_msg)
        .parse_mode(ParseMode::Html)
        .await)?;

    Ok(())
}

/// Handle /public command - toggle public access for group chats
async fn handle_public_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
    is_group_chat: bool,
    is_owner: bool,
) -> ResponseResult<()> {
    if !is_group_chat {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "This command is only available in group chats.")
            .await)?;
        return Ok(());
    }

    if !is_owner {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Only the bot owner can change public access settings.")
            .await)?;
        return Ok(());
    }

    let arg = text.strip_prefix("/public").unwrap_or("").trim().to_lowercase();
    let chat_key = chat_id.0.to_string();

    let response_msg = match arg.as_str() {
        "on" => {
            let mut data = state.lock().await;
            data.settings.as_public_for_group_chat.insert(chat_key, true);
            save_bot_settings(token, &data.settings);
            "✅ Public access <b>enabled</b> for this group.\nAll members can now use the bot.".to_string()
        }
        "off" => {
            let mut data = state.lock().await;
            data.settings.as_public_for_group_chat.remove(&chat_key);
            save_bot_settings(token, &data.settings);
            "❌ Public access <b>disabled</b> for this group.\nOnly the owner can use the bot.".to_string()
        }
        "" => {
            let data = state.lock().await;
            let is_public = data.settings.as_public_for_group_chat.get(&chat_key).copied().unwrap_or(false);
            let status = if is_public { "enabled" } else { "disabled" };
            format!(
                "Public access is currently <b>{}</b> for this group.\n\n\
                 <code>/public on</code> — Allow all members\n\
                 <code>/public off</code> — Owner only",
                status
            )
        }
        _ => {
            "Usage:\n<code>/public on</code> — Allow all group members\n<code>/public off</code> — Owner only".to_string()
        }
    };

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, &response_msg)
        .parse_mode(ParseMode::Html)
        .await)?;

    Ok(())
}

/// Handle regular text messages - send to Claude AI
async fn handle_text_message(
    bot: &Bot,
    chat_id: ChatId,
    user_text: &str,
    state: &SharedState,
) -> ResponseResult<()> {
    // Get session info, allowed tools, and pending uploads (drop lock before any await)
    let (session_info, allowed_tools, pending_uploads) = {
        let mut data = state.lock().await;
        let info = data.sessions.get(&chat_id).and_then(|session| {
            session.current_path.as_ref().map(|_| {
                (session.session_id.clone(), session.current_path.clone().unwrap_or_default())
            })
        });
        let tools = get_allowed_tools(&data.settings, chat_id);
        // Drain pending uploads so they are sent to Claude exactly once
        let uploads = data.sessions.get_mut(&chat_id)
            .map(|s| {
                s.cleared = false; // Reset cleared flag on new message
                std::mem::take(&mut s.pending_uploads)
            })
            .unwrap_or_default();
        (info, tools, uploads)
    };

    let (session_id, current_path) = match session_info {
        Some(info) => info,
        None => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "No active session. Use /start <path> first.")
                .await)?;
            return Ok(());
        }
    };

    // Note: user message is NOT added to history here.
    // It will be added together with the assistant response in the spawned task,
    // only on successful completion. On cancel, nothing is recorded.

    // Send placeholder message (update shared timestamp so spawned task knows)
    shared_rate_limit_wait(state, chat_id).await;
    let placeholder = tg!("send_message", bot.send_message(chat_id, "...").await)?;
    let placeholder_msg_id = placeholder.id;

    // Sanitize input
    let sanitized_input = ai_screen::sanitize_user_input(user_text);

    // Prepend pending file upload records so Claude knows about recently uploaded files
    let context_prompt = if pending_uploads.is_empty() {
        sanitized_input
    } else {
        let upload_context = pending_uploads.join("\n");
        format!("{}\n\n{}", upload_context, sanitized_input)
    };

    // Build disabled tools notice
    let default_tools: std::collections::HashSet<&str> = DEFAULT_ALLOWED_TOOLS.iter().copied().collect();
    let allowed_set: std::collections::HashSet<&str> = allowed_tools.iter().map(|s| s.as_str()).collect();
    let disabled: Vec<&&str> = default_tools.iter().filter(|t| !allowed_set.contains(**t)).collect();
    let disabled_notice = if disabled.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = disabled.iter().map(|t| **t).collect();
        format!(
            "\n\nDISABLED TOOLS: The following tools have been disabled by the user: {}.\n\
             You MUST NOT attempt to use these tools. \
             If a user's request requires a disabled tool, do NOT proceed with the task. \
             Instead, clearly inform the user which tool is needed and that it is currently disabled. \
             Suggest they re-enable it with: /allowed +ToolName",
            names.join(", ")
        )
    };

    // Build system prompt with sendfile instructions
    let system_prompt_owned = format!(
        "You are chatting with a user through Telegram.\n\
         Current working directory: {}\n\n\
         When your work produces a file the user would want (generated code, reports, images, archives, etc.),\n\
         send it by running this bash command:\n\n\
         cokacdir --sendfile <filepath> --chat {} --key {}\n\n\
         This delivers the file directly to the user's Telegram chat.\n\
         Do NOT tell the user to use /down — use the command above instead.\n\n\
         Always keep the user informed about what you are doing. \
         Briefly explain each step as you work (e.g. \"Reading the file...\", \"Creating the script...\", \"Running tests...\"). \
         The user cannot see your tool calls, so narrate your progress so they know what is happening.\n\n\
         IMPORTANT: The user is on Telegram and CANNOT interact with any interactive prompts, dialogs, or confirmation requests. \
         All tools that require user interaction (such as AskUserQuestion, EnterPlanMode, ExitPlanMode) will NOT work. \
         Never use tools that expect user interaction. If you need clarification, just ask in plain text.{}",
        current_path, chat_id.0, token_hash(bot.token()), disabled_notice
    );

    // Create cancel token for this request
    let cancel_token = Arc::new(CancelToken::new());
    {
        let mut data = state.lock().await;
        data.cancel_tokens.insert(chat_id, cancel_token.clone());
    }

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();

    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();

    // Run Claude in a blocking thread
    tokio::task::spawn_blocking(move || {
        let result = claude::execute_command_streaming(
            &context_prompt,
            session_id_clone.as_deref(),
            &current_path_clone,
            tx.clone(),
            Some(&system_prompt_owned),
            Some(&allowed_tools),
            Some(cancel_token_clone),
        );

        if let Err(e) = result {
            let _ = tx.send(StreamMessage::Error { message: e, stdout: String::new(), stderr: String::new(), exit_code: None });
        }
    });

    // Spawn the polling loop as a separate task so the handler returns immediately.
    // This allows teloxide's per-chat worker to process subsequent messages (e.g. /stop).
    let bot_owned = bot.clone();
    let state_owned = state.clone();
    let user_text_owned = user_text.to_string();
    tokio::spawn(async move {
        const SPINNER: &[&str] = &[
            "🕐 P",           "🕑 Pr",          "🕒 Pro",
            "🕓 Proc",        "🕔 Proce",       "🕕 Proces",
            "🕖 Process",     "🕗 Processi",    "🕘 Processin",
            "🕙 Processing",  "🕚 Processing.", "🕛 Processing..",
        ];
        let mut full_response = String::new();
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut new_session_id: Option<String> = None;
        let mut spin_idx: usize = 0;

        let polling_time_ms = {
            let data = state_owned.lock().await;
            data.polling_time_ms
        };
        let mut queue_done = false;
        let mut response_rendered = false;
        while !done || !queue_done {
            // Check cancel token
            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done { cancelled = true; }
                break;
            }

            // Sleep as polling interval (without reserving a rate limit slot)
            tokio::time::sleep(tokio::time::Duration::from_millis(polling_time_ms)).await;

            // Check cancel token again after sleep
            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done { cancelled = true; }
                break;
            }

            // === Phase 1: AI streaming (while !done) ===
            if !done {
                // Drain all available messages
                loop {
                    match rx.try_recv() {
                        Ok(msg) => {
                            match msg {
                                StreamMessage::Init { session_id: sid } => {
                                    new_session_id = Some(sid);
                                }
                                StreamMessage::Text { content } => {
                                    full_response.push_str(&content);
                                }
                                StreamMessage::ToolUse { name, input } => {
                                    let summary = format_tool_input(&name, &input);
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!("  [{ts}]   ⚙ {name}: {}", truncate_str(&summary, 80));
                                    full_response.push_str(&format!("\n\n⚙️ {}\n", summary));
                                }
                                StreamMessage::ToolResult { content, is_error } => {
                                    if is_error {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        println!("  [{ts}]   ✗ Error: {}", truncate_str(&content, 80));
                                        let truncated = truncate_str(&content, 500);
                                        if truncated.contains('\n') {
                                            full_response.push_str(&format!("\n❌\n```\n{}\n```\n", truncated));
                                        } else {
                                            full_response.push_str(&format!("\n❌ `{}`\n\n", truncated));
                                        }
                                    } else if !content.is_empty() {
                                        let truncated = truncate_str(&content, 300);
                                        if truncated.contains('\n') {
                                            full_response.push_str(&format!("\n```\n{}\n```\n", truncated));
                                        } else {
                                            full_response.push_str(&format!("\n✅ `{}`\n\n", truncated));
                                        }
                                    }
                                }
                                StreamMessage::TaskNotification { summary, .. } => {
                                    if !summary.is_empty() {
                                        full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                    }
                                }
                                StreamMessage::Done { result, session_id: sid } => {
                                    if !result.is_empty() && full_response.is_empty() {
                                        full_response = result;
                                    }
                                    if let Some(s) = sid {
                                        new_session_id = Some(s);
                                    }
                                    done = true;
                                }
                                StreamMessage::Error { message, stdout, stderr, exit_code } => {
                                    let stdout_display = if stdout.is_empty() { "(empty)".to_string() } else { stdout };
                                    let stderr_display = if stderr.is_empty() { "(empty)".to_string() } else { stderr };
                                    let code_display = match exit_code {
                                        Some(c) => c.to_string(),
                                        None => "(unknown)".to_string(),
                                    };
                                    full_response = format!(
                                        "Error: {}\n```\nexit code: {}\n\n[stdout]\n{}\n\n[stderr]\n{}\n```",
                                        message, code_display, stdout_display, stderr_display
                                    );
                                    done = true;
                                }
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            done = true;
                            break;
                        }
                    }
                }

                // Build display text with spinning clock+text indicator appended
                let indicator = SPINNER[spin_idx % SPINNER.len()];
                spin_idx += 1;

                let display_text = if full_response.is_empty() {
                    indicator.to_string()
                } else {
                    let normalized = normalize_empty_lines(&full_response);
                    let truncated = truncate_str(&normalized, TELEGRAM_MSG_LIMIT - 20);
                    format!("{}\n\n{}", truncated, indicator)
                };

                if display_text != last_edit_text && !done {
                    // Rate limit: reserve slot right before the actual API call
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let html_text = markdown_to_telegram_html(&display_text);
                    if let Err(e) = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_text)
                        .parse_mode(ParseMode::Html)
                        .await)
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}]   ⚠ edit_message failed (streaming): {e}");
                    }
                    last_edit_text = display_text;
                } else if !done {
                    // No new content to display, send typing indicator
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!("send_chat_action", bot_owned.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await);
                }
            }

            // === Render final response once when AI completes ===
            if done && !response_rendered {
                response_rendered = true;

                let stop_msg_id = {
                    let data = state_owned.lock().await;
                    data.stop_message_ids.get(&chat_id).cloned()
                };

                // Rate limit before final API call
                shared_rate_limit_wait(&state_owned, chat_id).await;

                // Final response
                if full_response.is_empty() {
                    full_response = "(No response)".to_string();
                }

                let final_response = normalize_empty_lines(&full_response);
                let html_response = markdown_to_telegram_html(&final_response);

                if html_response.len() <= TELEGRAM_MSG_LIMIT {
                    if let Err(e) = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_response)
                        .parse_mode(ParseMode::Html)
                        .await)
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}]   ⚠ edit_message failed (HTML): {e}");
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &final_response)
                            .await);
                    }
                } else {
                    let send_result = send_long_message(&bot_owned, chat_id, &html_response, Some(ParseMode::Html), &state_owned).await;
                    match send_result {
                        Ok(_) => {
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                        }
                        Err(e) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!("  [{ts}]   ⚠ send_long_message failed (HTML): {e}");
                            let fallback_result = send_long_message(&bot_owned, chat_id, &final_response, None, &state_owned).await;
                            match fallback_result {
                                Ok(_) => {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                                }
                                Err(e2) => {
                                    println!("  [{ts}]   ⚠ send_long_message failed (plain): {e2}");
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let truncated = truncate_str(&final_response, TELEGRAM_MSG_LIMIT);
                                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &truncated)
                                        .await);
                                }
                            }
                        }
                    }
                }

                // Clean up leftover "Stopping..." message if /stop raced with normal completion
                if let Some(msg_id) = stop_msg_id {
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
                }

                // Update session state
                {
                    let mut data = state_owned.lock().await;
                    if let Some(session) = data.sessions.get_mut(&chat_id) {
                        if !session.cleared {
                            if let Some(sid) = new_session_id.take() {
                                session.session_id = Some(sid);
                            }
                            session.history.push(HistoryItem {
                                item_type: HistoryType::User,
                                content: user_text_owned.clone(),
                            });
                            session.history.push(HistoryItem {
                                item_type: HistoryType::Assistant,
                                content: final_response,
                            });
                            save_session_to_file(session, &current_path);
                        }
                    }
                }

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ▶ Response sent");
            }

            // === Queue processing (both during streaming and after done) ===
            let queued = process_upload_queue(&bot_owned, chat_id, &state_owned).await;
            if done {
                queue_done = !queued;
            }
        }

        // === Post-loop: cancelled handling or lock release ===
        if cancelled {
            if let Ok(guard) = cancel_token.child_pid.lock() {
                if let Some(pid) = *guard {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                }
            }

            let stopped_response = if full_response.trim().is_empty() {
                "[Stopped]".to_string()
            } else {
                let normalized = normalize_empty_lines(&full_response);
                format!("{}\n\n[Stopped]", normalized)
            };

            shared_rate_limit_wait(&state_owned, chat_id).await;

            let html_stopped = markdown_to_telegram_html(&stopped_response);
            if html_stopped.len() <= TELEGRAM_MSG_LIMIT {
                if let Err(e) = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_stopped)
                    .parse_mode(ParseMode::Html)
                    .await)
                {
                    let ts_err = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts_err}]   ⚠ edit_message failed (stopped/HTML): {e}");
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &stopped_response)
                        .await);
                }
            } else {
                let send_result = send_long_message(&bot_owned, chat_id, &html_stopped, Some(ParseMode::Html), &state_owned).await;
                match send_result {
                    Ok(_) => {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                    }
                    Err(e) => {
                        let ts_err = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts_err}]   ⚠ send_long_message failed (stopped/HTML): {e}");
                        let fallback = send_long_message(&bot_owned, chat_id, &stopped_response, None, &state_owned).await;
                        match fallback {
                            Ok(_) => {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                            }
                            Err(_) => {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let truncated = truncate_str(&stopped_response, TELEGRAM_MSG_LIMIT);
                                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &truncated)
                                    .await);
                            }
                        }
                    }
                }
            }

            let stop_msg_id = {
                let data = state_owned.lock().await;
                data.stop_message_ids.get(&chat_id).cloned()
            };
            if let Some(msg_id) = stop_msg_id {
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Stopped");

            let mut data = state_owned.lock().await;
            if let Some(session) = data.sessions.get_mut(&chat_id) {
                if !session.cleared {
                    if let Some(sid) = new_session_id {
                        session.session_id = Some(sid);
                    }
                    session.history.push(HistoryItem {
                        item_type: HistoryType::User,
                        content: user_text_owned,
                    });
                    session.history.push(HistoryItem {
                        item_type: HistoryType::Assistant,
                        content: stopped_response,
                    });
                    save_session_to_file(session, &current_path);
                }
            }
            data.cancel_tokens.remove(&chat_id);
            data.stop_message_ids.remove(&chat_id);
            return;
        }

        // Clean up "Stopping..." message if /stop was sent during queue drain
        {
            let mut data = state_owned.lock().await;
            if let Some(msg_id) = data.stop_message_ids.remove(&chat_id) {
                drop(data);
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
            }
        }

        // Release lock: allow new messages for this chat
        {
            let mut data = state_owned.lock().await;
            data.cancel_tokens.remove(&chat_id);
        }
    });

    Ok(())
}

/// Load existing session from ai_sessions directory matching the given path
fn load_existing_session(current_path: &str) -> Option<(SessionData, std::time::SystemTime)> {
    let sessions_dir = ai_screen::ai_sessions_dir()?;

    if !sessions_dir.exists() {
        return None;
    }

    let mut matching_session: Option<(SessionData, std::time::SystemTime)> = None;

    if let Ok(entries) = fs::read_dir(&sessions_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(session_data) = serde_json::from_str::<SessionData>(&content) {
                        if session_data.current_path == current_path {
                            if let Ok(metadata) = path.metadata() {
                                if let Ok(modified) = metadata.modified() {
                                    match &matching_session {
                                        None => matching_session = Some((session_data, modified)),
                                        Some((_, latest_time)) if modified > *latest_time => {
                                            matching_session = Some((session_data, modified));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    matching_session
}

/// Save session to file in the ai_sessions directory
fn save_session_to_file(session: &ChatSession, current_path: &str) {
    let Some(ref session_id) = session.session_id else {
        return;
    };

    if session.history.is_empty() {
        return;
    }

    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        return;
    };

    if fs::create_dir_all(&sessions_dir).is_err() {
        return;
    }

    // Filter out system messages
    let saveable_history: Vec<HistoryItem> = session.history.iter()
        .filter(|item| !matches!(item.item_type, HistoryType::System))
        .cloned()
        .collect();

    if saveable_history.is_empty() {
        return;
    }

    let session_data = SessionData {
        session_id: session_id.clone(),
        history: saveable_history,
        current_path: current_path.to_string(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    };

    let file_path = sessions_dir.join(format!("{}.json", session_id));

    // Security: Verify the path is within sessions directory
    if let Some(parent) = file_path.parent() {
        if parent != sessions_dir {
            return;
        }
    }

    if let Ok(json) = serde_json::to_string_pretty(&session_data) {
        let _ = fs::write(file_path, json);
    }
}

/// Find the largest byte index <= `index` that is a valid UTF-8 char boundary
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        s.len()
    } else {
        let mut i = index;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
}

/// Process one pending upload queue file for the given chat.
/// Scans ~/.cokacdir/upload_queue/ for .queue files matching the current bot and chat_id,
/// sends the oldest one, and deletes the queue file on success.
/// Returns true if a file was processed (rate limit slot consumed).
async fn process_upload_queue(bot: &Bot, chat_id: ChatId, state: &SharedState) -> bool {
    let queue_dir = match dirs::home_dir() {
        Some(h) => h.join(".cokacdir").join("upload_queue"),
        None => return false,
    };
    if !queue_dir.is_dir() {
        return false;
    }

    let current_key = token_hash(bot.token());

    // Collect and sort queue files by name (timestamp-based, so alphabetical = chronological)
    let mut entries: Vec<std::path::PathBuf> = match fs::read_dir(&queue_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("queue"))
            .collect(),
        Err(_) => return false,
    };
    entries.sort();

    // Find the first entry matching this bot and chat_id
    for entry_path in entries {
        let content = match fs::read_to_string(&entry_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let json: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let file_chat_id = json.get("chat_id").and_then(|v| v.as_i64()).unwrap_or(0);
        let file_key = json.get("key").and_then(|v| v.as_str()).unwrap_or("");
        let file_path = json.get("path").and_then(|v| v.as_str()).unwrap_or("");

        if file_chat_id != chat_id.0 || file_key != current_key || file_path.is_empty() {
            continue;
        }

        let path = std::path::PathBuf::from(file_path);
        if !path.exists() {
            // File no longer exists, remove queue entry
            let _ = fs::remove_file(&entry_path);
            return false;
        }

        // Remove queue file before sending (regardless of send result)
        let _ = fs::remove_file(&entry_path);

        // Rate limit and send
        shared_rate_limit_wait(state, chat_id).await;
        match tg!("send_document", bot.send_document(
            chat_id,
            teloxide::types::InputFile::file(&path),
        ).await) {
            Ok(_) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}]   📤 Upload sent: {}", file_path);
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}]   ⚠ Upload failed: {e}");
            }
        }
        return true;
    }

    false
}

/// Acquires the lock briefly to calculate and reserve the next API call slot,
/// then releases the lock and sleeps until the reserved time.
/// This ensures that even concurrent tasks for the same chat maintain 3s gaps.
async fn shared_rate_limit_wait(state: &SharedState, chat_id: ChatId) {
    let sleep_until = {
        let mut data = state.lock().await;
        let min_gap = tokio::time::Duration::from_millis(data.polling_time_ms);
        let last = data.api_timestamps.entry(chat_id).or_insert_with(||
            tokio::time::Instant::now() - tokio::time::Duration::from_secs(10)
        );
        let earliest_next = *last + min_gap;
        let now = tokio::time::Instant::now();
        let target = if earliest_next > now { earliest_next } else { now };
        *last = target; // Reserve this slot
        target
    }; // Mutex released here
    tokio::time::sleep_until(sleep_until).await;
}

/// Send a message that may exceed Telegram's 4096 character limit
/// by splitting it into multiple messages, handling UTF-8 boundaries
/// and unclosed HTML tags (e.g. <pre>) across split points
async fn send_long_message(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    parse_mode: Option<ParseMode>,
    state: &SharedState,
) -> ResponseResult<()> {
    if text.len() <= TELEGRAM_MSG_LIMIT {
        shared_rate_limit_wait(state, chat_id).await;
        let mut req = bot.send_message(chat_id, text);
        if let Some(mode) = parse_mode {
            req = req.parse_mode(mode);
        }
        tg!("send_message", req.await)?;
        return Ok(());
    }

    let is_html = parse_mode.is_some();
    let mut remaining = text;
    let mut in_pre = false;

    while !remaining.is_empty() {
        // Reserve space for tags we may need to add (<pre> + </pre> = 11 bytes)
        let tag_overhead = if is_html && in_pre { 11 } else { 0 };
        let effective_limit = TELEGRAM_MSG_LIMIT.saturating_sub(tag_overhead);

        if remaining.len() <= effective_limit {
            let mut chunk = String::new();
            if is_html && in_pre {
                chunk.push_str("<pre>");
            }
            chunk.push_str(remaining);

            shared_rate_limit_wait(state, chat_id).await;
            let mut req = bot.send_message(chat_id, &chunk);
            if let Some(mode) = parse_mode {
                req = req.parse_mode(mode);
            }
            tg!("send_message", req.await)?;
            break;
        }

        // Find a safe UTF-8 char boundary, then find a newline before it
        let safe_end = floor_char_boundary(remaining, effective_limit);
        let split_at = remaining[..safe_end]
            .rfind('\n')
            .unwrap_or(safe_end);

        let (raw_chunk, rest) = remaining.split_at(split_at);

        let mut chunk = String::new();
        if is_html && in_pre {
            chunk.push_str("<pre>");
        }
        chunk.push_str(raw_chunk);

        // Track unclosed <pre> tags to close/reopen across chunks
        if is_html {
            let last_open = raw_chunk.rfind("<pre>");
            let last_close = raw_chunk.rfind("</pre>");
            in_pre = match (last_open, last_close) {
                (Some(o), Some(c)) => o > c,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => in_pre,
            };
            if in_pre {
                chunk.push_str("</pre>");
            }
        }

        shared_rate_limit_wait(state, chat_id).await;
        let mut req = bot.send_message(chat_id, &chunk);
        if let Some(mode) = parse_mode {
            req = req.parse_mode(mode);
        }
        tg!("send_message", req.await)?;

        // Skip the newline character at the split point
        remaining = rest.strip_prefix('\n').unwrap_or(rest);
    }

    Ok(())
}

/// Normalize consecutive empty lines to maximum of one
fn normalize_empty_lines(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_was_empty = false;

    for line in s.lines() {
        let is_empty = line.is_empty();
        if is_empty {
            if !prev_was_empty {
                result.push('\n');
            }
            prev_was_empty = true;
        } else {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(line);
            prev_was_empty = false;
        }
    }

    result
}

/// Escape special HTML characters for Telegram HTML parse mode
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Truncate a string to max_len bytes, cutting at a safe UTF-8 char and line boundary
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }

    let safe_end = floor_char_boundary(s, max_len);
    let truncated = &s[..safe_end];
    if let Some(pos) = truncated.rfind('\n') {
        truncated[..pos].to_string()
    } else {
        truncated.to_string()
    }
}

/// Convert standard markdown to Telegram-compatible HTML
fn markdown_to_telegram_html(md: &str) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut result = String::new();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim_start();

        // Fenced code block
        if trimmed.starts_with("```") {
            let mut code_lines = Vec::new();
            i += 1; // skip opening ```
            while i < lines.len() {
                if lines[i].trim_start().starts_with("```") {
                    break;
                }
                code_lines.push(lines[i]);
                i += 1;
            }
            let code = code_lines.join("\n");
            if !code.is_empty() {
                result.push_str(&format!("<pre>{}</pre>", html_escape(code.trim_end())));
            }
            result.push('\n');
            i += 1; // skip closing ```
            continue;
        }

        // Heading (# ~ ######)
        if let Some(rest) = strip_heading(trimmed) {
            result.push_str(&format!("<b>{}</b>", convert_inline(&html_escape(rest))));
            result.push('\n');
            i += 1;
            continue;
        }

        // Unordered list (- or *)
        if trimmed.starts_with("- ") {
            result.push_str(&format!("• {}", convert_inline(&html_escape(&trimmed[2..]))));
            result.push('\n');
            i += 1;
            continue;
        }
        if trimmed.starts_with("* ") && !trimmed.starts_with("**") {
            result.push_str(&format!("• {}", convert_inline(&html_escape(&trimmed[2..]))));
            result.push('\n');
            i += 1;
            continue;
        }

        // Regular line
        result.push_str(&convert_inline(&html_escape(lines[i])));
        result.push('\n');
        i += 1;
    }

    result.trim_end().to_string()
}

/// Strip markdown heading prefix (# ~ ######), return remaining text
fn strip_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches('#');
    // Must have consumed at least one # and be followed by a space
    if trimmed.len() < line.len() && trimmed.starts_with(' ') {
        let hashes = line.len() - trimmed.len();
        if hashes <= 6 {
            return Some(trimmed.trim_start());
        }
    }
    None
}

/// Convert inline markdown elements (bold, italic, code) in already HTML-escaped text
fn convert_inline(text: &str) -> String {
    // Process inline code first to protect content from further conversion
    let mut result = String::new();
    let mut remaining = text;

    // Split by inline code spans: `...`
    loop {
        if let Some(start) = remaining.find('`') {
            let after_start = &remaining[start + 1..];
            if let Some(end) = after_start.find('`') {
                // Found a complete inline code span
                let before = &remaining[..start];
                let code_content = &after_start[..end];
                result.push_str(&convert_bold_italic(before));
                result.push_str(&format!("<code>{}</code>", code_content));
                remaining = &after_start[end + 1..];
                continue;
            }
        }
        // No more inline code spans
        result.push_str(&convert_bold_italic(remaining));
        break;
    }

    result
}

/// Convert bold (**...**) and italic (*...*) in text
fn convert_bold_italic(text: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Bold: **...**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_closing_marker(&chars, i + 2, &['*', '*']) {
                let inner: String = chars[i + 2..end].iter().collect();
                result.push_str(&format!("<b>{}</b>", inner));
                i = end + 2;
                continue;
            }
        }
        // Italic: *...*
        if chars[i] == '*' {
            if let Some(end) = find_closing_single(&chars, i + 1, '*') {
                let inner: String = chars[i + 1..end].iter().collect();
                result.push_str(&format!("<i>{}</i>", inner));
                i = end + 1;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Find closing double marker (e.g., **) starting from pos
fn find_closing_marker(chars: &[char], start: usize, marker: &[char; 2]) -> Option<usize> {
    let len = chars.len();
    let mut i = start;
    while i + 1 < len {
        if chars[i] == marker[0] && chars[i + 1] == marker[1] {
            // Don't match empty content
            if i > start {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Find closing single marker (e.g., *) starting from pos
fn find_closing_single(chars: &[char], start: usize, marker: char) -> Option<usize> {
    let len = chars.len();
    let mut i = start;
    while i < len {
        if chars[i] == marker {
            // Don't match empty or double marker
            if i > start && (i + 1 >= len || chars[i + 1] != marker) {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Format tool input JSON into a human-readable summary
fn format_tool_input(name: &str, input: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        return format!("{} {}", name, truncate_str(input, 200));
    };

    match name {
        "Bash" => {
            let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if !desc.is_empty() {
                format!("{}: `{}`", desc, truncate_str(cmd, 150))
            } else {
                format!("`{}`", truncate_str(cmd, 200))
            }
        }
        "Read" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            format!("Read {}", fp)
        }
        "Write" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let content = v.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let lines = content.lines().count();
            if lines > 0 {
                format!("Write {} ({} lines)", fp, lines)
            } else {
                format!("Write {}", fp)
            }
        }
        "Edit" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let replace_all = v.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
            if replace_all {
                format!("Edit {} (replace all)", fp)
            } else {
                format!("Edit {}", fp)
            }
        }
        "Glob" => {
            let pattern = v.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                format!("Glob {} in {}", pattern, path)
            } else {
                format!("Glob {}", pattern)
            }
        }
        "Grep" => {
            let pattern = v.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let output_mode = v.get("output_mode").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                if !output_mode.is_empty() {
                    format!("Grep \"{}\" in {} ({})", pattern, path, output_mode)
                } else {
                    format!("Grep \"{}\" in {}", pattern, path)
                }
            } else {
                format!("Grep \"{}\"", pattern)
            }
        }
        "NotebookEdit" => {
            let nb_path = v.get("notebook_path").and_then(|v| v.as_str()).unwrap_or("");
            let cell_id = v.get("cell_id").and_then(|v| v.as_str()).unwrap_or("");
            if !cell_id.is_empty() {
                format!("Notebook {} ({})", nb_path, cell_id)
            } else {
                format!("Notebook {}", nb_path)
            }
        }
        "WebSearch" => {
            let query = v.get("query").and_then(|v| v.as_str()).unwrap_or("");
            format!("Search: {}", query)
        }
        "WebFetch" => {
            let url = v.get("url").and_then(|v| v.as_str()).unwrap_or("");
            format!("Fetch {}", url)
        }
        "Task" => {
            let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let subagent_type = v.get("subagent_type").and_then(|v| v.as_str()).unwrap_or("");
            if !subagent_type.is_empty() {
                format!("Task [{}]: {}", subagent_type, desc)
            } else {
                format!("Task: {}", desc)
            }
        }
        "TaskOutput" => {
            let task_id = v.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            format!("Get task output: {}", task_id)
        }
        "TaskStop" => {
            let task_id = v.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            format!("Stop task: {}", task_id)
        }
        "TodoWrite" => {
            if let Some(todos) = v.get("todos").and_then(|v| v.as_array()) {
                let pending = todos.iter().filter(|t| {
                    t.get("status").and_then(|s| s.as_str()) == Some("pending")
                }).count();
                let in_progress = todos.iter().filter(|t| {
                    t.get("status").and_then(|s| s.as_str()) == Some("in_progress")
                }).count();
                let completed = todos.iter().filter(|t| {
                    t.get("status").and_then(|s| s.as_str()) == Some("completed")
                }).count();
                format!("Todo: {} pending, {} in progress, {} completed", pending, in_progress, completed)
            } else {
                "Update todos".to_string()
            }
        }
        "Skill" => {
            let skill = v.get("skill").and_then(|v| v.as_str()).unwrap_or("");
            format!("Skill: {}", skill)
        }
        "AskUserQuestion" => {
            if let Some(questions) = v.get("questions").and_then(|v| v.as_array()) {
                if let Some(q) = questions.first() {
                    let question = q.get("question").and_then(|v| v.as_str()).unwrap_or("");
                    truncate_str(question, 200)
                } else {
                    "Ask user question".to_string()
                }
            } else {
                "Ask user question".to_string()
            }
        }
        "ExitPlanMode" => {
            "Exit plan mode".to_string()
        }
        "EnterPlanMode" => {
            "Enter plan mode".to_string()
        }
        "TaskCreate" => {
            let subject = v.get("subject").and_then(|v| v.as_str()).unwrap_or("");
            format!("Create task: {}", subject)
        }
        "TaskUpdate" => {
            let task_id = v.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
            let status = v.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if !status.is_empty() {
                format!("Update task {}: {}", task_id, status)
            } else {
                format!("Update task {}", task_id)
            }
        }
        "TaskGet" => {
            let task_id = v.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
            format!("Get task: {}", task_id)
        }
        "TaskList" => {
            "List tasks".to_string()
        }
        _ => {
            format!("{} {}", name, truncate_str(input, 200))
        }
    }
}

