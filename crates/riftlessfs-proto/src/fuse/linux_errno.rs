//! Translate this **host's** errno values into **Linux** errno values.
//!
//! This was found the hard way: riftlessfsd running on macOS, mounted from
//! a real Fedora guest over a real vhost-user-fs/QEMU connection, failed
//! with `mount: /mnt/rfs: fsconfig() failed: Remote address changed.` --
//! which makes no sense for a local mount, until you notice that Linux's
//! `strerror(78)` is `EREMCHG` ("Remote address changed"), and macOS's
//! `ENOSYS` is *also* `78`. Our `GETXATTR` handler correctly replied with
//! `-ENOSYS`, but that constant was `libc::ENOSYS` compiled for the
//! *host* (macOS) target, i.e. `78` -- which the Linux guest kernel
//! receiving it over the wire interpreted using *its own* errno table, as
//! `EREMCHG`, not `ENOSYS`.
//!
//! The FUSE wire protocol is defined by the Linux kernel ABI, so every
//! `fuse_out_header.error` we send must be a **Linux** errno number,
//! regardless of what platform riftlessfsd itself is compiled for. On
//! Linux hosts this translation is the identity function; on macOS (whose
//! low (POSIX-common) errno values mostly match Linux's, but diverge a lot
//! past roughly EDEADLK) it actually does something. This module is the
//! only place that distinction should matter -- everywhere else in the
//! codebase, errno means "whatever this host's libc uses", which is
//! correct for actual syscalls.
use riftlessfs_core::FsError;

/// Translate a host errno (as returned by [`FsError::errno`]) into the
/// Linux errno value that must go on the wire in a `fuse_out_header`.
pub fn to_linux_errno(host_errno: i32) -> i32 {
    // On Linux, this is the identity mapping (every arm below reduces to
    // `x => x`), so this function is a no-op there.
    match host_errno {
        e if e == libc::EPERM => 1,
        e if e == libc::ENOENT => 2,
        e if e == libc::ESRCH => 3,
        e if e == libc::EINTR => 4,
        e if e == libc::EIO => 5,
        e if e == libc::ENXIO => 6,
        e if e == libc::E2BIG => 7,
        e if e == libc::ENOEXEC => 8,
        e if e == libc::EBADF => 9,
        e if e == libc::ECHILD => 10,
        e if e == libc::EAGAIN => 11,
        e if e == libc::ENOMEM => 12,
        e if e == libc::EACCES => 13,
        e if e == libc::EFAULT => 14,
        e if e == libc::EBUSY => 16,
        e if e == libc::EEXIST => 17,
        e if e == libc::EXDEV => 18,
        e if e == libc::ENODEV => 19,
        e if e == libc::ENOTDIR => 20,
        e if e == libc::EISDIR => 21,
        e if e == libc::EINVAL => 22,
        e if e == libc::ENFILE => 23,
        e if e == libc::EMFILE => 24,
        e if e == libc::ENOTTY => 25,
        e if e == libc::ETXTBSY => 26,
        e if e == libc::EFBIG => 27,
        e if e == libc::ENOSPC => 28,
        e if e == libc::ESPIPE => 29,
        e if e == libc::EROFS => 30,
        e if e == libc::EMLINK => 31,
        e if e == libc::EPIPE => 32,
        e if e == libc::EDOM => 33,
        e if e == libc::ERANGE => 34,
        e if e == libc::EDEADLK => 35,
        // Past here, Linux and macOS/BSD errno numbering genuinely
        // diverge (this project's namesake bug lives in this range).
        e if e == libc::ENAMETOOLONG => 36,
        e if e == libc::ENOLCK => 37,
        e if e == libc::ENOSYS => 38,
        e if e == libc::ENOTEMPTY => 39,
        e if e == libc::ELOOP => 40,
        e if e == libc::ENOMSG => 42,
        e if e == libc::EIDRM => 43,
        e if e == libc::EOVERFLOW => 75,
        e if e == libc::EBADMSG => 74,
        e if e == libc::EILSEQ => 84,
        e if e == libc::ENOTSOCK => 88,
        e if e == libc::EDESTADDRREQ => 89,
        e if e == libc::EMSGSIZE => 90,
        e if e == libc::EPROTOTYPE => 91,
        e if e == libc::ENOPROTOOPT => 92,
        e if e == libc::EPROTONOSUPPORT => 93,
        // ENOTSUP and EOPNOTSUPP are the same value on Linux (95) but can
        // differ on other platforms (e.g. macOS: 45 vs 102); one arm
        // covers both without risking an "unreachable pattern" lint on
        // platforms where they coincide.
        e if e == libc::ENOTSUP || e == libc::EOPNOTSUPP => 95,
        e if e == libc::EAFNOSUPPORT => 97,
        e if e == libc::EADDRINUSE => 98,
        e if e == libc::EADDRNOTAVAIL => 99,
        e if e == libc::ENETDOWN => 100,
        e if e == libc::ENETUNREACH => 101,
        e if e == libc::ENETRESET => 102,
        e if e == libc::ECONNABORTED => 103,
        e if e == libc::ECONNRESET => 104,
        e if e == libc::ENOBUFS => 105,
        e if e == libc::EISCONN => 106,
        e if e == libc::ENOTCONN => 107,
        e if e == libc::ETIMEDOUT => 110,
        e if e == libc::ECONNREFUSED => 111,
        e if e == libc::EHOSTUNREACH => 113,
        e if e == libc::EALREADY => 114,
        e if e == libc::EINPROGRESS => 115,
        e if e == libc::ESTALE => 116,
        e if e == libc::EDQUOT => 122,
        e if e == libc::ECANCELED => 125,
        // Unknown/unmapped: EIO is a safe, generic "something went
        // wrong" rather than silently forwarding a raw host-specific
        // number that might collide with an unrelated Linux errno.
        _ => 5,
    }
}

/// Convenience wrapper: translate an [`FsError`]'s host errno into the
/// Linux errno to send on the wire.
pub fn fs_error_to_linux_errno(e: &FsError) -> i32 {
    to_linux_errno(e.errno())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_low_errnos_match_posix_standard_values() {
        assert_eq!(to_linux_errno(libc::ENOENT), 2);
        assert_eq!(to_linux_errno(libc::EACCES), 13);
        assert_eq!(to_linux_errno(libc::EEXIST), 17);
        assert_eq!(to_linux_errno(libc::EINVAL), 22);
        assert_eq!(to_linux_errno(libc::ENOTDIR), 20);
        assert_eq!(to_linux_errno(libc::EISDIR), 21);
        assert_eq!(to_linux_errno(libc::EROFS), 30);
    }

    /// The bug that motivated this module, pinned down as a regression
    /// test: ENOSYS must become Linux's 38 no matter what this host's own
    /// ENOSYS numeric value is (78 on macOS, 38 on Linux).
    #[test]
    fn enosys_is_always_linux_38() {
        assert_eq!(to_linux_errno(libc::ENOSYS), 38);
    }

    #[test]
    fn unmapped_errno_falls_back_to_eio_not_a_raw_passthrough() {
        assert_eq!(to_linux_errno(999_999), 5);
    }
}
