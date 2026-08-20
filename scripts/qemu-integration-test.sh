#!/usr/bin/env bash
# End-to-end integration test: boot a real Fedora Linux 44 guest under QEMU,
# attach riftlessfsd as a vhost-user-fs backend, mount it inside the guest,
# and verify real file operations round-trip correctly.
#
# This is the automated version of the manual verification described in
# the workspace README's "How this was actually verified" section. It's
# Linux-only for now: Linux distributions' packaged QEMU has vhost-user
# support enabled by default, whereas e.g. Homebrew's macOS QEMU build
# does not (see the README) -- reproducing this on macOS CI would require
# building QEMU from source there too, which is possible (also documented
# in the README) but not automated yet.
#
# Requires: qemu-system-{x86_64,aarch64}, cloud-image firmware
# (OVMF/AAVMF), genisoimage or xorriso, curl. On Debian/Ubuntu:
#   sudo apt-get install -y qemu-system-x86 qemu-system-arm ovmf \
#     qemu-efi-aarch64 genisoimage
set -euo pipefail

ARCH="$(uname -m)"
WORKDIR="$(mktemp -d)"
trap 'cleanup' EXIT

RIFTLESSFSD_PID=""
QEMU_PID=""

cleanup() {
  [ -n "$QEMU_PID" ] && kill "$QEMU_PID" 2>/dev/null || true
  [ -n "$RIFTLESSFSD_PID" ] && kill "$RIFTLESSFSD_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}

log() { echo "[qemu-integration-test] $*" >&2; }


# Find the first existing file among several candidates. Ubuntu's OVMF/
# AAVMF packaging has changed the exact firmware filenames across
# releases (e.g. plain "OVMF_CODE.fd" vs. the newer 4M-flash-sized
# "OVMF_CODE_4M.fd"), so probe for what's actually there rather than
# hardcoding one path.
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
    ACCEL_KVM="-accel kvm"
    FIRMWARE_CODE="$(first_existing /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/ovmf/OVMF.fd)"
    FIRMWARE_VARS_SRC="$(first_existing /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd)"
    ;;
  aarch64|arm64)
    QEMU_BIN=qemu-system-aarch64
    FEDORA_ARCH=aarch64
    MACHINE="virt,gic-version=max"
    ACCEL_KVM="-accel kvm -cpu host"
    FIRMWARE_CODE="$(first_existing /usr/share/AAVMF/AAVMF_CODE.fd /usr/share/qemu-efi-aarch64/QEMU_EFI.fd)"
    FIRMWARE_VARS_SRC="$(first_existing /usr/share/AAVMF/AAVMF_VARS.fd /usr/share/qemu-efi-aarch64/vars-template-pflash.raw)"
    ;;
  *)
    echo "unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

if [ -z "$FIRMWARE_CODE" ] || [ -z "$FIRMWARE_VARS_SRC" ]; then
  echo "could not locate UEFI firmware (OVMF/AAVMF) files; searched standard package locations" >&2
  echo "looked under /usr/share/OVMF, /usr/share/ovmf, /usr/share/AAVMF, /usr/share/qemu-efi-aarch64" >&2
  find /usr/share -iname '*ovmf*' -o -iname '*aavmf*' -o -iname '*qemu-efi*' 2>/dev/null >&2 || true
  exit 1
fi
log "using firmware code=$FIRMWARE_CODE vars=$FIRMWARE_VARS_SRC"

if [ ! -e /dev/kvm ]; then
  log "warning: /dev/kvm not available, falling back to TCG (much slower)"
  ACCEL="-accel tcg"
else
  ACCEL="$ACCEL_KVM"
fi

RIFTLESSFSD_BIN="${RIFTLESSFSD_BIN:-$(dirname "$0")/../target/release/riftlessfsd}"
if [ ! -x "$RIFTLESSFSD_BIN" ]; then
  log "building riftlessfsd (release)"
  cargo build --release -p riftlessfsd
  RIFTLESSFSD_BIN="$(dirname "$0")/../target/release/riftlessfsd"
fi

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

mkdir -p "$WORKDIR/cidata"
SHARED_DIR="$WORKDIR/shared"
mkdir -p "$SHARED_DIR"
echo "hello from host" > "$SHARED_DIR/hello.txt"

MARKER_DONE="RIFTLESSFS_QEMU_TEST_DONE"
MARKER_MOUNT_FAIL="RIFTLESSFS_QEMU_TEST_MOUNT_FAILED"

cat > "$WORKDIR/cidata/user-data" <<EOF
#cloud-config
users:
  - name: fedora
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - $(cat "$WORKDIR/key.pub")
ssh_pwauth: false
runcmd:
  - mkdir -p /mnt/rfs
  - >
    mount -t virtiofs myfs /mnt/rfs
    && echo hello from guest > /mnt/rfs/from_guest.txt
    && diff <(echo "hello from host") /mnt/rfs/hello.txt
    && dd if=/dev/urandom of=/tmp/bigfile bs=1M count=8
    && cp /tmp/bigfile /mnt/rfs/bigfile
    && sync
    && [ "\$(sha256sum </tmp/bigfile)" = "\$(sha256sum </mnt/rfs/bigfile)" ]
    && rm /mnt/rfs/bigfile
    && echo $MARKER_DONE
    || echo $MARKER_MOUNT_FAIL
  - poweroff
EOF
cat > "$WORKDIR/cidata/meta-data" <<EOF
instance-id: riftlessfs-ci
local-hostname: riftlessfs-ci
EOF
( cd "$WORKDIR/cidata" && genisoimage -output ../cidata.iso -volid cidata -joliet -rock user-data meta-data >/dev/null 2>&1 )

cp "$FIRMWARE_VARS_SRC" "$WORKDIR/efi-vars.fd"

log "starting riftlessfsd"
rm -f "$WORKDIR/rfs.sock"
"$RIFTLESSFSD_BIN" --shared-dir "$SHARED_DIR" --socket-path "$WORKDIR/rfs.sock" \
  > "$WORKDIR/riftlessfsd.log" 2>&1 &
RIFTLESSFSD_PID=$!
sleep 1

log "starting QEMU ($QEMU_BIN, accel: $ACCEL)"
# shellcheck disable=SC2086
"$QEMU_BIN" \
  -M "$MACHINE" $ACCEL -smp 2 -m 2048 \
  -object memory-backend-file,id=mem,size=2G,mem-path="$WORKDIR/qemu-mem",share=on \
  -numa node,memdev=mem \
  -drive if=pflash,format=raw,file="$FIRMWARE_CODE",readonly=on \
  -drive if=pflash,format=raw,file="$WORKDIR/efi-vars.fd" \
  -drive if=virtio,file="$WORKDIR/disk.qcow2",format=qcow2 \
  -drive if=virtio,file="$WORKDIR/cidata.iso",format=raw \
  -netdev user,id=net0 -device virtio-net-pci,netdev=net0 \
  -chardev socket,id=char0,path="$WORKDIR/rfs.sock" \
  -device vhost-user-fs-pci,chardev=char0,tag=myfs,queue-size=128 \
  -nographic -serial file:"$WORKDIR/serial.log" -monitor none \
  > "$WORKDIR/qemu.log" 2>&1 &
QEMU_PID=$!

log "waiting for guest to finish (up to 5 minutes)"
for _ in $(seq 1 300); do
  if grep -q "$MARKER_DONE" "$WORKDIR/serial.log" 2>/dev/null; then
    log "PASS: guest reported success"
    grep -q "hello from guest" "$SHARED_DIR/from_guest.txt" || {
      log "FAIL: host-side shared dir missing guest-written file"
      exit 1
    }
    log "PASS: host-side shared dir has the guest-written file"
    exit 0
  fi
  if grep -q "$MARKER_MOUNT_FAIL" "$WORKDIR/serial.log" 2>/dev/null; then
    log "FAIL: guest reported mount/test failure"
    tail -n 100 "$WORKDIR/serial.log" >&2
    tail -n 100 "$WORKDIR/riftlessfsd.log" >&2
    exit 1
  fi
  if ! kill -0 "$QEMU_PID" 2>/dev/null; then
    log "FAIL: QEMU exited before reporting success"
    cat "$WORKDIR/qemu.log" >&2
    exit 1
  fi
  sleep 1
done

log "FAIL: timed out waiting for guest"
tail -n 100 "$WORKDIR/serial.log" >&2
exit 1
