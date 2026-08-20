# riftlessfs

A userspace virtio-fs daemon (written in Rust) for sharing a host directory
into a Linux guest VM, with the goal of beating OrbStack's bind-mount
performance on macOS, Linux, and Windows hosts.

**Status: it mounts.** As of this writing, riftlessfsd has been verified
end-to-end against a real, unmodified **Fedora Linux 44** guest kernel
under **QEMU** (aarch64, HVF acceleration) on macOS: `mount -t virtiofs
myfs /mnt/rfs` succeeds, and file creation, reads, writes, directory
listing, renames, hardlinks, and an 8 MiB file copy all round-trip
correctly (verified with `sha256sum` matching on both sides). This is a
real milestone, not a simulation -- see "How this was actually verified"
below for exactly what was tested and how.

What's *not* true yet: **riftlessfs does not beat OrbStack.** Real,
head-to-head benchmarks now exist (see [BENCHMARKS.md](BENCHMARKS.md),
Phase 4) -- comparing riftlessfs against OrbStack, same guest OS, same
hardware, same workloads. Two real performance bugs were found and fixed
*by* that benchmarking pass (a disabled attribute cache, and a missing
`FUSE_WRITEBACK_CACHE` flag), taking sequential write throughput from
~100x behind OrbStack to ~5x behind, with random write now *ahead* of
OrbStack in that comparison -- real progress, driven by data, not yet a
win. Reads remain far behind (7.6-37x) and are next. Windows has no
transport at all (see Phase 3), and FUSE opcode coverage, while enough
for real everyday use, isn't exhaustive (xattrs, POSIX locks, and a few
other opcodes currently reply `ENOSYS`). This README stays explicit about
what's proven versus what isn't, so nobody mistakes "it mounts, and is
correct, and is a lot closer on writes than it was" for "this beats
OrbStack."

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

- **Phase 1 (done):** a transport-agnostic passthrough filesystem *engine*
  (`riftlessfs-core`) that's correct and fast in isolation, portable to
  Linux and macOS today.
- **Phase 2 (done -- verified against a real guest kernel):** a
  hand-rolled, portable subset of the vhost-user + virtio-fs
  (FUSE-over-virtio) wire protocol (`riftlessfs-proto`) that avoids the
  Linux-only pieces of the rust-vmm stack: message framing, `SCM_RIGHTS`
  socket transport, guest memory mapping, split-virtqueue parsing,
  FUSE-over-virtio wire structs, and a dispatch loop into
  `riftlessfs-core::PassthroughFs`. See "How this was actually verified"
  below.
- **Phase 3 (not started):** a Windows transport. Likely a custom protocol
  over Hyper-V sockets rather than vhost-user, since vhost-user itself
  isn't viable there. Scope/approach TBD.
- **Phase 4 (in progress -- gap narrowing, still losing):** real,
  head-to-head benchmarks against OrbStack, same guest OS (Fedora 44) and
  hardware for both sides. See [BENCHMARKS.md](BENCHMARKS.md) for full
  results and analysis. Two real bugs found and fixed *by* this
  benchmarking pass so far: attribute/entry caching was disabled entirely
  (a synthetic "stat 2000 files" benchmark was ~60x slower than it needed
  to be), and `FUSE_WRITEBACK_CACHE` wasn't advertised (every write,
  regardless of application request size, became an individual
  synchronous 4 KiB round trip). Fixing both took sequential write
  throughput from ~100x behind OrbStack to ~5x behind, and random write
  is now *ahead* of OrbStack in this single-run comparison. Reads are
  untouched by either fix and are now the largest relative gap (7.6x
  sequential, 37x random). Comparison against stock `virtiofsd` (Linux)
  hasn't been done yet.

## How this was actually verified

Getting a real front-end to talk to riftlessfsd surfaced two more
concrete, non-obvious portability findings, on top of the ones from
Phase 2:

1. **QEMU's own vhost-user support is Linux-only by default, on *any*
   host.** Homebrew's macOS QEMU build has no `vhost-user-fs-pci` device
   at all -- tracked down to `meson.build`:
   `have_vhost_user = get_option('vhost_user').disable_auto_if(host_os
   != 'linux')...`. This isn't specific to virtio-fs; it's vhost-user
   support in QEMU *as a front-end*, for any device. Building QEMU from
   source with `-Dvhost_user=enabled` works fine on macOS (confirmed:
   `vhost-user-fs-pci` shows up in `-device help` and works end-to-end),
   it's just not what you get from a package manager. This means CI/dev
   verification on macOS needs a custom QEMU build; Linux distributions'
   packaged QEMU has this enabled by default.
2. **A cross-platform errno bug that only a real Linux guest could catch.**
   The very first real mount attempt failed with `fsconfig() failed:
   Remote address changed` -- a nonsensical error for a local mount,
   until noticing that Linux's `strerror(78)` is `EREMCHG`
   ("Remote address changed"), and macOS's `ENOSYS` is *also* 78. Our
   `GETXATTR` handler correctly replied with `-ENOSYS`, but
   `libc::ENOSYS` is a **host** errno constant (78 on macOS, 38 on
   Linux) -- and the FUSE wire protocol is defined by the Linux kernel
   ABI, so every error code sent over it must be a *Linux* errno number
   regardless of what platform the daemon itself runs on. Fixed in
   `riftlessfs-proto::fuse::linux_errno`, which translates host errno
   values to hardcoded Linux ones at the one point they get written onto
   the wire (`fuse::wire::OutHeader::error_for`). This is exactly the
   kind of bug this project's whole premise is about catching early by
   actually testing across platforms rather than assuming POSIX means
   "the same everywhere."

With both of those fixed, the following was verified for real, on this
project's actual dev hardware (Apple Silicon macOS host):

- A QEMU 11.0.2 built from source with `-Dvhost_user=enabled`
  (`--target-list=aarch64-softmmu`), running with `-accel hvf`.
- A stock **Fedora Linux 44** aarch64 cloud image (kernel 6.19,
  unmodified), booted via UEFI, configured with cloud-init.
- `riftlessfsd` listening on a UNIX socket, with `-device
  vhost-user-fs-pci` as the QEMU-side client, backed by a
  `memory-backend-file` shared-memory region (the `memfd`-based backend
  isn't available on macOS QEMU either, for the same "Linux-only by
  default" reason, but the plain file-backed one works identically).
- Inside the guest: `mount -t virtiofs myfs /mnt/rfs` succeeds; creating,
  writing, reading, and listing files works; an 8 MiB file copied through
  the mount has a matching `sha256sum` on both sides; `rm`/`rmdir`
  (including the `ENOTEMPTY` case) and hardlinks work; `umount` is clean.

This same scenario is automated in `scripts/qemu-integration-test.sh` and
runs in CI (the `qemu-integration` job) on Linux x86_64 and aarch64
runners using each distro's packaged QEMU. It passes reliably (~90s) on
the x86_64 runner, which has usable `/dev/kvm`. The aarch64 runner
(`ubuntu-24.04-arm`, as of this writing) exposes no `/dev/kvm` at all,
forcing pure TCG software emulation, under which a full Fedora boot +
cloud-init + file I/O routinely exceeds even a 15-minute wait; CI detects
that up front and **skips** the test there rather than running it and
timing out (it's a runner limitation, not a code issue -- the identical
scenario is what was verified manually on real Apple Silicon hardware
above).

Anyone continuing this work should still read the module docs in
`riftlessfs-proto` before assuming everything left is easy: FUSE opcode
coverage is real but not exhaustive, and -- see
[BENCHMARKS.md](BENCHMARKS.md) -- performance, while much improved on
writes, is still well behind OrbStack on reads.

## What's implemented today

### `riftlessfs-core`

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

### `riftlessfs-proto`

`crates/riftlessfs-proto` implements the full vhost-user + FUSE-over-virtio
backend: message framing (12-byte header, payload structs, a
`Connection` type over `UnixStream` handling `SCM_RIGHTS` fds), guest
memory mapping (`vhost_user::memory`, including the gpa-vs-user-address
distinction described above), split-virtqueue parsing
(`vhost_user::virtqueue`), FUSE-over-virtio wire structs checked against
the current upstream `fuse.h` (`fuse::wire`), a request dispatcher into
`riftlessfs-core::PassthroughFs` (`fuse::dispatch`), the host-to-Linux
errno translation described above (`fuse::linux_errno`), and the event
loop tying it all together (`vhost_user::server::Server`). This is what's
been verified end-to-end against a real Fedora 44 guest (see above). It is
**Unix-only** (`riftlessfs_proto::VHOST_USER_SUPPORTED` is `false` on
Windows) for the same fd-passing reason as `riftlessfs-core`.

### Testing

All tests run real syscalls against a real temporary directory (no mocks):

```sh
cargo test --workspace
```

### Benchmarking

```sh
cargo bench -p riftlessfs-bench
```

This measures the engine's *own* overhead in-process (inode table
bookkeeping, locking, etc.) against raw `std::fs`/libc calls on the same
directory -- useful for catching regressions in the engine itself, but
it's not the same thing as end-to-end bind-mount performance across a VM
boundary. For that, see [BENCHMARKS.md](BENCHMARKS.md) and
`scripts/bind-mount-benchmark.sh`, which is where the real OrbStack
comparisons live.

## Repository layout

```
crates/
  riftlessfs-core   passthrough filesystem engine (Phase 1, implemented)
  riftlessfs-proto  vhost-user/virtio-fs backend (Phase 2, implemented and verified against a real guest)
  riftlessfsd       daemon binary wiring the above together
  riftlessfs-bench  criterion benchmarks + (future) full-stack benchmark scripts
```

## Usage

```sh
cargo build --release -p riftlessfsd
./target/release/riftlessfsd --shared-dir /path/to/share --socket-path /tmp/riftlessfs.sock
```

riftlessfsd listens on `--socket-path` and waits for a vhost-user
front-end to connect as a client. Point a VMM's vhost-user-fs device at
that same socket path, e.g. with QEMU:

```
-object memory-backend-file,id=mem,size=2G,mem-path=/tmp/qemu-mem,share=on
-numa node,memdev=mem
-chardev socket,id=char0,path=/tmp/riftlessfs.sock
-device vhost-user-fs-pci,chardev=char0,tag=myfs
```

(the memory backend **must** be `share=on`; the guest's RAM has to be
real shared memory riftlessfsd can `mmap`). Then, inside the guest:
`mount -t virtiofs myfs /mnt/somewhere`. Note that a stock Homebrew QEMU
on macOS won't have the `vhost-user-fs-pci` device at all -- see "How
this was actually verified" above.

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

CI (`.github/workflows/ci.yml`) builds, tests, and clippy-checks the
workspace on macOS (aarch64), Linux (x86_64/aarch64), and Windows
(x86_64/aarch64). On Windows, `riftlessfs-core`/`riftlessfs-proto`'s real
implementations aren't compiled in (their test files are
`#![cfg(unix)]`-gated), so those crates' tests pass trivially there (0
run) rather than being skipped outright -- everything still has to build
cleanly on all five targets.
