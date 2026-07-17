#!/bin/bash
# Run the CCA OpenVMM repro in the FVP interactive host, log in automatically,
# launch the staged OpenVMM script, and stop FVP after a known outcome.

set -euo pipefail

HOST_KERNEL="${CCA_REPRO_HOST_KERNEL:-$HOME/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image}"
GUEST_KERNEL="${CCA_REPRO_GUEST_KERNEL:-$HOME/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image}"
OPENVMM_MEMORY="${CCA_REPRO_OPENVMM_MEMORY:-256M}"
LOGS_DIR="${CCA_REPRO_LOGS_DIR:-target/cca-test/kvm-cca/logs/interactive}"
TIMEOUT_SECONDS="${CCA_REPRO_TIMEOUT_SECONDS:-300}"
XFLOWEY_EXIT_TIMEOUT_SECONDS="${CCA_REPRO_XFLOWEY_EXIT_TIMEOUT_SECONDS:-120}"
OPENVMM_EXIT_TIMEOUT_SECONDS="${CCA_REPRO_OPENVMM_EXIT_TIMEOUT_SECONDS:-15}"
LOGIN="${CCA_REPRO_LOGIN:-root}"
REMOTE_SCRIPT="${CCA_REPRO_REMOTE_SCRIPT:-/cca-share/run-openvmm-kvm-cca.sh}"
ERROR_PATTERN="${CCA_REPRO_ERROR_PATTERN:-fatal error|failed to run VP|[Gg]uest crash(?:ed)?|guest halted|triple fault|panicked at|assertion failed|abnormal exit|SIGABRT|core dumped|Internal error|Unhandled|VCPU panic|Kernel panic}"
SHELL_PATTERN="${CCA_REPRO_SHELL_PATTERN:-No root device specified\\. Dropping to a shell\\.|can.t access tty; job control turned off}"

python3 - \
    "$HOST_KERNEL" \
    "$GUEST_KERNEL" \
    "$OPENVMM_MEMORY" \
    "$LOGS_DIR" \
    "$TIMEOUT_SECONDS" \
    "$XFLOWEY_EXIT_TIMEOUT_SECONDS" \
    "$OPENVMM_EXIT_TIMEOUT_SECONDS" \
    "$LOGIN" \
    "$REMOTE_SCRIPT" \
    "$ERROR_PATTERN" \
    "$SHELL_PATTERN" \
    "$@" <<'PY'
import os
import pty
import re
import select
import signal
import subprocess
import sys
import termios
import time

(
    host_kernel,
    guest_kernel,
    openvmm_memory,
    logs_dir,
    timeout_seconds,
    xflowey_exit_timeout_seconds,
    openvmm_exit_timeout_seconds,
    login,
    remote_script,
    error_pattern,
    shell_pattern,
    *extra_args,
) = sys.argv[1:]
timeout_seconds = int(timeout_seconds)
xflowey_exit_timeout_seconds = int(xflowey_exit_timeout_seconds)
openvmm_exit_timeout_seconds = int(openvmm_exit_timeout_seconds)
error = re.compile(error_pattern)
shell_ready = re.compile(shell_pattern)
smoke_success = re.compile(r"(?:^|[\r\n])OVMM_SMOKE_ALL_PASS(?:[\r\n]|$)")
smoke_failure = re.compile(r"(?:^|[\r\n])OVMM_SMOKE_[A-Z_]+_FAIL(?:[\r\n]|$)")
shell_prompt = re.compile(r"(?:^|[\r\n])[^#\r\n]*#\s*$")
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

argv = [
    "cargo",
    "xflowey",
    "kvm-cca-tests",
    "--interactive-host",
    "--host-kernel",
    host_kernel,
    "--guest-kernel",
    guest_kernel,
    "--openvmm-memory",
    openvmm_memory,
    "--logs-dir",
    logs_dir,
    *extra_args,
]


def fvp_pids():
    try:
        output = subprocess.check_output(["ps", "-eo", "pid=,args="], text=True)
    except subprocess.SubprocessError:
        return []
    pids = []
    for line in output.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        pid_text, _, args = stripped.partition(" ")
        if not pid_text.isdigit():
            continue
        argv0 = args.split(" ", 1)[0]
        basename = os.path.basename(argv0)
        if basename.startswith("FVP_Base_RevC"):
            pids.append(int(pid_text))
    return pids


def stop_fvp():
    pids = fvp_pids()
    if not pids:
        return
    print(f"\nStopping FVP_Base_RevC processes: {' '.join(str(pid) for pid in pids)}", flush=True)
    for pid in pids:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not fvp_pids():
            return
        time.sleep(0.25)
    remaining = fvp_pids()
    if remaining:
        print(
            f"Force stopping remaining FVP_Base_RevC processes: {' '.join(str(pid) for pid in remaining)}",
            flush=True,
        )
        for pid in remaining:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def child_exit_code(status):
    if status is None:
        return None
    return os.waitstatus_to_exitcode(status)


def finish(outcome, message, exit_code):
    print(f"\nCCA repro {outcome}: {message}", file=sys.stderr, flush=True)
    sys.exit(exit_code)


def send_remote_script():
    print(f"\nCCA repro: running {remote_script}", file=sys.stderr, flush=True)
    os.write(fd, f"{remote_script}\r".encode())


def send_openvmm_escape():
    print("\nCCA repro: guest reached success; entering OpenVMM control prompt with Ctrl-Q", file=sys.stderr, flush=True)
    os.write(fd, b"\x11")


def send_openvmm_quit():
    print("\nCCA repro: quitting OpenVMM", file=sys.stderr, flush=True)
    os.write(fd, b"q\r")

def send_smoke_test():
    print("\nCCA repro: running virtio block/network smoke tests", file=sys.stderr, flush=True)
    payload = f"{smoke_command}\r".encode()
    for offset in range(0, len(payload), 8):
        os.write(fd, payload[offset : offset + 8])
        time.sleep(0.05)


pid, fd = pty.fork()
if pid == 0:
    os.execvp(argv[0], argv)

attrs = termios.tcgetattr(fd)
attrs[0] &= ~(termios.IXON | termios.IXOFF | termios.IXANY)
termios.tcsetattr(fd, termios.TCSANOW, attrs)

deadline = time.monotonic() + timeout_seconds
buffer = ""
logged_in = False
command_sent = False
matched_error = False
matched_success = False
timed_out = False
child_status = None
done_since = None
last_login_sent = 0.0
openvmm_quit_sent = False
openvmm_quit_command_sent = False
openvmm_quit_deadline = None
guest_shell_ready = False
smoke_sent = False

try:
    while True:
        finished, status = os.waitpid(pid, os.WNOHANG)
        if finished:
            child_status = status
            break

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            timed_out = True
            stop_fvp()
            break

        readable, _, _ = select.select([fd], [], [], min(0.5, remaining))
        if not readable:
            if done_since is not None and time.monotonic() - done_since > 1:
                stop_fvp()
                break
            if openvmm_quit_deadline is not None and time.monotonic() >= openvmm_quit_deadline:
                stop_fvp()
                break
            if logged_in and not command_sent and shell_prompt.search(buffer):
                send_remote_script()
                command_sent = True
                buffer = ""
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

        now = time.monotonic()
        if "buildroot login:" in buffer and not logged_in and now - last_login_sent > 2:
            os.write(fd, f"{login}\r".encode())
            last_login_sent = now
            logged_in = True
            continue

        if logged_in and not command_sent and shell_prompt.search(buffer):
            send_remote_script()
            command_sent = True
            buffer = ""
            continue

        repro_started = command_sent or "Run /init as init process" in buffer or shell_ready.search(buffer)
        if repro_started and error.search(buffer):
            matched_error = True
            stop_fvp()
            break
        if repro_started and shell_ready.search(buffer):
            guest_shell_ready = True
        if guest_shell_ready and shell_prompt.search(buffer) and not smoke_sent:
            send_smoke_test()
            smoke_sent = True
            buffer = ""
            continue
        if smoke_failure.search(buffer):
            matched_error = True
            stop_fvp()
            break
        if smoke_success.search(buffer):
            matched_success = True
            if not openvmm_quit_sent:
                send_openvmm_escape()
                openvmm_quit_sent = True
                openvmm_quit_deadline = now + openvmm_exit_timeout_seconds
                buffer = ""
            continue

        if openvmm_quit_sent and not openvmm_quit_command_sent and "openvmm>" in buffer:
            send_openvmm_quit()
            openvmm_quit_command_sent = True
            buffer = ""
            continue

        if openvmm_quit_sent and shell_prompt.search(buffer):
            stop_fvp()
            break
        if openvmm_quit_deadline is not None and now >= openvmm_quit_deadline:
            stop_fvp()
            break

    stop_deadline = time.monotonic() + xflowey_exit_timeout_seconds
    while child_status is None and time.monotonic() < stop_deadline:
        readable, _, _ = select.select([fd], [], [], 0.5)
        if readable:
            try:
                data = os.read(fd, 4096)
            except OSError:
                data = b""
            if data:
                text = data.decode(errors="replace")
                print(text, end="", flush=True)
                buffer = (buffer + text)[-8192:]
                repro_started = (
                    command_sent
                    or "Run /init as init process" in buffer
                    or shell_ready.search(buffer)
                )
                if repro_started and error.search(buffer):
                    matched_error = True
                    stop_fvp()
                if repro_started and shell_ready.search(buffer):
                    guest_shell_ready = True
                if guest_shell_ready and shell_prompt.search(buffer) and not smoke_sent:
                    send_smoke_test()
                    smoke_sent = True
                    buffer = ""
                if smoke_failure.search(buffer):
                    matched_error = True
                    stop_fvp()
                if smoke_success.search(buffer):
                    matched_success = True
                    if not openvmm_quit_sent:
                        send_openvmm_escape()
                        openvmm_quit_sent = True
                        openvmm_quit_deadline = time.monotonic() + openvmm_exit_timeout_seconds
                        buffer = ""
                if openvmm_quit_sent and not openvmm_quit_command_sent and "openvmm>" in buffer:
                    send_openvmm_quit()
                    openvmm_quit_command_sent = True
                    buffer = ""
                if openvmm_quit_sent and shell_prompt.search(buffer):
                    stop_fvp()
                if openvmm_quit_deadline is not None and time.monotonic() >= openvmm_quit_deadline:
                    stop_fvp()
        finished, status = os.waitpid(pid, os.WNOHANG)
        if finished:
            child_status = status
            break

    if child_status is None:
        print(
            f"\nCCA repro FAILURE: xflowey did not exit within {xflowey_exit_timeout_seconds} seconds after stopping FVP",
            file=sys.stderr,
            flush=True,
        )
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        _, child_status = os.waitpid(pid, 0)
        sys.exit(125)

    if matched_error:
        finish("FAILURE", "matched an error/crash pattern", 1)
    if matched_success:
        finish("SUCCESS", "virtio block and network smoke tests passed", 0)
    if timed_out:
        finish("FAILURE", f"timed out after {timeout_seconds} seconds", 124)

    exit_code = child_exit_code(child_status)
    if exit_code not in (None, 0):
        finish("FAILURE", f"command exited unexpectedly with status {exit_code}", exit_code)
    finish("FAILURE", "command exited before virtio smoke tests passed", 1)
except KeyboardInterrupt:
    stop_fvp()
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    raise
PY
