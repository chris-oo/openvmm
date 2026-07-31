#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

VERSION=0.3.0-110
ARCHIVE_SHA256=90e2c2b1e8455e3f8c5797d9fd930de5a2ec5246036efc726513f8e36179df74
BINARY_SHA256=f46bde5deeae8ba6bf592bb9737220f2522f5d95cf187eaaadc3ddaba53b98f8
URL="https://github.com/microsoft/openvmm-deps/releases/download/$VERSION/qemu-linux-static.x86_64.$VERSION.tar.gz"

OUTPUT_ROOT=

usage() {
    echo "Usage: resolve-qemu.sh --output-root PATH"
}

fail() {
    echo "error: $*" >&2
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
    --output-root)
        [[ $# -ge 2 ]] || fail "--output-root requires a value"
        OUTPUT_ROOT=$2
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

[[ -n "$OUTPUT_ROOT" ]] || fail "--output-root is required"
OUTPUT_ROOT="$(realpath -m "$OUTPUT_ROOT")"
[[ "$OUTPUT_ROOT" != "/" ]] || fail "--output-root cannot be the filesystem root"
OUTPUT_PARENT="$(dirname "$OUTPUT_ROOT")"
OUTPUT_NAME="$(basename "$OUTPUT_ROOT")"
mkdir -p "$OUTPUT_PARENT"
if [[ -e "$OUTPUT_ROOT" ]]; then
    [[ -f "$OUTPUT_ROOT/.openvmm-qemu-release" ]] ||
        fail "refusing to replace unmanaged output directory $OUTPUT_ROOT"
fi

for tool in curl python3 sha256sum tar; do
    command -v "$tool" >/dev/null || fail "missing required tool: $tool"
done

STAGE_ROOT="$(mktemp -d "$OUTPUT_PARENT/.${OUTPUT_NAME}.stage.XXXXXXXXXX")"
cleanup_stage() {
    rm -rf -- "$STAGE_ROOT"
}
trap cleanup_stage EXIT

curl --fail --location --retry 4 --output "$STAGE_ROOT/qemu.tar.gz" "$URL"
echo "$ARCHIVE_SHA256  $STAGE_ROOT/qemu.tar.gz" | sha256sum --check -
tar -xzf "$STAGE_ROOT/qemu.tar.gz" -C "$STAGE_ROOT"
rm "$STAGE_ROOT/qemu.tar.gz"

QEMU="$STAGE_ROOT/qemu-system-aarch64"
[[ -f "$QEMU" && ! -L "$QEMU" ]] || fail "archive did not contain qemu-system-aarch64"
echo "$BINARY_SHA256  $QEMU" | sha256sum --check -
chmod 0755 "$QEMU"
"$QEMU" --version | sed -n '1p' >"$STAGE_ROOT/version.txt"
cat >"$STAGE_ROOT/.openvmm-qemu-release" <<EOF
version=$VERSION
archive_sha256=$ARCHIVE_SHA256
binary_sha256=$BINARY_SHA256
EOF

python3 - "$STAGE_ROOT" "$OUTPUT_ROOT" <<'PY'
import ctypes
import os
import sys

stage, output = map(os.fsencode, sys.argv[1:])
if not os.path.exists(output):
    os.rename(stage, output)
else:
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.renameat2(-100, stage, -100, output, 2) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
PY

echo "Resolved QEMU: $OUTPUT_ROOT/qemu-system-aarch64"
