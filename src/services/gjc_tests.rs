use std::sync::mpsc;

use crate::services::claude::StreamMessage;
use crate::services::gjc::{
    build_gjc_args, execute_command_streaming, is_gjc_model, strip_gjc_prefix,
};
use crate::services::gjc_events::parse_gjc_event;
use crate::services::gjc_sessions::resumable_session_id_in;
use serde_json::json;

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

#[test]
fn command_path_invokes_gjc_and_parses_json_events() {
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
    std::env::set_var("COKAC_GJC_PATH", &script);
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).unwrap();
    std::env::set_var("GJC_CODING_AGENT_DIR", &agent_dir);

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
