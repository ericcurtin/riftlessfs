//! Fixed-layout payload structs for the vhost-user requests riftlessfsd
//! needs to handle, with manual little-endian (de)serialization (the wire
//! format is a C ABI struct, so this is simpler and more auditable than
//! pulling in a derive-macro-based serialization crate for a handful of
//! fields).

use crate::error::{ProtoError, ProtoResult};

/// A single `u64` payload: used for `GET_FEATURES`/`SET_FEATURES` replies
/// and requests, `GET_PROTOCOL_FEATURES`/`SET_PROTOCOL_FEATURES`, and the
/// `GET_QUEUE_NUM` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct U64Payload(pub u64);

impl U64Payload {
    pub const LEN: usize = 8;

    pub fn to_bytes(self) -> [u8; Self::LEN] {
        self.0.to_le_bytes()
    }

    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let arr: [u8; Self::LEN] = buf
            .get(..Self::LEN)
            .ok_or(ProtoError::Truncated)?
            .try_into()
            .unwrap();
        Ok(U64Payload(u64::from_le_bytes(arr)))
    }
}

/// `VhostUserVringState`: used for `SET_VRING_NUM`, `SET_VRING_BASE`, the
/// `GET_VRING_BASE` reply, and `SET_VRING_ENABLE` (where `num` is
/// repurposed as a 0/1 enable flag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VringState {
    pub index: u32,
    pub num: u32,
}

impl VringState {
    pub const LEN: usize = 8;

    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut buf = [0u8; Self::LEN];
        buf[0..4].copy_from_slice(&self.index.to_le_bytes());
        buf[4..8].copy_from_slice(&self.num.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        if buf.len() < Self::LEN {
            return Err(ProtoError::Truncated);
        }
        Ok(VringState {
            index: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            num: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        })
    }
}

/// `VhostUserVringAddr`: the guest-visible addresses of a vring's
/// descriptor table, used ring, and available ring (`SET_VRING_ADDR` in
/// the spec). These are addresses *within the mapped guest memory*
/// (translated via the regions from [`MemoryRegions`]), not host
/// pointers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VringAddr {
    pub index: u32,
    pub flags: u32,
    pub descriptor: u64,
    pub used: u64,
    pub avail: u64,
    pub log: u64,
}

impl VringAddr {
    pub const LEN: usize = 40;

    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        if buf.len() < Self::LEN {
            return Err(ProtoError::Truncated);
        }
        let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
        Ok(VringAddr {
            index: u32_at(0),
            flags: u32_at(4),
            descriptor: u64_at(8),
            used: u64_at(16),
            avail: u64_at(24),
            log: u64_at(32),
        })
    }

    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut buf = [0u8; Self::LEN];
        buf[0..4].copy_from_slice(&self.index.to_le_bytes());
        buf[4..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8..16].copy_from_slice(&self.descriptor.to_le_bytes());
        buf[16..24].copy_from_slice(&self.used.to_le_bytes());
        buf[24..32].copy_from_slice(&self.avail.to_le_bytes());
        buf[32..40].copy_from_slice(&self.log.to_le_bytes());
        buf
    }
}

/// Set on the low bits of the `SET_VRING_KICK`/`CALL`/`ERR` payload's
/// index field when no fd was passed along with it (meaning: the vring
/// should be polled rather than fd-notified).
pub const VRING_NOFD_MASK: u64 = 0x100;

/// `SET_VRING_KICK`/`SET_VRING_CALL`/`SET_VRING_ERR` share this payload
/// shape: a vring index (with [`VRING_NOFD_MASK`] optionally set in the
/// same word), paired with an fd passed via the socket's ancillary data
/// (unless the mask bit is set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VringFdPayload {
    pub index: u8,
    pub no_fd: bool,
}

impl VringFdPayload {
    pub const LEN: usize = 8;

    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let u = U64Payload::from_bytes(buf)?.0;
        Ok(VringFdPayload {
            index: (u & 0xff) as u8,
            no_fd: u & VRING_NOFD_MASK != 0,
        })
    }

    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut v = self.index as u64;
        if self.no_fd {
            v |= VRING_NOFD_MASK;
        }
        U64Payload(v).to_bytes()
    }
}

/// A single entry from a `SET_MEM_TABLE` request's region list. The
/// backing fd for the region is passed separately, via the socket's
/// ancillary data, in the same order as the regions appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRegion {
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
    pub mmap_offset: u64,
}

impl MemoryRegion {
    pub const LEN: usize = 32;

    fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        if buf.len() < Self::LEN {
            return Err(ProtoError::Truncated);
        }
        let u64_at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
        Ok(MemoryRegion {
            guest_phys_addr: u64_at(0),
            memory_size: u64_at(8),
            userspace_addr: u64_at(16),
            mmap_offset: u64_at(24),
        })
    }

    fn to_bytes(self) -> [u8; Self::LEN] {
        let mut buf = [0u8; Self::LEN];
        buf[0..8].copy_from_slice(&self.guest_phys_addr.to_le_bytes());
        buf[8..16].copy_from_slice(&self.memory_size.to_le_bytes());
        buf[16..24].copy_from_slice(&self.userspace_addr.to_le_bytes());
        buf[24..32].copy_from_slice(&self.mmap_offset.to_le_bytes());
        buf
    }
}

/// `VhostUserMemory`: the full `SET_MEM_TABLE` payload -- a count followed
/// by that many [`MemoryRegion`] entries (4 bytes of padding after the
/// count, per the spec's C struct layout).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryRegions {
    pub regions: Vec<MemoryRegion>,
}

impl MemoryRegions {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        if buf.len() < 8 {
            return Err(ProtoError::Truncated);
        }
        let count = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let mut regions = Vec::with_capacity(count);
        let mut offset = 8;
        for _ in 0..count {
            let end = offset + MemoryRegion::LEN;
            let region =
                MemoryRegion::from_bytes(buf.get(offset..end).ok_or(ProtoError::Truncated)?)?;
            regions.push(region);
            offset = end;
        }
        Ok(MemoryRegions { regions })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + self.regions.len() * MemoryRegion::LEN);
        buf.extend_from_slice(&(self.regions.len() as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // padding
        for r in &self.regions {
            buf.extend_from_slice(&r.to_bytes());
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u64_payload_roundtrip() {
        let p = U64Payload(0xdead_beef_0000_1234);
        assert_eq!(U64Payload::from_bytes(&p.to_bytes()).unwrap(), p);
    }

    #[test]
    fn vring_state_roundtrip() {
        let s = VringState { index: 3, num: 256 };
        assert_eq!(VringState::from_bytes(&s.to_bytes()).unwrap(), s);
    }

    #[test]
    fn vring_addr_roundtrip() {
        let a = VringAddr {
            index: 1,
            flags: 0,
            descriptor: 0x1000,
            used: 0x2000,
            avail: 0x3000,
            log: 0,
        };
        assert_eq!(VringAddr::from_bytes(&a.to_bytes()).unwrap(), a);
    }

    #[test]
    fn vring_fd_payload_mask() {
        let p = VringFdPayload {
            index: 2,
            no_fd: true,
        };
        let bytes = p.to_bytes();
        assert_eq!(VringFdPayload::from_bytes(&bytes).unwrap(), p);

        let p2 = VringFdPayload {
            index: 5,
            no_fd: false,
        };
        assert_eq!(VringFdPayload::from_bytes(&p2.to_bytes()).unwrap(), p2);
    }

    #[test]
    fn memory_regions_roundtrip() {
        let regions = MemoryRegions {
            regions: vec![
                MemoryRegion {
                    guest_phys_addr: 0,
                    memory_size: 0x1_0000_0000,
                    userspace_addr: 0x7f00_0000_0000,
                    mmap_offset: 0,
                },
                MemoryRegion {
                    guest_phys_addr: 0x1_0000_0000,
                    memory_size: 0x2000,
                    userspace_addr: 0x7f00_0001_0000,
                    mmap_offset: 0x1000,
                },
            ],
        };
        let bytes = regions.to_bytes();
        assert_eq!(MemoryRegions::from_bytes(&bytes).unwrap(), regions);
    }

    #[test]
    fn truncated_payload_is_an_error() {
        assert!(matches!(
            U64Payload::from_bytes(&[1, 2, 3]),
            Err(ProtoError::Truncated)
        ));
        assert!(matches!(
            VringState::from_bytes(&[1, 2, 3]),
            Err(ProtoError::Truncated)
        ));
        assert!(matches!(
            MemoryRegions::from_bytes(&[1, 0, 0, 0, 0, 0, 0, 0]),
            Err(ProtoError::Truncated)
        ));
    }
}
