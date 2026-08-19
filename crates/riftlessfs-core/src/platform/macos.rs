//! macOS-specific primitives.
//!
//! macOS has no `O_PATH`. `O_EVTONLY` is the closest analogue for the
//! per-inode reference handle: it doesn't require read/write permission on
//! the target and doesn't count as an "in use" reference for e.g.
//! unmounting, but (unlike `O_PATH`) it can't be escalated to a
//! read/write fd via `/dev/fd/N` -- see the `platform` module docs for why
//! that's fine (we never rely on that).

/// `O_EVTONLY` tells the kernel this descriptor is not going to be used to
/// read/write file *contents* and shouldn't count as an "in use" reference
/// for e.g. unmounting -- close enough to `O_PATH` for our reference-handle
/// use case without needing full read permission on the target.
pub(crate) fn reference_open_flags_impl() -> libc::c_int {
    libc::O_EVTONLY | libc::O_CLOEXEC
}

/// `O_SYMLINK` opens the symlink object itself rather than its target.
pub(crate) fn symlink_open_flags() -> libc::c_int {
    libc::O_SYMLINK | libc::O_CLOEXEC
}
