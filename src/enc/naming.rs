use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rand::RngCore;

use super::error::CokacencError;

pub const EXT: &str = ".cokacenc";

/// Generate a random group ID (8 bytes -> 16 hex characters).
pub fn generate_group_id() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Convert index to four-letter sequence label: 0->"aaaa", max 456975->"zzzz".
pub fn seq_label(index: usize) -> Result<String, CokacencError> {
    if index > 456_975 {
        return Err(CokacencError::SeqOverflow(index));
    }
    let a = b'a' + (index / (26 * 26 * 26)) as u8;
    let b = b'a' + ((index / (26 * 26)) % 26) as u8;
    let c = b'a' + ((index / 26) % 26) as u8;
    let d = b'a' + (index % 26) as u8;
    Ok(format!(
        "{}{}{}{}",
        a as char, b as char, c as char, d as char
    ))
}

/// Parse a four-letter sequence label back to index.
fn parse_seq_label(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.len() != 4 {
        return None;
    }
    let a = bytes[0].checked_sub(b'a')? as usize;
    let b = bytes[1].checked_sub(b'a')? as usize;
    let c = bytes[2].checked_sub(b'a')? as usize;
    let d = bytes[3].checked_sub(b'a')? as usize;
    if a > 25 || b > 25 || c > 25 || d > 25 {
        return None;
    }
    Some(a * 26 * 26 * 26 + b * 26 * 26 + c * 26 + d)
}

/// Extract key prefix from password: take first 6 bytes, filter ASCII alphanumeric.
///
/// This is not cosmetic. It preserves the cokacdircode_old Shift+E filename
/// contract: encrypted chunks are named
/// `[key_prefix_]<group_id>_<seq>.cokacenc`. Removing the prefix changes the
/// visible archive naming scheme and breaks user expectations around existing
/// encrypted directories.
pub fn key_prefix(password: &[u8]) -> String {
    let len = password.len().min(6);
    password[..len]
        .iter()
        .filter(|b| b.is_ascii_alphanumeric())
        .map(|&b| b as char)
        .collect()
}

/// Generate chunk filename: [<key_prefix>_]<group_id_16hex>_<seq_4letter>.cokacenc
pub fn chunk_filename(
    dir: &Path,
    key_prefix: &str,
    group_id: &str,
    seq: usize,
) -> Result<PathBuf, CokacencError> {
    let label = seq_label(seq)?;
    if key_prefix.is_empty() {
        Ok(dir.join(format!("{}_{}{}", group_id, label, EXT)))
    } else {
        Ok(dir.join(format!("{}_{}_{}{}", key_prefix, group_id, label, EXT)))
    }
}

/// Parsed info from a v2 .cokacenc filename.
#[derive(Debug, Clone)]
pub struct EncFileInfo {
    pub group_id: String,
    pub seq_index: usize,
    pub path: PathBuf,
}

/// Parse a .cokacenc filename: [<key_prefix>_]<group_id_16hex>_<seq_4letter>.cokacenc
/// Parses from the end: seq (4 chars), then group_id (16 hex), optional key_prefix.
pub fn parse_enc_filename(path: &Path) -> Option<EncFileInfo> {
    let filename = path.file_name()?.to_str()?;
    if !filename.ends_with(EXT) {
        return None;
    }
    // Remove .cokacenc suffix
    let base = &filename[..filename.len() - EXT.len()];

    // Minimum length: 16 (group_id) + 1 (_) + 4 (seq) = 21
    if base.len() < 21 {
        return None;
    }

    // Parse from the end: last 4 chars = seq label
    let seq_str = &base[base.len() - 4..];
    let seq_index = parse_seq_label(seq_str)?;

    // Before seq: must be '_'
    let rest = &base[..base.len() - 4];
    if !rest.ends_with('_') {
        return None;
    }
    let rest = &rest[..rest.len() - 1];

    // Last 16 chars of rest = group_id (hex)
    if rest.len() < 16 {
        return None;
    }
    let group_id = &rest[rest.len() - 16..];
    if !group_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    // Anything before group_id is optional key_prefix with trailing '_'
    let prefix_part = &rest[..rest.len() - 16];
    if !prefix_part.is_empty() {
        // Must end with '_' separator
        if !prefix_part.ends_with('_') {
            return None;
        }
        // key_prefix itself (before the '_') must be non-empty and alphanumeric
        let kp = &prefix_part[..prefix_part.len() - 1];
        if kp.is_empty() || !kp.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
    }

    Some(EncFileInfo {
        group_id: group_id.to_string(),
        seq_index,
        path: path.to_path_buf(),
    })
}

/// Check if a group_id already exists in the directory.
pub fn group_id_exists(dir: &Path, group_id: &str) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.ends_with(EXT) && name.contains(group_id)
        })
}

/// Group .cokacenc files by group_id, sorted by seq_index.
pub fn group_enc_files(dir: &Path) -> Result<BTreeMap<String, Vec<EncFileInfo>>, CokacencError> {
    let mut groups: BTreeMap<String, Vec<EncFileInfo>> = BTreeMap::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(info) = parse_enc_filename(&path) {
            groups.entry(info.group_id.clone()).or_default().push(info);
        }
    }

    for files in groups.values_mut() {
        files.sort_by_key(|f| f.seq_index);
    }

    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seq_label() {
        assert_eq!(seq_label(0).unwrap(), "aaaa");
        assert_eq!(seq_label(1).unwrap(), "aaab");
        assert_eq!(seq_label(26).unwrap(), "aaba");
        assert_eq!(seq_label(456_975).unwrap(), "zzzz");
        assert!(seq_label(456_976).is_err());
    }

    #[test]
    fn test_generate_group_id_length() {
        let gid = generate_group_id();
        assert_eq!(gid.len(), 16);
        assert!(gid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_key_prefix() {
        // Mixed alphanumeric and special chars
        assert_eq!(key_prefix(b"Ab3+/Z"), "Ab3Z");
        // All alphanumeric
        assert_eq!(key_prefix(b"Hello9"), "Hello9");
        // No alphanumeric in first 6 bytes
        assert_eq!(key_prefix(b"!@#$%^"), "");
        // Shorter than 6 bytes
        assert_eq!(key_prefix(b"aB"), "aB");
        // Longer than 6 bytes - only first 6 considered
        assert_eq!(key_prefix(b"abcdefghij"), "abcdef");
        // Empty password
        assert_eq!(key_prefix(b""), "");
    }

    #[test]
    fn test_chunk_filename_with_prefix() {
        let dir = std::env::temp_dir();
        let path = chunk_filename(&dir, "Ab3Z", "a1b2c3d4e5f6a7b8", 0).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "Ab3Z_a1b2c3d4e5f6a7b8_aaaa.cokacenc"
        );
    }

    #[test]
    fn test_chunk_filename_empty_prefix() {
        let dir = std::env::temp_dir();
        let path = chunk_filename(&dir, "", "a1b2c3d4e5f6a7b8", 0).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "a1b2c3d4e5f6a7b8_aaaa.cokacenc"
        );
    }

    #[test]
    fn test_parse_enc_filename_without_prefix() {
        let path = std::env::temp_dir().join("a1b2c3d4e5f6a7b8_aaab.cokacenc");
        let info = parse_enc_filename(&path).unwrap();
        assert_eq!(info.group_id, "a1b2c3d4e5f6a7b8");
        assert_eq!(info.seq_index, 1);
    }

    #[test]
    fn test_parse_enc_filename_with_prefix() {
        let path = std::env::temp_dir().join("Ab3Z_a1b2c3d4e5f6a7b8_aaaa.cokacenc");
        let info = parse_enc_filename(&path).unwrap();
        assert_eq!(info.group_id, "a1b2c3d4e5f6a7b8");
        assert_eq!(info.seq_index, 0);
    }

    #[test]
    fn test_parse_enc_filename_with_long_prefix() {
        let path = std::env::temp_dir().join("Hello9_a1b2c3d4e5f6a7b8_abcd.cokacenc");
        let info = parse_enc_filename(&path).unwrap();
        assert_eq!(info.group_id, "a1b2c3d4e5f6a7b8");
        assert_eq!(info.seq_index, 731); // a=0,b=1,c=2,d=3 -> 0*26^3+1*26^2+2*26+3
    }

    #[test]
    fn test_parse_enc_filename_invalid() {
        let td = std::env::temp_dir();
        // Too short
        assert!(parse_enc_filename(&td.join("abc.cokacenc")).is_none());
        // No underscore before seq
        assert!(parse_enc_filename(&td.join("a1b2c3d4e5f6a7b8aaaa.cokacenc")).is_none());
        // Wrong extension
        assert!(parse_enc_filename(&td.join("a1b2c3d4e5f6a7b8_aaaa.txt")).is_none());
        // Non-hex group_id
        assert!(parse_enc_filename(&td.join("g1b2c3d4e5f6a7b8_aaaa.cokacenc")).is_none());
        // Empty key_prefix (just underscore, no content)
        assert!(parse_enc_filename(&td.join("_a1b2c3d4e5f6a7b8_aaaa.cokacenc")).is_none());
        // Non-alphanumeric key_prefix
        assert!(parse_enc_filename(&td.join("a+b_a1b2c3d4e5f6a7b8_aaaa.cokacenc")).is_none());
    }
}
