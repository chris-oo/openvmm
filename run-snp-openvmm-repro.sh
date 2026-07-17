#!/bin/bash
# Run the SNP OpenVMM repro over SSH and automatically quit OpenVMM when a
# known failure pattern or timeout is observed.

set -euo pipefail

HOST="${SNP_REPRO_HOST:-cho-snp-ubuntu}"
REMOTE_SCRIPT="${SNP_REPRO_REMOTE_SCRIPT:-~/snp-openvmm/run-snp-openvmm.sh}"
TIMEOUT_SECONDS="${SNP_REPRO_TIMEOUT_SECONDS:-180}"
ERROR_PATTERN="${SNP_REPRO_ERROR_PATTERN:-fatal error|failed to run VP|Bad address|guest halted|triple fault|panicked at|assertion failed|abnormal exit|SIGABRT|core dumped|node failure|Connection reset by peer}"
SHELL_PATTERN="${SNP_REPRO_SHELL_PATTERN:-~ #}"

python3 - "$HOST" "$REMOTE_SCRIPT" "$TIMEOUT_SECONDS" "$ERROR_PATTERN" "$SHELL_PATTERN" <<'PY'
import os
import pty
import re
import select
import signal
import subprocess
import sys
import time

host, remote_script, timeout_seconds, error_pattern, shell_pattern = sys.argv[1:]
timeout_seconds = int(timeout_seconds)
pattern = re.compile(error_pattern)
shell_ready = re.compile(shell_pattern)
smoke_success = re.compile(r"(?:^|[\r\n])OVMM_SMOKE_ALL_PASS(?:[\r\n]|$)")
smoke_failure = re.compile(r"(?:^|[\r\n])OVMM_SMOKE_[A-Z_]+_FAIL(?:[\r\n]|$)")
smoke_command = (
    "ok=1; echo OVMM_SMOKE_BEGIN; "
    "d=; for p in /sys/class/block/vd*; do [ -e \"$p\" ] || continue; "
    "[ \"$(cat \"$p/size\")\" = 131072 ] && d=${p##*/} && break; done; "
    "m=OPENVMM-VIRTIO-BLK-SMOKE; "
    "if [ -n \"$d\" ] && printf %s \"$m\" | dd of=/dev/$d bs=1 count=${#m} conv=fsync 2>/dev/null "
    "&& [ \"$(dd if=/dev/$d bs=1 count=${#m} 2>/dev/null)\" = \"$m\" ]; "
    "then echo OVMM_SMOKE_BLK_PASS; else echo OVMM_SMOKE_BLK_FAIL; ok=0; fi; "
    "n=; for p in /sys/class/net/*; do [ -e \"$p\" ] || continue; "
    "[ \"${p##*/}\" != lo ] && n=${p##*/} && break; done; "
    "if [ -n \"$n\" ]; then echo OVMM_SMOKE_NET_ENUM_PASS; "
    "else echo OVMM_SMOKE_NET_ENUM_FAIL; ok=0; fi; "
    "if [ -n \"$n\" ] && ifconfig \"$n\" 10.0.0.2 netmask 255.255.255.0 up; "
    "then echo OVMM_SMOKE_NET_LINK_PASS; else echo OVMM_SMOKE_NET_LINK_FAIL; ok=0; fi; "
    "if [ -n \"$n\" ] && ping -c 1 -W 2 10.0.0.1 >/dev/null 2>&1; "
    "then echo OVMM_SMOKE_NET_PING_PASS; else echo OVMM_SMOKE_NET_PING_FAIL; ok=0; fi; "
    "if [ \"$ok\" = 1 ]; then echo OVMM_SMOKE_ALL_PASS; else echo OVMM_SMOKE_ALL_FAIL; fi"
)

argv = ["ssh", "-t", host, remote_script]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(argv[0], argv)

deadline = time.monotonic() + timeout_seconds
buffer = ""
matched_error = False
matched_success = False
timed_out = False
child_status = None
pending_quit = False
pending_quit_since = None
smoke_sent = False

def send_quit():
    os.write(fd, b"\x11q\r")

def send_smoke_test():
    print("\nSNP repro: running virtio block/network smoke tests", file=sys.stderr, flush=True)
    payload = f"{smoke_command}\r".encode()
    for offset in range(0, len(payload), 32):
        os.write(fd, payload[offset : offset + 32])
        time.sleep(0.01)

try:
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            timed_out = True
            send_quit()
            break

        readable, _, _ = select.select([fd], [], [], min(1, remaining))
        if not readable:
            if pending_quit and pending_quit_since is not None and time.monotonic() - pending_quit_since > 2:
                send_quit()
                break
            continue

        try:
            data = os.read(fd, 4096)
        except OSError:
            finished, status = os.waitpid(pid, os.WNOHANG)
            if finished:
                child_status = status
            break
        if not data:
            finished, status = os.waitpid(pid, os.WNOHANG)
            if finished:
                child_status = status
            break

        text = data.decode(errors="replace")
        print(text, end="", flush=True)
        buffer = (buffer + text)[-8192:]
        if pattern.search(buffer):
            matched_error = True
            if not pending_quit:
                pending_quit = True
                pending_quit_since = time.monotonic()
        if shell_ready.search(buffer) and not smoke_sent:
            send_smoke_test()
            smoke_sent = True
            buffer = ""
            continue
        if smoke_failure.search(buffer):
            matched_error = True
            if not pending_quit:
                pending_quit = True
                pending_quit_since = time.monotonic()
        if smoke_success.search(buffer):
            matched_success = True
            if not pending_quit:
                pending_quit = True
                pending_quit_since = time.monotonic()
        if pending_quit and "openvmm>" in buffer:
            send_quit()
            break
        if pending_quit and pending_quit_since is not None and time.monotonic() - pending_quit_since > 2:
            send_quit()
            break

    # Give OpenVMM a moment to process the quit sequence before escalating.
    end = time.monotonic() + 10
    while time.monotonic() < end:
        readable, _, _ = select.select([fd], [], [], 0.5)
        if readable:
            try:
                data = os.read(fd, 4096)
            except OSError:
                break
            if not data:
                break
            print(data.decode(errors="replace"), end="", flush=True)
        finished, status = os.waitpid(pid, os.WNOHANG)
        if finished:
            child_status = status
            break

    if child_status is not None:
        child_exit = os.waitstatus_to_exitcode(child_status)
        if matched_error:
            sys.exit(1)
        if matched_success:
            sys.exit(0)
        if timed_out:
            sys.exit(124)
        if child_exit != 0:
            print(f"\nSNP repro command exited unexpectedly with status {child_exit}", file=sys.stderr)
            sys.exit(child_exit)
        print("\nSNP repro exited before virtio smoke tests passed", file=sys.stderr)
        sys.exit(1)

    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    finished, status = os.waitpid(pid, 0)
    if matched_error:
        sys.exit(1)
    if matched_success:
        sys.exit(0)
    if timed_out:
        sys.exit(124)
    child_exit = os.waitstatus_to_exitcode(status)
    if child_exit != 0:
        sys.exit(child_exit)
    print("\nSNP repro exited before virtio smoke tests passed", file=sys.stderr)
    sys.exit(1)
except KeyboardInterrupt:
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    raise
PY
