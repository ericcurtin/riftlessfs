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
use crate::fuse::wire::{self, Opcode, OutHeader, ReadIn, WriteIn};

use super::connection::Connection;
use super::header::{MsgHeader, Request};
use super::memory::GuestMemory;
use super::payload::{MemoryRegions, U64Payload, VringAddr, VringFdPayload, VringState};
use super::virtqueue::{DescChain, Virtqueue};

/// `fuse_in_header` (40 bytes) + `fuse_write_in`/`fuse_read_in` (40 bytes
/// each, per the kernel ABI -- see `fuse::wire`'s module docs) -- the
/// fixed-size prefix before a `WRITE` request's payload, or before
/// nothing at all for `READ` (whose own reply, not request, carries the
/// bulk data). Bounding a `gather_readable_prefix` call to this size
/// parses the header cheaply regardless of how large the actual request
/// is, without assuming the header lands on a descriptor boundary --
/// see `try_write_zero_copy`/`try_read_zero_copy`.
const REQUEST_HEADER_LEN: usize = wire::IN_HEADER_LEN + 40;

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
            // Peeked from a small bounded gather (at most
            // `REQUEST_HEADER_LEN` bytes) rather than a full
            // `gather_readable`, specifically so `WRITE`'s (potentially
            // up-to-1-MiB) payload isn't copied out just to read its
            // opcode -- see `try_write_zero_copy`/`try_read_zero_copy`,
            // which reuse this same bounded prefix to parse the rest of
            // the fixed-size header they need.
            let header_prefix = vq.gather_readable_prefix(mem, &chain, REQUEST_HEADER_LEN)?;
            let opcode = header_prefix
                .get(4..8)
                .map(|b| Opcode::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
                .unwrap_or(Opcode::Unknown(0));

            // `WRITE`/`READ` get a zero-copy fast path straight against
            // guest memory (via `preadv`/`pwritev`), avoiding the
            // intermediate `Vec<u8>` copy `gather_readable`/
            // `scatter_writable` otherwise do for their (potentially
            // large) payload -- see BENCHMARKS.md's zero-copy-I/O
            // finding (from reading virtiofsd's own implementation).
            // Both fall back to the ordinary generic dispatch path
            // (`Session::handle`) if their header can't be parsed for
            // any reason, same as a malformed request would hit there
            // anyway -- deliberately not duplicating that error
            // handling here.
            let written = match opcode {
                Opcode::Write => {
                    match try_write_zero_copy(&self.session, mem, vq, &chain, &header_prefix) {
                        Some(result) => result?,
                        None => dispatch_generic(&self.session, mem, vq, &chain)?,
                    }
                }
                Opcode::Read => {
                    match try_read_zero_copy(&self.session, mem, vq, &chain, &header_prefix) {
                        Some(result) => result?,
                        None => dispatch_generic(&self.session, mem, vq, &chain)?,
                    }
                }
                _ => dispatch_generic(&self.session, mem, vq, &chain)?,
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

/// The ordinary (non-zero-copy) request path: gather the whole readable
/// chain into a buffer, dispatch it, and scatter the reply back into the
/// writable chain. Used for every opcode except `WRITE`/`READ`'s
/// zero-copy fast paths below, and as their fallback if those can't
/// apply to a given request.
fn dispatch_generic(
    session: &Session,
    mem: &GuestMemory,
    vq: &Virtqueue,
    chain: &DescChain,
) -> ProtoResult<u32> {
    let request = vq.gather_readable(mem, chain)?;
    let reply = session.handle(&request);
    Ok(match reply {
        Reply::Bytes(bytes) => vq.scatter_writable(mem, chain, &bytes)?,
        Reply::None => 0,
    })
}

/// Attempt the zero-copy `WRITE` path: parse `fh`/`offset`/`size` from
/// `header_prefix` (already gathered by the caller, bounded to
/// `REQUEST_HEADER_LEN`), then `pwritev()` directly from the readable
/// chain's guest-memory segments -- skipping past the header, wherever
/// it actually lands relative to descriptor boundaries (see
/// `Virtqueue::iovecs_from`) -- instead of copying the payload into a
/// `Vec<u8>` first the way `dispatch_generic`/`PassthroughFs::write` do.
///
/// Returns `None` if `header_prefix` doesn't parse as a valid `WRITE`
/// header at all, in which case the caller should fall back to
/// `dispatch_generic` (which has its own handling for malformed
/// requests -- deliberately not duplicated here). `Some(Err(_))` is
/// reserved for a real protocol-level error encountered *after*
/// committing to this path (e.g. a descriptor pointing outside mapped
/// guest memory), matching how `dispatch_generic` itself propagates
/// errors rather than swallowing them.
fn try_write_zero_copy(
    session: &Session,
    mem: &GuestMemory,
    vq: &Virtqueue,
    chain: &DescChain,
    header_prefix: &[u8],
) -> Option<ProtoResult<u32>> {
    let unique = wire::InHeader::from_bytes(header_prefix).ok()?.unique;
    let (w, _) = WriteIn::from_bytes(wire::InHeader::body(header_prefix)).ok()?;

    let iov = match Virtqueue::iovecs_from(
        mem,
        &chain.readable,
        REQUEST_HEADER_LEN as u64,
        w.size as u64,
        false,
    ) {
        Ok(iov) => iov,
        Err(e) => return Some(Err(e.into())),
    };

    // The reply itself (a `fuse_write_out`, 8 bytes) is far too small to
    // bother avoiding a copy for -- only the request's payload needed
    // the zero-copy treatment above.
    let wire_reply = match session.fs().write_vectored(w.fh, w.offset, &iov) {
        Ok(written) => OutHeader::reply(unique, 0, &wire::write_out(written as u32)),
        Err(e) => OutHeader::error_for(unique, e.errno()),
    };
    Some(
        vq.scatter_writable(mem, chain, &wire_reply)
            .map_err(Into::into),
    )
}

/// Attempt the zero-copy `READ` path: parse `fh`/`offset`/`size` from
/// `header_prefix`, reserve the writable chain's first `OUT_HEADER_LEN`
/// bytes for the reply header, `preadv()` directly into everything
/// after that (up to `size` bytes, or however much writable space the
/// guest actually offered, whichever is smaller), then fill in the
/// header last -- once the real byte count is known, since a read can
/// legitimately return fewer bytes than requested (e.g. near EOF) --
/// instead of `PassthroughFs::read`'s `Vec<u8>` (sized to the request)
/// that `scatter_writable` would otherwise copy out of.
///
/// Returns `None`/`Some(Err(_))` under the same conditions as
/// [`try_write_zero_copy`] (see its doc comment).
fn try_read_zero_copy(
    session: &Session,
    mem: &GuestMemory,
    vq: &Virtqueue,
    chain: &DescChain,
    header_prefix: &[u8],
) -> Option<ProtoResult<u32>> {
    let unique = wire::InHeader::from_bytes(header_prefix).ok()?.unique;
    let r = ReadIn::from_bytes(wire::InHeader::body(header_prefix)).ok()?;

    let writable_capacity: u64 = chain.writable.iter().map(|&(_, len)| len).sum();
    let payload_capacity = writable_capacity.saturating_sub(wire::OUT_HEADER_LEN as u64);
    let want = (r.size as u64).min(payload_capacity);

    let iov = match Virtqueue::iovecs_from(
        mem,
        &chain.writable,
        wire::OUT_HEADER_LEN as u64,
        want,
        true,
    ) {
        Ok(iov) => iov,
        Err(e) => return Some(Err(e.into())),
    };

    match session.fs().read_vectored(r.fh, r.offset, &iov) {
        Ok(n) => {
            let header = OutHeader::success_header_only(unique, (wire::OUT_HEADER_LEN + n) as u32);
            match vq.scatter_writable(mem, chain, &header) {
                Ok(header_written) => Some(Ok(header_written + n as u32)),
                Err(e) => Some(Err(e.into())),
            }
        }
        Err(e) => {
            let error_reply = OutHeader::error_for(unique, e.errno());
            Some(
                vq.scatter_writable(mem, chain, &error_reply)
                    .map_err(Into::into),
            )
        }
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

    const TEST_QUEUE_SIZE: u16 = 64;

    /// A real `Server` driven over a real `UnixStream::pair()`, with one
    /// virtqueue set up and enabled the same way a real vhost-user
    /// front-end (QEMU) would, plus a bump allocator over its mapped
    /// "guest memory" region for building descriptor chains -- shared
    /// setup for [`full_handshake_and_one_request_over_a_real_socket`]
    /// and the zero-copy `READ`/`WRITE` tests below, which all need this
    /// same real end-to-end path (not just `Session::handle` in
    /// isolation) to actually exercise `Server::process_vring`'s
    /// opcode-peeking and zero-copy branches.
    struct TestQueue {
        handle: std::thread::JoinHandle<ProtoResult<()>>,
        guest_mem: GuestMemory,
        desc_addr: u64,
        avail_addr: u64,
        used_addr: u64,
        call_r: OwnedFd,
        kick_w: OwnedFd,
        next_gpa: u64,
        next_desc_idx: u16,
        avail_idx: u16,
    }

    impl TestQueue {
        fn new(session: Session) -> (Connection, Self) {
            let (fe, be) = UnixStream::pair().unwrap();
            let server = Server::new(Connection::from_stream(be), session);
            let handle = std::thread::spawn(move || server.run());
            let fe = Connection::from_stream(fe);

            fe.send(MsgHeader::new(Request::GetFeatures, 0, 0), &[], &[])
                .unwrap();
            fe.recv().unwrap();
            fe.send(
                MsgHeader::new(Request::SetFeatures, 0, 8),
                &U64Payload(VIRTIO_FEATURES).to_bytes(),
                &[],
            )
            .unwrap();
            fe.send(MsgHeader::new(Request::GetProtocolFeatures, 0, 0), &[], &[])
                .unwrap();
            fe.recv().unwrap();
            fe.send(
                MsgHeader::new(Request::SetProtocolFeatures, 0, 8),
                &U64Payload(0).to_bytes(),
                &[],
            )
            .unwrap();
            fe.send(MsgHeader::new(Request::SetOwner, 0, 0), &[], &[])
                .unwrap();

            // 8 MiB region -- comfortably fits a handful of large
            // (up-to-1-MiB) `WRITE`/`READ` payloads across multiple
            // descriptors for the zero-copy tests, on top of the ring
            // structures themselves.
            let mem_file = tempfile::tempfile().unwrap();
            let mem_len: u64 = 8 << 20;
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
            let guest_mem = GuestMemory::map_regions(&[region], &[mem_file.as_raw_fd()]).unwrap();

            let desc_addr = USER_BASE + 0x1000;
            let avail_addr = desc_addr + TEST_QUEUE_SIZE as u64 * 16;
            let used_addr = avail_addr + 4 + TEST_QUEUE_SIZE as u64 * 2 + 64;
            // Buffers start well past the ring structures, at a
            // page-aligned offset for tidiness (not load-bearing).
            let data_base = GPA_BASE + 0x10_0000;

            fe.send(
                MsgHeader::new(Request::SetVringNum, 0, 8),
                &VringState {
                    index: 1,
                    num: TEST_QUEUE_SIZE as u32,
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
            drop(kick_r);

            (
                fe,
                TestQueue {
                    handle,
                    guest_mem,
                    desc_addr,
                    avail_addr,
                    used_addr,
                    call_r,
                    kick_w,
                    next_gpa: data_base,
                    next_desc_idx: 0,
                    avail_idx: 0,
                },
            )
        }

        /// Copy `data` into a fresh guest-physical buffer and return its
        /// `(addr, len)`, for a readable descriptor.
        fn readable_buf(&mut self, data: &[u8]) -> (u64, u32) {
            let addr = self.next_gpa;
            self.next_gpa += data.len() as u64;
            self.guest_mem
                .get_slice_mut(AddrSpace::Gpa, addr, data.len() as u64)
                .unwrap()
                .copy_from_slice(data);
            (addr, data.len() as u32)
        }

        /// Reserve `len` fresh (zeroed) guest-physical bytes and return
        /// its `(addr, len)`, for a writable descriptor (a reply
        /// buffer).
        fn writable_buf(&mut self, len: u32) -> (u64, u32) {
            let addr = self.next_gpa;
            self.next_gpa += len as u64;
            (addr, len)
        }

        /// Write a full descriptor chain from `segments`
        /// (`(addr, len, writable)`), linking them via `NEXT`, and
        /// return the chain's head index. Descriptor flag bits (`NEXT`
        /// = 1, `WRITE` = 2) are the virtio spec's, hardcoded as literals
        /// here rather than importing `virtqueue`'s private constants
        /// (matching the pre-existing test in this file, which does the
        /// same).
        fn write_chain(&mut self, segments: &[(u64, u32, bool)]) -> u16 {
            let start_idx = self.next_desc_idx;
            let n = segments.len();
            for (i, &(addr, len, writable)) in segments.iter().enumerate() {
                let idx = self.next_desc_idx;
                self.next_desc_idx += 1;
                let mut flags = if writable {
                    2 /* WRITE */
                } else {
                    0
                };
                let next = if i + 1 < n {
                    flags |= 1 /* NEXT */;
                    idx + 1
                } else {
                    0
                };
                let off = self.desc_addr + idx as u64 * 16;
                self.guest_mem
                    .get_slice_mut(AddrSpace::User, off, 8)
                    .unwrap()
                    .copy_from_slice(&addr.to_le_bytes());
                self.guest_mem
                    .write_u32(AddrSpace::User, off + 8, len)
                    .unwrap();
                self.guest_mem
                    .write_u16(AddrSpace::User, off + 12, flags)
                    .unwrap();
                self.guest_mem
                    .write_u16(AddrSpace::User, off + 14, next)
                    .unwrap();
            }
            start_idx
        }

        /// Publish `head` as the next avail entry, kick, wait for the
        /// call fd to be notified, and return the corresponding used
        /// entry's `(id, len)`.
        fn submit_and_wait(&mut self, head: u16) -> (u32, u32) {
            let slot = self.avail_idx % TEST_QUEUE_SIZE;
            let ring_off = self.avail_addr + 4 + slot as u64 * 2;
            self.guest_mem
                .write_u16(AddrSpace::User, ring_off, head)
                .unwrap();
            self.avail_idx = self.avail_idx.wrapping_add(1);
            self.guest_mem
                .write_u16(AddrSpace::User, self.avail_addr + 2, self.avail_idx)
                .unwrap();

            let one: u64 = 1;
            unsafe {
                libc::write(
                    self.kick_w.as_raw_fd(),
                    &one as *const u64 as *const libc::c_void,
                    8,
                );
            }

            let mut pfd = [libc::pollfd {
                fd: self.call_r.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            let rc = unsafe { libc::poll(pfd.as_mut_ptr(), 1, 5000) };
            assert_eq!(rc, 1, "server should have notified the call fd within 5s");
            let mut drain = [0u8; 64];
            unsafe {
                libc::read(
                    self.call_r.as_raw_fd(),
                    drain.as_mut_ptr() as *mut libc::c_void,
                    drain.len(),
                );
            }

            let used_idx = self
                .guest_mem
                .read_u16(AddrSpace::User, self.used_addr + 2)
                .unwrap();
            let slot = used_idx.wrapping_sub(1) % TEST_QUEUE_SIZE;
            let elem_off = self.used_addr + 4 + slot as u64 * 8;
            let used_id = self.guest_mem.read_u32(AddrSpace::User, elem_off).unwrap();
            let used_len = self
                .guest_mem
                .read_u32(AddrSpace::User, elem_off + 4)
                .unwrap();
            (used_id, used_len)
        }
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
    /// handshake over an actual `UnixStream::pair()`, using
    /// [`TestQueue`] for the shared-memory/virtqueue setup (which
    /// deliberately uses *different* values for the guest-physical base
    /// and the "user address" base, unlike a naive test that picks 0 for
    /// both: mixing up the two address spaces, see the `memory` module
    /// docs, doesn't fail loudly when they happen to coincide, which is
    /// exactly how that bug first slipped past this test and was only
    /// caught against a real QEMU front-end), place a FUSE INIT request
    /// in it, kick, and confirm we get a correctly negotiated INIT reply
    /// plus a call-fd notification -- all without a real VM or kernel.
    #[test]
    fn full_handshake_and_one_request_over_a_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path()).unwrap();
        let session = Session::new(fs);
        let (fe, mut tq) = TestQueue::new(session);

        let req = init_request(1);
        let (req_addr, req_len) = tq.readable_buf(&req);
        let (resp_addr, resp_len) = tq.writable_buf(4096);
        let head = tq.write_chain(&[(req_addr, req_len, false), (resp_addr, resp_len, true)]);
        let (used_id, used_len) = tq.submit_and_wait(head);

        assert_eq!(used_id, head as u32);
        assert_eq!(used_len as usize, wire::INIT_OUT_LEN + wire::OUT_HEADER_LEN);

        let reply = tq
            .guest_mem
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
        tq.handle.join().unwrap().unwrap();
    }

    /// Build a raw `WRITE` request's readable bytes (`fuse_in_header` +
    /// `fuse_write_in`, with `payload` appended so tests that don't care
    /// about zero-copy specifically can pass it as one descriptor).
    fn write_request_header(unique: u64, fh: u64, offset: u64, size: u32) -> Vec<u8> {
        let mut body = Writer::new();
        body.u64(fh).u64(offset).u32(size);
        body.u32(0); // write_flags
        body.u64(0); // lock_owner
        body.u32(0).u32(0); // flags, padding
        let body = body.into_vec();
        assert_eq!(body.len(), 40, "fuse_write_in must be 40 bytes");

        let mut w = Writer::new();
        w.u32((wire::IN_HEADER_LEN + body.len()) as u32);
        w.u32(16); // FUSE_WRITE
        w.u64(unique);
        w.u64(1); // nodeid (unused by our dispatch for WRITE)
        w.u32(0).u32(0).u32(0); // uid, gid, pid
        w.u32(0); // total_extlen + padding
        w.bytes(&body);
        w.into_vec()
    }

    fn read_request_header(unique: u64, fh: u64, offset: u64, size: u32) -> Vec<u8> {
        let mut body = Writer::new();
        body.u64(fh).u64(offset).u32(size);
        body.u32(0); // read_flags
        body.u64(0); // lock_owner
        body.u32(0).u32(0); // flags, padding
        let body = body.into_vec();
        assert_eq!(body.len(), 40, "fuse_read_in must be 40 bytes");

        let mut w = Writer::new();
        w.u32((wire::IN_HEADER_LEN + body.len()) as u32);
        w.u32(15); // FUSE_READ
        w.u64(unique);
        w.u64(1); // nodeid
        w.u32(0).u32(0).u32(0); // uid, gid, pid
        w.u32(0); // total_extlen + padding
        w.bytes(&body);
        w.into_vec()
    }

    /// End-to-end test of `try_write_zero_copy`/`try_read_zero_copy`
    /// through the *real* `Server`/virtqueue path (not just the unit
    /// tests of their building blocks in `virtqueue.rs`/
    /// `riftlessfs-core`): creates a real file, writes to it with a
    /// payload deliberately split across *three* separate readable
    /// descriptors (exercising `Virtqueue::iovecs_from`'s
    /// skip-and-gather-multiple-segments logic against real guest
    /// memory, not synthetic test buffers), reads it back with the
    /// reply split across *two* writable descriptors, and confirms the
    /// bytes on disk and the bytes read back both match exactly what
    /// was written -- the correctness bar that actually matters for a
    /// zero-copy data path, not just "it returns the right byte count".
    #[test]
    fn zero_copy_write_then_read_multi_descriptor_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path()).unwrap();
        let (_ino, fh, _attr) = fs
            .create(
                riftlessfs_core::ROOT_ID,
                std::ffi::OsStr::new("zerocopy.bin"),
                libc::O_RDWR,
                0o644,
            )
            .unwrap();
        let session = Session::new(fs);
        let (fe, mut tq) = TestQueue::new(session);

        // --- WRITE: header in its own descriptor, payload split across
        // three more (4000 + 4000 + 2000 = 10000 bytes) ---
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let header = write_request_header(2, fh, 0, payload.len() as u32);
        let (header_addr, header_len) = tq.readable_buf(&header);
        let (p0_addr, p0_len) = tq.readable_buf(&payload[0..4000]);
        let (p1_addr, p1_len) = tq.readable_buf(&payload[4000..8000]);
        let (p2_addr, p2_len) = tq.readable_buf(&payload[8000..10_000]);
        let (resp_addr, resp_len) = tq.writable_buf(64);
        let head = tq.write_chain(&[
            (header_addr, header_len, false),
            (p0_addr, p0_len, false),
            (p1_addr, p1_len, false),
            (p2_addr, p2_len, false),
            (resp_addr, resp_len, true),
        ]);
        let (used_id, used_len) = tq.submit_and_wait(head);
        assert_eq!(used_id, head as u32);

        let write_reply = tq
            .guest_mem
            .get_slice(AddrSpace::Gpa, resp_addr, used_len as u64)
            .unwrap();
        let err = i32::from_le_bytes(write_reply[4..8].try_into().unwrap());
        assert_eq!(err, 0, "WRITE should succeed");
        let written = u32::from_le_bytes(
            write_reply[wire::OUT_HEADER_LEN..wire::OUT_HEADER_LEN + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(written as usize, payload.len());

        let on_disk = std::fs::read(dir.path().join("zerocopy.bin")).unwrap();
        assert_eq!(
            on_disk, payload,
            "multi-descriptor WRITE landed on disk exactly as sent"
        );

        // --- READ: 10000 bytes back, reply split across two writable
        // descriptors (6000 + 4000, an arbitrary split not aligned to
        // the WRITE side's descriptor boundaries) ---
        // Writable capacity must cover the header *plus* the full
        // payload -- a real guest always sizes its reply buffer this
        // way, knowing the header overhead in advance; split unevenly
        // (6016 + 4000) so the header lands entirely in the first
        // descriptor without being a "nice" round number.
        let read_req = read_request_header(3, fh, 0, payload.len() as u32);
        let (read_req_addr, read_req_len) = tq.readable_buf(&read_req);
        let (out0_addr, out0_len) = tq.writable_buf(6016);
        let (out1_addr, out1_len) = tq.writable_buf(4000);
        let head = tq.write_chain(&[
            (read_req_addr, read_req_len, false),
            (out0_addr, out0_len, true),
            (out1_addr, out1_len, true),
        ]);
        let (used_id, used_len) = tq.submit_and_wait(head);
        assert_eq!(used_id, head as u32);
        assert_eq!(used_len as usize, wire::OUT_HEADER_LEN + payload.len());

        // Reassemble the reply from across both writable descriptors:
        // header + up-to-6000 bytes of payload in the first, the rest in
        // the second.
        let first = tq
            .guest_mem
            .get_slice(AddrSpace::Gpa, out0_addr, out0_len as u64)
            .unwrap();
        let err = i32::from_le_bytes(first[4..8].try_into().unwrap());
        assert_eq!(err, 0, "READ should succeed");
        let mut reassembled = Vec::new();
        reassembled.extend_from_slice(&first[wire::OUT_HEADER_LEN..]);
        let remaining = payload.len() - (out0_len as usize - wire::OUT_HEADER_LEN);
        let second = tq
            .guest_mem
            .get_slice(AddrSpace::Gpa, out1_addr, remaining as u64)
            .unwrap();
        reassembled.extend_from_slice(second);

        assert_eq!(
            reassembled, payload,
            "multi-descriptor READ reassembled exactly as written"
        );

        drop(fe);
        tq.handle.join().unwrap().unwrap();
    }

    /// A `READ` past EOF should succeed with a short reply (however many
    /// real bytes exist), not an error -- exercised through the real
    /// zero-copy path specifically, since `preadv`'s short-read-at-EOF
    /// behavior is exactly the kind of thing worth confirming end to end
    /// rather than assuming from the unit-level `read_vectored` test in
    /// `riftlessfs-core`.
    #[test]
    fn zero_copy_read_past_eof_returns_short_success_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path()).unwrap();
        let (_ino, fh, _attr) = fs
            .create(
                riftlessfs_core::ROOT_ID,
                std::ffi::OsStr::new("short.bin"),
                libc::O_RDWR,
                0o644,
            )
            .unwrap();
        fs.write(fh, 0, b"0123456789").unwrap();
        fs.fsync(fh).unwrap();
        let session = Session::new(fs);
        let (fe, mut tq) = TestQueue::new(session);

        // Ask for 100 bytes at offset 6 -- only 4 real bytes ("6789")
        // exist past that point.
        let read_req = read_request_header(4, fh, 6, 100);
        let (req_addr, req_len) = tq.readable_buf(&read_req);
        let (out_addr, out_len) = tq.writable_buf(200);
        let head = tq.write_chain(&[(req_addr, req_len, false), (out_addr, out_len, true)]);
        let (used_id, used_len) = tq.submit_and_wait(head);
        assert_eq!(used_id, head as u32);
        assert_eq!(used_len as usize, wire::OUT_HEADER_LEN + 4);

        let reply = tq
            .guest_mem
            .get_slice(AddrSpace::Gpa, out_addr, used_len as u64)
            .unwrap();
        let err = i32::from_le_bytes(reply[4..8].try_into().unwrap());
        assert_eq!(err, 0, "a short read past EOF is success, not an error");
        assert_eq!(&reply[wire::OUT_HEADER_LEN..], b"6789");

        drop(fe);
        tq.handle.join().unwrap().unwrap();
    }

    /// A `WRITE`/`READ` against a bogus file handle should still produce
    /// a proper errno reply through the zero-copy path (falling through
    /// to `PassthroughFs`'s own handle-lookup error, not panicking or
    /// silently dropping the reply) -- confirms `try_write_zero_copy`/
    /// `try_read_zero_copy`'s error branch, not just their success path.
    #[test]
    fn zero_copy_write_with_bad_handle_returns_errno_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path()).unwrap();
        let session = Session::new(fs);
        let (fe, mut tq) = TestQueue::new(session);

        let bogus_fh = 0xdead_beef_u64;
        let req = write_request_header(5, bogus_fh, 0, 4);
        let (req_addr, req_len) = tq.readable_buf(&req);
        let (payload_addr, payload_len) = tq.readable_buf(b"data");
        let (resp_addr, resp_len) = tq.writable_buf(64);
        let head = tq.write_chain(&[
            (req_addr, req_len, false),
            (payload_addr, payload_len, false),
            (resp_addr, resp_len, true),
        ]);
        let (used_id, used_len) = tq.submit_and_wait(head);
        assert_eq!(used_id, head as u32);
        assert_eq!(
            used_len as usize,
            wire::OUT_HEADER_LEN,
            "error replies carry no body"
        );

        let reply = tq
            .guest_mem
            .get_slice(AddrSpace::Gpa, resp_addr, used_len as u64)
            .unwrap();
        let err = i32::from_le_bytes(reply[4..8].try_into().unwrap());
        assert_ne!(
            err, 0,
            "a bogus handle should produce an error, not success"
        );

        drop(fe);
        tq.handle.join().unwrap().unwrap();
    }
}
