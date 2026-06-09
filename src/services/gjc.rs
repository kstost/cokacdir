use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{mpsc::Sender, Arc, OnceLock};

use crate::services::claude::{debug_log_to, kill_child_tree, CancelToken, StreamMessage};
use crate::services::gjc_events::parse_gjc_event;
use crate::services::gjc_path::resolve_gjc_path;
use crate::services::gjc_sessions::resumable_session_id;

static GJC_BIN: OnceLock<Option<String>> = OnceLock::new();

fn gjc_bin() -> Option<&'static String> {
    GJC_BIN.get_or_init(resolve_gjc_path).as_ref()
}

pub fn is_gjc_available() -> bool {
    gjc_bin().is_some()
}

pub fn is_gjc_model(model: Option<&str>) -> bool {
    model
        .map(|m| m == "gjc" || m.starts_with("gjc:"))
        .unwrap_or(false)
}

pub fn strip_gjc_prefix(model: &str) -> Option<&str> {
    model
        .strip_prefix("gjc:")
        .filter(|s| !s.is_empty())
        .map(|s| s.split(" \u{2014} ").next().unwrap_or(s).trim())
}

pub fn build_gjc_args(
    session_id: Option<&str>,
    system_prompt_file: Option<&str>,
    model: Option<&str>,
    no_session_persistence: bool,
) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        "--mode".into(),
        "json".into(),
        "--no-title".into(),
    ];
    if no_session_persistence {
        args.push("--no-session".into());
    }
    if let Some(path) = system_prompt_file.filter(|p| !p.is_empty()) {
        args.push("--append-system-prompt".into());
        args.push(path.to_string());
    }
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.to_string());
    }
    if let Some(sid) = session_id {
        args.push("--resume".into());
        args.push(sid.to_string());
    }
    args
}

fn write_temp_text(prefix: &str, label: &str, text: &str) -> Result<std::path::PathBuf, String> {
    let dir = dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".cokacdir");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create temp dir: {}", e))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = dir.join(format!("{prefix}_{nanos}_{}", std::process::id()));
    std::fs::write(&path, text).map_err(|e| format!("Failed to write {label}: {}", e))?;
    Ok(path)
}

struct TempFileGuard(Option<std::path::PathBuf>);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    _allowed_tools: Option<&[String]>,
    cancel_token: Option<Arc<CancelToken>>,
    model: Option<&str>,
    no_session_persistence: bool,
) -> Result<(), String> {
    if let Some(sid) = session_id {
        if !crate::services::process::is_valid_session_id(sid) {
            return Err(format!("Invalid session_id format: {}", sid));
        }
    }

    let resume_session_id = resumable_session_id(session_id);
    if session_id.is_some() && resume_session_id.is_none() {
        debug_log_to(
            "gjc.log",
            &format!(
                "[execute] skipping --resume for non-persisted Gajae-Code session {:?}",
                session_id
            ),
        );
    }

    let sp_path = match system_prompt {
        Some("") | None => None,
        Some(text) => Some(write_temp_text("gjc_sp", "system prompt", text)?),
    };
    let _sp_guard = TempFileGuard(sp_path.clone());
    let prompt_path = write_temp_text("gjc_prompt", "prompt", prompt)?;
    let _prompt_guard = TempFileGuard(Some(prompt_path.clone()));
    let mut args = build_gjc_args(
        resume_session_id,
        sp_path.as_ref().and_then(|p| p.to_str()),
        model,
        no_session_persistence,
    );
    args.push(format!("@{}", prompt_path.to_string_lossy()));
    let bin = gjc_bin().cloned().unwrap_or_else(|| "gjc".to_string());
    let mut logged_args = args.clone();
    if let Some(last) = logged_args.last_mut() {
        *last = format!("<prompt:{} chars>", prompt.chars().count());
    }
    debug_log_to(
        "gjc.log",
        &format!("[execute] bin={} args={:?}", bin, logged_args),
    );

    let mut cmd = Command::new(&bin);
    cmd.args(&args)
        .current_dir(working_dir)
        .env("PATH", crate::services::claude::enhanced_path_for_bin(&bin))
        .env("PI_NO_TITLE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::services::claude::detach_into_own_pgroup(&mut cmd);
    crate::services::claude::attach_cancel_cgroup(&mut cmd, cancel_token.as_ref());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start Gajae-Code: {}", e))?;
    if let Some(token) = cancel_token.as_ref() {
        let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(child.id());
    }

    let stderr_thread = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default())
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Gajae-Code stdout".to_string())?;
    let reader = BufReader::new(stdout);
    let mut final_text = String::new();
    let mut saw_json = false;

    for line in reader.lines() {
        if cancel_token
            .as_ref()
            .map(|t| t.cancelled.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
        {
            kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
        let line = line.map_err(|e| format!("Failed to read Gajae-Code stdout: {}", e))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(json) => {
                saw_json = true;
                for msg in parse_gjc_event(&json) {
                    if let StreamMessage::Text { content } = &msg {
                        final_text.push_str(content);
                    }
                    let _ = sender.send(msg);
                }
            }
            Err(_) => {
                final_text.push_str(&line);
                final_text.push('\n');
            }
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("Gajae-Code process error: {}", e))?;
    let stderr = stderr_thread
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    if !status.success() {
        let _ = sender.send(StreamMessage::Error {
            message: format!("Gajae-Code exited with code {:?}", status.code()),
            stdout: final_text,
            stderr,
            exit_code: status.code(),
        });
        return Ok(());
    }
    if !saw_json && !final_text.trim().is_empty() {
        let _ = sender.send(StreamMessage::Text {
            content: final_text.trim().to_string(),
        });
    }
    let _ = sender.send(StreamMessage::Done {
        result: final_text,
        session_id: None,
    });
    Ok(())
}
