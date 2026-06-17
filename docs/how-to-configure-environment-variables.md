# How to Configure Environment Variables

cokacdir reads a number of environment variables at startup to override binary paths, tune internal limits, and toggle debug logging. This page describes every environment variable the program consults, how to set them, and how to inspect their current values from within a running bot.

---

## Where to set environment variables

There are two ways to set environment variables for cokacdir:

### 1. `~/.cokacdir/.env.json` (recommended)

On startup, cokacdir reads `~/.cokacdir/.env.json` and injects every key/value pair from that file into the process environment. This is the most convenient place to store configuration because it persists across sessions without touching your shell profile.

Example `~/.cokacdir/.env.json`:

```json
{
  "COKAC_CLAUDE_PATH": "/home/alice/.local/bin/claude",
  "COKAC_CODEX_PATH": "/opt/codex/codex",
  "COKAC_FILE_ATTACH_THRESHOLD": "16384",
  "COKACDIR_DEBUG": "1"
}
```

The file must contain a **JSON object** at the root. Each key becomes an environment variable name, and its value becomes the value of that variable. Supported value types are **string**, **number**, and **boolean**. Objects, arrays, and `null` values are skipped with a warning printed to stderr.

**Important:** values in `.env.json` take **priority over the existing environment**. If you already have `COKAC_CLAUDE_PATH` exported in your shell and also set it in `.env.json`, the `.env.json` value wins. This means `.env.json` is the single source of truth — use it rather than mixing with shell exports to avoid confusion.

If the file does not exist, cokacdir silently proceeds with whatever is already in the process environment. If the file exists but contains invalid JSON (or a non-object root like a JSON array), a warning is printed and the file is ignored — startup continues normally.

> ⚠ **Boolean and number values are serialized to strings literally.** If you write `{"COKACDIR_DEBUG": true}`, cokacdir sets the environment variable to the literal string `"true"` — not `"1"`. Since `COKACDIR_DEBUG` only enables debug when its value equals `"1"`, writing `true` will *not* enable debug. Use the string `"1"` or the number `1` instead. The same applies to any variable whose code path expects a specific string — always check the variable's documented format below rather than assuming truthy-coercion.

### 2. Shell exports

You can also export variables the usual way before launching `cokacdir` or `cokacctl`:

```bash
export COKAC_CLAUDE_PATH=/home/alice/.local/bin/claude
cokacctl
```

This works, but any keys that also appear in `~/.cokacdir/.env.json` will be overwritten when the program starts.

---

## Environment variable reference

### `COKAC_CLAUDE_PATH`

Override the path to the Claude CLI binary. Normally cokacdir resolves Claude automatically with `which claude` (falling back to `bash -lc "which claude"` for non-interactive SSH sessions, and `SearchPathW` on Windows). Set this variable if you want to pin a specific installation, or if automatic resolution fails in your environment.

- **Type:** absolute path to an existing executable
- **Default:** not set (automatic resolution)
- **Behavior:** If the value is empty or points to a non-existent file, cokacdir falls through to the normal resolution logic rather than failing.
- **Example:** `COKAC_CLAUDE_PATH=/home/alice/.local/bin/claude`

### `COKAC_CODEX_PATH`

Override the path to the Codex CLI binary. Same semantics as `COKAC_CLAUDE_PATH` but for Codex. On Windows, the fallback resolver prefers `.cmd` (npm batch wrapper) over `.exe`.

- **Type:** absolute path to an existing executable
- **Default:** not set (automatic resolution)
- **Example:** `COKAC_CODEX_PATH=/opt/codex/codex`

### `COKAC_AGY_PATH`

Override the path to the Antigravity CLI `agy` binary. Same semantics as above but for Agy.

- **Type:** absolute path to an existing executable
- **Default:** not set (automatic resolution)
- **Example:** `COKAC_AGY_PATH=/usr/local/bin/agy`

### `COKAC_OPENCODE_PATH`

Override the path to the Opencode CLI binary. Same semantics as above but for Opencode. Note that Opencode is not supported on Windows — setting this variable on Windows has no effect.

- **Type:** absolute path to an existing executable
- **Default:** not set (automatic resolution)
- **Example:** `COKAC_OPENCODE_PATH=/usr/local/bin/opencode`

### `COKAC_FILE_ATTACH_THRESHOLD`

Controls the size threshold (in bytes) at which the bot switches from sending a response as multiple Telegram messages to sending it as a single `.txt` file attachment.

- **Type:** positive integer (bytes)
- **Default:** `8192` (twice Telegram's 4096-byte per-message limit)
- **Behavior:** Responses whose length exceeds this threshold are uploaded as a text file instead of being split into multiple chat messages. Lower the value if you prefer files sooner; raise it to keep more content inline.
- **Example:** `COKAC_FILE_ATTACH_THRESHOLD=16384` — switch to file attachment only when the response exceeds 16 KB.
- **Invalid values** (non-numeric, negative, etc.) are silently ignored and the default is used.

### `COKAC_SCHEDULE_INLINE`

Run scheduled tasks **inline in the chat's current session** instead of the default isolated workspace. With this enabled, the schedule's prompt and reply appear in the live conversation as if you had typed them yourself at the trigger time — see `how-to-use-schedules.md` for the full user-facing flow.

- **Type:** string — set to exactly `"1"` to enable. Any other value (including unset, `"0"`, `"true"`, `"yes"`) is treated as disabled. The check is strict string comparison, matching the convention of `COKACDIR_DEBUG`.
- **Default:** not set (disabled — schedules use isolated workspaces, the original behavior).
- **States:** binary. The variable has only two effective states — true (value is the literal string `"1"`) or false (everything else).
- **Scope:** loaded once at bot startup from `~/.cokacdir/.env.json`. Restart the bot after changing the value.
- **Example `.env.json`:**

  ```json
  {
    "COKAC_SCHEDULE_INLINE": "1"
  }
  ```

- **Behavior when enabled:**
  - At trigger time, the bot does **not** create a new workspace under `~/.cokacdir/workspace/<schedule_id>`, does **not** swap the chat's session for a fresh one, and does **not** inject `context_summary` into the prompt.
  - Instead, the schedule's prompt is sent to the provider with `--resume <chat's current session_id>` and `cwd = chat's current_path`, so the scheduled task continues the conversation already in progress.
  - The schedule prompt and the AI's reply are appended to the chat's session history just as if the user had typed the prompt themselves, then persisted to disk so they survive bot restarts.
  - If the provider issues a new `session_id` during the run (e.g. Claude fork-on-resume), the chat session's `session_id` is updated to the new one so the user's next message lands on the same continuation.
  - The bot does **not** append the `Use /<schedule_id> to continue this schedule session` hint to the reply, because the chat itself is the continuation point.
- **Safety fallback:** if the chat has no active session (no `current_path`) at trigger time, the bot silently falls back to the isolated-workspace path so the schedule still fires.
- **Concurrency:** unchanged — the existing busy/`pending_schedules` mechanism keeps inline runs serialized against the user's own messages. If you are mid-conversation when the schedule's trigger time hits, it queues until your current exchange finishes.
- **Verifying it loaded:** in a 1:1 chat with the bot, run `/envvars` (owner-only) and confirm `COKAC_SCHEDULE_INLINE=1` is listed. Be aware `/envvars` dumps every environment variable including secrets — clear those messages afterward.

> ⚠ **Side effects of enabling.** Inline mode is invasive by design — the schedule's prompt and reply land in the chat's live conversation, which is what most users want when this flag is on. Consequences worth knowing:
> - Recurring cron schedules in inline mode reuse the chat's session every run, so context accumulates instead of being summarized between runs. Use `/clear` if it grows unwieldy.
> - One-time inline schedules cannot be re-entered via `/<schedule_id>` because no isolated workspace is created — the work is already in the chat's live session. The shortcut will return "no workspace found" if attempted.
> - Tools that consult `entry.current_path` (the path the schedule was registered from) are bypassed in favor of whatever path the chat is currently on. Moving the chat with `/start <other_path>` between trigger times means the next run executes in that new path.

### `COKACDIR_DEBUG`

Enable debug logging globally at startup. This is the programmatic way to turn on debug for automated runs and CI — achieving the same effect as manually toggling `/debug` to ON in every chat after the bot starts.

- **Type:** string — set to exactly `"1"` to enable. The check is a strict string comparison (`value == "1"`), not a truthy coercion.
- **Default:** not set.
- **Scope:** global — affects all chats and all bots in the same process.
- **Behavior:** When debug is ON, detailed logs for Telegram API operations, AI service calls, and the cron scheduler are printed to stdout. Once enabled at startup, you can still toggle it at runtime with `/debug`.
- **Example:** `COKACDIR_DEBUG=1`

**Important — this variable cannot disable debug on its own.** The startup logic is a two-step check:

1. If `COKACDIR_DEBUG` equals `"1"`, debug is enabled immediately.
2. **Otherwise** (including when the variable is unset, empty, or set to any value other than `"1"` — such as `"0"`, `"false"`, `"true"`, `"yes"`), cokacdir falls through to read `~/.cokacdir/bot_settings.json` and enables debug if **any** bot in that file has `"debug": true`.

In other words, setting `COKACDIR_DEBUG=0` does **not** guarantee debug is off — it only skips the env-var enable path, after which `bot_settings.json` may still turn debug on. To definitively keep debug off, make sure no bot has `"debug": true` in `bot_settings.json` **and** that `COKACDIR_DEBUG` is not set to `"1"`. At runtime you can send `/debug` to flip the state back off, but be aware that `/debug` is a **pure toggle** — it takes no arguments and simply inverts the current state, so confirm the resulting state from the bot's reply (`Debug logging: ON` or `OFF`).

---

## `/envvars` — Inspect the running environment

`/envvars` is a chat command that prints every environment variable currently visible to the bot process, along with its value. The variables are sorted alphabetically and rendered as `KEY=VALUE` pairs in the response.

```
/envvars
```

### Access control

- **Bot owner only.** Non-owners are rejected with the message `Only the bot owner can use /envvars.` This matches the other admin-only commands in cokacdir.
- The command is available only in a 1:1 chat with the bot. Group chats are rejected even for the owner, to avoid accidentally posting secrets where other members can read them.

### ⚠ Security warning — `/envvars` exposes sensitive values

`/envvars` dumps **every** environment variable visible to the bot process, including API keys, authentication tokens, database credentials, and anything else that happens to be exported. There is **no redaction** — the code comment in the implementation explicitly notes this is intentional for admin debugging on a personal, single-user bot.

Be aware of the following before using it:

- Telegram message history is stored on Telegram's servers. Anything you send via `/envvars` is persisted there until you delete the messages.
- If you forward the response, screenshot it, or share your chat with anyone, the secrets are exposed.
- If a bot's owner account is ever compromised, the attacker can run `/envvars` and harvest every secret in your environment in one command.
- Group chats are rejected for `/envvars`. Always use it in a 1:1 chat with the bot so secrets are not posted where other members can read them.

Treat `/envvars` as a diagnostic tool for verifying configuration — for example, confirming that `.env.json` loaded correctly or that `COKAC_CLAUDE_PATH` is pointing where you expect — and clear the messages afterward.

### When to use it

- Verifying that `~/.cokacdir/.env.json` was loaded and your keys are applied.
- Checking whether a `COKAC_*` override is active in the running process.
- Diagnosing why a binary path override is not being picked up (for example, the variable is set but the file doesn't exist, so the fallback resolver ran instead).

---

## Troubleshooting

- **My `.env.json` doesn't seem to load.** Confirm the file is at exactly `~/.cokacdir/.env.json` (note the leading dot), that it is valid JSON, and that the root is a **JSON object** (`{ ... }`, not an array or a bare scalar). The values of that object's keys must each be a string, number, or boolean — objects, arrays, and `null` values are skipped with a warning. Run `/envvars` to see which variables are actually in the process environment.
- **`COKAC_CLAUDE_PATH` is set but Claude still uses the wrong binary.** The override is only used if the file at that path exists. If the path is wrong or the file is missing, cokacdir silently falls back to `which claude`. Double-check the path and file permissions.
- **`/envvars` returns "Only the bot owner can use /envvars."** You are not registered as the owner of this bot. The owner is the Telegram user ID that first successfully interacted with the bot after it started; see the token management and first-chat guides for how ownership is established.
