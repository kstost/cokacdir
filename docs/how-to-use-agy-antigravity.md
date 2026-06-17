# How cokacdir Uses Antigravity CLI (`agy`)

This page records the behavior measured against the local Antigravity CLI and the assumptions used by cokacdir's Agy provider.

Measured CLI version: `agy 1.0.8`.

## Invocation Contract

cokacdir runs Agy in print mode:

```bash
agy --print "" --print-timeout <duration> --log-file <temp-log> --dangerously-skip-permissions
```

The user prompt is written to stdin. When resuming a session, cokacdir adds:

```bash
--conversation <session_id>
```

The empty string after `--print` is intentional. A bare `--print` without a value is unsafe: it can make Agy use unintended context and produce unrelated output. `--prompt` is an alias for `--print`, but cokacdir uses `--print ""` consistently.

## Model Handling

`agy models` returns display labels, for example:

- `Gemini 3.5 Flash (Medium)`
- `Gemini 3.5 Flash (High)`
- `Gemini 3.5 Flash (Low)`
- `Gemini 3.1 Pro (Low)`
- `Gemini 3.1 Pro (High)`
- `Claude Sonnet 4.6 (Thinking)`
- `Claude Opus 4.6 (Thinking)`
- `GPT-OSS 120B (Medium)`

Measured behavior: an invalid `--model` label may still run instead of failing. cokacdir therefore validates requested Agy model labels against `agy models` before spawning the process.

Legacy `gemini` and `gemini:<model>` settings are accepted only as compatibility aliases and are routed through the Agy provider.

## Session Storage and Resume

Agy stores conversations under:

```text
~/.gemini/antigravity-cli/conversations/<session_id>.db
~/.gemini/antigravity-cli/conversations/<session_id>.pb
```

The latest conversation cache is under:

```text
~/.gemini/antigravity-cli/cache/last_conversations.json
```

Measured behavior: resuming with `--conversation` can replay previous stdout before adding new content. cokacdir strips already-seen assistant output before forwarding text to chat.

Measured behavior: a missing conversation can exit successfully and print a warning before starting a new response:

```text
Warning: conversation "<id>" not found.
```

cokacdir prevalidates the conversation file and treats that warning as fatal if it appears.

## Stdout and Stderr

Agy's print-mode interface is plain stdout, not JSONL or structured tool events.

Observed successful stdout shapes:

- final answer only, e.g. `QUOTA_RECHECK_TWO_OK`
- narration plus final marker, e.g. file writes produced lines such as `I will read...` before `FS_WRITE_OK`
- markdown links in text, e.g. grep returned a `file:///...` link
- resume output that includes previous assistant text plus the new answer

Observed stderr behavior: stderr is usually empty, even when the actual run failed.

Observed stdout failure shapes:

```text
Error: timed out waiting for response
Error: failed to send message: ...
Warning: conversation "<id>" not found.
```

These can appear with exit code `0`, so cokacdir parses stdout content instead of trusting the process status alone.

## Log-Only Failures

Agy can exit with code `0`, emit empty stdout and empty stderr, and put the actual failure only in the log file.

Measured quota failure:

```text
RESOURCE_EXHAUSTED (code 429): Individual quota reached. ... Resets in ...
PlannerResponse without ModifiedResponse encountered
```

Adapter implication: every Agy run gets a dedicated `--log-file`, and cokacdir reads that log when stdout is empty or no new resume output remains after deduplication.

Measured auth behavior: startup can log `You are not logged into Antigravity` before silent auth succeeds. That line is not necessarily fatal. It should only be surfaced as a failure if there is no later auth success line such as:

```text
Print mode: silent auth succeeded
applyAuthResult: ...
OAuth: authenticated successfully ...
```

## Tool Capability Probes

After quota recovered on 2026-06-17, these live probes produced stdout successfully:

| Probe | Observed stdout |
| --- | --- |
| filesystem list/read | `FS_READ_OK 4` |
| filesystem write/edit | narration followed by `FS_WRITE_OK`; file edits were present on disk |
| shell command | `SHELL_OK` |
| grep/search | `GREP_OK .../src/input.txt` |
| web/read-url/search | `WEB_OK example.com` |
| browser | `BROWSER_OK Example Domain` |
| subagent | narration followed by `SUBAGENT_OK subagent-pong` |
| MCP availability | `MCP_OK none` |
| skill/knowledge availability | `SKILL_KNOWLEDGE_OK 9` |

Conversation database strings also show internal tool/action names such as `list_dir`, `read_file`, `write_to_file`, `replace_file_content`, `multi_replace_file_content`, `run_command`, `grep_search`, `search_web`, `read_url`, `execute_url`, `browser`, `read_browser_page`, `invoke_subagent`, `define_subagent`, `send_message`, `send_input`, `mcp`, `skills`, and `knowledge`.

These internal tool names are not emitted as structured stdout events in print mode. cokacdir therefore streams only plain text for Agy and does not try to render per-tool events for this provider.

## Current Provider Limitations

- `/allowed` tool permissions are not enforced for Agy. The allowed-tools UI remains useful for Claude but does not constrain `agy`.
- `/loop` verification is not enabled for Agy because there is no measured isolated no-tools verifier mode equivalent to Claude fork sessions, Codex ephemeral execution, or OpenCode forked plan agents.
- Agy logs can contain benign internal errors even when stdout is successful. cokacdir only turns log summaries into user-visible errors when the process fails or when no visible stdout was produced.
