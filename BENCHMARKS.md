# Benchmarks: riftlessfs vs. OrbStack and stock virtiofsd

**Status: still behind OrbStack overall, but ahead on one real benchmark
(random write), with sequential write's gap cut from 10x to 2.6x by
fixing a real benchmark methodology bug (the test VM was memory-starved
relative to OrbStack's default), and random read now clearly the single
largest remaining gap.** This is Phase 4 from the README. Six real
bugs/gaps have been found and fixed by actually running these
benchmarks so far, plus one methodology bug in the benchmarks
themselves; the honest headline is still **riftlessfs does not beat
OrbStack overall**, but sequential write is down to 2.6x behind (from
an apparent 10x, most of which turned out to be an unfair 2 GiB vs. 16
GiB VM memory comparison -- see "Fix 5" below), sequential read is down
to ~2x behind (from 7.6x originally), random write is measured *ahead*
of OrbStack consistently across two independent 3-run samples, and a
same-hardware comparison against the *reference* vhost-user-fs
implementation (stock `virtiofsd` on real Linux + KVM) found and ruled
out several specific theories for what remains, narrowing it down to a
confirmed, real architectural difference
(zero-copy I/O) worth acting on next.

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

## Fix 4 (v4 -> v5): `max_write`/`max_pages`/`max_background`, re-checked against the actual target

Everything from here through the `syscall-cost-compare` investigation
above was measured against stock `virtiofsd`, not OrbStack -- a
deliberate choice to isolate the backend implementation as a variable
(see that section's intro), but one with a real risk: two genuine,
verified fixes (`max_write` 128 KiB -> 1 MiB, `FUSE_MAX_PAGES`/
`max_pages`, plus `max_background`/`congestion_threshold` raised and
`Virtqueue::MAX_CHAIN_LEN` 128 -> 512, all landed together) shipped and
were measured extensively against virtiofsd, but were **never
re-checked against OrbStack -- the thing this project is actually
trying to beat** -- until now. Re-running the original v1-v4
methodology (same script, same local Apple Silicon Mac, OrbStack and
riftlessfsd back to back) after those fixes, and this time 3 runs per
side instead of 1 (a real, if partial, start on the long-standing
"add repeatability" item):

| Benchmark | riftlessfs v4 (single run) | OrbStack v5 (min/avg/max, n=3) | riftlessfs v5 (min/avg/max, n=3) | v5 ratio (avg/avg) |
|---|---|---|---|---|
| Sequential write, 512 MiB, 1 MiB blocks | 557 MiB/s | 3631 / 4461 / 5689 MiB/s | 305 / 448 / 718 MiB/s | 10.0x behind |
| Sequential read, 512 MiB, 1 MiB blocks | 1673 MiB/s | 5818 / 6199 / 6827 MiB/s | 2798 / 3442 / 4267 MiB/s | **1.8x behind** (was 3.6x) |
| Random write, 128 MiB, 4 KiB blocks | 90.1 MiB/s | 115 / 118.7 / 123 MiB/s | 192 / 277 / 421 MiB/s | **riftlessfs ahead (2.3x)** |
| Random read, 128 MiB, 4 KiB blocks | 29.4 MiB/s | 934 / 1017 / 1067 MiB/s | 64.0 / 66.4 / 67.7 MiB/s | 15.3x behind (was 24.6x) |
| Create 2000 files | 1.241 s | 0.115 / 0.119 / 0.126 s | 0.435 / 0.600 / 0.759 s | 5.0x behind (was 9.3x) |
| Stat 2000 files | 0.281 s | ~0.002 s (all 3 runs) | 0.075 / 0.104 / 0.121 s | 52x behind (was 94x) -- tiny absolute times |
| Remove 2000 files | 1.363 s | 0.063 / 0.065 / 0.067 s | 0.316 / 0.405 / 0.457 s | 6.3x behind (was 19.2x) |
| tar create (1000 files) | 0.517 s | 0.042 / 0.044 / 0.045 s | 0.160 / 0.210 / 0.242 s | 4.8x behind (was 11.5x) |
| tar extract (1000 files) | 1.441 s | 0.100 / 0.105 / 0.108 s | 0.430 / 0.445 / 0.452 s | 4.3x behind (was 11.9x) |
| find (1000 files) | 0.061 s | ~0.006 s (all 3 runs) | 0.019 / 0.023 / 0.029 s | 3.8x behind (was 10.2x) |
| rm -rf (1000 files) | 0.775 s | 0.055 / 0.056 / 0.058 s | 0.213 / 0.241 / 0.292 s | 4.3x behind (was 13.1x) |

Verified correctness held throughout (not just inferred from
throughput numbers): a real 32 MiB `dd` + `cp` + `sha256sum` round trip
through the mount matched on both sides both before and after this
round of measurement.

**Real, substantial, mechanistically-explained wins:**

- **Sequential read: 3.6x -> 1.8x behind.** `max_write`/`max_pages`
  raising the writeback/readahead batching ceiling from 128 KiB to
  1 MiB (see the `virtiofsd` comparison section above, where the same
  fix was verified to change actual `pread()`/`pwrite()` sizes on the
  wire) means far fewer, larger requests for the same sequential
  transfer -- exactly the kind of workload that ceiling was raised for.
- **Random write: 1.32x behind -> ~2.3x *ahead*.** This is the biggest
  surprise in this update, and worth being precise about *why* it's
  plausible: raising `max_background`/`congestion_threshold` (from
  effectively minimal values to `u16::MAX`/`(u16::MAX/4)*3`, matching
  virtiofsd) lets the guest kernel keep vastly more dirty,
  writeback-cached pages in flight before throttling the writer, which
  matters most exactly for a small-request, high-concurrency workload
  like 4 KiB random writes with writeback caching enabled -- previously
  the writer was being throttled far earlier. This is a *different*
  lever from `max_write`/`max_pages` (which govern request *size*, not
  how many can be outstanding), landed in the same commit, and hadn't
  been isolated as mattering until this OrbStack-side re-check --  the
  virtiofsd comparison's random-write numbers didn't move from this
  same commit (see above), which in hindsight makes sense if virtiofsd
  was never as background-request-limited to begin with, so raising a
  ceiling it wasn't hitting anyway wouldn't show up there the way it
  does here.
- **Random read: 24.6x -> 15.3x behind.** Smaller, and less obviously
  explained by these specific fixes (a true random-read workload has no
  writeback/readahead batching to benefit from) -- plausibly a
  secondary effect of the same virtqueue changes (`MAX_CHAIN_LEN`
  128 -> 512 headroom), or partly noise; still far behind and still the
  single biggest remaining gap by ratio.

**Needs an honest caveat, not just celebration:** run-to-run variance
is real and visible here too (riftlessfs's own random write ranged
192-421 MiB/s across 3 runs, a 2.2x spread; OrbStack's sequential write
ranged 3631-5689 MiB/s, a 1.6x spread) -- 3 runs is better than 1, but
still not enough to fully trust a single average, especially for the
random-write "ahead" result, which is the most consequential claim
here and rests on comparing two noisy distributions (118.7 vs. 277
MiB/s average, but riftlessfs's *minimum* observed run, 192 MiB/s, is
still clearly above OrbStack's *maximum*, 123 MiB/s -- so despite the
noise, the two distributions don't actually overlap, which is a
meaningfully stronger claim than the averages alone would suggest).
Sequential write's ratio getting *worse* (5.7x -> 10.0x) is mostly
OrbStack's own number moving (3200 -> 4461 average) rather than a
riftlessfs regression (its own absolute numbers, 305-718 MiB/s, are
comparable to or better than v4's single 557 MiB/s run) -- a good
illustration of why ratios against a moving, uncontrolled comparison
point need the underlying absolute numbers checked before drawing
conclusions from the ratio alone.

**The metadata operations all show a fairly uniform ~2x *improvement*
in their ratio** (e.g. remove: 19.2x -> 6.3x behind) despite none of
this round's fixes touching metadata code paths at all -- the most
likely explanation is that this reflects general system-load/thermal
variance between measurement sessions on shared dev hardware (see
"Methodology"'s existing noise caveats) rather than a real effect, and
should not be attributed to `max_write`/`max_pages`/`max_background`.
Flagging this rather than either quietly using the improved numbers or
digging for a causal story that isn't there.

## Fix 5 (v5 -> v6): the test VM itself was memory-starved -- a benchmark methodology bug, not a code fix

v5's single biggest unexplained gap was sequential write at 10.0x
behind. Rather than guess at another protocol-level fix, it was
profiled the same way earlier gaps were: tracing real `pwrite()` sizes
and `Server::process_vring` batch sizes (the same instrumentation used
throughout this document) against a live guest running a 256 MiB
sequential-write fio job.

That trace turned up something not about riftlessfsd's protocol
handling at all: writes were arriving in **small batches (mostly 4-5
requests per wake-up)**, a sharp contrast to random write's
**100-200+ requests per wake-up** (see the writeback-batching trace
earlier in this document). Small batches mean many more separate
external wake-ups, each paying the full round-trip cost individually
-- directly explaining disproportionately low throughput for a
workload that, on paper, should be the *easiest* case (large,
already-1-MiB requests, no more `max_write`/`max_pages` headroom to
gain).

Investigating *why* batches were small led to the guest's dirty-page
writeback thresholds (`vm.dirty_ratio`, `vm.dirty_background_ratio`)
-- and from there, to the actual root cause: **the local test VM used
for every riftlessfs measurement in this document, including all of
v1-v5, was booted with `-m 2048` (2 GiB RAM), while OrbStack's actual
default machine reports 16 GiB (`/proc/meminfo` on a fresh
`orb create`).** With 2 GiB RAM and default ratios, the guest's
dirty-page budget before forced writeback is only ~200-400 MiB --
smaller than the 512 MiB sequential-write test itself, forcing
frequent, small, partial-range flushes throughout the test. With 16
GiB RAM, the same test's data comfortably fits under the budget,
letting the guest batch far more before flushing. **This was an unfair
comparison baked into the benchmark setup, not a riftlessfs
performance bug** -- every prior sequential-write number in this
document was measured with roughly 1/8th of OrbStack's default memory
allocation.

Confirmed by direct, isolated A/B testing on the same guest, changing
only the VM's `-m`/memory-backend-file size (not `vm.dirty_ratio`,
not `-smp`, nothing else) and always dropping caches between runs:

| | 2 GiB VM (matches v1-v5) | 16 GiB VM (matches OrbStack's default) |
|---|---|---|
| Sequential write, 256 MiB, 1 MiB blocks | 314 MiB/s | 1249 MiB/s (4.0x) |

(An earlier version of this experiment also tried manually raising
`vm.dirty_ratio`/`vm.dirty_background_ratio` on the 2 GiB VM instead of
fixing the memory size, and got a similar-looking improvement --
254 -> 444 MiB/s. That's a real, reproducible effect too, but it's the
*wrong fix to standardize on*: it was papering over the actual
mismatch (memory size) with a different knob that happens to move the
same underlying resource. Matching memory directly, and leaving
`vm.dirty_ratio` at its default, gave a *larger* improvement (4.0x vs.
1.75x) with one less variable changed.)

**Re-ran the full v1-v5 methodology with only this fixed** (16 GiB VM,
`-smp` left at 2 to match v5 exactly -- an earlier draft of this re-run
also bumped `-smp` to 4 at the same time, which muddied several
unrelated metrics; redone with a single changed variable), OrbStack
re-measured fresh alongside it (3 runs each side, same as v5):

| Benchmark | OrbStack v6 (min/avg/max, n=3) | riftlessfs v6 (min/avg/max, n=3) | v6 ratio (avg/avg) | v5 ratio (for comparison) |
|---|---|---|---|---|
| Sequential write, 512 MiB, 1 MiB blocks | 4231 / 5109 / 5818 MiB/s | 1193 / 1940 / 2381 MiB/s | **2.6x behind** | 10.0x behind |
| Sequential read, 512 MiB, 1 MiB blocks | 5818 / 6316 / 6649 MiB/s | 1875 / 3032 / 4376 MiB/s | 2.1x behind | 1.8x behind |
| Random write, 128 MiB, 4 KiB blocks | 110 / 118.3 / 128 MiB/s | 202 / 217 / 229 MiB/s | **riftlessfs ahead (1.8x)** | riftlessfs ahead (2.3x) |
| Random read, 128 MiB, 4 KiB blocks | 1000 / 1067 / 1185 MiB/s | 66.0 / 69.5 / 75.0 MiB/s | 15.4x behind | 15.3x behind |
| Create 2000 files | 0.113 / 0.116 / 0.120 s | 0.421 / 0.442 / 0.469 s | 3.8x behind | 5.0x behind |
| Stat 2000 files | ~0.002 s | 0.072 / 0.075 / 0.079 s | 37.5x behind -- tiny absolute times | 52x behind |
| Remove 2000 files | 0.064 / 0.065 / 0.067 s | 0.357 s (all 3 runs) | 5.5x behind | 6.3x behind |
| tar create (1000 files) | 0.042 / 0.044 / 0.045 s | 0.163 / 0.209 / 0.233 s | 4.8x behind | 4.8x behind |
| tar extract (1000 files) | 0.102 / 0.105 / 0.108 s | 0.443 / 0.537 / 0.713 s | 5.1x behind | 4.3x behind |
| find (1000 files) | 0.005 / 0.006 / 0.006 s | 0.020 / 0.023 / 0.029 s | 3.8x behind | 3.8x behind |
| rm -rf (1000 files) | 0.053 / 0.055 / 0.059 s | 0.194 / 0.203 / 0.215 s | 3.7x behind | 4.3x behind |

Verified correctness held throughout with a real 64 MiB `dd` + `cp` +
`sha256sum` round trip matching on both sides, on top of the existing
suite.

**What actually moved, and what didn't, tells a clean story:**

- **Sequential write: 10.0x -> 2.6x behind.** By far the largest single
  change in this entire document, and it's a benchmark-setup fix, not
  a code change. This means most of what looked like "riftlessfs is
  fundamentally ~10x slower at sequential write" in every prior version
  of this document was actually "riftlessfs was tested with 1/8th the
  RAM of what it was being compared against."
- **Random write, random read, and metadata: all within noise of v5**
  (random write's ratio moved from 2.3x to 1.8x ahead, but its
  *absolute* numbers on both sides are consistent with run-to-run
  variance already documented; random read is unchanged, as expected,
  since a true random read workload doesn't build up dirty pages and
  so was never affected by this VM's memory ceiling). This is exactly
  the pattern predicted before re-running: the 512 MiB *sequential*
  write test is the one workload in this suite big enough to exceed
  the 2 GiB VM's dirty-page budget; the 128 MiB random-write test
  isn't (even at the tighter 2 GiB/default-ratio ~200 MiB threshold),
  and reads don't dirty pages at all. The clean, mechanistic match
  between "which benchmark was large enough to hit the memory ceiling"
  and "which benchmark's ratio changed" is good evidence this
  explanation is right, not a coincidence.
- **Sequential read got nominally worse (1.8x -> 2.1x behind)**,
  likely just noise (3 runs, and this workload was never expected to
  be memory-ceiling-sensitive the way sequential write is) -- flagging
  rather than either dismissing or over-explaining a small change in
  the "wrong" direction.

**A methodological lesson worth stating plainly, matching this
document's established pattern of correcting itself rather than
quietly moving on**: this VM memory mismatch existed in *every*
riftlessfs-vs-OrbStack number in this document before this section,
including the original v1-v4 baseline. It was never questioned because
the comparison felt fair on its face (same script, same host, same
guest OS, back to back) -- but "fair" also requires checking that both
sides get comparable *resources*, which wasn't verified until a
protocol-level trace investigation happened to lead there. Given how
much this one variable mattered, `scripts/qemu-integration-test.sh`
and any future local test setup should default to a more realistic VM
memory size (or the comparison methodology should explicitly document
and justify whatever size is chosen) rather than reusing whatever
value was convenient for a quick correctness check.

## What's still behind, and why (updated for v6)

- **Random read (15.4x behind) is now, clearly, the single largest
  gap**, and the one this document understands the least -- it isn't
  explained by request size (`max_write`/`max_pages`, ruled out),
  syscall cost (ruled out via direct measurement against virtiofsd),
  or VM memory sizing (ruled out here: reads don't dirty pages, and
  the numbers didn't move). Per "Where the per-request latency
  actually goes" below, riftlessfsd's own processing is not the
  bottleneck for small requests -- external round-trip latency and
  (per the `virtiofsd` zero-copy finding above) an extra host-side
  memory copy per request are the remaining candidates, neither fully
  quantified against OrbStack specifically yet. This is the most
  promising place to focus next, precisely because so many other
  explanations have already been eliminated.
- **Sequential write (2.6x behind) improved dramatically** once tested
  fairly, and the remainder is a much smaller, more plausible gap
  (transport-level or zero-copy-related, same open questions as random
  read) rather than something looking structurally broken.
- **Random write remains ahead**, consistently across two independent
  3-run samples (v5 and v6) with non-overlapping min/max ranges both
  times -- the most solid result in this document.
- **Metadata operations are consistently in the 3.7-5.5x-behind range**
  (excluding `stat`, whose ratio is inflated by comparing
  millisecond-scale absolute times) across both v5 and v6 -- stable
  enough now to treat as a real, if secondary, gap rather than pure
  noise, though still not tied to a specific identified cause.

### What OrbStack's random-read number implies about its transport

A true `iodepth=1` random-read `fio` job (like this suite's) is
synchronous: each `read()` call blocks until the previous one
completes, so IOPS translates directly into an implied per-request
latency (`1 / IOPS` seconds). Doing that arithmetic on the v6 numbers:

- OrbStack: ~1067 MiB/s at 4 KiB blocks -> ~273,000 IOPS -> **~3.7 us
  implied per-request latency**.
- riftlessfs: ~69.5 MiB/s at 4 KiB blocks -> ~17,800 IOPS -> **~56 us
  implied per-request latency** (consistent, same order of magnitude,
  with the ~120 us figure measured directly by tracing in "Where the
  per-request latency actually goes" below and in the `virtiofsd`
  comparison -- both point at a round-trip cost in the tens-to-low-
  hundreds of microseconds for this class of transport).

**~3.7 microseconds is not a plausible round-trip time for a real
message exchange with a separate userspace process**, through *any*
vhost-user-style mechanism -- a VM exit, a context switch into a
different process, that process doing work and replying, a VM entry,
and the guest kernel resuming, even highly optimized, costs tens of
microseconds at minimum on real virtualization hardware (this is
exactly what the `virtiofsd` comparison measured directly: ~120 us,
and separately confirmed that riftlessfsd's own share of that is only
~2 us -- the rest is inherent to the round-trip mechanism itself, not
either backend's implementation). ~3.7 us *is*, however, a very
plausible cost for a guest-side page fault against host memory that's
directly mapped into the guest's address space -- i.e., DAX-style
shared memory, where a "read" becomes a plain memory access (with the
underlying page potentially already resident, or populated by a fault
handler) rather than a message sent to and answered by a separate
process.

This is strong, quantitative (not just "it's probably fancier")
evidence that OrbStack's random-read advantage specifically comes from
*not paying a per-request round-trip cost at all* for reads, rather
than from a faster or more efficient version of the same
request/response mechanism riftlessfs and virtiofsd both use. If
that's right, no amount of protocol-level tuning on riftlessfs's
current transport (larger requests, zero-copy, fewer syscalls) can
close this specific gap -- all of those still pay the same per-request
round-trip tax this arithmetic says OrbStack has largely eliminated.
Matching it would require riftlessfs to implement the same category of
mechanism (DAX/shared-memory reads, `Next steps` item below), which is
a materially larger undertaking than anything implemented in this
project so far -- not a specific missing flag or constant.

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

### Implemented it. Measured effect: none clearly visible, as predicted.

Implemented the zero-copy path: `Virtqueue::iovecs_from` builds
`iovec`s directly from a chain's guest-memory segments (handling a
header that doesn't land on a descriptor boundary, and capping to a
request's declared size), `PassthroughFs::read_vectored`/
`write_vectored` call `preadv`/`pwritev` with them, and
`Server::process_vring` routes `WRITE`/`READ` through this path
(falling back to the original gather-into-`Vec` path for every other
opcode, and for `WRITE`/`READ` themselves if header parsing ever
fails). Verified correct well beyond unit tests: a real daemon through
the local QEMU/HVF guest, with `sha256sum`-verified round trips at the
exact 1 MiB `max_write` boundary, a 64 MiB multi-request transfer, an
odd (12345-byte, non-page-aligned) size, and EOF-adjacent reads.

This was flagged in advance as *not* expected to close the
random-write gap specifically -- the byte-copy avoided is
sub-microsecond at the sizes involved, dwarfed by the tens-of-
microseconds-per-syscall and ~120 us-per-round-trip costs measured
elsewhere in this document. Measuring it directly (both comparisons
re-run against the exact same commit, same methodology as before)
confirmed that prediction rather than just assuming it:

| | before (v6 / prior virtiofsd run) | after (zero-copy) |
|---|---|---|
| vs. OrbStack, random write | ~1.8-2.3x ahead | ~1.7x ahead (still solidly ahead, same ballpark) |
| vs. OrbStack, random read | ~15.3-15.4x behind | ~17.0x behind (within this comparison's established noise band) |
| vs. virtiofsd, random write | ~3.3x behind | ~3.3x behind (unchanged) |
| vs. virtiofsd, random read | ~parity/slightly ahead | ~parity/slightly ahead (unchanged) |
| vs. virtiofsd, sequential write | ~2.1-2.15x behind | ~2.95x behind (single run; within the range already seen for this specific noisy metric, not a confirmed regression) |

**No clearly-attributable improvement or regression in either
comparison.** This isn't a null result in the sense of "the work was
wasted" -- the change is real, correct, verified, and does strictly
less work per request (one copy instead of two for large payloads) --
it's a null result in the sense of "this wasn't where the bottleneck
was," matching the analysis above and the earlier discovery that
virtiofsd (which already has zero-copy) doesn't beat riftlessfsd on
random read either. Kept because it's a legitimate improvement on its
own terms (less CPU work per request, no new attack surface, unlike
the DAX alternative), not because it moved a benchmark number.

## Next steps (in priority order)

Zero-copy I/O (previously listed here) is now implemented and
measured -- see above -- with no clearly-attributable effect on any
benchmark in either comparison. That leaves DAX as the one remaining
concrete, quantitative hypothesis for random read specifically, and
the least-tried, most-eliminated-alternatives item overall.

1. **Investigate DAX/shared-memory-style reads for real**, backed by a
   specific number to validate against rather than "OrbStack is
   probably fancier": if a working implementation doesn't get
   random-read's per-request latency down from ~56 us into single-digit
   microseconds, the DAX hypothesis itself was wrong and needs revisiting,
   not just an implementation detail. This is a materially larger
   undertaking than anything else in this project so far (vhost-user
   shared-memory-region negotiation, `FUSE_SETUPMAPPING`/
   `REMOVEMAPPING` support that riftlessfsd doesn't have at all today,
   and real security-surface implications of mapping host memory
   directly into a guest) -- likely worth its own design pass before
   writing code, not a quick follow-up.
2. **Add more repeatability to this benchmark suite.** Fix 5 already
   added 3-run min/avg/max reporting for the OrbStack comparison (a
   real improvement over the historical single run), but 3 is still a
   small sample -- and the `-smp` confound encountered while
   investigating Fix 5, plus the zero-copy measurement above landing
   well inside pre-existing noise bands in both directions, are
   concrete reminders that ad hoc re-runs can introduce new confounds
   as easily as they control for old ones, and that small effect sizes
   need more than 3 runs to trust either way. Worth scripting the whole
   OrbStack comparison (both sides, N repeats, VM parameters pinned
   explicitly) rather than doing it by hand each time.
3. **Consider whether `read_ahead_kb` is worth tuning/documenting from
   riftlessfsd's side.** Confirmed to be the actual governing factor for
   sequential read chunk size (see above), is a guest-side setting
   riftlessfsd doesn't currently influence at all, and defaults to a
   conservative 128 KiB versus the 1 MiB `max_write`/`max_pages`
   ceiling already advertised -- there may be real headroom here, same
   category of fix as Fix 5's memory-sizing finding (a guest/deployment
   tunable, not a protocol change).
4. **Revisit attribute cache and `FOPEN_KEEP_CACHE` policy once there's
   real cache invalidation.** Both are currently unconditional with no
   active invalidation (e.g. on a rename/unlink another client might
   have cached, or a host-side write outside riftlessfsd), which matters
   more with multiple guests or host-side writers involved.
5. If DAX turns out infeasible or doesn't pan out as hypothesized,
   **look for concurrency/pipelining differences** as a fallback --
   given syscall-level costs and counts are already confirmed nearly
   identical to virtiofsd's, but overall write throughput still
   differs there, something about how many requests each backend can
   have genuinely in flight/overlapping at once is a candidate worth
   checking against OrbStack too, though nothing concrete has been
   found here yet.

"riftlessfs beats OrbStack" is still not a true statement overall --
**random read remains ~15-17x behind**, by a margin nothing tried so
far has moved (request size, syscall cost, VM memory sizing, and now
zero-copy I/O are all specifically ruled out as the explanation) --
but the rest of the
picture has changed substantially since that sentence was first
written: **random write measures consistently ahead of OrbStack**
across two independent 3-run samples, and **sequential write's gap
went from an apparent 10x to 2.6x** once a real flaw in this document's
own benchmark setup (an unfairly memory-starved test VM) was found and
fixed -- not a code change, but exactly the kind of correction this
document has tried to make a habit of, same as the sequential-read and
`pwrite`-cost dead ends recorded above. Three of five benchmark
categories (random write, sequential write, and -- more provisionally
-- sequential read) are now either ahead or within reach; one category
(metadata) is a stable, secondary, ~4-5x gap; and one (random read) has
a specific, quantitative, but *not yet validated or acted on*
hypothesis (DAX/shared-memory reads bypassing the per-request round
trip entirely -- see above) rather than being a mystery. That's a
materially more specific and more encouraging state than "still
behind" implies, even though it remains true. This file is where the
OrbStack claim gets re-evaluated honestly as more of the above lands.
