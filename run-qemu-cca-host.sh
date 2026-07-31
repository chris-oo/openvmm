#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
QEMU_CCA_ROOT="${QEMU_CCA_ROOT:-$REPO_ROOT/target/cca-qemu}"
CCA_TEST_ROOT="${CCA_TEST_ROOT:-$REPO_ROOT/target/cca-test}"

QEMU_BIN="${QEMU_BIN:-$QEMU_CCA_ROOT/qemu/qemu-system-aarch64}"
FIRMWARE="${QEMU_CCA_FIRMWARE:-$QEMU_CCA_ROOT/firmware/flash.bin}"
HOST_KERNEL="${QEMU_CCA_HOST_KERNEL:-$CCA_TEST_ROOT/cca-kernels-v15/host-Image}"
HOST_ROOTFS="${QEMU_CCA_HOST_ROOTFS:-$CCA_TEST_ROOT/kvm-cca/rootfs.ext2}"
SHARE_DIR="${QEMU_CCA_SHARE_DIR:-$CCA_TEST_ROOT/kvm-cca/share}"
LOG_DIR="${QEMU_CCA_LOG_DIR:-$QEMU_CCA_ROOT/logs}"
MEMORY="${QEMU_CCA_MEMORY:-2G}"
PROCESSORS="${QEMU_CCA_PROCESSORS:-1}"
CPU="${QEMU_CCA_CPU:-max,x-rme=on,lpa2=off,sme=off,pauth-impdef=on}"

for path in "$QEMU_BIN" "$FIRMWARE" "$HOST_KERNEL" "$HOST_ROOTFS"; do
    [[ -f "$path" ]] || {
        echo "error: required file is missing: $path" >&2
        exit 1
    }
done
[[ -d "$SHARE_DIR" ]] || {
    echo "error: required share directory is missing: $SHARE_DIR" >&2
    exit 1
}
mkdir -p "$LOG_DIR"

exec "$QEMU_BIN" \
    -nodefaults \
    -accel tcg \
    -M virt,secure=on,virtualization=on,gic-version=3,acpi=off \
    -cpu "$CPU" \
    -m "$MEMORY" \
    -smp "$PROCESSORS" \
    -bios "$FIRMWARE" \
    -kernel "$HOST_KERNEL" \
    -drive "if=none,id=rootfs,format=raw,file=$HOST_ROOTFS" \
    -device virtio-blk-pci,drive=rootfs,romfile= \
    -virtfs "local,path=$SHARE_DIR,mount_tag=FM,security_model=none,readonly=off" \
    -netdev user,id=net0 \
    -device virtio-net-pci,netdev=net0,romfile= \
    -append "nokaslr root=/dev/vda rw console=ttyAMA0" \
    -display none \
    -serial stdio \
    -serial "file:$LOG_DIR/secondary-console.log" \
    -no-reboot
