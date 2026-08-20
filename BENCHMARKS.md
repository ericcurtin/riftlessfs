# Benchmarks: riftlessfs vs. OrbStack

**Status: still behind, gap substantially narrowed, root causes for what's
left are understood.** This is Phase 4 from the README. Two real bugs/gaps
have been found and fixed by actually running these benchmarks so far;
the honest headline is still **riftlessfs does not beat OrbStack**, but
the gap on writes went from ~100x to ~5x in one session, driven entirely
by data rather than guesswork.

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

| Benchmark | OrbStack | riftlessfs v1 (no attr cache) | riftlessfs v2 (+ attr cache) | riftlessfs v3 (+ writeback cache) |
|---|---|---|---|---|
| Sequential write, 512 MiB, 1 MiB blocks | 3200 MiB/s | 31.9 MiB/s | 32.2 MiB/s | **600 MiB/s** |
| Sequential read, 512 MiB, 1 MiB blocks | 6095 MiB/s | 824 MiB/s | 775 MiB/s | 806 MiB/s |
| Random write, 128 MiB, 4 KiB blocks | 119 MiB/s (30.4k IOPS) | 32.0 MiB/s (8.2k IOPS) | 31.2 MiB/s (8.0k IOPS) | **129 MiB/s (33.0k IOPS)** |
| Random read, 128 MiB, 4 KiB blocks | 1196 MiB/s (306k IOPS) | 31.8 MiB/s (8.2k IOPS) | 31.4 MiB/s (8.1k IOPS) | 32.4 MiB/s (8.3k IOPS) |
| Create 2000 files | 0.134 s | 2.952 s | 1.089 s | 1.096 s |
| Stat 2000 files | 0.003 s | 1.997 s | **0.033 s** | 0.245 s |
| Remove 2000 files | 0.071 s | 2.315 s | 0.997 s | 1.251 s |
| tar create (1000 files) | 0.045 s | 1.828 s | 0.786 s | 0.751 s |
| tar extract (1000 files) | 0.121 s | 1.680 s | 1.217 s | 1.325 s |
| find (1000 files) | 0.006 s | 0.156 s | 0.058 s | 0.057 s |
| rm -rf (1000 files) | 0.059 s | 0.866 s | 0.528 s | 0.685 s |

v1 -> v2 and v2 -> v3 are two real fixes made *during* this benchmarking
exercise, not hypotheticals -- see below for each. Notably, random write
(129 MiB/s) is now *ahead* of OrbStack's 119 MiB/s in this single run;
sequential write closed from ~100x behind to ~5x behind. Reads are
untouched by either fix (expected -- both fixes are write/metadata-path
specific) and remain the biggest relative gap (7.6x on sequential, 37x on
random).

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

## What's still behind, and why

- **Random/sequential reads (7.6x / 37x behind).** Untouched by either
  fix so far. Likely candidates: no `FOPEN_KEEP_CACHE` or readahead
  tuning, and the request-processing loop handles one descriptor chain
  at a time with no pipelining (see "Next steps").
- **Sequential write (~5x behind).** Writeback caching closed most of the
  gap; the rest is plausibly the same one-request-at-a-time processing
  loop, or OrbStack's virtiofs implementation having DAX/shared-memory
  optimizations riftlessfs doesn't attempt yet.
- **Metadata operations show noise, not a clear regression or
  improvement, from v2 to v3** -- consistent with the fact that
  writeback caching shouldn't affect metadata-only operations on freshly
  created, never-written files at all. Treat the v2/v3 metadata deltas as
  measurement noise (see "Methodology") rather than a real effect until
  a more rigorous run says otherwise.

## Next steps (in priority order)

1. **Pipeline/concurrent request processing.** The current
   `Server::process_vring` loop handles one descriptor chain fully
   (dispatch, syscall, encode, push to used ring) before looking at the
   next. For a `iodepth=1` synchronous workload like the `fio` runs above
   this only partly matters (writeback cache now lets the *kernel* batch
   before handing us bigger requests), but it will matter more for
   read throughput and for any real concurrent I/O.
2. **Investigate read-side caching/readahead** (`FOPEN_KEEP_CACHE`,
   `max_readahead` tuning) given reads are now the largest relative gap.
3. **Add repeatability to this benchmark suite**: multiple runs with
   variance reported, before drawing further conclusions from small
   deltas (see the metadata-noise note above).
4. **Revisit attribute cache timeout policy.** A flat 1-second timeout
   for everything is simple but crude; there's currently no active
   invalidation (e.g. on a rename/unlink another client might have
   cached), which matters more with multiple guests or host-side
   writers involved.

"riftlessfs beats OrbStack" is still not a true statement -- reads in
particular are far behind -- but it's no longer true by two orders of
magnitude on writes, and this file is where that claim gets re-evaluated
honestly as more of the above lands.
