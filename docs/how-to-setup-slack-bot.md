# Slack Bot Setup Guide

cokacdir's Slack backend uses **Socket Mode**, so no public HTTPS endpoint is required — the bot connects to Slack over a WebSocket.

## 1. Create a Slack App

1. Go to https://api.slack.com/apps
2. Click **Create New App** → **From scratch**
3. Enter an app name (e.g. `cokacbot3`) and pick the target workspace
4. Click **Create App**

## 2. Enable Socket Mode

1. Select **Socket Mode** from the left menu
2. Turn **Enable Socket Mode** on
3. When prompted, generate an **App-Level Token** with the `connections:write` scope
4. Copy and save the token (`xapp-...`) — it is shown only once

## 3. Subscribe to Events

1. Select **Event Subscriptions** from the left menu
2. Turn **Enable Events** on (Request URL is unused — Socket Mode replaces it)
3. Under **Subscribe to bot events**, add:
   - `app_mention` — receive messages where the bot is @-mentioned
   - `message.im` — direct messages with the bot
   - `message.channels` — public-channel messages
   - `message.groups` — private-channel messages
   - `message.mpim` — group DM messages

> The `message.*` events deliver every message in any channel the bot is in. If you only want the bot to react to mentions, drop `message.channels` / `message.groups` / `message.mpim` and keep just `app_mention` and `message.im`.

## 4. Add OAuth Scopes

1. Select **OAuth & Permissions** from the left menu
2. Under **Bot Token Scopes**, confirm the read scopes auto-added by step 3:
   - `app_mentions:read`
   - `im:history`
   - `channels:history`
   - `groups:history`
   - `mpim:history`
3. Add the write scopes the bot needs:
   - `chat:write` — send messages (**required**)
   - `files:write` — upload files (optional)
   - `chat:write.public` — post to public channels the bot has not been invited to (optional)

> Whenever you add or remove a scope, Slack requires you to **Reinstall to Workspace** before the new scope applies to the bot token.

## 5. Install to Workspace

1. At the top of **OAuth & Permissions**, click **Install to Workspace**
2. Approve the permission prompt
3. Copy and save the **Bot User OAuth Token** (`xoxb-...`)

## 6. Enable DM Input on App Home

1. Select **App Home** from the left menu
2. Under **Show Tabs**, turn **Messages Tab** on
3. Turn **Allow users to send Slash commands and messages from the messages tab** on

> Without the second toggle, the DM composer shows *"Sending messages to this app has been turned off"* and users cannot DM the bot.

## 7. Tokens You Should Have

| Type | Format | Purpose |
| --- | --- | --- |
| App-Level Token | `xapp-...` | Socket Mode WebSocket connection |
| Bot User OAuth Token | `xoxb-...` | Web API calls (send messages, upload files, etc.) |

> Both tokens are secrets — do not commit them; store them in environment variables or a secret manager.

## 8. Run cokacdir with the Slack Bot

Store the token pair in a user-only file, one bot configuration per line:

```bash
install -m 600 /dev/null ~/.cokacdir/ccserver.tokens
${EDITOR:-vi} ~/.cokacdir/ccserver.tokens
cokacdir --ccserver-token-file ~/.cokacdir/ccserver.tokens
```

The Slack line in that file has the form `slack:<xoxb-...>,<xapp-...>`. You can
also pipe the same line to `cokacdir --ccserver-stdin`. Avoid putting real
tokens directly after `--ccserver`: command-line arguments are visible to other
processes and system monitoring tools on many platforms.

Token-string rules:

- The `slack:` prefix is explicit; it can be omitted because cokacdir auto-detects token pairs that contain both `xoxb-` and `xapp-`.
- Separate the two tokens with a comma (`,`).
- Order does not matter (`xoxb-...,xapp-...` and `xapp-...,xoxb-...` are both accepted).

Examples:

```text
# ~/.cokacdir/ccserver.tokens
slack:<xoxb-...>,<xapp-...>

# Multiple bots: add one configuration per line
<telegram-token>
slack:<xoxb-...>,<xapp-...>
```

## 9. Final Checklist

- [ ] App created
- [ ] Socket Mode on, `xapp-` token saved
- [ ] Event Subscriptions on, 5 bot events subscribed
- [ ] `chat:write` (and any optional write scopes) added
- [ ] Installed to workspace, `xoxb-` token saved
- [ ] App Home → Messages Tab on, message sending allowed
