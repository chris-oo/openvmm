#!/bin/sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -eu

case "${1:-start}" in
start | "")
    pipette=/share/pipette
    preflight=/share/kvm_cca_preflight
    for argument in $(cat /proc/cmdline); do
        case "$argument" in
        incubator.pipette=*)
            pipette=${argument#incubator.pipette=}
            ;;
        incubator.preflight=*)
            preflight=${argument#incubator.preflight=}
            ;;
        esac
    done

    mkdir -p /share /cca/logs
    mount -t 9p -o trans=virtio,version=9p2000.L FM /share

    {
        ip address show
        ip route show
    } >/cca/logs/fvp-network.log 2>&1

    if ! ip route show default | grep -q .; then
        echo "FVP host has no default network route" >&2
        cat /cca/logs/fvp-network.log >&2
        poweroff -f
        exit 1
    fi

    set +e
    "$preflight" >/cca/logs/kvm-cca-preflight.log 2>&1
    preflight_status=$?
    set -e
    echo "$preflight_status" >/cca/logs/kvm-cca-preflight.status
    if [ "$preflight_status" -ne 0 ]; then
        cat /cca/logs/kvm-cca-preflight.log
        poweroff -f
        exit "$preflight_status"
    fi

    export HOME=/root
    cd /share
    exec "$pipette" --transport tcp
    ;;
esac

exit 0
