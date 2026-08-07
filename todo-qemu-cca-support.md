# QEMU CCA Incubator and VMM Test Plan

## Goal

Add a typed QEMU CCA incubator backend that boots an RME-capable L1 Linux/KVM
host under QEMU TCG, runs aarch64 VMM-test binaries through pipette, and allows
a normal `#[vmm_test]` to launch an L2 CCA Realm with OpenVMM.

QEMU support is additive:

- retain FVP as the architectural reference and fallback;
- retain the existing FVP/TMK CCA test;
- do not change SNP behavior; and
- do not change the existing generic aarch64 TCG incubator job.

## Verified v15 stack

| Component | Revision | Status |
| --- | --- | --- |
| Linux KVM CCA | `cca-host/v15` at `d6d40e6a310388f58def167f8b57e31d6c6057a2` | FVP preflight and OpenVMM smoke pass |
| TF-RMM | `topics/rmm-v2.0-poc_3` at `f00eac344b6f7c18abc6dad1948b07e9a82ff9f0` | FVP preflight and smoke pass |
| TF-A | v2.15 at `da738d5eae93af342fdc4995dd3c05acb4c9d757` | FVP preflight and smoke pass |
| RMM specification | DEN0137 2.0-bet2 | Required by KVM CCA v15 |
| kvm-unit-tests | `cca/v4` at `6810c578fff399b33528335c7eae3c90e33847ff` | Optional validation |
| QEMU | `openvmm-deps` `0.3.0-110`, QEMU 11.0.1 | Restored binary accepts experimental `-cpu max,x-rme=on` |

The QEMU, firmware, and Linux pins form one platform bundle. Do not update them
independently after Phase 0 proves a compatible set.

TF-A and TF-RMM do not require persistent host source checkouts. Build them in
a pinned Docker builder that clones the exact revisions inside the container.
Persist only explicit inputs, Git/download caches, build outputs, and the
manifest on the host.

## Current implementation

### Incubator

- `petri/incubator/src/profile.rs` has only
  `IncubatorBackend::QemuTcg`.
- `petri/incubator/src/qemu.rs` always direct-boots a kernel/initrd with one
  serial stream, user networking, and a 9p share.
- `petri/incubator/src/run.rs` assumes the backend is `QemuTcg` and scans QEMU
  stdout for `pipette_client::PIPETTE_READY_MARKER`.
- `petri/incubator/src/main.rs` requires a kernel and initrd for every backend.
- `petri/incubator/profiles/aarch64-tcg-pcie.toml` is a generic nested-KVM/VFIO
  profile, not an RME platform.

### Flowey

- `flowey/flowey_lib_hvlite/src/write_incubator_target_runner.rs` already
  installs incubator as Cargo's target runner and passes kernel, initrd, QEMU,
  pipette, share, output, and guest-working-directory paths.
- The local/archived VMM-test flows currently resolve the same generic
  kernel/initrd/QEMU artifacts for every incubator profile.
- `flowey/flowey_lib_hvlite/src/resolve_openvmm_qemu.rs` models only the generic
  `SystemAarch64` QEMU binary.
- `flowey/flowey_lib_hvlite/src/resolve_openvmm_test_linux_kernel.rs` selects
  `KvmCcaDev` for aarch64 incubator tests, but that published kernel has not
  been verified against the v15 UAPI.

### Working CCA path to reuse

- `build_support/cca/build-kernels.sh` reproducibly builds distinct v15 host
  and Realm guest kernels.
- `flowey/flowey_lib_hvlite/src/build_cca_linux_kernels.rs` exposes them as a
  typed Flowey artifact.
- `flowey/flowey_lib_hvlite/src/_jobs/local_stage_kvm_cca.rs` stages the host
  rootfs, preflight, OpenVMM, guest kernel/initrd, and generated launch scripts.
- `run-cca-openvmm-repro.sh` is the current block/network smoke oracle.
- `vmm_tests/vmm_tests/tests/cca.rs` is an FVP/Shrinkwrap `SimpleTest`; it is
  not a normal `#[vmm_test]`.

### Petri gaps

- `petri::IsolationType`, Petri requirements, OpenVMM construction, and
  `vmm_test_macros` do not expose CCA.
- `openvmm_defs::config::IsolationType::Cca` already exists.
- `petri/petri_artifacts_common/src/lib.rs` does not define the `cca`
  capability.
- Isolation is currently tied to OpenHCL firmware configuration; CCA needs a
  VM-level isolation selection usable by Linux direct boot.

## Decision gates

Resolve each gate in the named phase. Do not silently pick a fallback.

### D1: QEMU revision and delivery

Options:

1. Build a pinned upstream QEMU revision from source.
2. Publish a static RME-capable QEMU in `openvmm-deps`.
3. Use a downstream RME fork.

Decision for Phase 0: use the pinned `openvmm-deps` `0.3.0-110` static QEMU
11.0.1. It accepts:

```text
-accel tcg
-M virt,secure=on,virtualization=on,gic-version=3,acpi=off
-cpu max,x-rme=on
```

Do not require a QEMU source checkout unless the manual v15 platform proof
finds a QEMU bug that is fixed only after 11.0.1. If that happens, pin and
build the minimum upstream revision containing the fix, then publish it as a
new `openvmm-deps` artifact.

Resolved for Phase 0; formalized in Phase 3 artifact selection.

### D1a: Phase 0 boot chain

Decision: use EDK2 `ArmVirtQemuKernel` as TF-A BL33. QEMU's aarch64 firmware
boot protocol accepts the host `-kernel`/`-append` arguments with this EDK2
platform, matching the upstream QEMU RME functional test.

Resolved in Phase 0.

### D2: Host rootfs and pipette readiness

Options:

1. Dedicated CCA host rootfs with an init service that mounts 9p, configures
   networking, runs `kvm_cca_preflight`, and starts pipette.
2. Reuse the generic incubator initrd injection.
3. Initially reuse the FVP buildroot rootfs with `debugfs` injection.

Recommendation: use option 3 for Phase 0, then produce option 1 in Phase 1.
The generic initrd is unlikely to contain all required firmware/KVM host
support.

Exit criterion: the primary host console emits the pipette ready marker only
after `kvm_cca_preflight` passes.

### D3: Console topology

Model an arbitrary list of serial consoles with one marked primary. For QEMU
`virt`, initially expect two PL011 streams: normal-world host and secure
TF-A/RMM. Exactly one host console is scanned for readiness. Preserve every
console as a separate log. Use `-display none`; do not combine `-nographic`
with explicit serial chardevs.

Resolved in Phase 2.

### D4: Petri isolation representation

Options:

1. Add `PetriVmBuilder::with_isolation(IsolationType)` as a VM-level property.
2. Add isolation to `Firmware::LinuxDirect` and create a distinct
   `linux_direct_aarch64_cca` macro configuration.

Recommendation: option 1. CCA is a KVM/VM property rather than firmware, and
the builder method is reusable without multiplying configuration names.

Resolved in Phase 4.

### D5: Agent transport inside the Realm

Linux direct boot already uses pipette as init, and `with_no_vmbus()` selects
virtio-vsock. The primary question is whether virtio-PCI vsock works inside a
Realm with the required shared-page/SWIOTLB behavior.

Recommendation: try the existing pipette-as-init plus virtio-vsock path; use
TCP pipette over virtio-net only if vsock fails.

Resolved by a focused probe before Phase 5 is completed.

## Phase 0: Prove the v15 stack under QEMU manually

This phase is blocking. Do not implement the incubator backend until it passes.

### Build

- [x] Restore `openvmm-deps` and use its QEMU 11.0.1
  `qemu-system-aarch64`.
- [x] Verify `qemu-system-aarch64 --version`.
- [x] Verify `-cpu max,x-rme=on` is accepted.
- [x] Add a Docker firmware builder that:
  - uses a pinned builder image by immutable digest;
  - clones TF-RMM and TF-A into container-local working directories;
  - checks out exact commits, never branch names;
  - reuses a mounted Git/download cache without treating it as source;
  - builds TF-RMM with `RMM_CONFIG=qemu_virt_defcfg`;
  - builds EDK2 `ArmVirtQemuKernel`;
  - builds TF-A with `PLAT=qemu`, `ENABLE_RME=1`, the RMM image, and EDK2 as
    BL33; and
  - packs TF-A BL1/FIP into `flash.bin`.
- [x] Publish `rmm.img`, `QEMU_EFI.fd`, `bl1.bin`, `fip.bin`, `flash.bin`,
  logs, and the manifest through a staged atomic directory swap.
- [x] Run the builder as the invoking host UID/GID so outputs are not
  root-owned.
- [x] Do not mount the Docker socket or grant `--privileged`.
- [x] Build the v15 host kernel from `build_support/cca/host.config`, adding a
  QEMU fragment only if the current built-in virtio/PCI/9p set is insufficient.
- [x] Stage the current FVP buildroot rootfs with `kvm_cca_preflight`, OpenVMM,
  guest kernel/initrd, and the generated OpenVMM CCA launch script.
- [x] Record the builder image digest, repository URLs, revisions,
  configure/build commands, toolchains, and output hashes in
  `target/cca-qemu/firmware/manifest.txt`.

### Representative boot

The proven Phase 0 CPU shape is:

```text
max,x-rme=on,lpa2=off,sme=off,pauth-impdef=on
```

`lpa2=off` is required: QEMU otherwise exposes a 52-bit Realm IPA and the L2
guest resets before reaching userspace. With LPA2 disabled, host and Realm IPA
are both 48 bits and the smoke passes. One CPU and 2 GiB are sufficient.

```text
qemu-system-aarch64 \
  -nodefaults \
  -accel tcg \
  -M virt,secure=on,virtualization=on,gic-version=3,acpi=off \
  -cpu max,x-rme=on,lpa2=off,sme=off,pauth-impdef=on \
  -m 2G -smp 1 \
  -bios flash.bin \
  -kernel host-Image \
  -drive if=none,id=rootfs,format=raw,file=host-rootfs.ext4 \
  -device virtio-blk-pci,drive=rootfs,romfile= \
  -virtfs local,path=<share>,mount_tag=FM,security_model=none \
  -append "nokaslr root=/dev/vda rw console=ttyAMA0" \
  -display none -serial stdio
```

Add named firmware/RMM/host consoles and user networking as required.
`nokaslr` is required by the current upstream QEMU RME reference stack.

### Acceptance

- [x] Host reaches userspace.
- [x] Host log reports RMM/KVM Realm support.
- [x] `kvm_cca_preflight` passes capability 251, guestmemfd, Realm creation,
  and VGICv3 checks.
- [x] The generated OpenVMM CCA launch script boots the L2 Realm.
- [x] The existing block/network smoke emits `OVMM_SMOKE_ALL_PASS`.
- [x] Firmware, RMM, host, and Realm output is captured in the host console
  logs, with the secondary serial retained separately.
- [x] Record boot-to-preflight and smoke wall-clock timings.

### Phase 0 result

Phase 0 passes:

- `run-qemu-cca-preflight.sh`: approximately 4 seconds;
- `run-qemu-cca-smoke.sh`: approximately 32 seconds;
- host and Realm IPA: 48 bits;
- all block/network smoke markers pass; and
- QEMU exits normally after L1 `poweroff`.

The complete platform record is
`target/cca-qemu/phase0-manifest.txt`.

If any acceptance item fails, stop and update the platform pin set before
proceeding.

## Phase 1: Package QEMU CCA artifacts

- [x] Continue resolving QEMU 11.0.1 from `openvmm-deps` unless Phase 0 proves
  a source-only fix is required.
- [x] A QEMU source build is not required; the pinned `openvmm-deps` QEMU
  11.0.1 binary provides the required RME support. If that changes, add
  `build_support/cca/build-qemu.sh`,
  modeled on `build-kernels.sh`: exact revision check, clean source export,
  incremental output, reproducible environment, and manifest hashes.
- [x] Add `build_support/cca/build-qemu-firmware.sh` as the host wrapper for
  the Docker build. It must:
  - accept output/cache/input paths and exact revision overrides;
  - reject output paths that overlap inputs;
  - pull/verify the pinned image digest;
  - mount inputs read-only;
  - mount cache and output directories read-write;
  - perform cleanup through container lifecycle, not broad host deletion; and
  - verify all declared artifacts and hashes after the container exits.
- [x] Add the Docker build recipe under `build_support/cca/` rather than
  requiring TF-A/TF-RMM source directories beside the OpenVMM checkout.
- [x] A QEMU-specific host kernel config fragment is not required; the common
  v15 host kernel passes on both FVP and QEMU.
- [x] Add a host-rootfs builder with a deterministic init service that mounts
  9p, configures networking, runs preflight, and executes the freshly-built
  pipette from the share. Do not bake a stale pipette into the image.
- [x] Add typed Flowey nodes:
  - `resolve_cca_qemu`;
  - `build_cca_qemu_firmware`; and
  - `build_cca_qemu_host_rootfs`.
- [x] Have the firmware Flowey node install/verify Docker, invoke the wrapper,
  and return the manifest and firmware artifacts. It must not expose or depend
  on container-local source paths.
- [x] Centralize QEMU/TF-A/TF-RMM/Linux pins in one Rust module and have the
  FVP/QEMU tooling consume it where practical.
- [ ] Keep local-build artifacts first; publish a downloadable platform bundle
  only after reproducibility is demonstrated.

### Exit criteria

- [x] The pinned Docker build succeeds from an empty cache and in
  offline/cache-only
  mode after the cache is populated.
- [ ] Future: make EDK2/FIP/flash byte-for-byte reproducible. TF-RMM and BL1
  are already stable, but EDK2 firmware-volume output still varies.
- [x] A build succeeds on a machine with no TF-A or TF-RMM checkout.
- [x] A cache-hit rebuild avoids refetching unchanged Git objects while producing
  identical outputs.
- [x] Typed Flowey outputs provide QEMU, `flash.bin`, host kernel, rootfs, and
  manifest paths.
- [x] The packaged artifacts repeat the Phase 0 preflight and smoke result.

### Phase 1 validation

`cargo xflowey qemu-cca-artifacts` stages the typed platform and generated
launchers under `target/cca-qemu-packaged`. The offline/cache-only packaging
run completed successfully.

- Packaged host-rootfs preflight: 4.377 seconds, `PIPETTE READY`, child exit 0.
- Nested OpenVMM smoke: 36.201 seconds, block and network markers passed,
  normal QEMU poweroff, child exit 0.
- Combined platform record:
  `target/cca-qemu-packaged/phase1-manifest.txt`.

## Phase 2: Add a typed `qemu-cca` incubator backend

### Profile

- [x] Add `IncubatorBackend::QemuCca(QemuCcaConfig)` in
  `petri/incubator/src/profile.rs`.
- [x] Model machine, CPU, memory, SMP, an ordered console list, primary
  console, capabilities, and a small typed extra-argument escape hatch.
  Firmware, kernel, rootfs, and QEMU paths remain backend-specific runtime
  artifacts so profiles do not contain machine-local paths.
- [x] Add `petri/incubator/profiles/aarch64-qemu-cca.toml` with no
  machine-local paths.

### Runtime

- [x] Split common QEMU process/readiness/path/network code from the current
  direct-boot command builder.
- [x] Build the CCA command with `-bios`, host kernel, rootfs disk, named
  consoles, 9p, and user networking.
- [x] Change `run_in_incubator` from an irrefutable `QemuTcg` binding to a
  backend match.
- [x] Move `prepare_initrd` inside the `QemuTcg` branch; QEMU CCA rootfs boot
  must not patch or require the generic initrd.
- [x] Monitor only the configured primary host console for pipette readiness.
- [x] Preserve all console logs on success and failure.
- [x] Kill the QEMU process group and all console relays on exit.
- [x] Add backend-conditional CLI/env fields:
  - `INCUBATOR_FIRMWARE`;
  - `INCUBATOR_ROOTFS`;
  - existing `INCUBATOR_KERNEL`; and
  - existing `INCUBATOR_QEMU_BINARY`.
- [x] Make kernel/initrd requirements backend-specific.
- [x] Extract capability publishing from `setup_vfio_devices`; it currently
  returns early when no VFIO devices exist, while QEMU CCA still needs to
  publish `PETRI_CAPABILITIES=cca`.
- [x] Advertise `cca` only after host preflight and pipette readiness.

### Tests

- Profile parsing for both backends.
- CCA command-line construction.
- Named-console and primary-console selection.
- Capability merging.
- Teardown and timeout behavior.

### Phase 2 runtime validation

- A command executed through the packaged QEMU CCA L1 pipette path and exited
  zero in 4.31 seconds.
- `PETRI_CAPABILITIES=cca` was visible to the guest command only after
  preflight-backed pipette readiness.
- The host and secure console logs were retained, the writable host rootfs was
  per-run, and no QEMU process remained after completion.
- The existing AArch64 `qemu-tcg` profile completed `/bin/true` unchanged,
  including all three VFIO device setup paths.

## Phase 3: Wire backend-specific artifacts through Flowey

- [x] Extend `write_incubator_target_runner::Request` and
  `IncubatorRunnerConfig` with firmware/rootfs inputs.
- [x] Add `INCUBATOR_FIRMWARE` and `INCUBATOR_ROOTFS` to the runner
  environment and unit tests.
- [x] Add an explicit `IncubatorPlatform::{QemuTcg,QemuCca}` parameter or
  profile-name classification available at Flowey emit time. Do not depend on
  reading profile contents; CI receives the profile path only as a runtime
  `ReadVar`.
- [x] Keep the existing generic TCG kernel/initrd/QEMU resolution unchanged
  for `QemuTcg`.
- [x] Resolve QEMU CCA firmware/rootfs/kernel/QEMU only for `QemuCca`.
- [x] Add a distinct QEMU resolver variant or platform-bundle resolver; do not
  silently replace `QemuFile::SystemAarch64`.
- [x] Support:

```text
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-qemu-cca.toml \
  --filter "test(qemu_cca) & !binary(cca)"
```

### Exit criteria

- A trivial incubator test binary runs through pipette in the QEMU CCA L1
  host.
- The generic aarch64 TCG incubator still passes unchanged.
- Run `test(aarch64_tcg)` before and after the shared Flowey changes.

### Phase 3 validation

- The planned `test(qemu_cca) & !binary(cca)` command passed through the typed
  QEMU CCA platform in 12.112 seconds.
- The unchanged generic `test(aarch64_tcg)` pass completed both existing
  VFIO/P2P tests in 83.167 seconds.
- Foreign AArch64 artifact discovery uses Cargo-compatible
  `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER` semantics. On an x86-64
  host without binfmt, install `qemu-user` or place `qemu-aarch64` on `PATH`.
- The CCA kernel source defaults to a sibling `linux-cca` checkout and can be
  overridden with `--cca-kernel-src` or `OPENVMM_CCA_KERNEL_SRC`.

## Phase 4: Add CCA to Petri

- [x] Add `capabilities::CCA = "cca"` and include it in
  `KNOWN_CAPABILITIES`.
- [ ] Add `petri::IsolationType::Cca`.
- [ ] Evaluate CCA solely through `requires(cca)` and `PETRI_CAPABILITIES`;
  do not add an always-false Linux `TestRequirement::Isolation(Cca)` path.
- [ ] Add a VM-level isolation field and
  `PetriVmBuilder::with_isolation(IsolationType)`.
- [ ] Map CCA to `openvmm_defs::config::IsolationType::Cca` in OpenVMM
  construction.
- [ ] Preserve existing OpenHCL firmware isolation behavior.
- [ ] Add macro/config parsing only if the final API requires a named
  CCA-specific config; `requires(cca)` should work from the known capability.
- [ ] Run a Windows build and existing SNP test gate because Windows Hyper-V
  Petri code also consumes `petri::IsolationType`.

### Exit criteria

- Petri construction tests cover CCA.
- Non-CCA configurations are unchanged.
- A CCA test skips outside a CCA-capable incubator.

## Phase 5: Add the first normal CCA VMM test

Add an aarch64-exclusive test modeled on the existing no-VMBus TCG test:

```rust
#[vmm_test_with(
    openvmm,
    requires(cca),
    configs(linux_direct_aarch64)
)]
async fn boot_linux_direct_cca(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    // with_isolation(Cca), no VMBus, supported Realm devices only
}
```

- [x] Use `with_isolation(Cca)`.
- [x] Disable Hyper-V/VTL2/VMBus.
- [x] Use device-tree boot, one VP, and explicit small L2 memory.
- [x] Configure the PCIe root complex/root-port topology required by
  virtio-vsock and supported CCA devices.
- [x] Set `hypervisor.with_hv = false`.
- [x] Use pipette-as-init with virtio-vsock first.
- [x] Select the CCA guest kernel built by `build-kernels.sh` if the generic
  Petri guest kernel lacks the required Realm/CCA configuration.
- [x] If a CCA-specific guest kernel is needed, add its complete artifact
  path: declaration, known-path/local resolver, local `BuildSelections`, and
  archived/CI typed artifact-builder entry.
- [x] Resolve D5 with a focused vsock probe; TCP is the fallback.
- [x] Make Petri's hard-coded 10-minute VM watchdog configurable if Phase 0
  timings show nested TCG CCA can exceed it.
- [x] Require agent ping, clean power-off, and clean VM teardown.
- [x] Preserve Realm and L1 host logs on failure.
- [x] Keep the existing FVP/TMK `cca_runtime` test.

### Exit criteria

- Name the test with a `_qemu_cca` suffix.
- `cargo xflowey vmm-tests-run ... --filter "test(qemu_cca) & !binary(cca)"`
  passes without selecting the existing FVP test binary.
- The same test skips when `cca` is not advertised.
- No SNP or ordinary Linux-direct test changes behavior.

### Phase 5 validation

- `boot_linux_direct_qemu_cca` launches a 256 MiB, one-VP Realm with
  device-tree Linux direct boot, no VMBus/Hyper-V interfaces, and a static PCIe
  root port carrying virtio-vsock.
- The packaged generic AArch64 test kernel is already the
  `openvmm-test-linux-kvm-cca-dev` kernel, so no new guest-kernel artifact was
  required.
- The Realm agent pinged successfully, powered off, and completed clean
  teardown in 71.563 seconds.
- Under the generic QEMU TCG incubator, the CCA test was skipped by
  `requires(cca)` while both existing `aarch64_tcg` tests passed.
- The existing FVP/TMK `cca_runtime` binary remains unchanged.

## Phase 6: CI and deduplication

- [ ] Add a dedicated initially non-blocking
  `aarch64-linux-qemu-cca` VMM-test job.
- [ ] Define how non-blocking is represented. Initially exclude the job from
  required PR gates or add an explicit soft-fail mechanism.
- [ ] Use Phase 0 timings to set a long but bounded timeout.
- [ ] Filter only `test(qemu_cca) & !binary(cca)` initially.
- [ ] Factor shared rootfs staging and log extraction from
  `local_stage_kvm_cca.rs` for FVP and QEMU.
- [ ] Consolidate duplicated CCA pin definitions.
- [ ] Publish the QEMU CCA platform bundle after reproducibility is proven.
- [ ] Keep FVP coverage for architecture behavior and features QEMU does not
  emulate, including RME in SMMU/GIC and permission overlay/indirection.

## Risks

- QEMU RME is experimental and its `x-rme` interface may change.
- No public bundle currently proves the v15/bet2 stack under QEMU.
- The Docker builder image is part of the trusted pin set and must be immutable
  by digest.
- Network access is required only to populate an empty cache; support an
  offline/cache-only mode after the first successful build.
- QEMU does not emulate RME in the SMMU or GIC; device assignment is out of
  scope.
- Outer TCG plus an L2 Realm will be slow.
- Firmware memory carveouts and host RAM size must be treated as platform
  data, not arbitrary test overrides.
- Agent transport inside the Realm is not yet proven.
- Petri's default 10-minute watchdog may be too short for nested TCG CCA.
- Shared Petri isolation changes require a Windows/SNP compile and regression
  gate.
- The first test must not select the existing FVP `cca_runtime` binary.
- `PETRI_CAPABILITIES` rejects unknown capabilities, so the `cca` constant
  must land before a profile advertises it.
- Retain FVP until QEMU demonstrates equivalent coverage for the required
  behavior.

## Definition of done

- A pinned QEMU CCA platform manifest is reproducible.
- A checked-in `aarch64-qemu-cca.toml` contains no machine-local paths.
- QEMU boots the v15 L1 host and passes `kvm_cca_preflight`.
- Incubator starts pipette and preserves named console logs.
- A normal `#[vmm_test]` launches OpenVMM CCA and boots an L2 Realm to a
  working agent.
- The test skips outside a CCA incubator.
- FVP CCA and SNP regression paths remain passing.

## Removed stale assumptions

- `run-openvmm-kvm-cca.sh` is generated by
  `local_stage_kvm_cca.rs`; it is not a checked-in source file.
- CCA rootfs staging uses `debugfs`, not `sudo mount`.
- `INCUBATOR_QEMU_BINARY` already exists.
- CI incubator profiles are selected by profile name.
- Phase 0 starts at one CPU and 2 GiB, but retains 8 GiB, `sme=off`, and
  `pauth-impdef=on` as measured compatibility fallbacks.
- The public QEMU RME bundle and older Linaro manifest are references only;
  neither is compatible with KVM CCA v15 without repinning.

## Review

Plan review verdict: **Minor revisions**. The review confirmed the phase
structure and codebase survey. It required, and this plan now includes:

- a coherent direct-Linux BL33 boot chain;
- `security_model=none`;
- measured memory/CPU fallback knobs;
- a generic two-console initial QEMU model;
- Flowey emit-time platform classification;
- capability publishing independent of VFIO;
- explicit Petri topology, memory, VP count, DT boot, and vsock transport;
- complete ownership of a CCA-specific guest-kernel artifact if required;
- a configurable Petri watchdog;
- a `_qemu_cca` test/filter that excludes the FVP binary;
- a defined non-blocking CI mechanism; and
- Windows/SNP plus existing aarch64-TCG regression gates.
