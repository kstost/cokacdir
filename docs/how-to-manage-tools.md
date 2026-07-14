# How to Manage Tools

Tools are the actions the AI agent can perform — running commands, reading files, editing code, searching the web, etc. The tool-management commands on this page control **Claude only**.

> **Claude-only feature:** `/availabletools`, `/allowedtools`, and `/allowed` are available only while the active provider is Claude. They do not inspect, configure, or restrict Codex, Agy, or OpenCode.

---

## /availabletools (Claude only)

Lists all tools that can be enabled. Destructive tools are marked with `!!!`.

```
Available Tools

Bash !!! — Execute shell commands
Read — Read file contents from the filesystem
Edit !!! — Edit file contents
...
Total: 20
```

## /allowedtools (Claude only)

Shows the tools currently enabled for this chat.

## /allowed (Claude only)

Add or remove tools from the allowed list.

```
/allowed +Bash          → enable Bash
/allowed -WebSearch     → disable WebSearch
/allowed +Read -Bash    → enable Read and disable Bash at once
```

- Tool names are case-insensitive.
- Multiple `+`/`-` operations can be combined in a single command.
- Changes take effect immediately and persist across restarts.

### Default Allowed Tools

Bash, Read, Edit, Write, Glob, Grep, Task, TaskOutput, TaskStop, WebFetch, WebSearch, NotebookEdit, Skill, TaskCreate, TaskGet, TaskUpdate, TaskList

### Provider Restriction

The entire `allowed_tools` feature is **Claude-only**. With Codex, Agy, or OpenCode selected, all three commands (`/availabletools`, `/allowedtools`, and `/allowed`) are rejected with `Tool permissions are available only when the active provider is Claude.`

For non-Claude providers, cokacdir does not pass this list as a tool restriction and does not add disabled-tool instructions to the system prompt. Those agents keep their native/full permissions. A saved Claude list remains stored while another provider is active and takes effect again only after switching back to Claude.
