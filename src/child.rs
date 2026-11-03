//! Signalling of the `varnishncsa` child through a pidfd, so a recycled pid can never be signalled.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Time the child gets to exit after SIGTERM before it is killed.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// The running child, set once after it is spawned.
pub type Slot = Arc<OnceLock<Arc<Child>>>;

pub struct Child {
    pid: i32,
    pidfd: Option<OwnedFd>,
    reaped: AtomicBool,
}

fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open(2) takes plain integers; on success the returned descriptor is owned by nobody else.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

impl Child {
    /// Opens the pidfd right after the spawn, while the unreaped child still holds its pid.
    /// Only a kernel without pidfd support (or a seccomp filter refusing it) falls back to `kill(2)`.
    pub fn new(pid: u32) -> Child {
        let pid = pid as i32;
        let pidfd = match pidfd_open(pid) {
            Ok(fd) => Some(fd),
            Err(err) => {
                if err.raw_os_error() != Some(libc::ENOSYS) {
                    eprintln!("varnish-tracer: pidfd_open failed ({err}), signalling by pid");
                }
                None
            }
        };
        Child {
            pid,
            pidfd,
            reaped: AtomicBool::new(false),
        }
    }

    /// Must be called once the child has been waited for; the `kill(2)` fallback stops signalling then.
    pub fn mark_reaped(&self) {
        self.reaped.store(true, Relaxed);
    }

    pub fn is_reaped(&self) -> bool {
        self.reaped.load(Relaxed)
    }

    #[cfg(test)]
    pub fn uses_pidfd(&self) -> bool {
        self.pidfd.is_some()
    }

    /// Fails with ESRCH once the child has exited.
    pub fn signal(&self, signal: i32) -> io::Result<()> {
        if let Some(fd) = &self.pidfd {
            // SAFETY: the descriptor is valid for the lifetime of `self`; a null siginfo is allowed.
            let sent = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if sent == 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ENOSYS) {
                return Err(err);
            }
        }
        if self.is_reaped() {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        // SAFETY: kill(2) has no memory-safety preconditions.
        if unsafe { libc::kill(self.pid, signal) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Sends SIGTERM to the child and kills it if it is still unreaped after `STOP_TIMEOUT`.
pub fn stop(slot: &Slot) {
    let Some(child) = slot.get() else { return };
    if child.is_reaped() {
        return;
    }
    let _ = child.signal(libc::SIGTERM);
    let child = child.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            if child.is_reaped() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !child.is_reaped() {
            let _ = child.signal(libc::SIGKILL);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    #[test]
    fn signals_the_child_and_never_a_reaped_pid() {
        let mut process = Command::new("sleep").arg("30").spawn().unwrap();
        let child = Child::new(process.id());
        child.signal(libc::SIGTERM).unwrap();
        let status = process.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        // The pidfd refers to the dead process, not to whatever reuses its pid.
        let err = child.signal(libc::SIGKILL).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ESRCH));
        child.mark_reaped();
        assert!(child.signal(libc::SIGKILL).is_err());
    }

    #[test]
    fn pidfd_is_used_when_the_kernel_has_it() {
        let mut process = Command::new("sleep").arg("30").spawn().unwrap();
        let child = Child::new(process.id());
        let supported = pidfd_open(process.id() as i32).is_ok();
        assert_eq!(child.uses_pidfd(), supported);
        process.kill().unwrap();
        process.wait().unwrap();
    }
}
