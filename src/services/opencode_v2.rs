//! OpenCode 2.x execution adapter.
//!
//! opencode 2.x (verified against 2.0.24) replaced the 1.x server that the
//! parent module drives:
//! - every API route lives under `/api` behind HTTP basic auth (`opencode` /
//!   `OPENCODE_PASSWORD`); the old unprefixed routes now serve the web app;
//! - `/api/event` streams typed events (`session.text.delta`,
//!   `session.tool.success`, `session.created`, ...) without the 1.x
//!   `/global/event` envelope;
//! - a session's history is a list of typed messages: one Assistant message
//!   per model step, and an `idle` marker closing every turn;
//! - background subagents run as child sessions and report back with a
//!   synthetic message, which resumes the parent after it already went idle;
//! - an execution "claims" its session until it settles, and every OpenCode
//!   server resumes still-claimed sessions when it starts. A stopped turn must
//!   therefore be interrupted through the API before its server goes away, or
//!   it runs again the next time any OpenCode server starts.

use super::{
    find_double_newline, kill_serve_process_group, log_preview, missing_sse_terminal_delta,
    normalize_opencode_params, normalize_tool_name, opencode_debug, opencode_message_error_text,
    prepare_requested_system_prompt, random_base62, resolve_opencode_path,
    send_serve_stream_message, serve_cancel_hit, urlencoded, PollError, PreKillHookGuard,
    ReceiverDropSignal, ServeTurnTerminal, POLL_INTERVAL, POLL_MAX_CONSECUTIVE_ERRORS,
    POLL_REQUEST_TIMEOUT, POLL_REQUIRED_CONSECUTIVE, SERVE_READY_TIMEOUT,
};
use crate::services::claude::{send_success_terminal, CancelToken, StreamMessage};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Upper bound on waiting for the event stream to deliver the end of a
/// settled turn before the final text is repaired from the history instead.
const STREAM_CATCH_UP_TIMEOUT: Duration = Duration::from_secs(2);
/// Page size for the session message API (its maximum).
const MESSAGE_PAGE_LIMIT: usize = 200;
/// Upper bound on message pages read while locating the turn's prompt.
const MESSAGE_PAGE_MAX: usize = 50;
/// Upper bound on `parentID` hops when deciding whether a session belongs to
/// the turn's session tree.
const SESSION_TREE_MAX_DEPTH: usize = 8;
/// How long the first event may take before the prompt is sent anyway.
const EVENT_STREAM_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on the time `/stop` spends asking the server to interrupt the turn.
const INTERRUPT_DEADLINE: Duration = Duration::from_secs(2);
/// Bound on the turn's setup requests (session, agent, model, prompt). The
/// server loads a directory lazily, so the first of them that needs it bears
/// the load: installing a configured plugin there took 27.6s, measured, which
/// the 10s poll bound would cut off. 1.x did that work before readiness.
const SETUP_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
/// Feedback OpenCode's own non-interactive `run` attaches when it cancels a
/// question, so the model continues instead of waiting for an answer.
const QUESTION_CANCELLED_FEEDBACK: &str = "This non-interactive run cannot ask the user questions, so the question was cancelled. Continue without an answer; make reasonable assumptions and state them.";

// ============================================================
// Private `opencode serve`
// ============================================================

/// A private `opencode serve --stdio` instance. The server treats EOF on its
/// stdin as the end of its lease, so it also exits when cokacdir dies before
/// running any cleanup.
struct Server {
    child: Option<tokio::process::Child>,
    stdin: Option<tokio::process::ChildStdin>,
    base_url: String,
    /// `Authorization` header value for every `/api` request.
    auth: String,
    /// Holds the server PID for /stop; forgotten once the server is reaped.
    cancel_token: Option<Arc<CancelToken>>,
}

impl Server {
    /// The server's exit status once its process has exited (`None` while
    /// it runs). A dead server is known directly, not inferred from failing
    /// requests.
    fn exit_status(&mut self) -> Option<String> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(e) => {
                opencode_debug(&format!("[v2.serve] try_wait failed: {}", e));
                None
            }
        }
    }

    async fn shutdown(&mut self) {
        // Closing the lease pipe is the server's own graceful shutdown.
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            if tokio::time::timeout(Duration::from_secs(3), child.wait())
                .await
                .is_err()
            {
                opencode_debug("[v2.serve] server ignored its lease EOF; killing");
            }
            // Tool subprocesses may outlive the server; clear the whole group.
            kill_serve_process_group(pid);
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            // A recycled PID must never be signalled by a later /stop.
            if let (Some(token), Some(pid)) = (self.cancel_token.as_ref(), pid) {
                token.clear_child_pid(pid);
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_serve_process_group(child.id());
            let _ = child.start_kill();
        }
    }
}

/// Parse the `--stdio` readiness line: `{"url":"http://127.0.0.1:PORT"}`.
fn parse_ready_line(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let url = value.get("url")?.as_str()?.trim_end_matches('/');
    (url.starts_with("http://") || url.starts_with("https://")).then(|| url.to_string())
}

async fn spawn_server(
    working_dir: &str,
    cancel_token: Option<&Arc<CancelToken>>,
) -> Result<Server, String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use tokio::io::{AsyncBufReadExt, BufReader};

    let bin = resolve_opencode_path().unwrap_or_else(|| "opencode".to_string());
    opencode_debug(&format!(
        "[v2.serve.spawn] bin={} working_dir={}",
        bin, working_dir
    ));
    // The server takes its password from the environment; stdio mode removes
    // it from the environment its tools inherit.
    let password = random_base62(43);
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(["serve", "--stdio", "--port", "0", "--hostname", "127.0.0.1"])
        .current_dir(working_dir)
        .env("PWD", working_dir)
        .env("OPENCODE_PASSWORD", &password)
        .env("PATH", crate::services::claude::enhanced_path_for_bin(&bin))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Own process group, so the whole server family can be killed at once.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    crate::services::claude::attach_cancel_cgroup_tokio(&mut cmd, cancel_token);

    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {}", bin, e))?;
    opencode_debug(&format!("[v2.serve.spawn] spawned PID={:?}", child.id()));

    // Register the PID at once so /stop can kill the server while it boots.
    if let Some(token) = cancel_token {
        if let Some(pid) = child.id() {
            let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(pid);
        }
        if token.cancelled.load(Ordering::Relaxed) {
            opencode_debug("[v2.serve.spawn] cancelled after PID registration");
            token.cancel_now();
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            return Err("cancelled before opencode serve became ready".to_string());
        }
    }

    let stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture opencode serve stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture opencode serve stderr".to_string())?;
    // stderr is diagnostic only; drain it from the start so it never fills.
    tokio::task::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            opencode_debug(&format!("[v2.serve.stderr] {}", log_preview(&line, 200)));
        }
    });

    let mut stdout_lines = BufReader::new(stdout).lines();
    let base_url = tokio::time::timeout(SERVE_READY_TIMEOUT, async {
        loop {
            match stdout_lines.next_line().await {
                Ok(Some(line)) => {
                    opencode_debug(&format!("[v2.serve.stdout] {}", log_preview(&line, 200)));
                    if let Some(url) = parse_ready_line(&line) {
                        return Ok::<String, String>(url);
                    }
                }
                Ok(None) => return Err("opencode serve exited before reporting readiness".into()),
                Err(e) => return Err(format!("stdout read error: {}", e)),
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
    // Keep draining stdout so later server writes cannot hit a closed pipe.
    tokio::task::spawn(async move {
        while let Ok(Some(line)) = stdout_lines.next_line().await {
            opencode_debug(&format!("[v2.serve.stdout] {}", log_preview(&line, 200)));
        }
    });

    let auth = format!(
        "Basic {}",
        STANDARD.encode(format!("opencode:{}", password))
    );
    Ok(Server {
        child: Some(child),
        stdin,
        base_url,
        auth,
        cancel_token: cancel_token.cloned(),
    })
}

// ============================================================
// HTTP API
// ============================================================

#[derive(Debug)]
struct ApiError {
    status: Option<u16>,
    detail: String,
}

impl ApiError {
    fn not_found(&self) -> bool {
        self.status == Some(404)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

#[derive(Clone)]
struct Api {
    client: reqwest::Client,
    base_url: String,
    auth: String,
}

impl Api {
    fn new(server: &Server) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(POLL_REQUEST_TIMEOUT)
            .build()
            .map_err(|e| format!("HTTP client init failed: {}", e))?;
        Ok(Self {
            client,
            base_url: server.base_url.clone(),
            auth: server.auth.clone(),
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        self.send(method, path, body, &[], None).await
    }

    /// `call` for the turn's setup requests, bounded by `SETUP_REQUEST_TIMEOUT`
    /// instead of the client's poll bound.
    async fn call_setup(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        self.send(method, path, body, &[], Some(SETUP_REQUEST_TIMEOUT))
            .await
    }

    async fn call_with_headers(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        headers: &[(&'static str, String)],
    ) -> Result<Value, ApiError> {
        self.send(method, path, body, headers, None).await
    }

    /// Send one request and return its JSON body (`Null` for an empty one).
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        headers: &[(&'static str, String)],
        timeout: Option<Duration>,
    ) -> Result<Value, ApiError> {
        let mut request = self
            .client
            .request(method.clone(), format!("{}{}", self.base_url, path))
            .header(reqwest::header::AUTHORIZATION, self.auth.as_str());
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        if let Some(body) = body {
            let text = serde_json::to_string(body).map_err(|e| ApiError {
                status: None,
                detail: format!("{} {}: serialize: {}", method, path, e),
            })?;
            request = request
                .header("content-type", "application/json")
                .body(text);
        }
        let response = request.send().await.map_err(|e| ApiError {
            status: None,
            detail: format!("{} {}: {}", method, path, e),
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|e| ApiError {
            status: Some(status.as_u16()),
            detail: format!("{} {}: body: {}", method, path, e),
        })?;
        if !status.is_success() {
            return Err(ApiError {
                status: Some(status.as_u16()),
                detail: format!(
                    "{} {} returned {}: {}",
                    method,
                    path,
                    status,
                    log_preview(&text, 300)
                ),
            });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| ApiError {
            status: Some(status.as_u16()),
            detail: format!(
                "{} {} returned invalid JSON: {} ({})",
                method,
                path,
                e,
                log_preview(&text, 200)
            ),
        })
    }
}

fn session_path(session_id: &str, rest: &str) -> String {
    format!("/api/session/{}{}", urlencoded(session_id), rest)
}

/// Sessions with a foreground execution on this server.
async fn active_sessions(api: &Api) -> Result<HashSet<String>, String> {
    let value = api
        .call(reqwest::Method::GET, "/api/session/active", None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(value
        .get("data")
        .and_then(Value::as_object)
        .map(|sessions| sessions.keys().cloned().collect())
        .unwrap_or_default())
}

/// `provider/model` or `provider/model#variant` as the API's `Model.Ref`.
fn parse_model_ref(model: &str) -> Result<Value, String> {
    let invalid = || {
        format!(
            "Invalid OpenCode model '{}': expected provider/model",
            model
        )
    };
    let (provider, rest) = model.split_once('/').ok_or_else(invalid)?;
    let (id, variant) = match rest.split_once('#') {
        Some((id, variant)) => (id, Some(variant).filter(|v| !v.is_empty())),
        None => (rest, None),
    };
    if provider.is_empty() || id.is_empty() {
        return Err(invalid());
    }
    let mut reference = json!({ "providerID": provider, "id": id });
    if let Some(variant) = variant {
        reference["variant"] = json!(variant);
    }
    Ok(reference)
}

async fn create_session(api: &Api, working_dir: &str) -> Result<String, String> {
    // No title: OpenCode generates one from the conversation, as it does for
    // its own sessions.
    let body = json!({ "location": { "directory": working_dir } });
    let created = api
        .call_setup(reqwest::Method::POST, "/api/session", Some(&body))
        .await
        .map_err(|e| e.to_string())?;
    created
        .get("data")
        .and_then(|data| data.get("id"))
        .and_then(Value::as_str)
        .filter(|id| crate::services::process::is_valid_session_id(id))
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "session create: no valid id in response: {}",
                log_preview(&created.to_string(), 200)
            )
        })
}

// ============================================================
// Session tree (parent + descendants) and its blockers
// ============================================================

/// The turn's sessions: the parent plus every descendant (subagents run as
/// child sessions), learned from `session.created` events and, for sessions
/// the stream did not announce, from their `parentID` chain.
struct SessionTree {
    /// Parent first, then descendants in discovery order.
    owned: Vec<String>,
    /// Sessions known to belong to someone else (e.g. resumed at startup).
    foreign: HashSet<String>,
}

type SharedTree = Arc<Mutex<SessionTree>>;

impl SessionTree {
    fn new(parent: &str) -> Self {
        Self {
            owned: vec![parent.to_string()],
            foreign: HashSet::new(),
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.owned.iter().any(|owned| owned == id)
    }

    fn add(&mut self, id: &str) {
        if !self.contains(id) {
            self.owned.push(id.to_string());
        }
    }

    /// Descendants before their ancestors, the parent last: interrupting a
    /// child can resume its parent, so parents are stopped after children.
    fn children_first(&self) -> Vec<String> {
        self.owned.iter().rev().cloned().collect()
    }
}

fn lock_tree(tree: &SharedTree) -> std::sync::MutexGuard<'_, SessionTree> {
    tree.lock().unwrap_or_else(|e| e.into_inner())
}

async fn is_owned(api: &Api, tree: &SharedTree, id: &str) -> bool {
    let mut visited: Vec<String> = Vec::new();
    let mut current = id.to_string();
    for _ in 0..SESSION_TREE_MAX_DEPTH {
        {
            let mut tree = lock_tree(tree);
            if tree.contains(&current) {
                for session in &visited {
                    tree.add(session);
                }
                return true;
            }
            if tree.foreign.contains(&current) {
                tree.foreign.extend(visited);
                return false;
            }
        }
        visited.push(current.clone());
        let parent = match api
            .call(reqwest::Method::GET, &session_path(&current, ""), None)
            .await
        {
            Ok(info) => info
                .get("data")
                .and_then(|data| data.get("parentID"))
                .and_then(Value::as_str)
                .map(str::to_string),
            Err(e) => {
                // Unknown for now; ask again on the next poll.
                opencode_debug(&format!("[v2.tree] lookup of {} failed: {}", current, e));
                return false;
            }
        };
        match parent {
            Some(parent) => current = parent,
            None => break,
        }
    }
    lock_tree(tree).foreign.extend(visited);
    false
}

/// Answer what would otherwise block the turn forever, as OpenCode's own
/// non-interactive `run --auto` does: approve permission requests once, and
/// cancel forms (questions, MCP elicitations) — questions with feedback, so
/// the model continues without an answer.
async fn settle_blockers(api: &Api, running: &[String], working_dir: &str) {
    for session_id in running {
        if let Ok(requests) = api
            .call(
                reqwest::Method::GET,
                &session_path(session_id, "/permission"),
                None,
            )
            .await
        {
            for request in requests
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(request_id) = request.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let path = session_path(
                    session_id,
                    &format!("/permission/{}/reply", urlencoded(request_id)),
                );
                let body = json!({ "decision": "once" });
                let result = api.call(reqwest::Method::POST, &path, Some(&body)).await;
                opencode_debug(&format!(
                    "[v2.blockers] approved permission {} action={:?} resources={:?}: {:?}",
                    request_id,
                    request.get("action"),
                    request.get("resources"),
                    result.err().map(|e| e.to_string())
                ));
            }
        }
        if let Ok(forms) = api
            .call(
                reqwest::Method::GET,
                &session_path(session_id, "/form"),
                None,
            )
            .await
        {
            for form in forms
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                cancel_form(api, form, None).await;
            }
        }
    }
    // MCP elicitations wait on a "global" pseudo-session at the location.
    let path = format!(
        "/api/form?location%5Bdirectory%5D={}",
        urlencoded(working_dir)
    );
    if let Ok(forms) = api.call(reqwest::Method::GET, &path, None).await {
        for form in forms
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|form| form.get("sessionID").and_then(Value::as_str) == Some("global"))
        {
            cancel_form(api, form, Some(working_dir)).await;
        }
    }
}

async fn cancel_form(api: &Api, form: &Value, global_directory: Option<&str>) {
    let (Some(form_id), Some(session_id)) = (
        form.get("id").and_then(Value::as_str),
        form.get("sessionID").and_then(Value::as_str),
    ) else {
        return;
    };
    let is_question = form
        .get("metadata")
        .and_then(|metadata| metadata.get("kind"))
        .and_then(Value::as_str)
        == Some("question");
    let mut path = session_path(session_id, &format!("/form/{}", urlencoded(form_id)));
    if is_question {
        path.push_str("?message=");
        path.push_str(&urlencoded(QUESTION_CANCELLED_FEEDBACK));
    }
    let headers = global_directory
        .map(|directory| vec![("x-opencode-directory", urlencoded(directory))])
        .unwrap_or_default();
    let result = api
        .call_with_headers(reqwest::Method::DELETE, &path, None, &headers)
        .await;
    opencode_debug(&format!(
        "[v2.blockers] cancelled form {} (session={} question={}): {:?}",
        form_id,
        session_id,
        is_question,
        result.err().map(|e| e.to_string())
    ));
}

/// Stop every running session of the turn through the API. Repeated briefly,
/// because a cancelled child reports to its parent, which can resume it.
async fn interrupt_session_tree(api: &Api, tree: &SharedTree) {
    for _ in 0..3 {
        let active = match active_sessions(api).await {
            Ok(active) => active,
            Err(e) => {
                opencode_debug(&format!(
                    "[v2.interrupt] active sessions unavailable: {}",
                    e
                ));
                return;
            }
        };
        for id in &active {
            is_owned(api, tree, id).await;
        }
        let targets: Vec<String> = lock_tree(tree)
            .children_first()
            .into_iter()
            .filter(|id| active.contains(id))
            .collect();
        if targets.is_empty() {
            return;
        }
        for session_id in &targets {
            let result = api
                .call(
                    reqwest::Method::POST,
                    &session_path(session_id, "/interrupt"),
                    None,
                )
                .await;
            opencode_debug(&format!(
                "[v2.interrupt] {} -> {:?}",
                session_id,
                result.map_err(|e| e.to_string())
            ));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// `interrupt_session_tree` for `CancelToken::cancel_now`, which runs on an
/// arbitrary thread and kills the server right after. Bounded by
/// `INTERRUPT_DEADLINE`.
///
/// OpenCode answers the interrupt request at once but settles the turn (tool
/// aborted, `idle` marker written) a moment later — about 0.2s, measured. The
/// hook therefore returns only once none of the turn's sessions is running
/// any more; returning on the HTTP answer alone let the kill that follows
/// leave the session claimed mid-execution.
fn interrupt_session_tree_blocking(base_url: String, auth: String, tree: SharedTree) {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let deadline = Instant::now() + INTERRUPT_DEADLINE;
    std::thread::spawn(move || {
        let sessions = lock_tree(&tree).children_first();
        for session_id in &sessions {
            if let Err(e) = blocking_interrupt(&base_url, &auth, session_id) {
                opencode_debug(&format!("[v2.interrupt] {} failed: {}", session_id, e));
            }
        }
        let mut last_round = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(50));
            match blocking_active_sessions(&base_url, &auth) {
                Ok(active) => {
                    let running: Vec<&String> =
                        sessions.iter().filter(|id| active.contains(*id)).collect();
                    if running.is_empty() {
                        opencode_debug("[v2.interrupt] turn settled");
                        break;
                    }
                    // A cancelled child's report can resume its parent; stop
                    // whatever is still running again.
                    if last_round.elapsed() >= Duration::from_millis(200) {
                        for session_id in running {
                            let _ = blocking_interrupt(&base_url, &auth, session_id);
                        }
                        last_round = Instant::now();
                    }
                }
                Err(e) => opencode_debug(&format!("[v2.interrupt] active sessions: {}", e)),
            }
            if Instant::now() >= deadline {
                opencode_debug("[v2.interrupt] turn still running at the deadline");
                break;
            }
        }
        let _ = done_tx.send(());
    });
    let _ = done_rx.recv_timeout(INTERRUPT_DEADLINE);
}

/// Minimal blocking `POST /api/session/{id}/interrupt` to the loopback server.
fn blocking_interrupt(base_url: &str, auth: &str, session_id: &str) -> Result<(), String> {
    let (status_line, _) = blocking_request(
        base_url,
        auth,
        "POST",
        &session_path(session_id, "/interrupt"),
    )?;
    opencode_debug(&format!("[v2.interrupt] {} -> {}", session_id, status_line));
    Ok(())
}

/// Minimal blocking `GET /api/session/active` to the loopback server.
fn blocking_active_sessions(base_url: &str, auth: &str) -> Result<HashSet<String>, String> {
    let (status_line, body) = blocking_request(base_url, auth, "GET", "/api/session/active")?;
    if status_line.split_whitespace().nth(1) != Some("200") {
        return Err(status_line);
    }
    let value: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    Ok(value
        .get("data")
        .and_then(Value::as_object)
        .map(|sessions| sessions.keys().cloned().collect())
        .unwrap_or_default())
}

/// One blocking HTTP/1.1 request without a body to the loopback server.
/// Returns the status line and the response body (the server answers JSON
/// with a `content-length` and closes the connection, so the body is the
/// rest of the stream).
fn blocking_request(
    base_url: &str,
    auth: &str,
    method: &str,
    path: &str,
) -> Result<(String, Vec<u8>), String> {
    use std::io::{Read, Write};

    let authority = base_url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported server URL {}", base_url))?;
    let address: std::net::SocketAddr = authority
        .parse()
        .map_err(|e| format!("server address {}: {}", authority, e))?;
    let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500))
        .map_err(|e| e.to_string())?;
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    let request = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        method, path, authority, auth
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => response.extend_from_slice(&chunk[..read]),
            // A server that keeps the connection open past the body ends the
            // read here; what arrived is still the whole response.
            Err(_) if !response.is_empty() => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "incomplete HTTP response".to_string())?;
    let head = String::from_utf8_lossy(&response[..split]);
    let status_line = head.lines().next().unwrap_or("").to_string();
    Ok((status_line, response[split + 4..].to_vec()))
}

// ============================================================
// Turn messages
// ============================================================

fn message_type(message: &Value) -> &str {
    message.get("type").and_then(Value::as_str).unwrap_or("")
}

/// The turn's messages (oldest first) after the user message `prompt_id`, or
/// `None` while that message is not in the history yet.
async fn turn_messages(
    api: &Api,
    session_id: &str,
    prompt_id: &str,
) -> Result<Option<Vec<Value>>, String> {
    let mut newest_first: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MESSAGE_PAGE_MAX {
        let query = match &cursor {
            Some(cursor) => format!(
                "/message?limit={}&cursor={}",
                MESSAGE_PAGE_LIMIT,
                urlencoded(cursor)
            ),
            None => format!("/message?limit={}&order=desc", MESSAGE_PAGE_LIMIT),
        };
        let page = api
            .call(
                reqwest::Method::GET,
                &session_path(session_id, &query),
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        let messages = page
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for message in messages {
            if message.get("id").and_then(Value::as_str) == Some(prompt_id) {
                newest_first.reverse();
                return Ok(Some(newest_first));
            }
            newest_first.push(message);
        }
        cursor = page
            .get("cursor")
            .and_then(|cursor| cursor.get("next"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if cursor.is_none() {
            return Ok(None);
        }
    }
    Ok(None)
}

/// Background subagents launched during the turn that have not reported back.
/// The subagent tool returns at once with `metadata.status == "running"`; the
/// child's completion arrives as a synthetic message whose
/// `metadata.childID` names it, and then resumes the parent.
fn pending_background_subagents(turn: &[Value]) -> Vec<String> {
    let mut reported: HashSet<&str> = HashSet::new();
    for message in turn.iter().filter(|m| message_type(m) == "synthetic") {
        let Some(metadata) = message.get("metadata") else {
            continue;
        };
        if metadata.get("source").and_then(Value::as_str) != Some("subagent") {
            continue;
        }
        if let Some(child) = metadata.get("childID").and_then(Value::as_str) {
            reported.insert(child);
        }
    }
    let mut pending = Vec::new();
    for message in turn.iter().filter(|m| message_type(m) == "assistant") {
        for item in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if item.get("type").and_then(Value::as_str) != Some("tool")
                || item.get("name").and_then(Value::as_str) != Some("subagent")
            {
                continue;
            }
            let Some(state) = item.get("state") else {
                continue;
            };
            let metadata = state.get("metadata");
            let launched_in_background = state.get("status").and_then(Value::as_str)
                == Some("completed")
                && metadata
                    .and_then(|m| m.get("status"))
                    .and_then(Value::as_str)
                    == Some("running");
            if !launched_in_background {
                continue;
            }
            if let Some(child) = metadata
                .and_then(|m| m.get("sessionID"))
                .and_then(Value::as_str)
            {
                if !reported.contains(child) {
                    pending.push(child.to_string());
                }
            }
        }
    }
    pending
}

/// Every turn ends with an `idle` marker; anything after one means the parent
/// resumed (for example on a background report) and is not settled yet.
fn turn_is_closed(turn: &[Value]) -> bool {
    turn.last().map(message_type) == Some("idle")
}

/// Select the turn's authoritative terminal Assistant message. The event
/// stream is a lossy presentation transport; the message history decides.
/// `failure_cause` (from `session.execution.failed`) only explains a turn the
/// history already marks as failed.
fn parse_turn_terminal(
    turn: &[Value],
    failure_cause: Option<&str>,
) -> Result<ServeTurnTerminal, String> {
    let outcome = turn
        .iter()
        .rev()
        .find(|message| message_type(message) == "idle")
        .and_then(|message| message.get("outcome"))
        .and_then(Value::as_str);
    let latest = turn
        .iter()
        .rev()
        .find(|message| message_type(message) == "assistant");
    if let Some(error) = latest
        .and_then(|message| message.get("error"))
        .filter(|error| !error.is_null())
    {
        return Err(opencode_message_error_text(error));
    }
    match outcome {
        Some("succeeded") => {}
        Some("interrupted") => return Err("OpenCode turn was interrupted".to_string()),
        Some("failed") => {
            return Err(failure_cause
                .filter(|cause| !cause.is_empty())
                .unwrap_or("OpenCode turn failed")
                .to_string())
        }
        Some(other) => return Err(format!("OpenCode turn ended with outcome {other:?}")),
        None => return Err("OpenCode turn has no idle marker".to_string()),
    }
    let latest =
        latest.ok_or_else(|| "OpenCode turn ended without an Assistant message".to_string())?;
    let message_id = latest
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if latest
        .get("time")
        .and_then(|time| time.get("completed"))
        .filter(|completed| !completed.is_null())
        .is_none()
    {
        return Err("OpenCode's newest Assistant message is not complete".to_string());
    }
    let finish = latest.get("finish").and_then(Value::as_str).unwrap_or("");
    if matches!(finish, "" | "unknown" | "tool-calls") {
        return Err(format!(
            "OpenCode turn ended at a non-terminal Assistant finish state: {finish:?}"
        ));
    }
    if finish == "error" {
        return Err("OpenCode Assistant generation finished with an error".to_string());
    }
    let result = latest
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default();
    // `stop` is the only normal, complete model finish. Truncated or filtered
    // output can still be shown through Done.result, but is deliberately
    // ineligible for durable memory.
    let assistant_final = (finish == "stop" && !result.trim().is_empty()).then(|| result.clone());
    Ok(ServeTurnTerminal {
        message_id,
        result,
        assistant_final,
    })
}

fn note_poll_error(
    consecutive_errors: &mut u32,
    what: &str,
    error: String,
    start: Instant,
) -> Option<PollError> {
    *consecutive_errors = consecutive_errors.saturating_add(1);
    opencode_debug(&format!(
        "[v2.poll] {} failed ({} in a row): {}",
        what, consecutive_errors, error
    ));
    (*consecutive_errors >= POLL_MAX_CONSECUTIVE_ERRORS).then(|| {
        PollError::Fatal(format!(
            "opencode server unreachable: {} failed {} consecutive times ({:.1}s elapsed): {}",
            what,
            consecutive_errors,
            start.elapsed().as_secs_f64(),
            error
        ))
    })
}

/// IDs of the items waiting in a session's inbox: prompts, subagent reports
/// and compactions the session is still going to run.
async fn inbox_item_ids(api: &Api, session_id: &str) -> Result<Vec<String>, String> {
    let value = api
        .call(
            reqwest::Method::GET,
            &session_path(session_id, "/inbox"),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(value
        .get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// Wait until the turn has settled: no session of its tree is running, the
/// parent has nothing waiting in its inbox, every background subagent of the
/// turn has reported, and the history closes the turn with an `idle` marker
/// — confirmed on consecutive polls. Returns the turn's messages.
///
/// Each poll reads the active sessions, then the parent's inbox, then its
/// history. Work moves forward through those (a stopped subagent's report is
/// queued in the inbox, then delivered into the history while the parent
/// runs), so reading them in that order never misses work in transit.
async fn poll_until_settled(
    api: &Api,
    server: &mut Server,
    parent_sid: &str,
    prompt_id: &str,
    tree: &SharedTree,
    working_dir: &str,
    cancel_token: Option<&Arc<CancelToken>>,
) -> Result<Vec<Value>, PollError> {
    let start = Instant::now();
    let mut consecutive = 0u32;
    let mut consecutive_errors = 0u32;
    // Consecutive polls on which the prompt was neither queued nor recorded.
    let mut prompt_missing = 0u32;
    // Consecutive polls on which a stopped background subagent's report was
    // neither queued nor recorded.
    let mut reports_missing = 0u32;
    let mut iter = 0u32;
    loop {
        iter += 1;
        if serve_cancel_hit(cancel_token) {
            return Err(PollError::Cancelled);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        if serve_cancel_hit(cancel_token) {
            return Err(PollError::Cancelled);
        }
        if let Some(status) = server.exit_status() {
            return Err(PollError::Fatal(format!(
                "opencode server exited during the turn ({})",
                status
            )));
        }

        let active = match active_sessions(api).await {
            Ok(active) => active,
            Err(e) => {
                consecutive = 0;
                let fatal =
                    note_poll_error(&mut consecutive_errors, "/api/session/active", e, start);
                if let Some(fatal) = fatal {
                    return Err(fatal);
                }
                continue;
            }
        };
        let mut running = Vec::new();
        for id in &active {
            if is_owned(api, tree, id).await {
                running.push(id.clone());
            }
        }
        if !running.is_empty() {
            consecutive = 0;
            consecutive_errors = 0;
            prompt_missing = 0;
            reports_missing = 0;
            if iter % 10 == 0 {
                opencode_debug(&format!(
                    "[v2.poll iter={}] running={:?} elapsed={:.1}s",
                    iter,
                    running,
                    start.elapsed().as_secs_f64()
                ));
            }
            settle_blockers(api, &running, working_dir).await;
            continue;
        }

        let inbox = match inbox_item_ids(api, parent_sid).await {
            Ok(inbox) => inbox,
            Err(e) => {
                consecutive = 0;
                let fatal = note_poll_error(&mut consecutive_errors, "session inbox", e, start);
                if let Some(fatal) = fatal {
                    return Err(fatal);
                }
                continue;
            }
        };
        let turn = match turn_messages(api, parent_sid, prompt_id).await {
            Ok(turn) => {
                consecutive_errors = 0;
                turn
            }
            Err(e) => {
                consecutive = 0;
                let fatal = note_poll_error(&mut consecutive_errors, "session messages", e, start);
                if let Some(fatal) = fatal {
                    return Err(fatal);
                }
                continue;
            }
        };
        let Some(turn) = turn else {
            consecutive = 0;
            if inbox.iter().any(|id| id == prompt_id) {
                // Admitted and queued; it runs once the session takes it.
                prompt_missing = 0;
                continue;
            }
            // Read after the inbox, the history would hold a prompt that had
            // left the inbox in between: the prompt was dropped.
            prompt_missing = prompt_missing.saturating_add(1);
            if prompt_missing >= POLL_REQUIRED_CONSECUTIVE {
                return Err(PollError::Fatal(
                    "OpenCode dropped this turn: the prompt is neither queued nor in the session history"
                        .to_string(),
                ));
            }
            continue;
        };
        prompt_missing = 0;
        if !inbox.is_empty() {
            // Queued work (a subagent report, a compaction, a prompt) will run.
            consecutive = 0;
            reports_missing = 0;
            continue;
        }
        let pending = pending_background_subagents(&turn);
        if pending.is_empty() {
            reports_missing = 0;
        } else {
            // Nothing of the turn runs and nothing is queued, yet these
            // subagents have not reported. OpenCode queues a report the moment
            // its subagent stops (milliseconds, measured), so a report missing
            // on consecutive polls is never coming.
            reports_missing = reports_missing.saturating_add(1);
            if reports_missing < POLL_REQUIRED_CONSECUTIVE {
                consecutive = 0;
                opencode_debug(&format!(
                    "[v2.poll iter={}] waiting for background subagent reports: {:?}",
                    iter, pending
                ));
                continue;
            }
            opencode_debug(&format!(
                "[v2.poll] stopped background subagents never reported: {:?}",
                pending
            ));
        }
        if !turn_is_closed(&turn) {
            consecutive = 0;
            continue;
        }
        consecutive = consecutive.saturating_add(1);
        opencode_debug(&format!(
            "[v2.poll] turn settled (consecutive={}/{})",
            consecutive, POLL_REQUIRED_CONSECUTIVE
        ));
        if consecutive >= POLL_REQUIRED_CONSECUTIVE {
            return Ok(turn);
        }
    }
}

// ============================================================
// Event stream (presentation only)
// ============================================================

/// Presentation state observed by the event consumer. It never decides the
/// terminal result; it only lets the final answer repair a text delta that
/// was still in flight when the stream was closed.
#[derive(Default)]
struct SseState {
    /// Assistant text already shown, per message and text ordinal.
    delivered: HashMap<String, BTreeMap<u64, String>>,
    protocol_warning: Option<String>,
    /// Cause carried by the parent's latest `session.execution.failed`. An
    /// execution that fails before any model step (unknown model, provider
    /// routing) leaves only an `idle` marker with `outcome: "failed"` in the
    /// history, so the event is the only record of why.
    execution_failure: Option<String>,
    /// The parent's terminal execution events delivered so far; each comes
    /// after the text of its execution.
    parent_executions_ended: usize,
}

impl SseState {
    /// Text shown for one Assistant message, joined the way OpenCode joins
    /// a message's text content.
    fn delivered_text(&self, message_id: &str) -> Option<String> {
        let parts = self.delivered.get(message_id)?;
        Some(
            parts
                .values()
                .filter(|text| !text.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }
}

struct ToolCall {
    name: String,
    input: Value,
}

/// The JSON payload of one SSE frame; `Ok(None)` for comments (heartbeats).
fn parse_sse_frame(raw: &[u8]) -> Result<Option<Value>, String> {
    let text = String::from_utf8_lossy(raw);
    let mut payload = String::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            if !payload.is_empty() {
                payload.push('\n');
            }
            payload.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if payload.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&payload)
        .map(Some)
        .map_err(|e| format!("SSE JSON frame could not be parsed: {e}"))
}

/// Human-facing text of a tool result's content, as OpenCode's own CLI shows it.
fn tool_output_text(name: &str, content: Option<&Value>) -> String {
    let texts: Vec<&str> = content
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    // Shell content appends model-only status after the command output.
    if name == "shell" {
        return texts
            .first()
            .map(|text| text.to_string())
            .unwrap_or_default();
    }
    let joined = texts
        .iter()
        .filter(|text| !text.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if name == "read" {
        if let Some(text) = read_display_text(&joined) {
            return text;
        }
    }
    joined
}

/// Read results may be a JSON page envelope; unwrap its human-facing text.
fn read_display_text(text: &str) -> Option<String> {
    if !text.starts_with('{') {
        return None;
    }
    let envelope: Value = serde_json::from_str(text).ok()?;
    if let Some(content) = envelope.get("content").and_then(Value::as_str) {
        let is_text = envelope.get("type").and_then(Value::as_str) == Some("text-page")
            || envelope.get("encoding").and_then(Value::as_str) == Some("utf8");
        if is_text {
            return Some(content.to_string());
        }
    }
    let entries = envelope.get("entries")?.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|entry| {
                entry
                    .as_str()
                    .or_else(|| entry.get("path").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn str_field<'a>(data: &'a Value, key: &str) -> &'a str {
    data.get(key).and_then(Value::as_str).unwrap_or("")
}

async fn handle_event(
    event: &Value,
    parent_sid: &str,
    sender: &Sender<StreamMessage>,
    state: &Arc<tokio::sync::Mutex<SseState>>,
    tree: &SharedTree,
    tools: &mut HashMap<(String, String), ToolCall>,
    receiver_drop: &ReceiverDropSignal,
) {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    let data = event.get("data").unwrap_or(&Value::Null);
    let session_id = str_field(data, "sessionID");

    if event_type == "session.created" {
        if let Some(parent) = data.get("parentID").and_then(Value::as_str) {
            let mut tree = lock_tree(tree);
            if tree.contains(parent) {
                tree.add(session_id);
            }
        }
        return;
    }
    // Child sessions (subagents) and other sessions never reach the UI.
    if session_id != parent_sid {
        return;
    }
    let message_id = str_field(data, "assistantMessageID").to_string();
    match event_type {
        "session.text.delta" => {
            let delta = str_field(data, "delta");
            if delta.is_empty() {
                return;
            }
            let ordinal = data.get("ordinal").and_then(Value::as_u64).unwrap_or(0);
            if !send_serve_stream_message(
                sender,
                StreamMessage::Text {
                    content: delta.to_string(),
                },
                receiver_drop,
            ) {
                return;
            }
            state
                .lock()
                .await
                .delivered
                .entry(message_id)
                .or_default()
                .entry(ordinal)
                .or_default()
                .push_str(delta);
        }
        "session.text.ended" => {
            // The full text of the block: show whatever its deltas missed.
            let text = str_field(data, "text");
            let ordinal = data.get("ordinal").and_then(Value::as_u64).unwrap_or(0);
            let shown = state
                .lock()
                .await
                .delivered
                .get(&message_id)
                .and_then(|parts| parts.get(&ordinal))
                .cloned()
                .unwrap_or_default();
            if text == shown {
                return;
            }
            let missing = if let Some(rest) = text.strip_prefix(shown.as_str()) {
                rest
            } else if shown.is_empty() {
                text
            } else {
                // Cannot retract shown text; the final repair compares the
                // authoritative answer.
                opencode_debug(&format!(
                    "[v2.sse] text block {}#{} diverged from its deltas",
                    message_id, ordinal
                ));
                return;
            };
            if !missing.is_empty()
                && !send_serve_stream_message(
                    sender,
                    StreamMessage::Text {
                        content: missing.to_string(),
                    },
                    receiver_drop,
                )
            {
                return;
            }
            state
                .lock()
                .await
                .delivered
                .entry(message_id)
                .or_default()
                .insert(ordinal, text.to_string());
        }
        "session.tool.input.started" => {
            tools.insert(
                (message_id, str_field(data, "id").to_string()),
                ToolCall {
                    name: str_field(data, "name").to_string(),
                    input: json!({}),
                },
            );
        }
        "session.tool.called" => {
            let call = tools
                .entry((message_id, str_field(data, "id").to_string()))
                .or_insert_with(|| ToolCall {
                    name: "tool".to_string(),
                    input: json!({}),
                });
            if let Some(input) = data.get("input") {
                call.input = input.clone();
            }
        }
        "session.tool.success" | "session.tool.failed" => {
            let call = tools
                .remove(&(message_id, str_field(data, "id").to_string()))
                .unwrap_or_else(|| ToolCall {
                    name: "tool".to_string(),
                    input: json!({}),
                });
            let is_error = event_type == "session.tool.failed";
            let output = if is_error {
                data.get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Tool error")
                    .to_string()
            } else {
                tool_output_text(&call.name, data.get("content"))
            };
            let input = normalize_opencode_params(&call.name, &call.input);
            if !send_serve_stream_message(
                sender,
                StreamMessage::ToolUse {
                    name: normalize_tool_name(&call.name),
                    input: serde_json::to_string(&input).unwrap_or_default(),
                },
                receiver_drop,
            ) {
                return;
            }
            send_serve_stream_message(
                sender,
                StreamMessage::ToolResult {
                    content: output,
                    is_error,
                },
                receiver_drop,
            );
        }
        "session.execution.started" => {
            // A resumed execution (e.g. on a background report) starts over.
            state.lock().await.execution_failure = None;
        }
        "session.execution.succeeded" | "session.execution.interrupted" => {
            state.lock().await.parent_executions_ended += 1;
        }
        "session.step.failed" | "session.execution.failed" => {
            // Retries and compaction can still recover; the history decides.
            opencode_debug(&format!(
                "[v2.sse] {} (tentative): {:?}",
                event_type,
                data.get("error")
            ));
            if event_type == "session.execution.failed" {
                let mut state = state.lock().await;
                state.execution_failure = data
                    .get("error")
                    .filter(|error| !error.is_null())
                    .map(opencode_message_error_text);
                state.parent_executions_ended += 1;
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn consume_events(
    mut response: reqwest::Response,
    parent_sid: String,
    sender: Sender<StreamMessage>,
    state: Arc<tokio::sync::Mutex<SseState>>,
    tree: SharedTree,
    stop: Arc<AtomicBool>,
    receiver_drop: Arc<ReceiverDropSignal>,
    ready: tokio::sync::oneshot::Sender<()>,
) {
    opencode_debug("[v2.sse] consumer started");
    let mut ready = Some(ready);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tools: HashMap<(String, String), ToolCall> = HashMap::new();
    loop {
        if stop.load(Ordering::Relaxed) || receiver_drop.is_dropped() {
            break;
        }
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                if !stop.load(Ordering::Relaxed) {
                    state.lock().await.protocol_warning =
                        Some("event stream ended before controlled shutdown".to_string());
                }
                break;
            }
            Err(e) => {
                let warning = format!("event stream read failed: {e}");
                state.lock().await.protocol_warning = Some(warning);
                break;
            }
        };
        buf.extend_from_slice(&chunk);
        while let Some(pos) = find_double_newline(&buf) {
            let raw: Vec<u8> = buf.drain(..pos + 2).collect();
            let event = match parse_sse_frame(&raw) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(e) => {
                    opencode_debug(&format!("[v2.sse] {}", e));
                    state.lock().await.protocol_warning = Some(e);
                    continue;
                }
            };
            if let Some(ready) = ready.take() {
                let _ = ready.send(());
            }
            handle_event(
                &event,
                &parent_sid,
                &sender,
                &state,
                &tree,
                &mut tools,
                &receiver_drop,
            )
            .await;
            if receiver_drop.is_dropped() {
                break;
            }
        }
    }
    opencode_debug("[v2.sse] consumer exit");
}

// ============================================================
// Turn orchestration
// ============================================================

fn send_error(sender: &Sender<StreamMessage>, message: String) {
    let _ = sender.send(StreamMessage::Error {
        message,
        stdout: String::new(),
        stderr: String::new(),
        exit_code: None,
    });
}

/// Run one turn on a private opencode 2.x server. Same contract as the 1.x
/// `execute_command_streaming_serve`: Init, presentation Text/ToolUse/
/// ToolResult, then exactly one terminal (AssistantFinal+Done, or Error), or
/// nothing at all when the turn was cancelled or its receiver dropped.
pub(super) async fn execute_command_streaming_serve(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    cancel_token: Option<Arc<CancelToken>>,
    model: Option<&str>,
) -> Result<(), String> {
    opencode_debug("=== opencode v2 execute_command_streaming_serve START ===");
    opencode_debug(&format!(
        "[v2] prompt_len={} session_id={:?} working_dir={} model={:?} cancel_token={}",
        prompt.len(),
        session_id,
        working_dir,
        model,
        cancel_token.is_some()
    ));

    // OpenCode 2.x still reads the project's AGENTS.md as instructions.
    let _agents_md_guard = prepare_requested_system_prompt(working_dir, system_prompt)?;
    let model_ref = match model.map(parse_model_ref).transpose() {
        Ok(model_ref) => model_ref,
        Err(message) => {
            send_error(&sender, message);
            return Ok(());
        }
    };
    if serve_cancel_hit(cancel_token.as_ref()) {
        return Ok(());
    }

    let mut server = match spawn_server(working_dir, cancel_token.as_ref()).await {
        Ok(server) => server,
        Err(e) => {
            if serve_cancel_hit(cancel_token.as_ref()) {
                opencode_debug(&format!("[v2] spawn aborted after cancel: {}", e));
                return Ok(());
            }
            send_error(&sender, format!("Failed to start opencode serve: {}", e));
            return Ok(());
        }
    };
    opencode_debug(&format!("[v2] server ready at {}", server.base_url));
    let api = match Api::new(&server) {
        Ok(api) => api,
        Err(e) => {
            server.shutdown().await;
            send_error(&sender, e);
            return Ok(());
        }
    };
    // The event stream is held open for the whole turn: no request timeout,
    // only a bounded connect.
    let sse_client = match reqwest::Client::builder()
        .connect_timeout(POLL_REQUEST_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            server.shutdown().await;
            send_error(&sender, format!("HTTP client init failed: {}", e));
            return Ok(());
        }
    };

    // ---- Session ----
    let parent_sid = match session_id {
        Some(sid) if !sid.is_empty() => sid.to_string(),
        _ => match create_session(&api, working_dir).await {
            Ok(sid) => sid,
            Err(e) => {
                server.shutdown().await;
                send_error(&sender, format!("Failed to create session: {}", e));
                return Ok(());
            }
        },
    };
    opencode_debug(&format!("[v2] session_id={}", parent_sid));
    let session_missing = |e: &ApiError| {
        if e.not_found() {
            format!("OpenCode session not found: {}", parent_sid)
        } else {
            e.to_string()
        }
    };
    // Test-only agent override for `--test-opencode-sse --agent`.
    if let Ok(agent) = std::env::var("COKACDIR_OPENCODE_TEST_AGENT") {
        if !agent.is_empty() {
            let body = json!({ "agent": agent });
            if let Err(e) = api
                .call_setup(
                    reqwest::Method::POST,
                    &session_path(&parent_sid, "/agent"),
                    Some(&body),
                )
                .await
            {
                server.shutdown().await;
                send_error(
                    &sender,
                    format!("Failed to select agent: {}", session_missing(&e)),
                );
                return Ok(());
            }
        }
    }
    // The model is a session setting in 2.x, like `run --model`.
    if let Some(model_ref) = model_ref {
        let body = json!({ "model": model_ref });
        if let Err(e) = api
            .call_setup(
                reqwest::Method::POST,
                &session_path(&parent_sid, "/model"),
                Some(&body),
            )
            .await
        {
            server.shutdown().await;
            send_error(
                &sender,
                format!("Failed to select model: {}", session_missing(&e)),
            );
            return Ok(());
        }
    }

    if sender
        .send(StreamMessage::Init {
            session_id: parent_sid.clone(),
        })
        .is_err()
    {
        opencode_debug("[v2] Init send failed (receiver dropped), tearing down");
        server.shutdown().await;
        return Ok(());
    }

    // From here on the session may run. /stop must interrupt it through the
    // API before the server is killed, or the next OpenCode server resumes it.
    let tree: SharedTree = Arc::new(Mutex::new(SessionTree::new(&parent_sid)));
    let _pre_kill_hook_guard = PreKillHookGuard::new(cancel_token.clone());
    if let Some(token) = cancel_token.as_ref() {
        let base_url = server.base_url.clone();
        let auth = server.auth.clone();
        let tree = tree.clone();
        token.set_pre_kill_hook(Box::new(move || {
            interrupt_session_tree_blocking(base_url, auth, tree)
        }));
    }

    // ---- Event stream, subscribed before the prompt so nothing is missed ----
    let sse_response = match sse_client
        .get(format!("{}/api/event", server.base_url))
        .header(reqwest::header::AUTHORIZATION, server.auth.as_str())
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            server.shutdown().await;
            send_error(
                &sender,
                format!(
                    "Event subscribe failed ({}): {}",
                    status,
                    log_preview(&body, 200)
                ),
            );
            return Ok(());
        }
        Err(e) => {
            server.shutdown().await;
            send_error(&sender, format!("Event stream connect failed: {}", e));
            return Ok(());
        }
    };
    let sse_state = Arc::new(tokio::sync::Mutex::new(SseState::default()));
    let sse_stop = Arc::new(AtomicBool::new(false));
    let receiver_drop = Arc::new(ReceiverDropSignal::default());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let sse_handle = tokio::task::spawn(consume_events(
        sse_response,
        parent_sid.clone(),
        sender.clone(),
        sse_state.clone(),
        tree.clone(),
        sse_stop.clone(),
        receiver_drop.clone(),
        ready_tx,
    ));
    // The first event (`server.connected`) proves the subscription is live.
    if tokio::time::timeout(EVENT_STREAM_READY_TIMEOUT, ready_rx)
        .await
        .is_err()
    {
        opencode_debug("[v2] no event within the ready timeout; prompting anyway");
    }

    // ---- Prompt ----
    let prompt_body = json!({ "text": prompt });
    let prompt_id = match api
        .call_setup(
            reqwest::Method::POST,
            &session_path(&parent_sid, "/prompt"),
            Some(&prompt_body),
        )
        .await
    {
        Ok(admitted) => admitted
            .get("data")
            .and_then(|data| data.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        Err(e) => {
            opencode_debug(&format!("[v2] prompt failed: {}", e));
            // A request cut off by its timeout may still have been admitted.
            if !serve_cancel_hit(cancel_token.as_ref()) {
                interrupt_session_tree(&api, &tree).await;
            }
            sse_stop.store(true, Ordering::Relaxed);
            sse_handle.abort();
            let _ = sse_handle.await;
            server.shutdown().await;
            // /stop while the prompt was being submitted (the server is
            // killed under it) is a cancellation, not a failure.
            if serve_cancel_hit(cancel_token.as_ref()) {
                opencode_debug("[v2] prompt submission aborted by /stop");
                return Ok(());
            }
            send_error(
                &sender,
                format!("Prompt submission failed: {}", session_missing(&e)),
            );
            return Ok(());
        }
    };
    let Some(prompt_id) = prompt_id else {
        interrupt_session_tree(&api, &tree).await;
        sse_stop.store(true, Ordering::Relaxed);
        sse_handle.abort();
        let _ = sse_handle.await;
        server.shutdown().await;
        send_error(
            &sender,
            "Prompt submission failed: OpenCode returned no message id".to_string(),
        );
        return Ok(());
    };
    opencode_debug(&format!("[v2] prompt admitted as {}", prompt_id));

    // ---- Wait for the whole session tree to settle ----
    let mut poll_result = tokio::select! {
        result = poll_until_settled(
            &api,
            &mut server,
            &parent_sid,
            &prompt_id,
            &tree,
            working_dir,
            cancel_token.as_ref(),
        ) => result,
        _ = receiver_drop.wait() => {
            opencode_debug("[v2] stream receiver dropped; aborting");
            Err(PollError::ReceiverDropped)
        }
    };
    if receiver_drop.is_dropped() {
        poll_result = Err(PollError::ReceiverDropped);
    }
    if poll_result.is_ok() && serve_cancel_hit(cancel_token.as_ref()) {
        poll_result = Err(PollError::Cancelled);
    }

    // ---- Shut down ----
    if let Ok(turn) = &poll_result {
        // Let the presentation stream deliver the turn's final text. Every
        // execution of the parent ends with an `idle` marker in the history
        // and a terminal execution event after its text on the stream, so
        // once the stream has delivered as many terminal events as the turn
        // has markers, it has delivered all of the turn's text.
        let executions = turn
            .iter()
            .filter(|message| message_type(message) == "idle")
            .count();
        let deadline = Instant::now() + STREAM_CATCH_UP_TIMEOUT;
        loop {
            if sse_state.lock().await.parent_executions_ended >= executions {
                break;
            }
            if Instant::now() >= deadline {
                opencode_debug("[v2] event stream did not catch up; repairing from history");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    } else {
        // Stop the unfinished turn through the API so no OpenCode server
        // resumes it later. Skipped when /stop already did so via the hook.
        let hook_pending = match cancel_token.as_ref() {
            Some(token) => token.take_pre_kill_hook().is_some(),
            None => true,
        };
        if hook_pending {
            interrupt_session_tree(&api, &tree).await;
        }
    }
    if let Some(token) = cancel_token.as_ref() {
        token.clear_pre_kill_hook();
    }
    sse_stop.store(true, Ordering::Relaxed);
    sse_handle.abort();
    let _ = sse_handle.await;
    server.shutdown().await;

    // `/stop` can arrive during the drain or shutdown; it still wins.
    if poll_result.is_ok() && serve_cancel_hit(cancel_token.as_ref()) {
        opencode_debug("[v2] cancellation won during terminal verification/shutdown");
        poll_result = Err(PollError::Cancelled);
    }

    // ---- Report ----
    let sse_state = sse_state.lock().await;
    if let Some(warning) = sse_state.protocol_warning.as_deref() {
        opencode_debug(&format!(
            "[v2] presentation stream was incomplete; terminal repair active: {warning}"
        ));
    }
    // Parsed after the drain, so the failure event of a failed turn has been
    // consumed.
    match poll_result {
        Ok(turn) => match parse_turn_terminal(&turn, sse_state.execution_failure.as_deref()) {
            Ok(terminal) => {
                let shown = sse_state.delivered_text(&terminal.message_id);
                let missing = missing_sse_terminal_delta(shown.as_deref(), &terminal.result);
                if let Some(delta) = missing {
                    opencode_debug(&format!(
                        "[v2] repairing {} missing/divergent terminal bytes",
                        delta.len()
                    ));
                    if sender.send(StreamMessage::Text { content: delta }).is_err() {
                        return Ok(());
                    }
                }
                opencode_debug(&format!(
                    "[v2] terminal message={} result_len={} canonical={}",
                    terminal.message_id,
                    terminal.result.len(),
                    terminal.assistant_final.is_some()
                ));
                let _ = send_success_terminal(
                    &sender,
                    terminal.assistant_final,
                    terminal.result,
                    Some(parent_sid),
                );
            }
            Err(message) => {
                opencode_debug(&format!("[v2] turn failed: {}", message));
                send_error(&sender, message);
            }
        },
        Err(PollError::Cancelled) => {
            // No terminal: the UI reports the cancellation itself.
            opencode_debug("[v2] cancelled by user");
        }
        Err(PollError::ReceiverDropped) => {
            opencode_debug("[v2] receiver dropped; no terminal sent");
        }
        Err(PollError::Fatal(message)) => {
            opencode_debug(&format!("[v2] poll fatal: {}", message));
            send_error(&sender, message);
        }
    }

    opencode_debug("=== opencode v2 execute_command_streaming_serve END ===");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(id: &str, finish: &str, content: Value) -> Value {
        json!({
            "id": id,
            "type": "assistant",
            "finish": finish,
            "content": content,
            "time": { "created": 1, "completed": 2 },
        })
    }

    fn idle(outcome: &str) -> Value {
        json!({ "id": "msg_idle", "type": "idle", "outcome": outcome })
    }

    #[test]
    fn ready_line_is_the_stdio_json_url() {
        assert_eq!(
            parse_ready_line("{\"url\":\"http://127.0.0.1:32997\"}\n").as_deref(),
            Some("http://127.0.0.1:32997")
        );
        assert_eq!(
            parse_ready_line("server listening on http://127.0.0.1:1"),
            None
        );
        assert_eq!(parse_ready_line("{\"url\":\"ftp://x\"}"), None);
    }

    #[test]
    fn model_reference_parses_provider_model_and_variant() {
        assert_eq!(
            parse_model_ref("openai/gpt-6-luna").unwrap(),
            json!({ "providerID": "openai", "id": "gpt-6-luna" })
        );
        assert_eq!(
            parse_model_ref("openai/gpt-6-luna#high").unwrap(),
            json!({ "providerID": "openai", "id": "gpt-6-luna", "variant": "high" })
        );
        assert_eq!(
            parse_model_ref("opencode/models/nested").unwrap(),
            json!({ "providerID": "opencode", "id": "models/nested" })
        );
        assert!(parse_model_ref("big-pickle").is_err());
        assert!(parse_model_ref("/x").is_err());
    }

    #[test]
    fn terminal_is_the_last_assistant_of_a_succeeded_turn() {
        let turn = vec![
            assistant(
                "msg_a",
                "tool-calls",
                json!([{
                    "type": "tool",
                    "id": "c1",
                    "name": "shell",
                    "state": { "status": "completed" }
                }]),
            ),
            assistant(
                "msg_b",
                "stop",
                json!([
                    { "type": "reasoning", "text": "thinking" },
                    { "type": "text", "text": "first" },
                    { "type": "text", "text": "second" }
                ]),
            ),
            idle("succeeded"),
        ];
        let terminal = parse_turn_terminal(&turn, None).unwrap();
        assert_eq!(terminal.message_id, "msg_b");
        assert_eq!(terminal.result, "first\n\nsecond");
        assert_eq!(terminal.assistant_final.as_deref(), Some("first\n\nsecond"));
    }

    #[test]
    fn truncated_answer_is_shown_but_not_canonical() {
        let turn = vec![
            assistant(
                "msg_a",
                "length",
                json!([{ "type": "text", "text": "partial" }]),
            ),
            idle("succeeded"),
        ];
        let terminal = parse_turn_terminal(&turn, None).unwrap();
        assert_eq!(terminal.result, "partial");
        assert_eq!(terminal.assistant_final, None);
    }

    #[test]
    fn failed_or_interrupted_turns_are_errors() {
        let mut errored = assistant("msg_a", "error", json!([]));
        errored["error"] = json!({ "type": "provider.auth", "message": "usage limit reached" });
        // The Assistant's own error is the more specific cause.
        assert_eq!(
            parse_turn_terminal(&[errored, idle("failed")], Some("execution failed")).unwrap_err(),
            "usage limit reached"
        );
        let turn = vec![
            assistant("msg_a", "tool-calls", json!([])),
            idle("interrupted"),
        ];
        assert!(parse_turn_terminal(&turn, None).is_err());
        assert!(parse_turn_terminal(&[idle("succeeded")], None).is_err());
    }

    #[test]
    fn failure_before_any_model_step_reports_the_event_cause() {
        // An unknown model fails the execution before any Assistant message:
        // the history holds only `idle` with `outcome: "failed"`.
        let turn = [idle("failed")];
        assert_eq!(
            parse_turn_terminal(&turn, Some("Model unavailable: openai/nope")).unwrap_err(),
            "Model unavailable: openai/nope"
        );
        assert_eq!(
            parse_turn_terminal(&turn, None).unwrap_err(),
            "OpenCode turn failed"
        );
        // A recorded cause never turns a succeeded turn into a failure.
        let turn = vec![
            assistant("msg_a", "stop", json!([{ "type": "text", "text": "ok" }])),
            idle("succeeded"),
        ];
        assert!(parse_turn_terminal(&turn, Some("stale cause")).is_ok());
    }

    #[test]
    fn background_subagent_is_pending_until_its_report_arrives() {
        let launch = assistant(
            "msg_a",
            "stop",
            json!([{
                "type": "tool",
                "id": "c1",
                "name": "subagent",
                "state": {
                    "status": "completed",
                    "metadata": { "sessionID": "ses_child", "status": "running" }
                }
            }]),
        );
        let foreground = assistant(
            "msg_f",
            "stop",
            json!([{
                "type": "tool",
                "id": "c2",
                "name": "subagent",
                "state": {
                    "status": "completed",
                    "metadata": { "sessionID": "ses_done", "status": "completed" }
                }
            }]),
        );
        let turn = vec![launch.clone(), foreground, idle("succeeded")];
        assert_eq!(
            pending_background_subagents(&turn),
            vec!["ses_child".to_string()]
        );
        assert!(turn_is_closed(&turn));

        let report = json!({
            "id": "msg_s",
            "type": "synthetic",
            "metadata": { "source": "subagent", "childID": "ses_child", "state": "completed" },
            "text": "<subagent ...>"
        });
        let resumed = vec![launch, idle("succeeded"), report];
        assert!(pending_background_subagents(&resumed).is_empty());
        assert!(!turn_is_closed(&resumed));
    }

    #[test]
    fn sse_frames_yield_json_and_skip_heartbeats() {
        assert_eq!(parse_sse_frame(b": heartbeat\n\n").unwrap(), None);
        let frame = b"data: {\"type\":\"server.connected\",\"data\":{}}\n\n";
        assert_eq!(
            parse_sse_frame(frame).unwrap().unwrap()["type"],
            json!("server.connected")
        );
        assert!(parse_sse_frame(b"data: {broken\n\n").is_err());
    }

    #[test]
    fn tool_output_text_follows_opencode_display_rules() {
        let shell = json!([
            { "type": "text", "text": "3c56ad06a281\n" },
            { "type": "text", "text": "exit status for the model" }
        ]);
        assert_eq!(tool_output_text("shell", Some(&shell)), "3c56ad06a281\n");
        let page_text = json!({ "type": "text-page", "content": "hello" }).to_string();
        let page = json!([{ "type": "text", "text": page_text }]);
        assert_eq!(tool_output_text("read", Some(&page)), "hello");
        let listing_text = json!({ "entries": ["a.rs", { "path": "b" }] }).to_string();
        let listing = json!([{ "type": "text", "text": listing_text }]);
        assert_eq!(tool_output_text("read", Some(&listing)), "a.rs\nb");
        let plain = json!([{ "type": "text", "text": "one" }, { "type": "text", "text": "two" }]);
        assert_eq!(tool_output_text("grep", Some(&plain)), "one\ntwo");
        assert_eq!(tool_output_text("grep", None), "");
    }

    #[test]
    fn blocking_active_sessions_reads_the_loopback_response() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let read = stream.read(&mut request).unwrap();
            let body = r#"{"data":{"ses_a":{"type":"running"}}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            String::from_utf8_lossy(&request[..read]).to_string()
        });
        let active = blocking_active_sessions(&base_url, "Basic abc").unwrap();
        assert_eq!(active, HashSet::from(["ses_a".to_string()]));
        let request = server.join().unwrap();
        assert!(request.starts_with("GET /api/session/active HTTP/1.1\r\n"));
        assert!(request.contains("\r\nAuthorization: Basic abc\r\n"));
    }

    #[test]
    fn session_tree_interrupts_children_before_the_parent() {
        let mut tree = SessionTree::new("ses_parent");
        tree.add("ses_child");
        tree.add("ses_grandchild");
        tree.add("ses_child");
        assert_eq!(
            tree.children_first(),
            vec!["ses_grandchild", "ses_child", "ses_parent"]
        );
    }

    #[test]
    fn delivered_text_joins_text_blocks_like_the_history() {
        let mut state = SseState::default();
        let parts = state.delivered.entry("msg_b".to_string()).or_default();
        parts.insert(1, "second".to_string());
        parts.insert(0, "first".to_string());
        assert_eq!(
            state.delivered_text("msg_b").as_deref(),
            Some("first\n\nsecond")
        );
        assert_eq!(state.delivered_text("msg_other"), None);
    }
}
