//! Error type for the vhost-user protocol layer.

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("unknown vhost-user request code {0}")]
    UnknownRequest(u32),

    #[error("message payload truncated")]
    Truncated,

    #[error("peer closed the connection")]
    Disconnected,

    #[error("message payload too large ({0} bytes)")]
    PayloadTooLarge(usize),

    #[error("expected an fd attached to this message, but none was received")]
    MissingFd,

    #[cfg(unix)]
    #[error("virtqueue error: {0}")]
    Virtqueue(#[from] crate::vhost_user::virtqueue::VirtqueueError),
}

pub type ProtoResult<T> = Result<T, ProtoError>;
