use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

/// Delete a file or directory
pub fn delete_file(path: &Path) -> io::Result<()> {
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

    Ok(())
}
