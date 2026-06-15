use std::ffi::OsString;
use std::sync::{mpsc, Mutex, MutexGuard};

use crate::services::claude::StreamMessage;
use crate::services::gjc::{
    build_gjc_args, execute_command_streaming, format_gjc_exit_message, is_gjc_model,
    strip_gjc_prefix,
};
use crate::services::gjc_events::parse_gjc_event;
use crate::services::gjc_path::prefer_source_linked_gjc_for_candidate;
use crate::services::gjc_sessions::resumable_session_id_in;
use serde_json::json;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

struct EnvVarGuard {
    key: &'static str,
    old_value: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let old_value = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, old_value }
    }

    fn remove(key: &'static str) -> Self {
        let old_value = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, old_value }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.old_value {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn detects_and_strips_gjc_models() {
    assert!(is_gjc_model(Some("gjc")));
    assert!(is_gjc_model(Some("gjc:openai/gpt-5.2")));
    assert!(!is_gjc_model(Some("codex")));
    assert_eq!(
        strip_gjc_prefix("gjc:openai/gpt-5.2"),
        Some("openai/gpt-5.2")
    );
}

#[test]
fn builds_noninteractive_json_args() {
    let args = build_gjc_args(
        Some("018fd6d5-11e4-7b16-9b21-5d9037ecb777"),
        Some("/tmp/system.md"),
        Some("openai/gpt-5.2"),
        true,
    );
    assert_eq!(
        args,
        vec![
            "-p",
            "--mode",
            "json",
            "--no-title",
            "--no-session",
            "--append-system-prompt",
            "/tmp/system.md",
            "--model",
            "openai/gpt-5.2",
            "--resume",
            "018fd6d5-11e4-7b16-9b21-5d9037ecb777",
        ]
    );
}

#[test]
fn missing_gjc_session_is_not_resumable() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "018fd6d5-11e4-7b16-9b21-5d9037ecb777";

    assert_eq!(resumable_session_id_in(Some(session_id), dir.path()), None);
}

#[test]
fn existing_gjc_session_file_is_resumable() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "018fd6d5-11e4-7b16-9b21-5d9037ecb777";
    let project_dir = dir.path().join("-tmp");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        project_dir.join(format!("2026-06-09T00-00-00-000Z_{session_id}.jsonl")),
        format!(r#"{{"type":"session","id":"{session_id}"}}"#),
    )
    .unwrap();

    assert_eq!(
        resumable_session_id_in(Some(session_id), dir.path()),
        Some(session_id)
    );
}

#[test]
fn gjc_session_header_is_resumable_even_without_id_in_filename() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "018fd6d5-11e4-7b16-9b21-5d9037ecb777";
    std::fs::write(
        dir.path().join("session.jsonl"),
        format!(
            "{}\n{}",
            format!(r#"{{"type":"session","id":"{session_id}"}}"#),
            r#"{"type":"message","message":{"role":"assistant","content":[]}}"#
        ),
    )
    .unwrap();

    assert_eq!(
        resumable_session_id_in(Some(session_id), dir.path()),
        Some(session_id)
    );
}

#[test]
fn parses_real_gjc_text_delta_without_replaying_snapshot() {
    let events = parse_gjc_event(&json!({
        "type": "message_update",
        "assistantMessageEvent": {
            "type": "text_delta",
            "delta": "ok"
        },
        "message": {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "ok"}
            ]
        }
    }));

    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], StreamMessage::Text { content } if content == "ok"));
}

#[test]
fn ignores_real_gjc_text_end_snapshot() {
    let events = parse_gjc_event(&json!({
        "type": "message_update",
        "assistantMessageEvent": {
            "type": "text_end",
            "content": "ok"
        },
        "message": {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "ok"}
            ]
        }
    }));

    assert!(events.is_empty());
}

#[cfg(unix)]
#[test]
fn gjc_path_prefers_source_linked_cli_for_suspicious_standalone() {
    use std::os::unix::fs::PermissionsExt;

    let _env_guard = env_lock();
    let _source_guard = EnvVarGuard::remove("COKAC_GJC_SOURCE_PATH");
    let dir = tempfile::tempdir().unwrap();
    let candidate = dir.path().join("gjc");
    std::fs::write(&candidate, b"\xcf\xfa\xed\xfe").unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&candidate)
        .unwrap();
    file.set_len(64 * 1024 * 1024).unwrap();
    let mut candidate_perms = std::fs::metadata(&candidate).unwrap().permissions();
    candidate_perms.set_mode(0o755);
    std::fs::set_permissions(&candidate, candidate_perms).unwrap();

    let source_cli = dir
        .path()
        .join(".bun/install/global/node_modules/@gajae-code/coding-agent/src/cli.ts");
    std::fs::create_dir_all(source_cli.parent().unwrap()).unwrap();
    std::fs::write(&source_cli, b"#!/usr/bin/env bun\n").unwrap();
    let mut source_perms = std::fs::metadata(&source_cli).unwrap().permissions();
    source_perms.set_mode(0o755);
    std::fs::set_permissions(&source_cli, source_perms).unwrap();

    let preferred = prefer_source_linked_gjc_for_candidate(&candidate, dir.path())
        .expect("source-linked GJC should be preferred over suspicious standalone");

    assert_eq!(preferred, source_cli);
}

#[test]
fn gjc_native_bunfs_failure_gets_actionable_message() {
    let stderr = "[Uncaught Exception] ResolveMessage: Cannot find module '@gajae-code/natives' from '/$bunfs/root/gjc-darwin-arm64'";

    let message = format_gjc_exit_message(Some(1), "/tmp/gjc", stderr);

    assert!(message.contains("broken Bun-compiled Gajae-Code standalone"));
    assert!(message.contains("@gajae-code/natives"));
    assert!(message.contains("source-linked"));
    assert!(message.contains("COKAC_GJC_PATH"));
}

#[test]
fn command_path_invokes_gjc_and_parses_json_events() {
    let _env_guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("gjc");
    let args_file = dir.path().join("args.txt");
    let stdin_file = dir.path().join("stdin.txt");
    let prompt_file = dir.path().join("prompt.txt");
    let missing_session_id = "018fd6d5-11e4-7b16-9b21-5d9037ecb777";
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nlast=''\nfor arg in \"$@\"; do last=\"$arg\"; done\ncase \"$last\" in @*) cat \"${{last#@}}\" > '{}' ;; esac\ncat > '{}'\necho '{{\"type\":\"session\",\"id\":\"018fd6d5-11e4-7b16-9b21-5d9037ecb777\"}}'\necho '{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"hello from gjc\"}}]}}}}'\n",
            args_file.display(),
            prompt_file.display(),
            stdin_file.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
    }
    let _path_guard = EnvVarGuard::set("COKAC_GJC_PATH", &script);
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).unwrap();
    let _agent_dir_guard = EnvVarGuard::set("GJC_CODING_AGENT_DIR", &agent_dir);

    let (tx, rx) = mpsc::channel();
    execute_command_streaming(
        "prompt body",
        Some(missing_session_id),
        dir.path().to_str().unwrap(),
        tx,
        Some("system body"),
        None,
        None,
        Some("openai/gpt-5.2"),
        true,
    )
    .unwrap();

    let events = rx.try_iter().collect::<Vec<_>>();
    assert!(matches!(events.first(), Some(StreamMessage::Init { .. })));
    assert!(events.iter().any(|event| {
        matches!(event, StreamMessage::Text { content } if content == "hello from gjc")
    }));
    assert_eq!(std::fs::read_to_string(stdin_file).unwrap(), "");
    assert_eq!(std::fs::read_to_string(prompt_file).unwrap(), "prompt body");
    let args = std::fs::read_to_string(args_file).unwrap();
    assert!(args.contains("--mode\njson"));
    assert!(args.contains("--model\nopenai/gpt-5.2"));
    assert!(args.contains("--no-session"));
    assert!(!args.contains("--resume"));
    assert!(args.lines().last().unwrap().starts_with('@'));
}

#[cfg(unix)]
#[test]
fn command_path_prefers_source_linked_cli_over_suspicious_standalone() {
    use std::os::unix::fs::PermissionsExt;

    let _env_guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let bad_bin = dir.path().join("bad-bin");
    let work_dir = dir.path().join("work");
    std::fs::create_dir_all(&bad_bin).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();

    let bad_gjc = bad_bin.join("gjc");
    std::fs::write(&bad_gjc, b"\xcf\xfa\xed\xfe").unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&bad_gjc)
        .unwrap();
    file.set_len(64 * 1024 * 1024).unwrap();
    let mut bad_perms = std::fs::metadata(&bad_gjc).unwrap().permissions();
    bad_perms.set_mode(0o755);
    std::fs::set_permissions(&bad_gjc, bad_perms).unwrap();

    let source_gjc = dir.path().join("source-gjc");
    std::fs::write(
        &source_gjc,
        "#!/bin/sh\n\
         echo '{\"type\":\"session\",\"id\":\"018fd6d5-11e4-7b16-9b21-5d9037ecb777\"}'\n\
         echo '{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"source linked gjc used\"}]}}'\n",
    )
    .unwrap();
    let mut source_perms = std::fs::metadata(&source_gjc).unwrap().permissions();
    source_perms.set_mode(0o755);
    std::fs::set_permissions(&source_gjc, source_perms).unwrap();

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let joined_path = format!("{}:{}", bad_bin.display(), old_path.to_string_lossy());
    let _path_env_guard = EnvVarGuard::set("PATH", joined_path);
    let _source_guard = EnvVarGuard::set("COKAC_GJC_SOURCE_PATH", &source_gjc);
    let _explicit_guard = EnvVarGuard::remove("COKAC_GJC_PATH");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).unwrap();
    let _agent_dir_guard = EnvVarGuard::set("GJC_CODING_AGENT_DIR", &agent_dir);

    let (tx, rx) = mpsc::channel();
    execute_command_streaming(
        "prompt body",
        None,
        work_dir.to_str().unwrap(),
        tx,
        None,
        None,
        None,
        None,
        true,
    )
    .unwrap();

    let events = rx.try_iter().collect::<Vec<_>>();
    assert!(events.iter().any(|event| {
        matches!(event, StreamMessage::Text { content } if content == "source linked gjc used")
    }));
    assert!(!events
        .iter()
        .any(|event| matches!(event, StreamMessage::Error { .. })));
}

#[cfg(unix)]
#[test]
fn command_path_reports_actionable_native_module_failure() {
    use std::os::unix::fs::PermissionsExt;

    let _env_guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("gjc");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         echo '[Uncaught Exception] ResolveMessage: Cannot find module '\\''@gajae-code/natives'\\'' from '\\''/$bunfs/root/gjc-darwin-arm64'\\''' >&2\n\
         exit 1\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let _path_guard = EnvVarGuard::set("COKAC_GJC_PATH", &script);
    let _source_guard = EnvVarGuard::remove("COKAC_GJC_SOURCE_PATH");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).unwrap();
    let _agent_dir_guard = EnvVarGuard::set("GJC_CODING_AGENT_DIR", &agent_dir);

    let (tx, rx) = mpsc::channel();
    execute_command_streaming(
        "prompt body",
        None,
        dir.path().to_str().unwrap(),
        tx,
        None,
        None,
        None,
        None,
        true,
    )
    .unwrap();

    let events = rx.try_iter().collect::<Vec<_>>();
    assert!(events.iter().any(|event| {
        matches!(
            event,
            StreamMessage::Error { message, stderr, exit_code, .. }
                if message.contains("broken Bun-compiled Gajae-Code standalone")
                    && message.contains("source-linked")
                    && message.contains("COKAC_GJC_PATH")
                    && stderr.contains("@gajae-code/natives")
                    && *exit_code == Some(1)
        )
    }));
}
