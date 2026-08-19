//! Message type constants from the [vhost-user protocol
//! spec](https://qemu.readthedocs.io/en/master/interop/vhost-user.html).
//! Values only; no (de)serialization or socket handling yet (see the
//! module-level docs in `lib.rs`).

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontEndRequest {
    GetFeatures = 1,
    SetFeatures = 2,
    SetOwner = 3,
    ResetOwner = 4,
    SetMemTable = 5,
    SetLogBase = 6,
    SetLogFd = 7,
    SetVringNum = 8,
    SetVringAddr = 9,
    SetVringBase = 10,
    GetVringBase = 11,
    SetVringKick = 12,
    SetVringCall = 13,
    SetVringErr = 14,
    GetProtocolFeatures = 15,
    SetProtocolFeatures = 16,
    GetQueueNum = 17,
    SetVringEnable = 18,
}
