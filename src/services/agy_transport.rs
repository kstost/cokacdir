//! Bounded AGY process transport and request-local stream-json validation.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{atomic::Ordering, mpsc::Sender, Arc};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{AgyHookPrompt, AgyHookState, ReapingAgyChild};
use crate::services::claude::{
    create_private_temp_file, send_success_terminal, CancelToken, PrivateTempFile, StreamMessage,
};
use crate::services::file_ops::{
    open_directory_for_read, stable_file_identity, DirectoryFileOptions,
};

const OUTPUT_LIMIT: u64 = 64 * 1024 * 1024;
const STDERR_LIMIT: u64 = 4 * 1024 * 1024;
const EVENT_LIMIT: usize = 16 * 1024 * 1024;
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Positive Go-style durations accepted by --print-timeout, including 1m30s.
pub(super) fn parse_timeout(value: &str) -> Result<Duration, String> {
    let invalid = || {
        format!("Invalid COKAC_AGY_PRINT_TIMEOUT: {value:?}. Use a positive duration such as 30s, 1m30s, or 1h.")
    };
    let mut remaining = value.strip_prefix('+').unwrap_or(value);
    let mut seconds = 0.0;
    while !remaining.is_empty() {
        let number_len = remaining
            .bytes()
            .take_while(|b| b.is_ascii_digit() || *b == b'.')
            .count();
        let number: f64 = remaining[..number_len].parse().map_err(|_| invalid())?;
        remaining = &remaining[number_len..];
        let (unit, scale) = [
            ("ns", 1e-9),
            ("us", 1e-6),
            ("µs", 1e-6),
            ("μs", 1e-6),
            ("ms", 1e-3),
            ("s", 1.0),
            ("m", 60.0),
            ("h", 3600.0),
        ]
        .into_iter()
        .find(|(unit, _)| remaining.starts_with(unit))
        .ok_or_else(invalid)?;
        remaining = &remaining[unit.len()..];
        seconds += number * scale;
    }
    let duration = Duration::try_from_secs_f64(seconds).map_err(|_| invalid())?;
    if duration.is_zero() || duration > Duration::from_nanos(i64::MAX as u64) {
        return Err(invalid());
    }
    Ok(duration)
}

#[derive(Debug, Deserialize)]
struct AgyResult {
    status: String,
    #[serde(default)]
    conversation_id: String,
    #[serde(default)]
    response: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct StepUpdate {
    conversation_id: Option<String>,
    #[serde(default)]
    step_type: String,
    text_delta: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "event")]
enum Event {
    #[serde(rename = "init")]
    Init { conversation_id: String },
    #[serde(rename = "step_update")]
    StepUpdate { step_update: StepUpdate },
    #[serde(rename = "result")]
    Result { result: AgyResult },
    #[serde(other)]
    Other,
}

#[derive(Default)]
struct EventStream {
    pending: Vec<u8>,
    conversation_id: Option<String>,
    initialized: bool,
    result: Option<AgyResult>,
}

impl EventStream {
    fn check_id(&mut self, id: &str, expected: Option<&str>) -> Result<(), String> {
        if !crate::services::process::is_valid_session_id(id) {
            return Err(
                "Agy returned an invalid conversation ID; the response was discarded.".into(),
            );
        }
        if expected.is_some_and(|expected| expected != id) {
            return Err(format!(
                "Agy resumed a different conversation (requested {}, received {id}); the response was discarded. Start a new session explicitly to continue.",
                expected.unwrap_or_default()
            ));
        }
        if self
            .conversation_id
            .as_deref()
            .is_some_and(|seen| seen != id)
        {
            return Err(
                "Agy changed conversation ID during the request; the response was discarded."
                    .into(),
            );
        }
        self.conversation_id = Some(id.to_owned());
        Ok(())
    }

    fn event(&mut self, bytes: &[u8], expected: Option<&str>) -> Result<Option<String>, String> {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        if self.result.is_some() {
            return Err(
                "Agy emitted data after its terminal result; the response was discarded.".into(),
            );
        }
        let event: Event = serde_json::from_slice(bytes).map_err(|_| {
            "Agy returned invalid stream-json. Update Agy to a version supporting --output-format stream-json (1.1.8 or newer).".to_string()
        })?;
        match event {
            Event::Init { conversation_id } => {
                if self.initialized {
                    return Err("Agy emitted duplicate initialization events.".into());
                }
                self.check_id(&conversation_id, expected)?;
                self.initialized = true;
            }
            Event::StepUpdate { step_update } => {
                if let Some(id) = step_update.conversation_id {
                    self.check_id(&id, expected)?;
                }
                if step_update.step_type == "agent_response" {
                    return Ok(step_update.text_delta);
                }
            }
            Event::Result { result } => {
                // Startup failures can consist of a single ERROR result,
                // with no init event and an empty conversation_id.
                if !result.conversation_id.is_empty() || result.status == "SUCCESS" {
                    self.check_id(&result.conversation_id, expected)?;
                }
                self.result = Some(result);
            }
            Event::Other => {}
        }
        Ok(None)
    }

    fn push(&mut self, bytes: &[u8], expected: Option<&str>) -> Result<Vec<String>, String> {
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;
        let mut deltas = Vec::new();
        while let Some(end) = self.pending[consumed..].iter().position(|b| *b == b'\n') {
            let end = consumed + end;
            if end - consumed > EVENT_LIMIT {
                return Err("Agy stream-json event exceeded the output limit.".into());
            }
            let line = self.pending[consumed..end].to_vec();
            if let Some(delta) = self.event(&line, expected)? {
                deltas.push(delta);
            }
            consumed = end + 1;
        }
        self.pending.drain(..consumed);
        if self.pending.len() > EVENT_LIMIT {
            return Err("Agy stream-json event exceeded the output limit.".into());
        }
        Ok(deltas)
    }

    fn finish(mut self, expected: Option<&str>) -> Result<AgyResult, String> {
        let tail = std::mem::take(&mut self.pending);
        self.event(&tail, expected)?;
        let result = self.result.ok_or_else(|| {
            "Agy exited without a terminal stream-json result; the response was discarded."
                .to_string()
        })?;
        if result.status != "SUCCESS" || result.error.as_ref().is_some_and(|e| !e.is_empty()) {
            return Err(result.error.filter(|e| !e.is_empty()).unwrap_or_else(|| {
                format!(
                    "Agy did not complete successfully (status {}).",
                    result.status
                )
            }));
        }
        if result.response.trim().is_empty() {
            return Err("Agy completed without an assistant response.".into());
        }
        Ok(result)
    }
}

/// Open independent file descriptions: try_clone would share seek offsets
/// between the child's writer and a parent that is reading while it runs.
fn open_private(guard: &PrivateTempFile, writable: bool) -> io::Result<File> {
    let path = guard.verified_path()?;
    let (_, access, _) = open_directory_for_read(
        path.parent()
            .ok_or_else(|| io::Error::other("missing temporary directory"))?,
    )?;
    let file = access.open_file(
        path.file_name()
            .ok_or_else(|| io::Error::other("missing temporary filename"))?,
        DirectoryFileOptions::new().read(true).write(writable),
    )?;
    if stable_file_identity(&file)? != guard.identity() {
        return Err(io::Error::other("Agy temporary file changed while opening"));
    }
    Ok(file)
}

fn diagnostics(file: &mut File) -> String {
    let mut bytes = Vec::new();
    let _ = file.take(16 * 1024).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(super) fn run(
    mut command: Command,
    temp_dir: &Path,
    prompt: &str,
    timeout: Duration,
    expected_session: Option<&str>,
    sender: &Sender<StreamMessage>,
    hook: Option<&AgyHookPrompt>,
    cancel: Option<&Arc<CancelToken>>,
) -> Result<(), String> {
    if cancel.is_some_and(|token| token.cancelled.load(Ordering::Relaxed)) {
        return Ok(());
    }
    // A regular stdin file provides non-TTY input and EOF without a blocking
    // write to a child that never reads its stdin. Output files also avoid
    // pipe EOF waits on descendants after the direct child has exited.
    let input_guard = create_private_temp_file(temp_dir, "agy_stdin", prompt.as_bytes())
        .map_err(|e| format!("Failed to prepare Agy stdin: {e}"))?;
    let output_guard = create_private_temp_file(temp_dir, "agy_stdout", b"")
        .map_err(|e| format!("Failed to prepare Agy stdout: {e}"))?;
    let error_guard = create_private_temp_file(temp_dir, "agy_stderr", b"")
        .map_err(|e| format!("Failed to prepare Agy stderr: {e}"))?;
    let mut stdout = open_private(&output_guard, false).map_err(|e| e.to_string())?;
    let mut stderr = open_private(&error_guard, false).map_err(|e| e.to_string())?;
    command
        .stdin(Stdio::from(
            open_private(&input_guard, false).map_err(|e| e.to_string())?,
        ))
        .stdout(Stdio::from(
            open_private(&output_guard, true).map_err(|e| e.to_string())?,
        ))
        .stderr(Stdio::from(
            open_private(&error_guard, true).map_err(|e| e.to_string())?,
        ));
    // Keep only the open handles. Unix unlinks the names now; Windows marks
    // them for deletion when the inherited/shared handles close. A crash
    // cannot leave a new plaintext stdin/stdout file for the next run.
    for guard in [&input_guard, &output_guard, &error_guard] {
        crate::services::file_ops::remove_file_by_identity(guard.path(), guard.identity())
            .map_err(|e| format!("Failed to unlink Agy temporary I/O: {e}"))?;
    }
    crate::services::claude::detach_into_own_pgroup(&mut command);
    crate::services::claude::attach_cancel_cgroup(&mut command, cancel);
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "Agy request timeout is too large.".to_string())?;
    let mut child = ReapingAgyChild::new(
        command
            .spawn()
            .map_err(|e| format!("Failed to start Agy: {e}"))?,
    );
    drop(command);
    if let Some(token) = cancel {
        *token.child_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(child.id());
    }

    let mut events = EventStream::default();
    let mut forwarded = String::new();
    let can_stream = hook.is_none() && expected_session.is_none();
    let mut hook_pending = None;
    let mut exit_status: Option<ExitStatus> = None;
    let mut read_bytes = 0u64;
    let outcome = (|| -> Result<bool, String> {
        loop {
            if cancel.is_some_and(|token| token.cancelled.load(Ordering::Relaxed)) {
                return Ok(false);
            }
            if Instant::now() >= deadline {
                return Err(format!("Agy request exceeded its overall timeout ({timeout:?}); the process was terminated and the response discarded."));
            }
            if stdout.metadata().map_err(|e| e.to_string())?.len() > OUTPUT_LIMIT
                || stderr.metadata().map_err(|e| e.to_string())?.len() > STDERR_LIMIT
            {
                return Err(
                    "Agy exceeded the captured output limit; the response was discarded.".into(),
                );
            }
            exit_status = child
                .try_wait()
                .map_err(|e| format!("Agy process error: {e}"))?;
            let mut chunk = [0u8; 64 * 1024];
            let mut drained = false;
            // Yield back to deadline/cancellation checks even with a child
            // that writes continuously. Framing retains partial UTF-8/JSON.
            for _ in 0..16 {
                let count = stdout
                    .read(&mut chunk)
                    .map_err(|e| format!("Failed to read Agy output: {e}"))?;
                if count == 0 {
                    drained = true;
                    break;
                }
                read_bytes += count as u64;
                if read_bytes > OUTPUT_LIMIT {
                    return Err("Agy exceeded the captured output limit.".into());
                }
                for delta in events.push(&chunk[..count], expected_session)? {
                    if can_stream {
                        if sender
                            .send(StreamMessage::Text {
                                content: delta.clone(),
                            })
                            .is_err()
                        {
                            return Ok(false);
                        }
                        forwarded.push_str(&delta);
                    }
                }
            }
            if let Some(hook) = hook {
                match hook.hook_state() {
                    AgyHookState::Failed => return Err("Agy failed while running cokacdir's system-prompt hook; the response was discarded.".into()),
                    AgyHookState::Complete if hook.acknowledged() => hook_pending = None,
                    _ => {
                        let since = hook_pending.get_or_insert_with(Instant::now);
                        if since.elapsed() >= HOOK_TIMEOUT {
                            return Err("Agy's system-prompt hook did not complete within 30 seconds; the response was discarded.".into());
                        }
                    }
                }
            }
            if exit_status.is_some() && drained {
                return Ok(true);
            }
            if drained {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    })();

    if !matches!(outcome, Ok(true)) {
        if let Some(token) = cancel {
            if let Some(cgroup) = token
                .cgroup
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                cgroup.kill_all();
            }
        }
        child.kill_and_reap();
    }
    if let Some(token) = cancel {
        let mut pid = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        if *pid == Some(child.id()) {
            *pid = None;
        }
    }
    let stderr = diagnostics(&mut stderr);
    if matches!(outcome, Ok(false))
        || cancel.is_some_and(|token| token.cancelled.load(Ordering::Relaxed))
    {
        return Ok(());
    }
    let completed = outcome.and_then(|_| {
        let parsed = events.finish(expected_session);
        if exit_status.is_some_and(|status| !status.success()) {
            return Err(parsed.err().unwrap_or_else(|| format!("Agy exited with code {:?}.", exit_status.and_then(|s| s.code()))));
        }
        let result = parsed?;
        if let Some(hook) = hook {
            if hook.hook_state() != AgyHookState::Complete || !hook.acknowledged() {
                return Err("Agy completed without a verified system-prompt hook; the response was discarded.".into());
            }
        }
        Ok(result)
    });
    match completed {
        Err(message) => {
            super::agy_debug(&format!(
                "[stream] failed: {}; exit={:?}; stdout_bytes={read_bytes}",
                super::log_preview(&message, 500),
                exit_status.and_then(|s| s.code())
            ));
            let _ = sender.send(StreamMessage::Error {
                message,
                stdout: String::new(),
                stderr,
                exit_code: exit_status.and_then(|s| s.code()),
            });
        }
        Ok(result) => {
            super::agy_debug(&format!(
                "[stream] complete: conversation_id={}; response_bytes={}",
                result.conversation_id,
                result.response.len()
            ));
            // The terminal envelope is authoritative. Never treat error-like
            // prose or tool/thinking events as the completion status.
            if let Some(pending) = result
                .response
                .strip_prefix(&forwarded)
                .filter(|s| !s.is_empty())
            {
                if sender
                    .send(StreamMessage::Text {
                        content: pending.to_owned(),
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            let _ = send_success_terminal(
                sender,
                Some(result.response.clone()),
                result.response,
                Some(result.conversation_id),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result(id: &str, status: &str, response: &str) -> String {
        json!({"event":"result","result":{"conversation_id":id,"status":status,"response":response}}).to_string() + "\n"
    }

    #[test]
    fn timeout_accepts_positive_go_units_and_rejects_unbounded_values() {
        for (input, expected) in [
            ("1h", Duration::from_secs(3600)),
            ("1m30s", Duration::from_secs(90)),
            ("+1.5s", Duration::from_millis(1500)),
            (".5s", Duration::from_millis(500)),
            ("100µs", Duration::from_micros(100)),
            ("100μs", Duration::from_micros(100)),
            ("1ns", Duration::from_nanos(1)),
        ] {
            assert_eq!(parse_timeout(input).unwrap(), expected);
        }
        for input in [
            "",
            "0",
            "0s",
            "-1s",
            "NaNs",
            "infs",
            "1e9s",
            "1",
            "1d",
            "1..2s",
            "999999999999999h",
        ] {
            assert!(parse_timeout(input).is_err(), "{input}");
        }
    }

    #[test]
    fn fragmented_utf8_and_terminal_error_looking_prose_are_preserved() {
        let response = "한글🙂 Error: timeout waiting for response";
        let wire = result("actual-session", "SUCCESS", response);
        let mut stream = EventStream::default();
        for byte in wire.as_bytes() {
            stream.push(&[*byte], None).unwrap();
        }
        let result = stream.finish(None).unwrap();
        assert_eq!(result.response, response);
        assert_eq!(result.conversation_id, "actual-session");
    }

    #[test]
    fn preflight_error_without_init_preserves_the_provider_error() {
        let wire = json!({"event":"result","result":{"status":"ERROR","conversation_id":"","response":"","error":"invalid model selection"}}).to_string();
        let mut stream = EventStream::default();
        stream.push(wire.as_bytes(), None).unwrap();
        assert_eq!(stream.finish(None).unwrap_err(), "invalid model selection");
    }

    #[test]
    fn rejects_non_success_statuses_and_contradictory_success() {
        for status in [
            "ERROR",
            "CANCELED",
            "INTERRUPTED",
            "INVALID",
            "WAITING",
            "RUNNING",
            "UNKNOWN",
        ] {
            let mut stream = EventStream::default();
            stream
                .push(result("session", status, "partial").as_bytes(), None)
                .unwrap();
            assert!(stream.finish(None).is_err(), "{status}");
        }
        let mut stream = EventStream::default();
        stream.push(b"{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\",\"conversation_id\":\"session\",\"response\":\"partial\",\"error\":\"backend failure\"}}\n", None).unwrap();
        assert_eq!(stream.finish(None).unwrap_err(), "backend failure");
    }

    #[test]
    fn refuses_missing_invalid_or_changed_session_ids() {
        for id in ["", "../other", "-option"] {
            let mut stream = EventStream::default();
            assert!(stream
                .push(result(id, "SUCCESS", "answer").as_bytes(), None)
                .is_err());
        }
        for event in [
            json!({"event":"init","conversation_id":"new-session"}),
            json!({"event":"step_update","step_update":{"conversation_id":"new-session","step_type":"agent_response","text_delta":"wrong"}}),
            json!({"event":"result","result":{"conversation_id":"new-session","status":"SUCCESS","response":"wrong"}}),
        ] {
            let mut stream = EventStream::default();
            assert!(stream
                .push(
                    (event.to_string() + "\n").as_bytes(),
                    Some("requested-session")
                )
                .unwrap_err()
                .contains("different conversation"));
        }
        let mut stream = EventStream::default();
        stream
            .push(b"{\"event\":\"init\",\"conversation_id\":\"one\"}\n", None)
            .unwrap();
        assert!(stream
            .push(result("two", "SUCCESS", "answer").as_bytes(), None)
            .unwrap_err()
            .contains("changed conversation"));
    }

    #[test]
    fn rejects_missing_duplicate_and_truncated_terminal_results() {
        assert!(EventStream::default().finish(None).is_err());
        let mut stream = EventStream::default();
        stream.push(b"{\"event\":\"result\"", None).unwrap();
        assert!(stream.finish(None).is_err());
        let mut stream = EventStream::default();
        let wire = result("session", "SUCCESS", "answer");
        stream.push(wire.as_bytes(), None).unwrap();
        assert!(stream.push(wire.as_bytes(), None).is_err());
        let mut stream = EventStream::default();
        stream
            .push(result("session", "SUCCESS", " \n").as_bytes(), None)
            .unwrap();
        assert!(stream.finish(None).is_err());
    }

    #[test]
    fn only_assistant_text_deltas_are_visible() {
        let mut stream = EventStream::default();
        for step_type in ["tool", "thinking", "user_input"] {
            let wire = json!({"event":"step_update","step_update":{"step_type":step_type,"text_delta":"hidden"}}).to_string()+"\n";
            assert!(stream.push(wire.as_bytes(), None).unwrap().is_empty());
        }
        let wire = json!({"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"visible"}}).to_string()+"\n";
        assert_eq!(stream.push(wire.as_bytes(), None).unwrap(), vec!["visible"]);
    }

    #[cfg(unix)]
    fn command(script: &str, arguments: &[&str], directory: &Path) -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", script, "agy-test"])
            .args(arguments)
            .current_dir(directory);
        command
    }

    #[cfg(unix)]
    fn messages(
        script: &str,
        wire: &str,
        expected: Option<&str>,
        timeout: Duration,
    ) -> Vec<StreamMessage> {
        let temp = tempfile::tempdir().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        run(
            command(script, &[wire], temp.path()),
            temp.path(),
            "request",
            timeout,
            expected,
            &sender,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_dir(temp.path()).unwrap().count(),
            0,
            "temporary input/output must be removed"
        );
        drop(sender);
        receiver.into_iter().collect()
    }

    #[cfg(unix)]
    fn assert_failed_without_response(messages: &[StreamMessage], error: &str) {
        assert!(messages
            .iter()
            .any(|m| matches!(m, StreamMessage::Error { message, .. } if message.contains(error))));
        assert!(!messages.iter().any(|m| matches!(
            m,
            StreamMessage::Text { .. }
                | StreamMessage::AssistantFinal { .. }
                | StreamMessage::Done { .. }
        )));
    }

    #[cfg(unix)]
    #[test]
    fn process_uses_result_id_and_does_not_duplicate_streamed_text() {
        let wire = "{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"agent_response\",\"text_delta\":\"answer\"}}\n".to_owned()+&result("actual-session", "SUCCESS", "answer\n");
        let messages = messages(
            "cat >/dev/null; printf '%s' \"$1\"",
            &wire,
            None,
            Duration::from_secs(3),
        );
        let text: String = messages
            .iter()
            .filter_map(|m| {
                if let StreamMessage::Text { content } = m {
                    Some(content.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "answer\n");
        assert_eq!(
            messages
                .iter()
                .filter(|m| matches!(m, StreamMessage::AssistantFinal { .. }))
                .count(),
            1
        );
        assert!(messages.iter().any(|m| matches!(m, StreamMessage::Done { result, session_id } if result=="answer\n" && session_id.as_deref()==Some("actual-session"))));
    }

    #[cfg(unix)]
    #[test]
    fn request_io_names_are_removed_before_the_child_starts() {
        let messages = messages(
            "for path in agy_stdin_* agy_stdout_* agy_stderr_*; do [ ! -e \"$path\" ] || exit 9; done; printf '%s' \"$1\"",
            &result("session", "SUCCESS", "answer"),
            None,
            Duration::from_secs(3),
        );
        assert!(messages
            .iter()
            .any(|m| matches!(m, StreamMessage::Done { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn changed_resume_id_discards_even_earlier_text() {
        let wire = "{\"event\":\"init\",\"conversation_id\":\"old\"}\n{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"agent_response\",\"text_delta\":\"unverified\"}}\n".to_owned()+&result("new", "SUCCESS", "unverified");
        let messages = messages(
            "printf '%s' \"$1\"",
            &wire,
            Some("old"),
            Duration::from_secs(3),
        );
        assert_failed_without_response(&messages, "different conversation");
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_cannot_publish_a_success_envelope() {
        let messages = messages(
            "printf '%s' \"$1\"; exit 7",
            &result("session", "SUCCESS", "unverified"),
            None,
            Duration::from_secs(3),
        );
        assert_failed_without_response(&messages, "code Some(7)");
    }

    #[cfg(unix)]
    #[test]
    fn success_exit_requires_a_complete_terminal_result() {
        for wire in ["", "not-json\n", "{\"event\":\"result\""] {
            let messages = messages("printf '%s' \"$1\"", wire, None, Duration::from_secs(3));
            assert_failed_without_response(&messages, "Agy");
        }
    }

    #[cfg(unix)]
    #[test]
    fn overall_timeout_bounds_unread_stdin_and_closed_output_and_reaps_child() {
        for script in [
            "printf '%s' \"$$\" > \"$1\"; exec sleep 60",
            "printf '%s' \"$$\" > \"$1\"; exec 1>&-; exec 2>&-; exec sleep 60",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let pid_file = temp.path().join("pid");
            let (sender, receiver) = std::sync::mpsc::channel();
            let start = Instant::now();
            run(
                command(script, &[pid_file.to_str().unwrap()], temp.path()),
                temp.path(),
                &"p".repeat(2 * 1024 * 1024),
                Duration::from_millis(150),
                None,
                &sender,
                None,
                None,
            )
            .unwrap();
            assert!(start.elapsed() < Duration::from_secs(3));
            drop(sender);
            assert_failed_without_response(
                &receiver.into_iter().collect::<Vec<_>>(),
                "overall timeout",
            );
            let pid = std::fs::read_to_string(pid_file).unwrap();
            assert!(!Command::new("kill")
                .args(["-0", &pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success());
        }
    }

    #[cfg(unix)]
    #[test]
    fn completed_child_does_not_wait_for_descendants_output_descriptors() {
        let start = Instant::now();
        let messages = messages(
            "sleep 2 & printf '%s' \"$1\"",
            &result("session", "SUCCESS", "answer"),
            None,
            Duration::from_secs(3),
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(messages
            .iter()
            .any(|m| matches!(m, StreamMessage::Done { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn oversized_stderr_is_terminated() {
        let messages = messages(
            "head -c 5000000 /dev/zero >&2; exec sleep 60",
            "",
            None,
            Duration::from_secs(3),
        );
        assert_failed_without_response(&messages, "output limit");
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_the_child_and_clears_its_pid() {
        let temp = tempfile::tempdir().unwrap();
        let token = Arc::new(CancelToken::new());
        let (sender, receiver) = std::sync::mpsc::channel();
        let other = token.clone();
        let cancel = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while other.child_pid.lock().unwrap().is_none() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            other.cancel_now();
        });
        run(
            command("exec sleep 60", &[], temp.path()),
            temp.path(),
            "request",
            Duration::from_secs(3),
            None,
            &sender,
            None,
            Some(&token),
        )
        .unwrap();
        cancel.join().unwrap();
        assert!(token.child_pid.lock().unwrap().is_none());
        drop(sender);
        assert!(receiver.into_iter().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn dropped_receiver_does_not_leave_the_process_running() {
        let temp = tempfile::tempdir().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        drop(receiver);
        let wire = "{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"agent_response\",\"text_delta\":\"answer\"}}\n";
        let start = Instant::now();
        run(
            command("printf '%s' \"$1\"; exec sleep 60", &[wire], temp.path()),
            temp.path(),
            "request",
            Duration::from_secs(3),
            None,
            &sender,
            None,
            None,
        )
        .unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_hook_discards_a_successful_terminal_response() {
        let temp = tempfile::tempdir().unwrap();
        let hook = AgyHookPrompt::create_in(temp.path(), "system").unwrap();
        std::fs::write(
            hook.state_path(),
            format!("start {}\nfail {}\n", hook.token(), hook.token()),
        )
        .unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        run(
            command(
                "printf '%s' \"$1\"",
                &[&result("session", "SUCCESS", "unverified")],
                temp.path(),
            ),
            temp.path(),
            "request",
            Duration::from_secs(3),
            None,
            &sender,
            Some(&hook),
            None,
        )
        .unwrap();
        drop(sender);
        assert_failed_without_response(
            &receiver.into_iter().collect::<Vec<_>>(),
            "system-prompt hook",
        );
    }

    fn live_command(directory: &Path, session: &str) -> Command {
        let mut command =
            Command::new(super::super::get_agy_path().expect("Agy must be installed"));
        command
            .args(super::super::build_agy_command_args(
                Some(session),
                "30s",
                &directory.join("agy.log"),
                None,
            ))
            .current_dir(directory)
            .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true");
        command
    }

    fn live_session_id() -> String {
        let id = format!("{:032x}", rand::random::<u128>());
        format!(
            "{}-{}-{}-{}-{}",
            &id[..8],
            &id[8..12],
            &id[12..16],
            &id[16..20],
            &id[20..]
        )
    }

    #[test]
    #[ignore = "requires an installed, authenticated Agy CLI and network access"]
    fn live_missing_session_rejects_agys_replacement_conversation() {
        if std::env::var("COKAC_AGY_LIVE_TEST").as_deref() != Ok("1") {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let session = live_session_id();
        assert!(!super::super::conversation_exists(&session));
        let (sender, receiver) = std::sync::mpsc::channel();
        // Exercise the post-precheck transport directly, as when a session
        // disappears between the existence check and the AGY process load.
        run(
            live_command(temp.path(), &session),
            temp.path(),
            "Reply only RESUME_CHECK. Do not use tools.",
            Duration::from_secs(40),
            Some(&session),
            &sender,
            None,
            None,
        )
        .unwrap();
        drop(sender);
        assert_failed_live(
            &receiver.into_iter().collect::<Vec<_>>(),
            "different conversation",
        );
    }

    fn assert_failed_live(messages: &[StreamMessage], expected: &str) {
        assert!(
            messages.iter().any(
                |m| matches!(m, StreamMessage::Error { message, .. } if message.contains(expected))
            ),
            "expected {expected}"
        );
        assert!(!messages.iter().any(|m| matches!(
            m,
            StreamMessage::Text { .. }
                | StreamMessage::AssistantFinal { .. }
                | StreamMessage::Done { .. }
        )));
    }

    #[test]
    #[ignore = "requires an installed Agy CLI; creates and removes its own corrupt test conversation"]
    fn live_corrupt_session_is_bounded_by_the_parent_timeout() {
        if std::env::var("COKAC_AGY_LIVE_TEST").as_deref() != Ok("1") {
            return;
        }
        struct TestConversation {
            path: std::path::PathBuf,
            identity: crate::services::file_ops::StablePathIdentity,
        }
        impl Drop for TestConversation {
            fn drop(&mut self) {
                let _ =
                    crate::services::file_ops::remove_file_by_identity(&self.path, self.identity);
            }
        }
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let session = live_session_id();
        let path = super::super::conversation_dir()
            .unwrap()
            .join(format!("{session}.db"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).unwrap();
        let _cleanup = TestConversation {
            path,
            identity: stable_file_identity(&file).unwrap(),
        };
        file.write_all(b"cokacdir test: deliberately invalid SQLite database\n")
            .unwrap();
        drop(file);
        assert!(super::super::conversation_exists(&session));
        let (sender, receiver) = std::sync::mpsc::channel();
        let start = Instant::now();
        run(
            live_command(temp.path(), &session),
            temp.path(),
            "Reply only CORRUPT_CHECK. Do not use tools.",
            Duration::from_secs(2),
            Some(&session),
            &sender,
            None,
            None,
        )
        .unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(5), "elapsed={elapsed:?}");
        drop(sender);
        assert_failed_live(&receiver.into_iter().collect::<Vec<_>>(), "overall timeout");
        eprintln!("Corrupt Agy conversation terminated after {elapsed:?}");
    }
}
