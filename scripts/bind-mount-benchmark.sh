#!/usr/bin/env bash
# A small, reproducible benchmark suite for comparing bind-mount-style
# filesystem performance across backends (riftlessfs, OrbStack, stock
# virtiofsd, ...). Run the *same* copy of this script, inside the *same*
# guest OS if at all possible, against each mount you want to compare --
# see BENCHMARKS.md for how it was used to compare riftlessfs against
# OrbStack, and for the results and their analysis.
#
# Usage: bind-mount-benchmark.sh <mountpoint> <label>
#
# Requires: fio, python3, tar, find. No bc/awk-float-heroics beyond
# integer nanosecond timestamps, since minimal guest images (this was
# developed against a Fedora Cloud image) may not have `bc` installed.
set -euo pipefail
MNT="$1"
LABEL="$2"
WORK="$MNT/bench-$LABEL-$$"
rm -rf "$WORK"
mkdir -p "$WORK"
cd "$WORK"

echo "=== $LABEL ==="

echo "--- fio: sequential write 512MiB, bs=1M ---"
fio --name=seqwrite --directory="$WORK" --rw=write --bs=1M --size=512M \
  --numjobs=1 --direct=0 --ioengine=psync --group_reporting --output-format=normal 2>&1 | grep -E "WRITE:|write:"

echo "--- fio: sequential read 512MiB, bs=1M ---"
fio --name=seqread --directory="$WORK" --rw=read --bs=1M --size=512M \
  --numjobs=1 --direct=0 --ioengine=psync --group_reporting --output-format=normal 2>&1 | grep -E "READ:|read :"

echo "--- fio: random write 128MiB, bs=4k ---"
fio --name=randwrite --directory="$WORK" --rw=randwrite --bs=4k --size=128M \
  --numjobs=1 --direct=0 --ioengine=psync --group_reporting --output-format=normal 2>&1 | grep -E "WRITE:|write:|IOPS"

echo "--- fio: random read 128MiB, bs=4k ---"
fio --name=randread --directory="$WORK" --rw=randread --bs=4k --size=128M \
  --numjobs=1 --direct=0 --ioengine=psync --group_reporting --output-format=normal 2>&1 | grep -E "READ:|read :|IOPS"

rm -f "$WORK"/seqwrite* "$WORK"/seqread* "$WORK"/randwrite* "$WORK"/randread*

echo "--- metadata: create/stat/rm 2000 files (single-process, no fork/exec per op) ---"
# Deliberately not a bash loop calling external stat/rm binaries: with
# 2000 iterations, fork/exec overhead (a few hundred microseconds each)
# would dominate the measurement far more than the filesystem operations
# actually being tested.
mkdir -p "$WORK/meta"
python3 - "$WORK/meta" <<'PYEOF'
import os, sys, time
d = sys.argv[1]
n = 2000

t0 = time.monotonic()
for i in range(n):
    os.close(os.open(os.path.join(d, f"f{i}"), os.O_CREAT | os.O_WRONLY, 0o644))
t1 = time.monotonic()
print(f"create {n} files: {t1 - t0:.3f} s")

t0 = time.monotonic()
for i in range(n):
    os.stat(os.path.join(d, f"f{i}"))
t1 = time.monotonic()
print(f"stat {n} files: {t1 - t0:.3f} s")

t0 = time.monotonic()
for i in range(n):
    os.remove(os.path.join(d, f"f{i}"))
t1 = time.monotonic()
print(f"rm {n} files: {t1 - t0:.3f} s")
PYEOF
rmdir "$WORK/meta"

echo "--- synthetic source tree: create, tar, untar, find, rm ---"
mkdir -p "$WORK/srctree"
for d in $(seq 1 100); do
  mkdir -p "$WORK/srctree/dir$d"
  for f in $(seq 1 10); do
    head -c 2048 /dev/urandom > "$WORK/srctree/dir$d/file$f.dat"
  done
done

START=$(date +%s%N)
tar cf "$WORK/srctree.tar" -C "$WORK" srctree
END=$(date +%s%N)
awk -v s="$START" -v e="$END" 'BEGIN{printf "tar create (1000 files): %.3f s\n", (e-s)/1000000000}'

rm -rf "$WORK/srctree"

START=$(date +%s%N)
tar xf "$WORK/srctree.tar" -C "$WORK"
END=$(date +%s%N)
awk -v s="$START" -v e="$END" 'BEGIN{printf "tar extract (1000 files): %.3f s\n", (e-s)/1000000000}'

START=$(date +%s%N)
find "$WORK/srctree" -type f | wc -l > /dev/null
END=$(date +%s%N)
awk -v s="$START" -v e="$END" 'BEGIN{printf "find (1000 files): %.3f s\n", (e-s)/1000000000}'

START=$(date +%s%N)
rm -rf "$WORK/srctree" "$WORK/srctree.tar"
END=$(date +%s%N)
awk -v s="$START" -v e="$END" 'BEGIN{printf "rm -rf tree: %.3f s\n", (e-s)/1000000000}'

cd "$MNT"
rmdir "$WORK" 2>/dev/null || rm -rf "$WORK"
echo "=== done: $LABEL ==="
