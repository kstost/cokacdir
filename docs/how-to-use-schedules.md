# How to Use Schedules

## Overview

You can schedule tasks for the bot to execute at a specific time or on a recurring basis. Just describe what you want in natural language — the bot handles the rest.

---

## How to Schedule

Simply tell the bot what you want and when. For example:

```
Check disk usage tomorrow at 9am
Run the backup script in 30 minutes
Check server health every weekday at 9am
Clean logs every Sunday at midnight
```

The bot understands natural language for both one-time and recurring schedules.

---

## Schedule Types

- **One-time**: Runs once at the specified time, then is automatically deleted.
- **Recurring**: Runs repeatedly on a schedule (e.g., every day, every weekday, every 30 minutes).

---

## Managing Schedules

### View Schedules

Ask the bot to show your schedules:

```
Show my schedules
What schedules do I have?
```

### Cancel a Schedule

Ask the bot to remove a schedule:

```
Cancel the disk usage schedule
Remove all schedules
```

When a scheduled task is currently running, you can use `/stop` to cancel its execution.

### Continuing After a Scheduled Run

Scheduled tasks no longer create a separate workspace under `~/.cokacdir/workspace/<schedule_id>/`. The old resume commands below therefore have no schedule workspace to open:

```
/start <schedule_id>
```

The `/<schedule_id>` shortcut has the same limitation.

In the default mode, the scheduled run happens in a cloned or forked provider session, and the reply is streamed back to the chat. Your normal chat session is restored afterward. In inline mode (`COKAC_SCHEDULE_INLINE=1`), the scheduled prompt is sent directly into the chat's current session. In both modes, the bot does not append a "Use /<id> to continue this schedule session" hint.

---

## How Scheduled Tasks Execute

The bot has two execution modes for scheduled tasks. The mode is chosen by the `COKAC_SCHEDULE_INLINE` environment variable (see `how-to-configure-environment-variables.md`).

### Default mode

1. When the scheduled time arrives, the bot uses the provider, model, session id, and working directory captured when the schedule was registered.
2. If a source session id was captured, the provider session is cloned or forked first. Codex, OpenCode, and Agy run from a copied session; Claude uses its native fork-session mode.
3. The schedule prompt is sent to that cloned or forked session in the captured working directory. No separate schedule workspace is created, and no `context_summary` text is injected into the prompt.
4. The result is streamed to the chat as if it were a normal reply.
5. Your current chat session is not affected — it is backed up before the schedule runs and restored after the schedule completes.
6. Recurring schedules clone or fork the captured source session again on every run. One-time schedules are automatically deleted after execution.

### Inline mode (`COKAC_SCHEDULE_INLINE=1`)

Set this when you want scheduled tasks to feel like they were typed into your active conversation rather than running off to the side.

1. When the scheduled time arrives, **no separate workspace is created**.
2. The schedule's prompt is sent into the chat's **current** session (same `session_id`, same working directory), so the task continues whatever conversation is already in progress.
3. The result streams into the chat as normal; the prompt and the reply are appended to the live session history, just as if you had typed them yourself at that moment.
4. The reply does **not** include the `Use /<schedule_id> to continue this schedule session` hint, because the chat itself is already the continuation point — just send your next message to follow up.
5. If the chat has no active session at trigger time, inline mode falls back silently to the default cloned-session path so the schedule still runs.

#### Example user flow (inline mode)

```
You:  /start /home/alice/myproject
You:  Check this code for slow spots
Bot:  (analysis reply)
You:  Run the same check again in 5 minutes from a different angle
Bot:  (schedule registered)
```

Five minutes later, with no further action from you:

```
Bot:  ⏰ Run the same check again in 5 minutes from a different angle
Bot:  (reply that builds on the earlier analysis, since it's the same session)
```

You can then keep typing as if nothing unusual happened — the bot has full context of the schedule's prompt and reply.

#### Things to be aware of

- Recurring cron schedules in inline mode accumulate into the same chat session every run. Use `/clear` if the context gets too long.
- One-time inline schedules cannot be re-entered via `/<schedule_id>` because schedules do not create schedule workspaces — the conversation is already in the chat.
- The flag is global per bot process. To switch modes, edit `~/.cokacdir/.env.json` and restart the bot.

---

## Persistent Memory and Scheduled Runs

If `/usememory` is ON for the destination chat when a scheduled provider run actually starts, that Agent receives the same shared, read-only `memory_store` search guidance as a normal request. It may consult relevant preferences, constraints, or decisions contributed by any bot or chat.

The schedule prompt and its result do **not** create a new persistent User/Assistant record, in either default or inline schedule mode. A scheduled prompt is application-generated execution input rather than a new end-user utterance. Provider session history and persistent conversation memory therefore have different behavior in inline mode: the schedule may be appended to the live provider session, but it is still excluded from the normalized memory store.

The memory setting is evaluated at execution time, not registration time. A recurring schedule can therefore run with memory on one occurrence and off on another if `/usememory` changes between firings.

See [How to Use Persistent Conversation Memory](how-to-use-persistent-memory.md) for the complete read/write policy.

---

## Schedule Storage

Schedules are stored as JSON files in `~/.cokacdir/schedule/`. You can inspect or manually remove these files if needed.
