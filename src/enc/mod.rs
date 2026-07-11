pub mod crypto;
pub mod error;
pub mod naming;

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use base64::Engine;
use md5::{Digest, Md5};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::services::file_ops::{
    open_directory_for_read, open_regular_file_no_follow, prepare_file_deletion, rename_noreplace,
    stable_file_identity, stable_path_identity, ProgressMessage, StablePathIdentity,
};
use crypto::{
    decrypt_chunk_streaming, derive_key, generate_iv, generate_salt, load_key, read_header,
    write_header, ChunkEncryptor,
};
use error::CokacencError;

const READ_BUF_SIZE: usize = 64 * 1024; // 64KB
const MAX_METADATA_LEN: usize = 1024 * 1024; // metadata is normally well below 1KB

// ─── Chunk metadata (embedded inside each encrypted chunk) ─────────────

#[derive(Debug, Serialize, Deserialize)]
struct ChunkMetadata {
    #[serde(rename = "v")]
    version: u32,
    #[serde(rename = "group")]
    group_id: String,
    #[serde(rename = "name")]
    filename: String,
    #[serde(rename = "size")]
    file_size: u64,
    #[serde(rename = "md5")]
    file_md5: String,
    #[serde(rename = "mtime")]
    modified: i64,
    #[serde(rename = "perm")]
    permissions: u32,
    #[serde(rename = "chunks")]
    total_chunks: usize,
    #[serde(rename = "idx")]
    chunk_index: usize,
    #[serde(rename = "offset")]
    chunk_offset: u64,
    #[serde(rename = "len")]
    chunk_data_size: u64,
}

// ─── File info gathered in first pass ──────────────────────────────────

struct FileInfo {
    size: u64,
    md5: String,
    modified: i64,
    modified_time: Option<std::time::SystemTime>,
    permissions: u32,
    identity: StablePathIdentity,
}

/// Hash the exact byte stream delivered to a parser/decryptor. Re-reading the
/// path afterwards is insufficient because a concurrent same-inode writer may
/// have changed it between the two reads.
struct Sha256Reader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R> Sha256Reader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> (R, [u8; 32]) {
        (self.inner, self.hasher.finalize().into())
    }
}

impl<R: Read> Read for Sha256Reader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

/// Hash the exact ciphertext bytes accepted by the output file. The expected
/// archive digest must come from the write stream rather than a later reread:
/// otherwise a concurrent writer could corrupt a chunk before that reread and
/// make the corrupted bytes the new baseline.
struct Sha256Writer<W> {
    inner: W,
    hasher: Sha256,
}

impl<W> Sha256Writer<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> (W, [u8; 32]) {
        (self.inner, self.hasher.finalize().into())
    }
}

impl<W: Write> Write for Sha256Writer<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn gather_file_info(file: &mut File, use_md5: bool) -> Result<FileInfo, CokacencError> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(CokacencError::Other(
            "Only regular files can be encrypted".to_string(),
        ));
    }
    let size = metadata.len();

    let modified_time = metadata.modified().ok();
    let modified = modified_time
        .as_ref()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    };
    #[cfg(not(unix))]
    let permissions = 0u32;

    let md5 = if use_md5 {
        // Compute MD5 (first pass)
        let mut reader = BufReader::new(&mut *file);
        let mut hasher = Md5::new();
        let mut buf = [0u8; READ_BUF_SIZE];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        format!("{:032x}", hasher.finalize())
    } else {
        String::new()
    };
    file.seek(SeekFrom::Start(0))?;

    Ok(FileInfo {
        size,
        md5,
        modified,
        modified_time,
        permissions,
        identity: stable_file_identity(file)?,
    })
}

fn open_source_file(path: &Path) -> Result<File, CokacencError> {
    open_regular_file_no_follow(path)
        .map(|(file, _)| file)
        .map_err(CokacencError::from)
}

/// Open an encrypted chunk without following a final-component symlink or
/// Windows reparse point.  Discovery and open are deliberately checked
/// independently because an entry can be replaced after `read_dir` returns.
fn open_encrypted_chunk(path: &Path) -> Result<(File, StablePathIdentity), CokacencError> {
    let (file, _) = open_regular_file_no_follow(path)?;
    let identity = stable_file_identity(&file)?;
    Ok((file, identity))
}

// ─── MetadataSplitWriter (extracts metadata from decrypted stream) ─────

enum SplitState {
    ReadingLen,
    ReadingMeta,
    Data,
}

/// Writer that splits the decrypted plaintext into metadata + file data.
/// Plaintext format: [4B meta_len LE u32][metadata JSON][file data...]
/// The metadata is buffered; file data is forwarded to the inner writer.
struct MetadataSplitWriter<'a, W: Write> {
    state: SplitState,
    len_buf: [u8; 4],
    len_filled: usize,
    meta_buf: Vec<u8>,
    meta_len: usize,
    inner: &'a mut W,
}

impl<'a, W: Write> MetadataSplitWriter<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        Self {
            state: SplitState::ReadingLen,
            len_buf: [0u8; 4],
            len_filled: 0,
            meta_buf: Vec::new(),
            meta_len: 0,
            inner,
        }
    }

    fn take_metadata_bytes(&mut self) -> Result<Vec<u8>, CokacencError> {
        match self.state {
            SplitState::Data => Ok(std::mem::take(&mut self.meta_buf)),
            _ => Err(CokacencError::MetadataParse(
                "Incomplete metadata in chunk".to_string(),
            )),
        }
    }
}

impl<W: Write> Write for MetadataSplitWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let total = buf.len();
        let mut pos = 0;

        while pos < total {
            match self.state {
                SplitState::ReadingLen => {
                    let need = 4 - self.len_filled;
                    let take = need.min(total - pos);
                    self.len_buf[self.len_filled..self.len_filled + take]
                        .copy_from_slice(&buf[pos..pos + take]);
                    self.len_filled += take;
                    pos += take;
                    if self.len_filled == 4 {
                        self.meta_len = u32::from_le_bytes(self.len_buf) as usize;
                        if self.meta_len > MAX_METADATA_LEN {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!(
                                    "Chunk metadata is too large: {} bytes (max {})",
                                    self.meta_len, MAX_METADATA_LEN
                                ),
                            ));
                        }
                        self.meta_buf = Vec::with_capacity(self.meta_len);
                        if self.meta_len == 0 {
                            self.state = SplitState::Data;
                        } else {
                            self.state = SplitState::ReadingMeta;
                        }
                    }
                }
                SplitState::ReadingMeta => {
                    let need = self.meta_len - self.meta_buf.len();
                    let take = need.min(total - pos);
                    self.meta_buf.extend_from_slice(&buf[pos..pos + take]);
                    pos += take;
                    if self.meta_buf.len() == self.meta_len {
                        self.state = SplitState::Data;
                    }
                }
                SplitState::Data => {
                    self.inner.write_all(&buf[pos..])?;
                    pos = total;
                }
            }
        }

        Ok(total)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ─── TeeWriter (dual write to file + MD5 hasher) ──────────────────────

struct TeeWriter<'a, W: Write> {
    file: &'a mut W,
    hasher: &'a mut Md5,
    bytes_written: &'a mut u64,
}

impl<W: Write> Write for TeeWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        *self.bytes_written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

// ─── Key management ────────────────────────────────────────────────────

/// Ensure the encryption key file exists at ~/.cokacdir/credential/cokacenc.key.
/// Creates the directory and key file if they don't exist.
/// Returns key bytes read from the exact regular file that was validated.
pub fn ensure_key() -> Result<Vec<u8>, CokacencError> {
    let home = dirs::home_dir()
        .ok_or_else(|| CokacencError::Other("Cannot determine home directory".to_string()))?;
    ensure_key_in(&home)
}

fn ensure_key_in(home: &Path) -> Result<Vec<u8>, CokacencError> {
    let cred_dir = home.join(".cokacdir").join("credential");

    fs::create_dir_all(&cred_dir)?;
    let (directory, _, directory_metadata) = open_directory_for_read(&cred_dir)?;
    if !directory_metadata.is_dir() {
        return Err(CokacencError::Other(format!(
            "Credential path is not a real directory: {}",
            cred_dir.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    }

    let key_path = cred_dir.join("cokacenc.key");

    if !key_path.exists() {
        let mut raw = vec![0u8; 4096];
        rand::thread_rng().fill_bytes(&mut raw);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);

        let mut nonce = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut nonce);
        let temp_path = cred_dir.join(format!(".cokacenc.{}.tmp", hex::encode(nonce)));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut temp = options.open(&temp_path)?;
        let write_result = (|| -> Result<(), CokacencError> {
            temp.write_all(encoded.as_bytes())?;
            temp.sync_all()?;
            drop(temp);
            match publish_noclobber(&temp_path, &key_path) {
                Ok(()) => Ok(()),
                Err(CokacencError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Ok(())
                }
                Err(e) => Err(e),
            }
        })();
        let _ = fs::remove_file(&temp_path);
        write_result?;
    }

    let (key_file, _) = open_regular_file_no_follow(&key_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        key_file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    load_key(key_file).map_err(|error| {
        CokacencError::Other(format!(
            "Encryption key file is empty or unreadable ({}): {}",
            key_path.display(),
            error
        ))
    })
}

// ─── Pack (encrypt) ────────────────────────────────────────────────────

/// Pack (encrypt) all eligible files in a directory with progress reporting.
/// Uses 2-pass: first pass computes MD5+metadata, second pass encrypts.
/// Each chunk embeds full metadata. After encryption, original files are deleted.
pub fn pack_directory_with_progress(
    dir: &Path,
    password: &[u8],
    tx: Sender<ProgressMessage>,
    cancel_flag: Arc<AtomicBool>,
    split_size_mb: u64,
    use_md5: bool,
) {
    let split_size = if split_size_mb == 0 {
        u64::MAX
    } else {
        split_size_mb.checked_mul(1024 * 1024).unwrap_or(u64::MAX)
    };

    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                let path = e.path();
                if !fs::symlink_metadata(&path)
                    .map(|metadata| metadata.file_type().is_file())
                    .unwrap_or(false)
                {
                    return false;
                }
                let name = e.file_name().to_string_lossy().to_string();
                !name.ends_with(naming::EXT) && !name.starts_with('.')
            })
            .collect(),
        Err(e) => {
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Read dir error: {}", e),
            ));
            let _ = tx.send(ProgressMessage::Completed(0, 1));
            return;
        }
    };

    entries.sort_by_key(|e| e.file_name());

    if entries.is_empty() {
        let _ = tx.send(ProgressMessage::Completed(0, 0));
        return;
    }

    let total_files = entries.len();
    let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

    let mut success_count = 0;
    let mut failure_count = 0;

    for (i, entry) in entries.iter().enumerate() {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }

        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        let _ = tx.send(ProgressMessage::FileStarted(name.clone()));

        match pack_file(&path, &name, dir, password, split_size, use_md5) {
            Ok(packed_source) => match remove_packed_source(&path, packed_source) {
                Ok(PackedSourceRemoval::Removed) => {
                    success_count += 1;
                    let _ = tx.send(ProgressMessage::FileCompleted(name));
                }
                Ok(PackedSourceRemoval::RemovedButDirectorySyncFailed(error)) => {
                    failure_count += 1;
                    let _ = tx.send(ProgressMessage::Error(
                        name,
                        format!(
                            "Encrypted and removed source, but failed to persist the directory update: {}",
                            error
                        ),
                    ));
                }
                Err(e) => {
                    failure_count += 1;
                    let _ = tx.send(ProgressMessage::Error(
                        name,
                        format!("Encrypted but retained source: {}", e),
                    ));
                }
            },
            Err(e) => {
                failure_count += 1;
                let _ = tx.send(ProgressMessage::Error(name, e.to_string()));
            }
        }

        let _ = tx.send(ProgressMessage::TotalProgress(i + 1, total_files, 0, 0));
    }

    let _ = tx.send(ProgressMessage::Completed(success_count, failure_count));
}

/// Pack a single file using 2-pass approach.
/// Pass 1: gather file info (MD5, size, mtime, permissions).
/// Pass 2: encrypt with metadata embedded in each chunk.
struct PackedSource {
    info: FileInfo,
    encrypted_sha256: [u8; 32],
    chunks: Vec<PackedChunk>,
    // Keeping the source inode open until quarantine verification prevents an
    // unlinked inode from being reused between the final check and deletion.
    handle: File,
}

/// A generated archive chunk remains bound to the exact filesystem object
/// created by this pack operation until the plaintext source has been
/// removed.  Keeping the handle alive prevents object-id reuse, while the
/// digest detects same-inode writes between encryption and source deletion.
struct PackedChunk {
    path: PathBuf,
    identity: StablePathIdentity,
    expected_sha256: Option<[u8; 32]>,
    handle: File,
}

fn pack_file(
    file_path: &Path,
    original_name: &str,
    out_dir: &Path,
    password: &[u8],
    split_size: u64,
    use_md5: bool,
) -> Result<PackedSource, CokacencError> {
    // Shift+E must emit the original cokacdir v2 archive format. The visible
    // behavior is bigger than "can this process decrypt its own output": old
    // encrypted directories, the file panel's original-name display, and
    // cokacdircode_old all depend on this exact v2 header/metadata/filename
    // contract. Do not silently switch this writer to a new format.
    // ── Pass 1: gather info ──
    let mut file = open_source_file(file_path)?;
    let info = gather_file_info(&mut file, use_md5)?;

    let group_id = loop {
        let id = naming::generate_group_id();
        if !naming::group_id_exists(out_dir, &id) {
            break id;
        }
    };
    let kp = naming::key_prefix(password);
    let total_chunks = if info.size == 0 {
        1
    } else {
        ((info.size - 1) / split_size + 1) as usize
    };

    // ── Pass 2: encrypt ──
    let mut reader = BufReader::new(file);
    let mut read_buf = [0u8; READ_BUF_SIZE];
    let mut created_chunks: Vec<PackedChunk> = Vec::new();
    let mut second_pass_hasher = use_md5.then(Md5::new);
    // This digest is an internal deletion-safety invariant, not archive
    // metadata.  It is therefore always computed even when the user disables
    // the optional, compatibility-preserving MD5 field.
    let mut encrypted_hasher = Sha256::new();

    let result = (|| -> Result<(), CokacencError> {
        for chunk_idx in 0..total_chunks {
            let chunk_offset = chunk_idx as u64 * split_size;
            let chunk_data_size = if info.size == 0 {
                0
            } else {
                split_size.min(info.size - chunk_offset)
            };

            let metadata = ChunkMetadata {
                version: crypto::VERSION,
                group_id: group_id.clone(),
                filename: original_name.to_string(),
                file_size: info.size,
                file_md5: info.md5.clone(),
                modified: info.modified,
                permissions: info.permissions,
                total_chunks,
                chunk_index: chunk_idx,
                chunk_offset,
                chunk_data_size,
            };

            let chunk_path = naming::chunk_filename(out_dir, &kp, &group_id, chunk_idx)?;
            let mut chunk_options = OpenOptions::new();
            chunk_options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                chunk_options.mode(0o600);
            }
            let chunk_file = chunk_options.open(&chunk_path)?;
            let chunk_identity = stable_file_identity(&chunk_file)?;
            let retained_handle = chunk_file.try_clone()?;
            created_chunks.push(PackedChunk {
                path: chunk_path.clone(),
                identity: chunk_identity,
                expected_sha256: None,
                handle: retained_handle,
            });
            if stable_path_identity(&chunk_path)? != chunk_identity {
                return Err(CokacencError::Other(format!(
                    "Encrypted output chunk was replaced immediately after creation: '{}'",
                    chunk_path.display()
                )));
            }
            let mut writer = BufWriter::new(Sha256Writer::new(chunk_file));

            let salt = generate_salt();
            let iv = generate_iv();
            let key = derive_key(password, &salt);
            // The header filename must be the original plaintext filename.
            // The file panel reads this header without decrypting to show
            // "payload.txt" instead of only the opaque .cokacenc chunk name.
            write_header(&mut writer, &salt, &iv, original_name)?;

            let mut enc = ChunkEncryptor::new(&key, &iv);

            // Write metadata length + metadata into encrypted stream
            let meta_bytes = serde_json::to_vec(&metadata)
                .map_err(|e| CokacencError::Other(format!("JSON serialize: {}", e)))?;
            let meta_len_bytes = (meta_bytes.len() as u32).to_le_bytes();

            let encrypted = enc.update(&meta_len_bytes);
            writer.write_all(encrypted)?;
            let encrypted = enc.update(&meta_bytes);
            writer.write_all(encrypted)?;

            // Write file data portion
            let mut remaining = chunk_data_size;
            while remaining > 0 {
                let to_read = (READ_BUF_SIZE as u64).min(remaining) as usize;
                let n = reader.read(&mut read_buf[..to_read])?;
                if n == 0 {
                    return Err(CokacencError::Other(format!(
                        "Source file ended while encrypting chunk {} ({} bytes still expected)",
                        chunk_idx, remaining
                    )));
                }
                if let Some(hasher) = second_pass_hasher.as_mut() {
                    hasher.update(&read_buf[..n]);
                }
                encrypted_hasher.update(&read_buf[..n]);
                let encrypted = enc.update(&read_buf[..n]);
                writer.write_all(encrypted)?;
                remaining -= n as u64;
            }

            let final_block = enc.finalize();
            writer.write_all(&final_block)?;
            writer.flush()?;
            let digesting_writer = writer
                .into_inner()
                .map_err(|error| CokacencError::Io(error.into_error()))?;
            let (chunk_file, intended_sha256) = digesting_writer.finish();
            chunk_file.sync_all()?;
            drop(chunk_file);

            let created = created_chunks.last_mut().expect("chunk was just recorded");
            created.expected_sha256 = Some(intended_sha256);
            verify_packed_chunk(created)?;
        }

        sync_directory(out_dir)?;
        verify_packed_chunks(&mut created_chunks)?;

        let mut extra = [0u8; 1];
        if reader.read(&mut extra)? != 0 {
            return Err(CokacencError::Other(
                "Source file grew while it was being encrypted".to_string(),
            ));
        }
        if let Some(hasher) = second_pass_hasher.take() {
            let actual = format!("{:032x}", hasher.finalize());
            if actual != info.md5 {
                return Err(CokacencError::Other(
                    "Source file changed while it was being encrypted".to_string(),
                ));
            }
        }

        let final_metadata = fs::symlink_metadata(file_path)?;
        if !final_metadata.file_type().is_file()
            || final_metadata.len() != info.size
            || final_metadata.modified().ok() != info.modified_time
        {
            return Err(CokacencError::Other(
                "Source file changed while it was being encrypted".to_string(),
            ));
        }
        if stable_path_identity(file_path)? != info.identity {
            return Err(CokacencError::Other(
                "Source file was replaced while it was being encrypted".to_string(),
            ));
        }

        Ok(())
    })();

    if let Err(error) = result {
        let mut cleanup_notes = cleanup_created_chunks(&created_chunks);
        if let Err(sync_error) = sync_directory(out_dir) {
            cleanup_notes.push(format!(
                "the archive directory cleanup could not be persisted: {}",
                sync_error
            ));
        }
        if cleanup_notes.is_empty() {
            return Err(error);
        }
        return Err(CokacencError::Other(format!(
            "{}; cleanup details: {}",
            error,
            cleanup_notes.join("; ")
        )));
    }

    Ok(PackedSource {
        info,
        encrypted_sha256: encrypted_hasher.finalize().into(),
        chunks: created_chunks,
        handle: reader.into_inner(),
    })
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CokacencError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), CokacencError> {
    Ok(())
}

fn create_quarantine_dir(parent: &Path) -> Result<PathBuf, CokacencError> {
    for _ in 0..100 {
        let mut random = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut random);
        let path = parent.join(format!(".cokacenc-delete-{}", hex::encode(random)));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(CokacencError::Other(
        "Could not create a unique source quarantine directory".to_string(),
    ))
}

fn metadata_matches_packed_source(path: &Path, metadata: &fs::Metadata, info: &FileInfo) -> bool {
    if !metadata.file_type().is_file()
        || metadata.len() != info.size
        || metadata.modified().ok() != info.modified_time
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() != info.permissions {
            return false;
        }
    }
    stable_path_identity(path).ok().as_ref() == Some(&info.identity)
}

fn content_metadata_unchanged(before: &fs::Metadata, after: &fs::Metadata) -> bool {
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
        before.file_size() == after.file_size()
            && before.last_write_time() == after.last_write_time()
            && before.creation_time() == after.creation_time()
            && before.file_attributes() == after.file_attributes()
    }
    #[cfg(not(any(unix, windows)))]
    {
        before.len() == after.len() && before.modified().ok() == after.modified().ok()
    }
}

fn sha256_file(file: &mut File) -> Result<[u8; 32], CokacencError> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; READ_BUF_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn verify_packed_chunk(chunk: &mut PackedChunk) -> Result<(), CokacencError> {
    let expected_sha256 = chunk.expected_sha256.ok_or_else(|| {
        CokacencError::Other(format!(
            "Encrypted output chunk was not completely sealed: '{}'",
            chunk.path.display()
        ))
    })?;
    let path_metadata = fs::symlink_metadata(&chunk.path)?;
    if !path_metadata.file_type().is_file()
        || stable_file_identity(&chunk.handle)? != chunk.identity
        || stable_path_identity(&chunk.path)? != chunk.identity
    {
        return Err(CokacencError::Other(format!(
            "Encrypted output chunk changed identity before source deletion: '{}'",
            chunk.path.display()
        )));
    }

    let before = chunk.handle.metadata()?;
    let actual_sha256 = sha256_file(&mut chunk.handle)?;
    let after = chunk.handle.metadata()?;
    if !content_metadata_unchanged(&before, &after)
        || !after.file_type().is_file()
        || stable_file_identity(&chunk.handle)? != chunk.identity
        || stable_path_identity(&chunk.path)? != chunk.identity
        || actual_sha256 != expected_sha256
    {
        return Err(CokacencError::Other(format!(
            "Encrypted output chunk bytes changed before source deletion: '{}'",
            chunk.path.display()
        )));
    }
    Ok(())
}

fn verify_packed_chunks(chunks: &mut [PackedChunk]) -> Result<(), CokacencError> {
    for chunk in chunks {
        verify_packed_chunk(chunk)?;
    }
    Ok(())
}

/// Remove only archive objects that are still the exact files allocated by
/// this pack attempt. A racing replacement is deliberately retained.
fn cleanup_created_chunks(chunks: &[PackedChunk]) -> Vec<String> {
    let mut notes = Vec::new();
    for chunk in chunks {
        if stable_file_identity(&chunk.handle).ok() != Some(chunk.identity) {
            notes.push(format!(
                "generated chunk handle identity could not be verified: '{}'",
                chunk.path.display()
            ));
            continue;
        }
        match stable_path_identity(&chunk.path) {
            Ok(identity) if identity == chunk.identity => {
                if let Err(error) = prepare_file_deletion(&chunk.path, chunk.identity)
                    .and_then(|deletion| deletion.delete())
                {
                    notes.push(format!(
                        "generated chunk '{}' could not be removed safely: {}",
                        chunk.path.display(),
                        error
                    ));
                }
            }
            Ok(_) => notes.push(format!(
                "generated chunk path '{}' was replaced; the replacement was preserved",
                chunk.path.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => notes.push(format!(
                "generated chunk '{}' was moved or removed before cleanup",
                chunk.path.display()
            )),
            Err(error) => notes.push(format!(
                "generated chunk '{}' could not be identified for cleanup: {}",
                chunk.path.display(),
                error
            )),
        }
    }
    notes
}

/// Rebind the quarantined pathname to the original open object and prove that
/// its complete current byte stream is exactly the stream that was encrypted.
/// Metadata is sampled around hashing so a concurrent writer cannot quietly
/// produce a mixed read while restoring only the modification time.
fn verify_quarantined_packed_source(
    path: &Path,
    handle: &mut File,
    info: &FileInfo,
    encrypted_sha256: &[u8; 32],
) -> Result<(), CokacencError> {
    let path_metadata = fs::symlink_metadata(path)?;
    if !metadata_matches_packed_source(path, &path_metadata, info)
        || stable_file_identity(handle)? != info.identity
    {
        return Err(CokacencError::Other(
            "Source was replaced during encryption or had its metadata changed".to_string(),
        ));
    }

    let before = handle.metadata()?;
    let actual_sha256 = sha256_file(handle)?;
    let after = handle.metadata()?;
    if !content_metadata_unchanged(&before, &after)
        || after.len() != info.size
        || stable_file_identity(handle)? != info.identity
        || stable_path_identity(path)? != info.identity
        || actual_sha256 != *encrypted_sha256
    {
        return Err(CokacencError::Other(
            "Source bytes changed while or after they were encrypted".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
enum PackedSourceRemoval {
    Removed,
    RemovedButDirectorySyncFailed(CokacencError),
}

fn restore_quarantined_source_after_failure(
    original: &Path,
    quarantined: &Path,
    quarantine_dir: &Path,
    cause: CokacencError,
) -> CokacencError {
    match rename_noreplace(quarantined, original) {
        Ok(()) => {
            let cleanup_error = fs::remove_dir(quarantine_dir).err();
            let sync_error = original
                .parent()
                .and_then(|parent| sync_directory(parent).err());
            let mut detail = format!("{}; the source was restored", cause);
            if let Some(error) = cleanup_error {
                detail.push_str(&format!(
                    "; the empty recovery directory '{}' could not be removed: {}",
                    quarantine_dir.display(),
                    error
                ));
            }
            if let Some(error) = sync_error {
                detail.push_str(&format!(
                    "; the restored directory update could not be persisted: {}",
                    error
                ));
            }
            CokacencError::Other(detail)
        }
        Err(restore_error) => CokacencError::Other(format!(
            "{}; automatic restore failed: {}. The source is preserved at '{}'",
            cause,
            restore_error,
            quarantined.display()
        )),
    }
}

fn remove_packed_source(
    path: &Path,
    packed: PackedSource,
) -> Result<PackedSourceRemoval, CokacencError> {
    remove_packed_source_impl(path, packed, |_| {})
}

fn remove_packed_source_impl<F>(
    path: &Path,
    packed: PackedSource,
    after_initial_verification: F,
) -> Result<PackedSourceRemoval, CokacencError>
where
    F: FnOnce(&Path),
{
    let parent = path.parent().ok_or_else(|| {
        CokacencError::Other(format!("Source path has no parent: {}", path.display()))
    })?;
    let quarantine_dir = create_quarantine_dir(parent)?;
    let quarantine_path = quarantine_dir.join("source");

    if let Err(error) = rename_noreplace(path, &quarantine_path) {
        let _ = fs::remove_dir(&quarantine_dir);
        return Err(error.into());
    }

    // Persist the recoverable relocation before making any destructive
    // decision. If either directory cannot be synced, restore the source.
    if let Err(error) = sync_directory(&quarantine_dir).and_then(|()| sync_directory(parent)) {
        return Err(restore_quarantined_source_after_failure(
            path,
            &quarantine_path,
            &quarantine_dir,
            CokacencError::Other(format!(
                "Cannot persist source quarantine {}: {}",
                quarantine_path.display(),
                error
            )),
        ));
    }

    let PackedSource {
        info,
        encrypted_sha256,
        mut chunks,
        mut handle,
    } = packed;
    if let Err(error) =
        verify_quarantined_packed_source(&quarantine_path, &mut handle, &info, &encrypted_sha256)
    {
        return Err(restore_quarantined_source_after_failure(
            path,
            &quarantine_path,
            &quarantine_dir,
            error,
        ));
    }
    if let Err(error) = verify_packed_chunks(&mut chunks) {
        return Err(restore_quarantined_source_after_failure(
            path,
            &quarantine_path,
            &quarantine_dir,
            error,
        ));
    }

    let deletion = match prepare_file_deletion(&quarantine_path, info.identity) {
        Ok(deletion) => deletion,
        Err(error) => {
            return Err(restore_quarantined_source_after_failure(
                path,
                &quarantine_path,
                &quarantine_dir,
                CokacencError::Other(format!(
                    "Cannot prepare verified source deletion {}: {}",
                    quarantine_path.display(),
                    error
                )),
            ));
        }
    };

    // Re-hash after deletion has been bound to the quarantined object. This
    // closes the former final-check -> quarantine window and also catches a
    // writer that raced the first quarantined verification.
    after_initial_verification(&quarantine_path);
    if let Err(error) =
        verify_quarantined_packed_source(&quarantine_path, &mut handle, &info, &encrypted_sha256)
    {
        drop(deletion);
        return Err(restore_quarantined_source_after_failure(
            path,
            &quarantine_path,
            &quarantine_dir,
            error,
        ));
    }
    if let Err(error) = verify_packed_chunks(&mut chunks) {
        drop(deletion);
        return Err(restore_quarantined_source_after_failure(
            path,
            &quarantine_path,
            &quarantine_dir,
            error,
        ));
    }

    drop(handle);
    if let Err(error) = deletion.delete() {
        return Err(restore_quarantined_source_after_failure(
            path,
            &quarantine_path,
            &quarantine_dir,
            CokacencError::Other(format!(
                "Cannot remove verified source {}: {}",
                quarantine_path.display(),
                error
            )),
        ));
    }
    let _ = fs::remove_dir(&quarantine_dir);
    match sync_directory(parent) {
        Ok(()) => Ok(PackedSourceRemoval::Removed),
        Err(error) => Ok(PackedSourceRemoval::RemovedButDirectorySyncFailed(error)),
    }
}

#[derive(Debug, Clone)]
struct ChunkSource {
    original: PathBuf,
    identity: StablePathIdentity,
    read_sha256: [u8; 32],
}

#[derive(Debug, Clone)]
struct QuarantinedChunk {
    original: PathBuf,
    quarantined: PathBuf,
    identity: StablePathIdentity,
    read_sha256: [u8; 32],
}

fn verify_quarantined_chunk(chunk: &QuarantinedChunk) -> Result<(), CokacencError> {
    let (mut file, _) = open_regular_file_no_follow(&chunk.quarantined)?;
    if stable_file_identity(&file)? != chunk.identity {
        return Err(CokacencError::Other(format!(
            "Quarantined encrypted chunk changed identity: '{}'",
            chunk.quarantined.display()
        )));
    }
    let before = file.metadata()?;
    let sha256 = sha256_file(&mut file)?;
    let after = file.metadata()?;
    if !content_metadata_unchanged(&before, &after)
        || stable_file_identity(&file)? != chunk.identity
        || stable_path_identity(&chunk.quarantined)? != chunk.identity
        || sha256 != chunk.read_sha256
    {
        return Err(CokacencError::Other(format!(
            "Encrypted chunk bytes changed during decryption or quarantine: '{}'",
            chunk.original.display()
        )));
    }
    Ok(())
}

fn rollback_chunk_quarantine(
    dir: &Path,
    quarantine_dir: &Path,
    moved: &[QuarantinedChunk],
    cause: impl std::fmt::Display,
) -> CokacencError {
    let mut restore_failures = Vec::new();
    for chunk in moved.iter().rev() {
        match stable_path_identity(&chunk.quarantined) {
            Ok(identity) if identity == chunk.identity => {
                if let Err(error) = rename_noreplace(&chunk.quarantined, &chunk.original) {
                    restore_failures.push(format!(
                        "'{}' remains at recovery path '{}': {}",
                        chunk.original.display(),
                        chunk.quarantined.display(),
                        error
                    ));
                }
            }
            Ok(_) => restore_failures.push(format!(
                "recovery entry '{}' changed identity and was preserved for inspection",
                chunk.quarantined.display()
            )),
            Err(error) => restore_failures.push(format!(
                "recovery entry '{}' could not be verified and was preserved: {}",
                chunk.quarantined.display(),
                error
            )),
        }
    }

    let cleanup_note = if restore_failures.is_empty() {
        fs::remove_dir(quarantine_dir).err().map(|error| {
            format!(
                "empty recovery directory '{}' could not be removed: {}",
                quarantine_dir.display(),
                error
            )
        })
    } else {
        None
    };
    let quarantine_sync_error = if fs::symlink_metadata(quarantine_dir).is_ok() {
        sync_directory(quarantine_dir).err()
    } else {
        None
    };
    let sync_error = sync_directory(dir).err();

    let mut message = if restore_failures.is_empty() {
        format!("{}; encrypted chunks were restored", cause)
    } else {
        format!(
            "{}; some encrypted chunks could not be restored automatically and remain preserved",
            cause
        )
    };
    if !restore_failures.is_empty() {
        message.push_str("; recovery details: ");
        message.push_str(&restore_failures.join("; "));
    }
    if let Some(note) = cleanup_note {
        message.push_str("; ");
        message.push_str(&note);
    }
    if let Some(error) = quarantine_sync_error {
        message.push_str(&format!(
            "; recovery directory state could not be persisted: {}",
            error
        ));
    }
    if let Some(error) = sync_error {
        message.push_str(&format!(
            "; restored directory state could not be persisted: {}",
            error
        ));
    }
    CokacencError::Other(message)
}

fn quarantine_chunks(
    dir: &Path,
    chunks: &[ChunkSource],
) -> Result<(PathBuf, Vec<QuarantinedChunk>), CokacencError> {
    quarantine_chunks_impl(dir, chunks, |_, _| {})
}

fn quarantine_chunks_impl<F>(
    dir: &Path,
    chunks: &[ChunkSource],
    mut after_move: F,
) -> Result<(PathBuf, Vec<QuarantinedChunk>), CokacencError>
where
    F: FnMut(usize, &Path),
{
    let quarantine_dir = create_quarantine_dir(dir)?;
    let mut moved = Vec::with_capacity(chunks.len());

    for (index, chunk) in chunks.iter().enumerate() {
        if stable_path_identity(&chunk.original).ok() != Some(chunk.identity) {
            return Err(rollback_chunk_quarantine(
                dir,
                &quarantine_dir,
                &moved,
                format!(
                    "Encrypted chunk changed before quarantine: '{}'",
                    chunk.original.display()
                ),
            ));
        }

        let quarantined = quarantine_dir.join(format!("chunk-{index:08}"));
        if let Err(error) = rename_noreplace(&chunk.original, &quarantined) {
            return Err(rollback_chunk_quarantine(
                dir,
                &quarantine_dir,
                &moved,
                format!(
                    "Could not quarantine encrypted chunk '{}': {}",
                    chunk.original.display(),
                    error
                ),
            ));
        }
        moved.push(QuarantinedChunk {
            original: chunk.original.clone(),
            quarantined: quarantined.clone(),
            identity: chunk.identity,
            read_sha256: chunk.read_sha256,
        });
        after_move(index, &quarantined);

        if let Err(error) = verify_quarantined_chunk(moved.last().expect("just pushed")) {
            return Err(rollback_chunk_quarantine(
                dir,
                &quarantine_dir,
                &moved,
                format!(
                    "Encrypted chunk changed while being quarantined: '{}': {}",
                    chunk.original.display(),
                    error
                ),
            ));
        }
    }

    if let Err(error) = sync_directory(&quarantine_dir).and_then(|()| sync_directory(dir)) {
        return Err(rollback_chunk_quarantine(
            dir,
            &quarantine_dir,
            &moved,
            format!("Could not persist encrypted chunk quarantine: {}", error),
        ));
    }

    Ok((quarantine_dir, moved))
}

fn delete_quarantined_chunks(
    dir: &Path,
    quarantine_dir: &Path,
    chunks: Vec<QuarantinedChunk>,
) -> Result<(), CokacencError> {
    // Bind every deletion before committing the first one. A preparation
    // failure is still fully rollbackable because no archive object has yet
    // been deleted.
    let mut deletions = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        if let Err(error) = verify_quarantined_chunk(chunk) {
            drop(deletions);
            return Err(rollback_chunk_quarantine(
                dir,
                quarantine_dir,
                &chunks,
                format!(
                    "Encrypted chunk changed before deletion '{}': {}",
                    chunk.quarantined.display(),
                    error
                ),
            ));
        }
        match prepare_file_deletion(&chunk.quarantined, chunk.identity) {
            Ok(deletion) => deletions.push(deletion),
            Err(error) => {
                drop(deletions);
                return Err(rollback_chunk_quarantine(
                    dir,
                    quarantine_dir,
                    &chunks,
                    format!(
                        "Could not bind encrypted chunk deletion '{}': {}",
                        chunk.quarantined.display(),
                        error
                    ),
                ));
            }
        }
    }

    for (index, deletion) in deletions.into_iter().enumerate() {
        if let Err(error) = deletion.delete() {
            let quarantine_sync_error = sync_directory(quarantine_dir).err();
            let remaining = chunks[index..]
                .iter()
                .filter(|chunk| fs::symlink_metadata(&chunk.quarantined).is_ok())
                .map(|chunk| chunk.quarantined.display().to_string())
                .collect::<Vec<_>>();
            let mut message = format!(
                "Plaintext was published and synced, but archive cleanup failed: {}. Remaining encrypted data is preserved under '{}'{}",
                error,
                quarantine_dir.display(),
                if remaining.is_empty() {
                    String::new()
                } else {
                    format!(" at {}", remaining.join(", "))
                }
            );
            if let Some(sync_error) = quarantine_sync_error {
                message.push_str(&format!(
                    "; recovery directory state could not be persisted: {}",
                    sync_error
                ));
            }
            return Err(CokacencError::Other(message));
        }
    }

    fs::remove_dir(quarantine_dir).map_err(|error| {
        CokacencError::Other(format!(
            "Plaintext was published and all encrypted chunks were removed, but recovery directory '{}' could not be removed: {}",
            quarantine_dir.display(),
            error
        ))
    })?;
    sync_directory(dir)
}

struct TempOutputGuard {
    path: PathBuf,
}

impl Drop for TempOutputGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_unpack_temp(dir: &Path, group_id: &str) -> Result<(PathBuf, File), CokacencError> {
    for _ in 0..100 {
        let mut random = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut random);
        let path = dir.join(format!(".{}.{}.unpacking", group_id, hex::encode(random)));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(CokacencError::Other(
        "Could not create a unique temporary output file".to_string(),
    ))
}

fn publish_noclobber(temp_path: &Path, out_path: &Path) -> Result<(), CokacencError> {
    // Both paths are allocated in the same directory. Publish the already
    // synced complete file with the platform's atomic no-replace primitive so
    // no reader can ever observe a partially copied plaintext destination.
    rename_noreplace(temp_path, out_path)?;
    Ok(())
}

// ─── Unpack (decrypt) ──────────────────────────────────────────────────

/// Unpack (decrypt) all .cokacenc file groups in a directory with progress reporting.
/// Metadata is extracted from each chunk. After decryption, .cokacenc files are deleted.
pub fn unpack_directory_with_progress(
    dir: &Path,
    password: &[u8],
    tx: Sender<ProgressMessage>,
    cancel_flag: Arc<AtomicBool>,
) {
    let groups = match naming::group_enc_files(dir) {
        Ok(g) => g,
        Err(e) => {
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Read dir error: {}", e),
            ));
            let _ = tx.send(ProgressMessage::Completed(0, 1));
            return;
        }
    };

    if groups.is_empty() {
        let _ = tx.send(ProgressMessage::Completed(0, 0));
        return;
    }

    let total_groups = groups.len();
    let _ = tx.send(ProgressMessage::TotalProgress(0, total_groups, 0, 0));

    let mut success_count = 0;
    let mut failure_count = 0;

    for (i, (group_id, chunks)) in groups.iter().enumerate() {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }

        let _ = tx.send(ProgressMessage::FileStarted(format!(
            "{}...",
            &group_id[..8.min(group_id.len())]
        )));

        match unpack_file_group(dir, chunks, password, &tx) {
            Ok(original_name) => {
                success_count += 1;
                let _ = tx.send(ProgressMessage::FileCompleted(original_name));
            }
            Err(e) => {
                failure_count += 1;
                let _ = tx.send(ProgressMessage::Error(group_id.clone(), e.to_string()));
            }
        }

        let _ = tx.send(ProgressMessage::TotalProgress(i + 1, total_groups, 0, 0));
    }

    let _ = tx.send(ProgressMessage::Completed(success_count, failure_count));
}

/// Decrypt and merge a group of chunk files into the original file.
/// Returns the original filename on success.
fn unpack_file_group(
    dir: &Path,
    chunks: &[naming::EncFileInfo],
    password: &[u8],
    tx: &Sender<ProgressMessage>,
) -> Result<String, CokacencError> {
    // Shift+D is the counterpart to the old Shift+E writer above. It expects
    // the v2 chunk stream: header salt/iv/original name, then encrypted
    // `[metadata length][metadata][file bytes]`. Keep this reader aligned with
    // cokacdircode_old so old archives remain first-class, not a legacy edge.
    if chunks.is_empty() {
        return Err(CokacencError::NoEncFiles("empty group".to_string()));
    }

    // Validate sequence continuity
    for (i, chunk) in chunks.iter().enumerate() {
        if chunk.seq_index != i {
            let expected_label = naming::seq_label(i)?;
            return Err(CokacencError::MissingChunk {
                expected: expected_label,
            });
        }
    }

    let group_id = &chunks[0].group_id;
    let (temp_path, out_file) = create_unpack_temp(dir, group_id)?;
    let _temp_guard = TempOutputGuard {
        path: temp_path.clone(),
    };
    let mut file_writer = BufWriter::new(out_file);
    let mut md5_hasher = Md5::new();

    let mut original_name = String::new();
    let mut expected_md5 = String::new();
    let mut file_size = 0u64;
    let mut modified = 0i64;
    let mut permissions: u32 = 0;
    let mut total_data_written = 0u64;
    let mut chunk_sources = Vec::with_capacity(chunks.len());

    for (i, chunk_info) in chunks.iter().enumerate() {
        let (enc_file, chunk_identity) = open_encrypted_chunk(&chunk_info.path)?;
        let chunk_metadata_before = enc_file.metadata()?;
        let mut reader = BufReader::new(Sha256Reader::new(enc_file));

        let (salt, iv, header_filename) = read_header(&mut reader)?;
        let key = derive_key(password, &salt);

        // Decrypt through MetadataSplitWriter -> TeeWriter(file, md5)
        let meta_bytes;
        let chunk_start = total_data_written;
        {
            let mut tee = TeeWriter {
                file: &mut file_writer,
                hasher: &mut md5_hasher,
                bytes_written: &mut total_data_written,
            };
            let mut split = MetadataSplitWriter::new(&mut tee);
            decrypt_chunk_streaming(&mut reader, &mut split, &key, &iv)?;
            meta_bytes = split.take_metadata_bytes()?;
        }

        let meta: ChunkMetadata = serde_json::from_slice(&meta_bytes)
            .map_err(|e| CokacencError::MetadataParse(e.to_string()))?;

        // Validate chunk metadata
        if meta.chunk_index != i {
            return Err(CokacencError::MetadataParse(format!(
                "Chunk index mismatch: expected {}, got {}",
                i, meta.chunk_index
            )));
        }

        let actual_chunk_size = total_data_written - chunk_start;
        if meta.version != crypto::VERSION
            || meta.group_id != *group_id
            || meta.filename != header_filename
            || meta.total_chunks != chunks.len()
            || meta.chunk_offset != chunk_start
            || meta.chunk_data_size != actual_chunk_size
        {
            return Err(CokacencError::MetadataParse(format!(
                "Invalid metadata for chunk {}",
                i
            )));
        }

        if i == 0 {
            original_name = meta.filename.clone();
            expected_md5 = meta.file_md5.clone();
            file_size = meta.file_size;
            modified = meta.modified;
            permissions = meta.permissions;
            // Update progress with real filename
            let _ = tx.send(ProgressMessage::FileStarted(original_name.clone()));
        } else {
            // Cross-check metadata consistency across chunks
            if meta.filename != original_name
                || meta.file_size != file_size
                || meta.modified != modified
                || meta.permissions != permissions
                || meta.file_md5 != expected_md5
            {
                return Err(CokacencError::MetadataParse(
                    "Inconsistent metadata across chunks".to_string(),
                ));
            }
        }

        let digesting_reader = reader.into_inner();
        let (enc_file, read_sha256) = digesting_reader.finish();
        let chunk_metadata_after = enc_file.metadata()?;
        if !content_metadata_unchanged(&chunk_metadata_before, &chunk_metadata_after)
            || stable_file_identity(&enc_file)? != chunk_identity
            || stable_path_identity(&chunk_info.path)? != chunk_identity
        {
            return Err(CokacencError::Other(format!(
                "Encrypted chunk changed while it was being decrypted: '{}'",
                chunk_info.path.display()
            )));
        }
        chunk_sources.push(ChunkSource {
            original: chunk_info.path.clone(),
            identity: chunk_identity,
            read_sha256,
        });
    }

    file_writer.flush()?;

    // Verify MD5 (skip if MD5 was not computed during encryption)
    let md5_hex = format!("{:032x}", md5_hasher.finalize());
    if !expected_md5.is_empty() && md5_hex != expected_md5 {
        return Err(CokacencError::Md5Mismatch {
            expected: expected_md5,
            actual: md5_hex,
        });
    }

    // Verify file size
    let actual_size = file_writer.get_ref().metadata()?.len();
    if actual_size != file_size {
        return Err(CokacencError::Other(format!(
            "Size mismatch: expected {}, got {}",
            file_size, actual_size
        )));
    }

    // Rename to original filename (sanitize to prevent path traversal)
    let safe_name = match Path::new(&original_name)
        .file_name()
        .and_then(|n| n.to_str())
    {
        Some(name) => name,
        None => {
            return Err(CokacencError::MetadataParse(format!(
                "Invalid filename in metadata: {}",
                original_name
            )));
        }
    };
    let out_path = dir.join(safe_name);

    // Apply metadata to the still-open private temporary file. Path-based
    // metadata operations after publication could be redirected to a
    // concurrent replacement.
    #[cfg(unix)]
    if permissions != 0 {
        use std::os::unix::fs::PermissionsExt;
        let safe_mode = permissions & 0o0777;
        file_writer
            .get_ref()
            .set_permissions(fs::Permissions::from_mode(safe_mode))?;
    }
    if modified > 0 {
        use std::time::{Duration, SystemTime};
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(modified as u64);
        file_writer.get_ref().set_times(
            std::fs::FileTimes::new()
                .set_accessed(mtime)
                .set_modified(mtime),
        )?;
    }
    file_writer.get_ref().sync_all()?;
    drop(file_writer);

    // First relocate every exact archive object into one private recovery
    // directory. Any failure before plaintext publication rolls all prior
    // moves back, so callers never receive a half-consumed archive group.
    let (quarantine_dir, quarantined_chunks) = quarantine_chunks(dir, &chunk_sources)?;

    // Publish the completed temporary plaintext atomically without replacing a
    // path that appeared while decryption was in progress. If publication or
    // its directory sync fails, restore every quarantined chunk. Once this sync
    // succeeds, plaintext is the durable recovery copy that makes subsequent
    // archive cleanup non-destructive.
    if let Err(error) = publish_noclobber(&temp_path, &out_path) {
        return Err(rollback_chunk_quarantine(
            dir,
            &quarantine_dir,
            &quarantined_chunks,
            format!("Could not publish decrypted plaintext: {}", error),
        ));
    }
    if let Err(error) = sync_directory(dir) {
        return Err(rollback_chunk_quarantine(
            dir,
            &quarantine_dir,
            &quarantined_chunks,
            format!(
                "Plaintext was published, but its directory update could not be persisted: {}",
                error
            ),
        ));
    }

    delete_quarantined_chunks(dir, &quarantine_dir, quarantined_chunks)?;

    Ok(safe_name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn first_chunk_path(dir: &Path) -> PathBuf {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("cokacenc"))
            .expect("encrypted chunk should exist")
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

    #[cfg(unix)]
    #[test]
    fn ensure_key_rejects_symlinked_credential_directory_without_chmodding_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let config = home.join(".cokacdir");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&outside, config.join("credential")).unwrap();

        assert!(ensure_key_in(&home).is_err());
        assert_eq!(
            fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!outside.join("cokacenc.key").exists());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_key_rejects_symlinked_key_without_touching_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let credential = home.join(".cokacdir").join("credential");
        let victim = temp.path().join("victim.key");
        fs::create_dir_all(&credential).unwrap();
        fs::write(&victim, b"victim material").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&victim, credential.join("cokacenc.key")).unwrap();

        assert!(ensure_key_in(&home).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"victim material");
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn pack_preserves_old_v2_header_filename_and_key_prefix_contract() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("payload.txt");
        fs::write(&file_path, b"payload data").unwrap();

        let (tx, rx) = mpsc::channel();
        pack_directory_with_progress(
            temp_dir.path(),
            b"Ab3+/Z",
            tx,
            Arc::new(AtomicBool::new(false)),
            0,
            false,
        );
        assert_eq!(
            completed_message(&rx.try_iter().collect::<Vec<_>>()),
            Some((1, 0))
        );
        assert!(!file_path.exists());

        let chunk_path = first_chunk_path(temp_dir.path());
        let chunk_name = chunk_path.file_name().unwrap().to_string_lossy();
        assert!(chunk_name.starts_with("Ab3Z_"));

        let mut header_reader = BufReader::new(File::open(&chunk_path).unwrap());
        let (_salt, _iv, header_filename) = read_header(&mut header_reader).unwrap();
        assert_eq!(header_filename, "payload.txt");

        let bytes = fs::read(&chunk_path).unwrap();
        assert_eq!(&bytes[..crypto::MAGIC.len()], crypto::MAGIC);
        let version_start = crypto::MAGIC.len();
        let version =
            u32::from_le_bytes(bytes[version_start..version_start + 4].try_into().unwrap());
        assert_eq!(version, crypto::VERSION);
    }

    #[test]
    fn pack_without_md5_round_trips() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("payload.txt");
        fs::write(&file_path, b"payload data").unwrap();

        let (tx, rx) = mpsc::channel();
        pack_directory_with_progress(
            temp_dir.path(),
            b"test-key",
            tx,
            Arc::new(AtomicBool::new(false)),
            0,
            false,
        );
        assert_eq!(
            completed_message(&rx.try_iter().collect::<Vec<_>>()),
            Some((1, 0))
        );
        assert!(!file_path.exists());

        let (tx, rx) = mpsc::channel();
        unpack_directory_with_progress(
            temp_dir.path(),
            b"test-key",
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        let messages: Vec<ProgressMessage> = rx.try_iter().collect();

        assert_eq!(completed_message(&messages), Some((1, 0)));
        assert_eq!(fs::read(&file_path).unwrap(), b"payload data");
    }

    #[test]
    fn unpack_does_not_overwrite_a_newer_plaintext_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("payload.txt");
        fs::write(&file_path, b"archived data").unwrap();

        let (tx, _rx) = mpsc::channel();
        pack_directory_with_progress(
            temp_dir.path(),
            b"test-key",
            tx,
            Arc::new(AtomicBool::new(false)),
            0,
            true,
        );
        let chunk_path = first_chunk_path(temp_dir.path());
        fs::write(&file_path, b"newer data").unwrap();

        let (tx, rx) = mpsc::channel();
        unpack_directory_with_progress(
            temp_dir.path(),
            b"test-key",
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        let messages: Vec<_> = rx.try_iter().collect();

        assert_eq!(completed_message(&messages), Some((0, 1)));
        assert_eq!(fs::read(&file_path).unwrap(), b"newer data");
        assert!(chunk_path.exists(), "failed unpack must retain the archive");
    }

    #[cfg(unix)]
    #[test]
    fn unpack_does_not_follow_predictable_temp_symlink() {
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("payload.txt");
        let victim_path = temp_dir.path().join("victim.txt");
        fs::write(&file_path, b"payload data").unwrap();

        let (tx, _rx) = mpsc::channel();
        pack_directory_with_progress(
            temp_dir.path(),
            b"test-key",
            tx,
            Arc::new(AtomicBool::new(false)),
            0,
            true,
        );
        let chunk_path = first_chunk_path(temp_dir.path());
        let group_id = naming::parse_enc_filename(&chunk_path).unwrap().group_id;
        fs::write(&victim_path, b"must survive").unwrap();
        symlink(
            &victim_path,
            temp_dir.path().join(format!(".{}.unpacking", group_id)),
        )
        .unwrap();

        let (tx, rx) = mpsc::channel();
        unpack_directory_with_progress(
            temp_dir.path(),
            b"test-key",
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        let messages: Vec<_> = rx.try_iter().collect();

        assert_eq!(completed_message(&messages), Some((1, 0)));
        assert_eq!(fs::read(&victim_path).unwrap(), b"must survive");
        assert_eq!(fs::read(&file_path).unwrap(), b"payload data");
    }

    #[test]
    fn oversized_split_setting_does_not_overflow() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();

        pack_directory_with_progress(
            temp_dir.path(),
            b"test-key",
            tx,
            Arc::new(AtomicBool::new(false)),
            u64::MAX,
            true,
        );

        assert_eq!(
            completed_message(&rx.try_iter().collect::<Vec<_>>()),
            Some((0, 0))
        );
    }

    #[test]
    fn metadata_splitter_rejects_unbounded_allocation() {
        let mut output = Vec::new();
        let mut splitter = MetadataSplitWriter::new(&mut output);
        let claimed_len = (MAX_METADATA_LEN as u32 + 1).to_le_bytes();
        let error = splitter.write_all(&claimed_len).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(output.is_empty());
    }

    #[test]
    fn source_replacement_after_pack_is_restored_instead_of_deleted() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("payload.txt");
        let retained_original = temp_dir.path().join("retained-original.txt");
        fs::write(&file_path, b"archived data").unwrap();

        let packed = pack_file(
            &file_path,
            "payload.txt",
            temp_dir.path(),
            b"test-key",
            u64::MAX,
            true,
        )
        .unwrap();
        fs::rename(&file_path, &retained_original).unwrap();
        fs::write(&file_path, b"other payload").unwrap();
        let original_metadata = fs::metadata(&retained_original).unwrap();
        filetime::set_file_times(
            &file_path,
            filetime::FileTime::from_last_access_time(&original_metadata),
            filetime::FileTime::from_last_modification_time(&original_metadata),
        )
        .unwrap();

        let error = remove_packed_source(&file_path, packed).unwrap_err();
        assert!(error.to_string().contains("replaced during encryption"));
        assert_eq!(fs::read(&file_path).unwrap(), b"other payload");
        assert_eq!(fs::read(&retained_original).unwrap(), b"archived data");
        assert!(fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".cokacenc-delete-")));
    }

    #[cfg(unix)]
    #[test]
    fn pack_without_md5_detects_same_inode_byte_change_with_restored_mtime() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("payload.txt");
        fs::write(&file_path, b"original-content").unwrap();

        let packed = pack_file(
            &file_path,
            "payload.txt",
            temp_dir.path(),
            b"test-key",
            u64::MAX,
            false,
        )
        .unwrap();
        let original_metadata = fs::metadata(&file_path).unwrap();
        fs::write(&file_path, b"modified-content").unwrap();
        filetime::set_file_times(
            &file_path,
            filetime::FileTime::from_last_access_time(&original_metadata),
            filetime::FileTime::from_last_modification_time(&original_metadata),
        )
        .unwrap();

        let error = remove_packed_source(&file_path, packed).unwrap_err();
        assert!(error.to_string().contains("Source bytes changed"));
        assert_eq!(fs::read(&file_path).unwrap(), b"modified-content");
        assert!(first_chunk_path(temp_dir.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn pack_rechecks_quarantined_bytes_immediately_before_deletion() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("payload.txt");
        fs::write(&file_path, b"original-content").unwrap();

        let packed = pack_file(
            &file_path,
            "payload.txt",
            temp_dir.path(),
            b"test-key",
            u64::MAX,
            false,
        )
        .unwrap();
        let original_metadata = fs::metadata(&file_path).unwrap();
        let error = remove_packed_source_impl(&file_path, packed, |quarantined| {
            fs::write(quarantined, b"modified-content").unwrap();
            filetime::set_file_times(
                quarantined,
                filetime::FileTime::from_last_access_time(&original_metadata),
                filetime::FileTime::from_last_modification_time(&original_metadata),
            )
            .unwrap();
        })
        .unwrap_err();

        assert!(error.to_string().contains("Source bytes changed"));
        assert_eq!(fs::read(&file_path).unwrap(), b"modified-content");
        assert!(first_chunk_path(temp_dir.path()).exists());
    }

    #[test]
    fn packed_chunk_replacement_restores_source_and_preserves_replacement() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("payload.txt");
        let retained_chunk = temp_dir.path().join("retained.cokacenc");
        fs::write(&file_path, b"original-content").unwrap();

        let packed = pack_file(
            &file_path,
            "payload.txt",
            temp_dir.path(),
            b"test-key",
            u64::MAX,
            false,
        )
        .unwrap();
        let chunk_path = first_chunk_path(temp_dir.path());
        fs::rename(&chunk_path, &retained_chunk).unwrap();
        fs::write(&chunk_path, b"racing replacement").unwrap();

        let error = remove_packed_source(&file_path, packed).unwrap_err();
        assert!(error.to_string().contains("changed identity"));
        assert_eq!(fs::read(&file_path).unwrap(), b"original-content");
        assert_eq!(fs::read(&chunk_path).unwrap(), b"racing replacement");
        assert!(retained_chunk.exists());
    }

    #[test]
    fn packed_chunk_bytes_are_rechecked_immediately_before_source_deletion() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("payload.txt");
        fs::write(&file_path, b"original-content").unwrap();

        let packed = pack_file(
            &file_path,
            "payload.txt",
            temp_dir.path(),
            b"test-key",
            u64::MAX,
            false,
        )
        .unwrap();
        let chunk_path = first_chunk_path(temp_dir.path());
        let error = remove_packed_source_impl(&file_path, packed, |_| {
            fs::write(&chunk_path, b"modified archive bytes").unwrap();
        })
        .unwrap_err();

        assert!(error.to_string().contains("chunk bytes changed"));
        assert_eq!(fs::read(&file_path).unwrap(), b"original-content");
        assert_eq!(fs::read(&chunk_path).unwrap(), b"modified archive bytes");
    }

    #[test]
    fn failed_pack_cleanup_never_deletes_a_chunk_path_replacement() {
        let temp_dir = tempfile::tempdir().unwrap();
        let chunk_path = temp_dir.path().join("chunk.cokacenc");
        let retained_chunk = temp_dir.path().join("retained.cokacenc");
        let mut options = OpenOptions::new();
        let handle = options
            .read(true)
            .write(true)
            .create_new(true)
            .open(&chunk_path)
            .unwrap();
        let identity = stable_file_identity(&handle).unwrap();
        let chunks = vec![PackedChunk {
            path: chunk_path.clone(),
            identity,
            expected_sha256: None,
            handle,
        }];

        fs::rename(&chunk_path, &retained_chunk).unwrap();
        fs::write(&chunk_path, b"racing replacement").unwrap();
        let notes = cleanup_created_chunks(&chunks);

        assert!(notes.iter().any(|note| note.contains("was replaced")));
        assert_eq!(fs::read(&chunk_path).unwrap(), b"racing replacement");
        assert!(retained_chunk.exists());
    }

    #[test]
    fn chunk_quarantine_rolls_back_prior_moves_on_later_replacement() {
        let temp_dir = tempfile::tempdir().unwrap();
        let first = temp_dir.path().join("first.cokacenc");
        let second = temp_dir.path().join("second.cokacenc");
        let retained_second = temp_dir.path().join("retained-second.cokacenc");
        fs::write(&first, b"first archive").unwrap();
        fs::write(&second, b"second archive").unwrap();

        let (mut first_file, first_identity) = open_encrypted_chunk(&first).unwrap();
        let (mut second_file, second_identity) = open_encrypted_chunk(&second).unwrap();
        let first_sha256 = sha256_file(&mut first_file).unwrap();
        let second_sha256 = sha256_file(&mut second_file).unwrap();
        drop((first_file, second_file));
        let sources = vec![
            ChunkSource {
                original: first.clone(),
                identity: first_identity,
                read_sha256: first_sha256,
            },
            ChunkSource {
                original: second.clone(),
                identity: second_identity,
                read_sha256: second_sha256,
            },
        ];

        let error = quarantine_chunks_impl(temp_dir.path(), &sources, |index, _| {
            if index == 0 {
                fs::rename(&second, &retained_second).unwrap();
                fs::write(&second, b"path replacement").unwrap();
            }
        })
        .unwrap_err();

        assert!(error.to_string().contains("chunks were restored"));
        assert_eq!(fs::read(&first).unwrap(), b"first archive");
        assert_eq!(fs::read(&second).unwrap(), b"path replacement");
        assert_eq!(fs::read(&retained_second).unwrap(), b"second archive");
        assert!(fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".cokacenc-delete-")));
    }

    #[test]
    fn chunk_quarantine_restores_same_inode_content_change() {
        let temp_dir = tempfile::tempdir().unwrap();
        let chunk = temp_dir.path().join("chunk.cokacenc");
        fs::write(&chunk, b"archive-before").unwrap();
        let (mut file, identity) = open_encrypted_chunk(&chunk).unwrap();
        let read_sha256 = sha256_file(&mut file).unwrap();
        drop(file);
        let sources = vec![ChunkSource {
            original: chunk.clone(),
            identity,
            read_sha256,
        }];

        let error = quarantine_chunks_impl(temp_dir.path(), &sources, |_, quarantined| {
            fs::write(quarantined, b"archive-after!").unwrap();
        })
        .unwrap_err();

        assert!(error.to_string().contains("bytes changed"));
        assert_eq!(fs::read(&chunk).unwrap(), b"archive-after!");
        assert!(fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".cokacenc-delete-")));
    }

    #[cfg(unix)]
    #[test]
    fn pack_ignores_top_level_symlinks() {
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::tempdir().unwrap();
        let pack_dir = temp_dir.path().join("pack");
        fs::create_dir(&pack_dir).unwrap();
        let outside = temp_dir.path().join("outside.txt");
        let link = pack_dir.join("linked.txt");
        fs::write(&outside, b"must not be encrypted").unwrap();
        symlink(&outside, &link).unwrap();

        let (tx, rx) = mpsc::channel();
        pack_directory_with_progress(
            &pack_dir,
            b"test-key",
            tx,
            Arc::new(AtomicBool::new(false)),
            0,
            true,
        );

        assert_eq!(
            completed_message(&rx.try_iter().collect::<Vec<_>>()),
            Some((0, 0))
        );
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&outside).unwrap(), b"must not be encrypted");
    }

    #[cfg(unix)]
    #[test]
    fn chunk_open_rejects_symlink_replacement_after_discovery() {
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::tempdir().unwrap();
        let chunk_path = temp_dir.path().join("a1b2c3d4e5f6a7b8_aaaa.cokacenc");
        let outside = temp_dir.path().join("outside.cokacenc");
        fs::write(&chunk_path, b"discovered chunk").unwrap();
        fs::write(&outside, b"must not be opened").unwrap();

        let groups = naming::group_enc_files(temp_dir.path()).unwrap();
        let discovered_path = groups
            .values()
            .next()
            .and_then(|chunks| chunks.first())
            .unwrap()
            .path
            .clone();
        fs::remove_file(&chunk_path).unwrap();
        symlink(&outside, &chunk_path).unwrap();

        assert!(open_encrypted_chunk(&discovered_path).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"must not be opened");
    }

    #[cfg(unix)]
    #[test]
    fn prepared_chunk_deletion_never_removes_a_path_replacement() {
        let temp_dir = tempfile::tempdir().unwrap();
        let chunk_path = temp_dir.path().join("chunk.cokacenc");
        let retained = temp_dir.path().join("retained.cokacenc");
        fs::write(&chunk_path, b"opened archive").unwrap();

        let (_file, identity) = open_encrypted_chunk(&chunk_path).unwrap();
        let deletion = prepare_file_deletion(&chunk_path, identity).unwrap();
        fs::rename(&chunk_path, &retained).unwrap();
        fs::write(&chunk_path, b"replacement").unwrap();

        assert!(deletion.delete().is_err());
        assert_eq!(fs::read(&chunk_path).unwrap(), b"replacement");
        assert_eq!(fs::read(&retained).unwrap(), b"opened archive");
    }
}
