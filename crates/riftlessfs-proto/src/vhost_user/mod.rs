//! The vhost-user wire protocol: message framing over a UNIX domain
//! socket, with `SCM_RIGHTS` fd passing for shared memory and doorbells,
//! guest memory mapping, and split-virtqueue parsing.
//!
//! - [`header`]/[`payload`]: message (de)serialization.
//! - [`connection`]: the `SCM_RIGHTS`-aware socket transport.
//! - [`memory`]: mapping `SET_MEM_TABLE` regions and translating
//!   addresses (note the guest-physical-vs-user-address distinction
//!   documented there -- getting this wrong doesn't fail loudly).
//! - [`virtqueue`]: split-virtqueue descriptor/avail/used ring parsing.
//! - [`server::Server`]: the event loop tying all of the above together
//!   with [`crate::fuse::dispatch::Session`] into a working vhost-user-fs
//!   backend, verified end-to-end against a real Fedora Linux 44 guest
//!   under QEMU (see the workspace README).

pub mod connection;
pub mod header;
pub mod memory;
pub mod payload;
pub mod server;
pub mod virtqueue;
