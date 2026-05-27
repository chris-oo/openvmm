# Plan: KVM Arm CCA enlightened Linux guest and FVP validation

## Goal

Add OpenVMM support for launching an enlightened Linux guest as an Arm CCA Realm
on KVM, based on the current KVM SNP branch and the Flowey/FVP infrastructure
from microsoft/openvmm#3455. The target first milestone is a small, direct-boot
arm64 Linux `Image` guest with OpenVMM enlightenments where possible, running on
an Arm CCA-capable KVM stack under FVP.

This is not the same as the PR 3455 OpenHCL/TMK CCA path. That PR adds CCA as an
OpenHCL/mshv_vtl hardware-isolated backend and a Flowey pipeline for CCA FVP
test assets. This plan reuses the Flowey/FVP environment but adds a native
OpenVMM + KVM + Arm CCA launch path.

## Current state and evidence

- OpenVMM already has KVM SNP scaffolding that is useful for CCA: private
  `guest_memfd` slots, `KVM_SET_MEMORY_ATTRIBUTES`, initial page acceptance, SNP
  launch sequencing, and runtime private/shared conversion handling via
  `KVM_HC_MAP_GPA_RANGE`.
- The existing generic isolation model only names `Vbs`, `Snp`, and `Tdx`, and
  `PageVisibility` only models `Exclusive` and `Shared`. CCA needs a first-class
  isolation type and richer launch-page semantics.
- The current KVM aarch64 backend always creates a normal VM and uses userspace
  memory backing. It probes GIC support, creates vCPUs, configures GIC/timers,
  and builds a normal partition, but has no CCA Realm VM type, private memory,
  Realm population, RIPAS handling, or Realm-specific exit policy.
- The direct Linux loader already has an arm64 path that loads an `Image`,
  optional initrd, command line, and generated DTB, but it writes plaintext into
  `GuestMemory`. CCA needs backend-mediated population of Realm-private memory.
- `CCA_SNP_OVERLAP_FINDINGS.md` concluded that 40-60% of the CCA foundation
  overlaps with SNP, mostly in confidential VM capability plumbing,
  `guest_memfd`, private/shared page-state tracking, initial private page
  population, runtime conversions, and device/lifecycle restrictions. The final
  VM type, RMI population, PSCI, VGIC/timer, MMIO, and RIPAS details remain
  Arm-specific.
- The current userspace-facing LKML findings are in
  `~/lkml/cca-kvm-v14-summary.md`. That summary covers the v14 series,
  `[PATCH v14 00/44] arm64: Support for Arm CCA in KVM`, based on Linux
  `v7.1-rc1` and targeting RMM `v2.0-bet1`. Userspace creates a Realm VM with
  the arm64 Realm VM type, uses `guest_memfd` plus memory attributes for private
  memory, populates initial measured content with `KVM_ARM_RMI_POPULATE`, sets
  allowed initial registers, and enters `KVM_RUN`.
- The reference Linux tree for implementation and FVP validation is
  `~/ai/eevee/NV-Kernels`, which has the v14 KVM CCA series applied.
- PR 3455 adds Flowey CCA test infrastructure that installs/checks CCA emulation
  prerequisites, builds or updates TF-A, TF-RMM, Plane0 Linux, kvmtool, and
  rootfs assets, injects test payloads, and launches the CCA three-world
  shrinkwrap/FVP configuration via `cargo xflowey cca-tests`.

## Architecture approach

Keep the SNP work generic where the model really overlaps, then add an
Arm-specific CCA launch driver under the KVM aarch64 backend.

The reusable layer should own:

- confidential isolation selection and capability probing;
- guest-private memory backing and memory-attribute transitions;
- initial private/shared/measured/unmeasured page metadata;
- backend-mediated initial page population;
- runtime page-state conversion dispatch;
- confidential-VM device and DMA restrictions.

The CCA-specific layer should own:

- KVM Arm RMI capability probing;
- Realm VM creation with the arm64 Realm VM type;
- guest_memfd-backed private memory and measured/unmeasured population;
- launch ordering before first vCPU run;
- CCA MMIO, VGIC, timer, SVE, debug, and register-access rules;
- RIPAS change exits and their synchronization with OpenVMM memory state.

## Updated LKML v14 findings

The current userspace ABI summary is in `~/lkml/cca-kvm-v14-summary.md`. It is
based on the v14 LKML series, `[PATCH v14 00/44] arm64: Support for Arm CCA in
KVM`, dated 2026-05-13. The important updates for this plan are:

- Detect CCA with `KVM_CHECK_EXTENSION(KVM_CAP_ARM_RMI)`.
- Create the VM with
  `KVM_CREATE_VM(KVM_VM_TYPE_ARM_REALM | KVM_VM_TYPE_ARM_IPA_SIZE(bits))`.
- Back Realm RAM with `guest_memfd` memslots and mark protected regions private
  with `KVM_SET_MEMORY_ATTRIBUTES(KVM_MEMORY_ATTRIBUTE_PRIVATE)`.
- Populate initial Realm contents with the VM ioctl `KVM_ARM_RMI_POPULATE`.
  Userspace passes a normal userspace source pointer, KVM copies it into
  protected guest memory, and the ioctl may make partial progress; OpenVMM must
  loop with the kernel-updated `base`, `size`, and `source_uaddr` until
  `size == 0`.
- Use `KVM_ARM_RMI_POPULATE_FLAGS_MEASURE` for ranges that contribute to the
  Realm Initial Measurement.
- Complete all population and allowed initial register setup before any vCPU is
  run. KVM creates the Realm descriptor as needed, manages REC lifecycle, and
  activates the Realm on the first vCPU run.
- Rely on KVM's in-kernel RMI PSCI completion before re-entering the REC.
- Restrict initial register setup for Realms to the fields KVM/RMM allow:
  general-purpose registers `x0`-`x30`, `pc`, and selected writable ID/SVE
  configuration fields.
- Handle normal KVM exits plus Realm-specific memory behavior: memory faults,
  `RMI_EXIT_RIPAS_CHANGE`, MMIO, host calls, shutdown/internal errors, and
  restricted abort injection. There is currently no userspace ioctl to reject a
  RIPAS change and report rejection to the guest.
- Treat device assignment as unsupported. The v14 series explicitly prevents
  device mappings for Realms.

Implication: the first virt_kvm phase should target the v14 userspace ABI:
Realm VM type creation, `guest_memfd` private memory, `KVM_ARM_RMI_POPULATE` for
measured initial contents, allowed register setup, first `KVM_RUN` activation,
and runtime handling for RIPAS/MMIO/host-call/memory-fault behavior.

## Implementation plan

### 1. Add CCA to generic isolation/config plumbing

Add a CCA isolation mode alongside SNP/TDX in the generic virt and loader-facing
types. Do not overload SNP: CCA has different launch parameters, measurement
inputs, page population ioctls, register restrictions, and runtime exits.

Concrete work:

- Add `IsolationType::Cca` in the generic virt layer and matching config/CLI
  parsing in OpenVMM.
- Add a KVM-specific CCA configuration object for the first milestone:
  measured-population policy, debug enablement, SVE vector length policy, and
  whether the launch is FVP/test-only.
- Gate CCA to `guest_arch = "aarch64"` and KVM only.
- In the aarch64 KVM backend, continue rejecting `Vbs`, `Snp`, and `Tdx`, but
  accept `IsolationType::Cca` when the host exposes the required KVM CCA
  capabilities.
- Reject VTL2/OpenHCL assumptions, device assignment, migration, save/restore,
  hotplug, and unsupported devices for the first native KVM CCA milestone.
- Add clear capability errors when `/dev/kvm` lacks Arm RMI/CCA support.

### 2. Add v14 KVM UAPI bindings and wrappers

Add the exact v14 userspace ABI surface that virt_kvm needs before wiring the
backend logic. Keep these wrappers architecture-gated to arm64 where possible.

Concrete work:

- Add or update Rust bindings for `KVM_CAP_ARM_RMI`.
- Add arm64 VM type constants/helpers for `KVM_VM_TYPE_ARM_REALM` and
  `KVM_VM_TYPE_ARM_IPA_SIZE(bits)`.
- Add bindings for `KVM_ARM_RMI_POPULATE`,
  `KVM_ARM_RMI_POPULATE_FLAGS_MEASURE`, and `struct kvm_arm_rmi_populate`.
- Add a safe `kvm` crate wrapper for creating an arm64 VM with an explicit raw
  VM type, so virt_kvm can request
  `KVM_VM_TYPE_ARM_REALM | KVM_VM_TYPE_ARM_IPA_SIZE(bits)`.
- Add a `kvm` crate wrapper for `KVM_ARM_RMI_POPULATE` that preserves the
  kernel-updated `base`, `size`, and `source_uaddr` fields for partial-progress
  looping.
- Keep existing SNP/x86 KVM bindings and wrappers unchanged.

### 3. Generalize SNP private-memory work for CCA

The KVM SNP branch already has much of the memory shape CCA needs, but some of
it is x86/SNP-specific. Refactor just enough to share it.

Concrete work:

- Move `guest_memfd` slot setup, private memory attribute setup, and stale
  private/shared backing discard helpers out of x86-only SNP paths where
  possible.
- Let the aarch64 KVM partition choose `KvmMemoryBackingMode::GuestMemfd` when
  CCA isolation is requested.
- Keep per-slot private attribute tracking generic.
- Add coverage validation for initial private page ranges.
- Replace SNP-specific names in reusable helpers with confidential/private-memory
  names, while keeping SNP-specific launch update code isolated.
- Preserve the existing SNP behavior while making CCA call the shared slot and
  memory-attribute machinery.

### 4. Map boot page acceptance to CCA population policy

Use the existing loader `BootPageAcceptance` metadata as the first CCA
population contract. CCA does not need a new generic launch-page model for the
first direct Linux milestone: the loader already identifies imported content
ranges and whether they should be measured, unmeasured, or shared.

For CCA v14, treat loader-described imported ranges as the only ranges that need
`KVM_ARM_RMI_POPULATE` before first run. Realm RAM as a whole is backed by
`guest_memfd` and marked private through memory attributes; non-imported RAM is
available through the Realm/private memory model without being populated from
host bytes. For the first milestone, mark all imported direct-boot private
content as measured: kernel, initrd, DTB, and deterministic boot metadata should
use `BootPageAcceptance::Exclusive`.

Concrete work:

- Map `BootPageAcceptance::Exclusive` to private Realm population with
  `KVM_ARM_RMI_POPULATE_FLAGS_MEASURE`.
- Map `BootPageAcceptance::ExclusiveUnmeasured` to private Realm population
  without `KVM_ARM_RMI_POPULATE_FLAGS_MEASURE`.
- Map `BootPageAcceptance::Shared` to shared memory that is not populated as
  protected Realm content.
- Reject SNP-specific page types for CCA direct Linux boot until an Arm-specific
  meaning is defined: `VpContext`, `SecretsPage`, `CpuidPage`,
  `CpuidExtendedStatePage`, and `ErrorPage`.
- Keep using `InitialAcceptedPage` as the loader-to-hypervisor carrier for the
  first phase instead of introducing a parallel initial-memory descriptor.

### 5. Propagate arm64 loader accepted ranges

The SNP path already passes loader-imported ranges to the hypervisor as
`InitialAcceptedPage`. The arm64 direct Linux loader currently needs equivalent
plumbing so CCA can consume the same accepted-page flow.

Concrete work:

- Change the arm64 Linux load path to preserve the `InitialLoadInfo` imported
  ranges instead of returning only initial registers.
- Return both `Aarch64Register` initial state and `InitialAcceptedPage` ranges
  from the OpenVMM arm64 direct Linux loader path.
- Route those accepted ranges through the existing worker
  `set_initial_page_visibility()` call for CCA isolated partitions.
- Reject ACPI/EFI arm64 boot for the first CCA phase unless the EFI/ACPI table
  writes are also represented as accepted imported ranges.

### 6. Add KVM aarch64 Realm VM creation

Add an Arm CCA path to `virt_kvm/src/arch/aarch64`.

Concrete work:

- Probe `KVM_CAP_ARM_RMI` before enabling CCA.
- Check `KVM_CAP_GUEST_MEMFD` and
  `KVM_CAP_MEMORY_ATTRIBUTES(KVM_MEMORY_ATTRIBUTE_PRIVATE)` for the Realm VM.
- Create the VM with
  `KVM_VM_TYPE_ARM_REALM | KVM_VM_TYPE_ARM_IPA_SIZE(bits)` rather than creating
  a normal Arm VM when CCA isolation is selected.
- Configure only the Realm parameters exposed by the v14 ABI: IPA size, allowed
  debug configuration, and optional SVE vector length reduction.
- Let KVM create the Realm descriptor, manage REC lifecycle, and activate the
  Realm as part of the v14 lifecycle.
- Require GICv3 for Realm VMs and reject the existing GICv2 fallback path; Realm
  guests do not support GICv2.
- Use the normal aarch64 VGICv3 setup path; no separate CCA timer setup is
  required for the first phase.
- Add CCA-specific errors instead of reusing SNP errors such as
  `SnpPrivateMemoryNotImplemented`.

### 7. Implement CCA in the existing initial page acceptance flow

OpenVMM already has a loader-to-hypervisor path for isolated initial pages:
loader output is converted to `InitialAcceptedPage`, the worker calls
`set_initial_page_visibility()` for isolated partitions, and virt_kvm handles it
through `AcceptInitialPages`. CCA should plug into that flow rather than adding a
new loader path.

Concrete work:

- Back Realm RAM with `guest_memfd` memslots and mark protected regions private
  with `KVM_SET_MEMORY_ATTRIBUTES(KVM_MEMORY_ATTRIBUTE_PRIVATE)`.
- Add an aarch64/CCA `AcceptInitialPages` implementation in virt_kvm.
- For each loader-provided private `InitialAcceptedPage`, call
  `KVM_ARM_RMI_POPULATE` using the page acceptance mapping from section 4.
- Leave shared `InitialAcceptedPage` ranges out of protected Realm population.
- Do not invent extra population entries for non-imported RAM; the first phase
  should consume only the loader-provided accepted-page list.
- Implement `KVM_ARM_RMI_POPULATE` as a loop because KVM may update `base`,
  `size`, and `source_uaddr` after partial progress; continue until
  `size == 0`.
- Fail launch if any private accepted-page range is missing, overlaps MMIO, is
  misaligned, uses unsupported acceptance metadata, or is not backed by
  guest_memfd private memory.

### 8. Set allowed vCPU state before first run

CCA vCPUs do not expose unrestricted normal KVM register access. In the v14 ABI,
KVM manages REC creation and Realm activation implicitly, so OpenVMM's job is to
set the allowed initial state before the first `KVM_RUN`.

Concrete work:

- Translate OpenVMM's `Aarch64InitialRegs`/loader state into the Realm initial
  state accepted by KVM.
- Set only the v14-allowed initial registers: `x0`-`x30`, `pc`, and selected
  writable ID/SVE configuration fields.
- Add a Realm-specific initial register path that skips normal aarch64 writes to
  `SP`, `SP_EL1`, `PSTATE`, and EL1 system registers such as `SCTLR_EL1`,
  `TTBR*_EL1`, `TCR_EL1`, `MAIR_EL1`, `ELR_EL1`, and `VBAR_EL1`.
- Rely on KVM/RMM reset state for `PSTATE` and EL1 system registers. In
  `~/ai/eevee/NV-Kernels`, KVM's normal reset state matches the Linux direct
  boot requirements for the relevant fields: EL1h with DAIF masked and MMU off.
- Ensure the first `KVM_RUN` happens only after private memory setup, population,
  and allowed register setup are complete.
- Add state-machine checks so launch ioctls cannot be called after activation or
  repeated after a partial failure.

### 9. Handle CCA runtime exits

The minimal direct Linux boot path must handle the exits a Realm Linux guest will
produce under FVP.

Concrete work:

- Use the Realm-compatible PSCI conduit in the generated DTB and rely on KVM's
  in-kernel RMI PSCI completion.
- Handle Realm MMIO exits for the devices kept in the initial profile, starting
  with serial console and the minimum virtio profile required by the enlightened
  Linux guest.
- Handle VGIC and virtual timer behavior for Realm execution.
- Implement `RMI_EXIT_RIPAS_CHANGE` handling by mapping it to the shared
  confidential-memory conversion infrastructure: update KVM memory attributes,
  discard stale backing on the side becoming invalid, and resume the Realm only
  after conversion is complete.
- Treat unsupported or rejected RIPAS changes as a fatal vCPU/VM condition for
  now; v14 does not expose a userspace ioctl to reject a RIPAS change and report
  that rejection back to the guest.
- Rate-limit guest-triggerable trace events.
- Return explicit errors for unsupported Realm exits rather than panicking.

### 10. Define the first supported device/enlightenment profile

Start with a deliberately small profile.

Concrete work:

- Support direct arm64 Linux boot with generated DTB.
- Enable the serial console path needed for FVP validation.
- Use KVM's in-kernel VGICv3 device path and generated DTB GICv3 description.
- Use the Realm-compatible PSCI conduit selected for the v14 guest kernel.
- Suppress PMU nodes for CCA first phase because PMU support is not in the v14
  userspace ABI target.
- Reuse the existing SNP-safe virtio/PCIe device-model support where the device
  only accesses guest-shared memory.
- For CCA, validate the guest shared-memory path separately: the Realm guest must
  issue the CCA/RSI memory-state changes that surface to userspace as
  `RMI_EXIT_RIPAS_CHANGE`, and virt_kvm must convert the affected ranges with
  KVM memory attributes before device DMA/MMIO uses them.
- For the first smoke test, support serial plus the existing SNP-safe virtio/PCIe
  profile after validating shared-buffer operation through `RMI_EXIT_RIPAS_CHANGE`.
- Do not expose VMBus for the first native KVM CCA milestone.
- Reject VFIO/device assignment and unsafe DMA paths.
- Reject devices that require host direct access to private guest memory.
- Document unsupported features in the plan/PR: migration, save/restore,
  arbitrary PCI, production attestation, full interrupt/GIC hardening, and broad
  device support.

### 11. Reuse and extend PR 3455 Flowey/FVP infrastructure

Use PR 3455's FVP work as the environment provider, not as the CCA KVM launch
implementation.

Concrete work:

- Bring in or depend on the Flowey `cca-tests` pipeline that installs/checks CCA
  emulation prerequisites.
- Reuse its asset preparation for TF-A, TF-RMM, Plane0 Linux, kvmtool, rootfs,
  shrinkwrap, and FVP.
- Add an OpenVMM-native payload mode next to the TMK/OpenHCL payload mode:
  build/copy the OpenVMM binary and test Linux kernel/initrd into the FVP rootfs
  or overlay.
- Add a script/job step that starts the CCA three-world FVP/shrinkwrap
  environment, then runs OpenVMM in Plane0 Linux using KVM to launch the CCA
  Realm guest.
- Add Flowey knobs for:
  - install/update emulation environment;
  - rebuild Plane0 Linux/rootfs;
  - build OpenVMM for aarch64;
  - choose TMK/OpenHCL versus native OpenVMM KVM CCA test mode;
  - collect FVP, Plane0, OpenVMM, and guest serial logs.
- Keep `cargo xflowey cca-tests` behavior from PR 3455 intact, and add a
  separate command or option for the native KVM CCA guest test.
- The exact command shape is not part of the first virt_kvm implementation
  phase; decide whether it lives under `cca-tests` or a separate
  `kvm-cca-tests` command when doing Flowey integration.

#### Native KVM CCA debug/test modes

The PR 3455 infrastructure is now present in-tree. The existing
`cargo xflowey cca-tests` pipeline builds and injects the TMK/OpenHCL payload
(`tmk_vmm`, `simple_tmk`, `guest-disk.img`, `KVMTOOL_EFI.fd`, `lkvm`, and the
Plane0 Linux `Image`) into the shrinkwrap `cca-3world` rootfs, then runs
`shrinkwrap run cca-3world.yaml --rtvar ROOTFS=<rootfs.ext2>`. Keep that
behavior intact.

Add a separate native KVM CCA pipeline, tentatively
`cargo xflowey kvm-cca-tests`, so the native OpenVMM/KVM Realm path can evolve
without overloading the TMK/OpenHCL `cca-tests` command. The new pipeline should
reuse the same emulator prerequisite checks, shrinkwrap install/update jobs,
rootfs handling helpers, shrinkwrap/FVP launch, and logging conventions where
possible, but stage and run OpenVMM-native payloads. Reuse the emulator
installation infrastructure; do not duplicate the FVP/shrinkwrap install logic.
However, the native KVM CCA path must accept explicit host and guest kernel
inputs, because it needs a Plane0 host kernel with the v14 KVM CCA ABI being
tested and a Realm guest kernel with the matching CCA/RSI enlightenments.

For the first MVP, implement only these modes:

| Mode | Runs FVP? | Purpose | First implementation shape |
| --- | --- | --- | --- |
| `preflight` | Optional | Check whether the target Linux host exposes the KVM CCA ABI needed by OpenVMM. | Build a tiny aarch64 Rust probe binary, stage it into Plane0 for FVP runs, and print `/dev/kvm`, `KVM_CAP_ARM_RMI`, `KVM_CAP_GUEST_MEMFD`, `KVM_CAP_MEMORY_ATTRIBUTES(PRIVATE)`, `KVM_CAP_ARM_VM_IPA_SIZE`, Realm VM creation, VM private-memory caps, and VGICv3 availability. The same binary may be run manually on native CCA hosts, but automated native-host transport is deferred. |
| `stage-only` | No | Build/copy artifacts into an isolated rootfs copy but do not boot FVP. | Reuse the existing rootfs fsck/resize/mount/copy/unmount flow from `local_run_cca_test`, but first copy the shrinkwrap rootfs into `--test-root` so native KVM staging cannot pollute the shared `cca-tests` rootfs. Copy the OpenVMM binary, preflight binary, guest kernel, guest initrd, and any run scripts into `/cca` (or a native subdirectory under `/cca`) in that copy. |
| `interactive-host` | Yes | Boot Plane0 Linux with artifacts staged and leave the environment available for manual commands. | Run `shrinkwrap run cca-3world.yaml --rtvar ROOTFS=<rootfs.ext2>` with the staged rootfs and do not try to parse a guest success marker. Prefer the most direct shrinkwrap/FVP interactive mode available; if none exists, document the manual serial-console interaction needed for the first version. |
| `run-openvmm` | Yes | Non-interactive local smoke run of OpenVMM inside Plane0. | Stage artifacts and a script that runs the preflight first, then invokes OpenVMM with the actual CCA CLI/config supported by the code, direct arm64 Linux boot, device-tree boot mode, serial enabled, and only the first supported PCIe/virtio profile. For the MVP, use an explicit Plane0 boot-time init hook in the staged rootfs to run this script and write logs; do not rely on ad hoc serial typing for this mode. Stream/collect Plane0, OpenVMM, and Realm guest serial logs. |

Do not implement these modes in the first MVP, but leave room for them:

- `hold-on-failure`: a modifier for `run-openvmm` that leaves FVP/Plane0 alive
  after failure for manual inspection.
- `native-host`: run the same preflight and OpenVMM command directly on a real
  CCA-capable Linux host or an already-running Plane0 shell, without launching
  FVP.
- `smoke-test`: CI-shaped mode that waits for a guest serial marker and returns
  pass/fail; this should be the bridge toward a later VMM test.

Concrete Flowey implementation outline for the MVP:

- Add a new `kvm_cca_tests` pipeline under `flowey/flowey_hvlite/src/pipelines`
  and register it in `pipelines/mod.rs` and the top-level clap pipeline enum.
  Add any new Flowey jobs to `flowey_lib_hvlite/src/lib.rs` and `_jobs` module
  registration.
- Add CLI options for:
  - `--test-root`, defaulting to `target/cca-test`;
  - terminal maintenance modes `--install-emu` and `--update-emu`. These should
    perform the requested maintenance and exit; unlike the existing
    `cca-tests --update-emu` behavior, they should not continue into a run mode.
    Combining maintenance modes with `--preflight`, `--stage-only`,
    `--interactive-host`, or `--run-openvmm` should fail fast with a clear CLI
    error;
  - one mutually-exclusive run-mode selector: `--preflight`, `--stage-only`,
    `--interactive-host`, or `--run-openvmm`;
  - existing environment maintenance options equivalent to `cca-tests`
    (`--rebuild-plane0-linux`, `--rebuild-rootfs`) either by sharing the
    existing structs or by delegating to the existing jobs;
  - explicit Plane0 host kernel input for the FVP run, defaulting to the
    shared local CCA FVP kernel image from
    `~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image` when available.
    Also allow an explicit host kernel source tree/revision for rebuilds,
    because native KVM CCA tests need a different host kernel feature set than
    the existing TMK/OpenHCL `cca-tests` path;
  - explicit local guest kernel path for the Realm guest, defaulting to the same
    shared local CCA FVP kernel image used for Plane0 host testing. The
    `~/ai/eevee/linux/build-cca-fvp-kernels.sh` helper is intended to produce
    one `Image` suitable for both the Plane0 host and the direct-boot Realm
    guest for local MVP validation;
  - guest initrd handling that defaults to the aarch64 `openvmm-deps` initrd via
    `resolve_openvmm_test_initrd`, with `--guest-initrd <path>` as an override.
    Keep an explicit future `--no-guest-initrd` option possible later, since
    existing `cca-tests` kvmtool flow launches its Realm kernel without an
    initrd. For the MVP, absence of `--guest-initrd` means "use the default
    aarch64 openvmm-deps initrd", not "boot without an initrd";
  - an optional extra OpenVMM command-line string for local debugging.
- Reuse `flowey_lib_hvlite::build_openvmm::Node` to cross-build an aarch64
  Linux `openvmm` binary (`CommonArch::Aarch64`,
  `CommonPlatform::LinuxGnu`, debug profile for local MVP).
- Expand `~/ai/eevee/...` defaults through `$HOME`/home-directory logic before
  converting them to `PathBuf`s; do not rely on shell expansion inside Flowey
  Rust code. Validate before staging that host and guest kernel paths exist and
  are regular files, with clear errors if the shared local `Image` has not been
  built yet.
- Add a tiny aarch64 Rust preflight binary. Prefer a small crate/binary that
  depends on the existing `kvm` crate and performs the checks listed above.
  Flowey should build it for aarch64 and stage it next to OpenVMM. Keep this
  probe independent of FVP so it can also run on native CCA hosts later.
- Configure the aarch64 default initrd resolver before requesting it. The
  pipeline must initialize `resolve_openvmm_test_initrd` through
  `cfg_versions::Init` or an equivalent config path so
  `resolve_openvmm_test_initrd::Request::Get(CommonArch::Aarch64, ...)` has a
  release version or local path to resolve.
- Validate the staged OpenVMM and preflight binaries against the Plane0 rootfs
  before launching FVP. For dynamically linked aarch64 Linux GNU binaries, use
  cross-safe ELF/interpreter/dependency inspection (for example `readelf` plus
  sysroot lookup), not host `ldd`, and either stage required libraries or switch
  to a static/musl-compatible build if that becomes available.
- Add a new Flowey job (or refactor the reusable pieces from
  `local_run_cca_test`) for rootfs staging:
  - validate `shrinkwrap`, its venv, Plane0 Linux `Image`,
    `rootfs.ext2`, `e2fsck`, and `resize2fs`;
  - copy the shrinkwrap `rootfs.ext2` into a per-run isolated file under
    `--test-root` before modification, and pass that copy to shrinkwrap. If a
    stable path is needed for interactive reuse, guard it with a lock file under
    `--test-root`; a second concurrent invocation must fail fast with a clear
    error instead of waiting or retrying;
  - run the same fsck/resize/rootfs mount flow as `local_run_cca_test` against
    the isolated copy;
  - use a unique absolute mount directory under `--test-root` instead of the
    current relative `mnt` directory, so concurrent or failed runs do not
    collide;
  - check free space before resizing/staging large artifacts;
  - copy the OpenVMM binary, preflight binary, Realm guest kernel/initrd, and
    scripts into `/cca`;
  - always unmount/sync/cleanup on failure.
- Ensure FVP actually boots the explicit `--host-kernel`, not merely a kernel
  copied into the rootfs. Implement this by passing the appropriate shrinkwrap
  runtime variable or overlay for the Plane0 Linux `Image` (or by updating the
  isolated package/config copy under `--test-root` if shrinkwrap does not expose
  such a runtime variable). The preflight/run scripts should print and log
  `uname -a` plus any available kernel build identifier so the booted host
  kernel can be matched to the requested input.
- For `preflight` under FVP, stage only the preflight binary and a small script,
  boot FVP, and run the script inside Plane0. If the current shrinkwrap setup
  does not provide a reliable non-interactive Plane0 command transport, make the
  first version an interactive run with clear instructions to execute
  `/cca/kvm_cca_preflight` manually.
- For `interactive-host`, stage artifacts and run FVP without enforcing a
  success marker. The output should tell the developer where the artifacts are
  inside Plane0 and the exact OpenVMM command to run manually.
- Add `--logs-dir <path>` for FVP-backed modes. If omitted, use
  `target/cca-test/kvm-cca/logs/latest`. The Flowey job should extract any
  available `/cca/logs/*` files from the isolated rootfs after FVP exits or
  times out, report the host log directory, and include at least:
  `kvm-cca-host.log`, `kvm-cca-inputs.log`, `kvm-cca-preflight.log`,
  `kvm-cca-preflight.status`, `openvmm.log`, and `openvmm.status` when those
  files exist.
- For `run-openvmm`, stage a script such as `/cca/run-openvmm-kvm-cca.sh` that:
  - runs `/cca/kvm_cca_preflight`;
  - runs OpenVMM with the CCA isolation CLI/config currently implemented by
    OpenVMM, direct arm64 Linux boot, device-tree boot mode, serial enabled, and
    the supplied guest kernel plus the default or overridden guest initrd;
  - logs the resolved guest kernel path, initrd path/version, and host kernel
    identity before launch;
  - writes OpenVMM and guest logs under `/cca/logs`.
  The Flowey job must install a boot-time init hook into the staged rootfs to run
  this script inside Plane0, run shrinkwrap/FVP with an explicit timeout,
  terminate or clean up FVP on timeout/failure, and collect those logs from the
  isolated rootfs or serial output after shutdown. If the init-hook mechanism is
  not reliable enough for non-interactive execution, keep `run-openvmm` behind
  an explicit "not implemented" error rather than silently hanging.
- Keep the existing `cargo xflowey cca-tests` behavior unchanged.

Expected command usage for the MVP:

```bash
# One-time or after toolchain/FVP setup changes. Reuses existing cca-tests
# emulator prerequisite and shrinkwrap/FVP installation infrastructure.
cargo xflowey kvm-cca-tests --install-emu

# Rebuild/update the Plane0 host kernel used by native OpenVMM KVM CCA tests.
# The source tree/revision is explicit because this path tests a different KVM
# feature set than the TMK/OpenHCL cca-tests payload.
cargo xflowey kvm-cca-tests --update-emu \
  --host-kernel-src ~/ai/eevee/linux \
  --host-kernel-rev <rev-or-branch> \
  --rebuild-plane0-linux

# Stage artifacts into an isolated rootfs copy, but do not launch FVP.
cargo xflowey kvm-cca-tests --stage-only \
  --host-kernel ~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image \
  --guest-kernel ~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image

# Boot FVP/Plane0 and run only the KVM CCA preflight probe. Guest kernel/initrd
# are not needed for this mode.
cargo xflowey kvm-cca-tests --preflight \
  --host-kernel ~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image \
  --logs-dir target/cca-test/kvm-cca/logs/preflight

# Boot FVP/Plane0 with the staged artifacts and leave it available for manual
# debugging. The command output should print artifact locations under /cca and
# the exact OpenVMM command/script to run manually inside Plane0.
cargo xflowey kvm-cca-tests --interactive-host \
  --host-kernel ~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image \
  --guest-kernel ~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image \
  --logs-dir target/cca-test/kvm-cca/logs/interactive \
  --share-dir target/cca-test/kvm-cca/share \
  --openvmm-memory 128M

# Stage artifacts, boot FVP/Plane0, run the preflight and OpenVMM via the
# boot-time init hook, then collect logs and shut down/clean up.
cargo xflowey kvm-cca-tests --run-openvmm \
  --host-kernel ~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image \
  --guest-kernel ~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image \
  --logs-dir target/cca-test/kvm-cca/logs/run-openvmm \
  --openvmm-memory 128M \
  --openvmm-extra-args "<extra debug args if needed>"
```

Mode behavior:

- `--install-emu` should only install or validate common emulator prerequisites
  and shrinkwrap/FVP assets. It should not stage native OpenVMM artifacts.
- `--update-emu --rebuild-plane0-linux` should build the requested host kernel
  and record enough metadata under `--test-root` to know which source/revision
  produced the default `--host-kernel` image, then exit without staging or
  launching FVP. For local MVP testing, this should be able to invoke
  `~/ai/eevee/linux/build-cca-fvp-kernels.sh`, whose default output is
  `~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image`.
- `--stage-only` should produce the isolated rootfs path and logs path, then
  exit. It should resolve and stage the default aarch64 `openvmm-deps` guest
  initrd when `--guest-initrd` is not provided, and fail clearly if the host or
  guest kernel path is missing or not a regular file. It should not run
  shrinkwrap.
- `--preflight` should stage the preflight binary and boot FVP with the explicit
  host kernel. It should not require `--guest-kernel` or `--guest-initrd`. It
  should extract logs to `--logs-dir` or the default log directory.
- `--interactive-host` should launch shrinkwrap/FVP with the isolated rootfs and
  provided host kernel, then leave control/log output suitable for manual
  debugging. It should keep the rootfs payload small and stable: inject only the
  Plane0 host kernel, `/cca/mount-kvm-cca-share.sh`, and the init hook. Large or
  frequently changing artifacts (`openvmm`, `kvm_cca_preflight`, guest kernel,
  guest initrd, and `run-openvmm-kvm-cca.sh`) should be staged in the host 9p
  `--share-dir` and mounted in Plane0 at `/cca-share`. It should not interpret
  guest success or failure, but it should still extract logs to `--logs-dir`
  when FVP exits.
- Interactive debugging workflow:
  - launch:
    `cargo xflowey kvm-cca-tests --interactive-host --host-kernel ~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image --guest-kernel ~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image --logs-dir target/cca-test/kvm-cca/logs/interactive`;
  - the FVP Plane0 console is exposed on telnet port 5000. If the console is not
    already attached in the `xflowey` terminal, attach from another terminal with
    `telnet localhost 5000`, press Enter, and log in as `root`;
  - stable rootfs artifacts live under `/cca`; frequently changing artifacts
    live in the host 9p share, mounted at `/cca-share` inside Plane0. If the
    mount is not present, run `/cca/mount-kvm-cca-share.sh`;
  - update host-side artifacts in `target/cca-test/kvm-cca/share/`, then rerun
    `/cca-share/run-openvmm-kvm-cca.sh` from the still-running Plane0 shell to
    avoid rebooting FVP;
  - if OpenVMM reaches `openvmm>`, use `help`, `resume`, `inspect`, `input`, and
    `quit` from the REPL. Stop FVP when finished so Flowey can extract
    `/cca/logs/*` to `--logs-dir`;
  - the Realm guest command line should include the PL011 console and early
    console for OpenVMM's COM1 device using the shared IPA half:
    `console=ttyAMA0,115200 earlycon=pl011,mmio32,0x8000effec000`.
    OpenVMM's generated DT should advertise `/openvmm/uart@8000effec000` as
    `/chosen/stdout-path` for CCA and dispatch shared-IPA MMIO exits back to the
    lower device address before chipset emulation.
- Faster iteration defaults:
  - use `--openvmm-memory 128M` while debugging. The temporary CCA RAM-acceptance
    hack populates every private RAM page via `KVM_ARM_RMI_POPULATE`, so smaller
    guest RAM directly reduces launch time. In local FVP runs, reducing from
    512 MiB to 256 MiB cut CCA population from about 74s to about 50s, and
    128 MiB cut it to about 38s while still reaching the OpenVMM REPL;
  - use `--openvmm-extra-args "<args>"` to restage a modified OpenVMM command
    line without editing `/cca/run-openvmm-kvm-cca.sh` by hand;
  - for OpenVMM-only changes, rebuild/copy the new binary to
    `target/cca-test/kvm-cca/share/openvmm` and rerun the `/cca-share` script
    without rebooting FVP. Rerun `--interactive-host` only when changing the
    Plane0 rootfs bootstrap, FVP inputs, or guest/host kernel images.
  - the fastest reliable loop currently runs OpenVMM from the 9p share while
    keeping the guest kernel and initrd staged in the rootfs as `/cca/guest-Image`
    and `/cca/initrd`. Directly reading large guest kernels from 9p, or copying
    them from 9p to `/tmp`, was unreliable/truncated during local testing.
    Avoid passing `--guest-kernel` as a path inside `--share-dir`, because that
    can self-copy and truncate the share file.
- Current proven FVP state:
  - with the no-KASLR guest kernel, CCA direct Linux boot reaches early console,
    reports `RME: Using RSI version 1.0`, enables `ttyAMA0`, runs `/init`, and
    drops to an initrd shell because no root device is specified;
  - the temporary full-RAM population hack is still required because the current
    Linux guest expects all described RAM to be `RIPAS RAM` before entry;
  - the kvmtool-style four-PPI timer DT/KVM PPI change was tested and is not
    needed for this boot path, so it should remain dropped.
- Remaining issues to investigate:
  - Linux warns in `arm64_rsi_is_protected()` while probing the shared-IPA PL011
    MMIO regions. The console still works, but the warning should be understood
    before treating the device model as complete;
  - `--run-openvmm` still needs a non-interactive success condition instead of
    relying on the OpenVMM REPL;
  - iteration is still slowed by CCA population time (about 38 seconds at
    128 MiB with the full-RAM hack). Future improvements include a Realm boot
    shim that accepts RAM itself, using smaller purpose-built test payloads, and
    avoiding rootfs restaging for OpenVMM-only changes through the 9p share.
- `--run-openvmm` should resolve the guest initrd from `openvmm-deps` for
  aarch64 unless `--guest-initrd` is provided, then fail fast if the boot-time
  init hook cannot be staged. On success it should run
  `/cca/kvm_cca_preflight`, then `/cca/run-openvmm-kvm-cca.sh`, enforce a
  timeout, extract logs to `--logs-dir` or the default log directory, and clean
  up FVP/rootfs mounts.

### 12. Kernel and userspace prerequisites for FVP

The FVP environment needs both host-side CCA support in Plane0 Linux and an
enlightened guest kernel.

Concrete work:

- For native OpenVMM KVM CCA validation, treat the Plane0 host kernel as an
  explicit input to `kvm-cca-tests`, not an implicit artifact inherited from the
  existing TMK/OpenHCL `cca-tests` flow. For local testing, use
  `~/ai/eevee/linux/build-cca-fvp-kernels.sh`; it builds
  `~/ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image`, and its
  `kernel/configs/cca_fvp.config` enables the shared host/guest feature set:
  KVM, Arm CCA guest support, initrd, PL011 console, virtio MMIO/PCI, 9P, Hyper-V
  VTL mode, and common filesystems. The host kernel must provide `guest_memfd`,
  generic memory attributes, Realm VM creation, `KVM_ARM_RMI_POPULATE`, RIPAS
  changes, Realm MMIO, VGIC/timer, and in-kernel Realm PSCI completion support.
- Keep sharing the emulator/shrinkwrap install and rootfs build infrastructure
  from `cca-tests`; only the staged payloads and host/guest kernel inputs differ
  for the native OpenVMM KVM CCA path.
- Ensure the KVM userspace headers used by OpenVMM expose the v14 RMI/CCA ioctls
  and constants listed in section 2.
- Build or provide the guest Linux kernel with Arm CCA/Realm awareness and the
  enlightenments expected by the existing OpenVMM + KVM Linux path. For local
  MVP testing, use the same `Image` produced by
  `~/ai/eevee/linux/build-cca-fvp-kernels.sh` for the Realm guest. Use the
  aarch64 `openvmm-deps` initrd by default via `resolve_openvmm_test_initrd`,
  with an explicit `--guest-initrd` override when needed. Verify that this initrd
  works with the CCA guest kernel, serial console, and the supported virtio/PCIe
  profile.
- Include kvmtool in the FVP environment as a reference/debug tool, but use
  OpenVMM for the target launch.
- Add a preflight command that prints KVM RMI capability, supported Realm VM type
  and IPA size, and available SVE/debug features.
- Document the exact host/guest kernel source revision, the
  `cca_fvp.config` contents used for the build, the resolved initrd artifact
  version, and the smoke-test marker once selected.

### 13. Tests and validation

Add tests at three levels.

Unit tests:

- CCA config parsing and rejection of invalid measured-population/SVE/debug
  settings.
- Initial launch-page descriptor conversion for arm64 kernel/initrd/DTB/RAM.
- Private-memory range validation: alignment, overlap, MMIO exclusion, and full
  coverage.
- RIPAS conversion state updates and backing discard behavior.
- CCA state machine ordering: create Realm VM, set private memory, populate,
  set allowed registers, first run.

Local non-FVP checks:

- Existing SNP tests must keep passing after refactoring shared private-memory
  helpers.
- Normal KVM aarch64 non-isolated Linux boot must keep using userspace memory and
  the normal VM path.
- Unsupported CCA combinations must fail early with clear errors.

FVP validation:

- `cargo xflowey cca-tests --install-emu` installs or validates the emulation
  environment from PR 3455.
- `cargo xflowey kvm-cca-tests --preflight` builds/stages the tiny KVM CCA
  preflight probe and verifies the CCA KVM ABI inside Plane0 (or documents the
  manual command when the first version is interactive-only).
- `cargo xflowey kvm-cca-tests --stage-only` stages OpenVMM, the preflight
  probe, explicit Plane0 host kernel, guest kernel/initrd, and run scripts into
  the isolated CCA rootfs without launching FVP.
- `cargo xflowey kvm-cca-tests --interactive-host` boots the CCA FVP/Plane0
  environment with those artifacts staged for manual debugging.
- `cargo xflowey kvm-cca-tests --run-openvmm` builds or stages OpenVMM and
  launches it inside Plane0 Linux.
- OpenVMM creates a KVM Realm VM, sets private memory, populates
  kernel/initrd/DTB, sets allowed initial registers, and runs the guest.
- Guest serial output proves the enlightened Linux guest reaches userspace or the
  agreed smoke-test marker.
- Logs capture Realm measurement/configuration, KVM RMI capabilities, population
  ranges, first-run activation, RIPAS conversions, and any unsupported exits.

## Suggested milestones

1. **UAPI and gates:** add v14 KVM bindings/wrappers, CCA isolation config,
   capability probing, and early unsupported-feature errors without changing
   normal KVM/SNP behavior.
2. **Shared private memory:** refactor SNP `guest_memfd`/memory-attribute helpers
   and enable guest-private backing for aarch64 CCA partitions.
3. **Realm VM skeleton:** create the Realm VM type, require VGICv3, select
   guest_memfd backing, and add launch state machine checks.
4. **arm64 loader metadata:** preserve accepted ranges from the direct Linux
   loader and route them through `set_initial_page_visibility()` for CCA.
5. **Initial population:** consume `InitialAcceptedPage` in virt_kvm and populate
   private accepted pages with `KVM_ARM_RMI_POPULATE`.
6. **First run:** set Realm-allowed initial vCPU state, enter `KVM_RUN`, and
   reach the first Realm exit.
7. **Minimal runtime/device profile:** run direct Linux with serial plus
   virtio/PCIe, GICv3, PSCI, MMIO, timer basics, and fatal handling for
   unsupported exits/RIPAS ranges. Do not expose VMBus.
8. **FVP Flowey integration:** add a native `kvm-cca-tests` Flowey pipeline with
   the first four MVP modes: `preflight`, `stage-only`, `interactive-host`, and
   `run-openvmm`. Reuse the PR 3455 FVP environment setup and collect
   logs/artifacts.
9. **Hardening:** expand tests, tighten device policy, remove temporary FVP-only
   assumptions, and document unsupported production features.

## Out of scope for the first virt_kvm phase

- Production attestation. Preserve useful measurement/configuration logs, but do
  not implement an attestation flow.
- Non-MVP Flowey modes such as `hold-on-failure`, `native-host`, and automated
  `smoke-test` pass/fail behavior. Keep the first Flowey implementation focused
  on preflight, staging, interactive debugging, and a non-interactive local
  OpenVMM run.

## Review

Plan review for the native KVM CCA Flowey modes found the overall direction
acceptable with minor revisions. The plan was updated to require an explicit
Plane0 command-transport strategy, isolate native KVM rootfs staging from the
existing `cca-tests` rootfs, clarify that automated native-host mode is
deferred, validate/stage OpenVMM runtime dependencies, define timeout and
cleanup behavior, use safe mount directories under `--test-root`, and list the
Flowey registration points needed for the new pipeline and jobs.

A follow-up review asked for three clarifications before implementation: choose
a concrete Plane0 command transport for `run-openvmm`, avoid host `ldd` for
aarch64 dependency checks, and avoid concurrent rootfs staging collisions. The
plan now specifies a Plane0 boot-time init hook for `run-openvmm`, cross-safe
ELF/interpreter inspection for staged aarch64 binaries, and per-run rootfs
copies or a `--test-root` lock for reusable staging paths.

User follow-up clarified that lock contention should fail fast and that native
OpenVMM KVM CCA validation must use explicitly selected host and guest kernels.
The plan now says a second concurrent stable-rootfs staging invocation must
error immediately, the emulator/shrinkwrap install infrastructure should be
shared with existing `cca-tests`, and `kvm-cca-tests` must take explicit Plane0
host-kernel and Realm guest-kernel inputs for the feature set under test.

## Files likely touched

- `vmm_core/virt/src/generic.rs`
- `vmm_core/virt/src/aarch64/mod.rs`
- `vmm_core/virt_kvm/src/lib.rs`
- `vmm_core/virt_kvm/src/arch/aarch64/mod.rs`
- `vm/loader/src/importer.rs`
- `vm/loader/src/linux.rs`
- `vmm_core/vm_loader/src/lib.rs`
- `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs`
- OpenVMM CLI/config files that select isolation mode
- KVM Rust bindings for Arm RMI/CCA UAPI
- Flowey files from PR 3455 under `flowey/flowey_hvlite` and
  `flowey/flowey_lib_hvlite`
- `Guide/` documentation for the new FVP/native KVM CCA test command
