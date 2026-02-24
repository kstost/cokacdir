// English localization strings.

// Session
pub(super) const MSG_NO_SESSION: &str =
    "No active session. Use /start <path> first.";
pub(super) const MSG_SESSION_CLEARED: &str = "Session cleared.";

// AI busy
pub(super) const MSG_AI_BUSY: &str =
    "AI request in progress. Use /stop to cancel.";

// Permission
pub(super) const MSG_PERMISSION_DENIED: &str = "Permission denied.";

// Stop
pub(super) const MSG_STOPPING: &str = "Stopping...";
pub(super) const MSG_NO_ACTIVE_REQUEST: &str = "No active request to stop.";

// Public
pub(super) const MSG_GROUP_ONLY: &str =
    "This command is only available in group chats.";
pub(super) const MSG_PUBLIC_OWNER_ONLY: &str =
    "Only the bot owner can change public access settings.";
pub(super) const MSG_PUBLIC_ON: &str =
    "✅ Public access <b>enabled</b> for this group.\nAll members can now use the bot.";
pub(super) const MSG_PUBLIC_OFF: &str =
    "❌ Public access <b>disabled</b> for this group.\nOnly the owner can use the bot.";
pub(super) const MSG_PUBLIC_STATUS_ENABLED: &str = "enabled";
pub(super) const MSG_PUBLIC_STATUS_DISABLED: &str = "disabled";
pub(super) const MSG_PUBLIC_STATUS: &str =
    "Public access is currently <b>{}</b> for this group.\n\n<code>/public on</code> — Allow all members\n<code>/public off</code> — Owner only";
pub(super) const MSG_PUBLIC_USAGE: &str =
    "Usage:\n<code>/public on</code> — Allow all group members\n<code>/public off</code> — Owner only";

// Language
pub(super) const MSG_LANG_CHANGED: &str = "✅ Language set to <b>English</b>.";
pub(super) const MSG_LANG_USAGE: &str =
    "Usage: <code>/lang ko</code> or <code>/lang en</code>";

// Shell
pub(super) const MSG_SHELL_USAGE: &str =
    "Usage: !<command>\nExample: !mkdir /home/user/testcode";
pub(super) const MSG_SHELL_TIMEOUT: &str = "Command execution timed out (60s limit)";
pub(super) const MSG_SHELL_PROCESSING: &str = "Processing !{}...";

// Down (file download)
pub(super) const MSG_DOWN_USAGE: &str =
    "Usage: /down <filepath>\nExample: /down /home/user/file.txt";
pub(super) const MSG_DOWN_NO_SESSION: &str =
    "No active session. Use absolute path or /start <path> first.";

// File ops
pub(super) const MSG_FILE_SAVE_FAILED: &str = "Failed to save file: {}";
pub(super) const MSG_SANDBOX_DENIED: &str =
    "Error: path is outside the allowed sandbox (home directory).";

// Errors
pub(super) const MSG_ERROR_HOME: &str =
    "Error: cannot determine home directory.";
pub(super) const MSG_ERROR_INVALID_DIR: &str =
    "Error: '{}' is not a valid directory.";
pub(super) const MSG_ERROR_CREATE_WORKSPACE: &str =
    "Error: failed to create workspace: {}";

// Help text
pub(super) fn help_text() -> &'static str {
    "\
<b>cokacdir Telegram Bot</b>
Manage server files &amp; chat with Claude AI.

<b>Session</b>
<code>/start &lt;path&gt;</code> — Start session at directory
<code>/start</code> — Start with auto-generated workspace
<code>/pwd</code> — Show current working directory
<code>/clear</code> — Clear AI conversation history
<code>/stop</code> — Stop current AI request

<b>File Transfer</b>
<code>/down &lt;file&gt;</code> — Download file from server
Send a file/photo — Upload to session directory

<b>Shell</b>
<code>!&lt;command&gt;</code> — Run shell command directly
  e.g. <code>!ls -la</code>, <code>!git status</code>

<b>AI Chat</b>
Any other message is sent to Claude AI.
AI can read, edit, and run commands in your session.

<b>Tool Management</b>
<code>/availabletools</code> — List all available tools
<code>/allowedtools</code> — Show currently allowed tools
<code>/allowed +name</code> — Add tool (e.g. <code>/allowed +Bash</code>)
<code>/allowed -name</code> — Remove tool
<code>/allowed +a -b +c</code> — Multiple at once

<b>Group Chat</b>
<code>;</code><i>message</i> — Send message to AI
<code>;</code><i>caption</i> — Upload file with AI prompt
<code>/public on</code> — Allow all members to use bot
<code>/public off</code> — Owner only (default)

<b>Language</b>
<code>/lang ko</code> — 한국어
<code>/lang en</code> — English

<b>Settings</b>
<code>/setpollingtime &lt;ms&gt;</code> — Set API polling interval
  Too low may cause Telegram API rate limits.
  Minimum 2500ms, recommended 3000ms+.
<code>/debug</code> — Toggle API debug logging

<code>/help</code> — Show this help"
}
