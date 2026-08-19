//! `riftlessfs-core`: a transport-agnostic passthrough filesystem engine.
//!
//! This crate implements the actual filesystem semantics (lookup, read,
//! write, readdir, rename, ...) against a real "shared directory" on the
//! host, using fd-relative syscalls throughout. It knows nothing about
//! FUSE, virtio-fs, or any wire protocol -- those live in `riftlessfs-proto`
//! and `riftlessfsd`, which translate client requests into calls on
//! [`PassthroughFs`] and translate [`FsError`] back into whatever error
//! representation the transport needs.
//!
//! Only Unix-like hosts (Linux, macOS) are supported today; see
//! `platform/mod.rs` for why, and the workspace README for the Windows
//! roadmap.

pub mod attr;
pub mod error;

#[cfg(unix)]
mod handle;
#[cfg(unix)]
mod inode;
#[cfg(unix)]
mod passthrough;
#[cfg(unix)]
mod platform;

#[cfg(unix)]
pub use inode::ROOT_ID;
#[cfg(unix)]
pub use passthrough::{DirEntry, PassthroughFs};

pub use attr::{Attr, SetAttr};
pub use error::{FsError, FsResult};

/// `true` on platforms where [`PassthroughFs`] is actually implemented.
///
/// Windows is not yet supported: the on-the-wire transport riftlessfs uses
/// (vhost-user, see `riftlessfs-proto`) relies on Unix-domain-socket file
/// descriptor passing, which Windows sockets don't support, so Windows will
/// need a different transport before this engine is useful there. See the
/// workspace README for the current roadmap. This constant lets downstream
/// crates (and CI) compile everywhere while failing loudly at runtime
/// rather than silently doing nothing.
pub const PASSTHROUGH_SUPPORTED: bool = cfg!(unix);
