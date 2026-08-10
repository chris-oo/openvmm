#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
INIT_SCRIPT="$SCRIPT_DIR/qemu-host-init.sh"

SOURCE_ROOTFS=
OUTPUT_ROOT=
SIZE=1024M
E2FSCK="${E2FSCK:-e2fsck}"
RESIZE2FS="${RESIZE2FS:-resize2fs}"
DEBUGFS="${DEBUGFS:-debugfs}"

usage() {
    cat <<'EOF'
Usage: build-qemu-host-rootfs.sh --source-rootfs PATH --output-root PATH [OPTIONS]

Options:
  --init-script PATH  init script to inject (default: qemu-host-init.sh)
  --size SIZE       output filesystem size (default: 1024M)
  --e2fsck PATH     e2fsck binary
  --resize2fs PATH  resize2fs binary
  --debugfs PATH    debugfs binary
  -h, --help        show this help
EOF
}

fail() {
    echo "error: $*" >&2
    exit 1
}

resolve_tool() {
    local tool=$1
    if [[ "$tool" == */* ]]; then
        [[ -x "$tool" ]] || fail "tool is not executable: $tool"
        realpath "$tool"
        return
    fi
    if command -v "$tool" >/dev/null; then
        command -v "$tool"
        return
    fi
    for directory in /usr/sbin /sbin; do
        if [[ -x "$directory/$tool" ]]; then
            realpath "$directory/$tool"
            return
        fi
    done
    fail "missing required tool: $tool"
}

require_value() {
    [[ $# -ge 2 ]] || fail "$1 requires a value"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
    --source-rootfs)
        require_value "$@"
        SOURCE_ROOTFS=$2
        shift 2
        ;;
    --output-root)
        require_value "$@"
        OUTPUT_ROOT=$2
        shift 2
        ;;
    --size)
        require_value "$@"
        SIZE=$2
        shift 2
        ;;
    --init-script)
        require_value "$@"
        INIT_SCRIPT=$2
        shift 2
        ;;
    --e2fsck)
        require_value "$@"
        E2FSCK=$2
        shift 2
        ;;
    --resize2fs)
        require_value "$@"
        RESIZE2FS=$2
        shift 2
        ;;
    --debugfs)
        require_value "$@"
        DEBUGFS=$2
        shift 2
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        fail "unknown argument: $1"
        ;;
    esac
done

[[ -n "$SOURCE_ROOTFS" ]] || fail "--source-rootfs is required"
[[ -n "$OUTPUT_ROOT" ]] || fail "--output-root is required"
[[ "$SIZE" =~ ^[1-9][0-9]*[MG]$ ]] ||
    fail "--size must use an M or G suffix, for example 1024M or 2G"
SOURCE_ROOTFS="$(realpath "$SOURCE_ROOTFS")"
INIT_SCRIPT="$(realpath "$INIT_SCRIPT")"
OUTPUT_ROOT="$(realpath -m "$OUTPUT_ROOT")"
[[ -f "$SOURCE_ROOTFS" ]] || fail "source rootfs is not a regular file"
[[ -f "$INIT_SCRIPT" ]] || fail "init script is not a regular file"
[[ "$OUTPUT_ROOT" != "/" ]] || fail "--output-root cannot be the filesystem root"
[[ "$SOURCE_ROOTFS" != "$OUTPUT_ROOT" && "$SOURCE_ROOTFS" != "$OUTPUT_ROOT/"* ]] ||
    fail "output root must not contain the source rootfs"

E2FSCK="$(resolve_tool "$E2FSCK")"
RESIZE2FS="$(resolve_tool "$RESIZE2FS")"
DEBUGFS="$(resolve_tool "$DEBUGFS")"
for tool in cmp flock python3 sha256sum stat truncate; do
    command -v "$tool" >/dev/null || fail "missing required tool: $tool"
done

OUTPUT_PARENT="$(dirname "$OUTPUT_ROOT")"
OUTPUT_NAME="$(basename "$OUTPUT_ROOT")"
mkdir -p "$OUTPUT_PARENT"
LOCK_FILE="$OUTPUT_PARENT/.${OUTPUT_NAME}.lock"
exec 9>"$LOCK_FILE"
flock -n 9 || fail "another build is using output root $OUTPUT_ROOT"
if [[ -e "$OUTPUT_ROOT" ]]; then
    [[ -f "$OUTPUT_ROOT/.openvmm-cca-qemu-rootfs" ]] ||
        fail "refusing to replace unmanaged output directory $OUTPUT_ROOT"
fi

STAGE_ROOT="$(mktemp -d "$OUTPUT_PARENT/.${OUTPUT_NAME}.stage.XXXXXXXXXX")"
cleanup_stage() {
    rm -rf -- "$STAGE_ROOT"
}
trap cleanup_stage EXIT

ROOTFS="$STAGE_ROOT/host-rootfs.ext4"
cp --reflink=auto "$SOURCE_ROOTFS" "$ROOTFS"

set +e
"$E2FSCK" -fp "$ROOTFS"
fsck_status=$?
set -e
((fsck_status <= 1)) || fail "e2fsck failed with status $fsck_status"

SIZE_PROBE="$STAGE_ROOT/size-probe"
truncate --size "$SIZE" "$SIZE_PROBE"
target_size="$(stat --format %s "$SIZE_PROBE")"
rm "$SIZE_PROBE"
current_size="$(stat --format %s "$ROOTFS")"
if ((target_size > current_size)); then
    truncate --size "$SIZE" "$ROOTFS"
    "$RESIZE2FS" "$ROOTFS"
elif ((target_size < current_size)); then
    "$RESIZE2FS" "$ROOTFS" "$SIZE"
    truncate --size "$SIZE" "$ROOTFS"
fi

cp "$INIT_SCRIPT" "$STAGE_ROOT/qemu-host-init.sh"
(
    cd "$STAGE_ROOT"
    "$DEBUGFS" -w -R "rm /etc/init.d/S99kvm-cca-interactive-host" host-rootfs.ext4 \
        >/dev/null 2>&1 || true
    "$DEBUGFS" -w -R "rm /etc/init.d/S99qemu-cca-host" host-rootfs.ext4 \
        >/dev/null 2>&1 || true
    "$DEBUGFS" -w -R "write qemu-host-init.sh /etc/init.d/S99qemu-cca-host" \
        host-rootfs.ext4
    "$DEBUGFS" -w -R "set_inode_field /etc/init.d/S99qemu-cca-host mode 0100755" \
        host-rootfs.ext4
)

INJECTED_INIT="$STAGE_ROOT/injected-qemu-host-init.sh"
"$DEBUGFS" -R "dump /etc/init.d/S99qemu-cca-host $INJECTED_INIT" "$ROOTFS"
cmp "$INIT_SCRIPT" "$INJECTED_INIT" ||
    fail "injected QEMU CCA host init script does not match its source"
rm "$INJECTED_INIT"
init_stat="$("$DEBUGFS" -R "stat /etc/init.d/S99qemu-cca-host" "$ROOTFS" 2>&1)"
grep -Eq 'Type: regular +Mode: +0755( |$)' <<<"$init_stat" ||
    fail "injected QEMU CCA host init script is not an executable regular file"

set +e
"$E2FSCK" -fp "$ROOTFS"
fsck_status=$?
set -e
((fsck_status <= 1)) || fail "post-injection e2fsck failed with status $fsck_status"

cat >"$STAGE_ROOT/manifest.txt" <<EOF
source_rootfs=$SOURCE_ROOTFS
source_rootfs_sha256=$(sha256sum "$SOURCE_ROOTFS" | awk '{print $1}')
init_script_sha256=$(sha256sum "$INIT_SCRIPT" | awk '{print $1}')
rootfs_size=$SIZE
host_rootfs_sha256=$(sha256sum "$ROOTFS" | awk '{print $1}')
EOF
touch "$STAGE_ROOT/.openvmm-cca-qemu-rootfs"

python3 - "$STAGE_ROOT" "$OUTPUT_ROOT" <<'PY'
import ctypes
import errno
import os
import sys

stage, output = map(os.fsencode, sys.argv[1:])
libc = ctypes.CDLL(None, use_errno=True)


def rename(flags):
    if libc.renameat2(-100, stage, -100, output, flags) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))


try:
    rename(1)
except OSError as error:
    if error.errno != errno.EEXIST:
        raise
    rename(2)
    if not os.path.isfile(os.path.join(stage, b".openvmm-cca-qemu-rootfs")):
        rename(2)
        raise RuntimeError(
            f"refusing to replace unmanaged output directory {os.fsdecode(output)}"
        )
PY

echo "Built QEMU CCA host rootfs: $OUTPUT_ROOT/host-rootfs.ext4"
