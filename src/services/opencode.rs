//! OpenCode service — spawns `opencode run --format json` and translates its
//! JSONL event stream into the existing `StreamMessage` / `ClaudeResponse` types.
//!
//! The public API mirrors the other CLI providers so callers can swap backends
//! with minimal code changes.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::services::claude::{
    debug_log_to, kill_child_tree, terminate_child_after_receiver_drop, CancelToken,
    ClaudeResponse, StreamMessage,
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
static OPENCODE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

fn opencode_db_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        paths.push(
            PathBuf::from(local_app_data)
                .join("opencode")
                .join("opencode.db"),
        );
    }
    if let Ok(app_data) = std::env::var("APPDATA") {
        paths.push(PathBuf::from(app_data).join("opencode").join("opencode.db"));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(
            home.join(".local")
                .join("share")
                .join("opencode")
                .join("opencode.db"),
        );
    }
    paths
}

fn opencode_db_path() -> Option<PathBuf> {
    let candidates = opencode_db_candidates();
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

fn set_opencode_busy_timeout(conn: &rusqlite::Connection, label: &str) {
    if let Err(e) = conn.busy_timeout(Duration::from_secs(5)) {
        opencode_debug(&format!(
            "[opencode-db] failed to set OpenCode SQLite busy timeout for {}: {}",
            label, e
        ));
    }
}

fn make_opencode_id(prefix: &str) -> String {
    let descending = prefix == "ses_";
    format!("{}{}", prefix, opencode_identifier(descending))
}

fn opencode_identifier(descending: bool) -> String {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0);
    let base = unix_ms.wrapping_mul(0x1000);
    let sequence = loop {
        let last = OPENCODE_SEQUENCE.load(std::sync::atomic::Ordering::SeqCst);
        let next = if last < base {
            base.wrapping_add(1)
        } else {
            last.wrapping_add(1)
        };
        if OPENCODE_SEQUENCE
            .compare_exchange(
                last,
                next,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            break next;
        }
    };
    let mask = 0xffff_ffff_ffffu64;
    let mut timestamp_prefix = sequence & mask;
    if descending {
        timestamp_prefix = (!timestamp_prefix) & mask;
    }
    format!("{timestamp_prefix:012x}{}", random_base62(14))
}

fn random_base62(len: usize) -> String {
    use rand::RngCore;

    const CHARS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut out = String::with_capacity(len);
    while out.len() < len {
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut bytes);
        for byte in bytes {
            if out.len() >= len {
                break;
            }
            out.push(CHARS[byte as usize % CHARS.len()] as char);
        }
    }
    out
}

fn quote_sql_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn ordered_table_columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>, String> {
    let pragma_sql = format!("PRAGMA table_info({})", quote_sql_ident(table));
    let mut stmt = conn
        .prepare(&pragma_sql)
        .map_err(|e| format!("Failed to inspect OpenCode table {}: {}", table, e))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("Failed to read OpenCode table {} columns: {}", table, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect OpenCode table {} columns: {}", table, e))?;
    if columns.is_empty() {
        return Err(format!("OpenCode table {} has no columns", table));
    }
    Ok(columns)
}

fn opencode_table_exists(conn: &rusqlite::Connection, table: &str) -> Result<bool, String> {
    let mut stmt = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1")
        .map_err(|e| format!("Failed to inspect OpenCode schema: {}", e))?;
    match stmt.query_row(rusqlite::params![table], |_| Ok(())) {
        Ok(()) => Ok(true),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(e) => Err(format!("Failed to inspect OpenCode table {}: {}", table, e)),
    }
}

fn column_index(columns: &[String], name: &str) -> Result<usize, String> {
    columns
        .iter()
        .position(|column| column == name)
        .ok_or_else(|| format!("OpenCode row clone missing expected column `{}`", name))
}

fn optional_column_index(columns: &[String], name: &str) -> Option<usize> {
    columns.iter().position(|column| column == name)
}

fn sql_text(value: &rusqlite::types::Value) -> Option<&str> {
    match value {
        rusqlite::types::Value::Text(s) => Some(s.as_str()),
        _ => None,
    }
}

fn read_opencode_rows(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    columns: &[String],
    where_column: &str,
    where_value: &str,
) -> Result<Vec<Vec<rusqlite::types::Value>>, String> {
    let sql = format!(
        "SELECT {} FROM {} WHERE {} = ?1",
        columns
            .iter()
            .map(|column| quote_sql_ident(column))
            .collect::<Vec<_>>()
            .join(", "),
        quote_sql_ident(table),
        quote_sql_ident(where_column)
    );
    let mut stmt = tx
        .prepare(&sql)
        .map_err(|e| format!("Failed to prepare OpenCode {} clone read: {}", table, e))?;
    let rows = stmt
        .query_map(rusqlite::params![where_value], |row| {
            let mut values = Vec::with_capacity(columns.len());
            for idx in 0..columns.len() {
                values.push(row.get::<_, rusqlite::types::Value>(idx)?);
            }
            Ok(values)
        })
        .map_err(|e| format!("Failed to query OpenCode {} rows: {}", table, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect OpenCode {} rows: {}", table, e))?;
    Ok(rows)
}

fn insert_opencode_row(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    columns: &[String],
    values: &[rusqlite::types::Value],
) -> Result<(), String> {
    let placeholders = (1..=columns.len())
        .map(|idx| format!("?{}", idx))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote_sql_ident(table),
        columns
            .iter()
            .map(|column| quote_sql_ident(column))
            .collect::<Vec<_>>()
            .join(", "),
        placeholders
    );
    tx.execute(&sql, rusqlite::params_from_iter(values.iter()))
        .map_err(|e| format!("Failed to insert cloned OpenCode {} row: {}", table, e))?;
    Ok(())
}

type TodoFingerprintCounts = HashMap<String, usize>;

fn remap_known_opencode_id(
    id: &str,
    msg_id_map: &HashMap<String, String>,
    part_id_map: &HashMap<String, String>,
    event_id_map: &HashMap<String, String>,
    old_session_id: &str,
    new_session_id: &str,
) -> Option<String> {
    msg_id_map
        .get(id)
        .or_else(|| part_id_map.get(id))
        .or_else(|| event_id_map.get(id))
        .cloned()
        .or_else(|| {
            if id == old_session_id {
                Some(new_session_id.to_string())
            } else {
                None
            }
        })
}

fn remap_opencode_json_refs(
    value: &mut Value,
    msg_id_map: &HashMap<String, String>,
    part_id_map: &HashMap<String, String>,
    event_id_map: &HashMap<String, String>,
    old_session_id: &str,
    new_session_id: &str,
    old_cwd: Option<&str>,
    new_cwd: &str,
) {
    match value {
        Value::Object(obj) => {
            for (key, child) in obj.iter_mut() {
                if let Some(s) = child.as_str() {
                    let replacement = match key.as_str() {
                        "parentID" | "parent_id" | "messageID" | "message_id" => {
                            msg_id_map.get(s).cloned()
                        }
                        "partID" | "part_id" => part_id_map.get(s).cloned(),
                        "sessionID" | "session_id" => {
                            if s == old_session_id {
                                Some(new_session_id.to_string())
                            } else {
                                None
                            }
                        }
                        "id" => remap_known_opencode_id(
                            s,
                            msg_id_map,
                            part_id_map,
                            event_id_map,
                            old_session_id,
                            new_session_id,
                        ),
                        _ => None,
                    };
                    if let Some(new_value) = replacement {
                        *child = Value::String(new_value);
                        continue;
                    }
                }
                if key == "path" {
                    if let Some(old_cwd) = old_cwd {
                        if let Some(path_obj) = child.as_object_mut() {
                            if path_obj.get("cwd").and_then(|v| v.as_str()) == Some(old_cwd) {
                                path_obj
                                    .insert("cwd".to_string(), Value::String(new_cwd.to_string()));
                            }
                        }
                    }
                }
                remap_opencode_json_refs(
                    child,
                    msg_id_map,
                    part_id_map,
                    event_id_map,
                    old_session_id,
                    new_session_id,
                    old_cwd,
                    new_cwd,
                );
            }
        }
        Value::Array(items) => {
            for child in items {
                remap_opencode_json_refs(
                    child,
                    msg_id_map,
                    part_id_map,
                    event_id_map,
                    old_session_id,
                    new_session_id,
                    old_cwd,
                    new_cwd,
                );
            }
        }
        _ => {}
    }
}

fn rewrite_opencode_data_json(
    data: &str,
    msg_id_map: &HashMap<String, String>,
    part_id_map: &HashMap<String, String>,
    event_id_map: &HashMap<String, String>,
    old_session_id: &str,
    new_session_id: &str,
    old_cwd: Option<&str>,
    new_cwd: &str,
) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(data) else {
        return data.to_string();
    };
    remap_opencode_json_refs(
        &mut value,
        msg_id_map,
        part_id_map,
        event_id_map,
        old_session_id,
        new_session_id,
        old_cwd,
        new_cwd,
    );
    serde_json::to_string(&value).unwrap_or_else(|_| data.to_string())
}

fn clone_opencode_session_rows(
    conn: &mut rusqlite::Connection,
    source_session_id: &str,
    new_session_id: &str,
    working_dir: &str,
) -> Result<(usize, usize, usize, usize), String> {
    if !opencode_table_exists(conn, "session")? {
        return Err("OpenCode DB missing `session` table".to_string());
    }
    if !opencode_table_exists(conn, "message")? {
        return Err("OpenCode DB missing `message` table".to_string());
    }
    if !opencode_table_exists(conn, "part")? {
        return Err("OpenCode DB missing `part` table".to_string());
    }
    let has_session_message = opencode_table_exists(conn, "session_message")?;
    let has_session_share = opencode_table_exists(conn, "session_share")?;
    let has_todo = opencode_table_exists(conn, "todo")?;

    let session_columns = ordered_table_columns(conn, "session")?;
    let message_columns = ordered_table_columns(conn, "message")?;
    let part_columns = ordered_table_columns(conn, "part")?;
    let session_message_columns = if has_session_message {
        ordered_table_columns(conn, "session_message")?
    } else {
        Vec::new()
    };
    let todo_columns = if has_todo {
        ordered_table_columns(conn, "todo")?
    } else {
        Vec::new()
    };

    let tx = conn
        .transaction()
        .map_err(|e| format!("Failed to start OpenCode clone transaction: {}", e))?;

    let mut session_rows =
        read_opencode_rows(&tx, "session", &session_columns, "id", source_session_id)?;
    let mut session_row = match session_rows.pop() {
        Some(row) if session_rows.is_empty() => row,
        Some(_) => {
            return Err(format!(
                "Multiple OpenCode session rows for {}",
                source_session_id
            ))
        }
        None => return Err(format!("OpenCode session not found: {}", source_session_id)),
    };
    let old_directory = optional_column_index(&session_columns, "directory")
        .and_then(|idx| sql_text(&session_row[idx]).map(ToString::to_string));

    let message_rows = read_opencode_rows(
        &tx,
        "message",
        &message_columns,
        "session_id",
        source_session_id,
    )?;
    let part_rows =
        read_opencode_rows(&tx, "part", &part_columns, "session_id", source_session_id)?;
    let session_message_rows = if has_session_message {
        read_opencode_rows(
            &tx,
            "session_message",
            &session_message_columns,
            "session_id",
            source_session_id,
        )?
    } else {
        Vec::new()
    };
    let todo_rows = if has_todo {
        read_opencode_rows(&tx, "todo", &todo_columns, "session_id", source_session_id)?
    } else {
        Vec::new()
    };
    let todo_count = todo_rows.len();

    let session_id_idx = column_index(&session_columns, "id")?;
    session_row[session_id_idx] = rusqlite::types::Value::Text(new_session_id.to_string());
    if let Some(idx) = optional_column_index(&session_columns, "directory") {
        session_row[idx] = rusqlite::types::Value::Text(working_dir.to_string());
    }
    if let Some(idx) = optional_column_index(&session_columns, "share_url") {
        session_row[idx] = rusqlite::types::Value::Null;
    }
    if let Some(idx) = optional_column_index(&session_columns, "time_compacting") {
        session_row[idx] = rusqlite::types::Value::Null;
    }
    if let Some(idx) = optional_column_index(&session_columns, "time_archived") {
        session_row[idx] = rusqlite::types::Value::Null;
    }

    let message_id_idx = column_index(&message_columns, "id")?;
    let message_session_idx = column_index(&message_columns, "session_id")?;
    let mut msg_id_map = HashMap::<String, String>::new();
    for row in &message_rows {
        let Some(old_id) = sql_text(&row[message_id_idx]) else {
            return Err("OpenCode message row has non-text id".to_string());
        };
        msg_id_map.insert(old_id.to_string(), make_opencode_id("msg_"));
    }

    let part_id_idx = column_index(&part_columns, "id")?;
    let part_session_idx = column_index(&part_columns, "session_id")?;
    let mut part_id_map = HashMap::<String, String>::new();
    for row in &part_rows {
        let Some(old_id) = sql_text(&row[part_id_idx]) else {
            return Err("OpenCode part row has non-text id".to_string());
        };
        part_id_map.insert(old_id.to_string(), make_opencode_id("prt_"));
    }

    let mut event_id_map = HashMap::<String, String>::new();
    let session_message_id_idx = if has_session_message {
        let idx = column_index(&session_message_columns, "id")?;
        for row in &session_message_rows {
            let Some(old_id) = sql_text(&row[idx]) else {
                return Err("OpenCode session_message row has non-text id".to_string());
            };
            event_id_map.insert(old_id.to_string(), make_opencode_id("evt_"));
        }
        Some(idx)
    } else {
        None
    };
    let todo_session_idx = if has_todo {
        Some(column_index(&todo_columns, "session_id")?)
    } else {
        None
    };

    tx.execute(
        "DELETE FROM part WHERE session_id = ?1",
        rusqlite::params![new_session_id],
    )
    .map_err(|e| format!("Failed to clear OpenCode clone parts: {}", e))?;
    tx.execute(
        "DELETE FROM message WHERE session_id = ?1",
        rusqlite::params![new_session_id],
    )
    .map_err(|e| format!("Failed to clear OpenCode clone messages: {}", e))?;
    if has_session_message {
        tx.execute(
            "DELETE FROM session_message WHERE session_id = ?1",
            rusqlite::params![new_session_id],
        )
        .map_err(|e| format!("Failed to clear OpenCode clone events: {}", e))?;
    }
    if has_todo {
        tx.execute(
            "DELETE FROM todo WHERE session_id = ?1",
            rusqlite::params![new_session_id],
        )
        .map_err(|e| format!("Failed to clear OpenCode clone todos: {}", e))?;
    }
    if has_session_share {
        tx.execute(
            "DELETE FROM session_share WHERE session_id = ?1",
            rusqlite::params![new_session_id],
        )
        .map_err(|e| format!("Failed to clear OpenCode clone shares: {}", e))?;
    }
    tx.execute(
        "DELETE FROM session WHERE id = ?1",
        rusqlite::params![new_session_id],
    )
    .map_err(|e| format!("Failed to clear OpenCode clone session: {}", e))?;

    insert_opencode_row(&tx, "session", &session_columns, &session_row)?;

    let message_data_idx = optional_column_index(&message_columns, "data");
    let message_parent_idx = optional_column_index(&message_columns, "parent_id");
    for mut row in message_rows {
        let old_id = sql_text(&row[message_id_idx])
            .ok_or_else(|| "OpenCode message row has non-text id".to_string())?
            .to_string();
        let new_id = msg_id_map
            .get(&old_id)
            .ok_or_else(|| format!("OpenCode message id map missing {}", old_id))?;
        row[message_id_idx] = rusqlite::types::Value::Text(new_id.clone());
        row[message_session_idx] = rusqlite::types::Value::Text(new_session_id.to_string());
        if let Some(idx) = message_parent_idx {
            if let Some(parent_id) = sql_text(&row[idx]).map(ToString::to_string) {
                if let Some(new_parent_id) = msg_id_map.get(&parent_id) {
                    row[idx] = rusqlite::types::Value::Text(new_parent_id.clone());
                }
            }
        }
        if let Some(idx) = message_data_idx {
            if let Some(data) = sql_text(&row[idx]).map(ToString::to_string) {
                row[idx] = rusqlite::types::Value::Text(rewrite_opencode_data_json(
                    &data,
                    &msg_id_map,
                    &part_id_map,
                    &event_id_map,
                    source_session_id,
                    new_session_id,
                    old_directory.as_deref(),
                    working_dir,
                ));
            }
        }
        insert_opencode_row(&tx, "message", &message_columns, &row)?;
    }

    let part_message_idx = optional_column_index(&part_columns, "message_id");
    let part_data_idx = optional_column_index(&part_columns, "data");
    for mut row in part_rows {
        let old_id = sql_text(&row[part_id_idx])
            .ok_or_else(|| "OpenCode part row has non-text id".to_string())?
            .to_string();
        let new_id = part_id_map
            .get(&old_id)
            .ok_or_else(|| format!("OpenCode part id map missing {}", old_id))?;
        row[part_id_idx] = rusqlite::types::Value::Text(new_id.clone());
        row[part_session_idx] = rusqlite::types::Value::Text(new_session_id.to_string());
        if let Some(idx) = part_message_idx {
            let old_message_id = sql_text(&row[idx])
                .map(ToString::to_string)
                .ok_or_else(|| format!("OpenCode part {} has non-text message_id", old_id))?;
            let new_message_id = msg_id_map.get(&old_message_id).ok_or_else(|| {
                format!(
                    "OpenCode part {} references unknown message_id {}",
                    old_id, old_message_id
                )
            })?;
            row[idx] = rusqlite::types::Value::Text(new_message_id.clone());
        }
        if let Some(idx) = part_data_idx {
            if let Some(data) = sql_text(&row[idx]).map(ToString::to_string) {
                row[idx] = rusqlite::types::Value::Text(rewrite_opencode_data_json(
                    &data,
                    &msg_id_map,
                    &part_id_map,
                    &event_id_map,
                    source_session_id,
                    new_session_id,
                    old_directory.as_deref(),
                    working_dir,
                ));
            }
        }
        insert_opencode_row(&tx, "part", &part_columns, &row)?;
    }

    if has_session_message {
        let session_message_id_idx = session_message_id_idx.expect("checked above");
        let session_message_session_idx = column_index(&session_message_columns, "session_id")?;
        let session_message_data_idx = optional_column_index(&session_message_columns, "data");
        for mut row in session_message_rows {
            let old_id = sql_text(&row[session_message_id_idx])
                .ok_or_else(|| "OpenCode session_message row has non-text id".to_string())?
                .to_string();
            let new_id = event_id_map
                .get(&old_id)
                .ok_or_else(|| format!("OpenCode event id map missing {}", old_id))?;
            row[session_message_id_idx] = rusqlite::types::Value::Text(new_id.clone());
            row[session_message_session_idx] =
                rusqlite::types::Value::Text(new_session_id.to_string());
            if let Some(idx) = session_message_data_idx {
                if let Some(data) = sql_text(&row[idx]).map(ToString::to_string) {
                    row[idx] = rusqlite::types::Value::Text(rewrite_opencode_data_json(
                        &data,
                        &msg_id_map,
                        &part_id_map,
                        &event_id_map,
                        source_session_id,
                        new_session_id,
                        old_directory.as_deref(),
                        working_dir,
                    ));
                }
            }
            insert_opencode_row(&tx, "session_message", &session_message_columns, &row)?;
        }
    }

    if has_todo {
        let todo_session_idx = todo_session_idx.expect("checked above");
        for mut row in todo_rows {
            row[todo_session_idx] = rusqlite::types::Value::Text(new_session_id.to_string());
            insert_opencode_row(&tx, "todo", &todo_columns, &row)?;
        }
    }

    tx.commit()
        .map_err(|e| format!("Failed to commit OpenCode clone transaction: {}", e))?;
    Ok((
        msg_id_map.len(),
        part_id_map.len(),
        event_id_map.len(),
        todo_count,
    ))
}

/// Clone an OpenCode session by copying its SQLite rows and remapping only the
/// row identifiers/references that must be unique. The clone is then safe to
/// resume through the normal serve/SSE execution path.
pub fn clone_session_for_schedule(
    source_session_id: &str,
    working_dir: &str,
) -> Result<String, String> {
    opencode_debug(&format!(
        "[session-clone] cloning OpenCode session {}",
        source_session_id
    ));
    if !crate::services::process::is_valid_session_id(source_session_id) {
        return Err(format!("Invalid session_id format: {}", source_session_id));
    }
    let db_path = opencode_db_path().ok_or_else(|| "Cannot locate OpenCode DB".to_string())?;
    if !db_path.is_file() {
        return Err(format!("OpenCode DB not found: {}", db_path.display()));
    }
    let mut conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| format!("Failed to open OpenCode DB {}: {}", db_path.display(), e))?;
    set_opencode_busy_timeout(&conn, "session clone");
    conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        .map_err(|e| {
            format!(
                "Failed to acquire OpenCode DB write lock for {}: {}",
                db_path.display(),
                e
            )
        })?;

    let new_session_id = make_opencode_id("ses_");
    let (messages, parts, events, todos) =
        clone_opencode_session_rows(&mut conn, source_session_id, &new_session_id, working_dir)?;
    opencode_debug(&format!(
        "[session-clone] cloned OpenCode session {} -> {} (messages={}, parts={}, events={}, todos={})",
        source_session_id, new_session_id, messages, parts, events, todos
    ));
    Ok(new_session_id)
}

#[cfg(test)]
mod session_clone_tests {
    use super::*;
    use rusqlite::params;

    fn seed_opencode_clone_schema(conn: &rusqlite::Connection) {
        conn.execute_batch(
            r#"
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                parent_id TEXT,
                slug TEXT NOT NULL,
                directory TEXT NOT NULL,
                title TEXT NOT NULL,
                version TEXT NOT NULL,
                share_url TEXT,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                time_compacting INTEGER,
                time_archived INTEGER,
                workspace_id TEXT,
                path TEXT
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                parent_id TEXT,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE session_message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE session_share (
                session_id TEXT PRIMARY KEY,
                id TEXT NOT NULL,
                secret TEXT NOT NULL,
                url TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
            );
            CREATE TABLE todo (
                session_id TEXT NOT NULL,
                content TEXT NOT NULL,
                status TEXT NOT NULL,
                priority TEXT NOT NULL,
                position INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                PRIMARY KEY(session_id, position)
            );
            "#,
        )
        .unwrap();
    }

    fn seed_opencode_clone_rows(conn: &rusqlite::Connection) {
        conn.execute(
            "INSERT INTO session VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                "ses_source",
                "project_1",
                "source-slug",
                "/old/work",
                "Source",
                "1.0.0",
                "https://share.example/source",
                10_i64,
                20_i64,
                30_i64,
                40_i64,
                "workspace_1",
                "project-relative/path",
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
            params![
                "msg_parent",
                "ses_source",
                11_i64,
                12_i64,
                r#"{"path":{"cwd":"/old/work"},"text":"root"}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "msg_child",
                "ses_source",
                "msg_parent",
                13_i64,
                14_i64,
                r#"{"parentID":"msg_parent","path":{"cwd":"/old/work"},"text":"child"}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "prt_parent",
                "msg_parent",
                "ses_source",
                15_i64,
                16_i64,
                r#"{"type":"text","text":"root"}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "prt_child",
                "msg_child",
                "ses_source",
                17_i64,
                18_i64,
                r#"{"type":"text","text":"child","id":"prt_child","messageID":"msg_child","sessionID":"ses_source"}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "evt_source",
                "ses_source",
                "message",
                19_i64,
                20_i64,
                r#"{"id":"evt_source","messageID":"msg_child","partID":"prt_child","sessionID":"ses_source"}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO todo VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "ses_source",
                "first task",
                "pending",
                "high",
                0_i64,
                21_i64,
                22_i64,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO todo VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "ses_source",
                "done task",
                "completed",
                "low",
                1_i64,
                23_i64,
                24_i64,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_share VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "ses_source",
                "share_1",
                "secret_1",
                "https://share.example/source",
                25_i64,
                26_i64,
            ],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO session VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, ?10, ?11)",
            params![
                "ses_clone",
                "old_project",
                "stale-slug",
                "/stale",
                "Stale",
                "1.0.0",
                "https://share.example/stale",
                1_i64,
                2_i64,
                "stale_ws",
                "stale/path",
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
            params![
                "msg_stale",
                "ses_clone",
                1_i64,
                2_i64,
                r#"{"text":"stale"}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "prt_stale",
                "msg_stale",
                "ses_clone",
                1_i64,
                2_i64,
                r#"{"text":"stale"}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO todo VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params!["ses_clone", "stale", "pending", "low", 0_i64, 1_i64, 2_i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_share VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "ses_clone",
                "share_stale",
                "secret_stale",
                "https://share.example/stale",
                1_i64,
                2_i64,
            ],
        )
        .unwrap();
    }

    #[test]
    fn clone_opencode_session_rows_preserves_project_path_and_copies_todos() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        seed_opencode_clone_schema(&conn);
        seed_opencode_clone_rows(&conn);

        let counts =
            clone_opencode_session_rows(&mut conn, "ses_source", "ses_clone", "/new/work").unwrap();
        assert_eq!(counts, (2, 2, 1, 2));

        let (directory, path, share_url, compacting, archived): (
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
        ) = conn
            .query_row(
                "SELECT directory, path, share_url, time_compacting, time_archived FROM session WHERE id = ?1",
                params!["ses_clone"],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(directory, "/new/work");
        assert_eq!(path.as_deref(), Some("project-relative/path"));
        assert_eq!(share_url, None);
        assert_eq!(compacting, None);
        assert_eq!(archived, None);

        let root_id: String = conn
            .query_row(
                "SELECT id FROM message WHERE session_id = ?1 AND data LIKE '%root%'",
                params!["ses_clone"],
                |row| row.get(0),
            )
            .unwrap();
        let (child_id, child_parent_id, child_data): (String, String, String) = conn
            .query_row(
                "SELECT id, parent_id, data FROM message WHERE session_id = ?1 AND data LIKE '%child%'",
                params!["ses_clone"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_ne!(root_id, "msg_parent");
        assert_ne!(child_id, "msg_child");
        assert_eq!(child_parent_id, root_id);

        let child_json: Value = serde_json::from_str(&child_data).unwrap();
        assert_eq!(child_json["parentID"].as_str(), Some(root_id.as_str()));
        assert_eq!(child_json["path"]["cwd"].as_str(), Some("/new/work"));

        let part_message_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM part WHERE session_id = ?1 AND message_id IN (?2, ?3)",
                params!["ses_clone", root_id, child_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(part_message_count, 2);

        let (child_part_id, child_part_data): (String, String) = conn
            .query_row(
                "SELECT id, data FROM part WHERE session_id = ?1 AND message_id = ?2 AND data LIKE '%child%'",
                params!["ses_clone", child_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_ne!(child_part_id, "prt_child");
        let child_part_json: Value = serde_json::from_str(&child_part_data).unwrap();
        assert_eq!(child_part_json["id"].as_str(), Some(child_part_id.as_str()));
        assert_eq!(
            child_part_json["messageID"].as_str(),
            Some(child_id.as_str())
        );
        assert_eq!(child_part_json["sessionID"].as_str(), Some("ses_clone"));

        let (event_id, event_data): (String, String) = conn
            .query_row(
                "SELECT id, data FROM session_message WHERE session_id = ?1",
                params!["ses_clone"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_ne!(event_id, "evt_source");
        let event_json: Value = serde_json::from_str(&event_data).unwrap();
        assert_eq!(event_json["id"].as_str(), Some(event_id.as_str()));
        assert_eq!(event_json["messageID"].as_str(), Some(child_id.as_str()));
        assert_eq!(event_json["partID"].as_str(), Some(child_part_id.as_str()));
        assert_eq!(event_json["sessionID"].as_str(), Some("ses_clone"));

        let todo_rows: Vec<(String, String, String, i64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT content, status, priority, position FROM todo WHERE session_id = ?1 ORDER BY position",
                )
                .unwrap();
            stmt.query_map(params!["ses_clone"], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        assert_eq!(
            todo_rows,
            vec![
                (
                    "first task".to_string(),
                    "pending".to_string(),
                    "high".to_string(),
                    0,
                ),
                (
                    "done task".to_string(),
                    "completed".to_string(),
                    "low".to_string(),
                    1,
                ),
            ]
        );

        let clone_share_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_share WHERE session_id = ?1",
                params!["ses_clone"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(clone_share_rows, 0);
    }

    #[test]
    fn todo_pending_check_ignores_unchanged_baseline_todos() {
        let baseline_todos = vec![
            json!({
                "content": "old task",
                "status": "pending",
                "priority": "high",
                "position": 0,
            }),
            json!({
                "content": "done task",
                "status": "completed",
                "priority": "low",
                "position": 1,
            }),
        ];
        let baseline = unfinished_todo_fingerprints(&baseline_todos);

        assert!(!todos_pending_after_baseline(&baseline_todos, &baseline));

        let new_pending = vec![
            baseline_todos[0].clone(),
            json!({
                "content": "new task",
                "status": "pending",
                "priority": "medium",
                "position": 2,
            }),
        ];
        assert!(todos_pending_after_baseline(&new_pending, &baseline));

        let changed_existing = vec![json!({
            "content": "old task",
            "status": "in_progress",
            "priority": "high",
            "position": 0,
        })];
        assert!(todos_pending_after_baseline(&changed_existing, &baseline));

        // The HTTP todo endpoint exposes content/status/priority, but current
        // OpenCode SDK types do not expose the DB `position` column. Keep counts
        // so a newly-created duplicate unfinished todo is still detected.
        let http_todos = vec![json!({
            "content": "repeatable task",
            "status": "pending",
            "priority": "high",
        })];
        let http_baseline = unfinished_todo_fingerprints(&http_todos);
        assert!(!todos_pending_after_baseline(&http_todos, &http_baseline));

        let duplicated_http_todos = vec![http_todos[0].clone(), http_todos[0].clone()];
        assert!(todos_pending_after_baseline(
            &duplicated_http_todos,
            &http_baseline
        ));

        let id_baseline_todos = vec![json!({
            "id": "todo_a",
            "content": "same visible task",
            "status": "pending",
            "priority": "medium",
        })];
        let id_baseline = unfinished_todo_fingerprints(&id_baseline_todos);
        let same_visible_new_id = vec![json!({
            "id": "todo_b",
            "content": "same visible task",
            "status": "pending",
            "priority": "medium",
        })];
        assert!(todos_pending_after_baseline(
            &same_visible_new_id,
            &id_baseline
        ));
    }
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
// Inject system prompt into AGENTS.md with transactional restore
// ============================================================
//
// Every pathname operation below is fail-closed. In particular, a marker is
// never authority to overwrite AGENTS.md: both the filesystem object identity
// and the SHA-256 recorded before publication must still match. If a user edits
// or replaces AGENTS.md while OpenCode is running, the transaction and original
// quarantine are deliberately left in place for manual recovery.

const AGENTS_MD: &str = "AGENTS.md";
#[cfg(test)]
const LEGACY_WORKSPACE_LOCK_FILE: &str = ".AGENTS.md.cokacdir-lock";
const LOCK_DIRECTORY: &str = "opencode-agent-locks";
const STATE_PREFIX: &str = ".AGENTS.md.cokacdir-state-v2.";
const STATE_STAGING_PREFIX: &str = ".AGENTS.md.cokacdir-state-staging-v2.";
const STATE_DONE_PREFIX: &str = ".AGENTS.md.cokacdir-state-done-v2.";
const STAGED_PREFIX: &str = ".AGENTS.md.cokacdir-staged-v2.";
const BACKUP_PREFIX: &str = ".AGENTS.md.cokacdir-backup-v2.";
const RETIRED_PREFIX: &str = ".AGENTS.md.cokacdir-retired-v2.";

// These names were used by the unsafe legacy recovery protocol. Their content
// is ambiguous, so v2 never deletes or restores from them automatically.
const LEGACY_BACKUP_FILE: &str = ".AGENTS.md.cokacdir-backup";
const LEGACY_NO_ORIGINAL_SENTINEL: &str = ".AGENTS.md.cokacdir-no-original";
const LEGACY_TMP_FILE: &str = ".AGENTS.md.cokacdir-tmp";

const MAX_ORIGINAL_AGENTS_BYTES: u64 = 8 * 1024 * 1024;
const MAX_INJECTED_AGENTS_BYTES: u64 = 16 * 1024 * 1024;
const MAX_STATE_BYTES: u64 = 64 * 1024;
const TXN_VERSION: u32 = 2;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AgentsFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial: u64,
    #[cfg(windows)]
    file_index: u64,
    #[cfg(windows)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_id_128: Option<[u8; 16]>,
    #[cfg(not(any(unix, windows)))]
    length: u64,
    #[cfg(not(any(unix, windows)))]
    modified_nanos: u128,
}

impl PartialEq for AgentsFileIdentity {
    fn eq(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            return self.device == other.device && self.inode == other.inode;
        }
        #[cfg(windows)]
        {
            return match (self.file_id_128, other.file_id_128) {
                (Some(left), Some(right)) => {
                    self.volume_serial == other.volume_serial && left == right
                }
                _ => {
                    // Markers created by older versions contain only the
                    // legacy 64-bit index. Keep their recovery compatible,
                    // while new markers compare the complete ReFS-safe ID.
                    self.volume_serial == other.volume_serial && self.file_index == other.file_index
                }
            };
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.length == other.length && self.modified_nanos == other.modified_nanos
        }
    }
}

impl Eq for AgentsFileIdentity {}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AgentsFileRecord {
    identity: AgentsFileIdentity,
    length: u64,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AgentsTxnPlan {
    version: u32,
    phase: String,
    txn: String,
    original: Option<AgentsFileRecord>,
    injected_length: u64,
    injected_sha256: String,
    backup_name: Option<String>,
    staged_name: String,
    retired_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AgentsTxnReady {
    version: u32,
    phase: String,
    txn: String,
    injected: AgentsFileRecord,
}

#[derive(Debug)]
struct ParsedAgentsState {
    plan: AgentsTxnPlan,
    ready: Option<AgentsTxnReady>,
    marker_record: AgentsFileRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentsFileSnapshot {
    length: u64,
    readonly: bool,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(windows)]
    creation_time: u64,
    #[cfg(windows)]
    last_write_time: u64,
    #[cfg(windows)]
    attributes: u32,
    #[cfg(not(any(unix, windows)))]
    modified_nanos: u128,
}

fn metadata_is_safe_regular(metadata: &std::fs::Metadata) -> bool {
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn metadata_snapshot(metadata: &std::fs::Metadata) -> AgentsFileSnapshot {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        AgentsFileSnapshot {
            length: metadata.len(),
            readonly: metadata.permissions().readonly(),
            mode: metadata.mode(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        AgentsFileSnapshot {
            length: metadata.len(),
            readonly: metadata.permissions().readonly(),
            creation_time: metadata.creation_time(),
            last_write_time: metadata.last_write_time(),
            attributes: metadata.file_attributes(),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        AgentsFileSnapshot {
            length: metadata.len(),
            readonly: metadata.permissions().readonly(),
            modified_nanos,
        }
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &std::fs::File) -> std::io::Result<AgentsFileIdentity> {
    let (volume_serial, object) =
        crate::services::file_ops::stable_file_identity(file)?.components();
    let mut legacy_index = [0u8; 8];
    legacy_index.copy_from_slice(&object[..8]);
    Ok(AgentsFileIdentity {
        volume_serial,
        file_index: u64::from_le_bytes(legacy_index),
        file_id_128: Some(object),
    })
}

fn file_identity(
    file: &std::fs::File,
    metadata: &std::fs::Metadata,
) -> std::io::Result<AgentsFileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = file;
        Ok(AgentsFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        windows_file_identity(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let _ = file;
        Ok(AgentsFileIdentity {
            length: metadata.len(),
            modified_nanos,
        })
    }
}

fn open_regular_no_follow(
    path: &std::path::Path,
    writable: bool,
) -> std::io::Result<std::fs::File> {
    let before = std::fs::symlink_metadata(path)?;
    if !metadata_is_safe_regular(&before) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsafe non-regular or reparse file: {}", path.display()),
        ));
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(writable);
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
    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !metadata_is_safe_regular(&opened) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("opened unsafe file: {}", path.display()),
        ));
    }
    let identity = file_identity(&file, &opened)?;
    ensure_path_identity(path, &identity)?;
    Ok(file)
}

fn ensure_path_identity(
    path: &std::path::Path,
    expected: &AgentsFileIdentity,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata_is_safe_regular(&metadata)
            || metadata.dev() != expected.device
            || metadata.ino() != expected.inode
        {
            return Err(std::io::Error::other(format!(
                "file identity changed: {}",
                path.display()
            )));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata_is_safe_regular(&metadata) || &windows_file_identity(&file)? != expected {
            return Err(std::io::Error::other(format!(
                "file identity changed: {}",
                path.display()
            )));
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata_is_safe_regular(&metadata) || &file_identity(&file, &metadata)? != expected {
            return Err(std::io::Error::other(format!(
                "file identity changed: {}",
                path.display()
            )));
        }
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn read_regular_record(
    path: &std::path::Path,
    max_bytes: u64,
) -> std::io::Result<(Vec<u8>, AgentsFileRecord)> {
    use std::io::Read;

    let mut file = open_regular_no_follow(path, false)?;
    let before = file.metadata()?;
    if before.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds size limit: {}", path.display()),
        ));
    }
    let identity = file_identity(&file, &before)?;
    let snapshot = metadata_snapshot(&before);
    let mut bytes = Vec::with_capacity(before.len().min(max_bytes) as usize);
    std::io::Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file grew beyond size limit: {}", path.display()),
        ));
    }
    let after = file.metadata()?;
    if metadata_snapshot(&after) != snapshot || after.len() != bytes.len() as u64 {
        return Err(std::io::Error::other(format!(
            "file changed while being read: {}",
            path.display()
        )));
    }
    ensure_path_identity(path, &identity)?;
    let record = AgentsFileRecord {
        identity,
        length: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
    };
    Ok((bytes, record))
}

fn record_matches_plan(record: &AgentsFileRecord, length: u64, sha256: &str) -> bool {
    record.length == length && record.sha256 == sha256
}

fn create_private_new(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn agents_rename_noreplace(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS) | Some(libc::EINVAL)
        ) {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "filesystem does not support atomic no-clobber rename",
            ))
        } else {
            Err(error)
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn agents_rename_noreplace(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path"))?;
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn agents_rename_noreplace(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    crate::services::file_ops::rename_noreplace(source, destination)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
)))]
fn agents_rename_noreplace(
    _source: &std::path::Path,
    _destination: &std::path::Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-clobber rename is unavailable",
    ))
}

fn sync_agents_parent(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path.parent().unwrap_or_else(|| std::path::Path::new(".")))?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn rename_agents_noreplace_synced(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    agents_rename_noreplace(source, destination)?;
    sync_agents_parent(destination)
}

fn control_path_absent(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if !metadata_is_safe_regular(&metadata) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsafe control path: {}", path.display()),
        )),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("control path already exists: {}", path.display()),
        )),
    }
}

fn default_agents_lock_root() -> Option<std::path::PathBuf> {
    #[cfg(not(test))]
    {
        dirs::home_dir().map(|home| home.join(".cokacdir").join(LOCK_DIRECTORY))
    }
    #[cfg(test)]
    {
        Some(std::env::temp_dir().join(format!(
            "cokacdir-{}-tests-{}",
            LOCK_DIRECTORY,
            std::process::id()
        )))
    }
}

fn agents_lock_name(dir: &std::path::Path) -> std::io::Result<std::ffi::OsString> {
    let canonical = dir.canonicalize()?;
    if !canonical.metadata()?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("OpenCode workspace is not a directory: {}", dir.display()),
        ));
    }
    let (namespace, object) =
        crate::services::file_ops::stable_path_identity(&canonical)?.components();
    Ok(format!("agents-{namespace:016x}-{}.lock", hex::encode(object)).into())
}

fn open_agents_lock_directory(
    root: &std::path::Path,
) -> std::io::Result<(std::fs::File, crate::services::file_ops::DirectoryAccess)> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "OpenCode lock root is not a real directory: {}",
                    root.display()
                ),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(root)?;
        }
        Err(error) => return Err(error),
    }
    let (directory, access, metadata) = crate::services::file_ops::open_directory_for_read(root)?;
    let identity = crate::services::file_ops::stable_file_identity(&directory)?;
    if !metadata.is_dir() || identity != crate::services::file_ops::stable_path_identity(root)? {
        return Err(std::io::Error::other(format!(
            "OpenCode lock root changed while being opened: {}",
            root.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    if crate::services::file_ops::stable_file_identity(&directory)? != identity
        || crate::services::file_ops::stable_path_identity(root)? != identity
    {
        return Err(std::io::Error::other(format!(
            "OpenCode lock root changed while it was being secured: {}",
            root.display()
        )));
    }
    Ok((directory, access))
}

fn try_acquire_lock_at(
    dir: &std::path::Path,
    lock_root: &std::path::Path,
) -> Option<std::fs::File> {
    let lock_name = match agents_lock_name(dir) {
        Ok(name) => name,
        Err(error) => {
            opencode_debug(&format!("[agents lock] cannot identify workspace: {error}"));
            return None;
        }
    };
    let (directory, access) = match open_agents_lock_directory(lock_root) {
        Ok(opened) => opened,
        Err(error) => {
            opencode_debug(&format!(
                "[agents lock] cannot open private lock root: {error}"
            ));
            return None;
        }
    };
    let file = match access.open_file(
        &lock_name,
        crate::services::file_ops::DirectoryFileOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .pin_name(true)
            .mode(0o600),
    ) {
        Ok(file) => file,
        Err(error) => {
            opencode_debug(&format!("[agents lock] cannot safely open lock: {error}"));
            return None;
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) if metadata_is_safe_regular(&metadata) => metadata,
        Ok(_) => {
            opencode_debug("[agents lock] refusing non-regular private lock file");
            return None;
        }
        Err(error) => {
            opencode_debug(&format!(
                "[agents lock] cannot inspect private lock: {error}"
            ));
            return None;
        }
    };
    let identity = match crate::services::file_ops::stable_file_identity(&file) {
        Ok(identity) => identity,
        Err(error) => {
            opencode_debug(&format!(
                "[agents lock] cannot identify private lock: {error}"
            ));
            return None;
        }
    };
    if access.child_identity(&lock_name).ok() != Some(identity) {
        opencode_debug("[agents lock] private lock path changed while being opened");
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
            opencode_debug(&format!(
                "[agents lock] cannot secure private lock: {error}"
            ));
            return None;
        }
    }
    let _ = metadata;
    if let Err(error) = file.sync_all().and_then(|_| directory.sync_all()) {
        opencode_debug(&format!("[agents lock] cannot sync private lock: {error}"));
        return None;
    }

    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
        Err(error) => {
            opencode_debug(&format!("[agents lock] lock failed: {error}"));
            None
        }
    }
}

fn try_acquire_lock(dir: &std::path::Path) -> Option<std::fs::File> {
    let root = default_agents_lock_root()?;
    try_acquire_lock_at(dir, &root)
}

fn valid_txn_id(txn: &str) -> bool {
    txn.len() == 32 && txn.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn expected_txn_names(txn: &str, has_original: bool) -> (String, Option<String>, String, String) {
    (
        format!("{STAGED_PREFIX}{txn}"),
        has_original.then(|| format!("{BACKUP_PREFIX}{txn}")),
        format!("{RETIRED_PREFIX}{txn}"),
        format!("{STATE_DONE_PREFIX}{txn}"),
    )
}

fn transaction_state_name(txn: &str) -> String {
    format!("{STATE_PREFIX}{txn}")
}

fn transaction_state_staging_name(txn: &str) -> String {
    format!("{STATE_STAGING_PREFIX}{txn}")
}

fn validate_plan(plan: &AgentsTxnPlan) -> std::io::Result<()> {
    if plan.version != TXN_VERSION || plan.phase != "plan" || !valid_txn_id(&plan.txn) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid AGENTS transaction plan",
        ));
    }
    if plan.injected_length > MAX_INJECTED_AGENTS_BYTES
        || plan.injected_sha256.len() != 64
        || !plan
            .injected_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid injected file record",
        ));
    }
    if let Some(original) = &plan.original {
        if original.length > MAX_ORIGINAL_AGENTS_BYTES
            || original.sha256.len() != 64
            || !original.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid original file record",
            ));
        }
    }
    let (staged, backup, retired, _) = expected_txn_names(&plan.txn, plan.original.is_some());
    if plan.staged_name != staged || plan.backup_name != backup || plan.retired_name != retired {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "transaction contains unsafe control names",
        ));
    }
    Ok(())
}

fn parse_state_at(path: &std::path::Path) -> std::io::Result<ParsedAgentsState> {
    let (bytes, marker_record) = read_regular_record(path, MAX_STATE_BYTES)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "state marker is not UTF-8")
    })?;
    let lines: Vec<&str> = text.lines().collect();
    if !(lines.len() == 1 || lines.len() == 2) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "state marker has an invalid number of records",
        ));
    }
    let plan: AgentsTxnPlan = serde_json::from_str(lines[0]).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid state plan: {error}"),
        )
    })?;
    validate_plan(&plan)?;
    let ready = if lines.len() == 2 {
        let ready: AgentsTxnReady = serde_json::from_str(lines[1]).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid ready record: {error}"),
            )
        })?;
        if ready.version != TXN_VERSION
            || ready.phase != "ready"
            || ready.txn != plan.txn
            || !record_matches_plan(&ready.injected, plan.injected_length, &plan.injected_sha256)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ready record does not match transaction plan",
            ));
        }
        Some(ready)
    } else {
        None
    };
    Ok(ParsedAgentsState {
        plan,
        ready,
        marker_record,
    })
}

fn path_record_if_present(
    path: &std::path::Path,
    max_bytes: u64,
) -> std::io::Result<Option<AgentsFileRecord>> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
        Ok(metadata) if !metadata_is_safe_regular(&metadata) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsafe transaction path: {}", path.display()),
        )),
        Ok(_) => read_regular_record(path, max_bytes).map(|(_, record)| Some(record)),
    }
}

fn move_owned_file(
    source: &std::path::Path,
    destination: &std::path::Path,
    expected: &AgentsFileRecord,
) -> std::io::Result<()> {
    let current = path_record_if_present(source, expected.length.max(1))?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("owned file disappeared: {}", source.display()),
        )
    })?;
    if &current != expected {
        return Err(std::io::Error::other(format!(
            "owned file changed before move: {}",
            source.display()
        )));
    }
    rename_agents_noreplace_synced(source, destination)?;
    let moved = path_record_if_present(destination, expected.length.max(1))?.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "moved file disappeared")
    })?;
    if &moved != expected {
        // Never overwrite a path while attempting rollback. If AGENTS.md was
        // concurrently recreated, the changed file remains quarantined.
        let _ = rename_agents_noreplace_synced(destination, source);
        return Err(std::io::Error::other(format!(
            "owned file changed during move: {}",
            source.display()
        )));
    }
    Ok(())
}

fn remove_quarantined_file(
    path: &std::path::Path,
    expected: &AgentsFileRecord,
) -> std::io::Result<()> {
    use std::io::Read;

    // Keep the verified read handle alive while opening a DELETE-capable
    // no-follow handle for the same object. The final disposition is then
    // handle-bound on Windows; a pathname replacement cannot be unlinked.
    let (mut file, before) = crate::services::file_ops::open_regular_file_no_follow(path)?;
    let limit = expected.length.max(1);
    if before.len() > limit {
        return Err(std::io::Error::other(format!(
            "quarantined file changed: {}",
            path.display()
        )));
    }
    let legacy_identity = file_identity(&file, &before)?;
    let snapshot = metadata_snapshot(&before);
    let mut bytes = Vec::with_capacity(before.len().min(limit) as usize);
    std::io::Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let current = AgentsFileRecord {
        identity: legacy_identity.clone(),
        length: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
    };
    if bytes.len() as u64 > limit || metadata_snapshot(&after) != snapshot || current != *expected {
        return Err(std::io::Error::other(format!(
            "quarantined file changed: {}",
            path.display()
        )));
    }
    ensure_path_identity(path, &legacy_identity)?;
    let stable_identity = crate::services::file_ops::stable_file_identity(&file)?;
    let deletion = crate::services::file_ops::prepare_file_deletion(path, stable_identity)?;
    drop(file);
    deletion.delete()?;
    sync_agents_parent(path)
}

fn cleanup_state_marker(dir: &std::path::Path, state: &ParsedAgentsState) -> std::io::Result<()> {
    let state_path = dir.join(transaction_state_name(&state.plan.txn));
    let (_, _, _, done_name) = expected_txn_names(&state.plan.txn, state.plan.original.is_some());
    let done_path = dir.join(done_name);
    move_owned_file(&state_path, &done_path, &state.marker_record)?;
    remove_quarantined_file(&done_path, &state.marker_record)
}

fn cleanup_staged_file(
    dir: &std::path::Path,
    state: &ParsedAgentsState,
    expected: &AgentsFileRecord,
) -> std::io::Result<()> {
    let staged_path = dir.join(&state.plan.staged_name);
    let retired_path = dir.join(&state.plan.retired_name);
    match path_record_if_present(&staged_path, MAX_INJECTED_AGENTS_BYTES)? {
        None => Ok(()),
        Some(record) if &record == expected => {
            move_owned_file(&staged_path, &retired_path, expected)?;
            remove_quarantined_file(&retired_path, expected)
        }
        Some(_) => Err(std::io::Error::other(
            "staged AGENTS transaction file changed; preserving it",
        )),
    }
}

fn finish_ready_transaction(
    dir: &std::path::Path,
    state: &ParsedAgentsState,
) -> std::io::Result<()> {
    let ready = state.ready.as_ref().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "transaction is not ready")
    })?;
    let agents_path = dir.join(AGENTS_MD);
    let staged_path = dir.join(&state.plan.staged_name);
    let backup_path = state.plan.backup_name.as_ref().map(|name| dir.join(name));
    let retired_path = dir.join(&state.plan.retired_name);

    let agents = path_record_if_present(&agents_path, MAX_INJECTED_AGENTS_BYTES)?;
    let staged = path_record_if_present(&staged_path, MAX_INJECTED_AGENTS_BYTES)?;
    let retired = path_record_if_present(&retired_path, MAX_INJECTED_AGENTS_BYTES)?;
    let backup = match &backup_path {
        Some(path) => path_record_if_present(path, MAX_ORIGINAL_AGENTS_BYTES)?,
        None => None,
    };

    if let Some(record) = &staged {
        if record != &ready.injected {
            return Err(std::io::Error::other(
                "staged transaction file changed; preserving transaction",
            ));
        }
    }
    if let Some(record) = &retired {
        if record != &ready.injected {
            return Err(std::io::Error::other(
                "retired transaction file changed; preserving transaction",
            ));
        }
    }
    match (&state.plan.original, &backup) {
        (Some(expected), Some(actual)) if expected != actual => {
            return Err(std::io::Error::other(
                "original quarantine changed; preserving transaction",
            ));
        }
        (None, Some(_)) => {
            return Err(std::io::Error::other(
                "unexpected original quarantine; preserving transaction",
            ));
        }
        _ => {}
    }

    // If AGENTS is ours, first quarantine it. A rename followed by a second
    // identity/hash check closes the verification-to-delete window without
    // ever overwriting a concurrently created user file.
    if agents.as_ref() == Some(&ready.injected) {
        if retired.is_some() {
            return Err(std::io::Error::other(
                "retired path collision; preserving transaction",
            ));
        }
        move_owned_file(&agents_path, &retired_path, &ready.injected)?;
    } else if let Some(record) = &agents {
        if state.plan.original.as_ref() == Some(record) && backup.is_none() {
            if retired.is_none() {
                // Crash before the original was quarantined. No user pathname
                // was changed, so only our staged file needs cleanup.
                cleanup_staged_file(dir, state, &ready.injected)?;
                return cleanup_state_marker(dir, state);
            }
            if retired.as_ref() == Some(&ready.injected) && staged.is_none() {
                // Restoration was already committed, then the process died
                // before removing the retired injected inode and state.
                remove_quarantined_file(&retired_path, &ready.injected)?;
                return cleanup_state_marker(dir, state);
            }
        }
        return Err(std::io::Error::other(
            "AGENTS.md was changed by the user; preserving it and recovery files",
        ));
    } else if retired.as_ref() != Some(&ready.injected) {
        if staged.as_ref() == Some(&ready.injected) {
            // The ready record was durable but publication had not happened:
            // the injected inode is still at its private staged name. This is
            // the only AGENTS-absent state in which restoring an original is
            // safe without first observing the injected identity at AGENTS.
            if let (Some(original), Some(backup_path)) =
                (&state.plan.original, backup_path.as_ref())
            {
                if backup.as_ref() != Some(original) {
                    return Err(std::io::Error::other(
                        "pre-publish original quarantine is missing or changed",
                    ));
                }
                move_owned_file(backup_path, &agents_path, original)?;
            }
            cleanup_staged_file(dir, state, &ready.injected)?;
            return cleanup_state_marker(dir, state);
        }
        // A published file that disappeared may have been deliberately
        // deleted by the user. Absence alone is never permission to restore
        // an original or to discard the recovery marker.
        return Err(std::io::Error::other(
            "AGENTS.md disappeared after publication; preserving recovery files",
        ));
    }

    let retired_now = path_record_if_present(&retired_path, MAX_INJECTED_AGENTS_BYTES)?;
    match &state.plan.original {
        Some(original) => {
            let backup_path = backup_path.as_ref().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "original transaction is missing its backup name",
                )
            })?;
            let backup_now = path_record_if_present(backup_path, MAX_ORIGINAL_AGENTS_BYTES)?;
            if backup_now.as_ref() != Some(original) {
                return Err(std::io::Error::other(
                    "original quarantine is missing or changed; preserving transaction",
                ));
            }
            if path_record_if_present(&agents_path, MAX_INJECTED_AGENTS_BYTES)?.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "AGENTS.md was recreated; refusing to overwrite it",
                ));
            }
            move_owned_file(backup_path, &agents_path, original)?;
        }
        None => {
            if path_record_if_present(&agents_path, MAX_INJECTED_AGENTS_BYTES)?.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "AGENTS.md was recreated; refusing to delete it",
                ));
            }
        }
    }

    if retired_now.as_ref() == Some(&ready.injected) {
        remove_quarantined_file(&retired_path, &ready.injected)?;
    } else if retired_now.is_some() {
        return Err(std::io::Error::other(
            "retired injected file changed; preserving transaction",
        ));
    }
    cleanup_staged_file(dir, state, &ready.injected)?;
    cleanup_state_marker(dir, state)
}

fn finish_plan_only_transaction(
    dir: &std::path::Path,
    state: &ParsedAgentsState,
) -> std::io::Result<()> {
    let agents_path = dir.join(AGENTS_MD);
    let current = path_record_if_present(&agents_path, MAX_ORIGINAL_AGENTS_BYTES)?;
    if current != state.plan.original {
        return Err(std::io::Error::other(
            "AGENTS.md changed during transaction preparation; preserving state",
        ));
    }
    if let Some(backup_name) = &state.plan.backup_name {
        control_path_absent(&dir.join(backup_name))?;
    }
    control_path_absent(&dir.join(&state.plan.retired_name))?;

    let staged_path = dir.join(&state.plan.staged_name);
    if let Some(staged) = path_record_if_present(&staged_path, MAX_INJECTED_AGENTS_BYTES)? {
        if !record_matches_plan(
            &staged,
            state.plan.injected_length,
            &state.plan.injected_sha256,
        ) {
            return Err(std::io::Error::other(
                "uncommitted staged file changed; preserving state",
            ));
        }
        let retired_path = dir.join(&state.plan.retired_name);
        move_owned_file(&staged_path, &retired_path, &staged)?;
        remove_quarantined_file(&retired_path, &staged)?;
    }
    cleanup_state_marker(dir, state)
}

fn recover_done_markers(dir: &std::path::Path) -> std::io::Result<()> {
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(STATE_DONE_PREFIX) {
            continue;
        }
        let state = parse_state_at(&entry.path())?;
        let (_, backup, retired, done) =
            expected_txn_names(&state.plan.txn, state.plan.original.is_some());
        if name != done || state.ready.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid completed transaction marker",
            ));
        }
        control_path_absent(&dir.join(&state.plan.staged_name))?;
        if let Some(backup) = backup {
            control_path_absent(&dir.join(backup))?;
        }
        control_path_absent(&dir.join(retired))?;
        remove_quarantined_file(&entry.path(), &state.marker_record)?;
    }
    Ok(())
}

fn reject_legacy_controls(dir: &std::path::Path) -> std::io::Result<()> {
    for name in [
        LEGACY_BACKUP_FILE,
        LEGACY_NO_ORIGINAL_SENTINEL,
        LEGACY_TMP_FILE,
    ] {
        match std::fs::symlink_metadata(dir.join(name)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("legacy AGENTS recovery marker requires manual review: {name}"),
                ));
            }
        }
    }

    Ok(())
}

fn find_transaction_states(dir: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut states = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(STATE_PREFIX)
        {
            states.push(entry.path());
        }
    }
    Ok(states)
}

fn validate_unpublished_state_stages(dir: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(STATE_STAGING_PREFIX) {
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if !metadata_is_safe_regular(&metadata) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unsafe unpublished AGENTS transaction state: {name}"),
                ));
            }
            // This file was never published as an active marker, so the v2
            // protocol guarantees that AGENTS.md was not touched. Preserve a
            // partial write for diagnosis but do not let it block a new txn.
        }
    }
    Ok(())
}

fn reject_orphaned_dynamic_controls(dir: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.starts_with(STAGED_PREFIX)
            || name.starts_with(BACKUP_PREFIX)
            || name.starts_with(RETIRED_PREFIX)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("orphaned AGENTS transaction control requires review: {name}"),
            ));
        }
    }
    Ok(())
}

fn recover_agents_md_if_needed(
    dir: &std::path::Path,
    expected_txn: Option<&str>,
) -> std::io::Result<()> {
    reject_legacy_controls(dir)?;
    recover_done_markers(dir)?;
    validate_unpublished_state_stages(dir)?;
    let mut states = find_transaction_states(dir)?;
    if states.is_empty() {
        reject_orphaned_dynamic_controls(dir)?;
        return Ok(());
    }
    if states.len() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "multiple AGENTS transaction states require manual review",
        ));
    }
    let state_path = states.remove(0);
    let metadata = std::fs::symlink_metadata(&state_path)?;
    if !metadata_is_safe_regular(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "AGENTS state is a symlink/reparse/non-regular file",
        ));
    }
    let state = parse_state_at(&state_path)?;
    let expected_state_name = transaction_state_name(&state.plan.txn);
    if state_path.file_name() != Some(std::ffi::OsStr::new(&expected_state_name)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "transaction state filename does not match its transaction id",
        ));
    }
    if expected_txn.is_some_and(|txn| txn != state.plan.txn) {
        return Err(std::io::Error::other(
            "AGENTS transaction marker was replaced; preserving all files",
        ));
    }
    if state.ready.is_some() {
        finish_ready_transaction(dir, &state)
    } else {
        finish_plan_only_transaction(dir, &state)
    }
}

struct AgentsMdGuard {
    dir: std::path::PathBuf,
    txn: String,
    restore_on_drop: bool,
    _lock_file: std::fs::File,
}

impl Drop for AgentsMdGuard {
    fn drop(&mut self) {
        if self.restore_on_drop {
            if let Err(error) = recover_agents_md_if_needed(&self.dir, Some(&self.txn)) {
                opencode_debug(&format!(
                    "[AgentsMdGuard] restore stopped safely; recovery files preserved: {error}"
                ));
            }
        }
    }
}

#[cfg(test)]
impl AgentsMdGuard {
    fn simulate_crash(mut self) {
        self.restore_on_drop = false;
    }
}

fn inspect_original_agents(
    agents_path: &std::path::Path,
) -> std::io::Result<Option<(String, AgentsFileRecord)>> {
    match std::fs::symlink_metadata(agents_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
        Ok(metadata) if !metadata_is_safe_regular(&metadata) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "AGENTS.md must be an ordinary non-reparse file",
        )),
        Ok(_) => {
            let (bytes, record) = read_regular_record(agents_path, MAX_ORIGINAL_AGENTS_BYTES)?;
            let text = String::from_utf8(bytes).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "AGENTS.md is not UTF-8")
            })?;
            Ok(Some((text, record)))
        }
    }
}

fn inject_system_prompt_into_agents_md_impl(
    working_dir: &str,
    system_prompt: &str,
    forced_txn: Option<&str>,
) -> Option<AgentsMdGuard> {
    let dir = std::path::Path::new(working_dir);
    let agents_path = dir.join(AGENTS_MD);
    let lock_file = try_acquire_lock(dir)?;
    if let Err(error) = recover_agents_md_if_needed(dir, None) {
        opencode_debug(&format!(
            "[inject_agents_md] recovery stopped safely; files preserved: {error}"
        ));
        return None;
    }

    let original = match inspect_original_agents(&agents_path) {
        Ok(original) => original,
        Err(error) => {
            opencode_debug(&format!("[inject_agents_md] unsafe AGENTS.md: {error}"));
            return None;
        }
    };
    let injected = if let Some((original_text, _)) = &original {
        format!("{}\n\n{}\n", system_prompt, original_text.trim()).into_bytes()
    } else {
        format!("{system_prompt}\n").into_bytes()
    };
    if injected.len() as u64 > MAX_INJECTED_AGENTS_BYTES {
        opencode_debug("[inject_agents_md] injected AGENTS.md exceeds size limit");
        return None;
    }

    let txn = forced_txn
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{:032x}", rand::random::<u128>()));
    if !valid_txn_id(&txn) {
        opencode_debug("[inject_agents_md] invalid transaction id");
        return None;
    }
    let (staged_name, backup_name, retired_name, done_name) =
        expected_txn_names(&txn, original.is_some());
    let state_name = transaction_state_name(&txn);
    let state_staging_name = transaction_state_staging_name(&txn);
    for path in [
        Some(dir.join(&state_name)),
        Some(dir.join(&state_staging_name)),
        Some(dir.join(&staged_name)),
        backup_name.as_ref().map(|name| dir.join(name)),
        Some(dir.join(&retired_name)),
        Some(dir.join(&done_name)),
    ]
    .into_iter()
    .flatten()
    {
        if let Err(error) = control_path_absent(&path) {
            opencode_debug(&format!("[inject_agents_md] control collision: {error}"));
            return None;
        }
    }

    let plan = AgentsTxnPlan {
        version: TXN_VERSION,
        phase: "plan".to_string(),
        txn: txn.clone(),
        original: original.as_ref().map(|(_, record)| record.clone()),
        injected_length: injected.len() as u64,
        injected_sha256: sha256_hex(&injected),
        backup_name: backup_name.clone(),
        staged_name: staged_name.clone(),
        retired_name: retired_name.clone(),
    };
    let state_path = dir.join(state_name);
    let state_staging_path = dir.join(state_staging_name);
    let mut state_file = match create_private_new(&state_staging_path) {
        Ok(file) => file,
        Err(error) => {
            opencode_debug(&format!(
                "[inject_agents_md] cannot create staged state: {error}"
            ));
            return None;
        }
    };
    let plan_line = match serde_json::to_vec(&plan) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            bytes
        }
        Err(error) => {
            opencode_debug(&format!("[inject_agents_md] cannot encode state: {error}"));
            return None;
        }
    };
    if let Err(error) = state_file
        .write_all(&plan_line)
        .and_then(|_| state_file.sync_all())
        .and_then(|_| sync_agents_parent(&state_staging_path))
    {
        opencode_debug(&format!("[inject_agents_md] cannot persist plan: {error}"));
        return None;
    }
    if let Err(error) = rename_agents_noreplace_synced(&state_staging_path, &state_path) {
        opencode_debug(&format!(
            "[inject_agents_md] cannot publish transaction state: {error}"
        ));
        return None;
    }

    let staged_path = dir.join(&staged_name);
    let mut staged_file = match create_private_new(&staged_path) {
        Ok(file) => file,
        Err(error) => {
            opencode_debug(&format!(
                "[inject_agents_md] cannot stage AGENTS.md: {error}"
            ));
            let _ = recover_agents_md_if_needed(dir, Some(&txn));
            return None;
        }
    };
    if let Err(error) = staged_file
        .write_all(&injected)
        .and_then(|_| staged_file.sync_all())
        .and_then(|_| sync_agents_parent(&staged_path))
    {
        opencode_debug(&format!(
            "[inject_agents_md] cannot persist staged file: {error}"
        ));
        let _ = recover_agents_md_if_needed(dir, Some(&txn));
        return None;
    }
    let staged_metadata = match staged_file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            opencode_debug(&format!(
                "[inject_agents_md] cannot stat staged file: {error}"
            ));
            return None;
        }
    };
    let injected_record = match file_identity(&staged_file, &staged_metadata) {
        Ok(identity) => AgentsFileRecord {
            identity,
            length: injected.len() as u64,
            sha256: plan.injected_sha256.clone(),
        },
        Err(error) => {
            opencode_debug(&format!(
                "[inject_agents_md] cannot identify staged file: {error}"
            ));
            return None;
        }
    };
    if let Err(error) = ensure_path_identity(&staged_path, &injected_record.identity) {
        opencode_debug(&format!("[inject_agents_md] staged path changed: {error}"));
        return None;
    }

    let ready = AgentsTxnReady {
        version: TXN_VERSION,
        phase: "ready".to_string(),
        txn: txn.clone(),
        injected: injected_record.clone(),
    };
    let ready_line = match serde_json::to_vec(&ready) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            bytes
        }
        Err(error) => {
            opencode_debug(&format!(
                "[inject_agents_md] cannot encode ready state: {error}"
            ));
            return None;
        }
    };
    let state_identity = match state_file
        .metadata()
        .and_then(|metadata| file_identity(&state_file, &metadata))
    {
        Ok(identity) => identity,
        Err(error) => {
            opencode_debug(&format!(
                "[inject_agents_md] cannot identify state: {error}"
            ));
            return None;
        }
    };
    if let Err(error) = ensure_path_identity(&state_path, &state_identity)
        .and_then(|_| state_file.write_all(&ready_line))
        .and_then(|_| state_file.sync_all())
        .and_then(|_| sync_agents_parent(&state_path))
        .and_then(|_| ensure_path_identity(&state_path, &state_identity))
    {
        opencode_debug(&format!(
            "[inject_agents_md] cannot commit ready state: {error}"
        ));
        return None;
    }
    drop(staged_file);
    drop(state_file);

    if let Some((_, original_record)) = &original {
        let current = match path_record_if_present(&agents_path, MAX_ORIGINAL_AGENTS_BYTES) {
            Ok(Some(record)) if &record == original_record => record,
            Ok(_) => {
                opencode_debug("[inject_agents_md] original changed before quarantine");
                let _ = recover_agents_md_if_needed(dir, Some(&txn));
                return None;
            }
            Err(error) => {
                opencode_debug(&format!(
                    "[inject_agents_md] cannot recheck original: {error}"
                ));
                return None;
            }
        };
        let Some(backup_name) = backup_name.as_ref() else {
            opencode_debug("[inject_agents_md] original transaction has no backup name");
            return None;
        };
        let backup_path = dir.join(backup_name);
        if let Err(error) = move_owned_file(&agents_path, &backup_path, &current) {
            opencode_debug(&format!("[inject_agents_md] quarantine failed: {error}"));
            let _ = recover_agents_md_if_needed(dir, Some(&txn));
            return None;
        }
    } else {
        match std::fs::symlink_metadata(&agents_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                opencode_debug("[inject_agents_md] AGENTS.md appeared before publish");
                let _ = recover_agents_md_if_needed(dir, Some(&txn));
                return None;
            }
            Err(error) => {
                opencode_debug(&format!(
                    "[inject_agents_md] cannot confirm AGENTS.md absence: {error}"
                ));
                return None;
            }
        }
    }

    if let Err(error) = move_owned_file(&staged_path, &agents_path, &injected_record) {
        opencode_debug(&format!("[inject_agents_md] publish failed: {error}"));
        let _ = recover_agents_md_if_needed(dir, Some(&txn));
        return None;
    }

    Some(AgentsMdGuard {
        dir: dir.to_path_buf(),
        txn,
        restore_on_drop: true,
        _lock_file: lock_file,
    })
}

fn inject_system_prompt_into_agents_md(
    working_dir: &str,
    system_prompt: &str,
) -> Result<AgentsMdGuard, String> {
    inject_system_prompt_into_agents_md_impl(working_dir, system_prompt, None).ok_or_else(|| {
        format!(
            "Failed to safely inject requested system prompt into {}",
            std::path::Path::new(working_dir).join(AGENTS_MD).display()
        )
    })
}

/// Prepare project instructions before either OpenCode execution path starts.
/// `None` means that no system prompt was requested; every requested prompt,
/// including an empty string, must produce a live restoration guard or fail.
fn prepare_requested_system_prompt(
    working_dir: &str,
    system_prompt: Option<&str>,
) -> Result<Option<AgentsMdGuard>, String> {
    system_prompt
        .map(|prompt| inject_system_prompt_into_agents_md(working_dir, prompt))
        .transpose()
}

#[cfg(test)]
mod agents_md_lock_tests {
    use super::*;

    fn path(dir: &tempfile::TempDir) -> &str {
        dir.path().to_str().expect("UTF-8 temp path")
    }

    fn transaction_artifact(dir: &tempfile::TempDir, prefix: &str) -> std::path::PathBuf {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .expect("transaction artifact")
    }

    #[test]
    fn normal_original_is_restored_exactly_and_lock_contents_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        std::fs::write(&agents, "  user instructions\n\n").unwrap();
        std::fs::write(
            dir.path().join(LEGACY_WORKSPACE_LOCK_FILE),
            b"do not truncate this lock\n",
        )
        .unwrap();

        let guard = inject_system_prompt_into_agents_md(path(&dir), "system prompt").unwrap();
        assert!(std::fs::read_to_string(&agents)
            .unwrap()
            .starts_with("system prompt"));
        let state_path = transaction_artifact(&dir, STATE_PREFIX);
        let state = parse_state_at(&state_path).unwrap();
        assert_eq!(state.plan.txn, guard.txn);
        assert_eq!(state.plan.original.as_ref().unwrap().sha256.len(), 64);
        assert_eq!(state.plan.injected_sha256.len(), 64);
        assert!(state.plan.backup_name.is_some());
        assert!(state.ready.is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(state_path).unwrap().permissions().mode() & 0o077,
                0
            );
            assert_eq!(
                std::fs::metadata(&agents).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        assert!(inject_system_prompt_into_agents_md(path(&dir), "contender").is_err());
        drop(guard);

        assert_eq!(std::fs::read(&agents).unwrap(), b"  user instructions\n\n");
        assert_eq!(
            std::fs::read(dir.path().join(LEGACY_WORKSPACE_LOCK_FILE)).unwrap(),
            b"do not truncate this lock\n"
        );
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(STATE_PREFIX)));
    }

    #[test]
    fn normal_absent_original_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let guard = inject_system_prompt_into_agents_md(path(&dir), "system prompt").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(AGENTS_MD)).unwrap(),
            "system prompt\n"
        );
        drop(guard);
        assert!(!dir.path().join(AGENTS_MD).exists());
        assert!(!dir.path().join(LEGACY_WORKSPACE_LOCK_FILE).exists());
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(STATE_PREFIX)));
    }

    #[test]
    fn active_user_edit_with_original_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        std::fs::write(&agents, "original\n").unwrap();
        let guard = inject_system_prompt_into_agents_md(path(&dir), "prompt").unwrap();
        std::fs::write(&agents, "user edit during run\n").unwrap();
        drop(guard);

        assert_eq!(
            std::fs::read_to_string(&agents).unwrap(),
            "user edit during run\n"
        );
        assert!(transaction_artifact(&dir, STATE_PREFIX).exists());
        assert_eq!(
            std::fs::read_to_string(transaction_artifact(&dir, BACKUP_PREFIX)).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn active_user_edit_without_original_is_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        let guard = inject_system_prompt_into_agents_md(path(&dir), "prompt").unwrap();
        std::fs::write(&agents, "user edit during run\n").unwrap();
        drop(guard);
        assert_eq!(
            std::fs::read_to_string(&agents).unwrap(),
            "user edit during run\n"
        );
        assert!(transaction_artifact(&dir, STATE_PREFIX).exists());
    }

    #[test]
    fn active_user_deletion_with_original_is_not_undone() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        std::fs::write(&agents, "original\n").unwrap();
        let guard = inject_system_prompt_into_agents_md(path(&dir), "prompt").unwrap();
        std::fs::remove_file(&agents).unwrap();
        drop(guard);

        assert!(!agents.exists());
        assert!(transaction_artifact(&dir, STATE_PREFIX).exists());
        assert_eq!(
            std::fs::read_to_string(transaction_artifact(&dir, BACKUP_PREFIX)).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn active_user_deletion_without_original_preserves_state() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        let guard = inject_system_prompt_into_agents_md(path(&dir), "prompt").unwrap();
        std::fs::remove_file(&agents).unwrap();
        drop(guard);

        assert!(!agents.exists());
        assert!(transaction_artifact(&dir, STATE_PREFIX).exists());
    }

    #[test]
    fn crash_recovery_restores_unchanged_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        std::fs::write(&agents, "original\n").unwrap();
        inject_system_prompt_into_agents_md(path(&dir), "first")
            .unwrap()
            .simulate_crash();
        assert!(std::fs::read_to_string(&agents)
            .unwrap()
            .starts_with("first"));

        let next = inject_system_prompt_into_agents_md(path(&dir), "second").unwrap();
        assert!(std::fs::read_to_string(&agents)
            .unwrap()
            .starts_with("second"));
        drop(next);
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), "original\n");
    }

    #[test]
    fn crash_after_original_restore_finishes_retired_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        std::fs::write(&agents, "original\n").unwrap();
        let guard = inject_system_prompt_into_agents_md(path(&dir), "first").unwrap();
        let (_, backup_name, retired_name, _) = expected_txn_names(&guard.txn, true);
        let backup = dir.path().join(backup_name.unwrap());
        let retired = dir.path().join(retired_name);

        rename_agents_noreplace_synced(&agents, &retired).unwrap();
        rename_agents_noreplace_synced(&backup, &agents).unwrap();
        guard.simulate_crash();

        let next = inject_system_prompt_into_agents_md(path(&dir), "second").unwrap();
        assert!(!retired.exists());
        drop(next);
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), "original\n");
    }

    #[test]
    fn partial_unpublished_state_does_not_block_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join(transaction_state_staging_name(
            "0123456789abcdef0123456789abcdef",
        ));
        std::fs::write(&partial, b"{partial json").unwrap();

        let guard = inject_system_prompt_into_agents_md(path(&dir), "prompt").unwrap();
        drop(guard);
        assert!(partial.exists(), "untrusted partial control is preserved");
        assert!(!dir.path().join(AGENTS_MD).exists());
    }

    #[test]
    fn crash_recovery_preserves_changed_agents_and_backup() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        std::fs::write(&agents, "original\n").unwrap();
        inject_system_prompt_into_agents_md(path(&dir), "first")
            .unwrap()
            .simulate_crash();
        std::fs::write(&agents, "changed after crash\n").unwrap();

        assert!(inject_system_prompt_into_agents_md(path(&dir), "second").is_err());
        assert_eq!(
            std::fs::read_to_string(&agents).unwrap(),
            "changed after crash\n"
        );
        assert!(transaction_artifact(&dir, STATE_PREFIX).exists());
        assert_eq!(
            std::fs::read_to_string(transaction_artifact(&dir, BACKUP_PREFIX)).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn fixed_and_dynamic_collisions_do_not_modify_agents() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        std::fs::write(&agents, "original\n").unwrap();
        let state_txn = "fedcba9876543210fedcba9876543210";
        let foreign_state = dir.path().join(transaction_state_name(state_txn));
        std::fs::write(&foreign_state, "foreign state").unwrap();
        assert!(
            inject_system_prompt_into_agents_md_impl(path(&dir), "prompt", Some(state_txn))
                .is_none()
        );
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), "original\n");

        std::fs::remove_file(foreign_state).unwrap();
        let txn = "0123456789abcdef0123456789abcdef";
        std::fs::write(dir.path().join(format!("{BACKUP_PREFIX}{txn}")), "foreign").unwrap();
        assert!(
            inject_system_prompt_into_agents_md_impl(path(&dir), "prompt", Some(txn)).is_none()
        );
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), "original\n");
    }

    #[test]
    fn legacy_marker_is_preserved_and_never_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        std::fs::write(&agents, "current\n").unwrap();
        let legacy = dir.path().join(LEGACY_BACKUP_FILE);
        std::fs::write(&legacy, "legacy backup\n").unwrap();
        assert!(inject_system_prompt_into_agents_md(path(&dir), "prompt").is_err());
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), "current\n");
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "legacy backup\n");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_agents_and_control_files_are_refused_without_touching_legacy_lock() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "target data\n").unwrap();
        symlink(&target, dir.path().join(AGENTS_MD)).unwrap();
        assert!(inject_system_prompt_into_agents_md(path(&dir), "prompt").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "target data\n");

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "target data\n").unwrap();
        let legacy_lock = dir.path().join(LEGACY_WORKSPACE_LOCK_FILE);
        symlink(&target, &legacy_lock).unwrap();
        let guard = inject_system_prompt_into_agents_md(path(&dir), "prompt").unwrap();
        drop(guard);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "target data\n");
        assert!(std::fs::symlink_metadata(legacy_lock)
            .unwrap()
            .file_type()
            .is_symlink());

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "target data\n").unwrap();
        let txn = "0123456789abcdef0123456789abcdef";
        symlink(&target, dir.path().join(transaction_state_name(txn))).unwrap();
        assert!(
            inject_system_prompt_into_agents_md_impl(path(&dir), "prompt", Some(txn)).is_none()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "target data\n");
    }

    #[test]
    fn workspace_lock_is_external_and_serializes_contenders() {
        let workspace = tempfile::tempdir().unwrap();
        let lock_parent = tempfile::tempdir().unwrap();
        let lock_root = lock_parent.path().join("locks");

        let first = try_acquire_lock_at(workspace.path(), &lock_root).unwrap();
        assert!(!workspace.path().join(LEGACY_WORKSPACE_LOCK_FILE).exists());
        let lock_name = agents_lock_name(workspace.path()).unwrap();
        assert!(lock_root.join(lock_name).is_file());
        assert!(try_acquire_lock_at(workspace.path(), &lock_root).is_none());
        drop(first);
        assert!(try_acquire_lock_at(workspace.path(), &lock_root).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_external_lock_root_is_refused() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let lock_parent = tempfile::tempdir().unwrap();
        let real_root = lock_parent.path().join("real");
        let linked_root = lock_parent.path().join("linked");
        std::fs::create_dir(&real_root).unwrap();
        symlink(&real_root, &linked_root).unwrap();

        assert!(try_acquire_lock_at(workspace.path(), &linked_root).is_none());
        assert!(std::fs::read_dir(real_root).unwrap().next().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_agents_file_is_refused_when_symlinks_are_available() {
        use std::os::windows::fs::symlink_file;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "target data\n").unwrap();
        if symlink_file(&target, dir.path().join(AGENTS_MD)).is_ok() {
            assert!(inject_system_prompt_into_agents_md(path(&dir), "prompt").is_err());
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "target data\n");
        }
    }

    #[test]
    fn requested_prompt_failure_is_an_error_while_absent_prompt_needs_no_guard() {
        let dir = tempfile::tempdir().unwrap();
        let active = inject_system_prompt_into_agents_md(path(&dir), "active").unwrap();

        assert!(prepare_requested_system_prompt(path(&dir), Some("contender")).is_err());
        assert!(prepare_requested_system_prompt(path(&dir), None)
            .unwrap()
            .is_none());

        drop(active);
    }

    #[test]
    fn oversized_agents_file_makes_requested_prompt_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(AGENTS_MD);
        let file = std::fs::File::create(&agents).unwrap();
        file.set_len(MAX_ORIGINAL_AGENTS_BYTES + 1).unwrap();
        drop(file);

        assert!(prepare_requested_system_prompt(path(&dir), Some("prompt")).is_err());
        assert_eq!(
            std::fs::metadata(&agents).unwrap().len(),
            MAX_ORIGINAL_AGENTS_BYTES + 1
        );
    }

    #[test]
    fn explicitly_empty_system_prompt_is_still_injected() {
        let dir = tempfile::tempdir().unwrap();

        let guard = prepare_requested_system_prompt(path(&dir), Some(""))
            .unwrap()
            .expect("Some system prompt must produce a guard");
        assert_eq!(std::fs::read(dir.path().join(AGENTS_MD)).unwrap(), b"\n");

        drop(guard);
        assert!(!dir.path().join(AGENTS_MD).exists());
    }
}

// ============================================================
// Build the `opencode run` command
// ============================================================

fn build_opencode_command(
    session_id: Option<&str>,
    working_dir: &str,
    system_prompt_file: Option<&str>,
    model: Option<&str>,
    fork_session: bool,
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
        if fork_session {
            args.push("--fork".into());
        }
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

    let (mut cmd, _sp_path) = build_opencode_command(session_id, working_dir, None, model, false);

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
    fork_session: bool,
) -> Result<(), String> {
    if let Some(sid) = session_id {
        if !crate::services::process::is_valid_session_id(sid) {
            return Err(format!("Invalid session_id format: {}", sid));
        }
    }
    let force_legacy = std::env::var("COKACDIR_OPENCODE_LEGACY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if force_legacy || fork_session {
        opencode_debug(&format!(
            "[dispatch] legacy path (force_legacy={}, fork_session={})",
            force_legacy, fork_session
        ));
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
            fork_session,
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
                false,
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
    fork_session: bool,
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
        Some(sp) => {
            opencode_debug(&format!(
                "[stream] injecting system prompt into AGENTS.md ({} bytes)",
                sp.len()
            ));
            prepare_requested_system_prompt(working_dir, Some(sp))?
        }
        None => {
            opencode_debug("[stream] no system prompt, skipping AGENTS.md injection");
            prepare_requested_system_prompt(working_dir, None)?
        }
    };

    let (mut cmd, _sp_path) =
        build_opencode_command(session_id, working_dir, None, model, fork_session);

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
                if sender
                    .send(StreamMessage::Error {
                        message: format!("Failed to read output: {}", e),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: None,
                    })
                    .is_err()
                {
                    terminate_child_after_receiver_drop(&mut child);
                    return Ok(());
                }
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
                        terminate_child_after_receiver_drop(&mut child);
                        return Ok(());
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
                        terminate_child_after_receiver_drop(&mut child);
                        return Ok(());
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
                        terminate_child_after_receiver_drop(&mut child);
                        return Ok(());
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
                            terminate_child_after_receiver_drop(&mut child);
                            return Ok(());
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
                    if sender
                        .send(StreamMessage::Done {
                            result: final_result.clone(),
                            session_id: last_session_id.clone(),
                        })
                        .is_err()
                    {
                        opencode_debug("[stream] Done send failed (receiver dropped)");
                        terminate_child_after_receiver_drop(&mut child);
                        return Ok(());
                    }
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

#[derive(Default)]
struct ReceiverDropSignal {
    dropped: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl ReceiverDropSignal {
    fn mark_dropped(&self) {
        self.dropped
            .store(true, std::sync::atomic::Ordering::Release);
        // `notify_one` stores a permit when the poll task has not started
        // waiting yet, so a send failure cannot be lost in that race.
        self.notify.notify_one();
    }

    fn is_dropped(&self) -> bool {
        self.dropped.load(std::sync::atomic::Ordering::Acquire)
    }

    async fn wait(&self) {
        while !self.is_dropped() {
            self.notify.notified().await;
        }
    }
}

fn send_serve_stream_message(
    sender: &Sender<StreamMessage>,
    message: StreamMessage,
    receiver_drop: &ReceiverDropSignal,
) -> bool {
    if sender.send(message).is_ok() {
        true
    } else {
        receiver_drop.mark_dropped();
        false
    }
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
        Some(sp) => {
            opencode_debug(&format!(
                "[serve] injecting system prompt into AGENTS.md ({} bytes)",
                sp.len()
            ));
            prepare_requested_system_prompt(working_dir, Some(sp))?
        }
        None => {
            opencode_debug("[serve] no system prompt, skipping AGENTS.md injection");
            prepare_requested_system_prompt(working_dir, None)?
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

    // Existing unfinished todos are part of the cloned session's context. They
    // must not keep this new turn alive forever unless the turn touches them or
    // creates new unfinished todos.
    let todo_baseline =
        match get_unfinished_todo_fingerprints(&client, &base_url, &parent_sid).await {
            Ok(baseline) => {
                let unfinished_count: usize = baseline.values().sum();
                opencode_debug(&format!(
                    "[serve] todo baseline unfinished_count={} distinct_fingerprints={}",
                    unfinished_count,
                    baseline.len()
                ));
                baseline
            }
            Err(e) => {
                opencode_debug(&format!(
                    "[serve] todo baseline failed; using empty baseline: {}",
                    e
                ));
                TodoFingerprintCounts::new()
            }
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
    let receiver_drop = Arc::new(ReceiverDropSignal::default());

    let sse_handle = {
        let parent_sid = parent_sid.clone();
        let sender = sender.clone();
        let final_result = final_result.clone();
        let last_error = last_error.clone();
        let sse_stop = sse_stop.clone();
        let receiver_drop = receiver_drop.clone();
        tokio::task::spawn(async move {
            consume_sse_chunks(
                sse_resp,
                parent_sid,
                sender,
                final_result,
                last_error,
                sse_stop,
                receiver_drop,
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
    let mut poll_result = tokio::select! {
        result = poll_until_complete(
            &client,
            &base_url,
            &parent_sid,
            &todo_baseline,
            cancel_token.as_ref(),
        ) => result,
        _ = receiver_drop.wait() => {
            opencode_debug("[serve] stream receiver dropped; aborting HTTP polling");
            Err(PollError::ReceiverDropped)
        }
    };
    // If polling and the receiver-drop notification become ready together,
    // `select!` may choose either branch. Receiver loss still takes priority:
    // skip the trailing drain delay and avoid any terminal send attempt.
    if receiver_drop.is_dropped() {
        poll_result = Err(PollError::ReceiverDropped);
    }

    // ---- 9. Shut everything down ----
    sse_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    // Give the SSE task a brief moment to drain any trailing events, then
    // abort. The stop flag lets the loop exit on its next iteration; the
    // short sleep makes that likely before we force-abort.
    if !matches!(poll_result, Err(PollError::ReceiverDropped)) {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
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
        Err(PollError::ReceiverDropped) => {
            opencode_debug("[serve] receiver dropped; child shut down without terminal send");
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
    receiver_drop: Arc<ReceiverDropSignal>,
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
        if stop.load(std::sync::atomic::Ordering::Relaxed) || receiver_drop.is_dropped() {
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
                &receiver_drop,
            )
            .await;
            if receiver_drop.is_dropped() {
                opencode_debug("[serve.sse] receiver dropped, exiting consumer");
                break;
            }
        }
        if receiver_drop.is_dropped() {
            break;
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
    receiver_drop: &ReceiverDropSignal,
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
                            if !send_serve_stream_message(
                                sender,
                                StreamMessage::Text { content: delta },
                                receiver_drop,
                            ) {
                                return;
                            }
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
                    if !send_serve_stream_message(
                        sender,
                        StreamMessage::ToolUse {
                            name: tool_name.clone(),
                            input: input_str,
                        },
                        receiver_drop,
                    ) {
                        return;
                    }
                    if !send_serve_stream_message(
                        sender,
                        StreamMessage::ToolResult {
                            content: output_str,
                            is_error,
                        },
                        receiver_drop,
                    ) {
                        return;
                    }
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
            if !send_serve_stream_message(
                sender,
                StreamMessage::Text {
                    content: delta.to_string(),
                },
                receiver_drop,
            ) {
                return;
            }
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
    ReceiverDropped,
    Fatal(String),
}

/// Wait until the parent session is fully idle: no primary work in progress,
/// no running child sessions, and no unfinished todos. Mirrors
/// `oh-my-opencode run`'s `pollForCompletion`.
async fn poll_until_complete(
    client: &reqwest::Client,
    base_url: &str,
    parent_sid: &str,
    todo_baseline: &TodoFingerprintCounts,
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
        let todos_pending = match get_todos_pending(client, base_url, parent_sid, todo_baseline)
            .await
        {
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
    baseline: &TodoFingerprintCounts,
) -> Result<bool, String> {
    let list = get_todo_list(client, base_url, session_id).await?;
    Ok(todos_pending_after_baseline(&list, baseline))
}

async fn get_unfinished_todo_fingerprints(
    client: &reqwest::Client,
    base_url: &str,
    session_id: &str,
) -> Result<TodoFingerprintCounts, String> {
    let list = get_todo_list(client, base_url, session_id).await?;
    Ok(unfinished_todo_fingerprints(&list))
}

async fn get_todo_list(
    client: &reqwest::Client,
    base_url: &str,
    session_id: &str,
) -> Result<Vec<Value>, String> {
    let url = format!("{}/session/{}/todo", base_url, session_id);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("http: {}", e))?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(Vec::new());
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
        return Ok(Vec::new());
    }
    let arr: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };
    Ok(arr.as_array().cloned().unwrap_or_default())
}

fn todo_is_unfinished(todo: &Value) -> bool {
    let status = todo.get("status").and_then(|v| v.as_str()).unwrap_or("");
    status != "completed" && status != "cancelled"
}

fn todo_fingerprint(todo: &Value) -> String {
    let canonical = |key: &str| {
        todo.get(key)
            .map(|value| serde_json::to_string(value).unwrap_or_default())
            .unwrap_or_default()
    };
    [
        canonical("id"),
        canonical("content"),
        canonical("status"),
        canonical("priority"),
        canonical("position"),
    ]
    .join("\u{1f}")
}

fn unfinished_todo_fingerprints(todos: &[Value]) -> TodoFingerprintCounts {
    let mut counts = TodoFingerprintCounts::new();
    for fingerprint in todos
        .iter()
        .filter(|todo| todo_is_unfinished(todo))
        .map(todo_fingerprint)
    {
        *counts.entry(fingerprint).or_insert(0) += 1;
    }
    counts
}

fn todos_pending_after_baseline(todos: &[Value], baseline: &TodoFingerprintCounts) -> bool {
    let mut remaining_baseline = baseline.clone();
    for fingerprint in todos
        .iter()
        .filter(|todo| todo_is_unfinished(todo))
        .map(todo_fingerprint)
    {
        match remaining_baseline.get_mut(&fingerprint) {
            Some(count) if *count > 0 => *count -= 1,
            _ => return true,
        }
    }
    false
}

#[cfg(test)]
mod serve_receiver_drop_tests {
    use super::{send_serve_stream_message, ReceiverDropSignal};
    use crate::services::claude::StreamMessage;

    #[tokio::test]
    async fn failed_sse_send_wakes_the_poll_abort_waiter() {
        let signal = std::sync::Arc::new(ReceiverDropSignal::default());
        let waiter_signal = signal.clone();
        let waiter = tokio::spawn(async move { waiter_signal.wait().await });
        tokio::task::yield_now().await;

        let (sender, receiver) = std::sync::mpsc::channel();
        drop(receiver);
        assert!(!send_serve_stream_message(
            &sender,
            StreamMessage::Text {
                content: "ignored".to_string(),
            },
            &signal,
        ));

        tokio::time::timeout(std::time::Duration::from_millis(100), waiter)
            .await
            .expect("receiver-drop waiter must wake immediately")
            .expect("waiter task must not panic");
        assert!(signal.is_dropped());
    }

    #[tokio::test]
    async fn receiver_drop_notification_is_not_lost_before_wait_starts() {
        let signal = ReceiverDropSignal::default();
        signal.mark_dropped();
        tokio::time::timeout(std::time::Duration::from_millis(100), signal.wait())
            .await
            .expect("pre-existing receiver drop must be observed");
    }
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
