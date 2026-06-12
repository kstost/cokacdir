# How to Use /start, /session, and /clear

## /start

The `/start` command initializes a working session. It can be used in three ways:

### /start (no argument)

Creates a new workspace directory at `~/.cokacdir/workspace/<random_id>` (8 random alphanumeric characters) and starts a fresh session there. The random ID also serves as a shortcut — you can type `/<id>` later to resume the session.

### /start \<path\>

Starts a session at the specified filesystem path. If the directory does not exist, it will be created. Paths are recognized when they start with any of: `/`, `~`, `~/`, `~\`, `./`, `.\`, `.`, `..`, `../`, `..\`, or a Windows drive letter followed by `:\` or `:/` (e.g., `C:\projects`, `D:/work`).

### /start \<session_id or name\>

Resolves a session by ID (UUID) or name. The bot searches matching external session stores for providers that expose session IDs or names (Claude, Codex, Gemini, OpenCode). If the session was created with a different provider than the currently active one, the bot automatically switches the model to that provider.

Kiro is different: its CLI documentation focuses on directory-based persistence (`kiro-cli chat --resume`) rather than name/UUID lookup. In practice, Kiro sessions are restored by reopening the same working directory, not by `/start <kiro_session_id>`.

## Session Lifecycle

1. After `/start`, the session exists locally but has no session ID yet.
2. When you send your first message, most providers create an actual session and assign a unique UUID. Kiro may remain directory-based without a UUID exposed to cokacdir.
3. The session is saved to `~/.cokacdir/ai_sessions/<session_id>.json` when the provider exposes a session ID. Kiro uses a local snapshot file keyed by provider + path instead.
4. On subsequent messages, the conversation history is maintained within the same session.

## Session Restoration

When you use `/start` to open a directory that already has a previous session, the bot restores that session automatically — including conversation history and, when available, the provider session ID. A preview of recent messages is shown.

```
[Claude] Session restored at `/home/user/project`
Use /a1b2c3d4 to resume this session.

👤 Last user message...
🤖 AI response preview...
```

## Workspace Shortcut

If the session is in a workspace directory (`~/.cokacdir/workspace/<id>`), you can quickly resume it by typing `/<id>` instead of `/start <full_path>`.

## Auto-Restore on Restart

The bot remembers the last active path per chat. If the server restarts, the next message you send will automatically restore the previous session without needing to run `/start` again.

---

## /session

Displays the current session information:

- Session ID (UUID) when the provider exposes one
- Current working directory
- A ready-to-use CLI command to resume the session directly from your terminal

### Example Output

```
Current Claude session ID:
550e8400-e29b-41d4-a716-446655440000

To resume this session from your terminal:
cd "/home/user/project"; claude --resume 550e8400-e29b-41d4-a716-446655440000
```

### Resume Commands by Provider

| Provider | Command |
|----------|---------|
| Claude | `claude --resume <session_id>` |
| Codex | `codex resume <session_id>` |
| Gemini | `gemini --resume <session_id>` |
| Kiro | `cd "<path>"; kiro-cli chat --resume` |
| OpenCode | `opencode -s <session_id>` |

### No Active Session

If `/start` was run but no message has been sent yet (session ID not yet assigned), or if no session exists at all:

```
No active session.
```

---

## Cross-Provider Session Resolution

When you run `/start <session_id>`, the bot searches for the session across installed providers that expose an external session ID store. If the session belongs to a different provider than the currently active one, the bot automatically switches to that provider and loads the session.

Kiro does not currently participate in this UUID lookup flow. Its restore path is directory-based: open the same folder again, or use `/session` and run the suggested `kiro-cli chat --resume` command in that directory.

```
Model switched to Codex.
[Codex] Session restored at `/home/user/project`
```

---

## /clear

Discards the current session and prepares for a fresh start. The working directory is preserved, but the conversation is reset.

### What Happens

1. Any in-flight AI request for this chat is cancelled.
2. All queued messages for this chat are dropped.
3. Active loop / verification state is cleared.
4. The session ID is set to `None`.
5. All conversation history is cleared.
6. Any pending file uploads are cleared.
7. The session file on disk is overwritten with minimal data (path and provider only).
8. The current working directory is **not** changed — you stay in the same path.

### What Does NOT Happen

- The workspace directory and its files are not deleted.
- The previous session is not fully deleted from disk — it is overwritten so that `/start` in the same directory will begin a fresh session rather than restoring the old one.

### After /clear

The next message you send will create a brand new session. Providers that expose IDs will create a new UUID; Kiro starts a fresh directory-based conversation. In all cases, the new session starts with no conversation history. This is equivalent to beginning a fresh chat while staying in the same working directory.

```
Session cleared.
`/home/user/project`
```
