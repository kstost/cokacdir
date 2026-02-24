use std::fs::{self, OpenOptions};
use std::io::Write;

/// Log a command execution to the audit trail.
/// File: ~/.cokacdir/audit.log, permissions 0o600
pub fn log_command(user_id: u64, chat_id: i64, risk: &str, command: &str) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let log_dir = home.join(".cokacdir");
    let log_path = log_dir.join("audit.log");

    let _ = fs::create_dir_all(&log_dir);

    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let cmd_preview: String = command.chars().take(200).collect();
    let line = format!("[{timestamp}] user={user_id} chat={chat_id} risk={risk} cmd={cmd_preview}\n");

    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) else {
        return;
    };
    let _ = file.write_all(line.as_bytes());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(&log_path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            let _ = fs::set_permissions(&log_path, perms);
        }
    }
}
