//! Herdr agent provider.
//!
//! This adapter forwards a Cokacdir turn to an already-running Herdr agent,
//! waits for the agent to settle, then reads the terminal output back.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;

use serde_json::Value;

use crate::services::claude::{
    attach_cancel_cgroup, detach_into_own_pgroup, enhanced_path_for_bin, kill_child_tree,
    send_success_terminal, CancelToken, StreamMessage,
};

const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const DEFAULT_READ_LINES: u32 = 1000;

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

fn read_agent(bin: &str, target: &str) -> Result<String, String> {
    let output = command(bin)
        .args([
            "agent",
            "read",
            target,
            "--source",
            "recent-unwrapped",
            "--lines",
            &read_lines().to_string(),
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

fn current_turn_output(before: &str, after: &str, prompt: &str) -> String {
    let prompt = prompt.trim();
    if !prompt.is_empty() && !prompt.contains('\n') {
        let needle = format!("› {prompt}");
        if let Some(index) = after.rfind(&needle) {
            return after[index + needle.len()..].trim().to_string();
        }
    }
    snapshot_delta(before, after)
}

fn is_tui_separator(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= 20 && line.chars().all(|character| character == '─')
}

fn is_tui_elapsed_time(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('─') && line.contains("Worked for ") && line.ends_with('─')
}

fn codex_final_response(turn_output: &str) -> Option<String> {
    let lines: Vec<&str> = turn_output.lines().collect();
    let input_index = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with("› "))
        .unwrap_or(lines.len());
    let response_index = lines[..input_index]
        .iter()
        .rposition(|line| line.trim_start().starts_with("• "))?;
    let first_line = lines[response_index]
        .trim_start()
        .strip_prefix("• ")?
        .trim_end();
    let response_end = lines[response_index + 1..input_index]
        .iter()
        .position(|line| is_tui_separator(line) || is_tui_elapsed_time(line))
        .map(|offset| response_index + 1 + offset)
        .unwrap_or(input_index);

    let mut output = vec![first_line.to_string()];
    output.extend(lines[response_index + 1..response_end].iter().map(|line| {
        line.strip_prefix("  ")
            .unwrap_or(line)
            .trim_end()
            .to_string()
    }));
    let output = output.join("\n").trim().to_string();
    if output.is_empty() {
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
    let response = codex_final_response(&turn_output).unwrap_or(turn_output);
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
› Hello\n\
\n\
• I will check the status.\n\
\n\
• Ran git status\n\
  └ clean\n\
\n\
────────────────────────────────\n\
\n\
• Hello! How can I help?\n\
  - First item\n\
  - Second item\n\
\n\
────────────────────────────────\n\
\n\
› Use /skills to list available skills";
        assert_eq!(
            codex_final_response(snapshot).as_deref(),
            Some("Hello! How can I help?\n- First item\n- Second item")
        );
    }

    #[test]
    fn extracts_current_turn_without_separator() {
        let before = "\
• Previous answer\n\
\n\
› Use /skills to list available skills";
        let after = format!(
            "{before}\n\
\n\
› Reply normally.\n\
\n\
• Understood. I will reply normally.\n\
\n\
› Use /skills to list available skills"
        );
        let turn = current_turn_output(before, &after, "Reply normally.");
        assert_eq!(
            codex_final_response(&turn).as_deref(),
            Some("Understood. I will reply normally.")
        );
    }

    #[test]
    fn excludes_codex_elapsed_time_chrome() {
        let output = "\
• Completed the task.\n\
\n\
─ Worked for 1m 02s ─────────────────────────\n\
\n\
› Use /skills to list available skills";
        assert_eq!(
            codex_final_response(output).as_deref(),
            Some("Completed the task.")
        );
    }

    #[test]
    fn extracts_snapshot_suffix() {
        assert_eq!(
            snapshot_delta("old one\nold two", "old two\nnew one\nnew two"),
            "new one\nnew two"
        );
    }
}
