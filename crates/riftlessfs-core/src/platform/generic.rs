//! Fallback primitives for other POSIX-ish platforms (\*BSD, etc). Not
//! covered by CI; provided so the crate at least compiles elsewhere. Expect
//! rough edges -- patches welcome.

pub(crate) fn reference_open_flags_impl() -> libc::c_int {
    libc::O_RDONLY | libc::O_CLOEXEC
}

pub(crate) fn symlink_open_flags() -> libc::c_int {
    libc::O_RDONLY | libc::O_CLOEXEC
}
