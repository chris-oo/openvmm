#!/bin/bash
# Copy OpenVMM plus the OpenHCL kernel package artifacts used by IGVM builds to
# an SNP-capable host for the current Linux-direct KVM SNP bring-up path.

set -euo pipefail

HOST="${SNP_HOST:-cho-snp-ubuntu}"
DEST="${SNP_DEST:-~/snp-openvmm}"

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

OPENVMM_BIN="${OPENVMM_BIN:-$REPO_ROOT/target/x86_64-unknown-linux-gnu/debug/openvmm}"
OPENHCL_KERNEL="${SNP_KERNEL:-$REPO_ROOT/.packages/underhill-deps-private/x64/vmlinux}"
OPENHCL_INITRD="${SNP_INITRD:-$REPO_ROOT/.packages/underhill-deps-private/x64/initrd}"
RUN_SCRIPT="$REPO_ROOT/run-snp-openvmm.sh"

echo "Building OpenVMM..."
cargo build --target x86_64-unknown-linux-gnu -p openvmm

missing=0

check_artifact() {
    local name="$1"
    local path="$2"
    local fix="$3"

    if [[ ! -f "$path" ]]; then
        if (( missing == 0 )); then
            echo "ERROR: missing required SNP artifact(s):" >&2
        fi
        missing=1
        echo "  $name: $path" >&2
        echo "    fix: $fix" >&2
    fi
}

check_artifact "OpenVMM binary" "$OPENVMM_BIN" "cargo build --target x86_64-unknown-linux-gnu -p openvmm"
check_artifact "OpenHCL kernel package vmlinux" "$OPENHCL_KERNEL" "cargo xflowey restore-packages"
check_artifact "OpenHCL kernel package initrd" "$OPENHCL_INITRD" "cargo xflowey restore-packages"
check_artifact "SNP run helper" "$RUN_SCRIPT" "restore or recreate run-snp-openvmm.sh at the repo root"

if (( missing != 0 )); then
    echo >&2
    echo "Common setup:" >&2
    echo "  cargo xflowey restore-packages" >&2
    echo "  cargo build --target x86_64-unknown-linux-gnu -p openvmm" >&2
    exit 1
fi

ssh "$HOST" "mkdir -p $DEST"
scp "$OPENVMM_BIN" "$HOST:$DEST/openvmm"
ssh "$HOST" "rm -f $DEST/openhcl.bin"
scp "$OPENHCL_KERNEL" "$HOST:$DEST/vmlinux"
scp "$OPENHCL_INITRD" "$HOST:$DEST/initrd"
scp "$RUN_SCRIPT" "$HOST:$DEST/run-snp-openvmm.sh"

echo "Copied SNP artifacts to $HOST:$DEST"
echo "Run with:"
echo "  ssh -t $HOST '$DEST/run-snp-openvmm.sh'"
