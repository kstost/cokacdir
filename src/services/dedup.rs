use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use md5::{Digest, Md5};

use crate::services::file_ops::{
    metadata_still_matches, open_authorized_directory, stable_file_identity, stable_path_identity,
    DirectoryAccess, DirectoryAuthorization, StablePathIdentity,
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

    fn from_entry(parent: &DirectoryAccess, name: &OsStr) -> io::Result<Self> {
        let (file, _) = parent.open_regular_file(name)?;
        Self::from_file(&file)
    }
}

#[derive(Debug)]
struct DedupRoot {
    path: PathBuf,
    access: DirectoryAccess,
}

impl DedupRoot {
    fn verify(&self) -> io::Result<()> {
        if stable_path_identity(&self.path)? != stable_file_identity(self.access.file())? {
            return Err(io::Error::other("Duplicate-removal root directory changed"));
        }
        Ok(())
    }
}

/// Reopen the scanned ancestry through the held root, checking every directory
/// identity. Only a bounded number of handles (the current path depth) is kept
/// open, rather than one handle for every directory in a large scan.
#[derive(Debug)]
struct BoundFileLocation {
    root: Arc<DedupRoot>,
    parents: Vec<(OsString, DirectoryAccess)>,
    name: OsString,
    path: PathBuf,
    file_identity: StablePathIdentity,
    size: u64,
}

impl BoundFileLocation {
    fn bind(root: &Arc<DedupRoot>, entry: &FileEntry) -> io::Result<Self> {
        root.verify()?;
        let mut location = Self {
            root: root.clone(),
            parents: Vec::new(),
            name: entry
                .path
                .file_name()
                .ok_or_else(|| io::Error::other("Missing file name"))?
                .into(),
            path: entry.path.clone(),
            file_identity: entry.identity,
            size: entry.size,
        };
        for (name, expected) in &entry.directories {
            let (directory, access, _) = location.parent().open_directory(name)?;
            if stable_file_identity(&directory)? != *expected {
                return Err(io::Error::other(
                    "Scanned directory changed before duplicate comparison",
                ));
            }
            location.parents.push((name.clone(), access));
        }
        let current = location.parent().child_metadata(&location.name)?;
        if !current.is_file() || current.identity() != entry.identity || current.len() != entry.size
        {
            return Err(io::Error::other(
                "Scanned file changed before duplicate comparison",
            ));
        }
        location.verify_ancestry()?;
        Ok(location)
    }

    fn parent(&self) -> &DirectoryAccess {
        self.parents
            .last()
            .map(|(_, access)| access)
            .unwrap_or(&self.root.access)
    }

    fn verify_ancestry(&self) -> io::Result<()> {
        self.root.verify()?;
        let mut parent = &self.root.access;
        for (name, directory) in &self.parents {
            if parent.child_identity(name)? != stable_file_identity(directory.file())? {
                return Err(io::Error::other("Scanned directory ancestry changed"));
            }
            parent = directory;
        }
        Ok(())
    }

    fn matches(&self, expected: &FileFingerprint) -> bool {
        self.verify_ancestry().is_ok()
            && FileFingerprint::from_entry(self.parent(), &self.name)
                .ok()
                .as_ref()
                == Some(expected)
    }

    fn open(&self) -> io::Result<(File, fs::Metadata)> {
        self.verify_ancestry()?;
        let (file, metadata) = self.parent().open_regular_file(&self.name)?;
        if stable_file_identity(&file)? != self.file_identity || metadata.len() != self.size {
            return Err(io::Error::other("Scanned file changed while being opened"));
        }
        Ok((file, metadata))
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
    keep_location: BoundFileLocation,
    duplicate_location: BoundFileLocation,
}

impl VerifiedDuplicatePair {
    fn paths_still_match(&self, keep_path: &Path, duplicate_path: &Path) -> bool {
        if keep_path != self.keep_location.path || duplicate_path != self.duplicate_location.path {
            return false;
        }

        FileFingerprint::from_file(&self.keep_file).ok().as_ref() == Some(&self.keep_fingerprint)
            && FileFingerprint::from_file(&self.duplicate_file)
                .ok()
                .as_ref()
                == Some(&self.duplicate_fingerprint)
            && self.keep_location.matches(&self.keep_fingerprint)
            && self.duplicate_location.matches(&self.duplicate_fingerprint)
    }
}

fn restore_quarantined_duplicate(
    quarantine: DirectoryAccess,
    quarantine_name: &OsStr,
    quarantine_identity: StablePathIdentity,
    original: &BoundFileLocation,
) -> io::Result<()> {
    let result =
        quarantine.rename_noreplace_to(OsStr::new("duplicate"), original.parent(), &original.name);
    drop(quarantine);
    match result {
        Ok(()) => {
            original
                .parent()
                .remove_directory_if_identity(quarantine_name, quarantine_identity)?;
            Ok(())
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "Could not restore a retained duplicate to '{}': {}. It remains at '{}'",
                original.path.display(),
                error,
                original
                    .path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(quarantine_name)
                    .join("duplicate")
                    .display()
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

    let parent = verified.duplicate_location.parent();
    let quarantine_name = parent.create_private_directory("dedup")?;
    let quarantine_identity = parent.child_identity(&quarantine_name)?;
    let (directory, quarantine, _) = parent.open_directory(&quarantine_name)?;
    if stable_file_identity(&directory)? != quarantine_identity {
        return Err(io::Error::other("Duplicate deletion quarantine changed"));
    }
    drop(directory);
    let quarantine_dir = duplicate_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(&quarantine_name);
    let quarantined = quarantine_dir.join("duplicate");
    if let Err(error) = parent.rename_noreplace_to(
        &verified.duplicate_location.name,
        &quarantine,
        OsStr::new("duplicate"),
    ) {
        drop(quarantine);
        let _ = parent.remove_directory_if_identity(&quarantine_name, quarantine_identity);
        return Err(error);
    }
    after_quarantine(&quarantined);

    let duplicate_matches = FileFingerprint::from_entry(&quarantine, OsStr::new("duplicate"))
        .ok()
        .as_ref()
        == Some(&verified.duplicate_fingerprint)
        && FileFingerprint::from_file(&verified.duplicate_file)
            .ok()
            .as_ref()
            == Some(&verified.duplicate_fingerprint);
    let keep_matches = verified.keep_location.matches(&verified.keep_fingerprint)
        && FileFingerprint::from_file(&verified.keep_file)
            .ok()
            .as_ref()
            == Some(&verified.keep_fingerprint);
    if !duplicate_matches || !keep_matches {
        restore_quarantined_duplicate(
            quarantine,
            &quarantine_name,
            quarantine_identity,
            &verified.duplicate_location,
        )?;
        return Ok(false);
    }

    // Close the compared files before deletion (also needed for immediate
    // Windows removal), but retain both directory chains until cleanup ends.
    drop(verified.keep_file);
    drop(verified.duplicate_file);
    if let Err(error) = quarantine.remove_file_if_identity(
        OsStr::new("duplicate"),
        verified.duplicate_fingerprint.identity,
    ) {
        return match restore_quarantined_duplicate(
            quarantine,
            &quarantine_name,
            quarantine_identity,
            &verified.duplicate_location,
        ) {
            Ok(()) => Err(error),
            Err(restore_error) => Err(io::Error::new(
                error.kind(),
                format!("{}; {}", error, restore_error),
            )),
        };
    }
    drop(quarantine);
    parent.remove_directory_if_identity(&quarantine_name, quarantine_identity)?;
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
fn files_byte_equal_at(
    a: BoundFileLocation,
    b: BoundFileLocation,
    cancel_flag: &Arc<AtomicBool>,
) -> io::Result<Option<VerifiedDuplicatePair>> {
    let (fa, before_a) = a.open()?;
    let (fb, before_b) = b.open()?;
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
        keep_location: a,
        duplicate_location: b,
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
    identity: StablePathIdentity,
    directories: Vec<(OsString, StablePathIdentity)>,
}

fn scan_directory(
    dir: &Path,
    access: &DirectoryAccess,
    directories: &mut Vec<(OsString, StablePathIdentity)>,
    tx: &Sender<DedupMessage>,
    cancel_flag: &Arc<AtomicBool>,
    size_map: &mut HashMap<u64, Vec<FileEntry>>,
    scanned: &mut usize,
    before_directory_open: &mut impl FnMut(&Path),
) {
    // Directory-level skip: check if marker files exist INSIDE this directory
    // (matches removeduplicated.js lines 47-50)
    for &marker in DIR_MARKER_FILES {
        if access.child_metadata(OsStr::new(marker)).is_ok() {
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

    let entries = match access.entries() {
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

        let path = dir.join(&entry);

        let metadata = match access.child_metadata(&entry) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if metadata.is_dir() {
            before_directory_open(&path);
            let opened = access
                .open_directory(&entry)
                .and_then(|(directory, child, _)| {
                    if stable_file_identity(&directory)? != metadata.identity() {
                        return Err(io::Error::other("Scanned directory was replaced"));
                    }
                    Ok(child)
                });
            match opened {
                Ok(child) => {
                    directories.push((entry, metadata.identity()));
                    scan_directory(
                        &path,
                        &child,
                        directories,
                        tx,
                        cancel_flag,
                        size_map,
                        scanned,
                        before_directory_open,
                    );
                    directories.pop();
                }
                Err(error) => {
                    let _ = tx.send(DedupMessage::Error(format!(
                        "Cannot safely scan {}: {}",
                        path.display(),
                        error
                    )));
                }
            }
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

            size_map.entry(size).or_default().push(FileEntry {
                path,
                size,
                identity: metadata.identity(),
                directories: directories.clone(),
            });
        }
    }
}

fn compute_md5(
    location: &BoundFileLocation,
    file_size: u64,
    tx: &Sender<DedupMessage>,
    cancel_flag: &Arc<AtomicBool>,
) -> Option<String> {
    let path = &location.path;
    let (file, before) = match location.open() {
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
        || !location.matches(&FileFingerprint::from_file(&file).ok()?)
    {
        let _ = tx.send(DedupMessage::Error(format!(
            "File changed while hashing: {}",
            path.display()
        )));
        return None;
    }

    Some(format!("{:032x}", hasher.finalize()))
}

pub(crate) fn run_dedup(
    target_path: PathBuf,
    authorization: DirectoryAuthorization,
    tx: Sender<DedupMessage>,
    cancel_flag: Arc<AtomicBool>,
) {
    let root = match open_authorized_directory(&authorization, "Duplicate-removal root") {
        Ok((_, access)) => Arc::new(DedupRoot {
            path: authorization.resolved_path().to_path_buf(),
            access,
        }),
        Err(error) => {
            let _ = tx.send(DedupMessage::Error(format!(
                "Cannot safely scan {}: {}",
                target_path.display(),
                error
            )));
            let _ = tx.send(DedupMessage::Complete);
            return;
        }
    };
    // Phase 1: Scan
    let _ = tx.send(DedupMessage::Phase(DedupPhase::Scanning));
    let _ = tx.send(DedupMessage::Log("Scanning files...".into()));

    let mut size_map: HashMap<u64, Vec<FileEntry>> = HashMap::new();
    let mut scanned: usize = 0;

    scan_directory(
        &root.path,
        &root.access,
        &mut Vec::new(),
        &tx,
        &cancel_flag,
        &mut size_map,
        &mut scanned,
        &mut |_| {},
    );

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

    let mut hash_map: HashMap<String, Vec<&FileEntry>> = HashMap::new();

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

            let location = match BoundFileLocation::bind(&root, entry) {
                Ok(location) => location,
                Err(error) => {
                    let _ = tx.send(DedupMessage::Error(format!(
                        "Cannot safely hash {}: {}",
                        entry.path.display(),
                        error
                    )));
                    continue;
                }
            };
            if let Some(hash) = compute_md5(&location, entry.size, &tx, &cancel_flag) {
                let _ = tx.send(DedupMessage::Log(format!(
                    "{} {} % {} {}",
                    hash,
                    pct,
                    entry.size,
                    entry.path.display()
                )));
                hash_map.entry(hash).or_default().push(entry);
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
    let dup_groups: Vec<(&String, &Vec<&FileEntry>)> = hash_map
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
        let keep_entry = paths[0];
        let keep_path = &keep_entry.path;
        for duplicate_entry in paths.iter().skip(1) {
            let dup_path = &duplicate_entry.path;
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
            let comparison = (|| {
                let keep = BoundFileLocation::bind(&root, keep_entry)?;
                let duplicate = BoundFileLocation::bind(&root, duplicate_entry)?;
                files_byte_equal_at(keep, duplicate, &cancel_flag)
            })();
            let verified = match comparison {
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

    fn test_root(path: &Path) -> Arc<DedupRoot> {
        let authorization =
            crate::services::file_ops::capture_directory_authorization(path).unwrap();
        let (_, access) = open_authorized_directory(&authorization, "test root").unwrap();
        Arc::new(DedupRoot {
            path: authorization.resolved_path().to_path_buf(),
            access,
        })
    }

    fn files_byte_equal(
        a: &Path,
        b: &Path,
        cancel: &Arc<AtomicBool>,
    ) -> io::Result<Option<VerifiedDuplicatePair>> {
        let bind = |path: &Path| {
            let root = test_root(path.parent().unwrap());
            let metadata = root.access.child_metadata(path.file_name().unwrap())?;
            BoundFileLocation::bind(
                &root,
                &FileEntry {
                    path: path.to_path_buf(),
                    size: metadata.len(),
                    identity: metadata.identity(),
                    directories: Vec::new(),
                },
            )
        };
        files_byte_equal_at(bind(a)?, bind(b)?, cancel)
    }

    #[cfg(unix)]
    #[test]
    fn scan_does_not_follow_directory_replaced_after_lstat() {
        let temp = tempfile::tempdir().unwrap();
        let selected = temp.path().join("selected");
        let outside = temp.path().join("outside");
        fs::create_dir_all(selected.join("child")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("one"), b"same").unwrap();
        fs::write(outside.join("two"), b"same").unwrap();
        let root = test_root(&selected);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut sizes = HashMap::new();
        let mut scanned = 0;
        scan_directory(
            &selected,
            &root.access,
            &mut Vec::new(),
            &tx,
            &Arc::new(AtomicBool::new(false)),
            &mut sizes,
            &mut scanned,
            &mut |path| {
                if path == selected.join("child") {
                    fs::rename(path, selected.join("retained-child")).unwrap();
                    std::os::unix::fs::symlink(&outside, path).unwrap();
                }
            },
        );
        assert!(sizes.is_empty());
        assert_eq!(scanned, 0);
        assert!(rx
            .try_iter()
            .any(|message| matches!(message, DedupMessage::Error(_))));
        assert!(outside.join("one").exists());
        assert!(outside.join("two").exists());
    }

    #[cfg(unix)]
    #[test]
    fn scanned_ancestry_cannot_be_rebound_through_a_link() {
        let temp = tempfile::tempdir().unwrap();
        let selected = temp.path().join("selected");
        let outside = temp.path().join("outside");
        fs::create_dir_all(selected.join("child")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(selected.join("child/item"), b"same").unwrap();
        fs::write(outside.join("item"), b"same").unwrap();
        let root = test_root(&selected);
        let (tx, _) = std::sync::mpsc::channel();
        let mut sizes = HashMap::new();
        scan_directory(
            &selected,
            &root.access,
            &mut Vec::new(),
            &tx,
            &Arc::new(AtomicBool::new(false)),
            &mut sizes,
            &mut 0,
            &mut |_| {},
        );
        fs::rename(selected.join("child"), selected.join("retained-child")).unwrap();
        std::os::unix::fs::symlink(&outside, selected.join("child")).unwrap();
        assert!(BoundFileLocation::bind(&root, &sizes[&4][0]).is_err());
        assert!(outside.join("item").exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_replaced_after_parent_binding_is_rejected_when_opened() {
        let temp = tempfile::tempdir().unwrap();
        let root = test_root(temp.path());
        let path = temp.path().join("item");
        fs::write(&path, b"old").unwrap();
        let metadata = root.access.child_metadata(OsStr::new("item")).unwrap();
        let entry = FileEntry {
            path: path.clone(),
            size: metadata.len(),
            identity: metadata.identity(),
            directories: Vec::new(),
        };
        let location = BoundFileLocation::bind(&root, &entry).unwrap();
        fs::rename(&path, temp.path().join("retained")).unwrap();
        fs::write(&path, b"new").unwrap();
        assert!(location.open().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn parent_replacement_after_comparison_prevents_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let selected = temp.path().join("selected");
        let outside = temp.path().join("outside");
        fs::create_dir(&selected).unwrap();
        fs::create_dir(&outside).unwrap();
        for dir in [&selected, &outside] {
            fs::write(dir.join("keep"), b"same").unwrap();
            fs::write(dir.join("duplicate"), b"same").unwrap();
        }
        let keep = selected.join("keep");
        let duplicate = selected.join("duplicate");
        let verified = files_byte_equal(&keep, &duplicate, &Arc::new(AtomicBool::new(false)))
            .unwrap()
            .unwrap();
        fs::rename(&selected, temp.path().join("retained")).unwrap();
        std::os::unix::fs::symlink(&outside, &selected).unwrap();
        assert!(!delete_verified_duplicate(verified, &keep, &duplicate).unwrap());
        assert_eq!(fs::read(outside.join("duplicate")).unwrap(), b"same");
        assert!(temp.path().join("retained/duplicate").exists());
    }

    #[test]
    fn normal_duplicate_removal_keeps_one_file_and_cleans_quarantine() {
        let temp = tempfile::tempdir().unwrap();
        let keep = temp.path().join("keep");
        let duplicate = temp.path().join("duplicate");
        fs::write(&keep, b"same").unwrap();
        fs::write(&duplicate, b"same").unwrap();
        let verified = files_byte_equal(&keep, &duplicate, &Arc::new(AtomicBool::new(false)))
            .unwrap()
            .unwrap();
        assert!(delete_verified_duplicate(verified, &keep, &duplicate).unwrap());
        assert_eq!(fs::read(&keep).unwrap(), b"same");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn dedup_pipeline_handles_nested_files_and_skips_linked_directories() {
        let temp = tempfile::tempdir().unwrap();
        let selected = temp.path().join("selected");
        let outside = temp.path().join("outside");
        fs::create_dir_all(selected.join("sub")).unwrap();
        fs::create_dir_all(selected.join("ignored")).unwrap();
        fs::create_dir(&outside).unwrap();
        let names = ["one", "sub/two", "sub/three"];
        for name in names {
            fs::write(selected.join(name), b"identical bytes").unwrap();
        }
        fs::write(selected.join("ignored/.ignoresorting"), b"").unwrap();
        fs::write(selected.join("ignored/keep"), b"identical bytes").unwrap();
        fs::write(outside.join("keep"), b"identical bytes").unwrap();
        std::os::unix::fs::symlink(&outside, selected.join("linked")).unwrap();
        let authorization =
            crate::services::file_ops::capture_directory_authorization(&selected).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        run_dedup(
            selected.clone(),
            authorization,
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        let messages: Vec<_> = rx.try_iter().collect();
        assert!(!messages
            .iter()
            .any(|message| matches!(message, DedupMessage::Error(_))));
        assert!(messages
            .iter()
            .any(|message| matches!(message, DedupMessage::Stats { duplicates: 2, .. })));
        assert_eq!(
            names
                .iter()
                .filter(|name| selected.join(name).exists())
                .count(),
            1
        );
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"identical bytes");
        assert_eq!(
            fs::read(selected.join("ignored/keep")).unwrap(),
            b"identical bytes"
        );
        assert!(fs::symlink_metadata(selected.join("linked"))
            .unwrap()
            .is_symlink());
    }

    #[test]
    fn worker_rejects_root_replaced_after_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let selected = temp.path().join("selected");
        fs::create_dir(&selected).unwrap();
        let authorization =
            crate::services::file_ops::capture_directory_authorization(&selected).unwrap();
        fs::rename(&selected, temp.path().join("retained")).unwrap();
        fs::create_dir(&selected).unwrap();
        fs::write(selected.join("one"), b"same").unwrap();
        fs::write(selected.join("two"), b"same").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        run_dedup(
            selected.clone(),
            authorization,
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(rx
            .try_iter()
            .any(|message| matches!(message, DedupMessage::Error(_))));
        assert_eq!(fs::read_dir(selected).unwrap().count(), 2);
    }

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
