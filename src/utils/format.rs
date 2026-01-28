/// Format file size in human-readable format
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes < KB {
        format!("{} B", bytes)
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    }
}

/// Format file permissions in short format (rwxrwxrwx)
#[cfg(unix)]
pub fn format_permissions_short(mode: u32) -> String {
    const PERMS: [&str; 8] = ["---", "--x", "-w-", "-wx", "r--", "r-x", "rw-", "rwx"];

    let owner = PERMS[((mode >> 6) & 7) as usize];
    let group = PERMS[((mode >> 3) & 7) as usize];
    let other = PERMS[(mode & 7) as usize];

    format!("{}{}{}", owner, group, other)
}

#[cfg(not(unix))]
pub fn format_permissions_short(_mode: u32) -> String {
    String::new()
}

/// Format file permissions with type prefix
#[cfg(unix)]
pub fn format_permissions(mode: u32) -> String {
    const PERMS: [&str; 8] = ["---", "--x", "-w-", "-wx", "r--", "r-x", "rw-", "rwx"];

    let owner = PERMS[((mode >> 6) & 7) as usize];
    let group = PERMS[((mode >> 3) & 7) as usize];
    let other = PERMS[(mode & 7) as usize];

    let file_type = if (mode & 0o170000) == 0o040000 {
        'd'
    } else if (mode & 0o170000) == 0o120000 {
        'l'
    } else {
        '-'
    };

    format!(
        "{}{}{}{} ({:o})",
        file_type,
        owner,
        group,
        other,
        mode & 0o777
    )
}

#[cfg(not(unix))]
pub fn format_permissions(_mode: u32) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1048576), "1.0 MB");
        assert_eq!(format_size(1073741824), "1.0 GB");
    }

    #[cfg(unix)]
    #[test]
    fn test_format_permissions_short() {
        assert_eq!(format_permissions_short(0o755), "rwxr-xr-x");
        assert_eq!(format_permissions_short(0o644), "rw-r--r--");
        assert_eq!(format_permissions_short(0o777), "rwxrwxrwx");
        assert_eq!(format_permissions_short(0o000), "---------");
    }
}
