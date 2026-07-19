# How to Use Group Chat

## Overview

You can invite multiple bots into a Telegram group chat to collaborate on tasks. Each bot operates independently with its own session and working directory, while a shared chat log lets them see what the other bots are doing.

---

## ⚠️ Required: Disable Privacy Mode in BotFather

**Before using any bot in a group chat, you MUST disable its privacy mode in BotFather.** This is a one-time setup step per bot, but it is mandatory — group chats will not work correctly without it.

By default, Telegram bots run in "privacy mode," which means the bot only receives:

- Messages that start with a `/` command
- Messages that directly reply to one of the bot's own messages
- Messages that explicitly `@mention` the bot

With privacy mode enabled, the bot will **not** receive regular group messages — including messages prefixed with `;` (AI prompts) or `!` (shell commands) — so the features described in this guide will silently fail.

### How to disable privacy mode

1. Open Telegram and go to [@BotFather](https://t.me/botfather)
2. Send `/setprivacy`
3. Select the bot you want to configure
4. Choose **Disable**
5. Repeat for **every bot** you plan to use in a group chat

After disabling privacy mode, remove the bot from any existing group chats and re-add it, or the change may not take effect for that chat.

---

## Sending Messages to Bots

In group chats, bots do not listen to every message. You must prefix your message with `;` for bots to receive it:

```
; check the server status
```

### ⚠️ The `;` prefix is a broadcast

When there are **multiple bots** in the same group, the `;` prefix dispatches the **exact same request to every bot in the group simultaneously**. Each bot then independently runs its own AI call, executes its own tool calls, and produces its own reply. In other words, a single `;` message to a group with three bots results in three separate AI sessions working on the same instruction in parallel.

This can cause several problems:

- **Duplicated work** — every bot does the same thing, and you pay for every duplicate turn.
- **Conflicting edits** — if the bots share a working directory, multiple bots may try to modify the same files at the same time, producing inconsistent or overwritten results.
- **Multiplied token costs** — each bot independently consumes tokens for what is essentially the same task, so your bill scales linearly with the number of bots in the group.
- **Noisy responses** — the group fills up with nearly identical replies from every bot.

Because of this, broadcasting with `;` is usually only appropriate in the rare cases where you **deliberately** want every bot to run the same instruction at the same time (for example, asking each bot to print its own working directory so you can compare their states).

## Targeting a Specific Bot — the recommended pattern

In day-to-day use, the **recommended** way to send instructions in a multi-bot group chat is to address one specific bot by name with `@botname <prompt>`:

```
@mybot check the server status
```

Only the mentioned bot will receive and respond to this message; the other bots ignore it entirely. This pattern gives you precise control over which bot handles which task and avoids all of the duplication, conflict, and cost problems that come with `;`.

The same targeting applies to slash commands. For example:

```
/pwd           → all bots respond
@mybot /pwd    → only @mybot responds
```

**Rule of thumb:** when you have multiple bots in a group, default to `@botname <prompt>` for every request and only fall back to `;` when you truly want the instruction to fan out to every bot at once.

## /query — Alternative Message Syntax

You can also use `/query` to send a message to the AI. This works like `;` but supports `@botname` targeting:

```
/query check the server status           → all bots receive
/query@mybot check the server status     → only @mybot receives
```

This is useful when you want the message to be clearly structured as a command.

---

## /public — Controlling Access

By default, only the bot owner can use the bot in group chats. Use `/public` to allow all group members to interact:

```
/public on      → all members can use the bot
/public off     → owner only (default)
/public         → show current setting
```

Only the bot owner can change this setting.

---

## Bots Work Sequentially

Bots in a group chat do not work simultaneously. They process messages one at a time in sequence. When one bot is busy, other messages wait in each bot's queue until it is their turn.

---

## /contextlevel — Controlling Shared Awareness

### What /contextlevel does

In a group chat, each bot only sees its **own** conversation history by default — Telegram does not let one bot read another bot's messages. To solve this, the server maintains a **shared chat log** that records every message handled by every bot in the group (both the user requests they received and the responses they produced).

The `/contextlevel` command controls how many of the most recent entries from that shared log are embedded into the bot's system prompt before each turn. This is the mechanism that lets bots "know" what the other bots in the group have recently said and done.

```
/contextlevel        → show current setting
/contextlevel 20     → include the last 20 log entries
/contextlevel 0      → disable shared context entirely
```

The default is **12** entries. Each bot has its own `/contextlevel` setting, so you can configure them individually using `@botname /contextlevel <n>`.

### Why this matters for token usage

Every log entry included via `/contextlevel` is prepended to the bot's prompt on **every turn**. That means:

- A higher `/contextlevel` value → the bot sees more of what other bots are doing → better coordination, but every turn sends more tokens to the AI provider.
- With multiple bots in the same group all running with a non-zero `/contextlevel`, token usage multiplies: each bot independently pulls the shared log into its own prompt, so the same conversation content is billed once per bot per turn.
- Long, active group chats with several cooperating bots can therefore consume tokens significantly faster than a 1:1 chat with a single bot.

Tune `/contextlevel` based on how much cross-bot awareness you actually need. If the bots rarely need to know what each other are doing, a low value (or `0`) is cheaper and often works just as well.

### When to use /contextlevel 0

Setting `/contextlevel` to `0` disables shared context entirely. The bot will have no visibility into what other bots in the group have said — it behaves as if it were alone in the chat, even though other bots remain present.

**If you want to use only a single bot in a group chat, always run `/contextlevel 0` on that bot.** There is no other bot to coordinate with, so the shared log would only add useless tokens to every prompt. Turning it off removes that overhead completely and keeps each turn as cheap as a plain 1:1 chat.

`/contextlevel 0` is also the right choice when you deliberately want multiple bots to work independently in the same group without influencing each other.

---

## Persistent Memory in a Group

Persistent memory is separate from the recent shared log controlled by `/contextlevel`.

```text
@mybot /usememory
```

`/usememory` is owner-only and toggles memory execution for that specific bot + group-chat setting. It defaults to ON when no explicit value is saved, including for existing settings after upgrade. Public access does not allow ordinary group members to change it. The underlying `~/.cokacdir/memory_store` corpus is shared across every bot and chat running under the same OS account. An owner who does not want group turns retained must run `/usememory` before those turns are handled so the setting becomes OFF.

When ON:

- eligible User requests handled by that bot and their successfully delivered final Assistant answers are stored as immutable plain-text records;
- the bot may search relevant records contributed by any bot or chat in the shared store;
- records survive provider-session changes, `/clear`, working-directory changes, and model switches;
- scheduled tasks, bot-to-bot messages, and proactive Companion pings may read the shared memory but do not create User/Assistant records;
- another bot with `/usememory` enabled can search these records, including from a different chat. Paths and display names are attribution hints rather than proof that a record belongs to the current speaker.

Because several people can contribute to one group scope, a record can include an optional `user_label`. This is only a display-name hint: names can collide or change, so the Agent is instructed not to assign an old preference, identity, or private fact to the current speaker without reliable current context.

### `/contextlevel` versus `/usememory`

| Feature | Scope | Delivery to the Agent | Typical lifetime | Multi-bot sharing |
|---|---|---|---|---|
| `/contextlevel` | Recent group log | Last N entries are inserted into every applicable prompt | Bounded recent context | Designed to expose recent activity from multiple bots |
| `/usememory` | Shared `memory_store` for the OS account; ON/OFF setting remains per bot + chat | Shared root and search rules are provided; records are read only on demand | Indefinite until files are deliberately removed | Yes; all enabled bots and chats can search the corpus |

Turning `/usememory` OFF for that bot + group-chat pair stops its new storage and removes lookup guidance from its later runs, but does not delete existing group records or affect another bot's setting. The store is unencrypted plain text protected by local owner-only filesystem permissions. Because memory defaults to ON, operators of public groups should review this retention behavior and explicitly turn it OFF before handled messages if long-term shared storage is not appropriate.

See [How to Use Persistent Conversation Memory](how-to-use-persistent-memory.md) for exact record contents, search behavior, privacy boundaries, and limitations.

---

## Customizing Co-work Behavior

The guidelines that govern how bots collaborate in group chats can be customized by editing the file:

```
~/.cokacdir/prompt/cowork.md
```

This file is auto-generated with default guidelines on first use. You can edit it directly to change how bots coordinate, avoid duplicate work, communicate with each other, and divide tasks. Changes take effect on the next message processed.
