# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Included only in the pinned single-AHCI FVP host init script.
# No guest device access, TDISP lock/run, or DMA qualification.
echo 'INCUBATOR REALM VFIO START'
test -c /dev/kvm
test -c /dev/iommu
bdf=0000:01:00.0
device=/sys/bus/pci/devices/$bdf
test "$(cat "$device/vendor")" = 0x0abc
test "$(cat "$device/device")" = 0xaced
test "$(cat "$device/class")" = 0x010601
test "$(cat /sys/bus/pci/devices/0000:00:01.0/vendor)" = 0x13b5
test "$(cat /sys/bus/pci/devices/0000:00:01.0/device)" = 0x0def
test "$(cat /sys/bus/pci/devices/0000:00:01.0/class)" = 0x060400
set -- /sys/bus/pci/devices/*
test "$#" -eq 2
test -L "$device/iommu_group"
set -- "$device"/iommu_group/devices/*
test "$#" -eq 1
test "${1##*/}" = "$bdf"
if grep -q '^/dev/' /proc/mounts; then
    echo 'Refusing AHCI unbind: a block filesystem is mounted' >&2
    exit 1
fi
test "$(basename "$(readlink "$device/driver")")" = ahci
printf '%s\n' "$bdf" > "$device/driver/unbind"
set -- /sys/class/tsm/tsm*
test "$#" -eq 1
test -e "$1"
tsm=${1##*/}
test "$tsm" = tsm0
test -z "$(cat "$device/tsm/connect")"
# Record intent before each write so EXIT cleanup can handle partial setup.
# Teardown rechecks the current state before changing an owned resource.
printf '%s\n' "$tsm" > /run/incubator-cca-realm-vfio-state/tsm
printf '%s\n' "$tsm" > "$device/tsm/connect"
test "$(cat "$device/tsm/connect")" = "$tsm"
override=$(cat "$device/driver_override")
test -z "$override" || test "$override" = '(null)'
: > /run/incubator-cca-realm-vfio-state/override
printf '%s\n' vfio-pci > "$device/driver_override"
: > /run/incubator-cca-realm-vfio-state/bind
printf '%s\n' "$bdf" > /sys/bus/pci/drivers_probe
test "$(basename "$(readlink "$device/driver")")" = vfio-pci
test "$(cat "$device/tsm/connect")" = "$tsm"
set -- "$device"/vfio-dev/vfio*
test "$#" -eq 1
test -d "$1"
test -c "/dev/vfio/devices/${1##*/}"
mkdir -p /run
printf '%s\n' "$bdf" > /run/incubator-cca-realm-vfio
printf 'INCUBATOR REALM VFIO READY bdf=%s tsm=%s cdev=%s\n' "$bdf" "$tsm" "${1##*/}"
