# How to Use Herdr Agents

Cokacdir can route bot requests to an already-running
[Herdr](https://github.com/ogulcancelik/herdr) agent. This lets a Telegram,
Discord, or Slack bot continue an interactive agent session that is visible in
a Herdr pane.

## Prerequisites

1. Install the `herdr` CLI and start its server.
2. Start or name an agent in a Herdr pane.
3. Confirm that the same user and environment that runs Cokacdir can resolve
   the agent:

   ```bash
   herdr agent get worker
   ```

Cokacdir does not create or resume Herdr agents. The target must already be
running and ready to accept interactive input.

## Configuration

Select a target directly from chat:

```text
/model herdr:worker
```

To make `/model herdr` use a default target, add the following to
`~/.cokacdir/.env.json`:

```json
{
  "COKAC_HERDR_PATH": "/usr/local/bin/herdr",
  "COKAC_HERDR_AGENT": "worker"
}
```

Restart Cokacdir after changing the environment file. Herdr-specific
environment such as `HERDR_SOCKET_PATH` is inherited by the CLI process.

## Turn lifecycle

For each request, Cokacdir:

1. Reads the target agent's recent terminal output.
2. Submits the user request with `herdr agent prompt ... --wait`.
3. Waits for the agent to become `idle`, `done`, or `blocked`.
4. Reads the terminal again and returns the current turn's final response.

Cokacdir forwards the current request without prepending its generated system
prompt. The existing agent session keeps its own system, project, and
conversation context.

Codex TUI final responses are extracted from the current turn while tool output
and UI chrome are omitted. Other agent terminal layouts fall back to the
terminal delta and may include intermediate output.

Use `/stop` to cancel the waiting Cokacdir request and send `Ctrl+C` to the
target Herdr agent.

## Security

Herdr access grants interactive control over the target terminal session.
Protect its socket and configuration directory, run Cokacdir as the intended
user, and do not expose the socket to unrelated services.
