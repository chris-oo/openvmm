# KVM CCA v15 and guest_memfd In-Place Roadmap

## Strategy

Implement this work in two independently validated phases:

1. **CCA v14 to v15:** retain OpenVMM's existing two-backing model. Create
   guestmemfd with flags `0`, keep it permanently PRIVATE, use the separate
   userspace mapping as the shared view and Realm population source, and remove
   CCA memory-attribute ioctls.
2. **CCA guestmemfd in-place:** optionally replace the two backings with one
   mmap/init-shared guestmemfd and drive fd-owned attributes.

SNP remains on the existing non-in-place implementation in both phases because
upstream and distribution kernels do not yet provide the in-place ABI it would
need.

## Verified kernel model

The v15 kernel permits a Realm slot with guestmemfd plus a separate
`userspace_addr`. With guestmemfd creation flags `0`:

- the entire guestmemfd starts PRIVATE;
- the slot is not `KVM_MEMSLOT_GMEM_ONLY`;
- protected-IPA faults use guestmemfd;
- shared-alias faults use `userspace_addr`;
- `KVM_ARM_RMI_POPULATE` requires and accepts the separate userspace HVA;
- RIPAS changes are committed by KVM on the requesting REC's next entry; and
- arm64 fault routing does not consult guestmemfd attributes.

Therefore Phase 1 does **not** replace VM-fd `KVM_SET_MEMORY_ATTRIBUTES` with
guestmemfd-fd `KVM_SET_MEMORY_ATTRIBUTES2`. It removes CCA attribute changes
and leaves guestmemfd PRIVATE for its lifetime.

This applies only to Phase 1. With flags `0`, guestmemfd starts PRIVATE and
stays PRIVATE; the Realm's shared IPA alias uses the separate userspace
backing. In Phase 2, the single guestmemfd starts SHARED and OpenVMM must change
its attributes whenever memory transitions between shared and private.

## Phase 1: Move CCA from v14 to v15

### 1. Establish the v15 environment first

- [ ] Preserve the current v14 kernel and firmware artifacts as a rollback.
- [ ] Use `/home/coo/ai/jolteon/linux-cca`, currently `cca-host/v15` at
  `d6d40e6a310388f58def167f8b57e31d6c6057a2`.
- [ ] Verify and record the revision's v15 ancestry and LKML message IDs; do
  not reconstruct the already-applied patch stack.
- [ ] Add an OpenVMM-owned reproducible kernel build script and config
  fragments, for example under `build_support/cca/`. The script takes
  `--source /home/coo/ai/jolteon/linux-cca` and never modifies that pinned
  source tree.
- [ ] Build two separate configurations from the same pinned source revision:
  - FVP normal-world host kernel; and
  - Realm guest kernel for OpenVMM direct Linux boot.
- [ ] Keep separate output directories and `.config` files, for example
  `<output-root>/cca-fvp-host` and `<output-root>/cca-realm-guest`, and pass
  them through kernel `O=` builds. Never place `.config` or outputs in the
  source tree, reuse v14 output, or share one role's configuration with the
  other.
- [ ] Make the script accept explicit source revision, output root,
  cross-compiler/toolchain, and job-count inputs. Fail if the checked-out
  revision differs from the requested revision or the source worktree contains
  unaccounted changes.
- [ ] Generate each configuration from `arm64 defconfig` plus checked-in config
  fragments, run `olddefconfig`, and verify every required symbol after
  resolution.
- [ ] Host fragment requirements:
  - KVM/Realm host support and guestmemfd;
  - RMM firmware driver support selected by arm64 KVM;
  - PL011 console;
  - virtio block, net, PCI, console, and 9P support required by the FVP
    rootfs/share; and
  - required filesystems and initrd support.
- [ ] Assert exact built-in `=y` values for all boot-critical host symbols,
  especially `CONFIG_NET_9P`, `CONFIG_NET_9P_VIRTIO`, and `CONFIG_9P_FS`.
  `arm64 defconfig` sets these to modules, but the staged rootfs does not carry
  matching modules for this custom kernel.
- [ ] Guest fragment requirements:
  - `CONFIG_VIRT_DRIVERS=y` and `CONFIG_ARM_CCA_GUEST=y` for CCA attestation
    validation (the core RSI support needed to run as a Realm is built
    independently);
  - virtio PCI, block, net, console, and RNG;
  - PL011 as a debug fallback;
  - initrd/devtmpfs support; and
  - any filesystem/network options required by the OpenVMM smoke initrd.
- [ ] Assert exact `=y` for every guest driver needed before the initrd can
  load modules. Do not accept `=m` as equivalent.
- [ ] Carry forward the FVP build settings currently applied by the
  Shrinkwrap overlay: `CONFIG_HZ_100=y`, `CONFIG_HZ_250=n`, and
  `CONFIG_RANDOMIZE_BASE=n`.
- [ ] Make output reproducible by setting `KBUILD_BUILD_TIMESTAMP`,
  `KBUILD_BUILD_USER`, and `KBUILD_BUILD_HOST`, disabling
  `CONFIG_LOCALVERSION_AUTO`, and recording all values in the manifest.
- [ ] Produce:
  - `host-Image`;
  - `guest-Image`;
  - both resolved `.config` files;
  - `kernelrelease` values; and
  - a manifest containing the source commit, config-fragment hashes, toolchain
    identity, build commands, and output hashes.
- [ ] Add a clean-build mode that removes only the two named output
  directories and rebuilds both images from the pinned source.
- [ ] Record that arm64 KVM selects `ARM_RMM`; arm64 has no
  `CONFIG_KVM_VM_MEMORY_ATTRIBUTES`.
- [ ] Identify and pin a TF-RMM commit that implements RMI v2.0-bet2. RMI
  version negotiation reports 2.0 for both beta revisions, so only an actual
  Realm boot proves compatibility.
- [ ] Determine and pin the compatible TF-A revision.
- [ ] Query and record the host IPA limit before creating any Realm VM, then
  probe Realm VM creation with descending IPA sizes. Realm creation updates
  the kernel's effective IPA limit globally, and the Realm limit is
  constrained by the pinned RMM's `S2SZ`.
- [ ] Print the pre-Realm host limit and the first successful Realm limit, then
  use the Realm size for the VM type and shared-IPA bit.
- [ ] Record the supported Realm IPA size as a property of the pinned
  TF-RMM/TF-A firmware combination.
- [ ] Reconcile all host-kernel sources. Make `linux-cca` the canonical v15
  source for local/FVP validation and update conflicting
  `PLANE0_LINUX_BRANCH`, repro-script, Shrinkwrap revision, and
  `LinuxTestKernelVersion::KvmCcaDev` inputs. State explicitly if the incubator
  KvmCcaDev kernel keeps a separate pin.
- [ ] Update `kvm-cca-tests` to consume the build manifest's distinct host and
  guest images. Do not default `guest_kernel` to `host_kernel`.
- [ ] Replace the currently rejected `--host-kernel-src/--host-kernel-rev`
  options with source-wide names such as
  `--cca-kernel-src/--cca-kernel-rev`.
- [ ] Add a Flowey node that invokes the build script and returns both images
  plus the manifest as `ReadVar` outputs.
- [ ] Change `local_stage_kvm_cca::Params.host_kernel` and `guest_kernel` to
  consume `ReadVar<PathBuf>` values. Preserve explicit CLI path overrides via
  `ReadVar::from_static`.
- [ ] Update `run-cca-openvmm-repro.sh` defaults to the new host/guest artifact
  paths rather than the old `~/ai/eevee/linux` image.
- [ ] Keep the older `OHCL-Linux-Kernel:cca-dev` Plane0 Realm build only for
  legacy Realm-planes flows that still require it; the native KVM CCA v15 flow
  must use the pinned `linux-cca` build for the FVP normal-world host.
- [ ] Bump, do not remove, the Shrinkwrap Linux source revision because
  kvmtool header generation and kvm-unit-tests consume `LINUX_SOURCE`.
- [ ] Add Flowey package prerequisites to the kernel-build job, including
  `gcc-aarch64-linux-gnu`, `bc`, `bison`, `flex`, and `libssl-dev`; do not rely
  on the separate `--install-emu` job having run.
- [ ] Wire the TF-A and TF-RMM revisions through their actual Shrinkwrap
  overlay/`--btvar` locations, add staleness detection, and use the currently
  unused update-subcommand revision fields rather than leaving hardcoded
  overrides.
- [ ] Update Shrinkwrap/Flowey pins separately from OpenVMM code, but build and
  boot the pinned kernel/firmware before changing the shared CCA bookmark.

Relevant series:

- guestmemfd in-place v8:
  `<20260618-gmem-inplace-conversion-v8-0-9d2959357853@google.com>`;
- guestmemfd fix:
  `<114e2488-97ed-4740-a8e8-1edd991f26c5@arm.com>`;
- generic RMM firmware:
  `<20260715142739.80398-1-steven.price@arm.com>`; and
- KVM CCA v15:
  `<20260715142841.80544-1-steven.price@arm.com>`.

### 2. Update the CCA v15 ABI and preflight

Files:

- `vm/kvm/src/lib.rs`
- `vm/kvm/kvm_cca_preflight/src/main.rs`
- `vmm_core/virt_kvm/src/memory.rs`
- `vmm_core/virt_kvm/src/cca.rs`

- [ ] Change `KVM_CAP_ARM_RMI_UAPI` from `249` to `251`.
- [ ] Add `KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES = 250` for capability
  detection and diagnostics. Do not add the attribute ioctl until Phase 2.
- [ ] Update `check_private_memory_extensions()` and every caller, including
  CCA population, so arm64 no longer requires
  `KVM_CAP_MEMORY_ATTRIBUTES`.
- [ ] Require the Realm VM-fd query for
  `KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES` to report
  `KVM_MEMORY_ATTRIBUTE_PRIVATE`. The system-fd result alone is insufficient.
- [ ] Document why this capability is required in Phase 1 even though
  OpenVMM does not issue the attribute ioctl: arm64 population reads the
  guestmemfd-owned PRIVATE default, and a host booted with
  `gmem_in_place_conversion=0` cannot satisfy `KVM_ARM_RMI_POPULATE`.
- [ ] Keep `KVM_CAP_USER_MEMORY2`, `KVM_CAP_GUEST_MEMFD`, Realm VM creation,
  and VGICv3 checks.
- [ ] Have preflight identify likely v14 (`249`) versus v15 (`251`) kernels
  and print the effective guestmemfd-attribute mode.
- [ ] Add a no-progress guard to the `KVM_ARM_RMI_POPULATE` loop.
- [ ] Keep SNP capability checks and VM-fd attribute code unchanged.

The KVM-capability change and removal of CCA VM-fd attribute calls must land in
the same commit so no intermediate commit advertises v15 while executing the
v14-only ioctl.

This is a hard kernel cutover: after that commit, v14 CCA kernels are no longer
supported. A useful checkpoint is that v15 Realm launch and basic execution
should work immediately after the cutover, before the improved stale-backing
purge lands.

### 3. Keep guestmemfd permanently PRIVATE

File: `vmm_core/virt_kvm/src/memory.rs`

- [ ] Continue creating CCA guestmemfd with flags `0`.
- [ ] Continue registering slots with both the existing userspace address and
  guestmemfd offset.
- [ ] Remove CCA calls to VM-fd `KVM_SET_MEMORY_ATTRIBUTES`, including initial
  slot registration, RIPAS handling, replacement, and unmap.
- [ ] Do not call `KVM_SET_MEMORY_ATTRIBUTES2` in Phase 1.
- [ ] Keep `private_memory_range_from_slots()` returning the userspace HVA for
  `KVM_ARM_RMI_POPULATE.source_uaddr`.
- [ ] Replace the shared `private_attributes_set` assumption with an explicit
  private-state model, for example
  `VmFdAttributesPrivate` for SNP and `GmemDefaultPrivate` for Phase 1 CCA.
  Preserve SNP's existing population predicate unchanged.
- [ ] Keep measured/unmeasured population flags and partial-progress handling.

### 4. Handle RIPAS with cleanup only

Files:

- `vmm_core/virt/src/generic.rs`
- `vmm_core/virt_kvm/src/memory.rs`
- `vmm_core/virt_kvm/src/arch/aarch64/mod.rs`
- `vmm_core/virt_kvm/src/lib.rs`
- `openvmm/membacking/src/memory_manager/mod.rs`
- `openvmm/openvmm_core/src/worker/dispatch.rs`

For a CCA `KVM_EXIT_MEMORY_FAULT` caused by RIPAS:

- target RAM/private: discard the stale userspace/shared backing with a
  backing-aware purge;
- target EMPTY/shared: punch the guestmemfd range while its attributes remain
  PRIVATE; and
- re-enter the requesting VP so KVM commits RIPAS.

The PRIVATE attribute is a safety invariant. Guestmemfd hole punching derives
its invalidation filter from the current attribute; punching while SHARED can
free pages without undelegating them from the RMM.

- [ ] Keep the current strict `guest_memfd_range_segments()` behavior for SNP.
  Add a CCA-specific lenient intersection helper so unbacked portions of a
  valid RIPAS request are cleanup no-ops without weakening SNP validation.
- [ ] Purge the shared backing through the existing HVA with
  `MADV_REMOVE`. On `MAP_SHARED` shmem this punches the page cache without new
  fd/offset plumbing. If the mapping is anonymous and `MADV_REMOVE` returns
  `EINVAL`, use `MADV_DONTNEED`.
- [ ] Apply this backing-aware purge fix to the shared helper used by both CCA
  and SNP; it corrects a pre-existing SNP stale-shared-backing bug. Preserve
  architecture-specific validation and attribute behavior.
- [ ] Reject Phase 1 CCA configurations with pinned RAM, assigned devices,
  vhost-user memory consumers, or other uncoordinated DMA users before
  partition creation. Hole punching cannot race in-flight host/DMA access.
- [ ] Clean only configured guestmemfd-backed intersections. A valid RIPAS
  request over an unbacked IPA requires no OpenVMM cleanup and must not become
  a guest-triggerable fatal error.
- [ ] Snapshot and validate all slot and backing segments under their locks,
  then drop the locks before draining VPs. After the drain, reacquire the
  locks and revalidate the snapshot and generation before mutating anything.
  Abort without mutation if the mapping graph changed.
- [ ] Punch before re-entering the requesting VP. With PRIVATE attributes,
  KVM invalidation unmaps and undelegates before truncating the folios; KVM
  then commits RIPAS on REC re-entry.
- [ ] Purge the shared backing before re-entry for the private transition.
- [ ] Treat any failure after cleanup mutation begins as partition-fatal.
- [ ] Treat cleanup failures as partition-fatal; never resume with stale
  contents that could reappear on a later transition.

### 5. Stress concurrent RIPAS before adding a VP gate

KVM serializes guestmemfd invalidation and, when committing RIPAS EMPTY, unmaps
and undelegates any private page another VP raced to re-create. Do not add a
cross-VP rendezvous unless testing demonstrates a surviving correctness issue.

- [ ] Add a multi-VP stress test in which other VPs repeatedly access a range
  while one VP requests private/shared RIPAS transitions.
- [ ] Monitor for RMI errors, kernel warnings, leaked delegation, stale data,
  or failed transitions.
- [ ] Keep a cheap partition-wide fatal latch for cleanup failures after
  mutation. On fatal, kick all VPs and prevent every VP from re-entering KVM.
- [ ] If the stress test exposes a real race, then add an asynchronous bounded
  epoch drain with explicit in-KVM ordering, VP acknowledgements, and
  snapshot/drop-lock/drain/revalidate sequencing. Do not block a shared
  executor thread or wait while holding memory/backing locks.

### 6. Define slot reuse and teardown

- [ ] Guestmemfd attributes remain PRIVATE across bind/unbind.
- [ ] Before live unbind/reuse, punch the exact guestmemfd offset range while
  PRIVATE to purge contents and undelegate any mapped granules.
- [ ] Perform this punch before `clear_slot()` removes the guestmemfd binding;
  memslot deletion also undelegates, but pre-unbind punching guarantees content
  purging while the binding and invalidation context are explicit.
- [ ] Make punch failure fatal.
- [ ] Do not convert the range SHARED during unbind.
- [ ] On final teardown, keep the partition-owned guestmemfd open until all
  VPs stop and all slots are unbound; then close it.

### 7. Add mandatory test seams

Direct fd syscalls are difficult to unit-test.

- [ ] Add a small injectable backing-cleanup abstraction around
  `MADV_REMOVE`, `MADV_DONTNEED`, and guestmemfd hole punching.
- [ ] Add a pure, syscall-free fatal-latch state type and, if the stress gate
  becomes necessary, a pure VP epoch/drain state machine.
- [ ] Test pure range segmentation and intersection independently.
- [ ] Avoid duplicated test-only implementations of production helpers.

### Phase 1 validation

- [ ] Re-run the known-good v14 CCA smoke test.
- [ ] Run the kernel build script from a clean output root and verify the
  manifest records the pinned `linux-cca` commit.
- [ ] Boot the generated host image under FVP and verify `/dev/kvm`, Realm
  capability 251, guestmemfd, RMM initialization, 9P share mounting, and the
  updated preflight.
- [ ] Direct-boot the generated guest image under OpenVMM with the staged
  initrd and verify virtio console, block, net, RNG, and the Arm CCA guest
  driver.
- [ ] Cross-build all affected aarch64 packages.
- [ ] Run the v15 preflight on the pinned FVP host.
- [ ] Confirm CCA launch performs no memory-attribute ioctl.
- [ ] Confirm initial population uses the existing userspace HVA.
- [ ] Test measured/unmeasured population and no-progress handling.
- [ ] Test private/shared RIPAS transitions and ranges crossing slot
  boundaries.
- [ ] Test shared-backing purging for default file-backed RAM and anonymous
  RAM; verify old contents cannot reappear.
- [ ] Test unbacked portions of a RIPAS range as no-op cleanup.
- [ ] Run the multi-VP concurrent RIPAS stress test.
- [ ] Test the partition-wide fatal latch.
- [ ] If a VP gate is added, test epoch ordering, cancellation, and
  slot/backing generation changes between snapshot and revalidation.
- [ ] Test cleanup failure after mutation.
- [ ] Test slot unbind/reuse purging at the same and a different GPA.
- [ ] Pass FVP CCA block/network smoke tests.
- [ ] Re-run SNP direct-boot block/network tests on an
  upstream/distribution kernel without guestmemfd in-place support.

### Phase 1 commits

1. Add the pinned host/guest kernel build script, config fragments, manifest,
   and Flowey artifact wiring.
2. Land the v15 kernel/firmware pins and the matching OpenVMM v15 capability
   cutover together, then build and boot them. Do not create a commit where
   FVP boots v15 but OpenVMM still probes v14 capability 249.
3. Land the removal of CCA VM-fd attribute calls in that same cutover or an
   immediately inseparable commit.
4. Land RIPAS cleanup ordering, backing-aware purge, and the fatal latch.
5. Add focused tests.
6. Update remaining Flowey/Shrinkwrap pins.
7. Complete FVP CCA and hardware SNP validation.

Create a rollback bookmark before step 2. Kernel/firmware pins and CCA
capability constants must be reverted together.

## Phase 2: Move CCA to guestmemfd mmap/in-place

Start only after Phase 1 is stable. Keep Phase 2 explicitly opt-in, for example
with `CcaGuestMemfdMode::{TwoBacking, InPlaceExperimental}`. Capability
presence validates the requested mode but never enables it automatically.
`TwoBacking` remains the default and fallback.

### 1. Add the Phase 2 KVM ABI

Files:

- `vm/kvm/src/lib.rs`
- `vm/kvm/kvm_cca_preflight/src/main.rs`

- [ ] Add `KVM_CAP_GUEST_MEMFD_FLAGS = 244`.
- [ ] Add `GUEST_MEMFD_FLAG_MMAP` and
  `GUEST_MEMFD_FLAG_INIT_SHARED`.
- [ ] Add the 128-byte `kvm_memory_attributes2` layout with
  `error_offset` and `reserved[11]`.
- [ ] Add `_IOWR(KVMIO, 0xd2, struct kvm_memory_attributes2)` as a free
  function or `BorrowedFd` API targeting guestmemfd, not `Partition`.
- [ ] Change `create_guest_memfd()` to accept validated flags while preserving
  Phase 1 flags `0`.
- [ ] Add ABI, ioctl-encoding, flag-mask, and creation tests.
- [ ] Select in-place only when the Realm VM-fd capability includes PRIVATE
  and the creation flags are accepted.

`KVM_CAP_GUEST_MEMFD_FLAGS` is not a mode selector; legacy configurations may
advertise both flags. `KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES` is the
authoritative in-place capability.

### 2. Add backend-provided, non-exportable CCA RAM

Files:

- `vmm_core/virt/src/generic.rs`
- `vmm_core/virt_kvm/src/arch/aarch64/mod.rs`
- `vmm_core/virt_kvm/src/memory.rs`
- `openvmm/membacking/src/memory_manager/mod.rs`
- `openvmm/openvmm_core/src/worker/dispatch.rs`

- [ ] Add backend-neutral RAM-backing request/result types without exposing
  `membacking` through `virt`.
- [ ] Add an optional prototype-partition backing factory.
- [ ] For in-place CCA only, create one mmap/init-shared guestmemfd per RAM
  backing request, normally per NUMA node.
- [ ] Return one fd to the memory manager and retain a partition-owned fd plus
  the GPA-to-offset map.
- [ ] Mark the backing non-exportable. Disable `GuestMemorySharing`,
  restart-memory extraction, vhost-user fd sharing, and all dynamic fd export.
- [ ] Reject hugetlb, private-anonymous, restore-provided, assigned-device,
  vhost-user, and other uncoordinated external-memory configurations.
- [ ] Keep a partition-owned fd until all VPs stop, all slots unbind, all
  mappings disappear, and every duplicate is closed or revoked. KVM does not
  retain the guestmemfd for userspace.
- [ ] Land this plumbing behind an inactive experimental mode.

### 3. Add revocable host-access leases

Current mappers expose lifetime-stable raw pointers, which is incompatible
with making arbitrary pages inaccessible.

- [ ] Introduce a range lease/epoch API covering all host access, DMA, GUP,
  remote mappings, and raw-pointer users.
- [ ] Make long-lived raw pointers unavailable for CCA in-place-capable RAM.
- [ ] Conversion blocks new leases and drains existing leases before changing
  attributes.
- [ ] Make every mapper and DMA/device consumer acknowledge hide/unmap
  success; do not discard errors.
- [ ] Keep mapping metadata until hide succeeds or rollback completes.
- [ ] Treat failed mapping/DMA rollback as partition-fatal.
- [ ] Audit whether guarded `trycopy` covers every guest-memory access, but do
  not rely on process-wide SIGBUS recovery for raw-pointer users.

### 4. Support partial-range visibility

- [ ] Split mappings at arbitrary page-aligned RIPAS boundaries.
- [ ] Preserve file offsets, priority, mapping type, NUMA/THP policy,
  permissions, remote state, leases, and DMA metadata.
- [ ] Prepare fragments and rollback records before updating any mapper.
- [ ] Track private/shared state per range and guestmemfd.
- [ ] Translate fd-relative `error_offset` back to GPA using the correct
  guestmemfd.
- [ ] Account for `EAGAIN` tearing down userspace PTEs before returning even
  though attributes remain unchanged; restore/refault mappings before retry or
  rollback.
- [ ] Treat each attribute ioctl as atomic, but handle cross-fd partial
  success transactionally or mark the partition fatal.

### 5. Change CCA launch finalization

The loader initially writes through the shared guestmemfd mmap. After
conversion, that mapping cannot be the Realm population source.

- [ ] Validate every import before changing attributes.
- [ ] Split imports across slots/backing files.
- [ ] Copy measured and unmeasured imports into page-aligned anonymous staging
  buffers while guestmemfd is shared.
- [ ] Convert all Realm RAM private through the lease coordinator.
- [ ] Populate from staging, never the private guestmemfd HVA.
- [ ] Preserve populate partial-progress semantics and add a no-progress guard.
- [ ] Free staging only after completion.
- [ ] Treat conversion/population failure as terminal.
- [ ] Prevent first `KVM_RUN` until privacy conversion and population finish.

### 6. Keep both CCA modes

- [ ] In in-place mode, remove CCA `MADV_DONTNEED` and hole punching because
  there is one backing.
- [ ] In two-backing fallback mode, retain both cleanup operations.
- [ ] Preserve SNP cleanup and VM-fd attributes unchanged.
- [ ] Route behavior by explicit backing mode, not architecture capability
  alone.

### 7. Gate unsupported lifecycle operations

- [ ] Reject in-place CCA restart/restore before VM/RAM creation.
- [ ] Reject save/export before pausing or serializing units.
- [ ] Define debugger and inspection behavior for private ranges.
- [ ] Do not enable the experimental mode until ABI, backing ownership,
  leases, mapping splits, staging, teardown, and rejection gates land together
  in a runnable configuration.

### Phase 2 validation

- [ ] Capability and creation-flag checks.
- [ ] Multiple NUMA guestmemfds and fd-relative GPA translation.
- [ ] Non-exportable backing and duplicate-fd lifetime.
- [ ] Lease drain, cancellation, and raw-pointer rejection.
- [ ] DMA/GUP reference failures and `EAGAIN/error_offset`.
- [ ] Mapping splits, PTE teardown on `EAGAIN`, and exact rollback metadata.
- [ ] CCA staging, segmentation, measured/unmeasured flags, and partial
  population.
- [ ] Repeated shared/private transitions at slot/backing boundaries.
- [ ] Teardown only after all references and fds are gone.
- [ ] Rejection of save, restore, assigned-device, vhost-user, hugetlb, and
  private-anonymous configurations.
- [ ] FVP CCA block/network smoke tests.
- [ ] SNP regression on a kernel without guestmemfd in-place support.

### Phase 2 commits

1. Low-level ABI and inactive mode plumbing.
2. Non-exportable backend-provided RAM.
3. Revocable leases and acknowledged DMA/mapping coordination.
4. Partial-range mapping support.
5. Staging and launch finalization.
6. Cleanup/lifecycle gates and explicit mode enablement.
7. Tests and FVP validation.

## Reviews

The two-phase roadmap received:

- two independent adversarial reviews;
- a dedicated Claude Opus 5 plan review;
- an Opus 5 code-verification pass on the Phase 1 attribute model; and
- a focused review of the intended v15 RIPAS userspace flow.

The reviews found the original Phase 1 attribute-ioctl premise incorrect.
This version incorporates the verified permanently-PRIVATE two-backing model,
RIPAS cleanup ordering, concrete VP quiescence/fatal handling, firmware-first
sequencing, non-exportable Phase 2 backing, revocable access leases, and
explicit Phase 2 opt-in/fallback behavior. The final Opus 5 review returned
**Minor revisions** and added backing-aware shared-memory purging,
snapshot/drain/revalidation ordering, Realm IPA probing, explicit SNP/CCA
private-state modes, bounded asynchronous VP draining, and pre-unbind purge
requirements. The Opus 5 closure review retained the fatal latch but correctly
demoted the full VP rendezvous to a stress-test-triggered contingency, selected
`MADV_REMOVE` to avoid unnecessary Phase 1 layering, preserved strict SNP
segmentation, and added Phase 1 DMA/external-consumer rejection.
