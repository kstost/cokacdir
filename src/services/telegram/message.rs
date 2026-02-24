use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::path::Path;

use teloxide::prelude::*;
use teloxide::types::ParseMode;

use crate::services::claude::{self, StreamMessage};
use crate::ui::ai_screen::{self, HistoryItem, HistoryType};

use super::{ChatSession, SharedState, tg};
use super::commands::{
    handle_help_command, handle_start_command, handle_clear_command,
    handle_pwd_command, handle_stop_command, handle_public_command,
    handle_debug_command, handle_setpollingtime_command,
};
use super::tools::{handle_availabletools_command, handle_allowedtools_command, handle_allowed_command};
use super::file_ops::{handle_down_command, handle_file_upload, handle_shell_command, process_upload_queue};
use super::storage::{load_existing_session, save_bot_settings, save_session_to_file, token_hash};
use super::streaming::{
    markdown_to_telegram_html, normalize_empty_lines, send_long_message,
    shared_rate_limit_wait, truncate_str,
};
use super::tools::{format_tool_input, get_allowed_tools};
use crate::services::claude::DEFAULT_ALLOWED_TOOLS;
use super::TELEGRAM_MSG_LIMIT;

/// Route incoming messages to appropriate handlers
pub(crate) async fn handle_message(
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
    let _ = imprinted; // Owner registration is logged to server console only

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
    let cancel_token = Arc::new(crate::services::claude::CancelToken::new());
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
                                    let ts = chrono::Local::now().format("%H:%M:%S");
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
