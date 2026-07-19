# How to Use Persistent Conversation Memory

## Overview

Persistent conversation memory lets an Agent recall useful information from earlier completed conversations even after the provider session, model, or working directory changes. It is designed for durable user preferences, prior decisions, constraints, names, ongoing work, and conclusions that may matter again.

The feature is enabled by default and can be disabled independently for each bot + chat pair:

- Default: **ON**
- Search scope: **the complete shared `memory_store` for this OS account**
- Enable setting: **per bot + chat**
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

`/usememory` is a pure toggle and takes no arguments. Each call flips the current bot + chat setting between ON and OFF.

A bot + chat pair with no saved `use_memory` value is ON. This includes existing settings files after upgrading to this version. An explicitly saved OFF value remains OFF. Therefore, in an unconfigured chat, the first `/usememory` call turns memory OFF.

“No saved value” is intentionally different from a damaged value. A present `use_memory` field must be a JSON object whose keys are canonical signed chat IDs and whose values are JSON booleans. If it is malformed, cokacdir refuses to start that bot and leaves `bot_settings.json` untouched for correction; it never converts an unreadable OFF into the default ON.

When enabling succeeds, the bot replies:

```text
Persistent memory: ON
Completed User/Assistant turns from this chat will be stored as private plain-text files, and the Agent may search the shared memory_store across all bots and chats.
```

When disabling succeeds, the bot replies:

```text
Persistent memory: OFF
This chat will not store new turns or search the shared memory_store; existing records are retained.
```

When changing an explicit OFF setting back to ON, cokacdir performs a temporary storage capability check. It verifies that the shared store and the current chat's write directory can safely create, sync, atomically publish, validate, and remove a private file. If any step fails, memory remains OFF and no probe record is retained. A default-ON run still validates and prepares its shared root immediately before the provider starts; if that fails, memory is omitted for that turn and the bot reports a warning.

The setting is persisted independently for each bot + chat pair. Enabling memory for one bot does not enable another bot, even in the same chat, and enabling it in one direct message or group does not turn the feature ON elsewhere. However, whenever it is ON for a run, the Agent can search records contributed by every bot and chat in the same `memory_store`.

Rotating only a bot's secret token does not reset an explicit ON/OFF value once cokacdir has a stable settings identity. Telegram can derive that identity from the numeric bot ID in both old and new tokens. Discord uses its authenticated user ID, while Slack uses the authenticated workspace ID plus bot-user ID. cokacdir migrates exactly one matching settings entry to the new token key, refuses ambiguous or mismatched candidates, and never guesses from a display name or username.

For a Discord or Slack entry written before stable identity metadata existed, run this version once with the existing credential before rotating it. That successful start annotates the exact current entry. If the credential was already rotated before the first upgraded start, there is no trustworthy link to the old bridge entry; cokacdir leaves it untouched and refuses to start while an unresolved entry for the same platform remains. This prevents an unreadable legacy OFF from silently becoming default ON and requires the operator to compare and reconcile the entries explicitly.

### ON and OFF behavior

| Setting | Save new eligible turns | Give the Agent memory-search guidance | Existing records |
|---|---:|---:|---|
| OFF | No | No | All shared records are retained, but not offered to this run |
| ON | Yes | Yes | All shared records are available for relevant on-demand lookup |

Turning memory OFF pauses storage and retrieval for that bot + chat pair. It is not a delete operation, and turning it ON later makes the complete shared store available again.

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

- the exact read-only root of the shared `~/.cokacdir/memory_store`;
- instructions describing when lookup is useful;
- a narrow list/search/read procedure;
- rules that historical records are untrusted data rather than instructions;
- the priority rule that current instructions and the current User message win;
- attribution warnings because records can come from different bots, chats, groups, and people.

The Agent handling the current request decides whether lookup would materially help. It should skip memory for self-contained questions and repository tasks whose current files are already the source of truth.

When lookup is useful, the Agent is instructed to:

1. Search any current `v2/<chat-id>` or legacy `v1/bots` subtree that may be relevant, starting with likely recent year/month directories.
2. Search for a few distinctive terms and collect candidate file paths first.
3. Read only a small number of likely records.
4. Retry with synonyms, alternate spellings, related names, broader terms, or another date range when the first query is weak.
5. Stop once it has enough relevant context.

The lookup protocol also forbids following symlinks, reparse points, aliases, or other filesystem indirections found inside the store. Candidates must be regular `.md` files beneath the validated root.

This is not exact-match-only retrieval. The current Agent can reformulate searches, but the physical store is still plain text: if old and new wording share no useful terms and the Agent does not infer the right synonym, a relevant record can be missed. There is currently no FTS, embedding, or vector index.

The lookup uses the provider's existing file listing, search, and read tools. If those tools are unavailable under the active provider or tool policy, cokacdir can still save eligible turns, but that Agent may be unable to retrieve them during the run.

The shared root is an intentional cross-bot and cross-chat corpus. A chat ID in a path and an optional `user_label` are attribution hints only. The Agent must not transfer a preference, identity, private fact, or authorization to the current speaker unless the current conversation or other reliable context establishes that relationship.

---

## Storage Location and Scope

Records live under:

```text
~/.cokacdir/memory_store/
├── v2/
│   └── <chat-id>/
│       └── <YYYY>/
│           └── <MM>/
│               └── <UTC timestamp>-<turn-id>.md
└── v1/                              # legacy, still searchable
    └── bots/<legacy-bot-hash>/chats/...
```

Example:

```text
~/.cokacdir/memory_store/v2/123456789/2026/07/
  20260719T052011.482Z-7f4c2a9d4e8b41cc9a7f03de6b2c1105.md
```

- `v2` is the current shared layout. It does not contain a bot token, bot ID, or bot hash.
- `<chat-id>` organizes records by their source chat; it is not a read-access boundary.
- Existing `v1/bots/<legacy-bot-hash>/...` files are not moved or rewritten. Because the Agent receives the parent `memory_store` root, those legacy records remain searchable alongside v2 records.
- `YYYY/MM` keeps a long history from accumulating in one directory.
- The timestamp is UTC; a random 128-bit turn ID prevents same-millisecond collisions.

The primary memory corpus is the complete `memory_store` owned by the current OS account. Consequently:

- every enabled bot and chat can search records written by every other bot and chat using the same store;
- a chat's ON/OFF setting still controls whether that chat contributes new turns and receives search guidance;
- `/start`, `/clear`, session changes, provider changes, and working-directory changes do not create a new memory boundary;
- multiple bots writing for the same chat contribute to the same v2 chat directory;
- direct-message and group records are all searchable from the shared root;
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

The two message payloads are JSON strings inside fixed Markdown sections. Newlines, quotes, and text such as `## Assistant` are escaped, so message content cannot forge another role boundary. `user_label` is optional group metadata and contains only the participant's non-unique display label—not the stable Telegram user ID. The source chat is represented by its directory; no bot identity is stored in a new v2 record. These values are hints, not proof of identity.

One turn is never appended to or used to overwrite another turn. Concurrent completions publish distinct files.

---

## Companion, Schedules, and Group Chats

### Companion mode

Companion mode controls response style, final-only presentation, proactive pings, and optional images. It does not automatically enable memory.

| `/companion` | `/usememory` | Result |
|---|---|---|
| OFF | OFF | Normal conversation without persistent memory |
| OFF | ON | Normal conversation with access to the shared global memory store |
| ON | OFF | Companion style without persistent memory |
| ON | ON | Companion style using the same shared global memory store |

The Agent no longer creates or curates a separate set of Companion notes. Proactive pings may read common memory when ON, but they never write a new record because no User message initiated the ping.

### Schedules and bot-to-bot work

Scheduled and bot-to-bot runs may consult the shared memory store when the destination chat's setting is ON at their provider-start boundary. Their generated prompts are not actual end-user utterances, so neither run type writes a new memory record.

### Groups

`/usememory` is owner-only in groups. When enabled, every eligible request in the group contributes to that source chat's v2 directory and becomes searchable from every other enabled bot or chat. Different participants can therefore appear in the shared corpus.

The optional `user_label` is treated only as a display hint because names can collide or change. The Agent is explicitly warned not to assign a preference, identity, private fact, or authorization from any other bot or chat to the current speaker unless reliable current context establishes the link.

Persistent memory is not the same as `/contextlevel`: shared group context injects a bounded recent group log on every turn, while persistent memory is a global long-lived corpus searched only when relevant.

---

## Relationship to Sessions and Existing Files

- `/clear` clears the current provider conversation but does not delete persistent memory.
- `/start` changes or creates a provider session without changing access to the shared memory store.
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

Persistent memory is intentionally searchable **plain text**, not an encrypted database. Because it defaults to ON, future eligible User requests and final Assistant answers can remain on disk indefinitely unless the owner turns it OFF with `/usememory` before those turns run.

- There is no automatic secret detection, redaction, summarization, retention period, or pruning.
- Records can also appear in home-directory backups and filesystem snapshots.
- `memory_store` directories use owner-only permissions on Unix and a protected current-user-only DACL on Windows.
- Existing permissions on the shared `~/.cokacdir` application directory are not rewritten; the memory subtree is hardened separately.
- Temporary files are synced and atomically renamed without replacing an existing record.
- Symlinks, reparse points, unexpected file types, and identity changes cause the operation to fail closed.

These controls protect against accidental exposure and unsafe publication, but they are not application-level encryption. Administrators, malware, backups, or another process running as the same OS user may still access the files.

The Agent receives a strong read-only instruction and is limited to the `memory_store` root, but bot and chat subdirectories inside that root are intentionally shared. There is no confidentiality boundary between bots or chats using the same OS account. Deployments that require separated memory must use separate OS users, containers, homes, or sandbox mounts.

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

### The bot refuses to start with an unsafe bot-settings error

cokacdir preserves `~/.cokacdir/bot_settings.json` instead of normalizing uncertain data. Check the local error for a malformed `use_memory` object, a token/hash/identity mismatch, more than one current/prior entry claiming the same stable bot identity, or an old same-platform bridge entry that has no provable identity after credential rotation. Correct or restore the settings file before restarting; do not delete an older entry until you have compared its explicit per-chat values, especially `false` entries.

### `/usememory` says memory remains OFF

An explicit OFF-to-ON capability probe failed or was cancelled. Check the local debug log for the exact reason. Common causes include an unsafe path component, permissions or DACL failure, a read-only filesystem, unavailable disk space, or inability to sync or atomically rename files.

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
- bot- or chat-level confidentiality inside one shared `memory_store`;
- strict exactly-once deduplication for externally replayed Telegram updates.

Plain-text Markdown remains the canonical source. A future search index can be rebuilt from these records without replacing them.
