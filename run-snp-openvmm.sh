#!/bin/bash
# Run the minimal KVM SNP Linux direct-boot scenario from copied artifacts.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

OPENVMM_BIN="${OPENVMM_BIN:-$SCRIPT_DIR/openvmm}"
KERNEL="${SNP_KERNEL:-$SCRIPT_DIR/vmlinuz-6.17.0-23-generic}"
KERNEL_FORMAT="${SNP_KERNEL_FORMAT:-bzimage}"
INITRD="${SNP_INITRD:-$SCRIPT_DIR/initrd}"
# The compressed Ubuntu kernel touches memory above 64MB during early boot.
# Override with SNP_MEMORY when testing smaller/larger launch sizes.
MEMORY="${SNP_MEMORY:-128MB}"
PROCESSORS="${SNP_PROCESSORS:-1}"
KERNEL_CMDLINE="${SNP_CMDLINE:-console=ttyS0 earlyprintk=serial earlycon panic=-1}"
OPENVMM_LOG="${OPENVMM_LOG:-info,virt_kvm=trace,kvm=trace,openvmm_core::worker::dispatch=debug}"

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

exec env OPENVMM_LOG="$OPENVMM_LOG" "$OPENVMM_BIN" \
    --hypervisor kvm \
    --isolation snp \
    --kernel "$KERNEL" \
    --linux-kernel-format "$KERNEL_FORMAT" \
    --initrd "$INITRD" \
    -m "$MEMORY" \
    -p "$PROCESSORS" \
    -c "$KERNEL_CMDLINE"
