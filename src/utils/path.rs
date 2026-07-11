use std::path::{Path, PathBuf};

fn prepare_private_directory(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let (directory, _, metadata) = crate::services::file_ops::open_directory_for_read(dir)?;
    let identity = crate::services::file_ops::stable_file_identity(&directory)?;
    if !metadata.file_type().is_dir()
        || crate::services::file_ops::stable_path_identity(dir)? != identity
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "cokacdir temporary path is not a stable real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    if crate::services::file_ops::stable_path_identity(dir)? != identity {
        return Err(std::io::Error::other(
            "cokacdir temporary path changed while it was secured",
        ));
    }
    Ok(())
}

/// Return cokacdir's application-owned temporary directory.
///
/// Runtime files must stay below the user's cokacdir home rather than the
/// system-wide temporary directory. Failure to resolve or prepare the home
/// directory is reported to the caller; this function never falls back to
/// `/tmp` or another shared OS temporary location.
pub fn cokacdir_temp_dir() -> std::io::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "cannot determine home directory for cokacdir temporary files",
        )
    })?;
    let dir = home.join(".cokacdir").join("tmp");
    prepare_private_directory(&dir)?;
    Ok(dir)
}

fn join_home_relative(home: PathBuf, rest: &str) -> String {
    // `PathBuf::join` replaces the base when `rest` is rooted (for example
    // the remainder of `~//etc` is `/etc`).  Tilde expansion is textual: the
    // remainder must stay appended to the home prefix even when users type
    // repeated separators.  Build the path textually as a relative suffix so
    // Windows drive/prefix syntax cannot replace the home path either.
    let rest = rest.trim_start_matches(['/', '\\']);
    if rest.is_empty() {
        return home.display().to_string();
    }

    let mut expanded = home.into_os_string();
    expanded.push(std::path::MAIN_SEPARATOR.to_string());
    expanded.push(rest);
    PathBuf::from(expanded).display().to_string()
}

/// Expand a leading `~`, `~/`, or `~\` in a user-supplied path to the current user's home directory.
///
/// Conservative matching:
/// - `"~"` alone → home
/// - `"~/..."` or `"~\..."` → home joined with the remainder
/// - `"~user/..."`, `"~~/..."`, `"foo/~/bar"` → returned unchanged
///
/// If the home directory cannot be determined, the original string is returned unchanged
/// so callers fall through to their normal "not found" handling.
pub fn expand_tilde(path: &str) -> String {
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.display().to_string();
        }
        return path.to_string();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return join_home_relative(home, rest);
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> String {
        dirs::home_dir().unwrap().display().to_string()
    }

    #[test]
    fn prepares_private_cokacdir_temp_directory() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".cokacdir").join("tmp");

        prepare_private_directory(&dir).unwrap();

        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_cokacdir_temp_directory() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let home = tempfile::tempdir().unwrap();
        let outside = home.path().join("outside");
        let dir = home.path().join(".cokacdir").join("tmp");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&outside, &dir).unwrap();

        assert!(prepare_private_directory(&dir).is_err());
        assert_eq!(
            std::fs::metadata(outside).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn tilde_alone_expands_to_home() {
        assert_eq!(expand_tilde("~"), home());
    }

    #[test]
    fn tilde_slash_resolves_to_home() {
        // Compare via Path components so the test does not depend on whether
        // PathBuf::push("") appends a trailing MAIN_SEPARATOR (an std impl
        // detail that also varies if HOME itself ends with a separator or is
        // a Windows drive root / verbatim path).
        let out = expand_tilde("~/");
        let home = dirs::home_dir().unwrap();
        let out_components: Vec<_> = std::path::Path::new(&out).components().collect();
        let home_components: Vec<_> = home.components().collect();
        assert_eq!(
            out_components, home_components,
            "expected ~/ to resolve to home (got {:?}, home {:?})",
            out, home
        );
    }

    #[test]
    fn tilde_with_subpath_expands() {
        let expected = PathBuf::from(home()).join("a/b").display().to_string();
        assert_eq!(expand_tilde("~/a/b"), expected);
    }

    #[test]
    fn tilde_backslash_expands() {
        let expected = PathBuf::from(home()).join("a").display().to_string();
        assert_eq!(expand_tilde("~\\a"), expected);
    }

    #[test]
    fn tilde_backslash_alone_expands_to_home() {
        let out = expand_tilde("~\\");
        let home = dirs::home_dir().unwrap();
        let out_components: Vec<_> = std::path::Path::new(&out).components().collect();
        let home_components: Vec<_> = home.components().collect();
        assert_eq!(out_components, home_components);
    }

    #[test]
    fn repeated_separator_cannot_replace_home_prefix() {
        let home = dirs::home_dir().unwrap();
        let expected = home.join("absolute-looking");

        assert_eq!(
            std::path::Path::new(&expand_tilde("~//absolute-looking"))
                .components()
                .collect::<Vec<_>>(),
            expected.components().collect::<Vec<_>>()
        );
        assert_eq!(
            std::path::Path::new(&expand_tilde("~\\\\absolute-looking"))
                .components()
                .collect::<Vec<_>>(),
            expected.components().collect::<Vec<_>>()
        );
    }

    #[test]
    fn tilde_user_form_is_not_expanded() {
        assert_eq!(expand_tilde("~user/x"), "~user/x");
    }

    #[test]
    fn double_tilde_is_not_expanded() {
        assert_eq!(expand_tilde("~~/x"), "~~/x");
    }

    #[test]
    fn middle_tilde_is_not_expanded() {
        assert_eq!(expand_tilde("foo/~/x"), "foo/~/x");
    }

    #[test]
    fn absolute_path_is_unchanged() {
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }

    #[test]
    fn relative_path_is_unchanged() {
        assert_eq!(expand_tilde("relative"), "relative");
    }

    #[test]
    fn empty_is_unchanged() {
        assert_eq!(expand_tilde(""), "");
    }
}
