//! A small cross-platform "signal an fd, then have someone else notice"
//! primitive.
//!
//! In the real vhost-user protocol, kick/call/err doorbell fds are
//! *created by the front-end* (QEMU or whatever VMM) and handed to us via
//! `SCM_RIGHTS`; we just `read()`/`write()`/`poll()` them with plain POSIX
//! calls; we never need to create one ourselves in that path, and the
//! generic read/write/poll operations work identically regardless of
//! whether the front-end's fd happens to be a Linux `eventfd`, a pipe, or
//! something else -- that part of the earlier "eventfd doesn't exist on
//! macOS" concern turned out not to matter for the main protocol flow.
//!
//! Where a portable "create a doorbell" primitive *is* still useful:
//! internal daemon coordination (e.g. waking a blocked poll loop for
//! shutdown) and, here, simulating a front-end/back-end pair in tests
//! without needing a real VMM. On Linux this uses a real `eventfd(2)`; on
//! other Unix platforms (macOS included), a self-pipe.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub struct Doorbell {
    #[cfg(target_os = "linux")]
    fd: OwnedFd,
    #[cfg(not(target_os = "linux"))]
    read_fd: OwnedFd,
    #[cfg(not(target_os = "linux"))]
    write_fd: OwnedFd,
}

impl Doorbell {
    pub fn new() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Doorbell {
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut fds = [0i32; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
                return Err(io::Error::last_os_error());
            }
            for fd in fds {
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            }
            Ok(Doorbell {
                read_fd: unsafe { OwnedFd::from_raw_fd(fds[0]) },
                write_fd: unsafe { OwnedFd::from_raw_fd(fds[1]) },
            })
        }
    }

    /// The fd to hand to a poller (`poll()`/`kqueue`/`epoll`) that should
    /// wake up on [`notify`](Self::notify).
    pub fn read_fd(&self) -> RawFd {
        #[cfg(target_os = "linux")]
        {
            self.fd.as_raw_fd()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.read_fd.as_raw_fd()
        }
    }

    /// Signal the doorbell.
    pub fn notify(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let one: u64 = 1;
            let rc = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    &one as *const u64 as *const libc::c_void,
                    8,
                )
            };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let one: u8 = 1;
            let rc = unsafe {
                libc::write(
                    self.write_fd.as_raw_fd(),
                    &one as *const u8 as *const libc::c_void,
                    1,
                )
            };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Drain any pending notifications, returning whether there were any.
    /// Non-blocking.
    pub fn consume(&self) -> io::Result<bool> {
        let fd = self.read_fd();
        let mut buf = [0u8; 256];
        let mut any = false;
        loop {
            let rc = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if rc > 0 {
                any = true;
                if rc as usize == buf.len() {
                    continue; // there may be more
                }
                break;
            } else if rc == 0 {
                break;
            } else {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(err);
            }
        }
        Ok(any)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_then_consume() {
        let d = Doorbell::new().unwrap();
        assert!(!d.consume().unwrap());
        d.notify().unwrap();
        d.notify().unwrap();
        assert!(d.consume().unwrap());
        // Fully drained now.
        assert!(!d.consume().unwrap());
    }
}
