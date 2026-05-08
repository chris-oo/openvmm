# Plan: KVM SNP loader and launch update support

## Goal

Make OpenVMM's KVM SNP path boot the minimal x86_64 Linux direct-boot scenario by adding the missing loader metadata, SNP boot protocol pages, KVM SNP launch start/update/finish, and protected initial CPU state handling.

The current expected stop point is `KvmError::GuestMemfdLaunchNotImplemented` in `vmm_core/virt_kvm/src/arch/x86_64/mod.rs`. This plan replaces that blocker only for the intentionally narrow direct-boot case.

## Current state

- `plan-guest-memfd-snp.md` tracks the overall SNP guest_memfd effort.
- KVM SNP VM creation is implemented with `KVM_X86_SNP_VM`.
- `/dev/sev` is opened and `KVM_SEV_INIT2` is issued.
- KVM guest RAM is registered with guest_memfd-backed memslots via `KVM_SET_USER_MEMORY_REGION2`.
- Guest RAM is initially marked private with `KVM_SET_MEMORY_ATTRIBUTES`.
- Non-RAM mappings stay userspace-backed through the temporary RAM-range classifier in `vmm_core/virt_kvm/src/lib.rs`.
- `vm/kvm/src/lib.rs` has wrappers for `KVM_SEV_INIT2`, `KVM_CREATE_GUEST_MEMFD`, `KVM_SET_USER_MEMORY_REGION2`, and `KVM_SET_MEMORY_ATTRIBUTES`, but not SNP launch commands.
- Loader imports use `BootPageAcceptance` in `vm/loader/src/importer.rs`; SNP-specific accepted page types already exist for IGVM/UEFI/paravisor paths.
- Direct Linux loading currently returns only `Vec<X86Register>` through `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs`.
- The generic VM loader path already tracks accepted ranges, but only as `Vec<(MemoryRange, PageVisibility)>`, not as SNP launch page types with source tags.

## Minimal supported scenario

Support only:

- x86_64 KVM
- `--isolation snp`
- Linux direct boot with uncompressed `vmlinux`
- optional initrd
- no firmware
- no Hyper-V enlightenments
- no VTL2
- no VMBus devices
- no disks
- no PCAT/i440BX/Hyper-V VGA legacy chipset overlays
- one VP
- a serial console path suitable for `console=ttyS0`

The validation console still needs a 16550 COM port, so "no legacy chipset devices" means no legacy chipset memory-overlay dependencies, not "no serial device".

Out of scope for this milestone: SNP IGVM launch, UEFI SNP boot, OpenHCL/VTL2, multi-VP/AP startup, runtime shared/private page transitions, disk/network devices, and production attestation policy UX.

## Proposed shape

Add a runtime initial-load result that preserves what the loader deposited into `GuestMemory`:

- imported page ranges with `BootPageAcceptance` and a debug tag;
- x86 initial register imports;
- allocated SNP boot protocol pages, including Linux SNP CC blob, CPUID page, and secrets page;
- VMSA/protected CPU state source data.

The KVM SNP partition then performs launch before any vCPU runs:

1. `KVM_SEV_SNP_LAUNCH_START`
2. launch-update measured/unmeasured/private pages by reading final bytes from userspace `GuestMemory`/memslot VA and copying them into guest_memfd-private memory through KVM
3. launch-update SNP boot protocol pages with the correct SNP page types
4. use `KVM_SEV_SNP_PAGE_TYPE_ZERO` for all-zero private pages, especially for the first prototype that imports all guest RAM
5. establish and measure the BSP VMSA using the exact KVM SNP UAPI sequence required by this repo's `kvm_bindings`
6. `KVM_SEV_SNP_LAUNCH_FINISH`
7. allow vCPU bind/run only after launch finish succeeds

Do not use the original import byte slices as launch source data. Some loader imports are partial-page writes with zero-fill semantics; the source of truth at launch time is the final userspace backing that OpenVMM loaded.

For the first prototype, import the entire guest RAM aperture into the SNP launch context. Pages that were never explicitly populated by the loader, or whose final contents are all zero, should be populated with the KVM SNP zero-page type instead of allocating/copying a temporary zero buffer. The Linux KVM SNP UAPI in `~/ai/eevee/linux` documents `KVM_SEV_SNP_PAGE_TYPE_ZERO`; for zero-page launch updates, `uaddr` is ignored and KVM updates the request fields until `len == 0`.

## Implementation steps

### 1. Pin down the active KVM SNP UAPI

Touch points:

- `vm/kvm/src/lib.rs`
- root `Cargo.toml` / `kvm-bindings` version only if the current bindings lack required definitions

Work:

- Verify exact struct and constant names available for:
  - `KVM_SEV_SNP_LAUNCH_START`
  - `KVM_SEV_SNP_LAUNCH_UPDATE`
  - `KVM_SEV_SNP_LAUNCH_FINISH`
  - SNP page-type constants for normal, zero, unmeasured, secrets, CPUID, and VMSA pages
  - any separate vCPU/VMSA update command
  - launch finish fields and whether measurement is returned
- Add typed wrappers following the existing `Partition::sev_snp_init` `KVM_MEMORY_ENCRYPT_OP` pattern.
- Add explicit support for `KVM_SEV_SNP_PAGE_TYPE_ZERO`. In the Linux KVM UAPI, `struct kvm_sev_snp_launch_update.uaddr` is ignored when `type == KVM_SEV_SNP_PAGE_TYPE_ZERO`, and callers must continue issuing launch updates until KVM reports the whole range consumed.
- Add explicit capability or ioctl availability checks for SNP launch support in addition to the existing guest_memfd/private-memory checks.
- Do not invent measurement retrieval if the active kernel UAPI does not return it. Log/store the measurement only if the UAPI exposes it.

### 2. Introduce an initial launch metadata model

Touch points:

- `vm/loader/src/importer.rs`
- `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs`
- `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs`
- the `vm_loader::Loader` implementation used by OpenVMM

Work:

- Add an `InitialLoadInfo`/`LaunchInfo`-style type that can carry:
  - initial VP registers;
  - page ranges with `BootPageAcceptance`;
  - debug tags;
  - SNP-specific allocated page GPAs;
  - accepted/private/shared visibility where existing callers need it.
- Change direct Linux load to return this richer type instead of only `Vec<X86Register>`.
- Preserve existing non-SNP behavior by deriving the existing register vector and accepted-range data from the richer type where needed.
- Avoid panics for loader metadata conflicts. Existing code has panic-prone duplicate-register behavior; the SNP path should return typed errors for duplicate/conflicting initial state.
- Keep overlap handling conservative:
  - reject conflicting overlaps;
  - allow identical acceptance only when the final `GuestMemory` contents remain unambiguous;
  - coalesce adjacent ranges only as an optimization after validation.

### 3. Add Linux direct-boot SNP boot protocol pages

Touch points:

- `vm/loader/src/linux.rs`
- `vm/loader/src/cpuid.rs`
- `vm/loader/loader_defs/src/linux.rs`
- `openhcl/openhcl_boot/src/main.rs` as the in-repo reference implementation
- `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs`

Work:

- Add SNP-aware direct Linux loading when `IsolationType::Snp` is selected.
- Allocate and import the pages Linux SNP boot expects:
  - a secrets page imported as `BootPageAcceptance::SecretsPage`;
  - a CPUID page imported as `BootPageAcceptance::CpuidPage`;
  - an extended-state CPUID page only if the chosen CPUID table format/protocol requires it;
  - any unmeasured/shared page required by the Linux SEV/SNP boot protocol.
- Define the CPUID-page contract explicitly: the loader is responsible only for allocating/reserving the CPUID page GPA, setting `cc_blob_sev_info.cpuid_phys` and `cpuid_len`, linking the CC blob into Linux boot metadata, and recording that GPA in launch metadata. The loader should not require the caller to provide CPUID page contents and should not try to synthesize final CPUID values itself.
- The KVM SNP launch path owns the CPUID page contents. Final values depend on the hypervisor/KVM CPU model and per-vCPU CPUID state, so `virt_kvm` should populate the page immediately before `KVM_SEV_SNP_LAUNCH_UPDATE` with page type `KVM_SEV_SNP_PAGE_TYPE_CPUID`.
- Build the CPUID page in the Linux `struct snp_cpuid_table` format from `arch/x86/include/asm/sev.h`: `count` plus up to 64 `struct snp_cpuid_fn` entries containing input `eax/ecx/xcr0/xss` and output `eax/ebx/ecx/edx`.
- Follow QEMU's contract in `target/i386/sev.c`: QEMU queries `KVM_GET_CPUID2` from the first vCPU, converts each `struct kvm_cpuid_entry2` into an SNP CPUID table entry, applies the XSAVE `0xd` leaf adjustment for initial `XCR0=1` (`ebx = 0x240`, `xcr0_in = 1`, `xss_in = 0` for subleaves 0 and 1), writes that table into the CPUID page HVA, and then launch-updates that page as `KVM_SEV_SNP_PAGE_TYPE_CPUID`.
- In OpenVMM, prefer using the same final CPUID data already prepared for KVM vCPU setup in `virt_kvm` rather than static `vm/loader/src/cpuid.rs` UEFI/paravisor leaf lists. Static leaf lists can remain useful for firmware/paravisor paths, but direct Linux SNP should get the actual KVM-visible CPUID values that the guest will observe.
- Reuse the in-repo Linux boot protocol definitions in `vm/loader/loader_defs/src/linux.rs` rather than duplicating layouts:
  - `setup_data`;
  - `SETUP_CC_BLOB`;
  - `cc_blob_sev_info`;
  - `CC_BLOB_SEV_INFO_MAGIC`;
  - `cc_setup_data`.
- Follow `openhcl/openhcl_boot/src/main.rs` as the concrete reference: `build_cc_blob_sev_info` fills `cc_blob_sev_info`, allocates a `cc_setup_data`, sets `header.ty = SETUP_CC_BLOB`, sets `cc_blob_address`, and chains it through the setup-data list.
- Add a Linux SEV/SNP CC blob using the kernel/OpenVMM `cc_blob_sev_info` layout:
  - `magic = CC_BLOB_SEV_HDR_MAGIC` (`0x45444d41`);
  - `version = 0`, matching OpenHCL boot's enlightened Linux setup unless the target kernel requires a different version;
  - `secrets_phys` and `secrets_len = PAGE_SIZE`;
  - `cpuid_phys` and `cpuid_len >= PAGE_SIZE`.
- Make the CC blob discoverable through Linux direct boot. Linux accepts either `boot_params.cc_blob_address` or a `SETUP_CC_BLOB` setup-data entry. For direct boot, add a `SETUP_CC_BLOB` node to the zero page `hdr.setup_data` chain whose payload is the 32-bit `cc_blob_address`, matching `struct cc_setup_data` in `arch/x86/boot/startup/sev-shared.c`. If the loaded kernel path uses the compressed boot protocol and can consume `boot_params.cc_blob_address`, set that field too.
- Validate the exact zero-page/setup-data placement against Linux's discovery paths in `arch/x86/boot/startup/sev-startup.c`, `arch/x86/boot/startup/sev-shared.c`, and `arch/x86/boot/compressed/sev.c`.
- Keep the minimal serial command line explicit, e.g. `console=ttyS0 earlyprintk=serial earlycon panic=-1`, for hardware bring-up.
- Add typed rejection if the loader mode cannot provide the SNP boot protocol metadata.

### 4. Map loader acceptance to SNP launch update page types

Touch points:

- a new helper such as `vmm_core/virt_kvm/src/arch/x86_64/snp.rs`
- unit tests in `virt_kvm`

Initial mapping:

| `BootPageAcceptance` | SNP launch behavior |
| --- | --- |
| `Exclusive` | private normal measured page, or zero page when importing an all-zero page as part of whole-RAM launch population |
| `ExclusiveUnmeasured` | private unmeasured page, data copied but not measured |
| `VpContext` | VMSA/protected CPU state page if the active KVM UAPI uses launch update for VMSA |
| `SecretsPage` | SNP secrets page |
| `CpuidPage` | SNP CPUID page |
| `CpuidExtendedStatePage` | CPUID extended-state page if supported; otherwise reject clearly |
| `Shared` | do not launch-update as private; clear private attributes or reject for this milestone |
| `ErrorPage` | reject for this milestone |

Work:

- Implement a pure mapping helper with tests for every enum variant.
- For the minimal direct Linux path, require every launch-updated page to be fully contained in one guest_memfd-backed RAM slot.
- Add a zero-page classifier that can split launch ranges into non-zero subranges and all-zero subranges. All-zero private pages should use `KVM_SEV_SNP_PAGE_TYPE_ZERO`, with no source VA requirement, so the first whole-RAM prototype does not need to materialize zero buffers for untouched memory.
- Return typed errors for unsupported acceptances instead of falling back to userspace memory or panicking.

### 5. Implement launch update from final userspace memory

Touch points:

- `vmm_core/virt_kvm/src/lib.rs`
- `vmm_core/virt_kvm/src/arch/x86_64/mod.rs`
- new x86 SNP launch helper module

Work:

- Add a helper to resolve a GPA page range to:
  - the containing `KvmMemoryRange`;
  - `host_addr + (gpa - slot.range.start())`;
  - page count/byte length;
  - whether guest_memfd state and private attributes are present.
- Validate page alignment, range containment, guest_memfd backing, and private attributes before issuing launch update.
- Coalesce adjacent ranges only when GPA, host VA, and SNP page type are contiguous.
- Add a `virt_kvm` helper that takes the final BSP `kvm_cpuid_entry2` entries, converts them into Linux's SNP `snp_cpuid_table` format, writes the table into the loader-recorded CPUID page GPA, and launch-updates that page as `KVM_SEV_SNP_PAGE_TYPE_CPUID`. This helper should apply the same XSAVE leaf adjustment QEMU uses for initial `XCR0=1`.
- For the first prototype, iterate every guest RAM range, not only explicitly imported loader ranges. Use loader metadata to choose special page types and measurement expectations, but populate otherwise-unmentioned RAM as zero pages.
- For zero-page launch updates, pass the GPA/length and `KVM_SEV_SNP_PAGE_TYPE_ZERO`; do not require or pass a meaningful source userspace address.
- Treat launch update failure as fatal to the partition launch. Record failed state and keep vCPU bind/run blocked.
- Log launch-update page counts by page type and useful GPAs for hardware debugging.

### 6. Establish BSP VMSA/protected CPU state

Touch points:

- `vmm_core/virt_kvm/src/arch/x86_64/mod.rs`
- new `virt_kvm` SNP helper module
- possibly reusable code factored from `vm/loader/igvmfilegen/src/vp_context_builder/snp.rs`

Work:

- Verify the exact KVM SNP VMSA sequence first. Do not assume VMSA is always just another `LAUNCH_UPDATE` page.
- Convert the direct Linux initial register state into the protected CPU state expected by KVM:
  - CR0/CR3/CR4;
  - EFER;
  - RIP;
  - RSI/zero-page pointer;
  - RSP/RFLAGS defaults;
  - GDT/IDT and segment state;
  - PAT and required MSRs;
  - APIC ID / vCPU identity.
- Prefer factoring existing SNP VMSA construction logic from `igvmfilegen` if it can be moved without pulling generator-only dependencies into runtime code.
- Ensure the later `KvmProcessorBinder::bind` path does not overwrite measured SNP state with the normal non-confidential reset path.
- Keep the first implementation one-VP-only unless the same work naturally supports AP VMSAs safely.

### 7. Add launch state and sequencing

Touch points:

- `vmm_core/virt_kvm/src/lib.rs`
- `vmm_core/virt_kvm/src/arch/x86_64/mod.rs`
- the OpenVMM worker path that loads memory before vCPU run
- possibly a generic optional trait in `virt`

Work:

- Add explicit launch state such as `NotStarted`, `Started`, `Finished`, and `Failed`.
- Invoke the SNP launch hook after memory is mapped and loader writes are complete, after any required vCPU state setup, and before the first vCPU run.
- Prefer a generic optional launch/finalize trait over downcasting to KVM-specific types in generic worker code.
- Replace `GuestMemfdLaunchNotImplemented` with narrower errors:
  - launch not finished;
  - unsupported acceptance;
  - unsupported loader mode;
  - missing Linux SNP boot metadata;
  - unsupported VP count;
  - VMSA setup unsupported by current KVM UAPI.
- Make repeated launch calls explicitly no-op after `Finished` or an error, but never allow running after `Failed`.

### 8. Preserve and extend temporary gating

Touch points:

- existing SNP guest_memfd gating in OpenVMM configuration/worker plumbing

Work:

- Continue rejecting PCAT, Hyper-V VGA, i440BX/PAM dependencies, firmware, VTL2, VMBus, disks, Hyper-V enlightenments, and unsupported loader modes.
- Add temporary rejection for more than one VP until AP VMSA launch is implemented and tested.
- Allow the 16550 serial device required for `ttyS0` validation.
- Keep runtime shared/private page conversion out of scope, but fail with a clear error if the minimal Linux setup requires a shared RAM page before that support exists.

### 9. Update tracking docs as implementation lands

Touch points:

- `plan-guest-memfd-snp.md`
- this plan file

Work:

- Mark launch start/update/finish work in progress/done as changes land.

## Hardware-free test plan

Add targeted tests before relying on SNP hardware:

- loader metadata recording, conflict rejection, and duplicate-register error tests;
- direct Linux SNP CC blob / `setup_data` chaining tests;
- CPUID page generation tests;
- secrets/CPUID/extended-state page placement tests;
- `BootPageAcceptance` to SNP page-type mapping tests;
- whole-RAM launch population tests that classify untouched pages as zero pages;
- zero-page range splitting/coalescing tests, including mixed zero/non-zero pages inside one RAM range;
- KVM SNP launch wrapper construction tests where possible without `/dev/kvm`;
- GPA-to-userspace-VA resolution tests with fake slot state;
- launch update coalescing and boundary rejection tests;
- VMSA/protected state generation tests from the direct Linux register set;
- bind-before-launch-finish rejection and bind-after-finish success tests using mocked state;
- configuration gating tests for VP count, firmware/PCAT/VTL2/VMBus/disks/enlightenments, and allowed COM1 serial.

Run crate-scoped validation for affected packages:

```bash
cargo check -p kvm -p virt_kvm -p loader -p vm_loader -p openvmm_core
cargo clippy --all-targets -p kvm -p virt_kvm -p loader -p vm_loader -p openvmm_core
cargo doc --no-deps -p kvm -p virt_kvm -p loader -p vm_loader -p openvmm_core
cargo nextest run --profile agent -p kvm -p virt_kvm -p loader -p vm_loader -p openvmm_core
cargo xtask fmt --fix
```

Use `jj status` and `jj diff` for VCS inspection in this repo.

## SNP hardware launch validation

Prerequisites:

- AMD SNP-capable host with SNP enabled in firmware;
- host kernel and KVM with SNP guest_memfd launch UAPI support matching the repo bindings;
- accessible `/dev/kvm` and `/dev/sev`;
- guest Linux kernel configured for SEV-SNP guests;
- uncompressed `vmlinux`;
- initrd that can reach a serial shell or at least print early boot logs.

Suggested minimal command:

```bash
cargo run -p openvmm -- \
  --hypervisor kvm \
  --isolation snp \
  --kernel <path-to-vmlinux> \
  --initrd <path-to-initrd> \
  -m 1GB \
  -p 1 \
  -c "console=ttyS0 earlyprintk=serial earlycon panic=-1"
```

Expected validation checkpoints:

1. SNP VM type creation succeeds.
2. `/dev/sev` opens successfully.
3. `KVM_SEV_INIT2` succeeds.
4. guest RAM is registered as guest_memfd-backed memory.
5. private memory attributes are set for RAM.
6. Linux SNP CC blob, CPUID page, secrets page, and zero-page links are logged with expected GPAs.
7. `KVM_SEV_SNP_LAUNCH_START` succeeds.
8. normal measured pages are launch-updated from final userspace `GuestMemory`.
9. untouched/all-zero RAM pages are launch-updated with `KVM_SEV_SNP_PAGE_TYPE_ZERO`.
10. CPUID and secrets pages are launch-updated with the correct SNP page types.
11. BSP VMSA/protected state setup succeeds and is measured according to the active KVM UAPI.
12. `KVM_SEV_SNP_LAUNCH_FINISH` succeeds.
13. launch measurement is logged if the UAPI exposes it.
14. `GuestMemfdLaunchNotImplemented` is no longer reached for this minimal case.
15. the vCPU enters guest code.
16. Linux prints to the serial console, or failure is diagnosable from serial/KVM/SEV logs.
17. VM exit cleanup clears memslots/attributes and drops guest_memfd state.

If the guest does not reach serial output, collect:

- OpenVMM SNP launch logs including page counts, page types, GPAs, and measurement if available;
- KVM/SEV firmware error codes from the failed ioctl;
- host `dmesg` KVM/SNP lines;
- serial output up to the hang/fault;
- the exact kernel config and command line.

## Risks and open questions

- The exact VMSA setup path is kernel-UAPI-sensitive and must be verified before implementation.
- Linux direct boot probably needs SNP CC blob/secrets/CPUID protocol plumbing; launching only `Exclusive` pages is unlikely to boot.
- Some existing SNP CPUID helper lists are UEFI/paravisor-oriented; direct Linux may need a separate list.
- If Linux needs early shared pages or GHCB-mediated transitions before useful serial output, runtime page-state conversion may need to move into this milestone.
- Factoring VMSA generation from `igvmfilegen` may require moving shared structures into a runtime-safe crate to avoid generator-only dependencies.

## Follow-up work after this milestone

After the minimal SNP direct-boot launch works, the next major milestone is runtime SNP page-state handling. This launch milestone populates the initial private guest memory and starts the BSP, but it does not complete SNP support for long-running I/O-heavy workloads.

Follow-up work should include:

- tracking per-GFN private/shared state in `virt_kvm`;
- handling GHCB/MSR page-state-change requests;
- updating KVM memory attributes with `KVM_SET_MEMORY_ATTRIBUTES`;
- defining copy/zero semantics for private-to-shared and shared-to-private transitions;
- supporting guest I/O paths that require shared pages;
- extending hardware validation from "reaches early serial output" to "boots far enough to exercise devices and runtime sharing."

## Review

Verdict after review: Needs rework in the original draft; this saved version incorporates the required revisions.

Key review-driven changes:

- Added Linux SNP boot protocol work for CC blob, CPUID page, secrets page, and zero-page `setup_data` chaining.
- Made VMSA handling UAPI-first instead of assuming VMSA is always a normal launch-update page.
- Tightened launch sequencing around memory load, required vCPU/protected state setup, launch finish, and first run.
- Replaced a generic "record imports" idea with an explicit `InitialLoadInfo`/`LaunchInfo` path through direct Linux and VM loader code.
- Added tests for affected `loader`, `vm_loader`, and `openvmm_core` crates, not just `kvm` and `virt_kvm`.
- Clarified that COM1 serial is allowed for validation even while legacy chipset overlays remain out of scope.
