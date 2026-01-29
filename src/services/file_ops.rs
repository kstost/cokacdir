use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

/// File operation type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperationType {
    Copy,
    Move,
}

/// Progress message for file operations
#[derive(Debug, Clone)]
#[allow(dead_code)]  // Fields are used for debugging/logging, not always read
pub enum ProgressMessage {
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
            // Copy symlink
            #[cfg(unix)]
            {
                let link_target = fs::read_link(&src_path)?;

                // Security: Validate symlink target
                if link_target.is_absolute() {
                    let target_str = link_target.to_string_lossy();
                    let sensitive_paths = ["/etc", "/sys", "/proc", "/boot", "/root", "/var/log"];
                    for sensitive in sensitive_paths {
                        if target_str.starts_with(sensitive) {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                format!("Cannot copy symlink pointing to sensitive path: {}", target_str),
                            ));
                        }
                    }
                }

                std::os::unix::fs::symlink(&link_target, &dest_path)?;
            }
            #[cfg(not(unix))]
            {
                if src_path.is_file() {
                    fs::copy(&src_path, &dest_path)?;
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
            copy_dir_recursive_with_progress(
                &src_path,
                &dest_path,
                cancel_flag,
                progress_tx,
                completed_bytes,
                completed_files,
                total_bytes,
                total_files,
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

    Ok(())
}

/// Copy files with progress reporting (main entry point for progress-enabled copy)
pub fn copy_files_with_progress(
    files: Vec<PathBuf>,
    source_dir: &Path,
    target_dir: &Path,
    cancel_flag: Arc<AtomicBool>,
    progress_tx: Sender<ProgressMessage>,
) {
    let mut success_count = 0;
    let mut failure_count = 0;

    // Build full paths for size calculation
    let full_paths: Vec<PathBuf> = files.iter()
        .map(|f| if f.is_absolute() { f.clone() } else { source_dir.join(f) })
        .collect();

    // Calculate total size
    let (total_bytes, total_files) = match calculate_total_size(&full_paths, &cancel_flag) {
        Ok((size, count)) => (size, count),
        Err(e) => {
            let _ = progress_tx.send(ProgressMessage::Error("".to_string(), e.to_string()));
            let _ = progress_tx.send(ProgressMessage::Completed(0, files.len()));
            return;
        }
    };

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

        // Check if destination already exists
        if dest.exists() {
            failure_count += 1;
            let _ = progress_tx.send(ProgressMessage::Error(
                filename,
                "Target already exists".to_string(),
            ));
            continue;
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
pub fn move_files_with_progress(
    files: Vec<PathBuf>,
    source_dir: &Path,
    target_dir: &Path,
    cancel_flag: Arc<AtomicBool>,
    progress_tx: Sender<ProgressMessage>,
) {
    let mut success_count = 0;
    let mut failure_count = 0;

    // Build full paths for size calculation
    let full_paths: Vec<PathBuf> = files.iter()
        .map(|f| if f.is_absolute() { f.clone() } else { source_dir.join(f) })
        .collect();

    // Calculate total size upfront for accurate progress
    let (total_bytes, total_files) = match calculate_total_size(&full_paths, &cancel_flag) {
        Ok((size, count)) => (size, count),
        Err(e) => {
            let _ = progress_tx.send(ProgressMessage::Error("".to_string(), e.to_string()));
            let _ = progress_tx.send(ProgressMessage::Completed(0, files.len()));
            return;
        }
    };

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

        // Get file/dir size for progress tracking
        let (item_size, item_files) = if src.is_dir() {
            calculate_dir_size(&src, &cancel_flag).unwrap_or((0, 1))
        } else {
            (fs::metadata(&src).map(|m| m.len()).unwrap_or(0), 1)
        };

        // Check if destination already exists
        if dest.exists() {
            failure_count += 1;
            let _ = progress_tx.send(ProgressMessage::Error(
                filename,
                "Target already exists".to_string(),
            ));
            continue;
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
                if e.raw_os_error() == Some(libc::EXDEV) {
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
                        // Copy succeeded but delete failed - report but count as success
                        let _ = progress_tx.send(ProgressMessage::Error(
                            filename.clone(),
                            format!("Copied but failed to delete source: {}", e),
                        ));
                    }
                    success_count += 1;
                    let _ = progress_tx.send(ProgressMessage::FileCompleted(filename));
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
    let resolved_src = src.canonicalize()?;
    if dest.exists() {
        let resolved_dest = dest.canonicalize()?;
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

    if src.is_dir() {
        copy_dir_recursive(src, dest)
    } else {
        fs::copy(src, dest)?;
        Ok(())
    }
}

/// Maximum recursion depth for directory copy to prevent stack overflow
const MAX_COPY_DEPTH: usize = 256;

/// Copy directory recursively with symlink loop detection
fn copy_dir_recursive(src: &Path, dest: &Path) -> io::Result<()> {
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

    // Get canonical path to detect symlink loops
    let canonical_src = src.canonicalize().unwrap_or_else(|_| src.to_path_buf());

    // Check for circular symlink
    if visited.contains(&canonical_src) {
        return Err(io::Error::other(
            format!("Circular symlink detected: {}", src.display()),
        ));
    }
    visited.insert(canonical_src);

    fs::create_dir_all(dest)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        // Get metadata without following symlinks
        let metadata = fs::symlink_metadata(&src_path)?;

        if metadata.is_symlink() {
            // Copy symlink as symlink (don't follow it)
            #[cfg(unix)]
            {
                let link_target = fs::read_link(&src_path)?;

                // Security: Validate symlink target
                // Reject absolute symlinks pointing to sensitive system paths
                if link_target.is_absolute() {
                    let target_str = link_target.to_string_lossy();
                    let sensitive_paths = ["/etc", "/sys", "/proc", "/boot", "/root", "/var/log"];
                    for sensitive in sensitive_paths {
                        if target_str.starts_with(sensitive) {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                format!("Cannot copy symlink pointing to sensitive path: {}", target_str),
                            ));
                        }
                    }
                }

                std::os::unix::fs::symlink(&link_target, &dest_path)?;
            }
            #[cfg(not(unix))]
            {
                // On non-Unix, just skip symlinks or copy as regular file
                if src_path.is_file() {
                    fs::copy(&src_path, &dest_path)?;
                }
            }
        } else if metadata.is_dir() {
            copy_dir_recursive_inner(&src_path, &dest_path, visited, depth + 1)?;
        } else {
            fs::copy(&src_path, &dest_path)?;
        }
    }

    Ok(())
}

/// Move a file or directory
pub fn move_file(src: &Path, dest: &Path) -> io::Result<()> {
    // Check if source and destination are the same
    let resolved_src = src.canonicalize()?;
    if dest.exists() {
        let resolved_dest = dest.canonicalize()?;
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
            if e.raw_os_error() == Some(libc::EXDEV) {
                copy_file(src, dest)?;
                delete_file(src)?;
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// Protected system paths that should never be deleted
const PROTECTED_PATHS: &[&str] = &[
    "/", "/bin", "/boot", "/dev", "/etc", "/home", "/lib", "/lib64",
    "/opt", "/proc", "/root", "/sbin", "/sys", "/tmp", "/usr", "/var",
];

/// Delete a file or directory
pub fn delete_file(path: &Path) -> io::Result<()> {
    // Security: Prevent deletion of protected system paths
    if let Ok(canonical) = path.canonicalize() {
        let path_str = canonical.to_string_lossy();
        for protected in PROTECTED_PATHS {
            if path_str == *protected {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("Cannot delete protected system path: {}", protected),
                ));
            }
        }
    }

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

    #[test]
    fn test_delete_protected_path_rejected() {
        // Test that protected paths cannot be deleted
        for protected in PROTECTED_PATHS {
            let result = delete_file(Path::new(protected));
            // Either permission denied or the path protection kicks in
            assert!(result.is_err());
        }
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
}
