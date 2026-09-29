//! Herdr agent provider.
//!
//! This adapter forwards a Cokacdir turn to an already-running Herdr agent,
//! waits for the agent to settle, then reads the terminal output back.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::services::claude::{
    attach_cancel_cgroup, detach_into_own_pgroup, enhanced_path_for_bin, kill_child_tree,
    send_success_terminal, CancelToken, StreamMessage,
};

const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const DEFAULT_READ_LINES: u32 = 1000;
const DEFAULT_SCREEN_LINES: u32 = 120;
const COMPLETION_READ_LINES: u32 = 120;
const COMPLETION_PREVIEW_CHARS: usize = 1200;

static ACTIVE_PROMPTS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();

struct ActivePromptGuard {
    target: String,
}

impl ActivePromptGuard {
    fn new(target: &str) -> Self {
        let mut prompts = ACTIVE_PROMPTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *prompts.entry(target.to_string()).or_insert(0) += 1;
        Self {
            target: target.to_string(),
        }
    }
}

impl Drop for ActivePromptGuard {
    fn drop(&mut self) {
        let mut prompts = ACTIVE_PROMPTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = prompts.get_mut(&self.target) {
            *count -= 1;
            if *count == 0 {
                prompts.remove(&self.target);
            }
        }
    }
}

pub fn is_herdr_model(model: Option<&str>) -> bool {
    model
        .map(|model| model == "herdr" || model.starts_with("herdr:"))
        .unwrap_or(false)
}

pub fn strip_herdr_prefix(model: &str) -> Option<&str> {
    model
        .strip_prefix("herdr:")
        .filter(|target| !target.is_empty())
}

pub fn is_valid_target(target: &str) -> bool {
    let mut chars = target.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && target.len() <= 32
}

pub fn target_from_model(model: Option<&str>) -> Result<String, String> {
    if let Some(target) = model.and_then(strip_herdr_prefix) {
        if is_valid_target(target) {
            return Ok(target.to_string());
        }
        return Err(
            "Invalid Herdr agent name. Use 1-32 lowercase letters, digits, '-' or '_'.".to_string(),
        );
    }

    let target = std::env::var("COKAC_HERDR_AGENT").map_err(|_| {
        "Herdr agent is not configured. Use /model herdr:<agent-name> or set COKAC_HERDR_AGENT."
            .to_string()
    })?;
    if is_valid_target(&target) {
        Ok(target)
    } else {
        Err("COKAC_HERDR_AGENT contains an invalid Herdr agent name.".to_string())
    }
}

fn herdr_path() -> Option<String> {
    if let Ok(path) = std::env::var("COKAC_HERDR_PATH") {
        if !path.is_empty() && Path::new(&path).is_file() {
            return Some(path);
        }
    }
    let output = Command::new("which").arg("herdr").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty() && Path::new(&path).is_file()).then_some(path)
}

pub fn is_herdr_available() -> bool {
    herdr_path().is_some()
}

fn timeout_ms() -> u64 {
    std::env::var("COKAC_HERDR_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value >= 1000)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

fn read_lines() -> u32 {
    std::env::var("COKAC_HERDR_READ_LINES")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_READ_LINES)
}

fn screen_lines() -> u32 {
    std::env::var("COKAC_HERDR_SCREEN_LINES")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SCREEN_LINES)
}

fn command(bin: &str) -> Command {
    let mut command = Command::new(bin);
    command.env("PATH", enhanced_path_for_bin(bin));
    command
}

fn output_error(action: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    if detail.is_empty() {
        format!(
            "Herdr {action} failed with exit code {}.",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )
    } else {
        format!("Herdr {action} failed: {detail}")
    }
}

fn read_agent_lines(bin: &str, target: &str, lines: u32) -> Result<String, String> {
    let output = command(bin)
        .args([
            "agent",
            "read",
            target,
            "--source",
            "recent-unwrapped",
            "--lines",
            &lines.to_string(),
            "--format",
            "text",
        ])
        .output()
        .map_err(|error| format!("Failed to run Herdr agent read: {error}"))?;
    if !output.status.success() {
        return Err(output_error("agent read", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).replace('\r', ""))
}

fn read_agent(bin: &str, target: &str) -> Result<String, String> {
    read_agent_lines(bin, target, read_lines())
}

pub fn agent_status(target: &str) -> Result<String, String> {
    if !is_valid_target(target) {
        return Err("Invalid Herdr agent name.".to_string());
    }
    let bin = herdr_path().ok_or_else(|| "Herdr provider is not installed.".to_string())?;
    let output = command(&bin)
        .args(["agent", "get", target])
        .output()
        .map_err(|error| format!("Failed to run Herdr agent get: {error}"))?;
    if !output.status.success() {
        return Err(output_error("agent get", &output));
    }
    parse_agent_status(&output.stdout)
}

fn parse_agent_status(output: &[u8]) -> Result<String, String> {
    let value: Value = serde_json::from_slice(output)
        .map_err(|error| format!("Herdr returned invalid agent JSON: {error}"))?;
    if value.get("error").is_some() {
        return Err(format!("Herdr agent get failed: {value}"));
    }
    value
        .pointer("/result/agent/agent_status")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "Herdr agent status is missing from the response.".to_string())
}

pub fn is_cokacdir_prompt_active(target: &str) -> bool {
    ACTIVE_PROMPTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(target)
        .copied()
        .unwrap_or(0)
        > 0
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str("\n…");
    truncated
}

pub fn completion_preview(target: &str) -> Result<String, String> {
    if !is_valid_target(target) {
        return Err("Invalid Herdr agent name.".to_string());
    }
    let bin = herdr_path().ok_or_else(|| "Herdr provider is not installed.".to_string())?;
    let output = read_agent_lines(&bin, target, COMPLETION_READ_LINES)?;
    let response = codex_final_response(&output).unwrap_or_else(|| {
        output
            .lines()
            .rev()
            .take(30)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    });
    Ok(truncate_chars(&response, COMPLETION_PREVIEW_CHARS))
}

pub fn read_screen(model: Option<&str>) -> Result<(String, String), String> {
    let target = target_from_model(model)?;
    let bin = herdr_path().ok_or_else(|| "Herdr provider is not installed.".to_string())?;
    let output = command(&bin)
        .args([
            "agent",
            "read",
            &target,
            "--source",
            "visible",
            "--lines",
            &screen_lines().to_string(),
            "--format",
            "text",
        ])
        .output()
        .map_err(|error| format!("Failed to run Herdr agent read: {error}"))?;
    if !output.status.success() {
        return Err(output_error("screen read", &output));
    }
    let screen = String::from_utf8_lossy(&output.stdout)
        .replace('\r', "")
        .trim_end()
        .to_string();
    Ok((target, screen))
}

fn snapshot_delta(before: &str, after: &str) -> String {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let max_overlap = before_lines.len().min(after_lines.len());
    for overlap in (1..=max_overlap).rev() {
        if before_lines[before_lines.len() - overlap..] == after_lines[..overlap] {
            return after_lines[overlap..].join("\n").trim().to_string();
        }
    }
    after.trim().to_string()
}

fn is_tui_separator(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("─ Worked for ")
        || (line.chars().count() >= 20 && line.chars().all(|character| character == '─'))
}

fn current_turn_output(before: &str, after: &str, prompt: &str) -> String {
    let prompt = prompt.trim();
    if !prompt.is_empty() {
        let first_line = prompt.lines().next().unwrap_or("").trim();
        if !first_line.is_empty() {
            for marker in ["› ", "> "] {
                let needle = format!("{marker}{first_line}");
                if let Some(index) = after.rfind(&needle) {
                    let output = &after[index + needle.len()..];
                    if marker == "> " {
                        return output
                            .lines()
                            .take_while(|line| !is_tui_separator(line))
                            .collect::<Vec<_>>()
                            .join("\n");
                    }
                    return output.to_string();
                }
            }
        }
    }
    snapshot_delta(before, after)
}

fn strip_legacy_markers(response: &str) -> String {
    response
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.starts_with("COKACDIR_RESPONSE_BEGIN_")
                && !line.starts_with("COKACDIR_RESPONSE_END_")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn codex_final_response(turn_output: &str) -> Option<String> {
    let mut response_lines = Vec::new();
    for line in turn_output.lines() {
        if line.starts_with(' ') {
            response_lines.push(line.trim_start());
        } else if line.trim().is_empty() {
            response_lines.push("");
        }
    }

    let cleaned = response_lines.join("\n");
    let output = strip_legacy_markers(&cleaned);

    if output.trim().is_empty() {
        None
    } else {
        Some(output)
    }
}

fn interrupt_agent(bin: &str, target: &str) {
    let _ = command(bin)
        .args(["agent", "send-keys", target, "ctrl+c"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn execute_command_streaming(
    prompt: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    model: Option<&str>,
) -> Result<(), String> {
    let bin = herdr_path()
        .ok_or_else(|| "Herdr CLI not found. Set COKAC_HERDR_PATH or install herdr.".to_string())?;
    let target = target_from_model(model)?;
    let _active_prompt = ActivePromptGuard::new(&target);
    let before = read_agent(&bin, &target)?;

    let mut prompt_command = command(&bin);
    prompt_command
        .args([
            "agent",
            "prompt",
            &target,
            prompt,
            "--wait",
            "--until",
            "idle",
            "--until",
            "done",
            "--until",
            "blocked",
            "--timeout",
            &timeout_ms().to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    detach_into_own_pgroup(&mut prompt_command);
    attach_cancel_cgroup(&mut prompt_command, cancel_token.as_ref());
    let mut child = prompt_command
        .spawn()
        .map_err(|error| format!("Failed to run Herdr agent prompt: {error}"))?;

    if let Some(token) = cancel_token.as_ref() {
        let mut child_pid = token
            .child_pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *child_pid = Some(child.id());
        drop(child_pid);
        if token.cancelled.load(Ordering::Relaxed) {
            kill_child_tree(&mut child);
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|error| format!("Failed waiting for Herdr agent: {error}"))?;

    if let Some(token) = cancel_token.as_ref() {
        let mut child_pid = token
            .child_pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *child_pid = None;
        if token.cancelled.load(Ordering::Relaxed) {
            drop(child_pid);
            interrupt_agent(&bin, &target);
            return Ok(());
        }
    }

    if !output.status.success() {
        return Err(output_error("agent prompt", &output));
    }
    let prompt_result: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Herdr returned invalid prompt JSON: {error}"))?;
    if prompt_result.get("error").is_some() {
        return Err(format!("Herdr agent prompt failed: {prompt_result}"));
    }

    let after = read_agent(&bin, &target)?;
    let turn_output = current_turn_output(&before, &after, prompt);
    let response = codex_final_response(&turn_output).unwrap_or_else(|| {
        strip_legacy_markers(&turn_output)
    });
    if response.trim().is_empty() {
        return Err("Herdr agent completed without readable terminal output.".to_string());
    }

    send_success_terminal(&sender, Some(response.clone()), response, None)
        .map_err(|error| format!("Failed to publish Herdr response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_models_and_targets() {
        assert!(is_herdr_model(Some("herdr")));
        assert!(is_herdr_model(Some("herdr:worker_1")));
        assert!(!is_herdr_model(Some("codex")));
        assert_eq!(strip_herdr_prefix("herdr:worker"), Some("worker"));
        assert!(is_valid_target("worker-1"));
        assert!(!is_valid_target("Worker"));
        assert!(!is_valid_target("../worker"));
    }

    #[test]
    fn extracts_codex_final_response() {
        let snapshot = "\
  상태를 확인하겠습니다.\n\
\n\
  - 첫 번째 항목\n\
  - 두 번째 항목";
        assert_eq!(
            codex_final_response(snapshot).as_deref(),
            Some("상태를 확인하겠습니다.\n\n- 첫 번째 항목\n- 두 번째 항목")
        );
    }
}
