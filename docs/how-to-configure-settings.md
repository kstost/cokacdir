# How to Configure Settings

## /silent

Configures output verbosity for the current chat. Default: **compact**.

```
/silent
/silent status
/silent compact
/silent final
/silent verbose
```

- **compact** — Tool calls and normal tool results are hidden, while normal AI text/progress remains visible. This is the default and matches the old silent-on behavior.
- **final** — Tool calls, tool results, task notifications, cokacdir tool summaries, and intermediate response content are hidden. The existing animated clock/`Processing` placeholder is shown first, then replaced with the final response.
- **verbose** — Full tool call details are displayed, including commands run, tool summaries, tool results, and tool errors.

Running `/silent` with no argument does not change the setting. It shows the current mode and the available `/silent compact`, `/silent final`, and `/silent verbose` options.

Legacy settings are migrated safely: old `silent=true` maps to `compact`, old `silent=false` maps to `verbose`, and missing settings default to `compact`.

---

## /rich

Configures Telegram Bot API 10.1 Rich Message delivery for eligible final responses. Defaults: delivery **auto**, profile **safe**, RTL **off**, draft streaming **off**.

```
/rich
/rich status
/rich off
/rich auto
/rich on
/rich safe
/rich full
/rich profile safe|full
/rich rtl on|off
/rich draft on|off
```

- **off** — Always use the classic `sendMessage` / split-message / file-attachment path.
- **auto** — Use Rich Messages for eligible final responses when the classic path would otherwise split/attach the message, or when the response contains rich-only Markdown blocks such as tables.
- **on** — Prefer Rich Messages for all eligible final responses.
- **safe** — Text-focused Rich Markdown. Media blocks and unsupported raw HTML are escaped.
- **full** — Full Telegram Rich Markdown/HTML surface. Markdown media blocks, maps, collages, slideshows, anchors, references, date-time entities, custom emoji syntax, official HTML tags, and `sendRichMessageDraft`'s `<tg-thinking>` tag are passed through. `/rich full` also switches delivery to **on**.
- **rtl on|off** — Sets `InputRichMessage.is_rtl`.
- **draft on|off** — Opt-in streaming of `sendRichMessageDraft` previews while a final-only private-chat response is being generated. Drafts are ephemeral and the complete response is still sent normally when generation finishes.

Rich delivery applies to eligible final-response sends, including `final` output mode and final edits of existing rolling placeholders. In safe profile it uses sanitized Telegram Rich Markdown so supported advanced text blocks such as headings, tables, task lists, LaTeX formulas, footnotes, and details sections can render natively while media attachment blocks and unsupported raw HTML are escaped. In full profile it passes Telegram Rich Markdown through verbatim to expose the complete Bot API 10.1 formatting surface. Automatic entity detection is disabled, and the bot falls back to the classic path if Telegram rejects the rich message or the response exceeds conservative Rich Message limits.

When Rich delivery is `auto` or `on`, cokacdir also injects explicit response-format guidance into the AI system prompt. The guidance tells the model that the final answer is the rendered message body, not a source-code example; to produce renderable Telegram Rich Markdown/HTML; to output requested Markdown tables directly; and not to wrap rich-renderable markup in a code block unless the user explicitly asks to see literal Markdown/HTML source.

---

## /debug

Toggles debug logging. Default: **OFF**.

When enabled, detailed logs are printed for Telegram API operations, AI service calls, and the cron scheduler. The preference is stored per bot, but debug logging is process-wide while the bot server is running: if any bot in the same process has debug enabled, shared debug logs remain on.

---

## /usechrome

Toggles the `--chrome` flag for the Claude CLI for the current chat. Default: **OFF** per chat.

- **ON** (`🌐 Chrome mode: ON (--chrome)`): Claude is invoked with `--chrome`, allowing it to drive a real Chrome browser session for tasks that require web interaction.
- **OFF** (`🌐 Chrome mode: OFF`): Claude runs without the flag.

The setting only takes effect when the active model is Claude. Other providers ignore this toggle.

---

## /effort

Sets the effort level for the current chat's active Claude or Codex provider.

```
/effort high
/effort reset
```

- **Claude**: `low`, `medium`, `high`, `xhigh`, `max`
- **Codex**: `minimal`, `low`, `medium`, `high`, `xhigh`
- Without arguments, shows the current provider's value and accepted levels.
- `reset`, `clear`, or `default` removes the override for the current provider.

This setting is stored per chat by cokacdir. The underlying Claude CLI receives it as `--effort <level>` for each session invocation; Codex receives it as `-c model_reasoning_effort=<level>`.

---

## /fast

Toggles Codex Fast mode for the current chat. The setting only applies when the active provider is Codex.

```
/fast
/fast on
/fast off
/fast status
```

- **ON** — Codex receives `-c service_tier="fast"` for each invocation.
- **OFF** — cokacdir removes its per-chat override and Codex uses its default/configured service tier.

---

## /stt_model

Sets the transcriptor speech recognition model for the current chat.

```
/stt_model
/stt_model small
/stt_model large-v3-turbo
/stt_model path:/absolute/model.bin
/stt_model reset
```

- Without arguments, shows the current STT model setting.
- Bare model names are passed to transcriptor as `--model-name` and override an inherited `TRANSCRIPTOR_MODEL` value for that run.
- `path:<model_path>` is passed to transcriptor as `--model`.
- `reset`, `clear`, `default`, or `unset` removes the chat override and lets transcriptor use its environment, saved config, or default model.

If the selected model is not cached yet, transcriptor may download it on first use. Telegram STT progress messages show that download before recognition continues.

STT uses the MIT-licensed `transcriptor` binary and Whisper/whisper.cpp model
artifacts. See [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md) for
copyright, license, model, and audio-consent notices.

---

## /setpollingtime \<ms\>

Sets the API polling interval in milliseconds. This controls how frequently streaming responses and shell command output are updated on screen.

```
/setpollingtime 3000
```

- **Minimum**: 2500ms
- **Recommended**: 3000ms or higher
- Setting it too low may cause Telegram API rate limits.
- Without arguments, shows the current value.

---

## /envvars

Prints every environment variable currently visible to the bot process, sorted alphabetically. **Bot owner only.**

Useful for verifying that `~/.cokacdir/.env.json` loaded correctly, or checking whether a `COKAC_*` override is active.

> ⚠ **Security warning:** `/envvars` exposes **all** environment variables with no redaction — including API keys, tokens, and credentials. Telegram stores message history on its servers, so anything printed by this command is persisted until you delete the messages. Use it only for diagnostics, clear the response afterward, and **always use it in a 1:1 chat**. Group chats are rejected for this command.

See [How to Configure Environment Variables](how-to-configure-environment-variables.md) for the full list of variables cokacdir reads (`COKAC_CLAUDE_PATH`, `COKAC_CODEX_PATH`, `COKAC_AGY_PATH`, `COKAC_OPENCODE_PATH`, `COKAC_FILE_ATTACH_THRESHOLD`, `COKACDIR_DEBUG`) and for the `~/.cokacdir/.env.json` auto-loader.

---

## /help

Displays the full command reference with all available commands and usage examples.
