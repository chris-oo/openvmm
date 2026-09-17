# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Restore pinned test inputs from a preserved FVP run, outside cargo's target.

This restores host/guest payloads and package metadata, not BL1/FIP firmware
or the Shrinkwrap executable. Those must be supplied separately.
"""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import zlib


HOST_IMAGE = "3bfd2bf2e55e17f201ab26cdf8f6bfa7baa4dd3a373ca9c738b1e5f48f5ff01f"
HOST_CONFIG = "0728366e9a367fa18f0862c89360e34d92d197f4b91c50f0019953b1ddbff9c3"
HOST_INITRD = "74ecad46de9da08aff7520a93a09fa473276360bcb018e106ae41862a6f2feb2"
GUEST_IMAGE = "6bef4c54ac93d8513ad7f125737c9ff0a8b0b7e34720e77253e2b63460002437"
GUEST_INITRD = "d3ba987d83bd46a60cf2199d7989a7b940499065e1011125775034b5710dad92"
PACKAGE = "7e46a163b6c117fd903e199c55a21d44139f06bfb39ac3045be70dc51dea8785"
OVERLAY = "bd729901876bd7fbecba1b8526d7d40e70657ff9f280023aca3f52502db947f1"
ASSETS = {
    "pci.json": "7ffe3fb55b669d37302a3642eb2c3222ebb67d2a2cd2ab8d7ee792a35f39344d",
    "measurements.json": "b00220ce3b06618fa27e8efdfff98c59b222e3d6f6230753778df837694df637",
    "sample_key/ecp384/bundle_responder.certchain.der": "3ba926d9a62805d775fc99ad7be130b3e4be8d72433b94f408cc5d2775d4f682",
    "sample_key/ecp384/end_responder.key": "654c340cd764f74cb08ef5d4761b96f656cf4497866698586a5f44cb8d79e12c",
    "ahci-disk.img": "281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6",
}


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def require_hash(path, expected):
    if path.is_symlink() or not path.is_file() or digest(path) != expected:
        raise RuntimeError(f"input does not match its pin: {path}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_directory", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    run = args.run_directory.resolve(strict=True)
    destination = args.destination.absolute()
    if ".." in destination.parts or destination.is_symlink():
        raise RuntimeError("destination must not contain traversal or be a symlink")
    destination = destination.resolve()
    if destination == run or run in destination.parents or destination in run.parents:
        raise RuntimeError("source and destination trees must be separate")
    locator = json.loads((run / "outputs/session-result.json").read_text())
    run_id = locator["run_id"]
    if (
        locator.get("schema_version") != 2
        or not isinstance(run_id, str)
        or len(run_id) != 64
        or any(char not in "0123456789abcdef" for char in run_id)
    ):
        raise RuntimeError("unsupported FVP result locator")
    package = run / "outputs" / f"fvp-{run_id}" / "package"
    overlay = Path(__file__).resolve().parent / "fvp-realm-vfio-overlay.yaml"
    copies = [
        (run / "inputs/aarch64/Image", "host-payload/Image", HOST_IMAGE),
        (run / "inputs/aarch64/initrd", "host-payload/initrd", HOST_INITRD),
        (run / "inputs/cca-tdisp-guest/Image", "guest-payload/Image", GUEST_IMAGE),
        (run / "inputs/cca-tdisp-guest/initrd", "guest-payload/guest-initrd.cpio", GUEST_INITRD),
        (package / "cca-3world.yaml", "package/cca-3world.yaml", PACKAGE),
        (
            overlay,
            "cca-tdisp-stage-a/realm-vfio-reference-platform/overlay.yaml",
            OVERLAY,
        ),
    ]
    copies.extend(
        (package / "cca-3world" / name, "package/cca-3world/" + name, expected)
        for name, expected in ASSETS.items()
    )
    for source, _, expected in copies:
        require_hash(source, expected)
    image = (run / "inputs/aarch64/Image").read_bytes()
    marker = image.find(b"IKCFG_ST")
    if marker < 0:
        raise RuntimeError("pinned host kernel has no embedded configuration")
    config = zlib.decompress(image[marker + 8:], wbits=31)
    if hashlib.sha256(config).hexdigest() != HOST_CONFIG:
        raise RuntimeError("embedded host configuration does not match its pin")
    for directory in ["host-payload", "guest-payload", "package", "cca-tdisp-stage-a"]:
        if (destination / directory).exists() or (destination / directory).is_symlink():
            raise RuntimeError(f"refusing to overwrite {destination / directory}")
    for source, relative, expected in copies:
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        with source.open("rb") as stream, target.open("xb") as output:
            shutil.copyfileobj(stream, output)
        require_hash(target, expected)
        print(f"{expected}  {target}")
    with (destination / "host-payload/config").open("xb") as output:
        output.write(config)
    manifest = (
        "architecture=aarch64\n"
        "revision=2b68f486fdbc8d2818309f91199dde46b2b7cdd6\n"
        "kernel_release=7.2.0-rc3-g2b68f486fdbc\n"
        f"config_sha256={HOST_CONFIG}\n"
        f"Image_sha256={HOST_IMAGE}\n"
        "recovery=verified preserved Image and embedded IKCONFIG\n"
    )
    with (destination / "host-payload/manifest.txt").open("x") as output:
        output.write(manifest)
    print("Payloads and package metadata restored. BL1/FIP, DTB and Shrinkwrap are separate inputs.")


if __name__ == "__main__":
    main()
