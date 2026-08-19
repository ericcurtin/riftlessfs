//! A vhost-user connection: a UNIX domain socket that frames messages as
//! `(12-byte header, payload)` and carries `SCM_RIGHTS`-passed file
//! descriptors (for shared memory regions and kick/call/err doorbells)
//! alongside them.
//!
//! Ancillary (`SCM_RIGHTS`) data over a stream socket is associated with
//! whichever `sendmsg()`/`write()` call originally carried the bytes it
//! rides along with, not with a specific byte offset -- so to interop
//! correctly with real front-ends (QEMU, etc.) we always *send* a
//! header+payload pair in one `sendmsg()` call with every relevant fd
//! attached, and on *receive* we're lenient about which of our (possibly
//! multiple) `recv()` calls the fds actually show up on, by simply
//! accumulating fds across the whole read instead of assuming they arrive
//! with the first byte.

use std::io;
use std::os::fd::RawFd;
use std::os::unix::net::{UnixListener, UnixStream};

use sendfd::{RecvWithFd, SendWithFd};

use crate::error::{ProtoError, ProtoResult};
use crate::vhost_user::header::{MsgHeader, HEADER_LEN};

/// Generous fixed capacity for fds-per-`recv()` call. The largest payload
/// we handle that carries fds is `SET_MEM_TABLE`; `VHOST_MEMORY_MAX_NREGIONS`
/// in the reference implementation is 8, so 32 leaves comfortable headroom.
const MAX_FDS_PER_RECV: usize = 32;

pub struct Connection {
    stream: UnixStream,
}

impl Connection {
    pub fn from_stream(stream: UnixStream) -> Self {
        Connection { stream }
    }

    pub fn connect(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        Ok(Connection::from_stream(UnixStream::connect(path)?))
    }

    /// Accept a single incoming connection on `listener`. vhost-user
    /// back-ends normally serve exactly one front-end at a time per
    /// socket, so callers typically just want the first connection.
    pub fn accept(listener: &UnixListener) -> io::Result<Self> {
        let (stream, _addr) = listener.accept()?;
        Ok(Connection::from_stream(stream))
    }

    /// Send `header` followed by `payload`, with `fds` attached via
    /// `SCM_RIGHTS` to the same underlying `sendmsg()` call.
    pub fn send(&self, header: MsgHeader, payload: &[u8], fds: &[RawFd]) -> ProtoResult<()> {
        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        buf.extend_from_slice(&header.to_bytes());
        buf.extend_from_slice(payload);

        let mut sent = 0;
        while sent < buf.len() {
            let n = if sent == 0 && !fds.is_empty() {
                self.stream.send_with_fd(&buf[sent..], fds)?
            } else {
                self.stream.send_with_fd(&buf[sent..], &[])?
            };
            if n == 0 {
                return Err(ProtoError::Disconnected);
            }
            sent += n;
        }
        Ok(())
    }

    /// Receive one full message: header, payload, and any fds that arrived
    /// alongside either.
    pub fn recv(&self) -> ProtoResult<(MsgHeader, Vec<u8>, Vec<RawFd>)> {
        let mut fds = Vec::new();

        let mut header_buf = [0u8; HEADER_LEN];
        self.recv_exact(&mut header_buf, &mut fds)?;
        let header = MsgHeader::from_bytes(&header_buf);

        let size = header.size as usize;
        const MAX_PAYLOAD: usize = 1 << 20; // 1 MiB: generous, but bounded.
        if size > MAX_PAYLOAD {
            return Err(ProtoError::PayloadTooLarge(size));
        }
        let mut payload = vec![0u8; size];
        self.recv_exact(&mut payload, &mut fds)?;

        Ok((header, payload, fds))
    }

    /// Fill `buf` completely, appending any fds received along the way
    /// into `fds`. A zero-length `buf` is a no-op (used for messages with
    /// an empty payload).
    fn recv_exact(&self, buf: &mut [u8], fds: &mut Vec<RawFd>) -> ProtoResult<()> {
        let mut filled = 0;
        while filled < buf.len() {
            let mut fd_buf = [0i32; MAX_FDS_PER_RECV];
            let (n, nfds) = self.stream.recv_with_fd(&mut buf[filled..], &mut fd_buf)?;
            if n == 0 {
                return Err(ProtoError::Disconnected);
            }
            fds.extend_from_slice(&fd_buf[..nfds]);
            filled += n;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vhost_user::header::Request;
    use crate::vhost_user::payload::U64Payload;
    use std::os::fd::AsRawFd;

    #[test]
    fn send_recv_roundtrip_no_fds() {
        let (a, b) = UnixStream::pair().unwrap();
        let a = Connection::from_stream(a);
        let b = Connection::from_stream(b);

        let payload = U64Payload(0x1234).to_bytes();
        a.send(
            MsgHeader::new(Request::SetFeatures, 0, payload.len() as u32),
            &payload,
            &[],
        )
        .unwrap();

        let (header, recv_payload, fds) = b.recv().unwrap();
        assert_eq!(header.request().unwrap(), Request::SetFeatures);
        assert_eq!(recv_payload, payload);
        assert!(fds.is_empty());
    }

    #[test]
    fn send_recv_roundtrip_with_fd() {
        let (a, b) = UnixStream::pair().unwrap();
        let a = Connection::from_stream(a);
        let b = Connection::from_stream(b);

        let dummy = std::fs::File::open("/dev/null").unwrap();
        let payload = U64Payload(0).to_bytes();
        a.send(
            MsgHeader::new(Request::SetVringKick, 0, payload.len() as u32),
            &payload,
            &[dummy.as_raw_fd()],
        )
        .unwrap();

        let (header, _payload, fds) = b.recv().unwrap();
        assert_eq!(header.request().unwrap(), Request::SetVringKick);
        assert_eq!(fds.len(), 1);
        // We received a distinct (dup'd by the kernel) fd for the same
        // underlying file -- closing it is our responsibility.
        unsafe { libc::close(fds[0]) };
    }

    #[test]
    fn disconnect_is_reported_not_a_zero_length_message() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(a);
        let b = Connection::from_stream(b);
        assert!(matches!(b.recv(), Err(ProtoError::Disconnected)));
    }
}
