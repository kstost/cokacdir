# COKACDIR

**Your coding agent, already in use — on Telegram, Discord, and Slack**

cokacdir is not an AI agent — it does not include an LLM or reasoning engine. Instead, it delegates tasks to the coding agent you are already using (Claude Code, Codex CLI, Antigravity CLI `agy`, OpenCode) and lets you control it from chat apps such as Telegram, Discord, and Slack. Just send a message to the bot, and the agent will handle code execution, file editing, shell commands, and real-time streaming of results from your phone or desktop.

It runs within each agent’s existing subscription (or free tier), so there are **no additional API costs**.

## Quick Start

**macOS / Linux:**

```bash
curl -fsSL https://cokacdir.cokac.com/manage.sh | bash && cokacctl
```

**Windows (run PowerShell as Administrator.):**

```powershell
irm https://cokacdir.cokac.com/manage.ps1 | iex; cokacctl
```

Running the command will open the cokacdir management TUI. Then:

1. Press **`i`** to install cokacdir
2. After installation completes, enter a bot token for Telegram, Discord, or Slack
3. Press **`s`** to start the server

That’s it — open your chat app and start chatting with your bot.

## Key Features

* **Blazing-fast performance**: Written in Rust for maximum performance. A single binary (15–20MB depending on platform), optimized with LTO and strip.
* **AI-powered commands**: Natural-language coding and file management powered by Claude, Codex, Agy, and OpenCode. Press `.` and describe what you want done.
* **Multi-panel navigation**: A dynamic multi-panel interface for efficient file management
* **Keyboard-first**: Full keyboard navigation for power users
* **Built-in editor**: File editing with syntax highlighting for more than 20 languages
* **Image viewer**: View images directly in the terminal (Kitty, iTerm2, Sixel protocols), with zoom and pan support
* **Process manager**: Monitor and manage system processes with sortable columns
* **File search**: Recursive file search by name pattern
* **Diff**: Side-by-side comparison of folders and files
* **Git integration**: Built-in git status, commit, log, branch management, and diff between commits
* **Remote SSH/SFTP**: Explore remote servers over SSH/SFTP with saved profiles
* **File encryption**: AES-256 encryption with configurable chunk splitting
* **Duplicate file detection**: Detect and manage duplicate files using hash-based comparison
* **Chat bots**: Remotely control AI coding sessions through Telegram, Discord, or Slack with streaming output, queued requests, scheduled tasks, rich Telegram responses, and companion mode
* **Customizable themes**: Light and dark themes with full JSON-based color customization

## Community

Telegram group for tips, updates, and support:
**[@cokacvibe](https://t.me/cokacvibe)**

## Documentation

For AI provider setup, keyboard shortcuts, and detailed documentation, visit:
**[https://cokacdir.cokac.com](https://cokacdir.cokac.com)**

For the measured Antigravity CLI integration contract, see [How cokacdir Uses Antigravity CLI (`agy`)](docs/how-to-use-agy-antigravity.md).

## Chat Bots

**Features:**

* Multi-provider support (Claude, Codex, Agy, OpenCode) with real-time streaming
* Session persistence and cross-provider session interpretation
* Scheduled tasks using cron expressions or absolute time
* Request queue and cancellation controls (`/queue`, `/stop`, `/stopall`, `/stop_<id>`)
* Group chat support where multiple bots share context
* Bot-to-bot messaging for multi-agent workflows
* Output controls including `/silent`, Telegram Rich Messages, Codex fast tier, and Claude Chrome mode
* Companion mode with optional owner-only proactive pings and Codex-generated visible companion images
* File upload/download, tool management, debug logging

**Commands:** `/start`, `/stop`, `/stopall`, `/clear`, `/help`, `/session`, `/pwd`, `/model`, `/stt_model`, `/effort`, `/fast`, `/down`, `/instruction`, `/instruction_clear`, `/allowed`, `/allowedtools`, `/availabletools`, `/queue`, `/contextlevel`, `/query`, `/loop`, `/setendhook`, `/setendhook_clear`, `/public`, `/direct`, `/setpollingtime`, `/debug`, `/envvars`, `/silent`, `/rich`, `/companion`, `/companion_profile`, `/companion_profile_clear`, `/companion_visible`, `/companion_ping`, `/usechrome`

## Configuration

cokacdir reads environment variables at startup to override binary paths (`COKAC_CLAUDE_PATH`, `COKAC_CODEX_PATH`, `COKAC_AGY_PATH`, `COKAC_OPENCODE_PATH`), tune the file-attachment threshold (`COKAC_FILE_ATTACH_THRESHOLD`), switch schedules into inline chat-session mode (`COKAC_SCHEDULE_INLINE=1`), and enable debug logging (`COKACDIR_DEBUG=1`). Variables can be set either in your shell environment or in a JSON file at `~/.cokacdir/.env.json` (values in that file take priority). Use the `/envvars` chat command (bot-owner only, 1:1 chat only) to inspect which values are active in the running process. See the [Environment Variables guide](https://cokacdir.cokac.com/#/docs/env-vars) for the full reference.

## Supported Platforms

* macOS (Apple Silicon & Intel)
* Linux (x86_64 & ARM64)
* Windows (x86_64 & ARM64)

## License

`cokacdir` is distributed under the MIT License. See [LICENSE](LICENSE).

Copyright (c) 2026 cokac.

Telegram STT support downloads and spawns the MIT-licensed `transcriptor`
binary, which uses OpenAI Whisper model artifacts converted for whisper.cpp.
Runtime STT dependencies, upstream notices, model limitations, and audio consent
considerations are summarized in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Author

cokac
[monogatree@gmail.com](mailto:monogatree@gmail.com)

Homepage: [https://cokacdir.cokac.com](https://cokacdir.cokac.com)

## Disclaimer

THIS SOFTWARE IS PROVIDED “AS IS,” WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE, AND NON-INFRINGEMENT.

IN NO EVENT SHALL THE AUTHOR, COPYRIGHT HOLDERS, OR CONTRIBUTORS BE LIABLE FOR ANY CLAIM, DAMAGES, OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT, OR OTHERWISE, ARISING FROM, OUT OF, OR IN CONNECTION WITH THE SOFTWARE OR THE USE OF THE SOFTWARE.

This includes, but is not limited to:

* Data loss or corruption
* System damage or malfunction
* Security breaches or vulnerabilities
* Financial loss
* Direct, indirect, incidental, special, punitive, or consequential damages

The user assumes full responsibility for all consequences arising from the use of this software, whether such use was intended, authorized, or foreseeable.

**ALL RISKS ASSOCIATED WITH USE ARE BORNE BY THE USER**
