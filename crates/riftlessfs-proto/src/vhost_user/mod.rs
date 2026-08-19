//! The vhost-user wire protocol: message framing over a UNIX domain
//! socket, with `SCM_RIGHTS` fd passing for shared memory and doorbells.
//!
//! **What's implemented:** message header/payload (de)serialization
//! ([`header`], [`payload`]) and the socket transport itself
//! ([`connection`]), covering the requests needed to negotiate features
//! and set up vrings.
//!
//! **What's not implemented yet:** actually mapping the memory regions a
//! front-end hands us via `SET_MEM_TABLE`, parsing the virtqueue
//! descriptor/avail/used rings those addresses point into, decoding the
//! FUSE-over-virtio requests found there, and dispatching them to
//! `riftlessfs-core::PassthroughFs`. That's the remaining bulk of Phase 2
//! -- see the workspace README.

pub mod connection;
pub mod header;
pub mod payload;
