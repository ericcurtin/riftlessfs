//! Microbenchmarks of the `riftlessfs-core` passthrough engine, with plain
//! `std::fs`/libc calls against the same directory as a baseline.
//!
//! These measure the *engine's own overhead* (inode table locking,
//! bookkeeping, etc.) in-process -- they say nothing yet about end-to-end
//! bind-mount performance across a VM boundary, since the vhost-user
//! transport (`riftlessfs-proto`) isn't implemented yet. See the workspace
//! README for the full benchmark plan (fio/git/tar/compile workloads
//! against OrbStack once the transport exists).
//!
//! Run with: `cargo bench -p riftlessfs-bench`

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use riftlessfs_core::{PassthroughFs, ROOT_ID};
use std::ffi::OsStr;
use std::hint::black_box;
use std::os::unix::ffi::OsStrExt;

fn bench_getattr(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), b"data").unwrap();
    let fs = PassthroughFs::new(dir.path()).unwrap();
    let (ino, _) = fs.lookup(ROOT_ID, OsStr::new("f")).unwrap();

    let mut group = c.benchmark_group("getattr");
    group.bench_function("riftlessfs_core", |b| {
        b.iter(|| black_box(fs.getattr(ino).unwrap()))
    });
    group.bench_function("std_fs_metadata", |b| {
        b.iter(|| black_box(std::fs::metadata(dir.path().join("f")).unwrap()))
    });
    group.finish();
}

fn bench_lookup(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), b"data").unwrap();
    let fs = PassthroughFs::new(dir.path()).unwrap();

    let mut group = c.benchmark_group("lookup");
    group.bench_function("riftlessfs_core", |b| {
        b.iter_batched(
            || (),
            |_| {
                let (ino, _) = fs.lookup(ROOT_ID, OsStr::new("f")).unwrap();
                fs.forget(ino, 1);
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

fn bench_read_4k(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let payload = vec![0xABu8; 4096];
    std::fs::write(dir.path().join("f"), &payload).unwrap();
    let fs = PassthroughFs::new(dir.path()).unwrap();
    let (ino, _) = fs.lookup(ROOT_ID, OsStr::new("f")).unwrap();
    let handle = fs.open(ino, libc::O_RDONLY).unwrap();

    let path = dir.path().join("f");
    let raw_fd = unsafe {
        libc::open(
            std::ffi::CString::new(path.as_os_str().as_bytes())
                .unwrap()
                .as_ptr(),
            libc::O_RDONLY,
        )
    };

    let mut group = c.benchmark_group("read_4k");
    group.bench_function("riftlessfs_core", |b| {
        b.iter(|| black_box(fs.read(handle, 0, 4096).unwrap()))
    });
    group.bench_function("raw_pread", |b| {
        b.iter(|| {
            let mut buf = vec![0u8; 4096];
            let n = unsafe { libc::pread(raw_fd, buf.as_mut_ptr() as *mut libc::c_void, 4096, 0) };
            black_box(n)
        })
    });
    group.finish();
    unsafe { libc::close(raw_fd) };
}

fn bench_write_4k(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let payload = vec![0xABu8; 4096];
    let fs = PassthroughFs::new(dir.path()).unwrap();
    // iter_batched runs *all* `setup` closures for a batch before running
    // any `routine` closures, so each iteration needs its own filename
    // (create() uses O_EXCL) rather than reusing/unlinking one name.
    let counter = std::sync::atomic::AtomicU64::new(0);

    let mut group = c.benchmark_group("write_4k");
    group.bench_function("riftlessfs_core", |b| {
        b.iter_batched(
            || {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let name = format!("w{n}");
                let (ino, handle, _) = fs
                    .create(ROOT_ID, OsStr::new(&name), libc::O_RDWR, 0o644)
                    .unwrap();
                (name, ino, handle)
            },
            |(name, ino, handle)| {
                fs.write(handle, 0, &payload).unwrap();
                fs.release(handle).unwrap();
                fs.forget(ino, 1);
                fs.unlink(ROOT_ID, OsStr::new(&name)).unwrap();
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_getattr,
    bench_lookup,
    bench_read_4k,
    bench_write_4k
);
criterion_main!(benches);
