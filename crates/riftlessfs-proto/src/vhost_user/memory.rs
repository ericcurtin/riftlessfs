//! Guest memory: mapping the regions a `SET_MEM_TABLE` request hands us
//! (each backed by an fd received via `SCM_RIGHTS`) into our own address
//! space, and translating the guest-physical addresses that virtqueue
//! descriptors and vring addresses are expressed in into host pointers.

use std::io;
use std::os::fd::RawFd;

use super::payload::MemoryRegion;

struct Region {
    /// Guest physical address this region starts at.
    gpa: u64,
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
            len: region.memory_size,
            ptr,
            mmap_base: base,
            mmap_len: mmap_len as usize,
        })
    }

    /// Build a single-region `GuestMemory` backed by anonymous memory
    /// (not from any fd), for use in tests that need realistic
    /// guest-physical-address translation without a real vhost-user
    /// front-end.
    #[cfg(test)]
    pub fn new_anonymous_for_test(gpa: u64, len: usize) -> io::Result<Self> {
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
                len: len as u64,
                ptr: base as *mut u8,
                mmap_base: base,
                mmap_len: len,
            }],
        })
    }

    fn find(&self, addr: u64, len: u64) -> Option<&Region> {
        self.regions
            .iter()
            .find(|r| addr >= r.gpa && len <= r.len && addr - r.gpa <= r.len - len)
    }

    pub fn get_slice(&self, addr: u64, len: u64) -> Option<&[u8]> {
        let r = self.find(addr, len)?;
        let offset = (addr - r.gpa) as usize;
        Some(unsafe { std::slice::from_raw_parts(r.ptr.add(offset), len as usize) })
    }

    #[allow(clippy::mut_from_ref)] // intentional: shared guest memory, see module docs
    pub fn get_slice_mut(&self, addr: u64, len: u64) -> Option<&mut [u8]> {
        let r = self.find(addr, len)?;
        let offset = (addr - r.gpa) as usize;
        Some(unsafe { std::slice::from_raw_parts_mut(r.ptr.add(offset), len as usize) })
    }

    pub fn read_u16(&self, addr: u64) -> Option<u16> {
        self.get_slice(addr, 2)
            .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
    }

    pub fn read_u32(&self, addr: u64) -> Option<u32> {
        self.get_slice(addr, 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    }

    pub fn read_u64(&self, addr: u64) -> Option<u64> {
        self.get_slice(addr, 8)
            .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    }

    pub fn write_u16(&self, addr: u64, val: u16) -> Option<()> {
        self.get_slice_mut(addr, 2)?
            .copy_from_slice(&val.to_le_bytes());
        Some(())
    }

    pub fn write_u32(&self, addr: u64, val: u32) -> Option<()> {
        self.get_slice_mut(addr, 4)?
            .copy_from_slice(&val.to_le_bytes());
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_region_roundtrip() {
        let mem = GuestMemory::new_anonymous_for_test(0x1000, 4096).unwrap();
        mem.write_u32(0x1000, 0xdead_beef).unwrap();
        assert_eq!(mem.read_u32(0x1000), Some(0xdead_beef));

        mem.write_u16(0x1ffe, 0x1234).unwrap();
        assert_eq!(mem.read_u16(0x1ffe), Some(0x1234));
    }

    #[test]
    fn out_of_range_returns_none() {
        let mem = GuestMemory::new_anonymous_for_test(0x1000, 4096).unwrap();
        assert_eq!(mem.read_u32(0x500), None); // before region
        assert_eq!(mem.read_u32(0x2000), None); // after region
        assert_eq!(mem.read_u64(0x1ffc), None); // straddles end
    }

    #[test]
    fn slice_access() {
        let mem = GuestMemory::new_anonymous_for_test(0x1000, 4096).unwrap();
        let data = b"hello, virtio";
        mem.get_slice_mut(0x1000, data.len() as u64)
            .unwrap()
            .copy_from_slice(data);
        assert_eq!(mem.get_slice(0x1000, data.len() as u64).unwrap(), data);
    }
}
