/// Risk classification for commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRisk {
    Safe,
    Elevated,
    Dangerous,
}

/// Classify a command string by its risk level.
///
/// - Safe: read-only or informational commands
/// - Elevated: commands that change state but are bounded (relative paths, tool config)
/// - Dangerous: commands that can access arbitrary paths or change security settings
pub fn classify_command(cmd: &str) -> CommandRisk {
    let cmd = cmd.trim();

    // Shell execution is always dangerous
    if cmd.starts_with('!') {
        return CommandRisk::Dangerous;
    }

    // Extract the command word
    let command_word = cmd.split_whitespace().next().unwrap_or("").to_lowercase();

    match command_word.as_str() {
        // Safe: read-only, informational, or session management
        "/help" | "/pwd" | "/stop" | "/clear" => CommandRisk::Safe,

        // Elevated: state-changing but scoped
        "/start" | "/allowedtools" | "/availabletools" | "/setpollingtime" | "/debug" => {
            CommandRisk::Elevated
        }

        // /down: elevated for relative paths, dangerous for absolute paths or path traversal
        "/down" => {
            let arg = cmd.split_whitespace().nth(1).unwrap_or("");
            if arg.starts_with('/') || arg.starts_with("..") {
                CommandRisk::Dangerous
            } else {
                CommandRisk::Elevated
            }
        }

        // Dangerous: security / access control changes
        "/allowed" | "/public" => CommandRisk::Dangerous,

        // Plain text messages (no leading slash or !) are safe
        _ if !cmd.starts_with('/') => CommandRisk::Safe,

        // Unknown slash commands: treat as elevated to be cautious
        _ => CommandRisk::Elevated,
    }
}

/// Determine whether a user is allowed to execute a command.
///
/// Rules:
/// - Owner: all commands allowed
/// - Public mode non-owner: Safe commands only
/// - Non-public non-owner: nothing allowed (blocked at a higher level, but returns false here too)
pub fn can_execute(user_is_owner: bool, is_public: bool, risk: CommandRisk) -> bool {
    if user_is_owner {
        return true;
    }
    if is_public {
        return risk == CommandRisk::Safe;
    }
    false
}

/// Check whether `path` is contained within `sandbox_root`.
///
/// Both paths are canonicalized before comparison to resolve symlinks and
/// relative components. Returns `false` if canonicalization fails (treating
/// unresolvable paths as outside the sandbox).
pub fn is_path_within_sandbox(path: &std::path::Path, sandbox_root: &std::path::Path) -> bool {
    let Ok(canonical_path) = path.canonicalize() else {
        return false;
    };
    let Ok(canonical_root) = sandbox_root.canonicalize() else {
        return false;
    };
    canonical_path.starts_with(&canonical_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_safe_commands() {
        assert_eq!(classify_command("/help"), CommandRisk::Safe);
        assert_eq!(classify_command("/pwd"), CommandRisk::Safe);
        assert_eq!(classify_command("/stop"), CommandRisk::Safe);
        assert_eq!(classify_command("/clear"), CommandRisk::Safe);
        assert_eq!(classify_command("hello world"), CommandRisk::Safe);
        assert_eq!(classify_command("some text message"), CommandRisk::Safe);
    }

    #[test]
    fn test_classify_elevated_commands() {
        assert_eq!(classify_command("/start"), CommandRisk::Elevated);
        assert_eq!(classify_command("/allowedtools"), CommandRisk::Elevated);
        assert_eq!(classify_command("/availabletools"), CommandRisk::Elevated);
        assert_eq!(classify_command("/setpollingtime 30"), CommandRisk::Elevated);
        assert_eq!(classify_command("/debug"), CommandRisk::Elevated);
        assert_eq!(classify_command("/down relative/path"), CommandRisk::Elevated);
    }

    #[test]
    fn test_classify_dangerous_commands() {
        assert_eq!(classify_command("!ls -la"), CommandRisk::Dangerous);
        assert_eq!(classify_command("!rm -rf /"), CommandRisk::Dangerous);
        assert_eq!(classify_command("/down /absolute/path"), CommandRisk::Dangerous);
        assert_eq!(classify_command("/down ../escape"), CommandRisk::Dangerous);
        assert_eq!(classify_command("/allowed +tool"), CommandRisk::Dangerous);
        assert_eq!(classify_command("/public"), CommandRisk::Dangerous);
    }

    #[test]
    fn test_can_execute_owner() {
        assert!(can_execute(true, false, CommandRisk::Safe));
        assert!(can_execute(true, false, CommandRisk::Elevated));
        assert!(can_execute(true, false, CommandRisk::Dangerous));
        assert!(can_execute(true, true, CommandRisk::Dangerous));
    }

    #[test]
    fn test_can_execute_public_non_owner() {
        assert!(can_execute(false, true, CommandRisk::Safe));
        assert!(!can_execute(false, true, CommandRisk::Elevated));
        assert!(!can_execute(false, true, CommandRisk::Dangerous));
    }

    #[test]
    fn test_can_execute_non_public_non_owner() {
        assert!(!can_execute(false, false, CommandRisk::Safe));
        assert!(!can_execute(false, false, CommandRisk::Elevated));
        assert!(!can_execute(false, false, CommandRisk::Dangerous));
    }

    #[test]
    fn test_path_within_sandbox() {
        let tmp = std::env::temp_dir();
        // The temp dir itself should be within itself
        assert!(is_path_within_sandbox(&tmp, &tmp));
    }
}
