# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Rebuild the pinned FVP firmware, not the host/guest kernel or model DA assets.

Run from the repository root with Python 3.11+, Git, and Docker:
  python3 petri/incubator/platforms/rebuild-realm-firmware.py \
    --source-package path/to/preserved/cca-3world.yaml \
    --dtb path/to/verified/dt_bootargs.dtb \
    --source-cache ~/.shrinkwrap/build/source/cca-3world

The adjacent JSON pins inputs. No shell from the preserved YAML is executed.
Sources, logs, build trees, and published artifacts stay under
.packages/cca-tdisp-fvp, outside Cargo's target directory. Each run uses new
source copies without Git alternates. Existing runs are never overwritten.
The optional cache supplies Git objects only; its worktrees remain unchanged.
EDK2 is rebuilt, while the exact hashed DTB is reused. Rebuild hashes can differ
from the lost firmware. Review manifest.json before updating platform pins.
The pinned container includes the compiler and model; no model is booted here.
"""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys


BUILD = r"""set -ex
export CROSS_COMPILE=aarch64-none-elf-
export TMPDIR="$B/tmp"
export HOME="$B/home"
mkdir -p "$TMPDIR" "$HOME"
{
    aarch64-none-elf-gcc --version
    aarch64-none-elf-ld --version
    gcc --version
    cmake --version
    make --version
    python3 --version
    dtc --version
    iasl -v
    FVP_Base_RevC-2xAEMvA --version
} > "$B/toolchain.txt" 2>&1
grep -F '11.31.28' "$B/toolchain.txt"
make -C "$S/acpica" -j"$JOBS"
(
    export WORKSPACE="$B/edk2"
    export CONF_PATH="$WORKSPACE/Conf"
    export EDK_TOOLS_PATH="$S/edk2/BaseTools"
    export GCC_AARCH64_PREFIX="$CROSS_COMPILE"
    export PACKAGES_PATH="$S/edk2:$S/edk2-platforms"
    export IASL_PREFIX="$S/acpica/generate/unix/bin/"
    export PYTHON_COMMAND=/usr/bin/python3
    mkdir -p "$CONF_PATH"
    cd "$S/edk2"
    source edksetup.sh --reconfig
    make -C "$EDK_TOOLS_PATH" -j"$JOBS"
    build -a AARCH64 -b RELEASE -n "$JOBS" -t GCC \
        -p Platform/ARM/VExpressPkg/ArmVExpress-FVP-AArch64.dsc \
        -D EDK2_OUT_DIR="$B/edk2" \
        --pcd PcdShellDefaultDelay=0 --pcd PcdUefiShellDefaultBootEnable=1
)
cmake -DCMAKE_BUILD_TYPE=Debug -DLOG_LEVEL=40 -DRMM_CONFIG=fvp_defcfg \
    -DRMM_SANITIZERS= -DRMM_TOOLCHAIN=gnu -S "$S/rmm" -B "$B/rmm"
cmake --build "$B/rmm" -j "$JOBS"
make -C "$S/tfa" ARM_ARCH_MAJOR=9 ARM_ARCH_MINOR=2 \
    ARM_DISABLE_TRUSTED_WDOG=1 BRANCH_PROTECTION=1 \
    DEBUG=1 ENABLE_FEAT_RME=1 ENABLE_RMM=1 FEATURE_DETECTION=0 \
    CTX_INCLUDE_AARCH32_REGS=0 LOG_LEVEL=40 PLAT=fvp RMM_V1_COMPAT=0 \
    FVP_HW_CONFIG_DTS=fdts/fvp-base-gicv3-psci-1t.dts \
    FVP_HW_CONFIG="$I/dt_bootargs.dtb" \
    RMM="$B/rmm/Debug/rmm.img" \
    BL33="$B/edk2/RELEASE_GCC/FV/FVP_AARCH64_EFI.fd" \
    BUILD_BASE="$B/tfa" -j"$JOBS" all fip
"$S/tfa/tools/fiptool/fiptool" info "$B/tfa/fvp/debug/fip.bin"
mkdir "$B/fip-unpacked"
"$S/tfa/tools/fiptool/fiptool" unpack --out "$B/fip-unpacked" \
    "$B/tfa/fvp/debug/fip.bin"
cmp "$B/fip-unpacked/rmm-fw.bin" "$B/rmm/Debug/rmm.img"
cmp "$B/fip-unpacked/nt-fw.bin" "$B/edk2/RELEASE_GCC/FV/FVP_AARCH64_EFI.fd"
cmp "$B/fip-unpacked/hw-config.bin" "$I/dt_bootargs.dtb"
"""


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def require_hash(path, expected):
    if path.is_symlink() or not path.is_file() or digest(path) != expected:
        raise RuntimeError(f"Input does not match its pin: {path}")


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


class Runner:
    def __init__(self, log):
        self.log = log

    def run(self, *args, cwd=None):
        command = [str(arg) for arg in args]
        with self.log.open("a") as stream:
            stream.write(f"\n$ {shlex.join(command)}\n")
            stream.flush()
            result = subprocess.run(
                command, cwd=cwd, stdout=subprocess.PIPE, stderr=stream, text=True
            )
            stream.write(result.stdout)
        if result.returncode:
            raise RuntimeError(f"Command failed ({result.returncode}); see {self.log}")
        return result.stdout.strip()

    def git(self, repo, *args):
        if (repo / ".jj").exists() or not (repo / ".git").exists():
            raise RuntimeError(f"Not a standalone Git repository: {repo}")
        return self.run("git", "-C", repo, *args)


def clone_source(runner, destination, url, revision, cache, records):
    if destination.exists():
        raise RuntimeError(f"Refusing to overwrite source: {destination}")
    use_cache = cache and (cache / ".git").exists() and not (cache / ".jj").exists()
    origin = str(cache) if use_cache else url
    runner.run(
        "git", "clone", "--quiet", "--no-hardlinks", "--dissociate",
        "--no-checkout", origin, destination,
    )
    runner.git(destination, "remote", "set-url", "origin", url)
    # A cached branch need not contain the required commit.
    probe = subprocess.run(
        ["git", "-C", str(destination), "cat-file", "-e", revision + "^{commit}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if probe.returncode:
        runner.git(destination, "fetch", "--quiet", "origin", revision)
    runner.git(destination, "checkout", "--quiet", "--detach", revision)
    if runner.git(destination, "rev-parse", "HEAD") != revision:
        raise RuntimeError(f"Incorrect source revision: {destination}")
    if (destination / ".git/objects/info/alternates").exists():
        raise RuntimeError(f"Source depends on external objects: {destination}")
    if runner.git(destination, "status", "--porcelain", "--ignore-submodules=none"):
        raise RuntimeError(f"Source is not clean: {destination}")
    records.append(
        {"path": str(destination), "url": url, "revision": revision, "cache": origin}
    )
    if not (destination / ".gitmodules").exists():
        return
    entries = runner.git(
        destination, "config", "--file", ".gitmodules", "--get-regexp", r"^submodule\..*\.path$"
    )
    for entry in entries.splitlines():
        key, path = entry.split(None, 1)
        if Path(path).is_absolute() or ".." in Path(path).parts:
            raise RuntimeError(f"Unsafe submodule path: {path}")
        module_url = runner.git(
            destination, "config", "--file", ".gitmodules", "--get", key[:-4] + "url"
        )
        if not module_url.startswith("https://"):
            raise RuntimeError(f"Expected an absolute HTTPS submodule URL: {module_url}")
        tree = runner.git(destination, "ls-tree", "HEAD", "--", path).split()
        if len(tree) < 3 or tree[:2] != ["160000", "commit"]:
            raise RuntimeError(f"Missing pinned gitlink: {path}")
        child = destination / path
        if child.exists():
            child.rmdir()  # Git creates an empty gitlink directory at checkout.
        clone_source(
            runner, child, module_url, tree[2], cache / path if use_cache else None, records
        )
        runner.git(destination, "config", key[:-4] + "url", module_url)
        runner.git(destination, "config", key[:-4] + "active", "true")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-package", type=Path, required=True)
    parser.add_argument("--dtb", type=Path, required=True)
    parser.add_argument("--source-cache", type=Path)
    parser.add_argument("--name", default=datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ"))
    parser.add_argument("--jobs", type=int, default=8)
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", args.name) or args.jobs < 1:
        parser.error("Use a simple run name and a positive job count")

    recipe = Path(__file__).resolve()
    pins_path = recipe.with_suffix(".json")
    pins = json.loads(pins_path.read_text())
    root = recipe.parents[3] / ".packages/cca-tdisp-fvp"
    source_package = args.source_package.resolve()
    dtb = args.dtb.resolve()
    require_hash(source_package, pins["source_package_sha256"])
    require_hash(dtb, pins["dtb_sha256"])
    paths = {key: root / key / args.name for key in ("sources", "build", "inputs", "artifacts")}
    staging = root / "artifacts" / (args.name + ".staging")
    for path in (*paths.values(), staging):
        if path.exists() or path.is_symlink():
            raise RuntimeError(f"Refusing to overwrite an existing run: {path}")
    for key in ("sources", "build", "inputs"):
        paths[key].mkdir(parents=True)
    staging.mkdir(parents=True)
    build = paths["build"]
    runner = Runner(build / "commands.log")
    shutil.copyfile(source_package, paths["inputs"] / "cca-3world.yaml")
    shutil.copyfile(dtb, paths["inputs"] / "dt_bootargs.dtb")
    shutil.copyfile(recipe, build / recipe.name)
    shutil.copyfile(pins_path, build / pins_path.name)
    (build / "build.sh").write_text(BUILD)
    image = json.loads(runner.run("docker", "image", "inspect", pins["container"]))[0]
    write_json(build / "container.json", image)
    host_tools = {
        "docker": runner.run("docker", "--version"),
        "git": runner.run("git", "--version"),
        "python": sys.version,
    }
    records = []
    for name, source in pins["sources"].items():
        print(f"Cloning pinned {name} and submodules", flush=True)
        clone_source(
            runner,
            paths["sources"] / name,
            source["url"],
            source["revision"],
            args.source_cache.resolve() / name if args.source_cache else None,
            records,
        )
        write_json(build / "sources.json", records)
    print(f"Building firmware; log: {runner.log}", flush=True)
    runner.run(
        "docker", "run", "--rm", "--network=none",
        "--user", f"{os.getuid()}:{os.getgid()}",
        "--mount", f"type=bind,src={root},dst=/firmware",
        "--env", f"S=/firmware/sources/{args.name}",
        "--env", f"B=/firmware/build/{args.name}",
        "--env", f"I=/firmware/inputs/{args.name}",
        "--env", f"JOBS={args.jobs}",
        "--entrypoint", "bash", pins["container"],
        f"/firmware/build/{args.name}/build.sh",
    )
    source_changes = []
    for record in records:
        repo = Path(record["path"])
        status = runner.git(
            repo, "status", "--porcelain", "--untracked-files=no", "--ignore-submodules=all"
        )
        if status:
            patch = build / "source-changes" / repo.relative_to(paths["sources"]) / "build.patch"
            patch.parent.mkdir(parents=True, exist_ok=True)
            patch.write_text(runner.git(repo, "diff", "--binary", "HEAD", "--"))
            source_changes.append({"path": str(repo), "status": status, "patch_sha256": digest(patch)})
    outputs = {
        "bl1.bin": build / "tfa/fvp/debug/bl1.bin",
        "bl2.bin": build / "tfa/fvp/debug/bl2.bin",
        "bl31.bin": build / "tfa/fvp/debug/bl31.bin",
        "fip.bin": build / "tfa/fvp/debug/fip.bin",
        "rmm.img": build / "rmm/Debug/rmm.img",
        "FVP_AARCH64_EFI.fd": build / "edk2/RELEASE_GCC/FV/FVP_AARCH64_EFI.fd",
        "dt_bootargs.dtb": paths["inputs"] / "dt_bootargs.dtb",
    }
    hashes = {}
    for name, source in outputs.items():
        if not source.is_file() or source.stat().st_size == 0 or source.is_symlink():
            raise RuntimeError(f"Missing or invalid output: {source}")
        shutil.copyfile(source, staging / name)
        hashes[name] = {"sha256": digest(staging / name), "bytes": source.stat().st_size}
    require_hash(staging / "dt_bootargs.dtb", pins["dtb_sha256"])
    manifest = {
        "schema": 1,
        "created_utc": datetime.now(timezone.utc).isoformat(),
        "pins": pins,
        "container_id": image["Id"],
        "host_tools": host_tools,
        "sources": records,
        "build_generated_source_changes": source_changes,
        "recipe_sha256": digest(recipe),
        "pins_sha256": digest(pins_path),
        "build_script_sha256": digest(build / "build.sh"),
        "toolchain": (build / "toolchain.txt").read_text(),
        "commands_log": str(runner.log),
        "commands_log_sha256": digest(runner.log),
        "outputs": hashes,
        "efi": "rebuilt from pinned EDK2, edk2-platforms and ACPICA sources",
        "dtb": "reused exact pinned bytes",
        "validation": "build succeeded; fiptool unpack matches RMM, BL33 and HW_CONFIG",
        "model_test": "not run; parent must update pins and run the full TDISP VMM test",
        "reproducibility": "same source pins and flags, not a claim of identical lost bytes",
    }
    write_json(staging / "manifest.json", manifest)
    (staging / "SHA256SUMS").write_text(
        "".join(f"{info['sha256']}  {name}\n" for name, info in sorted(hashes.items()))
        + f"{digest(staging / 'manifest.json')}  manifest.json\n"
    )
    staging.rename(paths["artifacts"])
    print(f"Published: {paths['artifacts']}")
    print((paths["artifacts"] / "SHA256SUMS").read_text(), end="")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError) as error:
        sys.exit(str(error))
