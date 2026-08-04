#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
TIMEOUT_SECONDS="${QEMU_CCA_SMOKE_TIMEOUT_SECONDS:-1800}"
QEMU_CCA_ROOT="${QEMU_CCA_ROOT:-$REPO_ROOT/target/cca-qemu}"
CCA_TEST_ROOT="${CCA_TEST_ROOT:-$REPO_ROOT/target/cca-test}"
LOG_DIR="${QEMU_CCA_LOG_DIR:-$QEMU_CCA_ROOT/logs/smoke}"
HOST_LOG="$LOG_DIR/host-console.log"
STATUS_FILE="$LOG_DIR/smoke.status"
TIMING_FILE="$LOG_DIR/smoke-timing.txt"
PHASE0_MANIFEST="${QEMU_CCA_PHASE0_MANIFEST:-$QEMU_CCA_ROOT/phase0-manifest.txt}"
mkdir -p "$LOG_DIR"
rm -f "$STATUS_FILE" "$TIMING_FILE"
export QEMU_CCA_LOG_DIR="$LOG_DIR"

python3 - "$REPO_ROOT" "$TIMEOUT_SECONDS" "$HOST_LOG" "$STATUS_FILE" "$TIMING_FILE" <<'PY'
import os
import pty
import re
import select
import signal
import sys
import time

repo_root, timeout_seconds, host_log, status_file, timing_file = sys.argv[1:]
timeout_seconds = int(timeout_seconds)
started = time.monotonic()

login_prompt = re.compile(r"buildroot login:")
shell_prompt = re.compile(r"(?:^|[\r\n])[^#\r\n]*#\s*$")
guest_shell = re.compile(
    r"No root device specified\. Dropping to a shell\.|"
    r"can.t access tty; job control turned off"
)
smoke_success = re.compile(r"(?:^|[\r\n])OVMM_SMOKE_ALL_PASS(?:[\r\n]|$)")
smoke_failure = re.compile(r"(?:^|[\r\n])OVMM_SMOKE_[A-Z_]+_FAIL(?:[\r\n]|$)")
smoke_begin = re.compile(r"(?:^|[\r\n])OVMM_SMOKE_BEGIN(?:[\r\n]|$)")
failure_pattern = re.compile(
    r"Kernel panic|fatal error|failed to run VP|Guest crash|VCPU panic"
)
openvmm_exit = re.compile(r'mesh child exited successfully .* name="vm"')
smoke_command = (
    'ok=1; echo OVMM_SMOKE_BEGIN; '
    'd=; for p in /sys/class/block/vd*; do [ -e "$p" ] || continue; '
    '[ "$(cat "$p/size")" = 131072 ] && d=${p##*/} && break; done; '
    'm=OPENVMM-VIRTIO-BLK-SMOKE; '
    'if [ -n "$d" ] && printf %s "$m" | dd of=/dev/$d bs=1 count=${#m} conv=fsync 2>/dev/null '
    '&& [ "$(dd if=/dev/$d bs=1 count=${#m} 2>/dev/null)" = "$m" ]; '
    'then echo OVMM_SMOKE_BLK_PASS; else echo OVMM_SMOKE_BLK_FAIL; ok=0; fi; '
    'n=; for p in /sys/class/net/*; do [ -e "$p" ] || continue; '
    '[ "${p##*/}" != lo ] && n=${p##*/} && break; done; '
    'if [ -n "$n" ]; then echo OVMM_SMOKE_NET_ENUM_PASS; '
    'else echo OVMM_SMOKE_NET_ENUM_FAIL; ok=0; fi; '
    'if [ -n "$n" ] && ifconfig "$n" 10.0.0.2 netmask 255.255.255.0 up; '
    'then echo OVMM_SMOKE_NET_LINK_PASS; else echo OVMM_SMOKE_NET_LINK_FAIL; ok=0; fi; '
    'if [ -n "$n" ] && ping -c 1 -W 2 10.0.0.1 >/dev/null 2>&1; '
    'then echo OVMM_SMOKE_NET_PING_PASS; else echo OVMM_SMOKE_NET_PING_FAIL; ok=0; fi; '
    'if [ "$ok" = 1 ]; then echo OVMM_SMOKE_ALL_PASS; else echo OVMM_SMOKE_ALL_FAIL; fi'
)

argv = [os.path.join(repo_root, "run-qemu-cca-host.sh")]
deadline = time.monotonic() + timeout_seconds
buffer = ""
logged_in = False
openvmm_started = False
guest_ready = False
smoke_sent = False
smoke_started = False
quit_escape_sent = False
quit_command_sent = False
openvmm_exit_observed = False
poweroff_sent = False
shutdown_deadline = None
success = False
outcome = "exited"
child_exit = None
normal_exit_observed = False

with open(host_log, "w", encoding="utf-8", errors="replace") as log:
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(repo_root)
        os.execv(argv[0], argv)
    try:
        while time.monotonic() < deadline:
            if shutdown_deadline is not None and time.monotonic() >= shutdown_deadline:
                outcome = "shutdown_timeout"
                break
            readable, _, _ = select.select([fd], [], [], 1)
            if not readable:
                continue
            try:
                data = os.read(fd, 4096)
            except OSError:
                finished, child_status = os.waitpid(pid, os.WNOHANG)
                if finished:
                    child_exit = os.waitstatus_to_exitcode(child_status)
                    outcome = f"child_exit_{child_exit}"
                    normal_exit_observed = True
                else:
                    outcome = "pty_error"
                break
            if not data:
                finished, child_status = os.waitpid(pid, os.WNOHANG)
                if finished:
                    child_exit = os.waitstatus_to_exitcode(child_status)
                    outcome = f"child_exit_{child_exit}"
                    normal_exit_observed = True
                else:
                    outcome = "eof"
                break

            text = data.decode(errors="replace")
            print(text, end="", flush=True)
            log.write(text)
            log.flush()
            buffer = (buffer + text)[-32768:]

            if failure_pattern.search(buffer) or smoke_failure.search(buffer):
                outcome = "failure_marker"
                break
            if login_prompt.search(buffer) and not logged_in:
                os.write(fd, b"root\r")
                logged_in = True
                buffer = ""
                continue
            if logged_in and shell_prompt.search(buffer) and not openvmm_started:
                os.write(fd, b"/cca-share/run-openvmm-kvm-cca.sh\r")
                openvmm_started = True
                buffer = ""
                continue
            if openvmm_started and guest_shell.search(buffer):
                guest_ready = True
            if openvmm_started and not guest_ready and shell_prompt.search(buffer):
                outcome = "openvmm_exited_early"
                break
            if guest_ready and shell_prompt.search(buffer) and not smoke_sent:
                payload = (smoke_command + "\r").encode()
                for offset in range(0, len(payload), 16):
                    os.write(fd, payload[offset : offset + 16])
                    time.sleep(0.02)
                smoke_sent = True
                buffer = ""
                continue
            if smoke_success.search(buffer):
                success = True
                if not quit_escape_sent:
                    os.write(fd, b"\x11")
                    quit_escape_sent = True
                    buffer = ""
                continue
            if smoke_begin.search(buffer):
                smoke_started = True
            if smoke_started and not success and shell_prompt.search(buffer):
                outcome = "smoke_missing_success"
                break
            if quit_escape_sent and not quit_command_sent and "openvmm>" in buffer:
                os.write(fd, b"q\r")
                quit_command_sent = True
                buffer = ""
                continue
            if openvmm_exit.search(buffer):
                openvmm_exit_observed = True
            if (
                quit_command_sent
                and openvmm_exit_observed
                and shell_prompt.search(buffer)
                and not poweroff_sent
            ):
                os.write(fd, b"poweroff -f\r")
                poweroff_sent = True
                shutdown_deadline = time.monotonic() + 30
                buffer = ""
                continue
        else:
            outcome = "timeout"
    finally:
        try:
            os.killpg(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        cleanup_deadline = time.monotonic() + 5
        while time.monotonic() < cleanup_deadline:
            try:
                finished, child_status = os.waitpid(pid, os.WNOHANG)
            except ChildProcessError:
                finished = pid
                child_status = None
            if finished:
                if child_status is not None:
                    child_exit = os.waitstatus_to_exitcode(child_status)
                break
            time.sleep(0.1)
        else:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                _, child_status = os.waitpid(pid, 0)
                child_exit = os.waitstatus_to_exitcode(child_status)
            except ChildProcessError:
                pass

elapsed = time.monotonic() - started
if success and poweroff_sent and normal_exit_observed and child_exit == 0:
    outcome = "completed"
with open(timing_file, "w", encoding="utf-8") as timing:
    timing.write(f"smoke_wall_seconds={elapsed:.3f}\n")
    timing.write(f"outcome={outcome}\n")
    if child_exit is not None:
        timing.write(f"child_exit={child_exit}\n")

status = 0 if success and outcome == "completed" else (124 if outcome == "timeout" else 1)
with open(status_file, "w", encoding="utf-8") as status_output:
    status_output.write(f"{status}\n")
if status != 0:
    print(f"QEMU CCA smoke failed ({outcome}); see {host_log}", file=sys.stderr)
    sys.exit(status)
print(f"QEMU CCA smoke passed; logs: {host_log}")
PY

QEMU_BIN="${QEMU_BIN:-$QEMU_CCA_ROOT/qemu/qemu-system-aarch64}"
FIRMWARE="${QEMU_CCA_FIRMWARE:-$QEMU_CCA_ROOT/firmware/flash.bin}"
HOST_KERNEL="${QEMU_CCA_HOST_KERNEL:-$CCA_TEST_ROOT/cca-kernels-v15/host-Image}"
HOST_ROOTFS="${QEMU_CCA_HOST_ROOTFS:-$CCA_TEST_ROOT/kvm-cca/rootfs.ext2}"
SHARE_DIR="${QEMU_CCA_SHARE_DIR:-$CCA_TEST_ROOT/kvm-cca/share}"
FIRMWARE_MANIFEST="$QEMU_CCA_ROOT/firmware/manifest.txt"
KERNEL_MANIFEST="$CCA_TEST_ROOT/cca-kernels-v15/manifest.txt"
PREFLIGHT_TIMING="$QEMU_CCA_ROOT/logs/preflight-timing.txt"

for path in \
    "$QEMU_BIN" \
    "$FIRMWARE" \
    "$HOST_KERNEL" \
    "$HOST_ROOTFS" \
    "$SHARE_DIR/openvmm" \
    "$SHARE_DIR/guest-Image" \
    "$SHARE_DIR/initrd" \
    "$SHARE_DIR/kvm_cca_preflight" \
    "$SHARE_DIR/run-openvmm-kvm-cca.sh" \
    "$FIRMWARE_MANIFEST" \
    "$KERNEL_MANIFEST" \
    "$TIMING_FILE"; do
    [[ -f "$path" ]] || {
        echo "error: Phase 0 manifest input is missing: $path" >&2
        exit 1
    }
done

MANIFEST_DIR="$(dirname "$PHASE0_MANIFEST")"
mkdir -p "$MANIFEST_DIR"
MANIFEST_STAGE="$(mktemp "$MANIFEST_DIR/.phase0-manifest.XXXXXXXXXX")"
cleanup_manifest_stage() {
    rm -f -- "$MANIFEST_STAGE"
}
trap cleanup_manifest_stage EXIT
{
    echo "qemu_version=$("$QEMU_BIN" --version | sed -n '1p')"
    echo "qemu_sha256=$(sha256sum "$QEMU_BIN" | awk '{print $1}')"
    echo "qemu_cpu=${QEMU_CCA_CPU:-max,x-rme=on,lpa2=off,sme=off,pauth-impdef=on}"
    echo "qemu_memory=${QEMU_CCA_MEMORY:-2G}"
    echo "qemu_processors=${QEMU_CCA_PROCESSORS:-1}"
    echo "firmware_sha256=$(sha256sum "$FIRMWARE" | awk '{print $1}')"
    echo "host_kernel_sha256=$(sha256sum "$HOST_KERNEL" | awk '{print $1}')"
    echo "host_rootfs_sha256=$(sha256sum "$HOST_ROOTFS" | awk '{print $1}')"
    echo "openvmm_sha256=$(sha256sum "$SHARE_DIR/openvmm" | awk '{print $1}')"
    echo "preflight_sha256=$(sha256sum "$SHARE_DIR/kvm_cca_preflight" | awk '{print $1}')"
    echo "openvmm_launch_script_sha256=$(sha256sum "$SHARE_DIR/run-openvmm-kvm-cca.sh" | awk '{print $1}')"
    echo "guest_kernel_sha256=$(sha256sum "$SHARE_DIR/guest-Image" | awk '{print $1}')"
    echo "guest_initrd_sha256=$(sha256sum "$SHARE_DIR/initrd" | awk '{print $1}')"
    sed 's/^/firmware_/' "$FIRMWARE_MANIFEST"
    sed 's/^/kernel_/' "$KERNEL_MANIFEST"
    if [[ -f "$PREFLIGHT_TIMING" ]]; then
        sed 's/^/preflight_/' "$PREFLIGHT_TIMING"
    fi
    sed 's/^/smoke_/' "$TIMING_FILE"
    echo "host_console=$HOST_LOG"
    echo "secondary_console=$LOG_DIR/secondary-console.log"
} >"$MANIFEST_STAGE"
mv -f -- "$MANIFEST_STAGE" "$PHASE0_MANIFEST"
trap - EXIT
echo "Phase 0 manifest: $PHASE0_MANIFEST"
