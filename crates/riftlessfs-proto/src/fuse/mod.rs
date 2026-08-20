//! FUSE-over-virtio: decoding FUSE requests out of virtqueue descriptor
//! chains and dispatching them to [`riftlessfs_core::PassthroughFs`].

pub mod bytes;
pub mod dispatch;
pub mod wire;
