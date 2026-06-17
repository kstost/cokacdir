use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use crate::services::claude::{debug_log_to, kill_child_tree, CancelToken, StreamMessage};

/// Context required to auto-deliver images that Codex's built-in `image_gen`
/// tool drops into `~/.codex/generated_images/<session_id>/` without surfacing
/// any JSON event in `codex exec --json`. Without this fallback the bot has no
/// way to know an image was produced and the user sees nothing.
pub struct CodexAutoSendCtx {
    pub cokacdir_bin: String,
    pub chat_id: i64,
    pub bot_key: String,
}

/// Short (12-hex-char) SHA-256 of the input, used for /loop verification
/// forensic logs so consecutive-iteration outputs can be compared at a
/// glance. Not a security hash.
fn short_sha(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let r = h.finalize();
    hex::encode(&r[..6])
}

/// Cached path to the codex binary.
static CODEX_PATH: OnceLock<Option<String>> = OnceLock::new();

/// Resolve the path to the codex binary.
/// First tries `which codex`, then falls back to `bash -lc "which codex"`.
#[cfg(unix)]
fn resolve_codex_path() -> Option<String> {
    if let Ok(val) = std::env::var("COKAC_CODEX_PATH") {
        if !val.is_empty() && codex_path_is_runnable(&val) {
            return Some(val);
        }
    }

    if let Ok(output) = Command::new("which").arg("codex").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && codex_path_is_runnable(&path) {
                return Some(path);
            }
        }
    }

    if let Ok(output) = Command::new("bash").args(["-lc", "which codex"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && codex_path_is_runnable(&path) {
                return Some(path);
            }
        }
    }

    None
}

#[cfg(windows)]
fn resolve_codex_path() -> Option<String> {
    if let Ok(val) = std::env::var("COKAC_CODEX_PATH") {
        if !val.is_empty() && codex_path_is_runnable(&val) {
            return Some(val);
        }
    }

    // Use SearchPathW (UTF-16 native) — no code page issues with non-ASCII paths
    // Prefer .cmd (npm batch wrapper) over bare file (Unix shell script)
    if let Some(path) = crate::services::claude::search_path_wide("codex", Some(".cmd")) {
        return Some(path);
    }
    if let Some(path) = crate::services::claude::search_path_wide("codex", Some(".exe")) {
        return Some(path);
    }
    None
}

fn codex_path_is_runnable(path: &str) -> bool {
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

/// Get the cached codex binary path, resolving it on first call.
fn get_codex_path() -> Option<&'static str> {
    CODEX_PATH.get_or_init(|| resolve_codex_path()).as_deref()
}

/// Check if Codex CLI is available
pub fn is_codex_available() -> bool {
    get_codex_path().is_some()
}

/// Check if a model string refers to the Codex backend
pub fn is_codex_model(model: Option<&str>) -> bool {
    model
        .map(|m| m == "codex" || m.starts_with("codex:"))
        .unwrap_or(false)
}

/// Strip "codex:" prefix and return the actual model name.
/// Returns None if the input is just "codex" (use CLI default).
/// Also strips display-name suffix (" — Description") if present.
pub fn strip_codex_prefix(model: &str) -> Option<&str> {
    model
        .strip_prefix("codex:")
        .filter(|s| !s.is_empty())
        .map(|s| s.split(" \u{2014} ").next().unwrap_or(s).trim())
}

fn codex_debug_log(msg: &str) {
    debug_log_to("codex.log", msg);
}

/// Verify whether a Codex session's task has been fully completed.
///
/// Mirrors the high-level contract of `claude::verify_completion`, but the
/// mechanics differ because Codex has no non-interactive `--fork-session`:
/// instead of forking the live session, we read the full-fidelity archive
/// produced by `session_archive` (which is kept up to date by the normal
/// convert-and-save flow), synthesize a transcript, and dispatch a fresh
/// `codex exec --ephemeral` call that is completely independent of the
/// original session — no `resume`, no `thread_id` passed in, no rollout
/// file written. This guarantees the user-facing Codex session is not
/// modified by the verification call.
///
/// Contract:
/// - Returns `complete=true` iff the response contains `mission_complete`
///   and not `mission_pending`.
/// - `feedback` carries the "what remains" text with keywords stripped,
///   ready to be re-injected as the next user prompt.
pub fn verify_completion_codex(
    session_id: &str,
    working_dir: &str,
    reasoning_effort: Option<&str>,
    fast_mode: bool,
) -> Result<crate::services::claude::VerifyResult, String> {
    codex_debug_log("=== verify_completion_codex START ===");
    codex_debug_log(&format!("  session_id: {}", session_id));
    codex_debug_log(&format!("  working_dir: {}", working_dir));

    // 1. Load the archive (full fidelity, deduplicated, provider-agnostic).
    let transcript = crate::services::session_archive::build_verification_transcript(session_id)?;
    codex_debug_log(&format!("  transcript: {} chars", transcript.len()));
    // Forensic log: pair this sha with the transcript sha in session_archive.log
    // — if they differ between consecutive iterations the input is fresh.
    codex_debug_log(&format!(
        "[loop-verify input] sid={} transcript_len={} transcript_sha={}",
        session_id,
        transcript.len(),
        short_sha(&transcript)
    ));

    let codex_bin = get_codex_path().ok_or_else(|| {
        codex_debug_log("  ERROR: Codex CLI not found");
        "Codex CLI not found".to_string()
    })?;

    // 2. Build verification prompt. Read-only sandbox + "no tools" directive
    //    is the best Codex offers in place of Claude's `--tools ""`.
    let verify_prompt = format!(
        "Review the task transcript below. \
         Do NOT call any tools, do NOT read files, do NOT run commands — \
         judge purely from the transcript.\n\n\
         If the task appears fully and safely complete, respond with ONLY the single word: mission_complete\n\n\
         Otherwise respond with: mission_pending\n\
         followed by ONE short follow-up instruction (1–2 sentences).\n\n\
         CRITICAL — what this follow-up instruction IS:\n\
         The text you write after `mission_pending` will be taken verbatim and \
         delivered as the NEXT USER MESSAGE to the very same working agent that \
         produced the transcript. That agent will read it as if the user typed \
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
         - Match the language of the transcript.\n\
         - 1–2 sentences. No preface, no \"I think\", no meta commentary.\n\n\
         === TRANSCRIPT ===\n{}\n=== END TRANSCRIPT ===",
        transcript);

    // 3. Spawn a fresh, ephemeral Codex session. No `resume`, no session_id,
    //    no thread_id. --ephemeral prevents a rollout file from being created.
    //    The original user session is untouched.
    //
    // We route the final agent message to a tempfile via --output-last-message.
    // This is crucial: Codex's non-JSON stdout echoes the user prompt under a
    // "User instructions:" block. Our verify prompt itself contains the tokens
    // `mission_complete` and `mission_pending` as instructions; parsing stdout
    // directly would therefore ALWAYS find `mission_pending` (false positive)
    // and keep the loop running to its iteration cap. Reading the last-message
    // file instead yields only the model's actual reply.
    let out_path = std::env::temp_dir().join(format!(
        "cokac_verify_codex_{}_{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // Best-effort pre-clean; ignore failure.
    let _ = std::fs::remove_file(&out_path);

    // Note: `codex exec` does not accept `--ask-for-approval`; exec is
    // inherently non-interactive so there's nothing to prompt for. The
    // read-only sandbox prevents any filesystem writes the model might try,
    // and the prompt itself instructs "do not call any tools".
    let out_path_str = out_path.to_string_lossy().to_string();
    let mut args: Vec<String> = vec![
        "exec".to_string(),
        "--ephemeral".to_string(),
        "--skip-git-repo-check".to_string(),
        "--sandbox".to_string(),
        "read-only".to_string(),
        "--output-last-message".to_string(),
        out_path_str,
    ];
    if let Some(effort) = reasoning_effort {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort={}", effort));
    }
    if fast_mode {
        args.push("-c".to_string());
        args.push("service_tier=\"fast\"".to_string());
    }
    args.push("-".to_string());
    codex_debug_log(&format!("  args: {:?}", args));

    let spawn_start = std::time::Instant::now();
    let mut child = Command::new(codex_bin)
        .args(&args)
        .current_dir(working_dir)
        .env(
            "PATH",
            crate::services::claude::enhanced_path_for_bin(codex_bin),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            codex_debug_log(&format!("  ERROR: Failed to spawn: {}", e));
            format!("Failed to start Codex for verify_completion: {}", e)
        })?;
    codex_debug_log(&format!(
        "  spawned in {:?}, pid={:?}",
        spawn_start.elapsed(),
        child.id()
    ));

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(verify_prompt.as_bytes());
        drop(stdin);
    }

    let wait_start = std::time::Instant::now();
    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to read verify output: {}", e))?;
    codex_debug_log(&format!(
        "  completed in {:?}, exit={:?}",
        wait_start.elapsed(),
        output.status.code()
    ));

    // Read and clean up the last-message file regardless of exit status so we
    // don't leak tempfiles even on failure paths.
    let last_message = std::fs::read_to_string(&out_path).ok();
    let _ = std::fs::remove_file(&out_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(format!(
            "verify_completion_codex process failed (exit {:?}). stderr: {}",
            output.status.code(),
            crate::services::claude::safe_preview(&stderr, 500)
        ));
    }

    // 4. Extract the model's final response. --output-last-message gives us
    //    exactly the agent's final message text, with no prompt echo or
    //    session banner boilerplate.
    let response_text = match last_message {
        Some(s) if !s.trim().is_empty() => s,
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(format!(
                "verify_completion_codex produced no last-message output. stderr: {}",
                crate::services::claude::safe_preview(&stderr, 500)
            ));
        }
    };
    codex_debug_log(&format!(
        "  last_message len={}, preview: {}",
        response_text.len(),
        response_text.chars().take(300).collect::<String>()
    ));

    // Same decision rule as claude::verify_completion: treat as complete only
    // when `mission_complete` is present AND `mission_pending` is absent.
    let pending = response_text.contains("mission_pending");
    let complete = response_text.contains("mission_complete") && !pending;
    let feedback = if complete {
        None
    } else {
        let cleaned = response_text
            .replace("mission_pending", "")
            .replace("mission_complete", "");
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.to_string())
        }
    };

    codex_debug_log(&format!(
        "  complete={}, feedback={:?}",
        complete,
        feedback
            .as_ref()
            .map(|s| crate::services::claude::safe_preview(s, 200))
    ));
    // Forensic log: the full verifier reply plus its sha. Compare consecutive
    // iterations — identical `output_sha` across iterations with a *changing*
    // `transcript_sha` proves the verifier's output is converging to the same
    // text despite fresh input (the structural /loop "near-identical prompt"
    // pathology). Identical shas for BOTH would mean the archive is stale.
    codex_debug_log(&format!(
        "[loop-verify output] sid={} complete={} pending={} output_len={} output_sha={} feedback_sha={} output_full={:?}",
        session_id, complete, pending, response_text.len(),
        short_sha(&response_text),
        feedback.as_deref().map(short_sha).unwrap_or_default(),
        response_text));
    codex_debug_log("=== verify_completion_codex END ===");

    Ok(crate::services::claude::VerifyResult { complete, feedback })
}

/// Execute a command using Codex CLI with streaming output.
///
/// Parameters mirror `claude::execute_command_streaming` for consistency,
/// but some are ignored (allowed_tools, no_session_persistence)
/// because Codex exec has no tool restriction support.
///
/// When `session_id` is Some, uses `codex exec resume` to continue an existing
/// session (Codex manages conversation history natively). When None, starts a
/// new session with `codex exec`.
pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    _allowed_tools: Option<&[String]>, // ignored — Codex has no tool restriction
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    model: Option<&str>,                  // "codex:" prefix already stripped
    _no_session_persistence: bool,        // ignored — Codex exec handles persistence internally
    auto_send: Option<&CodexAutoSendCtx>, // when Some, deliver image_gen outputs that the model forgot to sendfile
    reasoning_effort: Option<&str>,       // None = use Codex CLI default (~/.codex/config.toml)
    fast_mode: bool,                      // true = pass `-c service_tier="fast"`
) -> Result<(), String> {
    codex_debug_log("========================================");
    codex_debug_log("=== codex execute_command_streaming START ===");
    codex_debug_log("========================================");
    codex_debug_log(&format!("prompt_len: {} chars", prompt.len()));
    codex_debug_log(&format!("session_id: {:?}", session_id));
    codex_debug_log(&format!("working_dir: {}", working_dir));
    codex_debug_log(&format!("model: {:?}", model));
    codex_debug_log(&format!("fast_mode: {}", fast_mode));
    let is_resume = session_id.is_some();
    codex_debug_log(&format!("is_resume: {}", is_resume));
    codex_debug_log(&format!(
        "system_prompt: is_some={}, len={}",
        system_prompt.is_some(),
        system_prompt.map(|s| s.len()).unwrap_or(0)
    ));

    // Write system prompt to file and use -c model_instructions_file to pass it,
    // mirroring Claude's --append-system-prompt-file pattern.
    // This works for both new sessions and resume (instruction changes take effect immediately).
    struct SpFileGuard(Option<std::path::PathBuf>);
    impl Drop for SpFileGuard {
        fn drop(&mut self) {
            if let Some(ref p) = self.0 {
                match std::fs::remove_file(p) {
                    Ok(()) => { /* SpFileGuard: cleaned up temp file */ }
                    Err(e) => {
                        // Cannot call codex_debug_log from Drop (no access),
                        // but the file is in ~/.cokacdir so it won't leak silently.
                        eprintln!("[codex] WARN: SpFileGuard failed to remove {:?}: {}", p, e);
                    }
                }
            }
        }
    }
    let mut _sp_guard = SpFileGuard(None);

    // Build CLI arguments
    let mut args = if let Some(sid) = session_id {
        // Validate before placing the value adjacent to other CLI flags;
        // an unvalidated sid like "--config /etc/passwd" would be treated
        // as a new flag by Codex's argument parser.
        if !crate::services::process::is_valid_session_id(sid) {
            return Err(format!("Invalid session_id format: {}", sid));
        }
        codex_debug_log(&format!("Building RESUME args, session_id={}", sid));
        // codex exec resume --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check <session_id> -
        vec![
            "exec".to_string(),
            "resume".to_string(),
            "--json".to_string(),
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "--skip-git-repo-check".to_string(),
            sid.to_string(),
        ]
    } else {
        codex_debug_log(&format!(
            "Building NEW SESSION args, working_dir={}",
            working_dir
        ));
        // codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <dir> -
        vec![
            "exec".to_string(),
            "--json".to_string(),
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "--skip-git-repo-check".to_string(),
            "-C".to_string(),
            working_dir.to_string(),
        ]
    };

    // Write system prompt to temp file and pass via -c model_instructions_file
    if let Some(sp) = system_prompt {
        if !sp.is_empty() {
            codex_debug_log(&format!(
                "[SP-FILE] is_resume={}, system_prompt_len={} — writing temp file",
                is_resume,
                sp.len()
            ));
            let sp_dir = dirs::home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join(".cokacdir");
            codex_debug_log(&format!("[SP-FILE] sp_dir={:?}", sp_dir));
            match std::fs::create_dir_all(&sp_dir) {
                Ok(()) => codex_debug_log("[SP-FILE] create_dir_all OK"),
                Err(e) => codex_debug_log(&format!(
                    "[SP-FILE] WARN: create_dir_all failed: {} (write may also fail)",
                    e
                )),
            }
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let sp_path = sp_dir.join(format!("codex_sp_{:x}_{}", nanos, std::process::id()));
            codex_debug_log(&format!("[SP-FILE] sp_path={:?}", sp_path));
            if let Err(e) = std::fs::write(&sp_path, sp) {
                codex_debug_log(&format!(
                    "[SP-FILE] ERROR: Failed to write system prompt file: {}",
                    e
                ));
                return Err(format!("Failed to write system prompt file: {}", e));
            }
            // Verify the file was written correctly
            match std::fs::metadata(&sp_path) {
                Ok(meta) => codex_debug_log(&format!(
                    "[SP-FILE] Written OK: file_size={}, sp_len={}, match={}",
                    meta.len(),
                    sp.len(),
                    meta.len() as usize == sp.len()
                )),
                Err(e) => codex_debug_log(&format!(
                    "[SP-FILE] WARN: metadata check failed after write: {}",
                    e
                )),
            }
            let arg_value = format!("model_instructions_file={}", sp_path.to_string_lossy());
            codex_debug_log(&format!("[SP-FILE] Adding args: -c {}", arg_value));
            args.push("-c".to_string());
            args.push(arg_value);
            _sp_guard = SpFileGuard(Some(sp_path));
        } else {
            codex_debug_log("[SP-FILE] system_prompt is Some but EMPTY — skipping file creation");
        }
    } else {
        codex_debug_log("[SP-FILE] system_prompt is None — no file created");
    }

    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }

    // Reasoning effort override (minimal/low/medium/high/xhigh).
    // Caller must already have validated the value; codex parses `-c key=value`
    // as JSON when possible, otherwise as a literal string — these enum
    // values are safe bare identifiers.
    if let Some(effort) = reasoning_effort {
        let arg_value = format!("model_reasoning_effort={}", effort);
        codex_debug_log(&format!("[EFFORT] Adding args: -c {}", arg_value));
        args.push("-c".to_string());
        args.push(arg_value);
    }

    if fast_mode {
        codex_debug_log("[FAST] Adding args: -c service_tier=\"fast\"");
        args.push("-c".to_string());
        args.push("service_tier=\"fast\"".to_string());
    }

    // `-` means read prompt from stdin
    args.push("-".to_string());

    let codex_bin = get_codex_path().ok_or_else(|| {
        codex_debug_log("ERROR: Codex CLI not found");
        "Codex CLI not found. Is Codex CLI installed?".to_string()
    })?;

    codex_debug_log("--- Spawning codex process ---");
    codex_debug_log(&format!("Command: {} {:?}", codex_bin, args));

    // Snapshot the generated-images dir for resumed sessions BEFORE spawn so
    // we can later isolate files created during this turn. New sessions start
    // with no thread_id-keyed dir, so an empty snapshot is correct.
    let image_dir_snapshot: HashSet<PathBuf> = if auto_send.is_some() {
        snapshot_image_dir(session_id)
    } else {
        HashSet::new()
    };
    let turn_started_at = std::time::SystemTime::now();

    let spawn_start = std::time::Instant::now();
    let mut cmd = Command::new(codex_bin);
    cmd.args(&args)
        .current_dir(working_dir)
        .env(
            "PATH",
            crate::services::claude::enhanced_path_for_bin(codex_bin),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::services::claude::detach_into_own_pgroup(&mut cmd);
    crate::services::claude::attach_cancel_cgroup(&mut cmd, cancel_token.as_ref());
    let mut child = cmd.spawn().map_err(|e| {
        codex_debug_log(&format!("ERROR: Failed to spawn: {}", e));
        format!("Failed to start Codex: {}. Is Codex CLI installed?", e)
    })?;
    codex_debug_log(&format!(
        "Codex process spawned in {:?}, pid={:?}",
        spawn_start.elapsed(),
        child.id()
    ));

    // Store child PID in cancel token. Recover from a poisoned mutex
    // (a prior holder panicked) instead of silently dropping the PID —
    // without the PID stored, /stop cannot signal this child.
    if let Some(ref token) = cancel_token {
        let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(child.id());
        drop(guard);
        // If /stop arrived before PID was stored, kill immediately
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    // Write prompt to stdin (system prompt is now passed via -c model_instructions_file)
    if let Some(mut stdin) = child.stdin.take() {
        codex_debug_log(&format!(
            "[STDIN] Writing prompt to stdin ({} bytes), is_resume={}",
            prompt.len(),
            is_resume
        ));
        codex_debug_log(&format!(
            "[STDIN] prompt_first_200={:?}",
            prompt.chars().take(200).collect::<String>()
        ));
        match stdin.write_all(prompt.as_bytes()) {
            Ok(()) => codex_debug_log("[STDIN] write_all OK"),
            Err(e) => codex_debug_log(&format!("[STDIN] ERROR: write_all failed: {}", e)),
        }
        codex_debug_log("[STDIN] dropping stdin (closing pipe)");
    }

    // Drain stderr in a background thread to prevent deadlock
    // (if child writes >64KB to stderr while we block reading stdout, both sides block)
    let stderr_thread = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default())
    });

    // Read stdout line by line (JSONL)
    let stdout = child.stdout.take().ok_or_else(|| {
        codex_debug_log("ERROR: Failed to capture stdout");
        "Failed to capture stdout".to_string()
    })?;
    let reader = BufReader::new(stdout);

    let mut last_session_id: Option<String> = None;
    let mut got_done = false;
    let mut stdout_error: Option<(String, String)> = None;
    let mut line_count = 0;
    // Track paths the model itself delivered via cokacdir --sendfile so the
    // post-turn auto-deliver pass doesn't double-send the same file.
    let mut model_sent_paths: Vec<PathBuf> = Vec::new();

    codex_debug_log("Entering JSONL lines loop...");
    'lines: for line in reader.lines() {
        // Check cancel token
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                codex_debug_log("Cancel detected — killing child process");
                kill_child_tree(&mut child);
                let _ = child.wait();
                return Ok(());
            }
        }

        let line = match line {
            Ok(l) => l,
            Err(e) => {
                codex_debug_log(&format!("ERROR: Failed to read line: {}", e));
                let _ = sender.send(StreamMessage::Error {
                    message: format!("Failed to read output: {}", e),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
                break;
            }
        };

        line_count += 1;

        if line.trim().is_empty() {
            continue;
        }

        let line_preview: String = line.chars().take(200).collect();
        codex_debug_log(&format!("Line {}: {}", line_count, line_preview));

        if let Ok(json) = serde_json::from_str::<Value>(&line) {
            let messages = parse_codex_event(&json);
            codex_debug_log(&format!("  Parsed {} messages", messages.len()));

            for msg in messages {
                match &msg {
                    StreamMessage::Init { session_id } => {
                        codex_debug_log(&format!("  >>> Init: session_id={}", session_id));
                        last_session_id = Some(session_id.clone());
                    }
                    StreamMessage::Done { .. } => {
                        codex_debug_log("  >>> Done");
                        // Inject auto-delivered images BEFORE forwarding Done so
                        // the polling loop processes them as part of this turn.
                        if let Some(ctx) = auto_send {
                            if let Some(sid) = last_session_id.as_deref() {
                                auto_deliver_new_images(
                                    sid,
                                    &image_dir_snapshot,
                                    &model_sent_paths,
                                    turn_started_at,
                                    ctx,
                                    &sender,
                                );
                            } else {
                                codex_debug_log("[auto-send] skipped: no session_id captured");
                            }
                        }
                        got_done = true;
                    }
                    StreamMessage::Error { ref message, .. } => {
                        codex_debug_log(&format!("  >>> Error: {}", message));
                        stdout_error = Some((message.clone(), line.clone()));
                        continue;
                    }
                    StreamMessage::Text { content } => {
                        let preview: String = content.chars().take(100).collect();
                        codex_debug_log(&format!(
                            "  >>> Text: {} chars, preview: {:?}",
                            content.len(),
                            preview
                        ));
                    }
                    StreamMessage::ToolUse { name, input } => {
                        let input_preview: String = input.chars().take(200).collect();
                        codex_debug_log(&format!(
                            "  >>> ToolUse: name={}, input={:?}",
                            name, input_preview
                        ));
                        if let Some(p) = extract_sendfile_path(name, input) {
                            codex_debug_log(&format!(
                                "  >>> ToolUse: model --sendfile path recorded: {}",
                                p.display()
                            ));
                            model_sent_paths.push(p);
                        }
                    }
                    StreamMessage::ToolResult { content, is_error } => {
                        codex_debug_log(&format!(
                            "  >>> ToolResult: is_error={}, len={}",
                            is_error,
                            content.len()
                        ));
                    }
                    StreamMessage::TaskNotification { .. } => {}
                }

                if sender.send(msg).is_err() {
                    codex_debug_log("  ERROR: Channel send failed (receiver dropped)");
                    break 'lines;
                }
            }
        } else {
            codex_debug_log(&format!("  NOT valid JSON: {}", line_preview));
        }
    }

    codex_debug_log(&format!(
        "--- Exited lines loop, {} lines read ---",
        line_count
    ));

    // Check cancel after loop
    if let Some(ref token) = cancel_token {
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            codex_debug_log("Cancel detected after loop — killing child process");
            kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    // Wait for process to finish
    let status = child.wait().map_err(|e| {
        codex_debug_log(&format!("ERROR: Process wait failed: {}", e));
        format!("Process error: {}", e)
    })?;
    codex_debug_log(&format!(
        "Process finished, exit_code: {:?}, is_resume={}, sp_file_used={}",
        status.code(),
        is_resume,
        _sp_guard.0.is_some()
    ));

    // Handle errors
    if stdout_error.is_some() || !status.success() {
        let stderr_msg = stderr_thread
            .and_then(|h| h.join().ok())
            .unwrap_or_default();

        // Log stderr for diagnosing -c flag issues (e.g., unrecognized option on resume)
        if !stderr_msg.is_empty() {
            codex_debug_log(&format!(
                "[STDERR] is_resume={}, exit_code={:?}, stderr_len={}",
                is_resume,
                status.code(),
                stderr_msg.len()
            ));
            codex_debug_log(&format!(
                "[STDERR] content_first_500={:?}",
                stderr_msg.chars().take(500).collect::<String>()
            ));
            // Check for common -c/config rejection patterns
            if stderr_msg.contains("unknown option")
                || stderr_msg.contains("unrecognized")
                || stderr_msg.contains("model_instructions_file")
            {
                codex_debug_log(&format!("[STDERR] *** LIKELY -c FLAG REJECTION: Codex may not support -c model_instructions_file on {}",
                    if is_resume { "exec resume" } else { "exec" }));
            }
        }

        let (message, stdout_raw) = if let Some((msg, raw)) = stdout_error {
            (msg, raw)
        } else {
            (
                format!("Process exited with code {:?}", status.code()),
                String::new(),
            )
        };

        codex_debug_log(&format!("Sending error: message={}", message));
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: stdout_raw,
            stderr: stderr_msg,
            exit_code: status.code(),
        });
        return Ok(());
    }

    // Even on success, check stderr for warnings (e.g., -c flag silently ignored)
    if let Some(h) = stderr_thread {
        if let Ok(stderr_content) = h.join() {
            if !stderr_content.is_empty() {
                codex_debug_log(&format!(
                    "[STDERR-SUCCESS] is_resume={}, stderr_len={}",
                    is_resume,
                    stderr_content.len()
                ));
                codex_debug_log(&format!(
                    "[STDERR-SUCCESS] content_first_500={:?}",
                    stderr_content.chars().take(500).collect::<String>()
                ));
                if stderr_content.contains("model_instructions_file")
                    || stderr_content.contains("unknown")
                    || stderr_content.contains("warning")
                    || stderr_content.contains("ignored")
                {
                    codex_debug_log("[STDERR-SUCCESS] *** WARNING: stderr mentions model_instructions_file/unknown/warning/ignored — -c flag may not be working as expected");
                }
            } else {
                codex_debug_log("[STDERR-SUCCESS] stderr is empty (no warnings)");
            }
        }
    }

    // Send synthetic Done if not received
    if !got_done {
        codex_debug_log("No Done message received, sending synthetic Done");
        // turn.completed never arrived, so the in-loop auto-deliver hook didn't
        // fire. Still try to deliver any image_gen output before closing the turn.
        if let Some(ctx) = auto_send {
            if let Some(sid) = last_session_id.as_deref() {
                auto_deliver_new_images(
                    sid,
                    &image_dir_snapshot,
                    &model_sent_paths,
                    turn_started_at,
                    ctx,
                    &sender,
                );
            }
        }
        let _ = sender.send(StreamMessage::Done {
            result: String::new(),
            session_id: last_session_id,
        });
    }

    codex_debug_log(&format!(
        "[SP-FILE] About to drop SpFileGuard (path={:?}), is_resume={}",
        _sp_guard.0, is_resume
    ));
    codex_debug_log("=== codex execute_command_streaming END (success) ===");
    Ok(())
    // _sp_guard dropped here — temp file removed
}

/// Resolve `~/.codex/generated_images/<session_id>/` for the given session.
fn generated_images_dir(session_id: &str) -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".codex/generated_images")
            .join(session_id),
    )
}

/// Snapshot existing files in the codex generated-images directory for a
/// resumed session, so we can later distinguish files created during this turn
/// from files left over from previous turns.
fn snapshot_image_dir(session_id: Option<&str>) -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    let Some(sid) = session_id else { return set };
    let Some(dir) = generated_images_dir(sid) else {
        return set;
    };
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                set.insert(entry.path());
            }
        }
    }
    set
}

/// Two paths point to the same file if either equality or canonicalization match.
fn paths_equivalent(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Extract the FILEPATH from a `cokacdir --sendfile <PATH> ...` invocation, if
/// the parsed Bash command happens to contain one. Used to record paths the
/// model already delivered so we don't double-send them.
fn extract_sendfile_path(name: &str, input_json: &str) -> Option<PathBuf> {
    if name != "Bash" {
        return None;
    }
    let v: Value = serde_json::from_str(input_json).ok()?;
    let cmd = v.get("command")?.as_str()?;

    // Locate `--sendfile` as a whitespace-bounded token (any whitespace,
    // matching what `split_whitespace` accepts), then capture its argument
    // honoring single/double quotes so paths with spaces survive —
    // split_whitespace alone would truncate at the first inner space.
    const TOKEN: &str = "--sendfile";
    let mut search_start = 0;
    while let Some(rel) = cmd[search_start..].find(TOKEN) {
        let pos = search_start + rel;
        let lhs_ok = pos == 0 || cmd[..pos].chars().last().map_or(true, char::is_whitespace);
        let rest = &cmd[pos + TOKEN.len()..];
        let rhs_ok = rest.chars().next().map_or(false, char::is_whitespace);
        if lhs_ok && rhs_ok {
            let after = rest.trim_start();
            let mut iter = after.chars();
            let first = iter.next()?;
            let arg: String = if first == '"' || first == '\'' {
                iter.take_while(|&c| c != first).collect()
            } else {
                std::iter::once(first)
                    .chain(iter.take_while(|c| !c.is_whitespace()))
                    .collect()
            };
            return Some(PathBuf::from(arg));
        }
        search_start = pos + TOKEN.len();
    }
    None
}

/// After a Codex turn completes, scan the session's generated-images directory
/// for image files created during this turn (mtime ≥ `started_at`) that aren't
/// in the pre-turn snapshot and weren't already delivered by the model itself,
/// then invoke `cokacdir --sendfile` for each one and emit synthetic
/// ToolUse/ToolResult events so the polling loop renders them like a normal
/// model-issued sendfile.
fn auto_deliver_new_images(
    session_id: &str,
    snapshot: &HashSet<PathBuf>,
    model_sent: &[PathBuf],
    started_at: std::time::SystemTime,
    ctx: &CodexAutoSendCtx,
    sender: &Sender<StreamMessage>,
) {
    let Some(dir) = generated_images_dir(session_id) else {
        return;
    };
    if !dir.exists() {
        return;
    }

    let rd = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) => {
            codex_debug_log(&format!(
                "[auto-send] read_dir failed: {} ({})",
                dir.display(),
                e
            ));
            return;
        }
    };

    // Collect candidate paths with their mtime, then sort by mtime ascending so
    // multi-image turns deliver in creation order.
    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        if snapshot.contains(&path) {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase());
        if !matches!(
            ext.as_deref(),
            Some("png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp")
        ) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < started_at {
            continue;
        }
        if model_sent.iter().any(|p| paths_equivalent(p, &path)) {
            codex_debug_log(&format!(
                "[auto-send] skip (already sent by model): {}",
                path.display()
            ));
            continue;
        }
        candidates.push((path, mtime));
    }
    candidates.sort_by_key(|(_, mtime)| *mtime);

    if candidates.is_empty() {
        return;
    }
    codex_debug_log(&format!(
        "[auto-send] {} image(s) to deliver from {}",
        candidates.len(),
        dir.display()
    ));

    for (path, _) in candidates {
        let path_str = path.to_string_lossy().to_string();
        codex_debug_log(&format!(
            "[auto-send] invoking cokacdir --sendfile {}",
            path_str
        ));

        let output = Command::new(&ctx.cokacdir_bin)
            .args([
                "--sendfile",
                &path_str,
                "--chat",
                &ctx.chat_id.to_string(),
                "--key",
                &ctx.bot_key,
            ])
            .output();

        let (stdout, exit_code, is_error) = match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let exit = out.status.code();
                let is_err = !out.status.success();
                codex_debug_log(&format!(
                    "[auto-send] result: exit={:?}, stdout_len={}, stderr_len={}",
                    exit,
                    stdout.len(),
                    out.stderr.len()
                ));
                if is_err {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    codex_debug_log(&format!(
                        "[auto-send] stderr_first_500={:?}",
                        stderr.chars().take(500).collect::<String>()
                    ));
                }
                (stdout, exit, is_err)
            }
            Err(e) => {
                codex_debug_log(&format!(
                    "[auto-send] spawn failed: {} (path={})",
                    e, path_str
                ));
                continue;
            }
        };

        // Build a Bash-shaped command string. `detect_cokacdir_command` matches
        // by basename of any whitespace-split token, so the bin path here makes
        // the polling loop route this exactly like a model-issued sendfile.
        let cmd_str = format!(
            "{} --sendfile {} --chat {} --key <auto>",
            ctx.cokacdir_bin, path_str, ctx.chat_id
        );
        let tool_input = serde_json::json!({
            "command": cmd_str,
            "exit_code": exit_code,
        })
        .to_string();

        if sender
            .send(StreamMessage::ToolUse {
                name: "Bash".to_string(),
                input: tool_input,
            })
            .is_err()
        {
            codex_debug_log("[auto-send] channel closed, aborting remaining deliveries");
            return;
        }
        if sender
            .send(StreamMessage::ToolResult {
                content: stdout,
                is_error,
            })
            .is_err()
        {
            codex_debug_log("[auto-send] channel closed after ToolUse, aborting");
            return;
        }
    }
}

/// Parse a Codex JSONL event into zero or more StreamMessages.
///
/// Returns Vec because some events (e.g. command_execution) produce
/// both ToolUse and ToolResult messages at once.
fn parse_codex_event(json: &Value) -> Vec<StreamMessage> {
    let event_type = match json.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return vec![],
    };

    match event_type {
        // Thread started — contains thread_id
        "thread.started" => {
            let thread_id = json
                .get("thread_id")
                .or_else(|| json.get("thread").and_then(|t| t.get("id")))
                .and_then(|v| v.as_str())
                .unwrap_or("codex-session")
                .to_string();
            vec![StreamMessage::Init {
                session_id: thread_id,
            }]
        }

        // Item completed — the main content carrier
        "item.completed" => parse_item_completed(json),

        // Turn completed — marks end of response
        "turn.completed" => {
            vec![StreamMessage::Done {
                result: String::new(),
                session_id: None,
            }]
        }

        // turn.failed has {error: {message: "..."}}
        "turn.failed" => {
            let message = json
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Codex error")
                .to_string();
            vec![StreamMessage::Error {
                message,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            }]
        }

        // Top-level error event has {message: "..."}
        "error" => {
            let message = json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Codex error")
                .to_string();
            vec![StreamMessage::Error {
                message,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            }]
        }

        // Ignored events — avoid duplicates (completed handles the final state)
        // Note: item.updated is intentionally ignored because StreamMessage::Text
        // appends to full_response. Processing todo_list updates would produce
        // duplicate lists in the Telegram output. The final state is captured
        // by item.completed → todo_list handler.
        "turn.started" | "item.started" | "item.updated" => vec![],

        // Unknown event types — ignore
        _ => {
            codex_debug_log(&format!("Unknown codex event type: {}", event_type));
            vec![]
        }
    }
}

/// Parse an `item.completed` event into StreamMessages.
fn parse_item_completed(json: &Value) -> Vec<StreamMessage> {
    // The item can be at top level or nested under "item"
    let item = json.get("item").unwrap_or(json);
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match item_type {
        // Agent text message
        "agent_message" | "message" => {
            let text = extract_text_content(item);
            if text.is_empty() {
                vec![]
            } else {
                vec![StreamMessage::Text { content: text }]
            }
        }

        // Command execution — produces ToolUse + ToolResult
        // Codex fields: command, aggregated_output, exit_code, status
        "command_execution" => {
            let command = item
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let aggregated_output = item
                .get("aggregated_output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let exit_code = item.get("exit_code").and_then(|v| v.as_i64());
            let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let is_error = matches!(status, "failed" | "declined")
                || exit_code.map(|c| c != 0).unwrap_or(false);

            vec![
                StreamMessage::ToolUse {
                    name: "Bash".to_string(),
                    input: serde_json::json!({"command": command, "exit_code": exit_code})
                        .to_string(),
                },
                StreamMessage::ToolResult {
                    content: aggregated_output,
                    is_error,
                },
            ]
        }

        // File change — Codex fields: changes (array of {path, kind, ...}), status
        "file_change" => {
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("completed");
            let is_error = status == "failed";

            let mut msgs = vec![StreamMessage::ToolUse {
                name: "FileChange".to_string(),
                input: item.to_string(),
            }];

            // Only generate ToolResult on error (ToolUse already shows changes via format_tool_input)
            if is_error {
                let content = item
                    .get("changes")
                    .and_then(|v| v.as_array())
                    .filter(|arr| !arr.is_empty())
                    .map(|changes| {
                        changes
                            .iter()
                            .map(|c| {
                                let path =
                                    c.get("path").and_then(|v| v.as_str()).unwrap_or("unknown");
                                let kind =
                                    c.get("kind").and_then(|v| v.as_str()).unwrap_or("update");
                                format!("{}: {}", kind, path)
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_else(|| "File change failed".to_string());
                msgs.push(StreamMessage::ToolResult {
                    content,
                    is_error: true,
                });
            }

            msgs
        }

        // MCP tool call — Codex fields: server, tool, arguments, result{content,structured_content}, error{message}, status
        "mcp_tool_call" => {
            let server = item.get("server").and_then(|v| v.as_str()).unwrap_or("");
            let tool = item
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let display_name = if server.is_empty() {
                tool.to_string()
            } else {
                format!("{}:{}", server, tool)
            };

            let arguments = item
                .get("arguments")
                .map(|v| v.to_string())
                .unwrap_or_default();

            let mut msgs = vec![StreamMessage::ToolUse {
                name: display_name,
                input: arguments,
            }];

            let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");

            // Check for error first (skip null — serde serializes None as null)
            if let Some(err) = item.get("error").filter(|v| !v.is_null()) {
                let message = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("MCP tool call failed")
                    .to_string();
                msgs.push(StreamMessage::ToolResult {
                    content: message,
                    is_error: true,
                });
            } else if let Some(result) = item.get("result").filter(|v| !v.is_null()) {
                // result has {content: [...], structured_content}
                let content = if let Some(arr) = result.get("content").and_then(|v| v.as_array()) {
                    arr.iter()
                        .filter_map(|c| {
                            c.get("text")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    result.to_string()
                };
                // codex's McpToolCallStatus: failed status overrides result presence.
                let is_error = status == "failed";
                msgs.push(StreamMessage::ToolResult { content, is_error });
            } else if status == "failed" {
                // status=failed but neither error nor result populated — surface a generic error
                // so the user isn't left wondering why a ToolUse had no result.
                msgs.push(StreamMessage::ToolResult {
                    content: "MCP tool call failed (no error details)".to_string(),
                    is_error: true,
                });
            }

            msgs
        }

        // Collab tool call — sub-agent interactions.
        // Codex CollabTool enum: spawn_agent, send_input, wait, close_agent.
        // (Other names — send_message/followup_task/wait_agent/list_agents — are
        //  forward-compat hooks for tools codex may add later; harmless if absent.)
        // Fields: tool, sender_thread_id, receiver_thread_ids, prompt, agents_states, status
        // CollabAgentStatus values: pending_init, running, interrupted, completed,
        //   errored, shutdown, not_found.
        "collab_tool_call" => {
            let tool = item
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
            // Extract prompt for tools that carry one; leave empty for others
            // (tool name is already in the Collab:{tool} name field — no need to repeat)
            let display = match tool {
                "spawn_agent" | "send_input" | "send_message" | "followup_task" => item
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(), // wait, close_agent, list_agents, wait_agent
            };

            let mut msgs = vec![StreamMessage::ToolUse {
                name: format!("Collab:{}", tool),
                input: display,
            }];

            // For agent-state tools, fold per-agent message into a ToolResult.
            // Successful agents (running/completed/shutdown) display message-only
            // to preserve existing UX. Problematic states (errored/interrupted/
            // not_found/pending_init) get a "[status]" prefix and mark the
            // ToolResult as is_error=true so the user actually sees the failure.
            if matches!(tool, "wait" | "close_agent" | "wait_agent" | "list_agents") {
                if let Some(states) = item.get("agents_states").and_then(|v| v.as_object()) {
                    let mut any_problem = false;
                    let entries: Vec<String> = states
                        .values()
                        .filter_map(|state| {
                            let agent_status =
                                state.get("status").and_then(|v| v.as_str()).unwrap_or("");
                            let message =
                                state.get("message").and_then(|v| v.as_str()).unwrap_or("");
                            let problem = matches!(
                                agent_status,
                                "errored" | "interrupted" | "not_found" | "pending_init"
                            );
                            if problem {
                                any_problem = true;
                            }
                            match (problem, message.is_empty()) {
                                (false, true) => None,
                                (false, false) => Some(message.to_string()),
                                (true, true) => Some(format!("[{}]", agent_status)),
                                (true, false) => Some(format!("[{}] {}", agent_status, message)),
                            }
                        })
                        .collect();
                    if !entries.is_empty() {
                        msgs.push(StreamMessage::ToolResult {
                            content: entries.join("\n---\n"),
                            is_error: any_problem || status == "failed",
                        });
                    } else if status == "failed" {
                        msgs.push(StreamMessage::ToolResult {
                            content: "Collab tool call failed".to_string(),
                            is_error: true,
                        });
                    }
                } else if status == "failed" {
                    msgs.push(StreamMessage::ToolResult {
                        content: "Collab tool call failed".to_string(),
                        is_error: true,
                    });
                }
            } else if status == "failed" {
                // Non-state tools (spawn_agent, send_input) may also fail — surface it.
                msgs.push(StreamMessage::ToolResult {
                    content: format!("Collab:{} failed", tool),
                    is_error: true,
                });
            }

            msgs
        }

        // Web search — Codex fields: id, query, action
        // action is a tagged enum (codex protocol::WebSearchAction) with variants:
        //   - search    {query?, queries?[]}
        //   - open_page {url?}
        //   - find_in_page {url?, pattern?}
        //   - other     (no fields)
        // Note: display is the raw query/URL without prefix — format_tool_input()
        // in telegram.rs adds the "Search:" prefix to avoid duplication.
        "web_search" => {
            let top_query = item.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let action = item.get("action");
            let action_type = action
                .and_then(|a| a.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let display = match action_type {
                "search" => {
                    // Prefer expanded queries[] when present, else fall back to action.query
                    // or top-level query.
                    let from_queries = action
                        .and_then(|a| a.get("queries"))
                        .and_then(|v| v.as_array())
                        .filter(|arr| !arr.is_empty())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|q| q.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        });
                    let from_action_query = action
                        .and_then(|a| a.get("query"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty());
                    from_queries
                        .or_else(|| from_action_query.map(|s| s.to_string()))
                        .unwrap_or_else(|| top_query.to_string())
                }
                "open_page" => {
                    let url = action
                        .and_then(|a| a.get("url"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if url.is_empty() {
                        top_query.to_string()
                    } else {
                        format!("open: {}", url)
                    }
                }
                "find_in_page" => {
                    let url = action
                        .and_then(|a| a.get("url"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let pattern = action
                        .and_then(|a| a.get("pattern"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match (url.is_empty(), pattern.is_empty()) {
                        (false, false) => format!("find: {} in {}", pattern, url),
                        (false, true) => format!("find_in_page: {}", url),
                        (true, false) => format!("find: {}", pattern),
                        (true, true) => top_query.to_string(),
                    }
                }
                _ => top_query.to_string(), // "other" or unknown — fall back
            };
            if display.is_empty() {
                vec![]
            } else {
                vec![StreamMessage::ToolUse {
                    name: "WebSearch".to_string(),
                    input: display,
                }]
            }
        }

        // Todo list — agent's running plan. Codex fields: items (Vec<{text, completed}>)
        "todo_list" => {
            if let Some(items) = item.get("items").and_then(|v| v.as_array()) {
                let summary: Vec<String> = items
                    .iter()
                    .map(|t| {
                        let text = t.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        let done = t
                            .get("completed")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        format!("[{}] {}", if done { "x" } else { " " }, text)
                    })
                    .collect();
                vec![StreamMessage::Text {
                    content: summary.join("\n"),
                }]
            } else {
                vec![]
            }
        }

        // Reasoning/thinking — internal, not shown to user
        "reasoning" => {
            codex_debug_log(&format!(
                "reasoning (filtered): {:?}",
                extract_text_content(item)
                    .chars()
                    .take(80)
                    .collect::<String>()
            ));
            vec![]
        }

        // Non-fatal error surfaced as an item — ErrorItem { message }
        "error" => {
            let message = item
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown error")
                .to_string();
            vec![StreamMessage::Text {
                content: format!("⚠ {}", message),
            }]
        }

        _ => {
            // Try to extract text from unknown item types (e.g. reasoning)
            let text = extract_text_content(item);
            if text.is_empty() {
                codex_debug_log(&format!("Unknown item type: {}", item_type));
                vec![]
            } else {
                vec![StreamMessage::Text { content: text }]
            }
        }
    }
}

/// Extract text content from a Codex item.
/// Handles both direct "content" string and array-of-objects format.
fn extract_text_content(item: &Value) -> String {
    // Try direct "content" string
    if let Some(text) = item.get("content").and_then(|v| v.as_str()) {
        return text.to_string();
    }

    // Try "text" field
    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
        return text.to_string();
    }

    // Try "content" as array of objects (OpenAI format)
    if let Some(content_arr) = item.get("content").and_then(|v| v.as_array()) {
        let mut text = String::new();
        for part in content_arr {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                text.push_str(t);
            }
        }
        if !text.is_empty() {
            return text;
        }
    }

    // Try nested message.content
    if let Some(message) = item.get("message") {
        if let Some(text) = message.get("content").and_then(|v| v.as_str()) {
            return text.to_string();
        }
        if let Some(content_arr) = message.get("content").and_then(|v| v.as_array()) {
            let mut text = String::new();
            for part in content_arr {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
            if !text.is_empty() {
                return text;
            }
        }
    }

    String::new()
}
