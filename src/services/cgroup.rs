//! cgroup v2 based bulletproof cancellation for spawned AI CLI children.
//!
//! 0.6.8 introduced process-group separation + negative-PID kill so cancel
//! signals reach grandchildren that inherit their parent's pgroup. 0.6.9
//! switched the signal from SIGTERM to SIGKILL so a graceful-shutdown handler
//! can't trap it. Both fixes cover descendants that stay in the spawned CLI's
//! process group — but Claude Code's Bash tool itself spawns its subprocess
//! with `child_process.spawn({ detached: true })`, which calls `setsid()` and
//! moves the bash subprocess into a brand-new session and pgroup. From that
//! point cokacdir's `kill(-pgroup, SIGKILL)` can't reach it: the bash and its
//! python/find/tar descendants survive as orphans, holding tokio
//! blocking-pool slots and billable API state.
//!
//! cgroup v2 closes that escape hatch. cgroup membership is inherited at
//! fork() time, propagates across exec/setsid/setpgid/double-fork, and cannot
//! be left by an unprivileged process. `cgroup.kill` is a kernel-atomic
//! "SIGKILL every member of this cgroup" operation, so the entire descendant
//! tree dies in one syscall regardless of any session/pgroup gymnastics the
//! AI CLI performs.
//!
//! Per-spawn lifecycle:
//! 1. Create a unique sub-cgroup under cokacdir's own cgroup
//!    (`/sys/fs/cgroup/<cokacdir-path>/cancel_<unique>/`).
//! 2. In `Command::pre_exec` (post-fork, pre-exec), write the child's own PID
//!    to `cgroup.procs`. This must happen before exec because cgroup
//!    membership is inherited at fork time — moving the parent after children
//!    exist leaves those children in the old cgroup.
//! 3. On cancel, write "1" to `cgroup.kill` — kernel SIGKILLs every PID in
//!    the cgroup, atomically.
//! 4. On drop, rmdir the cgroup directory (best-effort; brief retry to wait
//!    for the kernel to finish reaping killed processes).
//!
//! Falls back to None / no-op on non-Linux or when cgroup v2 isn't available
//! / permission denied — callers must keep the pgroup-based kill path as a
//! safety net for those environments.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Discover cokacdir's own cgroup v2 directory under /sys/fs/cgroup.
///
/// Reads /proc/self/cgroup for the "0::" line (cgroup v2 unified hierarchy)
/// and prepends `/sys/fs/cgroup`. Cached for the lifetime of the process.
/// Returns None if cgroup v2 isn't mounted, the file is unreadable, or no
/// v2 entry is present.
#[cfg(target_os = "linux")]
pub fn self_cgroup_root() -> Option<&'static Path> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let contents = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        for line in contents.lines() {
            // cgroup v2 unified hierarchy lines start with "0::"
            if let Some(rel) = line.strip_prefix("0::") {
                let rel = rel.trim();
                if rel.is_empty() || !rel.starts_with('/') {
                    return None;
                }
                // /proc/self/cgroup gives a path relative to /sys/fs/cgroup
                let mut abs = PathBuf::from("/sys/fs/cgroup");
                // strip leading '/' to avoid PathBuf::push replacing the base
                abs.push(rel.trim_start_matches('/'));
                if abs.is_dir() {
                    return Some(abs);
                }
            }
        }
        None
    })
    .as_deref()
}

#[cfg(not(target_os = "linux"))]
pub fn self_cgroup_root() -> Option<&'static Path> {
    None
}

/// Generate a unique cgroup directory name.
fn unique_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("cancel_{:x}_{}_{}", nanos, std::process::id(), n)
}

/// A per-spawn cgroup whose only purpose is to be `cgroup.kill`-ed when
/// cancellation fires. Membership is established via `attach_command`, which
/// installs a `pre_exec` hook on the `Command` so the child writes its own
/// PID to `cgroup.procs` immediately after fork. All descendants (across
/// setsid, setpgid, double-fork) automatically inherit this cgroup.
///
/// `Drop` rmdirs the cgroup directory (best-effort).
pub struct KillCgroup {
    #[cfg(target_os = "linux")]
    path: PathBuf,
}

#[cfg(target_os = "linux")]
impl KillCgroup {
    /// Try to create a new per-spawn cgroup under cokacdir's own cgroup.
    /// Returns None on any failure (no cgroup v2, permission denied, etc.) so
    /// the caller can transparently fall back to pgroup-based kill.
    pub fn new() -> Option<Self> {
        let root = self_cgroup_root()?;
        let path = root.join(unique_name());
        std::fs::create_dir(&path).ok()?;
        Some(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Build the post-fork pre-exec closure that writes the calling
    /// process's PID to this cgroup's `cgroup.procs`. Pulled out so it can
    /// be installed on either a `std::process::Command` (default) or a
    /// `tokio::process::Command` (opencode serve path) — both expose the
    /// same `unsafe pre_exec` API but require their own attach methods.
    ///
    /// The closure is signal-safe: only async-signal-safe libc primitives
    /// (open/write/close/getpid), no Rust heap allocation, no panics. Any
    /// failure path silently falls through so the spawn succeeds even when
    /// cgroup attachment fails — losing the cgroup safety net is preferable
    /// to failing the spawn outright (the pgroup fallback still applies).
    fn build_pre_exec_closure(
        &self,
    ) -> Option<impl FnMut() -> std::io::Result<()> + Send + Sync + 'static> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let procs_path = self.path.join("cgroup.procs");
        let path_cstr = CString::new(procs_path.as_os_str().as_bytes()).ok()?;

        Some(move || {
            // SAFETY: post-fork pre-exec context. Only async-signal-safe
            // libc primitives, no heap allocation, no panics.
            unsafe {
                let fd = libc::open(path_cstr.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd < 0 {
                    return Ok(());
                }
                let pid = libc::getpid();
                // Format PID right-aligned into a stack buffer (no allocation).
                let mut buf = [0u8; 32];
                let mut idx = buf.len();
                let mut n = pid as i64;
                let neg = n < 0;
                if neg {
                    n = -n;
                }
                loop {
                    idx -= 1;
                    buf[idx] = b'0' + (n % 10) as u8;
                    n /= 10;
                    if n == 0 {
                        break;
                    }
                }
                if neg {
                    idx -= 1;
                    buf[idx] = b'-';
                }
                let written = buf.len() - idx;
                let _ = libc::write(fd, buf.as_ptr().add(idx) as *const _, written);
                let _ = libc::write(fd, b"\n".as_ptr() as *const _, 1);
                libc::close(fd);
            }
            Ok(())
        })
    }

    /// Install the cgroup-attach `pre_exec` hook on a `std::process::Command`.
    /// See `build_pre_exec_closure` for behavior/safety notes.
    pub fn attach_command(&self, cmd: &mut Command) {
        use std::os::unix::process::CommandExt;
        let hook = match self.build_pre_exec_closure() {
            Some(h) => h,
            None => return,
        };
        unsafe {
            cmd.pre_exec(hook);
        }
    }

    /// Install the cgroup-attach `pre_exec` hook on a `tokio::process::Command`.
    /// Mirrors `attach_command` for the async path (opencode serve). Tokio's
    /// `pre_exec` has the same `unsafe fn` signature as std's, so the closure
    /// type is compatible.
    pub fn attach_tokio_command(&self, cmd: &mut tokio::process::Command) {
        let hook = match self.build_pre_exec_closure() {
            Some(h) => h,
            None => return,
        };
        unsafe {
            cmd.pre_exec(hook);
        }
    }

    /// Atomically SIGKILL every process currently in this cgroup. Returns
    /// `true` if the kernel accepted the request. cgroup v2's `cgroup.kill`
    /// is single-syscall and uncatchable; the kernel reaps members on the
    /// next scheduler tick.
    pub fn kill_all(&self) -> bool {
        std::fs::write(self.path.join("cgroup.kill"), b"1").is_ok()
    }
}

#[cfg(target_os = "linux")]
impl Drop for KillCgroup {
    fn drop(&mut self) {
        // cgroup directories cannot be rmdir'd while they contain processes.
        // After a successful cgroup.kill the kernel needs a moment to reap;
        // a short retry loop keeps the filesystem from accumulating stale
        // empty cgroup dirs without blocking the caller for long.
        for attempt in 0..5 {
            if std::fs::remove_dir(&self.path).is_ok() {
                return;
            }
            if attempt < 4 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

// Non-Linux stubs: cgroup v2 is Linux-only. Callers gracefully fall back to
// the existing pgroup-based kill via `detach_into_own_pgroup` + SIGKILL.
#[cfg(not(target_os = "linux"))]
impl KillCgroup {
    pub fn new() -> Option<Self> {
        None
    }
    pub fn path(&self) -> &Path {
        unreachable!("KillCgroup::new returns None on non-Linux")
    }
    pub fn attach_command(&self, _cmd: &mut Command) {}
    pub fn attach_tokio_command(&self, _cmd: &mut tokio::process::Command) {}
    pub fn kill_all(&self) -> bool {
        false
    }
}
