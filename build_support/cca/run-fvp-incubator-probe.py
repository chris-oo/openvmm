#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Run the noninteractive FVP CCA incubator platform probe."""

import argparse
import fcntl
import hashlib
import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

SHRINKWRAP_IMAGE = "shrinkwraptool/base-slim:2026.9.0.dev0"
PIPETTE_PORT = 0x1337
PORT_COLLISION_EXIT = 75


class PortCollision(RuntimeError):
    """The selected FVP host-forward port was claimed by another process."""


def regular_file(path: Path, label: str) -> Path:
    path = path.resolve()
    if not path.is_file():
        raise RuntimeError(f"{label} is not a regular file: {path}")
    return path


def directory(path: Path, label: str) -> Path:
    path = path.resolve()
    if not path.is_dir():
        raise RuntimeError(f"{label} is not a directory: {path}")
    return path


def reserve_port() -> socket.socket:
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    return listener


def docker_containers(label: str) -> set[str]:
    output = subprocess.check_output(
        ["docker", "ps", "-q", "--filter", f"label={label}"],
        text=True,
    )
    return set(output.split())


def stop_container(container: str) -> None:
    subprocess.run(
        ["docker", "stop", "--timeout", "5", container],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def wait_for_marker(path: Path, marker: str, process: subprocess.Popen, timeout: int) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file() and marker in path.read_text(errors="replace"):
            return
        status = process.poll()
        if status is not None:
            raise RuntimeError(
                f"Shrinkwrap exited with status {status} before {marker!r}"
            )
        time.sleep(0.5)
    raise RuntimeError(f"timed out waiting for {marker!r}")


def assert_loopback_listener(port: int) -> None:
    output = subprocess.check_output(["ss", "-ltnp"], text=True)
    expected = f"127.0.0.1:{port}"
    matching = [line for line in output.splitlines() if expected in line]
    if not matching:
        raise PortCollision(f"FVP did not bind the pipette port: {expected}")
    if not any("FVP_Base" in line for line in matching):
        raise PortCollision(f"pipette port is owned by another process: {expected}")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def atomic_write(path: Path, contents: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        "w", dir=path.parent, prefix=f".{path.name}.", delete=False
    ) as stream:
        temporary = Path(stream.name)
        stream.write(contents)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)
    directory_fd = os.open(path.parent, os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)


def remove_durable(path: Path) -> None:
    path.unlink(missing_ok=True)
    directory_fd = os.open(path.parent, os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--platform-root", required=True, type=Path)
    parser.add_argument("--output-root", required=True, type=Path)
    parser.add_argument("--host-kernel", required=True, type=Path)
    parser.add_argument("--host-rootfs", required=True, type=Path)
    parser.add_argument("--share-dir", required=True, type=Path)
    parser.add_argument("--pipette-probe", required=True, type=Path)
    parser.add_argument("--readiness-timeout", type=int, default=300)
    parser.add_argument("--ready-marker", default="PIPETTE READY")
    parser.add_argument("--launch-only", action="store_true")
    parser.add_argument("--endpoint-file", type=Path)
    parser.add_argument("--session-timeout", type=int, default=1800)
    parser.add_argument("--lock-timeout", type=int)
    parser.add_argument("--guest-pipette-path", default="/share/pipette")
    parser.add_argument("--consoles", default="host,edk2,rmm")
    parser.add_argument("--primary-console", default="host")
    parser.add_argument("--internal-attempt", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()

    if not args.internal_attempt:
        environment = os.environ.copy()
        for attempt in range(1, 4):
            result = subprocess.run(
                [sys.executable, str(Path(__file__).resolve()), *sys.argv[1:], "--internal-attempt"],
                env=environment,
                check=False,
            )
            if result.returncode != PORT_COLLISION_EXIT:
                return result.returncode
            print(
                f"warning: FVP port collision on attempt {attempt}; retrying",
                file=sys.stderr,
            )
            environment.pop("OPENVMM_FVP_PROBE_PIPETTE_PORT", None)
        raise RuntimeError("FVP port forwarding collided on all three attempts")
    lock_timeout = args.lock_timeout or args.session_timeout * 8

    platform_root = directory(args.platform_root, "FVP platform root")
    output_root = args.output_root.resolve()
    if output_root == platform_root or platform_root in output_root.parents:
        raise RuntimeError("probe output root must be outside the FVP platform root")
    host_kernel = regular_file(args.host_kernel, "CCA host kernel")
    host_rootfs = regular_file(args.host_rootfs, "CCA host rootfs")
    share_dir = directory(args.share_dir, "CCA share")
    pipette_probe = regular_file(args.pipette_probe, "pipette TCP probe")
    shrinkwrap_root = directory(platform_root / "shrinkwrap", "Shrinkwrap checkout")
    shrinkwrap = regular_file(
        shrinkwrap_root / "venv/bin/shrinkwrap", "Shrinkwrap executable"
    )
    overlay = regular_file(
        shrinkwrap_root / "config/kvm_cca_planes.yaml", "KVM CCA overlay"
    )
    source_rootfs = regular_file(
        platform_root / "kvm-cca/rootfs.ext2", "FVP CCA source rootfs"
    )

    output_root.mkdir(parents=True, exist_ok=True)
    logs = output_root / "logs"
    shutil.rmtree(logs, ignore_errors=True)
    logs.mkdir()
    for name in [
        "container-id.txt",
        "fvp-argv.txt",
        "manifest.txt",
        "probe-overlay.yaml",
        "shrinkwrap-command.txt",
    ]:
        (output_root / name).unlink(missing_ok=True)
    status_path = output_root / "status.txt"
    status_path.write_text("running\n")

    lock_path = platform_root / ".openvmm-fvp-cca.lock"
    lease_path = platform_root / ".openvmm-fvp-cca-lease.json"
    with lock_path.open("w") as lock:
        lock_deadline = time.monotonic() + lock_timeout
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= lock_deadline:
                    status_path.write_text("failed\n")
                    raise RuntimeError("timed out acquiring FVP CCA platform lock")
                time.sleep(0.25)

        platform_hash = hashlib.sha256(str(platform_root).encode()).hexdigest()[:16]
        platform_label = f"openvmm.fvp-cca-platform={platform_hash}"
        stale = docker_containers(platform_label)
        for container in stale:
            stop_container(container)
        if docker_containers(platform_label):
            status_path.write_text("failed\n")
            raise RuntimeError("stale FVP CCA containers could not be removed")
        remove_durable(lease_path)

        run_id = uuid.uuid4().hex
        run_label = f"openvmm.fvp-cca-run={run_id}"
        process_start = Path("/proc/self/stat").read_text().split()[21]

        def write_lease(state: str, container_id: str | None = None) -> None:
            atomic_write(
                lease_path,
                json.dumps(
                    {
                        "state": state,
                        "run_id": run_id,
                        "owner_pid": os.getpid(),
                        "owner_start": process_start,
                        "platform_root": str(platform_root),
                        "platform_hash": platform_hash,
                        "container_id": container_id,
                    },
                    sort_keys=True,
                )
                + "\n",
            )

        write_lease("reserved")
        ssh_reservation = reserve_port()
        forced_pipette_port = os.environ.get("OPENVMM_FVP_PROBE_PIPETTE_PORT")
        pipette_reservation = None if forced_pipette_port else reserve_port()
        ssh_port = ssh_reservation.getsockname()[1]
        pipette_port = (
            int(forced_pipette_port)
            if forced_pipette_port
            else pipette_reservation.getsockname()[1]
        )
        generated_overlay = output_root / "probe-overlay.yaml"
        generated_overlay.write_text(
            f"""%YAML 1.2
---
run:
  terminals:
    bp.terminal_0:
      friendly: host
      type: stdout
      logfile: {logs / "host.log"}
    bp.terminal_1:
      friendly: edk2
      type: stdout
      logfile: {logs / "edk2.log"}
    bp.terminal_3:
      friendly: rmm
      type: stdout
      logfile: {logs / "rmm.log"}
  params:
    -C bp.hostbridge.userNetPorts: 127.0.0.1:{ssh_port}=22,127.0.0.1:{pipette_port}={PIPETTE_PORT}
"""
        )

        command = [
            str(shrinkwrap),
            "--runtime=docker",
            f"--image={SHRINKWRAP_IMAGE}",
            "run",
            "--no-color",
            "--overlay",
            str(overlay),
            "--overlay",
            str(generated_overlay),
            "cca-3world.yaml",
            "--rtvar",
            f"ROOTFS={host_rootfs}",
            "--rtvar",
            f"KERNEL={host_kernel}",
            "--rtvar",
            f"SHARE={share_dir}",
            "--rtvar",
            "CMDLINE=console=ttyAMA0 earlycon=pl011,0x1c090000 root=/dev/vda "
            "ip=dhcp incubator.mount_tag=FM incubator.network=dhcp",
        ]
        environment = os.environ.copy()
        environment["VIRTUAL_ENV"] = str(shrinkwrap_root / "venv")
        environment["PATH"] = (
            f"{shrinkwrap_root / 'venv/bin'}:{environment.get('PATH', '')}"
        )
        environment["SHRINKWRAP_CONFIG"] = str(shrinkwrap_root / "config")
        environment["TUXMAKE_DOCKER_RUN"] = (
            f"--label openvmm.fvp-cca=true "
            f"--label {platform_label} --label {run_label}"
        )

        script_log = logs / "shrinkwrap.log"
        script_command = [
            "script",
            "-qefc",
            shlex.join(command),
            str(script_log),
        ]
        process = None
        containers: set[str] = set()
        succeeded = False

        def interrupted(signum, _frame):
            raise RuntimeError(f"interrupted by signal {signum}")

        old_int = signal.signal(signal.SIGINT, interrupted)
        old_term = signal.signal(signal.SIGTERM, interrupted)
        try:
            dry_run = subprocess.run(
                command[:3] + ["run", "--dry-run"] + command[4:],
                cwd=shrinkwrap_root,
                env=environment,
                text=True,
                capture_output=True,
                check=True,
            )
            (output_root / "fvp-argv.txt").write_text(dry_run.stdout)
            (output_root / "shrinkwrap-command.txt").write_text(
                shlex.join(command) + "\n"
            )

            ssh_reservation.close()
            if pipette_reservation is not None:
                pipette_reservation.close()
            process = subprocess.Popen(
                script_command,
                cwd=shrinkwrap_root,
                env=environment,
                start_new_session=True,
            )
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                containers = docker_containers(run_label)
                if containers:
                    break
                if process.poll() is not None:
                    break
                time.sleep(0.25)
            if len(containers) != 1:
                raise RuntimeError(
                    f"expected one new Shrinkwrap container, found {sorted(containers)}"
                )
            container = next(iter(containers))
            (output_root / "container-id.txt").write_text(container + "\n")
            write_lease("container-started", container)

            wait_for_marker(
                logs / "host.log",
                args.ready_marker,
                process,
                args.readiness_timeout,
            )
            assert_loopback_listener(pipette_port)
            subprocess.run(
                [
                    str(pipette_probe),
                    "--port",
                    str(pipette_port),
                    "--output-dir",
                    str(output_root / "pipette"),
                    "/bin/true",
                ],
                check=True,
                timeout=120,
            )
            try:
                return_code = process.wait(timeout=60)
            except subprocess.TimeoutExpired as error:
                raise RuntimeError("Shrinkwrap did not exit after L1 poweroff") from error
            if return_code != 0:
                raise RuntimeError(f"Shrinkwrap exited with status {return_code}")

            manifest = output_root / "manifest.txt"
            manifest.write_text(
                "\n".join(
                    [
                        f"platform_root={platform_root}",
                        f"shrinkwrap={shrinkwrap}",
                        f"shrinkwrap_image={SHRINKWRAP_IMAGE}",
                        f"host_kernel={host_kernel}",
                        f"host_kernel_sha256={sha256(host_kernel)}",
                        f"host_rootfs={host_rootfs}",
                        f"host_rootfs_sha256={sha256(host_rootfs)}",
                        f"source_rootfs={source_rootfs}",
                        f"share_dir={share_dir}",
                        f"ssh_port={ssh_port}",
                        f"pipette_port={pipette_port}",
                        f"container_id={container}",
                        f"run_id={run_id}",
                    ]
                )
                + "\n"
            )
            succeeded = True
        except BaseException:
            status_path.write_text("failed\n")
            raise
        finally:
            write_lease(
                "cleanup",
                next(iter(containers)) if len(containers) == 1 else None,
            )
            signal.signal(signal.SIGINT, old_int)
            signal.signal(signal.SIGTERM, old_term)
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)
            ssh_reservation.close()
            if pipette_reservation is not None:
                pipette_reservation.close()
            containers |= docker_containers(run_label)
            for container in containers:
                stop_container(container)
            remaining = docker_containers(run_label)
            if remaining:
                status_path.write_text("failed\n")
                raise RuntimeError(
                    f"FVP probe containers remain after cleanup: {sorted(remaining)}"
                )
            remove_durable(lease_path)
        if not succeeded:
            status_path.write_text("failed\n")
            raise RuntimeError("FVP CCA probe did not complete")
        status_path.write_text("passed\n")
        return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except PortCollision as error:
        print(f"port collision: {error}", file=sys.stderr)
        sys.exit(PORT_COLLISION_EXIT)
    except Exception as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
