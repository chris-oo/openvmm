#!/bin/sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -eu

case "${1:-start}" in
start | "")
    mkdir -p /share /cca/logs
    if ! mount -t 9p -o trans=virtio,version=9p2000.L host /share 2>/dev/null; then
        mount -t 9p -o trans=virtio,version=9p2000.L FM /share
    fi

    ip link set lo up
    ip link set eth0 up
    ip addr replace 10.0.2.15/24 dev eth0
    ip route replace default via 10.0.2.2
    printf 'nameserver 10.0.2.3\n' >/etc/resolv.conf

    set +e
    /share/kvm_cca_preflight > /cca/logs/kvm-cca-preflight.log 2>&1
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
    exec /share/pipette --transport tcp
    ;;
esac

exit 0
