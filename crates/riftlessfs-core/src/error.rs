//! Error type for the passthrough filesystem core.
//!
//! Every operation in this crate returns [`FsError`], which carries a POSIX
//! errno so that any transport (FUSE, virtio-fs, a custom wire protocol...)
//! can translate it back to whatever the client expects without this crate
//! needing to know about any particular transport.

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("I/O error (errno {errno}): {source}")]
    Io { errno: i32, source: io::Error },

    #[error("unknown inode {0}")]
    UnknownInode(u64),

    #[error("unknown file handle {0}")]
    UnknownHandle(u64),

    #[error("operation not supported: {0}")]
    Unsupported(&'static str),

    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),

    #[error("name too long")]
    NameTooLong,

    #[error("path escapes shared directory root")]
    PathEscape,
}

impl FsError {
    /// The POSIX errno that best represents this error, suitable for
    /// returning to a FUSE/virtio-fs client.
    pub fn errno(&self) -> i32 {
        match self {
            FsError::Io { errno, .. } => *errno,
            FsError::UnknownInode(_) => libc::EBADF,
            FsError::UnknownHandle(_) => libc::EBADF,
            FsError::Unsupported(_) => libc::ENOSYS,
            FsError::InvalidArgument(_) => libc::EINVAL,
            FsError::NameTooLong => libc::ENAMETOOLONG,
            FsError::PathEscape => libc::EACCES,
        }
    }
}

impl From<io::Error> for FsError {
    fn from(source: io::Error) -> Self {
        let errno = source.raw_os_error().unwrap_or(libc::EIO);
        FsError::Io { errno, source }
    }
}

#[cfg(unix)]
impl From<nix::Error> for FsError {
    fn from(e: nix::Error) -> Self {
        FsError::Io {
            errno: e as i32,
            source: io::Error::from_raw_os_error(e as i32),
        }
    }
}

pub type FsResult<T> = Result<T, FsError>;
