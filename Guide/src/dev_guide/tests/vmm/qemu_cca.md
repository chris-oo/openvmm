# CCA and FVP nested tests

Petri nested tests run OpenVMM inside an emulated Linux host and use prebuilt
platform dependencies rather than compiling firmware on the test machine.

## Test topology

The outer test runs on the development host. Its Petri backend boots an L1
Linux machine and connects pipette. Petri then runs the selected inner test
from an AArch64 nextest archive inside L1. For CCA tests, the inner test uses
KVM to create a Realm guest.

Select the outer test for the development host. Flowey discovers the nested
archive and its artifacts, then builds OpenVMM, pipette, and the test binaries
for their required targets. Do not cross-compile the outer test to AArch64
and use a cargo target runner to boot FVP.

The old `vmm-tests-run --incubator` and `--fvp-single-test` options are no
longer part of the nested-test path. The standalone `incubator` tool remains
separate from native Petri test registration.

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

To test a locally built firmware package:

```bash
export PETRI_FVP_PLATFORM_ROOT=path/to/fvp-runtime
FVP_PAYLOAD_ARCHIVE=path/to/openvmm-test-fvp-cca-payload.aarch64.<RELEASE>.tar.gz
NEXTEST_TEST_THREADS=1 PYTHONDONTWRITEBYTECODE=1 \
  cargo xflowey vmm-tests-run \
  --filter 'test(cca_tdisp_ahci_on_fvp)' \
  --fvp-deps-version <RELEASE> \
  --fvp-package-archive path/to/openvmm-test-fvp-cca.aarch64.<RELEASE>.tar.gz \
  --fvp-package-sha256 <ARCHIVE_SHA256> \
  --fvp-payload-archive "$FVP_PAYLOAD_ARCHIVE" \
  --fvp-payload-sha256 <PAYLOAD_ARCHIVE_SHA256> \
  --dir path/to/test-output
```

The separate payload archive supplies `host/Image`, `host/initrd`,
`guest/Image`, and `guest/initrd`, with its own manifest and checksums.
The current payload recipe imports pinned qualification binaries. It marks
them as local-only reference inputs, not rebuilt or publishable outputs.
Use an exact outer-test filter when checking one case. Omit the local
archive options only when both packages are available in the named
`microsoft/openvmm-deps` release. Until packages are published and qualified,
provide their release identity and both SHA-256 values explicitly.

Flowey verifies the archive before extracting and staging its contents.
Missing inputs and identity mismatches are errors. There is no fallback
that builds local firmware.

For a non-TDISP check, use the same package options with
`--filter 'test(cca_in_place_smoke_on_fvp)'`. This case uses the packaged
`host/Image`, qualified as a unified host and Realm kernel, with Petri's
standard pipette initrd. It boots an in-place CCA Realm, pings pipette over
Virtio-vsock, powers off, and requires clean OpenVMM teardown. It does not
assign a device to the Realm. The current outer FVP profile still prepares
and tears down its AHCI host fixture.

Without `--fvp-deps-version`, artifact discovery excludes FVP
package-dependent cases, including in `--build-only` mode. Payload-only
cases can use the release and payload options without firmware options.

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

Workspace verification allows up to 2 GiB and 4096 tree entries, including
the nested test runner. It uses the file-validation budget, not the shorter
process-cleanup budget. The original launcher evidence remains limited to
1 GiB. FVP's watchdog and cancellation wait include these separate phases;
other backends keep their existing watchdog behavior.

Shrinkwrap resolves the model arguments, runs the packaged pre-launch
commands, starts the model container, and captures its serial consoles.
Petri owns the pipette connection, nested-test execution, and VM teardown.
The FVP supervisor also checks fixture cleanup and model shutdown; a passing
inner test does not hide a failed outer lifecycle.

The qualified FVP model exports one `FM` 9P share. The backend must make the
nested test-content and image directories available at Petri's expected
guest paths rather than assuming QEMU's multiple-9P-device implementation.
EDK2 loads the Linux initrd through SemihostFs and `startup.nsh`.

## Qualification limits

The in-place Virtio block and network tests have passed on the pinned FVP
tuple. These are in-process Virtio tests, not assigned-device private-DMA
qualification.

The native packaged non-TDISP smoke run passed its inner Realm test: boot,
pipette ping, poweroff, and clean OpenVMM teardown. The nested process exited
0, and fixture cleanup and supervisor finalization succeeded. The outer test
still failed: FVP 11.31.28 reported `corrupted size vs. prev_size` and aborted
with status 134 after L1 poweroff; Shrinkwrap returned 1. The same model abort
also occurred in a run that failed before Realm creation. This is a separate
model-shutdown blocker, not a clean end-to-end lifecycle pass.

A controlled comparison with FVP 11.32.19 passed the same native smoke test
end to end. It kept the same firmware, Linux payloads, Shrinkwrap checkout,
and Debian 13/glibc 2.41 container userspace. The newer model was installed
on top of the baseline image, and the launcher used `docker-local`.
The inner test, fixture teardown, and Shrinkwrap each exited 0; supervisor
finalization succeeded. No allocator abort occurred in this run. This is
one non-TDISP smoke result, not assigned-device qualification or a fix for
the separate TDISP teardown failure. The local comparison image is not a
published dependency. The built-in runtime now selects the published
Shrinkwrap 2026.9.1 image containing the same FVP model version.

The native non-TDISP smoke test also passed end to end with this published
image: `cargo xflowey vmm-tests-run` reported one passing test and exited 0.
The inner test, fixture teardown, and Shrinkwrap each exited 0; supervisor
finalization succeeded, the native journal retired, and the model container
was removed. No allocator abort occurred. This validates the selected smoke
case and its full lifecycle, not the other CCA profiles or TDISP teardown.

Before migration to native Petri nesting, the 2026-10-12 reference TDISP
run passed exactly one guest test. It completed LOCK/RUN, matched a 64 MiB
AHCI read, completed checked UNLOCK and explicit OpenVMM assignment teardown,
and completed fixture cleanup. Kernel/RMM Realm destruction still returned
`RMI_ERROR_REALM`, and the outer FVP launcher failed during L1 shutdown.
That run was not a clean end-to-end lifecycle pass.

A native packaged run also completed the guest LOCK/RUN, AHCI hash check,
and UNLOCK sequence. Its inner test failed during clean OpenVMM teardown:
the assignment owner reported completion, but RAM unmap still hit the
frozen-assignment guard. RMM Realm destruction returned `RMI_ERROR_REALM`.
Fixture cleanup and supervisor finalization succeeded; L1 shutdown still
returned a failing launcher status. This is not a passing nested test.

Keep guest execution, checked OpenVMM teardown, fixture cleanup, and model
shutdown as separate outcomes. Neither an AHCI read nor a guest UNLOCK
marker proves host protected-map reclamation or private-DMA isolation.
Do not qualify a tuple that skips cleanup or treats model failure as success.

## Firmware development and recovery

The `openvmm-deps` package recipe is the normal place to build and publish
FVP firmware. Source revisions, configuration, toolchain, and output hashes
must remain explicit. Do not update runtime identities merely to accept an
unverified rebuild.

For recovery of the older local qualification inputs, the tracked
`petri/incubator/platforms/rebuild-realm-firmware.py` recipe and adjacent
JSON pins rebuild TF-A, RMM, and EDK2 in the pinned container. They require
the preserved package configuration and exact approved DTB. The script
preserves sources, commands, manifests, and output hashes outside `target/`.
Rebuilt firmware need not be byte-identical to historical output.

`petri/incubator/platforms/restore-realm-test-inputs.py` recovers the unchanged
host and guest payloads and DA assets from a preserved single-boot run.
It checks input hashes and refuses existing destination payloads. Those
recovery scripts do not acquire or redistribute the licensed model.
