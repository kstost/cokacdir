use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub fn resumable_session_id(session_id: Option<&str>) -> Option<&str> {
    let sessions_dir = gjc_agent_dir()?.join("sessions");
    resumable_session_id_in(session_id, &sessions_dir)
}

fn gjc_agent_dir() -> Option<PathBuf> {
    std::env::var_os("GJC_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".gjc").join("agent")))
}

pub(crate) fn session_exists_in(sessions_dir: &Path, session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    walk_sessions(sessions_dir, session_id, 0)
}

pub(crate) fn resumable_session_id_in<'a>(
    session_id: Option<&'a str>,
    sessions_dir: &Path,
) -> Option<&'a str> {
    let sid = session_id?;
    session_exists_in(sessions_dir, sid).then_some(sid)
}

fn walk_sessions(path: &Path, session_id: &str, depth: usize) -> bool {
    if depth > 6 {
        return false;
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        let entry_path = entry.path();
        if file_type.is_dir() {
            if walk_sessions(&entry_path, session_id, depth + 1) {
                return true;
            }
        } else if file_type.is_file() && file_matches_session(&entry_path, session_id) {
            return true;
        }
    }
    false
}

fn file_matches_session(path: &Path, session_id: &str) -> bool {
    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
        return false;
    }
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.contains(session_id))
        .unwrap_or(false)
    {
        return true;
    }
    first_line_session_id(path)
        .map(|id| id == session_id)
        .unwrap_or(false)
}

fn first_line_session_id(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    let value = serde_json::from_str::<Value>(line.trim()).ok()?;
    (value.get("type").and_then(Value::as_str) == Some("session"))
        .then(|| value.get("id").and_then(Value::as_str).map(str::to_string))
        .flatten()
}
