//! Antigravity CLI (`agy`) provider.
//!
//! Agy's non-TTY stdin interface emits plain stdout rather than a
//! Claude/Gemini-compatible JSON event stream. This adapter synthesizes
//! cokacdir's shared `StreamMessage` contract from that stdout.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use rusqlite::{backup::Backup, Connection, OpenFlags};
use serde_json::Value;

use crate::services::claude::{
    create_private_temp_file, debug_log_to, enhanced_path_for_bin, kill_child_tree, CancelToken,
    ClaudeResponse, PrivateTempFile, StreamMessage,
};
use crate::services::file_ops::{
    open_directory_for_read, stable_file_identity, stable_path_identity, StablePathIdentity,
};

#[path = "agy_reserved_vfs.rs"]
mod reserved_sqlite_vfs;

static AGY_PATH: OnceLock<Option<String>> = OnceLock::new();
static AGY_VERSION: OnceLock<Option<String>> = OnceLock::new();
static AGY_MODELS: OnceLock<Vec<String>> = OnceLock::new();

const AGY_SYSTEM_PROMPT_MAX_BYTES: usize = 16 * 1024 * 1024;
const AGY_SYSTEM_PROMPT_PREFIX: &str = "agy_system_prompt";
const AGY_HOOK_STATE_PREFIX: &str = "agy_hook_state";
const AGY_HOOK_LEASE_PREFIX: &str = "agy_hook_lease";
const AGY_HOOK_ENV_PROMPT_FILE: &str = "COKACDIR_AGY_SYSTEM_PROMPT_FILE";
const AGY_HOOK_ENV_TOKEN: &str = "COKACDIR_AGY_SYSTEM_PROMPT_TOKEN";
const AGY_HOOK_ENV_EXECUTABLE: &str = "COKACDIR_AGY_HOOK_EXECUTABLE";
const AGY_HOOK_ENV_STATE_FILE: &str = "COKACDIR_AGY_HOOK_STATE_FILE";
const AGY_HOOK_EXECUTABLE_OVERRIDE: &str = "COKACDIR_AGY_HOOK_EXECUTABLE_OVERRIDE";
const AGY_HOOK_INTERNAL_ARG: &str = "--internal-agy-pre-invocation-hook";
const AGY_HOOK_PLUGIN_DIR: &str = "cokacdir-runtime-system-prompt";
const AGY_HOOK_PLUGIN_MARKER: &[u8] = b"cokacdir agy runtime hook v1\n";
const AGY_HOOK_NAME: &str = "cokacdir-runtime-system-prompt-v1";

fn agy_debug(msg: &str) {
    debug_log_to("agy.log", msg);
}

fn log_preview(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn agy_path_is_runnable(path: &str) -> bool {
    let p = Path::new(path);
    if !p.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        matches!(ext.as_str(), "cmd" | "exe" | "bat" | "com")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
            .unwrap_or(false)
    }
    #[cfg(not(any(windows, unix)))]
    {
        p.is_file()
    }
}

#[cfg(unix)]
fn resolve_agy_path() -> Option<String> {
    if let Ok(val) = std::env::var("COKAC_AGY_PATH") {
        if !val.is_empty() && agy_path_is_runnable(&val) {
            agy_debug(&format!("[resolve_agy_path] COKAC_AGY_PATH={}", val));
            return Some(val);
        }
    }

    if let Ok(output) = Command::new("which").arg("agy").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && agy_path_is_runnable(&path) {
                agy_debug(&format!("[resolve_agy_path] which agy -> {}", path));
                return Some(path);
            }
        }
    }

    if let Ok(output) = Command::new("bash").args(["-lc", "which agy"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && agy_path_is_runnable(&path) {
                agy_debug(&format!("[resolve_agy_path] bash which agy -> {}", path));
                return Some(path);
            }
        }
    }

    agy_debug("[resolve_agy_path] not found");
    None
}

#[cfg(windows)]
fn resolve_agy_path() -> Option<String> {
    if let Ok(val) = std::env::var("COKAC_AGY_PATH") {
        if !val.is_empty() && agy_path_is_runnable(&val) {
            agy_debug(&format!("[resolve_agy_path] COKAC_AGY_PATH={}", val));
            return Some(val);
        } else if !val.is_empty() {
            agy_debug(&format!(
                "[resolve_agy_path] ignoring non-runnable COKAC_AGY_PATH={}",
                val
            ));
        }
    }

    // Prefer Antigravity's native Windows binary over a .cmd wrapper. Batch
    // wrappers add another argument/stdio layer, which is avoidable for agy.
    if let Some(path) = crate::services::claude::search_path_wide("agy", Some(".exe")) {
        agy_debug(&format!("[resolve_agy_path] SearchPathW .exe -> {}", path));
        return Some(path);
    }
    if let Some(path) = crate::services::claude::search_path_wide("agy", Some(".cmd")) {
        if let Some(native) = agy_native_exe_for_wrapper(&path) {
            agy_debug(&format!(
                "[resolve_agy_path] SearchPathW .cmd -> native exe {}",
                native
            ));
            return Some(native);
        }
        agy_debug(&format!("[resolve_agy_path] SearchPathW .cmd -> {}", path));
        return Some(path);
    }
    if let Ok(output) = Command::new("where.exe").arg("agy").output() {
        if output.status.success() {
            let decoded = crate::services::claude::decode_windows_output(&output.stdout);
            let mut fallback = None;
            for path in decoded.lines().map(str::trim).filter(|p| !p.is_empty()) {
                if !agy_path_is_runnable(path) {
                    continue;
                }
                if path.to_ascii_lowercase().ends_with(".exe") {
                    agy_debug(&format!("[resolve_agy_path] where.exe -> {}", path));
                    return Some(path.to_string());
                }
                if fallback.is_none() {
                    fallback = Some(path.to_string());
                }
            }
            if let Some(path) = fallback {
                agy_debug(&format!(
                    "[resolve_agy_path] where.exe fallback -> {}",
                    path
                ));
                return Some(path);
            }
        }
    }
    agy_debug("[resolve_agy_path] not found");
    None
}

#[cfg(windows)]
fn agy_native_exe_for_wrapper(wrapper_path: &str) -> Option<String> {
    let exe = Path::new(wrapper_path).with_extension("exe");
    let exe = exe.to_string_lossy().to_string();
    agy_path_is_runnable(&exe).then_some(exe)
}

fn get_agy_path() -> Option<&'static str> {
    AGY_PATH.get_or_init(resolve_agy_path).as_deref()
}

pub fn is_agy_available() -> bool {
    get_agy_path().is_some()
}

fn detect_agy_version() -> Option<String> {
    let bin = get_agy_path()?;
    let output = Command::new(bin).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

pub fn agy_version() -> Option<&'static String> {
    AGY_VERSION.get_or_init(detect_agy_version).as_ref()
}

fn detect_agy_models() -> Vec<String> {
    let Some(bin) = get_agy_path() else {
        return Vec::new();
    };
    let output = match Command::new(bin).arg("models").output() {
        Ok(o) => o,
        Err(e) => {
            agy_debug(&format!("[detect_agy_models] failed: {}", e));
            return Vec::new();
        }
    };
    if !output.status.success() {
        agy_debug(&format!(
            "[detect_agy_models] non-zero exit: {:?}",
            output.status.code()
        ));
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub fn list_models() -> Vec<String> {
    AGY_MODELS.get_or_init(detect_agy_models).clone()
}

pub fn is_valid_agy_model(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && list_models().iter().any(|m| m == model)
}

/// `gemini` is accepted as a compatibility alias but routed to `agy`.
pub fn is_agy_model(model: Option<&str>) -> bool {
    model
        .map(|m| m == "agy" || m.starts_with("agy:") || m == "gemini" || m.starts_with("gemini:"))
        .unwrap_or(false)
}

pub fn strip_agy_prefix(model: &str) -> Option<&str> {
    model
        .strip_prefix("agy:")
        .or_else(|| model.strip_prefix("gemini:"))
        .filter(|s| !s.is_empty())
        .map(|s| s.split(" \u{2014} ").next().unwrap_or(s).trim())
}

fn default_print_timeout() -> String {
    std::env::var("COKAC_AGY_PRINT_TIMEOUT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "1h".to_string())
}

fn conversation_dir() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".gemini")
            .join("antigravity-cli")
            .join("conversations"),
    )
}

pub fn conversation_path(session_id: &str) -> Option<PathBuf> {
    let dir = conversation_dir()?;
    let db = dir.join(format!("{}.db", session_id));
    if db.is_file() {
        return Some(db);
    }
    let pb = dir.join(format!("{}.pb", session_id));
    if pb.is_file() {
        return Some(pb);
    }
    None
}

pub fn conversation_exists(session_id: &str) -> bool {
    conversation_path(session_id).is_some()
}

pub fn read_last_conversation_id(working_dir: &str) -> Option<String> {
    let cache = dirs::home_dir()?
        .join(".gemini")
        .join("antigravity-cli")
        .join("cache")
        .join("last_conversations.json");
    let content = std::fs::read_to_string(cache).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    let obj = val.as_object()?;
    for key in working_dir_cache_keys(working_dir) {
        if let Some(sid) = obj.get(&key).and_then(|v| v.as_str()) {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
    }
    None
}

fn push_unique(vec: &mut Vec<String>, value: String) {
    if !value.is_empty() && !vec.iter().any(|v| v == &value) {
        vec.push(value);
    }
}

fn add_path_key_variants(vec: &mut Vec<String>, value: &str) {
    push_unique(vec, value.to_string());
    let looks_windows = value.contains('\\') || value.as_bytes().get(1) == Some(&b':');
    if value.contains('\\') {
        push_unique(vec, value.replace('\\', "/"));
    }
    if looks_windows && value.contains('/') {
        push_unique(vec, value.replace('/', "\\"));
    }
}

fn working_dir_cache_keys(working_dir: &str) -> Vec<String> {
    let mut keys = Vec::new();
    add_path_key_variants(&mut keys, working_dir);

    if let Ok(canonical) = Path::new(working_dir).canonicalize() {
        let canonical = crate::utils::format::strip_unc_prefix(canonical);
        add_path_key_variants(&mut keys, &canonical.display().to_string());
    }

    keys
}

fn stdout_absence_error_message(raw_stdout: &str) -> Option<String> {
    if !raw_stdout.trim().is_empty() {
        return None;
    }
    Some("Agy exited successfully but produced no stdout response.".to_string())
}

#[cfg(not(any(unix, windows)))]
fn build_legacy_agy_stdin_prompt(prompt: &str, system_prompt: Option<&str>) -> String {
    match system_prompt.filter(|value| !value.trim().is_empty()) {
        Some(system) => format!(
            "SYSTEM INSTRUCTIONS:\n{}\n\nUSER REQUEST:\n{}",
            system, prompt
        ),
        None => prompt.to_string(),
    }
}

fn build_agy_command_args(
    session_id: Option<&str>,
    print_timeout: &str,
    log_path: &Path,
    model: Option<&str>,
) -> Vec<OsString> {
    let mut args = Vec::<OsString>::new();
    if let Some(sid) = session_id {
        args.push("--conversation".into());
        args.push(sid.into());
    }
    args.push("--print-timeout".into());
    args.push(print_timeout.into());
    args.push("--log-file".into());
    args.push(log_path.as_os_str().to_owned());
    args.push("--dangerously-skip-permissions".into());
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args
}

fn verify_real_directory(path: &Path) -> io::Result<()> {
    let (directory, _, metadata) = open_directory_for_read(path)?;
    if !metadata.file_type().is_dir()
        || stable_file_identity(&directory)? != stable_path_identity(path)?
    {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a stable real directory", path.display()),
        ));
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    let (directory, _, metadata) = open_directory_for_read(path)?;
    let identity = stable_file_identity(&directory)?;
    if !metadata.file_type().is_dir() || stable_path_identity(path)? != identity {
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
    if stable_path_identity(path)? != identity {
        return Err(io::Error::other(format!(
            "{} changed while it was secured",
            path.display()
        )));
    }
    Ok(())
}

fn derived_hook_ack_path(prompt_path: &Path) -> io::Result<PathBuf> {
    let parent = prompt_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Agy system-prompt file has no parent directory",
        )
    })?;
    let mut name = prompt_path
        .file_name()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Agy system-prompt file has no filename",
            )
        })?
        .to_os_string();
    name.push(".ack");
    Ok(parent.join(name))
}

fn valid_hook_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn read_small_regular_file(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    let path_identity = stable_path_identity(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a real regular file", path.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a reparse point", path.display()),
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
    let mut file = options.open(path)?;
    if stable_file_identity(&file)? != path_identity {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("{} changed while opening", path.display()),
        ));
    }
    if file.metadata()?.len() > max_bytes as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds the allowed size", path.display()),
        ));
    }
    let mut contents = Vec::new();
    Read::by_ref(&mut file)
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut contents)?;
    if contents.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds the allowed size", path.display()),
        ));
    }
    if stable_path_identity(path)? != path_identity {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("{} changed while reading", path.display()),
        ));
    }
    Ok(contents)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgyHookState {
    Pending,
    Complete,
    Failed,
}

fn open_and_lock_owned_temp_file_shared(file: &PrivateTempFile) -> io::Result<std::fs::File> {
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
    let opened = options.open(file.path())?;
    if stable_file_identity(&opened)? != file.identity()
        || stable_path_identity(file.path())? != file.identity()
    {
        return Err(io::Error::other(format!(
            "{} changed while locking",
            file.path().display()
        )));
    }
    // Keep the lease readable while it is live. Windows LockFileEx exclusive
    // locks deny reads and writes through every other handle, so locking the
    // prompt or ledger themselves would prevent the hook process from using
    // them. A shared lock on a separate lease permits cleanup readers while an
    // exclusive try-lock still distinguishes a live run from crash residue.
    fs2::FileExt::lock_shared(&opened)?;
    Ok(opened)
}

fn is_random_agy_temp_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 33
            && suffix.starts_with('_')
            && suffix[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn acquire_agy_temp_cleanup_lock(base: &Path) -> io::Result<std::fs::File> {
    let path = base.join(".agy-hook-cleanup.lock");
    verify_regular_file_or_absent(&path)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let lock = options.open(&path)?;
    if !lock.metadata()?.is_file() || stable_file_identity(&lock)? != stable_path_identity(&path)? {
        return Err(io::Error::other(
            "Agy temporary cleanup lock is not a stable regular file",
        ));
    }
    fs2::FileExt::lock_exclusive(&lock)?;
    Ok(lock)
}

enum AgyTempFileLock {
    MissingOrUnsafe,
    Live(std::fs::File),
    Stale(std::fs::File, StablePathIdentity),
}

fn inspect_agy_temp_file_lock(path: &Path) -> io::Result<AgyTempFileLock> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(AgyTempFileLock::MissingOrUnsafe);
        }
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Ok(AgyTempFileLock::MissingOrUnsafe);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
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
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(AgyTempFileLock::MissingOrUnsafe);
        }
        Err(error) => return Err(error),
    };
    let identity = stable_file_identity(&file)?;
    let path_identity = match stable_path_identity(path) {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(AgyTempFileLock::MissingOrUnsafe);
        }
        Err(error) => return Err(error),
    };
    if path_identity != identity {
        return Ok(AgyTempFileLock::MissingOrUnsafe);
    }
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(AgyTempFileLock::Stale(file, identity)),
        Err(error) if agy_file_lock_is_contended(&error) => Ok(AgyTempFileLock::Live(file)),
        Err(error) => Err(error),
    }
}

fn agy_file_lock_is_contended(error: &io::Error) -> bool {
    let expected = fs2::lock_contended_error();
    match (error.raw_os_error(), expected.raw_os_error()) {
        (Some(actual), Some(expected)) => actual == expected,
        _ => error.kind() == expected.kind(),
    }
}

fn remove_regular_file_if_present(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            let identity = match stable_path_identity(path) {
                Ok(identity) => identity,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            match crate::services::file_ops::remove_file_by_identity(path, identity) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            }
        }
        Ok(_) => Ok(()),
    }
}

fn delete_locked_agy_temp_file(
    path: &Path,
    lock: std::fs::File,
    identity: StablePathIdentity,
) -> io::Result<()> {
    let deletion = match crate::services::file_ops::prepare_file_deletion(path, identity) {
        Ok(deletion) => deletion,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // Windows cannot commit the handle-bound deletion while this process still
    // owns a byte-range lock through a second handle.
    drop(lock);
    match deletion.delete() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn remove_stale_agy_temp_file(path: &Path, remove_prompt_ack: bool) -> io::Result<()> {
    let (lock, identity) = match inspect_agy_temp_file_lock(path)? {
        AgyTempFileLock::Stale(lock, identity) => (lock, identity),
        AgyTempFileLock::Live(_) | AgyTempFileLock::MissingOrUnsafe => return Ok(()),
    };
    if remove_prompt_ack {
        remove_regular_file_if_present(&derived_hook_ack_path(path)?)?;
    }
    delete_locked_agy_temp_file(path, lock, identity)
}

fn agy_hook_lease_contents(prompt_path: &Path, state_path: &Path) -> io::Result<Vec<u8>> {
    let prompt_name = prompt_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_random_agy_temp_name(name, AGY_SYSTEM_PROMPT_PREFIX))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid Agy prompt name"))?;
    let state_name = state_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_random_agy_temp_name(name, AGY_HOOK_STATE_PREFIX))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid Agy state name"))?;
    Ok(format!("{prompt_name}\n{state_name}\n").into_bytes())
}

fn read_agy_hook_lease(
    base: &Path,
    lease_file: &mut std::fs::File,
) -> io::Result<(PathBuf, PathBuf)> {
    const MAX_LEASE_BYTES: usize = 512;

    if lease_file.metadata()?.len() > MAX_LEASE_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Agy hook lease exceeds the allowed size",
        ));
    }
    lease_file.seek(std::io::SeekFrom::Start(0))?;
    let mut contents = Vec::new();
    Read::by_ref(lease_file)
        .take(MAX_LEASE_BYTES as u64 + 1)
        .read_to_end(&mut contents)?;
    if contents.len() > MAX_LEASE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Agy hook lease exceeds the allowed size",
        ));
    }
    let contents = std::str::from_utf8(&contents)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid Agy hook lease"))?;
    let mut lines = contents.lines();
    let prompt_name = lines
        .next()
        .filter(|name| is_random_agy_temp_name(name, AGY_SYSTEM_PROMPT_PREFIX))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Agy hook lease prompt",
            )
        })?;
    let state_name = lines
        .next()
        .filter(|name| is_random_agy_temp_name(name, AGY_HOOK_STATE_PREFIX))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Agy hook lease state",
            )
        })?;
    if lines.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Agy hook lease contents",
        ));
    }
    Ok((base.join(prompt_name), base.join(state_name)))
}

/// Remove hook files whose lease locks were released by a crash. The caller
/// holds the directory-wide cleanup lock, so another process cannot create an
/// unleased file in the scan/create gap. Legacy prompt/state locks are still
/// honored so an older live cokacdir process is not disrupted during upgrade.
fn cleanup_stale_agy_hook_files(base: &Path) -> io::Result<()> {
    verify_real_directory(base)?;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(base)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort_unstable();

    let mut live_files = std::collections::HashSet::<PathBuf>::new();

    // First pass: discover every live mapping without deleting anything. The
    // cleanup lock prevents a stale lease from becoming a newly live lease;
    // an owner can only release a live lease while this scan is in progress.
    for lease_path in &entries {
        let Some(name) = lease_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_random_agy_temp_name(name, AGY_HOOK_LEASE_PREFIX) {
            continue;
        }

        match inspect_agy_temp_file_lock(lease_path)? {
            AgyTempFileLock::Live(mut lease_file) => {
                let (prompt_path, state_path) = read_agy_hook_lease(base, &mut lease_file)?;
                live_files.insert(prompt_path);
                live_files.insert(state_path);
            }
            AgyTempFileLock::Stale(_, _) | AgyTempFileLock::MissingOrUnsafe => {}
        }
    }

    // With the live mapping set complete, stale leases can now be removed
    // without filename-order dependence. The per-file lock check also
    // preserves live prompt/state files created by pre-lease cokacdir builds.
    for lease_path in &entries {
        let Some(name) = lease_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_random_agy_temp_name(name, AGY_HOOK_LEASE_PREFIX) {
            continue;
        }
        let (mut lease_file, lease_identity) = match inspect_agy_temp_file_lock(lease_path)? {
            AgyTempFileLock::Stale(lease_file, identity) => (lease_file, identity),
            AgyTempFileLock::Live(_) | AgyTempFileLock::MissingOrUnsafe => continue,
        };
        let mapped_files = read_agy_hook_lease(base, &mut lease_file).ok();
        if let Some((prompt_path, state_path)) = mapped_files {
            if !live_files.contains(&prompt_path) {
                remove_stale_agy_temp_file(&prompt_path, true)?;
            }
            if !live_files.contains(&state_path) {
                remove_stale_agy_temp_file(&state_path, false)?;
            }
        }
        delete_locked_agy_temp_file(lease_path, lease_file, lease_identity)?;
    }

    for path in &entries {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_random_agy_temp_name(name, AGY_SYSTEM_PROMPT_PREFIX)
            && !is_random_agy_temp_name(name, AGY_HOOK_STATE_PREFIX)
        {
            continue;
        }
        if live_files.contains(path) {
            continue;
        }
        remove_stale_agy_temp_file(
            path,
            is_random_agy_temp_name(name, AGY_SYSTEM_PROMPT_PREFIX),
        )?;
    }

    // A crash normally leaves the prompt beside its acknowledgement. This
    // second pass also handles an acknowledgement whose prompt was removed by
    // an external process before the next startup.
    for path in entries {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(prompt_name) = name.strip_suffix(".ack") else {
            continue;
        };
        if is_random_agy_temp_name(prompt_name, AGY_SYSTEM_PROMPT_PREFIX)
            && !path.with_file_name(prompt_name).exists()
        {
            remove_regular_file_if_present(&path)?;
        }
    }
    Ok(())
}

struct AgyHookPrompt {
    // Field order is intentional: on Windows the lease lock handle must close
    // before the lease guard attempts to delete its file.
    _lease_lock: std::fs::File,
    _lease_file: PrivateTempFile,
    prompt_file: PrivateTempFile,
    state_file: PrivateTempFile,
    state_identity: StablePathIdentity,
    ack_path: PathBuf,
    token: String,
}

impl AgyHookPrompt {
    fn create_in(base: &Path, system_prompt: &str) -> io::Result<Self> {
        if system_prompt.len() > AGY_SYSTEM_PROMPT_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Agy system prompt exceeds {} bytes",
                    AGY_SYSTEM_PROMPT_MAX_BYTES
                ),
            ));
        }
        let prompt_file =
            create_private_temp_file(base, AGY_SYSTEM_PROMPT_PREFIX, system_prompt.as_bytes())?;
        // The shell wrapper records a start/ok pair for every hook invocation
        // in this pre-created ledger. An unmatched start or explicit failure
        // means a successful first invocation cannot hide a failed second one
        // after a tool call.
        let state_file = create_private_temp_file(base, AGY_HOOK_STATE_PREFIX, b"")?;
        let state_identity = stable_path_identity(state_file.path())?;
        let lease_contents = agy_hook_lease_contents(prompt_file.path(), state_file.path())?;
        let lease_file =
            create_private_temp_file(base, AGY_HOOK_LEASE_PREFIX, &lease_contents)?;
        let lease_lock = open_and_lock_owned_temp_file_shared(&lease_file)?;
        let ack_path = derived_hook_ack_path(prompt_file.path())?;
        match std::fs::symlink_metadata(&ack_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Agy hook acknowledgement path already exists",
                ))
            }
        }
        Ok(Self {
            _lease_lock: lease_lock,
            _lease_file: lease_file,
            prompt_file,
            state_file,
            state_identity,
            ack_path,
            token: format!("{:032x}", rand::random::<u128>()),
        })
    }

    fn prompt_path(&self) -> &Path {
        self.prompt_file.path()
    }

    fn token(&self) -> &str {
        &self.token
    }

    fn state_path(&self) -> &Path {
        self.state_file.path()
    }

    fn hook_state(&self) -> AgyHookState {
        if stable_path_identity(self.state_path()).ok() != Some(self.state_identity) {
            return AgyHookState::Failed;
        }
        let contents = match read_small_regular_file(self.state_path(), 1024 * 1024) {
            Ok(contents) => contents,
            Err(_) => return AgyHookState::Failed,
        };
        if contents.is_empty() || !contents.ends_with(b"\n") {
            return AgyHookState::Pending;
        }
        let contents = match std::str::from_utf8(&contents) {
            Ok(contents) => contents,
            Err(_) => return AgyHookState::Failed,
        };
        let expected_start = format!("start {}", self.token);
        let expected_ok = format!("ok {}", self.token);
        let expected_fail = format!("fail {}", self.token);
        let mut starts = 0usize;
        let mut successes = 0usize;
        for line in contents.lines() {
            if line == expected_start {
                starts += 1;
            } else if line == expected_ok {
                successes += 1;
                if successes > starts {
                    return AgyHookState::Failed;
                }
            } else if line == expected_fail {
                return AgyHookState::Failed;
            } else {
                return AgyHookState::Failed;
            }
        }
        if starts == 0 {
            AgyHookState::Failed
        } else if starts == successes {
            AgyHookState::Complete
        } else {
            AgyHookState::Pending
        }
    }

    fn acknowledgement_identity(&self) -> Option<StablePathIdentity> {
        let identity = stable_path_identity(&self.ack_path).ok()?;
        let contents = read_small_regular_file(&self.ack_path, 128).ok()?;
        (contents == self.token.as_bytes()
            && stable_path_identity(&self.ack_path).ok() == Some(identity))
        .then_some(identity)
    }

    fn acknowledged(&self) -> bool {
        self.acknowledgement_identity().is_some()
    }
}

impl Drop for AgyHookPrompt {
    fn drop(&mut self) {
        if let Some(identity) = self.acknowledgement_identity() {
            let _ = crate::services::file_ops::remove_file_by_identity(&self.ack_path, identity);
        }
    }
}

fn prepare_agy_hook_prompt(
    base: &Path,
    system_prompt: Option<&str>,
) -> io::Result<Option<AgyHookPrompt>> {
    match system_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        Some(prompt) => {
            let _cleanup_lock = acquire_agy_temp_cleanup_lock(base)?;
            cleanup_stale_agy_hook_files(base)?;
            AgyHookPrompt::create_in(base, prompt).map(Some)
        }
        None => Ok(None),
    }
}

fn hook_executable_path() -> io::Result<PathBuf> {
    let path = std::env::var_os(AGY_HOOK_EXECUTABLE_OVERRIDE)
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(std::env::current_exe)?;
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Agy hook executable does not exist: {}", path.display()),
        ));
    }
    Ok(path)
}

#[cfg(unix)]
fn agy_hook_command() -> String {
    format!(
        "if [ -z \"${{{exec}:-}}\" ]; then cat >/dev/null; printf '{{}}\\n'; else if [ -z \"${{{state}:-}}\" ] || [ -z \"${{{token}:-}}\" ]; then exit 125; fi; umask 077; printf 'start %s\\n' \"${{{token}}}\" >> \"${{{state}}}\" || exit 125; \"${{{exec}}}\" {arg}; status=$?; if [ \"$status\" -eq 0 ]; then phase=ok; else phase=fail; fi; printf '%s %s\\n' \"$phase\" \"${{{token}}}\" >> \"${{{state}}}\" || exit 125; exit \"$status\"; fi",
        exec = AGY_HOOK_ENV_EXECUTABLE,
        arg = AGY_HOOK_INTERNAL_ARG,
        state = AGY_HOOK_ENV_STATE_FILE,
        token = AGY_HOOK_ENV_TOKEN,
    )
}

#[cfg(windows)]
fn agy_hook_command() -> String {
    format!(
        "if not defined {exec} (more >nul & echo {{}}) else if not defined {state} (exit /b 125) else if not defined {token} (exit /b 125) else (>>\"%{state}%\" echo start %{token}% && \"%{exec}%\" {arg} && (>>\"%{state}%\" echo ok %{token}%) || (>>\"%{state}%\" echo fail %{token}% & exit /b 125))",
        exec = AGY_HOOK_ENV_EXECUTABLE,
        arg = AGY_HOOK_INTERNAL_ARG,
        state = AGY_HOOK_ENV_STATE_FILE,
        token = AGY_HOOK_ENV_TOKEN,
    )
}

#[cfg(not(any(unix, windows)))]
fn agy_hook_command() -> String {
    "printf '{}\\n'".to_string()
}

fn verify_regular_file_or_absent(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is a reparse point", path.display()),
                    ));
                }
            }
            Ok(())
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a real regular file", path.display()),
        )),
    }
}

fn write_private_file_if_changed(path: &Path, contents: &[u8]) -> io::Result<()> {
    verify_regular_file_or_absent(path)?;
    if std::fs::read(path)
        .map(|existing| existing == contents)
        .unwrap_or(false)
    {
        return Ok(());
    }
    crate::services::telegram::write_private_file_atomically(path, contents)
}

fn ensure_agy_hook_plugin_in(config_root: &Path) -> io::Result<PathBuf> {
    let plugins_dir = config_root.join("plugins");
    std::fs::create_dir_all(&plugins_dir)?;
    verify_real_directory(&plugins_dir)?;

    let lock_path = plugins_dir.join(".cokacdir-runtime-system-prompt.lock");
    verify_regular_file_or_absent(&lock_path)?;
    let mut lock_options = std::fs::OpenOptions::new();
    lock_options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        lock_options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let lock_file = lock_options.open(&lock_path)?;
    let lock_metadata = lock_file.metadata()?;
    if !lock_metadata.is_file()
        || stable_file_identity(&lock_file)? != stable_path_identity(&lock_path)?
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Agy hook installation lock is not a stable regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if lock_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Agy hook installation lock is a reparse point",
            ));
        }
    }
    fs2::FileExt::lock_exclusive(&lock_file)?;

    let plugin_dir = plugins_dir.join(AGY_HOOK_PLUGIN_DIR);
    match std::fs::symlink_metadata(&plugin_dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory(&plugin_dir)?;
        }
        Err(error) => return Err(error),
        Ok(_) => verify_real_directory(&plugin_dir)?,
    }

    let marker_path = plugin_dir.join(".cokacdir-owned");
    match std::fs::symlink_metadata(&marker_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let directory_was_empty = std::fs::read_dir(&plugin_dir)?.next().is_none();
            if !directory_was_empty {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "unowned Agy hook plugin directory already exists: {}",
                        plugin_dir.display()
                    ),
                ));
            }
            write_private_file_if_changed(&marker_path, AGY_HOOK_PLUGIN_MARKER)?;
        }
        Err(error) => return Err(error),
        Ok(_) => {
            if read_small_regular_file(&marker_path, 128)? != AGY_HOOK_PLUGIN_MARKER {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "Agy hook plugin directory is not owned by cokacdir: {}",
                        plugin_dir.display()
                    ),
                ));
            }
        }
    }

    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "$schema": "https://antigravity.google/schemas/v1/plugin.json",
        "name": AGY_HOOK_PLUGIN_DIR,
        "description": "Injects per-process Cokacdir system instructions through an Agy PreInvocation hook."
    }))?;
    let command = agy_hook_command();
    let hooks = serde_json::to_vec_pretty(&serde_json::json!({
        AGY_HOOK_NAME: {
            "PreInvocation": [{
                "type": "command",
                "command": command,
                "timeout": 10
            }]
        }
    }))?;
    let disabled_manifest = plugin_dir.join("plugin.json.disabled");
    match std::fs::symlink_metadata(&disabled_manifest) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
        Ok(_) => {
            verify_regular_file_or_absent(&disabled_manifest)?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "the cokacdir Agy hook plugin is disabled: {}",
                    disabled_manifest.display()
                ),
            ));
        }
    }
    write_private_file_if_changed(&plugin_dir.join("plugin.json"), &manifest)?;
    write_private_file_if_changed(&plugin_dir.join("hooks.json"), &hooks)?;
    Ok(plugin_dir)
}

fn ensure_agy_hook_plugin() -> io::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot determine home directory for Agy hook configuration",
        )
    })?;
    ensure_agy_hook_plugin_in(&home.join(".gemini").join("config"))
}

fn write_hook_ack(path: &Path, token: &str) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(create_error) if create_error.kind() == io::ErrorKind::AlreadyExists => {
            // Separate model invocations (for example nested agents) may run
            // this idempotent acknowledgement concurrently. The create-new
            // winner can still be writing when another helper observes the
            // pathname, so briefly wait for the complete token.
            for _ in 0..1_000 {
                match read_small_regular_file(path, 128) {
                    Ok(existing) if existing == token.as_bytes() => return Ok(()),
                    Ok(existing) if existing.len() < token.len() => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "Agy hook acknowledgement belongs to a different invocation",
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(error) => return Err(error),
                }
            }
            return Err(create_error);
        }
        Err(error) => return Err(error),
    };
    file.write_all(token.as_bytes())?;
    file.sync_all()
}

fn build_agy_hook_response(
    hook_input: &str,
    prompt_path: &Path,
    token: &str,
) -> io::Result<(Vec<u8>, PathBuf)> {
    if !valid_hook_token(token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Agy hook token",
        ));
    }
    let input: Value = serde_json::from_str(hook_input).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Agy hook input: {error}"),
        )
    })?;
    let _invocation_num = input
        .get("invocationNum")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Agy hook input is missing invocationNum",
            )
        })?;
    let _initial_num_steps = input
        .get("initialNumSteps")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Agy hook input is missing initialNumSteps",
            )
        })?;
    if !input
        .get("conversationId")
        .and_then(Value::as_str)
        .is_some_and(|conversation_id| !conversation_id.is_empty())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Agy hook input is missing PreInvocation fields",
        ));
    }

    let temp_dir = crate::utils::path::cokacdir_temp_dir()?;
    let owned_name = prompt_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(&format!("{}_", AGY_SYSTEM_PROMPT_PREFIX)))
        .is_some_and(|suffix| {
            suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    if prompt_path.parent() != Some(temp_dir.as_path()) || !owned_name {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Agy hook prompt path is outside cokacdir's private temporary directory",
        ));
    }
    let prompt = read_small_regular_file(prompt_path, AGY_SYSTEM_PROMPT_MAX_BYTES)?;
    let prompt = String::from_utf8(prompt).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Agy system prompt is not valid UTF-8",
        )
    })?;
    let response = serde_json::to_vec(&serde_json::json!({
        "injectSteps": [{ "ephemeralMessage": prompt }]
    }))
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((response, derived_hook_ack_path(prompt_path)?))
}

pub(crate) fn run_agy_pre_invocation_hook() -> io::Result<()> {
    let prompt_path = std::env::var_os(AGY_HOOK_ENV_PROMPT_FILE);
    let token = std::env::var(AGY_HOOK_ENV_TOKEN).ok();
    if prompt_path.is_none() && token.is_none() {
        println!("{{}}");
        return Ok(());
    }
    let prompt_path = prompt_path.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing Cokacdir Agy system-prompt file environment",
        )
    })?;
    let token = token.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing Cokacdir Agy system-prompt token environment",
        )
    })?;
    let mut input = String::new();
    std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut input)?;
    let (response, ack_path) = build_agy_hook_response(&input, Path::new(&prompt_path), &token)?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&response)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    write_hook_ack(&ack_path, &token)
}

/// Ensures that every return path, including I/O errors and unwinding, reaps
/// Agy before the per-invocation hook files are allowed to disappear.
struct ReapingAgyChild {
    child: std::process::Child,
    status: Option<std::process::ExitStatus>,
}

impl ReapingAgyChild {
    fn new(child: std::process::Child) -> Self {
        Self {
            child,
            status: None,
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.stdin.take()
    }

    fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.stderr.take()
    }

    fn kill_and_reap(&mut self) {
        if self.status.is_some() {
            return;
        }
        if matches!(self.try_wait(), Ok(Some(_))) {
            return;
        }
        kill_child_tree(&mut self.child);
        let _ = self.child.kill();
        if let Ok(status) = self.child.wait() {
            self.status = Some(status);
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let status = self.child.try_wait()?;
        if let Some(status) = status {
            self.status = Some(status);
        }
        Ok(status)
    }

    fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = self.child.wait()?;
        self.status = Some(status);
        Ok(status)
    }
}

impl Drop for ReapingAgyChild {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

fn make_conversation_clone_id(prefix: &str) -> String {
    format!("{}_{:032x}", prefix, rand::random::<u128>())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CloneFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn clone_file_identity(file: &std::fs::File) -> std::io::Result<CloneFileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "clone target is not a regular file",
        ));
    }
    Ok(CloneFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
fn clone_path_identity(path: &Path) -> std::io::Result<CloneFileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "clone target path is not a regular file",
        ));
    }
    Ok(CloneFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
type CloneFileIdentity = crate::services::file_ops::StablePathIdentity;

#[cfg(windows)]
fn clone_file_identity(file: &std::fs::File) -> std::io::Result<CloneFileIdentity> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "clone target is not a real regular file",
        ));
    }
    crate::services::file_ops::stable_file_identity(file)
}

#[cfg(windows)]
fn clone_path_identity(path: &Path) -> std::io::Result<CloneFileIdentity> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    clone_file_identity(&file)
}

fn clone_target_still_owned(
    path: &Path,
    reserved_identity: CloneFileIdentity,
) -> std::io::Result<bool> {
    clone_path_identity(path).map(|identity| identity == reserved_identity)
}

fn cleanup_owned_failed_clone(
    target: &Path,
    parent: &Path,
    reserved_identity: CloneFileIdentity,
) -> Result<bool, String> {
    if !clone_target_still_owned(target, reserved_identity).unwrap_or(false) {
        return Ok(false);
    }
    std::fs::remove_file(target).map_err(|e| {
        format!(
            "Failed to remove owned partial Agy clone {}: {}",
            target.display(),
            e
        )
    })?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| {
            format!(
                "Failed to persist cleanup of partial Agy clone {}: {}",
                target.display(),
                e
            )
        })?;
    Ok(true)
}

fn clone_failure_with_safe_cleanup(
    error: String,
    target: &Path,
    parent: &Path,
    reserved_identity: CloneFileIdentity,
) -> String {
    match cleanup_owned_failed_clone(target, parent, reserved_identity) {
        Ok(true) => error,
        Ok(false) => format!(
            "{}; clone target identity changed, so no path was deleted and the owned inode was left as recovery state",
            error
        ),
        Err(cleanup_error) => format!(
            "{}; partial clone cleanup failed and recovery state was preserved: {}",
            error, cleanup_error
        ),
    }
}

fn backup_sqlite_into_connection(
    source: &Connection,
    destination: &mut Connection,
    target: &Path,
) -> Result<(), String> {
    let backup = Backup::new(source, destination).map_err(|error| {
        format!(
            "Failed to start Agy conversation backup into {}: {}",
            target.display(),
            error
        )
    })?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

    loop {
        match backup.step(100).map_err(|error| {
            format!(
                "Failed to back up Agy conversation into {}: {}",
                target.display(),
                error
            )
        })? {
            rusqlite::backup::StepResult::Done => return Ok(()),
            rusqlite::backup::StepResult::More => {}
            rusqlite::backup::StepResult::Busy | rusqlite::backup::StepResult::Locked => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "Timed out waiting to back up Agy conversation into {}",
                        target.display()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                return Err(format!(
                    "SQLite returned an unknown backup state for {}",
                    target.display()
                ))
            }
        }
    }
}

fn backup_sqlite_to_reserved_handle(
    source: &Connection,
    reserved: &std::fs::File,
    target: &Path,
    reserved_identity: CloneFileIdentity,
) -> Result<(), String> {
    let held_identity = clone_file_identity(reserved).map_err(|error| {
        format!(
            "Failed to revalidate reserved Agy clone handle {}: {}",
            target.display(),
            error
        )
    })?;
    if held_identity != reserved_identity {
        return Err(format!(
            "Reserved Agy clone handle identity changed: {}",
            target.display()
        ));
    }

    // SQLite's backup API accepts a connection, not an existing File. This
    // one-shot VFS exposes only a duplicate of the no-clobber reservation, so
    // SQLite streams pages to that inode without ever resolving `target`.
    let registration = reserved_sqlite_vfs::Registration::register(reserved, reserved_identity)
        .map_err(|error| {
            format!(
                "Failed to prepare reserved Agy clone {}: {}",
                target.display(),
                error
            )
        })?;
    let mut destination = Connection::open_with_flags_and_vfs(
        registration.filename(),
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        registration.name(),
    )
    .map_err(|error| {
        format!(
            "Failed to open reserved Agy clone {}: {}",
            target.display(),
            error
        )
    })?;
    destination
        .execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-2048;")
        .map_err(|error| {
            format!(
                "Failed to configure reserved Agy clone {}: {}",
                target.display(),
                error
            )
        })?;
    backup_sqlite_into_connection(source, &mut destination, target)?;
    drop(destination);
    registration.unregister().map_err(|error| {
        format!(
            "Failed to release reserved Agy clone {}: {}",
            target.display(),
            error
        )
    })?;

    reserved
        .sync_all()
        .map_err(|error| format!("Failed to sync Agy clone {}: {}", target.display(), error))?;
    let mut verification = reserved.try_clone().map_err(|error| {
        format!(
            "Failed to duplicate written Agy clone handle {}: {}",
            target.display(),
            error
        )
    })?;
    verification.rewind().map_err(|error| {
        format!(
            "Failed to rewind written Agy clone {}: {}",
            target.display(),
            error
        )
    })?;
    let mut header = [0u8; 16];
    verification.read_exact(&mut header).map_err(|error| {
        format!(
            "Failed to read written Agy clone header {}: {}",
            target.display(),
            error
        )
    })?;
    if &header != b"SQLite format 3\0" {
        return Err(format!(
            "Agy backup did not produce a valid SQLite database: {}",
            target.display()
        ));
    }

    let final_identity = clone_file_identity(&verification).map_err(|error| {
        format!(
            "Failed to revalidate written Agy clone handle {}: {}",
            target.display(),
            error
        )
    })?;
    if final_identity != reserved_identity {
        return Err(format!(
            "Reserved Agy clone handle identity changed while writing: {}",
            target.display()
        ));
    }
    Ok(())
}

fn backup_sqlite_to_reserved_file(
    source: &Connection,
    reserved: &std::fs::File,
    target: &Path,
    reserved_identity: CloneFileIdentity,
) -> Result<(), String> {
    let held_identity = clone_file_identity(reserved).map_err(|error| {
        format!(
            "Failed to revalidate reserved Agy clone handle {}: {}",
            target.display(),
            error
        )
    })?;
    if held_identity != reserved_identity
        || !clone_target_still_owned(target, reserved_identity).unwrap_or(false)
    {
        return Err(format!(
            "Agy clone target changed before SQLite backup: {}",
            target.display()
        ));
    }

    backup_sqlite_to_reserved_handle(source, reserved, target, reserved_identity)?;
    if !clone_target_still_owned(target, reserved_identity).unwrap_or(false) {
        return Err(format!(
            "Agy clone target changed while writing SQLite backup: {}",
            target.display()
        ));
    }
    Ok(())
}

fn copy_conversation_to_id(source: &Path, target_id: &str) -> Result<(), String> {
    let dir = source
        .parent()
        .ok_or_else(|| format!("Invalid Agy conversation path: {}", source.display()))?;
    let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("db");
    let target = dir.join(format!("{}.{}", target_id, ext));

    if ext == "db" {
        let owned_paths = [
            target.clone(),
            dir.join(format!("{}.db-journal", target_id)),
            dir.join(format!("{}.db-wal", target_id)),
            dir.join(format!("{}.db-shm", target_id)),
        ];
        for path in &owned_paths {
            match std::fs::symlink_metadata(path) {
                Ok(_) => {
                    return Err(format!(
                        "Refusing to overwrite existing Agy clone target {}",
                        path.display()
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!(
                        "Failed to inspect Agy clone target {}: {}",
                        path.display(),
                        e
                    ))
                }
            }
        }

        // Open and validate the source before creating any clone path. SQLite's
        // NOFOLLOW flag rejects a final-component symlink on every platform
        // supported by the bundled SQLite build.
        let source_conn = Connection::open_with_flags(
            source,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|e| {
            format!(
                "Failed to open Agy conversation for backup {}: {}",
                source.display(),
                e
            )
        })?;
        source_conn
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| {
                format!(
                    "Failed to configure Agy conversation backup timeout {}: {}",
                    source.display(),
                    e
                )
            })?;

        // Reserve the randomly named destination with no-clobber semantics.
        // SQLite writes only through this held handle via a one-shot VFS, so a
        // concurrent pathname replacement cannot redirect clone contents.
        let mut reserve_options = std::fs::OpenOptions::new();
        reserve_options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            reserve_options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            reserve_options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let reserved = reserve_options.open(&target).map_err(|e| {
            format!(
                "Failed to reserve Agy clone target {}: {}",
                target.display(),
                e
            )
        })?;
        let reserved_identity = clone_file_identity(&reserved).map_err(|e| {
            format!(
                "Failed to identify reserved Agy clone target {}: {}",
                target.display(),
                e
            )
        })?;

        if let Err(backup_error) =
            backup_sqlite_to_reserved_file(&source_conn, &reserved, &target, reserved_identity)
        {
            return Err(clone_failure_with_safe_cleanup(
                backup_error,
                &target,
                dir,
                reserved_identity,
            ));
        }

        if !clone_target_still_owned(&target, reserved_identity).unwrap_or(false) {
            // Never unlink here: `target` may now name an external file. The
            // held/renamed clone inode is left as recovery state rather than
            // risking deletion of a path we did not create.
            return Err(format!(
                "Agy clone target changed during backup; clone not published: {}",
                target.display()
            ));
        }
        #[cfg(unix)]
        std::fs::File::open(dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| {
                format!(
                    "Failed to persist Agy clone directory {}: {}",
                    dir.display(),
                    e
                )
            })?;
    } else {
        let source_before = std::fs::symlink_metadata(source).map_err(|e| {
            format!(
                "Failed to inspect Agy conversation {}: {}",
                source.display(),
                e
            )
        })?;
        if !source_before.file_type().is_file() || source_before.file_type().is_symlink() {
            return Err(format!(
                "Agy conversation is not a regular file: {}",
                source.display()
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            if source_before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(format!(
                    "Agy conversation is a reparse point: {}",
                    source.display()
                ));
            }
        }
        let mut source_options = std::fs::OpenOptions::new();
        source_options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            source_options.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            source_options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let mut source_file = source_options.open(source).map_err(|e| {
            format!(
                "Failed to open Agy conversation {}: {}",
                source.display(),
                e
            )
        })?;
        let source_opened = source_file.metadata().map_err(|e| {
            format!(
                "Failed to inspect opened Agy conversation {}: {}",
                source.display(),
                e
            )
        })?;
        if !source_opened.is_file() {
            return Err(format!(
                "Agy conversation is not a regular file: {}",
                source.display()
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            if source_opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(format!(
                    "Agy conversation is a reparse point: {}",
                    source.display()
                ));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if source_before.dev() != source_opened.dev()
                || source_before.ino() != source_opened.ino()
            {
                return Err(format!(
                    "Agy conversation changed while opening: {}",
                    source.display()
                ));
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut target_file = options.open(&target).map_err(|e| {
            format!(
                "Failed to create Agy clone target {}: {}",
                target.display(),
                e
            )
        })?;
        let copy_result = (|| -> std::io::Result<()> {
            std::io::copy(&mut source_file, &mut target_file)?;
            let source_after = source_file.metadata()?;
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
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Agy conversation changed while it was being copied",
                    ));
                }
            }
            #[cfg(not(unix))]
            if source_after.len() != source_opened.len()
                || source_after.modified().ok() != source_opened.modified().ok()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Agy conversation changed while it was being copied",
                ));
            }
            target_file.sync_all()
        })();
        if let Err(e) = copy_result {
            drop(target_file);
            let _ = std::fs::remove_file(&target);
            return Err(format!(
                "Failed to copy Agy conversation {} -> {}: {}",
                source.display(),
                target.display(),
                e
            ));
        }
    }

    #[cfg(unix)]
    std::fs::File::open(dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| {
            format!(
                "Failed to sync Agy clone directory {}: {}",
                dir.display(),
                e
            )
        })?;

    Ok(())
}

/// Clone an Agy conversation for a scheduled run and leave the clone on disk.
/// Agy resumes by writing directly to the conversation file, so scheduled runs
/// must operate on a copied conversation id rather than the source id.
pub fn clone_session_for_schedule(session_id: &str, _working_dir: &str) -> Result<String, String> {
    agy_debug(&format!(
        "[session-clone] cloning Agy conversation {}",
        session_id
    ));
    if !crate::services::process::is_valid_session_id(session_id) {
        return Err(format!("Invalid session_id format: {}", session_id));
    }
    let source = conversation_path(session_id)
        .ok_or_else(|| format!("Agy conversation not found: {}", session_id))?;
    for _ in 0..32 {
        let clone_id = make_conversation_clone_id("cokacsched");
        match copy_conversation_to_id(&source, &clone_id) {
            Ok(()) => {
                agy_debug(&format!(
                    "[session-clone] cloned Agy conversation {} -> {}",
                    session_id, clone_id
                ));
                return Ok(clone_id);
            }
            Err(e) if e.contains("existing Agy clone target") => continue,
            Err(e) => return Err(e),
        }
    }
    Err("Failed to allocate a unique Agy clone id".to_string())
}

pub fn execute_command(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    _allowed_tools: Option<&[String]>,
    model: Option<&str>,
) -> ClaudeResponse {
    let (tx, rx) = std::sync::mpsc::channel();
    let result = execute_command_streaming(
        prompt,
        session_id,
        working_dir,
        tx,
        None,
        None,
        None,
        model,
        false,
    );
    if let Err(e) = result {
        return ClaudeResponse {
            success: false,
            response: None,
            session_id: None,
            error: Some(e),
        };
    }

    let mut response = String::new();
    let mut last_session_id = None;
    let mut error = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            StreamMessage::Text { content } => response.push_str(&content),
            StreamMessage::Done { result, session_id } => {
                if response.is_empty() {
                    response = result;
                }
                last_session_id = session_id;
                break;
            }
            StreamMessage::Error { message, .. } => {
                error = Some(message);
                break;
            }
            StreamMessage::Init { session_id } => last_session_id = Some(session_id),
            _ => {}
        }
    }

    ClaudeResponse {
        success: error.is_none(),
        response: if response.trim().is_empty() {
            None
        } else {
            Some(response.trim().to_string())
        },
        session_id: last_session_id,
        error,
    }
}

pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    _allowed_tools: Option<&[String]>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    model: Option<&str>,
    no_session_persistence: bool,
) -> Result<(), String> {
    agy_debug("=== agy execute_command_streaming START ===");
    agy_debug(&format!(
        "[stream] prompt_len={} session_id={:?} working_dir={} model={:?} no_session_persistence={}",
        prompt.len(),
        session_id,
        working_dir,
        model,
        no_session_persistence
    ));

    if let Some(sid) = session_id {
        if !crate::services::process::is_valid_session_id(sid) {
            return Err(format!("Invalid session_id format: {}", sid));
        }
        if !conversation_exists(sid) {
            return Err(format!("Agy conversation not found: {}", sid));
        }
    }

    if let Some(m) = model {
        if !is_valid_agy_model(m) {
            return Err(format!(
                "Unsupported agy model: {}. Use /model to list available agy models.",
                m
            ));
        }
    }
    let agy_bin = get_agy_path()
        .ok_or_else(|| "Agy CLI not found. Is Antigravity CLI installed?".to_string())?;

    let temp_dir = crate::utils::path::cokacdir_temp_dir()
        .map_err(|e| format!("Failed to prepare cokacdir temporary directory: {}", e))?;
    #[cfg(any(unix, windows))]
    let agy_hook_prompt = prepare_agy_hook_prompt(&temp_dir, system_prompt)
        .map_err(|e| format!("Failed to create private Agy system-prompt file: {}", e))?;
    #[cfg(not(any(unix, windows)))]
    let agy_hook_prompt: Option<AgyHookPrompt> = None;

    #[cfg(any(unix, windows))]
    let stdin_prompt: std::borrow::Cow<'_, str> = std::borrow::Cow::Borrowed(prompt);
    #[cfg(not(any(unix, windows)))]
    let stdin_prompt: std::borrow::Cow<'_, str> =
        std::borrow::Cow::Owned(build_legacy_agy_stdin_prompt(prompt, system_prompt));

    #[cfg(any(unix, windows))]
    let (hook_plugin, hook_executable) = if agy_hook_prompt.is_some() {
        let plugin = ensure_agy_hook_plugin()
            .map_err(|e| format!("Failed to configure the Agy system-prompt hook: {}", e))?;
        let executable = hook_executable_path()
            .map_err(|e| format!("Failed to resolve the Agy hook executable: {}", e))?;
        (Some(plugin), Some(executable))
    } else {
        (None, None)
    };
    #[cfg(not(any(unix, windows)))]
    let (hook_plugin, hook_executable): (Option<PathBuf>, Option<PathBuf>) = (None, None);
    let agy_log_guard = create_private_temp_file(&temp_dir, "agy_log", b"")
        .map_err(|e| format!("Failed to create private Agy log file: {}", e))?;
    let args = build_agy_command_args(
        session_id,
        &default_print_timeout(),
        agy_log_guard.path(),
        model,
    );
    let _agy_log_guard = agy_log_guard;

    agy_debug(&format!(
        "[stream] spawning {} {:?}; user_prompt_len={} system_prompt_len={} hook_plugin={:?}",
        agy_bin,
        args,
        prompt.len(),
        system_prompt.map(str::len).unwrap_or_default(),
        hook_plugin
    ));

    let mut cmd = Command::new(agy_bin);
    cmd.args(&args)
        .current_dir(working_dir)
        .env("PATH", enhanced_path_for_bin(agy_bin))
        .env_remove(AGY_HOOK_ENV_PROMPT_FILE)
        .env_remove(AGY_HOOK_ENV_TOKEN)
        .env_remove(AGY_HOOK_ENV_EXECUTABLE)
        .env_remove(AGY_HOOK_ENV_STATE_FILE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(ref hook_prompt) = agy_hook_prompt {
        cmd.env(AGY_HOOK_ENV_PROMPT_FILE, hook_prompt.prompt_path())
            .env(AGY_HOOK_ENV_TOKEN, hook_prompt.token())
            .env(AGY_HOOK_ENV_STATE_FILE, hook_prompt.state_path())
            .env(
                AGY_HOOK_ENV_EXECUTABLE,
                hook_executable
                    .as_ref()
                    .expect("hook executable exists with hook prompt"),
            );
    }
    crate::services::claude::detach_into_own_pgroup(&mut cmd);
    crate::services::claude::attach_cancel_cgroup(&mut cmd, cancel_token.as_ref());

    let mut child = ReapingAgyChild::new(cmd.spawn().map_err(|e| {
        agy_debug(&format!("[stream] spawn failed: {}", e));
        format!("Failed to start agy: {}", e)
    })?);
    agy_debug(&format!("[stream] spawned pid={}", child.id()));

    if let Some(ref token) = cancel_token {
        let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(child.id());
        drop(guard);
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            child.kill_and_reap();
            return Ok(());
        }
    }

    let stderr_thread = child.take_stderr().map(|stderr| {
        std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default())
    });

    // With no `--print`/`-p` flag, Agy treats non-TTY stdin as the complete
    // headless prompt. Closing the pipe is required so Agy sees EOF and starts
    // the request. On Unix and Windows, cokacdir's system instructions are
    // supplied as a transient system message by Agy's PreInvocation hook and
    // stdin contains only the user's current message. Unsupported target
    // families retain the composed-stdin compatibility transport.
    let stdin_result = match child.take_stdin() {
        Some(mut stdin) => {
            let result = stdin.write_all(stdin_prompt.as_bytes());
            drop(stdin);
            result.map_err(|e| format!("Failed to write Agy prompt to stdin: {}", e))
        }
        None => Err("Failed to open Agy stdin".to_string()),
    };
    if let Err(error) = stdin_result {
        agy_debug(&format!("[stream] stdin failed: {}", error));
        child.kill_and_reap();
        let stderr_msg = stderr_thread
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        if stderr_msg.is_empty() {
            return Err(error);
        }
        return Err(format!("{}; stderr: {}", error, stderr_msg.trim()));
    }
    agy_debug(&format!(
        "[stream] wrote {} stdin prompt bytes and closed it",
        stdin_prompt.len()
    ));

    // Agy 1.1.1 treats hook failures as non-fatal and would otherwise call the
    // model without cokacdir's system instructions. The helper acknowledges
    // only after its complete JSON response has been written. Refuse to wait
    // for or expose model output unless that handshake arrives promptly.
    if let Some(ref hook_prompt) = agy_hook_prompt {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let hook_state = hook_prompt.hook_state();
            if hook_state == AgyHookState::Failed {
                child.kill_and_reap();
                return Err(
                    "Agy failed while running cokacdir's system-prompt hook; the response was discarded."
                        .to_string(),
                );
            }
            if hook_prompt.acknowledged() && hook_state == AgyHookState::Complete {
                break;
            }
            if let Some(ref token) = cancel_token {
                if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    child.kill_and_reap();
                    return Ok(());
                }
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => {
                    child.kill_and_reap();
                    return Err(format!(
                        "Failed while waiting for the Agy system-prompt hook: {}",
                        error
                    ));
                }
            }
            if std::time::Instant::now() >= deadline {
                agy_debug("[stream] system-prompt hook acknowledgement timed out");
                child.kill_and_reap();
                let stderr_msg = stderr_thread
                    .and_then(|handle| handle.join().ok())
                    .unwrap_or_default();
                let suffix = if stderr_msg.trim().is_empty() {
                    String::new()
                } else {
                    format!("; stderr: {}", stderr_msg.trim())
                };
                return Err(format!(
                    "Agy did not acknowledge cokacdir's system-prompt hook within 30 seconds{}",
                    suffix
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    let stdout = child
        .take_stdout()
        .ok_or_else(|| "Failed to capture agy stdout".to_string())?;
    let (stdout_sender, stdout_receiver) = std::sync::mpsc::channel::<Result<String, String>>();
    let stdout_thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut chunk = String::new();
            match reader.read_line(&mut chunk) {
                Ok(0) => break,
                Ok(_) => {
                    if stdout_sender.send(Ok(chunk)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ =
                        stdout_sender.send(Err(format!("Failed to read agy output: {}", error)));
                    break;
                }
            }
        }
    });

    let mut raw_stdout = String::new();
    let mut visible_output = String::new();
    let mut forwarded_bytes = 0usize;
    let mut hook_pending_since: Option<std::time::Instant> = None;

    loop {
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                agy_debug("[stream] cancelled during stdout read");
                child.kill_and_reap();
                let _ = stdout_thread.join();
                let _ = stderr_thread.and_then(|handle| handle.join().ok());
                return Ok(());
            }
        }

        if let Some(ref hook_prompt) = agy_hook_prompt {
            match hook_prompt.hook_state() {
                AgyHookState::Complete => hook_pending_since = None,
                AgyHookState::Failed => {
                    agy_debug("[stream] Agy hook ledger reported failure");
                    child.kill_and_reap();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.and_then(|handle| handle.join().ok());
                    return Err(
                        "Agy failed while running cokacdir's system-prompt hook; the response was discarded."
                            .to_string(),
                    );
                }
                AgyHookState::Pending => {
                    let pending_since =
                        hook_pending_since.get_or_insert_with(std::time::Instant::now);
                    if pending_since.elapsed() >= std::time::Duration::from_secs(30) {
                        agy_debug("[stream] Agy hook ledger remained incomplete");
                        child.kill_and_reap();
                        let _ = stdout_thread.join();
                        let _ = stderr_thread.and_then(|handle| handle.join().ok());
                        return Err(
                            "Agy's system-prompt hook did not complete within 30 seconds; the response was discarded."
                                .to_string(),
                        );
                    }
                }
            }
        }

        let chunk = match stdout_receiver.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(Ok(chunk)) => chunk,
            Ok(Err(error)) => {
                agy_debug(&format!("[stream] stdout read failed: {}", error));
                child.kill_and_reap();
                let _ = stdout_thread.join();
                let _ = stderr_thread.and_then(|handle| handle.join().ok());
                return Err(error);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        raw_stdout.push_str(&chunk);
        agy_debug(&format!(
            "[stream] stdout chunk: {} bytes, preview={:?}",
            chunk.len(),
            log_preview(&chunk, 200)
        ));

        visible_output.push_str(&chunk);
        // Agy's hook runner is fail-open. With a system prompt, retain all
        // stdout until the child exits and every ledger start has a matching
        // successful completion;
        // otherwise a later failed PreInvocation could leak an answer after
        // the first successful acknowledgement. Runs without a hook retain
        // normal streaming behavior.
        if agy_hook_prompt.is_none() && forwarded_bytes < visible_output.len() {
            let pending = visible_output[forwarded_bytes..].to_string();
            if sender
                .send(StreamMessage::Text { content: pending })
                .is_err()
            {
                agy_debug("[stream] receiver dropped");
                child.kill_and_reap();
                let _ = stdout_thread.join();
                let _ = stderr_thread.and_then(|handle| handle.join().ok());
                return Ok(());
            }
            forwarded_bytes = visible_output.len();
        }
    }

    if stdout_thread.join().is_err() {
        child.kill_and_reap();
        let _ = stderr_thread.and_then(|handle| handle.join().ok());
        return Err("Agy stdout reader thread failed".to_string());
    }

    if let Some(ref token) = cancel_token {
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            agy_debug("[stream] cancelled after stdout read");
            child.kill_and_reap();
            let _ = stderr_thread.and_then(|handle| handle.join().ok());
            return Ok(());
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("Agy process error: {}", e))?;
    let stderr_msg = stderr_thread
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    if !stderr_msg.is_empty() {
        agy_debug(&format!(
            "[stream] stderr: {} bytes, preview={:?}",
            stderr_msg.len(),
            log_preview(&stderr_msg, 500)
        ));
    }

    let last_session_id = session_id
        .map(ToString::to_string)
        .or_else(|| read_last_conversation_id(working_dir));

    let hook_acknowledged = agy_hook_prompt
        .as_ref()
        .map(AgyHookPrompt::acknowledged)
        .unwrap_or(true);
    let hook_state = agy_hook_prompt
        .as_ref()
        .map(AgyHookPrompt::hook_state)
        .unwrap_or(AgyHookState::Complete);
    let hook_failed = hook_state != AgyHookState::Complete;
    let detected_error = if hook_failed {
        Some(
            "Agy failed while running cokacdir's system-prompt hook; the response was discarded."
                .to_string(),
        )
    } else if !hook_acknowledged {
        Some(
            "Agy completed without running cokacdir's system-prompt hook; the response was discarded."
                .to_string(),
        )
    } else if status.success() {
        stdout_absence_error_message(&raw_stdout)
    } else {
        None
    };
    if detected_error.is_some() || !status.success() {
        let discard_hook_output = hook_failed || !hook_acknowledged;
        let message =
            detected_error.unwrap_or_else(|| format!("Agy exited with code {:?}", status.code()));
        agy_debug(&format!(
            "[stream] error: {}, exit={:?}, stdout_len={}, stderr_len={}",
            message,
            status.code(),
            raw_stdout.len(),
            stderr_msg.len()
        ));
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: if discard_hook_output {
                String::new()
            } else {
                raw_stdout
            },
            stderr: stderr_msg,
            exit_code: status.code(),
        });
        return Ok(());
    }

    if forwarded_bytes < visible_output.len() {
        if sender
            .send(StreamMessage::Text {
                content: visible_output[forwarded_bytes..].to_string(),
            })
            .is_err()
        {
            return Ok(());
        }
    }

    let _ = sender.send(StreamMessage::Done {
        result: visible_output,
        session_id: last_session_id,
    });
    agy_debug("=== agy execute_command_streaming END ===");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        backup_sqlite_to_reserved_file, backup_sqlite_to_reserved_handle, build_agy_command_args,
        build_agy_hook_response, cleanup_owned_failed_clone, clone_file_identity,
        clone_target_still_owned, copy_conversation_to_id, ensure_agy_hook_plugin_in,
        execute_command_streaming, prepare_agy_hook_prompt, stdout_absence_error_message,
        working_dir_cache_keys, write_hook_ack, AGY_HOOK_INTERNAL_ARG,
    };

    #[test]
    fn agy_command_uses_user_stdin_without_prompt_or_workspace_flags() {
        let args = build_agy_command_args(
            Some("conversation-id"),
            "1h",
            std::path::Path::new(".cokacdir/tmp/agy.log"),
            Some("Gemini 3.5 Flash (Medium)"),
        );
        let args: Vec<_> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert!(!args.iter().any(|arg| matches!(
            arg.as_str(),
            "--print" | "-p" | "--prompt" | "--prompt-interactive" | "-i"
        )));
        assert!(!args.iter().any(|arg| arg == "--add-dir"));
        assert_eq!(
            args,
            vec![
                "--conversation",
                "conversation-id",
                "--print-timeout",
                "1h",
                "--log-file",
                ".cokacdir/tmp/agy.log",
                "--dangerously-skip-permissions",
                "--model",
                "Gemini 3.5 Flash (Medium)",
            ]
        );
    }

    #[test]
    fn hook_response_preserves_the_complete_utf8_system_prompt() {
        let temp = crate::utils::path::cokacdir_temp_dir().unwrap();
        let system_prompt = format!("  start\n{} end  ", "한글🙂".repeat(4_000));
        let hook_prompt = prepare_agy_hook_prompt(&temp, Some(&system_prompt))
            .unwrap()
            .unwrap();
        let input = r#"{"invocationNum":0,"initialNumSteps":1,"conversationId":"conversation"}"#;
        let (response, ack_path) =
            build_agy_hook_response(input, hook_prompt.prompt_path(), hook_prompt.token()).unwrap();
        let response: serde_json::Value = serde_json::from_slice(&response).unwrap();

        assert_eq!(
            response["injectSteps"][0]["ephemeralMessage"],
            system_prompt
        );
        assert!(!hook_prompt.acknowledged());
        write_hook_ack(&ack_path, hook_prompt.token()).unwrap();
        assert!(hook_prompt.acknowledged());
    }

    #[test]
    fn hook_response_requires_the_complete_pre_invocation_schema() {
        let temp = crate::utils::path::cokacdir_temp_dir().unwrap();
        let hook_prompt = prepare_agy_hook_prompt(&temp, Some("system prompt"))
            .unwrap()
            .unwrap();

        for input in [
            r#"{"initialNumSteps":1,"conversationId":"conversation"}"#,
            r#"{"invocationNum":0,"conversationId":"conversation"}"#,
            r#"{"invocationNum":0,"initialNumSteps":1,"conversationId":""}"#,
        ] {
            assert!(build_agy_hook_response(
                input,
                hook_prompt.prompt_path(),
                hook_prompt.token(),
            )
            .is_err());
        }
    }

    #[test]
    fn absent_or_blank_system_prompt_creates_no_hook_file() {
        let temp = tempfile::tempdir().unwrap();
        for prompt in [None, Some(""), Some(" \n\t ")] {
            assert!(prepare_agy_hook_prompt(temp.path(), prompt)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn private_hook_prompt_and_ack_are_removed_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let prompt = prepare_agy_hook_prompt(temp.path(), Some("secret system instructions"))
            .unwrap()
            .unwrap();
        let prompt_path = prompt.prompt_path().to_path_buf();
        let state_path = prompt.state_path().to_path_buf();
        let lease_path = prompt._lease_file.path().to_path_buf();
        let ack_path = super::derived_hook_ack_path(&prompt_path).unwrap();
        write_hook_ack(&ack_path, prompt.token()).unwrap();

        assert_eq!(
            std::fs::read_to_string(&prompt_path).unwrap(),
            "secret system instructions"
        );
        assert!(state_path.exists());
        assert_eq!(
            std::fs::read(&lease_path).unwrap(),
            super::agy_hook_lease_contents(&prompt_path, &state_path).unwrap()
        );
        assert!(ack_path.exists());
        drop(prompt);
        assert!(!prompt_path.exists());
        assert!(!state_path.exists());
        assert!(!lease_path.exists());
        assert!(!ack_path.exists());
    }

    #[test]
    fn hook_acknowledgement_is_idempotent_under_concurrent_writers() {
        let temp = tempfile::tempdir().unwrap();
        let ack_path = temp.path().join("hook.ack");
        let token = "0123456789abcdef0123456789abcdef".to_string();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let mut writers = Vec::new();

        for _ in 0..16 {
            let ack_path = ack_path.clone();
            let token = token.clone();
            let barrier = barrier.clone();
            writers.push(std::thread::spawn(move || {
                barrier.wait();
                write_hook_ack(&ack_path, &token)
            }));
        }

        for writer in writers {
            writer.join().unwrap().unwrap();
        }
        let contents = std::fs::read(&ack_path).unwrap();
        assert_eq!(contents.as_slice(), token.as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn private_hook_prompt_uses_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let prompt = prepare_agy_hook_prompt(temp.path(), Some("secret instructions"))
            .unwrap()
            .unwrap();
        for path in [
            prompt.prompt_path(),
            prompt.state_path(),
            prompt._lease_file.path(),
        ] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn hook_ledger_detects_a_later_invocation_failure() {
        let temp = tempfile::tempdir().unwrap();
        let prompt = prepare_agy_hook_prompt(temp.path(), Some("system instructions"))
            .unwrap()
            .unwrap();
        assert_eq!(prompt.hook_state(), super::AgyHookState::Pending);

        let run_hook_shell = |executable: &str| {
            std::process::Command::new("sh")
                .args(["-c", &super::agy_hook_command()])
                .env(super::AGY_HOOK_ENV_EXECUTABLE, executable)
                .env(super::AGY_HOOK_ENV_STATE_FILE, prompt.state_path())
                .env(super::AGY_HOOK_ENV_TOKEN, prompt.token())
                .output()
                .unwrap()
        };

        assert!(run_hook_shell("/bin/true").status.success());
        assert_eq!(prompt.hook_state(), super::AgyHookState::Complete);
        assert!(run_hook_shell("/bin/true").status.success());
        assert_eq!(prompt.hook_state(), super::AgyHookState::Complete);

        let failed = run_hook_shell("/bin/false");
        assert!(!failed.status.success());
        assert_eq!(prompt.hook_state(), super::AgyHookState::Failed);
        let ledger = std::fs::read_to_string(prompt.state_path()).unwrap();
        assert_eq!(ledger.matches("start ").count(), 3);
        assert_eq!(ledger.matches("ok ").count(), 2);
        assert_eq!(ledger.matches("fail ").count(), 1);
    }

    #[test]
    fn hook_ledger_rejects_path_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let prompt = prepare_agy_hook_prompt(temp.path(), Some("system instructions"))
            .unwrap()
            .unwrap();
        let original = prompt.state_path().with_extension("owned-original");
        std::fs::rename(prompt.state_path(), &original).unwrap();
        std::fs::write(prompt.state_path(), b"").unwrap();

        assert_eq!(prompt.hook_state(), super::AgyHookState::Failed);

        std::fs::remove_file(prompt.state_path()).unwrap();
        std::fs::rename(original, prompt.state_path()).unwrap();
    }

    #[test]
    fn hook_ledger_rejects_success_before_start() {
        let temp = tempfile::tempdir().unwrap();
        let prompt = prepare_agy_hook_prompt(temp.path(), Some("system instructions"))
            .unwrap()
            .unwrap();
        std::fs::write(
            prompt.state_path(),
            format!("ok {}\nstart {}\n", prompt.token(), prompt.token()),
        )
        .unwrap();

        assert_eq!(prompt.hook_state(), super::AgyHookState::Failed);
    }

    #[test]
    fn stale_hook_cleanup_preserves_live_leases_and_removes_crash_residue() {
        let temp = tempfile::tempdir().unwrap();
        let active = prepare_agy_hook_prompt(temp.path(), Some("active instructions"))
            .unwrap()
            .unwrap();
        let active_prompt = active.prompt_path().to_path_buf();
        let active_state = active.state_path().to_path_buf();
        let active_lease = active._lease_file.path().to_path_buf();

        let stale_prompt_path = temp.path().join(format!(
            "{}_{}",
            super::AGY_SYSTEM_PROMPT_PREFIX,
            "1".repeat(32)
        ));
        std::fs::write(&stale_prompt_path, b"stale secret").unwrap();
        let stale_ack = super::derived_hook_ack_path(&stale_prompt_path).unwrap();
        std::fs::write(&stale_ack, b"stale").unwrap();

        let stale_state_path = temp.path().join(format!(
            "{}_{}",
            super::AGY_HOOK_STATE_PREFIX,
            "2".repeat(32)
        ));
        std::fs::write(&stale_state_path, b"start stale").unwrap();
        let stale_lease_contents =
            super::agy_hook_lease_contents(&stale_prompt_path, &stale_state_path).unwrap();
        let stale_lease_path = temp.path().join(format!(
            "{}_{}",
            super::AGY_HOOK_LEASE_PREFIX,
            "3".repeat(32)
        ));
        std::fs::write(&stale_lease_path, stale_lease_contents).unwrap();

        let _cleanup_lock = super::acquire_agy_temp_cleanup_lock(temp.path()).unwrap();
        super::cleanup_stale_agy_hook_files(temp.path()).unwrap();

        assert!(active_prompt.exists());
        assert!(active_state.exists());
        assert!(active_lease.exists());
        assert!(!stale_prompt_path.exists());
        assert!(!stale_ack.exists());
        assert!(!stale_state_path.exists());
        assert!(!stale_lease_path.exists());
    }

    #[test]
    fn stale_lease_cannot_remove_files_mapped_by_a_live_lease() {
        let temp = tempfile::tempdir().unwrap();
        let active = prepare_agy_hook_prompt(temp.path(), Some("active instructions"))
            .unwrap()
            .unwrap();
        let active_prompt = active.prompt_path().to_path_buf();
        let active_state = active.state_path().to_path_buf();
        let active_lease = active._lease_file.path().to_path_buf();
        let active_ack = super::derived_hook_ack_path(&active_prompt).unwrap();
        write_hook_ack(&active_ack, active.token()).unwrap();

        // This valid-looking stale lease sorts before every normally generated
        // random lease and maliciously points at the active run's files.
        let stale_lease = temp.path().join(format!(
            "{}_{}",
            super::AGY_HOOK_LEASE_PREFIX,
            "0".repeat(32)
        ));
        assert_ne!(stale_lease, active_lease);
        std::fs::write(
            &stale_lease,
            super::agy_hook_lease_contents(&active_prompt, &active_state).unwrap(),
        )
        .unwrap();

        let _cleanup_lock = super::acquire_agy_temp_cleanup_lock(temp.path()).unwrap();
        super::cleanup_stale_agy_hook_files(temp.path()).unwrap();

        assert!(active_prompt.exists());
        assert!(active_state.exists());
        assert!(active_lease.exists());
        assert!(active_ack.exists());
        assert!(!stale_lease.exists());
    }

    #[test]
    fn stale_cleanup_honors_legacy_prompt_and_state_locks() {
        let temp = tempfile::tempdir().unwrap();
        let prompt_path = temp.path().join(format!(
            "{}_{}",
            super::AGY_SYSTEM_PROMPT_PREFIX,
            "4".repeat(32)
        ));
        let state_path = temp.path().join(format!(
            "{}_{}",
            super::AGY_HOOK_STATE_PREFIX,
            "5".repeat(32)
        ));
        std::fs::write(&prompt_path, b"legacy prompt").unwrap();
        std::fs::write(&state_path, b"legacy state").unwrap();
        let ack_path = super::derived_hook_ack_path(&prompt_path).unwrap();
        std::fs::write(&ack_path, b"legacy ack").unwrap();

        let prompt_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&prompt_path)
            .unwrap();
        let state_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&state_path)
            .unwrap();
        fs2::FileExt::lock_exclusive(&prompt_lock).unwrap();
        fs2::FileExt::lock_exclusive(&state_lock).unwrap();

        let _cleanup_lock = super::acquire_agy_temp_cleanup_lock(temp.path()).unwrap();
        super::cleanup_stale_agy_hook_files(temp.path()).unwrap();
        assert!(prompt_path.exists());
        assert!(state_path.exists());
        assert!(ack_path.exists());

        drop(prompt_lock);
        drop(state_lock);
        super::cleanup_stale_agy_hook_files(temp.path()).unwrap();
        assert!(!prompt_path.exists());
        assert!(!state_path.exists());
        assert!(!ack_path.exists());
    }

    #[test]
    fn platform_lock_contention_error_is_recognized() {
        assert!(super::agy_file_lock_is_contended(&fs2::lock_contended_error()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_hook_wrapper_contains_complete_ledger_protocol() {
        let command = super::agy_hook_command();

        assert!(command.contains(&format!(
            "echo start %{}%",
            super::AGY_HOOK_ENV_TOKEN
        )));
        assert!(command.contains(&format!("echo ok %{}%", super::AGY_HOOK_ENV_TOKEN)));
        assert!(command.contains(&format!(
            "echo fail %{}%",
            super::AGY_HOOK_ENV_TOKEN
        )));
        assert!(command.contains("exit /b 125"));
    }

    #[test]
    fn namespaced_hook_plugin_is_created_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let plugin = ensure_agy_hook_plugin_in(temp.path()).unwrap();
        let first_hooks = std::fs::read(plugin.join("hooks.json")).unwrap();
        let second = ensure_agy_hook_plugin_in(temp.path()).unwrap();

        assert_eq!(plugin, second);
        assert_eq!(
            first_hooks,
            std::fs::read(plugin.join("hooks.json")).unwrap()
        );
        let hooks: serde_json::Value = serde_json::from_slice(&first_hooks).unwrap();
        let command = hooks[super::AGY_HOOK_NAME]["PreInvocation"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains(AGY_HOOK_INTERNAL_ARG));
        assert!(command.contains(super::AGY_HOOK_ENV_STATE_FILE));
        assert!(plugin.join("plugin.json").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn global_hook_is_a_shell_level_noop_without_cokacdir_environment() {
        let output = std::process::Command::new("sh")
            .args(["-c", &super::agy_hook_command()])
            .env_remove(super::AGY_HOOK_ENV_EXECUTABLE)
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, b"{}\n");
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn hook_plugin_never_overwrites_an_unowned_directory() {
        let temp = tempfile::tempdir().unwrap();
        let plugin = temp.path().join("plugins").join(super::AGY_HOOK_PLUGIN_DIR);
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("user-file"), b"keep").unwrap();

        let error = ensure_agy_hook_plugin_in(temp.path()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(plugin.join("user-file")).unwrap(), b"keep");
    }

    #[test]
    fn hook_plugin_respects_an_explicit_disabled_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let plugin = ensure_agy_hook_plugin_in(temp.path()).unwrap();
        std::fs::rename(
            plugin.join("plugin.json"),
            plugin.join("plugin.json.disabled"),
        )
        .unwrap();

        let error = ensure_agy_hook_plugin_in(temp.path()).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!plugin.join("plugin.json").exists());
        assert!(plugin.join("plugin.json.disabled").is_file());
    }

    #[test]
    #[ignore = "requires an installed, authenticated Agy CLI and network access"]
    fn live_agy_streaming_round_trip() {
        if std::env::var("COKAC_AGY_LIVE_TEST").as_deref() != Ok("1") {
            return;
        }
        assert!(
            std::env::var_os(super::AGY_HOOK_EXECUTABLE_OVERRIDE).is_some(),
            "set COKACDIR_AGY_HOOK_EXECUTABLE_OVERRIDE to a built cokacdir binary"
        );

        fn collect_live_result(
            receiver: std::sync::mpsc::Receiver<crate::services::claude::StreamMessage>,
        ) -> (String, Option<String>) {
            for message in receiver {
                match message {
                    crate::services::claude::StreamMessage::Done { result, session_id } => {
                        return (result, session_id)
                    }
                    crate::services::claude::StreamMessage::Error {
                        message,
                        stdout,
                        stderr,
                        ..
                    } => {
                        panic!(
                            "Agy live test failed: {message}; stdout={stdout:?}; stderr={stderr:?}"
                        )
                    }
                    _ => {}
                }
            }
            panic!("Agy live test ended without a Done message")
        }

        let temp = tempfile::tempdir().unwrap();
        let working_dir = temp.path().to_string_lossy().into_owned();
        let model = super::list_models()
            .into_iter()
            .next()
            .expect("Agy must expose at least one model");
        let (sender, receiver) = std::sync::mpsc::channel();

        execute_command_streaming(
            "What is 2 + 2? Return only the result.",
            None,
            &working_dir,
            sender,
            Some(
                "For this integration check, create a file named cokacdir-hook-one.txt in the active workspace containing exactly HOOK_FILE_ONE. Then reply with exactly COKACDIR_AGY_LIVE_OK and nothing else.",
            ),
            None,
            None,
            Some(&model),
            false,
        )
        .unwrap();

        let (completed_result, session_id) = collect_live_result(receiver);
        assert!(completed_result.contains("COKACDIR_AGY_LIVE_OK"));
        assert_eq!(
            std::fs::read_to_string(temp.path().join("cokacdir-hook-one.txt"))
                .unwrap()
                .trim(),
            "HOOK_FILE_ONE"
        );
        let session_id = session_id.expect("new Agy call must report its conversation id");

        let (sender, receiver) = std::sync::mpsc::channel();
        execute_command_streaming(
            "What is 3 + 3? Return only the result.",
            Some(&session_id),
            &working_dir,
            sender,
            Some(
                "For this resumed integration check, create a file named cokacdir-hook-two.txt in the active workspace containing exactly HOOK_FILE_TWO. Then reply with exactly COKACDIR_AGY_RESUME_OK and nothing else.",
            ),
            None,
            None,
            Some(&model),
            false,
        )
        .unwrap();

        let (resumed_result, resumed_session_id) = collect_live_result(receiver);
        assert!(resumed_result.contains("COKACDIR_AGY_RESUME_OK"));
        assert_eq!(
            std::fs::read_to_string(temp.path().join("cokacdir-hook-two.txt"))
                .unwrap()
                .trim(),
            "HOOK_FILE_TWO"
        );
        assert_eq!(resumed_session_id.as_deref(), Some(session_id.as_str()));
    }

    #[test]
    fn detects_successful_empty_stdout_as_error() {
        assert_eq!(
            stdout_absence_error_message("").as_deref(),
            Some("Agy exited successfully but produced no stdout response.")
        );
    }

    #[test]
    fn allows_successful_visible_stdout() {
        assert!(stdout_absence_error_message("OLD\nNEW\n").is_none());
    }

    #[test]
    fn includes_windows_and_slash_cache_key_variants() {
        let keys = working_dir_cache_keys(r"C:\Users\kst\.cokacdir\workspace\eikfuccw");
        assert!(keys.contains(&r"C:\Users\kst\.cokacdir\workspace\eikfuccw".to_string()));
        assert!(keys.contains(&"C:/Users/kst/.cokacdir/workspace/eikfuccw".to_string()));
    }

    #[test]
    fn agy_clone_never_overwrites_an_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.pb");
        let target = temp.path().join("fixed.pb");
        std::fs::write(&source, b"source conversation").unwrap();
        std::fs::write(&target, b"existing clone").unwrap();

        let error = copy_conversation_to_id(&source, "fixed").unwrap_err();

        assert!(error.contains("create Agy clone target"));
        assert_eq!(std::fs::read(target).unwrap(), b"existing clone");
    }

    #[cfg(unix)]
    #[test]
    fn agy_clone_rejects_symlink_conversation_sources() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside.txt");
        let source = temp.path().join("source.pb");
        std::fs::write(&outside, b"must not be cloned").unwrap();
        symlink(&outside, &source).unwrap();

        let error = copy_conversation_to_id(&source, "clone").unwrap_err();

        assert!(error.contains("not a regular file"));
        assert!(!temp.path().join("clone.pb").exists());
    }

    #[cfg(unix)]
    #[test]
    fn agy_sqlite_clone_rejects_symlink_source_before_reserving_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside.db");
        let source = temp.path().join("source.db");
        rusqlite::Connection::open(&outside)
            .unwrap()
            .execute_batch("CREATE TABLE data(value TEXT);")
            .unwrap();
        symlink(&outside, &source).unwrap();

        let error = copy_conversation_to_id(&source, "clone").unwrap_err();

        assert!(error.contains("Failed to open Agy conversation"));
        assert!(!temp.path().join("clone.db").exists());
    }

    #[test]
    fn concurrent_sqlite_clone_collision_keeps_one_complete_database() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        {
            let conn = rusqlite::Connection::open(&source).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE messages(body TEXT); \
                 INSERT INTO messages VALUES ('first'), ('second');",
            )
            .unwrap();
        }

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let source = source.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                copy_conversation_to_id(&source, "same-clone")
            }));
        }
        barrier.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);

        assert!(!temp.path().join("same-clone.db-journal").exists());
        assert!(!temp.path().join("same-clone.db-wal").exists());
        assert!(!temp.path().join("same-clone.db-shm").exists());
        let clone = rusqlite::Connection::open(temp.path().join("same-clone.db")).unwrap();
        let count: i64 = clone
            .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn empty_sqlite_database_can_be_cloned() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("empty.db");
        drop(rusqlite::Connection::open(&source).unwrap());
        assert_eq!(std::fs::metadata(&source).unwrap().len(), 0);

        copy_conversation_to_id(&source, "empty-clone").unwrap();

        let clone = rusqlite::Connection::open(temp.path().join("empty-clone.db")).unwrap();
        let integrity: String = clone
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        assert!(
            std::fs::metadata(temp.path().join("empty-clone.db"))
                .unwrap()
                .len()
                >= 512
        );
    }

    #[test]
    fn sqlite_clone_includes_uncheckpointed_wal_rows() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("active.db");
        let writer = rusqlite::Connection::open(&source).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; \
                 CREATE TABLE messages(body TEXT); \
                 INSERT INTO messages VALUES ('first'), ('second'), ('third');",
            )
            .unwrap();
        let mut wal_path = source.as_os_str().to_os_string();
        wal_path.push("-wal");
        assert!(
            std::fs::metadata(std::path::PathBuf::from(wal_path))
                .unwrap()
                .len()
                > 0
        );

        copy_conversation_to_id(&source, "active-clone").unwrap();

        let clone = rusqlite::Connection::open(temp.path().join("active-clone.db")).unwrap();
        let bodies: String = clone
            .query_row(
                "SELECT group_concat(body, ',') FROM (SELECT body FROM messages ORDER BY rowid)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bodies, "first,second,third");
        let integrity: String = clone
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        drop(writer);
    }

    #[cfg(target_os = "linux")]
    fn current_rss_kib() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmRSS:")?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
            .expect("VmRSS must be present in /proc/self/status")
    }

    #[cfg(target_os = "linux")]
    fn run_sqlite_clone_streaming_memory_child() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("large.db");
        let source_conn = rusqlite::Connection::open(&source).unwrap();
        source_conn
            .execute_batch(
                "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; \
                 CREATE TABLE payload(value BLOB);",
            )
            .unwrap();
        const PAYLOAD_BYTES: i64 = 64 * 1024 * 1024;
        source_conn
            .execute("INSERT INTO payload VALUES (zeroblob(?1))", [PAYLOAD_BYTES])
            .unwrap();
        drop(source_conn);

        let source_size = std::fs::metadata(&source).unwrap().len();
        assert!(source_size >= PAYLOAD_BYTES as u64);
        let baseline = current_rss_kib();
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let peak = std::sync::Arc::new(AtomicU64::new(baseline));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let monitor_running = running.clone();
        let monitor_peak = peak.clone();
        let monitor = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            while monitor_running.load(Ordering::Acquire) {
                monitor_peak.fetch_max(current_rss_kib(), Ordering::AcqRel);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            monitor_peak.fetch_max(current_rss_kib(), Ordering::AcqRel);
        });
        ready_rx.recv().unwrap();

        let result = copy_conversation_to_id(&source, "large-clone");
        running.store(false, Ordering::Release);
        monitor.join().unwrap();
        result.unwrap();

        let growth_kib = peak.load(Ordering::Acquire).saturating_sub(baseline);
        let limit_kib = source_size / 2 / 1024;
        assert!(
            growth_kib < limit_kib,
            "SQLite clone RSS grew by {growth_kib} KiB for a {source_size}-byte database; expected less than {limit_kib} KiB"
        );
        let clone = rusqlite::Connection::open(temp.path().join("large-clone.db")).unwrap();
        let cloned_bytes: i64 = clone
            .query_row("SELECT length(value) FROM payload", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cloned_bytes, PAYLOAD_BYTES);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sqlite_clone_memory_stays_bounded() {
        const CHILD_ENV: &str = "COKACDIR_AGY_SQLITE_MEMORY_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).as_deref() == Some(std::ffi::OsStr::new("1")) {
            run_sqlite_clone_streaming_memory_child();
            return;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "services::agy::tests::sqlite_clone_memory_stays_bounded",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated memory regression test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn sqlite_clone_waits_for_a_transient_exclusive_writer() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        let locker = rusqlite::Connection::open(&source).unwrap();
        locker
            .execute_batch(
                "PRAGMA journal_mode=DELETE; CREATE TABLE messages(body TEXT); \
                 INSERT INTO messages VALUES ('committed'); BEGIN EXCLUSIVE; \
                 INSERT INTO messages VALUES ('pending');",
            )
            .unwrap();

        let source_for_worker = source.clone();
        let worker =
            std::thread::spawn(move || copy_conversation_to_id(&source_for_worker, "writer-clone"));
        std::thread::sleep(std::time::Duration::from_millis(150));
        locker.execute_batch("COMMIT;").unwrap();

        worker.join().unwrap().unwrap();
        let clone = rusqlite::Connection::open(temp.path().join("writer-clone.db")).unwrap();
        let count: i64 = clone
            .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[cfg(unix)]
    #[test]
    fn failed_clone_cleanup_removes_only_the_reserved_inode() {
        use std::os::unix::fs::OpenOptionsExt;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("partial.db");
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let reserved = options.open(&target).unwrap();
        let identity = clone_file_identity(&reserved).unwrap();

        assert!(cleanup_owned_failed_clone(&target, temp.path(), identity).unwrap());
        assert!(!target.exists());
        reserved.sync_all().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_backup_stays_bound_to_reserved_inode_after_path_swap() {
        use std::os::unix::fs::{symlink, OpenOptionsExt};

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        let target = temp.path().join("clone.db");
        let recovery = temp.path().join("clone-owned-recovery.db");
        let victim = temp.path().join("victim.txt");
        let source_conn = rusqlite::Connection::open(&source).unwrap();
        source_conn
            .execute_batch("CREATE TABLE messages(body TEXT); INSERT INTO messages VALUES ('ok');")
            .unwrap();
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let reserved = options.open(&target).unwrap();
        let identity = clone_file_identity(&reserved).unwrap();
        std::fs::rename(&target, &recovery).unwrap();
        std::fs::write(&victim, b"must survive").unwrap();
        symlink(&victim, &target).unwrap();
        backup_sqlite_to_reserved_handle(&source_conn, &reserved, &target, identity).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"must survive");
        assert!(!clone_target_still_owned(&target, identity).unwrap_or(false));
        assert!(!cleanup_owned_failed_clone(&target, temp.path(), identity).unwrap());
        let recovered = rusqlite::Connection::open(&recovery).unwrap();
        let body: String = recovered
            .query_row("SELECT body FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(body, "ok");
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_clone_rejects_path_swap_before_backup() {
        use std::os::unix::fs::{symlink, OpenOptionsExt};

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        let target = temp.path().join("clone.db");
        let recovery = temp.path().join("clone-owned-recovery.db");
        let victim = temp.path().join("victim.db");
        let source_conn = rusqlite::Connection::open(&source).unwrap();
        source_conn
            .execute_batch("CREATE TABLE messages(body TEXT); INSERT INTO messages VALUES ('ok');")
            .unwrap();
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let reserved = options.open(&target).unwrap();
        let identity = clone_file_identity(&reserved).unwrap();

        std::fs::rename(&target, &recovery).unwrap();
        rusqlite::Connection::open(&victim)
            .unwrap()
            .execute_batch("CREATE TABLE protected(value TEXT);")
            .unwrap();
        symlink(&victim, &target).unwrap();

        let error =
            backup_sqlite_to_reserved_file(&source_conn, &reserved, &target, identity).unwrap_err();
        assert!(error.contains("changed before SQLite backup"));
        let victim = rusqlite::Connection::open(&victim).unwrap();
        let table_count: i64 = victim
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'protected'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 1);
        assert_eq!(std::fs::metadata(recovery).unwrap().len(), 0);
    }
}
