use std::process::{Command, Stdio};
use std::io::Write;
use std::sync::OnceLock;
use regex::Regex;

#[derive(Debug, Clone)]
pub struct ClaudeResponse {
    pub success: bool,
    pub response: Option<String>,
    pub session_id: Option<String>,
    pub error: Option<String>,
}

/// Cached regex pattern for session ID validation
fn session_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[a-zA-Z0-9_-]+$").expect("Invalid session ID regex pattern"))
}

/// Validate session ID format (alphanumeric, dashes, underscores only)
fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty() && session_id.len() < 256 && session_id_regex().is_match(session_id)
}

/// Execute a command using Claude CLI
pub fn execute_command(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
) -> ClaudeResponse {
    let mut args = vec![
        "-p".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--append-system-prompt".to_string(),
        r#"You are a terminal file manager assistant. Be concise. Focus on file operations. Suggest safe commands only. Respond in the same language as the user.

IMPORTANT: Format your responses using Markdown for better readability:
- Use **bold** for important terms or commands
- Use `code` for file paths, commands, and technical terms
- Use bullet lists (- item) for multiple items
- Use numbered lists (1. item) for sequential steps
- Use code blocks (```language) for multi-line code or command examples
- Use headers (## Title) to organize longer responses
- Keep formatting minimal and terminal-friendly"#.to_string(),
    ];

    // Resume session if available
    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some("Invalid session ID format".to_string()),
            };
        }
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }

    let mut child = match Command::new("claude")
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!("Failed to start Claude: {}. Is Claude CLI installed?", e)),
            };
        }
    };

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }

    // Wait for output
    match child.wait_with_output() {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                parse_claude_output(&stdout)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                ClaudeResponse {
                    success: false,
                    response: None,
                    session_id: None,
                    error: Some(if stderr.is_empty() {
                        format!("Process exited with code {:?}", output.status.code())
                    } else {
                        stderr
                    }),
                }
            }
        }
        Err(e) => ClaudeResponse {
            success: false,
            response: None,
            session_id: None,
            error: Some(format!("Failed to read output: {}", e)),
        },
    }
}

/// Parse Claude CLI JSON output
fn parse_claude_output(output: &str) -> ClaudeResponse {
    let mut session_id: Option<String> = None;
    let mut response_text = String::new();

    for line in output.trim().lines() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            // Extract session ID
            if let Some(sid) = json.get("session_id").and_then(|v| v.as_str()) {
                session_id = Some(sid.to_string());
            }

            // Extract response text
            if let Some(result) = json.get("result").and_then(|v| v.as_str()) {
                response_text = result.to_string();
            } else if let Some(message) = json.get("message").and_then(|v| v.as_str()) {
                response_text = message.to_string();
            } else if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                response_text = content.to_string();
            }
        } else if !line.trim().is_empty() && !line.starts_with('{') {
            response_text.push_str(line);
            response_text.push('\n');
        }
    }

    // If no structured response, use raw output
    if response_text.is_empty() {
        response_text = output.trim().to_string();
    }

    ClaudeResponse {
        success: true,
        response: Some(response_text.trim().to_string()),
        session_id,
        error: None,
    }
}

/// Check if Claude CLI is available
pub fn is_claude_available() -> bool {
    #[cfg(not(unix))]
    {
        false
    }

    #[cfg(unix)]
    {
        match Command::new("which")
            .arg("claude")
            .output()
        {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }
}

/// Check if platform supports AI features
pub fn is_ai_supported() -> bool {
    cfg!(unix)
}
