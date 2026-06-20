//! Antigravity CLI (`agy`) provider.
//!
//! `agy --print` is a plain-stdout interface, not a Claude/Gemini-compatible
//! JSON event stream. This adapter synthesizes cokacdir's shared
//! `StreamMessage` contract from stdout.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::services::claude::{
    debug_log_to, enhanced_path_for_bin, kill_child_tree, CancelToken, ClaudeResponse,
    StreamMessage,
};

static AGY_PATH: OnceLock<Option<String>> = OnceLock::new();
static AGY_VERSION: OnceLock<Option<String>> = OnceLock::new();
static AGY_MODELS: OnceLock<Vec<String>> = OnceLock::new();

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
            agy_debug(&format!("[resolve_agy_path] COKAC_AGY_PATH={}", val));
            return Some(val);
        } else if !val.is_empty() {
            agy_debug(&format!(
                "[resolve_agy_path] ignoring non-runnable COKAC_AGY_PATH={}",
                val
            ));
        }
    }

    // Prefer Antigravity's native Windows binary over a .cmd wrapper. Batch
    // wrappers add another argument/stdio layer, which is avoidable for agy.
    if let Some(path) = crate::services::claude::search_path_wide("agy", Some(".exe")) {
        agy_debug(&format!("[resolve_agy_path] SearchPathW .exe -> {}", path));
        return Some(path);
    }
    if let Some(path) = crate::services::claude::search_path_wide("agy", Some(".cmd")) {
        if let Some(native) = agy_native_exe_for_wrapper(&path) {
            agy_debug(&format!(
                "[resolve_agy_path] SearchPathW .cmd -> native exe {}",
                native
            ));
            return Some(native);
        }
        agy_debug(&format!("[resolve_agy_path] SearchPathW .cmd -> {}", path));
        return Some(path);
    }
    if let Ok(output) = Command::new("where.exe").arg("agy").output() {
        if output.status.success() {
            let decoded = crate::services::claude::decode_windows_output(&output.stdout);
            let mut fallback = None;
            for path in decoded.lines().map(str::trim).filter(|p| !p.is_empty()) {
                if !agy_path_is_runnable(path) {
                    continue;
                }
                if path.to_ascii_lowercase().ends_with(".exe") {
                    agy_debug(&format!("[resolve_agy_path] where.exe -> {}", path));
                    return Some(path.to_string());
                }
                if fallback.is_none() {
                    fallback = Some(path.to_string());
                }
            }
            if let Some(path) = fallback {
                agy_debug(&format!(
                    "[resolve_agy_path] where.exe fallback -> {}",
                    path
                ));
                return Some(path);
            }
        }
    }
    agy_debug("[resolve_agy_path] not found");
    None
}

#[cfg(windows)]
fn agy_native_exe_for_wrapper(wrapper_path: &str) -> Option<String> {
    let exe = Path::new(wrapper_path).with_extension("exe");
    let exe = exe.to_string_lossy().to_string();
    agy_path_is_runnable(&exe).then_some(exe)
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
    for key in working_dir_cache_keys(working_dir) {
        if let Some(sid) = obj.get(&key).and_then(|v| v.as_str()) {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
    }
    None
}

fn push_unique(vec: &mut Vec<String>, value: String) {
    if !value.is_empty() && !vec.iter().any(|v| v == &value) {
        vec.push(value);
    }
}

fn add_path_key_variants(vec: &mut Vec<String>, value: &str) {
    push_unique(vec, value.to_string());
    let looks_windows = value.contains('\\') || value.as_bytes().get(1) == Some(&b':');
    if value.contains('\\') {
        push_unique(vec, value.replace('\\', "/"));
    }
    if looks_windows && value.contains('/') {
        push_unique(vec, value.replace('/', "\\"));
    }
}

fn working_dir_cache_keys(working_dir: &str) -> Vec<String> {
    let mut keys = Vec::new();
    add_path_key_variants(&mut keys, working_dir);

    if let Ok(canonical) = Path::new(working_dir).canonicalize() {
        let canonical = crate::utils::format::strip_unc_prefix(canonical);
        add_path_key_variants(&mut keys, &canonical.display().to_string());
    }

    keys
}

fn stdout_absence_error_message(raw_stdout: &str) -> Option<String> {
    if !raw_stdout.trim().is_empty() {
        return None;
    }
    Some("Agy exited successfully but produced no stdout response.".to_string())
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

    let mut raw_stdout = String::new();
    let mut visible_output = String::new();

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

        visible_output.push_str(&chunk);
        if sender
            .send(StreamMessage::Text {
                content: chunk.to_string(),
            })
            .is_err()
        {
            agy_debug("[stream] receiver dropped");
            break;
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

    let last_session_id = session_id
        .map(ToString::to_string)
        .or_else(|| read_last_conversation_id(working_dir));

    let detected_error = if status.success() {
        stdout_absence_error_message(&raw_stdout)
    } else {
        None
    };
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

    let _ = sender.send(StreamMessage::Done {
        result: visible_output,
        session_id: last_session_id,
    });
    agy_debug("=== agy execute_command_streaming END ===");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{stdout_absence_error_message, working_dir_cache_keys};

    #[test]
    fn detects_successful_empty_stdout_as_error() {
        assert_eq!(
            stdout_absence_error_message("").as_deref(),
            Some("Agy exited successfully but produced no stdout response.")
        );
    }

    #[test]
    fn allows_successful_visible_stdout() {
        assert!(stdout_absence_error_message("OLD\nNEW\n").is_none());
    }

    #[test]
    fn includes_windows_and_slash_cache_key_variants() {
        let keys = working_dir_cache_keys(r"C:\Users\kst\.cokacdir\workspace\eikfuccw");
        assert!(keys.contains(&r"C:\Users\kst\.cokacdir\workspace\eikfuccw".to_string()));
        assert!(keys.contains(&"C:/Users/kst/.cokacdir/workspace/eikfuccw".to_string()));
    }
}
