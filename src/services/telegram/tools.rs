use teloxide::prelude::*;
use teloxide::types::ParseMode;

use crate::services::claude::DEFAULT_ALLOWED_TOOLS;

use super::{BotSettings, SharedState, tg};
use super::streaming::{html_escape, send_long_message, shared_rate_limit_wait, truncate_str};

/// Get allowed tools for a specific chat_id.
/// Returns the chat-specific list if configured, otherwise DEFAULT_ALLOWED_TOOLS.
pub(crate) fn get_allowed_tools(settings: &BotSettings, chat_id: ChatId) -> Vec<String> {
    let key = chat_id.0.to_string();
    settings.allowed_tools.get(&key)
        .cloned()
        .unwrap_or_else(|| DEFAULT_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect())
}

/// Normalize tool name: first letter uppercase, rest lowercase
pub(crate) fn normalize_tool_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// All available tools with (description, is_destructive)
pub(crate) const ALL_TOOLS: &[(&str, &str, bool)] = &[
    ("Bash",            "Execute shell commands",                          true),
    ("Read",            "Read file contents from the filesystem",          false),
    ("Edit",            "Perform find-and-replace edits in files",         true),
    ("Write",           "Create or overwrite files",                       true),
    ("Glob",            "Find files by name pattern",                      false),
    ("Grep",            "Search file contents with regex",                 false),
    ("Task",            "Launch autonomous sub-agents for complex tasks",  true),
    ("TaskOutput",      "Retrieve output from background tasks",           false),
    ("TaskStop",        "Stop a running background task",                  false),
    ("WebFetch",        "Fetch and process web page content",              true),
    ("WebSearch",       "Search the web for up-to-date information",       true),
    ("NotebookEdit",    "Edit Jupyter notebook cells",                     true),
    ("Skill",           "Invoke slash-command skills",                     false),
    ("TaskCreate",      "Create a structured task in the task list",       false),
    ("TaskGet",         "Retrieve task details by ID",                     false),
    ("TaskUpdate",      "Update task status or details",                   false),
    ("TaskList",        "List all tasks and their status",                 false),
    ("AskUserQuestion", "Ask the user a question (interactive)",           false),
    ("EnterPlanMode",   "Enter planning mode (interactive)",               false),
    ("ExitPlanMode",    "Exit planning mode (interactive)",                false),
];

/// Tool info: (description, is_destructive)
pub(crate) fn tool_info(name: &str) -> (&'static str, bool) {
    ALL_TOOLS.iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, desc, destr)| (*desc, *destr))
        .unwrap_or(("Custom tool", false))
}

/// Format a risk badge for display
pub(crate) fn risk_badge(destructive: bool) -> &'static str {
    if destructive { "!!!" } else { "" }
}

/// Handle /availabletools command - show all available tools
pub(crate) async fn handle_availabletools_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let mut msg = String::from("<b>Available Tools</b>\n\n");

    for &(name, desc, destructive) in ALL_TOOLS {
        let badge = risk_badge(destructive);
        if badge.is_empty() {
            msg.push_str(&format!("<code>{}</code> — {}\n", html_escape(name), html_escape(desc)));
        } else {
            msg.push_str(&format!("<code>{}</code> {} — {}\n", html_escape(name), badge, html_escape(desc)));
        }
    }
    msg.push_str(&format!("\n{} = destructive\nTotal: {}", risk_badge(true), ALL_TOOLS.len()));

    send_long_message(bot, chat_id, &msg, Some(ParseMode::Html), state).await?;

    Ok(())
}

/// Handle /allowedtools command - show current allowed tools list
pub(crate) async fn handle_allowedtools_command(
    bot: &Bot,
    chat_id: ChatId,
    state: &SharedState,
) -> ResponseResult<()> {
    let tools = {
        let data = state.lock().await;
        get_allowed_tools(&data.settings, chat_id)
    };

    let mut msg = String::from("<b>Allowed Tools</b>\n\n");
    for tool in &tools {
        let (desc, destructive) = tool_info(tool);
        let badge = risk_badge(destructive);
        if badge.is_empty() {
            msg.push_str(&format!("<code>{}</code> — {}\n", html_escape(tool), html_escape(desc)));
        } else {
            msg.push_str(&format!("<code>{}</code> {} — {}\n", html_escape(tool), badge, html_escape(desc)));
        }
    }
    msg.push_str(&format!("\n{} = destructive\nTotal: {}", risk_badge(true), tools.len()));

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, &msg)
        .parse_mode(ParseMode::Html)
        .await)?;

    Ok(())
}

/// Handle /allowed command - add/remove tools
/// Usage: /allowed +toolname  (add)
///        /allowed -toolname  (remove)
///        /allowed +tool1 -tool2 +tool3  (multiple)
pub(crate) async fn handle_allowed_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &SharedState,
    token: &str,
) -> ResponseResult<()> {
    let arg = text.strip_prefix("/allowed").unwrap_or("").trim();

    if arg.is_empty() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Usage:\n/allowed +toolname — Add a tool\n/allowed -toolname — Remove a tool\n/allowed +tool1 -tool2 — Multiple at once\n/allowedtools — Show current list")
            .await)?;
        return Ok(());
    }

    // Skip if argument starts with "tools" (that's /allowedtools handled separately)
    if arg.starts_with("tools") {
        // This shouldn't happen due to routing order, but just in case
        return handle_allowedtools_command(bot, chat_id, state).await;
    }

    // Parse multiple +name / -name tokens
    let mut operations: Vec<(char, String)> = Vec::new();
    for token_str in arg.split_whitespace() {
        if let Some(name) = token_str.strip_prefix('+') {
            let name = name.trim();
            if !name.is_empty() {
                operations.push(('+', normalize_tool_name(name)));
            }
        } else if let Some(name) = token_str.strip_prefix('-') {
            let name = name.trim();
            if !name.is_empty() {
                operations.push(('-', normalize_tool_name(name)));
            }
        }
    }

    if operations.is_empty() {
        shared_rate_limit_wait(state, chat_id).await;
        tg!("send_message", bot.send_message(chat_id, "Use +toolname to add or -toolname to remove.\nExample: /allowed +Bash -Edit")
            .await)?;
        return Ok(());
    }

    let response_msg = {
        let mut data = state.lock().await;
        let chat_key = chat_id.0.to_string();
        // Ensure this chat has its own tool list (initialize from defaults if missing)
        if !data.settings.allowed_tools.contains_key(&chat_key) {
            let defaults: Vec<String> = DEFAULT_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect();
            data.settings.allowed_tools.insert(chat_key.clone(), defaults);
        }
        let tools = data.settings.allowed_tools.get_mut(&chat_key).unwrap();
        let mut results: Vec<String> = Vec::new();
        let mut changed = false;
        for (op, tool_name) in &operations {
            match op {
                '+' => {
                    if tools.iter().any(|t| t == tool_name) {
                        results.push(format!("<code>{}</code> already in list", html_escape(tool_name)));
                    } else {
                        tools.push(tool_name.clone());
                        changed = true;
                        results.push(format!("✅ <code>{}</code>", html_escape(tool_name)));
                    }
                }
                '-' => {
                    let before_len = tools.len();
                    tools.retain(|t| t != tool_name);
                    if tools.len() < before_len {
                        changed = true;
                        results.push(format!("❌ <code>{}</code>", html_escape(tool_name)));
                    } else {
                        results.push(format!("<code>{}</code> not in list", html_escape(tool_name)));
                    }
                }
                _ => unreachable!(),
            }
        }
        if changed {
            super::storage::save_bot_settings(token, &data.settings);
        }
        results.join("\n")
    };

    shared_rate_limit_wait(state, chat_id).await;
    tg!("send_message", bot.send_message(chat_id, &response_msg)
        .parse_mode(ParseMode::Html)
        .await)?;

    Ok(())
}

/// Format tool input JSON into a human-readable summary
pub(crate) fn format_tool_input(name: &str, input: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        return format!("{} {}", name, truncate_str(input, 200));
    };

    match name {
        "Bash" => {
            let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if !desc.is_empty() {
                format!("{}: `{}`", desc, truncate_str(cmd, 150))
            } else {
                format!("`{}`", truncate_str(cmd, 200))
            }
        }
        "Read" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            format!("Read {}", fp)
        }
        "Write" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let content = v.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let lines = content.lines().count();
            if lines > 0 {
                format!("Write {} ({} lines)", fp, lines)
            } else {
                format!("Write {}", fp)
            }
        }
        "Edit" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let replace_all = v.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
            if replace_all {
                format!("Edit {} (replace all)", fp)
            } else {
                format!("Edit {}", fp)
            }
        }
        "Glob" => {
            let pattern = v.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                format!("Glob {} in {}", pattern, path)
            } else {
                format!("Glob {}", pattern)
            }
        }
        "Grep" => {
            let pattern = v.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let output_mode = v.get("output_mode").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                if !output_mode.is_empty() {
                    format!("Grep \"{}\" in {} ({})", pattern, path, output_mode)
                } else {
                    format!("Grep \"{}\" in {}", pattern, path)
                }
            } else {
                format!("Grep \"{}\"", pattern)
            }
        }
        "NotebookEdit" => {
            let nb_path = v.get("notebook_path").and_then(|v| v.as_str()).unwrap_or("");
            let cell_id = v.get("cell_id").and_then(|v| v.as_str()).unwrap_or("");
            if !cell_id.is_empty() {
                format!("Notebook {} ({})", nb_path, cell_id)
            } else {
                format!("Notebook {}", nb_path)
            }
        }
        "WebSearch" => {
            let query = v.get("query").and_then(|v| v.as_str()).unwrap_or("");
            format!("Search: {}", query)
        }
        "WebFetch" => {
            let url = v.get("url").and_then(|v| v.as_str()).unwrap_or("");
            format!("Fetch {}", url)
        }
        "Task" => {
            let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let subagent_type = v.get("subagent_type").and_then(|v| v.as_str()).unwrap_or("");
            if !subagent_type.is_empty() {
                format!("Task [{}]: {}", subagent_type, desc)
            } else {
                format!("Task: {}", desc)
            }
        }
        "TaskOutput" => {
            let task_id = v.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            format!("Get task output: {}", task_id)
        }
        "TaskStop" => {
            let task_id = v.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            format!("Stop task: {}", task_id)
        }
        "TodoWrite" => {
            if let Some(todos) = v.get("todos").and_then(|v| v.as_array()) {
                let pending = todos.iter().filter(|t| {
                    t.get("status").and_then(|s| s.as_str()) == Some("pending")
                }).count();
                let in_progress = todos.iter().filter(|t| {
                    t.get("status").and_then(|s| s.as_str()) == Some("in_progress")
                }).count();
                let completed = todos.iter().filter(|t| {
                    t.get("status").and_then(|s| s.as_str()) == Some("completed")
                }).count();
                format!("Todo: {} pending, {} in progress, {} completed", pending, in_progress, completed)
            } else {
                "Update todos".to_string()
            }
        }
        "Skill" => {
            let skill = v.get("skill").and_then(|v| v.as_str()).unwrap_or("");
            format!("Skill: {}", skill)
        }
        "AskUserQuestion" => {
            if let Some(questions) = v.get("questions").and_then(|v| v.as_array()) {
                if let Some(q) = questions.first() {
                    let question = q.get("question").and_then(|v| v.as_str()).unwrap_or("");
                    truncate_str(question, 200)
                } else {
                    "Ask user question".to_string()
                }
            } else {
                "Ask user question".to_string()
            }
        }
        "ExitPlanMode" => {
            "Exit plan mode".to_string()
        }
        "EnterPlanMode" => {
            "Enter plan mode".to_string()
        }
        "TaskCreate" => {
            let subject = v.get("subject").and_then(|v| v.as_str()).unwrap_or("");
            format!("Create task: {}", subject)
        }
        "TaskUpdate" => {
            let task_id = v.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
            let status = v.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if !status.is_empty() {
                format!("Update task {}: {}", task_id, status)
            } else {
                format!("Update task {}", task_id)
            }
        }
        "TaskGet" => {
            let task_id = v.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
            format!("Get task: {}", task_id)
        }
        "TaskList" => {
            "List tasks".to_string()
        }
        _ => {
            format!("{} {}", name, truncate_str(input, 200))
        }
    }
}
