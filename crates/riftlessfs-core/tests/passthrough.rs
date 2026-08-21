//! End-to-end tests of the passthrough engine against a real temporary
//! directory on whatever filesystem the test runner uses (APFS on macOS,
//! ext4/btrfs/tmpfs on Linux CI). These exercise the actual syscalls, not
//! mocks.
//!
//! `riftlessfs_core::PassthroughFs` only exists on Unix (see
//! `PASSTHROUGH_SUPPORTED`), so this whole file compiles to nothing on
//! other platforms rather than failing to build there.
#![cfg(unix)]

use riftlessfs_core::{PassthroughFs, SetAttr, ROOT_ID};
use std::ffi::OsStr;

fn fs_in_tempdir() -> (tempfile::TempDir, PassthroughFs) {
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = tempfile::tempdir().expect("tempdir");
    let fs = PassthroughFs::new(dir.path()).expect("open shared dir");
    (dir, fs)
}

#[test]
fn root_getattr_is_a_directory() {
    let (_dir, fs) = fs_in_tempdir();
    let attr = fs.getattr(ROOT_ID).unwrap();
    assert!(attr.is_dir());
}

#[test]
fn create_write_read_roundtrip() {
    let (dir, fs) = fs_in_tempdir();
    let (ino, handle, attr) = fs
        .create(ROOT_ID, OsStr::new("hello.txt"), libc::O_RDWR, 0o644)
        .unwrap();
    assert!(attr.is_regular());

    let n = fs.write(handle, 0, b"hello, riftlessfs").unwrap();
    assert_eq!(n, 17);
    fs.fsync(handle).unwrap();

    let data = fs.read(handle, 0, 4096).unwrap();
    assert_eq!(&data, b"hello, riftlessfs");
    fs.release(handle).unwrap();

    // And confirm it's really on disk, independent of our own bookkeeping.
    let on_disk = std::fs::read(dir.path().join("hello.txt")).unwrap();
    assert_eq!(on_disk, b"hello, riftlessfs");

    let (looked_up_ino, attr2) = fs.lookup(ROOT_ID, OsStr::new("hello.txt")).unwrap();
    assert_eq!(looked_up_ino, ino);
    assert_eq!(attr2.size, 17);
}

#[test]
fn write_vectored_gathers_multiple_iovecs_into_one_write() {
    let (dir, fs) = fs_in_tempdir();
    let (_ino, handle, _) = fs
        .create(
            ROOT_ID,
            OsStr::new("vectored_write.txt"),
            libc::O_RDWR,
            0o644,
        )
        .unwrap();

    // Three separate buffers, as if they were three separate guest-memory
    // descriptors -- write_vectored should write them contiguously via a
    // single pwritev(), not require them to already be concatenated.
    let a = b"hello, ";
    let b = b"vectored ";
    let c = b"world";
    let iov = [
        libc::iovec {
            iov_base: a.as_ptr() as *mut libc::c_void,
            iov_len: a.len(),
        },
        libc::iovec {
            iov_base: b.as_ptr() as *mut libc::c_void,
            iov_len: b.len(),
        },
        libc::iovec {
            iov_base: c.as_ptr() as *mut libc::c_void,
            iov_len: c.len(),
        },
    ];
    let expected: Vec<u8> = a.iter().chain(b).chain(c).copied().collect();

    let n = fs.write_vectored(handle, 0, &iov).unwrap();
    assert_eq!(n, expected.len());
    fs.fsync(handle).unwrap();

    let on_disk = std::fs::read(dir.path().join("vectored_write.txt")).unwrap();
    assert_eq!(on_disk, expected);
    fs.release(handle).unwrap();
}

#[test]
fn read_vectored_scatters_one_read_into_multiple_iovecs() {
    let (_dir, fs) = fs_in_tempdir();
    let (_ino, handle, _) = fs
        .create(
            ROOT_ID,
            OsStr::new("vectored_read.txt"),
            libc::O_RDWR,
            0o644,
        )
        .unwrap();
    let full = b"0123456789abcdef";
    fs.write(handle, 0, full).unwrap();
    fs.fsync(handle).unwrap();

    // Three separate destination buffers of 5, 5, and 6 bytes -- should
    // be filled in order by a single preadv(), just like a request whose
    // reply spans multiple guest-memory descriptors.
    let mut buf_a = [0u8; 5];
    let mut buf_b = [0u8; 5];
    let mut buf_c = [0u8; 6];
    let iov = [
        libc::iovec {
            iov_base: buf_a.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf_a.len(),
        },
        libc::iovec {
            iov_base: buf_b.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf_b.len(),
        },
        libc::iovec {
            iov_base: buf_c.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf_c.len(),
        },
    ];

    let n = fs.read_vectored(handle, 0, &iov).unwrap();
    assert_eq!(n, full.len());
    assert_eq!(&buf_a, &full[0..5]);
    assert_eq!(&buf_b, &full[5..10]);
    assert_eq!(&buf_c, &full[10..16]);
    fs.release(handle).unwrap();
}

#[test]
fn read_vectored_at_offset_and_past_eof_matches_read() {
    let (_dir, fs) = fs_in_tempdir();
    let (_ino, handle, _) = fs
        .create(
            ROOT_ID,
            OsStr::new("vectored_read_eof.txt"),
            libc::O_RDWR,
            0o644,
        )
        .unwrap();
    fs.write(handle, 0, b"0123456789").unwrap();
    fs.fsync(handle).unwrap();

    // Ask for 8 bytes starting at offset 6, i.e. 4 real bytes ("6789")
    // followed by running off the end of the file -- preadv should
    // return a short count (4), not an error, matching plain pread()'s
    // short-read-at-EOF behavior.
    let mut buf = [0xffu8; 8];
    let iov = [libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    }];
    let n = fs.read_vectored(handle, 6, &iov).unwrap();
    assert_eq!(n, 4);
    assert_eq!(&buf[0..4], b"6789");
    fs.release(handle).unwrap();
}

#[test]
fn mkdir_lookup_readdir_rmdir() {
    let (_dir, fs) = fs_in_tempdir();
    let (sub_ino, attr) = fs.mkdir(ROOT_ID, OsStr::new("subdir"), 0o755).unwrap();
    assert!(attr.is_dir());

    fs.create(sub_ino, OsStr::new("a"), libc::O_RDWR, 0o644)
        .unwrap();
    fs.create(sub_ino, OsStr::new("b"), libc::O_RDWR, 0o644)
        .unwrap();

    let dh = fs.opendir(sub_ino).unwrap();
    let mut names: Vec<String> = fs
        .readdir(dh)
        .unwrap()
        .into_iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    fs.releasedir(dh).unwrap();
    assert_eq!(names, vec!["a", "b"]);

    fs.unlink(sub_ino, OsStr::new("a")).unwrap();
    fs.unlink(sub_ino, OsStr::new("b")).unwrap();
    fs.rmdir(ROOT_ID, OsStr::new("subdir")).unwrap();

    assert!(fs.lookup(ROOT_ID, OsStr::new("subdir")).is_err());
}

#[test]
fn rename_updates_tracked_location() {
    let (dir, fs) = fs_in_tempdir();
    let (ino, _, _) = fs
        .create(ROOT_ID, OsStr::new("old.txt"), libc::O_RDWR, 0o644)
        .unwrap();
    fs.rename(
        ROOT_ID,
        OsStr::new("old.txt"),
        ROOT_ID,
        OsStr::new("new.txt"),
    )
    .unwrap();

    assert!(!dir.path().join("old.txt").exists());
    assert!(dir.path().join("new.txt").exists());

    // setattr on the moved inode should act on new.txt, proving our
    // (parent, name) bookkeeping followed the rename.
    fs.setattr(
        ino,
        &SetAttr {
            mode: Some(0o600),
            ..Default::default()
        },
    )
    .unwrap();
    let meta = std::fs::metadata(dir.path().join("new.txt")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

#[test]
fn symlink_readlink() {
    let (_dir, fs) = fs_in_tempdir();
    let (ino, attr) = fs
        .symlink(ROOT_ID, OsStr::new("link"), OsStr::new("target"))
        .unwrap();
    assert!(attr.is_symlink());
    let target = fs.readlink(ino).unwrap();
    assert_eq!(target, OsStr::new("target"));
}

#[test]
fn setattr_truncate() {
    let (_dir, fs) = fs_in_tempdir();
    let (_ino, handle, _) = fs
        .create(ROOT_ID, OsStr::new("trunc.txt"), libc::O_RDWR, 0o644)
        .unwrap();
    fs.write(handle, 0, b"0123456789").unwrap();
    let (ino2, _) = fs.lookup(ROOT_ID, OsStr::new("trunc.txt")).unwrap();
    let attr = fs
        .setattr(
            ino2,
            &SetAttr {
                size: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(attr.size, 4);
    let data = fs.read(handle, 0, 4096).unwrap();
    assert_eq!(&data, b"0123");
    fs.release(handle).unwrap();
}

#[test]
fn hardlink() {
    let (dir, fs) = fs_in_tempdir();
    let (ino, _, _) = fs
        .create(ROOT_ID, OsStr::new("orig.txt"), libc::O_RDWR, 0o644)
        .unwrap();
    let attr = fs.link(ino, ROOT_ID, OsStr::new("linked.txt")).unwrap();
    assert_eq!(attr.nlink, 2);
    assert!(dir.path().join("linked.txt").exists());

    // Looking up either name should resolve to the same inode id (dedup by
    // (dev, ino) key).
    let (ino_a, _) = fs.lookup(ROOT_ID, OsStr::new("orig.txt")).unwrap();
    let (ino_b, _) = fs.lookup(ROOT_ID, OsStr::new("linked.txt")).unwrap();
    assert_eq!(ino_a, ino_b);
    assert_eq!(ino_a, ino);
}

#[test]
fn forget_evicts_inode() {
    let (_dir, fs) = fs_in_tempdir();
    let (ino, _, _) = fs
        .create(ROOT_ID, OsStr::new("f.txt"), libc::O_RDWR, 0o644)
        .unwrap();
    let before = fs.inode_count();
    fs.forget(ino, 1);
    assert_eq!(fs.inode_count(), before - 1);
}
