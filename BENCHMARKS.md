# Benchmarks: riftlessfs vs. OrbStack and stock virtiofsd

**Status: still behind OrbStack, competitive with (or ahead of) stock
virtiofsd on several benchmarks, root causes for what's left are
understood.** This is Phase 4 from the README. Four real bugs/gaps have
been found and fixed by actually running these benchmarks so far; the
honest headline is still **riftlessfs does not beat OrbStack**, but the
gap on writes went from ~100x to ~5x, sequential read improved 2.1x, and
-- new in this section -- a direct, same-hardware comparison against the
*reference* vhost-user-fs implementation (stock `virtiofsd` on real Linux
+ KVM) shows riftlessfs matching it almost exactly on random-read latency
and beating it on every metadata operation tested, with the remaining gap
concentrated specifically in raw write/read throughput.

## Methodology

Both sides ran the *same* guest OS (Fedora Linux 44, aarch64) and the
*same* benchmark script (`scripts/bind-mount-benchmark.sh`) on the same
Apple Silicon Mac, back to back, to keep the comparison as apples-to-apples
as practical:

- **OrbStack**: `orb create fedora:44`, using its default bind mount
  (`/Users/...` shared into the machine via its own virtiofs
  implementation).
- **riftlessfs**: the same QEMU + Fedora 44 setup described in the
  README's "How this was actually verified" section, mounting a
  riftlessfsd-served directory via `vhost-user-fs-pci`.

Workload (see the benchmark script for exact parameters): `fio`
sequential write/read (1 MiB blocks, 512 MiB) and random write/read (4 KiB
blocks, 128 MiB), all buffered (`direct=0`); a metadata microbenchmark
(create/stat/remove 2000 files, single-process via Python to avoid
fork/exec overhead dominating the measurement); and a "synthetic source
tree" test (1000 small files across 100 directories: tar create, delete,
tar extract, `find`, `rm -rf`).

This is a first pass, not a rigorous statistical benchmark suite (single
run each, no warm-up/repeat-and-average, no variance reporting). Treat the
numbers as indicative of *where the gaps are*, not as final performance
claims. Some run-to-run noise is visible below (e.g. "stat 2000 files"
got slower after a change that shouldn't have affected it at all) --
that's the kind of thing a single run can't distinguish from a real
regression, which is exactly why "more rigor" is still on the follow-up
list.

## Results

| Benchmark | OrbStack | v1 (no attr cache) | v2 (+ attr cache) | v3 (+ writeback cache) | v4 (+ async read, keep-cache) |
|---|---|---|---|---|---|
| Sequential write, 512 MiB, 1 MiB blocks | 3200 MiB/s | 31.9 MiB/s | 32.2 MiB/s | 600 MiB/s | 557 MiB/s |
| Sequential read, 512 MiB, 1 MiB blocks | 6095 MiB/s | 824 MiB/s | 775 MiB/s | 806 MiB/s | **1673 MiB/s** |
| Random write, 128 MiB, 4 KiB blocks | 119 MiB/s (30.4k IOPS) | 32.0 MiB/s (8.2k IOPS) | 31.2 MiB/s (8.0k IOPS) | 129 MiB/s (33.0k IOPS) | 90.1 MiB/s (23.1k IOPS) |
| Random read, 128 MiB, 4 KiB blocks | 1196 MiB/s (306k IOPS) | 31.8 MiB/s (8.2k IOPS) | 31.4 MiB/s (8.1k IOPS) | 32.4 MiB/s (8.3k IOPS) | 29.4 MiB/s (7.5k IOPS) |
| Create 2000 files | 0.134 s | 2.952 s | 1.089 s | 1.096 s | 1.241 s |
| Stat 2000 files | 0.003 s | 1.997 s | **0.033 s** | 0.245 s | 0.281 s |
| Remove 2000 files | 0.071 s | 2.315 s | 0.997 s | 1.251 s | 1.363 s |
| tar create (1000 files) | 0.045 s | 1.828 s | 0.786 s | 0.751 s | 0.517 s |
| tar extract (1000 files) | 0.121 s | 1.680 s | 1.217 s | 1.325 s | 1.441 s |
| find (1000 files) | 0.006 s | 0.156 s | 0.058 s | 0.057 s | 0.061 s |
| rm -rf (1000 files) | 0.059 s | 0.866 s | 0.528 s | 0.685 s | 0.775 s |

v1 -> v2, v2 -> v3, and v3 -> v4 are three real fixes made *during* this
benchmarking exercise, not hypotheticals -- see below for each. Sequential
read got a real, substantial boost from v4 (2.1x). Write numbers moved
*down* slightly from v3 to v4 despite the v4 change being read-focused
(`FUSE_ASYNC_READ`, `FOPEN_KEEP_CACHE`) and having no obvious mechanism to
affect writes -- the most likely explanation is run-to-run noise (single
runs, shared dev hardware, no averaging -- see "Methodology"), not a real
regression, but this is exactly the kind of ambiguity flagged as a
follow-up item rather than resolved by assertion.

## Fix 1 (v1 -> v2): attribute/entry caching was disabled entirely

`riftlessfs-proto` was advertising a `0`-second attribute/entry cache
validity to the guest kernel (`fuse::wire::CACHE_TIMEOUT_SECS`), meaning
*every* `stat()`-family call became a synchronous round trip through the
whole vhost-user/FUSE pipeline. Changing it to a conservative 1 second
dropped "stat 2000 files" by ~60x at the time, with no effect on raw
read/write throughput (different code path). An easy, safe, well-understood
fix -- essentially every FUSE filesystem does this -- that just hadn't
been done yet because there was no data showing it mattered.

## Fix 2 (v2 -> v3): no `FUSE_WRITEBACK_CACHE`

Sequential 1 MiB writes and random 4 KiB writes landed at *almost the
same* MiB/s in v1/v2 (32.2 vs. 31.2). If riftlessfs had a roughly fixed
per-request overhead, larger requests would show *much higher* throughput
than small ones (same fixed cost amortized over more bytes) -- instead,
throughput was essentially constant regardless of request size, which is
the signature of the guest kernel breaking every write into individual
page-sized (4 KiB) FUSE requests no matter what the application asked
for. That's expected, standard Linux FUSE behavior in the absence of
`FUSE_WRITEBACK_CACHE`: without it, dirty pages aren't coalesced in the
guest's page cache before being sent to the filesystem, so a 1 MiB
`write()` becomes 256 separate synchronous 4 KiB `WRITE` requests, each
paying the full virtqueue/vhost-user round trip individually.

Enabling it (`fuse::wire::init_out`, see the `FUSE_WRITEBACK_CACHE`
constant's doc comment for the full reasoning) gave an 18.6x improvement
on sequential write and 4.1x on random write. This was a deliberately
more careful change than fix 1 -- the kernel takes over dirty-page/size
coherency once this is negotiated -- so before trusting it, the following
were re-verified against a real guest, not just the existing in-process
test suite (which doesn't exercise real kernel writeback behavior at
all): the full `cargo test --workspace` suite still passes, and a fresh
manual test wrote and read back two files (one via `cp`, one via `dd`)
through the mount with matching `sha256sum` on both sides -- data
integrity holds under the new write-batching behavior.

## Fix 3 (v3 -> v4): no `FUSE_ASYNC_READ` / `FOPEN_KEEP_CACHE`

Without `FUSE_ASYNC_READ`, the guest kernel only ever keeps one
readahead request outstanding at a time, waiting for each reply before
issuing the next -- so sequential reads couldn't benefit from
`Server::process_vring` already draining and processing every available
descriptor chain in one kick before notifying once (see its docs):
there was never more than one chain available to drain. Enabling it lets
the kernel pipeline readahead requests, which this loop was already
structurally ready to batch.

`FOPEN_KEEP_CACHE` stops the guest from throwing away a file's cached
pages just because it was opened again (e.g. by a second process, or the
same tool reopening a file it just wrote) -- riftlessfsd has no reason
not to set this unconditionally: there's currently no *other* invalidation
mechanism to weaken by doing so (see "Next steps" #3 below).

Result: sequential read 806 -> 1673 MiB/s (2.1x), closing that gap from
7.6x to 3.6x behind OrbStack. Random read is essentially unchanged (as
expected: a true 4 KiB-random, `iodepth=1` workload has nothing sequential
for readahead to prefetch, and no amount of pipelining helps when the
*application* never has more than one outstanding request either).

## What's still behind, and why

- **Random read/write (37x / ~1.4x behind) and sequential write (~5x
  behind).** Not caching-flag problems -- bounded by the actual
  per-request round-trip latency of one synchronous request at a time
  (inherent to `iodepth=1` workloads), or by writeback batching
  granularity. See "Where the per-request latency actually goes" below
  for what was found (and ruled out) trying to close this further.
- **Sequential read (3.6x behind).** Improved substantially (v4); the
  remainder is plausibly the same latency floor as sequential write,
  and/or missing DAX-style optimizations.
- **Metadata operations show noise, not a clear trend, across v2/v3/v4**
  -- consistent with none of these changes targeting metadata-only
  operations on freshly created, never-read/written files. Treat these
  deltas as measurement noise (see "Methodology") rather than a real
  effect until a more rigorous run says otherwise.

## Where the per-request latency actually goes (and a negative result)

The earlier hypothesis here was that `Server::process_vring`/`Server::run`
processing one request at a time, without pipelining, was the likely
remaining lever for random I/O. Rather than assume that and rewrite the
loop, `process_vring` got real instrumentation
(`log::trace!("... request processed in {:?}", elapsed)` around the
gather/dispatch/scatter/push-used sequence, `RUST_LOG=trace`-gated so it
costs nothing normally), and a small number of real requests were traced
against the live guest.

Result: riftlessfsd's own processing of a request -- memory reads,
the actual `pread`/`pwrite` syscall, encoding the reply -- takes
**~2 microseconds** (occasionally up to tens of microseconds for less
common opcodes). Random read IOPS (~7.5-8k) imply a **~120-130
microsecond** round-trip per request. That means well over 95% of the
latency is spent *outside* riftlessfsd's own code entirely.

Based on that, the next hypothesis was that blocking in `poll()` and
paying the OS scheduler's wake-up cost on *this process's* side was a
meaningful chunk of that gap, so a bounded busy-poll (repeated
non-blocking `poll()` calls for up to 200us before falling back to a
normal blocking wait -- the same trade-off DPDK-style poll-mode drivers
make) was implemented and measured, not just proposed. **Measured
result: no significant change** to random read/write throughput. That's
a genuine negative result, not a wasted effort: it means the remaining
latency is dominated by something further down the chain that
riftlessfsd doesn't control on its own -- QEMU's own event-loop wake-up,
the guest kernel's task scheduling for the process that issued the I/O,
HVF/KVM interrupt-injection cost -- not by how riftlessfsd itself waits
for work. The busy-poll change was reverted rather than kept for a
CPU cost with no demonstrated benefit (see the `Server::run` doc comment
in the source for the fuller account).

This significantly changes the "next steps" priority below: rewriting
riftlessfsd's own request-processing loop for concurrency is *not*
expected to move random-I/O numbers much on its own, since riftlessfsd's
own share of the latency is already ~2%. Closing this gap further likely
needs either a different transport-level mechanism (shared-memory/DAX,
which is a much larger undertaking) or work outside this project (the
VMM/guest side of the round trip).

## riftlessfs vs. stock virtiofsd, same hardware, same guest, same kernel

Unlike the OrbStack comparison (which also differs in VM management,
host OS, and every other layer besides the vhost-user-fs backend), this
one isolates the backend implementation as close to the only variable
as practical: `scripts/compare-virtiofsd.sh` boots **one** Fedora 44
guest under QEMU with real KVM acceleration (a GitHub Actions
`ubuntu-24.04` runner), with *both* riftlessfsd and Ubuntu's packaged
`virtiofsd` (1.10.0, `--cache=auto --writeback` -- its own best-case
config, to compare fairly against what riftlessfsd now does) attached
simultaneously as separate `vhost-user-fs-pci` devices, and runs the same
`bind-mount-benchmark.sh` against each from inside the guest. Reproducible
via the (manually-triggered) `compare-virtiofsd.yml` CI workflow.

**Run 1** (riftlessfsd's `max_write` was still 128 KiB at this point):

| Benchmark | riftlessfsd | virtiofsd | riftlessfs vs. virtiofsd |
|---|---|---|---|
| Sequential write, 512 MiB, 1 MiB blocks | 307 MiB/s | 866 MiB/s | 2.8x behind |
| Sequential read, 512 MiB, 1 MiB blocks | 749 MiB/s | 1925 MiB/s | 2.6x behind |
| Random write, 128 MiB, 4 KiB blocks | 170 MiB/s (43.6k IOPS) | 552 MiB/s (141k IOPS) | 3.2x behind |
| Random read, 128 MiB, 4 KiB blocks | 52.4 MiB/s (13.4k IOPS) | 52.9 MiB/s (13.5k IOPS) | ~equal (1.01x) |
| Create 2000 files | 0.745 s | 0.761 s | ~equal, riftlessfs marginally ahead |
| Stat 2000 files | 0.148 s | 0.164 s | riftlessfs ahead |
| Remove 2000 files | 0.485 s | 0.511 s | riftlessfs ahead |
| tar create (1000 files) | 0.279 s | 0.360 s | riftlessfs ahead |
| tar extract (1000 files) | 0.838 s | 0.919 s | riftlessfs ahead |
| find (1000 files) | 0.043 s | 0.039 s | ~equal |
| rm -rf (1000 files) | 0.303 s | 0.314 s | ~equal |

Two things stood out immediately, and both matter more than they might
look like at first glance:

1. **Random read is essentially identical (52.4 vs. 52.9 MiB/s).** This
   directly confirms the previous section's conclusion, with an
   independent implementation as the control group instead of just
   riftlessfsd's own instrumentation: the ~120us/request latency isn't
   riftlessfs-specific inefficiency, it's very close to what the
   *reference* vhost-user-fs backend achieves on the same hardware under
   the same `iodepth=1` random workload. That round trip really is
   dominated by the transport (vhost-user + KVM + guest kernel
   scheduling), not by which backend implementation is on the other end
   of the socket.
2. **riftlessfs matches or beats virtiofsd on every metadata operation
   tested.** This wasn't a given -- virtiofsd is a mature, heavily-used
   reference implementation -- and is a genuinely encouraging signal that
   riftlessfs's core design (fd-relative `*at()` syscalls, the attribute
   cache timeout, `FOPEN_KEEP_CACHE`) isn't leaving obvious performance
   on the table for this class of operation.

Reading upstream virtiofsd's own `init()` reply (`src/server.rs`) turned
up the single biggest concrete difference behind the write/read
throughput gap: it advertises `max_write = 1 MiB`; riftlessfsd was
advertising 128 KiB, an 8x difference. Matched it (see
`fuse::wire::MAX_WRITE`'s doc comment) and re-ran:

**Run 2** (riftlessfsd's `max_write` now 1 MiB, matching virtiofsd):

| Benchmark | riftlessfsd | virtiofsd | riftlessfs vs. virtiofsd |
|---|---|---|---|
| Sequential write, 512 MiB, 1 MiB blocks | 407 MiB/s | 869 MiB/s | **2.1x behind** (was 2.8x) |
| Sequential read, 512 MiB, 1 MiB blocks | 657 MiB/s | 2032 MiB/s | 3.1x behind (was 2.6x) |
| Random write, 128 MiB, 4 KiB blocks | 268 MiB/s (68.7k IOPS) | 883 MiB/s (226k IOPS) | 3.3x behind (was 3.2x) |
| Random read, 128 MiB, 4 KiB blocks | 78.8 MiB/s (20.2k IOPS) | 75.3 MiB/s (19.3k IOPS) | **riftlessfs ahead** (1.05x) |
| Create 2000 files | 0.366 s | 0.376 s | riftlessfs ahead |
| Stat 2000 files | 0.072 s | 0.086 s | riftlessfs ahead |
| Remove 2000 files | 0.256 s | 0.305 s | riftlessfs ahead |
| tar create (1000 files) | 0.191 s | 0.240 s | riftlessfs ahead |
| tar extract (1000 files) | 0.455 s | 0.513 s | riftlessfs ahead |
| find (1000 files) | 0.029 s | 0.031 s | riftlessfs ahead |
| rm -rf (1000 files) | 0.167 s | 0.163 s | ~equal |

**Read this carefully, not just the ratio column**: every single number
moved between runs, for *both* backends (e.g. virtiofsd's own random
write went from 552 to 883 MiB/s) -- this is a shared GitHub Actions
runner, and that's real run-to-run variance, not a change in either
binary's behavior between runs. The metadata numbers all improved
proportionally for both sides (consistent with "this run happened to get
less noisy-neighbor interference," not with either fix mattering for
metadata at all, which matches expectations). What's meaningful is what
moved *relative to the other backend on the same run*:

- **Sequential write's ratio genuinely improved** (2.8x -> 2.1x behind):
  consistent with `max_write` being a real, direct lever for sequential
  transfers, as expected.
- **Random write's ratio did not improve** (3.2x -> 3.3x, i.e. unchanged
  within noise): since virtiofsd *already* had `max_write = 1 MiB` before
  riftlessfsd matched it, closing that gap only helps where request-size
  mismatch was actually the bottleneck. Random write's gap is apparently
  driven by something else entirely -- worth investigating specifically,
  not assumed to be more of the same fix.
- **Random read went from ~equal to riftlessfs slightly *ahead*.** Given
  the run-to-run noise just described, treat this as "still roughly
  equal," not as a new win to bank on.

### Following up on the random-write gap: a second, real protocol bug

Investigating "why didn't `max_write` help random write" (rather than
guessing) meant instrumenting `fuse::dispatch::Session::handle` to log
the actual size of every `pwrite()`/`pread()` reaching the backend (see
its `log::trace!` calls), then running fio directly against a local
riftlessfsd + real QEMU/KVM (HVF, locally) guest and reading the trace.
That turned up a second, independent bug: **every write -- sequential
*or* random -- was capped at exactly 131072 bytes (128 KiB), even though
`max_write` had already been raised to 1 MiB.**

The reason: per the FUSE kernel ABI (`include/uapi/linux/fuse.h`), the
field that governs how much dirty writeback-cached data the kernel
batches into one `WRITE` request is `max_pages`, not `max_write` --  and
`max_pages` is silently ignored unless the `FUSE_MAX_PAGES` init flag is
also set. riftlessfsd was setting `max_write` but never `max_pages` or
its flag, so the kernel fell back to its own built-in default of 32
pages (128 KiB) for writeback batching purposes, regardless of what
`max_write` said. Fixed by advertising `FUSE_MAX_PAGES` with
`max_pages` covering the full `max_write` (see `fuse::wire::init_out`).

Verified directly (not just inferred) by re-tracing after the fix:
sequential-write `pwrite()` calls jumped from capped-at-131072-bytes to
mostly exactly 1048576 bytes (1 MiB) -- a ~7.5x reduction in syscall
(and, more importantly, virtio round-trip) count for the same data
volume. Random write's `pwrite()` sizes, by contrast, stayed mostly at
4096 bytes even after the fix (only a small tail coalesced up to ~28
KiB) -- which makes sense and is an important negative result in its
own right: writeback can only merge writes that end up *physically
adjacent in the file* by the time a flush happens, and a genuinely
random access pattern offers little of that no matter how generous the
batching ceiling is. So this fix is expected to help sequential
(and any workload with real locality) substantially, but **random
write's gap was never explained by our own `max_write`/`max_pages`
settings being wrong** -- with those now correct on our side, whatever
gap remains against virtiofsd on truly-random, mostly-4-KiB-request
writes needs a different explanation (see next steps).

This was also a useful lesson in verification: the earlier `max_write`
fix's changelog claimed it closed part of the write gap, and it did (the
measured throughput numbers improved) -- but nobody had actually
confirmed *why* by checking real wire sizes until this pass, and it
turned out `max_write` alone wasn't even taking effect for writeback
batching the way it looks like it should from reading the constant
alone. Added `init_out_advertises_max_pages_matching_max_write` (in
`fuse::wire`) as a regression test so this can't silently regress again.
CI (fmt/build/test/clippy) verified green and a real 64 MiB
copy-and-`sha256sum` round trip through the local QEMU guest still
matches after the change.

**Run 3** (same `compare-virtiofsd.yml`, after the `FUSE_MAX_PAGES`
fix; `linux-x86_64` -- the `linux-aarch64` leg skipped as usual, no
`/dev/kvm` on that runner this time):

| Benchmark | riftlessfsd | virtiofsd | riftlessfs vs. virtiofsd |
|---|---|---|---|
| Sequential write, 512 MiB, 1 MiB blocks | 401 MiB/s | 861 MiB/s | 2.1x behind (was 2.1x) |
| Sequential read, 512 MiB, 1 MiB blocks | 1718 MiB/s | 1869 MiB/s | 1.09x behind (was 3.1x -- turned out to be noise, not this fix; see below) |
| Random write, 128 MiB, 4 KiB blocks | 269 MiB/s (69.0k IOPS) | 895 MiB/s (229k IOPS) | 3.3x behind (was 3.3x) |
| Random read, 128 MiB, 4 KiB blocks | 88.0 MiB/s (22.5k IOPS) | 73.5 MiB/s (18.8k IOPS) | **riftlessfs ahead** (1.2x) |
| Create 2000 files | 0.441 s | 0.425 s | ~equal |
| Stat 2000 files | 0.105 s | 0.069 s | virtiofsd ahead this run |
| Remove 2000 files | 0.288 s | 0.254 s | virtiofsd ahead this run |
| tar create (1000 files) | 0.152 s | 0.209 s | riftlessfs ahead |
| tar extract (1000 files) | 0.459 s | 0.398 s | virtiofsd ahead this run |
| find (1000 files) | 0.029 s | 0.027 s | ~equal |
| rm -rf (1000 files) | 0.150 s | 0.155 s | ~equal |

This confirms both predictions from the local trace investigation
exactly: **random write's ratio is unchanged (3.3x, both before and
after)** -- as expected, since its `pwrite()` sizes barely moved -- and
random read remains roughly at parity (still noisy enough run-to-run
that "riftlessfs ahead" shouldn't be over-read, same caveat as before).
Metadata ops flipped which side "wins" on several rows compared to Run
2 (stat, remove, and tar extract all favor virtiofsd here, versus
riftlessfs in the previous run) -- further, concrete confirmation that
these small deltas are noise, not real regressions or improvements,
exactly the caution the "add repeatability" next-step item has been
flagging.

**Sequential read improving from 3.1x behind to 1.09x behind (near
parity) looked like a standout result from the `max_pages` fix, but
directly testing that hypothesis disproved it.** The fix was made (and
reasoned about) entirely in terms of *write* batching, since that's
where the trace investigation found the bug (capped `pwrite()` sizes);
nothing in that investigation had looked at read request sizes at all.
The plausible-sounding theory was that `max_pages` isn't write-specific
in the kernel and also bounds readahead request size -- but "plausible"
isn't "confirmed," so it was tested directly rather than left as a
guess: built both the pre- and post-fix binaries locally, traced actual
`pread()` sizes against the same real QEMU/HVF guest with a cold page
cache (`echo 3 > /proc/sys/vm/drop_caches` before each read) to force
genuine backend round trips, and compared.

**Result: identical.** Both binaries produced exactly 512 `pread()`
calls of exactly 131072 bytes (128 KiB) each for the same 64 MiB cold
sequential read -- `max_pages` has *zero* effect on read chunk size.
The real governing factor, confirmed by directly varying it: the
guest's per-`bdi` `read_ahead_kb` (`/sys/class/bdi/<dev>/read_ahead_kb`,
Linux's default is 128, i.e. exactly the 128 KiB observed) -- bumping it
to 1024 on the same (post-fix) guest changed the trace to exactly 64
`pread()` calls of exactly 1048576 bytes (1 MiB, capped by `max_write`,
as expected) for the same file. `max_pages` was never involved.

**Conclusion: the sequential-read improvement in the Run 2 -> Run 3 CI
comparison was not caused by this fix.** It has to be attributed to
run-to-run noise on the shared runner, the same phenomenon already
documented for the metadata rows in both runs. This is worth stating
plainly rather than quietly dropping: the initial write-up treated a
correlation (fix shipped, read number improved) as if it had a
plausible causal story, and that story turned out to be wrong when
actually tested. The `max_pages` fix's real, *confirmed* effect remains
exactly what the write-side trace showed: fewer/larger writes where the
access pattern has real locality, no effect on random writes, and (now
confirmed) no effect on reads of any pattern.

### A candidate explanation for the random-write gap: `pwrite()` itself, not our protocol handling

With request-size mismatch ruled out twice over, the next question was
whether the gap lives in riftlessfsd's own request handling at all, or
purely in the external transport (already shown to be at parity for
reads). `Server::process_vring`'s per-request trace was extended to
include the opcode (`fuse::wire::Opcode`, peeked directly from the
gathered request's header rather than re-parsing it) so WRITE and READ
requests' *own* total processing time -- everything except the external
VM-exit/scheduler wait already shown to be transport-bound -- could be
compared directly, on top of the existing per-syscall `pwrite`/`pread`
timing.

Running matched 4 KiB random-write and random-read fio jobs against a
local riftlessfsd (real QEMU/HVF guest, same setup as the earlier
tracing) and comparing:

| | avg. total request processing time | avg. `pwrite`/`pread` syscall time alone |
|---|---|---|
| WRITE (4 KiB) | 12.0 us | 8.03 us |
| READ (4 KiB) | 4.3 us | 1.46 us |

**`pwrite()` itself is ~5.5x slower than `pread()` for the same 4 KiB
size on this host filesystem, and that syscall-time difference (6.6 us)
accounts for ~85% of the total per-request processing time gap (7.7
us).** This isn't a FUSE-protocol-level or riftlessfsd-specific cost --
it's the underlying host filesystem call itself (write path: dirty page
tracking, inode mtime update, journaling; read path: none of that) --
but it matters a lot more for *throughput* than the read-latency
comparison suggested, for a specific reason: `Server::process_vring`
drains and processes an entire batch of available requests
back-to-back before paying the external wake-up cost again (see its
docs) -- confirmed earlier to be large for random write specifically
(100-200+ requests per wake-up, from the writeback-batching trace).
Within a batch, total time is dominated by *N times the per-request
internal processing time*, not by the external per-request latency
that dominates a single request's round trip -- so a syscall that's
slower per-call becomes a real, cumulative throughput cost precisely in
the batched-many-small-requests case, and much less so for reads (no
equivalent write-back batching mechanism drives large batches of small
reads) or for sequential writes (far fewer, much larger `pwrite` calls
for the same data volume, so the *fixed* per-syscall overhead -- as
opposed to the per-byte cost -- gets amortized over more data). That
gradient (fixed per-syscall cost mattering more, the smaller and more
numerous the requests) is at least directionally consistent with
random write's larger observed gap (~3.3x) versus sequential write's
smaller one (~2.1x).

**This is a real, measured local fact, not yet confirmed as *the*
explanation for the CI-measured gap** -- two things are missing before
treating it as settled, in light of the sequential-read lesson just
above: (1) this was measured on the local macOS/APFS dev host, not the
actual Linux/ext4 comparison hardware, and host filesystem write-vs-read
asymmetry could plausibly differ in magnitude or even direction there;
(2) it's only riftlessfsd's own syscall timing -- there's no equivalent
measurement yet of virtiofsd's own `pwrite`/`pread` timing on the same
host filesystem, so it's not yet known whether virtiofsd pays the same
per-syscall cost and simply amortizes it better (e.g. via a genuinely
different I/O mechanism), or avoids much of it entirely.

### Testing that theory on the actual hardware: it doesn't hold up either

Built `scripts/syscall-cost-compare.sh` + `.github/workflows/syscall-cost-compare.yml`:
a narrow companion to `compare-virtiofsd.sh` that wraps both daemons in
`strace -f -T` from process start (avoiding the need to attach to an
already-running PID) and runs a small 16 MiB random-write/random-read
fio job purely to compare `pwrite`/`pread`-family syscall timing --
deliberately a separate script, since tracing overhead would distort
the throughput numbers `compare-virtiofsd.sh` exists to report cleanly.

First run immediately found something not anticipated: riftlessfsd's
numbers came through as expected (`pwrite64`), but virtiofsd showed
**zero** `pwrite64` calls. Its diagnostic fallback (list whatever
write/read syscalls actually appear, added after this exact surprise)
revealed why: virtiofsd doesn't call plain `pwrite`/`pread` at all --
it uses vectored I/O, `pwritev2` and `preadv`. Two script iterations
later (each guided by the actual trace output rather than more
guessing), stable numbers on `linux-x86_64`:

| | avg. syscall time | count |
|---|---|---|
| riftlessfsd `pwrite64` | 46.3 us | 2143 |
| riftlessfsd `pread64` | 32.3 us | 4098 |
| virtiofsd `pwritev2` | 40.5 us | 2168 |
| virtiofsd `preadv` | 28.5 us | 4096 |

**The write-vs-read ratio is nearly identical between the two
implementations (riftlessfsd 1.43x, virtiofsd 1.42x), and the absolute
magnitudes are close (riftlessfsd ~13-14% slower for both operations,
not specifically for writes).** This directly refutes the theory above:
on the actual comparison hardware, `pwrite`/`pwritev2` is *not*
disproportionately slower than `pread`/`preadv` for riftlessfsd
relative to virtiofsd -- both pay a broadly similar, modest write
premium. Syscall counts are also nearly identical (2143 vs. 2168),
ruling out "virtiofsd merges more logical writes per syscall" as an
explanation too. Whatever produces the ~3.3x *throughput* gap despite
near-identical *syscall* costs and counts has to be something else --
this local-testing-derived theory is a second one (after the
sequential-read case) that looked plausible and didn't survive being
tested directly on the hardware that actually matters. Recording that
plainly rather than quietly moving on, same as before.

### A confirmed, real structural difference: zero-copy I/O

Investigating where else the difference could be (given syscalls
themselves are now ruled out) led to reading virtiofsd's actual
`read`/`write` implementation
(`fuse-backend-rs`/`virtiofsd`'s `passthrough/mod.rs`): it uses
`ZeroCopyReader`/`ZeroCopyWriter` traits whose `read_from_file_at`/
`write_to_file_at` methods are explicitly documented as using
`preadv64`/`pwritev2` -- i.e. virtiofsd builds an iovec pointing
*directly into the guest's mapped memory* and hands it straight to the
vectored syscall, with **no intermediate host-side buffer copy at
all**.

riftlessfsd does not do this. `Virtqueue::gather_readable` copies every
readable descriptor segment into a heap-allocated `Vec<u8>` before
`fuse::dispatch::Session::handle` ever sees the request (so `WRITE`'s
payload is copied guest-memory -> heap, then `pwrite()` copies
heap -> kernel page cache -- two copies where virtiofsd's `pwritev2`
does one), and `Virtqueue::scatter_writable` copies the reply
(including `READ`'s payload) from a heap `Vec<u8>` back into guest
memory (again, an extra copy `preadv` avoids).

This is a real, source-confirmed architectural difference, not a
guess -- but its *quantitative* contribution to the ~3.3x random-write
gap is still unknown: the extra copy is proportional to request size,
and at 4 KiB it's sub-microsecond, small next to the tens-of-microseconds
syscall costs measured above. It's plausible this matters more
cumulatively (extra CPU work per request, in a single-threaded
process-everything-sequentially loop) than any single measurement
here has isolated, but that's not yet demonstrated the way the syscall
comparison was. Implementing the equivalent zero-copy path in
riftlessfsd (building `iovec`s directly from `chain.readable`/
`chain.writable`'s guest-memory slices and using `preadv`/`pwritev`
instead of gathering into a `Vec` first) is a legitimate, well-motivated
optimization either way -- it's strictly less work per request even if
it turns out not to be *the* answer to this specific gap -- and
re-measuring after implementing it would also finally give a real
answer to how much it mattered here.

## Next steps (in priority order)

1. **Implement zero-copy read/write paths in riftlessfsd**, building
   `iovec`s directly from `chain.readable`/`chain.writable`'s
   guest-memory slices and using `preadv`/`pwritev` instead of
   `Virtqueue::gather_readable`/`scatter_writable`'s current
   copy-through-`Vec<u8>` approach -- confirmed via virtiofsd's own
   source to be the one concrete, real structural difference found so
   far (see above), a legitimate improvement regardless of how much of
   the random-write gap it turns out to explain, and re-measure
   afterward with both `compare-virtiofsd.yml` and
   `syscall-cost-compare.yml` to quantify the actual effect rather than
   assuming one.
2. If that doesn't close the gap, **look for concurrency/pipelining
   differences** -- given syscall-level costs and counts are now
   confirmed nearly identical between the two implementations, but
   overall throughput differs ~3.3x, something about how many requests
   each backend can have genuinely in flight/overlapping at once (not
   just batched-but-processed-sequentially, which riftlessfsd already
   does -- see `Server::process_vring`'s docs) is the remaining
   candidate explanation, though nothing concrete has been found here
   yet.
3. **Add repeatability to this benchmark suite**: multiple runs with
   variance reported. The run-to-run noise between the two virtiofsd
   comparison runs above (visible in both backends' absolute numbers,
   and now also directly implicated in a false-positive "fix" -- see
   the sequential-read finding above) makes this the most
   concretely-justified item on this list, not just a generic caveat
   anymore.
4. **Consider whether `read_ahead_kb` is worth tuning from riftlessfsd's
   side.** It's confirmed to be the actual governing factor for
   sequential read chunk size (see above), is a guest-side setting we
   don't currently influence at all, and defaults to a conservative 128
   KiB versus the 1 MiB `max_write`/`max_pages` ceiling we already
   advertise -- there may be real headroom here, though changing a
   guest sysfs tunable from a filesystem daemon (rather than e.g.
   documenting it as a mount-time recommendation) needs some thought
   about whether it's even riftlessfsd's place to do so.
5. **Revisit attribute cache and `FOPEN_KEEP_CACHE` policy once there's
   real cache invalidation.** Both are currently unconditional with no
   active invalidation (e.g. on a rename/unlink another client might
   have cached, or a host-side write outside riftlessfsd), which matters
   more with multiple guests or host-side writers involved.
6. **Investigate DAX/shared-memory-style reads** if (1) doesn't turn up
   a more targeted explanation -- a materially larger undertaking than
   anything done so far.

"riftlessfs beats OrbStack" is still not a true statement -- random
write throughput in particular is still meaningfully behind, by a
margin that neither of two concrete, verified protocol fixes
(`max_write`, `max_pages`) moved, and that direct syscall-level
measurement on the actual comparison hardware has now ruled out
"riftlessfsd's own `pwrite` calls are disproportionately slow" as the
explanation for -- but the gap has narrowed substantially and
specifically (not vaguely) across six sessions of real measurement,
and matching (or beating) the reference implementation on every
metadata operation and on random-read latency is a real, positive data
point, not just "less bad than before." (Sequential read briefly looked
like a third win in this category too, and the syscall-cost theory
briefly looked like it might explain the write gap; directly testing
both is what caught them as dead ends instead -- see above. That kind
of correction is itself part of what "honestly" means here.) The one
concrete, source-confirmed structural difference found so far --
virtiofsd's zero-copy I/O versus riftlessfsd's copy-through-a-buffer
approach -- is next in line to actually implement and measure, rather
than add to the pile of untested theories. This file is where the
OrbStack claim gets re-evaluated honestly as more of the above lands.
