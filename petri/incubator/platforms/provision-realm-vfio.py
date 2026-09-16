# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Snapshot existing Stage A inputs; never build firmware or modify a toolchain.

Run from the repository root: python3 petri/incubator/platforms/provision-realm-vfio.py
The model key is the upstream public sample key, not a production credential.
"""

import hashlib
from pathlib import Path
import shutil


SOURCE_PACKAGE_SHA256 = "56bb44c6855d1661e948b32bf166db5e23beb19fa943be7a610b7155053c7083"
ASSETS = {
    "bl1.bin": "7e05af43b84d1ca26134c4de572754cbae02f9aeaf042d13ff2b00c04076fe5f",
    "fip.bin": "7cb0bf5e6227de272180b8414870dafa8ca8363cb22110f7242d7bab21ded7ef",
    "dt_bootargs.dtb": "3a2e6b85fc1c10f17cab2f6ec7eadb63b18c6f3d08a74e38eeddc6f587ea3630",
    "measurements.json": "b00220ce3b06618fa27e8efdfff98c59b222e3d6f6230753778df837694df637",
    "sample_key/ecp384/bundle_responder.certchain.der": "3ba926d9a62805d775fc99ad7be130b3e4be8d72433b94f408cc5d2775d4f682",
    "sample_key/ecp384/end_responder.key": "654c340cd764f74cb08ef5d4761b96f656cf4497866698586a5f44cb8d79e12c",
}
REFERENCE_DISK_SHA256 = "281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6"


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def require_hash(path, expected):
    if not path.is_file() or path.is_symlink() or digest(path) != expected:
        raise RuntimeError(f"input does not match its pin: {path}")


def main():
    stage = Path("target/cca-tdisp-stage-a")
    source = stage / "package"
    destination = stage / "realm-vfio-reference-platform"
    reference_disk = stage / "runs/run-20260914-3/package/cca-3world/ahci-disk.img"
    templates = Path("petri/incubator/platforms")
    require_hash(
        templates / "fvp-realm-vfio-pci.json",
        "7ffe3fb55b669d37302a3642eb2c3222ebb67d2a2cd2ab8d7ee792a35f39344d",
    )
    require_hash(
        templates / "fvp-realm-vfio-overlay.yaml",
        "bd729901876bd7fbecba1b8526d7d40e70657ff9f280023aca3f52502db947f1",
    )
    require_hash(source / "cca-3world.yaml", SOURCE_PACKAGE_SHA256)
    for name, expected in ASSETS.items():
        require_hash(source / "cca-3world" / name, expected)
    require_hash(reference_disk, REFERENCE_DISK_SHA256)
    text = (source / "cca-3world.yaml").read_text()
    for block in [
        "  - bash ${artifact:FIX_PCI_JSON_FILE} ${param:packagedir}\n",
        "    SAMPLE_KEY:\n      type: path\n      value: ${artifact:SAMPLE_KEY}\n      options: !!set {}\n",
    ]:
        if text.count(block) != 1:
            raise RuntimeError("unexpected Stage A package structure")
        text = text.replace(block, "")
    if hashlib.sha256(text.encode()).hexdigest() != "7e46a163b6c117fd903e199c55a21d44139f06bfb39ac3045be70dc51dea8785":
        raise RuntimeError("derived package does not match its pin")
    # Existing destinations must not be silently repaired or overwritten.
    destination.mkdir()
    package = destination / "package"
    (package / "cca-3world").mkdir(parents=True)
    (package / "cca-3world.yaml").write_text(text)
    for name, expected in ASSETS.items():
        target = package / "cca-3world" / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source / "cca-3world" / name, target)
        require_hash(target, expected)
    target_disk = package / "cca-3world/ahci-disk.img"
    shutil.copyfile(reference_disk, target_disk)
    require_hash(target_disk, REFERENCE_DISK_SHA256)
    shutil.copyfile(templates / "fvp-realm-vfio-pci.json", package / "cca-3world/pci.json")
    shutil.copyfile(templates / "fvp-realm-vfio-overlay.yaml", destination / "overlay.yaml")
    for path in sorted(destination.rglob("*")):
        if path.is_file():
            print(f"{digest(path)}  {path}")


if __name__ == "__main__":
    main()
