use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::runtime::Runtime;

use russh::keys::*;
use russh::*;
use russh_sftp::client::SftpSession as RusshSftpSession;

// Obfuscation key for password storage (NOT real encryption — prevents casual viewing only)
const OBFUSCATION_KEY: &[u8] = b"cokacdir_remote_v1_key";

/// Expand a leading "~/" or bare "~" to the user's home directory.
/// `~user/...` is intentionally NOT expanded — we cannot safely resolve another
/// user's home, and silently rewriting it to `$HOME/user/...` produces a wrong
/// path that the SSH layer would still try to read.
pub(crate) fn expand_tilde(path: &str) -> std::path::PathBuf {
    use std::path::PathBuf;
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// RAII guard that removes a partially-written file on drop unless `commit()`
/// is called. Used to ensure failed/cancelled downloads do not leave truncated
/// files behind for the user to mistake for a successful transfer.
///
/// The guard owns the open file handle so that on Drop the handle is released
/// *before* the unlink — Windows refuses to remove a file that still has an
/// open handle (sharing violation), which would otherwise leave the partial
/// file behind even though we asked the guard to clean it up.
struct PartialFileGuard {
    path: String,
    file: Option<std::fs::File>,
    keep: bool,
}

impl PartialFileGuard {
    fn create(path: String) -> std::io::Result<Self> {
        let file = std::fs::File::create(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            keep: false,
        })
    }

    fn writer(&mut self) -> &mut std::fs::File {
        self.file
            .as_mut()
            .expect("PartialFileGuard file handle already taken")
    }

    fn commit(mut self) {
        self.keep = true;
    }
}

impl Drop for PartialFileGuard {
    fn drop(&mut self) {
        // Release the file handle BEFORE attempting to unlink. On Windows an
        // open file cannot be removed; on Unix this is harmless.
        drop(self.file.take());
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Obfuscate a string for storage (XOR + base64, prefixed with "enc:")
pub fn obfuscate(plaintext: &str) -> String {
    let xored: Vec<u8> = plaintext
        .as_bytes()
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ OBFUSCATION_KEY[i % OBFUSCATION_KEY.len()])
        .collect();
    use base64::Engine;
    format!(
        "enc:{}",
        base64::engine::general_purpose::STANDARD.encode(&xored)
    )
}

/// Deobfuscate a stored string (reverse of obfuscate, with plaintext fallback)
pub fn deobfuscate(stored: &str) -> String {
    if let Some(encoded) = stored.strip_prefix("enc:") {
        use base64::Engine;
        if let Ok(xored) = base64::engine::general_purpose::STANDARD.decode(encoded) {
            let plain: Vec<u8> = xored
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ OBFUSCATION_KEY[i % OBFUSCATION_KEY.len()])
                .collect();
            return String::from_utf8(plain).unwrap_or_else(|_| stored.to_string());
        }
    }
    // Fallback: treat as plaintext (backward compatibility)
    stored.to_string()
}

mod obfuscated_string {
    use super::{deobfuscate, obfuscate};
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &str, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&obfuscate(value))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(deobfuscate(&s))
    }
}

mod obfuscated_option_string {
    use super::{deobfuscate, obfuscate};
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(v) => serializer.serialize_some(&obfuscate(v)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<String>::deserialize(deserializer)?;
        Ok(opt.map(|s| deobfuscate(&s)))
    }
}

/// Remote authentication method
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RemoteAuth {
    #[serde(rename = "password")]
    Password {
        #[serde(with = "obfuscated_string")]
        password: String,
    },
    #[serde(rename = "key_file")]
    KeyFile {
        path: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "obfuscated_option_string"
        )]
        passphrase: Option<String>,
    },
}

/// Remote server profile stored in settings.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteProfile {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    pub auth: RemoteAuth,
    #[serde(default)]
    pub default_path: String,
}

fn default_port() -> u16 {
    22
}

/// File entry from SFTP directory listing
#[derive(Debug, Clone)]
pub struct SftpFileEntry {
    pub name: String,
    pub is_directory: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: DateTime<Local>,
    pub permissions: String,
}

/// Connection status
#[derive(Debug, Clone)]
pub enum ConnectionStatus {
    Connected,
    Disconnected(String),
}

/// Remote context attached to a panel
pub struct RemoteContext {
    pub profile: RemoteProfile,
    pub session: SftpSession,
    pub status: ConnectionStatus,
}

impl std::fmt::Debug for RemoteContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteContext")
            .field("profile", &self.profile)
            .field("status", &self.status)
            .finish()
    }
}

/// SSH client handler for russh
pub(crate) struct SshHandler {
    host: String,
    port: u16,
}

impl SshHandler {
    pub(crate) fn new(profile: &RemoteProfile) -> Self {
        Self {
            host: profile.host.clone(),
            port: profile.port,
        }
    }
}

/// Format an SSH connect error, adding actionable guidance when the failure is a
/// changed host key (the security-rejection case the user must resolve by hand).
pub(crate) fn format_ssh_connect_error(e: &russh::Error) -> String {
    if let russh::Error::KeyChanged { line } = e {
        format!(
            "SSH connection failed: remote host key changed (known_hosts line {}). \
             If the server was legitimately reinstalled, remove that line from \
             ~/.ssh/known_hosts and reconnect.",
            line
        )
    } else {
        format!("SSH connection failed: {}", e)
    }
}

/// Path to the OpenSSH-compatible known_hosts file (`~/.ssh/known_hosts`).
/// russh-keys defaults to `~/ssh/known_hosts` (no dot) on Windows, which would
/// diverge from the ssh/rsync CLI used for transfers, so pass the path explicitly.
fn openssh_known_hosts_path() -> Result<std::path::PathBuf, russh::Error> {
    dirs::home_dir()
        .map(|home| home.join(".ssh").join("known_hosts"))
        .ok_or_else(|| russh::keys::Error::NoHomeDir.into())
}

#[async_trait::async_trait]
impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let known_hosts = openssh_known_hosts_path()?;
        match russh::keys::known_hosts::check_known_hosts_path(
            &self.host,
            self.port,
            server_public_key,
            &known_hosts,
        ) {
            Ok(true) => Ok(true),
            Ok(false) => {
                russh::keys::known_hosts::learn_known_hosts_path(
                    &self.host,
                    self.port,
                    server_public_key,
                    &known_hosts,
                )
                .map_err(|e| match e {
                    russh::keys::Error::KeyChanged { line } => russh::Error::KeyChanged { line },
                    other => other.into(),
                })?;
                Ok(true)
            }
            Err(russh::keys::Error::KeyChanged { line }) => Err(russh::Error::KeyChanged { line }),
            Err(e) => Err(e.into()),
        }
    }
}

/// SFTP session wrapper around russh
pub struct SftpSession {
    runtime: Runtime,
    ssh_handle: Option<client::Handle<SshHandler>>,
    sftp: Option<RusshSftpSession>,
}

impl std::fmt::Debug for SftpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpSession")
            .field("connected", &self.sftp.is_some())
            .finish()
    }
}

impl SftpSession {
    /// Connect to remote host via SSH and open SFTP channel
    pub fn connect(profile: &RemoteProfile) -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| format!("Failed to create runtime: {}", e))?;

        let profile = profile.clone();
        let (ssh_handle, sftp) = runtime.block_on(async { Self::connect_async(&profile).await })?;

        Ok(Self {
            runtime,
            ssh_handle: Some(ssh_handle),
            sftp: Some(sftp),
        })
    }

    async fn connect_async(
        profile: &RemoteProfile,
    ) -> Result<(client::Handle<SshHandler>, RusshSftpSession), String> {
        let config = client::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(300)),
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            keepalive_max: 3,
            ..Default::default()
        };

        let mut ssh = client::connect(
            Arc::new(config),
            (profile.host.as_str(), profile.port),
            SshHandler::new(profile),
        )
        .await
        .map_err(|e| format_ssh_connect_error(&e))?;

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

        // Open SFTP channel
        let channel = ssh
            .channel_open_session()
            .await
            .map_err(|e| format!("Failed to open channel: {}", e))?;

        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| format!("Failed to request SFTP subsystem: {}", e))?;

        let sftp = RusshSftpSession::new(channel.into_stream())
            .await
            .map_err(|e| format!("Failed to init SFTP session: {}", e))?;

        Ok((ssh, sftp))
    }

    /// Check if a remote directory exists
    pub fn dir_exists(&self, path: &str) -> bool {
        self.list_dir(path).is_ok()
    }

    /// List directory contents via SFTP
    pub fn list_dir(&self, path: &str) -> Result<Vec<SftpFileEntry>, String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let path = path.to_string();

        self.runtime.block_on(async {
            let dir = sftp
                .read_dir(&path)
                .await
                .map_err(|e| format!("Failed to read dir '{}': {}", path, e))?;

            let mut entries = Vec::new();
            for entry in dir {
                let name = entry.file_name();
                // Skip . and ..
                if name == "." || name == ".." {
                    continue;
                }

                let attrs = entry.metadata();
                let is_directory = attrs.is_dir();
                let is_symlink = attrs.is_symlink();
                let size = attrs.size.unwrap_or(0);
                let modified = attrs
                    .mtime
                    .and_then(|t| Local.timestamp_opt(t as i64, 0).single())
                    .unwrap_or_else(Local::now);

                let permissions = if let Some(perm) = attrs.permissions {
                    format_remote_permissions(perm)
                } else {
                    String::new()
                };

                entries.push(SftpFileEntry {
                    name,
                    is_directory,
                    is_symlink,
                    size,
                    modified,
                    permissions,
                });
            }

            Ok(entries)
        })
    }

    /// Remove file or directory via SFTP
    pub fn remove(&self, path: &str, is_dir: bool) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let path = path.to_string();

        self.runtime.block_on(async {
            if is_dir {
                Self::remove_dir_recursive(sftp, &path).await
            } else {
                sftp.remove_file(&path)
                    .await
                    .map_err(|e| format!("Failed to remove '{}': {}", path, e))
            }
        })
    }

    /// Recursively remove directory
    async fn remove_dir_recursive(sftp: &RusshSftpSession, path: &str) -> Result<(), String> {
        let entries = sftp
            .read_dir(path)
            .await
            .map_err(|e| format!("Failed to read dir '{}': {}", path, e))?;

        for entry in entries {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let child_path = format!("{}/{}", path.trim_end_matches('/'), name);
            let attrs = entry.metadata();
            if attrs.is_dir() {
                Box::pin(Self::remove_dir_recursive(sftp, &child_path)).await?;
            } else {
                sftp.remove_file(&child_path)
                    .await
                    .map_err(|e| format!("Failed to remove '{}': {}", child_path, e))?;
            }
        }

        sftp.remove_dir(path)
            .await
            .map_err(|e| format!("Failed to remove dir '{}': {}", path, e))
    }

    /// Rename file or directory via SFTP
    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let old = old_path.to_string();
        let new = new_path.to_string();

        self.runtime.block_on(async {
            sftp.rename(&old, &new)
                .await
                .map_err(|e| format!("Failed to rename '{}' to '{}': {}", old, new, e))
        })
    }

    /// Create directory via SFTP
    pub fn mkdir(&self, path: &str) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let path = path.to_string();

        self.runtime.block_on(async {
            sftp.create_dir(&path)
                .await
                .map_err(|e| format!("Failed to create dir '{}': {}", path, e))
        })
    }

    /// Create an empty file via SFTP
    pub fn create_file(&self, path: &str) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let path = path.to_string();

        self.runtime.block_on(async {
            // Open file with write+create+truncate flags, drop to close
            let _file = sftp
                .create(&path)
                .await
                .map_err(|e| format!("Failed to create file '{}': {}", path, e))?;
            Ok(())
        })
    }

    /// Download remote file to local path via SFTP (streaming, chunked)
    pub fn download_file(&self, remote_path: &str, local_path: &str) -> Result<u64, String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let remote_path = remote_path.to_string();
        let local_path = local_path.to_string();

        self.runtime.block_on(async {
            use tokio::io::AsyncReadExt;

            let mut remote_file = sftp
                .open(&remote_path)
                .await
                .map_err(|e| format!("Failed to open '{}': {}", remote_path, e))?;

            let mut guard = PartialFileGuard::create(local_path.clone())
                .map_err(|e| format!("Failed to create '{}': {}", local_path, e))?;

            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                let n = remote_file
                    .read(&mut buf)
                    .await
                    .map_err(|e| format!("Failed to read '{}': {}", remote_path, e))?;
                if n == 0 {
                    break;
                }
                std::io::Write::write_all(guard.writer(), &buf[..n])
                    .map_err(|e| format!("Failed to write '{}': {}", local_path, e))?;
                total += n as u64;
            }
            guard.commit();
            Ok(total)
        })
    }

    /// Download remote file with progress callback and cancellation support
    pub fn download_file_with_progress<F>(
        &self,
        remote_path: &str,
        local_path: &str,
        file_size: u64,
        cancel_flag: &std::sync::atomic::AtomicBool,
        on_progress: F,
    ) -> Result<u64, String>
    where
        F: Fn(u64, u64),
    {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let remote_path = remote_path.to_string();
        let local_path = local_path.to_string();

        self.runtime.block_on(async {
            use tokio::io::AsyncReadExt;

            let mut remote_file = sftp
                .open(&remote_path)
                .await
                .map_err(|e| format!("Failed to open '{}': {}", remote_path, e))?;

            // Guard owns the file handle; any early return (cancel / read-err
            // / write-err) drops the partial file. Successful path calls
            // commit() at the end.
            let mut guard = PartialFileGuard::create(local_path.clone())
                .map_err(|e| format!("Failed to create '{}': {}", local_path, e))?;

            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err("Cancelled".to_string());
                }
                let n = remote_file
                    .read(&mut buf)
                    .await
                    .map_err(|e| format!("Failed to read '{}': {}", remote_path, e))?;
                if n == 0 {
                    break;
                }
                std::io::Write::write_all(guard.writer(), &buf[..n])
                    .map_err(|e| format!("Failed to write '{}': {}", local_path, e))?;
                total += n as u64;
                on_progress(total, file_size);
            }
            guard.commit();
            Ok(total)
        })
    }

    /// Upload local file to remote path via SFTP (streaming, chunked)
    pub fn upload_file(&self, local_path: &str, remote_path: &str) -> Result<u64, String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let remote_path = remote_path.to_string();
        let local_path = local_path.to_string();

        self.runtime.block_on(async {
            use tokio::io::AsyncWriteExt;

            let mut local_file = std::fs::File::open(&local_path)
                .map_err(|e| format!("Failed to open '{}': {}", local_path, e))?;

            let mut remote_file = sftp
                .create(&remote_path)
                .await
                .map_err(|e| format!("Failed to create '{}': {}", remote_path, e))?;

            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                let n = std::io::Read::read(&mut local_file, &mut buf)
                    .map_err(|e| format!("Failed to read '{}': {}", local_path, e))?;
                if n == 0 {
                    break;
                }
                remote_file
                    .write_all(&buf[..n])
                    .await
                    .map_err(|e| format!("Failed to write '{}': {}", remote_path, e))?;
                total += n as u64;
            }
            remote_file
                .shutdown()
                .await
                .map_err(|e| format!("Failed to close '{}': {}", remote_path, e))?;
            Ok(total)
        })
    }

    /// Disconnect from remote host
    pub fn disconnect(&mut self) {
        // Drop SFTP first, then SSH
        self.sftp = None;
        if let Some(ssh) = self.ssh_handle.take() {
            let _ = self
                .runtime
                .block_on(async { ssh.disconnect(Disconnect::ByApplication, "", "en").await });
        }
    }

    /// Check if session is still connected
    pub fn is_connected(&self) -> bool {
        self.sftp.is_some()
    }
}

impl Drop for SftpSession {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Parse user@host:/path format
/// Returns (user, host, port, path) if matched
pub fn parse_remote_path(input: &str) -> Option<(String, String, u16, String)> {
    // Format: user@host:/path or user@host:port:/path
    let at_pos = input.find('@')?;
    let user = input[..at_pos].to_string();
    if user.is_empty() {
        return None;
    }

    let after_at = &input[at_pos + 1..];
    let colon_pos = after_at.find(':')?;
    let host_part = &after_at[..colon_pos];
    let after_first_colon = &after_at[colon_pos + 1..];

    // Check if there's a port number: host:port:/path
    let (host, port, path) = if let Some(second_colon) = after_first_colon.find(':') {
        let port_str = &after_first_colon[..second_colon];
        if let Ok(port) = port_str.parse::<u16>() {
            let path = &after_first_colon[second_colon + 1..];
            (
                host_part.to_string(),
                port,
                if path.is_empty() {
                    "/".to_string()
                } else {
                    path.to_string()
                },
            )
        } else {
            // Not a port number, treat entire after_first_colon as path
            (
                host_part.to_string(),
                22,
                if after_first_colon.is_empty() {
                    "/".to_string()
                } else {
                    after_first_colon.to_string()
                },
            )
        }
    } else {
        (
            host_part.to_string(),
            22,
            if after_first_colon.is_empty() {
                "/".to_string()
            } else {
                after_first_colon.to_string()
            },
        )
    };

    if host.is_empty() {
        return None;
    }

    // Ensure path starts with /
    let path = if path.starts_with('/') {
        path
    } else {
        format!("/{}", path)
    };

    Some((user, host, port, path))
}

/// Format remote permissions from mode bits to rwxrwxrwx string
fn format_remote_permissions(mode: u32) -> String {
    let mut perms = String::with_capacity(9);
    let flags = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for (bit, ch) in &flags {
        perms.push(if mode & bit != 0 { *ch } else { '-' });
    }
    perms
}

/// Build remote display path string (e.g., "user@host:/path")
pub fn format_remote_display(profile: &RemoteProfile, path: &str) -> String {
    if profile.port != 22 {
        format!(
            "{}@{}:{}:{}",
            profile.user, profile.host, profile.port, path
        )
    } else {
        format!("{}@{}:{}", profile.user, profile.host, path)
    }
}

/// Find matching profile from profiles list by user, host, port
pub fn find_matching_profile<'a>(
    profiles: &'a [RemoteProfile],
    user: &str,
    host: &str,
    port: u16,
) -> Option<&'a RemoteProfile> {
    profiles
        .iter()
        .find(|p| p.user == user && p.host == host && p.port == port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_remote_path_basic() {
        let result = parse_remote_path("user@host:/home/user");
        assert_eq!(
            result,
            Some((
                "user".to_string(),
                "host".to_string(),
                22,
                "/home/user".to_string()
            ))
        );
    }

    #[test]
    fn test_parse_remote_path_with_port() {
        let result = parse_remote_path("admin@server:2222:/var/log");
        assert_eq!(
            result,
            Some((
                "admin".to_string(),
                "server".to_string(),
                2222,
                "/var/log".to_string()
            ))
        );
    }

    #[test]
    fn test_parse_remote_path_no_path() {
        let result = parse_remote_path("user@host:");
        assert_eq!(
            result,
            Some(("user".to_string(), "host".to_string(), 22, "/".to_string()))
        );
    }

    #[test]
    fn test_parse_remote_path_invalid() {
        assert!(parse_remote_path("just/a/path").is_none());
        assert!(parse_remote_path("@host:/path").is_none());
        assert!(parse_remote_path("user@:/path").is_none());
    }

    #[test]
    fn test_format_remote_permissions() {
        assert_eq!(format_remote_permissions(0o755), "rwxr-xr-x");
        assert_eq!(format_remote_permissions(0o644), "rw-r--r--");
    }
}
