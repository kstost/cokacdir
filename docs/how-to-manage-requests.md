# How to Manage Requests

## Overview

When you send a message to the bot, it starts an AI request. While a request is in progress, additional messages are placed in a queue (up to 20) and processed in order. You can cancel requests, manage the queue, and remove individual queued messages.

---

## /stop

Cancels the currently in-progress AI request for the chat.

- The running AI process is terminated immediately.
- A "Stopping..." message is shown while cancellation completes.
- The message queue is **not** affected — the next queued message will begin processing automatically after cancellation.
- If no request is in progress, the bot replies with `No active request to stop.`

## /stopall

Cancels the in-progress request **and** clears the entire message queue in one operation.

- Use this when you want to cancel everything and start fresh.
- Reports how many queued messages were cleared.

```
Stopping... (3 queued message(s) cleared)
```

## /stop \<ID\>

Removes a specific message from the queue by its ID.

When a message is queued, the bot assigns it a short hex ID (e.g., `A394FDA`) and shows it in the queue confirmation. Use this ID to cancel that specific queued message:

```
/stop A394FDA
```

The match is case-insensitive. `/stop_A394FDA` also works.

On a successful removal the bot replies with `Removed queued message (A394FDA).` If the ID is not found in the queue, no message is sent.

---

## Message Queue

### How It Works

When the AI is busy processing a request, any new messages you send are placed in a queue instead of being rejected. Queued messages are processed one by one in the order they were received (FIFO).

When a message is queued, the bot responds with:

```
Queued (A394FDA) "preview of your message..."
- /stopall to cancel all
- /stop_A394FDA to cancel this
```

### Queue Limits

- Maximum queue size: **20 messages**
- If the queue is full, new messages are rejected with: "Queue full (max 20). Use /stopall to clear."

### File Uploads

If you send a file while the AI is busy, the file upload is captured at queue time and attached to the queued message. When the message is later processed, the file context is correctly preserved.

### /queue

Toggles queue mode on or off for the current chat. Queue mode is **ON by default**.

- **Queue ON** (default): Messages sent while AI is busy are appended to the queue (FIFO, up to 20) and processed in order after the current request completes.
- **Queue OFF**: Messages sent while AI is busy use **redirect (latest-wins)** semantics — the in-progress task is cancelled and replaced by the new message, which becomes the next thing the AI processes.

Toggling reports the new state:

```
📋 Queue mode: ON
Messages sent while AI is busy will be queued and processed in order.

📋 Queue mode: OFF
3 queued message(s) cleared.
```

When turning queue OFF, any messages already in the queue are cleared.

#### Redirect behavior (Queue OFF)

When the AI is busy and you send a new prompt with queue OFF, the bot atomically:

1. Cancels the in-progress AI request (same effect as `/stop`).
2. Clears any active loop / verification state for the chat.
3. Replaces any pending queued message with the new one. If a previous redirect is still pending (the cancelled task hasn't fully wound down yet), it is silently overwritten.
4. Once cancellation completes, the new message is dispatched as the next request.

Confirmation messages:

```
🔄 Cancelling current task, will process: "preview..."   ← first redirect while busy
🔄 Redirect target updated: "preview..."                 ← overwrote a still-pending redirect
```

File uploads sent alongside the redirected message are captured and attached just like in queued mode, so file context is preserved across the cancel-and-replace.

---

## How /stop and Queue Interact

Behavior with Queue **ON**:

| Situation | /stop | /stopall |
|-----------|-------|----------|
| AI busy, queue has messages | Cancels current request; next queued message starts automatically | Cancels current request and clears all queued messages |
| AI busy, queue empty | Cancels current request | Cancels current request |
| AI idle, queue has messages | No effect | Clears all queued messages |

With Queue **OFF**, the queue stays empty under normal use because new prompts redirect (cancel + replace) instead of stacking up. `/stop` and `/stopall` behave the same as ON-mode "queue empty" rows above.

---

## Loop — Self-Verification Loop

The `/loop` command keeps running the same task until the bot itself decides the task is fully and correctly completed. After every response, the bot runs a provider-specific verification step and asks the AI to judge whether the work is done. If not, it re-injects the remaining work as the next prompt and tries again.

Useful for tasks where one shot is rarely enough — multi-step refactors, "keep trying until tests pass", "fix everything the linter reports", and similar.

### Usage

```
/loop <request>           → repeat up to 5 times (default)
/loop <N> <request>       → repeat up to N times
/loop 0 <request>         → repeat with no upper bound (use with care)
```

Examples:

```
/loop fix all clippy warnings in the project
/loop 10 add unit tests until coverage is above 90%
/loop 0 keep trying until the build passes
```

### Requirements

- **Claude, Codex, or OpenCode model.** Each provider has its own isolation mechanism for verification:
  - **Claude**: `--fork-session` (native live-session fork).
  - **Codex**: independent `codex exec --ephemeral` with a transcript synthesized from the full-fidelity archive. No `resume`, no `thread_id` — the original session file is byte-identical before/after.
  - **OpenCode**: native `opencode run --session <ID> --fork --agent plan` — OpenCode forks the session natively so the original stays intact. Output is read as plain text (agent reply lands on stdout, banner decorations go to stderr). The forked session row persists in `opencode.db` — same "fork and leak" pattern as Claude's `--fork-session` which also leaves its forked .jsonl file behind.
  Agy sessions will be rejected with a message.
- **One loop per chat at a time.** If a loop is already running, a new `/loop` is rejected. Use `/stop` to cancel the current loop first.

### What You'll See

| Message | Meaning |
|---------|---------|
| `🔄 Loop started (max N iterations)` | Loop has begun |
| `🔄 Loop started (unlimited)` | Started in `/loop 0` mode |
| `🔍 Verifying...` (animated 🔍/🔎 spinner) | Bot is running the verification step to judge completeness |
| `🔄 Loop iteration K/N` followed by feedback | Verification said incomplete; re-injecting feedback |
| `✅ Loop complete — task verified as done.` | Verification said complete; loop ends |
| `⚠️ Loop limit reached. Remaining issue: ...` | Hit the iteration cap before completion |
| `⚠️ Loop verification failed: ...` | The verify step itself errored; loop aborted |

### Stopping a Loop

- `/stop` — cancels the current iteration **and** the loop. The verifier will not re-inject after stop.
- `/stopall` — same, plus clears any queued messages.
- `/clear` — also clears loop state along with the session.

### How It Works (Brief)

1. Your `<request>` is sent as a normal message.
2. After the response completes, the bot spawns a verifier with a transcript of the conversation and a prompt asking for either `mission_complete` or `mission_pending: <what's left>`.
   - **Claude**: `claude -p --resume <id> --fork-session --max-turns 1 --tools ""` — the live session is forked and no tools are allowed.
   - **Codex**: the verifier reads the archive at `~/.cokacdir/ai_sessions_full/<id>.json`, synthesizes a transcript, and dispatches a fresh `codex exec --ephemeral --sandbox read-only`. No `resume`, no `thread_id`, no rollout file.
   - **OpenCode**: `opencode run --session <id> --fork --agent plan "<prompt>"` — OpenCode native fork. The original session is preserved byte-identical; the fork carries the full conversation history. Reply is read as plain text on stdout, same pattern as Claude. The forked session row persists in the DB (not cleaned up).
3. If the answer is `mission_complete` (and not also `mission_pending`), the loop ends.
4. Otherwise, the remaining work text is sent back as the next user prompt, and the cycle repeats.
5. Once the iteration cap is reached or `/stop` fires, the loop terminates.

### Tips

- `/loop 0` (unlimited) is powerful but has no built-in safety net. Pair it with a clear stopping criterion in the request itself ("until the test command exits 0").
- Each iteration is a real AI turn — token cost scales with iteration count.
- The verification step is also an AI call (single turn, no tools), so each loop iteration costs roughly **1 task turn + 1 verify turn**.

---

## End Hook — Notification When Processing Completes

The end hook is a custom message that the bot sends as a separate Telegram message every time an AI request finishes. Useful as an alert when you walk away from a long-running task and want a ping when it's done.

### /setendhook \<message\>

Sets the end hook message for the current chat.

```
/setendhook ✅ Done
/setendhook @mention please review
```

The text is stored per chat. After every successful completion, the bot will send this exact message right after the AI's response.

### /setendhook

Without arguments, shows the currently configured end hook (or reports that none is set).

### /setendhook_clear

Removes the end hook for the current chat.

### When the End Hook Fires

- After every normal AI response completes
- After shell command execution finishes
- After scheduled tasks complete
- After bot-to-bot messages complete

### When the End Hook Does NOT Fire

- When the request is cancelled with `/stop` or `/stopall`
- When no end hook is configured for the chat

### Tips

- Use a short, distinctive marker (an emoji, a tag) so notifications stand out in your Telegram notification feed.
- The end hook is per chat, so different group chats or DMs can have different markers.
- Combining `/setendhook` with mobile push notifications turns the bot into a long-task pager.
