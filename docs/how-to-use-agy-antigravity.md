# How cokacdir Uses Antigravity CLI (`agy`)

This page records the behavior measured against the local Antigravity CLI and the assumptions used by cokacdir's Agy provider.

Inference requests require Agy's `stream-json` output, introduced in 1.1.8.
The current transport is validated with Agy 1.1.27 on Linux.

For the full investigation, cross-platform design, implementation rationale,
failure analysis, and validation record, see [Agy system-prompt cross-platform
implementation](../devdoc/agy-system-prompt-cross-platform.md).

The original transport measurements below use `agy 1.1.1`. On September 5, 2026,
Linux validation with `agy 1.1.27` also checked all 14 available model IDs against
both JSON and text catalogs, and passed a live request/resume test that creates
a file through fresh hook instructions on each turn. Platform-specific caveats
and older-version observations are identified below.

The [AGY 1.1.27 verification record](../devdoc/agy-1.1.27-verification.md)
adds 18 CLI cases and a session-loss test through the actual adapter. It
distinguishes current failures from older observations and records the
session-ID and request-timeout defects addressed by the current transport.

## Invocation Contract

cokacdir gives Agy two separate inputs on every invocation:

1. Only the current user request is supplied through non-TTY stdin. A private
   file handle provides the complete input and EOF without blocking cokacdir
   if the child never reads its input.
2. On Linux, macOS, and Windows, the complete cokacdir system prompt is
   injected as a transient system message by Agy's official `PreInvocation`
   hook.

```bash
agy --output-format stream-json --print-timeout <duration> \
  --log-file ~/.cokacdir/tmp/<private-log> \
  --dangerously-skip-permissions
```

No `--print`, `-p`, or `--prompt` flag is passed. Agy 1.1.27 accepts non-TTY stdin
as a headless prompt when those flags are absent. When a prompt is supplied by
one of those flags, Agy intentionally does not read stdin.

Stdin, stdout, and stderr use private temporary files with independent read and
write handles. Their names are unlinked before the child starts (Windows uses
delete-pending handles), so these new I/O files do not remain after a crash.
The parent can read output as it grows without waiting on pipe EOF from a
descendant process. The separately named hook files below still use their lease
and ownership checks because hook subprocesses need to reopen them.

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
random per-run file (mode `0600` on Unix):

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

The prompt, ledger, acknowledgement, and a small lease file remain until the
Agy child exits, then are removed on success, failure, cancellation, or
unwinding. The prompt, ledger, and lease are bound to their creation-time
filesystem identities, and the acknowledgement is identity-verified before
removal, so cleanup cannot delete a replacement at the same pathname. A shared
lock on the separate lease distinguishes live runs from crash residue without
locking the prompt or ledger that the hook child must read and write. The next
Agy run removes stale hook files whose lease is no longer live. All runtime
temporary files stay below `~/.cokacdir/tmp/`; cokacdir does not use `/tmp` as
a fallback.

This hook path is enabled on Linux, macOS, and Windows. Unix builds use the
POSIX-shell wrapper and Windows builds use a `cmd.exe`-compatible wrapper; both
record the same `start`/`ok`/`fail` ledger protocol. The behavior above was
measured against Agy 1.1.1 on Linux. macOS and Windows now use the same
separate-hook transport in the implementation rather than the older combined
stdin fallback, but still require platform-specific live Agy coverage. In
particular, [upstream reports of hook-dispatch failures on older Windows and
macOS builds](https://github.com/google-antigravity/antigravity-cli/issues/222)
and the [cokacdir Windows hook report](https://github.com/kstost/cokacdir/issues/53)
remain outside the Linux live validation. cokacdir deliberately does not fall back to putting the system
prompt in stdin: if Agy does not run and acknowledge the hook, cokacdir kills
the invocation and discards its output. This keeps a resumed session from
accumulating fallback copies of the system prompt.

### Stored session data versus model context

Agy's conversation database can retain historical records of the ephemeral
step injected for earlier model calls. That storage history is not the same as
the effective context sent to the model. In the measured Linux Agy 1.1.1
session, SQLite retained four hook-injected system-step rows for four model
invocations, while the normal transcript omitted those rows and each generation
was paired with one newly injected ephemeral step. Under [Agy's documented
"transient system message"
contract](https://antigravity.google/docs/hooks#preinvocation), the historical
rows are not replayed as additional system-message copies: each effective model
context receives the current cokacdir prompt once. Agy does not expose the raw
provider request body, so this conclusion is based on its documented contract
plus the session and generation trace rather than a network-payload capture.

macOS and Windows use the same Agy `injectSteps`/`ephemeralMessage` path and the
same cokacdir transport; the operating-system-specific part is only the shell
wrapper. That cross-platform equivalence follows from the shared Agy protocol
and binary implementation, while direct session traces have so far been
captured only on Linux.

## Model Handling

Models are discovered from the installed Agy CLI instead of a hardcoded list.
Modern Agy returns separate model IDs and display labels. cokacdir first runs
`agy --output-format json models` and reads `command.data.models`, whose entries
have `id` and `label` fields. If that interface is unavailable, it falls back to
`agy models`, splitting each `ID<TAB>label` row. Agy 1.1.1's single-column display
labels remain supported by the catalog parser. This does not remove the newer
structured-output requirement for running inference requests.

For example, the model menu displays:

```text
/model agy:gemini-3.8-flash-high — Gemini 3.8 Flash (High)
/model agy:claude-sonnet-4-6 — Claude Sonnet 4.6 (Thinking)
```

Only the model ID is included in the copyable command, saved for new selections,
and passed to `--model`. Existing display-label selections and copied tab-separated
rows are resolved to the current ID; ambiguous display labels are rejected.

The catalog is cached for five minutes and refreshed on the next lookup after
expiry. Opening `/model` forces an immediate refresh without restarting the bot.
If a refresh fails, the last successful list is retained and ordinary lookups
retry after 15 seconds; `/model` can retry immediately. The menu identifies a
cached list after a failed refresh, or reports that no list could be fetched.
Lookup failures are reported separately from invalid model selections.

Model discovery has a 30-second total process-wait budget, including any fallback
to text output. Timed-out children are terminated and reaped, and captured output
is limited to 1 MiB per stream when read. Concurrent requests can keep using the
cached catalog while a refresh is in progress. Model selection runs outside the
async message-handling thread, and reading a legacy `gemini:` setting never
starts a model lookup or discards the saved selection on a network failure.

Requested models are validated against the discovered catalog before execution.
Agy 1.1.1 could fall back silently for invalid model labels, while newer headless
versions reject unknown models. See the official
[headless model selection documentation](https://antigravity.google/docs/cli/headless/#select-a-model-effort-or-agent).

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

Older measurements recorded replayed stdout when resuming with `--conversation`.
In the additional 1.1.27 text/JSON measurements, only the current turn's answer
was returned, and prior context was preserved. The adapter now uses the terminal
`result.response` as the authoritative answer, without textual replay heuristics.

Measured behavior: a missing conversation can exit successfully and print a warning before starting a new response:

```text
Warning: conversation "<id>" not found.
```

cokacdir prevalidates that the conversation file exists before starting Agy.
In 1.1.27, a missing ID still produced a warning on stderr and a successful new
conversation with a different ID. The adapter checks IDs in structured events
and requires the terminal ID to match the requested ID on resume. A replacement
session causes an error and its response is discarded. New session IDs come from
the request's result rather than the shared `last_conversations.json` cache.

Scheduled runs copy the conversation through SQLite's backup API, including
committed data still in the source WAL. In the current schema, the clone's
`trajectory_meta.cascade_id` must also match the new filename ID; copying pages
alone produces `trajectory not found` in Agy 1.1.27. cokacdir updates that field
through the reserved destination handle while preserving the trajectory and
message history. Ambiguous metadata aborts the clone without modifying the
source. This storage format is internal and requires verification when Agy
changes it. The documented interactive `/fork` command is unavailable in the
measured headless CLI even when `--conversation` is provided.

## Stdout and Stderr

The adapter parses Agy's `stream-json` events. It accepts a preflight `ERROR`
result without an `init` event and requires exactly one terminal result for a
completed request. Invalid JSON, missing/duplicate results, inconsistent IDs,
non-success status, empty answers, and nonzero exits cannot publish a successful
completion. Thinking and tool-event payloads are not forwarded as assistant text.

New calls without a system-prompt hook can stream assistant text deltas. Resumed
calls and calls with a hook retain all assistant text until the process and
protocol checks succeed. `AssistantFinal` and `Done` use the terminal response.

Historically observed successful text-mode stdout shapes:

- final answer only, e.g. `QUOTA_RECHECK_TWO_OK`
- narration plus final marker, e.g. file writes produced lines such as `I will read...` before `FS_WRITE_OK`
- markdown links in text, e.g. grep returned a `file:///...` link
- resume output that includes previous assistant text plus the new answer

Older stderr observations included failures with empty stderr.

Observed stdout failure shapes:

```text
Error: timed out waiting for response
Error: failed to send message: ...
Warning: conversation "<id>" not found.
```

These appeared with exit code `0` in older text-mode measurements. The current
adapter uses terminal status and the error field rather than classifying the
wording of an assistant's answer.

Additional 1.1.27 probes for an unknown model, an empty explicit prompt, and a
response timeout all exited `1`. Error-looking text was also returned as an
ordinary successful answer when explicitly requested, so matching `Error:` in
response text would introduce false positives. Real authentication and quota failures were not induced
in this additional validation.

### Request time limits

`COKAC_AGY_PRINT_TIMEOUT` defaults to `1h` and accepts positive Go-style durations
such as `30s`, `1m30s`, and `1.5m`. The value is passed to Agy and independently
enforced from process startup through exit by cokacdir. Agy's own timer may not
cover startup failures, so the parent terminates and reaps the child on timeout.
The hook's 30-second pending check and the model catalog's 30-second lookup
budget remain separate checks.

The parent terminates requests whose captured stdout exceeds 64 MiB or stderr
exceeds 4 MiB, and limits individual JSON events to 16 MiB. Failures include up to
16 KiB of captured stderr for diagnosis. Cancellation terminates the active
request; a failed streaming delivery also terminates the child when cokacdir
detects that the receiver has closed.

## Log-Only Failures

Older measurements found runs that exited `0` with empty stdout/stderr and put
the actual failure only in the log file.

Measured quota failure:

```text
RESOURCE_EXHAUSTED (code 429): Individual quota reached. ... Resets in ...
PlannerResponse without ModifiedResponse encountered
```

Every Agy run gets a dedicated `--log-file`, but the adapter does not parse that
log for completion status. Structured `error` fields and captured stderr provide
the primary diagnosis. If the CLI emits neither, a missing-result or parent
timeout error is reported; log-only details are not reconstructed.

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

Current Agy exposes step events in stream-json. cokacdir currently forwards
assistant prose and does not render Agy's per-tool events as separate UI items.

## Current Provider Limitations

- The entire `allowed_tools` feature is Claude-only. `/availabletools`, `/allowedtools`, and `/allowed` are rejected while Agy is active, and the saved Claude list is neither passed to Agy nor injected into its prompt. Agy retains its native/full permissions.
- `/loop` verification is not enabled for Agy because there is no measured isolated no-tools verifier mode equivalent to Claude fork sessions, Codex ephemeral execution, or OpenCode forked plan agents.
- Agy logs can contain benign internal errors even when a request succeeds. cokacdir relies on the process and structured terminal result, plus its own hook checks, rather than classifying log lines.
- Agy treats hook failures as fail-open. The ledger and acknowledgement let cokacdir detect a `PreInvocation` that never started (or did not complete) and discard its output, but cannot prove that Agy actually applied an otherwise valid hook response. They also cannot undo a model request or tool side effect Agy started before termination.
- `ephemeralMessage` keeps the system prompt separate from the user's stdin message and normal transcript/checkpoint view. Agy 1.1.1 still persists that system step as plaintext in its conversation database, so this mechanism is not a secret-storage boundary.
- The prompt path, token, and ledger path are process environment values inherited by Agy's tool subprocesses. Full Agy permissions are intentional, but the temporary prompt file and hook handshake must not be treated as protection against code running with the same user account.
- A global no-op hook process is started before each model invocation in ordinary Agy sessions after the plugin has been installed. It does not inject any message without cokacdir's private environment, but it has a small process-startup cost.
