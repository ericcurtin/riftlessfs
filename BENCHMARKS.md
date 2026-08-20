# Benchmarks: riftlessfs vs. OrbStack

**Status: correctness-equivalent, performance behind, root causes
identified.** This is Phase 4 from the README, and the honest headline
result is: **riftlessfs does not currently beat OrbStack on these
benchmarks.** It's significantly slower on write throughput in particular.
The gap has concrete, understood causes (below), one of which was already
fixed as a direct result of running these benchmarks, and the rest are
documented as follow-up work rather than papered over.

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
claims.

## Results

| Benchmark | OrbStack | riftlessfs (before caching fix) | riftlessfs (after) |
|---|---|---|---|
| Sequential write, 512 MiB, 1 MiB blocks | 3200 MiB/s | 31.9 MiB/s | 32.2 MiB/s |
| Sequential read, 512 MiB, 1 MiB blocks | 6095 MiB/s | 824 MiB/s | 775 MiB/s |
| Random write, 128 MiB, 4 KiB blocks | 119 MiB/s (30.4k IOPS) | 32.0 MiB/s (8.2k IOPS) | 31.2 MiB/s (8.0k IOPS) |
| Random read, 128 MiB, 4 KiB blocks | 1196 MiB/s (306k IOPS) | 31.8 MiB/s (8.2k IOPS) | 31.4 MiB/s (8.1k IOPS) |
| Create 2000 files | 0.134 s | 2.952 s | 1.089 s |
| Stat 2000 files | 0.003 s | 1.997 s | **0.033 s** |
| Remove 2000 files | 0.071 s | 2.315 s | 0.997 s |
| tar create (1000 files) | 0.045 s | 1.828 s | 0.786 s |
| tar extract (1000 files) | 0.121 s | 1.680 s | 1.217 s |
| find (1000 files) | 0.006 s | 0.156 s | 0.058 s |
| rm -rf (1000 files) | 0.059 s | 0.866 s | 0.528 s |

"Before"/"after" refers to one real fix made *during* this benchmarking
pass, not a hypothetical: `riftlessfs-proto` was advertising a `0`-second
attribute/entry cache validity to the guest kernel (`fuse::wire::CACHE_TIMEOUT_SECS`),
meaning *every* `stat()`-family call became a synchronous round trip
through the whole vhost-user/FUSE pipeline. Changing it to a conservative
1 second dropped "stat 2000 files" by ~60x and gave a 1.4-2.7x improvement
across every metadata-shaped benchmark, with (as expected) no effect on
raw read/write throughput. This is a good example of the kind of gap this
whole exercise is meant to surface: it's an easy, safe, well-understood
fix (essentially every FUSE filesystem does this), and it just hadn't
been done yet because there was no data showing it mattered.

## Why write throughput is still ~100x behind

Sequential 1 MiB writes and random 4 KiB writes land at *almost the same*
MiB/s (32.2 vs. 31.2). If riftlessfs had a roughly fixed per-request
overhead, larger requests would show *much higher* throughput than small
ones (same fixed cost amortized over more bytes) -- instead, throughput is
essentially constant regardless of request size, which is the signature
of the guest kernel breaking every write into individual page-sized (4 KiB)
FUSE requests no matter what the application asked for.

That's expected, standard Linux FUSE behavior in the absence of
`FUSE_WRITEBACK_CACHE`: without it, dirty pages aren't coalesced in the
guest's page cache before being sent to the filesystem, so a 1 MiB
`write()` becomes 256 separate synchronous 4 KiB FUSE `WRITE` requests,
each paying the full virtqueue/vhost-user round-trip cost individually.
riftlessfsd doesn't currently advertise `FUSE_WRITEBACK_CACHE` (or any
optional feature flag) in its `INIT` reply -- see `fuse::wire::init_out`
and its `INIT_OUT_MINOR`/"no optional flags" rationale.

This is deliberately **not** fixed in this pass: enabling writeback
caching is a materially riskier change than the attribute-cache timeout
(the kernel takes over dirty-page/size coherency in ways that need to be
handled correctly on truncate, `fsync`, concurrent access, etc.), and
doing it carefully needs its own dedicated pass with its own tests rather
than a quick flag flip at the end of a long session. It's the clear,
concrete, high-value next step for closing the write-throughput gap, and
is the top item in "Next steps" below.

Read throughput (775-865 MiB/s) is much closer to OrbStack's territory
(6095 MiB/s) than writes are, though still meaningfully behind -- reads
benefit from the guest's normal readahead/page-cache behavior even
without writeback caching, so this gap is more likely attributable to
missing `FOPEN_KEEP_CACHE`/readahead tuning and general unoptimized
request handling (one request fully processed at a time, no pipelining)
than to a single missing feature flag.

## Next steps (in priority order)

1. **Enable `FUSE_WRITEBACK_CACHE`.** Expected to be the single highest-
   impact change based on the data above. Needs care around `SETATTR`
   (size changes), `FSYNC`, and making sure `riftlessfs-core`'s
   passthrough semantics stay correct once the kernel is coalescing
   writes on our behalf.
2. **Pipeline request processing.** The current `Server::process_vring`
   loop handles one descriptor chain fully (dispatch, syscall, encode,
   push to used ring) before looking at the next; for a `iodepth=1`
   synchronous workload like the `fio` runs above this doesn't matter
   (there's only ever one request in flight from the workload's own
   perspective), but it will matter for real-world concurrent I/O.
3. **Revisit attribute cache timeout policy.** A flat 1-second timeout
   for everything is simple but crude; consider differentiating
   (e.g. longer for directories that don't change often) once there's a
   reason to, backed by more benchmark data.
4. **Re-run this benchmark suite** after each of the above, with more
   rigor (multiple runs, variance, larger data sizes) once there's a
   credible case the write path is competitive.

Until (1) in particular lands and is re-measured, "riftlessfs beats
OrbStack" is not a true statement, and this file exists specifically so
that doesn't get asserted without the data to back it up.
