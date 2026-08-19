//! End-to-end tests of the passthrough engine against a real temporary
//! directory on whatever filesystem the test runner uses (APFS on macOS,
//! ext4/btrfs/tmpfs on Linux CI). These exercise the actual syscalls, not
//! mocks.

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
