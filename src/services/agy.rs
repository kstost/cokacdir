//! Antigravity CLI (`agy`) provider.
//!
//! `agy --print` is a plain-stdout interface, not a Claude/Gemini-compatible
//! JSON event stream. This adapter synthesizes cokacdir's shared
//! `StreamMessage` contract from stdout and treats known stdout warnings/errors
//! as failures even when `agy` exits with status 0.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::services::claude::{
    debug_log_to, enhanced_path_for_bin, kill_child_tree, CancelToken, ClaudeResponse,
    StreamMessage,
};

static AGY_PATH: OnceLock<Option<String>> = OnceLock::new();
static AGY_VERSION: OnceLock<Option<String>> = OnceLock::new();
static AGY_MODELS: OnceLock<Vec<String>> = OnceLock::new();
static SESSION_OUTPUT_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn agy_debug(msg: &str) {
    debug_log_to("agy.log", msg);
}

fn log_preview(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn agy_path_is_runnable(path: &str) -> bool {
    let p = Path::new(path);
    if !p.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        matches!(ext.as_str(), "cmd" | "exe" | "bat" | "com")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
            .unwrap_or(false)
    }
    #[cfg(not(any(windows, unix)))]
    {
        p.is_file()
    }
}

#[cfg(unix)]
fn resolve_agy_path() -> Option<String> {
    if let Ok(val) = std::env::var("COKAC_AGY_PATH") {
        if !val.is_empty() && agy_path_is_runnable(&val) {
            agy_debug(&format!("[resolve_agy_path] COKAC_AGY_PATH={}", val));
            return Some(val);
        }
    }

    if let Ok(output) = Command::new("which").arg("agy").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && agy_path_is_runnable(&path) {
                agy_debug(&format!("[resolve_agy_path] which agy -> {}", path));
                return Some(path);
            }
        }
    }

    if let Ok(output) = Command::new("bash").args(["-lc", "which agy"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && agy_path_is_runnable(&path) {
                agy_debug(&format!("[resolve_agy_path] bash which agy -> {}", path));
                return Some(path);
            }
        }
    }

    agy_debug("[resolve_agy_path] not found");
    None
}

#[cfg(windows)]
fn resolve_agy_path() -> Option<String> {
    if let Ok(val) = std::env::var("COKAC_AGY_PATH") {
        if !val.is_empty() && agy_path_is_runnable(&val) {
            return Some(val);
        }
    }
    if let Some(path) = crate::services::claude::search_path_wide("agy", Some(".cmd")) {
        return Some(path);
    }
    if let Some(path) = crate::services::claude::search_path_wide("agy", Some(".exe")) {
        return Some(path);
    }
    None
}

fn get_agy_path() -> Option<&'static str> {
    AGY_PATH.get_or_init(resolve_agy_path).as_deref()
}

pub fn is_agy_available() -> bool {
    get_agy_path().is_some()
}

fn detect_agy_version() -> Option<String> {
    let bin = get_agy_path()?;
    let output = Command::new(bin).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

pub fn agy_version() -> Option<&'static String> {
    AGY_VERSION.get_or_init(detect_agy_version).as_ref()
}

fn detect_agy_models() -> Vec<String> {
    let Some(bin) = get_agy_path() else {
        return Vec::new();
    };
    let output = match Command::new(bin).arg("models").output() {
        Ok(o) => o,
        Err(e) => {
            agy_debug(&format!("[detect_agy_models] failed: {}", e));
            return Vec::new();
        }
    };
    if !output.status.success() {
        agy_debug(&format!(
            "[detect_agy_models] non-zero exit: {:?}",
            output.status.code()
        ));
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub fn list_models() -> Vec<String> {
    AGY_MODELS.get_or_init(detect_agy_models).clone()
}

pub fn is_valid_agy_model(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && list_models().iter().any(|m| m == model)
}

/// `gemini` is accepted as a compatibility alias but routed to `agy`.
pub fn is_agy_model(model: Option<&str>) -> bool {
    model
        .map(|m| m == "agy" || m.starts_with("agy:") || m == "gemini" || m.starts_with("gemini:"))
        .unwrap_or(false)
}

pub fn strip_agy_prefix(model: &str) -> Option<&str> {
    model
        .strip_prefix("agy:")
        .or_else(|| model.strip_prefix("gemini:"))
        .filter(|s| !s.is_empty())
        .map(|s| s.split(" \u{2014} ").next().unwrap_or(s).trim())
}

fn default_print_timeout() -> String {
    std::env::var("COKAC_AGY_PRINT_TIMEOUT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "1h".to_string())
}

fn make_agy_log_file_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("cokacdir-agy-{}-{}.log", std::process::id(), nanos))
}

fn conversation_dir() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".gemini")
            .join("antigravity-cli")
            .join("conversations"),
    )
}

pub fn conversation_path(session_id: &str) -> Option<PathBuf> {
    let dir = conversation_dir()?;
    let db = dir.join(format!("{}.db", session_id));
    if db.is_file() {
        return Some(db);
    }
    let pb = dir.join(format!("{}.pb", session_id));
    if pb.is_file() {
        return Some(pb);
    }
    None
}

pub fn conversation_exists(session_id: &str) -> bool {
    conversation_path(session_id).is_some()
}

pub fn read_last_conversation_id(working_dir: &str) -> Option<String> {
    let cache = dirs::home_dir()?
        .join(".gemini")
        .join("antigravity-cli")
        .join("cache")
        .join("last_conversations.json");
    let content = std::fs::read_to_string(cache).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    let obj = val.as_object()?;
    if let Some(sid) = obj.get(working_dir).and_then(|v| v.as_str()) {
        if !sid.is_empty() {
            return Some(sid.to_string());
        }
    }
    let canonical = Path::new(working_dir)
        .canonicalize()
        .ok()
        .map(crate::utils::format::strip_unc_prefix)
        .map(|p| p.display().to_string());
    if let Some(canonical) = canonical {
        if let Some(sid) = obj.get(&canonical).and_then(|v| v.as_str()) {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
    }
    None
}

fn session_output_cache() -> &'static Mutex<HashMap<String, String>> {
    SESSION_OUTPUT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_output_prefix(session_id: &str) -> Option<String> {
    session_output_cache()
        .lock()
        .ok()
        .and_then(|map| map.get(session_id).cloned())
        .or_else(|| bot_session_assistant_prefix(session_id))
}

fn remember_output_prefix(session_id: &str, raw_stdout: &str) {
    if raw_stdout.is_empty() {
        return;
    }
    if let Ok(mut map) = session_output_cache().lock() {
        map.insert(session_id.to_string(), raw_stdout.to_string());
    }
}

fn bot_session_assistant_prefix(session_id: &str) -> Option<String> {
    let path = dirs::home_dir()?
        .join(".cokacdir")
        .join("ai_sessions")
        .join(format!("{}.json", session_id));
    let content = std::fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    let history = val.get("history")?.as_array()?;
    let mut parts = Vec::new();
    for item in history {
        let ty = item.get("item_type").and_then(|v| v.as_str()).unwrap_or("");
        if ty != "Assistant" {
            continue;
        }
        if let Some(text) = item.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                parts.push(text.trim_end_matches('\n').to_string());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("{}\n", parts.join("\n")))
    }
}

fn strip_seen_prefix<'a>(chunk: &'a str, prefix: &str, skipped: &mut usize) -> &'a str {
    if *skipped >= prefix.len() {
        return chunk;
    }
    let remaining = &prefix[*skipped..];
    if remaining.starts_with(chunk) {
        *skipped += chunk.len();
        ""
    } else if chunk.starts_with(remaining) {
        *skipped = prefix.len();
        &chunk[remaining.len()..]
    } else {
        // Prefix from cache/history no longer matches agy's output. Stop
        // suppressing rather than risk hiding fresh content.
        *skipped = prefix.len();
        chunk
    }
}

fn stdout_error_message(stdout: &str, status_success: bool) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "Error: timed out waiting for response"
        || trimmed.starts_with("Error: failed to send message:")
        || (trimmed.starts_with("Warning: conversation ") && trimmed.contains(" not found"))
    {
        return Some(trimmed.lines().next().unwrap_or(trimmed).to_string());
    }
    if !status_success && trimmed.starts_with("Error:") {
        return Some(trimmed.lines().next().unwrap_or(trimmed).to_string());
    }
    None
}

fn truncate_diagnostic(s: &str, max: usize) -> String {
    let clean = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.len() <= max {
        return clean;
    }
    let mut end = max;
    while end > 0 && !clean.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &clean[..end])
}

fn extract_agy_log_reason(line: &str, pattern: &str) -> String {
    if pattern == "You are not logged into Antigravity" {
        return "You are not logged into Antigravity.".to_string();
    }
    if pattern == "PlannerResponse without ModifiedResponse" {
        return "PlannerResponse without ModifiedResponse encountered; no final stdout was emitted."
            .to_string();
    }
    if let Some(pos) = line.find("agent executor error:") {
        return line[pos + "agent executor error:".len()..]
            .trim()
            .to_string();
    }
    line.find(pattern)
        .map(|pos| line[pos..].trim().to_string())
        .unwrap_or_else(|| line.trim().to_string())
}

fn is_auth_transient_pattern(pattern: &str) -> bool {
    pattern == "You are not logged into Antigravity" || pattern == "UNAUTHENTICATED"
}

fn is_auth_success_line(line: &str) -> bool {
    line.contains("Print mode: silent auth succeeded")
        || line.contains("OAuth: authenticated successfully")
        || line.contains("applyAuthResult:")
        || line.contains("ChainedAuth: authenticated")
}

fn auth_succeeded_after(lines: &[&str], idx: usize) -> bool {
    lines
        .get(idx + 1..)
        .map(|tail| tail.iter().any(|line| is_auth_success_line(line)))
        .unwrap_or(false)
}

fn agy_log_failure_summary(log_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(log_path).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let patterns = [
        "RESOURCE_EXHAUSTED",
        "Individual quota reached",
        "PERMISSION_DENIED",
        "agent executor error:",
        "PlannerResponse without ModifiedResponse",
        "UNAUTHENTICATED",
        "You are not logged into Antigravity",
    ];
    for pattern in patterns {
        if let Some((idx, line)) = lines
            .iter()
            .enumerate()
            .rev()
            .find(|(_, line)| line.contains(pattern))
        {
            if is_auth_transient_pattern(pattern) && auth_succeeded_after(&lines, idx) {
                continue;
            }
            let reason = extract_agy_log_reason(line, pattern);
            return Some(format!(
                "Agy log reports: {}",
                truncate_diagnostic(&reason, 360)
            ));
        }
    }
    None
}

fn stdout_absence_error_message(
    raw_stdout: &str,
    visible_output: &str,
    had_prior_prefix: bool,
    log_summary: Option<&str>,
) -> Option<String> {
    if !raw_stdout.trim().is_empty() && !visible_output.trim().is_empty() {
        return None;
    }
    let suffix = log_summary
        .map(|s| format!(" {}", s))
        .unwrap_or_else(|| " Check the Antigravity CLI log for details.".to_string());
    if raw_stdout.trim().is_empty() {
        return Some(format!(
            "Agy exited successfully but produced no stdout response.{}",
            suffix
        ));
    }
    if had_prior_prefix {
        return Some(format!(
            "Agy exited successfully but produced no new stdout response after replaying previous conversation output.{}",
            suffix
        ));
    }
    Some(format!(
        "Agy exited successfully but produced no visible stdout response.{}",
        suffix
    ))
}

fn line_is_fatal_stdout_error(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "Error: timed out waiting for response"
        || trimmed.starts_with("Error: failed to send message:")
        || (trimmed.starts_with("Warning: conversation ") && trimmed.contains(" not found"))
}

fn build_prompt(prompt: &str, system_prompt: Option<&str>) -> String {
    match system_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sp) => format!("SYSTEM INSTRUCTIONS:\n{}\n\nUSER REQUEST:\n{}", sp, prompt),
        None => prompt.to_string(),
    }
}

pub fn execute_command(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    _allowed_tools: Option<&[String]>,
    model: Option<&str>,
) -> ClaudeResponse {
    let (tx, rx) = std::sync::mpsc::channel();
    let result = execute_command_streaming(
        prompt,
        session_id,
        working_dir,
        tx,
        None,
        None,
        None,
        model,
        false,
    );
    if let Err(e) = result {
        return ClaudeResponse {
            success: false,
            response: None,
            session_id: None,
            error: Some(e),
        };
    }

    let mut response = String::new();
    let mut last_session_id = None;
    let mut error = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            StreamMessage::Text { content } => response.push_str(&content),
            StreamMessage::Done { result, session_id } => {
                if response.is_empty() {
                    response = result;
                }
                last_session_id = session_id;
                break;
            }
            StreamMessage::Error { message, .. } => {
                error = Some(message);
                break;
            }
            StreamMessage::Init { session_id } => last_session_id = Some(session_id),
            _ => {}
        }
    }

    ClaudeResponse {
        success: error.is_none(),
        response: if response.trim().is_empty() {
            None
        } else {
            Some(response.trim().to_string())
        },
        session_id: last_session_id,
        error,
    }
}

pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    _allowed_tools: Option<&[String]>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    model: Option<&str>,
    no_session_persistence: bool,
) -> Result<(), String> {
    agy_debug("=== agy execute_command_streaming START ===");
    agy_debug(&format!(
        "[stream] prompt_len={} session_id={:?} working_dir={} model={:?} no_session_persistence={}",
        prompt.len(),
        session_id,
        working_dir,
        model,
        no_session_persistence
    ));

    if let Some(sid) = session_id {
        if !crate::services::process::is_valid_session_id(sid) {
            return Err(format!("Invalid session_id format: {}", sid));
        }
        if !conversation_exists(sid) {
            return Err(format!("Agy conversation not found: {}", sid));
        }
    }

    if let Some(m) = model {
        if !is_valid_agy_model(m) {
            return Err(format!(
                "Unsupported agy model: {}. Use /model to list available agy models.",
                m
            ));
        }
    }

    let agy_bin = get_agy_path()
        .ok_or_else(|| "Agy CLI not found. Is Antigravity CLI installed?".to_string())?;

    let mut args = Vec::<String>::new();
    if let Some(sid) = session_id {
        args.push("--conversation".into());
        args.push(sid.to_string());
    }
    args.push("--print".into());
    args.push(String::new());
    args.push("--print-timeout".into());
    args.push(default_print_timeout());
    let agy_log_path = make_agy_log_file_path();
    args.push("--log-file".into());
    args.push(agy_log_path.display().to_string());
    args.push("--dangerously-skip-permissions".into());
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.to_string());
    }

    let final_prompt = build_prompt(prompt, system_prompt);
    agy_debug(&format!(
        "[stream] spawning {} {:?}; final_prompt_len={}",
        agy_bin,
        args,
        final_prompt.len()
    ));

    let mut cmd = Command::new(agy_bin);
    cmd.args(&args)
        .current_dir(working_dir)
        .env("PATH", enhanced_path_for_bin(agy_bin))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::services::claude::detach_into_own_pgroup(&mut cmd);
    crate::services::claude::attach_cancel_cgroup(&mut cmd, cancel_token.as_ref());

    let mut child = cmd.spawn().map_err(|e| {
        agy_debug(&format!("[stream] spawn failed: {}", e));
        format!("Failed to start agy: {}", e)
    })?;
    agy_debug(&format!("[stream] spawned pid={}", child.id()));

    if let Some(ref token) = cancel_token {
        let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(child.id());
        drop(guard);
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(final_prompt.as_bytes()) {
            agy_debug(&format!("[stream] stdin write failed: {}", e));
        }
        drop(stdin);
    }

    let stderr_thread = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default())
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture agy stdout".to_string())?;
    let mut reader = BufReader::new(stdout);

    let prior_prefix = session_id
        .and_then(cached_output_prefix)
        .unwrap_or_default();
    let mut skipped_prefix_len = 0usize;
    let mut raw_stdout = String::new();
    let mut visible_output = String::new();
    let mut stdout_error: Option<String> = None;

    loop {
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                agy_debug("[stream] cancelled during stdout read");
                kill_child_tree(&mut child);
                let _ = child.wait();
                return Ok(());
            }
        }

        let mut chunk = String::new();
        let bytes = reader.read_line(&mut chunk).map_err(|e| {
            agy_debug(&format!("[stream] stdout read failed: {}", e));
            format!("Failed to read agy output: {}", e)
        })?;
        if bytes == 0 {
            break;
        }
        raw_stdout.push_str(&chunk);
        agy_debug(&format!(
            "[stream] stdout chunk: {} bytes, preview={:?}",
            chunk.len(),
            log_preview(&chunk, 200)
        ));

        if line_is_fatal_stdout_error(&chunk) {
            let msg = chunk.trim().to_string();
            agy_debug(&format!("[stream] fatal stdout line: {}", msg));
            stdout_error = Some(msg);
            kill_child_tree(&mut child);
            break;
        }

        let emit = strip_seen_prefix(&chunk, &prior_prefix, &mut skipped_prefix_len);
        if !emit.is_empty() {
            visible_output.push_str(emit);
            if sender
                .send(StreamMessage::Text {
                    content: emit.to_string(),
                })
                .is_err()
            {
                agy_debug("[stream] receiver dropped");
                break;
            }
        }
    }

    if let Some(ref token) = cancel_token {
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            agy_debug("[stream] cancelled after stdout read");
            kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("Agy process error: {}", e))?;
    let stderr_msg = stderr_thread
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    if !stderr_msg.is_empty() {
        agy_debug(&format!(
            "[stream] stderr: {} bytes, preview={:?}",
            stderr_msg.len(),
            log_preview(&stderr_msg, 500)
        ));
    }

    let log_failure_summary = agy_log_failure_summary(&agy_log_path);
    if let Some(ref summary) = log_failure_summary {
        agy_debug(&format!("[stream] {}", summary));
    }

    let detected_error = stdout_error
        .or_else(|| stdout_error_message(&raw_stdout, status.success()))
        .or_else(|| {
            if status.success() {
                stdout_absence_error_message(
                    &raw_stdout,
                    &visible_output,
                    !prior_prefix.is_empty(),
                    log_failure_summary.as_deref(),
                )
            } else {
                log_failure_summary
            }
        });
    if detected_error.is_some() || !status.success() {
        let message =
            detected_error.unwrap_or_else(|| format!("Agy exited with code {:?}", status.code()));
        agy_debug(&format!(
            "[stream] error: {}, exit={:?}, stdout_len={}, stderr_len={}",
            message,
            status.code(),
            raw_stdout.len(),
            stderr_msg.len()
        ));
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: raw_stdout,
            stderr: stderr_msg,
            exit_code: status.code(),
        });
        return Ok(());
    }

    let last_session_id = session_id
        .map(ToString::to_string)
        .or_else(|| read_last_conversation_id(working_dir));
    if let Some(ref sid) = last_session_id {
        remember_output_prefix(sid, &raw_stdout);
    }

    let _ = sender.send(StreamMessage::Done {
        result: visible_output,
        session_id: last_session_id,
    });
    agy_debug("=== agy execute_command_streaming END ===");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        agy_log_failure_summary, stdout_absence_error_message, stdout_error_message,
        strip_seen_prefix,
    };

    #[test]
    fn strips_seen_prefix_across_line_chunks() {
        let prefix = "SESSION_ONE\nSESSION_TWO\n";
        let mut skipped = 0usize;

        assert_eq!(strip_seen_prefix("SESSION_ONE\n", prefix, &mut skipped), "");
        assert_eq!(strip_seen_prefix("SESSION_TWO\n", prefix, &mut skipped), "");
        assert_eq!(
            strip_seen_prefix("SESSION_THREE\n", prefix, &mut skipped),
            "SESSION_THREE\n"
        );
    }

    #[test]
    fn emits_suffix_when_chunk_crosses_prefix_boundary() {
        let prefix = "OLD";
        let mut skipped = 0usize;
        assert_eq!(strip_seen_prefix("OLDNEW\n", prefix, &mut skipped), "NEW\n");
    }

    #[test]
    fn detects_agy_stdout_errors_even_on_success() {
        assert_eq!(
            stdout_error_message("Error: timed out waiting for response\n", true).as_deref(),
            Some("Error: timed out waiting for response")
        );
        assert_eq!(
            stdout_error_message(
                "Warning: conversation \"000\" not found.\nSHOULD_NOT_RUN\n",
                true
            )
            .as_deref(),
            Some("Warning: conversation \"000\" not found.")
        );
    }

    #[test]
    fn detects_successful_empty_stdout_as_error() {
        assert_eq!(
            stdout_absence_error_message("", "", false, None).as_deref(),
            Some(
                "Agy exited successfully but produced no stdout response. Check the Antigravity CLI log for details."
            )
        );
    }

    #[test]
    fn detects_replayed_resume_without_new_output_as_error() {
        assert_eq!(
            stdout_absence_error_message("OLD\n", "", true, Some("Agy log reports: RESOURCE_EXHAUSTED (code 429).")).as_deref(),
            Some(
                "Agy exited successfully but produced no new stdout response after replaying previous conversation output. Agy log reports: RESOURCE_EXHAUSTED (code 429)."
            )
        );
    }

    #[test]
    fn allows_successful_visible_stdout() {
        assert!(stdout_absence_error_message("OLD\nNEW\n", "NEW\n", true, None).is_none());
    }

    #[test]
    fn summarizes_quota_log_before_planner_absence() {
        let path = std::env::temp_dir().join(format!(
            "cokacdir-agy-test-{}-{}.log",
            std::process::id(),
            "quota"
        ));
        std::fs::write(
            &path,
            "E log.go:398] agent executor error: RESOURCE_EXHAUSTED (code 429): Individual quota reached.\nI printmode_manager.go:90] PlannerResponse without ModifiedResponse encountered\n",
        )
        .unwrap();
        let summary = agy_log_failure_summary(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            summary.as_deref(),
            Some("Agy log reports: RESOURCE_EXHAUSTED (code 429): Individual quota reached.")
        );
    }

    #[test]
    fn ignores_transient_auth_errors_after_silent_auth_success() {
        let path = std::env::temp_dir().join(format!(
            "cokacdir-agy-test-{}-{}.log",
            std::process::id(),
            "auth-transient"
        ));
        std::fs::write(
            &path,
            "E log.go:398] error getting token source: You are not logged into Antigravity.\nI server_oauth.go:212] applyAuthResult: email=user@example.com, authMethod=consumer, quotaProject=\nI printmode.go:192] Print mode: silent auth succeeded\n",
        )
        .unwrap();
        let summary = agy_log_failure_summary(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(summary, None);
    }

    #[test]
    fn reports_auth_error_when_auth_never_succeeds() {
        let path = std::env::temp_dir().join(format!(
            "cokacdir-agy-test-{}-{}.log",
            std::process::id(),
            "auth-failure"
        ));
        std::fs::write(
            &path,
            "E log.go:398] error getting token source: You are not logged into Antigravity.\nE server.go:640] Failed to get OAuth token: error getting token source from auth provider: You are not logged into Antigravity.\n",
        )
        .unwrap();
        let summary = agy_log_failure_summary(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            summary.as_deref(),
            Some("Agy log reports: You are not logged into Antigravity.")
        );
    }
}
