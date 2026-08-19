//! Transport-agnostic file attribute representation.
//!
//! This mirrors the fields FUSE/virtio-fs `getattr`/`setattr` care about,
//! decoupled from `libc::stat` so callers don't need to reason about
//! per-platform field width/signedness differences (e.g. `st_atime` is
//! `i64` on some platforms and a struct with nanosecond fields on others).

// POSIX `st_mode` file-type bits, defined locally (rather than pulled from
// `libc::S_IF*`) because the `libc` crate doesn't define all of these for
// the `windows` target (notably `S_IFLNK`), and this crate needs to at
// least *compile* everywhere even though [`crate::PassthroughFs`] itself is
// `cfg(unix)`-only for now.
const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFREG: u32 = 0o100_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct Attr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: (i64, u32),
    pub mtime: (i64, u32),
    pub ctime: (i64, u32),
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
}

impl Attr {
    #[cfg(unix)]
    // `libc::stat` field widths vary by platform (e.g. `st_mode` is `u16`
    // on macOS but already `u32` on Linux/glibc), so these casts are
    // meaningful on some targets and genuinely redundant (but harmless) on
    // others -- not the "same type" bug clippy's lint is meant to catch.
    #[allow(clippy::unnecessary_cast)]
    pub(crate) fn from_stat(st: &libc::stat) -> Self {
        Attr {
            ino: st.st_ino,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
            atime: (st.st_atime, st.st_atime_nsec as u32),
            mtime: (st.st_mtime, st.st_mtime_nsec as u32),
            ctime: (st.st_ctime, st.st_ctime_nsec as u32),
            mode: st.st_mode as u32,
            nlink: st.st_nlink as u32,
            uid: st.st_uid,
            gid: st.st_gid,
            rdev: st.st_rdev as u32,
            blksize: st.st_blksize as u32,
        }
    }

    pub fn is_dir(&self) -> bool {
        (self.mode & S_IFMT) == S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        (self.mode & S_IFMT) == S_IFLNK
    }

    pub fn is_regular(&self) -> bool {
        (self.mode & S_IFMT) == S_IFREG
    }
}

/// Subset of attributes a client may request to change via `setattr`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SetAttr {
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub atime: Option<(i64, u32)>,
    pub mtime: Option<(i64, u32)>,
}
