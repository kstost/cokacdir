//! Full-fidelity session archive.
//!
//! Converts raw session data from Claude Code, Codex, Agy/OpenCode, and Gemini
//! into a normalized schema preserving all text, tool arguments, tool results,
//! timestamps, model info, and usage. No truncation.
//!
//! Output: `~/.cokacdir/ai_sessions_full/{session_id}.json`
//!
//! This archive is parallel to `~/.cokacdir/ai_sessions/` (the UI summary
//! written by telegram.rs `convert_and_save_session`). The summary truncates;
//! this archive does not. Each side updates independently based on source mtime.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::services::claude::debug_log_to;

fn dbg(msg: &str) {
    debug_log_to("session_archive.log", msg);
}

/// Short (12-hex-char) SHA-256 of the input, used for /loop verification
/// forensic logs so consecutive-iteration transcripts can be compared at a
/// glance. Not a security hash.
fn short_sha(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let r = h.finalize();
    hex::encode(&r[..6])
}

/// Root document written to disk.
#[derive(Debug, Serialize, Deserialize)]
pub struct FullSession {
    pub session_id: String,
    pub provider: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub source_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_meta: Option<Value>,
    pub messages: Vec<Message>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// One logical message or event in the conversation.
#[derive(Debug, Serialize, Deserialize)]
pub struct Message {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub role: String,
    /// Provider-specific origin tag (e.g. "claude:assistant",
    /// "codex:response_item.function_call", "opencode:part.text").
    pub source: String,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub meta: BTreeMap<String, Value>,
    /// Verbatim original record (line or row). Preserved to recover any
    /// fields not captured by the normalized schema.
    pub raw: Value,
}

/// One content element inside a Message. Fields are optional so new block
/// types can be added without breaking consumers; the `kind` discriminator
/// identifies which fields are meaningful.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContentBlock {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub extra: BTreeMap<String, Value>,
}

impl ContentBlock {
    fn text(s: impl Into<String>) -> Self {
        Self {
            kind: "text".into(),
            text: Some(s.into()),
            tool_name: None,
            tool_id: None,
            tool_input: None,
            tool_output: None,
            is_error: None,
            extra: BTreeMap::new(),
        }
    }
    fn thinking(s: impl Into<String>) -> Self {
        Self {
            kind: "thinking".into(),
            text: Some(s.into()),
            tool_name: None,
            tool_id: None,
            tool_input: None,
            tool_output: None,
            is_error: None,
            extra: BTreeMap::new(),
        }
    }
    fn tool_use(name: impl Into<String>, id: Option<String>, input: Value) -> Self {
        Self {
            kind: "tool_use".into(),
            text: None,
            tool_name: Some(name.into()),
            tool_id: id,
            tool_input: Some(input),
            tool_output: None,
            is_error: None,
            extra: BTreeMap::new(),
        }
    }
    fn tool_result(id: Option<String>, output: Value, is_error: Option<bool>) -> Self {
        Self {
            kind: "tool_result".into(),
            text: None,
            tool_name: None,
            tool_id: id,
            tool_input: None,
            tool_output: Some(output),
            is_error,
            extra: BTreeMap::new(),
        }
    }
    fn other(kind: impl Into<String>, raw: Value) -> Self {
        let mut extra = BTreeMap::new();
        extra.insert("raw".into(), raw);
        Self {
            kind: kind.into(),
            text: None,
            tool_name: None,
            tool_id: None,
            tool_input: None,
            tool_output: None,
            is_error: None,
            extra,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Value::is_null", default)]
    pub extra: Value,
}

// =====================================================================
// Entry point
// =====================================================================

pub fn archive_sessions_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cokacdir").join("ai_sessions_full"))
}

/// Load the full-fidelity archive for a session and render it as a compact
/// transcript suitable for isolated follow-up prompts. Used by the Codex
/// `/loop` verifier, which must not resume the original session directly.
///
/// System breadcrumbs (role="system") are dropped because they carry no
/// completion signal. Per-block text is capped at PER_BLOCK_LIMIT and the
/// whole transcript at TOTAL_LIMIT to keep the verifier's input bounded;
/// the archive on disk is untouched.
///
/// `session_id` is validated here so callers constructing the archive path
/// cannot traverse outside `archive_sessions_dir()` via `../` injection.
pub fn build_verification_transcript(session_id: &str) -> Result<String, String> {
    const PER_BLOCK_LIMIT: usize = 2000;
    const TOTAL_LIMIT: usize = 60000;
    // Anchor the original user request at the top of the transcript even on
    // long sessions so the verifier always knows what was asked. This is a
    // small share of TOTAL_LIMIT — the rest is reserved for the most recent
    // turns, which is what the verifier actually needs to judge completion.
    const HEAD_BUDGET: usize = 6000;

    if !is_valid_session_id(session_id) {
        return Err(format!("Invalid session_id: {:?}", session_id));
    }
    let archive_dir =
        archive_sessions_dir().ok_or_else(|| "Cannot locate archive dir".to_string())?;
    let path = archive_dir.join(format!("{}.json", session_id));
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Archive not found at {}: {}", path.display(), e))?;
    let archive_mtime = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dbg(&format!(
        "[loop-verify transcript] archive_read sid={} path={} raw_len={} raw_sha={} archive_mtime_epoch={}",
        session_id, path.display(), raw.len(), short_sha(&raw), archive_mtime));
    let archive: FullSession =
        serde_json::from_str(&raw).map_err(|e| format!("Archive parse error: {}", e))?;

    // Render a single message into a chunk. Honors PER_BLOCK_LIMIT per content
    // block but NOT TOTAL_LIMIT — callers compose selected chunks within the
    // global budget.
    let render_msg = |m: &Message| -> String {
        let header = match m.role.as_str() {
            "user" => "USER".to_string(),
            "assistant" => "ASSISTANT".to_string(),
            "tool" => "TOOL_RESULT".to_string(),
            "developer" => "DEVELOPER".to_string(),
            other => other.to_uppercase(),
        };
        let mut s = format!("\n[{}]\n", header);
        for b in &m.content {
            match b.kind.as_str() {
                "text" => {
                    if let Some(t) = &b.text {
                        s.push_str(&truncate_utf8_boundary(t, PER_BLOCK_LIMIT));
                        s.push('\n');
                    }
                }
                "thinking" => {
                    if let Some(t) = &b.text {
                        if !t.is_empty() {
                            s.push_str("(thinking) ");
                            s.push_str(&truncate_utf8_boundary(t, PER_BLOCK_LIMIT));
                            s.push('\n');
                        }
                    }
                }
                "tool_use" => {
                    let name = b.tool_name.as_deref().unwrap_or("?");
                    let input = b
                        .tool_input
                        .as_ref()
                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                        .unwrap_or_default();
                    s.push_str(&format!(
                        "(tool_use:{}) {}\n",
                        name,
                        truncate_utf8_boundary(&input, PER_BLOCK_LIMIT)
                    ));
                }
                "tool_result" => {
                    let output = b
                        .tool_output
                        .as_ref()
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => serde_json::to_string(other).unwrap_or_default(),
                        })
                        .unwrap_or_default();
                    let err_tag = if b.is_error == Some(true) {
                        " ERROR"
                    } else {
                        ""
                    };
                    s.push_str(&format!(
                        "(tool_result{}) {}\n",
                        err_tag,
                        truncate_utf8_boundary(&output, PER_BLOCK_LIMIT)
                    ));
                }
                "patch" => s.push_str("(patch applied)\n"),
                _ => {}
            }
        }
        s
    };

    // Indices of all visible messages (non-system, non-empty).
    let visible_idx: Vec<usize> = archive
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role != "system" && !m.content.is_empty())
        .map(|(i, _)| i)
        .collect();

    // The first user message is treated as the original request — pin it at
    // the top. If no user message exists yet, head is empty.
    let head_idx: Option<usize> = visible_idx.iter().copied().find(|i| {
        archive
            .messages
            .get(*i)
            .map(|m| m.role == "user")
            .unwrap_or(false)
    });

    let head_rendered: Option<String> = head_idx.and_then(|i| archive.messages.get(i)).map(|m| {
        let rendered = render_msg(m);
        if rendered.len() > HEAD_BUDGET {
            let mut end = HEAD_BUDGET;
            while end > 0 && !rendered.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}\n[...head truncated...]\n", &rendered[..end])
        } else {
            rendered
        }
    });

    // Fill the tail from the END of the conversation backwards, stopping when
    // the next chunk would exceed the remaining budget. This guarantees the
    // most recent turn is always included — the core fix for the "stale
    // transcript" bug where the old forward-fill ignored every message past
    // the 60 KB front slice.
    let head_len = head_rendered.as_ref().map(|s| s.len()).unwrap_or(0);
    let separator = "\n[...earlier transcript truncated — middle turns omitted...]\n";
    let tail_budget = TOTAL_LIMIT
        .saturating_sub(head_len)
        .saturating_sub(separator.len());

    let mut tail_chunks_rev: Vec<String> = Vec::new();
    let mut tail_acc: usize = 0;
    let mut dropped_middle = false;
    let mut is_newest = true;
    for &i in visible_idx.iter().rev() {
        if Some(i) == head_idx {
            // Reached the head from the tail side — every visible message
            // between head and tail is already included, no middle dropped.
            break;
        }
        let Some(m) = archive.messages.get(i) else {
            continue;
        };
        let mut chunk = render_msg(m);
        // Guard: if the single newest turn alone exceeds the tail budget,
        // truncate it instead of dropping it. Losing the newest turn entirely
        // would defeat the purpose of this function.
        if is_newest && chunk.len() > tail_budget {
            let marker = "\n[...turn truncated...]\n";
            let cap = tail_budget.saturating_sub(marker.len());
            let mut end = cap;
            while end > 0 && !chunk.is_char_boundary(end) {
                end -= 1;
            }
            chunk.truncate(end);
            chunk.push_str(marker);
            dropped_middle = true;
        }
        is_newest = false;
        if tail_acc + chunk.len() > tail_budget {
            // Budget exhausted before we reached the head (or before
            // exhausting visible_idx when there is no head). Some middle
            // turns will be omitted, so emit the separator.
            dropped_middle = true;
            break;
        }
        tail_acc += chunk.len();
        tail_chunks_rev.push(chunk);
    }

    // Reverse back to chronological order.
    tail_chunks_rev.reverse();
    let tail_joined: String = tail_chunks_rev.concat();

    let mut out = String::new();
    if let Some(h) = head_rendered.as_ref() {
        out.push_str(h);
        if dropped_middle {
            out.push_str(separator);
        }
    } else if dropped_middle {
        out.push_str(separator);
    }
    out.push_str(&tail_joined);

    // Forensic log: per-iteration transcript fingerprint. After the fix the
    // `sha` should change whenever a new turn lands; if it still doesn't, the
    // archive itself isn't advancing (check the archive REGENERATED line).
    let num_msgs = visible_idx.len();
    let tail_start = out.len().saturating_sub(500);
    let mut tail_off = tail_start;
    while tail_off < out.len() && !out.is_char_boundary(tail_off) {
        tail_off += 1;
    }
    let tail = &out[tail_off..];
    dbg(&format!(
        "[loop-verify transcript] built sid={} len={} sha={} non_system_msgs={} head={} dropped_middle={} tail_msgs={} tail_500={:?}",
        session_id, out.len(), short_sha(&out), num_msgs,
        head_rendered.is_some(), dropped_middle, tail_chunks_rev.len(), tail));

    Ok(out)
}

fn truncate_utf8_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn is_valid_session_id(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    if s.contains('/') || s.contains('\\') || s.contains("..") {
        return false;
    }
    // A leading `-` would be treated as a CLI flag if this id is ever
    // spliced into an argv list.
    if s.starts_with('-') {
        return false;
    }
    s.chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// Entry point: convert the given source and write the normalized archive.
/// Skips work if the target JSON is newer than the source.
pub fn archive_and_save_session(provider: &str, source_path: &Path, session_id: &str, cwd: &str) {
    dbg(&format!(
        "[archive] start: provider={}, source={}, id={}, cwd={}",
        provider,
        source_path.display(),
        session_id,
        cwd
    ));

    if !is_valid_session_id(session_id) {
        dbg(&format!("[archive] invalid session_id: {:?}", session_id));
        return;
    }
    let Some(out_dir) = archive_sessions_dir() else {
        dbg("[archive] archive_sessions_dir() returned None");
        return;
    };
    let target = out_dir.join(format!("{}.json", session_id));

    // Skip if up-to-date. On uncertain mtime (permission error, etc.),
    // fall through and regenerate rather than silently refusing to update.
    // Track pre-regenerate size so the forensic log below can show a delta.
    let prev_target_size = target.metadata().ok().map(|m| m.len()).unwrap_or(0);
    let src_epoch = source_path
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tgt_epoch = target
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if target.exists() {
        let src_m = source_path.metadata().ok().and_then(|m| m.modified().ok());
        let tgt_m = target.metadata().ok().and_then(|m| m.modified().ok());
        if let (Some(s), Some(t)) = (src_m, tgt_m) {
            if s <= t {
                dbg(&format!(
                    "[loop-verify archive] SKIPPED sid={} src_mtime={} tgt_mtime={} tgt_size={} — \
                     target is up-to-date (rollout has NOT grown since last refresh). \
                     If this fires on consecutive /loop iterations with no turn in between, \
                     the verifier will read the SAME transcript.",
                    session_id, src_epoch, tgt_epoch, prev_target_size
                ));
                return;
            }
        }
    }

    let result = match provider {
        "claude" => parse_claude(source_path, session_id, cwd),
        "codex" => parse_codex(source_path, session_id, cwd),
        "agy" => parse_agy(source_path, session_id, cwd),
        "gemini" => parse_gemini(source_path, session_id, cwd),
        "opencode" => parse_opencode(source_path, session_id, cwd),
        _ => {
            dbg(&format!("[archive] unknown provider: {}", provider));
            return;
        }
    };
    let Some(mut session) = result else {
        dbg("[archive] parser returned None");
        return;
    };

    // Stamp updated_at from source mtime if not set
    if session.updated_at.is_none() {
        if let Ok(m) = source_path.metadata() {
            if let Ok(t) = m.modified() {
                if let Ok(dt) = t.duration_since(std::time::UNIX_EPOCH) {
                    let secs = dt.as_secs() as i64;
                    if let Some(naive) = chrono::DateTime::from_timestamp(secs, 0) {
                        session.updated_at = Some(naive.to_rfc3339());
                    }
                }
            }
        }
    }

    let _ = fs::create_dir_all(&out_dir);
    match serde_json::to_string_pretty(&session) {
        Ok(json) => {
            // Atomic write: stage to <target>.tmp then rename, so a crash
            // mid-write never leaves a half-written JSON file at the target.
            let tmp = target.with_extension("json.tmp");
            match fs::write(&tmp, &json) {
                Ok(()) => {
                    let r = fs::rename(&tmp, &target);
                    let new_size = json.len() as u64;
                    let delta: i64 = new_size as i64 - prev_target_size as i64;
                    dbg(&format!(
                        "[loop-verify archive] REGENERATED sid={} path={} new_size={} prev_size={} delta={:+} messages={} src_mtime={} rename={:?}",
                        session_id, target.display(), new_size, prev_target_size,
                        delta, session.messages.len(), src_epoch, r));
                    if r.is_err() {
                        let _ = fs::remove_file(&tmp);
                    }
                }
                Err(e) => {
                    dbg(&format!("[archive] tmp write failed: {}", e));
                    let _ = fs::remove_file(&tmp);
                }
            }
        }
        Err(e) => dbg(&format!("[archive] serialize failed: {}", e)),
    }
}

// =====================================================================
// Claude Code parser
// =====================================================================

/// Claude JSONL: one record per line. Key types we care about:
/// - "user" / "assistant": the conversation turns
/// - "session_meta" / "permission-mode" / "file-history-snapshot": metadata
/// - "message" / "text" / "ai-title" / "attachment" / "last-prompt" /
///   "skill_listing" / "queue-operation": various auxiliary records
///
/// `isSidechain: true` entries are PRESERVED here (unlike the UI summary)
/// but tagged in `meta.is_sidechain` so consumers can filter them.
fn parse_claude(path: &Path, session_id: &str, cwd: &str) -> Option<FullSession> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut messages: Vec<Message> = Vec::new();
    let mut created_at: Option<String> = None;
    let mut last_model: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut session_meta: Option<Value> = None;
    let mut idx: u32 = 0;

    for line in reader.lines().flatten() {
        let Ok(val) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let rec_type = val
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ts = val
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(String::from);
        if created_at.is_none() {
            created_at = ts.clone();
        }
        if let Some(g) = val.get("gitBranch").and_then(|v| v.as_str()) {
            if !g.is_empty() && git_branch.is_none() {
                git_branch = Some(g.to_string());
            }
        }

        match rec_type.as_str() {
            "user" => {
                let msg = val.get("message").cloned().unwrap_or(Value::Null);
                let content_val = msg.get("content").cloned().unwrap_or(Value::Null);
                let blocks = claude_content_blocks(&content_val);

                let mut meta = BTreeMap::new();
                if let Some(b) = val.get("isSidechain").and_then(|v| v.as_bool()) {
                    if b {
                        meta.insert("is_sidechain".into(), json!(true));
                    }
                }
                if let Some(b) = val.get("isMeta").and_then(|v| v.as_bool()) {
                    if b {
                        meta.insert("is_meta".into(), json!(true));
                    }
                }
                if let Some(u) = val.get("uuid").and_then(|v| v.as_str()) {
                    meta.insert("uuid".into(), json!(u));
                }
                if let Some(p) = val.get("parentUuid").and_then(|v| v.as_str()) {
                    meta.insert("parent_uuid".into(), json!(p));
                }

                messages.push(Message {
                    index: idx,
                    timestamp: ts,
                    role: "user".into(),
                    source: "claude:user".into(),
                    content: blocks,
                    model: None,
                    usage: None,
                    stop_reason: None,
                    meta,
                    raw: val,
                });
                idx += 1;
            }
            "assistant" => {
                let msg = val.get("message").cloned().unwrap_or(Value::Null);
                let model = msg.get("model").and_then(|v| v.as_str()).map(String::from);
                if model.is_some() {
                    last_model = model.clone();
                }
                let content_val = msg.get("content").cloned().unwrap_or(Value::Null);
                let blocks = claude_content_blocks(&content_val);
                let stop_reason = msg
                    .get("stop_reason")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let usage = msg.get("usage").map(claude_parse_usage);

                let mut meta = BTreeMap::new();
                if let Some(b) = val.get("isSidechain").and_then(|v| v.as_bool()) {
                    if b {
                        meta.insert("is_sidechain".into(), json!(true));
                    }
                }
                if let Some(u) = val.get("uuid").and_then(|v| v.as_str()) {
                    meta.insert("uuid".into(), json!(u));
                }
                if let Some(p) = val.get("parentUuid").and_then(|v| v.as_str()) {
                    meta.insert("parent_uuid".into(), json!(p));
                }
                if let Some(r) = val.get("requestId").and_then(|v| v.as_str()) {
                    meta.insert("request_id".into(), json!(r));
                }
                if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                    meta.insert("message_id".into(), json!(id));
                }

                messages.push(Message {
                    index: idx,
                    timestamp: ts,
                    role: "assistant".into(),
                    source: "claude:assistant".into(),
                    content: blocks,
                    model,
                    usage,
                    stop_reason,
                    meta,
                    raw: val,
                });
                idx += 1;
            }
            // Session-level metadata: preserve the last such record encountered.
            // `system` carries turn_duration/messageCount style telemetry — keep as metadata.
            "session_meta"
            | "permission-mode"
            | "ai-title"
            | "skill_listing"
            | "file-history-snapshot"
            | "last-prompt"
            | "attachment"
            | "queue-operation"
            | "text"
            | "message"
            | "system" => {
                // Aggregate non-conversational records under session_meta[type].
                let bucket = session_meta.get_or_insert_with(|| json!({}));
                if let Some(obj) = bucket.as_object_mut() {
                    let arr = obj.entry(rec_type.clone()).or_insert_with(|| json!([]));
                    if let Some(a) = arr.as_array_mut() {
                        a.push(val);
                    }
                }
            }
            _ => {
                let bucket = session_meta.get_or_insert_with(|| json!({}));
                if let Some(obj) = bucket.as_object_mut() {
                    let arr = obj.entry("_other".to_string()).or_insert_with(|| json!([]));
                    if let Some(a) = arr.as_array_mut() {
                        a.push(val);
                    }
                }
            }
        }
    }

    if messages.is_empty() && session_meta.is_none() {
        return None;
    }

    Some(FullSession {
        session_id: session_id.to_string(),
        provider: "claude".into(),
        cwd: cwd.to_string(),
        created_at,
        updated_at: None,
        source_path: path.display().to_string(),
        model: last_model,
        git: git_branch.map(|b| GitInfo {
            branch: Some(b),
            commit: None,
        }),
        session_meta,
        messages,
    })
}

/// Decode Claude `message.content`: either a plain string or an array of blocks.
fn claude_content_blocks(content: &Value) -> Vec<ContentBlock> {
    if let Some(s) = content.as_str() {
        return vec![ContentBlock::text(s)];
    }
    let Some(arr) = content.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let t = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match t {
            "text" => {
                let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                out.push(ContentBlock::text(text));
            }
            "thinking" => {
                let text = item
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("text").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let mut b = ContentBlock::thinking(text);
                // `signature` is Anthropic's replay-signing marker for thinking
                // blocks. Preserve it so archives can be replayed.
                if let Some(sig) = item.get("signature") {
                    b.extra.insert("signature".into(), sig.clone());
                }
                out.push(b);
            }
            "tool_use" => {
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let id = item.get("id").and_then(|v| v.as_str()).map(String::from);
                let input = item.get("input").cloned().unwrap_or(Value::Null);
                out.push(ContentBlock::tool_use(name, id, input));
            }
            "tool_result" => {
                let id = item
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let output = item.get("content").cloned().unwrap_or(Value::Null);
                let is_err = item.get("is_error").and_then(|v| v.as_bool());
                out.push(ContentBlock::tool_result(id, output, is_err));
            }
            "image" => {
                let source = item.get("source").cloned().unwrap_or(Value::Null);
                let mut b = ContentBlock::other("image", source);
                // Preserve full image record for completeness
                b.extra.insert("block".into(), item.clone());
                out.push(b);
            }
            other => {
                out.push(ContentBlock::other(other, item.clone()));
            }
        }
    }
    out
}

fn claude_parse_usage(u: &Value) -> Usage {
    let g = |k: &str| u.get(k).and_then(|v| v.as_u64());
    Usage {
        input_tokens: g("input_tokens"),
        output_tokens: g("output_tokens"),
        cached_input_tokens: None,
        cache_creation_input_tokens: g("cache_creation_input_tokens"),
        cache_read_input_tokens: g("cache_read_input_tokens"),
        extra: u.clone(),
    }
}

// =====================================================================
// Codex parser
// =====================================================================

/// Codex JSONL line shape: `{"timestamp": "...", "type": "<rec>", "payload": {...}}`
/// Significant outer types: `session_meta`, `turn_context`, `event_msg`,
/// `response_item`, `token_count` (inside event_msg).
///
/// Normalization:
/// - `session_meta` → captured at session level (not a message)
/// - `turn_context` → tracked for per-turn `model`; emitted as a meta message
/// - `event_msg.user_message` / `event_msg.agent_message` → user/assistant
///   messages. These mirror `response_item.message.*_text` records but carry
///   flat text; BOTH are preserved so no information is lost. Consumers may
///   dedupe using `meta.codex_source`.
/// - `response_item.message` → user/assistant/developer message with content
///   array of `input_text` / `output_text`
/// - `response_item.function_call` → tool_use (arguments is a JSON string)
/// - `response_item.function_call_output` → tool_result
/// - `response_item.custom_tool_call` / `custom_tool_call_output` → same as above
/// - `response_item.reasoning` → thinking (encrypted_content preserved in raw)
/// - `event_msg.token_count` → Usage on a meta message (kept as informational)
fn parse_codex(path: &Path, session_id: &str, cwd: &str) -> Option<FullSession> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut messages: Vec<Message> = Vec::new();
    let mut session_meta_payload: Option<Value> = None;
    let mut created_at: Option<String> = None;
    let mut current_model: Option<String> = None;
    let mut session_model: Option<String> = None;
    let mut git: Option<GitInfo> = None;
    let mut idx: u32 = 0;

    for line in reader.lines().flatten() {
        let Ok(val) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let outer_type = val
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ts = val
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(String::from);
        if created_at.is_none() {
            created_at = ts.clone();
        }
        let payload = val.get("payload").cloned().unwrap_or(Value::Null);

        match outer_type.as_str() {
            "session_meta" => {
                if let Some(g) = payload.get("git") {
                    git = Some(GitInfo {
                        branch: g.get("branch").and_then(|v| v.as_str()).map(String::from),
                        commit: g
                            .get("commit_hash")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    });
                }
                session_meta_payload = Some(payload);
            }
            "turn_context" => {
                // Prefer top-level `model`; fall back to
                // `collaboration_mode.settings.model` which some modes set instead.
                let model_here = payload
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| {
                        payload
                            .get("collaboration_mode")
                            .and_then(|c| c.get("settings"))
                            .and_then(|s| s.get("model"))
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    });
                if let Some(m) = model_here {
                    current_model = Some(m);
                    if session_model.is_none() {
                        session_model = current_model.clone();
                    }
                }
                let mut meta = BTreeMap::new();
                meta.insert("codex_source".into(), json!("turn_context"));
                if let Some(tid) = payload.get("turn_id").and_then(|v| v.as_str()) {
                    meta.insert("turn_id".into(), json!(tid));
                }
                // Promote a few stable turn-level fields into meta for easy access.
                for k in [
                    "approval_policy",
                    "timezone",
                    "current_date",
                    "personality",
                    "realtime_active",
                ] {
                    if let Some(v) = payload.get(k) {
                        meta.insert(k.into(), v.clone());
                    }
                }
                messages.push(Message {
                    index: idx,
                    timestamp: ts,
                    role: "system".into(),
                    source: "codex:turn_context".into(),
                    content: vec![ContentBlock::other("turn_context", payload.clone())],
                    model: current_model.clone(),
                    usage: None,
                    stop_reason: None,
                    meta,
                    raw: val,
                });
                idx += 1;
            }
            "event_msg" => {
                let inner_type = payload
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match inner_type.as_str() {
                    // user_message / agent_message mirror the text content of
                    // `response_item.message` (which has richer structure).
                    // We skip the duplicated text here but still surface two
                    // extras event_msg carries that response_item doesn't:
                    //   • user_message.images / local_images  (image attachments)
                    //   • agent_message.memory_citation       (memory refs)
                    // The original line goes into `raw` either way.
                    "user_message" | "agent_message" => {
                        let mut blocks: Vec<ContentBlock> = Vec::new();
                        if inner_type == "user_message" {
                            if let Some(imgs) = payload.get("images").and_then(|v| v.as_array()) {
                                for img in imgs {
                                    let mut b = ContentBlock::other("image", img.clone());
                                    b.extra.insert(
                                        "source".into(),
                                        json!("codex:user_message.images"),
                                    );
                                    blocks.push(b);
                                }
                            }
                            if let Some(imgs) =
                                payload.get("local_images").and_then(|v| v.as_array())
                            {
                                for img in imgs {
                                    let mut b = ContentBlock::other("image", img.clone());
                                    b.extra.insert(
                                        "source".into(),
                                        json!("codex:user_message.local_images"),
                                    );
                                    blocks.push(b);
                                }
                            }
                        }
                        let mut meta = BTreeMap::new();
                        meta.insert(
                            "codex_source".into(),
                            json!(format!("event_msg.{}", inner_type)),
                        );
                        if inner_type == "user_message" && blocks.is_empty() {
                            meta.insert(
                                "skipped_reason".into(),
                                json!("mirrored by response_item.message; kept for raw fidelity"),
                            );
                        }
                        if let Some(mc) = payload.get("memory_citation") {
                            if !mc.is_null() {
                                meta.insert("memory_citation".into(), mc.clone());
                            }
                        }
                        if let Some(phase) = payload.get("phase").and_then(|v| v.as_str()) {
                            meta.insert("phase".into(), json!(phase));
                        }
                        let role = if inner_type == "user_message" && !blocks.is_empty() {
                            "user"
                        } else {
                            "system"
                        };
                        messages.push(Message {
                            index: idx,
                            timestamp: ts,
                            role: role.into(),
                            source: format!("codex:event_msg.{}", inner_type),
                            content: blocks,
                            model: None,
                            usage: None,
                            stop_reason: None,
                            meta,
                            raw: val,
                        });
                        idx += 1;
                    }
                    "token_count" => {
                        let usage = codex_extract_usage_from_token_count(&payload);
                        let mut meta = BTreeMap::new();
                        meta.insert("codex_source".into(), json!("event_msg.token_count"));
                        messages.push(Message {
                            index: idx,
                            timestamp: ts,
                            role: "system".into(),
                            source: "codex:event_msg.token_count".into(),
                            content: vec![ContentBlock::other("token_count", payload.clone())],
                            model: None,
                            usage,
                            stop_reason: None,
                            meta,
                            raw: val,
                        });
                        idx += 1;
                    }
                    // Lifecycle events (task_started/task_complete) carry
                    // timing info worth promoting; exec_command_end and
                    // patch_apply_end mirror function_call_output so we just
                    // preserve their raw payload as a system block.
                    "task_started" | "task_complete" => {
                        let mut meta = BTreeMap::new();
                        meta.insert(
                            "codex_source".into(),
                            json!(format!("event_msg.{}", inner_type)),
                        );
                        for k in [
                            "turn_id",
                            "started_at",
                            "completed_at",
                            "duration_ms",
                            "model_context_window",
                            "collaboration_mode_kind",
                            "last_agent_message",
                        ] {
                            if let Some(v) = payload.get(k) {
                                meta.insert(k.into(), v.clone());
                            }
                        }
                        messages.push(Message {
                            index: idx,
                            timestamp: ts,
                            role: "system".into(),
                            source: format!("codex:event_msg.{}", inner_type),
                            content: vec![ContentBlock::other(&inner_type, payload.clone())],
                            model: None,
                            usage: None,
                            stop_reason: None,
                            meta,
                            raw: val,
                        });
                        idx += 1;
                    }
                    _ => {
                        let mut meta = BTreeMap::new();
                        meta.insert(
                            "codex_source".into(),
                            json!(format!("event_msg.{}", inner_type)),
                        );
                        // Exec/patch/custom lifecycle events cross-reference via call_id.
                        if let Some(c) = payload.get("call_id").and_then(|v| v.as_str()) {
                            meta.insert("call_id".into(), json!(c));
                        }
                        if let Some(c) = payload.get("turn_id").and_then(|v| v.as_str()) {
                            meta.insert("turn_id".into(), json!(c));
                        }
                        messages.push(Message {
                            index: idx,
                            timestamp: ts,
                            role: "system".into(),
                            source: format!("codex:event_msg.{}", inner_type),
                            content: vec![ContentBlock::other(&inner_type, payload.clone())],
                            model: None,
                            usage: None,
                            stop_reason: None,
                            meta,
                            raw: val,
                        });
                        idx += 1;
                    }
                }
            }
            "response_item" => {
                let inner_type = payload
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match inner_type.as_str() {
                    "message" => {
                        let role = payload
                            .get("role")
                            .and_then(|v| v.as_str())
                            .unwrap_or("assistant")
                            .to_string();
                        let blocks = codex_message_content(payload.get("content"));
                        let mut meta = BTreeMap::new();
                        meta.insert("codex_source".into(), json!("response_item.message"));
                        if let Some(p) = payload.get("phase").and_then(|v| v.as_str()) {
                            meta.insert("phase".into(), json!(p));
                        }
                        messages.push(Message {
                            index: idx,
                            timestamp: ts,
                            role,
                            source: "codex:response_item.message".into(),
                            content: blocks,
                            model: current_model.clone(),
                            usage: None,
                            stop_reason: None,
                            meta,
                            raw: val,
                        });
                        idx += 1;
                    }
                    "function_call" | "custom_tool_call" => {
                        let name = payload
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let call_id = payload
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        // function_call.arguments is a JSON-encoded string;
                        // custom_tool_call.input is a plain string.
                        let input_val = if inner_type == "function_call" {
                            match payload.get("arguments").and_then(|v| v.as_str()) {
                                Some(s) => {
                                    serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!(s))
                                }
                                None => payload.get("arguments").cloned().unwrap_or(Value::Null),
                            }
                        } else {
                            payload.get("input").cloned().unwrap_or(Value::Null)
                        };
                        let mut meta = BTreeMap::new();
                        meta.insert(
                            "codex_source".into(),
                            json!(format!("response_item.{}", inner_type)),
                        );
                        if let Some(s) = payload.get("status").and_then(|v| v.as_str()) {
                            meta.insert("status".into(), json!(s));
                        }
                        messages.push(Message {
                            index: idx,
                            timestamp: ts,
                            role: "assistant".into(),
                            source: format!("codex:response_item.{}", inner_type),
                            content: vec![ContentBlock::tool_use(name, call_id, input_val)],
                            model: current_model.clone(),
                            usage: None,
                            stop_reason: None,
                            meta,
                            raw: val,
                        });
                        idx += 1;
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let call_id = payload
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        let output = payload.get("output").cloned().unwrap_or(Value::Null);
                        let mut meta = BTreeMap::new();
                        meta.insert(
                            "codex_source".into(),
                            json!(format!("response_item.{}", inner_type)),
                        );
                        messages.push(Message {
                            index: idx,
                            timestamp: ts,
                            role: "tool".into(),
                            source: format!("codex:response_item.{}", inner_type),
                            content: vec![ContentBlock::tool_result(call_id, output, None)],
                            model: None,
                            usage: None,
                            stop_reason: None,
                            meta,
                            raw: val,
                        });
                        idx += 1;
                    }
                    "reasoning" => {
                        // Codex ships encrypted_content; we preserve it verbatim.
                        let summary_text = payload
                            .get("summary")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|x| x.get("text").and_then(|v| v.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        let mut block = ContentBlock::thinking(summary_text);
                        if let Some(enc) = payload.get("encrypted_content").and_then(|v| v.as_str())
                        {
                            block.extra.insert("encrypted_content".into(), json!(enc));
                        }
                        if let Some(c) = payload.get("content") {
                            block.extra.insert("content".into(), c.clone());
                        }
                        let mut meta = BTreeMap::new();
                        meta.insert("codex_source".into(), json!("response_item.reasoning"));
                        messages.push(Message {
                            index: idx,
                            timestamp: ts,
                            role: "assistant".into(),
                            source: "codex:response_item.reasoning".into(),
                            content: vec![block],
                            model: current_model.clone(),
                            usage: None,
                            stop_reason: None,
                            meta,
                            raw: val,
                        });
                        idx += 1;
                    }
                    _ => {
                        let mut meta = BTreeMap::new();
                        meta.insert(
                            "codex_source".into(),
                            json!(format!("response_item.{}", inner_type)),
                        );
                        messages.push(Message {
                            index: idx,
                            timestamp: ts,
                            role: "system".into(),
                            source: format!("codex:response_item.{}", inner_type),
                            content: vec![ContentBlock::other(&inner_type, payload.clone())],
                            model: None,
                            usage: None,
                            stop_reason: None,
                            meta,
                            raw: val,
                        });
                        idx += 1;
                    }
                }
            }
            _ => {
                // Unknown outer type: preserve as system message so nothing is lost.
                let mut meta = BTreeMap::new();
                meta.insert("codex_source".into(), json!(outer_type.clone()));
                messages.push(Message {
                    index: idx,
                    timestamp: ts,
                    role: "system".into(),
                    source: format!("codex:{}", outer_type),
                    content: vec![ContentBlock::other(&outer_type, payload)],
                    model: None,
                    usage: None,
                    stop_reason: None,
                    meta,
                    raw: val,
                });
                idx += 1;
            }
        }
    }

    if messages.is_empty() && session_meta_payload.is_none() {
        return None;
    }

    Some(FullSession {
        session_id: session_id.to_string(),
        provider: "codex".into(),
        cwd: cwd.to_string(),
        created_at,
        updated_at: None,
        source_path: path.display().to_string(),
        model: session_model,
        git,
        session_meta: session_meta_payload,
        messages,
    })
}

/// `response_item.message.content` is an array of blocks like
/// `{"type":"input_text","text":"..."}` or `{"type":"output_text","text":"..."}`.
fn codex_message_content(content: Option<&Value>) -> Vec<ContentBlock> {
    let Some(arr) = content.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let t = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match t {
            "input_text" | "output_text" => {
                let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let mut b = ContentBlock::text(text);
                b.extra.insert("codex_block".into(), json!(t));
                out.push(b);
            }
            other => out.push(ContentBlock::other(other, item.clone())),
        }
    }
    out
}

/// Codex token_count event payload → Usage. Shape (verified against
/// codex-cli 0.121):
/// ```text
/// payload.info.last_token_usage.{input_tokens, output_tokens,
///                                 cached_input_tokens, reasoning_output_tokens,
///                                 total_tokens}
/// payload.info.total_token_usage.{same fields}  // cumulative
/// payload.info.model_context_window
/// payload.rate_limits.{primary, secondary, plan_type, ...}
/// ```
/// We prefer `last_token_usage` (per-turn) for Usage and keep the whole
/// payload in `extra`. Older shapes with flat `info.input_tokens` are also
/// tolerated for forward-compat.
fn codex_extract_usage_from_token_count(p: &Value) -> Option<Usage> {
    let info = p.get("info")?;
    if info.is_null() {
        return None;
    }
    // Prefer last_token_usage (per-turn), then total_token_usage, then flat.
    let bucket = info
        .get("last_token_usage")
        .or_else(|| info.get("total_token_usage"))
        .unwrap_or(info);
    let g = |k: &str| bucket.get(k).and_then(|v| v.as_u64());
    // Guard: if none of the standard fields are present, don't emit Usage.
    let any = g("input_tokens").is_some()
        || g("output_tokens").is_some()
        || g("cached_input_tokens").is_some()
        || g("total_tokens").is_some()
        || g("prompt_tokens").is_some()
        || g("completion_tokens").is_some();
    if !any {
        return None;
    }
    Some(Usage {
        input_tokens: g("input_tokens").or_else(|| g("prompt_tokens")),
        output_tokens: g("output_tokens").or_else(|| g("completion_tokens")),
        cached_input_tokens: g("cached_input_tokens"),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: g("cached_input_tokens"),
        extra: p.clone(),
    })
}

// =====================================================================
// Gemini parser
// =====================================================================

/// Gemini chat JSON (verified against gemini-cli 0.35):
/// ```ignore
/// {
///   "sessionId": "...", "projectHash": "...", "kind": "main",
///   "startTime": "...", "lastUpdated": "...",
///   "messages": [
///     { "id","timestamp","type":"user|gemini","content",
///       // assistant-only extras:
///       "thoughts":[{subject,description,timestamp}...],
///       "tokens":{input,output,cached,thoughts,tool,total},
///       "model":"gemini-...",
///       "toolCalls":[{id,name,args,result,status,timestamp,...}] }
///   ]
/// }
/// ```
/// A tool call in `toolCalls` records BOTH the invocation (args) and the
/// response (result). We split it into a ContentBlock::ToolUse + a separate
/// Message of role="tool" carrying ContentBlock::ToolResult so the normalized
/// structure matches Claude/Codex.
fn parse_gemini(path: &Path, session_id: &str, cwd: &str) -> Option<FullSession> {
    let content = fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    let messages_val = val.get("messages")?.as_array()?;
    let mut messages: Vec<Message> = Vec::new();
    let mut idx: u32 = 0;
    let session_id_val = val
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(String::from);
    let created_at = val
        .get("startTime")
        .and_then(|v| v.as_str())
        .map(String::from);
    let updated_at = val
        .get("lastUpdated")
        .and_then(|v| v.as_str())
        .map(String::from);
    let mut session_model: Option<String> = None;

    // Carry top-level metadata (projectHash, kind, etc.) forward.
    let mut session_meta_obj = serde_json::Map::new();
    for k in [
        "projectHash",
        "kind",
        "startTime",
        "lastUpdated",
        "sessionId",
    ] {
        if let Some(v) = val.get(k) {
            session_meta_obj.insert(k.into(), v.clone());
        }
    }
    let session_meta = if session_meta_obj.is_empty() {
        None
    } else {
        Some(Value::Object(session_meta_obj))
    };

    for msg in messages_val {
        let msg_type = msg
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ts = msg
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(String::from);
        let msg_id = msg.get("id").and_then(|v| v.as_str()).map(String::from);

        match msg_type.as_str() {
            "user" => {
                let mut blocks = Vec::new();
                match msg.get("content") {
                    Some(Value::Array(arr)) => {
                        for item in arr {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                blocks.push(ContentBlock::text(text));
                            } else {
                                blocks.push(ContentBlock::other("unknown", item.clone()));
                            }
                        }
                    }
                    Some(Value::String(s)) => blocks.push(ContentBlock::text(s)),
                    _ => {}
                }
                let mut meta = BTreeMap::new();
                if let Some(id) = msg_id.clone() {
                    meta.insert("message_id".into(), json!(id));
                }
                messages.push(Message {
                    index: idx,
                    timestamp: ts,
                    role: "user".into(),
                    source: "gemini:user".into(),
                    content: blocks,
                    model: None,
                    usage: None,
                    stop_reason: None,
                    meta,
                    raw: msg.clone(),
                });
                idx += 1;
            }
            "gemini" => {
                let model = msg.get("model").and_then(|v| v.as_str()).map(String::from);
                if model.is_some() && session_model.is_none() {
                    session_model = model.clone();
                }
                let usage = msg.get("tokens").and_then(gemini_parse_usage);

                let mut blocks: Vec<ContentBlock> = Vec::new();

                // Structured thoughts (reasoning). Each element has
                // {subject, description, timestamp}; concatenate to a single
                // thinking block whose extra carries the structured list.
                if let Some(thoughts) = msg.get("thoughts").and_then(|v| v.as_array()) {
                    if !thoughts.is_empty() {
                        let joined = thoughts
                            .iter()
                            .filter_map(|t| {
                                let s = t.get("subject").and_then(|v| v.as_str()).unwrap_or("");
                                let d = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
                                if s.is_empty() && d.is_empty() {
                                    None
                                } else {
                                    Some(format!("**{}**\n{}", s, d))
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let mut b = ContentBlock::thinking(joined);
                        b.extra
                            .insert("thoughts".into(), Value::Array(thoughts.clone()));
                        blocks.push(b);
                    }
                }

                // Primary text content.
                match msg.get("content") {
                    Some(Value::String(s)) if !s.is_empty() => blocks.push(ContentBlock::text(s)),
                    Some(Value::Array(arr)) => {
                        for item in arr {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                blocks.push(ContentBlock::text(text));
                            } else {
                                blocks.push(ContentBlock::other("unknown", item.clone()));
                            }
                        }
                    }
                    _ => {}
                }

                // Inline tool_use blocks on the assistant message.
                let tool_calls = msg
                    .get("toolCalls")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                for tc in &tool_calls {
                    let name = tc
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let id = tc.get("id").and_then(|v| v.as_str()).map(String::from);
                    let input = tc.get("args").cloned().unwrap_or(Value::Null);
                    let mut b = ContentBlock::tool_use(name, id, input);
                    for k in [
                        "status",
                        "timestamp",
                        "resultDisplay",
                        "description",
                        "displayName",
                        "renderOutputAsMarkdown",
                    ] {
                        if let Some(v) = tc.get(k) {
                            b.extra.insert(k.into(), v.clone());
                        }
                    }
                    blocks.push(b);
                }

                let mut meta = BTreeMap::new();
                if let Some(id) = msg_id.clone() {
                    meta.insert("message_id".into(), json!(id));
                }
                messages.push(Message {
                    index: idx,
                    timestamp: ts.clone(),
                    role: "assistant".into(),
                    source: "gemini:gemini".into(),
                    content: blocks,
                    model: model.clone(),
                    usage,
                    stop_reason: None,
                    meta,
                    raw: msg.clone(),
                });
                idx += 1;

                // Separate tool-result messages matching each tool call.
                for tc in &tool_calls {
                    let id = tc.get("id").and_then(|v| v.as_str()).map(String::from);
                    let result = tc.get("result").cloned().unwrap_or(Value::Null);
                    let status = tc
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let is_err = if status == "error" || status == "failed" {
                        Some(true)
                    } else {
                        None
                    };
                    let mut b = ContentBlock::tool_result(id.clone(), result, is_err);
                    if !status.is_empty() {
                        b.extra.insert("status".into(), json!(status));
                    }
                    if let Some(ts_tc) = tc.get("timestamp") {
                        b.extra.insert("timestamp".into(), ts_tc.clone());
                    }

                    let mut m = BTreeMap::new();
                    if let Some(i) = id {
                        m.insert("tool_call_id".into(), json!(i));
                    }
                    messages.push(Message {
                        index: idx,
                        timestamp: ts.clone(),
                        role: "tool".into(),
                        source: "gemini:tool_result".into(),
                        content: vec![b],
                        model: None,
                        usage: None,
                        stop_reason: None,
                        meta: m,
                        raw: tc.clone(),
                    });
                    idx += 1;
                }
            }
            other => {
                messages.push(Message {
                    index: idx,
                    timestamp: ts,
                    role: other.into(),
                    source: format!("gemini:{}", other),
                    content: vec![ContentBlock::other(other, msg.clone())],
                    model: None,
                    usage: None,
                    stop_reason: None,
                    meta: BTreeMap::new(),
                    raw: msg.clone(),
                });
                idx += 1;
            }
        }
    }

    let effective_id = session_id_val.unwrap_or_else(|| session_id.to_string());
    Some(FullSession {
        session_id: effective_id,
        provider: "gemini".into(),
        cwd: cwd.to_string(),
        created_at,
        updated_at,
        source_path: path.display().to_string(),
        model: session_model,
        git: None,
        session_meta,
        messages,
    })
}

// =====================================================================
// Agy parser
// =====================================================================

fn parse_agy(path: &Path, session_id: &str, cwd: &str) -> Option<FullSession> {
    if !path.is_file() {
        return None;
    }
    let meta = json!({
        "note": "Agy stores its full conversation in Antigravity CLI SQLite/protobuf data. cokacdir archives the external conversation metadata only.",
        "source_size": path.metadata().ok().map(|m| m.len()),
    });
    Some(FullSession {
        session_id: session_id.to_string(),
        provider: "agy".into(),
        cwd: cwd.to_string(),
        created_at: None,
        updated_at: None,
        source_path: path.display().to_string(),
        model: None,
        git: None,
        session_meta: Some(meta.clone()),
        messages: vec![Message {
            index: 0,
            timestamp: None,
            role: "system".into(),
            source: "agy:metadata".into(),
            content: vec![ContentBlock::text(
                "Agy conversation metadata archived. Full transcript remains in Antigravity CLI storage.",
            )],
            model: None,
            usage: None,
            stop_reason: None,
            meta: BTreeMap::new(),
            raw: meta,
        }],
    })
}

/// Gemini tokens shape: `{input, output, cached, thoughts, tool, total}`.
fn gemini_parse_usage(t: &Value) -> Option<Usage> {
    let g = |k: &str| t.get(k).and_then(|v| v.as_u64());
    Some(Usage {
        input_tokens: g("input"),
        output_tokens: g("output"),
        cached_input_tokens: g("cached"),
        cache_creation_input_tokens: None,
        cache_read_input_tokens: g("cached"),
        extra: t.clone(),
    })
}

// =====================================================================
// OpenCode parser
// =====================================================================

/// OpenCode sqlite: read `session` row, then all `message` + `part` rows
/// for that session. Each part becomes one ContentBlock; parts are grouped
/// under their parent message. Every column of message/part row is preserved
/// in `raw`, so no info is lost.
fn parse_opencode(db_path: &Path, session_id: &str, cwd: &str) -> Option<FullSession> {
    if !db_path.is_file() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;

    // Session-level metadata. Column list mirrors the full `session` schema
    // observed in opencode 1.3.x so new fields (parent_id, workspace_id,
    // share_url, summary_*, revert, permission) are preserved verbatim.
    let session_row: Option<OCSession> = conn
        .query_row(
            "SELECT id, project_id, parent_id, slug, directory, title, version, \
         share_url, summary_additions, summary_deletions, summary_files, \
         summary_diffs, revert, permission, \
         time_created, time_updated, time_compacting, time_archived, workspace_id \
         FROM session WHERE id = ?1",
            rusqlite::params![session_id],
            |row| {
                Ok(OCSession {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    parent_id: row.get(2).ok(),
                    slug: row.get(3)?,
                    directory: row.get(4)?,
                    title: row.get(5)?,
                    version: row.get(6)?,
                    share_url: row.get(7).ok(),
                    summary_additions: row.get(8).ok(),
                    summary_deletions: row.get(9).ok(),
                    summary_files: row.get(10).ok(),
                    summary_diffs: row.get(11).ok(),
                    revert: row.get(12).ok(),
                    permission: row.get(13).ok(),
                    time_created: row.get(14)?,
                    time_updated: row.get(15)?,
                    time_compacting: row.get(16).ok(),
                    time_archived: row.get(17).ok(),
                    workspace_id: row.get(18).ok(),
                })
            },
        )
        .ok();

    // Load all messages for the session
    let mut msg_stmt = conn
        .prepare(
            "SELECT id, time_created, time_updated, data \
         FROM message WHERE session_id = ?1 ORDER BY time_created ASC",
        )
        .ok()?;
    let msg_rows: Vec<(String, i64, i64, String)> = msg_stmt
        .query_map(rusqlite::params![session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .ok()?
        .flatten()
        .collect();

    // Load all parts for the session, grouped by message_id
    let mut part_stmt = conn
        .prepare(
            "SELECT id, message_id, time_created, time_updated, data \
         FROM part WHERE session_id = ?1 ORDER BY time_created ASC",
        )
        .ok()?;
    let part_rows: Vec<(String, String, i64, i64, String)> = part_stmt
        .query_map(rusqlite::params![session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .ok()?
        .flatten()
        .collect();

    let mut parts_by_msg: BTreeMap<String, Vec<(String, i64, i64, String)>> = BTreeMap::new();
    for (pid, mid, tc, tu, data) in part_rows {
        parts_by_msg
            .entry(mid)
            .or_default()
            .push((pid, tc, tu, data));
    }

    let mut messages: Vec<Message> = Vec::new();
    let mut session_model: Option<String> = None;
    for (i, (msg_id, tc, _tu, data)) in msg_rows.iter().enumerate() {
        let msg_data: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        let role = msg_data
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        // Model lookup order: nested `model.modelID`, flat `modelID`, flat `model` string.
        let model = msg_data
            .get("model")
            .and_then(|m| m.get("modelID"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                msg_data
                    .get("modelID")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .or_else(|| {
                msg_data
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            });
        if model.is_some() && session_model.is_none() {
            session_model = model.clone();
        }
        let agent = msg_data
            .get("agent")
            .and_then(|v| v.as_str())
            .map(String::from);
        // Message-level token usage (present on assistant messages in addition to
        // the per-step step-finish tokens). Prefer message-level when available
        // because it aggregates the whole message.
        let msg_level_usage = msg_data.get("tokens").and_then(|t| {
            let g = |k: &str| t.get(k).and_then(|v| v.as_u64());
            let cache = t.get("cache");
            let cache_read = cache.and_then(|c| c.get("read")).and_then(|v| v.as_u64());
            let cache_write = cache.and_then(|c| c.get("write")).and_then(|v| v.as_u64());
            Some(Usage {
                input_tokens: g("input"),
                output_tokens: g("output"),
                cached_input_tokens: cache_read,
                cache_creation_input_tokens: cache_write,
                cache_read_input_tokens: cache_read,
                extra: t.clone(),
            })
        });
        let msg_level_stop = msg_data.get("finish").and_then(|f| {
            // `finish` may be a string or an object with a `reason` field.
            if let Some(s) = f.as_str() {
                Some(s.to_string())
            } else if let Some(r) = f.get("reason").and_then(|v| v.as_str()) {
                Some(r.to_string())
            } else {
                None
            }
        });

        let parts = parts_by_msg.remove(msg_id).unwrap_or_default();
        let mut blocks = Vec::new();
        let mut raw_parts: Vec<Value> = Vec::new();
        let mut total_usage: Option<Usage> = None;
        let mut stop_reason: Option<String> = None;
        for (pid, ptc, ptu, pdata) in &parts {
            let part_json: Value = serde_json::from_str(pdata).unwrap_or(Value::Null);
            raw_parts.push(json!({
                "id": pid, "time_created": ptc, "time_updated": ptu,
                "data": part_json.clone(),
            }));
            let ptype = part_json
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match ptype.as_str() {
                "text" => {
                    let text = part_json.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    blocks.push(ContentBlock::text(text));
                }
                "reasoning" => {
                    let text = part_json.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let mut b = ContentBlock::thinking(text);
                    if let Some(m) = part_json.get("metadata") {
                        b.extra.insert("metadata".into(), m.clone());
                    }
                    blocks.push(b);
                }
                "tool" => {
                    let tool_name = part_json
                        .get("tool")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    // OpenCode: the tool-use identifier is `callID` (capital ID),
                    // not `id`. Fall back to the part row id only if callID is
                    // absent so cross-referencing with tool_result still works.
                    let tool_id = part_json
                        .get("callID")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .or_else(|| {
                            part_json
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                        })
                        .or(Some(pid.clone()));
                    let state = part_json.get("state").cloned().unwrap_or(Value::Null);
                    let status = state
                        .get("status")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let input = state.get("input").cloned().unwrap_or(Value::Null);
                    let output = state.get("output").cloned();
                    // On success `error` is usually absent. Treat present and
                    // non-empty as an error marker; explicit status=="error"
                    // also counts.
                    let is_err = match state.get("error") {
                        Some(e) if !e.is_null() && !(e.is_string() && e.as_str() == Some("")) => {
                            Some(true)
                        }
                        _ if status.as_deref() == Some("error") => Some(true),
                        _ => None,
                    };
                    let mut b = ContentBlock::tool_use(tool_name, tool_id, input);
                    b.tool_output = output;
                    b.is_error = is_err;
                    if let Some(s) = status {
                        b.extra.insert("status".into(), json!(s));
                    }
                    b.extra.insert("state".into(), state);
                    if let Some(m) = part_json.get("metadata") {
                        b.extra.insert("part_metadata".into(), m.clone());
                    }
                    blocks.push(b);
                }
                "step-finish" => {
                    if let Some(reason) = part_json.get("reason").and_then(|v| v.as_str()) {
                        stop_reason = Some(reason.to_string());
                    }
                    if let Some(tokens) = part_json.get("tokens") {
                        // OpenCode tokens shape:
                        // {total, input, output, reasoning, cache: {read, write}}
                        // We capture all of these into Usage; the full object
                        // also goes into `extra` so nothing is lost.
                        let g = |k: &str| tokens.get(k).and_then(|v| v.as_u64());
                        let cache = tokens.get("cache");
                        let cache_read = cache.and_then(|c| c.get("read")).and_then(|v| v.as_u64());
                        let cache_write =
                            cache.and_then(|c| c.get("write")).and_then(|v| v.as_u64());
                        total_usage = Some(Usage {
                            input_tokens: g("input"),
                            output_tokens: g("output"),
                            cached_input_tokens: cache_read,
                            cache_creation_input_tokens: cache_write,
                            cache_read_input_tokens: cache_read,
                            extra: tokens.clone(),
                        });
                    }
                    blocks.push(ContentBlock::other("step-finish", part_json.clone()));
                }
                "step-start" => {
                    blocks.push(ContentBlock::other("step-start", part_json.clone()));
                }
                "patch" => {
                    // Records the result of an apply_patch tool. Preserve the
                    // hash and file list as structured fields; raw holds the
                    // entire record.
                    let mut b = ContentBlock::other("patch", part_json.clone());
                    if let Some(h) = part_json.get("hash") {
                        b.extra.insert("hash".into(), h.clone());
                    }
                    if let Some(f) = part_json.get("files") {
                        b.extra.insert("files".into(), f.clone());
                    }
                    blocks.push(b);
                }
                _ => {
                    blocks.push(ContentBlock::other(&ptype, part_json.clone()));
                }
            }
        }

        let mut meta = BTreeMap::new();
        meta.insert("message_id".into(), json!(msg_id));
        meta.insert("time_created".into(), json!(tc));
        if let Some(a) = agent {
            meta.insert("agent".into(), json!(a));
        }
        // Extra message-level fields observed in practice.
        for k in ["parentID", "providerID", "modelID", "mode", "path", "cost"] {
            if let Some(v) = msg_data.get(k) {
                meta.insert(k.into(), v.clone());
            }
        }
        if let Some(t) = msg_data.get("time") {
            meta.insert("time".into(), t.clone());
        }
        if let Some(s) = msg_data.get("summary") {
            meta.insert("summary".into(), s.clone());
        }

        // Prefer message-level tokens if present; otherwise the aggregated
        // step-finish usage.
        let effective_usage = msg_level_usage.or(total_usage);
        // Prefer message-level finish reason; otherwise the last step-finish reason.
        let effective_stop = msg_level_stop.or(stop_reason);

        // Synthesize a millisecond timestamp string
        let ts = chrono::DateTime::from_timestamp_millis(*tc).map(|d| d.to_rfc3339());

        let raw = json!({
            "message_row": { "id": msg_id, "time_created": tc, "data": msg_data },
            "parts": raw_parts,
        });

        messages.push(Message {
            index: i as u32,
            timestamp: ts,
            role,
            source: "opencode:message".into(),
            content: blocks,
            model,
            usage: effective_usage,
            stop_reason: effective_stop,
            meta,
            raw,
        });
    }

    let session_meta_val = session_row
        .as_ref()
        .map(|s| serde_json::to_value(s).unwrap_or(Value::Null));
    let created_at = session_row
        .as_ref()
        .and_then(|s| chrono::DateTime::from_timestamp_millis(s.time_created))
        .map(|d| d.to_rfc3339());
    let updated_at = session_row
        .as_ref()
        .and_then(|s| chrono::DateTime::from_timestamp_millis(s.time_updated))
        .map(|d| d.to_rfc3339());

    Some(FullSession {
        session_id: session_id.to_string(),
        provider: "opencode".into(),
        cwd: cwd.to_string(),
        created_at,
        updated_at,
        source_path: db_path.display().to_string(),
        model: session_model,
        git: None,
        session_meta: session_meta_val,
        messages,
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct OCSession {
    id: String,
    project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    slug: String,
    directory: String,
    title: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    share_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_additions: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_deletions: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_files: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_diffs: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revert: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission: Option<String>,
    time_created: i64,
    time_updated: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_compacting: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_archived: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
}
