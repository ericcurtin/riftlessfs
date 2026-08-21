//! Dispatch: decode one gathered FUSE-over-virtio request, call the
//! matching `PassthroughFs` operation, and encode the reply.
//!
//! A [`Session`] owns the [`PassthroughFs`] plus the small bit of extra
//! state FUSE's wire protocol needs that the filesystem engine itself
//! doesn't track: a per-directory-handle cache of the entries returned by
//! the *first* `READDIR` call for that handle, so that a second `READDIR`
//! call at a later `offset` (which happens whenever a directory's listing
//! doesn't fit in one reply buffer) can serve a slice of the same listing
//! rather than re-reading a live, position-advancing OS directory stream
//! and getting an inconsistent result. This is the same "list once, page
//! through a snapshot" approach many minimal passthrough filesystems use.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Mutex;

use riftlessfs_core::{DirEntry, FileKind, FsError, PassthroughFs, SetAttr};

use super::wire::{self, ForgetIn, InHeader, Opcode, OutHeader};

/// What a dispatched request produced.
pub enum Reply {
    /// Write these bytes back to the guest and complete the descriptor
    /// chain with them.
    Bytes(Vec<u8>),
    /// Complete the descriptor chain with zero bytes written and *no*
    /// `fuse_out_header` at all -- used for `FORGET`/`BATCH_FORGET`, which
    /// the FUSE protocol defines as receiving no reply whatsoever.
    None,
}

pub struct Session {
    fs: PassthroughFs,
    readdir_cache: Mutex<HashMap<u64, Vec<DirEntry>>>,
}

fn errno_reply(unique: u64, e: FsError) -> Reply {
    Reply::Bytes(OutHeader::error_for(unique, e.errno()))
}

fn ok_reply(unique: u64, body: Vec<u8>) -> Reply {
    Reply::Bytes(OutHeader::reply(unique, 0, &body))
}

fn dt_type(kind: FileKind) -> u32 {
    match kind {
        FileKind::Dir => wire::DT_DIR,
        FileKind::Regular => wire::DT_REG,
        FileKind::Symlink => wire::DT_LNK,
        FileKind::Other => wire::DT_UNKNOWN,
    }
}

impl Session {
    pub fn new(fs: PassthroughFs) -> Self {
        Session {
            fs,
            readdir_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn fs(&self) -> &PassthroughFs {
        &self.fs
    }

    /// Handle one already-gathered request buffer (header + body), and
    /// produce the reply to write back.
    pub fn handle(&self, request: &[u8]) -> Reply {
        let header = match InHeader::from_bytes(request) {
            Ok(h) => h,
            Err(_) => return Reply::None, // nothing sane to reply with
        };
        let body = InHeader::body(request);
        let unique = header.unique;
        let nodeid = header.nodeid;

        macro_rules! try_fs {
            ($expr:expr) => {
                match $expr {
                    Ok(v) => v,
                    Err(e) => return errno_reply(unique, e),
                }
            };
        }
        macro_rules! try_parse {
            ($expr:expr) => {
                match $expr {
                    Ok(v) => v,
                    Err(_) => return Reply::Bytes(OutHeader::error_for(unique, libc::EINVAL)),
                }
            };
        }

        match Opcode::from(header.opcode) {
            Opcode::Init => {
                let init = try_parse!(wire::InitIn::from_bytes(body));
                ok_reply(unique, wire::init_out(init.max_readahead))
            }

            Opcode::Destroy => ok_reply(unique, Vec::new()),

            Opcode::Forget => {
                let f = try_parse!(ForgetIn::from_bytes(body));
                self.fs.forget(nodeid, f.nlookup);
                Reply::None
            }
            Opcode::BatchForget => {
                let entries = try_parse!(wire::parse_batch_forget(body));
                for e in entries {
                    self.fs.forget(e.nodeid, e.nlookup);
                }
                Reply::None
            }

            Opcode::Lookup => {
                let name = OsStr::from_bytes(trim_nul(body));
                let (ino, attr) = try_fs!(self.fs.lookup(nodeid, name));
                ok_reply(unique, wire::entry_out(ino, &attr))
            }

            Opcode::Getattr => {
                let _ = try_parse!(wire::GetattrIn::from_bytes(body));
                let attr = try_fs!(self.fs.getattr(nodeid));
                ok_reply(unique, wire::attr_out(&attr))
            }

            Opcode::Setattr => {
                let s = try_parse!(wire::SetattrIn::from_bytes(body));
                let mut set = SetAttr::default();
                if s.valid & wire::FATTR_SIZE != 0 {
                    set.size = Some(s.size);
                }
                if s.valid & wire::FATTR_MODE != 0 {
                    set.mode = Some(s.mode);
                }
                if s.valid & wire::FATTR_UID != 0 {
                    set.uid = Some(s.uid);
                }
                if s.valid & wire::FATTR_GID != 0 {
                    set.gid = Some(s.gid);
                }
                if s.valid & wire::FATTR_ATIME_NOW != 0 {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default();
                    set.atime = Some((now.as_secs() as i64, now.subsec_nanos()));
                } else if s.valid & wire::FATTR_ATIME != 0 {
                    set.atime = Some((s.atime as i64, s.atimensec));
                }
                if s.valid & wire::FATTR_MTIME_NOW != 0 {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default();
                    set.mtime = Some((now.as_secs() as i64, now.subsec_nanos()));
                } else if s.valid & wire::FATTR_MTIME != 0 {
                    set.mtime = Some((s.mtime as i64, s.mtimensec));
                }
                let attr = try_fs!(self.fs.setattr(nodeid, &set));
                ok_reply(unique, wire::attr_out(&attr))
            }

            Opcode::Readlink => {
                let target = try_fs!(self.fs.readlink(nodeid));
                ok_reply(unique, target.as_bytes().to_vec())
            }

            Opcode::Symlink => {
                let mut it = split_two_cstrs(body);
                let (Some(name), Some(target)) = (it.next(), it.next()) else {
                    return Reply::Bytes(OutHeader::error_for(unique, libc::EINVAL));
                };
                let (ino, attr) = try_fs!(self.fs.symlink(
                    nodeid,
                    OsStr::from_bytes(name),
                    OsStr::from_bytes(target)
                ));
                ok_reply(unique, wire::entry_out(ino, &attr))
            }

            Opcode::Mkdir => {
                let (m, name) = try_parse!(wire::MkdirIn::from_bytes(body));
                let (ino, attr) = try_fs!(self.fs.mkdir(nodeid, OsStr::from_bytes(name), m.mode));
                ok_reply(unique, wire::entry_out(ino, &attr))
            }

            Opcode::Unlink => {
                let name = OsStr::from_bytes(trim_nul(body));
                try_fs!(self.fs.unlink(nodeid, name));
                ok_reply(unique, Vec::new())
            }

            Opcode::Rmdir => {
                let name = OsStr::from_bytes(trim_nul(body));
                try_fs!(self.fs.rmdir(nodeid, name));
                ok_reply(unique, Vec::new())
            }

            Opcode::Rename => {
                let (r, old, new) = try_parse!(wire::RenameIn::from_bytes(body));
                try_fs!(self.fs.rename(
                    nodeid,
                    OsStr::from_bytes(old),
                    r.newdir,
                    OsStr::from_bytes(new)
                ));
                ok_reply(unique, Vec::new())
            }
            Opcode::Rename2 => {
                let (r, old, new) = try_parse!(wire::RenameIn::from_bytes_v2(body));
                try_fs!(self.fs.rename(
                    nodeid,
                    OsStr::from_bytes(old),
                    r.newdir,
                    OsStr::from_bytes(new)
                ));
                ok_reply(unique, Vec::new())
            }

            Opcode::Link => {
                let (l, name) = try_parse!(wire::LinkIn::from_bytes(body));
                let attr = try_fs!(self.fs.link(l.oldnodeid, nodeid, OsStr::from_bytes(name)));
                // nodeid of the (already-existing) linked inode is oldnodeid.
                ok_reply(unique, wire::entry_out(l.oldnodeid, &attr))
            }

            Opcode::Open => {
                let o = try_parse!(wire::OpenIn::from_bytes(body));
                let handle = try_fs!(self.fs.open(nodeid, o.flags as i32));
                ok_reply(unique, wire::open_out(handle))
            }

            Opcode::Create => {
                let (c, name) = try_parse!(wire::CreateIn::from_bytes(body));
                let (ino, handle, attr) = try_fs!(self.fs.create(
                    nodeid,
                    OsStr::from_bytes(name),
                    c.flags as i32,
                    c.mode
                ));
                let mut reply = wire::entry_out(ino, &attr);
                reply.extend_from_slice(&wire::open_out(handle));
                ok_reply(unique, reply)
            }

            Opcode::Read => {
                let r = try_parse!(wire::ReadIn::from_bytes(body));
                // Isolates the underlying `pread` syscall's own cost from
                // everything else in the request's round trip (virtqueue
                // parsing, guest-memory gather/scatter, notification) --
                // see the random-write investigation in BENCHMARKS.md,
                // which needed to know whether the write-vs-read
                // throughput gap versus virtiofsd is *inside* this
                // syscall or somewhere else in the pipeline.
                let t0 = std::time::Instant::now();
                let data = try_fs!(self.fs.read(r.fh, r.offset, r.size as usize));
                log::trace!("pread({} bytes) took {:?}", data.len(), t0.elapsed());
                ok_reply(unique, data)
            }

            Opcode::Write => {
                let (w, data) = try_parse!(wire::WriteIn::from_bytes(body));
                let n = (w.size as usize).min(data.len());
                let t0 = std::time::Instant::now();
                let written = try_fs!(self.fs.write(w.fh, w.offset, &data[..n]));
                log::trace!("pwrite({n} bytes) took {:?}", t0.elapsed());
                ok_reply(unique, wire::write_out(written as u32))
            }

            Opcode::Flush => {
                let _ = try_parse!(wire::FlushIn::from_bytes(body));
                // No-op: data already goes straight to the real fd via
                // pwrite, so there's nothing to flush.
                ok_reply(unique, Vec::new())
            }

            Opcode::Fsync => {
                let mut r = super::bytes::Reader::new(body);
                let fh = try_parse!(r.u64());
                try_fs!(self.fs.fsync(fh));
                ok_reply(unique, Vec::new())
            }

            Opcode::Release => {
                let rel = try_parse!(wire::ReleaseIn::from_bytes(body));
                try_fs!(self.fs.release(rel.fh));
                ok_reply(unique, Vec::new())
            }

            Opcode::Opendir => {
                let o = try_parse!(wire::OpenIn::from_bytes(body));
                let _ = o.flags;
                let handle = try_fs!(self.fs.opendir(nodeid));
                ok_reply(unique, wire::open_out(handle))
            }

            Opcode::Readdir => {
                let r = try_parse!(wire::ReadIn::from_bytes(body));
                let entries = {
                    let mut cache = self.readdir_cache.lock().unwrap();
                    if r.offset == 0 || !cache.contains_key(&r.fh) {
                        let listing = try_fs!(self.fs.readdir(r.fh));
                        cache.insert(r.fh, listing);
                    }
                    cache.get(&r.fh).cloned().unwrap_or_default()
                };
                let mut out = Vec::new();
                let start = r.offset as usize;
                for (i, entry) in entries.iter().enumerate().skip(start) {
                    let off = (i + 1) as u64;
                    let remaining = r.size as usize - out.len();
                    let fit = wire::push_dirent(
                        &mut out,
                        remaining,
                        entry.ino,
                        off,
                        dt_type(entry.kind),
                        entry.name.as_bytes(),
                    );
                    if !fit {
                        break;
                    }
                }
                ok_reply(unique, out)
            }

            Opcode::Releasedir => {
                let rel = try_parse!(wire::ReleaseIn::from_bytes(body));
                self.readdir_cache.lock().unwrap().remove(&rel.fh);
                try_fs!(self.fs.releasedir(rel.fh));
                ok_reply(unique, Vec::new())
            }

            Opcode::Fsyncdir => ok_reply(unique, Vec::new()),

            Opcode::Statfs => {
                let stat = try_fs!(self.fs.statfs(nodeid));
                ok_reply(unique, wire::statfs_out(&stat))
            }

            Opcode::Access => {
                let _ = try_parse!(wire::AccessIn::from_bytes(body));
                // No real permission check yet (core doesn't implement
                // one) -- mount with `-o default_permissions` so the
                // guest kernel enforces permissions from `getattr`
                // instead of relying on us here.
                ok_reply(unique, Vec::new())
            }

            Opcode::Getxattr | Opcode::Listxattr | Opcode::Setxattr | Opcode::Removexattr => {
                Reply::Bytes(OutHeader::error_for(unique, libc::ENOSYS))
            }

            Opcode::Unknown(op) => {
                log::debug!("unhandled FUSE opcode {op}, replying ENOSYS");
                Reply::Bytes(OutHeader::error_for(unique, libc::ENOSYS))
            }
        }
    }
}

fn trim_nul(buf: &[u8]) -> &[u8] {
    match buf.iter().position(|&b| b == 0) {
        Some(i) => &buf[..i],
        None => buf,
    }
}

/// Split a buffer containing two consecutive NUL-terminated strings.
fn split_two_cstrs(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = buf;
    let mut n = 0;
    std::iter::from_fn(move || {
        if n >= 2 {
            return None;
        }
        n += 1;
        let end = rest.iter().position(|&b| b == 0)?;
        let s = &rest[..end];
        rest = &rest[end + 1..];
        Some(s)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use riftlessfs_core::ROOT_ID;

    fn session_in_tempdir() -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path()).unwrap();
        (dir, Session::new(fs))
    }

    fn request(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
        let mut w = super::super::bytes::Writer::new();
        w.u32((wire::IN_HEADER_LEN + body.len()) as u32);
        w.u32(opcode);
        w.u64(unique);
        w.u64(nodeid);
        w.u32(0); // uid
        w.u32(0); // gid
        w.u32(0); // pid
        w.u32(0); // total_extlen + padding
        w.bytes(body);
        w.into_vec()
    }

    fn unwrap_ok(reply: Reply) -> Vec<u8> {
        match reply {
            Reply::Bytes(b) => {
                let err = i32::from_le_bytes(b[4..8].try_into().unwrap());
                assert_eq!(err, 0, "expected success reply, got errno {err}");
                b[wire::OUT_HEADER_LEN..].to_vec()
            }
            Reply::None => panic!("expected a reply"),
        }
    }

    #[test]
    fn init_negotiates_version() {
        let (_dir, session) = session_in_tempdir();
        let mut body = super::super::bytes::Writer::new();
        body.u32(7).u32(45).u32(0).u32(0);
        let req = request(26 /* INIT */, 1, 0, &body.into_vec());
        let reply = unwrap_ok(session.handle(&req));
        assert_eq!(reply.len(), wire::INIT_OUT_LEN);
        assert_eq!(u32::from_le_bytes(reply[0..4].try_into().unwrap()), 7);
        assert_eq!(
            u32::from_le_bytes(reply[4..8].try_into().unwrap()),
            wire::INIT_OUT_MINOR
        );
    }

    #[test]
    fn create_write_read_via_dispatch() {
        let (dir, session) = session_in_tempdir();

        let mut create_body = super::super::bytes::Writer::new();
        create_body
            .u32(libc::O_RDWR as u32)
            .u32(0o644)
            .u32(0)
            .u32(0);
        create_body.cstr(b"f.txt");
        let req = request(35 /* CREATE */, 1, ROOT_ID, &create_body.into_vec());
        let reply = unwrap_ok(session.handle(&req));
        assert_eq!(reply.len(), wire::ENTRY_OUT_LEN + 16);
        let fh = u64::from_le_bytes(
            reply[wire::ENTRY_OUT_LEN..wire::ENTRY_OUT_LEN + 8]
                .try_into()
                .unwrap(),
        );

        let mut write_body = super::super::bytes::Writer::new();
        write_body.u64(fh).u64(0).u32(5).u32(0).u64(0).u32(0).u32(0);
        write_body.bytes(b"hello");
        let req = request(16 /* WRITE */, 2, 0, &write_body.into_vec());
        let reply = unwrap_ok(session.handle(&req));
        assert_eq!(u32::from_le_bytes(reply[0..4].try_into().unwrap()), 5);

        assert_eq!(std::fs::read(dir.path().join("f.txt")).unwrap(), b"hello");

        let mut read_body = super::super::bytes::Writer::new();
        read_body
            .u64(fh)
            .u64(0)
            .u32(4096)
            .u32(0)
            .u64(0)
            .u32(0)
            .u32(0);
        let req = request(15 /* READ */, 3, 0, &read_body.into_vec());
        let reply = unwrap_ok(session.handle(&req));
        assert_eq!(reply, b"hello");
    }

    #[test]
    fn forget_gets_no_reply() {
        let (_dir, session) = session_in_tempdir();
        let mut body = super::super::bytes::Writer::new();
        body.u64(1);
        let req = request(2 /* FORGET */, 1, ROOT_ID, &body.into_vec());
        assert!(matches!(session.handle(&req), Reply::None));
    }

    #[test]
    fn unknown_opcode_is_enosys_not_a_panic() {
        let (_dir, session) = session_in_tempdir();
        let req = request(9999, 1, ROOT_ID, &[]);
        match session.handle(&req) {
            Reply::Bytes(b) => {
                let err = i32::from_le_bytes(b[4..8].try_into().unwrap());
                // -38, Linux's ENOSYS, on the wire -- *not* this host's
                // own ENOSYS value (see linux_errno module docs for why
                // that distinction matters).
                assert_eq!(err, -38);
            }
            Reply::None => panic!("expected an ENOSYS reply"),
        }
    }

    #[test]
    fn readdir_lists_created_entries() {
        let (_dir, session) = session_in_tempdir();

        for name in ["a", "b", "c"] {
            let mut body = super::super::bytes::Writer::new();
            body.u32(libc::O_RDWR as u32).u32(0o644).u32(0).u32(0);
            body.cstr(name.as_bytes());
            let req = request(35, 1, ROOT_ID, &body.into_vec());
            unwrap_ok(session.handle(&req));
        }

        let mut open_body = super::super::bytes::Writer::new();
        open_body.u32(0).u32(0);
        let req = request(27 /* OPENDIR */, 2, ROOT_ID, &open_body.into_vec());
        let reply = unwrap_ok(session.handle(&req));
        let dh = u64::from_le_bytes(reply[0..8].try_into().unwrap());

        let mut readdir_body = super::super::bytes::Writer::new();
        readdir_body
            .u64(dh)
            .u64(0)
            .u32(4096)
            .u32(0)
            .u64(0)
            .u32(0)
            .u32(0);
        let req = request(28 /* READDIR */, 3, ROOT_ID, &readdir_body.into_vec());
        let reply = unwrap_ok(session.handle(&req));

        let mut names = Vec::new();
        let mut off = 0;
        while off < reply.len() {
            let namelen =
                u32::from_le_bytes(reply[off + 16..off + 20].try_into().unwrap()) as usize;
            let name = &reply[off + 24..off + 24 + namelen];
            names.push(String::from_utf8_lossy(name).into_owned());
            let padded = (24 + namelen).div_ceil(8) * 8;
            off += padded;
        }
        names.sort();
        assert_eq!(names, vec!["a", "b", "c"]);
    }
}
