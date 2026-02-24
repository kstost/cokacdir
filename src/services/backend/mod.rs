pub mod claude;
pub mod codex;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Global flag: when true, AI backends skip all permission checks.
/// Set via `--madmax` CLI flag.
static MADMAX_MODE: AtomicBool = AtomicBool::new(false);

/// Enable madmax mode (skip all permission checks).
pub fn set_madmax(enabled: bool) {
    MADMAX_MODE.store(enabled, Ordering::Relaxed);
}

/// Check if madmax mode is active.
pub fn is_madmax() -> bool {
    MADMAX_MODE.load(Ordering::Relaxed)
}

/// Messages streamed from AI backend during execution
#[derive(Debug, Clone)]
pub enum BackendMessage {
    /// Initialization - contains session_id
    Init { session_id: String },
    /// Text response chunk
    Text(String),
    /// Tool use started
    ToolUse { name: String, input: String },
    /// Tool execution result
    ToolResult { content: String, is_error: bool },
    /// Completion with final response
    Complete { response: String },
    /// Error
    Error(String),
}

/// Token for cooperative cancellation of AI operations.
/// Holds a flag and the child process PID so the caller can terminate it.
pub struct CancelToken {
    pub cancelled: std::sync::atomic::AtomicBool,
    pub child_pid: std::sync::Mutex<Option<u32>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            child_pid: std::sync::Mutex::new(None),
        }
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // Kill child process if tracked
        if let Ok(guard) = self.child_pid.lock() {
            if let Some(pid) = *guard {
                #[cfg(unix)]
                {
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                }
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_child_pid(&self, pid: u32) {
        if let Ok(mut guard) = self.child_pid.lock() {
            *guard = Some(pid);
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait for AI backend implementations.
/// Each backend wraps a specific AI CLI tool (Claude, Codex, etc.)
/// and exposes a unified streaming execution interface.
#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    /// Execute a prompt with streaming output.
    ///
    /// Messages are sent to `sender` as they arrive. The function returns
    /// once the underlying process completes or is cancelled.
    async fn execute_streaming(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        working_dir: &str,
        sender: tokio::sync::mpsc::Sender<BackendMessage>,
        system_prompt: Option<&str>,
        allowed_tools: Option<&[String]>,
        cancel_token: Option<Arc<CancelToken>>,
    ) -> Result<(), String>;

    /// Human-readable name of this backend (e.g. "claude", "codex").
    fn name(&self) -> &str;

    /// Resolve the path to the backend binary, if available.
    fn binary_path(&self) -> Option<String>;

    /// Default set of tools this backend exposes.
    fn default_allowed_tools(&self) -> Vec<String>;
}

pub use claude::ClaudeBackend;
pub use codex::CodexBackend;
