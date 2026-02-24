use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::fs;
use std::path::Path;

use teloxide::prelude::*;
use teloxide::types::ParseMode;

use crate::services::claude::CancelToken;
use crate::ui::ai_screen::{HistoryItem, HistoryType};

use super::{SharedState, tg};
use super::storage::{save_session_to_file, token_hash};
use super::streaming::{html_escape, shared_rate_limit_wait};

/// Shell command output message type
pub(crate) enum ShellOutput {
    Line(String),
    Done { exit_code: i32 },
    Error(String),
}

/// Handle /down <filepath> - send file to user
pub(crate) async fn handle_down_command(
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
pub(crate) async fn handle_file_upload(
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

/// Handle !command - execute shell command directly with lock/stop/streaming support
pub(crate) async fn handle_shell_command(
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

/// Process one pending upload queue file for the given chat.
/// Scans ~/.cokacdir/upload_queue/ for .queue files matching the current bot and chat_id,
/// sends the oldest one, and deletes the queue file on success.
/// Returns true if a file was processed (rate limit slot consumed).
pub(crate) async fn process_upload_queue(bot: &Bot, chat_id: ChatId, state: &SharedState) -> bool {
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
