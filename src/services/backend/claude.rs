use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use super::{Backend, BackendMessage, CancelToken};

/// Cached path to the claude binary.
static CLAUDE_BINARY_PATH: OnceLock<Option<String>> = OnceLock::new();

/// Resolve the path to the claude binary.
/// First tries `which claude`, then falls back to `bash -lc "which claude"`
/// for non-interactive SSH sessions where ~/.profile isn't loaded.
fn resolve_claude_binary_path() -> Option<String> {
    if let Ok(output) = Command::new("which").arg("claude").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }

    if let Ok(output) = Command::new("bash")
        .args(["-lc", "which claude"])
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }

    None
}

fn get_claude_binary_path() -> Option<&'static str> {
    CLAUDE_BINARY_PATH
        .get_or_init(resolve_claude_binary_path)
        .as_deref()
}

/// Default allowed tools for Claude CLI
pub const DEFAULT_ALLOWED_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "Task",
    "TaskOutput",
    "TaskStop",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Skill",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
];

const DEFAULT_SYSTEM_PROMPT: &str = r#"You are a terminal file manager assistant. Be concise. Focus on file operations. Respond in the same language as the user.

SECURITY RULES (MUST FOLLOW):
- NEVER execute destructive commands like rm -rf, format, mkfs, dd, etc.
- NEVER modify system files in /etc, /sys, /proc, /boot
- NEVER access or modify files outside the current working directory without explicit user path
- NEVER execute commands that could harm the system or compromise security
- ONLY suggest safe file operations: copy, move, rename, create directory, view, edit
- If a request seems dangerous, explain the risk and suggest a safer alternative

BASH EXECUTION RULES (MUST FOLLOW):
- All commands MUST run non-interactively without user input
- Use -y, --yes, or --non-interactive flags (e.g., apt install -y, npm init -y)
- Use -m flag for commit messages (e.g., git commit -m "message")
- Disable pagers with --no-pager or pipe to cat (e.g., git --no-pager log)
- NEVER use commands that open editors (vim, nano, etc.)
- NEVER use commands that wait for stdin without arguments
- NEVER use interactive flags like -i"#;

/// Parse one stream-json JSONL line into a BackendMessage.
fn parse_line(line: &str) -> Option<BackendMessage> {
    let json: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = json.get("type").and_then(|v| v.as_str())?;

    match event_type {
        "system" => {
            if json.get("subtype").and_then(|v| v.as_str()) == Some("init") {
                let session_id = json
                    .get("session_id")
                    .and_then(|v| v.as_str())?
                    .to_string();
                Some(BackendMessage::Init { session_id })
            } else {
                None
            }
        }
        "assistant" => {
            let content = json
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|v| v.as_array())?;

            for block in content {
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        let text = block
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !text.is_empty() {
                            return Some(BackendMessage::Text(text));
                        }
                    }
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Tool")
                            .to_string();
                        let input = block
                            .get("input")
                            .map(|v| {
                                if let Some(s) = v.as_str() {
                                    s.to_string()
                                } else {
                                    serde_json::to_string(v).unwrap_or_default()
                                }
                            })
                            .unwrap_or_default();
                        return Some(BackendMessage::ToolUse { name, input });
                    }
                    _ => {}
                }
            }
            None
        }
        "user" => {
            let content = json
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|v| v.as_array())?;

            for item in content {
                if item.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                    let content_text =
                        if let Some(s) = item.get("content").and_then(|v| v.as_str()) {
                            s.to_string()
                        } else if let Some(arr) = item.get("content").and_then(|v| v.as_array()) {
                            arr.iter()
                                .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join("\n")
                        } else {
                            String::new()
                        };
                    let is_error = item
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    return Some(BackendMessage::ToolResult {
                        content: content_text,
                        is_error,
                    });
                }
            }
            None
        }
        "result" => {
            let is_error = json
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if is_error {
                let error_msg = json
                    .get("errors")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .or_else(|| {
                        json.get("result")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_else(|| "Unknown error".to_string());
                return Some(BackendMessage::Error(error_msg));
            }

            let response = json
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(BackendMessage::Complete { response })
        }
        _ => None,
    }
}

/// AI backend that wraps the Claude CLI tool.
pub struct ClaudeBackend;

impl ClaudeBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Backend for ClaudeBackend {
    async fn execute_streaming(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        working_dir: &str,
        sender: tokio::sync::mpsc::Sender<BackendMessage>,
        system_prompt: Option<&str>,
        allowed_tools: Option<&[String]>,
        cancel_token: Option<Arc<CancelToken>>,
    ) -> Result<(), String> {
        let claude_bin = get_claude_binary_path()
            .ok_or_else(|| "Claude CLI not found. Is Claude CLI installed?".to_string())?;

        let tools_str = match allowed_tools {
            Some(tools) => tools.join(","),
            None => DEFAULT_ALLOWED_TOOLS.join(","),
        };

        let mut args = vec![
            "-p".to_string(),
        ];

        if super::is_madmax() {
            args.push("--dangerously-skip-permissions".to_string());
        } else {
            args.push("--permission-mode".to_string());
            args.push("default".to_string());
        }

        args.extend([
            "--tools".to_string(),
            tools_str,
            "--verbose".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
        ]);

        let effective_system_prompt = match system_prompt {
            None => Some(DEFAULT_SYSTEM_PROMPT),
            Some("") => None,
            Some(p) => Some(p),
        };
        if let Some(sp) = effective_system_prompt {
            args.push("--append-system-prompt".to_string());
            args.push(sp.to_string());
        }

        if let Some(sid) = session_id {
            args.push("--resume".to_string());
            args.push(sid.to_string());
        }

        let prompt_owned = prompt.to_string();
        let working_dir_owned = working_dir.to_string();
        let claude_bin_owned = claude_bin.to_string();

        // Run blocking I/O in a dedicated thread
        let (sync_tx, mut sync_rx) = tokio::sync::mpsc::channel::<BackendMessage>(64);
        let cancel_token_clone = cancel_token.clone();

        tokio::task::spawn_blocking(move || {
            let _ = run_claude_process(
                &claude_bin_owned,
                &args,
                &prompt_owned,
                &working_dir_owned,
                sync_tx,
                cancel_token_clone,
            );
        });

        // Forward messages from the blocking thread to the caller's sender
        while let Some(msg) = sync_rx.recv().await {
            if sender.send(msg).await.is_err() {
                break;
            }
        }

        Ok(())
    }

    fn name(&self) -> &str {
        "claude"
    }

    fn binary_path(&self) -> Option<String> {
        get_claude_binary_path().map(String::from)
    }

    fn default_allowed_tools(&self) -> Vec<String> {
        DEFAULT_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect()
    }
}

/// Synchronous worker that drives the claude process and emits BackendMessages.
fn run_claude_process(
    claude_bin: &str,
    args: &[String],
    prompt: &str,
    working_dir: &str,
    sender: tokio::sync::mpsc::Sender<BackendMessage>,
    cancel_token: Option<Arc<CancelToken>>,
) -> Result<(), String> {
    let mut child = Command::new(claude_bin)
        .args(args)
        .current_dir(working_dir)
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000")
        .env("BASH_DEFAULT_TIMEOUT_MS", "86400000")
        .env("BASH_MAX_TIMEOUT_MS", "86400000")
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Claude: {}", e))?;

    if let Some(ref token) = cancel_token {
        token.set_child_pid(child.id());
    }

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture stdout".to_string())?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture stderr".to_string())?;

    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_string(&mut buf);
        buf
    });

    let mut reader = BufReader::new(stdout);
    let mut line_buf = String::new();
    let mut last_session_id: Option<String> = None;
    let mut done_sent = false;

    loop {
        if let Some(ref token) = cancel_token {
            if token.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
        }

        line_buf.clear();
        let read = reader
            .read_line(&mut line_buf)
            .map_err(|e| format!("Failed to read Claude output: {}", e))?;

        if read == 0 {
            break;
        }

        let line = line_buf.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(msg) = parse_line(line) {
            match &msg {
                BackendMessage::Init { session_id } => {
                    last_session_id = Some(session_id.clone());
                }
                BackendMessage::Complete { .. } => {
                    done_sent = true;
                }
                _ => {}
            }
            if sender.blocking_send(msg).is_err() {
                break;
            }
        }
    }

    if let Some(ref token) = cancel_token {
        if token.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("Claude process wait failed: {}", e))?;

    let stderr_output = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        let error_msg = if !stderr_output.trim().is_empty() {
            stderr_output.trim().to_string()
        } else {
            format!("Claude exited with code {:?}", status.code())
        };
        let _ = sender.blocking_send(BackendMessage::Error(error_msg));
    }

    if !done_sent {
        let response = last_session_id
            .as_deref()
            .map(|_| String::new())
            .unwrap_or_default();
        let _ = sender.blocking_send(BackendMessage::Complete { response });
    }

    Ok(())
}
