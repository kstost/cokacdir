use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use crate::services::claude::{
    create_private_temp_file, debug_log_to, kill_child_tree, send_success_terminal, CancelToken,
    PrivateTempFile, StreamMessage,
};

/// Context required to detect images that Codex's built-in `image_gen` tool
/// drops into `~/.codex/generated_images/<session_id>/` without surfacing any
/// JSON event in `codex exec --json`. Most callers auto-deliver them; companion
/// visible pings record the generated path so Telegram can attach it with a
/// caption and persist a visual reference.
pub struct CodexAutoSendCtx {
    pub cokacdir_bin: String,
    pub chat_id: i64,
    pub bot_key: String,
    pub send_files: bool,
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

#[derive(Debug)]
enum CodexJsonLine {
    Blank,
    Json(Value),
}

const CODEX_SQLITE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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

/// Forward a streaming event, terminating the spawned CLI when the consumer
/// has gone away.  Returning from the stdout loop and then calling
/// `child.wait()` is not sufficient: the reader still owns the stdout pipe,
/// so a child that keeps producing output can fill that pipe and block before
/// it exits.  Killing the request process group here makes receiver-drop a
/// terminal condition and prevents the worker thread from hanging forever.
fn send_or_abort_child(
    sender: &Sender<StreamMessage>,
    message: StreamMessage,
    child: &mut std::process::Child,
) -> bool {
    if sender.send(message).is_ok() {
        return true;
    }

    codex_debug_log("  ERROR: Channel receiver dropped; terminating Codex process tree");
    kill_child_tree(child);
    let _ = child.wait();
    false
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
    model: Option<&str>,
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
    let temp_dir = crate::utils::path::cokacdir_temp_dir()
        .map_err(|e| format!("Failed to prepare cokacdir temporary directory: {e}"))?;
    let out_guard = create_private_temp_file(&temp_dir, "verify_last_message", b"")
        .map_err(|e| format!("Failed to create verify output file: {e}"))?;
    let out_path = out_guard.path();

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
    if let Some(model) = model {
        args.push("-m".to_string());
        args.push(model.to_string());
    }
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
    let last_message = std::fs::read_to_string(out_path).ok();

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

fn codex_home_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".codex"))
}

fn codex_state_5_path() -> Option<PathBuf> {
    Some(codex_home_dir()?.join("state_5.sqlite"))
}

fn make_codex_uuid() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn codex_rollout_path(codex_home: &Path, session_id: &str) -> PathBuf {
    let ts = chrono::Utc::now();
    codex_home
        .join("sessions")
        .join(format!("{:04}", ts.format("%Y")))
        .join(format!("{:02}", ts.format("%m")))
        .join(format!("{:02}", ts.format("%d")))
        .join(format!(
            "rollout-{}-{}.jsonl",
            ts.format("%Y-%m-%dT%H-%M-%S"),
            session_id
        ))
}

fn quote_sql_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn set_codex_busy_timeout(conn: &rusqlite::Connection, label: &str) {
    if let Err(e) = conn.busy_timeout(CODEX_SQLITE_BUSY_TIMEOUT) {
        codex_debug_log(&format!(
            "[session-clone] failed to set Codex SQLite busy timeout for {}: {}",
            label, e
        ));
    }
}

fn ordered_table_columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({})", quote_sql_ident(table)))
        .map_err(|e| format!("Failed to inspect Codex state table {}: {}", table, e))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("Failed to read Codex state table info: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect Codex state columns: {}", e))?;
    if names.is_empty() {
        return Err(format!(
            "Codex state table `{}` not found or has no columns",
            table
        ));
    }
    Ok(names)
}

fn find_codex_rollout_from_state(session_id: &str) -> Option<PathBuf> {
    let state_5 = codex_state_5_path()?;
    if !state_5.is_file() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(&state_5, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    set_codex_busy_timeout(&conn, "state lookup");
    let path: String = conn
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .ok()?;
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

fn collect_codex_rollouts(dir: &Path, session_id: &str, out: &mut Vec<(u64, PathBuf)>) {
    let exact_suffix = format!("-{}.jsonl", session_id);
    let mut pending = vec![dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(&exact_suffix) {
                continue;
            }
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push((mtime, path));
        }
    }
}

fn find_codex_rollout_by_scan(session_id: &str) -> Option<PathBuf> {
    let root = codex_home_dir()?.join("sessions");
    let mut matches = Vec::new();
    collect_codex_rollouts(&root, session_id, &mut matches);
    matches.sort_by(|a, b| b.0.cmp(&a.0));
    matches.into_iter().map(|(_, path)| path).next()
}

fn find_codex_rollout(session_id: &str) -> Result<PathBuf, String> {
    find_codex_rollout_from_state(session_id)
        .or_else(|| find_codex_rollout_by_scan(session_id))
        .ok_or_else(|| format!("Codex rollout not found for session {}", session_id))
}

const MAX_CODEX_ROLLOUT_BYTES: u64 = 512 * 1024 * 1024;

fn read_stable_codex_rollout(path: &Path) -> Result<String, String> {
    for attempt in 0..3 {
        let before = std::fs::symlink_metadata(path)
            .map_err(|e| format!("Failed to inspect Codex rollout {}: {}", path.display(), e))?;
        if !before.file_type().is_file() || before.file_type().is_symlink() {
            return Err(format!(
                "Codex rollout is not a real regular file: {}",
                path.display()
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            if before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(format!(
                    "Codex rollout is a reparse point: {}",
                    path.display()
                ));
            }
        }
        if before.len() > MAX_CODEX_ROLLOUT_BYTES {
            return Err(format!(
                "Codex rollout exceeds the {} MiB safety limit: {}",
                MAX_CODEX_ROLLOUT_BYTES / 1024 / 1024,
                path.display()
            ));
        }

        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let mut file = options
            .open(path)
            .map_err(|e| format!("Failed to open Codex rollout {}: {}", path.display(), e))?;
        let opened = file
            .metadata()
            .map_err(|e| format!("Failed to inspect Codex rollout {}: {}", path.display(), e))?;
        if !opened.is_file() || opened.len() > MAX_CODEX_ROLLOUT_BYTES {
            return Err(format!(
                "Codex rollout is not a bounded regular file: {}",
                path.display()
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if before.dev() != opened.dev() || before.ino() != opened.ino() {
                if attempt < 2 {
                    continue;
                }
                return Err(format!(
                    "Codex rollout kept changing while being opened: {}",
                    path.display()
                ));
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            if opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                || opened.creation_time() != before.creation_time()
                || opened.last_write_time() != before.last_write_time()
                || opened.file_size() != before.file_size()
            {
                if attempt < 2 {
                    continue;
                }
                return Err(format!(
                    "Codex rollout kept changing while being opened: {}",
                    path.display()
                ));
            }
        }

        let mut bytes = Vec::with_capacity(opened.len().min(1024 * 1024) as usize);
        Read::by_ref(&mut file)
            .take(MAX_CODEX_ROLLOUT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Failed to read Codex rollout {}: {}", path.display(), e))?;
        if bytes.len() as u64 > MAX_CODEX_ROLLOUT_BYTES {
            return Err(format!(
                "Codex rollout grew beyond the {} MiB safety limit: {}",
                MAX_CODEX_ROLLOUT_BYTES / 1024 / 1024,
                path.display()
            ));
        }
        let after = file
            .metadata()
            .map_err(|e| format!("Failed to recheck Codex rollout {}: {}", path.display(), e))?;
        let current = std::fs::symlink_metadata(path).ok();
        #[cfg(unix)]
        let stable = {
            use std::os::unix::fs::MetadataExt;
            current.as_ref().is_some_and(|current| {
                current.file_type().is_file()
                    && !current.file_type().is_symlink()
                    && current.dev() == opened.dev()
                    && current.ino() == opened.ino()
            }) && after.dev() == opened.dev()
                && after.ino() == opened.ino()
                && after.len() == opened.len()
                && after.mtime() == opened.mtime()
                && after.mtime_nsec() == opened.mtime_nsec()
                && after.ctime() == opened.ctime()
                && after.ctime_nsec() == opened.ctime_nsec()
                && bytes.len() as u64 == opened.len()
        };
        #[cfg(windows)]
        let stable = {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            current.as_ref().is_some_and(|current| {
                current.file_type().is_file()
                    && current.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
                    && current.creation_time() == opened.creation_time()
                    && current.last_write_time() == opened.last_write_time()
                    && current.file_size() == opened.file_size()
            }) && after.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
                && after.creation_time() == opened.creation_time()
                && after.last_write_time() == opened.last_write_time()
                && after.file_size() == opened.file_size()
                && bytes.len() as u64 == opened.len()
        };
        #[cfg(not(any(unix, windows)))]
        let stable = current.as_ref().is_some_and(|current| {
            current.file_type().is_file()
                && current.len() == opened.len()
                && current.modified().ok() == opened.modified().ok()
        }) && after.len() == opened.len()
            && after.modified().ok() == opened.modified().ok()
            && bytes.len() as u64 == opened.len();

        if stable {
            return String::from_utf8(bytes).map_err(|e| {
                format!("Codex rollout is not valid UTF-8 {}: {}", path.display(), e)
            });
        }
        if attempt == 2 {
            return Err(format!(
                "Codex rollout kept changing while it was read: {}",
                path.display()
            ));
        }
    }
    unreachable!()
}

fn read_codex_jsonl_lines(path: &Path) -> Result<Vec<CodexJsonLine>, String> {
    let content = read_stable_codex_rollout(path)?;
    let ended_with_newline = content.ends_with('\n') || content.is_empty();
    let raw_lines = content.split('\n').collect::<Vec<_>>();
    let line_count = if ended_with_newline {
        raw_lines.len().saturating_sub(1)
    } else {
        raw_lines.len()
    };
    let mut lines = Vec::new();
    for (idx, line) in raw_lines.into_iter().take(line_count).enumerate() {
        if line.trim().is_empty() {
            lines.push(CodexJsonLine::Blank);
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => lines.push(CodexJsonLine::Json(value)),
            Err(e) => {
                return Err(format!(
                    "Failed to parse Codex rollout line {} in {}: {}",
                    idx + 1,
                    path.display(),
                    e
                ));
            }
        }
    }
    Ok(lines)
}

fn first_codex_payload_string(lines: &[CodexJsonLine], key: &str) -> Option<String> {
    for line in lines {
        let CodexJsonLine::Json(Value::Object(map)) = line else {
            continue;
        };
        let Some(Value::Object(payload)) = map.get("payload") else {
            continue;
        };
        if let Some(value) = payload.get(key).and_then(|v| v.as_str()) {
            return Some(value.to_string());
        }
    }
    None
}

fn rewrite_payload_string_if_equal(
    payload: &mut serde_json::Map<String, Value>,
    key: &str,
    old: &str,
    new: &str,
) {
    if let Some(Value::String(value)) = payload.get_mut(key) {
        if value == old {
            *value = new.to_string();
        }
    }
}

fn patch_codex_jsonl_lines(
    lines: &mut [CodexJsonLine],
    old_sid: &str,
    new_sid: &str,
    old_cwd: &str,
    new_cwd: &str,
) {
    for line in lines {
        let CodexJsonLine::Json(Value::Object(map)) = line else {
            continue;
        };
        let Some(Value::Object(payload)) = map.get_mut("payload") else {
            continue;
        };
        rewrite_payload_string_if_equal(payload, "id", old_sid, new_sid);
        rewrite_payload_string_if_equal(payload, "cwd", old_cwd, new_cwd);
    }
}

fn write_codex_jsonl_lines_atomic(path: &Path, lines: &[CodexJsonLine]) -> Result<u64, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "Failed to create Codex rollout dir {}: {}",
                parent.display(),
                e
            )
        })?;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("clone.jsonl");
    let tmp_path = path.with_file_name(format!(".{}.tmp-{}", file_name, make_codex_uuid()));
    let result = (|| -> Result<u64, String> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp_path).map_err(|e| {
            format!(
                "Failed to create temp Codex rollout {}: {}",
                tmp_path.display(),
                e
            )
        })?;
        for line in lines {
            match line {
                CodexJsonLine::Blank => {
                    writeln!(file)
                        .map_err(|e| format!("Failed to write blank Codex rollout line: {}", e))?;
                }
                CodexJsonLine::Json(value) => {
                    serde_json::to_writer(&mut file, value)
                        .map_err(|e| format!("Failed to write Codex rollout JSON: {}", e))?;
                    writeln!(file)
                        .map_err(|e| format!("Failed to finish Codex rollout line: {}", e))?;
                }
            }
        }
        file.sync_all()
            .map_err(|e| format!("Failed to sync Codex rollout {}: {}", tmp_path.display(), e))?;
        std::fs::rename(&tmp_path, path).map_err(|e| {
            format!(
                "Failed to move Codex rollout {} -> {}: {}",
                tmp_path.display(),
                path.display(),
                e
            )
        })?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|e| {
                    format!(
                        "Failed to sync Codex rollout directory {}: {}",
                        parent.display(),
                        e
                    )
                })?;
        }
        Ok(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

fn copy_codex_state_thread_row(
    source_session_id: &str,
    new_session_id: &str,
    rollout_path: &Path,
    cwd: &str,
) -> Result<Option<PathBuf>, String> {
    use rusqlite::types::Value as SqlValue;

    let Some(state_5) = codex_state_5_path() else {
        return Ok(None);
    };
    if !state_5.is_file() {
        return Ok(None);
    }

    let mut conn = rusqlite::Connection::open(&state_5)
        .map_err(|e| format!("Failed to open Codex state DB {}: {}", state_5.display(), e))?;
    set_codex_busy_timeout(&conn, "state row copy");
    let columns = ordered_table_columns(&conn, "threads")?;
    for required in ["id", "rollout_path", "cwd"] {
        if !columns.iter().any(|column| column == required) {
            return Err(format!(
                "Codex threads table missing expected column `{}`",
                required
            ));
        }
    }

    let tx = conn
        .transaction()
        .map_err(|e| format!("Failed to start Codex state transaction: {}", e))?;
    let select_sql = format!(
        "SELECT {} FROM threads WHERE id = ?1",
        columns
            .iter()
            .map(|column| quote_sql_ident(column))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut stmt = tx
        .prepare(&select_sql)
        .map_err(|e| format!("Failed to prepare Codex state row copy: {}", e))?;
    let mut values = match stmt.query_row(rusqlite::params![source_session_id], |row| {
        let mut values = Vec::with_capacity(columns.len());
        for idx in 0..columns.len() {
            values.push(row.get::<_, SqlValue>(idx)?);
        }
        Ok(values)
    }) {
        Ok(values) => values,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(format!(
                "Source Codex state row not found for {}; cannot clone session",
                source_session_id
            ));
        }
        Err(e) => return Err(format!("Failed to read source Codex state row: {}", e)),
    };
    drop(stmt);

    for (column, value) in columns.iter().zip(values.iter_mut()) {
        match column.as_str() {
            "id" => *value = SqlValue::Text(new_session_id.to_string()),
            "rollout_path" => *value = SqlValue::Text(rollout_path.display().to_string()),
            "cwd" => *value = SqlValue::Text(cwd.to_string()),
            _ => {}
        }
    }

    tx.execute(
        "DELETE FROM threads WHERE id = ?1",
        rusqlite::params![new_session_id],
    )
    .map_err(|e| format!("Failed to clear existing Codex clone state row: {}", e))?;
    let placeholders = (1..=columns.len())
        .map(|idx| format!("?{}", idx))
        .collect::<Vec<_>>()
        .join(", ");
    let insert_sql = format!(
        "INSERT INTO threads ({}) VALUES ({})",
        columns
            .iter()
            .map(|column| quote_sql_ident(column))
            .collect::<Vec<_>>()
            .join(", "),
        placeholders
    );
    tx.execute(&insert_sql, rusqlite::params_from_iter(values.iter()))
        .map_err(|e| format!("Failed to insert Codex clone state row: {}", e))?;
    tx.commit()
        .map_err(|e| format!("Failed to commit Codex clone state row: {}", e))?;
    Ok(Some(state_5))
}

fn clone_codex_session_raw(
    source_session_id: &str,
    working_dir: &str,
) -> Result<(String, PathBuf, Option<PathBuf>), String> {
    codex_debug_log(&format!(
        "[session-clone] cloning Codex session {}",
        source_session_id
    ));
    let source = find_codex_rollout(source_session_id)?;
    let codex_home = codex_home_dir().ok_or_else(|| "Cannot locate ~/.codex".to_string())?;
    let new_session_id = make_codex_uuid();
    let target = codex_rollout_path(&codex_home, &new_session_id);
    let mut lines = read_codex_jsonl_lines(&source)?;
    if !lines
        .iter()
        .any(|line| matches!(line, CodexJsonLine::Json(_)))
    {
        return Err(format!(
            "Codex rollout has no complete JSON records: {}",
            source.display()
        ));
    }
    let old_cwd =
        first_codex_payload_string(&lines, "cwd").unwrap_or_else(|| working_dir.to_string());
    patch_codex_jsonl_lines(
        &mut lines,
        source_session_id,
        &new_session_id,
        &old_cwd,
        working_dir,
    );
    let bytes = write_codex_jsonl_lines_atomic(&target, &lines)?;
    let state_5_path =
        match copy_codex_state_thread_row(source_session_id, &new_session_id, &target, working_dir)
        {
            Ok(path) => path,
            Err(e) => {
                let _ = std::fs::remove_file(&target);
                return Err(e);
            }
        };
    codex_debug_log(&format!(
        "[session-clone] cloned Codex rollout {} -> {}, new_session={}, bytes={}, indexed={}",
        source.display(),
        target.display(),
        new_session_id,
        bytes,
        state_5_path.is_some()
    ));
    Ok((new_session_id, target, state_5_path))
}

/// Clone a Codex session for a scheduled run and leave the clone on disk.
/// The scheduled task resumes the returned session id; the source session is
/// never mutated.
pub fn clone_session_for_schedule(
    source_session_id: &str,
    working_dir: &str,
) -> Result<String, String> {
    if !crate::services::process::is_valid_session_id(source_session_id) {
        return Err(format!("Invalid session_id: {}", source_session_id));
    }
    let (new_session_id, _, _) = clone_codex_session_raw(source_session_id, working_dir)?;
    Ok(new_session_id)
}

/// Execute a command using Codex CLI with streaming output.
///
/// Parameters mirror `claude::execute_command_streaming` for consistency.
/// Codex agents intentionally run with full execution permissions in this
/// application, so `allowed_tools` is compatibility metadata rather than a
/// security boundary and is deliberately ignored here. The approval/sandbox
/// bypass below is therefore expected product behavior, including for
/// background companion work. `no_session_persistence` maps to
/// `codex exec --ephemeral` for one-shot calls.
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
    _allowed_tools: Option<&[String]>, // intentionally advisory: Codex runs with full permissions
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    model: Option<&str>,                  // "codex:" prefix already stripped
    no_session_persistence: bool,         // true = pass `--ephemeral` on new sessions
    auto_send: Option<&CodexAutoSendCtx>, // when Some, detect image_gen outputs that the model forgot to sendfile
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
        "no_session_persistence: {}",
        no_session_persistence
    ));
    if is_resume && no_session_persistence {
        return Err("Codex ephemeral execution cannot resume an existing session".to_string());
    }
    codex_debug_log(&format!(
        "system_prompt: is_some={}, len={}",
        system_prompt.is_some(),
        system_prompt.map(|s| s.len()).unwrap_or(0)
    ));

    // Write system prompt to file and use -c model_instructions_file to pass it,
    // mirroring Claude's --append-system-prompt-file pattern.
    // This works for both new sessions and resume (instruction changes take effect immediately).
    let mut _sp_guard: Option<PrivateTempFile> = None;

    // Build CLI arguments
    let mut args = if let Some(sid) = session_id {
        // Validate before placing the value adjacent to other CLI flags;
        // an unvalidated sid like "--config /etc/passwd" would be treated
        // as a new flag by Codex's argument parser.
        if !crate::services::process::is_valid_session_id(sid) {
            return Err(format!("Invalid session_id format: {}", sid));
        }
        codex_debug_log(&format!("Building RESUME args, session_id={}", sid));
        // Full permissions are intentional for every Codex agent invocation;
        // the bypass is not an accidental omission of the caller's tool list.
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
        // Full permissions are intentional for every Codex agent invocation,
        // including ephemeral/background workers.
        // codex exec --json [--ephemeral] --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <dir> -
        let mut args = vec![
            "exec".to_string(),
            "--json".to_string(),
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "--skip-git-repo-check".to_string(),
            "-C".to_string(),
            working_dir.to_string(),
        ];
        if no_session_persistence {
            args.insert(2, "--ephemeral".to_string());
        }
        args
    };

    // Write system prompt to temp file and pass via -c model_instructions_file
    if let Some(sp) = system_prompt {
        if !sp.is_empty() {
            codex_debug_log(&format!(
                "[SP-FILE] is_resume={}, system_prompt_len={} — writing temp file",
                is_resume,
                sp.len()
            ));
            let sp_dir = crate::utils::path::cokacdir_temp_dir()
                .map_err(|e| format!("Failed to prepare cokacdir temporary directory: {}", e))?;
            codex_debug_log(&format!("[SP-FILE] sp_dir={:?}", sp_dir));
            let sp_guard =
                create_private_temp_file(&sp_dir, "codex_sp", sp.as_bytes()).map_err(|e| {
                    codex_debug_log(&format!(
                        "[SP-FILE] ERROR: Failed to write system prompt file: {}",
                        e
                    ));
                    format!("Failed to write system prompt file: {}", e)
                })?;
            let sp_path = sp_guard.path();
            codex_debug_log(&format!("[SP-FILE] sp_path={:?}", sp_path));
            // Verify the file was written correctly
            match std::fs::metadata(sp_path) {
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
            _sp_guard = Some(sp_guard);
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

    // Reasoning effort override. Accepted values are model-dependent and are
    // validated by the caller before they reach this CLI argument builder.
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
    let mut last_assistant_message: Option<String> = None;
    let mut stdout_error: Option<(String, String)> = None;
    let mut line_count = 0;
    // Track paths the model itself delivered via cokacdir --sendfile so the
    // post-turn auto-deliver pass doesn't double-send the same file.
    let mut model_sent_paths: Vec<PathBuf> = Vec::new();

    codex_debug_log("Entering JSONL lines loop...");
    for line in reader.lines() {
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
                stdout_error = Some((format!("Failed to read output: {}", e), String::new()));
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
            if let Some(content) = completed_agent_message(&json) {
                last_assistant_message = Some(content);
            } else if item_event_invalidates_terminal_candidate(&json) {
                // Fail closed as soon as any new item starts or updates, and
                // for every completed non-Assistant item. An Assistant message
                // followed by more work is intermediate narration unless a
                // later genuine completed Assistant message supersedes it.
                last_assistant_message = None;
            }

            let messages = parse_codex_event(&json);
            codex_debug_log(&format!("  Parsed {} messages", messages.len()));

            for msg in messages {
                match &msg {
                    StreamMessage::Init { session_id } => {
                        codex_debug_log(&format!("  >>> Init: session_id={}", session_id));
                        last_session_id = Some(session_id.clone());
                    }
                    StreamMessage::Done { .. } => {
                        codex_debug_log("  >>> Done (deferred until process exit)");
                        got_done = true;
                        continue;
                    }
                    StreamMessage::AssistantFinal { .. } => {}
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

                if !send_or_abort_child(&sender, msg, &mut child) {
                    // `send_or_abort_child` has already reaped the child.  Do
                    // not fall through to the normal `child.wait()` path.
                    return Ok(());
                }
            }
        } else {
            codex_debug_log(&format!("  NOT valid JSON: {}", line_preview));
            // The unknown line may describe work after the last completed
            // Assistant item. Keep UI streaming tolerant, but fail closed for
            // the canonical durable-memory projection.
            last_assistant_message = None;
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
        _sp_guard.is_some()
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

    if !got_done {
        codex_debug_log("No turn.completed event received; completing from successful exit");
    }

    // Image delivery and terminal messages are success effects: neither may
    // happen merely because a turn.completed line preceded a failing exit.
    if let Some(ctx) = auto_send {
        if let Some(sid) = last_session_id.as_deref() {
            auto_deliver_new_images(sid, &image_dir_snapshot, &model_sent_paths, ctx, &sender);
        } else {
            codex_debug_log("[auto-send] skipped: no session_id captured");
        }
    }

    let result = last_assistant_message.unwrap_or_default();
    let assistant_final = (got_done && !result.trim().is_empty()).then(|| result.clone());
    let _ = send_success_terminal(&sender, assistant_final, result, last_session_id);

    codex_debug_log(&format!(
        "[SP-FILE] About to drop SpFileGuard (path={:?}), is_resume={}",
        _sp_guard.as_ref().map(PrivateTempFile::path),
        is_resume
    ));
    codex_debug_log("=== codex execute_command_streaming END (success) ===");
    Ok(())
    // _sp_guard dropped here — temp file removed
}

/// Resolve `~/.codex/generated_images/<session_id>/` for the given session.
fn generated_images_dir(session_id: &str) -> Option<PathBuf> {
    generated_images_dir_from_roots(
        session_id,
        std::env::var_os("CODEX_HOME").as_deref(),
        dirs::home_dir().as_deref(),
    )
}

fn generated_images_dir_from_roots(
    session_id: &str,
    codex_home: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let codex_home = codex_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".codex")))?;
    Some(codex_home.join("generated_images").join(session_id))
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

fn build_auto_send_command(ctx: &CodexAutoSendCtx, path: &str) -> Command {
    let mut command = Command::new(&ctx.cokacdir_bin);
    command
        .args([
            "--sendfile",
            path,
            "--chat",
            &ctx.chat_id.to_string(),
            "--key-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn run_auto_send_command(
    ctx: &CodexAutoSendCtx,
    path: &str,
) -> std::io::Result<std::process::Output> {
    let mut child = build_auto_send_command(ctx, path).spawn()?;
    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("sendfile child stdin was not piped"))
        .and_then(|mut stdin| {
            stdin.write_all(ctx.bot_key.as_bytes())?;
            stdin.write_all(b"\n")
        });
    if let Err(error) = write_result {
        // A failed key handoff can leave the child blocked waiting for EOF.
        // Kill and reap it before returning so automatic delivery never leaks
        // a subprocess.
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    child.wait_with_output()
}

/// After a Codex turn completes, scan the session's generated-images directory
/// for image files that aren't in the pre-turn snapshot and weren't already
/// delivered by the model itself,
/// then invoke `cokacdir --sendfile` for each one and emit synthetic
/// ToolUse/ToolResult events so the polling loop renders them like a normal
/// model-issued sendfile.
fn auto_deliver_new_images(
    session_id: &str,
    snapshot: &HashSet<PathBuf>,
    model_sent: &[PathBuf],
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
        if !ctx.send_files {
            codex_debug_log(&format!(
                "[auto-send] recording generated image {}",
                path_str
            ));
            let tool_input = serde_json::json!({
                "path": path_str,
                "delivered": false,
            })
            .to_string();
            if sender
                .send(StreamMessage::ToolUse {
                    name: "GeneratedImage".to_string(),
                    input: tool_input.clone(),
                })
                .is_err()
            {
                codex_debug_log("[auto-send] channel closed, aborting generated-image records");
                return;
            }
            if sender
                .send(StreamMessage::ToolResult {
                    content: tool_input,
                    is_error: false,
                })
                .is_err()
            {
                codex_debug_log("[auto-send] channel closed after generated-image record");
                return;
            }
            continue;
        }

        codex_debug_log(&format!(
            "[auto-send] invoking cokacdir --sendfile {}",
            path_str
        ));

        // Pass the capability through a pipe, never argv or the environment.
        // Process listings and crash reports can expose command-line values.
        let output = run_auto_send_command(ctx, &path_str);

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
            "{} --sendfile {} --chat {} --key-stdin",
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

        // Ignored events — avoid duplicates (completed handles the final state).
        // Processing todo_list updates would produce duplicate lists in the
        // Telegram output. The final state is captured by the item.completed
        // todo_list handler as a task notification, not assistant answer text.
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
                let summary = summary.join("\n");
                if summary.is_empty() {
                    vec![]
                } else {
                    vec![StreamMessage::TaskNotification {
                        task_id: "todo_list".to_string(),
                        status: "updated".to_string(),
                        summary,
                    }]
                }
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

/// Return only a genuine completed Assistant message, excluding error items,
/// reasoning, task summaries, and unknown text-bearing protocol objects.
fn completed_agent_message(json: &Value) -> Option<String> {
    if json.get("type").and_then(Value::as_str) != Some("item.completed") {
        return None;
    }
    let item = json.get("item").unwrap_or(json);
    if !item_is_assistant_message(item) {
        return None;
    }
    let content = extract_text_content(item);
    (!content.trim().is_empty()).then_some(content)
}

fn item_is_assistant_message(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("agent_message") => true,
        // Some Codex protocol versions use the generic `message` item. If
        // they provide a role, accept only Assistant; an absent role keeps
        // compatibility with the untagged generic shape.
        Some("message") => item
            .get("role")
            .and_then(Value::as_str)
            .map(|role| role.eq_ignore_ascii_case("assistant"))
            .unwrap_or(true),
        _ => false,
    }
}

fn item_event_invalidates_terminal_candidate(json: &Value) -> bool {
    match json.get("type").and_then(Value::as_str) {
        // Starting or updating any new protocol item means a previously
        // completed Assistant message was not terminal. The later completed
        // Assistant item, if one arrives, becomes the new candidate.
        Some("item.started" | "item.updated") => true,
        Some("item.completed") => {
            let item = json.get("item").unwrap_or(json);
            !item_is_assistant_message(item)
        }
        _ => false,
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

#[cfg(test)]
mod receiver_drop_tests {
    use super::*;

    #[test]
    fn automatic_sendfile_command_keeps_key_out_of_argv() {
        let ctx = CodexAutoSendCtx {
            cokacdir_bin: "cokacdir-test".to_string(),
            chat_id: 42,
            bot_key: "capability-that-must-not-be-an-argument".to_string(),
            send_files: true,
        };
        let command = build_auto_send_command(&ctx, "/tmp/generated image.png");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            args,
            vec![
                "--sendfile",
                "/tmp/generated image.png",
                "--chat",
                "42",
                "--key-stdin",
            ]
        );
        assert!(!args.iter().any(|arg| arg.contains(&ctx.bot_key)));
    }

    #[test]
    fn canonical_assistant_projection_accepts_only_agent_messages() {
        let assistant = serde_json::json!({
            "type": "item.completed",
            "item": {"type": "agent_message", "content": "final answer"}
        });
        let non_fatal_error = serde_json::json!({
            "type": "item.completed",
            "item": {"type": "error", "message": "diagnostic"}
        });
        let unknown_text = serde_json::json!({
            "type": "item.completed",
            "item": {"type": "future_protocol_item", "text": "not canonical"}
        });
        let user_message = serde_json::json!({
            "type": "item.completed",
            "item": {"type": "message", "role": "user", "content": "not assistant"}
        });

        assert_eq!(
            completed_agent_message(&assistant).as_deref(),
            Some("final answer")
        );
        assert!(completed_agent_message(&non_fatal_error).is_none());
        assert!(completed_agent_message(&unknown_text).is_none());
        assert!(completed_agent_message(&user_message).is_none());
    }

    #[test]
    fn later_item_activity_invalidates_an_earlier_terminal_candidate() {
        for item_type in [
            "command_execution",
            "file_change",
            "mcp_tool_call",
            "collab_tool_call",
            "web_search",
            "todo_list",
            "reasoning",
            "error",
            "future_protocol_item",
        ] {
            let event = serde_json::json!({
                "type": "item.completed",
                "item": {"type": item_type}
            });
            assert!(
                item_event_invalidates_terminal_candidate(&event),
                "{item_type}"
            );
        }
        assert!(!item_event_invalidates_terminal_candidate(
            &serde_json::json!({
                "type": "item.completed",
                "item": {"type": "agent_message", "content": "answer"}
            })
        ));
        assert!(item_event_invalidates_terminal_candidate(
            &serde_json::json!({
                "type": "item.completed",
                "item": {"type": "message", "role": "user", "content": "question"}
            })
        ));
        assert!(item_event_invalidates_terminal_candidate(
            &serde_json::json!({
                "type": "item.started",
                "item": {"type": "agent_message"}
            })
        ));
        assert!(item_event_invalidates_terminal_candidate(
            &serde_json::json!({
                "type": "item.updated",
                "item": {"type": "future_protocol_item"}
            })
        ));
        assert!(!item_event_invalidates_terminal_candidate(
            &serde_json::json!({
                "type": "turn.completed"
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn automatic_sendfile_writes_key_only_to_child_stdin() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let receiver = temp.path().join("receiver.sh");
        std::fs::write(&receiver, "#!/bin/sh\ncat\n").unwrap();
        std::fs::set_permissions(&receiver, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ctx = CodexAutoSendCtx {
            cokacdir_bin: receiver.to_string_lossy().into_owned(),
            chat_id: 42,
            bot_key: "private-capability".to_string(),
            send_files: true,
        };

        let output = run_auto_send_command(&ctx, "/tmp/image.png").unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, b"private-capability\n");
        assert!(output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn dropped_stream_receiver_terminates_and_reaps_child() {
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::services::claude::detach_into_own_pgroup(&mut command);
        let mut child = command.spawn().expect("spawn output-producing child");

        let (sender, receiver) = std::sync::mpsc::channel();
        drop(receiver);
        assert!(!send_or_abort_child(
            &sender,
            StreamMessage::Text {
                content: "ignored".to_string(),
            },
            &mut child,
        ));
        assert!(child.try_wait().expect("query reaped child").is_some());
    }

    #[test]
    fn generated_images_honor_custom_codex_home() {
        let custom = std::path::Path::new("/custom/codex-home");
        let fallback = std::path::Path::new("/home/tester");
        assert_eq!(
            generated_images_dir_from_roots("session-1", Some(custom.as_os_str()), Some(fallback),),
            Some(custom.join("generated_images").join("session-1"))
        );
    }

    #[test]
    fn rollout_scan_handles_deep_directory_trees_iteratively() {
        let temp = tempfile::tempdir().unwrap();
        let mut current = temp.path().to_path_buf();
        for _ in 0..160 {
            current.push("d");
            std::fs::create_dir(&current).unwrap();
        }
        let rollout = current.join("rollout-session-1.jsonl");
        std::fs::write(&rollout, b"{}\n").unwrap();

        let mut found = Vec::new();
        collect_codex_rollouts(temp.path(), "session-1", &mut found);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, rollout);
    }

    #[test]
    fn session_clone_rejects_an_incomplete_final_rollout_record() {
        let temp = tempfile::tempdir().unwrap();
        let rollout = temp.path().join("rollout.jsonl");
        std::fs::write(&rollout, b"{\"type\":\"ok\"}\n{\"incomplete\":").unwrap();

        let error = read_codex_jsonl_lines(&rollout).unwrap_err();

        assert!(error.contains("Failed to parse Codex rollout line 2"));
    }

    #[cfg(unix)]
    #[test]
    fn session_clone_rejects_symlink_rollout_sources() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside.jsonl");
        let rollout = temp.path().join("rollout.jsonl");
        std::fs::write(&outside, b"{\"secret\":true}\n").unwrap();
        symlink(&outside, &rollout).unwrap();

        assert!(read_codex_jsonl_lines(&rollout).is_err());
    }
}
