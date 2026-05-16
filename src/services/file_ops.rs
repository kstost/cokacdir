use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use filetime::{self, FileTime};

use crate::utils::format::strip_unc_prefix;

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
#[allow(dead_code)]  // Fields are used for debugging/logging, not always read
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
}

/// File operation result
#[derive(Debug, Clone)]
pub struct FileOperationResult {
    pub success_count: usize,
    pub failure_count: usize,
    pub last_error: Option<String>,
}

/// Buffer size for file copy (64KB)
const COPY_BUFFER_SIZE: usize = 64 * 1024;

/// Try to clone file using APFS clonefile (macOS only)
/// Returns Ok(true) if clone succeeded, Ok(false) if should fallback to regular copy
#[cfg(target_os = "macos")]
fn try_clonefile(src: &Path, dest: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    extern "C" {
        fn clonefile(
            src: *const libc::c_char,
            dst: *const libc::c_char,
            flags: libc::c_int,
        ) -> libc::c_int;
    }

    let src_cstr = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid source path"))?;
    let dest_cstr = CString::new(dest.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid destination path"))?;

    let result = unsafe { clonefile(src_cstr.as_ptr(), dest_cstr.as_ptr(), 0) };

    if result == 0 {
        Ok(true) // Clone succeeded
    } else {
        let err = io::Error::last_os_error();
        // ENOTSUP (45) or EXDEV (18) means clonefile not supported - fallback to regular copy
        // Other errors should also fallback gracefully
        match err.raw_os_error() {
            Some(libc::ENOTSUP) | Some(libc::EXDEV) | Some(libc::EACCES) => Ok(false),
            _ => Ok(false), // Fallback for any other error
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn try_clonefile(_src: &Path, _dest: &Path) -> io::Result<bool> {
    Ok(false) // Not supported on non-macOS
}

/// Calculate total size of files to be copied/moved
pub fn calculate_total_size(files: &[PathBuf], cancel_flag: &Arc<AtomicBool>) -> io::Result<(u64, usize)> {
    let mut total_size: u64 = 0;
    let mut total_files: usize = 0;

    for path in files {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
        }

        if path.is_dir() {
            let (dir_size, dir_files) = calculate_dir_size(path, cancel_flag)?;
            total_size += dir_size;
            total_files += dir_files;
        } else if path.is_file() {
            total_size += fs::metadata(path)?.len();
            total_files += 1;
        }
    }

    Ok((total_size, total_files))
}

/// Calculate total size and file count of a directory
fn calculate_dir_size(path: &Path, cancel_flag: &Arc<AtomicBool>) -> io::Result<(u64, usize)> {
    let mut total_size: u64 = 0;
    let mut total_files: usize = 0;

    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.filter_map(|e| e.ok()) {
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
            }

            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path)?;

            if metadata.is_symlink() {
                // Symlinks count as 0 size
                total_files += 1;
            } else if metadata.is_dir() {
                let (sub_size, sub_files) = calculate_dir_size(&entry_path, cancel_flag)?;
                total_size += sub_size;
                total_files += sub_files;
            } else {
                total_size += metadata.len();
                total_files += 1;
            }
        }
    }

    Ok((total_size, total_files))
}

/// Copy a single file with progress callback
/// On macOS with APFS, tries clonefile first for instant copy
pub fn copy_file_with_progress<F>(
    src: &Path,
    dest: &Path,
    cancel_flag: &Arc<AtomicBool>,
    mut progress_callback: F,
) -> io::Result<u64>
where
    F: FnMut(u64, u64),
{
    let metadata = fs::metadata(src)?;
    let total_size = metadata.len();

    // Check for special files (device files, sockets, etc.) that cannot be copied
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let file_type = metadata.file_type();
        if file_type.is_block_device()
            || file_type.is_char_device()
            || file_type.is_fifo()
            || file_type.is_socket()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot copy special file (device, socket, or pipe)",
            ));
        }
    }

    // Check for cancellation before starting
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
    }

    // Try APFS clonefile first (macOS only)
    if try_clonefile(src, dest)? {
        // Clone succeeded - report 100% progress immediately
        progress_callback(total_size, total_size);
        return Ok(total_size);
    }

    // Fallback to regular copy with progress
    let mut src_file = File::open(src)?;
    let mut dest_file = File::create(dest)?;

    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    let mut copied: u64 = 0;

    loop {
        // Check for cancellation
        if cancel_flag.load(Ordering::Relaxed) {
            // Clean up incomplete file
            drop(dest_file);
            let _ = fs::remove_file(dest);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
        }

        let bytes_read = src_file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        dest_file.write_all(&buffer[..bytes_read])?;
        copied += bytes_read as u64;

        // Report progress
        progress_callback(copied, total_size);
    }

    // Preserve permissions
    #[cfg(unix)]
    {
        fs::set_permissions(dest, metadata.permissions())?;
    }

    // Preserve timestamps (mtime, atime)
    let _ = preserve_timestamps(dest, &metadata);

    Ok(copied)
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
    let mut visited = HashSet::new();
    copy_dir_recursive_with_progress_inner(
        src,
        dest,
        cancel_flag,
        progress_tx,
        completed_bytes,
        completed_files,
        total_bytes,
        total_files,
        &mut visited,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn copy_dir_recursive_with_progress_inner(
    src: &Path,
    dest: &Path,
    cancel_flag: &Arc<AtomicBool>,
    progress_tx: &Sender<ProgressMessage>,
    completed_bytes: &mut u64,
    completed_files: &mut usize,
    total_bytes: u64,
    total_files: usize,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) -> io::Result<()> {
    // Guard against pathological recursion (e.g. circular symlinks).
    if depth > MAX_COPY_DEPTH {
        return Err(io::Error::other(
            format!("Maximum directory depth ({}) exceeded - possible circular symlink", MAX_COPY_DEPTH),
        ));
    }

    // Detect symlink loops via canonicalised path. `visited` tracks only
    // the *current recursion stack* (DFS path), not every directory ever
    // seen — otherwise two siblings whose symlinks resolve to the same
    // physical directory would be falsely reported as a cycle. We insert
    // here and remove on exit (after the body, regardless of error).
    let canonical_src = src.canonicalize().map(strip_unc_prefix).unwrap_or_else(|_| src.to_path_buf());
    if visited.contains(&canonical_src) {
        return Err(io::Error::other(
            format!("Circular symlink detected: {}", src.display()),
        ));
    }
    visited.insert(canonical_src.clone());

    // Run the body inside an IIFE so any early return (`?`, explicit
    // `return Err`) still goes through the `visited.remove` cleanup
    // below — that's what enforces stack-only semantics.
    let result: io::Result<()> = (|| {
        // Check for cancellation
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
        }

        fs::create_dir_all(dest)?;

        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dest_path = dest.join(entry.file_name());

            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
            }

            let metadata = fs::symlink_metadata(&src_path)?;

            if metadata.is_symlink() {
                // Reject symlinks pointing to sensitive system paths
                #[cfg(unix)]
                if let Ok(resolved) = src_path.canonicalize().map(strip_unc_prefix) {
                    let resolved_str = resolved.to_string_lossy();
                    if target_is_sensitive(&resolved_str) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!(
                                "Symlink '{}' points to sensitive system path: {}",
                                src_path.display(),
                                resolved_str
                            ),
                        ));
                    }
                }
                // Copy symlink as-is
                #[cfg(unix)]
                {
                    let link_target = fs::read_link(&src_path)?;
                    std::os::unix::fs::symlink(&link_target, &dest_path)?;
                }
                #[cfg(not(unix))]
                {
                    if src_path.is_file() {
                        fs::copy(&src_path, &dest_path)?;
                        if let Ok(target_meta) = fs::metadata(&src_path) {
                            let _ = preserve_timestamps(&dest_path, &target_meta);
                        }
                    }
                }

                *completed_files += 1;
                let _ = progress_tx.send(ProgressMessage::TotalProgress(
                    *completed_files,
                    total_files,
                    *completed_bytes,
                    total_bytes,
                ));
            } else if metadata.is_dir() {
                copy_dir_recursive_with_progress_inner(
                    &src_path,
                    &dest_path,
                    cancel_flag,
                    progress_tx,
                    completed_bytes,
                    completed_files,
                    total_bytes,
                    total_files,
                    visited,
                    depth + 1,
                )?;
            } else {
                // Regular file - copy with progress
                let filename = src_path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();

                let _ = progress_tx.send(ProgressMessage::FileStarted(filename.clone()));

                let file_size = metadata.len();
                let file_completed_bytes = *completed_bytes;

                let result = copy_file_with_progress(
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
                        let _ = progress_tx.send(ProgressMessage::Error(filename, e.to_string()));
                    }
                }
            }
        }

        // Preserve directory timestamps (must be done after all contents are copied)
        if let Ok(src_metadata) = fs::metadata(src) {
            let _ = preserve_timestamps(dest, &src_metadata);
        }

        Ok(())
    })();

    visited.remove(&canonical_src);
    result
}

/// Copy files with progress reporting (main entry point for progress-enabled copy)
/// files_to_overwrite: Set of source paths that should overwrite existing destinations
/// files_to_skip: Set of source paths that should be skipped if destination exists
pub fn copy_files_with_progress(
    files: Vec<PathBuf>,
    source_dir: &Path,
    target_dir: &Path,
    files_to_overwrite: HashSet<PathBuf>,
    files_to_skip: HashSet<PathBuf>,
    cancel_flag: Arc<AtomicBool>,
    progress_tx: Sender<ProgressMessage>,
) {
    let mut success_count = 0;
    let mut failure_count = 0;

    // Build full paths for size calculation (excluding skipped files)
    let full_paths: Vec<PathBuf> = files.iter()
        .map(|f| if f.is_absolute() { f.clone() } else { source_dir.join(f) })
        .filter(|p| !files_to_skip.contains(p))
        .collect();

    // Send preparing message before calculating sizes
    let _ = progress_tx.send(ProgressMessage::Preparing("Calculating file sizes...".to_string()));

    // Calculate total size
    let (total_bytes, total_files) = match calculate_total_size(&full_paths, &cancel_flag) {
        Ok((size, count)) => (size, count),
        Err(e) => {
            let _ = progress_tx.send(ProgressMessage::Error("".to_string(), e.to_string()));
            let _ = progress_tx.send(ProgressMessage::Completed(0, files.len()));
            return;
        }
    };

    // Send prepare complete
    let _ = progress_tx.send(ProgressMessage::PrepareComplete);

    let mut completed_bytes: u64 = 0;
    let mut completed_files: usize = 0;

    for file_path in &files {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }

        let src = if file_path.is_absolute() {
            file_path.clone()
        } else {
            source_dir.join(file_path)
        };

        let filename = src.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let dest = target_dir.join(&filename);

        // Check if this file should be skipped
        if files_to_skip.contains(&src) {
            continue;
        }

        // Check if destination already exists
        if dest.exists() {
            if files_to_overwrite.contains(&src) {
                // Delete existing file/directory before copying
                if let Err(e) = delete_file(&dest) {
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(
                        filename,
                        format!("Failed to remove existing: {}", e),
                    ));
                    continue;
                }
            } else {
                // Not in overwrite set and not in skip set - unexpected conflict
                failure_count += 1;
                let _ = progress_tx.send(ProgressMessage::Error(
                    filename,
                    "Target already exists".to_string(),
                ));
                continue;
            }
        }

        let _ = progress_tx.send(ProgressMessage::FileStarted(filename.clone()));

        if src.is_dir() {
            match copy_dir_recursive_with_progress(
                &src,
                &dest,
                &cancel_flag,
                &progress_tx,
                &mut completed_bytes,
                &mut completed_files,
                total_bytes,
                total_files,
            ) {
                Ok(_) => {
                    success_count += 1;
                    let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        // Cancelled - clean up partial copy
                        let _ = fs::remove_dir_all(&dest);
                        break;
                    }
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, e.to_string()));
                }
            }
        } else {
            let file_size = fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
            let file_completed_bytes = completed_bytes;

            match copy_file_with_progress(
                &src,
                &dest,
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
                Ok(_) => {
                    completed_bytes += file_size;
                    completed_files += 1;
                    success_count += 1;
                    let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        break;
                    }
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, e.to_string()));
                }
            }
        }
    }

    let _ = progress_tx.send(ProgressMessage::Completed(success_count, failure_count));
}

/// Move files with progress reporting
/// files_to_overwrite: Set of source paths that should overwrite existing destinations
/// files_to_skip: Set of source paths that should be skipped if destination exists
pub fn move_files_with_progress(
    files: Vec<PathBuf>,
    source_dir: &Path,
    target_dir: &Path,
    files_to_overwrite: HashSet<PathBuf>,
    files_to_skip: HashSet<PathBuf>,
    cancel_flag: Arc<AtomicBool>,
    progress_tx: Sender<ProgressMessage>,
) {
    let mut success_count = 0;
    let mut failure_count = 0;

    // Build full paths for size calculation (excluding skipped files)
    let full_paths: Vec<PathBuf> = files.iter()
        .map(|f| if f.is_absolute() { f.clone() } else { source_dir.join(f) })
        .filter(|p| !files_to_skip.contains(p))
        .collect();

    // Send preparing message before calculating sizes
    let _ = progress_tx.send(ProgressMessage::Preparing("Calculating file sizes...".to_string()));

    // Calculate total size upfront for accurate progress
    let (total_bytes, total_files) = match calculate_total_size(&full_paths, &cancel_flag) {
        Ok((size, count)) => (size, count),
        Err(e) => {
            let _ = progress_tx.send(ProgressMessage::Error("".to_string(), e.to_string()));
            let _ = progress_tx.send(ProgressMessage::Completed(0, files.len()));
            return;
        }
    };

    // Send prepare complete
    let _ = progress_tx.send(ProgressMessage::PrepareComplete);

    let mut completed_bytes: u64 = 0;
    let mut completed_files: usize = 0;

    // First, try simple rename for each file (fast path for same filesystem)
    let mut needs_copy: Vec<(PathBuf, PathBuf, u64)> = Vec::new();  // (src, dest, size)

    for file_path in &files {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }

        let src = if file_path.is_absolute() {
            file_path.clone()
        } else {
            source_dir.join(file_path)
        };

        let filename = src.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let dest = target_dir.join(&filename);

        // Check if this file should be skipped
        if files_to_skip.contains(&src) {
            continue;
        }

        // Get file/dir size for progress tracking
        let (item_size, item_files) = if src.is_dir() {
            calculate_dir_size(&src, &cancel_flag).unwrap_or((0, 1))
        } else {
            (fs::metadata(&src).map(|m| m.len()).unwrap_or(0), 1)
        };

        // Check if destination already exists
        if dest.exists() {
            if files_to_overwrite.contains(&src) {
                // Delete existing file/directory before moving
                if let Err(e) = delete_file(&dest) {
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(
                        filename,
                        format!("Failed to remove existing: {}", e),
                    ));
                    continue;
                }
            } else {
                // Not in overwrite set and not in skip set - unexpected conflict
                failure_count += 1;
                let _ = progress_tx.send(ProgressMessage::Error(
                    filename,
                    "Target already exists".to_string(),
                ));
                continue;
            }
        }

        let _ = progress_tx.send(ProgressMessage::FileStarted(filename.clone()));

        // Try rename first
        match fs::rename(&src, &dest) {
            Ok(_) => {
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
                // If cross-device, we need to copy+delete
                if is_cross_device_error(&e) {
                    needs_copy.push((src, dest, item_size));
                } else {
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, e.to_string()));
                }
            }
        }
    }

    // Handle cross-device moves (copy + delete)
    if !needs_copy.is_empty() && !cancel_flag.load(Ordering::Relaxed) {
        for (src, dest, _) in needs_copy {
            if cancel_flag.load(Ordering::Relaxed) {
                break;
            }

            let filename = src.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            let _ = progress_tx.send(ProgressMessage::FileStarted(filename.clone()));

            let copy_result = if src.is_dir() {
                copy_dir_recursive_with_progress(
                    &src,
                    &dest,
                    &cancel_flag,
                    &progress_tx,
                    &mut completed_bytes,
                    &mut completed_files,
                    total_bytes,
                    total_files,
                )
            } else {
                let file_size = fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
                let file_completed_bytes = completed_bytes;

                copy_file_with_progress(
                    &src,
                    &dest,
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
                ).map(|_| {
                    completed_bytes += file_size;
                    completed_files += 1;
                })
            };

            match copy_result {
                Ok(_) => {
                    // Delete source after successful copy
                    if let Err(e) = delete_file(&src) {
                        // Copy succeeded but delete failed - this is a move failure
                        failure_count += 1;
                        let _ = progress_tx.send(ProgressMessage::Error(
                            filename,
                            format!("Move failed: copied but could not delete source: {}", e),
                        ));
                    } else {
                        success_count += 1;
                        let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
                    }
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        // Cancelled - clean up partial copy
                        if dest.is_dir() {
                            let _ = fs::remove_dir_all(&dest);
                        } else {
                            let _ = fs::remove_file(&dest);
                        }
                        break;
                    }
                    failure_count += 1;
                    let _ = progress_tx.send(ProgressMessage::Error(filename, e.to_string()));
                }
            }
        }
    }

    let _ = progress_tx.send(ProgressMessage::Completed(success_count, failure_count));
}

/// Copy a file or directory
pub fn copy_file(src: &Path, dest: &Path) -> io::Result<()> {
    // Check if source and destination are the same
    let resolved_src = strip_unc_prefix(src.canonicalize()?);
    if dest.exists() {
        let resolved_dest = strip_unc_prefix(dest.canonicalize()?);
        if resolved_src == resolved_dest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Source and destination are the same file",
            ));
        }
    }

    // Check if destination already exists
    if dest.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Target already exists. Delete it first or choose a different name.",
        ));
    }

    let src_metadata = fs::metadata(src)?;

    // Check for special files (device files, sockets, etc.) that cannot be copied
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let file_type = src_metadata.file_type();
        if file_type.is_block_device()
            || file_type.is_char_device()
            || file_type.is_fifo()
            || file_type.is_socket()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot copy special file (device, socket, or pipe)",
            ));
        }
    }

    if src_metadata.is_dir() {
        // copy_dir_recursive preserves timestamps for all entries including the top-level dir
        copy_dir_recursive(src, dest)?;
    } else {
        fs::copy(src, dest)?;
        // Preserve timestamps (mtime, atime)
        let _ = preserve_timestamps(dest, &src_metadata);
    }

    Ok(())
}

/// Maximum recursion depth for directory copy to prevent stack overflow
const MAX_COPY_DEPTH: usize = 256;

/// Copy directory recursively with symlink loop detection
pub fn copy_dir_recursive(src: &Path, dest: &Path) -> io::Result<()> {
    let mut visited = HashSet::new();
    copy_dir_recursive_inner(src, dest, &mut visited, 0)
}

/// Internal recursive copy with visited path tracking
fn copy_dir_recursive_inner(
    src: &Path,
    dest: &Path,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) -> io::Result<()> {
    // Check maximum depth to prevent stack overflow
    if depth > MAX_COPY_DEPTH {
        return Err(io::Error::other(
            format!("Maximum directory depth ({}) exceeded - possible circular symlink", MAX_COPY_DEPTH),
        ));
    }

    // Detect symlink loops via canonicalised path. `visited` tracks only
    // the *current recursion stack* (DFS path), not every directory ever
    // seen — otherwise two siblings whose canonical paths resolve to the
    // same physical directory (bind mount, BTRFS subvolume, etc.) would
    // be falsely reported as a cycle. We insert here and remove on exit
    // (after the body, regardless of error).
    let canonical_src = src.canonicalize().map(strip_unc_prefix).unwrap_or_else(|_| src.to_path_buf());
    if visited.contains(&canonical_src) {
        return Err(io::Error::other(
            format!("Circular symlink detected: {}", src.display()),
        ));
    }
    visited.insert(canonical_src.clone());

    // Run the body inside an IIFE so any early return (`?`, explicit
    // `return Err`) still goes through the `visited.remove` cleanup
    // below — that's what enforces stack-only semantics.
    let result: io::Result<()> = (|| {
        fs::create_dir_all(dest)?;

        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dest_path = dest.join(entry.file_name());

            // Get metadata without following symlinks
            let metadata = fs::symlink_metadata(&src_path)?;

            if metadata.is_symlink() {
                // Reject symlinks pointing to sensitive system paths
                #[cfg(unix)]
                if let Ok(resolved) = src_path.canonicalize().map(strip_unc_prefix) {
                    let resolved_str = resolved.to_string_lossy();
                    if target_is_sensitive(&resolved_str) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!(
                                "Symlink '{}' points to sensitive system path: {}",
                                src_path.display(),
                                resolved_str
                            ),
                        ));
                    }
                }
                // Copy symlink as-is (don't follow it)
                #[cfg(unix)]
                {
                    let link_target = fs::read_link(&src_path)?;
                    std::os::unix::fs::symlink(&link_target, &dest_path)?;
                }
                #[cfg(not(unix))]
                {
                    // On non-Unix, just skip symlinks or copy as regular file
                    if src_path.is_file() {
                        fs::copy(&src_path, &dest_path)?;
                        if let Ok(target_meta) = fs::metadata(&src_path) {
                            let _ = preserve_timestamps(&dest_path, &target_meta);
                        }
                    }
                }
            } else if metadata.is_dir() {
                copy_dir_recursive_inner(&src_path, &dest_path, visited, depth + 1)?;
            } else {
                fs::copy(&src_path, &dest_path)?;
                // Preserve file timestamps
                let _ = preserve_timestamps(&dest_path, &metadata);
            }
        }

        // Preserve directory timestamps (must be done after all contents are copied)
        if let Ok(src_metadata) = fs::metadata(src) {
            let _ = preserve_timestamps(dest, &src_metadata);
        }

        Ok(())
    })();

    visited.remove(&canonical_src);
    result
}

/// Move a file or directory
pub fn move_file(src: &Path, dest: &Path) -> io::Result<()> {
    // Check if source and destination are the same
    let resolved_src = strip_unc_prefix(src.canonicalize()?);
    if dest.exists() {
        let resolved_dest = strip_unc_prefix(dest.canonicalize()?);
        if resolved_src == resolved_dest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Source and destination are the same",
            ));
        }
    }

    // Check if destination already exists
    if dest.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Target already exists. Delete it first or choose a different name.",
        ));
    }

    // Try rename first (fast for same filesystem)
    match fs::rename(src, dest) {
        Ok(_) => Ok(()),
        Err(e) => {
            // If rename fails (cross-device), copy then delete
            if is_cross_device_error(&e) {
                copy_file(src, dest)?;
                delete_file(src)?;
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// Delete a file or directory
pub fn delete_file(path: &Path) -> io::Result<()> {
    // Use symlink_metadata to check if it's a symlink
    let metadata = fs::symlink_metadata(path)?;

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
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Directory already exists",
        ));
    }

    fs::create_dir_all(path)
}

/// Rename a file or directory
pub fn rename_file(old_path: &Path, new_path: &Path) -> io::Result<()> {
    if new_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Target already exists",
        ));
    }

    fs::rename(old_path, new_path)
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
    "/etc", "/sys", "/proc", "/boot", "/root", "/var/log",
    "/home", "/dev", "/run", "/var/run",
];

#[cfg(windows)]
const SENSITIVE_PATHS: &[&str] = &[
    "C:\\Windows", "C:\\Program Files", "C:\\Program Files (x86)",
];

/// True iff `target` equals or is contained within one of `SENSITIVE_PATHS`.
/// Matches on path-segment boundaries, so "/etc" does not match "/etcd/foo".
fn target_is_sensitive(target: &str) -> bool {
    #[cfg(unix)]
    const SEP: char = '/';
    #[cfg(windows)]
    const SEP: char = '\\';
    let normalized_target = {
        #[cfg(unix)]
        {
            target.strip_prefix("/private").unwrap_or(target)
        }
        #[cfg(not(unix))]
        {
            target
        }
    };
    for candidate in [target, normalized_target] {
        for sensitive in SENSITIVE_PATHS {
            if candidate == *sensitive {
                return true;
            }
            let mut boundary = String::with_capacity(sensitive.len() + 1);
            boundary.push_str(sensitive);
            boundary.push(SEP);
            if candidate.starts_with(&boundary) {
                return true;
            }
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
        let entries = fs::read_dir(path).map_err(|e| io::Error::new(
            e.kind(),
            format!("Cannot read directory '{}' for symlink check: {}", path.display(), e),
        ))?;
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
        collect_unsafe_symlinks(&file_path, base_dir, file, &mut excluded_paths, &mut visited);
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
                    collect_unsafe_symlinks(&entry.path(), base_dir, &entry_relative, excluded, visited);
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
    use std::sync::atomic::{AtomicU64, Ordering};

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
    fn test_target_is_sensitive_private_prefix() {
        assert!(target_is_sensitive("/private/etc"));
        assert!(target_is_sensitive("/private/var/log"));
        assert!(target_is_sensitive("/private/dev/null"));
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
