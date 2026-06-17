use std::io::{Read, Write};

use aes::Aes256;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::Hmac;
use rand::RngCore;
use sha2::Sha512;

use super::error::CokacencError;

pub const MAGIC: &[u8; 8] = b"COKACENC";
// IMPORTANT: Shift+E/Shift+D intentionally use the original cokacdir v2
// chunk format. Do not bump this default writer version or introduce a new
// default encrypted format here unless the file-panel UI, existing encrypted
// archives, and cokacdircode_old compatibility are migrated together.
pub const VERSION: u32 = 2;
const MAX_FILENAME_LEN: usize = 4096;
const AES_BLOCK: usize = 16;
const KEY_LEN: usize = 32;
const PBKDF2_ITERATIONS: u32 = 100_000;

type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256CbcDec = cbc::Decryptor<Aes256>;

/// Load and trim the key from a file.
pub fn load_key_file(path: &std::path::Path) -> Result<Vec<u8>, CokacencError> {
    let data = std::fs::read(path)?;
    let trimmed: Vec<u8> = match data.iter().rposition(|b| !b.is_ascii_whitespace()) {
        Some(pos) => data[..=pos].to_vec(),
        None => return Err(CokacencError::EmptyKeyFile),
    };
    if trimmed.is_empty() {
        return Err(CokacencError::EmptyKeyFile);
    }
    Ok(trimmed)
}

/// Derive a 32-byte AES key from password + salt via PBKDF2-HMAC-SHA512.
pub fn derive_key(password: &[u8], salt: &[u8; 16]) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    let _ = pbkdf2::pbkdf2::<Hmac<Sha512>>(password, salt, PBKDF2_ITERATIONS, &mut key);
    key
}

pub fn generate_salt() -> [u8; 16] {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    salt
}

pub fn generate_iv() -> [u8; 16] {
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);
    iv
}

/// Write the chunk header: magic + version + salt + iv + filename.
///
/// The plaintext filename in this header is part of the old Shift+E contract:
/// the file panel reads it without decrypting so encrypted chunks can still be
/// shown under their original names. Passing an empty filename breaks that UI
/// behavior even if decryption can technically recover the name from metadata.
pub fn write_header(
    w: &mut dyn Write,
    salt: &[u8; 16],
    iv: &[u8; 16],
    filename: &str,
) -> Result<(), CokacencError> {
    let name_bytes = filename.as_bytes();
    if name_bytes.len() > MAX_FILENAME_LEN {
        return Err(CokacencError::Other(format!(
            "Filename too long: {} bytes (max {})",
            name_bytes.len(),
            MAX_FILENAME_LEN,
        )));
    }
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(salt)?;
    w.write_all(iv)?;
    w.write_all(&(name_bytes.len() as u16).to_le_bytes())?;
    w.write_all(name_bytes)?;
    Ok(())
}

/// Read and validate the chunk header. Returns (salt, iv, filename).
pub fn read_header(r: &mut dyn Read) -> Result<([u8; 16], [u8; 16], String), CokacencError> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(CokacencError::InvalidMagic);
    }

    let mut ver_bytes = [0u8; 4];
    r.read_exact(&mut ver_bytes)?;
    let ver = u32::from_le_bytes(ver_bytes);
    if ver != VERSION {
        return Err(CokacencError::UnsupportedVersion(ver));
    }

    let mut salt = [0u8; 16];
    r.read_exact(&mut salt)?;

    let mut iv = [0u8; 16];
    r.read_exact(&mut iv)?;

    let mut name_len_bytes = [0u8; 2];
    r.read_exact(&mut name_len_bytes)?;
    let name_len = u16::from_le_bytes(name_len_bytes) as usize;
    if name_len > MAX_FILENAME_LEN {
        return Err(CokacencError::Other(format!(
            "Filename length in header too long: {} bytes (max {})",
            name_len, MAX_FILENAME_LEN,
        )));
    }
    let mut name_buf = vec![0u8; name_len];
    r.read_exact(&mut name_buf)?;
    let filename = String::from_utf8(name_buf)
        .map_err(|e| CokacencError::Other(format!("Invalid filename UTF-8: {}", e)))?;

    Ok((salt, iv, filename))
}

/// Streaming chunk encryptor that processes data block-by-block.
pub struct ChunkEncryptor {
    encryptor: Aes256CbcEnc,
    buf: Vec<u8>,     // partial block buffer
    out_buf: Vec<u8>, // reusable output buffer
}

impl ChunkEncryptor {
    pub fn new(key: &[u8; KEY_LEN], iv: &[u8; 16]) -> Self {
        Self {
            encryptor: Aes256CbcEnc::new(key.into(), iv.into()),
            buf: Vec::with_capacity(AES_BLOCK),
            out_buf: Vec::new(),
        }
    }

    /// Feed plaintext data; returns encrypted blocks (may be empty if not enough for a full block).
    pub fn update(&mut self, data: &[u8]) -> &[u8] {
        self.out_buf.clear();
        self.buf.extend_from_slice(data);

        let full_blocks = self.buf.len() / AES_BLOCK;
        if full_blocks == 0 {
            return &self.out_buf;
        }

        let process_len = full_blocks * AES_BLOCK;
        // Encrypt in-place
        let to_encrypt = &mut self.buf[..process_len];
        self.encryptor.encrypt_blocks_mut(to_blocks_mut(to_encrypt));
        self.out_buf.extend_from_slice(&self.buf[..process_len]);

        // Keep remainder
        let remainder = self.buf[process_len..].to_vec();
        self.buf.clear();
        self.buf.extend_from_slice(&remainder);

        &self.out_buf
    }

    /// Finalize: apply PKCS7 padding and encrypt the last block.
    pub fn finalize(mut self) -> Vec<u8> {
        // PKCS7 padding
        let pad_len = AES_BLOCK - (self.buf.len() % AES_BLOCK);
        let pad_byte = pad_len as u8;
        for _ in 0..pad_len {
            self.buf.push(pad_byte);
        }
        debug_assert!(self.buf.len() == AES_BLOCK);

        self.encryptor
            .encrypt_blocks_mut(to_blocks_mut(&mut self.buf));
        self.buf
    }
}

/// Decrypt a chunk from reader, writing plaintext to writer.
/// Uses 1-block look-ahead to handle PKCS7 unpadding on the final block.
pub fn decrypt_chunk_streaming(
    r: &mut dyn Read,
    w: &mut dyn Write,
    key: &[u8; KEY_LEN],
    iv: &[u8; 16],
) -> Result<(), CokacencError> {
    let mut decryptor = Aes256CbcDec::new(key.into(), iv.into());

    // Read all ciphertext into memory in blocks
    let mut ciphertext = Vec::new();
    r.read_to_end(&mut ciphertext)?;

    if ciphertext.is_empty() {
        return Err(CokacencError::InvalidPadding);
    }

    if ciphertext.len() % AES_BLOCK != 0 {
        return Err(CokacencError::InvalidPadding);
    }

    let total_blocks = ciphertext.len() / AES_BLOCK;

    // Decrypt all blocks except the last, write directly
    if total_blocks > 1 {
        let main_len = (total_blocks - 1) * AES_BLOCK;
        let main_part = &mut ciphertext[..main_len];
        decryptor.decrypt_blocks_mut(to_blocks_mut(main_part));
        w.write_all(main_part)?;
    }

    // Decrypt the last block and remove PKCS7 padding
    let last_start = (total_blocks - 1) * AES_BLOCK;
    let last_block = &mut ciphertext[last_start..last_start + AES_BLOCK];
    decryptor.decrypt_blocks_mut(to_blocks_mut(last_block));

    let pad_byte = last_block[AES_BLOCK - 1];
    if pad_byte == 0 || pad_byte as usize > AES_BLOCK {
        return Err(CokacencError::InvalidPadding);
    }
    let pad_len = pad_byte as usize;
    // Validate all padding bytes
    for &b in &last_block[AES_BLOCK - pad_len..] {
        if b != pad_byte {
            return Err(CokacencError::InvalidPadding);
        }
    }
    let data_len = AES_BLOCK - pad_len;
    w.write_all(&last_block[..data_len])?;

    Ok(())
}

/// Helper: reinterpret a mutable byte slice as mutable AES blocks.
#[allow(unsafe_code)]
fn to_blocks_mut(data: &mut [u8]) -> &mut [aes::Block] {
    assert!(data.len() % AES_BLOCK == 0);
    // SAFETY: aes::Block is [u8; 16] with the same alignment as u8
    unsafe {
        std::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut aes::Block, data.len() / AES_BLOCK)
    }
}
