use regex::Regex;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

pub(crate) struct PrivateTempFile {
    path: std::path::PathBuf,
    file_name: std::ffi::OsString,
    directory_access: crate::services::file_ops::DirectoryAccess,
    identity: crate::services::file_ops::StablePathIdentity,
    directory_path: std::path::PathBuf,
    directory_identity: crate::services::file_ops::StablePathIdentity,
    directory_guard: Option<std::fs::File>,
}

impl PrivateTempFile {
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub(crate) fn identity(&self) -> crate::services::file_ops::StablePathIdentity {
        self.identity
    }

    pub(crate) fn verified_path(&self) -> std::io::Result<&std::path::Path> {
        if crate::services::file_ops::stable_path_identity(&self.directory_path)?
            != self.directory_identity
            || crate::services::file_ops::stable_path_identity(&self.path)? != self.identity
        {
            return Err(std::io::Error::other(
                "private temporary path changed before use",
            ));
        }
        Ok(&self.path)
    }
}

impl Drop for PrivateTempFile {
    fn drop(&mut self) {
        let _ = self
            .directory_access
            .remove_file_if_identity(&self.file_name, self.identity);
        drop(self.directory_guard.take());
    }
}

/// Create an application-owned temporary file without predictable names,
/// symlink following, or a truncate-existing window. The guard removes only
/// the file it successfully created.
pub(crate) fn create_private_temp_file(
    dir: &std::path::Path,
    prefix: &str,
    contents: &[u8],
) -> std::io::Result<PrivateTempFile> {
    std::fs::create_dir_all(dir)?;
    let (directory_guard, directory_access, metadata) =
        crate::services::file_ops::open_directory_for_read(dir)?;
    let directory_identity = crate::services::file_ops::stable_file_identity(&directory_guard)?;
    if !metadata.file_type().is_dir()
        || crate::services::file_ops::stable_path_identity(dir)? != directory_identity
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "private temporary path is not a stable real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory_guard.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    if crate::services::file_ops::stable_path_identity(dir)? != directory_identity {
        return Err(std::io::Error::other(
            "private temporary directory changed while it was secured",
        ));
    }

    for _ in 0..32 {
        let file_name = format!("{}_{:032x}", prefix, rand::random::<u128>());
        let path = dir.join(&file_name);
        let mut file = match directory_access.open_file(
            std::ffi::OsStr::new(&file_name),
            crate::services::file_ops::DirectoryFileOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600),
        ) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let identity = match crate::services::file_ops::stable_file_identity(&file) {
            Ok(identity) => identity,
            Err(error) => {
                drop(file);
                let name = std::ffi::OsStr::new(&file_name);
                if let Ok(identity) = directory_access.child_identity(name) {
                    let _ = directory_access.remove_file_if_identity(name, identity);
                }
                return Err(error);
            }
        };
        if let Err(e) = file.write_all(contents).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = directory_access
                .remove_file_if_identity(std::ffi::OsStr::new(&file_name), identity);
            return Err(e);
        }
        drop(file);
        let published_identity = crate::services::file_ops::stable_path_identity(&path);
        let current_directory_identity = crate::services::file_ops::stable_path_identity(dir);
        if published_identity.as_ref().ok() != Some(&identity)
            || current_directory_identity.as_ref().ok() != Some(&directory_identity)
        {
            let _ = directory_access
                .remove_file_if_identity(std::ffi::OsStr::new(&file_name), identity);
            return Err(published_identity
                .err()
                .or_else(|| current_directory_identity.err())
                .unwrap_or_else(|| {
                    std::io::Error::other(
                        "private temporary path changed while the file was created",
                    )
                }));
        }
        return Ok(PrivateTempFile {
            path: path.clone(),
            file_name: file_name.into(),
            directory_access,
            identity,
            directory_path: dir.to_path_buf(),
            directory_identity,
            directory_guard: Some(directory_guard),
        });
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique private temporary file",
    ))
}

/// Global debug flag — toggled by /debug command or COKACDIR_DEBUG=1 env var
pub static DEBUG_ENABLED: AtomicBool = AtomicBool::new(false);

/// Build a UTF-8-safe preview string capped at `max_chars` characters.
pub fn safe_preview(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Initialize debug flag from environment variable or bot_settings.json (call once at startup)
pub fn init_debug_from_env() {
    if std::env::var("COKACDIR_DEBUG")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        DEBUG_ENABLED.store(true, Ordering::Relaxed);
        return;
    }
    // Also check bot_settings.json for any bot with debug=true
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".cokacdir").join("bot_settings.json");
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(obj) = json.as_object() {
                    for (_key, entry) in obj {
                        if entry
                            .get("debug")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            DEBUG_ENABLED.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Cached path to the claude binary.
/// Once resolved, reused for all subsequent calls.
static CLAUDE_PATH: OnceLock<Option<String>> = OnceLock::new();

fn claude_path_is_runnable(path: &str) -> bool {
    let path = std::path::Path::new(path);
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        matches!(extension.as_str(), "exe" | "cmd" | "bat" | "com")
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

fn runnable_claude_candidate(candidate: Option<String>) -> Option<String> {
    candidate.filter(|path| !path.is_empty() && claude_path_is_runnable(path))
}

/// Resolve the path to the claude binary.
/// First tries `which claude`, then falls back to `bash -lc "which claude"`
/// (for non-interactive SSH sessions where ~/.profile isn't loaded).
#[cfg(unix)]
fn resolve_claude_path() -> Option<String> {
    if let Some(path) = runnable_claude_candidate(std::env::var("COKAC_CLAUDE_PATH").ok()) {
        return Some(path);
    }

    // Try direct `which claude` first
    if let Ok(output) = Command::new("which").arg("claude").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Some(path) = runnable_claude_candidate(Some(path)) {
                return Some(path);
            }
        }
    }

    // Fallback: use login shell to resolve PATH
    if let Ok(output) = Command::new("bash").args(["-lc", "which claude"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Some(path) = runnable_claude_candidate(Some(path)) {
                return Some(path);
            }
        }
    }

    None
}

/// Decode bytes from Windows console command output into a UTF-8 String.
/// Windows commands like `where` output text in the system code page (e.g., CP949 for Korean),
/// not UTF-8. This function tries multiple code pages to ensure correct decoding:
///   1. UTF-8 (fast path — covers systems with UTF-8 locale enabled)
///   2. OEM code page (CP_OEMCP — what most console commands use)
///   3. ANSI code page (CP_ACP — fallback when OEM decoding produces an invalid path)
#[cfg(windows)]
pub fn decode_windows_output(bytes: &[u8]) -> String {
    // Fast path: if it's already valid UTF-8, no conversion needed
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }

    extern "system" {
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            src: *const u8,
            src_len: i32,
            dst: *mut u16,
            dst_len: i32,
        ) -> i32;
    }
    const CP_OEMCP: u32 = 1; // System OEM code page (e.g., CP949 for Korean)
    const CP_ACP: u32 = 0; // System ANSI code page (e.g., CP1252 for Western European)

    let decode_with_cp = |cp: u32| -> Option<String> {
        unsafe {
            let len = MultiByteToWideChar(
                cp,
                0,
                bytes.as_ptr(),
                bytes.len() as i32,
                std::ptr::null_mut(),
                0,
            );
            if len <= 0 {
                return None;
            }
            let mut wide = vec![0u16; len as usize];
            MultiByteToWideChar(
                cp,
                0,
                bytes.as_ptr(),
                bytes.len() as i32,
                wide.as_mut_ptr(),
                len,
            );
            Some(String::from_utf16_lossy(&wide))
        }
    };

    // Try OEM first (most console commands use this), verify the path exists
    if let Some(oem) = decode_with_cp(CP_OEMCP) {
        let first_line = oem.trim().lines().next().unwrap_or("");
        if first_line.is_empty() || std::path::Path::new(first_line).exists() {
            return oem;
        }
        // OEM decoded but path doesn't exist — try ANSI
        if let Some(ansi) = decode_with_cp(CP_ACP) {
            let ansi_first = ansi.trim().lines().next().unwrap_or("");
            if std::path::Path::new(ansi_first).exists() {
                return ansi;
            }
        }
        // Neither exists — return OEM result (less likely to be wrong)
        return oem;
    }

    // OEM decode failed entirely — try ANSI
    if let Some(ansi) = decode_with_cp(CP_ACP) {
        return ansi;
    }

    String::from_utf8_lossy(bytes).to_string()
}

/// Search for an executable using Windows SearchPathW API.
/// Returns the full path as UTF-8, with no code page conversion issues
/// since SearchPathW operates entirely in UTF-16.
#[cfg(windows)]
pub fn search_path_wide(name: &str, ext: Option<&str>) -> Option<String> {
    extern "system" {
        fn SearchPathW(
            path: *const u16,
            file_name: *const u16,
            extension: *const u16,
            buffer_len: u32,
            buffer: *mut u16,
            file_part: *mut *mut u16,
        ) -> u32;
    }

    let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let ext_w: Option<Vec<u16>> = ext.map(|e| e.encode_utf16().chain(std::iter::once(0)).collect());
    let ext_ptr = ext_w.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());

    unsafe {
        let needed = SearchPathW(
            std::ptr::null(),
            name_w.as_ptr(),
            ext_ptr,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if needed == 0 {
            return None;
        }
        let mut buf = vec![0u16; needed as usize];
        let written = SearchPathW(
            std::ptr::null(),
            name_w.as_ptr(),
            ext_ptr,
            needed,
            buf.as_mut_ptr(),
            std::ptr::null_mut(),
        );
        if written == 0 || written >= needed {
            return None;
        }
        String::from_utf16(&buf[..written as usize]).ok()
    }
}

#[cfg(windows)]
fn resolve_claude_path() -> Option<String> {
    if let Some(path) = runnable_claude_candidate(std::env::var("COKAC_CLAUDE_PATH").ok()) {
        return Some(path);
    }

    // Use SearchPathW (UTF-16 native) — no code page issues with non-ASCII paths
    if let Some(path) = search_path_wide("claude", Some(".exe")) {
        if claude_path_is_runnable(&path) {
            return Some(path);
        }
    }
    if let Some(path) = search_path_wide("claude", Some(".cmd")) {
        if claude_path_is_runnable(&path) {
            return Some(path);
        }
    }

    // Fallback: check npm global install paths
    if let Ok(output) = Command::new("cmd").args(["/c", "npm root -g"]).output() {
        if output.status.success() {
            let npm_root = decode_windows_output(&output.stdout).trim().to_string();
            // `npm root -g` points at .../npm/node_modules. CreateProcessW
            // cannot execute the package's cli.js directly; npm installs the
            // runnable command shim one directory above node_modules.
            let claude_path = std::path::Path::new(&npm_root)
                .parent()
                .map(|npm_bin| npm_bin.join("claude.cmd"))?;
            if claude_path_is_runnable(&claude_path.to_string_lossy()) {
                return Some(claude_path.display().to_string());
            }
        }
    }

    None
}

/// Get the cached claude binary path, resolving it on first call.
fn get_claude_path() -> Option<&'static str> {
    CLAUDE_PATH.get_or_init(|| resolve_claude_path()).as_deref()
}

/// Build a PATH string with the binary's parent directory prepended.
/// This ensures that Node.js-based CLI tools (with `#!/usr/bin/env node` shebang)
/// can find `node` even when launched from environments where nvm/fnm isn't loaded
/// (e.g., launchd services, cron, non-interactive SSH sessions).
pub fn enhanced_path_for_bin(bin_path: &str) -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    if let Some(parent) = std::path::Path::new(bin_path)
        .parent()
        .and_then(|p| p.to_str())
    {
        if !parent.is_empty() {
            let sep = if cfg!(windows) { ';' } else { ':' };
            if !current.split(sep).any(|p| p == parent) {
                return format!("{}{}{}", parent, sep, current);
            }
        }
    }
    current
}

/// Debug logging helper (active when /debug toggled ON or COKACDIR_DEBUG=1)
pub fn debug_log(msg: &str) {
    debug_log_to("claude.log", msg);
}

pub fn debug_log_to(filename: &str, msg: &str) {
    if !DEBUG_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if let Some(home) = dirs::home_dir() {
        let debug_dir = home.join(".cokacdir").join("debug");
        let _ = std::fs::create_dir_all(&debug_dir);
        let log_path = debug_dir.join(filename);
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(log_path)
        {
            let timestamp = chrono::Local::now().format("%H:%M:%S%.3f");
            let _ = writeln!(file, "[{}] {}", timestamp, msg);
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeResponse {
    pub success: bool,
    pub response: Option<String>,
    pub session_id: Option<String>,
    pub error: Option<String>,
}

/// Streaming message types for real-time Claude responses
#[derive(Debug, Clone)]
pub enum StreamMessage {
    /// Initialization - contains session_id
    Init { session_id: String },
    /// Text response chunk
    Text { content: String },
    /// Tool use started
    ToolUse { name: String, input: String },
    /// Tool execution result
    ToolResult { content: String, is_error: bool },
    /// Background task notification
    TaskNotification {
        task_id: String,
        status: String,
        summary: String,
    },
    /// Completion
    Done {
        result: String,
        session_id: Option<String>,
    },
    /// Error
    Error {
        message: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    },
}

/// Token for cooperative cancellation of streaming requests.
/// Holds a flag and the child process PID so the caller can kill it externally.
/// On Linux, also optionally holds a per-spawn cgroup v2 handle whose
/// `cgroup.kill` atomically kills every descendant — including ones the AI
/// CLI detaches via setsid/double-fork that escape pgroup-based kills.
pub struct CancelToken {
    pub cancelled: std::sync::atomic::AtomicBool,
    pub child_pid: std::sync::Mutex<Option<u32>>,
    pub owner_dispatch_id: std::sync::atomic::AtomicU64,
    pub cgroup: std::sync::Mutex<Option<std::sync::Arc<crate::services::cgroup::KillCgroup>>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            child_pid: std::sync::Mutex::new(None),
            owner_dispatch_id: std::sync::atomic::AtomicU64::new(0),
            cgroup: std::sync::Mutex::new(None),
        }
    }

    /// Signal cancellation and immediately terminate the tracked child
    /// process — including any subprocesses it spawned (Bash from a tool
    /// call, the bun binary behind opencode, etc.) — if its PID has been
    /// registered. Without the signal the streaming reader would stay
    /// blocked on stdout until the child exits naturally, defeating the
    /// purpose of cancellation.
    ///
    /// Linux: if a per-spawn cgroup v2 handle was attached
    /// (`attach_cancel_cgroup` at spawn time), write "1" to its
    /// `cgroup.kill` first — this is a kernel-atomic SIGKILL to every PID
    /// in the cgroup including descendants the AI CLI moved into a new
    /// session via `setsid()` (e.g. Claude Code's Bash tool spawns its
    /// shell with `child_process.spawn({ detached: true })`, which puts it
    /// outside any pgroup we can reach with `kill(-pgid, …)`). cgroup
    /// membership is inherited at fork() and cannot be left by an
    /// unprivileged process, so it catches setsid/setpgid/double-fork
    /// alike. The pgroup kill that follows is kept as a fallback for
    /// environments without cgroup v2 (older kernels, restrictive
    /// sandboxes) where `KillCgroup::new` returns None and the token's
    /// cgroup field stays empty.
    ///
    /// Unix pgroup fallback: SIGKILL the entire process group (negative
    /// PID). SIGKILL (not SIGTERM) because the AI CLIs are per-request
    /// throwaway processes with no client-side state to flush on shutdown,
    /// and SIGTERM is catchable — a graceful-shutdown handler that stops
    /// emitting stdout while in-flight API requests finish would leave the
    /// worker thread blocked on `reader.lines()` indefinitely. SIGKILL is
    /// uncatchable. Requires `detach_into_own_pgroup` at spawn so the
    /// negative PID targets the child's group, not cokacdir's.
    ///
    /// Windows: `taskkill /T /F` already kills the process tree by PID, so
    /// the cgroup path is unused.
    pub fn cancel_now(&self) {
        self.cancelled.store(true, Ordering::Relaxed);

        // Preferred path: cgroup v2 atomic kill catches every descendant.
        // Held in its own scope so the lock is released before we touch the
        // PID mutex below — the two are independent and we don't want a
        // poisoned pid mutex to skip the cgroup kill.
        {
            let cg_guard = self.cgroup.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref cg) = *cg_guard {
                cg.kill_all();
            }
        }

        // Fallback: pgroup-based kill of the immediate child. Harmless even
        // when the cgroup path already fired — on Linux the descendants are
        // already gone; this just hits the (probably already-dead) direct
        // child a second time. On non-Linux this is the only mechanism.
        let guard = self.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pid) = *guard {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .output();
            }
        }
    }
}

/// Set up the optional cgroup v2 layer for cancellation on `cmd` and store
/// the resulting cgroup in `token` so `cancel_now` can find it. The pgroup
/// layer (`detach_into_own_pgroup`) should already have been applied by the
/// caller; this is an additive enhancement that catches descendants the
/// pgroup kill can't reach (setsid'd grandchildren etc.).
///
/// No-op if `token` is None, if not running on Linux, or if cgroup v2 is
/// unavailable / sub-cgroup creation is denied — in those cases cancel
/// behavior degrades to the pre-existing pgroup-only path.
pub fn attach_cancel_cgroup(
    cmd: &mut std::process::Command,
    token: Option<&std::sync::Arc<CancelToken>>,
) {
    let token = match token {
        Some(t) => t,
        None => return,
    };
    if let Some(cg) = crate::services::cgroup::KillCgroup::new() {
        cg.attach_command(cmd);
        let arc = std::sync::Arc::new(cg);
        let mut guard = token.cgroup.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(arc);
    }
}

/// `attach_cancel_cgroup` variant for `tokio::process::Command` (used by the
/// opencode serve path, which spawns asynchronously). Same semantics as the
/// std-Command version — see `attach_cancel_cgroup` for details.
pub fn attach_cancel_cgroup_tokio(
    cmd: &mut tokio::process::Command,
    token: Option<&std::sync::Arc<CancelToken>>,
) {
    let token = match token {
        Some(t) => t,
        None => return,
    };
    if let Some(cg) = crate::services::cgroup::KillCgroup::new() {
        cg.attach_tokio_command(cmd);
        let arc = std::sync::Arc::new(cg);
        let mut guard = token.cgroup.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(arc);
    }
}

/// Place a child into its own process group on Unix so that
/// `cancel_now` / `kill_child_tree` can signal the entire group — the child
/// plus any subprocesses it spawned (Bash from a tool call, the bun binary
/// behind an npm-launcher CLI, etc.). Without this the kill targets only the
/// immediate child and grandchildren are reparented to init as orphans.
/// No-op on Windows where `taskkill /T /F` already kills the tree by PID.
pub fn detach_into_own_pgroup(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Kill a child process and its entire process tree.
/// On Unix, SIGKILLs the child's process group (requires the child was
/// spawned with `detach_into_own_pgroup`). On Windows, uses `taskkill
/// /PID /T /F` to kill the process tree so that child processes (bash.exe,
/// node.exe, etc.) don't survive.
pub fn kill_child_tree(child: &mut std::process::Child) {
    let pid = child.id();
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
    #[cfg(not(windows))]
    {
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

/// Receiver shutdown means nobody can consume any more pipe output. Kill and
/// reap immediately instead of breaking the read loop and then waiting while
/// the child can block forever on a full stdout pipe.
pub(crate) fn terminate_child_after_receiver_drop(child: &mut std::process::Child) {
    kill_child_tree(child);
    let _ = child.wait();
}

/// Cached regex pattern for session ID validation
fn session_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[a-zA-Z0-9_-]+$").expect("Invalid session ID regex pattern"))
}

/// Validate session ID format (alphanumeric, dashes, underscores only).
/// Rejects a leading `-` because the value is spliced into argv and would
/// otherwise be parsed as a CLI flag (`--config`, `-i`, …).
/// Max length capped at 64 characters for security.
fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 64
        && !session_id.starts_with('-')
        && session_id_regex().is_match(session_id)
}

/// Default allowed tools for Claude CLI
pub const DEFAULT_ALLOWED_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "Task",
    "TaskOutput",
    "TaskStop",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Skill",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
];

/// Execute a command using Claude CLI
pub fn execute_command(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    allowed_tools: Option<&[String]>,
    model: Option<&str>,
) -> ClaudeResponse {
    let tools_str = match allowed_tools {
        Some(tools) => tools.join(","),
        None => DEFAULT_ALLOWED_TOOLS.join(","),
    };
    let mut args = vec![
        "-p".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--tools".to_string(),
        tools_str,
        "--output-format".to_string(),
        "json".to_string(),
    ];

    let default_system_prompt = r#"You are a terminal file manager assistant. Be concise. Focus on file operations. Respond in the same language as the user.

SECURITY RULES (MUST FOLLOW):
- NEVER execute destructive commands like rm -rf, format, mkfs, dd, etc.
- NEVER modify system files in /etc, /sys, /proc, /boot
- NEVER access or modify files outside the current working directory without explicit user path
- NEVER execute commands that could harm the system or compromise security
- ONLY suggest safe file operations: copy, move, rename, create directory, view, edit
- If a request seems dangerous, explain the risk and suggest a safer alternative

BASH EXECUTION RULES (MUST FOLLOW):
- All commands MUST run non-interactively without user input
- Use -y, --yes, or --non-interactive flags (e.g., apt install -y, npm init -y)
- Use -m flag for commit messages (e.g., git commit -m "message")
- Disable pagers with --no-pager or pipe to cat (e.g., git --no-pager log)
- NEVER use commands that open editors (vim, nano, etc.)
- NEVER use commands that wait for stdin without arguments
- NEVER use interactive flags like -i

IMPORTANT: Format your responses using Markdown for better readability:
- Use **bold** for important terms or commands
- Use `code` for file paths, commands, and technical terms
- Use bullet lists (- item) for multiple items
- Use numbered lists (1. item) for sequential steps
- Use code blocks (```language) for multi-line code or command examples
- Use headers (## Title) to organize longer responses
- Keep formatting minimal and terminal-friendly"#;

    // Write system prompt to file to avoid OS "Argument list too long" (E2BIG).
    let sp_dir = match crate::utils::path::cokacdir_temp_dir() {
        Ok(dir) => dir,
        Err(e) => {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!(
                    "Failed to prepare cokacdir temporary directory: {}",
                    e
                )),
            }
        }
    };
    let sp_guard = match create_private_temp_file(
        &sp_dir,
        "system_prompt",
        default_system_prompt.as_bytes(),
    ) {
        Ok(guard) => guard,
        Err(e) => {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!("Failed to write system prompt file: {}", e)),
            }
        }
    };
    args.push("--append-system-prompt-file".to_string());
    let verified_prompt_path = match sp_guard.verified_path() {
        Ok(path) => path.to_string_lossy().to_string(),
        Err(error) => {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!("System prompt file changed before use: {error}")),
            }
        }
    };
    args.push(verified_prompt_path);
    let _sp_guard = sp_guard;

    // Set model if specified
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }

    // Resume session if available
    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some("Invalid session ID format".to_string()),
            };
        }
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }

    let claude_bin = match get_claude_path() {
        Some(path) => path,
        None => {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some("Claude CLI not found. Is Claude CLI installed?".to_string()),
            };
        }
    };

    let mut child = match Command::new(claude_bin)
        .args(&args)
        .current_dir(working_dir)
        .env("PATH", enhanced_path_for_bin(claude_bin))
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000")
        .env("BASH_DEFAULT_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env("BASH_MAX_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env_remove("CLAUDECODE") // Allow running from within Claude Code sessions
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!(
                    "Failed to start Claude: {}. Is Claude CLI installed?",
                    e
                )),
            };
        }
    };

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }

    // Wait for output
    match child.wait_with_output() {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                parse_claude_output(&stdout)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                ClaudeResponse {
                    success: false,
                    response: None,
                    session_id: None,
                    error: Some(if stderr.is_empty() {
                        format!("Process exited with code {:?}", output.status.code())
                    } else {
                        stderr
                    }),
                }
            }
        }
        Err(e) => ClaudeResponse {
            success: false,
            response: None,
            session_id: None,
            error: Some(format!("Failed to read output: {}", e)),
        },
    }
}

/// Parse Claude CLI JSON output
fn parse_claude_output(output: &str) -> ClaudeResponse {
    let mut session_id: Option<String> = None;
    let mut response_text = String::new();

    for line in output.trim().lines() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            // Extract session ID
            if let Some(sid) = json.get("session_id").and_then(|v| v.as_str()) {
                session_id = Some(sid.to_string());
            }

            // Extract response text
            if let Some(result) = json.get("result").and_then(|v| v.as_str()) {
                response_text = result.to_string();
            } else if let Some(message) = json.get("message").and_then(|v| v.as_str()) {
                response_text = message.to_string();
            } else if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                response_text = content.to_string();
            }
        } else if !line.trim().is_empty() && !line.starts_with('{') {
            response_text.push_str(line);
            response_text.push('\n');
        }
    }

    // If no structured response, use raw output
    if response_text.is_empty() {
        response_text = output.trim().to_string();
    }

    ClaudeResponse {
        success: true,
        response: Some(response_text.trim().to_string()),
        session_id,
        error: None,
    }
}

/// Verify whether a session's task has been fully completed.
/// Forks the session, asks Claude to judge completeness, and returns the result.
pub fn verify_completion(
    session_id: &str,
    working_dir: &str,
    effort: Option<&str>,
) -> Result<VerifyResult, String> {
    debug_log("=== verify_completion START ===");
    debug_log(&format!("  session_id: {}", session_id));
    debug_log(&format!("  working_dir: {}", working_dir));

    if !is_valid_session_id(session_id) {
        debug_log("  ERROR: Invalid session ID format");
        return Err("Invalid session ID format".to_string());
    }

    let claude_bin = get_claude_path().ok_or_else(|| {
        debug_log("  ERROR: Claude CLI not found");
        "Claude CLI not found".to_string()
    })?;
    debug_log(&format!("  claude_bin: {}", claude_bin));

    let mut args = vec![
        "-p".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--no-session-persistence".to_string(),
        "--max-turns".to_string(),
        "1".to_string(),
        "--tools".to_string(),
        "".to_string(),
    ];
    if let Some(effort) = effort {
        args.push("--effort".to_string());
        args.push(effort.to_string());
    }
    args.push("--resume".to_string());
    args.push(session_id.to_string());
    args.push("--fork-session".to_string());
    debug_log(&format!("  args: {:?}", args));

    let verify_prompt = "Review what you just did in this session. \
        Do NOT use any tools — judge purely from the conversation history. \
        \
        If the task appears fully and safely complete, respond with ONLY the single word: mission_complete \
        \
        Otherwise respond with: mission_pending \
        followed by ONE short follow-up instruction (1–2 sentences). \
        \
        CRITICAL — what this follow-up instruction IS: \
        The text you write after `mission_pending` will be taken verbatim and \
        delivered as the NEXT USER MESSAGE to the very same working agent that \
        just performed the task. The agent will read it as if the user typed it \
        into the chat. Therefore write it as a direct, second-person request \
        from the user, not as a review/verdict/analysis. \
        \
        The instruction should ask the agent to re-examine, re-verify, or \
        double-check whatever it just did — whatever form that work took. \
        Let the phrasing flow naturally from the actual work, not from a \
        fixed template. \
        \
        Rules: \
        - Second-person imperative, as the user would type. \
        - NOT a diagnosis, NOT a checklist of missing items, NOT a summary of \
          what was done. \
        - Match the language of the preceding conversation. \
        - 1–2 sentences. No preface, no \"I think\", no meta commentary.";

    debug_log("  Spawning Claude process...");
    let spawn_start = std::time::Instant::now();
    let mut child = Command::new(&claude_bin)
        .args(&args)
        .current_dir(working_dir)
        .env("PATH", enhanced_path_for_bin(&claude_bin))
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            debug_log(&format!("  ERROR: Failed to spawn: {}", e));
            format!("Failed to start Claude for verify_completion: {}", e)
        })?;
    debug_log(&format!(
        "  Process spawned in {:?}, pid={:?}",
        spawn_start.elapsed(),
        child.id()
    ));

    if let Some(mut stdin) = child.stdin.take() {
        debug_log("  Writing verify_prompt to stdin...");
        let write_result = stdin.write_all(verify_prompt.as_bytes());
        debug_log(&format!("  stdin write result: {:?}", write_result.is_ok()));
        drop(stdin);
        debug_log("  stdin dropped (closed)");
    } else {
        debug_log("  WARNING: Could not get stdin handle");
    }

    debug_log("  Waiting for process to complete...");
    let wait_start = std::time::Instant::now();
    let output = child.wait_with_output().map_err(|e| {
        debug_log(&format!(
            "  ERROR: wait_with_output failed after {:?}: {}",
            wait_start.elapsed(),
            e
        ));
        format!("Failed to read verify_completion output: {}", e)
    })?;
    debug_log(&format!(
        "  Process completed in {:?}",
        wait_start.elapsed()
    ));
    debug_log(&format!("  exit status: {:?}", output.status));
    debug_log(&format!("  stdout len: {} bytes", output.stdout.len()));
    debug_log(&format!("  stderr len: {} bytes", output.stderr.len()));

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        debug_log(&format!(
            "  ERROR: Process failed. exit_code={:?}",
            output.status.code()
        ));
        debug_log(&format!("  stderr: {}", safe_preview(&stderr, 500)));
        debug_log(&format!("  stdout: {}", safe_preview(&stdout, 500)));
        return Err(format!(
            "verify_completion process failed (exit {:?}). stderr: {}",
            output.status.code(),
            safe_preview(&stderr, 500)
        ));
    }
    debug_log("  Process exit status: success");

    let response_text = String::from_utf8_lossy(&output.stdout).to_string();
    let preview: String = response_text.chars().take(300).collect();
    debug_log(&format!("  response preview: {}", preview));

    if response_text.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        debug_log(&format!(
            "  ERROR: Empty response. exit={:?}, stderr: {}",
            output.status.code(),
            safe_preview(&stderr, 500)
        ));
        return Err(format!(
            "verify_completion returned empty (exit {:?}). stderr: {}",
            output.status.code(),
            safe_preview(&stderr, 500)
        ));
    }

    // Treat as complete only when "mission_complete" is present AND
    // "mission_pending" is absent. If both keywords coexist (LLM ambiguity),
    // err on the side of treating it as incomplete so feedback is consumed.
    let pending = response_text.contains("mission_pending");
    let complete = response_text.contains("mission_complete") && !pending;
    let feedback = if complete {
        None
    } else {
        // Strip verification keywords from feedback before returning.
        // This text is sent back to Claude as the next prompt, so it should
        // contain only the description of remaining work.
        let cleaned = response_text
            .replace("mission_pending", "")
            .replace("mission_complete", "");
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.to_string())
        }
    };

    debug_log(&format!(
        "  complete={}, feedback={:?}",
        complete,
        feedback.as_ref().map(|s| safe_preview(s, 200))
    ));
    debug_log("=== verify_completion END ===");

    Ok(VerifyResult { complete, feedback })
}

/// Result of verify_completion
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub complete: bool,
    pub feedback: Option<String>,
}

/// Check if Claude CLI is available
pub fn is_claude_available() -> bool {
    get_claude_path().is_some()
}

/// Check if a model string refers to the Claude backend
pub fn is_claude_model(model: Option<&str>) -> bool {
    model
        .map(|m| m == "claude" || m.starts_with("claude:"))
        .unwrap_or(false)
}

/// Strip "claude:" prefix and return the actual model name.
/// Returns None if the input is just "claude" (use CLI default).
/// Also strips display-name suffix (" — Description") if present.
pub fn strip_claude_prefix(model: &str) -> Option<&str> {
    model
        .strip_prefix("claude:")
        .filter(|s| !s.is_empty())
        .map(|s| s.split(" \u{2014} ").next().unwrap_or(s).trim())
}

/// Check if platform supports AI features
pub fn is_ai_supported() -> bool {
    true
}

/// Execute a command using Claude CLI with streaming output
/// If `system_prompt` is None, uses the default file manager system prompt.
/// If `system_prompt` is Some(""), no system prompt is appended.
pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    model: Option<&str>,
    no_session_persistence: bool,
    fork_session: bool,
    use_chrome: bool,
    effort: Option<&str>,
) -> Result<(), String> {
    debug_log("========================================");
    debug_log("=== execute_command_streaming START ===");
    debug_log("========================================");
    debug_log(&format!("prompt_len: {} chars", prompt.len()));
    let prompt_preview: String = prompt.chars().take(200).collect();
    debug_log(&format!("prompt_preview: {:?}", prompt_preview));
    debug_log(&format!("session_id: {:?}", session_id));
    debug_log(&format!("working_dir: {}", working_dir));
    debug_log(&format!("timestamp: {:?}", std::time::SystemTime::now()));

    let default_system_prompt = r#"You are a terminal file manager assistant. Be concise. Focus on file operations. Respond in the same language as the user.

SECURITY RULES (MUST FOLLOW):
- NEVER execute destructive commands like rm -rf, format, mkfs, dd, etc.
- NEVER modify system files in /etc, /sys, /proc, /boot
- NEVER access or modify files outside the current working directory without explicit user path
- NEVER execute commands that could harm the system or compromise security
- ONLY suggest safe file operations: copy, move, rename, create directory, view, edit
- If a request seems dangerous, explain the risk and suggest a safer alternative

BASH EXECUTION RULES (MUST FOLLOW):
- All commands MUST run non-interactively without user input
- Use -y, --yes, or --non-interactive flags (e.g., apt install -y, npm init -y)
- Use -m flag for commit messages (e.g., git commit -m "message")
- Disable pagers with --no-pager or pipe to cat (e.g., git --no-pager log)
- NEVER use commands that open editors (vim, nano, etc.)
- NEVER use commands that wait for stdin without arguments
- NEVER use interactive flags like -i

IMPORTANT: Format your responses using Markdown for better readability:
- Use **bold** for important terms or commands
- Use `code` for file paths, commands, and technical terms
- Use bullet lists (- item) for multiple items
- Use numbered lists (1. item) for sequential steps
- Use code blocks (```language) for multi-line code or command examples
- Use headers (## Title) to organize longer responses
- Keep formatting minimal and terminal-friendly"#;

    let tools_str = match allowed_tools {
        Some(tools) => tools.join(","),
        None => DEFAULT_ALLOWED_TOOLS.join(","),
    };
    let mut args = vec![
        "-p".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--tools".to_string(),
        tools_str,
        "--verbose".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];

    // Always write system prompt to file and use --append-system-prompt-file
    // to avoid OS "Argument list too long" (E2BIG) error.
    let effective_prompt = match system_prompt {
        None => Some(default_system_prompt),
        Some("") => None,
        Some(p) => Some(p),
    };
    let mut _sp_guard: Option<PrivateTempFile> = None;
    if let Some(sp) = effective_prompt {
        let sp_dir = crate::utils::path::cokacdir_temp_dir()
            .map_err(|e| format!("Failed to prepare cokacdir temporary directory: {}", e))?;
        let sp_guard =
            create_private_temp_file(&sp_dir, "system_prompt", sp.as_bytes()).map_err(|e| {
                debug_log(&format!("ERROR: Failed to write system prompt file: {}", e));
                format!("Failed to write system prompt file: {}", e)
            })?;
        debug_log(&format!(
            "System prompt written to {:?} ({} bytes)",
            sp_guard.path(),
            sp.len()
        ));
        let verified_prompt_path = sp_guard
            .verified_path()
            .map_err(|error| format!("System prompt file changed before use: {error}"))?
            .to_string_lossy()
            .to_string();
        args.push("--append-system-prompt-file".to_string());
        args.push(verified_prompt_path);
        _sp_guard = Some(sp_guard);
    }

    // Set model if specified
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }

    if let Some(effort) = effort {
        args.push("--effort".to_string());
        args.push(effort.to_string());
    }

    // Enable Chrome browser tool
    if use_chrome {
        args.push("--chrome".to_string());
    }

    // Disable session persistence (prevents Claude from saving session to ~/.claude/sessions/)
    if no_session_persistence {
        args.push("--no-session-persistence".to_string());
    }

    // Resume session if available
    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            debug_log("ERROR: Invalid session ID format");
            return Err("Invalid session ID format".to_string());
        }
        args.push("--resume".to_string());
        args.push(sid.to_string());
        if fork_session {
            args.push("--fork-session".to_string());
        }
    }

    let claude_bin = get_claude_path().ok_or_else(|| {
        debug_log("ERROR: Claude CLI not found");
        "Claude CLI not found. Is Claude CLI installed?".to_string()
    })?;

    debug_log("--- Spawning claude process ---");
    debug_log(&format!("Command: {}", claude_bin));
    debug_log(&format!("Args count: {}", args.len()));
    for (i, arg) in args.iter().enumerate() {
        if arg.len() > 100 {
            let truncated: String = arg.chars().take(100).collect();
            debug_log(&format!(
                "  arg[{}]: {}... (truncated, {} chars total)",
                i,
                truncated,
                arg.len()
            ));
        } else {
            debug_log(&format!("  arg[{}]: {}", i, arg));
        }
    }
    debug_log("Env: CLAUDE_CODE_MAX_OUTPUT_TOKENS=64000");
    debug_log("Env: BASH_DEFAULT_TIMEOUT_MS=86400000");
    debug_log("Env: BASH_MAX_TIMEOUT_MS=86400000");

    let spawn_start = std::time::Instant::now();
    let mut cmd = Command::new(claude_bin);
    cmd.args(&args)
        .current_dir(working_dir)
        .env("PATH", enhanced_path_for_bin(claude_bin))
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000")
        .env("BASH_DEFAULT_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env("BASH_MAX_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env_remove("CLAUDECODE") // Allow running from within Claude Code sessions
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    detach_into_own_pgroup(&mut cmd);
    attach_cancel_cgroup(&mut cmd, cancel_token.as_ref());
    let mut child = cmd.spawn().map_err(|e| {
        debug_log(&format!(
            "ERROR: Failed to spawn after {:?}: {}",
            spawn_start.elapsed(),
            e
        ));
        format!("Failed to start Claude: {}. Is Claude CLI installed?", e)
    })?;
    debug_log(&format!(
        "Claude process spawned successfully in {:?}, pid={:?}",
        spawn_start.elapsed(),
        child.id()
    ));

    // Store child PID in cancel token so the caller can kill it externally.
    // Recover from a poisoned mutex (a prior holder panicked) instead of
    // silently dropping the PID — without the PID stored, /stop cannot
    // signal this child and it would leak as an orphan.
    if let Some(ref token) = cancel_token {
        let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(child.id());
        drop(guard);
        // If /stop arrived before PID was stored, kill immediately
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        debug_log(&format!(
            "Writing prompt to stdin ({} bytes)...",
            prompt.len()
        ));
        let write_start = std::time::Instant::now();
        let write_result = stdin.write_all(prompt.as_bytes());
        debug_log(&format!(
            "stdin.write_all completed in {:?}, result={:?}",
            write_start.elapsed(),
            write_result.is_ok()
        ));
        // stdin is dropped here, which closes it - this signals end of input to claude
        debug_log("stdin handle dropped (closed)");
    } else {
        debug_log("WARNING: Could not get stdin handle!");
    }

    // Drain stderr in a background thread to prevent deadlock: if the child
    // writes more than the OS pipe buffer (~64KB) to stderr while we're
    // blocked reading stdout, the child's stderr write blocks and the whole
    // pipeline hangs. Mirrors the pattern in codex.rs / agy.rs.
    let stderr_thread = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default())
    });

    // Read stdout line by line for streaming
    debug_log("Taking stdout handle...");
    let stdout = child.stdout.take().ok_or_else(|| {
        debug_log("ERROR: Failed to capture stdout");
        "Failed to capture stdout".to_string()
    })?;
    let reader = BufReader::new(stdout);
    debug_log("BufReader created, ready to read lines...");

    let mut last_session_id: Option<String> = None;
    let mut final_result: Option<String> = None;
    let mut stdout_error: Option<(String, String)> = None; // (message, raw_line)
    let mut line_count = 0;
    let mut receiver_dropped = false;

    debug_log("Entering lines loop - will block until first line arrives...");
    for line in reader.lines() {
        // Check cancel token before processing each line
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                debug_log("Cancel detected — killing child process");
                kill_child_tree(&mut child);
                let _ = child.wait();
                return Ok(());
            }
        }

        debug_log(&format!("Line {} - read started", line_count + 1));
        let line = match line {
            Ok(l) => {
                debug_log(&format!(
                    "Line {} - read completed: {} chars",
                    line_count + 1,
                    l.len()
                ));
                l
            }
            Err(e) => {
                debug_log(&format!("ERROR: Failed to read line: {}", e));
                let _ = sender.send(StreamMessage::Error {
                    message: format!("Failed to read output: {}", e),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
                break;
            }
        };

        line_count += 1;
        debug_log(&format!("Line {}: {} chars", line_count, line.len()));

        if line.trim().is_empty() {
            debug_log("  (empty line, skipping)");
            continue;
        }

        let line_preview: String = line.chars().take(200).collect();
        debug_log(&format!("  Raw line preview: {}", line_preview));

        if let Ok(json) = serde_json::from_str::<Value>(&line) {
            let msg_type = json
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let msg_subtype = json.get("subtype").and_then(|v| v.as_str()).unwrap_or("-");
            debug_log(&format!(
                "  JSON parsed: type={}, subtype={}",
                msg_type, msg_subtype
            ));

            // Log more details for specific message types
            if msg_type == "assistant" {
                if let Some(content) = json.get("message").and_then(|m| m.get("content")) {
                    debug_log(&format!("  Assistant content array: {}", content));
                }
            }

            debug_log("  Calling parse_stream_message...");
            if let Some(msg) = parse_stream_message(&json) {
                debug_log(&format!(
                    "  Parsed message variant: {:?}",
                    std::mem::discriminant(&msg)
                ));

                // Track session_id and final result for Done message
                match &msg {
                    StreamMessage::Init { session_id } => {
                        debug_log(&format!("  >>> Init: session_id={}", session_id));
                        last_session_id = Some(session_id.clone());
                    }
                    StreamMessage::Text { content } => {
                        let preview: String = content.chars().take(100).collect();
                        debug_log(&format!(
                            "  >>> Text: {} chars, preview: {:?}",
                            content.len(),
                            preview
                        ));
                    }
                    StreamMessage::ToolUse { name, input } => {
                        let input_preview: String = input.chars().take(200).collect();
                        debug_log(&format!(
                            "  >>> ToolUse: name={}, input_preview={:?}",
                            name, input_preview
                        ));
                    }
                    StreamMessage::ToolResult { content, is_error } => {
                        let content_preview: String = content.chars().take(200).collect();
                        debug_log(&format!(
                            "  >>> ToolResult: is_error={}, content_len={}, preview={:?}",
                            is_error,
                            content.len(),
                            content_preview
                        ));
                    }
                    StreamMessage::Done { result, session_id } => {
                        let result_preview: String = result.chars().take(100).collect();
                        debug_log(&format!(
                            "  >>> Done: result_len={}, session_id={:?}, preview={:?}",
                            result.len(),
                            session_id,
                            result_preview
                        ));
                        final_result = Some(result.clone());
                        if session_id.is_some() {
                            last_session_id = session_id.clone();
                        }
                    }
                    StreamMessage::Error { ref message, .. } => {
                        debug_log(&format!("  >>> Error: {}", message));
                        stdout_error = Some((message.clone(), line.clone()));
                        continue; // don't send yet; will combine with stderr after process exits
                    }
                    StreamMessage::TaskNotification {
                        task_id,
                        status,
                        summary,
                    } => {
                        debug_log(&format!(
                            "  >>> TaskNotification: task_id={}, status={}, summary={}",
                            task_id, status, summary
                        ));
                    }
                }

                // Send message to channel
                debug_log("  Sending message to channel...");
                let send_result = sender.send(msg);
                if send_result.is_err() {
                    debug_log("  ERROR: Channel send failed (receiver dropped)");
                    receiver_dropped = true;
                    break;
                }
                debug_log("  Message sent to channel successfully");
            } else {
                debug_log(&format!(
                    "  parse_stream_message returned None for type={}",
                    msg_type
                ));
            }
        } else {
            let invalid_preview: String = line.chars().take(200).collect();
            debug_log(&format!("  NOT valid JSON: {}", invalid_preview));
        }
    }

    debug_log("--- Exited lines loop ---");
    debug_log(&format!("Total lines read: {}", line_count));
    debug_log(&format!("final_result present: {}", final_result.is_some()));
    debug_log(&format!("last_session_id: {:?}", last_session_id));

    if receiver_dropped {
        debug_log("Receiver dropped — terminating child before wait");
        terminate_child_after_receiver_drop(&mut child);
        return Ok(());
    }

    // Check cancel token after exiting the loop
    if let Some(ref token) = cancel_token {
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            debug_log("Cancel detected after loop — killing child process");
            kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    // Wait for process to finish
    debug_log("Waiting for child process to finish (child.wait())...");
    let wait_start = std::time::Instant::now();
    let status = child.wait().map_err(|e| {
        debug_log(&format!(
            "ERROR: Process wait failed after {:?}: {}",
            wait_start.elapsed(),
            e
        ));
        format!("Process error: {}", e)
    })?;
    debug_log(&format!(
        "Process finished in {:?}, status: {:?}, exit_code: {:?}",
        wait_start.elapsed(),
        status,
        status.code()
    ));

    // Handle stdout error or non-zero exit code
    if stdout_error.is_some() || !status.success() {
        // Collect stderr drained by the background thread.
        let stderr_msg = stderr_thread
            .and_then(|h| h.join().ok())
            .unwrap_or_default();

        let (message, stdout_raw) = if let Some((msg, raw)) = stdout_error {
            (msg, raw)
        } else {
            (
                format!("Process exited with code {:?}", status.code()),
                String::new(),
            )
        };

        debug_log(&format!(
            "Sending error: message={}, exit_code={:?}",
            message,
            status.code()
        ));
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: stdout_raw,
            stderr: stderr_msg,
            exit_code: status.code(),
        });
        return Ok(());
    }

    // If we didn't get a proper Done message, send one now
    if final_result.is_none() {
        debug_log("No Done message received, sending synthetic Done message...");
        let send_result = sender.send(StreamMessage::Done {
            result: String::new(),
            session_id: last_session_id.clone(),
        });
        debug_log(&format!(
            "Synthetic Done message sent, result={:?}",
            send_result.is_ok()
        ));
    } else {
        debug_log("Done message was already received, not sending synthetic one");
    }

    debug_log("========================================");
    debug_log("=== execute_command_streaming END (success) ===");
    debug_log("========================================");
    Ok(())
}

/// Parse a stream-json line into a StreamMessage
fn parse_stream_message(json: &Value) -> Option<StreamMessage> {
    let msg_type = json.get("type")?.as_str()?;

    match msg_type {
        "system" => {
            // {"type":"system","subtype":"init","session_id":"..."}
            // {"type":"system","subtype":"task_notification","task_id":"...","status":"...","summary":"..."}
            let subtype = json.get("subtype").and_then(|v| v.as_str())?;
            match subtype {
                "init" => {
                    let session_id = json.get("session_id")?.as_str()?.to_string();
                    Some(StreamMessage::Init { session_id })
                }
                "task_notification" => {
                    let task_id = json
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let status = json
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let summary = json
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(StreamMessage::TaskNotification {
                        task_id,
                        status,
                        summary,
                    })
                }
                _ => None,
            }
        }
        "assistant" => {
            // {"type":"assistant","message":{"content":[{"type":"text","text":"..."}]}}
            // or {"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{...}}]}}
            let content = json.get("message")?.get("content")?.as_array()?;

            for item in content {
                let item_type = item.get("type")?.as_str()?;
                match item_type {
                    "text" => {
                        let text = item.get("text")?.as_str()?.to_string();
                        return Some(StreamMessage::Text { content: text });
                    }
                    "tool_use" => {
                        let name = item.get("name")?.as_str()?.to_string();
                        let input = item
                            .get("input")
                            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                            .unwrap_or_default();
                        return Some(StreamMessage::ToolUse { name, input });
                    }
                    _ => {}
                }
            }
            None
        }
        "user" => {
            // {"type":"user","message":{"content":[{"type":"tool_result","content":"..." or [array]}]}}
            let content = json.get("message")?.get("content")?.as_array()?;

            for item in content {
                let item_type = item.get("type")?.as_str()?;
                if item_type == "tool_result" {
                    // content can be a string or an array of text items
                    let content_text = if let Some(s) = item.get("content").and_then(|v| v.as_str())
                    {
                        s.to_string()
                    } else if let Some(arr) = item.get("content").and_then(|v| v.as_array()) {
                        // Extract text from array: [{"type":"text","text":"..."},...]
                        arr.iter()
                            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        String::new()
                    };
                    let is_error = item
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    return Some(StreamMessage::ToolResult {
                        content: content_text,
                        is_error,
                    });
                }
            }
            None
        }
        "result" => {
            // {"type":"result","subtype":"error_during_execution","is_error":true,"errors":["..."]}
            // {"type":"result","subtype":"success","result":"...","session_id":"..."}
            let is_error = json
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_error {
                let errors_raw = json.get("errors");
                let result_raw = json.get("result").and_then(|v| v.as_str());
                // Try "errors" array first, then fall back to "result" field
                let error_msg = errors_raw
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .or_else(|| result_raw.map(|s| s.to_string()))
                    .unwrap_or_else(|| "Unknown error".to_string());
                return Some(StreamMessage::Error {
                    message: error_msg,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
            let result = json
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let session_id = json
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(StreamMessage::Done { result, session_id })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_temp_files_are_unique_no_clobber_and_raii_cleaned() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("private");
        std::fs::create_dir(&dir).unwrap();
        let existing = dir.join("system_prompt_existing");
        std::fs::write(&existing, b"must survive").unwrap();

        let dir = std::sync::Arc::new(dir);
        let workers: Vec<_> = (0..16)
            .map(|index| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    create_private_temp_file(
                        &dir,
                        "system_prompt",
                        format!("prompt-{index}").as_bytes(),
                    )
                    .unwrap()
                })
            })
            .collect();
        let guards: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        let paths: std::collections::HashSet<_> = guards
            .iter()
            .map(|guard| guard.path().to_path_buf())
            .collect();
        assert_eq!(paths.len(), guards.len());
        assert_eq!(std::fs::read(&existing).unwrap(), b"must survive");
        #[cfg(unix)]
        for path in &paths {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(guards);
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn private_temp_guard_never_deletes_a_replacement_file() {
        let temp = tempfile::tempdir().unwrap();
        let guard = create_private_temp_file(temp.path(), "prompt", b"owned").unwrap();
        let path = guard.path().to_path_buf();
        let original = temp.path().join("moved-owned-file");
        std::fs::rename(&path, &original).unwrap();
        std::fs::write(&path, b"replacement").unwrap();

        drop(guard);

        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        assert_eq!(std::fs::read(&original).unwrap(), b"owned");
    }

    #[cfg(unix)]
    #[test]
    fn private_temp_directory_symlink_is_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&real, &link).unwrap();
        assert!(create_private_temp_file(&link, "prompt", b"secret").is_err());
        assert!(std::fs::read_dir(real).unwrap().next().is_none());
        assert_eq!(
            std::fs::metadata(temp.path().join("real"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_runnable_override_falls_through_to_runnable_candidate() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("create temp dir");
        let override_directory = dir.path().join("not-a-program");
        let fallback = dir.path().join("claude");
        std::fs::create_dir(&override_directory).expect("create invalid override directory");
        std::fs::write(&fallback, b"#!/bin/sh\nexit 0\n").expect("write fallback");
        std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o755))
            .expect("make fallback executable");

        let selected =
            runnable_claude_candidate(Some(override_directory.to_string_lossy().into_owned()))
                .or_else(|| {
                    runnable_claude_candidate(Some(fallback.to_string_lossy().into_owned()))
                });

        assert_eq!(selected.as_deref(), fallback.to_str());
    }

    // ========== is_valid_session_id tests ==========

    #[test]
    fn test_session_id_valid() {
        assert!(is_valid_session_id("abc123"));
        assert!(is_valid_session_id("session-1"));
        assert!(is_valid_session_id("session_2"));
        assert!(is_valid_session_id("ABC-XYZ_123"));
        assert!(is_valid_session_id("a")); // Single char
    }

    #[test]
    fn test_session_id_empty_rejected() {
        assert!(!is_valid_session_id(""));
    }

    #[test]
    fn test_session_id_too_long_rejected() {
        // 64 characters should be valid
        let max_len = "a".repeat(64);
        assert!(is_valid_session_id(&max_len));

        // 65 characters should be rejected
        let too_long = "a".repeat(65);
        assert!(!is_valid_session_id(&too_long));
    }

    #[test]
    fn test_session_id_special_chars_rejected() {
        assert!(!is_valid_session_id("session;rm -rf"));
        assert!(!is_valid_session_id("session'OR'1=1"));
        assert!(!is_valid_session_id("session`cmd`"));
        assert!(!is_valid_session_id("session$(cmd)"));
        assert!(!is_valid_session_id("session\nline2"));
        assert!(!is_valid_session_id("session\0null"));
        assert!(!is_valid_session_id("path/traversal"));
        assert!(!is_valid_session_id("session with space"));
        assert!(!is_valid_session_id("session.dot"));
        assert!(!is_valid_session_id("session@email"));
    }

    #[test]
    fn test_session_id_unicode_rejected() {
        assert!(!is_valid_session_id("세션아이디"));
        assert!(!is_valid_session_id("session_日本語"));
        assert!(!is_valid_session_id("émoji🎉"));
    }

    #[test]
    fn test_session_id_argparse_injection_rejected() {
        // A leading `-` would be parsed as a CLI flag when spliced into argv.
        assert!(!is_valid_session_id("-i"));
        assert!(!is_valid_session_id("--help"));
        assert!(!is_valid_session_id("--config"));
        assert!(!is_valid_session_id("--version"));
        assert!(!is_valid_session_id("-"));
        // Leading underscore is fine for argparsers (kept for back-compat).
        assert!(is_valid_session_id("_internal"));
    }

    // ========== ClaudeResponse tests ==========

    #[test]
    fn test_claude_response_struct() {
        let response = ClaudeResponse {
            success: true,
            response: Some("Hello".to_string()),
            session_id: Some("abc123".to_string()),
            error: None,
        };

        assert!(response.success);
        assert_eq!(response.response, Some("Hello".to_string()));
        assert_eq!(response.session_id, Some("abc123".to_string()));
        assert!(response.error.is_none());
    }

    #[test]
    fn test_claude_response_error() {
        let response = ClaudeResponse {
            success: false,
            response: None,
            session_id: None,
            error: Some("Connection failed".to_string()),
        };

        assert!(!response.success);
        assert!(response.response.is_none());
        assert_eq!(response.error, Some("Connection failed".to_string()));
    }

    // ========== parse_claude_output tests ==========

    #[test]
    fn test_parse_claude_output_json_result() {
        let output = r#"{"session_id": "test-123", "result": "Hello, world!"}"#;
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(response.response, Some("Hello, world!".to_string()));
        assert_eq!(response.session_id, Some("test-123".to_string()));
    }

    #[test]
    fn test_parse_claude_output_json_message() {
        let output = r#"{"session_id": "sess-456", "message": "This is a message"}"#;
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(response.response, Some("This is a message".to_string()));
    }

    #[test]
    fn test_parse_claude_output_plain_text() {
        let output = "Just plain text response";
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(
            response.response,
            Some("Just plain text response".to_string())
        );
    }

    #[test]
    fn test_parse_claude_output_multiline() {
        let output = r#"{"session_id": "s1"}
{"result": "Final result"}"#;
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(response.session_id, Some("s1".to_string()));
        assert_eq!(response.response, Some("Final result".to_string()));
    }

    #[test]
    fn test_parse_claude_output_empty() {
        let output = "";
        let response = parse_claude_output(output);

        assert!(response.success);
        // Empty output should return empty response
        assert_eq!(response.response, Some("".to_string()));
    }

    // ========== is_ai_supported tests ==========

    #[test]
    fn test_is_ai_supported() {
        assert!(is_ai_supported());
    }

    // ========== session_id_regex tests ==========

    #[test]
    fn test_session_id_regex_caching() {
        // Multiple calls should return the same cached regex
        let regex1 = session_id_regex();
        let regex2 = session_id_regex();

        // Both should point to the same static instance
        assert!(std::ptr::eq(regex1, regex2));
    }

    // ========== parse_stream_message tests ==========

    #[test]
    fn test_parse_stream_message_init() {
        let json: Value =
            serde_json::from_str(r#"{"type":"system","subtype":"init","session_id":"test-123"}"#)
                .unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::Init { session_id }) => {
                assert_eq!(session_id, "test-123");
            }
            _ => panic!("Expected Init message"),
        }
    }

    #[test]
    fn test_parse_stream_message_text() {
        let json: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world"}]}}"#,
        )
        .unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::Text { content }) => {
                assert_eq!(content, "Hello world");
            }
            _ => panic!("Expected Text message"),
        }
    }

    #[test]
    fn test_parse_stream_message_tool_use() {
        let json: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#
        ).unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::ToolUse { name, input }) => {
                assert_eq!(name, "Bash");
                assert!(input.contains("ls"));
            }
            _ => panic!("Expected ToolUse message"),
        }
    }

    #[test]
    fn test_parse_stream_message_tool_result() {
        let json: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"file.txt","is_error":false}]}}"#
        ).unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::ToolResult { content, is_error }) => {
                assert_eq!(content, "file.txt");
                assert!(!is_error);
            }
            _ => panic!("Expected ToolResult message"),
        }
    }

    #[test]
    fn test_parse_stream_message_tool_result_error() {
        let json: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"Error: not found","is_error":true}]}}"#
        ).unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::ToolResult { content, is_error }) => {
                assert_eq!(content, "Error: not found");
                assert!(is_error);
            }
            _ => panic!("Expected ToolResult message with error"),
        }
    }

    #[test]
    fn test_parse_stream_message_result() {
        let json: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success","result":"Done!","session_id":"sess-456"}"#,
        )
        .unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::Done { result, session_id }) => {
                assert_eq!(result, "Done!");
                assert_eq!(session_id, Some("sess-456".to_string()));
            }
            _ => panic!("Expected Done message"),
        }
    }

    #[test]
    fn test_parse_stream_message_unknown_type() {
        let json: Value = serde_json::from_str(r#"{"type":"unknown","data":"something"}"#).unwrap();

        let msg = parse_stream_message(&json);
        assert!(msg.is_none());
    }
}
