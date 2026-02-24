mod en;
mod ko;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lang {
    Ko,
    En,
}

impl Default for Lang {
    fn default() -> Self {
        Lang::Ko
    }
}

pub fn resolve_lang(code: &str) -> Lang {
    match code.to_lowercase().as_str() {
        "en" => Lang::En,
        _ => Lang::Ko,
    }
}

// Session
pub fn msg_no_session(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_NO_SESSION,
        Lang::En => en::MSG_NO_SESSION,
    }
}

pub fn msg_session_cleared(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_SESSION_CLEARED,
        Lang::En => en::MSG_SESSION_CLEARED,
    }
}

// AI busy
pub fn msg_ai_busy(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_AI_BUSY,
        Lang::En => en::MSG_AI_BUSY,
    }
}

// Permission
pub fn msg_permission_denied(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_PERMISSION_DENIED,
        Lang::En => en::MSG_PERMISSION_DENIED,
    }
}

// Stop
pub fn msg_stopping(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_STOPPING,
        Lang::En => en::MSG_STOPPING,
    }
}

pub fn msg_no_active_request(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_NO_ACTIVE_REQUEST,
        Lang::En => en::MSG_NO_ACTIVE_REQUEST,
    }
}

// Public
pub fn msg_group_only(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_GROUP_ONLY,
        Lang::En => en::MSG_GROUP_ONLY,
    }
}

pub fn msg_public_owner_only(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_PUBLIC_OWNER_ONLY,
        Lang::En => en::MSG_PUBLIC_OWNER_ONLY,
    }
}

pub fn msg_public_enabled(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_PUBLIC_ON,
        Lang::En => en::MSG_PUBLIC_ON,
    }
}

pub fn msg_public_disabled(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_PUBLIC_OFF,
        Lang::En => en::MSG_PUBLIC_OFF,
    }
}

pub fn msg_public_status(lang: Lang, status: &str) -> String {
    let tpl = match lang {
        Lang::Ko => ko::MSG_PUBLIC_STATUS,
        Lang::En => en::MSG_PUBLIC_STATUS,
    };
    tpl.replacen("{}", status, 1)
}

pub fn msg_public_status_label(lang: Lang, is_public: bool) -> &'static str {
    if is_public {
        match lang {
            Lang::Ko => ko::MSG_PUBLIC_STATUS_ENABLED,
            Lang::En => en::MSG_PUBLIC_STATUS_ENABLED,
        }
    } else {
        match lang {
            Lang::Ko => ko::MSG_PUBLIC_STATUS_DISABLED,
            Lang::En => en::MSG_PUBLIC_STATUS_DISABLED,
        }
    }
}

pub fn msg_public_usage(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_PUBLIC_USAGE,
        Lang::En => en::MSG_PUBLIC_USAGE,
    }
}

// Language
pub fn msg_lang_changed(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_LANG_CHANGED,
        Lang::En => en::MSG_LANG_CHANGED,
    }
}

pub fn msg_lang_usage(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_LANG_USAGE,
        Lang::En => en::MSG_LANG_USAGE,
    }
}

// Shell
pub fn msg_shell_usage(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_SHELL_USAGE,
        Lang::En => en::MSG_SHELL_USAGE,
    }
}

pub fn msg_shell_timeout(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_SHELL_TIMEOUT,
        Lang::En => en::MSG_SHELL_TIMEOUT,
    }
}

pub fn msg_shell_processing(lang: Lang, cmd: &str) -> String {
    let tpl = match lang {
        Lang::Ko => ko::MSG_SHELL_PROCESSING,
        Lang::En => en::MSG_SHELL_PROCESSING,
    };
    tpl.replacen("{}", cmd, 1)
}

// Down (file download)
pub fn msg_down_usage(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_DOWN_USAGE,
        Lang::En => en::MSG_DOWN_USAGE,
    }
}

pub fn msg_down_no_session(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_DOWN_NO_SESSION,
        Lang::En => en::MSG_DOWN_NO_SESSION,
    }
}

// File ops
pub fn msg_file_save_failed(lang: Lang, err: &str) -> String {
    let tpl = match lang {
        Lang::Ko => ko::MSG_FILE_SAVE_FAILED,
        Lang::En => en::MSG_FILE_SAVE_FAILED,
    };
    tpl.replacen("{}", err, 1)
}

pub fn msg_sandbox_denied(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_SANDBOX_DENIED,
        Lang::En => en::MSG_SANDBOX_DENIED,
    }
}

// Errors
pub fn msg_error_home(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::MSG_ERROR_HOME,
        Lang::En => en::MSG_ERROR_HOME,
    }
}

pub fn msg_error_invalid_dir(lang: Lang, dir: &str) -> String {
    let tpl = match lang {
        Lang::Ko => ko::MSG_ERROR_INVALID_DIR,
        Lang::En => en::MSG_ERROR_INVALID_DIR,
    };
    tpl.replacen("{}", dir, 1)
}

pub fn msg_error_create_workspace(lang: Lang, err: &str) -> String {
    let tpl = match lang {
        Lang::Ko => ko::MSG_ERROR_CREATE_WORKSPACE,
        Lang::En => en::MSG_ERROR_CREATE_WORKSPACE,
    };
    tpl.replacen("{}", err, 1)
}

// Help
pub fn help_text(lang: Lang) -> &'static str {
    match lang {
        Lang::Ko => ko::help_text(),
        Lang::En => en::help_text(),
    }
}
