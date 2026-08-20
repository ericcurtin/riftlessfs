//! Guest memory: mapping the regions a `SET_MEM_TABLE` request hands us
//! (each backed by an fd received via `SCM_RIGHTS`) into our own address
//! space, and translating addresses into host pointers.
//!
//! There are **two distinct address spaces** in play here, and mixing them
//! up doesn't fail loudly -- it just reads/writes the wrong bytes (this was
//! confirmed the hard way against a real QEMU front-end: everything
//! worked in-process against synthetic tests that happened to use `0` for
//! both spaces, then broke against QEMU, which does not):
//!
//! - **Guest-physical addresses** (`AddrSpace::Gpa`): what the *guest
//!   kernel's virtio driver* itself writes into descriptor table entries
//!   (`virtq_desc.addr`) to say where a request/response buffer lives.
//!   The driver has no notion of vhost-user at all, so these are always
//!   real guest-physical addresses.
//! - **User addresses** (`AddrSpace::User`): what the *front-end*
//!   (QEMU) reports via `SET_VRING_ADDR` for the vring's own data
//!   structures (descriptor table, avail ring, used ring). Per the
//!   vhost-user spec, ring addresses are guest-physical only if
//!   `VHOST_USER_PROTOCOL_F_GPA_ADDRESSES` was negotiated; otherwise
//!   (our case -- we don't negotiate it, to keep things simple) they're
//!   given in terms of the memory region's `user address` field, i.e.
//!   QEMU's own idea of where the region lives, not the guest's.
//!
//! Both address spaces map onto the same underlying mapped regions (a
//! region has both a `guest_phys_addr` and a `user address`); which one a
//! given number should be looked up against just depends on where that
//! number came from.

use std::io;
use std::os::fd::RawFd;

use super::payload::MemoryRegion;

/// Which address space a given address should be resolved in -- see the
/// module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrSpace {
    /// A guest-physical address, as written into descriptor table entries
    /// by the guest's virtio driver.
    Gpa,
    /// A "user address", as reported by the front-end for a vring's own
    /// data structures via `SET_VRING_ADDR`.
    User,
}

struct Region {
    /// Guest physical address this region starts at.
    gpa: u64,
    /// The front-end's own "user address" for this region.
    user_addr: u64,
    /// Length of the *usable* mapping (`memory_size` from the region
    /// descriptor).
    len: u64,
    /// Host pointer to the start of the usable mapping (i.e. already
    /// offset past `mmap_offset`).
    ptr: *mut u8,
    /// The base pointer/length actually passed to `mmap`/`munmap` (may
    /// differ from `ptr`/`len` because of `mmap_offset`).
    mmap_base: *mut libc::c_void,
    mmap_len: usize,
}

impl Region {
    fn base_for(&self, space: AddrSpace) -> u64 {
        match space {
            AddrSpace::Gpa => self.gpa,
            AddrSpace::User => self.user_addr,
        }
    }
}

// SAFETY: `Region`/`GuestMemory` only ever hand out `&[u8]`/`&mut [u8]`
// slices scoped to a `&self`/`&mut self` borrow of `GuestMemory`, and the
// underlying mapping is `MAP_SHARED` memory that outlives the struct until
// `Drop`, so sharing the raw pointer across threads is fine as long as
// callers don't rely on any particular cross-thread memory-ordering
// guarantees for the *contents* (the guest can write concurrently; see
// the module docs' note on volatility).
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Drop for Region {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.mmap_base, self.mmap_len);
        }
    }
}

/// Guest memory, as described by one or more regions from a `SET_MEM_TABLE`
/// request, mapped into our address space.
///
/// **Volatility note:** reads/writes here use plain (non-atomic,
/// non-`volatile`) slice access. The guest *shouldn't* be concurrently
/// modifying the specific bytes we're reading at any given moment for the
/// fields this crate touches (we only read a descriptor/ring slot after
/// observing, via the avail ring index, that the guest has published it,
/// which on real hardware needs a memory barrier we don't yet emit -- see
/// `Virtqueue` docs). This is a known correctness gap to revisit before
/// relying on this for anything beyond development/testing.
pub struct GuestMemory {
    regions: Vec<Region>,
}

impl GuestMemory {
    /// Map each `regions[i]` using the corresponding `fds[i]` (`SET_MEM_TABLE`
    /// pairs them up positionally: one fd per region, via `SCM_RIGHTS`, in
    /// the same order the regions appear in the payload).
    pub fn map_regions(regions: &[MemoryRegion], fds: &[RawFd]) -> io::Result<Self> {
        if regions.len() != fds.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "SET_MEM_TABLE had {} region(s) but {} fd(s)",
                    regions.len(),
                    fds.len()
                ),
            ));
        }

        let mut mapped = Vec::with_capacity(regions.len());
        for (region, &fd) in regions.iter().zip(fds) {
            mapped.push(Self::map_one(region, fd)?);
        }
        Ok(GuestMemory { regions: mapped })
    }

    fn map_one(region: &MemoryRegion, fd: RawFd) -> io::Result<Region> {
        // mmap_offset must be page-aligned for mmap(); the reference
        // implementations always arrange this, but guard against a
        // misbehaving front-end anyway.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        let aligned_offset = region.mmap_offset - (region.mmap_offset % page_size);
        let extra = region.mmap_offset - aligned_offset;
        let mmap_len = region.memory_size + extra;

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mmap_len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                aligned_offset as libc::off_t,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        let ptr = unsafe { (base as *mut u8).add(extra as usize) };
        Ok(Region {
            gpa: region.guest_phys_addr,
            user_addr: region.userspace_addr,
            len: region.memory_size,
            ptr,
            mmap_base: base,
            mmap_len: mmap_len as usize,
        })
    }

    /// Build a single-region `GuestMemory` backed by anonymous memory
    /// (not from any fd), for use in tests that need realistic address
    /// translation without a real vhost-user front-end. `gpa` and
    /// `user_addr` can be given different values to exercise the
    /// dual-address-space behavior described in the module docs; most
    /// tests that don't care about that distinction can just pass the
    /// same value for both.
    #[cfg(test)]
    pub fn new_anonymous_for_test(gpa: u64, user_addr: u64, len: usize) -> io::Result<Self> {
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(GuestMemory {
            regions: vec![Region {
                gpa,
                user_addr,
                len: len as u64,
                ptr: base as *mut u8,
                mmap_base: base,
                mmap_len: len,
            }],
        })
    }

    fn find(&self, space: AddrSpace, addr: u64, len: u64) -> Option<&Region> {
        self.regions.iter().find(|r| {
            let base = r.base_for(space);
            addr >= base && len <= r.len && addr - base <= r.len - len
        })
    }

    pub fn get_slice(&self, space: AddrSpace, addr: u64, len: u64) -> Option<&[u8]> {
        let r = self.find(space, addr, len)?;
        let offset = (addr - r.base_for(space)) as usize;
        Some(unsafe { std::slice::from_raw_parts(r.ptr.add(offset), len as usize) })
    }

    #[allow(clippy::mut_from_ref)] // intentional: shared guest memory, see module docs
    pub fn get_slice_mut(&self, space: AddrSpace, addr: u64, len: u64) -> Option<&mut [u8]> {
        let r = self.find(space, addr, len)?;
        let offset = (addr - r.base_for(space)) as usize;
        Some(unsafe { std::slice::from_raw_parts_mut(r.ptr.add(offset), len as usize) })
    }

    pub fn read_u16(&self, space: AddrSpace, addr: u64) -> Option<u16> {
        self.get_slice(space, addr, 2)
            .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
    }

    pub fn read_u32(&self, space: AddrSpace, addr: u64) -> Option<u32> {
        self.get_slice(space, addr, 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    }

    pub fn read_u64(&self, space: AddrSpace, addr: u64) -> Option<u64> {
        self.get_slice(space, addr, 8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    }

    pub fn write_u16(&self, space: AddrSpace, addr: u64, val: u16) -> Option<()> {
        self.get_slice_mut(space, addr, 2)?
            .copy_from_slice(&val.to_le_bytes());
        Some(())
    }

    pub fn write_u32(&self, space: AddrSpace, addr: u64, val: u32) -> Option<()> {
        self.get_slice_mut(space, addr, 4)?
            .copy_from_slice(&val.to_le_bytes());
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_region_roundtrip() {
        let mem = GuestMemory::new_anonymous_for_test(0x1000, 0x1000, 4096).unwrap();
        mem.write_u32(AddrSpace::Gpa, 0x1000, 0xdead_beef).unwrap();
        assert_eq!(mem.read_u32(AddrSpace::Gpa, 0x1000), Some(0xdead_beef));

        mem.write_u16(AddrSpace::Gpa, 0x1ffe, 0x1234).unwrap();
        assert_eq!(mem.read_u16(AddrSpace::Gpa, 0x1ffe), Some(0x1234));
    }

    #[test]
    fn out_of_range_returns_none() {
        let mem = GuestMemory::new_anonymous_for_test(0x1000, 0x1000, 4096).unwrap();
        assert_eq!(mem.read_u32(AddrSpace::Gpa, 0x500), None); // before region
        assert_eq!(mem.read_u32(AddrSpace::Gpa, 0x2000), None); // after region
        assert_eq!(mem.read_u64(AddrSpace::Gpa, 0x1ffc), None); // straddles end
    }

    #[test]
    fn slice_access() {
        let mem = GuestMemory::new_anonymous_for_test(0x1000, 0x1000, 4096).unwrap();
        let data = b"hello, virtio";
        mem.get_slice_mut(AddrSpace::Gpa, 0x1000, data.len() as u64)
            .unwrap()
            .copy_from_slice(data);
        assert_eq!(
            mem.get_slice(AddrSpace::Gpa, 0x1000, data.len() as u64)
                .unwrap(),
            data
        );
    }

    /// The bug that only showed up against a real front-end: gpa and user
    /// address are genuinely different numbers, and using the wrong space
    /// must not silently "work" by accident.
    #[test]
    fn gpa_and_user_address_spaces_are_independent() {
        let mem = GuestMemory::new_anonymous_for_test(0x4000_0000, 0x7f_0000_0000, 4096).unwrap();

        mem.write_u32(AddrSpace::Gpa, 0x4000_0000, 111).unwrap();
        assert_eq!(mem.read_u32(AddrSpace::Gpa, 0x4000_0000), Some(111));
        // The same byte, read back via the *other* space's coordinate,
        // should not resolve (wrong base address for that space).
        assert_eq!(mem.read_u32(AddrSpace::User, 0x4000_0000), None);

        mem.write_u32(AddrSpace::User, 0x7f_0000_0000, 222).unwrap();
        assert_eq!(mem.read_u32(AddrSpace::User, 0x7f_0000_0000), Some(222));
        assert_eq!(mem.read_u32(AddrSpace::Gpa, 0x7f_0000_0000), None);
    }
}
