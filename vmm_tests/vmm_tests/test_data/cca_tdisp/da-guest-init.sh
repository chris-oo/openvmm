#!/bin/sh
set -eu
export PATH=/sbin:/usr/sbin:/bin:/usr/bin
shutdown() {
    status=$?
    trap - EXIT
    echo "DA_STAGE_A_GUEST_EXIT=$status"
    sync
    poweroff -f
}
trap shutdown EXIT
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /tmp
mount -t tmpfs tmpfs /tmp
echo DA_STAGE_A_GUEST_READY
uname -a
cat /proc/cmdline
if grep -q 'da_phase=boot' /proc/cmdline; then
    echo DA_STAGE_A_GUEST_BOOT_PASS
    exit 0
fi
grep -q 'da_phase=ahci' /proc/cmdline
lspci -k
count=0
guest_bdf=
for device in /sys/bus/pci/devices/*; do
    if [ "$(cat "$device/class")" = 0x010601 ]; then
        count=$((count + 1))
        guest_bdf=${device##*/}
    fi
done
test "$count" -eq 1
device=/sys/bus/pci/devices/$guest_bdf
echo "DA_STAGE_A_GUEST_AHCI=$guest_bdf"
od -Ax -tx1 -N256 "$device/config"
cat "$device/resource"
if [ -L "$device/driver" ]; then
    printf '%s\n' "$guest_bdf" > "$device/driver/unbind"
fi
set -- /sys/class/tsm/tsm*
test "$#" -eq 1
test -e "$1"
tsm=${1##*/}
printf '%s\n' "$tsm" > "$device/tsm/lock"
test "$(cat "$device/tsm/lock")" = "$tsm"
echo DA_STAGE_A_GUEST_LOCK_PASS
printf '1\n' > "$device/tsm/accept"
test "$(cat "$device/tsm/accept")" = 1
echo DA_STAGE_A_GUEST_ACCEPT_RETURNED
printf '%s\n' "$guest_bdf" > /sys/bus/pci/drivers_probe
attempt=0
while :; do
    count=0
    disk=
    for block in /sys/class/block/*; do
        case "$(readlink -f "$block/device")" in
            *"/$guest_bdf/"*)
                count=$((count + 1))
                disk=${block##*/}
                ;;
        esac
    done
    if [ "$count" -eq 1 ] && [ -b "/dev/$disk" ]; then
        break
    fi
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 30 ] || [ "$count" -gt 1 ]; then
        echo "AHCI disk discovery failed: count=$count attempts=$attempt"
        exit 1
    fi
    sleep 1
done
test "$count" -eq 1
test "$(cat "/sys/class/block/$disk/size")" = 131072
echo "DA_STAGE_A_DISK=$disk"
dd if="/dev/$disk" of=/tmp/ahci-read.bin bs=1048576 count=64 iflag=direct
sha256sum /tmp/ahci-read.bin > /tmp/actual.sha256
read actual file < /tmp/actual.sha256
read expected < /da-ahci.sha256
test "$actual" = "$expected"
echo "DA_STAGE_A_GUEST_AHCI_IO_PASS hash=$actual"
cat /proc/interrupts
od -Ax -tx1 -N256 "$device/config"
cat "$device/irq"
if [ -d "$device/msi_irqs" ]; then
    grep . "$device"/msi_irqs/*
fi
dmesg
if dmesg | grep -q 'failed to enable DMA from the device'; then
    exit 1
fi
printf '%s\n' "$guest_bdf" > "$device/driver/unbind"
printf '%s\n' "$tsm" > "$device/tsm/unlock"
echo DA_STAGE_A_GUEST_UNLOCK_RETURNED
