//! Split-virtqueue parsing: the descriptor table / available ring / used
//! ring layout from the [virtio 1.x
//! spec](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#x1-240006),
//! read out of [`GuestMemory`].
//!
//! **Known limitations** (fine for a first working version, worth
//! revisiting): indirect descriptors (`VIRTQ_DESC_F_INDIRECT`) aren't
//! supported -- we don't negotiate `VIRTIO_F_INDIRECT_DESC`, so a
//! well-behaved guest shouldn't send them, but we detect and error on them
//! rather than misinterpreting memory if one shows up anyway. We also
//! don't implement the `avail_event`/`used_event` fields
//! (`VIRTIO_F_RING_EVENT_IDX`), meaning we always notify/get notified
//! rather than the more efficient suppressed-notification scheme -- again,
//! fine as long as we don't negotiate that feature bit.

use super::memory::{AddrSpace, GuestMemory};

const DESC_LEN: u64 = 16;
const AVAIL_RING_OFFSET: u64 = 4;
const USED_RING_OFFSET: u64 = 4;
const USED_ELEM_LEN: u64 = 8;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// A maximum chain length we'll walk before giving up and treating it as
/// malformed/malicious -- real FUSE-over-virtio requests use a handful of
/// descriptors at most.
const MAX_CHAIN_LEN: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum VirtqueueError {
    #[error("descriptor chain read past the end of mapped guest memory")]
    OutOfBounds,
    #[error("descriptor chain exceeded the maximum supported length ({MAX_CHAIN_LEN})")]
    ChainTooLong,
    #[error("indirect descriptors are not supported")]
    IndirectUnsupported,
}

type Result<T> = std::result::Result<T, VirtqueueError>;

/// One descriptor chain read off the avail ring: the (possibly
/// multi-descriptor) readable segments the guest wrote a request into,
/// and the writable segments we should write our response into.
#[derive(Debug, Clone, Default)]
pub struct DescChain {
    pub head: u16,
    pub readable: Vec<(u64, u64)>,
    pub writable: Vec<(u64, u64)>,
}

/// State for one split virtqueue: the (guest-physical) addresses of its
/// three rings, its negotiated size, and how far we've consumed the avail
/// ring so far.
pub struct Virtqueue {
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,
    queue_size: u16,
    last_avail_idx: u16,
}

impl Virtqueue {
    pub fn new(queue_size: u16, desc_addr: u64, avail_addr: u64, used_addr: u64) -> Self {
        Virtqueue {
            desc_addr,
            avail_addr,
            used_addr,
            queue_size,
            last_avail_idx: 0,
        }
    }

    pub fn set_avail_base(&mut self, idx: u16) {
        self.last_avail_idx = idx;
    }

    pub fn avail_base(&self) -> u16 {
        self.last_avail_idx
    }

    /// Pop the next available descriptor chain's head index, if the guest
    /// has published one we haven't consumed yet.
    pub fn pop_avail(&mut self, mem: &GuestMemory) -> Result<Option<u16>> {
        // The avail ring's own location is a vring data structure, given
        // to us as a "user address" (see the `memory` module docs).
        let avail_idx = mem
            .read_u16(AddrSpace::User, self.avail_addr + 2)
            .ok_or(VirtqueueError::OutOfBounds)?;
        if avail_idx == self.last_avail_idx {
            return Ok(None);
        }
        let slot = self.last_avail_idx % self.queue_size;
        let ring_offset = AVAIL_RING_OFFSET + slot as u64 * 2;
        let head = mem
            .read_u16(AddrSpace::User, self.avail_addr + ring_offset)
            .ok_or(VirtqueueError::OutOfBounds)?;
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
        Ok(Some(head))
    }

    /// Walk the descriptor chain starting at `head`, splitting it into
    /// readable and writable `(addr, len)` segments in order. The
    /// descriptor *table* is read as a "user address" (a vring data
    /// structure); the buffer addresses stored *inside* each descriptor
    /// are guest-physical addresses set by the guest's own virtio driver
    /// -- see the `memory` module docs for why these differ.
    pub fn read_chain(&self, mem: &GuestMemory, head: u16) -> Result<DescChain> {
        let mut chain = DescChain {
            head,
            ..Default::default()
        };
        let mut idx = head;
        for _ in 0..MAX_CHAIN_LEN {
            let desc_off = self.desc_addr + idx as u64 * DESC_LEN;
            let addr = mem
                .read_u64(AddrSpace::User, desc_off)
                .ok_or(VirtqueueError::OutOfBounds)?;
            let len = mem
                .read_u32(AddrSpace::User, desc_off + 8)
                .ok_or(VirtqueueError::OutOfBounds)?;
            let flags = mem
                .read_u16(AddrSpace::User, desc_off + 12)
                .ok_or(VirtqueueError::OutOfBounds)?;
            let next = mem
                .read_u16(AddrSpace::User, desc_off + 14)
                .ok_or(VirtqueueError::OutOfBounds)?;

            if flags & VIRTQ_DESC_F_INDIRECT != 0 {
                return Err(VirtqueueError::IndirectUnsupported);
            }
            if flags & VIRTQ_DESC_F_WRITE != 0 {
                chain.writable.push((addr, len as u64));
            } else {
                chain.readable.push((addr, len as u64));
            }

            if flags & VIRTQ_DESC_F_NEXT == 0 {
                return Ok(chain);
            }
            idx = next;
        }
        Err(VirtqueueError::ChainTooLong)
    }

    /// Concatenate a chain's readable segments into one buffer. Segment
    /// addresses are guest-physical (see [`read_chain`](Self::read_chain)).
    pub fn gather_readable(&self, mem: &GuestMemory, chain: &DescChain) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        for &(addr, len) in &chain.readable {
            buf.extend_from_slice(
                mem.get_slice(AddrSpace::Gpa, addr, len)
                    .ok_or(VirtqueueError::OutOfBounds)?,
            );
        }
        Ok(buf)
    }

    /// Write `data` across a chain's writable segments in order, returning
    /// the number of bytes actually written (may be less than
    /// `data.len()` if the chain doesn't have enough writable capacity --
    /// callers should size responses to fit what the guest offered).
    /// Segment addresses are guest-physical (see
    /// [`read_chain`](Self::read_chain)).
    pub fn scatter_writable(
        &self,
        mem: &GuestMemory,
        chain: &DescChain,
        data: &[u8],
    ) -> Result<u32> {
        let mut remaining = data;
        let mut written = 0u32;
        for &(addr, len) in &chain.writable {
            if remaining.is_empty() {
                break;
            }
            let n = remaining.len().min(len as usize);
            mem.get_slice_mut(AddrSpace::Gpa, addr, n as u64)
                .ok_or(VirtqueueError::OutOfBounds)?
                .copy_from_slice(&remaining[..n]);
            remaining = &remaining[n..];
            written += n as u32;
        }
        Ok(written)
    }

    /// Publish a completed chain on the used ring and advance its index.
    /// The used ring's own location is a vring data structure (a "user
    /// address"), same as the avail ring and descriptor table.
    pub fn push_used(&mut self, mem: &GuestMemory, head: u16, written_len: u32) -> Result<()> {
        let used_idx = mem
            .read_u16(AddrSpace::User, self.used_addr + 2)
            .ok_or(VirtqueueError::OutOfBounds)?;
        let slot = used_idx % self.queue_size;
        let elem_off = self.used_addr + USED_RING_OFFSET + slot as u64 * USED_ELEM_LEN;
        mem.write_u32(AddrSpace::User, elem_off, head as u32)
            .ok_or(VirtqueueError::OutOfBounds)?;
        mem.write_u32(AddrSpace::User, elem_off + 4, written_len)
            .ok_or(VirtqueueError::OutOfBounds)?;
        mem.write_u16(
            AddrSpace::User,
            self.used_addr + 2,
            used_idx.wrapping_add(1),
        )
        .ok_or(VirtqueueError::OutOfBounds)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay out a minimal queue (1 descriptor table + avail + used ring,
    /// `queue_size` slots) in a fresh block of anonymous "guest memory",
    /// and hand back both the memory and a `Virtqueue` pointing into it.
    fn test_queue(queue_size: u16) -> (GuestMemory, Virtqueue) {
        let gpa = 0x1_0000;
        let desc_addr = gpa;
        let avail_addr = desc_addr + queue_size as u64 * DESC_LEN;
        let used_addr = avail_addr + AVAIL_RING_OFFSET + queue_size as u64 * 2 + 2; // +2 for used_event-less padding safety
                                                                                    // Generous fixed-size region: the rings themselves are tiny, and
                                                                                    // tests also place "data" descriptors well past them.
        let total: u64 = 1 << 20;
        // gpa == user_addr here: these tests focus on descriptor-chain
        // walking, not the gpa/user-address distinction (which
        // `memory::tests` covers directly).
        let mem = GuestMemory::new_anonymous_for_test(gpa, gpa, total as usize).unwrap();
        let vq = Virtqueue::new(queue_size, desc_addr, avail_addr, used_addr);
        (mem, vq)
    }

    fn write_desc(
        mem: &GuestMemory,
        vq: &Virtqueue,
        idx: u16,
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        let off = vq.desc_addr + idx as u64 * DESC_LEN;
        mem.get_slice_mut(AddrSpace::User, off, DESC_LEN).unwrap()[0..8]
            .copy_from_slice(&addr.to_le_bytes());
        mem.write_u32(AddrSpace::User, off + 8, len).unwrap();
        mem.write_u16(AddrSpace::User, off + 12, flags).unwrap();
        mem.write_u16(AddrSpace::User, off + 14, next).unwrap();
    }

    fn publish_avail(mem: &GuestMemory, vq: &Virtqueue, slot: u16, head: u16, new_idx: u16) {
        let ring_off = vq.avail_addr + AVAIL_RING_OFFSET + slot as u64 * 2;
        mem.write_u16(AddrSpace::User, ring_off, head).unwrap();
        mem.write_u16(AddrSpace::User, vq.avail_addr + 2, new_idx)
            .unwrap();
    }

    #[test]
    fn pop_avail_returns_none_when_empty() {
        let (mem, mut vq) = test_queue(8);
        assert!(vq.pop_avail(&mem).unwrap().is_none());
    }

    #[test]
    fn single_request_response_chain_roundtrip() {
        let (mem, mut vq) = test_queue(8);

        // Data area for our two descriptors, placed well past the rings.
        let req_addr = vq.used_addr + 4096;
        let resp_addr = req_addr + 256;

        // Descriptor 0: readable, holds the "request".
        write_desc(&mem, &vq, 0, req_addr, 5, VIRTQ_DESC_F_NEXT, 1);
        // Descriptor 1: writable, where we should write our "response".
        write_desc(&mem, &vq, 1, resp_addr, 64, VIRTQ_DESC_F_WRITE, 0);

        mem.get_slice_mut(AddrSpace::Gpa, req_addr, 5)
            .unwrap()
            .copy_from_slice(b"hello");
        publish_avail(&mem, &vq, 0, 0, 1);

        let head = vq
            .pop_avail(&mem)
            .unwrap()
            .expect("a chain should be available");
        assert_eq!(head, 0);
        assert!(
            vq.pop_avail(&mem).unwrap().is_none(),
            "shouldn't double-pop"
        );

        let chain = vq.read_chain(&mem, head).unwrap();
        assert_eq!(chain.readable, vec![(req_addr, 5)]);
        assert_eq!(chain.writable, vec![(resp_addr, 64)]);

        let request = vq.gather_readable(&mem, &chain).unwrap();
        assert_eq!(request, b"hello");

        let response = b"a fine response";
        let written = vq.scatter_writable(&mem, &chain, response).unwrap();
        assert_eq!(written, response.len() as u32);
        assert_eq!(
            mem.get_slice(AddrSpace::Gpa, resp_addr, response.len() as u64)
                .unwrap(),
            response
        );

        vq.push_used(&mem, head, written).unwrap();
        assert_eq!(mem.read_u16(AddrSpace::User, vq.used_addr + 2), Some(1));
    }

    #[test]
    fn push_used_advances_index_and_records_head_and_len() {
        let (mem, mut vq) = test_queue(4);
        vq.push_used(&mem, 2, 42).unwrap();
        assert_eq!(mem.read_u16(AddrSpace::User, vq.used_addr + 2), Some(1));
        let elem_off = vq.used_addr + USED_RING_OFFSET;
        assert_eq!(mem.read_u32(AddrSpace::User, elem_off), Some(2));
        assert_eq!(mem.read_u32(AddrSpace::User, elem_off + 4), Some(42));

        vq.push_used(&mem, 3, 7).unwrap();
        assert_eq!(mem.read_u16(AddrSpace::User, vq.used_addr + 2), Some(2));
        let elem_off2 = vq.used_addr + USED_RING_OFFSET + USED_ELEM_LEN;
        assert_eq!(mem.read_u32(AddrSpace::User, elem_off2), Some(3));
        assert_eq!(mem.read_u32(AddrSpace::User, elem_off2 + 4), Some(7));
    }

    #[test]
    fn multi_descriptor_readable_chain_is_gathered_in_order() {
        let (mem, mut vq) = test_queue(8);
        let a = vq.used_addr + 4096;
        let b = a + 64;
        let c = b + 64;

        write_desc(&mem, &vq, 0, a, 3, VIRTQ_DESC_F_NEXT, 1);
        write_desc(&mem, &vq, 1, b, 3, VIRTQ_DESC_F_NEXT, 2);
        write_desc(&mem, &vq, 2, c, 3, 0, 0);
        mem.get_slice_mut(AddrSpace::Gpa, a, 3)
            .unwrap()
            .copy_from_slice(b"foo");
        mem.get_slice_mut(AddrSpace::Gpa, b, 3)
            .unwrap()
            .copy_from_slice(b"bar");
        mem.get_slice_mut(AddrSpace::Gpa, c, 3)
            .unwrap()
            .copy_from_slice(b"baz");
        publish_avail(&mem, &vq, 0, 0, 1);

        let head = vq.pop_avail(&mem).unwrap().unwrap();
        let chain = vq.read_chain(&mem, head).unwrap();
        let gathered = vq.gather_readable(&mem, &chain).unwrap();
        assert_eq!(gathered, b"foobarbaz");
    }

    #[test]
    fn indirect_descriptor_is_rejected() {
        let (mem, mut vq) = test_queue(4);
        write_desc(&mem, &vq, 0, 0x1234, 16, VIRTQ_DESC_F_INDIRECT, 0);
        publish_avail(&mem, &vq, 0, 0, 1);
        let head = vq.pop_avail(&mem).unwrap().unwrap();
        assert!(matches!(
            vq.read_chain(&mem, head),
            Err(VirtqueueError::IndirectUnsupported)
        ));
    }
}
