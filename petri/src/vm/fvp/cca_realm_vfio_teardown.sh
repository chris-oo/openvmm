# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -eu
step=ownership
finish() {
    status=$?
    trap - EXIT
    if [ "$status" -ne 0 ]; then
        echo "INCUBATOR REALM VFIO TEARDOWN FAILED step=$step status=$status" >&2
    fi
    exit "$status"
}
trap finish EXIT
echo 'INCUBATOR REALM VFIO TEARDOWN START'
state=/run/incubator-cca-realm-vfio-state
test -d "$state"
if [ -f "$state/complete" ]; then
    echo 'INCUBATOR REALM VFIO TEARDOWN ALREADY COMPLETE'
    exit 0
fi
bdf=0000:01:00.0
device=/sys/bus/pci/devices/$bdf
write_sysfs() {
    echo "INCUBATOR REALM VFIO TEARDOWN step=$step"
    printf '%s\n' "$1" > "$2"
}
step=identity
test "$(cat "$device/vendor")" = 0x0abc
test "$(cat "$device/device")" = 0xaced
test "$(cat "$device/class")" = 0x010601
rm -f /run/incubator-cca-realm-vfio
if [ -f "$state/tsm" ]; then
    expected_tsm=$(cat "$state/tsm")
    test "$expected_tsm" = tsm0
    connected_tsm=$(cat "$device/tsm/connect")
    test -z "$connected_tsm" || test "$connected_tsm" = "$expected_tsm"
fi
if [ -f "$state/override" ]; then
    step=override-ownership
    override=$(cat "$device/driver_override")
    case "$override" in
        vfio-pci|''|'(null)') ;;
        *) exit 1 ;;
    esac
fi
step=vfio-unbind
if [ -f "$state/tsm" ] && [ -L "$device/driver" ]; then
    test -f "$state/bind"
    test "$(basename "$(readlink "$device/driver")")" = vfio-pci
fi
if [ -f "$state/tsm" ]; then
    # cb45796 removed bound. Disconnect refuses live vDEVICE contexts under
    # the PCI/TSM locks, so prove release before unbinding VFIO.
    step=realm-contexts
    if [ -f "$state/contexts-released" ]; then
        test -z "$connected_tsm"
    else
        test ! -f "$state/contexts-attempted"
        if [ -n "$connected_tsm" ]; then
            : > "$state/contexts-attempted"
            write_sysfs "$expected_tsm" "$device/tsm/disconnect"
            test -z "$(cat "$device/tsm/connect")"
        else
            test ! -f "$state/bind"
        fi
        : > "$state/contexts-released"
    fi
fi
step=vfio-unbind
if [ -f "$state/tsm" ] && [ -L "$device/driver" ]; then
    write_sysfs "$bdf" "$device/driver/unbind"
    test ! -L "$device/driver"
fi
if [ -f "$state/bind" ]; then
    set -- "$device"/vfio-dev/vfio*
    test ! -e "$1"
fi
step=driver-override
# Leave AHCI unbound; shutdown must not start another driver or disk operation.
if [ -f "$state/override" ]; then
    override=$(cat "$device/driver_override")
    case "$override" in
        vfio-pci) write_sysfs '' "$device/driver_override" ;;
        ''|'(null)') ;;
        *) exit 1 ;;
    esac
    override=$(cat "$device/driver_override")
    test -z "$override" || test "$override" = '(null)'
fi
step=complete
: > "$state/complete"
echo 'INCUBATOR REALM VFIO TEARDOWN COMPLETE'
