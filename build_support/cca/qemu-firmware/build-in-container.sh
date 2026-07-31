#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

: "${TF_RMM_REVISION:?}"
: "${TF_A_REVISION:?}"
: "${EDK2_REVISION:?}"
: "${BUILDER_IMAGE:?}"

TF_RMM_REPO=https://git.trustedfirmware.org/TF-RMM/tf-rmm.git
TF_A_REPO=https://git.trustedfirmware.org/TF-A/trusted-firmware-a.git
EDK2_REPO=https://github.com/tianocore/edk2.git

JOBS="${JOBS:-$(nproc)}"
OFFLINE="${OFFLINE:-false}"
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-0}"

export HOME=/tmp/home
export CROSS_COMPILE=aarch64-linux-gnu-
export KBUILD_BUILD_TIMESTAMP="@$SOURCE_DATE_EPOCH"
export KBUILD_BUILD_USER=openvmm
export KBUILD_BUILD_HOST=openvmm
export LC_ALL=C
export PYTHONHASHSEED=0
export TZ=UTC
mkdir -p "$HOME" /cache/git /cache/snapshots /work /out/logs

retry() {
    local attempt
    for attempt in 1 2 3 4; do
        if "$@"; then
            return 0
        fi
        if [[ "$attempt" != 4 ]]; then
            echo "command failed (attempt $attempt/4), retrying..." >&2
            sleep $((attempt * 5))
        fi
    done
    return 1
}

extract_snapshot() {
    local snapshot=$1
    local checkout=$2
    local revision=$3
    local checksum_file=$snapshot.sha256

    [[ -f "$snapshot" && -f "$checksum_file" ]] || return 1
    (
        cd "$(dirname "$snapshot")"
        sha256sum --check "$(basename "$checksum_file")"
    ) || return 1
    rm -rf -- "$checkout"
    mkdir -p "$checkout"
    tar -xf "$snapshot" -C "$checkout" || return 1
    [[ "$(<"$checkout/.openvmm-revision")" == "$revision" ]] || return 1
}

write_snapshot() {
    local snapshot=$1
    local checkout=$2
    shift 2

    tar "$@" -cf "$snapshot.tmp" -C "$checkout" .
    mv "$snapshot.tmp" "$snapshot"
    (
        cd "$(dirname "$snapshot")"
        sha256sum "$(basename "$snapshot")" >"$(basename "$snapshot").sha256"
    )
}

clone_at_revision() {
    local name=$1
    local url=$2
    local revision=$3
    local remote_ref=$4
    local mirror=/cache/git/$name.git
    local checkout=/work/$name

    if [[ -d "$mirror" ]] &&
        ! git -C "$mirror" fsck --connectivity-only --no-dangling "$revision" \
            >/dev/null 2>&1; then
        [[ "$OFFLINE" == false ]] ||
            { echo "corrupt offline Git cache for $name" >&2; exit 1; }
        rm -rf -- "$mirror"
    fi
    if [[ ! -d "$mirror" ]]; then
        [[ "$OFFLINE" == false ]] ||
            { echo "missing offline Git cache for $name" >&2; exit 1; }
        rm -rf -- "$mirror"
        git init --bare "$mirror"
        git -C "$mirror" remote add origin "$url"
    fi
    if ! git -C "$mirror" cat-file -e "$revision^{tree}" 2>/dev/null; then
        [[ "$OFFLINE" == false ]] ||
            { echo "revision $revision is absent from offline cache $name" >&2; exit 1; }
        if ! retry git -c http.version=HTTP/1.1 -C "$mirror" fetch \
            --depth=1 --force origin "$revision"; then
            retry git -c http.version=HTTP/1.1 -C "$mirror" fetch \
                --depth=1 --force origin "$remote_ref"
        fi
        [[ "$(git -C "$mirror" rev-parse FETCH_HEAD^{commit})" == "$revision" ]] ||
            { echo "$remote_ref did not resolve to pinned revision $revision" >&2; exit 1; }
    fi
    git -C "$mirror" cat-file -e "$revision^{tree}"
    git -C "$mirror" update-ref refs/heads/openvmm-pinned "$revision"

    rm -rf -- "$checkout"
    git clone --no-checkout "$mirror" "$checkout"
    git -C "$checkout" checkout --detach "$revision"
}

TF_RMM_SNAPSHOT=/cache/snapshots/tf-rmm-$TF_RMM_REVISION.tar
if ! extract_snapshot "$TF_RMM_SNAPSHOT" /work/tf-rmm "$TF_RMM_REVISION"; then
    rm -rf -- /work/tf-rmm "$TF_RMM_SNAPSHOT" "$TF_RMM_SNAPSHOT.sha256"
    [[ "$OFFLINE" == false ]] ||
        { echo "missing or invalid offline TF-RMM snapshot" >&2; exit 1; }
    clone_at_revision tf-rmm "$TF_RMM_REPO" "$TF_RMM_REVISION" \
        refs/heads/topics/rmm-v2.0-poc_3
fi
clone_at_revision tf-a "$TF_A_REPO" "$TF_A_REVISION" refs/tags/v2.15.0
EDK2_SNAPSHOT=/cache/snapshots/edk2-$EDK2_REVISION.tar
if ! extract_snapshot "$EDK2_SNAPSHOT" /work/edk2 "$EDK2_REVISION"; then
    rm -rf -- /work/edk2 "$EDK2_SNAPSHOT" "$EDK2_SNAPSHOT.sha256"
    [[ "$OFFLINE" == false ]] ||
        { echo "missing or invalid offline EDK2 snapshot" >&2; exit 1; }
    clone_at_revision edk2 "$EDK2_REPO" "$EDK2_REVISION" \
        refs/tags/edk2-stable202505
    git -C /work/edk2 submodule update --init --recursive
    printf '%s\n' "$EDK2_REVISION" >/work/edk2/.openvmm-revision
    write_snapshot "$EDK2_SNAPSHOT" /work/edk2 --exclude=.git
fi

(
    cd /work/edk2
    make -C BaseTools -j"$JOBS"
    export WORKSPACE=$PWD
    export CONF_PATH=$PWD/Conf
    export EDK_TOOLS_PATH=$PWD/BaseTools
    export GCC5_AARCH64_PREFIX=aarch64-linux-gnu-
    export PYTHON_COMMAND=python3
    # shellcheck disable=SC1091
    source edksetup.sh
    sed -i \
        's/^\*_\*_\*_GENFW_FLAGS[[:space:]]*=.*/\*_\*_\*_GENFW_FLAGS = -z/' \
        "$CONF_PATH/tools_def.txt"
    sed -i \
        's/^\(DEFINE GCC5_AARCH64_CC_FLAGS[[:space:]]*=.*\)$/\1 -frandom-seed=0/' \
        "$CONF_PATH/tools_def.txt"
    # EDK2 module completion order changes firmware-volume layout, so keep the
    # platform build serial to reduce build-to-build variation.
    # TODO: EDK2/FIP/flash are not yet byte-for-byte reproducible even with
    # fixed timestamps, GenFw -z, Python hashing, and GCC random seeds.
    build -q -n 1 -a AARCH64 -b DEBUG -t GCC5 \
        -p ArmVirtPkg/ArmVirtQemuKernel.dsc
) 2>&1 | tee /out/logs/edk2.log

(
    cd /work/tf-rmm
    env CROSS_COMPILE="$CROSS_COMPILE" cmake \
        -DRMM_CONFIG=qemu_virt_defcfg \
        -DCMAKE_BUILD_TYPE=Release \
        -DLOG_LEVEL=40 \
        -S . \
        -B build
) 2>&1 | tee /out/logs/tf-rmm.log
if [[ ! -f "$TF_RMM_SNAPSHOT" ]]; then
    printf '%s\n' "$TF_RMM_REVISION" >/work/tf-rmm/.openvmm-revision
    write_snapshot "$TF_RMM_SNAPSHOT" /work/tf-rmm --exclude=./build
fi
make -C /work/tf-rmm/build -j"$JOBS" 2>&1 | tee -a /out/logs/tf-rmm.log

EDK2_FD=/work/edk2/Build/ArmVirtQemuKernel-AARCH64/DEBUG_GCC5/FV/QEMU_EFI.fd
RMM_IMG=/work/tf-rmm/build/Release/rmm.img
[[ -f "$EDK2_FD" ]] || { echo "missing EDK2 firmware $EDK2_FD" >&2; exit 1; }
[[ -f "$RMM_IMG" ]] || { echo "missing RMM image $RMM_IMG" >&2; exit 1; }

(
    cd /work/tf-a
    make \
        PLAT=qemu \
        QEMU_USE_GIC_DRIVER=QEMU_GICV3 \
        BL33="$EDK2_FD" \
        ENABLE_RME=1 \
        RMM="$RMM_IMG" \
        LOG_LEVEL=40 \
        all fip \
        -j"$JOBS"
) 2>&1 | tee /out/logs/tf-a.log

BL1=/work/tf-a/build/qemu/release/bl1.bin
FIP=/work/tf-a/build/qemu/release/fip.bin
[[ -f "$BL1" ]] || { echo "missing TF-A BL1 $BL1" >&2; exit 1; }
[[ -f "$FIP" ]] || { echo "missing TF-A FIP $FIP" >&2; exit 1; }

install -m 0644 "$RMM_IMG" /out/rmm.img
install -m 0644 "$EDK2_FD" /out/QEMU_EFI.fd
install -m 0644 "$BL1" /out/bl1.bin
install -m 0644 "$FIP" /out/fip.bin
rm -f /out/flash.bin
dd if=/out/bl1.bin of=/out/flash.bin bs=4096 conv=notrunc status=none
dd if=/out/fip.bin of=/out/flash.bin seek=64 bs=4096 conv=notrunc status=none

cat >/out/manifest.txt <<EOF
builder_image=$BUILDER_IMAGE
source_date_epoch=$SOURCE_DATE_EPOCH
jobs=$JOBS
tf_rmm_repo=$TF_RMM_REPO
tf_rmm_revision=$TF_RMM_REVISION
tf_a_repo=$TF_A_REPO
tf_a_revision=$TF_A_REVISION
edk2_repo=$EDK2_REPO
edk2_revision=$EDK2_REVISION
cmake=$(cmake --version | sed -n '1p')
make=$(make --version | sed -n '1p')
cross_gcc=$(${CROSS_COMPILE}gcc --version | sed -n '1p')
rmm_sha256=$(sha256sum /out/rmm.img | awk '{print $1}')
edk2_sha256=$(sha256sum /out/QEMU_EFI.fd | awk '{print $1}')
bl1_sha256=$(sha256sum /out/bl1.bin | awk '{print $1}')
fip_sha256=$(sha256sum /out/fip.bin | awk '{print $1}')
flash_sha256=$(sha256sum /out/flash.bin | awk '{print $1}')
EOF
