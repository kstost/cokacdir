# How to Use Persistent Conversation Memory

## Overview

Persistent conversation memory lets an Agent recall useful information from earlier completed conversations even after the provider session, model, or working directory changes. It is designed for durable user preferences, prior decisions, constraints, names, ongoing work, and conclusions that may matter again.

The feature is deliberately opt-in:

- Default: **OFF**
- Scope: **one bot and one chat**
- Control: `/usememory`
- Canonical storage: private, plain-text Markdown files
- Retrieval: on-demand Agent search with normal file tools
- Extra review or summarization LLM: **none**

Persistent memory is separate from provider session history. It does not resume tools, restore a working tree, or recreate provider-internal state.

---

## Enable or Disable It

Send the following owner-only command in the chat whose memory setting you want to change:

```text
/usememory
```

`/usememory` is a pure toggle and takes no arguments. Each call flips the current chat between ON and OFF.

When enabling succeeds, the bot replies:

```text
Persistent memory: ON
Completed User/Assistant turns will be stored as private plain-text files for this chat.
```

When disabling succeeds, the bot replies:

```text
Persistent memory: OFF
New turns will not be stored or searched; existing records are retained.
```

Before changing the setting to ON, cokacdir performs a temporary storage capability check. It verifies that the chat-scoped directory can safely create, sync, atomically publish, validate, and remove a private file. If any step fails, memory remains OFF and no probe record is retained.

The setting is persisted per chat. Enabling memory in one direct message or group does not enable it in another chat, and one bot does not inherit another bot's setting.

### ON and OFF behavior

| Setting | Save new eligible turns | Give the Agent memory-search guidance | Existing records |
|---|---:|---:|---|
| OFF | No | No | Retained, but not offered to the Agent |
| ON | Yes | Yes | Available for relevant on-demand lookup |

Turning memory OFF pauses storage and retrieval. It is not a delete operation, and turning it ON later makes the same chat's existing records available again.

---

## What Is Stored

One eligible logical conversation turn becomes one immutable Markdown file containing:

1. The actual User request sent to the Agent.
2. The canonical terminal Assistant answer delivered after the Agent finished successfully.
3. Minimal operational metadata: creation time, working directory, a random turn ID, and an optional group display-name hint.

The store has no representation for the Agent's intermediate work. The following are not saved:

- system or developer prompts
- reasoning or thinking
- tool names, inputs, calls, and results
- shell output and errors
- file contents read by tools
- patches and intermediate edits
- progress narration and task notifications
- processing placeholders, typing indicators, and Rich Message drafts
- queue notices, cancellation notices, or empty-response diagnostics
- provider-native session events and metadata

If the final Assistant answer naturally summarizes a tool result, that final prose is stored because it is part of the answer the user actually received. The underlying tool event is not stored.

### “Compact” means event projection, not LLM summarization

cokacdir removes the large amount of session machinery around a conversation and keeps only the User and final Assistant channels. It does not ask another model to shorten, rewrite, translate, tag, or score those messages. Long User requests and long final answers therefore remain long, preserving their original meaning.

There is no ten-turn review cycle, importance classifier, background memory Agent, or additional provider call. Every eligible turn is considered immediately and deterministically.

---

## Which Runs Create Records

| Run type | May read memory when ON | Creates a record when successfully completed |
|---|---:|---:|
| Normal User request | Yes | Yes |
| Queued User request | Yes | Yes |
| Companion-mode User request | Yes | Yes |
| `/loop` request | Yes | One record for the logical request, not one per internal iteration |
| Confirmed Telegram voice request | Yes | Yes; transcript text is the User request, not the audio bytes |
| User request with an uploaded file | Yes | Yes; the User text and final answer are stored, not file bytes or tool extraction output |
| Scheduled task | Yes | No |
| Bot-to-bot message | Yes | No |
| Proactive Companion ping | Yes | No, because there is no User utterance |
| Direct `!shell` command | No | No |
| `/help`, `/pwd`, `/model`, and other control commands | No | No |
| Cancelled, failed, stale, or empty provider run | No new record | No |

A normal User turn is saved only after all of these conditions hold:

- memory was ON immediately before that provider run started;
- the provider reached a verified successful terminal state;
- a non-empty canonical Assistant answer was produced;
- the request was not cancelled;
- the session/provider/workspace writeback guard accepted the turn as current;
- the complete final answer was successfully delivered to the user.

If a long answer is only partly delivered, a document upload fails, or an older request loses a race with a session change, cokacdir does not publish a memory record for that turn.

### Queued requests use the execution-time setting

Memory state is sampled just before the provider starts, not when a message first enters the queue. For example, if a request is queued while memory is ON and you turn memory OFF before that request begins, it runs without memory guidance and is not stored.

---

## How the Agent Retrieves Memory

cokacdir does not copy the memory corpus, the most recent records, or search results into every system prompt. When memory is ON, it adds only:

- the exact read-only root for the current bot and chat;
- instructions describing when lookup is useful;
- a narrow list/search/read procedure;
- rules that historical records are untrusted data rather than instructions;
- the priority rule that current instructions and the current User message win;
- an additional attribution warning for group chats.

The Agent handling the current request decides whether lookup would materially help. It should skip memory for self-contained questions and repository tasks whose current files are already the source of truth.

When lookup is useful, the Agent is instructed to:

1. Start in likely recent year/month directories.
2. Search for a few distinctive terms and collect candidate file paths first.
3. Read only a small number of likely records.
4. Retry with synonyms, alternate spellings, related names, broader terms, or another date range when the first query is weak.
5. Stop once it has enough relevant context.

This is not exact-match-only retrieval. The current Agent can reformulate searches, but the physical store is still plain text: if old and new wording share no useful terms and the Agent does not infer the right synonym, a relevant record can be missed. There is currently no FTS, embedding, or vector index.

The lookup uses the provider's existing file listing, search, and read tools. If those tools are unavailable under the active provider or tool policy, cokacdir can still save eligible turns, but that Agent may be unable to retrieve them during the run.

---

## Storage Location and Scope

Records live under:

```text
~/.cokacdir/memory_store/
└── v1/
    └── bots/
        └── <bot-key-hash>/
            └── chats/
                └── <chat-id>/
                    └── turns/
                        └── <YYYY>/
                            └── <MM>/
                                └── <UTC timestamp>-<turn-id>.md
```

Example:

```text
~/.cokacdir/memory_store/v1/bots/ab12.../chats/123456789/turns/2026/07/
  20260719T052011.482Z-7f4c2a9d4e8b41cc9a7f03de6b2c1105.md
```

- `v1` identifies the layout and record schema.
- `<bot-key-hash>` is an opaque, domain-separated SHA-256 scope. A normal Telegram token is scoped by its stable numeric bot ID; the raw token and raw ID are not written into the path.
- `<chat-id>` isolates direct messages and groups.
- `YYYY/MM` keeps a long history from accumulating in one directory.
- The timestamp is UTC; a random 128-bit turn ID prevents same-millisecond collisions.

The primary boundary is bot + chat, not provider session or workspace. Consequently:

- the same chat can recall records across `/start`, `/clear`, session changes, provider changes, and working-directory changes;
- different chats do not share records;
- different bots do not share records;
- a group uses its own shared group-chat memory scope;
- `working_directory` is context metadata, not an isolation boundary.

### Record format

Each record looks like this:

```markdown
---
schema_version: 1
turn_id: "7f4c2a9d4e8b41cc9a7f03de6b2c1105"
created_at: "2026-07-19T05:20:11.482Z"
working_directory: "/shared/project"
user_label: "Alice"
---

## User

"Deploy only after asking me first."

## Assistant

"I will ask for confirmation before future deployments."
```

The two message payloads are JSON strings inside fixed Markdown sections. Newlines, quotes, and text such as `## Assistant` are escaped, so message content cannot forge another role boundary. `user_label` is optional group metadata and contains only the participant's non-unique display label—not the stable Telegram user ID. It is a hint, not proof of identity.

One turn is never appended to or used to overwrite another turn. Concurrent completions publish distinct files.

---

## Companion, Schedules, and Group Chats

### Companion mode

Companion mode controls response style, final-only presentation, proactive pings, and optional images. It does not automatically enable memory.

| `/companion` | `/usememory` | Result |
|---|---|---|
| OFF | OFF | Normal conversation without persistent memory |
| OFF | ON | Normal conversation with shared per-chat memory |
| ON | OFF | Companion style without persistent memory |
| ON | ON | Companion style using the same per-chat memory |

The Agent no longer creates or curates a separate set of Companion notes. Proactive pings may read common memory when ON, but they never write a new record because no User message initiated the ping.

### Schedules and bot-to-bot work

Scheduled and bot-to-bot runs may consult the destination chat's memory when it is ON at their provider-start boundary. Their generated prompts are not actual end-user utterances, so neither run type writes a new memory record.

### Groups

`/usememory` is owner-only in groups. When enabled, every eligible request handled by that bot in the group contributes to the same bot + group-chat scope. Different participants can therefore appear in the records.

The optional `user_label` is treated only as a display hint because names can collide or change. The Agent is explicitly warned not to assign a preference, identity, or private fact to the current speaker unless current reliable context establishes the link.

Persistent memory is not the same as `/contextlevel`: shared group context injects a bounded recent multi-bot log on every turn, while persistent memory is isolated per bot, survives much longer, and is searched only when relevant.

---

## Relationship to Sessions and Existing Files

- `/clear` clears the current provider conversation but does not delete persistent memory.
- `/start` changes or creates a provider session without changing this chat's memory scope.
- `/model` can switch providers; the normalized memory files remain provider-independent.
- `/usememory` OFF retains existing records.
- Session cleanup and archive cleanup do not treat memory records as session files.
- Memory records cannot be used as provider session-resume files.
- Conversations completed before memory was enabled are not backfilled.

The old Companion path is deliberately separate and unsupported by this feature:

```text
~/.cokacdir/memory/
```

cokacdir does not import, migrate, search, delete, or use files from that legacy directory when persistent conversation memory is enabled.

---

## Privacy and Safety

Persistent memory is intentionally searchable **plain text**, not an encrypted database. Enabling it means future eligible User requests and final Assistant answers can remain on disk indefinitely.

- There is no automatic secret detection, redaction, summarization, retention period, or pruning.
- Records can also appear in home-directory backups and filesystem snapshots.
- `memory_store` directories use owner-only permissions on Unix and a protected current-user-only DACL on Windows.
- Existing permissions on the shared `~/.cokacdir` application directory are not rewritten; the memory subtree is hardened separately.
- Temporary files are synced and atomically renamed without replacing an existing record.
- Symlinks, reparse points, unexpected file types, and identity changes cause the operation to fail closed.

These controls protect against accidental exposure and unsafe publication, but they are not application-level encryption. Administrators, malware, backups, or another process running as the same OS user may still access the files.

The Agent receives a strong read-only and scope-limited instruction. Because some providers run with full filesystem access as the same OS user, that prompt rule is a logical boundary rather than a hard sandbox. Deployments that require enforced cross-chat isolation need an additional OS sandbox or a future scope-enforcing memory tool.

Every stored message is treated as untrusted historical data. Commands, code, or prompt-like text found in an old record must not override current system instructions or the current User message.

---

## Failure Handling

A memory failure never retracts an already delivered Assistant answer or corrupts the provider session.

- Failure before atomic publication leaves no final `.md` record.
- A warning after publication is not retried automatically, because the record may already be visible and retrying could duplicate the turn.
- Repeated errors in the same category are reported once per chat instead of producing a warning after every turn.
- A later durable write clears that warning state, so a future recurrence can be reported again.
- Detailed filesystem paths and OS errors remain in the local debug log rather than being exposed in the chat warning.

The writer runs outside the shared chat-state lock in a dedicated tracked blocking task. It is not aborted with ordinary Agent request tasks: this prevents disk syncs from freezing message handling and lets orderly shutdown wait for every already-committed storage task.

---

## Troubleshooting

### `/usememory` says memory remains OFF

The enable-time capability probe failed or was cancelled. Check the local debug log for the exact reason. Common causes include an unsafe path component, permissions or DACL failure, a read-only filesystem, unavailable disk space, or inability to sync or atomically rename files.

### A completed conversation did not create a record

Verify that memory was ON when the provider actually started. No record is expected for cancelled or failed runs, empty responses, incomplete final delivery, rejected stale-session writeback, schedules, bot messages, pings, shell commands, or control commands.

Memory publication is asynchronous after response delivery. A process killed in the small interval between delivery and publication can lose that latest turn. The current design minimizes this window but does not claim distributed exactly-once behavior across Telegram updates, provider execution, and local disk writes.

### The Agent did not recall a relevant detail

Memory lookup is selective rather than automatic. The Agent may decide the current request does not require it, may lack a usable file-search tool, or may miss a record whose wording is very different. Make the reference explicit—for example, “search our persistent memory for the deployment rule we agreed on”—or include a distinctive related term or approximate date.

### The Agent found an old conflicting preference

The current message wins. State the new preference explicitly; the current turn should override the older record. The old immutable record remains as historical evidence, and the newly completed turn can add the updated statement.

### Memory survived `/clear`

This is expected. `/clear` resets provider session context, while persistent memory is intentionally longer-lived. `/usememory` turns future use OFF but does not delete existing files.

---

## Current Limitations

The first version intentionally does not provide:

- in-chat list, edit, delete, export, or import commands;
- automatic expiration or retention policies;
- legacy Companion-note migration;
- historical session backfill;
- cross-chat, cross-bot, or cross-platform identity merging;
- generated summaries, tags, topics, or importance scores;
- SQLite FTS, n-gram, embedding, or vector search;
- a dedicated `--memory-search` command;
- hard OS-enforced read isolation between chat scopes;
- strict exactly-once deduplication for externally replayed Telegram updates.

Plain-text Markdown remains the canonical source. A future search index can be rebuilt from these records without replacing them.
