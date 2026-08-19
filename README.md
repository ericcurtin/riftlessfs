# riftlessfs

A userspace virtio-fs daemon (written in Rust) for sharing a host directory
into a Linux guest VM, with the goal of beating OrbStack's bind-mount
performance on macOS, Linux, and Windows hosts.

**Status: early days.** The passthrough filesystem engine is real,
unit-tested, and benchmarked (see below). The actual transport that would
let a VM talk to it (vhost-user / virtio-fs) is not implemented yet. There
is no working end-to-end mount today, and no benchmark results against
OrbStack yet. This README is deliberately explicit about that so nobody
mistakes "the engine works" for "this beats OrbStack" -- it doesn't, yet.

## Why this is hard, and what "beating OrbStack" actually requires

Bind-mount performance work means different things depending on what's on
each side of the mount. OrbStack shares a macOS host directory into a
Linux VM using **virtio-fs**: a paravirtualized filesystem device where a
"backend" process on the host speaks the FUSE wire protocol over a
`vhost-user` UNIX-domain-socket connection to the VMM, which relays it into
the guest kernel's virtio-fs driver. The reference implementation of that
backend is [`virtiofsd`](https://gitlab.com/virtio-fs/virtiofsd), written in
Rust on top of the [rust-vmm](https://github.com/rust-vmm) crates
(`vhost`, `vhost-user-backend`, `vm-memory`, `virtio-queue`).

Two things became clear from research before writing any code:

1. **Upstream `virtiofsd` is Linux-only, and not by accident.** Its own
   Homebrew formula has `depends_on :linux`, and the crates it's built on
   don't compile on macOS -- verified directly in this repo's history:
   `vhost` fails on macOS with `unresolved import
   vmm_sys_util::eventfd` (Linux's `eventfd(2)` has no macOS equivalent)
   and `cannot find value SO_DOMAIN in crate libc`. That means OrbStack is
   necessarily running *its own*, not-open-source virtio-fs backend
   implementation on macOS -- there's no off-the-shelf one to fork. This is
   the real opportunity here: a **portable** vhost-user-fs backend that
   works on macOS (and Linux, and ideally Windows) doesn't exist in the
   open yet.
2. **Windows can't speak vhost-user the same way at all.** vhost-user's
   memory-sharing and doorbell mechanism relies on passing file descriptors
   over a UNIX domain socket via `SCM_RIGHTS`. Windows's `AF_UNIX` sockets
   don't support ancillary-data fd passing. So "one virtio-fs/vhost-user
   daemon, three host OSes" isn't achievable as stated -- Windows will need
   either a different transport (e.g. something Hyper-V-socket-based) or a
   narrower integration (e.g. targeting WSL2 specifically), which is
   tracked as a later phase rather than pretended away.

Given that, the plan is split into phases:

- **Phase 1 (this repo, in progress):** a transport-agnostic passthrough
  filesystem *engine* (`riftlessfs-core`) that's correct and fast in
  isolation, portable to Linux and macOS today.
- **Phase 2 (not started):** a hand-rolled, portable subset of the
  vhost-user + virtio-fs (FUSE-over-virtio) wire protocol
  (`riftlessfs-proto`) that avoids the Linux-only pieces of the rust-vmm
  stack -- UNIX-domain-socket + `SCM_RIGHTS` fd passing + `mmap` all work
  on both Linux and macOS; the main remaining gap is `eventfd`-shaped
  "kick"/"call" doorbells, which can be emulated with a self-pipe on
  platforms without a native eventfd.
- **Phase 3 (not started):** a Windows transport. Likely a custom protocol
  over Hyper-V sockets rather than vhost-user, since vhost-user itself
  isn't viable there. Scope/approach TBD.
- **Phase 4 (not started):** real, reproducible benchmarks against
  OrbStack (macOS-only, so this comparison only makes sense on macOS CI/dev
  hardware) and against stock `virtiofsd` (Linux), using representative
  workloads (`fio` random/sequential I/O, `git status`/`clone`, `tar`,
  a real compile) -- not synthetic microbenchmarks.

Anyone continuing this work should read this section before assuming the
vhost-user transport is a small remaining step; it's most of the actual
engineering effort.

## What's implemented today: `riftlessfs-core`

`crates/riftlessfs-core` is a transport-agnostic passthrough filesystem
engine. Given a "shared directory," it implements lookup, getattr,
setattr, mkdir/rmdir, create/unlink, rename, symlink/readlink, hardlink,
open/read/write/fsync/release, and opendir/readdir/releasedir/statfs, all
implemented with fd-relative (`*at()`) syscalls so that concurrent renames
elsewhere on the host can't be exploited to escape the shared root the way
naive path-string passthrough filesystems can be.

It deliberately avoids the two Linux-specific tricks upstream `virtiofsd`
relies on (`O_PATH` fds re-opened via `/proc/self/fd/N`), because neither
exists on macOS -- verified empirically (see
`crates/riftlessfs-core/src/platform/mod.rs` doc comments): reopening a
`/dev/fd/N` path on macOS can only *narrow* access relative to the
original open, never widen it, unlike Linux's `/proc/self/fd/N` trick for
`O_PATH` fds. Instead, operations that need real read/write access re-open
by walking back to `(parent directory fd, name)`, which is uniform across
platforms.

It is **Unix-only for now** (`riftlessfs_core::PASSTHROUGH_SUPPORTED` is
`false` on Windows); see Phase 3 above.

### Testing

All tests run real syscalls against a real temporary directory (no mocks):

```sh
cargo test --workspace
```

### Benchmarking

```sh
cargo bench -p riftlessfs-bench
```

This currently measures the engine's *own* overhead in-process (inode
table bookkeeping, locking, etc.) against raw `std::fs`/libc calls on the
same directory -- useful for catching regressions in the engine itself,
but it says nothing yet about end-to-end bind-mount performance across a
VM boundary, since there is no transport yet. Phase 4 above is where
OrbStack comparisons belong.

## Repository layout

```
crates/
  riftlessfs-core   passthrough filesystem engine (Phase 1, implemented)
  riftlessfs-proto  vhost-user/virtio-fs wire protocol (Phase 2, not implemented)
  riftlessfsd       daemon binary wiring the above together
  riftlessfs-bench  criterion benchmarks + (future) full-stack benchmark scripts
```

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

CI (`.github/workflows/ci.yml`) builds and clippy-checks the workspace on
macOS (aarch64), Linux (x86_64/aarch64), and Windows (x86_64/aarch64), and
runs the full test suite everywhere except Windows (where
`riftlessfs-core`'s real engine isn't compiled in yet).
