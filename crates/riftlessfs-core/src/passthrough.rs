//! The passthrough filesystem engine itself: a transport-agnostic set of
//! FUSE/virtio-fs-shaped operations (`lookup`, `getattr`, `read`, `write`,
//! `readdir`, ...) implemented in terms of real syscalls against the shared
//! directory, using fd-relative (`*at()`) operations throughout so that
//! concurrent renames elsewhere on the host can't be used to escape the
//! shared root (a class of bugs that plain path-string passthrough
//! filesystems are prone to).

use std::ffi::{CString, OsStr, OsString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::sync::Arc;

use crate::attr::{Attr, SetAttr};
use crate::error::{FsError, FsResult};
use crate::handle::{HandleData, HandleStore};
use crate::inode::{self, FileKind, InodeData, InodeStore, ROOT_ID};

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: OsString,
    pub ino: u64,
    pub kind: FileKind,
    pub next_offset: i64,
}

pub struct PassthroughFs {
    inodes: InodeStore,
    handles: HandleStore,
}

fn to_cstring(name: &OsStr) -> FsResult<CString> {
    CString::new(name.as_bytes()).map_err(|_| FsError::InvalidArgument("name contains NUL"))
}

fn check_errno(rc: libc::c_int) -> FsResult<()> {
    if rc < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

impl PassthroughFs {
    pub fn new(shared_dir: &Path) -> FsResult<Self> {
        let path = CString::new(shared_dir.as_os_str().as_bytes())
            .map_err(|_| FsError::InvalidArgument("path contains NUL"))?;
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        check_errno(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
        Ok(PassthroughFs {
            inodes: InodeStore::new(fd, &st),
            handles: HandleStore::new(),
        })
    }

    pub fn inode_count(&self) -> usize {
        self.inodes.len()
    }

    pub fn handle_count(&self) -> usize {
        self.handles.len()
    }

    fn inode(&self, id: u64) -> FsResult<Arc<InodeData>> {
        self.inodes.get(id)
    }

    pub fn lookup(&self, parent: u64, name: &OsStr) -> FsResult<(u64, Attr)> {
        let parent_data = self.inode(parent)?;
        let cname = to_cstring(name)?;
        let (fd, st) = inode::open_child(parent_data.raw_fd(), &cname)?;
        let data = self.inodes.register(parent, name.to_os_string(), fd, &st);
        Ok((data.id, Attr::from_stat(&st)))
    }

    pub fn forget(&self, ino: u64, count: u64) {
        self.inodes.forget(ino, count);
    }

    pub fn getattr(&self, ino: u64) -> FsResult<Attr> {
        let data = self.inode(ino)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // fstat() is valid on the minimal-privilege reference fd on both
        // Linux (O_PATH) and macOS (O_EVTONLY/O_SYMLINK).
        check_errno(unsafe { libc::fstat(data.raw_fd(), &mut st) })?;
        Ok(Attr::from_stat(&st))
    }

    pub fn setattr(&self, ino: u64, attr: &SetAttr) -> FsResult<Attr> {
        let data = self.inode(ino)?;

        if let Some(size) = attr.size {
            // Reference fd can't be truncated directly (O_PATH/O_EVTONLY);
            // reopen with write access just for this call.
            let wfd = self.reopen_via_parent(ino, libc::O_WRONLY)?;
            let rc = unsafe { libc::ftruncate(wfd, size as libc::off_t) };
            let err = if rc < 0 {
                Some(std::io::Error::last_os_error())
            } else {
                None
            };
            unsafe { libc::close(wfd) };
            if let Some(e) = err {
                return Err(e.into());
            }
        }

        if attr.mode.is_some()
            || attr.uid.is_some()
            || attr.gid.is_some()
            || attr.atime.is_some()
            || attr.mtime.is_some()
        {
            let (parent_fd, name) = self.parent_fd_name(&data)?;

            if let Some(mode) = attr.mode {
                check_errno(unsafe {
                    libc::fchmodat(
                        parent_fd,
                        name.as_ptr(),
                        mode as libc::mode_t,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                })?;
            }
            if attr.uid.is_some() || attr.gid.is_some() {
                let uid = attr
                    .uid
                    .map(|u| u as libc::uid_t)
                    .unwrap_or(u32::MAX as libc::uid_t);
                let gid = attr
                    .gid
                    .map(|g| g as libc::gid_t)
                    .unwrap_or(u32::MAX as libc::gid_t);
                check_errno(unsafe {
                    libc::fchownat(
                        parent_fd,
                        name.as_ptr(),
                        uid,
                        gid,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                })?;
            }
            if attr.atime.is_some() || attr.mtime.is_some() {
                let to_ts = |t: Option<(i64, u32)>| match t {
                    Some((sec, nsec)) => libc::timespec {
                        tv_sec: sec as _,
                        tv_nsec: nsec as _,
                    },
                    None => libc::timespec {
                        tv_sec: 0,
                        tv_nsec: libc::UTIME_OMIT,
                    },
                };
                let times = [to_ts(attr.atime), to_ts(attr.mtime)];
                check_errno(unsafe {
                    libc::utimensat(
                        parent_fd,
                        name.as_ptr(),
                        times.as_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                })?;
            }
        }

        self.getattr(ino)
    }

    fn parent_fd_name(&self, data: &InodeData) -> FsResult<(RawFd, CString)> {
        let parent_id = data.parent().unwrap_or(ROOT_ID);
        let parent = self.inode(parent_id)?;
        let name = to_cstring(&data.name())?;
        Ok((parent.raw_fd(), name))
    }

    /// Open a *new* fd for `ino` with the exact `flags` requested (e.g.
    /// `O_RDWR`), for use when the cheap per-inode reference fd (opened
    /// with the platform's minimal-privilege flags -- `O_PATH` on Linux,
    /// `O_EVTONLY`/`O_SYMLINK` on macOS) isn't sufficient.
    ///
    /// Note this deliberately does *not* use the "reopen via
    /// /proc/self/fd or /dev/fd" trick: that only lets you *narrow* access
    /// on macOS (verified empirically -- see platform module docs), and on
    /// Linux it only works for `O_PATH` fds specifically. Reopening by
    /// walking back to `(parent dirfd, name)` works uniformly on both and
    /// is what this does instead, at the cost of one extra permission
    /// check + requiring the name to still be valid (which our rename
    /// bookkeeping keeps true for anything renamed through us).
    fn reopen_via_parent(&self, ino: u64, flags: libc::c_int) -> FsResult<RawFd> {
        let data = self.inode(ino)?;
        if ino == ROOT_ID {
            let fd = unsafe { libc::fcntl(data.raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            return Ok(fd);
        }
        let (parent_fd, name) = self.parent_fd_name(&data)?;
        let fd = unsafe { libc::openat(parent_fd, name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(fd)
    }

    pub fn mkdir(&self, parent: u64, name: &OsStr, mode: u32) -> FsResult<(u64, Attr)> {
        let parent_data = self.inode(parent)?;
        let cname = to_cstring(name)?;
        check_errno(unsafe {
            libc::mkdirat(parent_data.raw_fd(), cname.as_ptr(), mode as libc::mode_t)
        })?;
        self.lookup(parent, name)
    }

    pub fn create(
        &self,
        parent: u64,
        name: &OsStr,
        flags: i32,
        mode: u32,
    ) -> FsResult<(u64, u64, Attr)> {
        let parent_data = self.inode(parent)?;
        let cname = to_cstring(name)?;
        let fd = unsafe {
            libc::openat(
                parent_data.raw_fd(),
                cname.as_ptr(),
                flags | libc::O_CREAT | libc::O_EXCL,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // We now have a ready-to-use fd with the client's requested flags;
        // still go through lookup so the inode gets tracked, then hand back
        // the already-open fd as the file handle (saves a reopen).
        let (ino, attr) = self.lookup(parent, name)?;
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let handle = self.handles.insert(HandleData::File(owned));
        Ok((ino, handle, attr))
    }

    pub fn unlink(&self, parent: u64, name: &OsStr) -> FsResult<()> {
        let parent_data = self.inode(parent)?;
        let cname = to_cstring(name)?;
        check_errno(unsafe { libc::unlinkat(parent_data.raw_fd(), cname.as_ptr(), 0) })
    }

    pub fn rmdir(&self, parent: u64, name: &OsStr) -> FsResult<()> {
        let parent_data = self.inode(parent)?;
        let cname = to_cstring(name)?;
        check_errno(unsafe {
            libc::unlinkat(parent_data.raw_fd(), cname.as_ptr(), libc::AT_REMOVEDIR)
        })
    }

    pub fn rename(
        &self,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
    ) -> FsResult<()> {
        let parent_data = self.inode(parent)?;
        let new_parent_data = self.inode(new_parent)?;
        let cname = to_cstring(name)?;
        let cnew = to_cstring(new_name)?;
        check_errno(unsafe {
            libc::renameat(
                parent_data.raw_fd(),
                cname.as_ptr(),
                new_parent_data.raw_fd(),
                cnew.as_ptr(),
            )
        })?;
        self.inodes
            .note_rename(parent, name, new_parent, new_name.to_os_string());
        Ok(())
    }

    pub fn symlink(&self, parent: u64, name: &OsStr, target: &OsStr) -> FsResult<(u64, Attr)> {
        let parent_data = self.inode(parent)?;
        let cname = to_cstring(name)?;
        let ctarget = to_cstring(target)?;
        check_errno(unsafe {
            libc::symlinkat(ctarget.as_ptr(), parent_data.raw_fd(), cname.as_ptr())
        })?;
        self.lookup(parent, name)
    }

    pub fn readlink(&self, ino: u64) -> FsResult<OsString> {
        let data = self.inode(ino)?;
        let (parent_fd, name) = self.parent_fd_name(&data)?;
        let mut buf = vec![0u8; 4096];
        let rc = unsafe {
            libc::readlinkat(
                parent_fd,
                name.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        buf.truncate(rc as usize);
        Ok(OsString::from_vec(buf))
    }

    pub fn link(&self, ino: u64, new_parent: u64, new_name: &OsStr) -> FsResult<Attr> {
        let data = self.inode(ino)?;
        let new_parent_data = self.inode(new_parent)?;
        let cnew = to_cstring(new_name)?;
        let empty = c"";
        // AT_EMPTY_PATH + a Linux O_PATH fd lets us hardlink "this exact
        // fd" without knowing its current name; not portable to macOS, so
        // there we fall back to the tracked (parent, name).
        #[cfg(target_os = "linux")]
        let rc = unsafe {
            libc::linkat(
                data.raw_fd(),
                empty.as_ptr(),
                new_parent_data.raw_fd(),
                cnew.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        };
        #[cfg(not(target_os = "linux"))]
        let rc = {
            let (parent_fd, name) = self.parent_fd_name(&data)?;
            let _ = empty;
            unsafe {
                libc::linkat(
                    parent_fd,
                    name.as_ptr(),
                    new_parent_data.raw_fd(),
                    cnew.as_ptr(),
                    0,
                )
            }
        };
        check_errno(rc)?;
        self.getattr(ino)
    }

    pub fn open(&self, ino: u64, flags: i32) -> FsResult<u64> {
        let fd = self.reopen_via_parent(ino, flags)?;
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(self.handles.insert(HandleData::File(owned)))
    }

    pub fn read(&self, handle: u64, offset: u64, size: usize) -> FsResult<Vec<u8>> {
        self.handles.with_file(handle, |fd| {
            let mut buf = vec![0u8; size];
            let rc = unsafe {
                libc::pread(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    size,
                    offset as libc::off_t,
                )
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            buf.truncate(rc as usize);
            Ok(buf)
        })
    }

    pub fn write(&self, handle: u64, offset: u64, data: &[u8]) -> FsResult<usize> {
        self.handles.with_file(handle, |fd| {
            let rc = unsafe {
                libc::pwrite(
                    fd.as_raw_fd(),
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                    offset as libc::off_t,
                )
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(rc as usize)
        })
    }

    pub fn fsync(&self, handle: u64) -> FsResult<()> {
        self.handles.with_file(handle, |fd| {
            check_errno(unsafe { libc::fsync(fd.as_raw_fd()) })
        })
    }

    pub fn release(&self, handle: u64) -> FsResult<()> {
        self.handles.remove(handle)?;
        Ok(())
    }

    pub fn opendir(&self, ino: u64) -> FsResult<u64> {
        let fd = self.reopen_via_parent(ino, libc::O_RDONLY | libc::O_DIRECTORY)?;
        let dir = nix::dir::Dir::from_fd(fd).map_err(FsError::from)?;
        Ok(self
            .handles
            .insert(HandleData::Dir(std::sync::Mutex::new(dir))))
    }

    pub fn readdir(&self, handle: u64) -> FsResult<Vec<DirEntry>> {
        self.handles.with_dir(handle, |dir| {
            let mut out = Vec::new();
            for entry in dir.iter() {
                let entry = entry.map_err(FsError::from)?;
                let name = entry.file_name();
                let bytes = name.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                let kind = match entry.file_type() {
                    Some(nix::dir::Type::Directory) => FileKind::Dir,
                    Some(nix::dir::Type::Symlink) => FileKind::Symlink,
                    Some(nix::dir::Type::File) => FileKind::Regular,
                    _ => FileKind::Other,
                };
                out.push(DirEntry {
                    name: OsString::from_vec(bytes.to_vec()),
                    ino: entry.ino(),
                    kind,
                    next_offset: 0,
                });
            }
            Ok(out)
        })
    }

    pub fn releasedir(&self, handle: u64) -> FsResult<()> {
        self.handles.remove(handle)?;
        Ok(())
    }

    pub fn statfs(&self, ino: u64) -> FsResult<libc::statvfs> {
        let data = self.inode(ino)?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        check_errno(unsafe { libc::fstatvfs(data.raw_fd(), &mut st) })?;
        Ok(st)
    }
}
