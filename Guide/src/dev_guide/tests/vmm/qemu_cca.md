# CCA and FVP nested tests

Petri nested tests run OpenVMM inside an emulated Linux host and use prebuilt
platform dependencies rather than compiling firmware on the test machine.

## Prebuilt FVP firmware

The `openvmm-deps` FVP firmware archive is named:

```text
openvmm-test-fvp-cca.aarch64.<RELEASE>.tar.gz
```

It supplies BL1, FIP, DTB, the resolved Shrinkwrap package configuration, and
the runtime overlay. FIP contains the FVP RMM and EDK2 firmware. The package
also records source and build identities, file hashes, and license notices.
QEMU firmware is not interchangeable with this FVP firmware.

The firmware package is separate from Linux payloads and the licensed model.
Use the kernel and initrd qualified for the selected CCA configuration. In
particular, the guest_memfd in-place tuple is not the older CCA v15 kernel
tuple. Device-assignment tests also need their approved fixture and guest
payloads.

Firmware and payloads can use different releases. Set
`--fvp-payload-version <PAYLOAD_RELEASE>` when the payload is not in the
firmware release. Both archive digests remain required.

Flowey verifies the archive before extracting and staging its contents.
Missing inputs and identity mismatches are errors. There is no fallback
that builds local firmware.

## Runtime prerequisites

```admonish warning
FVP remains opt-in. Acquire the model under the applicable Arm license.
The firmware archive does not distribute the model, a model container,
or a copied Python virtualenv. Hosted CI does not provide the model.
```

Use Linux or WSL2 with a local Docker daemon accessible through a Unix
socket. The checked-in runtime uses FVP 11.32.19 inside the published,
digest-pinned `shrinkwraptool/base-slim:2026.9.1` container. No local container
build or model compilation is required. Cache the approved image:

```bash
FVP_IMAGE=shrinkwraptool/base-slim@sha256:7379a13f850d93a540b6aa34685c9da7828c9df4bad5d4fbcf7e3436a8c266cc
docker pull "$FVP_IMAGE"
docker tag "$FVP_IMAGE" shrinkwraptool/base-slim:2026.9.1
```

Shrinkwrap and its Python dependencies must match the selected
toolchain identity. The checked-in platform manifests record the supported
model, tool, container, and source identities.

Set `PETRI_FVP_PLATFORM_ROOT` to the approved toolchain root. Its
`cca-test/shrinkwrap/venv/bin/shrinkwrap` must match that identity. Firmware,
Linux payloads, and device-assignment fixtures come from the resolved
packages, not from this toolchain root.

Do not modify the Shrinkwrap checkout or virtualenv during a run. A copied
virtualenv is not a portable tool distribution: its interpreter and editable
installation contain absolute paths.

## What runs locally

The development host builds OpenVMM, pipette, and the VMM test binaries from
the current source. It downloads or consumes prebuilt firmware and Linux
payloads. Per-run configuration and initrd preparation are packaging steps,
not firmware or kernel compilation.

The FVP launcher calls `shrinkwrap run`, not `shrinkwrap build`. It supplies
explicit paths for BL1, FIP, DTB, kernel, initrd, and the guest share. It uses
an owned writable workspace rather than Shrinkwrap's default package
directory.

The launcher uses Shrinkwrap's `docker-local` runtime. The supervisor checks
the cached image's exact digest before launch; it does not pull an image
during the run. Provision the pinned model image before starting the test.

Native tests keep recovery journals and launcher logs under
`~/.local/state/openvmm/petri-native/fvp-v1`, outside disposable test output.
They use separate recovery state from the standalone `incubator` CLI, but
share an exclusive model lock. Unknown ownership or incomplete cleanup keeps
the recovery state and makes the test fail. A new output directory or rebuilt
test binary does not bypass retained native state.

For explicit native recovery, first confirm that the recorded owner has exited.
Preserve its executable and independently record its SHA-256 before nextest
removes the extracted test binaries. Recovery requires that exact executable
at its recorded canonical path. An exact-byte copy can restore a removed
executable; a rebuilt binary cannot replace it.

Provide a JSON evidence file with `run_id`, the full recorded `owner` object
from the native journal, and `owner_sha256`. Then run:

```bash
PYTHONDONTWRITEBYTECODE=1 cargo run -p incubator -- recover-fvp-native \
  --platform-root path/to/fvp-runtime \
  --package-root path/to/test-output/aarch64/fvp-cca \
  --evidence path/to/recovery-evidence.json
```

This command authorizes only the named native run. It keeps the process,
container, workspace, and post-run verification checks. Do not delete the
journal to bypass recovery, and do not change its recorded owner identity.
