use std::collections::HashMap;
use std::fs;

use sha2::{Sha256, Digest};

use crate::ui::ai_screen::{self, HistoryItem, HistoryType, SessionData};

use super::{BotSettings, ChatSession};

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
pub(crate) fn load_bot_settings(token: &str) -> BotSettings {
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
pub(crate) fn save_bot_settings(token: &str, settings: &BotSettings) {
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

/// Load existing session from ai_sessions directory matching the given path
pub(crate) fn load_existing_session(current_path: &str) -> Option<(SessionData, std::time::SystemTime)> {
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
pub(crate) fn save_session_to_file(session: &ChatSession, current_path: &str) {
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

/// Remove session files older than `max_age_days` days from the sessions directory.
/// Silently ignores all errors (missing directory, unreadable metadata, etc.).
pub fn cleanup_stale_sessions(max_age_days: u64) {
    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(&sessions_dir) else {
        return;
    };
    let cutoff = std::time::Duration::from_secs(max_age_days * 24 * 60 * 60);
    let mut deleted = 0u32;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            if let Ok(metadata) = path.metadata() {
                if let Ok(modified) = metadata.modified() {
                    if let Ok(age) = std::time::SystemTime::now().duration_since(modified) {
                        if age > cutoff {
                            if fs::remove_file(&path).is_ok() {
                                deleted += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    if deleted > 0 {
        println!("  ✓ Cleaned up {deleted} stale session file(s) older than {max_age_days} days");
    }
}

/// Delete a specific session file by session_id.
/// Silently ignores errors (file may already be gone).
pub(crate) fn delete_session_file(session_id: &str) {
    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        return;
    };
    let file_path = sessions_dir.join(format!("{}.json", session_id));
    if let Some(parent) = file_path.parent() {
        if parent == sessions_dir {
            let _ = fs::remove_file(file_path);
        }
    }
}

