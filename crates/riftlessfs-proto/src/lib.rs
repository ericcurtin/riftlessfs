//! Portable vhost-user / virtio-fs wire protocol implementation.
//!
//! **Status: partial.** The message framing layer (header + payload
//! (de)serialization, `SCM_RIGHTS`-aware socket transport) is implemented
//! and tested. What's *not* implemented yet is the part that makes this
//! actually useful: mapping guest memory from `SET_MEM_TABLE`, parsing
//! virtqueues out of it, decoding FUSE-over-virtio requests, and
//! dispatching them to [`riftlessfs_core::PassthroughFs`]. There is no
//! mountable filesystem yet.
//!
//! The obvious way to implement this (the `vhost`, `vhost-user-backend`,
//! `vm-memory` crates from the rust-vmm project -- what upstream
//! `virtiofsd` itself uses) does not compile on macOS: it depends on Linux
//! `eventfd(2)` and `SO_DOMAIN`, neither of which exist there. See the
//! workspace README for how that was verified. This crate hand-rolls the
//! subset of the protocol needed instead, on top of:
//!
//! - `std::os::unix::net::UnixStream` + the [`sendfd`] crate for
//!   `SCM_RIGHTS` fd passing (works identically on Linux and macOS).
//! - `libc::mmap` for mapping guest memory (not yet wired up).
//! - A hand-written [`doorbell::Doorbell`] for the cases where *we* need
//!   to create an eventfd-like signal (internal coordination, and
//!   simulating a front-end in tests) -- turns out the main kick/call
//!   notification path never needs us to create one at all, since those
//!   fds are always supplied by the front-end and treated with plain
//!   POSIX read/write/poll regardless of what kind of fd they are.
//!
//! This is Unix-only for the same reason `riftlessfs-core` is: Windows
//! `AF_UNIX` sockets don't support `SCM_RIGHTS` ancillary data at all, so
//! vhost-user as a protocol isn't implementable there the same way. See
//! the workspace README's Phase 3 notes.

#[cfg(unix)]
pub mod doorbell;
pub mod error;
#[cfg(unix)]
pub mod fuse;
#[cfg(unix)]
pub mod vhost_user;

pub use error::{ProtoError, ProtoResult};

/// `true` on platforms where this crate's protocol implementation
/// actually exists. See the module-level docs for why Windows can't use
/// vhost-user at all.
pub const VHOST_USER_SUPPORTED: bool = cfg!(unix);
