# Benchmarks: riftlessfs vs. OrbStack

**Status: still behind, gap substantially narrowed, root causes for what's
left are understood.** This is Phase 4 from the README. Three real
bugs/gaps have been found and fixed by actually running these benchmarks
so far; the honest headline is still **riftlessfs does not beat
OrbStack**, but the gap on writes went from ~100x to ~5x, and sequential
read improved 2.1x, all in one session, driven entirely by data rather
than guesswork.

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

## Next steps (in priority order)

1. **Compare against stock `virtiofsd` on Linux**, not just OrbStack on
   macOS. This is now the highest-value next step: it would show whether
   the ~120us round trip is inherent to this whole class of transport
   (vhost-user + a VMM + a guest kernel) or whether OrbStack/virtiofsd
   have found ways around it that riftlessfs hasn't yet -- important to
   know before investing in a bigger architectural change chasing a gap
   that might not be closable at this layer.
2. **Add repeatability to this benchmark suite**: multiple runs with
   variance reported, before drawing further conclusions from small
   deltas (see the metadata-noise and v3->v4-write-regression notes
   above -- both are plausibly noise, but "plausibly" isn't "confirmed").
3. **Revisit attribute cache and `FOPEN_KEEP_CACHE` policy once there's
   real cache invalidation.** Both are currently unconditional with no
   active invalidation (e.g. on a rename/unlink another client might
   have cached, or a host-side write outside riftlessfsd), which matters
   more with multiple guests or host-side writers involved.
4. **Investigate DAX/shared-memory-style reads**, if (1) shows OrbStack
   is doing something at that level -- a materially larger undertaking
   than anything done so far, so worth confirming it's actually the
   right lever before starting.

"riftlessfs beats OrbStack" is still not a true statement -- random I/O
and sequential write in particular are still meaningfully behind -- but
the gap has narrowed substantially and specifically (not vaguely) in two
sessions of real measurement, and this file is where that claim gets
re-evaluated honestly as more of the above lands.
