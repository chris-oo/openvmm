#!/bin/bash
# Run the minimal KVM SNP Linux direct-boot scenario from copied artifacts.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

OPENVMM_BIN="${OPENVMM_BIN:-$SCRIPT_DIR/openvmm}"
KERNEL="${SNP_KERNEL:-$SCRIPT_DIR/vmlinuz-6.17.0-23-generic}"
KERNEL_FORMAT="${SNP_KERNEL_FORMAT:-bzimage}"
INITRD="${SNP_INITRD:-$SCRIPT_DIR/initrd}"
# need 160 mb with compressed kernel and initrd
MEMORY="${SNP_MEMORY:-160MB}"
PROCESSORS="${SNP_PROCESSORS:-2}"
KERNEL_CMDLINE="${SNP_CMDLINE:-console=ttyS0 earlyprintk=serial earlycon panic=-1}"
OPENVMM_LOG="${OPENVMM_LOG:-info,virt_kvm=trace,kvm=trace,openvmm_core::worker::dispatch=debug}"
VIRTIO_BLK_SIZE="${SNP_VIRTIO_BLK_SIZE:-64M}"

if [[ ! -f "$OPENVMM_BIN" ]]; then
    echo "ERROR: missing required artifact: $OPENVMM_BIN" >&2
    exit 1
fi

for file in "$KERNEL" "$INITRD"; do
    if [[ ! -f "$file" ]]; then
        echo "ERROR: missing required artifact: $file" >&2
        exit 1
    fi
done

cmd=(
    env "OPENVMM_LOG=$OPENVMM_LOG" "$OPENVMM_BIN"
    --hypervisor kvm
    --isolation snp
    --com1 console
    --pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255,low_mmio=4M,high_mmio=1G
    --pcie-root-port rc0:blk
    --virtio-blk "mem:$VIRTIO_BLK_SIZE,pcie_port=blk"
    --kernel "$KERNEL"
    --linux-kernel-format "$KERNEL_FORMAT"
    --initrd "$INITRD"
    -m "$MEMORY"
    -p "$PROCESSORS"
    -c "$KERNEL_CMDLINE"
)

printf 'Running:'
printf ' %q' "${cmd[@]}"
printf '\n'

exec "${cmd[@]}"
