// === UTF-8 safe string slicing utilities ===
use std::path::PathBuf;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Windows extended-length path prefix (`\\?\`) 제거
/// `canonicalize()`가 Windows에서 `\\?\C:\Users\...` 형태를 반환하므로 표시/비교 전에 strip
pub fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    path
}

/// Windows 백슬래시 경로를 슬래시로 정규화 (셸 명령/프롬프트에 삽입할 때 사용)
/// Unix에서는 no-op, Windows에서는 `C:\Users\kst` → `C:/Users/kst`
pub fn to_shell_path(path: &str) -> String {
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_string()
    }
}

/// Byte index를 가장 가까운 char boundary로 내림
pub fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Byte index를 가장 가까운 char boundary로 올림
fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 문자열 뒤에서 max_bytes 이내의 char boundary에서 자름 (앞부분 생략용)
pub fn safe_suffix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let start = s.len() - max_bytes;
    let boundary = ceil_char_boundary(s, start);
    &s[boundary..]
}

/// 문자열 앞에서 max_bytes 이내의 char boundary에서 자름 (뒷부분 생략용)
pub fn safe_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let boundary = floor_char_boundary(s, max_bytes);
    &s[..boundary]
}

/// String::truncate의 안전한 버전
pub fn safe_truncate(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let boundary = floor_char_boundary(s, max_bytes);
        s.truncate(boundary);
    }
}

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

// === CJK-aware display width utilities ===

/// 표시 너비(display width) 기준으로 문자열을 잘라낸다.
/// 전각 문자가 경계에 걸리면 공백으로 패딩하여 정확히 max_width 칸을 채운다.
pub fn truncate_to_display_width(s: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut result = String::new();
    for c in s.chars() {
        let cw = c.width().unwrap_or(1);
        if width + cw > max_width {
            break;
        }
        result.push(c);
        width += cw;
    }
    // 전각 문자 경계에서 잘린 경우 공백 패딩
    while width < max_width {
        result.push(' ');
        width += 1;
    }
    result
}

/// 표시 너비 기준으로 문자열을 target_width 칸에 맞춰 우측 공백 패딩한다.
/// 이미 target_width 이상이면 잘라낸다.
pub fn pad_to_display_width(s: &str, target_width: usize) -> String {
    let current = s.width();
    if current >= target_width {
        truncate_to_display_width(s, target_width)
    } else {
        format!("{}{}", s, " ".repeat(target_width - current))
    }
}

/// 표시 너비 기준으로 잘림 + "..." 접미사를 붙인다.
/// max_width 이하이면 원본 반환.
pub fn truncate_with_ellipsis(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let truncated = truncate_to_display_width(s, max_width.saturating_sub(3));
    let trimmed = truncated.trim_end();
    format!("{}...", trimmed)
}

/// 표시 너비 기준으로 뒤에서부터 max_width 칸 이내의 접미사를 반환한다.
/// "..." 접두사 없이 순수 접미사만 반환. 호출자가 "..." 등을 붙인다.
pub fn display_width_suffix(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut width = 0;
    let mut start_idx = chars.len();
    for i in (0..chars.len()).rev() {
        let cw = chars[i].width().unwrap_or(1);
        if width + cw > max_width {
            break;
        }
        width += cw;
        start_idx = i;
    }
    chars[start_idx..].iter().collect()
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

    #[test]
    fn test_truncate_to_display_width() {
        // ASCII only
        assert_eq!(truncate_to_display_width("abcdef", 4), "abcd");
        // CJK: 한=2, 글=2, t=1 → 총 5칸
        assert_eq!(truncate_to_display_width("한글t", 5), "한글t");
        // CJK 경계: 너비 3에 "한글" → "한 " (한=2 + 공백1)
        assert_eq!(truncate_to_display_width("한글", 3), "한 ");
        // CJK 정확히 맞음
        assert_eq!(truncate_to_display_width("한글", 4), "한글");
        // 빈 문자열
        assert_eq!(truncate_to_display_width("", 5), "     ");
    }

    #[test]
    fn test_pad_to_display_width() {
        assert_eq!(pad_to_display_width("abc", 6), "abc   ");
        // CJK: 한글=4칸 → 6칸으로 패딩하면 공백 2개
        assert_eq!(pad_to_display_width("한글", 6), "한글  ");
        // 이미 충분하면 잘림
        assert_eq!(pad_to_display_width("abcdef", 4), "abcd");
    }

    #[test]
    fn test_truncate_with_ellipsis() {
        assert_eq!(truncate_with_ellipsis("abc", 5), "abc");
        assert_eq!(truncate_with_ellipsis("abcdefgh", 6), "abc...");
        // CJK: "한글테스트" = 10칸, max=7 → "한글..."  (한글=4 + ...=3 = 7)
        assert_eq!(truncate_with_ellipsis("한글테스트", 7), "한글...");
    }

    #[test]
    fn test_display_width_suffix() {
        assert_eq!(display_width_suffix("abcdef", 3), "def");
        // CJK: "한글test" → 뒤에서 5칸: "test"(4칸)은 OK, '글'(2칸) 추가 시 6칸 초과 → "test"
        assert_eq!(display_width_suffix("한글test", 5), "test");
    }
}
