use std::sync::atomic::Ordering;

use teloxide::prelude::*;
use teloxide::types::ParseMode;

use super::{ChatSession, SharedState, tg};
use super::storage::{delete_session_file, save_bot_settings};
use super::streaming::{send_long_message, shared_rate_limit_wait};

/// Handle /help command
pub(crate) async fn handle_help_command(
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
pub(crate) async fn handle_start_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    use std::path::Path;
    use std::fs;
    use super::storage::load_existing_session;
    use crate::ui::ai_screen::HistoryType;

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
pub(crate) async fn handle_clear_command(
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

    let session_id_to_delete = {
        let mut data = state.lock().await;
        let session_id = data.sessions.get(&chat_id)
            .and_then(|s| s.session_id.clone());
        if let Some(session) = data.sessions.get_mut(&chat_id) {
            session.session_id = None;
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
        data.cancel_tokens.remove(&chat_id);
        data.stop_message_ids.remove(&chat_id);
        session_id
    };

    // Also remove the session file from disk
    if let Some(ref sid) = session_id_to_delete {
        delete_session_file(sid);
    }

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, "Session cleared.")
        .await)?;

    Ok(())
}

/// Handle /pwd command - show current session path
pub(crate) async fn handle_pwd_command(
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
pub(crate) async fn handle_stop_command(
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

/// Handle /public command - toggle public access for group chats
pub(crate) async fn handle_public_command(
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

/// Handle /debug command - toggle API debug logging
pub(crate) async fn handle_debug_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let prev = super::TG_DEBUG.load(Ordering::Relaxed);
    let next = !prev;
    super::TG_DEBUG.store(next, Ordering::Relaxed);
    let status = if next { "ON" } else { "OFF" };
    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, format!("🔍 Debug logging: {status}"))
        .await)?;
    Ok(())
}

/// Handle /setpollingtime command - set Telegram API polling interval
pub(crate) async fn handle_setpollingtime_command(
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

