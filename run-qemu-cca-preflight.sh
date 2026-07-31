#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
TIMEOUT_SECONDS="${QEMU_CCA_PREFLIGHT_TIMEOUT_SECONDS:-1800}"
LOG_DIR="${QEMU_CCA_LOG_DIR:-$REPO_ROOT/target/cca-qemu/logs}"
HOST_LOG="$LOG_DIR/host-console.log"
STATUS_FILE="$LOG_DIR/preflight.status"
TIMING_FILE="$LOG_DIR/preflight-timing.txt"
mkdir -p "$LOG_DIR"
rm -f "$STATUS_FILE" "$TIMING_FILE"

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
status_pattern = re.compile(r"QEMU_CCA_PREFLIGHT_STATUS=(\d+)")
failure_pattern = re.compile(r"Kernel panic|Unable to mount root|No working init")

argv = [os.path.join(repo_root, "run-qemu-cca-host.sh")]
deadline = time.monotonic() + timeout_seconds
buffer = ""
logged_in = False
command_sent = False
status = None
outcome = "exited"
child_exit = None

with open(host_log, "w", encoding="utf-8", errors="replace") as log:
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(repo_root)
        os.execv(argv[0], argv)
    try:
        while time.monotonic() < deadline:
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
                else:
                    outcome = "pty_error"
                break
            if not data:
                finished, child_status = os.waitpid(pid, os.WNOHANG)
                if finished:
                    child_exit = os.waitstatus_to_exitcode(child_status)
                    outcome = f"child_exit_{child_exit}"
                else:
                    outcome = "eof"
                break
            text = data.decode(errors="replace")
            print(text, end="", flush=True)
            log.write(text)
            log.flush()
            buffer = (buffer + text)[-16384:]

            if failure_pattern.search(buffer):
                outcome = "boot_failure"
                break
            if login_prompt.search(buffer) and not logged_in:
                os.write(fd, b"root\r")
                logged_in = True
                buffer = ""
                continue
            if logged_in and shell_prompt.search(buffer) and not command_sent:
                os.write(
                    fd,
                    b"/cca-share/kvm_cca_preflight; "
                    b"rc=$?; echo QEMU_CCA_PREFLIGHT_STATUS=$rc; "
                    b"poweroff -f\r",
                )
                command_sent = True
                buffer = ""
                continue
            match = status_pattern.search(buffer)
            if match:
                status = int(match.group(1))
                outcome = "completed"
                break
        else:
            outcome = "timeout"
    finally:
        try:
            os.killpg(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            _, child_status = os.waitpid(pid, 0)
            child_exit = os.waitstatus_to_exitcode(child_status)
        except ChildProcessError:
            pass

elapsed = time.monotonic() - started
with open(timing_file, "w", encoding="utf-8") as timing:
    timing.write(f"preflight_wall_seconds={elapsed:.3f}\n")
    timing.write(f"outcome={outcome}\n")
    if child_exit is not None:
        timing.write(f"child_exit={child_exit}\n")

if status is None:
    with open(status_file, "w", encoding="utf-8") as status_output:
        status_output.write(f"{outcome}\n")
    print(
        f"QEMU CCA preflight did not complete ({outcome}); see {host_log}",
        file=sys.stderr,
    )
    if outcome == "timeout":
        sys.exit(124)
    if child_exit not in (None, 0):
        sys.exit(child_exit)
    sys.exit(1)
with open(status_file, "w", encoding="utf-8") as status_output:
    status_output.write(f"{status}\n")
if status != 0:
    print(f"QEMU CCA preflight failed with status {status}; see {host_log}", file=sys.stderr)
    sys.exit(status)
print(f"QEMU CCA preflight passed; logs: {host_log}")
PY
