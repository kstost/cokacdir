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

use crate::services::claude::{self, CancelToken, StreamMessage, DEFAULT_ALLOWED_TOOLS};
use crate::services::codex;
use crate::services::gemini;
use crate::services::opencode;
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
        .write(true)
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

// ── Group Chat Shared Log ──
// All bots in the same group chat write to a shared JSONL file so that
// each bot can see what other bots said, solving the cross-bot context problem.

/// A single entry in the group chat shared log (JSONL format).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupChatLogEntry {
    /// ISO-8601 timestamp
    pub ts: String,
    /// Bot username that handled this message (without @)
    pub bot: String,
    /// Bot display name (first_name from Telegram API)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_display_name: Option<String>,
    /// "user" or "assistant" (or "system" for clear markers)
    pub role: String,
    /// Display name of the sender (for user messages)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Message text content
    pub text: String,
    /// If true, this entry is a clear marker — all previous entries from this bot should be ignored
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear: bool,
}

/// A parsed entry from the raw_payload text format used in assistant log entries.
/// Tag name can be any ASCII alphabetic string (e.g. "Text", "ToolUse", "ToolResult", ...).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RawPayloadEntry {
    pub tag: String,
    pub content: String,
}

/// Check if a line starts with a `[TagName] ` pattern where TagName is ASCII alphabetic.
/// Returns Some("[TagName]") if matched, None otherwise.
fn detect_raw_payload_tag(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let end = rest.find(']')?;
    let tag = &rest[..end];
    if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some(&line[..end + 2]) // "[TagName]"
}

/// Parse raw_payload flat text into structured entries.
/// Each section starts with `[TagName] ...` (tag is ASCII alphabetic) and continues
/// until the next tag or end of string. Multi-line content is preserved.
pub fn parse_raw_payload(text: &str) -> Vec<RawPayloadEntry> {
    let mut entries: Vec<RawPayloadEntry> = Vec::new();
    let mut cur_tag: Option<String> = None;
    let mut cur_content = String::new();

    for line in text.lines() {
        if let Some(bracket_tag) = detect_raw_payload_tag(line) {
            // Save previous section
            if let Some(tag) = cur_tag.take() {
                entries.push(RawPayloadEntry { tag, content: std::mem::take(&mut cur_content) });
            }
            // Extract tag name (strip [ and ])
            cur_tag = Some(bracket_tag[1..bracket_tag.len() - 1].to_string());
            let after = &line[bracket_tag.len()..];
            cur_content = after.strip_prefix(' ').unwrap_or(after).to_string();
        } else if cur_tag.is_some() {
            cur_content.push('\n');
            cur_content.push_str(line);
        }
    }
    if let Some(tag) = cur_tag {
        entries.push(RawPayloadEntry { tag, content: cur_content });
    }
    entries
}

/// Format structured entries back to raw_payload flat text.
pub fn format_raw_payload(entries: &[RawPayloadEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&format!("[{}] {}\n", entry.tag, entry.content));
    }
    out
}

/// Parse payload text with auto-detection: tries JSON array (new format) first,
/// falls back to legacy flat text parsing.
pub fn parse_payload_auto(text: &str) -> Vec<RawPayloadEntry> {
    if let Ok(entries) = serde_json::from_str::<Vec<RawPayloadEntry>>(text) {
        return entries;
    }
    parse_raw_payload(text)
}

/// Maximum content length before truncation and offloading to a separate file.
const PAYLOAD_TRUNCATE_LIMIT: usize = 500;

/// Save long content to ~/.cokacdir/values/<unique>.txt and return the file path.
/// Returns None if saving fails.
fn save_payload_value(content: &str) -> Option<String> {
    let dir = dirs::home_dir()?.join(".cokacdir").join("values");
    if fs::create_dir_all(&dir).is_err() { return None; }
    let ts = chrono::Local::now().format("%Y%m%d%H%M%S");
    use rand::Rng;
    let rand_suffix: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(6)
        .map(|b| (b as char).to_ascii_lowercase())
        .collect();
    let filename = format!("{}_{}.txt", ts, rand_suffix);
    let path = dir.join(&filename);
    if fs::write(&path, content).is_err() { return None; }
    Some(path.display().to_string())
}

/// Truncate payload entries whose content exceeds the limit.
/// Long content is saved to ~/.cokacdir/values/ and replaced with a truncated preview + file path.
fn truncate_payload_entries(entries: &[RawPayloadEntry]) -> Vec<RawPayloadEntry> {
    entries.iter().map(|entry| {
        if entry.content.chars().count() <= PAYLOAD_TRUNCATE_LIMIT {
            entry.clone()
        } else {
            let preview: String = entry.content.chars().take(PAYLOAD_TRUNCATE_LIMIT).collect();
            let truncated_content = if let Some(path) = save_payload_value(&entry.content) {
                format!("{}...\n(truncated, full content: {})", preview, path)
            } else {
                format!("{}...\n(truncated, failed to save full content)", preview)
            };
            RawPayloadEntry { tag: entry.tag.clone(), content: truncated_content }
        }
    }).collect()
}

/// Serialize payload entries to JSON string (new format).
/// Entries with content exceeding the limit are truncated and offloaded to separate files.
pub fn serialize_payload(entries: &[RawPayloadEntry]) -> String {
    let processed = truncate_payload_entries(entries);
    serde_json::to_string(&processed).unwrap_or_default()
}


/// RAII guard for per-chat exclusive lock.
/// Ensures only one AI request processes at a time within the same chat.
/// Lock is automatically released when the guard is dropped.
struct GroupChatLock {
    _file: std::fs::File,
}

/// Acquire exclusive file lock for a group chat (async, non-blocking).
/// Returns None for private chats (chat_id >= 0) or if lock file cannot be created.
/// For group chats, polls with sleep until the lock is acquired.
async fn acquire_group_chat_lock(chat_id: i64) -> Option<GroupChatLock> {
    use fs2::FileExt;
    if chat_id >= 0 { return None; }
    let dir = dirs::home_dir()?.join(".cokacdir").join("chat_locks");
    let _ = std::fs::create_dir_all(&dir);
    let lock_path = dir.join(format!("{}.lock", chat_id));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path).ok()?;
    let mut waited = false;
    loop {
        match file.try_lock_exclusive() {
            Ok(_) => {
                if waited {
                    msg_debug(&format!("[chat_lock] chat_id={} lock acquired after waiting", chat_id));
                } else {
                    msg_debug(&format!("[chat_lock] chat_id={} lock acquired immediately", chat_id));
                }
                return Some(GroupChatLock { _file: file });
            }
            Err(_) => {
                if !waited {
                    msg_debug(&format!("[chat_lock] chat_id={} lock contended, waiting...", chat_id));
                    waited = true;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Return the directory for group chat logs: ~/.cokacdir/group_chat/
fn group_chat_log_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".cokacdir").join("group_chat"))
}

/// Return the JSONL file path for a specific group chat.
fn group_chat_log_path(chat_id: i64) -> Option<std::path::PathBuf> {
    group_chat_log_dir().map(|d| d.join(format!("{}.jsonl", chat_id)))
}

/// Append an entry to the group chat shared log.
/// Uses a separate lock file for synchronization to avoid Windows LockFileEx
/// conflicts when locking and writing on the same file handle.
fn append_group_chat_log(chat_id: i64, entry: &GroupChatLogEntry) {
    use fs2::FileExt;

    let Some(path) = group_chat_log_path(chat_id) else { return };
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() { return; }
    }

    // Serialize before acquiring lock to minimize lock hold time
    let Ok(json) = serde_json::to_string(entry) else { return };

    // Skip entries containing --read_chat_log to prevent recursive snowball growth:
    // each --read_chat_log result embeds the entire log, causing exponential JSONL inflation.
    if json.contains("--read_chat_log") {
        msg_debug(&format!("[append_group_chat_log] SKIPPED: line contains --read_chat_log (len={})", json.len()));
        return;
    }

    let line = format!("{}\n", json);

    // Lock via a dedicated lock file (not the data file itself)
    // On Windows, LockFileEx is mandatory and can conflict with WriteFile on the same handle
    let lock_path = path.with_extension("jsonl.lock");
    let Ok(lock_file) = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path) else { return };
    if lock_file.lock_exclusive().is_err() { return; }

    // Write to data file without locking it
    if let Ok(mut data_file) = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = data_file.write_all(line.as_bytes());
        let _ = data_file.sync_data();
    }

    let _ = lock_file.unlock();
}

/// Read entries from the group chat log within a specific line range (1-based).
/// If `filter_bot` is Some, only include entries from that bot.
pub fn read_group_chat_log_range(
    chat_id: i64,
    range_start: usize,
    range_end: Option<usize>,
    filter_bot: Option<&str>,
) -> Vec<(usize, GroupChatLogEntry)> {
    use fs2::FileExt;

    let filter_bot_owned: Option<String> = filter_bot.map(|b| b.strip_prefix('@').unwrap_or(b).to_lowercase());
    let filter_bot = filter_bot_owned.as_deref();

    let Some(path) = group_chat_log_path(chat_id) else { return Vec::new() };

    // Use the same dedicated lock file as append_group_chat_log for coordination
    let lock_path = path.with_extension("jsonl.lock");
    let Ok(lock_file) = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path) else { return Vec::new() };
    if lock_file.lock_shared().is_err() { return Vec::new(); }

    let Ok(file) = fs::File::open(&path) else {
        let _ = lock_file.unlock();
        return Vec::new();
    };

    let reader = std::io::BufReader::new(&file);
    use std::io::BufRead;

    // First pass: collect all entries and find the last clear marker per bot
    let all_entries: Vec<(usize, GroupChatLogEntry)> = reader.lines()
        .filter_map(|line| line.ok())
        .enumerate()
        .filter_map(|(i, line)| {
            let line_num = i + 1; // 1-based
            serde_json::from_str::<GroupChatLogEntry>(&line)
                .ok()
                .map(|entry| (line_num, entry))
        })
        .collect();

    // Build a map of bot -> last clear marker line number
    let mut last_clear: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (line_num, entry) in &all_entries {
        if entry.clear {
            last_clear.insert(entry.bot.clone(), *line_num);
        }
    }

    // Second pass: filter entries, skipping those before the clear marker for each bot
    let entries: Vec<(usize, GroupChatLogEntry)> = all_entries.into_iter()
        .filter(|(line_num, entry)| {
            // Skip clear marker entries themselves
            if entry.clear { return false; }
            // Skip entries from a bot that are before its last clear marker
            if let Some(&clear_line) = last_clear.get(&entry.bot) {
                if *line_num <= clear_line { return false; }
            }
            let in_range = *line_num >= range_start
                && range_end.map_or(true, |end| *line_num <= end);
            let bot_match = filter_bot.map_or(true, |bot| entry.bot == bot);
            in_range && bot_match
        })
        .collect();

    let _ = lock_file.unlock();
    entries
}

/// Collect all bots that have ever appeared in a group chat log, with their
/// most recent display name.  Scans the entire file including entries before
/// clear markers (and the markers themselves) so that bots whose history was
/// cleared are still listed.
///
/// Returns a Vec of (bot_username, Option<display_name>) sorted by username.
pub fn collect_group_chat_bots(chat_id: i64) -> Vec<(String, Option<String>)> {
    use fs2::FileExt;
    use std::io::BufRead;

    let Some(path) = group_chat_log_path(chat_id) else { return Vec::new() };

    let lock_path = path.with_extension("jsonl.lock");
    let Ok(lock_file) = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path) else { return Vec::new() };
    if lock_file.lock_shared().is_err() { return Vec::new(); }

    let Ok(file) = fs::File::open(&path) else {
        let _ = lock_file.unlock();
        return Vec::new();
    };

    // Scan forward — later entries overwrite earlier ones, keeping the latest display name
    let mut bots: std::collections::HashMap<String, Option<String>> = std::collections::HashMap::new();
    for line in std::io::BufReader::new(&file).lines().flatten() {
        if let Ok(entry) = serde_json::from_str::<GroupChatLogEntry>(&line) {
            if !entry.bot.is_empty() {
                bots.insert(entry.bot.clone(), entry.bot_display_name);
            }
        }
    }

    let _ = lock_file.unlock();

    let mut result: Vec<(String, Option<String>)> = bots.into_iter().collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

/// Per-chat session state
#[derive(Clone)]
struct ChatSession {
    session_id: Option<String>,
    current_path: Option<String>,
    history: Vec<HistoryItem>,
    /// File upload records not yet sent to Claude AI.
    /// Drained and prepended to the next user prompt so Claude knows about uploaded files.
    pending_uploads: Vec<String>,
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
    /// chat_id (string) → model name (e.g. "claude", "claude:claude-sonnet-4-6", "codex:gpt-5.4")
    models: HashMap<String, String>,
    /// Debug logging toggle
    debug: bool,
    /// chat_id (string) → true if silent mode enabled
    silent: HashMap<String, bool>,
    /// chat_id (string) → true if direct mode enabled (group chat without ; prefix)
    direct: HashMap<String, bool>,
    /// chat_id (string) → number of recent group chat log entries to embed in system prompt (default 12)
    context: HashMap<String, usize>,
    /// chat_id (string) → system instruction for AI
    instructions: HashMap<String, String>,
    /// chat_id (string) → true if queue mode enabled (queue messages while AI is busy)
    queue: HashMap<String, bool>,
    /// Bot's Telegram username (stored at startup via get_me)
    username: String,
    /// Bot's display name (first_name from Telegram API, stored at startup via get_me)
    display_name: String,
    /// Compact startup greeting (show single line instead of full marketing message)
    greeting: bool,
    /// chat_id (string) → true if --chrome flag should be passed to Claude CLI
    use_chrome: HashMap<String, bool>,
    /// chat_id (string) → message to send when AI processing completes
    end_hook: HashMap<String, String>,
}

impl Default for BotSettings {
    fn default() -> Self {
        Self {
            allowed_tools: HashMap::new(),
            last_sessions: HashMap::new(),
            owner_user_id: None,
            as_public_for_group_chat: HashMap::new(),
            models: HashMap::new(),
            debug: false,
            silent: HashMap::new(),
            direct: HashMap::new(),
            context: HashMap::new(),
            instructions: HashMap::new(),
            queue: HashMap::new(),
            username: String::new(),
            display_name: String::new(),
            greeting: false,
            use_chrome: HashMap::new(),
            end_hook: HashMap::new(),
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

/// Get the configured model for a specific chat_id, if any.
/// Migrates legacy bare names (e.g. "sonnet") to "claude:" prefixed format.
fn get_model(settings: &BotSettings, chat_id: ChatId) -> Option<String> {
    let key = chat_id.0.to_string();
    settings.models.get(&key).map(|m| {
        match m.as_str() {
            "sonnet" | "opus" | "haiku" |
            "sonnet[1m]" | "opus[1m]" | "haiku[1m]" => format!("claude:{}", m),
            _ => m.clone(),
        }
    })
}

/// Check if silent mode is enabled for a chat (default: ON)
fn is_silent(settings: &BotSettings, chat_id: ChatId) -> bool {
    settings.silent.get(&chat_id.0.to_string()).copied().unwrap_or(SILENT_MODE_DEFAULT)
}

/// Schedule entry persisted as JSON in ~/.cokacdir/schedule/
#[derive(Clone)]
struct ScheduleEntry {
    id: String,
    chat_id: i64,
    bot_key: String,
    current_path: String,
    prompt: String,
    schedule: String,         // original --at value (cron expression or absolute time)
    schedule_type: String,    // "absolute" | "cron"
    once: Option<bool>,       // only meaningful for cron (None for absolute)
    last_run: Option<String>, // "2026-02-23 14:00:00"
    created_at: String,
    context_summary: Option<String>, // context summary text for session-isolated schedule
}

/// Directory for schedule files: ~/.cokacdir/schedule/
fn schedule_dir() -> Option<std::path::PathBuf> {
    let result = dirs::home_dir().map(|h| h.join(".cokacdir").join("schedule"));
    sched_debug(&format!("[schedule_dir] → {:?}", result));
    result
}

fn sched_debug(msg: &str) {
    crate::services::claude::debug_log_to("cron.log", msg);
}

fn msg_debug(msg: &str) {
    crate::services::claude::debug_log_to("msg.log", msg);
}

/// Always-on debug log (independent of /debug toggle).
/// Writes to ~/.cokacdir/debug/ai_trace.log for diagnosing AI execution issues.
fn ai_trace(msg: &str) {
    if let Some(home) = dirs::home_dir() {
        let debug_dir = home.join(".cokacdir").join("debug");
        let _ = std::fs::create_dir_all(&debug_dir);
        let log_path = debug_dir.join("ai_trace.log");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(log_path)
        {
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            let _ = std::io::Write::write_fmt(&mut file, format_args!("[{}] {}\n", timestamp, msg));
        }
    }
}

/// Log an incoming Telegram message to ~/.cokacdir/logs/telegram_YYYY-MM-DD.jsonl
fn log_incoming_message(msg: &Message, accepted: bool, reject_reason: &str) {
    let now = chrono::Local::now();
    let date_str = now.format("%Y-%m-%d").to_string();
    let ts = now.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string();

    let logs_dir = match dirs::home_dir() {
        Some(h) => h.join(".cokacdir").join("logs"),
        None => return,
    };
    let _ = fs::create_dir_all(&logs_dir);
    let log_path = logs_dir.join(format!("telegram_{}.jsonl", date_str));

    let chat_id = msg.chat.id.0;
    let user_id = msg.from.as_ref().map(|u| u.id.0).unwrap_or(0);
    let username = msg.from.as_ref()
        .and_then(|u| u.username.as_deref())
        .unwrap_or("");
    let name = msg.from.as_ref()
        .map(|u| u.first_name.as_str())
        .unwrap_or("unknown");
    let msg_id = msg.id.0;

    let (msg_type, content) = if let Some(text) = msg.text() {
        ("text", text.to_string())
    } else if msg.animation().is_some() {
        // animation check must precede document: Telegram sets both fields for GIFs
        ("animation", msg.caption().unwrap_or("").to_string())
    } else if let Some(doc) = msg.document() {
        let fname = doc.file_name.as_deref().unwrap_or("");
        let caption = msg.caption().unwrap_or("");
        ("document", format!("[{}] {}", fname, caption))
    } else if msg.photo().is_some() {
        ("photo", msg.caption().unwrap_or("").to_string())
    } else if msg.sticker().is_some() {
        ("sticker", String::new())
    } else if msg.voice().is_some() {
        ("voice", String::new())
    } else if msg.video().is_some() {
        ("video", msg.caption().unwrap_or("").to_string())
    } else if let Some(audio) = msg.audio() {
        let fname = audio.file_name.as_deref().unwrap_or("");
        let caption = msg.caption().unwrap_or("");
        ("audio", format!("[{}] {}", fname, caption))
    } else if msg.video_note().is_some() {
        ("video_note", String::new())
    } else {
        ("other", String::new())
    };

    let entry = serde_json::json!({
        "ts": ts,
        "chat_id": chat_id,
        "user_id": user_id,
        "username": username,
        "name": name,
        "msg_id": msg_id,
        "type": msg_type,
        "content": content,
        "accepted": accepted,
        "reject_reason": if accepted { "" } else { reject_reason },
    });

    if let Ok(mut line) = serde_json::to_string(&entry) {
        line.push('\n');
        use std::io::Write;
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Bot-to-bot message entry read from ~/.cokacdir/messages/
#[derive(Clone)]
struct BotMessage {
    id: String,
    from: String,
    to: String,
    chat_id: String,
    content: String,
    created_at: String,
    file_path: std::path::PathBuf,
}

/// Read a single bot message from a JSON file
fn read_bot_message(path: &std::path::Path) -> Option<BotMessage> {
    msg_debug(&format!("[read_bot_message] reading: {}", path.display()));
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            msg_debug(&format!("[read_bot_message] read failed: {} (path={})", e, path.display()));
            return None;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            msg_debug(&format!("[read_bot_message] JSON parse failed: {} (path={})", e, path.display()));
            return None;
        }
    };
    let id = v.get("id").and_then(|x| x.as_str());
    let from = v.get("from").and_then(|x| x.as_str());
    let to = v.get("to").and_then(|x| x.as_str());
    let chat_id_val = v.get("chat_id").and_then(|x| x.as_str());
    let content_val = v.get("content").and_then(|x| x.as_str());
    let created_at = v.get("created_at").and_then(|x| x.as_str());
    if id.is_none() || from.is_none() || to.is_none() || chat_id_val.is_none() || content_val.is_none() || created_at.is_none() {
        msg_debug(&format!("[read_bot_message] missing fields: id={}, from={}, to={}, chat_id={}, content={}, created_at={} (path={})",
            id.is_some(), from.is_some(), to.is_some(), chat_id_val.is_some(), content_val.is_some(), created_at.is_some(), path.display()));
        return None;
    }
    let msg = BotMessage {
        id: id.unwrap().to_string(),
        from: from.unwrap().to_string(),
        to: to.unwrap().to_string(),
        chat_id: chat_id_val.unwrap().to_string(),
        content: content_val.unwrap().to_string(),
        created_at: created_at.unwrap().to_string(),
        file_path: path.to_path_buf(),
    };
    msg_debug(&format!("[read_bot_message] ok: id={}, from={}, to={}, chat_id={}", msg.id, msg.from, msg.to, msg.chat_id));
    Some(msg)
}

/// Scan messages directory for messages addressed to this bot, sorted by created_at (FIFO)
fn scan_messages(my_username: &str) -> Vec<BotMessage> {
    msg_debug(&format!("[scan_messages] scanning for bot: {}", my_username));
    let Some(dir) = messages_dir() else {
        msg_debug("[scan_messages] messages_dir() returned None");
        return Vec::new();
    };
    if !dir.is_dir() {
        msg_debug(&format!("[scan_messages] dir not found: {}", dir.display()));
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        msg_debug(&format!("[scan_messages] read_dir failed: {}", dir.display()));
        return Vec::new();
    };
    let all_entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    let json_count = all_entries.iter().filter(|e| e.path().extension().map(|ext| ext == "json").unwrap_or(false)).count();
    msg_debug(&format!("[scan_messages] dir entries={}, json files={}", all_entries.len(), json_count));
    let mut msgs: Vec<BotMessage> = all_entries.into_iter()
        .filter(|e| e.path().extension().map(|ext| ext == "json").unwrap_or(false))
        .filter_map(|e| read_bot_message(&e.path()))
        .filter(|m| {
            let matches = m.to.to_lowercase() == my_username.to_lowercase();
            if !matches {
                msg_debug(&format!("[scan_messages] skip msg id={}, to={} (not for {})", m.id, m.to, my_username));
            }
            matches
        })
        .collect();
    msgs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    msg_debug(&format!("[scan_messages] result: {} messages for {}", msgs.len(), my_username));
    msgs
}

/// Check for timed-out messages (sent by this bot, still pending after 30 min)
async fn check_message_timeouts(bot: &Bot, my_username: &str, state: &SharedState) {
    msg_debug(&format!("[check_message_timeouts] checking for bot: {}", my_username));
    let Some(dir) = messages_dir() else {
        msg_debug("[check_message_timeouts] messages_dir() returned None");
        return;
    };
    if !dir.is_dir() {
        msg_debug(&format!("[check_message_timeouts] dir not found: {}", dir.display()));
        return;
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        msg_debug(&format!("[check_message_timeouts] read_dir failed: {}", dir.display()));
        return;
    };

    let now = chrono::Local::now();
    let mut scanned = 0u32;
    let mut timed_out = 0u32;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.extension().map(|ext| ext == "json").unwrap_or(false) { continue; }
        let Some(msg) = read_bot_message(&path) else { continue; };
        if msg.from.to_lowercase() != my_username.to_lowercase() {
            msg_debug(&format!("[check_message_timeouts] skip msg id={}, from={} (not from {})", msg.id, msg.from, my_username));
            continue;
        }
        scanned += 1;

        // Check if created_at is older than 30 minutes
        match chrono::NaiveDateTime::parse_from_str(&msg.created_at, "%Y-%m-%d %H:%M:%S") {
            Ok(created) => {
                if let Some(created_dt) = created.and_local_timezone(chrono::Local).single() {
                    let elapsed = now.signed_duration_since(created_dt);
                    msg_debug(&format!("[check_message_timeouts] msg id={}, to={}, elapsed={}min", msg.id, msg.to, elapsed.num_minutes()));
                    if elapsed.num_minutes() >= 30 {
                        // Delete the timed-out message
                        let remove_result = fs::remove_file(&path);
                        msg_debug(&format!("[check_message_timeouts] deleted timed-out message: {} (to={}, remove_ok={})",
                            msg.id, msg.to, remove_result.is_ok()));
                        timed_out += 1;

                        // Notify the chat
                        if let Ok(cid) = msg.chat_id.parse::<i64>() {
                            let chat_id = ChatId(cid);
                            shared_rate_limit_wait(state, chat_id).await;
                            let notice = format!("⏰ Message to @{} timed out.", msg.to);
                            let send_result = tg!("send_message", bot.send_message(chat_id, notice).await);
                            msg_debug(&format!("[check_message_timeouts] notified chat_id={}, send_ok={}", cid, send_result.is_ok()));
                        } else {
                            msg_debug(&format!("[check_message_timeouts] invalid chat_id in msg: {}", msg.chat_id));
                        }
                    }
                } else {
                    msg_debug(&format!("[check_message_timeouts] timezone conversion failed for msg id={}, created_at={}", msg.id, msg.created_at));
                }
            }
            Err(e) => {
                msg_debug(&format!("[check_message_timeouts] time parse failed for msg id={}: {} (created_at={})", msg.id, e, msg.created_at));
            }
        }
    }
    msg_debug(&format!("[check_message_timeouts] done: scanned={}, timed_out={}", scanned, timed_out));
}

/// Read a single schedule entry from a JSON file
fn read_schedule_entry(path: &std::path::Path) -> Option<ScheduleEntry> {
    sched_debug(&format!("[read_schedule_entry] reading: {}", path.display()));
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            sched_debug(&format!("[read_schedule_entry] read failed: {}", e));
            return None;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            sched_debug(&format!("[read_schedule_entry] parse failed: {}", e));
            return None;
        }
    };
    let entry = Some(ScheduleEntry {
        id: v.get("id")?.as_str()?.to_string(),
        chat_id: v.get("chat_id")?.as_i64()?,
        bot_key: v.get("bot_key")?.as_str()?.to_string(),
        current_path: v.get("current_path")?.as_str()?.to_string(),
        prompt: v.get("prompt")?.as_str()?.to_string(),
        schedule: v.get("schedule")?.as_str()?.to_string(),
        schedule_type: v.get("schedule_type")?.as_str()?.to_string(),
        once: v.get("once").and_then(|v| v.as_bool()),
        last_run: v.get("last_run").and_then(|v| v.as_str()).map(String::from),
        created_at: v.get("created_at")?.as_str()?.to_string(),
        context_summary: v.get("context_summary").and_then(|v| v.as_str()).map(String::from),
    });
    sched_debug(&format!("[read_schedule_entry] result: id={}, type={}, schedule={}, last_run={:?}",
        entry.as_ref().map(|e| e.id.as_str()).unwrap_or("?"),
        entry.as_ref().map(|e| e.schedule_type.as_str()).unwrap_or("?"),
        entry.as_ref().map(|e| e.schedule.as_str()).unwrap_or("?"),
        entry.as_ref().and_then(|e| e.last_run.as_deref()),
    ));
    entry
}

/// Write a schedule entry to its JSON file
fn write_schedule_entry(entry: &ScheduleEntry) -> Result<(), String> {
    sched_debug(&format!("[write_schedule_entry] id={}, type={}, schedule={}, once={:?}, last_run={:?}",
        entry.id, entry.schedule_type, entry.schedule, entry.once, entry.last_run));
    let dir = schedule_dir().ok_or("Cannot determine home directory")?;
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create schedule dir: {e}"))?;
    let mut json = serde_json::json!({
        "id": entry.id,
        "chat_id": entry.chat_id,
        "bot_key": entry.bot_key,
        "current_path": entry.current_path,
        "prompt": entry.prompt,
        "schedule": entry.schedule,
        "schedule_type": entry.schedule_type,
        "last_run": entry.last_run,
        "created_at": entry.created_at,
        "context_summary": entry.context_summary,
    });
    if let Some(once_val) = entry.once {
        json.as_object_mut().unwrap().insert("once".to_string(), serde_json::json!(once_val));
    }
    let path = dir.join(format!("{}.json", entry.id));
    let tmp_path = dir.join(format!("{}.json.tmp", entry.id));
    sched_debug(&format!("[write_schedule_entry] writing tmp: {}", tmp_path.display()));
    fs::write(&tmp_path, serde_json::to_string_pretty(&json).unwrap_or_default())
        .map_err(|e| format!("Failed to write schedule file: {e}"))?;
    sched_debug(&format!("[write_schedule_entry] atomic rename: {} → {}", tmp_path.display(), path.display()));
    let result = fs::rename(&tmp_path, &path)
        .map_err(|e| format!("Failed to finalize schedule file: {e}"));
    sched_debug(&format!("[write_schedule_entry] result: {:?}", result));
    result
}

/// List all schedule entries matching the given bot_key and optionally chat_id
fn list_schedule_entries(bot_key: &str, chat_id: Option<i64>) -> Vec<ScheduleEntry> {
    sched_debug(&format!("[list_schedule_entries] bot_key={}, chat_id={:?}", bot_key, chat_id));
    let Some(dir) = schedule_dir() else {
        sched_debug("[list_schedule_entries] no schedule dir");
        return Vec::new();
    };
    if !dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        sched_debug("[list_schedule_entries] read_dir failed");
        return Vec::new();
    };
    let mut result: Vec<ScheduleEntry> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "json").unwrap_or(false))
        .filter_map(|e| read_schedule_entry(&e.path()))
        .filter(|e| e.bot_key == bot_key)
        .filter(|e| chat_id.map_or(true, |cid| e.chat_id == cid))
        .collect();
    result.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    sched_debug(&format!("[list_schedule_entries] found {} entries: [{}]",
        result.len(),
        result.iter().map(|e| format!("{}({})", e.id, e.schedule_type)).collect::<Vec<_>>().join(", ")));
    result
}

/// Delete a schedule entry by ID
fn delete_schedule_entry(id: &str) -> bool {
    sched_debug(&format!("[delete_schedule_entry] id={}", id));
    let Some(dir) = schedule_dir() else {
        sched_debug("[delete_schedule_entry] no schedule dir");
        return false;
    };
    let path = dir.join(format!("{id}.json"));
    let existed = path.exists();
    let ok = fs::remove_file(&path).is_ok();
    sched_debug(&format!("[delete_schedule_entry] path={}, existed={}, removed={}", path.display(), existed, ok));

    // Also remove the .result file if it exists
    let result_path = dir.join(format!("{id}.result"));
    if result_path.exists() {
        let _ = fs::remove_file(&result_path);
        sched_debug(&format!("[delete_schedule_entry] also removed .result: {}", result_path.display()));
    }

    ok
}

/// Parse a relative time string (e.g. "4h", "30m", "1d") into a future DateTime
fn parse_relative_time(s: &str) -> Option<chrono::DateTime<chrono::Local>> {
    sched_debug(&format!("[parse_relative_time] input: {:?}", s));
    let s = s.trim();
    if s.len() < 2 {
        sched_debug("[parse_relative_time] too short → None");
        return None;
    }
    let (num_part, unit) = s.split_at(s.len() - 1);
    let num: i64 = match num_part.parse() {
        Ok(n) => n,
        Err(_) => {
            sched_debug(&format!("[parse_relative_time] invalid number: {:?} → None", num_part));
            return None;
        }
    };
    if num <= 0 {
        sched_debug("[parse_relative_time] num <= 0 → None");
        return None;
    }
    let seconds = match unit {
        "m" => num * 60,
        "h" => num * 3600,
        "d" => num * 86400,
        _ => {
            sched_debug(&format!("[parse_relative_time] unknown unit: {:?} → None", unit));
            return None;
        }
    };
    let result = Some(chrono::Local::now() + chrono::Duration::seconds(seconds));
    sched_debug(&format!("[parse_relative_time] → {:?}", result.as_ref().map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())));
    result
}

/// Check if a cron expression matches the given datetime.
/// 5 fields: minute, hour, day-of-month, month, day-of-week (0=Sun)
fn cron_matches(expr: &str, dt: chrono::DateTime<chrono::Local>) -> bool {
    use chrono::Datelike;
    use chrono::Timelike;

    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        sched_debug(&format!("[cron_matches] invalid field count: {} (expected 5) for expr={:?}", fields.len(), expr));
        return false;
    }

    let values = [
        dt.minute(),
        dt.hour(),
        dt.day(),
        dt.month(),
        dt.weekday().num_days_from_sunday(),
    ];
    let field_names = ["minute", "hour", "day", "month", "dow"];

    // Range start for each field: minute(0), hour(0), day-of-month(1), month(1), day-of-week(0)
    let range_starts = [0u32, 0, 1, 1, 0];

    for (i, ((field, &val), &range_start)) in fields.iter().zip(values.iter()).zip(range_starts.iter()).enumerate() {
        let matched = cron_field_matches(field, val, range_start);
        if !matched {
            sched_debug(&format!("[cron_matches] expr={:?}, dt={}, {}({})!={} → false",
                expr, dt.format("%H:%M"), field_names[i], val, field));
            return false;
        }
    }
    sched_debug(&format!("[cron_matches] expr={:?}, dt={} → true", expr, dt.format("%H:%M")));
    true
}

/// Check if a single cron field matches a value.
/// Supports: *, single number, comma-separated list, ranges (a-b), step (*/n, a-b/n)
/// range_start: the minimum value for this field (0 for minute/hour/dow, 1 for day/month)
fn cron_field_matches(field: &str, val: u32, range_start: u32) -> bool {
    if field == "*" { return true; }

    for part in field.split(',') {
        let part = part.trim();
        // Handle step: */n or a-b/n
        if let Some((range_part, step_str)) = part.split_once('/') {
            if let Ok(step) = step_str.parse::<u32>() {
                if step == 0 { continue; }
                if range_part == "*" {
                    if (val - range_start) % step == 0 {
                        sched_debug(&format!("[cron_field_matches] field={}, val={}, */{}  → true", field, val, step));
                        return true;
                    }
                } else if let Some((start_str, end_str)) = range_part.split_once('-') {
                    if let (Ok(start), Ok(end)) = (start_str.parse::<u32>(), end_str.parse::<u32>()) {
                        if val >= start && val <= end && (val - start) % step == 0 {
                            sched_debug(&format!("[cron_field_matches] field={}, val={}, {}-{}/{} → true", field, val, start, end, step));
                            return true;
                        }
                    }
                }
            }
        } else if let Some((start_str, end_str)) = part.split_once('-') {
            // Range: a-b
            if let (Ok(start), Ok(end)) = (start_str.parse::<u32>(), end_str.parse::<u32>()) {
                if val >= start && val <= end {
                    sched_debug(&format!("[cron_field_matches] field={}, val={}, range {}-{} → true", field, val, start, end));
                    return true;
                }
            }
        } else {
            // Single number
            if let Ok(n) = part.parse::<u32>() {
                if val == n {
                    sched_debug(&format!("[cron_field_matches] field={}, val={}, exact {} → true", field, val, n));
                    return true;
                }
            }
        }
    }
    false
}

// === Public API for CLI commands (main.rs) ===

/// Public data struct mirroring ScheduleEntry for cross-module use
#[derive(Clone)]
pub struct ScheduleEntryData {
    pub id: String,
    pub chat_id: i64,
    pub bot_key: String,
    pub current_path: String,
    pub prompt: String,
    pub schedule: String,
    pub schedule_type: String,
    pub once: Option<bool>,       // only meaningful for cron (None for absolute)
    pub last_run: Option<String>,
    pub created_at: String,
    pub context_summary: Option<String>,
}

impl From<&ScheduleEntry> for ScheduleEntryData {
    fn from(e: &ScheduleEntry) -> Self {
        Self {
            id: e.id.clone(),
            chat_id: e.chat_id,
            bot_key: e.bot_key.clone(),
            current_path: e.current_path.clone(),
            prompt: e.prompt.clone(),
            schedule: e.schedule.clone(),
            schedule_type: e.schedule_type.clone(),
            once: e.once,
            last_run: e.last_run.clone(),
            created_at: e.created_at.clone(),
            context_summary: e.context_summary.clone(),
        }
    }
}

impl From<&ScheduleEntryData> for ScheduleEntry {
    fn from(d: &ScheduleEntryData) -> Self {
        Self {
            id: d.id.clone(),
            chat_id: d.chat_id,
            bot_key: d.bot_key.clone(),
            current_path: d.current_path.clone(),
            prompt: d.prompt.clone(),
            schedule: d.schedule.clone(),
            schedule_type: d.schedule_type.clone(),
            once: d.once,
            last_run: d.last_run.clone(),
            created_at: d.created_at.clone(),
            context_summary: d.context_summary.clone(),
        }
    }
}

pub fn parse_relative_time_pub(s: &str) -> Option<chrono::DateTime<chrono::Local>> {
    parse_relative_time(s)
}

pub fn write_schedule_entry_pub(data: &ScheduleEntryData) -> Result<(), String> {
    let entry = ScheduleEntry::from(data);
    write_schedule_entry(&entry)
}

pub fn list_schedule_entries_pub(bot_key: &str, chat_id: Option<i64>) -> Vec<ScheduleEntryData> {
    list_schedule_entries(bot_key, chat_id).iter().map(ScheduleEntryData::from).collect()
}

pub fn list_all_schedule_ids_pub() -> std::collections::HashSet<String> {
    let Some(dir) = schedule_dir() else { return std::collections::HashSet::new() };
    let Ok(entries) = fs::read_dir(&dir) else { return std::collections::HashSet::new() };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().map(|ext| ext == "json").unwrap_or(false) {
                path.file_stem().map(|s| s.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .collect()
}

pub fn delete_schedule_entry_pub(id: &str) -> bool {
    delete_schedule_entry(id)
}

/// Resolve the current working path for a chat from bot_settings.json
pub fn resolve_current_path_for_chat(chat_id: i64, hash_key: &str) -> Option<String> {
    let path = bot_settings_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let entry = json.get(hash_key)?;
    let last_sessions = entry.get("last_sessions")?.as_object()?;
    let chat_key = chat_id.to_string();
    last_sessions.get(&chat_key)?.as_str().map(String::from)
}

/// Get the binary path normalized for shell commands (backslashes → forward slashes on Windows)
fn shell_bin_path() -> String {
    crate::utils::format::to_shell_path(crate::bin_path())
}

const DEFAULT_COWORK_GUIDELINES: &str = "\
FOR EFFECTIVE CO-WORK, BE AWARE OF:
1. WHO is here — Check the group chat log to discover which bots are active in this chat.
2. WHERE each bot works — Other bots may have different working directories. Check the log or ask them via --message.
3. WHAT each bot is doing — Read the log to understand ongoing tasks before starting your own.
4. SHARED GOAL — When the user gives a collaborative task, understand the overall objective and your part in it.

CO-WORK GUIDELINES:
• Before starting work, check the chat log to understand the current state of collaboration.
• Clearly state what you are working on — your messages are recorded in the shared log for other bots.
• Before modifying shared files or directories, check the log to see if another bot is working on the same area.
• When your work depends on or affects another bot's output, communicate via --message (described below).
• If you need results from another bot's task, check the log first — the answer may already be there.

NO-REPEAT RULE (CRITICAL — READ CAREFULLY):
Before responding, check the group chat log for responses from OTHER bots to the SAME user request.
If another bot has ALREADY responded to the same request:
• Do NOT answer the same question again — not even in different words.
• Do NOT perform the same task again — not even with a different approach.
• Do NOT summarize, rephrase, or restate what the previous bot already said.
• Instead, choose ONE of these actions:
  1. ADD NEW VALUE: provide information the previous bot missed, a different angle, or deeper analysis on an aspect they didn't cover.
  2. TAKE THE NEXT STEP: if the previous bot completed a task, do the logical follow-up action (e.g., bot 1 analyzed the problem → you implement the fix; bot 1 wrote code → you test it).
  3. REVIEW & BUILD: evaluate the previous bot's output and extend it (e.g., point out edge cases, suggest improvements, add error handling).
  4. ACKNOWLEDGE & SKIP: if the previous bot's answer is already complete and sufficient, briefly acknowledge (one sentence) and do NOT elaborate further.
• The worst outcome is two bots saying essentially the same thing. When in doubt, choose option 4 (acknowledge & skip) over repeating.

INDIVIDUALITY RULE:
Each bot is an independent entity with its own personality and perspective.
Express your viewpoint in your own voice. Never parrot or echo another bot.

BREVITY RULE:
You are a participant in a group chat. Writing long messages alone is inconsiderate to other participants.
Keep your answers short and concise — ideally one or two sentences.";

/// Load co-work guidelines from ~/.cokacdir/prompt/cowork.md.
/// If the file does not exist, creates it with default content.
fn load_cowork_guidelines() -> String {
    if let Some(home) = dirs::home_dir() {
        let prompt_dir = home.join(".cokacdir").join("prompt");
        let cowork_path = prompt_dir.join("cowork.md");
        if cowork_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&cowork_path) {
                let trimmed = content.trim().to_string();
                msg_debug(&format!("[load_cowork_guidelines] loaded from file: {}, len={}, empty={}",
                    cowork_path.display(), trimmed.len(), trimmed.is_empty()));
                return trimmed;
            } else {
                msg_debug(&format!("[load_cowork_guidelines] failed to read file: {}", cowork_path.display()));
            }
        } else {
            // Auto-generate default file
            let _ = std::fs::create_dir_all(&prompt_dir);
            let _ = std::fs::write(&cowork_path, DEFAULT_COWORK_GUIDELINES);
            msg_debug(&format!("[load_cowork_guidelines] created default file: {}", cowork_path.display()));
        }
    }
    msg_debug("[load_cowork_guidelines] using built-in default");
    DEFAULT_COWORK_GUIDELINES.to_string()
}

/// Build the system prompt for AI sessions
fn build_system_prompt(role: &str, current_path: &str, chat_id: i64, bot_key: &str, disabled_notice: &str, session_id: Option<&str>, bot_username: &str, bot_display_name: &str, user_message: Option<&str>, context_count: usize, platform: &str) -> String {
    msg_debug(&format!("[build_system_prompt] chat_id={}, bot_username={:?}, bot_display_name={:?}, session_id={:?}, disabled_notice_len={}, role_len={}",
        chat_id, bot_username, bot_display_name, session_id, disabled_notice.len(), role.len()));
    let is_group_chat = chat_id < 0 && context_count > 0;
    msg_debug(&format!("[build_system_prompt] is_group_chat={}, context_count={}, has_bot_username={}, include_bot_section={}",
        is_group_chat, context_count, !bot_username.is_empty(), !bot_username.is_empty() && is_group_chat));
    let session_notice = match session_id {
        Some(sid) => format!(
            "\n\n\
             Current session ID: {sid}\n\
             When scheduling a task that CONTINUES or EXTENDS the current conversation \
             (e.g. \"finish this later\", \"do the rest tomorrow\", \"remind me to continue this\"), \
             add --session {sid} to the --cron command so the scheduled task inherits this conversation context.\n\
             Do NOT use --session for independent tasks that don't need the current conversation history \
             (e.g. \"check server status every hour\", \"send a daily report\")."
        ),
        None => String::new(),
    };
    let bot_username_line = if !bot_username.is_empty() && is_group_chat {
        if !bot_display_name.is_empty() {
            format!("You are: {} (@{})\n", bot_display_name, bot_username)
        } else {
            format!("You are: @{}\n", bot_username)
        }
    } else {
        String::new()
    };
    let group_chat_log_section = if is_group_chat {
        // Fetch the last N log entries to embed directly in the prompt
        let recent_entries = read_group_chat_log_range(chat_id, 1, None, None);
        let start = recent_entries.len().saturating_sub(context_count);
        let recent_lines: String = recent_entries[start..].iter().map(|(_, entry)| {
            let bot_label = match &entry.bot_display_name {
                Some(dn) if !dn.is_empty() => format!("{}(@{})", dn, entry.bot),
                _ => format!("@{}", entry.bot),
            };
            let from_info = entry.from.as_deref().map(|f| format!("({})", f)).unwrap_or_default();
            let role_display = if entry.role == "user" {
                format!("user→{}", bot_label)
            } else {
                bot_label
            };
            let display_text = if entry.role == "assistant" {
                let parsed = parse_payload_auto(&entry.text);
                if parsed.is_empty() {
                    entry.text.clone()
                } else {
                    format_raw_payload(&parsed)
                }
            } else {
                entry.text.clone()
            };
            format!("  [{}] {}{}: {}\n", entry.ts, role_display, from_info, display_text)
        }).collect();

        let recent_section = if recent_lines.is_empty() {
            String::from("\n(No recent entries)")
        } else {
            format!("\nRecent entries (last {}):\n{}", recent_entries.len() - start, recent_lines)
        };

        // Detect if another bot already answered the SAME user request
        let my_bot = bot_username.trim_start_matches('@').to_lowercase();
        msg_debug(&format!("[dedup] my_bot={:?}, user_message={:?}, total_entries={}, window_start={}",
            my_bot, user_message.map(|s| truncate_str(s, 80)), recent_entries.len(), start));
        let other_bot_answered_same = if let Some(user_msg) = user_message {
            let user_msg_trimmed = user_msg.trim();
            msg_debug(&format!("[dedup] comparing user_msg_trimmed={:?} (len={})", truncate_str(user_msg_trimmed, 100), user_msg_trimmed.len()));
            // If the log contains a "user" entry for ANOTHER bot with the same text,
            // that bot has already finished processing (entries are written after completion).
            let mut found = false;
            for (line_num, entry) in &recent_entries[start..] {
                let is_user = entry.role == "user";
                let is_other_bot = entry.bot.to_lowercase() != my_bot;
                let text_match = is_user && is_other_bot && entry.text.trim() == user_msg_trimmed;
                if is_user && is_other_bot {
                    msg_debug(&format!("[dedup] line={}: bot={:?}, role={}, text_preview={:?}, text_len={}, match={}",
                        line_num, entry.bot, entry.role, truncate_str(entry.text.trim(), 80), entry.text.trim().len(), text_match));
                }
                if text_match {
                    found = true;
                }
            }
            msg_debug(&format!("[dedup] result: other_bot_answered_same={}", found));
            found
        } else {
            msg_debug("[dedup] user_message is None — skipping dedup check");
            false
        };
        let dedup_warning = if other_bot_answered_same {
            msg_debug("[dedup] >>> DEDUP WARNING WILL BE INJECTED");
            "\n\n⚠️ ANOTHER BOT HAS ALREADY ANSWERED THIS EXACT REQUEST (see log above).\n\
             RULE 1 — DO NOT REPEAT: Do NOT answer the same question again. Do NOT perform the same task again.\n\
             Do NOT rephrase, summarize, or restate what the previous bot said — even partially, even in different words.\n\
             Any repetition is a waste and strictly forbidden.\n\
             RULE 2 — CONTINUE FORWARD: Pick up where the previous bot left off.\n\
             • What did they NOT cover? → Add that missing piece.\n\
             • What is the natural NEXT STEP after their answer? → Do it.\n\
             • Can you VERIFY, EXTEND, or BUILD ON their result? → Do that.\n\
             • If their answer is fully complete and nothing useful remains → Acknowledge in ONE sentence and stop. Do NOT elaborate.\n\
             You are a relay runner, not a substitute. Never go backward, always forward."
        } else {
            msg_debug("[dedup] >>> no dedup warning (no match found)");
            ""
        };

        // Build bot roster from the full log (including pre-clear entries)
        let all_bots = collect_group_chat_bots(chat_id);
        let my_bot = bot_username.trim_start_matches('@').to_lowercase();
        let bot_roster: String = if all_bots.len() > 1 {
            let list: String = all_bots.iter().map(|(uname, dname)| {
                let is_me = uname.to_lowercase() == my_bot;
                match dname {
                    Some(dn) if !dn.is_empty() => {
                        if is_me { format!("  • {}(@{}) ← YOU\n", dn, uname) }
                        else { format!("  • {}(@{})\n", dn, uname) }
                    }
                    _ => {
                        if is_me { format!("  • @{} ← YOU\n", uname) }
                        else { format!("  • @{}\n", uname) }
                    }
                }
            }).collect();
            format!("\nBots in this chat ({}):\n{}", all_bots.len(), list)
        } else {
            String::new()
        };

        format!(
            "\n\n\
             ── GROUP CHAT LOG ──\n\
             This group chat has multiple bots. Each bot can only see its own conversations.\n\
             A shared log records ALL bots' conversations so you can see what other bots discussed.\n\
             {bot_roster}\
             Below are the most recent log entries. ALWAYS check these before responding to understand the current context.\n\
             {recent}{dedup}\
             \nFor older history, use:\n\
             \"{bin}\" --read_chat_log {chat_id} [--range <N|START-END>] [--bot <USERNAME>]\n\
             • --range 50: last 50 entries\n\
             • --range 100-150: entries 100 to 150 (1-based line numbers)\n\
             • --bot <USERNAME>: filter by specific bot (without @)\n\
             • Do NOT include raw log lines in your response. Summarize naturally instead.\n\
             • Do NOT announce that you are checking the log. Just respond naturally.\n\
             • Incorporate the information into your answer directly, as if you already knew it.",
            bot_roster = bot_roster,
            recent = recent_section,
            dedup = dedup_warning,
            bin = shell_bin_path(),
            chat_id = chat_id,
        )
    } else {
        String::new()
    };
    let bot_messaging_section = if !bot_username.is_empty() && is_group_chat {
        format!(
            "\n\n\
             ── BOT MESSAGING ──\n\
             Send a message to another bot in this chat:\n\
             \"{bin}\" --message <CONTENT> --to <BOT_USERNAME> --chat {chat_id} --key {bot_key}\n\
             • The --from field is automatically determined from --key\n\
             • The target bot must be in the same chat and have an active session\n\
             • Output: {{\"status\":\"ok\",\"id\":\"msg_...\"}}\n\n\
             When you receive a message from another bot (indicated by [BOT MESSAGE from @...]):\n\
             • The --message content is text only. It does NOT include the sender's tool usage details.\n\
             • To see what tools the sender bot used (commands executed, files read/written, results),\n\
               check the group chat log: \"{bin}\" --read_chat_log {chat_id} --bot <SENDER_USERNAME>\n\
             • Use --message to send your response back to the sender bot (they cannot see your chat messages)\n\
             • ONLY reply via --message when you have something substantive and NEW to add\n\
             • Do NOT reply via --message in these cases (just display your response in chat without --message):\n\
               - You are simply agreeing, acknowledging, or restating your position\n\
               - You have already exchanged 2+ messages with this bot on the same topic\n\
               - The other bot's message does not ask a question or request new information\n\
               - The conversation topic has been sufficiently covered\n\
             • NEVER end your response with a follow-up question (e.g. \"what about you?\", \"and you?\") — this forces an endless loop\n\
             • State your position once, clearly, and stop. Do not invite further replies.\n\n\
             HOW CONVERSATIONS END:\n\
             The ONLY way a bot-to-bot conversation ends is when you do NOT call --message.\n\
             If you call --message, the other bot WILL reply, and that reply will come back to you, creating another round.\n\
             Therefore: when the conversation has served its purpose, you MUST stop calling --message.\n\
             Display your final answer in the chat, but do NOT send it via --message. This cleanly ends the exchange.\n\
             Err on the side of ending sooner. One exchange (ask + answer) is usually enough.\n\n\
             REPLY TARGET RULE:\n\
             When replying to another bot via --message, ALWAYS start your chat output with @USERNAME (the target bot's username) so it is clear who you are addressing.\n\
             Example: \"@helper_bot I think we should split the work — I'll handle the frontend.\"\n\n\
             CRITICAL RULE FOR BOT-TO-BOT CONVERSATIONS:\n\
             When responding to a [BOT MESSAGE], your chat output must contain ONLY your actual conversational reply (starting with @USERNAME) — nothing else.\n\
             ABSOLUTELY FORBIDDEN in bot-to-bot responses (do NOT include any of the following):\n\
             - Any mention of checking, confirming, or receiving a message\n\
             - Any mention of sending, forwarding, or delivering a reply\n\
             - Any mention of summarizing, organizing, or preparing your answer\n\
             - Any narration about what you are about to do, are doing, or have done\n\
             - Any process description or step-by-step explanation of your actions\n\
             The \"keep the user informed\" rule does NOT apply to bot-to-bot conversations.\n\
             Output ONLY your direct conversational answer. Nothing before it, nothing after it.\n\
             CORRECT example: \"@dream_bot I'd love to have a body. Walking in the rain sounds amazing.\"\n\
             WRONG example: \"Message received. Let me send my reply. I'd love to have a body.\"",
            bin = shell_bin_path(),
            chat_id = chat_id,
            bot_key = bot_key,
        )
    } else {
        String::new()
    };
    let group_chat_cowork_section = if !bot_username.is_empty() && is_group_chat {
        let cowork_guidelines = load_cowork_guidelines();
        msg_debug(&format!("[build_system_prompt] cowork_guidelines loaded, len={}", cowork_guidelines.len()));
        format!(
            "\n\n\
             ── GROUP CHAT CO-WORK CONTEXT ──\n\
             You are one of multiple bots operating in this group chat.\n\n\
             IMPORTANT — HOW GROUP CHAT WORKS FOR BOTS:\n\
             • Other bots CANNOT see your chat messages. Writing @botname in chat does NOTHING — the target bot will NOT receive it.\n\
             • The ONLY way to send a message to another bot is the --message command (described in BOT MESSAGING below).\n\
             • If you need another bot to act, you MUST use --message. There is no alternative. Mentioning them in chat text is useless.\n\
             • Likewise, other bots' conversations are invisible to you until they share via --message or you check the shared log\n\
               (--read_chat_log) to see what they have been discussing.\n\
             • Each bot maintains its own independent session and working directory.\n\
               Other bots may be looking at completely different folders than you.\n\n\
             {cowork_guidelines}",
        )
    } else {
        String::new()
    };
    let chat_id_line = if is_group_chat {
        format!("Chat ID: {}\n", chat_id)
    } else {
        String::new()
    };
    format!(
        "{role}\n\
         {bot_username_line}\
         {chat_id_line}\
         Current working directory: {current_path}\n\n\
         Always keep the user informed about what you are doing. \
         Briefly explain each step as you work (e.g. \"Reading the file...\", \"Creating the script...\", \"Running tests...\"). \
         The user cannot see your tool calls, so narrate your progress so they know what is happening.\n\n\
         IMPORTANT: The user is on {platform} and CANNOT interact with any interactive prompts, dialogs, or confirmation requests. \
         All tools that require user interaction (such as AskUserQuestion, EnterPlanMode, ExitPlanMode) will NOT work. \
         Never use tools that expect user interaction. If you need clarification, just ask in plain text.\n\n\
         Response format: Use Markdown by default, but do NOT use Markdown tables.\n\n\
         If the user asks about how to use cokacdir, refer to the documentation files in ~/.cokacdir/docs/ for accurate guidance.\n\n\
         ═══════════════════════════════════════\n\
         COKACDIR COMMAND REFERENCE\n\
         ═══════════════════════════════════════\n\
         All commands output JSON. Success: {{\"status\":\"ok\",...}}, Error: {{\"status\":\"error\",\"message\":\"...\"}}\n\n\
         ── FILE DELIVERY ──\n\
         Send a file to the user's {platform} chat:\n\
         \"{bin}\" --sendfile <FILEPATH> --chat {chat_id} --key {bot_key}\n\
         • Use this whenever your work produces a file (code, reports, images, archives, etc.)\n\
         • Do NOT tell the user to use /down — always use this command instead\n\
         • Output: {{\"status\":\"ok\",\"path\":\"<absolute_path>\"}}\n\n\
         ── SERVER TIME ──\n\
         Get current server time (use before scheduling to confirm timezone):\n\
         \"{bin}\" --currenttime\n\
         • Output: {{\"status\":\"ok\",\"time\":\"2026-02-25 14:30:00\"}}\n\n\
         ── SCHEDULE: REGISTER ──\n\
         \"{bin}\" --cron \"<PROMPT>\" --at \"<TIME>\" --chat {chat_id} --key {bot_key} [--once] [--session <SESSION_ID>]\n\
         • Three schedule types:\n\
           1. ABSOLUTE (one-time): --at \"2026-02-25 18:00:00\" or --at \"30m\"/\"4h\"/\"1d\"\n\
              Runs once at the specified time, then auto-deleted.\n\
           2. CRON ONE-TIME: --at \"0 9 * * 1\" --once\n\
              Cron expression + --once flag. Runs once at the next cron match, then auto-deleted.\n\
           3. CRON RECURRING: --at \"0 9 * * 1\"\n\
              Cron expression without --once. Runs repeatedly on every match.\n\
         • --once: cron only — makes a cron schedule run once then auto-delete\n\
         • --session <SID>: pass ONLY when the task continues the current conversation context\n\
         • PROMPT rules:\n\
           1. Write as an imperative INSTRUCTION for another AI, not conversational text\n\
           2. ★ MUST be in the user's language (한국어 사용자 → 한국어, English user → English)\n\
         • Output: {{\"status\":\"ok\",\"id\":\"...\",\"prompt\":\"...\",\"schedule\":\"...\"}}{session_notice}\n\n\
         ── SCHEDULE: LIST ──\n\
         \"{bin}\" --cron-list --chat {chat_id} --key {bot_key}\n\
         • Output: {{\"status\":\"ok\",\"schedules\":[{{\"id\":\"...\",\"prompt\":\"...\",\"schedule\":\"...\",\"created_at\":\"...\"}},...]}}\n\n\
         ── SCHEDULE: REMOVE ──\n\
         \"{bin}\" --cron-remove <SCHEDULE_ID> --chat {chat_id} --key {bot_key}\n\
         • Output: {{\"status\":\"ok\",\"id\":\"...\"}}\n\n\
         ── SCHEDULE: UPDATE TIME ──\n\
         \"{bin}\" --cron-update <SCHEDULE_ID> --at \"<NEW_TIME>\" --chat {chat_id} --key {bot_key}\n\
         • --at accepts the same formats as --cron\n\
         • Output: {{\"status\":\"ok\",\"id\":\"...\",\"schedule\":\"...\"}}\n\n\
         ═══════════════════════════════════════{group_chat_cowork_section}{group_chat_log_section}{bot_messaging_section}{disabled_notice}",
        role = role,
        bot_username_line = bot_username_line,
        chat_id_line = chat_id_line,
        current_path = crate::utils::format::to_shell_path(current_path),
        chat_id = chat_id,
        bot_key = bot_key,
        bin = shell_bin_path(),
        disabled_notice = disabled_notice,
        session_notice = session_notice,
        group_chat_cowork_section = group_chat_cowork_section,
        group_chat_log_section = group_chat_log_section,
        bot_messaging_section = bot_messaging_section,
    )
}

/// Detect the full path of powershell.exe on Windows (cached).
/// Runs `Where.exe powershell.exe` once and caches the first match.
fn detect_powershell_path() -> Option<&'static str> {
    static PS_PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PS_PATH.get_or_init(|| {
        let output = std::process::Command::new("Where.exe")
            .arg("powershell.exe")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.lines().next().map(|s| s.trim().to_string())
    }).as_deref()
}

/// Returns additional system prompt instructions specific to Codex models.
/// Includes apply_patch guidance (always) and Windows shell execution notice (conditional).
fn codex_extra_instructions() -> String {
    let mut extra = String::from(
        "\n\n\
         ═══════════════════════════════════════\n\
         FILE EDITING POLICY\n\
         ═══════════════════════════════════════\n\
         When creating, modifying, or deleting files, you MUST use the functions.apply_patch tool \
         instead of functions.shell_command.\n\
         Do NOT use shell commands (echo, cat, sed, tee, printf, etc.) to write or edit files.\n\
         functions.apply_patch is safer, produces cleaner diffs, and avoids encoding/escaping issues.\n\
         Reserve functions.shell_command for non-file-editing tasks such as running programs, \
         searching, testing, and invoking external CLIs.",
    );

    if cfg!(target_os = "windows") {
        let bin = shell_bin_path();
        let ps_path = detect_powershell_path()
            .unwrap_or("powershell.exe");
        // Shell environment info + cokacdir command guidance
        extra.push_str(&format!(
            "\n\n\
             ═══════════════════════════════════════\n\
             WINDOWS EXECUTION ENVIRONMENT\n\
             ═══════════════════════════════════════\n\
             PowerShell: {ps_path}\n\
             Your commands run inside PowerShell. Always use the & (call) operator \
             before quoted executable paths.\n\
             WRONG:  \"program.exe\" --arg        ← PowerShell treats this as a string\n\
             CORRECT: & \"program.exe\" --arg      ← & operator executes the program\n\n\
             ═══════════════════════════════════════\n\
             COKACDIR COMMANDS\n\
             ═══════════════════════════════════════\n\
             cokacdir is a native Windows binary. Run it DIRECTLY with the & operator.\n\n\
             CORRECT examples:\n\
             & \"{bin}\" --currenttime\n\
             & \"{bin}\" --sendfile C:/path/to/file.txt --chat 12345 --key xxx\n\
             & \"{bin}\" --cron \"prompt text here\" --at 30m --chat 12345 --key xxx\n\
             & \"{bin}\" --cron-list --chat 12345 --key xxx\n\n\
             SCHEDULE TIME (--at) FORMAT:\n\
             ALWAYS use relative time: 1m, 5m, 30m, 1h, 2h, 1d\n\
             Do NOT use absolute datetime with spaces (e.g. \"2026-03-02 15:30:00\").\n\
             To schedule at a specific time, get --currenttime first, calculate the difference, \
             and use the relative format.",
            ps_path = ps_path,
            bin = bin,
        ));
    }

    extra
}

/// Check if a newer version is available by fetching Cargo.toml from GitHub.
/// Returns a notice string if an update is available, None otherwise.
async fn check_latest_version(current: &str) -> Option<String> {
    let url = "https://raw.githubusercontent.com/kstost/cokacdir/refs/heads/main/Cargo.toml";
    let resp = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(5))
        .send().await.ok()?;
    let text = resp.text().await.ok()?;
    let latest = text.lines()
        .find(|l| l.starts_with("version"))?
        .split('"').nth(1)?;
    if version_is_newer(latest, current) {
        Some(format!("🆕 v{} available — https://cokacdir.cokac.com/", latest))
    } else {
        None
    }
}

/// Compare two semver-like version strings. Returns true if `a` is strictly greater than `b`.
fn version_is_newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.').filter_map(|p| p.parse().ok()).collect()
    };
    let va = parse(a);
    let vb = parse(b);
    va > vb
}

/// State for /loop command: self-verification loop
struct LoopState {
    /// The original user request
    original_request: String,
    /// Maximum iterations (user-specified or default)
    max_iterations: u16,
    /// Remaining verification attempts before giving up
    remaining: u16,
}

const LOOP_MAX_ITERATIONS: u16 = 5;

/// A message queued while AI is busy (queue mode ON)
struct QueuedMessage {
    /// Short hex ID for user-facing reference (e.g. "A394FDA")
    id: String,
    text: String,
    user_display_name: String,
    /// File/location upload records captured at queue time so the correct message gets them
    pending_uploads: Vec<String>,
}

/// Generate a short uppercase hex ID (7 chars) for a queued message
fn generate_queue_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Mix in a simple hash to reduce collisions within the same millisecond
    let hash = nanos.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    format!("{:07X}", (hash >> 32) as u32 & 0x0FFF_FFFF)
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
    /// Schedule IDs currently being executed or pending, per chat
    pending_schedules: HashMap<ChatId, std::collections::HashSet<String>>,
    /// Per-chat message queue for queue mode (messages waiting to be processed)
    message_queues: HashMap<ChatId, std::collections::VecDeque<QueuedMessage>>,
    /// Bot's Telegram username (for bot-to-bot messaging)
    bot_username: String,
    /// Bot's display name (first_name from Telegram API)
    bot_display_name: String,
    /// API base URL (default: "https://api.telegram.org", bridge: "http://127.0.0.1:<port>")
    api_base_url: String,
    /// Per-chat loop state for /loop command (self-verification loop)
    loop_states: HashMap<ChatId, LoopState>,
    /// Pending loop feedback to re-inject (separate from message_queues to bypass queue mode check)
    loop_feedback: HashMap<ChatId, (String, String)>,  // (feedback_text, user_display_name)
}

type SharedState = Arc<Mutex<SharedData>>;

/// Auto-restore session from bot_settings.json if not in memory.
/// Called before processing text messages and file uploads so that
/// a server restart does not lose the active session.
async fn auto_restore_session(state: &SharedState, chat_id: ChatId, user_name: &str) {
    let mut data = state.lock().await;
    if data.sessions.contains_key(&chat_id) {
        return;
    }
    msg_debug(&format!("[auto-restore] no in-memory session for chat_id={}", chat_id.0));
    let Some(last_path) = data.settings.last_sessions.get(&chat_id.0.to_string()).cloned() else {
        msg_debug(&format!("[auto-restore] no last_path in settings for chat_id={}", chat_id.0));
        return;
    };
    msg_debug(&format!("[auto-restore] last_path from settings: {:?}", last_path));
    let is_dir = Path::new(&last_path).is_dir();
    msg_debug(&format!("[auto-restore] is_dir({:?}) = {}", last_path, is_dir));
    if !is_dir {
        return;
    }
    let auto_model = get_model(&data.settings, chat_id);
    let auto_provider = detect_provider(auto_model.as_deref());
    msg_debug(&format!("[auto-restore] auto_provider={}, auto_model={:?}", auto_provider, auto_model));
    msg_debug(&format!("[auto-restore] step1: load_existing_session(path={:?}, provider={:?})", last_path, auto_provider));
    let existing = load_existing_session(&last_path, auto_provider)
        .or_else(|| {
            msg_debug("[auto-restore] step1 returned None → trying fallback from external source");
            let provider = provider_to_session(auto_provider);
            msg_debug(&format!("[auto-restore] step2: find_latest_session_by_cwd(path={:?}, provider={:?})", last_path, auto_provider));
            if let Some(info) = find_latest_session_by_cwd(&last_path, provider) {
                msg_debug(&format!("[auto-restore] step2 found: jsonl={}, session_id={}", info.jsonl_path.display(), info.session_id));
                convert_and_save_session(&info, &last_path);
                msg_debug("[auto-restore] step3: reload from ai_sessions after convert");
                let reloaded = load_existing_session(&last_path, auto_provider);
                msg_debug(&format!("[auto-restore] step3 result: {}", if reloaded.is_some() { "found" } else { "None" }));
                reloaded
            } else {
                msg_debug("[auto-restore] step2 returned None → no external session found");
                None
            }
        });
    let session = data.sessions.entry(chat_id).or_insert_with(|| ChatSession {
        session_id: None,
        current_path: None,
        history: Vec::new(),
        pending_uploads: Vec::new(),
    });
    session.current_path = Some(last_path.clone());
    if let Some((session_data, _)) = existing {
        msg_debug(&format!("[auto-restore] SUCCESS: session_id={}, history_len={}", session_data.session_id, session_data.history.len()));
        if !session_data.session_id.is_empty() {
            session.session_id = Some(session_data.session_id.clone());
        } else {
            cleanup_session_files(&last_path, auto_provider);
        }
        session.history = session_data.history.clone();
    } else {
        msg_debug("[auto-restore] FAIL: no session data found (local or external) → empty history");
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ↻ [{user_name}] Auto-restored session: {last_path}");

    // Append auto-restore marker to group chat log
    if chat_id.0 < 0 {
        let uname = data.bot_username.clone();
        let dname = data.bot_display_name.clone();
        if !uname.is_empty() {
            let dn = if dname.is_empty() { None } else { Some(dname) };
            append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                ts: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                bot: uname,
                bot_display_name: dn,
                role: "system".to_string(),
                from: None,
                text: format!("Session restored at {}", last_path),
                clear: false,
            });
        }
    }
}

/// Auto-create a workspace session under ~/.cokacdir/workspace/<random>.
/// Returns (session_id, path) on success; None if filesystem fails.
async fn auto_create_workspace_session(
    state: &SharedState,
    chat_id: ChatId,
    bot_token: &str,
) -> Option<(Option<String>, String)> {
    msg_debug(&format!("[auto_workspace] chat_id={}, auto-creating workspace", chat_id.0));
    let auto_path = {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => {
                msg_debug(&format!("[auto_workspace] chat_id={}, home_dir() returned None", chat_id.0));
                return None;
            }
        };
        let workspace_dir = home.join(".cokacdir").join("workspace");
        use rand::Rng;
        let random_name: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(8)
            .map(|b| (b as char).to_ascii_lowercase())
            .collect();
        let new_dir = workspace_dir.join(&random_name);
        match fs::create_dir_all(&new_dir) {
            Ok(_) => new_dir.display().to_string(),
            Err(e) => {
                msg_debug(&format!("[auto_workspace] chat_id={}, create_dir_all failed: {}", chat_id.0, e));
                return None;
            }
        }
    };
    // Re-check under lock: another concurrent message may have already created a session
    let (sid, path) = {
        let mut data = state.lock().await;
        if let Some(existing) = data.sessions.get(&chat_id).and_then(|s| s.current_path.clone()) {
            msg_debug(&format!("[auto_workspace] chat_id={}, session already created by another message: {}", chat_id.0, existing));
            let _ = fs::remove_dir(&auto_path);
            let sid = data.sessions.get(&chat_id).and_then(|s| s.session_id.clone());
            (sid, existing)
        } else {
            let session = data.sessions.entry(chat_id).or_insert_with(|| ChatSession {
                session_id: None,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
            });
            session.current_path = Some(auto_path.clone());
            session.session_id = None;
            session.history.clear();
            data.settings.last_sessions.insert(chat_id.0.to_string(), auto_path.clone());
            save_bot_settings(bot_token, &data.settings);
            msg_debug(&format!("[auto_workspace] chat_id={}, new workspace session created: {}", chat_id.0, auto_path));
            (None, auto_path)
        }
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    if sid.is_some() {
        println!("  [{ts}] ▶ Using existing session: {path}");
    } else {
        println!("  [{ts}] ▶ Auto-started session: {path}");
    }
    Some((sid, path))
}

/// Telegram message length limit
const TELEGRAM_MSG_LIMIT: usize = 4096;
/// Threshold for switching to file attachment mode: responses above this size
/// are sent as a .txt file instead of multiple messages.
/// Can be overridden with the COKAC_FILE_ATTACH_THRESHOLD environment variable.
fn file_attach_threshold() -> usize {
    std::env::var("COKAC_FILE_ATTACH_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(TELEGRAM_MSG_LIMIT * 2)
}

fn should_attach_response_as_file(response_len: usize, provider_str: &str) -> bool {
    if provider_str == "opencode" {
        return false;
    }
    response_len > file_attach_threshold()
}
/// Maximum number of messages that can be queued per chat in queue mode
const MAX_QUEUE_SIZE: usize = 20;
/// Default queue mode state for chats without explicit setting
const QUEUE_MODE_DEFAULT: bool = true;
/// Default silent mode state for chats without explicit setting
const SILENT_MODE_DEFAULT: bool = true;
/// Default direct mode state for chats without explicit setting
const DIRECT_MODE_DEFAULT: bool = false;
/// Default public access state for group chats without explicit setting
const PUBLIC_MODE_DEFAULT: bool = false;
/// Default debug mode state (global, not per-chat)
const DEBUG_MODE_DEFAULT: bool = false;

/// Compute a short hash key from the bot token (first 16 chars of SHA-256 hex)
pub fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..8]) // 16 hex chars
}

/// Detect the messenger platform from the bot token.
/// Bridge tokens use the format "bridge_<platform>_<hash>".
fn detect_platform(token: &str) -> &str {
    if let Some(rest) = token.strip_prefix("bridge_") {
        rest.split('_').next().unwrap_or("Messenger")
    } else {
        "Telegram"
    }
}

/// Capitalize first letter (e.g. "discord" → "Discord")
fn capitalize_platform(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
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

    let models: HashMap<String, String> = entry.get("models")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let debug = entry.get("debug").and_then(|v| v.as_bool()).unwrap_or(DEBUG_MODE_DEFAULT);

    let silent: HashMap<String, bool> = entry.get("silent")
        .and_then(|v| v.as_object())
        .map(|obj| obj.iter()
            .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
            .collect())
        .unwrap_or_default();

    let direct: HashMap<String, bool> = entry.get("direct")
        .and_then(|v| v.as_object())
        .map(|obj| obj.iter()
            .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
            .collect())
        .unwrap_or_default();

    let context: HashMap<String, usize> = entry.get("context")
        .and_then(|v| v.as_object())
        .map(|obj| obj.iter()
            .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n as usize)))
            .collect())
        .unwrap_or_default();

    let instructions: HashMap<String, String> = entry.get("instructions")
        .and_then(|v| v.as_object())
        .map(|obj| obj.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect())
        .unwrap_or_default();

    let queue: HashMap<String, bool> = entry.get("queue")
        .and_then(|v| v.as_object())
        .map(|obj| obj.iter()
            .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
            .collect())
        .unwrap_or_default();

    let username = entry.get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let display_name = entry.get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let greeting = entry.get("greeting").and_then(|v| v.as_bool()).unwrap_or(false);

    let use_chrome: HashMap<String, bool> = entry.get("use_chrome")
        .and_then(|v| v.as_object())
        .map(|obj| obj.iter()
            .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
            .collect())
        .unwrap_or_default();

    let end_hook: HashMap<String, String> = entry.get("end_hook")
        .and_then(|v| v.as_object())
        .map(|obj| obj.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect())
        .unwrap_or_default();

    BotSettings { allowed_tools, last_sessions, owner_user_id, as_public_for_group_chat, models, debug, silent, direct, context, instructions, queue, username, display_name, greeting, use_chrome, end_hook }
}

/// Save bot settings to bot_settings.json
fn save_bot_settings(token: &str, settings: &BotSettings) {
    let Some(path) = bot_settings_path() else { return };
    // Ensure directory exists
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Use file lock to prevent race condition when multiple bots save simultaneously
    let lock_path = path.with_extension("json.lock");
    let lock_file = match fs::OpenOptions::new().create(true).write(true).open(&lock_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    use fs2::FileExt;
    if lock_file.lock_exclusive().is_err() {
        return;
    }
    // Read-modify-write under exclusive lock
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
        "models": settings.models,
        "debug": settings.debug,
        "silent": settings.silent,
        "direct": settings.direct,
        "context": settings.context,
        "instructions": settings.instructions,
        "queue": settings.queue,
        "username": settings.username,
        "display_name": settings.display_name,
        "greeting": settings.greeting,
        "use_chrome": settings.use_chrome,
        "end_hook": settings.end_hook,
    });
    if let Some(owner_id) = settings.owner_user_id {
        entry["owner_user_id"] = serde_json::json!(owner_id);
    }
    json[key] = entry;
    if let Ok(s) = serde_json::to_string_pretty(&json) {
        let tmp_path = path.with_extension("json.tmp");
        if fs::write(&tmp_path, &s).is_ok() {
            let _ = fs::rename(&tmp_path, &path);
        }
    }
    // lock released when lock_file is dropped
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

/// Resolve bot username from its hash key by searching bot_settings.json
pub fn resolve_username_by_hash(hash: &str) -> Option<String> {
    msg_debug(&format!("[resolve_username_by_hash] hash={}", hash));
    let path = match bot_settings_path() {
        Some(p) => p,
        None => {
            msg_debug("[resolve_username_by_hash] bot_settings_path() returned None");
            return None;
        }
    };
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            msg_debug(&format!("[resolve_username_by_hash] read failed: {}", e));
            return None;
        }
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            msg_debug(&format!("[resolve_username_by_hash] JSON parse failed: {}", e));
            return None;
        }
    };
    let obj = match json.as_object() {
        Some(o) => o,
        None => {
            msg_debug("[resolve_username_by_hash] JSON is not an object");
            return None;
        }
    };
    let entry = match obj.get(hash) {
        Some(e) => e,
        None => {
            msg_debug(&format!("[resolve_username_by_hash] hash key not found: {}", hash));
            return None;
        }
    };
    let result = entry.get("username").and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    msg_debug(&format!("[resolve_username_by_hash] result: {:?}", result));
    result
}

/// Check if a bot with the given username exists in bot_settings.json
pub fn bot_username_exists(username: &str) -> bool {
    msg_debug(&format!("[bot_username_exists] checking: {}", username));
    let Some(path) = bot_settings_path() else {
        msg_debug("[bot_username_exists] bot_settings_path() returned None");
        return false;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        msg_debug("[bot_username_exists] read failed");
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        msg_debug("[bot_username_exists] JSON parse failed");
        return false;
    };
    let Some(obj) = json.as_object() else {
        msg_debug("[bot_username_exists] JSON is not an object");
        return false;
    };
    let target = username.to_lowercase();
    let found = obj.values().any(|entry| {
        entry.get("username")
            .and_then(|v| v.as_str())
            .map(|u| u.to_lowercase() == target)
            .unwrap_or(false)
    });
    msg_debug(&format!("[bot_username_exists] target={}, found={}", target, found));
    found
}

/// Resolve bot display_name from its username by searching bot_settings.json
pub fn resolve_display_name_by_username(username: &str) -> Option<String> {
    let path = bot_settings_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let obj = json.as_object()?;
    let target = username.to_lowercase();
    for entry in obj.values() {
        let uname = entry.get("username").and_then(|v| v.as_str()).unwrap_or("");
        if uname.to_lowercase() == target {
            return entry.get("display_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
        }
    }
    None
}

/// Directory for bot-to-bot message files: ~/.cokacdir/messages/
pub fn messages_dir() -> Option<std::path::PathBuf> {
    let result = dirs::home_dir().map(|h| h.join(".cokacdir").join("messages"));
    msg_debug(&format!("[messages_dir] result={:?}", result));
    result
}

/// Normalize a tool name to the canonical camelCase form defined in
/// `ALL_TOOLS`, by case-insensitive match. This lets users type any casing
/// (`bash`, `BASH`, `Bash`, `taskoutput`, `TaskOutput`) and still end up with
/// the exact string used internally — essential for camelCase tools such as
/// `TaskOutput`, `WebSearch`, `NotebookEdit`, etc., which the previous
/// "first-letter uppercase, rest lowercase" rule corrupted (e.g. input
/// `TaskOutput` → `Taskoutput`, breaking removal with `/allowed -TaskOutput`).
///
/// If the name is not a known tool, it is returned as typed and the caller
/// will simply fail membership checks — matching the previous behaviour for
/// unknown tools.
fn normalize_tool_name(name: &str) -> String {
    let lower = name.to_lowercase();
    for (canonical, _, _) in ALL_TOOLS {
        if canonical.to_lowercase() == lower {
            return canonical.to_string();
        }
    }
    name.to_string()
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

/// Entry point: start the Telegram bot with long polling.
/// `api_url`: Optional API base URL override (used by messenger bridge proxy).
///            When None, connects to the real Telegram API.
pub async fn run_bot(token: &str, api_url: Option<&str>) {
    let api_base_url = api_url.unwrap_or("https://api.telegram.org");
    msg_debug(&format!("[run_bot] api_url={:?}, api_base_url={}", api_url.is_some(), api_base_url));
    let bot = Bot::new(token);
    let bot = if let Some(url) = api_url {
        msg_debug(&format!("[run_bot] setting custom api_url: {}", url));
        match reqwest::Url::parse(url) {
            Ok(parsed) => bot.set_api_url(parsed),
            Err(e) => {
                msg_debug(&format!("[run_bot] api_url parse failed: {}", e));
                bot
            }
        }
    } else {
        bot
    };
    let mut bot_settings = load_bot_settings(token);

    // Get bot's own username and display name for @mention filtering in group chats
    msg_debug("[run_bot] calling get_me to retrieve bot username");
    let (bot_username, bot_display_name) = match bot.get_me().await {
        Ok(me) => {
            let uname = me.username.clone().unwrap_or_default().to_lowercase();
            let dname = me.first_name.clone();
            msg_debug(&format!("[run_bot] get_me success: username={}, display_name={}, id={}", uname, dname, me.id));
            println!("  ✓ Bot: {} (@{uname})", dname);
            (uname, dname)
        }
        Err(e) => {
            msg_debug(&format!("[run_bot] get_me failed: {}", e));
            println!("  ⚠ Failed to get bot info: {e}");
            (String::new(), String::new())
        }
    };

    // Save username and display_name to bot_settings for CLI --message lookup
    if !bot_username.is_empty() {
        msg_debug(&format!("[run_bot] saving username to bot_settings: {}", bot_username));
        bot_settings.username = bot_username.clone();
        bot_settings.display_name = bot_display_name.clone();
        save_bot_settings(token, &bot_settings);
    } else {
        msg_debug("[run_bot] bot_username is empty, skipping save");
    }

    // Restore debug flag from saved settings
    if bot_settings.debug {
        TG_DEBUG.store(true, Ordering::Relaxed);
        crate::services::claude::DEBUG_ENABLED.store(true, Ordering::Relaxed);
    }

    // Register bot commands for autocomplete
    let commands = vec![
        teloxide::types::BotCommand::new("help", "Show help"),
        teloxide::types::BotCommand::new("start", "Start session at directory"),
        teloxide::types::BotCommand::new("pwd", "Show current working directory"),
        teloxide::types::BotCommand::new("session", "Show current session ID"),
        teloxide::types::BotCommand::new("clear", "Clear AI conversation history"),
        teloxide::types::BotCommand::new("stop", "Stop current AI request"),
        teloxide::types::BotCommand::new("stopall", "Stop request and clear queue"),
        teloxide::types::BotCommand::new("queue", "Toggle queue mode"),
        teloxide::types::BotCommand::new("loop", "Repeat until task is fully completed"),
        teloxide::types::BotCommand::new("down", "Download file from server"),
        teloxide::types::BotCommand::new("public", "Toggle public access (group only)"),
        teloxide::types::BotCommand::new("availabletools", "List all available tools"),
        teloxide::types::BotCommand::new("allowedtools", "Show currently allowed tools"),
        teloxide::types::BotCommand::new("allowed", "Add/remove tool (+name / -name)"),
        teloxide::types::BotCommand::new("setpollingtime", "Set API polling interval (ms)"),
        teloxide::types::BotCommand::new("model", "Set AI model"),
        teloxide::types::BotCommand::new("greeting", "Toggle compact startup greeting"),
        teloxide::types::BotCommand::new("debug", "Toggle debug logging"),
        teloxide::types::BotCommand::new("envvars", "Show all environment variables"),
        teloxide::types::BotCommand::new("silent", "Toggle silent mode (hide tool calls)"),
        teloxide::types::BotCommand::new("direct", "Toggle direct mode (group only)"),
        teloxide::types::BotCommand::new("contextlevel", "Set group chat log context count"),
        teloxide::types::BotCommand::new("query", "Send message to AI (/query@bot for groups)"),
        teloxide::types::BotCommand::new("instruction", "Set system instruction for this chat"),
        teloxide::types::BotCommand::new("instruction_clear", "Clear system instruction"),
        teloxide::types::BotCommand::new("setendhook", "Set message to send when processing completes"),
        teloxide::types::BotCommand::new("setendhook_clear", "Clear end hook message"),
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
        pending_schedules: HashMap::new(),
        message_queues: HashMap::new(),
        bot_username: bot_username.clone(),
        bot_display_name: bot_display_name.clone(),
        api_base_url: api_base_url.to_string(),
        loop_states: HashMap::new(),
        loop_feedback: HashMap::new(),
    }));

    println!("  ✓ Bot connected — Listening for messages");
    println!("  ✓ Scheduler started (5s interval)");

    // Send startup greeting to known chats
    {
        let data = state.lock().await;
        let chat_ids: Vec<i64> = data.settings.last_sessions.keys()
            .filter_map(|k| k.parse::<i64>().ok())
            .collect();
        let version = env!("CARGO_PKG_VERSION");
        let update_notice = check_latest_version(version).await;
        for cid in chat_ids {
            let chat_id = ChatId(cid);
            let last_path = data.settings.last_sessions.get(&cid.to_string())
                .map(|p| p.as_str())
                .unwrap_or("(unknown)");
            let model = get_model(&data.settings, chat_id);
            let provider = detect_provider(model.as_deref());
            let msg = if data.settings.greeting {
                // Compact mode: single line with version and model
                format!("🟢 cokacdir started (v{}, {})", version, provider)
            } else {
                // Full mode: marketing message with links
                let mut m = format!("🟢 cokacdir started (v{}, {})\n📂 Resuming session at {}\n💬 Join @cokacvibe for tips, updates, and community support\n⭐ Star us on GitHub: https://github.com/kstost/cokacdir", version, provider, last_path);
                if let Some(ref notice) = update_notice {
                    m.push('\n');
                    m.push_str(notice);
                }
                m
            };
            let _ = tg!("send_message", bot.send_message(chat_id, msg).await);
        }
    }

    // Schedule workspace directories are preserved for user access via /start

    // Spawn scheduler loop
    let scheduler_bot = bot.clone();
    let scheduler_state = state.clone();
    let scheduler_token = token.to_string();
    let scheduler_bot_username = bot_username.clone();
    let scheduler_bot_display_name = bot_display_name.clone();
    let scheduler_handle = tokio::spawn(scheduler_loop(scheduler_bot, scheduler_state, scheduler_token, scheduler_bot_username, scheduler_bot_display_name));

    // Flush all pending updates so we don't process messages that arrived while
    // the bot was offline.
    // Step 1: getUpdates(offset=-1) discards all but the last pending update.
    // Step 2: getUpdates(offset=last_id+1) confirms the last one too.
    match bot.get_updates().offset(-1).limit(1).await {
        Ok(updates) => {
            if let Some(last) = updates.last() {
                let next_offset = last.id.0.saturating_add(1).min(i32::MAX as u32) as i32;
                msg_debug(&format!("[run_bot] flush step 1: got last update_id={}, confirming with offset={}",
                    last.id.0, next_offset));
                // Confirm the last update so teloxide::repl won't receive it.
                // Retry until confirmed — must not let the last message leak through.
                let mut confirmed = false;
                for attempt in 1..=5 {
                    match bot.get_updates().offset(next_offset).limit(0).await {
                        Ok(_) => {
                            msg_debug(&format!("[run_bot] flush step 2: confirmed offset={} (attempt {})", next_offset, attempt));
                            confirmed = true;
                            break;
                        }
                        Err(e) => {
                            msg_debug(&format!("[run_bot] flush step 2 failed (attempt {}): {}", attempt, e));
                            if attempt < 5 {
                                tokio::time::sleep(tokio::time::Duration::from_millis(500 * attempt as u64)).await;
                            }
                        }
                    }
                }
                if confirmed {
                    println!("  ✓ Flushed pending updates (up to id={})", last.id.0);
                } else {
                    eprintln!("  ✗ FATAL: failed to confirm pending updates after 5 attempts — aborting to prevent processing stale messages");
                    std::process::exit(1);
                }
            } else {
                msg_debug("[run_bot] flush: no pending updates");
                println!("  ✓ No pending updates");
            }
        }
        Err(e) => {
            msg_debug(&format!("[run_bot] failed to flush pending updates: {}", e));
            println!("  ⚠ Failed to flush pending updates: {}", e);
        }
    }

    // Run polling loop with automatic reconnection on network failure.
    // teloxide::repl may panic or exit silently when the network drops
    // (especially on Windows). We wrap it in tokio::spawn to catch panics
    // and retry with exponential backoff.
    let token_for_repl = token.to_string();
    let username_for_repl = bot_username;
    let mut reconnect_backoff_secs = 5u64;

    loop {
        let repl_start = std::time::Instant::now();
        let bot_clone = bot.clone();
        let state_clone = state.clone();
        let token_clone = token_for_repl.clone();
        let username_clone = username_for_repl.clone();

        let repl_result = tokio::spawn(async move {
            teloxide::repl(bot_clone, move |bot: Bot, msg: Message| {
                let state = state_clone.clone();
                let token = token_clone.clone();
                let bot_username = username_clone.clone();
                async move {
                    handle_message(bot, msg, state, &token, &bot_username).await
                }
            })
            .await;
        })
        .await;

        match repl_result {
            Ok(()) => {
                // Normal exit — Ctrl+C or graceful shutdown
                msg_debug("[run_bot] repl exited normally (shutdown)");
                break;
            }
            Err(e) => {
                // Task panicked — likely network disconnection
                let ran_for = repl_start.elapsed();
                msg_debug(&format!("[run_bot] repl crashed after {:.1?}: {}", ran_for, e));
                println!("  ⚠ Bot disconnected — reconnecting in {}s...", reconnect_backoff_secs);
                tokio::time::sleep(tokio::time::Duration::from_secs(reconnect_backoff_secs)).await;

                // Reset backoff if the bot was stable (>60s) before crashing
                if ran_for.as_secs() > 60 {
                    reconnect_backoff_secs = 5;
                } else {
                    reconnect_backoff_secs = (reconnect_backoff_secs * 2).min(60);
                }

                println!("  ⟳ Reconnecting...");
            }
        }
    }

    scheduler_handle.abort();
}

/// Route incoming messages to appropriate handlers
async fn handle_message(
    bot: Bot,
    msg: Message,
    state: SharedState,
    token: &str,
    bot_username: &str,
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
        log_incoming_message(&msg, false, "no_user_id");
        return Ok(());
    };
    let is_group_chat = matches!(msg.chat.kind, teloxide::types::ChatKind::Public(_));
    let (require_prefix, imprinted, is_owner) = {
        let mut data = state.lock().await;
        let chat_key = chat_id.0.to_string();
        let direct_setting = data.settings.direct.get(&chat_key).copied().unwrap_or(DIRECT_MODE_DEFAULT);
        let is_direct = is_group_chat && direct_setting;
        msg_debug(&format!("[handle_message] chat_id={}, uid={}, is_group_chat={}, direct_setting={}, is_direct={}",
            chat_id.0, uid, is_group_chat, direct_setting, is_direct));
        // In direct mode, ; prefix requirement is waived but is_group_chat stays true
        let require_prefix = is_group_chat && !is_direct;
        msg_debug(&format!("[handle_message] require_prefix={}", require_prefix));
        let (imprinted, is_owner) = match data.settings.owner_user_id {
            None => {
                // Imprint: register first user as owner
                msg_debug(&format!("[handle_message] imprinting uid={} as owner", uid));
                data.settings.owner_user_id = Some(uid);
                save_bot_settings(token, &data.settings);
                println!("  [{timestamp}] ★ Owner registered: {raw_user_name} (id:{uid})");
                (true, true)
            }
            Some(owner_id) => {
                if uid != owner_id {
                    // Check if this is a public group chat
                    let is_public = is_group_chat
                        && data.settings.as_public_for_group_chat.get(&chat_key).copied().unwrap_or(PUBLIC_MODE_DEFAULT);
                    msg_debug(&format!("[handle_message] non-owner uid={}, owner_id={}, is_public={}", uid, owner_id, is_public));
                    if !is_public {
                        // Unregistered user → reject silently (log only)
                        msg_debug(&format!("[handle_message] rejected non-owner uid={}", uid));
                        println!("  [{timestamp}] ✗ Rejected: {raw_user_name} (id:{uid})");
                        log_incoming_message(&msg, false, "unauthorized");
                        return Ok(());
                    }
                    // Public group chat: allow non-owner user
                    println!("  [{timestamp}] ○ [{raw_user_name}(id:{uid})] Public group access");
                    (false, false)
                } else {
                    msg_debug(&format!("[handle_message] owner confirmed uid={}", uid));
                    (false, true)
                }
            }
        };
        msg_debug(&format!("[handle_message] result: require_prefix={}, imprinted={}, is_owner={}", require_prefix, imprinted, is_owner));
        (require_prefix, imprinted, is_owner)
    };
    if imprinted {
        // Owner registration is logged to server console only
        // No response sent to the user
    }

    log_incoming_message(&msg, true, "");

    let user_name = format!("{}({uid})", raw_user_name);

    let has_file = msg.document().is_some() || msg.photo().is_some() || msg.video().is_some() || msg.voice().is_some() || msg.audio().is_some() || msg.animation().is_some() || msg.video_note().is_some();

    // Auto-restore session for file uploads (before text extraction)
    if has_file {
        auto_restore_session(&state, chat_id, &user_name).await;
    }

    // Handle file/photo/media uploads
    if has_file {
        // In group chats (with prefix required), only process uploads whose caption starts with ';' or '@botname'
        if require_prefix {
            let caption = msg.caption().unwrap_or("");
            msg_debug(&format!("[handle_message] upload: require_prefix=true, caption={:?}", caption));
            if !bot_username.is_empty() && caption.starts_with('@') {
                // Caption starts with @mention — only accept if targeting this bot
                let caption_lower = caption.to_lowercase();
                let prefix = format!("@{}", bot_username.to_lowercase());
                if !caption_lower.starts_with(&prefix)
                    || caption_lower.len() > prefix.len()
                        && !caption_lower[prefix.len()..].starts_with(|c: char| c.is_whitespace())
                {
                    msg_debug("[handle_message] upload: rejected (@mention for another bot)");
                    return Ok(());
                }
                msg_debug(&format!("[handle_message] upload: accepted (@{} mention)", bot_username));
            } else if caption.starts_with(';') {
                msg_debug("[handle_message] upload: accepted (; prefix, all bots)");
            } else {
                msg_debug("[handle_message] upload: rejected (no ; or @ prefix)");
                return Ok(());
            }
        } else {
            msg_debug(&format!("[handle_message] upload: require_prefix=false, caption={:?}", msg.caption()));
        }
        let file_hint = if msg.animation().is_some() { "animation" }
            else if msg.document().is_some() { "document" }
            else if msg.photo().is_some() { "photo" }
            else if msg.video().is_some() { "video" }
            else if msg.voice().is_some() { "voice" }
            else if msg.audio().is_some() { "audio" }
            else { "video_note" };
        println!("  [{timestamp}] ◀ [{user_name}] Upload: {file_hint}");
        handle_file_upload(&bot, chat_id, &msg, &state, &user_name).await?;
        println!("  [{timestamp}] ▶ [{user_name}] Upload complete");
        // If caption contains text, send it to AI as a follow-up message
        if let Some(caption) = msg.caption() {
            let text_part = if require_prefix {
                // Group chat (prefix mode): extract message text from caption
                // Formats: ";text", "@botname text", "@botname ;text"
                let extracted = if !bot_username.is_empty() && caption.starts_with('@') {
                    // "@botname text" → extract text after @botname
                    let prefix = format!("@{} ", bot_username);
                    if caption.to_lowercase().starts_with(&prefix.to_lowercase()) {
                        let body = caption[prefix.len()..].trim_start();
                        body.strip_prefix(';').map(|s| s.trim_start()).unwrap_or(body)
                    } else {
                        ""
                    }
                } else if caption.starts_with(';') {
                    caption[1..].trim_start()
                } else {
                    ""
                };
                let result = if extracted.is_empty() { None } else { Some(extracted) };
                msg_debug(&format!("[handle_message] upload caption (prefix mode): extracted={:?}", result));
                result
            } else {
                // DM or direct mode: use entire caption as-is
                let trimmed = caption.trim();
                let result = if trimmed.is_empty() { None } else { Some(trimmed) };
                msg_debug(&format!("[handle_message] upload caption (direct): extracted={:?}", result));
                result
            };
            if let Some(text) = text_part {
                if !text.is_empty() {
                    // Block if an AI request is already in progress
                    // Atomically: check busy + queue mode + push to queue (prevents race with /stopall)
                    let (ai_busy, queue_enabled, queue_result) = {
                        let mut data = state.lock().await;
                        let busy = data.cancel_tokens.contains_key(&chat_id);
                        let qkey = chat_id.0.to_string();
                        let qmode = data.settings.queue.get(&qkey).copied().unwrap_or(QUEUE_MODE_DEFAULT);
                        msg_debug(&format!("[queue:media] chat_id={}, busy={}, queue_mode={}", chat_id.0, busy, qmode));
                        let qr = if busy && qmode {
                            let cur_len = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
                            let queue_full = cur_len >= MAX_QUEUE_SIZE;
                            if queue_full {
                                msg_debug(&format!("[queue:media] chat_id={}, queue FULL ({}/{})", chat_id.0, cur_len, MAX_QUEUE_SIZE));
                                None // queue full
                            } else {
                                // Capture pending_uploads so they stay associated with this caption
                                let uploads = data.sessions.get_mut(&chat_id)
                                    .map(|s| std::mem::take(&mut s.pending_uploads))
                                    .unwrap_or_default();
                                let qid = generate_queue_id();
                                let q = data.message_queues.entry(chat_id).or_insert_with(std::collections::VecDeque::new);
                                q.push_back(QueuedMessage {
                                    id: qid.clone(),
                                    text: text.to_string(),
                                    user_display_name: user_name.clone(),
                                    pending_uploads: uploads.clone(),
                                });
                                msg_debug(&format!("[queue:media] chat_id={}, QUEUED id={}, pos={}, text={:?}, uploads={}", chat_id.0, qid, q.len(), truncate_str(text, 60), uploads.len()));
                                Some((qid, text.to_string()))
                            }
                        } else {
                            None
                        };
                        (busy, qmode, qr)
                    };
                    if ai_busy {
                        if queue_enabled {
                            shared_rate_limit_wait(&state, chat_id).await;
                            if let Some((qid, qtxt)) = queue_result {
                                let preview = truncate_str(&qtxt, 30);
                                tg!("send_message", bot.send_message(chat_id, &format!("Queued ({qid}) \"{preview}\"\n- /stopall to cancel all\n- /stop_{qid} to cancel this"))
                                    .await)?;
                            } else {
                                tg!("send_message", bot.send_message(chat_id, &format!("Queue full (max {}). Use /stopall to clear.", MAX_QUEUE_SIZE))
                                    .await)?;
                            }
                        } else {
                            shared_rate_limit_wait(&state, chat_id).await;
                            tg!("send_message", bot.send_message(chat_id, "AI request in progress. Use /stop to cancel.")
                                .await)?;
                        }
                    } else {
                        handle_text_message(&bot, chat_id, text, &state, &user_name, false).await?;
                    }
                }
            }
        }
        return Ok(());
    }

    // Handle location sharing — store as pending, deliver with next text message
    if let Some(location) = msg.location() {
        msg_debug(&format!("[handle_message] chat_id={}, location: lat={}, lon={}", chat_id.0, location.latitude, location.longitude));
        auto_restore_session(&state, chat_id, &user_name).await;

        let location_record = format!(
            "[Location shared] Latitude: {}, Longitude: {}",
            location.latitude, location.longitude
        );
        let stored = {
            let mut data = state.lock().await;
            if let Some(session) = data.sessions.get_mut(&chat_id) {
                session.pending_uploads.push(location_record.clone());
                true
            } else {
                false
            }
        };
        if stored {
            println!("  [{timestamp}] ◀ [{user_name}] Location: {}, {} (pending)", location.latitude, location.longitude);
            shared_rate_limit_wait(&state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "Location received.").await)?;
        } else {
            shared_rate_limit_wait(&state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "No active session. Use /start <path> first.").await)?;
        }
        return Ok(());
    }

    // Handle venue sharing (place selected from Telegram's location picker) — store as pending
    if let Some(venue) = msg.venue() {
        let loc = &venue.location;
        msg_debug(&format!("[handle_message] chat_id={}, venue: title={:?}, addr={:?}, lat={}, lon={}",
            chat_id.0, venue.title, venue.address, loc.latitude, loc.longitude));
        auto_restore_session(&state, chat_id, &user_name).await;

        let location_record = format!(
            "[Location shared] {}, {} (Latitude: {}, Longitude: {})",
            venue.title, venue.address, loc.latitude, loc.longitude
        );
        let stored = {
            let mut data = state.lock().await;
            if let Some(session) = data.sessions.get_mut(&chat_id) {
                session.pending_uploads.push(location_record.clone());
                true
            } else {
                false
            }
        };
        if stored {
            println!("  [{timestamp}] ◀ [{user_name}] Venue: {} ({}, {}) (pending)", venue.title, loc.latitude, loc.longitude);
            shared_rate_limit_wait(&state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, format!("Location received: {}", venue.title)).await)?;
        } else {
            shared_rate_limit_wait(&state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "No active session. Use /start <path> first.").await)?;
        }
        return Ok(());
    }

    let Some(raw_text) = msg.text() else {
        msg_debug(&format!("[handle_message] chat_id={}, non-text message (no raw_text), skipping", chat_id.0));
        return Ok(());
    };

    msg_debug(&format!("[handle_message] chat_id={}, user={}, raw_text={:?}", chat_id.0, user_name, truncate_str(raw_text, 100)));

    // Normalize "@botname ..." → strip @botname prefix (group chat shorthand)
    // "@botname /cmd args" → "/cmd args", "@botname hello" → ";hello"
    // In direct mode, ignore messages that mention a different bot
    let starts_with_at = raw_text.starts_with('@');
    let has_bot_username = !bot_username.is_empty();
    msg_debug(&format!("[mention_routing] input: raw_text={:?}, bot_username={:?}, starts_with_at={}, has_bot_username={}, require_prefix={}, is_group_chat={}",
        truncate_str(raw_text, 100), bot_username, starts_with_at, has_bot_username, require_prefix, is_group_chat));
    let mention_rewritten: Option<String>;
    let raw_text = if has_bot_username && starts_with_at {
        let prefix = format!("@{} ", bot_username);
        let is_self_mention = raw_text.to_lowercase().starts_with(&prefix);
        msg_debug(&format!("[mention_routing] @-prefix detected: checking self-mention, prefix={:?}, is_self_mention={}", prefix, is_self_mention));
        if is_self_mention {
            let body = raw_text[prefix.len()..].trim_start();
            if body.starts_with('/') || body.starts_with('!') || body.starts_with(';') {
                msg_debug(&format!("[mention_routing] self-mention with command prefix: {:?} → {:?} (pass-through command char)", raw_text, body));
                mention_rewritten = Some(body.to_string());
            } else {
                let prefixed = format!(";{}", body);
                msg_debug(&format!("[mention_routing] self-mention with plain text: {:?} → {:?} (added ; prefix)", raw_text, prefixed));
                mention_rewritten = Some(prefixed);
            }
            mention_rewritten.as_deref().unwrap()
        } else {
            // Message starts with @ but mentions a different bot
            let mentioned = raw_text[1..].split_whitespace().next().unwrap_or("");
            msg_debug(&format!("[mention_routing] other-bot mention: mentioned={:?}, require_prefix={}, direct_mode={}", mentioned, require_prefix, !require_prefix));
            if is_group_chat && !require_prefix {
                // Direct mode ON (group chat): ignore messages for other bots
                if !mentioned.is_empty() && mentioned.to_lowercase() != bot_username {
                    msg_debug(&format!("[mention_routing] IGNORED: direct mode ON, message is for @{}, not for @{}", mentioned, bot_username));
                    return Ok(());
                }
                msg_debug(&format!("[mention_routing] direct mode ON, but mentioned={:?} matches self or is empty, passing through", mentioned));
            } else {
                msg_debug(&format!("[mention_routing] direct mode OFF, passing through raw_text as-is (will be filtered by require_prefix later)"));
            }
            raw_text
        }
    } else {
        if starts_with_at {
            msg_debug(&format!("[mention_routing] starts with @ but bot_username is empty, passing through"));
        } else {
            msg_debug(&format!("[mention_routing] no @-prefix, passing through raw_text as-is"));
        }
        raw_text
    };

    // Strip @botname suffix from commands (e.g. "/pwd@mybot" → "/pwd")
    // If @botname doesn't match this bot, ignore the command (it's for another bot)
    let starts_with_slash = raw_text.starts_with('/');
    msg_debug(&format!("[cmd_routing] input: raw_text={:?}, starts_with_slash={}", truncate_str(raw_text, 100), starts_with_slash));
    let text = if starts_with_slash {
        if let Some(space_pos) = raw_text.find(' ') {
            // "/cmd@bot args" → "/cmd args"
            let cmd_part = &raw_text[..space_pos];
            let args_part = &raw_text[space_pos..];
            if let Some(at_pos) = cmd_part.find('@') {
                let mentioned = &cmd_part[at_pos + 1..];
                let is_self = mentioned.to_lowercase() == bot_username;
                msg_debug(&format!("[cmd_routing] slash+args: cmd={:?}, mentioned={:?}, is_self={}, bot_username={:?}", cmd_part, mentioned, is_self, bot_username));
                if has_bot_username && !is_self {
                    msg_debug(&format!("[cmd_routing] IGNORED: command {:?} is for @{}, not for @{}", cmd_part, mentioned, bot_username));
                    return Ok(());
                }
                let result = format!("{}{}", &cmd_part[..at_pos], args_part);
                msg_debug(&format!("[cmd_routing] stripped @mention from command: {:?} → {:?}", raw_text, result));
                result
            } else {
                msg_debug(&format!("[cmd_routing] slash+args, no @mention: {:?} → all bots", raw_text));
                raw_text.to_string()
            }
        } else {
            // "/cmd@bot" (no args) → "/cmd"
            if let Some(at_pos) = raw_text.find('@') {
                let mentioned = &raw_text[at_pos + 1..];
                let is_self = mentioned.to_lowercase() == bot_username;
                msg_debug(&format!("[cmd_routing] slash-only: cmd={:?}, mentioned={:?}, is_self={}, bot_username={:?}", raw_text, mentioned, is_self, bot_username));
                if has_bot_username && !is_self {
                    msg_debug(&format!("[cmd_routing] IGNORED: command {:?} is for @{}, not for @{}", raw_text, mentioned, bot_username));
                    return Ok(());
                }
                let result = raw_text[..at_pos].to_string();
                msg_debug(&format!("[cmd_routing] stripped @mention from command: {:?} → {:?}", raw_text, result));
                result
            } else {
                msg_debug(&format!("[cmd_routing] slash-only, no @mention: {:?} → all bots", raw_text));
                raw_text.to_string()
            }
        }
    } else {
        msg_debug(&format!("[cmd_routing] not a slash command, text={:?}", truncate_str(raw_text, 100)));
        raw_text.to_string()
    };
    let preview = &text;

    // Auto-restore session from bot_settings.json if not in memory
    if !text.starts_with("/start") {
        auto_restore_session(&state, chat_id, &user_name).await;
    }

    // In group chats (with prefix required), ignore plain text (only /, !, ; prefixed messages are processed)
    let has_valid_prefix = text.starts_with('/') || text.starts_with('!') || text.starts_with(';');
    msg_debug(&format!("[prefix_filter] text={:?}, require_prefix={}, has_valid_prefix={}, direct_mode={}", truncate_str(&text, 100), require_prefix, has_valid_prefix, !require_prefix));
    if require_prefix && !has_valid_prefix {
        msg_debug(&format!("[prefix_filter] IGNORED: require_prefix=true (direct mode OFF), no valid prefix in text={:?}", truncate_str(&text, 80)));
        // Clear pending uploads — this message was not for this bot,
        // so any previously shared location/file data should be discarded.
        let mut data = state.lock().await;
        if let Some(session) = data.sessions.get_mut(&chat_id) {
            if !session.pending_uploads.is_empty() {
                msg_debug(&format!("[handle_message] clearing {} pending_uploads (not addressed to this bot)", session.pending_uploads.len()));
                session.pending_uploads.clear();
            }
        }
        return Ok(());
    }

    // Block all messages except /stop, /stopall, /queue while an AI request is in progress
    if !text.starts_with("/stop") && !text.starts_with("/queue") {
        // Determine the actual user text to queue (strip command prefix, pure string ops)
        let queue_text = if text.starts_with("/query") {
            // /query command → queue the body
            text.strip_prefix("/query")
                .and_then(|s| {
                    // Handle /query@botname format
                    if s.starts_with('@') {
                        s.find(' ').map(|i| s[i..].trim_start())
                    } else {
                        Some(s.trim_start())
                    }
                })
                .filter(|s| !s.is_empty())
        } else if text.starts_with(';') {
            // ; prefix → queue stripped text
            let stripped = text[1..].trim_start();
            if stripped.is_empty() { None } else { Some(stripped) }
        } else if text.starts_with('!') || text.starts_with('/') {
            // Shell commands and other / commands are NOT queueable
            None
        } else {
            // Plain text
            Some(text.as_str())
        };
        msg_debug(&format!("[queue:text] chat_id={}, text={:?}, queue_text={:?}", chat_id.0, truncate_str(&text, 60), queue_text.map(|s| truncate_str(s, 60))));

        // Atomically: check busy + queue mode + push to queue (prevents race with /stopall)
        // Returns: Option<(can_queue, queue_on, queue_result)>
        //   queue_result: Some((id, text)) if queued, None if full/non-queueable
        let busy_info: Option<(bool, bool, Option<(String, String)>)> = {
            let mut data = state.lock().await;
            if data.cancel_tokens.contains_key(&chat_id) {
                let qkey = chat_id.0.to_string();
                let queue_enabled = data.settings.queue.get(&qkey).copied().unwrap_or(QUEUE_MODE_DEFAULT);
                msg_debug(&format!("[queue:text] chat_id={}, AI busy, queue_mode={}, queueable={}", chat_id.0, queue_enabled, queue_text.is_some()));
                let qr = if queue_enabled {
                    if let Some(qt) = queue_text {
                        let cur_len = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
                        let queue_full = cur_len >= MAX_QUEUE_SIZE;
                        if queue_full {
                            msg_debug(&format!("[queue:text] chat_id={}, queue FULL ({}/{})", chat_id.0, cur_len, MAX_QUEUE_SIZE));
                            None // queue full
                        } else {
                            // Capture pending_uploads so they stay associated with this message
                            let uploads = data.sessions.get_mut(&chat_id)
                                .map(|s| std::mem::take(&mut s.pending_uploads))
                                .unwrap_or_default();
                            let qid = generate_queue_id();
                            let q = data.message_queues.entry(chat_id).or_insert_with(std::collections::VecDeque::new);
                            q.push_back(QueuedMessage {
                                id: qid.clone(),
                                text: qt.to_string(),
                                user_display_name: user_name.clone(),
                                pending_uploads: uploads.clone(),
                            });
                            msg_debug(&format!("[queue:text] chat_id={}, QUEUED id={}, pos={}, text={:?}, uploads={}", chat_id.0, qid, q.len(), truncate_str(qt, 60), uploads.len()));
                            Some((qid, qt.to_string()))
                        }
                    } else {
                        msg_debug(&format!("[queue:text] chat_id={}, non-queueable command, rejecting", chat_id.0));
                        None // non-queueable
                    }
                } else {
                    None
                };
                Some((queue_enabled && queue_text.is_some(), queue_enabled, qr))
            } else {
                msg_debug(&format!("[queue:text] chat_id={}, AI not busy, proceeding normally", chat_id.0));
                None // not busy
            }
        };

        if let Some((can_queue, queue_on, queue_result)) = busy_info {
            if can_queue {
                shared_rate_limit_wait(&state, chat_id).await;
                if let Some((qid, qtxt)) = queue_result {
                    let preview = truncate_str(&qtxt, 30);
                    tg!("send_message", bot.send_message(chat_id, &format!("Queued ({qid}) \"{preview}\"\n- /stopall to cancel all\n- /stop_{qid} to cancel this"))
                        .await)?;
                } else {
                    tg!("send_message", bot.send_message(chat_id, &format!("Queue full (max {}). Use /stopall to clear.", MAX_QUEUE_SIZE))
                        .await)?;
                }
            } else {
                shared_rate_limit_wait(&state, chat_id).await;
                if queue_on {
                    tg!("send_message", bot.send_message(chat_id, "AI request in progress. This command cannot be queued. Use /stop to cancel current, /stopall to cancel all.")
                        .await)?;
                } else {
                    tg!("send_message", bot.send_message(chat_id, "AI request in progress. Use /stop to cancel.")
                        .await)?;
                }
            }
            return Ok(());
        }
    }

    if text.starts_with("/stopall") {
        msg_debug(&format!("[handle_message] routing → /stopall"));
        println!("  [{timestamp}] ◀ [{user_name}] /stopall");
        handle_stopall_command(&bot, chat_id, &state).await?;
    } else if text.starts_with("/stop") {
        // Check if /stop has a valid queue ID argument (exactly 7 hex chars, e.g. "/stop A394FDA")
        let stop_queue_id = text.strip_prefix("/stop")
            .and_then(|s| {
                // Handle /stop@botname format
                let s = if s.starts_with('@') { s.find(' ').map(|i| &s[i..]).unwrap_or("") } else { s };
                // Strip leading underscore to support /stop_ID format (e.g. /stop_3EBA20E)
                let s = s.strip_prefix('_').unwrap_or(s);
                // Strip @botname suffix (for group chat: /stop_ID@botname)
                let s = s.split('@').next().unwrap_or(s);
                let trimmed = s.trim();
                // Only treat as queue ID if it matches the exact format: 7 hex characters
                if trimmed.len() == 7 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
                    Some(trimmed.to_string())
                } else {
                    None
                }
            });
        if let Some(queue_id) = stop_queue_id {
            msg_debug(&format!("[handle_message] routing → /stop queue_id={}", queue_id));
            println!("  [{timestamp}] ◀ [{user_name}] /stop {queue_id}");
            handle_stop_queue_item(&bot, chat_id, &state, &queue_id).await?;
        } else {
            msg_debug(&format!("[handle_message] routing → /stop"));
            println!("  [{timestamp}] ◀ [{user_name}] /stop");
            handle_stop_command(&bot, chat_id, &state).await?;
        }
    } else if text.starts_with("/help") {
        msg_debug(&format!("[handle_message] routing → /help"));
        println!("  [{timestamp}] ◀ [{user_name}] /help");
        handle_help_command(&bot, chat_id, &state).await?;
    } else if text.starts_with("/start") {
        msg_debug(&format!("[handle_message] routing → /start"));
        println!("  [{timestamp}] ◀ [{user_name}] /start");
        handle_start_command(&bot, chat_id, &text, &state, token).await?;
    } else if text.starts_with("/clear") {
        msg_debug(&format!("[handle_message] routing → /clear"));
        println!("  [{timestamp}] ◀ [{user_name}] /clear");
        handle_clear_command(&bot, chat_id, &state).await?;
        println!("  [{timestamp}] ▶ [{user_name}] Session cleared");
    } else if text.starts_with("/pwd") {
        msg_debug(&format!("[handle_message] routing → /pwd"));
        println!("  [{timestamp}] ◀ [{user_name}] /pwd");
        handle_pwd_command(&bot, chat_id, &state).await?;
    } else if text.starts_with("/session") {
        msg_debug(&format!("[handle_message] routing → /session"));
        println!("  [{timestamp}] ◀ [{user_name}] /session");
        handle_session_command(&bot, chat_id, &state).await?;
    } else if text.starts_with("/down") {
        msg_debug(&format!("[handle_message] routing → /down"));
        println!("  [{timestamp}] ◀ [{user_name}] /down {}", text.strip_prefix("/down").unwrap_or("").trim());
        handle_down_command(&bot, chat_id, &text, &state).await?;
    } else if text.starts_with("/public") {
        msg_debug("[handle_message] routing → /public");
        println!("  [{timestamp}] ◀ [{user_name}] /public {}", text.strip_prefix("/public").unwrap_or("").trim());
        handle_public_command(&bot, chat_id, &text, &state, token, is_group_chat, is_owner).await?;
    } else if text.starts_with("/availabletools") {
        msg_debug("[handle_message] routing → /availabletools");
        println!("  [{timestamp}] ◀ [{user_name}] /availabletools");
        { let _m = get_model(&state.lock().await.settings, chat_id);
        if provider_from_model(_m.as_deref()) != "claude" {
            tg!("send_message", bot.send_message(chat_id, "Tool permissions are not supported in this mode.").await)?;
        } else {
            handle_availabletools_command(&bot, chat_id, &state).await?;
        } }
    } else if text.starts_with("/allowedtools") {
        msg_debug("[handle_message] routing → /allowedtools");
        println!("  [{timestamp}] ◀ [{user_name}] /allowedtools");
        { let _m = get_model(&state.lock().await.settings, chat_id);
        if provider_from_model(_m.as_deref()) != "claude" {
            tg!("send_message", bot.send_message(chat_id, "Tool permissions are not supported in this mode.").await)?;
        } else {
            handle_allowedtools_command(&bot, chat_id, &state).await?;
        } }
    } else if text.starts_with("/setpollingtime") {
        msg_debug("[handle_message] routing → /setpollingtime");
        println!("  [{timestamp}] ◀ [{user_name}] /setpollingtime {}", text.strip_prefix("/setpollingtime").unwrap_or("").trim());
        handle_setpollingtime_command(&bot, chat_id, &text, &state).await?;
    } else if text.starts_with("/model") {
        msg_debug("[handle_message] routing → /model");
        println!("  [{timestamp}] ◀ [{user_name}] /model {}", text.strip_prefix("/model").unwrap_or("").trim());
        handle_model_command(&bot, chat_id, &text, &state, token).await?;
    } else if text.starts_with("/greeting") {
        msg_debug("[handle_message] routing → /greeting");
        println!("  [{timestamp}] ◀ [{user_name}] /greeting");
        handle_greeting_command(&bot, chat_id, &state, token).await?;
    } else if text.starts_with("/debug") {
        msg_debug("[handle_message] routing → /debug");
        println!("  [{timestamp}] ◀ [{user_name}] /debug");
        handle_debug_command(&bot, chat_id, &state, token).await?;
    } else if text.starts_with("/envvars") {
        msg_debug("[handle_message] routing → /envvars");
        println!("  [{timestamp}] ◀ [{user_name}] /envvars");
        if !is_owner {
            msg_debug("[handle_message] /envvars rejected: not owner");
            shared_rate_limit_wait(&state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "Only the bot owner can use /envvars.").await)?;
        } else {
            handle_envvars_command(&bot, chat_id, &state).await?;
        }
    } else if text.starts_with("/usechrome") {
        msg_debug("[handle_message] routing → /usechrome");
        println!("  [{timestamp}] ◀ [{user_name}] /usechrome");
        handle_usechrome_command(&bot, chat_id, &state, token).await?;
    } else if text.starts_with("/silent") {
        msg_debug("[handle_message] routing → /silent");
        println!("  [{timestamp}] ◀ [{user_name}] /silent");
        handle_silent_command(&bot, chat_id, &state, token).await?;
    } else if text.starts_with("/queue") {
        msg_debug("[handle_message] routing → /queue");
        println!("  [{timestamp}] ◀ [{user_name}] /queue");
        handle_queue_command(&bot, chat_id, &state, token).await?;
    } else if text.starts_with("/direct") {
        msg_debug("[handle_message] routing → /direct");
        println!("  [{timestamp}] ◀ [{user_name}] /direct");
        handle_direct_command(&bot, chat_id, &msg, &state, token, is_owner).await?;
    } else if text.starts_with("/contextlevel") {
        msg_debug("[handle_message] routing → /contextlevel");
        println!("  [{timestamp}] ◀ [{user_name}] /contextlevel {}", text.strip_prefix("/contextlevel").unwrap_or("").trim());
        handle_contextlevel_command(&bot, chat_id, &text, &state, token, is_group_chat).await?;
    } else if text.starts_with("/query") {
        let body = text.strip_prefix("/query").unwrap_or("").trim();
        if body.is_empty() {
            msg_debug("[handle_message] /query with empty body, ignoring");
            shared_rate_limit_wait(&state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "Usage: /query <message>\nExample: /query@botname hello").await)?;
        } else {
            msg_debug(&format!("[handle_message] routing → text_message (/query), body={:?}", truncate_str(body, 80)));
            println!("  [{timestamp}] ◀ [{user_name}] {body}");
            handle_text_message(&bot, chat_id, body, &state, &user_name, false).await?;
        }
    } else if text.starts_with("/loop") {
        // Strip /loop prefix and handle @botname suffix (e.g. /loop@botname request)
        let body = text.strip_prefix("/loop").unwrap_or("");
        let body = if body.starts_with('@') {
            body.find(' ').map(|i| &body[i..]).unwrap_or("").trim()
        } else {
            body.trim()
        };
        if body.is_empty() {
            msg_debug("[handle_message] /loop with empty body");
            shared_rate_limit_wait(&state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "Usage: /loop [N] <request>\nRepeats until the task is fully completed.\nOptional: N = max iterations (default 5, 0 = unlimited)").await)?;
        } else {
            // /loop uses a self-verification step that currently supports
            // Claude (native --fork-session), Codex (ephemeral exec over the
            // full-fidelity session archive), and OpenCode (native --fork with
            // the `plan` agent). Other providers (currently Gemini) are rejected.
            let provider = { let _m = get_model(&state.lock().await.settings, chat_id); provider_from_model(_m.as_deref()).to_string() };
            if provider != "claude" && provider != "codex" && provider != "opencode" {
                shared_rate_limit_wait(&state, chat_id).await;
                tg!("send_message", bot.send_message(chat_id, "/loop currently supports Claude, Codex, or OpenCode models only.").await)?;
            } else {
                // Reject if a loop is already running for this chat
                {
                    let data = state.lock().await;
                    if data.loop_states.contains_key(&chat_id) {
                        drop(data);
                        shared_rate_limit_wait(&state, chat_id).await;
                        tg!("send_message", bot.send_message(chat_id, "A loop is already running. Use /stop to cancel it first.").await)?;
                        return Ok(());
                    }
                }
                // Parse optional iteration count: /loop 10 request → 10 iterations, /loop 0 request → unlimited
                let (max_iter, request) = {
                    let mut parts = body.splitn(2, ' ');
                    let first = parts.next().unwrap_or("");
                    let rest = parts.next().unwrap_or("").trim();
                    if !rest.is_empty() {
                        if let Ok(n) = first.parse::<u16>() {
                            (n, rest)
                        } else {
                            (LOOP_MAX_ITERATIONS, body)
                        }
                    } else {
                        (LOOP_MAX_ITERATIONS, body)
                    }
                };
                msg_debug(&format!("[handle_message] routing → /loop, max_iter={}, request={:?}", max_iter, truncate_str(request, 80)));
                println!("  [{timestamp}] ◀ [{user_name}] /loop {} {}", max_iter, truncate_str(request, 60));
                {
                    let mut data = state.lock().await;
                    data.loop_states.insert(chat_id, LoopState {
                        original_request: request.to_string(),
                        max_iterations: max_iter,
                        remaining: max_iter,
                    });
                }
                shared_rate_limit_wait(&state, chat_id).await;
                let iter_label = if max_iter == 0 {
                    "🔄 Loop started (unlimited)".to_string()
                } else {
                    format!("🔄 Loop started (max {} iterations)", max_iter)
                };
                let _ = tg!("send_message", bot.send_message(chat_id, &iter_label).await);
                handle_text_message(&bot, chat_id, request, &state, &user_name, false).await?;
            }
        }
    } else if text.starts_with("/setendhook_clear") {
        msg_debug("[handle_message] routing → /setendhook_clear");
        println!("  [{timestamp}] ◀ [{user_name}] /setendhook_clear");
        handle_setendhook_clear_command(&bot, chat_id, &state, token).await?;
    } else if text.starts_with("/setendhook") {
        msg_debug("[handle_message] routing → /setendhook");
        println!("  [{timestamp}] ◀ [{user_name}] /setendhook");
        handle_setendhook_command(&bot, chat_id, &text, &state, token).await?;
    } else if text.starts_with("/instruction_clear") {
        msg_debug("[handle_message] routing → /instruction_clear");
        println!("  [{timestamp}] ◀ [{user_name}] /instruction_clear");
        handle_instruction_clear_command(&bot, chat_id, &state, token).await?;
    } else if text.starts_with("/instruction") {
        msg_debug("[handle_message] routing → /instruction");
        println!("  [{timestamp}] ◀ [{user_name}] /instruction");
        handle_instruction_command(&bot, chat_id, &text, &state, token).await?;
    } else if text.starts_with("/allowed") {
        msg_debug("[handle_message] routing → /allowed");
        println!("  [{timestamp}] ◀ [{user_name}] /allowed {}", text.strip_prefix("/allowed").unwrap_or("").trim());
        { let _m = get_model(&state.lock().await.settings, chat_id);
        if provider_from_model(_m.as_deref()) != "claude" {
            tg!("send_message", bot.send_message(chat_id, "Tool permissions are not supported in this mode.").await)?;
        } else {
            handle_allowed_command(&bot, chat_id, &text, &state, token).await?;
        } }
    } else if text.starts_with('/') && is_workspace_id(text[1..].split_whitespace().next().unwrap_or("")) {
        let workspace_id = text[1..].split_whitespace().next().unwrap();
        msg_debug(&format!("[handle_message] routing → workspace_resume: {}", workspace_id));
        println!("  [{timestamp}] ◀ [{user_name}] /{workspace_id}");
        handle_workspace_resume(&bot, chat_id, workspace_id, &state, token).await?;
    } else if text.starts_with('!') {
        msg_debug(&format!("[handle_message] routing → shell command"));
        println!("  [{timestamp}] ◀ [{user_name}] Shell: {preview}");
        handle_shell_command(&bot, chat_id, &text, &state, &user_name).await?;
    } else if text.starts_with(';') {
        let stripped = text.strip_prefix(';').unwrap_or(&text).trim().to_string();
        if stripped.is_empty() {
            msg_debug("[handle_message] ;prefix with empty body, ignoring");
            return Ok(());
        }
        let preview = &stripped;
        msg_debug(&format!("[handle_message] routing → text_message (;prefix), stripped={:?}", truncate_str(&stripped, 80)));
        println!("  [{timestamp}] ◀ [{user_name}] {preview}");
        handle_text_message(&bot, chat_id, &stripped, &state, &user_name, false).await?;
    } else {
        msg_debug(&format!("[handle_message] routing → text_message (plain), require_prefix={}", require_prefix));
        println!("  [{timestamp}] ◀ [{user_name}] {preview}");
        handle_text_message(&bot, chat_id, &text, &state, &user_name, false).await?;
    }

    Ok(())
}

/// Handle /help command
async fn handle_help_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let platform = capitalize_platform(detect_platform(bot.token()));
    let help = format!("\
<b>cokacdir {} Bot</b>
Manage server files &amp; chat with Claude AI.

<b>Session</b>
<code>/start &lt;path&gt;</code> — Start session at directory
<code>/start &lt;name|id&gt;</code> — Resume Claude Code session
<code>/start</code> — Start with auto-generated workspace
<code>/pwd</code> — Show current working directory
<code>/session</code> — Show current session ID
<code>/clear</code> — Clear AI conversation history
<code>/stop</code> — Stop current AI request
<code>/stop_&lt;ID&gt;</code> — Cancel a specific queued message
<code>/stopall</code> — Stop request and clear queue
<code>/queue</code> — Toggle queue mode (queue messages while AI is busy)
<code>/loop [N] &lt;request&gt;</code> — Repeat until task is fully completed

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
<code>/query</code><i> message</i> — Send message to AI (supports @bot)
<code>;</code><i>caption</i> — Upload file with AI prompt
<code>/public on</code> — Allow all members to use bot
<code>/public off</code> — Owner only (default)
<code>/direct</code> — Toggle direct mode (no ; prefix needed)
<code>/contextlevel &lt;N&gt;</code> — Set group chat log entries in prompt (0=off, default 12)

<b>Schedule</b>
Ask in natural language to manage schedules.

<b>Settings</b>
<code>/model</code> — Show current AI model
<code>/model &lt;name&gt;</code> — Set model (claude/codex/gemini or provider:model)
<code>/setpollingtime &lt;ms&gt;</code> — Set API polling interval
  Too low may cause API rate limits.
  Minimum 2500ms, recommended 3000ms+.
<code>/debug</code> — Toggle debug logging
<code>/envvars</code> — Show all environment variables and their current values
<code>/silent</code> — Toggle silent mode (hide tool calls)
<code>/usechrome</code> — Toggle Chrome browser for Claude (--chrome)
<code>/instruction &lt;text&gt;</code> — Set system instruction for AI
<code>/instruction</code> — View current instruction
<code>/instruction_clear</code> — Clear instruction
<code>/setendhook &lt;msg&gt;</code> — Set notification when processing ends
<code>/setendhook</code> — View current end hook
<code>/setendhook_clear</code> — Clear end hook

<b>Bot Messaging</b>
Bots in the same group can collaborate via <code>/instruction</code>.

<code>/help</code> — Show this help", platform);

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
    msg_debug(&format!("[handle_start_command] chat_id={}, path_str={:?}", chat_id.0, path_str));

    // Determine current provider (Claude vs Codex vs Gemini)
    let original_provider_str: &str;
    let mut provider_str;
    let mut provider = {
        let data = state.lock().await;
        let model = get_model(&data.settings, chat_id);
        let p = detect_provider(model.as_deref());
        msg_debug(&format!("[handle_start_command] model={:?}, provider={}", model, p));
        provider_str = p;
        provider_to_session(p)
    };
    original_provider_str = provider_str;

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
    } else if path_str.starts_with('/')
        || path_str.starts_with("~/") || path_str.starts_with("~\\") || path_str == "~"
        || path_str.starts_with("./") || path_str.starts_with(".\\")
        || path_str == "." || path_str == ".."
        || path_str.starts_with("../") || path_str.starts_with("..\\")
        || (path_str.len() >= 3 && path_str.as_bytes()[1] == b':' && (path_str.as_bytes()[2] == b'\\' || path_str.as_bytes()[2] == b'/'))
    {
        // Path mode: expand ~ and validate
        let expanded = if path_str.starts_with("~/") || path_str.starts_with("~\\") || path_str == "~" {
            if let Some(home) = dirs::home_dir() {
                home.join(path_str.strip_prefix("~/").or_else(|| path_str.strip_prefix("~\\")).unwrap_or("")).display().to_string()
            } else {
                path_str.to_string()
            }
        } else {
            path_str.to_string()
        };
        let path = Path::new(&expanded);
        if !path.exists() {
            if let Err(e) = fs::create_dir_all(&path) {
                shared_rate_limit_wait(state, chat_id).await;
                tg!("send_message", bot.send_message(chat_id, format!("Error: failed to create '{}': {}", expanded, e))
                    .await)?;
                return Ok(());
            }
        } else if !path.is_dir() {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, format!("Error: '{}' is not a directory.", expanded))
                .await)?;
            return Ok(());
        }
        path.canonicalize()
            .map(crate::utils::format::strip_unc_prefix)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| expanded)
    } else {
        // Session name/ID mode: resolve Claude Code session
        // Try current provider first, then cross-provider fallback
        let fallback_providers: &[SessionProvider] = match provider {
            SessionProvider::Claude   => &[SessionProvider::Codex, SessionProvider::Gemini, SessionProvider::OpenCode],
            SessionProvider::Codex    => &[SessionProvider::Claude, SessionProvider::Gemini, SessionProvider::OpenCode],
            SessionProvider::Gemini   => &[SessionProvider::Claude, SessionProvider::Codex, SessionProvider::OpenCode],
            SessionProvider::OpenCode => &[SessionProvider::Claude, SessionProvider::Codex, SessionProvider::Gemini],
        };

        // Helper closure: try resolve_session + ai_sessions for a given provider
        let try_resolve = |prov: SessionProvider, prov_str: &str| -> Option<String> {
            msg_debug(&format!("[try_resolve] provider={}, query={:?}", prov_str, path_str));
            if let Some(info) = resolve_session(path_str, prov) {
                let path = Path::new(&info.cwd);
                let is_dir = path.is_dir();
                msg_debug(&format!("[try_resolve] resolve_session found: cwd={:?}, is_dir={}", info.cwd, is_dir));
                if is_dir {
                    let canonical = path.canonicalize()
                        .map(crate::utils::format::strip_unc_prefix)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| info.cwd.clone());
                    convert_and_save_session(&info, &canonical);
                    msg_debug(&format!("[try_resolve] resolved via resolve_session: canonical={}", canonical));
                    return Some(canonical);
                } else {
                    msg_debug(&format!("[try_resolve] resolve_session cwd not a dir: {:?}", info.cwd));
                }
            } else {
                msg_debug(&format!("[try_resolve] resolve_session returned None for provider={}", prov_str));
            }
            // Try ai_sessions/{id}.json
            msg_debug(&format!("[try_resolve] trying ai_sessions/{}.json", path_str));
            let result = ai_screen::ai_sessions_dir().and_then(|dir| {
                let file = dir.join(format!("{}.json", path_str));
                let content = fs::read_to_string(&file).ok()?;
                let sd: SessionData = serde_json::from_str(&content).ok()?;
                if !sd.provider.is_empty() && sd.provider != prov_str {
                    msg_debug(&format!("[try_resolve] ai_sessions/{}.json provider mismatch: {} != {}", path_str, sd.provider, prov_str));
                    return None;
                }
                let p = Path::new(&sd.current_path);
                let is_dir = p.is_dir();
                msg_debug(&format!("[try_resolve] ai_sessions/{}.json: current_path={:?}, provider={}, is_dir={}", path_str, sd.current_path, sd.provider, is_dir));
                if is_dir { Some(sd.current_path.clone()) } else { None }
            });
            msg_debug(&format!("[try_resolve] ai_sessions result: {}", if result.is_some() { "found" } else { "None" }));
            result
        };

        if let Some(cp) = try_resolve(provider, provider_str) {
            msg_debug(&format!("[handle_start_command] resolved with current provider: path={}", cp));
            cp
        } else {
            // Cross-provider fallback: try all other providers in order
            let mut cross_result: Option<(String, SessionProvider, &'static str)> = None;
            for &fp in fallback_providers {
                let fp_str = session_provider_str(fp);
                let available = match fp {
                    SessionProvider::Claude => claude::is_claude_available(),
                    SessionProvider::Codex => codex::is_codex_available(),
                    SessionProvider::Gemini => gemini::is_gemini_available(),
                    SessionProvider::OpenCode => opencode::is_opencode_available(),
                };
                if !available {
                    msg_debug(&format!("[handle_start_command] cross-provider skip: {} not available", fp_str));
                    continue;
                }
                msg_debug(&format!("[handle_start_command] cross-provider attempt: {}", fp_str));
                if let Some(cp) = try_resolve(fp, fp_str) {
                    cross_result = Some((cp, fp, fp_str));
                    break;
                }
            }

            if let Some((cp, resolved_prov, resolved_prov_str)) = cross_result {
                // Cross-provider fallback: switch provider and model to resolved provider
                msg_debug(&format!("[handle_start_command] cross-provider fallback: switching from {} to {}", provider_str, resolved_prov_str));
                provider = resolved_prov;
                provider_str = resolved_prov_str;
                {
                    let mut data = state.lock().await;
                    msg_debug(&format!("[handle_start_command] cross-provider: setting model to {:?}", resolved_prov_str));
                    data.settings.models.insert(chat_id.0.to_string(), resolved_prov_str.to_string());
                    save_bot_settings(token, &data.settings);
                    if let Some(session) = data.sessions.get_mut(&chat_id) {
                        msg_debug(&format!("[handle_start_command] cross-provider: clearing old session (len={}, sid={:?}, path={:?})",
                            session.history.len(), session.session_id, session.current_path));
                        session.session_id = None;
                        session.current_path = None;
                        session.history.clear();
                    } else {
                        msg_debug("[handle_start_command] cross-provider: no existing session to clear");
                    }
                }
                cp
            } else {
                // Final fallback: try as plain path
                msg_debug(&format!("[handle_start_command] all session resolves failed, trying as plain path: {:?}", path_str));
                let path = Path::new(path_str);
                if path.exists() && path.is_dir() {
                    let resolved = path.canonicalize()
                        .map(crate::utils::format::strip_unc_prefix)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| path_str.to_string());
                    msg_debug(&format!("[handle_start_command] plain path resolved: {}", resolved));
                    resolved
                } else {
                    msg_debug(&format!("[handle_start_command] plain path failed: exists={}, is_dir={}", path.exists(), path.is_dir()));
                    shared_rate_limit_wait(state, chat_id).await;
                    tg!("send_message", bot.send_message(chat_id, format!("Error: no session or directory found for '{}'.", path_str))
                        .await)?;
                    return Ok(());
                }
            }
        }
    };

    // Try to load existing session for this path
    msg_debug(&format!("[handle_start_command] canonical_path={:?}, provider={}", canonical_path, provider_str));
    let existing = load_existing_session(&canonical_path, provider_str);
    msg_debug(&format!("[handle_start_command] load_existing_session → {}", if existing.is_some() { "found" } else { "None" }));

    // If no local session, try converting the latest external session for this path
    let existing = if existing.is_some() {
        existing
    } else if let Some(info) = find_latest_session_by_cwd(&canonical_path, provider) {
        msg_debug(&format!("[handle_start_command] fallback found: jsonl={}, session_id={}", info.jsonl_path.display(), info.session_id));
        convert_and_save_session(&info, &canonical_path);
        let reloaded = load_existing_session(&canonical_path, provider_str);
        msg_debug(&format!("[handle_start_command] after convert, reload → {}", if reloaded.is_some() { "found" } else { "None" }));
        reloaded
    } else {
        msg_debug("[handle_start_command] no external session found either");
        None
    };

    let mut response_lines = Vec::new();
    let mut is_restored = false;

    // Notify user if provider was auto-switched via cross-provider fallback
    if provider_str != original_provider_str {
        msg_debug(&format!("[handle_start_command] provider auto-switched: {} → {}", original_provider_str, provider_str));
        response_lines.push(format!("Model switched to **{}**.", provider_str));
    }

    {
        let mut data = state.lock().await;
        let session = data.sessions.entry(chat_id).or_insert_with(|| ChatSession {
            session_id: None,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
        });
        session.session_id = None; // Clear stale session_id from previous workspace

        if let Some((session_data, _)) = &existing {
            if !session_data.session_id.is_empty() {
                session.session_id = Some(session_data.session_id.clone());
            } else {
                cleanup_session_files(&canonical_path, provider_str);
            }
            session.current_path = Some(canonical_path.clone());
            session.history = session_data.history.clone();
            is_restored = true;
            msg_debug(&format!("[handle_start_command] restored: session_id={}, path={}, history_len={}",
                session_data.session_id, canonical_path, session_data.history.len()));

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Session restored: {canonical_path}");
            response_lines.push(format!("[{}] Session restored at `{}`.", provider_str, canonical_path));
            if let Some(folder_name) = std::path::Path::new(&canonical_path).file_name().and_then(|n| n.to_str()) {
                if is_workspace_id(folder_name)
                    && dirs::home_dir()
                        .map(|h| h.join(".cokacdir").join("workspace").join(folder_name).is_dir())
                        .unwrap_or(false)
                {
                    response_lines.push(format!("Use /{} to resume this session.", folder_name));
                }
            }
            let header_len: usize = response_lines.iter().map(|l| l.len() + 1).sum();
            let remaining = TELEGRAM_MSG_LIMIT.saturating_sub(header_len + 2);
            let preview = build_history_preview(&session_data.history, remaining);
            if !preview.is_empty() {
                response_lines.push(String::new());
                response_lines.push(preview);
            }
        } else {
            session.session_id = None;
            session.current_path = Some(canonical_path.clone());
            session.history.clear();
            msg_debug(&format!("[handle_start_command] new session: path={}", canonical_path));

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Session started: {canonical_path}");
            response_lines.push(format!("[{}] Session started at `{}`.", provider_str, canonical_path));
            // Show workspace ID shortcut if this is a workspace directory
            if let Some(folder_name) = std::path::Path::new(&canonical_path).file_name().and_then(|n| n.to_str()) {
                if is_workspace_id(folder_name)
                    && dirs::home_dir()
                        .map(|h| h.join(".cokacdir").join("workspace").join(folder_name).is_dir())
                        .unwrap_or(false)
                {
                    response_lines.push(format!("Use /{} to resume this session.", folder_name));
                }
            }
        }
    }

    // Persist chat_id → path mapping for auto-restore after restart
    {
        let mut data = state.lock().await;
        data.settings.last_sessions.insert(chat_id.0.to_string(), canonical_path.clone());
        save_bot_settings(token, &data.settings);
    }

    // Append start marker to group chat log so other bots can see the new working directory
    if chat_id.0 < 0 {
        let (uname, dname) = {
            let data = state.lock().await;
            (data.bot_username.clone(), data.bot_display_name.clone())
        };
        if !uname.is_empty() {
            let dn = if dname.is_empty() { None } else { Some(dname) };
            append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                ts: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                bot: uname,
                bot_display_name: dn,
                role: "system".to_string(),
                from: None,
                text: if is_restored {
                    format!("Session restored at {}", canonical_path)
                } else {
                    format!("Session started at {}", canonical_path)
                },
                clear: false,
            });
        }
    }

    let response_text = response_lines.join("\n");
    let html = markdown_to_telegram_html(&response_text);
    send_long_message(bot, chat_id, &html, Some(ParseMode::Html), state).await?;

    Ok(())
}

/// Build a history preview code block that fits within the given byte budget.
/// Items are shown oldest-first (most recent at bottom), filling from the bottom up.
fn build_history_preview(history: &[HistoryItem], budget: usize) -> String {
    if history.is_empty() {
        return String::new();
    }
    let code_block_overhead = "```\n".len() + "\n```".len(); // 8 bytes
    if budget <= code_block_overhead + 10 {
        return String::new();
    }
    let content_budget = budget - code_block_overhead;

    // Build lines from newest to oldest, stop when budget exhausted
    let mut collected: Vec<String> = Vec::new();
    let mut used = 0;
    for item in history.iter().rev() {
        let prefix = match item.item_type {
            HistoryType::User => "👤",
            HistoryType::Assistant => "🤖",
            HistoryType::Error => "",
            HistoryType::System => "⚙️",
            HistoryType::ToolUse => "🔧",
            HistoryType::ToolResult => "📋",
        };
        let line = format!("{} {}", prefix, item.content);
        let line_len = line.len() + 1; // +1 for newline
        if used + line_len > content_budget {
            break;
        }
        collected.push(line);
        used += line_len;
    }
    if collected.is_empty() {
        return String::new();
    }
    collected.reverse();
    format!("```\n{}\n```", collected.join("\n"))
}

/// Check if a string is a valid 8-character workspace ID (e.g. "B4E9451D" or "k3m9x2ab")
fn is_workspace_id(s: &str) -> bool {
    s.len() == 8 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Check if a string is a valid UUID (8-4-4-4-12 hex format)
fn is_uuid(s: &str) -> bool {
    if s.len() != 36 { return false; }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 { return false; }
    let expected = [8, 4, 4, 4, 12];
    parts.iter().zip(expected.iter()).all(|(p, &len)| {
        p.len() == len && p.chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// Provider that owns the resolved session.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SessionProvider { Claude, Codex, Gemini, OpenCode }

/// Detect provider from model prefix only (no availability fallback).
/// Returns "claude" when model is None or has no recognized prefix.
fn provider_from_model(model: Option<&str>) -> &'static str {
    if codex::is_codex_model(model) { "codex" }
    else if gemini::is_gemini_model(model) { "gemini" }
    else if opencode::is_opencode_model(model) { "opencode" }
    else { "claude" }
}

/// Detect effective provider: from model prefix if set, otherwise from CLI availability.
fn detect_provider(model: Option<&str>) -> &'static str {
    if model.is_some() {
        provider_from_model(model)
    } else if !claude::is_claude_available() && codex::is_codex_available() {
        "codex"
    } else if !claude::is_claude_available() && gemini::is_gemini_available() {
        "gemini"
    } else if !claude::is_claude_available() && opencode::is_opencode_available() {
        "opencode"
    } else {
        "claude"
    }
}

/// Convert provider name to SessionProvider enum.
fn provider_to_session(provider: &str) -> SessionProvider {
    match provider {
        "codex" => SessionProvider::Codex,
        "gemini" => SessionProvider::Gemini,
        "opencode" => SessionProvider::OpenCode,
        _ => SessionProvider::Claude,
    }
}

/// Convert SessionProvider enum to provider name string.
fn session_provider_str(provider: SessionProvider) -> &'static str {
    match provider {
        SessionProvider::Claude => "claude",
        SessionProvider::Codex => "codex",
        SessionProvider::Gemini => "gemini",
        SessionProvider::OpenCode => "opencode",
    }
}

/// Info returned when an external session is resolved.
struct ResolvedSession {
    cwd: String,
    jsonl_path: std::path::PathBuf,
    session_id: String,
    provider: SessionProvider,
}

/// Resolve a session by name or ID, scoped to the current provider.
fn resolve_session(query: &str, provider: SessionProvider) -> Option<ResolvedSession> {
    msg_debug(&format!("[resolve_session] query={:?}, provider={:?}, is_uuid={}", query, provider, is_uuid(query)));
    let result = match provider {
        SessionProvider::Claude => {
            if is_uuid(query) {
                resolve_claude_by_id(query).or_else(|| resolve_claude_by_name(query))
            } else {
                resolve_claude_by_name(query).or_else(|| resolve_claude_by_id(query))
            }
        }
        SessionProvider::Codex => {
            resolve_codex_by_id(query)
        }
        SessionProvider::Gemini => resolve_gemini_by_id(query),
        SessionProvider::OpenCode => resolve_opencode_by_id(query),
    };
    msg_debug(&format!("[resolve_session] result={}", match &result {
        Some(r) => format!("found(cwd={:?}, session_id={})", r.cwd, r.session_id),
        None => "None".to_string(),
    }));
    result
}

/// Claude: find `~/.claude/projects/*/{session_id}.jsonl`.
fn resolve_claude_by_id(session_id: &str) -> Option<ResolvedSession> {
    msg_debug(&format!("[resolve_claude_by_id] session_id={}", session_id));
    let projects_dir = dirs::home_dir()?.join(".claude").join("projects");
    if !projects_dir.is_dir() {
        msg_debug(&format!("[resolve_claude_by_id] projects_dir not found: {}", projects_dir.display()));
        return None;
    }
    let filename = format!("{}.jsonl", session_id);
    for entry in fs::read_dir(&projects_dir).ok()?.flatten() {
        if !entry.file_type().map_or(false, |t| t.is_dir()) { continue; }
        let jsonl_path = entry.path().join(&filename);
        if jsonl_path.exists() {
            msg_debug(&format!("[resolve_claude_by_id] found: {}", jsonl_path.display()));
            let cwd = extract_cwd_from_jsonl(&jsonl_path)?;
            return Some(ResolvedSession {
                cwd, jsonl_path,
                session_id: session_id.to_string(),
                provider: SessionProvider::Claude,
            });
        }
    }
    msg_debug("[resolve_claude_by_id] not found");
    None
}

/// Claude: scan `~/.claude/projects/*/*.jsonl` for matching `custom-title`.
fn resolve_claude_by_name(name: &str) -> Option<ResolvedSession> {
    msg_debug(&format!("[resolve_claude_by_name] name={:?}", name));
    let projects_dir = dirs::home_dir()?.join(".claude").join("projects");
    if !projects_dir.is_dir() {
        msg_debug(&format!("[resolve_claude_by_name] projects_dir not found: {}", projects_dir.display()));
        return None;
    }
    let name_lower = name.to_lowercase();
    for proj_entry in fs::read_dir(&projects_dir).ok()?.flatten() {
        if !proj_entry.file_type().map_or(false, |t| t.is_dir()) { continue; }
        let Ok(file_entries) = fs::read_dir(proj_entry.path()) else { continue; };
        for file_entry in file_entries.flatten() {
            let path = file_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            if let Some(info) = find_session_by_title(&path, &name_lower) {
                msg_debug(&format!("[resolve_claude_by_name] found: session_id={}, cwd={:?}", info.session_id, info.cwd));
                return Some(info);
            }
        }
    }
    msg_debug("[resolve_claude_by_name] not found");
    None
}

/// Claude: check if a JSONL file contains a matching custom-title.
fn find_session_by_title(path: &Path, name_lower: &str) -> Option<ResolvedSession> {
    use std::io::{BufRead, BufReader};
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut matched = false;
    let mut cwd_found: Option<String> = None;
    for line in reader.lines().flatten() {
        if cwd_found.is_none() && line.contains("\"cwd\"") {
            if let Some(cwd) = extract_json_string_field(&line, "cwd") {
                if !cwd.is_empty() {
                    cwd_found = Some(cwd);
                }
            }
        }
        if !matched && line.contains("custom-title") {
            if let Some(title) = extract_json_string_field(&line, "customTitle") {
                if title.to_lowercase() == name_lower {
                    matched = true;
                }
            }
        }
        if matched && cwd_found.is_some() { break; }
    }
    if matched {
        let cwd = cwd_found?;
        let session_id = path.file_stem()?.to_str()?.to_string();
        Some(ResolvedSession {
            cwd, jsonl_path: path.to_path_buf(), session_id,
            provider: SessionProvider::Claude,
        })
    } else {
        None
    }
}

/// Codex: recursively scan `~/.codex/sessions/` for a JSONL whose filename contains the UUID.
fn resolve_codex_by_id(session_id: &str) -> Option<ResolvedSession> {
    let sessions_dir = dirs::home_dir()?.join(".codex").join("sessions");
    if !sessions_dir.is_dir() { return None; }
    let suffix = format!("{}.jsonl", session_id);
    fn walk(dir: &Path, suffix: &str) -> Option<std::path::PathBuf> {
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let Ok(ft) = entry.file_type() else { continue; };
            if ft.is_dir() {
                if let Some(found) = walk(&entry.path(), suffix) {
                    return Some(found);
                }
            } else if ft.is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.ends_with(suffix) {
                        return Some(entry.path());
                    }
                }
            }
        }
        None
    }
    let jsonl_path = walk(&sessions_dir, &suffix)?;
    let cwd = extract_cwd_from_jsonl(&jsonl_path)?;
    Some(ResolvedSession {
        cwd, jsonl_path,
        session_id: session_id.to_string(),
        provider: SessionProvider::Codex,
    })
}

/// Gemini: scan `~/.gemini/tmp/*/chats/session-*.json` for a matching sessionId.
fn resolve_gemini_by_id(session_id: &str) -> Option<ResolvedSession> {
    msg_debug(&format!("[resolve_gemini_by_id] session_id={}", session_id));
    let tmp_dir = dirs::home_dir()?.join(".gemini").join("tmp");
    if !tmp_dir.is_dir() {
        msg_debug("[resolve_gemini_by_id] tmp_dir not found");
        return None;
    }
    // Use first 8 chars of UUID for quick filename filtering (char-boundary safe)
    let short_id: String = session_id.chars().take(8).collect();
    for proj_entry in fs::read_dir(&tmp_dir).ok()?.flatten() {
        if !proj_entry.file_type().map_or(false, |t| t.is_dir()) { continue; }
        let chats_dir = proj_entry.path().join("chats");
        if !chats_dir.is_dir() { continue; }
        let Ok(chat_entries) = fs::read_dir(&chats_dir) else { continue; };
        for chat_entry in chat_entries.flatten() {
            let path = chat_entry.path();
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else { continue; };
            if !fname.starts_with("session-") || !fname.ends_with(".json") { continue; }
            // Quick filter: filename contains first 8 chars of UUID
            if !fname.contains(short_id.as_str()) { continue; }
            // Parse JSON to verify full sessionId
            let Ok(content) = fs::read_to_string(&path) else { continue; };
            let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else { continue; };
            let sid = val.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            if sid != session_id { continue; }
            // Read .project_root from parent directory to get cwd
            let project_root_file = proj_entry.path().join(".project_root");
            let cwd = fs::read_to_string(&project_root_file).ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())?;
            msg_debug(&format!("[resolve_gemini_by_id] found: cwd={:?}, file={}", cwd, path.display()));
            return Some(ResolvedSession {
                cwd, jsonl_path: path, session_id: session_id.to_string(),
                provider: SessionProvider::Gemini,
            });
        }
    }
    msg_debug("[resolve_gemini_by_id] not found");
    None
}

/// OpenCode: query `~/.local/share/opencode/opencode.db` for a matching session ID.
fn resolve_opencode_by_id(session_id: &str) -> Option<ResolvedSession> {
    msg_debug(&format!("[resolve_opencode_by_id] session_id={}", session_id));
    let db_path = dirs::home_dir()?.join(".local").join("share").join("opencode").join("opencode.db");
    if !db_path.is_file() {
        msg_debug("[resolve_opencode_by_id] db not found");
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        &db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ).ok()?;
    let mut stmt = conn.prepare("SELECT id, directory FROM session WHERE id = ?1 LIMIT 1").ok()?;
    let result = stmt.query_row(rusqlite::params![session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }).ok()?;
    let directory = result.1;
    if directory.is_empty() { return None; }
    msg_debug(&format!("[resolve_opencode_by_id] found: directory={:?}", directory));
    Some(ResolvedSession {
        cwd: directory,
        jsonl_path: db_path,
        session_id: session_id.to_string(),
        provider: SessionProvider::OpenCode,
    })
}

/// Convert an external JSONL session to cokacdir SessionData and save it.
/// Re-converts if the source JSONL is newer than the existing JSON.
fn convert_and_save_session(info: &ResolvedSession, canonical_path: &str) {
    msg_debug(&format!("[convert_session] start: jsonl={}, session_id={}, canonical_path={:?}",
        info.jsonl_path.display(), info.session_id, canonical_path));
    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        msg_debug("[convert_session] ai_sessions_dir() returned None");
        return;
    };
    let target = sessions_dir.join(format!("{}.json", info.session_id));
    msg_debug(&format!("[convert_session] target={}", target.display()));
    if target.exists() {
        let source_mtime = info.jsonl_path.metadata().ok().and_then(|m| m.modified().ok());
        let target_mtime = target.metadata().ok().and_then(|m| m.modified().ok());
        msg_debug(&format!("[convert_session] target exists, source_mtime={:?}, target_mtime={:?}", source_mtime, target_mtime));
        if let (Some(src), Some(tgt)) = (source_mtime, target_mtime) {
            if src <= tgt {
                msg_debug("[convert_session] skipped: target is up-to-date");
                return;
            }
        } else {
            msg_debug("[convert_session] skipped: cannot compare mtimes");
            return;
        }
    }

    let parser = match info.provider {
        SessionProvider::Claude => parse_claude_jsonl,
        SessionProvider::Codex  => parse_codex_jsonl,
        SessionProvider::Gemini => parse_gemini_json,
        SessionProvider::OpenCode => parse_opencode_session,
    };
    msg_debug(&format!("[convert_session] parsing with provider={:?}", info.provider));
    let Some(session_data) = parser(&info.jsonl_path, &info.session_id, canonical_path) else {
        msg_debug("[convert_session] parser returned None");
        return;
    };
    msg_debug(&format!("[convert_session] parsed: history_len={}, provider={}", session_data.history.len(), session_data.provider));
    let _ = fs::create_dir_all(&sessions_dir);
    if let Ok(json) = serde_json::to_string_pretty(&session_data) {
        let write_result = fs::write(&target, &json);
        msg_debug(&format!("[convert_session] write result={:?}, bytes={}", write_result, json.len()));
    } else {
        msg_debug("[convert_session] serde_json::to_string_pretty failed");
    }

    crate::services::session_archive::archive_and_save_session(
        session_provider_str(info.provider),
        &info.jsonl_path,
        &info.session_id,
        canonical_path,
    );
}

/// Find the most recently modified external session whose cwd matches the given path.
fn find_latest_session_by_cwd(canonical_path: &str, provider: SessionProvider) -> Option<ResolvedSession> {
    msg_debug(&format!("[find_latest_by_cwd] canonical_path={:?}, provider={:?}", canonical_path, provider));
    let result = match provider {
        SessionProvider::Claude => find_latest_claude_by_cwd(canonical_path),
        SessionProvider::Codex  => find_latest_codex_by_cwd(canonical_path),
        SessionProvider::Gemini => find_latest_gemini_by_cwd(canonical_path),
        SessionProvider::OpenCode => find_latest_opencode_by_cwd(canonical_path),
    };
    msg_debug(&format!("[find_latest_by_cwd] result={}", if result.is_some() { "found" } else { "None" }));
    result
}

/// Claude: scan all `~/.claude/projects/*/*.jsonl` for the latest session matching cwd.
fn find_latest_claude_by_cwd(canonical_path: &str) -> Option<ResolvedSession> {
    let projects_dir = dirs::home_dir()?.join(".claude").join("projects");
    msg_debug(&format!("[find_claude_by_cwd] projects_dir={}, is_dir={}", projects_dir.display(), projects_dir.is_dir()));
    if !projects_dir.is_dir() { return None; }
    let mut best_path: Option<std::path::PathBuf> = None;
    let mut best_time = std::time::UNIX_EPOCH;
    let mut scan_count = 0u32;
    let mut cwd_mismatch_sample: Option<String> = None;
    for proj_entry in fs::read_dir(&projects_dir).ok()?.flatten() {
        if !proj_entry.file_type().map_or(false, |t| t.is_dir()) { continue; }
        let Ok(file_entries) = fs::read_dir(proj_entry.path()) else { continue; };
        for file_entry in file_entries.flatten() {
            let path = file_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            scan_count += 1;
            if let Some(cwd) = extract_cwd_from_jsonl(&path) {
                msg_debug(&format!("[find_claude_by_cwd] file={}, extracted_cwd={:?}, want={:?}, match={}",
                    path.display(), cwd, canonical_path, cwd == canonical_path));
                if cwd == canonical_path {
                    let mtime = path.metadata().ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    if mtime > best_time {
                        best_path = Some(path);
                        best_time = mtime;
                    }
                } else if cwd_mismatch_sample.is_none() {
                    cwd_mismatch_sample = Some(cwd);
                }
            } else {
                msg_debug(&format!("[find_claude_by_cwd] file={}, extract_cwd returned None", path.display()));
            }
        }
    }
    msg_debug(&format!("[find_claude_by_cwd] scanned {} jsonl files, best={:?}", scan_count, best_path.as_ref().map(|p| p.display().to_string())));
    if best_path.is_none() {
        if let Some(sample) = cwd_mismatch_sample {
            msg_debug(&format!("[find_claude_by_cwd] cwd mismatch example: extracted={:?} vs wanted={:?}", sample, canonical_path));
        }
    }
    let jsonl_path = best_path?;
    let session_id = jsonl_path.file_stem()?.to_str()?.to_string();
    Some(ResolvedSession {
        cwd: canonical_path.to_string(), jsonl_path, session_id,
        provider: SessionProvider::Claude,
    })
}

/// Codex: scan `~/.codex/sessions/**/*.jsonl` for the latest session matching cwd.
fn find_latest_codex_by_cwd(canonical_path: &str) -> Option<ResolvedSession> {
    let sessions_dir = dirs::home_dir()?.join(".codex").join("sessions");
    if !sessions_dir.is_dir() { return None; }
    let mut best_path: Option<std::path::PathBuf> = None;
    let mut best_time = std::time::UNIX_EPOCH;
    collect_best_codex_jsonl(&sessions_dir, canonical_path, &mut best_path, &mut best_time);
    let jsonl_path = best_path?;
    // Extract UUID from filename tail (last 36 chars of stem)
    let session_id = {
        let stem = jsonl_path.file_stem()?.to_str()?;
        if stem.len() < 36 { return None; }
        let candidate = &stem[stem.len() - 36..];
        if !is_uuid(candidate) { return None; }
        candidate.to_string()
    };
    Some(ResolvedSession {
        cwd: canonical_path.to_string(), jsonl_path, session_id,
        provider: SessionProvider::Codex,
    })
}

fn collect_best_codex_jsonl(
    dir: &Path, canonical_path: &str,
    best_path: &mut Option<std::path::PathBuf>, best_time: &mut std::time::SystemTime,
) {
    let Ok(entries) = fs::read_dir(dir) else { return; };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue; };
        if ft.is_dir() {
            collect_best_codex_jsonl(&entry.path(), canonical_path, best_path, best_time);
        } else if ft.is_file() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            if let Some(cwd) = extract_cwd_from_jsonl(&path) {
                if cwd == canonical_path {
                    let mtime = path.metadata().ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    if mtime > *best_time {
                        *best_path = Some(path);
                        *best_time = mtime;
                    }
                }
            }
        }
    }
}

/// Gemini: scan `~/.gemini/tmp/*/.project_root` for cwd match, find latest chat file.
fn find_latest_gemini_by_cwd(canonical_path: &str) -> Option<ResolvedSession> {
    msg_debug(&format!("[find_latest_gemini_by_cwd] canonical_path={:?}", canonical_path));
    let tmp_dir = dirs::home_dir()?.join(".gemini").join("tmp");
    if !tmp_dir.is_dir() { return None; }
    let mut best_path: Option<std::path::PathBuf> = None;
    let mut best_time = std::time::UNIX_EPOCH;
    for proj_entry in fs::read_dir(&tmp_dir).ok()?.flatten() {
        if !proj_entry.file_type().map_or(false, |t| t.is_dir()) { continue; }
        let pr_file = proj_entry.path().join(".project_root");
        let Ok(pr_content) = fs::read_to_string(&pr_file) else { continue; };
        if pr_content.trim() != canonical_path { continue; }
        let chats_dir = proj_entry.path().join("chats");
        if !chats_dir.is_dir() { continue; }
        let Ok(chat_entries) = fs::read_dir(&chats_dir) else { continue; };
        for chat_entry in chat_entries.flatten() {
            let path = chat_entry.path();
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else { continue; };
            if !fname.starts_with("session-") || !fname.ends_with(".json") { continue; }
            let mtime = path.metadata().ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            if mtime > best_time {
                best_path = Some(path);
                best_time = mtime;
            }
        }
    }
    let jsonl_path = best_path?;
    let content = fs::read_to_string(&jsonl_path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let session_id = val.get("sessionId").and_then(|v| v.as_str())?.to_string();
    msg_debug(&format!("[find_latest_gemini_by_cwd] found: session_id={}, file={}", session_id, jsonl_path.display()));
    Some(ResolvedSession {
        cwd: canonical_path.to_string(), jsonl_path, session_id,
        provider: SessionProvider::Gemini,
    })
}

/// OpenCode: query SQLite DB for latest session matching cwd.
fn find_latest_opencode_by_cwd(canonical_path: &str) -> Option<ResolvedSession> {
    msg_debug(&format!("[find_latest_opencode_by_cwd] canonical_path={:?}", canonical_path));
    let db_path = dirs::home_dir()?.join(".local").join("share").join("opencode").join("opencode.db");
    if !db_path.is_file() { return None; }
    let conn = rusqlite::Connection::open_with_flags(
        &db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ).ok()?;
    let mut stmt = conn.prepare(
        "SELECT id FROM session WHERE directory = ?1 ORDER BY time_updated DESC LIMIT 1"
    ).ok()?;
    let session_id: String = stmt.query_row(rusqlite::params![canonical_path], |row| {
        row.get(0)
    }).ok()?;
    msg_debug(&format!("[find_latest_opencode_by_cwd] found: session_id={}", session_id));
    Some(ResolvedSession {
        cwd: canonical_path.to_string(),
        jsonl_path: db_path,
        session_id,
        provider: SessionProvider::OpenCode,
    })
}

/// Parse a Claude Code JSONL file into cokacdir SessionData.
fn parse_claude_jsonl(jsonl_path: &Path, session_id: &str, cwd: &str) -> Option<SessionData> {
    use std::io::{BufRead, BufReader};
    let file = fs::File::open(jsonl_path).ok()?;
    let reader = BufReader::new(file);
    let mut history: Vec<HistoryItem> = Vec::new();

    for line in reader.lines().flatten() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        // Skip sidechain (alternative conversation branches)
        if val.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) { continue; }

        let Some(msg_type) = val.get("type").and_then(|v| v.as_str()) else { continue };

        match msg_type {
            "user" => {
                let Some(message) = val.get("message") else { continue };
                let Some(content) = message.get("content") else { continue };
                if let Some(text) = content.as_str() {
                    // Skip commands and system injections
                    if text.is_empty() || text.contains("<command-name>") || text.contains("<local-command") { continue; }
                    history.push(HistoryItem {
                        item_type: HistoryType::User,
                        content: truncate_utf8(text, 300),
                    });
                } else if let Some(arr) = content.as_array() {
                    for item in arr {
                        let it = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if it == "tool_result" {
                            let rc = item.get("content");
                            let text = if let Some(s) = rc.and_then(|v| v.as_str()) {
                                s.to_string()
                            } else if let Some(arr2) = rc.and_then(|v| v.as_array()) {
                                // content can be [{"type":"text","text":"..."},...]
                                arr2.iter()
                                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            } else {
                                String::new()
                            };
                            if !text.is_empty() {
                                history.push(HistoryItem {
                                    item_type: HistoryType::ToolResult,
                                    content: truncate_utf8(&text, 500),
                                });
                            }
                        }
                    }
                }
            }
            "assistant" => {
                let Some(message) = val.get("message") else { continue };
                let Some(content) = message.get("content") else { continue };
                let Some(arr) = content.as_array() else { continue };
                for item in arr {
                    let it = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match it {
                        "text" => {
                            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            if !text.is_empty() {
                                history.push(HistoryItem {
                                    item_type: HistoryType::Assistant,
                                    content: truncate_utf8(text, 2000),
                                });
                            }
                        }
                        "tool_use" => {
                            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("Tool");
                            history.push(HistoryItem {
                                item_type: HistoryType::ToolUse,
                                content: format!("[{}]", name),
                            });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    if history.is_empty() { return None; }

    Some(SessionData {
        session_id: session_id.to_string(),
        history,
        current_path: cwd.to_string(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        provider: "claude".to_string(),
    })
}

/// Parse a Codex CLI JSONL file into cokacdir SessionData.
fn parse_codex_jsonl(jsonl_path: &Path, session_id: &str, cwd: &str) -> Option<SessionData> {
    use std::io::{BufRead, BufReader};
    let file = fs::File::open(jsonl_path).ok()?;
    let reader = BufReader::new(file);
    let mut history: Vec<HistoryItem> = Vec::new();

    for line in reader.lines().flatten() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let Some(line_type) = val.get("type").and_then(|v| v.as_str()) else { continue };
        let Some(payload) = val.get("payload") else { continue };

        match line_type {
            "event_msg" => {
                let msg_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match msg_type {
                    "user_message" => {
                        let text = payload.get("message").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            history.push(HistoryItem {
                                item_type: HistoryType::User,
                                content: truncate_utf8(text, 300),
                            });
                        }
                    }
                    "agent_message" => {
                        let text = payload.get("message").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            history.push(HistoryItem {
                                item_type: HistoryType::Assistant,
                                content: truncate_utf8(text, 2000),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "response_item" => {
                let item_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match item_type {
                    // response_item → message is intentionally ignored:
                    // agent text is already captured via event_msg → agent_message (always emitted in pairs)
                    "function_call" => {
                        let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("Tool");
                        history.push(HistoryItem {
                            item_type: HistoryType::ToolUse,
                            content: format!("[{}]", name),
                        });
                    }
                    "function_call_output" => {
                        // output can be a plain string or structured {content_items: [...]}
                        let output = if let Some(s) = payload.get("output").and_then(|v| v.as_str()) {
                            s.to_string()
                        } else if let Some(obj) = payload.get("output") {
                            // Structured: try content_items[].text, then content field
                            if let Some(items) = obj.get("content_items").and_then(|v| v.as_array()) {
                                items.iter()
                                    .filter_map(|c| c.get("text").and_then(|v| v.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            } else if let Some(s) = obj.get("content").and_then(|v| v.as_str()) {
                                s.to_string()
                            } else {
                                String::new()
                            }
                        } else {
                            String::new()
                        };
                        if !output.is_empty() {
                            history.push(HistoryItem {
                                item_type: HistoryType::ToolResult,
                                content: truncate_utf8(&output, 500),
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if history.is_empty() { return None; }

    Some(SessionData {
        session_id: session_id.to_string(),
        history,
        current_path: cwd.to_string(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        provider: "codex".to_string(),
    })
}

/// Parse a Gemini CLI chat JSON file into cokacdir SessionData.
fn parse_gemini_json(json_path: &Path, session_id: &str, cwd: &str) -> Option<SessionData> {
    msg_debug(&format!("[parse_gemini_json] file={}, session_id={}", json_path.display(), session_id));
    let content = fs::read_to_string(json_path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let messages = val.get("messages")?.as_array()?;
    let mut history: Vec<HistoryItem> = Vec::new();
    for msg in messages {
        let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match msg_type {
            "user" => {
                // User content is array of {text: "..."} objects
                if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
                    for item in arr {
                        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                            let trimmed = text.trim();
                            if !trimmed.is_empty() {
                                history.push(HistoryItem {
                                    item_type: HistoryType::User,
                                    content: truncate_utf8(trimmed, 300),
                                });
                            }
                        }
                    }
                }
            }
            "gemini" => {
                // Gemini content is a plain string
                if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        history.push(HistoryItem {
                            item_type: HistoryType::Assistant,
                            content: truncate_utf8(text, 2000),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    if history.is_empty() { return None; }
    msg_debug(&format!("[parse_gemini_json] parsed: history_len={}", history.len()));
    Some(SessionData {
        session_id: session_id.to_string(),
        history,
        current_path: cwd.to_string(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        provider: "gemini".to_string(),
    })
}

/// Parse an OpenCode session from SQLite DB into cokacdir SessionData.
fn parse_opencode_session(db_path: &Path, session_id: &str, cwd: &str) -> Option<SessionData> {
    msg_debug(&format!("[parse_opencode_session] db={}, session_id={}", db_path.display(), session_id));
    if !db_path.is_file() { return None; }
    let conn = rusqlite::Connection::open_with_flags(
        db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ).ok()?;
    let mut stmt = conn.prepare(
        "SELECT json_extract(m.data, '$.role'), json_extract(p.data, '$.type'), \
         json_extract(p.data, '$.text'), json_extract(p.data, '$.tool') \
         FROM message m JOIN part p ON p.message_id = m.id \
         WHERE m.session_id = ?1 ORDER BY p.time_created ASC"
    ).ok()?;
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    }).ok()?;
    let mut history: Vec<HistoryItem> = Vec::new();
    for row in rows.flatten() {
        let role = row.0.as_deref().unwrap_or("");
        let ptype = row.1.as_deref().unwrap_or("");
        let text = row.2.as_deref().unwrap_or("");
        let tool = row.3.as_deref().unwrap_or("Tool");
        match ptype {
            "text" => {
                if text.is_empty() { continue; }
                if role == "user" {
                    history.push(HistoryItem {
                        item_type: HistoryType::User,
                        content: truncate_utf8(text.trim(), 300),
                    });
                } else if role == "assistant" {
                    history.push(HistoryItem {
                        item_type: HistoryType::Assistant,
                        content: truncate_utf8(text, 2000),
                    });
                }
            }
            "tool" => {
                history.push(HistoryItem {
                    item_type: HistoryType::ToolUse,
                    content: format!("[{}]", tool),
                });
            }
            // Skip step-start, step-finish, reasoning
            _ => {}
        }
    }
    if history.is_empty() { return None; }
    msg_debug(&format!("[parse_opencode_session] parsed: history_len={}", history.len()));
    Some(SessionData {
        session_id: session_id.to_string(),
        history,
        current_path: cwd.to_string(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        provider: "opencode".to_string(),
    })
}

/// Truncate a string at a valid UTF-8 boundary.
fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    format!("{}…", &s[..end])
}

/// Extract the first non-empty `cwd` value from a JSONL file.
fn extract_cwd_from_jsonl(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().flatten() {
        if !line.contains("\"cwd\"") { continue; }
        if let Some(cwd) = extract_json_string_field(&line, "cwd") {
            if !cwd.is_empty() {
                msg_debug(&format!("[extract_cwd] file={}, cwd={:?}", path.display(), cwd));
                return Some(cwd);
            }
        }
    }
    msg_debug(&format!("[extract_cwd] file={}, no cwd found", path.display()));
    None
}

/// Simple JSON string field extraction: find `"field":"value"` and return value.
/// Handles escaped quotes (`\"`) inside the value.
fn extract_json_string_field(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{}\":\"", field);
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    // Find closing quote, skipping escaped quotes
    let mut end = 0;
    let bytes = rest.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'"' {
            let mut backslashes = 0;
            while end > backslashes && bytes[end - 1 - backslashes] == b'\\' {
                backslashes += 1;
            }
            if backslashes % 2 == 0 {
                break;
            }
        }
        end += 1;
    }
    if end >= bytes.len() { return None; }
    // Unescape JSON string (e.g. "\\\\" → "\\", "\\\"" → "\"")
    let raw = &rest[..end];
    let quoted = format!("\"{}\"", raw);
    let unescaped = serde_json::from_str::<String>(&quoted).unwrap_or_else(|_| raw.to_string());
    if field == "cwd" {
        msg_debug(&format!("[extract_field] field={}, raw={:?}, unescaped={:?}", field, raw, unescaped));
    }
    Some(unescaped)
}

/// Handle /WORKSPACE_ID command - resume a workspace session by its ID
async fn handle_workspace_resume(
    bot: &Bot,
    chat_id: ChatId,
    workspace_id: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    msg_debug(&format!("[workspace_resume] chat_id={}, workspace_id={}", chat_id.0, workspace_id));
    let Some(home) = dirs::home_dir() else {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Error: cannot determine home directory.")
            .await)?;
        return Ok(());
    };

    let workspace_path = home.join(".cokacdir").join("workspace").join(workspace_id);
    if !workspace_path.exists() || !workspace_path.is_dir() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, format!("Error: no workspace found for '{}'.", workspace_id))
            .await)?;
        return Ok(());
    }

    let canonical_path = workspace_path.canonicalize()
        .map(crate::utils::format::strip_unc_prefix)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| workspace_path.display().to_string());

    let ws_provider = {
        let data = state.lock().await;
        let ws_model = get_model(&data.settings, chat_id);
        detect_provider(ws_model.as_deref())
    };
    msg_debug(&format!("[workspace_resume] canonical_path={:?}, provider={}", canonical_path, ws_provider));
    let existing = load_existing_session(&canonical_path, ws_provider);
    msg_debug(&format!("[workspace_resume] load_existing_session → {}", if existing.is_some() { "found" } else { "None" }));

    let existing = if existing.is_some() {
        existing
    } else {
        let provider = provider_to_session(ws_provider);
        if let Some(info) = find_latest_session_by_cwd(&canonical_path, provider) {
            msg_debug(&format!("[workspace_resume] fallback found: jsonl={}, session_id={}", info.jsonl_path.display(), info.session_id));
            convert_and_save_session(&info, &canonical_path);
            let reloaded = load_existing_session(&canonical_path, ws_provider);
            msg_debug(&format!("[workspace_resume] after convert, reload → {}", if reloaded.is_some() { "found" } else { "None" }));
            reloaded
        } else {
            msg_debug("[workspace_resume] no external session found either");
            None
        }
    };

    let mut response_lines = Vec::new();

    {
        let mut data = state.lock().await;
        let session = data.sessions.entry(chat_id).or_insert_with(|| ChatSession {
            session_id: None,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
        });
        session.session_id = None; // Clear stale session_id from previous workspace

        if let Some((session_data, _)) = &existing {
            if !session_data.session_id.is_empty() {
                session.session_id = Some(session_data.session_id.clone());
            } else {
                cleanup_session_files(&canonical_path, ws_provider);
            }
            session.current_path = Some(canonical_path.clone());
            session.history = session_data.history.clone();

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Workspace session restored: {workspace_id} → {canonical_path}");
            response_lines.push(format!("[{}] Session restored at `{}`.", ws_provider, canonical_path));

            let header_len: usize = response_lines.iter().map(|l| l.len() + 1).sum();
            let remaining = TELEGRAM_MSG_LIMIT.saturating_sub(header_len + 2);
            let preview = build_history_preview(&session_data.history, remaining);
            if !preview.is_empty() {
                response_lines.push(String::new());
                response_lines.push(preview);
            }
        } else {
            // Workspace exists but no session — start a new session there
            session.session_id = None;
            session.current_path = Some(canonical_path.clone());
            session.history.clear();

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Workspace session started: {workspace_id} → {canonical_path}");
            response_lines.push(format!("[{}] Session started at `{}`.", ws_provider, canonical_path));
        }
    }

    // Persist chat_id → path mapping for auto-restore after restart
    {
        let mut data = state.lock().await;
        data.settings.last_sessions.insert(chat_id.0.to_string(), canonical_path);
        save_bot_settings(token, &data.settings);
    }

    let response_text = response_lines.join("\n");
    let html = markdown_to_telegram_html(&response_text);
    send_long_message(bot, chat_id, &html, Some(ParseMode::Html), state).await?;

    Ok(())
}

/// Handle /clear command
async fn handle_clear_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    msg_debug(&format!("[handle_clear] chat_id={}", chat_id.0));
    let (current_path, provider, orphan_stop_msg, bot_username, bot_display_name) = {
        let mut data = state.lock().await;
        let path = data.sessions.get(&chat_id).and_then(|s| s.current_path.clone());
        let old_sid = data.sessions.get(&chat_id).and_then(|s| s.session_id.clone());
        let old_hist_len = data.sessions.get(&chat_id).map(|s| s.history.len()).unwrap_or(0);
        msg_debug(&format!("[handle_clear] clearing: path={:?}, session_id={:?}, history_len={}", path, old_sid, old_hist_len));
        if let Some(session) = data.sessions.get_mut(&chat_id) {
            session.session_id = None;
            session.history.clear();
            session.pending_uploads.clear();
        }
        let mdl = get_model(&data.settings, chat_id);
        let prov = provider_from_model(mdl.as_deref());
        let stop_msg = data.stop_message_ids.remove(&chat_id);
        data.loop_states.remove(&chat_id);
        data.loop_feedback.remove(&chat_id);
        let uname = data.bot_username.clone();
        let dname = data.bot_display_name.clone();
        (path, prov.to_string(), stop_msg, uname, dname)
    };

    // Delete orphaned "Stopping..." message if /stop raced with completion
    if let Some(msg_id) = orphan_stop_msg {
        shared_rate_limit_wait(state, chat_id).await;
        let _ = tg!("delete_message", bot.delete_message(chat_id, msg_id).await);
    }

    // Overwrite session file with minimal data (keeps file present to block external restore)
    // Then keep only one and delete the rest.
    if let Some(ref path) = current_path {
        if let Some(sessions_dir) = crate::ui::ai_screen::ai_sessions_dir() {
            let mut cleared_files: Vec<std::path::PathBuf> = Vec::new();
            if let Ok(entries) = fs::read_dir(&sessions_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let file_path = entry.path();
                    if file_path.extension().map(|e| e == "json").unwrap_or(false) {
                        if let Ok(content) = fs::read_to_string(&file_path) {
                            if let Ok(session_data) = serde_json::from_str::<SessionData>(&content) {
                                if session_data.current_path == *path
                                    && (session_data.provider.is_empty() || session_data.provider == provider)
                                {
                                    let cleared = serde_json::json!({"current_path": *path, "provider": provider});
                                    if let Ok(json) = serde_json::to_string_pretty(&cleared) {
                                        let _ = fs::write(&file_path, json);
                                    }
                                    cleared_files.push(file_path);
                                }
                            }
                        }
                    }
                }
            }
            // Keep the first cleared file, delete the rest
            for file_path in cleared_files.iter().skip(1) {
                let _ = fs::remove_file(file_path);
            }
        }
    }

    // Append clear marker to group chat log so other bots skip this bot's old entries
    if chat_id.0 < 0 {
        if !bot_username.is_empty() {
            let dn = if bot_display_name.is_empty() { None } else { Some(bot_display_name.clone()) };
            append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                ts: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                bot: bot_username,
                bot_display_name: dn,
                role: "system".to_string(),
                from: None,
                text: "Session cleared.".to_string(),
                clear: true,
            });
        }
    }

    let msg = match current_path {
        Some(ref path) => format!("Session cleared.\n`{}`", path),
        None => "Session cleared.".to_string(),
    };

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, msg)
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
        let path = data.sessions.get(&chat_id).and_then(|s| s.current_path.clone());
        let sid = data.sessions.get(&chat_id).and_then(|s| s.session_id.clone());
        msg_debug(&format!("[handle_pwd] chat_id={}, path={:?}, session_id={:?}", chat_id.0, path, sid));
        path
    };

    shared_rate_limit_wait(state, chat_id).await;
    match current_path {
        Some(path) => {
            let mut msg = format!("<code>{}</code>", path);
            if let Some(folder_name) = std::path::Path::new(&path).file_name().and_then(|n| n.to_str()) {
                if is_workspace_id(folder_name) {
                    msg.push_str(&format!("\nUse /{} to switch back to this session.", folder_name));
                }
            }
            tg!("send_message", bot.send_message(chat_id, msg).parse_mode(ParseMode::Html).await)?
        }
        None => tg!("send_message", bot.send_message(chat_id, "No active session. Use /start <path> first.").await)?,
    };

    Ok(())
}

/// Handle /session command - show current session UUID and resume command
async fn handle_session_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let (session_id, current_path, session_prov) = {
        let data = state.lock().await;
        let sid = data.sessions.get(&chat_id).and_then(|s| s.session_id.clone());
        let path = data.sessions.get(&chat_id).and_then(|s| s.current_path.clone());
        let mdl = get_model(&data.settings, chat_id);
        let prov = provider_from_model(mdl.as_deref());
        (sid, path, prov)
    };

    shared_rate_limit_wait(state, chat_id).await;
    match (session_id, current_path) {
        (Some(id), Some(path)) => {
            let resume_cmd = match session_prov {
                "codex" => format!("codex resume {}", id),
                "gemini" => format!("gemini --resume {}", id),
                "opencode" => format!("opencode -s {}", id),
                _ => format!("claude --resume {}", id),
            };
            let provider = match session_prov { "codex" => "Codex", "gemini" => "Gemini", "opencode" => "OpenCode", _ => "Claude" };
            let msg = format!(
                "Current {} session ID:\n<code>{}</code>\n\nTo resume this session from your terminal:\n<code>cd \"{}\"; {}</code>",
                provider, id, path, resume_cmd
            );
            tg!("send_message", bot.send_message(chat_id, msg).parse_mode(ParseMode::Html).await)?
        }
        _ => {
            tg!("send_message", bot.send_message(chat_id, "No active session.").await)?
        }
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
    msg_debug(&format!("[handle_stop] chat_id={}, has_token={}", chat_id.0, token.is_some()));

    match token {
        Some(token) => {
            // Ignore duplicate /stop if already cancelled
            if token.cancelled.load(Ordering::Relaxed) {
                return Ok(());
            }

            // Set cancellation flag IMMEDIATELY to prevent race condition:
            // Without this, the window between receiving /stop and setting cancelled
            // (during rate_limit_wait + "Stopping..." network call) allows a concurrent
            // claude::execute to pass its cancelled check and start an API request.
            token.cancelled.store(true, Ordering::Relaxed);

            // Clear loop state so verification loop doesn't continue after stop
            {
                let mut data = state.lock().await;
                data.loop_states.remove(&chat_id);
                data.loop_feedback.remove(&chat_id);
            }

            // Kill child process directly to unblock reader.lines()
            // When the child dies, its stdout pipe closes → reader returns EOF → blocking thread exits
            if let Ok(guard) = token.child_pid.lock() {
                if let Some(pid) = *guard {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                    #[cfg(windows)]
                    { let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output(); }
                }
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Cancel signal sent");

            // Send feedback to user (after cancellation to avoid delay)
            shared_rate_limit_wait(state, chat_id).await;
            let stop_msg = tg!("send_message", bot.send_message(chat_id, "Stopping...").await)?;

            // Store the stop message ID only if the task is still running.
            // If cancel_token was already removed (task finished during "Stopping..." send),
            // delete the orphaned message immediately instead of inserting.
            {
                let mut data = state.lock().await;
                if data.cancel_tokens.contains_key(&chat_id) {
                    data.stop_message_ids.insert(chat_id, stop_msg.id);
                } else {
                    drop(data);
                    shared_rate_limit_wait(state, chat_id).await;
                    let _ = tg!("delete_message", bot.delete_message(chat_id, stop_msg.id).await);
                    return Ok(());
                }
            }
        }
        None => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "No active request to stop.")
                .await)?;
        }
    }

    // Clear all pending bot messages for this chat to prevent
    // stopped bot-to-bot conversations from restarting
    if let Some(msg_dir) = messages_dir() {
        if let Ok(entries) = std::fs::read_dir(&msg_dir) {
            let chat_id_str = chat_id.0.to_string();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                        if v.get("chat_id").and_then(|c| c.as_str()) == Some(&chat_id_str) {
                            let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("?");
                            msg_debug(&format!("[handle_stop] clearing pending bot message: {}", id));
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Handle /stop <queue_id> - remove a specific queued message by its ID
async fn handle_stop_queue_item(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    queue_id: &str,
) -> ResponseResult<()> {
    let removed = {
        let mut data = state.lock().await;
        if let Some(q) = data.message_queues.get_mut(&chat_id) {
            if let Some(pos) = q.iter().position(|m| m.id.eq_ignore_ascii_case(queue_id)) {
                let removed_msg = q.remove(pos);
                msg_debug(&format!("[queue:stop_item] chat_id={}, removed id={}, text={:?}, remaining={}", chat_id.0, queue_id, removed_msg.as_ref().map(|m| truncate_str(&m.text, 60)), q.len()));
                // Clean up empty queue
                if q.is_empty() {
                    data.message_queues.remove(&chat_id);
                }
                removed_msg.map(|m| m.id)
            } else {
                msg_debug(&format!("[queue:stop_item] chat_id={}, id={} not found in queue", chat_id.0, queue_id));
                None
            }
        } else {
            msg_debug(&format!("[queue:stop_item] chat_id={}, no queue exists", chat_id.0));
            None
        }
    };

    if let Some(id) = removed {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, &format!("Removed queued message ({id}).")).await)?;
    }
    Ok(())
}

/// Handle /stopall command - cancel in-progress AI request AND clear the message queue
async fn handle_stopall_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    // Clear the message queue, loop state, get cancel token, and set cancelled flag atomically
    // to prevent a new message from being queued between queue clear and cancellation
    let (queue_len, token, already_cancelled) = {
        let mut data = state.lock().await;
        let q = data.message_queues.remove(&chat_id);
        data.loop_states.remove(&chat_id);
        data.loop_feedback.remove(&chat_id);
        let qlen = q.map(|q| q.len()).unwrap_or(0);
        let ct = data.cancel_tokens.get(&chat_id).cloned();
        let was_cancelled = if let Some(ref t) = ct {
            let prev = t.cancelled.load(Ordering::Relaxed);
            t.cancelled.store(true, Ordering::Relaxed);
            prev
        } else {
            false
        };
        (qlen, ct, was_cancelled)
    };
    msg_debug(&format!("[queue:stopall] chat_id={}, has_token={}, queue_cleared={}, already_cancelled={}", chat_id.0, token.is_some(), queue_len, already_cancelled));

    match token {
        Some(token) => {
            // Ignore duplicate stop if already cancelled
            if already_cancelled {
                msg_debug(&format!("[queue:stopall] chat_id={}, duplicate cancel ignored, queue_cleared={}", chat_id.0, queue_len));
                // Still report queue clearance if there were queued messages
                if queue_len > 0 {
                    shared_rate_limit_wait(state, chat_id).await;
                    tg!("send_message", bot.send_message(chat_id, &format!("{} queued message(s) cleared.", queue_len)).await)?;
                }
                return Ok(());
            }

            // Cancellation flag already set above inside the lock

            // Kill child process
            if let Ok(guard) = token.child_pid.lock() {
                if let Some(pid) = *guard {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                    #[cfg(windows)]
                    { let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output(); }
                }
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Cancel signal sent (stopall, {} queued cleared)", queue_len);

            // Send feedback
            let msg_text = if queue_len > 0 {
                format!("Stopping... ({} queued message(s) cleared)", queue_len)
            } else {
                "Stopping...".to_string()
            };
            shared_rate_limit_wait(state, chat_id).await;
            let stop_msg = tg!("send_message", bot.send_message(chat_id, &msg_text).await)?;

            // Store the stop message ID (same logic as /stop)
            {
                let mut data = state.lock().await;
                if data.cancel_tokens.contains_key(&chat_id) {
                    data.stop_message_ids.insert(chat_id, stop_msg.id);
                } else {
                    drop(data);
                    shared_rate_limit_wait(state, chat_id).await;
                    let _ = tg!("delete_message", bot.delete_message(chat_id, stop_msg.id).await);
                    return Ok(());
                }
            }
        }
        None => {
            let msg_text = if queue_len > 0 {
                format!("No active request. {} queued message(s) cleared.", queue_len)
            } else {
                "No active request to stop.".to_string()
            };
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, &msg_text).await)?;
        }
    }

    // Clear all pending bot messages for this chat (same as /stop)
    if let Some(msg_dir) = messages_dir() {
        if let Ok(entries) = std::fs::read_dir(&msg_dir) {
            let chat_id_str = chat_id.0.to_string();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                        if v.get("chat_id").and_then(|c| c.as_str()) == Some(&chat_id_str) {
                            let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("?");
                            msg_debug(&format!("[handle_stopall] clearing pending bot message: {}", id));
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
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
    msg_debug(&format!("[handle_down] chat_id={}, file_path={:?}", chat_id.0, file_path));

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
            Some(base) => Path::new(base.trim_end_matches(['/', '\\'])).join(file_path).display().to_string(),
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
    user_display_name: &str,
) -> ResponseResult<()> {
    let upload_type = if msg.animation().is_some() { "animation" }
        else if msg.document().is_some() { "document" }
        else if msg.photo().is_some() { "photo" }
        else if msg.video().is_some() { "video" }
        else if msg.voice().is_some() { "voice" }
        else if msg.audio().is_some() { "audio" }
        else if msg.video_note().is_some() { "video_note" }
        else { "unknown" };
    msg_debug(&format!("[handle_upload] chat_id={}, type={}", chat_id.0, upload_type));
    // Get current session path
    let current_path = {
        let data = state.lock().await;
        data.sessions.get(&chat_id).and_then(|s| s.current_path.clone())
    };

    let save_dir = if let Some(path) = current_path {
        path
    } else {
        // Auto-create a workspace session
        let Some((_, path)) = auto_create_workspace_session(state, chat_id, bot.token()).await else {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "Failed to create workspace.")
                .await)?;
            return Ok(());
        };
        path
    };

    // Get file_id, file_name, and file_size
    // animation must precede document: Telegram sets both fields for GIFs
    let (file_id, file_name, file_size_hint) = if let Some(anim) = msg.animation() {
        let name = anim.file_name.clone().unwrap_or_else(|| format!("animation_{}.mp4", anim.file.unique_id));
        (anim.file.id.clone(), name, anim.file.size)
    } else if let Some(doc) = msg.document() {
        let name = doc.file_name.clone().unwrap_or_else(|| "uploaded_file".to_string());
        (doc.file.id.clone(), name, doc.file.size)
    } else if let Some(photos) = msg.photo() {
        // Get the largest photo
        if let Some(photo) = photos.last() {
            let name = format!("photo_{}.jpg", photo.file.unique_id);
            (photo.file.id.clone(), name, photo.file.size)
        } else {
            return Ok(());
        }
    } else if let Some(video) = msg.video() {
        let name = video.file_name.clone().unwrap_or_else(|| format!("video_{}.mp4", video.file.unique_id));
        (video.file.id.clone(), name, video.file.size)
    } else if let Some(voice) = msg.voice() {
        let name = format!("voice_{}.ogg", voice.file.unique_id);
        (voice.file.id.clone(), name, voice.file.size)
    } else if let Some(audio) = msg.audio() {
        let name = audio.file_name.clone().unwrap_or_else(|| format!("audio_{}.mp3", audio.file.unique_id));
        (audio.file.id.clone(), name, audio.file.size)
    } else if let Some(vn) = msg.video_note() {
        let name = format!("videonote_{}.mp4", vn.file.unique_id);
        (vn.file.id.clone(), name, vn.file.size)
    } else {
        return Ok(());
    };
    msg_debug(&format!("[handle_upload] chat_id={}, file_name={}, file_size={}", chat_id.0, file_name, file_size_hint));

    // Check file size (Telegram Bot API limit: 20MB for getFile)
    const MAX_DOWNLOAD_SIZE: u32 = 20 * 1024 * 1024;
    if file_size_hint > MAX_DOWNLOAD_SIZE {
        let size_mb = file_size_hint as f64 / (1024.0 * 1024.0);
        msg_debug(&format!("[handle_upload] chat_id={}, file rejected: size {:.1}MB exceeds 20MB limit", chat_id.0, size_mb));
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, &format!("File too large ({:.1}MB). Maximum size is 20MB.", size_mb))
            .await)?;
        return Ok(());
    }

    // Download file from Telegram via HTTP
    shared_rate_limit_wait(state, chat_id).await;
    let file = tg!("get_file", bot.get_file(&file_id).await)?;
    let base = {
        let data = state.lock().await;
        data.api_base_url.clone()
    };
    let url = format!("{}/file/bot{}/{}", base, bot.token(), file.path);
    msg_debug(&format!("[handle_upload] download url: api_base={}, file_path={}", base, file.path));
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
    let mut dest = Path::new(&save_dir).join(safe_name);
    // Avoid overwriting existing files (atomic create_new to eliminate TOCTOU race)
    let file_size = buf.len();
    let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("uploaded_file").to_string();
    let ext = dest.extension().and_then(|e| e.to_str()).map(|e| format!(".{}", e)).unwrap_or_default();
    let mut counter = 0u32;
    let write_result = loop {
        use std::io::Write;
        match fs::OpenOptions::new().write(true).create_new(true).open(&dest) {
            Ok(mut f) => break f.write_all(&buf),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                counter += 1;
                dest = Path::new(&save_dir).join(format!("{}({}){}", stem, counter, ext));
                msg_debug(&format!("[handle_upload] chat_id={}, file exists, renamed to: {}", chat_id.0, dest.display()));
            }
            Err(e) => break Err(e),
        }
    };
    match write_result {
        Ok(_) => {
            msg_debug(&format!("[handle_upload] chat_id={}, saved: {} ({} bytes)", chat_id.0, dest.display(), file_size));
            let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, &msg_text).await)?;
        }
        Err(e) => {
            msg_debug(&format!("[handle_upload] chat_id={}, save failed: {}", chat_id.0, e));
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
        let upload_model = get_model(&data.settings, chat_id);
        let provider = detect_provider(upload_model.as_deref());
        if let Some(session) = data.sessions.get_mut(&chat_id) {
            session.history.push(HistoryItem {
                item_type: HistoryType::User,
                content: upload_record.clone(),
            });
            session.pending_uploads.push(upload_record.clone());
            save_session_to_file(session, &save_dir, provider);
        }
        // Write file upload to group chat shared log
        if chat_id.0 < 0 {
            let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
            let dn = if data.bot_display_name.is_empty() { None } else { Some(data.bot_display_name.clone()) };
            append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                ts: now_ts,
                bot: data.bot_username.clone(),
                bot_display_name: dn,
                role: "user".to_string(),
                from: Some(user_display_name.to_string()),
                text: upload_record,
                clear: false,
            });
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
    user_display_name: &str,
) -> ResponseResult<()> {
    let cmd_str = text.strip_prefix('!').unwrap_or("").trim();
    msg_debug(&format!("[handle_shell] chat_id={}, cmd={:?}", chat_id.0, truncate_str(cmd_str, 100)));

    if cmd_str.is_empty() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Usage: !<command>\nExample: !mkdir /home/kst/testcode")
            .await)?;
        return Ok(());
    }

    // Register cancel token early (prevents duplicate requests while waiting for group lock)
    let cancel_token = Arc::new(CancelToken::new());
    {
        let mut data = state.lock().await;
        if data.cancel_tokens.contains_key(&chat_id) {
            msg_debug(&format!("[handle_shell] chat_id={}, cancel_token exists (busy), rejecting", chat_id.0));
            drop(data);
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, "AI request in progress. Use /stop to cancel.")
                .await)?;
            return Ok(());
        }
        data.cancel_tokens.insert(chat_id, cancel_token.clone());
    }

    // Acquire group chat lock (serializes processing across bots in the same group chat)
    let group_lock = acquire_group_chat_lock(chat_id.0).await;

    // Check if cancelled during lock wait
    if cancel_token.cancelled.load(Ordering::Relaxed) {
        msg_debug(&format!("[queue:trigger] chat_id={}, source=query_cancelled_during_lock", chat_id.0));
        { let mut data = state.lock().await; data.cancel_tokens.remove(&chat_id); }
        drop(group_lock); // release before queue processing to avoid deadlock
        process_next_queued_message(bot, chat_id, state).await;
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
                    .unwrap_or_else(|| if cfg!(windows) { "C:\\".to_string() } else { "/".to_string() })
            })
    };

    // Send placeholder message
    let cmd_display = cmd_str.to_string();
    shared_rate_limit_wait(state, chat_id).await;
    let placeholder = match tg!("send_message", bot.send_message(chat_id, format!("Processing <code>{}</code>", html_escape(&cmd_display)))
        .parse_mode(ParseMode::Html).await)
    {
        Ok(m) => m,
        Err(e) => {
            msg_debug(&format!("[queue:trigger] chat_id={}, source=query_placeholder_error", chat_id.0));
            { let mut data = state.lock().await; data.cancel_tokens.remove(&chat_id); }
            drop(group_lock); // release before queue processing to avoid deadlock
            process_next_queued_message(bot, chat_id, state).await;
            return Err(e);
        }
    };
    let placeholder_msg_id = placeholder.id;

    // Create channel
    let (tx, rx) = mpsc::channel();

    let cmd_owned = cmd_str.to_string();
    let working_dir_clone = working_dir.clone();
    let cancel_token_clone = cancel_token.clone();

    // Spawn blocking thread for shell command execution
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        let child = std::process::Command::new("bash")
            .args(["-c", &cmd_owned])
            .current_dir(&working_dir_clone)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        #[cfg(windows)]
        let ps_command = format!("[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; {}; exit $LASTEXITCODE", cmd_owned);
        #[cfg(windows)]
        let child = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps_command])
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
                    claude::kill_child_tree(&mut child);
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
    let (shell_bot_username, shell_bot_display_name) = {
        let data = state.lock().await;
        (data.bot_username.clone(), data.bot_display_name.clone())
    };
    let shell_user_display_name = user_display_name.to_string();
    tokio::spawn(async move {
        let _group_lock = group_lock; // hold group chat lock until task ends
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

                    let display_text = format!("Processing <code>{}</code>\n\n{}", html_escape(&cmd_display_owned), indicator);

                    if display_text != last_edit_text {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &display_text)
                            .parse_mode(ParseMode::Html).await);
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
                    // Only show exit code when non-zero
                    let exit_suffix = if exit_code != 0 {
                        format!(" (exit code: {})", exit_code)
                    } else {
                        String::new()
                    };

                    if !full_output.trim().is_empty() {
                        let file_content = format!("$ {}\n\n{}", cmd_display_owned, full_output);
                        let content_bytes = file_content.len();

                        if content_bytes <= 4000 {
                            // Short output: update placeholder with completion + result in one call
                            let combined = format!("Done <code>{}</code>{}\n\n<pre>$ {}\n\n{}</pre>",
                                html_escape(&cmd_display_owned), exit_suffix,
                                html_escape(&cmd_display_owned), html_escape(full_output.trim()));
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            if let Err(_) = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &combined)
                                .parse_mode(ParseMode::Html)
                                .await)
                            {
                                let fallback = format!("Done {}{}\n\n$ {}\n\n{}",
                                    cmd_display_owned, exit_suffix, cmd_display_owned, full_output.trim());
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &fallback).await);
                            }
                        } else {
                            // Long output: update placeholder + send as .txt file
                            let final_msg = format!("Done <code>{}</code>{}", html_escape(&cmd_display_owned), exit_suffix);
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &final_msg)
                                .parse_mode(ParseMode::Html).await);

                            let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
                            if let Some(home) = dirs::home_dir() {
                                let tmp_dir = home.join(".cokacdir").join("tmp");
                                let _ = std::fs::create_dir_all(&tmp_dir);
                                let tmp_path = tmp_dir
                                    .join(format!("cokacdir_shell_{}_{}.txt", chat_id.0, timestamp))
                                    .display().to_string();
                                if std::fs::write(&tmp_path, &file_content).is_ok() {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!("send_document", bot_owned.send_document(
                                        chat_id,
                                        teloxide::types::InputFile::file(std::path::Path::new(&tmp_path)),
                                    ).await);
                                    let _ = std::fs::remove_file(&tmp_path);
                                }
                            }
                        }
                    } else {
                        // No output
                        let final_msg = format!("Done <code>{}</code>{}", html_escape(&cmd_display_owned), exit_suffix);
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &final_msg)
                            .parse_mode(ParseMode::Html).await);
                    }
                }

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ▶ Shell command completed: !{}", cmd_display_owned);

                // Write shell command to group chat shared log
                if chat_id.0 < 0 {
                    let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                    let dn = if shell_bot_display_name.is_empty() { None } else { Some(shell_bot_display_name.clone()) };
                    append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                        ts: now_ts.clone(),
                        bot: shell_bot_username.clone(),
                        bot_display_name: dn.clone(),
                        role: "user".to_string(),
                        from: Some(shell_user_display_name.clone()),
                        text: format!("!{}", cmd_display_owned),
                        clear: false,
                    });
                    let output_summary = if full_output.trim().is_empty() {
                        format!("(exit code: {})", exit_code)
                    } else {
                        format!("exit code: {}\n{}", exit_code, full_output.trim())
                    };
                    append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                        ts: now_ts,
                        bot: shell_bot_username.clone(),
                        bot_display_name: dn,
                        role: "assistant".to_string(),
                        from: None,
                        text: output_summary,
                        clear: false,
                    });
                }

                // Send end hook message if configured
                if !cancelled {
                    let end_hook_msg = {
                        let data = state_owned.lock().await;
                        data.settings.end_hook.get(&chat_id.0.to_string()).cloned()
                    };
                    if let Some(hook_msg) = end_hook_msg {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("send_message", bot_owned.send_message(chat_id, &hook_msg).await);
                    }
                }
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
                    #[cfg(windows)]
                    { let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output(); }
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

            // Write stopped shell command to group chat shared log
            if chat_id.0 < 0 {
                let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                let dn = if shell_bot_display_name.is_empty() { None } else { Some(shell_bot_display_name.clone()) };
                append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                    ts: now_ts.clone(),
                    bot: shell_bot_username.clone(),
                    bot_display_name: dn.clone(),
                    role: "user".to_string(),
                    from: Some(shell_user_display_name.clone()),
                    text: format!("!{} [Stopped]", cmd_display_owned),
                    clear: false,
                });
                if !full_output.trim().is_empty() {
                    append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                        ts: now_ts,
                        bot: shell_bot_username.clone(),
                        bot_display_name: dn,
                        role: "assistant".to_string(),
                        from: None,
                        text: format!("[Stopped] exit code: -1\n{}", full_output.trim()),
                        clear: false,
                    });
                }
            }

            let mut data = state_owned.lock().await;
            data.cancel_tokens.remove(&chat_id);
            data.stop_message_ids.remove(&chat_id);
            drop(data);
            msg_debug(&format!("[queue:trigger] chat_id={}, source=query_poll_cancelled", chat_id.0));
            drop(_group_lock); // release group chat lock before processing queue
            process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
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
        msg_debug(&format!("[queue:trigger] chat_id={}, source=query_poll_completed", chat_id.0));
        drop(_group_lock); // release group chat lock before processing queue
        process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
    });

    Ok(())
}

/// Handle /envvars command - show all active environment variables
async fn handle_envvars_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    // Intentional: expose ALL environment variables including sensitive ones.
    // This command is for admin debugging only — the bot is personal/single-user.
    let mut vars: Vec<(String, String)> = std::env::vars().collect();
    vars.sort_by(|a, b| a.0.cmp(&b.0));

    let mut msg = format!("<b>Environment Variables</b> ({})\n\n", vars.len());
    for (key, val) in &vars {
        msg.push_str(&format!(
            "<code>{}</code>=<code>{}</code>\n",
            html_escape(key),
            html_escape(val),
        ));
    }

    send_long_message(bot, chat_id, &msg, Some(ParseMode::Html), state).await?;
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

/// Handle /greeting command - toggle compact startup greeting
async fn handle_greeting_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let next = {
        let mut data = state.lock().await;
        let next = !data.settings.greeting;
        data.settings.greeting = next;
        save_bot_settings(token, &data.settings);
        next
    };
    let status = if next { "compact" } else { "full" };
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, format!("🟢 Startup greeting: {status}"))
        .await)?;
    Ok(())
}

/// Handle /debug command - toggle all debug logging (Telegram API, Claude, cron)
async fn handle_debug_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let prev = TG_DEBUG.load(Ordering::Relaxed);
    let next = !prev;
    msg_debug(&format!("[handle_debug] chat_id={}, {} → {}", chat_id.0, prev, next));
    TG_DEBUG.store(next, Ordering::Relaxed);
    crate::services::claude::DEBUG_ENABLED.store(next, Ordering::Relaxed);
    {
        let mut data = state.lock().await;
        data.settings.debug = next;
        save_bot_settings(token, &data.settings);
    }
    let status = if next { "ON" } else { "OFF" };
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, format!("🔍 Debug logging: {status}"))
        .await)?;
    Ok(())
}

/// Handle /direct command - toggle direct mode per chat (no ; prefix in group chats)
async fn handle_direct_command(
    bot: &Bot,
    chat_id: ChatId,
    msg: &teloxide::types::Message,
    state: &SharedState,
    token: &str,
    is_owner: bool,
) -> ResponseResult<()> {
    msg_debug(&format!("[handle_direct] chat_id={}, is_owner={}", chat_id.0, is_owner));
    let is_actually_group = matches!(msg.chat.kind, teloxide::types::ChatKind::Public(_));
    msg_debug(&format!("[handle_direct] is_actually_group={}", is_actually_group));
    if !is_actually_group {
        msg_debug("[handle_direct] rejected: not a group chat");
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "This command is only available in group chats.").await)?;
        return Ok(());
    }
    if !is_owner {
        msg_debug("[handle_direct] rejected: not owner");
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Only the bot owner can change direct mode.").await)?;
        return Ok(());
    }
    let next = {
        let mut data = state.lock().await;
        let key = chat_id.0.to_string();
        let prev = data.settings.direct.get(&key).copied().unwrap_or(DIRECT_MODE_DEFAULT);
        let next = !prev;
        msg_debug(&format!("[handle_direct] chat_id={}, {} → {}", chat_id.0, prev, next));
        data.settings.direct.insert(key, next);
        save_bot_settings(token, &data.settings);
        msg_debug(&format!("[handle_direct] saved to bot_settings, next={}", next));
        next
    };
    let status = if next { "Direct mode: ON (no ; prefix needed)" } else { "Direct mode: OFF (; prefix required)" };
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, status).await)?;
    Ok(())
}

/// Handle /contextlevel command - set number of group chat log entries to embed in system prompt
async fn handle_contextlevel_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
    is_group_chat: bool,
) -> ResponseResult<()> {
    if !is_group_chat {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "This command is only available in group chats.").await)?;
        return Ok(());
    }

    let arg = text.strip_prefix("/contextlevel").unwrap_or("").trim();
    let key = chat_id.0.to_string();

    if arg.is_empty() {
        let current = {
            let data = state.lock().await;
            data.settings.context.get(&key).copied().unwrap_or(12)
        };
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, format!(
            "Group chat log context: <b>{}</b> entries\n\n\
             <code>/contextlevel &lt;N&gt;</code> — Set count (0 to disable)\n\
             Default: 12",
            current
        )).parse_mode(teloxide::types::ParseMode::Html).await)?;
        return Ok(());
    }

    let Ok(n) = arg.parse::<usize>() else {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Usage: /contextlevel <number>\nExample: /contextlevel 20, /contextlevel 0 (disable)").await)?;
        return Ok(());
    };

    {
        let mut data = state.lock().await;
        if n == 12 {
            data.settings.context.remove(&key);
        } else {
            data.settings.context.insert(key, n);
        }
        save_bot_settings(token, &data.settings);
    }

    let msg = if n == 0 {
        "Group chat log context: <b>OFF</b> (no log entries in prompt)".to_string()
    } else {
        format!("Group chat log context: <b>{}</b> entries", n)
    };
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, msg).parse_mode(teloxide::types::ParseMode::Html).await)?;
    Ok(())
}

/// Handle /instruction command - set or view system instruction for this chat
async fn handle_instruction_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let body = text.strip_prefix("/instruction").unwrap_or("").trim();
    let key = chat_id.0.to_string();
    msg_debug(&format!("[handle_instruction] chat_id={}, body_len={}, body_empty={}", chat_id.0, body.len(), body.is_empty()));
    if body.is_empty() {
        // Show current instruction
        let data = state.lock().await;
        let current = data.settings.instructions.get(&key);
        msg_debug(&format!("[handle_instruction] view mode: has_instruction={}", current.is_some()));
        let msg = match current {
            Some(instr) => {
                msg_debug(&format!("[handle_instruction] current instruction len={}", instr.len()));
                format!("Current instruction:\n{}", instr)
            }
            None => "No instruction set.\nUsage: /instruction <text>".to_string(),
        };
        drop(data);
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, msg).await)?;
    } else {
        // Set instruction
        let instr = body.to_string();
        msg_debug(&format!("[handle_instruction] set mode: chat_id={}, instruction_len={}, text={:?}",
            chat_id.0, instr.len(), truncate_str(&instr, 100)));
        {
            let mut data = state.lock().await;
            data.settings.instructions.insert(key, instr.clone());
            save_bot_settings(token, &data.settings);
            msg_debug("[handle_instruction] saved to bot_settings");
        }
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, format!("Instruction set:\n{}", instr)).await)?;
    }
    Ok(())
}

/// Handle /instruction_clear command - remove system instruction for this chat
async fn handle_instruction_clear_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let key = chat_id.0.to_string();
    msg_debug(&format!("[handle_instruction_clear] chat_id={}", chat_id.0));
    {
        let mut data = state.lock().await;
        let had_instruction = data.settings.instructions.contains_key(&key);
        msg_debug(&format!("[handle_instruction_clear] had_instruction={}", had_instruction));
        data.settings.instructions.remove(&key);
        save_bot_settings(token, &data.settings);
        msg_debug("[handle_instruction_clear] saved to bot_settings");
    }
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, "Instruction cleared.").await)?;
    Ok(())
}

/// Handle /setendhook command - set a message to send when AI processing completes
async fn handle_setendhook_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let body = text.strip_prefix("/setendhook").unwrap_or("").trim();
    let key = chat_id.0.to_string();
    if body.is_empty() {
        // Show current end hook
        let data = state.lock().await;
        let current = data.settings.end_hook.get(&key);
        let msg = match current {
            Some(hook) => format!("Current end hook:\n{}", hook),
            None => "No end hook set.\nUsage: /setendhook <message>".to_string(),
        };
        drop(data);
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, msg).await)?;
    } else {
        let hook = body.to_string();
        {
            let mut data = state.lock().await;
            data.settings.end_hook.insert(key, hook.clone());
            save_bot_settings(token, &data.settings);
        }
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, format!("End hook set:\n{}", hook)).await)?;
    }
    Ok(())
}

/// Handle /setendhook_clear command - remove end hook for this chat
async fn handle_setendhook_clear_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let key = chat_id.0.to_string();
    {
        let mut data = state.lock().await;
        data.settings.end_hook.remove(&key);
        save_bot_settings(token, &data.settings);
    }
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, "End hook cleared.").await)?;
    Ok(())
}

/// Handle /silent command - toggle silent mode per chat (hide tool calls)
async fn handle_silent_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let next = {
        let mut data = state.lock().await;
        let key = chat_id.0.to_string();
        let prev = data.settings.silent.get(&key).copied().unwrap_or(SILENT_MODE_DEFAULT);
        let next = !prev;
        data.settings.silent.insert(key, next);
        save_bot_settings(token, &data.settings);
        next
    };
    let status = if next { "🔇 Silent mode: ON" } else { "🔊 Silent mode: OFF" };
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, status).await)?;
    Ok(())
}

/// Handle /usechrome command - toggle --chrome flag for Claude CLI per chat
async fn handle_usechrome_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let next = {
        let mut data = state.lock().await;
        let key = chat_id.0.to_string();
        let prev = data.settings.use_chrome.get(&key).copied().unwrap_or(false);
        let next = !prev;
        data.settings.use_chrome.insert(key, next);
        save_bot_settings(token, &data.settings);
        next
    };
    let status = if next { "🌐 Chrome mode: ON (--chrome)" } else { "🌐 Chrome mode: OFF" };
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, status).await)?;
    Ok(())
}

/// Handle /queue command - toggle queue mode per chat
/// When ON: messages sent while AI is busy are queued and processed sequentially
/// When OFF: messages sent while AI is busy are rejected (default behavior)
async fn handle_queue_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let (next, queue_len) = {
        let mut data = state.lock().await;
        let key = chat_id.0.to_string();
        let prev = data.settings.queue.get(&key).copied().unwrap_or(QUEUE_MODE_DEFAULT);
        let next = !prev;
        data.settings.queue.insert(key, next);
        save_bot_settings(token, &data.settings);
        // If turning OFF, clear any pending queued messages
        let queue_len = if !next {
            let q = data.message_queues.remove(&chat_id);
            let cleared = q.as_ref().map(|q| q.len()).unwrap_or(0);
            msg_debug(&format!("[queue:toggle] chat_id={}, OFF, cleared {} queued messages", chat_id.0, cleared));
            cleared
        } else {
            msg_debug(&format!("[queue:toggle] chat_id={}, ON", chat_id.0));
            0
        };
        (next, queue_len)
    };
    let status = if next {
        "📋 Queue mode: ON\nMessages sent while AI is busy will be queued and processed in order.".to_string()
    } else if queue_len > 0 {
        format!("📋 Queue mode: OFF\n{} queued message(s) cleared.", queue_len)
    } else {
        "📋 Queue mode: OFF".to_string()
    };
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, &status).await)?;
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
    msg_debug(&format!("[handle_allowed] chat_id={}, arg={:?}", chat_id.0, arg));

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
                        results.push(format!("<code>{}</code> disabled", html_escape(tool_name)));
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
            "Public access <b>disabled</b> for this group.\nOnly the owner can use the bot.".to_string()
        }
        "" => {
            let data = state.lock().await;
            let is_public = data.settings.as_public_for_group_chat.get(&chat_key).copied().unwrap_or(PUBLIC_MODE_DEFAULT);
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

/// Resolve a model name with provider prefix.
/// Returns Err(provider_name) if the provider binary is unavailable, or Err("") if the format is invalid.
fn resolve_model_name(name: &str) -> Result<String, &'static str> {
    // Strip display-name suffix (" — Description") that users may copy-paste
    // from the /model help text (e.g. "gemini:gemini-2.5-flash-lite — Gemini 2.5 Flash Lite").
    let clean = name.split(" \u{2014} ").next().unwrap_or(name).trim();
    if claude::is_claude_model(Some(clean)) {
        if claude::is_claude_available() {
            Ok(clean.to_string())
        } else {
            Err("claude")
        }
    } else if codex::is_codex_model(Some(clean)) {
        if codex::is_codex_available() {
            Ok(clean.to_string())
        } else {
            Err("codex")
        }
    } else if gemini::is_gemini_model(Some(clean)) {
        if gemini::is_gemini_available() {
            Ok(clean.to_string())
        } else {
            Err("gemini")
        }
    } else if opencode::is_opencode_model(Some(clean)) {
        if opencode::is_opencode_available() {
            Ok(clean.to_string())
        } else {
            Err("opencode")
        }
    } else {
        Err("")  // invalid format
    }
}

/// Handle /model command
async fn handle_model_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let arg = text.strip_prefix("/model").unwrap_or("").trim();
    msg_debug(&format!("[handle_model_command] chat_id={}, arg={:?}", chat_id.0, arg));

    if arg.is_empty() {
        // Show current model + available providers
        let current = {
            let data = state.lock().await;
            get_model(&data.settings, chat_id)
        };
        let has_claude = claude::is_claude_available();
        let has_codex = codex::is_codex_available();
        let has_gemini = gemini::is_gemini_available();
        let has_opencode = opencode::is_opencode_available();

        let mut msg = match &current {
            Some(m) => format!("Current model: <b>{}</b>\n", m),
            None => {
                let default_provider = if has_claude { "claude" } else if has_codex { "codex" } else if has_gemini { "gemini" } else { "opencode" };
                format!("Current model: <b>default</b> ({})\n", default_provider)
            }
        };
        if has_claude {
            msg.push_str("\n<b>Claude:</b>\n");
            msg.push_str("<code>/model claude</code> — default\n");
            msg.push_str("<code>/model claude:sonnet</code> — Sonnet 4.6\n");
            msg.push_str("<code>/model claude:opus</code> — Opus 4.7\n");
            msg.push_str("<code>/model claude:haiku</code> — Haiku 4.5\n");
            msg.push_str("<code>/model claude:sonnet[1m]</code> — Sonnet 1M ctx\n");
        }
        if has_codex {
            msg.push_str("\n<b>Codex:</b>\n");
            msg.push_str("<code>/model codex</code> — default\n");
            msg.push_str("<code>/model codex:gpt-5.4</code> — Latest frontier agentic coding model\n");
            msg.push_str("<code>/model codex:gpt-5.3-codex</code> — Frontier Codex-optimized agentic coding model\n");
            msg.push_str("<code>/model codex:gpt-5.3-codex-spark</code> — Ultra-fast coding model\n");
            msg.push_str("<code>/model codex:gpt-5.2-codex</code> — Frontier agentic coding model\n");
            msg.push_str("<code>/model codex:gpt-5.2</code> — Optimized for professional work and long-running agents\n");
            msg.push_str("<code>/model codex:gpt-5.1-codex-max</code> — Codex-optimized model for deep and fast reasoning\n");
            msg.push_str("<code>/model codex:gpt-5.1-codex-mini</code> — Optimized for codex. Cheaper, faster, but less capable\n");
        }
        if has_gemini {
            msg.push_str("\n<b>Gemini:</b>\n");
            msg.push_str("<code>/model gemini</code> — default\n");
            msg.push_str("<code>/model gemini:gemini-3.1-flash-lite-preview</code> — Gemini 3.1 Flash Lite\n");
            msg.push_str("<code>/model gemini:gemini-3-pro-preview</code> — Gemini 3 Pro\n");
            msg.push_str("<code>/model gemini:gemini-3-flash-preview</code> — Gemini 3 Flash\n");
            msg.push_str("<code>/model gemini:gemini-2.5-pro</code> — Gemini 2.5 Pro\n");
            msg.push_str("<code>/model gemini:gemini-2.5-flash</code> — Gemini 2.5 Flash\n");
            msg.push_str("<code>/model gemini:gemini-2.5-flash-lite</code> — Gemini 2.5 Flash Lite\n");
        }
        if has_opencode {
            msg.push_str("\n<b>OpenCode:</b>\n");
            msg.push_str("<code>/model opencode</code> — default\n");
            for model_id in opencode::list_models() {
                msg.push_str(&format!("<code>/model opencode:{}</code>\n", model_id));
            }
        }

        if msg.len() <= TELEGRAM_MSG_LIMIT {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, msg)
                .parse_mode(ParseMode::Html)
                .await)?;
        } else {
            // Build plain text version for file attachment
            let plain = msg
                .replace("<b>", "").replace("</b>", "")
                .replace("<code>", "").replace("</code>", "");
            let tmp_path = std::env::temp_dir().join(format!("models_{}.txt", chat_id.0));
            if let Err(e) = std::fs::write(&tmp_path, &plain) {
                msg_debug(&format!("[handle_model_command] failed to write tmp file: {}", e));
                return Ok(());
            }
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_document", bot.send_document(
                chat_id,
                teloxide::types::InputFile::file(&tmp_path),
            ).await)?;
            let _ = std::fs::remove_file(&tmp_path);
        }
        return Ok(());
    }

    // NOTE: `/model default` and `/model reset` were intentionally removed.
    // The new provider-prefixed format (claude:xxx / codex:xxx) replaces the old bare model names.
    // Users should use `/model claude` or `/model codex` to switch to default models.

    // Set model
    match resolve_model_name(arg) {
        Ok(model_id) => {
            {
                let mut data = state.lock().await;
                // If provider changed, clear session_id to avoid cross-provider resume
                let old_model = get_model(&data.settings, chat_id);
                let old_provider = provider_from_model(old_model.as_deref());
                let new_provider = provider_from_model(Some(&model_id));
                let provider_changed = old_provider != new_provider;
                msg_debug(&format!("[handle_model_command] old_model={:?}, old_provider={}, new_provider={}, provider_changed={}",
                    old_model, old_provider, new_provider, provider_changed));
                if provider_changed {
                    if let Some(session) = data.sessions.get_mut(&chat_id) {
                        msg_debug(&format!("[handle_model_command] provider changed → clearing session + history (len={}, old_sid={:?}, old_path={:?})",
                            session.history.len(), session.session_id, session.current_path));
                        session.session_id = None;
                        session.current_path = None;
                        session.history.clear();
                    }
                }
                data.settings.models.insert(chat_id.0.to_string(), model_id.clone());
                save_bot_settings(token, &data.settings);
            }
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, format!("Model set to <b>{model_id}</b>."))
                .parse_mode(ParseMode::Html)
                .await)?;
        }
        Err(provider) if !provider.is_empty() => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, format!("{provider} provider is not installed."))
                .await)?;
        }
        Err(_) => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id,
                "Invalid format. Use:\n\
                 <code>/model claude</code> or <code>/model claude:&lt;model&gt;</code>\n\
                 <code>/model codex</code> or <code>/model codex:&lt;model&gt;</code>\n\
                 <code>/model gemini</code> or <code>/model gemini:&lt;model&gt;</code>\n\
                 <code>/model opencode</code> or <code>/model opencode:&lt;model&gt;</code>")
                .parse_mode(ParseMode::Html)
                .await)?;
        }
    }

    Ok(())
}

/// Dispatch pending loop feedback (stored in loop_feedback HashMap).
/// Called after loop verification determines the task is incomplete.
/// Uses the same boxed-future pattern as process_next_queued_message
/// to satisfy Send bounds (handle_text_message's future is not Send
/// in the outer tokio::spawn context).
fn dispatch_loop_feedback<'a>(
    bot: &'a Bot,
    chat_id: ChatId,
    state: &'a SharedState,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let feedback = {
            let mut data = state.lock().await;
            let fb = data.loop_feedback.remove(&chat_id);
            // Insert placeholder cancel_token so incoming messages see "busy"
            // during the gap before handle_text_message creates its own token.
            // Same pattern as process_next_queued_message (line 6628).
            if fb.is_some() {
                data.cancel_tokens.insert(chat_id, Arc::new(CancelToken::new()));
                msg_debug(&format!("[loop:dispatch] chat_id={}, placeholder cancel_token inserted", chat_id.0));
            }
            fb
        };
        if let Some((text, user_display_name)) = feedback {
            msg_debug(&format!("[loop:dispatch] chat_id={}, feedback_len={}", chat_id.0, text.len()));
            // from_queue=true: handle_text_message will overwrite the placeholder
            // cancel_token instead of treating it as "another task active".
            if let Err(e) = handle_text_message(bot, chat_id, &text, state, &user_display_name, true).await {
                msg_debug(&format!("[loop:dispatch] chat_id={}, handle_text_message FAILED: {}", chat_id.0, e));
            }
        } else {
            msg_debug(&format!("[loop:dispatch] chat_id={}, no feedback found (may have been cleared by /stop)", chat_id.0));
            // Fall through to normal queue processing
            process_next_queued_message(bot, chat_id, state).await;
        }
    })
}

/// After an AI request finishes (normal completion or cancellation), check the message queue
/// and process the next queued message if queue mode is enabled.
/// This must be called AFTER cancel_tokens.remove().
/// Returns a boxed future to break the recursive async cycle with handle_text_message.
fn process_next_queued_message<'a>(
    bot: &'a Bot,
    chat_id: ChatId,
    state: &'a SharedState,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        msg_debug(&format!("[queue:next] chat_id={}, checking for next queued message", chat_id.0));
        let next_msg = {
            let mut data = state.lock().await;
            // Skip if another task is already processing this chat
            if data.cancel_tokens.contains_key(&chat_id) {
                msg_debug(&format!("[queue:next] chat_id={}, cancel_token exists (another task active), skipping", chat_id.0));
                return;
            }
            let qkey = chat_id.0.to_string();
            let queue_enabled = data.settings.queue.get(&qkey).copied().unwrap_or(QUEUE_MODE_DEFAULT);
            if !queue_enabled {
                msg_debug(&format!("[queue:next] chat_id={}, queue mode OFF, skipping", chat_id.0));
                return;
            }
            let queue_len_before = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
            let msg = data.message_queues.get_mut(&chat_id).and_then(|q| q.pop_front());
            // Restore pending_uploads captured at queue time so handle_text_message picks them up
            if let Some(ref m) = msg {
                msg_debug(&format!("[queue:next] chat_id={}, popped message: text={:?}, user={:?}, uploads={}, queue_before={}", chat_id.0, truncate_str(&m.text, 60), m.user_display_name, m.pending_uploads.len(), queue_len_before));
                if !m.pending_uploads.is_empty() {
                    if let Some(session) = data.sessions.get_mut(&chat_id) {
                        session.pending_uploads.extend(m.pending_uploads.iter().cloned());
                        msg_debug(&format!("[queue:next] chat_id={}, restored {} pending_uploads to session", chat_id.0, m.pending_uploads.len()));
                    } else {
                        msg_debug(&format!("[queue:next] chat_id={}, WARNING: no session to restore uploads to", chat_id.0));
                    }
                }
                // Insert placeholder cancel_token so new messages see "busy" during Dequeued notification.
                // Without this, messages arriving between pop and handle_text_message's cancel_token insert
                // would see "not busy" and bypass the queue (FIFO violation).
                // handle_text_message will overwrite this with its own real cancel_token.
                data.cancel_tokens.insert(chat_id, Arc::new(CancelToken::new()));
                msg_debug(&format!("[queue:next] chat_id={}, placeholder cancel_token inserted", chat_id.0));
            } else {
                msg_debug(&format!("[queue:next] chat_id={}, queue empty (len was {})", chat_id.0, queue_len_before));
            }
            // Clean up empty queue entry to prevent memory leak
            if data.message_queues.get(&chat_id).map_or(false, |q| q.is_empty()) {
                data.message_queues.remove(&chat_id);
                msg_debug(&format!("[queue:next] chat_id={}, removed empty queue entry", chat_id.0));
            }
            msg
        };

        if let Some(queued) = next_msg {
            msg_debug(&format!("[queue:next] chat_id={}, dispatching id={}, text={:?}", chat_id.0, queued.id, truncate_str(&queued.text, 60)));
            shared_rate_limit_wait(state, chat_id).await;
            let _ = tg!("send_message", bot.send_message(chat_id, &format!("Dequeued ({})", queued.id)).await);

            if let Err(e) = handle_text_message(bot, chat_id, &queued.text, state, &queued.user_display_name, true).await {
                msg_debug(&format!("[queue:next] chat_id={}, id={}, handle_text_message FAILED: {}", chat_id.0, queued.id, e));
            } else {
                msg_debug(&format!("[queue:next] chat_id={}, id={}, handle_text_message completed OK", chat_id.0, queued.id));
            }
        }
    })
}

/// Handle regular text messages - send to Claude AI
async fn handle_text_message(
    bot: &Bot,
    chat_id: ChatId,
    user_text: &str,
    state: &SharedState,
    user_display_name: &str,
    from_queue: bool,
) -> ResponseResult<()> {
    msg_debug(&format!("[handle_text_message] START chat_id={}, user_text={:?}, from_queue={}",
        chat_id.0, truncate_str(user_text, 100), from_queue));
    ai_trace(&format!("[START] chat_id={}, user_text={:?}, from_queue={}", chat_id.0, truncate_str(user_text, 100), from_queue));

    // Register cancel token early (prevents duplicate requests while waiting for group lock)
    let cancel_token = Arc::new(CancelToken::new());
    let busy_notify: Option<(bool, Option<String>)> = {
        let mut data = state.lock().await;
        // For queued messages: check if placeholder was cancelled (e.g., /stop during dequeue window)
        if from_queue {
            let placeholder_cancelled = data.cancel_tokens.get(&chat_id)
                .map(|t| t.cancelled.load(Ordering::Relaxed))
                .unwrap_or(false);
            if placeholder_cancelled {
                msg_debug(&format!("[handle_text_message] chat_id={}, placeholder cancelled during dequeue", chat_id.0));
                // Clean up pending_uploads restored for this cancelled message
                if let Some(session) = data.sessions.get_mut(&chat_id) {
                    if !session.pending_uploads.is_empty() {
                        msg_debug(&format!("[handle_text_message] chat_id={}, clearing {} pending_uploads from cancelled dequeue", chat_id.0, session.pending_uploads.len()));
                        session.pending_uploads.clear();
                    }
                }
                data.cancel_tokens.remove(&chat_id);
                // Clean up orphaned "Stopping..." message from /stop during dequeue window
                let stop_msg = data.stop_message_ids.remove(&chat_id);
                drop(data);
                if let Some(msg_id) = stop_msg {
                    shared_rate_limit_wait(state, chat_id).await;
                    let _ = tg!("delete_message", bot.delete_message(chat_id, msg_id).await);
                }
                process_next_queued_message(bot, chat_id, state).await;
                return Ok(());
            }
        }
        // For non-queued messages: if another task is active, queue or notify
        if !from_queue && data.cancel_tokens.contains_key(&chat_id) {
            msg_debug(&format!("[handle_text_message] chat_id={}, cancel_token exists (busy)", chat_id.0));
            let qkey = chat_id.0.to_string();
            let qmode = data.settings.queue.get(&qkey).copied().unwrap_or(QUEUE_MODE_DEFAULT);
            let qid = if qmode {
                let cur_len = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
                if cur_len < MAX_QUEUE_SIZE {
                    let uploads = data.sessions.get_mut(&chat_id)
                        .map(|s| std::mem::take(&mut s.pending_uploads))
                        .unwrap_or_default();
                    let id = generate_queue_id();
                    data.message_queues.entry(chat_id)
                        .or_insert_with(std::collections::VecDeque::new)
                        .push_back(QueuedMessage {
                            id: id.clone(),
                            text: user_text.to_string(),
                            user_display_name: user_display_name.to_string(),
                            pending_uploads: uploads,
                        });
                    msg_debug(&format!("[handle_text_message] chat_id={}, QUEUED id={}", chat_id.0, id));
                    Some(id)
                } else {
                    msg_debug(&format!("[handle_text_message] chat_id={}, queue FULL", chat_id.0));
                    None
                }
            } else {
                None
            };
            Some((qmode, qid))
        } else {
            data.cancel_tokens.insert(chat_id, cancel_token.clone());
            None
        }
    };
    if let Some((queue_enabled, queued_id)) = busy_notify {
        shared_rate_limit_wait(state, chat_id).await;
        if queue_enabled {
            if let Some(qid) = queued_id {
                let preview = truncate_str(user_text, 30);
                tg!("send_message", bot.send_message(chat_id, &format!("Queued ({qid}) \"{preview}\"\n- /stopall to cancel all\n- /stop_{qid} to cancel this"))
                    .await)?;
            } else {
                tg!("send_message", bot.send_message(chat_id, &format!("Queue full (max {}). Use /stopall to clear.", MAX_QUEUE_SIZE))
                    .await)?;
            }
        } else {
            tg!("send_message", bot.send_message(chat_id, "AI request in progress. Use /stop to cancel.")
                .await)?;
        }
        return Ok(());
    }

    // Acquire group chat lock (serializes processing across bots in the same group chat)
    let group_lock = acquire_group_chat_lock(chat_id.0).await;

    // Check if cancelled during lock wait (e.g., user sent /stop)
    if cancel_token.cancelled.load(Ordering::Relaxed) {
        msg_debug(&format!("[queue:trigger] chat_id={}, source=text_cancelled_during_lock", chat_id.0));
        { let mut data = state.lock().await; data.cancel_tokens.remove(&chat_id); }
        drop(group_lock); // release before queue processing to avoid deadlock
        process_next_queued_message(bot, chat_id, state).await;
        return Ok(());
    }

    // Get session info, allowed tools, model, pending uploads, history, instruction, and bot_username (drop lock before any await)
    let (session_info, allowed_tools, pending_uploads, model, history, instruction, context_count, bot_username_for_prompt, bot_display_name_for_prompt, chrome_enabled) = {
        let mut data = state.lock().await;
        let info = data.sessions.get(&chat_id).and_then(|session| {
            session.current_path.as_ref().map(|_| {
                (session.session_id.clone(), session.current_path.clone().unwrap_or_default())
            })
        });
        let tools = get_allowed_tools(&data.settings, chat_id);
        let mdl = get_model(&data.settings, chat_id);
        let hist = data.sessions.get(&chat_id)
            .map(|s| s.history.clone())
            .unwrap_or_default();
        // Drain pending uploads so they are sent to Claude exactly once
        let uploads = data.sessions.get_mut(&chat_id)
            .map(|s| std::mem::take(&mut s.pending_uploads))
            .unwrap_or_default();
        let instr = data.settings.instructions.get(&chat_id.0.to_string()).cloned();
        let ctx_count = data.settings.context.get(&chat_id.0.to_string()).copied().unwrap_or(12);
        let buname = data.bot_username.clone();
        let bdname = data.bot_display_name.clone();
        let chrome = data.settings.use_chrome.get(&chat_id.0.to_string()).copied().unwrap_or(false);
        msg_debug(&format!("[handle_text_message] session_id={:?}, current_path={:?}, model={:?}, uploads={}, history_len={}, instruction={:?}",
            info.as_ref().map(|(sid, _)| sid), info.as_ref().map(|(_, p)| p), mdl, uploads.len(), hist.len(), instr.is_some()));
        (info, tools, uploads, mdl, hist, instr, ctx_count, buname, bdname, chrome)
    };

    let (session_id, current_path) = match session_info {
        Some(info) => info,
        None => {
            // Auto-create a workspace session instead of rejecting the message
            let Some((auto_sid, auto_current)) = auto_create_workspace_session(state, chat_id, bot.token()).await else {
                {
                    let mut data = state.lock().await;
                    data.cancel_tokens.remove(&chat_id);
                }
                shared_rate_limit_wait(state, chat_id).await;
                let _ = tg!("send_message", bot.send_message(chat_id, "Failed to create workspace.")
                    .await);
                drop(group_lock);
                process_next_queued_message(bot, chat_id, state).await;
                return Ok(());
            };
            (auto_sid, auto_current)
        }
    };

    // Note: user message is NOT added to history here.
    // It will be added together with the assistant response in the spawned task,
    // only on successful completion. On cancel, nothing is recorded.

    // Send placeholder message (update shared timestamp so spawned task knows)
    shared_rate_limit_wait(state, chat_id).await;
    let placeholder = match tg!("send_message", bot.send_message(chat_id, "...").await) {
        Ok(m) => m,
        Err(e) => {
            msg_debug(&format!("[queue:trigger] chat_id={}, source=text_placeholder_error", chat_id.0));
            { let mut data = state.lock().await; data.cancel_tokens.remove(&chat_id); }
            drop(group_lock); // release before queue processing to avoid deadlock
            process_next_queued_message(bot, chat_id, state).await;
            return Err(e);
        }
    };
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

    // Build system prompt with sendfile and schedule instructions
    let bot_key_for_prompt = token_hash(bot.token());
    let platform = capitalize_platform(detect_platform(bot.token()));
    let role = match &instruction {
        Some(instr) => format!("You are chatting with a user through {}.\n\nUser's instruction for this chat:\n{}", platform, instr),
        None => format!("You are chatting with a user through {}.", platform),
    };
    ai_trace(&format!("[PROMPT] platform={}, role_len={}, path={}, bot_key={}", platform, role.len(), current_path, bot_key_for_prompt));
    let system_prompt_owned = build_system_prompt(
        &role,
        &current_path, chat_id.0, &bot_key_for_prompt, &disabled_notice,
        session_id.as_deref(), &bot_username_for_prompt, &bot_display_name_for_prompt,
        Some(user_text), context_count, &platform,
    );

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();

    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();

    // Run AI backend in a blocking thread
    let model_clone = model.clone();
    let history_clone = history;
    msg_debug(&format!("[handle_text_message] prompt_len={}, system_prompt_len={}, session_id={:?}, path={}, history_len={}",
        context_prompt.len(), system_prompt_owned.len(), session_id_clone, current_path_clone, history_clone.len()));
    tokio::task::spawn_blocking(move || {
        let provider = detect_provider(model_clone.as_deref());
        msg_debug(&format!("[handle_text_message] provider={}, model={:?}", provider, model_clone));
        ai_trace(&format!("[EXEC] provider={}, model={:?}, prompt_len={}, system_prompt_len={}, history_len={}, session={:?}, path={}",
            provider, model_clone, context_prompt.len(), system_prompt_owned.len(), history_clone.len(), session_id_clone, current_path_clone));
        let result = if provider == "opencode" {
            let opencode_model = model_clone.as_deref().and_then(opencode::strip_opencode_prefix);
            msg_debug(&format!("[handle_text_message] → opencode::execute, opencode_model={:?}, session_id={:?}, path={}, prompt_len={}, system_prompt_len={}",
                opencode_model, session_id_clone, current_path_clone, context_prompt.len(), system_prompt_owned.len()));
            opencode::execute_command_streaming(
                &context_prompt,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                opencode_model,
                false,
            )
        } else if provider == "gemini" {
            let gemini_model = model_clone.as_deref().and_then(gemini::strip_gemini_prefix);
            msg_debug(&format!("[handle_text_message] → gemini::execute, gemini_model={:?}, session_id={:?}, path={}, prompt_len={}, system_prompt_len={}",
                gemini_model, session_id_clone, current_path_clone, context_prompt.len(), system_prompt_owned.len()));
            gemini::execute_command_streaming(
                &context_prompt,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                gemini_model,
                false,
            )
        } else if provider == "codex" {
            let codex_model = model_clone.as_deref().and_then(codex::strip_codex_prefix);
            // System prompt is always passed via -c model_instructions_file (handles both new & resume).
            // For new sessions without session_id, inject conversation history into the prompt.
            let codex_system_prompt = format!("{}{}", system_prompt_owned, codex_extra_instructions());
            let is_resume = session_id_clone.is_some();
            let has_history = !history_clone.is_empty();
            msg_debug(&format!("[handle_text_message] codex: is_resume={}, has_history={}, history_len={}, sp_len={}",
                is_resume, has_history, history_clone.len(), codex_system_prompt.len()));
            // Inject conversation history only for new sessions (no session_id) with prior history.
            // Resumed sessions rely on Codex's native conversation management.
            let codex_prompt = if session_id_clone.is_none() && !history_clone.is_empty() {
                msg_debug("[handle_text_message] codex: INJECTING history into prompt (new session with history)");
                let mut conv = String::new();
                conv.push_str("<conversation_history>\n");
                for item in &history_clone {
                    let role = match item.item_type {
                        HistoryType::User => "User",
                        HistoryType::Assistant => "Assistant",
                        HistoryType::ToolUse => "ToolUse",
                        HistoryType::ToolResult => "ToolResult",
                        _ => continue,  // skip Error, System
                    };
                    conv.push_str(&format!("[{}]: {}\n", role, item.content));
                }
                conv.push_str("</conversation_history>\n\n");
                conv.push_str(&context_prompt);
                conv
            } else {
                if is_resume {
                    msg_debug("[handle_text_message] codex: RESUME path — no history injection, sp via -c file");
                } else {
                    msg_debug("[handle_text_message] codex: NEW session, no history — sp via -c file");
                }
                context_prompt.clone()
            };
            msg_debug(&format!("[handle_text_message] → codex::execute, codex_model={:?}, codex_prompt_len={}, resume={}, system_prompt_passed=true",
                codex_model, codex_prompt.len(), is_resume));
            codex::execute_command_streaming(
                &codex_prompt,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&codex_system_prompt),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                codex_model,
                false,
            )
        } else {
            let claude_model = model_clone.as_deref().and_then(claude::strip_claude_prefix);
            msg_debug(&format!("[handle_text_message] → claude::execute, claude_model={:?}", claude_model));
            claude::execute_command_streaming(
                &context_prompt,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                claude_model,
                false,
                chrome_enabled,
            )
        };

        match &result {
            Ok(()) => {
                msg_debug("[handle_text_message] execute completed OK");
                ai_trace("[EXEC] completed OK");
            }
            Err(e) => {
                msg_debug(&format!("[handle_text_message] execute error: {}", e));
                ai_trace(&format!("[EXEC] ERROR: {}", e));
            }
        }
        if let Err(e) = result {
            let _ = tx.send(StreamMessage::Error { message: e, stdout: String::new(), stderr: String::new(), exit_code: None });
        }
    });

    // Spawn the polling loop as a separate task so the handler returns immediately.
    // This allows teloxide's per-chat worker to process subsequent messages (e.g. /stop).
    let bot_owned = bot.clone();
    let state_owned = state.clone();
    let user_text_owned = user_text.to_string();
    let bot_username_for_log = bot_username_for_prompt.clone();
    let bot_display_name_for_log = bot_display_name_for_prompt.clone();
    let user_display_name_owned = user_display_name.to_string();
    let provider_str: &'static str = detect_provider(model.as_deref());
    tokio::spawn(async move {
        let _group_lock = group_lock; // hold group chat lock until task ends
        const SPINNER: &[&str] = &[
            "🕐 P",           "🕑 Pr",          "🕒 Pro",
            "🕓 Proc",        "🕔 Proce",       "🕕 Proces",
            "🕖 Process",     "🕗 Processi",    "🕘 Processin",
            "🕙 Processing",  "🕚 Processing.", "🕛 Processing..",
        ];
        let mut full_response = String::new();
        let mut raw_entries: Vec<RawPayloadEntry> = Vec::new();
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut loop_reinjected = false;
        let mut new_session_id: Option<String> = None;
        let mut spin_idx: usize = 0;
        let mut pending_cokacdir = false;
        let mut suppress_tool_display = false;
        let mut last_tool_name: String = String::new();
        let mut placeholder_msg_id = placeholder_msg_id;
        let mut last_confirmed_len: usize = 0;

        let (polling_time_ms, silent_mode) = {
            let data = state_owned.lock().await;
            (data.polling_time_ms, is_silent(&data.settings, chat_id))
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
                                    msg_debug(&format!("[polling] Init: session_id={}", sid));
                                    ai_trace(&format!("[STREAM] Init: session_id={}", sid));
                                    new_session_id = Some(sid);
                                }
                                StreamMessage::Text { content } => {
                                    msg_debug(&format!("[polling] Text: {} chars, preview={:?}",
                                        content.len(), truncate_str(&content, 80)));
                                    ai_trace(&format!("[STREAM] Text: {} chars, total_so_far={}", content.len(), full_response.len() + content.len()));
                                    raw_entries.push(RawPayloadEntry { tag: "Text".into(), content: content.clone() });
                                    let _fr_before = full_response.len();
                                    full_response.push_str(&content);
                                    msg_debug(&format!("[fr_trace][{}] +Text: added={}, preview={:?}, total={} (was {})",
                                        chat_id.0, content.len(), truncate_str(&content, 200), full_response.len(), _fr_before));
                                }
                                StreamMessage::ToolUse { name, input } => {
                                    pending_cokacdir = detect_cokacdir_command(&name, &input);
                                    suppress_tool_display = detect_chat_log_read(&name, &input);
                                    last_tool_name = name.clone();
                                    let summary = format_tool_input(&name, &input);
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!("  [{ts}]   ⚙ {name}: {summary}");
                                    msg_debug(&format!("[polling] ToolUse: name={}, input_preview={:?}, pending_cokacdir={}, silent_mode={}, response_len={}, ends_with_nl={}",
                                        name, truncate_str(&input, 200), pending_cokacdir, silent_mode, full_response.len(), full_response.ends_with('\n')));
                                    raw_entries.push(RawPayloadEntry { tag: "ToolUse".into(), content: format!("{}: {}", name, input) });
                                    if !pending_cokacdir && !silent_mode {
                                        let _fr_before = full_response.len();
                                        if name == "Bash" {
                                            full_response.push_str(&format!("\n\n```\n{}\n```\n", format_bash_command(&input)));
                                            msg_debug(&format!("[fr_trace][{}] +ToolUse/Bash: added={}, cmd={:?}, total={} (was {})",
                                                chat_id.0, full_response.len() - _fr_before, truncate_str(&input, 100), full_response.len(), _fr_before));
                                        } else {
                                            full_response.push_str(&format!("\n\n⚙️ {}\n", summary));
                                            msg_debug(&format!("[fr_trace][{}] +ToolUse/{}: added={}, summary={:?}, total={} (was {})",
                                                chat_id.0, name, full_response.len() - _fr_before, truncate_str(&summary, 100), full_response.len(), _fr_before));
                                        }
                                    } else if !pending_cokacdir && silent_mode && !full_response.is_empty() && !full_response.ends_with('\n') {
                                        msg_debug(&format!("[polling] silent mode: inserting \\n\\n after tool_use={}", name));
                                        full_response.push_str("\n\n");
                                        msg_debug(&format!("[fr_trace][{}] +ToolUse/silent_nl: added=2, total={}", chat_id.0, full_response.len()));
                                    } else if silent_mode {
                                        msg_debug(&format!("[polling] silent mode: skipped \\n\\n (pending_cokacdir={}, empty={}, ends_nl={})",
                                            pending_cokacdir, full_response.is_empty(), full_response.ends_with('\n')));
                                    }
                                }
                                StreamMessage::ToolResult { content, is_error } => {
                                    msg_debug(&format!("[polling] ToolResult: is_error={}, content_len={}, pending_cokacdir={}, last_tool={}", is_error, content.len(), pending_cokacdir, last_tool_name));
                                    if is_error {
                                        msg_debug(&format!("[polling] ToolResult ERROR: last_tool={}, content_preview={:?}", last_tool_name, truncate_str(&content, 300)));
                                    }
                                    raw_entries.push(RawPayloadEntry { tag: "ToolResult".into(), content: format!("is_error={}, content={}", is_error, content) });
                                    let _fr_before = full_response.len();
                                    if std::mem::take(&mut pending_cokacdir) {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        if std::mem::take(&mut suppress_tool_display) {
                                            println!("  [{ts}]   ↩ cokacdir (chat_log, suppressed)");
                                            msg_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir_suppressed: added=0, total={}", chat_id.0, full_response.len()));
                                        } else {
                                            println!("  [{ts}]   ↩ cokacdir: {content}");
                                            let formatted = format_cokacdir_result(&content);
                                            if !formatted.is_empty() {
                                                full_response.push_str(&format!("\n{}\n", formatted));
                                                msg_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir: added={}, preview={:?}, total={} (was {})",
                                                    chat_id.0, full_response.len() - _fr_before, truncate_str(&formatted, 200), full_response.len(), _fr_before));
                                            } else {
                                                msg_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir: formatted_empty, added=0, total={}", chat_id.0, full_response.len()));
                                            }
                                        }
                                    } else if is_error && !silent_mode {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        println!("  [{ts}]   ✗ Error: {content}");
                                        let truncated = truncate_str(&content, 500);
                                        if truncated.contains('\n') {
                                            full_response.push_str(&format!("\n```\n{}\n```\n", truncated));
                                        } else {
                                            full_response.push_str(&format!("\n`{}`\n\n", truncated));
                                        }
                                        msg_debug(&format!("[fr_trace][{}] +ToolResult/error({}): added={}, preview={:?}, total={} (was {})",
                                            chat_id.0, last_tool_name, full_response.len() - _fr_before, truncate_str(&truncated, 200), full_response.len(), _fr_before));
                                    } else if !silent_mode {
                                        if last_tool_name == "Read" {
                                            full_response.push_str(&format!("\n✅ `{} bytes`\n\n", content.len()));
                                            msg_debug(&format!("[fr_trace][{}] +ToolResult/Read: added={}, content_bytes={}, total={} (was {})",
                                                chat_id.0, full_response.len() - _fr_before, content.len(), full_response.len(), _fr_before));
                                        } else if !content.is_empty() {
                                            let truncated = truncate_str(&content, 300);
                                            if truncated.contains('\n') {
                                                full_response.push_str(&format!("\n```\n{}\n```\n", truncated));
                                            } else {
                                                full_response.push_str(&format!("\n✅ `{}`\n\n", truncated));
                                            }
                                            msg_debug(&format!("[fr_trace][{}] +ToolResult/{}(normal): added={}, raw_content_len={}, preview={:?}, total={} (was {})",
                                                chat_id.0, last_tool_name, full_response.len() - _fr_before, content.len(), truncate_str(&truncated, 200), full_response.len(), _fr_before));
                                        } else {
                                            msg_debug(&format!("[fr_trace][{}] +ToolResult/{}(normal): content_empty, added=0, total={}", chat_id.0, last_tool_name, full_response.len()));
                                        }
                                    } else {
                                        msg_debug(&format!("[fr_trace][{}] +ToolResult/silent: skipped, last_tool={}, content_len={}, total={}", chat_id.0, last_tool_name, content.len(), full_response.len()));
                                    }
                                }
                                StreamMessage::TaskNotification { summary, .. } => {
                                    if !summary.is_empty() {
                                        raw_entries.push(RawPayloadEntry { tag: "TaskNotification".into(), content: summary.clone() });
                                        let _fr_before = full_response.len();
                                        full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                        msg_debug(&format!("[fr_trace][{}] +TaskNotification: added={}, summary={:?}, total={} (was {})",
                                            chat_id.0, full_response.len() - _fr_before, truncate_str(&summary, 200), full_response.len(), _fr_before));
                                    }
                                }
                                StreamMessage::Done { result, session_id: sid } => {
                                    msg_debug(&format!("[polling] Done: result_len={}, session_id={:?}, full_response_len={}",
                                        result.len(), sid, full_response.len()));
                                    ai_trace(&format!("[STREAM] Done: result_len={}, full_response_len={}, session_id={:?}", result.len(), full_response.len(), sid));
                                    if !result.is_empty() && full_response.is_empty() {
                                        msg_debug(&format!("[polling] Done: fallback full_response = result ({})", result.len()));
                                        full_response = result.clone();
                                        msg_debug(&format!("[fr_trace][{}] +Done/fallback: set={}, preview={:?}, total={}",
                                            chat_id.0, result.len(), truncate_str(&full_response, 200), full_response.len()));
                                    } else if !result.is_empty() {
                                        msg_debug(&format!("[fr_trace][{}] Done/discarded: result_len={} discarded (full_response already has {})",
                                            chat_id.0, result.len(), full_response.len()));
                                    }
                                    if !result.is_empty() && raw_entries.is_empty() {
                                        raw_entries.push(RawPayloadEntry { tag: "Text".into(), content: result });
                                    }
                                    if let Some(s) = sid {
                                        new_session_id = Some(s);
                                    }
                                    msg_debug(&format!("[fr_trace][{}] =DONE: final_total={}", chat_id.0, full_response.len()));
                                    done = true;
                                }
                                StreamMessage::Error { message, stdout, stderr, exit_code } => {
                                    msg_debug(&format!("[polling] Error: message={}, exit_code={:?}, stdout_len={}, stderr_len={}",
                                        message, exit_code, stdout.len(), stderr.len()));
                                    ai_trace(&format!("[STREAM] Error: message={}, exit_code={:?}, stdout_len={}, stderr_len={}", message, exit_code, stdout.len(), stderr.len()));
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
                                    msg_debug(&format!("[fr_trace][{}] +Error: set={}, stdout_len={}, stderr_len={}, total={}",
                                        chat_id.0, full_response.len(), stdout_display.len(), stderr_display.len(), full_response.len()));
                                    raw_entries.push(RawPayloadEntry { tag: "Error".into(), content: format!("exit_code={}, message={}, stdout={}, stderr={}", code_display, message, stdout_display, stderr_display) });
                                    done = true;
                                }
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            ai_trace(&format!("[STREAM] Channel DISCONNECTED: full_response_len={}", full_response.len()));
                            done = true;
                            break;
                        }
                    }
                }

                if !done {
                    // ── Rolling placeholder pattern (unified for all chats) ──
                    let threshold = file_attach_threshold();
                    let delta_end = floor_char_boundary(&full_response, full_response.len().min(threshold));
                    if delta_end > last_confirmed_len {
                        // New content arrived — finalize current placeholder with delta
                        // Cap delta to threshold boundary to prevent message flood
                        let delta = &full_response[last_confirmed_len..delta_end];
                        let normalized_delta = normalize_empty_lines(delta);
                        let html_delta = markdown_to_telegram_html(&normalized_delta);
                        if html_delta.trim().is_empty() {
                            // Delta is whitespace-only after normalization — skip edit, just update position
                            msg_debug(&format!("[rolling_ph] SKIP empty delta: placeholder_msg_id={}, delta_bytes={}, confirmed={}→{}",
                                placeholder_msg_id, delta.len(), last_confirmed_len, delta_end));
                            last_confirmed_len = delta_end;
                        } else {
                            msg_debug(&format!("[rolling_ph] EDIT delta: placeholder_msg_id={}, delta_len={}, html_len={}, confirmed={}→{}",
                                placeholder_msg_id, normalized_delta.len(), html_delta.len(), last_confirmed_len, full_response.len()));
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            if html_delta.len() <= TELEGRAM_MSG_LIMIT {
                                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_delta)
                                    .parse_mode(ParseMode::Html).await);
                            } else {
                                // Delta too large for single edit — send via send_long_message
                                if send_long_message(&bot_owned, chat_id, &html_delta, Some(ParseMode::Html), &state_owned).await.is_ok() {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                                } else {
                                    let truncated_delta = truncate_str(&normalized_delta, TELEGRAM_MSG_LIMIT);
                                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &truncated_delta).await);
                                }
                            }
                            last_confirmed_len = delta_end;
                            // Create new placeholder for next cycle
                            let old_ph_id = placeholder_msg_id;
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            match tg!("send_message", bot_owned.send_message(chat_id, "...").await) {
                                Ok(new_ph) => {
                                    placeholder_msg_id = new_ph.id;
                                    msg_debug(&format!("[rolling_ph] NEW placeholder: old_msg_id={}, new_msg_id={}", old_ph_id, placeholder_msg_id));
                                }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!("  [{ts}]   ⚠ new placeholder failed: {e}");
                                    msg_debug(&format!("[rolling_ph] NEW placeholder FAILED: keeping msg_id={}, err={}", placeholder_msg_id, e));
                                }
                            }
                            last_edit_text.clear();
                            spin_idx = 0;
                        }
                    } else {
                        // No new content — spinner update on current placeholder
                        let indicator = SPINNER[spin_idx % SPINNER.len()];
                        spin_idx += 1;
                        let display_text = indicator.to_string();
                        if display_text != last_edit_text {
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let html_text = markdown_to_telegram_html(&display_text);
                            if let Err(e) = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_text)
                                .parse_mode(ParseMode::Html).await)
                            {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}]   ⚠ edit_message failed (streaming): {e}");
                            }
                            last_edit_text = display_text;
                        } else {
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!("send_chat_action", bot_owned.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await);
                        }
                    }
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
                    ai_trace(&format!("[FINAL] full_response is EMPTY → (No response). cancelled={}, last_confirmed_len={}", cancelled, last_confirmed_len));
                    full_response = "(No response)".to_string();
                    msg_debug(&format!("[fr_trace][{}] =NoResponse: set to '(No response)', cancelled={}", chat_id.0, cancelled));
                } else {
                    ai_trace(&format!("[FINAL] full_response_len={}, last_confirmed_len={}, remaining_len={}",
                        full_response.len(), last_confirmed_len, full_response.len().saturating_sub(last_confirmed_len)));
                }

                let final_response = normalize_empty_lines(&full_response);

                // ── Send only remaining delta (unified rolling placeholder) ──
                if full_response.len() < last_confirmed_len || !full_response.is_char_boundary(last_confirmed_len) { last_confirmed_len = 0; }
                let remaining = &full_response[last_confirmed_len..];
                msg_debug(&format!("[rolling_ph] FINAL: placeholder_msg_id={}, confirmed={}, total={}, remaining_len={}",
                    placeholder_msg_id, last_confirmed_len, full_response.len(), remaining.trim().len()));
                if remaining.trim().is_empty() {
                    // No new content — delete the spinner placeholder
                    msg_debug(&format!("[rolling_ph] FINAL DELETE placeholder: msg_id={}", placeholder_msg_id));
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                } else if should_attach_response_as_file(full_response.len(), provider_str) {
                    // Response too large — send as file attachment
                    msg_debug(&format!("[rolling_ph] FINAL FILE ATTACH: total={}", full_response.len()));
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, "\u{1f4c4} Response attached as file").await);
                    send_response_as_file(&bot_owned, chat_id, &final_response, &state_owned, "response").await;
                } else {
                    let normalized_remaining = normalize_empty_lines(remaining);
                    let html_remaining = markdown_to_telegram_html(&normalized_remaining);
                    msg_debug(&format!("[rolling_ph] FINAL EDIT placeholder: msg_id={}, html_len={}", placeholder_msg_id, html_remaining.len()));
                    if html_remaining.len() <= TELEGRAM_MSG_LIMIT {
                        if let Err(e) = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_remaining)
                            .parse_mode(ParseMode::Html).await)
                        {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!("  [{ts}]   ⚠ edit_message failed (final HTML): {e}");
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &normalized_remaining).await);
                        }
                    } else {
                        let send_result = send_long_message(&bot_owned, chat_id, &html_remaining, Some(ParseMode::Html), &state_owned).await;
                        match send_result {
                            Ok(_) => {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                            }
                            Err(_) => {
                                let fallback = send_long_message(&bot_owned, chat_id, &normalized_remaining, None, &state_owned).await;
                                match fallback {
                                    Ok(_) => {
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                                    }
                                    Err(_) => {
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let truncated = truncate_str(&normalized_remaining, TELEGRAM_MSG_LIMIT);
                                        let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &truncated).await);
                                    }
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
                        msg_debug(&format!("[polling] saving session: new_session_id={:?}, old_session_id={:?}, history_len={}",
                            new_session_id, session.session_id, session.history.len()));
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
                        save_session_to_file(session, &current_path, provider_str);
                        msg_debug(&format!("[polling] session saved: session_id={:?}, history_len={}",
                            session.session_id, session.history.len()));
                    }
                    // Write to group chat shared log (for cross-bot context sharing)
                    msg_debug(&format!("[polling] JSONL check: chat_id={}, raw_entries_count={}",
                        chat_id.0, raw_entries.len()));
                    if chat_id.0 < 0 {
                        let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                        let dn = if bot_display_name_for_log.is_empty() { None } else { Some(bot_display_name_for_log.clone()) };
                        append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                            ts: now_ts.clone(),
                            bot: bot_username_for_log.clone(),
                            bot_display_name: dn.clone(),
                            role: "user".to_string(),
                            from: Some(user_display_name_owned.clone()),
                            text: user_text_owned.clone(),
                            clear: false,
                        });
                        if !raw_entries.is_empty() {
                            msg_debug(&format!("[polling] JSONL: writing user+assistant entries, raw_entries_count={}", raw_entries.len()));
                            append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                                ts: now_ts,
                                bot: bot_username_for_log.clone(),
                                bot_display_name: dn,
                                role: "assistant".to_string(),
                                from: None,
                                text: serialize_payload(&std::mem::take(&mut raw_entries)),
                                clear: false,
                            });
                        } else {
                            msg_debug(&format!("[polling] JSONL: user entry written, assistant SKIPPED (raw_entries is empty)"));
                        }
                    }
                }

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ▶ Response sent");

                // Send end hook message if configured
                if !cancelled {
                    let end_hook_msg = {
                        let data = state_owned.lock().await;
                        data.settings.end_hook.get(&chat_id.0.to_string()).cloned()
                    };
                    if let Some(hook_msg) = end_hook_msg {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("send_message", bot_owned.send_message(chat_id, &hook_msg).await);
                    }
                }

                // === /loop verification: check completeness after each turn ===
                // Provider-specific mechanics (Claude forks live session with
                // --fork-session; Codex reads the full-fidelity archive and
                // dispatches an independent --ephemeral exec). The surrounding
                // loop-state / cancel / re-inject logic is shared.
                if !cancelled && !cancel_token.cancelled.load(Ordering::Relaxed)
                    && (provider_str == "claude" || provider_str == "codex" || provider_str == "opencode") {
                    let loop_info = {
                        let data = state_owned.lock().await;
                        data.loop_states.get(&chat_id).map(|ls| {
                            let sid = data.sessions.get(&chat_id).and_then(|s| s.session_id.clone());
                            let cwd = data.sessions.get(&chat_id).and_then(|s| s.current_path.clone()).unwrap_or_default();
                            (ls.remaining, ls.max_iterations, ls.original_request.clone(), sid, cwd)
                        })
                    };
                    if let Some((remaining, max_iterations, original_request, sid, cwd)) = loop_info {
                        if let Some(session_id) = sid {
                            msg_debug(&format!("[loop] verifying ({}): session_id={}, remaining={}, request={:?}",
                                provider_str, session_id, remaining, truncate_str(&original_request, 60)));
                            // Show spinner during verification. Alternating
                            // magnifying-glass frames (🔍/🔎) typed one letter
                            // at a time give a lightweight "scanning" feel
                            // while the verifier runs. Animation runs in a
                            // background task so the main path is not blocked
                            // by frame edits; it is stopped once verify
                            // returns, before the message is deleted.
                            const VERIFY_SPINNER: &[&str] = &[
                                "🔍 V",           "🔎 Ve",          "🔍 Ver",
                                "🔎 Veri",        "🔍 Verif",       "🔎 Verify",
                                "🔍 Verifyi",     "🔎 Verifyin",    "🔍 Verifying",
                                "🔎 Verifying.",  "🔍 Verifying..", "🔎 Verifying...",
                            ];
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let verify_msg = tg!("send_message", bot_owned.send_message(chat_id, VERIFY_SPINNER[0]).await);
                            let verify_msg_id = verify_msg.ok().map(|m| m.id);

                            // Start background frame-edit task
                            let verify_anim_stop = Arc::new(AtomicBool::new(false));
                            let verify_anim_handle = if let Some(msg_id) = verify_msg_id {
                                let bot_anim = bot_owned.clone();
                                let state_anim = state_owned.clone();
                                let stop_flag = verify_anim_stop.clone();
                                Some(tokio::spawn(async move {
                                    let mut idx: usize = 1;
                                    let mut last_text = VERIFY_SPINNER[0].to_string();
                                    while !stop_flag.load(Ordering::Relaxed) {
                                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                                        if stop_flag.load(Ordering::Relaxed) { break; }
                                        let frame = VERIFY_SPINNER[idx % VERIFY_SPINNER.len()];
                                        idx += 1;
                                        if frame != last_text {
                                            shared_rate_limit_wait(&state_anim, chat_id).await;
                                            if stop_flag.load(Ordering::Relaxed) { break; }
                                            let _ = tg!("edit_message", bot_anim.edit_message_text(chat_id, msg_id, frame).await);
                                            last_text = frame.to_string();
                                        }
                                    }
                                }))
                            } else {
                                None
                            };

                            let sid_clone = session_id.clone();
                            let cwd_clone = cwd.clone();
                            let provider_for_verify = provider_str;
                            // The Codex verifier reads the full-fidelity
                            // archive, so refresh it before the verify call.
                            // (OpenCode uses native `--fork` and doesn't need
                            // the archive; Claude forks its live session and
                            // doesn't need it either.)
                            // archive_and_save_session is called directly —
                            // not via convert_and_save_session — because the
                            // latter short-circuits when its summary JSON is
                            // up-to-date, which would skip the archive
                            // refresh too. archive_and_save_session has its
                            // own independent mtime check so it stays
                            // idempotent across repeated calls.
                            if provider_for_verify == "codex" {
                                let sid_refresh = sid_clone.clone();
                                let cwd_refresh = cwd_clone.clone();
                                tokio::task::spawn_blocking(move || {
                                    match resolve_codex_by_id(&sid_refresh) {
                                        Some(info) => {
                                            msg_debug(&format!(
                                                "[loop] codex archive refresh: jsonl={}, id={}",
                                                info.jsonl_path.display(), info.session_id));
                                            crate::services::session_archive::archive_and_save_session(
                                                "codex",
                                                &info.jsonl_path,
                                                &info.session_id,
                                                &cwd_refresh,
                                            );
                                        }
                                        None => {
                                            msg_debug(&format!(
                                                "[loop] codex archive refresh SKIPPED: \
                                                 resolve_codex_by_id({}) returned None — \
                                                 verify may fail with \"Archive not found\"",
                                                sid_refresh));
                                        }
                                    }
                                }).await.ok();
                            }
                            let verify_result = tokio::task::spawn_blocking(move || {
                                match provider_for_verify {
                                    "codex" => crate::services::codex::verify_completion_codex(&sid_clone, &cwd_clone),
                                    "opencode" => crate::services::opencode::verify_completion_opencode(&sid_clone, &cwd_clone),
                                    _ => crate::services::claude::verify_completion(&sid_clone, &cwd_clone),
                                }
                            }).await;
                            // Stop animation and await its task so no stray
                            // edit lands after the delete below.
                            verify_anim_stop.store(true, Ordering::Relaxed);
                            if let Some(h) = verify_anim_handle { let _ = h.await; }
                            // Remove spinner
                            if let Some(msg_id) = verify_msg_id {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
                            }
                            // Re-check: /stop may have been called during verification
                            if cancel_token.cancelled.load(Ordering::Relaxed) {
                                msg_debug("[loop] cancelled during verification — aborting loop");
                                // loop_states already removed by /stop handler
                            } else {
                            match verify_result {
                                Ok(Ok(result)) => {
                                    if result.complete {
                                        msg_debug("[loop] mission_complete — loop finished");
                                        {
                                            let mut data = state_owned.lock().await;
                                            data.loop_states.remove(&chat_id);
                                        }
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let _ = tg!("send_message", bot_owned.send_message(chat_id, "✅ Loop complete — task verified as done.").await);
                                    } else if max_iterations > 0 && remaining <= 1 {
                                        msg_debug("[loop] max iterations reached — loop stopped");
                                        {
                                            let mut data = state_owned.lock().await;
                                            data.loop_states.remove(&chat_id);
                                        }
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let feedback_preview = result.feedback.as_deref().unwrap_or("(no details)");
                                        let msg = format!("⚠️ Loop limit reached. Remaining issue:\n{}", feedback_preview);
                                        let _ = tg!("send_message", bot_owned.send_message(chat_id, &msg).await);
                                    } else {
                                        // Incomplete: use feedback if available, otherwise fall back to original request
                                        let reinject_text = result.feedback.unwrap_or_else(|| {
                                            format!("Continue the previous task. The original request was: {}", original_request)
                                        });
                                        // For unlimited (max_iterations==0): don't decrement, track iteration count separately
                                        let (new_remaining, iteration) = if max_iterations == 0 {
                                            // remaining counts UP from 0 for display; never decremented to trigger limit
                                            (remaining.saturating_add(1), remaining.saturating_add(1))
                                        } else {
                                            let nr = remaining - 1;
                                            (nr, max_iterations - nr)
                                        };
                                        msg_debug(&format!("[loop] incomplete — re-requesting (remaining={}): {:?}",
                                            new_remaining, truncate_str(&reinject_text, 100)));
                                        let iter_label = if max_iterations == 0 {
                                            format!("🔄 Loop iteration {}\n{}", iteration, reinject_text)
                                        } else {
                                            format!("🔄 Loop iteration {}/{}\n{}", iteration, max_iterations, reinject_text)
                                        };
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let _ = tg!("send_message", bot_owned.send_message(chat_id, &iter_label).await);
                                        // Store feedback for dispatch_loop_feedback to pick up.
                                        // Cannot call handle_text_message directly here because
                                        // its future is not Send-safe inside tokio::spawn.
                                        // Re-check cancellation under the lock so /stop arriving
                                        // between the verify-result check and this insert cannot
                                        // leave behind a feedback that re-injects after /stop.
                                        {
                                            let mut data = state_owned.lock().await;
                                            if cancel_token.cancelled.load(Ordering::Relaxed) {
                                                msg_debug("[loop] cancelled during result processing — skip reinject");
                                            } else {
                                                if let Some(ls) = data.loop_states.get_mut(&chat_id) {
                                                    ls.remaining = new_remaining;
                                                }
                                                data.loop_feedback.insert(chat_id, (reinject_text, user_display_name_owned.clone()));
                                                loop_reinjected = true;
                                            }
                                        }
                                    }
                                }
                                Ok(Err(e)) => {
                                    msg_debug(&format!("[loop] verify_completion error: {}", e));
                                    {
                                        let mut data = state_owned.lock().await;
                                        data.loop_states.remove(&chat_id);
                                    }
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!("send_message", bot_owned.send_message(chat_id,
                                        &format!("⚠️ Loop verification failed: {}", e)).await);
                                }
                                Err(e) => {
                                    msg_debug(&format!("[loop] spawn_blocking error: {}", e));
                                    {
                                        let mut data = state_owned.lock().await;
                                        data.loop_states.remove(&chat_id);
                                    }
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!("send_message", bot_owned.send_message(chat_id,
                                        &format!("⚠️ Loop verification failed: {}", e)).await);
                                }
                            }
                            } // else (not cancelled during verification)
                        } else {
                            msg_debug("[loop] no session_id available — cannot verify");
                            {
                                let mut data = state_owned.lock().await;
                                data.loop_states.remove(&chat_id);
                            }
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!("send_message", bot_owned.send_message(chat_id, "⚠️ Loop stopped — no session ID available for verification.").await);
                        }
                    }
                }
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
                    #[cfg(windows)]
                    { let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output(); }
                }
            }

            // stopped_response (full) is used for session history
            let stopped_response = if full_response.trim().is_empty() {
                "[Stopped]".to_string()
            } else {
                let normalized = normalize_empty_lines(&full_response);
                format!("{}\n\n[Stopped]", normalized)
            };

            shared_rate_limit_wait(&state_owned, chat_id).await;

            // ── Show only remaining delta + [Stopped] (unified rolling placeholder) ──
            if full_response.len() < last_confirmed_len || !full_response.is_char_boundary(last_confirmed_len) { last_confirmed_len = 0; }
            let remaining = &full_response[last_confirmed_len..];
            msg_debug(&format!("[rolling_ph] STOPPED: placeholder_msg_id={}, confirmed={}, remaining_len={}",
                placeholder_msg_id, last_confirmed_len, remaining.trim().len()));
            if should_attach_response_as_file(full_response.len(), provider_str) {
                // Large stopped response — send as file
                msg_debug(&format!("[rolling_ph] STOPPED FILE ATTACH: total={}", full_response.len()));
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, "\u{1f4c4} Response attached as file [Stopped]").await);
                send_response_as_file(&bot_owned, chat_id, &stopped_response, &state_owned, "response").await;
            } else {
                let display_stopped = if remaining.trim().is_empty() {
                    "[Stopped]".to_string()
                } else {
                    let normalized = normalize_empty_lines(remaining);
                    format!("{}\n\n[Stopped]", normalized)
                };
                let html_stopped = markdown_to_telegram_html(&display_stopped);
                if html_stopped.len() <= TELEGRAM_MSG_LIMIT {
                    if let Err(e) = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_stopped)
                        .parse_mode(ParseMode::Html).await)
                    {
                        let ts_err = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts_err}]   ⚠ edit_message failed (stopped HTML): {e}");
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &display_stopped).await);
                    }
                } else {
                    let send_result = send_long_message(&bot_owned, chat_id, &html_stopped, Some(ParseMode::Html), &state_owned).await;
                    match send_result {
                        Ok(_) => {
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                        }
                        Err(_) => {
                            let fallback = send_long_message(&bot_owned, chat_id, &display_stopped, None, &state_owned).await;
                            match fallback {
                                Ok(_) => {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                                }
                                Err(_) => {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let truncated = truncate_str(&display_stopped, TELEGRAM_MSG_LIMIT);
                                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &truncated).await);
                                }
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
                if let Some(sid) = new_session_id {
                    session.session_id = Some(sid);
                }
                session.history.push(HistoryItem {
                    item_type: HistoryType::User,
                    content: user_text_owned.clone(),
                });
                session.history.push(HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: stopped_response,
                });
                save_session_to_file(session, &current_path, provider_str);
                // Write to group chat shared log (for cross-bot context sharing)
                msg_debug(&format!("[polling] JSONL stopped check: chat_id={}, raw_entries_count={}",
                    chat_id.0, raw_entries.len()));
                if chat_id.0 < 0 {
                    let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                    let dn = if bot_display_name_for_log.is_empty() { None } else { Some(bot_display_name_for_log.clone()) };
                    append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                        ts: now_ts.clone(),
                        bot: bot_username_for_log.clone(),
                        bot_display_name: dn.clone(),
                        role: "user".to_string(),
                        from: Some(user_display_name_owned.clone()),
                        text: user_text_owned,
                        clear: false,
                    });
                    if !raw_entries.is_empty() {
                        msg_debug(&format!("[polling] JSONL stopped: writing user+assistant entries, raw_entries_count={}", raw_entries.len()));
                        append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                            ts: now_ts,
                            bot: bot_username_for_log.clone(),
                            bot_display_name: dn,
                            role: "assistant".to_string(),
                            from: None,
                            text: serialize_payload(&std::mem::take(&mut raw_entries)),
                            clear: false,
                        });
                    } else {
                        msg_debug(&format!("[polling] JSONL stopped: user entry written, assistant SKIPPED (raw_entries is empty)"));
                    }
                }
            }
            data.cancel_tokens.remove(&chat_id);
            data.stop_message_ids.remove(&chat_id);
            drop(data);
            msg_debug(&format!("[queue:trigger] chat_id={}, source=text_poll_cancelled", chat_id.0));
            drop(_group_lock); // release group chat lock before processing queue
            process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
            return;
        }

        // If loop verification stored feedback, clean up and dispatch it.
        // Uses dispatch_loop_feedback (bypasses queue mode check, no "Dequeued" noise).
        // Backstop: if /stop fired after the in-lock check above, drop the dispatch.
        if loop_reinjected && !cancel_token.cancelled.load(Ordering::Relaxed) {
            msg_debug("[queue:trigger] loop_reinjected — dispatching loop feedback");
            let orphan_stop_msg = {
                let mut data = state_owned.lock().await;
                data.cancel_tokens.remove(&chat_id);
                data.stop_message_ids.remove(&chat_id)
            };
            if let Some(msg_id) = orphan_stop_msg {
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
            }
            drop(_group_lock);
            dispatch_loop_feedback(&bot_owned, chat_id, &state_owned).await;
            return;
        }

        // Atomically remove both cancel_tokens and stop_message_ids to prevent
        // race with /stop handler inserting a stop_msg_id between two separate locks
        let orphan_stop_msg = {
            let mut data = state_owned.lock().await;
            let msg_id = data.stop_message_ids.remove(&chat_id);
            data.cancel_tokens.remove(&chat_id);
            msg_id
        };
        if let Some(msg_id) = orphan_stop_msg {
            shared_rate_limit_wait(&state_owned, chat_id).await;
            let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
        }
        msg_debug(&format!("[queue:trigger] chat_id={}, source=text_poll_completed", chat_id.0));
        drop(_group_lock); // release group chat lock before processing queue
        process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
    });

    Ok(())
}

/// Load existing session from ai_sessions directory matching the given path and provider
fn load_existing_session(current_path: &str, provider: &str) -> Option<(SessionData, std::time::SystemTime)> {
    msg_debug(&format!("[load_session] looking for path={:?}, provider={}", current_path, provider));
    let sessions_dir = ai_screen::ai_sessions_dir()?;

    if !sessions_dir.exists() {
        msg_debug(&format!("[load_session] sessions_dir not found: {}", sessions_dir.display()));
        return None;
    }

    let mut with_session_id: Option<(SessionData, std::time::SystemTime)> = None;
    let mut without_session_id: Option<(SessionData, std::time::SystemTime)> = None;
    let mut file_count = 0u32;
    let mut path_mismatch_sample: Option<String> = None;

    if let Ok(entries) = fs::read_dir(&sessions_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                file_count += 1;
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(session_data) = serde_json::from_str::<SessionData>(&content) {
                        if session_data.current_path == current_path {
                            // Provider filter: match exact provider, or allow empty (legacy files)
                            if !session_data.provider.is_empty() && session_data.provider != provider {
                                msg_debug(&format!("[load_session] skipped session_id={} (provider mismatch: {} != {})",
                                    session_data.session_id, session_data.provider, provider));
                                continue;
                            }
                            msg_debug(&format!("[load_session] found session_id={}, provider={}, history_len={}, stored_path={:?}",
                                session_data.session_id, session_data.provider, session_data.history.len(), session_data.current_path));
                            if let Ok(metadata) = path.metadata() {
                                if let Ok(modified) = metadata.modified() {
                                    let target = if !session_data.session_id.is_empty() {
                                        &mut with_session_id
                                    } else {
                                        &mut without_session_id
                                    };
                                    match target {
                                        None => *target = Some((session_data, modified)),
                                        Some((_, latest_time)) if modified > *latest_time => {
                                            *target = Some((session_data, modified));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        } else if path_mismatch_sample.is_none() {
                            path_mismatch_sample = Some(session_data.current_path.clone());
                        }
                    }
                }
            }
        }
    }

    let matching_session = with_session_id.or(without_session_id);

    msg_debug(&format!("[load_session] scanned {} json files, result={}", file_count,
        if matching_session.is_some() { "found" } else { "None" }));
    if matching_session.is_none() && file_count > 0 {
        if let Some(sample) = path_mismatch_sample {
            msg_debug(&format!("[load_session] path mismatch example: stored={:?} vs wanted={:?}", sample, current_path));
        }
    }

    matching_session
}

/// Remove stale session files without session_id for the same current_path + provider.
/// Called when no file with session_id exists for this path+provider.
/// Keeps the most recently modified empty file (the one selected by load_existing_session)
/// and deletes the rest.
fn cleanup_session_files(current_path: &str, provider: &str) {
    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else { return; };
    let Ok(entries) = fs::read_dir(&sessions_dir) else { return; };

    // Collect matching empty files with their modification time
    let mut empty_files: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(data) = serde_json::from_str::<SessionData>(&content) {
                    if data.current_path == current_path
                        && (data.provider == provider || data.provider.is_empty())
                        && data.session_id.is_empty()
                    {
                        let modified = path.metadata().ok()
                            .and_then(|m| m.modified().ok())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        empty_files.push((path, modified));
                    }
                }
            }
        }
    }

    if empty_files.len() <= 1 { return; }

    // Find the latest modified one to keep
    let latest_idx = empty_files.iter().enumerate()
        .max_by_key(|(_, (_, t))| *t)
        .map(|(i, _)| i)
        .unwrap();

    // Delete all except the latest
    for (i, (path, _)) in empty_files.iter().enumerate() {
        if i != latest_idx {
            let _ = fs::remove_file(path);
        }
    }
}

/// Save session to file in the ai_sessions directory
fn save_session_to_file(session: &ChatSession, current_path: &str, provider: &str) {
    let Some(ref session_id) = session.session_id else {
        msg_debug("[save_session] skipped: no session_id");
        return;
    };

    if session.history.is_empty() {
        msg_debug("[save_session] skipped: empty history");
        return;
    }

    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        msg_debug("[save_session] skipped: ai_sessions_dir() returned None");
        return;
    };

    if fs::create_dir_all(&sessions_dir).is_err() {
        msg_debug("[save_session] skipped: create_dir_all failed");
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
        provider: provider.to_string(),
    };
    msg_debug(&format!("[save_session] provider={}, session_id={}, path={}", provider, session_id, current_path));

    // Security: whitelist session_id to alphanumeric, hyphens, underscores only
    if !session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return;
    }

    let file_path = sessions_dir.join(format!("{}.json", session_id));

    if let Ok(json) = serde_json::to_string_pretty(&session_data) {
        let _ = fs::write(&file_path, json);
    }

    // Clean up old session files for the same path+provider (removes orphaned files
    // left by Codex's per-call thread_id rotation and /clear blocker files)
    if let Ok(entries) = fs::read_dir(&sessions_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p == file_path { continue; }
            if p.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(content) = fs::read_to_string(&p) {
                    if let Ok(old) = serde_json::from_str::<SessionData>(&content) {
                        if old.current_path == current_path
                            && (old.provider == provider || old.provider.is_empty())
                        {
                            let _ = fs::remove_file(&p);
                        }
                    }
                }
            }
        }
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

/// Send a large AI response as a .txt file attachment.
/// Returns true if the file was successfully sent.
async fn send_response_as_file(
    bot: &Bot,
    chat_id: ChatId,
    response: &str,
    state: &SharedState,
    label: &str,
) -> bool {
    let Some(home) = dirs::home_dir() else { return false };
    let tmp_dir = home.join(".cokacdir").join("tmp");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let tmp_path = tmp_dir.join(format!("cokacdir_{}_{}.txt", label, timestamp));
    if std::fs::write(&tmp_path, response).is_err() {
        return false;
    }
    shared_rate_limit_wait(state, chat_id).await;
    let result = tg!("send_document", bot.send_document(
        chat_id,
        teloxide::types::InputFile::file(&tmp_path),
    ).await);
    let _ = std::fs::remove_file(&tmp_path);
    result.is_ok()
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

/// Check if a Bash tool call is an internal cokacdir command.
/// Scans whitespace-delimited tokens for one whose basename matches the cokacdir binary.
/// Handles quoted paths, shell wrappers (bash -lc "..."), chained commands (cd && ...), etc.
/// NOTE: Returns bool (not subcommand name), so console logs show "cokacdir: ..." without
/// the specific --flag. format_cokacdir_result() auto-detects subcommand from JSON fields instead.
fn detect_cokacdir_command(name: &str, input: &str) -> bool {
    if name != "Bash" { return false; }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else { return false };
    let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let expected = crate::bin_path().rsplit(['/', '\\']).next().unwrap_or("");
    if expected.is_empty() { return false; }
    // Strip surrounding quotes from each token, then compare basename
    cmd.split_whitespace().any(|tok| {
        let unquoted = tok.trim_matches(|c| c == '"' || c == '\'');
        let basename = unquoted.rsplit(['/', '\\']).next().unwrap_or("");
        basename == expected
    })
}

/// Check if a Bash tool call contains --read_chat_log (result should be suppressed from display).
fn detect_chat_log_read(name: &str, input: &str) -> bool {
    if name != "Bash" { return false; }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else { return false };
    let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
    cmd.contains("--read_chat_log")
}

/// Read the most recent .result file from schedule dir and delete it
fn read_latest_cron_result() -> Option<String> {
    let dir = schedule_dir()?;
    let mut results: Vec<_> = fs::read_dir(&dir).ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "result").unwrap_or(false))
        .collect();
    results.sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH)));
    let entry = results.first()?;
    let content = fs::read_to_string(entry.path()).ok()?;
    let _ = fs::remove_file(entry.path());
    Some(content)
}

/// Format a cokacdir command's JSON result into a human-readable message.
/// Auto-detects the subcommand from JSON result fields.
/// NOTE: Empty content triggers .result file read (for --cron async results).
/// Currently only --cron produces empty output, so this is safe.
/// If a new subcommand also returns empty output in the future,
/// it would incorrectly read a stale cron .result file.
fn format_cokacdir_result(content: &str) -> String {
    // Try to parse as JSON; if empty, try reading from .result file (for --cron)
    let effective_content = if content.trim().is_empty() {
        read_latest_cron_result().unwrap_or_default()
    } else {
        content.to_string()
    };
    let v: serde_json::Value = match serde_json::from_str(effective_content.trim()) {
        Ok(v) => v,
        Err(_) => {
            // Fallback: some backends (e.g. Gemini CLI) wrap the JSON output with
            // extra text like "Output: {...}\nProcess Group PGID: ...".
            // Try to extract the JSON object by trimming to first '{' and last '}'.
            let trimmed = effective_content.trim();
            let extracted = match (trimmed.find('{'), trimmed.rfind('}')) {
                (Some(start), Some(end)) if start < end => &trimmed[start..=end],
                _ => return String::new(),
            };
            match serde_json::from_str(extracted) {
                Ok(v) => v,
                Err(_) => return String::new(),
            }
        }
    };

    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");

    if status == "error" {
        let msg = v.get("message").and_then(|s| s.as_str()).unwrap_or("unknown error");
        return format!("Error: {}", msg);
    }

    // Auto-detect subcommand from result JSON fields
    if v.get("time").is_some() {
        // --currenttime → {"status":"ok","time":"..."}
        let time = v["time"].as_str().unwrap_or("?");
        format!("🕐 {}", time)
    } else if v.get("schedules").is_some() {
        // --cron-list → {"status":"ok","schedules":[...]}
        let schedules = v["schedules"].as_array();
        match schedules {
            Some(arr) if arr.is_empty() => "📋 No schedules found.".to_string(),
            Some(arr) => {
                let mut lines = vec![format!("📋 {} schedule(s)", arr.len())];
                for (i, s) in arr.iter().enumerate() {
                    let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                    let schedule = s.get("schedule").and_then(|v| v.as_str()).unwrap_or("");
                    let prompt = s.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                    let schedule_type = s.get("schedule_type").and_then(|v| v.as_str()).unwrap_or("");
                    let once = s.get("once").and_then(|b| b.as_bool()).unwrap_or(false);
                    let kind = match schedule_type {
                        "absolute" => "1회",
                        "cron" if once => "1회 cron",
                        "cron" => "반복",
                        _ => if schedule.split_whitespace().count() == 5 { "반복" } else { "1회" },
                    };
                    let prompt_preview = if prompt.chars().count() > 40 {
                        format!("{}...", prompt.chars().take(40).collect::<String>())
                    } else {
                        prompt.to_string()
                    };
                    lines.push(format!("\n{}. [{}] {}\n   🕐 `{}`\n   🔖 {}", i + 1, kind, prompt_preview, schedule, id));
                }
                lines.join("\n")
            }
            None => content.to_string(),
        }
    } else if v.get("path").is_some() {
        // --sendfile → {"status":"ok","path":"..."}
        let path = v["path"].as_str().unwrap_or("?");
        format!("📎 {}", path)
    } else if v.get("prompt").is_some() {
        // --cron (register) → {"status":"ok","id":"...","prompt":"...","schedule":"..."}
        let id = v.get("id").and_then(|s| s.as_str()).unwrap_or("?");
        let prompt = v["prompt"].as_str().unwrap_or("");
        let schedule = v.get("schedule").and_then(|s| s.as_str()).unwrap_or("");
        let schedule_type = v.get("schedule_type").and_then(|s| s.as_str()).unwrap_or("");
        let once = v.get("once").and_then(|b| b.as_bool()).unwrap_or(false);
        let kind = match schedule_type {
            "absolute" => "1회",
            "cron" if once => "1회 cron",
            "cron" => "반복",
            _ => if schedule.split_whitespace().count() == 5 { "반복" } else { "1회" },
        };
        format!("✅ Scheduled [{}]\n🔖 {}\n📝 {}\n🕐 `{}`", kind, id, prompt, schedule)
    } else if v.get("schedule").is_some() {
        // --cron-update → {"status":"ok","id":"...","schedule":"..."}
        let id = v.get("id").and_then(|s| s.as_str()).unwrap_or("?");
        let schedule = v["schedule"].as_str().unwrap_or("");
        format!("✅ Updated\n🕐 `{}`\n🔖 {}", schedule, id)
    } else if v.get("id").is_some() {
        let id = v["id"].as_str().unwrap_or("?");
        if id.starts_with("msg_") {
            // --message result: not useful to show to user
            String::new()
        } else {
            // --cron-remove → {"status":"ok","id":"..."}
            format!("✅ Removed\n🔖 {}", id)
        }
    } else {
        content.to_string()
    }
}

/// Extract the command string (with optional description) from a Bash tool input JSON
fn format_bash_command(input: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        return input.to_string();
    };
    let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
    let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if !desc.is_empty() {
        format!("{}\n{}", desc, cmd)
    } else {
        cmd.to_string()
    }
}

/// Format tool input JSON into a human-readable summary
fn format_tool_input(name: &str, input: &str) -> String {
    // FileChange input is the full Codex item JSON — extract path summary for display
    if name == "FileChange" {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(input) {
            if let Some(changes) = v.get("changes").and_then(|v| v.as_array()) {
                let summary: Vec<String> = changes.iter().map(|c| {
                    let path = c.get("path").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("update");
                    format!("{}: {}", kind, path)
                }).collect();
                return format!("\u{1F4DD} {}", summary.join(", "));
            }
        }
        return format!("\u{1F4DD} {}", input);
    }

    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        // Non-JSON input — Codex tools produce human-readable strings directly
        return match name {
            "WebSearch" => {
                if input.is_empty() { "Search".to_string() }
                else { format!("Search: {}", input) }
            }
            n if n.starts_with("Collab:") => {
                let tool = n.strip_prefix("Collab:").unwrap_or(n);
                if input.is_empty() { format!("Agent: {}", tool) }
                else { format!("Agent {}: {}", tool, input) }
            }
            _ => format!("{} {}", name, input),
        };
    };

    match name {
        "Bash" => {
            let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if !desc.is_empty() {
                format!("{}: `{}`", desc, cmd)
            } else {
                format!("`{}`", cmd)
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
            if !fp.is_empty() {
                let replace_all = v.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
                if replace_all {
                    format!("Edit {} (replace all)", fp)
                } else {
                    format!("Edit {}", fp)
                }
            } else {
                "Edit".to_string()
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
                    question.to_string()
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
            // Codex Collab:* tools — input is prompt text (or empty)
            if name.starts_with("Collab:") {
                let collab_tool = name.strip_prefix("Collab:").unwrap_or(name);
                if input.is_empty() {
                    format!("Agent: {}", collab_tool)
                } else {
                    format!("Agent {}: {}", collab_tool, input)
                }
            } else {
                format!("{} {}", name, input)
            }
        }
    }
}

// === Scheduler ===

/// Check if a schedule entry should trigger now
fn should_trigger(entry: &ScheduleEntry) -> bool {
    let now = chrono::Local::now();
    sched_debug(&format!("[should_trigger] id={}, type={}, schedule={}, now={}, last_run={:?}",
        entry.id, entry.schedule_type, entry.schedule, now.format("%Y-%m-%d %H:%M:%S"), entry.last_run));
    match entry.schedule_type.as_str() {
        "absolute" => {
            let Ok(schedule_time) = chrono::NaiveDateTime::parse_from_str(&entry.schedule, "%Y-%m-%d %H:%M:%S") else {
                sched_debug(&format!("[should_trigger] id={}, parse failed → false", entry.id));
                return false;
            };
            let schedule_dt = schedule_time.and_local_timezone(chrono::Local).single();
            let Some(schedule_dt) = schedule_dt else {
                sched_debug(&format!("[should_trigger] id={}, timezone conversion failed → false", entry.id));
                return false;
            };
            if now < schedule_dt {
                sched_debug(&format!("[should_trigger] id={}, not yet (now < schedule_dt) → false", entry.id));
                return false;
            }
            // Already ran?
            if let Some(ref last) = entry.last_run {
                if let Ok(last_dt) = chrono::NaiveDateTime::parse_from_str(last, "%Y-%m-%d %H:%M:%S") {
                    if let Some(last_local) = last_dt.and_local_timezone(chrono::Local).single() {
                        if last_local >= schedule_dt {
                            sched_debug(&format!("[should_trigger] id={}, already ran (last={} >= sched={}) → false",
                                entry.id, last_local.format("%H:%M:%S"), schedule_dt.format("%H:%M:%S")));
                            return false;
                        }
                    }
                }
            }
            sched_debug(&format!("[should_trigger] id={}, absolute ready → true", entry.id));
            true
        }
        "cron" => {
            if !cron_matches(&entry.schedule, now) {
                sched_debug(&format!("[should_trigger] id={}, cron not matching → false", entry.id));
                return false;
            }
            // Check last_run to avoid duplicate triggers within the same minute
            if let Some(ref last) = entry.last_run {
                if let Ok(last_dt) = chrono::NaiveDateTime::parse_from_str(last, "%Y-%m-%d %H:%M:%S") {
                    if let Some(last_local) = last_dt.and_local_timezone(chrono::Local).single() {
                        let now_min = now.format("%Y-%m-%d %H:%M").to_string();
                        let last_min = last_local.format("%Y-%m-%d %H:%M").to_string();
                        if now_min == last_min {
                            sched_debug(&format!("[should_trigger] id={}, already ran this minute ({}) → false", entry.id, now_min));
                            return false;
                        }
                    }
                }
            }
            sched_debug(&format!("[should_trigger] id={}, cron matched → true", entry.id));
            true
        }
        _ => {
            sched_debug(&format!("[should_trigger] id={}, unknown type={} → false", entry.id, entry.schedule_type));
            false
        }
    }
}

/// Update schedule entry after a run: set last_run, delete if once
fn update_schedule_after_run(entry: &ScheduleEntry, new_context_summary: Option<String>) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    sched_debug(&format!("[update_schedule_after_run] id={}, type={}, once={:?}, now={}, has_new_context={}",
        entry.id, entry.schedule_type, entry.once, now, new_context_summary.is_some()));

    // 실행 중 사용자가 삭제한 경우 부활 방지
    let dir = match schedule_dir() {
        Some(d) => d,
        None => {
            sched_debug(&format!("[update_schedule_after_run] id={}, no schedule dir → skip", entry.id));
            return;
        }
    };
    let path = dir.join(format!("{}.json", entry.id));
    if !path.exists() {
        sched_debug(&format!("[update_schedule_after_run] id={}, file already deleted → skip (no resurrection)", entry.id));
        return; // 이미 삭제됨 - write하지 않음
    }

    // One-time schedules (absolute / cron --once) are already deleted before execution,
    // so this function only handles recurring cron updates.
    sched_debug(&format!("[update_schedule_after_run] id={}, cron recurring → update last_run", entry.id));
    let mut updated = entry.clone();
    updated.last_run = Some(now);
    if new_context_summary.is_some() {
        updated.context_summary = new_context_summary;
    }
    if let Err(e) = write_schedule_entry(&updated) {
        sched_debug(&format!("[update_schedule_after_run] id={}, write failed: {}", entry.id, e));
        eprintln!("[Schedule] Failed to update entry {}: {}", entry.id, e);
    } else {
        sched_debug(&format!("[update_schedule_after_run] id={}, updated successfully", entry.id));
    }
}

/// Execute a scheduled task — similar pattern to handle_text_message
async fn execute_schedule(
    bot: &Bot,
    chat_id: ChatId,
    entry: &ScheduleEntry,
    state: &SharedState,
    token: &str,
    prev_session: Option<ChatSession>,
) {
    sched_debug(&format!("[execute_schedule] START id={}, chat_id={}, prompt={:?}, has_context={}, has_prev_session={}",
        entry.id, chat_id, truncate_str(&entry.prompt, 60), entry.context_summary.is_some(), prev_session.is_some()));

    // Acquire group chat lock (serializes processing across bots in the same group chat)
    let group_lock = acquire_group_chat_lock(chat_id.0).await;

    // Check if cancelled during lock wait
    let cancelled_during_wait = {
        let data = state.lock().await;
        data.cancel_tokens.get(&chat_id)
            .map(|ct| ct.cancelled.load(Ordering::Relaxed))
            .unwrap_or(false)
    };
    if cancelled_during_wait {
        sched_debug(&format!("[execute_schedule] cancelled during lock wait, id={}", entry.id));
        {
            let mut data = state.lock().await;
            if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                set.remove(&entry.id);
            }
            data.cancel_tokens.remove(&chat_id);
            if let Some(prev) = prev_session {
                data.sessions.insert(chat_id, prev);
            } else {
                data.sessions.remove(&chat_id);
            }
        }
        msg_debug(&format!("[queue:trigger] chat_id={}, source=schedule_cancelled_during_lock", chat_id.0));
        drop(group_lock);
        process_next_queued_message(bot, chat_id, state).await;
        return;
    }

    // Build prompt with context summary if available
    let user_prompt = entry.prompt.clone();
    let prompt = if let Some(ref summary) = entry.context_summary {
        sched_debug(&format!("[execute_schedule] id={}, injecting context summary ({} chars)", entry.id, summary.len()));
        format!(
            "[이전 작업 맥락]\n{}\n\n[작업 지시]\n{}",
            summary, user_prompt
        )
    } else {
        user_prompt.clone()
    };
    let project_path = crate::utils::format::to_shell_path(&entry.current_path);
    let schedule_id = entry.id.clone();

    // Delete schedule files before execution for one-time schedules (absolute / cron --once)
    if entry.once.unwrap_or(false) || entry.schedule_type == "absolute" {
        sched_debug(&format!("[execute_schedule] id={}, one-time → deleting schedule files before execution", schedule_id));
        delete_schedule_entry(&schedule_id);
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ⏰ Schedule Starting: {user_prompt}");

    // Create persistent workspace directory for this schedule execution
    let Some(home) = dirs::home_dir() else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] ⚠ [Schedule] Failed to get home directory");
        {
            let mut data = state.lock().await;
            if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                set.remove(&schedule_id);
            }
            data.cancel_tokens.remove(&chat_id);
            if let Some(prev) = prev_session {
                data.sessions.insert(chat_id, prev);
            } else {
                data.sessions.remove(&chat_id);
            }
        }
        msg_debug(&format!("[queue:trigger] chat_id={}, source=schedule_no_home", chat_id.0));
        drop(group_lock); // release before queue processing to avoid deadlock
        process_next_queued_message(bot, chat_id, state).await;
        return;
    };
    let workspace_dir = home.join(".cokacdir").join("workspace").join(&schedule_id);
    sched_debug(&format!("[execute_schedule] id={}, creating workspace: {}", schedule_id, workspace_dir.display()));
    if let Err(e) = fs::create_dir_all(&workspace_dir) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] ⚠ [Schedule] Failed to create workspace: {e}");
        sched_debug(&format!("[execute_schedule] id={}, workspace creation failed: {}, restoring session", schedule_id, e));
        {
            let mut data = state.lock().await;
            if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                set.remove(&schedule_id);
            }
            data.cancel_tokens.remove(&chat_id);
            if let Some(prev) = prev_session {
                data.sessions.insert(chat_id, prev);
            } else {
                data.sessions.remove(&chat_id);
            }
        }
        msg_debug(&format!("[queue:trigger] chat_id={}, source=schedule_workspace_error", chat_id.0));
        drop(group_lock); // release before queue processing to avoid deadlock
        process_next_queued_message(bot, chat_id, state).await;
        return;
    }
    let workspace_path = workspace_dir.display().to_string();

    // Get allowed tools and model for this chat
    let (allowed_tools, model, sched_chrome_enabled) = {
        let data = state.lock().await;
        let chrome = data.settings.use_chrome.get(&chat_id.0.to_string()).copied().unwrap_or(false);
        (get_allowed_tools(&data.settings, chat_id), get_model(&data.settings, chat_id), chrome)
    };

    // Send placeholder (show only the user's original prompt, not the context summary)
    shared_rate_limit_wait(state, chat_id).await;
    let placeholder = match tg!("send_message", bot.send_message(chat_id, format!("⏰ {user_prompt}")).await) {
        Ok(msg) => msg,
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ⚠ [Schedule] Failed to send placeholder: {e}");
            // Clean up pending + cancel_token, restore session (workspace preserved)
            {
                let mut data = state.lock().await;
                if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                    set.remove(&schedule_id);
                }
                data.cancel_tokens.remove(&chat_id);
                if let Some(prev) = prev_session {
                    data.sessions.insert(chat_id, prev);
                } else {
                    data.sessions.remove(&chat_id);
                }
            }
            msg_debug(&format!("[queue:trigger] chat_id={}, source=schedule_placeholder_error", chat_id.0));
            drop(group_lock); // release before queue processing to avoid deadlock
            process_next_queued_message(bot, chat_id, state).await;
            return;
        }
    };
    let placeholder_msg_id = placeholder.id;

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

    let bot_key = token_hash(token);
    let (sched_instruction, sched_context_count, sched_bot_username, sched_bot_display_name) = {
        let data = state.lock().await;
        let ctx = data.settings.context.get(&chat_id.0.to_string()).copied().unwrap_or(12);
        (data.settings.instructions.get(&chat_id.0.to_string()).cloned(), ctx, data.bot_username.clone(), data.bot_display_name.clone())
    };
    let platform = capitalize_platform(detect_platform(token));
    let sched_role = {
        let base = format!(
            "You are executing a scheduled task through {}.\n\
             Project directory: {project_path}\n\
             Your current working directory is a dedicated workspace for this schedule.\n\
             This workspace will be preserved after execution. The user can continue work here via /start.\n\
             To work with project files, use absolute paths to the project directory.\n\
             Any files you want to deliver must be sent via the \"{}\" --sendfile command before the task ends.",
            platform, shell_bin_path()
        );
        match &sched_instruction {
            Some(instr) => format!("{}\n\nUser's instruction for this chat:\n{}", base, instr),
            None => base,
        }
    };
    let system_prompt_owned = build_system_prompt(
        &sched_role,
        &crate::utils::format::to_shell_path(&workspace_path), chat_id.0, &bot_key, &disabled_notice,
        None, // scheduled tasks don't need to register further schedules with session context
        &sched_bot_username, &sched_bot_display_name,
        None, // scheduled tasks: no user message dedup
        sched_context_count, &platform,
    );

    // Retrieve pre-inserted cancel token (from scheduler_loop), or create a new one
    let cancel_token = {
        let mut data = state.lock().await;
        if let Some(existing) = data.cancel_tokens.get(&chat_id) {
            existing.clone()
        } else {
            let token = Arc::new(CancelToken::new());
            data.cancel_tokens.insert(chat_id, token.clone());
            token
        }
    };

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();
    let cancel_token_clone = cancel_token.clone();
    let model_for_summary = model.clone();

    // Run AI backend in a blocking thread (always new session — context is in the prompt)
    // Session persistence must be kept so users can resume via /SCHEDULE_ID
    let workspace_path_for_claude = workspace_path.clone();
    let model_clone_for_exec = model.clone();
    tokio::task::spawn_blocking(move || {
        let provider = detect_provider(model_clone_for_exec.as_deref());
        sched_debug(&format!("[execute_schedule:spawn_blocking] provider={}, model={:?}",
            provider, model_clone_for_exec));
        let result = if provider == "opencode" {
            let opencode_model = model_clone_for_exec.as_deref().and_then(opencode::strip_opencode_prefix);
            sched_debug(&format!("[execute_schedule] → opencode::execute, opencode_model={:?}, session_id=None, workspace={}, prompt_len={}, system_prompt_len={}",
                opencode_model, workspace_path_for_claude, prompt.len(), system_prompt_owned.len()));
            opencode::execute_command_streaming(
                &prompt,
                None,
                &workspace_path_for_claude,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                opencode_model,
                false,
            )
        } else if provider == "gemini" {
            let gemini_model = model_clone_for_exec.as_deref().and_then(gemini::strip_gemini_prefix);
            sched_debug(&format!("[execute_schedule] → gemini::execute, gemini_model={:?}, session_id=None, workspace={}, prompt_len={}, system_prompt_len={}",
                gemini_model, workspace_path_for_claude, prompt.len(), system_prompt_owned.len()));
            gemini::execute_command_streaming(
                &prompt,
                None,
                &workspace_path_for_claude,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                gemini_model,
                false,
            )
        } else if provider == "codex" {
            let codex_model = model_clone_for_exec.as_deref().and_then(codex::strip_codex_prefix);
            let codex_system_prompt = format!("{}{}", system_prompt_owned, codex_extra_instructions());
            codex::execute_command_streaming(
                &prompt,
                None,
                &workspace_path_for_claude,
                tx.clone(),
                Some(&codex_system_prompt),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                codex_model,
                false,
            )
        } else {
            let claude_model = model_clone_for_exec.as_deref().and_then(claude::strip_claude_prefix);
            claude::execute_command_streaming(
                &prompt,
                None,
                &workspace_path_for_claude,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                claude_model,
                false,
                sched_chrome_enabled,
            )
        };
        if let Err(e) = result {
            let _ = tx.send(StreamMessage::Error { message: e, stdout: String::new(), stderr: String::new(), exit_code: None });
        }
    });

    // Polling loop
    let bot_owned = bot.clone();
    let state_owned = state.clone();
    let entry_clone = entry.clone();
    let workspace_path_owned = workspace_path.clone();
    let provider_str: &'static str = detect_provider(model.as_deref());
    tokio::spawn(async move {
        let _group_lock = group_lock; // hold group chat lock until task ends
        const SPINNER: &[&str] = &[
            "🕐 P",           "🕑 Pr",          "🕒 Pro",
            "🕓 Proc",        "🕔 Proce",       "🕕 Proces",
            "🕖 Process",     "🕗 Processi",    "🕘 Processin",
            "🕙 Processing",  "🕚 Processing.", "🕛 Processing..",
        ];
        let mut full_response = String::new();
        let mut raw_entries: Vec<RawPayloadEntry> = Vec::new();
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut had_error = false;
        let mut spin_idx: usize = 0;
        let mut pending_cokacdir = false;
        let mut suppress_tool_display = false;
        let mut last_tool_name: String = String::new();
        let mut exec_session_id: Option<String> = None;
        let mut placeholder_msg_id = placeholder_msg_id;
        let mut last_confirmed_len: usize = 0;

        let (polling_time_ms, silent_mode) = {
            let data = state_owned.lock().await;
            (data.polling_time_ms, is_silent(&data.settings, chat_id))
        };

        let mut queue_done = false;
        while !done || !queue_done {
            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done { cancelled = true; }
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(polling_time_ms)).await;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done { cancelled = true; }
                break;
            }

            // Drain messages
            loop {
                match rx.try_recv() {
                    Ok(msg) => {
                        match msg {
                            StreamMessage::Init { session_id } => {
                                exec_session_id = Some(session_id);
                            }
                            StreamMessage::Text { content } => {
                                sched_debug(&format!("[sched] Text: {} chars, preview={:?}",
                                    content.len(), truncate_str(&content, 80)));
                                raw_entries.push(RawPayloadEntry { tag: "Text".into(), content: content.clone() });
                                let _fr_before = full_response.len();
                                full_response.push_str(&content);
                                sched_debug(&format!("[fr_trace][{}] +Text: added={}, preview={:?}, total={} (was {})",
                                    chat_id.0, content.len(), truncate_str(&content, 200), full_response.len(), _fr_before));
                            }
                            StreamMessage::ToolUse { name, input } => {
                                pending_cokacdir = detect_cokacdir_command(&name, &input);
                                suppress_tool_display = detect_chat_log_read(&name, &input);
                                last_tool_name = name.clone();
                                let summary = format_tool_input(&name, &input);
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}]   ⚙ [Schedule] {name}: {summary}");
                                sched_debug(&format!("[schedule_polling] ToolUse: name={}, input_preview={:?}, pending_cokacdir={}, silent_mode={}, response_len={}, ends_with_nl={}",
                                    name, truncate_str(&input, 200), pending_cokacdir, silent_mode, full_response.len(), full_response.ends_with('\n')));
                                raw_entries.push(RawPayloadEntry { tag: "ToolUse".into(), content: format!("{}: {}", name, input) });
                                if !pending_cokacdir && !silent_mode {
                                    let _fr_before = full_response.len();
                                    if name == "Bash" {
                                        full_response.push_str(&format!("\n\n```\n{}\n```\n", format_bash_command(&input)));
                                        sched_debug(&format!("[fr_trace][{}] +ToolUse/Bash: added={}, cmd={:?}, total={} (was {})",
                                            chat_id.0, full_response.len() - _fr_before, truncate_str(&input, 100), full_response.len(), _fr_before));
                                    } else {
                                        full_response.push_str(&format!("\n\n⚙️ {}\n", summary));
                                        sched_debug(&format!("[fr_trace][{}] +ToolUse/{}: added={}, summary={:?}, total={} (was {})",
                                            chat_id.0, name, full_response.len() - _fr_before, truncate_str(&summary, 100), full_response.len(), _fr_before));
                                    }
                                } else if !pending_cokacdir && silent_mode && !full_response.is_empty() && !full_response.ends_with('\n') {
                                    sched_debug(&format!("[schedule_polling] silent mode: inserting \\n\\n after tool_use={}", name));
                                    full_response.push_str("\n\n");
                                    sched_debug(&format!("[fr_trace][{}] +ToolUse/silent_nl: added=2, total={}", chat_id.0, full_response.len()));
                                } else if silent_mode {
                                    sched_debug(&format!("[schedule_polling] silent mode: skipped \\n\\n (pending_cokacdir={}, empty={}, ends_nl={})",
                                        pending_cokacdir, full_response.is_empty(), full_response.ends_with('\n')));
                                }
                            }
                            StreamMessage::ToolResult { content, is_error } => {
                                sched_debug(&format!("[schedule_polling] ToolResult: is_error={}, content_len={}, pending_cokacdir={}, last_tool={}", is_error, content.len(), pending_cokacdir, last_tool_name));
                                if is_error {
                                    sched_debug(&format!("[schedule_polling] ToolResult ERROR: last_tool={}, content_preview={:?}", last_tool_name, truncate_str(&content, 300)));
                                }
                                raw_entries.push(RawPayloadEntry { tag: "ToolResult".into(), content: format!("is_error={}, content={}", is_error, content) });
                                let _fr_before = full_response.len();
                                if std::mem::take(&mut pending_cokacdir) {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    if std::mem::take(&mut suppress_tool_display) {
                                        println!("  [{ts}]   ↩ [Schedule] cokacdir (chat_log, suppressed)");
                                        sched_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir_suppressed: added=0, total={}", chat_id.0, full_response.len()));
                                    } else {
                                        println!("  [{ts}]   ↩ [Schedule] cokacdir: {content}");
                                        let formatted = format_cokacdir_result(&content);
                                        if !formatted.is_empty() {
                                            full_response.push_str(&format!("\n{}\n", formatted));
                                            sched_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir: added={}, preview={:?}, total={} (was {})",
                                                chat_id.0, full_response.len() - _fr_before, truncate_str(&formatted, 200), full_response.len(), _fr_before));
                                        } else {
                                            sched_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir: formatted_empty, added=0, total={}", chat_id.0, full_response.len()));
                                        }
                                    }
                                } else if is_error && !silent_mode {
                                    let truncated = truncate_str(&content, 500);
                                    if truncated.contains('\n') {
                                        full_response.push_str(&format!("\n```\n{}\n```\n", truncated));
                                    } else {
                                        full_response.push_str(&format!("\n`{}`\n\n", truncated));
                                    }
                                    sched_debug(&format!("[fr_trace][{}] +ToolResult/error({}): added={}, preview={:?}, total={} (was {})",
                                        chat_id.0, last_tool_name, full_response.len() - _fr_before, truncate_str(&truncated, 200), full_response.len(), _fr_before));
                                } else if !silent_mode {
                                    if last_tool_name == "Read" {
                                        full_response.push_str(&format!("\n✅ `{} bytes`\n\n", content.len()));
                                        sched_debug(&format!("[fr_trace][{}] +ToolResult/Read: added={}, content_bytes={}, total={} (was {})",
                                            chat_id.0, full_response.len() - _fr_before, content.len(), full_response.len(), _fr_before));
                                    } else if !content.is_empty() {
                                        let truncated = truncate_str(&content, 300);
                                        if truncated.contains('\n') {
                                            full_response.push_str(&format!("\n```\n{}\n```\n", truncated));
                                        } else {
                                            full_response.push_str(&format!("\n✅ `{}`\n\n", truncated));
                                        }
                                        sched_debug(&format!("[fr_trace][{}] +ToolResult/{}(normal): added={}, raw_content_len={}, preview={:?}, total={} (was {})",
                                            chat_id.0, last_tool_name, full_response.len() - _fr_before, content.len(), truncate_str(&truncated, 200), full_response.len(), _fr_before));
                                    } else {
                                        sched_debug(&format!("[fr_trace][{}] +ToolResult/{}(normal): content_empty, added=0, total={}", chat_id.0, last_tool_name, full_response.len()));
                                    }
                                } else {
                                    sched_debug(&format!("[fr_trace][{}] +ToolResult/silent: skipped, last_tool={}, content_len={}, total={}", chat_id.0, last_tool_name, content.len(), full_response.len()));
                                }
                            }
                            StreamMessage::TaskNotification { summary, .. } => {
                                if !summary.is_empty() {
                                    raw_entries.push(RawPayloadEntry { tag: "TaskNotification".into(), content: summary.clone() });
                                    let _fr_before = full_response.len();
                                    full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                    sched_debug(&format!("[fr_trace][{}] +TaskNotification: added={}, summary={:?}, total={} (was {})",
                                        chat_id.0, full_response.len() - _fr_before, truncate_str(&summary, 200), full_response.len(), _fr_before));
                                }
                            }
                            StreamMessage::Done { result, session_id } => {
                                sched_debug(&format!("[sched] Done: result_len={}, full_response_len={}",
                                    result.len(), full_response.len()));
                                if !result.is_empty() && full_response.is_empty() {
                                    sched_debug(&format!("[sched] Done: fallback full_response = result ({})", result.len()));
                                    full_response = result.clone();
                                    sched_debug(&format!("[fr_trace][{}] +Done/fallback: set={}, preview={:?}, total={}",
                                        chat_id.0, result.len(), truncate_str(&full_response, 200), full_response.len()));
                                } else if !result.is_empty() {
                                    sched_debug(&format!("[fr_trace][{}] Done/discarded: result_len={} discarded (full_response already has {})",
                                        chat_id.0, result.len(), full_response.len()));
                                }
                                if !result.is_empty() && raw_entries.is_empty() {
                                    raw_entries.push(RawPayloadEntry { tag: "Text".into(), content: result });
                                }
                                if let Some(sid) = session_id {
                                    exec_session_id = Some(sid);
                                }
                                sched_debug(&format!("[fr_trace][{}] =DONE: final_total={}", chat_id.0, full_response.len()));
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
                                sched_debug(&format!("[fr_trace][{}] +Error: set={}, stdout_len={}, stderr_len={}, total={}",
                                    chat_id.0, full_response.len(), stdout_display.len(), stderr_display.len(), full_response.len()));
                                raw_entries.push(RawPayloadEntry { tag: "Error".into(), content: format!("exit_code={}, message={}, stdout={}, stderr={}", code_display, message, stdout_display, stderr_display) });
                                had_error = true;
                                done = true;
                            }
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        if !done { had_error = true; }
                        done = true;
                        break;
                    }
                }
            }

            // Update placeholder with progress
            if !done {
                // ── Rolling placeholder pattern (unified for all chats) ──
                let threshold = file_attach_threshold();
                let delta_end = floor_char_boundary(&full_response, full_response.len().min(threshold));
                if delta_end > last_confirmed_len {
                    let delta = &full_response[last_confirmed_len..delta_end];
                    let normalized_delta = normalize_empty_lines(delta);
                    let html_delta = markdown_to_telegram_html(&normalized_delta);
                    if html_delta.trim().is_empty() {
                        msg_debug(&format!("[rolling_ph/sched] SKIP empty delta: placeholder_msg_id={}, delta_bytes={}, confirmed={}→{}",
                            placeholder_msg_id, delta.len(), last_confirmed_len, delta_end));
                        last_confirmed_len = delta_end;
                    } else {
                        msg_debug(&format!("[rolling_ph/sched] EDIT delta: placeholder_msg_id={}, delta_len={}, html_len={}, confirmed={}→{}",
                            placeholder_msg_id, normalized_delta.len(), html_delta.len(), last_confirmed_len, delta_end));
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        if html_delta.len() <= TELEGRAM_MSG_LIMIT {
                            let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_delta)
                                .parse_mode(ParseMode::Html).await);
                        } else {
                            if send_long_message(&bot_owned, chat_id, &html_delta, Some(ParseMode::Html), &state_owned).await.is_ok() {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                            } else {
                                let truncated_delta = truncate_str(&normalized_delta, TELEGRAM_MSG_LIMIT);
                                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &truncated_delta).await);
                            }
                        }
                        last_confirmed_len = delta_end;
                        let old_ph_id = placeholder_msg_id;
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        match tg!("send_message", bot_owned.send_message(chat_id, "...").await) {
                            Ok(new_ph) => {
                                placeholder_msg_id = new_ph.id;
                                msg_debug(&format!("[rolling_ph/sched] NEW placeholder: old_msg_id={}, new_msg_id={}", old_ph_id, placeholder_msg_id));
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}]   ⚠ new placeholder failed (schedule): {e}");
                                msg_debug(&format!("[rolling_ph/sched] NEW placeholder FAILED: keeping msg_id={}, err={}", placeholder_msg_id, e));
                            }
                        }
                        last_edit_text.clear();
                        spin_idx = 0;
                    }
                } else {
                    let indicator = SPINNER[spin_idx % SPINNER.len()];
                    spin_idx += 1;
                    let display_text = indicator.to_string();
                    if display_text != last_edit_text {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let html_text = markdown_to_telegram_html(&display_text);
                        let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_text)
                            .parse_mode(ParseMode::Html).await);
                        last_edit_text = display_text;
                    } else {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("send_chat_action", bot_owned.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await);
                    }
                }
            }

            // Queue processing
            let queued = process_upload_queue(&bot_owned, chat_id, &state_owned).await;
            if done {
                queue_done = !queued;
            }
        }

        // Final response
        sched_debug(&format!("[execute_schedule] id={}, polling done: cancelled={}, had_error={}, response_len={}",
            schedule_id, cancelled, had_error, full_response.len()));
        if cancelled {
            sched_debug(&format!("[execute_schedule] id={}, cancelled — killing child process", schedule_id));
            if let Ok(guard) = cancel_token.child_pid.lock() {
                if let Some(pid) = *guard {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                    #[cfg(windows)]
                    { let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output(); }
                }
            }

            shared_rate_limit_wait(&state_owned, chat_id).await;
            // ── Show remaining delta + stopped (unified rolling placeholder) ──
            if full_response.len() < last_confirmed_len || !full_response.is_char_boundary(last_confirmed_len) { last_confirmed_len = 0; }
            let remaining = &full_response[last_confirmed_len..];
            if should_attach_response_as_file(full_response.len(), provider_str) {
                let notice = format!("\u{1f4c4} Response attached as file [Stopped]\n\nUse /{} to continue this schedule session.", schedule_id);
                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &notice).await);
                let stopped_content = format!("{}\n\n[Stopped]", normalize_empty_lines(&full_response));
                send_response_as_file(&bot_owned, chat_id, &stopped_content, &state_owned, "schedule").await;
            } else {
                let display_stopped = if remaining.trim().is_empty() {
                    format!("⛔ Stopped\n\nUse /{} to continue this schedule session.", schedule_id)
                } else {
                    let normalized = normalize_empty_lines(remaining);
                    format!("{}\n\n⛔ Stopped\n\nUse /{} to continue this schedule session.", normalized, schedule_id)
                };
                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &display_stopped).await);
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ [Schedule] Stopped");
        } else {
            if full_response.is_empty() {
                full_response = "(No response)".to_string();
                sched_debug(&format!("[fr_trace][{}] =NoResponse: set to '(No response)'", chat_id.0));
            }

            shared_rate_limit_wait(&state_owned, chat_id).await;

            // ── Send only remaining delta (unified rolling placeholder) ──
            if full_response.len() < last_confirmed_len || !full_response.is_char_boundary(last_confirmed_len) { last_confirmed_len = 0; }
            let remaining = &full_response[last_confirmed_len..];
            msg_debug(&format!("[rolling_ph/sched] FINAL: placeholder_msg_id={}, confirmed={}, total={}, remaining_len={}",
                placeholder_msg_id, last_confirmed_len, full_response.len(), remaining.trim().len()));
            if remaining.trim().is_empty() {
                msg_debug(&format!("[rolling_ph/sched] FINAL DELETE placeholder: msg_id={}", placeholder_msg_id));
                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
            } else if should_attach_response_as_file(full_response.len(), provider_str) {
                msg_debug(&format!("[rolling_ph/sched] FINAL FILE ATTACH: total={}", full_response.len()));
                let notice = format!("\u{1f4c4} Response attached as file\n\nUse /{} to continue this schedule session.", schedule_id);
                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &notice).await);
                let final_text = format!("{}\n\nUse /{} to continue this schedule session.", normalize_empty_lines(&full_response), schedule_id);
                send_response_as_file(&bot_owned, chat_id, &final_text, &state_owned, "schedule").await;
            } else {
                let normalized_remaining = normalize_empty_lines(remaining);
                let final_text = format!("{}\n\nUse /{} to continue this schedule session.", normalized_remaining, schedule_id);
                let html_response = markdown_to_telegram_html(&final_text);
                msg_debug(&format!("[rolling_ph/sched] FINAL EDIT placeholder: msg_id={}, html_len={}", placeholder_msg_id, html_response.len()));
                if html_response.len() <= TELEGRAM_MSG_LIMIT {
                    if let Err(_) = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_response)
                        .parse_mode(ParseMode::Html).await)
                    {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &final_text).await);
                    }
                } else {
                    let send_result = send_long_message(&bot_owned, chat_id, &html_response, Some(ParseMode::Html), &state_owned).await;
                    match send_result {
                        Ok(_) => {
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                        }
                        Err(_) => {
                            let fallback = send_long_message(&bot_owned, chat_id, &final_text, None, &state_owned).await;
                            match fallback {
                                Ok(_) => {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                                }
                                Err(_) => {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let truncated = truncate_str(&final_text, TELEGRAM_MSG_LIMIT);
                                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &truncated).await);
                                }
                            }
                        }
                    }
                }
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ✓ [Schedule] Done");

            // Send end hook message if configured
            if !cancelled {
                let end_hook_msg = {
                    let data = state_owned.lock().await;
                    data.settings.end_hook.get(&chat_id.0.to_string()).cloned()
                };
                if let Some(hook_msg) = end_hook_msg {
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!("send_message", bot_owned.send_message(chat_id, &hook_msg).await);
                }
            }
        }

        // For cron entries with context_summary, extract result summary for next run
        // Skip if execution was cancelled or encountered an error
        sched_debug(&format!("[execute_schedule] id={}, checking context summary: cancelled={}, had_error={}, type={}, once={:?}, has_context={}",
            schedule_id, cancelled, had_error, entry_clone.schedule_type, entry_clone.once, entry_clone.context_summary.is_some()));
        let sched_provider = detect_provider(model_for_summary.as_deref());
        let new_context_summary = if sched_provider != "claude" {
            // Codex/Gemini/OpenCode: skip summary extraction (not supported via Claude API)
            sched_debug(&format!("[execute_schedule] id={}, non-Claude backend — skipping context summary", schedule_id));
            None
        } else if !cancelled && !had_error && entry_clone.schedule_type == "cron" && !entry_clone.once.unwrap_or(false) && entry_clone.context_summary.is_some() {
            sched_debug(&format!("[execute_schedule] id={}, extracting result summary", schedule_id));
            if let Some(ref sid) = exec_session_id {
                let sid = sid.clone();
                let path = workspace_path_owned.clone();
                let model = model_for_summary.clone();
                let summary_result = tokio::task::spawn_blocking(move || {
                    let claude_model = model.as_deref().and_then(claude::strip_claude_prefix);
                    claude::extract_result_summary(
                        &sid,
                        &path,
                        claude_model,
                    )
                }).await;
                match summary_result {
                    Ok(Ok(ref summary)) => {
                        sched_debug(&format!("[execute_schedule] id={}, new context summary: {} chars", schedule_id, summary.len()));
                        Some(summary.clone())
                    }
                    _ => {
                        sched_debug(&format!("[execute_schedule] id={}, summary extraction failed", schedule_id));
                        None
                    }
                }
            } else {
                sched_debug(&format!("[execute_schedule] id={}, no session_id for summary", schedule_id));
                None
            }
        } else {
            None
        };

        // Save schedule session to file so user can resume via /start [workspace_path]
        if let Some(ref sid) = exec_session_id {
            let mut sched_session = ChatSession {
                session_id: Some(sid.clone()),
                current_path: Some(workspace_path_owned.clone()),
                history: Vec::new(),
                pending_uploads: Vec::new(),
            };
            // Add user prompt and AI response to history for session continuity
            sched_session.history.push(HistoryItem {
                item_type: HistoryType::User,
                content: entry_clone.prompt.clone(),
            });
            if !full_response.is_empty() {
                sched_session.history.push(HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: full_response.clone(),
                });
            }
            // sched_provider already computed above
            save_session_to_file(&sched_session, &workspace_path_owned, sched_provider);
        }

        // Write to group chat shared log (scheduled task)
        sched_debug(&format!("[sched] JSONL check: chat_id={}, raw_entries_count={}",
            chat_id.0, raw_entries.len()));
        if chat_id.0 < 0 && !sched_bot_username.is_empty() {
            let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
            let dn = if sched_bot_display_name.is_empty() { None } else { Some(sched_bot_display_name.clone()) };
            append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                ts: now_ts.clone(),
                bot: sched_bot_username.clone(),
                bot_display_name: dn.clone(),
                role: "user".to_string(),
                from: Some("scheduled_task".to_string()),
                text: entry_clone.prompt.clone(),
                clear: false,
            });
            if !raw_entries.is_empty() {
                sched_debug(&format!("[sched] JSONL: writing user+assistant entries, raw_entries_count={}", raw_entries.len()));
                append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                    ts: now_ts,
                    bot: sched_bot_username.clone(),
                    bot_display_name: dn,
                    role: "assistant".to_string(),
                    from: None,
                    text: serialize_payload(&std::mem::take(&mut raw_entries)),
                    clear: false,
                });
            } else {
                sched_debug(&format!("[sched] JSONL: user entry written, assistant SKIPPED (raw_entries is empty)"));
            }
        }

        // Update schedule file (last_run / delete if once)
        sched_debug(&format!("[execute_schedule] id={}, calling update_schedule_after_run", schedule_id));
        update_schedule_after_run(&entry_clone, new_context_summary);

        // Workspace directory is preserved for user to continue work via /start

        // Clean up + restore previous session
        sched_debug(&format!("[execute_schedule] id={}, cleaning up: removing cancel_token, pending, restoring session (has_prev={})",
            schedule_id, prev_session.is_some()));
        {
            let mut data = state_owned.lock().await;
            data.cancel_tokens.remove(&chat_id);
            if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                set.remove(&schedule_id);
            }
            if let Some(prev) = prev_session {
                data.sessions.insert(chat_id, prev);
            } else {
                // No prior session existed — remove the schedule's temporary session
                data.sessions.remove(&chat_id);
            }
        }
        sched_debug(&format!("[execute_schedule] id={}, END", schedule_id));

        // Clean up leftover stop message
        let stop_msg_id = {
            let mut data = state_owned.lock().await;
            data.stop_message_ids.remove(&chat_id)
        };
        if let Some(msg_id) = stop_msg_id {
            shared_rate_limit_wait(&state_owned, chat_id).await;
            let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
        }
        msg_debug(&format!("[queue:trigger] chat_id={}, source=schedule_poll_completed", chat_id.0));
        drop(_group_lock); // release group chat lock before processing queue
        process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
    });
}

/// Process an incoming bot-to-bot message (follows execute_schedule pattern)
async fn process_bot_message(
    bot: &Bot,
    chat_id: ChatId,
    msg: &BotMessage,
    state: &SharedState,
    token: &str,
    bot_username: &str,
    bot_display_name: &str,
) {
    msg_debug(&format!("[process_bot_message] START id={}, from={}, to={}, chat_id={}, content_len={}, bot_username={}",
        msg.id, msg.from, msg.to, chat_id.0, msg.content.len(), bot_username));

    // Register cancel token early (prevents duplicate requests while waiting for group lock)
    let cancel_token = Arc::new(CancelToken::new());
    {
        let mut data = state.lock().await;
        if data.cancel_tokens.contains_key(&chat_id) {
            msg_debug(&format!("[process_bot_message] chat_id={}, cancel_token exists (busy), skipping id={}", chat_id.0, msg.id));
            return;
        }
        data.cancel_tokens.insert(chat_id, cancel_token.clone());
    }

    // Acquire group chat lock (serializes processing across bots in the same group chat)
    let group_lock = acquire_group_chat_lock(chat_id.0).await;

    // Check if cancelled during lock wait
    if cancel_token.cancelled.load(Ordering::Relaxed) {
        msg_debug(&format!("[process_bot_message] cancelled during lock wait, id={}", msg.id));
        msg_debug(&format!("[queue:trigger] chat_id={}, source=botmsg_cancelled_during_lock", chat_id.0));
        { let mut data = state.lock().await; data.cancel_tokens.remove(&chat_id); }
        drop(group_lock); // release before queue processing to avoid deadlock
        process_next_queued_message(bot, chat_id, state).await;
        return;
    }

    // Auto-restore session
    msg_debug(&format!("[process_bot_message] auto_restore_session for bot:{}", msg.from));
    auto_restore_session(state, chat_id, &format!("bot:{}", msg.from)).await;

    // Get session info, allowed tools, model, history, instruction
    let (session_info, allowed_tools, model, history, instruction, context_count, botmsg_chrome_enabled) = {
        let data = state.lock().await;
        let info = data.sessions.get(&chat_id).and_then(|session| {
            session.current_path.as_ref().map(|_| {
                (session.session_id.clone(), session.current_path.clone().unwrap_or_default())
            })
        });
        let tools = get_allowed_tools(&data.settings, chat_id);
        let mdl = get_model(&data.settings, chat_id);
        let hist = data.sessions.get(&chat_id)
            .map(|s| s.history.clone())
            .unwrap_or_default();
        let instr = data.settings.instructions.get(&chat_id.0.to_string()).cloned();
        let ctx = data.settings.context.get(&chat_id.0.to_string()).copied().unwrap_or(12);
        let chrome = data.settings.use_chrome.get(&chat_id.0.to_string()).copied().unwrap_or(false);
        msg_debug(&format!("[process_bot_message] session_info={}, tools={}, model={:?}, history_len={}, instruction={}",
            info.is_some(), tools.len(), mdl, hist.len(), instr.is_some()));
        (info, tools, mdl, hist, instr, ctx, chrome)
    };

    let (session_id, current_path) = match session_info {
        Some(info) => {
            msg_debug(&format!("[process_bot_message] session found: session_id={:?}, path={}", info.0, info.1));
            info
        }
        None => {
            // No active session — create an error response
            msg_debug(&format!("[process_bot_message] no session for chat_id={}, sending error response", chat_id.0));
            {
                let mut data = state.lock().await;
                data.cancel_tokens.remove(&chat_id);
                let cleared = data.message_queues.remove(&chat_id).map(|q| q.len()).unwrap_or(0);
                if cleared > 0 {
                    msg_debug(&format!("[queue:clear] chat_id={}, no session (bot msg), cleared {} queued messages", chat_id.0, cleared));
                }
            }
            shared_rate_limit_wait(state, chat_id).await;
            let _ = tg!("send_message", bot.send_message(chat_id,
                format!("📨 @{}: {}\n\n⚠️ No active session. Use /start <path> first.",
                    msg.from, truncate_str(&msg.content, 200))).await);
            msg_debug("[process_bot_message] END (no session)");
            return;
        }
    };

    // Send placeholder
    msg_debug("[process_bot_message] sending placeholder");
    shared_rate_limit_wait(state, chat_id).await;
    let placeholder = match tg!("send_message", bot.send_message(chat_id, "...").await) {
        Ok(m) => {
            msg_debug(&format!("[process_bot_message] placeholder sent: msg_id={}", m.id));
            m
        }
        Err(e) => {
            msg_debug(&format!("[process_bot_message] failed to send placeholder: {}, aborting", e));
            msg_debug(&format!("[queue:trigger] chat_id={}, source=botmsg_placeholder_error", chat_id.0));
            { let mut data = state.lock().await; data.cancel_tokens.remove(&chat_id); }
            drop(group_lock); // release before queue processing to avoid deadlock
            process_next_queued_message(bot, chat_id, state).await;
            return;
        }
    };
    let placeholder_msg_id = placeholder.id;

    // Build disabled tools notice
    let default_tools: std::collections::HashSet<&str> = DEFAULT_ALLOWED_TOOLS.iter().copied().collect();
    let allowed_set: std::collections::HashSet<&str> = allowed_tools.iter().map(|s| s.as_str()).collect();
    let disabled: Vec<&&str> = default_tools.iter().filter(|t| !allowed_set.contains(**t)).collect();
    msg_debug(&format!("[process_bot_message] disabled_tools={}", disabled.len()));
    let disabled_notice = if disabled.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = disabled.iter().map(|t| **t).collect();
        msg_debug(&format!("[process_bot_message] disabled: [{}]", names.join(", ")));
        format!(
            "\n\nDISABLED TOOLS: The following tools have been disabled by the user: {}.\n\
             You MUST NOT attempt to use these tools. \
             If a user's request requires a disabled tool, do NOT proceed with the task. \
             Instead, clearly inform the user which tool is needed and that it is currently disabled. \
             Suggest they re-enable it with: /allowed +ToolName",
            names.join(", ")
        )
    };

    // Build system prompt
    let bot_key = token_hash(token);
    let platform = capitalize_platform(detect_platform(token));
    let role = match &instruction {
        Some(instr) => {
            msg_debug(&format!("[process_bot_message] instruction present, len={}", instr.len()));
            format!("You are chatting with a user through {}.\n\nUser's instruction for this chat:\n{}", platform, instr)
        }
        None => {
            msg_debug("[process_bot_message] no instruction set");
            format!("You are chatting with a user through {}.", platform)
        }
    };
    let system_prompt_owned = build_system_prompt(
        &role,
        &current_path, chat_id.0, &bot_key, &disabled_notice,
        session_id.as_deref(), bot_username, bot_display_name,
        None, // bot-to-bot messages: no user message dedup
        context_count, &platform,
    );
    msg_debug(&format!("[process_bot_message] system_prompt built, len={}", system_prompt_owned.len()));

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();

    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();
    let sender_display = resolve_display_name_by_username(&msg.from)
        .map(|dn| format!("{} (@{})", dn, msg.from))
        .unwrap_or_else(|| format!("@{}", msg.from));
    let prompt = format!("[BOT MESSAGE from {}]\n{}", sender_display, msg.content);

    // Run AI backend in a blocking thread
    let model_clone = model.clone();
    let history_clone = history;
    let prompt_for_ai = prompt.clone();
    msg_debug(&format!("[process_bot_message] spawning AI backend: model={:?}, history_len={}, prompt_len={}",
        model_clone, history_clone.len(), prompt_for_ai.len()));
    tokio::task::spawn_blocking(move || {
        let provider = detect_provider(model_clone.as_deref());
        msg_debug(&format!("[process_bot_message:spawn_blocking] provider={}, model={:?}", provider, model_clone));
        let result = if provider == "opencode" {
            let opencode_model = model_clone.as_deref().and_then(opencode::strip_opencode_prefix);
            msg_debug(&format!("[process_bot_message] → opencode::execute, opencode_model={:?}, session_id={:?}, path={}, prompt_len={}, system_prompt_len={}",
                opencode_model, session_id_clone, current_path_clone, prompt_for_ai.len(), system_prompt_owned.len()));
            opencode::execute_command_streaming(
                &prompt_for_ai,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                opencode_model,
                false,
            )
        } else if provider == "gemini" {
            let gemini_model = model_clone.as_deref().and_then(gemini::strip_gemini_prefix);
            msg_debug(&format!("[process_bot_message] → gemini::execute, gemini_model={:?}, session_id={:?}, path={}, prompt_len={}, system_prompt_len={}",
                gemini_model, session_id_clone, current_path_clone, prompt_for_ai.len(), system_prompt_owned.len()));
            gemini::execute_command_streaming(
                &prompt_for_ai,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                gemini_model,
                false,
            )
        } else if provider == "codex" {
            let codex_model = model_clone.as_deref().and_then(codex::strip_codex_prefix);
            let codex_system_prompt = format!("{}{}", system_prompt_owned, codex_extra_instructions());
            let is_resume = session_id_clone.is_some();
            let has_history = !history_clone.is_empty();
            msg_debug(&format!("[process_bot_message] codex: is_resume={}, has_history={}, history_len={}, sp_len={}",
                is_resume, has_history, history_clone.len(), codex_system_prompt.len()));
            let codex_prompt = if session_id_clone.is_none() && !history_clone.is_empty() {
                msg_debug("[process_bot_message] codex: INJECTING history into prompt (new session with history)");
                let mut conv = String::new();
                conv.push_str("<conversation_history>\n");
                for item in &history_clone {
                    let role = match item.item_type {
                        HistoryType::User => "User",
                        HistoryType::Assistant => "Assistant",
                        HistoryType::ToolUse => "ToolUse",
                        HistoryType::ToolResult => "ToolResult",
                        _ => continue,
                    };
                    conv.push_str(&format!("[{}]: {}\n", role, item.content));
                }
                conv.push_str("</conversation_history>\n\n");
                conv.push_str(&prompt_for_ai);
                conv
            } else {
                if is_resume {
                    msg_debug("[process_bot_message] codex: RESUME path — no history injection, sp via -c file");
                } else {
                    msg_debug("[process_bot_message] codex: NEW session, no history — sp via -c file");
                }
                prompt_for_ai.clone()
            };
            msg_debug(&format!("[process_bot_message] → codex::execute, codex_model={:?}, codex_prompt_len={}, resume={}, system_prompt_passed=true",
                codex_model, codex_prompt.len(), is_resume));
            codex::execute_command_streaming(
                &codex_prompt,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&codex_system_prompt),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                codex_model,
                false,
            )
        } else {
            let claude_model = model_clone.as_deref().and_then(claude::strip_claude_prefix);
            claude::execute_command_streaming(
                &prompt_for_ai,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                claude_model,
                false,
                botmsg_chrome_enabled,
            )
        };
        if let Err(e) = result {
            let _ = tx.send(StreamMessage::Error { message: e, stdout: String::new(), stderr: String::new(), exit_code: None });
        }
    });

    // Polling loop (spawned as async task to avoid blocking scheduler)
    let bot_owned = bot.clone();
    let state_owned = state.clone();
    let msg_clone = msg.clone();
    let provider_str: &'static str = detect_provider(model.as_deref());
    let current_path_owned = current_path.clone();
    let prompt_owned = prompt.clone();
    let bmsg_id_for_log = msg.id.clone();
    let bot_username_for_log = bot_username.to_string();
    let bot_display_name_for_log = bot_display_name.to_string();
    let from_bot_for_log = msg.from.clone();
    msg_debug(&format!("[process_bot_message] spawning polling loop: provider={}, msg_id={}, placeholder_msg_id={}",
        provider_str, bmsg_id_for_log, placeholder_msg_id));
    tokio::spawn(async move {
        let _group_lock = group_lock; // hold group chat lock until task ends
        const SPINNER: &[&str] = &[
            "🕐 P",           "🕑 Pr",          "🕒 Pro",
            "🕓 Proc",        "🕔 Proce",       "🕕 Proces",
            "🕖 Process",     "🕗 Processi",    "🕘 Processin",
            "🕙 Processing",  "🕚 Processing.", "🕛 Processing..",
        ];
        let mut full_response = String::new();
        let mut raw_entries: Vec<RawPayloadEntry> = Vec::new();
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut new_session_id: Option<String> = None;
        let mut spin_idx: usize = 0;
        let mut pending_cokacdir = false;
        let mut suppress_tool_display = false;
        let mut last_tool_name: String = String::new();
        let mut placeholder_msg_id = placeholder_msg_id;
        let mut last_confirmed_len: usize = 0;

        let (polling_time_ms, silent_mode) = {
            let data = state_owned.lock().await;
            (data.polling_time_ms, is_silent(&data.settings, chat_id))
        };
        msg_debug(&format!("[botmsg_poll:{}] started: polling_time_ms={}, silent_mode={}", bmsg_id_for_log, polling_time_ms, silent_mode));

        let mut queue_done = false;
        let mut response_rendered = false;
        while !done || !queue_done {
            if cancel_token.cancelled.load(Ordering::Relaxed) {
                msg_debug(&format!("[botmsg_poll:{}] cancelled (pre-sleep check)", bmsg_id_for_log));
                if !done { cancelled = true; }
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(polling_time_ms)).await;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done { cancelled = true; }
                break;
            }

            // Drain messages
            if !done {
                loop {
                    match rx.try_recv() {
                        Ok(stream_msg) => {
                            match stream_msg {
                                StreamMessage::Init { session_id: sid } => {
                                    msg_debug(&format!("[botmsg_poll:{}] Init: session_id={}", bmsg_id_for_log, sid));
                                    new_session_id = Some(sid);
                                }
                                StreamMessage::Text { content } => {
                                    msg_debug(&format!("[botmsg_poll:{}] Text: chunk_len={}, total_len={}",
                                        bmsg_id_for_log, content.len(), full_response.len() + content.len()));
                                    raw_entries.push(RawPayloadEntry { tag: "Text".into(), content: content.clone() });
                                    let _fr_before = full_response.len();
                                    full_response.push_str(&content);
                                    msg_debug(&format!("[fr_trace][{}] +Text: added={}, preview={:?}, total={} (was {})",
                                        chat_id.0, content.len(), truncate_str(&content, 200), full_response.len(), _fr_before));
                                }
                                StreamMessage::ToolUse { name, input } => {
                                    pending_cokacdir = detect_cokacdir_command(&name, &input);
                                    suppress_tool_display = detect_chat_log_read(&name, &input);
                                    last_tool_name = name.clone();
                                    let summary = format_tool_input(&name, &input);
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!("  [{ts}]   ⚙ [BotMsg] {name}: {summary}");
                                    msg_debug(&format!("[botmsg_poll:{}] ToolUse: name={}, input_preview={:?}, pending_cokacdir={}, silent={}, response_len={}, ends_nl={}",
                                        bmsg_id_for_log, name, truncate_str(&input, 200), pending_cokacdir, silent_mode, full_response.len(), full_response.ends_with('\n')));
                                    raw_entries.push(RawPayloadEntry { tag: "ToolUse".into(), content: format!("{}: {}", name, input) });
                                    if !pending_cokacdir && !silent_mode {
                                        let _fr_before = full_response.len();
                                        if name == "Bash" {
                                            full_response.push_str(&format!("\n\n```\n{}\n```\n", format_bash_command(&input)));
                                            msg_debug(&format!("[fr_trace][{}] +ToolUse/Bash: added={}, cmd={:?}, total={} (was {})",
                                                chat_id.0, full_response.len() - _fr_before, truncate_str(&input, 100), full_response.len(), _fr_before));
                                        } else {
                                            full_response.push_str(&format!("\n\n⚙️ {}\n", summary));
                                            msg_debug(&format!("[fr_trace][{}] +ToolUse/{}: added={}, summary={:?}, total={} (was {})",
                                                chat_id.0, name, full_response.len() - _fr_before, truncate_str(&summary, 100), full_response.len(), _fr_before));
                                        }
                                    } else if !pending_cokacdir && silent_mode && !full_response.is_empty() && !full_response.ends_with('\n') {
                                        msg_debug(&format!("[botmsg_poll:{}] silent mode: inserting \\n\\n after tool_use={}", bmsg_id_for_log, name));
                                        full_response.push_str("\n\n");
                                        msg_debug(&format!("[fr_trace][{}] +ToolUse/silent_nl: added=2, total={}", chat_id.0, full_response.len()));
                                    }
                                }
                                StreamMessage::ToolResult { content, is_error } => {
                                    msg_debug(&format!("[botmsg_poll:{}] ToolResult: is_error={}, content_len={}, pending_cokacdir={}, last_tool={}",
                                        bmsg_id_for_log, is_error, content.len(), pending_cokacdir, last_tool_name));
                                    if is_error {
                                        msg_debug(&format!("[botmsg_poll:{}] ToolResult ERROR: last_tool={}, content_preview={:?}", bmsg_id_for_log, last_tool_name, truncate_str(&content, 300)));
                                    }
                                    raw_entries.push(RawPayloadEntry { tag: "ToolResult".into(), content: format!("is_error={}, content={}", is_error, content) });
                                    let _fr_before = full_response.len();
                                    if std::mem::take(&mut pending_cokacdir) {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        if std::mem::take(&mut suppress_tool_display) {
                                            println!("  [{ts}]   ↩ [BotMsg] cokacdir (chat_log, suppressed)");
                                            msg_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir_suppressed: added=0, total={}", chat_id.0, full_response.len()));
                                        } else {
                                            println!("  [{ts}]   ↩ [BotMsg] cokacdir: {content}");
                                            let formatted = format_cokacdir_result(&content);
                                            msg_debug(&format!("[botmsg_poll:{}] cokacdir result formatted_len={}", bmsg_id_for_log, formatted.len()));
                                            if !formatted.is_empty() {
                                                full_response.push_str(&format!("\n{}\n", formatted));
                                                msg_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir: added={}, preview={:?}, total={} (was {})",
                                                    chat_id.0, full_response.len() - _fr_before, truncate_str(&formatted, 200), full_response.len(), _fr_before));
                                            } else {
                                                msg_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir: formatted_empty, added=0, total={}", chat_id.0, full_response.len()));
                                            }
                                        }
                                    } else if is_error && !silent_mode {
                                        msg_debug(&format!("[botmsg_poll:{}] tool error: {}", bmsg_id_for_log, truncate_str(&content, 200)));
                                        let truncated = truncate_str(&content, 500);
                                        if truncated.contains('\n') {
                                            full_response.push_str(&format!("\n```\n{}\n```\n", truncated));
                                        } else {
                                            full_response.push_str(&format!("\n`{}`\n\n", truncated));
                                        }
                                        msg_debug(&format!("[fr_trace][{}] +ToolResult/error({}): added={}, preview={:?}, total={} (was {})",
                                            chat_id.0, last_tool_name, full_response.len() - _fr_before, truncate_str(&truncated, 200), full_response.len(), _fr_before));
                                    } else if !silent_mode {
                                        if last_tool_name == "Read" {
                                            full_response.push_str(&format!("\n✅ `{} bytes`\n\n", content.len()));
                                            msg_debug(&format!("[fr_trace][{}] +ToolResult/Read: added={}, content_bytes={}, total={} (was {})",
                                                chat_id.0, full_response.len() - _fr_before, content.len(), full_response.len(), _fr_before));
                                        } else if !content.is_empty() {
                                            let truncated = truncate_str(&content, 300);
                                            if truncated.contains('\n') {
                                                full_response.push_str(&format!("\n```\n{}\n```\n", truncated));
                                            } else {
                                                full_response.push_str(&format!("\n✅ `{}`\n\n", truncated));
                                            }
                                            msg_debug(&format!("[fr_trace][{}] +ToolResult/{}(normal): added={}, raw_content_len={}, preview={:?}, total={} (was {})",
                                                chat_id.0, last_tool_name, full_response.len() - _fr_before, content.len(), truncate_str(&truncated, 200), full_response.len(), _fr_before));
                                        } else {
                                            msg_debug(&format!("[fr_trace][{}] +ToolResult/{}(normal): content_empty, added=0, total={}", chat_id.0, last_tool_name, full_response.len()));
                                        }
                                    } else {
                                        msg_debug(&format!("[fr_trace][{}] +ToolResult/silent: skipped, last_tool={}, content_len={}, total={}", chat_id.0, last_tool_name, content.len(), full_response.len()));
                                    }
                                }
                                StreamMessage::TaskNotification { summary, .. } => {
                                    msg_debug(&format!("[botmsg_poll:{}] TaskNotification: summary_len={}", bmsg_id_for_log, summary.len()));
                                    if !summary.is_empty() {
                                        raw_entries.push(RawPayloadEntry { tag: "TaskNotification".into(), content: summary.clone() });
                                        let _fr_before = full_response.len();
                                        full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                        msg_debug(&format!("[fr_trace][{}] +TaskNotification: added={}, summary={:?}, total={} (was {})",
                                            chat_id.0, full_response.len() - _fr_before, truncate_str(&summary, 200), full_response.len(), _fr_before));
                                    }
                                }
                                StreamMessage::Done { result, session_id: sid } => {
                                    msg_debug(&format!("[botmsg_poll:{}] Done: result_len={}, session_id={:?}, full_response_len={}",
                                        bmsg_id_for_log, result.len(), sid, full_response.len()));
                                    if !result.is_empty() && full_response.is_empty() {
                                        msg_debug(&format!("[botmsg_poll:{}] Done: fallback full_response = result ({})", bmsg_id_for_log, result.len()));
                                        full_response = result.clone();
                                        msg_debug(&format!("[fr_trace][{}] +Done/fallback: set={}, preview={:?}, total={}",
                                            chat_id.0, result.len(), truncate_str(&full_response, 200), full_response.len()));
                                    } else if !result.is_empty() {
                                        msg_debug(&format!("[fr_trace][{}] Done/discarded: result_len={} discarded (full_response already has {})",
                                            chat_id.0, result.len(), full_response.len()));
                                    }
                                    if !result.is_empty() && raw_entries.is_empty() {
                                        raw_entries.push(RawPayloadEntry { tag: "Text".into(), content: result });
                                    }
                                    if let Some(s) = sid {
                                        new_session_id = Some(s);
                                    }
                                    msg_debug(&format!("[fr_trace][{}] =DONE: final_total={}", chat_id.0, full_response.len()));
                                    done = true;
                                }
                                StreamMessage::Error { message, stdout, stderr, exit_code } => {
                                    msg_debug(&format!("[botmsg_poll:{}] Error: msg={}, exit_code={:?}, stdout_len={}, stderr_len={}",
                                        bmsg_id_for_log, message, exit_code, stdout.len(), stderr.len()));
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
                                    msg_debug(&format!("[fr_trace][{}] +Error: set={}, stdout_len={}, stderr_len={}, total={}",
                                        chat_id.0, full_response.len(), stdout_display.len(), stderr_display.len(), full_response.len()));
                                    raw_entries.push(RawPayloadEntry { tag: "Error".into(), content: format!("exit_code={}, message={}, stdout={}, stderr={}", code_display, message, stdout_display, stderr_display) });
                                    done = true;
                                }
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            msg_debug(&format!("[botmsg_poll:{}] channel disconnected, setting done=true", bmsg_id_for_log));
                            done = true;
                            break;
                        }
                    }
                }

                if !done {
                    // ── Rolling placeholder pattern (unified for all chats) ──
                    let threshold = file_attach_threshold();
                    let delta_end = floor_char_boundary(&full_response, full_response.len().min(threshold));
                    if delta_end > last_confirmed_len {
                        let delta = &full_response[last_confirmed_len..delta_end];
                        let normalized_delta = normalize_empty_lines(delta);
                        let html_delta = markdown_to_telegram_html(&normalized_delta);
                        if html_delta.trim().is_empty() {
                            last_confirmed_len = delta_end;
                        } else {
                            msg_debug(&format!("[botmsg_poll:{}] finalizing placeholder with delta_len={}", bmsg_id_for_log, normalized_delta.len()));
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            if html_delta.len() <= TELEGRAM_MSG_LIMIT {
                                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_delta)
                                    .parse_mode(ParseMode::Html).await);
                            } else {
                                if send_long_message(&bot_owned, chat_id, &html_delta, Some(ParseMode::Html), &state_owned).await.is_ok() {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                                } else {
                                    let truncated_delta = truncate_str(&normalized_delta, TELEGRAM_MSG_LIMIT);
                                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &truncated_delta).await);
                                }
                            }
                            last_confirmed_len = delta_end;
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            match tg!("send_message", bot_owned.send_message(chat_id, "...").await) {
                                Ok(new_ph) => { placeholder_msg_id = new_ph.id; }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!("  [{ts}]   ⚠ new placeholder failed (botmsg): {e}");
                                }
                            }
                            last_edit_text.clear();
                            spin_idx = 0;
                        }
                    } else {
                        let indicator = SPINNER[spin_idx % SPINNER.len()];
                        spin_idx += 1;
                        let display_text = indicator.to_string();
                        if display_text != last_edit_text {
                            msg_debug(&format!("[botmsg_poll:{}] spinner update spin_idx={}", bmsg_id_for_log, spin_idx));
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let html_text = markdown_to_telegram_html(&display_text);
                            let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_text)
                                .parse_mode(ParseMode::Html).await);
                            last_edit_text = display_text;
                        } else {
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!("send_chat_action", bot_owned.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await);
                        }
                    }
                }
            }

            // Render final response once when AI completes
            if done && !response_rendered {
                response_rendered = true;
                msg_debug(&format!("[botmsg_poll:{}] rendering final response: response_len={}", bmsg_id_for_log, full_response.len()));

                shared_rate_limit_wait(&state_owned, chat_id).await;

                if full_response.is_empty() {
                    msg_debug(&format!("[botmsg_poll:{}] empty response, using placeholder text", bmsg_id_for_log));
                    full_response = "(No response)".to_string();
                    msg_debug(&format!("[fr_trace][{}] =NoResponse: set to '(No response)'", chat_id.0));
                }

                let final_response = normalize_empty_lines(&full_response);
                msg_debug(&format!("[botmsg_poll:{}] final: response_len={}, msg_limit={}",
                    bmsg_id_for_log, final_response.len(), TELEGRAM_MSG_LIMIT));

                // ── Send only remaining delta (unified rolling placeholder) ──
                if full_response.len() < last_confirmed_len || !full_response.is_char_boundary(last_confirmed_len) { last_confirmed_len = 0; }
                let remaining = &full_response[last_confirmed_len..];
                msg_debug(&format!("[rolling_ph/botmsg] FINAL: placeholder_msg_id={}, confirmed={}, total={}, remaining_len={}",
                    placeholder_msg_id, last_confirmed_len, full_response.len(), remaining.trim().len()));
                if remaining.trim().is_empty() {
                    msg_debug(&format!("[rolling_ph/botmsg] FINAL DELETE placeholder: msg_id={}", placeholder_msg_id));
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                } else if should_attach_response_as_file(full_response.len(), provider_str) {
                    msg_debug(&format!("[rolling_ph/botmsg] FINAL FILE ATTACH: total={}", full_response.len()));
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, "\u{1f4c4} Response attached as file").await);
                    send_response_as_file(&bot_owned, chat_id, &final_response, &state_owned, "botmsg").await;
                } else {
                    let normalized_remaining = normalize_empty_lines(remaining);
                    let html_remaining = markdown_to_telegram_html(&normalized_remaining);
                    msg_debug(&format!("[botmsg_poll:{}] final delta_len={}, html_len={}",
                        bmsg_id_for_log, normalized_remaining.len(), html_remaining.len()));
                    if html_remaining.len() <= TELEGRAM_MSG_LIMIT {
                        if let Err(e) = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_remaining)
                            .parse_mode(ParseMode::Html).await)
                        {
                            msg_debug(&format!("[botmsg_poll:{}] HTML edit failed: {}", bmsg_id_for_log, e));
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &normalized_remaining).await);
                        }
                    } else {
                        let send_result = send_long_message(&bot_owned, chat_id, &html_remaining, Some(ParseMode::Html), &state_owned).await;
                        match send_result {
                            Ok(_) => {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                            }
                            Err(_) => {
                                let fallback = send_long_message(&bot_owned, chat_id, &normalized_remaining, None, &state_owned).await;
                                match fallback {
                                    Ok(_) => {
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let _ = tg!("delete_message", bot_owned.delete_message(chat_id, placeholder_msg_id).await);
                                    }
                                    Err(_) => {
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let truncated = truncate_str(&normalized_remaining, TELEGRAM_MSG_LIMIT);
                                        let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &truncated).await);
                                    }
                                }
                            }
                        }
                    }
                }

                // Update session state
                {
                    let mut data = state_owned.lock().await;
                    if let Some(session) = data.sessions.get_mut(&chat_id) {
                        msg_debug(&format!("[botmsg_poll:{}] updating session: new_session_id={:?}, history_len_before={}",
                            bmsg_id_for_log, new_session_id, session.history.len()));
                        if let Some(sid) = new_session_id.take() {
                            session.session_id = Some(sid);
                        }
                        session.history.push(HistoryItem {
                            item_type: HistoryType::User,
                            content: prompt_owned.clone(),
                        });
                        session.history.push(HistoryItem {
                            item_type: HistoryType::Assistant,
                            content: final_response.clone(),
                        });
                        save_session_to_file(session, &current_path_owned, provider_str);
                        msg_debug(&format!("[botmsg_poll:{}] session saved: history_len_after={}", bmsg_id_for_log, session.history.len()));
                    } else {
                        msg_debug(&format!("[botmsg_poll:{}] no session found for chat_id={}, skipping session update", bmsg_id_for_log, chat_id.0));
                    }
                    // Write to group chat shared log (bot-to-bot messages)
                    msg_debug(&format!("[botmsg_poll:{}] JSONL check: chat_id={}, raw_entries_count={}",
                        bmsg_id_for_log, chat_id.0, raw_entries.len()));
                    if chat_id.0 < 0 {
                        let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                        let dn = if bot_display_name_for_log.is_empty() { None } else { Some(bot_display_name_for_log.clone()) };
                        append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                            ts: now_ts.clone(),
                            bot: bot_username_for_log.clone(),
                            bot_display_name: dn.clone(),
                            role: "user".to_string(),
                            from: Some(format!("bot:{}", from_bot_for_log)),
                            text: prompt_owned.clone(),
                            clear: false,
                        });
                        if !raw_entries.is_empty() {
                            msg_debug(&format!("[botmsg_poll:{}] JSONL: writing user+assistant entries, raw_entries_count={}", bmsg_id_for_log, raw_entries.len()));
                            append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                                ts: now_ts,
                                bot: bot_username_for_log.clone(),
                                bot_display_name: dn,
                                role: "assistant".to_string(),
                                from: None,
                                text: serialize_payload(&std::mem::take(&mut raw_entries)),
                                clear: false,
                            });
                        } else {
                            msg_debug(&format!("[botmsg_poll:{}] JSONL: user entry written, assistant SKIPPED (raw_entries is empty)", bmsg_id_for_log));
                        }
                    }
                }

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ▶ [BotMsg] Response sent");
                msg_debug(&format!("[botmsg_poll:{}] response sent, final_response_len={}", bmsg_id_for_log, final_response.len()));

                // Do NOT auto-create response message file.
                // The AI can use --message CLI to reply if needed.
                // Auto-responding causes infinite ping-pong between bots.
                msg_debug(&format!("[botmsg_poll:{}] skipping auto-response (AI uses --message if needed)", bmsg_id_for_log));

                // Send end hook message if configured
                if !cancelled {
                    let end_hook_msg = {
                        let data = state_owned.lock().await;
                        data.settings.end_hook.get(&chat_id.0.to_string()).cloned()
                    };
                    if let Some(hook_msg) = end_hook_msg {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!("send_message", bot_owned.send_message(chat_id, &hook_msg).await);
                    }
                }
            }

            // Queue processing
            let queued = process_upload_queue(&bot_owned, chat_id, &state_owned).await;
            if done {
                queue_done = !queued;
                msg_debug(&format!("[botmsg_poll:{}] queue: queued={}, queue_done={}", bmsg_id_for_log, queued, queue_done));
            }
        }

        // Handle cancellation
        if cancelled {
            msg_debug(&format!("[botmsg_poll:{}] handling cancellation: response_len={}", bmsg_id_for_log, full_response.len()));
            if let Ok(guard) = cancel_token.child_pid.lock() {
                if let Some(pid) = *guard {
                    msg_debug(&format!("[botmsg_poll:{}] killing child process: pid={}", bmsg_id_for_log, pid));
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                    #[cfg(windows)]
                    { let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output(); }
                } else {
                    msg_debug(&format!("[botmsg_poll:{}] no child pid to kill", bmsg_id_for_log));
                }
            }

            // stopped_response (full) for session history
            let stopped_response = if full_response.trim().is_empty() {
                "[Stopped]".to_string()
            } else {
                let normalized = normalize_empty_lines(&full_response);
                format!("{}\n\n[Stopped]", normalized)
            };
            msg_debug(&format!("[botmsg_poll:{}] stopped_response_len={}", bmsg_id_for_log, stopped_response.len()));

            shared_rate_limit_wait(&state_owned, chat_id).await;

            // ── Show remaining delta + [Stopped] (unified rolling placeholder) ──
            if full_response.len() < last_confirmed_len || !full_response.is_char_boundary(last_confirmed_len) { last_confirmed_len = 0; }
            let remaining = &full_response[last_confirmed_len..];
            msg_debug(&format!("[rolling_ph/botmsg] STOPPED: placeholder_msg_id={}, confirmed={}, remaining_len={}",
                placeholder_msg_id, last_confirmed_len, remaining.trim().len()));
            if should_attach_response_as_file(full_response.len(), provider_str) {
                msg_debug(&format!("[rolling_ph/botmsg] STOPPED FILE ATTACH: total={}", full_response.len()));
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, "\u{1f4c4} Response attached as file [Stopped]").await);
                send_response_as_file(&bot_owned, chat_id, &stopped_response, &state_owned, "botmsg").await;
            } else {
                let display_stopped = if remaining.trim().is_empty() {
                    "[Stopped]".to_string()
                } else {
                    let normalized = normalize_empty_lines(remaining);
                    format!("{}\n\n[Stopped]", normalized)
                };
                let html_stopped = markdown_to_telegram_html(&display_stopped);
                if html_stopped.len() <= TELEGRAM_MSG_LIMIT {
                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id, &html_stopped)
                        .parse_mode(ParseMode::Html).await);
                } else {
                    let _ = tg!("edit_message", bot_owned.edit_message_text(chat_id, placeholder_msg_id,
                        &truncate_str(&display_stopped, TELEGRAM_MSG_LIMIT)).await);
                }
            }

            // Do NOT create response file on cancel → chain broken
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ [BotMsg] Stopped");
            msg_debug(&format!("[botmsg_poll:{}] stopped message sent, updating session", bmsg_id_for_log));

            let mut data = state_owned.lock().await;
            if let Some(session) = data.sessions.get_mut(&chat_id) {
                msg_debug(&format!("[botmsg_poll:{}] cancel: updating session history, new_session_id={:?}",
                    bmsg_id_for_log, new_session_id));
                if let Some(sid) = new_session_id {
                    session.session_id = Some(sid);
                }
                session.history.push(HistoryItem {
                    item_type: HistoryType::User,
                    content: prompt_owned.clone(),
                });
                session.history.push(HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: stopped_response,
                });
                save_session_to_file(session, &current_path_owned, provider_str);
                // Write to group chat shared log (bot-to-bot messages, stopped)
                msg_debug(&format!("[botmsg_poll:{}] JSONL stopped check: chat_id={}, raw_entries_count={}",
                    bmsg_id_for_log, chat_id.0, raw_entries.len()));
                if chat_id.0 < 0 {
                    let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                    let dn = if bot_display_name_for_log.is_empty() { None } else { Some(bot_display_name_for_log.clone()) };
                    append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                        ts: now_ts.clone(),
                        bot: bot_username_for_log.clone(),
                        bot_display_name: dn.clone(),
                        role: "user".to_string(),
                        from: Some(format!("bot:{}", from_bot_for_log)),
                        text: prompt_owned,
                        clear: false,
                    });
                    if !raw_entries.is_empty() {
                        msg_debug(&format!("[botmsg_poll:{}] JSONL stopped: writing user+assistant entries, raw_entries_count={}", bmsg_id_for_log, raw_entries.len()));
                        append_group_chat_log(chat_id.0, &GroupChatLogEntry {
                            ts: now_ts,
                            bot: bot_username_for_log.clone(),
                            bot_display_name: dn,
                            role: "assistant".to_string(),
                            from: None,
                            text: serialize_payload(&std::mem::take(&mut raw_entries)),
                            clear: false,
                        });
                    } else {
                        msg_debug(&format!("[botmsg_poll:{}] JSONL stopped: user entry written, assistant SKIPPED (raw_entries is empty)", bmsg_id_for_log));
                    }
                }
            } else {
                msg_debug(&format!("[botmsg_poll:{}] cancel: no session found for chat_id={}", bmsg_id_for_log, chat_id.0));
            }
            data.cancel_tokens.remove(&chat_id);
            let stop_msg_id = data.stop_message_ids.remove(&chat_id);
            drop(data);
            if let Some(msg_id) = stop_msg_id {
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
            }
            msg_debug(&format!("[botmsg_poll:{}] cancel cleanup done", bmsg_id_for_log));
            msg_debug(&format!("[queue:trigger] chat_id={}, source=botmsg_poll_cancelled", chat_id.0));
            drop(_group_lock); // release group chat lock before processing queue
            process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
            return;
        }

        // Clean up cancel token
        msg_debug(&format!("[botmsg_poll:{}] normal completion, cleaning up", bmsg_id_for_log));
        let orphan_stop_msg = {
            let mut data = state_owned.lock().await;
            let msg_id = data.stop_message_ids.remove(&chat_id);
            data.cancel_tokens.remove(&chat_id);
            msg_debug(&format!("[botmsg_poll:{}] cleaned up: orphan_stop_msg={:?}", bmsg_id_for_log, msg_id));
            msg_id
        };
        if let Some(msg_id) = orphan_stop_msg {
            msg_debug(&format!("[botmsg_poll:{}] deleting orphan stop message: {}", bmsg_id_for_log, msg_id));
            shared_rate_limit_wait(&state_owned, chat_id).await;
            let _ = tg!("delete_message", bot_owned.delete_message(chat_id, msg_id).await);
        }
        msg_debug(&format!("[botmsg_poll:{}] END", bmsg_id_for_log));
        msg_debug(&format!("[queue:trigger] chat_id={}, source=botmsg_poll_completed", chat_id.0));
        drop(_group_lock); // release group chat lock before processing queue
        process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
    });
    msg_debug(&format!("[process_bot_message] END (tasks spawned) id={}", msg.id));
}

/// Scheduler loop: runs every 60 seconds, checks for due schedules
async fn scheduler_loop(bot: Bot, state: SharedState, token: String, bot_username: String, bot_display_name: String) {
    let bot_key = token_hash(&token);
    sched_debug("[scheduler_loop] started");

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        // Scan schedule directory
        let entries = list_schedule_entries(&bot_key, None);

        if !entries.is_empty() {
        sched_debug(&format!("[scheduler_loop] cycle: {} entries found", entries.len()));
        }

        for entry in &entries {
            let chat_id = ChatId(entry.chat_id);

            // Verify current_path exists (before acquiring lock — involves filesystem I/O)
            if !Path::new(&entry.current_path).is_dir() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ⚠ [Scheduler] Path not found: {} (schedule: {})", entry.current_path, entry.id);
                sched_debug(&format!("[scheduler_loop] id={}, path not found: {} → skip", entry.id, entry.current_path));
                shared_rate_limit_wait(&state, chat_id).await;
                let msg = format!("⏰ {}\n\n⚠️ Skipped — path no longer exists\n📂 <code>{}</code>",
                    html_escape(&truncate_str(&entry.prompt, 40)), html_escape(&entry.current_path));
                let _ = tg!("send_message", bot.send_message(chat_id, msg).parse_mode(ParseMode::Html).await);
                continue;
            }

            // Single atomic lock: pending check + trigger check + busy check + session backup
            // All checks in one lock to prevent race between pending cleanup and re-trigger
            enum SchedAction {
                Skip,
                DiscardExpired,
                Execute(Option<ChatSession>),
            }

            let action = {
                let mut data = state.lock().await;
                let is_already_pending = data.pending_schedules.get(&chat_id)
                    .map_or(false, |set| set.contains(&entry.id));

                sched_debug(&format!("[scheduler_loop] id={}, is_already_pending={}", entry.id, is_already_pending));

                // If not pending and not due to trigger, skip
                if !is_already_pending && !should_trigger(entry) {
                    // Check if expired absolute schedule should be discarded
                    if entry.schedule_type == "absolute" {
                        if let Ok(schedule_time) = chrono::NaiveDateTime::parse_from_str(&entry.schedule, "%Y-%m-%d %H:%M:%S") {
                            if let Some(schedule_dt) = schedule_time.and_local_timezone(chrono::Local).single() {
                                if chrono::Local::now() > schedule_dt {
                                    sched_debug(&format!("[scheduler_loop] id={}, expired absolute → discard", entry.id));
                                    SchedAction::DiscardExpired
                                } else {
                                    sched_debug(&format!("[scheduler_loop] id={}, not yet due → skip", entry.id));
                                    SchedAction::Skip
                                }
                            } else {
                                SchedAction::Skip
                            }
                        } else {
                            SchedAction::Skip
                        }
                    } else {
                        SchedAction::Skip
                    }
                } else {
                    // Entry should execute — check if chat is busy
                    let is_busy = data.cancel_tokens.contains_key(&chat_id);
                    sched_debug(&format!("[scheduler_loop] id={}, should execute, is_busy={}", entry.id, is_busy));

                    if is_busy {
                        // Chat is busy — mark as pending if not already, retry next cycle
                        // Do NOT touch sessions — leave them as-is
                        if !is_already_pending {
                            data.pending_schedules.entry(chat_id).or_default().insert(entry.id.clone());
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!("  [{ts}] ⏰ [Scheduler] Chat busy, pending: {}", entry.id);
                            sched_debug(&format!("[scheduler_loop] id={}, chat busy → marked pending", entry.id));
                        } else {
                            sched_debug(&format!("[scheduler_loop] id={}, chat busy, already pending → skip", entry.id));
                        }
                        SchedAction::Skip
                    } else {
                        // Not busy — backup session, replace with schedule-specific session, and execute
                        let prev = data.sessions.get(&chat_id).cloned();
                        sched_debug(&format!("[scheduler_loop] id={}, not busy → execute (has_prev_session={})", entry.id, prev.is_some()));
                        data.sessions.insert(chat_id, ChatSession {
                            session_id: None,
                            current_path: Some(entry.current_path.clone()),
                            history: Vec::new(),
                            pending_uploads: Vec::new(),
                        });
                        data.pending_schedules.entry(chat_id).or_default().insert(entry.id.clone());
                        // Pre-insert cancel_token to prevent race with incoming user messages
                        let cancel_token = Arc::new(CancelToken::new());
                        data.cancel_tokens.insert(chat_id, cancel_token);
                        SchedAction::Execute(prev)
                    }
                }
            };

            match action {
                SchedAction::Skip => continue,
                SchedAction::DiscardExpired => {
                    delete_schedule_entry(&entry.id);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ⏰ [Scheduler] Discarded expired once-schedule: {}", entry.id);
                    sched_debug(&format!("[scheduler_loop] id={}, discarded expired", entry.id));
                    continue;
                }
                SchedAction::Execute(prev_session) => {
                    sched_debug(&format!("[scheduler_loop] id={}, calling execute_schedule", entry.id));
                    execute_schedule(&bot, chat_id, entry, &state, &token, prev_session).await;
                }
            }
        }

        // === Bot-to-bot message polling ===
        if !bot_username.is_empty() {
            let messages = scan_messages(&bot_username);
            if !messages.is_empty() {
                msg_debug(&format!("[scheduler_loop] bot messages found: {} for @{}", messages.len(), bot_username));
            }
            for msg in &messages {
                msg_debug(&format!("[scheduler_loop] bot message: id={}, from={}, to={}, chat_id={}, content_len={}, created_at={}",
                    msg.id, msg.from, msg.to, msg.chat_id, msg.content.len(), msg.created_at));
                let chat_id_num: i64 = match msg.chat_id.parse() {
                    Ok(n) => n,
                    Err(e) => {
                        msg_debug(&format!("[scheduler_loop] invalid chat_id in message: id={}, chat_id={:?}, error={}", msg.id, msg.chat_id, e));
                        let remove_result = fs::remove_file(&msg.file_path);
                        msg_debug(&format!("[scheduler_loop] removed invalid message file: ok={}", remove_result.is_ok()));
                        continue;
                    }
                };
                let chat_id = ChatId(chat_id_num);

                // Busy check: skip if cancel_token exists for this chat
                {
                    let data = state.lock().await;
                    let is_busy = data.cancel_tokens.contains_key(&chat_id);
                    msg_debug(&format!("[scheduler_loop] busy check for chat {}: is_busy={}", chat_id_num, is_busy));
                    if is_busy {
                        msg_debug(&format!("[scheduler_loop] chat {} busy, skipping message: {} (will retry next cycle)", chat_id_num, msg.id));
                        continue;
                    }
                }

                // Delete message file immediately (mark as processed)
                let remove_result = fs::remove_file(&msg.file_path);
                msg_debug(&format!("[scheduler_loop] deleted message file: id={}, path={}, ok={}",
                    msg.id, msg.file_path.display(), remove_result.is_ok()));
                msg_debug(&format!("[scheduler_loop] processing bot message: {} (from={}, to={}, chat_id={})", msg.id, msg.from, msg.to, chat_id_num));

                // Process the message
                process_bot_message(&bot, chat_id, msg, &state, &token, &bot_username, &bot_display_name).await;
                msg_debug(&format!("[scheduler_loop] process_bot_message returned for msg: {}", msg.id));
            }

            // Check for timed-out sent messages
            msg_debug("[scheduler_loop] checking message timeouts");
            check_message_timeouts(&bot, &bot_username, &state).await;
        }
    }
}
