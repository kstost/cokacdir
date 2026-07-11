use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    Pid,
    Cpu,
    Mem,
    Command,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: i32,
    /// Linux `/proc/<pid>/stat` field 22. Captured when the row is listed so
    /// destructive actions can reject a PID that was reused before confirm.
    pub start_time_ticks: Option<u64>,
    pub user: String,
    pub cpu: f32,
    pub mem: f32,
    pub vsz: u64,
    pub rss: u64,
    pub tty: String,
    pub stat: String,
    pub start: String,
    pub time: String,
    pub command: String,
}

/// Validate a session ID for use as a CLI argument.
/// Accepts only `[a-zA-Z0-9_-]{1..=64}` AND rejects a leading `-` —
/// without that, `--config` (made up entirely of `-`/alnum) would slip
/// through and be treated as a new flag by the downstream argument parser.
pub fn is_valid_session_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Protected PIDs that should never be killed
const PROTECTED_PIDS: &[i32] = &[1, 2];

/// Minimum PID threshold - PIDs below this are likely kernel threads
const MIN_SAFE_PID: i32 = 300;

/// Validate PID is a safe positive integer
fn is_valid_pid(pid: i32) -> bool {
    if cfg!(unix) {
        pid > 0 && pid <= 4194304 // Max PID on Linux
    } else {
        pid > 0 // Windows PIDs can exceed Linux max
    }
}

/// Check if PID is protected from being killed
fn is_protected_pid(pid: i32, command: Option<&str>) -> Result<(), String> {
    // Check if it's our own process
    let current_pid = std::process::id() as i32;
    if pid == current_pid {
        return Err("Cannot kill the file manager itself".to_string());
    }

    // Check protected system PIDs
    if PROTECTED_PIDS.contains(&pid) {
        return Err(format!("Cannot kill system process (PID {})", pid));
    }

    // Warn about low PIDs (likely kernel threads)
    if pid < MIN_SAFE_PID {
        return Err(format!(
            "Cannot kill low PID ({}) - likely a kernel thread",
            pid
        ));
    }

    // Check if command indicates kernel thread
    if let Some(cmd) = command {
        if cmd.starts_with('[') && cmd.ends_with(']') {
            return Err("Cannot kill kernel threads".to_string());
        }
    }

    Ok(())
}

/// Result type for process list operations
pub type ProcessListResult = Result<Vec<ProcessInfo>, String>;

/// Get list of running processes
pub fn get_process_list() -> Vec<ProcessInfo> {
    get_process_list_result().unwrap_or_default()
}

/// Get list of running processes with error handling
#[cfg(unix)]
pub fn get_process_list_result() -> ProcessListResult {
    let output = Command::new("ps")
        .args(["aux"])
        .output()
        .map_err(|e| format!("Failed to execute ps command: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ps command failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut processes: Vec<ProcessInfo> = stdout
        .lines()
        .skip(1) // Skip header line (compatible with both Linux and macOS)
        .filter_map(parse_process_line)
        .collect();

    // Sort by CPU usage descending by default
    processes.sort_by(|a, b| {
        b.cpu
            .partial_cmp(&a.cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(processes)
}

#[cfg(windows)]
pub fn get_process_list_result() -> ProcessListResult {
    let output = Command::new("tasklist")
        .args(["/FO", "CSV", "/V"])
        .output()
        .map_err(|e| format!("Failed to execute tasklist command: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tasklist command failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut processes: Vec<ProcessInfo> = stdout
        .lines()
        .skip(1) // Skip header line
        .filter_map(parse_tasklist_csv_line)
        .collect();

    // Sort by CPU usage descending by default (cpu is always 0.0 from tasklist)
    processes.sort_by(|a, b| {
        b.rss.cmp(&a.rss) // Sort by memory instead since cpu% is unavailable
    });

    Ok(processes)
}

#[cfg(unix)]
fn parse_process_line(line: &str) -> Option<ProcessInfo> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 11 {
        return None;
    }

    let pid = parts[1].parse::<i32>().ok()?;
    let cpu = parts[2].parse::<f32>().ok()?;
    let mem = parts[3].parse::<f32>().ok()?;
    let vsz = parts[4].parse::<u64>().ok()?;
    let rss = parts[5].parse::<u64>().ok()?;
    #[cfg(target_os = "linux")]
    let start_time_ticks = get_process_starttime(pid);
    #[cfg(not(target_os = "linux"))]
    let start_time_ticks = None;

    Some(ProcessInfo {
        pid,
        start_time_ticks,
        user: parts[0].to_string(),
        cpu,
        mem,
        vsz,
        rss,
        tty: parts[6].to_string(),
        stat: parts[7].to_string(),
        start: parts[8].to_string(),
        time: parts[9].to_string(),
        command: parts[10..].join(" "),
    })
}

/// Parse a line from `tasklist /FO CSV /V` output
/// Format: "Image Name","PID","Session Name","Session#","Mem Usage","Status","User Name","CPU Time","Window Title"
#[cfg(windows)]
fn parse_tasklist_csv_line(line: &str) -> Option<ProcessInfo> {
    let fields: Vec<&str> = parse_csv_line(line);
    if fields.len() < 9 {
        return None;
    }

    let pid = fields[1].parse::<i32>().ok()?;
    let mem_str = fields[4]
        .replace(" K", "")
        .replace(",", "")
        .replace("\"", "");
    let rss = mem_str.trim().parse::<u64>().unwrap_or(0);

    Some(ProcessInfo {
        pid,
        start_time_ticks: None,
        user: fields[6].to_string(),
        cpu: 0.0, // tasklist doesn't provide CPU%
        mem: 0.0,
        vsz: 0,
        rss,
        tty: fields[2].to_string(),  // Session Name
        stat: fields[5].to_string(), // Status
        start: String::new(),
        time: fields[7].to_string(),    // CPU Time
        command: fields[0].to_string(), // Image Name
    })
}

/// Simple CSV line parser that handles quoted fields
#[cfg(windows)]
fn parse_csv_line(line: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let bytes = line.as_bytes();

    for i in 0..bytes.len() {
        if bytes[i] == b'"' {
            in_quotes = !in_quotes;
        } else if bytes[i] == b',' && !in_quotes {
            let field = &line[start..i];
            fields.push(field.trim_matches('"'));
            start = i + 1;
        }
    }
    if start < line.len() {
        fields.push(line[start..].trim_matches('"'));
    }
    fields
}

/// Get process start time from /proc/[pid]/stat for additional PID validation
#[cfg(target_os = "linux")]
fn get_process_starttime(pid: i32) -> Option<u64> {
    let stat_path = format!("/proc/{}/stat", pid);
    let content = std::fs::read_to_string(stat_path).ok()?;

    parse_process_starttime(&content)
}

/// Parse field 22 (`starttime`) from Linux `/proc/<pid>/stat`.
///
/// The `comm` field is parenthesized but may itself contain spaces and `)`.
/// Therefore the delimiter is the *last* closing parenthesis, not the first.
#[cfg(target_os = "linux")]
fn parse_process_starttime(content: &str) -> Option<u64> {
    // Field 22 (0-indexed: 21) is starttime
    // Format: pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt
    //         utime stime cutime cstime priority nice num_threads itrealvalue starttime ...

    // Find the closing parenthesis of comm field (which may contain spaces or
    // closing parentheses itself).
    let comm_end = content.rfind(')')?;
    let after_comm = content.get(comm_end + 1..)?.trim_start();
    let fields: Vec<&str> = after_comm.split_whitespace().collect();

    // starttime is field 20 after comm (0-indexed: 19)
    fields.get(19).and_then(|s| s.parse().ok())
}

/// Verify process identity before kill to mitigate PID reuse race condition
#[cfg(target_os = "linux")]
fn verify_process_identity(pid: i32, saved_starttime: Option<u64>) -> Result<(), String> {
    if let Some(saved) = saved_starttime {
        let current = get_process_starttime(pid).ok_or_else(|| {
            "Cannot verify process identity; refusing to signal a possibly reused PID".to_string()
        })?;
        if saved != current {
            return Err("Process PID was reused by a different process".to_string());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_process_identity(_pid: i32, _saved_starttime: Option<u64>) -> Result<(), String> {
    // On non-Linux platforms, skip starttime verification
    Ok(())
}

/// Kill a process by PID
pub fn kill_process(pid: i32) -> Result<(), String> {
    kill_process_with_verification(pid, None)
}

/// Kill a process by PID with optional starttime verification
#[cfg(unix)]
pub fn kill_process_with_verification(pid: i32, starttime: Option<u64>) -> Result<(), String> {
    if !is_valid_pid(pid) {
        return Err("Invalid PID".to_string());
    }

    // Get process info to check if it's a kernel thread
    let command = get_process_command(pid);
    is_protected_pid(pid, command.as_deref())?;

    // Verify process identity if starttime is provided (Linux only)
    #[cfg(target_os = "linux")]
    verify_process_identity(pid, starttime)?;
    #[cfg(not(target_os = "linux"))]
    let _ = starttime; // Suppress unused warning

    // Use libc kill for safety
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    if result == 0 {
        Ok(())
    } else {
        let errno = std::io::Error::last_os_error();
        match errno.raw_os_error() {
            Some(libc::ESRCH) => Err("Process not found".to_string()),
            Some(libc::EPERM) => Err("Permission denied".to_string()),
            _ => Err(errno.to_string()),
        }
    }
}

#[cfg(windows)]
pub fn kill_process_with_verification(pid: i32, _starttime: Option<u64>) -> Result<(), String> {
    if !is_valid_pid(pid) {
        return Err("Invalid PID".to_string());
    }

    let command = get_process_command(pid);
    is_protected_pid(pid, command.as_deref())?;

    let output = Command::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .output()
        .map_err(|e| format!("Failed to execute taskkill: {}", e))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(stderr.trim().to_string())
    }
}

/// Force kill a process by PID (SIGKILL)
pub fn force_kill_process(pid: i32) -> Result<(), String> {
    force_kill_process_with_verification(pid, None)
}

/// Force kill a process by PID (SIGKILL) with optional starttime verification
#[cfg(unix)]
pub fn force_kill_process_with_verification(
    pid: i32,
    starttime: Option<u64>,
) -> Result<(), String> {
    if !is_valid_pid(pid) {
        return Err("Invalid PID".to_string());
    }

    let command = get_process_command(pid);
    is_protected_pid(pid, command.as_deref())?;

    // Verify process identity if starttime is provided (Linux only)
    #[cfg(target_os = "linux")]
    verify_process_identity(pid, starttime)?;
    #[cfg(not(target_os = "linux"))]
    let _ = starttime; // Suppress unused warning

    let result = unsafe { libc::kill(pid, libc::SIGKILL) };
    if result == 0 {
        Ok(())
    } else {
        let errno = std::io::Error::last_os_error();
        match errno.raw_os_error() {
            Some(libc::ESRCH) => Err("Process not found".to_string()),
            Some(libc::EPERM) => Err("Permission denied".to_string()),
            _ => Err(errno.to_string()),
        }
    }
}

#[cfg(windows)]
pub fn force_kill_process_with_verification(
    pid: i32,
    _starttime: Option<u64>,
) -> Result<(), String> {
    if !is_valid_pid(pid) {
        return Err("Invalid PID".to_string());
    }

    let command = get_process_command(pid);
    is_protected_pid(pid, command.as_deref())?;

    let output = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output()
        .map_err(|e| format!("Failed to execute taskkill: {}", e))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(stderr.trim().to_string())
    }
}

/// Get process command by PID
#[cfg(unix)]
fn get_process_command(pid: i32) -> Option<String> {
    // Use "command=" format to suppress header (POSIX compatible, works on Linux and macOS)
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let command = stdout.trim();
    if command.is_empty() {
        None
    } else {
        Some(command.to_string())
    }
}

#[cfg(windows)]
fn get_process_command(pid: i32) -> Option<String> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    if line.is_empty() || line.contains("No tasks") {
        None
    } else {
        // First field is the image name
        line.split(',')
            .next()
            .map(|s| s.trim_matches('"').to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========== is_valid_pid tests ==========

    #[test]
    fn test_is_valid_pid_positive() {
        assert!(is_valid_pid(1));
        assert!(is_valid_pid(100));
        assert!(is_valid_pid(1000));
        #[cfg(unix)]
        assert!(is_valid_pid(4194304)); // Max PID on Linux
    }

    #[test]
    fn test_is_valid_pid_invalid() {
        assert!(!is_valid_pid(0));
        assert!(!is_valid_pid(-1));
        assert!(!is_valid_pid(-100));
        #[cfg(unix)]
        assert!(!is_valid_pid(4194305)); // Exceeds max PID on Linux
    }

    // ========== is_protected_pid tests ==========

    #[test]
    fn test_is_protected_pid_init() {
        // PID 1 is init/systemd and should be protected
        let result = is_protected_pid(1, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("system process"));
    }

    #[test]
    fn test_is_protected_pid_kthreadd() {
        // PID 2 is kthreadd and should be protected
        let result = is_protected_pid(2, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("system process"));
    }

    #[test]
    fn test_is_protected_pid_self() {
        // Current process should be protected
        let current_pid = std::process::id() as i32;
        let result = is_protected_pid(current_pid, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("file manager itself"));
    }

    #[test]
    fn test_is_protected_pid_low_pid() {
        // Low PIDs (< 300) are likely kernel threads
        let result = is_protected_pid(100, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("kernel thread"));
    }

    #[test]
    fn test_is_protected_pid_normal() {
        // Normal user process PIDs should be allowed
        // Use a high PID that's unlikely to be the current process
        let high_pid = 50000;
        if high_pid != std::process::id() as i32 {
            let result = is_protected_pid(high_pid, None);
            assert!(result.is_ok());
        }
    }

    // ========== kernel thread detection tests ==========

    #[test]
    fn test_kernel_thread_detection_bracket_format() {
        // Kernel threads have names like [kworker/0:0]
        let result = is_protected_pid(50000, Some("[kworker/0:0]"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("kernel threads"));
    }

    #[test]
    fn test_kernel_thread_detection_normal_process() {
        // Normal processes should pass
        let result = is_protected_pid(50000, Some("/usr/bin/bash"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_kernel_thread_detection_various_formats() {
        // Various kernel thread names
        assert!(is_protected_pid(50000, Some("[migration/0]")).is_err());
        assert!(is_protected_pid(50000, Some("[ksoftirqd/0]")).is_err());
        assert!(is_protected_pid(50000, Some("[rcu_sched]")).is_err());
    }

    // ========== parse_process_line tests ==========

    #[cfg(unix)]
    #[test]
    fn test_parse_process_line_valid() {
        let line = "root         1  0.0  0.1  12345  6789 ?        Ss   Jan01   0:05 /sbin/init";
        let result = parse_process_line(line);
        assert!(result.is_some());

        let info = result.unwrap();
        assert_eq!(info.pid, 1);
        assert_eq!(info.user, "root");
        assert_eq!(info.cpu, 0.0);
        assert_eq!(info.mem, 0.1);
        assert_eq!(info.command, "/sbin/init");
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_process_line_invalid_short() {
        let line = "root 1 0.0"; // Too few fields
        let result = parse_process_line(line);
        assert!(result.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_process_line_command_with_spaces() {
        let line = "user     12345  1.5  2.3  54321  9876 pts/0    S+   10:00   0:01 /usr/bin/program --arg value";
        let result = parse_process_line(line);
        assert!(result.is_some());

        let info = result.unwrap();
        assert_eq!(info.pid, 12345);
        assert_eq!(info.command, "/usr/bin/program --arg value");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_process_starttime_with_closing_parenthesis_in_command() {
        // Fields after comm begin at state (field 3); starttime is the 20th
        // field in that suffix (overall field 22).
        let stat = "123 (worker) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 20";
        assert_eq!(parse_process_starttime(stat), Some(424242));
    }

    // ========== SortField tests ==========

    #[test]
    fn test_sort_field_equality() {
        assert_eq!(SortField::Pid, SortField::Pid);
        assert_eq!(SortField::Cpu, SortField::Cpu);
        assert_eq!(SortField::Mem, SortField::Mem);
        assert_eq!(SortField::Command, SortField::Command);
        assert_ne!(SortField::Pid, SortField::Cpu);
    }

    // ========== ProcessInfo tests ==========

    #[test]
    fn test_process_info_clone() {
        let info = ProcessInfo {
            pid: 1234,
            start_time_ticks: Some(42),
            user: "test".to_string(),
            cpu: 1.5,
            mem: 2.5,
            vsz: 1000,
            rss: 500,
            tty: "pts/0".to_string(),
            stat: "S".to_string(),
            start: "10:00".to_string(),
            time: "0:01".to_string(),
            command: "test_cmd".to_string(),
        };

        let cloned = info.clone();
        assert_eq!(cloned.pid, info.pid);
        assert_eq!(cloned.start_time_ticks, info.start_time_ticks);
        assert_eq!(cloned.user, info.user);
        assert_eq!(cloned.command, info.command);
    }

    // ========== is_valid_session_id tests ==========

    #[test]
    fn test_session_id_basic() {
        assert!(is_valid_session_id("abc123"));
        assert!(is_valid_session_id("a"));
        assert!(is_valid_session_id("session-1"));
        assert!(is_valid_session_id("session_2"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id(&"a".repeat(65)));
    }

    #[test]
    fn test_session_id_argparse_injection_rejected() {
        assert!(!is_valid_session_id("-i"));
        assert!(!is_valid_session_id("--help"));
        assert!(!is_valid_session_id("--config"));
        assert!(!is_valid_session_id("-"));
        // A leading underscore is harmless to argparsers, keep it valid for
        // backward-compat with any pre-existing session ids on disk.
        assert!(is_valid_session_id("_internal"));
    }

    #[test]
    fn test_session_id_unicode_rejected() {
        assert!(!is_valid_session_id("세션"));
        assert!(!is_valid_session_id("session_日本語"));
    }
}
