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

### 12. Kernel and userspace prerequisites for FVP

The FVP environment needs both host-side CCA support in Plane0 Linux and an
enlightened guest kernel.

Concrete work:

- Build Plane0 Linux from `~/ai/eevee/NV-Kernels`, the reference Linux tree with
  the v14 Arm CCA KVM series applied. It must provide `guest_memfd`, generic
  memory attributes, Realm VM creation, `KVM_ARM_RMI_POPULATE`, RIPAS changes,
  Realm MMIO, VGIC/timer, and in-kernel Realm PSCI completion support.
- Ensure the KVM userspace headers used by OpenVMM expose the v14 RMI/CCA ioctls
  and constants listed in section 2.
- Build the guest Linux kernel with Arm CCA/Realm awareness and the enlightenments
  expected by the existing OpenVMM + KVM Linux path.
- Include kvmtool in the FVP environment as a reference/debug tool, but use
  OpenVMM for the target launch.
- Add a preflight command that prints KVM RMI capability, supported Realm VM type
  and IPA size, and available SVE/debug features.

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
- A new/native mode builds or stages OpenVMM and launches it inside Plane0 Linux.
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
8. **FVP Flowey integration:** add the native OpenVMM KVM CCA mode to the PR 3455
   FVP pipeline and collect logs/artifacts.
9. **Hardening:** expand tests, tighten device policy, remove temporary FVP-only
   assumptions, and document unsupported production features.

## Out of scope for the first virt_kvm phase

- Production attestation. Preserve useful measurement/configuration logs, but do
  not implement an attestation flow.
- Final Flowey command shape. The native FVP mode can be added under
  `cca-tests` or a separate `kvm-cca-tests` command during Flowey integration.

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
