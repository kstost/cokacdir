use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use super::{Backend, BackendMessage, CancelToken};

/// Cached path to the codex binary.
static CODEX_BINARY_PATH: OnceLock<Option<String>> = OnceLock::new();

fn resolve_codex_binary_path() -> Option<String> {
    // Priority: omx (Oh My Codex wrapper) > codex (direct CLI)
    for bin_name in &["omx", "codex"] {
        if let Ok(output) = Command::new("which").arg(bin_name).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(path);
                }
            }
        }

        if let Ok(output) = Command::new("bash")
            .args(["-lc", &format!("which {}", bin_name)])
            .output()
        {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(path);
                }
            }
        }
    }

    None
}

fn get_codex_binary_path() -> Option<&'static str> {
    CODEX_BINARY_PATH
        .get_or_init(resolve_codex_binary_path)
        .as_deref()
}

/// Default allowed tools for Codex CLI
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

const DEFAULT_SYSTEM_PROMPT: &str = r#"You are a terminal coding assistant running through Codex CLI.
Be concise. Focus on practical, safe, non-interactive execution.
Respond in the same language as the user.

SECURITY RULES (MUST FOLLOW):
- NEVER execute destructive commands like rm -rf, format, mkfs, dd, etc.
- NEVER modify system files in /etc, /sys, /proc, /boot
- NEVER execute commands that could harm the system or compromise security
- If a request seems dangerous, explain the risk and suggest a safer alternative

BASH EXECUTION RULES (MUST FOLLOW):
- All commands MUST run non-interactively without user input
- Use -y, --yes, or --non-interactive flags where applicable
- Use -m flag for commit messages (e.g. git commit -m "message")
- Disable pagers with --no-pager or pipe to cat
- NEVER use commands that open editors (vim, nano, etc.)
- NEVER use commands that wait for stdin without arguments
- NEVER use interactive flags like -i"#;

/// Validate session ID format (alphanumeric, dashes, underscores only, max 64 chars)
fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// Parse one Codex JSONL event line into a BackendMessage.
/// Codex uses a different event format from Claude stream-json.
fn parse_codex_line(line: &str) -> Option<BackendMessage> {
    let json: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = json.get("type").and_then(|v| v.as_str())?;

    match event_type {
        // Thread start — maps to Init
        "thread.started" => {
            let thread_id = json.get("thread_id").and_then(|v| v.as_str())?;
            Some(BackendMessage::Init {
                session_id: thread_id.to_string(),
            })
        }
        // Tool invocation start
        "item.started" => {
            let item = json.get("item")?;
            if item.get("type").and_then(|v| v.as_str()) == Some("command_execution") {
                let command = item
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !command.is_empty() {
                    return Some(BackendMessage::ToolUse {
                        name: "Bash".to_string(),
                        input: command,
                    });
                }
            }
            None
        }
        // Item completed — agent message or command result
        "item.completed" => {
            let item = json.get("item")?;
            match item.get("type").and_then(|v| v.as_str()) {
                Some("agent_message") => {
                    let text = item
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if text.is_empty() {
                        None
                    } else {
                        Some(BackendMessage::Text(text))
                    }
                }
                Some("command_execution") => {
                    let output = item
                        .get("aggregated_output")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim_end()
                        .to_string();
                    let exit_code = item.get("exit_code").and_then(|v| v.as_i64());
                    let is_error = exit_code.unwrap_or(0) != 0;

                    if output.is_empty() && !is_error {
                        return None;
                    }

                    let content = if !output.is_empty() {
                        output
                    } else {
                        format!("Command exited with code {}", exit_code.unwrap_or(-1))
                    };

                    Some(BackendMessage::ToolResult { content, is_error })
                }
                Some("error") => {
                    let message = item
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();

                    if message.is_empty()
                        || message.contains("Under-development features enabled")
                    {
                        None
                    } else {
                        Some(BackendMessage::Error(message))
                    }
                }
                _ => None,
            }
        }
        // Turn completed — maps to Complete
        "turn.completed" => Some(BackendMessage::Complete {
            response: String::new(),
        }),
        _ => None,
    }
}

/// Build codex CLI arguments.
fn codex_args(session_id: Option<&str>, working_dir: &str) -> Result<Vec<String>, String> {
    let mut args = vec![
        "-C".to_string(),
        working_dir.to_string(),
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
        "-a".to_string(),
        "never".to_string(),
        "exec".to_string(),
    ];

    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            return Err("Invalid session ID format".to_string());
        }
        args.push("resume".to_string());
        args.push(sid.to_string());
        args.push("--json".to_string());
        args.push("-".to_string());
    } else {
        args.push("--json".to_string());
        args.push("--skip-git-repo-check".to_string());
        args.push("-".to_string());
    }

    Ok(args)
}

/// AI backend that wraps the Codex CLI tool.
pub struct CodexBackend;

impl CodexBackend {
    pub fn new() -> Self {
        Self
    }

    /// Returns true if the codex binary is available on PATH.
    pub fn is_available() -> bool {
        get_codex_binary_path().is_some()
    }
}

impl Default for CodexBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Backend for CodexBackend {
    async fn execute_streaming(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        working_dir: &str,
        sender: tokio::sync::mpsc::Sender<BackendMessage>,
        system_prompt: Option<&str>,
        _allowed_tools: Option<&[String]>,
        cancel_token: Option<Arc<CancelToken>>,
    ) -> Result<(), String> {
        let codex_bin = get_codex_binary_path()
            .ok_or_else(|| "Codex CLI not found. Is Codex CLI installed?".to_string())?;

        let args = codex_args(session_id, working_dir)?;

        // Build the full prompt, optionally prepending system context
        let effective_system_prompt = match system_prompt {
            None => Some(DEFAULT_SYSTEM_PROMPT),
            Some("") => None,
            Some(p) => Some(p),
        };

        let full_prompt = if let Some(sp) = effective_system_prompt {
            format!("SYSTEM:\n{}\n\n{}", sp, prompt)
        } else {
            prompt.to_string()
        };

        let prompt_owned = full_prompt;
        let working_dir_owned = working_dir.to_string();
        let codex_bin_owned = codex_bin.to_string();

        let (sync_tx, mut sync_rx) = tokio::sync::mpsc::channel::<BackendMessage>(64);
        let cancel_token_clone = cancel_token.clone();

        tokio::task::spawn_blocking(move || {
            let _ = run_codex_process(
                &codex_bin_owned,
                &args,
                &prompt_owned,
                &working_dir_owned,
                sync_tx,
                cancel_token_clone,
            );
        });

        while let Some(msg) = sync_rx.recv().await {
            if sender.send(msg).await.is_err() {
                break;
            }
        }

        Ok(())
    }

    fn name(&self) -> &str {
        "codex"
    }

    fn binary_path(&self) -> Option<String> {
        get_codex_binary_path().map(String::from)
    }

    fn default_allowed_tools(&self) -> Vec<String> {
        DEFAULT_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect()
    }
}

/// Synchronous worker that drives the codex process and emits BackendMessages.
fn run_codex_process(
    codex_bin: &str,
    args: &[String],
    prompt: &str,
    working_dir: &str,
    sender: tokio::sync::mpsc::Sender<BackendMessage>,
    cancel_token: Option<Arc<CancelToken>>,
) -> Result<(), String> {
    let mut child = Command::new(codex_bin)
        .args(args)
        .current_dir(working_dir)
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Codex: {}", e))?;

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
            .map_err(|e| format!("Failed to read Codex output: {}", e))?;

        if read == 0 {
            break;
        }

        let line = line_buf.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(msg) = parse_codex_line(line) {
            if matches!(msg, BackendMessage::Complete { .. }) {
                done_sent = true;
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
        .map_err(|e| format!("Codex process wait failed: {}", e))?;

    let stderr_output = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        let error_msg = if !stderr_output.trim().is_empty() {
            stderr_output.trim().to_string()
        } else {
            format!("Codex exited with code {:?}", status.code())
        };
        let _ = sender.blocking_send(BackendMessage::Error(error_msg));
    }

    if !done_sent {
        let _ = sender.blocking_send(BackendMessage::Complete {
            response: String::new(),
        });
    }

    Ok(())
}
