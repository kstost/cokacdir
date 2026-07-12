//! Messenger Bridge: translates external messengers into Telegram Bot API format.
//!
//! Architecture:
//!   External Messenger ←→ MessengerBackend ←→ TG Bot API Proxy ←→ telegram.rs (unchanged)
//!
//! The proxy runs a local HTTP server that implements the Telegram Bot API subset
//! used by telegram.rs. teloxide connects to this proxy instead of the real
//! Telegram API, enabling any messenger to reuse the existing telegram.rs logic
//! without modification.
//!
//! Discord and Slack bots are launched via `--ccserver` (auto-detected by token format).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use slack_morphism::prelude::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

// ============================================================
// Common types
// ============================================================

/// Bot identity information (returned by getMe)
pub struct BotInfo {
    pub id: i64,
    pub username: String,
    pub first_name: String,
}

/// An incoming message from the external messenger
pub struct IncomingMessage {
    /// Mapped chat ID (must be stable for the same chat/channel)
    pub chat_id: i64,
    /// Mapped message ID (unique within the chat)
    pub message_id: i32,
    /// Sender's user ID
    pub from_id: u64,
    /// Sender's display name
    pub from_first_name: String,
    /// Sender's username (optional)
    pub from_username: Option<String>,
    /// Text content
    pub text: Option<String>,
    /// Whether this is a group/channel (vs DM)
    pub is_group: bool,
    /// Group/channel title (required if is_group)
    pub group_title: Option<String>,
    /// File attachment
    pub document: Option<FileAttachment>,
    /// Photo attachments
    pub photo: Option<Vec<PhotoAttachment>>,
    /// Caption for media
    pub caption: Option<String>,
    /// Album/media-group identifier shared by all attachments of one upstream message.
    /// Set by Discord/Slack fan-out so the downstream prefix check can recognize
    /// follow-up attachments whose captions are dropped to None on i>=1.
    pub media_group_id: Option<String>,
}

pub struct FileAttachment {
    pub file_id: String,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<u64>,
}

pub struct PhotoAttachment {
    pub file_id: String,
    pub width: u32,
    pub height: u32,
    pub file_size: Option<u64>,
}

/// Result of sending a message through the backend
pub struct SentMessage {
    pub message_id: i32,
    pub chat_id: i64,
    pub text: Option<String>,
}

/// File info for downloads
pub struct FileInfo {
    pub file_id: String,
    pub file_path: String,
    pub file_size: Option<u64>,
}

/// Reason `run_bridge` returned. `Fatal` mirrors the prior
/// `process::exit(1)` signal so single-bot deployments keep emitting
/// exit code 1 to supervisors (systemd, docker), while multi-bot
/// deployments can keep their healthy bots running and propagate the
/// non-zero exit only after every bot's task has finished.
pub enum BridgeExit {
    /// Bot side exited (e.g. Telegram `PollingExit::Fatal` against the
    /// local proxy, or a future graceful-shutdown path). The backend
    /// listener is aborted by `run_bridge` before this returns.
    Graceful,
    /// Backend listener died (token revoked, persistent gateway
    /// disconnect, panic) or initialization failed. The local proxy
    /// task is aborted before this returns. The caller is expected to
    /// surface this as a non-zero process exit at the outermost scope
    /// it knows is safe — i.e. only after no other in-process bot is
    /// still serving traffic.
    Fatal,
}

// ============================================================
// MessengerBackend trait
// ============================================================

#[async_trait]
pub trait MessengerBackend: Send + Sync {
    /// Backend name (e.g., "discord", "slack", "console")
    fn name(&self) -> &str;

    /// Initialize the backend and return bot info
    async fn init(&mut self) -> Result<BotInfo, String>;

    /// Start listening for incoming messages, sending them through `tx`.
    ///
    /// Implementations spawn a long-lived background task and return its
    /// `JoinHandle<()>` immediately. The handle resolves only when the
    /// backend's listener dies — gateway disconnect with no recovery,
    /// token revocation, or internal panic. `run_bridge` watches the
    /// handle alongside `run_bot` and exits cokacdir with a clear error
    /// when it completes, so a silently-dead backend (proxy still up,
    /// no messages flowing) cannot masquerade as a healthy bot.
    async fn start(
        &self,
        tx: mpsc::Sender<IncomingMessage>,
    ) -> Result<tokio::task::JoinHandle<()>, String>;

    /// Send a text message to a chat
    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<SentMessage, String>;

    /// Edit an existing message
    async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i32,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<SentMessage, String>;

    /// Delete a message
    async fn delete_message(&self, chat_id: i64, message_id: i32) -> Result<bool, String>;

    /// Send a chat action such as "typing".
    async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<bool, String>;

    /// Send a file/document
    async fn send_document(
        &self,
        chat_id: i64,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
    ) -> Result<SentMessage, String>;

    /// Get file info for downloading
    async fn get_file(&self, file_id: &str) -> Result<FileInfo, String>;

    /// Download file data by file_path (returned from get_file)
    async fn get_file_data(&self, file_path: &str) -> Result<Vec<u8>, String>;
}

// ============================================================
// HTTP helpers
// ============================================================

struct HttpRequest {
    path: String,
    content_type: String,
    body: Vec<u8>,
}

/// Maximum request body size (100 MB — covers Telegram's 50 MB file upload limit)
const MAX_BODY_SIZE: usize = 100 * 1024 * 1024;
/// Inbound files are ultimately consumed by telegram.rs's getFile-compatible
/// 20 MiB path. Enforce the same bound while reading Discord/Slack responses,
/// including responses without Content-Length, so an unknown-size file cannot
/// exhaust the bridge process before telegram.rs gets a chance to reject it.
const BRIDGE_FILE_DOWNLOAD_MAX_BYTES: usize = 20 * 1024 * 1024;
const BRIDGE_FILE_DOWNLOAD_TIMEOUT_SECS: u64 = 120;

fn bridge_api_client(timeout_secs: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(timeout_secs))
        // Certificate verification remains enabled; no danger-accept option
        // is used when selecting the native TLS backend.
        .build()
        .map_err(|e| format!("Failed to create messenger API client: {e}"))
}

fn bridge_file_download_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        // Covers both headers and the streamed response body. This prevents a
        // stalled or indefinitely trickling CDN peer from pinning a chat
        // worker forever.
        .timeout(std::time::Duration::from_secs(
            BRIDGE_FILE_DOWNLOAD_TIMEOUT_SECS,
        ))
        .build()
        .map_err(|e| format!("Failed to create file download client: {e}"))
}

async fn read_download_response_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    if !response.status().is_success() {
        return Err(format!("{} failed: HTTP {}", label, response.status()));
    }
    if let Some(length) = response.content_length() {
        if length > max_bytes as u64 {
            return Err(format!(
                "{} too large ({} bytes; maximum {} bytes)",
                label, length, max_bytes
            ));
        }
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("{} read failed: {}", label, e))?
    {
        let next_len = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| format!("{} size overflow", label))?;
        if next_len > max_bytes {
            return Err(format!(
                "{} too large (more than {} bytes)",
                label, max_bytes
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_http_line_limited(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    limit: usize,
) -> Result<Option<String>, ()> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|_| ())?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map(|index| index + 1).unwrap_or(available.len());
        if bytes.len().checked_add(take).is_none_or(|len| len > limit) {
            return Err(());
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }
    String::from_utf8(bytes).map(Some).map_err(|_| ())
}

async fn read_http_request(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Option<HttpRequest> {
    const MAX_HEADER_LINE: usize = 16 * 1024;
    const MAX_HEADER_BYTES: usize = 64 * 1024;

    let request_line = read_http_line_limited(reader, MAX_HEADER_LINE)
        .await
        .ok()??;

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() != 3 || !parts[2].starts_with("HTTP/1.") {
        return None;
    }
    let path = parts[1].to_string();

    let mut content_length: Option<usize> = None;
    let mut content_type = String::new();
    let mut chunked = false;
    let mut header_bytes = request_line.len();
    loop {
        let line = read_http_line_limited(reader, MAX_HEADER_LINE)
            .await
            .ok()??;
        header_bytes = header_bytes.checked_add(line.len())?;
        if header_bytes > MAX_HEADER_BYTES {
            return None;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_lowercase();
        if let Some(val) = lower.strip_prefix("content-length:") {
            let parsed = val.trim().parse::<usize>().ok()?;
            if content_length.is_some_and(|previous| previous != parsed) {
                return None;
            }
            content_length = Some(parsed);
        } else if lower.starts_with("content-type:") {
            content_type = trimmed["content-type:".len()..].trim().to_string();
        } else if lower.starts_with("transfer-encoding:") {
            let encodings = lower["transfer-encoding:".len()..]
                .split(',')
                .map(str::trim)
                .collect::<Vec<_>>();
            if encodings.as_slice() != ["chunked"] {
                return None;
            }
            chunked = true;
        }
    }
    if chunked && content_length.is_some() {
        // Reject ambiguous framing instead of choosing one interpretation and
        // leaving bytes to be parsed as the next request.
        return None;
    }

    let body = if chunked {
        // Read chunked transfer encoding
        let mut body = Vec::new();
        loop {
            let size_line = read_http_line_limited(reader, MAX_HEADER_LINE)
                .await
                .ok()??;
            let size_text = size_line.trim().split(';').next().unwrap_or("");
            let chunk_size = match usize::from_str_radix(size_text, 16) {
                Ok(s) => s,
                Err(_) => return None,
            };
            if chunk_size == 0 {
                let mut trailer_bytes = 0usize;
                loop {
                    let trailer = read_http_line_limited(reader, MAX_HEADER_LINE)
                        .await
                        .ok()??;
                    trailer_bytes = trailer_bytes.checked_add(trailer.len())?;
                    if trailer_bytes > MAX_HEADER_BYTES {
                        return None;
                    }
                    if trailer.trim().is_empty() {
                        break;
                    }
                }
                break;
            }
            if body
                .len()
                .checked_add(chunk_size)
                .is_none_or(|len| len > MAX_BODY_SIZE)
            {
                return None;
            }
            let mut chunk = vec![0u8; chunk_size];
            if reader.read_exact(&mut chunk).await.is_err() {
                return None;
            }
            body.extend_from_slice(&chunk);
            let trailing = read_http_line_limited(reader, 2).await.ok()??;
            if trailing != "\r\n" && trailing != "\n" {
                return None;
            }
        }
        body
    } else {
        let cl = content_length.unwrap_or(0);
        if cl > MAX_BODY_SIZE {
            return None;
        }
        let mut body = vec![0u8; cl];
        if cl > 0 {
            if reader.read_exact(&mut body).await.is_err() {
                return None;
            }
        }
        body
    };

    Some(HttpRequest {
        path,
        content_type,
        body,
    })
}

fn http_json_response(status: u16, body: &[u8]) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        status, status_text, body.len()
    );
    let mut resp = header.into_bytes();
    resp.extend_from_slice(body);
    resp
}

fn http_file_response(data: &[u8], content_type: &str) -> Vec<u8> {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        content_type,
        data.len()
    );
    let mut resp = header.into_bytes();
    resp.extend_from_slice(data);
    resp
}

// ============================================================
// Multipart / URL-encoded parsers
// ============================================================

/// Find the first occurrence of `needle` in `haystack` (byte-level search).
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_multipart_to_json(content_type: &str, body: &[u8]) -> Value {
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"');
    if boundary.is_empty() {
        return json!({});
    }

    let mut result = serde_json::Map::new();
    let delim = format!("--{}", boundary);
    let delim_bytes = delim.as_bytes();
    let sep = b"\r\n\r\n";

    // Walk through the body finding delimiter-separated parts
    let mut search_from = 0;
    let mut parts: Vec<(usize, usize)> = Vec::new(); // (start, end) of each part body

    while let Some(d_pos) = find_bytes(&body[search_from..], delim_bytes) {
        let abs_d = search_from + d_pos;
        // Content starts after delimiter + \r\n
        let after_delim = abs_d + delim_bytes.len();
        if after_delim >= body.len() {
            break;
        }
        // Check for closing "--" (end marker)
        if body[after_delim..].starts_with(b"--") {
            break;
        }
        // Skip \r\n after delimiter
        let content_start = if body[after_delim..].starts_with(b"\r\n") {
            after_delim + 2
        } else {
            after_delim
        };

        // Find next delimiter to determine part end
        if let Some(next_d) = find_bytes(&body[content_start..], delim_bytes) {
            let part_end = content_start + next_d;
            // Strip trailing \r\n before delimiter
            let trimmed_end = if part_end >= 2 && &body[part_end - 2..part_end] == b"\r\n" {
                part_end - 2
            } else {
                part_end
            };
            parts.push((content_start, trimmed_end));
            search_from = content_start + next_d;
        } else {
            // Last part (no next delimiter found)
            parts.push((content_start, body.len()));
            break;
        }
    }

    for &(start, end) in &parts {
        if start >= end {
            continue;
        }
        let part = &body[start..end];

        // Split headers from content at \r\n\r\n
        let header_end = match find_bytes(part, sep) {
            Some(pos) => pos,
            None => continue,
        };

        let header_str = std::str::from_utf8(&part[..header_end]).unwrap_or("");
        let content = &part[header_end + sep.len()..];

        let name = extract_header_param(header_str, "name");
        let filename = extract_header_param(header_str, "filename");

        if let Some(name) = name {
            if let Some(fname) = filename {
                // File field: encode as base64 to preserve binary content
                use base64::{engine::general_purpose::STANDARD, Engine as _};
                result.insert("_filename".to_string(), json!(fname));
                result.insert(
                    "_file_data_b64".to_string(),
                    json!(STANDARD.encode(content)),
                );
            } else {
                // Text field
                let text = std::str::from_utf8(content).unwrap_or("");
                result.insert(name, json!(text));
            }
        }
    }

    // Convert numeric string fields to numbers
    for key in &["chat_id", "message_id", "offset", "limit", "timeout"] {
        if let Some(Value::String(s)) = result.get(*key) {
            if let Ok(n) = s.parse::<i64>() {
                result.insert(key.to_string(), json!(n));
            }
        }
    }

    Value::Object(result)
}

fn extract_header_param(headers: &str, param: &str) -> Option<String> {
    let search = format!("{}=\"", param);
    let start = headers.find(&search)?;
    let rest = &headers[start + search.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn parse_urlencoded_to_json(body: &[u8]) -> Value {
    let body_str = String::from_utf8_lossy(body);
    let mut result = serde_json::Map::new();
    for pair in body_str.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            let decoded = simple_url_decode(value);
            if let Ok(n) = decoded.parse::<i64>() {
                result.insert(key.to_string(), json!(n));
            } else if decoded == "true" {
                result.insert(key.to_string(), json!(true));
            } else if decoded == "false" {
                result.insert(key.to_string(), json!(false));
            } else {
                result.insert(key.to_string(), json!(decoded));
            }
        }
    }
    Value::Object(result)
}

fn simple_url_decode(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hex: Vec<u8> = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(decoded) = u8::from_str_radix(std::str::from_utf8(&hex).unwrap_or(""), 16)
                {
                    bytes.push(decoded);
                    continue;
                }
            }
            // Malformed percent-encoding: keep original
            bytes.push(b'%');
            bytes.extend_from_slice(&hex);
        } else if b == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).to_string())
}

// ============================================================
// Proxy State
// ============================================================

struct ProxyState {
    backend: Arc<dyn MessengerBackend>,
    bot_info: BotInfo,
    update_rx: Mutex<mpsc::Receiver<IncomingMessage>>,
    update_id_counter: AtomicI64,
    /// Expected bot token — requests with a mismatched token are rejected.
    expected_token: String,
}

impl ProxyState {
    fn next_update_id(&self) -> i64 {
        self.update_id_counter.fetch_add(1, Ordering::Relaxed)
    }
}

// ============================================================
// Proxy Server
// ============================================================

async fn run_proxy_server(state: Arc<ProxyState>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = state.clone();
                tokio::spawn(handle_connection(state, stream));
            }
            Err(e) => {
                eprintln!("  [bridge] accept error: {}", e);
            }
        }
    }
}

async fn handle_connection(state: Arc<ProxyState>, stream: tokio::net::TcpStream) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        let req = match tokio::time::timeout(
            std::time::Duration::from_secs(120),
            read_http_request(&mut reader),
        )
        .await
        {
            Ok(Some(request)) => request,
            Ok(None) | Err(_) => break,
        };
        let resp_bytes = route_request(&state, &req).await;
        if write_half.write_all(&resp_bytes).await.is_err() {
            break;
        }
        if write_half.flush().await.is_err() {
            break;
        }
    }
}

async fn route_request(state: &ProxyState, req: &HttpRequest) -> Vec<u8> {
    let path = &req.path;
    let unauthorized = json!({"ok": false, "description": "Unauthorized"});

    // File download: /file/bot<token>/<file_path>
    if path.starts_with("/file/bot") {
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        // parts = ["", "file", "bot<token>", "<file_path>"]
        if parts.len() >= 4 {
            // Verify token: "bot<token>" → strip "bot" prefix
            let token = parts[2].strip_prefix("bot").unwrap_or("");
            if !tokens_eq_constant_time(token, &state.expected_token) {
                return http_json_response(401, unauthorized.to_string().as_bytes());
            }
            return handle_file_download(state, parts[3]).await;
        }
        let err = json!({"ok": false, "description": "Invalid file path"});
        return http_json_response(400, err.to_string().as_bytes());
    }

    // API method: /bot<token>/<method>
    let (token, method) = extract_token_and_method(path);

    // Verify token
    if !tokens_eq_constant_time(token, &state.expected_token) {
        return http_json_response(401, unauthorized.to_string().as_bytes());
    }

    if method.is_empty() {
        let err = json!({"ok": false, "description": "Unknown method"});
        return http_json_response(404, err.to_string().as_bytes());
    }

    let body_json = parse_request_body(&req.content_type, &req.body);
    if method == "SendDocument" || method == "sendDocument" {
        eprintln!(
            "  [bridge-proxy] SendDocument: content_type={:?}, body_len={}, parsed_keys={:?}",
            req.content_type,
            req.body.len(),
            body_json.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );
    }
    let result = handle_api_method(state, method, &body_json).await;
    http_json_response(200, result.to_string().as_bytes())
}

/// Extract token and method from path like `/bot<token>/sendMessage` → `("<token>", "sendMessage")`
fn extract_token_and_method(path: &str) -> (&str, &str) {
    let after_bot = match path.find("/bot") {
        Some(pos) => &path[pos + 4..],
        None => return ("", ""),
    };
    match after_bot.find('/') {
        Some(pos) => (&after_bot[..pos], &after_bot[pos + 1..]),
        None => (after_bot, ""),
    }
}

fn parse_request_body(content_type: &str, body: &[u8]) -> Value {
    if content_type.contains("multipart/form-data") {
        parse_multipart_to_json(content_type, body)
    } else if content_type.contains("application/x-www-form-urlencoded") {
        parse_urlencoded_to_json(body)
    } else {
        // Default: try JSON
        serde_json::from_slice(body).unwrap_or(json!({}))
    }
}

// ============================================================
// API Method Router
// ============================================================

async fn handle_api_method(state: &ProxyState, method: &str, body: &Value) -> Value {
    // teloxide 0.17 uses the Rust payload type name (PascalCase) as the
    // endpoint name. Keep the canonical Bot API camelCase aliases too for
    // compatibility with direct clients.
    match method {
        "GetMe" | "getMe" => handle_get_me(state),
        "SetMyCommands" | "setMyCommands" => json!({"ok": true, "result": true}),
        "GetUpdates" | "getUpdates" => handle_get_updates(state, body).await,
        "SendMessage" | "sendMessage" => handle_send_message(state, body).await,
        // Telegram Bot API 10.1 Rich Messages are not implemented by the
        // Discord/Slack backends. Return an explicit API failure so
        // telegram.rs takes its established classic send/edit fallback. The
        // previous catch-all success response made callers believe a final
        // response was delivered when nothing had been sent.
        "SendRichMessage" | "sendRichMessage" | "SendRichMessageDraft" | "sendRichMessageDraft" => {
            json!({"ok": false, "description": "Rich Messages are not supported by this bridge"})
        }
        "EditMessageText" | "editMessageText" => handle_edit_message(state, body).await,
        "DeleteMessage" | "deleteMessage" => handle_delete_message(state, body).await,
        "SendDocument" | "sendDocument" => handle_send_document(state, body).await,
        "GetFile" | "getFile" => handle_get_file(state, body).await,
        "SendChatAction" | "sendChatAction" => handle_send_chat_action(state, body).await,
        "GetWebhookInfo" | "getWebhookInfo" => {
            json!({"ok": true, "result": {"url": "", "has_custom_certificate": false, "pending_update_count": 0}})
        }
        _ => json!({"ok": true, "result": true}),
    }
}

// ============================================================
// Endpoint Handlers
// ============================================================

fn handle_get_me(state: &ProxyState) -> Value {
    json!({
        "ok": true,
        "result": {
            "id": state.bot_info.id,
            "is_bot": true,
            "first_name": state.bot_info.first_name,
            "username": state.bot_info.username,
            "can_join_groups": true,
            "can_read_all_group_messages": true,
            "supports_inline_queries": false,
            "can_connect_to_business": false,
            "has_main_web_app": false,
        }
    })
}

async fn handle_get_updates(state: &ProxyState, body: &Value) -> Value {
    let offset = body.get("offset").and_then(|v| v.as_i64()).unwrap_or(0);
    let timeout_secs = body.get("timeout").and_then(|v| v.as_u64()).unwrap_or(0);
    let limit = body.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;

    // Negative offset: flush/discard all pending messages (startup sequence)
    if offset < 0 {
        let mut rx = state.update_rx.lock().await;
        while rx.try_recv().is_ok() {}
        return json!({"ok": true, "result": []});
    }

    // limit=0 is a confirmation/flush request — return empty
    if limit == 0 {
        return json!({"ok": true, "result": []});
    }

    let mut updates = Vec::new();
    let mut rx = state.update_rx.lock().await;

    // Drain immediately available messages
    while updates.len() < limit {
        match rx.try_recv() {
            Ok(msg) => updates.push(incoming_to_update(state, msg)),
            Err(_) => break,
        }
    }

    // If nothing yet and timeout > 0, wait for the first message
    if updates.is_empty() && timeout_secs > 0 {
        let duration = std::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(duration, rx.recv()).await {
            Ok(Some(msg)) => {
                updates.push(incoming_to_update(state, msg));
                // Drain any more that arrived while we waited
                while updates.len() < limit {
                    match rx.try_recv() {
                        Ok(msg) => updates.push(incoming_to_update(state, msg)),
                        Err(_) => break,
                    }
                }
            }
            _ => {} // Timeout or channel closed
        }
    }

    json!({"ok": true, "result": updates})
}

async fn handle_send_message(state: &ProxyState, body: &Value) -> Value {
    let chat_id = body.get("chat_id").and_then(|v| v.as_i64()).unwrap_or(0);
    let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let parse_mode = body.get("parse_mode").and_then(|v| v.as_str());

    match state.backend.send_message(chat_id, text, parse_mode).await {
        Ok(sent) => json!({
            "ok": true,
            "result": make_bot_message_json(state, sent.message_id, chat_id,
                sent.text.as_deref().unwrap_or(text))
        }),
        Err(e) => json!({"ok": false, "description": e}),
    }
}

async fn handle_edit_message(state: &ProxyState, body: &Value) -> Value {
    if body.get("rich_message").is_some() {
        // A raw Rich Message edit has no classic `text` field. Passing it
        // through would edit the upstream Discord/Slack message to an empty
        // string and then report success, losing the final response instead
        // of triggering telegram.rs's classic fallback.
        return json!({
            "ok": false,
            "description": "Rich Message edits are not supported by this bridge"
        });
    }
    let chat_id = body.get("chat_id").and_then(|v| v.as_i64()).unwrap_or(0);
    let message_id = body.get("message_id").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let parse_mode = body.get("parse_mode").and_then(|v| v.as_str());

    match state
        .backend
        .edit_message(chat_id, message_id, text, parse_mode)
        .await
    {
        Ok(_) => json!({
            "ok": true,
            "result": make_bot_message_json(state, message_id, chat_id, text)
        }),
        Err(e) => json!({"ok": false, "description": e}),
    }
}

async fn handle_delete_message(state: &ProxyState, body: &Value) -> Value {
    let chat_id = body.get("chat_id").and_then(|v| v.as_i64()).unwrap_or(0);
    let message_id = body.get("message_id").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

    match state.backend.delete_message(chat_id, message_id).await {
        Ok(_) => json!({"ok": true, "result": true}),
        Err(e) => json!({"ok": false, "description": e}),
    }
}

async fn handle_send_chat_action(state: &ProxyState, body: &Value) -> Value {
    let chat_id = body.get("chat_id").and_then(|v| v.as_i64()).unwrap_or(0);
    let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("");

    match state.backend.send_chat_action(chat_id, action).await {
        Ok(_) => json!({"ok": true, "result": true}),
        Err(e) => json!({"ok": false, "description": e}),
    }
}

async fn handle_send_document(state: &ProxyState, body: &Value) -> Value {
    let chat_id = body.get("chat_id").and_then(|v| v.as_i64()).unwrap_or(0);
    eprintln!(
        "  [bridge-proxy] send_document: chat_id={}, has_filename={}, has_file_data={}",
        chat_id,
        body.get("_filename").is_some(),
        body.get("_file_data_b64").is_some()
    );
    let caption = body.get("caption").and_then(|v| v.as_str());
    let filename = body
        .get("_filename")
        .and_then(|v| v.as_str())
        .unwrap_or("file");
    let file_data = body
        .get("_file_data_b64")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            STANDARD.decode(s).ok()
        })
        .unwrap_or_default();

    match state
        .backend
        .send_document(chat_id, &file_data, filename, caption)
        .await
    {
        Ok(sent) => json!({
            "ok": true,
            "result": make_bot_message_json(state, sent.message_id, chat_id,
                caption.unwrap_or(""))
        }),
        Err(e) => json!({"ok": false, "description": e}),
    }
}

async fn handle_get_file(state: &ProxyState, body: &Value) -> Value {
    let file_id = body.get("file_id").and_then(|v| v.as_str()).unwrap_or("");

    match state.backend.get_file(file_id).await {
        Ok(info) => json!({
            "ok": true,
            "result": {
                "file_id": info.file_id,
                "file_unique_id": info.file_id,
                "file_size": info.file_size,
                "file_path": info.file_path,
            }
        }),
        Err(e) => json!({"ok": false, "description": e}),
    }
}

async fn handle_file_download(state: &ProxyState, file_path: &str) -> Vec<u8> {
    match state.backend.get_file_data(file_path).await {
        Ok(data) => {
            let ct = if file_path.ends_with(".jpg") || file_path.ends_with(".jpeg") {
                "image/jpeg"
            } else if file_path.ends_with(".png") {
                "image/png"
            } else if file_path.ends_with(".pdf") {
                "application/pdf"
            } else {
                "application/octet-stream"
            };
            http_file_response(&data, ct)
        }
        Err(e) => {
            let err = json!({"ok": false, "description": e});
            http_json_response(404, err.to_string().as_bytes())
        }
    }
}

#[cfg(test)]
mod rich_message_fallback_tests {
    use super::*;
    use teloxide::payloads::GetUpdatesSetters;
    use teloxide::prelude::Requester;

    fn proxy_state() -> ProxyState {
        let (_tx, rx) = mpsc::channel(1);
        ProxyState {
            backend: Arc::new(ConsoleBackend::new()),
            bot_info: BotInfo {
                id: 1,
                username: "bridge-test".to_string(),
                first_name: "Bridge Test".to_string(),
            },
            update_rx: Mutex::new(rx),
            update_id_counter: AtomicI64::new(1),
            expected_token: "test-token".to_string(),
        }
    }

    #[tokio::test]
    async fn unsupported_rich_send_returns_api_failure_for_classic_fallback() {
        let state = proxy_state();
        let result = handle_api_method(
            &state,
            "sendRichMessage",
            &json!({"chat_id": 1, "rich_message": {"markdown": "hello"}}),
        )
        .await;
        assert_eq!(result.get("ok").and_then(Value::as_bool), Some(false));
    }

    #[tokio::test]
    async fn unsupported_rich_edit_does_not_send_an_empty_classic_edit() {
        let state = proxy_state();
        let result = handle_api_method(
            &state,
            "editMessageText",
            &json!({
                "chat_id": 1,
                "message_id": 2,
                "rich_message": {"markdown": "final answer"}
            }),
        )
        .await;
        assert_eq!(result.get("ok").and_then(Value::as_bool), Some(false));
    }

    #[tokio::test]
    async fn teloxide_017_round_trips_through_the_proxy() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind proxy test server");
        let addr = listener.local_addr().expect("proxy test address");
        let (update_tx, update_rx) = mpsc::channel(1);
        let state = Arc::new(ProxyState {
            backend: Arc::new(ConsoleBackend::new()),
            bot_info: BotInfo {
                id: 1,
                username: "bridge-test".to_string(),
                first_name: "Bridge Test".to_string(),
            },
            update_rx: Mutex::new(update_rx),
            update_id_counter: AtomicI64::new(1),
            expected_token: "test-token".to_string(),
        });
        let server = tokio::spawn(run_proxy_server(state, listener));

        let client = teloxide::net::default_reqwest_settings()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("build teloxide proxy client");
        let api_url = format!("http://{addr}")
            .parse()
            .expect("parse proxy API URL");
        let bot = teloxide::Bot::with_client("test-token", client).set_api_url(api_url);

        // This exercises teloxide's exact endpoint casing and validates that
        // the proxy's getMe response still satisfies the current Me schema.
        let me = bot.get_me().await.expect("getMe through proxy");
        assert_eq!(me.username(), "bridge-test");

        // The sendMessage response must remain deserializable as the current
        // Message type, not merely return syntactically valid JSON.
        let sent = bot
            .send_message(teloxide::types::ChatId(7), "hello")
            .await
            .expect("sendMessage through proxy");
        assert_eq!(sent.id.0, 1);
        assert_eq!(sent.text(), Some("hello"));

        update_tx
            .send(IncomingMessage {
                chat_id: 7,
                message_id: 2,
                from_id: 42,
                from_first_name: "Tester".to_string(),
                from_username: Some("tester".to_string()),
                text: Some("incoming".to_string()),
                is_group: false,
                group_title: None,
                document: None,
                photo: None,
                caption: None,
                media_group_id: None,
            })
            .await
            .expect("queue proxy update");
        let updates = bot
            .get_updates()
            .offset(0)
            .limit(1)
            .await
            .expect("getUpdates through proxy");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].id.0, 1);
        let teloxide::types::UpdateKind::Message(message) = &updates[0].kind else {
            panic!("proxy update was not a message");
        };
        assert_eq!(message.text(), Some("incoming"));

        server.abort();
        let _ = server.await;
    }
}

#[cfg(test)]
mod bounded_download_tests {
    use super::*;

    async fn parse_raw_proxy_request(raw: Vec<u8>) -> Option<HttpRequest> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind parser test listener");
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream.write_all(&raw).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, _) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let request = read_http_request(&mut reader).await;
        client.await.unwrap();
        request
    }

    #[tokio::test]
    async fn unknown_length_download_is_stopped_at_runtime_limit() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nABCD\r\n4\r\nEFGH\r\n0\r\n\r\n",
                )
                .await
                .expect("write response");
        });

        let response = reqwest::get(format!("http://{addr}/file"))
            .await
            .expect("request test file");
        let error = read_download_response_limited(response, 6, "test download")
            .await
            .expect_err("eight-byte chunked body must exceed six-byte limit");
        assert!(error.contains("too large"), "unexpected error: {error}");
        server.await.expect("test server task");
    }

    #[tokio::test]
    async fn proxy_rejects_an_oversized_unterminated_request_line() {
        let mut raw = vec![b'G'; 16 * 1024 + 1];
        raw.extend_from_slice(b"\r\n\r\n");

        assert!(parse_raw_proxy_request(raw).await.is_none());
    }

    #[tokio::test]
    async fn proxy_rejects_ambiguous_content_length_and_chunked_framing() {
        let raw = b"POST /botx/sendMessage HTTP/1.1\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec();

        assert!(parse_raw_proxy_request(raw).await.is_none());
    }

    #[tokio::test]
    async fn proxy_accepts_a_well_framed_chunked_body() {
        let raw = b"POST /botx/sendMessage HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n4\r\ntest\r\n0\r\n\r\n".to_vec();

        let request = parse_raw_proxy_request(raw).await.expect("valid request");
        assert_eq!(request.body, b"test");
    }
}

// ============================================================
// JSON Builders (Telegram-compatible format)
// ============================================================

/// Convert IncomingMessage to Telegram Update JSON
fn incoming_to_update(state: &ProxyState, msg: IncomingMessage) -> Value {
    let update_id = state.next_update_id();
    let ts = chrono::Local::now().timestamp();

    let chat = if msg.is_group {
        json!({
            "id": msg.chat_id,
            "type": "supergroup",
            "title": msg.group_title.as_deref().unwrap_or("Group"),
        })
    } else {
        json!({
            "id": msg.chat_id,
            "type": "private",
            "first_name": msg.from_first_name,
        })
    };

    let mut from = json!({
        "id": msg.from_id,
        "is_bot": false,
        "first_name": msg.from_first_name,
    });
    if let Some(uname) = &msg.from_username {
        from["username"] = json!(uname);
    }

    let mut message = json!({
        "message_id": msg.message_id,
        "from": from,
        "chat": chat,
        "date": ts,
    });

    if let Some(text) = &msg.text {
        message["text"] = json!(text);
    }
    if let Some(caption) = &msg.caption {
        message["caption"] = json!(caption);
    }
    if let Some(group_id) = &msg.media_group_id {
        message["media_group_id"] = json!(group_id);
    }
    if let Some(doc) = &msg.document {
        message["document"] = json!({
            "file_id": doc.file_id,
            "file_unique_id": doc.file_id,
            "file_name": doc.file_name,
            "mime_type": doc.mime_type,
            "file_size": doc.file_size,
        });
    }
    if let Some(photos) = &msg.photo {
        let arr: Vec<Value> = photos
            .iter()
            .map(|p| {
                json!({
                    "file_id": p.file_id,
                    "file_unique_id": p.file_id,
                    "width": p.width,
                    "height": p.height,
                    "file_size": p.file_size,
                })
            })
            .collect();
        message["photo"] = json!(arr);
    }

    json!({
        "update_id": update_id,
        "message": message,
    })
}

/// Build a Telegram Message JSON for bot-sent messages (used in sendMessage/editMessage responses)
fn make_bot_message_json(state: &ProxyState, msg_id: i32, chat_id: i64, text: &str) -> Value {
    let chat = if chat_id < 0 {
        json!({
            "id": chat_id,
            "type": "supergroup",
            "title": "Group",
        })
    } else {
        json!({
            "id": chat_id,
            "type": "private",
            "first_name": state.bot_info.first_name,
        })
    };

    json!({
        "message_id": msg_id,
        "from": {
            "id": state.bot_info.id,
            "is_bot": true,
            "first_name": state.bot_info.first_name,
            "username": state.bot_info.username,
        },
        "chat": chat,
        "date": chrono::Local::now().timestamp(),
        "text": text,
    })
}

// ============================================================
// Console Backend (for testing)
// ============================================================

struct ConsoleBackend {
    msg_id_counter: Arc<AtomicI32>,
}

impl ConsoleBackend {
    fn new() -> Self {
        Self {
            msg_id_counter: Arc::new(AtomicI32::new(1)),
        }
    }
}

fn validate_console_document_filename(filename: &str) -> Result<&std::ffi::OsStr, String> {
    if filename.is_empty() || filename.contains('\0') || filename.contains('\\') {
        return Err("Invalid document filename".to_string());
    }
    let path = Path::new(filename);
    let mut components = path.components();
    let Some(Component::Normal(name)) = components.next() else {
        return Err("Document filename must be a plain basename".to_string());
    };
    if components.next().is_some() || name != path.as_os_str() {
        return Err("Document filename must be a plain basename".to_string());
    }
    Ok(name)
}

fn persist_console_document_in(dir: &Path, data: &[u8], filename: &str) -> Result<PathBuf, String> {
    use std::io::Write;

    let basename = validate_console_document_filename(filename)?;
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("Failed to create console document directory: {e}"))?;
    let (directory, directory_access, metadata) =
        crate::services::file_ops::open_directory_for_read(dir)
            .map_err(|e| format!("Failed to open console document directory: {e}"))?;
    let directory_identity = crate::services::file_ops::stable_file_identity(&directory)
        .map_err(|e| format!("Failed to identify console document directory: {e}"))?;
    if !metadata.file_type().is_dir()
        || crate::services::file_ops::stable_path_identity(dir)
            .map_err(|e| format!("Failed to bind console document directory: {e}"))?
            != directory_identity
    {
        return Err("Console document path is not a stable real directory".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("Failed to secure console document directory: {e}"))?;
    }
    if crate::services::file_ops::stable_path_identity(dir)
        .map_err(|e| format!("Failed to rebind console document directory: {e}"))?
        != directory_identity
    {
        return Err("Console document directory changed while it was secured".to_string());
    }

    for _ in 0..32 {
        let file_name = format!(
            "{:032x}-{}",
            rand::random::<u128>(),
            basename.to_string_lossy()
        );
        let path = dir.join(&file_name);
        let name = std::ffi::OsStr::new(&file_name);
        let mut file = match directory_access.open_file(
            name,
            crate::services::file_ops::DirectoryFileOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600),
        ) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Failed to create console document: {e}")),
        };
        let identity = match crate::services::file_ops::stable_file_identity(&file) {
            Ok(identity) => identity,
            Err(error) => {
                drop(file);
                if let Ok(identity) = directory_access.child_identity(name) {
                    let _ = directory_access.remove_file_if_identity(name, identity);
                }
                return Err(format!("Failed to identify console document: {error}"));
            }
        };
        if let Err(e) = file.write_all(data).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = directory_access.remove_file_if_identity(name, identity);
            return Err(format!("Failed to save console document: {e}"));
        }
        drop(file);
        let path_matches = crate::services::file_ops::stable_path_identity(&path)
            .map(|current| current == identity)
            .unwrap_or(false);
        let directory_matches = crate::services::file_ops::stable_path_identity(dir)
            .map(|current| current == directory_identity)
            .unwrap_or(false);
        if !path_matches || !directory_matches {
            let _ = directory_access.remove_file_if_identity(name, identity);
            return Err("Console document path changed while the file was being saved".to_string());
        }
        #[cfg(unix)]
        directory
            .sync_all()
            .map_err(|e| format!("Failed to persist console document: {e}"))?;
        return Ok(path);
    }

    Err("Failed to allocate a unique console document path".to_string())
}

#[async_trait]
impl MessengerBackend for ConsoleBackend {
    fn name(&self) -> &str {
        "console"
    }

    async fn init(&mut self) -> Result<BotInfo, String> {
        Ok(BotInfo {
            id: 100,
            username: "console_bot".to_string(),
            first_name: "ConsoleBot".to_string(),
        })
    }

    async fn start(
        &self,
        tx: mpsc::Sender<IncomingMessage>,
    ) -> Result<tokio::task::JoinHandle<()>, String> {
        let counter = self.msg_id_counter.clone();

        let handle = tokio::task::spawn_blocking(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            let reader = stdin.lock();

            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                let text = line.trim().to_string();
                if text.is_empty() {
                    continue;
                }

                let msg_id = counter.fetch_add(1, Ordering::Relaxed);
                let msg = IncomingMessage {
                    chat_id: 1,
                    message_id: msg_id,
                    from_id: 1000,
                    from_first_name: "ConsoleUser".to_string(),
                    from_username: Some("console_user".to_string()),
                    text: Some(text),
                    is_group: false,
                    group_title: None,
                    document: None,
                    photo: None,
                    caption: None,
                    media_group_id: None,
                };

                if tx.blocking_send(msg).is_err() {
                    break;
                }
            }
        });

        // `spawn_blocking` returns `JoinHandle<()>` directly — same shape
        // as `spawn`, no map needed.
        Ok(handle)
    }

    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        _parse_mode: Option<&str>,
    ) -> Result<SentMessage, String> {
        let clean = strip_html(text);
        println!("\n{}\n", clean);
        let msg_id = self.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        Ok(SentMessage {
            message_id: msg_id,
            chat_id,
            text: Some(text.to_string()),
        })
    }

    async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i32,
        text: &str,
        _parse_mode: Option<&str>,
    ) -> Result<SentMessage, String> {
        let clean = strip_html(text);
        // Overwrite previous line with updated text
        print!("\x1b[2K\r{}", clean);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        Ok(SentMessage {
            message_id,
            chat_id,
            text: Some(text.to_string()),
        })
    }

    async fn delete_message(&self, _chat_id: i64, _message_id: i32) -> Result<bool, String> {
        Ok(true)
    }

    async fn send_chat_action(&self, _chat_id: i64, _action: &str) -> Result<bool, String> {
        Ok(true)
    }

    async fn send_document(
        &self,
        chat_id: i64,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
    ) -> Result<SentMessage, String> {
        let dir = crate::utils::path::cokacdir_temp_dir()
            .map_err(|e| format!("Failed to prepare cokacdir temporary directory: {e}"))?
            .join("cokacdir_bridge");
        let path = persist_console_document_in(&dir, data, filename)?;
        println!(
            "\n[File: {} ({} bytes) → {}]",
            filename,
            data.len(),
            path.display()
        );
        if let Some(cap) = caption {
            println!("  {}", cap);
        }
        let msg_id = self.msg_id_counter.fetch_add(1, Ordering::Relaxed);
        Ok(SentMessage {
            message_id: msg_id,
            chat_id,
            text: None,
        })
    }

    async fn get_file(&self, _file_id: &str) -> Result<FileInfo, String> {
        Err("Console backend does not support file downloads".to_string())
    }

    async fn get_file_data(&self, _file_path: &str) -> Result<Vec<u8>, String> {
        Err("Console backend does not support file downloads".to_string())
    }
}

#[cfg(test)]
mod console_document_tests {
    use super::{persist_console_document_in, validate_console_document_filename};

    #[test]
    fn rejects_traversal_and_absolute_document_filenames() {
        assert!(validate_console_document_filename("../victim").is_err());
        assert!(validate_console_document_filename("sub/file").is_err());
        assert!(validate_console_document_filename("sub\\file").is_err());
        assert!(validate_console_document_filename("/absolute").is_err());
        assert!(validate_console_document_filename("..").is_err());
    }

    #[test]
    fn saves_to_a_private_unique_file_without_touching_existing_names() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("bridge");
        std::fs::create_dir(&dir).unwrap();
        let existing = dir.join("report.txt");
        std::fs::write(&existing, b"keep me").unwrap();

        let saved = persist_console_document_in(&dir, b"new data", "report.txt").unwrap();

        assert_ne!(saved, existing);
        assert_eq!(std::fs::read(existing).unwrap(), b"keep me");
        assert_eq!(std::fs::read(&saved).unwrap(), b"new data");
        assert_eq!(saved.parent(), Some(dir.as_path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(saved).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn console_document_directory_symlink_is_rejected_without_touching_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("bridge");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, &link).unwrap();

        assert!(persist_console_document_in(&link, b"secret", "report.txt").is_err());
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

/// Convert Telegram HTML to Discord Markdown, preserving formatting.
/// Handles: `<b>`, `<i>`, `<code>`, `<pre>`, and HTML entities.
fn telegram_html_to_discord(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '<' {
            let mut tag = String::new();
            for tc in chars.by_ref() {
                if tc == '>' {
                    break;
                }
                tag.push(tc);
            }
            match tag.as_str() {
                "b" => result.push_str("**"),
                "/b" => result.push_str("**"),
                "i" => result.push('*'),
                "/i" => result.push('*'),
                "code" => result.push('`'),
                "/code" => result.push('`'),
                "pre" => {
                    if !result.is_empty() && !result.ends_with('\n') {
                        result.push('\n');
                    }
                    result.push_str("```\n");
                }
                "/pre" => {
                    if !result.ends_with('\n') {
                        result.push('\n');
                    }
                    result.push_str("```");
                }
                _ => {} // strip unknown tags
            }
        } else if c == '&' {
            let mut entity = String::new();
            for ec in chars.by_ref() {
                if ec == ';' {
                    break;
                }
                entity.push(ec);
            }
            match entity.as_str() {
                "lt" => result.push('<'),
                "gt" => result.push('>'),
                "amp" => result.push('&'),
                "quot" => result.push('"'),
                _ => {
                    result.push('&');
                    result.push_str(&entity);
                    result.push(';');
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

fn strip_html(s: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

// ============================================================
// Discord Backend
// ============================================================

/// File metadata stored for later download via get_file / get_file_data
#[derive(Clone)]
struct StoredFile {
    url: String,
    #[allow(dead_code)]
    filename: String,
    #[allow(dead_code)]
    mime_type: Option<String>,
    size: Option<u64>,
}

/// Shared state between Discord EventHandler and DiscordBackend methods.
/// Uses std::sync::Mutex (not tokio) because critical sections are very short
/// (HashMap lookups/inserts only, no I/O).
struct DiscordState {
    msg_counter: AtomicI32,
    file_counter: AtomicI32,
    /// telegram msg_id → (discord_channel_id, discord_message_id)
    tg_to_discord: std::sync::Mutex<HashMap<i32, (u64, u64)>>,
    /// (chat_id, discord_message_id) → telegram msg_id
    discord_to_tg: std::sync::Mutex<HashMap<(i64, u64), i32>>,
    /// file_id string → stored file info
    files: std::sync::Mutex<HashMap<String, StoredFile>>,
}

impl DiscordState {
    fn new() -> Self {
        Self {
            msg_counter: AtomicI32::new(1),
            file_counter: AtomicI32::new(1),
            tg_to_discord: std::sync::Mutex::new(HashMap::new()),
            discord_to_tg: std::sync::Mutex::new(HashMap::new()),
            files: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Get or create a Telegram-compatible i32 message ID for a Discord message.
    fn map_message_id(&self, chat_id: i64, discord_msg_id: u64) -> i32 {
        let key = (chat_id, discord_msg_id);
        let mut d2t = self.discord_to_tg.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&id) = d2t.get(&key) {
            return id;
        }
        let new_id = self.msg_counter.fetch_add(1, Ordering::Relaxed);
        d2t.insert(key, new_id);
        drop(d2t);
        self.tg_to_discord
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(new_id, (chat_id_to_channel_u64(chat_id), discord_msg_id));
        new_id
    }

    /// Resolve a Telegram message ID back to (discord_channel_id, discord_message_id).
    fn resolve_message_id(&self, tg_msg_id: i32) -> Option<(u64, u64)> {
        self.tg_to_discord
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&tg_msg_id)
            .copied()
    }

    /// Allocate an additional Telegram message ID that reverse-maps to the same
    /// Discord message. Used when a single Discord message carries multiple
    /// attachments and we fan out one IncomingMessage per attachment.
    /// The forward (discord_to_tg) mapping retains the canonical first ID so
    /// that Discord-side replies still resolve to a stable tg_msg_id.
    fn allocate_extra_message_id(&self, chat_id: i64, discord_msg_id: u64) -> i32 {
        let new_id = self.msg_counter.fetch_add(1, Ordering::Relaxed);
        self.tg_to_discord
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(new_id, (chat_id_to_channel_u64(chat_id), discord_msg_id));
        new_id
    }

    /// Store a Discord file URL for later download, returning a bridge file_id.
    fn store_file(
        &self,
        url: String,
        filename: String,
        mime_type: Option<String>,
        size: Option<u64>,
    ) -> String {
        let id = self.file_counter.fetch_add(1, Ordering::Relaxed);
        let file_id = format!("df_{}", id);
        self.files.lock().unwrap_or_else(|e| e.into_inner()).insert(
            file_id.clone(),
            StoredFile {
                url,
                filename,
                mime_type,
                size,
            },
        );
        file_id
    }

    fn get_stored_file(&self, file_id: &str) -> Option<StoredFile> {
        self.files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(file_id)
            .cloned()
    }
}

/// Convert Discord channel ID to Telegram-compatible chat ID.
/// Guild channels → negative (triggers group chat logic), DM → positive (private chat).
fn discord_chat_id(channel_id: u64, is_guild: bool) -> i64 {
    let id = (channel_id & 0x7FFFFFFFFFFFFFFF) as i64;
    if is_guild {
        -id
    } else {
        id
    }
}

/// Convert Telegram chat ID back to Discord channel ID (u64).
/// Guards against zero (serenity ChannelId requires NonZeroU64).
fn chat_id_to_channel_u64(chat_id: i64) -> u64 {
    let v = chat_id.unsigned_abs();
    if v == 0 {
        1
    } else {
        v
    }
}

struct DiscordBackend {
    token: String,
    http: Option<Arc<serenity::http::Http>>,
    state: Arc<DiscordState>,
}

impl DiscordBackend {
    fn new(token: String) -> Self {
        Self {
            token,
            http: None,
            state: Arc::new(DiscordState::new()),
        }
    }

    fn http(&self) -> Result<&Arc<serenity::http::Http>, String> {
        self.http
            .as_ref()
            .ok_or_else(|| "Discord not initialized".to_string())
    }
}

/// serenity EventHandler that converts Discord messages to IncomingMessage
struct DiscordHandler {
    tx: mpsc::Sender<IncomingMessage>,
    state: Arc<DiscordState>,
}

#[async_trait]
impl serenity::all::EventHandler for DiscordHandler {
    async fn message(&self, ctx: serenity::all::Context, msg: serenity::all::Message) {
        // Ignore bot messages (including our own)
        if msg.author.bot {
            return;
        }

        let is_guild = msg.guild_id.is_some();
        let chat_id = discord_chat_id(msg.channel_id.get(), is_guild);

        // Convert Discord mentions (<@ID>) to Telegram-style (@username)
        let text = if msg.content.is_empty() {
            None
        } else {
            let mut content = msg.content.clone();
            for mention in &msg.mentions {
                let patterns = [
                    format!("<@!{}>", mention.id), // nickname mention
                    format!("<@{}>", mention.id),  // regular mention
                ];
                for pat in &patterns {
                    if content.contains(pat.as_str()) {
                        content = content.replace(pat.as_str(), &format!("@{}", mention.name));
                    }
                }
            }
            Some(content)
        };

        // Guild name from cache (falls back to "Discord")
        let group_title = if is_guild {
            msg.guild_id
                .and_then(|gid| ctx.cache.guild(gid).map(|g| g.name.clone()))
                .or_else(|| Some("Discord".to_string()))
        } else {
            None
        };

        if msg.attachments.is_empty() {
            let tg_msg_id = self.state.map_message_id(chat_id, msg.id.get());
            let incoming = IncomingMessage {
                chat_id,
                message_id: tg_msg_id,
                from_id: msg.author.id.get(),
                from_first_name: msg.author.name.clone(),
                from_username: Some(msg.author.name.clone()),
                text,
                is_group: is_guild,
                group_title,
                document: None,
                photo: None,
                caption: None,
                media_group_id: None,
            };
            let _ = self.tx.send(incoming).await;
            return;
        }
        // All attachments of the same Discord message share a synthetic media_group_id
        // so the downstream prefix check can recognize i>=1 fan-outs (whose captions
        // are dropped to None) as continuations of the i=0 accepted upload.
        let group_id = format!("d:{}", msg.id.get());
        eprintln!(
            "  [discord] fan-out: chat_id={}, msg_id={}, attachments={}, media_group_id={}",
            chat_id,
            msg.id.get(),
            msg.attachments.len(),
            group_id
        );

        // Fan out: one IncomingMessage per attachment so each file is processed
        // individually downstream. Caption rides on the first attachment only,
        // matching Telegram's native multi-file convention.
        for (i, att) in msg.attachments.iter().enumerate() {
            let tg_msg_id = if i == 0 {
                self.state.map_message_id(chat_id, msg.id.get())
            } else {
                self.state.allocate_extra_message_id(chat_id, msg.id.get())
            };
            let is_image = att
                .content_type
                .as_ref()
                .map(|ct| ct.starts_with("image/"))
                .unwrap_or(false)
                && att.width.is_some();
            let file_id = self.state.store_file(
                att.url.clone(),
                att.filename.clone(),
                att.content_type.clone(),
                Some(att.size as u64),
            );
            let (document, photo) = if is_image {
                (
                    None,
                    Some(vec![PhotoAttachment {
                        file_id,
                        width: att.width.unwrap_or(0) as u32,
                        height: att.height.unwrap_or(0) as u32,
                        file_size: Some(att.size as u64),
                    }]),
                )
            } else {
                (
                    Some(FileAttachment {
                        file_id,
                        file_name: Some(att.filename.clone()),
                        mime_type: att.content_type.clone(),
                        file_size: Some(att.size as u64),
                    }),
                    None,
                )
            };
            let caption = if i == 0 { text.clone() } else { None };
            let incoming = IncomingMessage {
                chat_id,
                message_id: tg_msg_id,
                from_id: msg.author.id.get(),
                from_first_name: msg.author.name.clone(),
                from_username: Some(msg.author.name.clone()),
                text: None,
                is_group: is_guild,
                group_title: group_title.clone(),
                document,
                photo,
                caption,
                media_group_id: Some(group_id.clone()),
            };
            if self.tx.send(incoming).await.is_err() {
                return;
            }
        }
    }

    async fn ready(&self, _ctx: serenity::all::Context, ready: serenity::all::Ready) {
        println!(
            "  ✓ Discord gateway: {} ({})",
            ready.user.name, ready.user.id
        );
    }
}

#[async_trait]
impl MessengerBackend for DiscordBackend {
    fn name(&self) -> &str {
        "discord"
    }

    async fn init(&mut self) -> Result<BotInfo, String> {
        let http = Arc::new(serenity::http::Http::new(&self.token));
        let user = http
            .get_current_user()
            .await
            .map_err(|e| format!("Discord auth failed: {}", e))?;

        self.http = Some(http);

        Ok(BotInfo {
            id: user.id.get() as i64,
            username: user.name.clone(),
            first_name: user.name.clone(),
        })
    }

    async fn start(
        &self,
        tx: mpsc::Sender<IncomingMessage>,
    ) -> Result<tokio::task::JoinHandle<()>, String> {
        let handler = DiscordHandler {
            tx,
            state: self.state.clone(),
        };

        let intents = serenity::all::GatewayIntents::GUILD_MESSAGES
            | serenity::all::GatewayIntents::DIRECT_MESSAGES
            | serenity::all::GatewayIntents::MESSAGE_CONTENT;

        let mut client = serenity::all::Client::builder(&self.token, intents)
            .event_handler(handler)
            .await
            .map_err(|e| format!("Discord client error: {}", e))?;

        // serenity reconnects internally on transient gateway errors; this
        // handle resolves only when reconnection has been given up (token
        // revoked, banned, persistent network failure, panic).
        let handle = tokio::spawn(async move {
            if let Err(e) = client.start().await {
                eprintln!("  ✗ Discord gateway error: {}", e);
            }
        });

        Ok(handle)
    }

    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<SentMessage, String> {
        let http = self.http()?;
        let channel = serenity::all::ChannelId::new(chat_id_to_channel_u64(chat_id));
        let clean = match parse_mode {
            Some("Html") | Some("HTML") | Some("html") => telegram_html_to_discord(text),
            Some(_) => strip_html(text),
            None => text.to_string(),
        };
        // Discord rejects empty messages
        let clean = if clean.trim().is_empty() {
            "\u{200b}".to_string() // zero-width space
        } else {
            clean
        };

        // Discord 2000 char limit — split if needed
        let chunks = split_discord_message(&clean);
        let mut last_msg_id = 0i32;

        for chunk in &chunks {
            let sent = channel
                .say(http.as_ref(), chunk)
                .await
                .map_err(|e| format!("Discord send: {}", e))?;
            last_msg_id = self.state.map_message_id(chat_id, sent.id.get());
        }

        Ok(SentMessage {
            message_id: last_msg_id,
            chat_id,
            text: Some(clean),
        })
    }

    async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i32,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<SentMessage, String> {
        let http = self.http()?;
        let (channel_u64, discord_msg_u64) = self
            .state
            .resolve_message_id(message_id)
            .ok_or_else(|| format!("Unknown msg ID: {}", message_id))?;
        let channel = serenity::all::ChannelId::new(channel_u64);
        let msg_id = serenity::all::MessageId::new(discord_msg_u64);
        let clean = match parse_mode {
            Some("Html") | Some("HTML") | Some("html") => telegram_html_to_discord(text),
            Some(_) => strip_html(text),
            None => text.to_string(),
        };

        // Discord rejects empty messages
        let clean = if clean.trim().is_empty() {
            "\u{200b}".to_string()
        } else {
            clean
        };

        // Truncate for Discord's 2000 char limit (streaming edits may exceed)
        let display = if clean.len() > 2000 {
            let mut end = 1997;
            while end > 0 && !clean.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &clean[..end])
        } else {
            clean.clone()
        };

        let edit = serenity::all::EditMessage::new().content(&display);
        channel
            .edit_message(http.as_ref(), msg_id, edit)
            .await
            .map_err(|e| format!("Discord edit: {}", e))?;

        Ok(SentMessage {
            message_id,
            chat_id,
            text: Some(clean),
        })
    }

    async fn delete_message(&self, _chat_id: i64, message_id: i32) -> Result<bool, String> {
        let http = self.http()?;
        let (channel_u64, discord_msg_u64) = self
            .state
            .resolve_message_id(message_id)
            .ok_or_else(|| format!("Unknown msg ID: {}", message_id))?;
        let channel = serenity::all::ChannelId::new(channel_u64);
        let msg_id = serenity::all::MessageId::new(discord_msg_u64);

        channel
            .delete_message(http.as_ref(), msg_id)
            .await
            .map_err(|e| format!("Discord delete: {}", e))?;
        Ok(true)
    }

    async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<bool, String> {
        let _ = self.http()?;
        if !action.eq_ignore_ascii_case("typing") {
            return Ok(true);
        }
        let channel_id = chat_id_to_channel_u64(chat_id);
        let url = format!("https://discord.com/api/v10/channels/{}/typing", channel_id);
        let response = bridge_api_client(30)?
            .post(url)
            .header("Authorization", format!("Bot {}", self.token))
            .send()
            .await
            .map_err(|e| format!("Discord typing: {}", e))?;
        if response.status().is_success() {
            Ok(true)
        } else {
            Err(format!("Discord typing failed: {}", response.status()))
        }
    }

    async fn send_document(
        &self,
        chat_id: i64,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
    ) -> Result<SentMessage, String> {
        let http = self.http()?;
        let channel = serenity::all::ChannelId::new(chat_id_to_channel_u64(chat_id));

        let attachment = serenity::all::CreateAttachment::bytes(data.to_vec(), filename);
        let mut builder = serenity::all::CreateMessage::new().add_file(attachment);
        if let Some(cap) = caption {
            let clean = strip_html(cap);
            if clean.len() <= 2000 {
                builder = builder.content(clean);
            }
        }

        let sent = channel
            .send_message(http.as_ref(), builder)
            .await
            .map_err(|e| format!("Discord send_document: {}", e))?;
        let tg_msg_id = self.state.map_message_id(chat_id, sent.id.get());

        Ok(SentMessage {
            message_id: tg_msg_id,
            chat_id,
            text: None,
        })
    }

    async fn get_file(&self, file_id: &str) -> Result<FileInfo, String> {
        let stored = self
            .state
            .get_stored_file(file_id)
            .ok_or_else(|| format!("File not found: {}", file_id))?;
        Ok(FileInfo {
            file_id: file_id.to_string(),
            file_path: stored.url,
            file_size: stored.size,
        })
    }

    async fn get_file_data(&self, file_path: &str) -> Result<Vec<u8>, String> {
        // file_path is supposed to be a Discord CDN URL stored by store_file.
        // The proxy receives `file_path` from an HTTP path component, so an
        // attacker who can reach the bridge port could otherwise have us
        // fetch arbitrary URLs (SSRF). Restrict to Discord CDN hosts.
        if !is_allowed_discord_file_url(file_path) {
            return Err(format!("Refused to fetch non-Discord URL: {}", file_path));
        }
        let resp = bridge_file_download_client()?
            .get(file_path)
            .send()
            .await
            .map_err(|e| format!("Download failed: {}", e))?;
        read_download_response_limited(resp, BRIDGE_FILE_DOWNLOAD_MAX_BYTES, "Discord download")
            .await
    }
}

/// Constant-time byte comparison for authentication tokens.
/// Always inspects every byte of both inputs so that an attacker cannot
/// learn the prefix of the expected token from response timing.
fn tokens_eq_constant_time(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        // Still consume one full pass over `a` to discourage length-leak via timing.
        let mut acc: u8 = 0;
        for &x in a {
            acc |= x;
        }
        let _ = std::hint::black_box(acc);
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn is_allowed_discord_file_url(url: &str) -> bool {
    // Accept only HTTPS Discord-controlled CDN hosts. Matching the host on a
    // segment boundary prevents tricks like `https://cdn.discordapp.com.evil/`.
    const ALLOWED_HOSTS: &[&str] = &["cdn.discordapp.com", "media.discordapp.net"];
    let rest = match url.strip_prefix("https://") {
        Some(r) => r,
        None => return false,
    };
    // The host portion ends at the first '/', '?', or '#' — without including
    // '?' and '#' a query-only or fragment-only URL would smuggle them into
    // the host slice and the eq comparison would fail open or false-negative.
    let host_end = rest
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let host = &rest[..host_end];
    ALLOWED_HOSTS.iter().any(|h| host.eq_ignore_ascii_case(h))
}

/// Split text into Discord-compatible chunks (max 2000 chars each).
/// Tries to split at newlines or spaces for readability.
fn split_discord_message(text: &str) -> Vec<String> {
    const MAX: usize = 2000;
    if text.len() <= MAX {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        if text.len() - pos <= MAX {
            chunks.push(text[pos..].to_string());
            break;
        }
        let mut end = pos + MAX;
        while !text.is_char_boundary(end) && end > pos {
            end -= 1;
        }
        let chunk = &text[pos..end];
        let split = chunk
            .rfind('\n')
            .or_else(|| chunk.rfind(' '))
            .map(|p| pos + p + 1);
        let split = match split {
            Some(s) if s > pos => s,
            _ => end,
        };
        chunks.push(text[pos..split].to_string());
        pos = split;
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

// ============================================================
// Slack Backend (Socket Mode adapter)
// ============================================================

#[derive(Clone)]
struct SlackStoredFile {
    url_private: String,
    #[allow(dead_code)]
    filename: String,
    #[allow(dead_code)]
    mime_type: Option<String>,
    size: Option<u64>,
}

struct SlackState {
    msg_counter: AtomicI32,
    file_counter: AtomicI32,
    bot_user_id: std::sync::Mutex<Option<String>>,
    bot_id: std::sync::Mutex<Option<String>>,
    bot_username: std::sync::Mutex<Option<String>>,
    team: std::sync::Mutex<Option<String>>,
    channel_map_path: Option<std::path::PathBuf>,
    /// tg_msg_id → (chat_id, slack_ts)
    tg_to_slack: std::sync::Mutex<HashMap<i32, (i64, String)>>,
    /// (chat_id, slack_ts) → tg_msg_id
    slack_to_tg: std::sync::Mutex<HashMap<(i64, String), i32>>,
    /// chat_id → slack channel string (for outgoing API calls)
    chat_to_channel: std::sync::Mutex<HashMap<i64, String>>,
    /// file_id (e.g. "sf_1") → SlackStoredFile
    files: std::sync::Mutex<HashMap<String, SlackStoredFile>>,
    /// Slack file_id (e.g. "F012ABC") → (chat_id, tg_msg_id, registered_at)
    /// waiting for the auto-posted file_share event so we can record its ts.
    /// Without this, edit/delete on document messages would fail with "Unknown
    /// msg ID". The timestamp is used to evict stale entries when the matching
    /// file_share event never arrives (e.g. the workspace did not subscribe to
    /// the corresponding `message.*` event).
    pending_uploads: std::sync::Mutex<HashMap<String, (i64, i32, std::time::Instant)>>,
    /// Slack can emit both `app_mention` and `message.*` for the same channel ts.
    /// Keep a bounded set so the same user message is only processed once.
    seen_incoming: std::sync::Mutex<HashSet<(i64, String)>>,
    seen_incoming_order: std::sync::Mutex<VecDeque<(i64, String)>>,
    /// Slack recommends roughly one posted message per second per channel.
    last_post_at: std::sync::Mutex<HashMap<i64, std::time::Instant>>,
}

impl SlackState {
    fn new(channel_map_path: Option<std::path::PathBuf>) -> Self {
        let chat_to_channel = channel_map_path
            .as_ref()
            .and_then(|path| load_slack_channel_map(path))
            .unwrap_or_default();

        Self {
            msg_counter: AtomicI32::new(1),
            file_counter: AtomicI32::new(1),
            bot_user_id: std::sync::Mutex::new(None),
            bot_id: std::sync::Mutex::new(None),
            bot_username: std::sync::Mutex::new(None),
            team: std::sync::Mutex::new(None),
            channel_map_path,
            tg_to_slack: std::sync::Mutex::new(HashMap::new()),
            slack_to_tg: std::sync::Mutex::new(HashMap::new()),
            chat_to_channel: std::sync::Mutex::new(chat_to_channel),
            files: std::sync::Mutex::new(HashMap::new()),
            pending_uploads: std::sync::Mutex::new(HashMap::new()),
            seen_incoming: std::sync::Mutex::new(HashSet::new()),
            seen_incoming_order: std::sync::Mutex::new(VecDeque::new()),
            last_post_at: std::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn wait_for_post_slot(&self, chat_id: i64) {
        const MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1100);

        let wait = {
            let mut last = self.last_post_at.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            let wait = last
                .get(&chat_id)
                .and_then(|prev| MIN_INTERVAL.checked_sub(now.saturating_duration_since(*prev)));
            let next = wait.map(|w| now + w).unwrap_or(now);
            last.insert(chat_id, next);
            wait
        };

        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
    }

    fn claim_incoming_event(&self, chat_id: i64, slack_ts: &str) -> bool {
        const MAX_SEEN_INCOMING: usize = 4096;

        let key = (chat_id, slack_ts.to_string());
        let mut seen = self.seen_incoming.lock().unwrap_or_else(|e| e.into_inner());
        if !seen.insert(key.clone()) {
            return false;
        }

        let mut order = self
            .seen_incoming_order
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        order.push_back(key);
        while order.len() > MAX_SEEN_INCOMING {
            if let Some(old) = order.pop_front() {
                seen.remove(&old);
            }
        }
        true
    }

    /// Insert an outgoing-document mapping so that when Slack auto-posts the
    /// `file_share` message we can attach the real ts to the synthetic msg_id.
    /// Opportunistically prunes entries older than `PENDING_UPLOAD_TTL` so a
    /// missing file_share event (e.g. when `message.channels` is unsubscribed)
    /// does not let the map grow without bound.
    fn register_pending_upload(&self, slack_file_id: &str, chat_id: i64, tg_msg_id: i32) {
        const PENDING_UPLOAD_TTL: std::time::Duration = std::time::Duration::from_secs(300);
        let mut map = self
            .pending_uploads
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        map.retain(|_, (_, _, registered_at)| {
            now.saturating_duration_since(*registered_at) < PENDING_UPLOAD_TTL
        });
        map.insert(slack_file_id.to_string(), (chat_id, tg_msg_id, now));
    }

    /// Resolve a Slack file_id to its pending tg_msg_id (consuming the entry).
    fn take_pending_upload(&self, slack_file_id: &str) -> Option<(i64, i32)> {
        self.pending_uploads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(slack_file_id)
            .map(|(chat_id, tg_msg_id, _)| (chat_id, tg_msg_id))
    }

    /// Bind a Slack ts to an existing tg_msg_id (used when the bot's own file_share
    /// event arrives after a document upload).
    fn bind_message_id(&self, tg_msg_id: i32, chat_id: i64, slack_ts: &str) {
        self.slack_to_tg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((chat_id, slack_ts.to_string()), tg_msg_id);
        self.tg_to_slack
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tg_msg_id, (chat_id, slack_ts.to_string()));
    }

    fn map_message_id(&self, chat_id: i64, slack_ts: &str) -> i32 {
        let key = (chat_id, slack_ts.to_string());
        let mut s2t = self.slack_to_tg.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&id) = s2t.get(&key) {
            return id;
        }
        let new_id = self.msg_counter.fetch_add(1, Ordering::Relaxed);
        s2t.insert(key, new_id);
        drop(s2t);
        self.tg_to_slack
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(new_id, (chat_id, slack_ts.to_string()));
        new_id
    }

    fn resolve_message_id(&self, tg_msg_id: i32) -> Option<(i64, String)> {
        self.tg_to_slack
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&tg_msg_id)
            .cloned()
    }

    /// Allocate an additional Telegram message ID that reverse-maps to the same
    /// Slack ts. Used to fan out multi-file Slack messages into one
    /// IncomingMessage per file; the canonical slack_to_tg forward mapping is
    /// left untouched so Slack-side lookups still resolve to the first ID.
    fn allocate_extra_message_id(&self, chat_id: i64, slack_ts: &str) -> i32 {
        let new_id = self.msg_counter.fetch_add(1, Ordering::Relaxed);
        self.tg_to_slack
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(new_id, (chat_id, slack_ts.to_string()));
        new_id
    }

    fn register_channel(&self, chat_id: i64, channel: &str) {
        let mut map = self
            .chat_to_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if map
            .get(&chat_id)
            .map(|existing| existing == channel)
            .unwrap_or(false)
        {
            return;
        }
        map.insert(chat_id, channel.to_string());
        // Keep snapshot mutation and persistence under one ordering lock. If
        // two event callbacks persist outside this guard, an older snapshot
        // can finish last and erase the newer channel from disk.
        self.persist_channel_map(&map);
    }

    fn channel_for_chat(&self, chat_id: i64) -> Option<String> {
        self.chat_to_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&chat_id)
            .cloned()
    }

    fn store_file(
        &self,
        url: String,
        filename: String,
        mime_type: Option<String>,
        size: Option<u64>,
    ) -> String {
        let id = self.file_counter.fetch_add(1, Ordering::Relaxed);
        let file_id = format!("sf_{}", id);
        self.files.lock().unwrap_or_else(|e| e.into_inner()).insert(
            file_id.clone(),
            SlackStoredFile {
                url_private: url,
                filename,
                mime_type,
                size,
            },
        );
        file_id
    }

    fn get_stored_file(&self, file_id: &str) -> Option<SlackStoredFile> {
        self.files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(file_id)
            .cloned()
    }

    fn bot_id_value(&self) -> Option<String> {
        self.bot_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn bot_user_id_value(&self) -> Option<String> {
        self.bot_user_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn bot_username_value(&self) -> Option<String> {
        self.bot_username
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn persist_channel_map(&self, map: &HashMap<i64, String>) {
        let Some(path) = &self.channel_map_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("  [slack] channel map dir create failed: {}", e);
                return;
            }
        }
        let json_map: serde_json::Map<String, serde_json::Value> = map
            .iter()
            .map(|(chat_id, channel)| (chat_id.to_string(), serde_json::json!(channel)))
            .collect();
        let body = serde_json::Value::Object(json_map).to_string();
        if let Err(e) =
            crate::services::telegram::write_private_file_atomically(path, body.as_bytes())
        {
            eprintln!("  [slack] channel map persist failed: {}", e);
        }
    }
}

fn slack_channel_map_path(bot_token: &str) -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|home| {
        home.join(".cokacdir").join("bridge_maps").join(format!(
            "slack_{}.json",
            crate::services::telegram::token_hash(bot_token)
        ))
    })
}

fn load_slack_channel_map(path: &std::path::Path) -> Option<HashMap<i64, String>> {
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let obj = value.as_object()?;
    let mut map = HashMap::new();
    for (chat_id, channel) in obj {
        let Ok(chat_id) = chat_id.parse::<i64>() else {
            continue;
        };
        let Some(channel) = channel.as_str() else {
            continue;
        };
        if !channel.is_empty() {
            map.insert(chat_id, channel.to_string());
        }
    }
    Some(map)
}

#[cfg(test)]
mod slack_tests {
    use super::{
        extract_slack_complete_upload_ts, load_slack_channel_map,
        normalize_slack_text_for_telegram, slack_chat_id, telegram_html_to_slack_mrkdwn,
        SlackState,
    };

    #[test]
    fn escapes_literal_angle_and_amp_in_slack_mrkdwn() {
        // Plain text containing entity-encoded specials must not produce raw
        // `<…>` in the Slack output, otherwise Slack interprets it as markup
        // and silently drops the contained text.
        assert_eq!(
            telegram_html_to_slack_mrkdwn("Use the &lt;button&gt; tag"),
            "Use the &lt;button&gt; tag"
        );
        assert_eq!(telegram_html_to_slack_mrkdwn("a &amp; b"), "a &amp; b");

        // Bold/italic/code conversions still apply, and label text inside an
        // anchor is escaped without disturbing the surrounding `<URL|label>`.
        assert_eq!(
            telegram_html_to_slack_mrkdwn("<b>hi &lt;x&gt;</b>"),
            "*hi &lt;x&gt;*"
        );
        assert_eq!(
            telegram_html_to_slack_mrkdwn(
                "<a href=\"https://example.com/?a=1&amp;b=2\">label &lt;x&gt;</a>"
            ),
            "<https://example.com/?a=1&b=2|label &lt;x&gt;>"
        );
    }

    #[test]
    fn persists_and_loads_slack_channel_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slack_map.json");
        let state = SlackState::new(Some(path.clone()));

        let channel = "C0123456789";
        let chat_id = slack_chat_id(channel);
        state.register_channel(chat_id, channel);

        let loaded = load_slack_channel_map(&path).unwrap();
        assert_eq!(loaded.get(&chat_id).map(String::as_str), Some(channel));

        let restored = SlackState::new(Some(path));
        assert_eq!(restored.channel_for_chat(chat_id).as_deref(), Some(channel));
    }

    #[test]
    fn concurrent_channel_registrations_all_survive_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slack_map.json");
        let state = std::sync::Arc::new(SlackState::new(Some(path.clone())));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(17));
        let mut workers = Vec::new();
        for index in 0..16i64 {
            let state = state.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                state.register_channel(-(index + 1), &format!("C{index:010}"));
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        let reloaded = load_slack_channel_map(&path).expect("load persisted channel map");
        assert_eq!(reloaded.len(), 16);
        for index in 0..16i64 {
            assert_eq!(
                reloaded.get(&-(index + 1)).map(String::as_str),
                Some(format!("C{index:010}").as_str())
            );
        }
    }

    #[test]
    fn slack_chat_id_preserves_channel_kind() {
        assert!(slack_chat_id("D0123456789") > 0);
        assert!(slack_chat_id("C0123456789") < 0);
        assert!(slack_chat_id("G0123456789") < 0);
    }

    #[test]
    fn normalizes_slack_bot_mentions_for_group_routing() {
        let state = SlackState::new(None);
        *state.bot_user_id.lock().unwrap() = Some("U012BOT".to_string());
        *state.bot_username.lock().unwrap() = Some("cokac".to_string());

        assert_eq!(
            normalize_slack_text_for_telegram("<@U012BOT> hello", &state),
            "@cokac hello"
        );
        assert_eq!(
            normalize_slack_text_for_telegram("<@U012BOT> ;status", &state),
            ";status"
        );
        assert_eq!(
            normalize_slack_text_for_telegram("please ask <@U012BOT>", &state),
            "please ask @cokac"
        );
    }

    #[test]
    fn deduplicates_slack_events_by_chat_and_timestamp() {
        let state = SlackState::new(None);
        let chat_id = slack_chat_id("C0123456789");

        assert!(state.claim_incoming_event(chat_id, "1712345678.000100"));
        assert!(!state.claim_incoming_event(chat_id, "1712345678.000100"));
        assert!(state.claim_incoming_event(chat_id, "1712345678.000200"));
    }

    #[test]
    fn extracts_file_share_timestamp_from_complete_upload_response() {
        let response = serde_json::json!({
            "ok": true,
            "files": [{
                "id": "F012ABC",
                "shares": {
                    "public": {
                        "C0123456789": [{"ts": "1712345678.000300"}]
                    }
                }
            }]
        });

        assert_eq!(
            extract_slack_complete_upload_ts(&response, "C0123456789").as_deref(),
            Some("1712345678.000300")
        );
    }

    #[test]
    fn extracts_file_share_timestamp_from_private_only_share() {
        let response = serde_json::json!({
            "ok": true,
            "files": [{
                "id": "F012DEF",
                "shares": {
                    "private": {
                        "G0123456789": [{"ts": "1712345678.000400"}]
                    }
                }
            }]
        });

        assert_eq!(
            extract_slack_complete_upload_ts(&response, "G0123456789").as_deref(),
            Some("1712345678.000400")
        );
    }
}

struct SlackBackend {
    bot_token: String,
    app_token: String,
    client: Option<Arc<SlackHyperClient>>,
    state: Arc<SlackState>,
}

impl SlackBackend {
    fn new(bot_token: String, app_token: String) -> Self {
        let channel_map_path = slack_channel_map_path(&bot_token);
        Self {
            bot_token,
            app_token,
            client: None,
            state: Arc::new(SlackState::new(channel_map_path)),
        }
    }
}

/// Stable FNV-1a hash of a Slack ID string to a non-negative i64.
/// Used to map Slack's string IDs (e.g. "U012ABC", "C0123456789") to Telegram's i64 chat/user IDs.
fn slack_id_hash(s: &str) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h & 0x7FFFFFFFFFFFFFFF) as i64
}

/// Convert Slack channel ID string to Telegram-compatible chat_id (i64).
/// Public/private channels (C/G) → negative (group), DM (D) → positive (private).
fn slack_chat_id(channel: &str) -> i64 {
    let id = slack_id_hash(channel);
    if channel.starts_with('D') {
        id
    } else {
        -id
    }
}

/// Convert Telegram HTML to Slack mrkdwn syntax.
fn telegram_html_to_slack_mrkdwn(html: &str) -> String {
    fn decode_entities(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
    }

    // Slack mrkdwn treats raw `<` as the start of a markup token (`<URL|label>`,
    // `<@USER>`, etc.). Literal text content carried over from Telegram HTML must
    // therefore be re-escaped after entity decoding, otherwise a user-supplied
    // `<` truncates or eats the surrounding text on Slack's side.
    fn push_escaped_text(text: &str, out: &mut String) {
        for c in text.chars() {
            match c {
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '&' => out.push_str("&amp;"),
                _ => out.push(c),
            }
        }
    }

    fn flush_text(buf: &mut String, out: &mut String) {
        if buf.is_empty() {
            return;
        }
        let decoded = decode_entities(buf);
        push_escaped_text(&decoded, out);
        buf.clear();
    }

    let mut result = String::new();
    let mut chars = html.chars().peekable();
    let mut text_buf = String::new();

    while let Some(ch) = chars.next() {
        if ch == '<' {
            flush_text(&mut text_buf, &mut result);
            let mut tag = String::new();
            while let Some(&c) = chars.peek() {
                if c == '>' {
                    chars.next();
                    break;
                }
                tag.push(c);
                chars.next();
            }
            let lower = tag.to_lowercase();
            match lower.as_str() {
                "b" | "strong" | "/b" | "/strong" => result.push('*'),
                "i" | "em" | "/i" | "/em" => result.push('_'),
                "s" | "strike" | "del" | "/s" | "/strike" | "/del" => result.push('~'),
                "code" | "/code" => result.push('`'),
                "pre" | "/pre" => result.push_str("```"),
                "/a" => result.push('>'),
                _ if lower.starts_with("a href=") => {
                    if let Some(qs) = tag.find('"') {
                        let after_q = &tag[qs + 1..];
                        if let Some(qe) = after_q.find('"') {
                            let url = &after_q[..qe];
                            // URL sits inside `<URL|label>` markup: decode entities
                            // (Telegram encodes `&` in query strings as `&amp;`),
                            // but do not Slack-escape — Slack expects a raw URL.
                            result.push('<');
                            result.push_str(&decode_entities(url));
                            result.push('|');
                        }
                    }
                }
                _ => {}
            }
        } else {
            text_buf.push(ch);
        }
    }
    flush_text(&mut text_buf, &mut result);

    result
}

/// Slack mrkdwn safe text limit (12,000 chars per chat.postMessage docs).
const SLACK_TEXT_LIMIT: usize = 12000;

/// Split text into Slack-compatible chunks (max 12,000 chars each).
fn split_slack_message(text: &str) -> Vec<String> {
    if text.len() <= SLACK_TEXT_LIMIT {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        if text.len() - pos <= SLACK_TEXT_LIMIT {
            chunks.push(text[pos..].to_string());
            break;
        }
        let mut end = pos + SLACK_TEXT_LIMIT;
        while !text.is_char_boundary(end) && end > pos {
            end -= 1;
        }
        let chunk = &text[pos..end];
        let split = chunk
            .rfind('\n')
            .or_else(|| chunk.rfind(' '))
            .map(|p| pos + p + 1);
        let split = match split {
            Some(s) if s > pos => s,
            _ => end,
        };
        chunks.push(text[pos..split].to_string());
        pos = split;
    }
    if chunks.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

fn extract_slack_complete_upload_ts(value: &serde_json::Value, channel: &str) -> Option<String> {
    fn extract_from_file(file: &serde_json::Value, channel: &str) -> Option<String> {
        let shares = file.get("shares")?;
        for scope in ["public", "private"] {
            // `shares` may contain either scope (or both) depending on whether the
            // file was shared into a public channel, a private channel, a DM, or
            // a group DM. A missing scope must skip to the next iteration rather
            // than short-circuit the whole search — using `?` here would cause
            // private-channel/DM uploads to never match.
            let Some(entries) = shares
                .get(scope)
                .and_then(|s| s.get(channel))
                .and_then(|v| v.as_array())
            else {
                continue;
            };
            if let Some(ts) = entries
                .iter()
                .find_map(|entry| entry.get("ts").and_then(|ts| ts.as_str()))
            {
                return Some(ts.to_string());
            }
        }
        None
    }

    if let Some(ts) = value
        .get("file")
        .and_then(|file| extract_from_file(file, channel))
    {
        return Some(ts);
    }

    value
        .get("files")
        .and_then(|files| files.as_array())
        .and_then(|files| {
            files
                .iter()
                .find_map(|file| extract_from_file(file, channel))
        })
}

/// Decode a single Slack mrkdwn markup token (the contents between `<` and `>`).
/// Slack documents this format under "Formatting message text" → mrkdwn.
fn decode_slack_token(
    token: &str,
    bot_user_id: Option<&str>,
    bot_username: Option<&str>,
) -> String {
    // <!here>, <!channel>, <!everyone>, <!subteam^...|label>
    if let Some(rest) = token.strip_prefix('!') {
        let (head, label) = match rest.split_once('|') {
            Some((h, l)) => (h, Some(l)),
            None => (rest, None),
        };
        if let Some(l) = label {
            return format!("@{}", l);
        }
        return match head {
            "here" | "channel" | "everyone" => format!("@{}", head),
            other if other.starts_with("subteam^") => "@group".to_string(),
            other if other.starts_with("date^") => other.to_string(),
            other => format!("<{}>", other),
        };
    }

    // <@U012ABC> or <@U012ABC|label>
    if let Some(rest) = token.strip_prefix('@') {
        let (uid, label) = match rest.split_once('|') {
            Some((u, l)) => (u, Some(l)),
            None => (rest, None),
        };
        if Some(uid) == bot_user_id {
            return format!("@{}", bot_username.unwrap_or(uid));
        }
        if let Some(l) = label {
            return format!("@{}", l);
        }
        return format!("@{}", uid);
    }

    // <#C012ABC|name> or <#C012ABC>
    if let Some(rest) = token.strip_prefix('#') {
        let (cid, label) = match rest.split_once('|') {
            Some((c, l)) => (c, Some(l)),
            None => (rest, None),
        };
        return format!("#{}", label.unwrap_or(cid));
    }

    // <https://example.com|label> or bare <https://example.com>
    let (url, label) = match token.split_once('|') {
        Some((u, l)) => (u, Some(l)),
        None => (token, None),
    };
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:") {
        return match label {
            Some(l) => format!("{} ({})", l, url),
            None => url.to_string(),
        };
    }

    // Unknown — preserve raw form to avoid silent data loss.
    format!("<{}>", token)
}

fn decode_slack_markup(
    text: &str,
    bot_user_id: Option<&str>,
    bot_username: Option<&str>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut token = String::new();
            let mut closed = false;
            while let Some(&n) = chars.peek() {
                if n == '>' {
                    chars.next();
                    closed = true;
                    break;
                }
                token.push(n);
                chars.next();
            }
            if closed {
                out.push_str(&decode_slack_token(&token, bot_user_id, bot_username));
            } else {
                out.push('<');
                out.push_str(&token);
            }
        } else {
            out.push(c);
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn normalize_slack_text_for_telegram(text: &str, state: &SlackState) -> String {
    let bot_user_id = state.bot_user_id_value();
    let bot_username = state.bot_username_value();
    let decoded = decode_slack_markup(text, bot_user_id.as_deref(), bot_username.as_deref());

    // If the original message led with the bot mention, allow command-prefixed
    // text (";status", "/foo", "!cmd") to flow through cleanly without an
    // "@bot " prefix that would break command parsing.
    let Some(bot_user_id) = bot_user_id else {
        return decoded;
    };
    let bot_label = bot_username.as_deref().unwrap_or(&bot_user_id);
    let raw_mention = format!("<@{}>", bot_user_id);
    let trimmed = text.trim_start();
    if trimmed.starts_with(&raw_mention) {
        let rest = trimmed[raw_mention.len()..].trim_start();
        if rest.starts_with('/') || rest.starts_with('!') || rest.starts_with(';') {
            return decode_slack_markup(rest, Some(&bot_user_id), bot_username.as_deref());
        }
    }
    let labeled = format!("@{}", bot_label);
    if let Some(rest) = decoded.trim_start().strip_prefix(&labeled) {
        if rest.is_empty() {
            return format!("@{} ", bot_label);
        }
    }
    decoded
}

#[async_trait]
impl MessengerBackend for SlackBackend {
    fn name(&self) -> &str {
        "slack"
    }

    async fn init(&mut self) -> Result<BotInfo, String> {
        let connector = SlackClientHyperConnector::new()
            .map_err(|e| format!("Slack hyper connector init failed: {}", e))?;
        let client = Arc::new(SlackClient::new(connector));

        let bot_token_value: SlackApiTokenValue = self.bot_token.clone().into();
        let bot_token = SlackApiToken::new(bot_token_value);

        let session = client.open_session(&bot_token);
        let auth_resp = session
            .auth_test()
            .await
            .map_err(|e| format!("Slack auth.test failed: {}", e))?;

        let user_id_str = auth_resp.user_id.to_string();
        let bot_id_str = auth_resp.bot_id.as_ref().map(|b| b.to_string());
        let username = auth_resp
            .user
            .clone()
            .unwrap_or_else(|| "slack_bot".to_string());
        let team = auth_resp.team.clone();

        *self
            .state
            .bot_user_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(user_id_str.clone());
        *self.state.bot_id.lock().unwrap_or_else(|e| e.into_inner()) = bot_id_str;
        *self
            .state
            .bot_username
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(username.clone());
        *self.state.team.lock().unwrap_or_else(|e| e.into_inner()) = Some(team);
        self.client = Some(client);

        Ok(BotInfo {
            id: slack_id_hash(&user_id_str),
            username: username.clone(),
            first_name: username,
        })
    }

    async fn start(
        &self,
        tx: mpsc::Sender<IncomingMessage>,
    ) -> Result<tokio::task::JoinHandle<()>, String> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| "Slack client not initialized".to_string())?
            .clone();

        let app_token: SlackApiToken = SlackApiToken::new(self.app_token.clone().into());
        let state = self.state.clone();

        let listener_environment =
            Arc::new(SlackClientEventsListenerEnvironment::new(client.clone()));

        let push_callback = move |evt: SlackPushEventCallback,
                                  _client: Arc<SlackHyperClient>,
                                  _states: SlackClientEventsUserState| {
            let state = state.clone();
            let tx = tx.clone();
            async move {
                if let Err(e) = process_slack_push_event(evt, state, tx).await {
                    eprintln!("  ✗ Slack push event error: {}", e);
                }
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            }
        };

        let mut callbacks: SlackSocketModeListenerCallbacks<SlackClientHyperHttpsConnector> =
            SlackSocketModeListenerCallbacks::new();
        callbacks.push_events_callback = Box::new(push_callback);

        let listener = SlackClientSocketModeListener::new(
            &SlackClientSocketModeConfig::new(),
            listener_environment,
            callbacks,
        );

        listener
            .listen_for(&app_token)
            .await
            .map_err(|e| format!("Slack listen_for: {}", e))?;

        // `serve()` runs the Socket Mode loop forever; it returns only on
        // persistent failure (auth revoked, repeated reconnect failure)
        // or panic. The handle resolution is the death signal.
        let handle = tokio::spawn(async move {
            listener.serve().await;
        });

        Ok(handle)
    }

    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<SentMessage, String> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| "Slack client not initialized".to_string())?;

        let channel_str = self
            .state
            .channel_for_chat(chat_id)
            .ok_or_else(|| format!("Unknown chat_id: {}", chat_id))?;

        let mrkdwn = match parse_mode {
            Some("Html") | Some("HTML") | Some("html") => telegram_html_to_slack_mrkdwn(text),
            Some(_) => strip_html(text),
            None => text.to_string(),
        };
        let mrkdwn = if mrkdwn.trim().is_empty() {
            "\u{200b}".to_string()
        } else {
            mrkdwn
        };

        let chunks = split_slack_message(&mrkdwn);
        let bot_token: SlackApiToken = SlackApiToken::new(self.bot_token.clone().into());
        let session = client.open_session(&bot_token);

        let channel: SlackChannelId = channel_str.clone().into();
        let mut last_msg_id = 0i32;
        let mut last_text = String::new();

        for chunk in &chunks {
            self.state.wait_for_post_slot(chat_id).await;
            let req = SlackApiChatPostMessageRequest::new(
                channel.clone(),
                SlackMessageContent::new().with_text(chunk.clone()),
            );
            let resp = session
                .chat_post_message(&req)
                .await
                .map_err(|e| format!("Slack chat.postMessage: {}", e))?;
            let ts_str = resp.ts.to_string();
            last_msg_id = self.state.map_message_id(chat_id, &ts_str);
            last_text = chunk.clone();
        }

        Ok(SentMessage {
            message_id: last_msg_id,
            chat_id,
            text: Some(last_text),
        })
    }

    async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i32,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<SentMessage, String> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| "Slack client not initialized".to_string())?;

        // Use the chat_id stored alongside the ts when the message was first
        // mapped, so an edit always targets the channel that actually carries
        // the ts. Falling back to the caller-provided chat_id would let a
        // mismatched arg silently target a different channel's API request.
        // Matches `delete_message`'s resolution policy.
        let (resolved_chat_id, ts_str) = self
            .state
            .resolve_message_id(message_id)
            .ok_or_else(|| format!("Unknown msg ID: {}", message_id))?;

        let channel_str = self
            .state
            .channel_for_chat(resolved_chat_id)
            .ok_or_else(|| format!("Unknown chat_id for msg: {}", message_id))?;

        let mrkdwn = match parse_mode {
            Some("Html") | Some("HTML") | Some("html") => telegram_html_to_slack_mrkdwn(text),
            Some(_) => strip_html(text),
            None => text.to_string(),
        };
        let mrkdwn = if mrkdwn.trim().is_empty() {
            "\u{200b}".to_string()
        } else {
            mrkdwn
        };

        // chat.update has a 4000-char hard limit
        let display = if mrkdwn.len() > 4000 {
            let mut end = 3997;
            while end > 0 && !mrkdwn.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &mrkdwn[..end])
        } else {
            mrkdwn.clone()
        };

        let bot_token: SlackApiToken = SlackApiToken::new(self.bot_token.clone().into());
        let session = client.open_session(&bot_token);

        let channel: SlackChannelId = channel_str.into();
        let ts: SlackTs = ts_str.into();

        let req = SlackApiChatUpdateRequest::new(
            channel,
            SlackMessageContent::new().with_text(display.clone()),
            ts,
        );

        session
            .chat_update(&req)
            .await
            .map_err(|e| format!("Slack chat.update: {}", e))?;

        Ok(SentMessage {
            message_id,
            chat_id,
            text: Some(mrkdwn),
        })
    }

    async fn delete_message(&self, _chat_id: i64, message_id: i32) -> Result<bool, String> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| "Slack client not initialized".to_string())?;

        let (resolved_chat_id, ts_str) = self
            .state
            .resolve_message_id(message_id)
            .ok_or_else(|| format!("Unknown msg ID: {}", message_id))?;

        let channel_str = self
            .state
            .channel_for_chat(resolved_chat_id)
            .ok_or_else(|| format!("Unknown chat_id for msg: {}", message_id))?;

        let bot_token: SlackApiToken = SlackApiToken::new(self.bot_token.clone().into());
        let session = client.open_session(&bot_token);

        let channel: SlackChannelId = channel_str.into();
        let ts: SlackTs = ts_str.into();

        let req = SlackApiChatDeleteRequest::new(channel, ts);
        session
            .chat_delete(&req)
            .await
            .map_err(|e| format!("Slack chat.delete: {}", e))?;

        Ok(true)
    }

    async fn send_chat_action(&self, _chat_id: i64, _action: &str) -> Result<bool, String> {
        // Slack Socket Mode/Web API does not expose the RTM `typing` action.
        Ok(true)
    }

    async fn send_document(
        &self,
        chat_id: i64,
        data: &[u8],
        filename: &str,
        caption: Option<&str>,
    ) -> Result<SentMessage, String> {
        let channel_str = self
            .state
            .channel_for_chat(chat_id)
            .ok_or_else(|| format!("Unknown chat_id: {}", chat_id))?;

        // Includes the potentially large raw upload body, so use the same
        // bounded 120-second budget as file downloads.
        let http = bridge_api_client(BRIDGE_FILE_DOWNLOAD_TIMEOUT_SECS)?;
        let auth = format!("Bearer {}", self.bot_token);

        // Step 1: Get upload URL
        let url_resp: serde_json::Value = http
            .post("https://slack.com/api/files.getUploadURLExternal")
            .header("Authorization", &auth)
            .header("Content-Type", "application/json; charset=utf-8")
            .json(&serde_json::json!({
                "filename": filename,
                "length": data.len(),
            }))
            .send()
            .await
            .map_err(|e| format!("Slack getUploadURLExternal request: {}", e))?
            .json()
            .await
            .map_err(|e| format!("Slack getUploadURLExternal parse: {}", e))?;

        if !url_resp
            .get("ok")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(format!("Slack getUploadURLExternal failed: {}", url_resp));
        }
        let upload_url = url_resp
            .get("upload_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing upload_url".to_string())?
            .to_string();
        let file_id_resp = url_resp
            .get("file_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing file_id".to_string())?
            .to_string();

        // Step 2: POST raw bytes to upload URL
        let upload_resp = http
            .post(&upload_url)
            .body(data.to_vec())
            .send()
            .await
            .map_err(|e| format!("Slack file upload: {}", e))?;
        if !upload_resp.status().is_success() {
            return Err(format!(
                "Slack file upload failed: {}",
                upload_resp.status()
            ));
        }

        // Step 3: Complete upload (associate with channel)
        let mut complete_body = serde_json::json!({
            "files": [{"id": file_id_resp.clone(), "title": filename}],
            "channel_id": channel_str.clone(),
        });
        if let Some(cap) = caption {
            // Captions arrive in Telegram-HTML form (cokacdir uses ParseMode::Html
            // throughout). Convert to mrkdwn so formatting survives, matching the
            // behavior of `send_message` rather than discarding tags via strip_html.
            let cap_mrkdwn = telegram_html_to_slack_mrkdwn(cap);
            let cap_trimmed = cap_mrkdwn.trim();
            if !cap_trimmed.is_empty() && cap_mrkdwn.len() <= 4000 {
                complete_body["initial_comment"] = serde_json::Value::String(cap_mrkdwn);
            }
        }
        let complete_body_str =
            serde_json::to_string(&complete_body).map_err(|e| format!("JSON encode: {}", e))?;

        // Reserve a tg_msg_id and register the pending mapping BEFORE triggering
        // the upload completion. completeUploadExternal causes Slack to auto-post
        // a file_share message and emit the corresponding event over Socket Mode
        // — that event can race with the HTTP response, so the binding must be in
        // place before either returns.
        let msg_id = self.state.msg_counter.fetch_add(1, Ordering::Relaxed);
        self.state
            .register_pending_upload(&file_id_resp, chat_id, msg_id);

        let complete_resp_result: Result<serde_json::Value, String> = async {
            let resp = http
                .post("https://slack.com/api/files.completeUploadExternal")
                .header("Authorization", &auth)
                .header("Content-Type", "application/json; charset=utf-8")
                .body(complete_body_str)
                .send()
                .await
                .map_err(|e| format!("Slack completeUploadExternal: {}", e))?;
            resp.json::<serde_json::Value>()
                .await
                .map_err(|e| format!("Slack completeUploadExternal parse: {}", e))
        }
        .await;

        let complete_resp = match complete_resp_result {
            Ok(v) => v,
            Err(e) => {
                // Roll back the pending entry so it does not leak.
                let _ = self.state.take_pending_upload(&file_id_resp);
                return Err(e);
            }
        };
        if !complete_resp
            .get("ok")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let _ = self.state.take_pending_upload(&file_id_resp);
            return Err(format!(
                "Slack completeUploadExternal failed: {}",
                complete_resp
            ));
        }
        if let Some(ts) = extract_slack_complete_upload_ts(&complete_resp, &channel_str) {
            let _ = self.state.take_pending_upload(&file_id_resp);
            self.state.bind_message_id(msg_id, chat_id, &ts);
        }

        Ok(SentMessage {
            message_id: msg_id,
            chat_id,
            text: None,
        })
    }

    async fn get_file(&self, file_id: &str) -> Result<FileInfo, String> {
        let stored = self
            .state
            .get_stored_file(file_id)
            .ok_or_else(|| format!("File not found: {}", file_id))?;
        Ok(FileInfo {
            file_id: file_id.to_string(),
            file_path: stored.url_private,
            file_size: stored.size,
        })
    }

    async fn get_file_data(&self, file_path: &str) -> Result<Vec<u8>, String> {
        // The Authorization header below carries the Slack bot token.
        // file_path arrives via an HTTP path component, so without a host
        // check we'd happily ship the bot token to any attacker-controlled
        // URL. Restrict to Slack-owned hosts.
        if !is_allowed_slack_file_url(file_path) {
            return Err(format!("Refused to fetch non-Slack URL: {}", file_path));
        }
        let auth = format!("Bearer {}", self.bot_token);
        let resp = bridge_file_download_client()?
            .get(file_path)
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("Slack file download: {}", e))?;
        read_download_response_limited(resp, BRIDGE_FILE_DOWNLOAD_MAX_BYTES, "Slack file download")
            .await
    }
}

fn is_allowed_slack_file_url(url: &str) -> bool {
    let rest = match url.strip_prefix("https://") {
        Some(r) => r,
        None => return false,
    };
    // Host ends at '/', '?', or '#' — see is_allowed_discord_file_url for why.
    let host_end = rest
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let host = &rest[..host_end].to_ascii_lowercase();
    // Slack file URLs are served from files.slack.com; allow subdomains of
    // slack.com only on a path-segment boundary so "files.slack.com.evil"
    // does not match.
    host == "files.slack.com" || host == "slack.com" || host.ends_with(".slack.com")
}

/// Process a Slack push event from the Socket Mode listener.
/// Filters bot's own messages, extracts text/files/sender info,
/// registers ID mappings, and dispatches an IncomingMessage to the proxy.
async fn process_slack_push_event(
    evt: SlackPushEventCallback,
    state: Arc<SlackState>,
    tx: mpsc::Sender<IncomingMessage>,
) -> Result<(), String> {
    let body = evt.event;
    let msg = match body {
        SlackEventCallbackBody::Message(m) => m,
        SlackEventCallbackBody::AppMention(m) => {
            return process_slack_app_mention_event(m, state, tx).await;
        }
        _ => return Ok(()),
    };

    if !matches!(
        msg.subtype,
        None | Some(SlackMessageEventType::FileShare)
            | Some(SlackMessageEventType::ThreadBroadcast)
    ) {
        return Ok(());
    }

    let bot_id_self = state.bot_id_value();
    let bot_user_id_self = state.bot_user_id_value();
    let from_user_id = msg.sender.user.as_ref().map(|u| u.to_string());
    let bot_id_match =
        bot_id_self.is_some() && msg.sender.bot_id.as_ref().map(|b| b.to_string()) == bot_id_self;
    let user_id_match = bot_user_id_self.is_some() && from_user_id == bot_user_id_self;
    let is_own = bot_id_match || user_id_match;

    // Bot's own file_share event is the only signal that links a Slack file_id
    // to its actual posted message ts. Capture that mapping before discarding.
    if is_own {
        if matches!(msg.subtype, Some(SlackMessageEventType::FileShare)) {
            if let (Some(channel_id), Some(content)) =
                (msg.origin.channel.as_ref(), msg.content.as_ref())
            {
                let channel_str = channel_id.to_string();
                let ts_str = msg.origin.ts.to_string();
                let event_chat_id = slack_chat_id(&channel_str);
                if let Some(files) = content.files.as_ref() {
                    for f in files {
                        let f_id = f.id.to_string();
                        if let Some((expected_chat_id, tg_msg_id)) =
                            state.take_pending_upload(&f_id)
                        {
                            if expected_chat_id == event_chat_id {
                                state.bind_message_id(tg_msg_id, event_chat_id, &ts_str);
                            } else {
                                // Channel mismatch — restore so a later event can claim it.
                                state.register_pending_upload(&f_id, expected_chat_id, tg_msg_id);
                            }
                        }
                    }
                }
            }
        }
        return Ok(());
    }

    let channel_str = match msg.origin.channel.as_ref() {
        Some(c) => c.to_string(),
        None => return Ok(()),
    };
    let ts_str = msg.origin.ts.to_string();
    let chat_id = slack_chat_id(&channel_str);
    state.register_channel(chat_id, &channel_str);
    if !state.claim_incoming_event(chat_id, &ts_str) {
        return Ok(());
    }

    let from_id_str = from_user_id.unwrap_or_else(|| "unknown".to_string());
    let from_id_u64 = (slack_id_hash(&from_id_str) as u64) & 0x7FFFFFFFFFFFFFFF;
    let is_group = !channel_str.starts_with('D');
    let group_title = if is_group {
        state.team.lock().unwrap_or_else(|e| e.into_inner()).clone()
    } else {
        None
    };

    let content = msg.content.as_ref();
    let text = content
        .and_then(|c| c.text.clone())
        .map(|t| normalize_slack_text_for_telegram(&t, &state))
        .filter(|t| !t.is_empty());
    let files: Vec<_> = content
        .and_then(|c| c.files.as_ref())
        .cloned()
        .unwrap_or_default();

    if files.is_empty() {
        let tg_msg_id = state.map_message_id(chat_id, &ts_str);
        let incoming = IncomingMessage {
            chat_id,
            message_id: tg_msg_id,
            from_id: from_id_u64,
            from_first_name: from_id_str.clone(),
            from_username: Some(from_id_str),
            text,
            is_group,
            group_title,
            document: None,
            photo: None,
            caption: None,
            media_group_id: None,
        };
        tx.send(incoming)
            .await
            .map_err(|e| format!("send to channel: {}", e))?;
        return Ok(());
    }

    // Fan out: one IncomingMessage per file. The caption rides on the first
    // file only; subsequent files share the same Slack ts (reverse-mapped via
    // allocate_extra_message_id) so Slack-side reply lookups still resolve.
    // All fan-outs share a synthetic media_group_id so the downstream prefix
    // check can recognize i>=1 messages (whose captions are None) as album
    // continuations of the i=0 accepted upload.
    let group_id = format!("s:{}", ts_str);
    eprintln!(
        "  [slack] fan-out: chat_id={}, ts={}, files={}, media_group_id={}",
        chat_id,
        ts_str,
        files.len(),
        group_id
    );
    for (i, f) in files.iter().enumerate() {
        let tg_msg_id = if i == 0 {
            state.map_message_id(chat_id, &ts_str)
        } else {
            state.allocate_extra_message_id(chat_id, &ts_str)
        };
        let mime = f.mimetype.clone().map(|m| m.to_string());
        let is_image = mime
            .as_deref()
            .map(|m| m.starts_with("image/"))
            .unwrap_or(false);
        let url = f
            .url_private
            .as_ref()
            .map(|u| u.to_string())
            .unwrap_or_default();
        let fname = f
            .name
            .clone()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "file".to_string());
        // slack-morphism v2 SlackFile does not expose size/original_w/h fields
        let size: Option<u64> = None;
        let stored_id = state.store_file(url, fname.clone(), mime.clone(), size);
        let (document, photo) = if is_image {
            (
                None,
                Some(vec![PhotoAttachment {
                    file_id: stored_id,
                    width: 0,
                    height: 0,
                    file_size: size,
                }]),
            )
        } else {
            (
                Some(FileAttachment {
                    file_id: stored_id,
                    file_name: Some(fname),
                    mime_type: mime,
                    file_size: size,
                }),
                None,
            )
        };
        let caption = if i == 0 { text.clone() } else { None };
        let incoming = IncomingMessage {
            chat_id,
            message_id: tg_msg_id,
            from_id: from_id_u64,
            from_first_name: from_id_str.clone(),
            from_username: Some(from_id_str.clone()),
            text: None,
            is_group,
            group_title: group_title.clone(),
            document,
            photo,
            caption,
            media_group_id: Some(group_id.clone()),
        };
        tx.send(incoming)
            .await
            .map_err(|e| format!("send to channel: {}", e))?;
    }
    Ok(())
}

async fn process_slack_app_mention_event(
    mention: SlackAppMentionEvent,
    state: Arc<SlackState>,
    tx: mpsc::Sender<IncomingMessage>,
) -> Result<(), String> {
    let channel_str = mention.channel.to_string();
    let ts_str = mention.origin.ts.to_string();
    let chat_id = slack_chat_id(&channel_str);
    state.register_channel(chat_id, &channel_str);
    if !state.claim_incoming_event(chat_id, &ts_str) {
        return Ok(());
    }

    let from_id_str = mention.user.to_string();
    let from_id_u64 = (slack_id_hash(&from_id_str) as u64) & 0x7FFFFFFFFFFFFFFF;
    let is_group = !channel_str.starts_with('D');
    let group_title = if is_group {
        state.team.lock().unwrap_or_else(|e| e.into_inner()).clone()
    } else {
        None
    };

    let text = mention
        .content
        .text
        .as_ref()
        .map(|t| normalize_slack_text_for_telegram(t, &state))
        .filter(|t| !t.is_empty());
    // Slack delivers both `app_mention` and `message.*` events for the same ts
    // when the bot is in the channel. claim_incoming_event makes "first one
    // wins"; whichever wins must therefore carry any attached files. Mirror
    // the message-event fan-out below so file-bearing mentions are not lost
    // when the app_mention event arrives first.
    let files: Vec<_> = mention.content.files.clone().unwrap_or_default();

    if files.is_empty() {
        let tg_msg_id = state.map_message_id(chat_id, &ts_str);
        let incoming = IncomingMessage {
            chat_id,
            message_id: tg_msg_id,
            from_id: from_id_u64,
            from_first_name: from_id_str.clone(),
            from_username: Some(from_id_str),
            text,
            is_group,
            group_title,
            document: None,
            photo: None,
            caption: None,
            media_group_id: None,
        };
        tx.send(incoming)
            .await
            .map_err(|e| format!("send to channel: {}", e))?;
        return Ok(());
    }

    let group_id = format!("s:{}", ts_str);
    eprintln!(
        "  [slack] app_mention fan-out: chat_id={}, ts={}, files={}, media_group_id={}",
        chat_id,
        ts_str,
        files.len(),
        group_id
    );
    for (i, f) in files.iter().enumerate() {
        let tg_msg_id = if i == 0 {
            state.map_message_id(chat_id, &ts_str)
        } else {
            state.allocate_extra_message_id(chat_id, &ts_str)
        };
        let mime = f.mimetype.clone().map(|m| m.to_string());
        let is_image = mime
            .as_deref()
            .map(|m| m.starts_with("image/"))
            .unwrap_or(false);
        let url = f
            .url_private
            .as_ref()
            .map(|u| u.to_string())
            .unwrap_or_default();
        let fname = f
            .name
            .clone()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "file".to_string());
        let size: Option<u64> = None;
        let stored_id = state.store_file(url, fname.clone(), mime.clone(), size);
        let (document, photo) = if is_image {
            (
                None,
                Some(vec![PhotoAttachment {
                    file_id: stored_id,
                    width: 0,
                    height: 0,
                    file_size: size,
                }]),
            )
        } else {
            (
                Some(FileAttachment {
                    file_id: stored_id,
                    file_name: Some(fname),
                    mime_type: mime,
                    file_size: size,
                }),
                None,
            )
        };
        let caption = if i == 0 { text.clone() } else { None };
        let incoming = IncomingMessage {
            chat_id,
            message_id: tg_msg_id,
            from_id: from_id_u64,
            from_first_name: from_id_str.clone(),
            from_username: Some(from_id_str.clone()),
            text: None,
            is_group,
            group_title: group_title.clone(),
            document,
            photo,
            caption,
            media_group_id: Some(group_id.clone()),
        };
        tx.send(incoming)
            .await
            .map_err(|e| format!("send to channel: {}", e))?;
    }

    Ok(())
}

// ============================================================
// Public entry point
// ============================================================

/// Run the messenger bridge.
///
/// `backend_name`: "console", "discord", "slack", etc.
/// `args`: backend-specific arguments
pub async fn run_bridge(backend_name: &str, args: &[String]) -> BridgeExit {
    let mut backend: Box<dyn MessengerBackend> = match backend_name {
        "console" => Box::new(ConsoleBackend::new()),
        "discord" => {
            let token = match args.first() {
                Some(t) => t.clone(),
                None => {
                    eprintln!("Error: Discord bridge requires a bot token");
                    eprintln!("Usage: cokacdir --ccserver <DISCORD_BOT_TOKEN>");
                    return BridgeExit::Fatal;
                }
            };
            Box::new(DiscordBackend::new(token))
        }
        "slack" => {
            if args.len() < 2 {
                eprintln!("Error: Slack bridge requires both bot token (xoxb-) and app-level token (xapp-)");
                eprintln!("Usage: cokacdir --ccserver slack:<xoxb-...>,<xapp-...>");
                return BridgeExit::Fatal;
            }
            Box::new(SlackBackend::new(args[0].clone(), args[1].clone()))
        }
        other => {
            eprintln!(
                "Error: Unknown messenger backend '{}'. Supported: console, discord, slack",
                other
            );
            return BridgeExit::Fatal;
        }
    };

    // Initialize backend
    let bot_info = match backend.init().await {
        Ok(info) => {
            println!("  ✓ Backend: {} (@{})", info.first_name, info.username);
            info
        }
        Err(e) => {
            eprintln!("  ✗ Backend init failed: {}", e);
            return BridgeExit::Fatal;
        }
    };

    // Message channel: backend → proxy → teloxide
    let (tx, rx) = mpsc::channel(256);

    // Start backend listener — the returned `JoinHandle` resolves only when
    // the backend's gateway listener dies (token revoked, persistent
    // disconnect, panic). We watch it alongside `run_bot` below so a dead
    // backend cannot silently masquerade as a healthy bot (proxy still up,
    // no messages flowing) — the same failure-class as Telegram 401/409,
    // which `polling_loop` already detects and surfaces.
    let backend_arc: Arc<dyn MessengerBackend> = Arc::from(backend);
    let backend_handle = match backend_arc.start(tx).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("  ✗ Backend listener init failed: {}", e);
            return BridgeExit::Fatal;
        }
    };
    // Independent aborter so we can stop the listener even after the
    // `JoinHandle` is moved into `tokio::select!` below — `select!`
    // drops the un-selected branch's future, but dropping a tokio
    // `JoinHandle` does NOT abort the underlying task. Without an
    // explicit abort the gateway listener would leak in multi-bot
    // setups when a sibling bot's bridge exits first (process::exit
    // used to mask this leak by tearing down the runtime).
    let backend_aborter = backend_handle.abort_handle();

    // Generate a stable bridge token for telegram.rs settings storage.
    // Hash the real token to avoid exposing it in URL paths and debug logs.
    let token_discriminator = args
        .first()
        .map(|t| crate::services::telegram::token_hash(t))
        .unwrap_or_else(|| "default".to_string());
    let bridge_token = format!("bridge_{}_{}", backend_name, token_discriminator);

    // Proxy state
    let state = Arc::new(ProxyState {
        backend: backend_arc,
        bot_info,
        update_rx: Mutex::new(rx),
        update_id_counter: AtomicI64::new(1),
        expected_token: bridge_token.clone(),
    });

    // Bind local proxy server
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("  ✗ Failed to bind proxy server: {}", e);
            // Best-effort abort of the already-spawned backend listener
            // so a multi-bot run doesn't leak it on this early-error
            // path. (No-op if it has already exited.)
            backend_aborter.abort();
            return BridgeExit::Fatal;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let api_url = format!("http://127.0.0.1:{}", port);
    println!("  ✓ Proxy: {}", api_url);

    // Start proxy server. Held as a `JoinHandle` rather than detached so
    // we can abort it on bridge exit — otherwise the listener would
    // keep its TCP port bound (causing port collisions on bot restart)
    // and outlive its own bot in multi-bot setups.
    let proxy_state = state.clone();
    let proxy_handle = tokio::spawn(run_proxy_server(proxy_state, listener));

    // Run the existing telegram bot logic — it connects to our proxy.
    // Race the backend handle so a backend death (gateway disconnect with
    // no recovery, token revocation, panic) is surfaced as
    // `BridgeExit::Fatal` instead of leaving a vegetative proxy +
    // teloxide pair that silently never delivers messages. The actual
    // process-exit decision is deferred to `main` so a sibling bot in
    // a multi-bot run is not killed by one backend's death.
    let exit = tokio::select! {
        bot_exit = crate::services::telegram::run_bot(&bridge_token, Some(&api_url)) => {
            // Bot exited. Forward `BotExit::Fatal` (revoked token,
            // persistent `Conflict`) as `BridgeExit::Fatal` so the
            // outermost caller surfaces a non-zero process exit and the
            // supervisor restarts us — without this mapping, run_bot's
            // fatal signal is silently lost on the bridge path and the
            // bot stays dead with exit code 0. `backend_handle` is
            // dropped by the select but that does NOT abort the
            // underlying task — the explicit `backend_aborter.abort()`
            // below does.
            match bot_exit {
                crate::services::telegram::BotExit::Fatal => BridgeExit::Fatal,
                crate::services::telegram::BotExit::Graceful => BridgeExit::Graceful,
            }
        }
        res = backend_handle => {
            eprintln!("  ✗ Backend listener stopped — bot can no longer receive messages.");
            match res {
                Ok(()) => eprintln!(
                    "    Reason: backend listener exited (token revoked, persistent gateway disconnect, or remote shutdown)."
                ),
                Err(join_err) if join_err.is_panic() => eprintln!(
                    "    Reason: backend task PANICKED: {}", join_err
                ),
                Err(join_err) => eprintln!(
                    "    Reason: backend task error: {}", join_err
                ),
            }
            eprintln!("    Fix the underlying issue and restart cokacdir.");
            BridgeExit::Fatal
        }
    };

    // Cleanup: abort the listener and proxy server before returning so
    // they don't leak in a multi-bot deployment where the runtime keeps
    // running for sibling bots. Aborting an already-completed task is a
    // no-op, so calling on the Fatal arm (where `backend_handle` already
    // resolved) is safe.
    backend_aborter.abort();
    proxy_handle.abort();

    exit
}
