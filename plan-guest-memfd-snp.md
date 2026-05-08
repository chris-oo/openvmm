# Plan: KVM guest_memfd support for SNP

## Goal

Enable OpenVMM's KVM backend to build an SNP partition by adding guest_memfd-backed memory registration. The generic `PartitionMemoryMap::map_range` contract should continue to mean "add this GPA range to the VM"; for SNP, `virt_kvm` will interpret the supplied VA range as the shared backing and will create/manage guest_memfd as the private backing internally.

## Current state

- `IsolationType::Snp` is plumbed into KVM partition creation.
- KVM creates an SNP VM type with `KVM_X86_SNP_VM`.
- `/dev/sev` is opened and `KVM_SEV_INIT2` is issued.
- `KvmProtoPartition::build` still returns `SnpPrivateMemoryNotImplemented` before memory is registered.
- Existing KVM memslot code only uses `KVM_SET_USER_MEMORY_REGION` with a userspace VA backing.

## Proposed design

Add a KVM-internal memory backing mode:

```rust
enum KvmMemoryBackingMode {
    Userspace,
    GuestMemfd,
}
```

Derive this once from partition isolation during KVM proto-partition creation:

- `IsolationType::None` -> `Userspace`
- `IsolationType::Snp` -> `GuestMemfd`
- `IsolationType::Vbs` and `IsolationType::Tdx` remain unsupported for now

Store the selected mode on `KvmProtoPartition` and carry it into `KvmPartitionInner`. The memslot path should branch on `KvmMemoryBackingMode`, not directly on SNP isolation.

Guest_memfd backing must be applied only to guest RAM ranges. Device memory, framebuffer mappings, BARs, and other non-RAM regions should continue to use userspace-backed mappings even when the partition is in guest_memfd mode, because those ranges are intentionally host/device visible. Today, `PartitionMemoryMap::map_range` only receives `(VA, size, GPA, writable, executable)` and does not identify whether the range is RAM; that classification is in the memory manager/region manager.

For the first implementation, use a deliberately narrow RAM classifier inside `virt_kvm`: store the RAM ranges from the boot `MemoryLayout`/proto-partition configuration on the KVM partition and treat a mapping as guest_memfd-eligible only if the GPA range is fully contained in one of those RAM ranges. This is acceptable for the initial target of Linux VMs with enlightenment-oriented/non-legacy platform configuration and without PC legacy RAM visibility overlays. Configurations that depend on overlapping device mappings inside RAM are out of scope for SNP guest_memfd support unless they are proven legal and required.

## Implementation steps

### Completed so far

- Added `vm/kvm` wrappers for `KVM_SET_USER_MEMORY_REGION2`, `KVM_CREATE_GUEST_MEMFD`, and `KVM_SET_MEMORY_ATTRIBUTES`.
- Added private-memory capability checks for `KVM_CAP_USER_MEMORY2`, `KVM_CAP_GUEST_MEMFD`, and `KVM_CAP_MEMORY_ATTRIBUTES(KVM_MEMORY_ATTRIBUTE_PRIVATE)` on both `/dev/kvm` and the SNP VM fd.
- Added `KvmMemoryBackingMode` and wired `IsolationType::Snp` to guest_memfd-backed KVM memory while keeping `Vbs` and `Tdx` unsupported.
- Stored boot RAM ranges from `MemoryLayout`, including the VTL2 range, and added a temporary classifier that uses guest_memfd only for mappings fully contained in exactly one RAM range.
- Added classifier tests for RAM, non-RAM, partial-overlap, adjacent-range, and ambiguous-overlap cases.
- Added guest_memfd-backed RAM memslot registration with `KVM_SET_USER_MEMORY_REGION2`, private attribute setup with `KVM_SET_MEMORY_ATTRIBUTES`, fd lifetime tracking, and cleanup/rollback handling.
- Preserved userspace-backed mappings for ranges outside stored RAM ranges.
- Added OpenVMM-side gating that rejects SNP guest_memfd with PCAT, Hyper-V VGA, or i440BX host PCI bridge configurations.
- Narrowed the early SNP private-memory build blocker to an explicit `GuestMemfdLaunchNotImplemented` blocker before vCPU launch. SNP support is still not bootable.

### Remaining work

- Implement SNP launch start/update/finish and measurement, including copying the existing userspace guest-memory contents into private guest_memfd-backed pages during launch update.
- Add VMSA/protected CPU state handling before allowing vCPU launch.
- Wire SNP CPUID/secrets/page-state expectations for the first direct-boot Linux target.
- Add or factor additional guest_memfd bookkeeping tests where practical without SNP hardware.
- Validate on SNP-capable hardware through VM creation, guest_memfd RAM registration, private attributes, cleanup, and the next explicit SNP launch blocker.
- Follow up with runtime shared/private page conversion handling via `KVM_SET_MEMORY_ATTRIBUTES` and explicit GFN state tracking.

1. Keep SNP VM creation as the only private-memory VM type in this plan.
   - Continue using `KVM_X86_SNP_VM` for `IsolationType::Snp`.
   - In `virt_kvm`, introduce a KVM-internal VM creation/memory backing selection type, for example:

     ```rust
     enum KvmVmKind {
         Normal,
         Snp { sev: File },
     }
     ```

     `KvmVmKind::Snp` selects `KvmMemoryBackingMode::GuestMemfd` and performs `/dev/sev` setup and `KVM_SEV_INIT2`.
   - Derive `KvmVmKind::Normal` and `KvmVmKind::Snp` from `ProtoPartitionConfig::isolation` in production paths.
   - Do not route `IsolationType::Vbs` to any x86 private-memory VM type. `IsolationType::Vbs` should remain unsupported on KVM unless a real VBS implementation is added.

2. Add KVM API wrappers in `vm/kvm`.
   - Bind `KVM_SET_USER_MEMORY_REGION2`.
   - Bind `KVM_CREATE_GUEST_MEMFD`.
   - Bind `KVM_SET_MEMORY_ATTRIBUTES`.
   - Add or verify constants/structs for `KVM_MEM_GUEST_MEMFD`, `KVM_MEMORY_ATTRIBUTE_PRIVATE`, `GUEST_MEMFD_FLAG_*`, and `kvm_userspace_memory_region2`.
   - Add capability checks when creating SNP/private-memory partitions. Check capabilities on `/dev/kvm` before VM creation where that is the only available query point, and re-check on the VM fd after creating the SNP VM for capabilities whose result can be VM-type-sensitive.
     - `KVM_CAP_USER_MEMORY2` must be available.
     - `KVM_CAP_GUEST_MEMFD` must be available.
     - `KVM_CAP_MEMORY_ATTRIBUTES` must report support for `KVM_MEMORY_ATTRIBUTE_PRIVATE`, not merely return nonzero.
   - Surface missing support during SNP/private partition creation with typed internal errors instead of delaying failure until the first memory mapping ioctl. These may be wrapped in `anyhow::Error` at existing trait boundaries such as `PartitionMemoryMap::map_range`.
   - Expose `KVM_CREATE_GUEST_MEMFD` as a VM ioctl wrapper that returns an owned fd. The slot state must own that fd for at least as long as the KVM memslot references it.

3. Extend KVM memory state.
   - Keep current `host_addr` and `range` tracking.
   - Add optional guest_memfd state to each slot record so the fd remains alive for the memslot lifetime.
   - Start with one guest_memfd per mapped KVM slot/range. This is simpler than a global guest_memfd allocator and matches current slot-granularity bookkeeping.
   - Define slot reuse for guest_memfd mode explicitly:
     - Reusing a slot for the same shared VA is allowed only if the size is unchanged.
     - If the GPA changes but the size and shared VA are unchanged, recreate or update the KVM slot while preserving correct guest_memfd state.
     - If the size changes, reject it or clear and recreate the slot and guest_memfd; do not silently reuse stale guest_memfd metadata.
   - Keep guest_memfd slot state independent of SNP launch state so future KVM private-memory VM types can reuse it, but do not add another VM type in this milestone.

4. Update KVM `map_region`.
   - In `Userspace` mode, keep the existing `KVM_SET_USER_MEMORY_REGION` behavior unchanged.
   - Store the boot memory layout's RAM ranges on the KVM partition, including any VTL2 RAM range that is mapped as guest RAM.
   - Treat this guest_memfd backing mode as independent from `MemoryConfig.private_memory`, which controls OpenVMM's userspace memory-manager backing. SNP guest_memfd uses KVM's private-memory fd as the private backing while retaining the supplied userspace VA as the shared backing.
   - In `GuestMemfd` mode, classify a mapping as guest RAM only if the mapped GPA range is fully contained in a stored RAM range.
   - Add a prominent TODO next to this classifier explaining that overlapping device mappings inside RAM are out of scope for SNP guest_memfd unless a future requirement proves they are legal and needed.
   - In `GuestMemfd` mode for classified RAM ranges:
     - Create a guest_memfd sized to the mapped region.
     - Register the slot with `KVM_SET_USER_MEMORY_REGION2`.
     - Set `userspace_addr = data` from `map_range` for shared pages.
     - Set `guest_memfd` and `guest_memfd_offset = 0` for private pages.
     - Set `KVM_MEM_GUEST_MEMFD`, plus `KVM_MEM_READONLY` if requested.
     - Store the guest_memfd fd in the slot record.
   - In `GuestMemfd` mode for ranges outside the stored RAM ranges, continue using the userspace-only memslot path.
   - Gate or reject configurations known to rely on RAM/device overlays for the initial guest_memfd path, including PCAT, Hyper-V VGA, and i440BX RAM visibility control. The first supported target is fully enlightened Linux.
     - Enforce this in OpenVMM configuration/worker plumbing where `LoadMode`, chipset options, and the selected KVM mode are all visible, before building or running a guest_memfd-backed partition.
     - Reject at least `LoadMode::Pcat`, `cfg.chipset.with_hyperv_vga`, and `cfg.chipset.with_i440bx_host_pci_bridge` for SNP guest_memfd bring-up while the temporary classifier is in use.
     - Keep a defensive check in `virt_kvm` for partial RAM overlaps: in `GuestMemfd` mode, if a range overlaps a stored RAM range but is not fully contained in one RAM range, return a typed internal error instead of falling back to userspace.
   - Keep non-RAM direct mappings independent of guest_memfd and do not require device code to know about SNP.
   - Validate or document existing guarantees for page alignment of GPA, VA, size, guest_memfd size, and memory-attribute ranges. If the existing callers do not guarantee alignment, return a typed internal error before issuing KVM ioctls.

5. Mark guest_memfd RAM private initially for SNP.
   - After registering a guest_memfd-backed memslot, call `KVM_SET_MEMORY_ATTRIBUTES` over the mapped GPA range with `KVM_MEMORY_ATTRIBUTE_PRIVATE` for SNP.
   - This makes private faults resolve from guest_memfd instead of the shared VA backing.
   - Commit the `KvmMemoryRangeState` entry only after both slot registration and memory-attribute setup succeed.
   - If `KVM_SET_USER_MEMORY_REGION2` succeeds but `KVM_SET_MEMORY_ATTRIBUTES` fails, clear the KVM slot before returning the error and drop the newly-created guest_memfd. This avoids leaving an untracked registered memslot in KVM.
   - Treat memory attributes as GPA state, not slot/fd state. Clear `KVM_MEMORY_ATTRIBUTE_PRIVATE` when unmapping a guest_memfd-backed RAM range, moving a slot to a different GPA, rolling back a partially successful map, or remapping a range through the userspace-only path. Do this before or as part of slot teardown so stale private attributes do not survive slot deletion/reuse.

6. Preserve existing unmap behavior.
   - Clearing a slot still sets memory size to zero.
   - For guest_memfd-backed slots, prefer clearing with the same `KVM_SET_USER_MEMORY_REGION2` wrapper unless verified that legacy `KVM_SET_USER_MEMORY_REGION` deletion is valid for `KVM_MEM_GUEST_MEMFD` slots.
   - Clear private memory attributes for the GPA range before dropping guest_memfd slot state.
   - Dropping the slot record drops the guest_memfd fd.
   - Keep the current invariant that unmaps must fully contain mapped ranges; do not add subrange splitting in the first change.

7. Remove the build-time SNP blocker.
   - Once SNP memslots can be registered, narrow `SnpPrivateMemoryNotImplemented` rather than removing it wholesale.
   - Allow the code to reach the next explicit SNP blocker only for the narrow memslot/attribute validation path.
   - Keep later SNP launch, measurement, and page-state protocol gaps as explicit errors if they are still unimplemented.
   - Do not present this milestone as bootable SNP support. Expected remaining SNP work includes launch start/update/finish, page measurement/population, VMSA/protected CPU state, secrets/CPUID handling, and runtime shared/private conversion handling.

8. Defer runtime page conversion handling unless needed for the first boot milestone.
   - A later change should handle guest shared/private transitions by updating `KVM_MEMORY_ATTRIBUTE_PRIVATE`.
   - That follow-up likely needs explicit GFN state tracking and wiring to the SNP GHCB/MSR/protocol path.

9. Preserve non-RAM mapping behavior.
   - Ensure guest_memfd mode does not convert every `map_range` call to guest_memfd.
   - Add tests or assertions covering that RAM ranges use guest_memfd while ranges outside RAM still use userspace-backed memslots.
   - Document that overlapping device mappings inside RAM are intentionally unsupported for SNP guest_memfd unless a future requirement proves they are legal and needed.

## Testing plan

### Tests to write as part of this milestone

Write tests for the logic that can be validated without SNP hardware. The implementation should be factored so these are normal Rust tests, not only manual checks against `/dev/kvm`.

- Add `virt_kvm` unit tests for the temporary RAM classifier:
  - a range fully contained in one RAM range is classified as guest_memfd RAM;
  - a range outside all RAM ranges uses the userspace memslot path;
  - a range that partially overlaps RAM returns an error instead of falling back to userspace or guest_memfd;
  - adjacent RAM ranges do not accidentally combine into one guest_memfd-eligible range unless the implementation explicitly supports that.
- Add `virt_kvm` unit tests for guest_memfd slot bookkeeping by factoring memslot decision/update logic behind a testable helper or fake KVM backend:
  - guest_memfd fd state is retained for the lifetime of a guest_memfd-backed slot;
  - a failed memory-attribute update rolls back the registered slot and drops the new guest_memfd state;
  - unmap clears private attributes before dropping guest_memfd slot state;
  - userspace-only mappings outside RAM do not create guest_memfd state;
  - slot reuse with size changes is rejected or clear-and-recreate, not stale metadata reuse.
- Add `vm/kvm` tests for UAPI wrapper construction where possible:
  - `kvm_userspace_memory_region2` is populated with `KVM_MEM_GUEST_MEMFD`, `userspace_addr`, `guest_memfd`, and `guest_memfd_offset` as expected;
  - memory-attribute requests use page-aligned GPA/size and `KVM_MEMORY_ATTRIBUTE_PRIVATE`;
  - capability parsing treats `KVM_CAP_MEMORY_ATTRIBUTES` as a bitmask and requires `KVM_MEMORY_ATTRIBUTE_PRIVATE`.

### Manual/SNP-hardware validation

The first implementation should also include a documented manual validation path for SNP-capable hosts. This is not expected to run in ordinary CI.

Run a minimal Linux direct-boot SNP guest with KVM, no firmware, no Hyper-V enlightenments, no VTL2, no VMBus devices, no disks, and no legacy chipset devices:

```bash
cargo run -p openvmm -- \
  --hypervisor kvm \
  --isolation snp \
  --kernel <path-to-vmlinux> \
  --initrd <path-to-initrd> \
  -m 1GB \
  -p 1 \
  -c "console=ttyS0 panic=-1"
```

The kernel must be an uncompressed Linux kernel image (`vmlinux`, not `bzImage`). If the repo's `.cargo/config.toml` sample kernel/initrd environment variables are usable on the SNP host, the `--kernel` and `--initrd` arguments can be omitted.

- Create an SNP KVM VM and successfully run through `KVM_SEV_INIT2`.
- Register guest RAM as guest_memfd-backed private memory.
- Stop before vCPU execution at the next explicit SNP launch/measurement blocker.
- Verify cleanup by clearing the guestmemfd-backed memslot, clearing private memory attributes, and dropping the fd state.
- Run targeted Rust validation for modified crates:
  - `cargo check -p kvm -p virt_kvm`
  - `cargo clippy --all-targets -p kvm -p virt_kvm`
  - `cargo doc --no-deps -p kvm -p virt_kvm`
  - `cargo nextest run --profile agent -p kvm -p virt_kvm` if tests exist and nextest is available
  - `cargo xtask fmt --fix`

### Later boot tests that require SNP hardware

SNP-specific VM creation and execution still requires an SNP-capable AMD host, firmware, kernel, KVM support, and `/dev/sev`.

- Eventually boot an SNP-capable guest and test page sharing/private transitions.
- Validate real RMP-backed private memory behavior, SNP launch start/update/finish, VMSA handling, measurement, secrets/CPUID handling, and GHCB-mediated runtime conversions.

## Milestones

1. [done] Add SNP-only private-memory VM creation and mode selection plumbing.
2. [done] Add wrappers and capability checks.
3. [done] Add `KvmMemoryBackingMode`, guestmemfd slot state, and the temporary `MemoryLayout` RAM-range classifier.
4. [done] Add configuration gating for the temporary classifier and legacy overlay rejection.
5. [done] Register guestmemfd RAM memslots with `KVM_SET_USER_MEMORY_REGION2`.
6. [done] Mark initial private RAM with `KVM_SET_MEMORY_ATTRIBUTES`.
7. [done] Remove or narrow the current SNP private-memory build blocker.
8. [partial] Add generic guest_memfd wrapper/bookkeeping tests where the kernel and factoring support them.
9. [next] Add SNP-hardware smoke coverage for SNP VM creation, guestmemfd memslots, attributes, cleanup, and the current launch blocker.
10. [next] Implement the SNP launch flow for bootable SNP guests: launch start, launch update from the existing userspace guest-memory contents into private guest_memfd pages, measurement, and launch finish.
11. [todo] Follow up with SNP page-state transition handling.

## Readiness assessment

The initial KVM guest_memfd plumbing milestone is implemented through RAM memslot registration, private memory attributes, cleanup/rollback handling, and hardware-free classifier tests. It is not ready for end-to-end SNP boot: the current expected stop point is `GuestMemfdLaunchNotImplemented` before vCPU launch. It intentionally does not support PCAT, Hyper-V VGA, i440BX/PAM/VGA behavior, or other dynamic overlapping device/RAM mappings for SNP guest_memfd.
