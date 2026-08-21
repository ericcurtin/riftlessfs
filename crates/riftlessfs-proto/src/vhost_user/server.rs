//! Ties the pieces together: accept a vhost-user connection, negotiate
//! features, handle `SET_MEM_TABLE`/`SET_VRING_*` to build up
//! [`GuestMemory`] and per-vring [`Virtqueue`] state, and then run a
//! blocking event loop that polls the control socket and every enabled
//! vring's kick fd, dispatching FUSE requests found there to a
//! [`crate::fuse::dispatch::Session`] and notifying the guest via the
//! matching call fd.
//!
//! This is intentionally simple (single-threaded, one connection, no
//! event-index/notification-suppression optimization, no live migration
//! support) -- it's meant to be a correct first version to validate
//! against a real guest kernel, not a tuned one. See the workspace README
//! for performance follow-up work once correctness is established.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::error::{ProtoError, ProtoResult};
use crate::fuse::dispatch::{Reply, Session};
use crate::fuse::wire::Opcode;

use super::connection::Connection;
use super::header::{MsgHeader, Request};
use super::memory::GuestMemory;
use super::payload::{MemoryRegions, U64Payload, VringAddr, VringFdPayload, VringState};
use super::virtqueue::Virtqueue;

/// We advertise no optional vhost-user protocol features (no
/// `REPLY_ACK`, no `MQ`, no `CONFIG`, ...): none of them are needed for a
/// correct first version, and each one is more surface area to get wrong.
const PROTOCOL_FEATURES: u64 = 0;

/// Bit 30 (`VHOST_USER_F_PROTOCOL_FEATURES`) tells the front-end we
/// understand `GET/SET_PROTOCOL_FEATURES` at all. Bit 32
/// (`VIRTIO_F_VERSION_1`) is required for "modern" (non-transitional)
/// virtio devices -- without it, front-ends that only support modern mode
/// for vhost-user-fs (e.g. QEMU) refuse to attach at all ("Device doesn't
/// support modern mode, and legacy mode is disabled"), which is how this
/// omission was originally caught.
const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_FEATURES: u64 = (1 << 30) | VIRTIO_F_VERSION_1;

/// Total vrings: 0 = hiprio, 1 = the (single) request queue. Both are
/// processed identically by this implementation -- see module docs.
const NUM_QUEUES: usize = 2;

#[derive(Default)]
struct VringSlot {
    queue_size: Option<u16>,
    pending_avail_base: u16,
    vq: Option<Virtqueue>,
    kick_fd: Option<OwnedFd>,
    call_fd: Option<OwnedFd>,
    enabled: bool,
}

impl VringSlot {
    fn maybe_build(&mut self, addr: &VringAddr) {
        if let Some(qs) = self.queue_size {
            let mut vq = Virtqueue::new(qs, addr.descriptor, addr.avail, addr.used);
            vq.set_avail_base(self.pending_avail_base);
            self.vq = Some(vq);
        } else {
            log::warn!(
                "SET_VRING_ADDR for index with no prior SET_VRING_NUM; ignoring until one arrives"
            );
        }
    }
}

pub struct Server {
    conn: Connection,
    mem: Option<GuestMemory>,
    vrings: Vec<VringSlot>,
    session: Session,
}

impl Server {
    pub fn new(conn: Connection, session: Session) -> Self {
        let mut vrings = Vec::with_capacity(NUM_QUEUES);
        vrings.resize_with(NUM_QUEUES, VringSlot::default);
        Server {
            conn,
            mem: None,
            vrings,
            session,
        }
    }

    /// Run the blocking control-message + virtqueue-processing loop until
    /// the connection is closed.
    ///
    /// A bounded busy-poll before blocking in `poll()` was tried and
    /// measured here (not just considered): profiling (see
    /// [`Self::process_vring`]'s trace logging) showed our own
    /// per-request processing takes ~2us, so essentially all of a
    /// request's ~120us round-trip latency (measured against a real
    /// guest -- see BENCHMARKS.md) is spent waiting to be woken up, not
    /// doing work, suggesting a busy-poll (avoiding this process's own
    /// scheduler wake-up cost) might help. Measured result: no
    /// significant change to random-I/O throughput/latency. That's a
    /// useful negative result, not a wasted one -- it means the
    /// remaining latency is dominated by something further down the
    /// chain this process doesn't control (QEMU's own event loop
    /// wake-up, the guest kernel's own task scheduling, HVF/KVM
    /// interrupt-injection cost, ...), not by riftlessfsd's own wait
    /// here. Documented in BENCHMARKS.md rather than left as a silent
    /// revert; not worth the CPU cost of keeping given no measured
    /// benefit in the one environment this could be tested in so far.
    pub fn run(mut self) -> ProtoResult<()> {
        loop {
            let mut fds = vec![libc::pollfd {
                fd: self.conn.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            let mut kick_slots = Vec::new();
            for (i, slot) in self.vrings.iter().enumerate() {
                if slot.enabled {
                    if let Some(kfd) = &slot.kick_fd {
                        fds.push(libc::pollfd {
                            fd: kfd.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        });
                        kick_slots.push(i);
                    }
                }
            }

            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.into());
            }

            if fds[0].revents & libc::POLLIN != 0 {
                match self.handle_one_message() {
                    Ok(()) => {}
                    Err(ProtoError::Disconnected) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
            if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                return Ok(());
            }

            for (slot_idx, pfd) in kick_slots.into_iter().zip(fds.into_iter().skip(1)) {
                if pfd.revents & libc::POLLIN != 0 {
                    drain_fd(pfd.fd);
                    self.process_vring(slot_idx)?;
                }
            }
        }
    }

    fn process_vring(&mut self, idx: usize) -> ProtoResult<()> {
        let Some(mem) = &self.mem else { return Ok(()) };
        let slot = &mut self.vrings[idx];
        let Some(vq) = &mut slot.vq else {
            return Ok(());
        };

        let mut processed = 0;
        while let Some(head) = vq.pop_avail(mem)? {
            log::debug!("vring {idx}: popped avail head {head}");
            let chain = vq.read_chain(mem, head)?;
            log::debug!(
                "vring {idx}: chain readable={:?} writable={:?}",
                chain.readable,
                chain.writable
            );
            // Cheap when RUST_LOG doesn't enable trace (the format! args
            // aren't even evaluated), but lets us answer "how much of the
            // per-request latency is our own processing, vs. everything
            // outside our control (VM exit/entry, scheduler wake-up,
            // etc.)?" with real numbers instead of guessing -- see
            // BENCHMARKS.md's random-I/O discussion.
            let t0 = std::time::Instant::now();
            let request = vq.gather_readable(mem, &chain)?;
            // `fuse_in_header.opcode` is the second u32 (bytes 4..8),
            // right after `len` -- peeked directly rather than doing a
            // full `InHeader::from_bytes` parse here, purely so this
            // trace line can distinguish opcodes without duplicating
            // dispatch's own parsing. Used to compare our own
            // (non-syscall) per-request overhead between opcodes at
            // matching sizes -- e.g. WRITE vs READ -- as part of the
            // random-write investigation in BENCHMARKS.md: per-request
            // `pwrite`/`pread` timing already isolates the syscall
            // itself (see `fuse::dispatch::Session::handle`), so this is
            // what's needed to see whether the *rest* of our own
            // request handling differs between the two.
            let opcode = request
                .get(4..8)
                .map(|b| Opcode::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
                .unwrap_or(Opcode::Unknown(0));
            let reply = self.session.handle(&request);
            let written = match reply {
                Reply::Bytes(bytes) => vq.scatter_writable(mem, &chain, &bytes)?,
                Reply::None => 0,
            };
            vq.push_used(mem, head, written)?;
            log::trace!(
                "vring {idx}: {opcode:?} request processed in {:?}",
                t0.elapsed()
            );
            processed += 1;
        }

        if processed > 0 {
            // How many requests were available to drain in one wake-up,
            // i.e. how well the guest's own submission pattern batches
            // *before* kicking us -- each separate wake-up pays the
            // external (VM exit/entry, scheduler) latency documented
            // above regardless of how many requests it contains, so a
            // workload issuing many small batches pays that cost far
            // more often than one issuing few large batches. Relevant to
            // the random-write investigation in BENCHMARKS.md: it's a
            // candidate explanation for why random write (presumably
            // small, frequent writeback batches) fares worse than
            // sequential write (presumably fewer, larger ones) even
            // though single-request round-trip latency alone doesn't
            // differ between reads and writes.
            log::trace!("vring {idx}: drained batch of {processed} request(s)");
            if let Some(call_fd) = &slot.call_fd {
                notify(call_fd.as_raw_fd());
            }
        }
        Ok(())
    }

    fn handle_one_message(&mut self) -> ProtoResult<()> {
        let (header, payload, mut fds) = self.conn.recv()?;
        let Ok(request) = header.request() else {
            log::warn!(
                "ignoring unknown vhost-user request code {}",
                header.request
            );
            return Ok(());
        };

        match request {
            Request::GetFeatures => {
                self.reply(request, &U64Payload(VIRTIO_FEATURES).to_bytes())?;
            }
            Request::SetFeatures => {
                let _features = U64Payload::from_bytes(&payload)?.0;
            }
            Request::SetOwner | Request::ResetOwner => {}
            Request::GetProtocolFeatures => {
                self.reply(request, &U64Payload(PROTOCOL_FEATURES).to_bytes())?;
            }
            Request::SetProtocolFeatures => {}
            Request::GetQueueNum => {
                self.reply(request, &U64Payload(NUM_QUEUES as u64).to_bytes())?;
            }

            Request::SetMemTable => {
                let regions = MemoryRegions::from_bytes(&payload)?;
                if regions.regions.len() != fds.len() {
                    return Err(ProtoError::Truncated);
                }
                for r in &regions.regions {
                    log::debug!(
                        "SET_MEM_TABLE region: gpa=0x{:x} size=0x{:x} userspace_addr=0x{:x} mmap_offset=0x{:x}",
                        r.guest_phys_addr,
                        r.memory_size,
                        r.userspace_addr,
                        r.mmap_offset
                    );
                }
                let mem = GuestMemory::map_regions(&regions.regions, &fds)?;
                fds.clear(); // ownership transferred into GuestMemory's mmaps
                self.mem = Some(mem);
            }

            Request::SetVringNum => {
                let s = VringState::from_bytes(&payload)?;
                if let Some(slot) = self.vrings.get_mut(s.index as usize) {
                    slot.queue_size = Some(s.num as u16);
                }
            }
            Request::SetVringAddr => {
                let a = VringAddr::from_bytes(&payload)?;
                log::debug!(
                    "SET_VRING_ADDR index={} desc=0x{:x} avail=0x{:x} used=0x{:x}",
                    a.index,
                    a.descriptor,
                    a.avail,
                    a.used
                );
                if let Some(slot) = self.vrings.get_mut(a.index as usize) {
                    slot.maybe_build(&a);
                }
            }
            Request::SetVringBase => {
                let s = VringState::from_bytes(&payload)?;
                if let Some(slot) = self.vrings.get_mut(s.index as usize) {
                    slot.pending_avail_base = s.num as u16;
                    if let Some(vq) = &mut slot.vq {
                        vq.set_avail_base(s.num as u16);
                    }
                }
            }
            Request::GetVringBase => {
                let s = VringState::from_bytes(&payload)?;
                let base = self
                    .vrings
                    .get(s.index as usize)
                    .and_then(|slot| slot.vq.as_ref())
                    .map(|vq| vq.avail_base())
                    .unwrap_or(0);
                self.reply(
                    request,
                    &VringState {
                        index: s.index,
                        num: base as u32,
                    }
                    .to_bytes(),
                )?;
            }
            Request::SetVringKick => {
                let p = VringFdPayload::from_bytes(&payload)?;
                if let Some(slot) = self.vrings.get_mut(p.index as usize) {
                    slot.kick_fd = take_fd(&mut fds, p.no_fd);
                }
            }
            Request::SetVringCall => {
                let p = VringFdPayload::from_bytes(&payload)?;
                if let Some(slot) = self.vrings.get_mut(p.index as usize) {
                    slot.call_fd = take_fd(&mut fds, p.no_fd);
                }
            }
            Request::SetVringErr => {
                let p = VringFdPayload::from_bytes(&payload)?;
                let _ = take_fd(&mut fds, p.no_fd); // received, intentionally unused
            }
            Request::SetVringEnable => {
                let s = VringState::from_bytes(&payload)?;
                if let Some(slot) = self.vrings.get_mut(s.index as usize) {
                    slot.enabled = s.num != 0;
                }
            }

            Request::SetLogBase | Request::SetLogFd => {}
        }

        Ok(())
    }

    fn reply(&self, request: Request, body: &[u8]) -> ProtoResult<()> {
        self.conn
            .send(MsgHeader::reply(request, body.len() as u32), body, &[])
    }
}

fn take_fd(fds: &mut Vec<RawFd>, no_fd: bool) -> Option<OwnedFd> {
    if no_fd || fds.is_empty() {
        return None;
    }
    let fd = fds.remove(0);
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Drain whatever's pending on a kick fd without assuming anything about
/// its exact type (real Linux `eventfd`, a pipe, ...) -- any pollable fd
/// that supports `read()` works here.
fn drain_fd(fd: RawFd) {
    let mut buf = [0u8; 256];
    loop {
        let rc = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if rc <= 0 {
            break;
        }
        if (rc as usize) < buf.len() {
            break;
        }
    }
}

/// Notify the guest via a call fd. Same "any pollable fd" caveat as
/// [`drain_fd`]: a single byte is enough to wake a poller regardless of
/// whether the fd is a real eventfd (which coalesces counts) or a pipe.
fn notify(fd: RawFd) {
    let one: u64 = 1;
    unsafe {
        libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
    }
}

#[cfg(test)]
mod tests {
    use super::super::memory::AddrSpace;
    use super::*;
    use crate::fuse::bytes::Writer;
    use crate::fuse::wire;
    use crate::vhost_user::payload::MemoryRegion;
    use riftlessfs_core::PassthroughFs;
    use std::os::unix::net::UnixStream;

    fn make_pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn init_request(unique: u64) -> Vec<u8> {
        let mut body = Writer::new();
        body.u32(7).u32(45).u32(0).u32(0);
        let body = body.into_vec();

        let mut w = Writer::new();
        w.u32((wire::IN_HEADER_LEN + body.len()) as u32);
        w.u32(26); // FUSE_INIT
        w.u64(unique);
        w.u64(0); // nodeid
        w.u32(0).u32(0).u32(0); // uid, gid, pid
        w.u32(0); // total_extlen + padding
        w.bytes(&body);
        w.into_vec()
    }

    /// Full loop test: drive a `Server` through the real vhost-user
    /// handshake over an actual `UnixStream::pair()`, set up a real
    /// shared-memory-backed virtqueue (an anonymous tempfile, mapped
    /// independently by "our" side and the simulated front-end, exactly
    /// like a real VMM and backend would each map the same fd), place a
    /// FUSE INIT request in it, kick, and confirm we get a correctly
    /// negotiated INIT reply plus a call-fd notification -- all without
    /// a real VM or kernel.
    #[test]
    fn full_handshake_and_one_request_over_a_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path()).unwrap();
        let session = Session::new(fs);

        let (fe, be) = UnixStream::pair().unwrap();
        let server = Server::new(Connection::from_stream(be), session);
        let handle = std::thread::spawn(move || server.run());

        let fe = Connection::from_stream(fe);

        // --- feature negotiation ---
        fe.send(MsgHeader::new(Request::GetFeatures, 0, 0), &[], &[])
            .unwrap();
        let (h, p, _) = fe.recv().unwrap();
        assert!(h.is_reply());
        assert_ne!(U64Payload::from_bytes(&p).unwrap().0 & VIRTIO_FEATURES, 0);

        fe.send(
            MsgHeader::new(Request::SetFeatures, 0, 8),
            &U64Payload(VIRTIO_FEATURES).to_bytes(),
            &[],
        )
        .unwrap();

        fe.send(MsgHeader::new(Request::GetProtocolFeatures, 0, 0), &[], &[])
            .unwrap();
        let (_h, _p, _) = fe.recv().unwrap();

        fe.send(
            MsgHeader::new(Request::SetProtocolFeatures, 0, 8),
            &U64Payload(0).to_bytes(),
            &[],
        )
        .unwrap();
        fe.send(MsgHeader::new(Request::SetOwner, 0, 0), &[], &[])
            .unwrap();

        // --- shared memory: one 1 MiB anonymous-file-backed region ---
        //
        // Deliberately use *different* values for the guest-physical base
        // and the "user address" base (unlike a naive test that picks 0
        // for both): mixing up the two address spaces (see the `memory`
        // module docs) doesn't fail loudly when they happen to coincide,
        // which is exactly how this bug first slipped past this test and
        // was only caught against a real QEMU front-end.
        let mem_file = tempfile::tempfile().unwrap();
        let mem_len: u64 = 1 << 20;
        unsafe { libc::ftruncate(mem_file.as_raw_fd(), mem_len as libc::off_t) };
        const GPA_BASE: u64 = 0x4000_0000;
        const USER_BASE: u64 = 0x7f_0000_0000;
        let region = MemoryRegion {
            guest_phys_addr: GPA_BASE,
            memory_size: mem_len,
            userspace_addr: USER_BASE,
            mmap_offset: 0,
        };
        let regions = MemoryRegions {
            regions: vec![region],
        };
        fe.send(
            MsgHeader::new(Request::SetMemTable, 0, regions.to_bytes().len() as u32),
            &regions.to_bytes(),
            &[mem_file.as_raw_fd()],
        )
        .unwrap();

        // Our own independent mapping of the same file, standing in for
        // what a real guest kernel would do with its half of the shared
        // memory.
        let guest_mem = GuestMemory::map_regions(&[region], &[mem_file.as_raw_fd()]).unwrap();

        // --- one virtqueue (index 1: the "request" queue) ---
        // Ring locations are "user addresses" (offsets from USER_BASE);
        // request/response buffer locations (like a guest driver would
        // set them) are guest-physical (offsets from GPA_BASE). Both
        // happen to cover the same underlying bytes in this single-region
        // setup, just addressed via different bases.
        const QUEUE_SIZE: u16 = 8;
        let desc_addr = USER_BASE + 0x1000;
        let avail_addr = desc_addr + QUEUE_SIZE as u64 * 16;
        let used_addr = avail_addr + 4 + QUEUE_SIZE as u64 * 2 + 64;
        let req_addr = GPA_BASE + 0x2000;
        let resp_addr = req_addr + 256;

        fe.send(
            MsgHeader::new(Request::SetVringNum, 0, 8),
            &VringState {
                index: 1,
                num: QUEUE_SIZE as u32,
            }
            .to_bytes(),
            &[],
        )
        .unwrap();
        fe.send(
            MsgHeader::new(Request::SetVringAddr, 0, VringAddr::LEN as u32),
            &VringAddr {
                index: 1,
                flags: 0,
                descriptor: desc_addr,
                used: used_addr,
                avail: avail_addr,
                log: 0,
            }
            .to_bytes(),
            &[],
        )
        .unwrap();
        fe.send(
            MsgHeader::new(Request::SetVringBase, 0, 8),
            &VringState { index: 1, num: 0 }.to_bytes(),
            &[],
        )
        .unwrap();

        let (kick_r, kick_w) = make_pipe();
        let (call_r, call_w) = make_pipe();
        fe.send(
            MsgHeader::new(Request::SetVringKick, 0, 8),
            &VringFdPayload {
                index: 1,
                no_fd: false,
            }
            .to_bytes(),
            &[kick_r.as_raw_fd()],
        )
        .unwrap();
        fe.send(
            MsgHeader::new(Request::SetVringCall, 0, 8),
            &VringFdPayload {
                index: 1,
                no_fd: false,
            }
            .to_bytes(),
            &[call_w.as_raw_fd()],
        )
        .unwrap();
        fe.send(
            MsgHeader::new(Request::SetVringEnable, 0, 8),
            &VringState { index: 1, num: 1 }.to_bytes(),
            &[],
        )
        .unwrap();
        // These were dup'd across the socket; our copies are no longer
        // needed once sent (except the ends we still use below).
        drop(kick_r);

        // --- write an INIT request into "guest memory" and kick ---
        let req = init_request(1);
        guest_mem
            .get_slice_mut(AddrSpace::Gpa, req_addr, req.len() as u64)
            .unwrap()
            .copy_from_slice(&req);

        // descriptor 0 (readable: the request), descriptor 1 (writable: the
        // reply). The descriptor *table* lives at a user address; the
        // `addr` field *inside* each descriptor is guest-physical.
        let d0 = desc_addr;
        guest_mem
            .get_slice_mut(AddrSpace::User, d0, 8)
            .unwrap()
            .copy_from_slice(&req_addr.to_le_bytes());
        guest_mem
            .write_u32(AddrSpace::User, d0 + 8, req.len() as u32)
            .unwrap();
        guest_mem
            .write_u16(AddrSpace::User, d0 + 12, 1 /* NEXT */)
            .unwrap();
        guest_mem.write_u16(AddrSpace::User, d0 + 14, 1).unwrap();
        let d1 = desc_addr + 16;
        guest_mem
            .get_slice_mut(AddrSpace::User, d1, 8)
            .unwrap()
            .copy_from_slice(&resp_addr.to_le_bytes());
        guest_mem.write_u32(AddrSpace::User, d1 + 8, 4096).unwrap();
        guest_mem
            .write_u16(AddrSpace::User, d1 + 12, 2 /* WRITE */)
            .unwrap();
        guest_mem.write_u16(AddrSpace::User, d1 + 14, 0).unwrap();

        guest_mem
            .write_u16(AddrSpace::User, avail_addr + 4, 0)
            .unwrap(); // ring[0] = head 0
        guest_mem
            .write_u16(AddrSpace::User, avail_addr + 2, 1)
            .unwrap(); // avail.idx = 1

        let one: u64 = 1;
        unsafe {
            libc::write(
                kick_w.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            );
        }

        // --- wait for the call fd to be notified, then check the used ring + reply ---
        let mut pfd = [libc::pollfd {
            fd: call_r.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let rc = unsafe { libc::poll(pfd.as_mut_ptr(), 1, 5000) };
        assert_eq!(rc, 1, "server should have notified the call fd within 5s");
        assert_ne!(pfd[0].revents & libc::POLLIN, 0);

        assert_eq!(guest_mem.read_u16(AddrSpace::User, used_addr + 2), Some(1)); // used.idx advanced
        let used_id = guest_mem.read_u32(AddrSpace::User, used_addr + 4).unwrap();
        let used_len = guest_mem.read_u32(AddrSpace::User, used_addr + 8).unwrap();
        assert_eq!(used_id, 0); // head descriptor index
        assert_eq!(used_len as usize, wire::INIT_OUT_LEN + wire::OUT_HEADER_LEN);

        let reply = guest_mem
            .get_slice(AddrSpace::Gpa, resp_addr, used_len as u64)
            .unwrap();
        let err = i32::from_le_bytes(reply[4..8].try_into().unwrap());
        assert_eq!(err, 0);
        let major = u32::from_le_bytes(
            reply[wire::OUT_HEADER_LEN..wire::OUT_HEADER_LEN + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(major, 7);

        drop(fe);
        handle.join().unwrap().unwrap();
    }
}
