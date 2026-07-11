use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc::Sender, Arc};

use russh::{client, ChannelMsg, Disconnect};
use tokio::runtime::Runtime;

use crate::services::file_ops::{
    copy_path_authorized, create_private_quarantine_directory, open_directory_for_read,
    open_regular_file_no_follow, remove_file_by_identity, rename_noreplace, stable_file_identity,
    stable_path_identity, verify_directory_authorization, verify_path_authorization,
    DirectoryAuthorization, PathAuthorization, ProgressMessage, StablePathIdentity,
};
use crate::services::remote::{
    expand_tilde, load_supported_secret_key, RemoteAuth, RemotePrivateDirectoryIdentity,
    RemoteProfile, RemoteStagingParentIdentity, SftpSession, SshHandler,
};

const MAX_COMMAND_OUTPUT: usize = 64 * 1024;
const MAX_PROGRESS_LINE: usize = 64 * 1024;

fn append_bounded_tail(output: &mut Vec<u8>, data: &[u8], limit: usize) {
    if data.len() >= limit {
        output.clear();
        output.extend_from_slice(&data[data.len() - limit..]);
        return;
    }
    let overflow = output
        .len()
        .saturating_add(data.len())
        .saturating_sub(limit);
    if overflow > 0 {
        output.drain(..overflow);
    }
    output.extend_from_slice(data);
}

fn read_bounded_tail(mut reader: impl Read, limit: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => append_bounded_tail(&mut output, &chunk[..n], limit),
        }
    }
    output
}

fn transfer_name(path: &Path) -> Result<String, String> {
    let mut components = path.components();
    let Some(Component::Normal(name)) = components.next() else {
        return Err(format!("Invalid transfer item name: '{}'", path.display()));
    };
    if components.next().is_some() || name.is_empty() {
        return Err(format!("Invalid transfer item name: '{}'", path.display()));
    }
    name.to_str().map(ToOwned::to_owned).ok_or_else(|| {
        format!(
            "Transfer item name is not valid UTF-8: '{}'",
            path.display()
        )
    })
}

fn remote_join(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", directory.trim_end_matches('/'))
    }
}

fn remote_stage_dir(target: &str) -> String {
    remote_join(
        target,
        &format!(
            ".cokacdir-transfer-{}-{:032x}",
            std::process::id(),
            rand::random::<u128>()
        ),
    )
}

fn validate_rsync_profile(profile: &RemoteProfile) -> Result<(), String> {
    let valid_user = !profile.user.is_empty()
        && !profile.user.starts_with('-')
        && profile
            .user
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
    if !valid_user {
        return Err(
            "SSH user contains characters that cannot be passed safely to rsync".to_string(),
        );
    }
    let host = crate::services::remote::canonical_remote_host(&profile.host).map_err(|_| {
        "SSH host contains characters that cannot be passed safely to rsync".to_string()
    })?;
    let valid_host = !host.starts_with('-')
        && host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':' | '%' | '_'));
    if !valid_host {
        return Err(
            "SSH host contains characters that cannot be passed safely to rsync".to_string(),
        );
    }
    Ok(())
}

struct LocalStageDir {
    path: PathBuf,
    identity: StablePathIdentity,
    parent_path: PathBuf,
    parent_identity: StablePathIdentity,
    directory_guard: Option<std::fs::File>,
    parent_guard: std::fs::File,
    cleaned: bool,
}

impl LocalStageDir {
    fn create(target: &Path) -> Result<Self, String> {
        let (parent_guard, stable_parent_path, parent_metadata) = open_directory_for_read(target)
            .map_err(|error| {
            format!(
                "Failed to open transfer staging parent '{}': {}",
                target.display(),
                error
            )
        })?;
        verify_local_staging_parent(target, &parent_metadata)?;
        let parent_identity = stable_file_identity(&parent_guard).map_err(|error| {
            format!(
                "Failed to identify transfer staging parent '{}': {}",
                target.display(),
                error
            )
        })?;

        let stable_created = create_private_quarantine_directory(&stable_parent_path, "transfer")
            .map_err(|error| {
            format!(
                "Failed to create private transfer staging directory in '{}': {}",
                target.display(),
                error
            )
        })?;
        let result = (|| {
            let name = stable_created.file_name().ok_or_else(|| {
                "Private transfer staging directory has no final path component".to_string()
            })?;
            let path = target.join(name);
            if stable_path_identity(target).map_err(|error| {
                format!(
                    "Failed to re-identify transfer staging parent '{}': {}",
                    target.display(),
                    error
                )
            })? != parent_identity
            {
                return Err(format!(
                    "Transfer staging parent changed while the private directory was created: '{}'",
                    target.display()
                ));
            }
            let (directory_guard, _, stage_metadata) =
                open_directory_for_read(&path).map_err(|error| {
                    format!(
                        "Failed to open private transfer staging directory '{}': {}",
                        path.display(),
                        error
                    )
                })?;
            verify_local_private_stage(&path, &stage_metadata)?;
            let identity = stable_file_identity(&directory_guard).map_err(|error| {
                format!(
                    "Failed to identify private transfer staging directory '{}': {}",
                    path.display(),
                    error
                )
            })?;
            if stable_path_identity(&path).map_err(|error| {
                format!(
                    "Failed to bind private transfer staging path '{}': {}",
                    path.display(),
                    error
                )
            })? != identity
            {
                return Err(format!(
                    "Private transfer staging path changed while it was opened: '{}'",
                    path.display()
                ));
            }
            Ok((path, identity, directory_guard))
        })();
        let (path, identity, directory_guard) = match result {
            Ok(created) => created,
            Err(error) => {
                let _ = std::fs::remove_dir(&stable_created);
                return Err(error);
            }
        };
        Ok(Self {
            path,
            identity,
            parent_path: target.to_path_buf(),
            parent_identity,
            directory_guard: Some(directory_guard),
            parent_guard,
            cleaned: false,
        })
    }

    fn verify(&self) -> Result<(), String> {
        if stable_file_identity(&self.parent_guard)
            .map_err(|error| format!("Failed to identify held transfer staging parent: {error}"))?
            != self.parent_identity
            || stable_path_identity(&self.parent_path).map_err(|error| {
                format!(
                    "Failed to inspect transfer staging parent '{}': {}",
                    self.parent_path.display(),
                    error
                )
            })? != self.parent_identity
        {
            return Err(format!(
                "Transfer staging parent changed at '{}'; refusing to publish or delete staged data",
                self.parent_path.display()
            ));
        }
        let parent_metadata = std::fs::symlink_metadata(&self.parent_path).map_err(|error| {
            format!(
                "Failed to inspect transfer staging parent '{}': {}",
                self.parent_path.display(),
                error
            )
        })?;
        verify_local_staging_parent(&self.parent_path, &parent_metadata)?;

        if let Some(directory_guard) = self.directory_guard.as_ref() {
            if stable_file_identity(directory_guard).map_err(|error| {
                format!("Failed to identify held transfer staging directory: {error}")
            })? != self.identity
            {
                return Err(format!(
                    "Held transfer staging directory changed at '{}'; refusing to publish or delete staged data",
                    self.path.display()
                ));
            }
        }
        if stable_path_identity(&self.path).map_err(|error| {
            format!(
                "Failed to inspect private transfer staging path '{}': {}",
                self.path.display(),
                error
            )
        })? != self.identity
        {
            return Err(format!(
                "Private transfer staging path changed at '{}'; refusing to publish or delete staged data",
                self.path.display()
            ));
        }
        let metadata = std::fs::symlink_metadata(&self.path).map_err(|error| {
            format!(
                "Failed to inspect private transfer staging directory '{}': {}",
                self.path.display(),
                error
            )
        })?;
        verify_local_private_stage(&self.path, &metadata)
    }

    fn cleanup(mut self) -> Result<(), String> {
        self.verify()?;
        drop(self.directory_guard.take());
        if stable_path_identity(&self.path).map_err(|error| {
            format!(
                "Failed to re-identify transfer staging directory before cleanup '{}': {}",
                self.path.display(),
                error
            )
        })? != self.identity
        {
            return Err(format!(
                "Private transfer staging path changed before cleanup at '{}'; it was left untouched",
                self.path.display()
            ));
        }
        std::fs::remove_dir_all(&self.path).map_err(|error| {
            format!(
                "Failed to clean verified transfer staging directory '{}': {}",
                self.path.display(),
                error
            )
        })?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for LocalStageDir {
    fn drop(&mut self) {
        if self.cleaned || self.verify().is_err() {
            return;
        }
        drop(self.directory_guard.take());
        if stable_path_identity(&self.path).ok() == Some(self.identity)
            && std::fs::remove_dir_all(&self.path).is_ok()
        {
            self.cleaned = true;
        }
    }
}

fn verify_local_staging_parent(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "Transfer staging parent is not a real directory: '{}'",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            return Err(format!(
                "Transfer staging parent '{}' is group/world-writable without the sticky bit (mode {:04o}); refusing a replaceable staging path",
                path.display(),
                mode & 0o7777
            ));
        }
    }
    Ok(())
}

fn verify_local_private_stage(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "Private transfer staging path is not a real directory: '{}'",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o777 != 0o700 {
            return Err(format!(
                "Transfer staging directory '{}' is not private (mode {:04o})",
                path.display(),
                mode & 0o7777
            ));
        }
    }
    Ok(())
}

struct RemoteStageDir {
    session: SftpSession,
    path: String,
    identity: RemotePrivateDirectoryIdentity,
    parent_path: String,
    parent_identity: RemoteStagingParentIdentity,
    cleaned: bool,
}

impl RemoteStageDir {
    fn create(profile: &RemoteProfile, target: &str, names: &[String]) -> Result<Self, String> {
        let session = SftpSession::connect(profile)?;
        let parent_identity = session.staging_parent_identity(target)?;
        for name in names {
            let destination = remote_join(target, name);
            if session.path_exists_no_follow(&destination)? {
                return Err(format!("Destination already exists: '{destination}'"));
            }
        }

        for _ in 0..32 {
            let path = remote_stage_dir(target);
            if session.path_exists_no_follow(&path)? {
                continue;
            }
            match session.create_private_dir(&path) {
                Ok(identity) => {
                    return Ok(Self {
                        session,
                        path,
                        identity,
                        parent_path: target.to_string(),
                        parent_identity,
                        cleaned: false,
                    })
                }
                Err(error) => return Err(error),
            }
        }
        Err("Unable to allocate a unique remote transfer staging directory".to_string())
    }

    fn verify(&self) -> Result<(), String> {
        self.session
            .verify_staging_parent(&self.parent_path, self.parent_identity)?;
        self.session.verify_private_dir(&self.path, self.identity)
    }

    fn cleanup(mut self) -> Result<(), String> {
        self.verify()?;
        self.session.remove_private_dir(&self.path, self.identity)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for RemoteStageDir {
    fn drop(&mut self) {
        if !self.cleaned
            && self.verify().is_ok()
            && self
                .session
                .remove_private_dir(&self.path, self.identity)
                .is_ok()
        {
            self.cleaned = true;
        }
    }
}

fn cleanup_remote_stage(stage: RemoteStageDir) -> Result<(), String> {
    let path = stage.path.clone();
    stage.cleanup().map_err(|error| {
        format!(
            "Remote transfer staging cleanup failed at '{path}'; staged data may remain: {error}"
        )
    })
}

fn cleanup_local_stage(stage: LocalStageDir) -> Result<(), String> {
    let path = stage.path.clone();
    stage.cleanup().map_err(|error| {
        format!(
            "Local transfer staging cleanup failed at '{}'; staged data may remain: {error}",
            path.display()
        )
    })
}

fn finish_transfer_with_cleanup(
    transfer_result: Result<(), String>,
    cancelled: bool,
    cleanup_errors: Vec<String>,
    tx: &Sender<ProgressMessage>,
) -> Result<(), String> {
    if cleanup_errors.is_empty() {
        return transfer_result;
    }
    if transfer_result.is_ok() && !cancelled {
        return Err(cleanup_errors.join("; "));
    }
    for error in cleanup_errors {
        let _ = tx.send(ProgressMessage::Warning(String::new(), error));
    }
    transfer_result
}

fn rename_local_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    rename_noreplace(source, destination)
}

#[derive(Debug)]
struct LocalPublishFailure {
    message: String,
    committed: bool,
    cancelled: bool,
}

fn publication_is_cancelled(cancel_flag: &AtomicBool) -> bool {
    cancel_flag.load(Ordering::Relaxed)
}

fn sync_local_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn sync_local_entry_recursive(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        let (file, _) = open_regular_file_no_follow(path)?;
        let expected = stable_file_identity(&file)?;
        file.sync_all()?;
        if stable_path_identity(path)? != expected {
            return Err(std::io::Error::other(format!(
                "Local transfer staging file changed during sync: '{}'",
                path.display()
            )));
        }
        return Ok(());
    }
    if metadata.is_dir() {
        let (directory, stable_path, _) = open_directory_for_read(path)?;
        let expected = stable_file_identity(&directory)?;
        for entry in std::fs::read_dir(&stable_path)? {
            sync_local_entry_recursive(&entry?.path())?;
        }
        #[cfg(unix)]
        directory.sync_all()?;
        if stable_path_identity(path)? != expected {
            return Err(std::io::Error::other(format!(
                "Local transfer staging directory changed during sync: '{}'",
                path.display()
            )));
        }
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("Cannot publish special local entry: '{}'", path.display()),
    ))
}

fn publish_local_noreplace(
    source: &Path,
    destination: &Path,
    cancel_flag: &AtomicBool,
) -> Result<Vec<String>, LocalPublishFailure> {
    sync_local_entry_recursive(source).map_err(|error| LocalPublishFailure {
        message: format!(
            "Failed to make local staging data durable before publication '{}': {}",
            source.display(),
            error
        ),
        committed: false,
        cancelled: false,
    })?;
    let expected = stable_path_identity(source).map_err(|error| LocalPublishFailure {
        message: format!(
            "Failed to bind local staging data before publication '{}': {}",
            source.display(),
            error
        ),
        committed: false,
        cancelled: false,
    })?;
    if publication_is_cancelled(cancel_flag) {
        return Err(LocalPublishFailure {
            message: "Cancelled".to_string(),
            committed: false,
            cancelled: true,
        });
    }
    rename_local_noreplace(source, destination).map_err(|error| LocalPublishFailure {
        message: if error.kind() == std::io::ErrorKind::AlreadyExists
            || std::fs::symlink_metadata(destination).is_ok()
        {
            format!("Destination already exists: '{}'", destination.display())
        } else {
            format!(
                "Failed to publish '{}' without overwriting the destination: {}",
                destination.display(),
                error
            )
        },
        committed: false,
        cancelled: false,
    })?;

    if stable_path_identity(destination).ok() != Some(expected) {
        return Err(LocalPublishFailure {
            message: format!(
                "Local destination was committed at '{}', but the staged object could not be rebound; inspect it manually",
                destination.display()
            ),
            committed: true,
            cancelled: false,
        });
    }
    let mut warnings = Vec::new();
    if let Err(error) = sync_local_parent(destination) {
        warnings.push(format!(
            "Local destination was committed, but target-directory durability could not be confirmed for '{}': {}",
            destination.display(), error
        ));
    }
    if source.parent() != destination.parent() {
        if let Err(error) = sync_local_parent(source) {
            warnings.push(format!(
                "Local destination was committed, but staging-directory durability could not be confirmed for '{}': {}",
                source.display(), error
            ));
        }
    }
    Ok(warnings)
}

fn validate_local_source_authorizations(config: &TransferConfig) -> Result<(), String> {
    if config.direction != TransferDirection::LocalToRemote {
        return Ok(());
    }
    let source_base = Path::new(&config.source_base);
    if let Some(authorization) = config.local_source_directory_authorization.as_ref() {
        verify_directory_authorization(source_base, authorization, "Clipboard source directory")
            .map_err(|error| error.to_string())?;
    }
    for source_file in &config.source_files {
        let source = source_base.join(source_file);
        if let Some(authorization) = config.local_source_authorizations.get(&source) {
            verify_path_authorization(&source, authorization, "Clipboard source item")
                .map_err(|error| error.to_string())?;
        } else if !config.local_source_authorizations.is_empty() {
            return Err(format!(
                "Missing clipboard authorization for local source '{}'",
                source.display()
            ));
        }
    }
    Ok(())
}

fn prepare_authorized_local_snapshot(
    config: &TransferConfig,
    cancel_flag: &Arc<AtomicBool>,
    tx: &Sender<ProgressMessage>,
) -> Result<Option<LocalStageDir>, String> {
    if config.direction != TransferDirection::LocalToRemote
        || (config.local_source_authorizations.is_empty()
            && config.local_source_directory_authorization.is_none())
    {
        return Ok(None);
    }

    let home = dirs::home_dir().ok_or("Failed to locate a private local snapshot root")?;
    let base_tmp = home.join(".cokacdir").join("tmp");
    std::fs::create_dir_all(&base_tmp).map_err(|error| {
        format!(
            "Failed to create local transfer snapshot root '{}': {}",
            base_tmp.display(),
            error
        )
    })?;
    let stage = LocalStageDir::create(&base_tmp)?;
    let source_base = Path::new(&config.source_base);

    for source_file in &config.source_files {
        if cancel_flag.load(Ordering::Relaxed) {
            let cleanup = stage
                .cleanup()
                .err()
                .map(|error| format!("; snapshot cleanup also failed: {error}"))
                .unwrap_or_default();
            return Err(format!("Cancelled{cleanup}"));
        }
        if let Some(directory) = config.local_source_directory_authorization.as_ref() {
            verify_directory_authorization(source_base, directory, "Clipboard source directory")
                .map_err(|error| error.to_string())?;
        }
        let source = source_base.join(source_file);
        let authorization = config
            .local_source_authorizations
            .get(&source)
            .ok_or_else(|| {
                format!(
                    "Missing clipboard authorization for local source '{}'",
                    source.display()
                )
            })?;
        let name = transfer_name(source_file)?;
        let destination = stage.path.join(name);
        match copy_path_authorized(&source, &destination, authorization, cancel_flag, tx) {
            Ok(warnings) => {
                for warning in warnings {
                    let _ = tx.send(ProgressMessage::Warning(
                        source_file.display().to_string(),
                        format!("Local snapshot: {warning}"),
                    ));
                }
            }
            Err(error) => {
                let cleanup = stage
                    .cleanup()
                    .err()
                    .map(|cleanup_error| format!("; snapshot cleanup also failed: {cleanup_error}"))
                    .unwrap_or_default();
                return Err(format!(
                    "Failed to create a verified local snapshot for '{}': {}{}",
                    source.display(),
                    error,
                    cleanup
                ));
            }
        }
        stage.verify()?;
    }

    Ok(Some(stage))
}

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
    pub local_source_authorizations: HashMap<PathBuf, PathAuthorization>,
    pub local_source_directory_authorization: Option<DirectoryAuthorization>,
    pub local_target_directory_authorization: Option<DirectoryAuthorization>,
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
    crate::services::remote::remote_hosts_equal(&a.host, &b.host)
        && a.port == b.port
        && a.user == b.user
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

            let connect_host = crate::services::remote::canonical_remote_host(&profile.host)?;
            let handler = SshHandler::new(&profile)?;
            let mut ssh = client::connect(Arc::new(config), (connect_host, profile.port), handler)
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
                            append_bounded_tail(&mut stderr_bytes, &data, MAX_COMMAND_OUTPUT);
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
        ssh_cmd.push_str(" -o IdentitiesOnly=yes");
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
    let canonical_host =
        crate::services::remote::canonical_remote_host(&profile.host).unwrap_or(&profile.host);
    let host = if canonical_host.contains(':') {
        format!("[{canonical_host}]")
    } else {
        canonical_host.to_string()
    };
    if rsync_uses_modern_arg_protection() {
        // Modern rsync escapes remote args itself; adding our own quotes would make them
        // literal characters in the path. Pass the path verbatim.
        format!("{}@{}:{}", profile.user, host, path)
    } else {
        // Older rsync passes the arg through the remote shell: single-quote to prevent
        // shell interpretation. Only single quotes inside the path need escaping: ' → '\''
        let escaped = path.replace('\'', "'\\''");
        format!("{}@{}:'{}'", profile.user, host, escaped)
    }
}

/// RAII guard for the temporary SSH_ASKPASS script: removes the file on drop
/// so the password leak window survives panics and early returns.
struct AskpassGuard {
    path: PathBuf,
    identity: StablePathIdentity,
}

impl AskpassGuard {
    fn new(password: &str) -> Result<Self, String> {
        let (path, identity) = create_askpass_script(password)?;
        Ok(Self { path, identity })
    }

    fn path(&self) -> Result<&std::path::Path, String> {
        if stable_path_identity(&self.path).map_err(|error| {
            format!(
                "Failed to re-identify SSH_ASKPASS script '{}': {error}",
                self.path.display()
            )
        })? != self.identity
        {
            return Err(format!(
                "SSH_ASKPASS script changed before execution: '{}'",
                self.path.display()
            ));
        }
        Ok(&self.path)
    }
}

impl Drop for AskpassGuard {
    fn drop(&mut self) {
        let _ = remove_file_by_identity(&self.path, self.identity);
    }
}

/// Create a temporary SSH_ASKPASS script for password authentication.
/// Prefer `AskpassGuard::new` over calling this directly; the guard ensures
/// the script is removed even on panic / early return.
fn create_askpass_script(password: &str) -> Result<(PathBuf, StablePathIdentity), String> {
    let content = askpass_script_content(password)?;
    let home = dirs::home_dir().ok_or_else(|| "Failed to get home directory".to_string())?;
    let tmp_dir = home.join(".cokacdir").join("tmp");

    #[cfg(unix)]
    {
        create_askpass_script_in_dir(&tmp_dir, &content)
    }

    #[cfg(windows)]
    {
        let _ = tmp_dir;
        Err("SSH_ASKPASS fallback on Windows would require writing a plaintext script; install sshpass for password authentication or use an unencrypted SSH key.".to_string())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (tmp_dir, content);
        Err("SSH_ASKPASS fallback is unsupported on this platform".to_string())
    }
}

#[cfg(unix)]
fn create_askpass_script_in_dir(
    tmp_dir: &Path,
    content: &str,
) -> Result<(PathBuf, StablePathIdentity), String> {
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("Failed to create tmp dir: {}", e))?;
    let (directory, stable_directory, metadata) = open_directory_for_read(tmp_dir)
        .map_err(|error| format!("Failed to open private askpass directory: {error}"))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "Askpass directory is not a real directory: '{}'",
            tmp_dir.display()
        ));
    }
    use std::os::unix::fs::PermissionsExt;
    directory
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("Failed to secure askpass directory: {error}"))?;

    // Random suffix prevents collision when the same process spawns multiple
    // transfers concurrently (single-PID file would race).
    let nonce: u32 = rand::random();
    let file_name = format!("askpass_{}_{:08x}", std::process::id(), nonce);
    let script_path = tmp_dir.join(&file_name);
    let stable_script_path = stable_directory.join(&file_name);

    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&stable_script_path)
        .map_err(|e| format!("Failed to create askpass script: {}", e))?;
    let identity = match stable_file_identity(&file) {
        Ok(identity) => identity,
        Err(error) => {
            drop(file);
            let _ = std::fs::remove_file(&stable_script_path);
            return Err(format!("Failed to identify askpass script: {error}"));
        }
    };
    let write_result = file
        .write_all(content.as_bytes())
        .and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = remove_file_by_identity(&stable_script_path, identity);
        return Err(format!("Failed to write askpass script: {error}"));
    }

    let published_identity = match stable_path_identity(&script_path) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = remove_file_by_identity(&stable_script_path, identity);
            return Err(format!(
                "Failed to bind askpass script to '{}': {error}",
                script_path.display()
            ));
        }
    };
    if published_identity != identity {
        let _ = remove_file_by_identity(&stable_script_path, identity);
        return Err(format!(
            "Askpass directory changed while the private script was created: '{}'",
            tmp_dir.display()
        ));
    }

    drop(directory);
    Ok((script_path, identity))
}

fn askpass_script_content(secret: &str) -> Result<String, String> {
    if secret.contains(['\0', '\r', '\n']) {
        return Err(
            "SSH secret contains a NUL or line break and cannot be passed to ssh".to_string(),
        );
    }
    // Escape single quotes in the secret. `echo` is intentionally avoided: it
    // interprets leading `-n` and backslash sequences on common shells.
    let escaped = secret.replace('\'', "'\\''");
    Ok(format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", escaped))
}

/// Transfer files using rsync with progress reporting.
/// Uses --progress flag (compatible with GNU rsync and openrsync/macOS).
/// For password auth: tries sshpass first, falls back to SSH_ASKPASS mechanism.
fn transfer_rsync(
    config: &TransferConfig,
    cancel_flag: &Arc<AtomicBool>,
    tx: &Sender<ProgressMessage>,
    committed_count: &AtomicUsize,
) -> Result<(), String> {
    validate_rsync_profile(&config.profile)?;
    let ssh_option = build_ssh_option(&config.profile);
    let total_files = config.source_files.len();
    let mut completed_files: usize = 0;

    let names = config
        .source_files
        .iter()
        .map(|path| transfer_name(path))
        .collect::<Result<Vec<_>, _>>()?;

    // Load configured keys through the same no-RSA parser used by russh. The
    // actual data transfer uses OpenSSH, but accepting a key here and rejecting
    // it later for SFTP publication would produce confusing partial transfers.
    if let RemoteAuth::KeyFile { path, passphrase } = &config.profile.auth {
        let key_path = expand_tilde(path);
        let _ = load_supported_secret_key(&key_path, passphrase.as_deref())?;
    }

    let authorized_local_target = config
        .local_target_directory_authorization
        .as_ref()
        .map(|authorization| authorization.resolved_path().to_path_buf());
    let local_target =
        authorized_local_target.unwrap_or_else(|| PathBuf::from(&config.target_path));
    let local_stage = if config.direction == TransferDirection::RemoteToLocal {
        if let Some(authorization) = config.local_target_directory_authorization.as_ref() {
            verify_directory_authorization(
                &local_target,
                authorization,
                "Remote transfer target directory",
            )
            .map_err(|error| error.to_string())?;
        }
        let metadata = std::fs::metadata(&local_target).map_err(|error| {
            format!(
                "Failed to inspect local transfer destination '{}': {}",
                local_target.display(),
                error
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "Transfer destination is not a directory: '{}'",
                local_target.display()
            ));
        }
        for name in &names {
            let destination = local_target.join(name);
            if std::fs::symlink_metadata(&destination).is_ok() {
                return Err(format!(
                    "Destination already exists: '{}'",
                    destination.display()
                ));
            }
        }
        Some(LocalStageDir::create(&local_target)?)
    } else {
        None
    };
    let remote_stage = if config.direction == TransferDirection::LocalToRemote {
        Some(RemoteStageDir::create(
            &config.profile,
            &config.target_path,
            &names,
        )?)
    } else {
        None
    };

    // Prepare password or encrypted-key authentication for the OpenSSH process.
    let needs_password = matches!(&config.profile.auth, RemoteAuth::Password { .. });
    let use_sshpass = needs_password && has_sshpass();
    let askpass_secret = match &config.profile.auth {
        RemoteAuth::Password { password } if !use_sshpass => Some(password.as_str()),
        RemoteAuth::KeyFile {
            passphrase: Some(passphrase),
            ..
        } => Some(passphrase.as_str()),
        _ => None,
    };
    let askpass_script = askpass_secret.map(AskpassGuard::new).transpose()?;

    let transfer_result = (|| -> Result<(), String> {
        for (source_file, name) in config.source_files.iter().zip(&names) {
            if cancel_flag.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(stage) = local_stage.as_ref() {
                if let Some(authorization) = config.local_target_directory_authorization.as_ref() {
                    verify_directory_authorization(
                        &local_target,
                        authorization,
                        "Remote transfer target directory",
                    )
                    .map_err(|error| error.to_string())?;
                }
                stage.verify()?;
            }
            if let Some(stage) = remote_stage.as_ref() {
                stage.verify()?;
            }

            let file_name = source_file.display().to_string();
            let _ = tx.send(ProgressMessage::FileStarted(file_name.clone()));

            let source_full = format!(
                "{}/{}",
                config.source_base.trim_end_matches('/'),
                source_file.display()
            );
            let (src, dst) = match config.direction {
                TransferDirection::LocalToRemote => (
                    source_full,
                    build_remote_spec(
                        &config.profile,
                        &format!("{}/", remote_stage.as_ref().expect("remote stage").path),
                    ),
                ),
                TransferDirection::RemoteToLocal => (
                    build_remote_spec(&config.profile, &source_full),
                    format!(
                        "{}{sep}",
                        local_stage.as_ref().expect("local stage").path.display(),
                        sep = std::path::MAIN_SEPARATOR
                    ),
                ),
            };

            // Build rsync command with --progress (compatible with all rsync versions)
            let mut cmd = Command::new("rsync");
            cmd.arg("-a").arg("--progress");
            if rsync_uses_modern_arg_protection() {
                cmd.arg("--protect-args");
            }
            cmd.arg("-e")
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
                cmd.env("SSH_ASKPASS", guard.path()?)
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
                std::thread::spawn(move || read_bounded_tail(stderr, MAX_COMMAND_OUTPUT))
            });

            // Parse rsync progress output.
            // rsync --progress uses \r (carriage return) to update progress in-place,
            // so we read byte-by-byte and split on both \r and \n.
            if let Some(stdout) = child.stdout.take() {
                let mut reader = BufReader::new(stdout);
                let mut line_buf = Vec::new();
                let mut line_truncated = false;
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
                                line_truncated = false;
                            } else {
                                if line_buf.len() < MAX_PROGRESS_LINE {
                                    line_buf.push(b);
                                } else {
                                    line_truncated = true;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                // Process remaining data in buffer
                if !line_buf.is_empty() && !line_truncated {
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
                match config.direction {
                    TransferDirection::LocalToRemote => {
                        let stage = remote_stage.as_ref().expect("remote stage");
                        stage.verify()?;
                        if publication_is_cancelled(cancel_flag) {
                            return Ok(());
                        }
                        let staged_path = remote_join(&stage.path, name);
                        let destination = remote_join(&config.target_path, name);
                        stage.session.rename_noreplace(&staged_path, &destination)?;
                    }
                    TransferDirection::RemoteToLocal => {
                        let stage = local_stage.as_ref().expect("local stage");
                        if let Some(authorization) =
                            config.local_target_directory_authorization.as_ref()
                        {
                            verify_directory_authorization(
                                &local_target,
                                authorization,
                                "Remote transfer target directory",
                            )
                            .map_err(|error| error.to_string())?;
                        }
                        stage.verify()?;
                        let staged_path = stage.path.join(name);
                        let destination = local_target.join(name);
                        match publish_local_noreplace(&staged_path, &destination, cancel_flag) {
                            Ok(warnings) => {
                                for warning in warnings {
                                    let _ = tx
                                        .send(ProgressMessage::Warning(file_name.clone(), warning));
                                }
                            }
                            Err(failure) if failure.committed => {
                                completed_files += 1;
                                committed_count.store(completed_files, Ordering::Release);
                                let _ = tx.send(ProgressMessage::TerminalError(
                                    file_name,
                                    failure.message.clone(),
                                ));
                                return Err(failure.message);
                            }
                            Err(failure) if failure.cancelled => return Ok(()),
                            Err(failure) => return Err(failure.message),
                        }
                    }
                }
                completed_files += 1;
                committed_count.store(completed_files, Ordering::Release);
                let _ = tx.send(ProgressMessage::FileCompleted(file_name));
                let _ = tx.send(ProgressMessage::TotalProgress(
                    completed_files,
                    total_files,
                    0,
                    0,
                ));
            } else {
                let stderr_output = String::from_utf8_lossy(&stderr_output).into_owned();
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
    })();

    let mut cleanup_errors = Vec::new();
    if let Some(stage) = local_stage {
        if let Err(error) = cleanup_local_stage(stage) {
            cleanup_errors.push(error);
        }
    }
    if let Some(stage) = remote_stage {
        if let Err(error) = cleanup_remote_stage(stage) {
            cleanup_errors.push(error);
        }
    }
    finish_transfer_with_cleanup(
        transfer_result,
        publication_is_cancelled(cancel_flag),
        cleanup_errors,
        tx,
    )
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

/// A remote-to-remote transfer downloads into an outer, private local stage.
/// `transfer_rsync` can return an error after every requested item has already
/// been published into that outer stage when only its own now-empty inner-stage
/// cleanup fails. The downloaded objects remain valid inputs for phase two, so
/// distinguish that post-commit warning from an incomplete download.
fn classify_remote_download_phase(
    result: Result<(), String>,
    downloaded: usize,
    total_files: usize,
) -> Result<Option<String>, String> {
    match result {
        Ok(()) => Ok(None),
        Err(error) if downloaded == total_files => Ok(Some(format!(
            "All remote source items were downloaded, but local download staging cleanup failed: {error}"
        ))),
        Err(error) => Err(error),
    }
}

/// Main transfer function — always uses rsync
/// When `is_cut` is true, source files are deleted after successful transfer.
/// Cuts are rejected because an rsync/SFTP boundary cannot bind both the read
/// and later source deletion to one race-free object snapshot.
pub fn transfer_files_with_progress(
    config: TransferConfig,
    cancel_flag: Arc<AtomicBool>,
    tx: Sender<ProgressMessage>,
    is_cut: bool,
    _source_profile: Option<RemoteProfile>,
) {
    let total_files = config.source_files.len();

    let _ = tx.send(ProgressMessage::Preparing(format!(
        "Transferring {} file(s)...",
        total_files
    )));

    if is_cut {
        let message = "Cut across a remote-transfer boundary is disabled because the source cannot be read and deleted from one race-free snapshot. Copy the files, verify them, then delete the source explicitly."
            .to_string();
        let _ = tx.send(ProgressMessage::Error(String::new(), message));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }

    if let Err(message) = validate_local_source_authorizations(&config) {
        let _ = tx.send(ProgressMessage::Error(String::new(), message));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
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

    let local_snapshot = match prepare_authorized_local_snapshot(&config, &cancel_flag, &tx) {
        Ok(snapshot) => snapshot,
        Err(message) => {
            let cancelled = message.starts_with("Cancelled");
            let _ = tx.send(ProgressMessage::Error(String::new(), message));
            let _ = tx.send(ProgressMessage::Completed(
                0,
                if cancelled { 1 } else { total_files },
            ));
            return;
        }
    };
    let mut transfer_config = config.clone();
    if let Some(snapshot) = local_snapshot.as_ref() {
        transfer_config.source_base = snapshot.path.display().to_string();
        transfer_config.local_source_authorizations.clear();
        transfer_config.local_source_directory_authorization = None;
    }

    let _ = tx.send(ProgressMessage::PrepareComplete);
    let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

    if local_snapshot.is_none() {
        if let Err(message) = validate_local_source_authorizations(&config) {
            let _ = tx.send(ProgressMessage::Error(String::new(), message));
            let _ = tx.send(ProgressMessage::Completed(0, total_files));
            return;
        }
    }
    let committed_count = AtomicUsize::new(0);
    let result = transfer_rsync(&transfer_config, &cancel_flag, &tx, &committed_count);
    if let Some(snapshot) = local_snapshot {
        if let Err(message) = snapshot.cleanup() {
            let _ = tx.send(ProgressMessage::Warning(
                String::new(),
                format!("Local source snapshot cleanup failed: {message}"),
            ));
        }
    }
    let committed = committed_count.load(Ordering::Acquire);

    match result {
        Ok(_) => {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    String::new(),
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(committed, 1));
            } else {
                let _ = tx.send(ProgressMessage::Completed(
                    committed,
                    total_files - committed,
                ));
            }
        }
        Err(ref msg) => {
            if committed == total_files {
                let _ = tx.send(ProgressMessage::Warning(
                    String::new(),
                    format!(
                        "All destinations were committed, but transfer staging cleanup failed: {}",
                        msg
                    ),
                ));
                let _ = tx.send(ProgressMessage::Completed(total_files, 0));
            } else {
                let _ = tx.send(ProgressMessage::Error(String::new(), msg.clone()));
                let _ = tx.send(ProgressMessage::Completed(
                    committed,
                    total_files - committed,
                ));
            }
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
    committed_count: &AtomicUsize,
) -> Result<(), String> {
    let total_files = source_files.len();
    let mut completed_files: usize = 0;
    let names = source_files
        .iter()
        .map(|path| transfer_name(path))
        .collect::<Result<Vec<_>, _>>()?;

    let cut_session = if is_cut {
        let session = SftpSession::connect(profile)?;
        for name in &names {
            let destination = remote_join(target_path, name);
            if session.path_exists_no_follow(&destination)? {
                return Err(format!("Destination already exists: '{destination}'"));
            }
        }
        Some(session)
    } else {
        None
    };
    let copy_stage = if is_cut {
        None
    } else {
        Some(RemoteStageDir::create(profile, target_path, &names)?)
    };
    let ssh = if is_cut {
        None
    } else {
        Some(SshExec::connect(profile)?)
    };

    let transfer_result = (|| -> Result<(), String> {
        for (source_file, name) in source_files.iter().zip(&names) {
            if cancel_flag.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(stage) = copy_stage.as_ref() {
                stage.verify()?;
            }

            let file_name = source_file.display().to_string();
            let _ = tx.send(ProgressMessage::FileStarted(file_name.clone()));

            let source_full = format!(
                "{}/{}",
                source_base.trim_end_matches('/'),
                source_file.display()
            );
            let destination = remote_join(target_path, name);
            if publication_is_cancelled(cancel_flag) {
                return Ok(());
            }
            if is_cut {
                cut_session
                    .as_ref()
                    .expect("cut session")
                    .rename_noreplace(&source_full, &destination)?;
            } else {
                let stage = copy_stage.as_ref().expect("copy stage");
                let escaped_src = source_full.replace('\'', "'\\''");
                let escaped_stage = stage.path.replace('\'', "'\\''");
                let remote_cmd = format!("cp -a '{}' '{}/'", escaped_src, escaped_stage);
                let (success, stderr) = match ssh
                    .as_ref()
                    .expect("SSH copy executor")
                    .exec_cancelable(&remote_cmd, Some(cancel_flag))
                {
                    Ok(result) => result,
                    Err(_) if cancel_flag.load(Ordering::Relaxed) => return Ok(()),
                    Err(e) => return Err(e),
                };
                if !success {
                    let err_msg = format!("Failed to copy '{}': {}", file_name, stderr.trim());
                    let _ = tx.send(ProgressMessage::Error(file_name, err_msg.clone()));
                    return Err(err_msg);
                }
                stage.verify()?;
                if publication_is_cancelled(cancel_flag) {
                    return Ok(());
                }
                let staged_path = remote_join(&stage.path, name);
                stage.session.rename_noreplace(&staged_path, &destination)?;
            }

            completed_files += 1;
            committed_count.store(completed_files, Ordering::Release);
            let _ = tx.send(ProgressMessage::FileCompleted(file_name));
            let _ = tx.send(ProgressMessage::TotalProgress(
                completed_files,
                total_files,
                0,
                0,
            ));
        }

        Ok(())
    })();

    let mut cleanup_errors = Vec::new();
    if let Some(stage) = copy_stage {
        if let Err(error) = cleanup_remote_stage(stage) {
            cleanup_errors.push(error);
        }
    }
    finish_transfer_with_cleanup(
        transfer_result,
        publication_is_cancelled(cancel_flag),
        cleanup_errors,
        tx,
    )
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

    if is_cut {
        let message = "Remote cut is disabled because SFTP cannot bind the selected source to a race-free object snapshot. Copy the files, verify them, then delete the source explicitly."
            .to_string();
        let _ = tx.send(ProgressMessage::Error(String::new(), message));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }

    // Same server optimization: use cp -a / mv directly via SSH
    if is_same_server(&source_profile, &target_profile) {
        let _ = tx.send(ProgressMessage::PrepareComplete);
        let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

        let committed_count = AtomicUsize::new(0);
        let result = transfer_same_server(
            &source_profile,
            &source_files,
            &source_base,
            &target_path,
            &cancel_flag,
            &tx,
            is_cut,
            &committed_count,
        );
        let committed = committed_count.load(Ordering::Acquire);

        match result {
            Ok(_) => {
                if cancel_flag.load(Ordering::Relaxed) {
                    let _ = tx.send(ProgressMessage::Error(
                        String::new(),
                        "Cancelled".to_string(),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(committed, 1));
                } else {
                    let _ = tx.send(ProgressMessage::Completed(
                        committed,
                        total_files - committed,
                    ));
                }
            }
            Err(ref msg) => {
                if committed == total_files {
                    let _ = tx.send(ProgressMessage::Warning(
                        String::new(),
                        format!(
                            "All destinations were committed, but remote staging cleanup failed: {}",
                            msg
                        ),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(total_files, 0));
                } else {
                    let _ = tx.send(ProgressMessage::Error(String::new(), msg.clone()));
                    let _ = tx.send(ProgressMessage::Completed(
                        committed,
                        total_files - committed,
                    ));
                }
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

    let Some(home) = dirs::home_dir() else {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            "Failed to get home directory".to_string(),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    };
    let base_tmp = home.join(".cokacdir").join("tmp");
    if let Err(e) = std::fs::create_dir_all(&base_tmp) {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            format!("Failed to create remote-transfer temp root: {}", e),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }
    let temp_dir = match LocalStageDir::create(&base_tmp) {
        Ok(dir) => dir,
        Err(e) => {
            let _ = tx.send(ProgressMessage::Error(String::new(), e));
            let _ = tx.send(ProgressMessage::Completed(0, total_files));
            return;
        }
    };

    let _ = tx.send(ProgressMessage::PrepareComplete);
    let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

    // Phase 1: Download from source remote to local temp
    let download_config = TransferConfig {
        direction: TransferDirection::RemoteToLocal,
        profile: source_profile.clone(),
        source_files: source_files.clone(),
        source_base: source_base.clone(),
        target_path: temp_dir.path.display().to_string(),
        local_source_authorizations: HashMap::new(),
        local_source_directory_authorization: None,
        local_target_directory_authorization: None,
    };

    if let Err(message) = temp_dir.verify() {
        let _ = tx.send(ProgressMessage::Error(String::new(), message));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }

    let downloaded_count = AtomicUsize::new(0);
    let dl_result = transfer_rsync(&download_config, &cancel_flag, &tx, &downloaded_count);

    let downloaded = downloaded_count.load(Ordering::Acquire);
    match classify_remote_download_phase(dl_result, downloaded, total_files) {
        Ok(Some(warning)) => {
            let _ = tx.send(ProgressMessage::Warning(String::new(), warning));
        }
        Ok(None) => {}
        Err(message) => {
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Download failed: {message}"),
            ));
            let _ = tx.send(ProgressMessage::Completed(0, total_files));
            return;
        }
    }

    if let Err(message) = temp_dir.verify() {
        let _ = tx.send(ProgressMessage::Error(String::new(), message));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }

    if cancel_flag.load(Ordering::Relaxed) {
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
        source_base: temp_dir.path.display().to_string(),
        target_path,
        local_source_authorizations: HashMap::new(),
        local_source_directory_authorization: None,
        local_target_directory_authorization: None,
    };

    let uploaded_count = AtomicUsize::new(0);
    let ul_result = transfer_rsync(&upload_config, &cancel_flag, &tx, &uploaded_count);
    let uploaded = uploaded_count.load(Ordering::Acquire);
    let cleanup_result = temp_dir.cleanup();

    match ul_result {
        Ok(_) => {
            if let Err(message) = cleanup_result {
                let _ = tx.send(ProgressMessage::Warning(String::new(), message));
            }
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    String::new(),
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(uploaded, 1));
            } else {
                let _ = tx.send(ProgressMessage::Completed(uploaded, total_files - uploaded));
            }
        }
        Err(ref msg) => {
            if let Err(message) = cleanup_result {
                let _ = tx.send(ProgressMessage::Warning(
                    String::new(),
                    format!("Local transfer staging cleanup failed: {}", message),
                ));
            }
            if uploaded == total_files {
                let _ = tx.send(ProgressMessage::Warning(
                    String::new(),
                    format!(
                        "All remote destinations were committed, but final cleanup failed: {}",
                        msg
                    ),
                ));
                let _ = tx.send(ProgressMessage::Completed(total_files, 0));
            } else {
                let _ = tx.send(ProgressMessage::Error(
                    String::new(),
                    format!("Upload failed: {}", msg),
                ));
                let _ = tx.send(ProgressMessage::Completed(uploaded, total_files - uploaded));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(user: &str, host: &str) -> RemoteProfile {
        RemoteProfile {
            name: "test".to_string(),
            host: host.to_string(),
            port: 22,
            user: user.to_string(),
            auth: RemoteAuth::Password {
                password: "test".to_string(),
            },
            default_path: String::new(),
        }
    }

    #[test]
    fn bounded_tail_keeps_only_the_end() {
        let mut output = b"abcdef".to_vec();
        append_bounded_tail(&mut output, b"ghijkl", 8);
        assert_eq!(output, b"efghijkl");
        append_bounded_tail(&mut output, b"0123456789", 8);
        assert_eq!(output, b"23456789");
        assert_eq!(read_bounded_tail(&b"abcdefghijkl"[..], 5), b"hijkl");
    }

    #[test]
    fn fully_downloaded_remote_phase_treats_cleanup_failure_as_a_warning() {
        let warning = classify_remote_download_phase(
            Err("inner stage could not be removed".to_string()),
            2,
            2,
        )
        .expect("all requested downloads were already committed")
        .expect("post-commit cleanup failure must remain visible");

        assert!(warning.contains("were downloaded"));
        assert!(warning.contains("inner stage could not be removed"));
        assert!(classify_remote_download_phase(Err("download failed".to_string()), 1, 2).is_err());
    }

    #[test]
    fn cancelled_transfer_surfaces_preserved_stage_cleanup_warning() {
        let (tx, rx) = std::sync::mpsc::channel();
        let stage_warning =
            "Remote transfer staging cleanup failed at '/target/.cokacdir-transfer-test'; staged data may remain"
                .to_string();

        assert!(
            finish_transfer_with_cleanup(Ok(()), true, vec![stage_warning.clone()], &tx,).is_ok()
        );
        assert!(matches!(
            rx.recv().unwrap(),
            ProgressMessage::Warning(_, warning) if warning == stage_warning
        ));
    }

    #[test]
    fn transfer_items_must_be_single_names() {
        assert_eq!(
            transfer_name(Path::new("report.txt")).unwrap(),
            "report.txt"
        );
        assert!(transfer_name(Path::new("../report.txt")).is_err());
        assert!(transfer_name(Path::new("nested/report.txt")).is_err());
        assert!(transfer_name(Path::new("/report.txt")).is_err());
        assert!(transfer_name(Path::new(".")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn transfer_item_rejects_non_utf8_name_instead_of_changing_it() {
        use std::os::unix::ffi::OsStringExt;

        let name = std::ffi::OsString::from_vec(vec![b'f', 0xff]);
        assert!(transfer_name(Path::new(&name)).is_err());
    }

    #[test]
    fn remote_paths_join_without_double_slashes() {
        assert_eq!(remote_join("/", "a"), "/a");
        assert_eq!(remote_join("/home/user/", "a"), "/home/user/a");
        assert_eq!(remote_join("relative", "a"), "relative/a");
    }

    #[test]
    fn rsync_profile_validation_rejects_option_and_shell_injection() {
        assert!(validate_rsync_profile(&profile("user", "example.com")).is_ok());
        assert!(validate_rsync_profile(&profile("user", "2001:db8::1")).is_ok());
        assert!(validate_rsync_profile(&profile("user", "[2001:db8::1]")).is_ok());
        assert!(validate_rsync_profile(&profile("-oProxyCommand=x", "example.com")).is_err());
        assert!(validate_rsync_profile(&profile("user", "host;touch /tmp/pwned")).is_err());

        let raw = profile("user", "2001:db8::1");
        let bracketed = profile("user", "[2001:db8::1]");
        assert_eq!(
            build_remote_spec(&raw, "/work/file"),
            build_remote_spec(&bracketed, "/work/file")
        );
        assert!(is_same_server(&raw, &bracketed));
    }

    #[cfg(unix)]
    #[test]
    fn askpass_printf_preserves_dash_backslashes_and_quotes() {
        let secret = "-n\\word'secret";
        let script = askpass_script_content(secret).unwrap();
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("run askpass script");
        assert!(output.status.success());
        assert_eq!(output.stdout, format!("{secret}\n").as_bytes());
        assert!(askpass_script_content("first\nsecond").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn askpass_creation_rejects_symlinked_private_directory() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let link = root.path().join("tmp");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, &link).unwrap();

        assert!(create_askpass_script_in_dir(&link, "#!/bin/sh\nexit 0\n").is_err());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(std::fs::read_dir(target).unwrap().next().is_none());
    }

    #[test]
    fn local_publish_never_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("stage");
        let destination = dir.path().join("final");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&destination, b"old").unwrap();
        assert!(publish_local_noreplace(&source, &destination, &AtomicBool::new(false)).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");
        assert_eq!(std::fs::read(&source).unwrap(), b"new");
    }

    #[test]
    fn local_publish_moves_complete_new_entry() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("stage");
        let destination = dir.path().join("final");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("data"), b"complete").unwrap();
        publish_local_noreplace(&source, &destination, &AtomicBool::new(false)).unwrap();
        assert!(!source.exists());
        assert_eq!(
            std::fs::read(destination.join("data")).unwrap(),
            b"complete"
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_publish_treats_dangling_symlink_as_conflict() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("stage");
        let destination = dir.path().join("final");
        std::fs::write(&source, b"new").unwrap();
        symlink(dir.path().join("missing"), &destination).unwrap();
        assert!(publish_local_noreplace(&source, &destination, &AtomicBool::new(false)).is_err());
        assert!(std::fs::symlink_metadata(destination)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn cancelled_local_publication_keeps_the_staged_entry() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("stage");
        let destination = dir.path().join("final");
        std::fs::write(&source, b"complete but cancelled").unwrap();
        let cancel_flag = AtomicBool::new(true);

        let failure = publish_local_noreplace(&source, &destination, &cancel_flag)
            .expect_err("cancelled staged data must not be published");

        assert!(failure.cancelled);
        assert!(!failure.committed);
        assert_eq!(std::fs::read(source).unwrap(), b"complete but cancelled");
        assert!(std::fs::symlink_metadata(destination).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn local_stage_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let stage = LocalStageDir::create(dir.path()).unwrap();
        let mode = std::fs::metadata(&stage.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn local_stage_cleanup_removes_the_verified_directory() {
        let dir = tempfile::tempdir().unwrap();
        let stage = LocalStageDir::create(dir.path()).unwrap();
        let path = stage.path.clone();
        std::fs::write(path.join("payload"), b"data").unwrap();
        stage.cleanup().unwrap();
        assert!(std::fs::symlink_metadata(path).is_err());
    }

    #[test]
    fn authorized_local_transfer_uses_an_immutable_private_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, b"confirmed").unwrap();
        let directory =
            crate::services::file_ops::capture_directory_authorization(dir.path()).unwrap();
        let item = crate::services::file_ops::capture_path_authorization(&source).unwrap();
        let config = TransferConfig {
            direction: TransferDirection::LocalToRemote,
            profile: profile("user", "example.com"),
            source_files: vec![PathBuf::from("source")],
            source_base: directory.resolved_path().display().to_string(),
            target_path: "/target".to_string(),
            local_source_authorizations: HashMap::from([(source.clone(), item)]),
            local_source_directory_authorization: Some(directory),
            local_target_directory_authorization: None,
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        let snapshot =
            prepare_authorized_local_snapshot(&config, &Arc::new(AtomicBool::new(false)), &tx)
                .unwrap()
                .unwrap();

        std::fs::write(&source, b"changed later").unwrap();

        assert_eq!(
            std::fs::read(snapshot.path.join("source")).unwrap(),
            b"confirmed"
        );
        snapshot.cleanup().unwrap();
    }

    #[test]
    fn remote_boundary_cut_is_rejected_before_network_or_source_access() {
        let config = TransferConfig {
            direction: TransferDirection::LocalToRemote,
            profile: profile("user", "example.com"),
            source_files: vec![PathBuf::from("missing")],
            source_base: "/missing".to_string(),
            target_path: "/target".to_string(),
            local_source_authorizations: HashMap::new(),
            local_source_directory_authorization: None,
            local_target_directory_authorization: None,
        };
        let (tx, rx) = std::sync::mpsc::channel();

        transfer_files_with_progress(config, Arc::new(AtomicBool::new(false)), tx, true, None);

        let messages: Vec<_> = rx.try_iter().collect();
        assert!(messages.iter().any(|message| {
            matches!(message, ProgressMessage::Error(_, error) if error.contains("disabled"))
        }));
        assert!(matches!(
            messages.last(),
            Some(ProgressMessage::Completed(0, 1))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn local_stage_rejects_non_sticky_shared_parent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = match LocalStageDir::create(dir.path()) {
            Ok(_) => panic!("non-sticky shared parent must be rejected"),
            Err(error) => error,
        };
        assert!(error.contains("group/world-writable"));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn local_stage_drop_does_not_delete_a_replacement_path() {
        let dir = tempfile::tempdir().unwrap();
        let stage = LocalStageDir::create(dir.path()).unwrap();
        let path = stage.path.clone();
        let retained = dir.path().join("retained-stage");
        std::fs::rename(&path, &retained).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("replacement"), b"do not delete").unwrap();
        drop(stage);

        assert_eq!(
            std::fs::read(path.join("replacement")).unwrap(),
            b"do not delete"
        );
        assert!(retained.is_dir());
    }
}
