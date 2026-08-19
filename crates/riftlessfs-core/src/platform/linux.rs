//! Linux-specific primitives.

/// `O_PATH` gives us a handle that doesn't require read/write permission,
/// can't be used for I/O directly, but *can* be used as the `dirfd` argument
/// to every `*at()` syscall and re-opened via `/proc/self/fd/N` -- exactly
/// what the passthrough engine needs for its per-inode reference handle.
pub(crate) fn reference_open_flags_impl() -> libc::c_int {
    libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC
}

/// `O_PATH` already refers to the symlink itself without following it, so
/// no special flag is needed beyond what `reference_open_flags_impl`
/// provides.
pub(crate) fn symlink_open_flags() -> libc::c_int {
    libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC
}
