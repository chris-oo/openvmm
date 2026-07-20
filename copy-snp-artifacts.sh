#!/bin/bash
# Copy OpenVMM plus the Linux direct-boot artifacts to an SNP-capable host for
# the current KVM SNP bring-up path.

set -euo pipefail

HOST="${SNP_HOST:-cho-snp-ubuntu}"
DEST="${SNP_DEST:-~/snp-openvmm}"

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

OPENVMM_BIN="${OPENVMM_BIN:-$REPO_ROOT/target/x86_64-unknown-linux-gnu/debug/openvmm}"
LINUX_KERNEL="${SNP_KERNEL:-$REPO_ROOT/vmlinuz-6.17.0-23-generic}"
LINUX_INITRD="${SNP_INITRD:-$REPO_ROOT/target/vmm_tests/x64/initrd}"
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
# check_artifact "Linux bzImage kernel" "$LINUX_KERNEL" "copy vmlinuz-6.17.0-23-generic to the repo root or set SNP_KERNEL"
check_artifact "Linux direct-boot initrd" "$LINUX_INITRD" "build or restore the vmm-tests initrd, or set SNP_INITRD"
check_artifact "SNP run helper" "$RUN_SCRIPT" "restore or recreate run-snp-openvmm.sh at the repo root"

if (( missing != 0 )); then
    echo >&2
    echo "Common setup:" >&2
    echo "  cargo xflowey restore-packages" >&2
    echo "  cargo build --target x86_64-unknown-linux-gnu -p openvmm" >&2
    exit 1
fi

ssh "$HOST" "mkdir -p $DEST"
scp "$OPENVMM_BIN" "$HOST:$DEST/openvmm.new"
ssh "$HOST" "mv -f $DEST/openvmm.new $DEST/openvmm"
ssh "$HOST" "rm -f $DEST/openhcl.bin"
# scp "$LINUX_KERNEL" "$HOST:$DEST/vmlinuz-6.17.0-23-generic.new"
# ssh "$HOST" "mv -f $DEST/vmlinuz-6.17.0-23-generic.new $DEST/vmlinuz-6.17.0-23-generic"
scp "$LINUX_INITRD" "$HOST:$DEST/initrd.new"
ssh "$HOST" "mv -f $DEST/initrd.new $DEST/initrd"
scp "$RUN_SCRIPT" "$HOST:$DEST/run-snp-openvmm.sh.new"
ssh "$HOST" "mv -f $DEST/run-snp-openvmm.sh.new $DEST/run-snp-openvmm.sh"

echo "Copied SNP artifacts to $HOST:$DEST"
echo "Run with:"
echo "  ssh -t $HOST '$DEST/run-snp-openvmm.sh'"
