use std::path::{Path, PathBuf};
use std::process::Command;

pub fn resolve_gjc_path() -> Option<String> {
    if let Ok(val) = std::env::var("COKAC_GJC_PATH") {
        if !val.is_empty() && gjc_path_is_runnable(&val) {
            return Some(val);
        }
    }

    #[cfg(unix)]
    {
        if let Some(path) = resolve_unix_gjc(["which", "gjc"]) {
            return Some(prefer_source_linked_gjc(path));
        }
        if let Some(path) = resolve_unix_gjc(["bash", "-lc", "which gjc"]) {
            return Some(prefer_source_linked_gjc(path));
        }
    }

    #[cfg(windows)]
    {
        if let Some(path) = crate::services::claude::search_path_wide("gjc", Some(".cmd")) {
            return Some(path);
        }
        if let Some(path) = crate::services::claude::search_path_wide("gjc", Some(".exe")) {
            return Some(path);
        }
    }

    None
}

#[cfg(unix)]
fn resolve_unix_gjc<const N: usize>(cmd: [&str; N]) -> Option<String> {
    let output = Command::new(cmd[0]).args(&cmd[1..]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty() && gjc_path_is_runnable(&path)).then_some(path)
}

#[cfg(unix)]
fn prefer_source_linked_gjc(path: String) -> String {
    let Some(home) = dirs::home_dir() else {
        return path;
    };
    prefer_source_linked_gjc_for_candidate(Path::new(&path), &home)
        .map(|preferred| preferred.to_string_lossy().to_string())
        .unwrap_or(path)
}

#[cfg(unix)]
pub(crate) fn prefer_source_linked_gjc_for_candidate(
    candidate: &Path,
    home_dir: &Path,
) -> Option<PathBuf> {
    if !looks_like_bun_standalone(candidate) {
        return None;
    }

    source_linked_gjc_candidates(home_dir)
        .into_iter()
        .find(|path| gjc_path_is_runnable_path(path))
}

#[cfg(unix)]
fn source_linked_gjc_candidates(home_dir: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("COKAC_GJC_SOURCE_PATH") {
        if !path.trim().is_empty() {
            candidates.push(PathBuf::from(path));
        }
    }
    candidates.push(
        home_dir
            .join(".bun")
            .join("install")
            .join("global")
            .join("node_modules")
            .join("@gajae-code")
            .join("coding-agent")
            .join("src")
            .join("cli.ts"),
    );
    candidates
}

#[cfg(unix)]
fn looks_like_bun_standalone(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if name != "gjc" && !name.starts_with("gjc-") {
        return false;
    }

    let platform_binary_name = name.starts_with("gjc-darwin")
        || name.starts_with("gjc-linux")
        || name.starts_with("gjc-windows");
    let large_binary = metadata.len() >= 50 * 1024 * 1024;
    (platform_binary_name || large_binary) && has_binary_magic(path)
}

#[cfg(unix)]
fn has_binary_magic(path: &Path) -> bool {
    use std::io::Read;

    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut bytes = [0_u8; 4];
    if file.read_exact(&mut bytes).is_err() {
        return false;
    }
    matches!(
        bytes,
        [0x7f, b'E', b'L', b'F']
            | [b'M', b'Z', _, _]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xcf, 0xfa, 0xed, 0xfe]
            | [0xfe, 0xed, 0xfa, 0xce]
            | [0xce, 0xfa, 0xed, 0xfe]
            | [0xca, 0xfe, 0xba, 0xbe]
            | [0xbe, 0xba, 0xfe, 0xca]
    )
}

fn gjc_path_is_runnable(path: &str) -> bool {
    gjc_path_is_runnable_path(Path::new(path))
}

fn gjc_path_is_runnable_path(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return path
            .metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }
    #[cfg(windows)]
    {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        return matches!(ext.as_str(), "cmd" | "exe" | "bat" | "com");
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}
