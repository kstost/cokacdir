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
            return Some(path);
        }
        if let Some(path) = resolve_unix_gjc(["bash", "-lc", "which gjc"]) {
            return Some(path);
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

fn gjc_path_is_runnable(path: &str) -> bool {
    let p = std::path::Path::new(path);
    if !p.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return p
            .metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }
    #[cfg(windows)]
    {
        let ext = p
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
