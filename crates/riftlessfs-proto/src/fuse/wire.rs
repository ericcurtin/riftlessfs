//! FUSE-over-virtio wire structs, matching the kernel ABI in
//! `include/uapi/linux/fuse.h` (checked against the current upstream
//! header, protocol 7.45, on 2026-08-20). Only the opcodes riftlessfsd
//! actually handles get a typed payload; everything else is decoded far
//! enough to reply with `-ENOSYS` safely (see `fuse::dispatch`).
//!
//! We deliberately negotiate a conservative protocol minor version (see
//! [`INIT_OUT_MINOR`]) with none of the optional feature flags set, so we
//! only ever need to correctly implement the well-established core of the
//! protocol -- the kernel negotiates down to `min(our minor, its minor)`
//! and gates every newer field/behavior behind either a higher minor or an
//! init flag we don't advertise.

use crate::error::ProtoResult;
use crate::fuse::bytes::{Reader, Writer};
use riftlessfs_core::Attr;

pub const IN_HEADER_LEN: usize = 40;
pub const OUT_HEADER_LEN: usize = 16;

/// The minor protocol version we claim in our `FUSE_INIT` reply. Chosen to
/// be well past the ancient "compat" struct sizes (so we always send the
/// same, full `fuse_init_out`) but conservative on features: we advertise
/// no optional flags (no writeback cache, no readdirplus, no posix ACLs,
/// ...), so none of the minor-gated behavior newer than this actually
/// matters.
pub const INIT_OUT_MINOR: u32 = 31;
pub const INIT_OUT_LEN: usize = 64;

#[derive(Debug, Clone, Copy)]
pub struct InHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

impl InHeader {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let mut r = Reader::new(buf);
        let len = r.u32()?;
        let opcode = r.u32()?;
        let unique = r.u64()?;
        let nodeid = r.u64()?;
        let uid = r.u32()?;
        let gid = r.u32()?;
        let pid = r.u32()?;
        // total_extlen (u16) + padding (u16): unused, we never advertise
        // FUSE_SECURITY_CTX so the kernel should never populate this.
        r.skip(4)?;
        Ok(InHeader {
            len,
            opcode,
            unique,
            nodeid,
            uid,
            gid,
            pid,
        })
    }

    /// The request body, i.e. everything in the gathered readable buffer
    /// after this fixed-size header.
    pub fn body(buf: &[u8]) -> &[u8] {
        &buf[IN_HEADER_LEN.min(buf.len())..]
    }
}

pub struct OutHeader;

impl OutHeader {
    /// Build a full reply (header + body) for `unique`. `error` should be
    /// 0 for success or a *negative* errno on failure (matching what the
    /// kernel expects on the wire); on error, `body` is ignored and an
    /// empty payload is sent, matching standard FUSE server behavior.
    pub fn reply(unique: u64, error: i32, body: &[u8]) -> Vec<u8> {
        let body = if error == 0 { body } else { &[] };
        let mut w = Writer::new();
        w.u32((OUT_HEADER_LEN + body.len()) as u32);
        w.i32(error);
        w.u64(unique);
        w.bytes(body);
        w.into_vec()
    }

    /// Build an error reply for `unique` from a **host** errno (e.g. from
    /// [`riftlessfs_core::FsError::errno`] or a `libc::E*` constant).
    /// Translates to the Linux errno the wire protocol requires --
    /// callers should never write a raw host errno onto the wire
    /// themselves; see [`crate::fuse::linux_errno`] for why that's
    /// dangerous on non-Linux hosts.
    pub fn error_for(unique: u64, host_errno: i32) -> Vec<u8> {
        Self::reply(
            unique,
            -crate::fuse::linux_errno::to_linux_errno(host_errno),
            &[],
        )
    }
}

/// Opcodes riftlessfsd has specific handling for. Anything else observed
/// on the wire is treated as [`Opcode::Unknown`] and answered with
/// `-ENOSYS`, rather than panicking on an opcode from a newer kernel we
/// don't yet understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Lookup,
    Forget,
    Getattr,
    Setattr,
    Readlink,
    Symlink,
    Mkdir,
    Unlink,
    Rmdir,
    Rename,
    Link,
    Open,
    Read,
    Write,
    Statfs,
    Release,
    Fsync,
    Getxattr,
    Listxattr,
    Setxattr,
    Removexattr,
    Flush,
    Init,
    Opendir,
    Readdir,
    Releasedir,
    Fsyncdir,
    Access,
    Create,
    Destroy,
    BatchForget,
    Rename2,
    Unknown(u32),
}

impl From<u32> for Opcode {
    fn from(v: u32) -> Self {
        match v {
            1 => Opcode::Lookup,
            2 => Opcode::Forget,
            3 => Opcode::Getattr,
            4 => Opcode::Setattr,
            5 => Opcode::Readlink,
            6 => Opcode::Symlink,
            9 => Opcode::Mkdir,
            10 => Opcode::Unlink,
            11 => Opcode::Rmdir,
            12 => Opcode::Rename,
            13 => Opcode::Link,
            14 => Opcode::Open,
            15 => Opcode::Read,
            16 => Opcode::Write,
            17 => Opcode::Statfs,
            18 => Opcode::Release,
            20 => Opcode::Fsync,
            21 => Opcode::Setxattr,
            22 => Opcode::Getxattr,
            23 => Opcode::Listxattr,
            24 => Opcode::Removexattr,
            25 => Opcode::Flush,
            26 => Opcode::Init,
            27 => Opcode::Opendir,
            28 => Opcode::Readdir,
            29 => Opcode::Releasedir,
            30 => Opcode::Fsyncdir,
            34 => Opcode::Access,
            35 => Opcode::Create,
            38 => Opcode::Destroy,
            42 => Opcode::BatchForget,
            45 => Opcode::Rename2,
            other => Opcode::Unknown(other),
        }
    }
}

/// `struct fuse_attr` (88 bytes).
pub struct FuseAttr;

impl FuseAttr {
    pub fn write(w: &mut Writer, attr: &Attr) {
        w.u64(attr.ino);
        w.u64(attr.size);
        w.u64(attr.blocks);
        w.u64(attr.atime.0 as u64);
        w.u64(attr.mtime.0 as u64);
        w.u64(attr.ctime.0 as u64);
        w.u32(attr.atime.1);
        w.u32(attr.mtime.1);
        w.u32(attr.ctime.1);
        w.u32(attr.mode);
        w.u32(attr.nlink);
        w.u32(attr.uid);
        w.u32(attr.gid);
        w.u32(attr.rdev);
        w.u32(attr.blksize);
        w.u32(0); // flags: FUSE_ATTR_SUBMOUNT/DAX, unused
    }
}

pub const ATTR_LEN: usize = 88;
pub const ENTRY_OUT_LEN: usize = 128;
pub const ATTR_OUT_LEN: usize = 104;

/// A fixed cache timeout for attribute/entry validity, applied uniformly
/// (no distinction between recently-modified-by-us vs. untouched
/// entries).
///
/// A real measurement motivated picking a nonzero value here: with this
/// at 0, a synthetic "stat 2000 files" benchmark took ~2s against
/// riftlessfsd vs. ~3ms against OrbStack, because every single `stat()`
/// call became a synchronous round trip through the whole vhost-user/FUSE
/// pipeline instead of being served from the guest kernel's own cache.
/// One second is a common, conservative default (used by e.g. many
/// FUSE-based network filesystems) -- it means a change made directly on
/// the host (bypassing riftlessfsd) can take up to this long to become
/// visible in the guest, which is an ordinary, expected FUSE caching
/// trade-off, not a correctness bug. There's no active cache
/// invalidation yet (e.g. on rename/unlink of an entry another client
/// might have cached), which matters more once multiple guests or
/// host-side writers are involved -- tracked as follow-up work.
const CACHE_TIMEOUT_SECS: u64 = 1;

/// `struct fuse_entry_out`: nodeid, generation, entry/attr cache timeouts,
/// then a `fuse_attr`.
pub fn entry_out(nodeid: u64, attr: &Attr) -> Vec<u8> {
    let mut w = Writer::new();
    w.u64(nodeid); // nodeid
    w.u64(1); // generation
    w.u64(CACHE_TIMEOUT_SECS); // entry_valid
    w.u64(CACHE_TIMEOUT_SECS); // attr_valid
    w.u32(0); // entry_valid_nsec
    w.u32(0); // attr_valid_nsec
    FuseAttr::write(&mut w, attr);
    debug_assert_eq!(w.len(), ENTRY_OUT_LEN);
    w.into_vec()
}

/// `struct fuse_attr_out`: attr cache timeout, then a `fuse_attr`.
pub fn attr_out(attr: &Attr) -> Vec<u8> {
    let mut w = Writer::new();
    w.u64(CACHE_TIMEOUT_SECS); // attr_valid
    w.u32(0); // attr_valid_nsec
    w.u32(0); // dummy
    FuseAttr::write(&mut w, attr);
    debug_assert_eq!(w.len(), ATTR_OUT_LEN);
    w.into_vec()
}

/// `struct fuse_init_in`'s fixed-size prefix (major, minor, max_readahead,
/// flags); we don't need anything past that.
pub struct InitIn {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
}

impl InitIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let mut r = Reader::new(buf);
        Ok(InitIn {
            major: r.u32()?,
            minor: r.u32()?,
            max_readahead: r.u32()?,
            flags: r.u32().unwrap_or(0),
        })
    }
}

/// Maximum single read/write size we advertise (and enforce) -- 128 KiB,
/// a conservative, widely-used value.
pub const MAX_WRITE: u32 = 128 * 1024;

/// Without this, the guest kernel doesn't coalesce dirty pages before
/// sending `WRITE` requests: every buffered `write()` syscall, regardless
/// of size, gets flushed as its own synchronous page-sized (4 KiB) FUSE
/// request. Measured impact (see BENCHMARKS.md): sequential 1 MiB writes
/// and random 4 KiB writes achieved almost identical MiB/s without this
/// flag -- the signature of every write being split into individual 4 KiB
/// round trips regardless of the application's actual request size.
///
/// This is a bigger behavioral change than the other flags here (the
/// kernel now owns dirty-page/size coherency until writeback), so it's
/// worth being explicit about what was checked before enabling it:
/// `riftlessfs-core`'s own operations don't buffer writes themselves
/// (every `WRITE` request we do receive is immediately `pwrite()`'d to
/// the real file), so from our side there's no new buffering to get
/// wrong; the risk is entirely in trusting the kernel's writeback
/// behavior on the other side of the wire. Verified after enabling: the
/// full `cargo test --workspace` suite still passes, and
/// `scripts/qemu-integration-test.sh`'s 8 MiB file copy still produces a
/// matching `sha256sum` on both sides of a real mount.
const FUSE_WRITEBACK_CACHE: u32 = 1 << 16;

/// `struct fuse_init_out`, sized and populated per [`INIT_OUT_MINOR`] and
/// the flags above.
pub fn init_out(max_readahead: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(7); // major
    w.u32(INIT_OUT_MINOR);
    w.u32(max_readahead);
    w.u32(FUSE_WRITEBACK_CACHE);
    w.u16(0); // max_background
    w.u16(0); // congestion_threshold
    w.u32(MAX_WRITE);
    w.u32(1); // time_gran: 1ns (we report real nanosecond timestamps)
    w.u16(0); // max_pages (feature not advertised, kernel ignores)
    w.u16(0); // map_alignment
    w.u32(0); // flags2
    w.u32(0); // max_stack_depth
    w.u16(0); // request_timeout
    w.pad_to(INIT_OUT_LEN);
    debug_assert_eq!(w.len(), INIT_OUT_LEN);
    w.into_vec()
}

pub struct GetattrIn {
    #[allow(dead_code)]
    pub flags: u32,
    pub fh: u64,
}

impl GetattrIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let mut r = Reader::new(buf);
        let flags = r.u32()?;
        r.skip(4)?; // dummy
        let fh = r.u64()?;
        Ok(GetattrIn { flags, fh })
    }
}

pub const FATTR_MODE: u32 = 1 << 0;
pub const FATTR_UID: u32 = 1 << 1;
pub const FATTR_GID: u32 = 1 << 2;
pub const FATTR_SIZE: u32 = 1 << 3;
pub const FATTR_ATIME: u32 = 1 << 4;
pub const FATTR_MTIME: u32 = 1 << 5;
pub const FATTR_ATIME_NOW: u32 = 1 << 7;
pub const FATTR_MTIME_NOW: u32 = 1 << 8;

pub struct SetattrIn {
    pub valid: u32,
    pub size: u64,
    pub atime: u64,
    pub mtime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl SetattrIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let mut r = Reader::new(buf);
        let valid = r.u32()?;
        r.skip(4)?; // padding
        r.skip(8)?; // fh (unused: we always go by (parent, name))
        let size = r.u64()?;
        r.skip(8)?; // lock_owner
        let atime = r.u64()?;
        let mtime = r.u64()?;
        r.skip(8)?; // ctime
        let atimensec = r.u32()?;
        let mtimensec = r.u32()?;
        r.skip(4)?; // ctimensec
        let mode = r.u32()?;
        r.skip(4)?; // unused4
        let uid = r.u32()?;
        let gid = r.u32()?;
        Ok(SetattrIn {
            valid,
            size,
            atime,
            mtime,
            atimensec,
            mtimensec,
            mode,
            uid,
            gid,
        })
    }
}

pub struct MkdirIn {
    pub mode: u32,
}

impl MkdirIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<(Self, &[u8])> {
        let mut r = Reader::new(buf);
        let mode = r.u32()?;
        r.skip(4)?; // umask
        let name = r.cstr()?;
        Ok((MkdirIn { mode }, name))
    }
}

pub struct RenameIn {
    pub newdir: u64,
}

impl RenameIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<(Self, &[u8], &[u8])> {
        let mut r = Reader::new(buf);
        let newdir = r.u64()?;
        let old = r.cstr()?;
        let new = r.cstr()?;
        Ok((RenameIn { newdir }, old, new))
    }
}

/// `FUSE_RENAME2`'s payload is `fuse_rename2_in` (newdir + flags + padding)
/// followed by the same two names; we ignore the flags (no
/// `RENAME_EXCHANGE`/`RENAME_NOREPLACE` support yet).
impl RenameIn {
    pub fn from_bytes_v2(buf: &[u8]) -> ProtoResult<(Self, &[u8], &[u8])> {
        let mut r = Reader::new(buf);
        let newdir = r.u64()?;
        r.skip(8)?; // flags + padding
        let old = r.cstr()?;
        let new = r.cstr()?;
        Ok((RenameIn { newdir }, old, new))
    }
}

pub struct LinkIn {
    pub oldnodeid: u64,
}

impl LinkIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<(Self, &[u8])> {
        let mut r = Reader::new(buf);
        let oldnodeid = r.u64()?;
        let name = r.cstr()?;
        Ok((LinkIn { oldnodeid }, name))
    }
}

pub struct OpenIn {
    pub flags: u32,
}

impl OpenIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let mut r = Reader::new(buf);
        Ok(OpenIn { flags: r.u32()? })
    }
}

pub struct CreateIn {
    pub flags: u32,
    pub mode: u32,
}

impl CreateIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<(Self, &[u8])> {
        let mut r = Reader::new(buf);
        let flags = r.u32()?;
        let mode = r.u32()?;
        r.skip(4)?; // umask
        r.skip(4)?; // open_flags
        let name = r.cstr()?;
        Ok((CreateIn { flags, mode }, name))
    }
}

pub fn open_out(fh: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.u64(fh);
    w.u32(0); // open_flags
    w.i32(0); // backing_id
    w.into_vec()
}

pub struct ReleaseIn {
    pub fh: u64,
}

impl ReleaseIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let mut r = Reader::new(buf);
        Ok(ReleaseIn { fh: r.u64()? })
    }
}

pub struct FlushIn {
    pub fh: u64,
}

impl FlushIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let mut r = Reader::new(buf);
        Ok(FlushIn { fh: r.u64()? })
    }
}

pub struct ReadIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
}

impl ReadIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let mut r = Reader::new(buf);
        let fh = r.u64()?;
        let offset = r.u64()?;
        let size = r.u32()?;
        Ok(ReadIn { fh, offset, size })
    }
}

pub struct WriteIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
}

impl WriteIn {
    /// Returns the parsed header and the data to write (whatever's left in
    /// the readable buffer past the fixed 40-byte `fuse_write_in`).
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<(Self, &[u8])> {
        let mut r = Reader::new(buf);
        let fh = r.u64()?;
        let offset = r.u64()?;
        let size = r.u32()?;
        r.skip(4)?; // write_flags
        r.skip(8)?; // lock_owner
        r.skip(4)?; // flags
        r.skip(4)?; // padding
        Ok((WriteIn { fh, offset, size }, r.remaining()))
    }
}

pub fn write_out(size: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(size);
    w.u32(0);
    w.into_vec()
}

// `libc::statvfs` field widths vary by platform (e.g. these are `u32` on
// macOS but already `u64`/wider on Linux/glibc), so these casts are
// meaningful on some targets and a harmless no-op on others.
#[allow(clippy::unnecessary_cast)]
pub fn statfs_out(stat: &libc::statvfs) -> Vec<u8> {
    let mut w = Writer::new();
    w.u64(stat.f_blocks as u64);
    w.u64(stat.f_bfree as u64);
    w.u64(stat.f_bavail as u64);
    w.u64(stat.f_files as u64);
    w.u64(stat.f_ffree as u64);
    w.u32(stat.f_bsize as u32);
    w.u32(stat.f_namemax as u32);
    w.u32(stat.f_frsize as u32);
    w.u32(0); // padding
    for _ in 0..6 {
        w.u32(0); // spare
    }
    w.into_vec()
}

pub struct AccessIn {
    #[allow(dead_code)]
    pub mask: u32,
}

impl AccessIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let mut r = Reader::new(buf);
        Ok(AccessIn { mask: r.u32()? })
    }
}

/// One `fuse_forget_one` entry from a `FUSE_BATCH_FORGET` request.
pub struct ForgetOne {
    pub nodeid: u64,
    pub nlookup: u64,
}

pub fn parse_batch_forget(buf: &[u8]) -> ProtoResult<Vec<ForgetOne>> {
    let mut r = Reader::new(buf);
    let count = r.u32()?;
    r.skip(4)?; // dummy
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let nodeid = r.u64()?;
        let nlookup = r.u64()?;
        out.push(ForgetOne { nodeid, nlookup });
    }
    Ok(out)
}

pub struct ForgetIn {
    pub nlookup: u64,
}

impl ForgetIn {
    pub fn from_bytes(buf: &[u8]) -> ProtoResult<Self> {
        let mut r = Reader::new(buf);
        Ok(ForgetIn { nlookup: r.u64()? })
    }
}

/// Encode one `READDIR` entry (`struct fuse_dirent` + name, 8-byte
/// aligned) into `out`, returning `false` (and leaving `out` unchanged) if
/// it wouldn't fit in `remaining` bytes -- callers should stop adding
/// entries once this happens, matching how the kernel expects a `READDIR`
/// reply to be truncated to whatever fits in the buffer it offered.
pub fn push_dirent(
    out: &mut Vec<u8>,
    remaining: usize,
    ino: u64,
    off: u64,
    kind: u32,
    name: &[u8],
) -> bool {
    let unpadded = 24 + name.len();
    let padded = unpadded.div_ceil(8) * 8;
    if padded > remaining {
        return false;
    }
    out.extend_from_slice(&ino.to_le_bytes());
    out.extend_from_slice(&off.to_le_bytes());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(name);
    out.resize(out.len() + (padded - unpadded), 0);
    true
}

/// `DT_*` constants from `<dirent.h>`, used as `fuse_dirent.type`.
pub const DT_UNKNOWN: u32 = 0;
pub const DT_DIR: u32 = 4;
pub const DT_REG: u32 = 8;
pub const DT_LNK: u32 = 10;
