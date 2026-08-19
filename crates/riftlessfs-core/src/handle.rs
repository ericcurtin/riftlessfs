//! Open file/directory handle table. Separate from the inode table because
//! a client may `open()` the same inode multiple times with different
//! flags (e.g. one reader + one writer), each needing its own fd, offset,
//! and (for directories) iteration state.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use crate::error::{FsError, FsResult};

pub enum HandleData {
    File(OwnedFd),
    Dir(Mutex<nix::dir::Dir>),
}

pub struct HandleStore {
    handles: RwLock<HashMap<u64, HandleData>>,
    next_id: AtomicU64,
}

impl Default for HandleStore {
    fn default() -> Self {
        Self::new()
    }
}

impl HandleStore {
    pub fn new() -> Self {
        HandleStore {
            handles: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn insert(&self, data: HandleData) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.handles.write().unwrap().insert(id, data);
        id
    }

    pub fn remove(&self, id: u64) -> FsResult<HandleData> {
        self.handles
            .write()
            .unwrap()
            .remove(&id)
            .ok_or(FsError::UnknownHandle(id))
    }

    pub fn with_file<R>(&self, id: u64, f: impl FnOnce(&OwnedFd) -> FsResult<R>) -> FsResult<R> {
        let handles = self.handles.read().unwrap();
        match handles.get(&id) {
            Some(HandleData::File(fd)) => f(fd),
            Some(HandleData::Dir(_)) => Err(FsError::InvalidArgument(
                "handle is a directory, not a file",
            )),
            None => Err(FsError::UnknownHandle(id)),
        }
    }

    pub fn with_dir<R>(
        &self,
        id: u64,
        f: impl FnOnce(&mut nix::dir::Dir) -> FsResult<R>,
    ) -> FsResult<R> {
        let handles = self.handles.read().unwrap();
        match handles.get(&id) {
            Some(HandleData::Dir(dir)) => f(&mut dir.lock().unwrap()),
            Some(HandleData::File(_)) => Err(FsError::InvalidArgument(
                "handle is a file, not a directory",
            )),
            None => Err(FsError::UnknownHandle(id)),
        }
    }

    pub fn len(&self) -> usize {
        self.handles.read().unwrap().len()
    }
}
