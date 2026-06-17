pub mod crypto;
pub mod error;
pub mod naming;

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use base64::Engine;
use md5::{Digest, Md5};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::services::file_ops::ProgressMessage;
use crypto::{
    decrypt_chunk_streaming, derive_key, generate_iv, generate_salt, load_key_file, read_header,
    write_header, ChunkEncryptor,
};
use error::CokacencError;

const READ_BUF_SIZE: usize = 64 * 1024; // 64KB

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
    permissions: u32,
}

fn gather_file_info(path: &Path, use_md5: bool) -> Result<FileInfo, CokacencError> {
    let metadata = fs::metadata(path)?;
    let size = metadata.len();

    let modified = metadata
        .modified()
        .ok()
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
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
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

    Ok(FileInfo {
        size,
        md5,
        modified,
        permissions,
    })
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
}

impl<W: Write> Write for TeeWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

// ─── Key management ────────────────────────────────────────────────────

/// Ensure the encryption key file exists at ~/.cokacdir/credential/cokacenc.key.
/// Creates the directory and key file if they don't exist.
/// Returns the path to the key file.
pub fn ensure_key() -> Result<PathBuf, CokacencError> {
    let home = dirs::home_dir()
        .ok_or_else(|| CokacencError::Other("Cannot determine home directory".to_string()))?;
    let cred_dir = home.join(".cokacdir").join("credential");

    if !cred_dir.exists() {
        fs::create_dir_all(&cred_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&cred_dir, fs::Permissions::from_mode(0o700))?;
        }
    }

    let key_path = cred_dir.join("cokacenc.key");

    if !key_path.exists() {
        let mut raw = vec![0u8; 4096];
        rand::thread_rng().fill_bytes(&mut raw);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);

        fs::write(&key_path, encoded.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
        }
    }

    Ok(key_path)
}

// ─── Pack (encrypt) ────────────────────────────────────────────────────

/// Pack (encrypt) all eligible files in a directory with progress reporting.
/// Uses 2-pass: first pass computes MD5+metadata, second pass encrypts.
/// Each chunk embeds full metadata. After encryption, original files are deleted.
pub fn pack_directory_with_progress(
    dir: &Path,
    key_path: &Path,
    tx: Sender<ProgressMessage>,
    cancel_flag: Arc<AtomicBool>,
    split_size_mb: u64,
    use_md5: bool,
) {
    let password = match load_key_file(key_path) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Key file error: {}", e),
            ));
            let _ = tx.send(ProgressMessage::Completed(0, 1));
            return;
        }
    };

    let split_size = if split_size_mb == 0 {
        u64::MAX
    } else {
        split_size_mb * 1024 * 1024
    };

    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                let path = e.path();
                if !path.is_file() {
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

        match pack_file(&path, &name, dir, &password, split_size, use_md5) {
            Ok(_) => {
                // Delete original after successful encryption
                if let Err(e) = fs::remove_file(&path) {
                    let _ = tx.send(ProgressMessage::Error(
                        name.clone(),
                        format!("Encrypted but failed to delete original: {}", e),
                    ));
                }
                success_count += 1;
                let _ = tx.send(ProgressMessage::FileCompleted(name));
            }
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
fn pack_file(
    file_path: &Path,
    original_name: &str,
    out_dir: &Path,
    password: &[u8],
    split_size: u64,
    use_md5: bool,
) -> Result<(), CokacencError> {
    // Shift+E must emit the original cokacdir v2 archive format. The visible
    // behavior is bigger than "can this process decrypt its own output": old
    // encrypted directories, the file panel's original-name display, and
    // cokacdircode_old all depend on this exact v2 header/metadata/filename
    // contract. Do not silently switch this writer to a new format.
    // ── Pass 1: gather info ──
    let info = gather_file_info(file_path, use_md5)?;

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
        ((info.size + split_size - 1) / split_size) as usize
    };

    // ── Pass 2: encrypt ──
    let file = File::open(file_path)?;
    let mut reader = BufReader::new(file);
    let mut read_buf = [0u8; READ_BUF_SIZE];
    let mut created_chunks: Vec<PathBuf> = Vec::new();

    let result = (|| -> Result<(), CokacencError> {
        for chunk_idx in 0..total_chunks {
            let chunk_offset = chunk_idx as u64 * split_size;
            let chunk_data_size = if info.size == 0 {
                0
            } else {
                split_size.min(info.size - chunk_offset)
            };

            let metadata = ChunkMetadata {
                version: 2,
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
            let chunk_file = File::create(&chunk_path)?;
            created_chunks.push(chunk_path);
            let mut writer = BufWriter::new(chunk_file);

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
                    break;
                }
                let encrypted = enc.update(&read_buf[..n]);
                writer.write_all(encrypted)?;
                remaining -= n as u64;
            }

            let final_block = enc.finalize();
            writer.write_all(&final_block)?;
            writer.flush()?;
        }

        Ok(())
    })();

    // On error, clean up any partial chunk files
    if result.is_err() {
        for path in &created_chunks {
            let _ = fs::remove_file(path);
        }
    }

    result
}

// ─── Unpack (decrypt) ──────────────────────────────────────────────────

/// Unpack (decrypt) all .cokacenc file groups in a directory with progress reporting.
/// Metadata is extracted from each chunk. After decryption, .cokacenc files are deleted.
pub fn unpack_directory_with_progress(
    dir: &Path,
    key_path: &Path,
    tx: Sender<ProgressMessage>,
    cancel_flag: Arc<AtomicBool>,
) {
    let password = match load_key_file(key_path) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Key file error: {}", e),
            ));
            let _ = tx.send(ProgressMessage::Completed(0, 1));
            return;
        }
    };

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

        match unpack_file_group(dir, chunks, &password, &tx) {
            Ok(original_name) => {
                // Delete .cokacenc files after successful decryption
                for chunk_info in chunks {
                    let _ = fs::remove_file(&chunk_info.path);
                }
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
    let temp_path = dir.join(format!(".{}.unpacking", group_id));

    let out_file = File::create(&temp_path)?;
    let mut file_writer = BufWriter::new(out_file);
    let mut md5_hasher = Md5::new();

    let mut original_name = String::new();
    let mut expected_md5 = String::new();
    let mut file_size = 0u64;
    let mut modified = 0i64;
    let mut permissions: u32 = 0;

    for (i, chunk_info) in chunks.iter().enumerate() {
        let enc_file = File::open(&chunk_info.path)?;
        let mut reader = BufReader::new(enc_file);

        let (salt, iv, _header_filename) = read_header(&mut reader)?;
        let key = derive_key(password, &salt);

        // Decrypt through MetadataSplitWriter -> TeeWriter(file, md5)
        let meta_bytes;
        {
            let mut tee = TeeWriter {
                file: &mut file_writer,
                hasher: &mut md5_hasher,
            };
            let mut split = MetadataSplitWriter::new(&mut tee);
            decrypt_chunk_streaming(&mut reader, &mut split, &key, &iv)?;
            meta_bytes = split.take_metadata_bytes()?;
        }

        let meta: ChunkMetadata = serde_json::from_slice(&meta_bytes)
            .map_err(|e| CokacencError::MetadataParse(e.to_string()))?;

        // Validate chunk metadata
        if meta.chunk_index != i {
            let _ = fs::remove_file(&temp_path);
            return Err(CokacencError::MetadataParse(format!(
                "Chunk index mismatch: expected {}, got {}",
                i, meta.chunk_index
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
                || (!expected_md5.is_empty() && meta.file_md5 != expected_md5)
            {
                let _ = fs::remove_file(&temp_path);
                return Err(CokacencError::MetadataParse(
                    "Inconsistent metadata across chunks".to_string(),
                ));
            }
        }
    }

    file_writer.flush()?;
    drop(file_writer);

    // Verify MD5 (skip if MD5 was not computed during encryption)
    let md5_hex = format!("{:032x}", md5_hasher.finalize());
    if !expected_md5.is_empty() && md5_hex != expected_md5 {
        let _ = fs::remove_file(&temp_path);
        return Err(CokacencError::Md5Mismatch {
            expected: expected_md5,
            actual: md5_hex,
        });
    }

    // Verify file size
    let actual_size = fs::metadata(&temp_path).map(|m| m.len()).unwrap_or(0);
    if actual_size != file_size {
        let _ = fs::remove_file(&temp_path);
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
            let _ = fs::remove_file(&temp_path);
            return Err(CokacencError::MetadataParse(format!(
                "Invalid filename in metadata: {}",
                original_name
            )));
        }
    };
    let out_path = dir.join(safe_name);
    fs::rename(&temp_path, &out_path)?;

    // Restore permissions
    #[cfg(unix)]
    if permissions != 0 {
        use std::os::unix::fs::PermissionsExt;
        // Mask out setuid/setgid/sticky bits to prevent privilege-escalation
        // when decrypting a maliciously crafted .cokacenc file.
        let safe_mode = permissions & 0o0777;
        let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(safe_mode));
    }

    // Restore mtime
    #[cfg(unix)]
    if modified > 0 {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        if let Ok(cpath) = CString::new(out_path.as_os_str().as_bytes()) {
            let times = [
                libc::timespec {
                    tv_sec: modified as libc::time_t,
                    tv_nsec: 0,
                }, // atime
                libc::timespec {
                    tv_sec: modified as libc::time_t,
                    tv_nsec: 0,
                }, // mtime
            ];
            #[allow(unsafe_code)]
            unsafe {
                libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0);
            }
        }
    }

    #[cfg(not(unix))]
    if modified > 0 {
        use std::time::{Duration, SystemTime};
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(modified as u64);
        if let Ok(file) = fs::OpenOptions::new().write(true).open(&out_path) {
            let times = std::fs::FileTimes::new()
                .set_accessed(mtime)
                .set_modified(mtime);
            let _ = file.set_times(times);
        }
    }

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

    #[test]
    fn pack_preserves_old_v2_header_filename_and_key_prefix_contract() {
        let temp_dir = tempfile::tempdir().unwrap();
        let key_path = temp_dir.path().join(".key.txt");
        let file_path = temp_dir.path().join("payload.txt");
        fs::write(&key_path, "Ab3+/Z").unwrap();
        fs::write(&file_path, b"payload data").unwrap();

        let (tx, rx) = mpsc::channel();
        pack_directory_with_progress(
            temp_dir.path(),
            &key_path,
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
        let key_path = temp_dir.path().join(".key.txt");
        let file_path = temp_dir.path().join("payload.txt");
        fs::write(&key_path, "test-key").unwrap();
        fs::write(&file_path, b"payload data").unwrap();

        let (tx, rx) = mpsc::channel();
        pack_directory_with_progress(
            temp_dir.path(),
            &key_path,
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
            &key_path,
            tx,
            Arc::new(AtomicBool::new(false)),
        );
        let messages: Vec<ProgressMessage> = rx.try_iter().collect();

        assert_eq!(completed_message(&messages), Some((1, 0)));
        assert_eq!(fs::read(&file_path).unwrap(), b"payload data");
    }
}
