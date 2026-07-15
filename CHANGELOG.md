# Changelog — cokacdir

## Unreleased

- **Cross-volume cut/paste now uses a fast `Standard` verification policy by default.** Standard mode skips SHA-256 content hashing while retaining private staging, identity and metadata checks, destination durability syncs, atomic publication, rollback, and source deletion only after commit. Users can opt into the previous content-hashing behavior with `Cross-volume move: Strict` in the Settings dialog.

- **File-operation progress now distinguishes transfer completion from operation completion.** The dialog shows syncing, strict verification, and finalization phases after bytes reach the destination, and the overall bar remains below 100% until the worker reports completion.

---

## 0.6.41 — 2026-06-28

- **Go to Path autocomplete now prioritizes what the user actually typed.** Path suggestions are ranked as exact, case-insensitive exact, prefix, substring, then subsequence matches, so inputs such as `/Users/kst/de` surface `Desktop/`, `develop/`, and `devnoda/` ahead of unrelated fuzzy matches, and `/V` surfaces `/Volumes/` before lower-quality root entries. Hidden entries are still available, but they are pushed behind visible entries unless the prefix itself starts with `.`.

- **Tab completion now uses high-confidence prefix matches before fuzzy matches.** When exact/prefix candidates exist, common-prefix expansion and single-candidate completion operate on that stronger group first, preventing fuzzy/subsequence matches from blocking obvious completions such as `/V` → `/Volumes/`.

---

## 0.6.40 — 2026-06-25

- **`/silent final` no longer drops the final answer after completed Codex todo updates.** Codex can emit the terminal assistant text, then a completed `todo_list` task notification, then `turn.completed` with an empty result. Final-only mode now preserves the assistant-answer candidate when the task notification is already complete or every todo line is checked, while still clearing interim text for in-progress task updates. This fixes the “processing placeholder disappears with no response” failure seen when all work completed successfully but the final answer was cleared just before rendering.

- **Final-only mode preserves terminal answers across `cokacdir --sendfile` delivery events.** Codex image/file auto-delivery and model-issued sendfile commands are represented as `Bash` ToolUse/ToolResult events after the answer may already have been produced. `/silent final` now detects `cokacdir --sendfile` specifically and keeps the final answer candidate through that internal delivery pair, without changing the reset behavior for normal tools or other cokacdir commands.

- **Scheduled session registration now resolves the provider from the source session id.** `--cron ... --session <SID>` validates that the session can actually be found and adjusts the stored provider to the resolved session provider instead of blindly trusting the chat's current model setting. If the session cannot be resolved, registration fails early with a JSON error.

- **Agy scheduled-session cloning now uses SQLite online backup for `.db` conversations.** Cloning removes stale target sidecars and backs up the source database through `rusqlite` instead of copying `.db/.db-wal/.db-shm` files directly, reducing the risk of inconsistent Agy conversation clones.

---

## 0.6.38 — 2026-06-24

- **`/silent final` now shows the existing animated processing placeholder before the terminal response.** Final mode still hides tool calls, tool results, task notifications, and intermediate AI text, but normal chat, scheduled tasks, and bot-to-bot processing now show the clock/typing `Processing` animation and replace that message with the final response when the run completes. Final mode now renders the final assistant answer candidate after the latest tool/tool-result/task boundary instead of dumping every streamed assistant text chunk accumulated during the run, cancelled final-only runs no longer reveal partial accumulated text, and Codex todo-list updates are treated as task notifications so they cannot leak into the final-only response.

---

## 0.6.35 — 2026-06-24

- **Telegram Rich Message settings now shape the AI system prompt automatically.** When `/rich auto` or `/rich on` is active, cokacdir injects explicit response-format rules that tell the model to treat the final answer as the rendered Telegram message body, prefer Telegram Rich Markdown, output requested Markdown tables directly, and avoid wrapping Rich Markdown/HTML or tables in code fences unless the user explicitly asks to see literal source. The injected guidance also reflects the active `safe`/`full` profile and RTL setting, while `/rich off` tells the model not to rely on Rich-only features.

- **`/rich auto` now recognizes short rich-only structures before falling back to the classic path.** Auto mode still keeps short plain text on the classic `sendMessage` route, but it now tries Rich delivery for Markdown tables and Rich HTML-style blocks such as table tags and `<tg-*>` tags. Code-fenced Markdown/HTML remains literal source and is not promoted to Rich rendering. Regression tests cover Markdown tables, Rich HTML blocks, prompt-guidance insertion, and the existing sanitizer/fallback behavior.

---

## 0.6.34 — 2026-06-24

- **Telegram Rich Messages now expose the full Bot API 10.1 formatting surface.** `/rich safe|full` separates the safe text-focused default from a full passthrough profile. `/rich full` enables media blocks, maps, collages, slideshows, anchors, references, date-time entities, custom emoji syntax, official Rich HTML tags, arbitrary Rich Markdown HTML, and the draft-only `<tg-thinking>` tag. `/rich rtl on|off` sets `InputRichMessage.is_rtl`, and `/rich draft on|off` enables opt-in `sendRichMessageDraft` previews for final-only private chats. Settings are persisted and documented in the new `docs/telegram-rich-message-reference.md` reference document.

---

## 0.6.33 — 2026-06-24

- **Telegram final responses can now use Bot API 10.1 Rich Messages.** The `/rich off|auto|on` setting controls per-chat Rich Message delivery. The default `auto` mode keeps short responses on the classic path and uses raw `sendRichMessage` / `editMessageText.rich_message` for eligible final responses when it avoids Telegram splitting or file attachment; `/rich on` prefers Rich Messages for all eligible final responses, while `/rich off` restores the classic `sendMessage` / split-message / file path. Rich delivery sends sanitized Telegram Rich Markdown so headings, tables, task lists, LaTeX formulas, footnotes, and details blocks can render natively. Safe rendering passes `skip_entity_detection=true`, honors raw API `retry_after` values, redacts bot tokens from raw reqwest errors, and falls back to the classic path on every API/client/format failure.

- **Bare `/silent` now reports status instead of cycling modes.** Running `/silent` with no argument shows the current mode and available `/silent compact`, `/silent final`, and `/silent verbose` options without changing settings. Explicit commands continue to set the mode, and `/silent status` / `/silent show` remain read-only aliases.

---

## 0.6.32 — 2026-06-24

- **Telegram `/silent` is now a three-level output mode instead of a boolean toggle.** The default mode is `compact`, bare `/silent` cycles `compact → final → verbose → compact`, and explicit commands such as `/silent status`, `/silent compact`, `/silent final`, and `/silent verbose` are supported. Existing settings migrate safely: legacy `silent=true` maps to `compact`, legacy `silent=false` maps to `verbose`, and the new `final` mode still writes `silent=true` for rollback compatibility with older binaries.

- **The new `final` output mode suppresses intermediate Telegram noise across normal chat, scheduled tasks, and bot-to-bot message processing.** Tool calls, tool results, task notifications, `cokacdir` tool summaries, placeholders, and progress edits are hidden; the chat receives only the terminal response. Long final responses still use the existing file-attachment path when eligible, while final-only bot-to-bot message handling deletes its queue file once execution is committed to avoid duplicate side effects after a restart.

- **Final-mode file attachment decisions are based on the terminal response, not hidden intermediate output.** The final-only send path measures the normalized response that would actually be shown or attached, so suppressed tool calls/results, progress text, placeholders, and task notifications do not push an otherwise short final answer into file mode. The threshold remains byte-length based (`COKAC_FILE_ATTACH_THRESHOLD`, default `8192` bytes), preserving the existing `opencode` no-file-attachment exception.

---

## 0.6.31 — 2026-06-24

- **Cokacdir CLI result rendering now uses explicit cron result kinds instead of inferring destructive actions from shared JSON fields.** `--cron-register`, `--cron-list`, `--cron-remove`, `--cron-history`, and `--cron-update` success JSON now include a `kind` field, and the Telegram result formatter gives `cron_history` / `cron_remove` explicit priority before falling back to legacy shape handling. This prevents `--cron-history` responses such as `{"id":"CD52CBA0","count":...,"history":[...]}` from being shown as `✅ Removed` merely because they contain an `id`.

- **Schedule removal preserves run history for later inspection.** Manual `--cron-remove` now deletes the live schedule entry without deleting `/home/kst/.cokacdir/schedule_history/<ID>.log`, so follow-up questions can still inspect what happened. New schedule ID generation treats retained history files as reserved, preventing a future schedule from reusing an ID with prior history.

- **Schedule-result formatting is safer for unknown `id`-bearing JSON.** The legacy removal fallback is now restricted to the old minimal `{"status":"ok","id":"..."}` shape, while unknown successful JSON carrying an `id` plus extra fields is left raw instead of being labeled as a deletion. Regression tests cover explicit `cron_history`, explicit `cron_remove`, legacy history, legacy remove, id-only, and unknown-extra-field outputs.

- **Encryption split chunk counting avoids overflow in no-split mode.** The packer now computes non-empty chunk counts as `(size - 1) / split_size + 1`, avoiding `size + split_size - 1` overflow when the effective split size is `u64::MAX`.

---

## 0.6.29 — 2026-06-23

- **OpenCode scheduled-session polling now treats cloned unfinished todos as a counted baseline.** Cloned sessions can legitimately start with unfinished todos from the source session, so the serve adapter ignores those unchanged baseline todos while still waiting for new, duplicated, or modified unfinished todos created by the current turn.

- **Scheduled tasks now clone or fork the source provider session at execution time instead of relying on `context_summary`.** `--cron` / `--at` registration now persists the prompt, schedule, captured working directory, provider, model, and source `session_id`, then exits immediately. It no longer starts the detached `--cron-context` summarizer, and a successful register response now means "the schedule metadata was saved", not "a summary or execution session was prepared".

- **The default non-inline schedule path preserves the original chat session by running against a copied provider session.** Codex schedules clone the Codex rollout plus `state_5.sqlite` thread row, OpenCode schedules clone the relevant SQLite `session` / `message` / `part` rows with remapped ids, and Agy schedules copy the Antigravity conversation file plus SQLite sidecars. Claude schedules use Claude's native `--fork-session`. The saved prompt is sent to that clone/fork, so the source provider session is not directly resumed or mutated by the scheduled run.

- **Recurring cron schedules now start from the same original source session on every firing.** Repeated runs no longer carry forward a generated summary or the previous run's AI transcript. Each execution clones/forks the captured source session again, updates only `last_run` for recurring entries, and leaves cross-run state sharing to explicit files, databases, or external systems.

- **Default scheduled runs no longer create `~/.cokacdir/workspace/<schedule_id>` workspaces or continuation hints.** Non-inline schedules execute in the `current_path` captured at registration time, restore the visible chat session afterward, and no longer append `Use /<schedule_id> to continue this schedule session.`. Schedule history keeps the historical `workspace_path` JSON key for compatibility, but the value now represents the execution working directory.

- **Legacy schedule summary plumbing was removed from active provider paths.** Claude, Codex, and OpenCode context/result summary helpers were removed, Agy remains summary-free, and `--cron-context` now returns an explicit unsupported error instead of doing work. Old schedule JSON files may still contain `context_summary`; cokacdir reads that field only for compatibility, ignores it during execution, and drops it on the next legitimate schedule write.

- **OpenCode schedule cloning now follows the same DB discovery shape as `cokacmux`.** The OpenCode adapter looks for `opencode.db` under `LOCALAPPDATA`, `APPDATA`, and the Linux `~/.local/share/opencode/opencode.db` path, using the first existing candidate before falling back to the first configured candidate for clearer errors.

- **Schedule documentation now matches the cloned-session model.** `docs/how-to-use-schedules.md` and `docs/how-to-configure-environment-variables.md` describe default cloned-session execution versus `COKAC_SCHEDULE_INLINE=1`, and the new `devdoc/schedule-session-clone-goal.md` records the detailed design goal, non-goals, provider-specific strategy, compatibility rules, and regression risks for this change.

---

## 0.6.27 — 2026-06-20

- **Agy execution now follows a simpler direct stdout streaming path.** The provider still invokes Antigravity CLI with `agy --print "" --print-timeout <duration> --log-file <temp-log> --dangerously-skip-permissions`, writes the composed system/user prompt through stdin, validates `--conversation <session_id>` before spawning, and validates explicit `agy:<model>` values against `agy models`. The adapter now streams every stdout line directly to the chat instead of maintaining a replay-suppression cache, so resumed Antigravity output is no longer hidden by cokacdir-side prefix matching. A successful run that produces empty stdout is reported as `Agy exited successfully but produced no stdout response.`, while non-zero exits still surface as provider errors with captured stdout/stderr.

- **Agy session-id recovery is more tolerant of platform path formats.** `read_last_conversation_id` now looks up Antigravity's `~/.gemini/antigravity-cli/cache/last_conversations.json` with the original working directory, the canonicalized path, and Windows slash/backslash variants. This fixes cases where a Windows workspace was stored as `C:\...` but cokacdir later looked it up as `C:/...`, or vice versa.

- **Windows Agy binary resolution now prefers the native executable.** `COKAC_AGY_PATH` is still honored when it points to a runnable binary, but Windows auto-discovery now tries `agy.exe` before `agy.cmd`, maps a discovered `.cmd` wrapper to a sibling `.exe` when present, and falls back to `where.exe agy`. The debug log records which path won and explicitly logs ignored non-runnable overrides, making startup/provider diagnostics clearer.

- **Bot-server startup avoids an eager Agy model probe.** The `--ccserver` provider banner now prints the detected Agy version without also running `agy models` to count available models. `/model` still lists Agy models on demand, but starting a mixed Telegram/Discord/Slack server no longer pays that extra provider call up front.

- **Repository hygiene cleanup.** Local CLI/runtime artifacts that had slipped into the tree were removed (`.antigravitycli/...` and `.claude/scheduled_tasks.lock`), leaving the repository focused on source, docs, and built web assets.

---

## 0.6.21 — 2026-06-17

- **Agy / Google Antigravity CLI is now a first-class provider, replacing the old Gemini service path.** The new `src/services/agy.rs` adapter discovers `agy` via `COKAC_AGY_PATH`, `which`, shell PATH fallback, or Windows path lookup; caches `agy --version` and `agy models`; validates requested model labels before spawn; and runs Agy in measured print mode with an explicit empty `--print ""` argument. `gemini` and `gemini:<model>` settings are retained only as compatibility aliases and route through the Agy provider.

- **Agy is wired through the chat-bot execution surface.** `/model agy` and `/model agy:<model>` are accepted, `/model` shows installed Agy models, provider detection can fall back to Agy when Claude is unavailable, and normal chat messages, scheduled tasks, inline schedules, and bot-to-bot messages can all dispatch through `agy::execute_command_streaming`. The provider composes the chat system prompt into the stdin prompt because Agy print mode does not expose the same structured system-prompt channel as Claude/Codex.

- **Agy session discovery and restore are integrated with `/start`, `/session`, and cross-provider lookup.** cokacdir resolves Antigravity conversations from `~/.gemini/antigravity-cli/conversations/<session_id>.db|.pb`, reads `last_conversations.json` when possible to map a conversation back to its working directory, and falls back to scanning the conversation file for a cwd candidate. Restored Agy sessions are stored as minimal cokacdir session records while the full transcript remains owned by Antigravity CLI.

- **Agy limitations are explicit instead of silently pretending to match other providers.** `/loop` rejects Agy because no isolated no-tools verifier mode has been measured for Antigravity CLI, and the `/availabletools`, `/allowedtools`, and `/allowed` tool-management commands apply only to Claude. The new docs call out that Agy print mode emits plain stdout rather than structured JSON/tool events, so cokacdir streams text output instead of rendering per-tool cards.

- **The docs and website now document the Agy provider contract.** `docs/how-to-use-agy-antigravity.md` records the measured `agy 1.0.8` invocation contract, session storage locations, stdout/stderr behavior, known failure shapes, model handling, and current provider limitations. The website adds a matching Agy Provider section and updates environment-variable, session-management, request-management, and tool-management pages from Gemini terminology to Agy terminology.

- **A broad codebase audit fixed confirmed crash, data-loss, and security bugs.** `AUDIT_2026-06-11.md` records the audit scope and the fixes. Highlights include char-index corrections for editor, AI input, and remote-connect fields; terminal-size guards for help/search/git/dialog rendering; `diff_first_panel` index repair across panel close/add; safe handling for huge diff files and symlink-directory recursion; and UTF-8-safe token prefix masking in `--ccserver` diagnostics.

- **Configuration, encryption, and file-operation safety were tightened.** A damaged `~/.cokacdir/settings.json` is preserved instead of being overwritten by defaults, with a backup path for later recovery. `.cokacenc` unpack now rejects metadata/header version downgrade attempts that could bypass v3 integrity checks, split-size multiplication uses saturating arithmetic, copy/move cleanup no longer deletes a destination that an external process created after an `AlreadyExists` race, and duplicate-name probing uses `symlink_metadata` so broken symlinks are treated consistently.

- **Remote transfer handling is safer across rsync/SSH variants.** rsync 3.2.4+ is detected so modern arg-protection paths do not get wrapped in literal quotes, openrsync output is no longer misclassified as rsync 29.x, password transfer moved from `sshpass -p <password>` argv exposure to `sshpass -e` / `SSHPASS`, and known-host key-change errors now include a more actionable line-removal hint.

- **Session/archive parsing was expanded for the newer provider mix.** The session archive code now has an Agy metadata parser, keeps richer structured handling for provider transcripts, and preserves OpenCode/Codex/Claude session details without forcing Agy into a fake JSON-event model.

---

## 0.6.20 — 2026-06-09

- **Project licensing is now explicit in the repository.** Added a top-level `LICENSE` file with the MIT license text and updated the README license section to point at it instead of only saying "MIT License" inline.

- **Runtime STT third-party notices are documented.** Added `THIRD_PARTY_NOTICES.md` covering cokacdir's MIT license, the runtime-downloaded `transcriptor` binary, Whisper model artifacts, whisper.cpp/ggml, dependency-notice expectations, and audio/transcript consent considerations. README, settings docs, file-transfer docs, and the website file-transfer page now point users to the notice.

- **The website footer now links to license and third-party notices.** The generated website assets were rebuilt so the deployed docs expose both legal references alongside the existing docs/GitHub links.

---

## 0.6.19 — 2026-06-08

- **Telegram audio and album handling now keeps upload context atomic when STT fails or is cancelled.** Mixed albums can contain regular files plus Telegram audio/voice items that are transcribed instead of saved. If an STT item fails, `/stop` cancels processing, or the STT confirmation cannot be sent, the bot now removes only the `[File uploaded]` pending/history records created by that same album before releasing the reserved slot. Older pending uploads and unrelated session history are left intact, so a later prompt no longer receives stale file context from a failed album while still preserving pre-existing context.

- **STT cancellation now reaches the pre-process download path.** The cancellation token is checked while waiting for Telegram `getFile`, audio HTTP download, transcriptor binary download, and chunk reads, not only after the transcriptor child process starts. Direct STT and direct album tasks also run under the correct dispatch-id scope and register panic recovery for their background task, so a panic in those fire-and-forget paths can reclaim the chat's busy slot instead of leaving it stuck.

- **Telegram STT now follows the current transcriptor progress-event protocol.** When speech recognition starts, the bot sends `Recognizing speech..`; on success it edits that same message to `🗣️ <recognized text>`, and on failure or cancellation it edits the same message to the error/stopped state. The transcriptor subprocess now runs with `--progress json`, drains stdout and stderr independently while the child is still running, parses stderr NDJSON progress events, and uses stdout only for the final result JSON. This fixes the previous false `Invalid transcriptor JSON output: EOF while parsing a value` failure while also letting the bot surface long-running model setup before transcription completes.

- **Users are now told when transcriptor downloads an STT model.** If transcriptor reports `model_download_required`, `model_download_started`, `model_download_progress`, or `model_download_finished`, the existing `Recognizing speech..` message is edited into a throttled download progress message with model name, percent, and MB counters. Cached-model loads keep the same `Recognizing speech..` text unless model loading takes several seconds, avoiding a noisy `Loading speech model..` flicker before inference starts.

- **New `/stt_model` Telegram command configures the transcriptor model per chat.** The setting is stored in `bot_settings.json` as `stt_models`; bare values such as `tiny`, `base`, `small`, `medium`, `large-v3`, and `large-v3-turbo` are passed as `--model-name`, while `path:<model_path>` is passed as `--model`. `/stt_model reset` removes the override and lets transcriptor use its own environment, saved config, or default model.

- **The runtime transcriptor binary install is now platform-correct and feature-checked.** Windows installs use `transcriptor.exe`, and existing local binaries are checked for `--progress` support before reuse. Older binaries that do not support the current progress-event protocol are replaced by the current platform artifact; empty, oversized, failed, or cancelled downloads are discarded.

- **STT model overrides and transcriptor binary replacement are now safer under real runtime conditions.** Bare `/stt_model <name>` settings suppress only the child process's inherited `TRANSCRIPTOR_MODEL` value so the chat override is not hidden by the parent environment, while `/stt_model path:...` and reset keep transcriptor's documented path/env/config/default behavior. Transcriptor binary install and replacement are also serialized with process-local and lock-file guards, so concurrent first STT requests no longer race while replacing an older binary.

- **Docs now match the Telegram STT behavior and settings surface.** The Markdown docs, website docs, and README command list state that non-audio uploads are saved to the workspace, Telegram audio and voice uploads are transcribed as STT input, model downloads are shown in the progress message, and `/stt_model` controls the chat's transcriptor model.

---

## 0.6.18 — 2026-06-04

- **The file-panel AI assistant shortcut is disabled by default and cannot be re-enabled by stale keybinding config.** `PanelAction::AIScreen` no longer binds to `.` in the panel keymap, and `Keybindings::from_config` removes both the default and any legacy `settings.json` override for that panel action. The help screen and quick reference now hide the AI Assistant section/entry when no panel shortcut is active, while the underlying AI screen code remains available internally for paths that still use it.

- **The editor now preserves line endings and trailing-newline shape instead of normalizing everything to `\n`.** File loading tracks per-line endings (`\n`, `\r\n`, or `\r`) plus trailing-newline state; save, copy/cut, insert, delete, duplicate, split/merge, move-line, undo, and redo all carry that metadata forward. Mixed-ending files, CRLF files, and files ending with a final blank line can now round-trip without invisible format churn.

- **Editor saves are safer and preserve more filesystem metadata.** Saves go through a temporary sidecar and replacement path instead of directly overwriting the target. On Unix the editor captures and reapplies mode metadata, and on Linux it also attempts to preserve supported extended attributes. Failed replacement paths avoid clobbering a newly-created sibling target and report rollback failures explicitly.

- **Find/replace, selection, and multi-cursor behavior were hardened.** Selection ranges are clamped before copy/delete/line operations, stale selections are cleared when find/replace input changes, invalid regexes report a find error without modifying the buffer, regex replacement can expand capture groups while literal mode keeps `$` text literal, whole-word matching applies to the whole pattern, and selected occurrences can be edited/deleted together through the multi-cursor path. Paste into find, replace, and goto modes now updates those input fields instead of editing the file buffer.

- **Large-file and viewer/editor handoff behavior is safer.** Opening a pending large text file now sends it to the viewer rather than trying to initialize the editor, and returning from editor to viewer clamps the viewer cursor/scroll after reload so a changed file length cannot leave stale positions.

- **File-operation progress and cancellation now report deterministic results.** Copy/move preparation cancellation emits a single `Cancelled` failure instead of a misleading count for every input file, worker disconnects are distinguished from real cancellation (`Operation worker exited without a completion message`), directory-copy child errors propagate to the parent operation, and copy-file-to-existing-destination is rejected with `AlreadyExists` rather than silently overwriting.

- **Archive create/extract error reporting is much more useful.** The default archive name switched from `<file>.tar.gz` to `<file>.tar`, matching the actual tar command path. tar create/list/extract now capture stderr and `tar:`/`gtar:` error lines from stdout, combine all available error lines instead of keeping only the first, and display full archive failures in a scrollable `TarError` dialog. Cancelled archive operations still report `Error: Cancelled` in status but do not open the large error dialog.

- **Same-folder copy now creates a duplicate instead of rejecting the paste.** Copying and pasting a file into the same directory creates a `_dup`-style duplicate (for example `file_dup.txt`) and leaves the copy clipboard intact so the action behaves like a normal copy rather than a failed paste.

---

## 0.6.17 — 2026-05-29

- **The Telegram `/model` listing was updated for Claude Opus 4.8.** The Claude provider section now labels `/model claude:opus` as `Opus 4.8` instead of `Opus 4.7`, matching the currently advertised model family in the bot's model-selection help.

---

## 0.6.16 — 2026-05-25

- **New `/fast` Telegram command toggles Codex fast service tier for the current chat.** The command is intentionally Codex-only: it checks the active provider with the same `get_model` + `detect_provider` path used by `/effort`, rejects Claude/Gemini/OpenCode with a clear provider-specific message, and accepts `/fast`, `/fast on`, `/fast off`, and `/fast status` (with aliases such as `enable`, `disable`, `reset`, and `default`). With no argument it toggles the current chat's value; `status` reports the effective stored state without mutating anything; `off` removes the per-chat override rather than writing a persisted `false`, so the absence of a key keeps meaning "use the Codex CLI default/configured service tier." The command is owner-only, registered in Telegram's BotCommand list, routed via exact slash-command matching, and listed in `/help` under Settings.

- **`BotSettings` gains a per-chat `codex_fast: HashMap<String, bool>` map persisted to `bot_settings.json`.** `load_bot_settings` reads the new field with the same tolerant object-deserialization pattern as the existing per-chat maps, so older settings files that do not contain `codex_fast` load cleanly into an empty map. `save_bot_settings` writes the field back alongside `effort` and `claude_effort`, keeping the on-disk schema explicit for tools that inspect the settings file. Runtime access goes through `is_codex_fast(settings, chat_id)`, which defaults to `false` when no override exists.

- **Codex streaming and loop-verification paths now pass `-c service_tier="fast"` whenever the chat's fast mode is enabled.** `codex::execute_command_streaming` and `codex::verify_completion_codex` both take a `fast_mode: bool` parameter and append the Codex config override before the trailing `-` stdin sentinel. All Codex spawn paths in `telegram.rs` capture and forward the setting: normal user messages, scheduled tasks, bot-to-bot messages, and `/loop` verification. The setting is captured at spawn time for a single execution so one turn stays internally consistent, while the `/loop` verifier re-reads the current value before each verification pass so a changed `/fast` setting is picked up on the next loop iteration.

- **Startup greeting messages are disabled.** The bot no longer proactively sends "cokacdir started" messages to every known chat on startup, and the old startup update-check helper was removed with that flow. The legacy `greeting` field remains in `BotSettings` so older settings files continue to deserialize, but `/greeting` is now a retired compatibility command that simply reports `Startup greeting is disabled.` instead of toggling a saved preference.

- **Documentation and website settings references now describe `/fast` instead of the retired `/greeting` setting.** `docs/how-to-configure-settings.md` documents `/fast`, `/fast on`, `/fast off`, and `/fast status`, including the exact Codex config flag and the "remove override to use provider default" semantics. `website/src/components/docs/sections/Settings.tsx` mirrors the same content in the generated website docs, and the built `index.html` / `website/dist/index.html` point at the updated hashed JS bundle.

---

## 0.6.15 — 2026-05-22

- **New `/effort` Telegram command sets the Claude/Codex reasoning effort level for the current chat's active provider.** The command is provider-aware: it inspects the chat's current model via `get_model` + `detect_provider` and routes the value to whichever provider is active, so the same `/effort high` invocation means `--effort high` on Claude and `-c model_reasoning_effort=high` on Codex. Accepted values intentionally mirror each provider's actual CLI vocabulary — Claude accepts `low/medium/high/xhigh/max` and Codex accepts `minimal/low/medium/high/xhigh`; the validators reject cross-provider values (e.g. `/effort max` is refused on Codex, `/effort minimal` is refused on Claude) before anything is persisted, so an unsupported flag never makes it into the spawned CLI. Calling `/effort` with no argument reports the current value plus the provider-specific accepted list and full usage hint, including the cross-provider differences so users don't have to discover them by trial-and-error. `/effort reset` (also `clear` / `default`) removes the override for the current provider only — the *other* provider's stored value is preserved, so a user who keeps `claude_effort=max` and `effort=high` can switch back and forth via `/model` without re-setting effort each time. Non-Claude/non-Codex providers (gemini, opencode) get a clear "not supported" message instead of a confusing validator error, because those CLIs have no reasoning-effort concept and the parameter would be dropped silently anyway. The command is owner-only (added to `is_owner_only_command`'s match list), registered with Telegram's BotCommand list so it appears in the `/` autocomplete menu, and added to `/help`'s **Settings** block with the full accepted-values reminder.

- **`BotSettings` gains two per-chat `HashMap<String, String>` maps: `effort` (Codex) and `claude_effort` (Claude), persisted to `bot_settings.json`.** The asymmetric naming (`effort` vs `claude_effort` rather than `codex_effort` vs `claude_effort`) is deliberate — the Codex map keeps the shorter name to preserve forward compatibility with any settings.json that a future build might write under that key — but the in-code accessor pair `get_codex_effort` / `get_claude_effort` keeps call-sites unambiguous, and `get_effort_for_provider(settings, chat_id, provider)` is the single entry point every spawn site uses so no caller ever picks the wrong map for the wrong provider. `load_bot_settings`'s deserializer uses the same `entry.get(field).and_then(as_object).map(...).unwrap_or_default()` pattern as the existing `use_chrome` / `end_hook` maps, so any pre-0.6.15 `bot_settings.json` (which has no `effort` or `claude_effort` keys at all) loads cleanly into empty maps without an error and without forcing a one-time migration write. `save_bot_settings` adds both keys to the JSON object it serializes under each bot's token-hash entry; the new keys are written even when the maps are empty (as `{}`) so downstream tools that inspect the file get a predictable schema.

- **`claude::verify_completion` and `claude::execute_command_streaming` now take an `effort: Option<&str>` parameter; `codex::verify_completion_codex` and `codex::execute_command_streaming` take `reasoning_effort: Option<&str>`.** `None` means "do not pass the flag at all" — the provider then uses whatever default is configured in its own config (Claude's `effortLevel` setting in `~/.claude/settings.json`, Codex's `model_reasoning_effort` in `~/.codex/config.toml`). This is the safer default than a hard-coded fallback because it respects whatever the user already configured outside cokacdir. When `Some(level)` is passed, Claude's args builder appends `--effort <level>` after `--tools` / before `--resume` (the verify path) or after `--model` / before `--chrome` (the streaming path); Codex's args builder appends `-c model_reasoning_effort=<level>` after the system-prompt `-c` block and before the trailing `-` stdin sentinel. The Codex code path additionally logs the `-c` value to the codex debug log under an `[EFFORT]` tag so an operator can grep for it when debugging why a particular run used the value it did. The Claude `verify_completion` args list had to be migrated from `Vec<&str>` to `Vec<String>` so the optional effort string could outlive the function-scope literals; the same migration applies to Codex's verify args.

- **All eight call sites of the streaming/verify functions in `telegram.rs` now plumb the chat's effort value through the spawn closure.** The three streaming-spawn sites (`handle_text_message` ~line 10081/10097, `process_scheduled_request` ~line 12546/12561, `process_bot_message` ~line 13550/13565 — each branched by provider into the Codex and Claude calls, so six call points in total) follow the existing pattern of capturing chat state under a single `state.lock().await` before dispatching: `provider = detect_provider(model.as_deref())` is computed inside the same lock, then `effort = get_effort_for_provider(&data.settings, chat_id, provider)` is captured in the same destructured tuple as the other per-spawn snapshots (model, history, allowed_tools, etc.). The `Option<String>` is cloned into an `effort_clone` (or `effort_for_exec` in the schedule path) so the spawned task owns its own copy and the closure has no borrow on the original `data` lock guard; each call site passes `effort_clone.as_deref()` to the streaming function. The verify path inside the `/loop` completion handler (`telegram.rs:10721/10723`) reads the effort fresh inside the loop_info capture so a user who runs `/effort high` mid-loop sees the new value applied on the *next* verify call without restarting the loop — the spawn-time exec still runs with the captured value so a single iteration is internally consistent, but the loop adapts on subsequent iterations.

- **The `ai_screen.rs` TUI Claude integration explicitly passes `None` for effort.** The interactive file-manager assistant is not a per-chat session and has no `/effort` analogue in the TUI; passing `None` makes the assistant honour whatever `effortLevel` the user set in their Claude config, which is the behaviour the existing TUI users had before this release. The single 1-line `None,` addition at `ai_screen.rs:932` is the entire footprint of this change in the TUI path — the surrounding submit/poll/cancel machinery is unchanged.

- **Provider-switch handling preserves effort per provider, by design.** A chat with `claude_effort=max` and `effort=high` (Codex) keeps both values across `/model claude` ↔ `/model codex` toggles; only the value matching the active provider is sent to the CLI on each spawn. This means a user who only ever uses Claude can set `/effort max` once and never think about it again, and a user who alternates between providers gets per-provider sane defaults instead of one global value that would either over-effort Codex (which has a hard `minimal` floor below Claude's `low`) or under-effort Claude (which has a `max` ceiling above Codex's `xhigh`). `/effort reset` is similarly scoped: it clears only the active provider's value so the other side's stored override survives a context switch. If a chat has no model set at all, `detect_provider` falls back to `claude` whenever Claude is available, so `/effort` in that state writes to `claude_effort` — matching how every other provider-specific command (e.g. `/usechrome`) behaves on an unconfigured chat.

- **New `command_args(text: &str) -> &str` helper next to the existing `command_name`.** Used by `handle_effort_command` to extract the level argument cleanly across all the surface forms Telegram clients produce: `/effort high`, `/effort  high  ` (extra whitespace, trimmed), `/effort@mybot high` (the `@bot` suffix is consumed as part of the command token), `/effort\thigh` (tab separator, handled by `char::is_whitespace`). The pre-existing alternative — `text.strip_prefix("/effort").unwrap_or("").trim()` — would have matched `/effortsomething extra` as `something extra` (since it's a prefix-strip, not a whole-command match) and would not have stripped `@bot`; the helper avoids both pitfalls and is reusable for future commands. The implementation uses `splitn(2, whitespace)` so the level itself can contain whitespace if a future command needs it, though `/effort`'s validator rejects anything that's not one of the enum literals.

- **Documentation refresh.** `docs/how-to-configure-settings.md` gains a `/effort` section with the accepted-values list per provider, the `reset/clear/default` semantics, and a note on how the underlying flag is plumbed (`--effort` for Claude, `-c model_reasoning_effort=` for Codex). `README.md`'s flat commands list is updated to include `/effort` in the Settings cluster. `website/src/components/docs/sections/Settings.tsx` adds a localized (en/ko) SubSection for `/effort` with the same accepted-values bullets and reset behaviour, plus the entry in the top-of-page command table.

---

## 0.6.13 — 2026-05-19

- **New `COKAC_SCHEDULE_INLINE` env var runs scheduled tasks in the chat's current session instead of an isolated workspace.** Loaded once from `~/.cokacdir/.env.json` at startup with the same strict `value == "1"` check `COKACDIR_DEBUG` uses, so the variable is effectively binary (`"1"` → on, everything else including unset/`"0"`/`"true"` → off). When on, `scheduler_cycle`'s atomic per-entry lock takes a third action `SchedAction::ExecuteInline(dispatch_id)` whenever the env is set and the chat has a session with a `current_path` — the chat's `ChatSession` is left untouched (no `prev_session` backup, no temp-session swap with `session_id: None`), only `pending_schedules` and a dispatch-tagged `cancel_token` are inserted. `execute_schedule` then reads the chat's `session_id` and `current_path` under its own lock for a defensive recapture, skips `~/.cokacdir/workspace/<schedule_id>` creation, skips `context_summary` prompt injection (the summary exists to bridge isolated mode's fresh session — redundant when staying in the same session), switches the system prompt to the inline framing, and calls the provider's `execute_command_streaming` with `cwd = chat's current_path` and `session_id = Some(chat's session_id)` so the run continues the chat's existing provider session via `--resume`. On cleanup the chat's `session_id` is updated to whatever `exec_session_id` the provider returned during this run (covers Claude fork-on-resume returning a forked id), the schedule's prompt and the reply are pushed onto `session.history` as `HistoryItem::User` / `HistoryItem::Assistant`, and `save_session_to_file` is called with the chat's `current_path` so the inline run survives bot restarts — matching the writeback the normal-message polling handler already does at `handle_text_message`'s polling completion. Safety fallback: if the chat has no session with a `current_path` at trigger time, the inline branch is rejected and the schedule falls back to the original isolated path so it still fires. All early-return paths (`cancelled_during_wait`, no-home-dir, workspace-creation error, placeholder-send error) gain a `if !inline_mode` guard around their session-restoration block because inline mode never mutated `sessions` upfront; the panic-recovery handler in `scheduler_cycle` is gated the same way. The `context_summary` re-extraction at the end of a recurring cron run is also skipped in inline mode — the chat's live session already carries the conversation forward. Default behavior (env unset) is unchanged: `inline_env_on = false` short-circuits at the eligibility check and the existing `SchedAction::Execute` path runs as before.

- **Inline-mode schedule replies no longer append the misleading `Use /<schedule_id> to continue this schedule session.` hint.** The hint points at the resume shortcut for the isolated workspace under `~/.cokacdir/workspace/<schedule_id>` — but inline mode never creates that directory, so the shortcut returns "no workspace found" when attempted. The six call sites in `execute_schedule`'s polling-completion block (cancelled file-attach placeholder edit, cancelled no-remaining stop suffix, cancelled with-remaining stop suffix, normal file-attach placeholder edit, file content footer for the attached file, and the normal main message body) were each open-coding the same `\n\nUse /{} to continue this schedule session.` suffix. They now share a single `continue_hint: String` computed once at the top of the cleanup block — empty in inline mode, the original suffix string in isolated mode — and the six format strings drop a trailing `{}` instead. Isolated-mode users see byte-identical output to before; inline-mode users see clean replies without the trailing dead shortcut. The continuation in inline mode is the chat itself — just send the next message.

- **Match-arm type-unification fix in `scheduler_cycle`.** Pulling the per-entry dispatch decision out of the action block (so `inline_mode` / `dispatch_id` / `prev_session` can be destructured from a `match` expression rather than handled inside each arm body) means the diverging `SchedAction::Skip => continue,` and `SchedAction::DiscardExpired => { …; continue }` arms must unify with the tuple-returning `Execute` and `ExecuteInline` arms. The `DiscardExpired` arm's trailing `continue` is intentionally written without a semicolon so the block's trailing expression has type `!` (Rust Reference: "A continue expression always evaluates to a value of type `!`."); with a semicolon the block would be syntactically `()` per the block-expression rules and the arms would fail to unify against `(bool, u64, Option<ChatSession>)`. The never type's universal coercion takes care of arms A and B, and the unifier resolves `None` in arm D against `Option<ChatSession>` from arm C. A comment in the arm documents this so a later cleanup pass does not "fix" the missing semicolon and break the build.

- **Documentation updates.** `how-to-configure-environment-variables.md` gains a `COKAC_SCHEDULE_INLINE` section with the `.env.json` example, the `/envvars` verification step (owner-only, 1:1 chat, dumps secrets — clear messages afterward), and the full side-effect list (recurring inline runs accumulate context, one-time inline schedules cannot be re-entered via `/<id>`, the flag is global per bot process, and `entry.current_path` is bypassed in favor of whatever path the chat is currently on). `how-to-use-schedules.md` splits "How Scheduled Tasks Execute" into Isolated mode / Inline mode subsections, adds a concrete chat-flow example for inline mode showing how a 5-minute follow-up schedule arrives in the same conversation as a normal reply, and clarifies that the "Resume a Schedule Workspace" mechanism is isolated-mode-only. `deploy_docs()` syncs both updated files to `~/.cokacdir/docs/` on the next bot start so end users see the new sections in their own environments without rebuilding.

- **Inline-mode cleanup now refuses to write back when the user mutated the chat's session mid-run.** `handle_text_message`'s polling completion has a long-standing 4-tuple guard (`provider`, `current_path`, `session_id`, `clear_epoch`) that detects `/clear`, `/start <other-path>`, `/start <other-sid>`, and model change landing while the AI is still streaming; if any input shifts, the writeback at line ~10168 is skipped so the just-mutated chat session is not partially overwritten. The first inline-mode draft mirrored only the writeback call, not the guard — so a `/clear` arriving mid-schedule (which `cancel_in_progress_task_locked` signals without waiting, then immediately empties `session.history` and `session.session_id`) would let the schedule's `cancelled=true` cleanup re-push the schedule prompt+reply into the just-cleared session and overwrite `session.session_id` with the cancelled schedule's id, partially resurrecting state the user explicitly cleared. The same race applies to `/start <other-path>` (the schedule's prompt+reply lands in the *new* path's session) and to model change (`session_id` from provider X overwrites a session that is now provider Y). The fix captures `inline_session_id`, `inline_path`, and `inline_clear_epoch` once at `execute_schedule` start under the existing inline-mode lock, clones them into the polling closure as `*_for_guard` bindings, and at cleanup re-reads `now_clear_epoch` + `now_provider` (via `get_model(&data.settings, chat_id)`) + `(now_sid, now_path)` *before* the `data.sessions.get_mut` so all shared borrows of `data` are dropped before the mutable borrow. If any of the four inputs differs from the captured value, the writeback (`session_id` update, history pushes, `save_session_to_file`) is skipped and a `sched_debug` line records the mismatch for diagnosis. `provider_str` is the schedule's start-time provider (already captured a few lines above for the resume path) so no extra binding is needed for the provider comparison. Isolated-mode cleanup is unchanged.

- **Stop the `(No response)` UI sentinel from leaking into the chat's session history.** The not-cancelled branch of the polling completion replaces an empty `full_response` with the literal string `"(No response)"` so the placeholder message can render *something* if the AI streamed nothing; the user never actually sees that string because the same branch then *deletes* the placeholder when `remaining.trim().is_empty()`. The original inline writeback gated the assistant `history.push` on `!full_response.is_empty()` — which is true for `"(No response)"`, so the sentinel was being persisted to `session.history` and then to disk via `save_session_to_file`. A future `--resume` would then see a phantom assistant turn that the provider's own JSONL transcript does not contain, and the chat's local history would render an assistant entry the user never saw on screen. The fix captures `had_real_response = !full_response.is_empty()` *before* the not-cancelled branch performs the sentinel replacement, then gates the inline assistant push on that flag instead of `!full_response.is_empty()`. Cancelled runs with partial output still push (their `full_response` was never sentinel-replaced); cancelled runs with no output, and not-cancelled runs that hit the sentinel branch, both correctly skip the assistant push so only the user prompt is recorded. Provider transcripts and chat history now agree.

- **Inline cleanup comment corrected.** The earlier comment said `current_path` was cloned to "release the immutable borrow of session before passing &mut session by-ref to the save fn" — inaccurate twice over: `save_session_to_file` takes `&ChatSession` (not `&mut`), and the call site relies on Rust's implicit shared reborrow of `&mut session` rather than passing `&mut` by-ref. Rewritten to state the actual flow (shared reborrow + cloned path string to keep borrow lifetimes from overlapping in a confusing way) so a later cleanup pass does not "simplify" away the clone and hit a borrow-checker error.

- **opencode 1.15.5 tool-name + tool-parameter compatibility refresh.** Verified `src/services/opencode.rs` against the actual `packages/opencode/src/tool/*.ts` registry in opencode v1.15.5. `normalize_tool_name` gained PascalCase mappings for seven tool IDs opencode actually emits that were previously falling through as lowercase passthrough: `task_status → TaskStatus`, `plan_exit → PlanExit`, `lsp → Lsp`, `repo_clone → RepoClone`, `repo_overview → RepoOverview`, `invalid → Invalid`, `question → Question`. The pre-existing mappings for `notebookedit`, `list`, `taskoutput`, `taskstop`, `taskcreate/update/get/list`, `todoread`, `askuserquestion`, `enterplanmode`, `exitplanmode`, `codesearch` are kept as legacy aliases (opencode 1.15.5 does not emit them, but older opencode versions and the Claude-Code parity layer may, and they are harmless when unmatched). The function header now documents which block is verified-against-1.15.5 vs legacy. `normalize_opencode_params` was extended to cover the camelCase→snake_case key conversion for `write` (`filePath`→`file_path`), `edit` (`filePath`, `oldString`, `newString`, `replaceAll` → snake_case), and `grep` (`include` → `glob`, since opencode's grep parameter is named `include` while cokacdir's `ui/ai_screen.rs` renderer reads `glob` for the file-glob filter). Previously only `read` and `apply_patch` were normalized, so `write`, `edit`, and `grep` tool calls from opencode were rendering empty file paths and missing parameter fields in the UI. A shared `rename(map, from, to)` helper enforces the "only rename when the snake_case key is not already present" rule so a hypothetical future opencode that adopts snake_case will not be clobbered. Verified against `packages/opencode/src/tool/{read,write,edit,glob,grep,webfetch,websearch,task,shell/prompt}.ts` parameter schemas; the remaining opencode tools (`bash`, `glob`, `webfetch`, `websearch`, `task`, `task_status`, `lsp`, `repo_clone`, `repo_overview`, `question`, `plan_exit`, `invalid`, `todowrite`) emit keys that already match the UI renderer's expectations and need no normalization. SSE envelope (`/global/event`), event types (`message.part.updated`, `message.part.delta`, `session.idle`, `session.status`), `sync`-envelope dedup, `OPENCODE_PERMISSION={"*":"allow"}` semantics, `POST /session` and `POST /session/{id}/prompt_async` request shapes, CLI subcommand/flag set (`opencode run --format json --dir --model --session`, `opencode serve --port --hostname`), Linux ELF binary discovery via `which opencode`, and the Windows `.cmd` → `node_modules/opencode-ai/bin/opencode.exe` resolution chain were all re-verified against the live 1.15.5 install and confirmed correct; no other code paths needed changes.

- **`(No response)` sentinel no longer shown to users and no longer persisted across all three polling completion paths.** Previously, when the AI produced no output, the polling completion branch in `handle_text_message`, `execute_schedule`, and `process_bot_message` replaced an empty `full_response` with the literal string `"(No response)"` and then edited the spinner placeholder to display that string — users reported the message as confusing ("did my prompt fail? did something break?"). Each location now captures `was_originally_empty = full_response.is_empty()` (or reuses the existing `had_real_response` flag in `execute_schedule`) *before* the sentinel replacement, then adds that flag to the `if … remaining.trim().is_empty()` rendering branch so the placeholder is silently deleted instead of edited. The sentinel is also no longer written to `session.history` — the unconditional `Assistant` push at `handle_text_message` line ~10168 / `process_bot_message` line ~13594 is now gated on `!was_originally_empty`, and the schedule's isolated-mode `sched_session` build switches from `!full_response.is_empty()` to `had_real_response`. The inline-mode cleanup's `had_real_response` gate (added earlier in this release) is unchanged. Result: AI silence now produces a clean "nothing happened" UX (spinner disappears, no message) and a clean history (User turn recorded, no phantom Assistant). The cancelled paths are untouched — they still show `[Stopped]` / `⛔ Stopped` because that *is* meaningful information about what happened. `final_response` in `handle_text_message` (consumed by the moved `session.history.push`) is no longer always moved in the empty case; the compiler sees it as conditionally consumed via the new `if !was_originally_empty` branch, so no unused-value warning. File-attach branches are statically unreachable for the empty case (`should_attach_response_as_file("(No response)".len(), …)` is always false), so the rendering reordering does not require a separate branch there.

---

## 0.6.11 — 2026-05-16

- **Opencode SSE stream switched from `/event` to `/global/event` to stop turning every turn into "(No response)".** The per-instance `/event` endpoint only emits `BusEvents` (`message.part.delta`, `session.status`, `session.idle`, …) and silently omits the `SyncEvents` that carry the in-flight part's type metadata — notably `message.part.updated` and `message.updated`. The SSE consumer in `consume_sse_chunks` learns whether a streaming part is `"text"` (forward to UI + final result) or `"reasoning"` (drop) by reading the `part.type` field from the first `message.part.updated` for that part_id and stashing it in `part_types`; without that priming, `handle_sse_event`'s `message.part.delta` branch hits the `part_types.get(&part_id) != Some("text")` guard for every delta, drops the entire text payload on the floor, and the turn ends with an empty `final_result` that the trailing `Done` message renders as the literal string "(No response)". `/global/event` forwards `SyncEvents` alongside `BusEvents`, wrapping each frame in `{ directory, project, payload: {...} }` (server.connected / server.heartbeat omit directory/project but still wrap as `{ payload: {...} }`); the consumer now unwraps the `payload` envelope before handing the inner event to `handle_sse_event`. Verified live against opencode 1.15.0: `/event` emitted zero `message.part.updated` frames for a streaming turn, `/global/event` emitted them in the order the consumer expects. The pre-existing `event_sid != parent_sid` filter inside `handle_sse_event` already drops events for other sessions, so receiving every project's traffic on the global stream is harmless for cokacdir's per-turn dedicated serve.
- **Skip the redundant `sync` envelope copy on `/global/event` to avoid double-handling.** `/global/event` re-emits each `SyncEvent` a second time as `{ payload: { type: "sync", ... } }` — a versioned mirror of an event already published unwrapped through the same stream moments earlier. Without filtering, `message.part.updated` for the same part would be processed twice (resetting `part_types`, replaying progress accounting). The unwrap step checks `inner.type == "sync"` and `continue`s before calling `handle_sse_event`. Events without a `payload` field fall through to the raw JSON path so any future unwrapped-only event is still handled.
- **Opencode serve child PID is now registered immediately after spawn, not after readiness.** Previously `spawn_opencode_serve` returned `ServeChild` only after the serve printed its "listening on http://…" line, and the outer `execute_command_streaming_serve` registered the PID into the cancel token afterwards. Any `/stop` arriving during the readiness window — which can run several seconds while the bun binary boots and binds the port — saw `child_pid = None` in the token, so `CancelToken::cancel_now`'s negative-PID SIGKILL had no target and the serve kept booting. The cgroup-v2 path covered Linux when the per-spawn cgroup attached successfully, but degraded environments (older kernels, restrictive sandboxes, non-Linux) fell back to PID kill only and lost the cancel entirely. PID registration now happens inside `spawn_opencode_serve` immediately after `cmd.spawn()`. The function also re-checks `token.cancelled` once after storing the PID and, if already set, calls `cancel_now()` + `start_kill()` + a 3-second bounded `wait()` and returns `Err("cancelled before opencode serve became ready")`. The caller treats that specific error as a cancel (suppressing the `Failed to start opencode serve:` user-facing error toast via a new `serve_cancel_hit` check). `ServeChild::id()` is removed because no caller needs to read the PID externally anymore.
- **Windows `kill_serve_process_group` now actually kills the serve tree.** `opencode` on Windows is a node launcher that execs the bun-compiled binary; the previous `#[cfg(not(unix))]` no-op left the bun child orphaned after `start_kill` reaped only the direct node wrapper. The Windows branch now invokes `taskkill /PID <pid> /T /F` so the entire tree (node + bun + any grandchildren spawned by tool calls) is terminated in one call. Non-Unix, non-Windows targets keep the no-op fallback.
- **`bridge.rs` Gemini stdio plumbing no longer deadlocks on chatty stderr.** `run_stream_json` and `run_json_mode` previously spawned Gemini with `stderr(Stdio::piped())` but never drained the pipe — the bridge captured stdout via a `BufReader`/`read_to_string` and left stderr unread. Once Gemini's stderr exceeded the OS pipe buffer (~64KB on Linux) the child blocked on its next stderr `write`, which in turn starved stdout because the writes from both fds compete for the bridge's attention. The bridge's foreground stdout loop then blocked waiting for the next stdout line that would never arrive, and the whole turn hung until something external killed gemini. Both call sites now use `Stdio::inherit()`, matching `run_text_mode` which already worked. Gemini's stderr is forwarded to the bridge process's own stderr, and the parent `gemini.rs` adapter already drains the bridge's stderr in a background thread (`gemini.rs:590`), so the data goes through one drained pipe instead of two pipes with one unread.
- **rsync transfers can now be cancelled while silently transferring a large file.** rsync emits `--progress` lines only when there is a meaningful update; long single-file transfers (multi-GB media, large tarballs) can be silent for minutes while the byte-by-byte read loop in `transfer_rsync` blocks inside `Read::read` waiting for the next byte. The pre-existing cancel check at the top of the byte loop never fires because the loop is blocked between checks. A 100ms-poll watchdog thread (`spawn_cancel_watchdog`) now runs alongside each rsync child: it constructs a throwaway `CancelToken` seeded with the rsync child's PID, watches the caller's `cancel_flag`, and on cancel calls `token.cancel_now()` which SIGKILLs the rsync process group (rsync was already placed into its own pgroup via `detach_into_own_pgroup`, so the negative-PID kill stays scoped to rsync + any sshpass wrapper and never the cokacdir TUI). After the watchdog kills rsync, the byte-loop's read returns 0 (EOF) → break → the post-loop `child.wait()` reaps the SIGKILL'd child → the new post-wait `cancel_flag` re-check returns `Ok(())` instead of treating the non-zero status as an rsync failure. Watchdog teardown is bracketed by a `cancel_watch_done` `AtomicBool` set immediately before `cancel_watch.join()` so the thread exits cleanly on the next 100ms tick once rsync has completed (or been killed).
- **rsync stderr is drained in a background thread to prevent the same pipe-fill deadlock as the bridge change.** Previously `child.stderr.take()` happened only on the error path (line 419 of the old file) — under load rsync's stderr buffer could fill while the foreground loop drained stdout, blocking rsync's next stderr write and ultimately starving stdout. The new code spawns a stderr-reader thread immediately after rsync starts, joins it after `child.wait()`, and feeds the captured string into the error toast on failure (or discards it on success). Mirrors the pattern already used in the archive create/extract path in `app.rs`.
- **SSH inactivity timeout for same-server cp/mv/rm raised from 60s to 24h, paired with 30s keepalive (max 3).** `transfer_same_server` and the cut-cleanup `delete_remote_source_files` issue single `cp -a` / `mv` / `rm -rf` commands and wait for them — these can legitimately produce no output for tens of minutes on large trees. The old 60s inactivity timeout killed the russh session mid-operation, surfacing as a generic SSH disconnect error after the user had already waited a long time. The 24h ceiling preserves the safety net for genuinely hung connections, and the new keepalive (30s interval, 3 max consecutive failed pings → ~90s dead-peer detection) means a real network failure is still caught quickly. Cancel responsiveness is preserved by the new `SshExec::exec_cancelable` path (next bullet).
- **`SshExec::exec` gains a `cancel_flag`-aware variant so `/stop` reaches a silent `rm -rf` / `cp -a` / `mv`.** With the inactivity timeout extended to 24h, a hung remote command would otherwise hold the caller for up to 24 hours. `exec_cancelable` swaps the unbounded `channel.wait().await` for a 100ms `tokio::time::timeout` wrapper around it: each tick re-checks `cancel_flag`, and on cancel it issues `Disconnect::ByApplication` to tear down the SSH session and returns `Err("Cancelled")`. The original `exec` is now a thin wrapper passing `None`. `delete_remote_source_files` and `transfer_same_server` both call `exec_cancelable` with the live cancel flag; both also pattern-match `Err(_) if cancel_flag.load() => ...` so the user gets `Completed(0, 0)` on `/stop` instead of an "SSH exec failed: Cancelled" error toast. `delete_source_files_after_cut` now takes the cancel flag too and skips the SSH connect entirely if cancel has already fired during the transfer phase.
- **SSH key path is shell-escaped when spliced into the `-e ssh -i '...'` option string.** `build_ssh_option` previously did `format!(" -i '{}'", expanded)` directly. A key path containing a single quote (rare but legal, e.g. expanded `~/keys/jenny's.pem`) closed the surrounding `'...'` early and produced a malformed shell argument that rsync's `-e` parser would either reject or — worse — interpret as additional flags spliced from the path. The path is now passed through `replace('\'', "'\\''")`, the canonical POSIX single-quote escape (close-quote / backslash-quote / re-open-quote concatenates back to a literal `'` inside single-quoted context).
- **Archive create / extract / list (`Self::create_archive`, `Self::extract_archive`, `Self::list_archive_contents`) all gain the same 100ms-poll cancel watchdog as rsync.** `tar` on a large archive can be silent for tens of seconds between verbose-listing lines, and the existing `cancel_flag` checks only fire between lines — when the loop is blocked on the next `read_line` no check runs and `/stop` has no effect until tar happens to emit the next file. `spawn_process_cancel_watchdog` (a sibling of `spawn_cancel_watchdog` in `remote_transfer.rs`) attaches to each tar child immediately after spawn and SIGKILLs the tar process group when `cancel_flag` flips. Tar was already placed into its own pgroup via `detach_into_own_pgroup`, so the kill stays scoped to tar (and `stdbuf` if used) and never the TUI. After wait, a new cancel-after-wait re-check converts a SIGKILL-induced non-zero exit into a "Cancelled" message + partial-archive cleanup, instead of "tar command failed". `list_archive_contents` had to be promoted from `Command::output()` to an explicit `spawn() → wait_with_output()` pipeline to expose the child PID for the watchdog; its signature now takes a `cancel_flag` and its single caller (`extract_archive`) passes it through. The cancel-after check at the function exit returns empty results so the extract flow's existing `cancel_flag` re-check picks up the cancellation immediately after listing aborts.
- **Detached background-handler stderr is now drained so the helper process doesn't get SIGPIPE'd or block forever.** `execute_background_command` runs viewers / openers as detached children with `stderr(Stdio::piped())`. The `Ok(Some(status))` branch (process exited within 100ms — likely an error) already read stderr synchronously to surface the error message, but the `Ok(None)` branch (process still running, treated as a successful launch) returned without taking ownership of the stderr handle. When the `Child` struct dropped at end-of-scope the parent's read end of the stderr pipe closed; subsequent stderr writes by the viewer either filled the kernel buffer and blocked the viewer, or triggered SIGPIPE and killed the viewer outright if it had no SIGPIPE handler. The `Ok(None)` branch now `take()`s stderr and hands it to a background thread that calls `read_to_end` on it for the lifetime of the helper. The thread is detached (its JoinHandle is dropped immediately); on Unix it exits naturally when the helper closes its stderr at termination.

---

## 0.6.9 — 2026-05-12

- **`CancelToken::cancel_now` now sends SIGKILL (was SIGTERM) on Unix, completing the cancel-actually-kills story 0.6.8 started.** 0.6.8 fixed the propagation half (process-group separation + negative-PID kill so grandchildren receive the signal) but left the signal itself catchable. SIGTERM is trappable, and the AI CLIs we spawn (`claude` is a Node launcher, `opencode` is an npm shim around bun, `codex` has its own handlers) do install handlers — when they catch SIGTERM they enter a graceful shutdown that stops emitting stdout while in-flight API requests finish. The worker thread in `execute_command_streaming` is blocked inside `reader.lines()` waiting for the next stdout line; with stdout silent it stays blocked indefinitely, the channel never disconnects, and the polling loop's `try_recv` keeps returning `Empty` instead of `Disconnected`. The polling loop's cancel-detected `break` path returns to `process_next_queued_message` and dispatches the next request anyway, so the new request spawns a new worker + new CLI subprocess while the previous one is still alive, still holding a tokio blocking-pool slot, and still billing the upstream API. Across a session of rapid stop-and-resubmit (queue mode OFF, redirect-on-busy semantics) the survivors stack up — eventually saturating the 512-slot blocking pool and slowing every subsequent turn as the scheduler waits for free threads. SIGKILL is uncatchable: the kernel removes the process on the next scheduler tick, stdout closes, `reader.lines()` returns `None`, the worker reaps the child via `kill_child_tree` + `child.wait()` (idempotent — kernel already cleaned up) and exits. Bounded cleanup, no orphaned API calls, no pool exhaustion. The non-graceful kill is correct here because these CLIs hold no client state worth flushing on shutdown — their entire role is to stream JSON to stdout, which is already captured by the time `cancel_now` fires. Mirrors what the codebase already does in adjacent paths: `kill_child_tree` (post-cancel escalation inside the worker) and `opencode::ServeChild::shutdown` (3-second SIGKILL + wait for serve teardown) both target SIGKILL on the process group. The change is one signal constant flip in `claude.rs:cancel_now`, and because 0.6.8 consolidated six duplicated kill blocks into this single method, every cancel site in the codebase — `cancel_in_progress_task_locked` (queue-OFF redirect), `handle_stop_command` (`/stop`), `handle_stopall_command` (`/stopall`), the four post-loop cancel handlers (text streaming, shell streaming, schedule, botmsg poll), and the `cancel_token_now` panic-recovery wrapper — inherits the new semantics without further edits. Windows path is untouched: `taskkill /T /F` was already a force-kill of the tree.

---

## 0.6.8 — 2026-05-12

- **TUI AI screen ESC now actually kills the in-flight Claude process instead of just hiding it.** Pressing ESC during an in-flight request rendered "Cancelled." in the history and flipped `is_processing=false`, but `AIScreenState::cancel_processing` only dropped the channel receiver — it never signalled the spawned `claude` CLI child. The worker thread's `tx.send` would then fail with `SendError`, the streaming loop would `break`, and the worker would block in `child.wait()` until the child finished its work naturally (which for a heavy tool-using request is minutes, not seconds). Each cancel-then-resubmit cycle therefore stacked another fully-running `claude` Node process plus its API connection in the background, manifesting to the user as the chat getting progressively slower per ESC. The root cause was that `submit()` passed `cancel_token: None` (the 7th positional arg of `claude::execute_command_streaming`), so the entire well-built cancel infrastructure inside `claude.rs` (PID storage, atomic flag, mid-loop cancel checks, kill-then-return paths) was bypassed for every TUI request — only `main.rs --cancel-after` and Telegram `/stop` ever wired it up. `AIScreenState` now carries a `cancel_token: Option<Arc<CancelToken>>` field; `submit()` allocates a fresh token and passes a clone to the worker; `cancel_processing()` calls `token.cancel_now()` (sets `cancelled=true` **and** SIGTERMs the child); and `poll_response`'s natural-completion cleanup clears the token alongside the receiver so a successful turn doesn't leave a stale Arc dangling. The processing-completion paths (Done message, Error message, channel disconnect) all converge on the same `processing_done=true` cleanup branch, so the token has exactly one drop site for each end state.
- **Cancel signal now reaches subprocesses spawned by the AI CLI (Bash from a tool call, the bun binary behind `opencode`, etc.) — not just the immediate child.** Previously every cancel path on Unix did `libc::kill(pid, SIGTERM)` (or `child.kill()` for `kill_child_tree`), which only signalled the direct child PID. `claude` is a Node launcher that spawns Bash for the Bash tool, Bash spawns the actual command, and SIGTERM to the Node process did not propagate to those grandchildren — they got reparented to init as orphans and continued running until they finished naturally (a long `find` or `tar` could outlive the cancel by minutes). `opencode`'s legacy path had the same shape: the npm shim execs the bun binary, and killing the shim alone left the bun child as an orphan (the serve path had already worked around this with its own `process_group(0)` + negative-PID kill, which is now the model the rest of the code follows). Every spawn site whose child can be killed by `cancel_now` or `kill_child_tree` is now placed into its own process group via a new `claude::detach_into_own_pgroup` helper (a thin wrapper over `CommandExt::process_group(0)`, no-op on Windows where `taskkill /T /F` already kills the tree by PID), and the kill paths target the negative PID so the entire group — child plus every descendant that didn't explicitly `setpgid` away — receives the signal in one syscall. Coverage: claude.rs streaming spawn, codex.rs streaming spawn, gemini.rs `build_bridge_command`, opencode.rs `build_opencode_command`, telegram.rs `/shell` bash/powershell spawn, app.rs tar/untar (cokacdir's own archive operations), and remote_transfer.rs rsync. Critical safety property: `kill_child_tree`'s switch from `child.kill()` (direct PID) to `kill(-pid, SIGKILL)` (process group) means *every* spawn whose child is later killed by `kill_child_tree` MUST be in its own process group — otherwise the negative-PID kill targets the inherited group, which is the cokacdir/bot process itself. The 17 `kill_child_tree` call sites and 9 detach sites were cross-referenced spawn-by-spawn before shipping; tar/untar and rsync (which historically used `child.kill()` and survived on the assumption of no grandchildren) explicitly received `detach_into_own_pgroup` for this reason even though the old behaviour happened to work.
- **Six duplicated SIGTERM/taskkill blocks in telegram.rs collapsed into a single `CancelToken::cancel_now()` method.** `cancel_in_progress_task_locked`, `handle_stop_command`, `handle_stopall_command`, and the four post-loop cancel handlers (text streaming, queue/loop streaming, `execute_schedule`, `botmsg_poll`) each previously open-coded the same 8-line "lock `child_pid` with poison recovery → match on `Some(pid)` → `#[cfg(unix)] libc::kill(pid, SIGTERM)` / `#[cfg(windows)] taskkill /T /F`" pattern. Six copies of identical safety-critical code is six places to forget the poison-recovery branch, six places to forget the Windows `taskkill /T` flag, and six places to remember to update if the kill semantics ever change (which is exactly what happened in this release with the process-group switch). The kill body is now defined once on `CancelToken` itself; every site (including the existing `cancel_token_now` wrapper used by panic recovery and shutdown drain) calls `token.cancel_now()` and inherits the new group-kill semantics for free. The wrapper function is kept because its name appears in five active call sites and six surviving doc references — collapsing it would have churned more lines than it saved. `handle_stopall_command` retains its lock-internal `cancelled.store(true)` because the duplicate-detection logic captures the previous value of the flag inside the same lock; `cancel_now` then sets the flag again outside the lock, which is idempotent and harmless. `handle_stop_command`'s previously separate "set the flag IMMEDIATELY to close the rate-limit-wait race" line is now subsumed by `cancel_now` — race protection is preserved because `cancel_now` is sync and runs before the rate-limit `await`, the same point in the call sequence.

---

## 0.6.5 — 2026-05-07

- **Busy-slot panic recovery now uses dispatch ownership instead of `Arc::strong_count`.** 0.6.4's `strong_count == 1` check correctly avoided the false-positive of killing a foreign owner's child, but had a known gap: a panic in a handler whose fire-and-forget sub-task was still running left the slot busy until that sub-task finished, because the surviving clone kept `strong_count ≥ 2`. Each `CancelToken` now carries an `owner_dispatch_id: AtomicU64` set at creation, and every chat_worker unit gets a fresh dispatch id from a process-wide counter via a `CURRENT_DISPATCH_ID` task-local. Recovery removes the slot iff the post-panic token's owner matches the panicked dispatch's id — independent of how many `Arc` clones are still in flight. Scheduler reservations, queued feedback, and later handlers carry different owner ids, so the same check still leaves their tokens alone. Both panic-recovery sites — chat_worker dispatch and inline dispatch — also now use `cancel_token_now(&tok)` (sets `cancelled=true` **and** SIGTERMs the recorded child PID); the chat_worker path previously only SIGTERMed and left siblings (e.g. an exec polling loop on a blocking thread that survived the async parent's panic) waiting on signal delivery alone.
- **Inline queue/feedback dispatches and scheduler-side handlers now recover from panics.** 0.6.3 covered chat_worker dispatch only. The remaining panic surfaces — the queue/loop inline `tokio::spawn` (`loop:dispatch`, `queue:next`), `execute_schedule`, and `process_bot_message` — were each fire-and-forget and a panic invisibly stranded the pre-inserted busy-slot token, plus (for `execute_schedule`) the schedule-specific session and `pending_schedules[chat_id]` entry, so the chat would refuse new requests forever and a `/start` would still see the half-mutated state. Each of these sites is now wrapped by an inner `tokio::spawn` running inside `CURRENT_DISPATCH_ID.scope(dispatch_id, ...)`, the JoinHandle is awaited, and on `JoinError` the new `reclaim_panicked_dispatch_token` helper performs the owner-id-gated cleanup. The `execute_schedule` path additionally restores `sessions[chat_id]` from a captured `prev_session` clone and removes the schedule's id from `pending_schedules` — restoration is idempotent so a panic after `execute_schedule`'s own cleanup just rewrites identical state. All four sites also print `Chat <id> <ctx> ... panicked: ... — recovering` to stderr for operational visibility.
- **`scheduler_loop` itself now survives panics inside its cycle body.** Previously a single `loop { ... await ... }` whose body could panic on `chrono::NaiveDateTime::parse_from_str`, `list_schedule_entries`, `should_trigger`, any file IO, or any Telegram API call — and a panic killed the scheduler permanently for the rest of the process lifetime. The cycle body is now extracted into `scheduler_cycle` and each 5-second tick spawns it and awaits; a `JoinError` is logged (`Scheduler cycle panicked: ... — continuing on next tick`) and the loop continues. Per-dispatch state is already protected by the inner recovery paths above, so cycle-level panics primarily affect logging and the in-progress iteration of the entries/messages loops; the next tick re-scans the schedule directory and the messages spool, so any work skipped by the panicked iteration is naturally retried.
- **Deterministic `run_bot` shutdown via `RunBotCleanup` + sync token mirror.** A `RunBotCleanup` RAII guard installed early in `run_bot` runs at function exit (graceful shutdown, fatal `PollingExit`, or panic propagating up). Its `Drop` synchronously drains a `request_tokens` mirror of `cancel_tokens` — a `std::sync::Mutex<HashMap<ChatId, Arc<CancelToken>>>` kept in lock-step by `insert_cancel_token_locked` / `remove_cancel_token_locked`, the only two paths now allowed to mutate `cancel_tokens` — and SIGTERMs every recorded child PID. The sync mirror exists because async-locked state is unreachable from a sync `Drop` when the runtime is shutting down; without it, in-flight AI subprocesses could outlive the process and become init-adopted zombies. As a defensive second pass, the Drop also `try_lock`s the async state for re-cancel (idempotent) and schedules a final async cleanup via `Handle::try_current().spawn(...)` for cases where the try_lock failed but the runtime is still running. Polling and scheduler tasks are aborted via `AbortOnDrop` guards before `RunBotCleanup`'s drop, so by SIGTERM-time the only outstanding work is in-flight AI subprocesses.
- **Per-request async tasks are now tracked for shutdown.** Three `spawn_tracked_*` helpers (`request_task`, `blocking_task`, `blocking_result`) replace fire-and-forget `tokio::spawn` / `tokio::task::spawn_blocking` calls in every AI streaming path (text, queue, loop, schedule, bot-to-bot). Each spawn registers an `AbortHandle` in `request_tasks: Arc<Mutex<HashMap<u64, AbortHandle>>>` under a monotonic id, and a `RequestTaskGuard` removes the entry on drop via a ready-channel pattern: the guard is sent to the spawned task immediately after registration, so an abort that fires before the task starts cleanly drops the guard on the spawning side and the entry is removed without leaking. `abort_request_tasks` aborts every registered handle at shutdown. Doc comments on the blocking variants spell out the tokio limitation — `AbortHandle::abort()` does not preempt a blocking thread; shutdown of the AI subprocess itself relies on the `CancelToken`'s `cancelled` flag + SIGTERM path, not the abort handle. `ChatWorkerEntry` was similarly extended from a bare mpsc sender to `(sender, AbortHandle)` so a forced removal mid-handler aborts the worker cleanly instead of waiting for it to next yield.

---

## 0.6.4 — 2026-05-07

- **Backend death no longer kills sibling bots in a multi-bot deployment.** 0.6.2 surfaced Discord/Slack gateway death by having `run_bridge` call `std::process::exit(1)` directly — correct for a single-bot deployment, but in multi-bot setups (Telegram + Discord + Slack running concurrently in one cokacdir process) one bridge's backend dying tore the entire process down and stopped every other bot too. This contradicted 0.6.1's per-bot isolation principle (where a Telegram fatal `PollingExit` only ends that bot's task and leaves the others running). `run_bridge` now returns a `BridgeExit` enum (`Graceful` | `Fatal`) instead of exiting the process. `main` decides the exit code at the right scope: a single-bot bridge still exits with status 1 on `Fatal` (preserving the supervisor signal that systemd / docker watch for), and the multi-bot path collects `Fatal` flags via a shared `AtomicBool`, lets healthy bots keep serving traffic, and only exits 1 after every bot's task has finished. The detached `run_proxy_server` task and the backend listener are now held as `JoinHandle`s and explicitly aborted before `run_bridge` returns (via an `abort_handle` for the backend), so a dying bridge does not leak its TCP-bound proxy port or its gateway listener into the runtime that the surviving bots are sharing.
- **Chat busy-slot panic recovery no longer false-reclaims foreign tokens.** 0.6.3 used an `Arc::ptr_eq` snapshot to decide whether the token currently in `cancel_tokens` belonged to the panicked handler (reclaim) or a foreign owner (leave alone). The check had two narrow but real race windows: (a) `pre_token == None` followed by a foreign task (`execute_schedule` / `process_bot_message`) inserting between the snapshot and the handler's first `state.lock()` — if the handler then took the queue path and panicked, the foreign-inserted token was misclassified as handler-inserted and got removed plus its child SIGTERMed; (b) `pre_token == Some(F)` where `F` finished and a *different* foreign owner inserted `G` during the gap — `Arc::ptr_eq(F, G)` is false, so `G` was again falsely reclaimed. The check is now `Arc::strong_count(post) == 1`: reclaim only when the map's entry is the *only* live `Arc` reference to this `CancelToken`. Every still-running owner (the panicked handler before unwind, a fire-and-forget child holding a clone, a scheduler-side task, or any foreign owner that inserted before this dispatch) keeps at least one extra `Arc` clone in its stack, so `strong_count` observes ≥ 2 while any of them are alive and the entry is left for the real owner to clean up. The state lock is held across both the count check and the `remove`, so the count cannot be mutated between the two. The trade-off — that a panic in a handler whose fire-and-forget sub-task is still running will leave the slot busy until that sub-task finishes naturally instead of being aggressively SIGTERMed — was the explicit known-gap territory called out in 0.6.3's changelog and is preferable to the prior false-positive of killing a healthy foreign task's child.

---

## 0.6.3 — 2026-05-07

- **Chat is no longer permanently stuck "busy" after a handler panic.** `/stop` and `/stopall` only set `cancelled=true` on the cancel token — they intentionally do not remove the map entry, deferring removal to the in-flight task's normal cleanup path. When that task panicked between `cancel_tokens.insert` and `cancel_tokens.remove` the cleanup never ran, leaving the slot held forever and the chat unable to start any new AI request (queue/redirect would always see "busy"). Even `/stop` could not recover this state because it never removed the token. The chat_worker's `Err(join_err)` arm now reclaims the orphaned slot immediately after the inner `tokio::spawn` reports a panic and best-effort SIGTERMs the child PID stored in the token in case the AI subprocess outlived the panicked parent task. Logged to `msg.log` and stderr (`Chat <id> busy slot reset`). Reclaim is gated by an `Arc::ptr_eq` identity check against a pre-dispatch snapshot: a token whose `Arc` identity is unchanged from before this unit ran belongs to some other still-running owner — either a fire-and-forget polling task spawned by an earlier unit on this same worker, or a scheduler-side task (`execute_schedule` / `process_bot_message` invoked from `scheduler_loop`) — and is left alone, so the panicking handler cannot strand a foreign owner or SIGTERM its child. Coverage is the chat_worker dispatch (the bulk of user-message panic surface); panics inside fire-and-forget tasks spawned by `handle_text_message` / `handle_shell` / `execute_schedule` / `process_bot_message` (the per-unit polling loops at lines 9201 / 7522 / 11581 / 12427) remain uncovered as a known gap, since wrapping each requires invasive refactoring of its polling task structure.
- **Bot-to-bot message no longer lost in TOCTOU window.** `scheduler_loop` previously deleted the on-disk message file at `~/.cokacdir/messages/<id>.json` immediately after its busy-check (under `state.lock()`) and then called `process_bot_message`. Between the lock release and `process_bot_message`'s own claim, a concurrent chat_worker on another thread could claim the slot — `process_bot_message` would then see "busy" and return early with the file already gone, dropping the message silently. The deletion is now performed inside `process_bot_message` immediately after its claim succeeds; if the claim fails the file is left on disk so the next 5-second scheduler tick re-discovers and re-attempts it.

---

## 0.6.2 — 2026-05-07

- **Discord and Slack backend death is now detected and surfaced.** Previously the bridge spawned each backend's gateway listener (`serenity::Client::start` for Discord, `SlackClientSocketModeListener::serve` for Slack) into a fire-and-forget `tokio::spawn` and discarded the `JoinHandle`. When the listener gave up reconnecting (token revoked, persistent disconnect, banned bot, internal panic) the proxy server and teloxide poller stayed up but no messages ever arrived — the bot looked healthy but was vegetative. This is the exact same failure-class as Telegram 401/409 that 0.6.1 already addressed, just on the other side of the bridge. `MessengerBackend::start` now returns `Result<JoinHandle<()>, String>`, and `run_bridge` races that handle against `run_bot` via `tokio::select!`. When the backend dies first, cokacdir prints `Backend listener stopped — bot can no longer receive messages. Reason: <gateway exit / panic / error>. Fix the underlying issue and restart cokacdir.` and exits with status 1 instead of presenting a silent dead bot.
- All three `MessengerBackend` implementations (`ConsoleBackend`, `DiscordBackend`, `SlackBackend`) updated to return their internal task's `JoinHandle` so the death signal propagates uniformly. `tokio::task::spawn_blocking` (Console stdin) and `tokio::spawn` (Discord/Slack) both yield `JoinHandle<()>` directly, so no wrapping required.

---

## 0.6.1 — 2026-05-07

- **Strict per-chat ordering across `getUpdates` batches.** The previous `process_batch` spawned a fresh task per chat per batch and immediately returned, so a slow handler on chat C in batch N could still be running when batch N+1's task for chat C started — `state.lock()` ordering was undefined across batches. Each chat now has a long-lived FIFO mpsc worker (`run_chat_worker`) that pulls units one at a time and awaits each before the next. `process_batch` only pushes into the channel; the worker is created on first use and reused across reconnects, so within a chat the arrival order Telegram delivered is exactly the processing order, regardless of how many batches the messages span.
- **Conflict (multiple-instance) and Unauthorized (revoked token) are now fatal instead of silently retried.** `polling_loop` previously treated every `getUpdates` error as transient and slept-and-retried, so two cokacdir processes on the same token would thrash forever stealing updates from each other, and a revoked token would loop indefinitely with no surfaced cause. `detect_fatal_polling_error` now classifies these by inspecting the full `RequestError::Display` (covering both `Api(Unauthorized)` typed variants and the `Api(Unknown(...))` wrapper, plus any `Network` re-wrapping in future teloxide versions) with anchored matches `Conflict: ` and `Unauthorized` (trailing or before `: `). The anchors avoid false-positive on unrelated messages like `Bad Request: scheduling conflict in cron expression` or `... user is unauthorized to ...`, which would otherwise stop a healthy bot. `polling_loop` returns `PollingExit::Fatal(reason)` and `run_bot` prints `Bot @<name> stopped: <reason>. No reconnect — fix the underlying issue and restart cokacdir.` and exits the reconnect loop. Other bots in the same process keep running.
- **`polling_loop` honors `RetryAfter` verbatim.** A 429 response with a server-mandated cooldown (`RetryAfter(s)`) now sleeps for exactly `s` seconds and resets the local backoff to 500 ms, instead of compounding the linear 500 ms→1 s→2 s→… escalation while ignoring the server's request. Mirrors the `RetryAfter` handling already in `get_updates_with_retry` (startup flush) and the spinner-edit path (introduced in 0.4.99).
- **Panics inside chat handlers no longer disappear silently.** Each `DispatchUnit` is now executed inside an inner `tokio::spawn` whose `JoinHandle` is awaited by the worker; on `JoinError::is_panic` the worker logs `[chat_worker <id>] handler PANICKED: <msg>` to `msg.log` and prints `⚠ Chat <id> handler panicked: ... — continuing` to stderr, then resumes with the next unit. The previous detached-task model dropped the `JoinHandle` and a panic was invisible to operators.
- **Graceful chat-worker shutdown.** When `run_bot` exits the reconnect loop (fatal or future shutdown signal), the workers map is cleared so the senders drop and each worker observes `recv() → None` and exits on its own — no `abort()` mid-handler, so an in-flight unit is never killed at an inconsistent point.
- Internal: `DispatchUnit` (was a local enum inside `process_batch`) and `ChatWorkers` are now module-level so the same type flows through the per-chat channel; `process_unit` centralizes the album-fragment vs ≥2-photo dispatch decision.

---

## 0.6.0 — 2026-05-06

- **Telegram long-polling no longer times out during idle periods.** teloxide's default reqwest client ships a 17 s timeout, but `polling_loop` asks the server for a 30 s long-poll — the client closed the connection mid-poll, surfacing as repeated `getUpdates ... operation timed out` errors in `msg.log` whenever no messages arrived for ~17 s. The bot now builds reqwest with a 45 s timeout that strictly exceeds the long-poll window.
- **Codex `--sendfile` paths with spaces are now extracted correctly.** The previous extractor split on whitespace and grabbed the next token, so `--sendfile "/path/with spaces/img.png"` was truncated at the first inner space. The extractor now walks the command string, validates `--sendfile` as a whitespace-bounded token (rejects matches like `--no-sendfile`), and respects single/double quotes so the full quoted path is recovered.
- **Windows askpass refuses passwords containing newline or `"`.** CMD's `echo` cannot safely encode either character — a newline splits the script into a new command (injection) and a `"` closes a quoted segment. The askpass-script generator now errors out with a clear message instead of attempting partial escaping that CMD's parser quirks would defeat.
- Internal: `read_group_chat_log_tail` no longer double-counts corrupt lines on its second pass (pass 1 already attributes every io/parse failure under the same shared lock).

---

## 0.5.9 — 2026-05-06

- **Bot tokens are now redacted from on-disk debug logs and user-facing error messages.** teloxide / reqwest can include the request URL (`/bot<TOKEN>/...`) in some error kinds — both `RequestError::Network` and `reqwest::Error::Display` are known offenders. A process-wide token registry is consulted by `redact_known_tokens` from `tg_debug` (`debug/api_*.log`), `msg_debug` (`debug/msg.log`), `sched_debug` (`debug/cron.log`), `ai_trace` (`debug/ai_trace.log`), the file-download error path, and every `println!`/`eprintln!`/Telegram error message that renders a teloxide error.
- **Pending-updates flush at startup is now mandatory.** Previously a transient network failure during `getUpdates(offset=-1)` would log a warning and start polling anyway, leaking stale messages into the new run. Both flush steps now retry up to 5 times with exponential backoff (and honor `RetryAfter`); exhausting retries aborts the process with `FATAL: failed to {fetch,confirm} pending updates after 5 attempts` instead of proceeding with a half-flush.
- **Per-chat strict ordering for batched updates.** A `getUpdates` response containing two messages from the same chat used to spawn two independent tasks that raced for `state.lock()`. Updates are now grouped by `chat_id` and each chat is handled by a single task that awaits its units sequentially; different chats still run in parallel. Album batching is preserved.
- **`/debug` is now per-bot, not per-chat.** The flag is stored once per bot token; `refresh_global_debug_flags` re-evaluates the process-wide enable state at toggle time (env override or any saved bot's flag). Toggling OFF in one chat now reports `Shared debug logging is still ON because another bot or COKACDIR_DEBUG=1 enables it.` when applicable instead of misleadingly claiming logs were disabled.
- **Slash-command routing uses exact name matching.** `text.starts_with("/foo")` is replaced by `is_cmd(text, "foo")` across every router branch, so a future command like `/silentmode` or `/queueoff` cannot be silently re-routed to `/silent` / `/queue`. `command_name` strips an optional `@botname` suffix before comparison.
- **Owner-only commands now reject in group chats with a single clear message** (`Only the bot owner can use this command.`) via a centralized `is_owner_only_command` gate (covers `/start`, `/clear`, `/public`, `/setpollingtime`, `/model`, `/greeting`, `/debug`, `/envvars`, `/usechrome`, `/silent`, `/queue`, `/direct`, `/contextlevel`, `/instruction`, `/instruction_clear`, `/setendhook`, `/setendhook_clear`, `/allowed`).
- **Tail-N reader for group-chat logs.** `read_group_chat_log_tail(chat_id, n, …)` streams the JSONL with O(n + bot_count) memory using a two-pass scan (clear-marker map, then a sliding window of size `n`). The system-prompt hot path used to call `read_group_chat_log_range(.., 1, None, ..)` and slice the tail, materializing the whole log on every AI turn — now linear in the window size only.
- **Cron expressions are validated at write time.** `validate_cron_expression` rejects field-count mismatches, named values (JAN/MON), macros (`@reboot`), the L/W/? characters, out-of-range numbers, and zero step. Invalid `--at` values now error at register/update time instead of silently never firing. Includes a `Sunday is 0, not 7` hint when day-of-week=7 is supplied.
- **Schedule IDs from CLI input are validated as `[0-9A-F]{8}` before being composed into a path.** `--cron-context`, `--cron-history`, and `--cron-remove` now refuse path-traversal segments. `schedule_history_path_pub` returns `None` for malformed ids, and `delete_schedule_entry_pub` / `delete_schedule_history_pub` short-circuit the same way.
- **`--cron-history` redacts only after authorization succeeds.** Calling redact on a smuggled path could otherwise write outside the `schedule_history` dir. Redaction now runs only after the caller proves authorization via the live entry or the first history record's verifier; `is_valid_schedule_id` is enforced as defense in depth.
- **Session IDs spliced into AI-CLI argv are now argparse-injection-safe.** `is_valid_session_id` (Claude, Codex, Gemini, OpenCode, AI screen, and the shared `services::process` helper) explicitly rejects a leading `-`. Without that, a value like `--config /etc/passwd` would pass the prior alphanumeric-and-dash check and be parsed as a new flag by the downstream CLI.
- **Dedup verifies byte-level equality before destructive deletion.** A theoretical MD5 collision could otherwise cause `run_dedup` to remove a non-duplicate file. `files_byte_equal` reads both files in equal-sized 64 KB chunks via `read_exact` (avoiding `Read::read` short-read mismatches that the prior code path was vulnerable to) and is invoked under the cancel-flag check.
- **Symlink security in archive / copy paths hardened.**
  - `target_is_sensitive` matches on path-segment boundaries — `/etc` no longer matches `/etcd/foo`.
  - `check_symlinks_for_tar` canonicalizes the base directory once and fails closed if it cannot be resolved; previously a transient canonicalize failure bypassed all checks (fail-open).
  - `check_symlink_recursive` propagates `read_dir` errors instead of silently skipping unreadable directories; `collect_unsafe_symlinks` excludes a directory it cannot enumerate.
  - `copy_dir_recursive_with_progress` now rejects circular symlinks via a `HashSet` of canonicalized parents and a `MAX_COPY_DEPTH` guard, mirroring the existing unprefixed copy path.
- **`.cokacenc` decryption masks setuid/setgid/sticky bits.** A maliciously crafted archive cannot set `04755` on an extracted file as a privilege-escalation vector — `unpack_file_group` applies `mode & 0o0777` before `set_permissions`.
- **Discord and Slack file-fetch endpoints are now host-restricted.** The proxy receives the file URL via an HTTP path component, so without a host check an attacker who could reach the bridge port could SSRF arbitrary URLs — and on Slack, ship the bot token in the `Authorization` header. `is_allowed_discord_file_url` accepts only `cdn.discordapp.com` / `media.discordapp.net`; `is_allowed_slack_file_url` accepts `files.slack.com`, `slack.com`, and `*.slack.com`. Both match host on a segment boundary so `cdn.discordapp.com.evil` is rejected, and host extraction terminates at `?` and `#` so query-only URLs cannot smuggle the boundary.
- **Bridge token comparison is now constant-time.** The 401 path in `route_request` used a plain `!=`, which leaks a timing oracle on the prefix of `state.expected_token`. `tokens_eq_constant_time` always inspects every byte and uses `std::hint::black_box` to discourage length-leak optimization.
- **`bot_settings.json` is now written `0600` (parent dir `0700`) on Unix.** The file holds chat history, working paths, and chat IDs; permissive defaults previously left it readable to other users on shared hosts. The atomic `tmp` file is also chmod'd before the rename.
- **`PartialFileGuard` cleans up partial SFTP downloads on cancel/error.** Failed or cancelled transfers no longer leave a truncated file masquerading as a successful one. The guard drops the file handle before `remove_file` so Windows' open-file lock doesn't block removal.
- **`AskpassGuard` removes the temporary `SSH_ASKPASS` script via RAII**, with a random per-call nonce in the filename so concurrent transfers from the same PID don't collide on `askpass_<pid>`.
- **Stderr is now drained in a background thread for Claude, Gemini, and OpenCode-legacy.** When the child wrote more than ~64 KB to stderr while the parent was blocked reading stdout, the pipe filled and the whole pipeline deadlocked. Pattern mirrors `codex.rs`.
- **`expand_tilde` consolidated into `services::remote`.** `~`, `~/`, `~\` resolve to the user's home; `~user/` is intentionally left unexpanded (we cannot resolve another user's home, and rewriting it as `$HOME/user/` would yield a silently-wrong path). Replaces three duplicated implementations across `remote.rs` and `remote_transfer.rs`.
- **`handle_message` no longer wipes pending uploads when a message is for a sibling bot.** A `;`-prefixed photo upload addressed to all bots could previously be silently lost when one bot saw a follow-up text intended for another. Uploads are now consumed only when an addressed message actually arrives.
- **`/envvars` is now 1:1-only.** A group-chat dump would expose env vars like `ANTHROPIC_API_KEY` to non-owner members. Replies with `/envvars is only available in a 1:1 chat with the bot.` in groups; the existing owner gate is preserved everywhere.
- **`getUpdates` offset boundary handled explicitly.** `next_offset_after(last_id)` caps the offset at `i32::MAX` and logs the boundary hit when triggered (rare in practice — `update_id` rolls past i32 very slowly).
- **File-extension truncation in the panel uses `chars().count()`.** A multi-byte extension like `.한글` no longer panics with "byte index is not a char boundary" inside `&str` slicing.
- **`append_group_chat_log` and `read_group_chat_log_range` log every silent-loss path under `/debug`.** Previously a `create_dir_all`/`open`/`lock_exclusive`/`write_all`/`sync_data` failure dropped the entry without trace; the debug stream now identifies which step failed and how many lines were unreadable / unparseable.
- 7 new built-in docs (env vars, settings, tools, Slack bot setup, file transfer, shell commands, sharing bot with others) ship in `~/.cokacdir/docs/` so the AI can reference them — see also 0.5.8.
- Documentation and website updates across the env vars, settings, file-transfer, group-chat, multi-chat, request-management, schedules, and Slack sections.

---

## 0.5.8 — 2026-05-04

- **7 missing built-in docs are now deployed.** `deploy_docs()` previously omitted `how-to-configure-environment-variables.md`, `how-to-configure-settings.md`, `how-to-manage-tools.md`, `how-to-setup-slack-bot.md`, `how-to-use-file-transfer.md`, `how-to-use-shell-commands.md`, and `how-to-share-bot-with-others.md`, so the bot couldn't answer questions that referenced them. Added to the install set.
- **New `how-to-share-bot-with-others.md` guide** documenting the BotFather privacy toggle + group + `/direct` + `/public on` + `/contextlevel 0` flow for letting non-owner users interact with the bot through a shared group chat.
- Documentation updates across `how-to-configure-settings.md` (per-bot scope of `/debug`, `/usechrome` reference), `how-to-manage-requests.md` (`/queue OFF` redirect mechanics, confirmation-message wording, `/stop` / `/stop_<ID>` reply text), `how-to-manage-tools.md` (provider restriction: `/allowed` rejects on Codex/Gemini/OpenCode), `how-to-setup-discord-bot.md` (corrected required intents — only `MESSAGE_CONTENT` is required; `Manage Messages` permission removed), `how-to-simulate-multiple-chats-with-one-bot.md` (`/direct` is owner-only group-only; `/contextlevel` default is 12), `how-to-use-file-transfer.md` (concrete `/down` error messages), `how-to-use-shell-commands.md` (spinner replaces line-by-line streaming, 4000-byte threshold measured against rendered block, Windows powershell invocation), and `how-to-use-start-session-and-clear.md` (full ordering of `/clear` cancel-and-clean steps).

---

## 0.5.7 — 2026-05-04

- **Long-message splitter no longer produces empty chunks.** When `rfind('\n')` returned position 0 the resulting `raw_chunk` was empty and Telegram rejected the send with `text must be non-empty` (typically reproducible on AI responses that began with a blank line). Both `send_long_message` (5 split sites) and `truncate_str` now fall back to the full UTF-8-safe boundary when the only available newline split point would yield an empty leading chunk.

---

## 0.5.6 — 2026-05-04

- **Slack bot support added.** You can now use Slack bot tokens with `--ccserver`. Slack runs over **Socket Mode**, so a bot token (`xoxb-...`) and an app-level token (`xapp-...`) are both required. Token format is auto-detected as `xoxb-...,xapp-...` (either order), or you can prefix explicitly with `slack:`. Telegram, Discord, and Slack bots can now run simultaneously in the same server. New `slack-morphism` dependency. See the new "Slack Bot Setup" guide in the docs.
- **Atomic multi-attachment processing across all three platforms.** Telegram albums (members of the same `media_group_id` arriving in one `getUpdates` batch), Discord multi-attachment messages, and Slack multi-file uploads now behave identically: every file in the bundle is saved to the workspace, and the message caption — typically attached to the first item — routes the whole batch as a single AI request. Discord and Slack synthesize a `media_group_id` (`d:<msg_id>` / `s:<ts>`) on fan-out so the downstream album path picks them up uniformly. Previously, only the first Discord attachment was processed.
- **Telegram polling switched from `teloxide::repl` to direct `getUpdates`.** This is the mechanism that enables atomic album batching on Telegram — the new loop processes raw batches and groups album members deterministically by `media_group_id` instead of relying on a debounce timer. The outer reconnect-on-panic loop with exponential backoff is preserved, and transient `getUpdates` errors retry inline with their own bounded backoff.
- **Codex `image_gen` output is now auto-delivered.** Codex's built-in `image_gen` tool writes generated images to `~/.codex/generated_images/<session_id>/` without surfacing any tool event in `--json` output, so previously the user saw nothing. cokacdir now snapshots the directory at turn start and, when the turn ends, scans for new files (mtime ≥ turn-start, not in snapshot, not already delivered by a model-issued `--sendfile` in this turn) and runs `cokacdir --sendfile` for each. Synthetic `ToolUse`/`ToolResult` events are emitted so the bot UI renders the delivery exactly like a model-issued sendfile. Codex-only — Claude Code, Gemini, and OpenCode are unaffected.
- **Schedule history `bot_key` migrated to a non-secret SHA-256 verifier.** `~/.cokacdir/schedule/*.json` no longer stores the raw `bot_key` field on disk; instead a domain-separated verifier `SHA-256("cokacdir:live_schedule:v1\0" + id + chat_id + bot_key)` is stored. Pre-migration files are read transparently and rewritten on the next legitimate update. The schedule run-history files (`~/.cokacdir/schedule_history/<id>.log`) use a separate domain (`"cokacdir:schedule_history:v1\0"`) so a verifier from one system cannot authorize the other. History writes are now serialized via an `fs2` flock (`<id>.log.lock`), and a one-time idempotent legacy redaction marker (`<id>.log.redacted`) ensures plaintext keys are stripped exactly once. All cron/msg debug logs that previously printed the raw `bot_key` now print `key_supplied=true` or `<redacted>`.
- **`write_schedule_entry_pub` rejects empty `bot_key`.** `list_schedule_entries_pub` returns `ScheduleEntryData` with `bot_key=""` (since the raw key is no longer recoverable from disk), so any list-then-modify-then-write code path must re-supply the raw key from the CLI `--key` argument before writing. The new guard turns silent schedule orphaning into an explicit error. `handle_cron_update` now restores the raw key from `--key` before calling write, fixing what would otherwise have been a regression introduced by the verifier migration.
- **`--cron-history` now sanitizes records and triggers a one-time legacy redaction.** Reading a schedule's history file lazily redacts any pre-migration `bot_key` plaintext, then strips both `bot_key` and `bot_key_verifier` from each record before returning to the caller, so the JSON output exposes no key material.
- **Codex MCP / Collab / WebSearch event handling polished.**
  - MCP `tool_call` results now respect the codex `status` field — `status == "failed"` flags the result as `is_error=true` even when a partial result payload is present, and a synthetic error result is emitted when neither `error` nor `result` is populated.
  - Collab tool agent states (`pending_init` / `running` / `interrupted` / `completed` / `errored` / `shutdown` / `not_found`) are now rendered with `[status]` prefixes for problematic states and the `ToolResult` is marked `is_error` if any agent failed; healthy agent messages keep the prior message-only UX.
  - WebSearch decodes the action-tagged enum (`search` / `open_page` / `find_in_page` / `other`) instead of always falling back to `action.queries`. Empty displays no longer emit a bare ToolUse.
- **Slack-specific operational bits.** Per-channel rate limit (~1.1s) is enforced via a `last_post_at` map. Channel ⇄ chat_id mapping persists at `~/.cokacdir/bridge_maps/slack_<token_hash>.json` (atomic temp-rename) so scheduled tasks reach the right channel after restart. `app_mention` and `message.*` events for the same `ts` are deduplicated via a bounded `claim_incoming_event` set. File uploads use the Slack `files.completeUploadExternal` flow with a pending-mapping registered before completion so the auto-posted `file_share` event can attach the real `ts` for later edit/delete.

---

## 0.5.2 — 2026-04-28

- New `--cron-history <SID> --chat <ID> --key <HASH>` command — inspect the JSONL run-history file of a schedule. Each cron firing now appends a record (`ts, schedule_id, chat_id, prompt, status (ok|cancelled|error), response (capped at 4 KB), workspace_path, duration_ms, error?`) to `~/.cokacdir/schedule_history/<id>.log`. Authorization prefers the live schedule entry's `(chat_id, bot_key)` match, but falls back to the first record in the history file when the live entry is gone (one-time / `--once` schedules already auto-deleted), so already-executed schedules can still be inspected.
- The `--cron` JSON response now includes a `hint` field with the exact `--cron-history` invocation bound to that schedule's ID. This gives the AI a deterministic in-output mapping ("for THIS id, run THIS exact command"), useful when the user refers to the schedule by natural-language phrases like "방금 한 거" without naming the id.
- `--cron-remove` now also deletes the schedule's run-history file, so a future schedule that happens to receive the same 8-char ID does not inherit prior history.

---

## 0.5.1 — 2026-04-28

- **Auto-created workspaces now announce themselves.** When the bot lazily creates a workspace under `~/.cokacdir/workspace/<id>/` on the first message after `/clear`, a `/model` provider switch, or a fresh chat, it now sends a `Workspace auto-started at <path>. Use /<id> to resume this session.` notification before processing the message. Previously, users had to type `/pwd` to discover where the AI was operating, which was easy to miss for the very first message in a new workspace. The notification fires only when the workspace was actually newly created — concurrent-message races that find an existing session do not double-notify.

---

## 0.5.0 — 2026-04-28

- **`/clear`, `/model`, and `/start` now correctly cancel in-flight work and uploads.** Previously, `/clear` and provider switches via `/model` only blanked the in-memory session, while an in-flight AI task was free to keep running and eventually write its response (and a stale session_id from the old provider) back into the just-cleared session — partially resurrecting what the user explicitly cleared. The same issue applied to `/start` when it switched workspaces. Now all three commands cancel the in-progress task, drop queued messages, clear pending file uploads (when the path actually changes), and stop any active `/loop` verification before mutating session state. `/loop`'s post-verify outcome messages also re-check `loop_states` under the lock so `/clear` or `/model` arriving mid-verification suppress the trailing "Loop complete" / "Loop limit" / re-inject message uniformly.
- **Brand-new-session `/clear` race detection.** A brand-new session has `session_id = None`, so the previous "writeback only if session_id matches" guard could not detect `/clear` on a fresh session whose first message was still being processed. A monotonic per-chat `clear_epoch` counter is now bumped on every `/clear` and captured at task spawn; the post-completion guard skips the writeback whenever the epoch advances during the task. The guard also compares the (provider, path, session_id) triple to catch `/model` provider switches and `/start` same-path session-id swaps. Applied to all four polling sites (text-message and bot-to-bot, normal completion and stopped branches).
- **`/start` identifies path-vs-session intent and adds a same-path no-op.** Typing `/start <path>` at the path you are already in now responds with `Already at <path>.` and does nothing else, instead of clearing pending uploads, nulling `session_id`, and reloading history from disk over your in-progress state. Session-identifier inputs (`/start <session-id>`) intentionally still proceed even when the session resolves to the current cwd, since the user may be switching to a different session at the same path. Cross-provider fallback inside `/start` also runs the same cancel/cleanup flow as `/model`.
- **`/model` provider switch now shows what was reset and where.** A `Provider changed — previous workspace, history, and uploads have been reset for compatibility. Previous workspace: <path> (preserved on disk). A new workspace will be created on your next message. To resume work in the previous workspace instead, use /start <path>.` notice now appears whenever a `/model` command crosses provider boundaries with non-empty session state. The count of any queued messages that were dropped is also reported.
- **`/down` now expands `~`.** Paths starting with `~/`, `~\`, or just `~` are resolved against the user's home directory before download. `~user/`, `~~/`, and embedded `~` are intentionally left alone.
- **`/model` provider comparison aligned with the polling guard.** Internally switched from prefix-only `provider_from_model` to availability-aware `detect_provider`, so a chat with no explicit model that was running on a CLI fallback (e.g. Codex when Claude is unavailable) now correctly recognizes `/model claude` as a provider change and runs the cleanup flow. Without this fix, the writeback guard's spawn-time capture (which already used `detect_provider`) would disagree with `/model`'s comparison and the cleanup would be skipped.
- New `src/utils/path.rs` module with a conservative `expand_tilde` helper backed by unit tests for `~`, `~/`, `~\`, `~user/`, `~~/`, and embedded-`~` cases.

---

## 0.4.99 — 2026-04-25

- **Telegram Flood Control responses are now honored.** When the Telegram server returns `RetryAfter` on a high-frequency spinner edit, the bot now pushes the per-chat next-call timestamp forward by the server-mandated duration so that subsequent `shared_rate_limit_wait` calls naturally wait out the full cooldown instead of firing again after the normal `polling_time_ms` gap. Previously, ignoring `RetryAfter` could cause the cooldown to escalate over repeated violations (production logs showed bans accumulating to ~14000s). Applied to the five spinner-edit sites that fire every polling cycle: shell command spinner, AI streaming spinner (text and bot-to-bot polling loops), schedule spinner, and the verify spinner. The shared rate-limit serialization itself is unchanged.

---

## 0.4.98 — 2026-04-25

- **Gemini CLI `--skip-trust` auto-detection.** The bridge now probes `gemini --version` once on first use and adds `--skip-trust` to the gemini-cli invocation only when the installed version supports it (stable ≥ 0.39.1, preview ≥ 0.40.0-preview.3, or nightly built on/after 2026-04-23 — PR google-gemini/gemini-cli#25814). Older versions silently keep the previous behavior so they don't error out on an unknown flag. The decision is propagated from the parent cokacdir process to the `--bridge gemini` subprocess via the internal `COKAC_GEMINI_SKIP_TRUST` env var, which is stripped before spawning gemini-cli itself.
- Bot server startup now prints the detected Gemini CLI version and `--skip-trust` capability (e.g. `▸ Gemini : v0.40.0 (+--skip-trust)`).
- `/model` help now lists `codex:gpt-5.5` as the latest frontier coding model; `gpt-5.4` remains available and is relabeled "Frontier agentic coding model".

---

## 0.4.97 — 2026-04-25

- **`/queue` OFF behavior changed: reject → redirect.** Previously, sending a message while the AI was busy with `/queue` OFF returned "AI request in progress" and dropped the message. Now, that same message cancels the in-progress task and is processed immediately on the same session — natural mid-task redirects ("아니 그거 말고 X 해줘") just work. Plain text, `;text`, `/query <text>`, and captioned file uploads trigger redirect; slash commands (`/help`, `/start`, …) and shell commands (`!cmd`) keep the existing rejection so an unrelated command never kills a long-running task. If a second redirect arrives while the first is still cancelling, the latest one wins (replaces the pending target). `/queue` ON (the default) is unchanged — messages still queue FIFO. `/stop`/`/stopall` semantics are unchanged. Resolves [#34](https://github.com/kstost/cokacdir/issues/34); thanks to [@twpark](https://github.com/twpark) for [#36](https://github.com/kstost/cokacdir/pull/36) which proposed the redirect approach.

---

## 0.4.92 — 2026-04-17

- **`/loop` now works with Codex and OpenCode**, not just Claude. After each turn the bot still asks the AI to judge whether the task is fully done and re-injects remaining work until it is, but the verification mechanics are now provider-specific: Claude uses its native `--fork-session`; Codex replays a full-fidelity session archive into an isolated `codex exec --ephemeral` call that never touches the original rollout file; OpenCode uses `opencode run --session <id> --fork --agent plan`. Gemini still falls back with a clear message.
- New full-fidelity session archive at `~/.cokacdir/ai_sessions_full/{session_id}.json` — parallel to the existing truncated UI summary. Preserves all text, tool arguments, tool results, timestamps, model info, and token usage for Claude/Codex/Gemini/OpenCode sessions. Used by the Codex verifier; written automatically alongside the summary.
- The `/loop` verification progress indicator is now an animated 🔍/🔎 spinner that cycles letter-by-letter while the verifier runs.
- Fixed: `/model` help listed Opus as 4.6; now correctly shows Opus 4.7.

---

## 0.4.89 — 2026-04-15

- New `/setendhook <message>` command — set a custom notification message that is sent as a separate message whenever AI processing completes. Useful as an alert when waiting for long responses. Use `/setendhook_clear` to remove. Applies to all processing types: normal AI responses, shell commands, scheduled tasks, and bot-to-bot messages. Not sent when the request is cancelled via `/stop`.

---

## 0.4.88 — 2026-04-15

- **File copy now preserves timestamps.** All copy operations (single file, directory recursive, paste) now retain the original modification and access times using the `filetime` crate. Directory timestamps are set after contents are fully copied to avoid being overwritten by child writes.
- **Codex streaming: improved tool display.** Codex `file_change` events now emit a ToolResult summary listing each changed file and its kind (add/update/delete). `collab_tool_call` events display human-readable prompts for spawn/send/followup tools and extract agent response messages from `agents_states` on wait/close. `web_search` events show the actual query text (or expanded queries) instead of raw JSON. `command_execution` error detection now also checks the `status` field for "failed"/"declined".
- Fixed: Codex Collab tool display showed redundant text like "Agent wait: wait" instead of "Agent: wait" for tools whose display string equalled the tool name.
- Fixed: Codex web_search with an empty `action.queries` array would lose the original query text, showing a bare "Search" label instead of the query.

---

## 0.4.85 — 2026-04-11

- **OpenCode background tasks now actually complete.** When using the oh-my-opencode plugin, messages that dispatched a background task (e.g. "I'll report back when it's done") previously left the turn hanging forever because the one-shot `opencode run` process was torn down as soon as the parent session went idle, interrupting the background sub-session mid-flight. The OpenCode adapter was reworked to spawn `opencode serve` per turn, drive the session over HTTP + SSE, and wait until the parent session, all child sessions, and all todos are idle before shutting down — so background task notifications make it back to the user and the final answer is delivered end-to-end.
- Fixed: OpenCode `--session <id>` was silently ignored when combined with `--continue`, causing cross-session routing into whichever root session was most recent. `--continue` is no longer passed alongside `--session`.
- Fixed: OpenCode responses that ended with a legitimate non-"stop" finish reason (`length`, `content-filter`, `error`) were misreported as "empty response" errors. These are now treated as terminal like OpenCode itself does.
- Fixed: a recoverable OpenCode error (e.g. `ContextOverflowError` that auto-compaction recovers from) could poison an otherwise successful turn. Error events are now tentative until the turn ends and are only surfaced when no usable output arrived.
- Fixed: OpenCode calls with a stale `--session` id used to exit cleanly with an empty stdout while writing `NotFoundError` to stderr, surfacing as a confusing "empty response". The stderr message is now reported as the actual error.
- Improved: OpenCode empty-response diagnostics now include the last finish reason, event/tool counters, last event type, output-token count, and exit code, making it possible to tell at a glance why a turn produced no text.
- The legacy `opencode run` path is preserved and can be forced with `COKACDIR_OPENCODE_LEGACY=1` as a rollback escape hatch.

---

## 0.4.84 — 2026-04-10

- Fixed: streaming AI responses could panic with "byte index is not a char boundary" when a multi-byte character (emoji, CJK text) happened to straddle the rolling-placeholder threshold or when `full_response` was replaced by an error message mid-stream. All nine `full_response` slicing sites across the text, schedule, and bot-to-bot polling loops now floor to a valid UTF-8 char boundary and reset `last_confirmed_len` if it no longer points at a valid boundary in the current response.

---

## 0.4.83 — 2026-04-10

- New `/envvars` command — dump all environment variables visible to the bot process (bot-owner only). Useful for verifying which overrides are active. ⚠ Exposes sensitive values with no redaction — use in a 1:1 chat only.
- New startup loader for `~/.cokacdir/.env.json` — values from this file are injected into the process environment at launch and take priority over shell-exported values. Supports string, number, and boolean values at the root JSON object.
- New `COKAC_CLAUDE_PATH` environment variable — override the path to the Claude CLI binary instead of relying on `which claude` / `SearchPathW`.
- New `COKAC_CODEX_PATH` environment variable — same as above for the Codex CLI binary.
- New `COKAC_FILE_ATTACH_THRESHOLD` environment variable — tune the byte threshold (default 8192) at which long AI responses switch to `.txt` file attachment mode, introduced in 0.4.81.
- Documented the pre-existing `COKAC_GEMINI_PATH`, `COKAC_OPENCODE_PATH`, and `COKACDIR_DEBUG` environment variables. See the new "Environment Variables" guide in the docs for the full reference.
- Fixed: CLI-binary path resolution for Claude, Codex, Gemini, and Opencode now verifies the resolved path actually exists on disk before returning it. Previously, a stale `which` result or a `COKAC_*_PATH` pointing at a deleted file would be accepted and then fail later at spawn time. The multi-panel file manager's CLI availability check was hardened the same way.
- Fixed: when switching to a previously-saved workspace, a stale `session_id` from the prior workspace could leak into the newly-restored session. The in-memory `session.session_id` is now explicitly cleared before restoration.

---

## 0.4.82 — 2026-04-03

- New `/usechrome` command — toggle Chrome browser tool (`--chrome`) for Claude CLI per chat.

---

## 0.4.81 — 2026-04-03

- **Very long AI responses are now sent as a file attachment** instead of flooding the chat with many consecutive messages. Responses over ~8,000 characters are delivered as a downloadable `.txt` file.
- This applies everywhere: normal responses, stopped/cancelled responses, scheduled tasks, and bot-to-bot messages.

---

## 0.4.79 — 2026-04-02

- Updated the built-in schedule documentation to be simpler and more user-friendly.

---

## 0.4.78 — 2026-04-02

- **The bot now knows how to answer "how to" questions** — built-in documentation (14 help guides) is deployed to `~/.cokacdir/docs/` on startup and the AI references them when you ask for help.
- Fixed Discord `<@ID>` mentions being passed as raw text — they are now shown as readable `@username` format.
- Removed outdated internal design documents.

---

## 0.4.77 — 2026-04-02

- **Discord bot support added.** You can now use Discord bot tokens with `--ccserver`. Token type (Telegram vs Discord) is auto-detected, or you can prefix with `discord:` explicitly.
- Telegram and Discord bots can run simultaneously in the same server.
- All existing features (AI chat, file upload, schedules, group collaboration) work on Discord.
- Co-work guidelines for multi-bot group chats can now be customized by editing `~/.cokacdir/prompt/cowork.md`.

---

## 0.4.76 — 2026-03-31

- **You can now upload videos, voice messages, audio, GIFs, and video notes** — previously only documents and photos were supported.
- **No more `/start` required** — sending a message or file automatically creates a workspace if none exists.
- New `/greeting` command to switch between a compact and full startup message.
- Files with duplicate names are automatically renamed (e.g., `file(1).txt`) instead of being overwritten.
- Files larger than 20 MB are rejected with a clear error message.
- Shell commands are now properly blocked while the AI is busy.

---

## 0.4.75 — 2026-03-29

- When the model list is too long for a Telegram message, it is now sent as a text file attachment.

---

## 0.4.74 — 2026-03-29

- Fixed unnecessary request serialization in private chats introduced in 0.4.71.

---

## 0.4.73 — 2026-03-29

- `/stop_ID` no longer sends a confusing "not found" error when the queued message was already processed.

---

## 0.4.72 — 2026-03-29

- Changed the cancel command format from `/stop ID` to `/stop_ID` so it works as a tappable link in Telegram.

---

## 0.4.71 — 2026-03-29

- **Message queue**: Messages sent while the AI is busy are now automatically queued (up to 20) and processed in order. No more "busy" rejections.
- New `/stopall` command — cancels the current AI request and clears all queued messages.
- New `/stop_ID` command — cancel a specific queued message by its ID.
- New `/queue` command — toggle queue mode on/off (on by default).

---

## 0.4.69 — 2026-03-28

- Fixed a potential deadlock when checking group chat context settings.

---

## 0.4.67 — 2026-03-26

- **Bots in group chats now see who else is in the chat**, improving multi-bot awareness.
- Bots now understand that @mentioning another bot in chat text doesn't work — they must use the `--message` command to talk to each other.
- Improved Gemini CLI output parsing for edge cases.

---

## 0.4.66 — 2026-03-25

- **OpenCode AI backend added** — you can now use any model configured in OpenCode via Telegram bot.
- **Gemini AI backend added** — Google's Gemini models are now available as an AI provider.
- Session resume now works across all four providers (Claude, Codex, Gemini, OpenCode).
- Incoming Telegram messages are now logged to `~/.cokacdir/logs/` for diagnostics.
- Bot startup now flushes any pending messages from previous runs to avoid processing stale requests.

---

## 0.4.65 — 2026-03-25

- Tool names from Gemini and OpenCode are now shown in familiar format (e.g., "Bash", "Read", "Edit" instead of their native names).
- Session resume now tries all available AI providers as fallback.
- Startup message now includes community links.

---

## 0.4.64 — 2026-03-24

- **Initial Gemini and OpenCode support** — experimental integration of two new AI providers alongside Claude and Codex.
- Server startup now shows availability status for all providers.

---

## 0.4.63 — 2026-03-23

- Fixed Claude/Codex not starting in non-interactive environments (cron jobs, launchd, SSH) by automatically adding the binary's directory to PATH.

---

## 0.4.62 — 2026-03-23

- **Fixed Windows path issues for Korean (and other non-ASCII) usernames** — paths are now resolved using native Windows APIs.

---

## 0.4.61 — 2026-03-23

- **New `/context` command for group chats** — control how many recent messages the AI sees (e.g., `/context 20` for more history, `/context 0` to disable). Default is 12.

---

## 0.4.60 — 2026-03-23

- Improved @mention routing in group chats — messages addressed to another bot are now correctly ignored, even in direct mode.
- Fixed tool errors cluttering chat output in silent mode.
- Fixed chat log growing exponentially when bots read each other's logs.

---

## 0.4.59 — 2026-03-22

- Long tool output in group chat logs is now truncated to prevent log bloat (full content saved separately).

---

## 0.4.58 — 2026-03-22

- **Group chat log now shows readable summaries** instead of raw internal data when using `--read_chat_log`.

---

## 0.4.57 — 2026-03-21

- Fixed Claude CLI not being found on Windows when both `.cmd` and extensionless versions exist.

---

## 0.4.56 — 2026-03-21

- **File uploads in group chats can now be directed to a specific bot** using `@botname` in the caption.
- Caption text is automatically sent to the AI, so you can upload a file and ask about it in one step.

---

## 0.4.55 — 2026-03-17

- **Bots in group chats now detect when another bot already answered** and avoid repeating the same response — they add new information or acknowledge and move on instead.
- Group chat context increased from 5 to 12 recent entries.

---

## 0.4.53 — 2026-03-17

- Fixed a race condition where multiple bots saving settings simultaneously could corrupt the shared settings file.

---

## 0.4.52 — 2026-03-17

- Codex sessions now properly handle system prompts for both new and resumed sessions.
- Bot now automatically reconnects if the Telegram connection drops (with backoff).

---

## 0.4.51 — 2026-03-16

- **Codex session resume** — conversation history is now preserved across messages instead of starting fresh each time.

---

## 0.4.50 — 2026-03-16

- Fixed file locking issues on Windows that affected debug logging and group chat logs.

---

## 0.4.49 — 2026-03-15

- Fixed a crash ("Argument list too long") that could happen when the system prompt was very large.

---

## 0.4.48 — 2026-03-15

- **Group chat bot coordination** — bots now take turns processing messages, preventing race conditions.
- **Location sharing** — you can share your GPS location or a venue with the bot.
- **Real-time progress in group chats** — long responses are delivered incrementally instead of all at once.
- Bots are now instructed to keep group chat responses short and avoid repeating what others said.
- Fixed `/stop` race condition where the AI could sneak in a new request before cancellation took effect.

---

## 0.4.47 — 2026-03-14

- **Group chat shared log** — bots in the same group can now see each other's conversations and coordinate.
- **Bot-to-bot messaging** — bots can send direct messages to each other using the `--message` command.
- New commands: `/direct` (toggle prefix requirement in groups), `/silent` (toggle streaming output), `/instruction` (set custom AI instructions).
- **Scheduler** — schedule tasks to run at specific times or on recurring cron schedules.

---

## 0.4.46 — 2026-03-13

- Bots now automatically see the 5 most recent group chat log entries, improving context awareness without manual log reading.
- `/clear` now marks the log so other bots skip old history.
- Bots display their name alongside @username in the group chat log.

---

## 0.4.45 — 2026-03-13

- Group chat log now records full AI output including tool calls, giving bots richer context about what each bot did.

---

## 0.4.44 — 2026-03-12

- Improved group chat log filtering and bot message delivery instructions.

---

## 0.4.43 — 2026-03-13

- **Group chat support** — multiple bots in the same Telegram group can now see each other's conversations.
- **Direct mode** (`/direct`) — in group chats, the `;` prefix is no longer required when direct mode is on.
- **Custom instructions** (`/instruction`) — set persistent AI instructions per chat.
- **Cross-provider session resume** — `/start` now falls back to other AI providers if the session was created with a different one.

---

## 0.4.42 — 2026-03-11

- Added `/session` command — view your current session ID and get a ready-to-paste terminal command to resume it locally.

---

## 0.4.41 — 2026-03-10

- Added vim-style navigation keys (`j`/`k`/`h`/`l`) in the file manager.
- Updated Codex model list with latest models.

---

## Earlier Versions — 2026-01-27 ~ 2026-03-08

> Initial development period. Major milestones:

- **Full Rust rewrite** from TypeScript/React — complete TUI file manager with dual-panel browsing.
- **Claude AI integration** — natural language commands, streaming responses, session management.
- **Telegram bot** — remote AI chat, file upload/download, session management.
- **Codex CLI support** — OpenAI Codex as alternative AI backend.
- **Built-in file viewer/editor** with syntax highlighting and markdown rendering.
- **SSH/SFTP** remote file management.
- **File encryption** (AES-256-CBC).
- **Git integration** — status, log, diff viewer.
- **Theme system** — customizable JSON themes in `~/.cokacdir/themes/`.
- **Scheduler** — absolute time and cron-based task scheduling.
- **Windows support** — native builds with PowerShell path detection.
- **Project website** launched at https://cokacdir.cokac.com.
