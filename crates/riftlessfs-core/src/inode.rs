//! Inode table: maps opaque 64-bit inode ids (the identity clients see) to
//! an open reference file descriptor plus enough bookkeeping to reconstruct
//! a `(parent dirfd, name)` pair for the handful of syscalls that have no
//! `*at()`/fd-relative form.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::fd::{OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use crate::error::{FsError, FsResult};
use crate::platform;

pub const ROOT_ID: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Dir,
    Symlink,
    Regular,
    Other,
}

impl FileKind {
    // `libc::S_IF*` constants have platform-dependent widths (e.g. `u16`
    // on macOS vs already-`u32` on Linux/glibc), so `as u32` is meaningful
    // on some targets and a harmless no-op on others.
    #[allow(clippy::unnecessary_cast)]
    pub fn from_mode(mode: u32) -> Self {
        match mode & libc::S_IFMT as u32 {
            m if m == libc::S_IFDIR as u32 => FileKind::Dir,
            m if m == libc::S_IFLNK as u32 => FileKind::Symlink,
            m if m == libc::S_IFREG as u32 => FileKind::Regular,
            _ => FileKind::Other,
        }
    }
}

/// Uniquely identifies an underlying file, independent of path, so hard
/// links / repeated lookups of the same file collapse onto one inode id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileKey {
    dev: u64,
    ino: u64,
}

pub struct InodeData {
    pub id: u64,
    pub fd: OwnedFd,
    /// Cached file type from the last lookup/stat. Not yet consulted
    /// anywhere performance-sensitive (every op re-`fstat`s as needed), but
    /// kept around for upcoming readdirplus-style optimizations.
    #[allow(dead_code)]
    pub kind: FileKind,
    key: FileKey,
    parent: Mutex<Option<u64>>,
    name: Mutex<OsString>,
    lookups: AtomicU64,
}

impl InodeData {
    pub fn raw_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }

    pub fn parent(&self) -> Option<u64> {
        *self.parent.lock().unwrap()
    }

    pub fn name(&self) -> OsString {
        self.name.lock().unwrap().clone()
    }

    pub fn set_location(&self, parent: u64, name: OsString) {
        *self.parent.lock().unwrap() = Some(parent);
        *self.name.lock().unwrap() = name;
    }
}

pub struct InodeStore {
    by_id: RwLock<HashMap<u64, std::sync::Arc<InodeData>>>,
    by_key: RwLock<HashMap<FileKey, u64>>,
    next_id: AtomicU64,
}

impl InodeStore {
    // `st_dev`/`st_mode` widths vary by platform (e.g. `dev_t` is `i32` on
    // macOS but already `u64` on Linux/glibc), so these casts are
    // meaningful on some targets and a harmless no-op on others.
    #[allow(clippy::unnecessary_cast)]
    pub fn new(root_fd: OwnedFd, root_stat: &libc::stat) -> Self {
        let root = std::sync::Arc::new(InodeData {
            id: ROOT_ID,
            fd: root_fd,
            kind: FileKind::Dir,
            key: FileKey {
                dev: root_stat.st_dev as u64,
                ino: root_stat.st_ino,
            },
            parent: Mutex::new(None),
            name: Mutex::new(OsString::new()),
            lookups: AtomicU64::new(1),
        });
        let mut by_id = HashMap::new();
        let mut by_key = HashMap::new();
        by_key.insert(root.key, ROOT_ID);
        by_id.insert(ROOT_ID, root);
        InodeStore {
            by_id: RwLock::new(by_id),
            by_key: RwLock::new(by_key),
            next_id: AtomicU64::new(ROOT_ID + 1),
        }
    }

    pub fn get(&self, id: u64) -> FsResult<std::sync::Arc<InodeData>> {
        self.by_id
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(FsError::UnknownInode(id))
    }

    /// Register a freshly-opened child (or dedup onto an existing inode if
    /// this file is already known, e.g. via a hard link or a previous
    /// lookup), bumping its lookup refcount by one either way.
    #[allow(clippy::unnecessary_cast)]
    pub fn register(
        &self,
        parent: u64,
        name: OsString,
        fd: OwnedFd,
        st: &libc::stat,
    ) -> std::sync::Arc<InodeData> {
        let key = FileKey {
            dev: st.st_dev as u64,
            ino: st.st_ino,
        };

        if let Some(&existing) = self.by_key.read().unwrap().get(&key) {
            let data = self.by_id.read().unwrap().get(&existing).unwrap().clone();
            data.lookups.fetch_add(1, Ordering::SeqCst);
            data.set_location(parent, name);
            // We already have a reference handle open; drop the duplicate.
            drop(fd);
            return data;
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let data = std::sync::Arc::new(InodeData {
            id,
            fd,
            kind: FileKind::from_mode(st.st_mode as u32),
            key,
            parent: Mutex::new(Some(parent)),
            name: Mutex::new(name),
            lookups: AtomicU64::new(1),
        });
        self.by_id.write().unwrap().insert(id, data.clone());
        self.by_key.write().unwrap().insert(key, id);
        data
    }

    /// FUSE-style forget: drop `count` references; once the refcount hits
    /// zero, the inode is evicted and its reference fd closed.
    pub fn forget(&self, id: u64, count: u64) {
        if id == ROOT_ID {
            return;
        }
        let should_evict = {
            let by_id = self.by_id.read().unwrap();
            match by_id.get(&id) {
                Some(data) => {
                    let prev = data.lookups.fetch_sub(
                        count.min(data.lookups.load(Ordering::SeqCst)),
                        Ordering::SeqCst,
                    );
                    prev <= count
                }
                None => false,
            }
        };
        if should_evict {
            let mut by_id = self.by_id.write().unwrap();
            if let Some(data) = by_id.remove(&id) {
                self.by_key.write().unwrap().remove(&data.key);
            }
        }
    }

    /// Called after a successful rename so a currently-tracked inode's
    /// cached `(parent, name)` stays in sync with reality.
    pub fn note_rename(
        &self,
        old_parent: u64,
        old_name: &std::ffi::OsStr,
        new_parent: u64,
        new_name: OsString,
    ) {
        let by_id = self.by_id.read().unwrap();
        for data in by_id.values() {
            if data.parent() == Some(old_parent) && data.name() == old_name {
                data.set_location(new_parent, new_name);
                return;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.by_id.read().unwrap().len()
    }
}

/// Open a child of `parent_fd` and stat it, choosing the minimal-privilege
/// open flags appropriate for its type (see `platform` module docs).
#[allow(clippy::unnecessary_cast)]
pub fn open_child(parent_fd: RawFd, name: &std::ffi::CStr) -> FsResult<(OwnedFd, libc::stat)> {
    use std::os::fd::FromRawFd;

    // First stat (without following symlinks) to learn the type.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(parent_fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let kind = FileKind::from_mode(st.st_mode as u32);
    let flags = platform::reference_open_flags(kind == FileKind::Dir, kind == FileKind::Symlink);
    let fd = unsafe { libc::openat(parent_fd, name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok((fd, st))
}
