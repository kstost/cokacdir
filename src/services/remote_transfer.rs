use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};

use russh::{client, ChannelMsg, Disconnect};
use tokio::runtime::Runtime;

use crate::services::file_ops::ProgressMessage;
use crate::services::remote::{expand_tilde, RemoteAuth, RemoteProfile, SshHandler};

/// Transfer direction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    LocalToRemote,
    RemoteToLocal,
}

/// Transfer configuration
#[derive(Debug, Clone)]
pub struct TransferConfig {
    pub direction: TransferDirection,
    pub profile: RemoteProfile,
    pub source_files: Vec<PathBuf>,
    pub source_base: String,
    pub target_path: String,
}

/// Check if rsync is available
fn has_rsync() -> bool {
    Command::new("rsync")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn spawn_cancel_watchdog(
    cancel_flag: Arc<AtomicBool>,
    pid: u32,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let done = Arc::new(AtomicBool::new(false));
    let done_for_thread = done.clone();
    let token = Arc::new(crate::services::claude::CancelToken::new());
    {
        let mut guard = token.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(pid);
    }
    let handle = std::thread::spawn(move || {
        while !done_for_thread.load(Ordering::Relaxed) {
            if cancel_flag.load(Ordering::Relaxed) {
                token.cancel_now();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    (done, handle)
}

/// Check if two remote profiles refer to the same server
fn is_same_server(a: &RemoteProfile, b: &RemoteProfile) -> bool {
    a.host == b.host && a.port == b.port && a.user == b.user
}

/// SSH command executor using russh library (no external ssh process needed).
/// Connects once, executes multiple commands on the same connection,
/// and disconnects automatically on drop.
struct SshExec {
    runtime: Runtime,
    handle: client::Handle<SshHandler>,
}

impl SshExec {
    /// Connect to remote server via russh and authenticate.
    fn connect(profile: &RemoteProfile) -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| format!("Failed to create runtime: {}", e))?;

        let profile = profile.clone();
        let handle = runtime.block_on(async {
            let config = client::Config {
                // Same-server cp/mv/rm commands can be silent for a long time
                // while still making progress on the remote host.
                inactivity_timeout: Some(std::time::Duration::from_secs(24 * 60 * 60)),
                keepalive_interval: Some(std::time::Duration::from_secs(30)),
                keepalive_max: 3,
                ..Default::default()
            };

            let mut ssh = client::connect(
                Arc::new(config),
                (profile.host.as_str(), profile.port),
                SshHandler::new(&profile),
            )
            .await
            .map_err(|e| crate::services::remote::format_ssh_connect_error(&e))?;

            // Authenticate
            let auth_result = match &profile.auth {
                RemoteAuth::Password { password } => ssh
                    .authenticate_password(&profile.user, password)
                    .await
                    .map_err(|e| format!("Password auth failed: {}", e))?,
                RemoteAuth::KeyFile { path, passphrase } => {
                    let key_path = expand_tilde(path);

                    let key_pair = if let Some(pass) = passphrase {
                        russh_keys::load_secret_key(&key_path, Some(pass))
                            .map_err(|e| format!("Failed to load key: {}", e))?
                    } else {
                        russh_keys::load_secret_key(&key_path, None)
                            .map_err(|e| format!("Failed to load key: {}", e))?
                    };

                    ssh.authenticate_publickey(&profile.user, Arc::new(key_pair))
                        .await
                        .map_err(|e| format!("Key auth failed: {}", e))?
                }
            };

            if !auth_result {
                return Err("Authentication rejected by server".to_string());
            }

            Ok(ssh)
        })?;

        Ok(Self { runtime, handle })
    }

    /// Execute a command on the remote server.
    /// Returns (success, stderr_string).
    fn exec(&self, cmd: &str) -> Result<(bool, String), String> {
        self.exec_cancelable(cmd, None)
    }

    /// Execute a command on the remote server, optionally aborting the SSH
    /// session when the caller cancels a long silent command such as cp/mv/rm.
    /// Returns (success, stderr_string).
    fn exec_cancelable(
        &self,
        cmd: &str,
        cancel_flag: Option<&Arc<AtomicBool>>,
    ) -> Result<(bool, String), String> {
        let cmd = cmd.to_string();
        self.runtime.block_on(async {
            let mut channel = self
                .handle
                .channel_open_session()
                .await
                .map_err(|e| format!("Failed to open channel: {}", e))?;

            channel
                .exec(true, cmd)
                .await
                .map_err(|e| format!("Failed to exec command: {}", e))?;

            let mut stderr_bytes = Vec::new();
            let mut exit_status: Option<u32> = None;

            loop {
                if cancel_flag
                    .map(|flag| flag.load(Ordering::Relaxed))
                    .unwrap_or(false)
                {
                    let _ = self
                        .handle
                        .disconnect(Disconnect::ByApplication, "cancelled", "en")
                        .await;
                    return Err("Cancelled".to_string());
                }

                let msg = match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    channel.wait(),
                )
                .await
                {
                    Ok(Some(msg)) => msg,
                    Ok(None) => break,
                    Err(_) => continue,
                };

                match msg {
                    ChannelMsg::ExtendedData { data, ext } => {
                        if ext == 1 {
                            stderr_bytes.extend_from_slice(&data);
                        }
                    }
                    ChannelMsg::ExitStatus { exit_status: s } => {
                        exit_status = Some(s);
                    }
                    _ => {}
                }
            }

            let success = exit_status.map_or(false, |s| s == 0);
            let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();

            Ok((success, stderr))
        })
    }
}

impl Drop for SshExec {
    fn drop(&mut self) {
        let _ = self.runtime.block_on(async {
            self.handle
                .disconnect(Disconnect::ByApplication, "", "en")
                .await
        });
    }
}

/// Check if sshpass is available
fn has_sshpass() -> bool {
    Command::new("sshpass")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build SSH command option string for rsync
fn build_ssh_option(profile: &RemoteProfile) -> String {
    let mut ssh_cmd = String::from("ssh");

    // Port
    if profile.port != 22 {
        ssh_cmd.push_str(&format!(" -p {}", profile.port));
    }

    // Key file
    if let RemoteAuth::KeyFile { ref path, .. } = profile.auth {
        let expanded = expand_tilde(path).display().to_string();
        let escaped = expanded.replace('\'', "'\\''");
        ssh_cmd.push_str(&format!(" -i '{}'", escaped));
    }

    // Learn first-seen host keys, but reject changed keys.
    #[cfg(unix)]
    ssh_cmd.push_str(" -o StrictHostKeyChecking=accept-new -o LogLevel=ERROR");
    #[cfg(windows)]
    ssh_cmd.push_str(" -o StrictHostKeyChecking=accept-new -o LogLevel=ERROR");

    ssh_cmd
}

/// Parse a (major, minor, patch) version tuple from `rsync --version` output.
/// Looks for the "version X.Y[.Z]" token on the first line.
fn parse_rsync_version(output: &str) -> Option<(u32, u32, u32)> {
    let first_line = output.lines().next()?;
    // Only trust upstream rsync ("rsync  version X.Y.Z  protocol version N"). Apple's
    // openrsync prints "openrsync: protocol version 29", which would otherwise parse as
    // (29,0,0) and wrongly count as modern-arg-protection — openrsync does NOT escape
    // remote args, so the quote-wrapping must be kept there.
    if !first_line.starts_with("rsync") {
        return None;
    }
    let after = first_line.split("version ").nth(1)?;
    let token = after.split_whitespace().next()?;
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Whether the local rsync (>= 3.2.4) applies "modern argument protection", i.e. it
/// backslash-escapes shell-active characters in remote args itself. On such versions our
/// own single-quote wrapping arrives on the remote as *literal* quote characters in the
/// path and breaks the transfer, so we must NOT add quotes. On older rsync the quoting is
/// still the shell-interpretation defense and must be kept. Detected once and cached;
/// parse failures fall back to the conservative (quote) behavior.
fn rsync_uses_modern_arg_protection() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        Command::new("rsync")
            .arg("--version")
            .output()
            .ok()
            .and_then(|out| parse_rsync_version(&String::from_utf8_lossy(&out.stdout)))
            .map(|ver| ver >= (3, 2, 4))
            .unwrap_or(false)
    })
}

/// Build remote path string for rsync: user@host:/path
fn build_remote_spec(profile: &RemoteProfile, path: &str) -> String {
    if rsync_uses_modern_arg_protection() {
        // Modern rsync escapes remote args itself; adding our own quotes would make them
        // literal characters in the path. Pass the path verbatim.
        format!("{}@{}:{}", profile.user, profile.host, path)
    } else {
        // Older rsync passes the arg through the remote shell: single-quote to prevent
        // shell interpretation. Only single quotes inside the path need escaping: ' → '\''
        let escaped = path.replace('\'', "'\\''");
        format!("{}@{}:'{}'", profile.user, profile.host, escaped)
    }
}

/// RAII guard for the temporary SSH_ASKPASS script: removes the file on drop
/// so the password leak window survives panics and early returns.
struct AskpassGuard {
    path: PathBuf,
}

impl AskpassGuard {
    fn new(password: &str) -> Result<Self, String> {
        Ok(Self {
            path: create_askpass_script(password)?,
        })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for AskpassGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Create a temporary SSH_ASKPASS script for password authentication.
/// Prefer `AskpassGuard::new` over calling this directly; the guard ensures
/// the script is removed even on panic / early return.
fn create_askpass_script(password: &str) -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "Failed to get home directory".to_string())?;
    let tmp_dir = home.join(".cokacdir").join("tmp");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("Failed to create tmp dir: {}", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("Failed to set tmp dir permissions: {}", e))?;
    }

    // Random suffix prevents collision when the same process spawns multiple
    // transfers concurrently (single-PID file would race).
    let nonce: u32 = rand::random();
    #[cfg(unix)]
    let script_path = tmp_dir.join(format!("askpass_{}_{:08x}", std::process::id(), nonce));
    #[cfg(windows)]
    let script_path = tmp_dir.join(format!("askpass_{}_{:08x}.bat", std::process::id(), nonce));

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Escape single quotes in password: replace ' with '\''
        let escaped = password.replace('\'', "'\\''");
        let content = format!("#!/bin/sh\necho '{}'\n", escaped);

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&script_path)
            .map_err(|e| format!("Failed to create askpass script: {}", e))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("Failed to write askpass script: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("Failed to sync askpass script: {}", e))?;

        Ok(script_path)
    }

    #[cfg(windows)]
    {
        let _ = password;
        let _ = script_path;
        Err("Password rsync fallback on Windows would require writing a plaintext askpass script; install sshpass or use key authentication.".to_string())
    }
}

/// Transfer files using rsync with progress reporting.
/// Uses --progress flag (compatible with GNU rsync and openrsync/macOS).
/// For password auth: tries sshpass first, falls back to SSH_ASKPASS mechanism.
fn transfer_rsync(
    config: &TransferConfig,
    cancel_flag: &Arc<AtomicBool>,
    tx: &Sender<ProgressMessage>,
) -> Result<(), String> {
    let ssh_option = build_ssh_option(&config.profile);
    let total_files = config.source_files.len();
    let mut completed_files: usize = 0;

    // Prepare password auth mechanism
    let needs_password = matches!(&config.profile.auth, RemoteAuth::Password { .. });
    let use_sshpass = needs_password && has_sshpass();
    let askpass_script: Option<AskpassGuard> = if needs_password && !use_sshpass {
        if let RemoteAuth::Password { ref password } = config.profile.auth {
            Some(AskpassGuard::new(password)?)
        } else {
            None
        }
    } else {
        None
    };

    for source_file in &config.source_files {
        if cancel_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        let file_name = source_file.display().to_string();
        let _ = tx.send(ProgressMessage::FileStarted(file_name.clone()));

        let source_full = format!(
            "{}/{}",
            config.source_base.trim_end_matches('/'),
            source_file.display()
        );
        let target = &config.target_path;

        let (src, dst) = match config.direction {
            TransferDirection::LocalToRemote => {
                (source_full, build_remote_spec(&config.profile, target))
            }
            TransferDirection::RemoteToLocal => (
                build_remote_spec(&config.profile, &source_full),
                target.clone(),
            ),
        };

        // Build rsync command with --progress (compatible with all rsync versions)
        let mut cmd = Command::new("rsync");
        cmd.arg("-a")
            .arg("--progress")
            .arg("-e")
            .arg(&ssh_option)
            .arg(&src)
            .arg(&dst)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Apply password auth
        let mut cmd = if use_sshpass {
            if let RemoteAuth::Password { ref password } = config.profile.auth {
                let mut sshpass_cmd = Command::new("sshpass");
                // Pass the password via the SSHPASS env var (-e) instead of -p <pw>, so it
                // does not appear in the process argument list (visible in `ps` / auditd).
                sshpass_cmd.arg("-e").env("SSHPASS", password);
                let program = cmd.get_program().to_string_lossy().to_string();
                let args: Vec<String> = cmd
                    .get_args()
                    .map(|a| a.to_string_lossy().to_string())
                    .collect();
                sshpass_cmd.arg(program);
                for arg in args {
                    sshpass_cmd.arg(arg);
                }
                sshpass_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
                sshpass_cmd
            } else {
                cmd
            }
        } else if let Some(ref guard) = askpass_script {
            cmd.env("SSH_ASKPASS", guard.path())
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env("DISPLAY", ":0")
                .stdin(Stdio::null());
            cmd
        } else {
            cmd
        };

        // Place rsync in its own process group so kill_child_tree's
        // group-targeted SIGKILL stays scoped to rsync (and any sshpass
        // wrapper) — and never touches the cokacdir TUI process itself.
        crate::services::claude::detach_into_own_pgroup(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to start rsync: {}", e))?;
        let (cancel_watch_done, cancel_watch) =
            spawn_cancel_watchdog(cancel_flag.clone(), child.id());
        let mut stderr_thread = child.stderr.take().map(|stderr| {
            std::thread::spawn(move || {
                let mut buf = String::new();
                let mut stderr = stderr;
                let _ = std::io::Read::read_to_string(&mut stderr, &mut buf);
                buf
            })
        });

        // Parse rsync progress output.
        // rsync --progress uses \r (carriage return) to update progress in-place,
        // so we read byte-by-byte and split on both \r and \n.
        if let Some(stdout) = child.stdout.take() {
            let mut reader = BufReader::new(stdout);
            let mut line_buf = Vec::new();
            let mut byte_buf = [0u8; 1];
            loop {
                if cancel_flag.load(Ordering::Relaxed) {
                    crate::services::claude::kill_child_tree(&mut child);
                    let _ = child.wait();
                    cancel_watch_done.store(true, Ordering::Relaxed);
                    let _ = cancel_watch.join();
                    if let Some(handle) = stderr_thread.take() {
                        let _ = handle.join();
                    }
                    return Ok(());
                }

                match std::io::Read::read(&mut reader, &mut byte_buf) {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let b = byte_buf[0];
                        if b == b'\r' || b == b'\n' {
                            if !line_buf.is_empty() {
                                let line = String::from_utf8_lossy(&line_buf).to_string();
                                if let Some(progress) = parse_rsync_progress(&line) {
                                    let _ = tx.send(ProgressMessage::FileProgress(
                                        progress.0, progress.1,
                                    ));
                                }
                                line_buf.clear();
                            }
                        } else {
                            line_buf.push(b);
                        }
                    }
                    Err(_) => break,
                }
            }
            // Process remaining data in buffer
            if !line_buf.is_empty() {
                let line = String::from_utf8_lossy(&line_buf).to_string();
                if let Some(progress) = parse_rsync_progress(&line) {
                    let _ = tx.send(ProgressMessage::FileProgress(progress.0, progress.1));
                }
            }
        }

        let status = match child.wait() {
            Ok(status) => status,
            Err(e) => {
                cancel_watch_done.store(true, Ordering::Relaxed);
                let _ = cancel_watch.join();
                if let Some(handle) = stderr_thread.take() {
                    let _ = handle.join();
                }
                if cancel_flag.load(Ordering::Relaxed) {
                    return Ok(());
                }
                return Err(format!("rsync wait failed: {}", e));
            }
        };
        cancel_watch_done.store(true, Ordering::Relaxed);
        let _ = cancel_watch.join();
        let stderr_output = stderr_thread
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();

        if cancel_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        if status.success() {
            completed_files += 1;
            let _ = tx.send(ProgressMessage::FileCompleted(file_name));
            let _ = tx.send(ProgressMessage::TotalProgress(
                completed_files,
                total_files,
                0,
                0,
            ));
        } else {
            let stderr_msg = Some(stderr_output)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    format!("rsync exited with code {}", status.code().unwrap_or(-1))
                });
            let _ = tx.send(ProgressMessage::Error(file_name, stderr_msg.clone()));
            return Err(stderr_msg);
        }
    }

    Ok(())
}

/// Parse rsync --progress output line.
/// Format: "  1,234,567  42%  1.23MB/s  0:01:23"
/// Returns (transferred_bytes, total_bytes) if parseable.
fn parse_rsync_progress(line: &str) -> Option<(u64, u64)> {
    let trimmed = line.trim();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() >= 2 {
        // First part: bytes transferred (with commas)
        let bytes_str = parts[0].replace(',', "");
        let transferred: u64 = bytes_str.parse().ok()?;

        // Second part: percentage
        let pct_str = parts[1].trim_end_matches('%');
        let pct: u64 = pct_str.parse().ok()?;

        if pct > 0 {
            let total = transferred * 100 / pct;
            return Some((transferred, total));
        } else if transferred > 0 {
            return Some((0, transferred));
        }
    }
    None
}

/// Delete source files after a successful cut (move) transfer.
/// If `source_profile` is Some, the source is remote (delete via SSH rm -rf).
/// If `source_profile` is None, the source is local (delete via std::fs).
/// `cancel_flag` lets the remote `rm -rf` be aborted while it is still
/// running, matching the rest of the transfer pipeline (which raised the
/// SSH inactivity timeout to 24h and therefore can no longer rely on a
/// short timeout to break out of a hung deletion).
fn delete_source_files_after_cut(
    source_files: &[PathBuf],
    source_base: &str,
    source_profile: Option<&RemoteProfile>,
    cancel_flag: &Arc<AtomicBool>,
    tx: &Sender<ProgressMessage>,
) {
    match source_profile {
        Some(profile) => {
            delete_remote_source_files(profile, source_files, source_base, cancel_flag, tx)
        }
        None => delete_local_source_files(source_files, source_base, tx),
    }
}

/// Delete local source files after cut
fn delete_local_source_files(
    source_files: &[PathBuf],
    source_base: &str,
    tx: &Sender<ProgressMessage>,
) {
    for source_file in source_files {
        let full_path = PathBuf::from(source_base).join(source_file);
        let result = if full_path.is_dir() {
            std::fs::remove_dir_all(&full_path)
        } else {
            std::fs::remove_file(&full_path)
        };
        if let Err(e) = result {
            let _ = tx.send(ProgressMessage::Error(
                source_file.display().to_string(),
                format!("Failed to delete source: {}", e),
            ));
        }
    }
}

/// Delete remote source files via russh SSH exec (rm -rf).
/// Honors `cancel_flag` so a slow `rm -rf` on a large tree can be aborted
/// via /stop. Without this, the 24h SSH inactivity timeout would let a
/// silent rm hold the caller hostage for a full day before unsticking.
fn delete_remote_source_files(
    profile: &RemoteProfile,
    source_files: &[PathBuf],
    source_base: &str,
    cancel_flag: &Arc<AtomicBool>,
    tx: &Sender<ProgressMessage>,
) {
    // Build rm -rf paths
    let mut rm_paths = Vec::new();
    for source_file in source_files {
        let full_path = format!(
            "{}/{}",
            source_base.trim_end_matches('/'),
            source_file.display()
        );
        let escaped = full_path.replace('\'', "'\\''");
        rm_paths.push(format!("'{}'", escaped));
    }

    if rm_paths.is_empty() {
        return;
    }

    // Already-cancelled: skip the SSH connect entirely. The transfer phase
    // already finished — there is nothing further the user is waiting on.
    if cancel_flag.load(Ordering::Relaxed) {
        return;
    }

    let rm_cmd = format!("rm -rf {}", rm_paths.join(" "));

    let ssh = match SshExec::connect(profile) {
        Ok(s) => s,
        Err(e) => {
            if cancel_flag.load(Ordering::Relaxed) {
                return;
            }
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Failed to connect for source deletion: {}", e),
            ));
            return;
        }
    };

    match ssh.exec_cancelable(&rm_cmd, Some(cancel_flag)) {
        Ok((success, stderr)) => {
            if !success {
                let _ = tx.send(ProgressMessage::Error(
                    String::new(),
                    format!("Failed to delete remote source: {}", stderr.trim()),
                ));
            }
        }
        // Cancellation surfaces as Err from exec_cancelable; suppress the
        // error toast because the user asked for the stop, they did not hit
        // an SSH failure.
        Err(_) if cancel_flag.load(Ordering::Relaxed) => {}
        Err(e) => {
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Failed to run SSH for source deletion: {}", e),
            ));
        }
    }
}

/// Main transfer function — always uses rsync
/// When `is_cut` is true, source files are deleted after successful transfer.
/// `source_profile` is needed when the source is remote (for SSH rm deletion).
pub fn transfer_files_with_progress(
    config: TransferConfig,
    cancel_flag: Arc<AtomicBool>,
    tx: Sender<ProgressMessage>,
    is_cut: bool,
    source_profile: Option<RemoteProfile>,
) {
    let total_files = config.source_files.len();

    let _ = tx.send(ProgressMessage::Preparing(format!(
        "Transferring {} file(s)...",
        total_files
    )));

    if !has_rsync() {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            "rsync is not installed. Please install rsync to transfer files.".to_string(),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }

    let _ = tx.send(ProgressMessage::PrepareComplete);
    let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

    let result = transfer_rsync(&config, &cancel_flag, &tx);

    match result {
        Ok(_) => {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Completed(0, 0));
            } else {
                // Delete source files if this is a cut (move) operation
                if is_cut {
                    delete_source_files_after_cut(
                        &config.source_files,
                        &config.source_base,
                        source_profile.as_ref(),
                        &cancel_flag,
                        &tx,
                    );
                }
                let _ = tx.send(ProgressMessage::Completed(total_files, 0));
            }
        }
        Err(ref msg) => {
            let _ = tx.send(ProgressMessage::Error(String::new(), msg.clone()));
            let _ = tx.send(ProgressMessage::Completed(0, total_files));
        }
    }
}

/// Transfer files within the same remote server using cp -a (copy) or mv (move) via russh SSH exec.
fn transfer_same_server(
    profile: &RemoteProfile,
    source_files: &[PathBuf],
    source_base: &str,
    target_path: &str,
    cancel_flag: &Arc<AtomicBool>,
    tx: &Sender<ProgressMessage>,
    is_cut: bool,
) -> Result<(), String> {
    let total_files = source_files.len();
    let mut completed_files: usize = 0;

    let ssh = SshExec::connect(profile)?;

    for source_file in source_files {
        if cancel_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        let file_name = source_file.display().to_string();
        let _ = tx.send(ProgressMessage::FileStarted(file_name.clone()));

        let source_full = format!(
            "{}/{}",
            source_base.trim_end_matches('/'),
            source_file.display()
        );
        let escaped_src = source_full.replace('\'', "'\\''");
        let escaped_dst = target_path.replace('\'', "'\\''");

        let remote_cmd = if is_cut {
            format!("mv '{}' '{}'", escaped_src, escaped_dst)
        } else {
            format!("cp -a '{}' '{}'", escaped_src, escaped_dst)
        };

        let (success, stderr) = match ssh.exec_cancelable(&remote_cmd, Some(cancel_flag)) {
            Ok(result) => result,
            Err(_) if cancel_flag.load(Ordering::Relaxed) => return Ok(()),
            Err(e) => return Err(e),
        };

        if success {
            completed_files += 1;
            let _ = tx.send(ProgressMessage::FileCompleted(file_name));
            let _ = tx.send(ProgressMessage::TotalProgress(
                completed_files,
                total_files,
                0,
                0,
            ));
        } else {
            let err_msg = format!(
                "Failed to {} '{}': {}",
                if is_cut { "move" } else { "copy" },
                file_name,
                stderr.trim()
            );
            let _ = tx.send(ProgressMessage::Error(file_name, err_msg.clone()));
            return Err(err_msg);
        }
    }

    Ok(())
}

/// Transfer files between two remote servers via local temp directory
/// Phase 1: Download from source remote to local temp
/// Phase 2: Upload from local temp to target remote
/// When `is_cut` is true, source files are deleted from source remote after successful upload.
pub fn transfer_remote_to_remote_with_progress(
    source_profile: RemoteProfile,
    target_profile: RemoteProfile,
    source_files: Vec<PathBuf>,
    source_base: String,
    target_path: String,
    cancel_flag: Arc<AtomicBool>,
    tx: Sender<ProgressMessage>,
    is_cut: bool,
) {
    let total_files = source_files.len();

    let _ = tx.send(ProgressMessage::Preparing(format!(
        "Transferring {} file(s) between remote servers...",
        total_files
    )));

    // Same server optimization: use cp -a / mv directly via SSH
    if is_same_server(&source_profile, &target_profile) {
        let _ = tx.send(ProgressMessage::PrepareComplete);
        let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

        let result = transfer_same_server(
            &source_profile,
            &source_files,
            &source_base,
            &target_path,
            &cancel_flag,
            &tx,
            is_cut,
        );

        match result {
            Ok(_) => {
                if cancel_flag.load(Ordering::Relaxed) {
                    let _ = tx.send(ProgressMessage::Completed(0, 0));
                } else {
                    let _ = tx.send(ProgressMessage::Completed(total_files, 0));
                }
            }
            Err(ref msg) => {
                let _ = tx.send(ProgressMessage::Error(String::new(), msg.clone()));
                let _ = tx.send(ProgressMessage::Completed(0, total_files));
            }
        }
        return;
    }

    if !has_rsync() {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            "rsync is not installed. Please install rsync to transfer files.".to_string(),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }

    // Create temp directory under ~/.cokacdir/tmp/
    let temp_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let Some(home) = dirs::home_dir() else {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            "Failed to get home directory".to_string(),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    };
    let base_tmp = home.join(".cokacdir").join("tmp");
    let temp_dir = base_tmp.join(format!("r2r_{}", temp_id));
    if let Err(e) = std::fs::create_dir_all(&temp_dir) {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            format!("Failed to create temp dir: {}", e),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }

    let _ = tx.send(ProgressMessage::PrepareComplete);
    let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

    // Phase 1: Download from source remote to local temp
    let download_config = TransferConfig {
        direction: TransferDirection::RemoteToLocal,
        profile: source_profile.clone(),
        source_files: source_files.clone(),
        source_base: source_base.clone(),
        target_path: temp_dir.display().to_string(),
    };

    let dl_result = transfer_rsync(&download_config, &cancel_flag, &tx);

    if let Err(ref msg) = dl_result {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            format!("Download failed: {}", msg),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        let _ = std::fs::remove_dir_all(&temp_dir);
        return;
    }

    if cancel_flag.load(Ordering::Relaxed) {
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = tx.send(ProgressMessage::Completed(0, 0));
        return;
    }

    // Phase 2: Upload from local temp to target remote
    // Reset progress counters so progress bar starts from 0% again
    let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

    let upload_config = TransferConfig {
        direction: TransferDirection::LocalToRemote,
        profile: target_profile,
        source_files: source_files.clone(),
        source_base: temp_dir.display().to_string(),
        target_path,
    };

    let ul_result = transfer_rsync(&upload_config, &cancel_flag, &tx);

    // Cleanup temp directory
    let _ = std::fs::remove_dir_all(&temp_dir);

    match ul_result {
        Ok(_) => {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Completed(0, 0));
            } else {
                // Delete source files from source remote if this is a cut
                if is_cut {
                    delete_source_files_after_cut(
                        &source_files,
                        &source_base,
                        Some(&source_profile),
                        &cancel_flag,
                        &tx,
                    );
                }
                let _ = tx.send(ProgressMessage::Completed(total_files, 0));
            }
        }
        Err(ref msg) => {
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Upload failed: {}", msg),
            ));
            let _ = tx.send(ProgressMessage::Completed(0, total_files));
        }
    }
}
