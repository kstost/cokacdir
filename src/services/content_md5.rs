use md5::{Digest, Md5};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::services::file_ops;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AddContentMd5Outcome {
    Renamed {
        new_path: PathBuf,
        hash: String,
        warnings: Vec<String>,
    },
    SkippedExistingHash {
        candidate_count: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerifyContentMd5Outcome {
    Match {
        hash: String,
    },
    Mismatch {
        candidates: Vec<String>,
        actual: String,
    },
    NoHash,
}

struct AuthorizedFile {
    file_name: String,
    path: PathBuf,
    source: file_ops::PathAuthorization,
    directory: file_ops::DirectoryAuthorization,
}

pub(crate) fn filename_md5_candidates(file_name: &str) -> Vec<String> {
    let bytes = file_name.as_bytes();
    let mut candidates = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        if !bytes[index].is_ascii_hexdigit() {
            index += 1;
            continue;
        }

        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_hexdigit() {
            index += 1;
        }
        if index - start == 32 {
            candidates.push(file_name[start..index].to_ascii_lowercase());
        }
    }

    candidates
}

pub(crate) fn filename_with_content_md5(file_name: &str, hash: &str) -> String {
    match file_name.rfind('.').filter(|dot| *dot > 0) {
        Some(dot) => format!("{}.{}{}", &file_name[..dot], hash, &file_name[dot..]),
        None => format!("{}.{}", file_name, hash),
    }
}

pub(crate) fn content_md5_for_authorized_file(
    path: &Path,
    authorization: &file_ops::PathAuthorization,
) -> io::Result<String> {
    file_ops::verify_path_authorization(path, authorization, "MD5 source file")?;
    let (file, before) = file_ops::open_regular_file_no_follow(path)?;
    let mut reader = io::BufReader::new(file);
    let mut hasher = Md5::new();
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let file = reader.into_inner();
    let after = file.metadata()?;
    if !file_ops::metadata_still_matches(&before, &after)
        || file_ops::stable_file_identity(&file)? != file_ops::stable_path_identity(path)?
    {
        return Err(io::Error::other(
            "File changed while its MD5 hash was being calculated",
        ));
    }
    file_ops::verify_path_authorization(path, authorization, "MD5 source file")?;

    Ok(format!("{:032x}", hasher.finalize()))
}

pub(crate) fn validate_authorized_regular_file(
    path: &Path,
    authorization: &file_ops::PathAuthorization,
) -> io::Result<()> {
    file_ops::verify_path_authorization(path, authorization, "MD5 source file")?;
    let (opened, before) = file_ops::open_regular_file_no_follow(path)?;
    let after = opened.metadata()?;
    if !file_ops::metadata_still_matches(&before, &after)
        || file_ops::stable_file_identity(&opened)? != file_ops::stable_path_identity(path)?
    {
        return Err(io::Error::other(
            "File changed while it was being validated",
        ));
    }
    file_ops::verify_path_authorization(path, authorization, "MD5 source file")
}

fn prepare_file(path: &Path) -> io::Result<AuthorizedFile> {
    let file_name_os = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Path has no filename: '{}'", path.display()),
        )
    })?;
    let file_name = file_name_os
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Filename is not valid UTF-8"))?
        .to_string();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = file_ops::capture_directory_authorization(parent)?;
    let resolved_path = directory.resolved_path().join(file_name_os);
    let source = file_ops::capture_path_authorization(&resolved_path)?;

    Ok(AuthorizedFile {
        file_name,
        path: resolved_path,
        source,
        directory,
    })
}

fn verify_authorized_regular_file(file: &AuthorizedFile) -> io::Result<()> {
    file_ops::verify_directory_authorization(
        file.directory.resolved_path(),
        &file.directory,
        "MD5 parent directory",
    )?;
    validate_authorized_regular_file(&file.path, &file.source)?;
    file_ops::verify_directory_authorization(
        file.directory.resolved_path(),
        &file.directory,
        "MD5 parent directory",
    )
}

fn hash_authorized_file(file: &AuthorizedFile) -> io::Result<String> {
    file_ops::verify_directory_authorization(
        file.directory.resolved_path(),
        &file.directory,
        "MD5 parent directory",
    )?;
    let hash = content_md5_for_authorized_file(&file.path, &file.source)?;
    file_ops::verify_directory_authorization(
        file.directory.resolved_path(),
        &file.directory,
        "MD5 parent directory",
    )?;
    Ok(hash)
}

pub(crate) fn add_content_md5_to_path(path: &Path) -> io::Result<AddContentMd5Outcome> {
    let file = prepare_file(path)?;
    let candidates = filename_md5_candidates(&file.file_name);
    if !candidates.is_empty() {
        verify_authorized_regular_file(&file)?;
        return Ok(AddContentMd5Outcome::SkippedExistingHash {
            candidate_count: candidates.len(),
        });
    }

    let hash = hash_authorized_file(&file)?;
    let new_name = filename_with_content_md5(&file.file_name, &hash);
    let new_path = file.directory.resolved_path().join(new_name);
    let warnings =
        file_ops::rename_file_authorized(&file.path, &new_path, &file.source, &file.directory)?;

    Ok(AddContentMd5Outcome::Renamed {
        new_path,
        hash,
        warnings,
    })
}

pub(crate) fn verify_content_md5_path(path: &Path) -> io::Result<VerifyContentMd5Outcome> {
    let file = prepare_file(path)?;
    let candidates = filename_md5_candidates(&file.file_name);
    if candidates.is_empty() {
        verify_authorized_regular_file(&file)?;
        return Ok(VerifyContentMd5Outcome::NoHash);
    }

    let actual = hash_authorized_file(&file)?;
    if candidates
        .iter()
        .any(|candidate| actual.eq_ignore_ascii_case(candidate))
    {
        Ok(VerifyContentMd5Outcome::Match { hash: actual })
    } else {
        Ok(VerifyContentMd5Outcome::Mismatch { candidates, actual })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_api_renames_then_verifies_a_regular_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("hello.tar");
        std::fs::write(&path, b"hello").unwrap();

        let outcome = add_content_md5_to_path(&path).unwrap();
        let new_path = match outcome {
            AddContentMd5Outcome::Renamed {
                new_path,
                hash,
                warnings,
            } => {
                assert_eq!(hash, "5d41402abc4b2a76b9719d911017c592");
                assert!(warnings.is_empty());
                new_path
            }
            AddContentMd5Outcome::SkippedExistingHash { .. } => {
                panic!("an unhashed filename must be renamed")
            }
        };

        assert!(!path.exists());
        assert!(new_path.exists());
        assert_eq!(
            verify_content_md5_path(&new_path).unwrap(),
            VerifyContentMd5Outcome::Match {
                hash: "5d41402abc4b2a76b9719d911017c592".to_string(),
            }
        );
    }

    #[test]
    fn path_api_checks_every_filename_hash_candidate() {
        const HELLO_HASH: &str = "5d41402abc4b2a76b9719d911017c592";
        const EMPTY_HASH: &str = "d41d8cd98f00b204e9800998ecf8427e";
        let temp_dir = tempfile::tempdir().unwrap();

        let mismatch = temp_dir.path().join(format!("hello.{HELLO_HASH}.txt"));
        std::fs::write(&mismatch, b"changed").unwrap();
        assert_eq!(
            add_content_md5_to_path(&mismatch).unwrap(),
            AddContentMd5Outcome::SkippedExistingHash { candidate_count: 1 }
        );
        assert!(matches!(
            verify_content_md5_path(&mismatch).unwrap(),
            VerifyContentMd5Outcome::Mismatch { .. }
        ));

        let no_hash = temp_dir.path().join("README");
        std::fs::write(&no_hash, b"plain").unwrap();
        assert_eq!(
            verify_content_md5_path(&no_hash).unwrap(),
            VerifyContentMd5Outcome::NoHash
        );

        let matching_candidates = temp_dir
            .path()
            .join(format!("two-{HELLO_HASH}-{EMPTY_HASH}.txt"));
        std::fs::write(&matching_candidates, b"hello").unwrap();
        assert_eq!(
            verify_content_md5_path(&matching_candidates).unwrap(),
            VerifyContentMd5Outcome::Match {
                hash: HELLO_HASH.to_string(),
            }
        );

        let mismatching_candidates = temp_dir
            .path()
            .join(format!("none-{HELLO_HASH}-{EMPTY_HASH}.txt"));
        std::fs::write(&mismatching_candidates, b"changed").unwrap();
        assert_eq!(
            verify_content_md5_path(&mismatching_candidates).unwrap(),
            VerifyContentMd5Outcome::Mismatch {
                candidates: vec![HELLO_HASH.to_string(), EMPTY_HASH.to_string()],
                actual: "8977dfac2f8e04cb96e66882235f5aba".to_string(),
            }
        );
    }
}
