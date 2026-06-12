//! Kiro service — spawns standalone `kiro-cli chat` in non-interactive mode
//! and streams plain-text output.
//!
//! The supported target is the standalone `kiro-cli` documented at kiro.dev.
//! The packaged desktop-app launcher bundled inside `Kiro.app` is not selected
//! automatically because it opens the GUI and does not provide compatible
//! headless chat output for cokacdir.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use regex::Regex;

use crate::services::claude::{
    attach_cancel_cgroup, debug_log_to, detach_into_own_pgroup, enhanced_path_for_bin,
    kill_child_tree, CancelToken, StreamMessage,
};
use crate::ui::ai_screen::{self, SessionData};

fn kiro_debug(msg: &str) {
    debug_log_to("kiro.log", msg);
}

#[derive(Clone, Debug)]
struct KiroCommand {
    program: String,
    fixed_args: Vec<String>,
    debug_label: String,
}

static KIRO_COMMAND: OnceLock<Option<KiroCommand>> = OnceLock::new();
static NODE_PATH: OnceLock<Option<String>> = OnceLock::new();
static ANSI_RE: OnceLock<Regex> = OnceLock::new();

fn ansi_re() -> &'static Regex {
    ANSI_RE.get_or_init(|| {
        Regex::new(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])").expect("valid ANSI regex")
    })
}

#[cfg(unix)]
fn resolve_node_path() -> Option<String> {
    if let Ok(output) = Command::new("which").arg("node").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && binary_path_is_runnable(&path) {
                return Some(path);
            }
        }
    }

    if let Ok(output) = Command::new("bash").args(["-lc", "which node"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && binary_path_is_runnable(&path) {
                return Some(path);
            }
        }
    }

    None
}

#[cfg(unix)]
fn resolve_kiro_env_command() -> Option<KiroCommand> {
    if let Ok(val) = std::env::var("COKAC_KIRO_PATH") {
        if !val.is_empty() {
            if let Some(command) = command_from_path(&val) {
                return Some(command);
            }
        }
    }
    None
}

#[cfg(unix)]
fn resolve_kiro_command() -> Option<KiroCommand> {
    if let Some(home) = dirs::home_dir() {
        for candidate in [
            home.join(".local").join("bin").join("kiro-cli"),
            home.join("bin").join("kiro-cli"),
            Path::new("/opt/homebrew/bin/kiro-cli").to_path_buf(),
            Path::new("/usr/local/bin/kiro-cli").to_path_buf(),
            Path::new("/usr/bin/kiro-cli").to_path_buf(),
        ] {
            if let Some(command) = candidate.to_str().and_then(command_from_path) {
                return Some(command);
            }
        }
    }

    if let Ok(output) = Command::new("which").arg("kiro-cli").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                if let Some(command) = command_from_path(&path) {
                    return Some(command);
                }
            }
        }
    }

    if let Ok(output) = Command::new("bash")
        .args(["-lc", "which kiro-cli"])
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                if let Some(command) = command_from_path(&path) {
                    return Some(command);
                }
            }
        }
    }

    None
}

#[cfg(windows)]
fn resolve_node_path() -> Option<String> {
    if let Some(path) = crate::services::claude::search_path_wide("node", Some(".exe")) {
        return Some(path);
    }
    if let Some(path) = crate::services::claude::search_path_wide("node", Some(".cmd")) {
        return Some(path);
    }

    None
}

#[cfg(windows)]
fn resolve_kiro_env_command() -> Option<KiroCommand> {
    if let Ok(val) = std::env::var("COKAC_KIRO_PATH") {
        if !val.is_empty() {
            if let Some(command) = command_from_path(&val) {
                return Some(command);
            }
        }
    }
    None
}

#[cfg(windows)]
fn resolve_kiro_command() -> Option<KiroCommand> {
    if let Some(path) = crate::services::claude::search_path_wide("kiro-cli", Some(".cmd")) {
        if let Some(command) = command_from_path(&path) {
            return Some(command);
        }
    }
    if let Some(path) = crate::services::claude::search_path_wide("kiro-cli", Some(".exe")) {
        if let Some(command) = command_from_path(&path) {
            return Some(command);
        }
    }

    None
}

fn node_bin() -> Option<&'static str> {
    NODE_PATH.get_or_init(resolve_node_path).as_deref()
}

fn binary_path_is_runnable(path: &str) -> bool {
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

fn path_is_node_script(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("js" | "mjs" | "cjs")
    )
}

fn command_from_path(path: &str) -> Option<KiroCommand> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }

    let p = Path::new(trimmed);
    if path_is_node_script(p) {
        let node = node_bin()?;
        return Some(KiroCommand {
            program: node.to_string(),
            fixed_args: vec![trimmed.to_string()],
            debug_label: format!("{} {}", node, trimmed),
        });
    }

    if binary_path_is_runnable(trimmed) {
        return Some(KiroCommand {
            program: trimmed.to_string(),
            fixed_args: Vec::new(),
            debug_label: trimmed.to_string(),
        });
    }

    None
}

fn kiro_command() -> Option<KiroCommand> {
    if let Some(command) = resolve_kiro_env_command() {
        return Some(command);
    }

    KIRO_COMMAND.get_or_init(resolve_kiro_command).clone()
}

pub fn is_kiro_available() -> bool {
    let result = kiro_command().is_some();
    kiro_debug(&format!("[is_kiro_available] result={}", result));
    result
}

pub fn is_kiro_model(model: Option<&str>) -> bool {
    let result = model.map(|m| m.trim() == "kiro").unwrap_or(false);
    kiro_debug(&format!(
        "[is_kiro_model] model={:?} result={}",
        model, result
    ));
    result
}

fn has_saved_kiro_session(working_dir: &str) -> bool {
    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        return false;
    };
    let Ok(entries) = fs::read_dir(&sessions_dir) else {
        return false;
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(session_data) = serde_json::from_str::<SessionData>(&content) else {
            continue;
        };
        if session_data.provider == "kiro"
            && session_data.current_path == working_dir
            && !session_data.history.is_empty()
        {
            return true;
        }
    }

    false
}

fn strip_ansi_and_normalize(raw: &str) -> String {
    let cleaned = ansi_re().replace_all(raw, "");
    cleaned.replace('\r', "\n")
}

fn build_effective_prompt(prompt: &str, system_prompt: Option<&str>) -> String {
    match system_prompt {
        Some(sp) if !sp.trim().is_empty() => format!(
            "<system_instructions>\n{}\n</system_instructions>\n\n<user_request>\n{}\n</user_request>\n",
            sp.trim(),
            prompt
        ),
        _ => prompt.to_string(),
    }
}

fn build_chat_args(
    prompt: &str,
    session_id: Option<&str>,
    resume_dir_available: bool,
    no_session_persistence: bool,
    allowed_tools: Option<&[String]>,
) -> (Vec<String>, &'static str, &'static str) {
    let mut args = vec!["chat".to_string()];
    let mut resume_mode = "new";
    if let Some(sid) = session_id.filter(|s| !s.trim().is_empty()) {
        args.push("--resume-id".to_string());
        args.push(sid.to_string());
        resume_mode = "resume-id";
    } else if !no_session_persistence && resume_dir_available {
        args.push("--resume".to_string());
        resume_mode = "resume-dir";
    }

    let trust_mode = match allowed_tools {
        Some(tools) if tools.is_empty() => {
            args.push("--trust-tools=".to_string());
            "none"
        }
        Some(_) => {
            args.push("--trust-all-tools".to_string());
            "all-configured"
        }
        None => {
            args.push("--trust-all-tools".to_string());
            "all-default"
        }
    };

    args.push("--no-interactive".to_string());
    args.push("--wrap".to_string());
    args.push("never".to_string());
    args.push(prompt.to_string());
    (args, resume_mode, trust_mode)
}

fn is_kiro_error_output(output: &str) -> bool {
    let trimmed = output.trim();
    trimmed.contains("Kiro is having trouble responding right now:")
        || trimmed.starts_with("error:")
}

pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    _model: Option<&str>,
    no_session_persistence: bool,
) -> Result<(), String> {
    kiro_debug("=== kiro execute_command_streaming START ===");
    kiro_debug(&format!(
        "[stream] prompt_len={} session_id={:?} working_dir={} no_session_persistence={}",
        prompt.len(),
        session_id,
        working_dir,
        no_session_persistence
    ));

    let kiro_command = kiro_command().ok_or_else(|| {
        "Kiro CLI not found. Install standalone `kiro-cli`, or set `COKAC_KIRO_PATH` to a compatible Kiro CLI executable.".to_string()
    })?;

    let effective_prompt = build_effective_prompt(prompt, system_prompt);
    kiro_debug(&format!(
        "[stream] effective_prompt_len={} system_prompt_len={}",
        effective_prompt.len(),
        system_prompt.map(|s| s.len()).unwrap_or(0)
    ));

    let allowed_tools_count = allowed_tools.map(|tools| tools.len()).unwrap_or(0);
    let (args, resume_mode, trust_mode) = build_chat_args(
        &effective_prompt,
        session_id,
        has_saved_kiro_session(working_dir),
        no_session_persistence,
        allowed_tools,
    );

    kiro_debug(&format!(
        "[stream] command={} args={:?} resume_mode={} trust_mode={} allowed_tools_count={}",
        kiro_command.debug_label, args, resume_mode, trust_mode, allowed_tools_count
    ));

    let mut cmd = Command::new(&kiro_command.program);
    cmd.args(&kiro_command.fixed_args)
        .args(&args)
        .current_dir(working_dir)
        .env("PATH", enhanced_path_for_bin(&kiro_command.program))
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .env("TERM", "dumb")
        .env("CI", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    detach_into_own_pgroup(&mut cmd);
    attach_cancel_cgroup(&mut cmd, cancel_token.as_ref());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start Kiro: {}", e))?;
    kiro_debug(&format!("[stream] spawned pid={:?}", child.id()));

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

    let stderr_thread = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default())
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Kiro stdout".to_string())?;
    let reader = BufReader::new(stdout);

    let mut final_result = String::new();
    let mut stdout_error: Option<String> = None;

    for line in reader.lines() {
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                kill_child_tree(&mut child);
                let _ = child.wait();
                return Ok(());
            }
        }

        let line = match line {
            Ok(line) => line,
            Err(e) => {
                stdout_error = Some(format!("Failed to read Kiro output: {}", e));
                break;
            }
        };

        let cleaned = strip_ansi_and_normalize(&line);
        if cleaned.trim().is_empty() {
            if !final_result.ends_with('\n') {
                final_result.push('\n');
                let _ = sender.send(StreamMessage::Text {
                    content: "\n".to_string(),
                });
            }
            continue;
        }

        let mut chunk = cleaned;
        if !chunk.ends_with('\n') {
            chunk.push('\n');
        }
        final_result.push_str(&chunk);
        let _ = sender.send(StreamMessage::Text { content: chunk });
    }

    if let Some(ref token) = cancel_token {
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("Kiro process wait failed: {}", e))?;
    let stderr_msg = stderr_thread
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    if let Some(message) = stdout_error {
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: final_result,
            stderr: stderr_msg,
            exit_code: status.code(),
        });
        return Ok(());
    }

    if !status.success() {
        let _ = sender.send(StreamMessage::Error {
            message: format!("Kiro exited with code {:?}", status.code()),
            stdout: final_result,
            stderr: stderr_msg,
            exit_code: status.code(),
        });
        return Ok(());
    }

    let final_result = final_result.trim_end_matches('\n').to_string();
    if final_result.trim().is_empty() {
        let _ = sender.send(StreamMessage::Error {
            message:
                "Kiro returned no text. Ensure standalone `kiro-cli` is installed and authenticated."
                    .to_string(),
            stdout: final_result,
            stderr: stderr_msg,
            exit_code: status.code(),
        });
        return Ok(());
    }

    if is_kiro_error_output(&final_result) {
        let _ = sender.send(StreamMessage::Error {
            message: "Kiro reported an error".to_string(),
            stdout: final_result,
            stderr: stderr_msg,
            exit_code: status.code(),
        });
        return Ok(());
    }

    let _ = sender.send(StreamMessage::Done {
        result: final_result,
        session_id: session_id.map(|sid| sid.to_string()),
    });

    kiro_debug("=== kiro execute_command_streaming END ===");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_chat_args, resolve_kiro_command};

    #[cfg(unix)]
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[cfg(unix)]
    struct EnvVarGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old }
        }
    }

    #[cfg(unix)]
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(old) = self.old.take() {
                std::env::set_var(self.key, old);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn chat_args_include_trust_all_and_noninteractive_prompt_for_new_session() {
        let (args, resume_mode, trust_mode) = build_chat_args("hello", None, false, false, None);
        assert_eq!(resume_mode, "new");
        assert_eq!(trust_mode, "all-default");
        assert_eq!(
            args,
            vec![
                "chat",
                "--trust-all-tools",
                "--no-interactive",
                "--wrap",
                "never",
                "hello",
            ]
        );
    }

    #[test]
    fn chat_args_include_resume_id_before_trust_and_prompt() {
        let (args, resume_mode, trust_mode) =
            build_chat_args("hello", Some("abc-123"), false, false, None);
        assert_eq!(resume_mode, "resume-id");
        assert_eq!(trust_mode, "all-default");
        assert_eq!(
            args,
            vec![
                "chat",
                "--resume-id",
                "abc-123",
                "--trust-all-tools",
                "--no-interactive",
                "--wrap",
                "never",
                "hello",
            ]
        );
    }

    #[test]
    fn chat_args_include_resume_flag_before_trust_and_prompt() {
        let (args, resume_mode, trust_mode) = build_chat_args("hello", None, true, false, None);
        assert_eq!(resume_mode, "resume-dir");
        assert_eq!(trust_mode, "all-default");
        assert_eq!(
            args,
            vec![
                "chat",
                "--resume",
                "--trust-all-tools",
                "--no-interactive",
                "--wrap",
                "never",
                "hello",
            ]
        );
    }

    #[test]
    fn no_session_persistence_disables_resume_dir_but_keeps_trust_and_prompt() {
        let (args, resume_mode, trust_mode) = build_chat_args("hello", None, true, true, None);
        assert_eq!(resume_mode, "new");
        assert_eq!(trust_mode, "all-default");
        assert_eq!(
            args,
            vec![
                "chat",
                "--trust-all-tools",
                "--no-interactive",
                "--wrap",
                "never",
                "hello",
            ]
        );
    }

    #[test]
    fn explicit_empty_allowed_tools_disables_kiro_tool_trust() {
        let allowed_tools: Vec<String> = Vec::new();
        let (args, resume_mode, trust_mode) =
            build_chat_args("hello", None, false, false, Some(&allowed_tools));
        assert_eq!(resume_mode, "new");
        assert_eq!(trust_mode, "none");
        assert_eq!(
            args,
            vec![
                "chat",
                "--trust-tools=",
                "--no-interactive",
                "--wrap",
                "never",
                "hello",
            ]
        );
    }

    #[test]
    fn nonempty_allowed_tools_still_use_trust_all_tools() {
        let allowed_tools = vec!["Bash".to_string(), "WebSearch".to_string()];
        let (args, resume_mode, trust_mode) =
            build_chat_args("hello", None, false, false, Some(&allowed_tools));
        assert_eq!(resume_mode, "new");
        assert_eq!(trust_mode, "all-configured");
        assert_eq!(
            args,
            vec![
                "chat",
                "--trust-all-tools",
                "--no-interactive",
                "--wrap",
                "never",
                "hello",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_kiro_command_finds_home_local_bin_install() {
        use std::os::unix::fs::PermissionsExt;

        let _env_guard = env_lock().lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let bin_dir = home.path().join(".local").join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let fake_kiro = bin_dir.join("kiro-cli");
        std::fs::write(&fake_kiro, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&fake_kiro).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_kiro, perms).unwrap();

        let _home_guard = EnvVarGuard::set("HOME", home.path());
        let command = resolve_kiro_command().expect("expected resolver to find fake kiro-cli");
        assert_eq!(command.program, fake_kiro.display().to_string());
    }
}
