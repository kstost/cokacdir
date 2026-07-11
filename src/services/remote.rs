use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::runtime::Runtime;

use russh::{client, Disconnect};
use russh_sftp::client::SftpSession as RusshSftpSession;
use russh_sftp::protocol::{OpenFlags, StatusCode};

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

fn remote_upload_parent(path: &str) -> Result<String, String> {
    match path.rsplit_once('/') {
        Some((_, "")) => Err(format!("Invalid remote upload destination: {path:?}")),
        Some(("", _)) => Ok("/".to_string()),
        Some((parent, _)) => Ok(parent.to_string()),
        None if !path.is_empty() => Ok(".".to_string()),
        None => Err(format!("Invalid remote upload destination: {path:?}")),
    }
}

fn remote_upload_sidecar_path(path: &str, label: &str, nonce: u64) -> Result<String, String> {
    let parent = remote_upload_parent(path)?;
    let sidecar = format!(".cokacdir-{label}-{}-{nonce:016x}", std::process::id());
    Ok(if parent == "/" {
        format!("/{sidecar}")
    } else if parent == "." {
        sidecar
    } else {
        format!("{parent}/{sidecar}")
    })
}

fn remote_child_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else if parent == "." {
        name.to_string()
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}

fn remote_removal_is_directory(metadata: &russh_sftp::protocol::FileAttributes) -> bool {
    // LSTAT metadata for a symlink must always be treated as file-like. Calling
    // READDIR on it could otherwise traverse and delete the link target.
    metadata.is_dir() && !metadata.is_symlink()
}

fn remote_create_file_flags() -> OpenFlags {
    // SFTP v3 requires CREATE together with EXCLUDE for create-new semantics.
    OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemotePrivateDirectoryIdentity {
    uid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteStagingParentIdentity {
    uid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemoteRegularFileIdentity {
    uid: u32,
    gid: Option<u32>,
    size: u64,
    permissions: u32,
}

/// No-follow identity recorded for an existing editor destination before its
/// replacement is staged. `atime` is intentionally excluded because opening a
/// file for reading elsewhere may legitimately update it. `mtime` is included
/// when the server reports it, which catches ordinary concurrent edits in
/// addition to owner, size, and mode changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemoteDestinationIdentity {
    uid: u32,
    gid: Option<u32>,
    size: u64,
    permissions: u32,
    mtime: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFileVersion {
    identity: RemoteDestinationIdentity,
    sha256: [u8; 32],
}

impl RemoteFileVersion {
    pub(crate) fn matches_content_hash(&self, sha256: [u8; 32]) -> bool {
        self.sha256 == sha256
    }
}

#[cfg(test)]
impl RemoteFileVersion {
    pub(crate) fn for_test(seed: u8) -> Self {
        Self {
            identity: RemoteDestinationIdentity {
                uid: 1000,
                gid: Some(1000),
                size: 1,
                permissions: 0o100600,
                mtime: Some(seed as u32),
            },
            sha256: [seed; 32],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedRemoteFile {
    pub bytes: u64,
    pub version: RemoteFileVersion,
}

pub struct LocalUploadSnapshot {
    file: std::fs::File,
    size: u64,
    sha256: [u8; 32],
    display_path: String,
}

impl LocalUploadSnapshot {
    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        use std::io::{Read, Seek};

        let (mut file, before) = crate::services::file_ops::open_regular_file_no_follow(path)
            .map_err(|error| {
                format!(
                    "Failed to snapshot local upload source '{}': {}",
                    path.display(),
                    error
                )
            })?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(|error| {
                format!(
                    "Failed to read local upload snapshot '{}': {}",
                    path.display(),
                    error
                )
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            total += read as u64;
        }
        let after = file.metadata().map_err(|error| {
            format!(
                "Failed to re-inspect local upload snapshot '{}': {}",
                path.display(),
                error
            )
        })?;
        if total != before.len()
            || !crate::services::file_ops::metadata_still_matches(&before, &after)
        {
            return Err(format!(
                "Local upload source changed while it was snapshotted: '{}'",
                path.display()
            ));
        }
        file.seek(std::io::SeekFrom::Start(0))
            .map_err(|error| format!("Failed to rewind local upload snapshot: {error}"))?;
        Ok(Self {
            file,
            size: total,
            sha256: hasher.finalize().into(),
            display_path: path.display().to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemotePublishedFileIdentity {
    uid: u32,
    gid: Option<u32>,
    size: u64,
    permissions: u32,
}

/// A remote upload can be semantically committed even when metadata restoration
/// or removal of private staging/recovery artifacts fails afterward. Callers
/// must not retry those saves as though the content upload failed, because doing
/// so would replace an already-current destination and create another recovery
/// transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadFileOutcome {
    Complete {
        bytes: u64,
        version: RemoteFileVersion,
    },
    CommittedWithWarning {
        bytes: u64,
        version: RemoteFileVersion,
        warning: String,
    },
}

fn completed_upload_outcome(
    bytes: u64,
    version: RemoteFileVersion,
    warnings: Vec<String>,
) -> UploadFileOutcome {
    if warnings.is_empty() {
        UploadFileOutcome::Complete { bytes, version }
    } else {
        UploadFileOutcome::CommittedWithWarning {
            bytes,
            version,
            warning: warnings.join("; "),
        }
    }
}

fn remote_ownership_change_warning(
    path: &str,
    original: RemoteDestinationIdentity,
    committed: RemoteDestinationIdentity,
) -> Option<String> {
    if original.uid == committed.uid && original.gid == committed.gid {
        return None;
    }
    Some(format!(
        "replacement content was committed at '{path}', but ownership changed from uid {} / gid {:?} to uid {} / gid {:?}; SFTP cannot safely restore ownership for every account",
        original.uid, original.gid, committed.uid, committed.gid
    ))
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
    destination: std::path::PathBuf,
    temp_path: std::path::PathBuf,
    staging_dir: std::path::PathBuf,
    file: Option<std::fs::File>,
    file_identity: crate::services::file_ops::StablePathIdentity,
    directory_guard: Option<std::fs::File>,
    directory_identity: crate::services::file_ops::StablePathIdentity,
    committed: bool,
}

impl PartialFileGuard {
    fn create(path: String) -> std::io::Result<Self> {
        use std::fs::OpenOptions;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;

        let destination = std::path::PathBuf::from(path);
        let parent = destination.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "download destination has no parent directory",
            )
        })?;
        let staging_dir =
            crate::services::file_ops::create_private_quarantine_directory(parent, "download")?;
        let temp_path = staging_dir.join("download.tmp");
        let result = (|| {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let file = options.open(&temp_path)?;
            let file_identity = crate::services::file_ops::stable_file_identity(&file)?;
            let (directory_guard, _, metadata) =
                crate::services::file_ops::open_directory_for_read(&staging_dir)?;
            if !metadata.is_dir() {
                return Err(std::io::Error::other(
                    "download staging path is not a directory",
                ));
            }
            let directory_identity =
                crate::services::file_ops::stable_file_identity(&directory_guard)?;
            Ok(Self {
                destination,
                temp_path: temp_path.clone(),
                staging_dir: staging_dir.clone(),
                file: Some(file),
                file_identity,
                directory_guard: Some(directory_guard),
                directory_identity,
                committed: false,
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp_path);
            let _ = std::fs::remove_dir(&staging_dir);
        }
        result
    }

    fn writer(&mut self) -> &mut std::fs::File {
        self.file
            .as_mut()
            .expect("PartialFileGuard file handle already taken")
    }

    fn commit(mut self) -> std::io::Result<()> {
        self.commit_with_policy(DownloadCommitPolicy::NoReplace)
    }

    /// Publish a completed download into a cache path that may already hold a
    /// previous copy of the same remote file. Only a real regular destination
    /// may be replaced; symbolic links and Windows reparse points are rejected
    /// without following them.
    fn commit_replacing_regular_destination(mut self) -> std::io::Result<()> {
        self.commit_with_policy(DownloadCommitPolicy::ReplaceRegular)
    }

    fn commit_with_policy(&mut self, policy: DownloadCommitPolicy) -> std::io::Result<()> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| std::io::Error::other("download staging handle is unavailable"))?;
        file.sync_all()?;
        if crate::services::file_ops::stable_path_identity(&self.staging_dir)?
            != self.directory_identity
            || crate::services::file_ops::stable_path_identity(&self.temp_path)?
                != self.file_identity
        {
            return Err(std::io::Error::other(
                "download staging path was replaced; refusing to publish it",
            ));
        }
        match policy {
            DownloadCommitPolicy::NoReplace => {
                replace_download_destination(&self.temp_path, &self.destination)?
            }
            DownloadCommitPolicy::ReplaceRegular => {
                replace_regular_download_destination(&self.temp_path, &self.destination)?
            }
        }
        if crate::services::file_ops::stable_path_identity(&self.destination)? != self.file_identity
        {
            return Err(std::io::Error::other(format!(
                "download publication identity mismatch; inspect '{}' and recovery directory '{}'",
                self.destination.display(),
                self.staging_dir.display()
            )));
        }
        self.committed = true;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum DownloadCommitPolicy {
    NoReplace,
    ReplaceRegular,
}

impl Drop for PartialFileGuard {
    fn drop(&mut self) {
        let owns_staging_file = !self.committed
            && crate::services::file_ops::stable_path_identity(&self.staging_dir).ok()
                == Some(self.directory_identity)
            && crate::services::file_ops::stable_path_identity(&self.temp_path).ok()
                == Some(self.file_identity);
        drop(self.file.take());
        if owns_staging_file {
            let _ = crate::services::file_ops::remove_file_by_identity(
                &self.temp_path,
                self.file_identity,
            );
        }
        drop(self.directory_guard.take());
        if crate::services::file_ops::stable_path_identity(&self.staging_dir).ok()
            == Some(self.directory_identity)
        {
            let _ = std::fs::remove_dir(&self.staging_dir);
        }
    }
}

/// Install a completed local download without exposing a partial destination
/// and without ever replacing an existing path. The temporary file is created
/// in the destination directory, so the platform no-replace rename is atomic
/// and cannot fail with a cross-filesystem move.
fn replace_download_destination(
    temp: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    crate::services::file_ops::rename_noreplace(temp, destination)
}

fn local_metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

/// Atomically replace a previously downloaded cache file. A no-follow metadata
/// check prevents this path from being used to replace directories or other
/// special objects. If the path changes to a symlink after the check, the
/// rename replaces the link entry itself rather than following its target.
fn replace_regular_download_destination(
    temp: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    match std::fs::symlink_metadata(destination) {
        Ok(metadata)
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || local_metadata_is_reparse_point(&metadata) =>
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "download cache destination '{}' is not a real regular file",
                    destination.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    atomic_replace_local_file(temp, destination)
}

#[cfg(not(windows))]
fn atomic_replace_local_file(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn atomic_replace_local_file(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
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
#[derive(Clone, Serialize, Deserialize)]
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

impl std::fmt::Debug for RemoteAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password { .. } => formatter
                .debug_struct("Password")
                .field("password", &"<redacted>")
                .finish(),
            Self::KeyFile {
                path, passphrase, ..
            } => formatter
                .debug_struct("KeyFile")
                .field("path", path)
                .field("passphrase", &passphrase.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
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

/// Return the host form used by DNS/TCP and known_hosts. Square brackets are
/// accepted only as one complete pair around an IPv6 literal; they belong in
/// display strings, not in the connection target itself.
pub(crate) fn canonical_remote_host(host: &str) -> Result<&str, String> {
    if host.is_empty() {
        return Err("Remote host cannot be empty".to_string());
    }
    if host
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '@' | '/' | '\\'))
    {
        return Err(format!("Invalid remote host: {host:?}"));
    }

    if host.starts_with('[') || host.ends_with(']') {
        let inner = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .ok_or_else(|| format!("Invalid bracketed remote host: {host:?}"))?;
        if inner.is_empty()
            || !inner.contains(':')
            || inner.chars().any(|ch| matches!(ch, '[' | ']'))
        {
            return Err(format!("Invalid bracketed remote host: {host:?}"));
        }
        return Ok(inner);
    }

    if host.chars().any(|ch| matches!(ch, '[' | ']')) {
        return Err(format!("Invalid bracketed remote host: {host:?}"));
    }
    Ok(host)
}

pub(crate) fn remote_hosts_equal(left: &str, right: &str) -> bool {
    match (canonical_remote_host(left), canonical_remote_host(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
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
    pub(crate) fn new(profile: &RemoteProfile) -> Result<Self, String> {
        Ok(Self {
            host: canonical_remote_host(&profile.host)?.to_string(),
            port: profile.port,
        })
    }
}

/// Format an SSH connect error, adding actionable guidance when the failure is a
/// changed host key (the security-rejection case the user must resolve by hand).
pub(crate) fn format_ssh_connect_error(e: &russh::Error) -> String {
    match e {
        russh::Error::KeyChanged { line } => format!(
            "SSH connection failed: remote host key changed (known_hosts line {}). \
             If the server was legitimately reinstalled, remove that line from \
             ~/.ssh/known_hosts and reconnect.",
            line
        ),
        russh::Error::NoCommonAlgo {
            kind: russh::AlgorithmKind::Key,
            ..
        } => "SSH connection failed: the server offers no compatible host-key algorithm. Configure the server with a host key supported by this client (Ed25519 or ECDSA are recommended)."
            .to_string(),
        _ => format!("SSH connection failed: {}", e),
    }
}

pub(crate) fn load_supported_secret_key(
    path: &std::path::Path,
    passphrase: Option<&str>,
) -> Result<russh::keys::PrivateKey, String> {
    let key = russh::keys::load_secret_key(path, passphrase).map_err(|error| match &error {
        russh::keys::Error::UnsupportedKeyType { key_type_string, .. }
            if key_type_string.contains("rsa") =>
        {
            "RSA SSH keys are not supported because the upstream Rust RSA implementation has an unresolved timing vulnerability. Use an Ed25519 or ECDSA key instead."
                .to_string()
        }
        _ => format!("Failed to load key '{}': {error}", path.display()),
    })?;
    if key.algorithm().is_rsa() {
        return Err(
            "RSA SSH keys are not supported because the upstream Rust RSA implementation has an unresolved timing vulnerability. Use an Ed25519 or ECDSA key instead."
                .to_string(),
        );
    }
    Ok(key)
}

async fn sftp_path_exists_no_follow(
    sftp: &RusshSftpSession,
    path: &str,
) -> Result<bool, russh_sftp::client::error::Error> {
    match sftp.symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(russh_sftp::client::error::Error::Status(status))
            if status.status_code == StatusCode::NoSuchFile =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn remote_private_directory_identity(
    path: &str,
    metadata: &russh_sftp::protocol::FileAttributes,
) -> Result<RemotePrivateDirectoryIdentity, String> {
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(format!(
            "Remote staging path is not a real directory: '{path}'"
        ));
    }
    let permissions = metadata.permissions.ok_or_else(|| {
        format!("The SFTP server did not report permissions for remote staging path '{path}'")
    })?;
    if permissions & 0o777 != 0o700 {
        return Err(format!(
            "Remote staging directory '{path}' is not private (mode {:04o})",
            permissions & 0o7777
        ));
    }
    let uid = metadata.uid.ok_or_else(|| {
        format!(
            "The SFTP server did not report an owner UID for remote staging path '{path}'; ownership cannot be verified safely"
        )
    })?;
    Ok(RemotePrivateDirectoryIdentity { uid })
}

fn remote_regular_file_identity(
    path: &str,
    metadata: &russh_sftp::protocol::FileAttributes,
) -> Result<RemoteRegularFileIdentity, String> {
    if !metadata.is_regular() || metadata.is_symlink() {
        return Err(format!(
            "Remote upload staging path is not a regular file: '{path}'"
        ));
    }
    let permissions = metadata.permissions.ok_or_else(|| {
        format!("The SFTP server did not report permissions for remote upload path '{path}'")
    })?;
    if permissions & 0o077 != 0 {
        return Err(format!(
            "Remote upload staging file '{path}' is accessible by another account (mode {:04o})",
            permissions & 0o7777
        ));
    }
    Ok(RemoteRegularFileIdentity {
        uid: metadata.uid.ok_or_else(|| {
            format!(
                "The SFTP server did not report an owner UID for remote upload path '{path}'; ownership cannot be verified safely"
            )
        })?,
        gid: metadata.gid,
        size: metadata.size.ok_or_else(|| {
            format!("The SFTP server did not report a size for remote upload path '{path}'")
        })?,
        permissions,
    })
}

fn remote_destination_identity(
    path: &str,
    metadata: &russh_sftp::protocol::FileAttributes,
) -> Result<RemoteDestinationIdentity, String> {
    if !metadata.is_regular() || metadata.is_symlink() {
        return Err(format!(
            "Remote editor destination is not a real regular file: '{path}'"
        ));
    }
    let permissions = metadata.permissions.ok_or_else(|| {
        format!("The SFTP server did not report permissions for remote editor destination '{path}'")
    })?;
    Ok(RemoteDestinationIdentity {
        uid: metadata.uid.ok_or_else(|| {
            format!(
                "The SFTP server did not report an owner UID for remote editor destination '{path}'; ownership cannot be verified safely"
            )
        })?,
        gid: metadata.gid,
        size: metadata.size.ok_or_else(|| {
            format!("The SFTP server did not report a size for remote editor destination '{path}'")
        })?,
        permissions,
        mtime: metadata.mtime,
    })
}

fn remote_published_file_identity(
    path: &str,
    metadata: &russh_sftp::protocol::FileAttributes,
) -> Result<RemotePublishedFileIdentity, String> {
    if !metadata.is_regular() || metadata.is_symlink() {
        return Err(format!(
            "Published remote upload is not a real regular file: '{path}'"
        ));
    }
    Ok(RemotePublishedFileIdentity {
        uid: metadata.uid.ok_or_else(|| {
            format!(
                "The SFTP server did not report an owner UID for published remote upload '{path}'"
            )
        })?,
        gid: metadata.gid,
        size: metadata.size.ok_or_else(|| {
            format!("The SFTP server did not report a size for published remote upload '{path}'")
        })?,
        permissions: metadata.permissions.ok_or_else(|| {
            format!(
                "The SFTP server did not report permissions for published remote upload '{path}'"
            )
        })?,
    })
}

/// Replacing file contents must not re-enable setuid/setgid/sticky bits that
/// an in-place write would ordinarily clear. Preserve the file type and normal
/// rwx mode only.
fn safe_replacement_permissions(permissions: u32) -> u32 {
    permissions & !0o7000
}

async fn sftp_existing_destination_identity(
    sftp: &RusshSftpSession,
    path: &str,
) -> Result<Option<RemoteDestinationIdentity>, String> {
    match sftp.symlink_metadata(path).await {
        Ok(metadata) => remote_destination_identity(path, &metadata).map(Some),
        Err(russh_sftp::client::error::Error::Status(status))
            if status.status_code == StatusCode::NoSuchFile =>
        {
            Ok(None)
        }
        Err(error) => Err(format!(
            "Failed to inspect remote editor destination '{path}': {error}"
        )),
    }
}

async fn sftp_hash_stable_regular_file(
    sftp: &RusshSftpSession,
    path: &str,
    cancel_flag: Option<&std::sync::atomic::AtomicBool>,
) -> Result<RemoteFileVersion, String> {
    use tokio::io::AsyncReadExt;

    let before = sftp_existing_destination_identity(sftp, path)
        .await?
        .ok_or_else(|| {
            format!("Remote file disappeared while its version was checked: '{path}'")
        })?;
    let mut file = sftp.open(path).await.map_err(|error| {
        format!("Failed to open remote file for version check '{path}': {error}")
    })?;
    let opened = file
        .metadata()
        .await
        .map_err(|error| format!("Failed to inspect opened remote file '{path}': {error}"))?;
    if remote_destination_identity(path, &opened)? != before {
        return Err(format!(
            "Remote file changed while it was opened for version check: '{path}'"
        ));
    }

    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        if cancel_flag.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err("Cancelled".to_string());
        }
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|error| format!("Failed to hash remote file '{path}': {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    if total != before.size {
        return Err(format!(
            "Remote file size changed while its version was checked: '{path}'"
        ));
    }

    let opened_after = file
        .metadata()
        .await
        .map_err(|error| format!("Failed to re-inspect opened remote file '{path}': {error}"))?;
    if remote_destination_identity(path, &opened_after)? != before {
        return Err(format!(
            "Remote file changed while its contents were hashed: '{path}'"
        ));
    }
    drop(file);
    let path_after = sftp_existing_destination_identity(sftp, path)
        .await?
        .ok_or_else(|| format!("Remote file disappeared after version check: '{path}'"))?;
    if path_after != before {
        return Err(format!(
            "Remote file path changed while its version was checked: '{path}'"
        ));
    }

    Ok(RemoteFileVersion {
        identity: before,
        sha256: hasher.finalize().into(),
    })
}

fn remote_version_mismatch(path: &str) -> String {
    format!(
        "Remote file changed since it was opened; refusing to overwrite external changes: '{path}'"
    )
}

async fn sftp_verify_destination_identity(
    sftp: &RusshSftpSession,
    path: &str,
    expected: RemoteDestinationIdentity,
) -> Result<(), String> {
    match sftp_existing_destination_identity(sftp, path).await? {
        Some(current) if current == expected => Ok(()),
        Some(_) => Err(format!(
            "Remote editor destination changed while the replacement was being staged: '{path}'"
        )),
        None => Err(format!(
            "Remote editor destination disappeared while the replacement was being staged: '{path}'"
        )),
    }
}

async fn sftp_verify_published_file(
    sftp: &RusshSftpSession,
    path: &str,
    payload: RemoteRegularFileIdentity,
    permissions: u32,
) -> Result<(), String> {
    let metadata = sftp
        .symlink_metadata(path)
        .await
        .map_err(|error| format!("Failed to inspect published remote upload '{path}': {error}"))?;
    let current = remote_published_file_identity(path, &metadata)?;
    let expected = RemotePublishedFileIdentity {
        uid: payload.uid,
        gid: payload.gid,
        size: payload.size,
        permissions,
    };
    if current != expected {
        return Err(format!(
            "Published remote upload changed at '{path}'; expected {expected:?}, found {current:?}"
        ));
    }
    Ok(())
}

async fn sftp_verify_staging_parent(
    sftp: &RusshSftpSession,
    path: &str,
) -> Result<RemoteStagingParentIdentity, String> {
    let metadata = sftp
        .symlink_metadata(path)
        .await
        .map_err(|error| format!("Failed to inspect remote staging parent '{path}': {error}"))?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(format!(
            "Remote staging parent is not a real directory: '{path}'"
        ));
    }
    let permissions = metadata.permissions.ok_or_else(|| {
        format!(
            "The SFTP server did not report permissions for remote staging parent '{path}'; safe staging cannot be verified"
        )
    })?;
    if permissions & 0o022 != 0 && permissions & 0o1000 == 0 {
        return Err(format!(
            "Remote staging parent '{path}' is group/world-writable without the sticky bit (mode {:04o}); refusing a replaceable staging path",
            permissions & 0o7777
        ));
    }
    let uid = metadata.uid.ok_or_else(|| {
        format!(
            "The SFTP server did not report an owner UID for remote staging parent '{path}'; ownership cannot be verified safely"
        )
    })?;
    Ok(RemoteStagingParentIdentity { uid })
}

async fn sftp_verify_staging_parent_identity(
    sftp: &RusshSftpSession,
    path: &str,
    expected: RemoteStagingParentIdentity,
) -> Result<(), String> {
    if sftp_verify_staging_parent(sftp, path).await? != expected {
        return Err(format!(
            "Remote staging parent ownership changed at '{path}'; refusing to publish or delete staged data"
        ));
    }
    Ok(())
}

async fn sftp_verify_private_directory(
    sftp: &RusshSftpSession,
    path: &str,
    expected: RemotePrivateDirectoryIdentity,
) -> Result<(), String> {
    let metadata = sftp.symlink_metadata(path).await.map_err(|error| {
        format!("Failed to re-inspect remote staging directory '{path}': {error}")
    })?;
    let current = remote_private_directory_identity(path, &metadata)?;
    if current != expected {
        return Err(format!(
            "Remote staging directory ownership changed at '{path}'; refusing to publish or delete it"
        ));
    }
    Ok(())
}

async fn sftp_directory_is_empty(sftp: &RusshSftpSession, path: &str) -> Result<bool, String> {
    let entries = sftp
        .read_dir(path)
        .await
        .map_err(|error| format!("Failed to inspect remote staging directory '{path}': {error}"))?;
    Ok(entries
        .filter(|entry| {
            let name = entry.file_name();
            name != "." && name != ".."
        })
        .next()
        .is_none())
}

async fn sftp_create_private_directory(
    sftp: &RusshSftpSession,
    path: &str,
) -> Result<RemotePrivateDirectoryIdentity, String> {
    sftp.create_dir(path)
        .await
        .map_err(|error| format!("Failed to create private remote directory '{path}': {error}"))?;

    let mut attrs = russh_sftp::protocol::FileAttributes::empty();
    attrs.permissions = Some(0o700);
    if let Err(error) = sftp.set_metadata(path, attrs).await {
        let cleanup = sftp
            .remove_dir(path)
            .await
            .err()
            .map(|cleanup_error| format!("; cleanup also failed: {cleanup_error}"))
            .unwrap_or_default();
        return Err(format!(
            "Failed to restrict remote temporary directory '{path}': {error}{cleanup}"
        ));
    }

    let metadata = sftp.symlink_metadata(path).await.map_err(|error| {
        format!("Failed to verify new remote staging directory '{path}': {error}")
    })?;
    let identity = remote_private_directory_identity(path, &metadata)?;
    if !sftp_directory_is_empty(sftp, path).await? {
        return Err(format!(
            "New remote staging directory '{path}' was modified before it could be secured; it was left untouched for inspection"
        ));
    }
    Ok(identity)
}

async fn sftp_verify_regular_file_path(
    sftp: &RusshSftpSession,
    path: &str,
    expected: RemoteRegularFileIdentity,
) -> Result<(), String> {
    let metadata = sftp.symlink_metadata(path).await.map_err(|error| {
        format!("Failed to inspect remote upload staging file '{path}': {error}")
    })?;
    let current = remote_regular_file_identity(path, &metadata)?;
    if current != expected {
        return Err(format!(
            "Remote upload staging file changed at '{path}'; refusing to publish it"
        ));
    }
    Ok(())
}

async fn sftp_cleanup_upload_stage(
    sftp: &RusshSftpSession,
    stage_path: &str,
    stage_identity: RemotePrivateDirectoryIdentity,
    payload_path: &str,
) -> Result<(), String> {
    sftp_verify_private_directory(sftp, stage_path, stage_identity).await?;
    match sftp.symlink_metadata(payload_path).await {
        Ok(metadata) => {
            let payload = remote_regular_file_identity(payload_path, &metadata)?;
            if payload.uid != stage_identity.uid {
                return Err(format!(
                    "Remote upload payload ownership changed at '{payload_path}'; refusing to delete it"
                ));
            }
            sftp.remove_file(payload_path).await.map_err(|error| {
                format!("Failed to remove remote upload payload '{payload_path}': {error}")
            })?;
        }
        Err(russh_sftp::client::error::Error::Status(status))
            if status.status_code == StatusCode::NoSuchFile => {}
        Err(error) => {
            return Err(format!(
                "Failed to inspect remote upload payload '{payload_path}': {error}"
            ));
        }
    }
    if !sftp_directory_is_empty(sftp, stage_path).await? {
        return Err(format!(
            "Remote upload staging directory '{stage_path}' contains an unexpected entry; refusing recursive cleanup"
        ));
    }
    sftp.remove_dir(stage_path).await.map_err(|error| {
        format!("Failed to remove remote upload staging directory '{stage_path}': {error}")
    })
}

async fn sftp_allocate_upload_sidecar(
    sftp: &RusshSftpSession,
    remote_path: &str,
    label: &str,
) -> Result<String, String> {
    let mut last_error = None;
    for _ in 0..32 {
        let candidate = remote_upload_sidecar_path(remote_path, label, rand::random::<u64>())?;
        match sftp_path_exists_no_follow(sftp, &candidate).await {
            Ok(false) => return Ok(candidate),
            Ok(true) => {}
            Err(error) => {
                last_error = Some(format!(
                    "Failed to inspect remote {label} path '{candidate}': {error}"
                ));
                break;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| format!("Unable to allocate a unique remote {label} path")))
}

async fn sftp_publish_upload_noreplace(
    sftp: &RusshSftpSession,
    payload_path: &str,
    destination: &str,
) -> Result<(), russh_sftp::client::error::Error> {
    // Prefer OpenSSH's hardlink extension for a race-free no-clobber
    // publication. Standard SFTP v3 RENAME has the same no-replace rule and
    // is the fallback for servers without that extension.
    match sftp.hardlink(payload_path, destination).await {
        Ok(true) => Ok(()),
        Ok(false) => sftp.rename(payload_path, destination).await,
        Err(russh_sftp::client::error::Error::Status(hardlink_error)) => sftp
            .rename(payload_path, destination)
            .await
            .map_err(|rename_error| {
                russh_sftp::client::error::Error::UnexpectedBehavior(format!(
                    "hard-link publish failed ({hardlink_error:?}); rename fallback failed: {rename_error}"
                ))
            }),
        Err(error) => Err(error),
    }
}

async fn sftp_restore_upload_backup(
    sftp: &RusshSftpSession,
    destination: &str,
    backup_path: &str,
    expected: RemoteDestinationIdentity,
) -> Result<(), String> {
    match sftp_existing_destination_identity(sftp, destination).await {
        Ok(Some(current)) if current == expected => return Ok(()),
        Ok(Some(_)) => {
            return Err(format!(
                "the destination is occupied by a different file; the original remains at recovery path '{backup_path}'"
            ));
        }
        Ok(None) => {}
        Err(error) => {
            return Err(format!(
                "could not verify whether the destination is free ({error}); the original remains at recovery path '{backup_path}'"
            ));
        }
    }

    if let Err(rename_error) = sftp.rename(backup_path, destination).await {
        // SFTP status delivery can fail after the server completed the rename.
        // Re-observe the destination before declaring rollback failure.
        match sftp_existing_destination_identity(sftp, destination).await {
            Ok(Some(current)) if current == expected => return Ok(()),
            _ => {
                return Err(format!(
                    "rollback rename failed: {rename_error}; the original remains at recovery path '{backup_path}' or was restored at '{destination}', but the server state could not be verified"
                ));
            }
        }
    }
    sftp_verify_destination_identity(sftp, destination, expected)
        .await
        .map_err(|error| {
            format!(
                "rollback response was not verifiable ({error}); inspect '{destination}' and recovery path '{backup_path}'"
            )
        })
}

/// Path to the OpenSSH-compatible known_hosts file (`~/.ssh/known_hosts`).
/// russh-keys defaults to `~/ssh/known_hosts` (no dot) on Windows, which would
/// diverge from the ssh/rsync CLI used for transfers, so pass the path explicitly.
fn openssh_known_hosts_path() -> Result<std::path::PathBuf, russh::Error> {
    dirs::home_dir()
        .map(|home| home.join(".ssh").join("known_hosts"))
        .ok_or_else(|| russh::keys::Error::NoHomeDir.into())
}

impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
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

        // URI-style brackets are presentation syntax, not part of the host
        // passed to DNS/TCP or OpenSSH known_hosts matching.
        let handler = SshHandler::new(profile)?;
        let connect_host = handler.host.clone();
        let mut ssh = client::connect(
            Arc::new(config),
            (connect_host.as_str(), profile.port),
            handler,
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
                let key_pair = load_supported_secret_key(&key_path, passphrase.as_deref())?;

                ssh.authenticate_publickey(
                    &profile.user,
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key_pair), None),
                )
                .await
                .map_err(|e| format!("Key auth failed: {}", e))?
            }
        };

        if !auth_result.success() {
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

    /// Check for any remote directory entry without following symlinks. This
    /// treats dangling symlinks as occupied destinations.
    pub(crate) fn path_exists_no_follow(&self, path: &str) -> Result<bool, String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        self.runtime
            .block_on(sftp_path_exists_no_follow(sftp, path))
            .map_err(|e| format!("Failed to inspect remote path '{}': {}", path, e))
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

    /// Remove a file or directory via SFTP. The listing-time type is retained
    /// in the API for compatibility but is deliberately ignored: the current
    /// object is inspected with LSTAT immediately before removal.
    pub fn remove(&self, path: &str, _listing_said_directory: bool) -> Result<(), String> {
        self.remove_path(path)
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
            // Directory entries can become stale while a recursive delete is
            // running. Re-LSTAT each child instead of trusting READDIR attrs.
            Box::pin(Self::remove_path_current(sftp, &child_path)).await?;
        }

        sftp.remove_dir(path)
            .await
            .map_err(|e| format!("Failed to remove dir '{}': {}", path, e))
    }

    /// Rename file or directory via SFTP
    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<(), String> {
        self.rename_noreplace(old_path, new_path)
    }

    /// SFTP v3 RENAME is specified to fail when `new_path` already exists.
    /// Check with LSTAT first for an actionable error (including dangling
    /// symlinks), then rely on the protocol operation for race-free publish.
    pub(crate) fn rename_noreplace(&self, old_path: &str, new_path: &str) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let old = old_path.to_string();
        let new = new_path.to_string();

        self.runtime.block_on(async {
            if sftp_path_exists_no_follow(sftp, &new)
                .await
                .map_err(|e| format!("Failed to inspect remote destination '{}': {}", new, e))?
            {
                return Err(format!("Destination already exists: '{}'", new));
            }
            sftp.rename(&old, &new)
                .await
                .map_err(|e| format!("Failed to rename '{}' to '{}': {}", old, new, e))
        })
    }

    pub(crate) fn remove_path(&self, path: &str) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let path = path.to_string();
        self.runtime
            .block_on(Self::remove_path_current(sftp, &path))
    }

    async fn remove_path_current(sftp: &RusshSftpSession, path: &str) -> Result<(), String> {
        let metadata = sftp
            .symlink_metadata(path)
            .await
            .map_err(|e| format!("Failed to inspect '{}': {}", path, e))?;
        if remote_removal_is_directory(&metadata) {
            Box::pin(Self::remove_dir_recursive(sftp, path)).await
        } else {
            sftp.remove_file(path)
                .await
                .map_err(|e| format!("Failed to remove '{}': {}", path, e))
        }
    }

    pub(crate) fn staging_parent_identity(
        &self,
        path: &str,
    ) -> Result<RemoteStagingParentIdentity, String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        self.runtime
            .block_on(sftp_verify_staging_parent(sftp, path))
    }

    pub(crate) fn verify_staging_parent(
        &self,
        path: &str,
        expected: RemoteStagingParentIdentity,
    ) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        self.runtime
            .block_on(sftp_verify_staging_parent_identity(sftp, path, expected))
    }

    pub(crate) fn create_private_dir(
        &self,
        path: &str,
    ) -> Result<RemotePrivateDirectoryIdentity, String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        self.runtime
            .block_on(sftp_create_private_directory(sftp, path))
    }

    pub(crate) fn verify_private_dir(
        &self,
        path: &str,
        expected: RemotePrivateDirectoryIdentity,
    ) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        self.runtime
            .block_on(sftp_verify_private_directory(sftp, path, expected))
    }

    pub(crate) fn remove_private_dir(
        &self,
        path: &str,
        expected: RemotePrivateDirectoryIdentity,
    ) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        self.runtime.block_on(async {
            sftp_verify_private_directory(sftp, path, expected).await?;
            if !sftp_directory_is_empty(sftp, path).await? {
                return Err(format!(
                    "Remote private directory '{path}' is not empty and was preserved for recovery"
                ));
            }
            sftp.remove_dir(path).await.map_err(|error| {
                format!("Failed to remove empty private directory '{path}': {error}")
            })
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
            // Match local mkfile semantics: creating an already-existing path
            // must fail rather than silently truncating the remote file.
            let _file = sftp
                .open_with_flags(&path, remote_create_file_flags())
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
            guard
                .commit()
                .map_err(|e| format!("Failed to install '{}': {}", local_path, e))?;
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
            guard
                .commit_replacing_regular_destination()
                .map_err(|e| format!("Failed to install '{}': {}", local_path, e))?;
            Ok(total)
        })
    }

    /// Download a remote editor file and return the exact open-time version.
    /// The downloaded bytes are hashed while streaming, then the remote path
    /// is reopened and hashed again before the local cache is published. This
    /// rejects path replacement and same-size/same-second content changes that
    /// sparse SFTP attributes alone cannot detect.
    pub fn download_editor_file_with_progress<F>(
        &self,
        remote_path: &str,
        local_path: &str,
        cancel_flag: &std::sync::atomic::AtomicBool,
        on_progress: F,
    ) -> Result<DownloadedRemoteFile, String>
    where
        F: Fn(u64, u64),
    {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let remote_path = remote_path.to_string();
        let local_path = local_path.to_string();

        self.runtime.block_on(async {
            use tokio::io::AsyncReadExt;

            let expected_identity = sftp_existing_destination_identity(sftp, &remote_path)
                .await?
                .ok_or_else(|| format!("Remote editor file does not exist: '{remote_path}'"))?;
            let mut remote_file = sftp
                .open(&remote_path)
                .await
                .map_err(|error| format!("Failed to open '{}': {}", remote_path, error))?;
            let opened = remote_file.metadata().await.map_err(|error| {
                format!(
                    "Failed to inspect opened remote file '{}': {}",
                    remote_path, error
                )
            })?;
            if remote_destination_identity(&remote_path, &opened)? != expected_identity {
                return Err(format!(
                    "Remote editor file changed while it was opened: '{}'",
                    remote_path
                ));
            }

            let mut guard = PartialFileGuard::create(local_path.clone())
                .map_err(|error| format!("Failed to create '{}': {}", local_path, error))?;
            let mut hasher = Sha256::new();
            let mut buffer = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err("Cancelled".to_string());
                }
                let read = remote_file
                    .read(&mut buffer)
                    .await
                    .map_err(|error| format!("Failed to read '{}': {}", remote_path, error))?;
                if read == 0 {
                    break;
                }
                std::io::Write::write_all(guard.writer(), &buffer[..read])
                    .map_err(|error| format!("Failed to write '{}': {}", local_path, error))?;
                hasher.update(&buffer[..read]);
                total += read as u64;
                on_progress(total, expected_identity.size);
            }
            if total != expected_identity.size {
                return Err(format!(
                    "Remote editor file size changed during download: '{}'",
                    remote_path
                ));
            }
            let opened_after = remote_file.metadata().await.map_err(|error| {
                format!(
                    "Failed to re-inspect opened remote file '{}': {}",
                    remote_path, error
                )
            })?;
            if remote_destination_identity(&remote_path, &opened_after)? != expected_identity {
                return Err(format!(
                    "Remote editor file changed during download: '{}'",
                    remote_path
                ));
            }
            drop(remote_file);
            let path_after = sftp_existing_destination_identity(sftp, &remote_path)
                .await?
                .ok_or_else(|| {
                    format!("Remote editor file disappeared during download: '{remote_path}'")
                })?;
            if path_after != expected_identity {
                return Err(format!(
                    "Remote editor file path changed during download: '{}'",
                    remote_path
                ));
            }

            let downloaded_version = RemoteFileVersion {
                identity: expected_identity,
                sha256: hasher.finalize().into(),
            };
            let final_path_version =
                sftp_hash_stable_regular_file(sftp, &remote_path, Some(cancel_flag)).await?;
            if final_path_version != downloaded_version {
                return Err(format!(
                    "Remote editor file contents changed during download: '{}'",
                    remote_path
                ));
            }

            guard
                .commit_replacing_regular_destination()
                .map_err(|error| format!("Failed to install '{}': {}", local_path, error))?;
            Ok(DownloadedRemoteFile {
                bytes: total,
                version: downloaded_version,
            })
        })
    }

    /// Upload local file to remote path via SFTP (streaming, chunked)
    pub fn upload_file(
        &self,
        mut local_snapshot: LocalUploadSnapshot,
        remote_path: &str,
        expected_version: &RemoteFileVersion,
    ) -> Result<UploadFileOutcome, String> {
        let sftp = self.sftp.as_ref().ok_or("Not connected")?;
        let remote_path = remote_path.to_string();
        let expected_version = expected_version.clone();

        self.runtime.block_on(async {
            use tokio::io::AsyncWriteExt;

            // `upload_file` is the remote editor's save operation. Record the
            // existing no-follow identity before spending time on staging so
            // a symlink, directory, or ordinarily changed file is never
            // silently replaced.
            let destination_identity = sftp_existing_destination_identity(sftp, &remote_path)
                .await?
                .ok_or_else(|| remote_version_mismatch(&remote_path))?;
            if destination_identity != expected_version.identity {
                return Err(remote_version_mismatch(&remote_path));
            }

            let parent = remote_upload_parent(&remote_path)?;
            let parent_identity = sftp_verify_staging_parent(sftp, &parent).await?;

            // A sibling temporary file can be unlinked and replaced while its
            // open handle still receives our bytes. Use a private directory,
            // record its owner, and verify both it and the payload again before
            // any path-based publish or cleanup operation.
            let (stage_path, stage_identity) = {
                let mut created = None;
                for _ in 0..32 {
                    let candidate = remote_upload_sidecar_path(
                        &remote_path,
                        "upload",
                        rand::random::<u64>(),
                    )?;
                    if sftp_path_exists_no_follow(sftp, &candidate)
                        .await
                        .map_err(|error| {
                            format!(
                                "Failed to inspect remote upload staging path '{}': {}",
                                candidate, error
                            )
                        })?
                    {
                        continue;
                    }
                    let identity = sftp_create_private_directory(sftp, &candidate).await?;
                    created = Some((candidate, identity));
                    break;
                }
                created.ok_or_else(|| {
                    "Unable to allocate a unique private remote upload directory".to_string()
                })?
            };
            let temp_path = remote_child_path(&stage_path, "payload");
            sftp_verify_staging_parent_identity(sftp, &parent, parent_identity).await?;
            let mut create_attrs = russh_sftp::protocol::FileAttributes::empty();
            create_attrs.permissions = Some(0o600);
            let mut remote_file = match sftp
                .open_with_flags_and_attributes(
                    &temp_path,
                    OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                    create_attrs,
                )
                .await
            {
                Ok(file) => file,
                Err(error) => {
                    let cleanup = sftp_cleanup_upload_stage(
                        sftp,
                        &stage_path,
                        stage_identity,
                        &temp_path,
                    )
                    .await
                    .err()
                    .map(|cleanup_error| format!("; cleanup also failed: {cleanup_error}"))
                    .unwrap_or_default();
                    return Err(format!(
                        "Failed to create private remote upload payload '{}': {}{}",
                        temp_path, error, cleanup
                    ));
                }
            };

            let mut private_attrs = russh_sftp::protocol::FileAttributes::empty();
            private_attrs.permissions = Some(0o600);
            if let Err(error) = remote_file.set_metadata(private_attrs).await {
                drop(remote_file);
                let cleanup = sftp_cleanup_upload_stage(
                    sftp,
                    &stage_path,
                    stage_identity,
                    &temp_path,
                )
                .await
                .err()
                .map(|cleanup_error| format!("; cleanup also failed: {cleanup_error}"))
                .unwrap_or_default();
                return Err(format!(
                    "Failed to restrict remote upload payload '{}': {}{}",
                    temp_path, error, cleanup
                ));
            }

            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            let mut payload_hasher = Sha256::new();
            let transfer_result: Result<(RemoteRegularFileIdentity, [u8; 32]), String> = async {
                loop {
                    let n = std::io::Read::read(&mut local_snapshot.file, &mut buf).map_err(
                        |e| {
                            format!(
                                "Failed to read local upload snapshot '{}': {}",
                                local_snapshot.display_path, e
                            )
                        },
                    )?;
                    if n == 0 {
                        break;
                    }
                    remote_file
                        .write_all(&buf[..n])
                        .await
                        .map_err(|e| format!("Failed to write staging file for '{}': {}", remote_path, e))?;
                    payload_hasher.update(&buf[..n]);
                    total += n as u64;
                }
                remote_file.flush().await.map_err(|e| {
                    format!("Failed to flush staging file for '{}': {}", remote_path, e)
                })?;
                remote_file.sync_all().await.map_err(|e| {
                    format!("Failed to sync staging file for '{}': {}", remote_path, e)
                })?;
                let metadata = remote_file.metadata().await.map_err(|e| {
                    format!("Failed to identify staging file for '{}': {}", remote_path, e)
                })?;
                let identity = remote_regular_file_identity(&temp_path, &metadata)?;
                if identity.uid != stage_identity.uid || identity.size != total {
                    return Err(format!(
                        "Remote upload handle ownership or size changed for '{}'",
                        remote_path
                    ));
                }
                remote_file
                    .shutdown()
                    .await
                    .map_err(|e| format!("Failed to close staging file for '{}': {}", remote_path, e))?;
                Ok((identity, payload_hasher.finalize().into()))
            }
            .await;
            drop(remote_file);
            let (payload_identity, payload_sha256) = match transfer_result {
                Ok(payload) => payload,
                Err(error) => {
                    let cleanup = sftp_cleanup_upload_stage(
                        sftp,
                        &stage_path,
                        stage_identity,
                        &temp_path,
                    )
                    .await
                    .err()
                    .map(|cleanup_error| format!("; cleanup also failed: {cleanup_error}"))
                    .unwrap_or_default();
                    return Err(format!("{error}{cleanup}"));
                }
            };
            if total != local_snapshot.size || payload_sha256 != local_snapshot.sha256 {
                let cleanup = sftp_cleanup_upload_stage(
                    sftp,
                    &stage_path,
                    stage_identity,
                    &temp_path,
                )
                .await
                .err()
                .map(|cleanup_error| format!("; cleanup also failed: {cleanup_error}"))
                .unwrap_or_default();
                return Err(format!(
                    "Local upload snapshot changed while it was read: '{}'{}",
                    local_snapshot.display_path, cleanup
                ));
            }

            sftp_verify_staging_parent_identity(sftp, &parent, parent_identity).await?;
            sftp_verify_private_directory(sftp, &stage_path, stage_identity).await?;
            sftp_verify_regular_file_path(sftp, &temp_path, payload_identity).await?;

            // Re-hash immediately before the backup rename. This detects
            // content changes since the editor opened the file even when size
            // and second-resolution mtime are unchanged.
            let current_version = match sftp_hash_stable_regular_file(sftp, &remote_path, None).await
            {
                Ok(version) => version,
                Err(error) => {
                    let cleanup = sftp_cleanup_upload_stage(
                        sftp,
                        &stage_path,
                        stage_identity,
                        &temp_path,
                    )
                    .await
                    .err()
                    .map(|cleanup_error| format!("; staging cleanup also failed: {cleanup_error}"))
                    .unwrap_or_default();
                    return Err(format!("{error}{cleanup}"));
                }
            };
            if current_version != expected_version {
                let cleanup = sftp_cleanup_upload_stage(
                    sftp,
                    &stage_path,
                    stage_identity,
                    &temp_path,
                )
                .await
                .err()
                .map(|cleanup_error| format!("; staging cleanup also failed: {cleanup_error}"))
                .unwrap_or_default();
                return Err(format!("{}{}", remote_version_mismatch(&remote_path), cleanup));
            }

            let backup_path = match sftp_allocate_upload_sidecar(sftp, &remote_path, "backup").await
            {
                Ok(path) => path,
                Err(error) => {
                    let cleanup = sftp_cleanup_upload_stage(
                        sftp,
                        &stage_path,
                        stage_identity,
                        &temp_path,
                    )
                    .await
                    .err()
                    .map(|cleanup_error| {
                        format!("; staging cleanup also failed: {cleanup_error}")
                    })
                    .unwrap_or_default();
                    return Err(format!("{error}{cleanup}"));
                }
            };
            let rename_result = sftp.rename(&remote_path, &backup_path).await;
            if let Err(rename_error) = rename_result {
                let destination_after = sftp_existing_destination_identity(sftp, &remote_path).await;
                let backup_after = sftp_existing_destination_identity(sftp, &backup_path).await;
                match (destination_after, backup_after) {
                    (Ok(None), Ok(Some(current))) if current == destination_identity => {
                        // The server response was ambiguous, but the no-follow
                        // observations prove that a candidate backup landed.
                    }
                    (Ok(Some(current)), Ok(None)) if current == destination_identity => {
                        let cleanup = sftp_cleanup_upload_stage(
                            sftp,
                            &stage_path,
                            stage_identity,
                            &temp_path,
                        )
                        .await
                        .err()
                        .map(|cleanup_error| {
                            format!("; staging cleanup also failed: {cleanup_error}")
                        })
                        .unwrap_or_default();
                        return Err(format!(
                            "Failed to create recovery backup for remote upload '{}': {}{}",
                            remote_path, rename_error, cleanup
                        ));
                    }
                    _ => {
                        return Err(format!(
                            "Failed to create a verifiable recovery backup for remote upload '{}': {}. Inspect destination '{}' and possible recovery path '{}'; private staged data remains at '{}'",
                            remote_path, rename_error, remote_path, backup_path, stage_path
                        ));
                    }
                }
            }

            // Re-hash the renamed object before publication. This closes the
            // check/rename window: an object swapped in after the pre-check is
            // restored instead of being silently overwritten.
            let backup_version = sftp_hash_stable_regular_file(sftp, &backup_path, None).await;
            if !matches!(&backup_version, Ok(version) if version == &expected_version) {
                let error = match backup_version {
                    Ok(_) => remote_version_mismatch(&remote_path),
                    Err(error) => error,
                };
                let rollback = sftp_restore_upload_backup(
                    sftp,
                    &remote_path,
                    &backup_path,
                    destination_identity,
                )
                .await;
                return Err(match rollback {
                    Ok(()) => format!(
                        "Remote upload backup did not match the open-time version ({error}); the destination was restored. Private staged data remains at '{stage_path}'"
                    ),
                    Err(rollback_error) => format!(
                        "Remote upload backup did not match the open-time version ({error}); {rollback_error}. Private staged data remains at '{stage_path}'"
                    ),
                });
            }
            let backup = Some((backup_path, expected_version.clone()));

            let publish_result =
                sftp_publish_upload_noreplace(sftp, &temp_path, &remote_path).await;
            let publication_verified = sftp_verify_regular_file_path(
                sftp,
                &remote_path,
                payload_identity,
            )
            .await;
            if let Err(publish_error) = publish_result {
                if let Err(verify_error) = publication_verified {
                    if let Some((backup_path, expected)) = &backup {
                        match sftp_existing_destination_identity(sftp, &remote_path).await {
                            Ok(None) => {
                                let rollback = sftp_restore_upload_backup(
                                    sftp,
                                    &remote_path,
                                    backup_path,
                                    expected.identity,
                                )
                                .await;
                                let cleanup = if rollback.is_ok() {
                                    sftp_cleanup_upload_stage(
                                        sftp,
                                        &stage_path,
                                        stage_identity,
                                        &temp_path,
                                    )
                                    .await
                                    .err()
                                    .map(|error| {
                                        format!("; staging cleanup also failed: {error}")
                                    })
                                    .unwrap_or_default()
                                } else {
                                    String::new()
                                };
                                return Err(match rollback {
                                    Ok(()) => format!(
                                        "Failed to publish replacement for '{}': {}. The original destination was restored{}",
                                        remote_path, publish_error, cleanup
                                    ),
                                    Err(rollback_error) => format!(
                                        "Failed to publish replacement for '{}': {}; {}. Private staged data remains at '{}'",
                                        remote_path, publish_error, rollback_error, stage_path
                                    ),
                                });
                            }
                            Ok(Some(_)) | Err(_) => {
                                return Err(format!(
                                    "Remote upload publish response was ambiguous for '{}': {}; publication verification also failed: {}. The original remains at recovery path '{}' and private staged data remains at '{}'",
                                    remote_path, publish_error, verify_error, backup_path, stage_path
                                ));
                            }
                        }
                    }

                    let cleanup = match sftp_path_exists_no_follow(sftp, &remote_path).await {
                        Ok(false) => sftp_cleanup_upload_stage(
                            sftp,
                            &stage_path,
                            stage_identity,
                            &temp_path,
                        )
                        .await
                        .err()
                        .map(|error| format!("; staging cleanup also failed: {error}"))
                        .unwrap_or_default(),
                        _ => format!(
                            "; private staged data was preserved at '{}' because the server state is ambiguous",
                            stage_path
                        ),
                    };
                    return Err(format!(
                        "Failed to publish remote upload '{}': {}; publication verification also failed: {}{}",
                        remote_path, publish_error, verify_error, cleanup
                    ));
                }
                // A failed SFTP response may still follow a completed server
                // operation. Exact payload identity at the destination is the
                // authoritative success observation.
            } else if let Err(verify_error) = publication_verified {
                let recovery = backup
                    .as_ref()
                    .map(|(path, _)| format!(" The original remains at recovery path '{path}'."))
                    .unwrap_or_default();
                return Err(format!(
                    "Upload publication at '{}' could not be verified: {}.{} Private staged data remains at '{}'",
                    remote_path, verify_error, recovery, stage_path
                ));
            }

            // Cleanup happens after publication and therefore cannot turn a
            // verified content commit back into a failed save. Accumulate its
            // warning, finish the mode/identity checks, and report a committed
            // outcome so the editor will not retry the same save.
            let mut commit_warnings = Vec::new();
            if let Err(error) = sftp_cleanup_upload_stage(
                sftp,
                &stage_path,
                stage_identity,
                &temp_path,
            )
            .await
            {
                let recovery = backup
                    .as_ref()
                    .map(|(path, _)| format!("; the original remains at recovery path '{path}'"))
                    .unwrap_or_default();
                commit_warnings.push(format!(
                    "verified staging cleanup failed for '{}': {}{}",
                    stage_path, error, recovery
                ));
            }

            let final_permissions = safe_replacement_permissions(
                destination_identity.permissions,
            );
            let mode_request_error = if final_permissions != payload_identity.permissions {
                let mut attrs = russh_sftp::protocol::FileAttributes::empty();
                attrs.permissions = Some(final_permissions & 0o0777);
                sftp
                    .set_metadata(&remote_path, attrs)
                    .await
                    .err()
                    .map(|error| error.to_string())
            } else {
                None
            };
            let metadata_verified = if let Err(error) = sftp_verify_published_file(
                sftp,
                &remote_path,
                payload_identity,
                final_permissions,
            )
            .await
            {
                let request = mode_request_error
                    .as_deref()
                    .map(|request_error| {
                        format!("; the mode restoration request also returned: {request_error}")
                    })
                    .unwrap_or_default();
                commit_warnings.push(format!(
                    "replacement content was installed at '{}', but its previous mode/identity could not be restored and verified: {}{}",
                    remote_path, error, request
                ));
                false
            } else {
                true
            };

            let committed_version = match sftp_hash_stable_regular_file(
                sftp,
                &remote_path,
                None,
            )
            .await
            {
                Ok(version) if version.sha256 == payload_sha256 => version,
                Ok(_) => {
                    let recovery = backup
                        .as_ref()
                        .map(|(path, _)| {
                            format!(" The original remains at recovery path '{path}'.")
                        })
                        .unwrap_or_default();
                    return Err(format!(
                        "Replacement at '{}' did not match the staged content hash.{}",
                        remote_path, recovery
                    ));
                }
                Err(error) => {
                    let recovery = backup
                        .as_ref()
                        .map(|(path, _)| {
                            format!(" The original remains at recovery path '{path}'.")
                        })
                        .unwrap_or_default();
                    return Err(format!(
                        "Replacement at '{}' could not be content-verified: {}.{}",
                        remote_path, error, recovery
                    ));
                }
            };

            if let Some(warning) = remote_ownership_change_warning(
                &remote_path,
                destination_identity,
                committed_version.identity,
            ) {
                commit_warnings.push(warning);
            }

            if let Some((backup_path, expected)) = backup {
                if !metadata_verified {
                    commit_warnings.push(format!(
                        "the previous file was preserved at recovery path '{}' because replacement metadata was not verified",
                        backup_path
                    ));
                } else {
                    match sftp_verify_staging_parent_identity(sftp, &parent, parent_identity).await {
                        Err(error) => commit_warnings.push(format!(
                            "recovery backup was preserved at '{}' because its parent could not be revalidated: {}",
                            backup_path, error
                        )),
                        Ok(()) => {
                            match sftp_hash_stable_regular_file(sftp, &backup_path, None).await {
                                Ok(version) if version == expected => {
                                    if let Err(error) = sftp.remove_file(&backup_path).await {
                                        commit_warnings.push(format!(
                                            "the previous file may remain at recovery path '{}': {}",
                                            backup_path, error
                                        ));
                                    }
                                }
                                Ok(_) => commit_warnings.push(format!(
                                    "recovery backup '{}' no longer matches the open-time version and was preserved",
                                    backup_path
                                )),
                                Err(error) => commit_warnings.push(format!(
                                    "recovery backup '{}' changed or could not be verified and was preserved: {}",
                                    backup_path, error
                                )),
                            }
                        }
                    }
                }
            }

            Ok(completed_upload_outcome(
                total,
                committed_version,
                commit_warnings,
            ))
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
    let at_pos = input.find('@')?;
    let user = input[..at_pos].to_string();
    if user.is_empty()
        || user
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '@' | ':' | '[' | ']' | '/' | '\\'))
    {
        return None;
    }

    let after_at = &input[at_pos + 1..];
    let (host, suffix) = if let Some(bracketed) = after_at.strip_prefix('[') {
        let closing = bracketed.find(']')?;
        let host = &bracketed[..closing];
        let suffix = bracketed[closing + 1..].strip_prefix(':')?;
        if host.is_empty()
            || host
                .chars()
                .any(|ch| ch.is_whitespace() || matches!(ch, '[' | ']' | '@' | '/' | '\\'))
        {
            return None;
        }
        (host.to_string(), suffix)
    } else {
        let (host, suffix) = after_at.split_once(':')?;
        if host.is_empty()
            || host
                .chars()
                .any(|ch| ch.is_whitespace() || matches!(ch, '@' | '[' | ']' | '/' | '\\'))
        {
            return None;
        }
        (host.to_string(), suffix)
    };

    let (port, path) = if let Some((candidate, remainder)) = suffix.split_once(':') {
        if !candidate.is_empty() && candidate.bytes().all(|byte| byte.is_ascii_digit()) {
            (candidate.parse::<u16>().ok()?, remainder)
        } else {
            (22, suffix)
        }
    } else {
        (22, suffix)
    };

    let path = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
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
    format_remote_display_parts(&profile.user, &profile.host, profile.port, path)
}

pub fn format_remote_display_parts(user: &str, host: &str, port: u16, path: &str) -> String {
    // Invalid stored values are preserved verbatim for diagnostics; valid
    // bracketed and unbracketed IPv6 forms are both rendered with one pair.
    let canonical_host = canonical_remote_host(host).unwrap_or(host);
    let host = if canonical_host.contains(':') {
        format!("[{canonical_host}]")
    } else {
        canonical_host.to_string()
    };
    let normalized_path = path.replace('\\', "/");
    let path = if normalized_path.starts_with('/') {
        normalized_path
    } else {
        format!("/{normalized_path}")
    };
    if port != 22 {
        format!("{user}@{host}:{port}:{path}")
    } else {
        format!("{user}@{host}:{path}")
    }
}

/// Find matching profile from profiles list by user, host, port
pub fn find_matching_profile<'a>(
    profiles: &'a [RemoteProfile],
    user: &str,
    host: &str,
    port: u16,
) -> Option<&'a RemoteProfile> {
    let host = canonical_remote_host(host).ok()?;
    profiles.iter().find(|profile| {
        profile.user == user
            && canonical_remote_host(&profile.host).ok() == Some(host)
            && profile.port == port
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_upload_sidecars_stay_in_destination_directory() {
        assert_eq!(
            remote_upload_sidecar_path("/home/user/report.txt", "upload", 42).unwrap(),
            format!(
                "/home/user/.cokacdir-upload-{}-000000000000002a",
                std::process::id()
            )
        );
        assert_eq!(
            remote_upload_sidecar_path("report.txt", "backup", 1).unwrap(),
            format!(".cokacdir-backup-{}-0000000000000001", std::process::id())
        );
        assert!(remote_upload_sidecar_path("/", "upload", 1).is_err());
    }

    #[test]
    fn remote_staging_identity_requires_private_owned_objects() {
        let mut directory = russh_sftp::protocol::FileAttributes::empty();
        directory.permissions = Some(0o040700);
        directory.uid = Some(1000);
        directory.gid = Some(1000);
        assert_eq!(
            remote_private_directory_identity("stage", &directory).unwrap(),
            RemotePrivateDirectoryIdentity { uid: 1000 }
        );

        directory.permissions = Some(0o040770);
        assert!(remote_private_directory_identity("stage", &directory).is_err());
        directory.permissions = Some(0o040700);
        directory.uid = None;
        assert!(remote_private_directory_identity("stage", &directory).is_err());

        let mut file = russh_sftp::protocol::FileAttributes::empty();
        file.permissions = Some(0o100600);
        file.uid = Some(1000);
        file.gid = Some(1000);
        file.size = Some(12);
        assert_eq!(
            remote_regular_file_identity("payload", &file).unwrap(),
            RemoteRegularFileIdentity {
                uid: 1000,
                gid: Some(1000),
                size: 12,
                permissions: 0o100600,
            }
        );
        file.permissions = Some(0o100606);
        assert!(remote_regular_file_identity("payload", &file).is_err());
    }

    #[test]
    fn current_lstat_symlink_is_never_treated_as_stale_listed_directory() {
        let mut listed_directory = russh_sftp::protocol::FileAttributes::empty();
        listed_directory.permissions = Some(0o040755);
        assert!(remote_removal_is_directory(&listed_directory));

        // Simulate the same name being replaced by a symlink after READDIR.
        // The deletion path uses this fresh LSTAT result and removes the link
        // itself instead of calling READDIR through it.
        let mut current_symlink = russh_sftp::protocol::FileAttributes::empty();
        current_symlink.permissions = Some(0o120777);
        assert!(!remote_removal_is_directory(&current_symlink));
    }

    #[test]
    fn remote_empty_file_creation_is_exclusive_and_never_truncates() {
        let flags = remote_create_file_flags();
        assert!(flags.contains(OpenFlags::WRITE));
        assert!(flags.contains(OpenFlags::CREATE));
        assert!(flags.contains(OpenFlags::EXCLUDE));
        assert!(!flags.contains(OpenFlags::TRUNCATE));
    }

    #[test]
    fn remote_editor_identity_rejects_special_files_and_tracks_changes() {
        let mut file = russh_sftp::protocol::FileAttributes::empty();
        file.permissions = Some(0o100640);
        file.uid = Some(1000);
        file.gid = Some(100);
        file.size = Some(12);
        file.mtime = Some(1234);
        let identity = remote_destination_identity("document", &file).unwrap();
        assert_eq!(
            identity,
            RemoteDestinationIdentity {
                uid: 1000,
                gid: Some(100),
                size: 12,
                permissions: 0o100640,
                mtime: Some(1234),
            }
        );

        file.mtime = Some(1235);
        assert_ne!(
            remote_destination_identity("document", &file).unwrap(),
            identity
        );
        file.permissions = Some(0o120777);
        assert!(remote_destination_identity("document", &file).is_err());
        file.permissions = Some(0o040700);
        assert!(remote_destination_identity("document", &file).is_err());
    }

    #[test]
    fn editor_replacement_does_not_restore_special_permission_bits() {
        assert_eq!(safe_replacement_permissions(0o104755), 0o100755);
        assert_eq!(safe_replacement_permissions(0o102640), 0o100640);
        assert_eq!(safe_replacement_permissions(0o100600), 0o100600);
    }

    #[test]
    fn completed_upload_distinguishes_post_commit_warning_from_failure() {
        let version = RemoteFileVersion::for_test(7);
        assert_eq!(
            completed_upload_outcome(12, version.clone(), Vec::new()),
            UploadFileOutcome::Complete {
                bytes: 12,
                version: version.clone(),
            }
        );
        assert_eq!(
            completed_upload_outcome(
                12,
                version.clone(),
                vec![
                    "mode could not be restored".to_string(),
                    "backup remains".to_string()
                ]
            ),
            UploadFileOutcome::CommittedWithWarning {
                bytes: 12,
                version,
                warning: "mode could not be restored; backup remains".to_string(),
            }
        );
    }

    #[test]
    fn remote_ownership_change_is_reported_after_commit() {
        let original = RemoteDestinationIdentity {
            uid: 1000,
            gid: Some(100),
            size: 4,
            permissions: 0o100640,
            mtime: Some(1),
        };
        let mut committed = original;

        assert!(remote_ownership_change_warning("document", original, committed).is_none());

        committed.gid = Some(200);
        let warning = remote_ownership_change_warning("document", original, committed)
            .expect("changed ownership must not be silent");
        assert!(warning.contains("ownership changed"));
        assert!(warning.contains("gid Some(100)"));
        assert!(warning.contains("gid Some(200)"));
    }

    #[test]
    fn content_hash_distinguishes_versions_with_identical_attributes() {
        let first = RemoteFileVersion::for_test(7);
        let mut second = first.clone();
        second.sha256 = [8; 32];
        assert_ne!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn local_upload_snapshot_keeps_generation_bytes_after_path_replacement() {
        use std::io::{Read, Seek};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("editor-cache");
        let replacement = directory.path().join("next-generation");
        std::fs::write(&path, b"generation-one").unwrap();
        let mut snapshot = LocalUploadSnapshot::open(&path).unwrap();
        std::fs::write(&replacement, b"generation-two").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let next_snapshot = LocalUploadSnapshot::open(&path).unwrap();

        snapshot.file.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        snapshot.file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"generation-one");
        assert_eq!(std::fs::read(path).unwrap(), b"generation-two");
        assert_ne!(snapshot.sha256, next_snapshot.sha256);
    }

    #[test]
    fn failed_download_preserves_existing_destination() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("create temp dir");
        let destination = dir.path().join("existing.txt");
        std::fs::write(&destination, b"original").expect("seed destination");

        {
            let mut guard = PartialFileGuard::create(destination.display().to_string())
                .expect("create staged download");
            guard.writer().write_all(b"partial").expect("stage bytes");
            // Simulate cancellation / transfer error by dropping without commit.
        }

        assert_eq!(
            std::fs::read(&destination).expect("read preserved destination"),
            b"original"
        );
    }

    #[test]
    fn completed_download_refuses_existing_destination_at_commit() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("create temp dir");
        let destination = dir.path().join("existing.txt");
        std::fs::write(&destination, b"original").expect("seed destination");

        let mut guard = PartialFileGuard::create(destination.display().to_string())
            .expect("create staged download");
        let temp_path = guard.temp_path.clone();
        guard.writer().write_all(b"complete").expect("stage bytes");
        assert_eq!(
            std::fs::read(&destination).expect("read original before commit"),
            b"original"
        );
        let error = guard.commit().expect_err("existing destination must win");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);

        assert_eq!(
            std::fs::read(&destination).expect("read installed destination"),
            b"original"
        );
        assert!(
            !temp_path.exists(),
            "failed commit must clean its staging file"
        );
    }

    #[test]
    fn completed_cache_download_atomically_replaces_existing_regular_file() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("create temp dir");
        let destination = dir.path().join("existing.txt");
        std::fs::write(&destination, b"old-cache").expect("seed cache");

        let mut guard = PartialFileGuard::create(destination.display().to_string())
            .expect("create staged download");
        let staged_identity = guard.file_identity;
        guard
            .writer()
            .write_all(b"fresh-remote-data")
            .expect("stage bytes");
        guard
            .commit_replacing_regular_destination()
            .expect("replace stale cache");

        assert_eq!(
            std::fs::read(&destination).expect("read refreshed cache"),
            b"fresh-remote-data"
        );
        assert_eq!(
            crate::services::file_ops::stable_path_identity(&destination)
                .expect("identify refreshed cache"),
            staged_identity
        );
    }

    #[test]
    fn completed_download_publishes_new_destination() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("create temp dir");
        let destination = dir.path().join("new.txt");
        let mut guard = PartialFileGuard::create(destination.display().to_string())
            .expect("create staged download");
        let temp_path = guard.temp_path.clone();
        guard.writer().write_all(b"complete").expect("stage bytes");
        guard.commit().expect("commit staged download");
        assert_eq!(
            std::fs::read(destination).expect("read destination"),
            b"complete"
        );
        assert!(
            !temp_path.exists(),
            "successful commit must consume staging file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn completed_download_rejects_replaced_staging_path_without_deleting_it() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("create temp dir");
        let destination = dir.path().join("new.txt");
        let mut guard = PartialFileGuard::create(destination.display().to_string())
            .expect("create staged download");
        guard.writer().write_all(b"owned").expect("stage bytes");
        let replacement = guard.temp_path.clone();
        let retained_owned = guard.staging_dir.join("owned-retained.tmp");
        std::fs::rename(&replacement, &retained_owned).expect("move owned staging file");
        std::fs::write(&replacement, b"unowned replacement").expect("replace staging path");

        let error = guard
            .commit()
            .expect_err("replacement must not be published");
        assert!(error.to_string().contains("replaced"));
        assert_eq!(
            std::fs::read(&replacement).expect("replacement must be preserved"),
            b"unowned replacement"
        );
        assert_eq!(
            std::fs::read(&retained_owned).expect("owned file must be preserved"),
            b"owned"
        );
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn completed_download_refuses_symlink_destination() {
        use std::io::Write;
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("create temp dir");
        let target = dir.path().join("target.txt");
        let destination = dir.path().join("download.txt");
        std::fs::write(&target, b"target-data").expect("seed target");
        symlink(&target, &destination).expect("create destination symlink");

        let mut guard = PartialFileGuard::create(destination.display().to_string())
            .expect("create staged download");
        guard
            .writer()
            .write_all(b"downloaded")
            .expect("stage bytes");
        let error = guard.commit().expect_err("symlink destination must win");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);

        assert_eq!(std::fs::read(&target).expect("read target"), b"target-data");
        assert!(std::fs::symlink_metadata(&destination)
            .expect("destination metadata")
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn completed_cache_download_refuses_symlink_without_touching_target() {
        use std::io::Write;
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("create temp dir");
        let target = dir.path().join("target.txt");
        let destination = dir.path().join("download.txt");
        std::fs::write(&target, b"target-data").expect("seed target");
        symlink(&target, &destination).expect("create destination symlink");

        let mut guard = PartialFileGuard::create(destination.display().to_string())
            .expect("create staged download");
        guard
            .writer()
            .write_all(b"downloaded")
            .expect("stage bytes");
        let error = guard
            .commit_replacing_regular_destination()
            .expect_err("cache symlink must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

        assert_eq!(std::fs::read(&target).expect("read target"), b"target-data");
        assert!(std::fs::symlink_metadata(&destination)
            .expect("destination metadata")
            .file_type()
            .is_symlink());
    }

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
        assert!(parse_remote_path("user@[2001:db8::1:/path").is_none());
        assert!(parse_remote_path("user@[2001:db8::1]oops:/path").is_none());
        assert!(parse_remote_path("bad@user@host:/path").is_none());
    }

    #[test]
    fn ipv6_remote_display_round_trips_default_and_custom_ports() {
        for (stored_host, canonical_host, port, expected) in [
            ("2001:db8::1", "2001:db8::1", 22, "user@[2001:db8::1]:/work"),
            (
                "[2001:db8::1]",
                "2001:db8::1",
                22,
                "user@[2001:db8::1]:/work",
            ),
            (
                "fe80::1%eth0",
                "fe80::1%eth0",
                2222,
                "user@[fe80::1%eth0]:2222:/work",
            ),
        ] {
            let profile = RemoteProfile {
                name: "ipv6".to_string(),
                host: stored_host.to_string(),
                port,
                user: "user".to_string(),
                auth: RemoteAuth::Password {
                    password: "secret".to_string(),
                },
                default_path: "/".to_string(),
            };
            let display = format_remote_display(&profile, "/work");
            assert_eq!(display, expected);
            assert_eq!(
                parse_remote_path(&display),
                Some((
                    "user".to_string(),
                    canonical_host.to_string(),
                    port,
                    "/work".to_string()
                ))
            );
        }
    }

    #[test]
    fn canonical_remote_host_strips_one_valid_ipv6_bracket_pair() {
        assert_eq!(canonical_remote_host("::1").unwrap(), "::1");
        assert_eq!(canonical_remote_host("[::1]").unwrap(), "::1");
        assert_eq!(
            canonical_remote_host("[fe80::1%eth0]").unwrap(),
            "fe80::1%eth0"
        );
        assert!(remote_hosts_equal("::1", "[::1]"));

        for malformed in ["[::1", "::1]", "[[::1]]", "[hostname]", ""] {
            assert!(canonical_remote_host(malformed).is_err(), "{malformed:?}");
        }

        let profile = RemoteProfile {
            name: "ipv6".to_string(),
            host: "[::1]".to_string(),
            port: 22,
            user: "user".to_string(),
            auth: RemoteAuth::Password {
                password: "secret".to_string(),
            },
            default_path: "/".to_string(),
        };
        assert_eq!(SshHandler::new(&profile).unwrap().host, "::1");
    }

    #[test]
    fn remote_auth_debug_redacts_secrets() {
        let password = format!(
            "{:?}",
            RemoteAuth::Password {
                password: "password-secret".to_string()
            }
        );
        assert!(!password.contains("password-secret"));
        assert!(password.contains("<redacted>"));

        let key = format!(
            "{:?}",
            RemoteAuth::KeyFile {
                path: "/home/user/key".to_string(),
                passphrase: Some("passphrase-secret".to_string())
            }
        );
        assert!(key.contains("/home/user/key"));
        assert!(!key.contains("passphrase-secret"));
        assert!(key.contains("<redacted>"));
    }

    #[test]
    fn test_format_remote_permissions() {
        assert_eq!(format_remote_permissions(0o755), "rwxr-xr-x");
        assert_eq!(format_remote_permissions(0o644), "rw-r--r--");
    }
}
