#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
DOCKER_CONTEXT="$SCRIPT_DIR/qemu-firmware"

BASE_IMAGE=shrinkwraptool/base-slim@sha256:dc5ac44b4cd8d8142c8df0f7fab4b5ecb11f9a399c6b94320b14f07f1476ea99
TF_RMM_REVISION=f00eac344b6f7c18abc6dad1948b07e9a82ff9f0
TF_A_REVISION=da738d5eae93af342fdc4995dd3c05acb4c9d757
EDK2_REVISION=6951dfe7d59d144a3a980bd7eda699db2d8554ac
SOURCE_DATE_EPOCH=0

OUTPUT_ROOT=
CACHE_ROOT=
JOBS=
CLEAN=false
OFFLINE=false

usage() {
    cat <<'EOF'
Usage: build-qemu-firmware.sh --output-root PATH [OPTIONS]

Build the pinned QEMU CCA TF-RMM/TF-A/EDK2 firmware stack in Docker.

Options:
  --output-root PATH  artifact output directory
  --cache-root PATH   persistent Git cache (default: <output-root>.cache)
  --jobs COUNT        parallel build jobs
  --clean             remove declared output artifacts before building
  --offline           disable container network and require populated caches
  -h, --help          show this help
EOF
}

fail() {
    echo "error: $*" >&2
    exit 1
}

require_value() {
    [[ $# -ge 2 ]] || fail "$1 requires a value"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
    --output-root)
        require_value "$@"
        OUTPUT_ROOT=$2
        shift 2
        ;;
    --cache-root)
        require_value "$@"
        CACHE_ROOT=$2
        shift 2
        ;;
    --jobs)
        require_value "$@"
        JOBS=$2
        shift 2
        ;;
    --clean)
        CLEAN=true
        shift
        ;;
    --offline)
        OFFLINE=true
        shift
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
CACHE_ROOT="${CACHE_ROOT:-$OUTPUT_ROOT.cache}"
CACHE_ROOT="$(realpath -m "$CACHE_ROOT")"
[[ "$CACHE_ROOT" != "/" ]] || fail "--cache-root cannot be the filesystem root"
[[ "$CACHE_ROOT" != "$OUTPUT_ROOT" ]] || fail "cache root must differ from output root"
[[ "$CACHE_ROOT" != "$OUTPUT_ROOT/"* ]] ||
    fail "cache root must be outside output root"

for tool in docker flock python3 realpath sha256sum; do
    command -v "$tool" >/dev/null || fail "missing required tool: $tool"
done
if [[ -n "$JOBS" && ! "$JOBS" =~ ^[1-9][0-9]*$ ]]; then
    fail "--jobs must be a positive integer"
fi
JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)}"

OUTPUT_PARENT="$(dirname "$OUTPUT_ROOT")"
OUTPUT_NAME="$(basename "$OUTPUT_ROOT")"
mkdir -p "$OUTPUT_PARENT" "$CACHE_ROOT"
exec 9>"$CACHE_ROOT/.openvmm-cca-firmware.lock"
flock 9

STAGE_ROOT="$(mktemp -d "$OUTPUT_PARENT/.${OUTPUT_NAME}.stage.XXXXXXXXXX")"
cleanup_stage() {
    rm -rf -- "$STAGE_ROOT"
}
trap cleanup_stage EXIT

DOCKERFILE_SHA="$(sha256sum "$DOCKER_CONTEXT/Dockerfile" | awk '{print $1}')"
ENTRYPOINT_SHA="$(sha256sum "$DOCKER_CONTEXT/build-in-container.sh" | awk '{print $1}')"
BASE_IMAGE_SHA="$(printf '%s' "$BASE_IMAGE" | sha256sum | awk '{print $1}')"
IMAGE_TAG="openvmm-cca-qemu-firmware:${BASE_IMAGE_SHA:0:12}-${DOCKERFILE_SHA:0:12}-${ENTRYPOINT_SHA:0:12}"

if "$OFFLINE"; then
    docker image inspect "$IMAGE_TAG" >/dev/null 2>&1 ||
        fail "offline builder image is not available locally: $IMAGE_TAG"
else
    docker build \
        --build-arg "BASE_IMAGE=$BASE_IMAGE" \
        --tag "$IMAGE_TAG" \
        "$DOCKER_CONTEXT"
fi
IMAGE_ID="$(docker image inspect "$IMAGE_TAG" --format '{{.Id}}')"

network_args=()
if "$OFFLINE"; then
    network_args=(--network none)
fi

docker run --rm \
    --pull=never \
    "${network_args[@]}" \
    --user "$(id -u):$(id -g)" \
    --env "HOME=/tmp/home" \
    --env "BUILDER_IMAGE=$IMAGE_ID" \
    --env "TF_RMM_REVISION=$TF_RMM_REVISION" \
    --env "TF_A_REVISION=$TF_A_REVISION" \
    --env "EDK2_REVISION=$EDK2_REVISION" \
    --env "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH" \
    --env "JOBS=$JOBS" \
    --env "OFFLINE=$OFFLINE" \
    --mount "type=bind,src=$CACHE_ROOT,dst=/cache" \
    --mount "type=bind,src=$STAGE_ROOT,dst=/out" \
    "$IMAGE_ID"

for artifact in rmm.img QEMU_EFI.fd bl1.bin fip.bin flash.bin manifest.txt; do
    [[ -f "$STAGE_ROOT/$artifact" && ! -L "$STAGE_ROOT/$artifact" ]] ||
        fail "firmware build did not produce a regular $artifact"
done

manifest_value() {
    local key=$1
    awk -F= -v key="$key" '$1 == key { value = substr($0, length(key) + 2); count++ }
        END { if (count == 1) print value; else exit 1 }' "$STAGE_ROOT/manifest.txt"
}

verify_manifest_value() {
    local key=$1
    local expected=$2
    local actual
    actual="$(manifest_value "$key")" ||
        fail "manifest does not contain exactly one $key field"
    [[ "$actual" == "$expected" ]] ||
        fail "manifest $key is '$actual', expected '$expected'"
}

verify_artifact_hash() {
    local artifact=$1
    local key=$2
    local expected
    local actual
    expected="$(manifest_value "$key")" ||
        fail "manifest does not contain exactly one $key field"
    actual="$(sha256sum "$STAGE_ROOT/$artifact" | awk '{print $1}')"
    [[ "$actual" == "$expected" ]] ||
        fail "$artifact hash $actual does not match manifest $expected"
}

verify_manifest_value builder_image "$IMAGE_ID"
verify_manifest_value tf_rmm_revision "$TF_RMM_REVISION"
verify_manifest_value tf_a_revision "$TF_A_REVISION"
verify_manifest_value edk2_revision "$EDK2_REVISION"
verify_artifact_hash rmm.img rmm_sha256
verify_artifact_hash QEMU_EFI.fd edk2_sha256
verify_artifact_hash bl1.bin bl1_sha256
verify_artifact_hash fip.bin fip_sha256
verify_artifact_hash flash.bin flash_sha256

cat >>"$STAGE_ROOT/manifest.txt" <<EOF
base_image=$BASE_IMAGE
dockerfile_sha256=$DOCKERFILE_SHA
entrypoint_sha256=$ENTRYPOINT_SHA
base_image_pin_sha256=$BASE_IMAGE_SHA
clean_requested=$CLEAN
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
    renameat2 = libc.renameat2
    renameat2.argtypes = [
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_uint,
    ]
    renameat2.restype = ctypes.c_int
    if renameat2(-100, stage, -100, output, 2) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
PY

echo "Built QEMU CCA firmware:"
echo "  flash:    $OUTPUT_ROOT/flash.bin"
echo "  manifest: $OUTPUT_ROOT/manifest.txt"
