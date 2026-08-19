//! The 12-byte header that precedes every vhost-user message, and the
//! front-end request codes from the [vhost-user protocol
//! spec](https://qemu.readthedocs.io/en/master/interop/vhost-user.html).
//!
//! Only the subset of requests riftlessfsd needs to actually speak
//! virtio-fs is enumerated here (no migration/logging/postcopy support).

use crate::error::{ProtoError, ProtoResult};

pub const HEADER_LEN: usize = 12;

/// Bit 0 of the header's `flags` field: must always be set (protocol
/// version 1 -- the only version that exists).
pub const FLAG_VERSION_1: u32 = 0x1;
/// Set by the *reply* to a request.
pub const FLAG_REPLY: u32 = 0x4;
/// Set by the *requester* to ask for a reply even to messages that
/// normally don't get one (used with `VHOST_USER_PROTOCOL_F_REPLY_ACK`).
pub const FLAG_NEED_REPLY: u32 = 0x8;

/// Front-end -> back-end request codes we understand. Anything else is
/// surfaced as [`ProtoError::UnknownRequest`] rather than panicking, since
/// an unrecognized-but-harmless request from a newer front-end shouldn't
/// take the daemon down.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
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

impl TryFrom<u32> for Request {
    type Error = ProtoError;

    fn try_from(v: u32) -> ProtoResult<Self> {
        Ok(match v {
            1 => Request::GetFeatures,
            2 => Request::SetFeatures,
            3 => Request::SetOwner,
            4 => Request::ResetOwner,
            5 => Request::SetMemTable,
            6 => Request::SetLogBase,
            7 => Request::SetLogFd,
            8 => Request::SetVringNum,
            9 => Request::SetVringAddr,
            10 => Request::SetVringBase,
            11 => Request::GetVringBase,
            12 => Request::SetVringKick,
            13 => Request::SetVringCall,
            14 => Request::SetVringErr,
            15 => Request::GetProtocolFeatures,
            16 => Request::SetProtocolFeatures,
            17 => Request::GetQueueNum,
            18 => Request::SetVringEnable,
            other => return Err(ProtoError::UnknownRequest(other)),
        })
    }
}

/// The 12-byte header every vhost-user message starts with: a raw
/// (not-yet-validated) request code, flags, and the length of the payload
/// that follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgHeader {
    pub request: u32,
    pub flags: u32,
    pub size: u32,
}

impl MsgHeader {
    pub fn new(request: Request, flags: u32, size: u32) -> Self {
        MsgHeader {
            request: request as u32,
            flags: flags | FLAG_VERSION_1,
            size,
        }
    }

    pub fn reply(request: Request, size: u32) -> Self {
        Self::new(request, FLAG_REPLY, size)
    }

    pub fn request(&self) -> ProtoResult<Request> {
        Request::try_from(self.request)
    }

    pub fn is_reply(&self) -> bool {
        self.flags & FLAG_REPLY != 0
    }

    pub fn needs_reply(&self) -> bool {
        self.flags & FLAG_NEED_REPLY != 0
    }

    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(&self.request.to_le_bytes());
        buf[4..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8..12].copy_from_slice(&self.size.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; HEADER_LEN]) -> Self {
        MsgHeader {
            request: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            size: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = MsgHeader::new(Request::GetFeatures, 0, 8);
        let bytes = h.to_bytes();
        let h2 = MsgHeader::from_bytes(&bytes);
        assert_eq!(h, h2);
        assert_eq!(h2.request().unwrap(), Request::GetFeatures);
        assert!(!h2.is_reply());
    }

    #[test]
    fn reply_sets_flag() {
        let h = MsgHeader::reply(Request::GetFeatures, 8);
        assert!(h.is_reply());
        assert_eq!(h.flags & FLAG_VERSION_1, FLAG_VERSION_1);
    }

    #[test]
    fn unknown_request_is_an_error_not_a_panic() {
        let h = MsgHeader::from_bytes(&MsgHeader::new(Request::GetFeatures, 0, 0).to_bytes());
        let mut raw = h;
        raw.request = 9999;
        assert!(matches!(
            raw.request(),
            Err(ProtoError::UnknownRequest(9999))
        ));
    }
}
