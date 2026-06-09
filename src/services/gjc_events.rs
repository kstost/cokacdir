use serde_json::Value;

use crate::services::claude::StreamMessage;

pub fn parse_gjc_event(json: &Value) -> Vec<StreamMessage> {
    let mut out = Vec::new();
    if json.get("type").and_then(|v| v.as_str()) == Some("session") {
        if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
            out.push(StreamMessage::Init {
                session_id: id.to_string(),
            });
        }
    }
    if let Some(message) = json
        .get("error")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("errorMessage").and_then(|v| v.as_str()))
    {
        out.push(StreamMessage::Error {
            message: message.to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        });
    }
    if let Some(text) = assistant_delta_text(json) {
        out.push(StreamMessage::Text { content: text });
    } else if should_parse_full_message(json) {
        if let Some(text) = assistant_text(json) {
            out.push(StreamMessage::Text { content: text });
        }
    }
    out
}

fn assistant_delta_text(json: &Value) -> Option<String> {
    let event = json.get("assistantMessageEvent")?;
    if event.get("type").and_then(|v| v.as_str()) != Some("text_delta") {
        return None;
    }
    event
        .get("delta")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn should_parse_full_message(json: &Value) -> bool {
    match json.get("type").and_then(|v| v.as_str()) {
        Some("message") => true,
        None => true,
        _ => false,
    }
}

fn assistant_text(json: &Value) -> Option<String> {
    let message = json.get("message").unwrap_or(json);
    if message.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return None;
    }
    let content = message.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let text = content
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}
