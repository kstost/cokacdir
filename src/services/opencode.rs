//! OpenCode service — spawns `opencode run --format json` and translates its
//! JSONL event stream into the existing `StreamMessage` / `ClaudeResponse` types.
//!
//! The public API mirrors the other CLI providers so callers can swap backends
//! with minimal code changes.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::services::claude::{
    debug_log_to, kill_child_tree, CancelToken, ClaudeResponse, StreamMessage,
};

// ============================================================
// opencode serve adapter constants (SSE-based execution path)
// ============================================================

/// Substring that identifies the "server listening" line in stdout/stderr.
const SERVE_READY_NEEDLE: &str = "listening on http://";
/// Maximum time to wait for opencode serve to print its readiness line.
const SERVE_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Base poll interval for completion detection. Matches oh-my-opencode run.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How many consecutive "looks complete" checks before we declare done.
const POLL_REQUIRED_CONSECUTIVE: u32 = 2;
/// Minimum time the session must have been busy at least once before we trust
/// an "all idle" reading. Protects against premature exits before the first
/// message.part.updated arrives.
const POLL_MIN_STABILIZATION: Duration = Duration::from_secs(1);
/// HTTP request timeout for individual poll endpoints.
const POLL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum consecutive HTTP error iterations in poll_until_complete before we
/// declare the turn dead. Covers the "opencode serve crashed mid-turn" case.
/// 6 * 500ms = ~3s of grace before bailing out.
const POLL_MAX_CONSECUTIVE_ERRORS: u32 = 6;

fn opencode_debug(msg: &str) {
    debug_log_to("opencode.log", msg);
}

/// Verify whether an OpenCode session's task has been fully completed.
///
/// Mirrors the contract of `claude::verify_completion` and
/// `codex::verify_completion_codex`:
/// returns `complete=true` iff the model's reply contains `mission_complete`
/// and not `mission_pending`; otherwise `feedback` carries the remaining-work
/// text with keywords stripped.
///
/// Isolation: uses OpenCode's native `--fork`. `opencode run --session <ORIG>
/// --fork` creates a branched session whose state is a copy of the original
/// at the time of forking — the original's rows are never written to
/// (empirically verified: hash identical before/after). `--agent plan`
/// applies the `plan` built-in agent to the fork; its default permissions
/// deny write/bash, which is the closest OpenCode offers to Claude's
/// `--tools ""` (soft, not hard).
///
/// Output is read as plain text on stdout — OpenCode's default formatter
/// writes exactly the agent's reply to stdout (header decorations go to
/// stderr), so no JSON parsing is needed. This mirrors the plain-text shape
/// used by `claude::verify_completion` and keeps this function symmetrical
/// with the Claude path.
///
/// Forked session rows are NOT cleaned up. Claude's `--fork-session` also
/// leaves a persisted .jsonl file in the user's project directory; OpenCode
/// forks persist the same way. Keeping these symmetrical avoids a DELETE
/// code path whose failure modes would be worse than the clutter it avoids.
pub fn verify_completion_opencode(
    session_id: &str,
    working_dir: &str,
) -> Result<crate::services::claude::VerifyResult, String> {
    opencode_debug("=== verify_completion_opencode START ===");
    opencode_debug(&format!("  session_id: {}", session_id));
    opencode_debug(&format!("  working_dir: {}", working_dir));

    let opencode_bin = resolve_opencode_path().unwrap_or_else(|| "opencode".to_string());
    opencode_debug(&format!("  opencode_bin: {}", opencode_bin));

    // Verify prompt is short because the fork already carries conversation
    // context. Spelling out "mission_complete" / "mission_pending" literally
    // is what the parser below looks for.
    let verify_prompt =
        "Review what you just did in this session. \
         Do NOT call any tools, do NOT read files, do NOT run commands — \
         judge purely from the conversation history visible to you.\n\n\
         If the task appears fully and safely complete, respond with ONLY the single word: mission_complete\n\n\
         Otherwise respond with: mission_pending\n\
         followed by ONE short follow-up instruction (1–2 sentences).\n\n\
         CRITICAL — what this follow-up instruction IS:\n\
         The text you write after `mission_pending` will be taken verbatim and \
         delivered as the NEXT USER MESSAGE to the very same working agent that \
         just performed the task. The agent will read it as if the user typed \
         it into the chat. Therefore write it as a direct, second-person \
         request from the user, not as a review/verdict/analysis.\n\n\
         The instruction should ask the agent to re-examine, re-verify, or \
         double-check whatever it just did — whatever form that work took. \
         Let the phrasing flow naturally from the actual work, not from a \
         fixed template.\n\n\
         Rules:\n\
         - Second-person imperative, as the user would type.\n\
         - NOT a diagnosis, NOT a checklist of missing items, NOT a summary \
           of what was done.\n\
         - Match the language of the preceding conversation.\n\
         - 1–2 sentences. No preface, no \"I think\", no meta commentary.";

    // Spawn the fork. No --format flag means plain text; stdin is closed so
    // opencode doesn't block on input.
    let spawn_start = std::time::Instant::now();
    let child = Command::new(&opencode_bin)
        .args([
            "run",
            "--session",
            session_id,
            "--fork",
            "--agent",
            "plan",
            verify_prompt,
        ])
        .current_dir(working_dir)
        .env(
            "PATH",
            crate::services::claude::enhanced_path_for_bin(&opencode_bin),
        )
        // Block `question` / `plan_exit` — both wait on a user reply via
        // opencode's `question.ask` Deferred and would hang the verify fork.
        // The verify prompt itself says "Do NOT call any tools", but the
        // `plan` agent's plan_exit is auto-called when planning concludes, so
        // the deny rule is a belt-and-braces safeguard. See the matching
        // comment in `build_opencode_command` / `spawn_opencode_serve`.
        .env(
            "OPENCODE_PERMISSION",
            r#"{"*":"allow","question":"deny","plan_exit":"deny"}"#,
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn opencode for verify: {}", e))?;
    opencode_debug(&format!(
        "  spawned in {:?}, pid={:?}",
        spawn_start.elapsed(),
        child.id()
    ));

    let wait_start = std::time::Instant::now();
    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to read opencode verify output: {}", e))?;
    opencode_debug(&format!(
        "  completed in {:?}, exit={:?}",
        wait_start.elapsed(),
        output.status.code()
    ));

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(format!(
            "verify_completion_opencode process failed (exit {:?}). stderr: {}",
            output.status.code(),
            crate::services::claude::safe_preview(&stderr, 500)
        ));
    }

    // OpenCode default format writes ONLY the agent's reply to stdout (the
    // "> plan · gpt-5.4" banner goes to stderr). There is no prompt echo,
    // so direct substring matching on stdout is safe.
    let reply = String::from_utf8_lossy(&output.stdout).to_string();
    opencode_debug(&format!(
        "  reply len={}, preview: {}",
        reply.len(),
        reply.chars().take(300).collect::<String>()
    ));

    if reply.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(format!(
            "verify_completion_opencode produced empty reply. stderr: {}",
            crate::services::claude::safe_preview(&stderr, 500)
        ));
    }

    // Same decision rule as claude::verify_completion: complete iff
    // `mission_complete` is present AND `mission_pending` is absent.
    let pending = reply.contains("mission_pending");
    let complete = reply.contains("mission_complete") && !pending;
    let feedback = if complete {
        None
    } else {
        let cleaned = reply
            .replace("mission_pending", "")
            .replace("mission_complete", "");
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.to_string())
        }
    };

    opencode_debug(&format!(
        "  complete={}, feedback_len={:?}",
        complete,
        feedback.as_ref().map(|s| s.len())
    ));
    opencode_debug("=== verify_completion_opencode END ===");

    Ok(crate::services::claude::VerifyResult { complete, feedback })
}

/// Truncate a string for log previews (char-boundary safe).
fn log_preview(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    // Find the last char boundary at or before max
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ============================================================
// OpenCode availability check
// ============================================================

static OPENCODE_AVAILABLE: OnceLock<bool> = OnceLock::new();

fn check_opencode_available() -> bool {
    opencode_debug("[check_opencode_available] START");

    if let Some(path) = resolve_opencode_path() {
        opencode_debug(&format!("[check_opencode_available] found: {}", path));
        return true;
    }

    opencode_debug("[check_opencode_available] NOT FOUND");
    false
}

pub fn is_opencode_available() -> bool {
    let result = *OPENCODE_AVAILABLE.get_or_init(check_opencode_available);
    opencode_debug(&format!("[is_opencode_available] result={}", result));
    result
}

/// Check if a model string refers to the OpenCode backend
pub fn is_opencode_model(model: Option<&str>) -> bool {
    let result = model
        .map(|m| m == "opencode" || m.starts_with("opencode:"))
        .unwrap_or(false);
    opencode_debug(&format!(
        "[is_opencode_model] model={:?} result={}",
        model, result
    ));
    result
}

/// Strip "opencode:" prefix and return the actual model name.
/// Returns None if the input is just "opencode" (use default).
/// Also strips display-name suffix (" — Description") if present.
pub fn strip_opencode_prefix(model: &str) -> Option<&str> {
    let result = model
        .strip_prefix("opencode:")
        .filter(|s| !s.is_empty())
        .map(|s| s.split(" \u{2014} ").next().unwrap_or(s).trim());
    opencode_debug(&format!(
        "[strip_opencode_prefix] model={:?} result={:?}",
        model, result
    ));
    result
}

// ============================================================
// List available models (cached)
// ============================================================

static OPENCODE_MODELS: OnceLock<Vec<String>> = OnceLock::new();

/// Fetch available models by running `opencode models`.
/// Result is cached for the process lifetime.
pub fn list_models() -> &'static [String] {
    OPENCODE_MODELS.get_or_init(|| {
        opencode_debug("[list_models] fetching model list...");
        let bin = resolve_opencode_path().unwrap_or_else(|| "opencode".to_string());
        let output = match Command::new(&bin).args(["models"]).output() {
            Ok(o) => o,
            Err(e) => {
                opencode_debug(&format!("[list_models] FAILED to run '{}': {}", bin, e));
                return Vec::new();
            }
        };
        if !output.status.success() {
            opencode_debug(&format!(
                "[list_models] exit code {:?}",
                output.status.code()
            ));
            return Vec::new();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let models: Vec<String> = stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with('{'))
            .collect();
        opencode_debug(&format!(
            "[list_models] found {} models: {:?}",
            models.len(),
            models
        ));
        models
    })
}

// ============================================================
// Resolve opencode binary path
// ============================================================

fn resolve_opencode_path() -> Option<String> {
    opencode_debug("[resolve_opencode_path] START");

    if let Ok(val) = std::env::var("COKAC_OPENCODE_PATH") {
        if !val.is_empty() && opencode_path_is_runnable(&val) {
            opencode_debug(&format!(
                "[resolve_opencode_path] COKAC_OPENCODE_PATH={}",
                val
            ));
            #[cfg(windows)]
            if let Some(path) = opencode_native_exe_for_wrapper(&val) {
                opencode_debug(&format!(
                    "[resolve_opencode_path] env wrapper -> native exe {}",
                    path
                ));
                return Some(path);
            }
            return Some(val);
        }
    }

    #[cfg(unix)]
    {
        if let Ok(output) = Command::new("which").arg("opencode").output() {
            if output.status.success() {
                let p = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !p.is_empty() && opencode_path_is_runnable(&p) {
                    opencode_debug(&format!("[resolve_opencode_path] which → {}", p));
                    return Some(p);
                }
            }
        }
        if let Ok(output) = Command::new("bash")
            .args(["-lc", "which opencode"])
            .output()
        {
            if output.status.success() {
                let p = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !p.is_empty() && opencode_path_is_runnable(&p) {
                    opencode_debug(&format!("[resolve_opencode_path] bash -lc which → {}", p));
                    return Some(p);
                }
            }
        }
    }

    #[cfg(windows)]
    {
        // Prefer native executables over npm .cmd wrappers. Rust can run
        // .cmd/.bat files, but doing so goes through cmd.exe and adds a batch
        // argument-escaping layer for arbitrary user prompts.
        if let Some(path) = crate::services::claude::search_path_wide("opencode", Some(".exe")) {
            opencode_debug(&format!(
                "[resolve_opencode_path] SearchPathW .exe -> {}",
                path
            ));
            return Some(path);
        }
        // npm also installs an extensionless POSIX shell script named
        // `opencode`; CreateProcess cannot run it. The .cmd wrapper normally
        // sits next to node_modules/opencode-ai/bin/opencode.exe, which is the
        // safer target when present.
        if let Some(path) = crate::services::claude::search_path_wide("opencode", Some(".cmd")) {
            if let Some(native) = opencode_native_exe_for_wrapper(&path) {
                opencode_debug(&format!(
                    "[resolve_opencode_path] SearchPathW .cmd -> native exe {}",
                    native
                ));
                return Some(native);
            }
            opencode_debug(&format!(
                "[resolve_opencode_path] SearchPathW .cmd -> {}",
                path
            ));
            return Some(path);
        }
        if let Ok(output) = Command::new("where.exe").arg("opencode").output() {
            if output.status.success() {
                let decoded = crate::services::claude::decode_windows_output(&output.stdout);
                for p in decoded.lines().map(str::trim).filter(|p| !p.is_empty()) {
                    if opencode_path_is_runnable(p) {
                        if let Some(native) = opencode_native_exe_for_wrapper(p) {
                            opencode_debug(&format!(
                                "[resolve_opencode_path] where -> native exe {}",
                                native
                            ));
                            return Some(native);
                        }
                        opencode_debug(&format!("[resolve_opencode_path] where -> {}", p));
                        return Some(p.to_string());
                    }
                }
            }
        }
        if let Ok(output) = Command::new("cmd").args(["/c", "npm root -g"]).output() {
            if output.status.success() {
                let npm_root = crate::services::claude::decode_windows_output(&output.stdout)
                    .trim()
                    .to_string();
                let p = std::path::Path::new(&npm_root)
                    .join("opencode-ai")
                    .join("bin")
                    .join("opencode.exe");
                if p.exists() {
                    let p = p.display().to_string();
                    opencode_debug(&format!(
                        "[resolve_opencode_path] npm root fallback -> {}",
                        p
                    ));
                    return Some(p);
                }
            }
        }
    }

    opencode_debug("[resolve_opencode_path] NOT FOUND, will use 'opencode'");
    None
}

fn opencode_path_is_runnable(path: &str) -> bool {
    let p = std::path::Path::new(path);
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
        true
    }
}

#[cfg(windows)]
fn opencode_native_exe_for_wrapper(path: &str) -> Option<String> {
    let wrapper = std::path::Path::new(path);
    let ext = wrapper
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "cmd" && ext != "bat" {
        return None;
    }
    let parent = wrapper.parent()?;
    let p = parent
        .join("node_modules")
        .join("opencode-ai")
        .join("bin")
        .join("opencode.exe");
    if p.exists() {
        Some(p.display().to_string())
    } else {
        None
    }
}

// ============================================================
// Inject system prompt into AGENTS.md with automatic restore
// ============================================================
//
// OpenCode reads project instructions from AGENTS.md (preferred) or CLAUDE.md.
// If AGENTS.md exists, CLAUDE.md is ignored entirely.
//
// Safety requirements:
//   - The user's original AGENTS.md must NEVER be lost or corrupted.
//   - Recovery must work after SIGKILL, crash, or power loss.
//   - Concurrent execution on the same directory must not corrupt files.
//
// Strategy:
//   1. Acquire a PID-based lock file to prevent concurrent access.
//   2. Recover from any previous crash (leftover backup/sentinel).
//   3. Back up original AGENTS.md atomically (write tmp → rename).
//      If no original exists, write a sentinel file instead.
//   4. Only AFTER backup is confirmed on disk, write the modified AGENTS.md.
//   5. On Drop: restore from backup (or delete if sentinel), then release lock.
//   6. On next call: detect leftover backup/sentinel and auto-recover.

const AGENTS_MD: &str = "AGENTS.md";
const BACKUP_FILE: &str = ".AGENTS.md.cokacdir-backup";
const NO_ORIGINAL_SENTINEL: &str = ".AGENTS.md.cokacdir-no-original";
const LOCK_FILE: &str = ".AGENTS.md.cokacdir-lock";

/// Check if a process with the given PID is still alive.
/// Note: on Unix, uses `kill -0` which requires same-user ownership.
/// A process owned by a different user returns false (EPERM → exit code 1).
/// This is acceptable because cokacdir instances on the same directory
/// are always run by the same user.
fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use std::process::Command;
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        // Conservative: assume alive to avoid stealing lock.
        let _ = pid;
        true
    }
}

/// Try to acquire a PID-based lock file. Returns false if another live
/// process holds the lock (concurrent execution on same directory).
/// Uses O_EXCL (create_new) for atomic creation to prevent TOCTOU races.
fn try_acquire_lock(dir: &std::path::Path) -> bool {
    use std::io::Write;
    let lock_path = dir.join(LOCK_FILE);
    let my_pid = std::process::id();

    // Attempt 1: atomic create (O_EXCL) — fails if file already exists
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(mut f) => {
            let _ = f.write_all(my_pid.to_string().as_bytes());
            opencode_debug(&format!("[lock] acquired (PID={})", my_pid));
            return true;
        }
        Err(_) => {
            // File exists — check if the holder is still alive
        }
    }

    // Read existing lock to check liveness
    let content = match std::fs::read_to_string(&lock_path) {
        Ok(c) => c,
        Err(_) => {
            opencode_debug("[lock] cannot read existing lock file, skipping");
            return false;
        }
    };
    let holder_pid = match content.trim().parse::<u32>() {
        Ok(p) => p,
        Err(_) => {
            opencode_debug("[lock] lock file has invalid content, treating as stale");
            0 // treat as dead
        }
    };

    if holder_pid != my_pid && holder_pid != 0 && is_pid_alive(holder_pid) {
        opencode_debug(&format!(
            "[lock] another process (PID={}) holds the lock, skipping injection",
            holder_pid
        ));
        return false;
    }

    // Stale lock from dead process — remove and retry with O_EXCL
    opencode_debug(&format!(
        "[lock] stale lock from dead PID={}, taking over",
        holder_pid
    ));
    let _ = std::fs::remove_file(&lock_path);

    // Attempt 2: another process might have grabbed it between remove and create
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(mut f) => {
            let _ = f.write_all(my_pid.to_string().as_bytes());
            opencode_debug(&format!("[lock] acquired on retry (PID={})", my_pid));
            true
        }
        Err(_) => {
            // Another process won the race — that's fine, we skip injection
            opencode_debug("[lock] lost race on retry, skipping injection");
            false
        }
    }
}

fn release_lock(dir: &std::path::Path) {
    let lock_path = dir.join(LOCK_FILE);
    let _ = std::fs::remove_file(&lock_path);
    opencode_debug("[lock] released");
}

/// Write content to a file atomically: write to a temp file in the same
/// directory, then rename. This prevents partial writes from corrupting
/// the target file.
fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("cokacdir-tmp");
    if let Err(e) = std::fs::write(&tmp, content) {
        // tmp write failed — clean up partial tmp and return error
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        // rename failed — clean up tmp, do NOT attempt non-atomic fallback
        opencode_debug(&format!("[atomic_write] rename failed: {}", e));
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Recover from a previous crash: if a backup/sentinel file exists, restore.
/// Called at the start of every inject to guarantee AGENTS.md is clean.
/// Also cleans up any leftover tmp files from interrupted atomic_write.
fn recover_agents_md_if_needed(dir: &std::path::Path) {
    // Clean up leftover tmp file from interrupted atomic_write
    let tmp_path = dir.join(BACKUP_FILE).with_extension("cokacdir-tmp");
    if tmp_path.exists() {
        opencode_debug("[recover] removing leftover tmp file");
        let _ = std::fs::remove_file(&tmp_path);
    }

    let agents_path = dir.join(AGENTS_MD);
    let backup_path = dir.join(BACKUP_FILE);
    let sentinel_path = dir.join(NO_ORIGINAL_SENTINEL);

    if sentinel_path.exists() {
        // Original file did not exist → remove injected AGENTS.md and sentinel
        opencode_debug("[recover] sentinel found → removing injected AGENTS.md");
        let _ = std::fs::remove_file(&agents_path);
        if !agents_path.exists() {
            // AGENTS.md is gone — safe to remove sentinel
            let _ = std::fs::remove_file(&sentinel_path);
            opencode_debug("[recover] cleaned up sentinel");
        } else {
            // Cannot delete AGENTS.md — keep sentinel for next recovery attempt
            opencode_debug(
                "[recover] cannot delete AGENTS.md, sentinel preserved for next recovery",
            );
        }
        // Also clean up any stale lock
        release_lock(dir);
    } else if backup_path.exists() {
        // Original file existed → restore from backup
        opencode_debug("[recover] backup found → restoring original AGENTS.md");
        match std::fs::rename(&backup_path, &agents_path) {
            Ok(()) => opencode_debug("[recover] restored OK (rename)"),
            Err(e) => {
                opencode_debug(&format!(
                    "[recover] rename failed ({}), trying read+write",
                    e
                ));
                match std::fs::read_to_string(&backup_path) {
                    Ok(content) => {
                        match std::fs::write(&agents_path, &content) {
                            Ok(()) => {
                                // Only delete backup AFTER successful restore
                                let _ = std::fs::remove_file(&backup_path);
                                opencode_debug("[recover] restored OK (copy+delete)");
                            }
                            Err(e2) => {
                                // Write failed — do NOT delete backup
                                opencode_debug(&format!("[recover] CRITICAL: restore write FAILED: {}, backup preserved", e2));
                            }
                        }
                    }
                    Err(e2) => {
                        opencode_debug(&format!("[recover] CRITICAL: cannot read backup: {}", e2));
                        // Leave backup in place — don't delete what we can't read.
                    }
                }
            }
        }
        release_lock(dir);
    }
}

/// RAII guard that restores AGENTS.md to its original state when dropped.
struct AgentsMdGuard {
    dir: std::path::PathBuf,
    /// true = original file existed and was backed up to BACKUP_FILE.
    /// false = original file did not exist; sentinel was written.
    had_original: bool,
}

impl Drop for AgentsMdGuard {
    fn drop(&mut self) {
        let agents_path = self.dir.join(AGENTS_MD);
        let backup_path = self.dir.join(BACKUP_FILE);
        let sentinel_path = self.dir.join(NO_ORIGINAL_SENTINEL);

        if self.had_original {
            opencode_debug("[AgentsMdGuard] restoring original AGENTS.md from backup");
            match std::fs::rename(&backup_path, &agents_path) {
                Ok(()) => opencode_debug("[AgentsMdGuard] restored OK (rename)"),
                Err(e) => {
                    opencode_debug(&format!(
                        "[AgentsMdGuard] rename failed ({}), trying read+write",
                        e
                    ));
                    match std::fs::read_to_string(&backup_path) {
                        Ok(content) => {
                            match std::fs::write(&agents_path, &content) {
                                Ok(()) => {
                                    opencode_debug("[AgentsMdGuard] restored OK (copy)");
                                    // Only delete backup AFTER successful restore
                                    let _ = std::fs::remove_file(&backup_path);
                                }
                                Err(e2) => {
                                    // Write failed — do NOT delete backup, it's the only copy of the original
                                    opencode_debug(&format!("[AgentsMdGuard] CRITICAL: restore write FAILED: {}, backup preserved for recovery", e2));
                                }
                            }
                        }
                        Err(e2) => {
                            // Backup unreadable — do NOT delete it. Leave it for manual recovery.
                            opencode_debug(&format!("[AgentsMdGuard] CRITICAL: backup unreadable ({}), leaving for manual recovery", e2));
                        }
                    }
                }
            }
        } else {
            opencode_debug("[AgentsMdGuard] removing injected AGENTS.md (no original)");
            let _ = std::fs::remove_file(&agents_path);
            if !agents_path.exists() {
                // AGENTS.md is gone — safe to remove sentinel
                let _ = std::fs::remove_file(&sentinel_path);
            } else {
                // Cannot delete AGENTS.md — keep sentinel for crash recovery
                opencode_debug(
                    "[AgentsMdGuard] cannot delete AGENTS.md, sentinel preserved for recovery",
                );
            }
        }

        release_lock(&self.dir);
    }
}

/// Inject system prompt into AGENTS.md, prepended before any existing content.
/// Returns `Some(guard)` on success (guard restores on drop), or `None` if
/// injection was skipped (lock held by another process, or write failures).
fn inject_system_prompt_into_agents_md(
    working_dir: &str,
    system_prompt: &str,
) -> Option<AgentsMdGuard> {
    let dir = std::path::Path::new(working_dir);
    let agents_path = dir.join(AGENTS_MD);
    let backup_path = dir.join(BACKUP_FILE);
    let sentinel_path = dir.join(NO_ORIGINAL_SENTINEL);

    opencode_debug(&format!(
        "[inject_agents_md] dir={} system_prompt_len={}",
        working_dir,
        system_prompt.len()
    ));

    // Step 0: recover from any previous crash
    recover_agents_md_if_needed(dir);

    // Step 1: acquire lock (prevents concurrent corruption)
    if !try_acquire_lock(dir) {
        opencode_debug("[inject_agents_md] SKIPPED: could not acquire lock");
        return None;
    }

    // Step 2: back up original to disk (atomically)
    let had_original = agents_path.exists();
    if had_original {
        let original = match std::fs::read_to_string(&agents_path) {
            Ok(c) => c,
            Err(e) => {
                opencode_debug(&format!(
                    "[inject_agents_md] ABORT: cannot read original AGENTS.md: {}",
                    e
                ));
                release_lock(dir);
                return None;
            }
        };
        opencode_debug(&format!(
            "[inject_agents_md] backing up original ({} bytes)",
            original.len()
        ));

        // Atomic write: tmp file → rename to backup
        if let Err(e) = atomic_write(&backup_path, &original) {
            opencode_debug(&format!(
                "[inject_agents_md] ABORT: backup write FAILED: {}",
                e
            ));
            release_lock(dir);
            return None; // Do NOT touch AGENTS.md without a confirmed backup
        }
        opencode_debug("[inject_agents_md] backup confirmed on disk");

        // Step 3: NOW safe to write combined content
        let combined = format!("{}\n\n{}\n", system_prompt, original.trim());
        if let Err(e) = std::fs::write(&agents_path, &combined) {
            opencode_debug(&format!(
                "[inject_agents_md] combined write FAILED: {}, restoring from backup",
                e
            ));
            // Immediately restore — backup is confirmed intact (atomic_write succeeded).
            // If rename also fails, leave backup in place for crash recovery.
            match std::fs::rename(&backup_path, &agents_path) {
                Ok(()) => opencode_debug("[inject_agents_md] restored from backup OK"),
                Err(e2) => opencode_debug(&format!(
                    "[inject_agents_md] restore rename also FAILED: {} — backup preserved at {:?} for recovery",
                    e2, backup_path)),
            }
            release_lock(dir);
            return None;
        }
        opencode_debug(&format!(
            "[inject_agents_md] injected OK ({} bytes)",
            combined.len()
        ));
    } else {
        // No original → write sentinel FIRST (so crash recovery knows to delete)
        opencode_debug("[inject_agents_md] no original AGENTS.md");
        if let Err(e) = std::fs::write(&sentinel_path, "") {
            opencode_debug(&format!(
                "[inject_agents_md] ABORT: sentinel write FAILED: {}",
                e
            ));
            release_lock(dir);
            return None; // Do NOT create AGENTS.md without a sentinel
        }
        opencode_debug("[inject_agents_md] sentinel written");

        // NOW safe to write AGENTS.md
        let content = format!("{}\n", system_prompt);
        if let Err(e) = std::fs::write(&agents_path, &content) {
            opencode_debug(&format!("[inject_agents_md] write FAILED: {}", e));
            // Partial write may have left a corrupted AGENTS.md — try to delete it.
            match std::fs::remove_file(&agents_path) {
                Ok(()) => {
                    // AGENTS.md cleaned up — safe to remove sentinel
                    let _ = std::fs::remove_file(&sentinel_path);
                    opencode_debug("[inject_agents_md] cleaned up partial AGENTS.md + sentinel");
                }
                Err(_) => {
                    // Cannot delete corrupted AGENTS.md — keep sentinel for crash recovery
                    opencode_debug("[inject_agents_md] cannot delete partial AGENTS.md, sentinel preserved for recovery");
                }
            }
            release_lock(dir);
            return None;
        }
        opencode_debug(&format!(
            "[inject_agents_md] created AGENTS.md ({} bytes)",
            content.len()
        ));
    }

    Some(AgentsMdGuard {
        dir: dir.to_path_buf(),
        had_original,
    })
}

// ============================================================
// Build the `opencode run` command
// ============================================================

fn build_opencode_command(
    session_id: Option<&str>,
    working_dir: &str,
    system_prompt_file: Option<&str>,
    model: Option<&str>,
) -> (Command, Option<std::path::PathBuf>) {
    let opencode_bin = resolve_opencode_path().unwrap_or_else(|| "opencode".to_string());
    opencode_debug(&format!(
        "[build_cmd] bin={} working_dir={} session_id={:?} model={:?}",
        opencode_bin, working_dir, session_id, model
    ));

    let mut args: Vec<String> = vec!["run".into(), "--format".into(), "json".into()];

    // Working directory
    args.push("--dir".into());
    args.push(working_dir.into());

    // Model
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.to_string());
    }

    // Session resume — pass only --session, never combined with --continue.
    // opencode prioritizes --continue and routes to session.list().find(s => !s.parentID),
    // i.e. the most recent root session, which silently ignores --session and causes
    // cross-session routing. --session alone correctly resumes the exact session.
    if let Some(sid) = session_id {
        args.push("--session".into());
        args.push(sid.to_string());
    }

    // System prompt is written to AGENTS.md in working_dir by the caller
    // (opencode reads AGENTS.md as project instructions automatically)
    let sp_path: Option<std::path::PathBuf> = None;
    let _ = system_prompt_file;

    opencode_debug(&format!(
        "[build_cmd] full args: {} {}",
        opencode_bin,
        args.join(" ")
    ));

    let mut cmd = Command::new(&opencode_bin);
    cmd.args(&args)
        .current_dir(working_dir)
        // `question` and `plan_exit` are the only opencode tools that block on
        // a user reply through opencode's `question.ask` Deferred. cokacdir's
        // Telegram flow has no handler that posts answers back, so an AI call
        // to either tool would hang the session forever. Deny them explicitly
        // while keeping every other tool allowed. Permission evaluation uses
        // `findLast` so the trailing keys override the leading `*` rule.
        .env(
            "OPENCODE_PERMISSION",
            r#"{"*":"allow","question":"deny","plan_exit":"deny"}"#,
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::services::claude::detach_into_own_pgroup(&mut cmd);

    (cmd, sp_path)
}

// ============================================================
// Parse opencode JSONL events → StreamMessage
// ============================================================

/// Extract text content from an opencode `text` event
fn parse_text_event(json: &Value) -> Option<String> {
    json.get("part")
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .map(String::from)
}

/// Normalize opencode's lowercase tool names to PascalCase (system standard).
///
/// Verified against opencode v1.15.5 `packages/opencode/src/tool/*.ts` —
/// each `Tool.define("<id>", …)` first-arg is the wire-level tool name we
/// receive on `message.part.updated` events with `part.type == "tool"`.
///
/// The first block lists every tool ID opencode 1.15.5 actually emits; the
/// second block is legacy aliases (older opencode versions / Claude-Code
/// alternate names) kept for backward compatibility — opencode 1.15.5 does
/// not emit them, so they are dead in the current version but harmless and
/// useful when running against older binaries.
fn normalize_tool_name(name: &str) -> String {
    match name {
        // ── opencode 1.15.5 tool IDs (verified against tool registry) ──
        "bash" => "Bash",
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "glob" => "Glob",
        "grep" => "Grep",
        "webfetch" => "WebFetch",
        "websearch" => "WebSearch",
        "task" => "Task",
        "task_status" => "TaskStatus",
        "skill" => "Skill",
        "todowrite" => "TodoWrite",
        "question" => "Question",
        "plan_exit" => "PlanExit",
        "lsp" => "Lsp",
        "repo_clone" => "RepoClone",
        "repo_overview" => "RepoOverview",
        "invalid" => "Invalid",
        "apply_patch" => "Edit",
        // ── Legacy aliases (older opencode / Claude-Code parity) ──
        "notebookedit" => "NotebookEdit",
        "list" => "Glob",
        "taskoutput" => "TaskOutput",
        "taskstop" => "TaskStop",
        "taskcreate" => "TaskCreate",
        "taskupdate" => "TaskUpdate",
        "taskget" => "TaskGet",
        "tasklist" => "TaskList",
        "todoread" => "TodoRead",
        "askuserquestion" => "AskUserQuestion",
        "enterplanmode" => "EnterPlanMode",
        "exitplanmode" => "ExitPlanMode",
        "codesearch" => "Grep",
        _ => name,
    }
    .to_string()
}

/// Normalize OpenCode tool input field names to Claude-compatible names.
///
/// opencode 1.15.5 uses **camelCase** for tool parameters (e.g. `filePath`,
/// `oldString`, `replaceAll`, `include`), while cokacdir's UI renderer in
/// `ui/ai_screen.rs` looks up **snake_case** keys (`file_path`, `old_string`,
/// `replace_all`, `glob`). Without this normalization the UI would display
/// empty file paths and missing parameters for `write`, `edit`, and `grep`
/// tool calls coming from opencode.
///
/// Per-tool key map (opencode wire key → cokacdir canonical key):
/// - `read`  : filePath → file_path
/// - `write` : filePath → file_path
/// - `edit`  : filePath → file_path, oldString → old_string, newString → new_string, replaceAll → replace_all
/// - `grep`  : include → glob
/// - `apply_patch` : synth file_path from `*** Add/Update/Delete File:` line
/// - `skill` : name → skill
///
/// Other opencode tools (`bash`, `glob`, `webfetch`, `websearch`, `task`,
/// `task_status`, `lsp`, `repo_clone`, `repo_overview`, `question`,
/// `plan_exit`, `invalid`, `todowrite`) already use keys the UI renderer
/// accepts as-is, so they need no normalization.
fn normalize_opencode_params(tool: &str, input: &Value) -> Value {
    let Some(obj) = input.as_object() else {
        return input.clone();
    };
    let mut out = obj.clone();

    // Rename a single camelCase key to its snake_case canonical form, only if
    // the snake_case key is not already present (so we never clobber an
    // already-correct value emitted by a hypothetical future opencode that
    // adopts snake_case).
    fn rename(out: &mut serde_json::Map<String, Value>, from: &str, to: &str) {
        if out.contains_key(from) && !out.contains_key(to) {
            if let Some(v) = out.remove(from) {
                out.insert(to.to_string(), v);
            }
        }
    }

    match tool {
        "read" => {
            rename(&mut out, "filePath", "file_path");
        }
        "write" => {
            rename(&mut out, "filePath", "file_path");
        }
        "edit" => {
            rename(&mut out, "filePath", "file_path");
            rename(&mut out, "oldString", "old_string");
            rename(&mut out, "newString", "new_string");
            rename(&mut out, "replaceAll", "replace_all");
        }
        "grep" => {
            // opencode's grep tool uses `include` (file-glob filter) while
            // cokacdir's UI displays it under the canonical `glob` key — same
            // semantic, different name.
            rename(&mut out, "include", "glob");
        }
        "lsp" => {
            // No "Lsp" handler in ui/ai_screen.rs today — display falls through
            // to the generic key-listing branch — but normalize here so the
            // listed keys read consistently with the rest of the system
            // (snake_case), and so a future Lsp-specific UI handler can read
            // `file_path` like the other file-touching tools.
            rename(&mut out, "filePath", "file_path");
        }
        "apply_patch" => {
            // Extract file_path from patchText for display
            if let Some(patch) = out.get("patchText").and_then(|v| v.as_str()) {
                let file_path = patch.lines().find_map(|l| {
                    l.strip_prefix("*** Add File: ")
                        .or_else(|| l.strip_prefix("*** Update File: "))
                        .or_else(|| l.strip_prefix("*** Delete File: "))
                });
                if let Some(fp) = file_path {
                    out.insert("file_path".to_string(), Value::String(fp.to_string()));
                }
            }
        }
        "skill" => {
            rename(&mut out, "name", "skill");
        }
        _ => {}
    }

    Value::Object(out)
}

/// Extract tool use info from an opencode `tool_use` event
fn parse_tool_use_event(json: &Value) -> Option<(String, String, String, String, bool)> {
    let part = json.get("part")?;
    let raw_name = part
        .get("tool")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let tool_name = normalize_tool_name(raw_name);
    let call_id = part
        .get("callID")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let state = part.get("state")?;
    let status = state.get("status").and_then(|v| v.as_str()).unwrap_or("");

    let raw_input = state
        .get("input")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let normalized_input = normalize_opencode_params(raw_name, &raw_input);
    if raw_input != normalized_input {
        opencode_debug(&format!(
            "[parse_tool_use] normalized params for {}: {:?}→{:?}",
            raw_name,
            raw_input.as_object().map(|o| o.keys().collect::<Vec<_>>()),
            normalized_input
                .as_object()
                .map(|o| o.keys().collect::<Vec<_>>())
        ));
    }
    let input = serde_json::to_string_pretty(&normalized_input).unwrap_or_default();

    let (output, is_error) = match status {
        "completed" => {
            let out = state
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            (out, false)
        }
        "error" => {
            let err = state
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("Tool error")
                .to_string();
            (err, true)
        }
        _ => (String::new(), false),
    };

    opencode_debug(&format!(
        "[parse_tool_use] tool={} call_id={} status={} input_len={} output_len={} is_error={}",
        tool_name,
        call_id,
        status,
        input.len(),
        output.len(),
        is_error
    ));
    Some((tool_name, call_id, input, output, is_error))
}

/// Extract session ID from any event
fn extract_session_id(json: &Value) -> Option<String> {
    json.get("sessionID")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Extract tokens/cost from step_finish event
fn extract_step_finish(json: &Value) -> (Option<String>, bool) {
    let part = match json.get("part") {
        Some(p) => p,
        None => {
            opencode_debug("[extract_step_finish] no 'part' field");
            return (None, false);
        }
    };
    let reason = part.get("reason").and_then(|v| v.as_str()).unwrap_or("");
    // opencode's own loop exit check (packages/opencode/src/session/prompt.ts) treats
    // any finish reason that is not "tool-calls" or "unknown" as a terminal step.
    // Common terminal reasons: "stop" (normal), "length" (max_tokens hit),
    // "content-filter" (blocked), "error" (provider error surfaced as finish).
    // Treating only "stop" as final previously caused false "empty response" errors
    // when the model legitimately stopped for length/content-filter/error with no text.
    let is_final = matches!(reason, "stop" | "length" | "content-filter" | "error");
    let cost = part.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0);

    // Extract token details
    let tokens = part.get("tokens");
    let total_tokens = tokens
        .and_then(|t| t.get("total"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let input_tokens = tokens
        .and_then(|t| t.get("input"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = tokens
        .and_then(|t| t.get("output"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning_tokens = tokens
        .and_then(|t| t.get("reasoning"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = tokens
        .and_then(|t| t.get("cache"))
        .and_then(|c| c.get("read"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    opencode_debug(&format!("[extract_step_finish] reason={} is_final={} cost={:.6} tokens(total={} in={} out={} reasoning={} cache_read={})",
        reason, is_final, cost, total_tokens, input_tokens, output_tokens, reasoning_tokens, cache_read));

    (Some(reason.to_string()), is_final)
}

// ============================================================
// execute_command — non-streaming
// ============================================================

pub fn execute_command(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    _allowed_tools: Option<&[String]>,
    model: Option<&str>,
) -> ClaudeResponse {
    opencode_debug(&format!(
        "[execute_command] START prompt_len={} session_id={:?} working_dir={} model={:?}",
        prompt.len(),
        session_id,
        working_dir,
        model
    ));
    opencode_debug(&format!(
        "[execute_command] prompt_preview={:?}",
        log_preview(prompt, 200)
    ));

    if let Some(sid) = session_id {
        if !crate::services::process::is_valid_session_id(sid) {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!("Invalid session_id format: {}", sid)),
            };
        }
    }

    let (mut cmd, _sp_path) = build_opencode_command(session_id, working_dir, None, model);

    // When --model is specified, opencode ignores stdin → must use positional arg.
    // When no --model, stdin works and avoids shell arg size limits.
    let use_positional = model.is_some();
    if use_positional {
        opencode_debug(&format!(
            "[execute_command] using positional arg (--model set), prompt_len={}",
            prompt.len()
        ));
        cmd.arg("--");
        cmd.arg(prompt);
    }

    opencode_debug("[execute_command] spawning process...");
    let mut child = match cmd.spawn() {
        Ok(c) => {
            opencode_debug(&format!("[execute_command] spawned PID={}", c.id()));
            c
        }
        Err(e) => {
            opencode_debug(&format!("[execute_command] spawn FAILED: {}", e));
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!("Failed to start opencode: {}", e)),
            };
        }
    };

    // Write prompt to stdin (only when not using positional arg)
    if use_positional {
        drop(child.stdin.take());
        opencode_debug("[execute_command] stdin closed (positional mode)");
    } else if let Some(mut stdin) = child.stdin.take() {
        match stdin.write_all(prompt.as_bytes()) {
            Ok(()) => opencode_debug(&format!(
                "[execute_command] stdin: wrote {} bytes",
                prompt.len()
            )),
            Err(e) => opencode_debug(&format!("[execute_command] stdin write FAILED: {}", e)),
        }
        drop(stdin);
        opencode_debug("[execute_command] stdin closed");
    } else {
        opencode_debug("[execute_command] WARN: no stdin handle");
    }

    opencode_debug("[execute_command] waiting for output...");
    match child.wait_with_output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            opencode_debug(&format!(
                "[execute_command] exit={:?} stdout_len={} stderr_len={}",
                output.status.code(),
                stdout.len(),
                stderr.len()
            ));
            if !stderr.is_empty() {
                opencode_debug(&format!(
                    "[execute_command] STDERR: {}",
                    log_preview(&stderr, 500)
                ));
            }

            let mut sid: Option<String> = None;
            let mut response_text = String::new();
            let mut line_count = 0u32;
            let mut text_event_count = 0u32;
            let mut got_final_step = false;
            let mut pending_error: Option<String> = None;
            let mut last_finish_reason: Option<String> = None;
            let mut last_event_type = String::new();

            for line in stdout.trim().lines() {
                line_count += 1;
                opencode_debug(&format!(
                    "[execute_command] line {}: {}",
                    line_count,
                    log_preview(line, 300)
                ));

                if let Ok(json) = serde_json::from_str::<Value>(line) {
                    // Capture session ID from any event
                    if let Some(s) = extract_session_id(&json) {
                        opencode_debug(&format!("[execute_command] session_id extracted: {}", s));
                        sid = Some(s);
                    }

                    let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if !event_type.is_empty() {
                        last_event_type = event_type.to_string();
                    }
                    opencode_debug(&format!("[execute_command] event_type={}", event_type));

                    match event_type {
                        "text" => {
                            text_event_count += 1;
                            if let Some(text) = parse_text_event(&json) {
                                opencode_debug(&format!(
                                    "[execute_command] TEXT: {} chars, preview={:?}",
                                    text.len(),
                                    log_preview(&text, 100)
                                ));
                                response_text.push_str(&text);
                            } else {
                                opencode_debug(&format!(
                                    "[execute_command] TEXT parse FAILED: {}",
                                    log_preview(line, 300)
                                ));
                            }
                        }
                        "step_start" => {
                            opencode_debug("[execute_command] STEP_START");
                        }
                        "step_finish" => {
                            let (reason, is_final) = extract_step_finish(&json);
                            if let Some(ref r) = reason {
                                if !r.is_empty() {
                                    last_finish_reason = Some(r.clone());
                                }
                            }
                            if is_final {
                                got_final_step = true;
                            }
                            opencode_debug(&format!(
                                "[execute_command] STEP_FINISH: reason={:?} is_final={}",
                                reason, is_final
                            ));
                        }
                        "tool_use" => {
                            opencode_debug(&format!(
                                "[execute_command] TOOL_USE event (non-streaming, skipped)"
                            ));
                        }
                        "reasoning" => {
                            opencode_debug("[execute_command] REASONING event (skipped)");
                        }
                        "error" => {
                            // Don't bail out here: opencode emits recoverable errors
                            // (e.g. ContextOverflowError → auto-compaction) alongside
                            // eventual successful output. Record the most recent error
                            // and decide at the end whether to surface it.
                            let err_msg = json
                                .get("error")
                                .and_then(|v| {
                                    v.get("message")
                                        .and_then(|m| m.as_str())
                                        .or_else(|| {
                                            v.get("data")
                                                .and_then(|d| d.get("message"))
                                                .and_then(|m| m.as_str())
                                        })
                                        .or_else(|| v.get("name").and_then(|n| n.as_str()))
                                        .or_else(|| v.as_str())
                                })
                                .unwrap_or("Unknown error");
                            opencode_debug(&format!(
                                "[execute_command] ERROR event captured: {}",
                                err_msg
                            ));
                            pending_error = Some(err_msg.to_string());
                        }
                        _ => {
                            opencode_debug(&format!(
                                "[execute_command] unknown event_type={}",
                                event_type
                            ));
                        }
                    }
                } else {
                    opencode_debug(&format!(
                        "[execute_command] JSON parse failed for line {}",
                        line_count
                    ));
                }
            }

            // A captured error is transient if subsequent events yielded real output
            // or a final step — in that case the successful result wins.
            let fatal_error = pending_error
                .filter(|_| !(got_final_step || !response_text.is_empty() || text_event_count > 0));

            opencode_debug(&format!(
                "[execute_command] DONE: lines={} text_events={} response_len={} got_final={} fatal_error={:?} session_id={:?} exit={:?}",
                line_count,
                text_event_count,
                response_text.len(),
                got_final_step,
                fatal_error,
                sid,
                output.status.code()
            ));

            if let Some(err) = fatal_error {
                return ClaudeResponse {
                    success: false,
                    response: None,
                    session_id: sid,
                    error: Some(err),
                };
            }

            // Exit code 0 with no events at all — opencode failed silently on stdout
            // (typical case: stale --session id that hits NotFoundError on stderr).
            if output.status.success() && line_count == 0 {
                let err = if !stderr.trim().is_empty() {
                    stderr
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("OpenCode produced no output")
                        .to_string()
                } else {
                    "OpenCode produced no output".to_string()
                };
                opencode_debug(&format!(
                    "[execute_command] exit 0 with zero events → surfacing: {}",
                    err
                ));
                return ClaudeResponse {
                    success: false,
                    response: None,
                    session_id: sid,
                    error: Some(err),
                };
            }

            if !output.status.success() {
                return ClaudeResponse {
                    success: false,
                    response: None,
                    session_id: sid,
                    error: Some(if stderr.trim().is_empty() {
                        format!("opencode exited with code {:?}", output.status.code())
                    } else {
                        stderr
                    }),
                };
            }

            // Exit 0 reached here with at least one event. If we still have no text,
            // synthesize a diagnostic response string rather than returning a blank.
            if response_text.trim().is_empty() && text_event_count == 0 {
                let hint = match last_finish_reason.as_deref() {
                    Some("length") => "Model hit the output token limit before producing any text.",
                    Some("content-filter") => "Response was blocked by a content filter.",
                    Some("error") => "Model reported an internal error during generation.",
                    Some("tool-calls") => "Session ended while still waiting for a tool call to complete.",
                    Some("unknown") => "Model ended the step with an unknown finish reason.",
                    Some(_) => "Stream ended without a final 'stop' step.",
                    None => "Stream ended before any step_finish event arrived — the run was likely interrupted.",
                };
                let diagnostic = format!(
                    "[OpenCode] model='{}' returned empty response — {} (events={}, text_events=0, last_event={}, last_finish_reason={:?}, exit_code={:?})",
                    model.unwrap_or("default"),
                    hint,
                    line_count,
                    if last_event_type.is_empty() { "-" } else { last_event_type.as_str() },
                    last_finish_reason.as_deref().unwrap_or("-"),
                    output.status.code()
                );
                opencode_debug(&format!(
                    "[execute_command] empty response → {}",
                    diagnostic
                ));
                return ClaudeResponse {
                    success: true,
                    response: Some(diagnostic),
                    session_id: sid,
                    error: None,
                };
            }

            ClaudeResponse {
                success: true,
                response: Some(response_text.trim().to_string()),
                session_id: sid,
                error: None,
            }
        }
        Err(e) => {
            opencode_debug(&format!("[execute_command] wait_with_output FAILED: {}", e));
            ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!("Failed to read output: {}", e)),
            }
        }
    }
}

// ============================================================
// execute_command_streaming — stream JSONL events
// ============================================================

/// Public entry point. Dispatches between the new SSE-based adapter (default)
/// and the legacy `opencode run --format json` subprocess path.
///
/// Routing rules:
/// - If env `COKACDIR_OPENCODE_LEGACY=1` is set, always use the legacy path.
/// - If there is no tokio runtime available at the call site, fall back to
///   legacy (the SSE path requires an async runtime).
/// - Otherwise use the SSE path, which keeps the opencode instance alive for
///   the whole turn and correctly handles oh-my-opencode's background tasks
///   by waiting for all child sessions and todos to settle before returning.
pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    model: Option<&str>,
    no_session_persistence: bool,
) -> Result<(), String> {
    if let Some(sid) = session_id {
        if !crate::services::process::is_valid_session_id(sid) {
            return Err(format!("Invalid session_id format: {}", sid));
        }
    }
    let force_legacy = std::env::var("COKACDIR_OPENCODE_LEGACY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if force_legacy {
        opencode_debug("[dispatch] COKACDIR_OPENCODE_LEGACY=1 → legacy path");
        return execute_command_streaming_legacy(
            prompt,
            session_id,
            working_dir,
            sender,
            system_prompt,
            allowed_tools,
            cancel_token,
            model,
            no_session_persistence,
        );
    }
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            opencode_debug("[dispatch] using serve/SSE adapter");
            let prompt = prompt.to_string();
            let session_id = session_id.map(|s| s.to_string());
            let working_dir = working_dir.to_string();
            let system_prompt = system_prompt.map(|s| s.to_string());
            let model = model.map(|s| s.to_string());
            handle.block_on(async move {
                execute_command_streaming_serve(
                    &prompt,
                    session_id.as_deref(),
                    &working_dir,
                    sender,
                    system_prompt.as_deref(),
                    cancel_token,
                    model.as_deref(),
                )
                .await
            })
        }
        Err(e) => {
            opencode_debug(&format!(
                "[dispatch] no tokio runtime ({}) → legacy path",
                e
            ));
            execute_command_streaming_legacy(
                prompt,
                session_id,
                working_dir,
                sender,
                system_prompt,
                allowed_tools,
                cancel_token,
                model,
                no_session_persistence,
            )
        }
    }
}

/// Legacy subprocess-based adapter. Preserved verbatim for env-flagged
/// fallback and for the no-tokio-runtime case.
fn execute_command_streaming_legacy(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    _allowed_tools: Option<&[String]>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    model: Option<&str>,
    _no_session_persistence: bool,
) -> Result<(), String> {
    opencode_debug("=== opencode execute_command_streaming_legacy START ===");
    opencode_debug(&format!(
        "[stream] prompt_len={} session_id={:?} working_dir={} model={:?}",
        prompt.len(),
        session_id,
        working_dir,
        model
    ));
    opencode_debug(&format!(
        "[stream] system_prompt_len={} cancel_token={}",
        system_prompt.map_or(0, |s| s.len()),
        cancel_token.is_some()
    ));
    opencode_debug(&format!(
        "[stream] prompt_preview={:?}",
        log_preview(prompt, 200)
    ));

    // Inject system prompt into AGENTS.md so opencode reads it as project
    // instructions. The guard restores the original file when dropped (on
    // function return, including early returns and panics).
    let _agents_md_guard: Option<AgentsMdGuard> = match system_prompt {
        Some(sp) if !sp.is_empty() => {
            opencode_debug(&format!(
                "[stream] injecting system prompt into AGENTS.md ({} bytes)",
                sp.len()
            ));
            inject_system_prompt_into_agents_md(working_dir, sp)
        }
        _ => {
            opencode_debug("[stream] no system prompt, skipping AGENTS.md injection");
            None
        }
    };

    let (mut cmd, _sp_path) = build_opencode_command(session_id, working_dir, None, model);

    // When --model is specified, opencode ignores stdin → must use positional arg.
    // When no --model, stdin works and avoids shell arg size limits.
    let use_positional = model.is_some();
    if use_positional {
        opencode_debug(&format!(
            "[stream] using positional arg (--model set), prompt_len={}",
            prompt.len()
        ));
        cmd.arg("--");
        cmd.arg(prompt);
    }
    opencode_debug(&format!(
        "[stream] effective_prompt_len={} delivery={}",
        prompt.len(),
        if use_positional {
            "positional"
        } else {
            "stdin"
        }
    ));

    crate::services::claude::attach_cancel_cgroup(&mut cmd, cancel_token.as_ref());
    opencode_debug("[stream] spawning process...");
    let mut child = cmd.spawn().map_err(|e| {
        opencode_debug(&format!("[stream] spawn FAILED: {}", e));
        format!("Failed to start opencode: {}", e)
    })?;
    opencode_debug(&format!("[stream] spawned PID={}", child.id()));

    // Store PID for cancel. Recover from a poisoned mutex (a prior holder
    // panicked) instead of silently dropping the PID — without it stored,
    // /stop cannot signal this child.
    if let Some(ref token) = cancel_token {
        let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(child.id());
        drop(guard);
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            opencode_debug("[stream] cancelled before stdin write, killing");
            kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    // Write prompt to stdin (only when not using positional arg)
    if use_positional {
        drop(child.stdin.take());
        opencode_debug("[stream] stdin closed (positional mode)");
    } else if let Some(mut stdin) = child.stdin.take() {
        match stdin.write_all(prompt.as_bytes()) {
            Ok(()) => opencode_debug(&format!("[stream] stdin: wrote {} bytes", prompt.len())),
            Err(e) => opencode_debug(&format!("[stream] stdin write FAILED: {}", e)),
        }
        drop(stdin);
        opencode_debug("[stream] stdin closed");
    } else {
        opencode_debug("[stream] WARN: no stdin handle");
    }

    // Drain stderr in a background thread to prevent deadlock: if the child
    // writes more than the OS pipe buffer (~64KB) to stderr while we're
    // blocked reading stdout, the child's stderr write blocks and the whole
    // pipeline hangs. Mirrors the pattern in codex.rs / agy.rs.
    let stderr_thread = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default())
    });

    // Read stdout line by line
    let stdout = child.stdout.take().ok_or_else(|| {
        opencode_debug("[stream] FAILED to capture stdout");
        "Failed to capture stdout".to_string()
    })?;
    let reader = BufReader::new(stdout);
    opencode_debug("[stream] stdout reader ready, entering event loop");

    let mut last_session_id: Option<String> = None;
    let mut final_result = String::new();
    let mut got_done = false;
    let mut stdout_error: Option<(String, String)> = None;
    let mut init_sent = false;
    let mut event_count = 0u32;
    let mut text_event_count = 0u32;
    let mut tool_event_count = 0u32;
    // Diagnostics for the empty-response path: remember the last top-level event type
    // seen and the reason/output tokens from the most recent step_finish. These let us
    // produce a useful error message when the stream yields no usable text.
    let mut last_event_type = String::new();
    let mut last_finish_reason: Option<String> = None;
    let mut last_output_tokens: Option<u64> = None;

    for line in reader.lines() {
        // Check cancel
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                opencode_debug("[stream] cancelled during event loop, killing");
                kill_child_tree(&mut child);
                let _ = child.wait();
                return Ok(());
            }
        }

        let line = match line {
            Ok(l) => l,
            Err(e) => {
                opencode_debug(&format!("[stream] stdout read error: {}", e));
                let _ = sender.send(StreamMessage::Error {
                    message: format!("Failed to read output: {}", e),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
                break;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        event_count += 1;

        // Log raw event (truncated) for debugging
        opencode_debug(&format!(
            "[stream] RAW[{}]: {}",
            event_count,
            log_preview(&line, 500)
        ));

        let json: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                opencode_debug(&format!(
                    "[stream] JSON parse error on event {}: {}",
                    event_count, e
                ));
                continue;
            }
        };

        // Extract session ID from every event
        if let Some(sid) = extract_session_id(&json) {
            if last_session_id.as_deref() != Some(&sid) {
                opencode_debug(&format!(
                    "[stream] session_id updated: {:?} → {}",
                    last_session_id, sid
                ));
            }
            last_session_id = Some(sid);
        }

        let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !event_type.is_empty() {
            last_event_type = event_type.to_string();
        }

        match event_type {
            "step_start" => {
                opencode_debug(&format!(
                    "[stream] STEP_START (event {}), init_sent={}",
                    event_count, init_sent
                ));
                // Send Init on first step_start
                if !init_sent {
                    let sid = last_session_id.clone().unwrap_or_default();
                    opencode_debug(&format!("[stream] sending Init with session_id={}", sid));
                    if sender
                        .send(StreamMessage::Init { session_id: sid })
                        .is_err()
                    {
                        opencode_debug("[stream] Init send failed (receiver dropped)");
                        break;
                    }
                    init_sent = true;
                }
            }

            "text" => {
                text_event_count += 1;
                if let Some(text) = parse_text_event(&json) {
                    opencode_debug(&format!(
                        "[stream] TEXT[{}]: {} chars, preview={:?}, cumulative_result_len={}",
                        text_event_count,
                        text.len(),
                        log_preview(&text, 100),
                        final_result.len() + text.len()
                    ));
                    final_result.push_str(&text);
                    if sender.send(StreamMessage::Text { content: text }).is_err() {
                        opencode_debug("[stream] Text send failed (receiver dropped)");
                        break;
                    }
                } else {
                    opencode_debug(&format!(
                        "[stream] TEXT[{}] parse FAILED: {}",
                        text_event_count,
                        log_preview(&line, 300)
                    ));
                }
            }

            "tool_use" => {
                tool_event_count += 1;
                opencode_debug(&format!(
                    "[stream] TOOL_USE[{}] (event {})",
                    tool_event_count, event_count
                ));
                if let Some((tool_name, call_id, input, output, is_error)) =
                    parse_tool_use_event(&json)
                {
                    let state = json
                        .get("part")
                        .and_then(|p| p.get("state"))
                        .and_then(|s| s.get("status"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    opencode_debug(&format!("[stream] TOOL_USE: name={} call_id={} state={} input_len={} output_len={} is_error={}",
                        tool_name, call_id, state, input.len(), output.len(), is_error));

                    // Send ToolUse
                    if sender
                        .send(StreamMessage::ToolUse {
                            name: tool_name.clone(),
                            input: input.clone(),
                        })
                        .is_err()
                    {
                        opencode_debug("[stream] ToolUse send failed (receiver dropped)");
                        break;
                    }

                    // Send ToolResult if completed or error
                    if state == "completed" || state == "error" {
                        opencode_debug(&format!(
                            "[stream] sending ToolResult: tool={} is_error={} output_preview={:?}",
                            tool_name,
                            is_error,
                            log_preview(&output, 200)
                        ));
                        if sender
                            .send(StreamMessage::ToolResult {
                                content: output,
                                is_error,
                            })
                            .is_err()
                        {
                            opencode_debug("[stream] ToolResult send failed (receiver dropped)");
                            break;
                        }
                    }
                } else {
                    opencode_debug(&format!(
                        "[stream] TOOL_USE parse FAILED: {}",
                        log_preview(&line, 300)
                    ));
                }
            }

            "reasoning" => {
                let reasoning_text = json
                    .get("part")
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                opencode_debug(&format!(
                    "[stream] REASONING (event {}): {} chars",
                    event_count,
                    reasoning_text.len()
                ));
            }

            "step_finish" => {
                let (reason, is_final) = extract_step_finish(&json);
                if let Some(ref r) = reason {
                    if !r.is_empty() {
                        last_finish_reason = Some(r.clone());
                    }
                }
                // Capture output tokens so the empty-response diagnostic can say
                // whether the model actually generated anything.
                if let Some(out) = json
                    .get("part")
                    .and_then(|p| p.get("tokens"))
                    .and_then(|t| t.get("output"))
                    .and_then(|v| v.as_u64())
                {
                    last_output_tokens = Some(out);
                }
                opencode_debug(&format!(
                    "[stream] STEP_FINISH (event {}): reason={:?} is_final={} result_len={}",
                    event_count,
                    reason,
                    is_final,
                    final_result.len()
                ));
                if is_final {
                    got_done = true;
                    opencode_debug(&format!(
                        "[stream] sending Done: result_len={} session_id={:?}",
                        final_result.len(),
                        last_session_id
                    ));
                    let _ = sender.send(StreamMessage::Done {
                        result: final_result.clone(),
                        session_id: last_session_id.clone(),
                    });
                }
            }

            "error" => {
                let err_msg = json
                    .get("error")
                    .and_then(|v| {
                        v.get("message")
                            .and_then(|m| m.as_str())
                            .or_else(|| {
                                v.get("data")
                                    .and_then(|d| d.get("message"))
                                    .and_then(|m| m.as_str())
                            })
                            .or_else(|| v.get("name").and_then(|n| n.as_str()))
                            .or_else(|| v.as_str())
                    })
                    .unwrap_or("Unknown error")
                    .to_string();
                opencode_debug(&format!(
                    "[stream] ERROR event (event {}): {}",
                    event_count, err_msg
                ));
                stdout_error = Some((err_msg.clone(), line.clone()));
            }

            _ => {
                opencode_debug(&format!(
                    "[stream] UNKNOWN event_type={} (event {}): {}",
                    event_type,
                    event_count,
                    log_preview(&line, 200)
                ));
            }
        }
    }

    opencode_debug(&format!("[stream] event loop ended: events={} text_events={} tool_events={} got_done={} result_len={}",
        event_count, text_event_count, tool_event_count, got_done, final_result.len()));

    // Check cancel before waiting
    if let Some(ref token) = cancel_token {
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            opencode_debug("[stream] cancelled after event loop, killing");
            kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    opencode_debug("[stream] waiting for process exit...");
    let status = child.wait().map_err(|e| {
        opencode_debug(&format!("[stream] wait FAILED: {}", e));
        format!("Process error: {}", e)
    })?;

    // Collect stderr drained by the background thread.
    let stderr_msg = stderr_thread
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    if !stderr_msg.is_empty() {
        opencode_debug(&format!(
            "[stream] STDERR: {}",
            log_preview(&stderr_msg, 500)
        ));
    }
    opencode_debug(&format!(
        "[stream] exit_code={:?} success={} got_done={} result_len={} stderr_len={}",
        status.code(),
        status.success(),
        got_done,
        final_result.len(),
        stderr_msg.len()
    ));

    // Tentative stdout_error: opencode publishes a session.error event even for
    // recoverable conditions like ContextOverflowError, which then triggers
    // auto-compaction and continues the session successfully. If, by the time the
    // stream ends, we have accumulated real output or seen a final step_finish, the
    // earlier error was transient and must not poison the result.
    if stdout_error.is_some() && (got_done || !final_result.is_empty() || text_event_count > 0) {
        opencode_debug("[stream] transient stdout error ignored — subsequent output succeeded");
        stdout_error = None;
    }

    // If the process exited cleanly but produced no events at all, surface stderr
    // as the error. Typical trigger: `--session <stale>` where opencode exits 0
    // with a NotFoundError on stderr and nothing on stdout.
    if stdout_error.is_none()
        && status.success()
        && event_count == 0
        && !stderr_msg.trim().is_empty()
    {
        opencode_debug("[stream] exit 0 with zero events → surfacing stderr as error");
        let summary = stderr_msg
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("OpenCode produced no output")
            .to_string();
        stdout_error = Some((summary, String::new()));
    }

    // Handle errors
    if stdout_error.is_some() || !status.success() {
        let (message, stdout_raw) = if let Some((msg, raw)) = stdout_error {
            opencode_debug(&format!("[stream] reporting error: {}", msg));
            (msg, raw)
        } else {
            let msg = format!("Process exited with code {:?}", status.code());
            opencode_debug(&format!("[stream] reporting exit error: {}", msg));
            (msg, String::new())
        };
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: stdout_raw,
            stderr: stderr_msg,
            exit_code: status.code(),
        });
        return Ok(());
    }

    // Send Done if not already sent
    if !got_done {
        // Decide between three cases:
        //   (a) text events arrived (even empty) or final_result has content → legitimate Done
        //   (b) nothing usable arrived → synthesize a diagnostic message
        if text_event_count > 0 || !final_result.is_empty() {
            opencode_debug(&format!(
                "[stream] sending fallback Done (no final step_finish): result_len={} text_events={} session_id={:?}",
                final_result.len(), text_event_count, last_session_id
            ));
            let _ = sender.send(StreamMessage::Done {
                result: final_result,
                session_id: last_session_id,
            });
        } else {
            let model_name = model.unwrap_or("default");
            let hint = match last_finish_reason.as_deref() {
                Some("length") => "Model hit the output token limit before producing any text.",
                Some("content-filter") => "Response was blocked by a content filter.",
                Some("error") => "Model reported an internal error during generation.",
                Some("tool-calls") => "Session ended while still waiting for a tool call to complete.",
                Some("unknown") => "Model ended the step with an unknown finish reason.",
                Some(_) => "Stream ended without a final 'stop' step.",
                None => "Stream ended before any step_finish event arrived — the run was likely interrupted.",
            };
            let reason = format!(
                "[OpenCode] model='{}' returned empty response — {} (events={}, text_events={}, tool_events={}, last_event={}, last_finish_reason={:?}, output_tokens={:?}, exit_code={:?}, stderr_len={})",
                model_name,
                hint,
                event_count,
                text_event_count,
                tool_event_count,
                if last_event_type.is_empty() { "-" } else { last_event_type.as_str() },
                last_finish_reason.as_deref().unwrap_or("-"),
                last_output_tokens,
                status.code(),
                stderr_msg.len()
            );
            opencode_debug(&format!("[stream] empty response → {}", reason));
            let _ = sender.send(StreamMessage::Done {
                result: reason,
                session_id: last_session_id,
            });
        }
    }

    opencode_debug("=== opencode execute_command_streaming_legacy END ===");
    Ok(())
}

// ============================================================
// SSE / `opencode serve` adapter
// ============================================================
//
// This is the path that lets oh-my-opencode's background tasks actually
// complete: instead of running `opencode run --format json` as a one-shot
// (which kills the instance the moment the parent session first hits idle
// and therefore aborts any in-flight child sub-sessions), we spawn
// `opencode serve` ourselves, keep it alive for the whole turn, drive the
// session over HTTP + SSE, and only shut the server down when the parent
// session is idle AND all child sessions are idle AND all todos are
// finished — mirroring `oh-my-opencode run`'s pollForCompletion.

/// Guard that owns the spawned `opencode serve` child process and ensures it
/// is killed on drop. Uses `start_kill()` (sync, non-blocking) since Drop
/// cannot `.await`. A best-effort `try_wait()` follows to reap the zombie.
struct ServeChild {
    child: Option<tokio::process::Child>,
}

impl ServeChild {
    fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }
    async fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid_opt = child.id();
            // Kill the entire process group first (covers node + bun
            // grandchild). Then start_kill the direct Child handle so tokio
            // can reap it cleanly.
            kill_serve_process_group(pid_opt);
            if let Err(e) = child.start_kill() {
                opencode_debug(&format!("[serve] start_kill failed: {}", e));
            }
            // Give the process a short grace period to exit, then reap.
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
    }
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_serve_process_group(child.id());
            let _ = child.start_kill();
            // Cannot await in Drop; leave the reap to the OS. The start_kill
            // call above dispatches SIGKILL on unix so the process goes down
            // even if we cannot wait here.
        }
    }
}

/// Kill the whole serve process family led by `pid`. Unix uses a process
/// group kill; Windows uses taskkill's tree mode. Ignores errors: the worst
/// case is that `start_kill` below still kills the direct child.
#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_serve_process_group(pid_opt: Option<u32>) {
    if let Some(pid) = pid_opt {
        // Negative pid tells kill(2) to target the process group with that id.
        // Safety: libc::kill is a trivial syscall that takes only C types.
        // SIGKILL on a non-existent pgid returns ESRCH which we ignore.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
fn kill_serve_process_group(pid_opt: Option<u32>) {
    if let Some(pid) = pid_opt {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
}

#[cfg(all(not(unix), not(windows)))]
fn kill_serve_process_group(_pid_opt: Option<u32>) {
    // Other non-Unix platforms: fall back to the direct child kill only.
}

/// Async-side entry point for the SSE adapter. Orchestrates the whole turn.
async fn execute_command_streaming_serve(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    cancel_token: Option<Arc<CancelToken>>,
    model: Option<&str>,
) -> Result<(), String> {
    opencode_debug("=== opencode execute_command_streaming_serve START ===");
    opencode_debug(&format!(
        "[serve] prompt_len={} session_id={:?} working_dir={} model={:?} cancel_token={}",
        prompt.len(),
        session_id,
        working_dir,
        model,
        cancel_token.is_some()
    ));

    // ---- 1. AGENTS.md injection (same semantics as legacy path) ----
    let _agents_md_guard: Option<AgentsMdGuard> = match system_prompt {
        Some(sp) if !sp.is_empty() => {
            opencode_debug(&format!(
                "[serve] injecting system prompt into AGENTS.md ({} bytes)",
                sp.len()
            ));
            inject_system_prompt_into_agents_md(working_dir, sp)
        }
        _ => {
            opencode_debug("[serve] no system prompt, skipping AGENTS.md injection");
            None
        }
    };

    // ---- 2. Early cancel check ----
    if serve_cancel_hit(cancel_token.as_ref()) {
        opencode_debug("[serve] cancelled before spawn");
        return Ok(());
    }

    // ---- 3. Spawn opencode serve and wait for readiness ----
    let (mut serve_child, base_url) =
        match spawn_opencode_serve(working_dir, cancel_token.as_ref()).await {
            Ok(pair) => pair,
            Err(e) => {
                if serve_cancel_hit(cancel_token.as_ref()) {
                    opencode_debug(&format!("[serve] spawn aborted after cancel: {}", e));
                    return Ok(());
                }
                opencode_debug(&format!("[serve] spawn failed: {}", e));
                let _ = sender.send(StreamMessage::Error {
                    message: format!("Failed to start opencode serve: {}", e),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
                return Ok(());
            }
        };
    opencode_debug(&format!("[serve] ready at {}", base_url));

    // ---- 4. Build HTTP clients ----
    //
    // We need two separate clients because SSE is a long-lived stream:
    //   * `client` — short-lived requests (session create, prompt_async,
    //     status / children / todo polls). Has a bounded per-request timeout.
    //   * `sse_client` — the /event subscription. No per-request timeout,
    //     because the stream is intentionally held open for the whole turn
    //     and reqwest's timeout treats that as a read-timeout hit.
    let client = match reqwest::Client::builder()
        .timeout(POLL_REQUEST_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            opencode_debug(&format!("[serve] http client build failed: {}", e));
            serve_child.shutdown().await;
            let _ = sender.send(StreamMessage::Error {
                message: format!("HTTP client init failed: {}", e),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
            return Ok(());
        }
    };
    let sse_client = match reqwest::Client::builder()
        // No overall request timeout — the SSE stream is intentionally
        // long-lived — but keep a bounded connect timeout so a dead server
        // never gets us stuck waiting for a TCP/TLS handshake.
        .connect_timeout(POLL_REQUEST_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            opencode_debug(&format!("[serve] sse client build failed: {}", e));
            serve_child.shutdown().await;
            let _ = sender.send(StreamMessage::Error {
                message: format!("HTTP client init failed: {}", e),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
            return Ok(());
        }
    };

    // ---- 5. Resolve or create the parent session ----
    let parent_sid = match session_id {
        Some(sid) if !sid.is_empty() => {
            opencode_debug(&format!("[serve] reusing session_id={}", sid));
            sid.to_string()
        }
        _ => match create_session(&client, &base_url, working_dir, prompt).await {
            Ok(sid) => {
                opencode_debug(&format!("[serve] created new session_id={}", sid));
                sid
            }
            Err(e) => {
                opencode_debug(&format!("[serve] session create failed: {}", e));
                serve_child.shutdown().await;
                let _ = sender.send(StreamMessage::Error {
                    message: format!("Failed to create session: {}", e),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
                return Ok(());
            }
        },
    };

    // Announce the session id so the UI can pin it immediately.
    if sender
        .send(StreamMessage::Init {
            session_id: parent_sid.clone(),
        })
        .is_err()
    {
        opencode_debug("[serve] Init send failed (receiver dropped), tearing down");
        serve_child.shutdown().await;
        return Ok(());
    }

    // ---- 6. Open the SSE stream SYNCHRONOUSLY before firing the prompt ----
    //
    // We must have an active /event subscription *before* posting prompt_async,
    // otherwise early events (`server.connected`, the first `message.updated`,
    // initial `message.part.updated`s) can be emitted on the bus before our
    // consumer has connected and we would miss them entirely. To close that
    // race, the HTTP GET /event call happens on this (main) task; only the
    // chunk-reading loop is then handed off to a spawned task.
    // Use /global/event, not /event. The per-instance /event endpoint only
    // emits BusEvents (message.part.delta, session.status, session.idle, …)
    // and silently omits SyncEvents like message.part.updated and
    // message.updated. Without message.part.updated, the consumer below
    // cannot learn that an in-flight part has type "text" (versus
    // "reasoning"), so every delta is dropped by the part_types guard and
    // the turn ends with an empty result → "(No response)". /global/event
    // wraps each event in a {directory, project, payload} envelope and
    // forwards SyncEvents alongside BusEvents (and a redundant payload.type
    // == "sync" copy that we skip during unwrap). Verified live against
    // opencode 1.15.0: with /event the SSE stream emitted 0
    // message.part.updated frames; with /global/event the same turn emitted
    // them in the order the legacy consumer expects.
    let sse_url = format!("{}/global/event", base_url);
    opencode_debug(&format!("[serve] connecting SSE: {}", sse_url));
    let sse_resp = match sse_client.get(&sse_url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let code = r.status();
            let body = r.text().await.unwrap_or_default();
            opencode_debug(&format!(
                "[serve] SSE non-2xx {}: {}",
                code,
                log_preview(&body, 200)
            ));
            serve_child.shutdown().await;
            let _ = sender.send(StreamMessage::Error {
                message: format!(
                    "SSE subscribe failed ({}): {}",
                    code,
                    log_preview(&body, 200)
                ),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
            return Ok(());
        }
        Err(e) => {
            opencode_debug(&format!("[serve] SSE connect failed: {}", e));
            serve_child.shutdown().await;
            let _ = sender.send(StreamMessage::Error {
                message: format!("SSE connect failed: {}", e),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
            return Ok(());
        }
    };

    let final_result: Arc<tokio::sync::Mutex<String>> =
        Arc::new(tokio::sync::Mutex::new(String::new()));
    // The most recent session.error message seen by the SSE consumer. Stays
    // None if no error event arrived. Used by the main task to demote a
    // "completed with no output" outcome into a hard failure.
    let last_error: Arc<tokio::sync::Mutex<Option<String>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let sse_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let sse_handle = {
        let parent_sid = parent_sid.clone();
        let sender = sender.clone();
        let final_result = final_result.clone();
        let last_error = last_error.clone();
        let sse_stop = sse_stop.clone();
        tokio::task::spawn(async move {
            consume_sse_chunks(
                sse_resp,
                parent_sid,
                sender,
                final_result,
                last_error,
                sse_stop,
            )
            .await;
        })
    };

    // ---- 7. Fire the prompt via prompt_async ----
    if let Err(e) = post_prompt_async(
        &client,
        &base_url,
        &parent_sid,
        prompt,
        model,
        system_prompt,
    )
    .await
    {
        opencode_debug(&format!("[serve] prompt_async failed: {}", e));
        sse_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        sse_handle.abort();
        let _ = sse_handle.await;
        serve_child.shutdown().await;
        let _ = sender.send(StreamMessage::Error {
            message: format!("Prompt submission failed: {}", e),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        });
        return Ok(());
    }

    // ---- 8. Poll until everything (parent + children + todos) is idle ----
    let poll_result =
        poll_until_complete(&client, &base_url, &parent_sid, cancel_token.as_ref()).await;

    // ---- 9. Shut everything down ----
    sse_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    // Give the SSE task a brief moment to drain any trailing events, then
    // abort. The stop flag lets the loop exit on its next iteration; the
    // short sleep makes that likely before we force-abort.
    tokio::time::sleep(Duration::from_millis(500)).await;
    sse_handle.abort();
    let _ = sse_handle.await;
    serve_child.shutdown().await;

    // ---- 10. Report the final outcome ----
    let accumulated = {
        let guard = final_result.lock().await;
        guard.clone()
    };
    let captured_error = {
        let guard = last_error.lock().await;
        guard.clone()
    };
    match poll_result {
        Ok(()) => {
            // If the SSE consumer captured a session.error AND the turn
            // produced no usable output, demote the "complete-with-no-text"
            // outcome into a hard failure. This is the fast-fail case (e.g.
            // an unknown model where halt() fires before the model ever
            // emits text). When subsequent text DID materialise, we treat
            // the error as transient (matches Gap 3 in the legacy path).
            if accumulated.is_empty() && captured_error.is_some() {
                let msg = captured_error.unwrap_or_else(|| "Unknown error".into());
                opencode_debug(&format!(
                    "[serve] completed but empty result + captured error → demoting to Error: {}",
                    msg
                ));
                let _ = sender.send(StreamMessage::Error {
                    message: msg,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            } else {
                opencode_debug(&format!(
                    "[serve] completed normally, final_result_len={} captured_error={}",
                    accumulated.len(),
                    captured_error.is_some()
                ));
                let _ = sender.send(StreamMessage::Done {
                    result: accumulated,
                    session_id: Some(parent_sid),
                });
            }
        }
        Err(PollError::Cancelled) => {
            opencode_debug("[serve] poll cancelled by user");
            // No Done / Error — cancel path matches legacy behaviour of
            // returning without a terminal message so the UI shows "Cancelled".
        }
        Err(PollError::Fatal(msg)) => {
            opencode_debug(&format!("[serve] poll fatal: {}", msg));
            let _ = sender.send(StreamMessage::Error {
                message: msg,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
        }
    }

    opencode_debug("=== opencode execute_command_streaming_serve END ===");
    Ok(())
}

/// Spawn `opencode serve --port 0 --hostname 127.0.0.1` in `working_dir`,
/// wait for it to print its "listening on http://HOST:PORT" line, and return
/// the child handle along with the parsed base URL.
async fn spawn_opencode_serve(
    working_dir: &str,
    cancel_token: Option<&Arc<CancelToken>>,
) -> Result<(ServeChild, String), String> {
    use tokio::io::AsyncBufReadExt;
    use tokio::io::BufReader as TokioBufReader;

    let bin = resolve_opencode_path().unwrap_or_else(|| "opencode".to_string());
    opencode_debug(&format!(
        "[serve.spawn] bin={} working_dir={}",
        bin, working_dir
    ));

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(["serve", "--port", "0", "--hostname", "127.0.0.1"])
        .current_dir(working_dir)
        // Auto-approve every permission request. Without this, any tool call
        // touching a path outside the session directory (e.g. glob on
        // ~/.cokacdir/docs) makes opencode create a pending `external_directory`
        // permission and park the session in "busy" forever — the bot has no
        // UI to approve these in a Telegram flow.
        //
        // Value must be a JSON object: opencode merges it via `mergeDeep` into
        // the effective config (see opencode config/config.ts), so a bare
        // JSON string like `"allow"` silently no-ops. `{"*":"allow"}` becomes
        // the ruleset rule `{permission:"*", pattern:"*", action:"allow"}`
        // which matches every permission check including `external_directory`.
        //
        // `question` and `plan_exit` are the only tools that block on a user
        // reply through opencode's `question.ask` Deferred (see
        // packages/opencode/src/tool/question.ts and plan.ts). cokacdir's
        // Telegram flow has no handler that posts answers back to opencode,
        // so an AI call to either tool would hang the session forever. Deny
        // them explicitly while keeping every other tool allowed. Permission
        // evaluation uses `findLast` (see permission/evaluate.ts), so the
        // trailing `question`/`plan_exit` rules override the leading `*`.
        .env(
            "OPENCODE_PERMISSION",
            r#"{"*":"allow","question":"deny","plan_exit":"deny"}"#,
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Defence-in-depth: if this function returns early before we wrap
        // the Child in a ServeChild (e.g. stdout.take fails or the readiness
        // timeout fires), tokio's Child Drop will SIGKILL the process and
        // we never leak an orphan opencode serve.
        .kill_on_drop(true);
    // Put the child into its own process group so we can later kill the
    // *entire* group — not just the direct child — with a single SIGKILL.
    // This is critical because `opencode` is a node launcher that execs a
    // bun-compiled binary; killing only the node wrapper leaves the bun
    // child as an orphan reparented to init.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    crate::services::claude::attach_cancel_cgroup_tokio(&mut cmd, cancel_token);

    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {}", bin, e))?;
    opencode_debug(&format!("[serve.spawn] spawned PID={:?}", child.id()));

    // Register PID immediately after spawn so /stop can kill the serve
    // process even while we are still waiting for the readiness line.
    // Recover from a poisoned mutex instead of silently dropping the PID.
    if let Some(token) = cancel_token {
        if let Some(pid) = child.id() {
            let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(pid);
        }
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            opencode_debug("[serve.spawn] cancelled after PID registration");
            token.cancel_now();
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            return Err("cancelled before opencode serve became ready".to_string());
        }
    }

    // Take stdout/stderr readers. The readiness line can appear on either one
    // depending on how opencode decided to log in this build — probe both.
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture opencode serve stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture opencode serve stderr".to_string())?;

    let mut stdout_reader = TokioBufReader::new(stdout).lines();
    let mut stderr_reader = TokioBufReader::new(stderr).lines();

    // Wait up to SERVE_READY_TIMEOUT for a line containing the ready needle.
    let ready = tokio::time::timeout(SERVE_READY_TIMEOUT, async {
        loop {
            tokio::select! {
                line = stdout_reader.next_line() => {
                    match line {
                        Ok(Some(l)) => {
                            opencode_debug(&format!("[serve.stdout] {}", log_preview(&l, 200)));
                            if let Some(url) = extract_serve_url(&l) {
                                return Ok::<String, String>(url);
                            }
                        }
                        Ok(None) => return Err("opencode serve stdout closed before ready".into()),
                        Err(e) => return Err(format!("stdout read error: {}", e)),
                    }
                }
                line = stderr_reader.next_line() => {
                    match line {
                        Ok(Some(l)) => {
                            opencode_debug(&format!("[serve.stderr] {}", log_preview(&l, 200)));
                            if let Some(url) = extract_serve_url(&l) {
                                return Ok::<String, String>(url);
                            }
                        }
                        Ok(None) => return Err("opencode serve stderr closed before ready".into()),
                        Err(e) => return Err(format!("stderr read error: {}", e)),
                    }
                }
            }
        }
    })
    .await
    .map_err(|_| {
        format!(
            "opencode serve did not become ready within {}s",
            SERVE_READY_TIMEOUT.as_secs()
        )
    })??;

    // Drain remaining stdout/stderr lines in background so the pipes never
    // fill up and block the server. We just log them for post-mortem.
    tokio::task::spawn(async move {
        while let Ok(Some(l)) = stdout_reader.next_line().await {
            opencode_debug(&format!("[serve.stdout] {}", log_preview(&l, 200)));
        }
    });
    tokio::task::spawn(async move {
        while let Ok(Some(l)) = stderr_reader.next_line().await {
            opencode_debug(&format!("[serve.stderr] {}", log_preview(&l, 200)));
        }
    });

    Ok((ServeChild::new(child), ready))
}

/// Extracts the "http://host:port" URL from an opencode serve log line such as
/// "opencode server listening on http://127.0.0.1:4096".
fn extract_serve_url(line: &str) -> Option<String> {
    let idx = line.find(SERVE_READY_NEEDLE)?;
    let after = &line[idx + "listening on ".len()..];
    // Trim trailing whitespace and any stray punctuation.
    let end = after
        .find(|c: char| c.is_whitespace() || c == ',')
        .unwrap_or(after.len());
    let url = after[..end].trim_end_matches('/').to_string();
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url)
    } else {
        None
    }
}

/// Checks the cancel token (if any) for a cancellation request.
fn serve_cancel_hit(token: Option<&Arc<CancelToken>>) -> bool {
    match token {
        Some(t) => t.cancelled.load(std::sync::atomic::Ordering::Relaxed),
        None => false,
    }
}

/// Create a brand-new session on the opencode server. The title is derived
/// from the prompt's first line, mirroring opencode's own default.
async fn create_session(
    client: &reqwest::Client,
    base_url: &str,
    working_dir: &str,
    prompt: &str,
) -> Result<String, String> {
    let title = {
        let first_line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let truncated: String = first_line.chars().take(60).collect();
        if truncated.is_empty() {
            "cokacdir session".to_string()
        } else {
            truncated
        }
    };
    let body = json!({ "title": title });
    // Serialize manually because cokacdir's reqwest build does NOT enable the
    // `json` feature — `RequestBuilder::json` is therefore unavailable.
    let body_str = serde_json::to_string(&body).map_err(|e| format!("serialize: {}", e))?;
    let url = format!("{}/session?directory={}", base_url, urlencoded(working_dir));
    opencode_debug(&format!("[serve.create_session] POST {}", url));
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body_str)
        .send()
        .await
        .map_err(|e| format!("http: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("body: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "session create returned {}: {}",
            status,
            log_preview(&text, 300)
        ));
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("session create parse: {} ({})", e, log_preview(&text, 200)))?;
    v.get("id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            format!(
                "session create: no id in response: {}",
                log_preview(&text, 200)
            )
        })
}

/// Fire the user prompt as `prompt_async` so the server returns 204 immediately
/// and opencode processes the turn in the background (while we consume events
/// via SSE and wait for completion via polling).
async fn post_prompt_async(
    client: &reqwest::Client,
    base_url: &str,
    session_id: &str,
    prompt: &str,
    model: Option<&str>,
    _system_prompt_already_injected: Option<&str>,
) -> Result<(), String> {
    // Build the request body using the json! macro so we don't depend on
    // serde_json::Map::new()'s type inference.
    let mut body = if let Some(m) = model {
        let (provider_id, model_id) = match m.split_once('/') {
            Some((p, rest)) => (p, rest),
            None => ("", m),
        };
        json!({
            "parts": [{ "type": "text", "text": prompt }],
            "model": { "providerID": provider_id, "modelID": model_id },
        })
    } else {
        json!({
            "parts": [{ "type": "text", "text": prompt }],
        })
    };
    // Test-only agent override: the `--test-opencode-sse` harness in
    // main.rs can stash a plugin agent name (possibly zwsp-prefixed) via
    // COKACDIR_OPENCODE_TEST_AGENT so bg-task smoke tests can target the
    // Sisyphus family without changing the production API.
    if let Ok(test_agent) = std::env::var("COKACDIR_OPENCODE_TEST_AGENT") {
        if !test_agent.is_empty() {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("agent".to_string(), Value::String(test_agent));
            }
        }
    }

    let body_str = serde_json::to_string(&body).map_err(|e| format!("serialize: {}", e))?;
    let url = format!("{}/session/{}/prompt_async", base_url, session_id);
    opencode_debug(&format!(
        "[serve.prompt_async] POST {} prompt_len={} model={:?}",
        url,
        prompt.len(),
        model
    ));
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body_str)
        .send()
        .await
        .map_err(|e| format!("http: {}", e))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "prompt_async returned {}: {}",
            status,
            log_preview(&text, 300)
        ));
    }
    Ok(())
}

/// Consume an already-connected `GET /global/event` response as a stream of
/// SSE frames, translating each into zero or more `StreamMessage` variants
/// that belong to the parent session. Each frame's outer JSON wraps the real
/// event in a `payload` field (see the unwrap in the body); `handle_sse_event`
/// itself operates on the unwrapped event. Text parts feed both the live UI
/// stream (via `StreamMessage::Text`) and a shared accumulator used for the
/// final `Done.result`.
async fn consume_sse_chunks(
    mut resp: reqwest::Response,
    parent_sid: String,
    sender: Sender<StreamMessage>,
    final_result: Arc<tokio::sync::Mutex<String>>,
    last_error: Arc<tokio::sync::Mutex<Option<String>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    opencode_debug("[serve.sse] consumer started");

    // Simple SSE parser: events are separated by "\n\n", and within each event
    // one or more "data: <payload>" lines carry the JSON blob.
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut init_sent = false;
    // Mirror the current full text of each in-progress text part. Used for
    // two things: (a) computing the suffix to emit when `message.part.updated`
    // ships a full snapshot, and (b) deduping across the two event paths
    // (`message.part.delta` and `message.part.updated`) so the same content is
    // never sent twice. StreamMessage::Text carries deltas, not snapshots.
    let mut part_progress: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    // Track the type of each known partID so `message.part.delta` events
    // (which carry only partID/field/delta and omit the part type) can
    // filter out reasoning parts. Without this map, reasoning deltas slip
    // through the text-field check and leak into the user-facing stream.
    let mut part_types: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    // Track the role of each known messageID so we can drop text parts that
    // belong to user messages (including plugin-injected
    // `<system-reminder>` notifications).
    let mut message_roles: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            opencode_debug("[serve.sse] stop flag set, exiting loop");
            break;
        }
        let next_chunk = resp.chunk().await;
        let chunk = match next_chunk {
            Ok(Some(c)) => c,
            Ok(None) => {
                opencode_debug("[serve.sse] stream ended");
                break;
            }
            Err(e) => {
                opencode_debug(&format!("[serve.sse] chunk error: {}", e));
                break;
            }
        };
        buf.extend_from_slice(&chunk);

        // Extract complete events on "\n\n".
        loop {
            let Some(pos) = find_double_newline(&buf) else {
                break;
            };
            let raw: Vec<u8> = buf.drain(..pos + 2).collect();
            let raw_text = String::from_utf8_lossy(&raw);
            // Collect "data:" lines into a single payload.
            let mut payload = String::new();
            for line in raw_text.split('\n') {
                let line = line.trim_end_matches('\r');
                if let Some(rest) = line.strip_prefix("data:") {
                    payload.push_str(rest.trim_start());
                    payload.push('\n');
                }
            }
            let payload = payload.trim_end_matches('\n').to_string();
            if payload.is_empty() {
                continue;
            }
            let raw_json: Value = match serde_json::from_str(&payload) {
                Ok(v) => v,
                Err(e) => {
                    opencode_debug(&format!(
                        "[serve.sse] json parse err: {} line={}",
                        e,
                        log_preview(&payload, 200)
                    ));
                    continue;
                }
            };
            // /global/event wraps every event in
            //   { "directory": "...", "project": "...", "payload": {...} }
            // (server.connected / server.heartbeat omit the directory/project
            // keys but still wrap as { "payload": {...} }). When the inner
            // payload itself has `type == "sync"`, it is a versioned mirror
            // of an event that was already published unwrapped through the
            // same stream — `handle_sse_event` would see the unwrapped copy
            // moments earlier, so we skip the sync envelope here to avoid
            // double-handling.
            let json: Value = match raw_json.get("payload") {
                Some(inner) => {
                    let inner_type = inner.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if inner_type == "sync" {
                        continue;
                    }
                    inner.clone()
                }
                None => raw_json,
            };
            handle_sse_event(
                &json,
                &parent_sid,
                &sender,
                &final_result,
                &last_error,
                &mut init_sent,
                &mut part_progress,
                &mut part_types,
                &mut message_roles,
            )
            .await;
        }
    }
    opencode_debug("[serve.sse] consumer exit");
}

/// Find the byte index of the first "\n\n" (or "\r\n\r\n") boundary in `buf`.
fn find_double_newline(buf: &[u8]) -> Option<usize> {
    // Accept both LF and CRLF framings.
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some(i + 2);
        }
    }
    None
}

/// Translate one decoded SSE event into zero or more `StreamMessage`s.
///
/// Invariants enforced here:
/// - Events for sessions other than `parent_sid` are dropped entirely, so the
///   child sub-sessions spawned by background tasks never pollute the UI.
/// - Text parts whose owning message is known to be a user message are also
///   dropped: this includes the original user prompt and plugin-injected
///   `<system-reminder>` notifications (the plugin delivers those as
///   internal user messages, which is a plumbing detail we should hide).
/// - Empty text content never writes to `final_result` so the trailing
///   `Done.result` stays clean.
async fn handle_sse_event(
    json: &Value,
    parent_sid: &str,
    sender: &Sender<StreamMessage>,
    final_result: &Arc<tokio::sync::Mutex<String>>,
    last_error: &Arc<tokio::sync::Mutex<Option<String>>>,
    init_sent: &mut bool,
    part_progress: &mut std::collections::HashMap<String, String>,
    part_types: &mut std::collections::HashMap<String, String>,
    message_roles: &mut std::collections::HashMap<String, String>,
) {
    let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
    // Common top-level session id. Present on every bus event we care about.
    let props = json.get("properties");
    let event_sid = props
        .and_then(|p| p.get("sessionID"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match event_type {
        // Heartbeats and session-level bookkeeping we have no use for.
        "server.connected" | "server.heartbeat" | "session.diff" | "session.updated"
        | "session.status" | "session.created" | "tui.toast.show" => {}

        "message.updated" => {
            if event_sid != parent_sid {
                return;
            }
            // Remember this message's role so part events can be filtered.
            let info = props.and_then(|p| p.get("info"));
            let msg_id = info
                .and_then(|i| i.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let role = info
                .and_then(|i| i.get("role"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !msg_id.is_empty() && !role.is_empty() {
                message_roles.insert(msg_id.to_string(), role.to_string());
            }
            // Do NOT emit another Init here. The main task already sends
            // Init exactly once after resolving the parent session id, and
            // if that send fails the whole turn tears down before this
            // task runs. Emitting a second Init only duplicates session
            // pinning in the UI.
            let _ = init_sent;
        }

        "message.part.updated" => {
            if event_sid != parent_sid {
                return;
            }
            let props = match props {
                Some(p) => p,
                None => return,
            };
            let part = match props.get("part") {
                Some(p) => p,
                None => return,
            };
            let msg_id = part.get("messageID").and_then(|v| v.as_str()).unwrap_or("");
            // Drop parts belonging to known user messages.
            if message_roles.get(msg_id).map(String::as_str) == Some("user") {
                return;
            }
            let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let part_id = part
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !part_id.is_empty() && !part_type.is_empty() {
                part_types.insert(part_id.clone(), part_type.to_string());
            }
            match part_type {
                "text" => {
                    let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let has_end = part
                        .get("time")
                        .and_then(|t| t.get("end"))
                        .map(|v| !v.is_null())
                        .unwrap_or(false);
                    let previously = part_progress.get(&part_id).cloned().unwrap_or_default();
                    // opencode's SSE ships full snapshots of the in-progress
                    // text part, but downstream consumers (telegram.rs) treat
                    // StreamMessage::Text as append-only deltas — matching the
                    // claude/codex/gemini adapters. Emit only the newly
                    // appended suffix so snapshots don't double-accumulate.
                    if !text.is_empty() && text != previously {
                        let delta = if text.starts_with(&previously) {
                            text[previously.len()..].to_string()
                        } else {
                            text.to_string()
                        };
                        part_progress.insert(part_id.clone(), text.to_string());
                        if !delta.is_empty() {
                            let _ = sender.send(StreamMessage::Text { content: delta });
                        }
                    }
                    if has_end {
                        // Finalize this text part into the trailing-Done
                        // accumulator — but only if it carried content.
                        if !text.is_empty() {
                            let mut guard = final_result.lock().await;
                            if !guard.is_empty() {
                                guard.push_str("\n\n");
                            }
                            guard.push_str(text);
                        }
                        part_progress.remove(&part_id);
                        part_types.remove(&part_id);
                    }
                }
                "tool" => {
                    let state = part.get("state");
                    let status = state
                        .and_then(|s| s.get("status"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if status != "completed" && status != "error" {
                        return;
                    }
                    let tool_raw = part.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                    let tool_name = normalize_tool_name(tool_raw);
                    let input_json = state
                        .and_then(|s| s.get("input"))
                        .map(|v| normalize_opencode_params(tool_raw, v))
                        .unwrap_or(Value::Null);
                    let input_str = serde_json::to_string(&input_json).unwrap_or_default();
                    let output_str = state
                        .and_then(|s| s.get("output"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let is_error = status == "error";
                    let _ = sender.send(StreamMessage::ToolUse {
                        name: tool_name.clone(),
                        input: input_str,
                    });
                    let _ = sender.send(StreamMessage::ToolResult {
                        content: output_str,
                        is_error,
                    });
                }
                "step-start" | "step-finish" | "reasoning" | "patch" | "snapshot" => {
                    // Intentionally ignored: bookkeeping parts.
                }
                _ => {
                    opencode_debug(&format!(
                        "[serve.sse] unknown part.type={} id={}",
                        part_type, part_id
                    ));
                }
            }
        }

        "message.part.delta" => {
            if event_sid != parent_sid {
                return;
            }
            let props = match props {
                Some(p) => p,
                None => return,
            };
            let field = props.get("field").and_then(|v| v.as_str()).unwrap_or("");
            if field != "text" {
                return;
            }
            let msg_id = props
                .get("messageID")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Drop deltas for user-role messages.
            if message_roles.get(msg_id).map(String::as_str) == Some("user") {
                return;
            }
            let part_id = props
                .get("partID")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if part_id.is_empty() {
                return;
            }
            // Only forward deltas for "text" type parts. opencode streams
            // a part's initial `message.part.updated` (carrying part.type)
            // before any deltas, so by the time we see a delta the type is
            // already known. Verified with gpt-5.4, gpt-5.1-codex-mini,
            // and big-pickle: only "text" and "reasoning" parts emit deltas,
            // and only "text" should reach the user.
            if part_types.get(&part_id).map(String::as_str) != Some("text") {
                return;
            }
            let delta = props.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                return;
            }
            {
                let entry = part_progress.entry(part_id).or_insert_with(String::new);
                entry.push_str(delta);
            }
            let _ = sender.send(StreamMessage::Text {
                content: delta.to_string(),
            });
        }

        "session.error" => {
            if event_sid != parent_sid {
                return;
            }
            // Apply the same "tentative error" policy as the legacy adapter:
            // transient errors such as ContextOverflowError are followed by a
            // successful retry, so we only log here and let the main task
            // decide the terminal outcome based on whether subsequent text
            // arrived. We DO record the latest error message into the shared
            // `last_error` slot — the main task uses it to demote a "no
            // output" outcome into a hard failure for fast-fail cases like
            // an unknown model.
            let err_val = props.and_then(|p| p.get("error"));
            let msg = err_val
                .and_then(|v| {
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .or_else(|| {
                            v.get("data")
                                .and_then(|d| d.get("message"))
                                .and_then(|m| m.as_str())
                        })
                        .or_else(|| v.get("name").and_then(|n| n.as_str()))
                        .or_else(|| v.as_str())
                })
                .unwrap_or("Unknown error")
                .to_string();
            opencode_debug(&format!("[serve.sse] session.error (tentative): {}", msg));
            let mut guard = last_error.lock().await;
            *guard = Some(msg);
        }

        _ => {}
    }
}

#[derive(Debug)]
enum PollError {
    Cancelled,
    Fatal(String),
}

/// Wait until the parent session is fully idle: no primary work in progress,
/// no running child sessions, and no unfinished todos. Mirrors
/// `oh-my-opencode run`'s `pollForCompletion`.
async fn poll_until_complete(
    client: &reqwest::Client,
    base_url: &str,
    parent_sid: &str,
    cancel_token: Option<&Arc<CancelToken>>,
) -> Result<(), PollError> {
    let start = Instant::now();
    let mut consecutive = 0u32;
    let mut ever_busy = false;
    let mut iter = 0u32;
    // Track consecutive HTTP failures so we can fast-fail when the
    // `opencode serve` child has crashed or become unreachable.
    let mut consecutive_http_errors = 0u32;
    let mut last_http_error: Option<String> = None;
    loop {
        iter += 1;
        if serve_cancel_hit(cancel_token) {
            opencode_debug("[serve.poll] cancelled");
            return Err(PollError::Cancelled);
        }
        tokio::time::sleep(POLL_INTERVAL).await;

        // ---- parent session status ----
        let parent_kind = match get_session_status_kind(client, base_url, parent_sid).await {
            Ok(kind) => {
                consecutive_http_errors = 0;
                kind
            }
            Err(e) => {
                opencode_debug(&format!("[serve.poll iter={}] status error: {}", iter, e));
                consecutive_http_errors = consecutive_http_errors.saturating_add(1);
                last_http_error = Some(e);
                if consecutive_http_errors >= POLL_MAX_CONSECUTIVE_ERRORS {
                    let detail = last_http_error.as_deref().unwrap_or("unknown HTTP error");
                    opencode_debug(&format!(
                        "[serve.poll] POLL ABORT: {} consecutive HTTP errors on /session/status endpoint (elapsed={:.1}s, iter={}). last error: {}",
                        consecutive_http_errors, start.elapsed().as_secs_f64(), iter, detail
                    ));
                    return Err(PollError::Fatal(format!(
                        "opencode server unreachable: /session/status failed {} consecutive times ({:.1}s elapsed): {}",
                        consecutive_http_errors, start.elapsed().as_secs_f64(), detail
                    )));
                }
                consecutive = 0;
                continue;
            }
        };
        let parent_idle = if parent_kind == "busy" || parent_kind == "retry" {
            ever_busy = true;
            false
        } else if parent_kind == "idle" {
            true
        } else {
            // Empty or unknown kind → assume idle after we've seen at least
            // one busy cycle, OR after the stabilization window has elapsed
            // even if we never observed busy (covers fast-fail paths like
            // an unknown model where the session never transitions busy).
            ever_busy || start.elapsed() >= POLL_MIN_STABILIZATION
        };
        opencode_debug(&format!(
            "[serve.poll iter={}] kind={:?} parent_idle={} ever_busy={} consecutive={}",
            iter, parent_kind, parent_idle, ever_busy, consecutive
        ));
        if !parent_idle {
            consecutive = 0;
            continue;
        }

        // ---- active children ----
        let children_busy = match get_children_busy(client, base_url, parent_sid).await {
            Ok(b) => {
                consecutive_http_errors = 0;
                b
            }
            Err(e) => {
                opencode_debug(&format!("[serve.poll] children error: {}", e));
                consecutive_http_errors = consecutive_http_errors.saturating_add(1);
                last_http_error = Some(e);
                if consecutive_http_errors >= POLL_MAX_CONSECUTIVE_ERRORS {
                    let detail = last_http_error.as_deref().unwrap_or("unknown HTTP error");
                    opencode_debug(&format!(
                        "[serve.poll] POLL ABORT: {} consecutive HTTP errors on /session/{{}}/children endpoint (elapsed={:.1}s, iter={}). last error: {}",
                        consecutive_http_errors, start.elapsed().as_secs_f64(), iter, detail
                    ));
                    return Err(PollError::Fatal(format!(
                        "opencode server unreachable: /session/{{}}/children failed {} consecutive times ({:.1}s elapsed): {}",
                        consecutive_http_errors, start.elapsed().as_secs_f64(), detail
                    )));
                }
                consecutive = 0;
                continue;
            }
        };
        if children_busy {
            consecutive = 0;
            continue;
        }

        // ---- unfinished todos ----
        let todos_pending = match get_todos_pending(client, base_url, parent_sid).await {
            Ok(p) => {
                consecutive_http_errors = 0;
                p
            }
            Err(e) => {
                opencode_debug(&format!("[serve.poll] todo error: {}", e));
                consecutive_http_errors = consecutive_http_errors.saturating_add(1);
                last_http_error = Some(e);
                if consecutive_http_errors >= POLL_MAX_CONSECUTIVE_ERRORS {
                    let detail = last_http_error.as_deref().unwrap_or("unknown HTTP error");
                    opencode_debug(&format!(
                        "[serve.poll] POLL ABORT: {} consecutive HTTP errors on /session/{{}}/todo endpoint (elapsed={:.1}s, iter={}). last error: {}",
                        consecutive_http_errors, start.elapsed().as_secs_f64(), iter, detail
                    ));
                    return Err(PollError::Fatal(format!(
                        "opencode server unreachable: /session/{{}}/todo failed {} consecutive times ({:.1}s elapsed): {}",
                        consecutive_http_errors, start.elapsed().as_secs_f64(), detail
                    )));
                }
                consecutive = 0;
                continue;
            }
        };
        if todos_pending {
            consecutive = 0;
            continue;
        }

        // All three conditions satisfied.
        if !ever_busy && start.elapsed() < POLL_MIN_STABILIZATION {
            // Not enough stabilization time yet — the server may not have
            // started processing the prompt at all.
            continue;
        }
        consecutive = consecutive.saturating_add(1);
        opencode_debug(&format!(
            "[serve.poll] all idle (consecutive={}/{})",
            consecutive, POLL_REQUIRED_CONSECUTIVE
        ));
        if consecutive >= POLL_REQUIRED_CONSECUTIVE {
            return Ok(());
        }
    }
}

async fn get_session_status_kind(
    client: &reqwest::Client,
    base_url: &str,
    session_id: &str,
) -> Result<String, String> {
    let url = format!("{}/session/status", base_url);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("http: {}", e))?;
    let text = resp.text().await.map_err(|e| format!("body: {}", e))?;
    let map: Value = serde_json::from_str(&text)
        .map_err(|e| format!("status parse: {} ({})", e, log_preview(&text, 200)))?;
    let kind = map
        .get(session_id)
        .and_then(|v| v.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(kind)
}

async fn get_children_busy(
    client: &reqwest::Client,
    base_url: &str,
    session_id: &str,
) -> Result<bool, String> {
    let url = format!("{}/session/{}/children", base_url, session_id);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("http: {}", e))?;
    let text = resp.text().await.map_err(|e| format!("body: {}", e))?;
    let arr: Value = serde_json::from_str(&text)
        .map_err(|e| format!("children parse: {} ({})", e, log_preview(&text, 200)))?;
    let list = match arr.as_array() {
        Some(l) => l,
        None => return Ok(false),
    };
    if list.is_empty() {
        return Ok(false);
    }
    // For each child, check its status. A child without an entry in
    // /session/status is assumed idle (it has already finalized).
    let statuses = {
        let url = format!("{}/session/status", base_url);
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("http: {}", e))?;
        let text = resp.text().await.map_err(|e| format!("body: {}", e))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("status parse: {} ({})", e, log_preview(&text, 200)))?;
        v
    };
    for child in list {
        let cid = match child.get("id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let kind = statuses
            .get(cid)
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("idle");
        if kind == "busy" || kind == "retry" {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn get_todos_pending(
    client: &reqwest::Client,
    base_url: &str,
    session_id: &str,
) -> Result<bool, String> {
    let url = format!("{}/session/{}/todo", base_url, session_id);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("http: {}", e))?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(false);
    }
    let text = resp.text().await.map_err(|e| format!("body: {}", e))?;
    if !status.is_success() {
        // Treat unexpected errors as "nothing pending" rather than blocking
        // forever — the caller's consecutive-check will still protect us.
        opencode_debug(&format!(
            "[serve.poll] todo http {}: {}",
            status,
            log_preview(&text, 200)
        ));
        return Ok(false);
    }
    let arr: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    let list = match arr.as_array() {
        Some(l) => l,
        None => return Ok(false),
    };
    for t in list {
        let st = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if st != "completed" && st != "cancelled" {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Minimal URL-encoder for path segments / query values. Covers the subset we
/// actually pass (working directory paths) without pulling in an extra crate.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
