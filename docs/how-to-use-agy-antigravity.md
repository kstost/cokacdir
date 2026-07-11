# How cokacdir Uses Antigravity CLI (`agy`)

This page records the behavior measured against the local Antigravity CLI and the assumptions used by cokacdir's Agy provider.

Measured CLI version: `agy 1.1.1`.

## Invocation Contract

cokacdir gives Agy two separate inputs on every invocation:

1. Only the current user request is written to the process's non-TTY stdin.
   The pipe is then closed so Agy sees EOF and starts the request.
2. On Linux, the complete cokacdir system prompt is injected as a transient
   system message by Agy's official `PreInvocation` hook.

```bash
agy --print-timeout <duration> \
  --log-file ~/.cokacdir/tmp/<private-log> \
  --dangerously-skip-permissions
```

No `--print`, `-p`, or `--prompt` flag is passed. Agy 1.1.1 accepts piped stdin
as a headless prompt when those flags are absent. When a prompt is supplied by
one of those flags, Agy intentionally does not read stdin.

For the hook transport, cokacdir installs one namespaced plugin below Agy's
global plugin directory:

```text
~/.gemini/config/plugins/cokacdir-runtime-system-prompt/plugin.json
~/.gemini/config/plugins/cokacdir-runtime-system-prompt/hooks.json
```

The plugin is inert for ordinary Agy processes: without cokacdir's per-process
environment it consumes the hook input and returns an empty JSON object. During
a cokacdir run, the hook starts the same cokacdir executable through a private
internal entry point. The helper reads the complete system prompt from a
random, owner-only file:

```text
~/.cokacdir/tmp/agy_system_prompt_<random>
```

It returns `{"injectSteps":[{"ephemeralMessage":"..."}]}` to Agy. The path,
a random acknowledgement token, and the helper executable are passed only in
the Agy child process's environment. The prompt is not split into rule files,
no `--add-dir` is used, and the user's project, `AGENTS.md`, and active Agy
workspace are not changed.

When resuming a session, cokacdir creates a fresh private prompt file containing
the current complete system prompt and adds:

```bash
--conversation <session_id>
```

`PreInvocation` runs before every model call, so the helper returns the complete
system prompt each time Agy invokes it. The wrapper records a `start`/`ok` pair
for every invocation in a private ledger, while the helper acknowledges only
after its JSON response has been flushed. cokacdir polls that ledger, kills the
process tree when an invocation fails or stays incomplete for 30 seconds, and
holds all Agy stdout until the child exits with every recorded invocation
complete. An unverified response is discarded rather than forwarded.

The prompt, ledger, and acknowledgement remain until the Agy child exits, then
are removed on success, failure, cancellation, or unwinding. Each file is bound
to its creation-time filesystem identity so cleanup cannot delete a replacement
at the same pathname. Advisory locks distinguish live runs from crash residue;
the next Agy run removes unlocked stale hook files. All runtime temporary files
stay below `~/.cokacdir/tmp/`; cokacdir does not use `/tmp` as a fallback.

This hook path is enabled only on Linux, where it was measured against Agy
1.1.1. Agy hook execution is currently reported broken on Windows and not yet
verified here on macOS, so those platforms retain the older compatibility
transport that combines the system instructions and user request in stdin.

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

Agy's headless stdin interface is plain stdout, not JSONL or structured tool events.

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

These internal tool names are not emitted as structured stdout events in headless mode. cokacdir therefore streams only plain text for Agy and does not try to render per-tool events for this provider.

## Current Provider Limitations

- `/allowed` tool permissions are not enforced for Agy. The allowed-tools UI remains useful for Claude but does not constrain `agy`.
- `/loop` verification is not enabled for Agy because there is no measured isolated no-tools verifier mode equivalent to Claude fork sessions, Codex ephemeral execution, or OpenCode forked plan agents.
- Agy logs can contain benign internal errors even when stdout is successful. cokacdir only turns log summaries into user-visible errors when the process fails or when no visible stdout was produced.
- Agy treats hook failures as fail-open. The ledger and acknowledgement let cokacdir detect a `PreInvocation` that never started (or did not complete) and discard its output, but cannot prove that Agy actually applied an otherwise valid hook response. They also cannot undo a model request or tool side effect Agy started before termination.
- `ephemeralMessage` keeps the system prompt separate from the user's stdin message and normal transcript/checkpoint view. Agy 1.1.1 still persists that system step as plaintext in its conversation database, so this mechanism is not a secret-storage boundary.
- The prompt path, token, and ledger path are process environment values inherited by Agy's tool subprocesses. Full Agy permissions are intentional, but the temporary prompt file and hook handshake must not be treated as protection against code running with the same user account.
- A global no-op hook process is started before each model invocation in ordinary Agy sessions after the plugin has been installed. It does not inject any message without cokacdir's private environment, but it has a small process-startup cost.
