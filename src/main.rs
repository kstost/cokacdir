mod config;
mod enc;
mod keybindings;
mod services;
mod ui;
mod utils;

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::env;
use std::io::{self, Read, Write};
use std::sync::OnceLock;
use std::time::Duration;

use crate::keybindings::PanelAction;
use crate::services::agy;
use crate::services::claude;
use crate::services::codex;
use crate::services::opencode;
use crate::ui::app::{App, Screen};
use crate::utils::markdown::{is_line_empty, render_markdown, MarkdownTheme};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PROJECT_LICENSE: &str = include_str!("../LICENSE");
const THIRD_PARTY_NOTICES: &str = include_str!("../THIRD_PARTY_NOTICES.md");
const OPENSSL_LICENSE: &str = include_str!("../LICENSES/OpenSSL-3.6.3.txt");

/// Global binary path, resolved once at startup via `std::env::current_exe()`.
/// Works on Linux (/proc/self/exe), macOS (_NSGetExecutablePath), Windows (GetModuleFileNameW).
static BIN_PATH: OnceLock<String> = OnceLock::new();

/// Initialize the global binary path. Call once at startup.
fn init_bin_path() {
    BIN_PATH.get_or_init(|| {
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    "cokacdir.exe".to_string()
                } else {
                    "cokacdir".to_string()
                }
            })
    });
}

/// Get the resolved binary path.
pub fn bin_path() -> &'static str {
    BIN_PATH.get().map(|s| s.as_str()).unwrap_or("cokacdir")
}

fn open_private_directory(
    path: &std::path::Path,
) -> io::Result<(
    std::fs::File,
    crate::services::file_ops::DirectoryAccess,
    crate::services::file_ops::StablePathIdentity,
)> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
        }
        Err(error) => return Err(error),
    }
    let (directory, stable_path, metadata) =
        crate::services::file_ops::open_directory_for_read(path)?;
    let identity = crate::services::file_ops::stable_file_identity(&directory)?;
    if !metadata.file_type().is_dir()
        || crate::services::file_ops::stable_path_identity(path)? != identity
    {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a stable real directory", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    if crate::services::file_ops::stable_path_identity(path)? != identity {
        return Err(io::Error::other(format!(
            "{} changed while it was secured",
            path.display()
        )));
    }
    Ok((directory, stable_path, identity))
}

fn ensure_private_directory(path: &std::path::Path) -> io::Result<()> {
    open_private_directory(path).map(|_| ())
}

/// Preserve an unreadable settings file without ever truncating an older
/// recovery copy.  A malformed settings file may later be replaced when the
/// user changes an in-memory setting, so the backup must be complete and
/// durable before the TUI starts accepting input.
fn backup_unparseable_settings(path: &std::path::Path) -> io::Result<std::path::PathBuf> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let (parent_guard, stable_parent, parent_identity) = open_private_directory(parent)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid settings path"))?
        .to_string_lossy();
    let source_before = std::fs::symlink_metadata(path)?;
    if !source_before.file_type().is_file() || source_before.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "settings path is not a real regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if source_before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings path is a reparse point",
            ));
        }
    }
    let source_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid settings path"))?;
    let mut source = stable_parent.open_file(
        source_name,
        crate::services::file_ops::DirectoryFileOptions::new().read(true),
    )?;
    let source_opened = source.metadata()?;
    if !source_opened.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "settings path is not a real regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if source_opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings path is a reparse point",
            ));
        }
    }
    let source_identity = crate::services::file_ops::stable_file_identity(&source)?;
    if crate::services::file_ops::stable_path_identity(path)? != source_identity
        || crate::services::file_ops::stable_path_identity(parent)? != parent_identity
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "settings path changed while being opened",
        ));
    }

    for attempt in 0..128u32 {
        let backup_name = if attempt == 0 {
            format!("{file_name}.bak")
        } else {
            format!("{file_name}.bak.{:032x}", rand::random::<u128>())
        };
        let backup_path = parent.join(backup_name);
        let backup_name = backup_path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid settings backup path")
        })?;

        let mut backup = match stable_parent.open_file(
            backup_name,
            crate::services::file_ops::DirectoryFileOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600),
        ) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };

        let backup_identity = match crate::services::file_ops::stable_file_identity(&backup) {
            Ok(identity) => identity,
            Err(error) => {
                drop(backup);
                if let Ok(identity) = stable_parent.child_identity(backup_name) {
                    let _ = stable_parent.remove_file_if_identity(backup_name, identity);
                }
                return Err(error);
            }
        };
        let result = (|| {
            io::copy(&mut source, &mut backup)?;
            let source_after = source.metadata()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if source_after.dev() != source_opened.dev()
                    || source_after.ino() != source_opened.ino()
                    || source_after.len() != source_opened.len()
                    || source_after.mtime() != source_opened.mtime()
                    || source_after.mtime_nsec() != source_opened.mtime_nsec()
                    || source_after.ctime() != source_opened.ctime()
                    || source_after.ctime_nsec() != source_opened.ctime_nsec()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "settings changed while being backed up",
                    ));
                }
            }
            #[cfg(not(unix))]
            if source_after.len() != source_opened.len()
                || source_after.modified().ok() != source_opened.modified().ok()
            {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "settings changed while being backed up",
                ));
            }
            backup.flush()?;
            backup.sync_all()?;
            Ok(())
        })();
        if let Err(e) = result {
            drop(backup);
            let _ = stable_parent.remove_file_if_identity(backup_name, backup_identity);
            return Err(e);
        }

        drop(backup);
        let published_identity = stable_parent.child_identity(backup_name);
        let current_parent_identity = crate::services::file_ops::stable_path_identity(parent);
        if published_identity.as_ref().ok() != Some(&backup_identity)
            || current_parent_identity.as_ref().ok() != Some(&parent_identity)
        {
            let _ = stable_parent.remove_file_if_identity(backup_name, backup_identity);
            return Err(published_identity
                .err()
                .or_else(|| current_parent_identity.err())
                .unwrap_or_else(|| {
                    io::Error::other("settings backup path changed while it was written")
                }));
        }
        #[cfg(unix)]
        parent_guard.sync_all()?;

        return Ok(backup_path);
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a settings recovery file",
    ))
}

fn create_new_private_file_with(
    path: &std::path::Path,
    write_contents: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no name"))?;
    let (directory, stable_directory, directory_identity) = open_private_directory(parent)?;
    let mut file = stable_directory.open_file(
        file_name,
        crate::services::file_ops::DirectoryFileOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600),
    )?;
    let file_identity = match crate::services::file_ops::stable_file_identity(&file) {
        Ok(identity) => identity,
        Err(error) => {
            drop(file);
            if let Ok(identity) = stable_directory.child_identity(file_name) {
                let _ = stable_directory.remove_file_if_identity(file_name, identity);
            }
            return Err(error);
        }
    };
    if let Err(error) = write_contents(&mut file).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = stable_directory.remove_file_if_identity(file_name, file_identity);
        return Err(error);
    }
    drop(file);
    let published_identity = crate::services::file_ops::stable_path_identity(path);
    let current_directory_identity = crate::services::file_ops::stable_path_identity(parent);
    if published_identity.as_ref().ok() != Some(&file_identity)
        || current_directory_identity.as_ref().ok() != Some(&directory_identity)
    {
        let _ = stable_directory.remove_file_if_identity(file_name, file_identity);
        return Err(published_identity
            .err()
            .or_else(|| current_directory_identity.err())
            .unwrap_or_else(|| {
                io::Error::other("private file path changed while it was created")
            }));
    }
    #[cfg(unix)]
    if let Err(error) = directory.sync_all() {
        // The create-new entry is already committed and the file itself is
        // synced. Returning an error here could make callers retry a message
        // or upload request that a concurrent bot process can already see.
        eprintln!(
            "Warning: private file was created, but directory durability could not be confirmed: {error}"
        );
    }
    Ok(())
}

fn write_new_private_file(path: &std::path::Path, contents: &[u8]) -> io::Result<()> {
    create_new_private_file_with(path, |file| file.write_all(contents))
}

fn print_help() {
    println!("cokacdir {} - Multi-panel terminal file manager", VERSION);
    println!();
    println!("USAGE:");
    println!("    cokacdir [OPTIONS] [PATH...]");
    println!();
    println!("ARGS:");
    println!("    [PATH...]               Open panels at given paths (max 10)");
    println!();
    println!("OPTIONS:");
    println!("    -h, --help              Print help information");
    println!("    -v, --version           Print version information");
    println!("    --licenses              Print project license and third-party notices");
    println!("    --prompt <TEXT>         Send prompt to AI and print rendered response");
    println!("    --design                Enable theme hot-reload (for theme development)");
    println!("    --base64 <TEXT>         Decode base64 and print (internal use)");
    println!("    --ccserver-token-file <PATH>");
    println!(
        "                            Start bot server(s), reading one token per line (recommended)"
    );
    println!(
        "    --ccserver-stdin         Start bot server(s), reading one token per line from stdin"
    );
    println!("    --ccserver <TOKEN>...   Legacy token arguments (visible in process listings)");
    println!("    --sendfile <PATH> --chat <ID> --key-file <PATH>");
    println!(
        "                            Send file via Telegram bot (internal use, HASH = token hash)"
    );
    println!("    --currenttime            Print current server time");
    println!(
        "    --cron <PROMPT> --at <TIME> --chat <ID> --key-file <PATH> [--once] [--session <SID>]"
    );
    println!("                            Register a scheduled task");
    println!("    --cron-list --chat <ID> --key-file <PATH>");
    println!("                            List registered schedules");
    println!("    --cron-remove <SID> --chat <ID> --key-file <PATH>");
    println!("                            Remove a schedule");
    println!("    --cron-update <SID> --at <TIME> --chat <ID> --key-file <PATH>");
    println!("                            Update schedule time");
    println!("    --cron-history <SID> --chat <ID> --key-file <PATH>");
    println!("                            Read run history of a schedule (JSONL records)");
    println!("    --message <TEXT> --to <BOT> --chat <ID> --key-file <PATH>");
    println!("                            Send message to another bot (internal use)");
    println!("    --key-file <PATH>       Read an internal authorization key from a private file");
    println!("    --key-stdin             Read an internal authorization key from stdin");
    println!("    --read_chat_log <CHAT_ID> [--range <N|START-END>] [--bot <USERNAME>]");
    println!("                            Read group chat shared log");
    println!();
    println!("HOMEPAGE: https://cokacdir.cokac.com");
}

fn handle_base64(encoded: &str) {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    match BASE64.decode(encoded) {
        Ok(decoded) => {
            if let Ok(text) = String::from_utf8(decoded) {
                print!("{}", text);
            } else {
                std::process::exit(1);
            }
        }
        Err(_) => {
            std::process::exit(1);
        }
    }
}

fn canonical_sendfile_path(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map(crate::utils::format::strip_unc_prefix)
        .map_err(|e| format!("failed to resolve path: {}", e))?;
    if !canonical.is_file() {
        return Err(format!("not a regular file: {}", canonical.display()));
    }
    Ok(canonical)
}

fn enqueue_upload_request(
    queue_dir: &std::path::Path,
    abs_path: &std::path::Path,
    chat_id: i64,
    hash_key: &str,
) -> io::Result<std::path::PathBuf> {
    use md5::{Digest, Md5};
    use rand::RngCore;

    let abs_path_text = abs_path.to_string_lossy();
    let queue_content = serde_json::to_vec(&serde_json::json!({
        "path": abs_path_text,
        "chat_id": chat_id,
        "key": hash_key,
    }))
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let now = chrono::Local::now();
    let timestamp = now.format("%Y-%m-%d-%H-%M-%S-%3f").to_string();
    let path_hash = format!("{:x}", Md5::digest(abs_path_text.as_bytes()));

    for _ in 0..100 {
        let mut nonce = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut nonce);
        let filename = format!("{}.{}.{}.queue", timestamp, path_hash, hex::encode(nonce));
        let queue_path = queue_dir.join(filename);
        match write_new_private_file(&queue_path, &queue_content) {
            Ok(()) => return Ok(queue_path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate a unique upload queue filename",
    ))
}

fn handle_sendfile(path: &str, chat_id: i64, hash_key: &str) {
    let file_path = std::path::Path::new(path);
    let abs_path = match canonical_sendfile_path(file_path) {
        Ok(path) => path,
        Err(message) => {
            eprintln!(
                "{}",
                serde_json::json!({"status":"error","message":message})
            );
            std::process::exit(1);
        }
    };

    let queue_dir = match dirs::home_dir() {
        Some(h) => h.join(".cokacdir").join("upload_queue"),
        None => {
            eprintln!(
                "{}",
                serde_json::json!({"status":"error","message":"cannot determine home directory"})
            );
            std::process::exit(1);
        }
    };
    match enqueue_upload_request(&queue_dir, &abs_path, chat_id, hash_key) {
        Ok(_) => println!(
            "{}",
            serde_json::json!({"status":"ok","path":abs_path.to_string_lossy()})
        ),
        Err(e) => {
            eprintln!(
                "{}",
                serde_json::json!({"status":"error","message":format!("failed to write queue file: {}", e)})
            );
            std::process::exit(1);
        }
    }
}

fn cron_debug(msg: &str) {
    claude::debug_log_to("cron.log", msg);
}

fn msg_debug(msg: &str) {
    claude::debug_log_to("msg.log", msg);
}

fn handle_read_group_chat(chat_id: i64, range_str: Option<&str>, filter_bot: Option<&str>) {
    use services::telegram;

    // Parse range: either a single number N (last N entries) or "START-END" (1-based line range)
    enum ReadMode {
        LastN(usize),
        Range(usize, Option<usize>),
    }
    let mode = match range_str {
        Some(s) if s.contains('-') => {
            let parts: Vec<&str> = s.splitn(2, '-').collect();
            let start: usize = parts[0].parse().unwrap_or(1);
            let end: usize = parts
                .get(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(usize::MAX);
            ReadMode::Range(start, Some(end))
        }
        Some(s) => ReadMode::LastN(s.parse().unwrap_or(20)),
        None => ReadMode::LastN(20),
    };

    let entries = match mode {
        ReadMode::LastN(n) => telegram::read_group_chat_log_tail(chat_id, n, filter_bot),
        ReadMode::Range(start, end) => {
            telegram::read_group_chat_log_range(chat_id, start, end, filter_bot)
        }
    };

    if entries.is_empty() {
        println!("(no entries found for chat_id {})", chat_id);
        return;
    }
    for (line_num, entry) in &entries {
        let from_info = entry
            .from
            .as_deref()
            .map(|f| format!("({})", f))
            .unwrap_or_default();
        let bot_label = match &entry.bot_display_name {
            Some(dn) if !dn.is_empty() => format!("{}(@{})", dn, entry.bot),
            _ => format!("@{}", entry.bot),
        };
        let role_display = if entry.role == "user" {
            format!("user→{}", bot_label)
        } else {
            bot_label
        };
        let display_text = if entry.role == "assistant" {
            let parsed = telegram::parse_payload_auto(&entry.text);
            if parsed.is_empty() {
                entry.text.clone()
            } else {
                telegram::format_raw_payload(&parsed)
            }
        } else {
            entry.text.clone()
        };
        println!(
            "{:>5} [{}] {}{}: {}",
            line_num, entry.ts, role_display, from_info, display_text
        );
    }
}

fn handle_cron_register(
    prompt: &str,
    at_value: &str,
    chat_id: i64,
    hash_key: &str,
    once: bool,
    session_id: Option<&str>,
) {
    use services::telegram;

    cron_debug("========================================");
    cron_debug("=== handle_cron_register START ===");
    cron_debug("========================================");
    cron_debug(&format!("  prompt: {}", prompt));
    cron_debug(&format!("  at_value: {}", at_value));
    cron_debug(&format!("  chat_id: {}", chat_id));
    cron_debug("  key_supplied: true");
    cron_debug(&format!("  once(raw): {}", once));
    cron_debug(&format!("  session_id: {:?}", session_id));

    let now = chrono::Local::now();
    cron_debug(&format!("  now: {}", now.format("%Y-%m-%d %H:%M:%S%.3f")));

    // Determine schedule_type and schedule value
    cron_debug("  Parsing --at value...");
    let (schedule_type, schedule_value) = if let Some(dt) =
        telegram::parse_relative_time_pub(at_value)
    {
        // Relative time → convert to absolute
        cron_debug(&format!(
            "  Parsed as relative time → absolute: {}",
            dt.format("%Y-%m-%d %H:%M:%S")
        ));
        (
            "absolute".to_string(),
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        )
    } else if at_value.split_whitespace().count() == 5 {
        // Cron expression (5 fields)
        cron_debug(&format!("  Parsed as cron expression: {}", at_value));
        ("cron".to_string(), at_value.to_string())
    } else {
        // Try absolute time: "YYYY-MM-DD HH:MM:SS"
        if chrono::NaiveDateTime::parse_from_str(at_value, "%Y-%m-%d %H:%M:%S").is_ok() {
            cron_debug(&format!("  Parsed as absolute time: {}", at_value));
            ("absolute".to_string(), at_value.to_string())
        } else {
            cron_debug(&format!("  ERROR: invalid --at value: {}", at_value));
            eprintln!(
                "{}",
                serde_json::json!({"status":"error","message":format!("invalid --at value: {}", at_value)})
            );
            std::process::exit(1);
        }
    };
    cron_debug(&format!(
        "  schedule_type={}, schedule_value={}",
        schedule_type, schedule_value
    ));

    // Generate 8-char uppercase hex ID (0-9, A-F), unique among existing live
    // schedule files and retained history files. History is intentionally kept
    // after manual removal so follow-up questions can inspect what happened; do
    // not reuse a schedule ID that still has history.
    cron_debug("  Generating unique ID...");
    let id = {
        use std::collections::HashSet;
        let existing: HashSet<String> = telegram::list_all_schedule_ids_pub();
        cron_debug(&format!("  Existing schedule IDs: {:?}", existing));
        loop {
            let candidate = format!("{:08X}", rand::random::<u32>());
            let history_exists = telegram::schedule_history_path_pub(&candidate)
                .map(|p| p.exists())
                .unwrap_or(false);
            if !existing.contains(&candidate) && !history_exists {
                cron_debug(&format!("  Generated ID: {}", candidate));
                break candidate;
            }
            cron_debug(&format!(
                "  ID collision/reserved by history: {}, retrying...",
                candidate
            ));
        }
    };

    // Resolve current_path from bot_settings using chat_id + hash_key
    cron_debug("  Resolving current_path...");
    let current_path =
        telegram::resolve_current_path_for_chat(chat_id, hash_key).unwrap_or_else(|| {
            let fallback = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "/".to_string());
            cron_debug(&format!("  current_path fallback: {}", fallback));
            fallback
        });
    cron_debug(&format!("  current_path: {}", current_path));

    // Resolve the current chat model/provider now. The scheduled task may run
    // later, but the captured session_id belongs to the provider active at
    // registration time.
    cron_debug("  Resolving current model/provider...");
    let mut current_model = telegram::resolve_model_for_chat(chat_id, hash_key);
    let mut provider = telegram::detect_provider_pub(current_model.as_deref()).to_string();
    if let Some(source_sid) = session_id {
        match telegram::resolve_session_provider_pub(source_sid, Some(&provider)) {
            Some(resolved_provider) => {
                if resolved_provider != provider {
                    cron_debug(&format!(
                        "  provider adjusted from model setting: {} -> {}",
                        provider, resolved_provider
                    ));
                    provider = resolved_provider;
                }
                if matches!(
                    current_model
                        .as_deref()
                        .map(|model| telegram::detect_provider_pub(Some(model))),
                    Some(model_provider) if model_provider != provider.as_str()
                ) {
                    cron_debug(&format!(
                        "  model {:?} belongs to a different provider; clearing stored model for schedule",
                        current_model
                    ));
                    current_model = None;
                }
            }
            None => {
                cron_debug(&format!(
                    "  ERROR: --session could not be resolved to a provider: {}",
                    source_sid
                ));
                eprintln!(
                    "{}",
                    serde_json::json!({"status":"error","message":format!("--session could not be resolved to a provider: {}", source_sid)})
                );
                std::process::exit(1);
            }
        }
    }
    cron_debug(&format!("  current_model: {:?}", current_model));
    cron_debug(&format!("  provider: {}", provider));

    // Register the schedule with the source session metadata needed for
    // execution-time cloning.
    cron_debug("  Writing schedule entry with source session metadata...");
    telegram::write_schedule_entry_pub(&telegram::ScheduleEntryData {
        id: id.clone(),
        chat_id,
        bot_key: hash_key.to_string(),
        current_path: current_path.clone(),
        prompt: prompt.to_string(),
        schedule: schedule_value.clone(),
        schedule_type: schedule_type.clone(),
        once: if schedule_type == "cron" {
            Some(once)
        } else {
            None
        },
        last_run: None,
        created_at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
        session_id: session_id.map(str::to_string),
        provider: Some(provider.clone()),
        model: current_model.clone(),
        context_summary: None,
    })
    .unwrap_or_else(|e| {
        cron_debug(&format!("  ERROR: write_schedule_entry failed: {}", e));
        eprintln!(
            "{}",
            serde_json::json!({"status":"error","message":format!("{}", e)})
        );
        std::process::exit(1);
    });
    cron_debug("  Schedule entry written successfully");

    let mut output = serde_json::json!({
        "status": "ok",
        "kind": "cron_register",
        "id": id,
        "prompt": prompt,
        "schedule": schedule_value,
        "schedule_type": schedule_type,
    });
    if schedule_type == "cron" {
        output
            .as_object_mut()
            .unwrap()
            .insert("once".to_string(), serde_json::json!(once));
    }
    // Embed a usage hint that binds this specific schedule_id to the --cron-history
    // command. This gives the AI a direct, in-output mapping ("for THIS id, run THIS
    // exact command"), which is stronger than the generic system-prompt instruction
    // alone — useful for follow-up turns where the user refers to the schedule by
    // natural-language phrases like "방금 한 거" without naming the id.
    let bin = crate::utils::format::to_shell_path(bin_path());
    // Quote the binary path so spaces in install locations (notably on Windows) don't
    // break the suggested command. Matches the `\"{bin}\"` quoting used throughout the
    // system-prompt SCHEDULE sections.
    let hint = format!(
        "To inspect run history of this schedule, call: \"{}\" --cron-history {} --chat {} --key-file <PRIVATE_KEY_FILE>",
        bin, id, chat_id
    );
    output
        .as_object_mut()
        .unwrap()
        .insert("hint".to_string(), serde_json::json!(hint));
    cron_debug(&format!("  Output: {}", output));
    // Write result to temp file so the bot can read it even if Bash tool misses stdout
    if let Some(home) = dirs::home_dir() {
        let result_path = home
            .join(".cokacdir")
            .join("schedule")
            .join(format!("{}.result", id));
        let _ = services::telegram::write_private_file_atomically(
            &result_path,
            output.to_string().as_bytes(),
        );
        cron_debug(&format!("  Result file written: {}", result_path.display()));
    }
    println!("{}", output);
    // Flush stdout immediately so the Bash tool captures the output
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // Scheduled runs clone the source provider session at execution time, so
    // registration only persists metadata and exits after writing the schedule.
    if session_id.is_some() {
        cron_debug(
            "  Source session stored; schedule will clone provider session at execution time",
        );
    } else {
        cron_debug("  No session_id provided; schedule will run without source-session clone");
    }

    cron_debug("=== handle_cron_register END ===");
}

fn handle_cron_list(chat_id: i64, hash_key: &str) {
    use services::telegram;

    cron_debug(&format!(
        "[handle_cron_list] chat_id={}, key_supplied=true",
        chat_id
    ));
    let entries = telegram::list_schedule_entries_pub(hash_key, Some(chat_id));
    cron_debug(&format!(
        "[handle_cron_list] found {} entries",
        entries.len()
    ));
    let items: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            let mut obj = serde_json::json!({
                "id": e.id,
                "prompt": e.prompt,
                "schedule": e.schedule,
                "schedule_type": e.schedule_type,
                "created_at": e.created_at
            });
            if let Some(once_val) = e.once {
                obj.as_object_mut()
                    .unwrap()
                    .insert("once".to_string(), serde_json::json!(once_val));
            }
            if let Some(ref sid) = e.session_id {
                obj.as_object_mut()
                    .unwrap()
                    .insert("session_id".to_string(), serde_json::json!(sid));
            }
            if let Some(ref provider) = e.provider {
                obj.as_object_mut()
                    .unwrap()
                    .insert("provider".to_string(), serde_json::json!(provider));
            }
            if let Some(ref model) = e.model {
                obj.as_object_mut()
                    .unwrap()
                    .insert("model".to_string(), serde_json::json!(model));
            }
            obj
        })
        .collect();
    println!(
        "{}",
        serde_json::json!({"status":"ok","kind":"cron_list","schedules":items})
    );
}

fn handle_cron_remove(id: &str, chat_id: i64, hash_key: &str) {
    use services::telegram;

    cron_debug(&format!(
        "[handle_cron_remove] id={}, chat_id={}, key_supplied=true",
        id, chat_id
    ));
    // Verify ownership
    let entries = telegram::list_schedule_entries_pub(hash_key, Some(chat_id));
    if !entries.iter().any(|e| e.id == id) {
        cron_debug(&format!(
            "[handle_cron_remove] id={}, not found or access denied",
            id
        ));
        eprintln!(
            "{}",
            serde_json::json!({"status":"error","message":format!("schedule not found or access denied: {}", id)})
        );
        std::process::exit(1);
    }

    if telegram::delete_schedule_entry_pub(id) {
        // Keep run history after manual removal. Follow-up turns often need to
        // inspect why a schedule fired or was removed, and `--cron-history`
        // authorizes against retained history records when the live entry is gone.
        // ID generation avoids reusing IDs with existing history files.
        cron_debug(&format!(
            "[handle_cron_remove] id={}, deleted successfully",
            id
        ));
        println!(
            "{}",
            serde_json::json!({"status":"ok","kind":"cron_remove","id":id})
        );
    } else {
        cron_debug(&format!("[handle_cron_remove] id={}, delete failed", id));
        eprintln!(
            "{}",
            serde_json::json!({"status":"error","message":format!("failed to remove schedule: {}", id)})
        );
        std::process::exit(1);
    }
}

/// Read the run-history JSONL file for a schedule and emit its records as a JSON array.
///
/// Authorization: prefers the live schedule entry's (chat_id, key) match. If the
/// entry is gone (one-time schedule already executed and auto-deleted), falls back to
/// the first record in the history file — both `chat_id` and the key verifier must match.
/// Output:
/// `{"status":"ok","kind":"cron_history","id":"...","count":N,"history":[{...}, ...]}`.
/// An entry that exists but has never run yields `count:0, history:[]`.
fn handle_cron_history(id: &str, chat_id: i64, hash_key: &str) {
    use services::telegram;

    cron_debug(&format!(
        "[handle_cron_history] id={}, chat_id={}, key_supplied=true",
        id, chat_id
    ));

    let history_path = match telegram::schedule_history_path_pub(id) {
        Some(p) => p,
        None => {
            // `schedule_history_path_pub` returns None for malformed ids
            // (path-traversal guard) as well as a missing home dir. Either
            // way we can't locate a history file — refuse uniformly.
            cron_debug(&format!(
                "[handle_cron_history] schedule_history_path_pub returned None for id={:?}",
                id
            ));
            eprintln!(
                "{}",
                serde_json::json!({"status":"error","message":format!("schedule not found or access denied: {}", id)})
            );
            std::process::exit(1);
        }
    };

    // Authorization check 1: live schedule entry matching (chat_id, key)
    let entries = telegram::list_schedule_entries_pub(hash_key, Some(chat_id));
    let entry_authorized = entries.iter().any(|e| e.id == id);

    // Authorization check 2 (fallback for already-deleted one-time schedules):
    // first record in the history file must carry the same chat_id and key verifier.
    // We must read the file pre-redact for this check — the redact step only
    // touches legacy `bot_key` fields and can leave `bot_key_verifier` /
    // `chat_id` intact, so authorization works either way; running redact
    // before auth would let an unauthorized caller trigger writes outside
    // the schedule_history dir if they smuggled a path in via `id`.
    let mut authorized_via_history = false;
    if !entry_authorized {
        if let Ok(content) = std::fs::read_to_string(&history_path) {
            if let Some(first_line) = content.lines().find(|l| !l.trim().is_empty()) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(first_line) {
                    if telegram::schedule_history_record_authorized_pub(&v, id, chat_id, hash_key) {
                        authorized_via_history = true;
                    }
                }
            }
        }
    }

    if !entry_authorized && !authorized_via_history {
        cron_debug(&format!(
            "[handle_cron_history] id={}, not authorized (no entry, no matching history)",
            id
        ));
        eprintln!(
            "{}",
            serde_json::json!({"status":"error","message":format!("schedule not found or access denied: {}", id)})
        );
        std::process::exit(1);
    }

    // Caller is authorized — safe to redact legacy `bot_key` fields now.
    if history_path.exists() {
        telegram::redact_schedule_history_file_pub(&history_path);
    }

    // Read history file. Missing file is valid (entry exists but never ran) → empty list.
    let history: Vec<serde_json::Value> = if history_path.exists() {
        match std::fs::read_to_string(&history_path) {
            Ok(content) => content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| {
                    let mut record: serde_json::Value = serde_json::from_str(l).ok()?;
                    if !telegram::schedule_history_record_authorized_pub(
                        &record, id, chat_id, hash_key,
                    ) {
                        return None;
                    }
                    telegram::sanitize_schedule_history_record_pub(&mut record);
                    Some(record)
                })
                .collect(),
            Err(e) => {
                cron_debug(&format!(
                    "[handle_cron_history] id={}, read failed: {}",
                    id, e
                ));
                eprintln!(
                    "{}",
                    serde_json::json!({"status":"error","message":format!("failed to read history file: {}", e)})
                );
                std::process::exit(1);
            }
        }
    } else {
        Vec::new()
    };

    let output = serde_json::json!({
        "status": "ok",
        "kind": "cron_history",
        "id": id,
        "count": history.len(),
        "history": history,
    });
    cron_debug(&format!(
        "[handle_cron_history] id={}, returned {} record(s)",
        id,
        history.len()
    ));
    println!("{}", output);
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn handle_cron_update(id: &str, at_value: &str, chat_id: i64, hash_key: &str) {
    use services::telegram;

    cron_debug(&format!(
        "[handle_cron_update] id={}, at_value={:?}, chat_id={}, key_supplied=true",
        id, at_value, chat_id
    ));
    // Find the entry
    let entries = telegram::list_schedule_entries_pub(hash_key, Some(chat_id));
    let entry = entries.iter().find(|e| e.id == id);
    let Some(entry) = entry else {
        cron_debug(&format!(
            "[handle_cron_update] id={}, not found or access denied",
            id
        ));
        eprintln!(
            "{}",
            serde_json::json!({"status":"error","message":format!("schedule not found or access denied: {}", id)})
        );
        std::process::exit(1);
    };

    // Parse new schedule value
    let (schedule_type, schedule_value) = if let Some(dt) =
        telegram::parse_relative_time_pub(at_value)
    {
        cron_debug(&format!(
            "[handle_cron_update] id={}, parsed as relative → absolute: {}",
            id,
            dt.format("%Y-%m-%d %H:%M:%S")
        ));
        (
            "absolute".to_string(),
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        )
    } else if at_value.split_whitespace().count() == 5 {
        cron_debug(&format!(
            "[handle_cron_update] id={}, parsed as cron: {}",
            id, at_value
        ));
        ("cron".to_string(), at_value.to_string())
    } else if chrono::NaiveDateTime::parse_from_str(at_value, "%Y-%m-%d %H:%M:%S").is_ok() {
        cron_debug(&format!(
            "[handle_cron_update] id={}, parsed as absolute datetime: {}",
            id, at_value
        ));
        ("absolute".to_string(), at_value.to_string())
    } else {
        cron_debug(&format!(
            "[handle_cron_update] id={}, invalid --at value: {:?}",
            id, at_value
        ));
        eprintln!(
            "{}",
            serde_json::json!({"status":"error","message":format!("invalid --at value: {}", at_value)})
        );
        std::process::exit(1);
    };

    // Update and write back
    let mut updated = entry.clone();
    // `list_schedule_entries_pub` no longer surfaces the raw bot_key (it cannot
    // recover the raw key from the on-disk verifier). Re-supply it from the
    // CLI arg before write, so `From<&ScheduleEntryData> for ScheduleEntry`
    // recomputes the same verifier this entry already passed the list filter
    // with — without this line, the write would derive the verifier from an
    // empty key and orphan the schedule from its owning bot.
    updated.bot_key = hash_key.to_string();
    updated.schedule = schedule_value.clone();
    updated.schedule_type = schedule_type.clone();
    updated.last_run = None; // Reset last_run so it triggers again
                             // once is only meaningful for cron; clear it for absolute
    if schedule_type == "absolute" {
        updated.once = None;
    } else if updated.once.is_none() {
        updated.once = Some(false);
    }

    cron_debug(&format!(
        "[handle_cron_update] id={}, writing: type={}, schedule={}, last_run=None",
        id, schedule_type, schedule_value
    ));
    telegram::write_schedule_entry_pub(&updated).unwrap_or_else(|e| {
        cron_debug(&format!(
            "[handle_cron_update] id={}, write failed: {}",
            id, e
        ));
        eprintln!(
            "{}",
            serde_json::json!({"status":"error","message":format!("{}", e)})
        );
        std::process::exit(1);
    });

    cron_debug(&format!(
        "[handle_cron_update] id={}, updated successfully",
        id
    ));
    println!(
        "{}",
        serde_json::json!({"status":"ok","kind":"cron_update","id":id,"schedule":schedule_value})
    );
}

fn handle_bot_message(content: &str, to: &str, chat_id: i64, hash_key: &str) {
    use services::telegram;

    msg_debug("========================================");
    msg_debug(&format!(
        "[handle_bot_message] START: to={}, chat_id={}, key_supplied=true, content_len={}",
        to,
        chat_id,
        content.len()
    ));

    // 1. Verify sender: resolve username from --key
    msg_debug("[handle_bot_message] resolving sender from supplied key");
    let from_username = match telegram::resolve_username_by_hash(hash_key) {
        Some(u) => {
            msg_debug(&format!("[handle_bot_message] sender resolved: {}", u));
            u
        }
        None => {
            msg_debug("[handle_bot_message] sender resolution failed for supplied key");
            eprintln!(
                "{}",
                serde_json::json!({"status":"error","message":"Invalid key"})
            );
            std::process::exit(1);
        }
    };

    // 2. Verify receiver: check if --to bot exists
    let to_clean = to.strip_prefix('@').unwrap_or(to);
    let to_lower = to_clean.to_lowercase();
    msg_debug(&format!(
        "[handle_bot_message] checking receiver bot: {}",
        to_lower
    ));
    if !telegram::bot_username_exists(&to_lower) {
        msg_debug(&format!(
            "[handle_bot_message] receiver bot not found: {}",
            to_lower
        ));
        eprintln!(
            "{}",
            serde_json::json!({"status":"error","message":format!("Bot '{}' not found", to_lower)})
        );
        std::process::exit(1);
    }
    msg_debug(&format!(
        "[handle_bot_message] receiver bot confirmed: {}",
        to_lower
    ));

    // 3. Create messages directory
    msg_debug("[handle_bot_message] getting messages directory");
    let msg_dir = match telegram::messages_dir() {
        Some(d) => {
            msg_debug(&format!(
                "[handle_bot_message] messages_dir={}",
                d.display()
            ));
            d
        }
        None => {
            msg_debug("[handle_bot_message] messages_dir() returned None");
            eprintln!(
                "{}",
                serde_json::json!({"status":"error","message":"cannot determine home directory"})
            );
            std::process::exit(1);
        }
    };
    if let Err(e) = ensure_private_directory(&msg_dir) {
        msg_debug(&format!(
            "[handle_bot_message] create_dir_all failed: {}",
            e
        ));
        eprintln!(
            "{}",
            serde_json::json!({"status":"error","message":format!("failed to create messages directory: {}", e)})
        );
        std::process::exit(1);
    }
    msg_debug("[handle_bot_message] messages directory ready");

    // 4. Generate message file
    let now = chrono::Local::now();
    let timestamp = now.format("%Y%m%d_%H%M%S").to_string();
    let random_hex = format!("{:032x}", rand::random::<u128>());
    let msg_id = format!("msg_{}_{}", timestamp, random_hex);
    msg_debug(&format!(
        "[handle_bot_message] generated msg_id={}, timestamp={}",
        msg_id, timestamp
    ));

    let msg_json = serde_json::json!({
        "id": msg_id,
        "from": from_username,
        "to": to_lower,
        "chat_id": chat_id.to_string(),
        "content": content,
        "created_at": now.format("%Y-%m-%d %H:%M:%S").to_string(),
    });

    let file_path = msg_dir.join(format!("{}.json", msg_id));
    msg_debug(&format!(
        "[handle_bot_message] writing message file: {}",
        file_path.display()
    ));
    let message_bytes = serde_json::to_vec_pretty(&msg_json).unwrap_or_default();
    match write_new_private_file(&file_path, &message_bytes) {
        Ok(_) => {
            msg_debug(&format!(
                "[handle_bot_message] OK: from={}, to={}, id={}, path={}",
                from_username,
                to_lower,
                msg_id,
                file_path.display()
            ));
            println!("{}", serde_json::json!({"status":"ok","id":msg_id}));
        }
        Err(e) => {
            msg_debug(&format!("[handle_bot_message] write failed: {}", e));
            eprintln!(
                "{}",
                serde_json::json!({"status":"error","message":format!("failed to write message file: {}", e)})
            );
            std::process::exit(1);
        }
    }
    msg_debug("[handle_bot_message] END");
}

fn print_version() {
    println!("cokacdir {}", VERSION);
}

fn print_licenses() {
    println!("cokacdir project license (MIT)");
    println!("================================");
    print!("{}", PROJECT_LICENSE);
    if !PROJECT_LICENSE.ends_with('\n') {
        println!();
    }

    println!("\nThird-party notices");
    println!("===================");
    print!("{}", THIRD_PARTY_NOTICES);
    if !THIRD_PARTY_NOTICES.ends_with('\n') {
        println!();
    }

    println!("\nOpenSSL 3.6.3 license (Apache License 2.0)");
    println!("============================================");
    print!("{}", OPENSSL_LICENSE);
    if !OPENSSL_LICENSE.ends_with('\n') {
        println!();
    }
}

/// Telegram token format: `<digits>:<alphanumeric_hash>`
fn is_telegram_token(token: &str) -> bool {
    if let Some((id_part, _hash_part)) = token.split_once(':') {
        !id_part.is_empty() && id_part.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

/// Discord token format: `<base64>.<timestamp>.<hmac>` (contains dots, no colons)
fn is_discord_token(token: &str) -> bool {
    !token.contains(':') && token.chars().filter(|&c| c == '.').count() >= 2
}

/// Slack token format: `slack:<xoxb-...>,<xapp-...>` (explicit) or `<xoxb-...>,<xapp-...>` (auto).
/// Socket Mode requires both a bot token (xoxb-) and an app-level token (xapp-).
fn is_slack_token(token: &str) -> bool {
    let s = token.strip_prefix("slack:").unwrap_or(token);
    s.contains(',') && s.contains("xoxb-") && s.contains("xapp-")
}

/// Parse a Slack token pair string (e.g. `xoxb-...,xapp-...`) into (bot_token, app_token).
/// Accepts either order.
fn parse_slack_pair(s: &str) -> Option<(String, String)> {
    let s = s.strip_prefix("slack:").unwrap_or(s);
    let (a, b) = s.split_once(',')?;
    let (a, b) = (a.trim(), b.trim());
    if a.starts_with("xoxb-") && b.starts_with("xapp-") {
        Some((a.to_string(), b.to_string()))
    } else if a.starts_with("xapp-") && b.starts_with("xoxb-") {
        Some((b.to_string(), a.to_string()))
    } else {
        None
    }
}

const MAX_CCSERVER_TOKEN_INPUT_BYTES: u64 = 1024 * 1024;
const MAX_CLI_KEY_BYTES: u64 = 16 * 1024;

fn read_cli_key<R: Read>(reader: R) -> Result<String, String> {
    let mut input = String::new();
    let mut limited = reader.take(MAX_CLI_KEY_BYTES + 1);
    limited
        .read_to_string(&mut input)
        .map_err(|e| format!("failed to read authorization key: {}", e))?;
    if input.len() as u64 > MAX_CLI_KEY_BYTES {
        return Err(format!(
            "authorization key exceeds {} bytes",
            MAX_CLI_KEY_BYTES
        ));
    }
    let key = input.trim();
    if key.is_empty() {
        return Err("authorization key is empty".to_string());
    }
    Ok(key.to_string())
}

fn open_private_secret_file(
    path: &std::path::Path,
    description: &str,
) -> Result<std::fs::File, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| {
        format!(
            "failed to inspect {description} file '{}': {e}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "{description} path '{}' is not a regular file",
            path.display()
        ));
    }
    let inspected_identity =
        crate::services::file_ops::stable_path_identity(path).map_err(|e| {
            format!(
                "failed to identify {description} file '{}': {e}",
                path.display()
            )
        })?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "{description} path '{}' is a reparse point",
                path.display()
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "{description} file '{}' must not be accessible by group or other users (use chmod 600)",
                path.display()
            ));
        }
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|e| {
        format!(
            "failed to open {description} file '{}': {e}",
            path.display()
        )
    })?;
    let opened_metadata = file.metadata().map_err(|e| {
        format!(
            "failed to inspect {description} file '{}': {e}",
            path.display()
        )
    })?;
    if !opened_metadata.file_type().is_file() {
        return Err(format!(
            "{description} path '{}' is not a regular file",
            path.display()
        ));
    }
    let opened_identity = crate::services::file_ops::stable_file_identity(&file).map_err(|e| {
        format!(
            "failed to identify opened {description} file '{}': {e}",
            path.display()
        )
    })?;
    let current_identity = crate::services::file_ops::stable_path_identity(path).map_err(|e| {
        format!(
            "failed to re-identify {description} file '{}': {e}",
            path.display()
        )
    })?;
    if inspected_identity != opened_identity || current_identity != opened_identity {
        return Err(format!(
            "{description} file '{}' changed while being opened",
            path.display()
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if opened_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "{description} path '{}' is a reparse point",
                path.display()
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if opened_metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "{description} file '{}' must not be accessible by group or other users (use chmod 600)",
                path.display()
            ));
        }
    }
    Ok(file)
}

fn read_cli_key_file(path: &std::path::Path) -> Result<String, String> {
    read_cli_key(open_private_secret_file(path, "key")?)
}

fn parse_cli_key_argument(
    args: &[String],
    index: usize,
) -> Result<Option<(String, usize)>, String> {
    match args.get(index).map(String::as_str) {
        Some("--key") => {
            let value = args
                .get(index + 1)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "--key requires a value".to_string())?;
            eprintln!(
                "Warning: --key exposes the authorization key in process listings; use --key-file or --key-stdin"
            );
            Ok(Some((value.clone(), index + 2)))
        }
        Some("--key-file") => {
            let path = args
                .get(index + 1)
                .ok_or_else(|| "--key-file requires a path".to_string())?;
            Ok(Some((
                read_cli_key_file(std::path::Path::new(path))?,
                index + 2,
            )))
        }
        Some("--key-stdin") => Ok(Some((read_cli_key(std::io::stdin().lock())?, index + 1))),
        _ => Ok(None),
    }
}

fn redact_cli_args_for_log(args: &[String]) -> Vec<String> {
    let mut redacted = Vec::with_capacity(args.len());
    let mut redact_next = false;
    for argument in args {
        if redact_next {
            redacted.push("<redacted>".to_string());
            redact_next = false;
        } else if argument == "--key" {
            redacted.push(argument.clone());
            redact_next = true;
        } else if argument.starts_with("--key=") {
            redacted.push("--key=<redacted>".to_string());
        } else {
            redacted.push(argument.clone());
        }
    }
    redacted
}

fn parse_ccserver_token_input(input: &str) -> Result<Vec<String>, String> {
    let tokens: Vec<String> = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect();
    if tokens.is_empty() {
        return Err("token input is empty".to_string());
    }
    Ok(tokens)
}

fn read_ccserver_tokens<R: Read>(reader: R) -> Result<Vec<String>, String> {
    let mut input = String::new();
    let mut limited = reader.take(MAX_CCSERVER_TOKEN_INPUT_BYTES + 1);
    limited
        .read_to_string(&mut input)
        .map_err(|e| format!("failed to read token input: {}", e))?;
    if input.len() as u64 > MAX_CCSERVER_TOKEN_INPUT_BYTES {
        return Err(format!(
            "token input exceeds {} bytes",
            MAX_CCSERVER_TOKEN_INPUT_BYTES
        ));
    }
    parse_ccserver_token_input(&input)
}

fn read_ccserver_token_file(path: &std::path::Path) -> Result<Vec<String>, String> {
    read_ccserver_tokens(open_private_secret_file(path, "token")?)
}

fn handle_ccserver(tokens: Vec<String>) {
    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    // Classify tokens by format:
    //   "discord:<token>" → explicit Discord
    //   "slack:<xoxb-...>,<xapp-...>" → explicit Slack (token pair)
    //   "<digits>:<hash>" → Telegram  (e.g. 8603189801:AAHOgQ5z...)
    //   "<base64>.<ts>.<hmac>" → Discord (e.g. MTQ4OTA3..._zZ5-.fAh9...)
    //   "xoxb-...,xapp-..." → Slack (auto)
    let mut tg_tokens: Vec<String> = Vec::new();
    let mut discord_tokens: Vec<String> = Vec::new();
    let mut slack_tokens: Vec<(String, String)> = Vec::new();
    let mut invalid_tokens = 0usize;
    for token in &tokens {
        if let Some(dt) = token.strip_prefix("discord:") {
            discord_tokens.push(dt.to_string());
        } else if token.starts_with("slack:") || is_slack_token(token) {
            match parse_slack_pair(token) {
                Some(pair) => slack_tokens.push(pair),
                None => {
                    eprintln!("  [ccserver] invalid slack token format (expected: slack:xoxb-...,xapp-...)");
                    invalid_tokens += 1;
                }
            }
        } else if is_telegram_token(token) {
            tg_tokens.push(token.clone());
        } else if is_discord_token(token) {
            discord_tokens.push(token.clone());
        } else {
            // Unknown format — assume Telegram for backward compatibility
            tg_tokens.push(token.clone());
        }
    }
    if invalid_tokens > 0 {
        eprintln!("  [ccserver] aborting: {} invalid token(s)", invalid_tokens);
        std::process::exit(2);
    }

    // Log token classification
    for (i, token) in tokens.iter().enumerate() {
        let kind = if token.starts_with("discord:") {
            "discord (explicit)"
        } else if token.starts_with("slack:") {
            "slack (explicit)"
        } else if is_telegram_token(token) {
            "telegram (auto)"
        } else if is_slack_token(token) {
            "slack (auto)"
        } else if is_discord_token(token) {
            "discord (auto)"
        } else {
            "telegram (fallback)"
        };
        // Never include any portion of a credential in diagnostics.
        eprintln!("  [ccserver] token #{}: {}", i + 1, kind);
    }

    let total = tg_tokens.len() + discord_tokens.len() + slack_tokens.len();
    let title = format!("  cokacdir v{}  |  Bot Server  ", VERSION);
    let width = title.chars().count();
    println!();
    println!("  ┌{}┐", "─".repeat(width));
    println!("  │{}│", title);
    println!("  └{}┘", "─".repeat(width));
    println!();

    // Check provider availability
    let has_claude = claude::is_claude_available();
    let has_codex = codex::is_codex_available();
    let has_agy = agy::is_agy_available();
    let has_opencode = opencode::is_opencode_available();
    let mark = |available: bool| if available { "✓" } else { "✗" };
    println!(
        "  ▸ Providers    : claude {}  codex {}  agy {}  opencode {}",
        mark(has_claude),
        mark(has_codex),
        mark(has_agy),
        mark(has_opencode)
    );

    if has_agy {
        let ver = agy::agy_version().map(|s| s.as_str()).unwrap_or("unknown");
        println!("  ▸ Agy          : v{}", ver);
    }

    if !has_claude && !has_codex && !has_agy && !has_opencode {
        eprintln!();
        eprintln!("  Error: No AI provider available.");
        eprintln!("  Install Claude CLI, Codex CLI, Antigravity CLI (agy), or OpenCode.");
        std::process::exit(1);
    }

    if !tg_tokens.is_empty() {
        println!("  ▸ Telegram     : {} bot(s)", tg_tokens.len());
    }
    if !discord_tokens.is_empty() {
        println!("  ▸ Discord      : {} bot(s)", discord_tokens.len());
    }
    if !slack_tokens.is_empty() {
        println!("  ▸ Slack        : {} bot(s)", slack_tokens.len());
    }
    println!();

    if total == 1 && discord_tokens.is_empty() && slack_tokens.is_empty() {
        // Single Telegram bot — run directly. A `BotExit::Fatal`
        // (revoked token, persistent Conflict) maps to exit code 1 so
        // the supervisor (systemd, docker) restarts cokacdir instead of
        // observing a clean exit and leaving the bot dead.
        let exit = rt.block_on(services::telegram::run_bot(&tg_tokens[0], None));
        if matches!(exit, services::telegram::BotExit::Fatal) {
            std::process::exit(1);
        }
    } else if total == 1 && tg_tokens.is_empty() && slack_tokens.is_empty() {
        // Single Discord bot — run bridge directly. A `BridgeExit::Fatal`
        // (backend death, init failure) maps to exit code 1 here so
        // supervisors (systemd, docker) see the same signal they did
        // before `run_bridge` was made non-fatal-returning.
        let args = vec![discord_tokens[0].clone()];
        let exit = rt.block_on(services::messenger_bridge::run_bridge("discord", &args));
        if matches!(exit, services::messenger_bridge::BridgeExit::Fatal) {
            std::process::exit(1);
        }
    } else if total == 1 && tg_tokens.is_empty() && discord_tokens.is_empty() {
        // Single Slack bot — same exit-code handling as Discord above.
        let (bot, app) = slack_tokens[0].clone();
        let args = vec![bot, app];
        let exit = rt.block_on(services::messenger_bridge::run_bridge("slack", &args));
        if matches!(exit, services::messenger_bridge::BridgeExit::Fatal) {
            std::process::exit(1);
        }
    } else {
        // Multiple bots — spawn all concurrently. A backend death in one
        // bridge no longer kills the others (previously `run_bridge`
        // called `process::exit(1)` directly, which also tore down
        // unrelated Telegram/Slack/Discord bots). Instead each bridge
        // sets a shared `any_fatal` flag, and the process exits with
        // status 1 only after every bot's task has finished — so
        // healthy bots keep serving traffic until they exit on their
        // own (or the operator restarts cokacdir).
        rt.block_on(async {
            let any_fatal = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut handles = Vec::new();
            for token in tg_tokens {
                let any_fatal = any_fatal.clone();
                handles.push(tokio::spawn(async move {
                    let exit = services::telegram::run_bot(&token, None).await;
                    if matches!(exit, services::telegram::BotExit::Fatal) {
                        any_fatal.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }));
            }
            for dt in discord_tokens {
                let any_fatal = any_fatal.clone();
                handles.push(tokio::spawn(async move {
                    let args = vec![dt];
                    let exit = services::messenger_bridge::run_bridge("discord", &args).await;
                    if matches!(exit, services::messenger_bridge::BridgeExit::Fatal) {
                        any_fatal.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }));
            }
            for (bot, app) in slack_tokens {
                let any_fatal = any_fatal.clone();
                handles.push(tokio::spawn(async move {
                    let args = vec![bot, app];
                    let exit = services::messenger_bridge::run_bridge("slack", &args).await;
                    if matches!(exit, services::messenger_bridge::BridgeExit::Fatal) {
                        any_fatal.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }));
            }
            for handle in handles {
                let _ = handle.await;
            }
            if any_fatal.load(std::sync::atomic::Ordering::Relaxed) {
                std::process::exit(1);
            }
        });
    }
}

fn handle_prompt(prompt: &str) -> Result<(), String> {
    use crate::ui::theme::Theme;

    // Check if Claude is available
    if !claude::is_claude_available() {
        return Err(
            "Claude CLI is not available. Install it from https://claude.ai/cli".to_string(),
        );
    }

    // Execute Claude command
    let current_dir = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let response = claude::execute_command(prompt, None, &current_dir, None, None);

    if !response.success {
        return Err(response
            .error
            .unwrap_or_else(|| "Claude command failed with an unknown error".to_string()));
    }

    let content = response.response.unwrap_or_default();

    // Normalize empty lines first
    let normalized = normalize_consecutive_empty_lines(&content);

    // Render markdown
    let theme = Theme::default();
    let md_theme = MarkdownTheme::from_theme(&theme);
    let lines = render_markdown(&normalized, md_theme);

    // Remove consecutive empty lines from rendered output
    let mut prev_was_empty = false;
    for line in lines {
        let is_empty = is_line_empty(&line);
        if is_empty {
            if !prev_was_empty {
                println!();
            }
            prev_was_empty = true;
        } else {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("{}", content);
            prev_was_empty = false;
        }
    }
    Ok(())
}

/// Internal smoke test: drive `opencode::execute_command_streaming` with a
/// single prompt, print every `StreamMessage` to stdout, and assert a minimum
/// set of expected events. Used for post-build verification of the new SSE
/// adapter (and the legacy path when `COKACDIR_OPENCODE_LEGACY=1`).
///
/// Usage: `cokacdir --test-opencode-sse "<prompt>" [--model provider/model]
///                                                 [--session <sid>]
///                                                 [--dir <path>]`
///
/// Exit code: 0 on PASS, 1 on FAIL, 2 on usage error.
fn test_opencode_sse(prompt: &str, extra: &[String]) -> i32 {
    use crate::services::claude::StreamMessage;
    use std::sync::mpsc;

    // Parse optional flags after the prompt
    let mut model: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut working_dir: Option<String> = None;
    let mut inject_agent: Option<String> = None;
    let mut expect_error = false;
    let mut cancel_after_ms: Option<u64> = None;
    let mut expect_cancelled = false;
    let mut i = 0;
    while i < extra.len() {
        match extra[i].as_str() {
            "--model" => {
                if i + 1 < extra.len() {
                    model = Some(extra[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("[TEST] --model requires a value");
                    return 2;
                }
            }
            "--session" => {
                if i + 1 < extra.len() {
                    session_id = Some(extra[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("[TEST] --session requires a value");
                    return 2;
                }
            }
            "--dir" => {
                if i + 1 < extra.len() {
                    working_dir = Some(extra[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("[TEST] --dir requires a value");
                    return 2;
                }
            }
            "--agent" => {
                if i + 1 < extra.len() {
                    inject_agent = Some(extra[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("[TEST] --agent requires a value");
                    return 2;
                }
            }
            "--expect-error" => {
                expect_error = true;
                i += 1;
            }
            "--cancel-after" => {
                if i + 1 < extra.len() {
                    match extra[i + 1].parse::<u64>() {
                        Ok(ms) => cancel_after_ms = Some(ms),
                        Err(e) => {
                            eprintln!("[TEST] --cancel-after parse error: {}", e);
                            return 2;
                        }
                    }
                    i += 2;
                } else {
                    eprintln!("[TEST] --cancel-after requires a value (milliseconds)");
                    return 2;
                }
            }
            "--expect-cancelled" => {
                expect_cancelled = true;
                i += 1;
            }
            other => {
                eprintln!("[TEST] unknown arg: {}", other);
                return 2;
            }
        }
    }
    // Stash the agent override in an env var that the test harness can read
    // at the HTTP layer. The public execute_command_streaming signature has
    // no agent parameter, so this is the least invasive way to inject an
    // agent for plugin-based smoke tests.
    if let Some(agent) = inject_agent.as_ref() {
        std::env::set_var("COKACDIR_OPENCODE_TEST_AGENT", agent);
    } else {
        std::env::remove_var("COKACDIR_OPENCODE_TEST_AGENT");
    }

    let wd = working_dir.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });
    println!(
        "[TEST] start prompt_len={} model={:?} session={:?} dir={} legacy={}",
        prompt.len(),
        model,
        session_id,
        wd,
        std::env::var("COKACDIR_OPENCODE_LEGACY").unwrap_or_default()
    );

    // Turn on opencode debug log sink so ~/.cokacdir/debug/opencode.log captures details.
    crate::services::claude::DEBUG_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[TEST] failed to build tokio runtime: {}", e);
            return 1;
        }
    };

    let prompt_owned = prompt.to_string();
    let wd_owned = wd.clone();
    let session_owned = session_id.clone();
    let model_owned = model.clone();

    // If caller asked for cancel, create a real CancelToken so we can flip
    // the `cancelled` atomic from a timer task. Otherwise pass None so the
    // adapter runs to completion.
    let cancel_token_outer: Option<std::sync::Arc<crate::services::claude::CancelToken>> =
        if cancel_after_ms.is_some() {
            Some(std::sync::Arc::new(
                crate::services::claude::CancelToken::new(),
            ))
        } else {
            None
        };

    let summary = rt.block_on(async move {
        let (tx, rx) = mpsc::channel();
        let prompt = prompt_owned.clone();
        let wd = wd_owned.clone();
        let session = session_owned.clone();
        let model = model_owned.clone();
        let cancel_token = cancel_token_outer.clone();

        // If --cancel-after is set, spawn a timer task that flips the
        // cancel flag after the requested delay. This simulates a user
        // clicking "cancel" mid-turn in the bot UI.
        if let Some(ms) = cancel_after_ms {
            if let Some(token_for_timer) = cancel_token.clone() {
                tokio::task::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    println!("[TEST] firing cancel at {}ms", ms);
                    token_for_timer
                        .cancelled
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                });
            }
        }

        // Spawn the actual call on the blocking pool — this matches how
        // telegram.rs invokes the adapter at runtime, so Handle::try_current
        // inside execute_command_streaming sees this runtime.
        let call_cancel = cancel_token.clone();
        let call = tokio::task::spawn_blocking(move || {
            opencode::execute_command_streaming(
                &prompt,
                session.as_deref(),
                &wd,
                tx,
                None,
                None,
                call_cancel,
                model.as_deref(),
                false,
                false,
            )
        });

        // Drain the message channel on a separate blocking task so the call
        // and the consumer make progress in parallel (the send side is sync
        // mpsc, so the drain must be sync too).
        let drain = tokio::task::spawn_blocking(move || -> TestSummary {
            let mut s = TestSummary::default();
            while let Ok(msg) = rx.recv() {
                s.total += 1;
                match msg {
                    StreamMessage::Init { session_id } => {
                        s.init += 1;
                        s.last_session_id = Some(session_id.clone());
                        println!("[TEST] Init session_id={}", session_id);
                    }
                    StreamMessage::Text { content } => {
                        s.text += 1;
                        let preview: String = content.chars().take(160).collect();
                        println!("[TEST] Text ({}B): {:?}", content.len(), preview);
                    }
                    StreamMessage::ToolUse { name, input } => {
                        s.tool_use += 1;
                        let preview: String = input.chars().take(120).collect();
                        println!(
                            "[TEST] ToolUse name={} input({}B)={:?}",
                            name,
                            input.len(),
                            preview
                        );
                    }
                    StreamMessage::ToolResult { content, is_error } => {
                        s.tool_result += 1;
                        let preview: String = content.chars().take(120).collect();
                        println!(
                            "[TEST] ToolResult is_error={} ({}B)={:?}",
                            is_error,
                            content.len(),
                            preview
                        );
                    }
                    StreamMessage::TaskNotification {
                        task_id,
                        status,
                        summary,
                    } => {
                        s.task_notif += 1;
                        println!(
                            "[TEST] TaskNotification task_id={} status={} summary={}",
                            task_id, status, summary
                        );
                    }
                    StreamMessage::Done { result, session_id } => {
                        s.done += 1;
                        s.last_session_id = session_id.clone();
                        s.done_result_len = result.len();
                        let preview: String = result.chars().take(240).collect();
                        println!(
                            "[TEST] Done session_id={:?} result({}B)={:?}",
                            session_id,
                            result.len(),
                            preview
                        );
                    }
                    StreamMessage::Error {
                        message,
                        stdout,
                        stderr,
                        exit_code,
                    } => {
                        s.error += 1;
                        s.last_error = Some(message.clone());
                        println!(
                            "[TEST] Error exit_code={:?} stdout_len={} stderr_len={} message={}",
                            exit_code,
                            stdout.len(),
                            stderr.len(),
                            message
                        );
                    }
                }
            }
            s
        });

        let call_res = call.await;
        let summary = drain.await.unwrap_or_default();
        println!(
            "[TEST] call_result={:?}",
            call_res.as_ref().map(|r| match r {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("err:{}", e),
            })
        );
        summary
    });

    println!(
        "[TEST] summary total={} init={} text={} tool_use={} tool_result={} task_notif={} done={} error={} done_result_len={} last_session={:?} last_error={:?}",
        summary.total,
        summary.init,
        summary.text,
        summary.tool_use,
        summary.tool_result,
        summary.task_notif,
        summary.done,
        summary.error,
        summary.done_result_len,
        summary.last_session_id,
        summary.last_error
    );

    let pass = if expect_cancelled {
        // Cancel path: the SSE adapter deliberately emits neither Done nor
        // Error on clean cancel (legacy parity). Init should have landed
        // before the cancel signal was honoured.
        summary.init >= 1 && summary.done == 0 && summary.error == 0
    } else if expect_error {
        // Caller said an error is the expected outcome (e.g. bogus model).
        summary.init >= 1 && summary.error >= 1 && summary.done == 0
    } else {
        summary.init >= 1 && summary.done >= 1 && summary.error == 0
    };
    if pass {
        println!(
            "[TEST] RESULT: PASS (expect_error={} expect_cancelled={})",
            expect_error, expect_cancelled
        );
        0
    } else {
        println!(
            "[TEST] RESULT: FAIL (expect_error={} expect_cancelled={})",
            expect_error, expect_cancelled
        );
        1
    }
}

#[derive(Default, Debug)]
struct TestSummary {
    total: u32,
    init: u32,
    text: u32,
    tool_use: u32,
    tool_result: u32,
    task_notif: u32,
    done: u32,
    error: u32,
    last_session_id: Option<String>,
    last_error: Option<String>,
    done_result_len: usize,
}

/// Normalize consecutive empty lines to maximum of one
fn normalize_consecutive_empty_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result_lines: Vec<&str> = Vec::new();
    let mut prev_was_empty = false;

    for line in lines {
        let is_empty = line.chars().all(|c| c.is_whitespace());
        if is_empty {
            if !prev_was_empty {
                result_lines.push("");
            }
            prev_was_empty = true;
        } else {
            result_lines.push(line);
            prev_was_empty = false;
        }
    }

    result_lines.join("\n")
}

/// Deploy bundled documentation files to ~/.cokacdir/docs/
fn deploy_docs() {
    const DOCS: &[(&str, &str)] = &[
        (
            "how-to-install.md",
            include_str!("../docs/how-to-install.md"),
        ),
        ("how-to-update.md", include_str!("../docs/how-to-update.md")),
        (
            "how-to-manage-tokens.md",
            include_str!("../docs/how-to-manage-tokens.md"),
        ),
        (
            "how-to-setup-telegram-bot.md",
            include_str!("../docs/how-to-setup-telegram-bot.md"),
        ),
        (
            "how-to-setup-discord-bot.md",
            include_str!("../docs/how-to-setup-discord-bot.md"),
        ),
        (
            "how-to-start-first-chat.md",
            include_str!("../docs/how-to-start-first-chat.md"),
        ),
        (
            "how-to-use-start-session-and-clear.md",
            include_str!("../docs/how-to-use-start-session-and-clear.md"),
        ),
        (
            "how-to-set-instructions.md",
            include_str!("../docs/how-to-set-instructions.md"),
        ),
        (
            "how-to-manage-requests.md",
            include_str!("../docs/how-to-manage-requests.md"),
        ),
        (
            "how-to-use-group-chat.md",
            include_str!("../docs/how-to-use-group-chat.md"),
        ),
        (
            "how-to-use-schedules.md",
            include_str!("../docs/how-to-use-schedules.md"),
        ),
        (
            "how-to-simulate-multiple-chats-with-one-bot.md",
            include_str!("../docs/how-to-simulate-multiple-chats-with-one-bot.md"),
        ),
        (
            "how-to-install-claude-code-on-windows.md",
            include_str!("../docs/how-to-install-claude-code-on-windows.md"),
        ),
        (
            "how-to-install-codex-on-windows.md",
            include_str!("../docs/how-to-install-codex-on-windows.md"),
        ),
        (
            "how-to-configure-environment-variables.md",
            include_str!("../docs/how-to-configure-environment-variables.md"),
        ),
        (
            "how-to-configure-settings.md",
            include_str!("../docs/how-to-configure-settings.md"),
        ),
        (
            "how-to-manage-tools.md",
            include_str!("../docs/how-to-manage-tools.md"),
        ),
        (
            "how-to-setup-slack-bot.md",
            include_str!("../docs/how-to-setup-slack-bot.md"),
        ),
        (
            "how-to-use-file-transfer.md",
            include_str!("../docs/how-to-use-file-transfer.md"),
        ),
        (
            "how-to-use-shell-commands.md",
            include_str!("../docs/how-to-use-shell-commands.md"),
        ),
        (
            "how-to-use-agy-antigravity.md",
            include_str!("../docs/how-to-use-agy-antigravity.md"),
        ),
        (
            "how-to-share-bot-with-others.md",
            include_str!("../docs/how-to-share-bot-with-others.md"),
        ),
    ];
    if let Some(home) = dirs::home_dir() {
        let docs_dir = home.join(".cokacdir").join("docs");
        let _ = std::fs::create_dir_all(&docs_dir);
        for (name, content) in DOCS {
            let path = docs_dir.join(name);
            let _ = services::telegram::write_private_file_atomically(&path, content.as_bytes());
        }
    }
}

/// Load ~/.cokacdir/.env.json and set environment variables.
/// Values from this file take priority over the existing environment.
fn is_valid_environment_entry(key: &str, value: &str) -> bool {
    !key.is_empty() && !key.contains(['=', '\0']) && !value.contains('\0')
}

fn load_dot_env() {
    let env_path = match dirs::home_dir() {
        Some(h) => h.join(".cokacdir").join(".env.json"),
        None => return,
    };
    let before = match std::fs::symlink_metadata(&env_path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            metadata
        }
        _ => return,
    };
    const MAX_ENV_FILE_BYTES: u64 = 1024 * 1024;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return;
        }
    }
    if before.len() > MAX_ENV_FILE_BYTES {
        eprintln!("Warning: ~/.cokacdir/.env.json exceeds the 1 MiB size limit");
        return;
    }
    let inspected_identity = match crate::services::file_ops::stable_path_identity(&env_path) {
        Ok(identity) => identity,
        Err(_) => return,
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = match options.open(&env_path) {
        Ok(file) => file,
        Err(_) => return,
    };
    let opened = match file.metadata() {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_ENV_FILE_BYTES => metadata,
        _ => return,
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return;
        }
    }
    let opened_identity = match crate::services::file_ops::stable_file_identity(&file) {
        Ok(identity) => identity,
        Err(_) => return,
    };
    if inspected_identity != opened_identity
        || crate::services::file_ops::stable_path_identity(&env_path).ok() != Some(opened_identity)
    {
        return;
    }
    let mut bytes = Vec::new();
    let content = match Read::by_ref(&mut file)
        .take(MAX_ENV_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        Ok(_) if bytes.len() as u64 <= MAX_ENV_FILE_BYTES => match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => return,
        },
        Err(_) => return,
        Ok(_) => return,
    };
    let after = match file.metadata() {
        Ok(metadata) => metadata,
        Err(_) => return,
    };
    if crate::services::file_ops::stable_file_identity(&file).ok() != Some(opened_identity)
        || crate::services::file_ops::stable_path_identity(&env_path).ok() != Some(opened_identity)
    {
        eprintln!("Warning: ~/.cokacdir/.env.json changed while being read");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if after.dev() != opened.dev()
            || after.ino() != opened.ino()
            || after.len() != opened.len()
            || after.mtime() != opened.mtime()
            || after.mtime_nsec() != opened.mtime_nsec()
            || after.ctime() != opened.ctime()
            || after.ctime_nsec() != opened.ctime_nsec()
        {
            eprintln!("Warning: ~/.cokacdir/.env.json changed while being read");
            return;
        }
    }
    #[cfg(not(unix))]
    if after.len() != opened.len() || after.modified().ok() != opened.modified().ok() {
        eprintln!("Warning: ~/.cokacdir/.env.json changed while being read");
        return;
    }
    let map: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Warning: failed to parse ~/.cokacdir/.env.json: {}", e);
            return;
        }
    };
    for (key, val) in &map {
        let value = if let Some(s) = val.as_str() {
            Some(s.to_string())
        } else if val.is_number() || val.is_boolean() {
            Some(val.to_string())
        } else {
            eprintln!(
                "Warning: ~/.cokacdir/.env.json: skipping {:?} (unsupported value type)",
                key.escape_debug().to_string()
            );
            None
        };
        if let Some(value) = value {
            if is_valid_environment_entry(key, &value) {
                std::env::set_var(key, value);
            } else {
                eprintln!("Warning: ~/.cokacdir/.env.json: skipping invalid environment entry");
            }
        }
    }
}

fn main() -> io::Result<()> {
    // Agy invokes this private hook entry point before each model call. Handle
    // it before normal startup so the hook neither loads user env overrides
    // nor initializes the TUI, bot state, or deployed documentation.
    let mut early_args = std::env::args_os();
    let _program = early_args.next();
    let first_early_arg = early_args.next();
    let has_more_early_args = early_args.next().is_some();
    if first_early_arg.as_deref()
        == Some(std::ffi::OsStr::new("--internal-agy-pre-invocation-hook"))
        && !has_more_early_args
    {
        return agy::run_agy_pre_invocation_hook();
    }
    // Keep license material available even when no home directory can be
    // resolved and normal application initialization is therefore impossible.
    if first_early_arg.as_deref() == Some(std::ffi::OsStr::new("--licenses"))
        && !has_more_early_args
    {
        print_licenses();
        return Ok(());
    }

    // Resolve binary path at startup (works on Linux, macOS, Windows)
    init_bin_path();

    // Create and repair the private application directory before reading env
    // files, deploying docs, or writing bearer-bearing upload queue entries.
    let config_dir = match dirs::home_dir() {
        Some(home) => home.join(".cokacdir"),
        None => {
            eprintln!("Error: Cannot determine home directory. ~/.cokacdir is required.");
            std::process::exit(1);
        }
    };
    if let Err(e) = ensure_private_directory(&config_dir) {
        eprintln!("Error: Cannot secure ~/.cokacdir: {}", e);
        std::process::exit(1);
    }

    // Load ~/.cokacdir/.env.json — overrides existing environment variables
    load_dot_env();

    // Initialize debug flag from environment variable
    claude::init_debug_from_env();

    // Deploy documentation to ~/.cokacdir/docs/
    deploy_docs();

    // Handle command line arguments
    let args: Vec<String> = env::args().collect();
    let mut design_mode = false;
    let mut start_paths: Vec<std::path::PathBuf> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-v" | "--version" => {
                print_version();
                return Ok(());
            }
            "--licenses" => {
                print_licenses();
                return Ok(());
            }
            "--prompt" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --prompt requires a text argument");
                    eprintln!("Usage: cokacdir --prompt \"your question\"");
                    std::process::exit(2);
                }
                if let Err(error) = handle_prompt(&args[i + 1]) {
                    eprintln!("Error: {}", error);
                    std::process::exit(1);
                }
                return Ok(());
            }
            "--test-opencode-sse" => {
                // Internal smoke test for the opencode SSE adapter. Drives
                // execute_command_streaming directly and prints every
                // StreamMessage to stdout in a debuggable form.
                if i + 1 >= args.len() {
                    eprintln!("Error: --test-opencode-sse requires a prompt argument");
                    std::process::exit(2);
                }
                let prompt = args[i + 1].clone();
                let extra: Vec<String> = args[i + 2..].to_vec();
                let code = test_opencode_sse(&prompt, &extra);
                std::process::exit(code);
            }
            "--base64" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --base64 requires a text argument");
                    std::process::exit(2);
                }
                handle_base64(&args[i + 1]);
                return Ok(());
            }
            "--ccserver-token-file" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --ccserver-token-file requires a path");
                    std::process::exit(2);
                }
                let tokens = match read_ccserver_token_file(std::path::Path::new(&args[i + 1])) {
                    Ok(tokens) => tokens,
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(2);
                    }
                };
                handle_ccserver(tokens);
                return Ok(());
            }
            "--ccserver-stdin" => {
                let tokens = match read_ccserver_tokens(std::io::stdin().lock()) {
                    Ok(tokens) => tokens,
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(2);
                    }
                };
                handle_ccserver(tokens);
                return Ok(());
            }
            "--ccserver" => {
                let tokens: Vec<String> = args[i + 1..]
                    .iter()
                    .filter(|a| !a.starts_with('-'))
                    .cloned()
                    .collect();
                let tokens = if tokens.is_empty() {
                    match std::env::var("COKACDIR_CCSERVER_TOKEN_FILE") {
                        Ok(path) => match read_ccserver_token_file(std::path::Path::new(&path)) {
                            Ok(tokens) => tokens,
                            Err(e) => {
                                eprintln!("Error: {}", e);
                                std::process::exit(2);
                            }
                        },
                        Err(_) => {
                            eprintln!("Error: --ccserver requires a secure token source");
                            eprintln!(
                                "Use --ccserver-token-file <PATH>, --ccserver-stdin, or set COKACDIR_CCSERVER_TOKEN_FILE"
                            );
                            std::process::exit(2);
                        }
                    }
                } else {
                    eprintln!(
                        "Warning: token arguments are visible in process listings; use --ccserver-token-file or --ccserver-stdin"
                    );
                    tokens
                };
                handle_ccserver(tokens);
                return Ok(());
            }
            "--currenttime" => {
                println!(
                    "{}",
                    serde_json::json!({"status":"ok","time":chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()})
                );
                return Ok(());
            }
            "--cron" => {
                cron_debug("=== --cron argument parsing START ===");
                cron_debug(&format!(
                    "  Args (authorization key redacted): {:?}",
                    redact_cli_args_for_log(&args[i..])
                ));
                // Parse: --cron "prompt" --at "time" --chat ID <key source> [--once] [--session SID]
                let mut prompt: Option<String> = None;
                let mut at_value: Option<String> = None;
                let mut chat_id: Option<i64> = None;
                let mut key: Option<String> = None;
                let mut once = false;
                let mut session_id: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--at" => {
                            if j + 1 < args.len() {
                                at_value = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--chat" => {
                            if j + 1 < args.len() {
                                chat_id = args[j + 1].parse().ok();
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--key" | "--key-file" | "--key-stdin" => {
                            match parse_cli_key_argument(&args, j) {
                                Ok(Some((value, next))) => {
                                    key = Some(value);
                                    j = next;
                                }
                                Ok(None) => unreachable!(),
                                Err(error) => {
                                    eprintln!("Error: {}", error);
                                    std::process::exit(2);
                                }
                            }
                        }
                        "--session" => {
                            if j + 1 < args.len() {
                                session_id = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--once" => {
                            once = true;
                            j += 1;
                        }
                        _ if prompt.is_none() && !args[j].starts_with("--") => {
                            prompt = Some(args[j].clone());
                            j += 1;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                cron_debug(&format!(
                    "  Parsed: prompt={:?}, at={:?}, chat_id={:?}, key_supplied={}, once={}, session_id={:?}",
                    prompt,
                    at_value,
                    chat_id,
                    key.is_some(),
                    once,
                    session_id
                ));
                match (prompt, at_value, chat_id, key) {
                    (Some(p), Some(at), Some(cid), Some(k)) => {
                        cron_debug("  All required args present, calling handle_cron_register");
                        handle_cron_register(&p, &at, cid, &k, once, session_id.as_deref());
                    }
                    _ => {
                        cron_debug("  ERROR: Missing required arguments");
                        eprintln!(
                            "{}",
                            serde_json::json!({"status":"error","message":"--cron requires \"prompt\", --at \"time\", --chat <ID>, and --key-file <PATH> (or --key-stdin)"})
                        );
                        std::process::exit(2);
                    }
                }
                cron_debug("=== --cron argument parsing END ===");
                return Ok(());
            }
            "--cron-context" => {
                cron_debug("  ERROR: --cron-context is no longer supported");
                eprintln!(
                    "{}",
                    serde_json::json!({"status":"error","message":"--cron-context is no longer supported; scheduled runs clone provider sessions at execution time"})
                );
                std::process::exit(2);
            }
            "--cron-list" => {
                let mut chat_id: Option<i64> = None;
                let mut key: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--chat" => {
                            if j + 1 < args.len() {
                                chat_id = args[j + 1].parse().ok();
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--key" | "--key-file" | "--key-stdin" => {
                            match parse_cli_key_argument(&args, j) {
                                Ok(Some((value, next))) => {
                                    key = Some(value);
                                    j = next;
                                }
                                Ok(None) => unreachable!(),
                                Err(error) => {
                                    eprintln!("Error: {}", error);
                                    std::process::exit(2);
                                }
                            }
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (chat_id, key) {
                    (Some(cid), Some(k)) => handle_cron_list(cid, &k),
                    _ => {
                        eprintln!(
                            "{}",
                            serde_json::json!({"status":"error","message":"--cron-list requires --chat <ID> and --key-file <PATH> (or --key-stdin)"})
                        );
                        std::process::exit(2);
                    }
                }
                return Ok(());
            }
            "--cron-remove" => {
                let mut sched_id: Option<String> = None;
                let mut chat_id: Option<i64> = None;
                let mut key: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--chat" => {
                            if j + 1 < args.len() {
                                chat_id = args[j + 1].parse().ok();
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--key" | "--key-file" | "--key-stdin" => {
                            match parse_cli_key_argument(&args, j) {
                                Ok(Some((value, next))) => {
                                    key = Some(value);
                                    j = next;
                                }
                                Ok(None) => unreachable!(),
                                Err(error) => {
                                    eprintln!("Error: {}", error);
                                    std::process::exit(2);
                                }
                            }
                        }
                        _ if sched_id.is_none() && !args[j].starts_with("--") => {
                            sched_id = Some(args[j].clone());
                            j += 1;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (sched_id, chat_id, key) {
                    (Some(sid), Some(cid), Some(k)) => handle_cron_remove(&sid, cid, &k),
                    _ => {
                        eprintln!(
                            "{}",
                            serde_json::json!({"status":"error","message":"--cron-remove requires <ID>, --chat <ID>, and --key-file <PATH> (or --key-stdin)"})
                        );
                        std::process::exit(2);
                    }
                }
                return Ok(());
            }
            "--cron-history" => {
                let mut sched_id: Option<String> = None;
                let mut chat_id: Option<i64> = None;
                let mut key: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--chat" => {
                            if j + 1 < args.len() {
                                chat_id = args[j + 1].parse().ok();
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--key" | "--key-file" | "--key-stdin" => {
                            match parse_cli_key_argument(&args, j) {
                                Ok(Some((value, next))) => {
                                    key = Some(value);
                                    j = next;
                                }
                                Ok(None) => unreachable!(),
                                Err(error) => {
                                    eprintln!("Error: {}", error);
                                    std::process::exit(2);
                                }
                            }
                        }
                        _ if sched_id.is_none() && !args[j].starts_with("--") => {
                            sched_id = Some(args[j].clone());
                            j += 1;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (sched_id, chat_id, key) {
                    (Some(sid), Some(cid), Some(k)) => handle_cron_history(&sid, cid, &k),
                    _ => {
                        eprintln!(
                            "{}",
                            serde_json::json!({"status":"error","message":"--cron-history requires <ID>, --chat <ID>, and --key-file <PATH> (or --key-stdin)"})
                        );
                        std::process::exit(2);
                    }
                }
                return Ok(());
            }
            "--cron-update" => {
                let mut sched_id: Option<String> = None;
                let mut at_value: Option<String> = None;
                let mut chat_id: Option<i64> = None;
                let mut key: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--at" => {
                            if j + 1 < args.len() {
                                at_value = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--chat" => {
                            if j + 1 < args.len() {
                                chat_id = args[j + 1].parse().ok();
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--key" | "--key-file" | "--key-stdin" => {
                            match parse_cli_key_argument(&args, j) {
                                Ok(Some((value, next))) => {
                                    key = Some(value);
                                    j = next;
                                }
                                Ok(None) => unreachable!(),
                                Err(error) => {
                                    eprintln!("Error: {}", error);
                                    std::process::exit(2);
                                }
                            }
                        }
                        _ if sched_id.is_none() && !args[j].starts_with("--") => {
                            sched_id = Some(args[j].clone());
                            j += 1;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (sched_id, at_value, chat_id, key) {
                    (Some(sid), Some(at), Some(cid), Some(k)) => {
                        handle_cron_update(&sid, &at, cid, &k)
                    }
                    _ => {
                        eprintln!(
                            "{}",
                            serde_json::json!({"status":"error","message":"--cron-update requires <ID>, --at \"time\", --chat <ID>, and --key-file <PATH> (or --key-stdin)"})
                        );
                        std::process::exit(2);
                    }
                }
                return Ok(());
            }
            "--sendfile" => {
                // Parse: --sendfile <PATH> --chat <ID> <key source>
                let mut file_path: Option<String> = None;
                let mut chat_id: Option<i64> = None;
                let mut key: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--chat" => {
                            if j + 1 < args.len() {
                                chat_id = args[j + 1].parse().ok();
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--key" | "--key-file" | "--key-stdin" => {
                            match parse_cli_key_argument(&args, j) {
                                Ok(Some((value, next))) => {
                                    key = Some(value);
                                    j = next;
                                }
                                Ok(None) => unreachable!(),
                                Err(error) => {
                                    eprintln!("Error: {}", error);
                                    std::process::exit(2);
                                }
                            }
                        }
                        _ if file_path.is_none() && !args[j].starts_with("--") => {
                            file_path = Some(args[j].clone());
                            j += 1;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (file_path, chat_id, key) {
                    (Some(fp), Some(cid), Some(k)) => {
                        handle_sendfile(&fp, cid, &k);
                    }
                    _ => {
                        eprintln!(
                            "{}",
                            serde_json::json!({"status":"error","message":"--sendfile requires <PATH>, --chat <ID>, and --key-file <PATH> (or --key-stdin)"})
                        );
                        std::process::exit(2);
                    }
                }
                return Ok(());
            }
            "--message" => {
                // Parse: --message <TEXT> --to <BOT> --chat <ID> <key source>
                msg_debug(&format!(
                    "[main:--message] parsing args starting at i={}, remaining_args={}",
                    i,
                    args.len() - i - 1
                ));
                let mut message: Option<String> = None;
                let mut to_bot: Option<String> = None;
                let mut chat_id: Option<i64> = None;
                let mut key: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    msg_debug(&format!("[main:--message] examining arg index {}", j));
                    match args[j].as_str() {
                        "--to" => {
                            if j + 1 < args.len() {
                                to_bot = Some(args[j + 1].clone());
                                msg_debug(&format!("[main:--message] --to={}", args[j + 1]));
                                j += 2;
                            } else {
                                msg_debug("[main:--message] --to missing value");
                                j += 1;
                            }
                        }
                        "--chat" => {
                            if j + 1 < args.len() {
                                chat_id = args[j + 1].parse().ok();
                                msg_debug(&format!(
                                    "[main:--message] --chat={:?} (parsed={:?})",
                                    args[j + 1],
                                    chat_id
                                ));
                                j += 2;
                            } else {
                                msg_debug("[main:--message] --chat missing value");
                                j += 1;
                            }
                        }
                        "--key" | "--key-file" | "--key-stdin" => {
                            match parse_cli_key_argument(&args, j) {
                                Ok(Some((value, next))) => {
                                    key = Some(value);
                                    msg_debug("[main:--message] authorization key supplied");
                                    j = next;
                                }
                                Ok(None) => unreachable!(),
                                Err(error) => {
                                    msg_debug("[main:--message] invalid authorization key source");
                                    eprintln!("Error: {}", error);
                                    std::process::exit(2);
                                }
                            }
                        }
                        _ if message.is_none() && !args[j].starts_with("--") => {
                            message = Some(args[j].clone());
                            msg_debug(&format!(
                                "[main:--message] message text captured: len={}",
                                args[j].len()
                            ));
                            j += 1;
                        }
                        _ => {
                            msg_debug(&format!(
                                "[main:--message] skipping unrecognized arg at index {}",
                                j
                            ));
                            j += 1;
                        }
                    }
                }
                msg_debug(&format!(
                    "[main:--message] parsed: message={}, to={}, chat_id={}, key={}",
                    message.is_some(),
                    to_bot.is_some(),
                    chat_id.is_some(),
                    key.is_some()
                ));
                match (message, to_bot, chat_id, key) {
                    (Some(msg), Some(to), Some(cid), Some(k)) => {
                        msg_debug(&format!(
                            "[main:--message] calling handle_bot_message: to={}, chat_id={}",
                            to, cid
                        ));
                        handle_bot_message(&msg, &to, cid, &k);
                    }
                    _ => {
                        msg_debug("[main:--message] incomplete arguments, showing error");
                        eprintln!(
                            "{}",
                            serde_json::json!({"status":"error","message":"--message requires <TEXT>, --to <BOT>, --chat <ID>, and --key-file <PATH> (or --key-stdin)"})
                        );
                        std::process::exit(2);
                    }
                }
                return Ok(());
            }
            "--read_chat_log" => {
                // Parse: --read_chat_log <CHAT_ID> [--range <N|START-END>] [--bot <USERNAME>]
                let mut chat_id: Option<i64> = None;
                let mut range_str: Option<String> = None;
                let mut filter_bot: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--range" => {
                            if j + 1 < args.len() {
                                range_str = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--bot" => {
                            if j + 1 < args.len() {
                                filter_bot = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        _ if chat_id.is_none() && !args[j].starts_with("--") => {
                            chat_id = args[j].parse().ok();
                            j += 1;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match chat_id {
                    Some(cid) => {
                        handle_read_group_chat(cid, range_str.as_deref(), filter_bot.as_deref())
                    }
                    None => {
                        eprintln!(
                            "{}",
                            serde_json::json!({"status":"error","message":"--read_chat_log requires <CHAT_ID>"})
                        );
                        std::process::exit(2);
                    }
                }
                return Ok(());
            }
            "--design" => {
                design_mode = true;
            }
            arg if arg.starts_with('-') => {
                eprintln!("Unknown option: {}", arg);
                eprintln!("Use --help for usage information");
                std::process::exit(2);
            }
            path => {
                // Treat as a directory path
                let p = std::path::PathBuf::from(path);
                let resolved = if p.is_absolute() {
                    p
                } else {
                    env::current_dir()
                        .unwrap_or_else(|_| {
                            if cfg!(windows) {
                                std::path::PathBuf::from("C:\\")
                            } else {
                                std::path::PathBuf::from("/")
                            }
                        })
                        .join(p)
                };
                start_paths.push(resolved);
            }
        }
        i += 1;
    }

    // Setup panic hook to restore terminal on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste,
            crossterm::cursor::Show
        );
        original_hook(panic_info);
    }));

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Clear screen before entering alternate screen
    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Detect terminal image protocol (must be after alternate screen, before event loop)
    let picker = {
        #[cfg(unix)]
        let mut p = ratatui_image::picker::Picker::from_termios()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::new((8, 16)));
        #[cfg(not(unix))]
        let mut p = ratatui_image::picker::Picker::new((8, 16));
        p.guess_protocol();
        p
    };

    // Load settings and create app state
    let (settings, settings_error) = match config::Settings::load_with_error() {
        Ok(s) => (s, None),
        Err(e) if e.is_parse_error() => (config::Settings::default(), Some(e)),
        Err(e) => {
            // Directory, file-type, permission, and I/O failures are not a
            // malformed JSON recovery case. Starting with defaults would let
            // eager settings changes write into a path we have not established
            // as safe, so fail closed after restoring the terminal.
            let _ = disable_raw_mode();
            let _ = execute!(
                terminal.backend_mut(),
                LeaveAlternateScreen,
                DisableMouseCapture,
                DisableBracketedPaste,
                crossterm::cursor::Show
            );
            return Err(io::Error::new(io::ErrorKind::Other, e.to_string()));
        }
    };
    // If the on-disk settings failed to parse, we run on in-memory defaults — but must NOT
    // save on exit, or we would overwrite the user's intact-but-unparseable settings file
    // (remote profiles, keybindings, theme, ...) with those defaults.
    let settings_load_failed = settings_error.is_some();
    // The exit-save skip alone is not enough: changing any setting during the session
    // (bookmarks, remote profiles, ...) also calls settings.save() and would clobber the
    // original. Preserve the unparseable file with a one-time backup copy.
    let settings_backup_path = if settings_load_failed {
        match config::Settings::config_path() {
            Some(path) => match backup_unparseable_settings(&path) {
                Ok(backup) => Some(backup),
                Err(e) => {
                    // Do not enter an interactive session backed by defaults
                    // when we could not preserve the malformed original:
                    // settings dialogs save eagerly and could otherwise
                    // destroy the only recoverable copy.
                    let _ = disable_raw_mode();
                    let _ = execute!(
                        terminal.backend_mut(),
                        LeaveAlternateScreen,
                        DisableMouseCapture,
                        DisableBracketedPaste,
                        crossterm::cursor::Show
                    );
                    return Err(io::Error::new(
                        e.kind(),
                        format!("settings could not be parsed and the recovery backup failed: {e}"),
                    ));
                }
            },
            None => None,
        }
    } else {
        None
    };
    let mut app = App::with_settings(settings);
    app.image_picker = Some(picker);
    app.design_mode = design_mode;

    // Override panels with command-line paths if provided
    if !start_paths.is_empty() {
        app.set_panels_from_paths(start_paths);
    }

    // Show settings load error if any
    if let Some(err) = settings_error {
        if let Some(backup) = settings_backup_path {
            app.show_message(&format!(
                "Settings error: {} (using defaults; original backed up to {})",
                err,
                backup.display()
            ));
        } else {
            app.show_message(&format!("Settings error: {} (using defaults)", err));
        }
    }

    // Show design mode message if active
    if design_mode {
        app.show_message("Design mode: theme hot-reload enabled");
    }

    // Run app
    let result = run_app(&mut terminal, &mut app);

    // Save settings before exit — but only if the original file parsed. Otherwise we would
    // clobber the user's existing (unparseable) settings with defaults.
    let settings_save_result = if !settings_load_failed {
        app.save_settings()
    } else {
        Ok(())
    };

    // Save last directory for shell cd (skip remote paths). When launched via
    // the shell wrapper, write to a per-run file so non-TUI commands cannot
    // accidentally reuse a stale ~/.cokacdir/lastdir value.
    if !app.active_panel().is_remote() {
        let last_dir = app.active_panel().path.display().to_string();
        if let Some(path) = shell_lastdir_output_path() {
            let _ = write_shell_lastdir_output(&path, &last_dir);
        } else if let Some(config_dir) = config::Settings::config_dir() {
            let lastdir_path = config_dir.join("lastdir");
            let _ = services::telegram::write_private_file_atomically(
                &lastdir_path,
                last_dir.as_bytes(),
            );
        }
    }

    // Restore the terminal even when the application loop fails, then return
    // the original failure so shells and supervisors receive a non-zero exit.
    let restore_result = (|| -> io::Result<()> {
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0),
            crossterm::cursor::Show
        )?;
        Ok(())
    })();

    match (result, restore_result) {
        (Err(app_error), Err(restore_error)) => {
            return Err(io::Error::new(
                app_error.kind(),
                format!(
                    "{} (terminal restoration also failed: {})",
                    app_error, restore_error
                ),
            ));
        }
        (Err(app_error), Ok(())) => return Err(app_error),
        (Ok(()), Err(restore_error)) => return Err(restore_error),
        (Ok(()), Ok(())) => {}
    }

    // Report persistence failure only after leaving raw/alternate-screen
    // mode so the diagnostic is visible and the process returns non-zero.
    settings_save_result?;

    // Print goodbye message
    print_goodbye_message();

    Ok(())
}

fn print_goodbye_message() {
    // Check for updates
    check_for_updates();

    println!("Thank you for using COKACDIR! 🙏");
    println!();
    println!("If you found this useful, consider checking out my other content:");
    println!("  📺 YouTube: https://www.youtube.com/@코드깎는노인");
    println!("  📚 Classes: https://cokac.com/");
    println!();
    println!("Happy coding!");
}

fn check_for_updates() {
    let current_version = env!("CARGO_PKG_VERSION");

    // Fetch latest version from GitHub (with timeout)
    let output = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "3",
            "https://raw.githubusercontent.com/kstost/cokacdir/refs/heads/main/Cargo.toml",
        ])
        .output();

    let latest_version = match output {
        Ok(output) if output.status.success() => {
            let content = String::from_utf8_lossy(&output.stdout);
            parse_version_from_cargo_toml(&content)
        }
        _ => None,
    };

    if let Some(latest) = latest_version {
        if is_newer_version(&latest, current_version) {
            println!(
                "┌──────────────────────────────────────────────────────────────────────────┐"
            );
            println!(
                "│  🚀 New version available: v{} (current: v{})                            ",
                latest, current_version
            );
            println!(
                "│                                                                          │"
            );
            println!(
                "│  Update with:                                                            │"
            );
            println!(
                "│  /bin/bash -c \"$(curl -fsSL https://cokacdir.cokac.com/install.sh)\"      │"
            );
            println!(
                "└──────────────────────────────────────────────────────────────────────────┘"
            );
            println!();
        }
    }
}

fn parse_version_from_cargo_toml(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("version") {
            // Parse: version = "x.x.x"
            if let Some(start) = line.find('"') {
                if let Some(end) = line.rfind('"') {
                    if start < end {
                        return Some(line[start + 1..end].to_string());
                    }
                }
            }
        }
    }
    None
}

fn is_newer_version(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> { v.split('.').filter_map(|s| s.parse().ok()).collect() };

    let latest_parts = parse(latest);
    let current_parts = parse(current);

    for i in 0..latest_parts.len().max(current_parts.len()) {
        let l = latest_parts.get(i).copied().unwrap_or(0);
        let c = current_parts.get(i).copied().unwrap_or(0);
        if l > c {
            return true;
        } else if l < c {
            return false;
        }
    }
    false
}

fn file_operation_completion_message(
    progress: &crate::ui::app::FileOperationProgress,
    pending_tar_archive: Option<&str>,
    pending_extract_dir: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Some(result) = progress.result.as_ref() else {
        return (None, None);
    };

    if progress.operation_type == crate::services::file_ops::FileOperationType::Tar {
        if result.failure_count == 0 {
            let message = if let Some(archive_name) = pending_tar_archive {
                format!("Created: {}", archive_name)
            } else {
                format!("Archived {} file(s)", result.success_count)
            };
            return (Some(message), None);
        }

        let error = result.last_error.as_deref().unwrap_or("Archive failed");
        let tar_error = if error == "Cancelled" {
            None
        } else {
            let archive_name = pending_tar_archive.unwrap_or("archive");
            Some(format!(
                "Failed to create archive '{}'.\n\n{}",
                archive_name, error
            ))
        };
        return (Some(format!("Error: {}", error)), tar_error);
    }

    if progress.operation_type == crate::services::file_ops::FileOperationType::Untar {
        if result.failure_count == 0 {
            let message = if let Some(extract_dir) = pending_extract_dir {
                format!("Extracted to: {}", extract_dir)
            } else {
                format!("Extracted {} file(s)", result.success_count)
            };
            return (Some(message), None);
        }

        let error = result.last_error.as_deref().unwrap_or("Extract failed");
        let tar_error = if error == "Cancelled" {
            None
        } else if let Some(extract_dir) = pending_extract_dir {
            Some(format!(
                "Failed to extract archive to '{}'.\n\n{}",
                extract_dir, error
            ))
        } else {
            Some(format!("Failed to extract archive.\n\n{}", error))
        };
        return (Some(format!("Error: {}", error)), tar_error);
    }

    let op_name = match progress.operation_type {
        crate::services::file_ops::FileOperationType::Copy => "Copied",
        crate::services::file_ops::FileOperationType::Move => "Moved",
        crate::services::file_ops::FileOperationType::Tar => "Archived",
        crate::services::file_ops::FileOperationType::Untar => "Extracted",
        crate::services::file_ops::FileOperationType::Download => "Downloaded",
        crate::services::file_ops::FileOperationType::Encrypt => "Encrypted",
        crate::services::file_ops::FileOperationType::Decrypt => "Decrypted",
    };
    let total = result.success_count + result.failure_count;
    if result.failure_count == 0 {
        let warning = if result.warnings.is_empty() {
            String::new()
        } else {
            format!(". Warning: {}", result.warnings.join("; "))
        };
        (
            Some(format!(
                "{} {} file(s){}",
                op_name, result.success_count, warning
            )),
            None,
        )
    } else {
        let warning = if result.warnings.is_empty() {
            String::new()
        } else {
            format!(". Warning: {}", result.warnings.join("; "))
        };
        (
            Some(format!(
                "{} {}/{}. Error: {}{}",
                op_name,
                result.success_count,
                total,
                result.last_error.as_deref().unwrap_or("Unknown error"),
                warning
            )),
            None,
        )
    }
}

fn completed_file_operation_succeeded(progress: &crate::ui::app::FileOperationProgress) -> bool {
    progress
        .result
        .as_ref()
        .map(|result| result.failure_count == 0 && result.success_count > 0)
        .unwrap_or(false)
}

fn shell_lastdir_output_path() -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from(std::env::var_os("COKACDIR_LASTDIR_FILE")?);
    let config_dir = config::Settings::config_dir()?;
    if is_valid_shell_lastdir_output_path(&path, &config_dir) {
        Some(path)
    } else {
        None
    }
}

fn is_valid_shell_lastdir_output_path(
    path: &std::path::Path,
    config_dir: &std::path::Path,
) -> bool {
    let expected_dir = config_dir.join("_lastdir");
    let Some(parent) = path.parent() else {
        return false;
    };
    if parent != expected_dir {
        return false;
    }

    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("cokacdir-lastdir."))
        .is_some_and(|suffix| !suffix.is_empty())
}

fn write_shell_lastdir_output(path: &std::path::Path, last_dir: &str) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let (parent_guard, stable_parent, parent_identity) = open_private_directory(parent)?;
    let before = std::fs::symlink_metadata(path)?;
    if !before.file_type().is_file() || before.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "last-directory output is not a real regular file",
        ));
    }
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "last-directory output has no name",
        )
    })?;
    let mut file = stable_parent.open_file(
        file_name,
        crate::services::file_ops::DirectoryFileOptions::new().write(true),
    )?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "last-directory output is not a real regular file",
        ));
    }
    let opened_identity = crate::services::file_ops::stable_file_identity(&file)?;
    if crate::services::file_ops::stable_path_identity(path)? != opened_identity
        || crate::services::file_ops::stable_path_identity(parent)? != parent_identity
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "last-directory output changed while being opened",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "last-directory output is a reparse point",
            ));
        }
    }
    file.set_len(0)?;
    file.write_all(last_dir.as_bytes())?;
    file.sync_all()?;
    if crate::services::file_ops::stable_file_identity(&file)? != opened_identity
        || crate::services::file_ops::stable_path_identity(path)? != opened_identity
        || crate::services::file_ops::stable_path_identity(parent)? != parent_identity
    {
        return Err(io::Error::other(
            "last-directory output changed while it was written",
        ));
    }
    #[cfg(unix)]
    parent_guard.sync_all()?;
    Ok(())
}

fn new_tar_error_dialog(message: String) -> crate::ui::app::Dialog {
    crate::ui::app::Dialog {
        dialog_type: crate::ui::app::DialogType::TarError,
        input: String::new(),
        cursor_pos: 0,
        message,
        completion: None,
        selected_button: 0,
        selection: None,
        use_md5: false,
    }
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        // Check if full redraw is needed (after terminal mode command like vim)
        if app.needs_full_redraw {
            terminal.clear()?;
            app.needs_full_redraw = false;
        }

        terminal.draw(|f| ui::draw::draw(f, app))?;

        // For AI screen, FileInfo with calculation, ImageViewer loading, diff comparing, file operation progress, or remote spinner, use fast polling
        let is_file_info_calculating = app.current_screen == Screen::FileInfo
            && app
                .file_info_state
                .as_ref()
                .map(|s| s.is_calculating)
                .unwrap_or(false);
        let is_image_loading = app.current_screen == Screen::ImageViewer
            && app
                .image_viewer_state
                .as_ref()
                .map(|s| s.is_loading)
                .unwrap_or(false);
        let is_diff_comparing = app.current_screen == Screen::DiffScreen
            && app
                .diff_state
                .as_ref()
                .map(|s| s.is_comparing)
                .unwrap_or(false);
        let is_dedup_active = app.current_screen == Screen::DedupScreen
            && app
                .dedup_screen_state
                .as_ref()
                .map(|s| !s.is_complete)
                .unwrap_or(false);
        let is_progress_active = app
            .file_operation_progress
            .as_ref()
            .map(|p| p.is_active)
            .unwrap_or(false);
        let is_remote_spinner = app.remote_spinner.is_some();

        let poll_timeout = if is_progress_active || is_dedup_active {
            Duration::from_millis(16) // ~60fps for smooth real-time updates
        } else if is_remote_spinner {
            Duration::from_millis(100) // Fast polling for spinner animation
        } else if app.current_screen == Screen::AIScreen
            || app.is_ai_mode()
            || is_file_info_calculating
            || is_image_loading
            || is_diff_comparing
        {
            Duration::from_millis(100) // Fast polling for spinner animation
        } else {
            Duration::from_millis(250)
        };

        // Poll for AI responses if on AI screen or AI mode (panel)
        if app.current_screen == Screen::AIScreen || app.is_ai_mode() {
            if let Some(ref mut state) = app.ai_state {
                // poll_response()가 true를 반환하면 새 내용이 추가된 것
                let has_new_content = state.poll_response();
                if has_new_content {
                    app.refresh_panels();
                }
            }
        }

        // Poll for file info calculation if on FileInfo screen
        if app.current_screen == Screen::FileInfo {
            if let Some(ref mut state) = app.file_info_state {
                state.poll();
            }
        }

        // Poll for image loading if on ImageViewer screen
        if app.current_screen == Screen::ImageViewer {
            if let Some(ref mut state) = app.image_viewer_state {
                let was_loading = state.is_loading;
                state.poll();
                // Create inline protocol when loading completes
                if was_loading && !state.is_loading && state.image.is_some() {
                    if let Some(ref mut picker) = app.image_picker {
                        if picker.protocol_type != ratatui_image::picker::ProtocolType::Halfblocks {
                            let img = state.image.as_ref().expect("checked above").clone();
                            state.inline_protocol = Some(picker.new_resize_protocol(img));
                            state.use_inline = true;
                        }
                    }
                }
            }
        }

        // Poll for diff comparison progress if on DiffScreen
        if app.current_screen == Screen::DiffScreen {
            if let Some(ref mut state) = app.diff_state {
                let just_completed = state.poll();
                if just_completed && !state.has_differences() {
                    app.diff_state = None;
                    app.current_screen = Screen::FilePanel;
                    app.show_message("No differences found");
                }
            }
        }

        // Poll for remote spinner completion
        app.poll_remote_spinner();

        // Check for theme file changes (hot-reload, only in design mode)
        if app.design_mode && app.theme_watch_state.check_for_changes() {
            app.reload_theme();
        }

        // Poll for file operation progress
        let mut tar_error_dialog: Option<String> = None;
        let progress_message: Option<String> =
            if let Some(ref mut progress) = app.file_operation_progress {
                let still_active = progress.poll();
                if !still_active {
                    let (msg, tar_error) = file_operation_completion_message(
                        progress,
                        app.pending_tar_archive.as_deref(),
                        app.pending_extract_dir.as_deref(),
                    );
                    tar_error_dialog = tar_error;
                    msg
                } else {
                    None
                }
            } else {
                None
            };

        // Handle progress completion (outside of borrow)
        if progress_message.is_some() {
            let operation_succeeded = app
                .file_operation_progress
                .as_ref()
                .map(completed_file_operation_succeeded)
                .unwrap_or(false);

            // 원격 다운로드 완료 → 편집기/뷰어 열기
            if let Some(pending) = app.pending_remote_open.take() {
                app.file_operation_progress = None;
                app.dialog = None;

                // Completion result decides success; tmp existence is only a final sanity check.
                let tmp_exists = match &pending {
                    crate::ui::app::PendingRemoteOpen::Editor { tmp_path, .. } => tmp_path.exists(),
                    crate::ui::app::PendingRemoteOpen::ImageViewer { tmp_path } => {
                        tmp_path.exists()
                    }
                };

                if !operation_succeeded || !tmp_exists {
                    if let Some(msg) = progress_message {
                        app.show_message(&msg);
                    } else {
                        app.show_message("Download failed");
                    }
                } else {
                    match pending {
                        crate::ui::app::PendingRemoteOpen::Editor {
                            tmp_path,
                            panel_index,
                            remote_path,
                            endpoint,
                            edit_session_id,
                            version,
                        } => {
                            let Some(version) = version.get().cloned() else {
                                app.show_message(
                                    "Cannot open remote file: downloaded version is unavailable",
                                );
                                continue;
                            };
                            let mut editor = crate::ui::file_editor::EditorState::new();
                            editor.set_syntax_colors(app.theme.syntax);
                            match editor.load_file(&tmp_path) {
                                Ok(_) => {
                                    let local_hash_matches = editor
                                        .loaded_content_sha256()
                                        .map(|hash| version.matches_content_hash(hash))
                                        .unwrap_or(false);
                                    if !local_hash_matches {
                                        app.show_message(
                                            "Cannot open remote file: local cache changed after download",
                                        );
                                        continue;
                                    }
                                    editor.remote_origin =
                                        Some(crate::ui::file_editor::RemoteEditOrigin {
                                            panel_index,
                                            remote_path,
                                            endpoint,
                                            expected_version: version,
                                            edit_session_id,
                                            committed_generation: 0,
                                        });
                                    app.editor_state = Some(editor);
                                    app.current_screen = Screen::FileEditor;
                                }
                                Err(e) => {
                                    app.show_message(&format!("Cannot open file: {}", e));
                                }
                            }
                        }
                        crate::ui::app::PendingRemoteOpen::ImageViewer { tmp_path } => {
                            if !crate::ui::image_viewer::supports_true_color() {
                                app.pending_large_image = Some(tmp_path);
                                app.dialog = Some(crate::ui::app::Dialog {
                                    dialog_type: crate::ui::app::DialogType::TrueColorWarning,
                                    input: String::new(),
                                    cursor_pos: 0,
                                    message: "Terminal doesn't support true color. Open anyway?"
                                        .to_string(),
                                    completion: None,
                                    selected_button: 1,
                                    selection: None,
                                    use_md5: false,
                                });
                            } else {
                                app.image_viewer_state =
                                    Some(crate::ui::image_viewer::ImageViewerState::new(&tmp_path));
                                app.current_screen = Screen::ImageViewer;
                            }
                        }
                    }
                }
            } else {
                app.finish_pending_cut_operation(operation_succeeded);
                let show_tar_error_dialog = tar_error_dialog.is_some();
                if let Some(msg) = progress_message {
                    if !show_tar_error_dialog {
                        app.show_message(&msg);
                    }
                }
                // Focus on created tar archive if applicable
                if let Some(archive_name) = app.pending_tar_archive.take() {
                    app.refresh_panels();
                    if let Some(idx) = app
                        .active_panel()
                        .files
                        .iter()
                        .position(|f| f.name == archive_name)
                    {
                        app.active_panel_mut().selected_index = idx;
                    }
                // Focus on extracted directory if applicable
                } else if let Some(extract_dir) = app.pending_extract_dir.take() {
                    app.refresh_panels();
                    if let Some(idx) = app
                        .active_panel()
                        .files
                        .iter()
                        .position(|f| f.name == extract_dir)
                    {
                        app.active_panel_mut().selected_index = idx;
                    }
                // Focus on first pasted file (by panel's sorted order) if applicable
                } else if let Some(paste_names) = app.pending_paste_focus.take() {
                    app.refresh_panels();
                    // Find the first file in the panel's sorted list that matches any pasted name
                    if let Some(idx) = app
                        .active_panel()
                        .files
                        .iter()
                        .position(|f| paste_names.contains(&f.name))
                    {
                        app.active_panel_mut().selected_index = idx;
                    }
                } else {
                    app.refresh_panels();
                }
                app.file_operation_progress = None;
                app.dialog = None;
                if let Some(message) = tar_error_dialog {
                    app.dialog = Some(new_tar_error_dialog(message));
                }
            }
        }

        // Check for key events with timeout
        if event::poll(poll_timeout)? {
            // Block all input while remote spinner is active
            if app.remote_spinner.is_some() {
                let ev = event::read()?;
                if let Event::Key(key) = ev {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if key.code == KeyCode::Esc {
                        request_remote_spinner_cancel(app);
                    }
                }
                continue;
            }
            let ev = event::read()?;

            // Windows: crossterm의 bracketed paste 미지원 워크어라운드 (crossterm#737)
            // Windows Terminal이 Ctrl+V 시 클립보드 텍스트를 개별 키 이벤트로 전송함.
            // 연속으로 즉시 도착하는 문자 키 이벤트를 paste burst로 감지하여 처리.
            #[cfg(windows)]
            {
                if let Event::Key(ref key) = ev {
                    if key.kind == KeyEventKind::Press {
                        if let KeyCode::Char(first_c) = key.code {
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            {
                                // 즉시 도착하는 후속 이벤트가 있는지 확인 (paste burst)
                                let mut paste_buf = String::new();
                                paste_buf.push(first_c);
                                while event::poll(Duration::ZERO)? {
                                    match event::read()? {
                                        Event::Key(nk) if nk.kind == KeyEventKind::Press => {
                                            match nk.code {
                                                KeyCode::Char(nc)
                                                    if !nk.modifiers.intersects(
                                                        KeyModifiers::CONTROL | KeyModifiers::ALT,
                                                    ) =>
                                                {
                                                    paste_buf.push(nc);
                                                }
                                                KeyCode::Enter => paste_buf.push('\n'),
                                                _ => break,
                                            }
                                        }
                                        _ => continue, // Release 이벤트 등 무시
                                    }
                                }
                                if paste_buf.len() > 1 {
                                    // 멀티 문자 paste burst 감지 → paste로 처리
                                    handle_windows_paste(app, &paste_buf);
                                    continue;
                                }
                                // 단일 문자 → 정상 키 처리로 fall through
                            }
                        }
                    }
                }
            }

            match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match app.current_screen {
                        Screen::FilePanel => {
                            if handle_panel_input(app, key.code, key.modifiers) {
                                return Ok(());
                            }
                        }
                        Screen::FileViewer => {
                            ui::file_viewer::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::FileEditor => {
                            ui::file_editor::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::FileInfo => {
                            ui::file_info::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::ProcessManager => {
                            ui::process_manager::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::Help => {
                            if ui::help::handle_input(app, key.code) {
                                app.current_screen = Screen::FilePanel;
                            }
                        }
                        Screen::AIScreen => {
                            if let Some(ref mut state) = app.ai_state {
                                if ui::ai_screen::handle_input(
                                    state,
                                    key.code,
                                    key.modifiers,
                                    &app.keybindings,
                                ) {
                                    // Save session to file before leaving
                                    state.save_session_to_file();
                                    app.current_screen = Screen::FilePanel;
                                    app.ai_state = None;
                                    // Refresh panels in case AI modified files
                                    app.refresh_panels();
                                }
                            }
                        }
                        Screen::SystemInfo => {
                            if ui::system_info::handle_input(
                                &mut app.system_info_state,
                                key.code,
                                key.modifiers,
                                &app.keybindings,
                            ) {
                                app.current_screen = Screen::FilePanel;
                            }
                        }
                        Screen::ImageViewer => {
                            // 다이얼로그가 열려있으면 다이얼로그 입력 처리
                            if app.dialog.is_some() {
                                ui::dialogs::handle_dialog_input(app, key.code, key.modifiers);
                            } else {
                                ui::image_viewer::handle_input(app, key.code, key.modifiers);
                            }
                        }
                        Screen::SearchResult => {
                            let result = ui::search_result::handle_input(
                                &mut app.search_result_state,
                                key.code,
                                key.modifiers,
                                &app.keybindings,
                            );
                            match result {
                                Some(crate::keybindings::SearchResultAction::Open) => {
                                    app.goto_search_result();
                                }
                                Some(crate::keybindings::SearchResultAction::Close) => {
                                    app.search_result_state.active = false;
                                    app.current_screen = Screen::FilePanel;
                                }
                                _ => {}
                            }
                        }
                        Screen::DiffScreen => {
                            ui::diff_screen::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::DiffFileView => {
                            ui::diff_file_view::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::GitScreen => {
                            ui::git_screen::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::DedupScreen => {
                            if let Some(ref mut state) = app.dedup_screen_state {
                                if ui::dedup_screen::handle_input(state, key.code, key.modifiers) {
                                    app.current_screen = Screen::FilePanel;
                                    app.dedup_screen_state = None;
                                    app.refresh_panels();
                                }
                            }
                        }
                    }
                }
                Event::Paste(text) => {
                    match app.current_screen {
                        Screen::AIScreen => {
                            if let Some(ref mut state) = app.ai_state {
                                ui::ai_screen::handle_paste(state, &text);
                            }
                        }
                        Screen::FilePanel => {
                            // AI mode with focus on AI panel
                            if app.is_ai_mode()
                                && app.ai_panel_index == Some(app.active_panel_index)
                            {
                                if let Some(ref mut state) = app.ai_state {
                                    ui::ai_screen::handle_paste(state, &text);
                                }
                            } else if app.dialog.is_some() {
                                ui::dialogs::handle_paste(app, &text);
                            } else if app.advanced_search_state.active {
                                ui::advanced_search::handle_paste(
                                    &mut app.advanced_search_state,
                                    &text,
                                );
                            }
                        }
                        Screen::FileEditor => {
                            ui::file_editor::handle_paste(app, &text);
                        }
                        Screen::ImageViewer => {
                            if app.dialog.is_some() {
                                ui::dialogs::handle_paste(app, &text);
                            }
                        }
                        Screen::GitScreen => {
                            if let Some(ref mut state) = app.git_screen_state {
                                ui::git_screen::handle_paste(state, &text);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

fn request_remote_spinner_cancel(app: &mut App) {
    if let Some(spinner) = app.remote_spinner.as_mut() {
        // The worker owns the RemoteContext until it sends its result. Dropping
        // the receiver here permanently disconnects the panel. Keep polling so
        // the context is always returned when the in-flight operation finishes.
        spinner.message = "Waiting for remote operation to finish safely...".to_string();
    }
}

/// Windows: paste burst로 감지된 텍스트를 현재 화면 컨텍스트에 맞게 처리
#[cfg(windows)]
fn handle_windows_paste(app: &mut App, text: &str) {
    match app.current_screen {
        Screen::FilePanel => {
            if app.is_ai_mode() && app.ai_panel_index == Some(app.active_panel_index) {
                if let Some(ref mut state) = app.ai_state {
                    ui::ai_screen::handle_paste(state, text);
                }
            } else if app.dialog.is_some() {
                ui::dialogs::handle_paste(app, text);
            } else if app.advanced_search_state.active {
                ui::advanced_search::handle_paste(&mut app.advanced_search_state, text);
            }
        }
        Screen::FileEditor => {
            ui::file_editor::handle_paste(app, text);
        }
        Screen::AIScreen => {
            if let Some(ref mut state) = app.ai_state {
                ui::ai_screen::handle_paste(state, text);
            }
        }
        Screen::GitScreen => {
            if let Some(ref mut state) = app.git_screen_state {
                ui::git_screen::handle_paste(state, text);
            }
        }
        Screen::ImageViewer => {
            if app.dialog.is_some() {
                ui::dialogs::handle_paste(app, text);
            }
        }
        _ => {}
    }
}

fn handle_panel_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    // AI 모드일 때: active_panel이 AI 패널 쪽이면 AI로 입력 전달, 아니면 파일 패널 조작
    if app.is_ai_mode() {
        let ai_has_focus = app.ai_panel_index == Some(app.active_panel_index);
        if app.keybindings.panel_action(code, modifiers) == Some(PanelAction::SwitchPanel) {
            // AI fullscreen 모드에서는 패널 전환 차단
            let ai_fullscreen = app.ai_state.as_ref().map_or(false, |s| s.ai_fullscreen);
            if !ai_fullscreen {
                app.switch_panel();
            }
            return false;
        }
        if ai_has_focus {
            if let Some(ref mut state) = app.ai_state {
                if ui::ai_screen::handle_input(state, code, modifiers, &app.keybindings) {
                    // AI 화면 종료 요청
                    app.close_ai_screen();
                }
            }
            return false;
        }
        // ai_has_focus가 false면 아래 파일 패널 로직으로 진행
    }

    // Handle advanced search dialog first
    if app.advanced_search_state.active {
        if let Some(criteria) = ui::advanced_search::handle_input(
            &mut app.advanced_search_state,
            code,
            modifiers,
            &app.keybindings,
        ) {
            app.execute_advanced_search(&criteria);
        }
        return false;
    }

    // Handle dialog input first
    if app.dialog.is_some() {
        return ui::dialogs::handle_dialog_input(app, code, modifiers);
    }

    // Look up action from keybindings
    if let Some(action) = app.keybindings.panel_action(code, modifiers) {
        match action {
            PanelAction::Quit => return true,
            PanelAction::MoveUp => app.move_cursor(-1),
            PanelAction::MoveDown => app.move_cursor(1),
            PanelAction::PageUp => app.move_cursor(-10),
            PanelAction::PageDown => app.move_cursor(10),
            PanelAction::GoHome => app.cursor_to_start(),
            PanelAction::GoEnd => app.cursor_to_end(),
            PanelAction::Open => app.enter_selected(),
            PanelAction::ParentDir => {
                if app.diff_first_panel.is_some() {
                    app.diff_first_panel = None;
                    app.show_message("Diff cancelled");
                } else {
                    app.go_to_parent();
                }
            }
            PanelAction::SwitchPanel => app.switch_panel(),
            PanelAction::SwitchPanelLeft => app.switch_panel_left(),
            PanelAction::SwitchPanelRight => app.switch_panel_right(),
            PanelAction::ToggleSelect => app.toggle_selection(),
            PanelAction::SelectAll => app.toggle_all_selection(),
            PanelAction::SelectByExtension => app.select_by_extension(),
            PanelAction::SelectUp => app.move_cursor_with_selection(-1),
            PanelAction::SelectDown => app.move_cursor_with_selection(1),
            PanelAction::Copy => app.clipboard_copy(),
            PanelAction::Cut => app.clipboard_cut(),
            PanelAction::Paste => app.clipboard_paste(),
            PanelAction::SortByName => app.toggle_sort_by_name(),
            PanelAction::SortByType => app.toggle_sort_by_type(),
            PanelAction::SortBySize => app.toggle_sort_by_size(),
            PanelAction::SortByDate => app.toggle_sort_by_date(),
            PanelAction::Help => app.show_help(),
            PanelAction::FileInfo => app.show_file_info(),
            PanelAction::Edit => app.edit_file(),
            PanelAction::Mkdir => app.show_mkdir_dialog(),
            PanelAction::Mkfile => app.show_mkfile_dialog(),
            PanelAction::Delete => app.show_delete_dialog(),
            PanelAction::ProcessManager => app.show_process_manager(),
            PanelAction::Rename => app.show_rename_dialog(),
            PanelAction::Tar => app.show_tar_dialog(),
            PanelAction::Search => app.show_search_dialog(),
            PanelAction::GoToPath => app.show_goto_dialog(),
            PanelAction::AddPanel => app.add_panel(),
            PanelAction::GoHomeDir => app.goto_home(),
            PanelAction::Refresh => app.refresh_panels(),
            PanelAction::GitLogDiff => app.show_git_log_diff_dialog(),
            PanelAction::StartDiff => app.start_diff(),
            PanelAction::ClosePanel => app.close_panel(),
            PanelAction::AIScreen => app.show_ai_screen(),
            PanelAction::Settings => app.show_settings_dialog(),
            PanelAction::GitScreen => app.show_git_screen(),
            PanelAction::ToggleBookmark => app.toggle_bookmark(),
            PanelAction::SetHandler => app.show_handler_dialog(),
            PanelAction::EncryptAll => app.show_encrypt_dialog(),
            PanelAction::DecryptAll => app.show_decrypt_dialog(),
            PanelAction::RemoveDuplicates => app.show_dedup_screen(),
            #[cfg(target_os = "macos")]
            PanelAction::OpenInFinder => app.open_in_finder(),
            #[cfg(target_os = "macos")]
            PanelAction::OpenInVSCode => app.open_in_vscode(),
            #[cfg(target_os = "windows")]
            PanelAction::OpenInExplorer => app.open_in_explorer(),
            #[cfg(target_os = "windows")]
            PanelAction::OpenInVSCode => app.open_in_vscode_win(),
        }
    }
    false
}

#[cfg(test)]
mod cli_token_tests {
    use super::{
        is_slack_token, parse_ccserver_token_input, parse_slack_pair, read_ccserver_token_file,
        read_ccserver_tokens, read_cli_key, read_cli_key_file, redact_cli_args_for_log,
        MAX_CCSERVER_TOKEN_INPUT_BYTES, MAX_CLI_KEY_BYTES,
    };
    use std::io::Cursor;

    #[test]
    fn parses_explicit_slack_token_pair() {
        let pair = parse_slack_pair("slack:xoxb-bot-token,xapp-app-token");
        assert_eq!(
            pair,
            Some(("xoxb-bot-token".to_string(), "xapp-app-token".to_string()))
        );
    }

    #[test]
    fn parses_slack_token_pair_in_either_order() {
        let pair = parse_slack_pair("xapp-app-token, xoxb-bot-token");
        assert_eq!(
            pair,
            Some(("xoxb-bot-token".to_string(), "xapp-app-token".to_string()))
        );
    }

    #[test]
    fn rejects_malformed_explicit_slack_token_pair() {
        assert!(parse_slack_pair("slack:not-a-pair").is_none());
        assert!(parse_slack_pair("slack:xoxb-only,missing-app").is_none());
    }

    #[test]
    fn detects_auto_slack_token_pair() {
        assert!(is_slack_token("xoxb-bot-token,xapp-app-token"));
        assert!(is_slack_token("slack:xoxb-bot-token,xapp-app-token"));
        assert!(!is_slack_token("123456789:telegram-token"));
    }

    #[test]
    fn parses_one_ccserver_token_per_nonempty_line() {
        let tokens = parse_ccserver_token_input(
            "# one token per line\nfirst-placeholder\n\n second-placeholder \r\n",
        )
        .unwrap();
        assert_eq!(tokens, ["first-placeholder", "second-placeholder"]);
    }

    #[test]
    fn rejects_empty_ccserver_token_input() {
        assert!(parse_ccserver_token_input(" \n\r\n").is_err());
    }

    #[test]
    fn bounds_ccserver_token_input_size() {
        let oversized = vec![b'x'; MAX_CCSERVER_TOKEN_INPUT_BYTES as usize + 1];
        let error = read_ccserver_tokens(Cursor::new(oversized)).unwrap_err();
        assert!(error.contains("exceeds"));
    }

    #[test]
    fn redacts_legacy_authorization_key_arguments() {
        let args = vec![
            "--cron".to_string(),
            "prompt".to_string(),
            "--key".to_string(),
            "super-secret".to_string(),
            "--key=second-secret".to_string(),
        ];
        let rendered = format!("{:?}", redact_cli_args_for_log(&args));
        assert!(!rendered.contains("super-secret"));
        assert!(!rendered.contains("second-secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn bounds_and_trims_cli_authorization_keys() {
        assert_eq!(
            read_cli_key(Cursor::new(b"  placeholder-key\n")).unwrap(),
            "placeholder-key"
        );
        let oversized = vec![b'x'; MAX_CLI_KEY_BYTES as usize + 1];
        assert!(read_cli_key(Cursor::new(oversized)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cli_key_file_must_be_private_and_not_a_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let key_path = temp.path().join("key");
        let link_path = temp.path().join("key-link");
        std::fs::write(&key_path, b"placeholder-key\n").unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_cli_key_file(&key_path).is_err());
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_cli_key_file(&key_path).unwrap(), "placeholder-key");
        symlink(&key_path, &link_path).unwrap();
        assert!(read_cli_key_file(&link_path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn ccserver_token_file_must_be_private_and_not_a_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let token_path = temp.path().join("tokens");
        let link_path = temp.path().join("tokens-link");
        std::fs::write(&token_path, b"placeholder-token\n").unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_ccserver_token_file(&token_path).is_err());
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_ccserver_token_file(&token_path).unwrap(),
            ["placeholder-token"]
        );
        symlink(&token_path, &link_path).unwrap();
        assert!(read_ccserver_token_file(&link_path).is_err());
    }
}

#[cfg(test)]
mod settings_recovery_tests {
    use super::backup_unparseable_settings;

    #[test]
    fn recovery_backup_never_truncates_an_older_backup() {
        let temp = tempfile::tempdir().unwrap();
        let settings = temp.path().join("settings.json");
        let old_backup = temp.path().join("settings.json.bak");
        std::fs::write(&settings, b"current malformed settings").unwrap();
        std::fs::write(&old_backup, b"older recovery copy").unwrap();

        let new_backup = backup_unparseable_settings(&settings).unwrap();

        assert_ne!(new_backup, old_backup);
        assert_eq!(std::fs::read(&old_backup).unwrap(), b"older recovery copy");
        assert_eq!(
            std::fs::read(&new_backup).unwrap(),
            b"current malformed settings"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(new_backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[cfg(test)]
mod remote_spinner_cancel_tests {
    use super::request_remote_spinner_cancel;
    use crate::ui::app::{App, RemoteSpinner, RemoteSpinnerResult};
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn escape_keeps_receiver_until_worker_result_is_processed() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(temp.path().to_path_buf(), temp.path().to_path_buf());
        let (tx, rx) = mpsc::channel();
        app.remote_spinner = Some(RemoteSpinner {
            message: "Working...".to_string(),
            started_at: Instant::now(),
            receiver: rx,
        });

        request_remote_spinner_cancel(&mut app);
        assert!(app.remote_spinner.is_some());
        assert_eq!(
            app.remote_spinner.as_ref().unwrap().message,
            "Waiting for remote operation to finish safely..."
        );

        tx.send(RemoteSpinnerResult::LocalOp {
            message: Ok("finished".to_string()),
            reload: false,
        })
        .unwrap();
        app.poll_remote_spinner();
        assert!(app.remote_spinner.is_none());
    }
}

#[cfg(test)]
mod sendfile_queue_tests {
    use super::{canonical_sendfile_path, enqueue_upload_request, ensure_private_directory};
    use std::fs;

    #[test]
    fn sendfile_rejects_directories() {
        let temp = tempfile::tempdir().unwrap();
        assert!(canonical_sendfile_path(temp.path()).is_err());
    }

    #[test]
    fn same_file_requests_get_distinct_queue_entries() {
        let temp = tempfile::tempdir().unwrap();
        let queue_dir = temp.path().join("queue");
        let payload = temp.path().join("payload.txt");
        fs::write(&payload, b"payload").unwrap();

        let first = enqueue_upload_request(&queue_dir, &payload, 1, "placeholder-hash").unwrap();
        let second = enqueue_upload_request(&queue_dir, &payload, 1, "placeholder-hash").unwrap();

        assert_ne!(first, second);
        assert_eq!(fs::read_dir(&queue_dir).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn queue_directory_and_entries_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let queue_dir = temp.path().join("queue");
        let payload = temp.path().join("payload.txt");
        fs::write(&payload, b"payload").unwrap();
        ensure_private_directory(&queue_dir).unwrap();
        fs::set_permissions(&queue_dir, fs::Permissions::from_mode(0o777)).unwrap();

        let entry = enqueue_upload_request(&queue_dir, &payload, 1, "placeholder-hash").unwrap();
        assert_eq!(
            fs::metadata(&queue_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(entry).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_rejects_symlink_without_chmodding_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared");
        let link = temp.path().join("messages");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&shared, &link).unwrap();

        let error = ensure_private_directory(&link).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
        assert_eq!(
            fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

#[cfg(test)]
mod environment_entry_tests {
    use super::is_valid_environment_entry;

    #[test]
    fn rejects_entries_that_would_make_set_var_panic() {
        assert!(!is_valid_environment_entry("", "value"));
        assert!(!is_valid_environment_entry("BAD=NAME", "value"));
        assert!(!is_valid_environment_entry("BAD\0NAME", "value"));
        assert!(!is_valid_environment_entry("GOOD_NAME", "bad\0value"));
        assert!(is_valid_environment_entry("GOOD_NAME", "value"));
    }
}

#[cfg(test)]
mod license_material_tests {
    use super::{OPENSSL_LICENSE, THIRD_PARTY_NOTICES};

    #[test]
    fn bundled_openssl_license_and_notice_are_present() {
        assert!(OPENSSL_LICENSE.contains("Apache License"));
        assert!(OPENSSL_LICENSE.contains("Version 2.0, January 2004"));
        assert!(THIRD_PARTY_NOTICES.contains("`openssl-src` 300.6.1+3.6.3"));
        assert!(THIRD_PARTY_NOTICES.contains("cokacdir --licenses"));
    }
}

#[cfg(test)]
mod shell_lastdir_tests {
    use super::{is_valid_shell_lastdir_output_path, write_shell_lastdir_output};
    use std::path::PathBuf;

    #[test]
    fn accepts_wrapper_temp_file_inside_lastdir_dir() {
        let config_dir = PathBuf::from("home").join("user").join(".cokacdir");
        let path = config_dir.join("_lastdir").join("cokacdir-lastdir.A1b2C3");

        assert!(is_valid_shell_lastdir_output_path(&path, &config_dir));
    }

    #[test]
    fn rejects_output_path_outside_lastdir_dir() {
        let config_dir = PathBuf::from("home").join("user").join(".cokacdir");
        let path = config_dir.join("settings.json");

        assert!(!is_valid_shell_lastdir_output_path(&path, &config_dir));
    }

    #[test]
    fn rejects_wrong_temp_file_prefix() {
        let config_dir = PathBuf::from("home").join("user").join(".cokacdir");
        let path = config_dir.join("_lastdir").join("other-file");

        assert!(!is_valid_shell_lastdir_output_path(&path, &config_dir));
    }

    #[test]
    fn rejects_empty_temp_file_suffix() {
        let config_dir = PathBuf::from("home").join("user").join(".cokacdir");
        let path = config_dir.join("_lastdir").join("cokacdir-lastdir.");

        assert!(!is_valid_shell_lastdir_output_path(&path, &config_dir));
    }

    #[test]
    fn rejects_parent_traversal_path() {
        let config_dir = PathBuf::from("home").join("user").join(".cokacdir");
        let path = config_dir
            .join("_lastdir")
            .join("..")
            .join("cokacdir-lastdir.A1b2C3");

        assert!(!is_valid_shell_lastdir_output_path(&path, &config_dir));
    }

    #[cfg(unix)]
    #[test]
    fn writer_rejects_symlinked_parent_without_touching_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        let parent_link = temp.path().join("_lastdir");
        std::fs::create_dir(&outside).unwrap();
        let victim = outside.join("cokacdir-lastdir.test");
        std::fs::write(&victim, b"must survive").unwrap();
        symlink(&outside, &parent_link).unwrap();

        let error =
            write_shell_lastdir_output(&parent_link.join("cokacdir-lastdir.test"), "/replacement")
                .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
        assert_eq!(std::fs::read(victim).unwrap(), b"must survive");
    }

    #[cfg(unix)]
    #[test]
    fn writer_rejects_symlinked_output_without_touching_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("_lastdir");
        std::fs::create_dir(&parent).unwrap();
        let victim = temp.path().join("victim");
        let output = parent.join("cokacdir-lastdir.test");
        std::fs::write(&victim, b"must survive").unwrap();
        symlink(&victim, &output).unwrap();

        assert!(write_shell_lastdir_output(&output, "/replacement").is_err());
        assert_eq!(std::fs::read(victim).unwrap(), b"must survive");
    }
}

#[cfg(test)]
mod file_operation_completion_tests {
    use super::{
        completed_file_operation_succeeded, file_operation_completion_message, new_tar_error_dialog,
    };
    use crate::services::file_ops::{FileOperationResult, FileOperationType};
    use crate::ui::app::{DialogType, FileOperationProgress};

    #[test]
    fn tar_failure_returns_status_and_modal_message_with_full_error() {
        let mut progress = FileOperationProgress::new(FileOperationType::Tar);
        progress.result = Some(FileOperationResult {
            success_count: 0,
            failure_count: 1,
            last_error: Some("line 1\nline 2".to_string()),
            warnings: Vec::new(),
        });

        let (status, modal) = file_operation_completion_message(&progress, Some("bad.tar"), None);

        assert_eq!(status, Some("Error: line 1\nline 2".to_string()));
        assert_eq!(
            modal,
            Some("Failed to create archive 'bad.tar'.\n\nline 1\nline 2".to_string())
        );
    }

    #[test]
    fn tar_cancel_does_not_return_modal_message() {
        let mut progress = FileOperationProgress::new(FileOperationType::Tar);
        progress.result = Some(FileOperationResult {
            success_count: 0,
            failure_count: 1,
            last_error: Some("Cancelled".to_string()),
            warnings: Vec::new(),
        });

        let (status, modal) = file_operation_completion_message(&progress, Some("bad.tar"), None);

        assert_eq!(status, Some("Error: Cancelled".to_string()));
        assert_eq!(modal, None);
    }

    #[test]
    fn tar_error_dialog_starts_at_top() {
        let dialog = new_tar_error_dialog("full error".to_string());

        assert_eq!(dialog.dialog_type, DialogType::TarError);
        assert_eq!(dialog.cursor_pos, 0);
        assert_eq!(dialog.message, "full error");
    }

    #[test]
    fn completed_operation_success_requires_success_without_failures() {
        let mut progress = FileOperationProgress::new(FileOperationType::Download);
        assert!(!completed_file_operation_succeeded(&progress));

        progress.result = Some(FileOperationResult {
            success_count: 0,
            failure_count: 1,
            last_error: Some("failed".to_string()),
            warnings: Vec::new(),
        });
        assert!(!completed_file_operation_succeeded(&progress));

        progress.result = Some(FileOperationResult {
            success_count: 1,
            failure_count: 0,
            last_error: None,
            warnings: Vec::new(),
        });
        assert!(completed_file_operation_succeeded(&progress));
    }
}
