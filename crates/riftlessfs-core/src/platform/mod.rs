//! Platform-specific primitives.
//!
//! The passthrough engine keeps one lightweight "reference" file descriptor
//! per inode, opened with the minimum possible rights: `O_PATH` on Linux
//! (doesn't require any read/write permission at all and works uniformly
//! for every file type), or `O_EVTONLY`/`O_SYMLINK` on macOS, which has no
//! `O_PATH` equivalent.
//!
//! When an operation needs *real* read/write access (`open`, `opendir`,
//! truncating via `setattr`), we do **not** try to "reopen the reference fd
//! with new flags" via `/proc/self/fd` or `/dev/fd` -- empirically, on
//! macOS that can only *narrow* access relative to how the original fd was
//! opened, never widen it (confirmed: reopening an `O_RDONLY`-opened fd via
//! `/dev/fd/N` with `O_WRONLY` fails with `EACCES`), and on Linux it's only
//! guaranteed to work for `O_PATH` fds specifically. Instead,
//! [`crate::passthrough::PassthroughFs::reopen_via_parent`] walks back to
//! `(parent dirfd, name)` and does a fresh `openat()`, which works
//! identically on both platforms.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos::*;

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
mod generic;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
use generic::*;

/// The minimal set of open flags to use for the "reference" handle kept
/// alive per-inode. On Linux this is `O_PATH` (doesn't even require read
/// permission and works for any file type, including sockets/FIFOs without
/// triggering blocking semantics). macOS has no `O_PATH`, so we fall back to
/// `O_EVTONLY` for regular files/directories, and `O_SYMLINK` for symlinks.
pub fn reference_open_flags(is_dir: bool, is_symlink: bool) -> libc::c_int {
    let base = if is_dir { libc::O_DIRECTORY } else { 0 };
    if is_symlink {
        symlink_open_flags()
    } else {
        base | reference_open_flags_impl()
    }
}
