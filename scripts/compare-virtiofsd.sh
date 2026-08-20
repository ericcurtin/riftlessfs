#!/usr/bin/env bash
# Head-to-head benchmark: riftlessfsd vs. stock virtiofsd, same host, same
# QEMU invocation, same Fedora 44 guest kernel, same benchmark workload,
# both mounted simultaneously in one boot. This is the automated version
# of the "compare against stock virtiofsd on Linux" item in
# BENCHMARKS.md's next steps -- unlike the OrbStack comparison (which
# necessarily also differs in VM management, macOS-vs-Linux host, etc),
# this isolates the vhost-user-fs *backend implementation* as the only
# variable.
#
# Requires a Linux host with usable /dev/kvm (this is not useful under
# TCG -- both backends would be dominated by emulation overhead, not
# their own implementation differences), plus everything
# qemu-integration-test.sh requires, plus a `virtiofsd` binary (Ubuntu:
# `apt-get install virtiofsd`).
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

log() { echo "[compare-virtiofsd] $*" >&2; }

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

BENCH_SCRIPT="$(cd "$(dirname "$0")" && pwd)/bind-mount-benchmark.sh"

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

# Two independent shared directories, one per backend, each holding a
# copy of the benchmark script so the guest can run it without needing
# network access or a second copy baked into cloud-init.
mkdir -p "$WORKDIR/share-riftless" "$WORKDIR/share-virtiofsd"
cp "$BENCH_SCRIPT" "$WORKDIR/share-riftless/bench.sh"
cp "$BENCH_SCRIPT" "$WORKDIR/share-virtiofsd/bench.sh"

MARKER_DONE="RIFTLESSFS_COMPARE_DONE"

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
  - bash /mnt/riftless/bench.sh /mnt/riftless riftlessfs > /mnt/riftless/results.txt 2>&1
  - bash /mnt/virtiofsd/bench.sh /mnt/virtiofsd virtiofsd > /mnt/virtiofsd/results.txt 2>&1
  - echo $MARKER_DONE
  - poweroff
EOF
cat > "$WORKDIR/cidata/meta-data" <<EOF
instance-id: riftlessfs-compare
local-hostname: riftlessfs-compare
EOF
( cd "$WORKDIR/cidata" && genisoimage -output ../cidata.iso -volid cidata -joliet -rock user-data meta-data >/dev/null 2>&1 )

cp "$FIRMWARE_VARS_SRC" "$WORKDIR/efi-vars.fd"

log "starting riftlessfsd (shared-dir: $WORKDIR/share-riftless)"
rm -f "$WORKDIR/riftless.sock"
"$RIFTLESSFSD_BIN" --shared-dir "$WORKDIR/share-riftless" --socket-path "$WORKDIR/riftless.sock" \
  > "$WORKDIR/riftlessfsd.log" 2>&1 &
PIDS+=("$!")

log "starting virtiofsd (shared-dir: $WORKDIR/share-virtiofsd)"
rm -f "$WORKDIR/virtiofsd.sock"
sudo "$VIRTIOFSD_BIN" --socket-path "$WORKDIR/virtiofsd.sock" --shared-dir "$WORKDIR/share-virtiofsd" \
  --cache=auto --writeback --sandbox none --log-level info \
  > "$WORKDIR/virtiofsd-daemon.log" 2>&1 &
PIDS+=("$!")

sleep 1
# virtiofsd creates its socket as root; QEMU (running as this user) needs
# to be able to connect to it.
sudo chmod 666 "$WORKDIR/virtiofsd.sock"

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

echo "============================================================"
echo "riftlessfsd results:"
echo "============================================================"
cat "$WORKDIR/share-riftless/results.txt" || echo "(missing -- see $WORKDIR/serial.log)"
echo
echo "============================================================"
echo "virtiofsd results:"
echo "============================================================"
cat "$WORKDIR/share-virtiofsd/results.txt" || echo "(missing -- see $WORKDIR/serial.log)"
