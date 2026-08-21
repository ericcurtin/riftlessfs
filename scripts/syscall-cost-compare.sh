#!/usr/bin/env bash
# Narrow, purpose-built companion to compare-virtiofsd.sh: instead of the
# full benchmark suite, this wraps both daemons in `strace -T` from the
# moment they start and runs a small 4 KiB random-write/random-read fio
# job, purely to compare the two backends' own `pwrite64`/`pread64`
# syscall timing directly on the actual comparison hardware.
#
# Why a separate script rather than folding this into
# compare-virtiofsd.sh: tracing overhead (strace, or riftlessfsd's own
# RUST_LOG=trace) would distort the *throughput* numbers that script's
# main comparison reports, which need to reflect untraced, real
# performance. This script exists to answer a narrower question --
# "does riftlessfsd's `pwrite()` really cost more than `pread()` here,
# the way local (macOS/APFS) tracing found, and does virtiofsd pay a
# similar cost?" -- see BENCHMARKS.md's random-write investigation --
# without touching the numbers that script produces.
#
# Wrapping each daemon in strace from process start (rather than
# attaching to an already-running one) avoids needing to discover the
# right PID to attach to after the fact, at the cost of not being able
# to reuse compare-virtiofsd.sh's own already-running daemons -- this
# boots its own, separate, smaller guest instead.
#
# Requires the same things compare-virtiofsd.sh does, plus `strace`.
set -euo pipefail

ARCH="$(uname -m)"
WORKDIR="$(mktemp -d)"
trap 'cleanup' EXIT

PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}

log() { echo "[syscall-cost-compare] $*" >&2; }

first_existing() {
  for f in "$@"; do
    if [ -e "$f" ]; then
      echo "$f"
      return 0
    fi
  done
  return 1
}

case "$ARCH" in
  x86_64)
    QEMU_BIN=qemu-system-x86_64
    FEDORA_ARCH=x86_64
    MACHINE=q35
    FIRMWARE_CODE="$(first_existing /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/ovmf/OVMF.fd)"
    FIRMWARE_VARS_SRC="$(first_existing /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd)"
    ;;
  aarch64|arm64)
    QEMU_BIN=qemu-system-aarch64
    FEDORA_ARCH=aarch64
    MACHINE="virt,gic-version=max"
    FIRMWARE_CODE="$(first_existing /usr/share/AAVMF/AAVMF_CODE.fd /usr/share/qemu-efi-aarch64/QEMU_EFI.fd)"
    FIRMWARE_VARS_SRC="$(first_existing /usr/share/AAVMF/AAVMF_VARS.fd /usr/share/qemu-efi-aarch64/vars-template-pflash.raw)"
    ;;
  *)
    echo "unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

if [ ! -e /dev/kvm ] || [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
  echo "usable /dev/kvm required (this comparison isn't meaningful under TCG)" >&2
  exit 1
fi

if ! command -v strace >/dev/null; then
  echo "strace not found (apt-get install strace)" >&2
  exit 1
fi

VIRTIOFSD_BIN="$(first_existing /usr/libexec/virtiofsd /usr/lib/qemu/virtiofsd)" || {
  VIRTIOFSD_BIN="$(command -v virtiofsd || true)"
}
if [ -z "$VIRTIOFSD_BIN" ]; then
  echo "virtiofsd not found (apt-get install virtiofsd); looked in \$PATH, /usr/libexec, /usr/lib/qemu" >&2
  exit 1
fi
log "using virtiofsd: $VIRTIOFSD_BIN"

RIFTLESSFSD_BIN="${RIFTLESSFSD_BIN:-$(dirname "$0")/../target/release/riftlessfsd}"
if [ ! -x "$RIFTLESSFSD_BIN" ]; then
  log "building riftlessfsd (release)"
  cargo build --release -p riftlessfsd
  RIFTLESSFSD_BIN="$(dirname "$0")/../target/release/riftlessfsd"
fi
RIFTLESSFSD_BIN="$(cd "$(dirname "$RIFTLESSFSD_BIN")" && pwd)/$(basename "$RIFTLESSFSD_BIN")"

IMAGE_URL="https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/${FEDORA_ARCH}/images/Fedora-Cloud-Base-Generic-44-1.7.${FEDORA_ARCH}.qcow2"
CACHE_DIR="${RIFTLESSFS_QEMU_TEST_CACHE:-$HOME/.cache/riftlessfs-qemu-test}"
mkdir -p "$CACHE_DIR"
BASE_IMAGE="$CACHE_DIR/Fedora-Cloud-Base-44.${FEDORA_ARCH}.qcow2"
if [ ! -e "$BASE_IMAGE" ]; then
  log "downloading Fedora 44 cloud image for $FEDORA_ARCH"
  curl -fsSL -o "$BASE_IMAGE.tmp" "$IMAGE_URL"
  mv "$BASE_IMAGE.tmp" "$BASE_IMAGE"
fi

qemu-img create -f qcow2 -F qcow2 -b "$BASE_IMAGE" "$WORKDIR/disk.qcow2" >/dev/null
qemu-img resize "$WORKDIR/disk.qcow2" 10G >/dev/null

ssh-keygen -t ed25519 -N "" -f "$WORKDIR/key" -C riftlessfs-ci >/dev/null

mkdir -p "$WORKDIR/share-riftless" "$WORKDIR/share-virtiofsd"

MARKER_DONE="RIFTLESSFS_SYSCALL_COST_DONE"

# Deliberately small (a few thousand requests is enough for stable
# average syscall timing) and cold-cache (drop_caches before each read,
# same as the earlier local pread()-size investigation) so every read
# actually reaches the backend rather than being served from the
# guest's page cache.
mkdir -p "$WORKDIR/cidata"
cat > "$WORKDIR/cidata/user-data" <<EOF
#cloud-config
users:
  - name: fedora
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - $(cat "$WORKDIR/key.pub")
ssh_pwauth: false
package_update: false
runcmd:
  - dnf install -y fio
  - mkdir -p /mnt/riftless /mnt/virtiofsd
  - mount -t virtiofs riftless /mnt/riftless
  - mount -t virtiofs virtiofsd /mnt/virtiofsd
  - mkdir -p /mnt/riftless/t /mnt/virtiofsd/t
  - fio --name=w --directory=/mnt/riftless/t --rw=randwrite --bs=4k --size=16M --numjobs=1 --direct=0 --ioengine=psync > /dev/null 2>&1
  - sync
  - echo 3 > /proc/sys/vm/drop_caches
  - fio --name=r --directory=/mnt/riftless/t --rw=randread --bs=4k --size=16M --numjobs=1 --direct=0 --ioengine=psync > /dev/null 2>&1
  - fio --name=w --directory=/mnt/virtiofsd/t --rw=randwrite --bs=4k --size=16M --numjobs=1 --direct=0 --ioengine=psync > /dev/null 2>&1
  - sync
  - echo 3 > /proc/sys/vm/drop_caches
  - fio --name=r --directory=/mnt/virtiofsd/t --rw=randread --bs=4k --size=16M --numjobs=1 --direct=0 --ioengine=psync > /dev/null 2>&1
  - echo $MARKER_DONE
  - poweroff
EOF
cat > "$WORKDIR/cidata/meta-data" <<EOF
instance-id: riftlessfs-syscall-cost
local-hostname: riftlessfs-syscall-cost
EOF
( cd "$WORKDIR/cidata" && genisoimage -output ../cidata.iso -volid cidata -joliet -rock user-data meta-data >/dev/null 2>&1 )

cp "$FIRMWARE_VARS_SRC" "$WORKDIR/efi-vars.fd"

log "starting riftlessfsd under strace (shared-dir: $WORKDIR/share-riftless)"
rm -f "$WORKDIR/riftless.sock"
# Deliberately not narrowing with `-e trace=...`: the exact syscall name
# strace reports for pwrite/pread (e.g. `pwrite64` vs `pwrite`) can vary
# by architecture/libc, and this workload is small enough that tracing
# everything and filtering afterward (see summarize(), which tries
# several candidate names) is more robust than guessing the right
# `-e trace=` filter upfront.
strace -f -T -o "$WORKDIR/riftless-strace.log" -- \
  "$RIFTLESSFSD_BIN" --shared-dir "$WORKDIR/share-riftless" --socket-path "$WORKDIR/riftless.sock" \
  > "$WORKDIR/riftlessfsd.log" 2>&1 &
PIDS+=("$!")

log "starting virtiofsd under strace (shared-dir: $WORKDIR/share-virtiofsd)"
rm -f "$WORKDIR/virtiofsd.sock"
sudo strace -f -T -o "$WORKDIR/virtiofsd-strace.log" -- \
  "$VIRTIOFSD_BIN" --socket-path "$WORKDIR/virtiofsd.sock" --shared-dir "$WORKDIR/share-virtiofsd" \
  --cache=auto --writeback --sandbox none --log-level info \
  > "$WORKDIR/virtiofsd-daemon.log" 2>&1 &
PIDS+=("$!")

sleep 1
sudo chmod 666 "$WORKDIR/virtiofsd.sock"
# strace-wrapped virtiofsd writes its strace log as root; needed to read
# it back after the guest finishes.
sudo chmod 644 "$WORKDIR/virtiofsd-strace.log" 2>/dev/null || true

log "starting QEMU ($QEMU_BIN, KVM-accelerated)"
"$QEMU_BIN" \
  -M "$MACHINE" -accel kvm -cpu host -smp 4 -m 4096 \
  -object memory-backend-memfd,id=mem,size=4G,share=on \
  -numa node,memdev=mem \
  -drive if=pflash,format=raw,file="$FIRMWARE_CODE",readonly=on \
  -drive if=pflash,format=raw,file="$WORKDIR/efi-vars.fd" \
  -drive if=virtio,file="$WORKDIR/disk.qcow2",format=qcow2 \
  -drive if=virtio,file="$WORKDIR/cidata.iso",format=raw \
  -netdev user,id=net0 -device virtio-net-pci,netdev=net0 \
  -chardev socket,id=char-riftless,path="$WORKDIR/riftless.sock" \
  -device vhost-user-fs-pci,queue-size=1024,chardev=char-riftless,tag=riftless \
  -chardev socket,id=char-virtiofsd,path="$WORKDIR/virtiofsd.sock" \
  -device vhost-user-fs-pci,queue-size=1024,chardev=char-virtiofsd,tag=virtiofsd \
  -nographic -serial file:"$WORKDIR/serial.log" -monitor none \
  > "$WORKDIR/qemu.log" 2>&1 &
QEMU_PID=$!
PIDS+=("$QEMU_PID")

log "waiting for guest to finish (up to 10 minutes)"
for _ in $(seq 1 600); do
  if grep -q "$MARKER_DONE" "$WORKDIR/serial.log" 2>/dev/null; then
    log "guest finished"
    break
  fi
  if ! kill -0 "$QEMU_PID" 2>/dev/null; then
    log "FAIL: QEMU exited before reporting completion"
    cat "$WORKDIR/qemu.log" >&2
    exit 1
  fi
  sleep 1
done

if ! grep -q "$MARKER_DONE" "$WORKDIR/serial.log" 2>/dev/null; then
  log "FAIL: timed out waiting for guest"
  tail -n 100 "$WORKDIR/serial.log" >&2
  exit 1
fi

# Give strace a moment to flush its log after the traced process exits
# (QEMU shutting down closes the vhost-user connection, but the daemon
# process itself may take an instant to notice and exit).
sleep 1

summarize() {
  local label="$1" file="$2"
  shift 2
  local candidates=("$@")
  # strace -T output lines look like:
  #   pwrite64(7, "...", 4096, 12288) = 4096 <0.000042>
  # Try each candidate syscall name (exact match up to the opening
  # paren, so e.g. "pwrite(" doesn't also match "pwritev(") until one
  # yields results, then extract the trailing <seconds> field with
  # grep -oE and reduce with a plain, portable awk (avoiding
  # gawk-specific match()-with-array, since the default `awk` on Ubuntu
  # runners may be mawk, which doesn't support it).
  local name matches
  for name in "${candidates[@]}"; do
    matches="$(grep -hE "(^|[[:space:]])${name}\(" "$file" 2>/dev/null || true)"
    if [ -n "$matches" ]; then
      break
    fi
  done
  if [ -z "$matches" ]; then
    echo "$label ${candidates[0]}: no matching syscalls found (tried: ${candidates[*]})"
    # Diagnostic fallback: list what write/read-related syscalls *did*
    # occur, with counts, so a wrong guess at the candidate list doesn't
    # require another round-trip through CI to find the actual name
    # (this is exactly what caught virtiofsd using pwritev64/preadv64
    # instead of pwrite64/pread64 -- see BENCHMARKS.md).
    echo "  (syscalls containing 'write' or 'read' actually seen in this trace:)"
    grep -ohE '[a-z_0-9]*(write|read)[a-z_0-9]*\(' "$file" 2>/dev/null |
      sed 's/(//' | sort | uniq -c | sort -rn | sed 's/^/    /'
    return
  fi
  echo "$matches" | grep -oE '<[0-9.]+>$' | tr -d '<>' | awk -v label="$label" -v sc="$name" '
    {
      n++
      sum += $1
      if (n == 1 || $1 > max) max = $1
      if (n == 1 || $1 < min) min = $1
    }
    END {
      if (n > 0) {
        printf "%s %s: n=%d avg=%.1fus min=%.1fus max=%.1fus\n", label, sc, n, (sum/n)*1e6, min*1e6, max*1e6
      } else {
        printf "%s %s: no matching syscalls found in trace\n", label, sc
      }
    }
  '
}

echo "============================================================"
echo "syscall timing (strace -T, wall time per call):"
echo "============================================================"
# riftlessfsd uses plain pwrite()/pread() (see
# riftlessfs-core::passthrough); virtiofsd (fuse-backend-rs) uses
# vectored I/O -- specifically `pwritev2` (not `pwritev`/`pwritev64`)
# and `preadv` (not `preadv64`) -- instead, discovered by this script's
# own diagnostic fallback across its first two runs against real
# hardware -- see BENCHMARKS.md. Trying multiple forms for both
# binaries rather than hardcoding only what each is *currently* known
# to call.
summarize "riftlessfsd" "$WORKDIR/riftless-strace.log" pwrite64 pwrite pwritev2 pwritev pwritev64
summarize "riftlessfsd" "$WORKDIR/riftless-strace.log" pread64 pread preadv preadv64
summarize "virtiofsd  " "$WORKDIR/virtiofsd-strace.log" pwritev2 pwritev pwritev64 pwrite64 pwrite
summarize "virtiofsd  " "$WORKDIR/virtiofsd-strace.log" preadv preadv64 pread64 pread
