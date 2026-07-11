use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use md5::{Digest, Md5};

use crate::services::file_ops::{
    create_private_quarantine_directory, metadata_still_matches, open_regular_file_no_follow,
    prepare_file_deletion, rename_noreplace, stable_file_identity, stable_path_identity,
    StablePathIdentity,
};

const READ_BUF_SIZE: usize = 64 * 1024; // 64KB

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified: Option<std::time::SystemTime>,
    identity: StablePathIdentity,
}

impl FileFingerprint {
    fn from_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            identity: stable_file_identity(file)?,
        })
    }

    fn from_path(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            identity: stable_path_identity(path)?,
        })
    }
}

/// Keeps both compared files open until deletion and records the exact
/// directory-entry identities that were compared. This prevents a different
/// file (or a symlink) swapped into either path after comparison from being
/// unlinked as though it had been verified.
#[derive(Debug)]
struct VerifiedDuplicatePair {
    keep_file: File,
    duplicate_file: File,
    keep_fingerprint: FileFingerprint,
    duplicate_fingerprint: FileFingerprint,
    duplicate_size: u64,
}

impl VerifiedDuplicatePair {
    fn paths_still_match(&self, keep_path: &Path, duplicate_path: &Path) -> bool {
        let Ok(keep_path_meta) = fs::symlink_metadata(keep_path) else {
            return false;
        };
        let Ok(duplicate_path_meta) = fs::symlink_metadata(duplicate_path) else {
            return false;
        };
        if !keep_path_meta.is_file() || !duplicate_path_meta.is_file() {
            return false;
        }

        FileFingerprint::from_file(&self.keep_file).ok().as_ref() == Some(&self.keep_fingerprint)
            && FileFingerprint::from_file(&self.duplicate_file)
                .ok()
                .as_ref()
                == Some(&self.duplicate_fingerprint)
            && FileFingerprint::from_path(keep_path).ok().as_ref() == Some(&self.keep_fingerprint)
            && FileFingerprint::from_path(duplicate_path).ok().as_ref()
                == Some(&self.duplicate_fingerprint)
    }
}

fn restore_quarantined_duplicate(
    quarantined: &Path,
    original: &Path,
    quarantine_dir: &Path,
) -> io::Result<()> {
    match rename_noreplace(quarantined, original) {
        Ok(()) => {
            let _ = fs::remove_dir(quarantine_dir);
            Ok(())
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "Could not restore a retained duplicate to '{}': {}. It remains at '{}'",
                original.display(),
                error,
                quarantined.display()
            ),
        )),
    }
}

fn delete_verified_duplicate(
    verified: VerifiedDuplicatePair,
    keep_path: &Path,
    duplicate_path: &Path,
) -> io::Result<bool> {
    delete_verified_duplicate_impl(verified, keep_path, duplicate_path, |_| {})
}

fn delete_verified_duplicate_impl<F>(
    verified: VerifiedDuplicatePair,
    keep_path: &Path,
    duplicate_path: &Path,
    after_quarantine: F,
) -> io::Result<bool>
where
    F: FnOnce(&Path),
{
    if !verified.paths_still_match(keep_path, duplicate_path) {
        return Ok(false);
    }

    let parent = duplicate_path.parent().unwrap_or_else(|| Path::new("."));
    let quarantine_dir = create_private_quarantine_directory(parent, "dedup")?;
    let quarantined = quarantine_dir.join("duplicate");
    if let Err(error) = rename_noreplace(duplicate_path, &quarantined) {
        let _ = fs::remove_dir(&quarantine_dir);
        return Err(error);
    }
    after_quarantine(&quarantined);

    let duplicate_matches = FileFingerprint::from_path(&quarantined).ok().as_ref()
        == Some(&verified.duplicate_fingerprint)
        && FileFingerprint::from_file(&verified.duplicate_file)
            .ok()
            .as_ref()
            == Some(&verified.duplicate_fingerprint);
    let keep_matches = FileFingerprint::from_path(keep_path).ok().as_ref()
        == Some(&verified.keep_fingerprint)
        && FileFingerprint::from_file(&verified.keep_file)
            .ok()
            .as_ref()
            == Some(&verified.keep_fingerprint);
    if !duplicate_matches || !keep_matches {
        restore_quarantined_duplicate(&quarantined, duplicate_path, &quarantine_dir)?;
        return Ok(false);
    }

    let deletion =
        match prepare_file_deletion(&quarantined, verified.duplicate_fingerprint.identity) {
            Ok(deletion) => deletion,
            Err(error) => {
                return match restore_quarantined_duplicate(
                    &quarantined,
                    duplicate_path,
                    &quarantine_dir,
                ) {
                    Ok(()) => Err(error),
                    Err(restore_error) => Err(io::Error::new(
                        error.kind(),
                        format!("{}; {}", error, restore_error),
                    )),
                };
            }
        };
    drop(verified);
    if let Err(error) = deletion.delete() {
        return match restore_quarantined_duplicate(&quarantined, duplicate_path, &quarantine_dir) {
            Ok(()) => Err(error),
            Err(restore_error) => Err(io::Error::new(
                error.kind(),
                format!("{}; {}", error, restore_error),
            )),
        };
    }
    fs::remove_dir(&quarantine_dir)?;
    Ok(true)
}

/// Byte-level equality check; guards against MD5 collisions before destructive deletion.
///
/// Uses `read_exact` over equal-sized chunks rather than two independent
/// `read` calls — `Read::read` is allowed to short-read, and the previous
/// implementation could compare two slices of unequal length and falsely
/// report identical files as different.
///
/// Polls `cancel_flag` between chunks so a `/stop` during a multi-GB
/// compare returns promptly instead of waiting for the file pair to
/// finish. Cancellation is reported as `io::ErrorKind::Interrupted` so
/// the caller can distinguish it from a real I/O error.
fn files_byte_equal(
    a: &Path,
    b: &Path,
    cancel_flag: &Arc<AtomicBool>,
) -> io::Result<Option<VerifiedDuplicatePair>> {
    let (fa, before_a) = open_regular_file_no_follow(a)?;
    let (fb, before_b) = open_regular_file_no_follow(b)?;
    let len_a = before_a.len();
    let len_b = before_b.len();
    if len_a != len_b {
        return Ok(None);
    }
    let mut ra = BufReader::with_capacity(READ_BUF_SIZE, fa);
    let mut rb = BufReader::with_capacity(READ_BUF_SIZE, fb);
    let mut buf_a = [0u8; READ_BUF_SIZE];
    let mut buf_b = [0u8; READ_BUF_SIZE];
    let mut remaining = len_a;
    while remaining > 0 {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let chunk = std::cmp::min(remaining, READ_BUF_SIZE as u64) as usize;
        ra.read_exact(&mut buf_a[..chunk])?;
        rb.read_exact(&mut buf_b[..chunk])?;
        if buf_a[..chunk] != buf_b[..chunk] {
            return Ok(None);
        }
        remaining -= chunk as u64;
    }
    let keep_file = ra.into_inner();
    let duplicate_file = rb.into_inner();
    if !metadata_still_matches(&before_a, &keep_file.metadata()?)
        || !metadata_still_matches(&before_b, &duplicate_file.metadata()?)
    {
        return Err(io::Error::other(
            "Compared file changed while duplicate contents were being read",
        ));
    }
    let keep_fingerprint = FileFingerprint::from_file(&keep_file)?;
    let duplicate_fingerprint = FileFingerprint::from_file(&duplicate_file)?;
    Ok(Some(VerifiedDuplicatePair {
        keep_file,
        duplicate_file,
        keep_fingerprint,
        duplicate_fingerprint,
        duplicate_size: len_b,
    }))
}

// Marker files: if any of these exist INSIDE a directory, skip that entire directory
// (matches removeduplicated.js lines 47-50)
const DIR_MARKER_FILES: &[&str] = &[
    ".ignoresorting",
    ".ignoreplaceken",
    "CurrentVersion.plist",
    "__Sync__",
];

// Path substring: if directory path contains this string, skip it
// (matches removeduplicated.js line 51)
const DIR_PATH_SKIP: &[&str] = &[".fcpbundle"];

// Individual file names to skip during scan
// (matches removeduplicated.js lines 60-61)
const SKIP_FILE_NAMES: &[&str] = &[".ignoresorting", ".ignoreplaceken"];

#[derive(Debug, Clone, PartialEq)]
pub enum DedupPhase {
    Scanning,
    Hashing,
    Deleting,
    Complete,
}

pub enum DedupMessage {
    Phase(DedupPhase),
    Scanning(String),
    Hashing(String, u8),
    Deleting(String),
    Log(String),
    Stats {
        scanned: usize,
        duplicates: usize,
        freed: u64,
    },
    Error(String),
    Complete,
}

#[derive(Debug)]
struct FileEntry {
    path: PathBuf,
    size: u64,
}

fn scan_directory(
    dir: &Path,
    tx: &Sender<DedupMessage>,
    cancel_flag: &Arc<AtomicBool>,
    size_map: &mut HashMap<u64, Vec<FileEntry>>,
    scanned: &mut usize,
) {
    // Directory-level skip: check if marker files exist INSIDE this directory
    // (matches removeduplicated.js lines 47-50)
    for &marker in DIR_MARKER_FILES {
        if dir.join(marker).exists() {
            return;
        }
    }

    // Directory-level skip: check if path string contains skip patterns
    // (matches removeduplicated.js line 51)
    let dir_str = dir.to_string_lossy();
    for &pattern in DIR_PATH_SKIP {
        if dir_str.contains(pattern) {
            return;
        }
    }

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            let _ = tx.send(DedupMessage::Error(format!(
                "Cannot read {}: {}",
                dir.display(),
                e
            )));
            return;
        }
    };

    for entry in entries {
        if cancel_flag.load(Ordering::Relaxed) {
            return;
        }

        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();

        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if metadata.is_dir() {
            scan_directory(&path, tx, cancel_flag, size_map, scanned);
        } else if metadata.is_file() {
            // Skip specific file names (matches removeduplicated.js lines 60-61)
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if SKIP_FILE_NAMES.contains(&name) {
                    continue;
                }
            }

            let size = metadata.len();
            if size == 0 {
                continue; // Skip empty files
            }

            *scanned += 1;
            let _ = tx.send(DedupMessage::Scanning(path.display().to_string()));
            let _ = tx.send(DedupMessage::Log(format!("READING {}", path.display())));
            let _ = tx.send(DedupMessage::Stats {
                scanned: *scanned,
                duplicates: 0,
                freed: 0,
            });

            size_map
                .entry(size)
                .or_default()
                .push(FileEntry { path, size });
        }
    }
}

fn compute_md5(
    path: &Path,
    file_size: u64,
    tx: &Sender<DedupMessage>,
    cancel_flag: &Arc<AtomicBool>,
) -> Option<String> {
    let (file, before) = match open_regular_file_no_follow(path) {
        Ok(opened) => opened,
        Err(e) => {
            let _ = tx.send(DedupMessage::Error(format!(
                "Cannot open {}: {}",
                path.display(),
                e
            )));
            return None;
        }
    };

    let mut reader = BufReader::new(file);
    let mut hasher = Md5::new();
    let mut buf = [0u8; READ_BUF_SIZE];
    let mut bytes_read: u64 = 0;

    loop {
        if cancel_flag.load(Ordering::Relaxed) {
            return None;
        }

        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = tx.send(DedupMessage::Error(format!(
                    "Read error {}: {}",
                    path.display(),
                    e
                )));
                return None;
            }
        };

        hasher.update(&buf[..n]);
        bytes_read += n as u64;

        if file_size > 0 {
            let progress = ((bytes_read as f64 / file_size as f64) * 100.0) as u8;
            let _ = tx.send(DedupMessage::Hashing(
                path.display().to_string(),
                progress.min(100),
            ));
        }
    }

    let file = reader.into_inner();
    if !metadata_still_matches(&before, &file.metadata().ok()?)
        || stable_file_identity(&file).ok()? != stable_path_identity(path).ok()?
    {
        let _ = tx.send(DedupMessage::Error(format!(
            "File changed while hashing: {}",
            path.display()
        )));
        return None;
    }

    Some(format!("{:032x}", hasher.finalize()))
}

pub fn run_dedup(target_path: PathBuf, tx: Sender<DedupMessage>, cancel_flag: Arc<AtomicBool>) {
    // Phase 1: Scan
    let _ = tx.send(DedupMessage::Phase(DedupPhase::Scanning));
    let _ = tx.send(DedupMessage::Log("Scanning files...".into()));

    let mut size_map: HashMap<u64, Vec<FileEntry>> = HashMap::new();
    let mut scanned: usize = 0;

    scan_directory(&target_path, &tx, &cancel_flag, &mut size_map, &mut scanned);

    if cancel_flag.load(Ordering::Relaxed) {
        let _ = tx.send(DedupMessage::Log("Cancelled.".into()));
        let _ = tx.send(DedupMessage::Complete);
        return;
    }

    // Filter to groups with 2+ files (potential duplicates)
    let candidate_groups: Vec<Vec<FileEntry>> = size_map
        .into_values()
        .filter(|group| group.len() >= 2)
        .collect();

    let candidate_count: usize = candidate_groups.iter().map(|g| g.len()).sum();
    let _ = tx.send(DedupMessage::Log(format!(
        "Scan complete: {} files scanned, {} candidates in {} groups",
        scanned,
        candidate_count,
        candidate_groups.len()
    )));

    // Phase 2: Hash
    let _ = tx.send(DedupMessage::Phase(DedupPhase::Hashing));

    let mut hash_map: HashMap<String, Vec<PathBuf>> = HashMap::new();

    // Calculate total size for percentage
    let total_bytes: u64 = candidate_groups
        .iter()
        .flat_map(|g| g.iter())
        .map(|e| e.size)
        .sum();
    let mut accum_bytes: u64 = 0;

    for group in &candidate_groups {
        for entry in group {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(DedupMessage::Log("Cancelled.".into()));
                let _ = tx.send(DedupMessage::Complete);
                return;
            }

            accum_bytes += entry.size;
            let pct = if total_bytes > 0 {
                ((accum_bytes as f64 / total_bytes as f64) * 100.0).round() as u8
            } else {
                0
            };

            if let Some(hash) = compute_md5(&entry.path, entry.size, &tx, &cancel_flag) {
                let _ = tx.send(DedupMessage::Log(format!(
                    "{} {} % {} {}",
                    hash,
                    pct,
                    entry.size,
                    entry.path.display()
                )));
                hash_map.entry(hash).or_default().push(entry.path.clone());
            }
        }
    }

    // `compute_md5` returns `None` when cancellation arrives mid-file. If it
    // was the final candidate there is no next iteration to observe the flag.
    if cancel_flag.load(Ordering::Relaxed) {
        let _ = tx.send(DedupMessage::Log("Cancelled.".into()));
        let _ = tx.send(DedupMessage::Stats {
            scanned,
            duplicates: 0,
            freed: 0,
        });
        let _ = tx.send(DedupMessage::Complete);
        return;
    }

    // Filter to duplicate groups (2+ files with same hash)
    let dup_groups: Vec<(&String, &Vec<PathBuf>)> = hash_map
        .iter()
        .filter(|(_, paths)| paths.len() >= 2)
        .collect();

    let total_duplicates: usize = dup_groups.iter().map(|(_, paths)| paths.len() - 1).sum();

    if total_duplicates == 0 {
        let _ = tx.send(DedupMessage::Log("No duplicates found.".into()));
        let _ = tx.send(DedupMessage::Stats {
            scanned,
            duplicates: 0,
            freed: 0,
        });
        let _ = tx.send(DedupMessage::Phase(DedupPhase::Complete));
        let _ = tx.send(DedupMessage::Complete);
        return;
    }

    // Phase 3: Delete
    let _ = tx.send(DedupMessage::Phase(DedupPhase::Deleting));
    let _ = tx.send(DedupMessage::Log("Removing duplicates...".into()));

    let mut deleted_count: usize = 0;
    let mut freed_bytes: u64 = 0;

    for (_hash, paths) in &dup_groups {
        // Keep first file, delete the rest
        let keep_path = &paths[0];
        for dup_path in paths.iter().skip(1) {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(DedupMessage::Log(format!(
                    "Cancelled. Removed {} files, freed {}",
                    deleted_count,
                    format_size(freed_bytes)
                )));
                let _ = tx.send(DedupMessage::Stats {
                    scanned,
                    duplicates: deleted_count,
                    freed: freed_bytes,
                });
                let _ = tx.send(DedupMessage::Complete);
                return;
            }

            // Verify byte-level equality before destructive deletion (guard against MD5 collision)
            let verified = match files_byte_equal(keep_path, dup_path, &cancel_flag) {
                Ok(Some(verified)) => verified,
                Ok(None) => {
                    let _ = tx.send(DedupMessage::Log(format!(
                        "SKIP (hash collision; contents differ): {}",
                        dup_path.display()
                    )));
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    // Cancellation can arrive while comparing the final pair,
                    // in which case there is no "next iteration" to observe
                    // the flag. Finish immediately instead of incorrectly
                    // reporting the destructive pass as successfully complete.
                    let _ = tx.send(DedupMessage::Log(format!(
                        "Cancelled. Removed {} files, freed {}",
                        deleted_count,
                        format_size(freed_bytes)
                    )));
                    let _ = tx.send(DedupMessage::Stats {
                        scanned,
                        duplicates: deleted_count,
                        freed: freed_bytes,
                    });
                    let _ = tx.send(DedupMessage::Complete);
                    return;
                }
                Err(e) => {
                    let _ = tx.send(DedupMessage::Error(format!(
                        "Failed to verify {} vs {}: {}",
                        keep_path.display(),
                        dup_path.display(),
                        e
                    )));
                    continue;
                }
            };

            let file_size = verified.duplicate_size;

            // Move the candidate into a private, no-clobber quarantine first,
            // then bind that name back to the compared open handle before
            // deletion. A replacement at the public path is never unlinked.
            match delete_verified_duplicate(verified, keep_path, dup_path) {
                Ok(true) => {
                    deleted_count += 1;
                    freed_bytes += file_size;
                    let _ = tx.send(DedupMessage::Deleting(dup_path.display().to_string()));
                    let _ = tx.send(DedupMessage::Log(format!(
                        "REMOVE {} {}",
                        _hash,
                        dup_path.display()
                    )));
                    let _ = tx.send(DedupMessage::Stats {
                        scanned,
                        duplicates: deleted_count,
                        freed: freed_bytes,
                    });
                }
                Ok(false) => {
                    let _ = tx.send(DedupMessage::Log(format!(
                        "SKIP (file changed after verification): {}",
                        dup_path.display()
                    )));
                }
                Err(e) => {
                    let _ = tx.send(DedupMessage::Error(format!(
                        "Failed to delete {}: {}",
                        dup_path.display(),
                        e
                    )));
                }
            }
        }
    }

    let _ = tx.send(DedupMessage::Log(format!(
        "Complete! Removed {} duplicate files, freed {}",
        deleted_count,
        format_size(freed_bytes)
    )));
    let _ = tx.send(DedupMessage::Stats {
        scanned,
        duplicates: deleted_count,
        freed: freed_bytes,
    });
    let _ = tx.send(DedupMessage::Phase(DedupPhase::Complete));
    let _ = tx.send(DedupMessage::Complete);
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_comparison_reports_cancellation_as_interrupted() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let left = dir.path().join("left.bin");
        let right = dir.path().join("right.bin");
        fs::write(&left, vec![7u8; READ_BUF_SIZE + 1]).expect("write left");
        fs::write(&right, vec![7u8; READ_BUF_SIZE + 1]).expect("write right");
        let cancelled = Arc::new(AtomicBool::new(true));

        let error = files_byte_equal(&left, &right, &cancelled)
            .expect_err("cancelled comparison must stop");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[cfg(unix)]
    #[test]
    fn verified_pair_rejects_replaced_duplicate_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let keep = dir.path().join("keep.bin");
        let duplicate = dir.path().join("duplicate.bin");
        let old_duplicate = dir.path().join("old-duplicate.bin");
        fs::write(&keep, b"same bytes").expect("write keep");
        fs::write(&duplicate, b"same bytes").expect("write duplicate");
        let cancelled = Arc::new(AtomicBool::new(false));

        let verified = files_byte_equal(&keep, &duplicate, &cancelled)
            .expect("compare files")
            .expect("files should match");
        fs::rename(&duplicate, &old_duplicate).expect("move compared duplicate");
        fs::write(&duplicate, b"new content").expect("replace duplicate path");

        assert!(!verified.paths_still_match(&keep, &duplicate));
        assert_eq!(fs::read(&duplicate).unwrap(), b"new content");
    }

    #[test]
    fn quarantined_duplicate_swap_is_retained_not_deleted() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let keep = dir.path().join("keep.bin");
        let duplicate = dir.path().join("duplicate.bin");
        let retained_original = dir.path().join("retained-original.bin");
        fs::write(&keep, b"same bytes").expect("write keep");
        fs::write(&duplicate, b"same bytes").expect("write duplicate");
        let cancelled = Arc::new(AtomicBool::new(false));
        let verified = files_byte_equal(&keep, &duplicate, &cancelled)
            .expect("compare files")
            .expect("files should match");

        let deleted = delete_verified_duplicate_impl(verified, &keep, &duplicate, |quarantined| {
            fs::rename(quarantined, &retained_original).expect("retain compared object");
            fs::write(quarantined, b"different!").expect("inject same-size replacement");
            let metadata = fs::metadata(&retained_original).expect("inspect original");
            let accessed = filetime::FileTime::from_last_access_time(&metadata);
            let modified = filetime::FileTime::from_last_modification_time(&metadata);
            filetime::set_file_times(quarantined, accessed, modified)
                .expect("match replacement timestamps");
        })
        .expect("replacement should be restored safely");

        assert!(!deleted);
        assert_eq!(fs::read(&duplicate).unwrap(), b"different!");
        assert_eq!(fs::read(&retained_original).unwrap(), b"same bytes");
    }
}
