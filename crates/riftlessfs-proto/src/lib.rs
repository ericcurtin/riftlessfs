//! Portable vhost-user / virtio-fs wire protocol implementation.
//!
//! **Status: work in progress, not yet functional.**
//!
//! The obvious way to implement this (the `vhost`, `vhost-user-backend`,
//! `vm-memory` crates from the rust-vmm project -- what upstream
//! `virtiofsd` itself uses) does not compile on macOS: it depends on Linux
//! `eventfd(2)` and `SO_DOMAIN`, neither of which exist there. See the
//! workspace README for details and the plan to hand-roll a portable
//! subset of the protocol (UNIX-domain-socket + `SCM_RIGHTS` fd passing +
//! `mmap`, all of which *are* available on both Linux and macOS, plus a
//! pipe-based stand-in for the eventfd-shaped "kick"/"call" signalling
//! `virtio-queue` doorbells need).
//!
//! This crate currently only contains protocol constants/message shapes
//! from the virtio-fs and vhost-user specs, with no working socket layer
//! yet.

pub mod messages;
