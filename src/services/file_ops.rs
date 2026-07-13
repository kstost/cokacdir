use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use filetime::{self, FileTime};
use sha2::{Digest, Sha256};

use crate::utils::format::strip_unc_prefix;

/// An open directory descriptor used as the namespace root for child access.
///
/// A pathname such as `/proc/self/fd/N/child` happens to work on Linux, but
/// macOS' `/dev/fd/N` entries are not traversable directories. Keeping a
/// duplicated descriptor here lets all Unix platforms use the actual `*at`
/// system calls instead of pretending that a descriptor is a pathname.
#[derive(Debug)]
pub(crate) struct DirectoryAccess {
    directory: File,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectoryFileOptions {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    create_new: bool,
    pin_name: bool,
    mode: u32,
}

impl Default for DirectoryFileOptions {
    fn default() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            create: false,
            create_new: false,
            pin_name: false,
            mode: 0o600,
        }
    }
}

impl DirectoryFileOptions {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn read(mut self, value: bool) -> Self {
        self.read = value;
        self
    }

    pub(crate) fn write(mut self, value: bool) -> Self {
        self.write = value;
        self
    }

    pub(crate) fn append(mut self, value: bool) -> Self {
        self.append = value;
        self
    }

    pub(crate) fn create(mut self, value: bool) -> Self {
        self.create = value;
        self
    }

    pub(crate) fn create_new(mut self, value: bool) -> Self {
        self.create_new = value;
        self
    }

    pub(crate) fn pin_name(mut self, value: bool) -> Self {
        self.pin_name = value;
        self
    }

    pub(crate) fn mode(mut self, value: u32) -> Self {
        self.mode = value;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectoryEntryMetadata {
    kind: DirectoryEntryKind,
    len: u64,
    mode: u32,
    identity: StablePathIdentity,
}

pub(crate) struct DirectoryEntries {
    #[cfg(unix)]
    directory: rustix::fs::Dir,
    #[cfg(not(unix))]
    directory: fs::ReadDir,
}

impl Iterator for DirectoryEntries {
    type Item = io::Result<OsString>;

    fn next(&mut self) -> Option<Self::Item> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            loop {
                let entry = match self.directory.next()? {
                    Ok(entry) => entry,
                    Err(error) => return Some(Err(io::Error::from(error))),
                };
                let bytes = entry.file_name().to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                return Some(Ok(OsString::from_vec(bytes.to_vec())));
            }
        }

        #[cfg(not(unix))]
        {
            self.directory
                .next()
                .map(|entry| entry.map(|entry| entry.file_name()))
        }
    }
}

impl DirectoryEntryMetadata {
    pub(crate) fn is_file(self) -> bool {
        self.kind == DirectoryEntryKind::File
    }

    pub(crate) fn is_dir(self) -> bool {
        self.kind == DirectoryEntryKind::Directory
    }

    pub(crate) fn is_symlink(self) -> bool {
        self.kind == DirectoryEntryKind::Symlink
    }

    pub(crate) fn len(self) -> u64 {
        self.len
    }

    pub(crate) fn mode(self) -> u32 {
        self.mode
    }

    pub(crate) fn identity(self) -> StablePathIdentity {
        self.identity
    }
}

fn validate_directory_entry_name(name: &OsStr) -> io::Result<()> {
    use std::path::Component;

    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if component == name => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Directory-relative access requires one normal path component",
        )),
    }
}

impl DirectoryAccess {
    fn new(directory: &File, path: &Path) -> io::Result<Self> {
        Ok(Self {
            directory: directory.try_clone()?,
            path: path.to_path_buf(),
        })
    }

    pub(crate) fn file(&self) -> &File {
        &self.directory
    }

    pub(crate) fn open_file(
        &self,
        name: &OsStr,
        options: DirectoryFileOptions,
    ) -> io::Result<File> {
        validate_directory_entry_name(name)?;

        #[cfg(unix)]
        {
            use rustix::fs::{openat, Mode, OFlags};

            let access = if options.read && (options.write || options.append) {
                OFlags::RDWR
            } else if options.write || options.append {
                OFlags::WRONLY
            } else {
                OFlags::RDONLY
            };
            let _ = options.pin_name;
            let mut flags = access | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
            if options.append {
                flags |= OFlags::APPEND;
            }
            if options.create || options.create_new {
                flags |= OFlags::CREATE;
            }
            if options.create_new {
                flags |= OFlags::EXCL;
            }
            let mode = Mode::from(options.mode as rustix::fs::RawMode);
            return openat(&self.directory, name, flags, mode)
                .map(File::from)
                .map_err(io::Error::from);
        }

        #[cfg(not(unix))]
        {
            let mut platform_options = OpenOptions::new();
            platform_options
                .read(options.read)
                .write(options.write)
                .append(options.append)
                .create(options.create)
                .create_new(options.create_new);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
                platform_options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
                if options.pin_name {
                    const FILE_SHARE_READ: u32 = 0x0000_0001;
                    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
                    platform_options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
                }
            }
            platform_options.open(self.path.join(name))
        }
    }

    pub(crate) fn open_regular_file(&self, name: &OsStr) -> io::Result<(File, fs::Metadata)> {
        let file = self.open_file(name, DirectoryFileOptions::new().read(true))?;
        let metadata = file.metadata()?;
        #[cfg(windows)]
        let is_reparse = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x0400 != 0
        };
        #[cfg(not(windows))]
        let is_reparse = false;
        if !metadata.is_file() || is_reparse {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Directory entry is not a real regular file",
            ));
        }
        if stable_file_identity(&file)? != self.child_identity(name)? {
            return Err(io::Error::other(
                "Directory entry changed while the regular file was being opened",
            ));
        }
        Ok((file, metadata))
    }

    pub(crate) fn open_directory(
        &self,
        name: &OsStr,
    ) -> io::Result<(File, DirectoryAccess, fs::Metadata)> {
        validate_directory_entry_name(name)?;

        #[cfg(unix)]
        {
            use rustix::fs::{openat, Mode, OFlags};

            let owned = openat(
                &self.directory,
                name,
                OFlags::RDONLY
                    | OFlags::DIRECTORY
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC
                    | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            let file = File::from(owned);
            let metadata = file.metadata()?;
            if !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Directory entry is not a directory",
                ));
            }
            let access = DirectoryAccess::new(&file, &self.path.join(name))?;
            return Ok((file, access, metadata));
        }

        #[cfg(not(unix))]
        {
            open_directory_for_read(&self.path.join(name))
        }
    }

    pub(crate) fn create_directory(&self, name: &OsStr, mode: u32) -> io::Result<()> {
        validate_directory_entry_name(name)?;

        #[cfg(unix)]
        {
            return rustix::fs::mkdirat(
                &self.directory,
                name,
                rustix::fs::Mode::from(mode as rustix::fs::RawMode),
            )
            .map_err(io::Error::from);
        }

        #[cfg(not(unix))]
        {
            let _ = mode;
            fs::create_dir(self.path.join(name))
        }
    }

    pub(crate) fn create_private_directory(&self, label: &str) -> io::Result<OsString> {
        let mut last_collision = None;
        for _ in 0..128 {
            let name = OsString::from(format!(
                ".cokacdir-{}-{}-{:032x}",
                label,
                std::process::id(),
                rand::random::<u128>()
            ));
            match self.create_directory(&name, 0o700) {
                Ok(()) => return Ok(name),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_collision = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_collision.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Unable to allocate a private directory entry",
            )
        }))
    }

    pub(crate) fn child_metadata(&self, name: &OsStr) -> io::Result<DirectoryEntryMetadata> {
        validate_directory_entry_name(name)?;

        #[cfg(unix)]
        {
            use rustix::fs::{statat, AtFlags, FileType};

            let stat = statat(&self.directory, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
            let kind = match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => DirectoryEntryKind::File,
                FileType::Directory => DirectoryEntryKind::Directory,
                FileType::Symlink => DirectoryEntryKind::Symlink,
                _ => DirectoryEntryKind::Other,
            };
            let mut object = [0u8; 16];
            object[..8].copy_from_slice(&(stat.st_ino as u64).to_le_bytes());
            return Ok(DirectoryEntryMetadata {
                kind,
                len: u64::try_from(stat.st_size).unwrap_or(0),
                mode: stat.st_mode as u32,
                identity: StablePathIdentity {
                    namespace: stat.st_dev as u64,
                    object,
                },
            });
        }

        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            let metadata = fs::symlink_metadata(&path)?;
            let kind = if metadata.file_type().is_symlink() {
                DirectoryEntryKind::Symlink
            } else if metadata.is_file() {
                DirectoryEntryKind::File
            } else if metadata.is_dir() {
                DirectoryEntryKind::Directory
            } else {
                DirectoryEntryKind::Other
            };
            Ok(DirectoryEntryMetadata {
                kind,
                len: metadata.len(),
                mode: 0,
                identity: stable_path_identity(&path)?,
            })
        }
    }

    pub(crate) fn child_identity(&self, name: &OsStr) -> io::Result<StablePathIdentity> {
        self.child_metadata(name)
            .map(DirectoryEntryMetadata::identity)
    }

    pub(crate) fn entries(&self) -> io::Result<DirectoryEntries> {
        #[cfg(unix)]
        {
            let directory = rustix::fs::Dir::read_from(&self.directory).map_err(io::Error::from)?;
            return Ok(DirectoryEntries { directory });
        }

        #[cfg(not(unix))]
        {
            Ok(DirectoryEntries {
                directory: fs::read_dir(&self.path)?,
            })
        }
    }

    pub(crate) fn collect_entry_names(&self) -> io::Result<Vec<OsString>> {
        self.entries()?.collect()
    }

    pub(crate) fn remove_file_if_identity(
        &self,
        name: &OsStr,
        expected: StablePathIdentity,
    ) -> io::Result<()> {
        if self.child_identity(name)? != expected {
            return Err(io::Error::other(
                "Directory entry changed before verified deletion",
            ));
        }

        #[cfg(unix)]
        {
            return rustix::fs::unlinkat(&self.directory, name, rustix::fs::AtFlags::empty())
                .map_err(io::Error::from);
        }

        #[cfg(not(unix))]
        {
            remove_file_by_identity(&self.path.join(name), expected)
        }
    }

    pub(crate) fn remove_directory_if_identity(
        &self,
        name: &OsStr,
        expected: StablePathIdentity,
    ) -> io::Result<()> {
        let metadata = self.child_metadata(name)?;
        if metadata.identity() != expected || !metadata.is_dir() {
            return Err(io::Error::other(
                "Directory entry changed before verified directory removal",
            ));
        }

        #[cfg(unix)]
        {
            return rustix::fs::unlinkat(&self.directory, name, rustix::fs::AtFlags::REMOVEDIR)
                .map_err(io::Error::from);
        }

        #[cfg(not(unix))]
        {
            fs::remove_dir(self.path.join(name))
        }
    }

    pub(crate) fn rename_replace(&self, source: &OsStr, destination: &OsStr) -> io::Result<()> {
        validate_directory_entry_name(source)?;
        validate_directory_entry_name(destination)?;

        #[cfg(unix)]
        {
            return rustix::fs::renameat(&self.directory, source, &self.directory, destination)
                .map_err(io::Error::from);
        }

        #[cfg(windows)]
        {
            return windows_replace_file(&self.path.join(source), &self.path.join(destination));
        }

        #[cfg(not(any(unix, windows)))]
        {
            fs::rename(self.path.join(source), self.path.join(destination))
        }
    }

    #[cfg(unix)]
    pub(crate) fn read_link(&self, name: &OsStr) -> io::Result<PathBuf> {
        use std::os::unix::ffi::OsStringExt;

        validate_directory_entry_name(name)?;
        rustix::fs::readlinkat(&self.directory, name, Vec::new())
            .map(|target| PathBuf::from(OsString::from_vec(target.into_bytes())))
            .map_err(io::Error::from)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_replace_file(source: &Path, destination: &Path) -> io::Result<()> {
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
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Preserve modification time and access time from source metadata to destination path.
pub fn preserve_timestamps(dest: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let mtime = FileTime::from_last_modification_time(metadata);
    let atime = FileTime::from_last_access_time(metadata);
    filetime::set_file_times(dest, atime, mtime)?;
    Ok(())
}

/// Check if an error is a cross-device rename error
#[cfg(unix)]
fn is_cross_device_error(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EXDEV)
}

#[cfg(windows)]
fn is_cross_device_error(e: &io::Error) -> bool {
    e.raw_os_error() == Some(17) // ERROR_NOT_SAME_DEVICE
}

#[cfg(not(any(unix, windows)))]
fn is_cross_device_error(_e: &io::Error) -> bool {
    false
}

/// File operation type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperationType {
    Copy,
    Move,
    Tar,
    Untar,
    Download,
    Encrypt,
    Decrypt,
}

/// Progress message for file operations
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields are used for debugging/logging, not always read
pub enum ProgressMessage {
    /// Preparing operation (message)
    Preparing(String),
    /// Preparation complete, starting actual operation
    PrepareComplete,
    /// File operation started (filename)
    FileStarted(String),
    /// File progress (copied bytes, total bytes)
    FileProgress(u64, u64),
    /// File completed (filename)
    FileCompleted(String),
    /// Total progress (completed files, total files, completed bytes, total bytes)
    TotalProgress(usize, usize, u64, u64),
    /// Operation completed (success count, failure count)
    Completed(usize, usize),
    /// Error occurred (filename, error message)
    Error(String, String),
    /// Destination may have committed, so a failed cut item must not be
    /// automatically retried even when its source was restored.
    TerminalError(String, String),
    /// Operation committed, but durability or recovery-artifact cleanup needs attention.
    Warning(String, String),
}

/// File operation result
#[derive(Debug, Clone)]
pub struct FileOperationResult {
    pub success_count: usize,
    pub failure_count: usize,
    pub last_error: Option<String>,
    pub warnings: Vec<String>,
}

/// Buffer size for file copy (64KB)
const COPY_BUFFER_SIZE: usize = 64 * 1024;

fn send_prepare_error_result(
    progress_tx: &Sender<ProgressMessage>,
    err: io::Error,
    fallback_failure_count: usize,
) {
    let cancelled = err.kind() == io::ErrorKind::Interrupted;
    let message = if cancelled {
        "Cancelled".to_string()
    } else {
        err.to_string()
    };
    let failure_count = if cancelled { 1 } else { fallback_failure_count };

    let _ = progress_tx.send(ProgressMessage::Error(String::new(), message));
    let _ = progress_tx.send(ProgressMessage::Completed(0, failure_count));
}

fn send_operation_warnings(
    progress_tx: &Sender<ProgressMessage>,
    filename: &str,
    warnings: Vec<String>,
) {
    for warning in warnings {
        let _ = progress_tx.send(ProgressMessage::Warning(filename.to_string(), warning));
    }
}

fn finish_copied_item(
    publication_staging: Option<PrivateStagingDirectory>,
    destination: &Path,
    expected_destination: Option<PathIdentity>,
    published: PublishedStage,
    cancel_flag: &Arc<AtomicBool>,
    target_authorization: Option<&DirectoryAuthorization>,
) -> io::Result<Vec<String>> {
    let (published_identity, mut warnings) = verified_publication_parts(published, destination)?;
    match publication_staging {
        Some(staging) => {
            // Adopt the identity verified by the inner publication. Rebinding
            // the pathname here would authorize a replacement that raced in
            // after the copy helper returned.
            let stage = OwnedStage::from_published(staging, published_identity)?;
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(match stage.cleanup() {
                    Ok(()) => io::Error::new(io::ErrorKind::Interrupted, "Cancelled"),
                    Err(cleanup_error) => io::Error::new(
                        io::ErrorKind::Interrupted,
                        format!(
                            "Cancelled before overwrite commit; verified staging cleanup failed: {}",
                            cleanup_error
                        ),
                    ),
                });
            }
            if let Some(authorization) = target_authorization {
                if let Err(error) = authorized_current_directory(
                    destination.parent().unwrap_or_else(|| Path::new(".")),
                    authorization,
                    "Paste target directory",
                ) {
                    return Err(match stage.cleanup() {
                        Ok(()) => error,
                        Err(cleanup_error) => io::Error::new(
                            error.kind(),
                            format!(
                                "{}; publication staging cleanup also failed: {}",
                                error, cleanup_error
                            ),
                        ),
                    });
                }
            }
            if let Some(expected) = expected_destination {
                warnings.extend(install_owned_replacement_if_unchanged(
                    stage,
                    destination,
                    expected,
                )?);
            } else {
                let published = stage
                    .publish_noreplace(destination)
                    .map_err(error_with_owned_stage_cleanup)?;
                let (_identity, publish_warnings) =
                    verified_publication_parts(published, destination)?;
                warnings.extend(publish_warnings);
            }
        }
        None if target_authorization.is_some() => {
            return Err(io::Error::other(
                "Authorized copy is missing final publication staging",
            ));
        }
        None if expected_destination.is_some() => {
            return Err(io::Error::other(
                "Unexpected destination identity without overwrite staging",
            ));
        }
        None => drop(published_identity),
    }
    Ok(warnings)
}

fn verified_publication_parts(
    published: PublishedStage,
    destination: &Path,
) -> io::Result<(PathIdentity, Vec<String>)> {
    let PublishedStage { identity, warnings } = published;
    let identity = identity.ok_or_else(|| {
        io::Error::other(format!(
            "Copy publication at '{}' committed, but its identity could not be verified; inspect it manually",
            destination.display()
        ))
    })?;
    Ok((identity, warnings))
}

fn path_exists_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn validate_destination_not_self(src: &Path, dest: &Path, operation: &str) -> io::Result<()> {
    if src == dest {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Cannot {} a file onto itself", operation),
        ));
    }

    let source_metadata = fs::symlink_metadata(src)?;
    if path_exists_no_follow(dest)
        && matches!(
            (stable_path_identity(src), stable_path_identity(dest)),
            (Ok(source), Ok(destination)) if source == destination
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Cannot {} a file onto another name for the same object",
                operation
            ),
        ));
    }
    let canonical_src = src.canonicalize().ok();

    if let (Some(canonical_src), Ok(canonical_dest)) = (&canonical_src, dest.canonicalize()) {
        if canonical_src == &canonical_dest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Cannot {} a file onto itself", operation),
            ));
        }
    }

    if source_metadata.is_dir() && !source_metadata.is_symlink() {
        let canonical_src = canonical_src.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot resolve the source directory",
            )
        })?;
        let dest_anchor = if path_exists_no_follow(dest) {
            dest.canonicalize()
        } else {
            dest.parent()
                .unwrap_or_else(|| Path::new("."))
                .canonicalize()
        };

        if let Ok(canonical_anchor) = dest_anchor {
            if canonical_anchor.starts_with(&canonical_src) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Cannot {} a directory into itself", operation),
                ));
            }
        }
    }

    Ok(())
}

fn copy_symlink(src: &Path, dest: &Path) -> io::Result<()> {
    copy_symlink_detailed(src, dest, dest, None).map(|_| ())
}

pub(crate) fn copy_symlink_authorized(
    src: &Path,
    dest: &Path,
    expected_source: &PathAuthorization,
) -> io::Result<Vec<String>> {
    copy_symlink_detailed(src, dest, dest, Some(expected_source))
        .map(|published| published.warnings)
}

fn copy_symlink_detailed(
    src: &Path,
    dest: &Path,
    logical_destination: &Path,
    expected_source: Option<&PathAuthorization>,
) -> io::Result<PublishedStage> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let staging = PrivateStagingDirectory::create(parent, "copy-link")?;
    let temp = staging.payload();
    if let Err(error) = copy_symlink_to_new(src, &temp, logical_destination, expected_source) {
        return Err(error_with_staging_cleanup(error, staging));
    }
    let stage = OwnedStage::bind(staging)?;
    let published = match stage.publish_noreplace(dest) {
        Ok(outcome) => outcome,
        Err(failure) => return Err(error_with_owned_stage_cleanup(failure)),
    };
    if published.identity.is_none() {
        return Err(io::Error::other(format!(
            "Symlink publication at '{}' committed, but the destination no longer identifies the staged link; inspect it manually",
            dest.display()
        )));
    }
    Ok(published)
}

fn error_with_staging_cleanup(cause: io::Error, staging: PrivateStagingDirectory) -> io::Error {
    let retry_unsafe = is_retry_unsafe(&cause);
    // A retry-unsafe error may be the only accurate pointer to source or
    // destination recovery data still stored in payload. Moving the owning
    // directory again during cleanup would make that reported path stale.
    if retry_unsafe && path_exists_no_follow(&staging.payload()) {
        return cause;
    }
    match staging.cleanup() {
        Ok(()) => cause,
        Err(cleanup_error) => operation_error(
            cause.kind(),
            format!(
                "{}; private staging cleanup also failed: {}",
                cause, cleanup_error
            ),
            retry_unsafe,
        ),
    }
}

#[derive(Debug)]
struct RetryUnsafeOperationError(String);

impl std::fmt::Display for RetryUnsafeOperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RetryUnsafeOperationError {}

fn operation_error(kind: io::ErrorKind, message: String, retry_unsafe: bool) -> io::Error {
    if retry_unsafe {
        io::Error::new(kind, RetryUnsafeOperationError(message))
    } else {
        io::Error::new(kind, message)
    }
}

fn is_retry_unsafe(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<RetryUnsafeOperationError>().is_some())
}

fn error_with_partial_stage_cleanup(cause: io::Error, stage: PartialStage) -> io::Error {
    let retry_unsafe = is_retry_unsafe(&cause);
    match stage.cleanup() {
        Ok(()) => cause,
        Err(cleanup_error) => operation_error(
            cause.kind(),
            format!(
                "{}; verified partial staging cleanup also failed: {}",
                cause, cleanup_error
            ),
            retry_unsafe,
        ),
    }
}

fn error_with_owned_stage_cleanup(failure: StagePublishFailure) -> io::Error {
    let retry_unsafe = is_retry_unsafe(&failure.error);
    match failure.stage.cleanup() {
        Ok(()) => failure.error,
        Err(cleanup_error) => operation_error(
            failure.error.kind(),
            format!(
                "{}; verified staging cleanup also failed: {}",
                failure.error, cleanup_error
            ),
            retry_unsafe,
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct StablePathIdentity {
    namespace: u64,
    object: [u8; 16],
}

impl StablePathIdentity {
    pub(crate) fn components(self) -> (u64, [u8; 16]) {
        (self.namespace, self.object)
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
struct WindowsHandleInfo {
    identity: StablePathIdentity,
    creation_time: u64,
    last_write_time: u64,
    size: u64,
    attributes: u32,
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_handle_info(
    file: &impl std::os::windows::io::AsRawHandle,
) -> io::Result<WindowsHandleInfo> {
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct ByHandleFileInformation {
        attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }
    #[repr(C)]
    struct FileId128 {
        identifier: [u8; 16],
    }
    #[repr(C)]
    struct FileIdInfo {
        volume_serial_number: u64,
        file_id: FileId128,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut c_void,
            information: *mut ByHandleFileInformation,
        ) -> i32;
        fn GetFileInformationByHandleEx(
            handle: *mut c_void,
            information_class: i32,
            information: *mut c_void,
            buffer_size: u32,
        ) -> i32;
    }

    let mut information = MaybeUninit::<ByHandleFileInformation>::uninit();
    let result = unsafe {
        GetFileInformationByHandle(file.as_raw_handle().cast(), information.as_mut_ptr())
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    let information = unsafe { information.assume_init() };
    const FILE_ID_INFO_CLASS: i32 = 18;
    let mut extended = MaybeUninit::<FileIdInfo>::uninit();
    let extended_result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FILE_ID_INFO_CLASS,
            extended.as_mut_ptr().cast(),
            std::mem::size_of::<FileIdInfo>() as u32,
        )
    };
    let identity = if extended_result != 0 {
        let extended = unsafe { extended.assume_init() };
        StablePathIdentity {
            namespace: extended.volume_serial_number,
            object: extended.file_id.identifier,
        }
    } else {
        let file_index =
            (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low);
        let mut object = [0u8; 16];
        object[..8].copy_from_slice(&file_index.to_le_bytes());
        StablePathIdentity {
            namespace: u64::from(information.volume_serial_number),
            object,
        }
    };
    let time = |value: FileTime| (u64::from(value.high) << 32) | u64::from(value.low);
    Ok(WindowsHandleInfo {
        identity,
        creation_time: time(information.creation_time),
        last_write_time: time(information.last_write_time),
        size: (u64::from(information.file_size_high) << 32) | u64::from(information.file_size_low),
        attributes: information.attributes,
    })
}

#[cfg(windows)]
pub(crate) fn stable_windows_handle_identity(
    handle: &impl std::os::windows::io::AsRawHandle,
) -> io::Result<StablePathIdentity> {
    windows_handle_info(handle).map(|information| information.identity)
}

#[cfg(windows)]
fn open_windows_path_identity(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    let mut options = OpenOptions::new();
    options
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

#[cfg(windows)]
fn open_windows_path_for_delete(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const DELETE_ACCESS: u32 = 0x0001_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    let mut options = OpenOptions::new();
    options
        .access_mode(DELETE_ACCESS)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

pub(crate) fn stable_file_identity(file: &File) -> io::Result<StablePathIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        return Ok(StablePathIdentity {
            namespace: metadata.dev(),
            object: {
                let mut object = [0u8; 16];
                object[..8].copy_from_slice(&metadata.ino().to_le_bytes());
                object
            },
        });
    }
    #[cfg(windows)]
    {
        return windows_handle_info(file).map(|information| information.identity);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Stable filesystem object identity is unavailable on this platform",
        ))
    }
}

pub(crate) fn stable_path_identity(path: &Path) -> io::Result<StablePathIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path)?;
        return Ok(StablePathIdentity {
            namespace: metadata.dev(),
            object: {
                let mut object = [0u8; 16];
                object[..8].copy_from_slice(&metadata.ino().to_le_bytes());
                object
            },
        });
    }
    #[cfg(windows)]
    {
        return windows_handle_info(&open_windows_path_identity(path)?)
            .map(|information| information.identity);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Stable filesystem object identity is unavailable on this platform",
        ))
    }
}

pub(crate) struct PreparedFileDeletion {
    #[cfg(windows)]
    file: File,
    #[cfg(not(windows))]
    path: PathBuf,
    #[cfg(not(windows))]
    expected: StablePathIdentity,
}

/// Bind a future deletion to the exact non-directory object currently named by
/// `path`. Callers with an existing identity handle can keep it alive during
/// this step, then close it before `delete` sets Windows disposition.
pub(crate) fn prepare_file_deletion(
    path: &Path,
    expected: StablePathIdentity,
) -> io::Result<PreparedFileDeletion> {
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
        let file = open_windows_path_for_delete(path)?;
        let information = windows_handle_info(&file)?;
        if information.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Handle-bound file deletion does not accept directories",
            ));
        }
        if information.identity != expected {
            return Err(io::Error::other(format!(
                "Filesystem object changed before handle-bound deletion: '{}'",
                path.display()
            )));
        }
        Ok(PreparedFileDeletion { file })
    }
    #[cfg(not(windows))]
    {
        if stable_path_identity(path)? != expected {
            return Err(io::Error::other(format!(
                "Filesystem object changed before deletion: '{}'",
                path.display()
            )));
        }
        Ok(PreparedFileDeletion {
            path: path.to_path_buf(),
            expected,
        })
    }
}

impl PreparedFileDeletion {
    #[allow(unsafe_code)]
    pub(crate) fn delete(self) -> io::Result<()> {
        #[cfg(windows)]
        {
            use std::ffi::c_void;
            use std::os::windows::io::AsRawHandle;

            #[repr(C)]
            struct FileDispositionInfo {
                delete_file: u8,
            }
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn SetFileInformationByHandle(
                    handle: *mut c_void,
                    information_class: i32,
                    information: *const c_void,
                    buffer_size: u32,
                ) -> i32;
            }

            const FILE_DISPOSITION_INFO_CLASS: i32 = 4;
            let disposition = FileDispositionInfo { delete_file: 1 };
            let result = unsafe {
                SetFileInformationByHandle(
                    self.file.as_raw_handle().cast(),
                    FILE_DISPOSITION_INFO_CLASS,
                    (&disposition as *const FileDispositionInfo).cast(),
                    std::mem::size_of::<FileDispositionInfo>() as u32,
                )
            };
            if result == 0 {
                return Err(io::Error::last_os_error());
            }
            drop(self.file);
            Ok(())
        }
        #[cfg(not(windows))]
        {
            if stable_path_identity(&self.path)? != self.expected {
                return Err(io::Error::other(format!(
                    "Filesystem object changed before deletion: '{}'",
                    self.path.display()
                )));
            }
            fs::remove_file(&self.path)
        }
    }
}

/// Remove exactly the non-directory filesystem object identified by
/// `expected`. Windows deletion is committed through a verified handle rather
/// than by reopening the pathname at the remove call.
pub(crate) fn remove_file_by_identity(path: &Path, expected: StablePathIdentity) -> io::Result<()> {
    prepare_file_deletion(path, expected)?.delete()
}

#[derive(Debug)]
struct PathIdentity {
    stable: StablePathIdentity,
    is_directory: bool,
    #[cfg(windows)]
    _handle: File,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    size: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(windows)]
    creation_time: u64,
    #[cfg(windows)]
    last_write_time: u64,
    #[cfg(windows)]
    size: u64,
    #[cfg(windows)]
    attributes: u32,
    #[cfg(not(any(unix, windows)))]
    len: u64,
}

/// Handle-free snapshot retained while an overwrite dialog is open. Keeping
/// thousands of Windows handles alive across user interaction would both
/// exhaust resources and interfere with later delete-pending cleanup.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PathAuthorization {
    stable: StablePathIdentity,
    is_directory: bool,
    #[cfg(unix)]
    size: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(windows)]
    creation_time: u64,
    #[cfg(windows)]
    last_write_time: u64,
    #[cfg(windows)]
    size: u64,
    #[cfg(windows)]
    attributes: u32,
    #[cfg(not(any(unix, windows)))]
    len: u64,
}

impl PathAuthorization {
    fn from_identity(identity: &PathIdentity) -> Self {
        Self {
            stable: identity.stable,
            is_directory: identity.is_directory,
            #[cfg(unix)]
            size: identity.size,
            #[cfg(unix)]
            modified_seconds: identity.modified_seconds,
            #[cfg(unix)]
            modified_nanoseconds: identity.modified_nanoseconds,
            #[cfg(unix)]
            changed_seconds: identity.changed_seconds,
            #[cfg(unix)]
            changed_nanoseconds: identity.changed_nanoseconds,
            #[cfg(windows)]
            creation_time: identity.creation_time,
            #[cfg(windows)]
            last_write_time: identity.last_write_time,
            #[cfg(windows)]
            size: identity.size,
            #[cfg(windows)]
            attributes: identity.attributes,
            #[cfg(not(any(unix, windows)))]
            len: identity.len,
        }
    }

    fn matches_snapshot(&self, current: &PathIdentity) -> bool {
        if current.stable != self.stable || current.is_directory != self.is_directory {
            return false;
        }
        #[cfg(unix)]
        {
            current.size == self.size
                && current.modified_seconds == self.modified_seconds
                && current.modified_nanoseconds == self.modified_nanoseconds
                && current.changed_seconds == self.changed_seconds
                && current.changed_nanoseconds == self.changed_nanoseconds
        }
        #[cfg(windows)]
        {
            current.creation_time == self.creation_time
                && current.last_write_time == self.last_write_time
                && current.size == self.size
                && current.attributes == self.attributes
        }
        #[cfg(not(any(unix, windows)))]
        {
            current.len == self.len
        }
    }
}

pub(crate) fn capture_path_authorization(path: &Path) -> io::Result<PathAuthorization> {
    let identity = path_identity(path)?;
    Ok(PathAuthorization::from_identity(&identity))
}

#[derive(Debug, Clone)]
pub(crate) struct DirectoryAuthorization {
    resolved: PathBuf,
    object: PathAuthorization,
}

impl DirectoryAuthorization {
    pub(crate) fn resolved_path(&self) -> &Path {
        &self.resolved
    }
}

pub(crate) fn capture_directory_authorization(path: &Path) -> io::Result<DirectoryAuthorization> {
    let resolved = path.canonicalize().map(strip_unc_prefix)?;
    let identity = path_identity(&resolved)?;
    if !identity.is_directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Path is not a real directory: '{}'", path.display()),
        ));
    }
    Ok(DirectoryAuthorization {
        resolved,
        object: PathAuthorization::from_identity(&identity),
    })
}

fn authorized_current_identity(
    path: &Path,
    authorization: &PathAuthorization,
    role: &str,
) -> io::Result<PathIdentity> {
    let current = path_identity(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{} changed before the operation started: {}", role, error),
        )
    })?;
    if authorization.matches_snapshot(&current) {
        Ok(current)
    } else {
        Err(io::Error::other(format!(
            "{} changed after confirmation; retry the operation",
            role
        )))
    }
}

pub(crate) fn verify_path_authorization(
    path: &Path,
    authorization: &PathAuthorization,
    role: &str,
) -> io::Result<()> {
    authorized_current_identity(path, authorization, role).map(drop)
}

fn authorized_current_directory(
    _path: &Path,
    authorization: &DirectoryAuthorization,
    role: &str,
) -> io::Result<PathIdentity> {
    let current = path_identity(&authorization.resolved).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{} changed before the operation started: {}", role, error),
        )
    })?;
    if current.is_directory && current.stable == authorization.object.stable {
        Ok(current)
    } else {
        Err(io::Error::other(format!(
            "{} was replaced after confirmation; retry the operation",
            role
        )))
    }
}

pub(crate) fn verify_directory_authorization(
    path: &Path,
    authorization: &DirectoryAuthorization,
    role: &str,
) -> io::Result<()> {
    authorized_current_directory(path, authorization, role).map(drop)
}

fn path_identity(path: &Path) -> io::Result<PathIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path)?;
        Ok(PathIdentity {
            stable: StablePathIdentity {
                namespace: metadata.dev(),
                object: {
                    let mut object = [0u8; 16];
                    object[..8].copy_from_slice(&metadata.ino().to_le_bytes());
                    object
                },
            },
            is_directory: metadata.file_type().is_dir(),
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }
    #[cfg(windows)]
    {
        let handle = open_windows_path_identity(path)?;
        let information = windows_handle_info(&handle)?;
        Ok(PathIdentity {
            stable: information.identity,
            is_directory: information.attributes & 0x0010 != 0,
            _handle: handle,
            creation_time: information.creation_time,
            last_write_time: information.last_write_time,
            size: information.size,
            attributes: information.attributes,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Stable filesystem object identity is unavailable on this platform",
        ))
    }
}

impl PathIdentity {
    fn same_snapshot(&self, expected: &Self) -> bool {
        if self.stable != expected.stable || self.is_directory != expected.is_directory {
            return false;
        }
        #[cfg(unix)]
        {
            self.device == expected.device
                && self.inode == expected.inode
                && self.size == expected.size
                && self.modified_seconds == expected.modified_seconds
                && self.modified_nanoseconds == expected.modified_nanoseconds
                && self.changed_seconds == expected.changed_seconds
                && self.changed_nanoseconds == expected.changed_nanoseconds
        }
        #[cfg(windows)]
        {
            self.creation_time == expected.creation_time
                && self.last_write_time == expected.last_write_time
                && self.size == expected.size
                && self.attributes == expected.attributes
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.len == expected.len
        }
    }

    fn matches_after_relocation(&self, expected: &Self) -> bool {
        if self.stable != expected.stable || self.is_directory != expected.is_directory {
            return false;
        }
        #[cfg(unix)]
        {
            self.device == expected.device
                && self.inode == expected.inode
                && self.size == expected.size
                && self.modified_seconds == expected.modified_seconds
                && self.modified_nanoseconds == expected.modified_nanoseconds
        }
        #[cfg(windows)]
        {
            self.creation_time == expected.creation_time
                && self.last_write_time == expected.last_write_time
                && self.size == expected.size
                && self.attributes == expected.attributes
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.len == expected.len
        }
    }
}

pub(crate) fn metadata_still_matches(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.size() == after.size()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        before.creation_time() == after.creation_time()
            && before.last_write_time() == after.last_write_time()
            && before.file_size() == after.file_size()
            && before.file_attributes() == after.file_attributes()
    }
    #[cfg(not(any(unix, windows)))]
    {
        before.len() == after.len() && before.modified().ok() == after.modified().ok()
    }
}

pub(crate) fn create_private_quarantine_directory(
    parent: &Path,
    label: &str,
) -> io::Result<PathBuf> {
    let mut last_collision = None;
    for _ in 0..128 {
        let directory = parent.join(format!(
            ".cokacdir-{}-{}-{:032x}",
            label,
            std::process::id(),
            rand::random::<u128>()
        ));
        match create_private_directory(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(error)
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_collision.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Unable to allocate a private deletion quarantine",
        )
    }))
}

struct PrivateStagingDirectory {
    path: PathBuf,
    stable: StablePathIdentity,
}

struct OwnedStage {
    directory: PrivateStagingDirectory,
    identity: PathIdentity,
}

/// A payload that was created inside a private staging directory but is still
/// being populated.  Its size and timestamps are expected to change, so only
/// the stable filesystem object identity is bound until the copy is sealed.
struct PartialStage {
    directory: PrivateStagingDirectory,
    stable: StablePathIdentity,
    is_directory: bool,
}

struct StagePublishFailure {
    error: io::Error,
    stage: OwnedStage,
}

struct PublishedStage {
    identity: Option<PathIdentity>,
    warnings: Vec<String>,
}

impl PartialStage {
    fn bind(directory: PrivateStagingDirectory) -> io::Result<Self> {
        let path = directory.payload();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Created staging payload could not be inspected and was preserved at '{}': {}",
                    directory.path.display(),
                    error
                ),
            )
        })?;
        let stable = stable_path_identity(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Created staging payload could not be bound and was preserved at '{}': {}",
                    directory.path.display(),
                    error
                ),
            )
        })?;
        Ok(Self {
            directory,
            stable,
            is_directory: metadata.is_dir() && !metadata.is_symlink(),
        })
    }

    fn bind_file(directory: PrivateStagingDirectory, file: &File) -> io::Result<Self> {
        let stable = stable_file_identity(file)?;
        let path = directory.payload();
        if stable_path_identity(&path)? != stable {
            return Err(io::Error::other(format!(
                "Created staging file was replaced and the staging directory was preserved at '{}'",
                directory.path.display()
            )));
        }
        Ok(Self {
            directory,
            stable,
            is_directory: false,
        })
    }

    fn bind_directory(directory: PrivateStagingDirectory) -> io::Result<Self> {
        let path = directory.payload();
        let (handle, _, metadata) = open_directory_for_read(&path)?;
        let stable = stable_file_identity(&handle)?;
        if !metadata.is_dir() || stable_path_identity(&path)? != stable {
            return Err(io::Error::other(format!(
                "Created staging directory was replaced and was preserved at '{}'",
                directory.path.display()
            )));
        }
        Ok(Self {
            directory,
            stable,
            is_directory: true,
        })
    }

    fn path(&self) -> PathBuf {
        self.directory.payload()
    }

    fn seal(self) -> io::Result<OwnedStage> {
        let current = path_identity(&self.path()).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Completed staging payload could not be rebound and was preserved at '{}': {}",
                    self.directory.path.display(),
                    error
                ),
            )
        })?;
        if current.stable != self.stable || current.is_directory != self.is_directory {
            return Err(io::Error::other(format!(
                "Staging payload changed while it was being populated and was preserved at '{}'",
                self.directory.path.display()
            )));
        }
        Ok(OwnedStage {
            directory: self.directory,
            identity: current,
        })
    }

    fn cleanup(self) -> io::Result<()> {
        let path = self.path();
        let current = match path_identity(&path) {
            Ok(current)
                if current.stable == self.stable && current.is_directory == self.is_directory =>
            {
                current
            }
            Ok(_) => {
                return Err(io::Error::other(format!(
                    "Partial staging payload changed and was preserved at '{}'",
                    self.directory.path.display()
                )))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return self.directory.cleanup()
            }
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                    "Partial staging payload could not be verified and was preserved at '{}': {}",
                    self.directory.path.display(),
                    error
                ),
                ))
            }
        };
        delete_replacement_backup_if_unchanged(&path, current)?;
        self.directory.cleanup()
    }
}

impl OwnedStage {
    fn bind(directory: PrivateStagingDirectory) -> io::Result<Self> {
        PartialStage::bind(directory)?.seal()
    }

    fn from_published(
        directory: PrivateStagingDirectory,
        identity: PathIdentity,
    ) -> io::Result<Self> {
        let current = path_identity(&directory.payload())?;
        if !current.same_snapshot(&identity) {
            return Err(io::Error::other(format!(
                "Published staging payload changed before it could be adopted and was preserved at '{}'",
                directory.path.display()
            )));
        }
        drop(current);
        Ok(Self {
            directory,
            identity,
        })
    }

    fn path(&self) -> PathBuf {
        self.directory.payload()
    }

    fn verify(&self) -> io::Result<()> {
        let current = path_identity(&self.path())?;
        if current.same_snapshot(&self.identity) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "Private staging payload changed before publication: '{}'",
                self.path().display()
            )))
        }
    }

    fn cleanup(self) -> io::Result<()> {
        let OwnedStage {
            directory,
            identity,
        } = self;
        let path = directory.payload();
        match path_identity(&path) {
            Ok(current) if current.same_snapshot(&identity) => {
                // On Windows PathIdentity owns a delete-sharing handle.  Close
                // the original binding before deleting the payload and its
                // parent staging directory.
                drop(identity);
                delete_replacement_backup_if_unchanged(&path, current)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(io::Error::other(format!(
                    "Private staging payload changed and the staging directory was preserved at '{}'",
                    directory.path.display()
                )))
            }
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "Private staging payload could not be verified; staging was preserved at '{}': {}",
                        directory.path.display(), error
                    ),
                ))
            }
        }
        directory.cleanup()
    }

    fn publish_noreplace(self, destination: &Path) -> Result<PublishedStage, StagePublishFailure> {
        if let Err(error) = self.verify() {
            return Err(StagePublishFailure { error, stage: self });
        }
        let OwnedStage {
            directory,
            identity,
        } = self;
        let source_path = directory.payload();
        if let Err(error) = rename_noreplace(&source_path, destination) {
            return Err(StagePublishFailure {
                error,
                stage: OwnedStage {
                    directory,
                    identity,
                },
            });
        }

        let published_identity = match path_identity(destination) {
            Ok(current) if current.matches_after_relocation(&identity) => Some(current),
            _ => None,
        };
        drop(identity);
        let mut warnings = Vec::new();
        if published_identity.is_none() {
            warnings.push(format!(
                "Published destination could not be rebound to the verified staging object: '{}'",
                destination.display()
            ));
        }
        if let Err(error) = sync_parent(destination) {
            warnings.push(format!(
                "'{}' was published, but parent-directory durability could not be confirmed: {}",
                destination.display(),
                error
            ));
        }
        if let Err(error) = sync_parent(&source_path) {
            warnings.push(format!(
                "'{}' was published, but staging-directory durability could not be confirmed: {}",
                destination.display(),
                error
            ));
        }
        if let Err(error) = directory.cleanup() {
            warnings.push(format!(
                "'{}' was published, but private staging cleanup could not be confirmed: {}",
                destination.display(),
                error
            ));
        }
        Ok(PublishedStage {
            identity: published_identity,
            warnings,
        })
    }
}

impl PrivateStagingDirectory {
    fn create(parent: &Path, label: &str) -> io::Result<Self> {
        let path = create_private_quarantine_directory(parent, label)?;
        let stable = match stable_path_identity(&path) {
            Ok(identity) => identity,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "Created private staging directory could not be bound safely and was preserved at '{}': {}",
                        path.display(), error
                    ),
                ));
            }
        };
        if let Err(error) = sync_parent(&path) {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "Created private staging directory could not be made durable and was preserved at '{}': {}",
                    path.display(), error
                ),
            ));
        }
        Ok(Self { path, stable })
    }

    fn payload(&self) -> PathBuf {
        self.path.join("payload")
    }

    fn cleanup(self) -> io::Result<()> {
        cleanup_private_staging_directory(&self.path, self.stable)
    }
}

fn cleanup_private_staging_directory(
    staging: &Path,
    expected: StablePathIdentity,
) -> io::Result<()> {
    if stable_path_identity(staging)? != expected {
        return Err(io::Error::other(format!(
            "Private staging directory changed and was preserved at '{}'",
            staging.display()
        )));
    }

    let parent = staging.parent().unwrap_or_else(|| Path::new("."));
    let quarantine = create_private_quarantine_directory(parent, "staging-cleanup")?;
    let moved = quarantine.join("staging");
    if let Err(error) = rename_noreplace(staging, &moved) {
        let _ = fs::remove_dir(&quarantine);
        return Err(error);
    }
    let moved_identity = stable_path_identity(&moved).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Moved staging directory could not be verified and was preserved at '{}': {}",
                moved.display(),
                error
            ),
        )
    })?;
    if moved_identity != expected {
        return Err(io::Error::other(format!(
            "Private staging directory was replaced during cleanup; the moved entry is preserved at '{}'",
            moved.display()
        )));
    }
    if let Err(error) = fs::remove_dir(&moved) {
        return Err(io::Error::new(
            error.kind(),
            format!(
                "Private staging directory was not empty or could not be removed: {}. It was preserved under '{}'",
                error,
                quarantine.display()
            ),
        ));
    }
    fs::remove_dir(&quarantine)?;
    sync_parent(staging)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn rename_noreplace(src: &Path, dest: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let src = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid source path"))?;
    let dest = CString::new(dest.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid destination path"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            src.as_ptr(),
            libc::AT_FDCWD,
            dest.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS) | Some(libc::EINVAL)
        ) {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "This filesystem does not support atomic no-clobber rename",
            ))
        } else {
            Err(error)
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn rename_noreplace(src: &Path, dest: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let src = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid source path"))?;
    let dest = CString::new(dest.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid destination path"))?;
    let result = unsafe { libc::renamex_np(src.as_ptr(), dest.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn rename_noreplace(src: &Path, dest: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut value = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if value.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows path contains a NUL character",
            ));
        }
        value.push(0);
        Ok(value)
    }

    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
    let src = wide_path(src)?;
    let dest = wide_path(dest)?;
    let result = unsafe { MoveFileExW(src.as_ptr(), dest.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if result != 0 {
        return Ok(());
    }

    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(80 | 183)) {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            error.to_string(),
        ))
    } else {
        Err(error)
    }
}

// Avoid silently using POSIX rename semantics (which replace an existing
// destination) on platforms without a known atomic no-replace primitive.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
)))]
pub(crate) fn rename_noreplace(_src: &Path, _dest: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Atomic no-clobber rename is not supported on this platform",
    ))
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Debug)]
enum PublishNoreplaceOutcome {
    Durable,
    CommittedWithDurabilityWarning(io::Error),
}

#[derive(Debug)]
enum VerifiedCleanupOutcome {
    Removed,
    RemovedWithDurabilityWarning(io::Error),
}

impl PublishNoreplaceOutcome {
    fn warning(self) -> Option<io::Error> {
        match self {
            Self::Durable => None,
            Self::CommittedWithDurabilityWarning(error) => Some(error),
        }
    }
}

fn publish_noreplace(temp_path: &Path, dest: &Path) -> io::Result<PublishNoreplaceOutcome> {
    rename_noreplace(temp_path, dest)?;
    let mut failures = Vec::new();
    if let Err(error) = sync_parent(dest) {
        failures.push(format!("destination parent: {}", error));
    }
    if temp_path.parent() != dest.parent() {
        if let Err(error) = sync_parent(temp_path) {
            failures.push(format!("staging parent: {}", error));
        }
    }
    Ok(if failures.is_empty() {
        PublishNoreplaceOutcome::Durable
    } else {
        PublishNoreplaceOutcome::CommittedWithDurabilityWarning(io::Error::other(
            failures.join("; "),
        ))
    })
}

#[cfg(test)]
fn install_completed_replacement(temp_path: &Path, dest: &Path) -> io::Result<()> {
    install_completed_replacement_impl(temp_path, dest, None, None, |_| {}).map(|_| ())
}

fn install_completed_replacement_if_unchanged(
    temp_path: &Path,
    dest: &Path,
    expected_destination: PathIdentity,
    expected_candidate: Option<&PathIdentity>,
) -> io::Result<Vec<String>> {
    install_completed_replacement_impl(
        temp_path,
        dest,
        Some(expected_destination),
        expected_candidate,
        |_| {},
    )
}

fn install_owned_replacement_if_unchanged(
    stage: OwnedStage,
    destination: &Path,
    expected_destination: PathIdentity,
) -> io::Result<Vec<String>> {
    let stage_path = stage.path();
    let result = install_completed_replacement_if_unchanged(
        &stage_path,
        destination,
        expected_destination,
        Some(&stage.identity),
    );
    match result {
        Ok(mut warnings) => {
            if let Err(error) = stage.cleanup() {
                warnings.push(format!(
                    "Replacement committed at '{}', but owned staging cleanup could not be confirmed: {}",
                    destination.display(), error
                ));
            }
            Ok(warnings)
        }
        Err(error) => {
            let retry_unsafe = is_retry_unsafe(&error);
            Err(match stage.cleanup() {
                Ok(()) => error,
                Err(cleanup_error) => operation_error(
                    error.kind(),
                    format!(
                        "{}; owned staging cleanup also failed: {}",
                        error, cleanup_error
                    ),
                    retry_unsafe,
                ),
            })
        }
    }
}

fn install_completed_replacement_impl<F>(
    temp_path: &Path,
    dest: &Path,
    expected_destination: Option<PathIdentity>,
    expected_candidate: Option<&PathIdentity>,
    after_backup: F,
) -> io::Result<Vec<String>>
where
    F: FnOnce(&Path),
{
    let mut terminal_failure = false;
    let result = install_completed_replacement_impl_inner(
        temp_path,
        dest,
        expected_destination,
        expected_candidate,
        after_backup,
        &mut terminal_failure,
    );
    match result {
        Err(error) if terminal_failure && !is_retry_unsafe(&error) => {
            Err(operation_error(error.kind(), error.to_string(), true))
        }
        other => other,
    }
}

fn install_completed_replacement_detailed(
    temp_path: &Path,
    dest: &Path,
    expected_destination: PathIdentity,
    expected_candidate: &PathIdentity,
) -> (io::Result<Vec<String>>, bool) {
    let mut terminal_failure = false;
    let result = install_completed_replacement_impl_inner(
        temp_path,
        dest,
        Some(expected_destination),
        Some(expected_candidate),
        |_| {},
        &mut terminal_failure,
    );
    (result, terminal_failure)
}

fn install_completed_replacement_impl_inner<F>(
    temp_path: &Path,
    dest: &Path,
    mut expected_destination: Option<PathIdentity>,
    expected_candidate: Option<&PathIdentity>,
    after_backup: F,
    terminal_failure: &mut bool,
) -> io::Result<Vec<String>>
where
    F: FnOnce(&Path),
{
    if let Some(expected) = expected_candidate {
        let current = path_identity(temp_path)?;
        if !current.same_snapshot(expected) {
            return Err(io::Error::other(format!(
                "Replacement staging payload changed before destination backup: '{}'",
                temp_path.display()
            )));
        }
    }

    // POSIX rename replaces files atomically. Prefer that single operation
    // where available; Windows and non-empty destination directories fall
    // through to the recoverable backup transaction below.
    if expected_destination.is_none() {
        match fs::rename(temp_path, dest) {
            Ok(()) => {
                return Ok(sync_parent(dest)
                    .err()
                    .map(|error| {
                        format!(
                            "Replacement committed at '{}', but parent-directory durability could not be confirmed: {}",
                            dest.display(), error
                        )
                    })
                    .into_iter()
                    .collect())
            }
            Err(error) if !path_exists_no_follow(dest) => return Err(error),
            Err(_) => {}
        }
    } else {
        let current = path_identity(dest).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Destination changed before replacement and could not be verified: {}",
                    error
                ),
            )
        })?;
        if !current.same_snapshot(expected_destination.as_ref().expect("checked above")) {
            return Err(io::Error::other(format!(
                "Destination changed while the replacement was being prepared; refusing to overwrite '{}'",
                dest.display()
            )));
        }
    }

    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let mut backup = None;
    let mut last_error = None;
    for _ in 0..128 {
        let candidate = parent.join(format!(
            ".cokacdir_backup_{}_{:032x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        match rename_noreplace(dest, &candidate) {
            Ok(()) => {
                backup = Some(candidate);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    let backup = backup.ok_or_else(|| {
        last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Unable to allocate a recovery backup path",
            )
        })
    })?;

    let backup_identity = match path_identity(&backup) {
        Ok(identity) => identity,
        Err(error) => {
            *terminal_failure = true;
            return Err(operation_error(
                error.kind(),
                format!(
                    "The backed-up destination could not be verified after relocation and was not restored. Inspect recovery path '{}': {}",
                    backup.display(), error
                ),
                true,
            ));
        }
    };
    if let Some(expected) = expected_destination.as_ref() {
        let moved = &backup_identity;
        if !moved.matches_after_relocation(expected) {
            *terminal_failure = true;
            return Err(operation_error(
                io::ErrorKind::Other,
                format!(
                    "Destination was replaced during backup; the unverified entry was not restored. Inspect recovery path '{}'",
                    backup.display()
                ),
                true,
            ));
        }
    }

    // The original destination identity may own a Windows handle that now
    // follows the entry at the backup path.  It is no longer needed after the
    // relocation check and must be closed before backup cleanup can remove its
    // quarantine directory.
    drop(expected_destination.take());

    // Persist the recoverable state before attempting the second rename.
    if let Err(sync_error) = sync_parent(dest) {
        let error = restore_verified_replacement_backup(
            &backup,
            dest,
            &backup_identity,
            io::Error::new(
                sync_error.kind(),
                format!("Could not persist the replacement backup: {}", sync_error),
            ),
        );
        *terminal_failure = is_retry_unsafe(&error);
        return Err(error);
    }
    after_backup(&backup);

    if let Some(expected) = expected_candidate {
        let current = match path_identity(temp_path) {
            Ok(current) => current,
            Err(error) => {
                let error = restore_verified_replacement_backup(
                    &backup,
                    dest,
                    &backup_identity,
                    io::Error::new(
                        error.kind(),
                        format!("Replacement staging payload became unavailable: {error}"),
                    ),
                );
                *terminal_failure = is_retry_unsafe(&error);
                return Err(error);
            }
        };
        if !current.same_snapshot(expected) {
            drop(current);
            let error = restore_verified_replacement_backup(
                &backup,
                dest,
                &backup_identity,
                io::Error::other(
                    "Replacement staging payload changed after the destination was backed up",
                ),
            );
            *terminal_failure = is_retry_unsafe(&error);
            return Err(error);
        }
    }

    match publish_noreplace(temp_path, dest) {
        Ok(published) => {
            let mut warnings = published
                .warning()
                .map(|error| {
                    format!(
                        "Replacement committed at '{}', but parent-directory durability could not be confirmed: {}",
                        dest.display(), error
                    )
                })
                .into_iter()
                .collect::<Vec<_>>();
            if let Some(expected) = expected_candidate {
                let installed_matches = matches!(
                    path_identity(dest),
                    Ok(current) if current.matches_after_relocation(expected)
                );
                if !installed_matches {
                    *terminal_failure = true;
                    return Err(io::Error::other(format!(
                        "Replacement publication committed at '{}', but it could not be rebound to the verified staging object. The previous target is preserved at recovery path '{}'; inspect both paths manually",
                        dest.display(), backup.display()
                    )));
                }
            }
            match delete_replacement_backup_if_unchanged(&backup, backup_identity) {
                Ok(VerifiedCleanupOutcome::Removed) => {}
                Ok(VerifiedCleanupOutcome::RemovedWithDurabilityWarning(error)) => {
                    warnings.push(format!(
                        "Replacement succeeded and the previous target was removed, but cleanup durability could not be confirmed for '{}': {}",
                        dest.display(), error
                    ));
                }
                Err(error) => {
                    warnings.push(format!(
                        "Replacement succeeded, but previous-target cleanup could not be confirmed (recovery started at '{}'): {}",
                        backup.display(), error
                    ));
                }
            }
            if let Err(error) = sync_parent(dest) {
                warnings.push(format!(
                    "Replacement cleanup completed, but final parent-directory durability could not be confirmed for '{}': {}",
                    dest.display(), error
                ));
            }
            Ok(warnings)
        }
        Err(publish_error) => {
            let error = restore_verified_replacement_backup(
                &backup,
                dest,
                &backup_identity,
                io::Error::new(
                    publish_error.kind(),
                    format!("Failed to replace '{}': {}", dest.display(), publish_error),
                ),
            );
            *terminal_failure = is_retry_unsafe(&error);
            Err(error)
        }
    }
}

fn delete_replacement_backup_if_unchanged(
    backup: &Path,
    expected: PathIdentity,
) -> io::Result<VerifiedCleanupOutcome> {
    let current = path_identity(backup)?;
    if !current.same_snapshot(&expected) {
        return Err(io::Error::other(format!(
            "Recovery backup changed before cleanup and was preserved at '{}'",
            backup.display()
        )));
    }
    drop(current);

    let parent = backup.parent().unwrap_or_else(|| Path::new("."));
    let quarantine = create_private_quarantine_directory(parent, "replacement-cleanup")?;
    let quarantined = quarantine.join("previous-target");
    if let Err(error) = rename_noreplace(backup, &quarantined) {
        let _ = fs::remove_dir(&quarantine);
        return Err(error);
    }

    let moved = match path_identity(&quarantined) {
        Ok(identity) => identity,
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "Recovery backup cleanup could not verify the moved entry: {}. It is preserved at '{}'",
                    error,
                    quarantined.display()
                ),
            ));
        }
    };
    if !moved.matches_after_relocation(&expected) {
        return Err(io::Error::other(format!(
            "Recovery backup was replaced during cleanup; the moved entry is preserved at '{}'",
            quarantined.display()
        )));
    }
    let prepared_file = if moved.is_directory {
        None
    } else {
        Some(prepare_file_deletion(&quarantined, moved.stable).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Verified previous-target deletion could not be bound: {}. Recovery data remains under '{}'",
                    error,
                    quarantine.display()
                ),
            )
        })?)
    };
    drop(moved);
    drop(expected);

    let deletion = match prepared_file {
        Some(prepared) => prepared.delete(),
        None => delete_path_unchecked(&quarantined),
    };
    if let Err(error) = deletion {
        return Err(io::Error::new(
            error.kind(),
            format!(
                "Could not remove the verified previous target: {}. Remaining recovery data is preserved under '{}'",
                error,
                quarantine.display()
            ),
        ));
    }
    if let Err(error) = fs::remove_dir(&quarantine) {
        return Ok(VerifiedCleanupOutcome::RemovedWithDurabilityWarning(
            io::Error::new(
                error.kind(),
                format!(
                    "verified data was removed, but cleanup directory '{}' remains: {}",
                    quarantine.display(),
                    error
                ),
            ),
        ));
    }
    Ok(match sync_parent(backup) {
        Ok(()) => VerifiedCleanupOutcome::Removed,
        Err(error) => VerifiedCleanupOutcome::RemovedWithDurabilityWarning(error),
    })
}

fn restore_verified_replacement_backup(
    backup: &Path,
    destination: &Path,
    expected: &PathIdentity,
    cause: io::Error,
) -> io::Error {
    match path_identity(backup) {
        Ok(current) if current.same_snapshot(expected) => drop(current),
        Ok(_) => {
            return operation_error(
                cause.kind(),
                format!(
                    "{}; recovery backup changed and was not restored. Inspect '{}' and '{}' manually",
                    cause,
                    backup.display(),
                    destination.display()
                ),
                true,
            )
        }
        Err(identity_error) => {
            return operation_error(
                cause.kind(),
                format!(
                    "{}; recovery backup could not be verified and was not restored: {}. Inspect '{}' and '{}' manually",
                    cause,
                    identity_error,
                    backup.display(),
                    destination.display()
                ),
                true,
            )
        }
    }

    if let Err(restore_error) = rename_noreplace(backup, destination) {
        return operation_error(
            cause.kind(),
            format!(
                "{}; restore failed without clobbering '{}': {}. The verified original target is preserved at recovery path '{}'",
                cause,
                destination.display(),
                restore_error,
                backup.display()
            ),
            true,
        );
    }
    let restored = matches!(
        path_identity(destination),
        Ok(current) if current.matches_after_relocation(expected)
    );
    if !restored {
        return operation_error(
            cause.kind(),
            format!(
                "{}; a restore rename committed at '{}', but the restored object could not be rebound. Inspect it manually",
                cause,
                destination.display()
            ),
            true,
        );
    }
    match sync_parent(destination) {
        Ok(()) => io::Error::new(
            cause.kind(),
            format!("{}; the backed-up entry was restored", cause),
        ),
        Err(sync_error) => io::Error::new(
            cause.kind(),
            format!(
                "{}; the backed-up entry was restored, but parent-directory durability could not be confirmed: {}",
                cause, sync_error
            ),
        ),
    }
}

fn restore_staged_directory_after_failure(
    source: &Path,
    staging: &Path,
    destination: &Path,
    expected: &PathIdentity,
    cause: io::Error,
) -> io::Error {
    if !path_exists_no_follow(staging) {
        return operation_error(
            cause.kind(),
            format!(
                "{}; the staged source is no longer at '{}'. Inspect destination '{}' and any recovery path reported above",
                cause,
                staging.display(),
                destination.display()
            ),
            true,
        );
    }

    match path_identity(staging) {
        Ok(current) if current.matches_after_relocation(expected) => {}
        Ok(_) => {
            return operation_error(
                cause.kind(),
                format!(
                    "{}; the directory at recovery path '{}' is no longer the verified source, so it was not moved. Inspect it and destination '{}'",
                    cause,
                    staging.display(),
                    destination.display()
                ),
                true,
            );
        }
        Err(identity_error) => {
            return operation_error(
                cause.kind(),
                format!(
                    "{}; recovery path '{}' could not be rebound to the verified source and was not moved: {}. Inspect destination '{}'",
                    cause,
                    staging.display(),
                    identity_error,
                    destination.display()
                ),
                true,
            );
        }
    }

    match rename_noreplace(staging, source) {
        Ok(()) => {
            if matches!(
                path_identity(source),
                Ok(current) if current.matches_after_relocation(expected)
            ) {
                let mut sync_failures = Vec::new();
                if let Err(error) = sync_parent(source) {
                    sync_failures.push(format!("source parent: {error}"));
                }
                if source.parent() != staging.parent() {
                    if let Err(error) = sync_parent(staging) {
                        sync_failures.push(format!("staging parent: {error}"));
                    }
                }
                if sync_failures.is_empty() {
                    io::Error::new(
                        cause.kind(),
                        format!("{}; the original source was restored", cause),
                    )
                } else {
                    io::Error::new(
                        cause.kind(),
                        format!(
                            "{}; the original source was restored, but directory durability could not be confirmed ({})",
                            cause,
                            sync_failures.join("; ")
                        ),
                    )
                }
            } else {
                operation_error(
                    cause.kind(),
                    format!(
                        "{}; source restore committed at '{}', but the restored object could not be rebound; inspect it manually",
                        cause,
                        source.display()
                    ),
                    true,
                )
            }
        }
        Err(restore_error) => operation_error(
            cause.kind(),
            format!(
                "{}; automatic source restore to '{}' failed without clobbering it: {}. The source is preserved at recovery path '{}'",
                cause,
                source.display(),
                restore_error,
                staging.display()
            ),
            true,
        ),
    }
}

struct QuarantinedSource {
    original: PathBuf,
    directory: PrivateStagingDirectory,
    identity: PathIdentity,
    verified_directory_digest: Option<[u8; 32]>,
}

impl QuarantinedSource {
    fn prepare(original: &Path, expected: PathIdentity) -> io::Result<Self> {
        Self::prepare_impl(original, expected, |_| {})
    }

    fn prepare_impl<F>(
        original: &Path,
        expected: PathIdentity,
        after_snapshot_check: F,
    ) -> io::Result<Self>
    where
        F: FnOnce(&Path),
    {
        let current = path_identity(original)?;
        if !current.same_snapshot(&expected) {
            return Err(io::Error::other(format!(
                "Source changed before it could be isolated for move: '{}'",
                original.display()
            )));
        }
        drop(current);
        after_snapshot_check(original);

        let parent = original.parent().unwrap_or_else(|| Path::new("."));
        let directory = PrivateStagingDirectory::create(parent, "move-source")?;
        let payload = directory.payload();
        if let Err(error) = rename_noreplace(original, &payload) {
            return Err(error_with_staging_cleanup(error, directory));
        }

        let moved = match path_identity(&payload) {
            Ok(identity) if identity.matches_after_relocation(&expected) => identity,
            Ok(_) => {
                let error = restore_staged_directory_after_failure(
                    original,
                    &payload,
                    original,
                    &expected,
                    io::Error::other(
                        "Source changed while it was being isolated for cross-filesystem move",
                    ),
                );
                return Err(error_with_staging_cleanup(error, directory));
            }
            Err(error) => {
                let error = restore_staged_directory_after_failure(
                    original, &payload, original, &expected, error,
                );
                return Err(error_with_staging_cleanup(error, directory));
            }
        };

        if let Err(error) = sync_parent(original).and_then(|()| sync_parent(&payload)) {
            drop(moved);
            let error = restore_staged_directory_after_failure(
                original, &payload, original, &expected, error,
            );
            return Err(error_with_staging_cleanup(error, directory));
        }
        drop(expected);

        Ok(Self {
            original: original.to_path_buf(),
            directory,
            identity: moved,
            verified_directory_digest: None,
        })
    }

    fn path(&self) -> PathBuf {
        self.directory.payload()
    }

    fn verify_unchanged(&self) -> io::Result<()> {
        let current = path_identity(&self.path())?;
        if !current.same_snapshot(&self.identity) {
            return Err(io::Error::other(format!(
                "Isolated move source changed before destination publication: '{}'",
                self.original.display()
            )));
        }
        if let Some(expected_digest) = self.verified_directory_digest {
            let current_digest = sha256_directory_tree_snapshot(&self.path(), &self.identity)?;
            if current_digest != expected_digest {
                return Err(io::Error::other(format!(
                    "Isolated move source tree changed before destination publication: '{}'",
                    self.original.display()
                )));
            }
        }
        Ok(())
    }

    fn bind_verified_directory_copy(&mut self, stage: &OwnedStage) -> io::Result<()> {
        if !self.identity.is_directory || !stage.identity.is_directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Directory move verification requires two directories",
            ));
        }
        let source_digest = sha256_directory_tree_snapshot(&self.path(), &self.identity)?;
        let staged_digest = sha256_directory_tree_snapshot(&stage.path(), &stage.identity)?;
        if source_digest != staged_digest {
            return Err(io::Error::other(
                "Source directory changed after it was copied; destination was not published",
            ));
        }
        self.verified_directory_digest = Some(source_digest);
        Ok(())
    }

    fn verify_stage_matches(&self, stage: &OwnedStage) -> io::Result<()> {
        stage.verify()?;
        if let Some(expected_digest) = self.verified_directory_digest {
            let current_digest = sha256_directory_tree_snapshot(&stage.path(), &stage.identity)?;
            if current_digest != expected_digest {
                return Err(io::Error::other(
                    "Verified directory staging changed before destination publication",
                ));
            }
        }
        Ok(())
    }

    fn restore(self, cause: io::Error) -> io::Error {
        let cause_retry_unsafe = is_retry_unsafe(&cause);
        let Self {
            original,
            directory,
            identity,
            verified_directory_digest: _,
        } = self;
        let payload = directory.payload();
        let current = match path_identity(&payload) {
            Ok(current) if current.same_snapshot(&identity) => current,
            Ok(_) => {
                return operation_error(
                    cause.kind(),
                    format!(
                        "{}; isolated source changed and was preserved at '{}'",
                        cause,
                        directory.path.display()
                    ),
                    true,
                )
            }
            Err(error) => {
                return operation_error(
                    cause.kind(),
                    format!(
                        "{}; isolated source could not be verified and was preserved at '{}': {}",
                        cause,
                        directory.path.display(),
                        error
                    ),
                    true,
                )
            }
        };
        drop(current);

        if let Err(error) = rename_noreplace(&payload, &original) {
            return operation_error(
                cause.kind(),
                format!(
                    "{}; source restore failed without clobbering '{}': {}. The source is preserved at '{}'",
                    cause,
                    original.display(),
                    error,
                    payload.display()
                ),
                true,
            );
        }
        let restored_verified = matches!(
            path_identity(&original),
            Ok(current) if current.matches_after_relocation(&identity)
        );
        drop(identity);
        let mut message = format!("{}; the source was restored", cause);
        if !restored_verified {
            message
                .push_str(" but its restored identity could not be rebound; inspect it manually");
        }
        if let Err(error) = sync_parent(&original) {
            message.push_str(&format!(
                "; restored-source durability could not be confirmed: {}",
                error
            ));
        }
        if let Err(error) = directory.cleanup() {
            message.push_str(&format!(
                "; source staging cleanup could not be confirmed: {}",
                error
            ));
        }
        operation_error(
            cause.kind(),
            message,
            cause_retry_unsafe || !restored_verified,
        )
    }

    fn finalize_after_commit(self) -> Vec<String> {
        let Self {
            original,
            directory,
            identity,
            verified_directory_digest,
        } = self;
        let payload = directory.payload();
        let current = match path_identity(&payload) {
            Ok(current) if current.same_snapshot(&identity) => current,
            Ok(_) => {
                return vec![format!(
                    "Destination was committed, but the isolated source changed and was preserved under '{}'",
                    directory.path.display()
                )]
            }
            Err(error) => {
                return vec![format!(
                    "Destination was committed, but the isolated source could not be verified and was preserved under '{}': {}",
                    directory.path.display(), error
                )]
            }
        };
        if let Some(expected_digest) = verified_directory_digest {
            match sha256_directory_tree_snapshot(&payload, &current) {
                Ok(current_digest) if current_digest == expected_digest => {}
                Ok(_) => {
                    return vec![format!(
                        "Destination was committed, but the isolated source tree changed and was preserved under '{}'",
                        directory.path.display()
                    )]
                }
                Err(error) => {
                    return vec![format!(
                        "Destination was committed, but the isolated source tree could not be reverified and was preserved under '{}': {}",
                        directory.path.display(), error
                    )]
                }
            }
        }
        drop(identity);

        let mut warnings = Vec::new();
        match delete_replacement_backup_if_unchanged(&payload, current) {
            Ok(VerifiedCleanupOutcome::Removed) => {}
            Ok(VerifiedCleanupOutcome::RemovedWithDurabilityWarning(error)) => {
                warnings.push(format!(
                    "Destination was committed and source data was removed, but deletion durability could not be confirmed for '{}': {}",
                    original.display(), error
                ));
            }
            Err(error) => {
                return vec![format!(
                    "Destination was committed, but verified source cleanup failed. Recovery data is preserved under '{}': {}",
                    directory.path.display(), error
                )];
            }
        }
        let staging_path = directory.path.clone();
        if let Err(error) = directory.cleanup() {
            warnings.push(format!(
                "Destination was committed, but source staging cleanup could not be confirmed under '{}': {}",
                staging_path.display(), error
            ));
        }
        warnings
    }
}

enum CrossFilesystemMoveFailure {
    NotCommitted(io::Error),
    CommittedUnverified(io::Error),
}

fn commit_cross_filesystem_move(
    stage: OwnedStage,
    destination: &Path,
    expected_destination: Option<PathIdentity>,
    source: QuarantinedSource,
    mut warnings: Vec<String>,
    target_authorization: Option<&DirectoryAuthorization>,
) -> Result<Vec<String>, CrossFilesystemMoveFailure> {
    if let Err(error) = source
        .verify_unchanged()
        .and_then(|()| source.verify_stage_matches(&stage))
    {
        let error = match stage.cleanup() {
            Ok(()) => error,
            Err(cleanup_error) => io::Error::new(
                error.kind(),
                format!(
                    "{}; destination staging cleanup also failed: {}",
                    error, cleanup_error
                ),
            ),
        };
        let error = source.restore(error);
        return Err(if is_retry_unsafe(&error) {
            CrossFilesystemMoveFailure::CommittedUnverified(error)
        } else {
            CrossFilesystemMoveFailure::NotCommitted(error)
        });
    }

    if let Some(authorization) = target_authorization {
        if let Err(error) = authorized_current_directory(
            destination.parent().unwrap_or_else(|| Path::new(".")),
            authorization,
            "Paste target directory",
        ) {
            let error = match stage.cleanup() {
                Ok(()) => error,
                Err(cleanup_error) => io::Error::new(
                    error.kind(),
                    format!(
                        "{}; destination staging cleanup also failed: {}",
                        error, cleanup_error
                    ),
                ),
            };
            let error = source.restore(error);
            return Err(if is_retry_unsafe(&error) {
                CrossFilesystemMoveFailure::CommittedUnverified(error)
            } else {
                CrossFilesystemMoveFailure::NotCommitted(error)
            });
        }
    }

    if let Some(expected_destination) = expected_destination {
        let stage_path = stage.path();
        let (install, terminal_failure) = install_completed_replacement_detailed(
            &stage_path,
            destination,
            expected_destination,
            &stage.identity,
        );
        match install {
            Ok(install_warnings) => {
                warnings.extend(install_warnings);
                if let Err(error) = stage.cleanup() {
                    warnings.push(format!(
                        "Move committed at '{}', but destination staging cleanup could not be confirmed: {}",
                        destination.display(), error
                    ));
                }
                warnings.extend(source.finalize_after_commit());
                Ok(warnings)
            }
            Err(error) => {
                // A missing staging payload means publication may already have
                // committed.  Never consume/retry the source in that state.
                let committed_unverified = terminal_failure
                    || !matches!(
                        path_identity(&stage_path),
                        Ok(current) if current.same_snapshot(&stage.identity)
                    );
                let error = match stage.cleanup() {
                    Ok(()) => error,
                    Err(cleanup_error) => io::Error::new(
                        error.kind(),
                        format!(
                            "{}; destination staging cleanup also failed: {}",
                            error, cleanup_error
                        ),
                    ),
                };
                let error = source.restore(error);
                if committed_unverified || is_retry_unsafe(&error) {
                    Err(CrossFilesystemMoveFailure::CommittedUnverified(error))
                } else {
                    Err(CrossFilesystemMoveFailure::NotCommitted(error))
                }
            }
        }
    } else {
        match stage.publish_noreplace(destination) {
            Ok(published) => {
                let PublishedStage {
                    identity,
                    warnings: publish_warnings,
                } = published;
                warnings.extend(publish_warnings);
                if let Some(identity) = identity {
                    drop(identity);
                    warnings.extend(source.finalize_after_commit());
                    Ok(warnings)
                } else {
                    let error = source.restore(io::Error::other(format!(
                        "Move publication committed at '{}', but the destination could not be rebound to the verified copy; inspect it manually",
                        destination.display()
                    )));
                    Err(CrossFilesystemMoveFailure::CommittedUnverified(error))
                }
            }
            Err(failure) => {
                let error = error_with_owned_stage_cleanup(failure);
                let error = source.restore(error);
                if is_retry_unsafe(&error) {
                    Err(CrossFilesystemMoveFailure::CommittedUnverified(error))
                } else {
                    Err(CrossFilesystemMoveFailure::NotCommitted(error))
                }
            }
        }
    }
}

#[derive(Debug)]
struct EntryMoveFailure {
    error: io::Error,
    terminal: bool,
}

fn move_entry_overwrite_same_filesystem(
    source: &Path,
    destination: &Path,
    expected: &PathIdentity,
    expected_destination: PathIdentity,
) -> Result<Vec<String>, EntryMoveFailure> {
    let terminal = std::cell::Cell::new(false);
    let result = move_entry_overwrite_same_filesystem_impl(
        source,
        destination,
        expected,
        |staging, destination, staged_identity| {
            let (result, is_terminal) = install_completed_replacement_detailed(
                staging,
                destination,
                expected_destination,
                staged_identity,
            );
            terminal.set(is_terminal);
            result
        },
    );
    result.map_err(|error| EntryMoveFailure {
        terminal: terminal.get() || is_retry_unsafe(&error),
        error,
    })
}

fn move_entry_overwrite_same_filesystem_impl<F>(
    source: &Path,
    destination: &Path,
    expected: &PathIdentity,
    install: F,
) -> io::Result<Vec<String>>
where
    F: FnOnce(&Path, &Path, &PathIdentity) -> io::Result<Vec<String>>,
{
    let target_parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let staging_directory = PrivateStagingDirectory::create(target_parent, "move-stage")?;
    let staging = staging_directory.payload();
    if let Err(error) = rename_noreplace(source, &staging) {
        let error = if error.kind() == io::ErrorKind::Unsupported {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Entry overwrite requires an atomic same-filesystem no-clobber rename for '{}': {}",
                    source.display(),
                    error
                ),
            )
        } else {
            error
        };
        return Err(error_with_staging_cleanup(error, staging_directory));
    }

    if let Err(error) = sync_parent(source).and_then(|()| sync_parent(&staging)) {
        let error =
            restore_staged_directory_after_failure(source, &staging, destination, expected, error);
        return Err(error_with_staging_cleanup(error, staging_directory));
    }

    let staged_identity = match path_identity(&staging) {
        Ok(identity) => identity,
        Err(error) => {
            let error = restore_staged_directory_after_failure(
                source,
                &staging,
                destination,
                expected,
                error,
            );
            return Err(error_with_staging_cleanup(error, staging_directory));
        }
    };
    if !staged_identity.matches_after_relocation(expected) {
        drop(staged_identity);
        let error = restore_staged_directory_after_failure(
            source,
            &staging,
            destination,
            expected,
            io::Error::other("Source changed while it was moved to replacement staging"),
        );
        return Err(error_with_staging_cleanup(error, staging_directory));
    }

    match install(&staging, destination, &staged_identity) {
        Ok(mut warnings) => {
            drop(staged_identity);
            if let Err(error) = staging_directory.cleanup() {
                warnings.push(format!(
                    "Move committed at '{}', but private staging cleanup could not be confirmed: {}",
                    destination.display(), error
                ));
            }
            Ok(warnings)
        }
        Err(error) => {
            drop(staged_identity);
            let error = restore_staged_directory_after_failure(
                source,
                &staging,
                destination,
                expected,
                error,
            );
            Err(error_with_staging_cleanup(error, staging_directory))
        }
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

fn create_new_destination_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub(crate) fn open_regular_file_no_follow(path: &Path) -> io::Result<(File, fs::Metadata)> {
    let mut options = OpenOptions::new();
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

    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    let is_reparse = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x0400 != 0
    };
    #[cfg(not(windows))]
    let is_reparse = false;
    if !metadata.is_file() || is_reparse {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Source is not a real regular file",
        ));
    }
    if stable_file_identity(&file)? != stable_path_identity(path)? {
        return Err(io::Error::other(
            "Source path changed while the regular file was being opened",
        ));
    }
    Ok((file, metadata))
}

#[cfg(unix)]
pub(crate) fn open_directory_for_read(
    path: &Path,
) -> io::Result<(File, DirectoryAccess, fs::Metadata)> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Source is not a directory",
        ));
    }
    let access = DirectoryAccess::new(&file, path)?;
    Ok((file, access, metadata))
}

#[cfg(windows)]
pub(crate) fn open_directory_for_read(
    path: &Path,
) -> io::Result<(File, DirectoryAccess, fs::Metadata)> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES)
        // Omitting FILE_SHARE_DELETE pins this name while the recursive walk
        // keeps the returned handle alive. This prevents a directory or
        // junction swap between validation and read_dir(path).
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let information = windows_handle_info(&file)?;
    if information.attributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || information.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Source is not a real directory",
        ));
    }
    let metadata = file.metadata()?;
    let access = DirectoryAccess::new(&file, path)?;
    Ok((file, access, metadata))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_directory_for_read(
    path: &Path,
) -> io::Result<(File, DirectoryAccess, fs::Metadata)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "Race-safe directory traversal is unavailable on this platform: '{}'",
            path.display()
        ),
    ))
}

fn preserve_file_metadata(file: &File, metadata: &fs::Metadata) -> io::Result<()> {
    #[cfg(unix)]
    file.set_permissions(metadata.permissions())?;

    let mtime = FileTime::from_last_modification_time(metadata);
    let atime = FileTime::from_last_access_time(metadata);
    filetime::set_file_handle_times(file, Some(atime), Some(mtime))
}

fn preserve_directory_metadata_no_follow(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let (directory, _, opened) = open_directory_for_read(path)?;
    let identity = stable_file_identity(&directory)?;
    if !opened.file_type().is_dir() || stable_path_identity(path)? != identity {
        return Err(io::Error::other(format!(
            "Destination directory changed before metadata preservation: '{}'",
            path.display()
        )));
    }
    #[cfg(unix)]
    preserve_file_metadata(&directory, metadata)?;
    #[cfg(windows)]
    {
        // The held directory handle denies delete sharing, so the pathname
        // cannot be swapped while filetime opens it with write-attributes.
        preserve_timestamps(path, metadata)?;
    }
    #[cfg(not(any(unix, windows)))]
    preserve_timestamps(path, metadata)?;
    // Child names must be durable before their top-level directory is
    // published. Every recursive directory reaches this point after its
    // descendants have been synced.
    #[cfg(unix)]
    directory.sync_all()?;
    if stable_path_identity(path)? != identity {
        return Err(io::Error::other(format!(
            "Destination directory changed during metadata preservation: '{}'",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn lexically_normalized_absolute(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::from("/");
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
            Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Unexpected path prefix on Unix",
                ))
            }
        }
    }
    Ok(normalized)
}

/// Resolve the deepest existing prefix and append any missing suffix without
/// following it. This catches an existing symlink component even when the
/// final logical destination tree or link target does not exist yet.
#[cfg(unix)]
fn resolve_existing_prefix(path: &Path) -> io::Result<PathBuf> {
    let mut prefix = path.to_path_buf();
    let mut missing_suffix = Vec::new();

    loop {
        match fs::symlink_metadata(&prefix) {
            Ok(_) => {
                let mut resolved = prefix.canonicalize().map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "Could not safely resolve existing symlink target prefix '{}': {}",
                            prefix.display(),
                            error
                        ),
                    )
                })?;
                for component in missing_suffix.iter().rev() {
                    resolved.push(component);
                }
                return lexically_normalized_absolute(&resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let component = prefix.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("Could not resolve a path prefix for '{}'", path.display()),
                    )
                })?;
                missing_suffix.push(component.to_os_string());
                if !prefix.pop() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("Could not resolve a path prefix for '{}'", path.display()),
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn copy_symlink_to_new(
    src: &Path,
    dest: &Path,
    logical_destination: &Path,
    expected_source: Option<&PathAuthorization>,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(src)?;
    if !metadata.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Source is no longer a symbolic link",
        ));
    }

    #[cfg(unix)]
    {
        let before = path_identity(src)?;
        if expected_source.is_some_and(|expected| !expected.matches_snapshot(&before)) {
            return Err(io::Error::other(format!(
                "Clipboard source symlink changed before it could be copied: '{}'",
                src.display()
            )));
        }
        let target = fs::read_link(src)?;
        let after = path_identity(src)?;
        if !after.same_snapshot(&before) {
            return Err(io::Error::other(format!(
                "Source symlink changed while it was being copied: '{}'",
                src.display()
            )));
        }

        let target_path = if target.is_absolute() {
            target.clone()
        } else {
            logical_destination
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&target)
        };
        let lexical = lexically_normalized_absolute(&target_path)?;
        let lexical_text = lexical.to_string_lossy();
        if target_is_sensitive(&lexical_text) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Symlink '{}' would point to sensitive system path: {}",
                    logical_destination.display(),
                    lexical_text
                ),
            ));
        }
        let resolved = resolve_existing_prefix(&lexical)?;
        let resolved = resolved.to_string_lossy();
        if target_is_sensitive(&resolved) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Symlink '{}' would resolve through an existing path component to sensitive system path: {}",
                    logical_destination.display(),
                    resolved
                ),
            ));
        }
        std::os::unix::fs::symlink(target, dest)
    }
    #[cfg(not(unix))]
    {
        let _ = dest;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Copying symbolic links is not supported safely on this platform",
        ))
    }
}

fn copy_open_symlink_to_new(
    access: &DirectoryAccess,
    name: &OsStr,
    source_display: &Path,
    dest: &Path,
    logical_destination: &Path,
) -> io::Result<()> {
    let before = access.child_metadata(name)?;
    if !before.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Source is no longer a symbolic link",
        ));
    }

    #[cfg(unix)]
    {
        let target = access.read_link(name)?;
        let after = access.child_metadata(name)?;
        if after.identity() != before.identity() || !after.is_symlink() {
            return Err(io::Error::other(format!(
                "Source symlink changed while it was being copied: '{}'",
                source_display.display()
            )));
        }

        let target_path = if target.is_absolute() {
            target.clone()
        } else {
            logical_destination
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&target)
        };
        let lexical = lexically_normalized_absolute(&target_path)?;
        let lexical_text = lexical.to_string_lossy();
        if target_is_sensitive(&lexical_text) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Symlink '{}' would point to sensitive system path: {}",
                    logical_destination.display(),
                    lexical_text
                ),
            ));
        }
        let resolved = resolve_existing_prefix(&lexical)?;
        let resolved = resolved.to_string_lossy();
        if target_is_sensitive(&resolved) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Symlink '{}' would resolve through an existing path component to sensitive system path: {}",
                    logical_destination.display(),
                    resolved
                ),
            ));
        }
        std::os::unix::fs::symlink(target, dest)
    }
    #[cfg(not(unix))]
    {
        let _ = (source_display, dest, logical_destination);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Copying symbolic links is not supported safely on this platform",
        ))
    }
}

/// Calculate total size of files to be copied/moved
pub fn calculate_total_size(
    files: &[PathBuf],
    cancel_flag: &Arc<AtomicBool>,
) -> io::Result<(u64, usize)> {
    let mut total_size: u64 = 0;
    let mut total_files: usize = 0;

    for path in files {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
        }

        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_symlink() {
            total_files += 1;
        } else if metadata.is_dir() {
            let (dir_size, dir_files) = calculate_dir_size(path, cancel_flag)?;
            total_size += dir_size;
            total_files += dir_files;
        } else if metadata.is_file() {
            total_size += metadata.len();
            total_files += 1;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Cannot copy special file: {}", path.display()),
            ));
        }
    }

    Ok((total_size, total_files))
}

/// Calculate total size and file count of a directory
fn calculate_dir_size(path: &Path, cancel_flag: &Arc<AtomicBool>) -> io::Result<(u64, usize)> {
    let (source_guard, access, _) = open_directory_for_read(path)?;
    drop(source_guard);
    calculate_open_dir_size(path, &access, cancel_flag)
}

fn calculate_open_dir_size(
    public_path: &Path,
    access: &DirectoryAccess,
    cancel_flag: &Arc<AtomicBool>,
) -> io::Result<(u64, usize)> {
    let mut total_size = 0u64;
    let mut total_files = 0usize;
    // Recurse only after the directory stream has been dropped. `entries()`
    // owns a second descriptor on Unix; retaining one at every active stack
    // frame halves the usable recursion depth and can exhaust macOS' process
    // descriptor limit. Only child-directory names and identities survive this
    // pass, which also detects replacement before deferred recursion.
    let mut child_directories = Vec::new();

    for name in access.entries()? {
        let name = name?;
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
        }

        let entry_path = public_path.join(&name);
        let metadata = access.child_metadata(&name)?;

        if metadata.is_symlink() {
            // Symlinks count as 0 size
            total_files += 1;
        } else if metadata.is_dir() {
            child_directories.push((name, metadata.identity()));
        } else if metadata.is_file() {
            total_size += metadata.len();
            total_files += 1;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Cannot copy special file: {}", entry_path.display()),
            ));
        }
    }

    for (name, expected_identity) in child_directories {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
        }
        let entry_path = public_path.join(&name);
        let (child_guard, child_access, _) = access.open_directory(&name)?;
        if stable_file_identity(&child_guard)? != expected_identity {
            return Err(io::Error::other(format!(
                "Source directory changed before traversal: '{}'",
                entry_path.display()
            )));
        }
        drop(child_guard);
        let (sub_size, sub_files) =
            calculate_open_dir_size(&entry_path, &child_access, cancel_flag)?;
        total_size += sub_size;
        total_files += sub_files;
    }

    Ok((total_size, total_files))
}

/// Copy a single regular file with progress callback.
///
/// The source is opened without following its final symlink component and the
/// destination is created exclusively, so a racing symlink cannot redirect or
/// truncate an unrelated file.
fn copy_regular_file_to_open_with_progress<F>(
    src: &Path,
    dest_file: &mut File,
    expected_source: Option<&PathAuthorization>,
    cancel_flag: &Arc<AtomicBool>,
    progress_callback: F,
) -> io::Result<(u64, [u8; 32])>
where
    F: FnMut(u64, u64),
{
    let (mut src_file, metadata) = open_regular_file_no_follow(src)?;
    if let Some(expected) = expected_source {
        let current = path_identity(src)?;
        if !expected.matches_snapshot(&current)
            || stable_file_identity(&src_file)? != current.stable
        {
            return Err(io::Error::other(format!(
                "Clipboard source changed before it could be copied: '{}'",
                src.display()
            )));
        }
    }
    copy_open_regular_file_to_open_with_progress(
        &mut src_file,
        &metadata,
        src,
        dest_file,
        cancel_flag,
        progress_callback,
    )
}

fn copy_open_regular_file_to_open_with_progress<F>(
    src_file: &mut File,
    metadata: &fs::Metadata,
    source_display: &Path,
    dest_file: &mut File,
    cancel_flag: &Arc<AtomicBool>,
    mut progress_callback: F,
) -> io::Result<(u64, [u8; 32])>
where
    F: FnMut(u64, u64),
{
    let total_size = metadata.len();

    // Check for cancellation before starting
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
    }

    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    let mut copied: u64 = 0;
    let mut source_hasher = Sha256::new();

    loop {
        // Check for cancellation
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
        }

        let bytes_read = src_file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        dest_file.write_all(&buffer[..bytes_read])?;
        source_hasher.update(&buffer[..bytes_read]);
        copied += bytes_read as u64;

        // Report progress
        progress_callback(copied, total_size);
    }

    // A regular file can be modified through the same inode while it is
    // being read. Publishing a mixed snapshot (and, for move, deleting the
    // newer source) would be data loss, so require its identity and content
    // metadata to remain stable for the whole read.
    if !metadata_still_matches(metadata, &src_file.metadata()?) {
        return Err(io::Error::other(format!(
            "Source changed while it was being copied: {}",
            source_display.display()
        )));
    }

    preserve_file_metadata(dest_file, metadata)?;
    dest_file.sync_all()?;

    Ok((copied, source_hasher.finalize().into()))
}

/// Hash a regular file through a no-follow handle while proving that both the
/// handle and pathname remain the exact isolated move source. This binds the
/// post-copy source bytes to the digest observed during the copy, including a
/// same-inode write that restores size and mtime around the quarantine rename.
fn sha256_regular_file_snapshot(path: &Path, expected: &PathIdentity) -> io::Result<[u8; 32]> {
    let (mut file, before) = open_regular_file_no_follow(path)?;
    if stable_file_identity(&file)? != expected.stable
        || !matches!(path_identity(path), Ok(current) if current.same_snapshot(expected))
    {
        return Err(io::Error::other(
            "Isolated move source changed before content verification",
        ));
    }

    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let after = file.metadata()?;
    if !metadata_still_matches(&before, &after)
        || stable_file_identity(&file)? != expected.stable
        || !matches!(path_identity(path), Ok(current) if current.same_snapshot(expected))
    {
        return Err(io::Error::other(
            "Isolated move source changed during content verification",
        ));
    }
    Ok(hasher.finalize().into())
}

fn verify_isolated_source_matches_copy(
    source: &QuarantinedSource,
    copied_source_sha256: [u8; 32],
) -> io::Result<()> {
    let current_sha256 = sha256_regular_file_snapshot(&source.path(), &source.identity)?;
    if current_sha256 != copied_source_sha256 {
        return Err(io::Error::other(
            "Source bytes changed after they were copied; destination was not published",
        ));
    }
    Ok(())
}

fn update_tree_digest_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn update_tree_digest_name(hasher: &mut Sha256, name: &OsStr) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        update_tree_digest_bytes(hasher, name.as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let bytes = name
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        update_tree_digest_bytes(hasher, &bytes);
    }
    #[cfg(not(any(unix, windows)))]
    update_tree_digest_bytes(hasher, name.to_string_lossy().as_bytes());
}

fn tree_digest_permission_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

fn hash_open_directory_tree(
    access: &DirectoryAccess,
    display_path: &Path,
    hasher: &mut Sha256,
) -> io::Result<()> {
    let directory_before = access.file().metadata()?;
    hasher.update(b"directory\0");
    hasher.update(tree_digest_permission_mode(&directory_before).to_le_bytes());

    let mut names = access.collect_entry_names()?;
    names.sort();
    for name in &names {
        let child_path = display_path.join(name);
        let before = access.child_metadata(name)?;
        update_tree_digest_name(hasher, name);

        if before.is_file() {
            hasher.update(b"file\0");
            hasher.update(before.len().to_le_bytes());
            hasher.update((before.mode() & 0o7777).to_le_bytes());
            let (mut file, opened_metadata) = access.open_regular_file(name)?;
            if stable_file_identity(&file)? != before.identity() {
                return Err(io::Error::other(format!(
                    "Directory tree entry changed before hashing: '{}'",
                    child_path.display()
                )));
            }
            let mut content_hasher = Sha256::new();
            let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                content_hasher.update(&buffer[..read]);
            }
            let after = file.metadata()?;
            let current = access.child_metadata(name)?;
            if !metadata_still_matches(&opened_metadata, &after)
                || stable_file_identity(&file)? != before.identity()
                || current.identity() != before.identity()
                || !current.is_file()
                || current.len() != before.len()
                || (current.mode() & 0o7777) != (before.mode() & 0o7777)
            {
                return Err(io::Error::other(format!(
                    "Directory tree file changed while hashing: '{}'",
                    child_path.display()
                )));
            }
            hasher.update(content_hasher.finalize());
        } else if before.is_dir() {
            hasher.update(b"directory-entry\0");
            hasher.update((before.mode() & 0o7777).to_le_bytes());
            let (child_file, child_access, _) = access.open_directory(name)?;
            if stable_file_identity(&child_file)? != before.identity() {
                return Err(io::Error::other(format!(
                    "Directory tree directory changed before hashing: '{}'",
                    child_path.display()
                )));
            }
            drop(child_file);
            hash_open_directory_tree(&child_access, &child_path, hasher)?;
            let current = access.child_metadata(name)?;
            if current.identity() != before.identity()
                || !current.is_dir()
                || (current.mode() & 0o7777) != (before.mode() & 0o7777)
            {
                return Err(io::Error::other(format!(
                    "Directory tree directory changed while hashing: '{}'",
                    child_path.display()
                )));
            }
        } else if before.is_symlink() {
            hasher.update(b"symlink\0");
            #[cfg(unix)]
            {
                let target = access.read_link(name)?;
                update_tree_digest_name(hasher, target.as_os_str());
                let current = access.child_metadata(name)?;
                if current.identity() != before.identity() || !current.is_symlink() {
                    return Err(io::Error::other(format!(
                        "Directory tree symlink changed while hashing: '{}'",
                        child_path.display()
                    )));
                }
            }
            #[cfg(not(unix))]
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "Safe symbolic-link verification is unavailable for '{}'",
                        child_path.display()
                    ),
                ));
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Cannot verify special file in directory move: '{}'",
                    child_path.display()
                ),
            ));
        }
    }

    let mut names_after = access.collect_entry_names()?;
    names_after.sort();
    let directory_after = access.file().metadata()?;
    if names_after != names || !metadata_still_matches(&directory_before, &directory_after) {
        return Err(io::Error::other(format!(
            "Directory tree changed while hashing: '{}'",
            display_path.display()
        )));
    }
    Ok(())
}

fn sha256_directory_tree_snapshot(path: &Path, expected: &PathIdentity) -> io::Result<[u8; 32]> {
    if !expected.is_directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Directory tree verification requires a directory",
        ));
    }
    let (directory, access, _) = open_directory_for_read(path)?;
    if stable_file_identity(&directory)? != expected.stable
        || !matches!(path_identity(path), Ok(current) if current.same_snapshot(expected))
    {
        return Err(io::Error::other(format!(
            "Directory tree changed before verification: '{}'",
            path.display()
        )));
    }
    drop(directory);

    let mut hasher = Sha256::new();
    hasher.update(b"cokacdir-directory-tree-v1\0");
    hash_open_directory_tree(&access, path, &mut hasher)?;
    if stable_file_identity(access.file())? != expected.stable
        || !matches!(path_identity(path), Ok(current) if current.same_snapshot(expected))
    {
        return Err(io::Error::other(format!(
            "Directory tree changed during verification: '{}'",
            path.display()
        )));
    }
    Ok(hasher.finalize().into())
}

/// Copy directly into an exclusively-created entry of a private tree.  The
/// caller owns the tree-level staging transaction, so nested publication
/// staging would only create recovery artifacts inside the payload.
fn copy_regular_file_into_private_tree<F>(
    src: &Path,
    dest: &Path,
    cancel_flag: &Arc<AtomicBool>,
    progress_callback: F,
) -> io::Result<u64>
where
    F: FnMut(u64, u64),
{
    let mut destination = create_new_destination_file(dest)?;
    let expected = stable_file_identity(&destination)?;
    if stable_path_identity(dest)? != expected {
        return Err(io::Error::other(format!(
            "Private-tree destination changed immediately after creation: '{}'",
            dest.display()
        )));
    }
    let (copied, _) = copy_regular_file_to_open_with_progress(
        src,
        &mut destination,
        None,
        cancel_flag,
        progress_callback,
    )?;
    if stable_path_identity(dest)? != expected {
        return Err(io::Error::other(format!(
            "Private-tree destination changed while it was being copied: '{}'",
            dest.display()
        )));
    }
    Ok(copied)
}

fn copy_open_regular_file_into_private_tree<F>(
    src_file: &mut File,
    source_metadata: &fs::Metadata,
    source_display: &Path,
    dest: &Path,
    cancel_flag: &Arc<AtomicBool>,
    progress_callback: F,
) -> io::Result<u64>
where
    F: FnMut(u64, u64),
{
    let mut destination = create_new_destination_file(dest)?;
    let expected = stable_file_identity(&destination)?;
    if stable_path_identity(dest)? != expected {
        return Err(io::Error::other(format!(
            "Private-tree destination changed immediately after creation: '{}'",
            dest.display()
        )));
    }
    let (copied, _) = copy_open_regular_file_to_open_with_progress(
        src_file,
        source_metadata,
        source_display,
        &mut destination,
        cancel_flag,
        progress_callback,
    )?;
    if stable_path_identity(dest)? != expected {
        return Err(io::Error::other(format!(
            "Private-tree destination changed while it was being copied: '{}'",
            dest.display()
        )));
    }
    Ok(copied)
}

pub fn copy_file_with_progress<F>(
    src: &Path,
    dest: &Path,
    cancel_flag: &Arc<AtomicBool>,
    progress_callback: F,
) -> io::Result<u64>
where
    F: FnMut(u64, u64),
{
    copy_file_with_progress_detailed(src, dest, None, cancel_flag, progress_callback)
        .map(|(copied, _published, _source_sha256)| copied)
}

pub(crate) fn copy_file_with_progress_authorized<F>(
    src: &Path,
    dest: &Path,
    expected_source: &PathAuthorization,
    cancel_flag: &Arc<AtomicBool>,
    progress_callback: F,
) -> io::Result<(u64, Vec<String>)>
where
    F: FnMut(u64, u64),
{
    copy_file_with_progress_detailed(
        src,
        dest,
        Some(expected_source),
        cancel_flag,
        progress_callback,
    )
    .map(|(copied, published, _source_sha256)| (copied, published.warnings))
}

fn copy_file_with_progress_detailed<F>(
    src: &Path,
    dest: &Path,
    expected_source: Option<&PathAuthorization>,
    cancel_flag: &Arc<AtomicBool>,
    mut progress_callback: F,
) -> io::Result<(u64, PublishedStage, [u8; 32])>
where
    F: FnMut(u64, u64),
{
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let staging = PrivateStagingDirectory::create(parent, "copy-file")?;
    let temp = staging.payload();
    let mut destination = match create_new_destination_file(&temp) {
        Ok(file) => file,
        Err(error) => return Err(error_with_staging_cleanup(error, staging)),
    };
    let partial = match PartialStage::bind_file(staging, &destination) {
        Ok(stage) => stage,
        Err(error) => return Err(error),
    };

    let result = copy_regular_file_to_open_with_progress(
        src,
        &mut destination,
        expected_source,
        cancel_flag,
        |copied, total| progress_callback(copied, total),
    );
    drop(destination);
    match result {
        Ok((copied, source_sha256)) => {
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(error_with_partial_stage_cleanup(
                    io::Error::new(io::ErrorKind::Interrupted, "Cancelled"),
                    partial,
                ));
            }
            let stage = partial.seal()?;
            match stage.publish_noreplace(dest) {
                Ok(published) if published.identity.is_some() => {
                    Ok((copied, published, source_sha256))
                }
                Ok(_) => Err(io::Error::other(format!(
                    "File publication at '{}' committed, but the destination no longer identifies the staged file; inspect it manually",
                    dest.display()
                ))),
                Err(failure) => Err(error_with_owned_stage_cleanup(failure)),
            }
        }
        Err(error) => Err(error_with_partial_stage_cleanup(error, partial)),
    }
}

/// Copy directory recursively with progress reporting
pub fn copy_dir_recursive_with_progress(
    src: &Path,
    dest: &Path,
    cancel_flag: &Arc<AtomicBool>,
    progress_tx: &Sender<ProgressMessage>,
    completed_bytes: &mut u64,
    completed_files: &mut usize,
    total_bytes: u64,
    total_files: usize,
) -> io::Result<()> {
    copy_dir_recursive_with_progress_detailed(
        src,
        dest,
        dest,
        None,
        cancel_flag,
        progress_tx,
        completed_bytes,
        completed_files,
        total_bytes,
        total_files,
    )
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn copy_dir_recursive_with_progress_authorized(
    src: &Path,
    dest: &Path,
    expected_source: &PathAuthorization,
    cancel_flag: &Arc<AtomicBool>,
    progress_tx: &Sender<ProgressMessage>,
    completed_bytes: &mut u64,
    completed_files: &mut usize,
    total_bytes: u64,
    total_files: usize,
) -> io::Result<Vec<String>> {
    copy_dir_recursive_with_progress_detailed(
        src,
        dest,
        dest,
        Some(expected_source),
        cancel_flag,
        progress_tx,
        completed_bytes,
        completed_files,
        total_bytes,
        total_files,
    )
    .map(|published| published.warnings)
}

/// Materialize one clipboard-authorized source into a new destination. This is
/// used by boundary transfers to create a coherent local snapshot before an
/// external process such as rsync is allowed to read it.
pub(crate) fn copy_path_authorized(
    src: &Path,
    dest: &Path,
    expected_source: &PathAuthorization,
    cancel_flag: &Arc<AtomicBool>,
    progress_tx: &Sender<ProgressMessage>,
) -> io::Result<Vec<String>> {
    let metadata = fs::symlink_metadata(src)?;
    if metadata.is_symlink() {
        copy_symlink_authorized(src, dest, expected_source)
    } else if metadata.is_dir() {
        let (total_bytes, total_files) = calculate_total_size(&[src.to_path_buf()], cancel_flag)?;
        let mut completed_bytes = 0;
        let mut completed_files = 0;
        copy_dir_recursive_with_progress_authorized(
            src,
            dest,
            expected_source,
            cancel_flag,
            progress_tx,
            &mut completed_bytes,
            &mut completed_files,
            total_bytes,
            total_files,
        )
    } else if metadata.is_file() {
        copy_file_with_progress_authorized(
            src,
            dest,
            expected_source,
            cancel_flag,
            |copied, total| {
                let _ = progress_tx.send(ProgressMessage::FileProgress(copied, total));
            },
        )
        .map(|(_, warnings)| warnings)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Cannot snapshot special file: {}", src.display()),
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_dir_recursive_with_progress_detailed(
    src: &Path,
    dest: &Path,
    logical_destination: &Path,
    expected_source: Option<&PathAuthorization>,
    cancel_flag: &Arc<AtomicBool>,
    progress_tx: &Sender<ProgressMessage>,
    completed_bytes: &mut u64,
    completed_files: &mut usize,
    total_bytes: u64,
    total_files: usize,
) -> io::Result<PublishedStage> {
    let metadata = fs::symlink_metadata(src)?;
    if metadata.is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Source is not a directory",
        ));
    }

    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let staging = PrivateStagingDirectory::create(parent, "copy-directory")?;
    let temp = staging.payload();
    if let Err(error) = create_private_directory(&temp) {
        return Err(error_with_staging_cleanup(error, staging));
    }
    let partial = PartialStage::bind_directory(staging)?;
    let mut visited = HashSet::new();
    let result = copy_dir_recursive_with_progress_inner(
        src,
        &temp,
        logical_destination,
        expected_source,
        cancel_flag,
        progress_tx,
        completed_bytes,
        completed_files,
        total_bytes,
        total_files,
        &mut visited,
        0,
        true,
        None,
    );
    match result {
        Ok(()) => {
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(error_with_partial_stage_cleanup(
                    io::Error::new(io::ErrorKind::Interrupted, "Cancelled"),
                    partial,
                ));
            }
            let stage = partial.seal()?;
            match stage.publish_noreplace(dest) {
                Ok(published) if published.identity.is_some() => Ok(published),
                Ok(_) => Err(io::Error::other(format!(
                    "Directory publication at '{}' committed, but the destination no longer identifies the staged directory; inspect it manually",
                    dest.display()
                ))),
                Err(failure) => Err(error_with_owned_stage_cleanup(failure)),
            }
        }
        Err(error) => Err(error_with_partial_stage_cleanup(error, partial)),
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_dir_recursive_with_progress_inner(
    src: &Path,
    dest: &Path,
    logical_destination: &Path,
    expected_source: Option<&PathAuthorization>,
    cancel_flag: &Arc<AtomicBool>,
    progress_tx: &Sender<ProgressMessage>,
    completed_bytes: &mut u64,
    completed_files: &mut usize,
    total_bytes: u64,
    total_files: usize,
    visited: &mut HashSet<StablePathIdentity>,
    depth: usize,
    destination_already_created: bool,
    opened_source: Option<(File, DirectoryAccess, fs::Metadata)>,
) -> io::Result<()> {
    // Guard against pathological recursion (e.g. circular symlinks).
    if depth > MAX_COPY_DEPTH {
        return Err(io::Error::other(format!(
            "Maximum directory depth ({}) exceeded - possible circular symlink",
            MAX_COPY_DEPTH
        )));
    }

    // Hold an O_NOFOLLOW directory descriptor for the entire traversal on
    // Unix. Entries are addressed through that descriptor, so replacing a
    // source directory path with a symlink cannot redirect recursion.
    let (source_guard, source_access, src_metadata) = match opened_source {
        Some(opened) => opened,
        None => open_directory_for_read(src)?,
    };
    let source_identity = stable_file_identity(&source_guard)?;
    if let Some(expected) = expected_source {
        let current = path_identity(src)?;
        if !expected.matches_snapshot(&current)
            || stable_file_identity(&source_guard)? != current.stable
        {
            return Err(io::Error::other(format!(
                "Clipboard source directory changed before it could be copied: '{}'",
                src.display()
            )));
        }
    }
    drop(source_guard);

    // Detect cycles by opened filesystem identity. `visited` tracks only the
    // current DFS stack, not every directory ever seen, so sibling aliases do
    // not produce false positives. Every exit removes the current identity.
    if visited.contains(&source_identity) {
        return Err(io::Error::other(format!(
            "Circular symlink detected: {}",
            src.display()
        )));
    }
    visited.insert(source_identity);

    // Run the body inside an IIFE so any early return (`?`, explicit
    // `return Err`) still goes through the `visited.remove` cleanup
    // below — that's what enforces stack-only semantics.
    let result: io::Result<()> = (|| {
        // Check for cancellation
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
        }

        if !destination_already_created {
            create_private_directory(dest)?;
        }

        // Do not recurse while the Unix directory stream is alive: the stream
        // owns an additional descriptor. Deferring only directory names keeps
        // file-heavy directories streaming while avoiding two retained FDs per
        // recursion level. Captured identities reject replacements before the
        // deferred child is opened.
        let mut child_directories = Vec::new();
        for name in source_access.entries()? {
            let name = name?;
            let src_path = src.join(&name);
            let dest_path = dest.join(&name);
            let logical_dest_path = logical_destination.join(&name);

            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
            }

            let metadata = source_access.child_metadata(&name)?;

            if metadata.is_symlink() {
                // Copy the link itself; never dereference it on platforms where
                // that cannot be done safely and unambiguously.
                copy_open_symlink_to_new(
                    &source_access,
                    &name,
                    &src_path,
                    &dest_path,
                    &logical_dest_path,
                )?;

                *completed_files += 1;
                let _ = progress_tx.send(ProgressMessage::TotalProgress(
                    *completed_files,
                    total_files,
                    *completed_bytes,
                    total_bytes,
                ));
            } else if metadata.is_dir() {
                child_directories.push((name, metadata.identity()));
            } else if metadata.is_file() {
                // Regular file - copy with progress
                let filename = src_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();

                let _ = progress_tx.send(ProgressMessage::FileStarted(filename.clone()));

                let (mut source_file, source_metadata) = source_access.open_regular_file(&name)?;
                let file_size = source_metadata.len();
                let file_completed_bytes = *completed_bytes;

                let result = copy_open_regular_file_into_private_tree(
                    &mut source_file,
                    &source_metadata,
                    &src_path,
                    &dest_path,
                    cancel_flag,
                    |copied, total| {
                        let _ = progress_tx.send(ProgressMessage::FileProgress(copied, total));
                        let _ = progress_tx.send(ProgressMessage::TotalProgress(
                            *completed_files,
                            total_files,
                            file_completed_bytes + copied,
                            total_bytes,
                        ));
                    },
                );

                match result {
                    Ok(_) => {
                        *completed_bytes += file_size;
                        *completed_files += 1;
                        let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
                    }
                    Err(e) => {
                        if e.kind() == io::ErrorKind::Interrupted {
                            return Err(e);
                        }
                        let error_kind = e.kind();
                        let error_message = format!("{}: {}", src_path.display(), e);
                        let _ = progress_tx
                            .send(ProgressMessage::Error(filename, error_message.clone()));
                        return Err(io::Error::new(error_kind, error_message));
                    }
                }
            } else {
                let filename = src_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();
                let message = format!("Cannot copy special file: {}", src_path.display());
                let _ = progress_tx.send(ProgressMessage::Error(filename, message.clone()));
                return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
            }
        }

        for (name, expected_identity) in child_directories {
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
            }
            let src_path = src.join(&name);
            let dest_path = dest.join(&name);
            let logical_dest_path = logical_destination.join(&name);
            let opened_child = source_access.open_directory(&name)?;
            if stable_file_identity(&opened_child.0)? != expected_identity {
                return Err(io::Error::other(format!(
                    "Source directory changed before it could be copied: '{}'",
                    src_path.display()
                )));
            }
            copy_dir_recursive_with_progress_inner(
                &src_path,
                &dest_path,
                &logical_dest_path,
                None,
                cancel_flag,
                progress_tx,
                completed_bytes,
                completed_files,
                total_bytes,
                total_files,
                visited,
                depth + 1,
                false,
                Some(opened_child),
            )?;
        }

        // Preserve metadata through the exact no-follow directory handle;
        // path-based chmod/utimes could be redirected after recursive copy.
        preserve_directory_metadata_no_follow(dest, &src_metadata)?;
        sync_parent(dest)?;
        if !metadata_still_matches(&src_metadata, &source_access.file().metadata()?) {
            return Err(io::Error::other(format!(
                "Source directory changed while it was being copied: '{}'",
                src.display()
            )));
        }

        Ok(())
    })();

    visited.remove(&source_identity);
    result
}

/// Copy files with progress reporting (main entry point for progress-enabled copy)
/// files_to_overwrite: Set of source paths that should overwrite existing destinations
/// files_to_skip: Set of source paths that should be skipped if destination exists
pub fn copy_files_with_progress(
    files: Vec<PathBuf>,
    source_dir: &Path,
    target_dir: &Path,
    mut files_to_overwrite: HashMap<PathBuf, PathAuthorization>,
    files_to_skip: HashSet<PathBuf>,
    target_authorization: Option<DirectoryAuthorization>,
    mut source_authorizations: HashMap<PathBuf, PathAuthorization>,
    source_directory_authorization: Option<DirectoryAuthorization>,
    cancel_flag: Arc<AtomicBool>,
    progress_tx: Sender<ProgressMessage>,
) {
    let mut success_count = 0;
    let mut failure_count = 0;
    let mut cancelled = false;
    let authorized_target_path = target_authorization
        .as_ref()
        .map(|authorization| authorization.resolved.clone());
    let authorized_source_path = source_directory_authorization
        .as_ref()
        .map(|authorization| authorization.resolved.clone());
    let target_dir = authorized_target_path.as_deref().unwrap_or(target_dir);
    let source_dir = authorized_source_path.as_deref().unwrap_or(source_dir);

    if let Some(authorization) = target_authorization.as_ref() {
        if let Err(error) =
            authorized_current_directory(target_dir, authorization, "Paste target directory")
        {
            send_prepare_error_result(&progress_tx, error, files.len());
            return;
        }
    }
    if let Some(authorization) = source_directory_authorization.as_ref() {
        if let Err(error) =
            authorized_current_directory(source_dir, authorization, "Clipboard source directory")
        {
            send_prepare_error_result(&progress_tx, error, files.len());
            return;
        }
    }

    // Build full paths for size calculation (excluding skipped files)
    let full_paths: Vec<PathBuf> = files
        .iter()
        .map(|f| {
            if f.is_absolute() {
                f.clone()
            } else {
                source_dir.join(f)
            }
        })
        .filter(|p| !files_to_skip.contains(p))
        .collect();

    // Send preparing message before calculating sizes
    let _ = progress_tx.send(ProgressMessage::Preparing(
        "Calculating file sizes...".to_string(),
    ));

    // Calculate total size
    let (total_bytes, total_files) = match calculate_total_size(&full_paths, &cancel_flag) {
        Ok((size, count)) => (size, count),
        Err(e) => {
            send_prepare_error_result(&progress_tx, e, files.len());
            return;
        }
    };

    // Send prepare complete
    let _ = progress_tx.send(ProgressMessage::PrepareComplete);

    let mut completed_bytes: u64 = 0;
    let mut completed_files: usize = 0;

    for file_path in &files {
        if cancel_flag.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }

        let src = if file_path.is_absolute() {
            file_path.clone()
        } else {
            source_dir.join(file_path)
        };
        if files_to_skip.contains(&src) {
            continue;
        }

        if let Some(authorization) = target_authorization.as_ref() {
            if let Err(error) =
                authorized_current_directory(target_dir, authorization, "Paste target directory")
            {
                failure_count += 1;
                let name = src
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let _ = progress_tx.send(ProgressMessage::Error(name, error.to_string()));
                break;
            }
        }
        if let Some(authorization) = source_directory_authorization.as_ref() {
            if let Err(error) = authorized_current_directory(
                source_dir,
                authorization,
                "Clipboard source directory",
            ) {
                failure_count += 1;
                let name = src
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let _ = progress_tx.send(ProgressMessage::Error(name, error.to_string()));
                break;
            }
        }

        let source_authorization = match source_authorizations.remove(&src) {
            Some(authorization) => Some(authorization),
            None if source_directory_authorization.is_some() => {
                failure_count += 1;
                let name = src
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let _ = progress_tx.send(ProgressMessage::Error(
                    name,
                    "Missing clipboard authorization for local source item".to_string(),
                ));
                continue;
            }
            None => None,
        };
        if let Some(authorization) = source_authorization.as_ref() {
            if let Err(error) =
                authorized_current_identity(&src, authorization, "Clipboard source item")
            {
                failure_count += 1;
                let name = src
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let _ = progress_tx.send(ProgressMessage::Error(name, error.to_string()));
                continue;
            }
        }

        let filename = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let dest = target_dir.join(&filename);

        if let Err(e) = validate_destination_not_self(&src, &dest, "copy") {
            failure_count += 1;
            let _ = progress_tx.send(ProgressMessage::Error(filename, e.to_string()));
            continue;
        }

        let dest_exists = path_exists_no_follow(&dest);
        let overwrite_authorization = files_to_overwrite.remove(&src);
        let overwriting = overwrite_authorization.is_some();
        if overwriting && !dest_exists {
            failure_count += 1;
            let _ = progress_tx.send(ProgressMessage::Error(
                filename,
                "Destination changed after overwrite confirmation; retry the operation".to_string(),
            ));
            continue;
        }
        if dest_exists && !overwriting {
            // Not in overwrite set and not in skip set - unexpected conflict
            failure_count += 1;
            let _ = progress_tx.send(ProgressMessage::Error(
                filename,
                "Target already exists".to_string(),
            ));
            continue;
        }
        let destination_identity = if overwriting {
            match authorized_current_identity(
                &dest,
                overwrite_authorization
                    .as_ref()
                    .expect("overwrite authorization was captured"),
                "Overwrite destination",
            ) {
                Ok(identity) => Some(identity),
                Err(error) => {
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                    continue;
                }
            }
        } else {
            None
        };
        let _ = progress_tx.send(ProgressMessage::FileStarted(filename.clone()));
        // Every authorized copy is first completed under an owned staging
        // name. This gives us a final target-directory revalidation point for
        // both overwrite and no-clobber publication.
        let mut overwrite_staging = match PrivateStagingDirectory::create(
            target_dir,
            if overwriting {
                "copy-overwrite"
            } else {
                "copy-publish"
            },
        ) {
            Ok(staging) => Some(staging),
            Err(e) => {
                failure_count += 1;
                let _ = progress_tx.send(ProgressMessage::Error(filename, e.to_string()));
                continue;
            }
        };
        if let Some(authorization) = target_authorization.as_ref() {
            if let Err(error) =
                authorized_current_directory(target_dir, authorization, "Paste target directory")
            {
                let error = match overwrite_staging.take() {
                    Some(staging) => error_with_staging_cleanup(error, staging),
                    None => error,
                };
                failure_count += 1;
                let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                continue;
            }
        }
        let copy_dest = if let Some(staging) = overwrite_staging.as_ref() {
            staging.payload()
        } else {
            dest.clone()
        };

        let source_metadata = match fs::symlink_metadata(&src) {
            Ok(metadata) => metadata,
            Err(error) => {
                let error = match overwrite_staging.take() {
                    Some(staging) => error_with_staging_cleanup(error, staging),
                    None => error,
                };
                failure_count += 1;
                let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                continue;
            }
        };

        if source_metadata.is_dir() && !source_metadata.is_symlink() {
            match copy_dir_recursive_with_progress_detailed(
                &src,
                &copy_dest,
                &dest,
                source_authorization.as_ref(),
                &cancel_flag,
                &progress_tx,
                &mut completed_bytes,
                &mut completed_files,
                total_bytes,
                total_files,
            ) {
                Ok(warnings) => match finish_copied_item(
                    overwrite_staging.take(),
                    &dest,
                    destination_identity,
                    warnings,
                    &cancel_flag,
                    target_authorization.as_ref(),
                ) {
                    Ok(warnings) => {
                        send_operation_warnings(&progress_tx, &filename, warnings);
                        success_count += 1;
                        let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
                    }
                    Err(error) => {
                        failure_count += 1;
                        let message = if is_retry_unsafe(&error) {
                            ProgressMessage::TerminalError(filename, error.to_string())
                        } else {
                            ProgressMessage::Error(filename, error.to_string())
                        };
                        let _ = progress_tx.send(message);
                    }
                },
                Err(error) => {
                    let error = match overwrite_staging.take() {
                        Some(staging) => error_with_staging_cleanup(error, staging),
                        None => error,
                    };
                    if error.kind() == io::ErrorKind::Interrupted {
                        if error.to_string() != "Cancelled" {
                            let _ = progress_tx.send(ProgressMessage::Warning(
                                filename.clone(),
                                error.to_string(),
                            ));
                        }
                        cancelled = true;
                        break;
                    }
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                }
            }
        } else if source_metadata.is_symlink() {
            match copy_symlink_detailed(&src, &copy_dest, &dest, source_authorization.as_ref()) {
                Ok(warnings) => match finish_copied_item(
                    overwrite_staging.take(),
                    &dest,
                    destination_identity,
                    warnings,
                    &cancel_flag,
                    target_authorization.as_ref(),
                ) {
                    Ok(warnings) => {
                        send_operation_warnings(&progress_tx, &filename, warnings);
                        completed_files += 1;
                        success_count += 1;
                        let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
                    }
                    Err(error) => {
                        failure_count += 1;
                        let message = if is_retry_unsafe(&error) {
                            ProgressMessage::TerminalError(filename, error.to_string())
                        } else {
                            ProgressMessage::Error(filename, error.to_string())
                        };
                        let _ = progress_tx.send(message);
                    }
                },
                Err(error) => {
                    let error = match overwrite_staging.take() {
                        Some(staging) => error_with_staging_cleanup(error, staging),
                        None => error,
                    };
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                }
            }
        } else if source_metadata.is_file() {
            let file_size = source_metadata.len();
            let file_completed_bytes = completed_bytes;

            match copy_file_with_progress_detailed(
                &src,
                &copy_dest,
                source_authorization.as_ref(),
                &cancel_flag,
                |copied, total| {
                    let _ = progress_tx.send(ProgressMessage::FileProgress(copied, total));
                    let _ = progress_tx.send(ProgressMessage::TotalProgress(
                        completed_files,
                        total_files,
                        file_completed_bytes + copied,
                        total_bytes,
                    ));
                },
            ) {
                Ok((_, warnings, _source_sha256)) => match finish_copied_item(
                    overwrite_staging.take(),
                    &dest,
                    destination_identity,
                    warnings,
                    &cancel_flag,
                    target_authorization.as_ref(),
                ) {
                    Ok(warnings) => {
                        send_operation_warnings(&progress_tx, &filename, warnings);
                        completed_bytes += file_size;
                        completed_files += 1;
                        success_count += 1;
                        let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
                    }
                    Err(error) => {
                        failure_count += 1;
                        let message = if is_retry_unsafe(&error) {
                            ProgressMessage::TerminalError(filename, error.to_string())
                        } else {
                            ProgressMessage::Error(filename, error.to_string())
                        };
                        let _ = progress_tx.send(message);
                    }
                },
                Err(error) => {
                    let error = match overwrite_staging.take() {
                        Some(staging) => error_with_staging_cleanup(error, staging),
                        None => error,
                    };
                    if error.kind() == io::ErrorKind::Interrupted {
                        if error.to_string() != "Cancelled" {
                            let _ = progress_tx.send(ProgressMessage::Warning(
                                filename.clone(),
                                error.to_string(),
                            ));
                        }
                        cancelled = true;
                        break;
                    }
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                }
            }
        } else {
            if let Some(staging) = overwrite_staging.take() {
                if let Err(cleanup_error) = staging.cleanup() {
                    let _ = progress_tx.send(ProgressMessage::Warning(
                        filename.clone(),
                        format!(
                            "Unused private overwrite staging could not be cleaned: {}",
                            cleanup_error
                        ),
                    ));
                }
            }
            failure_count += 1;
            let _ = progress_tx.send(ProgressMessage::Error(
                filename,
                "Cannot copy special file".to_string(),
            ));
        }
    }

    if cancelled {
        let _ = progress_tx.send(ProgressMessage::Error(
            String::new(),
            "Cancelled".to_string(),
        ));
        let _ = progress_tx.send(ProgressMessage::Completed(success_count, failure_count + 1));
    } else {
        let _ = progress_tx.send(ProgressMessage::Completed(success_count, failure_count));
    }
}

/// Move files with progress reporting
/// files_to_overwrite: Set of source paths that should overwrite existing destinations
/// files_to_skip: Set of source paths that should be skipped if destination exists
pub fn move_files_with_progress(
    files: Vec<PathBuf>,
    source_dir: &Path,
    target_dir: &Path,
    mut files_to_overwrite: HashMap<PathBuf, PathAuthorization>,
    files_to_skip: HashSet<PathBuf>,
    target_authorization: Option<DirectoryAuthorization>,
    mut source_authorizations: HashMap<PathBuf, PathAuthorization>,
    source_directory_authorization: Option<DirectoryAuthorization>,
    cancel_flag: Arc<AtomicBool>,
    progress_tx: Sender<ProgressMessage>,
) {
    let mut success_count = 0;
    let mut failure_count = 0;
    let mut cancelled = false;
    let authorized_target_path = target_authorization
        .as_ref()
        .map(|authorization| authorization.resolved.clone());
    let authorized_source_path = source_directory_authorization
        .as_ref()
        .map(|authorization| authorization.resolved.clone());
    let target_dir = authorized_target_path.as_deref().unwrap_or(target_dir);
    let source_dir = authorized_source_path.as_deref().unwrap_or(source_dir);

    if let Some(authorization) = target_authorization.as_ref() {
        if let Err(error) =
            authorized_current_directory(target_dir, authorization, "Paste target directory")
        {
            send_prepare_error_result(&progress_tx, error, files.len());
            return;
        }
    }
    if let Some(authorization) = source_directory_authorization.as_ref() {
        if let Err(error) =
            authorized_current_directory(source_dir, authorization, "Clipboard source directory")
        {
            send_prepare_error_result(&progress_tx, error, files.len());
            return;
        }
    }

    // Build full paths for size calculation (excluding skipped files)
    let full_paths: Vec<PathBuf> = files
        .iter()
        .map(|f| {
            if f.is_absolute() {
                f.clone()
            } else {
                source_dir.join(f)
            }
        })
        .filter(|p| !files_to_skip.contains(p))
        .collect();

    // Send preparing message before calculating sizes
    let _ = progress_tx.send(ProgressMessage::Preparing(
        "Calculating file sizes...".to_string(),
    ));

    // Calculate total size upfront for accurate progress
    let (total_bytes, total_files) = match calculate_total_size(&full_paths, &cancel_flag) {
        Ok((size, count)) => (size, count),
        Err(e) => {
            send_prepare_error_result(&progress_tx, e, files.len());
            return;
        }
    };

    // Send prepare complete
    let _ = progress_tx.send(ProgressMessage::PrepareComplete);

    let mut completed_bytes: u64 = 0;
    let mut completed_files: usize = 0;

    // First, try simple rename for each file (fast path for same filesystem)
    let mut needs_copy: Vec<(
        PathBuf,
        PathBuf,
        u64,
        bool,
        PathIdentity,
        Option<PathIdentity>,
    )> = Vec::new();

    for file_path in &files {
        if cancel_flag.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }

        let src = if file_path.is_absolute() {
            file_path.clone()
        } else {
            source_dir.join(file_path)
        };
        if files_to_skip.contains(&src) {
            continue;
        }

        if let Some(authorization) = target_authorization.as_ref() {
            if let Err(error) =
                authorized_current_directory(target_dir, authorization, "Paste target directory")
            {
                failure_count += 1;
                let name = src
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let _ = progress_tx.send(ProgressMessage::Error(name, error.to_string()));
                break;
            }
        }
        if let Some(authorization) = source_directory_authorization.as_ref() {
            if let Err(error) = authorized_current_directory(
                source_dir,
                authorization,
                "Clipboard source directory",
            ) {
                failure_count += 1;
                let name = src
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let _ = progress_tx.send(ProgressMessage::Error(name, error.to_string()));
                break;
            }
        }

        let filename = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let dest = target_dir.join(&filename);

        if let Err(e) = validate_destination_not_self(&src, &dest, "move") {
            failure_count += 1;
            let _ = progress_tx.send(ProgressMessage::Error(filename, e.to_string()));
            continue;
        }

        let source_identity = match source_authorizations.remove(&src) {
            Some(authorization) => {
                match authorized_current_identity(&src, &authorization, "Clipboard source item") {
                    Ok(identity) => identity,
                    Err(error) => {
                        failure_count += 1;
                        let _ =
                            progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                        continue;
                    }
                }
            }
            None if source_directory_authorization.is_some() => {
                failure_count += 1;
                let _ = progress_tx.send(ProgressMessage::Error(
                    filename,
                    "Missing clipboard authorization for local source item".to_string(),
                ));
                continue;
            }
            None => match path_identity(&src) {
                Ok(identity) => identity,
                Err(error) => {
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                    continue;
                }
            },
        };
        let source_metadata = match fs::symlink_metadata(&src) {
            Ok(metadata) => metadata,
            Err(error) => {
                failure_count += 1;
                let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                continue;
            }
        };

        // Get file/dir size for progress tracking without following a top-level symlink.
        let (item_size, item_files) = if source_metadata.is_dir() && !source_metadata.is_symlink() {
            match calculate_dir_size(&src, &cancel_flag) {
                Ok(size) => size,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    cancelled = true;
                    break;
                }
                Err(_) => (0, 1),
            }
        } else if source_metadata.is_file() {
            (source_metadata.len(), 1)
        } else if source_metadata.is_symlink() {
            (0, 1)
        } else {
            failure_count += 1;
            let _ = progress_tx.send(ProgressMessage::Error(
                filename,
                "Cannot move special file".to_string(),
            ));
            continue;
        };

        let dest_exists = path_exists_no_follow(&dest);
        let overwrite_authorization = files_to_overwrite.remove(&src);
        let overwriting = overwrite_authorization.is_some();
        if overwriting && !dest_exists {
            failure_count += 1;
            let _ = progress_tx.send(ProgressMessage::Error(
                filename,
                "Destination changed after overwrite confirmation; retry the operation".to_string(),
            ));
            continue;
        }
        if dest_exists && !overwriting {
            // Not in overwrite set and not in skip set - unexpected conflict
            failure_count += 1;
            let _ = progress_tx.send(ProgressMessage::Error(
                filename,
                "Target already exists".to_string(),
            ));
            continue;
        }
        let destination_identity = if overwriting {
            match authorized_current_identity(
                &dest,
                overwrite_authorization
                    .as_ref()
                    .expect("overwrite authorization was captured"),
                "Overwrite destination",
            ) {
                Ok(identity) => Some(identity),
                Err(error) => {
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                    continue;
                }
            }
        } else {
            None
        };
        let _ = progress_tx.send(ProgressMessage::FileStarted(filename.clone()));

        // A same-filesystem overwrite first isolates the exact
        // source object under the target parent, then transactionally installs
        // that staging name. EXDEV is rejected before any destination change.
        if overwriting {
            let destination_identity =
                destination_identity.expect("overwrite destination identity was captured");
            if source_identity.stable.namespace == destination_identity.stable.namespace {
                match move_entry_overwrite_same_filesystem(
                    &src,
                    &dest,
                    &source_identity,
                    destination_identity,
                ) {
                    Ok(warnings) => {
                        send_operation_warnings(&progress_tx, &filename, warnings);
                        success_count += 1;
                        completed_bytes += item_size;
                        completed_files += item_files;
                        let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
                        let _ = progress_tx.send(ProgressMessage::TotalProgress(
                            completed_files,
                            total_files,
                            completed_bytes,
                            total_bytes,
                        ));
                    }
                    Err(failure) => {
                        failure_count += 1;
                        let message = if failure.terminal {
                            ProgressMessage::TerminalError(filename, failure.error.to_string())
                        } else {
                            ProgressMessage::Error(filename, failure.error.to_string())
                        };
                        let _ = progress_tx.send(message);
                    }
                }
                continue;
            }
            needs_copy.push((
                src,
                dest,
                item_size,
                true,
                source_identity,
                Some(destination_identity),
            ));
            continue;
        }

        // Try an atomic rename first. The no-overwrite path must use an
        // explicit no-clobber primitive because POSIX rename replaces a target
        // that appears after the existence check.
        match rename_noreplace(&src, &dest) {
            Ok(()) => {
                let moved_identity = path_identity(&dest);
                if !matches!(moved_identity, Ok(identity) if identity.matches_after_relocation(&source_identity))
                {
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::TerminalError(
                        filename,
                        "Source changed during rename; the moved entry was left at the destination"
                            .to_string(),
                    ));
                    continue;
                }
                let mut warnings = Vec::new();
                if let Err(error) = sync_parent(&dest) {
                    warnings.push(format!(
                        "Move committed at '{}', but destination-directory durability could not be confirmed: {}",
                        dest.display(), error
                    ));
                }
                if src.parent() != dest.parent() {
                    if let Err(error) = sync_parent(&src) {
                        warnings.push(format!(
                            "Move committed from '{}', but source-directory durability could not be confirmed: {}",
                            src.display(), error
                        ));
                    }
                }
                send_operation_warnings(&progress_tx, &filename, warnings);
                success_count += 1;
                completed_bytes += item_size;
                completed_files += item_files;
                let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
                let _ = progress_tx.send(ProgressMessage::TotalProgress(
                    completed_files,
                    total_files,
                    completed_bytes,
                    total_bytes,
                ));
            }
            Err(e) => {
                // Cross-device rename cannot publish directly. Unsupported
                // no-replace primitives also use the safe copy/publish path.
                if is_cross_device_error(&e) {
                    needs_copy.push((
                        src,
                        dest,
                        item_size,
                        overwriting,
                        source_identity,
                        destination_identity,
                    ));
                } else if e.kind() == io::ErrorKind::Unsupported {
                    needs_copy.push((
                        src,
                        dest,
                        item_size,
                        overwriting,
                        source_identity,
                        destination_identity,
                    ));
                } else {
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, e.to_string()));
                }
            }
        }
    }

    // Handle cross-device moves. The copy is completed in a private target
    // stage, then the exact source is isolated before destination publication.
    // This keeps pre-commit failures reversible and prevents a changed source
    // from being deleted after its copy was published.
    if !needs_copy.is_empty() && !cancelled {
        for (src, dest, item_size, overwriting, source_identity, destination_identity) in needs_copy
        {
            if cancel_flag.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }

            let filename = src
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Some(authorization) = target_authorization.as_ref() {
                if let Err(error) = authorized_current_directory(
                    target_dir,
                    authorization,
                    "Paste target directory",
                ) {
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                    continue;
                }
            }

            let _ = progress_tx.send(ProgressMessage::FileStarted(filename.clone()));
            let mut destination_staging = match PrivateStagingDirectory::create(
                dest.parent().unwrap_or_else(|| Path::new(".")),
                "move-copy",
            ) {
                Ok(staging) => Some(staging),
                Err(error) => {
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                    continue;
                }
            };
            if let Some(authorization) = target_authorization.as_ref() {
                if let Err(error) = authorized_current_directory(
                    target_dir,
                    authorization,
                    "Paste target directory",
                ) {
                    let error = error_with_staging_cleanup(
                        error,
                        destination_staging
                            .take()
                            .expect("destination staging was created"),
                    );
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                    continue;
                }
            }
            let copy_dest = destination_staging
                .as_ref()
                .expect("destination staging was created")
                .payload();

            let source_metadata = match fs::symlink_metadata(&src) {
                Ok(metadata) => metadata,
                Err(error) => {
                    let error = error_with_staging_cleanup(
                        error,
                        destination_staging
                            .take()
                            .expect("destination staging was created"),
                    );
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                    continue;
                }
            };
            if !matches!(path_identity(&src), Ok(identity) if identity.same_snapshot(&source_identity))
            {
                let error = error_with_staging_cleanup(
                    io::Error::other("Source changed before it could be copied"),
                    destination_staging
                        .take()
                        .expect("destination staging was created"),
                );
                failure_count += 1;
                let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                continue;
            }
            let copy_source_authorization = PathAuthorization::from_identity(&source_identity);
            let source_is_directory = source_metadata.is_dir() && !source_metadata.is_symlink();

            let copy_result: io::Result<(PublishedStage, Option<[u8; 32]>)> = if source_is_directory
            {
                copy_dir_recursive_with_progress_detailed(
                    &src,
                    &copy_dest,
                    &dest,
                    Some(&copy_source_authorization),
                    &cancel_flag,
                    &progress_tx,
                    &mut completed_bytes,
                    &mut completed_files,
                    total_bytes,
                    total_files,
                )
                .map(|published| (published, None))
            } else if source_metadata.is_symlink() {
                copy_symlink_detailed(&src, &copy_dest, &dest, Some(&copy_source_authorization))
                    .map(|published| (published, None))
            } else if source_metadata.is_file() {
                let file_completed_bytes = completed_bytes;

                copy_file_with_progress_detailed(
                    &src,
                    &copy_dest,
                    Some(&copy_source_authorization),
                    &cancel_flag,
                    |copied, total| {
                        let _ = progress_tx.send(ProgressMessage::FileProgress(copied, total));
                        let _ = progress_tx.send(ProgressMessage::TotalProgress(
                            completed_files,
                            total_files,
                            file_completed_bytes + copied,
                            total_bytes,
                        ));
                    },
                )
                .map(|(_, published, source_sha256)| (published, Some(source_sha256)))
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Cannot move special file",
                ))
            };

            match copy_result {
                Ok((published, copied_source_sha256)) => {
                    let staging = destination_staging
                        .take()
                        .expect("destination staging was created");
                    let (published_identity, copy_warnings) =
                        match verified_publication_parts(published, &copy_dest) {
                            Ok(parts) => parts,
                            Err(error) => {
                                let error = error_with_staging_cleanup(error, staging);
                                failure_count += 1;
                                let _ = progress_tx
                                    .send(ProgressMessage::Error(filename, error.to_string()));
                                continue;
                            }
                        };
                    let stage = match OwnedStage::from_published(staging, published_identity) {
                        Ok(stage) => stage,
                        Err(error) => {
                            failure_count += 1;
                            let _ = progress_tx
                                .send(ProgressMessage::Error(filename, error.to_string()));
                            continue;
                        }
                    };

                    if cancel_flag.load(Ordering::Relaxed) {
                        if let Err(cleanup_error) = stage.cleanup() {
                            let _ = progress_tx.send(ProgressMessage::Warning(
                                filename.clone(),
                                format!("Cancelled move staging cleanup failed: {}", cleanup_error),
                            ));
                        }
                        cancelled = true;
                        break;
                    }

                    let mut isolated_source =
                        match QuarantinedSource::prepare(&src, source_identity) {
                            Ok(source) => source,
                            Err(error) => {
                                let retry_unsafe = is_retry_unsafe(&error);
                                let error = match stage.cleanup() {
                                    Ok(()) => error,
                                    Err(cleanup_error) => operation_error(
                                        error.kind(),
                                        format!(
                                            "{}; destination staging cleanup also failed: {}",
                                            error, cleanup_error
                                        ),
                                        retry_unsafe,
                                    ),
                                };
                                failure_count += 1;
                                let message = if is_retry_unsafe(&error) {
                                    ProgressMessage::TerminalError(filename, error.to_string())
                                } else {
                                    ProgressMessage::Error(filename, error.to_string())
                                };
                                let _ = progress_tx.send(message);
                                continue;
                            }
                        };

                    let source_verification = if source_is_directory {
                        isolated_source.bind_verified_directory_copy(&stage)
                    } else if let Some(copied_source_sha256) = copied_source_sha256 {
                        verify_isolated_source_matches_copy(&isolated_source, copied_source_sha256)
                    } else {
                        Ok(())
                    };
                    if let Err(verify_error) = source_verification {
                        let error = match stage.cleanup() {
                            Ok(()) => verify_error,
                            Err(cleanup_error) => io::Error::new(
                                verify_error.kind(),
                                format!(
                                    "{}; destination staging cleanup also failed: {}",
                                    verify_error, cleanup_error
                                ),
                            ),
                        };
                        let error = isolated_source.restore(error);
                        failure_count += 1;
                        let message = if is_retry_unsafe(&error) {
                            ProgressMessage::TerminalError(filename, error.to_string())
                        } else {
                            ProgressMessage::Error(filename, error.to_string())
                        };
                        let _ = progress_tx.send(message);
                        continue;
                    }

                    if cancel_flag.load(Ordering::Relaxed) {
                        let cancellation = match stage.cleanup() {
                            Ok(()) => io::Error::new(io::ErrorKind::Interrupted, "Cancelled"),
                            Err(cleanup_error) => io::Error::new(
                                io::ErrorKind::Interrupted,
                                format!(
                                    "Cancelled; destination staging cleanup also failed: {}",
                                    cleanup_error
                                ),
                            ),
                        };
                        let cancellation = isolated_source.restore(cancellation);
                        if is_retry_unsafe(&cancellation) {
                            let _ = progress_tx.send(ProgressMessage::TerminalError(
                                filename.clone(),
                                cancellation.to_string(),
                            ));
                        } else if cancellation.to_string() != "Cancelled; the source was restored" {
                            let _ = progress_tx.send(ProgressMessage::Warning(
                                filename.clone(),
                                cancellation.to_string(),
                            ));
                        }
                        cancelled = true;
                        break;
                    }

                    let expected_destination = if overwriting {
                        destination_identity
                    } else {
                        None
                    };
                    match commit_cross_filesystem_move(
                        stage,
                        &dest,
                        expected_destination,
                        isolated_source,
                        copy_warnings,
                        target_authorization.as_ref(),
                    ) {
                        Ok(warnings) => {
                            send_operation_warnings(&progress_tx, &filename, warnings);
                            if !source_is_directory {
                                completed_bytes += item_size;
                                completed_files += 1;
                            }
                            success_count += 1;
                            let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
                        }
                        Err(CrossFilesystemMoveFailure::NotCommitted(error)) => {
                            failure_count += 1;
                            let _ = progress_tx
                                .send(ProgressMessage::Error(filename, error.to_string()));
                        }
                        Err(CrossFilesystemMoveFailure::CommittedUnverified(error)) => {
                            failure_count += 1;
                            let _ = progress_tx
                                .send(ProgressMessage::TerminalError(filename, error.to_string()));
                        }
                    }
                }
                Err(error) => {
                    let error = error_with_staging_cleanup(
                        error,
                        destination_staging
                            .take()
                            .expect("destination staging was created"),
                    );
                    if error.kind() == io::ErrorKind::Interrupted {
                        if error.to_string() != "Cancelled" {
                            let _ = progress_tx.send(ProgressMessage::Warning(
                                filename.clone(),
                                error.to_string(),
                            ));
                        }
                        cancelled = true;
                        break;
                    }
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, error.to_string()));
                }
            }
        }
    }

    if cancelled {
        let _ = progress_tx.send(ProgressMessage::Error(
            String::new(),
            "Cancelled".to_string(),
        ));
        let _ = progress_tx.send(ProgressMessage::Completed(success_count, failure_count + 1));
    } else {
        let _ = progress_tx.send(ProgressMessage::Completed(success_count, failure_count));
    }
}

/// Copy a file or directory
pub fn copy_file(src: &Path, dest: &Path) -> io::Result<()> {
    validate_destination_not_self(src, dest, "copy")?;

    // Check if destination already exists
    if path_exists_no_follow(dest) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Target already exists. Delete it first or choose a different name.",
        ));
    }

    let src_metadata = fs::symlink_metadata(src)?;
    if src_metadata.is_symlink() {
        copy_symlink(src, dest)
    } else if src_metadata.is_dir() {
        copy_dir_recursive(src, dest)
    } else if src_metadata.is_file() {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        copy_file_with_progress(src, dest, &cancel_flag, |_, _| {}).map(|_| ())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cannot copy special file (device, socket, or pipe)",
        ))
    }
}

/// Maximum recursion depth for directory copy to prevent stack overflow
const MAX_COPY_DEPTH: usize = 256;

/// Copy directory recursively with symlink loop detection
pub fn copy_dir_recursive(src: &Path, dest: &Path) -> io::Result<()> {
    copy_dir_recursive_detailed(src, dest, dest, None).map(drop)
}

fn copy_dir_recursive_detailed(
    src: &Path,
    dest: &Path,
    logical_destination: &Path,
    expected_source: Option<&PathAuthorization>,
) -> io::Result<PublishedStage> {
    let metadata = fs::symlink_metadata(src)?;
    if metadata.is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Source is not a directory",
        ));
    }
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let staging = PrivateStagingDirectory::create(parent, "copy-directory")?;
    let temp = staging.payload();
    if let Err(error) = create_private_directory(&temp) {
        return Err(error_with_staging_cleanup(error, staging));
    }
    let partial = PartialStage::bind_directory(staging)?;
    let mut visited = HashSet::new();
    match copy_dir_recursive_inner(
        src,
        &temp,
        logical_destination,
        expected_source,
        &mut visited,
        0,
        true,
        None,
    ) {
        Ok(()) => {
            let stage = partial.seal()?;
            match stage.publish_noreplace(dest) {
                Ok(published) if published.identity.is_some() => Ok(published),
                Ok(_) => Err(io::Error::other(format!(
                    "Directory publication at '{}' committed, but the destination no longer identifies the staged directory; inspect it manually",
                    dest.display()
                ))),
                Err(failure) => Err(error_with_owned_stage_cleanup(failure)),
            }
        }
        Err(error) => Err(error_with_partial_stage_cleanup(error, partial)),
    }
}

/// Internal recursive copy with visited path tracking
fn copy_dir_recursive_inner(
    src: &Path,
    dest: &Path,
    logical_destination: &Path,
    expected_source: Option<&PathAuthorization>,
    visited: &mut HashSet<StablePathIdentity>,
    depth: usize,
    destination_already_created: bool,
    opened_source: Option<(File, DirectoryAccess, fs::Metadata)>,
) -> io::Result<()> {
    // Check maximum depth to prevent stack overflow
    if depth > MAX_COPY_DEPTH {
        return Err(io::Error::other(format!(
            "Maximum directory depth ({}) exceeded - possible circular symlink",
            MAX_COPY_DEPTH
        )));
    }

    // Pin the source directory before enumerating it so a concurrent path
    // replacement cannot redirect the copy through a symlink.
    let (source_guard, source_access, src_metadata) = match opened_source {
        Some(opened) => opened,
        None => open_directory_for_read(src)?,
    };
    let source_identity = stable_file_identity(&source_guard)?;
    if let Some(expected) = expected_source {
        let current = path_identity(src)?;
        if !expected.matches_snapshot(&current)
            || stable_file_identity(&source_guard)? != current.stable
        {
            return Err(io::Error::other(format!(
                "Clipboard source directory changed before it could be copied: '{}'",
                src.display()
            )));
        }
    }
    drop(source_guard);

    // Detect cycles by opened filesystem identity. `visited` tracks only the
    // current DFS stack, not every directory ever seen, so sibling aliases do
    // not produce false positives. Every exit removes the current identity.
    if visited.contains(&source_identity) {
        return Err(io::Error::other(format!(
            "Circular symlink detected: {}",
            src.display()
        )));
    }
    visited.insert(source_identity);

    // Run the body inside an IIFE so any early return (`?`, explicit
    // `return Err`) still goes through the `visited.remove` cleanup
    // below — that's what enforces stack-only semantics.
    let result: io::Result<()> = (|| {
        if !destination_already_created {
            create_private_directory(dest)?;
        }

        // Close the per-directory enumeration descriptor before descending.
        // Retaining only subdirectory names/identities preserves streaming for
        // ordinary files without multiplying descriptors by recursion depth.
        let mut child_directories = Vec::new();
        for name in source_access.entries()? {
            let name = name?;
            let src_path = src.join(&name);
            let dest_path = dest.join(&name);
            let logical_dest_path = logical_destination.join(&name);

            // Get metadata without following symlinks
            let metadata = source_access.child_metadata(&name)?;

            if metadata.is_symlink() {
                copy_open_symlink_to_new(
                    &source_access,
                    &name,
                    &src_path,
                    &dest_path,
                    &logical_dest_path,
                )?;
            } else if metadata.is_dir() {
                child_directories.push((name, metadata.identity()));
            } else if metadata.is_file() {
                let cancel_flag = Arc::new(AtomicBool::new(false));
                let (mut source_file, source_metadata) = source_access.open_regular_file(&name)?;
                copy_open_regular_file_into_private_tree(
                    &mut source_file,
                    &source_metadata,
                    &src_path,
                    &dest_path,
                    &cancel_flag,
                    |_, _| {},
                )?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Cannot copy special file: {}", src_path.display()),
                ));
            }
        }

        for (name, expected_identity) in child_directories {
            let src_path = src.join(&name);
            let dest_path = dest.join(&name);
            let logical_dest_path = logical_destination.join(&name);
            let opened_child = source_access.open_directory(&name)?;
            if stable_file_identity(&opened_child.0)? != expected_identity {
                return Err(io::Error::other(format!(
                    "Source directory changed before it could be copied: '{}'",
                    src_path.display()
                )));
            }
            copy_dir_recursive_inner(
                &src_path,
                &dest_path,
                &logical_dest_path,
                None,
                visited,
                depth + 1,
                false,
                Some(opened_child),
            )?;
        }

        // Preserve metadata through the exact no-follow directory handle;
        // path-based chmod/utimes could be redirected after recursive copy.
        preserve_directory_metadata_no_follow(dest, &src_metadata)?;
        sync_parent(dest)?;
        if !metadata_still_matches(&src_metadata, &source_access.file().metadata()?) {
            return Err(io::Error::other(format!(
                "Source directory changed while it was being copied: '{}'",
                src.display()
            )));
        }

        Ok(())
    })();

    visited.remove(&source_identity);
    result
}

/// Move a file or directory
pub fn move_file(src: &Path, dest: &Path) -> io::Result<()> {
    validate_destination_not_self(src, dest, "move")?;

    // Check if destination already exists
    if path_exists_no_follow(dest) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Target already exists. Delete it first or choose a different name.",
        ));
    }

    let identity = path_identity(src)?;

    // Try rename first (fast for same filesystem)
    match rename_noreplace(src, dest) {
        Ok(()) => {
            let moved_identity = path_identity(dest);
            if matches!(moved_identity, Ok(current) if current.matches_after_relocation(&identity))
            {
                // The namespace change already committed. Durability sync is
                // best-effort in this unstructured API; returning an ordinary
                // error here would invite a destructive retry of a completed
                // move.
                let _ = sync_parent(dest);
                if src.parent() != dest.parent() {
                    let _ = sync_parent(src);
                }
                Ok(())
            } else {
                Err(io::Error::other(
                    "Source changed during rename; the moved entry was left at the destination",
                ))
            }
        }
        Err(e) => {
            // If rename fails (cross-device), copy then delete
            if is_cross_device_error(&e) {
                move_file_via_verified_copy(src, dest, identity)
            } else if e.kind() == io::ErrorKind::Unsupported {
                move_file_via_verified_copy(src, dest, identity)
            } else {
                Err(e)
            }
        }
    }
}

fn move_file_via_verified_copy(
    src: &Path,
    dest: &Path,
    source_identity: PathIdentity,
) -> io::Result<()> {
    let staging = PrivateStagingDirectory::create(
        dest.parent().unwrap_or_else(|| Path::new(".")),
        "move-copy",
    )?;
    let copy_dest = staging.payload();
    let metadata = fs::symlink_metadata(src)?;
    let source_is_directory = metadata.is_dir() && !metadata.is_symlink();
    let copy_source_authorization = PathAuthorization::from_identity(&source_identity);
    let published = if metadata.is_symlink() {
        copy_symlink_detailed(src, &copy_dest, dest, Some(&copy_source_authorization))
            .map(|published| (published, None))
    } else if source_is_directory {
        copy_dir_recursive_detailed(src, &copy_dest, dest, Some(&copy_source_authorization))
            .map(|published| (published, None))
    } else if metadata.is_file() {
        let cancel = Arc::new(AtomicBool::new(false));
        copy_file_with_progress_detailed(
            src,
            &copy_dest,
            Some(&copy_source_authorization),
            &cancel,
            |_, _| {},
        )
        .map(|(_, published, source_sha256)| (published, Some(source_sha256)))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cannot move special file",
        ))
    };
    let (published, copied_source_sha256) = match published {
        Ok(published) => published,
        Err(error) => return Err(error_with_staging_cleanup(error, staging)),
    };
    let (identity, copy_warnings) = match verified_publication_parts(published, &copy_dest) {
        Ok(parts) => parts,
        Err(error) => return Err(error_with_staging_cleanup(error, staging)),
    };
    let stage = OwnedStage::from_published(staging, identity)?;
    let mut source = match QuarantinedSource::prepare(src, source_identity) {
        Ok(source) => source,
        Err(error) => {
            return Err(match stage.cleanup() {
                Ok(()) => error,
                Err(cleanup_error) => io::Error::new(
                    error.kind(),
                    format!(
                        "{}; destination staging cleanup also failed: {}",
                        error, cleanup_error
                    ),
                ),
            })
        }
    };
    let source_verification = if source_is_directory {
        source.bind_verified_directory_copy(&stage)
    } else if let Some(copied_source_sha256) = copied_source_sha256 {
        verify_isolated_source_matches_copy(&source, copied_source_sha256)
    } else {
        Ok(())
    };
    if let Err(verify_error) = source_verification {
        let error = match stage.cleanup() {
            Ok(()) => verify_error,
            Err(cleanup_error) => io::Error::new(
                verify_error.kind(),
                format!(
                    "{}; destination staging cleanup also failed: {}",
                    verify_error, cleanup_error
                ),
            ),
        };
        return Err(source.restore(error));
    }
    match commit_cross_filesystem_move(stage, dest, None, source, copy_warnings, None) {
        Ok(warnings) if warnings.is_empty() => Ok(()),
        Ok(warnings) => Err(io::Error::other(format!(
            "Move committed with warnings; do not retry automatically: {}",
            warnings.join("; ")
        ))),
        Err(
            CrossFilesystemMoveFailure::NotCommitted(error)
            | CrossFilesystemMoveFailure::CommittedUnverified(error),
        ) => Err(error),
    }
}

/// Delete a file or directory
pub fn delete_file(path: &Path) -> io::Result<()> {
    delete_file_detailed(path).map(|_| ())
}

/// Delete an exact selected entry. Once the verified payload has been removed,
/// cleanup and directory-sync failures are warnings: reporting an ordinary
/// failure at that point would invite a destructive retry of a name that may
/// already refer to a different object.
pub(crate) fn delete_file_detailed(path: &Path) -> io::Result<Vec<String>> {
    delete_file_detailed_impl(path, None)
}

pub(crate) fn delete_file_detailed_authorized(
    path: &Path,
    expected_source: &PathAuthorization,
) -> io::Result<Vec<String>> {
    delete_file_detailed_impl(path, Some(expected_source))
}

fn delete_file_detailed_impl(
    path: &Path,
    expected_source: Option<&PathAuthorization>,
) -> io::Result<Vec<String>> {
    let expected = path_identity(path)?;
    if expected_source.is_some_and(|authorization| !authorization.matches_snapshot(&expected)) {
        return Err(io::Error::other(format!(
            "The item confirmed for deletion changed before it could be isolated: '{}'",
            path.display()
        )));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let quarantine = PrivateStagingDirectory::create(parent, "delete-selected")?;
    let isolated = quarantine.payload();
    if let Err(error) = rename_noreplace(path, &isolated) {
        return Err(error_with_staging_cleanup(error, quarantine));
    }

    let moved = match path_identity(&isolated) {
        Ok(moved) if moved.matches_after_relocation(&expected) => moved,
        Ok(_) => {
            let error = restore_staged_directory_after_failure(
                path,
                &isolated,
                path,
                &expected,
                io::Error::other("Selected path changed while it was isolated for deletion"),
            );
            return Err(error_with_staging_cleanup(error, quarantine));
        }
        Err(error) => {
            let error =
                restore_staged_directory_after_failure(path, &isolated, path, &expected, error);
            return Err(error_with_staging_cleanup(error, quarantine));
        }
    };
    if let Err(error) = sync_parent(path).and_then(|()| sync_parent(&isolated)) {
        drop(moved);
        let error = restore_staged_directory_after_failure(path, &isolated, path, &expected, error);
        return Err(error_with_staging_cleanup(error, quarantine));
    }
    let prepared_file = if moved.is_directory {
        None
    } else {
        Some(prepare_file_deletion(&isolated, moved.stable).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Verified selected-file deletion could not be bound: {}. Recovery data remains under '{}'",
                    error,
                    quarantine.path.display()
                ),
            )
        })?)
    };
    drop(moved);
    drop(expected);

    let deletion = match prepared_file {
        Some(prepared) => prepared.delete(),
        None => delete_path_unchecked(&isolated),
    };
    if let Err(error) = deletion {
        return Err(io::Error::new(
            error.kind(),
            format!(
                "Could not delete the verified selected entry: {}. Remaining recovery data is preserved under '{}'",
                error,
                quarantine.path.display()
            ),
        ));
    }
    let cleanup_path = quarantine.path.clone();
    let mut warnings = Vec::new();
    if let Err(error) = quarantine.cleanup() {
        warnings.push(format!(
            "Deletion committed, but private cleanup could not be confirmed under '{}': {}",
            cleanup_path.display(),
            error
        ));
    }
    if let Err(error) = sync_parent(path) {
        warnings.push(format!(
            "Deletion committed, but parent-directory durability could not be confirmed for '{}': {}",
            path.display(),
            error
        ));
    }
    Ok(warnings)
}

fn delete_path_unchecked(path: &Path) -> io::Result<()> {
    // Use symlink_metadata to check if it's a symlink
    let metadata = fs::symlink_metadata(path)?;

    // A Windows junction/mount point is a directory reparse point but is not
    // guaranteed to be reported as a symbolic link. Never recurse through it.
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return if metadata.is_dir() {
                fs::remove_dir(path)
            } else {
                fs::remove_file(path)
            };
        }
    }

    if metadata.is_symlink() {
        // Just remove the symlink itself, don't follow it
        fs::remove_file(path)
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Create a new directory
pub fn create_directory(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // The final component is created exclusively. Unlike exists()+create_dir_all,
    // this cannot report success when a racing file or symlink occupies it.
    fs::create_dir(path)
}

/// Rename a file or directory
pub fn rename_file(old_path: &Path, new_path: &Path) -> io::Result<()> {
    rename_file_detailed(old_path, new_path, None, None).map(|_| ())
}

pub(crate) fn rename_file_authorized(
    old_path: &Path,
    new_path: &Path,
    expected_source: &PathAuthorization,
    expected_directory: &DirectoryAuthorization,
) -> io::Result<Vec<String>> {
    rename_file_detailed(
        old_path,
        new_path,
        Some(expected_source),
        Some(expected_directory),
    )
}

fn rename_file_detailed(
    old_path: &Path,
    new_path: &Path,
    expected_source: Option<&PathAuthorization>,
    expected_directory: Option<&DirectoryAuthorization>,
) -> io::Result<Vec<String>> {
    if let Some(directory) = expected_directory {
        authorized_current_directory(
            directory.resolved_path(),
            directory,
            "Rename parent directory",
        )?;
        if old_path.parent() != Some(directory.resolved_path())
            || new_path.parent() != Some(directory.resolved_path())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Rename paths no longer belong to the confirmed directory",
            ));
        }
    }
    if path_exists_no_follow(new_path) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Target already exists",
        ));
    }

    let expected = match expected_source {
        Some(authorization) => {
            authorized_current_identity(old_path, authorization, "Rename source item")?
        }
        None => path_identity(old_path)?,
    };
    rename_noreplace(old_path, new_path)?;
    let verified = matches!(
        path_identity(new_path),
        Ok(current) if current.matches_after_relocation(&expected)
    );
    let mut warnings = Vec::new();
    if let Err(error) = sync_parent(new_path) {
        warnings.push(format!(
            "Rename committed at '{}', but destination-directory durability could not be confirmed: {}",
            new_path.display(), error
        ));
    }
    if old_path.parent() != new_path.parent() {
        if let Err(error) = sync_parent(old_path) {
            warnings.push(format!(
                "Rename committed from '{}', but source-directory durability could not be confirmed: {}",
                old_path.display(), error
            ));
        }
    }
    if verified {
        Ok(warnings)
    } else {
        Err(operation_error(
            io::ErrorKind::Other,
            format!(
                "Rename committed at '{}', but the moved entry could not be rebound; do not retry automatically",
                new_path.display()
            ),
            true,
        ))
    }
}

/// Maximum filename length (POSIX limit)
const MAX_FILENAME_LENGTH: usize = 255;

/// Validate filename for dangerous characters
pub fn is_valid_filename(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.trim().is_empty() {
        return Err("Filename cannot be empty");
    }

    // Check for path separators
    if name.contains('/') || name.contains('\\') {
        return Err("Filename cannot contain path separators");
    }

    // Check for null bytes
    if name.contains('\0') {
        return Err("Filename cannot contain null bytes");
    }

    // Check for reserved names
    if name == "." || name == ".." {
        return Err("Invalid filename");
    }

    // Check length limit
    if name.len() > MAX_FILENAME_LENGTH {
        return Err("Filename too long (max 255 characters)");
    }

    // Check for control characters
    if name.chars().any(|c| c.is_control()) {
        return Err("Filename cannot contain control characters");
    }

    // Check for leading/trailing whitespace
    if name != name.trim() {
        return Err("Filename cannot start or end with whitespace");
    }

    // Check for leading hyphen (could be interpreted as option)
    if name.starts_with('-') {
        return Err("Filename cannot start with hyphen");
    }

    Ok(())
}

/// Sensitive paths that symlinks should not point to
#[cfg(unix)]
const SENSITIVE_PATHS: &[&str] = &[
    "/etc", "/sys", "/proc", "/boot", "/root", "/var/log", "/home", "/dev", "/run", "/var/run",
];

#[cfg(windows)]
const SENSITIVE_PATHS: &[&str] = &[
    "C:\\Windows",
    "C:\\Program Files",
    "C:\\Program Files (x86)",
];

/// True iff `target` equals or is contained within one of `SENSITIVE_PATHS`.
/// Matches on path-segment boundaries, so "/etc" does not match "/etcd/foo".
fn target_is_sensitive(target: &str) -> bool {
    #[cfg(unix)]
    const SEP: char = '/';
    #[cfg(windows)]
    const SEP: char = '\\';
    for sensitive in SENSITIVE_PATHS {
        if target == *sensitive {
            return true;
        }
        let mut boundary = String::with_capacity(sensitive.len() + 1);
        boundary.push_str(sensitive);
        boundary.push(SEP);
        if target.starts_with(&boundary) {
            return true;
        }
    }
    false
}

/// Check symlinks in files to be archived for security
/// Returns an error if any symlink points outside base_dir or to sensitive system paths
pub fn check_symlinks_for_tar(base_dir: &Path, files: &[String]) -> io::Result<()> {
    use std::collections::HashSet;
    // Compute base canonical once and fail-secure if it cannot be resolved.
    let base_canonical = base_dir.canonicalize().map(strip_unc_prefix).map_err(|e| {
        io::Error::other(format!(
            "Cannot canonicalize base directory '{}': {}",
            base_dir.display(),
            e
        ))
    })?;
    let mut visited = HashSet::new();
    for file in files {
        let file_path = base_dir.join(file);
        check_symlink_recursive(&file_path, &base_canonical, &mut visited)?;
    }
    Ok(())
}

/// Recursively check symlinks in a file or directory.
/// `base_canonical` is the canonicalised archive root and is used as the
/// containment boundary; computing it once also closes the prior fail-open
/// behaviour where a transient base canonicalize failure bypassed all checks.
fn check_symlink_recursive(
    path: &Path,
    base_canonical: &Path,
    visited: &mut std::collections::HashSet<std::path::PathBuf>,
) -> io::Result<()> {
    // Detect symlink loops using visited set
    if let Ok(canonical_path) = path.canonicalize().map(strip_unc_prefix) {
        if !visited.insert(canonical_path.clone()) {
            // Already visited - symlink loop detected, skip to avoid infinite recursion
            return Ok(());
        }
    }

    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            // File doesn't exist - this is a dangling symlink if parent exists
            if path.parent().map(|p| p.exists()).unwrap_or(false) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Dangling symlink or inaccessible file: {}", path.display()),
                ));
            }
            return Err(e);
        }
    };

    if metadata.is_symlink() {
        let link_target = fs::read_link(path)?;

        // Absolute symlinks pointing outside base_dir are always rejected
        if link_target.is_absolute() {
            match link_target.canonicalize().map(strip_unc_prefix) {
                Ok(target_canonical) => {
                    if !target_canonical.starts_with(base_canonical) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!(
                                "Symlink '{}' points outside archive directory: {}",
                                path.display(),
                                link_target.display()
                            ),
                        ));
                    }
                }
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "Symlink '{}' points to unresolvable absolute path: {}",
                            path.display(),
                            link_target.display()
                        ),
                    ));
                }
            }
        }

        // Resolve the symlink target to check where it actually points
        let resolved_target = if link_target.is_absolute() {
            link_target.clone()
        } else {
            // Relative symlink - resolve from the symlink's parent directory
            let parent = path.parent().unwrap_or(base_canonical);
            parent.join(&link_target)
        };

        // Get canonical path to resolve all symlinks and ".." components
        match resolved_target.canonicalize().map(strip_unc_prefix) {
            Ok(canonical) => {
                let target_str = canonical.to_string_lossy();
                if !canonical.starts_with(base_canonical) {
                    // Use the more specific sensitive-path message when applicable.
                    if target_is_sensitive(&target_str) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!(
                                "Symlink '{}' points to sensitive system path: {}",
                                path.display(),
                                target_str
                            ),
                        ));
                    }
                    // Otherwise reject any symlink pointing outside base_dir.
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "Symlink '{}' points outside archive directory: {}",
                            path.display(),
                            target_str
                        ),
                    ));
                }
            }
            Err(_) => {
                // Cannot resolve the target - this could be a dangling symlink
                // or circular reference. Reject for safety.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Symlink '{}' has unresolvable target: {}",
                        path.display(),
                        link_target.display()
                    ),
                ));
            }
        }
    } else if metadata.is_dir() {
        // Recursively check directory contents. Fail-secure: if the directory
        // cannot be enumerated we cannot prove its contents are safe, so error.
        let entries = fs::read_dir(path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Cannot read directory '{}' for symlink check: {}",
                    path.display(),
                    e
                ),
            )
        })?;
        for entry in entries {
            let entry = entry?;
            check_symlink_recursive(&entry.path(), base_canonical, visited)?;
        }
    }

    Ok(())
}

/// Filter out unsafe symlinks from files to be archived
/// Returns (files, excluded_paths) - original files and paths to exclude via tar --exclude
pub fn filter_symlinks_for_tar(base_dir: &Path, files: &[String]) -> (Vec<String>, Vec<String>) {
    use std::collections::HashSet;
    let mut excluded_paths = Vec::new();
    let mut visited = HashSet::new();

    for file in files {
        let file_path = base_dir.join(file);
        collect_unsafe_symlinks(
            &file_path,
            base_dir,
            file,
            &mut excluded_paths,
            &mut visited,
        );
    }

    (files.to_vec(), excluded_paths)
}

/// Recursively collect unsafe symlinks paths for exclusion
fn collect_unsafe_symlinks(
    path: &Path,
    base_dir: &Path,
    relative_path: &str,
    excluded: &mut Vec<String>,
    visited: &mut std::collections::HashSet<std::path::PathBuf>,
) {
    // Detect symlink loops
    if let Ok(canonical_path) = path.canonicalize().map(strip_unc_prefix) {
        if !visited.insert(canonical_path.clone()) {
            return; // Already visited, skip
        }
    }

    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => {
            // Can't access - exclude it
            excluded.push(relative_path.to_string());
            return;
        }
    };

    if metadata.is_symlink() {
        // Check if symlink is safe
        let is_unsafe = match fs::read_link(path) {
            Ok(link_target) => {
                let resolved_target = if link_target.is_absolute() {
                    link_target.clone()
                } else {
                    let parent = path.parent().unwrap_or(base_dir);
                    parent.join(&link_target)
                };

                match resolved_target.canonicalize().map(strip_unc_prefix) {
                    Ok(canonical) => {
                        if let Ok(base_canonical) = base_dir.canonicalize().map(strip_unc_prefix) {
                            !canonical.starts_with(&base_canonical)
                        } else {
                            true // Can't resolve base, unsafe
                        }
                    }
                    Err(_) => true, // Can't resolve (dangling symlink)
                }
            }
            Err(_) => true, // Can't read link
        };

        if is_unsafe {
            excluded.push(relative_path.to_string());
        }
    } else if metadata.is_dir() {
        // Recursively check directory contents. Fail-secure: if the directory
        // cannot be enumerated, exclude the whole directory rather than letting
        // its (unknown) contents into the archive.
        match fs::read_dir(path) {
            Ok(entries) => {
                for entry in entries.filter_map(|e| e.ok()) {
                    let entry_name = entry.file_name().to_string_lossy().to_string();
                    let entry_relative = format!("{}/{}", relative_path, entry_name);
                    collect_unsafe_symlinks(
                        &entry.path(),
                        base_dir,
                        &entry_relative,
                        excluded,
                        visited,
                    );
                }
            }
            Err(_) => {
                excluded.push(relative_path.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{mpsc, Arc};

    /// Counter for unique temp directory names
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Helper to create a temporary directory for testing
    fn create_temp_dir() -> PathBuf {
        let unique_id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let temp_dir = std::env::temp_dir().join(format!(
            "cokacdir_test_{}_{}",
            std::process::id(),
            unique_id
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");
        temp_dir
    }

    /// Helper to cleanup temp directory
    fn cleanup_temp_dir(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    #[cfg(unix)]
    fn create_socket(path: &Path) -> std::os::unix::net::UnixListener {
        std::os::unix::net::UnixListener::bind(path).unwrap()
    }

    #[cfg(unix)]
    fn create_cross_filesystem_test_dirs() -> Option<(tempfile::TempDir, tempfile::TempDir)> {
        let source = tempfile::Builder::new()
            .prefix("cokacdir-cross-source-")
            .tempdir()
            .ok()?;
        let source_namespace = stable_path_identity(source.path()).ok()?.namespace;
        let mut candidates = Vec::new();
        if let Some(candidate) = std::env::var_os("COKACDIR_CROSS_FILESYSTEM_TEST_DIR") {
            candidates.push(PathBuf::from(candidate));
        }
        candidates.push(PathBuf::from("/dev/shm"));
        if let Ok(current) = std::env::current_dir() {
            candidates.push(current);
        }
        for candidate in candidates {
            let Ok(target) = tempfile::Builder::new()
                .prefix("cokacdir-cross-target-")
                .tempdir_in(candidate)
            else {
                continue;
            };
            let Ok(target_identity) = stable_path_identity(target.path()) else {
                continue;
            };
            if target_identity.namespace != source_namespace {
                return Some((source, target));
            }
        }
        None
    }

    #[cfg(unix)]
    #[test]
    fn directory_access_keeps_using_the_open_directory_after_path_replacement() {
        let root = create_temp_dir();
        let original = root.join("original");
        let detached = root.join("detached");
        let replacement = root.join("replacement");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&replacement).unwrap();

        let (_guard, access, _) = open_directory_for_read(&original).unwrap();
        fs::rename(&original, &detached).unwrap();
        std::os::unix::fs::symlink(&replacement, &original).unwrap();

        let name = OsStr::new("created.txt");
        let mut file = access
            .open_file(
                name,
                DirectoryFileOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600),
            )
            .unwrap();
        file.write_all(b"anchored").unwrap();
        file.sync_all().unwrap();
        let identity = stable_file_identity(&file).unwrap();
        drop(file);

        assert_eq!(fs::read(detached.join(name)).unwrap(), b"anchored");
        assert!(!replacement.join(name).exists());
        assert_eq!(access.child_identity(name).unwrap(), identity);
        assert!(access
            .entries()
            .unwrap()
            .any(|entry| entry.unwrap().as_os_str() == name));

        access.remove_file_if_identity(name, identity).unwrap();
        assert!(!detached.join(name).exists());
        drop(access);
        drop(_guard);
        cleanup_temp_dir(&root);
    }

    #[test]
    fn directory_access_rejects_nested_child_paths() {
        let root = create_temp_dir();
        let (_guard, access, _) = open_directory_for_read(&root).unwrap();

        let error = access
            .open_file(
                OsStr::new("nested/file"),
                DirectoryFileOptions::new().read(true),
            )
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        drop(access);
        drop(_guard);
        cleanup_temp_dir(&root);
    }

    fn completed_message(messages: &[ProgressMessage]) -> Option<(usize, usize)> {
        messages.iter().rev().find_map(|message| {
            if let ProgressMessage::Completed(success, failure) = message {
                Some((*success, *failure))
            } else {
                None
            }
        })
    }

    // ========== is_valid_filename tests ==========

    #[test]
    fn test_is_valid_filename_normal() {
        assert!(is_valid_filename("test.txt").is_ok());
        assert!(is_valid_filename("my_file").is_ok());
        assert!(is_valid_filename("file-name.rs").is_ok());
        assert!(is_valid_filename("FILE123").is_ok());
        assert!(is_valid_filename(".hidden").is_ok());
    }

    #[test]
    fn test_is_valid_filename_empty_rejected() {
        assert!(is_valid_filename("").is_err());
        assert!(is_valid_filename("   ").is_err());
    }

    #[test]
    fn test_is_valid_filename_path_separator_rejected() {
        assert!(is_valid_filename("path/file").is_err());
        assert!(is_valid_filename("path\\file").is_err());
        assert!(is_valid_filename("/absolute").is_err());
    }

    #[test]
    fn test_is_valid_filename_null_byte_rejected() {
        assert!(is_valid_filename("file\0name").is_err());
    }

    #[test]
    fn test_is_valid_filename_reserved_names_rejected() {
        assert!(is_valid_filename(".").is_err());
        assert!(is_valid_filename("..").is_err());
    }

    #[test]
    fn test_is_valid_filename_too_long_rejected() {
        let long_name = "a".repeat(256);
        assert!(is_valid_filename(&long_name).is_err());

        let max_name = "a".repeat(255);
        assert!(is_valid_filename(&max_name).is_ok());
    }

    #[test]
    fn test_is_valid_filename_control_chars_rejected() {
        assert!(is_valid_filename("file\nname").is_err());
        assert!(is_valid_filename("file\tname").is_err());
        assert!(is_valid_filename("file\rname").is_err());
    }

    #[test]
    fn test_is_valid_filename_whitespace_rejected() {
        assert!(is_valid_filename(" leading").is_err());
        assert!(is_valid_filename("trailing ").is_err());
        assert!(is_valid_filename(" both ").is_err());
    }

    #[test]
    fn test_is_valid_filename_leading_hyphen_rejected() {
        assert!(is_valid_filename("-option").is_err());
        assert!(is_valid_filename("--long-option").is_err());
    }

    // ========== copy_file tests ==========

    #[test]
    fn test_copy_file_basic() {
        let temp_dir = create_temp_dir();
        let src = temp_dir.join("source.txt");
        let dest = temp_dir.join("dest.txt");

        let mut file = File::create(&src).unwrap();
        writeln!(file, "test content").unwrap();

        let result = copy_file(&src, &dest);
        assert!(result.is_ok());
        assert!(dest.exists());

        let content = fs::read_to_string(&dest).unwrap();
        assert!(content.contains("test content"));

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_copy_file_same_path_rejected() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.join("same.txt");

        File::create(&file_path).unwrap();

        let result = copy_file(&file_path, &file_path);
        assert!(result.is_err());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_copy_file_dest_exists_rejected() {
        let temp_dir = create_temp_dir();
        let src = temp_dir.join("src.txt");
        let dest = temp_dir.join("dest.txt");

        File::create(&src).unwrap();
        File::create(&dest).unwrap();

        let result = copy_file(&src, &dest);
        assert!(result.is_err());
        assert!(result.unwrap_err().kind() == std::io::ErrorKind::AlreadyExists);

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_copy_file_preserves_top_level_dangling_symlink() {
        let temp_dir = create_temp_dir();
        let src = temp_dir.join("source-link");
        let dest = temp_dir.join("dest-link");
        std::os::unix::fs::symlink("missing-target", &src).unwrap();

        copy_file(&src, &dest).unwrap();

        assert!(fs::symlink_metadata(&dest).unwrap().is_symlink());
        assert_eq!(
            fs::read_link(&dest).unwrap(),
            PathBuf::from("missing-target")
        );
        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_regular_copy_api_does_not_follow_source_symlink() {
        let temp_dir = create_temp_dir();
        let target = temp_dir.join("target");
        let src = temp_dir.join("source-link");
        let dest = temp_dir.join("dest");
        fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(&target, &src).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        assert!(copy_file_with_progress(&src, &dest, &cancel, |_, _| {}).is_err());
        assert!(fs::symlink_metadata(&dest).is_err());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_copy_does_not_follow_destination_symlink() {
        let temp_dir = create_temp_dir();
        let src = temp_dir.join("source");
        let target = temp_dir.join("target");
        let dest = temp_dir.join("dest-link");
        fs::write(&src, "new").unwrap();
        fs::write(&target, "original").unwrap();
        std::os::unix::fs::symlink(&target, &dest).unwrap();

        let error = copy_file(&src, &dest).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        assert!(fs::symlink_metadata(&dest).unwrap().is_symlink());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_publish_noreplace_preserves_late_destination() {
        let temp_dir = create_temp_dir();
        let staged = temp_dir.join("staged");
        let dest = temp_dir.join("dest");
        fs::write(&staged, "new").unwrap();
        fs::write(&dest, "racer").unwrap();

        assert!(publish_noreplace(&staged, &dest).is_err());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "racer");
        assert_eq!(fs::read_to_string(&staged).unwrap(), "new");

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_directory_replacement_is_transactional() {
        let temp_dir = create_temp_dir();
        let staged = temp_dir.join("staged");
        let dest = temp_dir.join("dest");
        fs::create_dir(&staged).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(staged.join("new"), "new").unwrap();
        fs::write(dest.join("old"), "old").unwrap();

        install_completed_replacement(&staged, &dest).unwrap();

        assert_eq!(fs::read_to_string(dest.join("new")).unwrap(), "new");
        assert!(!dest.join("old").exists());
        assert!(!staged.exists());
        assert!(!fs::read_dir(&temp_dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("cokacdir_backup")));
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn verified_replacement_refuses_a_destination_changed_during_copy() {
        let temp_dir = create_temp_dir();
        let staged = temp_dir.join("staged");
        let dest = temp_dir.join("dest");
        let retained_original = temp_dir.join("retained-original");
        fs::write(&staged, "new").unwrap();
        fs::write(&dest, "original").unwrap();
        let expected = path_identity(&dest).unwrap();

        fs::rename(&dest, &retained_original).unwrap();
        fs::write(&dest, "racer").unwrap();

        let error =
            install_completed_replacement_if_unchanged(&staged, &dest, expected, None).unwrap_err();

        assert!(error.to_string().contains("Destination changed"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "racer");
        assert_eq!(fs::read_to_string(&staged).unwrap(), "new");
        assert_eq!(fs::read_to_string(&retained_original).unwrap(), "original");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_failed_replacement_restores_original_directory() {
        let temp_dir = create_temp_dir();
        let dest = temp_dir.join("dest");
        fs::create_dir(&dest).unwrap();
        let staged_inside_dest = dest.join("staged");
        fs::write(&staged_inside_dest, "new").unwrap();

        let result = install_completed_replacement(&staged_inside_dest, &dest);

        assert!(result.is_err());
        assert!(dest.is_dir());
        assert_eq!(fs::read_to_string(dest.join("staged")).unwrap(), "new");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_failed_rollback_retains_recovery_backup() {
        let temp_dir = create_temp_dir();
        let dest = temp_dir.join("dest");
        fs::create_dir(&dest).unwrap();
        let staged_inside_dest = dest.join("staged");
        fs::write(&staged_inside_dest, "new").unwrap();

        let result =
            install_completed_replacement_impl(&staged_inside_dest, &dest, None, None, |_| {
                fs::write(&dest, "racer").unwrap();
            });

        let error = result.unwrap_err().to_string();
        assert!(error.contains("original target is preserved"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "racer");
        let recovery = fs::read_dir(&temp_dir)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("cokacdir_backup")
            })
            .expect("recovery backup should remain")
            .path();
        assert_eq!(fs::read_to_string(recovery.join("staged")).unwrap(), "new");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_copy_file_with_progress_dest_exists_rejected() {
        let temp_dir = create_temp_dir();
        let src = temp_dir.join("src.txt");
        let dest = temp_dir.join("dest.txt");

        fs::write(&src, "new").unwrap();
        fs::write(&dest, "original").unwrap();

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let result = copy_file_with_progress(&src, &dest, &cancel_flag, |_, _| {});

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "original");

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_cancelled_staged_copy_preserves_racing_destination() {
        let temp_dir = create_temp_dir();
        let src = temp_dir.join("src.bin");
        let dest = temp_dir.join("dest.bin");
        fs::write(&src, vec![0x5a; COPY_BUFFER_SIZE * 2]).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));

        let result = copy_file_with_progress(&src, &dest, &cancel, |copied, _| {
            if copied > 0 && !dest.exists() {
                fs::write(&dest, "racer").unwrap();
                cancel.store(true, Ordering::SeqCst);
            }
        });

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "racer");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn cancellation_after_the_final_read_does_not_publish() {
        let temp_dir = create_temp_dir();
        let src = temp_dir.join("src.bin");
        let dest = temp_dir.join("dest.bin");
        fs::write(&src, vec![0x5a; COPY_BUFFER_SIZE / 2]).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));

        let result = copy_file_with_progress(&src, &dest, &cancel, |copied, total| {
            if copied == total {
                cancel.store(true, Ordering::SeqCst);
            }
        });

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(fs::symlink_metadata(&dest).is_err());
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_source_growth_during_copy_is_not_published() {
        let temp_dir = create_temp_dir();
        let src = temp_dir.join("src.bin");
        let dest = temp_dir.join("dest.bin");
        fs::write(&src, vec![0x5a; COPY_BUFFER_SIZE * 2]).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut changed = false;

        let result = copy_file_with_progress(&src, &dest, &cancel, |copied, _| {
            if copied > 0 && !changed {
                changed = true;
                let mut source = OpenOptions::new().append(true).open(&src).unwrap();
                source.write_all(b"changed").unwrap();
                source.sync_all().unwrap();
            }
        });

        assert!(result.unwrap_err().to_string().contains("Source changed"));
        assert!(fs::symlink_metadata(&dest).is_err());
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_copy_overwrite_missing_source_preserves_destination() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();

        let src = source_dir.join("missing.txt");
        let dest = target_dir.join("missing.txt");
        fs::write(&dest, "original").unwrap();

        let mut overwrite = HashMap::new();
        overwrite.insert(src, capture_path_authorization(&dest).unwrap());

        let (tx, rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("missing.txt")],
            &source_dir,
            &target_dir,
            overwrite,
            HashSet::new(),
            None,
            HashMap::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<ProgressMessage> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((0, 1)));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "original");

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn overwrite_authorization_rejects_destination_replaced_after_prompt() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        let source = source_dir.join("item");
        let destination = target_dir.join("item");
        let retained = target_dir.join("retained-original");
        fs::write(&source, "new").unwrap();
        fs::write(&destination, "prompted").unwrap();
        let authorization = capture_path_authorization(&destination).unwrap();
        fs::rename(&destination, &retained).unwrap();
        fs::write(&destination, "racer").unwrap();

        let (tx, rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("item")],
            &source_dir,
            &target_dir,
            HashMap::from([(source.clone(), authorization)]),
            HashSet::new(),
            None,
            HashMap::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((0, 1)));
        assert_eq!(fs::read_to_string(&destination).unwrap(), "racer");
        assert_eq!(fs::read_to_string(&retained).unwrap(), "prompted");
        assert_eq!(fs::read_to_string(&source).unwrap(), "new");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn move_authorization_rejects_destination_replaced_after_prompt() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        let source = source_dir.join("item");
        let destination = target_dir.join("item");
        let retained = target_dir.join("retained-original");
        fs::write(&source, "new").unwrap();
        fs::write(&destination, "prompted").unwrap();
        let authorization = capture_path_authorization(&destination).unwrap();
        fs::rename(&destination, &retained).unwrap();
        fs::write(&destination, "racer").unwrap();

        let (tx, rx) = mpsc::channel();
        move_files_with_progress(
            vec![PathBuf::from("item")],
            &source_dir,
            &target_dir,
            HashMap::from([(source.clone(), authorization)]),
            HashSet::new(),
            None,
            HashMap::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((0, 1)));
        assert_eq!(fs::read_to_string(&destination).unwrap(), "racer");
        assert_eq!(fs::read_to_string(&retained).unwrap(), "prompted");
        assert_eq!(fs::read_to_string(&source).unwrap(), "new");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn target_authorization_allows_its_own_multi_item_copy_changes() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(source_dir.join("one"), "1").unwrap();
        fs::write(source_dir.join("two"), "2").unwrap();
        let target_authorization = capture_directory_authorization(&target_dir).unwrap();

        let (tx, rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("one"), PathBuf::from("two")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            Some(target_authorization),
            HashMap::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((2, 0)));
        assert_eq!(fs::read_to_string(target_dir.join("one")).unwrap(), "1");
        assert_eq!(fs::read_to_string(target_dir.join("two")).unwrap(), "2");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn target_authorization_allows_its_own_multi_item_move_changes() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(source_dir.join("one"), "1").unwrap();
        fs::write(source_dir.join("two"), "2").unwrap();
        let target_authorization = capture_directory_authorization(&target_dir).unwrap();

        let (tx, rx) = mpsc::channel();
        move_files_with_progress(
            vec![PathBuf::from("one"), PathBuf::from("two")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            Some(target_authorization),
            HashMap::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((2, 0)));
        assert!(fs::symlink_metadata(source_dir.join("one")).is_err());
        assert!(fs::symlink_metadata(source_dir.join("two")).is_err());
        assert_eq!(fs::read_to_string(target_dir.join("one")).unwrap(), "1");
        assert_eq!(fs::read_to_string(target_dir.join("two")).unwrap(), "2");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn target_authorization_rejects_replaced_directory() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        let retained_target = temp_dir.join("retained-dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(source_dir.join("item"), "new").unwrap();
        let authorization = capture_directory_authorization(&target_dir).unwrap();
        fs::rename(&target_dir, &retained_target).unwrap();
        fs::create_dir(&target_dir).unwrap();

        let (tx, rx) = mpsc::channel();
        move_files_with_progress(
            vec![PathBuf::from("item")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            Some(authorization),
            HashMap::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((0, 1)));
        assert_eq!(fs::read_to_string(source_dir.join("item")).unwrap(), "new");
        assert!(fs::symlink_metadata(target_dir.join("item")).is_err());
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn source_directory_authorization_rejects_replacement_after_copy_selection() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let retained_source = temp_dir.join("retained-src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&target_dir).unwrap();
        let selected = source_dir.join("item");
        fs::write(&selected, "confirmed").unwrap();
        let directory = capture_directory_authorization(&source_dir).unwrap();
        let item = capture_path_authorization(&selected).unwrap();
        fs::rename(&source_dir, &retained_source).unwrap();
        fs::create_dir(&source_dir).unwrap();
        fs::write(source_dir.join("item"), "racer").unwrap();

        let (tx, rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("item")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            None,
            HashMap::from([(selected, item)]),
            Some(directory),
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((0, 1)));
        assert!(fs::symlink_metadata(target_dir.join("item")).is_err());
        assert_eq!(
            fs::read_to_string(source_dir.join("item")).unwrap(),
            "racer"
        );
        assert_eq!(
            fs::read_to_string(retained_source.join("item")).unwrap(),
            "confirmed"
        );
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn source_item_authorization_rejects_replacement_after_copy_selection() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&target_dir).unwrap();
        let selected = source_dir.join("item");
        let retained = source_dir.join("retained");
        fs::write(&selected, "confirmed").unwrap();
        let directory = capture_directory_authorization(&source_dir).unwrap();
        let item = capture_path_authorization(&selected).unwrap();
        fs::rename(&selected, &retained).unwrap();
        fs::write(&selected, "racer").unwrap();

        let (tx, rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("item")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            None,
            HashMap::from([(selected.clone(), item)]),
            Some(directory),
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((0, 1)));
        assert!(fs::symlink_metadata(target_dir.join("item")).is_err());
        assert_eq!(fs::read_to_string(&selected).unwrap(), "racer");
        assert_eq!(fs::read_to_string(&retained).unwrap(), "confirmed");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn authorized_local_workers_fail_closed_when_item_authorization_is_missing() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&target_dir).unwrap();
        let selected = source_dir.join("item");
        fs::write(&selected, "confirmed").unwrap();
        let directory = capture_directory_authorization(&source_dir).unwrap();

        let (copy_tx, copy_rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("item")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            None,
            HashMap::new(),
            Some(directory.clone()),
            Arc::new(AtomicBool::new(false)),
            copy_tx,
        );
        let copy_messages: Vec<_> = copy_rx.try_iter().collect();
        assert_eq!(completed_message(&copy_messages), Some((0, 1)));
        assert!(fs::symlink_metadata(target_dir.join("item")).is_err());

        let (move_tx, move_rx) = mpsc::channel();
        move_files_with_progress(
            vec![PathBuf::from("item")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            None,
            HashMap::new(),
            Some(directory),
            Arc::new(AtomicBool::new(false)),
            move_tx,
        );
        let move_messages: Vec<_> = move_rx.try_iter().collect();
        assert_eq!(completed_message(&move_messages), Some((0, 1)));
        assert_eq!(fs::read_to_string(&selected).unwrap(), "confirmed");
        assert!(fs::symlink_metadata(target_dir.join("item")).is_err());
        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_source_directory_uses_resolved_item_authorization_key() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("real-src");
        let source_alias = temp_dir.join("source-alias");
        let target_dir = temp_dir.join("dest");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&target_dir).unwrap();
        std::os::unix::fs::symlink(&source_dir, &source_alias).unwrap();

        let selected = source_dir.join("item");
        let retained = source_dir.join("retained");
        fs::write(&selected, "confirmed").unwrap();
        let directory = capture_directory_authorization(&source_alias).unwrap();
        let resolved_selected = directory.resolved_path().join("item");
        let item = capture_path_authorization(&source_alias.join("item")).unwrap();
        fs::rename(&selected, &retained).unwrap();
        fs::write(&selected, "racer").unwrap();

        let (tx, rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("item")],
            &source_alias,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            None,
            HashMap::from([(resolved_selected, item)]),
            Some(directory),
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((0, 1)));
        assert!(fs::symlink_metadata(target_dir.join("item")).is_err());
        assert_eq!(fs::read_to_string(&selected).unwrap(), "racer");
        assert_eq!(fs::read_to_string(&retained).unwrap(), "confirmed");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_copy_directory_into_itself_is_rejected_by_progress_copy() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let photos = source_dir.join("photos");
        let target_dir = photos.join("backup");
        fs::create_dir_all(&photos).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(photos.join("a.txt"), "a").unwrap();

        let (tx, rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("photos")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            None,
            HashMap::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<ProgressMessage> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((0, 1)));
        assert!(!target_dir.join("photos").exists());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_overwrite_same_file_via_symlink_preserves_destination() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();

        let dest = target_dir.join("same.txt");
        let link = source_dir.join("same.txt");
        fs::write(&dest, "original").unwrap();
        std::os::unix::fs::symlink(&dest, &link).unwrap();

        let mut overwrite = HashMap::new();
        overwrite.insert(link, capture_path_authorization(&dest).unwrap());

        let (tx, rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("same.txt")],
            &source_dir,
            &target_dir,
            overwrite,
            HashSet::new(),
            None,
            HashMap::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<ProgressMessage> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((0, 1)));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "original");

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_copy_prepare_cancel_reports_single_cancelled_failure() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(source_dir.join("a.txt"), "a").unwrap();
        fs::write(source_dir.join("b.txt"), "b").unwrap();

        let cancel_flag = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            None,
            HashMap::new(),
            None,
            cancel_flag,
            tx,
        );

        let messages: Vec<ProgressMessage> = rx.try_iter().collect();
        assert!(messages
            .iter()
            .any(|msg| { matches!(msg, ProgressMessage::Error(_, err) if err == "Cancelled") }));
        assert!(matches!(
            messages.last(),
            Some(ProgressMessage::Completed(0, 1))
        ));

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_move_prepare_cancel_reports_single_cancelled_failure() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(source_dir.join("a.txt"), "a").unwrap();
        fs::write(source_dir.join("b.txt"), "b").unwrap();

        let cancel_flag = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::channel();
        move_files_with_progress(
            vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            None,
            HashMap::new(),
            None,
            cancel_flag,
            tx,
        );

        let messages: Vec<ProgressMessage> = rx.try_iter().collect();
        assert!(messages
            .iter()
            .any(|msg| { matches!(msg, ProgressMessage::Error(_, err) if err == "Cancelled") }));
        assert!(matches!(
            messages.last(),
            Some(ProgressMessage::Completed(0, 1))
        ));

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn same_filesystem_directory_overwrite_moves_via_verified_staging() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        let source = source_dir.join("item");
        let destination = target_dir.join("item");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("new"), "new").unwrap();
        fs::write(destination.join("old"), "old").unwrap();

        let (tx, rx) = mpsc::channel();
        move_files_with_progress(
            vec![PathBuf::from("item")],
            &source_dir,
            &target_dir,
            HashMap::from([(
                source.clone(),
                capture_path_authorization(&destination).unwrap(),
            )]),
            HashSet::new(),
            None,
            HashMap::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((1, 0)));
        assert!(fs::symlink_metadata(&source).is_err());
        assert_eq!(fs::read_to_string(destination.join("new")).unwrap(), "new");
        assert!(fs::symlink_metadata(destination.join("old")).is_err());
        assert!(fs::read_dir(&target_dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cokacdir-move-stage")
        }));
        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn cross_filesystem_directory_move_preserves_tree_and_removes_source() {
        let Some((source_root, target_root)) = create_cross_filesystem_test_dirs() else {
            return;
        };
        let source_dir = source_root.path().join("source");
        let target_dir = target_root.path().join("target");
        let source = source_dir.join("item");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir_all(source.join("empty")).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(source.join("root.txt"), "root").unwrap();
        fs::write(source.join("nested/child.txt"), "child").unwrap();

        let (tx, rx) = mpsc::channel();
        move_files_with_progress(
            vec![PathBuf::from("item")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            None,
            HashMap::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            tx,
        );

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(completed_message(&messages), Some((1, 0)), "{messages:?}");
        assert!(!source.exists());
        let destination = target_dir.join("item");
        assert_eq!(
            fs::read_to_string(destination.join("root.txt")).unwrap(),
            "root"
        );
        assert_eq!(
            fs::read_to_string(destination.join("nested/child.txt")).unwrap(),
            "child"
        );
        assert!(destination.join("empty").is_dir());
    }

    #[test]
    fn directory_tree_snapshot_detects_same_length_descendant_rewrite() {
        let temp_dir = create_temp_dir();
        let source = temp_dir.join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        let child = source.join("nested/data");
        fs::write(&child, b"original").unwrap();
        let identity = path_identity(&source).unwrap();
        let before = sha256_directory_tree_snapshot(&source, &identity).unwrap();

        fs::write(&child, b"modified").unwrap();

        let after = sha256_directory_tree_snapshot(&source, &identity).unwrap();
        assert_ne!(before, after);
        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn unstructured_cross_filesystem_directory_move_uses_verified_copy() {
        let Some((source_root, target_root)) = create_cross_filesystem_test_dirs() else {
            return;
        };
        let source = source_root.path().join("item");
        let destination = target_root.path().join("item");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/data"), "verified").unwrap();

        move_file(&source, &destination).unwrap();

        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(destination.join("nested/data")).unwrap(),
            "verified"
        );
    }

    #[cfg(unix)]
    #[test]
    fn same_filesystem_file_overwrite_preserves_the_moved_inode() {
        let temp_dir = create_temp_dir();
        let source = temp_dir.join("source");
        let alias = temp_dir.join("source-alias");
        let destination = temp_dir.join("destination");
        fs::write(&source, "new").unwrap();
        fs::hard_link(&source, &alias).unwrap();
        fs::write(&destination, "old").unwrap();
        let source_identity = path_identity(&source).unwrap();
        let destination_identity = path_identity(&destination).unwrap();

        move_entry_overwrite_same_filesystem(
            &source,
            &destination,
            &source_identity,
            destination_identity,
        )
        .unwrap();

        assert!(fs::symlink_metadata(&source).is_err());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "new");
        assert_eq!(
            stable_path_identity(&destination).unwrap(),
            stable_path_identity(&alias).unwrap()
        );
        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn cross_filesystem_move_rejects_same_inode_change_in_the_isolation_window() {
        let temp_dir = create_temp_dir();
        let source = temp_dir.join("source");
        fs::write(&source, b"original").unwrap();
        let copied_sha256: [u8; 32] = Sha256::digest(b"original").into();
        let expected = path_identity(&source).unwrap();
        let original_metadata = fs::metadata(&source).unwrap();

        let isolated = QuarantinedSource::prepare_impl(&source, expected, |path| {
            // Same inode, same length, restored mtime: relocation necessarily
            // changes ctime too, so metadata alone cannot distinguish this
            // write from the quarantine rename.
            fs::write(path, b"modified").unwrap();
            filetime::set_file_times(
                path,
                FileTime::from_last_access_time(&original_metadata),
                FileTime::from_last_modification_time(&original_metadata),
            )
            .unwrap();
        })
        .unwrap();

        let error = verify_isolated_source_matches_copy(&isolated, copied_sha256).unwrap_err();
        assert!(error.to_string().contains("Source bytes changed"));
        let restored = isolated.restore(error);
        assert!(restored.to_string().contains("source was restored"));
        assert_eq!(fs::read(&source).unwrap(), b"modified");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn failed_directory_overwrite_restores_staged_source_without_clobbering() {
        let temp_dir = create_temp_dir();
        let source = temp_dir.join("source");
        let destination = temp_dir.join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("new"), "new").unwrap();
        fs::write(destination.join("old"), "old").unwrap();
        let expected = path_identity(&source).unwrap();

        let error = move_entry_overwrite_same_filesystem_impl(
            &source,
            &destination,
            &expected,
            |_, _, _| Err(io::Error::other("injected replacement failure")),
        )
        .unwrap_err();

        assert!(error.to_string().contains("original source was restored"));
        assert_eq!(fs::read_to_string(source.join("new")).unwrap(), "new");
        assert_eq!(fs::read_to_string(destination.join("old")).unwrap(), "old");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn failed_directory_overwrite_preserves_staging_when_source_is_reoccupied() {
        let temp_dir = create_temp_dir();
        let source = temp_dir.join("source");
        let destination = temp_dir.join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("new"), "new").unwrap();
        fs::write(destination.join("old"), "old").unwrap();
        let expected = path_identity(&source).unwrap();

        let error = move_entry_overwrite_same_filesystem_impl(
            &source,
            &destination,
            &expected,
            |_, _, _| {
                fs::create_dir(&source)?;
                fs::write(source.join("racer"), "racer")?;
                Err(io::Error::other("injected replacement failure"))
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("recovery path"));
        assert_eq!(fs::read_to_string(source.join("racer")).unwrap(), "racer");
        assert_eq!(fs::read_to_string(destination.join("old")).unwrap(), "old");
        let recovery = fs::read_dir(&temp_dir)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cokacdir-move-stage")
            })
            .expect("staged source must remain available for recovery")
            .path();
        assert_eq!(
            fs::read_to_string(recovery.join("payload/new")).unwrap(),
            "new"
        );
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn failed_directory_overwrite_never_restores_a_replaced_staging_path() {
        let temp_dir = create_temp_dir();
        let source = temp_dir.join("source");
        let destination = temp_dir.join("destination");
        let retained_verified_source = temp_dir.join("verified-source-recovery");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("new"), "new").unwrap();
        fs::write(destination.join("old"), "old").unwrap();
        let expected = path_identity(&source).unwrap();

        let error = move_entry_overwrite_same_filesystem_impl(
            &source,
            &destination,
            &expected,
            |staging, _, _| {
                fs::rename(staging, &retained_verified_source)?;
                fs::create_dir(staging)?;
                fs::write(staging.join("unowned"), "unowned")?;
                Err(io::Error::other("injected replacement failure"))
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("no longer the verified source"));
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(retained_verified_source.join("new")).unwrap(),
            "new"
        );
        assert_eq!(fs::read_to_string(destination.join("old")).unwrap(), "old");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_dir_recursive_with_progress_propagates_child_file_error() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let dest_dir = temp_dir.join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        let _listener = create_socket(&source_dir.join("socket"));

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let mut completed_bytes = 0;
        let mut completed_files = 0;

        let result = copy_dir_recursive_with_progress(
            &source_dir,
            &dest_dir,
            &cancel_flag,
            &tx,
            &mut completed_bytes,
            &mut completed_files,
            0,
            1,
        );

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);

        let messages: Vec<ProgressMessage> = rx.try_iter().collect();
        assert!(messages.iter().any(|msg| {
            matches!(msg, ProgressMessage::Error(_, err) if err.contains("Cannot copy special file"))
        }));

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_files_with_progress_counts_child_file_error_as_directory_failure() {
        let temp_dir = create_temp_dir();
        let source_dir = temp_dir.join("src");
        let target_dir = temp_dir.join("dest");
        let nested_dir = source_dir.join("folder");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        let _listener = create_socket(&nested_dir.join("socket"));

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        copy_files_with_progress(
            vec![PathBuf::from("folder")],
            &source_dir,
            &target_dir,
            HashMap::new(),
            HashSet::new(),
            None,
            HashMap::new(),
            None,
            cancel_flag,
            tx,
        );

        let messages: Vec<ProgressMessage> = rx.try_iter().collect();
        assert!(messages.iter().any(|msg| {
            matches!(msg, ProgressMessage::Error(_, err) if err.contains("Cannot copy special file"))
        }));
        assert!(matches!(
            messages.last(),
            Some(ProgressMessage::Completed(0, 1))
        ));

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_copy_dir_recursive() {
        let temp_dir = create_temp_dir();
        let src_dir = temp_dir.join("src_dir");
        let dest_dir = temp_dir.join("dest_dir");

        fs::create_dir_all(src_dir.join("subdir")).unwrap();
        File::create(src_dir.join("file1.txt")).unwrap();
        File::create(src_dir.join("subdir/file2.txt")).unwrap();

        let result = copy_file(&src_dir, &dest_dir);
        assert!(result.is_ok());
        assert!(dest_dir.exists());
        assert!(dest_dir.join("file1.txt").exists());
        assert!(dest_dir.join("subdir/file2.txt").exists());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_loop_detection() {
        let temp_dir = create_temp_dir();
        let dir_a = temp_dir.join("dir_a");
        let dir_b = temp_dir.join("dir_b");
        let dest = temp_dir.join("dest");

        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        // Create symlink from dir_a/link -> dir_b
        std::os::unix::fs::symlink(&dir_b, dir_a.join("link_to_b")).unwrap();
        // Create symlink from dir_b/link -> dir_a (circular)
        std::os::unix::fs::symlink(&dir_a, dir_b.join("link_to_a")).unwrap();

        // This should detect the circular symlink
        let result = copy_file(&dir_a, &dest);
        // The copy should succeed since we don't follow symlinks into loops
        // (symlinks are copied as symlinks, not followed)
        assert!(result.is_ok());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_sensitive_path_symlink_rejected() {
        let temp_dir = create_temp_dir();
        let src_dir = temp_dir.join("src_dir");
        let dest_dir = temp_dir.join("dest_dir");

        fs::create_dir_all(&src_dir).unwrap();

        // Create symlink pointing to /etc (sensitive path)
        std::os::unix::fs::symlink("/etc", src_dir.join("sensitive_link")).unwrap();

        let result = copy_file(&src_dir, &dest_dir);
        assert!(result.is_err());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_sensitive_path_symlink_via_existing_prefix_is_rejected() {
        let temp_dir = create_temp_dir();
        let source = temp_dir.join("source-link");
        let physical_destination = temp_dir.join("copied-link");
        let logical_destination = temp_dir.join("new/a/link");
        let external = temp_dir.join("external");
        fs::create_dir(&external).unwrap();
        std::os::unix::fs::symlink("/etc", external.join("alias")).unwrap();
        std::os::unix::fs::symlink("../../external/alias/passwd", &source).unwrap();

        let error = copy_symlink_to_new(&source, &physical_destination, &logical_destination, None)
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(fs::symlink_metadata(&physical_destination).is_err());
        cleanup_temp_dir(&temp_dir);
    }

    // ========== move_file tests ==========

    #[test]
    fn test_move_file_basic() {
        let temp_dir = create_temp_dir();
        let src = temp_dir.join("move_src.txt");
        let dest = temp_dir.join("move_dest.txt");

        let mut file = File::create(&src).unwrap();
        writeln!(file, "move content").unwrap();
        drop(file);

        let result = move_file(&src, &dest);
        assert!(result.is_ok());
        assert!(!src.exists());
        assert!(dest.exists());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_move_file_same_path_rejected() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.join("same_move.txt");

        File::create(&file_path).unwrap();

        let result = move_file(&file_path, &file_path);
        assert!(result.is_err());

        cleanup_temp_dir(&temp_dir);
    }

    // ========== delete_file tests ==========

    #[test]
    fn test_delete_file_basic() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.join("delete_me.txt");

        File::create(&file_path).unwrap();
        assert!(file_path.exists());

        let result = delete_file(&file_path);
        assert!(result.is_ok());
        assert!(!file_path.exists());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn authorized_delete_rejects_an_item_replaced_after_confirmation() {
        let temp_dir = create_temp_dir();
        let selected = temp_dir.join("selected");
        let retained = temp_dir.join("retained");
        fs::write(&selected, "confirmed").unwrap();
        let authorization = capture_path_authorization(&selected).unwrap();
        fs::rename(&selected, &retained).unwrap();
        fs::write(&selected, "racer").unwrap();

        let error = delete_file_detailed_authorized(&selected, &authorization).unwrap_err();

        assert!(error.to_string().contains("changed"));
        assert_eq!(fs::read_to_string(&selected).unwrap(), "racer");
        assert_eq!(fs::read_to_string(&retained).unwrap(), "confirmed");
        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_delete_directory() {
        let temp_dir = create_temp_dir();
        let dir_path = temp_dir.join("delete_dir");

        fs::create_dir_all(dir_path.join("subdir")).unwrap();
        File::create(dir_path.join("file.txt")).unwrap();

        let result = delete_file(&dir_path);
        assert!(result.is_ok());
        assert!(!dir_path.exists());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_delete_symlink() {
        let temp_dir = create_temp_dir();
        let target = temp_dir.join("target.txt");
        let link = temp_dir.join("link");

        File::create(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Delete symlink should not delete target
        let result = delete_file(&link);
        assert!(result.is_ok());
        assert!(!link.exists());
        assert!(target.exists()); // Target should still exist

        cleanup_temp_dir(&temp_dir);
    }

    // ========== create_directory tests ==========

    #[test]
    fn test_create_directory_basic() {
        let temp_dir = create_temp_dir();
        let new_dir = temp_dir.join("new_dir");

        let result = create_directory(&new_dir);
        assert!(result.is_ok());
        assert!(new_dir.is_dir());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_create_directory_nested() {
        let temp_dir = create_temp_dir();
        let nested_dir = temp_dir.join("a/b/c/d");

        let result = create_directory(&nested_dir);
        assert!(result.is_ok());
        assert!(nested_dir.is_dir());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_create_directory_exists_rejected() {
        let temp_dir = create_temp_dir();
        let dir_path = temp_dir.join("existing_dir");

        fs::create_dir(&dir_path).unwrap();

        let result = create_directory(&dir_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().kind() == std::io::ErrorKind::AlreadyExists);

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_create_directory_rejects_dangling_symlink() {
        let temp_dir = create_temp_dir();
        let path = temp_dir.join("directory");
        std::os::unix::fs::symlink("missing", &path).unwrap();

        assert_eq!(
            create_directory(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(fs::symlink_metadata(&path).unwrap().is_symlink());

        cleanup_temp_dir(&temp_dir);
    }

    // ========== rename_file tests ==========

    #[test]
    fn test_rename_file_basic() {
        let temp_dir = create_temp_dir();
        let old_path = temp_dir.join("old_name.txt");
        let new_path = temp_dir.join("new_name.txt");

        File::create(&old_path).unwrap();

        let result = rename_file(&old_path, &new_path);
        assert!(result.is_ok());
        assert!(!old_path.exists());
        assert!(new_path.exists());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_rename_file_dest_exists_rejected() {
        let temp_dir = create_temp_dir();
        let old_path = temp_dir.join("old.txt");
        let new_path = temp_dir.join("new.txt");

        File::create(&old_path).unwrap();
        File::create(&new_path).unwrap();

        let result = rename_file(&old_path, &new_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().kind() == std::io::ErrorKind::AlreadyExists);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn authorized_rename_rejects_an_item_replaced_after_dialog_open() {
        let temp_dir = create_temp_dir();
        let old_path = temp_dir.join("old.txt");
        let retained = temp_dir.join("retained.txt");
        let new_path = temp_dir.join("new.txt");
        fs::write(&old_path, "confirmed").unwrap();
        let directory = capture_directory_authorization(&temp_dir).unwrap();
        let source = capture_path_authorization(&old_path).unwrap();
        fs::rename(&old_path, &retained).unwrap();
        fs::write(&old_path, "racer").unwrap();

        let error = rename_file_authorized(&old_path, &new_path, &source, &directory).unwrap_err();

        assert!(error.to_string().contains("changed"));
        assert_eq!(fs::read_to_string(&old_path).unwrap(), "racer");
        assert_eq!(fs::read_to_string(&retained).unwrap(), "confirmed");
        assert!(fs::symlink_metadata(&new_path).is_err());
        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn copy_rejects_a_hardlink_destination_to_the_source() {
        let temp_dir = create_temp_dir();
        let source = temp_dir.join("source");
        let destination = temp_dir.join("destination");
        fs::write(&source, "content").unwrap();
        fs::hard_link(&source, &destination).unwrap();

        let error = copy_file(&source, &destination).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read_to_string(&source).unwrap(), "content");
        assert_eq!(fs::read_to_string(&destination).unwrap(), "content");
        cleanup_temp_dir(&temp_dir);
    }

    // ========== check_symlinks_for_tar tests ==========

    #[test]
    fn test_check_symlinks_for_tar_regular_files() {
        let temp_dir = create_temp_dir();

        File::create(temp_dir.join("file1.txt")).unwrap();
        File::create(temp_dir.join("file2.txt")).unwrap();

        let files = vec!["file1.txt".to_string(), "file2.txt".to_string()];
        let result = check_symlinks_for_tar(&temp_dir, &files);
        assert!(result.is_ok());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_symlinks_for_tar_safe_symlink() {
        let temp_dir = create_temp_dir();

        // Create a file and a symlink pointing to it (safe - within the directory)
        let target = temp_dir.join("target.txt");
        File::create(&target).unwrap();
        std::os::unix::fs::symlink("target.txt", temp_dir.join("link")).unwrap();

        let files = vec!["link".to_string()];
        let result = check_symlinks_for_tar(&temp_dir, &files);
        assert!(result.is_ok());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_symlinks_for_tar_sensitive_symlink_rejected() {
        let temp_dir = create_temp_dir();

        // Create a symlink pointing to /etc (sensitive path)
        std::os::unix::fs::symlink("/etc/passwd", temp_dir.join("sensitive_link")).unwrap();

        let files = vec!["sensitive_link".to_string()];
        let result = check_symlinks_for_tar(&temp_dir, &files);
        assert!(result.is_err());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_symlinks_for_tar_nested_symlink() {
        let temp_dir = create_temp_dir();

        // Create a subdirectory with a file and a safe symlink
        fs::create_dir_all(temp_dir.join("subdir")).unwrap();
        File::create(temp_dir.join("subdir/file.txt")).unwrap();
        std::os::unix::fs::symlink("file.txt", temp_dir.join("subdir/link")).unwrap();

        let files = vec!["subdir".to_string()];
        let result = check_symlinks_for_tar(&temp_dir, &files);
        assert!(result.is_ok());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_symlinks_for_tar_nested_sensitive_rejected() {
        let temp_dir = create_temp_dir();

        // Create a subdirectory with a sensitive symlink inside
        fs::create_dir_all(temp_dir.join("subdir")).unwrap();
        std::os::unix::fs::symlink("/etc", temp_dir.join("subdir/etc_link")).unwrap();

        let files = vec!["subdir".to_string()];
        let result = check_symlinks_for_tar(&temp_dir, &files);
        assert!(result.is_err());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_symlinks_for_tar_dangling_symlink_rejected() {
        let temp_dir = create_temp_dir();

        // Create a symlink pointing to non-existent path
        std::os::unix::fs::symlink("/nonexistent/path/file", temp_dir.join("dangling")).unwrap();

        let files = vec!["dangling".to_string()];
        let result = check_symlinks_for_tar(&temp_dir, &files);
        assert!(result.is_err());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_symlinks_for_tar_outside_basedir_rejected() {
        let temp_dir = create_temp_dir();

        // Create a symlink pointing outside base_dir (to /usr which is not sensitive but outside)
        std::os::unix::fs::symlink("/usr", temp_dir.join("usr_link")).unwrap();

        let files = vec!["usr_link".to_string()];
        let result = check_symlinks_for_tar(&temp_dir, &files);
        assert!(result.is_err());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_symlinks_for_tar_home_symlink_rejected() {
        let temp_dir = create_temp_dir();

        // Create a symlink pointing to /home (now in SENSITIVE_PATHS)
        std::os::unix::fs::symlink("/home", temp_dir.join("home_link")).unwrap();

        let files = vec!["home_link".to_string()];
        let result = check_symlinks_for_tar(&temp_dir, &files);
        assert!(result.is_err());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_symlinks_for_tar_dev_symlink_rejected() {
        let temp_dir = create_temp_dir();

        // Create a symlink pointing to /dev (now in SENSITIVE_PATHS)
        std::os::unix::fs::symlink("/dev/null", temp_dir.join("dev_link")).unwrap();

        let files = vec!["dev_link".to_string()];
        let result = check_symlinks_for_tar(&temp_dir, &files);
        assert!(result.is_err());

        cleanup_temp_dir(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_symlinks_for_tar_relative_escape_rejected() {
        let temp_dir = create_temp_dir();

        // Create a symlink using relative path to escape base_dir
        std::os::unix::fs::symlink("../../etc/passwd", temp_dir.join("relative_escape")).unwrap();

        let files = vec!["relative_escape".to_string()];
        let result = check_symlinks_for_tar(&temp_dir, &files);
        assert!(result.is_err());

        cleanup_temp_dir(&temp_dir);
    }
}
