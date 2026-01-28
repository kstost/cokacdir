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

/// Protected PIDs that should never be killed
const PROTECTED_PIDS: &[i32] = &[1, 2];

/// Minimum PID threshold - PIDs below this are likely kernel threads
const MIN_SAFE_PID: i32 = 300;

/// Validate PID is a safe positive integer
fn is_valid_pid(pid: i32) -> bool {
    pid > 0 && pid <= 4194304 // Max PID on Linux
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
        return Err(format!("Cannot kill low PID ({}) - likely a kernel thread", pid));
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
pub fn get_process_list_result() -> ProcessListResult {
    let output = Command::new("ps")
        .args(["aux", "--no-headers"])
        .output()
        .map_err(|e| format!("Failed to execute ps command: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ps command failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut processes: Vec<ProcessInfo> = stdout
        .lines()
        .filter_map(parse_process_line)
        .collect();

    // Sort by CPU usage descending by default
    processes.sort_by(|a, b| {
        b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(processes)
}

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

    Some(ProcessInfo {
        pid,
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

/// Get process start time from /proc/[pid]/stat for additional PID validation
#[cfg(target_os = "linux")]
fn get_process_starttime(pid: i32) -> Option<u64> {
    let stat_path = format!("/proc/{}/stat", pid);
    let content = std::fs::read_to_string(stat_path).ok()?;

    // Field 22 (0-indexed: 21) is starttime
    // Format: pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt
    //         utime stime cutime cstime priority nice num_threads itrealvalue starttime ...

    // Find the closing parenthesis of comm field (which may contain spaces)
    let comm_end = content.find(')')?;
    let after_comm = &content[comm_end + 2..]; // Skip ") "
    let fields: Vec<&str> = after_comm.split_whitespace().collect();

    // starttime is field 20 after comm (0-indexed: 19)
    fields.get(19).and_then(|s| s.parse().ok())
}

/// Verify process identity before kill to mitigate PID reuse race condition
#[cfg(target_os = "linux")]
fn verify_process_identity(pid: i32, saved_starttime: Option<u64>) -> Result<(), String> {
    if let Some(saved) = saved_starttime {
        if let Some(current) = get_process_starttime(pid) {
            if saved != current {
                return Err("Process PID was reused by a different process".to_string());
            }
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

/// Force kill a process by PID (SIGKILL)
pub fn force_kill_process(pid: i32) -> Result<(), String> {
    force_kill_process_with_verification(pid, None)
}

/// Force kill a process by PID (SIGKILL) with optional starttime verification
pub fn force_kill_process_with_verification(pid: i32, starttime: Option<u64>) -> Result<(), String> {
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

/// Get process command by PID
fn get_process_command(pid: i32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command", "--no-headers"])
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
