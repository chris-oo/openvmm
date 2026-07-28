#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
HOST_FRAGMENT="$SCRIPT_DIR/host.config"
GUEST_FRAGMENT="$SCRIPT_DIR/guest.config"

SOURCE=
REVISION=
OUTPUT_ROOT=
CROSS_COMPILE=aarch64-linux-gnu-
JOBS=
CLEAN=false

usage() {
    cat <<'EOF'
Usage: build-kernels.sh --source PATH --revision COMMIT --output-root PATH [OPTIONS]

Build the FVP normal-world host and Realm guest kernels from one pinned
linux-cca source tree.

Options:
  --source PATH          linux-cca source tree
  --revision COMMIT      exact source commit required
  --output-root PATH     output root outside the source tree
  --cross-compile PREFIX cross compiler prefix (default: aarch64-linux-gnu-)
  --jobs COUNT           parallel build jobs (default: available CPUs)
  --clean                remove the two named output directories first
  -h, --help             show this help
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
    --source)
        require_value "$@"
        SOURCE=$2
        shift 2
        ;;
    --revision)
        require_value "$@"
        REVISION=$2
        shift 2
        ;;
    --output-root)
        require_value "$@"
        OUTPUT_ROOT=$2
        shift 2
        ;;
    --cross-compile)
        require_value "$@"
        CROSS_COMPILE=$2
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
    -h | --help)
        usage
        exit 0
        ;;
    *)
        fail "unknown argument: $1"
        ;;
    esac
done

[[ -n "$SOURCE" ]] || fail "--source is required"
[[ -n "$REVISION" ]] || fail "--revision is required"
[[ -n "$OUTPUT_ROOT" ]] || fail "--output-root is required"

SOURCE="$(realpath "$SOURCE")"
OUTPUT_ROOT="$(realpath -m "$OUTPUT_ROOT")"
[[ "$OUTPUT_ROOT" != "/" ]] || fail "--output-root cannot be the filesystem root"
HOST_OUTPUT="$(realpath -m "$OUTPUT_ROOT/cca-fvp-host")"
GUEST_OUTPUT="$(realpath -m "$OUTPUT_ROOT/cca-realm-guest")"
BUILD_SOURCE="$(realpath -m "$OUTPUT_ROOT/.openvmm-cca-linux-source")"
BUILD_SOURCE_MARKER="$BUILD_SOURCE/.openvmm-source-revision"

[[ -f "$SOURCE/Makefile" ]] || fail "$SOURCE is not a Linux source tree"
[[ -x "$SOURCE/scripts/kconfig/merge_config.sh" ]] ||
    fail "$SOURCE is missing scripts/kconfig/merge_config.sh"
[[ "$OUTPUT_ROOT" != "$SOURCE" && "$OUTPUT_ROOT" != "$SOURCE/"* ]] ||
    fail "--output-root must be outside the source tree"
[[ "$SOURCE" != "$HOST_OUTPUT" && "$SOURCE" != "$HOST_OUTPUT/"* ]] ||
    fail "--source cannot be inside the managed host output directory"
[[ "$SOURCE" != "$GUEST_OUTPUT" && "$SOURCE" != "$GUEST_OUTPUT/"* ]] ||
    fail "--source cannot be inside the managed guest output directory"
[[ "$SOURCE" != "$BUILD_SOURCE" && "$SOURCE" != "$BUILD_SOURCE/"* ]] ||
    fail "--source cannot be inside the managed source cache"

for tool in git make sha256sum realpath tar "${CROSS_COMPILE}gcc"; do
    command -v "$tool" >/dev/null || fail "missing required tool: $tool"
done

if [[ -z "$JOBS" ]]; then
    JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"
fi
[[ "$JOBS" =~ ^[1-9][0-9]*$ ]] || fail "--jobs must be a positive integer"

HEAD_REVISION="$(git -C "$SOURCE" rev-parse HEAD)"
EXPECTED_REVISION="$(git -C "$SOURCE" rev-parse "$REVISION^{commit}")"
[[ "$HEAD_REVISION" == "$EXPECTED_REVISION" ]] ||
    fail "source is at $HEAD_REVISION, expected $EXPECTED_REVISION"

if [[ -n "$(git -C "$SOURCE" status --porcelain --untracked-files=no)" ]]; then
    fail "source tree has tracked modifications"
fi

if "$CLEAN"; then
    rm -rf -- "$HOST_OUTPUT" "$GUEST_OUTPUT"
fi
mkdir -p "$OUTPUT_ROOT"

# Export the committed bytes instead of building the worktree. This avoids
# host Git settings such as core.autocrlf changing executable kernel scripts.
if [[ -e "$BUILD_SOURCE" ]]; then
    [[ -f "$BUILD_SOURCE_MARKER" ]] ||
        fail "$BUILD_SOURCE exists but is not an OpenVMM CCA source cache"
    CACHED_REVISION="$(<"$BUILD_SOURCE_MARKER")"
    if [[ "$CACHED_REVISION" != "$EXPECTED_REVISION" ]]; then
        rm -rf -- "$BUILD_SOURCE" "$HOST_OUTPUT" "$GUEST_OUTPUT"
    fi
fi
if [[ ! -e "$BUILD_SOURCE" ]]; then
    SOURCE_STAGE="$(mktemp -d "$OUTPUT_ROOT/.openvmm-cca-linux-source.XXXXXXXXXX")"
    cleanup_source_stage() {
        if [[ -n "${SOURCE_STAGE:-}" && -d "$SOURCE_STAGE" ]]; then
            rm -rf -- "$SOURCE_STAGE"
        fi
    }
    trap cleanup_source_stage EXIT
    git -C "$SOURCE" archive "$EXPECTED_REVISION" | tar -x -C "$SOURCE_STAGE"
    printf '%s\n' "$EXPECTED_REVISION" >"$SOURCE_STAGE/.openvmm-source-revision"
    mv "$SOURCE_STAGE" "$BUILD_SOURCE"
    SOURCE_STAGE=
    trap - EXIT
fi
mkdir -p "$HOST_OUTPUT" "$GUEST_OUTPUT"

SOURCE_EPOCH="$(git -C "$SOURCE" show -s --format=%ct "$EXPECTED_REVISION")"
export KBUILD_BUILD_TIMESTAMP="@$SOURCE_EPOCH"
export KBUILD_BUILD_USER=openvmm
export KBUILD_BUILD_HOST=openvmm
export KBUILD_BUILD_VERSION=1

make_kernel() {
    local role=$1
    local output=$2
    local fragment=$3
    local config_stamp=$output/.openvmm-cca-config-stamp
    local expected_config_stamp

    expected_config_stamp="$(
        printf '%s\n%s\n' \
            "$EXPECTED_REVISION" \
            "$(sha256sum "$fragment" | awk '{print $1}')"
    )"

    if [[ ! -f "$output/.config" || ! -f "$config_stamp" ||
        "$(<"$config_stamp")" != "$expected_config_stamp" ]]; then
        echo "Configuring $role kernel in $output"
        make -C "$BUILD_SOURCE" O="$output" ARCH=arm64 CROSS_COMPILE="$CROSS_COMPILE" defconfig
        (
            cd "$BUILD_SOURCE"
            KCONFIG_CONFIG="$output/.config" \
                scripts/kconfig/merge_config.sh -m -y -O "$output" \
                "$output/.config" "$fragment"
        )
        make -C "$BUILD_SOURCE" O="$output" ARCH=arm64 CROSS_COMPILE="$CROSS_COMPILE" olddefconfig
        printf '%s' "$expected_config_stamp" >"$config_stamp"
    else
        echo "Reusing $role kernel configuration in $output"
    fi

    while IFS= read -r setting; do
        [[ -n "$setting" ]] || continue
        if [[ "$setting" == "# CONFIG_"*" is not set" ]]; then
            grep -Fxq "$setting" "$output/.config" ||
                fail "$role config did not preserve '$setting'"
        else
            grep -Fxq "$setting" "$output/.config" ||
                fail "$role config did not resolve '$setting'"
        fi
    done <"$fragment"

    echo "Building $role kernel"
    make -C "$BUILD_SOURCE" O="$output" ARCH=arm64 CROSS_COMPILE="$CROSS_COMPILE" \
        -j"$JOBS" Image
    [[ -f "$output/arch/arm64/boot/Image" ]] ||
        fail "$role kernel Image was not produced"
}

make_kernel host "$HOST_OUTPUT" "$HOST_FRAGMENT"
make_kernel guest "$GUEST_OUTPUT" "$GUEST_FRAGMENT"

cp "$HOST_OUTPUT/arch/arm64/boot/Image" "$OUTPUT_ROOT/host-Image"
cp "$GUEST_OUTPUT/arch/arm64/boot/Image" "$OUTPUT_ROOT/guest-Image"
cp "$HOST_OUTPUT/.config" "$OUTPUT_ROOT/host.config"
cp "$GUEST_OUTPUT/.config" "$OUTPUT_ROOT/guest.config"

HOST_RELEASE="$(
    make -s -C "$BUILD_SOURCE" O="$HOST_OUTPUT" ARCH=arm64 \
        CROSS_COMPILE="$CROSS_COMPILE" kernelrelease
)"
GUEST_RELEASE="$(
    make -s -C "$BUILD_SOURCE" O="$GUEST_OUTPUT" ARCH=arm64 \
        CROSS_COMPILE="$CROSS_COMPILE" kernelrelease
)"
TOOLCHAIN="$("${CROSS_COMPILE}gcc" --version | sed -n '1p')"
MAKE_VERSION="$(make --version | sed -n '1p')"

cat >"$OUTPUT_ROOT/manifest.txt" <<EOF
source=$SOURCE
revision=$EXPECTED_REVISION
source_epoch=$SOURCE_EPOCH
cross_compile=$CROSS_COMPILE
toolchain=$TOOLCHAIN
make=$MAKE_VERSION
jobs=$JOBS
kbuild_build_timestamp=$KBUILD_BUILD_TIMESTAMP
kbuild_build_user=$KBUILD_BUILD_USER
kbuild_build_host=$KBUILD_BUILD_HOST
kbuild_build_version=$KBUILD_BUILD_VERSION
host_fragment_sha256=$(sha256sum "$HOST_FRAGMENT" | awk '{print $1}')
guest_fragment_sha256=$(sha256sum "$GUEST_FRAGMENT" | awk '{print $1}')
host_config_sha256=$(sha256sum "$OUTPUT_ROOT/host.config" | awk '{print $1}')
guest_config_sha256=$(sha256sum "$OUTPUT_ROOT/guest.config" | awk '{print $1}')
host_image_sha256=$(sha256sum "$OUTPUT_ROOT/host-Image" | awk '{print $1}')
guest_image_sha256=$(sha256sum "$OUTPUT_ROOT/guest-Image" | awk '{print $1}')
host_kernelrelease=$HOST_RELEASE
guest_kernelrelease=$GUEST_RELEASE
host_build_command=make ARCH=arm64 CROSS_COMPILE=$CROSS_COMPILE O=$HOST_OUTPUT Image
guest_build_command=make ARCH=arm64 CROSS_COMPILE=$CROSS_COMPILE O=$GUEST_OUTPUT Image
EOF

echo "Built CCA kernels:"
echo "  host:     $OUTPUT_ROOT/host-Image"
echo "  guest:    $OUTPUT_ROOT/guest-Image"
echo "  manifest: $OUTPUT_ROOT/manifest.txt"
