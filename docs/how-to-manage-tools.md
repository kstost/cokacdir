# How to Manage Tools

Tools are the actions the AI agent can perform — running commands, reading files, editing code, searching the web, etc. You can control which tools are available per chat.

---

## /availabletools

Lists all tools that can be enabled. Destructive tools are marked with `!!!`.

```
Available Tools

Bash !!! — Execute shell commands
Read — Read file contents from the filesystem
Edit !!! — Edit file contents
...
Total: 20
```

## /allowedtools

Shows the tools currently enabled for this chat.

## /allowed

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

Tool permissions only apply when the active model is **Claude**. With Codex, Agy, or OpenCode selected, `/allowed` is rejected with `Tool permissions are not supported in this mode.` The other tool commands (`/availabletools`, `/allowedtools`) still work for inspection.
