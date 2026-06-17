use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use teloxide::prelude::*;
use teloxide::types::{ParseMode, UpdateKind};
use tokio::sync::Mutex;

use crate::services::agy;
use crate::services::claude::{self, CancelToken, StreamMessage, DEFAULT_ALLOWED_TOOLS};
use crate::services::codex;
use crate::services::opencode;
use crate::ui::ai_screen::{self, HistoryItem, HistoryType, SessionData};

/// Global debug log flag for Telegram API calls
static TG_DEBUG: AtomicBool = AtomicBool::new(false);

/// In-process registry of bot tokens used solely for redaction in debug logs.
/// reqwest / teloxide errors can include the request URL on some failure
/// kinds, and that URL embeds `bot<TOKEN>` in plaintext — without redaction
/// the daily debug log would persist the token. Tokens are appended on
/// `run_bot` startup; the list is only ever read for redaction.
static TG_BOT_TOKENS: std::sync::OnceLock<std::sync::RwLock<Vec<String>>> =
    std::sync::OnceLock::new();
static TRANSCRIPTOR_BINARY_MUTEX: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();

fn register_token_for_redaction(token: &str) {
    let lock = TG_BOT_TOKENS.get_or_init(|| std::sync::RwLock::new(Vec::new()));
    if let Ok(mut guard) = lock.write() {
        if !guard.iter().any(|t| t == token) {
            guard.push(token.to_string());
        }
    }
}

fn redact_known_tokens(s: &str) -> String {
    let Some(lock) = TG_BOT_TOKENS.get() else {
        return s.to_string();
    };
    let Ok(guard) = lock.read() else {
        return s.to_string();
    };
    if guard.is_empty() {
        return s.to_string();
    }
    let mut out = s.to_string();
    for t in guard.iter() {
        if !t.is_empty() && out.contains(t.as_str()) {
            out = out.replace(t.as_str(), "<bot_token_redacted>");
        }
    }
    out
}

/// Render any `Display` value with registered bot tokens stripped. Use
/// wherever a `teloxide::RequestError` / `reqwest::Error` ends up in
/// `println!` / `eprintln!` / a user-facing Telegram message — both can
/// include the `bot<TOKEN>` request URL in their `Display` impl.
fn redact_err(e: &impl std::fmt::Display) -> String {
    redact_known_tokens(&e.to_string())
}

fn any_saved_bot_debug_enabled() -> bool {
    let Some(path) = bot_settings_path() else {
        return false;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    let Some(obj) = json.as_object() else {
        return false;
    };
    obj.values().any(|entry| {
        entry
            .get("debug")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    })
}

fn refresh_global_debug_flags() -> bool {
    let enabled_by_env = std::env::var("COKACDIR_DEBUG")
        .map(|v| v == "1")
        .unwrap_or(false);
    let enabled = enabled_by_env || any_saved_bot_debug_enabled();
    TG_DEBUG.store(enabled, Ordering::Relaxed);
    crate::services::claude::DEBUG_ENABLED.store(enabled, Ordering::Relaxed);
    enabled
}

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
        // Redact any bot token that may appear inside the error string before
        // it is persisted to disk. teloxide / reqwest can include the request
        // URL (`/bot<TOKEN>/...`) in some error kinds.
        Err(e) => redact_known_tokens(&format!("✗ {e}")),
    };
    let line = format!("[{ts}] {name}: {status}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

/// Wrap a Telegram API call to log its result in debug mode.
///
/// Two forms:
/// - `tg!(name, future_result)` — log only.
/// - `tg!(name, state, chat_id, future_result)` — log + honor any
///   server-mandated `RetryAfter` by pushing `api_timestamps[chat_id]`
///   forward via `honor_telegram_retry_after`. Use this form for calls in
///   high-frequency polling loops (e.g. spinner edits) so a single Flood
///   Control hit propagates to subsequent `shared_rate_limit_wait` calls
///   instead of being ignored.
macro_rules! tg {
    ($name:expr, $fut:expr) => {{
        let r = $fut;
        tg_debug($name, &r);
        r
    }};
    ($name:expr, $state:expr, $chat:expr, $fut:expr) => {{
        let r = $fut;
        tg_debug($name, &r);
        honor_telegram_retry_after($state, $chat, &r).await;
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
                entries.push(RawPayloadEntry {
                    tag,
                    content: std::mem::take(&mut cur_content),
                });
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
        entries.push(RawPayloadEntry {
            tag,
            content: cur_content,
        });
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
    if fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let ts = chrono::Local::now().format("%Y%m%d%H%M%S");
    use rand::Rng;
    let rand_suffix: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(6)
        .map(|b| (b as char).to_ascii_lowercase())
        .collect();
    let filename = format!("{}_{}.txt", ts, rand_suffix);
    let path = dir.join(&filename);
    if fs::write(&path, content).is_err() {
        return None;
    }
    Some(path.display().to_string())
}

/// Truncate payload entries whose content exceeds the limit.
/// Long content is saved to ~/.cokacdir/values/ and replaced with a truncated preview + file path.
fn truncate_payload_entries(entries: &[RawPayloadEntry]) -> Vec<RawPayloadEntry> {
    entries
        .iter()
        .map(|entry| {
            if entry.content.chars().count() <= PAYLOAD_TRUNCATE_LIMIT {
                entry.clone()
            } else {
                let preview: String = entry.content.chars().take(PAYLOAD_TRUNCATE_LIMIT).collect();
                let truncated_content = if let Some(path) = save_payload_value(&entry.content) {
                    format!("{}...\n(truncated, full content: {})", preview, path)
                } else {
                    format!("{}...\n(truncated, failed to save full content)", preview)
                };
                RawPayloadEntry {
                    tag: entry.tag.clone(),
                    content: truncated_content,
                }
            }
        })
        .collect()
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
    if chat_id >= 0 {
        return None;
    }
    let dir = dirs::home_dir()?.join(".cokacdir").join("chat_locks");
    let _ = std::fs::create_dir_all(&dir);
    let lock_path = dir.join(format!("{}.lock", chat_id));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .ok()?;
    let mut waited = false;
    loop {
        match file.try_lock_exclusive() {
            Ok(_) => {
                if waited {
                    msg_debug(&format!(
                        "[chat_lock] chat_id={} lock acquired after waiting",
                        chat_id
                    ));
                } else {
                    msg_debug(&format!(
                        "[chat_lock] chat_id={} lock acquired immediately",
                        chat_id
                    ));
                }
                return Some(GroupChatLock { _file: file });
            }
            Err(_) => {
                if !waited {
                    msg_debug(&format!(
                        "[chat_lock] chat_id={} lock contended, waiting...",
                        chat_id
                    ));
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
///
/// All failure paths trace via `msg_debug` so silent log loss is at least
/// diagnosable when /debug is enabled. Entries dropped here are not retried.
fn append_group_chat_log(chat_id: i64, entry: &GroupChatLogEntry) {
    use fs2::FileExt;

    let Some(path) = group_chat_log_path(chat_id) else {
        msg_debug(&format!(
            "[append_group_chat_log] LOST: no log path resolvable for chat_id={}",
            chat_id
        ));
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            msg_debug(&format!(
                "[append_group_chat_log] LOST: create_dir_all({:?}) failed for chat_id={}: {}",
                parent, chat_id, e
            ));
            return;
        }
    }

    // Serialize before acquiring lock to minimize lock hold time
    let json = match serde_json::to_string(entry) {
        Ok(j) => j,
        Err(e) => {
            msg_debug(&format!(
                "[append_group_chat_log] LOST: serialize failed for chat_id={}: {}",
                chat_id, e
            ));
            return;
        }
    };

    // Skip entries containing --read_chat_log to prevent recursive snowball growth:
    // each --read_chat_log result embeds the entire log, causing exponential JSONL inflation.
    if json.contains("--read_chat_log") {
        msg_debug(&format!(
            "[append_group_chat_log] SKIPPED: line contains --read_chat_log (len={})",
            json.len()
        ));
        return;
    }

    let line = format!("{}\n", json);

    // Lock via a dedicated lock file (not the data file itself)
    // On Windows, LockFileEx is mandatory and can conflict with WriteFile on the same handle
    let lock_path = path.with_extension("jsonl.lock");
    let lock_file = match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            msg_debug(&format!(
                "[append_group_chat_log] LOST: open lock_file({:?}) failed for chat_id={}: {}",
                lock_path, chat_id, e
            ));
            return;
        }
    };
    if let Err(e) = lock_file.lock_exclusive() {
        msg_debug(&format!(
            "[append_group_chat_log] LOST: lock_exclusive failed for chat_id={}: {}",
            chat_id, e
        ));
        return;
    }

    // Write to data file without locking it
    match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&path)
    {
        Ok(mut data_file) => {
            use std::io::Write;
            if let Err(e) = data_file.write_all(line.as_bytes()) {
                msg_debug(&format!("[append_group_chat_log] LOST: write_all failed for chat_id={} (entry len={}): {}", chat_id, line.len(), e));
            }
            if let Err(e) = data_file.sync_data() {
                msg_debug(&format!("[append_group_chat_log] sync_data failed for chat_id={} (entry written but not durable): {}", chat_id, e));
            }
        }
        Err(e) => {
            msg_debug(&format!(
                "[append_group_chat_log] LOST: open data file({:?}) failed for chat_id={}: {}",
                path, chat_id, e
            ));
        }
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
    let filter_bot_owned: Option<String> =
        filter_bot.map(|b| b.strip_prefix('@').unwrap_or(b).to_lowercase());
    let filter_bot = filter_bot_owned.as_deref();

    let Some(path) = group_chat_log_path(chat_id) else {
        return Vec::new();
    };

    // Use the same dedicated lock file as append_group_chat_log for coordination
    let lock_path = path.with_extension("jsonl.lock");
    let Ok(lock_file) = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
    else {
        return Vec::new();
    };
    if fs2::FileExt::lock_shared(&lock_file).is_err() {
        return Vec::new();
    }

    let Ok(file) = fs::File::open(&path) else {
        let _ = fs2::FileExt::unlock(&lock_file);
        return Vec::new();
    };

    let reader = std::io::BufReader::new(&file);
    use std::io::BufRead;

    // First pass: collect all entries and find the last clear marker per bot.
    // Track corrupt lines so a sudden surge in malformed entries is at least
    // visible via /debug instead of silently disappearing.
    let mut dropped_io: usize = 0;
    let mut dropped_parse: usize = 0;
    let all_entries: Vec<(usize, GroupChatLogEntry)> = reader
        .lines()
        .enumerate()
        .filter_map(|(i, line_result)| {
            let line_num = i + 1; // 1-based
            let line = match line_result {
                Ok(l) => l,
                Err(_) => {
                    dropped_io += 1;
                    return None;
                }
            };
            match serde_json::from_str::<GroupChatLogEntry>(&line) {
                Ok(entry) => Some((line_num, entry)),
                Err(_) => {
                    dropped_parse += 1;
                    None
                }
            }
        })
        .collect();
    if dropped_io > 0 || dropped_parse > 0 {
        msg_debug(&format!(
            "[read_group_chat_log_range] chat_id={}: dropped {} io-error line(s), {} unparseable line(s)",
            chat_id, dropped_io, dropped_parse
        ));
    }

    // Build a map of bot -> last clear marker line number
    let mut last_clear: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (line_num, entry) in &all_entries {
        if entry.clear {
            last_clear.insert(entry.bot.clone(), *line_num);
        }
    }

    // Second pass: filter entries, skipping those before the clear marker for each bot
    let entries: Vec<(usize, GroupChatLogEntry)> = all_entries
        .into_iter()
        .filter(|(line_num, entry)| {
            // Skip clear marker entries themselves
            if entry.clear {
                return false;
            }
            // Skip entries from a bot that are before its last clear marker
            if let Some(&clear_line) = last_clear.get(&entry.bot) {
                if *line_num <= clear_line {
                    return false;
                }
            }
            let in_range =
                *line_num >= range_start && range_end.map_or(true, |end| *line_num <= end);
            let bot_match = filter_bot.map_or(true, |bot| entry.bot == bot);
            in_range && bot_match
        })
        .collect();

    let _ = fs2::FileExt::unlock(&lock_file);
    entries
}

/// Tail-N variant of `read_group_chat_log_range`: returns at most `n` of the
/// most recent entries (after applying clear-marker suppression and
/// `filter_bot`). Streams the file in two passes with O(n + bot_count)
/// memory instead of loading every entry — important for the system-prompt
/// hot path where the log can grow into the MB-range while the caller only
/// wants the last 12 entries.
pub fn read_group_chat_log_tail(
    chat_id: i64,
    n: usize,
    filter_bot: Option<&str>,
) -> Vec<(usize, GroupChatLogEntry)> {
    use std::collections::VecDeque;
    use std::io::BufRead;

    if n == 0 {
        return Vec::new();
    }

    let filter_bot_owned: Option<String> =
        filter_bot.map(|b| b.strip_prefix('@').unwrap_or(b).to_lowercase());
    let filter_bot = filter_bot_owned.as_deref();

    let Some(path) = group_chat_log_path(chat_id) else {
        return Vec::new();
    };

    let lock_path = path.with_extension("jsonl.lock");
    let Ok(lock_file) = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
    else {
        return Vec::new();
    };
    if fs2::FileExt::lock_shared(&lock_file).is_err() {
        return Vec::new();
    }

    let mut dropped_io: usize = 0;
    let mut dropped_parse: usize = 0;

    // Pass 1: scan for clear markers (small per-bot map). We need to know
    // every marker before deciding which entries in the tail are suppressed.
    let mut last_clear: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    {
        let Ok(file) = fs::File::open(&path) else {
            let _ = fs2::FileExt::unlock(&lock_file);
            return Vec::new();
        };
        for (i, line_result) in std::io::BufReader::new(&file).lines().enumerate() {
            let line_num = i + 1;
            let line = match line_result {
                Ok(l) => l,
                Err(_) => {
                    dropped_io += 1;
                    continue;
                }
            };
            match serde_json::from_str::<GroupChatLogEntry>(&line) {
                Ok(entry) => {
                    if entry.clear {
                        last_clear.insert(entry.bot.clone(), line_num);
                    }
                }
                Err(_) => {
                    dropped_parse += 1;
                }
            }
        }
    }

    // Pass 2: stream forward, keep a sliding window of the last `n` entries
    // that pass the marker and bot filters.
    let mut tail: VecDeque<(usize, GroupChatLogEntry)> = VecDeque::with_capacity(n + 1);
    {
        let Ok(file) = fs::File::open(&path) else {
            let _ = fs2::FileExt::unlock(&lock_file);
            return Vec::new();
        };
        for (i, line_result) in std::io::BufReader::new(&file).lines().enumerate() {
            let line_num = i + 1;
            // Pass 1 already counted any io / parse errors on this same file
            // (locked shared, deterministic). Re-counting here would
            // double-report the same corrupt lines.
            let Ok(line) = line_result else {
                continue;
            };
            let Ok(entry) = serde_json::from_str::<GroupChatLogEntry>(&line) else {
                continue;
            };
            if entry.clear {
                continue;
            }
            if let Some(&clear_line) = last_clear.get(&entry.bot) {
                if line_num <= clear_line {
                    continue;
                }
            }
            if let Some(b) = filter_bot {
                if entry.bot != b {
                    continue;
                }
            }
            tail.push_back((line_num, entry));
            if tail.len() > n {
                tail.pop_front();
            }
        }
    }

    if dropped_io > 0 || dropped_parse > 0 {
        msg_debug(&format!(
            "[read_group_chat_log_tail] chat_id={}: dropped {} io-error line(s), {} unparseable line(s)",
            chat_id, dropped_io, dropped_parse
        ));
    }

    let _ = fs2::FileExt::unlock(&lock_file);
    tail.into_iter().collect()
}

/// Collect all bots that have ever appeared in a group chat log, with their
/// most recent display name.  Scans the entire file including entries before
/// clear markers (and the markers themselves) so that bots whose history was
/// cleared are still listed.
///
/// Returns a Vec of (bot_username, Option<display_name>) sorted by username.
pub fn collect_group_chat_bots(chat_id: i64) -> Vec<(String, Option<String>)> {
    use std::io::BufRead;

    let Some(path) = group_chat_log_path(chat_id) else {
        return Vec::new();
    };

    let lock_path = path.with_extension("jsonl.lock");
    let Ok(lock_file) = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
    else {
        return Vec::new();
    };
    if fs2::FileExt::lock_shared(&lock_file).is_err() {
        return Vec::new();
    }

    let Ok(file) = fs::File::open(&path) else {
        let _ = fs2::FileExt::unlock(&lock_file);
        return Vec::new();
    };

    // Scan forward — later entries overwrite earlier ones, keeping the latest display name
    let mut bots: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    for line in std::io::BufReader::new(&file).lines().flatten() {
        if let Ok(entry) = serde_json::from_str::<GroupChatLogEntry>(&line) {
            if !entry.bot.is_empty() {
                bots.insert(entry.bot.clone(), entry.bot_display_name);
            }
        }
    }

    let _ = fs2::FileExt::unlock(&lock_file);

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
    /// Wall-clock instant of the most recent push to `pending_uploads`.
    /// Used to expire stale `;`-prefixed broadcast uploads in group chats
    /// when no addressed message arrives within `PENDING_UPLOAD_STALE_TTL`.
    /// Invariant: `Some` iff `pending_uploads` is non-empty (maintained by
    /// helper methods on this struct).
    pending_uploads_added_at: Option<std::time::Instant>,
}

/// Maximum age for `pending_uploads` before they are considered stale and
/// dropped on the next unrelated message. 5 minutes balances "user finished
/// their photo burst and is now typing the addressed message" against
/// "user moved on hours ago, don't surprise them by re-attaching old data".
const PENDING_UPLOAD_STALE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

impl ChatSession {
    /// Push a new upload record and refresh the freshness timestamp.
    fn push_pending_upload(&mut self, record: String) {
        self.pending_uploads.push(record);
        self.pending_uploads_added_at = Some(std::time::Instant::now());
    }

    /// Append upload records (e.g. restoring queued uploads on dequeue) and
    /// refresh the freshness timestamp if any were added.
    fn extend_pending_uploads<I: IntoIterator<Item = String>>(&mut self, iter: I) {
        let before = self.pending_uploads.len();
        self.pending_uploads.extend(iter);
        if self.pending_uploads.len() > before {
            self.pending_uploads_added_at = Some(std::time::Instant::now());
        }
    }

    /// Drain pending uploads and reset the freshness timestamp.
    fn take_pending_uploads(&mut self) -> Vec<String> {
        self.pending_uploads_added_at = None;
        std::mem::take(&mut self.pending_uploads)
    }

    /// Empty pending uploads and reset the freshness timestamp.
    fn clear_pending_uploads(&mut self) {
        self.pending_uploads.clear();
        self.pending_uploads_added_at = None;
    }

    fn remove_pending_upload_records(&mut self, records: &[String]) -> usize {
        if records.is_empty() {
            return 0;
        }
        let mut remaining: HashMap<&str, usize> = HashMap::new();
        for record in records {
            *remaining.entry(record.as_str()).or_insert(0) += 1;
        }

        let before = self.pending_uploads.len();
        self.pending_uploads.retain(|record| {
            if let Some(count) = remaining.get_mut(record.as_str()) {
                if *count > 0 {
                    *count -= 1;
                    return false;
                }
            }
            true
        });

        let removed = before - self.pending_uploads.len();
        if self.pending_uploads.is_empty() {
            self.pending_uploads_added_at = None;
        } else if self.pending_uploads_added_at.is_none() {
            self.pending_uploads_added_at = Some(std::time::Instant::now());
        }
        removed
    }

    fn remove_user_history_records(&mut self, records: &[String]) -> usize {
        if records.is_empty() {
            return 0;
        }
        let mut remaining: HashMap<&str, usize> = HashMap::new();
        for record in records {
            *remaining.entry(record.as_str()).or_insert(0) += 1;
        }

        let mut removed = 0usize;
        let mut idx = self.history.len();
        while idx > 0 {
            idx -= 1;
            let should_remove = self.history[idx].item_type == HistoryType::User
                && remaining
                    .get_mut(self.history[idx].content.as_str())
                    .map(|count| {
                        if *count > 0 {
                            *count -= 1;
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
            if should_remove {
                self.history.remove(idx);
                removed += 1;
            }
        }
        removed
    }
}

#[cfg(test)]
mod pending_upload_tests {
    use super::{ChatSession, HistoryItem, HistoryType};

    fn session_with_records(records: &[&str]) -> ChatSession {
        let mut session = ChatSession {
            session_id: None,
            current_path: Some("/tmp/cokacdir-test".to_string()),
            history: Vec::new(),
            pending_uploads: Vec::new(),
            pending_uploads_added_at: None,
        };
        for record in records {
            session.push_pending_upload((*record).to_string());
            session.history.push(HistoryItem {
                item_type: HistoryType::User,
                content: (*record).to_string(),
            });
        }
        session
    }

    #[test]
    fn removes_only_targeted_pending_upload_and_history_records() {
        let mut session = session_with_records(&["before", "album-a", "album-b", "after"]);
        session.history.push(HistoryItem {
            item_type: HistoryType::Assistant,
            content: "album-a".to_string(),
        });

        let records = vec!["album-a".to_string(), "album-b".to_string()];
        assert_eq!(session.remove_pending_upload_records(&records), 2);
        assert_eq!(session.pending_uploads, vec!["before", "after"]);
        assert!(session.pending_uploads_added_at.is_some());

        assert_eq!(session.remove_user_history_records(&records), 2);
        let contents: Vec<_> = session
            .history
            .iter()
            .map(|item| item.content.as_str())
            .collect();
        assert_eq!(contents, vec!["before", "after", "album-a"]);
        assert_eq!(
            session.history.last().unwrap().item_type,
            HistoryType::Assistant
        );
    }

    #[test]
    fn clearing_last_targeted_pending_upload_resets_timestamp() {
        let mut session = session_with_records(&["album-a"]);
        let records = vec!["album-a".to_string()];

        assert_eq!(session.remove_pending_upload_records(&records), 1);

        assert!(session.pending_uploads.is_empty());
        assert!(session.pending_uploads_added_at.is_none());
    }
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
    /// Legacy startup greeting preference retained for older settings files.
    greeting: bool,
    /// chat_id (string) → true if --chrome flag should be passed to Claude CLI
    use_chrome: HashMap<String, bool>,
    /// chat_id (string) → message to send when AI processing completes
    end_hook: HashMap<String, String>,
    /// chat_id (string) -> codex reasoning effort.
    /// Applied only when current model is codex; passed via `-c model_reasoning_effort=<v>`.
    effort: HashMap<String, String>,
    /// chat_id (string) -> true if Codex should run with `-c service_tier="fast"`.
    codex_fast: HashMap<String, bool>,
    /// chat_id (string) -> Claude effort level.
    /// Applied only when current model is Claude; passed via `--effort <v>`.
    claude_effort: HashMap<String, String>,
    /// chat_id (string) -> transcriptor model setting for STT.
    /// Bare values are passed as `--model-name`; `path:<PATH>` is passed as `--model`.
    stt_models: HashMap<String, String>,
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
            effort: HashMap::new(),
            codex_fast: HashMap::new(),
            claude_effort: HashMap::new(),
            stt_models: HashMap::new(),
        }
    }
}

/// Get allowed tools for a specific chat_id.
/// Returns the chat-specific list if configured, otherwise DEFAULT_ALLOWED_TOOLS.
fn get_allowed_tools(settings: &BotSettings, chat_id: ChatId) -> Vec<String> {
    let key = chat_id.0.to_string();
    settings
        .allowed_tools
        .get(&key)
        .cloned()
        .unwrap_or_else(|| {
            DEFAULT_ALLOWED_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
}

/// Get the configured model for a specific chat_id, if any.
/// Migrates legacy bare names (e.g. "sonnet") and deprecated Gemini settings.
fn get_model(settings: &BotSettings, chat_id: ChatId) -> Option<String> {
    let key = chat_id.0.to_string();
    settings.models.get(&key).map(|m| match m.as_str() {
        "sonnet" | "opus" | "haiku" | "sonnet[1m]" | "opus[1m]" | "haiku[1m]" => {
            format!("claude:{}", m)
        }
        "gemini" => "agy".to_string(),
        s if s.starts_with("gemini:") => {
            let model = s.trim_start_matches("gemini:");
            if agy::is_valid_agy_model(model) {
                s.to_string()
            } else {
                "agy".to_string()
            }
        }
        _ => m.clone(),
    })
}

/// Check if silent mode is enabled for a chat (default: ON)
fn is_silent(settings: &BotSettings, chat_id: ChatId) -> bool {
    settings
        .silent
        .get(&chat_id.0.to_string())
        .copied()
        .unwrap_or(SILENT_MODE_DEFAULT)
}

/// Get the configured Codex reasoning effort for a specific chat_id.
/// Returns None when unset, so the Codex CLI / config.toml default applies.
fn get_codex_effort(settings: &BotSettings, chat_id: ChatId) -> Option<String> {
    settings.effort.get(&chat_id.0.to_string()).cloned()
}

/// Get the configured Claude effort for a specific chat_id.
/// Returns None when unset, so Claude Code's effortLevel/default applies.
fn get_claude_effort(settings: &BotSettings, chat_id: ChatId) -> Option<String> {
    settings.claude_effort.get(&chat_id.0.to_string()).cloned()
}

fn get_effort_for_provider(
    settings: &BotSettings,
    chat_id: ChatId,
    provider: &str,
) -> Option<String> {
    match provider {
        "claude" => get_claude_effort(settings, chat_id),
        "codex" => get_codex_effort(settings, chat_id),
        _ => None,
    }
}

fn is_codex_fast(settings: &BotSettings, chat_id: ChatId) -> bool {
    settings
        .codex_fast
        .get(&chat_id.0.to_string())
        .copied()
        .unwrap_or(false)
}

fn get_stt_model(settings: &BotSettings, chat_id: ChatId) -> Option<String> {
    settings.stt_models.get(&chat_id.0.to_string()).cloned()
}

fn is_valid_stt_model_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn normalize_stt_model_setting(arg: &str) -> Result<Option<String>, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err(
            "Usage: /stt_model <model>, /stt_model path:<model_path>, or /stt_model reset"
                .to_string(),
        );
    }
    let lower = arg.to_ascii_lowercase();
    if matches!(lower.as_str(), "reset" | "clear" | "default" | "unset") {
        return Ok(None);
    }

    let clean = arg.split(" \u{2014} ").next().unwrap_or(arg).trim();
    if let Some(path) = clean.strip_prefix("path:") {
        let path = path.trim();
        if path.is_empty() {
            return Err(
                "Model path is empty. Usage: /stt_model path:/absolute/model.bin".to_string(),
            );
        }
        return Ok(Some(format!("path:{path}")));
    }

    if !is_valid_stt_model_name(clean) {
        return Err("Invalid STT model name. Use names like tiny, base, small, medium, large-v3, large-v3-turbo, or path:<model_path>.".to_string());
    }
    Ok(Some(clean.to_string()))
}

fn stt_model_setting_display(setting: Option<&str>) -> String {
    match setting {
        Some(value) if value.starts_with("path:") => {
            format!("path:{}", value.trim_start_matches("path:"))
        }
        Some(value) => value.to_string(),
        None => "default (transcriptor env/config/base)".to_string(),
    }
}

fn expand_user_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).display().to_string();
        }
    }
    path.to_string()
}

/// Validate a Codex reasoning_effort value.
fn is_valid_codex_effort(v: &str) -> bool {
    matches!(v, "minimal" | "low" | "medium" | "high" | "xhigh")
}

/// Validate a Claude Code --effort value.
fn is_valid_claude_effort(v: &str) -> bool {
    matches!(v, "low" | "medium" | "high" | "xhigh" | "max")
}

fn valid_effort_values(provider: &str) -> &'static str {
    match provider {
        "claude" => "low, medium, high, xhigh, max",
        "codex" => "minimal, low, medium, high, xhigh",
        _ => "Claude: low, medium, high, xhigh, max; Codex: minimal, low, medium, high, xhigh",
    }
}

fn is_valid_effort_for_provider(provider: &str, v: &str) -> bool {
    match provider {
        "claude" => is_valid_claude_effort(v),
        "codex" => is_valid_codex_effort(v),
        _ => false,
    }
}

/// Schedule entry persisted as JSON in ~/.cokacdir/schedule/
#[derive(Clone)]
struct ScheduleEntry {
    id: String,
    chat_id: i64,
    /// SHA-256 verifier of the owning bot_key, bound to (id, chat_id). The raw
    /// `bot_key` is never stored on disk in the modern format — see
    /// `live_schedule_key_verifier`. Legacy entries written before this change
    /// still carry a plaintext `bot_key` field on disk; `read_schedule_entry`
    /// transparently computes the verifier from those at load time, and the
    /// next `write_schedule_entry` rewrites the file in the new format.
    bot_key_verifier: String,
    current_path: String,
    prompt: String,
    schedule: String,      // original --at value (cron expression or absolute time)
    schedule_type: String, // "absolute" | "cron"
    once: Option<bool>,    // only meaningful for cron (None for absolute)
    last_run: Option<String>, // "2026-02-23 14:00:00"
    created_at: String,
    context_summary: Option<String>, // context summary text for session-isolated schedule
}

/// Verifier hash for a live schedule entry's owning bot_key. Binding to
/// `(schedule_id, chat_id)` ensures a verifier from one schedule cannot be
/// reused to authenticate another. The domain separator distinguishes this
/// from `schedule_history_key_verifier`, so verifiers from the two systems
/// are not interchangeable even if their input keys collide.
fn live_schedule_key_verifier(schedule_id: &str, chat_id: i64, bot_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cokacdir:live_schedule:v1\0");
    hasher.update(schedule_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(chat_id.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(bot_key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Directory for schedule files: ~/.cokacdir/schedule/
fn schedule_dir() -> Option<std::path::PathBuf> {
    let result = dirs::home_dir().map(|h| h.join(".cokacdir").join("schedule"));
    sched_debug(&format!("[schedule_dir] → {:?}", result));
    result
}

/// When `COKAC_SCHEDULE_INLINE=1`, scheduled tasks run inline in the chat's
/// current session (reusing `session_id` and `current_path`) instead of being
/// isolated into `~/.cokacdir/workspace/<schedule_id>` with a fresh session.
/// Any value other than the literal string `"1"` (including unset) is false.
fn is_schedule_inline_enabled() -> bool {
    std::env::var("COKAC_SCHEDULE_INLINE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn sched_debug(msg: &str) {
    // AI provider errors that surface here can carry the bot token via
    // teloxide / reqwest URL renderings. Redact before persisting, matching
    // `msg_debug` and `tg_debug`.
    crate::services::claude::debug_log_to("cron.log", &redact_known_tokens(msg));
}

fn msg_debug(msg: &str) {
    // teloxide / reqwest errors can embed `bot<TOKEN>` URLs in their Display
    // output. Redact before persisting to disk.
    crate::services::claude::debug_log_to("msg.log", &redact_known_tokens(msg));
}

/// Always-on debug log (independent of /debug toggle).
/// Writes to ~/.cokacdir/debug/ai_trace.log for diagnosing AI execution issues.
/// AI provider stream errors can include URLs with the bot token; redact
/// before persisting so this always-on log stays safe to share.
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
            let redacted = redact_known_tokens(msg);
            let _ = std::io::Write::write_fmt(
                &mut file,
                format_args!("[{}] {}\n", timestamp, redacted),
            );
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
    let username = msg
        .from
        .as_ref()
        .and_then(|u| u.username.as_deref())
        .unwrap_or("");
    let name = msg
        .from
        .as_ref()
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
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
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
            msg_debug(&format!(
                "[read_bot_message] read failed: {} (path={})",
                e,
                path.display()
            ));
            return None;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            msg_debug(&format!(
                "[read_bot_message] JSON parse failed: {} (path={})",
                e,
                path.display()
            ));
            return None;
        }
    };
    let id = v.get("id").and_then(|x| x.as_str());
    let from = v.get("from").and_then(|x| x.as_str());
    let to = v.get("to").and_then(|x| x.as_str());
    let chat_id_val = v.get("chat_id").and_then(|x| x.as_str());
    let content_val = v.get("content").and_then(|x| x.as_str());
    let created_at = v.get("created_at").and_then(|x| x.as_str());
    if id.is_none()
        || from.is_none()
        || to.is_none()
        || chat_id_val.is_none()
        || content_val.is_none()
        || created_at.is_none()
    {
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
    msg_debug(&format!(
        "[read_bot_message] ok: id={}, from={}, to={}, chat_id={}",
        msg.id, msg.from, msg.to, msg.chat_id
    ));
    Some(msg)
}

/// Scan messages directory for messages addressed to this bot, sorted by created_at (FIFO)
fn scan_messages(my_username: &str) -> Vec<BotMessage> {
    msg_debug(&format!(
        "[scan_messages] scanning for bot: {}",
        my_username
    ));
    let Some(dir) = messages_dir() else {
        msg_debug("[scan_messages] messages_dir() returned None");
        return Vec::new();
    };
    if !dir.is_dir() {
        msg_debug(&format!("[scan_messages] dir not found: {}", dir.display()));
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        msg_debug(&format!(
            "[scan_messages] read_dir failed: {}",
            dir.display()
        ));
        return Vec::new();
    };
    let all_entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    let json_count = all_entries
        .iter()
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .count();
    msg_debug(&format!(
        "[scan_messages] dir entries={}, json files={}",
        all_entries.len(),
        json_count
    ));
    let mut msgs: Vec<BotMessage> = all_entries
        .into_iter()
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .filter_map(|e| read_bot_message(&e.path()))
        .filter(|m| {
            let matches = m.to.to_lowercase() == my_username.to_lowercase();
            if !matches {
                msg_debug(&format!(
                    "[scan_messages] skip msg id={}, to={} (not for {})",
                    m.id, m.to, my_username
                ));
            }
            matches
        })
        .collect();
    msgs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    msg_debug(&format!(
        "[scan_messages] result: {} messages for {}",
        msgs.len(),
        my_username
    ));
    msgs
}

/// Check for timed-out messages (sent by this bot, still pending after 30 min)
async fn check_message_timeouts(bot: &Bot, my_username: &str, state: &SharedState) {
    msg_debug(&format!(
        "[check_message_timeouts] checking for bot: {}",
        my_username
    ));
    let Some(dir) = messages_dir() else {
        msg_debug("[check_message_timeouts] messages_dir() returned None");
        return;
    };
    if !dir.is_dir() {
        msg_debug(&format!(
            "[check_message_timeouts] dir not found: {}",
            dir.display()
        ));
        return;
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        msg_debug(&format!(
            "[check_message_timeouts] read_dir failed: {}",
            dir.display()
        ));
        return;
    };

    let now = chrono::Local::now();
    let mut scanned = 0u32;
    let mut timed_out = 0u32;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.extension().map(|ext| ext == "json").unwrap_or(false) {
            continue;
        }
        let Some(msg) = read_bot_message(&path) else {
            continue;
        };
        if msg.from.to_lowercase() != my_username.to_lowercase() {
            msg_debug(&format!(
                "[check_message_timeouts] skip msg id={}, from={} (not from {})",
                msg.id, msg.from, my_username
            ));
            continue;
        }
        scanned += 1;

        // Check if created_at is older than 30 minutes
        match chrono::NaiveDateTime::parse_from_str(&msg.created_at, "%Y-%m-%d %H:%M:%S") {
            Ok(created) => {
                if let Some(created_dt) = created.and_local_timezone(chrono::Local).single() {
                    let elapsed = now.signed_duration_since(created_dt);
                    msg_debug(&format!(
                        "[check_message_timeouts] msg id={}, to={}, elapsed={}min",
                        msg.id,
                        msg.to,
                        elapsed.num_minutes()
                    ));
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
                            let send_result =
                                tg!("send_message", bot.send_message(chat_id, notice).await);
                            msg_debug(&format!(
                                "[check_message_timeouts] notified chat_id={}, send_ok={}",
                                cid,
                                send_result.is_ok()
                            ));
                        } else {
                            msg_debug(&format!(
                                "[check_message_timeouts] invalid chat_id in msg: {}",
                                msg.chat_id
                            ));
                        }
                    }
                } else {
                    msg_debug(&format!("[check_message_timeouts] timezone conversion failed for msg id={}, created_at={}", msg.id, msg.created_at));
                }
            }
            Err(e) => {
                msg_debug(&format!(
                    "[check_message_timeouts] time parse failed for msg id={}: {} (created_at={})",
                    msg.id, e, msg.created_at
                ));
            }
        }
    }
    msg_debug(&format!(
        "[check_message_timeouts] done: scanned={}, timed_out={}",
        scanned, timed_out
    ));
}

/// Read a single schedule entry from a JSON file
fn read_schedule_entry(path: &std::path::Path) -> Option<ScheduleEntry> {
    sched_debug(&format!(
        "[read_schedule_entry] reading: {}",
        path.display()
    ));
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
    let id = v.get("id")?.as_str()?.to_string();
    let chat_id = v.get("chat_id")?.as_i64()?;
    // Modern files carry `bot_key_verifier`; pre-migration files still carry a
    // plaintext `bot_key`. Compute the verifier in-memory for legacy entries so
    // the rest of the read path is uniform; the next `write_schedule_entry`
    // (e.g. context update or last_run after a cron fire) will rewrite the
    // file without the plaintext field. We deliberately do NOT rewrite the
    // file from the read path: a concurrent `delete_schedule_entry` could let
    // such a rewrite resurrect an already-removed schedule.
    let bot_key_verifier =
        if let Some(verifier) = v.get("bot_key_verifier").and_then(|x| x.as_str()) {
            verifier.to_string()
        } else if let Some(legacy_key) = v.get("bot_key").and_then(|x| x.as_str()) {
            live_schedule_key_verifier(&id, chat_id, legacy_key)
        } else {
            sched_debug("[read_schedule_entry] missing both bot_key_verifier and bot_key");
            return None;
        };
    let entry = Some(ScheduleEntry {
        id,
        chat_id,
        bot_key_verifier,
        current_path: v.get("current_path")?.as_str()?.to_string(),
        prompt: v.get("prompt")?.as_str()?.to_string(),
        schedule: v.get("schedule")?.as_str()?.to_string(),
        schedule_type: v.get("schedule_type")?.as_str()?.to_string(),
        once: v.get("once").and_then(|v| v.as_bool()),
        last_run: v.get("last_run").and_then(|v| v.as_str()).map(String::from),
        created_at: v.get("created_at")?.as_str()?.to_string(),
        context_summary: v
            .get("context_summary")
            .and_then(|v| v.as_str())
            .map(String::from),
    });
    sched_debug(&format!(
        "[read_schedule_entry] result: id={}, type={}, schedule={}, last_run={:?}",
        entry.as_ref().map(|e| e.id.as_str()).unwrap_or("?"),
        entry
            .as_ref()
            .map(|e| e.schedule_type.as_str())
            .unwrap_or("?"),
        entry.as_ref().map(|e| e.schedule.as_str()).unwrap_or("?"),
        entry.as_ref().and_then(|e| e.last_run.as_deref()),
    ));
    entry
}

/// Write a schedule entry to its JSON file
fn write_schedule_entry(entry: &ScheduleEntry) -> Result<(), String> {
    sched_debug(&format!(
        "[write_schedule_entry] id={}, type={}, schedule={}, once={:?}, last_run={:?}",
        entry.id, entry.schedule_type, entry.schedule, entry.once, entry.last_run
    ));
    // Reject cron expressions the matcher can't interpret so a syntactically
    // wrong --at value fails loudly at register/update time instead of
    // silently never firing. `absolute` schedules carry a wall-clock
    // timestamp, not cron syntax, so they're exempt.
    if entry.schedule_type == "cron" {
        if let Err(e) = validate_cron_expression(&entry.schedule) {
            sched_debug(&format!(
                "[write_schedule_entry] id={}, invalid cron: {}",
                entry.id, e
            ));
            return Err(e);
        }
    }
    let dir = schedule_dir().ok_or("Cannot determine home directory")?;
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create schedule dir: {e}"))?;
    // `bot_key_verifier` replaces the legacy plaintext `bot_key` field.
    // Re-writing a legacy file through this path naturally drops the old
    // plaintext field on the next atomic rename.
    let mut json = serde_json::json!({
        "id": entry.id,
        "chat_id": entry.chat_id,
        "bot_key_verifier": entry.bot_key_verifier,
        "current_path": entry.current_path,
        "prompt": entry.prompt,
        "schedule": entry.schedule,
        "schedule_type": entry.schedule_type,
        "last_run": entry.last_run,
        "created_at": entry.created_at,
        "context_summary": entry.context_summary,
    });
    if let Some(once_val) = entry.once {
        json.as_object_mut()
            .unwrap()
            .insert("once".to_string(), serde_json::json!(once_val));
    }
    let path = dir.join(format!("{}.json", entry.id));
    let tmp_path = dir.join(format!("{}.json.tmp", entry.id));
    sched_debug(&format!(
        "[write_schedule_entry] writing tmp: {}",
        tmp_path.display()
    ));
    fs::write(
        &tmp_path,
        serde_json::to_string_pretty(&json).unwrap_or_default(),
    )
    .map_err(|e| format!("Failed to write schedule file: {e}"))?;
    sched_debug(&format!(
        "[write_schedule_entry] atomic rename: {} → {}",
        tmp_path.display(),
        path.display()
    ));
    let result =
        fs::rename(&tmp_path, &path).map_err(|e| format!("Failed to finalize schedule file: {e}"));
    sched_debug(&format!("[write_schedule_entry] result: {:?}", result));
    result
}

/// List all schedule entries matching the given bot_key and optionally chat_id
fn list_schedule_entries(bot_key: &str, chat_id: Option<i64>) -> Vec<ScheduleEntry> {
    sched_debug(&format!(
        "[list_schedule_entries] bot_key=<redacted>, chat_id={:?}",
        chat_id
    ));
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
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .filter_map(|e| read_schedule_entry(&e.path()))
        .filter(|e| {
            // Recompute the expected verifier per entry — chat_id and id are
            // baked into the verifier, so two schedules sharing a bot_key but
            // different ids produce different verifiers.
            e.bot_key_verifier == live_schedule_key_verifier(&e.id, e.chat_id, bot_key)
        })
        .filter(|e| chat_id.map_or(true, |cid| e.chat_id == cid))
        .collect();
    result.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    sched_debug(&format!(
        "[list_schedule_entries] found {} entries: [{}]",
        result.len(),
        result
            .iter()
            .map(|e| format!("{}({})", e.id, e.schedule_type))
            .collect::<Vec<_>>()
            .join(", ")
    ));
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
    sched_debug(&format!(
        "[delete_schedule_entry] path={}, existed={}, removed={}",
        path.display(),
        existed,
        ok
    ));

    // Also remove the .result file if it exists
    let result_path = dir.join(format!("{id}.result"));
    if result_path.exists() {
        let _ = fs::remove_file(&result_path);
        sched_debug(&format!(
            "[delete_schedule_entry] also removed .result: {}",
            result_path.display()
        ));
    }

    ok
}

/// Directory for schedule run history files: ~/.cokacdir/schedule_history/
/// Each schedule keeps one JSONL file (`<id>.log`) with one record per execution.
fn schedule_history_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".cokacdir").join("schedule_history"))
}

/// Public access to the schedule history file path (for `--cron-history` in main.rs).
/// Rejects ids outside the `[0-9A-F]{8}` format generated by `--cron register`
/// so user-controlled `id` strings can't compose path-traversal segments
/// (e.g. `../../etc/passwd`) into the resulting path.
pub fn schedule_history_path_pub(id: &str) -> Option<std::path::PathBuf> {
    if !is_valid_schedule_id(id) {
        sched_debug(&format!(
            "[schedule_history_path_pub] rejected id={:?} (must match [0-9A-F]{{8}})",
            id
        ));
        return None;
    }
    schedule_history_dir().map(|d| d.join(format!("{}.log", id)))
}

/// Append one JSONL record describing a single schedule execution.
///
/// Best-effort logging: any filesystem error is swallowed (with a sched_debug entry)
/// so a failure here never affects the schedule's own user-facing completion path.
/// Each record carries `chat_id` and a non-secret verifier so `--cron-history` can
/// authorize the caller even after the underlying schedule entry has been deleted
/// (one-time / `--once` schedules) without exposing the bot key capability.
fn append_schedule_history(
    schedule_id: &str,
    chat_id: i64,
    bot_key: &str,
    prompt: &str,
    status: &str, // "ok" | "cancelled" | "error"
    response: &str,
    error: Option<&str>,
    workspace_path: &str,
    duration_ms: u64,
) {
    let Some(dir) = schedule_history_dir() else {
        sched_debug(&format!(
            "[append_schedule_history] id={}, no home dir → skip",
            schedule_id
        ));
        return;
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        sched_debug(&format!(
            "[append_schedule_history] id={}, create_dir_all failed: {}",
            schedule_id, e
        ));
        return;
    }

    // Cap response length to avoid unbounded growth on long-output schedules.
    // Floor to a UTF-8 char boundary so the truncated string stays valid.
    const MAX_RESPONSE_LEN: usize = 4096;
    let response_capped = if response.len() <= MAX_RESPONSE_LEN {
        response.to_string()
    } else {
        let mut end = MAX_RESPONSE_LEN;
        while end > 0 && !response.is_char_boundary(end) {
            end -= 1;
        }
        let mut s = response[..end].to_string();
        s.push_str("\n…[truncated]");
        s
    };

    let mut record = serde_json::json!({
        "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
        "schedule_id": schedule_id,
        "chat_id": chat_id,
        "bot_key_verifier": schedule_history_key_verifier(schedule_id, chat_id, bot_key),
        "prompt": prompt,
        "status": status,
        "response": response_capped,
        "workspace_path": workspace_path,
        "duration_ms": duration_ms,
    });
    if let Some(err) = error {
        record
            .as_object_mut()
            .unwrap()
            .insert("error".to_string(), serde_json::json!(err));
    }

    let path = dir.join(format!("{}.log", schedule_id));
    let Some(_history_lock) = lock_schedule_history_file(&path) else {
        sched_debug(&format!(
            "[append_schedule_history] id={}, lock failed: {}",
            schedule_id,
            path.display()
        ));
        return;
    };
    redact_schedule_history_file_once_unlocked(&path);
    let line = format!("{}\n", record);
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    match result {
        Ok(_) => sched_debug(&format!(
            "[append_schedule_history] id={}, status={}, bytes={}, path={}",
            schedule_id,
            status,
            line.len(),
            path.display()
        )),
        Err(e) => sched_debug(&format!(
            "[append_schedule_history] id={}, write failed: {}, path={}",
            schedule_id,
            e,
            path.display()
        )),
    }
}

fn schedule_history_key_verifier(schedule_id: &str, chat_id: i64, bot_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cokacdir:schedule_history:v1\0");
    hasher.update(schedule_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(chat_id.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(bot_key.as_bytes());
    hex::encode(hasher.finalize())
}

fn lock_schedule_history_file(path: &std::path::Path) -> Option<std::fs::File> {
    use fs2::FileExt;

    let lock_path = path.with_extension("log.lock");
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .ok()?;
    if file.lock_exclusive().is_err() {
        return None;
    }
    Some(file)
}

fn redact_schedule_history_file(path: &std::path::Path) {
    let Some(_history_lock) = lock_schedule_history_file(path) else {
        sched_debug(&format!(
            "[redact_schedule_history_file] lock failed: {}",
            path.display()
        ));
        return;
    };
    redact_schedule_history_file_once_unlocked(path);
}

fn schedule_history_redaction_marker_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_extension("log.redacted")
}

fn mark_schedule_history_redacted(path: &std::path::Path) {
    let marker = schedule_history_redaction_marker_path(path);
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(marker, b"v1\n");
}

fn redact_schedule_history_file_once_unlocked(path: &std::path::Path) {
    let marker = schedule_history_redaction_marker_path(path);
    if marker.exists() {
        return;
    }

    let ok = if path.exists() {
        redact_schedule_history_file_unlocked(path)
    } else {
        true
    };
    if ok {
        mark_schedule_history_redacted(path);
    }
}

fn redact_schedule_history_file_unlocked(path: &std::path::Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };

    let mut changed = false;
    let mut rewritten = String::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            rewritten.push_str(line);
            rewritten.push('\n');
            continue;
        }

        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(mut record) => {
                let legacy_key = record
                    .get("bot_key")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let schedule_id = record
                    .get("schedule_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let chat_id = record.get("chat_id").and_then(|v| v.as_i64());

                if let (Some(legacy_key), Some(schedule_id), Some(chat_id)) =
                    (legacy_key, schedule_id, chat_id)
                {
                    if let Some(obj) = record.as_object_mut() {
                        obj.insert(
                            "bot_key_verifier".to_string(),
                            serde_json::json!(schedule_history_key_verifier(
                                &schedule_id,
                                chat_id,
                                &legacy_key,
                            )),
                        );
                        obj.remove("bot_key");
                        changed = true;
                    }
                }

                rewritten.push_str(&record.to_string());
            }
            Err(_) => rewritten.push_str(line),
        }
        rewritten.push('\n');
    }

    if changed {
        // Atomic rewrite: write to a sibling tmp file then rename over the target,
        // matching the Slack channel-map persist pattern. This keeps the file in
        // either its old or new state under power-loss / partial-write conditions.
        let tmp_path = path.with_extension("log.redact.tmp");
        let result = fs::write(&tmp_path, rewritten).and_then(|_| fs::rename(&tmp_path, path));
        match result {
            Ok(_) => {
                sched_debug(&format!(
                    "[redact_schedule_history_file] redacted legacy bot_key fields: {}",
                    path.display()
                ));
                true
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                sched_debug(&format!(
                    "[redact_schedule_history_file] rewrite failed: {}, path={}",
                    e,
                    path.display()
                ));
                false
            }
        }
    } else {
        true
    }
}

pub fn redact_schedule_history_file_pub(path: &std::path::Path) {
    redact_schedule_history_file(path);
}

pub fn schedule_history_record_authorized_pub(
    record: &serde_json::Value,
    schedule_id: &str,
    chat_id: i64,
    bot_key: &str,
) -> bool {
    let schedule_match = record.get("schedule_id").and_then(|v| v.as_str()) == Some(schedule_id);
    let chat_match = record.get("chat_id").and_then(|v| v.as_i64()) == Some(chat_id);
    if !schedule_match || !chat_match {
        return false;
    }

    let expected = schedule_history_key_verifier(schedule_id, chat_id, bot_key);
    if record.get("bot_key_verifier").and_then(|v| v.as_str()) == Some(expected.as_str()) {
        return true;
    }

    // Backward compatibility for history records written before the verifier field
    // existed. These legacy records are sanitized before being returned to callers.
    record.get("bot_key").and_then(|v| v.as_str()) == Some(bot_key)
}

pub fn sanitize_schedule_history_record_pub(record: &mut serde_json::Value) {
    if let Some(obj) = record.as_object_mut() {
        obj.remove("bot_key");
        obj.remove("bot_key_verifier");
    }
}

#[cfg(test)]
mod schedule_history_tests {
    use super::{
        redact_schedule_history_file_pub, sanitize_schedule_history_record_pub,
        schedule_history_key_verifier, schedule_history_record_authorized_pub,
        schedule_history_redaction_marker_path,
    };

    #[test]
    fn authorizes_history_with_verifier_or_legacy_key() {
        let verifier = schedule_history_key_verifier("sched-1", -42, "secret-key");
        let modern = serde_json::json!({
            "schedule_id": "sched-1",
            "chat_id": -42,
            "bot_key_verifier": verifier,
        });
        assert!(schedule_history_record_authorized_pub(
            &modern,
            "sched-1",
            -42,
            "secret-key"
        ));
        assert!(!schedule_history_record_authorized_pub(
            &modern,
            "sched-1",
            -42,
            "wrong-key"
        ));

        let legacy = serde_json::json!({
            "schedule_id": "sched-1",
            "chat_id": -42,
            "bot_key": "secret-key",
        });
        assert!(schedule_history_record_authorized_pub(
            &legacy,
            "sched-1",
            -42,
            "secret-key"
        ));
    }

    #[test]
    fn sanitizes_history_records_before_output() {
        let mut record = serde_json::json!({
            "schedule_id": "sched-1",
            "chat_id": -42,
            "bot_key": "secret-key",
            "bot_key_verifier": "verifier",
            "status": "ok",
        });

        sanitize_schedule_history_record_pub(&mut record);

        assert!(record.get("bot_key").is_none());
        assert!(record.get("bot_key_verifier").is_none());
        assert_eq!(record.get("status").and_then(|v| v.as_str()), Some("ok"));
    }

    #[test]
    fn redacts_legacy_schedule_history_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sched-1.log");
        std::fs::write(
            &path,
            r#"{"schedule_id":"sched-1","chat_id":-42,"bot_key":"secret-key","status":"ok"}"#,
        )
        .unwrap();

        redact_schedule_history_file_pub(&path);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("bot_key\":\"secret-key"));
        assert!(content.contains("bot_key_verifier"));
        assert!(schedule_history_redaction_marker_path(&path).exists());
    }
}

#[cfg(test)]
mod live_schedule_tests {
    use super::{
        live_schedule_key_verifier, read_schedule_entry, schedule_history_key_verifier,
        write_schedule_entry_pub, ScheduleEntryData,
    };

    fn data_with_bot_key(bot_key: &str) -> ScheduleEntryData {
        ScheduleEntryData {
            id: "sched-guard".to_string(),
            chat_id: 1,
            bot_key: bot_key.to_string(),
            current_path: "/tmp".to_string(),
            prompt: "p".to_string(),
            schedule: "* * * * *".to_string(),
            schedule_type: "cron".to_string(),
            once: Some(false),
            last_run: None,
            created_at: "2026-01-01 00:00:00".to_string(),
            context_summary: None,
        }
    }

    #[test]
    fn write_schedule_entry_pub_rejects_empty_bot_key() {
        // Empty bot_key would compute a verifier the owning bot cannot match,
        // silently orphaning the schedule. Guard catches the
        // list-then-modify-then-write footgun before it touches disk.
        let result = write_schedule_entry_pub(&data_with_bot_key(""));
        assert!(result.is_err(), "empty bot_key must be rejected");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("orphan"),
            "error must explain why empty bot_key is rejected: {}",
            msg
        );
    }

    #[test]
    fn live_verifier_distinct_from_history_verifier() {
        // Domain separation guarantees a verifier minted for live ownership
        // cannot be reused to authorize a history record (or vice versa) even
        // when (id, chat_id, bot_key) collide.
        let live = live_schedule_key_verifier("sched-1", -42, "secret");
        let hist = schedule_history_key_verifier("sched-1", -42, "secret");
        assert_ne!(live, hist);
    }

    #[test]
    fn live_verifier_binds_to_id_and_chat_id() {
        // Changing any input changes the verifier — this is what keeps the
        // verifier from one schedule from authorizing access to another.
        let base = live_schedule_key_verifier("a", 1, "k");
        assert_ne!(base, live_schedule_key_verifier("b", 1, "k"));
        assert_ne!(base, live_schedule_key_verifier("a", 2, "k"));
        assert_ne!(base, live_schedule_key_verifier("a", 1, "k2"));
        assert_eq!(base, live_schedule_key_verifier("a", 1, "k"));
    }

    #[test]
    fn read_schedule_entry_migrates_legacy_bot_key_in_memory() {
        // Pre-migration files store the raw bot_key. The reader must still
        // surface a usable ScheduleEntry whose verifier matches what the
        // current `live_schedule_key_verifier(id, chat_id, raw_key)` produces,
        // so list filters keep working until the file is re-written.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sched-legacy.json");
        std::fs::write(
            &path,
            r#"{
                "id": "sched-legacy",
                "chat_id": 99,
                "bot_key": "legacy-raw-key",
                "current_path": "/tmp",
                "prompt": "p",
                "schedule": "* * * * *",
                "schedule_type": "cron",
                "created_at": "2026-01-01 00:00:00"
            }"#,
        )
        .unwrap();

        let entry = read_schedule_entry(&path).expect("legacy entry should parse");
        assert_eq!(
            entry.bot_key_verifier,
            live_schedule_key_verifier("sched-legacy", 99, "legacy-raw-key")
        );
        // Read must NOT rewrite the file — that would race with concurrent
        // delete and could resurrect a removed schedule. The plaintext stays
        // on disk until the next legitimate write.
        let content_after = std::fs::read_to_string(&path).unwrap();
        assert!(content_after.contains("\"bot_key\""));
    }

    #[test]
    fn read_schedule_entry_prefers_verifier_when_both_present() {
        // Hybrid files (verifier added but legacy field not yet stripped) must
        // trust the verifier — recomputing from the legacy key would silently
        // mask a tampered/rotated verifier.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sched-hybrid.json");
        let real_verifier = live_schedule_key_verifier("sched-hybrid", 1, "actual-key");
        let body = format!(
            r#"{{
                "id": "sched-hybrid",
                "chat_id": 1,
                "bot_key": "stale-key",
                "bot_key_verifier": "{}",
                "current_path": "/tmp",
                "prompt": "p",
                "schedule": "* * * * *",
                "schedule_type": "cron",
                "created_at": "2026-01-01 00:00:00"
            }}"#,
            real_verifier
        );
        std::fs::write(&path, body).unwrap();
        let entry = read_schedule_entry(&path).expect("hybrid entry should parse");
        assert_eq!(entry.bot_key_verifier, real_verifier);
    }
}

/// Delete a schedule's run-history file. Called from `--cron-remove` so that
/// removing a schedule also clears its accumulated history (consistent with how
/// `delete_schedule_entry` already removes the schedule's `.result` companion).
fn delete_schedule_history(id: &str) {
    let Some(dir) = schedule_history_dir() else {
        return;
    };
    let path = dir.join(format!("{}.log", id));
    if path.exists() {
        match fs::remove_file(&path) {
            Ok(_) => sched_debug(&format!(
                "[delete_schedule_history] removed: {}",
                path.display()
            )),
            Err(e) => sched_debug(&format!(
                "[delete_schedule_history] remove failed: {}, path={}",
                e,
                path.display()
            )),
        }
    }
    let marker = schedule_history_redaction_marker_path(&path);
    if marker.exists() {
        let _ = fs::remove_file(marker);
    }
    // Lock sentinel created by lock_schedule_history_file. Removing it keeps the
    // schedule_history dir tidy when a schedule (and its history) goes away.
    let lock_path = path.with_extension("log.lock");
    if lock_path.exists() {
        let _ = fs::remove_file(lock_path);
    }
}

/// Public wrapper for `delete_schedule_history` (used by `--cron-remove` in main.rs).
/// Refuses ids that don't match the generator format — see
/// `is_valid_schedule_id` for the path-traversal rationale.
pub fn delete_schedule_history_pub(id: &str) {
    if !is_valid_schedule_id(id) {
        sched_debug(&format!(
            "[delete_schedule_history_pub] rejected id={:?} (must match [0-9A-F]{{8}})",
            id
        ));
        return;
    }
    delete_schedule_history(id);
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
            sched_debug(&format!(
                "[parse_relative_time] invalid number: {:?} → None",
                num_part
            ));
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
            sched_debug(&format!(
                "[parse_relative_time] unknown unit: {:?} → None",
                unit
            ));
            return None;
        }
    };
    let result = Some(chrono::Local::now() + chrono::Duration::seconds(seconds));
    sched_debug(&format!(
        "[parse_relative_time] → {:?}",
        result
            .as_ref()
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
    ));
    result
}

/// Check if a cron expression matches the given datetime.
/// 5 fields: minute, hour, day-of-month, month, day-of-week (0=Sun)
fn cron_matches(expr: &str, dt: chrono::DateTime<chrono::Local>) -> bool {
    use chrono::Datelike;
    use chrono::Timelike;

    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        sched_debug(&format!(
            "[cron_matches] invalid field count: {} (expected 5) for expr={:?}",
            fields.len(),
            expr
        ));
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

    for (i, ((field, &val), &range_start)) in fields
        .iter()
        .zip(values.iter())
        .zip(range_starts.iter())
        .enumerate()
    {
        let matched = cron_field_matches(field, val, range_start);
        if !matched {
            sched_debug(&format!(
                "[cron_matches] expr={:?}, dt={}, {}({})!={} → false",
                expr,
                dt.format("%H:%M"),
                field_names[i],
                val,
                field
            ));
            return false;
        }
    }
    sched_debug(&format!(
        "[cron_matches] expr={:?}, dt={} → true",
        expr,
        dt.format("%H:%M")
    ));
    true
}

/// True iff `id` is the format produced by `--cron register` (8 uppercase
/// hex chars). Used as a path-traversal guard wherever an `id` from outside
/// the trust boundary is composed into a filesystem path. Internal callers
/// that pass `id` from a disk-loaded `ScheduleEntry` are already trusted —
/// only the public CLI surface needs this check, but the lower-level path
/// helpers also enforce it as defense in depth.
fn is_valid_schedule_id(id: &str) -> bool {
    id.len() == 8
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

/// Validate one comma-separated subexpression of a cron field. Mirrors the
/// shape `cron_field_matches` actually accepts so we never write a schedule
/// the matcher cannot interpret. `full_field` is included in errors to make
/// the failure self-explanatory.
fn validate_cron_part(
    part: &str,
    range_min: u32,
    range_max: u32,
    field_name: &str,
    full_field: &str,
) -> Result<(), String> {
    // Step form: */n or a-b/n
    if let Some((range_part, step_str)) = part.split_once('/') {
        let step: u32 = step_str.parse().map_err(|_| {
            format!(
                "{}: invalid step '{}' in '{}'",
                field_name, step_str, full_field
            )
        })?;
        if step == 0 {
            return Err(format!(
                "{}: step must be > 0 in '{}'",
                field_name, full_field
            ));
        }
        if range_part == "*" {
            return Ok(());
        }
        if let Some((start_str, end_str)) = range_part.split_once('-') {
            let start: u32 = start_str.parse().map_err(|_| {
                format!(
                    "{}: invalid range start '{}' in '{}'",
                    field_name, start_str, full_field
                )
            })?;
            let end: u32 = end_str.parse().map_err(|_| {
                format!(
                    "{}: invalid range end '{}' in '{}'",
                    field_name, end_str, full_field
                )
            })?;
            if start > end {
                return Err(format!(
                    "{}: range {}-{} (start > end) in '{}'",
                    field_name, start, end, full_field
                ));
            }
            if start < range_min || end > range_max {
                return Err(format!(
                    "{}: range {}-{} out of bounds [{},{}] in '{}'",
                    field_name, start, end, range_min, range_max, full_field
                ));
            }
            return Ok(());
        }
        return Err(format!(
            "{}: step requires '*/n' or 'a-b/n' (got '{}' in '{}')",
            field_name, part, full_field
        ));
    }
    // Range: a-b
    if let Some((start_str, end_str)) = part.split_once('-') {
        let start: u32 = start_str.parse().map_err(|_| {
            format!(
                "{}: invalid range start '{}' in '{}'",
                field_name, start_str, full_field
            )
        })?;
        let end: u32 = end_str.parse().map_err(|_| {
            format!(
                "{}: invalid range end '{}' in '{}'",
                field_name, end_str, full_field
            )
        })?;
        if start > end {
            return Err(format!(
                "{}: range {}-{} (start > end) in '{}'",
                field_name, start, end, full_field
            ));
        }
        if start < range_min || end > range_max {
            return Err(format!(
                "{}: range {}-{} out of bounds [{},{}] in '{}'",
                field_name, start, end, range_min, range_max, full_field
            ));
        }
        return Ok(());
    }
    // Single number
    let n: u32 = part.parse().map_err(|_| {
        format!(
            "{}: unsupported value '{}' in '{}' (named values like JAN/MON, macros like @reboot, and characters L/W/? are not supported)",
            field_name, part, full_field
        )
    })?;
    if n < range_min || n > range_max {
        // Common-case hint: many cron implementations accept day-of-week=7
        // as Sunday alias, but our matcher uses 0..=6 (chrono
        // `num_days_from_sunday`). Surface the convention explicitly so the
        // user doesn't have to spelunk the source to figure out the right
        // value.
        let hint = if field_name == "day-of-week" && n == 7 {
            " (Sunday is 0, not 7)"
        } else {
            ""
        };
        return Err(format!(
            "{}: {} out of bounds [{},{}]{} in '{}'",
            field_name, n, range_min, range_max, hint, full_field
        ));
    }
    Ok(())
}

fn validate_cron_field(
    field: &str,
    range_min: u32,
    range_max: u32,
    field_name: &str,
) -> Result<(), String> {
    if field == "*" {
        return Ok(());
    }
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("{}: empty part in '{}'", field_name, field));
        }
        validate_cron_part(part, range_min, range_max, field_name, field)?;
    }
    Ok(())
}

/// Validate a 5-field cron expression against the syntax actually accepted
/// by `cron_field_matches`. Rejects field-count mismatches, named values
/// (JAN/MON), macros (`@reboot`), and special characters (L/W/?) at write
/// time so users see an immediate error instead of a silently dead
/// schedule.
fn validate_cron_expression(expr: &str) -> Result<(), String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron expression must have exactly 5 fields (minute hour day-of-month month day-of-week), got {}",
            fields.len()
        ));
    }
    let bounds: [(u32, u32, &str); 5] = [
        (0, 59, "minute"),
        (0, 23, "hour"),
        (1, 31, "day-of-month"),
        (1, 12, "month"),
        (0, 6, "day-of-week"),
    ];
    for (i, field) in fields.iter().enumerate() {
        let (min, max, name) = bounds[i];
        validate_cron_field(field, min, max, name)?;
    }
    Ok(())
}

/// Check if a single cron field matches a value.
/// Supports: *, single number, comma-separated list, ranges (a-b), step (*/n, a-b/n)
/// range_start: the minimum value for this field (0 for minute/hour/dow, 1 for day/month)
fn cron_field_matches(field: &str, val: u32, range_start: u32) -> bool {
    if field == "*" {
        return true;
    }

    for part in field.split(',') {
        let part = part.trim();
        // Handle step: */n or a-b/n
        if let Some((range_part, step_str)) = part.split_once('/') {
            if let Ok(step) = step_str.parse::<u32>() {
                if step == 0 {
                    continue;
                }
                if range_part == "*" {
                    if (val - range_start) % step == 0 {
                        sched_debug(&format!(
                            "[cron_field_matches] field={}, val={}, */{}  → true",
                            field, val, step
                        ));
                        return true;
                    }
                } else if let Some((start_str, end_str)) = range_part.split_once('-') {
                    if let (Ok(start), Ok(end)) = (start_str.parse::<u32>(), end_str.parse::<u32>())
                    {
                        if val >= start && val <= end && (val - start) % step == 0 {
                            sched_debug(&format!(
                                "[cron_field_matches] field={}, val={}, {}-{}/{} → true",
                                field, val, start, end, step
                            ));
                            return true;
                        }
                    }
                }
            }
        } else if let Some((start_str, end_str)) = part.split_once('-') {
            // Range: a-b
            if let (Ok(start), Ok(end)) = (start_str.parse::<u32>(), end_str.parse::<u32>()) {
                if val >= start && val <= end {
                    sched_debug(&format!(
                        "[cron_field_matches] field={}, val={}, range {}-{} → true",
                        field, val, start, end
                    ));
                    return true;
                }
            }
        } else {
            // Single number
            if let Ok(n) = part.parse::<u32>() {
                if val == n {
                    sched_debug(&format!(
                        "[cron_field_matches] field={}, val={}, exact {} → true",
                        field, val, n
                    ));
                    return true;
                }
            }
        }
    }
    false
}

// === Public API for CLI commands (main.rs) ===

/// Public data struct mirroring ScheduleEntry for cross-module use.
///
/// `bot_key` is the **raw bot_key** supplied by the caller and is asymmetric
/// across the conversion boundary: callers populate it with the raw key on
/// the way in (`From<&ScheduleEntryData> for ScheduleEntry` derives the
/// verifier from it), while on the way out (`From<&ScheduleEntry>`), no raw
/// key is recoverable from disk and the field is left empty.
///
/// **Round-trip caveat**: a list-then-modify-then-write sequence will lose
/// ownership unless the caller re-populates `bot_key` with the raw key
/// before calling `write_schedule_entry_pub`. The latter rejects empty
/// `bot_key` to make the failure explicit instead of silently orphaning
/// the entry.
#[derive(Clone)]
pub struct ScheduleEntryData {
    pub id: String,
    pub chat_id: i64,
    pub bot_key: String,
    pub current_path: String,
    pub prompt: String,
    pub schedule: String,
    pub schedule_type: String,
    pub once: Option<bool>, // only meaningful for cron (None for absolute)
    pub last_run: Option<String>,
    pub created_at: String,
    pub context_summary: Option<String>,
}

impl From<&ScheduleEntry> for ScheduleEntryData {
    fn from(e: &ScheduleEntry) -> Self {
        Self {
            id: e.id.clone(),
            chat_id: e.chat_id,
            // The raw bot_key is not stored on disk; surfacing the verifier
            // here would mislead callers that pass this back through
            // `From<&ScheduleEntryData>` (they'd hash the verifier). Leaving
            // it empty makes the asymmetry explicit.
            bot_key: String::new(),
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
            bot_key_verifier: live_schedule_key_verifier(&d.id, d.chat_id, &d.bot_key),
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
    // Refuse to persist an entry derived from an empty raw bot_key: the
    // resulting `bot_key_verifier` would not match any real owner and the
    // schedule would be silently orphaned (invisible to the bot that should
    // own it). `list_schedule_entries_pub` returns ScheduleEntryData with an
    // empty bot_key, so any list-then-modify-then-write code path must
    // re-supply the raw key before calling this function.
    if data.bot_key.is_empty() {
        return Err(
            "write_schedule_entry_pub: empty bot_key would orphan the entry; \
             callers performing read-modify-write must reset bot_key from the \
             owning bot's key before write"
                .to_string(),
        );
    }
    let entry = ScheduleEntry::from(data);
    write_schedule_entry(&entry)
}

pub fn list_schedule_entries_pub(bot_key: &str, chat_id: Option<i64>) -> Vec<ScheduleEntryData> {
    list_schedule_entries(bot_key, chat_id)
        .iter()
        .map(ScheduleEntryData::from)
        .collect()
}

pub fn list_all_schedule_ids_pub() -> std::collections::HashSet<String> {
    let Some(dir) = schedule_dir() else {
        return std::collections::HashSet::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return std::collections::HashSet::new();
    };
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
    if !is_valid_schedule_id(id) {
        sched_debug(&format!(
            "[delete_schedule_entry_pub] rejected id={:?} (must match [0-9A-F]{{8}})",
            id
        ));
        return false;
    }
    delete_schedule_entry(id)
}

/// Public wrapper for `is_valid_schedule_id` so CLI handlers in `main.rs`
/// (notably `--cron-context`, which is externally invokable as a CLI
/// subcommand) can refuse a malformed `id` before it reaches any
/// path-composing function. The internal `write_schedule_entry` itself
/// stays unchecked so unit tests using non-generator ids (`sched-1`,
/// `sched-guard`, …) keep passing.
pub fn is_valid_schedule_id_pub(id: &str) -> bool {
    is_valid_schedule_id(id)
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
                msg_debug(&format!(
                    "[load_cowork_guidelines] loaded from file: {}, len={}, empty={}",
                    cowork_path.display(),
                    trimmed.len(),
                    trimmed.is_empty()
                ));
                return trimmed;
            } else {
                msg_debug(&format!(
                    "[load_cowork_guidelines] failed to read file: {}",
                    cowork_path.display()
                ));
            }
        } else {
            // Auto-generate default file
            let _ = std::fs::create_dir_all(&prompt_dir);
            let _ = std::fs::write(&cowork_path, DEFAULT_COWORK_GUIDELINES);
            msg_debug(&format!(
                "[load_cowork_guidelines] created default file: {}",
                cowork_path.display()
            ));
        }
    }
    msg_debug("[load_cowork_guidelines] using built-in default");
    DEFAULT_COWORK_GUIDELINES.to_string()
}

/// Build the system prompt for AI sessions
fn build_system_prompt(
    role: &str,
    current_path: &str,
    chat_id: i64,
    bot_key: &str,
    disabled_notice: &str,
    session_id: Option<&str>,
    bot_username: &str,
    bot_display_name: &str,
    user_message: Option<&str>,
    context_count: usize,
    platform: &str,
) -> String {
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
        // Fetch the last N log entries to embed directly in the prompt.
        // Uses the tail-N variant so we don't load the whole JSONL into RAM
        // on every AI request when the chat history grows large.
        let recent_entries = read_group_chat_log_tail(chat_id, context_count, None);
        let recent_lines: String = recent_entries
            .iter()
            .map(|(_, entry)| {
                let bot_label = match &entry.bot_display_name {
                    Some(dn) if !dn.is_empty() => format!("{}(@{})", dn, entry.bot),
                    _ => format!("@{}", entry.bot),
                };
                let from_info = entry
                    .from
                    .as_deref()
                    .map(|f| format!("({})", f))
                    .unwrap_or_default();
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
                format!(
                    "  [{}] {}{}: {}\n",
                    entry.ts, role_display, from_info, display_text
                )
            })
            .collect();

        let recent_section = if recent_lines.is_empty() {
            String::from("\n(No recent entries)")
        } else {
            format!(
                "\nRecent entries (last {}):\n{}",
                recent_entries.len(),
                recent_lines
            )
        };

        // Detect if another bot already answered the SAME user request
        let my_bot = bot_username.trim_start_matches('@').to_lowercase();
        msg_debug(&format!(
            "[dedup] my_bot={:?}, user_message={:?}, window_size={}",
            my_bot,
            user_message.map(|s| truncate_str(s, 80)),
            recent_entries.len()
        ));
        let other_bot_answered_same = if let Some(user_msg) = user_message {
            let user_msg_trimmed = user_msg.trim();
            msg_debug(&format!(
                "[dedup] comparing user_msg_trimmed={:?} (len={})",
                truncate_str(user_msg_trimmed, 100),
                user_msg_trimmed.len()
            ));
            // If the log contains a "user" entry for ANOTHER bot with the same text,
            // that bot has already finished processing (entries are written after completion).
            let mut found = false;
            for (line_num, entry) in &recent_entries {
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
            msg_debug(&format!(
                "[dedup] result: other_bot_answered_same={}",
                found
            ));
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
            let list: String = all_bots
                .iter()
                .map(|(uname, dname)| {
                    let is_me = uname.to_lowercase() == my_bot;
                    match dname {
                        Some(dn) if !dn.is_empty() => {
                            if is_me {
                                format!("  • {}(@{}) ← YOU\n", dn, uname)
                            } else {
                                format!("  • {}(@{})\n", dn, uname)
                            }
                        }
                        _ => {
                            if is_me {
                                format!("  • @{} ← YOU\n", uname)
                            } else {
                                format!("  • @{}\n", uname)
                            }
                        }
                    }
                })
                .collect();
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
        msg_debug(&format!(
            "[build_system_prompt] cowork_guidelines loaded, len={}",
            cowork_guidelines.len()
        ));
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
         ── SCHEDULE: HISTORY (agent-driven inspection) ──\n\
         Schedule run records persist as JSONL on disk at:\n\
         {history_dir}/<SCHEDULE_ID>.log\n\
         Each line is one execution record with fields:\n\
         {{ts, schedule_id, chat_id, prompt, status (ok|cancelled|error), response (capped at 4KB), workspace_path, duration_ms, error?}}\n\n\
         WHEN TO INSPECT: whenever the user refers to a recent scheduled task, \
         including cases where the schedule id is not stated. The folder outlives \
         one-time schedules whose entry has been auto-deleted, so records remain \
         readable even when --cron-list no longer shows the schedule.\n\n\
         HOW: use your normal file/search/listing tools to locate the relevant \
         record(s) inside {history_dir}, then parse the matching JSONL line(s) to \
         answer the user's question. No extra CLI command is required for this path.\n\n\
         ⚠ ISOLATION: this folder is shared across every chat this bot serves. Every \
         record you cite to the user MUST have chat_id == {chat_id}. Records with a \
         different chat_id belong to a different conversation — silently skip them and \
         NEVER quote them back to this user.\n\n\
         CLI ALTERNATIVE — for a sanitized, ownership-checked JSON dump of one schedule's \
         full run history (preferred when you already know the id):\n\
         \"{bin}\" --cron-history <SCHEDULE_ID> --chat {chat_id} --key {bot_key}\n\
         • Output: {{\"status\":\"ok\",\"id\":\"...\",\"count\":N,\"history\":[{{...}}, ...]}}\n\n\
         ═══════════════════════════════════════{group_chat_cowork_section}{group_chat_log_section}{bot_messaging_section}{disabled_notice}",
        role = role,
        bot_username_line = bot_username_line,
        chat_id_line = chat_id_line,
        current_path = crate::utils::format::to_shell_path(current_path),
        chat_id = chat_id,
        bot_key = bot_key,
        bin = shell_bin_path(),
        history_dir = schedule_history_dir_for_prompt(),
        disabled_notice = disabled_notice,
        session_notice = session_notice,
        group_chat_cowork_section = group_chat_cowork_section,
        group_chat_log_section = group_chat_log_section,
        bot_messaging_section = bot_messaging_section,
    )
}

/// Resolve the schedule_history directory to a shell-friendly absolute path for
/// embedding in the system prompt. Falls back to the literal `~/.cokacdir/...`
/// form only when the home directory cannot be determined — the AI's file tools
/// still resolve `~` correctly via shell expansion in that fallback case.
fn schedule_history_dir_for_prompt() -> String {
    match schedule_history_dir() {
        Some(p) => crate::utils::format::to_shell_path(&p.display().to_string()),
        None => "~/.cokacdir/schedule_history".to_string(),
    }
}

/// Detect the full path of powershell.exe on Windows (cached).
/// Runs `Where.exe powershell.exe` once and caches the first match.
fn detect_powershell_path() -> Option<&'static str> {
    static PS_PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PS_PATH
        .get_or_init(|| {
            let output = std::process::Command::new("Where.exe")
                .arg("powershell.exe")
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.lines().next().map(|s| s.trim().to_string())
        })
        .as_deref()
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
        let ps_path = detect_powershell_path().unwrap_or("powershell.exe");
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
    user_display_name: String,
    payload: QueuedPayload,
}

enum QueuedPayload {
    Text {
        text: String,
        /// File/location upload records captured at queue time so the correct message gets them
        pending_uploads: Vec<String>,
    },
    SttAudio {
        msg: Message,
        caption_text: Option<String>,
    },
    AlbumAttachments {
        msgs: Vec<Message>,
        caption_str: Option<String>,
        require_prefix: bool,
        bot_username: String,
    },
}

impl QueuedMessage {
    fn preview_text(&self) -> String {
        match &self.payload {
            QueuedPayload::Text { text, .. } => text.clone(),
            QueuedPayload::SttAudio { .. } => "Audio upload: STT".to_string(),
            QueuedPayload::AlbumAttachments { msgs, .. } => {
                format!("Album: {} attachment(s)", msgs.len())
            }
        }
    }

    fn pending_uploads_len(&self) -> usize {
        match &self.payload {
            QueuedPayload::Text {
                pending_uploads, ..
            } => pending_uploads.len(),
            QueuedPayload::SttAudio { .. } | QueuedPayload::AlbumAttachments { .. } => 0,
        }
    }
}

/// Generate a short uppercase hex ID (7 chars) for a queued message
fn generate_queue_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Mix in a simple hash to reduce collisions within the same millisecond
    let hash = nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    format!("{:07X}", (hash >> 32) as u32 & 0x0FFF_FFFF)
}

/// Shared state: per-chat sessions + bot settings
struct SharedData {
    sessions: HashMap<ChatId, ChatSession>,
    settings: BotSettings,
    /// Per-chat cancel tokens for stopping in-progress AI requests
    cancel_tokens: HashMap<ChatId, Arc<CancelToken>>,
    /// Synchronous mirror of active request tokens for drop-time cleanup.
    request_tokens: RequestTokens,
    /// Request-level tasks spawned below chat workers/scheduler.
    request_tasks: RequestTasks,
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
    loop_feedback: HashMap<ChatId, (String, String)>, // (feedback_text, user_display_name)
    /// Monotonic counter incremented on each /clear. Polling tasks capture it at spawn
    /// and skip session writeback if it changes — covers the case where /clear lands on
    /// a brand-new session whose session_id is None on both spawn and post-completion
    /// (so the sid comparison alone cannot detect the clear).
    clear_epoch: HashMap<ChatId, u64>,
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
    msg_debug(&format!(
        "[auto-restore] no in-memory session for chat_id={}",
        chat_id.0
    ));
    let Some(last_path) = data
        .settings
        .last_sessions
        .get(&chat_id.0.to_string())
        .cloned()
    else {
        msg_debug(&format!(
            "[auto-restore] no last_path in settings for chat_id={}",
            chat_id.0
        ));
        return;
    };
    msg_debug(&format!(
        "[auto-restore] last_path from settings: {:?}",
        last_path
    ));
    let is_dir = Path::new(&last_path).is_dir();
    msg_debug(&format!(
        "[auto-restore] is_dir({:?}) = {}",
        last_path, is_dir
    ));
    if !is_dir {
        return;
    }
    let auto_model = get_model(&data.settings, chat_id);
    let auto_provider = detect_provider(auto_model.as_deref());
    msg_debug(&format!(
        "[auto-restore] auto_provider={}, auto_model={:?}",
        auto_provider, auto_model
    ));
    msg_debug(&format!(
        "[auto-restore] step1: load_existing_session(path={:?}, provider={:?})",
        last_path, auto_provider
    ));
    let existing = load_existing_session(&last_path, auto_provider).or_else(|| {
        msg_debug("[auto-restore] step1 returned None → trying fallback from external source");
        let provider = provider_to_session(auto_provider);
        msg_debug(&format!(
            "[auto-restore] step2: find_latest_session_by_cwd(path={:?}, provider={:?})",
            last_path, auto_provider
        ));
        if let Some(info) = find_latest_session_by_cwd(&last_path, provider) {
            msg_debug(&format!(
                "[auto-restore] step2 found: jsonl={}, session_id={}",
                info.jsonl_path.display(),
                info.session_id
            ));
            convert_and_save_session(&info, &last_path);
            msg_debug("[auto-restore] step3: reload from ai_sessions after convert");
            let reloaded = load_existing_session(&last_path, auto_provider);
            msg_debug(&format!(
                "[auto-restore] step3 result: {}",
                if reloaded.is_some() { "found" } else { "None" }
            ));
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
        pending_uploads_added_at: None,
    });
    session.current_path = Some(last_path.clone());
    let mut restored_active_session = false;
    if let Some((session_data, _)) = existing {
        msg_debug(&format!(
            "[auto-restore] SUCCESS: session_id={}, history_len={}",
            session_data.session_id,
            session_data.history.len()
        ));
        if session_data_has_active_session(&session_data) {
            session.session_id = Some(session_data.session_id.clone());
            session.history = session_data.history.clone();
            restored_active_session = true;
        } else {
            cleanup_session_files(&last_path, auto_provider);
            session.session_id = None;
            session.history.clear();
        }
    } else {
        msg_debug("[auto-restore] FAIL: no session data found (local or external) → empty history");
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    if restored_active_session {
        println!("  [{ts}] ↻ [{user_name}] Auto-restored session: {last_path}");
    } else {
        println!("  [{ts}] ↻ [{user_name}] Auto-started session path: {last_path}");
    }

    // Append auto-restore marker to group chat log
    if chat_id.0 < 0 {
        let uname = data.bot_username.clone();
        let dname = data.bot_display_name.clone();
        if !uname.is_empty() {
            let dn = if dname.is_empty() { None } else { Some(dname) };
            append_group_chat_log(
                chat_id.0,
                &GroupChatLogEntry {
                    ts: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    bot: uname,
                    bot_display_name: dn,
                    role: "system".to_string(),
                    from: None,
                    text: if restored_active_session {
                        format!("Session restored at {}", last_path)
                    } else {
                        format!("Session started at {}", last_path)
                    },
                    clear: false,
                },
            );
        }
    }
}

/// Auto-create a workspace session under ~/.cokacdir/workspace/<random>.
/// Returns (session_id, path) on success; None if filesystem fails.
///
/// Sends a Telegram notification with the new workspace path so the user
/// knows where the AI is operating without having to type /pwd. Only sent
/// when a new workspace is actually created — if a concurrent message
/// already established a session, no notification is sent (the user will
/// learn the path through that other message's flow).
async fn auto_create_workspace_session(
    bot: &Bot,
    state: &SharedState,
    chat_id: ChatId,
    bot_token: &str,
) -> Option<(Option<String>, String)> {
    msg_debug(&format!(
        "[auto_workspace] chat_id={}, auto-creating workspace",
        chat_id.0
    ));
    let auto_path = {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => {
                msg_debug(&format!(
                    "[auto_workspace] chat_id={}, home_dir() returned None",
                    chat_id.0
                ));
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
                msg_debug(&format!(
                    "[auto_workspace] chat_id={}, create_dir_all failed: {}",
                    chat_id.0, e
                ));
                return None;
            }
        }
    };
    // Re-check under lock: another concurrent message may have already created a session
    let (sid, path, was_new) = {
        let mut data = state.lock().await;
        if let Some(existing) = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.current_path.clone())
        {
            msg_debug(&format!(
                "[auto_workspace] chat_id={}, session already created by another message: {}",
                chat_id.0, existing
            ));
            let _ = fs::remove_dir(&auto_path);
            let sid = data
                .sessions
                .get(&chat_id)
                .and_then(|s| s.session_id.clone());
            (sid, existing, false)
        } else {
            let session = data.sessions.entry(chat_id).or_insert_with(|| ChatSession {
                session_id: None,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                pending_uploads_added_at: None,
            });
            session.current_path = Some(auto_path.clone());
            session.session_id = None;
            session.history.clear();
            data.settings
                .last_sessions
                .insert(chat_id.0.to_string(), auto_path.clone());
            save_bot_settings(bot_token, &data.settings);
            msg_debug(&format!(
                "[auto_workspace] chat_id={}, new workspace session created: {}",
                chat_id.0, auto_path
            ));
            (None, auto_path, true)
        }
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    if sid.is_some() {
        println!("  [{ts}] ▶ Using existing session: {path}");
    } else {
        println!("  [{ts}] ▶ Auto-started session: {path}");
    }
    if was_new {
        let display = crate::utils::format::to_shell_path(&path);
        let folder_name = std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let mut notice = format!(
            "Workspace auto-started at <code>{}</code>.",
            html_escape(&display)
        );
        if is_workspace_id(folder_name) {
            notice.push_str(&format!("\nUse /{} to resume this session.", folder_name));
        }
        shared_rate_limit_wait(state, chat_id).await;
        let _ = tg!(
            "send_message",
            bot.send_message(chat_id, &notice)
                .parse_mode(ParseMode::Html)
                .await
        );
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
/// Maximum audio file size accepted for STT.
const STT_AUDIO_MAX_BYTES: usize = 100 * 1024 * 1024;
/// Maximum time to wait for the transcriptor process.
const STT_TIMEOUT_SECS: u64 = 30 * 60;
/// Maximum time to wait for Telegram audio download.
const STT_DOWNLOAD_TIMEOUT_SECS: u64 = 10 * 60;
/// Maximum time to wait for transcriptor binary download.
const TRANSCRIPTOR_DOWNLOAD_TIMEOUT_SECS: u64 = 5 * 60;
/// Maximum transcriptor binary size accepted during download.
const TRANSCRIPTOR_MAX_BYTES: u64 = 200 * 1024 * 1024;
const STT_PROGRESS_TEXT: &str = "Recognizing speech..";
const STT_MODEL_LOAD_TEXT: &str = "Loading speech model..";
const STT_MODEL_LOAD_NOTICE_DELAY: tokio::time::Duration = tokio::time::Duration::from_secs(5);
const STT_STOPPED_TEXT: &str = "Speech recognition stopped.";

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
    let last_sessions: HashMap<String, String> = entry
        .get("last_sessions")
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
            let tool_list: Vec<String> = arr
                .iter()
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
                        let tools: Vec<String> = arr
                            .iter()
                            .filter_map(|t| t.as_str().map(String::from))
                            .collect();
                        (k.clone(), tools)
                    })
                })
                .collect()
        }
        _ => HashMap::new(),
    };

    let as_public_for_group_chat: HashMap<String, bool> = entry
        .get("as_public_for_group_chat")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                .collect()
        })
        .unwrap_or_default();

    let models: HashMap<String, String> = entry
        .get("models")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let debug = entry
        .get("debug")
        .and_then(|v| v.as_bool())
        .unwrap_or(DEBUG_MODE_DEFAULT);

    let silent: HashMap<String, bool> = entry
        .get("silent")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                .collect()
        })
        .unwrap_or_default();

    let direct: HashMap<String, bool> = entry
        .get("direct")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                .collect()
        })
        .unwrap_or_default();

    let context: HashMap<String, usize> = entry
        .get("context")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n as usize)))
                .collect()
        })
        .unwrap_or_default();

    let instructions: HashMap<String, String> = entry
        .get("instructions")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let queue: HashMap<String, bool> = entry
        .get("queue")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                .collect()
        })
        .unwrap_or_default();

    let username = entry
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let display_name = entry
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let greeting = entry
        .get("greeting")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let use_chrome: HashMap<String, bool> = entry
        .get("use_chrome")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                .collect()
        })
        .unwrap_or_default();

    let end_hook: HashMap<String, String> = entry
        .get("end_hook")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let effort: HashMap<String, String> = entry
        .get("effort")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let codex_fast: HashMap<String, bool> = entry
        .get("codex_fast")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                .collect()
        })
        .unwrap_or_default();

    let claude_effort: HashMap<String, String> = entry
        .get("claude_effort")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let stt_models: HashMap<String, String> = entry
        .get("stt_models")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    BotSettings {
        allowed_tools,
        last_sessions,
        owner_user_id,
        as_public_for_group_chat,
        models,
        debug,
        silent,
        direct,
        context,
        instructions,
        queue,
        username,
        display_name,
        greeting,
        use_chrome,
        end_hook,
        effort,
        codex_fast,
        claude_effort,
        stt_models,
    }
}

/// Save bot settings to bot_settings.json
fn save_bot_settings(token: &str, settings: &BotSettings) {
    let Some(path) = bot_settings_path() else {
        return;
    };
    // Ensure directory exists
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    // Use file lock to prevent race condition when multiple bots save simultaneously
    let lock_path = path.with_extension("json.lock");
    let lock_file = match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
    {
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
        "effort": settings.effort,
        "codex_fast": settings.codex_fast,
        "claude_effort": settings.claude_effort,
        "stt_models": settings.stt_models,
    });
    if let Some(owner_id) = settings.owner_user_id {
        entry["owner_user_id"] = serde_json::json!(owner_id);
    }
    json[key] = entry;
    if let Ok(s) = serde_json::to_string_pretty(&json) {
        let tmp_path = path.with_extension("json.tmp");
        // Create tmp with 0o600 *atomically* on unix so the post-write
        // window between fs::write (defaulting to 0o644 minus umask) and
        // a follow-up set_permissions cannot expose the bot token to
        // another local user. We remove any leftover tmp from a prior
        // crash first, because `OpenOptions::mode` only applies to
        // newly-created files — re-opening a stale tmp with looser
        // permissions would silently keep them.
        let write_ok = {
            #[cfg(unix)]
            {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                let _ = fs::remove_file(&tmp_path);
                let mut opts = fs::OpenOptions::new();
                opts.write(true).create_new(true).mode(0o600);
                match opts.open(&tmp_path) {
                    Ok(mut f) => f.write_all(s.as_bytes()).is_ok(),
                    Err(_) => false,
                }
            }
            #[cfg(not(unix))]
            {
                fs::write(&tmp_path, &s).is_ok()
            }
        };
        if write_ok {
            // Rename preserves the 0o600 bits set above. The follow-up
            // set_permissions is retained as a safety net for the rare
            // case where `path` already existed with looser permissions
            // and was hardlinked somewhere — rename replaces the inode,
            // but `set_permissions` here is harmless and self-documenting.
            let _ = fs::rename(&tmp_path, &path);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
        } else {
            // Best-effort cleanup of partial tmp file on write failure.
            let _ = fs::remove_file(&tmp_path);
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
    entry
        .get("token")
        .and_then(|v| v.as_str())
        .map(String::from)
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
            msg_debug(&format!(
                "[resolve_username_by_hash] JSON parse failed: {}",
                e
            ));
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
            msg_debug(&format!(
                "[resolve_username_by_hash] hash key not found: {}",
                hash
            ));
            return None;
        }
    };
    let result = entry
        .get("username")
        .and_then(|v| v.as_str())
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
        entry
            .get("username")
            .and_then(|v| v.as_str())
            .map(|u| u.to_lowercase() == target)
            .unwrap_or(false)
    });
    msg_debug(&format!(
        "[bot_username_exists] target={}, found={}",
        target, found
    ));
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
            return entry
                .get("display_name")
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

/// Normalize tool name: first letter uppercase, rest lowercase
fn normalize_tool_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn command_name(text: &str) -> Option<&str> {
    let token = text
        .strip_prefix('/')?
        .split_whitespace()
        .next()
        .unwrap_or("");
    if token.is_empty() {
        return None;
    }
    Some(token.split('@').next().unwrap_or(token))
}

fn command_args(text: &str) -> &str {
    let Some(rest) = text.trim_start().strip_prefix('/') else {
        return "";
    };
    let mut parts = rest.splitn(2, char::is_whitespace);
    let _ = parts.next();
    parts.next().unwrap_or("").trim()
}

/// True iff `text` is the slash command `name` — i.e. `/name`, `/name args`,
/// `/name@bot`, or `/name@bot args`. Substring-prefix matches do NOT count
/// (e.g., `/silentmode` is not `/silent`). Use this everywhere instead of
/// raw `text.starts_with("/foo")` so that future commands that share a
/// prefix with an existing one don't get silently re-routed.
fn is_cmd(text: &str, name: &str) -> bool {
    command_name(text).map(|n| n == name).unwrap_or(false)
}

/// Exact-match check against the owner-only command list. Compares against
/// `command_name` (after `@bot` and arg stripping) rather than raw
/// `starts_with` so that adding a future command like `/silentmode` does
/// not get mis-classified as owner-only just because it shares a prefix.
fn is_owner_only_command(text: &str) -> bool {
    let Some(name) = command_name(text) else {
        return false;
    };
    matches!(
        name,
        // `start` is intentionally NOT owner-only. Once a group is set to
        // `/public on`, any member can already run arbitrary shell
        // commands through the `!` prefix (`handle_shell_command`),
        // which is strictly more powerful than what `/start` does
        // (switching the bot's working directory). Blocking `/start`
        // while leaving `!` open would be security theater. `/public
        // off` groups already silent-reject every non-owner message
        // upstream (see the imprinting check), so dropping `start`
        // here does not weaken the closed-group case either.
        "clear"
            | "public"
            | "setpollingtime"
            | "model"
            | "stt_model"
            | "effort"
            | "fast"
            | "greeting"
            | "debug"
            | "envvars"
            | "usechrome"
            | "silent"
            | "queue"
            | "direct"
            | "contextlevel"
            | "instruction"
            | "instruction_clear"
            | "setendhook"
            | "setendhook_clear"
            | "allowed"
    )
}

/// All available tools with (description, is_destructive)
const ALL_TOOLS: &[(&str, &str, bool)] = &[
    ("Bash", "Execute shell commands", true),
    ("Read", "Read file contents from the filesystem", false),
    ("Edit", "Perform find-and-replace edits in files", true),
    ("Write", "Create or overwrite files", true),
    ("Glob", "Find files by name pattern", false),
    ("Grep", "Search file contents with regex", false),
    (
        "Task",
        "Launch autonomous sub-agents for complex tasks",
        true,
    ),
    ("TaskOutput", "Retrieve output from background tasks", false),
    ("TaskStop", "Stop a running background task", false),
    ("WebFetch", "Fetch and process web page content", true),
    (
        "WebSearch",
        "Search the web for up-to-date information",
        true,
    ),
    ("NotebookEdit", "Edit Jupyter notebook cells", true),
    ("Skill", "Invoke slash-command skills", false),
    (
        "TaskCreate",
        "Create a structured task in the task list",
        false,
    ),
    ("TaskGet", "Retrieve task details by ID", false),
    ("TaskUpdate", "Update task status or details", false),
    ("TaskList", "List all tasks and their status", false),
    (
        "AskUserQuestion",
        "Ask the user a question (interactive)",
        false,
    ),
    ("EnterPlanMode", "Enter planning mode (interactive)", false),
    ("ExitPlanMode", "Exit planning mode (interactive)", false),
];

/// Tool info: (description, is_destructive)
fn tool_info(name: &str) -> (&'static str, bool) {
    ALL_TOOLS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, desc, destr)| (*desc, *destr))
        .unwrap_or(("Custom tool", false))
}

/// Format a risk badge for display
fn risk_badge(destructive: bool) -> &'static str {
    if destructive {
        "!!!"
    } else {
        ""
    }
}

/// One unit of work dispatched into a chat's serial worker. Module-level
/// (not local to `process_batch`) so the same type can flow through the
/// per-chat mpsc channel.
enum DispatchUnit {
    Single(Message),
    Album(Vec<Message>),
}

/// Per-chat serial worker registry. Each chat has at most one
/// long-lived worker task that consumes a FIFO mpsc; this guarantees
/// strict-arrival-order processing **across** `getUpdates` batches, not
/// just within one batch. Different chats keep running in parallel.
struct ChatWorkerEntry {
    tx: tokio::sync::mpsc::UnboundedSender<DispatchUnit>,
    abort: tokio::task::AbortHandle,
}

impl Drop for ChatWorkerEntry {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

type ChatWorkers = Arc<Mutex<HashMap<ChatId, ChatWorkerEntry>>>;
type RequestTasks = Arc<std::sync::Mutex<HashMap<u64, tokio::task::AbortHandle>>>;
type RequestTokens = Arc<std::sync::Mutex<HashMap<ChatId, Arc<CancelToken>>>>;

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct RunBotCleanup {
    state: SharedState,
    request_tokens: RequestTokens,
    request_tasks: RequestTasks,
}

impl Drop for RunBotCleanup {
    fn drop(&mut self) {
        cancel_request_tokens(&self.request_tokens);
        if let Ok(data) = self.state.try_lock() {
            for token in data.cancel_tokens.values() {
                cancel_token_now(token);
            }
        }
        abort_request_tasks(&self.request_tasks);
        let state = self.state.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let data = state.lock().await;
                for token in data.cancel_tokens.values() {
                    cancel_token_now(token);
                }
            });
        }
    }
}

fn new_chat_workers() -> ChatWorkers {
    Arc::new(Mutex::new(HashMap::new()))
}

fn new_request_tasks() -> RequestTasks {
    Arc::new(std::sync::Mutex::new(HashMap::new()))
}

fn new_request_tokens() -> RequestTokens {
    Arc::new(std::sync::Mutex::new(HashMap::new()))
}

static NEXT_DISPATCH_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static NEXT_REQUEST_TASK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

tokio::task_local! {
    static CURRENT_DISPATCH_ID: u64;
}

fn next_dispatch_id() -> u64 {
    NEXT_DISPATCH_ID.fetch_add(1, Ordering::Relaxed).max(1)
}

fn current_dispatch_id() -> u64 {
    CURRENT_DISPATCH_ID.try_with(|id| *id).unwrap_or(0)
}

fn new_cancel_token() -> Arc<CancelToken> {
    new_cancel_token_for_owner(current_dispatch_id())
}

fn new_cancel_token_for_owner(owner_dispatch_id: u64) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    token
        .owner_dispatch_id
        .store(owner_dispatch_id, Ordering::Relaxed);
    token
}

fn cancel_token_now(token: &Arc<CancelToken>) {
    token.cancel_now();
}

fn insert_cancel_token_locked(data: &mut SharedData, chat_id: ChatId, token: Arc<CancelToken>) {
    data.cancel_tokens.insert(chat_id, token.clone());
    if let Ok(mut tokens) = data.request_tokens.lock() {
        tokens.insert(chat_id, token);
    }
}

fn remove_cancel_token_locked(data: &mut SharedData, chat_id: ChatId) -> Option<Arc<CancelToken>> {
    if let Ok(mut tokens) = data.request_tokens.lock() {
        tokens.remove(&chat_id);
    }
    data.cancel_tokens.remove(&chat_id)
}

fn cancel_request_tokens(tokens: &RequestTokens) {
    let snapshot = if let Ok(tokens) = tokens.lock() {
        tokens.values().cloned().collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for token in snapshot {
        cancel_token_now(&token);
    }
}

async fn reclaim_panicked_dispatch_token(
    state: &SharedState,
    chat_id: ChatId,
    dispatch_id: u64,
    context: &str,
) {
    let leaked: Option<Arc<CancelToken>> = {
        let mut data = state.lock().await;
        let reclaim = match data.cancel_tokens.get(&chat_id) {
            Some(post) => {
                let owner = post.owner_dispatch_id.load(Ordering::Relaxed);
                if owner != dispatch_id {
                    msg_debug(&format!(
                        "[{} {}] panic recovery: token owner={} does not match dispatch={}, no cleanup",
                        context, chat_id.0, owner, dispatch_id
                    ));
                }
                owner == dispatch_id
            }
            None => false,
        };
        if reclaim {
            remove_cancel_token_locked(&mut data, chat_id)
        } else {
            None
        }
    };

    if let Some(tok) = leaked {
        cancel_token_now(&tok);
        msg_debug(&format!(
            "[{} {}] reset busy slot after panic",
            context, chat_id.0
        ));
    }
}

struct RequestTaskGuard {
    tasks: RequestTasks,
    id: u64,
}

impl Drop for RequestTaskGuard {
    fn drop(&mut self) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.remove(&self.id);
        }
    }
}

fn abort_request_tasks(tasks: &RequestTasks) {
    let handles = if let Ok(mut tasks) = tasks.lock() {
        tasks.drain().map(|(_, handle)| handle).collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for handle in handles {
        handle.abort();
    }
}

fn spawn_tracked_request_task<F, T>(tasks: RequestTasks, fut: F) -> tokio::task::JoinHandle<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let id = NEXT_REQUEST_TASK_ID.fetch_add(1, Ordering::Relaxed).max(1);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<RequestTaskGuard>();
    let handle = tokio::spawn(async move {
        let _guard = ready_rx
            .await
            .expect("request task guard dropped before start");
        fut.await
    });
    let abort = handle.abort_handle();
    let guard = RequestTaskGuard {
        tasks: tasks.clone(),
        id,
    };
    if let Ok(mut task_map) = tasks.lock() {
        task_map.insert(id, abort);
    }
    let _ = ready_tx.send(guard);
    handle
}

fn monitor_dispatch_panic<T: Send + 'static>(
    join: tokio::task::JoinHandle<T>,
    state: SharedState,
    chat_id: ChatId,
    dispatch_id: u64,
    context: &'static str,
) {
    tokio::spawn(async move {
        let Err(join_err) = join.await else {
            return;
        };
        if !join_err.is_panic() {
            return;
        }
        let panic_str = redact_known_tokens(&join_err.to_string());
        msg_debug(&format!(
            "[{}] chat_id={}, background dispatch PANICKED: {}",
            context, chat_id.0, panic_str
        ));
        eprintln!(
            "  ⚠ Chat {} {} dispatch panicked: {} — recovering",
            chat_id.0, context, panic_str
        );
        reclaim_panicked_dispatch_token(&state, chat_id, dispatch_id, context).await;
    });
}

/// Spawn a tracked blocking task. NOTE: `AbortHandle::abort()` only signals
/// cancellation on the JoinHandle side — the blocking thread itself runs `f`
/// to completion (a tokio limitation for `spawn_blocking`). Real shutdown of
/// the blocking work must come from the `CancelToken` (cancelled flag +
/// SIGKILL to the child PID's process group) carried in by the caller, not
/// from this map's abort handle.
fn spawn_tracked_blocking_task<F>(tasks: RequestTasks, f: F) -> tokio::task::JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    let id = NEXT_REQUEST_TASK_ID.fetch_add(1, Ordering::Relaxed).max(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<RequestTaskGuard>(1);
    let handle = tokio::task::spawn_blocking(move || {
        let Ok(_guard) = ready_rx.recv() else {
            return;
        };
        f();
    });
    let abort = handle.abort_handle();
    let guard = RequestTaskGuard {
        tasks: tasks.clone(),
        id,
    };
    if let Ok(mut task_map) = tasks.lock() {
        task_map.insert(id, abort);
    }
    let _ = ready_tx.send(guard);
    handle
}

/// Variant of `spawn_tracked_blocking_task` that returns `f`'s result. The
/// outer `Option` is `None` only if the guard channel closed before `f` ran
/// (e.g. the spawning future was dropped between spawn and send). The same
/// abort caveat applies: cancelling the JoinHandle does not preempt the
/// blocking thread; rely on the `CancelToken` carried by `f` for shutdown.
fn spawn_tracked_blocking_result<F, T>(
    tasks: RequestTasks,
    f: F,
) -> tokio::task::JoinHandle<Option<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let id = NEXT_REQUEST_TASK_ID.fetch_add(1, Ordering::Relaxed).max(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<RequestTaskGuard>(1);
    let handle = tokio::task::spawn_blocking(move || {
        let Ok(_guard) = ready_rx.recv() else {
            return None;
        };
        Some(f())
    });
    let abort = handle.abort_handle();
    let guard = RequestTaskGuard {
        tasks: tasks.clone(),
        id,
    };
    if let Ok(mut task_map) = tasks.lock() {
        task_map.insert(id, abort);
    }
    let _ = ready_tx.send(guard);
    handle
}

/// Result of `polling_loop`. `Fatal` means the loop must NOT be restarted
/// by the outer reconnect wrapper — restarting would just re-trigger the
/// same condition (e.g. another instance polling, or a revoked token).
enum PollingExit {
    Fatal(String),
}

/// Outcome of a `run_bot` invocation, surfaced to the caller in `main` so
/// that `Fatal` (revoked token, persistent Conflict, or other
/// non-recoverable polling condition) can map to a non-zero process exit
/// code. Mirrors the role of `BridgeExit` for Discord/Slack — without
/// this, a Telegram-only deployment whose token gets revoked would exit
/// with status 0 and a supervisor (systemd, docker) would not restart it.
pub enum BotExit {
    Graceful,
    Fatal,
}

/// Classify a `getUpdates` error as a non-recoverable misconfiguration we
/// must surface to the user instead of silently retrying. Returns `Some`
/// with a human-readable label only for cases where retrying cannot help.
///
/// We inspect the full `RequestError` `Display` (not just the inner
/// `ApiError`) so the check works regardless of whether teloxide reports
/// the failure as `Api(Unauthorized)`, `Api(Unknown("Unauthorized"))`, or
/// (in some versions) wraps it in `Network(...)`.
///
/// Anchoring on the exact Telegram description prefixes — `Conflict: `
/// and `Unauthorized` at a word boundary — avoids false positives on
/// generic strings like "merge conflict" or sentences containing
/// "unauthorized" mid-message. False-positive here is severe (it would
/// stop a healthy bot), so we err on the side of precision; a missed
/// classification merely degrades to the prior infinite-retry behaviour.
fn detect_fatal_polling_error(e: &teloxide::RequestError) -> Option<&'static str> {
    let display = e.to_string();

    // 409: Telegram returns description literally `Conflict: terminated by
    // other getUpdates request` (or similar `Conflict: <reason>`). The
    // colon+space anchor keeps unrelated uses of the word out.
    if display.contains("Conflict: ") {
        return Some("CONFLICT: another bot instance is using this token");
    }

    // 401: typed `ApiError::Unauthorized` renders as `... Unauthorized` at
    // the end of the Display string; `ApiError::Unknown("Unauthorized")`
    // and the rare `Unauthorized: <reason>` form also surface here.
    // Trailing-anchor and `: ` anchor avoid matches like
    // `... user is unauthorized to ...` mid-message.
    if display.ends_with("Unauthorized") || display.contains("Unauthorized: ") {
        return Some("UNAUTHORIZED: bot token is invalid or has been revoked");
    }

    None
}

/// Get-or-create the FIFO sender for `cid`. The worker task is spawned on
/// first use and lives until the sender is dropped (workers map cleared on
/// shutdown). Sender is cheap to clone (Arc inside).
async fn ensure_chat_worker(
    workers: &ChatWorkers,
    cid: ChatId,
    bot: &Bot,
    state: &SharedState,
    token: &str,
    bot_username: &str,
) -> tokio::sync::mpsc::UnboundedSender<DispatchUnit> {
    let mut guard = workers.lock().await;
    if let Some(entry) = guard.get(&cid) {
        return entry.tx.clone();
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let bot_owned = bot.clone();
    let state_owned = state.clone();
    let token_owned = token.to_string();
    let username_owned = bot_username.to_string();
    let handle = tokio::spawn(run_chat_worker(
        rx,
        bot_owned,
        state_owned,
        token_owned,
        username_owned,
        cid,
    ));
    let abort = handle.abort_handle();
    guard.insert(
        cid,
        ChatWorkerEntry {
            tx: tx.clone(),
            abort,
        },
    );
    tx
}

/// Long-lived per-chat worker: pulls units from the FIFO and runs each
/// handler to completion before pulling the next. Handlers are run inside
/// a child `tokio::spawn` so a panic kills only that one unit — the worker
/// observes it via `JoinError` and continues.
async fn run_chat_worker(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<DispatchUnit>,
    bot: Bot,
    state: SharedState,
    token: String,
    bot_username: String,
    cid: ChatId,
) {
    while let Some(unit) = rx.recv().await {
        let dispatch_id = next_dispatch_id();
        let bot_c = bot.clone();
        let state_c = state.clone();
        let token_c = token.clone();
        let username_c = bot_username.clone();
        // This per-unit spawn is intentionally not inserted into `request_tasks`:
        // its lifecycle is bound to the chat_worker via the `join.await` below,
        // and the chat_worker itself is tracked in `ChatWorkerEntry` (whose Drop
        // aborts it). Aborting a tracked async parent does not kill blocking
        // descendants anyway — child processes are reaped via the cancel_token's
        // SIGKILL path during run_bot shutdown, which is the correctness boundary
        // that actually matters.
        let join = tokio::spawn(async move {
            CURRENT_DISPATCH_ID
                .scope(dispatch_id, async move {
                    process_unit(unit, bot_c, state_c, &token_c, &username_c).await
                })
                .await
        });
        match join.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => msg_debug(&format!("[chat_worker {}] handler error: {}", cid.0, e)),
            Err(join_err) => {
                let panic_str = redact_known_tokens(&join_err.to_string());
                msg_debug(&format!(
                    "[chat_worker {}] handler PANICKED: {}",
                    cid.0, panic_str
                ));
                eprintln!(
                    "  ⚠ Chat {} handler panicked: {} — recovering",
                    cid.0, panic_str
                );
                // Recover only the busy slot owned by this dispatch. Scheduler
                // reservations, queued feedback, and later handlers have
                // different owner ids, so panic cleanup must not use Arc counts.
                // A matching owner id means the panicked dispatch inserted
                // the currently active token and no foreign reservation has
                // replaced it. Mismatched owner ids are left for their owner.
                let leaked: Option<Arc<CancelToken>> = {
                    let mut data = state.lock().await;
                    let reclaim = match data.cancel_tokens.get(&cid) {
                        Some(post) => {
                            let owner = post.owner_dispatch_id.load(Ordering::Relaxed);
                            if owner != dispatch_id {
                                msg_debug(&format!(
                                    "[chat_worker {}] panic recovery: token owner={} does not match dispatch={}, no cleanup",
                                    cid.0, owner, dispatch_id
                                ));
                            }
                            owner == dispatch_id
                        }
                        None => false,
                    };
                    if reclaim {
                        remove_cancel_token_locked(&mut data, cid)
                    } else {
                        None
                    }
                };
                if let Some(tok) = leaked {
                    // Best-effort cleanup: set cancelled=true AND SIGKILL the child
                    // PID's process group. The cancelled flag matters when a sibling
                    // thread (e.g. an
                    // exec polling loop on a blocking thread that survived the async
                    // parent's panic) is still polling the token — it lets that
                    // thread exit promptly instead of relying on signal delivery
                    // alone. The owner check above prevents touching a child process
                    // belonging to a scheduler or later handler.
                    cancel_token_now(&tok);
                    msg_debug(&format!(
                        "[chat_worker {}] reset busy slot after panic",
                        cid.0
                    ));
                    eprintln!("  ⚠ Chat {} busy slot reset (held by panicked task)", cid.0);
                }
            }
        }
    }
    msg_debug(&format!(
        "[chat_worker {}] channel closed, worker exiting",
        cid.0
    ));
}

/// Dispatch a single unit to its real handler. Album fragments (size 1)
/// fall through to the regular text path; size ≥ 2 albums hit the atomic
/// album handler.
///
/// FIFO invariant: this function is awaited inline by `run_chat_worker`,
/// which is single-consumer per chat. Long-running work (AI streaming,
/// shell commands, etc.) MUST be spawned via `spawn_tracked_request_task`
/// / `spawn_tracked_blocking_task` so the handler returns promptly and
/// the worker can pull the next unit. Awaiting a slow operation directly
/// here head-of-line-blocks every subsequent message in the same chat —
/// including `/stop`, `/queue`, and other fast commands. See 0.6.1
/// changelog for the original per-chat FIFO design.
async fn process_unit(
    unit: DispatchUnit,
    bot: Bot,
    state: SharedState,
    token: &str,
    bot_username: &str,
) -> ResponseResult<()> {
    match unit {
        DispatchUnit::Single(msg) => handle_message(bot, msg, state, token, bot_username).await,
        DispatchUnit::Album(msgs) => {
            if msgs.len() < 2 {
                if let Some(msg) = msgs.into_iter().next() {
                    handle_message(bot, msg, state, token, bot_username).await
                } else {
                    Ok(())
                }
            } else {
                handle_album_batch(bot, msgs, state, token, bot_username).await
            }
        }
    }
}

/// Compute the `getUpdates` offset that confirms `last_id`.
/// Telegram's offset parameter is `i32`. If `last_id` sits at the i32 upper
/// bound, `last_id + 1` is not representable; we cap at `i32::MAX` and log
/// the boundary hit so production occurrences are visible. This means the
/// boundary update will be re-delivered until Telegram's update_id rolls
/// past it (rare in practice).
fn next_offset_after(last_id: u32) -> i32 {
    if last_id >= i32::MAX as u32 {
        msg_debug(&format!(
            "[polling] update_id {} at i32::MAX boundary — confirmation not representable, this update may be re-delivered",
            last_id
        ));
        i32::MAX
    } else {
        (last_id + 1) as i32
    }
}

/// Call `getUpdates(offset, limit=1)` with retry + exponential backoff.
/// Honors a server-mandated `RetryAfter` (429) by sleeping the requested
/// duration instead of the linear backoff. Used during startup flush where
/// failure must not silently leak stale updates into the polling loop.
async fn get_updates_with_retry(
    bot: &Bot,
    offset: i32,
    max_attempts: u32,
    label: &str,
) -> Result<Vec<teloxide::types::Update>, teloxide::RequestError> {
    let mut last_err: Option<teloxide::RequestError> = None;
    for attempt in 1..=max_attempts {
        match bot.get_updates().offset(offset).limit(1).await {
            Ok(updates) => return Ok(updates),
            Err(e) => {
                let backoff = match &e {
                    teloxide::RequestError::RetryAfter(s) => s.duration(),
                    _ => std::time::Duration::from_millis(
                        500u64.saturating_mul(attempt as u64).min(8_000),
                    ),
                };
                msg_debug(&format!(
                    "[run_bot] {} attempt {}/{} failed (sleep {}ms): {}",
                    label,
                    attempt,
                    max_attempts,
                    backoff.as_millis(),
                    e
                ));
                last_err = Some(e);
                if attempt < max_attempts {
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
    Err(last_err.expect("get_updates_with_retry: max_attempts >= 1"))
}

/// Entry point: start the Telegram bot with long polling.
/// `api_url`: Optional API base URL override (used by messenger bridge proxy).
///            When None, connects to the real Telegram API.
pub async fn run_bot(token: &str, api_url: Option<&str>) -> BotExit {
    // Register before any debug logging so a startup failure log still gets
    // its token redacted.
    register_token_for_redaction(token);
    let api_base_url = api_url.unwrap_or("https://api.telegram.org");
    msg_debug(&format!(
        "[run_bot] api_url={:?}, api_base_url={}",
        api_url.is_some(),
        api_base_url
    ));
    // The HTTP client timeout must exceed the long-polling timeout used in
    // `polling_loop` (30s, see `bot.get_updates().timeout(30)`). teloxide's
    // default_reqwest_settings ships a 17s timeout which closes the connection
    // before the server-side long-poll completes, surfacing as repeated
    // `getUpdates ... operation timed out` errors in idle periods.
    let client = teloxide::net::default_reqwest_settings()
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .expect("failed to build reqwest client for Telegram bot");
    let bot = Bot::with_client(token, client);
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
            msg_debug(&format!(
                "[run_bot] get_me success: username={}, display_name={}, id={}",
                uname, dname, me.id
            ));
            println!("  ✓ Bot: {} (@{uname})", dname);
            (uname, dname)
        }
        Err(e) => {
            msg_debug(&format!("[run_bot] get_me failed: {}", e));
            println!("  ⚠ Failed to get bot info: {}", redact_err(&e));
            (String::new(), String::new())
        }
    };

    // Save username and display_name to bot_settings for CLI --message lookup
    if !bot_username.is_empty() {
        msg_debug(&format!(
            "[run_bot] saving username to bot_settings: {}",
            bot_username
        ));
        bot_settings.username = bot_username.clone();
        bot_settings.display_name = bot_display_name.clone();
        save_bot_settings(token, &bot_settings);
    } else {
        msg_debug("[run_bot] bot_username is empty, skipping save");
    }

    // Restore process-wide debug flag from env or any saved bot setting.
    refresh_global_debug_flags();

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
        teloxide::types::BotCommand::new("stt_model", "Set speech recognition model"),
        teloxide::types::BotCommand::new("effort", "Set Claude/Codex effort level"),
        teloxide::types::BotCommand::new("fast", "Toggle Codex fast service tier"),
        teloxide::types::BotCommand::new("debug", "Toggle debug logging"),
        teloxide::types::BotCommand::new("envvars", "Show all environment variables"),
        teloxide::types::BotCommand::new("silent", "Toggle silent mode (hide tool calls)"),
        teloxide::types::BotCommand::new("direct", "Toggle direct mode (group only)"),
        teloxide::types::BotCommand::new("contextlevel", "Set group chat log context count"),
        teloxide::types::BotCommand::new("query", "Send message to AI (/query@bot for groups)"),
        teloxide::types::BotCommand::new("instruction", "Set system instruction for this chat"),
        teloxide::types::BotCommand::new("instruction_clear", "Clear system instruction"),
        teloxide::types::BotCommand::new(
            "setendhook",
            "Set message to send when processing completes",
        ),
        teloxide::types::BotCommand::new("setendhook_clear", "Clear end hook message"),
    ];
    if let Err(e) = tg!("set_my_commands", bot.set_my_commands(commands).await) {
        println!("  ⚠ Failed to set bot commands: {}", redact_err(&e));
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
        request_tokens: new_request_tokens(),
        request_tasks: new_request_tasks(),
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
        clear_epoch: HashMap::new(),
    }));
    let (request_tokens, request_tasks) = {
        let data = state.lock().await;
        (data.request_tokens.clone(), data.request_tasks.clone())
    };
    let _run_bot_cleanup = RunBotCleanup {
        state: state.clone(),
        request_tokens: request_tokens.clone(),
        request_tasks: request_tasks.clone(),
    };

    println!("  ✓ Bot connected — Listening for messages");
    println!("  ✓ Scheduler started (5s interval)");

    // Do not proactively message users on bot startup. The bot now waits until
    // a user sends a command or message before responding.

    // Schedule workspace directories are preserved for user access via /start

    // Spawn scheduler loop
    let scheduler_bot = bot.clone();
    let scheduler_state = state.clone();
    let scheduler_token = token.to_string();
    let scheduler_bot_username = bot_username.clone();
    let scheduler_bot_display_name = bot_display_name.clone();
    let scheduler_handle = tokio::spawn(scheduler_loop(
        scheduler_bot,
        scheduler_state,
        scheduler_token,
        scheduler_bot_username,
        scheduler_bot_display_name,
    ));
    let scheduler_abort = scheduler_handle.abort_handle();
    let _scheduler_guard = AbortOnDrop(scheduler_abort);

    // Flush all pending updates so we don't process messages that arrived while
    // the bot was offline.
    // Step 1: getUpdates(offset=-1) returns the last pending update id.
    // Step 2: getUpdates(offset=last_id+1) confirms it so the polling loop
    //         won't receive it.
    // Both steps retry with backoff and honor `RetryAfter`. Either step
    // failing exhausting all retries is fatal — proceeding without a
    // successful flush would silently process stale messages, which the
    // documented contract above forbids.
    const FLUSH_MAX_ATTEMPTS: u32 = 5;
    match get_updates_with_retry(&bot, -1, FLUSH_MAX_ATTEMPTS, "flush step 1").await {
        Ok(updates) => {
            if let Some(last) = updates.last() {
                let next_offset = next_offset_after(last.id.0);
                msg_debug(&format!(
                    "[run_bot] flush step 1: got last update_id={}, confirming with offset={}",
                    last.id.0, next_offset
                ));
                match get_updates_with_retry(&bot, next_offset, FLUSH_MAX_ATTEMPTS, "flush step 2")
                    .await
                {
                    Ok(_) => {
                        msg_debug(&format!(
                            "[run_bot] flush step 2: confirmed offset={}",
                            next_offset
                        ));
                        println!("  ✓ Flushed pending updates (up to id={})", last.id.0);
                    }
                    Err(e) => {
                        eprintln!(
                            "  ✗ FATAL: failed to confirm pending updates after {} attempts: {}",
                            FLUSH_MAX_ATTEMPTS,
                            redact_err(&e)
                        );
                        eprintln!("    Aborting to prevent processing stale messages.");
                        // Return Fatal instead of `process::exit(1)` so the
                        // RAII guards installed earlier in this function run:
                        // `_scheduler_guard` aborts the already-spawned
                        // scheduler, and `_run_bot_cleanup` SIGKILLs any
                        // child process the scheduler may have launched
                        // during the gap between its spawn and this flush
                        // failure. Direct `process::exit` skipped both,
                        // orphaning children and — in multi-bot mode —
                        // tearing down healthy sibling bots (Discord/Slack)
                        // that were running in the same runtime.
                        return BotExit::Fatal;
                    }
                }
            } else {
                msg_debug("[run_bot] flush: no pending updates");
                println!("  ✓ No pending updates");
            }
        }
        Err(e) => {
            eprintln!(
                "  ✗ FATAL: failed to fetch pending updates after {} attempts: {}",
                FLUSH_MAX_ATTEMPTS,
                redact_err(&e)
            );
            eprintln!("    Cannot safely start polling without flushing — aborting.");
            // See comment on the symmetric path above: return Fatal so the
            // RAII guards run and sibling bots survive in multi-bot mode.
            return BotExit::Fatal;
        }
    }

    // Run polling loop with automatic reconnection on network failure.
    // We process raw `getUpdates` batches so album members (same
    // `media_group_id`) sharing one batch are grouped atomically — Telegram
    // delivers an album as a contiguous run of Updates within one
    // `getUpdates` response, so the batch boundary is a deterministic
    // end-of-album signal. The `limit=100` request is far above Telegram's
    // 10-photo album cap, so any album fits in a single response unless
    // there's a backlog of >90 unrelated updates ahead of it (rare; on
    // restart we already flush pending updates above). Albums split across
    // two batches in such backlog conditions are processed as separate
    // shorter dispatches — no timing heuristic involved.
    //
    // Per-chat FIFO workers: each chat has one long-lived task that pulls
    // units in arrival order, so strict ordering holds across batches as
    // well as within them. Workers are reused across reconnects.
    //
    // Outer reconnect loop: catches panics inside the polling task and
    // retries with exponential backoff. A `PollingExit::Fatal` from
    // `polling_loop` (Conflict, Unauthorized) breaks out instead — those
    // conditions cannot recover via reconnect.
    let chat_workers: ChatWorkers = new_chat_workers();
    let token_for_loop = token.to_string();
    let username_for_loop = bot_username;
    let mut reconnect_backoff_secs = 5u64;
    // Tracks why the loop exits so the caller can map it to a process
    // exit code. The only break path today is `PollingExit::Fatal`; any
    // future graceful-shutdown signal that breaks the loop without
    // setting this stays `Graceful` by default.
    let mut bot_exit = BotExit::Graceful;

    loop {
        let loop_start = std::time::Instant::now();
        let bot_clone = bot.clone();
        let state_clone = state.clone();
        let workers_clone = chat_workers.clone();
        let token_clone = token_for_loop.clone();
        let username_clone = username_for_loop.clone();

        let polling_handle = tokio::spawn(async move {
            polling_loop(
                bot_clone,
                state_clone,
                workers_clone,
                token_clone,
                username_clone,
            )
            .await
        });
        let _polling_guard = AbortOnDrop(polling_handle.abort_handle());
        let task_result = polling_handle.await;

        match task_result {
            Ok(PollingExit::Fatal(reason)) => {
                eprintln!("  ✗ Bot @{} stopped: {}", username_for_loop, reason);
                eprintln!("    No reconnect — fix the underlying issue and restart cokacdir.");
                msg_debug(&format!("[run_bot] polling exited fatally: {}", reason));
                bot_exit = BotExit::Fatal;
                break;
            }
            Err(e) => {
                // Task panicked — likely network disconnection or runtime issue
                let ran_for = loop_start.elapsed();
                msg_debug(&format!(
                    "[run_bot] polling task crashed after {:.1?}: {}",
                    ran_for, e
                ));
                println!(
                    "  ⚠ Bot disconnected — reconnecting in {}s...",
                    reconnect_backoff_secs
                );
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

    // Tear down per-chat workers. Clearing the map drops each
    // `ChatWorkerEntry`, which (a) drops the stored `tx` so workers waiting
    // on `recv().await` observe channel closure and exit, and (b) fires
    // `abort()` on the worker's outer task via `ChatWorkerEntry::Drop` —
    // necessary to wake a worker stuck on `join.await` for an in-flight
    // unit. The per-unit handler is spawned as a separate child task whose
    // `JoinHandle` drop does NOT abort it, so the handler runs to
    // completion and is never killed mid-execution. Any cancel_token left
    // in the map by a panicked handler whose worker was aborted before its
    // match arm could reclaim it is reaped by `_run_bot_cleanup`'s Drop.
    chat_workers.lock().await.clear();
    scheduler_handle.abort();
    bot_exit
}

/// Long-poll `getUpdates` and dispatch each batch via `process_batch`.
/// Returns `PollingExit::Fatal` when Telegram reports a non-recoverable
/// condition (Conflict, Unauthorized) so the outer reconnect wrapper can
/// stop instead of looping forever. Transient errors (network blips,
/// 5xx) sleep with bounded exponential backoff and retry inline; a
/// `RetryAfter(s)` is honored verbatim for `s` seconds.
async fn polling_loop(
    bot: Bot,
    state: SharedState,
    workers: ChatWorkers,
    token: String,
    bot_username: String,
) -> PollingExit {
    let mut offset: i32 = 0;
    let mut transient_backoff_ms: u64 = 500;
    loop {
        let result = bot
            .get_updates()
            .offset(offset)
            .timeout(30)
            .limit(100)
            .await;
        match result {
            Ok(updates) => {
                transient_backoff_ms = 500;
                if let Some(last) = updates.last() {
                    // Match the conversion in the flush logic above so we
                    // stay compatible with teloxide's UpdateId type.
                    offset = next_offset_after(last.id.0);
                }
                if updates.is_empty() {
                    continue;
                }
                msg_debug(&format!(
                    "[polling_loop] batch: {} update(s)",
                    updates.len()
                ));
                process_batch(&bot, updates, &state, &workers, &token, &bot_username).await;
            }
            Err(e) => {
                // Misconfiguration: bail out so the outer wrapper doesn't
                // loop forever on a condition that retrying cannot fix.
                if let Some(reason) = detect_fatal_polling_error(&e) {
                    return PollingExit::Fatal(format!("{}: {}", reason, redact_err(&e)));
                }
                // Server-mandated cooldown is honored verbatim. Reset our
                // own backoff so we don't compound it on the next loop
                // (the server told us exactly how long to wait).
                let (sleep_dur, is_retry_after) = match &e {
                    teloxide::RequestError::RetryAfter(s) => (s.duration(), true),
                    _ => (
                        std::time::Duration::from_millis(transient_backoff_ms),
                        false,
                    ),
                };
                msg_debug(&format!(
                    "[polling_loop] getUpdates error (sleeping {}ms{}): {}",
                    sleep_dur.as_millis(),
                    if is_retry_after { ", RetryAfter" } else { "" },
                    redact_err(&e),
                ));
                tokio::time::sleep(sleep_dur).await;
                if is_retry_after {
                    transient_backoff_ms = 500;
                } else {
                    // Cap at 10s; outer reconnect handles longer outages via panic path.
                    transient_backoff_ms = (transient_backoff_ms * 2).min(10_000);
                }
            }
        }
    }
}

/// Group album members by `(chat_id, media_group_id)` within this batch and
/// hand each unit off to its chat's serial worker. Albums of size ≥2 go to
/// `handle_album_batch` for atomic processing; singletons (no
/// media_group_id, or 1-photo album fragments from a split batch) fall
/// through to the existing `handle_message` path.
///
/// Strict arrival order is preserved per chat **across** batches: each
/// chat has a long-lived FIFO worker, and we only push into the FIFO here
/// — the worker itself awaits each unit before pulling the next, so two
/// messages from the same chat in different batches cannot race for
/// `state.lock()` either. Different chats keep running in parallel.
async fn process_batch(
    bot: &Bot,
    updates: Vec<Update>,
    state: &SharedState,
    workers: &ChatWorkers,
    token: &str,
    bot_username: &str,
) {
    // Insertion-ordered list of dispatch units. An album spanning interleaved
    // updates appears at the position of its first photo.
    let mut units: Vec<DispatchUnit> = Vec::new();
    let mut album_pos: HashMap<(ChatId, String), usize> = HashMap::new();

    for upd in updates {
        let UpdateKind::Message(msg) = upd.kind else {
            // Bot only handles fresh messages today; ignore edited messages,
            // callback queries, channel posts, etc. (matches prior `repl`
            // behaviour, which used `Update::filter_message`.)
            continue;
        };
        let chat_id = msg.chat.id;
        if let Some(gid) = msg.media_group_id() {
            let key = (chat_id, gid.to_string());
            match album_pos.get(&key) {
                Some(&idx) => {
                    if let DispatchUnit::Album(ref mut msgs) = units[idx] {
                        msgs.push(msg);
                    }
                }
                None => {
                    let idx = units.len();
                    album_pos.insert(key, idx);
                    units.push(DispatchUnit::Album(vec![msg]));
                }
            }
        } else {
            units.push(DispatchUnit::Single(msg));
        }
    }

    // Push each unit into its chat's FIFO worker. We do not await the
    // handler here — that would re-introduce head-of-line blocking across
    // chats. Sending is non-blocking (unbounded mpsc).
    for unit in units {
        let cid = match &unit {
            DispatchUnit::Single(m) => m.chat.id,
            // Album is built with `vec![msg]` and only appended to, so it
            // is never empty here.
            DispatchUnit::Album(msgs) => msgs[0].chat.id,
        };
        if let DispatchUnit::Album(ref msgs) = unit {
            if msgs.len() >= 2 {
                msg_debug(&format!(
                    "[process_batch] atomic album: {} attachment(s) in one batch",
                    msgs.len()
                ));
            }
        }
        let tx = ensure_chat_worker(workers, cid, bot, state, token, bot_username).await;
        if let Err(send_err) = tx.send(unit) {
            // Worker exited (only possible if its sender was dropped from
            // the map). Recreate exactly once and retry; a second failure
            // is logged and dropped rather than spinning.
            msg_debug(&format!(
                "[process_batch] chat_worker for {} closed; recreating",
                cid.0
            ));
            workers.lock().await.remove(&cid);
            let new_tx = ensure_chat_worker(workers, cid, bot, state, token, bot_username).await;
            if let Err(e) = new_tx.send(send_err.0) {
                msg_debug(&format!(
                    "[process_batch] chat {} send failed even after recreate: {:?}",
                    cid.0, e
                ));
            }
        }
    }
}

#[derive(Clone)]
struct SttAudioUpload {
    file_id: String,
    file_name: String,
    file_size_hint: u32,
}

fn audio_extension_from_mime(mime: Option<&str>) -> &'static str {
    match mime.unwrap_or("").to_ascii_lowercase().as_str() {
        "audio/aac" => "aac",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/m4a" | "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/webm" => "webm",
        _ => "audio",
    }
}

fn is_audio_file_name(name: &str) -> bool {
    let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "aac"
            | "aif"
            | "aiff"
            | "amr"
            | "caf"
            | "flac"
            | "m4a"
            | "mp3"
            | "oga"
            | "ogg"
            | "opus"
            | "wav"
            | "webm"
            | "wma"
    )
}

fn stt_audio_upload_info(msg: &Message) -> Option<SttAudioUpload> {
    if let Some(voice) = msg.voice() {
        let mime = voice.mime_type.as_ref().map(|m| m.essence_str());
        return Some(SttAudioUpload {
            file_id: voice.file.id.clone(),
            file_name: format!(
                "voice_{}.{}",
                voice.file.unique_id,
                audio_extension_from_mime(mime).trim_start_matches('.')
            ),
            file_size_hint: voice.file.size,
        });
    }
    if let Some(audio) = msg.audio() {
        let mime = audio.mime_type.as_ref().map(|m| m.essence_str());
        let file_name = audio.file_name.clone().unwrap_or_else(|| {
            format!(
                "audio_{}.{}",
                audio.file.unique_id,
                audio_extension_from_mime(mime)
            )
        });
        return Some(SttAudioUpload {
            file_id: audio.file.id.clone(),
            file_name,
            file_size_hint: audio.file.size,
        });
    }
    if let Some(doc) = msg.document() {
        let mime = doc.mime_type.as_ref().map(|m| m.essence_str());
        let mime_is_audio = mime
            .map(|m| m.to_ascii_lowercase().starts_with("audio/"))
            .unwrap_or(false);
        let name_is_audio = doc
            .file_name
            .as_deref()
            .map(is_audio_file_name)
            .unwrap_or(false);
        if mime_is_audio || name_is_audio {
            let file_name = doc.file_name.clone().unwrap_or_else(|| {
                format!(
                    "audio_{}.{}",
                    doc.file.unique_id,
                    audio_extension_from_mime(mime)
                )
            });
            return Some(SttAudioUpload {
                file_id: doc.file.id.clone(),
                file_name,
                file_size_hint: doc.file.size,
            });
        }
    }
    None
}

fn transcriptor_bin_path() -> Result<PathBuf, String> {
    let Some(home) = dirs::home_dir() else {
        return Err("Cannot determine home directory.".to_string());
    };
    let binary_name = if cfg!(windows) {
        "transcriptor.exe"
    } else {
        "transcriptor"
    };
    Ok(home.join(".cokacdir").join("bin").join(binary_name))
}

fn transcriptor_artifact_name() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "aarch64") => Ok("transcriptor-linux-aarch64"),
        ("linux", "x86_64") => Ok("transcriptor-linux-x86_64"),
        ("macos", "aarch64") => Ok("transcriptor-macos-aarch64"),
        ("macos", "x86_64") => Ok("transcriptor-macos-x86_64"),
        ("windows", "aarch64") => Ok("transcriptor-windows-aarch64.exe"),
        ("windows", "x86_64") => Ok("transcriptor-windows-x86_64.exe"),
        (os, arch) => Err(format!("Unsupported transcriptor target: {os}-{arch}")),
    }
}

fn transcriptor_binary_is_present(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn stt_file_debug_summary(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(meta) => format!(
            "path={} exists=true file={} len={} readonly={}",
            path.display(),
            meta.is_file(),
            meta.len(),
            meta.permissions().readonly()
        ),
        Err(e) => format!("path={} exists=false error={}", path.display(), e),
    }
}

fn stt_env_debug_summary() -> String {
    fn env_state(name: &str, path_like: bool) -> String {
        match std::env::var(name) {
            Ok(value) if value.is_empty() => format!("{name}=empty"),
            Ok(value) if path_like => {
                let path = Path::new(&value);
                format!(
                    "{}=set(len={}, exists={}, is_file={}, is_dir={})",
                    name,
                    value.len(),
                    path.exists(),
                    path.is_file(),
                    path.is_dir()
                )
            }
            Ok(value) => format!("{name}=set({})", truncate_str(&value, 80)),
            Err(std::env::VarError::NotPresent) => format!("{name}=unset"),
            Err(std::env::VarError::NotUnicode(_)) => format!("{name}=not_unicode"),
        }
    }

    [
        env_state("TRANSCRIPTOR_MODEL", true),
        env_state("TRANSCRIPTOR_HOME", true),
        env_state("TRANSCRIPTOR_LANGUAGE", false),
        env_state("TRANSCRIPTOR_THREADS", false),
    ]
    .join(", ")
}

fn stt_stdout_shape(stdout: &str) -> String {
    let trimmed = stdout.trim();
    let first = trimmed.chars().next();
    format!(
        "trimmed_len={}, first_non_ws={:?}, jsonish={}",
        trimmed.len(),
        first,
        matches!(first, Some('{') | Some('['))
    )
}

fn stt_stderr_preview(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        "<empty>".to_string()
    } else {
        truncate_str(trimmed, 500)
    }
}

fn make_transcriptor_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)
            .map_err(|e| format!("Failed to stat transcriptor: {e}"))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)
            .map_err(|e| format!("Failed to set transcriptor executable permission: {e}"))?;
    }
    let _ = path;
    Ok(())
}

struct TranscriptorBinaryFileLock {
    _file: std::fs::File,
}

fn transcriptor_lock_path(path: &Path) -> Result<PathBuf, String> {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err("Invalid transcriptor lock path.".to_string());
    };
    Ok(path.with_file_name(format!("{file_name}.lock")))
}

async fn acquire_transcriptor_binary_mutex(
    cancel_token: Option<&Arc<CancelToken>>,
    trace_id: &str,
) -> Result<tokio::sync::MutexGuard<'static, ()>, String> {
    let mutex = TRANSCRIPTOR_BINARY_MUTEX.get_or_init(|| Mutex::new(()));
    let lock_fut = mutex.lock();
    tokio::pin!(lock_fut);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            guard = &mut lock_fut => {
                msg_debug(&format!("[stt:{trace_id}:bin] in-process install lock acquired"));
                return Ok(guard);
            }
            _ = interval.tick() => {
                if stt_cancelled(cancel_token) {
                    msg_debug(&format!("[stt:{trace_id}:bin] cancelled while waiting for in-process install lock"));
                    return Err(stt_cancelled_error());
                }
            }
        }
    }
}

async fn acquire_transcriptor_binary_file_lock(
    path: &Path,
    cancel_token: Option<&Arc<CancelToken>>,
    trace_id: &str,
) -> Result<TranscriptorBinaryFileLock, String> {
    use fs2::FileExt;

    let lock_path = transcriptor_lock_path(path)?;
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create transcriptor lock directory: {e}"))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("Failed to open transcriptor install lock: {e}"))?;

    let mut waited = false;
    loop {
        if stt_cancelled(cancel_token) {
            msg_debug(&format!(
                "[stt:{trace_id}:bin] cancelled while waiting for file install lock"
            ));
            return Err(stt_cancelled_error());
        }
        match file.try_lock_exclusive() {
            Ok(()) => {
                if waited {
                    msg_debug(&format!(
                        "[stt:{trace_id}:bin] file install lock acquired after waiting path={}",
                        lock_path.display()
                    ));
                } else {
                    msg_debug(&format!(
                        "[stt:{trace_id}:bin] file install lock acquired path={}",
                        lock_path.display()
                    ));
                }
                return Ok(TranscriptorBinaryFileLock { _file: file });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if !waited {
                    msg_debug(&format!(
                        "[stt:{trace_id}:bin] file install lock contended path={} err={}",
                        lock_path.display(),
                        truncate_str(&e.to_string(), 300)
                    ));
                    waited = true;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            }
            Err(e) => {
                return Err(format!("Failed to lock transcriptor install lock: {e}"));
            }
        }
    }
}

async fn transcriptor_binary_supports_progress(path: &Path, trace_id: &str) -> bool {
    let mut cmd = tokio::process::Command::new(path);
    cmd.arg("--help")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output =
        match tokio::time::timeout(tokio::time::Duration::from_secs(10), cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                msg_debug(&format!(
                    "[stt:{trace_id}:bin] capability check failed to run --help: {}",
                    truncate_str(&e.to_string(), 300)
                ));
                return false;
            }
            Err(_) => {
                msg_debug(&format!("[stt:{trace_id}:bin] capability check timed out"));
                return false;
            }
        };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    let supports = combined.contains("--progress");
    msg_debug(&format!(
        "[stt:{trace_id}:bin] capability check status={} supports_progress={} help_preview={:?}",
        output.status,
        supports,
        truncate_str(combined.trim(), 500)
    ));
    supports
}

async fn ensure_transcriptor_binary(
    cancel_token: Option<&Arc<CancelToken>>,
    trace_id: &str,
) -> Result<PathBuf, String> {
    if stt_cancelled(cancel_token) {
        msg_debug(&format!("[stt:{trace_id}:bin] cancelled before ensure"));
        return Err(stt_cancelled_error());
    }
    let artifact = transcriptor_artifact_name()?;
    let path = transcriptor_bin_path()?;
    msg_debug(&format!(
        "[stt:{trace_id}:bin] ensure start artifact={}, path={}",
        artifact,
        path.display()
    ));
    let Some(bin_dir) = path.parent() else {
        return Err("Invalid transcriptor path.".to_string());
    };
    fs::create_dir_all(bin_dir)
        .map_err(|e| format!("Failed to create transcriptor bin directory: {e}"))?;

    let _in_process_lock = acquire_transcriptor_binary_mutex(cancel_token, trace_id).await?;
    let _file_lock = acquire_transcriptor_binary_file_lock(&path, cancel_token, trace_id).await?;
    if stt_cancelled(cancel_token) {
        msg_debug(&format!(
            "[stt:{trace_id}:bin] cancelled after install lock"
        ));
        return Err(stt_cancelled_error());
    }

    if transcriptor_binary_is_present(&path) {
        msg_debug(&format!(
            "[stt:{trace_id}:bin] existing binary {}",
            stt_file_debug_summary(&path)
        ));
        make_transcriptor_executable(&path)?;
        if transcriptor_binary_supports_progress(&path, trace_id).await {
            msg_debug(&format!("[stt:{trace_id}:bin] existing binary accepted"));
            return Ok(path);
        }
        msg_debug(&format!(
            "[stt:{trace_id}:bin] existing binary lacks --progress support, replacing"
        ));
    }

    if let Ok(meta) = fs::metadata(&path) {
        if meta.is_file() && meta.len() == 0 {
            msg_debug(&format!(
                "[stt:{trace_id}:bin] removing zero-byte binary before download"
            ));
            match fs::remove_file(&path) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!(
                        "Failed to remove zero-byte transcriptor binary: {e}"
                    ))
                }
            }
        }
    }

    let url =
        format!("https://raw.githubusercontent.com/kstost/transcriptor/main/dist_beta/{artifact}");
    msg_debug(&format!(
        "[stt:{trace_id}:bin] downloading artifact={} timeout_secs={} max_bytes={}",
        artifact, TRANSCRIPTOR_DOWNLOAD_TIMEOUT_SECS, TRANSCRIPTOR_MAX_BYTES
    ));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            TRANSCRIPTOR_DOWNLOAD_TIMEOUT_SECS,
        ))
        .build()
        .map_err(|e| format!("Failed to create transcriptor downloader: {e}"))?;
    let mut resp = await_stt_or_cancel(client.get(&url).send(), cancel_token)
        .await?
        .map_err(|e| format!("Failed to download transcriptor: {e}"))?;
    msg_debug(&format!(
        "[stt:{trace_id}:bin] download response status={} content_length={:?}",
        resp.status(),
        resp.content_length()
    ));
    if !resp.status().is_success() {
        return Err(format!(
            "Failed to download transcriptor: HTTP {}",
            resp.status()
        ));
    }
    if let Some(len) = resp.content_length() {
        if len > TRANSCRIPTOR_MAX_BYTES {
            return Err(format!(
                "Downloaded transcriptor is too large ({:.1}MB).",
                len as f64 / (1024.0 * 1024.0)
            ));
        }
    }

    let tmp_path = bin_dir.join(format!(
        ".transcriptor.download.{:08x}",
        rand::random::<u32>()
    ));
    let write_result: Result<u64, String> = async {
        use std::io::Write;
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| format!("Failed to create transcriptor temp file: {e}"))?;
        let mut hasher = Sha256::new();
        let mut downloaded = 0u64;
        loop {
            let Some(chunk) = await_stt_or_cancel(resp.chunk(), cancel_token)
                .await?
                .map_err(|e| format!("Failed to read transcriptor download: {e}"))?
            else {
                break;
            };
            downloaded = downloaded
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| "Transcriptor download size overflow.".to_string())?;
            if downloaded > TRANSCRIPTOR_MAX_BYTES {
                return Err(format!(
                    "Downloaded transcriptor is too large ({:.1}MB).",
                    downloaded as f64 / (1024.0 * 1024.0)
                ));
            }
            hasher.update(&chunk);
            out.write_all(&chunk)
                .map_err(|e| format!("Failed to write transcriptor download: {e}"))?;
        }
        if downloaded == 0 {
            return Err("Downloaded transcriptor is empty.".to_string());
        }
        let actual_sha256 = format!("{:x}", hasher.finalize());
        msg_debug(&format!(
            "[stt:{trace_id}:bin] download completed bytes={} actual_sha256={}",
            downloaded, actual_sha256
        ));
        out.flush()
            .map_err(|e| format!("Failed to flush transcriptor download: {e}"))?;
        Ok(downloaded)
    }
    .await;
    let downloaded = match write_result {
        Ok(downloaded) => downloaded,
        Err(e) => {
            msg_debug(&format!(
                "[stt:{trace_id}:bin] download/write failed: {}",
                truncate_str(&e, 500)
            ));
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
    };
    if stt_cancelled(cancel_token) {
        msg_debug(&format!(
            "[stt:{trace_id}:bin] cancelled after download bytes={downloaded}"
        ));
        let _ = fs::remove_file(&tmp_path);
        return Err(stt_cancelled_error());
    }
    if let Err(e) = make_transcriptor_executable(&tmp_path) {
        msg_debug(&format!(
            "[stt:{trace_id}:bin] chmod failed: {}",
            truncate_str(&e, 500)
        ));
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    if path.exists() {
        match fs::remove_file(&path) {
            Ok(_) => msg_debug(&format!(
                "[stt:{trace_id}:bin] removed old binary before install"
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                msg_debug(&format!(
                    "[stt:{trace_id}:bin] failed to remove old binary before install: {}",
                    truncate_str(&e.to_string(), 500)
                ));
                let _ = fs::remove_file(&tmp_path);
                return Err(format!("Failed to replace old transcriptor binary: {e}"));
            }
        }
    }

    match fs::rename(&tmp_path, &path) {
        Ok(_) => {}
        Err(e) => {
            if transcriptor_binary_is_present(&path) {
                msg_debug(&format!(
                    "[stt:{trace_id}:bin] install rename failed but destination exists: {}",
                    truncate_str(&e.to_string(), 500)
                ));
                let _ = fs::remove_file(&tmp_path);
            } else {
                let _ = fs::remove_file(&tmp_path);
                return Err(format!("Failed to install transcriptor: {e}"));
            }
        }
    }
    make_transcriptor_executable(&path)?;
    msg_debug(&format!(
        "[stt:{trace_id}:bin] installed binary bytes={} {}",
        downloaded,
        stt_file_debug_summary(&path)
    ));
    if !transcriptor_binary_supports_progress(&path, trace_id).await {
        return Err("Downloaded transcriptor does not support progress events.".to_string());
    }
    Ok(path)
}

fn stt_temp_dir() -> Result<PathBuf, String> {
    let Some(home) = dirs::home_dir() else {
        return Err("Cannot determine home directory.".to_string());
    };
    Ok(home.join(".cokacdir").join("stttemp"))
}

fn cleanup_stt_temp_dir(dir: &Path) {
    const TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if now
            .duration_since(modified)
            .map(|age| age > TTL)
            .unwrap_or(false)
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn safe_audio_temp_extension(file_name: &str) -> Option<String> {
    let ext = Path::new(file_name)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    if ext.len() <= 12 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(ext)
    } else {
        None
    }
}

fn stt_temp_path_for(file_name: &str) -> Result<PathBuf, String> {
    let dir = stt_temp_dir()?;
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create STT temp directory: {e}"))?;
    cleanup_stt_temp_dir(&dir);
    let stamp = chrono::Local::now().timestamp_millis();
    let rand = rand::random::<u32>();
    let name = match safe_audio_temp_extension(file_name) {
        Some(ext) => format!("stt_{stamp}_{rand:08x}.{ext}"),
        None => format!("stt_{stamp}_{rand:08x}.audio"),
    };
    Ok(dir.join(name))
}

fn stt_audio_too_large_message(bytes: u64) -> String {
    format!(
        "Audio file too large ({:.1}MB). Maximum STT size is {:.0}MB.",
        bytes as f64 / (1024.0 * 1024.0),
        STT_AUDIO_MAX_BYTES as f64 / (1024.0 * 1024.0)
    )
}

fn stt_cancelled(cancel_token: Option<&Arc<CancelToken>>) -> bool {
    cancel_token
        .map(|token| token.cancelled.load(Ordering::Relaxed))
        .unwrap_or(false)
}

fn stt_cancelled_error() -> String {
    "Audio transcription cancelled.".to_string()
}

async fn await_stt_or_cancel<F, T>(
    future: F,
    cancel_token: Option<&Arc<CancelToken>>,
) -> Result<T, String>
where
    F: std::future::IntoFuture<Output = T>,
{
    let future = future.into_future();
    let Some(token) = cancel_token else {
        return Ok(future.await);
    };
    if stt_cancelled(Some(token)) {
        return Err(stt_cancelled_error());
    }

    tokio::pin!(future);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            output = &mut future => return Ok(output),
            _ = interval.tick() => {
                if stt_cancelled(Some(token)) {
                    return Err(stt_cancelled_error());
                }
            }
        }
    }
}

async fn download_stt_audio_to_temp(
    bot: &Bot,
    chat_id: ChatId,
    info: &SttAudioUpload,
    state: &SharedState,
    cancel_token: Option<&Arc<CancelToken>>,
    trace_id: &str,
) -> Result<PathBuf, String> {
    let file_ext = safe_audio_temp_extension(&info.file_name).unwrap_or_else(|| "none".to_string());
    msg_debug(&format!(
        "[stt:{trace_id}:download] start chat_id={} file_name_len={} ext={} size_hint={} max_bytes={}",
        chat_id.0,
        info.file_name.len(),
        file_ext,
        info.file_size_hint,
        STT_AUDIO_MAX_BYTES
    ));
    if stt_cancelled(cancel_token) {
        msg_debug(&format!("[stt:{trace_id}:download] cancelled before start"));
        return Err(stt_cancelled_error());
    }
    if info.file_size_hint != u32::MAX
        && u64::from(info.file_size_hint) > STT_AUDIO_MAX_BYTES as u64
    {
        msg_debug(&format!(
            "[stt:{trace_id}:download] rejected by size hint bytes={}",
            info.file_size_hint
        ));
        return Err(stt_audio_too_large_message(u64::from(info.file_size_hint)));
    }

    shared_rate_limit_wait(state, chat_id).await;
    if stt_cancelled(cancel_token) {
        msg_debug(&format!(
            "[stt:{trace_id}:download] cancelled after rate-limit wait"
        ));
        return Err(stt_cancelled_error());
    }
    let file_result = await_stt_or_cancel(bot.get_file(&info.file_id), cancel_token).await?;
    let file = match tg!("get_file", file_result) {
        Ok(file) => file,
        Err(e) => {
            msg_debug(&format!(
                "[stt:{trace_id}:download] get_file failed: {}",
                redact_err(&e)
            ));
            return Err(format!("getFile failed: {}", redact_err(&e)));
        }
    };
    msg_debug(&format!(
        "[stt:{trace_id}:download] get_file ok telegram_path_len={}",
        file.path.len()
    ));
    let base = {
        let data = state.lock().await;
        data.api_base_url.clone()
    };
    let token = bot.token().to_string();
    let url = format!("{}/file/bot{}/{}", base, token, file.path);
    let scrub =
        |e: reqwest::Error| -> String { e.to_string().replace(&token, "<bot_token_redacted>") };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(STT_DOWNLOAD_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Failed to create audio downloader: {e}"))?;
    let mut resp = await_stt_or_cancel(client.get(&url).send(), cancel_token)
        .await?
        .map_err(|e| format!("Audio download failed: {}", scrub(e)))?;
    msg_debug(&format!(
        "[stt:{trace_id}:download] http response status={} content_length={:?}",
        resp.status(),
        resp.content_length()
    ));
    if !resp.status().is_success() {
        return Err(format!("Audio download failed: HTTP {}", resp.status()));
    }
    if let Some(len) = resp.content_length() {
        if len > STT_AUDIO_MAX_BYTES as u64 {
            return Err(stt_audio_too_large_message(len));
        }
    }
    if stt_cancelled(cancel_token) {
        msg_debug(&format!(
            "[stt:{trace_id}:download] cancelled before temp write"
        ));
        return Err(stt_cancelled_error());
    }

    let temp_path = stt_temp_path_for(&info.file_name)?;
    msg_debug(&format!(
        "[stt:{trace_id}:download] temp path allocated {}",
        stt_file_debug_summary(&temp_path)
    ));
    let write_result: Result<u64, String> = async {
        use std::io::Write;
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|e| format!("Failed to create STT temp file: {e}"))?;
        let mut downloaded = 0u64;
        loop {
            let Some(chunk) = await_stt_or_cancel(resp.chunk(), cancel_token)
                .await?
                .map_err(|e| format!("Audio download read failed: {}", scrub(e)))?
            else {
                break;
            };
            downloaded = downloaded
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| "Audio download size overflow.".to_string())?;
            if downloaded > STT_AUDIO_MAX_BYTES as u64 {
                return Err(stt_audio_too_large_message(downloaded));
            }
            if stt_cancelled(cancel_token) {
                msg_debug(&format!(
                    "[stt:{trace_id}:download] cancelled while writing downloaded={}",
                    downloaded
                ));
                return Err(stt_cancelled_error());
            }
            out.write_all(&chunk)
                .map_err(|e| format!("Failed to write STT temp file: {e}"))?;
        }
        if downloaded == 0 {
            return Err("Audio download produced an empty file.".to_string());
        }
        out.flush()
            .map_err(|e| format!("Failed to flush STT temp file: {e}"))?;
        Ok(downloaded)
    }
    .await;
    let downloaded = match write_result {
        Ok(downloaded) => downloaded,
        Err(e) => {
            msg_debug(&format!(
                "[stt:{trace_id}:download] write failed: {}",
                truncate_str(&e, 500)
            ));
            let _ = fs::remove_file(&temp_path);
            return Err(e);
        }
    };
    msg_debug(&format!(
        "[stt:{trace_id}:download] completed bytes={} {}",
        downloaded,
        stt_file_debug_summary(&temp_path)
    ));
    Ok(temp_path)
}

struct SttProgressReporter {
    bot: Bot,
    chat_id: ChatId,
    state: SharedState,
    progress_message_id: Option<teloxide::types::MessageId>,
    context: String,
    last_text: String,
    last_edit: Option<tokio::time::Instant>,
    last_download_percent: Option<f64>,
    model_load_started_at: Option<tokio::time::Instant>,
    model_load_notice_shown: bool,
}

impl SttProgressReporter {
    fn new(
        bot: Bot,
        chat_id: ChatId,
        state: SharedState,
        progress_message_id: Option<teloxide::types::MessageId>,
        context: String,
    ) -> Self {
        Self {
            bot,
            chat_id,
            state,
            progress_message_id,
            context,
            last_text: STT_PROGRESS_TEXT.to_string(),
            last_edit: None,
            last_download_percent: None,
            model_load_started_at: None,
            model_load_notice_shown: false,
        }
    }

    async fn set_text(&mut self, text: String, force: bool) -> bool {
        let Some(message_id) = self.progress_message_id else {
            return false;
        };
        if text == self.last_text {
            return false;
        }
        let now = tokio::time::Instant::now();
        if !force {
            if let Some(last) = self.last_edit {
                if now.duration_since(last) < tokio::time::Duration::from_secs(5) {
                    return false;
                }
            }
        }

        shared_rate_limit_wait(&self.state, self.chat_id).await;
        match tg!(
            "edit_message",
            &self.state,
            self.chat_id,
            self.bot
                .edit_message_text(self.chat_id, message_id, &text)
                .await
        ) {
            Ok(_) => {
                msg_debug(&format!(
                    "[stt:{}] chat_id={}, progress event edit id={} text={:?}",
                    self.context,
                    self.chat_id.0,
                    message_id.0,
                    truncate_str(&text, 160)
                ));
                self.last_text = text;
                self.last_edit = Some(tokio::time::Instant::now());
                true
            }
            Err(e) => {
                msg_debug(&format!(
                    "[stt:{}] chat_id={}, progress event edit failed id={}: {}",
                    self.context,
                    self.chat_id.0,
                    message_id.0,
                    redact_err(&e)
                ));
                false
            }
        }
    }

    async fn handle_event(&mut self, event: &serde_json::Value) {
        let Some(name) = event.get("event").and_then(|v| v.as_str()) else {
            return;
        };
        match name {
            "model_download_required" | "model_download_started" => {
                self.last_download_percent = None;
                let text = stt_download_progress_text(event)
                    .unwrap_or_else(|| "Downloading speech model..".to_string());
                let edited = self.set_text(text, true).await;
                if edited {
                    self.last_download_percent = event.get("percent").and_then(|v| v.as_f64());
                }
            }
            "model_download_progress" => {
                let percent = event.get("percent").and_then(|v| v.as_f64());
                if let Some(percent) = percent {
                    let should_try = self
                        .last_download_percent
                        .map(|last| percent >= 100.0 || percent - last >= 5.0)
                        .unwrap_or(true);
                    if !should_try {
                        return;
                    }
                }
                let text = stt_download_progress_text(event)
                    .unwrap_or_else(|| "Downloading speech model..".to_string());
                let force = percent.map(|p| p >= 100.0).unwrap_or(false);
                let edited = self.set_text(text, force).await;
                if edited {
                    self.last_download_percent = percent;
                }
            }
            "model_download_finished" => {
                let text = stt_download_progress_text(event)
                    .unwrap_or_else(|| "Downloading speech model: 100%".to_string());
                let edited = self.set_text(text, true).await;
                if edited {
                    self.last_download_percent = Some(100.0);
                }
            }
            "model_load_started" => {
                self.model_load_started_at = Some(tokio::time::Instant::now());
                self.model_load_notice_shown = false;
                msg_debug(&format!(
                    "[stt:{}] chat_id={}, model load started; delaying visible notice by {}s",
                    self.context,
                    self.chat_id.0,
                    STT_MODEL_LOAD_NOTICE_DELAY.as_secs()
                ));
            }
            "whisper_inference_started" => {
                self.model_load_started_at = None;
                self.model_load_notice_shown = false;
                let _ = self.set_text(STT_PROGRESS_TEXT.to_string(), true).await;
            }
            _ => {}
        }
    }

    async fn maybe_show_delayed_model_load_notice(&mut self) {
        if self.model_load_notice_shown {
            return;
        }
        let Some(started_at) = self.model_load_started_at else {
            return;
        };
        if tokio::time::Instant::now().duration_since(started_at) < STT_MODEL_LOAD_NOTICE_DELAY {
            return;
        }
        let _ = self.set_text(STT_MODEL_LOAD_TEXT.to_string(), true).await;
        self.model_load_notice_shown = true;
    }
}

fn stt_download_progress_text(event: &serde_json::Value) -> Option<String> {
    let model = event
        .get("model_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("model");
    let downloaded = event.get("downloaded_bytes").and_then(|v| v.as_u64());
    let total = event.get("total_bytes").and_then(|v| v.as_u64());
    let percent = event.get("percent").and_then(|v| v.as_f64());

    match (downloaded, total, percent) {
        (Some(downloaded), Some(total), Some(percent)) if total > 0 => Some(format!(
            "Downloading speech model {model}: {:.0}% ({:.1}/{:.1} MB)",
            percent,
            downloaded as f64 / (1024.0 * 1024.0),
            total as f64 / (1024.0 * 1024.0)
        )),
        (Some(downloaded), _, _) => Some(format!(
            "Downloading speech model {model}: {:.1} MB",
            downloaded as f64 / (1024.0 * 1024.0)
        )),
        _ => Some(format!("Downloading speech model {model}..")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SttModelArgMode {
    InheritTranscriptorDefaults,
    ExplicitPath,
    ModelNameOverride,
}

fn stt_model_arg_mode(setting: Option<&str>) -> SttModelArgMode {
    let Some(setting) = setting.map(str::trim).filter(|s| !s.is_empty()) else {
        return SttModelArgMode::InheritTranscriptorDefaults;
    };
    if setting.strip_prefix("path:").is_some() {
        SttModelArgMode::ExplicitPath
    } else {
        SttModelArgMode::ModelNameOverride
    }
}

fn apply_stt_model_args(
    cmd: &mut tokio::process::Command,
    setting: Option<&str>,
) -> SttModelArgMode {
    let mode = stt_model_arg_mode(setting);
    match mode {
        SttModelArgMode::InheritTranscriptorDefaults => {}
        SttModelArgMode::ExplicitPath => {
            let setting = setting.unwrap_or("").trim();
            let path = setting.strip_prefix("path:").unwrap_or("").trim();
            cmd.arg("--model").arg(expand_user_path(path));
        }
        SttModelArgMode::ModelNameOverride => {
            let setting = setting.unwrap_or("").trim();
            // transcriptor's documented precedence puts TRANSCRIPTOR_MODEL
            // above --model-name. A per-chat cokacdir model name must therefore
            // suppress only that inherited model-path env var for this child.
            cmd.env_remove("TRANSCRIPTOR_MODEL");
            cmd.arg("--model-name").arg(setting);
        }
    }
    mode
}

async fn run_transcriptor(
    audio_path: &Path,
    stt_model_setting: Option<String>,
    mut progress: Option<SttProgressReporter>,
    cancel_token: Option<Arc<CancelToken>>,
    trace_id: &str,
) -> Result<String, String> {
    msg_debug(&format!(
        "[stt:{trace_id}:run] start audio={} stt_model={} env=[{}]",
        stt_file_debug_summary(audio_path),
        stt_model_setting_display(stt_model_setting.as_deref()),
        stt_env_debug_summary()
    ));
    if stt_cancelled(cancel_token.as_ref()) {
        msg_debug(&format!(
            "[stt:{trace_id}:run] cancelled before binary ensure"
        ));
        return Err(stt_cancelled_error());
    }
    let bin = ensure_transcriptor_binary(cancel_token.as_ref(), trace_id).await?;
    msg_debug(&format!(
        "[stt:{trace_id}:run] binary ready {}",
        stt_file_debug_summary(&bin)
    ));
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("--progress").arg("json");
    let stt_model_arg_mode = apply_stt_model_args(&mut cmd, stt_model_setting.as_deref());
    msg_debug(&format!(
        "[stt:{trace_id}:run] model_arg_mode={:?} transcriptor_model_env_removed={}",
        stt_model_arg_mode,
        stt_model_arg_mode == SttModelArgMode::ModelNameOverride
    ));
    cmd.arg(audio_path);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    claude::attach_cancel_cgroup_tokio(&mut cmd, cancel_token.as_ref());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to run transcriptor: {e}"))?;
    let child_pid = child.id();
    msg_debug(&format!("[stt:{trace_id}:run] spawned pid={:?}", child_pid));
    if let (Some(token), Some(pid)) = (cancel_token.as_ref(), child_pid) {
        let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(pid);
    }
    if stt_cancelled(cancel_token.as_ref()) {
        msg_debug(&format!(
            "[stt:{trace_id}:run] cancelled immediately after spawn pid={:?}",
            child_pid
        ));
        if let Some(token) = cancel_token.as_ref() {
            token.cancel_now();
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture transcriptor stdout.".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture transcriptor stderr.".to_string())?;

    let stdout_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .await
            .map_err(|e| format!("Failed to read transcriptor stdout: {e}"))?;
        Ok::<Vec<u8>, String>(buf)
    });

    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let trace_id_for_stderr = trace_id.to_string();
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stderr).lines();
        let mut stderr_text = String::new();
        while let Some(line) = reader
            .next_line()
            .await
            .map_err(|e| format!("Failed to read transcriptor stderr: {e}"))?
        {
            if !stderr_text.is_empty() {
                stderr_text.push('\n');
            }
            stderr_text.push_str(&line);
            match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(value) => {
                    msg_debug(&format!(
                        "[stt:{trace_id_for_stderr}:progress] {}",
                        truncate_str(&line, 500)
                    ));
                    let _ = progress_tx.send(value);
                }
                Err(_) => {
                    msg_debug(&format!(
                        "[stt:{trace_id_for_stderr}:stderr] {}",
                        truncate_str(&line, 500)
                    ));
                }
            }
        }
        Ok::<String, String>(stderr_text)
    });

    let mut wait_status: Option<std::process::ExitStatus> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut progress_open = true;
    {
        let mut wait_fut = Box::pin(child.wait());
        let mut deadline = Box::pin(tokio::time::sleep(tokio::time::Duration::from_secs(
            STT_TIMEOUT_SECS,
        )));
        let mut cancel_interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
        loop {
            tokio::select! {
                status = &mut wait_fut => {
                    wait_status = Some(status.map_err(|e| format!("Failed to run transcriptor: {e}"))?);
                    break;
                }
                _ = &mut deadline => {
                    timed_out = true;
                    break;
                }
                _ = cancel_interval.tick() => {
                    if stt_cancelled(cancel_token.as_ref()) {
                        cancelled = true;
                        break;
                    }
                    if let Some(progress) = progress.as_mut() {
                        progress.maybe_show_delayed_model_load_notice().await;
                    }
                }
                event = progress_rx.recv(), if progress_open => {
                    match event {
                        Some(event) => {
                            if let Some(progress) = progress.as_mut() {
                                progress.handle_event(&event).await;
                            }
                        }
                        None => {
                            progress_open = false;
                        }
                    }
                }
            }
        }
    }

    if timed_out || cancelled {
        if let Some(token) = cancel_token.as_ref() {
            token.cancel_now();
        } else {
            let _ = child.start_kill();
        }
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(3), child.wait()).await;
        if let Some(token) = cancel_token.as_ref() {
            let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
            if *guard == child_pid {
                *guard = None;
            }
        }
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(3), stdout_task).await;
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(3), stderr_task).await;
        if timed_out {
            msg_debug(&format!(
                "[stt:{trace_id}:run] timeout after {}s pid={:?}",
                STT_TIMEOUT_SECS, child_pid
            ));
            return Err("Audio transcription timed out.".to_string());
        }
        msg_debug(&format!(
            "[stt:{trace_id}:run] cancelled while waiting pid={:?}",
            child_pid
        ));
        return Err(stt_cancelled_error());
    }

    let status = wait_status.ok_or_else(|| "transcriptor exited without status.".to_string())?;
    while let Ok(event) = progress_rx.try_recv() {
        if let Some(progress) = progress.as_mut() {
            progress.handle_event(&event).await;
        }
    }

    let stdout_bytes = stdout_task
        .await
        .map_err(|e| format!("transcriptor stdout task failed: {e}"))??;
    let stderr = stderr_task
        .await
        .map_err(|e| format!("transcriptor stderr task failed: {e}"))??;

    if let Some(token) = cancel_token.as_ref() {
        let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == child_pid {
            *guard = None;
        }
    }
    if stt_cancelled(cancel_token.as_ref()) {
        msg_debug(&format!(
            "[stt:{trace_id}:run] cancelled after process output pid={:?}",
            child_pid
        ));
        return Err(stt_cancelled_error());
    }

    let stdout = String::from_utf8_lossy(&stdout_bytes);
    msg_debug(&format!(
        "[stt:{trace_id}:run] output status={} code={:?} success={} stdout_bytes={} stdout_sha256={} stdout_shape=[{}] stderr_bytes={} stderr_preview={:?}",
        status,
        status.code(),
        status.success(),
        stdout_bytes.len(),
        sha256_hex_bytes(&stdout_bytes),
        stt_stdout_shape(&stdout),
        stderr.len(),
        stt_stderr_preview(&stderr)
    ));
    if !status.success() {
        msg_debug(&format!(
            "[stt:{trace_id}:run] nonzero exit status={} stderr_preview={:?}",
            status,
            stt_stderr_preview(&stderr)
        ));
        return Err(format!(
            "transcriptor exited with {}: {}",
            status,
            truncate_str(stderr.trim(), 500)
        ));
    }
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| {
            msg_debug(&format!(
                "[stt:{trace_id}:run] json parse failed: error={} stdout_bytes={} stdout_sha256={} stdout_shape=[{}] stderr_preview={:?}",
                e,
                stdout_bytes.len(),
                sha256_hex_bytes(&stdout_bytes),
                stt_stdout_shape(&stdout),
                stt_stderr_preview(&stderr)
            ));
            format!("Invalid transcriptor JSON output: {e}")
        })?;
    let text = value
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        msg_debug(&format!(
            "[stt:{trace_id}:run] json parsed but text empty keys={:?}",
            value
                .as_object()
                .map(|obj| obj.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        ));
        return Err("Transcription produced no text.".to_string());
    }
    msg_debug(&format!(
        "[stt:{trace_id}:run] success transcript_chars={}",
        text.chars().count()
    ));
    Ok(text)
}

async fn transcribe_audio_message(
    bot: &Bot,
    chat_id: ChatId,
    msg: &Message,
    state: &SharedState,
    progress_message_id: Option<teloxide::types::MessageId>,
    cancel_token: Option<Arc<CancelToken>>,
) -> Result<String, String> {
    let trace_id = format!("{:08x}", rand::random::<u32>());
    msg_debug(&format!(
        "[stt:{trace_id}:message] start chat_id={} message_id={}",
        chat_id.0, msg.id.0
    ));
    let Some(info) = stt_audio_upload_info(msg) else {
        msg_debug(&format!(
            "[stt:{trace_id}:message] rejected: message is not audio upload"
        ));
        return Err("Message is not an audio upload.".to_string());
    };
    msg_debug(&format!(
        "[stt:{trace_id}:message] audio detected file_name_len={} size_hint={}",
        info.file_name.len(),
        info.file_size_hint
    ));
    let temp_path =
        download_stt_audio_to_temp(bot, chat_id, &info, state, cancel_token.as_ref(), &trace_id)
            .await?;
    let stt_model_setting = {
        let data = state.lock().await;
        get_stt_model(&data.settings, chat_id)
    };
    let progress = Some(SttProgressReporter::new(
        bot.clone(),
        chat_id,
        state.clone(),
        progress_message_id,
        format!("{trace_id}:progress"),
    ));
    let result = run_transcriptor(
        &temp_path,
        stt_model_setting,
        progress,
        cancel_token,
        &trace_id,
    )
    .await;
    match &result {
        Ok(text) => msg_debug(&format!(
            "[stt:{trace_id}:message] transcriptor ok transcript_chars={}",
            text.chars().count()
        )),
        Err(e) => msg_debug(&format!(
            "[stt:{trace_id}:message] transcriptor error={}",
            truncate_str(e, 500)
        )),
    }
    match fs::remove_file(&temp_path) {
        Ok(_) => msg_debug(&format!(
            "[stt:{trace_id}:message] temp removed path={}",
            temp_path.display()
        )),
        Err(e) => msg_debug(&format!(
            "[stt:{trace_id}:message] temp remove failed path={} error={}",
            temp_path.display(),
            e
        )),
    }
    result
}

fn build_stt_request_text(caption_text: Option<String>, transcripts: &[String]) -> Option<String> {
    let cleaned: Vec<String> = transcripts
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();
    let transcript_block = match cleaned.len() {
        0 => String::new(),
        1 => cleaned[0].clone(),
        _ => cleaned
            .iter()
            .enumerate()
            .map(|(idx, text)| format!("[Audio {}]\n{}", idx + 1, text))
            .collect::<Vec<_>>()
            .join("\n\n"),
    };
    let caption = caption_text
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    match (caption, transcript_block.is_empty()) {
        (Some(c), false) => Some(format!("{c}\n\n[Transcribed audio]\n{transcript_block}")),
        (Some(c), true) => Some(c),
        (None, false) => Some(transcript_block),
        (None, true) => None,
    }
}

fn build_stt_confirmation_text(transcripts: &[String]) -> Option<String> {
    let cleaned: Vec<String> = transcripts
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();

    if cleaned.is_empty() {
        return None;
    }

    Some(
        cleaned
            .iter()
            .map(|text| format!("🗣️ {text}"))
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

fn build_stt_error_text(err: &str) -> String {
    format!("Audio transcription failed: {}", truncate_str(err, 500))
}

async fn send_stt_progress_message(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    context: &str,
) -> Option<teloxide::types::MessageId> {
    shared_rate_limit_wait(state, chat_id).await;
    match tg!(
        "send_message",
        bot.send_message(chat_id, STT_PROGRESS_TEXT).await
    ) {
        Ok(msg) => {
            msg_debug(&format!(
                "[stt:{}] chat_id={}, progress message sent id={}",
                context, chat_id.0, msg.id.0
            ));
            Some(msg.id)
        }
        Err(e) => {
            msg_debug(&format!(
                "[stt:{}] chat_id={}, progress message send failed: {}",
                context,
                chat_id.0,
                redact_err(&e)
            ));
            None
        }
    }
}

async fn edit_stt_progress_or_send(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    progress_message_id: Option<teloxide::types::MessageId>,
    text: &str,
    long_fallback_notice: &str,
    context: &str,
) -> ResponseResult<()> {
    if let Some(message_id) = progress_message_id {
        if text.len() <= TELEGRAM_MSG_LIMIT {
            shared_rate_limit_wait(state, chat_id).await;
            match tg!(
                "edit_message",
                bot.edit_message_text(chat_id, message_id, text).await
            ) {
                Ok(_) => {
                    msg_debug(&format!(
                        "[stt:{}] chat_id={}, progress message edited id={} len={}",
                        context,
                        chat_id.0,
                        message_id.0,
                        text.len()
                    ));
                    return Ok(());
                }
                Err(e) => {
                    msg_debug(&format!(
                        "[stt:{}] chat_id={}, progress message edit failed id={}: {}",
                        context,
                        chat_id.0,
                        message_id.0,
                        redact_err(&e)
                    ));
                }
            }
        } else {
            shared_rate_limit_wait(state, chat_id).await;
            if let Err(e) = tg!(
                "edit_message",
                bot.edit_message_text(chat_id, message_id, long_fallback_notice)
                    .await
            ) {
                msg_debug(&format!(
                    "[stt:{}] chat_id={}, long-result notice edit failed id={}: {}",
                    context,
                    chat_id.0,
                    message_id.0,
                    redact_err(&e)
                ));
            }
        }
    }

    send_long_message(bot, chat_id, text, None, state).await
}

async fn send_stt_confirmation(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    progress_message_id: Option<teloxide::types::MessageId>,
    transcripts: &[String],
) -> ResponseResult<()> {
    if let Some(text) = build_stt_confirmation_text(transcripts) {
        edit_stt_progress_or_send(
            bot,
            chat_id,
            state,
            progress_message_id,
            &text,
            "🗣️ Speech recognized. Sending long result below.",
            "confirmation",
        )
        .await?;
    }
    Ok(())
}

async fn send_stt_error(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    progress_message_id: Option<teloxide::types::MessageId>,
    err: &str,
) -> ResponseResult<()> {
    let text = build_stt_error_text(err);
    edit_stt_progress_or_send(
        bot,
        chat_id,
        state,
        progress_message_id,
        &text,
        "Audio transcription failed. Sending details below.",
        "error",
    )
    .await?;
    Ok(())
}

async fn mark_stt_progress_stopped(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    progress_message_id: Option<teloxide::types::MessageId>,
    context: &str,
) {
    if let Err(e) = edit_stt_progress_or_send(
        bot,
        chat_id,
        state,
        progress_message_id,
        STT_STOPPED_TEXT,
        STT_STOPPED_TEXT,
        context,
    )
    .await
    {
        msg_debug(&format!(
            "[stt:{}] chat_id={}, failed to mark progress stopped: {}",
            context,
            chat_id.0,
            redact_err(&e)
        ));
    }
}

#[cfg(test)]
mod stt_confirmation_tests {
    use std::path::Path;

    use super::{
        build_stt_confirmation_text, normalize_stt_model_setting, stt_model_arg_mode,
        stt_model_setting_display, transcriptor_lock_path, SttModelArgMode,
    };

    #[test]
    fn stt_confirmation_uses_speaking_emoji_with_space() {
        let transcripts = vec![" 안녕하세요 ".to_string()];

        assert_eq!(
            build_stt_confirmation_text(&transcripts).as_deref(),
            Some("🗣️ 안녕하세요")
        );
    }

    #[test]
    fn stt_confirmation_prefixes_each_transcript() {
        let transcripts = vec!["첫 번째".to_string(), "두 번째".to_string()];

        assert_eq!(
            build_stt_confirmation_text(&transcripts).as_deref(),
            Some("🗣️ 첫 번째\n\n🗣️ 두 번째")
        );
    }

    #[test]
    fn stt_model_setting_accepts_model_name() {
        assert_eq!(
            normalize_stt_model_setting("large-v3-turbo").unwrap(),
            Some("large-v3-turbo".to_string())
        );
    }

    #[test]
    fn stt_model_setting_accepts_explicit_path() {
        assert_eq!(
            normalize_stt_model_setting("path:/models/ggml-small.bin").unwrap(),
            Some("path:/models/ggml-small.bin".to_string())
        );
    }

    #[test]
    fn stt_model_setting_reset_clears_override() {
        assert_eq!(normalize_stt_model_setting("reset").unwrap(), None);
    }

    #[test]
    fn stt_model_display_shows_transcriptor_default_when_unset() {
        assert_eq!(
            stt_model_setting_display(None),
            "default (transcriptor env/config/base)"
        );
    }

    #[test]
    fn stt_model_name_uses_model_name_override_mode() {
        assert_eq!(
            stt_model_arg_mode(Some("small")),
            SttModelArgMode::ModelNameOverride
        );
    }

    #[test]
    fn stt_model_path_uses_explicit_path_mode() {
        assert_eq!(
            stt_model_arg_mode(Some("path:/models/ggml-small.bin")),
            SttModelArgMode::ExplicitPath
        );
    }

    #[test]
    fn stt_model_unset_inherits_transcriptor_defaults() {
        assert_eq!(
            stt_model_arg_mode(None),
            SttModelArgMode::InheritTranscriptorDefaults
        );
    }

    #[test]
    fn transcriptor_lock_path_keeps_binary_extension_visible() {
        assert_eq!(
            transcriptor_lock_path(Path::new("/tmp/transcriptor.exe")).unwrap(),
            Path::new("/tmp/transcriptor.exe.lock")
        );
    }
}

async fn cleanup_reserved_stt_slot(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    expected_token: &Arc<CancelToken>,
) {
    let (removed_slot, stop_msg_id) = {
        let mut data = state.lock().await;
        let should_remove = data
            .cancel_tokens
            .get(&chat_id)
            .map(|token| Arc::ptr_eq(token, expected_token))
            .unwrap_or(false);
        if should_remove {
            remove_cancel_token_locked(&mut data, chat_id);
            (true, data.stop_message_ids.remove(&chat_id))
        } else {
            msg_debug(&format!(
                "[stt_cleanup] chat_id={}, token already taken over or removed",
                chat_id.0
            ));
            (false, None)
        }
    };
    if let Some(msg_id) = stop_msg_id {
        shared_rate_limit_wait(state, chat_id).await;
        let _ = tg!("delete_message", bot.delete_message(chat_id, msg_id).await);
    }
    if removed_slot {
        msg_debug(&format!(
            "[queue:trigger] chat_id={}, source=stt_cleanup",
            chat_id.0
        ));
        process_next_queued_message(bot, chat_id, state).await;
    }
}

async fn rollback_album_upload_records(
    state: &SharedState,
    chat_id: ChatId,
    upload_records: &[String],
    reason: &str,
) {
    if upload_records.is_empty() {
        return;
    }

    let (pending_removed, history_removed) = {
        let mut data = state.lock().await;
        let upload_model = get_model(&data.settings, chat_id);
        let provider = detect_provider(upload_model.as_deref());
        let Some(session) = data.sessions.get_mut(&chat_id) else {
            return;
        };
        let pending_removed = session.remove_pending_upload_records(upload_records);
        let history_removed = session.remove_user_history_records(upload_records);
        if pending_removed > 0 || history_removed > 0 {
            if let Some(save_dir) = session.current_path.clone() {
                save_session_to_file(session, &save_dir, provider);
            }
        }
        (pending_removed, history_removed)
    };

    if pending_removed > 0 || history_removed > 0 {
        msg_debug(&format!(
            "[album_batch] chat_id={}, rolled back album upload records after {} (pending={}, history={})",
            chat_id.0, reason, pending_removed, history_removed
        ));
    }
}

async fn process_reserved_stt_message(
    bot: Bot,
    chat_id: ChatId,
    msg: Message,
    state: SharedState,
    user_name: String,
    caption_text: Option<String>,
    cancel_token: Arc<CancelToken>,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    if cancel_token.cancelled.load(Ordering::Relaxed) {
        println!("  [{ts}] ■ [{user_name}] STT cancelled before start");
        cleanup_reserved_stt_slot(&bot, chat_id, &state, &cancel_token).await;
        return;
    }

    let progress_message_id = send_stt_progress_message(&bot, chat_id, &state, "single").await;

    let transcript = match transcribe_audio_message(
        &bot,
        chat_id,
        &msg,
        &state,
        progress_message_id,
        Some(cancel_token.clone()),
    )
    .await
    {
        Ok(text) => text,
        Err(e) => {
            if cancel_token.cancelled.load(Ordering::Relaxed) {
                println!("  [{ts}] ■ [{user_name}] STT stopped");
                mark_stt_progress_stopped(
                    &bot,
                    chat_id,
                    &state,
                    progress_message_id,
                    "single_cancelled",
                )
                .await;
                cleanup_reserved_stt_slot(&bot, chat_id, &state, &cancel_token).await;
                return;
            }
            msg_debug(&format!(
                "[stt:single] chat_id={}, stt failed: {}",
                chat_id.0, e
            ));
            println!("  [{ts}]   ⚠ STT failed: {}", truncate_str(&e, 200));
            let _ = send_stt_error(&bot, chat_id, &state, progress_message_id, &e).await;
            cleanup_reserved_stt_slot(&bot, chat_id, &state, &cancel_token).await;
            return;
        }
    };

    if cancel_token.cancelled.load(Ordering::Relaxed) {
        println!("  [{ts}] ■ [{user_name}] STT stopped");
        mark_stt_progress_stopped(
            &bot,
            chat_id,
            &state,
            progress_message_id,
            "single_cancelled",
        )
        .await;
        cleanup_reserved_stt_slot(&bot, chat_id, &state, &cancel_token).await;
        return;
    }
    println!("  [{ts}] ▶ [{user_name}] STT complete");

    if let Err(e) = send_stt_confirmation(
        &bot,
        chat_id,
        &state,
        progress_message_id,
        std::slice::from_ref(&transcript),
    )
    .await
    {
        msg_debug(&format!(
            "[stt:single] chat_id={}, confirmation failed: {}",
            chat_id.0,
            redact_err(&e)
        ));
        cleanup_reserved_stt_slot(&bot, chat_id, &state, &cancel_token).await;
        return;
    }

    if cancel_token.cancelled.load(Ordering::Relaxed) {
        println!("  [{ts}] ■ [{user_name}] STT stopped");
        mark_stt_progress_stopped(
            &bot,
            chat_id,
            &state,
            progress_message_id,
            "single_cancelled",
        )
        .await;
        cleanup_reserved_stt_slot(&bot, chat_id, &state, &cancel_token).await;
        return;
    }

    let Some(request_text) = build_stt_request_text(caption_text, &[transcript]) else {
        let err = "Transcription produced no text.";
        let _ = send_stt_error(&bot, chat_id, &state, progress_message_id, err).await;
        cleanup_reserved_stt_slot(&bot, chat_id, &state, &cancel_token).await;
        return;
    };

    let result = handle_text_message(&bot, chat_id, &request_text, &state, &user_name, true).await;
    if let Err(e) = result {
        msg_debug(&format!(
            "[stt:single] chat_id={}, dispatch failed: {}",
            chat_id.0,
            redact_err(&e)
        ));
        cleanup_reserved_stt_slot(&bot, chat_id, &state, &cancel_token).await;
    }
}

async fn start_stt_message_task(
    bot: Bot,
    chat_id: ChatId,
    msg: Message,
    state: SharedState,
    user_name: String,
    caption_text: Option<String>,
) -> ResponseResult<()> {
    enum SttAdmission {
        Start {
            request_tasks: RequestTasks,
            cancel_token: Arc<CancelToken>,
            msg: Message,
            user_name: String,
            caption_text: Option<String>,
        },
        Queued {
            id: String,
            preview: String,
        },
        Redirect {
            preview: String,
            replaced: bool,
        },
        QueueFull,
    }

    let dispatch_id = next_dispatch_id();
    let cancel_token = new_cancel_token_for_owner(dispatch_id);
    let admission = {
        let mut data = state.lock().await;
        let request_tasks = data.request_tasks.clone();
        if data.cancel_tokens.contains_key(&chat_id) {
            msg_debug(&format!(
                "[stt:single] chat_id={}, slot busy at admission",
                chat_id.0
            ));
            let qkey = chat_id.0.to_string();
            let qmode = data
                .settings
                .queue
                .get(&qkey)
                .copied()
                .unwrap_or(QUEUE_MODE_DEFAULT);
            let preview = "Audio upload: STT".to_string();
            if qmode {
                let cur_len = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
                if cur_len >= MAX_QUEUE_SIZE {
                    msg_debug(&format!(
                        "[stt:single] chat_id={}, queue FULL ({}/{})",
                        chat_id.0, cur_len, MAX_QUEUE_SIZE
                    ));
                    SttAdmission::QueueFull
                } else {
                    let id = generate_queue_id();
                    let q = data
                        .message_queues
                        .entry(chat_id)
                        .or_insert_with(std::collections::VecDeque::new);
                    q.push_back(QueuedMessage {
                        id: id.clone(),
                        user_display_name: user_name,
                        payload: QueuedPayload::SttAudio { msg, caption_text },
                    });
                    msg_debug(&format!(
                        "[stt:single] chat_id={}, QUEUED id={}, pos={}",
                        chat_id.0,
                        id,
                        q.len()
                    ));
                    SttAdmission::Queued { id, preview }
                }
            } else {
                cancel_in_progress_task_locked(&data, chat_id);
                data.loop_states.remove(&chat_id);
                data.loop_feedback.remove(&chat_id);
                let q = data
                    .message_queues
                    .entry(chat_id)
                    .or_insert_with(std::collections::VecDeque::new);
                let replaced = !q.is_empty();
                q.clear();
                let id = generate_queue_id();
                q.push_back(QueuedMessage {
                    id: id.clone(),
                    user_display_name: user_name,
                    payload: QueuedPayload::SttAudio { msg, caption_text },
                });
                msg_debug(&format!(
                    "[stt:single] chat_id={}, REDIRECT id={}, replaced={}",
                    chat_id.0, id, replaced
                ));
                SttAdmission::Redirect { preview, replaced }
            }
        } else {
            insert_cancel_token_locked(&mut data, chat_id, cancel_token.clone());
            msg_debug(&format!(
                "[stt:single] chat_id={}, reserved AI slot (placeholder)",
                chat_id.0
            ));
            SttAdmission::Start {
                request_tasks,
                cancel_token,
                msg,
                user_name,
                caption_text,
            }
        }
    };

    match admission {
        SttAdmission::Start {
            request_tasks,
            cancel_token,
            msg,
            user_name,
            caption_text,
        } => {
            let state_for_task = state.clone();
            let state_for_monitor = state.clone();
            let join = spawn_tracked_request_task(request_tasks, async move {
                CURRENT_DISPATCH_ID
                    .scope(dispatch_id, async move {
                        process_reserved_stt_message(
                            bot,
                            chat_id,
                            msg,
                            state_for_task,
                            user_name,
                            caption_text,
                            cancel_token,
                        )
                        .await;
                    })
                    .await
            });
            monitor_dispatch_panic(join, state_for_monitor, chat_id, dispatch_id, "stt:single");
        }
        SttAdmission::Queued { id, preview } => {
            shared_rate_limit_wait(&state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, &format!("Queued ({id}) \"{preview}\"\n- /stopall to cancel all\n- /stop_{id} to cancel this"))
                .await)?;
        }
        SttAdmission::Redirect { preview, replaced } => {
            shared_rate_limit_wait(&state, chat_id).await;
            let msg = if replaced {
                format!("🔄 Redirect target updated: \"{preview}\"")
            } else {
                format!("🔄 Cancelling current task, will process: \"{preview}\"")
            };
            tg!("send_message", bot.send_message(chat_id, &msg).await)?;
        }
        SttAdmission::QueueFull => {
            shared_rate_limit_wait(&state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(
                    chat_id,
                    &format!(
                        "Queue full (max {}). Use /stopall to clear.",
                        MAX_QUEUE_SIZE
                    )
                )
                .await
            )?;
        }
    }

    Ok(())
}

async fn process_album_attachments_task(
    bot: Bot,
    chat_id: ChatId,
    msgs: Vec<Message>,
    state: SharedState,
    user_name: String,
    caption_str: Option<String>,
    require_prefix: bool,
    bot_username: String,
    placeholder_token: Arc<CancelToken>,
    reserved_slot: bool,
) {
    let timestamp = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{timestamp}] ◀ [{user_name}] Album: {} attachment(s)",
        msgs.len()
    );

    // Sequential downloads/transcriptions: handle_file_upload and STT download
    // both rate-limit their own Telegram API calls, and sequential processing
    // keeps pending_uploads plus transcript ordering deterministic.
    let has_stt_audio = msgs.iter().any(|m| stt_audio_upload_info(m).is_some());
    let progress_message_id = if has_stt_audio {
        send_stt_progress_message(&bot, chat_id, &state, "album").await
    } else {
        None
    };
    let mut ok_count = 0usize;
    let mut stt_texts: Vec<String> = Vec::new();
    let mut upload_records: Vec<String> = Vec::new();
    let mut stt_failed = false;
    let mut first_stt_error: Option<String> = None;
    for m in &msgs {
        if reserved_slot && placeholder_token.cancelled.load(Ordering::Relaxed) {
            break;
        }
        if stt_audio_upload_info(m).is_some() {
            match transcribe_audio_message(
                &bot,
                chat_id,
                m,
                &state,
                progress_message_id,
                Some(placeholder_token.clone()),
            )
            .await
            {
                Ok(text) => {
                    ok_count += 1;
                    stt_texts.push(text);
                }
                Err(e) => {
                    if reserved_slot && placeholder_token.cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    msg_debug(&format!(
                        "[album_batch] chat_id={}, stt failed: {}",
                        chat_id.0, e
                    ));
                    if first_stt_error.is_none() {
                        first_stt_error = Some(e);
                    }
                    stt_failed = true;
                }
            }
        } else {
            match handle_file_upload(&bot, chat_id, m, &state, &user_name).await {
                Ok(record) => {
                    ok_count += 1;
                    if let Some(record) = record {
                        upload_records.push(record);
                    }
                }
                Err(e) => msg_debug(&format!(
                    "[album_batch] chat_id={}, upload failed: {}",
                    chat_id.0, e
                )),
            }
        }
    }
    println!(
        "  [{timestamp}] ▶ [{user_name}] Album processing complete ({}/{})",
        ok_count,
        msgs.len()
    );

    // If the user issued /stop or /stopall during downloads, the placeholder
    // token will have been marked cancelled. Release the slot and run any
    // queued message instead of dispatching this album.
    if reserved_slot && placeholder_token.cancelled.load(Ordering::Relaxed) {
        msg_debug(&format!(
            "[album_batch] chat_id={}, cancelled during downloads → aborting dispatch",
            chat_id.0
        ));
        mark_stt_progress_stopped(
            &bot,
            chat_id,
            &state,
            progress_message_id,
            "album_cancelled",
        )
        .await;
        rollback_album_upload_records(&state, chat_id, &upload_records, "cancel").await;
        cleanup_reserved_stt_slot(&bot, chat_id, &state, &placeholder_token).await;
        return;
    }

    if stt_failed {
        msg_debug(&format!(
            "[album_batch] chat_id={}, aborting dispatch because at least one STT item failed",
            chat_id.0
        ));
        if let Some(e) = first_stt_error.as_deref() {
            let _ = send_stt_error(&bot, chat_id, &state, progress_message_id, e).await;
        }
        rollback_album_upload_records(&state, chat_id, &upload_records, "stt failure").await;
        if reserved_slot {
            cleanup_reserved_stt_slot(&bot, chat_id, &state, &placeholder_token).await;
        }
        return;
    }

    if !stt_texts.is_empty() {
        if let Err(e) =
            send_stt_confirmation(&bot, chat_id, &state, progress_message_id, &stt_texts).await
        {
            msg_debug(&format!(
                "[album_batch] chat_id={}, stt confirmation failed: {}",
                chat_id.0,
                redact_err(&e)
            ));
            rollback_album_upload_records(&state, chat_id, &upload_records, "confirmation failure")
                .await;
            if reserved_slot {
                cleanup_reserved_stt_slot(&bot, chat_id, &state, &placeholder_token).await;
            }
            return;
        }
    }

    let caption_text = caption_str
        .as_deref()
        .and_then(|c| extract_caption_text(c, require_prefix, &bot_username));
    let request_text = build_stt_request_text(caption_text, &stt_texts);
    match (request_text, reserved_slot) {
        (Some(text), true) => {
            msg_debug(&format!(
                "[album_batch] chat_id={}, dispatching caption (len={}) via reserved slot",
                chat_id.0,
                text.len()
            ));
            let _ = handle_text_message(&bot, chat_id, &text, &state, &user_name, true).await;
        }
        (Some(text), false) => {
            msg_debug(&format!(
                "[album_batch] chat_id={}, dispatching caption (len={}) via queue/redirect",
                chat_id.0,
                text.len()
            ));
            dispatch_album_caption(bot.clone(), chat_id, state.clone(), user_name.clone(), text)
                .await;
        }
        (None, true) => {
            msg_debug(&format!(
                "[album_batch] chat_id={}, no caption → releasing slot",
                chat_id.0
            ));
            {
                let mut data = state.lock().await;
                if let Some(t) = data.cancel_tokens.get(&chat_id) {
                    if Arc::ptr_eq(t, &placeholder_token) {
                        remove_cancel_token_locked(&mut data, chat_id);
                    }
                }
            }
            process_next_queued_message(&bot, chat_id, &state).await;
        }
        (None, false) => {
            msg_debug(&format!("[album_batch] chat_id={}, no caption, slot was busy → uploads pending for next text", chat_id.0));
        }
    }
}

/// Extract the AI-bound text from a media caption, applying the same prefix
/// rules used by `handle_message` for direct text messages. Returns `None`
/// when the caption is empty or, in prefix-required group chats, doesn't
/// address this bot.
fn extract_caption_text(caption: &str, require_prefix: bool, bot_username: &str) -> Option<String> {
    if require_prefix {
        let extracted = if !bot_username.is_empty() && caption.starts_with('@') {
            let prefix = format!("@{} ", bot_username);
            if caption.to_lowercase().starts_with(&prefix.to_lowercase()) {
                let body = caption[prefix.len()..].trim_start();
                body.strip_prefix(';')
                    .map(|s| s.trim_start())
                    .unwrap_or(body)
            } else {
                ""
            }
        } else if caption.starts_with(';') {
            caption[1..].trim_start()
        } else {
            ""
        };
        if extracted.is_empty() {
            None
        } else {
            Some(extracted.to_string())
        }
    } else {
        let trimmed = caption.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

/// Dispatch the album's caption text via the same busy/queue/redirect logic
/// that `handle_message` uses for caption-bearing single-photo uploads.
async fn dispatch_album_caption(
    bot: Bot,
    chat_id: ChatId,
    state: SharedState,
    user_name: String,
    text: String,
) {
    let (ai_busy, queue_enabled, queue_result, redirect_result): (
        bool,
        bool,
        Option<(String, String)>,
        Option<(String, String, bool)>,
    ) = {
        let mut data = state.lock().await;
        let busy = data.cancel_tokens.contains_key(&chat_id);
        let qkey = chat_id.0.to_string();
        let qmode = data
            .settings
            .queue
            .get(&qkey)
            .copied()
            .unwrap_or(QUEUE_MODE_DEFAULT);
        msg_debug(&format!(
            "[album:dispatch] chat_id={}, busy={}, queue_mode={}",
            chat_id.0, busy, qmode
        ));
        let (qr, rr) = if busy && qmode {
            let cur_len = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
            let queue_full = cur_len >= MAX_QUEUE_SIZE;
            let qr = if queue_full {
                msg_debug(&format!(
                    "[album:dispatch] chat_id={}, queue FULL ({}/{})",
                    chat_id.0, cur_len, MAX_QUEUE_SIZE
                ));
                None
            } else {
                let uploads = data
                    .sessions
                    .get_mut(&chat_id)
                    .map(|s| s.take_pending_uploads())
                    .unwrap_or_default();
                let qid = generate_queue_id();
                let q = data
                    .message_queues
                    .entry(chat_id)
                    .or_insert_with(std::collections::VecDeque::new);
                q.push_back(QueuedMessage {
                    id: qid.clone(),
                    user_display_name: user_name.clone(),
                    payload: QueuedPayload::Text {
                        text: text.clone(),
                        pending_uploads: uploads.clone(),
                    },
                });
                msg_debug(&format!(
                    "[album:dispatch] chat_id={}, QUEUED id={}, pos={}, uploads={}",
                    chat_id.0,
                    qid,
                    q.len(),
                    uploads.len()
                ));
                Some((qid, text.clone()))
            };
            (qr, None)
        } else if busy && !qmode {
            let uploads = data
                .sessions
                .get_mut(&chat_id)
                .map(|s| s.take_pending_uploads())
                .unwrap_or_default();
            let (qid, replaced) = enqueue_redirect_locked(
                &mut *data,
                chat_id,
                text.clone(),
                user_name.clone(),
                uploads,
            );
            msg_debug(&format!(
                "[album:dispatch] chat_id={}, REDIRECT id={}, replaced={}",
                chat_id.0, qid, replaced
            ));
            (None, Some((qid, text.clone(), replaced)))
        } else {
            (None, None)
        };
        (busy, qmode, qr, rr)
    };
    if ai_busy {
        if let Some((_qid, qtxt, replaced)) = redirect_result {
            shared_rate_limit_wait(&state, chat_id).await;
            let preview = truncate_str(&qtxt, 30);
            let m = if replaced {
                format!("🔄 Redirect target updated: \"{preview}\"")
            } else {
                format!("🔄 Cancelling current task, will process: \"{preview}\"")
            };
            let _ = tg!("send_message", bot.send_message(chat_id, &m).await);
        } else if queue_enabled {
            shared_rate_limit_wait(&state, chat_id).await;
            if let Some((qid, qtxt)) = queue_result {
                let preview = truncate_str(&qtxt, 30);
                let _ = tg!("send_message", bot.send_message(chat_id, &format!("Queued ({qid}) \"{preview}\"\n- /stopall to cancel all\n- /stop_{qid} to cancel this")).await);
            } else {
                let _ = tg!(
                    "send_message",
                    bot.send_message(
                        chat_id,
                        &format!(
                            "Queue full (max {}). Use /stopall to clear.",
                            MAX_QUEUE_SIZE
                        )
                    )
                    .await
                );
            }
        } else {
            shared_rate_limit_wait(&state, chat_id).await;
            let _ = tg!(
                "send_message",
                bot.send_message(chat_id, "AI request in progress. Use /stop to cancel.")
                    .await
            );
        }
    } else {
        let _ = handle_text_message(&bot, chat_id, &text, &state, &user_name, false).await;
    }
}

/// Atomically handle an album that arrived as a contiguous run of media items
/// inside a single `getUpdates` batch (size ≥2, all same `media_group_id`).
///
/// Differs from the per-photo path in `handle_message`: we know the exact
/// album size up-front (it's `msgs.len()`) so no debounce/timeout buffering
/// is needed. Non-audio files are saved in order and audio files are
/// transcribed in order. The resulting caption/transcript request is
/// dispatched once via `dispatch_album_caption`, the same helper used by
/// the buffered fallback path.
///
/// Auth replicates the relevant parts of `handle_message`: imprinting,
/// owner/public check, `require_prefix` calculation, and the `;`/`@bot`
/// caption admission rule for prefix-mode group chats.
async fn handle_album_batch(
    bot: Bot,
    msgs: Vec<Message>,
    state: SharedState,
    token: &str,
    bot_username: &str,
) -> ResponseResult<()> {
    if msgs.is_empty() {
        return Ok(());
    }
    let primary = &msgs[0];
    let chat_id = primary.chat.id;
    let raw_user_name = primary
        .from
        .as_ref()
        .map(|u| u.first_name.as_str())
        .unwrap_or("unknown");
    let timestamp = chrono::Local::now().format("%H:%M:%S");
    let user_id = primary.from.as_ref().map(|u| u.id.0);

    let Some(uid) = user_id else {
        for m in &msgs {
            log_incoming_message(m, false, "no_user_id");
        }
        return Ok(());
    };

    let is_group_chat = matches!(primary.chat.kind, teloxide::types::ChatKind::Public(_));
    let require_prefix = {
        let mut data = state.lock().await;
        let chat_key = chat_id.0.to_string();
        let direct_setting = data
            .settings
            .direct
            .get(&chat_key)
            .copied()
            .unwrap_or(DIRECT_MODE_DEFAULT);
        let is_direct = is_group_chat && direct_setting;
        let require_prefix = is_group_chat && !is_direct;
        match data.settings.owner_user_id {
            None => {
                data.settings.owner_user_id = Some(uid);
                save_bot_settings(token, &data.settings);
                println!("  [{timestamp}] ★ Owner registered: {raw_user_name} (id:{uid})");
            }
            Some(owner_id) => {
                if uid != owner_id {
                    let is_public = is_group_chat
                        && data
                            .settings
                            .as_public_for_group_chat
                            .get(&chat_key)
                            .copied()
                            .unwrap_or(PUBLIC_MODE_DEFAULT);
                    if !is_public {
                        println!("  [{timestamp}] ✗ Rejected: {raw_user_name} (id:{uid})");
                        for m in &msgs {
                            log_incoming_message(m, false, "unauthorized");
                        }
                        return Ok(());
                    }
                    println!("  [{timestamp}] ○ [{raw_user_name}(id:{uid})] Public group access");
                }
            }
        }
        require_prefix
    };

    for m in &msgs {
        log_incoming_message(m, true, "");
    }

    let user_name = format!("{}({uid})", raw_user_name);

    // First non-empty caption — Telegram clients put it on the first photo,
    // but be defensive and scan the album in arrival order.
    let caption_str: Option<String> = msgs
        .iter()
        .find_map(|m| m.caption().filter(|c| !c.is_empty()).map(|c| c.to_string()));

    // Prefix-mode admission: in group chats with prefix required, the album
    // is admitted only if its caption (typically on the first photo) starts
    // with `;` or `@bot`. Within a single getUpdates batch we always have
    // the caption-bearing photo, so caption-only admission is sufficient
    // and fully deterministic.
    if require_prefix {
        let admitted = match caption_str.as_deref() {
            None => false,
            Some(c) => {
                if !bot_username.is_empty() && c.starts_with('@') {
                    let prefix = format!("@{}", bot_username.to_lowercase());
                    let lower = c.to_lowercase();
                    lower.starts_with(&prefix)
                        && (lower.len() == prefix.len()
                            || lower[prefix.len()..].starts_with(|ch: char| ch.is_whitespace()))
                } else {
                    c.starts_with(';')
                }
            }
        };
        if !admitted {
            msg_debug(&format!(
                "[album_batch] chat_id={}, rejected: caption lacks ;/@",
                chat_id.0
            ));
            return Ok(());
        }
        msg_debug(&format!(
            "[album_batch] chat_id={}, admitted via caption prefix",
            chat_id.0
        ));
    }

    auto_restore_session(&state, chat_id, &user_name).await;

    enum AlbumAdmission {
        Start {
            request_tasks: RequestTasks,
            placeholder_token: Arc<CancelToken>,
            msgs: Vec<Message>,
            user_name: String,
            caption_str: Option<String>,
            bot_username: String,
        },
        Queued {
            id: String,
            preview: String,
        },
        Redirect {
            preview: String,
            replaced: bool,
        },
        QueueFull,
    }

    // Reserve the per-chat AI slot with a placeholder cancel token *before*
    // starting downloads/transcriptions. If another AI request is already
    // active, defer the entire album payload through the normal queue so no
    // STT/download work runs outside the chat's cancellation/order model.
    let dispatch_id = current_dispatch_id();
    let placeholder_token = new_cancel_token_for_owner(dispatch_id);
    let bot_username_owned = bot_username.to_string();
    let admission = {
        let mut data = state.lock().await;
        let request_tasks = data.request_tasks.clone();
        if data.cancel_tokens.contains_key(&chat_id) {
            msg_debug(&format!(
                "[album_batch] chat_id={}, slot busy at admission → deferring album",
                chat_id.0
            ));
            let qkey = chat_id.0.to_string();
            let qmode = data
                .settings
                .queue
                .get(&qkey)
                .copied()
                .unwrap_or(QUEUE_MODE_DEFAULT);
            let preview = format!("Album: {} attachment(s)", msgs.len());
            if qmode {
                let cur_len = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
                if cur_len >= MAX_QUEUE_SIZE {
                    msg_debug(&format!(
                        "[album_batch] chat_id={}, queue FULL ({}/{})",
                        chat_id.0, cur_len, MAX_QUEUE_SIZE
                    ));
                    AlbumAdmission::QueueFull
                } else {
                    let id = generate_queue_id();
                    let q = data
                        .message_queues
                        .entry(chat_id)
                        .or_insert_with(std::collections::VecDeque::new);
                    q.push_back(QueuedMessage {
                        id: id.clone(),
                        user_display_name: user_name,
                        payload: QueuedPayload::AlbumAttachments {
                            msgs,
                            caption_str,
                            require_prefix,
                            bot_username: bot_username_owned,
                        },
                    });
                    msg_debug(&format!(
                        "[album_batch] chat_id={}, QUEUED id={}, pos={}",
                        chat_id.0,
                        id,
                        q.len()
                    ));
                    AlbumAdmission::Queued { id, preview }
                }
            } else {
                cancel_in_progress_task_locked(&data, chat_id);
                data.loop_states.remove(&chat_id);
                data.loop_feedback.remove(&chat_id);
                let q = data
                    .message_queues
                    .entry(chat_id)
                    .or_insert_with(std::collections::VecDeque::new);
                let replaced = !q.is_empty();
                q.clear();
                let id = generate_queue_id();
                q.push_back(QueuedMessage {
                    id: id.clone(),
                    user_display_name: user_name,
                    payload: QueuedPayload::AlbumAttachments {
                        msgs,
                        caption_str,
                        require_prefix,
                        bot_username: bot_username_owned,
                    },
                });
                msg_debug(&format!(
                    "[album_batch] chat_id={}, REDIRECT id={}, replaced={}",
                    chat_id.0, id, replaced
                ));
                AlbumAdmission::Redirect { preview, replaced }
            }
        } else {
            insert_cancel_token_locked(&mut data, chat_id, placeholder_token.clone());
            msg_debug(&format!(
                "[album_batch] chat_id={}, reserved AI slot (placeholder)",
                chat_id.0
            ));
            AlbumAdmission::Start {
                request_tasks,
                placeholder_token,
                msgs,
                user_name,
                caption_str,
                bot_username: bot_username_owned,
            }
        }
    };

    match admission {
        AlbumAdmission::Start {
            request_tasks,
            placeholder_token,
            msgs,
            user_name,
            caption_str,
            bot_username,
        } => {
            let state_for_task = state.clone();
            let state_for_monitor = state.clone();
            let join = spawn_tracked_request_task(request_tasks, async move {
                CURRENT_DISPATCH_ID
                    .scope(dispatch_id, async move {
                        process_album_attachments_task(
                            bot,
                            chat_id,
                            msgs,
                            state_for_task,
                            user_name,
                            caption_str,
                            require_prefix,
                            bot_username,
                            placeholder_token,
                            true,
                        )
                        .await;
                    })
                    .await
            });
            monitor_dispatch_panic(
                join,
                state_for_monitor,
                chat_id,
                dispatch_id,
                "album:direct",
            );
        }
        AlbumAdmission::Queued { id, preview } => {
            shared_rate_limit_wait(&state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, &format!("Queued ({id}) \"{preview}\"\n- /stopall to cancel all\n- /stop_{id} to cancel this"))
                .await)?;
        }
        AlbumAdmission::Redirect { preview, replaced } => {
            shared_rate_limit_wait(&state, chat_id).await;
            let msg = if replaced {
                format!("🔄 Redirect target updated: \"{preview}\"")
            } else {
                format!("🔄 Cancelling current task, will process: \"{preview}\"")
            };
            tg!("send_message", bot.send_message(chat_id, &msg).await)?;
        }
        AlbumAdmission::QueueFull => {
            shared_rate_limit_wait(&state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(
                    chat_id,
                    &format!(
                        "Queue full (max {}). Use /stopall to clear.",
                        MAX_QUEUE_SIZE
                    )
                )
                .await
            )?;
        }
    }
    Ok(())
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
    let raw_user_name = msg
        .from
        .as_ref()
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
        let direct_setting = data
            .settings
            .direct
            .get(&chat_key)
            .copied()
            .unwrap_or(DIRECT_MODE_DEFAULT);
        let is_direct = is_group_chat && direct_setting;
        msg_debug(&format!("[handle_message] chat_id={}, uid={}, is_group_chat={}, direct_setting={}, is_direct={}",
            chat_id.0, uid, is_group_chat, direct_setting, is_direct));
        // In direct mode, ; prefix requirement is waived but is_group_chat stays true
        let require_prefix = is_group_chat && !is_direct;
        msg_debug(&format!(
            "[handle_message] require_prefix={}",
            require_prefix
        ));
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
                        && data
                            .settings
                            .as_public_for_group_chat
                            .get(&chat_key)
                            .copied()
                            .unwrap_or(PUBLIC_MODE_DEFAULT);
                    msg_debug(&format!(
                        "[handle_message] non-owner uid={}, owner_id={}, is_public={}",
                        uid, owner_id, is_public
                    ));
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
        msg_debug(&format!(
            "[handle_message] result: require_prefix={}, imprinted={}, is_owner={}",
            require_prefix, imprinted, is_owner
        ));
        (require_prefix, imprinted, is_owner)
    };
    if imprinted {
        // Owner registration is logged to server console only
        // No response sent to the user
    }

    log_incoming_message(&msg, true, "");

    let user_name = format!("{}({uid})", raw_user_name);

    let has_file = msg.document().is_some()
        || msg.photo().is_some()
        || msg.video().is_some()
        || msg.voice().is_some()
        || msg.audio().is_some()
        || msg.animation().is_some()
        || msg.video_note().is_some();

    // Auto-restore session for file uploads (before text extraction)
    if has_file {
        auto_restore_session(&state, chat_id, &user_name).await;
    }

    // Audio uploads are treated as spoken user input. They are downloaded only
    // to ~/.cokacdir/stttemp for transcription, never saved into the session
    // workspace and never recorded as pending file uploads.
    if has_file && stt_audio_upload_info(&msg).is_some() {
        if require_prefix {
            let caption = msg.caption().unwrap_or("");
            msg_debug(&format!(
                "[handle_message] stt upload: require_prefix=true, caption={:?}, media_group={:?}",
                caption,
                msg.media_group_id()
            ));
            if !bot_username.is_empty() && caption.starts_with('@') {
                let caption_lower = caption.to_lowercase();
                let prefix = format!("@{}", bot_username.to_lowercase());
                if !caption_lower.starts_with(&prefix)
                    || caption_lower.len() > prefix.len()
                        && !caption_lower[prefix.len()..].starts_with(|c: char| c.is_whitespace())
                {
                    msg_debug("[handle_message] stt upload: rejected (@mention for another bot)");
                    return Ok(());
                }
                msg_debug(&format!(
                    "[handle_message] stt upload: accepted (@{} mention)",
                    bot_username
                ));
            } else if caption.starts_with(';') {
                msg_debug("[handle_message] stt upload: accepted (; prefix, all bots)");
            } else {
                msg_debug("[handle_message] stt upload: rejected (no ; or @ prefix)");
                return Ok(());
            }
        } else {
            msg_debug(&format!(
                "[handle_message] stt upload: require_prefix=false, caption={:?}",
                msg.caption()
            ));
        }

        println!("  [{timestamp}] ◀ [{user_name}] Audio upload: STT");
        let caption_text = msg
            .caption()
            .and_then(|caption| extract_caption_text(caption, require_prefix, bot_username));
        start_stt_message_task(
            bot.clone(),
            chat_id,
            msg,
            state.clone(),
            user_name,
            caption_text,
        )
        .await?;
        return Ok(());
    }

    // Handle file/photo/media uploads
    if has_file {
        // In group chats (with prefix required), only process uploads whose
        // caption starts with `;` or `@bot`. Albums in this code path are
        // 1-photo fragments from a split `getUpdates` batch (the multi-photo
        // case is handled atomically in `handle_album_batch`); since they
        // necessarily lack the album's caption, prefix-mode rejects them.
        if require_prefix {
            let caption = msg.caption().unwrap_or("");
            msg_debug(&format!(
                "[handle_message] upload: require_prefix=true, caption={:?}, media_group={:?}",
                caption,
                msg.media_group_id()
            ));
            if !bot_username.is_empty() && caption.starts_with('@') {
                let caption_lower = caption.to_lowercase();
                let prefix = format!("@{}", bot_username.to_lowercase());
                if !caption_lower.starts_with(&prefix)
                    || caption_lower.len() > prefix.len()
                        && !caption_lower[prefix.len()..].starts_with(|c: char| c.is_whitespace())
                {
                    msg_debug("[handle_message] upload: rejected (@mention for another bot)");
                    return Ok(());
                }
                msg_debug(&format!(
                    "[handle_message] upload: accepted (@{} mention)",
                    bot_username
                ));
            } else if caption.starts_with(';') {
                msg_debug("[handle_message] upload: accepted (; prefix, all bots)");
            } else {
                msg_debug("[handle_message] upload: rejected (no ; or @ prefix)");
                return Ok(());
            }
        } else {
            msg_debug(&format!(
                "[handle_message] upload: require_prefix=false, caption={:?}",
                msg.caption()
            ));
        }
        let file_hint = if msg.animation().is_some() {
            "animation"
        } else if msg.document().is_some() {
            "document"
        } else if msg.photo().is_some() {
            "photo"
        } else if msg.video().is_some() {
            "video"
        } else if msg.voice().is_some() {
            "voice"
        } else if msg.audio().is_some() {
            "audio"
        } else {
            "video_note"
        };
        println!("  [{timestamp}] ◀ [{user_name}] Upload: {file_hint}");

        let _ = handle_file_upload(&bot, chat_id, &msg, &state, &user_name).await?;
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
                        body.strip_prefix(';')
                            .map(|s| s.trim_start())
                            .unwrap_or(body)
                    } else {
                        ""
                    }
                } else if caption.starts_with(';') {
                    caption[1..].trim_start()
                } else {
                    ""
                };
                let result = if extracted.is_empty() {
                    None
                } else {
                    Some(extracted)
                };
                msg_debug(&format!(
                    "[handle_message] upload caption (prefix mode): extracted={:?}",
                    result
                ));
                result
            } else {
                // DM or direct mode: use entire caption as-is
                let trimmed = caption.trim();
                let result = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                msg_debug(&format!(
                    "[handle_message] upload caption (direct): extracted={:?}",
                    result
                ));
                result
            };
            if let Some(text) = text_part {
                if !text.is_empty() {
                    // Block if an AI request is already in progress
                    // Atomically: check busy + queue mode + push to queue (prevents race with /stopall)
                    // queue_result:    ON mode queue push outcome (Some(id,text) queued, None full)
                    // redirect_result: OFF mode redirect outcome (Some(id,text,was_replacement))
                    let (ai_busy, queue_enabled, queue_result, redirect_result): (
                        bool,
                        bool,
                        Option<(String, String)>,
                        Option<(String, String, bool)>,
                    ) = {
                        let mut data = state.lock().await;
                        let busy = data.cancel_tokens.contains_key(&chat_id);
                        let qkey = chat_id.0.to_string();
                        let qmode = data
                            .settings
                            .queue
                            .get(&qkey)
                            .copied()
                            .unwrap_or(QUEUE_MODE_DEFAULT);
                        msg_debug(&format!(
                            "[queue:media] chat_id={}, busy={}, queue_mode={}",
                            chat_id.0, busy, qmode
                        ));
                        let (qr, rr) = if busy && qmode {
                            let cur_len = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
                            let queue_full = cur_len >= MAX_QUEUE_SIZE;
                            let qr = if queue_full {
                                msg_debug(&format!(
                                    "[queue:media] chat_id={}, queue FULL ({}/{})",
                                    chat_id.0, cur_len, MAX_QUEUE_SIZE
                                ));
                                None // queue full
                            } else {
                                // Capture pending_uploads so they stay associated with this caption
                                let uploads = data
                                    .sessions
                                    .get_mut(&chat_id)
                                    .map(|s| s.take_pending_uploads())
                                    .unwrap_or_default();
                                let qid = generate_queue_id();
                                let q = data
                                    .message_queues
                                    .entry(chat_id)
                                    .or_insert_with(std::collections::VecDeque::new);
                                q.push_back(QueuedMessage {
                                    id: qid.clone(),
                                    user_display_name: user_name.clone(),
                                    payload: QueuedPayload::Text {
                                        text: text.to_string(),
                                        pending_uploads: uploads.clone(),
                                    },
                                });
                                msg_debug(&format!("[queue:media] chat_id={}, QUEUED id={}, pos={}, text={:?}, uploads={}", chat_id.0, qid, q.len(), truncate_str(text, 60), uploads.len()));
                                Some((qid, text.to_string()))
                            };
                            (qr, None)
                        } else if busy && !qmode {
                            // OFF mode: caption with text is always redirect-eligible
                            let uploads = data
                                .sessions
                                .get_mut(&chat_id)
                                .map(|s| s.take_pending_uploads())
                                .unwrap_or_default();
                            let (qid, replaced) = enqueue_redirect_locked(
                                &mut *data,
                                chat_id,
                                text.to_string(),
                                user_name.clone(),
                                uploads,
                            );
                            msg_debug(&format!(
                                "[queue:media] chat_id={}, REDIRECT id={}, replaced={}, text={:?}",
                                chat_id.0,
                                qid,
                                replaced,
                                truncate_str(text, 60)
                            ));
                            (None, Some((qid, text.to_string(), replaced)))
                        } else {
                            (None, None)
                        };
                        (busy, qmode, qr, rr)
                    };
                    if ai_busy {
                        if let Some((_qid, qtxt, replaced)) = redirect_result {
                            shared_rate_limit_wait(&state, chat_id).await;
                            let preview = truncate_str(&qtxt, 30);
                            let msg = if replaced {
                                format!("🔄 Redirect target updated: \"{preview}\"")
                            } else {
                                format!("🔄 Cancelling current task, will process: \"{preview}\"")
                            };
                            tg!("send_message", bot.send_message(chat_id, &msg).await)?;
                        } else if queue_enabled {
                            shared_rate_limit_wait(&state, chat_id).await;
                            if let Some((qid, qtxt)) = queue_result {
                                let preview = truncate_str(&qtxt, 30);
                                tg!("send_message", bot.send_message(chat_id, &format!("Queued ({qid}) \"{preview}\"\n- /stopall to cancel all\n- /stop_{qid} to cancel this"))
                                    .await)?;
                            } else {
                                tg!(
                                    "send_message",
                                    bot.send_message(
                                        chat_id,
                                        &format!(
                                            "Queue full (max {}). Use /stopall to clear.",
                                            MAX_QUEUE_SIZE
                                        )
                                    )
                                    .await
                                )?;
                            }
                        } else {
                            shared_rate_limit_wait(&state, chat_id).await;
                            tg!(
                                "send_message",
                                bot.send_message(
                                    chat_id,
                                    "AI request in progress. Use /stop to cancel."
                                )
                                .await
                            )?;
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
        msg_debug(&format!(
            "[handle_message] chat_id={}, location: lat={}, lon={}",
            chat_id.0, location.latitude, location.longitude
        ));
        auto_restore_session(&state, chat_id, &user_name).await;

        let location_record = format!(
            "[Location shared] Latitude: {}, Longitude: {}",
            location.latitude, location.longitude
        );
        let stored = {
            let mut data = state.lock().await;
            if let Some(session) = data.sessions.get_mut(&chat_id) {
                session.push_pending_upload(location_record.clone());
                true
            } else {
                false
            }
        };
        if stored {
            println!(
                "  [{timestamp}] ◀ [{user_name}] Location: {}, {} (pending)",
                location.latitude, location.longitude
            );
            shared_rate_limit_wait(&state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, "Location received.").await
            )?;
        } else {
            shared_rate_limit_wait(&state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, "No active session. Use /start <path> first.")
                    .await
            )?;
        }
        return Ok(());
    }

    // Handle venue sharing (place selected from Telegram's location picker) — store as pending
    if let Some(venue) = msg.venue() {
        let loc = &venue.location;
        msg_debug(&format!(
            "[handle_message] chat_id={}, venue: title={:?}, addr={:?}, lat={}, lon={}",
            chat_id.0, venue.title, venue.address, loc.latitude, loc.longitude
        ));
        auto_restore_session(&state, chat_id, &user_name).await;

        let location_record = format!(
            "[Location shared] {}, {} (Latitude: {}, Longitude: {})",
            venue.title, venue.address, loc.latitude, loc.longitude
        );
        let stored = {
            let mut data = state.lock().await;
            if let Some(session) = data.sessions.get_mut(&chat_id) {
                session.push_pending_upload(location_record.clone());
                true
            } else {
                false
            }
        };
        if stored {
            println!(
                "  [{timestamp}] ◀ [{user_name}] Venue: {} ({}, {}) (pending)",
                venue.title, loc.latitude, loc.longitude
            );
            shared_rate_limit_wait(&state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, format!("Location received: {}", venue.title))
                    .await
            )?;
        } else {
            shared_rate_limit_wait(&state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, "No active session. Use /start <path> first.")
                    .await
            )?;
        }
        return Ok(());
    }

    let Some(raw_text) = msg.text() else {
        msg_debug(&format!(
            "[handle_message] chat_id={}, non-text message (no raw_text), skipping",
            chat_id.0
        ));
        return Ok(());
    };

    msg_debug(&format!(
        "[handle_message] chat_id={}, user={}, raw_text={:?}",
        chat_id.0,
        user_name,
        truncate_str(raw_text, 100)
    ));

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
                msg_debug(&format!(
                    "[mention_routing] self-mention with plain text: {:?} → {:?} (added ; prefix)",
                    raw_text, prefixed
                ));
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
            msg_debug(&format!(
                "[mention_routing] starts with @ but bot_username is empty, passing through"
            ));
        } else {
            msg_debug(&format!(
                "[mention_routing] no @-prefix, passing through raw_text as-is"
            ));
        }
        raw_text
    };

    // Strip @botname suffix from commands (e.g. "/pwd@mybot" → "/pwd")
    // If @botname doesn't match this bot, ignore the command (it's for another bot)
    let starts_with_slash = raw_text.starts_with('/');
    msg_debug(&format!(
        "[cmd_routing] input: raw_text={:?}, starts_with_slash={}",
        truncate_str(raw_text, 100),
        starts_with_slash
    ));
    let text = if starts_with_slash {
        if let Some(space_pos) = raw_text.find(' ') {
            // "/cmd@bot args" → "/cmd args"
            let cmd_part = &raw_text[..space_pos];
            let args_part = &raw_text[space_pos..];
            if let Some(at_pos) = cmd_part.find('@') {
                let mentioned = &cmd_part[at_pos + 1..];
                // When bot_username is empty (get_me failed at startup) we
                // can't tell whether `@x` is us or another bot. Treat any
                // explicit `@suffix` on a command as "not for us" so we
                // don't silently steal commands intended for siblings in a
                // multi-bot setup.
                let is_self = has_bot_username && mentioned.to_lowercase() == bot_username;
                msg_debug(&format!("[cmd_routing] slash+args: cmd={:?}, mentioned={:?}, is_self={}, bot_username={:?}", cmd_part, mentioned, is_self, bot_username));
                if !is_self {
                    msg_debug(&format!(
                        "[cmd_routing] IGNORED: command {:?} is for @{}, not for @{}",
                        cmd_part, mentioned, bot_username
                    ));
                    return Ok(());
                }
                let result = format!("{}{}", &cmd_part[..at_pos], args_part);
                msg_debug(&format!(
                    "[cmd_routing] stripped @mention from command: {:?} → {:?}",
                    raw_text, result
                ));
                result
            } else {
                msg_debug(&format!(
                    "[cmd_routing] slash+args, no @mention: {:?} → all bots",
                    raw_text
                ));
                raw_text.to_string()
            }
        } else {
            // "/cmd@bot" (no args) → "/cmd"
            if let Some(at_pos) = raw_text.find('@') {
                let mentioned = &raw_text[at_pos + 1..];
                // See note above: unknown bot_username + explicit @suffix
                // → treat as not-for-us.
                let is_self = has_bot_username && mentioned.to_lowercase() == bot_username;
                msg_debug(&format!("[cmd_routing] slash-only: cmd={:?}, mentioned={:?}, is_self={}, bot_username={:?}", raw_text, mentioned, is_self, bot_username));
                if !is_self {
                    msg_debug(&format!(
                        "[cmd_routing] IGNORED: command {:?} is for @{}, not for @{}",
                        raw_text, mentioned, bot_username
                    ));
                    return Ok(());
                }
                let result = raw_text[..at_pos].to_string();
                msg_debug(&format!(
                    "[cmd_routing] stripped @mention from command: {:?} → {:?}",
                    raw_text, result
                ));
                result
            } else {
                msg_debug(&format!(
                    "[cmd_routing] slash-only, no @mention: {:?} → all bots",
                    raw_text
                ));
                raw_text.to_string()
            }
        }
    } else {
        msg_debug(&format!(
            "[cmd_routing] not a slash command, text={:?}",
            truncate_str(raw_text, 100)
        ));
        raw_text.to_string()
    };
    let preview = &text;

    // Auto-restore session from bot_settings.json if not in memory.
    // /start owns its own session setup, so skip auto-restore there.
    if !is_cmd(&text, "start") {
        auto_restore_session(&state, chat_id, &user_name).await;
    }

    // In group chats (with prefix required), ignore plain text (only /, !, ; prefixed messages are processed)
    let has_valid_prefix = text.starts_with('/') || text.starts_with('!') || text.starts_with(';');
    msg_debug(&format!(
        "[prefix_filter] text={:?}, require_prefix={}, has_valid_prefix={}, direct_mode={}",
        truncate_str(&text, 100),
        require_prefix,
        has_valid_prefix,
        !require_prefix
    ));
    if require_prefix && !has_valid_prefix {
        msg_debug(&format!("[prefix_filter] IGNORED: require_prefix=true (direct mode OFF), no valid prefix in text={:?}", truncate_str(&text, 80)));
        // A previous `;`-prefixed photo upload (addressed to all bots) may be
        // waiting for the next addressed text message; wiping it because of
        // an unrelated message to a sibling bot would silently lose the
        // user's data. So we keep recent uploads — but expire ones older than
        // PENDING_UPLOAD_STALE_TTL so they don't re-attach hours later when
        // the user has clearly moved on.
        {
            let mut data = state.lock().await;
            if let Some(session) = data.sessions.get_mut(&chat_id) {
                if let Some(added_at) = session.pending_uploads_added_at {
                    if added_at.elapsed() > PENDING_UPLOAD_STALE_TTL {
                        let n = session.pending_uploads.len();
                        if n > 0 {
                            msg_debug(&format!(
                                "[prefix_filter] expiring {} stale pending_uploads (age > {}s)",
                                n,
                                PENDING_UPLOAD_STALE_TTL.as_secs()
                            ));
                            session.clear_pending_uploads();
                        }
                    }
                }
            }
        }
        return Ok(());
    }

    if is_group_chat && !is_owner && is_owner_only_command(&text) {
        msg_debug(&format!(
            "[handle_message] owner-only command rejected: text={:?}",
            truncate_str(&text, 80)
        ));
        shared_rate_limit_wait(&state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, "Only the bot owner can use this command.")
                .await
        )?;
        return Ok(());
    }

    // Block all messages except /stop, /stopall, /queue while an AI request is in progress
    let cmd_for_busy = command_name(&text);
    let is_busy_bypass = matches!(cmd_for_busy, Some("stop" | "stopall" | "queue"))
        || cmd_for_busy.map_or(false, |c| c.starts_with("stop_"));
    if !is_busy_bypass {
        // Determine the actual user text to queue (strip command prefix, pure string ops)
        let queue_text = if is_cmd(&text, "query") {
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
            if stripped.is_empty() {
                None
            } else {
                Some(stripped)
            }
        } else if text.starts_with('!') || text.starts_with('/') {
            // Shell commands and other / commands are NOT queueable
            None
        } else {
            // Plain text
            Some(text.as_str())
        };
        msg_debug(&format!(
            "[queue:text] chat_id={}, text={:?}, queue_text={:?}",
            chat_id.0,
            truncate_str(&text, 60),
            queue_text.map(|s| truncate_str(s, 60))
        ));

        // Atomically: check busy + queue mode + push to queue (prevents race with /stopall)
        // Returns: Option<(can_queue, queue_on, queue_result, redirect_result)>
        //   queue_result:    Some((id, text)) if queued (ON mode), None if full/non-queueable
        //   redirect_result: Some((id, text, was_replacement)) if OFF mode redirect triggered
        let busy_info: Option<(
            bool,
            bool,
            Option<(String, String)>,
            Option<(String, String, bool)>,
        )> = {
            let mut data = state.lock().await;
            if data.cancel_tokens.contains_key(&chat_id) {
                let qkey = chat_id.0.to_string();
                let queue_enabled = data
                    .settings
                    .queue
                    .get(&qkey)
                    .copied()
                    .unwrap_or(QUEUE_MODE_DEFAULT);
                msg_debug(&format!(
                    "[queue:text] chat_id={}, AI busy, queue_mode={}, queueable={}",
                    chat_id.0,
                    queue_enabled,
                    queue_text.is_some()
                ));
                let (qr, rr) = if queue_enabled {
                    let qr = if let Some(qt) = queue_text {
                        let cur_len = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
                        let queue_full = cur_len >= MAX_QUEUE_SIZE;
                        if queue_full {
                            msg_debug(&format!(
                                "[queue:text] chat_id={}, queue FULL ({}/{})",
                                chat_id.0, cur_len, MAX_QUEUE_SIZE
                            ));
                            None // queue full
                        } else {
                            // Capture pending_uploads so they stay associated with this message
                            let uploads = data
                                .sessions
                                .get_mut(&chat_id)
                                .map(|s| s.take_pending_uploads())
                                .unwrap_or_default();
                            let qid = generate_queue_id();
                            let q = data
                                .message_queues
                                .entry(chat_id)
                                .or_insert_with(std::collections::VecDeque::new);
                            q.push_back(QueuedMessage {
                                id: qid.clone(),
                                user_display_name: user_name.clone(),
                                payload: QueuedPayload::Text {
                                    text: qt.to_string(),
                                    pending_uploads: uploads.clone(),
                                },
                            });
                            msg_debug(&format!("[queue:text] chat_id={}, QUEUED id={}, pos={}, text={:?}, uploads={}", chat_id.0, qid, q.len(), truncate_str(qt, 60), uploads.len()));
                            Some((qid, qt.to_string()))
                        }
                    } else {
                        msg_debug(&format!(
                            "[queue:text] chat_id={}, non-queueable command, rejecting",
                            chat_id.0
                        ));
                        None // non-queueable
                    };
                    (qr, None)
                } else {
                    // OFF mode: redirect-eligible messages cancel the current task and become
                    // the next dispatch target (latest-wins). Slash/shell commands stay rejected.
                    let rr = if let Some(qt) = queue_text {
                        let uploads = data
                            .sessions
                            .get_mut(&chat_id)
                            .map(|s| s.take_pending_uploads())
                            .unwrap_or_default();
                        let (qid, replaced) = enqueue_redirect_locked(
                            &mut *data,
                            chat_id,
                            qt.to_string(),
                            user_name.clone(),
                            uploads,
                        );
                        msg_debug(&format!(
                            "[queue:text] chat_id={}, REDIRECT id={}, replaced={}, text={:?}",
                            chat_id.0,
                            qid,
                            replaced,
                            truncate_str(qt, 60)
                        ));
                        Some((qid, qt.to_string(), replaced))
                    } else {
                        msg_debug(&format!("[queue:text] chat_id={}, OFF mode + non-redirectable command, rejecting", chat_id.0));
                        None
                    };
                    (None, rr)
                };
                Some((queue_enabled && queue_text.is_some(), queue_enabled, qr, rr))
            } else {
                msg_debug(&format!(
                    "[queue:text] chat_id={}, AI not busy, proceeding normally",
                    chat_id.0
                ));
                None // not busy
            }
        };

        if let Some((can_queue, queue_on, queue_result, redirect_result)) = busy_info {
            if let Some((_qid, qtxt, replaced)) = redirect_result {
                shared_rate_limit_wait(&state, chat_id).await;
                let preview = truncate_str(&qtxt, 30);
                let msg = if replaced {
                    format!("🔄 Redirect target updated: \"{preview}\"")
                } else {
                    format!("🔄 Cancelling current task, will process: \"{preview}\"")
                };
                tg!("send_message", bot.send_message(chat_id, &msg).await)?;
            } else if can_queue {
                shared_rate_limit_wait(&state, chat_id).await;
                if let Some((qid, qtxt)) = queue_result {
                    let preview = truncate_str(&qtxt, 30);
                    tg!("send_message", bot.send_message(chat_id, &format!("Queued ({qid}) \"{preview}\"\n- /stopall to cancel all\n- /stop_{qid} to cancel this"))
                        .await)?;
                } else {
                    tg!(
                        "send_message",
                        bot.send_message(
                            chat_id,
                            &format!(
                                "Queue full (max {}). Use /stopall to clear.",
                                MAX_QUEUE_SIZE
                            )
                        )
                        .await
                    )?;
                }
            } else {
                shared_rate_limit_wait(&state, chat_id).await;
                if queue_on {
                    tg!("send_message", bot.send_message(chat_id, "AI request in progress. This command cannot be queued. Use /stop to cancel current, /stopall to cancel all.")
                        .await)?;
                } else {
                    tg!(
                        "send_message",
                        bot.send_message(chat_id, "AI request in progress. Use /stop to cancel.")
                            .await
                    )?;
                }
            }
            return Ok(());
        }
    }

    let cmd_name_opt = command_name(&text);
    let is_stop_id_form = cmd_name_opt.map_or(false, |c| c.starts_with("stop_"));
    if is_cmd(&text, "stopall") {
        msg_debug(&format!("[handle_message] routing → /stopall"));
        println!("  [{timestamp}] ◀ [{user_name}] /stopall");
        handle_stopall_command(&bot, chat_id, &state).await?;
    } else if is_cmd(&text, "stop") || is_stop_id_form {
        // Check if /stop has a valid queue ID argument (exactly 7 hex chars, e.g. "/stop A394FDA")
        let stop_queue_id = text.strip_prefix("/stop").and_then(|s| {
            // Handle /stop@botname format
            let s = if s.starts_with('@') {
                s.find(' ').map(|i| &s[i..]).unwrap_or("")
            } else {
                s
            };
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
            msg_debug(&format!(
                "[handle_message] routing → /stop queue_id={}",
                queue_id
            ));
            println!("  [{timestamp}] ◀ [{user_name}] /stop {queue_id}");
            handle_stop_queue_item(&bot, chat_id, &state, &queue_id).await?;
        } else {
            msg_debug(&format!("[handle_message] routing → /stop"));
            println!("  [{timestamp}] ◀ [{user_name}] /stop");
            handle_stop_command(&bot, chat_id, &state).await?;
        }
    } else if is_cmd(&text, "help") {
        msg_debug(&format!("[handle_message] routing → /help"));
        println!("  [{timestamp}] ◀ [{user_name}] /help");
        handle_help_command(&bot, chat_id, &state).await?;
    } else if is_cmd(&text, "start") {
        msg_debug(&format!("[handle_message] routing → /start"));
        println!("  [{timestamp}] ◀ [{user_name}] /start");
        handle_start_command(&bot, chat_id, &text, &state, token).await?;
    } else if is_cmd(&text, "clear") {
        msg_debug(&format!("[handle_message] routing → /clear"));
        println!("  [{timestamp}] ◀ [{user_name}] /clear");
        handle_clear_command(&bot, chat_id, &state).await?;
        println!("  [{timestamp}] ▶ [{user_name}] Session cleared");
    } else if is_cmd(&text, "pwd") {
        msg_debug(&format!("[handle_message] routing → /pwd"));
        println!("  [{timestamp}] ◀ [{user_name}] /pwd");
        handle_pwd_command(&bot, chat_id, &state).await?;
    } else if is_cmd(&text, "session") {
        msg_debug(&format!("[handle_message] routing → /session"));
        println!("  [{timestamp}] ◀ [{user_name}] /session");
        handle_session_command(&bot, chat_id, &state).await?;
    } else if is_cmd(&text, "down") {
        msg_debug(&format!("[handle_message] routing → /down"));
        println!(
            "  [{timestamp}] ◀ [{user_name}] /down {}",
            text.strip_prefix("/down").unwrap_or("").trim()
        );
        handle_down_command(&bot, chat_id, &text, &state).await?;
    } else if is_cmd(&text, "public") {
        msg_debug("[handle_message] routing → /public");
        println!(
            "  [{timestamp}] ◀ [{user_name}] /public {}",
            text.strip_prefix("/public").unwrap_or("").trim()
        );
        handle_public_command(&bot, chat_id, &text, &state, token, is_group_chat, is_owner).await?;
    } else if is_cmd(&text, "availabletools") {
        msg_debug("[handle_message] routing → /availabletools");
        println!("  [{timestamp}] ◀ [{user_name}] /availabletools");
        {
            let _m = get_model(&state.lock().await.settings, chat_id);
            if detect_provider(_m.as_deref()) != "claude" {
                tg!(
                    "send_message",
                    bot.send_message(chat_id, "Tool permissions are not supported in this mode.")
                        .await
                )?;
            } else {
                handle_availabletools_command(&bot, chat_id, &state).await?;
            }
        }
    } else if is_cmd(&text, "allowedtools") {
        msg_debug("[handle_message] routing → /allowedtools");
        println!("  [{timestamp}] ◀ [{user_name}] /allowedtools");
        {
            let _m = get_model(&state.lock().await.settings, chat_id);
            if detect_provider(_m.as_deref()) != "claude" {
                tg!(
                    "send_message",
                    bot.send_message(chat_id, "Tool permissions are not supported in this mode.")
                        .await
                )?;
            } else {
                handle_allowedtools_command(&bot, chat_id, &state).await?;
            }
        }
    } else if is_cmd(&text, "setpollingtime") {
        msg_debug("[handle_message] routing → /setpollingtime");
        println!(
            "  [{timestamp}] ◀ [{user_name}] /setpollingtime {}",
            text.strip_prefix("/setpollingtime").unwrap_or("").trim()
        );
        handle_setpollingtime_command(&bot, chat_id, &text, &state).await?;
    } else if is_cmd(&text, "stt_model") {
        msg_debug("[handle_message] routing → /stt_model");
        println!(
            "  [{timestamp}] ◀ [{user_name}] /stt_model {}",
            command_args(&text)
        );
        handle_stt_model_command(&bot, chat_id, &text, &state, token).await?;
    } else if is_cmd(&text, "model") {
        msg_debug("[handle_message] routing → /model");
        println!(
            "  [{timestamp}] ◀ [{user_name}] /model {}",
            text.strip_prefix("/model").unwrap_or("").trim()
        );
        handle_model_command(&bot, chat_id, &text, &state, token).await?;
    } else if is_cmd(&text, "effort") {
        msg_debug("[handle_message] routing → /effort");
        println!(
            "  [{timestamp}] ◀ [{user_name}] /effort {}",
            command_args(&text)
        );
        handle_effort_command(&bot, chat_id, &text, &state, token).await?;
    } else if is_cmd(&text, "fast") {
        msg_debug("[handle_message] routing → /fast");
        println!(
            "  [{timestamp}] ◀ [{user_name}] /fast {}",
            command_args(&text)
        );
        handle_fast_command(&bot, chat_id, &text, &state, token).await?;
    } else if is_cmd(&text, "greeting") {
        msg_debug("[handle_message] routing → /greeting");
        println!("  [{timestamp}] ◀ [{user_name}] /greeting");
        handle_greeting_command(&bot, chat_id, &state).await?;
    } else if is_cmd(&text, "debug") {
        msg_debug("[handle_message] routing → /debug");
        println!("  [{timestamp}] ◀ [{user_name}] /debug");
        handle_debug_command(&bot, chat_id, &state, token).await?;
    } else if is_cmd(&text, "envvars") {
        msg_debug("[handle_message] routing → /envvars");
        println!("  [{timestamp}] ◀ [{user_name}] /envvars");
        // Non-owner-in-group is already blocked by the `is_owner_only_command`
        // gate above; non-owner-in-1:1 is rejected during the imprinting
        // check earlier in this function. So only owners reach this point.
        if is_group_chat {
            msg_debug("[handle_message] /envvars rejected: group chat");
            shared_rate_limit_wait(&state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(
                    chat_id,
                    "/envvars is only available in a 1:1 chat with the bot."
                )
                .await
            )?;
        } else {
            handle_envvars_command(&bot, chat_id, &state).await?;
        }
    } else if is_cmd(&text, "usechrome") {
        msg_debug("[handle_message] routing → /usechrome");
        println!("  [{timestamp}] ◀ [{user_name}] /usechrome");
        handle_usechrome_command(&bot, chat_id, &state, token).await?;
    } else if is_cmd(&text, "silent") {
        msg_debug("[handle_message] routing → /silent");
        println!("  [{timestamp}] ◀ [{user_name}] /silent");
        handle_silent_command(&bot, chat_id, &state, token).await?;
    } else if is_cmd(&text, "queue") {
        msg_debug("[handle_message] routing → /queue");
        println!("  [{timestamp}] ◀ [{user_name}] /queue");
        handle_queue_command(&bot, chat_id, &state, token).await?;
    } else if is_cmd(&text, "direct") {
        msg_debug("[handle_message] routing → /direct");
        println!("  [{timestamp}] ◀ [{user_name}] /direct");
        handle_direct_command(&bot, chat_id, &msg, &state, token, is_owner).await?;
    } else if is_cmd(&text, "contextlevel") {
        msg_debug("[handle_message] routing → /contextlevel");
        println!(
            "  [{timestamp}] ◀ [{user_name}] /contextlevel {}",
            text.strip_prefix("/contextlevel").unwrap_or("").trim()
        );
        handle_contextlevel_command(&bot, chat_id, &text, &state, token, is_group_chat).await?;
    } else if is_cmd(&text, "query") {
        let body = text.strip_prefix("/query").unwrap_or("").trim();
        if body.is_empty() {
            msg_debug("[handle_message] /query with empty body, ignoring");
            shared_rate_limit_wait(&state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(
                    chat_id,
                    "Usage: /query <message>\nExample: /query@botname hello"
                )
                .await
            )?;
        } else {
            msg_debug(&format!(
                "[handle_message] routing → text_message (/query), body={:?}",
                truncate_str(body, 80)
            ));
            println!("  [{timestamp}] ◀ [{user_name}] {body}");
            handle_text_message(&bot, chat_id, body, &state, &user_name, false).await?;
        }
    } else if is_cmd(&text, "loop") {
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
            // the `plan` agent). Other providers (currently Agy) are rejected.
            let provider = {
                let _m = get_model(&state.lock().await.settings, chat_id);
                detect_provider(_m.as_deref()).to_string()
            };
            if provider != "claude" && provider != "codex" && provider != "opencode" {
                shared_rate_limit_wait(&state, chat_id).await;
                tg!(
                    "send_message",
                    bot.send_message(
                        chat_id,
                        "/loop currently supports Claude, Codex, or OpenCode models only."
                    )
                    .await
                )?;
            } else {
                // Reject if a loop is already running for this chat
                {
                    let data = state.lock().await;
                    if data.loop_states.contains_key(&chat_id) {
                        drop(data);
                        shared_rate_limit_wait(&state, chat_id).await;
                        tg!(
                            "send_message",
                            bot.send_message(
                                chat_id,
                                "A loop is already running. Use /stop to cancel it first."
                            )
                            .await
                        )?;
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
                msg_debug(&format!(
                    "[handle_message] routing → /loop, max_iter={}, request={:?}",
                    max_iter,
                    truncate_str(request, 80)
                ));
                println!(
                    "  [{timestamp}] ◀ [{user_name}] /loop {} {}",
                    max_iter,
                    truncate_str(request, 60)
                );
                {
                    let mut data = state.lock().await;
                    data.loop_states.insert(
                        chat_id,
                        LoopState {
                            original_request: request.to_string(),
                            max_iterations: max_iter,
                            remaining: max_iter,
                        },
                    );
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
    } else if is_cmd(&text, "setendhook_clear") {
        msg_debug("[handle_message] routing → /setendhook_clear");
        println!("  [{timestamp}] ◀ [{user_name}] /setendhook_clear");
        handle_setendhook_clear_command(&bot, chat_id, &state, token).await?;
    } else if is_cmd(&text, "setendhook") {
        msg_debug("[handle_message] routing → /setendhook");
        println!("  [{timestamp}] ◀ [{user_name}] /setendhook");
        handle_setendhook_command(&bot, chat_id, &text, &state, token).await?;
    } else if is_cmd(&text, "instruction_clear") {
        msg_debug("[handle_message] routing → /instruction_clear");
        println!("  [{timestamp}] ◀ [{user_name}] /instruction_clear");
        handle_instruction_clear_command(&bot, chat_id, &state, token).await?;
    } else if is_cmd(&text, "instruction") {
        msg_debug("[handle_message] routing → /instruction");
        println!("  [{timestamp}] ◀ [{user_name}] /instruction");
        handle_instruction_command(&bot, chat_id, &text, &state, token).await?;
    } else if is_cmd(&text, "allowed") {
        msg_debug("[handle_message] routing → /allowed");
        println!(
            "  [{timestamp}] ◀ [{user_name}] /allowed {}",
            text.strip_prefix("/allowed").unwrap_or("").trim()
        );
        {
            let _m = get_model(&state.lock().await.settings, chat_id);
            if detect_provider(_m.as_deref()) != "claude" {
                tg!(
                    "send_message",
                    bot.send_message(chat_id, "Tool permissions are not supported in this mode.")
                        .await
                )?;
            } else {
                handle_allowed_command(&bot, chat_id, &text, &state, token).await?;
            }
        }
    } else if text.starts_with('/')
        && is_workspace_id(text[1..].split_whitespace().next().unwrap_or(""))
    {
        let workspace_id = text[1..].split_whitespace().next().unwrap();
        msg_debug(&format!(
            "[handle_message] routing → workspace_resume: {}",
            workspace_id
        ));
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
        msg_debug(&format!(
            "[handle_message] routing → text_message (;prefix), stripped={:?}",
            truncate_str(&stripped, 80)
        ));
        println!("  [{timestamp}] ◀ [{user_name}] {preview}");
        handle_text_message(&bot, chat_id, &stripped, &state, &user_name, false).await?;
    } else {
        msg_debug(&format!(
            "[handle_message] routing → text_message (plain), require_prefix={}",
            require_prefix
        ));
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
<code>/model &lt;name&gt;</code> — Set model (claude/codex/agy/opencode or provider:model)
<code>/stt_model</code> — Show current speech recognition model
<code>/stt_model &lt;name|path:...&gt;</code> — Set transcriptor STT model
<code>/effort</code> — Show current Claude/Codex effort
<code>/effort &lt;level&gt;</code> — Set effort (Claude: low/medium/high/xhigh/max, Codex: minimal/low/medium/high/xhigh, or reset)
<code>/fast</code> — Toggle Codex fast service tier
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
    tg!(
        "send_message",
        bot.send_message(chat_id, help)
            .parse_mode(ParseMode::Html)
            .await
    )?;

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
    msg_debug(&format!(
        "[handle_start_command] chat_id={}, path_str={:?}",
        chat_id.0, path_str
    ));

    // Determine current provider (Claude vs Codex vs Agy)
    let original_provider_str: &str;
    let mut provider_str;
    let mut provider = {
        let data = state.lock().await;
        let model = get_model(&data.settings, chat_id);
        let p = detect_provider(model.as_deref());
        msg_debug(&format!(
            "[handle_start_command] model={:?}, provider={}",
            model, p
        ));
        provider_str = p;
        provider_to_session(p)
    };
    original_provider_str = provider_str;

    // Tracks whether the user's input was a path-like value (vs. a session
    // identifier). Same-path no-op only applies when path-intent is true —
    // session identifiers may resolve to a path matching current cwd while
    // intending to switch to a different session there.
    let mut is_path_intent = false;
    let canonical_path = if path_str.is_empty() {
        // Create random workspace directory
        let Some(home) = dirs::home_dir() else {
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, "Error: cannot determine home directory.")
                    .await
            )?;
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
            tg!(
                "send_message",
                bot.send_message(chat_id, format!("Error: failed to create workspace: {}", e))
                    .await
            )?;
            return Ok(());
        }
        new_dir.display().to_string()
    } else if path_str.starts_with('/')
        || path_str.starts_with("~/")
        || path_str.starts_with("~\\")
        || path_str == "~"
        || path_str.starts_with("./")
        || path_str.starts_with(".\\")
        || path_str == "."
        || path_str == ".."
        || path_str.starts_with("../")
        || path_str.starts_with("..\\")
        || (path_str.len() >= 3
            && path_str.as_bytes()[1] == b':'
            && (path_str.as_bytes()[2] == b'\\' || path_str.as_bytes()[2] == b'/'))
    {
        is_path_intent = true;
        // Path mode: expand ~ and validate
        let expanded =
            if path_str.starts_with("~/") || path_str.starts_with("~\\") || path_str == "~" {
                if let Some(home) = dirs::home_dir() {
                    home.join(
                        path_str
                            .strip_prefix("~/")
                            .or_else(|| path_str.strip_prefix("~\\"))
                            .unwrap_or(""),
                    )
                    .display()
                    .to_string()
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
                tg!(
                    "send_message",
                    bot.send_message(
                        chat_id,
                        format!("Error: failed to create '{}': {}", expanded, e)
                    )
                    .await
                )?;
                return Ok(());
            }
        } else if !path.is_dir() {
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(
                    chat_id,
                    format!("Error: '{}' is not a directory.", expanded)
                )
                .await
            )?;
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
            SessionProvider::Claude => &[
                SessionProvider::Codex,
                SessionProvider::Agy,
                SessionProvider::OpenCode,
            ],
            SessionProvider::Codex => &[
                SessionProvider::Claude,
                SessionProvider::Agy,
                SessionProvider::OpenCode,
            ],
            SessionProvider::Agy => &[
                SessionProvider::Claude,
                SessionProvider::Codex,
                SessionProvider::OpenCode,
            ],
            SessionProvider::OpenCode => &[
                SessionProvider::Claude,
                SessionProvider::Codex,
                SessionProvider::Agy,
            ],
        };

        // Helper closure: try resolve_session + ai_sessions for a given provider
        let try_resolve = |prov: SessionProvider, prov_str: &str| -> Option<String> {
            msg_debug(&format!(
                "[try_resolve] provider={}, query={:?}",
                prov_str, path_str
            ));
            if let Some(info) = resolve_session(path_str, prov) {
                let path = Path::new(&info.cwd);
                let is_dir = path.is_dir();
                msg_debug(&format!(
                    "[try_resolve] resolve_session found: cwd={:?}, is_dir={}",
                    info.cwd, is_dir
                ));
                if is_dir {
                    let canonical = path
                        .canonicalize()
                        .map(crate::utils::format::strip_unc_prefix)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| info.cwd.clone());
                    convert_and_save_session(&info, &canonical);
                    msg_debug(&format!(
                        "[try_resolve] resolved via resolve_session: canonical={}",
                        canonical
                    ));
                    return Some(canonical);
                } else {
                    msg_debug(&format!(
                        "[try_resolve] resolve_session cwd not a dir: {:?}",
                        info.cwd
                    ));
                }
            } else {
                msg_debug(&format!(
                    "[try_resolve] resolve_session returned None for provider={}",
                    prov_str
                ));
            }
            // Try ai_sessions/{id}.json
            msg_debug(&format!(
                "[try_resolve] trying ai_sessions/{}.json",
                path_str
            ));
            let result = ai_screen::ai_sessions_dir().and_then(|dir| {
                let file = dir.join(format!("{}.json", path_str));
                let content = fs::read_to_string(&file).ok()?;
                let sd: SessionData = serde_json::from_str(&content).ok()?;
                if !sd.provider.is_empty() && sd.provider != prov_str {
                    msg_debug(&format!(
                        "[try_resolve] ai_sessions/{}.json provider mismatch: {} != {}",
                        path_str, sd.provider, prov_str
                    ));
                    return None;
                }
                let p = Path::new(&sd.current_path);
                let is_dir = p.is_dir();
                msg_debug(&format!(
                    "[try_resolve] ai_sessions/{}.json: current_path={:?}, provider={}, is_dir={}",
                    path_str, sd.current_path, sd.provider, is_dir
                ));
                if is_dir {
                    Some(sd.current_path.clone())
                } else {
                    None
                }
            });
            msg_debug(&format!(
                "[try_resolve] ai_sessions result: {}",
                if result.is_some() { "found" } else { "None" }
            ));
            result
        };

        if let Some(cp) = try_resolve(provider, provider_str) {
            msg_debug(&format!(
                "[handle_start_command] resolved with current provider: path={}",
                cp
            ));
            cp
        } else {
            // Cross-provider fallback: try all other providers in order
            let mut cross_result: Option<(String, SessionProvider, &'static str)> = None;
            for &fp in fallback_providers {
                let fp_str = session_provider_str(fp);
                let available = match fp {
                    SessionProvider::Claude => claude::is_claude_available(),
                    SessionProvider::Codex => codex::is_codex_available(),
                    SessionProvider::Agy => agy::is_agy_available(),
                    SessionProvider::OpenCode => opencode::is_opencode_available(),
                };
                if !available {
                    msg_debug(&format!(
                        "[handle_start_command] cross-provider skip: {} not available",
                        fp_str
                    ));
                    continue;
                }
                msg_debug(&format!(
                    "[handle_start_command] cross-provider attempt: {}",
                    fp_str
                ));
                if let Some(cp) = try_resolve(fp, fp_str) {
                    cross_result = Some((cp, fp, fp_str));
                    break;
                }
            }

            if let Some((cp, resolved_prov, resolved_prov_str)) = cross_result {
                // Cross-provider fallback: switch provider and model to resolved provider
                msg_debug(&format!(
                    "[handle_start_command] cross-provider fallback: switching from {} to {}",
                    provider_str, resolved_prov_str
                ));
                provider = resolved_prov;
                provider_str = resolved_prov_str;
                {
                    let mut data = state.lock().await;
                    msg_debug(&format!(
                        "[handle_start_command] cross-provider: setting model to {:?}",
                        resolved_prov_str
                    ));
                    data.settings
                        .models
                        .insert(chat_id.0.to_string(), resolved_prov_str.to_string());
                    save_bot_settings(token, &data.settings);
                    // Mirror /model provider-switch cleanup: cancel in-flight task and drop
                    // queued messages / loop state. Their captured pending_uploads point at the
                    // old workspace's files, and verify-loop feedback was authored against the
                    // old provider's session — both would be misapplied under the new provider.
                    cancel_in_progress_task_locked(&data, chat_id);
                    let dropped_queue = data
                        .message_queues
                        .remove(&chat_id)
                        .map(|q| q.len())
                        .unwrap_or(0);
                    data.loop_states.remove(&chat_id);
                    data.loop_feedback.remove(&chat_id);
                    if let Some(session) = data.sessions.get_mut(&chat_id) {
                        msg_debug(&format!("[handle_start_command] cross-provider: clearing old session (len={}, uploads={}, queue={}, sid={:?}, path={:?})",
                            session.history.len(), session.pending_uploads.len(), dropped_queue, session.session_id, session.current_path));
                        session.session_id = None;
                        session.current_path = None;
                        session.history.clear();
                        session.clear_pending_uploads();
                    } else {
                        msg_debug(&format!("[handle_start_command] cross-provider: no existing session to clear (queue={})", dropped_queue));
                    }
                }
                cp
            } else {
                // Final fallback: try as plain path
                msg_debug(&format!("[handle_start_command] all session resolves failed, trying as plain path: {:?}", path_str));
                let path = Path::new(path_str);
                if path.exists() && path.is_dir() {
                    is_path_intent = true;
                    let resolved = path
                        .canonicalize()
                        .map(crate::utils::format::strip_unc_prefix)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| path_str.to_string());
                    msg_debug(&format!(
                        "[handle_start_command] plain path resolved: {}",
                        resolved
                    ));
                    resolved
                } else {
                    msg_debug(&format!(
                        "[handle_start_command] plain path failed: exists={}, is_dir={}",
                        path.exists(),
                        path.is_dir()
                    ));
                    shared_rate_limit_wait(state, chat_id).await;
                    tg!(
                        "send_message",
                        bot.send_message(
                            chat_id,
                            format!("Error: no session or directory found for '{}'.", path_str)
                        )
                        .await
                    )?;
                    return Ok(());
                }
            }
        }
    };

    // Same-path no-op: if the user typed a path-like input that resolved to the
    // session's existing current_path, treat it as a confirmation and skip the
    // rest. Without this guard, the trailing block (3598+) would clear
    // pending_uploads, null session_id, and overwrite in-memory history with
    // the on-disk version — destroying the user's in-progress state for a
    // no-op intent.
    //
    // Only path-intent invocations qualify (modes B and final-fallback D).
    // Session-identifier inputs (mode C) intentionally proceed even at the
    // same path because the user may be switching to a different session
    // whose cwd happens to match the current one.
    //
    // Cross-provider fallback (handled inside the C branch) sets
    // current_path = None before reaching here, so the comparison naturally
    // fails for that case and execution proceeds.
    if is_path_intent {
        let same_path = {
            let data = state.lock().await;
            data.sessions
                .get(&chat_id)
                .and_then(|s| s.current_path.clone())
                .as_deref()
                == Some(canonical_path.as_str())
        };
        if same_path {
            msg_debug(&format!(
                "[handle_start_command] same-path no-op: {}",
                canonical_path
            ));
            shared_rate_limit_wait(state, chat_id).await;
            let display = crate::utils::format::to_shell_path(&canonical_path);
            tg!(
                "send_message",
                bot.send_message(
                    chat_id,
                    format!("Already at <code>{}</code>.", html_escape(&display))
                )
                .parse_mode(ParseMode::Html)
                .await
            )?;
            return Ok(());
        }
    }

    // Try to load existing session for this path
    msg_debug(&format!(
        "[handle_start_command] canonical_path={:?}, provider={}",
        canonical_path, provider_str
    ));
    let existing = load_existing_session(&canonical_path, provider_str);
    msg_debug(&format!(
        "[handle_start_command] load_existing_session → {}",
        if existing.is_some() { "found" } else { "None" }
    ));

    // If no local session, try converting the latest external session for this path
    let existing = if existing.is_some() {
        existing
    } else if let Some(info) = find_latest_session_by_cwd(&canonical_path, provider) {
        msg_debug(&format!(
            "[handle_start_command] fallback found: jsonl={}, session_id={}",
            info.jsonl_path.display(),
            info.session_id
        ));
        convert_and_save_session(&info, &canonical_path);
        let reloaded = load_existing_session(&canonical_path, provider_str);
        msg_debug(&format!(
            "[handle_start_command] after convert, reload → {}",
            if reloaded.is_some() { "found" } else { "None" }
        ));
        reloaded
    } else {
        msg_debug("[handle_start_command] no external session found either");
        None
    };

    let mut response_lines = Vec::new();
    let mut is_restored = false;

    // Notify user if provider was auto-switched via cross-provider fallback
    if provider_str != original_provider_str {
        msg_debug(&format!(
            "[handle_start_command] provider auto-switched: {} → {}",
            original_provider_str, provider_str
        ));
        response_lines.push(format!("Model switched to **{}**.", provider_str));
    }

    {
        let mut data = state.lock().await;
        // Session-change cleanup: if /start switches to a different workspace path OR
        // a different session_id at the same path, drop the in-flight task, queued
        // messages, and loop state. They were authored against the old session and
        // would be misapplied — the queued pending_uploads point at the old session's
        // uploads, verify-loop feedback was authored against the old session, and
        // an in-flight task's writeback would resurrect the old session_id and history
        // into the just-loaded new session (corrupting the on-disk session file).
        // Idempotent with the cross-provider cleanup above (which leaves
        // current_path = None, so path_changed is always true on re-entry).
        let old_path = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.current_path.clone());
        let old_sid = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.session_id.clone());
        let new_sid = existing.as_ref().and_then(|(sd, _)| {
            if sd.session_id.is_empty() {
                None
            } else {
                Some(sd.session_id.clone())
            }
        });
        let path_changed = old_path.as_deref() != Some(canonical_path.as_str());
        let sid_changed = old_sid != new_sid;
        if path_changed || sid_changed {
            cancel_in_progress_task_locked(&data, chat_id);
            data.message_queues.remove(&chat_id);
            data.loop_states.remove(&chat_id);
            data.loop_feedback.remove(&chat_id);
        }
        let session = data.sessions.entry(chat_id).or_insert_with(|| ChatSession {
            session_id: None,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            pending_uploads_added_at: None,
        });
        session.session_id = None; // Clear stale session_id from previous workspace
                                   // Drop uploads only when the workspace path changed — upload paths point at
                                   // the old workspace's files. When only session_id changes at the same path
                                   // (mode C, session-identifier swap), uploads still reference valid files in
                                   // the current workspace and remain relevant to the user's next message.
        if path_changed {
            session.clear_pending_uploads();
        }

        if let Some((session_data, _)) = &existing {
            if session_data_has_active_session(session_data) {
                session.session_id = Some(session_data.session_id.clone());
                session.current_path = Some(canonical_path.clone());
                session.history = session_data.history.clone();
                is_restored = true;
                msg_debug(&format!(
                    "[handle_start_command] restored: session_id={}, path={}, history_len={}",
                    session_data.session_id,
                    canonical_path,
                    session_data.history.len()
                ));

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ▶ Session restored: {canonical_path}");
                response_lines.push(format!(
                    "[{}] Session restored at `{}`.",
                    provider_str, canonical_path
                ));
                if let Some(folder_name) = std::path::Path::new(&canonical_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                {
                    if is_workspace_id(folder_name)
                        && dirs::home_dir()
                            .map(|h| {
                                h.join(".cokacdir")
                                    .join("workspace")
                                    .join(folder_name)
                                    .is_dir()
                            })
                            .unwrap_or(false)
                    {
                        response_lines
                            .push(format!("Use /{} to resume this session.", folder_name));
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
                cleanup_session_files(&canonical_path, provider_str);
                session.session_id = None;
                session.current_path = Some(canonical_path.clone());
                session.history.clear();
                msg_debug(&format!(
                    "[handle_start_command] placeholder session found; starting fresh at path={}",
                    canonical_path
                ));

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ▶ Session started: {canonical_path}");
                response_lines.push(format!(
                    "[{}] Session started at `{}`.",
                    provider_str, canonical_path
                ));
                if let Some(folder_name) = std::path::Path::new(&canonical_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                {
                    if is_workspace_id(folder_name)
                        && dirs::home_dir()
                            .map(|h| {
                                h.join(".cokacdir")
                                    .join("workspace")
                                    .join(folder_name)
                                    .is_dir()
                            })
                            .unwrap_or(false)
                    {
                        response_lines
                            .push(format!("Use /{} to resume this session.", folder_name));
                    }
                }
            }
        } else {
            session.session_id = None;
            session.current_path = Some(canonical_path.clone());
            session.history.clear();
            msg_debug(&format!(
                "[handle_start_command] new session: path={}",
                canonical_path
            ));

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Session started: {canonical_path}");
            response_lines.push(format!(
                "[{}] Session started at `{}`.",
                provider_str, canonical_path
            ));
            // Show workspace ID shortcut if this is a workspace directory
            if let Some(folder_name) = std::path::Path::new(&canonical_path)
                .file_name()
                .and_then(|n| n.to_str())
            {
                if is_workspace_id(folder_name)
                    && dirs::home_dir()
                        .map(|h| {
                            h.join(".cokacdir")
                                .join("workspace")
                                .join(folder_name)
                                .is_dir()
                        })
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
        data.settings
            .last_sessions
            .insert(chat_id.0.to_string(), canonical_path.clone());
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
            append_group_chat_log(
                chat_id.0,
                &GroupChatLogEntry {
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
                },
            );
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
    if s.len() != 36 {
        return false;
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let expected = [8, 4, 4, 4, 12];
    parts
        .iter()
        .zip(expected.iter())
        .all(|(p, &len)| p.len() == len && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Provider that owns the resolved session.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SessionProvider {
    Claude,
    Codex,
    Agy,
    OpenCode,
}

/// Detect provider from model prefix only (no availability fallback).
/// Returns "claude" when model is None or has no recognized prefix.
fn provider_from_model(model: Option<&str>) -> &'static str {
    if codex::is_codex_model(model) {
        "codex"
    } else if agy::is_agy_model(model) {
        "agy"
    } else if opencode::is_opencode_model(model) {
        "opencode"
    } else {
        "claude"
    }
}

/// Detect effective provider: from model prefix if set, otherwise from CLI availability.
fn detect_provider(model: Option<&str>) -> &'static str {
    if model.is_some() {
        provider_from_model(model)
    } else if !claude::is_claude_available() && codex::is_codex_available() {
        "codex"
    } else if !claude::is_claude_available() && agy::is_agy_available() {
        "agy"
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
        "agy" | "gemini" => SessionProvider::Agy,
        "opencode" => SessionProvider::OpenCode,
        _ => SessionProvider::Claude,
    }
}

/// Convert SessionProvider enum to provider name string.
fn session_provider_str(provider: SessionProvider) -> &'static str {
    match provider {
        SessionProvider::Claude => "claude",
        SessionProvider::Codex => "codex",
        SessionProvider::Agy => "agy",
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
    msg_debug(&format!(
        "[resolve_session] query={:?}, provider={:?}, is_uuid={}",
        query,
        provider,
        is_uuid(query)
    ));
    let result = match provider {
        SessionProvider::Claude => {
            if is_uuid(query) {
                resolve_claude_by_id(query).or_else(|| resolve_claude_by_name(query))
            } else {
                resolve_claude_by_name(query).or_else(|| resolve_claude_by_id(query))
            }
        }
        SessionProvider::Codex => resolve_codex_by_id(query),
        SessionProvider::Agy => resolve_agy_by_id(query),
        SessionProvider::OpenCode => resolve_opencode_by_id(query),
    };
    msg_debug(&format!(
        "[resolve_session] result={}",
        match &result {
            Some(r) => format!("found(cwd={:?}, session_id={})", r.cwd, r.session_id),
            None => "None".to_string(),
        }
    ));
    result
}

/// Claude: find `~/.claude/projects/*/{session_id}.jsonl`.
fn resolve_claude_by_id(session_id: &str) -> Option<ResolvedSession> {
    msg_debug(&format!("[resolve_claude_by_id] session_id={}", session_id));
    let projects_dir = dirs::home_dir()?.join(".claude").join("projects");
    if !projects_dir.is_dir() {
        msg_debug(&format!(
            "[resolve_claude_by_id] projects_dir not found: {}",
            projects_dir.display()
        ));
        return None;
    }
    let filename = format!("{}.jsonl", session_id);
    for entry in fs::read_dir(&projects_dir).ok()?.flatten() {
        if !entry.file_type().map_or(false, |t| t.is_dir()) {
            continue;
        }
        let jsonl_path = entry.path().join(&filename);
        if jsonl_path.exists() {
            msg_debug(&format!(
                "[resolve_claude_by_id] found: {}",
                jsonl_path.display()
            ));
            let cwd = extract_cwd_from_jsonl(&jsonl_path)?;
            return Some(ResolvedSession {
                cwd,
                jsonl_path,
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
        msg_debug(&format!(
            "[resolve_claude_by_name] projects_dir not found: {}",
            projects_dir.display()
        ));
        return None;
    }
    let name_lower = name.to_lowercase();
    for proj_entry in fs::read_dir(&projects_dir).ok()?.flatten() {
        if !proj_entry.file_type().map_or(false, |t| t.is_dir()) {
            continue;
        }
        let Ok(file_entries) = fs::read_dir(proj_entry.path()) else {
            continue;
        };
        for file_entry in file_entries.flatten() {
            let path = file_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(info) = find_session_by_title(&path, &name_lower) {
                msg_debug(&format!(
                    "[resolve_claude_by_name] found: session_id={}, cwd={:?}",
                    info.session_id, info.cwd
                ));
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
        if matched && cwd_found.is_some() {
            break;
        }
    }
    if matched {
        let cwd = cwd_found?;
        let session_id = path.file_stem()?.to_str()?.to_string();
        Some(ResolvedSession {
            cwd,
            jsonl_path: path.to_path_buf(),
            session_id,
            provider: SessionProvider::Claude,
        })
    } else {
        None
    }
}

/// Codex: recursively scan `~/.codex/sessions/` for a JSONL whose filename contains the UUID.
fn resolve_codex_by_id(session_id: &str) -> Option<ResolvedSession> {
    let sessions_dir = dirs::home_dir()?.join(".codex").join("sessions");
    if !sessions_dir.is_dir() {
        return None;
    }
    let suffix = format!("{}.jsonl", session_id);
    fn walk(dir: &Path, suffix: &str) -> Option<std::path::PathBuf> {
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
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
        cwd,
        jsonl_path,
        session_id: session_id.to_string(),
        provider: SessionProvider::Codex,
    })
}

/// Agy: resolve `~/.gemini/antigravity-cli/conversations/<id>.db|.pb`.
fn resolve_agy_by_id(session_id: &str) -> Option<ResolvedSession> {
    msg_debug(&format!("[resolve_agy_by_id] session_id={}", session_id));
    let path = agy::conversation_path(session_id)?;
    let cwd = find_agy_cwd_for_session(session_id)
        .or_else(|| extract_agy_cwd_from_conversation(&path))?;
    msg_debug(&format!(
        "[resolve_agy_by_id] found: cwd={:?}, file={}",
        cwd,
        path.display()
    ));
    Some(ResolvedSession {
        cwd,
        jsonl_path: path,
        session_id: session_id.to_string(),
        provider: SessionProvider::Agy,
    })
}

fn find_agy_cwd_for_session(session_id: &str) -> Option<String> {
    let cache = dirs::home_dir()?
        .join(".gemini")
        .join("antigravity-cli")
        .join("cache")
        .join("last_conversations.json");
    let content = fs::read_to_string(cache).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let obj = val.as_object()?;
    for (cwd, sid) in obj {
        if sid.as_str() == Some(session_id) && Path::new(cwd).is_dir() {
            return Some(cwd.clone());
        }
    }
    None
}

fn extract_agy_cwd_from_conversation(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let mut current = Vec::new();
    let mut candidates = Vec::new();
    for b in bytes {
        if b.is_ascii_graphic() || b == b' ' {
            current.push(b);
        } else if current.len() >= 4 {
            if let Ok(s) = String::from_utf8(std::mem::take(&mut current)) {
                candidates.push(s);
            }
        } else {
            current.clear();
        }
    }
    if current.len() >= 4 {
        if let Ok(s) = String::from_utf8(current) {
            candidates.push(s);
        }
    }
    for s in candidates {
        let raw = s
            .strip_prefix("file://")
            .unwrap_or(&s)
            .trim_matches(|c| c == '"' || c == '\'' || c == '`');
        if raw.starts_with('/') && Path::new(raw).is_dir() {
            return Some(raw.to_string());
        }
    }
    None
}

/// OpenCode: query `~/.local/share/opencode/opencode.db` for a matching session ID.
fn resolve_opencode_by_id(session_id: &str) -> Option<ResolvedSession> {
    msg_debug(&format!(
        "[resolve_opencode_by_id] session_id={}",
        session_id
    ));
    let db_path = dirs::home_dir()?
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db");
    if !db_path.is_file() {
        msg_debug("[resolve_opencode_by_id] db not found");
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let mut stmt = conn
        .prepare("SELECT id, directory FROM session WHERE id = ?1 LIMIT 1")
        .ok()?;
    let result = stmt
        .query_row(rusqlite::params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;
    let directory = result.1;
    if directory.is_empty() {
        return None;
    }
    msg_debug(&format!(
        "[resolve_opencode_by_id] found: directory={:?}",
        directory
    ));
    Some(ResolvedSession {
        cwd: directory,
        jsonl_path: db_path,
        session_id: session_id.to_string(),
        provider: SessionProvider::OpenCode,
    })
}

/// Convert an external JSONL session to cokacdir SessionData and save it.
/// Re-converts if the source JSONL is newer than the existing JSON, or if the
/// existing JSON is a cleared/incomplete placeholder.
fn convert_and_save_session(info: &ResolvedSession, canonical_path: &str) {
    msg_debug(&format!(
        "[convert_session] start: jsonl={}, session_id={}, canonical_path={:?}",
        info.jsonl_path.display(),
        info.session_id,
        canonical_path
    ));
    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        msg_debug("[convert_session] ai_sessions_dir() returned None");
        return;
    };
    let target = sessions_dir.join(format!("{}.json", info.session_id));
    let provider_str = session_provider_str(info.provider);
    msg_debug(&format!("[convert_session] target={}", target.display()));
    if target.exists() {
        if !converted_session_file_is_complete(
            &target,
            &info.session_id,
            canonical_path,
            provider_str,
        ) {
            msg_debug(
                "[convert_session] target exists but is incomplete or mismatched; reconverting",
            );
        } else {
            let source_mtime = info
                .jsonl_path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok());
            let target_mtime = target.metadata().ok().and_then(|m| m.modified().ok());
            msg_debug(&format!(
                "[convert_session] target exists, source_mtime={:?}, target_mtime={:?}",
                source_mtime, target_mtime
            ));
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
    }

    let parser = match info.provider {
        SessionProvider::Claude => parse_claude_jsonl,
        SessionProvider::Codex => parse_codex_jsonl,
        SessionProvider::Agy => parse_agy_session,
        SessionProvider::OpenCode => parse_opencode_session,
    };
    msg_debug(&format!(
        "[convert_session] parsing with provider={:?}",
        info.provider
    ));
    let Some(session_data) = parser(&info.jsonl_path, &info.session_id, canonical_path) else {
        msg_debug("[convert_session] parser returned None");
        return;
    };
    msg_debug(&format!(
        "[convert_session] parsed: history_len={}, provider={}",
        session_data.history.len(),
        session_data.provider
    ));
    let _ = fs::create_dir_all(&sessions_dir);
    if let Ok(json) = serde_json::to_string_pretty(&session_data) {
        let write_result = fs::write(&target, &json);
        msg_debug(&format!(
            "[convert_session] write result={:?}, bytes={}",
            write_result,
            json.len()
        ));
    } else {
        msg_debug("[convert_session] serde_json::to_string_pretty failed");
    }

    crate::services::session_archive::archive_and_save_session(
        provider_str,
        &info.jsonl_path,
        &info.session_id,
        canonical_path,
    );
}

fn converted_session_file_is_complete(
    target: &Path,
    session_id: &str,
    canonical_path: &str,
    provider: &str,
) -> bool {
    fs::read_to_string(target)
        .ok()
        .and_then(|content| serde_json::from_str::<SessionData>(&content).ok())
        .map(|data| {
            data.session_id == session_id
                && data.current_path == canonical_path
                && !data.history.is_empty()
                && (data.provider.is_empty() || data.provider == provider)
        })
        .unwrap_or(false)
}

fn session_data_has_active_session(session_data: &SessionData) -> bool {
    !session_data.session_id.is_empty()
}

#[cfg(test)]
mod session_conversion_tests {
    use super::{
        converted_session_file_is_complete, session_data_has_active_session, HistoryItem,
        HistoryType, SessionData,
    };

    fn write_session_file(path: &std::path::Path, data: &SessionData) {
        let json = serde_json::to_string_pretty(data).unwrap();
        std::fs::write(path, json).unwrap();
    }

    fn valid_session_data() -> SessionData {
        SessionData {
            session_id: "7a34c35a-d26f-4cf8-afe4-6ab2111c8afc".to_string(),
            history: vec![HistoryItem {
                item_type: HistoryType::User,
                content: "hello".to_string(),
            }],
            current_path: "/tmp/cokacdir-session".to_string(),
            created_at: "2026-05-21 00:00:00".to_string(),
            provider: "claude".to_string(),
        }
    }

    #[test]
    fn cleared_placeholder_is_not_a_complete_conversion_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"current_path":"/tmp/cokacdir-session","provider":"claude"}"#,
        )
        .unwrap();

        assert!(!converted_session_file_is_complete(
            &path,
            "7a34c35a-d26f-4cf8-afe4-6ab2111c8afc",
            "/tmp/cokacdir-session",
            "claude",
        ));
    }

    #[test]
    fn complete_converted_session_can_use_mtime_fast_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let data = valid_session_data();
        write_session_file(&path, &data);

        assert!(converted_session_file_is_complete(
            &path,
            &data.session_id,
            &data.current_path,
            &data.provider,
        ));
    }

    #[test]
    fn mismatched_or_empty_converted_session_is_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let mut data = valid_session_data();
        data.history.clear();
        write_session_file(&path, &data);
        assert!(!converted_session_file_is_complete(
            &path,
            &data.session_id,
            &data.current_path,
            &data.provider,
        ));

        data = valid_session_data();
        data.current_path = "/tmp/other-session".to_string();
        write_session_file(&path, &data);
        assert!(!converted_session_file_is_complete(
            &path,
            &data.session_id,
            "/tmp/cokacdir-session",
            &data.provider,
        ));

        data = valid_session_data();
        data.provider = "codex".to_string();
        write_session_file(&path, &data);
        assert!(!converted_session_file_is_complete(
            &path,
            &data.session_id,
            &data.current_path,
            "claude",
        ));
    }

    #[test]
    fn active_session_requires_non_empty_session_id() {
        let mut data = valid_session_data();
        assert!(session_data_has_active_session(&data));

        data.session_id.clear();
        assert!(!session_data_has_active_session(&data));
    }
}

/// Find the most recently modified external session whose cwd matches the given path.
fn find_latest_session_by_cwd(
    canonical_path: &str,
    provider: SessionProvider,
) -> Option<ResolvedSession> {
    msg_debug(&format!(
        "[find_latest_by_cwd] canonical_path={:?}, provider={:?}",
        canonical_path, provider
    ));
    let result = match provider {
        SessionProvider::Claude => find_latest_claude_by_cwd(canonical_path),
        SessionProvider::Codex => find_latest_codex_by_cwd(canonical_path),
        SessionProvider::Agy => find_latest_agy_by_cwd(canonical_path),
        SessionProvider::OpenCode => find_latest_opencode_by_cwd(canonical_path),
    };
    msg_debug(&format!(
        "[find_latest_by_cwd] result={}",
        if result.is_some() { "found" } else { "None" }
    ));
    result
}

/// Claude: scan all `~/.claude/projects/*/*.jsonl` for the latest session matching cwd.
fn find_latest_claude_by_cwd(canonical_path: &str) -> Option<ResolvedSession> {
    let projects_dir = dirs::home_dir()?.join(".claude").join("projects");
    msg_debug(&format!(
        "[find_claude_by_cwd] projects_dir={}, is_dir={}",
        projects_dir.display(),
        projects_dir.is_dir()
    ));
    if !projects_dir.is_dir() {
        return None;
    }
    let mut best_path: Option<std::path::PathBuf> = None;
    let mut best_time = std::time::UNIX_EPOCH;
    let mut scan_count = 0u32;
    let mut cwd_mismatch_sample: Option<String> = None;
    for proj_entry in fs::read_dir(&projects_dir).ok()?.flatten() {
        if !proj_entry.file_type().map_or(false, |t| t.is_dir()) {
            continue;
        }
        let Ok(file_entries) = fs::read_dir(proj_entry.path()) else {
            continue;
        };
        for file_entry in file_entries.flatten() {
            let path = file_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            scan_count += 1;
            if let Some(cwd) = extract_cwd_from_jsonl(&path) {
                msg_debug(&format!(
                    "[find_claude_by_cwd] file={}, extracted_cwd={:?}, want={:?}, match={}",
                    path.display(),
                    cwd,
                    canonical_path,
                    cwd == canonical_path
                ));
                if cwd == canonical_path {
                    let mtime = path
                        .metadata()
                        .ok()
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
                msg_debug(&format!(
                    "[find_claude_by_cwd] file={}, extract_cwd returned None",
                    path.display()
                ));
            }
        }
    }
    msg_debug(&format!(
        "[find_claude_by_cwd] scanned {} jsonl files, best={:?}",
        scan_count,
        best_path.as_ref().map(|p| p.display().to_string())
    ));
    if best_path.is_none() {
        if let Some(sample) = cwd_mismatch_sample {
            msg_debug(&format!(
                "[find_claude_by_cwd] cwd mismatch example: extracted={:?} vs wanted={:?}",
                sample, canonical_path
            ));
        }
    }
    let jsonl_path = best_path?;
    let session_id = jsonl_path.file_stem()?.to_str()?.to_string();
    Some(ResolvedSession {
        cwd: canonical_path.to_string(),
        jsonl_path,
        session_id,
        provider: SessionProvider::Claude,
    })
}

/// Codex: scan `~/.codex/sessions/**/*.jsonl` for the latest session matching cwd.
fn find_latest_codex_by_cwd(canonical_path: &str) -> Option<ResolvedSession> {
    let sessions_dir = dirs::home_dir()?.join(".codex").join("sessions");
    if !sessions_dir.is_dir() {
        return None;
    }
    let mut best_path: Option<std::path::PathBuf> = None;
    let mut best_time = std::time::UNIX_EPOCH;
    collect_best_codex_jsonl(
        &sessions_dir,
        canonical_path,
        &mut best_path,
        &mut best_time,
    );
    let jsonl_path = best_path?;
    // Extract UUID from filename tail (last 36 chars of stem)
    let session_id = {
        let stem = jsonl_path.file_stem()?.to_str()?;
        if stem.len() < 36 {
            return None;
        }
        let candidate = &stem[stem.len() - 36..];
        if !is_uuid(candidate) {
            return None;
        }
        candidate.to_string()
    };
    Some(ResolvedSession {
        cwd: canonical_path.to_string(),
        jsonl_path,
        session_id,
        provider: SessionProvider::Codex,
    })
}

fn collect_best_codex_jsonl(
    dir: &Path,
    canonical_path: &str,
    best_path: &mut Option<std::path::PathBuf>,
    best_time: &mut std::time::SystemTime,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            collect_best_codex_jsonl(&entry.path(), canonical_path, best_path, best_time);
        } else if ft.is_file() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(cwd) = extract_cwd_from_jsonl(&path) {
                if cwd == canonical_path {
                    let mtime = path
                        .metadata()
                        .ok()
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

/// Agy: use `last_conversations.json` workspace mapping.
fn find_latest_agy_by_cwd(canonical_path: &str) -> Option<ResolvedSession> {
    msg_debug(&format!(
        "[find_latest_agy_by_cwd] canonical_path={:?}",
        canonical_path
    ));
    let session_id = agy::read_last_conversation_id(canonical_path)?;
    let path = agy::conversation_path(&session_id)?;
    msg_debug(&format!(
        "[find_latest_agy_by_cwd] found: session_id={}, file={}",
        session_id,
        path.display()
    ));
    Some(ResolvedSession {
        cwd: canonical_path.to_string(),
        jsonl_path: path,
        session_id,
        provider: SessionProvider::Agy,
    })
}

/// OpenCode: query SQLite DB for latest session matching cwd.
fn find_latest_opencode_by_cwd(canonical_path: &str) -> Option<ResolvedSession> {
    msg_debug(&format!(
        "[find_latest_opencode_by_cwd] canonical_path={:?}",
        canonical_path
    ));
    let db_path = dirs::home_dir()?
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db");
    if !db_path.is_file() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let mut stmt = conn
        .prepare("SELECT id FROM session WHERE directory = ?1 ORDER BY time_updated DESC LIMIT 1")
        .ok()?;
    let session_id: String = stmt
        .query_row(rusqlite::params![canonical_path], |row| row.get(0))
        .ok()?;
    msg_debug(&format!(
        "[find_latest_opencode_by_cwd] found: session_id={}",
        session_id
    ));
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
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // Skip sidechain (alternative conversation branches)
        if val.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }

        let Some(msg_type) = val.get("type").and_then(|v| v.as_str()) else {
            continue;
        };

        match msg_type {
            "user" => {
                let Some(message) = val.get("message") else {
                    continue;
                };
                let Some(content) = message.get("content") else {
                    continue;
                };
                if let Some(text) = content.as_str() {
                    // Skip commands and system injections
                    if text.is_empty()
                        || text.contains("<command-name>")
                        || text.contains("<local-command")
                    {
                        continue;
                    }
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
                let Some(message) = val.get("message") else {
                    continue;
                };
                let Some(content) = message.get("content") else {
                    continue;
                };
                let Some(arr) = content.as_array() else {
                    continue;
                };
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

    if history.is_empty() {
        return None;
    }

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
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(line_type) = val.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(payload) = val.get("payload") else {
            continue;
        };

        match line_type {
            "event_msg" => {
                let msg_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match msg_type {
                    "user_message" => {
                        let text = payload
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !text.is_empty() {
                            history.push(HistoryItem {
                                item_type: HistoryType::User,
                                content: truncate_utf8(text, 300),
                            });
                        }
                    }
                    "agent_message" => {
                        let text = payload
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
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
                        let name = payload
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Tool");
                        history.push(HistoryItem {
                            item_type: HistoryType::ToolUse,
                            content: format!("[{}]", name),
                        });
                    }
                    "function_call_output" => {
                        // output can be a plain string or structured {content_items: [...]}
                        let output = if let Some(s) = payload.get("output").and_then(|v| v.as_str())
                        {
                            s.to_string()
                        } else if let Some(obj) = payload.get("output") {
                            // Structured: try content_items[].text, then content field
                            if let Some(items) = obj.get("content_items").and_then(|v| v.as_array())
                            {
                                items
                                    .iter()
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

    if history.is_empty() {
        return None;
    }

    Some(SessionData {
        session_id: session_id.to_string(),
        history,
        current_path: cwd.to_string(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        provider: "codex".to_string(),
    })
}

/// Parse an Agy conversation into a minimal cokacdir SessionData record.
///
/// Antigravity stores the real conversation in SQLite/protobuf blobs under
/// `~/.gemini/antigravity-cli/conversations/`. For resume, cokacdir only needs
/// the session id and cwd; `agy --conversation <id>` owns the full transcript.
fn parse_agy_session(path: &Path, session_id: &str, cwd: &str) -> Option<SessionData> {
    msg_debug(&format!(
        "[parse_agy_session] file={}, session_id={}",
        path.display(),
        session_id
    ));
    if !path.is_file() {
        return None;
    }
    Some(SessionData {
        session_id: session_id.to_string(),
        history: vec![HistoryItem {
            item_type: HistoryType::System,
            content: "Agy conversation restored. Full transcript is stored by Antigravity CLI."
                .to_string(),
        }],
        current_path: cwd.to_string(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        provider: "agy".to_string(),
    })
}

/// Parse an OpenCode session from SQLite DB into cokacdir SessionData.
fn parse_opencode_session(db_path: &Path, session_id: &str, cwd: &str) -> Option<SessionData> {
    msg_debug(&format!(
        "[parse_opencode_session] db={}, session_id={}",
        db_path.display(),
        session_id
    ));
    if !db_path.is_file() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT json_extract(m.data, '$.role'), json_extract(p.data, '$.type'), \
         json_extract(p.data, '$.text'), json_extract(p.data, '$.tool') \
         FROM message m JOIN part p ON p.message_id = m.id \
         WHERE m.session_id = ?1 ORDER BY p.time_created ASC",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![session_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .ok()?;
    let mut history: Vec<HistoryItem> = Vec::new();
    for row in rows.flatten() {
        let role = row.0.as_deref().unwrap_or("");
        let ptype = row.1.as_deref().unwrap_or("");
        let text = row.2.as_deref().unwrap_or("");
        let tool = row.3.as_deref().unwrap_or("Tool");
        match ptype {
            "text" => {
                if text.is_empty() {
                    continue;
                }
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
    if history.is_empty() {
        return None;
    }
    msg_debug(&format!(
        "[parse_opencode_session] parsed: history_len={}",
        history.len()
    ));
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
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Extract the first non-empty `cwd` value from a JSONL file.
fn extract_cwd_from_jsonl(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().flatten() {
        if !line.contains("\"cwd\"") {
            continue;
        }
        if let Some(cwd) = extract_json_string_field(&line, "cwd") {
            if !cwd.is_empty() {
                msg_debug(&format!(
                    "[extract_cwd] file={}, cwd={:?}",
                    path.display(),
                    cwd
                ));
                return Some(cwd);
            }
        }
    }
    msg_debug(&format!(
        "[extract_cwd] file={}, no cwd found",
        path.display()
    ));
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
    if end >= bytes.len() {
        return None;
    }
    // Unescape JSON string (e.g. "\\\\" → "\\", "\\\"" → "\"")
    let raw = &rest[..end];
    let quoted = format!("\"{}\"", raw);
    let unescaped = serde_json::from_str::<String>(&quoted).unwrap_or_else(|_| raw.to_string());
    if field == "cwd" {
        msg_debug(&format!(
            "[extract_field] field={}, raw={:?}, unescaped={:?}",
            field, raw, unescaped
        ));
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
    msg_debug(&format!(
        "[workspace_resume] chat_id={}, workspace_id={}",
        chat_id.0, workspace_id
    ));
    let Some(home) = dirs::home_dir() else {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, "Error: cannot determine home directory.")
                .await
        )?;
        return Ok(());
    };

    let workspace_path = home.join(".cokacdir").join("workspace").join(workspace_id);
    if !workspace_path.exists() || !workspace_path.is_dir() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                format!("Error: no workspace found for '{}'.", workspace_id)
            )
            .await
        )?;
        return Ok(());
    }

    let canonical_path = workspace_path
        .canonicalize()
        .map(crate::utils::format::strip_unc_prefix)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| workspace_path.display().to_string());

    let ws_provider = {
        let data = state.lock().await;
        let ws_model = get_model(&data.settings, chat_id);
        detect_provider(ws_model.as_deref())
    };
    msg_debug(&format!(
        "[workspace_resume] canonical_path={:?}, provider={}",
        canonical_path, ws_provider
    ));
    let existing = load_existing_session(&canonical_path, ws_provider);
    msg_debug(&format!(
        "[workspace_resume] load_existing_session → {}",
        if existing.is_some() { "found" } else { "None" }
    ));

    let existing = if existing.is_some() {
        existing
    } else {
        let provider = provider_to_session(ws_provider);
        if let Some(info) = find_latest_session_by_cwd(&canonical_path, provider) {
            msg_debug(&format!(
                "[workspace_resume] fallback found: jsonl={}, session_id={}",
                info.jsonl_path.display(),
                info.session_id
            ));
            convert_and_save_session(&info, &canonical_path);
            let reloaded = load_existing_session(&canonical_path, ws_provider);
            msg_debug(&format!(
                "[workspace_resume] after convert, reload → {}",
                if reloaded.is_some() { "found" } else { "None" }
            ));
            reloaded
        } else {
            msg_debug("[workspace_resume] no external session found either");
            None
        }
    };

    let mut response_lines = Vec::new();

    {
        let mut data = state.lock().await;
        // Session-change cleanup: if this resume switches to a different workspace path
        // OR a different session_id at the same path, drop the in-flight task, queued
        // messages, and loop state authored against the old session — they would be
        // misapplied here, and an in-flight task's writeback would otherwise overwrite
        // the just-loaded session_id/history with the old task's data.
        let old_path = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.current_path.clone());
        let old_sid = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.session_id.clone());
        let new_sid = existing.as_ref().and_then(|(sd, _)| {
            if sd.session_id.is_empty() {
                None
            } else {
                Some(sd.session_id.clone())
            }
        });
        let path_changed = old_path.as_deref() != Some(canonical_path.as_str());
        let sid_changed = old_sid != new_sid;
        if path_changed || sid_changed {
            cancel_in_progress_task_locked(&data, chat_id);
            data.message_queues.remove(&chat_id);
            data.loop_states.remove(&chat_id);
            data.loop_feedback.remove(&chat_id);
        }
        let session = data.sessions.entry(chat_id).or_insert_with(|| ChatSession {
            session_id: None,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            pending_uploads_added_at: None,
        });
        session.session_id = None; // Clear stale session_id from previous workspace
                                   // Drop uploads only when the workspace path changed — upload paths point at
                                   // the old workspace's files. Re-resuming the same workspace (same path) keeps
                                   // uploads since they reference valid files the user just sent.
        if path_changed {
            session.clear_pending_uploads();
        }

        if let Some((session_data, _)) = &existing {
            if session_data_has_active_session(session_data) {
                session.session_id = Some(session_data.session_id.clone());
                session.current_path = Some(canonical_path.clone());
                session.history = session_data.history.clone();

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ▶ Workspace session restored: {workspace_id} → {canonical_path}"
                );
                response_lines.push(format!(
                    "[{}] Session restored at `{}`.",
                    ws_provider, canonical_path
                ));

                let header_len: usize = response_lines.iter().map(|l| l.len() + 1).sum();
                let remaining = TELEGRAM_MSG_LIMIT.saturating_sub(header_len + 2);
                let preview = build_history_preview(&session_data.history, remaining);
                if !preview.is_empty() {
                    response_lines.push(String::new());
                    response_lines.push(preview);
                }
            } else {
                cleanup_session_files(&canonical_path, ws_provider);
                session.session_id = None;
                session.current_path = Some(canonical_path.clone());
                session.history.clear();

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ▶ Workspace session started: {workspace_id} → {canonical_path}");
                response_lines.push(format!(
                    "[{}] Session started at `{}`.",
                    ws_provider, canonical_path
                ));
            }
        } else {
            // Workspace exists but no session — start a new session there
            session.session_id = None;
            session.current_path = Some(canonical_path.clone());
            session.history.clear();

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Workspace session started: {workspace_id} → {canonical_path}");
            response_lines.push(format!(
                "[{}] Session started at `{}`.",
                ws_provider, canonical_path
            ));
        }
    }

    // Persist chat_id → path mapping for auto-restore after restart
    {
        let mut data = state.lock().await;
        data.settings
            .last_sessions
            .insert(chat_id.0.to_string(), canonical_path);
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
    let (current_path, model_for_provider, orphan_stop_msg, bot_username, bot_display_name) = {
        let mut data = state.lock().await;
        let path = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.current_path.clone());
        let old_sid = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.session_id.clone());
        let old_hist_len = data
            .sessions
            .get(&chat_id)
            .map(|s| s.history.len())
            .unwrap_or(0);
        msg_debug(&format!(
            "[handle_clear] clearing: path={:?}, session_id={:?}, history_len={}",
            path, old_sid, old_hist_len
        ));
        // Cancel any in-flight AI task. Without this, its completion handler would write
        // back the response into the just-cleared session, partially resurrecting what the
        // user explicitly cleared. The polling guard's sid comparison (captured_sid vs
        // sid_now=None) is the second layer that prevents writeback even if /clear lands
        // mid-completion. Drop queued messages and loop state for the same reason — they
        // were authored against the pre-clear context.
        cancel_in_progress_task_locked(&data, chat_id);
        let dropped_queue = data
            .message_queues
            .remove(&chat_id)
            .map(|q| q.len())
            .unwrap_or(0);
        if dropped_queue > 0 {
            msg_debug(&format!(
                "[handle_clear] dropped {} queued message(s)",
                dropped_queue
            ));
        }
        // Bump clear_epoch so any in-flight task's polling guard detects the clear
        // even when session_id is None on both spawn and post-completion (brand-new
        // workspace + first message + /clear).
        let entry = data.clear_epoch.entry(chat_id).or_insert(0);
        *entry = entry.wrapping_add(1);
        if let Some(session) = data.sessions.get_mut(&chat_id) {
            session.session_id = None;
            session.history.clear();
            session.clear_pending_uploads();
        }
        let mdl = get_model(&data.settings, chat_id);
        let stop_msg = data.stop_message_ids.remove(&chat_id);
        data.loop_states.remove(&chat_id);
        data.loop_feedback.remove(&chat_id);
        let uname = data.bot_username.clone();
        let dname = data.bot_display_name.clone();
        (path, mdl, stop_msg, uname, dname)
    };
    let provider = detect_provider(model_for_provider.as_deref()).to_string();

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
                            if let Ok(session_data) = serde_json::from_str::<SessionData>(&content)
                            {
                                if session_data.current_path == *path
                                    && (session_data.provider.is_empty()
                                        || session_data.provider == provider)
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
            let dn = if bot_display_name.is_empty() {
                None
            } else {
                Some(bot_display_name.clone())
            };
            append_group_chat_log(
                chat_id.0,
                &GroupChatLogEntry {
                    ts: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    bot: bot_username,
                    bot_display_name: dn,
                    role: "system".to_string(),
                    from: None,
                    text: "Session cleared.".to_string(),
                    clear: true,
                },
            );
        }
    }

    let msg = match current_path {
        Some(ref path) => format!("Session cleared.\n`{}`", path),
        None => "Session cleared.".to_string(),
    };

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, msg).await)?;

    Ok(())
}

/// Handle /pwd command - show current session path
async fn handle_pwd_command(bot: &Bot, chat_id: ChatId, state: &SharedState) -> ResponseResult<()> {
    let current_path = {
        let data = state.lock().await;
        let path = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.current_path.clone());
        let sid = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.session_id.clone());
        msg_debug(&format!(
            "[handle_pwd] chat_id={}, path={:?}, session_id={:?}",
            chat_id.0, path, sid
        ));
        path
    };

    shared_rate_limit_wait(state, chat_id).await;
    match current_path {
        Some(path) => {
            let mut msg = format!("<code>{}</code>", path);
            if let Some(folder_name) = std::path::Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
            {
                if is_workspace_id(folder_name) {
                    msg.push_str(&format!(
                        "\nUse /{} to switch back to this session.",
                        folder_name
                    ));
                }
            }
            tg!(
                "send_message",
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::Html)
                    .await
            )?
        }
        None => tg!(
            "send_message",
            bot.send_message(chat_id, "No active session. Use /start <path> first.")
                .await
        )?,
    };

    Ok(())
}

/// Handle /session command - show current session UUID and resume command
async fn handle_session_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let (session_id, current_path, model_for_provider) = {
        let data = state.lock().await;
        let sid = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.session_id.clone());
        let path = data
            .sessions
            .get(&chat_id)
            .and_then(|s| s.current_path.clone());
        let mdl = get_model(&data.settings, chat_id);
        (sid, path, mdl)
    };
    let session_prov = detect_provider(model_for_provider.as_deref());

    shared_rate_limit_wait(state, chat_id).await;
    match (session_id, current_path) {
        (Some(id), Some(path)) => {
            let resume_cmd = match session_prov {
                "codex" => format!("codex resume {}", id),
                "agy" => format!("agy --conversation {} --print \"\"", id),
                "opencode" => format!("opencode -s {}", id),
                _ => format!("claude --resume {}", id),
            };
            let provider = match session_prov {
                "codex" => "Codex",
                "agy" => "Agy",
                "opencode" => "OpenCode",
                _ => "Claude",
            };
            let msg = format!(
                "Current {} session ID:\n<code>{}</code>\n\nTo resume this session from your terminal:\n<code>cd \"{}\"; {}</code>",
                provider, id, path, resume_cmd
            );
            tg!(
                "send_message",
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::Html)
                    .await
            )?
        }
        _ => tg!(
            "send_message",
            bot.send_message(chat_id, "No active session.").await
        )?,
    };

    Ok(())
}

/// Cancel the in-progress AI task for `chat_id` (idempotent): set the cancel flag and
/// kill the child process so the blocking reader thread exits at EOF. No-op if no token
/// exists or it was already cancelled. Caller must hold the state lock.
fn cancel_in_progress_task_locked(data: &SharedData, chat_id: ChatId) {
    if let Some(token) = data.cancel_tokens.get(&chat_id).cloned() {
        if !token.cancelled.load(Ordering::Relaxed) {
            token.cancel_now();
        }
    }
}

/// Atomically cancel the current AI task for `chat_id` and replace any queued items
/// with a single redirect message (latest-wins semantics). Used when /queue is OFF
/// and a redirect-eligible message arrives while the AI is busy.
///
/// Caller must hold the state lock and pass `&mut SharedData`. The cancelled task's
/// post-cancellation cleanup will trigger `process_next_queued_message`, which then
/// dispatches the redirect target.
///
/// Returns `(queue_id, was_replacement)` where `was_replacement` is true when a
/// pending redirect was overwritten by the new one (rapid successive redirects).
fn enqueue_redirect_locked(
    data: &mut SharedData,
    chat_id: ChatId,
    text: String,
    user_display_name: String,
    pending_uploads: Vec<String>,
) -> (String, bool) {
    // Step 1: cancel the in-progress task (mirror /stop behavior).
    // If a previous /stop or redirect already cancelled it, this is idempotent.
    cancel_in_progress_task_locked(data, chat_id);

    // Step 2: clear loop state so a verification loop doesn't continue
    data.loop_states.remove(&chat_id);
    data.loop_feedback.remove(&chat_id);

    // Step 3: latest-wins push. If a previous redirect is still pending, drop it.
    let queue = data
        .message_queues
        .entry(chat_id)
        .or_insert_with(std::collections::VecDeque::new);
    let was_replacement = !queue.is_empty();
    queue.clear();
    let qid = generate_queue_id();
    queue.push_back(QueuedMessage {
        id: qid.clone(),
        user_display_name,
        payload: QueuedPayload::Text {
            text,
            pending_uploads,
        },
    });

    (qid, was_replacement)
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
    msg_debug(&format!(
        "[handle_stop] chat_id={}, has_token={}",
        chat_id.0,
        token.is_some()
    ));

    match token {
        Some(token) => {
            // Ignore duplicate /stop if already cancelled
            if token.cancelled.load(Ordering::Relaxed) {
                return Ok(());
            }

            // Set cancellation flag and SIGKILL the child IMMEDIATELY to
            // prevent a race: without this, the window between receiving
            // /stop and setting cancelled (during rate_limit_wait +
            // "Stopping..." network call) allows a concurrent claude::execute
            // to pass its cancelled check and start an API request.
            token.cancel_now();

            // Clear loop state so verification loop doesn't continue after stop
            {
                let mut data = state.lock().await;
                data.loop_states.remove(&chat_id);
                data.loop_feedback.remove(&chat_id);
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Cancel signal sent");

            // Send feedback to user (after cancellation to avoid delay)
            shared_rate_limit_wait(state, chat_id).await;
            let stop_msg = tg!(
                "send_message",
                bot.send_message(chat_id, "Stopping...").await
            )?;

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
                    let _ = tg!(
                        "delete_message",
                        bot.delete_message(chat_id, stop_msg.id).await
                    );
                    return Ok(());
                }
            }
        }
        None => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, "No active request to stop.")
                    .await
            )?;
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
                            msg_debug(&format!(
                                "[handle_stop] clearing pending bot message: {}",
                                id
                            ));
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
                msg_debug(&format!(
                    "[queue:stop_item] chat_id={}, removed id={}, text={:?}, remaining={}",
                    chat_id.0,
                    queue_id,
                    removed_msg
                        .as_ref()
                        .map(|m| truncate_str(&m.preview_text(), 60)),
                    q.len()
                ));
                // Clean up empty queue
                if q.is_empty() {
                    data.message_queues.remove(&chat_id);
                }
                removed_msg.map(|m| m.id)
            } else {
                msg_debug(&format!(
                    "[queue:stop_item] chat_id={}, id={} not found in queue",
                    chat_id.0, queue_id
                ));
                None
            }
        } else {
            msg_debug(&format!(
                "[queue:stop_item] chat_id={}, no queue exists",
                chat_id.0
            ));
            None
        }
    };

    if let Some(id) = removed {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, &format!("Removed queued message ({id})."))
                .await
        )?;
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
    msg_debug(&format!(
        "[queue:stopall] chat_id={}, has_token={}, queue_cleared={}, already_cancelled={}",
        chat_id.0,
        token.is_some(),
        queue_len,
        already_cancelled
    ));

    match token {
        Some(token) => {
            // Ignore duplicate stop if already cancelled
            if already_cancelled {
                msg_debug(&format!(
                    "[queue:stopall] chat_id={}, duplicate cancel ignored, queue_cleared={}",
                    chat_id.0, queue_len
                ));
                // Still report queue clearance if there were queued messages
                if queue_len > 0 {
                    shared_rate_limit_wait(state, chat_id).await;
                    tg!(
                        "send_message",
                        bot.send_message(
                            chat_id,
                            &format!("{} queued message(s) cleared.", queue_len)
                        )
                        .await
                    )?;
                }
                return Ok(());
            }

            // Cancellation flag already set above inside the lock; this
            // SIGKILLs the child's process group (cancel_now sets the flag
            // again — idempotent).
            token.cancel_now();

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ■ Cancel signal sent (stopall, {} queued cleared)",
                queue_len
            );

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
                    let _ = tg!(
                        "delete_message",
                        bot.delete_message(chat_id, stop_msg.id).await
                    );
                    return Ok(());
                }
            }
        }
        None => {
            let msg_text = if queue_len > 0 {
                format!(
                    "No active request. {} queued message(s) cleared.",
                    queue_len
                )
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
                            msg_debug(&format!(
                                "[handle_stopall] clearing pending bot message: {}",
                                id
                            ));
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
    let raw_file_path = text.strip_prefix("/down").unwrap_or("").trim();
    msg_debug(&format!(
        "[handle_down] chat_id={}, file_path={:?}",
        chat_id.0, raw_file_path
    ));

    if raw_file_path.is_empty() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                "Usage: /down <filepath>\nExample: /down /home/kst/file.txt"
            )
            .await
        )?;
        return Ok(());
    }

    let expanded = crate::utils::path::expand_tilde(raw_file_path);
    let file_path = expanded.as_str();

    // Resolve relative path using current session path
    let resolved_path = if Path::new(file_path).is_absolute() {
        file_path.to_string()
    } else {
        let current_path = {
            let data = state.lock().await;
            data.sessions
                .get(&chat_id)
                .and_then(|s| s.current_path.clone())
        };
        match current_path {
            Some(base) => Path::new(base.trim_end_matches(['/', '\\']))
                .join(file_path)
                .display()
                .to_string(),
            None => {
                shared_rate_limit_wait(state, chat_id).await;
                tg!(
                    "send_message",
                    bot.send_message(
                        chat_id,
                        "No active session. Use absolute path or /start <path> first."
                    )
                    .await
                )?;
                return Ok(());
            }
        }
    };

    let path = Path::new(&resolved_path);
    if !path.exists() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, &format!("File not found: {}", resolved_path))
                .await
        )?;
        return Ok(());
    }
    if !path.is_file() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, &format!("Not a file: {}", resolved_path))
                .await
        )?;
        return Ok(());
    }

    shared_rate_limit_wait(state, chat_id).await;
    tg!(
        "send_document",
        bot.send_document(chat_id, teloxide::types::InputFile::file(path))
            .await
    )?;

    Ok(())
}

/// Handle file/photo upload - save to current session path
async fn handle_file_upload(
    bot: &Bot,
    chat_id: ChatId,
    msg: &Message,
    state: &SharedState,
    user_display_name: &str,
) -> ResponseResult<Option<String>> {
    let upload_type = if msg.animation().is_some() {
        "animation"
    } else if msg.document().is_some() {
        "document"
    } else if msg.photo().is_some() {
        "photo"
    } else if msg.video().is_some() {
        "video"
    } else if msg.voice().is_some() {
        "voice"
    } else if msg.audio().is_some() {
        "audio"
    } else if msg.video_note().is_some() {
        "video_note"
    } else {
        "unknown"
    };
    msg_debug(&format!(
        "[handle_upload] chat_id={}, type={}",
        chat_id.0, upload_type
    ));
    // Get current session path
    let current_path = {
        let data = state.lock().await;
        data.sessions
            .get(&chat_id)
            .and_then(|s| s.current_path.clone())
    };

    let save_dir = if let Some(path) = current_path {
        path
    } else {
        // Auto-create a workspace session
        let Some((_, path)) = auto_create_workspace_session(bot, state, chat_id, bot.token()).await
        else {
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, "Failed to create workspace.")
                    .await
            )?;
            return Ok(None);
        };
        path
    };

    // Get file_id, file_name, and file_size
    // animation must precede document: Telegram sets both fields for GIFs
    let (file_id, file_name, file_size_hint) = if let Some(anim) = msg.animation() {
        let name = anim
            .file_name
            .clone()
            .unwrap_or_else(|| format!("animation_{}.mp4", anim.file.unique_id));
        (anim.file.id.clone(), name, anim.file.size)
    } else if let Some(doc) = msg.document() {
        let name = doc
            .file_name
            .clone()
            .unwrap_or_else(|| "uploaded_file".to_string());
        (doc.file.id.clone(), name, doc.file.size)
    } else if let Some(photos) = msg.photo() {
        // Get the largest photo
        if let Some(photo) = photos.last() {
            let name = format!("photo_{}.jpg", photo.file.unique_id);
            (photo.file.id.clone(), name, photo.file.size)
        } else {
            return Ok(None);
        }
    } else if let Some(video) = msg.video() {
        let name = video
            .file_name
            .clone()
            .unwrap_or_else(|| format!("video_{}.mp4", video.file.unique_id));
        (video.file.id.clone(), name, video.file.size)
    } else if let Some(voice) = msg.voice() {
        let name = format!("voice_{}.ogg", voice.file.unique_id);
        (voice.file.id.clone(), name, voice.file.size)
    } else if let Some(audio) = msg.audio() {
        let name = audio
            .file_name
            .clone()
            .unwrap_or_else(|| format!("audio_{}.mp3", audio.file.unique_id));
        (audio.file.id.clone(), name, audio.file.size)
    } else if let Some(vn) = msg.video_note() {
        let name = format!("videonote_{}.mp4", vn.file.unique_id);
        (vn.file.id.clone(), name, vn.file.size)
    } else {
        return Ok(None);
    };
    msg_debug(&format!(
        "[handle_upload] chat_id={}, file_name={}, file_size={}",
        chat_id.0, file_name, file_size_hint
    ));

    // Check file size (Telegram Bot API limit: 20MB for getFile)
    const MAX_DOWNLOAD_SIZE: u32 = 20 * 1024 * 1024;
    if file_size_hint > MAX_DOWNLOAD_SIZE {
        let size_mb = file_size_hint as f64 / (1024.0 * 1024.0);
        msg_debug(&format!(
            "[handle_upload] chat_id={}, file rejected: size {:.1}MB exceeds 20MB limit",
            chat_id.0, size_mb
        ));
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                &format!("File too large ({:.1}MB). Maximum size is 20MB.", size_mb)
            )
            .await
        )?;
        return Ok(None);
    }

    // Download file from Telegram via HTTP
    shared_rate_limit_wait(state, chat_id).await;
    let file = tg!("get_file", bot.get_file(&file_id).await)?;
    let base = {
        let data = state.lock().await;
        data.api_base_url.clone()
    };
    let token = bot.token().to_string();
    let url = format!("{}/file/bot{}/{}", base, token, file.path);
    msg_debug(&format!(
        "[handle_upload] download url: api_base={}, file_path={}",
        base, file.path
    ));
    // Strip the bot token from any error string before showing it to the chat.
    // reqwest's Display impl can include the request URL on some error kinds
    // (redirect, parse, …), and the URL embeds `bot<TOKEN>` in plaintext.
    let scrub = |e: reqwest::Error| -> String {
        let s = e.to_string();
        s.replace(&token, "<bot_token_redacted>")
    };
    let buf = match reqwest::get(&url).await {
        Ok(resp) => match resp.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                shared_rate_limit_wait(state, chat_id).await;
                tg!(
                    "send_message",
                    bot.send_message(chat_id, &format!("Download failed: {}", scrub(e)))
                        .await
                )?;
                return Ok(None);
            }
        },
        Err(e) => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, &format!("Download failed: {}", scrub(e)))
                    .await
            )?;
            return Ok(None);
        }
    };

    // Save to session path (sanitize file_name to prevent path traversal)
    let safe_name = Path::new(&file_name)
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("uploaded_file"));
    let mut dest = Path::new(&save_dir).join(safe_name);
    // Avoid overwriting existing files (atomic create_new to eliminate TOCTOU race)
    let file_size = buf.len();
    let stem = dest
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("uploaded_file")
        .to_string();
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e))
        .unwrap_or_default();
    let mut counter = 0u32;
    let write_result = loop {
        use std::io::Write;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
        {
            Ok(mut f) => break f.write_all(&buf),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                counter += 1;
                dest = Path::new(&save_dir).join(format!("{}({}){}", stem, counter, ext));
                msg_debug(&format!(
                    "[handle_upload] chat_id={}, file exists, renamed to: {}",
                    chat_id.0,
                    dest.display()
                ));
            }
            Err(e) => break Err(e),
        }
    };
    match write_result {
        Ok(_) => {
            msg_debug(&format!(
                "[handle_upload] chat_id={}, saved: {} ({} bytes)",
                chat_id.0,
                dest.display(),
                file_size
            ));
            let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, &msg_text).await)?;
        }
        Err(e) => {
            msg_debug(&format!(
                "[handle_upload] chat_id={}, save failed: {}",
                chat_id.0, e
            ));
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, &format!("Failed to save file: {}", e))
                    .await
            )?;
            return Ok(None);
        }
    }

    // Record upload in session history and pending queue for Claude
    let upload_record = format!(
        "[File uploaded] {} → {} ({} bytes)",
        file_name,
        dest.display(),
        file_size
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
            session.push_pending_upload(upload_record.clone());
            save_session_to_file(session, &save_dir, provider);
        }
        // Write file upload to group chat shared log
        if chat_id.0 < 0 {
            let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
            let dn = if data.bot_display_name.is_empty() {
                None
            } else {
                Some(data.bot_display_name.clone())
            };
            append_group_chat_log(
                chat_id.0,
                &GroupChatLogEntry {
                    ts: now_ts,
                    bot: data.bot_username.clone(),
                    bot_display_name: dn,
                    role: "user".to_string(),
                    from: Some(user_display_name.to_string()),
                    text: upload_record.clone(),
                    clear: false,
                },
            );
        }
    }

    Ok(Some(upload_record))
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
    msg_debug(&format!(
        "[handle_shell] chat_id={}, cmd={:?}",
        chat_id.0,
        truncate_str(cmd_str, 100)
    ));

    if cmd_str.is_empty() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                "Usage: !<command>\nExample: !mkdir /home/kst/testcode"
            )
            .await
        )?;
        return Ok(());
    }

    // Register cancel token early (prevents duplicate requests while waiting for group lock)
    let cancel_token = new_cancel_token();
    {
        let mut data = state.lock().await;
        if data.cancel_tokens.contains_key(&chat_id) {
            msg_debug(&format!(
                "[handle_shell] chat_id={}, cancel_token exists (busy), rejecting",
                chat_id.0
            ));
            drop(data);
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, "AI request in progress. Use /stop to cancel.")
                    .await
            )?;
            return Ok(());
        }
        insert_cancel_token_locked(&mut data, chat_id, cancel_token.clone());
    }

    // Group chat lock acquisition is deferred into the spawned polling task
    // below so the per-chat FIFO worker can pull the next message (e.g.
    // `/stop`) immediately instead of waiting for cross-bot lock contention.
    // The cancel_token reservation above already guards this bot's per-chat
    // slot. Placeholder send is also deferred so a `/stop` arriving during
    // lock wait does not leave an orphan "Processing ..." message.

    // Get current_path for working directory (default to home directory)
    let working_dir = {
        let data = state.lock().await;
        data.sessions
            .get(&chat_id)
            .and_then(|s| s.current_path.clone())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or_else(|| {
                        if cfg!(windows) {
                            "C:\\".to_string()
                        } else {
                            "/".to_string()
                        }
                    })
            })
    };

    let cmd_display = cmd_str.to_string();

    // Create channel
    let (tx, rx) = mpsc::channel();

    let cmd_owned = cmd_str.to_string();
    let working_dir_clone = working_dir.clone();
    let cancel_token_clone = cancel_token.clone();

    // Shell child spawn is deferred into the polling task below — it runs
    // only after the cross-bot group_lock is acquired. Variables prepared
    // here (cmd_owned, working_dir_clone, cancel_token_clone, tx) are
    // captured by the polling task's `async move` closure and consumed by
    // the inner `spawn_tracked_blocking_task` call.

    // Spawn polling loop (same pattern as AI streaming)
    let bot_owned = bot.clone();
    let state_owned = state.clone();
    let cmd_display_owned = cmd_display.clone();
    let (shell_bot_username, shell_bot_display_name) = {
        let data = state.lock().await;
        (data.bot_username.clone(), data.bot_display_name.clone())
    };
    let shell_user_display_name = user_display_name.to_string();
    let request_tasks = {
        let data = state.lock().await;
        data.request_tasks.clone()
    };
    // Cloned for the inner shell-exec spawn (the outer call below consumes
    // the original).
    let request_tasks_for_shell = request_tasks.clone();
    let _ = spawn_tracked_request_task(request_tasks, async move {
        // Acquire the cross-bot group chat lock here (was previously inline,
        // which head-of-line-blocked the per-chat FIFO worker — see
        // `run_chat_worker`). Placeholder send and shell-child spawn are
        // also deferred until after the lock so (a) `/stop` during lock
        // wait does not leave an orphan "Processing ..." message and
        // (b) two bots in the same group chat do not run shell children
        // concurrently.
        let group_lock = acquire_group_chat_lock(chat_id.0).await;

        // Shell child has not been spawned yet (we spawn it below, after the
        // lock is acquired and the placeholder is sent), so cleanup is just
        // state + queue processing; no child process to SIGKILL.
        if cancel_token.cancelled.load(Ordering::Relaxed) {
            msg_debug(&format!(
                "[queue:trigger] chat_id={}, source=query_cancelled_during_lock",
                chat_id.0
            ));
            {
                let mut data = state_owned.lock().await;
                remove_cancel_token_locked(&mut data, chat_id);
            }
            drop(group_lock);
            process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
            return;
        }
        let _group_lock = group_lock; // hold group chat lock until task ends

        // Send placeholder message (was previously inline before this spawn).
        // Shell child is spawned only after the placeholder succeeds, so a
        // network error here means no child to clean up.
        shared_rate_limit_wait(&state_owned, chat_id).await;
        let placeholder = match tg!(
            "send_message",
            bot_owned
                .send_message(
                    chat_id,
                    format!(
                        "Processing <code>{}</code>",
                        html_escape(&cmd_display_owned)
                    )
                )
                .parse_mode(ParseMode::Html)
                .await
        ) {
            Ok(m) => m,
            Err(e) => {
                msg_debug(&format!(
                    "[queue:trigger] chat_id={}, source=query_placeholder_error: {}",
                    chat_id.0, e
                ));
                {
                    let mut data = state_owned.lock().await;
                    remove_cancel_token_locked(&mut data, chat_id);
                }
                process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
                return;
            }
        };
        let placeholder_msg_id = placeholder.id;

        // Spawn the shell child now that the lock is held and the placeholder
        // is up. This was previously inline in `handle_shell_command`, but
        // running it before the lock meant two bots in the same group chat
        // could both spawn shell processes simultaneously and `/stop` during
        // lock wait could not prevent execution.
        let _ = spawn_tracked_blocking_task(request_tasks_for_shell, move || {
            #[cfg(unix)]
            let mut cmd = {
                let mut c = std::process::Command::new("bash");
                c.args(["-c", &cmd_owned])
                    .current_dir(&working_dir_clone)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                c
            };
            #[cfg(windows)]
            let ps_command = format!(
                "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; {}; exit $LASTEXITCODE",
                cmd_owned
            );
            #[cfg(windows)]
            let mut cmd = {
                let mut c = std::process::Command::new("powershell");
                c.args(["-NoProfile", "-NonInteractive", "-Command", &ps_command])
                    .current_dir(&working_dir_clone)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                c
            };
            // Put the shell into its own process group so /stop's
            // kill(-pid, SIGKILL) targets the bash/powershell tree —
            // and not the bot's own pgroup. No-op on Windows (taskkill /T).
            claude::detach_into_own_pgroup(&mut cmd);
            let child = cmd.spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(ShellOutput::Error(format!("Failed to execute: {}", e)));
                    return;
                }
            };

            // Store PID for /stop kill. Recover from a poisoned mutex (a prior
            // holder panicked) instead of silently dropping the PID — without
            // it stored, /stop cannot signal this child.
            {
                let mut guard = cancel_token_clone
                    .child_pid
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *guard = Some(child.id());
            }
            // /stop landing between `cancel_token_clone.cancelled = true` and
            // PID storage above would leave the child alive. Re-check after
            // storing so the cancel observer in this thread kills the child
            // even if /stop fired in that narrow window.
            if cancel_token_clone
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                claude::kill_child_tree(&mut child);
                let _ = child.wait();
                return;
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
                    if cancel_token_clone
                        .cancelled
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        claude::kill_child_tree(&mut child);
                        let _ = child.wait();
                        return;
                    }
                    let _ = tx.send(ShellOutput::Line(line));
                }
            }

            let stderr_output = stderr_thread.join().unwrap_or_default();
            if !stderr_output.is_empty() {
                let _ = tx.send(ShellOutput::Line(format!(
                    "[stderr]\n{}",
                    stderr_output.trim_end()
                )));
            }

            let status = child.wait();
            let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            let _ = tx.send(ShellOutput::Done { exit_code });
        });

        const SPINNER: &[&str] = &[
            "🕐 P",
            "🕑 Pr",
            "🕒 Pro",
            "🕓 Proc",
            "🕔 Proce",
            "🕕 Proces",
            "🕖 Process",
            "🕗 Processi",
            "🕘 Processin",
            "🕙 Processing",
            "🕚 Processing.",
            "🕛 Processing..",
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
                if !done {
                    cancelled = true;
                }
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(polling_time_ms)).await;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done {
                    cancelled = true;
                }
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

                    let display_text = format!(
                        "Processing <code>{}</code>\n\n{}",
                        html_escape(&cmd_display_owned),
                        indicator
                    );

                    if display_text != last_edit_text {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!(
                            "edit_message",
                            &state_owned,
                            chat_id,
                            bot_owned
                                .edit_message_text(chat_id, placeholder_msg_id, &display_text)
                                .parse_mode(ParseMode::Html)
                                .await
                        );
                        last_edit_text = display_text;
                    } else {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!(
                            "send_chat_action",
                            bot_owned
                                .send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
                                .await
                        );
                    }
                }
            }

            // Render final result once
            if done && !response_rendered {
                response_rendered = true;

                if let Some(err) = &spawn_error {
                    // Spawn error - just show error message
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!(
                        "edit_message",
                        bot_owned
                            .edit_message_text(chat_id, placeholder_msg_id, err)
                            .await
                    );
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
                            let combined = format!(
                                "Done <code>{}</code>{}\n\n<pre>$ {}\n\n{}</pre>",
                                html_escape(&cmd_display_owned),
                                exit_suffix,
                                html_escape(&cmd_display_owned),
                                html_escape(full_output.trim())
                            );
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            if let Err(_) = tg!(
                                "edit_message",
                                bot_owned
                                    .edit_message_text(chat_id, placeholder_msg_id, &combined)
                                    .parse_mode(ParseMode::Html)
                                    .await
                            ) {
                                let fallback = format!(
                                    "Done {}{}\n\n$ {}\n\n{}",
                                    cmd_display_owned,
                                    exit_suffix,
                                    cmd_display_owned,
                                    full_output.trim()
                                );
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!(
                                    "edit_message",
                                    bot_owned
                                        .edit_message_text(chat_id, placeholder_msg_id, &fallback)
                                        .await
                                );
                            }
                        } else {
                            // Long output: update placeholder + send as .txt file
                            let final_msg = format!(
                                "Done <code>{}</code>{}",
                                html_escape(&cmd_display_owned),
                                exit_suffix
                            );
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!(
                                "edit_message",
                                bot_owned
                                    .edit_message_text(chat_id, placeholder_msg_id, &final_msg)
                                    .parse_mode(ParseMode::Html)
                                    .await
                            );

                            let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
                            if let Some(home) = dirs::home_dir() {
                                let tmp_dir = home.join(".cokacdir").join("tmp");
                                let _ = std::fs::create_dir_all(&tmp_dir);
                                let tmp_path = tmp_dir
                                    .join(format!("cokacdir_shell_{}_{}.txt", chat_id.0, timestamp))
                                    .display()
                                    .to_string();
                                if std::fs::write(&tmp_path, &file_content).is_ok() {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!(
                                        "send_document",
                                        bot_owned
                                            .send_document(
                                                chat_id,
                                                teloxide::types::InputFile::file(
                                                    std::path::Path::new(&tmp_path)
                                                ),
                                            )
                                            .await
                                    );
                                    let _ = std::fs::remove_file(&tmp_path);
                                }
                            }
                        }
                    } else {
                        // No output
                        let final_msg = format!(
                            "Done <code>{}</code>{}",
                            html_escape(&cmd_display_owned),
                            exit_suffix
                        );
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!(
                            "edit_message",
                            bot_owned
                                .edit_message_text(chat_id, placeholder_msg_id, &final_msg)
                                .parse_mode(ParseMode::Html)
                                .await
                        );
                    }
                }

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ▶ Shell command completed: !{}", cmd_display_owned);

                // Write shell command to group chat shared log
                if chat_id.0 < 0 {
                    let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                    let dn = if shell_bot_display_name.is_empty() {
                        None
                    } else {
                        Some(shell_bot_display_name.clone())
                    };
                    append_group_chat_log(
                        chat_id.0,
                        &GroupChatLogEntry {
                            ts: now_ts.clone(),
                            bot: shell_bot_username.clone(),
                            bot_display_name: dn.clone(),
                            role: "user".to_string(),
                            from: Some(shell_user_display_name.clone()),
                            text: format!("!{}", cmd_display_owned),
                            clear: false,
                        },
                    );
                    let output_summary = if full_output.trim().is_empty() {
                        format!("(exit code: {})", exit_code)
                    } else {
                        format!("exit code: {}\n{}", exit_code, full_output.trim())
                    };
                    append_group_chat_log(
                        chat_id.0,
                        &GroupChatLogEntry {
                            ts: now_ts,
                            bot: shell_bot_username.clone(),
                            bot_display_name: dn,
                            role: "assistant".to_string(),
                            from: None,
                            text: output_summary,
                            clear: false,
                        },
                    );
                }

                // Send end hook message if configured
                if !cancelled {
                    let end_hook_msg = {
                        let data = state_owned.lock().await;
                        data.settings.end_hook.get(&chat_id.0.to_string()).cloned()
                    };
                    if let Some(hook_msg) = end_hook_msg {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!(
                            "send_message",
                            bot_owned.send_message(chat_id, &hook_msg).await
                        );
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
            // Re-send SIGKILL as a safety belt (cancelled flag already true,
            // cancel_now is idempotent).
            cancel_token.cancel_now();

            shared_rate_limit_wait(&state_owned, chat_id).await;
            let _ = tg!(
                "edit_message",
                bot_owned
                    .edit_message_text(chat_id, placeholder_msg_id, "[Stopped]")
                    .await
            );

            let stop_msg_id = {
                let data = state_owned.lock().await;
                data.stop_message_ids.get(&chat_id).cloned()
            };
            if let Some(msg_id) = stop_msg_id {
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!(
                    "delete_message",
                    bot_owned.delete_message(chat_id, msg_id).await
                );
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Shell command stopped: !{}", cmd_display_owned);

            // Write stopped shell command to group chat shared log
            if chat_id.0 < 0 {
                let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                let dn = if shell_bot_display_name.is_empty() {
                    None
                } else {
                    Some(shell_bot_display_name.clone())
                };
                append_group_chat_log(
                    chat_id.0,
                    &GroupChatLogEntry {
                        ts: now_ts.clone(),
                        bot: shell_bot_username.clone(),
                        bot_display_name: dn.clone(),
                        role: "user".to_string(),
                        from: Some(shell_user_display_name.clone()),
                        text: format!("!{} [Stopped]", cmd_display_owned),
                        clear: false,
                    },
                );
                if !full_output.trim().is_empty() {
                    append_group_chat_log(
                        chat_id.0,
                        &GroupChatLogEntry {
                            ts: now_ts,
                            bot: shell_bot_username.clone(),
                            bot_display_name: dn,
                            role: "assistant".to_string(),
                            from: None,
                            text: format!("[Stopped] exit code: -1\n{}", full_output.trim()),
                            clear: false,
                        },
                    );
                }
            }

            let mut data = state_owned.lock().await;
            remove_cancel_token_locked(&mut data, chat_id);
            data.stop_message_ids.remove(&chat_id);
            drop(data);
            msg_debug(&format!(
                "[queue:trigger] chat_id={}, source=query_poll_cancelled",
                chat_id.0
            ));
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
                let _ = tg!(
                    "delete_message",
                    bot_owned.delete_message(chat_id, msg_id).await
                );
            }
        }

        // Release lock
        {
            let mut data = state_owned.lock().await;
            remove_cancel_token_locked(&mut data, chat_id);
        }
        msg_debug(&format!(
            "[queue:trigger] chat_id={}, source=query_poll_completed",
            chat_id.0
        ));
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
            msg.push_str(&format!(
                "<code>{}</code> — {}\n",
                html_escape(name),
                html_escape(desc)
            ));
        } else {
            msg.push_str(&format!(
                "<code>{}</code> {} — {}\n",
                html_escape(name),
                badge,
                html_escape(desc)
            ));
        }
    }
    msg.push_str(&format!(
        "\n{} = destructive\nTotal: {}",
        risk_badge(true),
        ALL_TOOLS.len()
    ));

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
            msg.push_str(&format!(
                "<code>{}</code> — {}\n",
                html_escape(tool),
                html_escape(desc)
            ));
        } else {
            msg.push_str(&format!(
                "<code>{}</code> {} — {}\n",
                html_escape(tool),
                badge,
                html_escape(desc)
            ));
        }
    }
    msg.push_str(&format!(
        "\n{} = destructive\nTotal: {}",
        risk_badge(true),
        tools.len()
    ));

    shared_rate_limit_wait(state, chat_id).await;
    tg!(
        "send_message",
        bot.send_message(chat_id, &msg)
            .parse_mode(ParseMode::Html)
            .await
    )?;

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
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                format!(
                    "Current polling time: {}ms\nUsage: /setpollingtime <ms>\nMinimum: 2500ms",
                    current
                )
            )
            .await
        )?;
        return Ok(());
    }

    let value: u64 = match arg.parse() {
        Ok(v) => v,
        Err(_) => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(
                    chat_id,
                    "Invalid number. Usage: /setpollingtime <ms>\nExample: /setpollingtime 3000"
                )
                .await
            )?;
            return Ok(());
        }
    };

    if value < 2500 {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, "Minimum polling time is 2500ms.")
                .await
        )?;
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
    tg!(
        "send_message",
        bot.send_message(chat_id, format!("✅ Polling time set to {}ms", value))
            .await
    )?;

    Ok(())
}

/// Handle the retired /greeting command.
async fn handle_greeting_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    shared_rate_limit_wait(state, chat_id).await;
    tg!(
        "send_message",
        bot.send_message(chat_id, "Startup greeting is disabled.")
            .await
    )?;
    Ok(())
}

/// Handle /debug command - toggle all debug logging (Telegram API, Claude, cron)
async fn handle_debug_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let prev = {
        let data = state.lock().await;
        data.settings.debug
    };
    let next = !prev;
    msg_debug(&format!(
        "[handle_debug] chat_id={}, {} → {}",
        chat_id.0, prev, next
    ));
    {
        let mut data = state.lock().await;
        data.settings.debug = next;
        save_bot_settings(token, &data.settings);
    }
    let global_enabled = refresh_global_debug_flags();
    let status = if next { "ON" } else { "OFF" };
    let note = if !next && global_enabled {
        "\nShared debug logging is still ON because another bot or COKACDIR_DEBUG=1 enables it."
    } else {
        ""
    };
    shared_rate_limit_wait(state, chat_id).await;
    tg!(
        "send_message",
        bot.send_message(chat_id, format!("🔍 Debug logging: {status}{note}"))
            .await
    )?;
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
    msg_debug(&format!(
        "[handle_direct] chat_id={}, is_owner={}",
        chat_id.0, is_owner
    ));
    let is_actually_group = matches!(msg.chat.kind, teloxide::types::ChatKind::Public(_));
    msg_debug(&format!(
        "[handle_direct] is_actually_group={}",
        is_actually_group
    ));
    if !is_actually_group {
        msg_debug("[handle_direct] rejected: not a group chat");
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, "This command is only available in group chats.")
                .await
        )?;
        return Ok(());
    }
    if !is_owner {
        msg_debug("[handle_direct] rejected: not owner");
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, "Only the bot owner can change direct mode.")
                .await
        )?;
        return Ok(());
    }
    let next = {
        let mut data = state.lock().await;
        let key = chat_id.0.to_string();
        let prev = data
            .settings
            .direct
            .get(&key)
            .copied()
            .unwrap_or(DIRECT_MODE_DEFAULT);
        let next = !prev;
        msg_debug(&format!(
            "[handle_direct] chat_id={}, {} → {}",
            chat_id.0, prev, next
        ));
        data.settings.direct.insert(key, next);
        save_bot_settings(token, &data.settings);
        msg_debug(&format!(
            "[handle_direct] saved to bot_settings, next={}",
            next
        ));
        next
    };
    let status = if next {
        "Direct mode: ON (no ; prefix needed)"
    } else {
        "Direct mode: OFF (; prefix required)"
    };
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
        tg!(
            "send_message",
            bot.send_message(chat_id, "This command is only available in group chats.")
                .await
        )?;
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
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                format!(
                    "Group chat log context: <b>{}</b> entries\n\n\
             <code>/contextlevel &lt;N&gt;</code> — Set count (0 to disable)\n\
             Default: 12",
                    current
                )
            )
            .parse_mode(teloxide::types::ParseMode::Html)
            .await
        )?;
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
    tg!(
        "send_message",
        bot.send_message(chat_id, msg)
            .parse_mode(teloxide::types::ParseMode::Html)
            .await
    )?;
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
    msg_debug(&format!(
        "[handle_instruction] chat_id={}, body_len={}, body_empty={}",
        chat_id.0,
        body.len(),
        body.is_empty()
    ));
    if body.is_empty() {
        // Show current instruction
        let data = state.lock().await;
        let current = data.settings.instructions.get(&key);
        msg_debug(&format!(
            "[handle_instruction] view mode: has_instruction={}",
            current.is_some()
        ));
        let msg = match current {
            Some(instr) => {
                msg_debug(&format!(
                    "[handle_instruction] current instruction len={}",
                    instr.len()
                ));
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
        msg_debug(&format!(
            "[handle_instruction] set mode: chat_id={}, instruction_len={}, text={:?}",
            chat_id.0,
            instr.len(),
            truncate_str(&instr, 100)
        ));
        {
            let mut data = state.lock().await;
            data.settings.instructions.insert(key, instr.clone());
            save_bot_settings(token, &data.settings);
            msg_debug("[handle_instruction] saved to bot_settings");
        }
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, format!("Instruction set:\n{}", instr))
                .await
        )?;
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
        msg_debug(&format!(
            "[handle_instruction_clear] had_instruction={}",
            had_instruction
        ));
        data.settings.instructions.remove(&key);
        save_bot_settings(token, &data.settings);
        msg_debug("[handle_instruction_clear] saved to bot_settings");
    }
    shared_rate_limit_wait(state, chat_id).await;
    tg!(
        "send_message",
        bot.send_message(chat_id, "Instruction cleared.").await
    )?;
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
        tg!(
            "send_message",
            bot.send_message(chat_id, format!("End hook set:\n{}", hook))
                .await
        )?;
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
    tg!(
        "send_message",
        bot.send_message(chat_id, "End hook cleared.").await
    )?;
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
        let prev = data
            .settings
            .silent
            .get(&key)
            .copied()
            .unwrap_or(SILENT_MODE_DEFAULT);
        let next = !prev;
        data.settings.silent.insert(key, next);
        save_bot_settings(token, &data.settings);
        next
    };
    let status = if next {
        "🔇 Silent mode: ON"
    } else {
        "🔊 Silent mode: OFF"
    };
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
    let status = if next {
        "🌐 Chrome mode: ON (--chrome)"
    } else {
        "🌐 Chrome mode: OFF"
    };
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
        let prev = data
            .settings
            .queue
            .get(&key)
            .copied()
            .unwrap_or(QUEUE_MODE_DEFAULT);
        let next = !prev;
        data.settings.queue.insert(key, next);
        save_bot_settings(token, &data.settings);
        // If turning OFF, clear any pending queued messages
        let queue_len = if !next {
            let q = data.message_queues.remove(&chat_id);
            let cleared = q.as_ref().map(|q| q.len()).unwrap_or(0);
            msg_debug(&format!(
                "[queue:toggle] chat_id={}, OFF, cleared {} queued messages",
                chat_id.0, cleared
            ));
            cleared
        } else {
            msg_debug(&format!("[queue:toggle] chat_id={}, ON", chat_id.0));
            0
        };
        (next, queue_len)
    };
    let status = if next {
        "📋 Queue mode: ON\nMessages sent while AI is busy will be queued and processed in order."
            .to_string()
    } else if queue_len > 0 {
        format!(
            "📋 Queue mode: OFF\n{} queued message(s) cleared.",
            queue_len
        )
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
    msg_debug(&format!(
        "[handle_allowed] chat_id={}, arg={:?}",
        chat_id.0, arg
    ));

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
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                "Use +toolname to add or -toolname to remove.\nExample: /allowed +Bash -Edit"
            )
            .await
        )?;
        return Ok(());
    }

    let response_msg = {
        let mut data = state.lock().await;
        let chat_key = chat_id.0.to_string();
        // Ensure this chat has its own tool list (initialize from defaults if missing)
        if !data.settings.allowed_tools.contains_key(&chat_key) {
            let defaults: Vec<String> = DEFAULT_ALLOWED_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect();
            data.settings
                .allowed_tools
                .insert(chat_key.clone(), defaults);
        }
        let tools = data.settings.allowed_tools.get_mut(&chat_key).unwrap();
        let mut results: Vec<String> = Vec::new();
        let mut changed = false;
        for (op, tool_name) in &operations {
            match op {
                '+' => {
                    if tools.iter().any(|t| t == tool_name) {
                        results.push(format!(
                            "<code>{}</code> already in list",
                            html_escape(tool_name)
                        ));
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
                        results.push(format!(
                            "<code>{}</code> not in list",
                            html_escape(tool_name)
                        ));
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
    tg!(
        "send_message",
        bot.send_message(chat_id, &response_msg)
            .parse_mode(ParseMode::Html)
            .await
    )?;

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
        tg!(
            "send_message",
            bot.send_message(chat_id, "This command is only available in group chats.")
                .await
        )?;
        return Ok(());
    }

    if !is_owner {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                "Only the bot owner can change public access settings."
            )
            .await
        )?;
        return Ok(());
    }

    let arg = text
        .strip_prefix("/public")
        .unwrap_or("")
        .trim()
        .to_lowercase();
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
    tg!(
        "send_message",
        bot.send_message(chat_id, &response_msg)
            .parse_mode(ParseMode::Html)
            .await
    )?;

    Ok(())
}

/// Resolve a model name with provider prefix.
/// Returns Err(provider_name) if the provider binary is unavailable, or Err("") if the format is invalid.
fn resolve_model_name(name: &str) -> Result<String, &'static str> {
    // Strip display-name suffix (" — Description") that users may copy-paste
    // from the /model help text.
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
    } else if agy::is_agy_model(Some(clean)) {
        if agy::is_agy_available() {
            if let Some(model) = agy::strip_agy_prefix(clean) {
                if !agy::is_valid_agy_model(model) {
                    return Err("");
                }
                return Ok(format!("agy:{}", model));
            }
            Ok("agy".to_string())
        } else {
            Err("agy")
        }
    } else if opencode::is_opencode_model(Some(clean)) {
        if opencode::is_opencode_available() {
            Ok(clean.to_string())
        } else {
            Err("opencode")
        }
    } else {
        Err("") // invalid format
    }
}

/// Handle /effort command — set Claude/Codex effort for the current provider.
/// Usage:
///   /effort           — show current value + accepted values
///   /effort <level>   — set for current provider
///   /effort reset     — clear current provider override
async fn handle_effort_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let arg = command_args(text).to_lowercase();
    msg_debug(&format!(
        "[handle_effort_command] chat_id={}, arg={:?}",
        chat_id.0, arg
    ));
    let current_model = {
        let data = state.lock().await;
        get_model(&data.settings, chat_id)
    };
    let provider = detect_provider(current_model.as_deref());
    let provider_label = match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        _ => provider,
    };

    if arg.is_empty() {
        let current = {
            let data = state.lock().await;
            get_effort_for_provider(&data.settings, chat_id, provider)
        };
        let mut msg = if provider == "claude" || provider == "codex" {
            match &current {
                Some(v) => format!(
                    "Current {} effort: <b>{}</b>\n",
                    provider_label,
                    html_escape(v)
                ),
                None => format!("Current {} effort: <b>default</b>\n", provider_label),
            }
        } else {
            format!("Effort is supported only for Claude and Codex.\nCurrent model provider: <b>{}</b>\n", html_escape(provider_label))
        };
        if provider == "claude" || provider == "codex" {
            msg.push_str(&format!(
                "\nAccepted for {}: <code>{}</code>\n",
                provider_label,
                valid_effort_values(provider)
            ));
        }
        msg.push_str("\n<b>Usage:</b>\n");
        msg.push_str("<code>/effort low</code>, <code>/effort medium</code>, <code>/effort high</code>, <code>/effort xhigh</code>\n");
        msg.push_str("<code>/effort max</code> — Claude only\n");
        msg.push_str("<code>/effort minimal</code> — Codex only\n");
        msg.push_str("<code>/effort reset</code> — clear current provider override\n");
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, msg)
                .parse_mode(ParseMode::Html)
                .await
        )?;
        return Ok(());
    }

    if provider != "claude" && provider != "codex" {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                format!(
                    "/effort is supported only for Claude and Codex. Current provider: {}.",
                    provider_label
                ),
            )
            .await
        )?;
        return Ok(());
    }

    if arg == "reset" || arg == "clear" || arg == "default" {
        let removed = {
            let mut data = state.lock().await;
            let key = chat_id.0.to_string();
            let r = match provider {
                "claude" => data.settings.claude_effort.remove(&key).is_some(),
                "codex" => data.settings.effort.remove(&key).is_some(),
                _ => false,
            };
            save_bot_settings(token, &data.settings);
            r
        };
        let msg = if removed {
            format!(
                "{} effort cleared — provider default will be used.",
                provider_label
            )
        } else {
            format!("{} effort was not set.", provider_label)
        };
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, msg).await)?;
        return Ok(());
    }

    if !is_valid_effort_for_provider(provider, &arg) {
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                format!(
                    "Invalid {} effort value: '{}'. Accepted: {}, reset.",
                    provider_label,
                    arg,
                    valid_effort_values(provider)
                ),
            )
            .await
        )?;
        return Ok(());
    }

    {
        let mut data = state.lock().await;
        let key = chat_id.0.to_string();
        match provider {
            "claude" => {
                data.settings.claude_effort.insert(key, arg.clone());
            }
            "codex" => {
                data.settings.effort.insert(key, arg.clone());
            }
            _ => {}
        }
        save_bot_settings(token, &data.settings);
    };
    let arg_escaped = html_escape(&arg);
    let msg = format!("{} effort set to <b>{}</b>.", provider_label, arg_escaped);
    shared_rate_limit_wait(state, chat_id).await;
    tg!(
        "send_message",
        bot.send_message(chat_id, msg)
            .parse_mode(ParseMode::Html)
            .await
    )?;
    Ok(())
}

/// Handle /fast command — toggle Codex `service_tier="fast"` for the current chat.
async fn handle_fast_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let arg = command_args(text).to_lowercase();
    msg_debug(&format!(
        "[handle_fast_command] chat_id={}, arg={:?}",
        chat_id.0, arg
    ));
    let current_model = {
        let data = state.lock().await;
        get_model(&data.settings, chat_id)
    };
    let provider = detect_provider(current_model.as_deref());
    if provider != "codex" {
        let provider_label = match provider {
            "claude" => "Claude",
            "agy" => "Agy",
            "opencode" => "OpenCode",
            _ => provider,
        };
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(
                chat_id,
                format!(
                    "/fast is supported only for Codex. Current provider: {}.",
                    provider_label
                ),
            )
            .await
        )?;
        return Ok(());
    }

    if arg == "status" || arg == "show" {
        let enabled = {
            let data = state.lock().await;
            is_codex_fast(&data.settings, chat_id)
        };
        let status = if enabled {
            "⚡ Codex fast mode: ON (-c service_tier=\"fast\")"
        } else {
            "⚡ Codex fast mode: OFF (provider default)"
        };
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, status).await)?;
        return Ok(());
    }

    let requested = match arg.as_str() {
        "" => None,
        "on" | "enable" | "enabled" | "true" | "1" => Some(true),
        "off" | "disable" | "disabled" | "false" | "0" | "reset" | "clear" | "default" => {
            Some(false)
        }
        _ => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, "Usage: /fast, /fast on, /fast off, /fast status",)
                    .await
            )?;
            return Ok(());
        }
    };

    let next = {
        let mut data = state.lock().await;
        let key = chat_id.0.to_string();
        let prev = is_codex_fast(&data.settings, chat_id);
        let next = requested.unwrap_or(!prev);
        if next {
            data.settings.codex_fast.insert(key, true);
        } else {
            data.settings.codex_fast.remove(&key);
        }
        save_bot_settings(token, &data.settings);
        next
    };

    let status = if next {
        "⚡ Codex fast mode: ON (-c service_tier=\"fast\")"
    } else {
        "⚡ Codex fast mode: OFF (provider default)"
    };
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, status).await)?;
    Ok(())
}

/// Handle /stt_model command - set or view transcriptor model for this chat.
async fn handle_stt_model_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let arg = command_args(text);
    msg_debug(&format!(
        "[handle_stt_model_command] chat_id={}, arg={:?}",
        chat_id.0, arg
    ));
    let key = chat_id.0.to_string();

    if arg.is_empty() {
        let current = {
            let data = state.lock().await;
            get_stt_model(&data.settings, chat_id)
        };
        let msg = format!(
            "Current STT model: <b>{}</b>\n\n\
             <b>Usage:</b>\n\
             <code>/stt_model tiny</code>\n\
             <code>/stt_model base</code>\n\
             <code>/stt_model small</code>\n\
             <code>/stt_model medium</code>\n\
             <code>/stt_model large-v3</code>\n\
             <code>/stt_model large-v3-turbo</code>\n\
             <code>/stt_model path:/absolute/model.bin</code>\n\
             <code>/stt_model reset</code> — use transcriptor env/config/default",
            html_escape(&stt_model_setting_display(current.as_deref()))
        );
        shared_rate_limit_wait(state, chat_id).await;
        tg!(
            "send_message",
            bot.send_message(chat_id, msg)
                .parse_mode(ParseMode::Html)
                .await
        )?;
        return Ok(());
    }

    let normalized = match normalize_stt_model_setting(arg) {
        Ok(value) => value,
        Err(e) => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, e).await)?;
            return Ok(());
        }
    };

    match normalized {
        Some(value) => {
            {
                let mut data = state.lock().await;
                data.settings.stt_models.insert(key, value.clone());
                save_bot_settings(token, &data.settings);
            }
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(
                    chat_id,
                    format!(
                        "STT model set to <b>{}</b>.",
                        html_escape(&stt_model_setting_display(Some(&value)))
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await
            )?;
        }
        None => {
            let removed = {
                let mut data = state.lock().await;
                let removed = data.settings.stt_models.remove(&key).is_some();
                save_bot_settings(token, &data.settings);
                removed
            };
            let msg = if removed {
                "STT model reset. Transcriptor env/config/default will be used."
            } else {
                "STT model was not set. Transcriptor env/config/default is already used."
            };
            shared_rate_limit_wait(state, chat_id).await;
            tg!("send_message", bot.send_message(chat_id, msg).await)?;
        }
    }

    Ok(())
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
    msg_debug(&format!(
        "[handle_model_command] chat_id={}, arg={:?}",
        chat_id.0, arg
    ));

    if arg.is_empty() {
        // Show current model + available providers
        let current = {
            let data = state.lock().await;
            get_model(&data.settings, chat_id)
        };
        let has_claude = claude::is_claude_available();
        let has_codex = codex::is_codex_available();
        let has_agy = agy::is_agy_available();
        let has_opencode = opencode::is_opencode_available();

        let mut msg = match &current {
            Some(m) => format!("Current model: <b>{}</b>\n", m),
            None => {
                let default_provider = if has_claude {
                    "claude"
                } else if has_codex {
                    "codex"
                } else if has_agy {
                    "agy"
                } else {
                    "opencode"
                };
                format!("Current model: <b>default</b> ({})\n", default_provider)
            }
        };
        if has_claude {
            msg.push_str("\n<b>Claude:</b>\n");
            msg.push_str("<code>/model claude</code> — default\n");
            msg.push_str("<code>/model claude:sonnet</code> — Sonnet 4.6\n");
            msg.push_str("<code>/model claude:opus</code> — Opus 4.8\n");
            msg.push_str("<code>/model claude:haiku</code> — Haiku 4.5\n");
            msg.push_str("<code>/model claude:sonnet[1m]</code> — Sonnet 1M ctx\n");
        }
        if has_codex {
            msg.push_str("\n<b>Codex:</b>\n");
            msg.push_str("<code>/model codex</code> — default\n");
            msg.push_str(
                "<code>/model codex:gpt-5.5</code> — Latest frontier agentic coding model\n",
            );
            msg.push_str("<code>/model codex:gpt-5.4</code> — Frontier agentic coding model\n");
            msg.push_str("<code>/model codex:gpt-5.3-codex</code> — Frontier Codex-optimized agentic coding model\n");
            msg.push_str(
                "<code>/model codex:gpt-5.3-codex-spark</code> — Ultra-fast coding model\n",
            );
            msg.push_str(
                "<code>/model codex:gpt-5.2-codex</code> — Frontier agentic coding model\n",
            );
            msg.push_str("<code>/model codex:gpt-5.2</code> — Optimized for professional work and long-running agents\n");
            msg.push_str("<code>/model codex:gpt-5.1-codex-max</code> — Codex-optimized model for deep and fast reasoning\n");
            msg.push_str("<code>/model codex:gpt-5.1-codex-mini</code> — Optimized for codex. Cheaper, faster, but less capable\n");
        }
        if has_agy {
            msg.push_str("\n<b>Agy (Antigravity):</b>\n");
            msg.push_str("<code>/model agy</code> — default\n");
            for model_id in agy::list_models() {
                msg.push_str(&format!(
                    "<code>/model agy:{}</code>\n",
                    html_escape(&model_id)
                ));
            }
            msg.push_str("<i>Legacy /model gemini is accepted as an agy alias.</i>\n");
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
            tg!(
                "send_message",
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::Html)
                    .await
            )?;
        } else {
            // Build plain text version for file attachment
            let plain = msg
                .replace("<b>", "")
                .replace("</b>", "")
                .replace("<code>", "")
                .replace("</code>", "");
            let tmp_path = std::env::temp_dir().join(format!("models_{}.txt", chat_id.0));
            if let Err(e) = std::fs::write(&tmp_path, &plain) {
                msg_debug(&format!(
                    "[handle_model_command] failed to write tmp file: {}",
                    e
                ));
                return Ok(());
            }
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_document",
                bot.send_document(chat_id, teloxide::types::InputFile::file(&tmp_path),)
                    .await
            )?;
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
            let (provider_changed, had_state, old_path, queue_cleared) = {
                let mut data = state.lock().await;
                // If provider changed, clear session_id to avoid cross-provider resume.
                // Use detect_provider so the comparison reflects the *effective* provider
                // (CLI-availability fallback when model is unset) — matches the polling
                // guard's spawn-time capture and avoids missing user-visible transitions
                // like None→codex (Claude unavailable) → /model claude.
                let old_model = get_model(&data.settings, chat_id);
                let old_provider = detect_provider(old_model.as_deref());
                let new_provider = detect_provider(Some(&model_id));
                let provider_changed = old_provider != new_provider;
                msg_debug(&format!("[handle_model_command] old_model={:?}, old_provider={}, new_provider={}, provider_changed={}",
                    old_model, old_provider, new_provider, provider_changed));
                let (had_state, old_path, queue_cleared) = if provider_changed {
                    // Cancel any in-flight AI task on the old provider. Without this, its
                    // completion handler would later write the old provider's session_id and
                    // response into the (now-cleared) session, contradicting the "history has
                    // been reset" notice and leaving a stale cross-provider session_id behind.
                    cancel_in_progress_task_locked(&data, chat_id);
                    // Drop queued user messages: they targeted the old workspace context, and their
                    // captured pending_uploads point at the old workspace's files. Restoring them
                    // into the new (auto-created) workspace would prepend stale upload references
                    // to the next prompt, contradicting the "uploads have been reset" notice.
                    let dropped_queue = data
                        .message_queues
                        .remove(&chat_id)
                        .map(|q| q.len())
                        .unwrap_or(0);
                    // Cancel any verification loop. Its feedback was authored against the old
                    // provider's session and would be misapplied under the new provider.
                    data.loop_states.remove(&chat_id);
                    data.loop_feedback.remove(&chat_id);
                    if let Some(session) = data.sessions.get_mut(&chat_id) {
                        let prev_path = session.current_path.clone();
                        let had = session.session_id.is_some()
                            || prev_path.is_some()
                            || !session.history.is_empty()
                            || !session.pending_uploads.is_empty()
                            || dropped_queue > 0;
                        msg_debug(&format!("[handle_model_command] provider changed → clearing session + history + uploads + queue + loop (hist_len={}, uploads={}, queue={}, old_sid={:?}, old_path={:?})",
                            session.history.len(), session.pending_uploads.len(), dropped_queue, session.session_id, session.current_path));
                        session.session_id = None;
                        session.current_path = None;
                        session.history.clear();
                        session.clear_pending_uploads();
                        (had, prev_path, dropped_queue)
                    } else {
                        (dropped_queue > 0, None, dropped_queue)
                    }
                } else {
                    (false, None, 0)
                };
                data.settings
                    .models
                    .insert(chat_id.0.to_string(), model_id.clone());
                save_bot_settings(token, &data.settings);
                (provider_changed, had_state, old_path, queue_cleared)
            };
            shared_rate_limit_wait(state, chat_id).await;
            let queue_note = if queue_cleared > 0 {
                format!("\n{} queued message(s) discarded.", queue_cleared)
            } else {
                String::new()
            };
            let model_id_escaped = html_escape(&model_id);
            let msg = if provider_changed && had_state {
                if let Some(prev) = old_path {
                    let prev_display = crate::utils::format::to_shell_path(&prev);
                    format!(
                        "Model set to <b>{}</b>.\n\n\
                         Provider changed — previous workspace, history, and uploads have been reset for compatibility.{}\n\
                         Previous workspace: <code>{}</code> (preserved on disk)\n\
                         A new workspace will be created on your next message. To resume work in the previous workspace instead, use <code>/start &lt;path&gt;</code>.",
                        model_id_escaped, queue_note, html_escape(&prev_display)
                    )
                } else {
                    format!(
                        "Model set to <b>{}</b>.\n\n\
                         Provider changed — conversation history and uploads have been reset for compatibility.{} A new workspace will be created on your next message.",
                        model_id_escaped, queue_note
                    )
                }
            } else {
                format!("Model set to <b>{}</b>.", model_id_escaped)
            };
            tg!(
                "send_message",
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::Html)
                    .await
            )?;
        }
        Err(provider) if !provider.is_empty() => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(chat_id, format!("{provider} provider is not installed."))
                    .await
            )?;
        }
        Err(_) => {
            shared_rate_limit_wait(state, chat_id).await;
            tg!(
                "send_message",
                bot.send_message(
                    chat_id,
                    "Invalid format. Use:\n\
                 <code>/model claude</code> or <code>/model claude:&lt;model&gt;</code>\n\
                 <code>/model codex</code> or <code>/model codex:&lt;model&gt;</code>\n\
                 <code>/model agy</code> or <code>/model agy:&lt;model&gt;</code>\n\
                 <code>/model opencode</code> or <code>/model opencode:&lt;model&gt;</code>"
                )
                .parse_mode(ParseMode::Html)
                .await
            )?;
        }
    }

    Ok(())
}

async fn run_inline_text_dispatch(
    bot: Bot,
    chat_id: ChatId,
    state: SharedState,
    text: String,
    user_display_name: String,
    from_queue: bool,
    dispatch_id: u64,
    context: &'static str,
) {
    let state_for_task = state.clone();
    let request_tasks = {
        let data = state.lock().await;
        data.request_tasks.clone()
    };
    let join = spawn_tracked_request_task(request_tasks, async move {
        CURRENT_DISPATCH_ID
            .scope(dispatch_id, async move {
                handle_text_message(
                    &bot,
                    chat_id,
                    &text,
                    &state_for_task,
                    &user_display_name,
                    from_queue,
                )
                .await
            })
            .await
    });

    match join.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => msg_debug(&format!(
            "[{}] chat_id={}, handle_text_message FAILED: {}",
            context, chat_id.0, e
        )),
        Err(join_err) => {
            let panic_str = redact_known_tokens(&join_err.to_string());
            msg_debug(&format!(
                "[{}] chat_id={}, inline dispatch PANICKED: {}",
                context, chat_id.0, panic_str
            ));
            eprintln!(
                "  ⚠ Chat {} {} dispatch panicked: {} — recovering",
                chat_id.0, context, panic_str
            );
            reclaim_panicked_dispatch_token(&state, chat_id, dispatch_id, context).await;
        }
    }
}

async fn run_inline_stt_dispatch(
    bot: Bot,
    chat_id: ChatId,
    state: SharedState,
    msg: Message,
    user_display_name: String,
    caption_text: Option<String>,
    cancel_token: Arc<CancelToken>,
    dispatch_id: u64,
    context: &'static str,
) {
    let state_for_task = state.clone();
    let request_tasks = {
        let data = state.lock().await;
        data.request_tasks.clone()
    };
    let join = spawn_tracked_request_task(request_tasks, async move {
        CURRENT_DISPATCH_ID
            .scope(dispatch_id, async move {
                process_reserved_stt_message(
                    bot,
                    chat_id,
                    msg,
                    state_for_task,
                    user_display_name,
                    caption_text,
                    cancel_token,
                )
                .await;
            })
            .await
    });

    if let Err(join_err) = join.await {
        let panic_str = redact_known_tokens(&join_err.to_string());
        msg_debug(&format!(
            "[{}] chat_id={}, inline STT dispatch PANICKED: {}",
            context, chat_id.0, panic_str
        ));
        eprintln!(
            "  ⚠ Chat {} {} STT dispatch panicked: {} — recovering",
            chat_id.0, context, panic_str
        );
        reclaim_panicked_dispatch_token(&state, chat_id, dispatch_id, context).await;
    }
}

async fn run_inline_album_dispatch(
    bot: Bot,
    chat_id: ChatId,
    state: SharedState,
    msgs: Vec<Message>,
    user_display_name: String,
    caption_str: Option<String>,
    require_prefix: bool,
    bot_username: String,
    placeholder_token: Arc<CancelToken>,
    dispatch_id: u64,
    context: &'static str,
) {
    let state_for_task = state.clone();
    let request_tasks = {
        let data = state.lock().await;
        data.request_tasks.clone()
    };
    let join = spawn_tracked_request_task(request_tasks, async move {
        CURRENT_DISPATCH_ID
            .scope(dispatch_id, async move {
                process_album_attachments_task(
                    bot,
                    chat_id,
                    msgs,
                    state_for_task,
                    user_display_name,
                    caption_str,
                    require_prefix,
                    bot_username,
                    placeholder_token,
                    true,
                )
                .await;
            })
            .await
    });

    if let Err(join_err) = join.await {
        let panic_str = redact_known_tokens(&join_err.to_string());
        msg_debug(&format!(
            "[{}] chat_id={}, inline album dispatch PANICKED: {}",
            context, chat_id.0, panic_str
        ));
        eprintln!(
            "  ⚠ Chat {} {} album dispatch panicked: {} — recovering",
            chat_id.0, context, panic_str
        );
        reclaim_panicked_dispatch_token(&state, chat_id, dispatch_id, context).await;
    }
}

/// Dispatch pending loop feedback (stored in loop_feedback HashMap).
/// Called after loop verification determines the task is incomplete.
/// Uses the same boxed-future pattern as process_next_queued_message
/// to avoid a recursive async type between queue and feedback dispatch.
fn dispatch_loop_feedback<'a>(
    bot: &'a Bot,
    chat_id: ChatId,
    state: &'a SharedState,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let dispatch_id = next_dispatch_id();
        let feedback = {
            let mut data = state.lock().await;
            let fb = data.loop_feedback.remove(&chat_id);
            // Insert placeholder cancel_token so incoming messages see "busy"
            // during the gap before handle_text_message creates its own token.
            // Same pattern as process_next_queued_message (line 6628).
            if fb.is_some() {
                insert_cancel_token_locked(
                    &mut data,
                    chat_id,
                    new_cancel_token_for_owner(dispatch_id),
                );
                msg_debug(&format!(
                    "[loop:dispatch] chat_id={}, placeholder cancel_token inserted",
                    chat_id.0
                ));
            }
            fb
        };
        if let Some((text, user_display_name)) = feedback {
            msg_debug(&format!(
                "[loop:dispatch] chat_id={}, feedback_len={}",
                chat_id.0,
                text.len()
            ));
            // from_queue=true: handle_text_message will overwrite the placeholder
            // cancel_token instead of treating it as "another task active".
            run_inline_text_dispatch(
                bot.clone(),
                chat_id,
                state.clone(),
                text,
                user_display_name,
                true,
                dispatch_id,
                "loop:dispatch",
            )
            .await;
        } else {
            msg_debug(&format!(
                "[loop:dispatch] chat_id={}, no feedback found (may have been cleared by /stop)",
                chat_id.0
            ));
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
        let dispatch_id = next_dispatch_id();
        msg_debug(&format!(
            "[queue:next] chat_id={}, checking for next queued message",
            chat_id.0
        ));
        let next_msg = {
            let mut data = state.lock().await;
            // Skip if another task is already processing this chat
            if data.cancel_tokens.contains_key(&chat_id) {
                msg_debug(&format!(
                    "[queue:next] chat_id={}, cancel_token exists (another task active), skipping",
                    chat_id.0
                ));
                return;
            }
            // NOTE: We do NOT skip when queue mode is OFF. In OFF mode, the queue is used
            // as a single-slot buffer for redirect targets (latest-wins). If something is
            // pending here, it was placed by enqueue_redirect_locked and must be dispatched.
            let qkey = chat_id.0.to_string();
            let queue_enabled = data
                .settings
                .queue
                .get(&qkey)
                .copied()
                .unwrap_or(QUEUE_MODE_DEFAULT);
            let queue_len_before = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
            msg_debug(&format!(
                "[queue:next] chat_id={}, queue_mode={}, queue_len={}",
                chat_id.0, queue_enabled, queue_len_before
            ));
            let msg = data
                .message_queues
                .get_mut(&chat_id)
                .and_then(|q| q.pop_front());
            // Restore pending_uploads captured at queue time so handle_text_message picks them up
            if let Some(ref m) = msg {
                let preview = m.preview_text();
                let uploads_len = m.pending_uploads_len();
                msg_debug(&format!("[queue:next] chat_id={}, popped message: text={:?}, user={:?}, uploads={}, queue_before={}", chat_id.0, truncate_str(&preview, 60), m.user_display_name, uploads_len, queue_len_before));
                if let QueuedPayload::Text {
                    pending_uploads, ..
                } = &m.payload
                {
                    if !pending_uploads.is_empty() {
                        if let Some(session) = data.sessions.get_mut(&chat_id) {
                            session.extend_pending_uploads(pending_uploads.iter().cloned());
                            msg_debug(&format!(
                                "[queue:next] chat_id={}, restored {} pending_uploads to session",
                                chat_id.0,
                                pending_uploads.len()
                            ));
                        } else {
                            msg_debug(&format!("[queue:next] chat_id={}, WARNING: no session to restore uploads to", chat_id.0));
                        }
                    }
                }
                // Insert placeholder cancel_token so new messages see "busy" during Dequeued notification.
                // Without this, messages arriving between pop and handle_text_message's cancel_token insert
                // would see "not busy" and bypass the queue (FIFO violation).
                // Text dispatch overwrites this with its own real cancel_token;
                // STT/album dispatch uses this same token while transcribing.
                insert_cancel_token_locked(
                    &mut data,
                    chat_id,
                    new_cancel_token_for_owner(dispatch_id),
                );
                msg_debug(&format!(
                    "[queue:next] chat_id={}, placeholder cancel_token inserted",
                    chat_id.0
                ));
            } else {
                msg_debug(&format!(
                    "[queue:next] chat_id={}, queue empty (len was {})",
                    chat_id.0, queue_len_before
                ));
            }
            // Clean up empty queue entry to prevent memory leak
            if data
                .message_queues
                .get(&chat_id)
                .map_or(false, |q| q.is_empty())
            {
                data.message_queues.remove(&chat_id);
                msg_debug(&format!(
                    "[queue:next] chat_id={}, removed empty queue entry",
                    chat_id.0
                ));
            }
            msg
        };

        if let Some(queued) = next_msg {
            let preview = queued.preview_text();
            let placeholder_token = {
                let data = state.lock().await;
                data.cancel_tokens.get(&chat_id).cloned()
            };
            let Some(placeholder_token) = placeholder_token else {
                msg_debug(&format!("[queue:next] chat_id={}, missing placeholder token for id={}, dropping queued item", chat_id.0, queued.id));
                return;
            };
            msg_debug(&format!(
                "[queue:next] chat_id={}, dispatching id={}, text={:?}",
                chat_id.0,
                queued.id,
                truncate_str(&preview, 60)
            ));
            shared_rate_limit_wait(state, chat_id).await;
            let _ = tg!(
                "send_message",
                bot.send_message(chat_id, &format!("Dequeued ({})", queued.id))
                    .await
            );

            match queued.payload {
                QueuedPayload::Text { text, .. } => {
                    run_inline_text_dispatch(
                        bot.clone(),
                        chat_id,
                        state.clone(),
                        text,
                        queued.user_display_name,
                        true,
                        dispatch_id,
                        "queue:next",
                    )
                    .await;
                }
                QueuedPayload::SttAudio { msg, caption_text } => {
                    run_inline_stt_dispatch(
                        bot.clone(),
                        chat_id,
                        state.clone(),
                        msg,
                        queued.user_display_name,
                        caption_text,
                        placeholder_token,
                        dispatch_id,
                        "queue:next",
                    )
                    .await;
                }
                QueuedPayload::AlbumAttachments {
                    msgs,
                    caption_str,
                    require_prefix,
                    bot_username,
                } => {
                    run_inline_album_dispatch(
                        bot.clone(),
                        chat_id,
                        state.clone(),
                        msgs,
                        queued.user_display_name,
                        caption_str,
                        require_prefix,
                        bot_username,
                        placeholder_token,
                        dispatch_id,
                        "queue:next",
                    )
                    .await;
                }
            }
            msg_debug(&format!(
                "[queue:next] chat_id={}, id={}, inline dispatch returned",
                chat_id.0, queued.id
            ));
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
    msg_debug(&format!(
        "[handle_text_message] START chat_id={}, user_text={:?}, from_queue={}",
        chat_id.0,
        truncate_str(user_text, 100),
        from_queue
    ));
    ai_trace(&format!(
        "[START] chat_id={}, user_text={:?}, from_queue={}",
        chat_id.0,
        truncate_str(user_text, 100),
        from_queue
    ));

    // Register cancel token early (prevents duplicate requests while waiting for group lock)
    let cancel_token = new_cancel_token();
    // busy_notify: (queue_enabled, queued_id, redirect_info)
    //   queue_enabled: ON/OFF mode
    //   queued_id:     ON mode push id (Some=queued, None=full)
    //   redirect_info: OFF mode redirect (Some(id, was_replacement))
    let busy_notify: Option<(bool, Option<String>, Option<(String, bool)>)> = {
        let mut data = state.lock().await;
        // For queued messages: check if placeholder was cancelled (e.g., /stop during dequeue window)
        if from_queue {
            let placeholder_cancelled = data
                .cancel_tokens
                .get(&chat_id)
                .map(|t| t.cancelled.load(Ordering::Relaxed))
                .unwrap_or(false);
            if placeholder_cancelled {
                msg_debug(&format!(
                    "[handle_text_message] chat_id={}, placeholder cancelled during dequeue",
                    chat_id.0
                ));
                // Clean up pending_uploads restored for this cancelled message
                if let Some(session) = data.sessions.get_mut(&chat_id) {
                    if !session.pending_uploads.is_empty() {
                        msg_debug(&format!("[handle_text_message] chat_id={}, clearing {} pending_uploads from cancelled dequeue", chat_id.0, session.pending_uploads.len()));
                        session.clear_pending_uploads();
                    }
                }
                remove_cancel_token_locked(&mut data, chat_id);
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
        // For non-queued messages: if another task is active, queue (ON) or redirect (OFF)
        if !from_queue && data.cancel_tokens.contains_key(&chat_id) {
            msg_debug(&format!(
                "[handle_text_message] chat_id={}, cancel_token exists (busy)",
                chat_id.0
            ));
            let qkey = chat_id.0.to_string();
            let qmode = data
                .settings
                .queue
                .get(&qkey)
                .copied()
                .unwrap_or(QUEUE_MODE_DEFAULT);
            let (qid, redirect) = if qmode {
                let cur_len = data.message_queues.get(&chat_id).map_or(0, |q| q.len());
                let qid = if cur_len < MAX_QUEUE_SIZE {
                    let uploads = data
                        .sessions
                        .get_mut(&chat_id)
                        .map(|s| s.take_pending_uploads())
                        .unwrap_or_default();
                    let id = generate_queue_id();
                    data.message_queues
                        .entry(chat_id)
                        .or_insert_with(std::collections::VecDeque::new)
                        .push_back(QueuedMessage {
                            id: id.clone(),
                            user_display_name: user_display_name.to_string(),
                            payload: QueuedPayload::Text {
                                text: user_text.to_string(),
                                pending_uploads: uploads,
                            },
                        });
                    msg_debug(&format!(
                        "[handle_text_message] chat_id={}, QUEUED id={}",
                        chat_id.0, id
                    ));
                    Some(id)
                } else {
                    msg_debug(&format!(
                        "[handle_text_message] chat_id={}, queue FULL",
                        chat_id.0
                    ));
                    None
                };
                (qid, None)
            } else {
                // OFF mode: messages reaching handle_text_message are user-prompt-style (slash
                // commands are routed elsewhere in handle_message), so redirect them.
                let uploads = data
                    .sessions
                    .get_mut(&chat_id)
                    .map(|s| s.take_pending_uploads())
                    .unwrap_or_default();
                let (rid, replaced) = enqueue_redirect_locked(
                    &mut *data,
                    chat_id,
                    user_text.to_string(),
                    user_display_name.to_string(),
                    uploads,
                );
                msg_debug(&format!(
                    "[handle_text_message] chat_id={}, REDIRECT id={}, replaced={}",
                    chat_id.0, rid, replaced
                ));
                (None, Some((rid, replaced)))
            };
            Some((qmode, qid, redirect))
        } else {
            insert_cancel_token_locked(&mut data, chat_id, cancel_token.clone());
            None
        }
    };
    if let Some((queue_enabled, queued_id, redirect_info)) = busy_notify {
        shared_rate_limit_wait(state, chat_id).await;
        if let Some((_rid, replaced)) = redirect_info {
            let preview = truncate_str(user_text, 30);
            let msg = if replaced {
                format!("🔄 Redirect target updated: \"{preview}\"")
            } else {
                format!("🔄 Cancelling current task, will process: \"{preview}\"")
            };
            tg!("send_message", bot.send_message(chat_id, &msg).await)?;
        } else if queue_enabled {
            if let Some(qid) = queued_id {
                let preview = truncate_str(user_text, 30);
                tg!("send_message", bot.send_message(chat_id, &format!("Queued ({qid}) \"{preview}\"\n- /stopall to cancel all\n- /stop_{qid} to cancel this"))
                    .await)?;
            } else {
                tg!(
                    "send_message",
                    bot.send_message(
                        chat_id,
                        &format!(
                            "Queue full (max {}). Use /stopall to clear.",
                            MAX_QUEUE_SIZE
                        )
                    )
                    .await
                )?;
            }
        } else {
            tg!(
                "send_message",
                bot.send_message(chat_id, "AI request in progress. Use /stop to cancel.")
                    .await
            )?;
        }
        return Ok(());
    }

    // Group chat lock acquisition is deferred into the spawned polling task
    // below so the per-chat FIFO worker can pull the next message (e.g.
    // `/stop`) immediately instead of waiting for cross-bot lock contention.
    // The cancel_token reservation above already prevents a duplicate AI
    // request on this bot for this chat.

    // Get session info, allowed tools, model, pending uploads, history, instruction, and bot_username (drop lock before any await)
    let (
        session_info,
        allowed_tools,
        pending_uploads,
        model,
        history,
        instruction,
        context_count,
        bot_username_for_prompt,
        bot_display_name_for_prompt,
        chrome_enabled,
        effort,
        codex_fast_enabled,
    ) = {
        let mut data = state.lock().await;
        let info = data.sessions.get(&chat_id).and_then(|session| {
            session.current_path.as_ref().map(|_| {
                (
                    session.session_id.clone(),
                    session.current_path.clone().unwrap_or_default(),
                )
            })
        });
        let tools = get_allowed_tools(&data.settings, chat_id);
        let mdl = get_model(&data.settings, chat_id);
        let hist = data
            .sessions
            .get(&chat_id)
            .map(|s| s.history.clone())
            .unwrap_or_default();
        // Drain pending uploads so they are sent to Claude exactly once
        let uploads = data
            .sessions
            .get_mut(&chat_id)
            .map(|s| s.take_pending_uploads())
            .unwrap_or_default();
        let instr = data
            .settings
            .instructions
            .get(&chat_id.0.to_string())
            .cloned();
        let ctx_count = data
            .settings
            .context
            .get(&chat_id.0.to_string())
            .copied()
            .unwrap_or(12);
        let buname = data.bot_username.clone();
        let bdname = data.bot_display_name.clone();
        let chrome = data
            .settings
            .use_chrome
            .get(&chat_id.0.to_string())
            .copied()
            .unwrap_or(false);
        let provider = detect_provider(mdl.as_deref());
        let effort = get_effort_for_provider(&data.settings, chat_id, provider);
        let codex_fast = is_codex_fast(&data.settings, chat_id);
        msg_debug(&format!("[handle_text_message] session_id={:?}, current_path={:?}, model={:?}, uploads={}, history_len={}, instruction={:?}",
            info.as_ref().map(|(sid, _)| sid), info.as_ref().map(|(_, p)| p), mdl, uploads.len(), hist.len(), instr.is_some()));
        (
            info, tools, uploads, mdl, hist, instr, ctx_count, buname, bdname, chrome, effort,
            codex_fast,
        )
    };

    let (session_id, current_path) = match session_info {
        Some(info) => info,
        None => {
            // Auto-create a workspace session instead of rejecting the message
            let Some((auto_sid, auto_current)) =
                auto_create_workspace_session(bot, state, chat_id, bot.token()).await
            else {
                {
                    let mut data = state.lock().await;
                    remove_cancel_token_locked(&mut data, chat_id);
                }
                shared_rate_limit_wait(state, chat_id).await;
                let _ = tg!(
                    "send_message",
                    bot.send_message(chat_id, "Failed to create workspace.")
                        .await
                );
                process_next_queued_message(bot, chat_id, state).await;
                return Ok(());
            };
            (auto_sid, auto_current)
        }
    };

    // Note: user message is NOT added to history here.
    // It will be added together with the assistant response in the spawned task,
    // only on successful completion. On cancel, nothing is recorded.

    // Placeholder send is deferred into the spawned polling task — it happens
    // after the group_lock is acquired there, matching the prior ordering
    // (placeholder appears once cross-bot serialization permits this bot to
    // start). This also avoids leaving an orphan placeholder if `/stop`
    // arrives during lock wait.

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
    let default_tools: std::collections::HashSet<&str> =
        DEFAULT_ALLOWED_TOOLS.iter().copied().collect();
    let allowed_set: std::collections::HashSet<&str> =
        allowed_tools.iter().map(|s| s.as_str()).collect();
    let disabled: Vec<&&str> = default_tools
        .iter()
        .filter(|t| !allowed_set.contains(**t))
        .collect();
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
        Some(instr) => format!(
            "You are chatting with a user through {}.\n\nUser's instruction for this chat:\n{}",
            platform, instr
        ),
        None => format!("You are chatting with a user through {}.", platform),
    };
    ai_trace(&format!(
        "[PROMPT] platform={}, role_len={}, path={}, bot_key=<redacted>",
        platform,
        role.len(),
        current_path
    ));
    let system_prompt_owned = build_system_prompt(
        &role,
        &current_path,
        chat_id.0,
        &bot_key_for_prompt,
        &disabled_notice,
        session_id.as_deref(),
        &bot_username_for_prompt,
        &bot_display_name_for_prompt,
        Some(user_text),
        context_count,
        &platform,
    );

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();

    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();

    // AI backend spawn is deferred into the polling task below — it runs only
    // after the cross-bot group_lock is acquired, preserving the original
    // semantic that two bots in the same group chat do not run AI concurrently.
    // Variables prepared here (model_clone, history_clone, context_prompt,
    // system_prompt_owned, allowed_tools, chrome_enabled, bot_key_for_prompt,
    // session_id_clone, current_path_clone, cancel_token_clone, tx) are
    // captured by the polling task's `async move` closure and consumed by the
    // inner `spawn_tracked_blocking_task` call.
    let model_clone = model.clone();
    let history_clone = history;
    // None when unset: let the provider's own default/config apply.
    let effort_clone: Option<String> = effort;
    let codex_fast_clone = codex_fast_enabled;

    // Spawn the polling loop as a separate task so the handler returns immediately.
    // This allows teloxide's per-chat worker to process subsequent messages (e.g. /stop).
    let bot_owned = bot.clone();
    let state_owned = state.clone();
    let user_text_owned = user_text.to_string();
    let bot_username_for_log = bot_username_for_prompt.clone();
    let bot_display_name_for_log = bot_display_name_for_prompt.clone();
    let user_display_name_owned = user_display_name.to_string();
    let provider_str: &'static str = detect_provider(model.as_deref());
    // Captured at spawn so the post-completion guard can detect /clear or /start
    // session-id swaps. The provider/path comparison alone misses same-path same-provider
    // mutations (e.g. /clear nullifies session_id; /start <session-id> swaps to a different
    // session at the same path). clear_epoch covers the brand-new-session case where
    // session_id is None on both sides of the comparison.
    let captured_sid = session_id.clone();
    let captured_clear_epoch = {
        let data = state.lock().await;
        data.clear_epoch.get(&chat_id).copied().unwrap_or(0)
    };
    let request_tasks = {
        let data = state.lock().await;
        data.request_tasks.clone()
    };
    // Cloned for the inner AI spawn (the outer call below consumes the original).
    let request_tasks_for_ai = request_tasks.clone();
    let _ = spawn_tracked_request_task(request_tasks, async move {
        // Acquire the cross-bot group chat lock here (was previously inline,
        // which head-of-line-blocked the per-chat FIFO worker — see
        // `run_chat_worker`). The cancel_token reservation in the inline
        // section already guards this bot's per-chat slot, so doing the wait
        // here only delays the placeholder send, not subsequent messages.
        let group_lock = acquire_group_chat_lock(chat_id.0).await;

        // /stop arriving while we waited for the group lock sets cancelled=true.
        // AI has not been spawned yet (we spawn it below, after the lock is
        // acquired and the placeholder is sent), so cleanup is just state +
        // queue processing; no child to SIGKILL, no API calls billed.
        if cancel_token.cancelled.load(Ordering::Relaxed) {
            msg_debug(&format!(
                "[queue:trigger] chat_id={}, source=text_cancelled_during_lock",
                chat_id.0
            ));
            {
                let mut data = state_owned.lock().await;
                remove_cancel_token_locked(&mut data, chat_id);
            }
            drop(group_lock);
            process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
            return;
        }
        let _group_lock = group_lock; // hold group chat lock until task ends

        // Send placeholder message (was previously inline before this spawn).
        // AI is spawned only after the placeholder succeeds, so a network
        // error here means no AI subprocess to clean up.
        shared_rate_limit_wait(&state_owned, chat_id).await;
        let placeholder = match tg!("send_message", bot_owned.send_message(chat_id, "...").await) {
            Ok(m) => m,
            Err(e) => {
                msg_debug(&format!(
                    "[queue:trigger] chat_id={}, source=text_placeholder_error: {}",
                    chat_id.0, e
                ));
                {
                    let mut data = state_owned.lock().await;
                    remove_cancel_token_locked(&mut data, chat_id);
                }
                process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
                return;
            }
        };
        let mut placeholder_msg_id = placeholder.id;

        // Now that the lock is held and the placeholder is up, spawn the AI
        // backend. This was previously inline in `handle_text_message`, but
        // running it before the lock was held meant that two bots in the
        // same group chat could both have their AI subprocesses running
        // simultaneously (the lock only serialized the *output* path), and
        // that `/stop` arriving during lock wait could no longer prevent
        // billing because the API call had already started.
        msg_debug(&format!("[handle_text_message] prompt_len={}, system_prompt_len={}, session_id={:?}, path={}, history_len={}",
            context_prompt.len(), system_prompt_owned.len(), session_id_clone, current_path_clone, history_clone.len()));
        let _ = spawn_tracked_blocking_task(request_tasks_for_ai, move || {
            let provider = detect_provider(model_clone.as_deref());
            msg_debug(&format!(
                "[handle_text_message] provider={}, model={:?}",
                provider, model_clone
            ));
            ai_trace(&format!("[EXEC] provider={}, model={:?}, prompt_len={}, system_prompt_len={}, history_len={}, session={:?}, path={}",
                provider, model_clone, context_prompt.len(), system_prompt_owned.len(), history_clone.len(), session_id_clone, current_path_clone));
            let result = if provider == "opencode" {
                let opencode_model = model_clone
                    .as_deref()
                    .and_then(opencode::strip_opencode_prefix);
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
            } else if provider == "agy" {
                let agy_model = model_clone.as_deref().and_then(agy::strip_agy_prefix);
                msg_debug(&format!("[handle_text_message] → agy::execute, agy_model={:?}, session_id={:?}, path={}, prompt_len={}, system_prompt_len={}",
                    agy_model, session_id_clone, current_path_clone, context_prompt.len(), system_prompt_owned.len()));
                agy::execute_command_streaming(
                    &context_prompt,
                    session_id_clone.as_deref(),
                    &current_path_clone,
                    tx.clone(),
                    Some(&system_prompt_owned),
                    Some(&allowed_tools),
                    Some(cancel_token_clone),
                    agy_model,
                    false,
                )
            } else if provider == "codex" {
                let codex_model = model_clone.as_deref().and_then(codex::strip_codex_prefix);
                let codex_system_prompt =
                    format!("{}{}", system_prompt_owned, codex_extra_instructions());
                let is_resume = session_id_clone.is_some();
                let has_history = !history_clone.is_empty();
                msg_debug(&format!("[handle_text_message] codex: is_resume={}, has_history={}, history_len={}, sp_len={}",
                    is_resume, has_history, history_clone.len(), codex_system_prompt.len()));
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
                            _ => continue,
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
                        msg_debug(
                            "[handle_text_message] codex: NEW session, no history — sp via -c file",
                        );
                    }
                    context_prompt.clone()
                };
                msg_debug(&format!("[handle_text_message] → codex::execute, codex_model={:?}, codex_prompt_len={}, resume={}, system_prompt_passed=true",
                    codex_model, codex_prompt.len(), is_resume));
                let codex_auto_send = codex::CodexAutoSendCtx {
                    cokacdir_bin: crate::bin_path().to_string(),
                    chat_id: chat_id.0,
                    bot_key: bot_key_for_prompt.clone(),
                };
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
                    Some(&codex_auto_send),
                    effort_clone.as_deref(),
                    codex_fast_clone,
                )
            } else {
                let claude_model = model_clone.as_deref().and_then(claude::strip_claude_prefix);
                msg_debug(&format!(
                    "[handle_text_message] → claude::execute, claude_model={:?}",
                    claude_model
                ));
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
                    effort_clone.as_deref(),
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
                let _ = tx.send(StreamMessage::Error {
                    message: e,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
        });

        const SPINNER: &[&str] = &[
            "🕐 P",
            "🕑 Pr",
            "🕒 Pro",
            "🕓 Proc",
            "🕔 Proce",
            "🕕 Proces",
            "🕖 Process",
            "🕗 Processi",
            "🕘 Processin",
            "🕙 Processing",
            "🕚 Processing.",
            "🕛 Processing..",
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
                if !done {
                    cancelled = true;
                }
                break;
            }

            // Sleep as polling interval (without reserving a rate limit slot)
            tokio::time::sleep(tokio::time::Duration::from_millis(polling_time_ms)).await;

            // Check cancel token again after sleep
            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done {
                    cancelled = true;
                }
                break;
            }

            // === Phase 1: AI streaming (while !done) ===
            if !done {
                // Drain all available messages
                loop {
                    match rx.try_recv() {
                        Ok(msg) => match msg {
                            StreamMessage::Init { session_id: sid } => {
                                msg_debug(&format!("[polling] Init: session_id={}", sid));
                                ai_trace(&format!("[STREAM] Init: session_id={}", sid));
                                new_session_id = Some(sid);
                            }
                            StreamMessage::Text { content } => {
                                msg_debug(&format!(
                                    "[polling] Text: {} chars, preview={:?}",
                                    content.len(),
                                    truncate_str(&content, 80)
                                ));
                                ai_trace(&format!(
                                    "[STREAM] Text: {} chars, total_so_far={}",
                                    content.len(),
                                    full_response.len() + content.len()
                                ));
                                raw_entries.push(RawPayloadEntry {
                                    tag: "Text".into(),
                                    content: content.clone(),
                                });
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
                                raw_entries.push(RawPayloadEntry {
                                    tag: "ToolUse".into(),
                                    content: format!("{}: {}", name, input),
                                });
                                if !pending_cokacdir && !silent_mode {
                                    let _fr_before = full_response.len();
                                    if name == "Bash" {
                                        full_response.push_str(&format!(
                                            "\n\n```\n{}\n```\n",
                                            format_bash_command(&input)
                                        ));
                                        msg_debug(&format!("[fr_trace][{}] +ToolUse/Bash: added={}, cmd={:?}, total={} (was {})",
                                                chat_id.0, full_response.len() - _fr_before, truncate_str(&input, 100), full_response.len(), _fr_before));
                                    } else {
                                        full_response.push_str(&format!("\n\n⚙️ {}\n", summary));
                                        msg_debug(&format!("[fr_trace][{}] +ToolUse/{}: added={}, summary={:?}, total={} (was {})",
                                                chat_id.0, name, full_response.len() - _fr_before, truncate_str(&summary, 100), full_response.len(), _fr_before));
                                    }
                                } else if !pending_cokacdir
                                    && silent_mode
                                    && !full_response.is_empty()
                                    && !full_response.ends_with('\n')
                                {
                                    msg_debug(&format!(
                                        "[polling] silent mode: inserting \\n\\n after tool_use={}",
                                        name
                                    ));
                                    full_response.push_str("\n\n");
                                    msg_debug(&format!(
                                        "[fr_trace][{}] +ToolUse/silent_nl: added=2, total={}",
                                        chat_id.0,
                                        full_response.len()
                                    ));
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
                                raw_entries.push(RawPayloadEntry {
                                    tag: "ToolResult".into(),
                                    content: format!("is_error={}, content={}", is_error, content),
                                });
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
                                        full_response
                                            .push_str(&format!("\n```\n{}\n```\n", truncated));
                                    } else {
                                        full_response.push_str(&format!("\n`{}`\n\n", truncated));
                                    }
                                    msg_debug(&format!("[fr_trace][{}] +ToolResult/error({}): added={}, preview={:?}, total={} (was {})",
                                            chat_id.0, last_tool_name, full_response.len() - _fr_before, truncate_str(&truncated, 200), full_response.len(), _fr_before));
                                } else if !silent_mode {
                                    if last_tool_name == "Read" {
                                        full_response.push_str(&format!(
                                            "\n✅ `{} bytes`\n\n",
                                            content.len()
                                        ));
                                        msg_debug(&format!("[fr_trace][{}] +ToolResult/Read: added={}, content_bytes={}, total={} (was {})",
                                                chat_id.0, full_response.len() - _fr_before, content.len(), full_response.len(), _fr_before));
                                    } else if !content.is_empty() {
                                        let truncated = truncate_str(&content, 300);
                                        if truncated.contains('\n') {
                                            full_response
                                                .push_str(&format!("\n```\n{}\n```\n", truncated));
                                        } else {
                                            full_response
                                                .push_str(&format!("\n✅ `{}`\n\n", truncated));
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
                                    raw_entries.push(RawPayloadEntry {
                                        tag: "TaskNotification".into(),
                                        content: summary.clone(),
                                    });
                                    let _fr_before = full_response.len();
                                    full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                    msg_debug(&format!("[fr_trace][{}] +TaskNotification: added={}, summary={:?}, total={} (was {})",
                                            chat_id.0, full_response.len() - _fr_before, truncate_str(&summary, 200), full_response.len(), _fr_before));
                                }
                            }
                            StreamMessage::Done {
                                result,
                                session_id: sid,
                            } => {
                                msg_debug(&format!("[polling] Done: result_len={}, session_id={:?}, full_response_len={}",
                                        result.len(), sid, full_response.len()));
                                ai_trace(&format!("[STREAM] Done: result_len={}, full_response_len={}, session_id={:?}", result.len(), full_response.len(), sid));
                                if !result.is_empty() && full_response.is_empty() {
                                    msg_debug(&format!(
                                        "[polling] Done: fallback full_response = result ({})",
                                        result.len()
                                    ));
                                    full_response = result.clone();
                                    msg_debug(&format!("[fr_trace][{}] +Done/fallback: set={}, preview={:?}, total={}",
                                            chat_id.0, result.len(), truncate_str(&full_response, 200), full_response.len()));
                                } else if !result.is_empty() {
                                    msg_debug(&format!("[fr_trace][{}] Done/discarded: result_len={} discarded (full_response already has {})",
                                            chat_id.0, result.len(), full_response.len()));
                                }
                                if !result.is_empty() && raw_entries.is_empty() {
                                    raw_entries.push(RawPayloadEntry {
                                        tag: "Text".into(),
                                        content: result,
                                    });
                                }
                                if let Some(s) = sid {
                                    new_session_id = Some(s);
                                }
                                msg_debug(&format!(
                                    "[fr_trace][{}] =DONE: final_total={}",
                                    chat_id.0,
                                    full_response.len()
                                ));
                                done = true;
                            }
                            StreamMessage::Error {
                                message,
                                stdout,
                                stderr,
                                exit_code,
                            } => {
                                msg_debug(&format!("[polling] Error: message={}, exit_code={:?}, stdout_len={}, stderr_len={}",
                                        message, exit_code, stdout.len(), stderr.len()));
                                ai_trace(&format!("[STREAM] Error: message={}, exit_code={:?}, stdout_len={}, stderr_len={}", message, exit_code, stdout.len(), stderr.len()));
                                let stdout_display = if stdout.is_empty() {
                                    "(empty)".to_string()
                                } else {
                                    stdout
                                };
                                let stderr_display = if stderr.is_empty() {
                                    "(empty)".to_string()
                                } else {
                                    stderr
                                };
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
                                raw_entries.push(RawPayloadEntry {
                                    tag: "Error".into(),
                                    content: format!(
                                        "exit_code={}, message={}, stdout={}, stderr={}",
                                        code_display, message, stdout_display, stderr_display
                                    ),
                                });
                                done = true;
                            }
                        },
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            ai_trace(&format!(
                                "[STREAM] Channel DISCONNECTED: full_response_len={}",
                                full_response.len()
                            ));
                            done = true;
                            break;
                        }
                    }
                }

                if !done {
                    // ── Rolling placeholder pattern (unified for all chats) ──
                    let threshold = file_attach_threshold();
                    let delta_end =
                        floor_char_boundary(&full_response, full_response.len().min(threshold));
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
                                let _ = tg!(
                                    "edit_message",
                                    bot_owned
                                        .edit_message_text(chat_id, placeholder_msg_id, &html_delta)
                                        .parse_mode(ParseMode::Html)
                                        .await
                                );
                            } else {
                                // Delta too large for single edit — send via send_long_message
                                if send_long_message(
                                    &bot_owned,
                                    chat_id,
                                    &html_delta,
                                    Some(ParseMode::Html),
                                    &state_owned,
                                )
                                .await
                                .is_ok()
                                {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!(
                                        "delete_message",
                                        bot_owned.delete_message(chat_id, placeholder_msg_id).await
                                    );
                                } else {
                                    let truncated_delta =
                                        truncate_str(&normalized_delta, TELEGRAM_MSG_LIMIT);
                                    let _ = tg!(
                                        "edit_message",
                                        bot_owned
                                            .edit_message_text(
                                                chat_id,
                                                placeholder_msg_id,
                                                &truncated_delta
                                            )
                                            .await
                                    );
                                }
                            }
                            last_confirmed_len = delta_end;
                            // Create new placeholder for next cycle
                            let old_ph_id = placeholder_msg_id;
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            match tg!("send_message", bot_owned.send_message(chat_id, "...").await)
                            {
                                Ok(new_ph) => {
                                    placeholder_msg_id = new_ph.id;
                                    msg_debug(&format!("[rolling_ph] NEW placeholder: old_msg_id={}, new_msg_id={}", old_ph_id, placeholder_msg_id));
                                }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!(
                                        "  [{ts}]   ⚠ new placeholder failed: {}",
                                        redact_err(&e)
                                    );
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
                            if let Err(e) = tg!(
                                "edit_message",
                                &state_owned,
                                chat_id,
                                bot_owned
                                    .edit_message_text(chat_id, placeholder_msg_id, &html_text)
                                    .parse_mode(ParseMode::Html)
                                    .await
                            ) {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!(
                                    "  [{ts}]   ⚠ edit_message failed (streaming): {}",
                                    redact_err(&e)
                                );
                            }
                            last_edit_text = display_text;
                        } else {
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!(
                                "send_chat_action",
                                bot_owned
                                    .send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
                                    .await
                            );
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

                // Final response. Capture "AI produced no output at all" before
                // the sentinel replacement so (a) the UI can silently delete the
                // placeholder instead of editing it to the user-confusing
                // "(No response)" string, and (b) the session writeback below
                // can skip the assistant push so the sentinel does not end up
                // in saved history / future /start preview / future --resume.
                let was_originally_empty = full_response.is_empty();
                if was_originally_empty {
                    ai_trace(&format!("[FINAL] full_response is EMPTY → (No response). cancelled={}, last_confirmed_len={}", cancelled, last_confirmed_len));
                    full_response = "(No response)".to_string();
                    msg_debug(&format!(
                        "[fr_trace][{}] =NoResponse: set to '(No response)', cancelled={}",
                        chat_id.0, cancelled
                    ));
                } else {
                    ai_trace(&format!(
                        "[FINAL] full_response_len={}, last_confirmed_len={}, remaining_len={}",
                        full_response.len(),
                        last_confirmed_len,
                        full_response.len().saturating_sub(last_confirmed_len)
                    ));
                }

                let final_response = normalize_empty_lines(&full_response);

                // ── Send only remaining delta (unified rolling placeholder) ──
                if full_response.len() < last_confirmed_len
                    || !full_response.is_char_boundary(last_confirmed_len)
                {
                    last_confirmed_len = 0;
                }
                let remaining = &full_response[last_confirmed_len..];
                msg_debug(&format!("[rolling_ph] FINAL: placeholder_msg_id={}, confirmed={}, total={}, remaining_len={}",
                    placeholder_msg_id, last_confirmed_len, full_response.len(), remaining.trim().len()));
                if was_originally_empty || remaining.trim().is_empty() {
                    // No new content — delete the spinner placeholder
                    msg_debug(&format!(
                        "[rolling_ph] FINAL DELETE placeholder: msg_id={}",
                        placeholder_msg_id
                    ));
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!(
                        "delete_message",
                        bot_owned.delete_message(chat_id, placeholder_msg_id).await
                    );
                } else if should_attach_response_as_file(full_response.len(), provider_str) {
                    // Response too large — send as file attachment
                    msg_debug(&format!(
                        "[rolling_ph] FINAL FILE ATTACH: total={}",
                        full_response.len()
                    ));
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!(
                        "edit_message",
                        bot_owned
                            .edit_message_text(
                                chat_id,
                                placeholder_msg_id,
                                "\u{1f4c4} Response attached as file"
                            )
                            .await
                    );
                    send_response_as_file(
                        &bot_owned,
                        chat_id,
                        &final_response,
                        &state_owned,
                        "response",
                    )
                    .await;
                } else {
                    let normalized_remaining = normalize_empty_lines(remaining);
                    let html_remaining = markdown_to_telegram_html(&normalized_remaining);
                    msg_debug(&format!(
                        "[rolling_ph] FINAL EDIT placeholder: msg_id={}, html_len={}",
                        placeholder_msg_id,
                        html_remaining.len()
                    ));
                    if html_remaining.len() <= TELEGRAM_MSG_LIMIT {
                        if let Err(e) = tg!(
                            "edit_message",
                            bot_owned
                                .edit_message_text(chat_id, placeholder_msg_id, &html_remaining)
                                .parse_mode(ParseMode::Html)
                                .await
                        ) {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!(
                                "  [{ts}]   ⚠ edit_message failed (final HTML): {}",
                                redact_err(&e)
                            );
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!(
                                "edit_message",
                                bot_owned
                                    .edit_message_text(
                                        chat_id,
                                        placeholder_msg_id,
                                        &normalized_remaining
                                    )
                                    .await
                            );
                        }
                    } else {
                        let send_result = send_long_message(
                            &bot_owned,
                            chat_id,
                            &html_remaining,
                            Some(ParseMode::Html),
                            &state_owned,
                        )
                        .await;
                        match send_result {
                            Ok(_) => {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!(
                                    "delete_message",
                                    bot_owned.delete_message(chat_id, placeholder_msg_id).await
                                );
                            }
                            Err(_) => {
                                let fallback = send_long_message(
                                    &bot_owned,
                                    chat_id,
                                    &normalized_remaining,
                                    None,
                                    &state_owned,
                                )
                                .await;
                                match fallback {
                                    Ok(_) => {
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let _ = tg!(
                                            "delete_message",
                                            bot_owned
                                                .delete_message(chat_id, placeholder_msg_id)
                                                .await
                                        );
                                    }
                                    Err(_) => {
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let truncated =
                                            truncate_str(&normalized_remaining, TELEGRAM_MSG_LIMIT);
                                        let _ = tg!(
                                            "edit_message",
                                            bot_owned
                                                .edit_message_text(
                                                    chat_id,
                                                    placeholder_msg_id,
                                                    &truncated
                                                )
                                                .await
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                // Clean up leftover "Stopping..." message if /stop raced with normal completion
                if let Some(msg_id) = stop_msg_id {
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!(
                        "delete_message",
                        bot_owned.delete_message(chat_id, msg_id).await
                    );
                }

                // Update session state
                {
                    let mut data = state_owned.lock().await;
                    // Guard: if /model (provider switch) or /start (workspace switch) ran during
                    // this task, the session has been intentionally reset. Writing the old
                    // provider's session_id and the response into the (now-cleared) session would
                    // resurrect stale state under a new context, contradicting the user-facing
                    // "history has been reset" notice and breaking the next message's provider
                    // routing. Skip the session update — group chat log is still written below
                    // so other bots see the response.
                    //
                    // Use detect_provider (not provider_from_model) so the comparison matches how
                    // provider_str was captured at task spawn — otherwise, a chat with no model
                    // set running on a CLI fallback (e.g. Codex when Claude is unavailable) would
                    // see provider_str = "codex" but provider_now = "claude" and skip every
                    // writeback.
                    let model_now = data.settings.models.get(&chat_id.0.to_string()).cloned();
                    let provider_now = detect_provider(model_now.as_deref());
                    let path_now = data
                        .sessions
                        .get(&chat_id)
                        .and_then(|s| s.current_path.clone());
                    let sid_now = data
                        .sessions
                        .get(&chat_id)
                        .and_then(|s| s.session_id.clone());
                    let epoch_now = data.clear_epoch.get(&chat_id).copied().unwrap_or(0);
                    // sid comparison catches /clear (sid → None) and /start <session-id>
                    // same-path swaps (sid → different value). epoch_now catches /clear on
                    // a brand-new session where sid stays None on both sides.
                    let session_changed = provider_now != provider_str
                        || path_now.as_deref() != Some(current_path.as_str())
                        || sid_now != captured_sid
                        || epoch_now != captured_clear_epoch;
                    if session_changed {
                        msg_debug(&format!("[polling] session changed during task — skip session update (path: {:?} → {:?}, provider: {} → {}, sid: {:?} → {:?}, epoch: {} → {})",
                            current_path, path_now, provider_str, provider_now, captured_sid, sid_now, captured_clear_epoch, epoch_now));
                    } else if let Some(session) = data.sessions.get_mut(&chat_id) {
                        msg_debug(&format!("[polling] saving session: new_session_id={:?}, old_session_id={:?}, history_len={}",
                            new_session_id, session.session_id, session.history.len()));
                        if let Some(sid) = new_session_id.take() {
                            session.session_id = Some(sid);
                        }
                        session.history.push(HistoryItem {
                            item_type: HistoryType::User,
                            content: user_text_owned.clone(),
                        });
                        // Skip the assistant push when the AI produced no output.
                        // The UI now silently deletes the placeholder in that case
                        // (instead of editing it to "(No response)"), so persisting
                        // the sentinel string here would create a phantom assistant
                        // turn that the user never saw and the provider's JSONL
                        // does not contain.
                        if !was_originally_empty {
                            session.history.push(HistoryItem {
                                item_type: HistoryType::Assistant,
                                content: final_response,
                            });
                        }
                        save_session_to_file(session, &current_path, provider_str);
                        msg_debug(&format!(
                            "[polling] session saved: session_id={:?}, history_len={}",
                            session.session_id,
                            session.history.len()
                        ));
                    }
                    // Write to group chat shared log (for cross-bot context sharing)
                    msg_debug(&format!(
                        "[polling] JSONL check: chat_id={}, raw_entries_count={}",
                        chat_id.0,
                        raw_entries.len()
                    ));
                    if chat_id.0 < 0 {
                        let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                        let dn = if bot_display_name_for_log.is_empty() {
                            None
                        } else {
                            Some(bot_display_name_for_log.clone())
                        };
                        append_group_chat_log(
                            chat_id.0,
                            &GroupChatLogEntry {
                                ts: now_ts.clone(),
                                bot: bot_username_for_log.clone(),
                                bot_display_name: dn.clone(),
                                role: "user".to_string(),
                                from: Some(user_display_name_owned.clone()),
                                text: user_text_owned.clone(),
                                clear: false,
                            },
                        );
                        if !raw_entries.is_empty() {
                            msg_debug(&format!("[polling] JSONL: writing user+assistant entries, raw_entries_count={}", raw_entries.len()));
                            append_group_chat_log(
                                chat_id.0,
                                &GroupChatLogEntry {
                                    ts: now_ts,
                                    bot: bot_username_for_log.clone(),
                                    bot_display_name: dn,
                                    role: "assistant".to_string(),
                                    from: None,
                                    text: serialize_payload(&std::mem::take(&mut raw_entries)),
                                    clear: false,
                                },
                            );
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
                        let _ = tg!(
                            "send_message",
                            bot_owned.send_message(chat_id, &hook_msg).await
                        );
                    }
                }

                // === /loop verification: check completeness after each turn ===
                // Provider-specific mechanics (Claude forks live session with
                // --fork-session; Codex reads the full-fidelity archive and
                // dispatches an independent --ephemeral exec). The surrounding
                // loop-state / cancel / re-inject logic is shared.
                if !cancelled
                    && !cancel_token.cancelled.load(Ordering::Relaxed)
                    && (provider_str == "claude"
                        || provider_str == "codex"
                        || provider_str == "opencode")
                {
                    let loop_info = {
                        let data = state_owned.lock().await;
                        data.loop_states.get(&chat_id).map(|ls| {
                            let sid = data
                                .sessions
                                .get(&chat_id)
                                .and_then(|s| s.session_id.clone());
                            let cwd = data
                                .sessions
                                .get(&chat_id)
                                .and_then(|s| s.current_path.clone())
                                .unwrap_or_default();
                            let effort =
                                get_effort_for_provider(&data.settings, chat_id, provider_str);
                            let codex_fast = is_codex_fast(&data.settings, chat_id);
                            (
                                ls.remaining,
                                ls.max_iterations,
                                ls.original_request.clone(),
                                sid,
                                cwd,
                                effort,
                                codex_fast,
                            )
                        })
                    };
                    if let Some((
                        remaining,
                        max_iterations,
                        original_request,
                        sid,
                        cwd,
                        effort_for_verify,
                        codex_fast_for_verify,
                    )) = loop_info
                    {
                        if let Some(session_id) = sid {
                            msg_debug(&format!(
                                "[loop] verifying ({}): session_id={}, remaining={}, request={:?}",
                                provider_str,
                                session_id,
                                remaining,
                                truncate_str(&original_request, 60)
                            ));
                            // Show spinner during verification. Alternating
                            // magnifying-glass frames (🔍/🔎) typed one letter
                            // at a time give a lightweight "scanning" feel
                            // while the verifier runs. Animation runs in a
                            // background task so the main path is not blocked
                            // by frame edits; it is stopped once verify
                            // returns, before the message is deleted.
                            const VERIFY_SPINNER: &[&str] = &[
                                "🔍 V",
                                "🔎 Ve",
                                "🔍 Ver",
                                "🔎 Veri",
                                "🔍 Verif",
                                "🔎 Verify",
                                "🔍 Verifyi",
                                "🔎 Verifyin",
                                "🔍 Verifying",
                                "🔎 Verifying.",
                                "🔍 Verifying..",
                                "🔎 Verifying...",
                            ];
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let verify_msg = tg!(
                                "send_message",
                                bot_owned.send_message(chat_id, VERIFY_SPINNER[0]).await
                            );
                            let verify_msg_id = verify_msg.ok().map(|m| m.id);

                            // Start background frame-edit task
                            let verify_anim_stop = Arc::new(AtomicBool::new(false));
                            let verify_anim_handle = if let Some(msg_id) = verify_msg_id {
                                let bot_anim = bot_owned.clone();
                                let state_anim = state_owned.clone();
                                let stop_flag = verify_anim_stop.clone();
                                let request_tasks = {
                                    let data = state_owned.lock().await;
                                    data.request_tasks.clone()
                                };
                                Some(spawn_tracked_request_task(request_tasks, async move {
                                    let mut idx: usize = 1;
                                    let mut last_text = VERIFY_SPINNER[0].to_string();
                                    while !stop_flag.load(Ordering::Relaxed) {
                                        tokio::time::sleep(std::time::Duration::from_millis(400))
                                            .await;
                                        if stop_flag.load(Ordering::Relaxed) {
                                            break;
                                        }
                                        let frame = VERIFY_SPINNER[idx % VERIFY_SPINNER.len()];
                                        idx += 1;
                                        if frame != last_text {
                                            shared_rate_limit_wait(&state_anim, chat_id).await;
                                            if stop_flag.load(Ordering::Relaxed) {
                                                break;
                                            }
                                            let _ = tg!(
                                                "edit_message",
                                                &state_anim,
                                                chat_id,
                                                bot_anim
                                                    .edit_message_text(chat_id, msg_id, frame)
                                                    .await
                                            );
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
                                let request_tasks = {
                                    let data = state_owned.lock().await;
                                    data.request_tasks.clone()
                                };
                                spawn_tracked_blocking_task(request_tasks, move || {
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
                            let request_tasks = {
                                let data = state_owned.lock().await;
                                data.request_tasks.clone()
                            };
                            let verify_result =
                                spawn_tracked_blocking_result(request_tasks, move || {
                                    match provider_for_verify {
                                        "codex" => crate::services::codex::verify_completion_codex(
                                            &sid_clone,
                                            &cwd_clone,
                                            effort_for_verify.as_deref(),
                                            codex_fast_for_verify,
                                        ),
                                        "opencode" => {
                                            crate::services::opencode::verify_completion_opencode(
                                                &sid_clone, &cwd_clone,
                                            )
                                        }
                                        _ => crate::services::claude::verify_completion(
                                            &sid_clone,
                                            &cwd_clone,
                                            effort_for_verify.as_deref(),
                                        ),
                                    }
                                })
                                .await;
                            // Stop animation and await its task so no stray
                            // edit lands after the delete below.
                            verify_anim_stop.store(true, Ordering::Relaxed);
                            if let Some(h) = verify_anim_handle {
                                let _ = h.await;
                            }
                            // Remove spinner
                            if let Some(msg_id) = verify_msg_id {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!(
                                    "delete_message",
                                    bot_owned.delete_message(chat_id, msg_id).await
                                );
                            }
                            // Re-check: /stop may have been called during verification.
                            // /clear or /model (provider switch) may have removed loop_states
                            // during verification. Suppress all post-verify outcome messages
                            // (complete / limit / incomplete) so the user's explicit cancel is
                            // honoured uniformly. Each inner branch (complete/limit/reinject)
                            // additionally re-checks loop_states under the lock as a race-safety
                            // net for /clear or /model arriving between this gate and the
                            // branch-internal write.
                            let loop_active_post_verify = {
                                let data = state_owned.lock().await;
                                data.loop_states.contains_key(&chat_id)
                            };
                            if cancel_token.cancelled.load(Ordering::Relaxed) {
                                msg_debug("[loop] cancelled during verification — aborting loop");
                                // loop_states already removed by /stop handler
                            } else if !loop_active_post_verify {
                                msg_debug("[loop] loop_states removed during verification — skip post-verify outcome");
                            } else {
                                match verify_result {
                                    Ok(Some(Ok(result))) => {
                                        if result.complete {
                                            msg_debug("[loop] mission_complete — loop finished");
                                            // Re-check loop_states under the lock: /clear or /model
                                            // (provider switch) may have removed it between the outer
                                            // gate and now. Honour that intent — skip the outcome
                                            // message uniformly with the reinject branch's safety net.
                                            let still_active = {
                                                let mut data = state_owned.lock().await;
                                                data.loop_states.remove(&chat_id).is_some()
                                            };
                                            if still_active {
                                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                                let _ = tg!("send_message", bot_owned.send_message(chat_id, "✅ Loop complete — task verified as done.").await);
                                            } else {
                                                msg_debug("[loop] loop_states removed before complete-message — skip");
                                            }
                                        } else if max_iterations > 0 && remaining <= 1 {
                                            msg_debug(
                                                "[loop] max iterations reached — loop stopped",
                                            );
                                            // Same race-safety net as the complete branch.
                                            let still_active = {
                                                let mut data = state_owned.lock().await;
                                                data.loop_states.remove(&chat_id).is_some()
                                            };
                                            if still_active {
                                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                                let feedback_preview = result
                                                    .feedback
                                                    .as_deref()
                                                    .unwrap_or("(no details)");
                                                let msg = format!(
                                                    "⚠️ Loop limit reached. Remaining issue:\n{}",
                                                    feedback_preview
                                                );
                                                let _ = tg!(
                                                    "send_message",
                                                    bot_owned.send_message(chat_id, &msg).await
                                                );
                                            } else {
                                                msg_debug("[loop] loop_states removed before limit-message — skip");
                                            }
                                        } else {
                                            // Incomplete: use feedback if available, otherwise fall back to original request
                                            let reinject_text = result.feedback.unwrap_or_else(|| {
                                            format!("Continue the previous task. The original request was: {}", original_request)
                                        });
                                            // For unlimited (max_iterations==0): don't decrement, track iteration count separately
                                            let (new_remaining, iteration) = if max_iterations == 0
                                            {
                                                // remaining counts UP from 0 for display; never decremented to trigger limit
                                                (
                                                    remaining.saturating_add(1),
                                                    remaining.saturating_add(1),
                                                )
                                            } else {
                                                let nr = remaining - 1;
                                                (nr, max_iterations - nr)
                                            };
                                            msg_debug(&format!("[loop] incomplete — re-requesting (remaining={}): {:?}",
                                            new_remaining, truncate_str(&reinject_text, 100)));
                                            let iter_label = if max_iterations == 0 {
                                                format!(
                                                    "🔄 Loop iteration {}\n{}",
                                                    iteration, reinject_text
                                                )
                                            } else {
                                                format!(
                                                    "🔄 Loop iteration {}/{}\n{}",
                                                    iteration, max_iterations, reinject_text
                                                )
                                            };
                                            shared_rate_limit_wait(&state_owned, chat_id).await;
                                            let _ = tg!(
                                                "send_message",
                                                bot_owned.send_message(chat_id, &iter_label).await
                                            );
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
                                                } else if !data.loop_states.contains_key(&chat_id) {
                                                    // loop_states cleared externally (e.g. /clear, /model provider switch)
                                                    // between verify start and finish — honour that intent and skip reinject
                                                    // so the loop does not silently resume.
                                                    msg_debug("[loop] loop_states removed during verification — skip reinject");
                                                } else {
                                                    if let Some(ls) =
                                                        data.loop_states.get_mut(&chat_id)
                                                    {
                                                        ls.remaining = new_remaining;
                                                    }
                                                    data.loop_feedback.insert(
                                                        chat_id,
                                                        (
                                                            reinject_text,
                                                            user_display_name_owned.clone(),
                                                        ),
                                                    );
                                                    loop_reinjected = true;
                                                }
                                            }
                                        }
                                    }
                                    Ok(Some(Err(e))) => {
                                        msg_debug(&format!(
                                            "[loop] verify_completion error: {}",
                                            e
                                        ));
                                        {
                                            let mut data = state_owned.lock().await;
                                            data.loop_states.remove(&chat_id);
                                        }
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let _ = tg!(
                                            "send_message",
                                            bot_owned
                                                .send_message(
                                                    chat_id,
                                                    &format!("⚠️ Loop verification failed: {}", e)
                                                )
                                                .await
                                        );
                                    }
                                    Ok(None) => {
                                        msg_debug("[loop] verify task aborted before start");
                                        {
                                            let mut data = state_owned.lock().await;
                                            data.loop_states.remove(&chat_id);
                                        }
                                    }
                                    Err(e) => {
                                        msg_debug(&format!("[loop] spawn_blocking error: {}", e));
                                        {
                                            let mut data = state_owned.lock().await;
                                            data.loop_states.remove(&chat_id);
                                        }
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let _ = tg!(
                                            "send_message",
                                            bot_owned
                                                .send_message(
                                                    chat_id,
                                                    &format!("⚠️ Loop verification failed: {}", e)
                                                )
                                                .await
                                        );
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
            // Re-send SIGKILL as a safety belt (cancel_now is idempotent).
            cancel_token.cancel_now();

            // stopped_response (full) is used for session history
            let stopped_response = if full_response.trim().is_empty() {
                "[Stopped]".to_string()
            } else {
                let normalized = normalize_empty_lines(&full_response);
                format!("{}\n\n[Stopped]", normalized)
            };

            shared_rate_limit_wait(&state_owned, chat_id).await;

            // ── Show only remaining delta + [Stopped] (unified rolling placeholder) ──
            if full_response.len() < last_confirmed_len
                || !full_response.is_char_boundary(last_confirmed_len)
            {
                last_confirmed_len = 0;
            }
            let remaining = &full_response[last_confirmed_len..];
            msg_debug(&format!(
                "[rolling_ph] STOPPED: placeholder_msg_id={}, confirmed={}, remaining_len={}",
                placeholder_msg_id,
                last_confirmed_len,
                remaining.trim().len()
            ));
            if should_attach_response_as_file(full_response.len(), provider_str) {
                // Large stopped response — send as file
                msg_debug(&format!(
                    "[rolling_ph] STOPPED FILE ATTACH: total={}",
                    full_response.len()
                ));
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!(
                    "edit_message",
                    bot_owned
                        .edit_message_text(
                            chat_id,
                            placeholder_msg_id,
                            "\u{1f4c4} Response attached as file [Stopped]"
                        )
                        .await
                );
                send_response_as_file(
                    &bot_owned,
                    chat_id,
                    &stopped_response,
                    &state_owned,
                    "response",
                )
                .await;
            } else {
                let display_stopped = if remaining.trim().is_empty() {
                    "[Stopped]".to_string()
                } else {
                    let normalized = normalize_empty_lines(remaining);
                    format!("{}\n\n[Stopped]", normalized)
                };
                let html_stopped = markdown_to_telegram_html(&display_stopped);
                if html_stopped.len() <= TELEGRAM_MSG_LIMIT {
                    if let Err(e) = tg!(
                        "edit_message",
                        bot_owned
                            .edit_message_text(chat_id, placeholder_msg_id, &html_stopped)
                            .parse_mode(ParseMode::Html)
                            .await
                    ) {
                        let ts_err = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts_err}]   ⚠ edit_message failed (stopped HTML): {}",
                            redact_err(&e)
                        );
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!(
                            "edit_message",
                            bot_owned
                                .edit_message_text(chat_id, placeholder_msg_id, &display_stopped)
                                .await
                        );
                    }
                } else {
                    let send_result = send_long_message(
                        &bot_owned,
                        chat_id,
                        &html_stopped,
                        Some(ParseMode::Html),
                        &state_owned,
                    )
                    .await;
                    match send_result {
                        Ok(_) => {
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!(
                                "delete_message",
                                bot_owned.delete_message(chat_id, placeholder_msg_id).await
                            );
                        }
                        Err(_) => {
                            let fallback = send_long_message(
                                &bot_owned,
                                chat_id,
                                &display_stopped,
                                None,
                                &state_owned,
                            )
                            .await;
                            match fallback {
                                Ok(_) => {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!(
                                        "delete_message",
                                        bot_owned.delete_message(chat_id, placeholder_msg_id).await
                                    );
                                }
                                Err(_) => {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let truncated =
                                        truncate_str(&display_stopped, TELEGRAM_MSG_LIMIT);
                                    let _ = tg!(
                                        "edit_message",
                                        bot_owned
                                            .edit_message_text(
                                                chat_id,
                                                placeholder_msg_id,
                                                &truncated
                                            )
                                            .await
                                    );
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
                let _ = tg!(
                    "delete_message",
                    bot_owned.delete_message(chat_id, msg_id).await
                );
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Stopped");

            let mut data = state_owned.lock().await;
            // Same guard as the normal-completion branch: if /model or /start changed the
            // session during this task, skip the partial-response writeback so it does not
            // bleed into the new context. Group chat log is still written below — other bots
            // benefit from seeing the partial response, and the shared log is not part of the
            // (now-cleared) session it would have leaked into.
            //
            // detect_provider mirrors the spawn-time capture (see normal-completion guard).
            // sid + clear_epoch mirror normal-completion: catch /clear, /start same-path swaps,
            // and brand-new-session /clear (where sid stays None).
            let model_now = data.settings.models.get(&chat_id.0.to_string()).cloned();
            let provider_now = detect_provider(model_now.as_deref());
            let path_now = data
                .sessions
                .get(&chat_id)
                .and_then(|s| s.current_path.clone());
            let sid_now = data
                .sessions
                .get(&chat_id)
                .and_then(|s| s.session_id.clone());
            let epoch_now = data.clear_epoch.get(&chat_id).copied().unwrap_or(0);
            let session_changed = provider_now != provider_str
                || path_now.as_deref() != Some(current_path.as_str())
                || sid_now != captured_sid
                || epoch_now != captured_clear_epoch;
            if session_changed {
                msg_debug(&format!("[polling] session changed during stopped task — skip session writeback (path: {:?} → {:?}, provider: {} → {}, sid: {:?} → {:?}, epoch: {} → {})",
                    current_path, path_now, provider_str, provider_now, captured_sid, sid_now, captured_clear_epoch, epoch_now));
            } else if let Some(session) = data.sessions.get_mut(&chat_id) {
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
            }
            // Write to group chat shared log (for cross-bot context sharing).
            // Mirrors the normal-completion branch: written regardless of session_changed.
            msg_debug(&format!(
                "[polling] JSONL stopped check: chat_id={}, raw_entries_count={}",
                chat_id.0,
                raw_entries.len()
            ));
            if chat_id.0 < 0 {
                let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                let dn = if bot_display_name_for_log.is_empty() {
                    None
                } else {
                    Some(bot_display_name_for_log.clone())
                };
                append_group_chat_log(
                    chat_id.0,
                    &GroupChatLogEntry {
                        ts: now_ts.clone(),
                        bot: bot_username_for_log.clone(),
                        bot_display_name: dn.clone(),
                        role: "user".to_string(),
                        from: Some(user_display_name_owned.clone()),
                        text: user_text_owned,
                        clear: false,
                    },
                );
                if !raw_entries.is_empty() {
                    msg_debug(&format!("[polling] JSONL stopped: writing user+assistant entries, raw_entries_count={}", raw_entries.len()));
                    append_group_chat_log(
                        chat_id.0,
                        &GroupChatLogEntry {
                            ts: now_ts,
                            bot: bot_username_for_log.clone(),
                            bot_display_name: dn,
                            role: "assistant".to_string(),
                            from: None,
                            text: serialize_payload(&std::mem::take(&mut raw_entries)),
                            clear: false,
                        },
                    );
                } else {
                    msg_debug(&format!("[polling] JSONL stopped: user entry written, assistant SKIPPED (raw_entries is empty)"));
                }
            }
            remove_cancel_token_locked(&mut data, chat_id);
            data.stop_message_ids.remove(&chat_id);
            drop(data);
            msg_debug(&format!(
                "[queue:trigger] chat_id={}, source=text_poll_cancelled",
                chat_id.0
            ));
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
                remove_cancel_token_locked(&mut data, chat_id);
                data.stop_message_ids.remove(&chat_id)
            };
            if let Some(msg_id) = orphan_stop_msg {
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!(
                    "delete_message",
                    bot_owned.delete_message(chat_id, msg_id).await
                );
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
            remove_cancel_token_locked(&mut data, chat_id);
            msg_id
        };
        if let Some(msg_id) = orphan_stop_msg {
            shared_rate_limit_wait(&state_owned, chat_id).await;
            let _ = tg!(
                "delete_message",
                bot_owned.delete_message(chat_id, msg_id).await
            );
        }
        msg_debug(&format!(
            "[queue:trigger] chat_id={}, source=text_poll_completed",
            chat_id.0
        ));
        drop(_group_lock); // release group chat lock before processing queue
        process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
    });

    Ok(())
}

/// Load existing session from ai_sessions directory matching the given path and provider
fn load_existing_session(
    current_path: &str,
    provider: &str,
) -> Option<(SessionData, std::time::SystemTime)> {
    msg_debug(&format!(
        "[load_session] looking for path={:?}, provider={}",
        current_path, provider
    ));
    let sessions_dir = ai_screen::ai_sessions_dir()?;

    if !sessions_dir.exists() {
        msg_debug(&format!(
            "[load_session] sessions_dir not found: {}",
            sessions_dir.display()
        ));
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
                            if !session_data.provider.is_empty()
                                && session_data.provider != provider
                            {
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

    msg_debug(&format!(
        "[load_session] scanned {} json files, result={}",
        file_count,
        if matching_session.is_some() {
            "found"
        } else {
            "None"
        }
    ));
    if matching_session.is_none() && file_count > 0 {
        if let Some(sample) = path_mismatch_sample {
            msg_debug(&format!(
                "[load_session] path mismatch example: stored={:?} vs wanted={:?}",
                sample, current_path
            ));
        }
    }

    matching_session
}

/// Remove stale session files without session_id for the same current_path + provider.
/// Called when no file with session_id exists for this path+provider.
/// Keeps the most recently modified empty file (the one selected by load_existing_session)
/// and deletes the rest.
fn cleanup_session_files(current_path: &str, provider: &str) {
    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(&sessions_dir) else {
        return;
    };

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
                        let modified = path
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        empty_files.push((path, modified));
                    }
                }
            }
        }
    }

    if empty_files.len() <= 1 {
        return;
    }

    // Find the latest modified one to keep
    let latest_idx = empty_files
        .iter()
        .enumerate()
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
    let saveable_history: Vec<HistoryItem> = session
        .history
        .iter()
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
    msg_debug(&format!(
        "[save_session] provider={}, session_id={}, path={}",
        provider, session_id, current_path
    ));

    // Security: whitelist session_id to alphanumeric, hyphens, underscores only
    if !session_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
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
            if p == file_path {
                continue;
            }
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
        match tg!(
            "send_document",
            bot.send_document(chat_id, teloxide::types::InputFile::file(&path),)
                .await
        ) {
            Ok(_) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}]   📤 Upload sent: {}", file_path);
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}]   ⚠ Upload failed: {}", redact_err(&e));
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
        let last = data
            .api_timestamps
            .entry(chat_id)
            .or_insert_with(|| tokio::time::Instant::now() - tokio::time::Duration::from_secs(10));
        let earliest_next = *last + min_gap;
        let now = tokio::time::Instant::now();
        let target = if earliest_next > now {
            earliest_next
        } else {
            now
        };
        *last = target; // Reserve this slot
        target
    }; // Mutex released here
    tokio::time::sleep_until(sleep_until).await;
}

/// Honor a Telegram-server-mandated `RetryAfter` by pushing the next safe
/// call time for `chat_id` forward by the requested duration. Subsequent
/// `shared_rate_limit_wait` calls for the same chat will then naturally
/// wait the full server-mandated cooldown before allowing another call.
///
/// Telegram explicitly instructs clients to respect `RetryAfter`; ignoring
/// it causes the cooldown to escalate (production logs have shown bans
/// accumulating to ~14000s after repeated violations).
async fn honor_telegram_retry_after<T>(
    state: &SharedState,
    chat_id: ChatId,
    result: &Result<T, teloxide::RequestError>,
) {
    if let Err(teloxide::RequestError::RetryAfter(seconds)) = result {
        let until = tokio::time::Instant::now() + seconds.duration();
        let mut data = state.lock().await;
        let entry = data.api_timestamps.entry(chat_id).or_insert(until);
        if *entry < until {
            *entry = until;
        }
    }
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

        // Find a safe UTF-8 char boundary, then find a newline before it.
        // Only split at a newline if it produces a non-empty chunk; otherwise
        // use the full UTF-8-safe boundary (a leading '\n' would yield an
        // empty raw_chunk, which Telegram rejects with "text must be non-empty").
        let safe_end = floor_char_boundary(remaining, effective_limit);
        let split_at = match remaining[..safe_end].rfind('\n') {
            Some(pos) if pos > 0 => pos,
            _ => safe_end,
        };

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
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let tmp_dir = home.join(".cokacdir").join("tmp");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let tmp_path = tmp_dir.join(format!("cokacdir_{}_{}.txt", label, timestamp));
    if std::fs::write(&tmp_path, response).is_err() {
        return false;
    }
    shared_rate_limit_wait(state, chat_id).await;
    let result = tg!(
        "send_document",
        bot.send_document(chat_id, teloxide::types::InputFile::file(&tmp_path),)
            .await
    );
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
    // Prefer cutting at a newline, but only if it leaves non-empty content.
    // Otherwise fall back to the full UTF-8-safe truncation.
    if let Some(pos) = truncated.rfind('\n') {
        if pos > 0 {
            return truncated[..pos].to_string();
        }
    }
    truncated.to_string()
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
            result.push_str(&format!(
                "• {}",
                convert_inline(&html_escape(&trimmed[2..]))
            ));
            result.push('\n');
            i += 1;
            continue;
        }
        if trimmed.starts_with("* ") && !trimmed.starts_with("**") {
            result.push_str(&format!(
                "• {}",
                convert_inline(&html_escape(&trimmed[2..]))
            ));
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
    if name != "Bash" {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        return false;
    };
    let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let expected = crate::bin_path().rsplit(['/', '\\']).next().unwrap_or("");
    if expected.is_empty() {
        return false;
    }
    // Strip surrounding quotes from each token, then compare basename
    cmd.split_whitespace().any(|tok| {
        let unquoted = tok.trim_matches(|c| c == '"' || c == '\'');
        let basename = unquoted.rsplit(['/', '\\']).next().unwrap_or("");
        basename == expected
    })
}

/// Check if a Bash tool call contains --read_chat_log (result should be suppressed from display).
fn detect_chat_log_read(name: &str, input: &str) -> bool {
    if name != "Bash" {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        return false;
    };
    let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
    cmd.contains("--read_chat_log")
}

/// Read the most recent .result file from schedule dir and delete it
fn read_latest_cron_result() -> Option<String> {
    let dir = schedule_dir()?;
    let mut results: Vec<_> = fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "result")
                .unwrap_or(false)
        })
        .collect();
    results.sort_by_key(|e| {
        std::cmp::Reverse(
            e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        )
    });
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
            // Fallback: some backends wrap the JSON output with
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
        let msg = v
            .get("message")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown error");
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
                    let schedule_type = s
                        .get("schedule_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let once = s.get("once").and_then(|b| b.as_bool()).unwrap_or(false);
                    let kind = match schedule_type {
                        "absolute" => "1회",
                        "cron" if once => "1회 cron",
                        "cron" => "반복",
                        _ => {
                            if schedule.split_whitespace().count() == 5 {
                                "반복"
                            } else {
                                "1회"
                            }
                        }
                    };
                    let prompt_preview = if prompt.chars().count() > 40 {
                        format!("{}...", prompt.chars().take(40).collect::<String>())
                    } else {
                        prompt.to_string()
                    };
                    lines.push(format!(
                        "\n{}. [{}] {}\n   🕐 `{}`\n   🔖 {}",
                        i + 1,
                        kind,
                        prompt_preview,
                        schedule,
                        id
                    ));
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
        let schedule_type = v
            .get("schedule_type")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let once = v.get("once").and_then(|b| b.as_bool()).unwrap_or(false);
        let kind = match schedule_type {
            "absolute" => "1회",
            "cron" if once => "1회 cron",
            "cron" => "반복",
            _ => {
                if schedule.split_whitespace().count() == 5 {
                    "반복"
                } else {
                    "1회"
                }
            }
        };
        format!(
            "✅ Scheduled [{}]\n🔖 {}\n📝 {}\n🕐 `{}`",
            kind, id, prompt, schedule
        )
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
                let summary: Vec<String> = changes
                    .iter()
                    .map(|c| {
                        let path = c.get("path").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("update");
                        format!("{}: {}", kind, path)
                    })
                    .collect();
                return format!("\u{1F4DD} {}", summary.join(", "));
            }
        }
        return format!("\u{1F4DD} {}", input);
    }

    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        // Non-JSON input — Codex tools produce human-readable strings directly
        return match name {
            "WebSearch" => {
                if input.is_empty() {
                    "Search".to_string()
                } else {
                    format!("Search: {}", input)
                }
            }
            n if n.starts_with("Collab:") => {
                let tool = n.strip_prefix("Collab:").unwrap_or(n);
                if input.is_empty() {
                    format!("Agent: {}", tool)
                } else {
                    format!("Agent {}: {}", tool, input)
                }
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
                let replace_all = v
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
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
            let nb_path = v
                .get("notebook_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
            let subagent_type = v
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
                let pending = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("pending"))
                    .count();
                let in_progress = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("in_progress"))
                    .count();
                let completed = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("completed"))
                    .count();
                format!(
                    "Todo: {} pending, {} in progress, {} completed",
                    pending, in_progress, completed
                )
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
        "ExitPlanMode" => "Exit plan mode".to_string(),
        "EnterPlanMode" => "Enter plan mode".to_string(),
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
        "TaskList" => "List tasks".to_string(),
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
    sched_debug(&format!(
        "[should_trigger] id={}, type={}, schedule={}, now={}, last_run={:?}",
        entry.id,
        entry.schedule_type,
        entry.schedule,
        now.format("%Y-%m-%d %H:%M:%S"),
        entry.last_run
    ));
    match entry.schedule_type.as_str() {
        "absolute" => {
            let Ok(schedule_time) =
                chrono::NaiveDateTime::parse_from_str(&entry.schedule, "%Y-%m-%d %H:%M:%S")
            else {
                sched_debug(&format!(
                    "[should_trigger] id={}, parse failed → false",
                    entry.id
                ));
                return false;
            };
            let schedule_dt = schedule_time.and_local_timezone(chrono::Local).single();
            let Some(schedule_dt) = schedule_dt else {
                sched_debug(&format!(
                    "[should_trigger] id={}, timezone conversion failed → false",
                    entry.id
                ));
                return false;
            };
            if now < schedule_dt {
                sched_debug(&format!(
                    "[should_trigger] id={}, not yet (now < schedule_dt) → false",
                    entry.id
                ));
                return false;
            }
            // Already ran? An unparseable / timezone-ambiguous `last_run`
            // is treated as "already ran" defensively — silently re-firing
            // a one-shot absolute schedule because of a corrupted timestamp
            // is far worse than not firing at all (the user will notice and
            // fix). Corrupted/legacy entries thus stay dormant.
            if let Some(ref last) = entry.last_run {
                match chrono::NaiveDateTime::parse_from_str(last, "%Y-%m-%d %H:%M:%S") {
                    Ok(last_dt) => match last_dt.and_local_timezone(chrono::Local).single() {
                        Some(last_local) => {
                            if last_local >= schedule_dt {
                                sched_debug(&format!("[should_trigger] id={}, already ran (last={} >= sched={}) → false",
                                    entry.id, last_local.format("%H:%M:%S"), schedule_dt.format("%H:%M:%S")));
                                return false;
                            }
                        }
                        None => {
                            sched_debug(&format!("[should_trigger] id={}, last_run timezone-ambiguous {:?} → defensively false", entry.id, last));
                            return false;
                        }
                    },
                    Err(e) => {
                        sched_debug(&format!("[should_trigger] id={}, last_run parse failed {:?}: {} → defensively false", entry.id, last, e));
                        return false;
                    }
                }
            }
            sched_debug(&format!(
                "[should_trigger] id={}, absolute ready → true",
                entry.id
            ));
            true
        }
        "cron" => {
            if !cron_matches(&entry.schedule, now) {
                sched_debug(&format!(
                    "[should_trigger] id={}, cron not matching → false",
                    entry.id
                ));
                return false;
            }
            // Check last_run to avoid duplicate triggers within the same
            // minute. An unparseable / timezone-ambiguous `last_run` is
            // treated as "ran this minute" defensively so a corrupted
            // timestamp can't cause repeated firing on every 5s scheduler
            // tick. The schedule stays dormant until the next legitimate
            // write rewrites `last_run`.
            if let Some(ref last) = entry.last_run {
                match chrono::NaiveDateTime::parse_from_str(last, "%Y-%m-%d %H:%M:%S") {
                    Ok(last_dt) => {
                        match last_dt.and_local_timezone(chrono::Local).single() {
                            Some(last_local) => {
                                let now_min = now.format("%Y-%m-%d %H:%M").to_string();
                                let last_min = last_local.format("%Y-%m-%d %H:%M").to_string();
                                if now_min == last_min {
                                    sched_debug(&format!("[should_trigger] id={}, already ran this minute ({}) → false", entry.id, now_min));
                                    return false;
                                }
                            }
                            None => {
                                sched_debug(&format!("[should_trigger] id={}, last_run timezone-ambiguous {:?} → defensively false", entry.id, last));
                                return false;
                            }
                        }
                    }
                    Err(e) => {
                        sched_debug(&format!("[should_trigger] id={}, last_run parse failed {:?}: {} → defensively false", entry.id, last, e));
                        return false;
                    }
                }
            }
            sched_debug(&format!(
                "[should_trigger] id={}, cron matched → true",
                entry.id
            ));
            true
        }
        _ => {
            sched_debug(&format!(
                "[should_trigger] id={}, unknown type={} → false",
                entry.id, entry.schedule_type
            ));
            false
        }
    }
}

/// Update schedule entry after a run: set last_run, delete if once
fn update_schedule_after_run(entry: &ScheduleEntry, new_context_summary: Option<String>) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    sched_debug(&format!(
        "[update_schedule_after_run] id={}, type={}, once={:?}, now={}, has_new_context={}",
        entry.id,
        entry.schedule_type,
        entry.once,
        now,
        new_context_summary.is_some()
    ));

    // 실행 중 사용자가 삭제한 경우 부활 방지
    let dir = match schedule_dir() {
        Some(d) => d,
        None => {
            sched_debug(&format!(
                "[update_schedule_after_run] id={}, no schedule dir → skip",
                entry.id
            ));
            return;
        }
    };
    let path = dir.join(format!("{}.json", entry.id));
    if !path.exists() {
        sched_debug(&format!(
            "[update_schedule_after_run] id={}, file already deleted → skip (no resurrection)",
            entry.id
        ));
        return; // 이미 삭제됨 - write하지 않음
    }

    // One-time schedules (absolute / cron --once) are already deleted before execution,
    // so this function only handles recurring cron updates.
    sched_debug(&format!(
        "[update_schedule_after_run] id={}, cron recurring → update last_run",
        entry.id
    ));
    let mut updated = entry.clone();
    updated.last_run = Some(now);
    if new_context_summary.is_some() {
        updated.context_summary = new_context_summary;
    }
    if let Err(e) = write_schedule_entry(&updated) {
        sched_debug(&format!(
            "[update_schedule_after_run] id={}, write failed: {}",
            entry.id, e
        ));
        eprintln!("[Schedule] Failed to update entry {}: {}", entry.id, e);
    } else {
        sched_debug(&format!(
            "[update_schedule_after_run] id={}, updated successfully",
            entry.id
        ));
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
    inline_mode: bool,
) {
    sched_debug(&format!("[execute_schedule] START id={}, chat_id={}, prompt={:?}, has_context={}, has_prev_session={}, inline_mode={}",
        entry.id, chat_id, truncate_str(&entry.prompt, 60), entry.context_summary.is_some(), prev_session.is_some(), inline_mode));

    // Inline mode: capture the chat's current session_id, current_path, and the
    // chat's clear_epoch under a single lock so we can (a) route the provider
    // call into that exact session, and (b) detect at cleanup time whether the
    // user mutated the chat's session mid-run (via /clear, /start <other-path>,
    // /start <other-sid>, or model change). scheduler_cycle already verified
    // the session exists with a current_path before dispatching an inline run;
    // if it disappeared in between we still respect inline_mode and fall back
    // to entry.current_path with no session_id, which is the same shape
    // isolated mode would have used. clear_epoch defaults to 0 (a /clear
    // bumps it, matching the post-completion guard in handle_text_message).
    let (inline_session_id, inline_path, inline_clear_epoch): (
        Option<String>,
        Option<String>,
        u64,
    ) = if inline_mode {
        let data = state.lock().await;
        let s = data.sessions.get(&chat_id);
        let epoch = data.clear_epoch.get(&chat_id).copied().unwrap_or(0);
        (
            s.and_then(|s| s.session_id.clone()),
            s.and_then(|s| s.current_path.clone()),
            epoch,
        )
    } else {
        (None, None, 0)
    };

    // Acquire group chat lock (serializes processing across bots in the same group chat)
    let group_lock = acquire_group_chat_lock(chat_id.0).await;

    // Check if cancelled during lock wait
    let cancelled_during_wait = {
        let data = state.lock().await;
        data.cancel_tokens
            .get(&chat_id)
            .map(|ct| ct.cancelled.load(Ordering::Relaxed))
            .unwrap_or(false)
    };
    if cancelled_during_wait {
        sched_debug(&format!(
            "[execute_schedule] cancelled during lock wait, id={}",
            entry.id
        ));
        {
            let mut data = state.lock().await;
            if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                set.remove(&entry.id);
            }
            remove_cancel_token_locked(&mut data, chat_id);
            // Inline mode never replaced the session, so leave it untouched.
            if !inline_mode {
                if let Some(prev) = prev_session {
                    data.sessions.insert(chat_id, prev);
                } else {
                    data.sessions.remove(&chat_id);
                }
            }
        }
        msg_debug(&format!(
            "[queue:trigger] chat_id={}, source=schedule_cancelled_during_lock",
            chat_id.0
        ));
        drop(group_lock);
        process_next_queued_message(bot, chat_id, state).await;
        return;
    }

    // Build prompt with context summary if available.
    // Inline mode runs in the chat's current session, so context_summary (which
    // exists to bridge the isolated-session gap) would just duplicate context
    // the model already has — skip injection.
    let user_prompt = entry.prompt.clone();
    let prompt = match (&entry.context_summary, inline_mode) {
        (Some(summary), false) => {
            sched_debug(&format!(
                "[execute_schedule] id={}, injecting context summary ({} chars)",
                entry.id,
                summary.len()
            ));
            format!(
                "[이전 작업 맥락]\n{}\n\n[작업 지시]\n{}",
                summary, user_prompt
            )
        }
        _ => user_prompt.clone(),
    };
    let project_path = crate::utils::format::to_shell_path(&entry.current_path);
    let schedule_id = entry.id.clone();

    // Delete schedule files before execution for one-time schedules (absolute / cron --once)
    if entry.once.unwrap_or(false) || entry.schedule_type == "absolute" {
        sched_debug(&format!(
            "[execute_schedule] id={}, one-time → deleting schedule files before execution",
            schedule_id
        ));
        delete_schedule_entry(&schedule_id);
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ⏰ Schedule Starting: {user_prompt}");

    // Resolve the working directory. Inline mode runs in the chat's current path
    // (no isolated workspace), so we skip the create_dir_all entirely. Isolated
    // mode creates ~/.cokacdir/workspace/<schedule_id> as before.
    let workspace_path: String = if inline_mode {
        let path = inline_path
            .clone()
            .unwrap_or_else(|| entry.current_path.clone());
        sched_debug(&format!(
            "[execute_schedule] id={}, inline mode → working dir={}",
            schedule_id, path
        ));
        path
    } else {
        let Some(home) = dirs::home_dir() else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ⚠ [Schedule] Failed to get home directory");
            {
                let mut data = state.lock().await;
                if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                    set.remove(&schedule_id);
                }
                remove_cancel_token_locked(&mut data, chat_id);
                if let Some(prev) = prev_session {
                    data.sessions.insert(chat_id, prev);
                } else {
                    data.sessions.remove(&chat_id);
                }
            }
            msg_debug(&format!(
                "[queue:trigger] chat_id={}, source=schedule_no_home",
                chat_id.0
            ));
            drop(group_lock); // release before queue processing to avoid deadlock
            process_next_queued_message(bot, chat_id, state).await;
            return;
        };
        let workspace_dir = home.join(".cokacdir").join("workspace").join(&schedule_id);
        sched_debug(&format!(
            "[execute_schedule] id={}, creating workspace: {}",
            schedule_id,
            workspace_dir.display()
        ));
        if let Err(e) = fs::create_dir_all(&workspace_dir) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ⚠ [Schedule] Failed to create workspace: {e}");
            sched_debug(&format!(
                "[execute_schedule] id={}, workspace creation failed: {}, restoring session",
                schedule_id, e
            ));
            {
                let mut data = state.lock().await;
                if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                    set.remove(&schedule_id);
                }
                remove_cancel_token_locked(&mut data, chat_id);
                if let Some(prev) = prev_session {
                    data.sessions.insert(chat_id, prev);
                } else {
                    data.sessions.remove(&chat_id);
                }
            }
            msg_debug(&format!(
                "[queue:trigger] chat_id={}, source=schedule_workspace_error",
                chat_id.0
            ));
            drop(group_lock); // release before queue processing to avoid deadlock
            process_next_queued_message(bot, chat_id, state).await;
            return;
        }
        workspace_dir.display().to_string()
    };

    // Get allowed tools, model, and provider options for this chat
    let (allowed_tools, model, sched_chrome_enabled, effort, codex_fast_enabled) = {
        let data = state.lock().await;
        let chrome = data
            .settings
            .use_chrome
            .get(&chat_id.0.to_string())
            .copied()
            .unwrap_or(false);
        let model = get_model(&data.settings, chat_id);
        let provider = detect_provider(model.as_deref());
        let effort = get_effort_for_provider(&data.settings, chat_id, provider);
        let codex_fast = is_codex_fast(&data.settings, chat_id);
        (
            get_allowed_tools(&data.settings, chat_id),
            model,
            chrome,
            effort,
            codex_fast,
        )
    };

    // Send placeholder (show only the user's original prompt, not the context summary)
    shared_rate_limit_wait(state, chat_id).await;
    let placeholder = match tg!(
        "send_message",
        bot.send_message(chat_id, format!("⏰ {user_prompt}")).await
    ) {
        Ok(msg) => msg,
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ [Schedule] Failed to send placeholder: {}",
                redact_err(&e)
            );
            // Clean up pending + cancel_token, restore session (workspace preserved)
            {
                let mut data = state.lock().await;
                if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                    set.remove(&schedule_id);
                }
                remove_cancel_token_locked(&mut data, chat_id);
                // Inline mode never replaced the session, so leave it untouched.
                if !inline_mode {
                    if let Some(prev) = prev_session {
                        data.sessions.insert(chat_id, prev);
                    } else {
                        data.sessions.remove(&chat_id);
                    }
                }
            }
            msg_debug(&format!(
                "[queue:trigger] chat_id={}, source=schedule_placeholder_error",
                chat_id.0
            ));
            drop(group_lock); // release before queue processing to avoid deadlock
            process_next_queued_message(bot, chat_id, state).await;
            return;
        }
    };
    let placeholder_msg_id = placeholder.id;

    // Build disabled tools notice
    let default_tools: std::collections::HashSet<&str> =
        DEFAULT_ALLOWED_TOOLS.iter().copied().collect();
    let allowed_set: std::collections::HashSet<&str> =
        allowed_tools.iter().map(|s| s.as_str()).collect();
    let disabled: Vec<&&str> = default_tools
        .iter()
        .filter(|t| !allowed_set.contains(**t))
        .collect();
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
        let ctx = data
            .settings
            .context
            .get(&chat_id.0.to_string())
            .copied()
            .unwrap_or(12);
        (
            data.settings
                .instructions
                .get(&chat_id.0.to_string())
                .cloned(),
            ctx,
            data.bot_username.clone(),
            data.bot_display_name.clone(),
        )
    };
    let platform = capitalize_platform(detect_platform(token));
    let sched_role = {
        let base = if inline_mode {
            format!(
                "You are executing a scheduled task through {}.\n\
                 This task runs inline in the chat's current session — its prompt and your reply continue the ongoing conversation here, not a separate workspace.\n\
                 Any files you want to deliver must be sent via the \"{}\" --sendfile command before the task ends.",
                platform, shell_bin_path()
            )
        } else {
            format!(
                "You are executing a scheduled task through {}.\n\
                 Project directory: {project_path}\n\
                 Your current working directory is a dedicated workspace for this schedule.\n\
                 This workspace will be preserved after execution. The user can continue work here via /start.\n\
                 To work with project files, use absolute paths to the project directory.\n\
                 Any files you want to deliver must be sent via the \"{}\" --sendfile command before the task ends.",
                platform, shell_bin_path()
            )
        };
        match &sched_instruction {
            Some(instr) => format!("{}\n\nUser's instruction for this chat:\n{}", base, instr),
            None => base,
        }
    };
    let system_prompt_owned = build_system_prompt(
        &sched_role,
        &crate::utils::format::to_shell_path(&workspace_path),
        chat_id.0,
        &bot_key,
        &disabled_notice,
        None, // scheduled tasks don't need to register further schedules with session context
        &sched_bot_username,
        &sched_bot_display_name,
        None, // scheduled tasks: no user message dedup
        sched_context_count,
        &platform,
    );

    // Retrieve pre-inserted cancel token (from scheduler_loop), or create a new one
    let cancel_token = {
        let mut data = state.lock().await;
        if let Some(existing) = data.cancel_tokens.get(&chat_id) {
            existing.clone()
        } else {
            let token = new_cancel_token();
            insert_cancel_token_locked(&mut data, chat_id, token.clone());
            token
        }
    };

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();
    let cancel_token_clone = cancel_token.clone();
    let model_for_summary = model.clone();

    // Run AI backend in a blocking thread.
    // - Isolated mode: always a new provider session (session_id=None); context arrives via the prompt.
    // - Inline mode: resume the chat's current provider session by id so the schedule's
    //   prompt and reply are appended to that conversation.
    // Session persistence must be kept so users can resume via /SCHEDULE_ID
    let workspace_path_for_claude = workspace_path.clone();
    let resume_session_id: Option<String> = if inline_mode {
        inline_session_id.clone()
    } else {
        None
    };
    let model_clone_for_exec = model.clone();
    let effort_for_exec: Option<String> = effort.clone();
    let codex_fast_for_exec = codex_fast_enabled;
    let bot_key_for_codex = bot_key.clone();
    let request_tasks = {
        let data = state.lock().await;
        data.request_tasks.clone()
    };
    let _ = spawn_tracked_blocking_task(request_tasks, move || {
        let provider = detect_provider(model_clone_for_exec.as_deref());
        sched_debug(&format!(
            "[execute_schedule:spawn_blocking] provider={}, model={:?}, resume_session_id={:?}",
            provider, model_clone_for_exec, resume_session_id
        ));
        let resume_ref = resume_session_id.as_deref();
        let result = if provider == "opencode" {
            let opencode_model = model_clone_for_exec
                .as_deref()
                .and_then(opencode::strip_opencode_prefix);
            sched_debug(&format!("[execute_schedule] → opencode::execute, opencode_model={:?}, session_id={:?}, workspace={}, prompt_len={}, system_prompt_len={}",
                opencode_model, resume_ref, workspace_path_for_claude, prompt.len(), system_prompt_owned.len()));
            opencode::execute_command_streaming(
                &prompt,
                resume_ref,
                &workspace_path_for_claude,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                opencode_model,
                false,
            )
        } else if provider == "agy" {
            let agy_model = model_clone_for_exec
                .as_deref()
                .and_then(agy::strip_agy_prefix);
            sched_debug(&format!("[execute_schedule] → agy::execute, agy_model={:?}, session_id={:?}, workspace={}, prompt_len={}, system_prompt_len={}",
                agy_model, resume_ref, workspace_path_for_claude, prompt.len(), system_prompt_owned.len()));
            agy::execute_command_streaming(
                &prompt,
                resume_ref,
                &workspace_path_for_claude,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                agy_model,
                false,
            )
        } else if provider == "codex" {
            let codex_model = model_clone_for_exec
                .as_deref()
                .and_then(codex::strip_codex_prefix);
            let codex_system_prompt =
                format!("{}{}", system_prompt_owned, codex_extra_instructions());
            let codex_auto_send = codex::CodexAutoSendCtx {
                cokacdir_bin: crate::bin_path().to_string(),
                chat_id: chat_id.0,
                bot_key: bot_key_for_codex,
            };
            codex::execute_command_streaming(
                &prompt,
                resume_ref,
                &workspace_path_for_claude,
                tx.clone(),
                Some(&codex_system_prompt),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                codex_model,
                false,
                Some(&codex_auto_send),
                effort_for_exec.as_deref(),
                codex_fast_for_exec,
            )
        } else {
            let claude_model = model_clone_for_exec
                .as_deref()
                .and_then(claude::strip_claude_prefix);
            claude::execute_command_streaming(
                &prompt,
                resume_ref,
                &workspace_path_for_claude,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                claude_model,
                false,
                sched_chrome_enabled,
                effort_for_exec.as_deref(),
            )
        };
        if let Err(e) = result {
            let _ = tx.send(StreamMessage::Error {
                message: e,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
        }
    });

    // Polling loop
    let bot_owned = bot.clone();
    let state_owned = state.clone();
    let entry_clone = entry.clone();
    let workspace_path_owned = workspace_path.clone();
    let inline_mode_owned = inline_mode;
    // Owned copies of the inline-mode start-state, moved into the polling closure
    // so its cleanup block can run the session_changed guard before writing back
    // to the chat's session. provider comparison uses provider_str (already
    // captured below from the same `model` snapshot taken at execute start).
    let inline_session_id_for_guard = inline_session_id.clone();
    let inline_path_for_guard = inline_path.clone();
    let inline_clear_epoch_for_guard = inline_clear_epoch;
    let provider_str: &'static str = detect_provider(model.as_deref());
    // Captured for schedule_history append at the end of this run.
    // Wall-clock duration is measured from this point (just after placeholder send +
    // pre-execution setup) to the polling loop's completion — close to the user-visible
    // "task is running" window.
    let history_start = std::time::Instant::now();
    let history_bot_key = bot_key.clone();
    let request_tasks = {
        let data = state.lock().await;
        data.request_tasks.clone()
    };
    let _ = spawn_tracked_request_task(request_tasks, async move {
        let _group_lock = group_lock; // hold group chat lock until task ends
        const SPINNER: &[&str] = &[
            "🕐 P",
            "🕑 Pr",
            "🕒 Pro",
            "🕓 Proc",
            "🕔 Proce",
            "🕕 Proces",
            "🕖 Process",
            "🕗 Processi",
            "🕘 Processin",
            "🕙 Processing",
            "🕚 Processing.",
            "🕛 Processing..",
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
                if !done {
                    cancelled = true;
                }
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(polling_time_ms)).await;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done {
                    cancelled = true;
                }
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
                                sched_debug(&format!(
                                    "[sched] Text: {} chars, preview={:?}",
                                    content.len(),
                                    truncate_str(&content, 80)
                                ));
                                raw_entries.push(RawPayloadEntry {
                                    tag: "Text".into(),
                                    content: content.clone(),
                                });
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
                                raw_entries.push(RawPayloadEntry {
                                    tag: "ToolUse".into(),
                                    content: format!("{}: {}", name, input),
                                });
                                if !pending_cokacdir && !silent_mode {
                                    let _fr_before = full_response.len();
                                    if name == "Bash" {
                                        full_response.push_str(&format!(
                                            "\n\n```\n{}\n```\n",
                                            format_bash_command(&input)
                                        ));
                                        sched_debug(&format!("[fr_trace][{}] +ToolUse/Bash: added={}, cmd={:?}, total={} (was {})",
                                            chat_id.0, full_response.len() - _fr_before, truncate_str(&input, 100), full_response.len(), _fr_before));
                                    } else {
                                        full_response.push_str(&format!("\n\n⚙️ {}\n", summary));
                                        sched_debug(&format!("[fr_trace][{}] +ToolUse/{}: added={}, summary={:?}, total={} (was {})",
                                            chat_id.0, name, full_response.len() - _fr_before, truncate_str(&summary, 100), full_response.len(), _fr_before));
                                    }
                                } else if !pending_cokacdir
                                    && silent_mode
                                    && !full_response.is_empty()
                                    && !full_response.ends_with('\n')
                                {
                                    sched_debug(&format!("[schedule_polling] silent mode: inserting \\n\\n after tool_use={}", name));
                                    full_response.push_str("\n\n");
                                    sched_debug(&format!(
                                        "[fr_trace][{}] +ToolUse/silent_nl: added=2, total={}",
                                        chat_id.0,
                                        full_response.len()
                                    ));
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
                                raw_entries.push(RawPayloadEntry {
                                    tag: "ToolResult".into(),
                                    content: format!("is_error={}, content={}", is_error, content),
                                });
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
                                        full_response
                                            .push_str(&format!("\n```\n{}\n```\n", truncated));
                                    } else {
                                        full_response.push_str(&format!("\n`{}`\n\n", truncated));
                                    }
                                    sched_debug(&format!("[fr_trace][{}] +ToolResult/error({}): added={}, preview={:?}, total={} (was {})",
                                        chat_id.0, last_tool_name, full_response.len() - _fr_before, truncate_str(&truncated, 200), full_response.len(), _fr_before));
                                } else if !silent_mode {
                                    if last_tool_name == "Read" {
                                        full_response.push_str(&format!(
                                            "\n✅ `{} bytes`\n\n",
                                            content.len()
                                        ));
                                        sched_debug(&format!("[fr_trace][{}] +ToolResult/Read: added={}, content_bytes={}, total={} (was {})",
                                            chat_id.0, full_response.len() - _fr_before, content.len(), full_response.len(), _fr_before));
                                    } else if !content.is_empty() {
                                        let truncated = truncate_str(&content, 300);
                                        if truncated.contains('\n') {
                                            full_response
                                                .push_str(&format!("\n```\n{}\n```\n", truncated));
                                        } else {
                                            full_response
                                                .push_str(&format!("\n✅ `{}`\n\n", truncated));
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
                                    raw_entries.push(RawPayloadEntry {
                                        tag: "TaskNotification".into(),
                                        content: summary.clone(),
                                    });
                                    let _fr_before = full_response.len();
                                    full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                    sched_debug(&format!("[fr_trace][{}] +TaskNotification: added={}, summary={:?}, total={} (was {})",
                                        chat_id.0, full_response.len() - _fr_before, truncate_str(&summary, 200), full_response.len(), _fr_before));
                                }
                            }
                            StreamMessage::Done { result, session_id } => {
                                sched_debug(&format!(
                                    "[sched] Done: result_len={}, full_response_len={}",
                                    result.len(),
                                    full_response.len()
                                ));
                                if !result.is_empty() && full_response.is_empty() {
                                    sched_debug(&format!(
                                        "[sched] Done: fallback full_response = result ({})",
                                        result.len()
                                    ));
                                    full_response = result.clone();
                                    sched_debug(&format!("[fr_trace][{}] +Done/fallback: set={}, preview={:?}, total={}",
                                        chat_id.0, result.len(), truncate_str(&full_response, 200), full_response.len()));
                                } else if !result.is_empty() {
                                    sched_debug(&format!("[fr_trace][{}] Done/discarded: result_len={} discarded (full_response already has {})",
                                        chat_id.0, result.len(), full_response.len()));
                                }
                                if !result.is_empty() && raw_entries.is_empty() {
                                    raw_entries.push(RawPayloadEntry {
                                        tag: "Text".into(),
                                        content: result,
                                    });
                                }
                                if let Some(sid) = session_id {
                                    exec_session_id = Some(sid);
                                }
                                sched_debug(&format!(
                                    "[fr_trace][{}] =DONE: final_total={}",
                                    chat_id.0,
                                    full_response.len()
                                ));
                                done = true;
                            }
                            StreamMessage::Error {
                                message,
                                stdout,
                                stderr,
                                exit_code,
                            } => {
                                let stdout_display = if stdout.is_empty() {
                                    "(empty)".to_string()
                                } else {
                                    stdout
                                };
                                let stderr_display = if stderr.is_empty() {
                                    "(empty)".to_string()
                                } else {
                                    stderr
                                };
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
                                raw_entries.push(RawPayloadEntry {
                                    tag: "Error".into(),
                                    content: format!(
                                        "exit_code={}, message={}, stdout={}, stderr={}",
                                        code_display, message, stdout_display, stderr_display
                                    ),
                                });
                                had_error = true;
                                done = true;
                            }
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        if !done {
                            had_error = true;
                        }
                        done = true;
                        break;
                    }
                }
            }

            // Update placeholder with progress
            if !done {
                // ── Rolling placeholder pattern (unified for all chats) ──
                let threshold = file_attach_threshold();
                let delta_end =
                    floor_char_boundary(&full_response, full_response.len().min(threshold));
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
                            let _ = tg!(
                                "edit_message",
                                bot_owned
                                    .edit_message_text(chat_id, placeholder_msg_id, &html_delta)
                                    .parse_mode(ParseMode::Html)
                                    .await
                            );
                        } else {
                            if send_long_message(
                                &bot_owned,
                                chat_id,
                                &html_delta,
                                Some(ParseMode::Html),
                                &state_owned,
                            )
                            .await
                            .is_ok()
                            {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!(
                                    "delete_message",
                                    bot_owned.delete_message(chat_id, placeholder_msg_id).await
                                );
                            } else {
                                let truncated_delta =
                                    truncate_str(&normalized_delta, TELEGRAM_MSG_LIMIT);
                                let _ = tg!(
                                    "edit_message",
                                    bot_owned
                                        .edit_message_text(
                                            chat_id,
                                            placeholder_msg_id,
                                            &truncated_delta
                                        )
                                        .await
                                );
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
                                println!(
                                    "  [{ts}]   ⚠ new placeholder failed (schedule): {}",
                                    redact_err(&e)
                                );
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
                        let _ = tg!(
                            "edit_message",
                            &state_owned,
                            chat_id,
                            bot_owned
                                .edit_message_text(chat_id, placeholder_msg_id, &html_text)
                                .parse_mode(ParseMode::Html)
                                .await
                        );
                        last_edit_text = display_text;
                    } else {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!(
                            "send_chat_action",
                            bot_owned
                                .send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
                                .await
                        );
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
        sched_debug(&format!(
            "[execute_schedule] id={}, polling done: cancelled={}, had_error={}, response_len={}",
            schedule_id,
            cancelled,
            had_error,
            full_response.len()
        ));
        // Snapshot whether the AI actually produced any output, taken BEFORE the
        // not-cancelled branch below replaces an empty `full_response` with the
        // user-facing "(No response)" sentinel. Inline cleanup uses this flag so
        // the sentinel does not leak into the chat's session history or get
        // persisted to disk — the chat would otherwise show a phantom assistant
        // turn that the provider's own JSONL transcript does not have.
        let had_real_response = !full_response.is_empty();
        // The "Use /<id> to continue this schedule session" hint points users
        // at the isolated workspace's resume shortcut. Inline mode does not
        // create an isolated workspace, so that shortcut would fail — the
        // continuation is the chat session itself. Suppress the hint there.
        let continue_hint: String = if inline_mode_owned {
            String::new()
        } else {
            format!(
                "\n\nUse /{} to continue this schedule session.",
                schedule_id
            )
        };
        if cancelled {
            sched_debug(&format!(
                "[execute_schedule] id={}, cancelled — killing child process",
                schedule_id
            ));
            // Re-send SIGKILL as a safety belt (cancel_now is idempotent).
            cancel_token.cancel_now();

            shared_rate_limit_wait(&state_owned, chat_id).await;
            // ── Show remaining delta + stopped (unified rolling placeholder) ──
            if full_response.len() < last_confirmed_len
                || !full_response.is_char_boundary(last_confirmed_len)
            {
                last_confirmed_len = 0;
            }
            let remaining = &full_response[last_confirmed_len..];
            if should_attach_response_as_file(full_response.len(), provider_str) {
                let notice = format!(
                    "\u{1f4c4} Response attached as file [Stopped]{}",
                    continue_hint
                );
                let _ = tg!(
                    "edit_message",
                    bot_owned
                        .edit_message_text(chat_id, placeholder_msg_id, &notice)
                        .await
                );
                let stopped_content =
                    format!("{}\n\n[Stopped]", normalize_empty_lines(&full_response));
                send_response_as_file(
                    &bot_owned,
                    chat_id,
                    &stopped_content,
                    &state_owned,
                    "schedule",
                )
                .await;
            } else {
                let display_stopped = if remaining.trim().is_empty() {
                    format!("⛔ Stopped{}", continue_hint)
                } else {
                    let normalized = normalize_empty_lines(remaining);
                    format!("{}\n\n⛔ Stopped{}", normalized, continue_hint)
                };
                let _ = tg!(
                    "edit_message",
                    bot_owned
                        .edit_message_text(chat_id, placeholder_msg_id, &display_stopped)
                        .await
                );
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ [Schedule] Stopped");
        } else {
            // had_real_response (captured above before this branch ran) is the
            // pre-replacement emptiness signal. When the AI produced no output
            // at all, delete the placeholder silently rather than editing it to
            // the "(No response)" sentinel — that string was confusing users
            // and is also gated out of session history below.
            if full_response.is_empty() {
                full_response = "(No response)".to_string();
                sched_debug(&format!(
                    "[fr_trace][{}] =NoResponse: set to '(No response)'",
                    chat_id.0
                ));
            }

            shared_rate_limit_wait(&state_owned, chat_id).await;

            // ── Send only remaining delta (unified rolling placeholder) ──
            if full_response.len() < last_confirmed_len
                || !full_response.is_char_boundary(last_confirmed_len)
            {
                last_confirmed_len = 0;
            }
            let remaining = &full_response[last_confirmed_len..];
            msg_debug(&format!("[rolling_ph/sched] FINAL: placeholder_msg_id={}, confirmed={}, total={}, remaining_len={}",
                placeholder_msg_id, last_confirmed_len, full_response.len(), remaining.trim().len()));
            if !had_real_response || remaining.trim().is_empty() {
                msg_debug(&format!(
                    "[rolling_ph/sched] FINAL DELETE placeholder: msg_id={}",
                    placeholder_msg_id
                ));
                let _ = tg!(
                    "delete_message",
                    bot_owned.delete_message(chat_id, placeholder_msg_id).await
                );
            } else if should_attach_response_as_file(full_response.len(), provider_str) {
                msg_debug(&format!(
                    "[rolling_ph/sched] FINAL FILE ATTACH: total={}",
                    full_response.len()
                ));
                let notice = format!("\u{1f4c4} Response attached as file{}", continue_hint);
                let _ = tg!(
                    "edit_message",
                    bot_owned
                        .edit_message_text(chat_id, placeholder_msg_id, &notice)
                        .await
                );
                let final_text =
                    format!("{}{}", normalize_empty_lines(&full_response), continue_hint);
                send_response_as_file(&bot_owned, chat_id, &final_text, &state_owned, "schedule")
                    .await;
            } else {
                let normalized_remaining = normalize_empty_lines(remaining);
                let final_text = format!("{}{}", normalized_remaining, continue_hint);
                let html_response = markdown_to_telegram_html(&final_text);
                msg_debug(&format!(
                    "[rolling_ph/sched] FINAL EDIT placeholder: msg_id={}, html_len={}",
                    placeholder_msg_id,
                    html_response.len()
                ));
                if html_response.len() <= TELEGRAM_MSG_LIMIT {
                    if let Err(_) = tg!(
                        "edit_message",
                        bot_owned
                            .edit_message_text(chat_id, placeholder_msg_id, &html_response)
                            .parse_mode(ParseMode::Html)
                            .await
                    ) {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!(
                            "edit_message",
                            bot_owned
                                .edit_message_text(chat_id, placeholder_msg_id, &final_text)
                                .await
                        );
                    }
                } else {
                    let send_result = send_long_message(
                        &bot_owned,
                        chat_id,
                        &html_response,
                        Some(ParseMode::Html),
                        &state_owned,
                    )
                    .await;
                    match send_result {
                        Ok(_) => {
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!(
                                "delete_message",
                                bot_owned.delete_message(chat_id, placeholder_msg_id).await
                            );
                        }
                        Err(_) => {
                            let fallback = send_long_message(
                                &bot_owned,
                                chat_id,
                                &final_text,
                                None,
                                &state_owned,
                            )
                            .await;
                            match fallback {
                                Ok(_) => {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!(
                                        "delete_message",
                                        bot_owned.delete_message(chat_id, placeholder_msg_id).await
                                    );
                                }
                                Err(_) => {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let truncated = truncate_str(&final_text, TELEGRAM_MSG_LIMIT);
                                    let _ = tg!(
                                        "edit_message",
                                        bot_owned
                                            .edit_message_text(
                                                chat_id,
                                                placeholder_msg_id,
                                                &truncated
                                            )
                                            .await
                                    );
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
                    let _ = tg!(
                        "send_message",
                        bot_owned.send_message(chat_id, &hook_msg).await
                    );
                }
            }
        }

        // For cron entries with context_summary, extract result summary for next run
        // Skip if execution was cancelled or encountered an error.
        // Inline mode also skips: the chat's live session already carries the
        // conversation forward, so an extra summary text would be redundant.
        sched_debug(&format!("[execute_schedule] id={}, checking context summary: cancelled={}, had_error={}, type={}, once={:?}, has_context={}, inline={}",
            schedule_id, cancelled, had_error, entry_clone.schedule_type, entry_clone.once, entry_clone.context_summary.is_some(), inline_mode_owned));
        let sched_provider = detect_provider(model_for_summary.as_deref());
        let new_context_summary = if inline_mode_owned {
            sched_debug(&format!(
                "[execute_schedule] id={}, inline mode — skipping context summary extraction",
                schedule_id
            ));
            None
        } else if sched_provider != "claude" {
            // Codex/Agy/OpenCode: skip summary extraction (not supported via Claude API)
            sched_debug(&format!(
                "[execute_schedule] id={}, non-Claude backend — skipping context summary",
                schedule_id
            ));
            None
        } else if !cancelled
            && !had_error
            && entry_clone.schedule_type == "cron"
            && !entry_clone.once.unwrap_or(false)
            && entry_clone.context_summary.is_some()
        {
            sched_debug(&format!(
                "[execute_schedule] id={}, extracting result summary",
                schedule_id
            ));
            if let Some(ref sid) = exec_session_id {
                let sid = sid.clone();
                let path = workspace_path_owned.clone();
                let model = model_for_summary.clone();
                let request_tasks = {
                    let data = state_owned.lock().await;
                    data.request_tasks.clone()
                };
                let summary_result = spawn_tracked_blocking_result(request_tasks, move || {
                    let claude_model = model.as_deref().and_then(claude::strip_claude_prefix);
                    claude::extract_result_summary(&sid, &path, claude_model)
                })
                .await;
                match summary_result {
                    Ok(Some(Ok(ref summary))) => {
                        sched_debug(&format!(
                            "[execute_schedule] id={}, new context summary: {} chars",
                            schedule_id,
                            summary.len()
                        ));
                        Some(summary.clone())
                    }
                    _ => {
                        sched_debug(&format!(
                            "[execute_schedule] id={}, summary extraction failed",
                            schedule_id
                        ));
                        None
                    }
                }
            } else {
                sched_debug(&format!(
                    "[execute_schedule] id={}, no session_id for summary",
                    schedule_id
                ));
                None
            }
        } else {
            None
        };

        // Save schedule session to file so user can resume via /start [workspace_path].
        // Inline mode has no isolated workspace — the provider session is part of
        // the chat's normal session, which persists through the live ChatSession
        // and provider-side JSONL. Skipping the workspace write avoids littering
        // the chat's working directory with an unrelated session snapshot.
        if !inline_mode_owned {
            if let Some(ref sid) = exec_session_id {
                let mut sched_session = ChatSession {
                    session_id: Some(sid.clone()),
                    current_path: Some(workspace_path_owned.clone()),
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    pending_uploads_added_at: None,
                };
                // Add user prompt and AI response to history for session continuity.
                // Use had_real_response (not !full_response.is_empty()) so the
                // "(No response)" UI sentinel does not leak into the isolated
                // workspace's session file either — keeping all three paths
                // (inline cleanup, isolated workspace, /start preview) free of
                // a phantom assistant turn the user never saw.
                sched_session.history.push(HistoryItem {
                    item_type: HistoryType::User,
                    content: entry_clone.prompt.clone(),
                });
                if had_real_response {
                    sched_session.history.push(HistoryItem {
                        item_type: HistoryType::Assistant,
                        content: full_response.clone(),
                    });
                }
                // sched_provider already computed above
                save_session_to_file(&sched_session, &workspace_path_owned, sched_provider);
            }
        }

        // Write to group chat shared log (scheduled task)
        sched_debug(&format!(
            "[sched] JSONL check: chat_id={}, raw_entries_count={}",
            chat_id.0,
            raw_entries.len()
        ));
        if chat_id.0 < 0 && !sched_bot_username.is_empty() {
            let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
            let dn = if sched_bot_display_name.is_empty() {
                None
            } else {
                Some(sched_bot_display_name.clone())
            };
            append_group_chat_log(
                chat_id.0,
                &GroupChatLogEntry {
                    ts: now_ts.clone(),
                    bot: sched_bot_username.clone(),
                    bot_display_name: dn.clone(),
                    role: "user".to_string(),
                    from: Some("scheduled_task".to_string()),
                    text: entry_clone.prompt.clone(),
                    clear: false,
                },
            );
            if !raw_entries.is_empty() {
                sched_debug(&format!(
                    "[sched] JSONL: writing user+assistant entries, raw_entries_count={}",
                    raw_entries.len()
                ));
                append_group_chat_log(
                    chat_id.0,
                    &GroupChatLogEntry {
                        ts: now_ts,
                        bot: sched_bot_username.clone(),
                        bot_display_name: dn,
                        role: "assistant".to_string(),
                        from: None,
                        text: serialize_payload(&std::mem::take(&mut raw_entries)),
                        clear: false,
                    },
                );
            } else {
                sched_debug(&format!(
                    "[sched] JSONL: user entry written, assistant SKIPPED (raw_entries is empty)"
                ));
            }
        }

        // Append a JSONL run record to ~/.cokacdir/schedule_history/<id>.log.
        // Best-effort: any I/O failure is swallowed inside append_schedule_history so
        // it cannot affect the user-facing completion path. Both `cancelled` and
        // `had_error` paths fall through to here, which is why the call lives outside
        // those branches.
        let history_status = if cancelled {
            "cancelled"
        } else if had_error {
            "error"
        } else {
            "ok"
        };
        let history_duration_ms = history_start.elapsed().as_millis() as u64;
        append_schedule_history(
            &schedule_id,
            chat_id.0,
            &history_bot_key,
            &entry_clone.prompt,
            history_status,
            &full_response,
            None,
            &workspace_path_owned,
            history_duration_ms,
        );

        // Update schedule file (last_run / delete if once)
        sched_debug(&format!(
            "[execute_schedule] id={}, calling update_schedule_after_run",
            schedule_id
        ));
        update_schedule_after_run(&entry_clone, new_context_summary);

        // Workspace directory is preserved for user to continue work via /start

        // Clean up.
        // - Isolated mode: restore the user's pre-schedule session (the temp session
        //   we inserted in scheduler_cycle is discarded).
        // - Inline mode: leave the chat session in place; if the provider issued a
        //   new session_id during this run (e.g. Claude fork on resume), propagate
        //   it so the user's next message continues the same provider session.
        //   Append the schedule prompt+reply to history for continuity, then
        //   persist the session to disk to match the normal-message handler so
        //   the inline run survives bot restarts (matches the writeback at line
        //   ~10168 in handle_text_message's polling completion).
        sched_debug(&format!("[execute_schedule] id={}, cleaning up: removing cancel_token, pending, inline={}, has_prev={}",
            schedule_id, inline_mode_owned, prev_session.is_some()));
        {
            let mut data = state_owned.lock().await;
            remove_cancel_token_locked(&mut data, chat_id);
            if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                set.remove(&schedule_id);
            }
            if inline_mode_owned {
                // session_changed guard: if the user mutated the chat's session
                // mid-run, the writeback below would partially resurrect a /clear,
                // land the schedule's prompt+reply in a different /start path, or
                // overwrite a different model's session_id. Mirrors the polling
                // completion guard at handle_text_message's writeback. Read all
                // guard inputs before any &mut borrow of `sessions` to avoid a
                // re-borrow conflict with the `get_mut` below.
                let now_clear_epoch = data.clear_epoch.get(&chat_id).copied().unwrap_or(0);
                let now_model = get_model(&data.settings, chat_id);
                let now_provider = detect_provider(now_model.as_deref());
                let (now_sid, now_path) = data
                    .sessions
                    .get(&chat_id)
                    .map(|s| (s.session_id.clone(), s.current_path.clone()))
                    .unwrap_or((None, None));
                let session_changed = now_provider != provider_str
                    || now_path != inline_path_for_guard
                    || now_sid != inline_session_id_for_guard
                    || now_clear_epoch != inline_clear_epoch_for_guard;
                if session_changed {
                    sched_debug(&format!(
                        "[execute_schedule] id={}, inline cleanup: session changed during run — skipping writeback \
                         (provider: {}→{}, path: {:?}→{:?}, sid: {:?}→{:?}, epoch: {}→{})",
                        schedule_id, provider_str, now_provider,
                        inline_path_for_guard, now_path,
                        inline_session_id_for_guard, now_sid,
                        inline_clear_epoch_for_guard, now_clear_epoch
                    ));
                } else if let Some(session) = data.sessions.get_mut(&chat_id) {
                    if let Some(ref sid) = exec_session_id {
                        session.session_id = Some(sid.clone());
                    }
                    session.history.push(HistoryItem {
                        item_type: HistoryType::User,
                        content: entry_clone.prompt.clone(),
                    });
                    // Skip the assistant push when the AI produced no real output
                    // (had_real_response was captured BEFORE the "(No response)"
                    // sentinel replacement). Without this guard the chat history
                    // would contain a phantom assistant turn that no provider
                    // JSONL backs.
                    if had_real_response {
                        session.history.push(HistoryItem {
                            item_type: HistoryType::Assistant,
                            content: full_response.clone(),
                        });
                    }
                    // Persist to disk so the inline run is durable across restarts.
                    // save_session_to_file takes `&ChatSession`; the `session`
                    // binding here is `&mut ChatSession` and Rust performs an
                    // implicit shared reborrow at the call. current_path is
                    // cloned first so the path argument and the session reborrow
                    // do not need to share a live borrow of `session`.
                    let cp = session.current_path.clone().unwrap_or_default();
                    if !cp.is_empty() {
                        save_session_to_file(session, &cp, provider_str);
                    } else {
                        sched_debug(&format!("[execute_schedule] id={}, inline cleanup: session has no current_path — skipping save_session_to_file", schedule_id));
                    }
                }
            } else if let Some(prev) = prev_session {
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
            let _ = tg!(
                "delete_message",
                bot_owned.delete_message(chat_id, msg_id).await
            );
        }
        msg_debug(&format!(
            "[queue:trigger] chat_id={}, source=schedule_poll_completed",
            chat_id.0
        ));
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
    let cancel_token = new_cancel_token();
    {
        let mut data = state.lock().await;
        if data.cancel_tokens.contains_key(&chat_id) {
            msg_debug(&format!(
                "[process_bot_message] chat_id={}, cancel_token exists (busy), skipping id={}",
                chat_id.0, msg.id
            ));
            // Slot was claimed by someone else between the scheduler's
            // busy-check and our claim attempt. Leave the bot-message
            // file on disk so the next scheduler tick re-tries — without
            // this, the message would be lost (file deleted by scheduler
            // before claim, claim fails, no retry path).
            return;
        }
        insert_cancel_token_locked(&mut data, chat_id, cancel_token.clone());
    }

    // File deletion is deferred until the placeholder send succeeds (see the
    // delete site after `placeholder_msg_id`). Deleting here would lose the
    // message on every failure path between claim and placeholder —
    // cancellation during lock wait, panic in session lookup, placeholder
    // network error — because none of those branches have a re-queue path.
    // The cancel_token holds the per-chat slot until removed, so the
    // scheduler's busy-check still skips this file on subsequent ticks
    // (preventing the original TOCTOU where a sibling claim left a deleted
    // file unprocessed).

    // Acquire group chat lock (serializes processing across bots in the same group chat)
    let group_lock = acquire_group_chat_lock(chat_id.0).await;

    // Check if cancelled during lock wait
    if cancel_token.cancelled.load(Ordering::Relaxed) {
        // File is intentionally NOT deleted here: removing the cancel_token
        // below frees the busy slot, so the next scheduler tick re-discovers
        // this file via scan_messages and re-runs process_bot_message. This
        // is the recovery path for /stop landing while we waited for the
        // group lock — without it the message vanishes silently.
        msg_debug(&format!(
            "[process_bot_message] cancelled during lock wait, id={} (file preserved for retry)",
            msg.id
        ));
        msg_debug(&format!(
            "[queue:trigger] chat_id={}, source=botmsg_cancelled_during_lock",
            chat_id.0
        ));
        {
            let mut data = state.lock().await;
            remove_cancel_token_locked(&mut data, chat_id);
        }
        drop(group_lock); // release before queue processing to avoid deadlock
        process_next_queued_message(bot, chat_id, state).await;
        return;
    }

    // Auto-restore session
    msg_debug(&format!(
        "[process_bot_message] auto_restore_session for bot:{}",
        msg.from
    ));
    auto_restore_session(state, chat_id, &format!("bot:{}", msg.from)).await;

    // Get session info, allowed tools, model, history, instruction, and provider options
    let (
        session_info,
        allowed_tools,
        model,
        history,
        instruction,
        context_count,
        botmsg_chrome_enabled,
        effort,
        codex_fast_enabled,
    ) = {
        let data = state.lock().await;
        let info = data.sessions.get(&chat_id).and_then(|session| {
            session.current_path.as_ref().map(|_| {
                (
                    session.session_id.clone(),
                    session.current_path.clone().unwrap_or_default(),
                )
            })
        });
        let tools = get_allowed_tools(&data.settings, chat_id);
        let mdl = get_model(&data.settings, chat_id);
        let hist = data
            .sessions
            .get(&chat_id)
            .map(|s| s.history.clone())
            .unwrap_or_default();
        let instr = data
            .settings
            .instructions
            .get(&chat_id.0.to_string())
            .cloned();
        let ctx = data
            .settings
            .context
            .get(&chat_id.0.to_string())
            .copied()
            .unwrap_or(12);
        let chrome = data
            .settings
            .use_chrome
            .get(&chat_id.0.to_string())
            .copied()
            .unwrap_or(false);
        let provider = detect_provider(mdl.as_deref());
        let eff = get_effort_for_provider(&data.settings, chat_id, provider);
        let codex_fast = is_codex_fast(&data.settings, chat_id);
        msg_debug(&format!("[process_bot_message] session_info={}, tools={}, model={:?}, history_len={}, instruction={}",
            info.is_some(), tools.len(), mdl, hist.len(), instr.is_some()));
        (info, tools, mdl, hist, instr, ctx, chrome, eff, codex_fast)
    };

    let (session_id, current_path) = match session_info {
        Some(info) => {
            msg_debug(&format!(
                "[process_bot_message] session found: session_id={:?}, path={}",
                info.0, info.1
            ));
            info
        }
        None => {
            // No active session — create an error response
            msg_debug(&format!(
                "[process_bot_message] no session for chat_id={}, sending error response",
                chat_id.0
            ));
            // Discard the on-disk file: re-queueing would loop the same
            // "no session" error every scheduler tick. The user has been
            // (best-effort) notified via the send_message below.
            let remove_result = fs::remove_file(&msg.file_path);
            msg_debug(&format!(
                "[process_bot_message] deleted message file (no session): id={}, ok={}",
                msg.id,
                remove_result.is_ok()
            ));
            {
                let mut data = state.lock().await;
                remove_cancel_token_locked(&mut data, chat_id);
                let cleared = data
                    .message_queues
                    .remove(&chat_id)
                    .map(|q| q.len())
                    .unwrap_or(0);
                if cleared > 0 {
                    msg_debug(&format!("[queue:clear] chat_id={}, no session (bot msg), cleared {} queued messages", chat_id.0, cleared));
                }
            }
            shared_rate_limit_wait(state, chat_id).await;
            let _ = tg!(
                "send_message",
                bot.send_message(
                    chat_id,
                    format!(
                        "📨 @{}: {}\n\n⚠️ No active session. Use /start <path> first.",
                        msg.from,
                        truncate_str(&msg.content, 200)
                    )
                )
                .await
            );
            msg_debug("[process_bot_message] END (no session)");
            return;
        }
    };

    // Send placeholder
    msg_debug("[process_bot_message] sending placeholder");
    shared_rate_limit_wait(state, chat_id).await;
    let placeholder = match tg!("send_message", bot.send_message(chat_id, "...").await) {
        Ok(m) => {
            msg_debug(&format!(
                "[process_bot_message] placeholder sent: msg_id={}",
                m.id
            ));
            m
        }
        Err(e) => {
            msg_debug(&format!(
                "[process_bot_message] failed to send placeholder: {}, aborting",
                e
            ));
            msg_debug(&format!(
                "[queue:trigger] chat_id={}, source=botmsg_placeholder_error",
                chat_id.0
            ));
            {
                let mut data = state.lock().await;
                remove_cancel_token_locked(&mut data, chat_id);
            }
            // Discard the on-disk file: leaving it would let the scheduler
            // re-attempt every 5s, which for a permanent failure (bot kicked,
            // chat closed, token revoked) becomes log spam with the same
            // error class. A transient failure forces the user to manually
            // re-send, but that matches the "Please try again" notice below.
            let remove_result = fs::remove_file(&msg.file_path);
            msg_debug(&format!(
                "[process_bot_message] deleted message file (placeholder failed): id={}, ok={}",
                msg.id,
                remove_result.is_ok()
            ));
            // Best-effort surface the dropped bot-to-bot message. The notice
            // itself may also fail (the original placeholder error was likely
            // a network / rate-limit / permissions issue affecting all
            // outbound messages in this chat) — that's why this is `let _ =`.
            shared_rate_limit_wait(state, chat_id).await;
            let _ =
                tg!("send_message", bot.send_message(chat_id,
                format!("📨 @{}: {}\n\n⚠️ Failed to deliver bot message ({}). Please try again.",
                    msg.from, truncate_str(&msg.content, 200), e)).await);
            drop(group_lock); // release before queue processing to avoid deadlock
            process_next_queued_message(bot, chat_id, state).await;
            return;
        }
    };
    let placeholder_msg_id = placeholder.id;

    // Placeholder is visible to the user — processing has visibly started.
    // This is the safe point to delete the on-disk message file:
    //   • Earlier (at claim) lost the message on cancel-during-lock-wait,
    //     no-session, or placeholder-send failure paths.
    //   • Later (after AI completes) would risk replay if the bot restarts
    //     mid-turn — the user has already seen "..." and would receive the
    //     same prompt again on next startup.
    let remove_result = fs::remove_file(&msg.file_path);
    msg_debug(&format!(
        "[process_bot_message] deleted message file post-placeholder: id={}, path={}, ok={}",
        msg.id,
        msg.file_path.display(),
        remove_result.is_ok()
    ));

    // Build disabled tools notice
    let default_tools: std::collections::HashSet<&str> =
        DEFAULT_ALLOWED_TOOLS.iter().copied().collect();
    let allowed_set: std::collections::HashSet<&str> =
        allowed_tools.iter().map(|s| s.as_str()).collect();
    let disabled: Vec<&&str> = default_tools
        .iter()
        .filter(|t| !allowed_set.contains(**t))
        .collect();
    msg_debug(&format!(
        "[process_bot_message] disabled_tools={}",
        disabled.len()
    ));
    let disabled_notice = if disabled.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = disabled.iter().map(|t| **t).collect();
        msg_debug(&format!(
            "[process_bot_message] disabled: [{}]",
            names.join(", ")
        ));
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
            msg_debug(&format!(
                "[process_bot_message] instruction present, len={}",
                instr.len()
            ));
            format!(
                "You are chatting with a user through {}.\n\nUser's instruction for this chat:\n{}",
                platform, instr
            )
        }
        None => {
            msg_debug("[process_bot_message] no instruction set");
            format!("You are chatting with a user through {}.", platform)
        }
    };
    let system_prompt_owned = build_system_prompt(
        &role,
        &current_path,
        chat_id.0,
        &bot_key,
        &disabled_notice,
        session_id.as_deref(),
        bot_username,
        bot_display_name,
        None, // bot-to-bot messages: no user message dedup
        context_count,
        &platform,
    );
    msg_debug(&format!(
        "[process_bot_message] system_prompt built, len={}",
        system_prompt_owned.len()
    ));

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
    let bot_key_for_codex = bot_key.clone();
    let effort_clone: Option<String> = effort.clone();
    let codex_fast_clone = codex_fast_enabled;
    msg_debug(&format!(
        "[process_bot_message] spawning AI backend: model={:?}, history_len={}, prompt_len={}",
        model_clone,
        history_clone.len(),
        prompt_for_ai.len()
    ));
    let request_tasks = {
        let data = state.lock().await;
        data.request_tasks.clone()
    };
    let _ = spawn_tracked_blocking_task(request_tasks, move || {
        let provider = detect_provider(model_clone.as_deref());
        msg_debug(&format!(
            "[process_bot_message:spawn_blocking] provider={}, model={:?}",
            provider, model_clone
        ));
        let result = if provider == "opencode" {
            let opencode_model = model_clone
                .as_deref()
                .and_then(opencode::strip_opencode_prefix);
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
        } else if provider == "agy" {
            let agy_model = model_clone.as_deref().and_then(agy::strip_agy_prefix);
            msg_debug(&format!("[process_bot_message] → agy::execute, agy_model={:?}, session_id={:?}, path={}, prompt_len={}, system_prompt_len={}",
                agy_model, session_id_clone, current_path_clone, prompt_for_ai.len(), system_prompt_owned.len()));
            agy::execute_command_streaming(
                &prompt_for_ai,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                agy_model,
                false,
            )
        } else if provider == "codex" {
            let codex_model = model_clone.as_deref().and_then(codex::strip_codex_prefix);
            let codex_system_prompt =
                format!("{}{}", system_prompt_owned, codex_extra_instructions());
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
                    msg_debug(
                        "[process_bot_message] codex: NEW session, no history — sp via -c file",
                    );
                }
                prompt_for_ai.clone()
            };
            msg_debug(&format!("[process_bot_message] → codex::execute, codex_model={:?}, codex_prompt_len={}, resume={}, system_prompt_passed=true",
                codex_model, codex_prompt.len(), is_resume));
            let codex_auto_send = codex::CodexAutoSendCtx {
                cokacdir_bin: crate::bin_path().to_string(),
                chat_id: chat_id.0,
                bot_key: bot_key_for_codex,
            };
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
                Some(&codex_auto_send),
                effort_clone.as_deref(),
                codex_fast_clone,
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
                effort_clone.as_deref(),
            )
        };
        if let Err(e) = result {
            let _ = tx.send(StreamMessage::Error {
                message: e,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
        }
    });

    // Polling loop (spawned as async task to avoid blocking scheduler)
    let bot_owned = bot.clone();
    let state_owned = state.clone();
    let msg_clone = msg.clone();
    let provider_str: &'static str = detect_provider(model.as_deref());
    let current_path_owned = current_path.clone();
    // Captured at spawn so the post-completion guard catches /clear and /start
    // same-path session-id swaps (see handle_text_message guard for rationale).
    let captured_sid = session_id.clone();
    let captured_clear_epoch = {
        let data = state.lock().await;
        data.clear_epoch.get(&chat_id).copied().unwrap_or(0)
    };
    let prompt_owned = prompt.clone();
    let bmsg_id_for_log = msg.id.clone();
    let bot_username_for_log = bot_username.to_string();
    let bot_display_name_for_log = bot_display_name.to_string();
    let from_bot_for_log = msg.from.clone();
    msg_debug(&format!("[process_bot_message] spawning polling loop: provider={}, msg_id={}, placeholder_msg_id={}",
        provider_str, bmsg_id_for_log, placeholder_msg_id));
    let request_tasks = {
        let data = state.lock().await;
        data.request_tasks.clone()
    };
    let _ = spawn_tracked_request_task(request_tasks, async move {
        let _group_lock = group_lock; // hold group chat lock until task ends
        const SPINNER: &[&str] = &[
            "🕐 P",
            "🕑 Pr",
            "🕒 Pro",
            "🕓 Proc",
            "🕔 Proce",
            "🕕 Proces",
            "🕖 Process",
            "🕗 Processi",
            "🕘 Processin",
            "🕙 Processing",
            "🕚 Processing.",
            "🕛 Processing..",
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
        msg_debug(&format!(
            "[botmsg_poll:{}] started: polling_time_ms={}, silent_mode={}",
            bmsg_id_for_log, polling_time_ms, silent_mode
        ));

        let mut queue_done = false;
        let mut response_rendered = false;
        while !done || !queue_done {
            if cancel_token.cancelled.load(Ordering::Relaxed) {
                msg_debug(&format!(
                    "[botmsg_poll:{}] cancelled (pre-sleep check)",
                    bmsg_id_for_log
                ));
                if !done {
                    cancelled = true;
                }
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(polling_time_ms)).await;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                if !done {
                    cancelled = true;
                }
                break;
            }

            // Drain messages
            if !done {
                loop {
                    match rx.try_recv() {
                        Ok(stream_msg) => match stream_msg {
                            StreamMessage::Init { session_id: sid } => {
                                msg_debug(&format!(
                                    "[botmsg_poll:{}] Init: session_id={}",
                                    bmsg_id_for_log, sid
                                ));
                                new_session_id = Some(sid);
                            }
                            StreamMessage::Text { content } => {
                                msg_debug(&format!(
                                    "[botmsg_poll:{}] Text: chunk_len={}, total_len={}",
                                    bmsg_id_for_log,
                                    content.len(),
                                    full_response.len() + content.len()
                                ));
                                raw_entries.push(RawPayloadEntry {
                                    tag: "Text".into(),
                                    content: content.clone(),
                                });
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
                                raw_entries.push(RawPayloadEntry {
                                    tag: "ToolUse".into(),
                                    content: format!("{}: {}", name, input),
                                });
                                if !pending_cokacdir && !silent_mode {
                                    let _fr_before = full_response.len();
                                    if name == "Bash" {
                                        full_response.push_str(&format!(
                                            "\n\n```\n{}\n```\n",
                                            format_bash_command(&input)
                                        ));
                                        msg_debug(&format!("[fr_trace][{}] +ToolUse/Bash: added={}, cmd={:?}, total={} (was {})",
                                                chat_id.0, full_response.len() - _fr_before, truncate_str(&input, 100), full_response.len(), _fr_before));
                                    } else {
                                        full_response.push_str(&format!("\n\n⚙️ {}\n", summary));
                                        msg_debug(&format!("[fr_trace][{}] +ToolUse/{}: added={}, summary={:?}, total={} (was {})",
                                                chat_id.0, name, full_response.len() - _fr_before, truncate_str(&summary, 100), full_response.len(), _fr_before));
                                    }
                                } else if !pending_cokacdir
                                    && silent_mode
                                    && !full_response.is_empty()
                                    && !full_response.ends_with('\n')
                                {
                                    msg_debug(&format!("[botmsg_poll:{}] silent mode: inserting \\n\\n after tool_use={}", bmsg_id_for_log, name));
                                    full_response.push_str("\n\n");
                                    msg_debug(&format!(
                                        "[fr_trace][{}] +ToolUse/silent_nl: added=2, total={}",
                                        chat_id.0,
                                        full_response.len()
                                    ));
                                }
                            }
                            StreamMessage::ToolResult { content, is_error } => {
                                msg_debug(&format!("[botmsg_poll:{}] ToolResult: is_error={}, content_len={}, pending_cokacdir={}, last_tool={}",
                                        bmsg_id_for_log, is_error, content.len(), pending_cokacdir, last_tool_name));
                                if is_error {
                                    msg_debug(&format!("[botmsg_poll:{}] ToolResult ERROR: last_tool={}, content_preview={:?}", bmsg_id_for_log, last_tool_name, truncate_str(&content, 300)));
                                }
                                raw_entries.push(RawPayloadEntry {
                                    tag: "ToolResult".into(),
                                    content: format!("is_error={}, content={}", is_error, content),
                                });
                                let _fr_before = full_response.len();
                                if std::mem::take(&mut pending_cokacdir) {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    if std::mem::take(&mut suppress_tool_display) {
                                        println!(
                                            "  [{ts}]   ↩ [BotMsg] cokacdir (chat_log, suppressed)"
                                        );
                                        msg_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir_suppressed: added=0, total={}", chat_id.0, full_response.len()));
                                    } else {
                                        println!("  [{ts}]   ↩ [BotMsg] cokacdir: {content}");
                                        let formatted = format_cokacdir_result(&content);
                                        msg_debug(&format!(
                                            "[botmsg_poll:{}] cokacdir result formatted_len={}",
                                            bmsg_id_for_log,
                                            formatted.len()
                                        ));
                                        if !formatted.is_empty() {
                                            full_response.push_str(&format!("\n{}\n", formatted));
                                            msg_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir: added={}, preview={:?}, total={} (was {})",
                                                    chat_id.0, full_response.len() - _fr_before, truncate_str(&formatted, 200), full_response.len(), _fr_before));
                                        } else {
                                            msg_debug(&format!("[fr_trace][{}] +ToolResult/cokacdir: formatted_empty, added=0, total={}", chat_id.0, full_response.len()));
                                        }
                                    }
                                } else if is_error && !silent_mode {
                                    msg_debug(&format!(
                                        "[botmsg_poll:{}] tool error: {}",
                                        bmsg_id_for_log,
                                        truncate_str(&content, 200)
                                    ));
                                    let truncated = truncate_str(&content, 500);
                                    if truncated.contains('\n') {
                                        full_response
                                            .push_str(&format!("\n```\n{}\n```\n", truncated));
                                    } else {
                                        full_response.push_str(&format!("\n`{}`\n\n", truncated));
                                    }
                                    msg_debug(&format!("[fr_trace][{}] +ToolResult/error({}): added={}, preview={:?}, total={} (was {})",
                                            chat_id.0, last_tool_name, full_response.len() - _fr_before, truncate_str(&truncated, 200), full_response.len(), _fr_before));
                                } else if !silent_mode {
                                    if last_tool_name == "Read" {
                                        full_response.push_str(&format!(
                                            "\n✅ `{} bytes`\n\n",
                                            content.len()
                                        ));
                                        msg_debug(&format!("[fr_trace][{}] +ToolResult/Read: added={}, content_bytes={}, total={} (was {})",
                                                chat_id.0, full_response.len() - _fr_before, content.len(), full_response.len(), _fr_before));
                                    } else if !content.is_empty() {
                                        let truncated = truncate_str(&content, 300);
                                        if truncated.contains('\n') {
                                            full_response
                                                .push_str(&format!("\n```\n{}\n```\n", truncated));
                                        } else {
                                            full_response
                                                .push_str(&format!("\n✅ `{}`\n\n", truncated));
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
                                msg_debug(&format!(
                                    "[botmsg_poll:{}] TaskNotification: summary_len={}",
                                    bmsg_id_for_log,
                                    summary.len()
                                ));
                                if !summary.is_empty() {
                                    raw_entries.push(RawPayloadEntry {
                                        tag: "TaskNotification".into(),
                                        content: summary.clone(),
                                    });
                                    let _fr_before = full_response.len();
                                    full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                    msg_debug(&format!("[fr_trace][{}] +TaskNotification: added={}, summary={:?}, total={} (was {})",
                                            chat_id.0, full_response.len() - _fr_before, truncate_str(&summary, 200), full_response.len(), _fr_before));
                                }
                            }
                            StreamMessage::Done {
                                result,
                                session_id: sid,
                            } => {
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
                                    raw_entries.push(RawPayloadEntry {
                                        tag: "Text".into(),
                                        content: result,
                                    });
                                }
                                if let Some(s) = sid {
                                    new_session_id = Some(s);
                                }
                                msg_debug(&format!(
                                    "[fr_trace][{}] =DONE: final_total={}",
                                    chat_id.0,
                                    full_response.len()
                                ));
                                done = true;
                            }
                            StreamMessage::Error {
                                message,
                                stdout,
                                stderr,
                                exit_code,
                            } => {
                                msg_debug(&format!("[botmsg_poll:{}] Error: msg={}, exit_code={:?}, stdout_len={}, stderr_len={}",
                                        bmsg_id_for_log, message, exit_code, stdout.len(), stderr.len()));
                                let stdout_display = if stdout.is_empty() {
                                    "(empty)".to_string()
                                } else {
                                    stdout
                                };
                                let stderr_display = if stderr.is_empty() {
                                    "(empty)".to_string()
                                } else {
                                    stderr
                                };
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
                                raw_entries.push(RawPayloadEntry {
                                    tag: "Error".into(),
                                    content: format!(
                                        "exit_code={}, message={}, stdout={}, stderr={}",
                                        code_display, message, stdout_display, stderr_display
                                    ),
                                });
                                done = true;
                            }
                        },
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            msg_debug(&format!(
                                "[botmsg_poll:{}] channel disconnected, setting done=true",
                                bmsg_id_for_log
                            ));
                            done = true;
                            break;
                        }
                    }
                }

                if !done {
                    // ── Rolling placeholder pattern (unified for all chats) ──
                    let threshold = file_attach_threshold();
                    let delta_end =
                        floor_char_boundary(&full_response, full_response.len().min(threshold));
                    if delta_end > last_confirmed_len {
                        let delta = &full_response[last_confirmed_len..delta_end];
                        let normalized_delta = normalize_empty_lines(delta);
                        let html_delta = markdown_to_telegram_html(&normalized_delta);
                        if html_delta.trim().is_empty() {
                            last_confirmed_len = delta_end;
                        } else {
                            msg_debug(&format!(
                                "[botmsg_poll:{}] finalizing placeholder with delta_len={}",
                                bmsg_id_for_log,
                                normalized_delta.len()
                            ));
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            if html_delta.len() <= TELEGRAM_MSG_LIMIT {
                                let _ = tg!(
                                    "edit_message",
                                    bot_owned
                                        .edit_message_text(chat_id, placeholder_msg_id, &html_delta)
                                        .parse_mode(ParseMode::Html)
                                        .await
                                );
                            } else {
                                if send_long_message(
                                    &bot_owned,
                                    chat_id,
                                    &html_delta,
                                    Some(ParseMode::Html),
                                    &state_owned,
                                )
                                .await
                                .is_ok()
                                {
                                    shared_rate_limit_wait(&state_owned, chat_id).await;
                                    let _ = tg!(
                                        "delete_message",
                                        bot_owned.delete_message(chat_id, placeholder_msg_id).await
                                    );
                                } else {
                                    let truncated_delta =
                                        truncate_str(&normalized_delta, TELEGRAM_MSG_LIMIT);
                                    let _ = tg!(
                                        "edit_message",
                                        bot_owned
                                            .edit_message_text(
                                                chat_id,
                                                placeholder_msg_id,
                                                &truncated_delta
                                            )
                                            .await
                                    );
                                }
                            }
                            last_confirmed_len = delta_end;
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            match tg!("send_message", bot_owned.send_message(chat_id, "...").await)
                            {
                                Ok(new_ph) => {
                                    placeholder_msg_id = new_ph.id;
                                }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!(
                                        "  [{ts}]   ⚠ new placeholder failed (botmsg): {}",
                                        redact_err(&e)
                                    );
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
                            msg_debug(&format!(
                                "[botmsg_poll:{}] spinner update spin_idx={}",
                                bmsg_id_for_log, spin_idx
                            ));
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let html_text = markdown_to_telegram_html(&display_text);
                            let _ = tg!(
                                "edit_message",
                                &state_owned,
                                chat_id,
                                bot_owned
                                    .edit_message_text(chat_id, placeholder_msg_id, &html_text)
                                    .parse_mode(ParseMode::Html)
                                    .await
                            );
                            last_edit_text = display_text;
                        } else {
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!(
                                "send_chat_action",
                                bot_owned
                                    .send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
                                    .await
                            );
                        }
                    }
                }
            }

            // Render final response once when AI completes
            if done && !response_rendered {
                response_rendered = true;
                msg_debug(&format!(
                    "[botmsg_poll:{}] rendering final response: response_len={}",
                    bmsg_id_for_log,
                    full_response.len()
                ));

                shared_rate_limit_wait(&state_owned, chat_id).await;

                // Capture pre-replacement emptiness so the UI silently deletes
                // the placeholder when the AI produced nothing, and the session
                // writeback below skips the assistant push — see handle_text_message
                // for the same pattern.
                let was_originally_empty = full_response.is_empty();
                if was_originally_empty {
                    msg_debug(&format!(
                        "[botmsg_poll:{}] empty response, using placeholder text",
                        bmsg_id_for_log
                    ));
                    full_response = "(No response)".to_string();
                    msg_debug(&format!(
                        "[fr_trace][{}] =NoResponse: set to '(No response)'",
                        chat_id.0
                    ));
                }

                let final_response = normalize_empty_lines(&full_response);
                msg_debug(&format!(
                    "[botmsg_poll:{}] final: response_len={}, msg_limit={}",
                    bmsg_id_for_log,
                    final_response.len(),
                    TELEGRAM_MSG_LIMIT
                ));

                // ── Send only remaining delta (unified rolling placeholder) ──
                if full_response.len() < last_confirmed_len
                    || !full_response.is_char_boundary(last_confirmed_len)
                {
                    last_confirmed_len = 0;
                }
                let remaining = &full_response[last_confirmed_len..];
                msg_debug(&format!("[rolling_ph/botmsg] FINAL: placeholder_msg_id={}, confirmed={}, total={}, remaining_len={}",
                    placeholder_msg_id, last_confirmed_len, full_response.len(), remaining.trim().len()));
                if was_originally_empty || remaining.trim().is_empty() {
                    msg_debug(&format!(
                        "[rolling_ph/botmsg] FINAL DELETE placeholder: msg_id={}",
                        placeholder_msg_id
                    ));
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!(
                        "delete_message",
                        bot_owned.delete_message(chat_id, placeholder_msg_id).await
                    );
                } else if should_attach_response_as_file(full_response.len(), provider_str) {
                    msg_debug(&format!(
                        "[rolling_ph/botmsg] FINAL FILE ATTACH: total={}",
                        full_response.len()
                    ));
                    shared_rate_limit_wait(&state_owned, chat_id).await;
                    let _ = tg!(
                        "edit_message",
                        bot_owned
                            .edit_message_text(
                                chat_id,
                                placeholder_msg_id,
                                "\u{1f4c4} Response attached as file"
                            )
                            .await
                    );
                    send_response_as_file(
                        &bot_owned,
                        chat_id,
                        &final_response,
                        &state_owned,
                        "botmsg",
                    )
                    .await;
                } else {
                    let normalized_remaining = normalize_empty_lines(remaining);
                    let html_remaining = markdown_to_telegram_html(&normalized_remaining);
                    msg_debug(&format!(
                        "[botmsg_poll:{}] final delta_len={}, html_len={}",
                        bmsg_id_for_log,
                        normalized_remaining.len(),
                        html_remaining.len()
                    ));
                    if html_remaining.len() <= TELEGRAM_MSG_LIMIT {
                        if let Err(e) = tg!(
                            "edit_message",
                            bot_owned
                                .edit_message_text(chat_id, placeholder_msg_id, &html_remaining)
                                .parse_mode(ParseMode::Html)
                                .await
                        ) {
                            msg_debug(&format!(
                                "[botmsg_poll:{}] HTML edit failed: {}",
                                bmsg_id_for_log, e
                            ));
                            shared_rate_limit_wait(&state_owned, chat_id).await;
                            let _ = tg!(
                                "edit_message",
                                bot_owned
                                    .edit_message_text(
                                        chat_id,
                                        placeholder_msg_id,
                                        &normalized_remaining
                                    )
                                    .await
                            );
                        }
                    } else {
                        let send_result = send_long_message(
                            &bot_owned,
                            chat_id,
                            &html_remaining,
                            Some(ParseMode::Html),
                            &state_owned,
                        )
                        .await;
                        match send_result {
                            Ok(_) => {
                                shared_rate_limit_wait(&state_owned, chat_id).await;
                                let _ = tg!(
                                    "delete_message",
                                    bot_owned.delete_message(chat_id, placeholder_msg_id).await
                                );
                            }
                            Err(_) => {
                                let fallback = send_long_message(
                                    &bot_owned,
                                    chat_id,
                                    &normalized_remaining,
                                    None,
                                    &state_owned,
                                )
                                .await;
                                match fallback {
                                    Ok(_) => {
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let _ = tg!(
                                            "delete_message",
                                            bot_owned
                                                .delete_message(chat_id, placeholder_msg_id)
                                                .await
                                        );
                                    }
                                    Err(_) => {
                                        shared_rate_limit_wait(&state_owned, chat_id).await;
                                        let truncated =
                                            truncate_str(&normalized_remaining, TELEGRAM_MSG_LIMIT);
                                        let _ = tg!(
                                            "edit_message",
                                            bot_owned
                                                .edit_message_text(
                                                    chat_id,
                                                    placeholder_msg_id,
                                                    &truncated
                                                )
                                                .await
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                // Update session state
                {
                    let mut data = state_owned.lock().await;
                    // Same guard as text-message polling: skip session writeback if /model or
                    // /start changed the session during this bot-to-bot task. detect_provider
                    // mirrors the spawn-time capture (see normal text-msg guard for rationale).
                    // sid + clear_epoch catch /clear and /start same-path swaps.
                    let model_now = data.settings.models.get(&chat_id.0.to_string()).cloned();
                    let provider_now = detect_provider(model_now.as_deref());
                    let path_now = data
                        .sessions
                        .get(&chat_id)
                        .and_then(|s| s.current_path.clone());
                    let sid_now = data
                        .sessions
                        .get(&chat_id)
                        .and_then(|s| s.session_id.clone());
                    let epoch_now = data.clear_epoch.get(&chat_id).copied().unwrap_or(0);
                    let session_changed = provider_now != provider_str
                        || path_now.as_deref() != Some(current_path_owned.as_str())
                        || sid_now != captured_sid
                        || epoch_now != captured_clear_epoch;
                    if session_changed {
                        msg_debug(&format!("[botmsg_poll:{}] session changed during task — skip session update (path: {:?} → {:?}, provider: {} → {}, sid: {:?} → {:?}, epoch: {} → {})",
                            bmsg_id_for_log, current_path_owned, path_now, provider_str, provider_now, captured_sid, sid_now, captured_clear_epoch, epoch_now));
                    } else if let Some(session) = data.sessions.get_mut(&chat_id) {
                        msg_debug(&format!("[botmsg_poll:{}] updating session: new_session_id={:?}, history_len_before={}",
                            bmsg_id_for_log, new_session_id, session.history.len()));
                        if let Some(sid) = new_session_id.take() {
                            session.session_id = Some(sid);
                        }
                        session.history.push(HistoryItem {
                            item_type: HistoryType::User,
                            content: prompt_owned.clone(),
                        });
                        // Skip the assistant push when the AI produced no output —
                        // mirrors the UI's silent-delete behavior so saved history
                        // does not contain a sentinel string the user never saw.
                        if !was_originally_empty {
                            session.history.push(HistoryItem {
                                item_type: HistoryType::Assistant,
                                content: final_response.clone(),
                            });
                        }
                        save_session_to_file(session, &current_path_owned, provider_str);
                        msg_debug(&format!(
                            "[botmsg_poll:{}] session saved: history_len_after={}",
                            bmsg_id_for_log,
                            session.history.len()
                        ));
                    } else {
                        msg_debug(&format!("[botmsg_poll:{}] no session found for chat_id={}, skipping session update", bmsg_id_for_log, chat_id.0));
                    }
                    // Write to group chat shared log (bot-to-bot messages)
                    msg_debug(&format!(
                        "[botmsg_poll:{}] JSONL check: chat_id={}, raw_entries_count={}",
                        bmsg_id_for_log,
                        chat_id.0,
                        raw_entries.len()
                    ));
                    if chat_id.0 < 0 {
                        let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                        let dn = if bot_display_name_for_log.is_empty() {
                            None
                        } else {
                            Some(bot_display_name_for_log.clone())
                        };
                        append_group_chat_log(
                            chat_id.0,
                            &GroupChatLogEntry {
                                ts: now_ts.clone(),
                                bot: bot_username_for_log.clone(),
                                bot_display_name: dn.clone(),
                                role: "user".to_string(),
                                from: Some(format!("bot:{}", from_bot_for_log)),
                                text: prompt_owned.clone(),
                                clear: false,
                            },
                        );
                        if !raw_entries.is_empty() {
                            msg_debug(&format!("[botmsg_poll:{}] JSONL: writing user+assistant entries, raw_entries_count={}", bmsg_id_for_log, raw_entries.len()));
                            append_group_chat_log(
                                chat_id.0,
                                &GroupChatLogEntry {
                                    ts: now_ts,
                                    bot: bot_username_for_log.clone(),
                                    bot_display_name: dn,
                                    role: "assistant".to_string(),
                                    from: None,
                                    text: serialize_payload(&std::mem::take(&mut raw_entries)),
                                    clear: false,
                                },
                            );
                        } else {
                            msg_debug(&format!("[botmsg_poll:{}] JSONL: user entry written, assistant SKIPPED (raw_entries is empty)", bmsg_id_for_log));
                        }
                    }
                }

                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ▶ [BotMsg] Response sent");
                msg_debug(&format!(
                    "[botmsg_poll:{}] response sent, final_response_len={}",
                    bmsg_id_for_log,
                    final_response.len()
                ));

                // Do NOT auto-create response message file.
                // The AI can use --message CLI to reply if needed.
                // Auto-responding causes infinite ping-pong between bots.
                msg_debug(&format!(
                    "[botmsg_poll:{}] skipping auto-response (AI uses --message if needed)",
                    bmsg_id_for_log
                ));

                // Send end hook message if configured
                if !cancelled {
                    let end_hook_msg = {
                        let data = state_owned.lock().await;
                        data.settings.end_hook.get(&chat_id.0.to_string()).cloned()
                    };
                    if let Some(hook_msg) = end_hook_msg {
                        shared_rate_limit_wait(&state_owned, chat_id).await;
                        let _ = tg!(
                            "send_message",
                            bot_owned.send_message(chat_id, &hook_msg).await
                        );
                    }
                }
            }

            // Queue processing
            let queued = process_upload_queue(&bot_owned, chat_id, &state_owned).await;
            if done {
                queue_done = !queued;
                msg_debug(&format!(
                    "[botmsg_poll:{}] queue: queued={}, queue_done={}",
                    bmsg_id_for_log, queued, queue_done
                ));
            }
        }

        // Handle cancellation
        if cancelled {
            msg_debug(&format!(
                "[botmsg_poll:{}] handling cancellation: response_len={}",
                bmsg_id_for_log,
                full_response.len()
            ));
            // Peek the recorded PID for debug logging, then re-send SIGKILL
            // as a safety belt (cancel_now is idempotent).
            {
                let pid_opt = *cancel_token
                    .child_pid
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                match pid_opt {
                    Some(pid) => msg_debug(&format!(
                        "[botmsg_poll:{}] killing child process: pid={}",
                        bmsg_id_for_log, pid
                    )),
                    None => msg_debug(&format!(
                        "[botmsg_poll:{}] no child pid to kill",
                        bmsg_id_for_log
                    )),
                }
            }
            cancel_token.cancel_now();

            // stopped_response (full) for session history
            let stopped_response = if full_response.trim().is_empty() {
                "[Stopped]".to_string()
            } else {
                let normalized = normalize_empty_lines(&full_response);
                format!("{}\n\n[Stopped]", normalized)
            };
            msg_debug(&format!(
                "[botmsg_poll:{}] stopped_response_len={}",
                bmsg_id_for_log,
                stopped_response.len()
            ));

            shared_rate_limit_wait(&state_owned, chat_id).await;

            // ── Show remaining delta + [Stopped] (unified rolling placeholder) ──
            if full_response.len() < last_confirmed_len
                || !full_response.is_char_boundary(last_confirmed_len)
            {
                last_confirmed_len = 0;
            }
            let remaining = &full_response[last_confirmed_len..];
            msg_debug(&format!("[rolling_ph/botmsg] STOPPED: placeholder_msg_id={}, confirmed={}, remaining_len={}",
                placeholder_msg_id, last_confirmed_len, remaining.trim().len()));
            if should_attach_response_as_file(full_response.len(), provider_str) {
                msg_debug(&format!(
                    "[rolling_ph/botmsg] STOPPED FILE ATTACH: total={}",
                    full_response.len()
                ));
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!(
                    "edit_message",
                    bot_owned
                        .edit_message_text(
                            chat_id,
                            placeholder_msg_id,
                            "\u{1f4c4} Response attached as file [Stopped]"
                        )
                        .await
                );
                send_response_as_file(
                    &bot_owned,
                    chat_id,
                    &stopped_response,
                    &state_owned,
                    "botmsg",
                )
                .await;
            } else {
                let display_stopped = if remaining.trim().is_empty() {
                    "[Stopped]".to_string()
                } else {
                    let normalized = normalize_empty_lines(remaining);
                    format!("{}\n\n[Stopped]", normalized)
                };
                let html_stopped = markdown_to_telegram_html(&display_stopped);
                if html_stopped.len() <= TELEGRAM_MSG_LIMIT {
                    let _ = tg!(
                        "edit_message",
                        bot_owned
                            .edit_message_text(chat_id, placeholder_msg_id, &html_stopped)
                            .parse_mode(ParseMode::Html)
                            .await
                    );
                } else {
                    let _ = tg!(
                        "edit_message",
                        bot_owned
                            .edit_message_text(
                                chat_id,
                                placeholder_msg_id,
                                &truncate_str(&display_stopped, TELEGRAM_MSG_LIMIT)
                            )
                            .await
                    );
                }
            }

            // Do NOT create response file on cancel → chain broken
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ [BotMsg] Stopped");
            msg_debug(&format!(
                "[botmsg_poll:{}] stopped message sent, updating session",
                bmsg_id_for_log
            ));

            let mut data = state_owned.lock().await;
            // Same guard: skip the partial-response writeback if the session was reset
            // (via /model or /start) during this bot-to-bot task. Group chat log is still
            // written below — mirrors the normal-completion branch so other bots see the
            // partial response. detect_provider mirrors the spawn-time capture.
            // sid + clear_epoch catch /clear and /start same-path swaps.
            let model_now = data.settings.models.get(&chat_id.0.to_string()).cloned();
            let provider_now = detect_provider(model_now.as_deref());
            let path_now = data
                .sessions
                .get(&chat_id)
                .and_then(|s| s.current_path.clone());
            let sid_now = data
                .sessions
                .get(&chat_id)
                .and_then(|s| s.session_id.clone());
            let epoch_now = data.clear_epoch.get(&chat_id).copied().unwrap_or(0);
            let session_changed = provider_now != provider_str
                || path_now.as_deref() != Some(current_path_owned.as_str())
                || sid_now != captured_sid
                || epoch_now != captured_clear_epoch;
            if session_changed {
                msg_debug(&format!("[botmsg_poll:{}] cancel: session changed during task — skip session writeback (path: {:?} → {:?}, provider: {} → {}, sid: {:?} → {:?}, epoch: {} → {})",
                    bmsg_id_for_log, current_path_owned, path_now, provider_str, provider_now, captured_sid, sid_now, captured_clear_epoch, epoch_now));
            } else if let Some(session) = data.sessions.get_mut(&chat_id) {
                msg_debug(&format!(
                    "[botmsg_poll:{}] cancel: updating session history, new_session_id={:?}",
                    bmsg_id_for_log, new_session_id
                ));
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
            } else {
                msg_debug(&format!(
                    "[botmsg_poll:{}] cancel: no session found for chat_id={}",
                    bmsg_id_for_log, chat_id.0
                ));
            }
            // Write to group chat shared log (bot-to-bot messages, stopped).
            // Mirrors the normal-completion branch: written regardless of session_changed.
            msg_debug(&format!(
                "[botmsg_poll:{}] JSONL stopped check: chat_id={}, raw_entries_count={}",
                bmsg_id_for_log,
                chat_id.0,
                raw_entries.len()
            ));
            if chat_id.0 < 0 {
                let now_ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                let dn = if bot_display_name_for_log.is_empty() {
                    None
                } else {
                    Some(bot_display_name_for_log.clone())
                };
                append_group_chat_log(
                    chat_id.0,
                    &GroupChatLogEntry {
                        ts: now_ts.clone(),
                        bot: bot_username_for_log.clone(),
                        bot_display_name: dn.clone(),
                        role: "user".to_string(),
                        from: Some(format!("bot:{}", from_bot_for_log)),
                        text: prompt_owned,
                        clear: false,
                    },
                );
                if !raw_entries.is_empty() {
                    msg_debug(&format!("[botmsg_poll:{}] JSONL stopped: writing user+assistant entries, raw_entries_count={}", bmsg_id_for_log, raw_entries.len()));
                    append_group_chat_log(
                        chat_id.0,
                        &GroupChatLogEntry {
                            ts: now_ts,
                            bot: bot_username_for_log.clone(),
                            bot_display_name: dn,
                            role: "assistant".to_string(),
                            from: None,
                            text: serialize_payload(&std::mem::take(&mut raw_entries)),
                            clear: false,
                        },
                    );
                } else {
                    msg_debug(&format!("[botmsg_poll:{}] JSONL stopped: user entry written, assistant SKIPPED (raw_entries is empty)", bmsg_id_for_log));
                }
            }
            remove_cancel_token_locked(&mut data, chat_id);
            let stop_msg_id = data.stop_message_ids.remove(&chat_id);
            drop(data);
            if let Some(msg_id) = stop_msg_id {
                shared_rate_limit_wait(&state_owned, chat_id).await;
                let _ = tg!(
                    "delete_message",
                    bot_owned.delete_message(chat_id, msg_id).await
                );
            }
            msg_debug(&format!(
                "[botmsg_poll:{}] cancel cleanup done",
                bmsg_id_for_log
            ));
            msg_debug(&format!(
                "[queue:trigger] chat_id={}, source=botmsg_poll_cancelled",
                chat_id.0
            ));
            drop(_group_lock); // release group chat lock before processing queue
            process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
            return;
        }

        // Clean up cancel token
        msg_debug(&format!(
            "[botmsg_poll:{}] normal completion, cleaning up",
            bmsg_id_for_log
        ));
        let orphan_stop_msg = {
            let mut data = state_owned.lock().await;
            let msg_id = data.stop_message_ids.remove(&chat_id);
            remove_cancel_token_locked(&mut data, chat_id);
            msg_debug(&format!(
                "[botmsg_poll:{}] cleaned up: orphan_stop_msg={:?}",
                bmsg_id_for_log, msg_id
            ));
            msg_id
        };
        if let Some(msg_id) = orphan_stop_msg {
            msg_debug(&format!(
                "[botmsg_poll:{}] deleting orphan stop message: {}",
                bmsg_id_for_log, msg_id
            ));
            shared_rate_limit_wait(&state_owned, chat_id).await;
            let _ = tg!(
                "delete_message",
                bot_owned.delete_message(chat_id, msg_id).await
            );
        }
        msg_debug(&format!("[botmsg_poll:{}] END", bmsg_id_for_log));
        msg_debug(&format!(
            "[queue:trigger] chat_id={}, source=botmsg_poll_completed",
            chat_id.0
        ));
        drop(_group_lock); // release group chat lock before processing queue
        process_next_queued_message(&bot_owned, chat_id, &state_owned).await;
    });
    msg_debug(&format!(
        "[process_bot_message] END (tasks spawned) id={}",
        msg.id
    ));
}

/// Scheduler loop: runs every 60 seconds, checks for due schedules
async fn scheduler_loop(
    bot: Bot,
    state: SharedState,
    token: String,
    bot_username: String,
    bot_display_name: String,
) {
    let bot_key = token_hash(&token);
    sched_debug("[scheduler_loop] started");

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        // Each cycle runs in its own child task so any panic inside the cycle
        // (chrono parse, Telegram API call, file IO, etc.) only kills that
        // cycle. The scheduler keeps running on the next 5-second tick. Per-
        // dispatch state (busy slots, sessions, pending) is already protected
        // by the inner `execute_schedule` / `process_bot_message` recovery
        // paths, so a cycle-level panic mostly affects in-progress logging
        // and the current iteration of the entries/messages loops.
        let bot_c = bot.clone();
        let state_c = state.clone();
        let token_c = token.clone();
        let bot_key_c = bot_key.clone();
        let bot_username_c = bot_username.clone();
        let bot_display_name_c = bot_display_name.clone();
        let cycle_join = tokio::spawn(async move {
            scheduler_cycle(
                bot_c,
                state_c,
                token_c,
                bot_key_c,
                bot_username_c,
                bot_display_name_c,
            )
            .await
        });
        if let Err(join_err) = cycle_join.await {
            let panic_str = redact_known_tokens(&join_err.to_string());
            msg_debug(&format!("[scheduler_loop] cycle PANICKED: {}", panic_str));
            eprintln!(
                "  ⚠ Scheduler cycle panicked: {} — continuing on next tick",
                panic_str
            );
        }
    }
}

async fn scheduler_cycle(
    bot: Bot,
    state: SharedState,
    token: String,
    bot_key: String,
    bot_username: String,
    bot_display_name: String,
) {
    // Scan schedule directory
    let entries = list_schedule_entries(&bot_key, None);

    if !entries.is_empty() {
        sched_debug(&format!(
            "[scheduler_loop] cycle: {} entries found",
            entries.len()
        ));
    }

    for entry in &entries {
        let chat_id = ChatId(entry.chat_id);

        // Verify current_path exists (before acquiring lock — involves filesystem I/O)
        if !Path::new(&entry.current_path).is_dir() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ [Scheduler] Path not found: {} (schedule: {})",
                entry.current_path, entry.id
            );
            sched_debug(&format!(
                "[scheduler_loop] id={}, path not found: {} → skip",
                entry.id, entry.current_path
            ));
            shared_rate_limit_wait(&state, chat_id).await;
            let msg = format!(
                "⏰ {}\n\n⚠️ Skipped — path no longer exists\n📂 <code>{}</code>",
                html_escape(&truncate_str(&entry.prompt, 40)),
                html_escape(&entry.current_path)
            );
            let _ = tg!(
                "send_message",
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::Html)
                    .await
            );
            continue;
        }

        // Single atomic lock: pending check + trigger check + busy check + session backup
        // All checks in one lock to prevent race between pending cleanup and re-trigger
        enum SchedAction {
            Skip,
            DiscardExpired,
            Execute(Option<ChatSession>, u64),
            /// Inline mode: run the scheduled task inside the chat's current
            /// session (no backup, no isolated workspace). Carries dispatch_id only —
            /// the existing session is left in place.
            ExecuteInline(u64),
        }
        let inline_env_on = is_schedule_inline_enabled();

        let action = {
            let mut data = state.lock().await;
            let is_already_pending = data
                .pending_schedules
                .get(&chat_id)
                .map_or(false, |set| set.contains(&entry.id));

            sched_debug(&format!(
                "[scheduler_loop] id={}, is_already_pending={}",
                entry.id, is_already_pending
            ));

            // If not pending and not due to trigger, skip
            if !is_already_pending && !should_trigger(entry) {
                // Check if expired absolute schedule should be discarded
                if entry.schedule_type == "absolute" {
                    if let Ok(schedule_time) =
                        chrono::NaiveDateTime::parse_from_str(&entry.schedule, "%Y-%m-%d %H:%M:%S")
                    {
                        if let Some(schedule_dt) =
                            schedule_time.and_local_timezone(chrono::Local).single()
                        {
                            if chrono::Local::now() > schedule_dt {
                                sched_debug(&format!(
                                    "[scheduler_loop] id={}, expired absolute → discard",
                                    entry.id
                                ));
                                SchedAction::DiscardExpired
                            } else {
                                sched_debug(&format!(
                                    "[scheduler_loop] id={}, not yet due → skip",
                                    entry.id
                                ));
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
                sched_debug(&format!(
                    "[scheduler_loop] id={}, should execute, is_busy={}",
                    entry.id, is_busy
                ));

                if is_busy {
                    // Chat is busy — mark as pending if not already, retry next cycle
                    // Do NOT touch sessions — leave them as-is
                    if !is_already_pending {
                        data.pending_schedules
                            .entry(chat_id)
                            .or_default()
                            .insert(entry.id.clone());
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] ⏰ [Scheduler] Chat busy, pending: {}", entry.id);
                        sched_debug(&format!(
                            "[scheduler_loop] id={}, chat busy → marked pending",
                            entry.id
                        ));
                    } else {
                        sched_debug(&format!(
                            "[scheduler_loop] id={}, chat busy, already pending → skip",
                            entry.id
                        ));
                    }
                    SchedAction::Skip
                } else {
                    // Inline mode requires an active chat session with a current_path;
                    // otherwise fall back to isolated mode so the schedule still runs.
                    let inline_usable = inline_env_on
                        && data
                            .sessions
                            .get(&chat_id)
                            .and_then(|s| s.current_path.as_ref())
                            .is_some();
                    let dispatch_id = next_dispatch_id();
                    if inline_usable {
                        sched_debug(&format!("[scheduler_loop] id={}, inline mode → execute in current session (dispatch_id={})", entry.id, dispatch_id));
                        data.pending_schedules
                            .entry(chat_id)
                            .or_default()
                            .insert(entry.id.clone());
                        let cancel_token = new_cancel_token_for_owner(dispatch_id);
                        insert_cancel_token_locked(&mut data, chat_id, cancel_token);
                        SchedAction::ExecuteInline(dispatch_id)
                    } else {
                        // Not busy — backup session, replace with schedule-specific session, and execute
                        let prev = data.sessions.get(&chat_id).cloned();
                        sched_debug(&format!("[scheduler_loop] id={}, not busy → execute (has_prev_session={}, dispatch_id={})", entry.id, prev.is_some(), dispatch_id));
                        data.sessions.insert(
                            chat_id,
                            ChatSession {
                                session_id: None,
                                current_path: Some(entry.current_path.clone()),
                                history: Vec::new(),
                                pending_uploads: Vec::new(),
                                pending_uploads_added_at: None,
                            },
                        );
                        data.pending_schedules
                            .entry(chat_id)
                            .or_default()
                            .insert(entry.id.clone());
                        // Pre-insert cancel_token tagged with this dispatch_id so a panic
                        // inside execute_schedule can be safely reclaimed by the owner
                        // check in reclaim_panicked_dispatch_token.
                        let cancel_token = new_cancel_token_for_owner(dispatch_id);
                        insert_cancel_token_locked(&mut data, chat_id, cancel_token);
                        SchedAction::Execute(prev, dispatch_id)
                    }
                }
            }
        };

        let (inline_mode, dispatch_id, prev_session) = match action {
            SchedAction::Skip => continue,
            SchedAction::DiscardExpired => {
                delete_schedule_entry(&entry.id);
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ⏰ [Scheduler] Discarded expired once-schedule: {}",
                    entry.id
                );
                sched_debug(&format!(
                    "[scheduler_loop] id={}, discarded expired",
                    entry.id
                ));
                // Trailing `continue` (no semicolon) makes this block's type `!`,
                // which coerces cleanly to the tuple type of the Execute arms in
                // match-arm type unification. With a semicolon the block would be
                // syntactically `()` and the arms would fail to unify.
                continue;
            }
            SchedAction::Execute(prev, did) => (false, did, prev),
            SchedAction::ExecuteInline(did) => (true, did, None),
        };
        sched_debug(&format!(
            "[scheduler_loop] id={}, calling execute_schedule (dispatch_id={}, inline={})",
            entry.id, dispatch_id, inline_mode
        ));
        // Run inside CURRENT_DISPATCH_ID scope and inside a child spawn so
        // a panic inside execute_schedule kills only that one task — the
        // scheduler keeps running and the busy slot is reclaimed via the
        // owner_id check. Session and pending_schedules state inserted
        // by the pre-execute lock above are also restored on panic so
        // the chat does not see an empty session or a stuck pending id.
        let bot_c = bot.clone();
        let state_c = state.clone();
        let token_c = token.clone();
        let entry_c = entry.clone();
        let entry_id_for_log = entry.id.clone();
        let prev_for_recover = prev_session.clone();
        let join = tokio::spawn(async move {
            CURRENT_DISPATCH_ID
                .scope(dispatch_id, async move {
                    execute_schedule(
                        &bot_c,
                        chat_id,
                        &entry_c,
                        &state_c,
                        &token_c,
                        prev_session,
                        inline_mode,
                    )
                    .await
                })
                .await
        });
        if let Err(join_err) = join.await {
            let panic_str = redact_known_tokens(&join_err.to_string());
            msg_debug(&format!(
                "[scheduler_loop] id={}, execute_schedule PANICKED: {}",
                entry_id_for_log, panic_str
            ));
            eprintln!(
                "  ⚠ Chat {} schedule {} panicked: {} — recovering",
                chat_id.0, entry_id_for_log, panic_str
            );
            reclaim_panicked_dispatch_token(&state, chat_id, dispatch_id, "scheduler:execute")
                .await;
            // Restore session/pending state that the pre-execute lock
            // had mutated on this dispatch's behalf. If the panic
            // happened after execute_schedule already restored these
            // (e.g. very late in cleanup), the writes here are
            // overwriting equivalent state — safe and idempotent.
            // Inline mode never mutated `sessions` upfront, so we only
            // touch sessions on the isolated path.
            let mut data = state.lock().await;
            if !inline_mode {
                match prev_for_recover {
                    Some(prev) => {
                        data.sessions.insert(chat_id, prev);
                    }
                    None => {
                        data.sessions.remove(&chat_id);
                    }
                }
            }
            if let Some(set) = data.pending_schedules.get_mut(&chat_id) {
                set.remove(&entry_id_for_log);
                if set.is_empty() {
                    data.pending_schedules.remove(&chat_id);
                }
            }
        }
    }

    // === Bot-to-bot message polling ===
    if !bot_username.is_empty() {
        let messages = scan_messages(&bot_username);
        if !messages.is_empty() {
            msg_debug(&format!(
                "[scheduler_loop] bot messages found: {} for @{}",
                messages.len(),
                bot_username
            ));
        }
        for msg in &messages {
            msg_debug(&format!("[scheduler_loop] bot message: id={}, from={}, to={}, chat_id={}, content_len={}, created_at={}",
                    msg.id, msg.from, msg.to, msg.chat_id, msg.content.len(), msg.created_at));
            let chat_id_num: i64 = match msg.chat_id.parse() {
                Ok(n) => n,
                Err(e) => {
                    msg_debug(&format!("[scheduler_loop] invalid chat_id in message: id={}, chat_id={:?}, error={}", msg.id, msg.chat_id, e));
                    let remove_result = fs::remove_file(&msg.file_path);
                    msg_debug(&format!(
                        "[scheduler_loop] removed invalid message file: ok={}",
                        remove_result.is_ok()
                    ));
                    continue;
                }
            };
            let chat_id = ChatId(chat_id_num);

            // Busy check: skip if cancel_token exists for this chat
            {
                let data = state.lock().await;
                let is_busy = data.cancel_tokens.contains_key(&chat_id);
                msg_debug(&format!(
                    "[scheduler_loop] busy check for chat {}: is_busy={}",
                    chat_id_num, is_busy
                ));
                if is_busy {
                    msg_debug(&format!("[scheduler_loop] chat {} busy, skipping message: {} (will retry next cycle)", chat_id_num, msg.id));
                    continue;
                }
            }

            // Note: file deletion happens inside `process_bot_message` only
            // after the placeholder send succeeds. Deleting here (or at claim
            // time) lost the message on every interim failure path
            // (cancel-during-lock-wait, no-session, placeholder send error)
            // because none of those branches re-queue the file.
            msg_debug(&format!(
                "[scheduler_loop] processing bot message: {} (from={}, to={}, chat_id={})",
                msg.id, msg.from, msg.to, chat_id_num
            ));

            // Process the message (will delete the file iff its claim succeeds).
            // Run inside CURRENT_DISPATCH_ID scope and a child spawn so a panic
            // inside process_bot_message does not take down the scheduler — the
            // owner_id check then reclaims the busy slot inserted by this dispatch.
            let dispatch_id = next_dispatch_id();
            let bot_c = bot.clone();
            let state_c = state.clone();
            let token_c = token.clone();
            let bot_username_c = bot_username.clone();
            let bot_display_name_c = bot_display_name.clone();
            let msg_c = msg.clone();
            let msg_id_for_log = msg.id.clone();
            let join = tokio::spawn(async move {
                CURRENT_DISPATCH_ID
                    .scope(dispatch_id, async move {
                        process_bot_message(
                            &bot_c,
                            chat_id,
                            &msg_c,
                            &state_c,
                            &token_c,
                            &bot_username_c,
                            &bot_display_name_c,
                        )
                        .await
                    })
                    .await
            });
            if let Err(join_err) = join.await {
                let panic_str = redact_known_tokens(&join_err.to_string());
                msg_debug(&format!(
                    "[scheduler_loop] process_bot_message id={} PANICKED: {}",
                    msg_id_for_log, panic_str
                ));
                eprintln!(
                    "  ⚠ Chat {} bot-message {} panicked: {} — recovering",
                    chat_id.0, msg_id_for_log, panic_str
                );
                reclaim_panicked_dispatch_token(&state, chat_id, dispatch_id, "scheduler:botmsg")
                    .await;
            }
            msg_debug(&format!(
                "[scheduler_loop] process_bot_message returned for msg: {}",
                msg_id_for_log
            ));
        }

        // Check for timed-out sent messages
        msg_debug("[scheduler_loop] checking message timeouts");
        check_message_timeouts(&bot, &bot_username, &state).await;
    }
}
