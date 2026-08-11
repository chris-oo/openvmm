#!/bin/sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -eu

case "${1:-start}" in
start | "")
    mount_tag=host
    network=qemu-static
    pipette=/share/pipette
    preflight=/share/kvm_cca_preflight
    for argument in $(cat /proc/cmdline); do
        case "$argument" in
        incubator.mount_tag=*)
            mount_tag=${argument#incubator.mount_tag=}
            ;;
        incubator.network=*)
            network=${argument#incubator.network=}
            ;;
        incubator.pipette=*)
            pipette=${argument#incubator.pipette=}
            ;;
        incubator.preflight=*)
            preflight=${argument#incubator.preflight=}
            ;;
        esac
    done

    case "$mount_tag" in
    host | FM) ;;
    *)
        echo "invalid incubator mount tag: $mount_tag" >&2
        exit 1
        ;;
    esac
    case "$network" in
    qemu-static | dhcp) ;;
    *)
        echo "invalid incubator network mode: $network" >&2
        exit 1
        ;;
    esac
    case "$pipette" in
    /share/*) ;;
    *)
        echo "invalid incubator pipette path: $pipette" >&2
        exit 1
        ;;
    esac
    case "$preflight" in
    /share/*) ;;
    *)
        echo "invalid incubator preflight path: $preflight" >&2
        exit 1
        ;;
    esac

    mkdir -p /share /cca/logs
    mount -t 9p -o trans=virtio,version=9p2000.L "$mount_tag" /share

    if [ "$network" = qemu-static ]; then
        ip link set lo up
        ip link set eth0 up
        ip addr replace 10.0.2.15/24 dev eth0
        ip route replace default via 10.0.2.2
        printf 'nameserver 10.0.2.3\n' >/etc/resolv.conf
    fi

    {
        echo "mode=$network"
        ip address show
        ip route show
    } >/cca/logs/incubator-network.log 2>&1
    if ! ip route show default | grep -q .; then
        echo "incubator host has no default network route" >&2
        cat /cca/logs/incubator-network.log >&2
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
