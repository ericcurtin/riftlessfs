//! FUSE-over-virtio: decoding FUSE requests out of virtqueue descriptor
//! chains and dispatching them to [`riftlessfs_core::PassthroughFs`].
//!
//! - [`wire`]: FUSE ABI structs, checked against upstream `fuse.h`.
//! - [`dispatch::Session`]: decodes a request, calls the matching
//!   `PassthroughFs` operation, encodes the reply.
//! - [`linux_errno`]: translates this host's errno values to the Linux
//!   errno values the wire protocol requires -- **read this module's
//!   docs**; getting it wrong produces confusing failures on non-Linux
//!   hosts (this is exactly how the bug it fixes was found).
//! - [`bytes`]: a small cursor reader/writer used by `wire`.

pub mod bytes;
pub mod dispatch;
pub mod linux_errno;
pub mod wire;
