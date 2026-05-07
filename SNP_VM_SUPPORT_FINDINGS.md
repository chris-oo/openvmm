# SEV-SNP VM support findings

This note summarizes how upstream Linux/KVM and upstream QEMU support AMD
SEV-SNP guests, and what that implies for adding SNP VM support to OpenVMM.

## Executive summary

SNP support is not mainly a loader-only change. The launch/load path is central,
but KVM/QEMU support also requires:

- a KVM SNP VM type and SEV initialization flow;
- guest-private memory backing with `guest_memfd`;
- private/shared page attribute tracking and conversion;
- SNP launch update page typing for normal, zero, unmeasured, secrets, CPUID,
  and VMSA pages;
- CPUID page construction and validation;
- vCPU protected-state/VMSA measurement at launch finish;
- runtime handling for guest private/shared conversion requests;
- attestation certificate handling; and
- policy/device restrictions such as no SMM and no conventional migration/debug
  flows.

For OpenVMM, the largest gaps appear to be in the Linux/KVM backend and the
generic isolation/page-visibility abstractions, with loader changes needed to
carry SNP-specific page semantics rather than flattening everything into
shared/exclusive.

## Upstream Linux/KVM model

### VM type and initialization

KVM exposes SNP as a distinct x86 VM type, `KVM_X86_SNP_VM`, and advertises it
through `KVM_CAP_VM_TYPES` when SNP is enabled. In `sev_vm_init`, SNP VMs set
`has_private_mem = true`, protected register state, and defer pre-faulting until
after launch finish. In CPU capability setup, KVM sets `X86_FEATURE_SEV_SNP` and
adds `BIT(KVM_X86_SNP_VM)` to supported VM types.

Evidence:

- `~/ai/eevee/linux/arch/x86/include/uapi/asm/kvm.h:972` defines
  `KVM_X86_SNP_VM`.
- `~/ai/eevee/linux/arch/x86/kvm/svm/sev.c:2935-2955` initializes SNP VM state.
- `~/ai/eevee/linux/arch/x86/kvm/svm/sev.c:3014-3027` advertises SEV, SEV-ES,
  and SNP VM types.
- `~/ai/eevee/linux/tools/testing/selftests/kvm/lib/x86/sev.c:74-80` shows the
  selftest flow invoking `KVM_SEV_INIT2` for `KVM_X86_SNP_VM`.

### SNP launch ioctls

SNP launch is done through `KVM_MEMORY_ENCRYPT_OP` commands:

1. `KVM_SEV_INIT2`
2. `KVM_SEV_SNP_LAUNCH_START`
3. repeated `KVM_SEV_SNP_LAUNCH_UPDATE`
4. `KVM_SEV_SNP_LAUNCH_FINISH`

Once an instance is initialized as SNP, KVM rejects pre-SNP SEV commands for
that VM.

Evidence:

- `~/ai/eevee/linux/arch/x86/include/uapi/asm/kvm.h:741-748` declares
  `KVM_SEV_INIT2` and SNP-specific commands.
- `~/ai/eevee/linux/arch/x86/include/uapi/asm/kvm.h:877-918` defines
  `kvm_sev_snp_launch_start`, page type constants,
  `kvm_sev_snp_launch_update`, and `kvm_sev_snp_launch_finish`.
- `~/ai/eevee/linux/Documentation/virt/kvm/x86/amd-memory-encryption.rst:469-572`
  documents launch start/update/finish.
- `~/ai/eevee/linux/arch/x86/kvm/svm/sev.c:2655-2660` restricts SNP VMs to
  SNP-specific commands.
- `~/ai/eevee/linux/arch/x86/kvm/svm/sev.c:2724-2734` dispatches the SNP launch
  commands and certificate-enablement command.

### Private memory is mandatory for SNP launch update

Before `KVM_SEV_SNP_LAUNCH_UPDATE`, userspace must mark the GPA range private
with `KVM_SET_MEMORY_ATTRIBUTES` and the `KVM_MEMORY_ATTRIBUTE_PRIVATE` bit.
The KVM implementation additionally verifies that the backing memslot has
`guest_memfd`, that the GFN has the private attribute, and that the PFN has not
already been assigned private in the RMP.

Evidence:

- `~/ai/eevee/linux/include/uapi/linux/kvm.h:1642-1651` defines
  `KVM_SET_MEMORY_ATTRIBUTES` and `KVM_MEMORY_ATTRIBUTE_PRIVATE`.
- `~/ai/eevee/linux/Documentation/virt/kvm/api.rst:6364-6398` documents the
  memory attributes API and notes userspace must track page state.
- `~/ai/eevee/linux/Documentation/virt/kvm/x86/amd-memory-encryption.rst:493-506`
  states that SNP launch-update ranges must be marked private in advance.
- `~/ai/eevee/linux/arch/x86/kvm/svm/sev.c:2410-2489` implements
  `snp_launch_update`; comments at `2443-2451` list the private memslot,
  private attribute, and RMP preconditions.

### Page types and VMSA measurement

SNP launch update takes page types:

- `NORMAL`
- `ZERO`
- `UNMEASURED`
- `SECRETS`
- `CPUID`

KVM handles VMSA pages internally during launch finish: it syncs each VMSA,
marks the VMSA page private in the RMP, performs SNP firmware launch update for
the VMSA page type, sets guest state protected, and enables LBR virtualization.

Evidence:

- `~/ai/eevee/linux/arch/x86/include/uapi/asm/kvm.h:885-891` defines SNP page
  type constants.
- `~/ai/eevee/linux/arch/x86/kvm/svm/sev.c:2491-2546` measures/encrypts VMSAs
  during SNP launch finish.
- `~/ai/eevee/linux/arch/x86/kvm/svm/sev.c:2548-2611` calls VMSA update before
  `SEV_CMD_SNP_LAUNCH_FINISH` and enables pre-faulting after successful finish.

### Attestation certificates

KVM can exit to userspace with `KVM_EXIT_SNP_REQ_CERTS` during guest attestation
if the VMM enables this path with `KVM_SEV_SNP_ENABLE_REQ_CERTS`. The kernel
documentation recommends file-locking discipline so cert blobs stay synchronized
with firmware endorsement keys.

Evidence:

- `~/ai/eevee/linux/Documentation/virt/kvm/x86/amd-memory-encryption.rst:575-619`
  documents `KVM_SEV_SNP_ENABLE_REQ_CERTS` and certificate synchronization.
- `~/ai/eevee/linux/arch/x86/kvm/svm/sev.c:2624-2631` enables certificate exits
  only before vCPU creation and only for SNP guests.
- `~/ai/eevee/linux/Documentation/virt/kvm/x86/amd-memory-encryption.rst:621-635`
  documents the SEV device attributes, including SNP request-certs support.

## Upstream QEMU model

### User-facing configuration

QEMU exposes SNP through a `sev-snp-guest` confidential guest object and the
machine's `confidential-guest-support` property:

```text
-machine ...,confidential-guest-support=sev0
-object sev-snp-guest,id=sev0,cbitpos=51,reduced-phys-bits=1
```

The QAPI properties map closely to KVM/SNP firmware launch fields: policy,
guest-visible workarounds, ID block, ID auth, author-key-enabled, host-data,
and vcek-disabled.

Evidence:

- `~/ai/eevee/qemu/docs/system/i386/amd-memory-encryption.rst:166-229`
  documents the SNP launch flow and command-line example.
- `~/ai/eevee/qemu/qapi/qom.json:1069-1122` documents
  `SevSnpGuestProperties`.
- `~/ai/eevee/qemu/target/i386/sev.h:35-38` defines object type names,
  including `TYPE_SEV_SNP_GUEST`.

### `guest_memfd` and private memory

QEMU marks SNP confidential guest support as requiring `guest_memfd`. Memory
regions and RAM blocks are then allocated with guest-private backing, and QEMU
uses `KVM_SET_MEMORY_ATTRIBUTES` to mark launch-update ranges private.

Evidence:

- `~/ai/eevee/qemu/target/i386/sev.c:3185-3195` sets
  `cgs->require_guest_memfd = true` for `sev-snp-guest`.
- `~/ai/eevee/qemu/include/system/confidential-guest-support.h:64-70` defines
  `require_guest_memfd`.
- `~/ai/eevee/qemu/system/physmem.c:2190-2207` creates a KVM guest memfd for
  private guest memory.
- `~/ai/eevee/qemu/accel/kvm/kvm-all.c:1602-1630` implements setting private
  and shared memory attributes.
- `~/ai/eevee/qemu/target/i386/sev.c:1207-1215` marks SNP launch-update ranges
  private before issuing `KVM_SEV_SNP_LAUNCH_UPDATE`.

### Launch sequencing

QEMU's SNP object installs SNP-specific callbacks for launch start, launch
finish, launch update data, KVM init, CPUID feature adjustment, and KVM VM type.
`sev_snp_launch_start` enables `KVM_HC_MAP_GPA_RANGE`, then calls
`KVM_SEV_SNP_LAUNCH_START`. `sev_snp_launch_update` performs the private-memory
attribute update and loops over `KVM_SEV_SNP_LAUNCH_UPDATE` until KVM reports
the full range consumed. `sev_snp_launch_finish` populates metadata pages,
launch-updates all queued data pages, calls `KVM_SEV_SNP_LAUNCH_FINISH`, then
marks guest state protected.

Evidence:

- `~/ai/eevee/qemu/target/i386/sev.c:1065-1092` implements SNP launch start.
- `~/ai/eevee/qemu/target/i386/sev.c:1182-1249` implements SNP launch update.
- `~/ai/eevee/qemu/target/i386/sev.c:1609-1659` implements SNP launch finish.
- `~/ai/eevee/qemu/target/i386/sev.c:3148-3160` wires SNP callbacks into the
  class.

### CPUID, secrets, kernel hashes, and OVMF metadata

QEMU builds a firmware CPUID page from `KVM_GET_CPUID2` results and passes it
as `KVM_SEV_SNP_PAGE_TYPE_CPUID`. It uses OVMF SEV metadata to find SNP secure
memory, secrets, CPUID, and kernel-hashes pages. If an IGVM is provided, QEMU
does not use the OVMF metadata path and expects IGVM to configure metadata
pages directly.

Evidence:

- `~/ai/eevee/qemu/target/i386/sev.c:244-264` defines the SNP CPUID info
  structures.
- `~/ai/eevee/qemu/target/i386/sev.c:1456-1537` constructs and launch-updates
  the CPUID page.
- `~/ai/eevee/qemu/target/i386/sev.c:1539-1554` handles kernel hashes.
- `~/ai/eevee/qemu/target/i386/sev.c:1556-1607` maps OVMF metadata descriptors
  to SNP page types and populates metadata pages.
- `~/ai/eevee/qemu/target/i386/sev.c:1618-1638` skips OVMF metadata if IGVM is
  used, because IGVM configures metadata pages directly.
- `~/ai/eevee/qemu/target/i386/sev.c:1230-1233` reports CPUID mismatches if
  firmware rejects the CPUID page.

### Runtime private/shared conversion

QEMU handles `KVM_HC_MAP_GPA_RANGE` exits for guest_memfd-backed confidential
guests. For SNP, KVM may synthesize these exits from platform-specific GHCB
requests. QEMU converts memory attributes and optionally pre-faults memory.

Evidence:

- `~/ai/eevee/qemu/target/i386/sev.c:1075-1077` enables
  `KVM_HC_MAP_GPA_RANGE` during SNP launch start.
- `~/ai/eevee/qemu/target/i386/kvm/kvm.c:6462-6480` explains that
  `KVM_HC_MAP_GPA_RANGE` is used to service guest private/shared conversion
  requests for guest_memfd-backed guests such as SNP and TDX.
- `~/ai/eevee/qemu/target/i386/kvm/kvm.c:6482-6510` handles the exit, calls
  `kvm_convert_memory`, and begins pre-faulting.

### Restrictions

QEMU disables SMM for SNP and rejects explicit SMM-on configuration. It also
does not expose a normal SNP launch measurement query; the documentation says
the measurement is available inside the guest through attestation. Traditional
SEV debug/snapshot/migration flows are not the SNP path.

Evidence:

- `~/ai/eevee/qemu/target/i386/sev.c:1985-2003` disables or rejects SMM for
  SNP.
- `~/ai/eevee/qemu/target/i386/sev.c:1973-1979` states QEMU skips the SEV
  machine-done measurement notifier for SNP because measurement is part of
  guest attestation.
- `~/ai/eevee/qemu/docs/system/i386/amd-memory-encryption.rst:232-245` marks
  debug/snapshot as unsupported/TODO in the SEV documentation.

### IGVM handling in QEMU

QEMU's IGVM backend can allocate IGVM-loaded memory with `guest_memfd` when the
confidential guest support object requires it. It scans IGVM supported-platform
headers and chooses the strongest supported technology, preferring SEV-SNP over
SEV-ES, SEV, and native.

Evidence:

- `~/ai/eevee/qemu/backends/igvm.c:207-230` allocates IGVM regions with
  `memory_region_init_ram_guest_memfd` when `require_guest_memfd` is true.
- `~/ai/eevee/qemu/backends/igvm.c:767-845` recognizes
  `IGVM_PLATFORM_TYPE_SEV_SNP` and prefers it when supported.

## Current OpenVMM state

OpenVMM already has some generic concepts for hardware isolation, but the KVM
backend and loader abstractions do not currently implement SNP launch semantics.
The first Linux/KVM milestone is now present: the CLI can request a minimal SNP
configuration, the low-level KVM wrapper can create an SNP VM type after checking
`KVM_CAP_VM_TYPES`, and `virt_kvm` can create `KVM_X86_SNP_VM`. The path then
stops deliberately before normal build/load setup because SNP launch and private
memory are not implemented yet.

### Existing useful abstractions

- `IsolationType` already includes `Snp` and `Tdx`, with hardware isolation
  detection.
- `openvmm` accepts `--isolation snp` for a strict minimal Linux direct-boot
  configuration, and `openvmm_defs::IsolationType` carries `Snp` through to the
  virt layer.
- `vm/kvm` exposes a typed x86 SNP VM creation API that checks
  `KVM_CAP_VM_TYPES`, and `virt_kvm` uses it to create `KVM_X86_SNP_VM` when
  `virt::IsolationType::Snp` is requested.
- The SNP CLI policy currently selects an enlightened Linux direct chipset with
  no emulated chipset devices and rejects Hyper-V enlightenments, VTL2, VMBus,
  PCI/VPCI/device assignment, legacy storage, framebuffer, and non-Linux direct
  boot.
- `PageVisibility` already distinguishes `Exclusive` and `Shared`.
- The generic loader tracks imported/accepted page ranges and returns initial
  page visibility for the hypervisor backend.

Evidence:

- `vmm_core/virt/src/generic.rs:85-108` defines `IsolationType::{None,Vbs,Snp,Tdx}`.
- `openvmm/openvmm_entry/src/cli_args.rs` exposes `IsolationCli::Snp`, and
  `openvmm/openvmm_entry/src/lib.rs` validates the current minimal SNP policy.
- `openvmm/openvmm_defs/src/config.rs` maps `IsolationType::Snp` to
  `virt::IsolationType::Snp`.
- `vm/kvm/src/lib.rs` defines `X86VmType::Snp` and checks `KVM_CAP_VM_TYPES`
  before creating that VM type.
- `vmm_core/virt_kvm/src/arch/x86_64/mod.rs` creates `KVM_X86_SNP_VM` for SNP
  isolation and returns `SnpLaunchNotImplemented` before launch setup.
- `vmm_core/virt/src/generic.rs:137-145` defines `PageVisibility`.
- `vmm_core/vm_loader/src/lib.rs:60-95` returns initial registers and accepted
  visibility ranges.

### Important gaps

1. **The shared loader currently declares all OpenVMM loads non-isolated.**
   `ImageLoad::isolation_config` returns `IsolationType::None` unconditionally.
   This prevents IGVM/importer code from selecting SNP-specific directives.

   Evidence: `vmm_core/vm_loader/src/lib.rs:129-136`.

2. **SNP-specific boot page acceptance types are not represented.** The loader
   maps `Exclusive`, `ExclusiveUnmeasured`, and `Shared` to the two generic
   visibility states, but has `todo!()` for `VpContext`, `ErrorPage`,
   `SecretsPage`, `CpuidPage`, and `CpuidExtendedStatePage`.

   Evidence: `vmm_core/vm_loader/src/lib.rs:71-89`.

3. **Import currently writes plaintext directly into `GuestMemory`.** For SNP on
   KVM, launch pages must be copied through `KVM_SEV_SNP_LAUNCH_UPDATE` into
   guest_memfd/private memory, not merely written into normal userspace-backed
   memory.

   Evidence: `vmm_core/vm_loader/src/lib.rs:139-180` writes imported pages to
   guest memory and zero-fills the rest.

4. **The Linux MSHV backend rejects all isolation.** If SNP is intended on the
   current Linux MSHV path, that backend would need fundamental changes or a new
   KVM backend path.

   Evidence: `vmm_core/virt_mshv/src/x86_64/mod.rs:73-79`.

5. **OpenVMM can now request and create the KVM SNP VM type, but not launch it.**
   The CLI/config path supports `--isolation snp` for a strict minimal Linux
   direct-boot configuration, and `virt_kvm` can create `KVM_X86_SNP_VM`. The
   backend intentionally returns `SnpLaunchNotImplemented` before normal
   build/load setup because `/dev/sev`, `KVM_SEV_INIT2`, guest_memfd,
   memory-attribute setup, SNP launch update/finish, CPUID/secrets pages, and
   VMSA measurement are still missing.

   Evidence:
   - `vmm_tests/vmm_test_macros/src/lib.rs:57-66`
   - `openvmm/openvmm_defs/src/config.rs`
   - `vm/kvm/src/lib.rs`
   - `vmm_core/virt_kvm/src/arch/x86_64/mod.rs`

## Scope: `virt_kvm` vs. cross-cutting changes

Roughly half of the work is KVM-backend-specific and can be localized to
`vmm_core/virt_kvm` plus the low-level `vm/kvm` bindings. The other half is
cross-cutting because OpenVMM needs to describe SNP page types, memory
ownership, runtime visibility, device constraints, and user configuration in
generic layers above the KVM backend.

| Area | Mostly in `virt_kvm`? | Notes |
|---|---:|---|
| KVM capability probing, `KVM_X86_SNP_VM`, `KVM_SEV_INIT2`, SNP launch ioctls | Yes | Backend-specific KVM setup and launch plumbing. |
| `/dev/sev` fd handling, SNP firmware errors, request-certs exits | Yes | Probably `virt_kvm` plus low-level `vm/kvm` bindings. |
| vCPU protected state, VMSA sequencing, KVM CPUID page source | Mostly yes | The KVM calls are backend-specific, but the final CPU policy comes from generic CPU/topology configuration. |
| `guest_memfd` memslots and `KVM_SET_MEMORY_ATTRIBUTES` | Partly | KVM calls are backend-specific; memory backing and page-state tracking likely need shared OpenVMM abstractions. |
| Runtime private/shared conversion exits | Partly | Exit handling is KVM-specific; validation, page-state tracking, and device/DMA policy are cross-cutting. |
| IGVM/load path SNP page types | No | `vm_loader` and importer abstractions need to preserve secrets/CPUID/VMSA/unmeasured page semantics. |
| Generic isolation/page visibility model | No | `virt::PageVisibility` is too small for SNP launch state. |
| CLI/config/policy/host-data/ID block knobs | No | OpenVMM config surface. |
| Device/lifecycle restrictions | No | Needs audits outside `virt_kvm`: SMM/reset, migration, debug, DMA, ballooning/hotplug. |
| Docs/tests | No | Cross-repo. |

In short, `virt_kvm` owns the SNP mechanism, but layers outside `virt_kvm` own
the contract: how loaders describe SNP pages, how memory tracks private/shared
state, which devices and lifecycle operations are allowed, and how users
configure SNP launch policy.

## What OpenVMM would need to change

### 1. Add a Linux/KVM SNP backend surface

OpenVMM needs a backend path that can create `KVM_X86_SNP_VM`, call
`KVM_SEV_INIT2`, manage `/dev/sev`, and issue `KVM_MEMORY_ENCRYPT_OP` with SNP
launch commands. The current Linux MSHV backend rejecting isolation suggests
this is not a small toggle in existing MSHV code.

Required backend capabilities:

- query KVM support for `KVM_X86_SNP_VM`, `KVM_CAP_GUEST_MEMFD`,
  `KVM_CAP_MEMORY_ATTRIBUTES`, and SNP request-certs attributes;
- create SNP VM type; **done for `KVM_CAP_VM_TYPES` + `KVM_X86_SNP_VM` only**;
- open/pass `/dev/sev` fd in `kvm_sev_cmd`;
- implement `KVM_SEV_SNP_LAUNCH_START`, `UPDATE`, `FINISH`;
- enable request-certs if OpenVMM will serve certificate blobs;
- model SNP firmware errors distinctly enough for diagnostics.

### 2. Add guest-private memory backing and page-attribute tracking

SNP on upstream KVM depends on `guest_memfd` for private pages. OpenVMM would
need memory backing that can create/register guest_memfd-backed RAM/memslots and
track shared/private attributes per GPA, because KVM has no get API for memory
attributes.

Required memory changes:

- allocate private RAM through `KVM_CREATE_GUEST_MEMFD`;
- register memslots with guest_memfd and offsets;
- call `KVM_SET_MEMORY_ATTRIBUTES` before launch update;
- track page visibility/private/shared state in OpenVMM;
- support runtime shared/private conversion exits;
- coordinate discard/punch-hole semantics where KVM expects them.

### 3. Preserve SNP page types through the loader

The loading path must stop flattening SNP-specific accepted page types into
only `Exclusive` or `Shared`. It needs to preserve page purpose:

- normal measured pages;
- zero/private pages;
- unmeasured private pages;
- secrets page;
- CPUID page;
- VMSA/VP context page;
- error page and CPUID extended-state page if required by the IGVM/importer
  format and firmware.

The likely abstraction change is to replace or extend `PageVisibility` for
initial load with a richer "initial page state" that carries both visibility
and SNP launch page type.

### 4. Implement SNP launch import semantics

For KVM SNP, initial guest contents should be queued or directly submitted as
SNP launch-update ranges:

- normal data pages -> `KVM_SEV_SNP_PAGE_TYPE_NORMAL`;
- zero/unmeasured pages -> `ZERO` or `UNMEASURED` as appropriate;
- secrets page -> `SECRETS`;
- CPUID page -> build from the actual KVM vCPU CPUID and submit as `CPUID`;
- VP/VMSA pages -> ensure vCPU state is established before launch finish so KVM
  can measure VMSAs.

This is the part that is "mainly loading path", but it depends on the backend
and memory changes above.

### 5. Add CPUID policy/validation flow

QEMU constructs the SNP CPUID page from `KVM_GET_CPUID2`, applies SNP-specific
adjustments, and uses firmware's failed CPUID page copy-back to report
mismatches. OpenVMM would need equivalent logic tied to its CPU topology and
feature filtering.

Open questions for implementation:

- where OpenVMM's final vCPU CPUID policy is materialized for KVM;
- how to encode SNP CPUID page entries;
- how to report firmware mismatch data without panicking.

### 6. Handle runtime private/shared conversions

Guests must be able to request conversion between private and shared pages.
QEMU handles `KVM_HC_MAP_GPA_RANGE` exits, which KVM may synthesize from SNP
GHCB requests. OpenVMM needs an equivalent exit path that:

- validates GPA and size;
- updates KVM memory attributes;
- updates OpenVMM's page-state tracking;
- coordinates guest_memfd backing and shared userspace mappings;
- pre-faults opportunistically if supported;
- integrates with DMA/device models so devices only touch shared pages.

This is outside the initial loading path and is required for normal guest
operation.

### 7. Enforce confidential-guest device and platform restrictions

OpenVMM should reject or disable features incompatible with SNP, similar to
QEMU's SMM rejection. Additional areas to audit:

- SMM/reset flows that require writing protected register state;
- migration, save/restore, debugging, and memory inspection;
- device assignment and DMA into private memory;
- memory ballooning/hotplug;
- crash dump and diagnostics features;
- firmware requirements for OVMF/IGVM SNP metadata.

### 8. Add attestation certificate plumbing

If OpenVMM wants guest SNP attestation to work with KVM's request-certs flow, it
needs userspace handling for `KVM_EXIT_SNP_REQ_CERTS` after enabling
`KVM_SEV_SNP_ENABLE_REQ_CERTS`, plus a configuration surface for cert blobs and
locking semantics compatible with the kernel documentation.

### 9. Update config, docs, and tests

Likely OpenVMM surfaces:

- CLI/config: expose SNP isolation and SNP launch policy fields
  (`policy`, GOSVW, ID block/auth, host data, VCEK/VLEK choice).
- IGVM loader: choose SNP supported-platform headers and honor shared GPA
  boundary/isolation config.
- Docs: update the Linux/OpenHCL run documentation that currently says KVM lacks
  primitives once a KVM SNP path exists.
- Tests: add unit tests for page type mapping, CPUID page construction, and
  mocked launch sequencing; add integration/VMM tests only where SNP hardware
  and firmware are available.

## Concrete OpenVMM change list

This is the practical set of changes OpenVMM likely needs, grouped by subsystem.

### KVM bindings and `virt_kvm`

- Add or update low-level KVM bindings for `KVM_SEV_INIT2`,
  `KVM_SEV_SNP_LAUNCH_START`,
  `KVM_SEV_SNP_LAUNCH_UPDATE`, `KVM_SEV_SNP_LAUNCH_FINISH`,
  `KVM_SEV_SNP_ENABLE_REQ_CERTS`, `KVM_CREATE_GUEST_MEMFD`,
  `KVM_SET_MEMORY_ATTRIBUTES`, `KVM_MEMORY_ATTRIBUTE_PRIVATE`, and
  `KVM_EXIT_SNP_REQ_CERTS`.
- `KVM_CAP_VM_TYPES` probing and `KVM_X86_SNP_VM` creation are implemented in
  `vm/kvm` and `virt_kvm`; add the remaining capability checks for
  `KVM_CAP_GUEST_MEMFD`, `KVM_CAP_MEMORY_ATTRIBUTES`, SEV device attributes, and
  `/dev/sev` availability.
- Teach the KVM partition build path to continue past the current
  `SnpLaunchNotImplemented` stop once SNP initialization and memory backing are
  available.
- Add a launch context object in `virt_kvm` that owns `/dev/sev`, the SNP policy
  fields, launch state, and firmware error translation.
- Implement launch start/update/finish sequencing and ensure launch finish
  happens only after all initial memory, CPUID/secrets pages, and vCPU state are
  ready.
- Add vCPU/VMSA protected-state setup sequencing consistent with KVM's SNP
  launch-finish behavior.
- Add KVM exit handling for `KVM_HC_MAP_GPA_RANGE`/private-shared conversion
  and `KVM_EXIT_SNP_REQ_CERTS`.

### Memory backing and page-state tracking

- Add a guest-private memory backing mode using `guest_memfd`.
- Extend memslot registration so KVM slots can carry `guest_memfd` and
  `guest_memfd_offset`, while still preserving shared userspace mappings where
  needed.
- Add OpenVMM-owned page-state tracking for private/shared state because KVM's
  memory-attributes API has no get operation.
- Add helpers to convert GPA ranges between private and shared by updating both
  OpenVMM state and KVM memory attributes.
- Audit existing memory mapping, remote mapper, hugepage, file-backed memory,
  discard, and DMA assumptions for guest-private memory.

### Loader, IGVM, and initial page model

- Change the loader-facing initial page model so it carries SNP launch page type
  in addition to visibility. `PageVisibility::{Exclusive, Shared}` alone is not
  enough.
- Replace the `todo!()` handling for `BootPageAcceptance::VpContext`,
  `ErrorPage`, `SecretsPage`, `CpuidPage`, and
  `CpuidExtendedStatePage` with explicit initial-page descriptors.
- Make `ImageLoad::isolation_config` report SNP isolation when the VM is being
  loaded as SNP, instead of always returning non-isolated.
- Preserve IGVM SNP supported-platform selection, shared GPA boundary, and page
  acceptance metadata through to the KVM launch code.
- Route imported page contents through SNP launch update instead of only writing
  plaintext into `GuestMemory`.

### CPUID and firmware metadata

- Generate the SNP CPUID page from the final KVM vCPU CPUID values, not from a
  stale or pre-filtered policy.
- Add SNP-specific CPUID adjustments and validation/error reporting so firmware
  CPUID mismatch failures are actionable.
- Populate SNP secrets, CPUID, unmeasured, zero, and kernel-hashes/firmware
  metadata pages from IGVM or firmware metadata.
- Ensure VP context/VMSA state is established early enough for KVM to measure it
  during launch finish.

### Runtime operation

- Add private/shared conversion handling to the VP run loop.
- Ensure device models, DMA paths, MMIO emulation, debugging, diagnostics, and
  memory inspection only access shared pages unless an explicit SNP-safe path
  exists.
- Reject or disable unsupported confidential-guest lifecycle operations such as
  migration, save/restore, debug decrypt/encrypt, memory snapshots, and reset
  paths that require modifying protected guest state.
- Decide whether memory hotplug, ballooning, and assigned devices are unsupported
  initially or require SNP-specific support.

### Configuration and policy

- `--isolation snp` is implemented for an intentionally narrow initial
  configuration: Linux direct boot, KVM, no Hyper-V enlightenments, no VTL2, no
  VMBus, no PCI/VPCI/device assignment, no legacy storage, no framebuffer, no
  emulated chipset devices, and virtio-only devices.
- Add user/config fields for SNP policy, guest-visible workarounds, ID block,
  ID auth, author-key-enabled, host data, VCEK/VLEK selection, and certificate
  blob paths if OpenVMM serves attestation certs.
- Broaden validation as more backends, firmware/IGVM paths, and device models
  become SNP-capable.
- Surface clear diagnostics for missing KVM caps, missing `/dev/sev`, unsupported
  VM type, firmware command failures, CPUID validation failures, and unsupported
  devices.

### Tests and documentation

- Add unit tests for initial page descriptor mapping and duplicate/overlap
  handling with SNP page types.
- Add tests for KVM SNP launch sequencing with mocked KVM ioctls where possible.
- Add CPUID page encoding tests.
- Add hardware-gated integration/VMM tests for actual SNP launch.
- Update Guide documentation for Linux/KVM SNP requirements, supported launch
  modes, unsupported features, and attestation configuration.

## Booting an enlightened Linux kernel via IGVM

Booting an enlightened Linux kernel via IGVM adds another layer on top of the
basic SNP enablement. The IGVM must describe a Linux direct-boot payload and the
runtime must either boot that payload directly as VTL0, or boot OpenHCL first and
let OpenHCL load the measured VTL0 Linux payload described by the IGVM.

### Current OpenVMM/igvmfilegen state

OpenVMM already has partial building blocks:

- `igvmfilegen_config::Image::Linux(LinuxImage)` exists, with `LinuxKernel` and
  optional `LinuxInitrd` resources.
- `igvmfilegen_config::Image::Openhcl { linux: Option<LinuxImage>, ... }`
  exists, meaning an OpenHCL IGVM can include a measured VTL0 Linux direct-boot
  payload.
- `igvmfilegen` already has SNP isolation configuration, SNP platform headers,
  SNP guest policy, SNP measurement generation, SNP VMSA/VP context generation,
  and mappings from `BootPageAcceptance::{SecretsPage,CpuidPage,
  CpuidExtendedStatePage,VpContext}` to SNP IGVM page/directive types.
- The OpenHCL loader can parse measured VTL0 Linux metadata and, on x86_64,
  construct Linux boot parameters, ACPI tables, GDT/page tables, command line,
  and VTL0 register state.

Evidence:

- `vm/loader/igvmfilegen_config/src/lib.rs:84-125` defines `Image::Linux`,
  `Image::Openhcl { linux: ... }`, and `LinuxImage`.
- `vm/loader/igvmfilegen_config/src/lib.rs:208-218` defines the
  `LinuxKernel` and `LinuxInitrd` resource types.
- `vm/loader/igvmfilegen/src/main.rs:632-641` packages a nested VTL0 Linux
  payload into `Vtl0Config::supports_linux` for OpenHCL.
- `vm/loader/src/paravisor.rs:827-875` records measured VTL0 Linux kernel,
  initrd, entrypoint, and command-line metadata.
- `openhcl/underhill_core/src/loader/mod.rs:157-196` selects the measured VTL0
  Linux payload and appends host-provided command-line text.
- `openhcl/underhill_core/src/loader/mod.rs:249-399` builds x86_64 Linux boot
  state from the measured kernel/initrd regions.
- `vm/loader/igvmfilegen/src/file_loader.rs:198-237` creates SNP platform and
  policy headers.
- `vm/loader/igvmfilegen/src/file_loader.rs:822-848` emits SNP VP context
  directives.
- `vm/loader/igvmfilegen/src/file_loader.rs:851-873` maps secrets, CPUID, CPUID
  extended-state, shared, measured, and unmeasured page acceptances to IGVM page
  data types.

### Needed igvmfilegen changes

Some `igvmfilegen` support exists, but it likely needs hardening and new
manifest coverage for this scenario:

- Add a first-class manifest/recipe for "SNP enlightened Linux via IGVM" rather
  than relying on the existing OpenHCL recipe shape.
- Decide whether the target mode is:
  - **OpenHCL-contained VTL0 Linux**: `Image::Openhcl { linux: Some(...) }`.
    This reuses existing OpenHCL measured VTL0 Linux support.
  - **Direct VTL0 Linux IGVM**: `Image::Linux(...)` with no OpenHCL. This likely
    needs more runtime work because the current host-side IGVM reader is VBS
    oriented and the Linux command line is not used by `load_linux()` in the
    top-level `Image::Linux` path.
- Ensure the generated IGVM contains an SEV-SNP supported-platform header, guest
  policy, shared GPA boundary, VMSA/VP context, measured kernel/initrd pages,
  command line metadata, and any required secrets/CPUID pages.
- Fix or implement unsupported page types such as `BootPageAcceptance::ErrorPage`
  if the Linux/SNP flow requires them.
- Ensure generated SNP VMSA fields match what KVM/QEMU accept for IGVM SNP
  VMSAs. QEMU's IGVM documentation restricts VMSA fields and requires the VMSA
  GPA to match KVM's hardcoded VMSA GPA.
- Add tests that build an IGVM with an SNP Linux payload and inspect that the
  output contains the expected SNP platform, policy, page types, VMSA context,
  Linux kernel/initrd metadata, and measurement.

### Needed OpenVMM runtime changes

The host/runtime changes depend on the chosen mode:

#### If booting VTL0 Linux through OpenHCL

This is the lower-risk path because OpenHCL already has measured VTL0 Linux
plumbing. Required work is mostly to make it function with SNP-on-KVM:

- Make host IGVM parsing choose the SNP supported-platform header instead of
  assuming VBS.
- Preserve SNP page types through host loading and KVM launch update.
- Ensure OpenHCL receives the measured VTL0 Linux metadata and chooses
  `LoadKind::Linux`.
- Ensure VTL0 Linux boot state pages that OpenHCL constructs at runtime are made
  visible/accepted with the correct SNP private/shared state.
- Ensure VTL0 Linux command-line append remains policy-compatible. Anything
  host-appended after measurement must be treated as unmeasured or explicitly
  part of the trust model.

#### If booting direct VTL0 Linux from IGVM without OpenHCL

This is more work:

- Teach the OpenVMM host IGVM loader to parse SNP IGVM files directly. Today the
  host IGVM reader calls `IgvmFile::new_from_binary(..., Some(IsolationType::Vbs))`
  and looks for `VSM_ISOLATION` headers.
- Implement SNP platform selection and compatibility-mask handling in
  `openvmm_core` similar to QEMU's IGVM backend.
- Make top-level `Image::Linux` generation carry and use the Linux command line,
  not just kernel/initrd load info.
- Have host-side OpenVMM construct or consume Linux boot params, command line,
  initrd info, ACPI/CC blob data, and initial registers from IGVM in a way that
  can be measured/launched through KVM SNP.
- Decide how AP startup works without OpenHCL assistance.

### Kernel choice

Use a recent upstream x86_64 Linux kernel from `~/ai/eevee/linux`, built with
SEV-SNP guest support and Hyper-V/OpenVMM enlightenments enabled. It should be
an uncompressed `vmlinux` if using OpenVMM's existing Linux direct-boot loader;
the OpenVMM user guide explicitly says `--kernel` expects `vmlinux`, not
`bzImage`.

The kernel should include at least:

- AMD memory encryption / SEV-SNP guest support, including the SEV guest driver
  for attestation/report paths;
- GHCB/VC handling needed by SNP guests;
- Hyper-V guest drivers/enlightenments used by OpenVMM/OpenHCL, including VMBus
  if the test initrd expects VMBus devices;
- serial console support for early bring-up;
- initrd support;
- virtio or VMBus storage/network drivers depending on the test environment.

The safest bring-up kernel is the local upstream Linux tree with a small
known-good config for SNP + Hyper-V/OpenVMM. Avoid distro kernels initially
unless their config is known to include SNP guest support and the needed
enlightenment drivers.

### Initrd choice

For initial bring-up, use a minimal initrd rather than a full distro image:

- include BusyBox or an equivalent shell;
- include modules matching the exact test kernel, or build required drivers in;
- mount `proc`, `sysfs`, and `devtmpfs`;
- print `dmesg`;
- expose a shell on the serial console;
- optionally include SNP attestation tools later, once the basic boot path works.

OpenVMM already has prebuilt test kernel/initrd artifacts restored by
`cargo xflowey restore-packages`, documented in the OpenVMM run guide, but those
are best treated as non-SNP direct-boot smoke-test inputs unless their kernel
config is verified for SNP. For SNP bring-up, prefer building a matched kernel
and initrd from the local Linux tree so the config and modules are controlled.

The Alpine direct-boot setup in `scripts/setup-alpine.sh` is a reasonable
starting point for smoke testing. It downloads an Alpine cloud image, extracts
`vmlinuz-virt` and `initramfs-virt`, converts the disk to raw, and creates a
cloud-init data disk. For the IGVM flow, the useful pieces are:

- `vmlinux-virt` as the `LinuxKernel` resource if using Alpine's kernel;
- `initramfs-virt` as the `LinuxInitrd` resource;
- `disk.raw` as the root disk;
- `cidata.img` for root login/cloud-init setup;
- a command line like `root=/dev/vda2 rootfstype=ext4
  modules=virtio_pci,virtio_blk,ext4`, plus the console settings appropriate
  for the OpenHCL/IGVM path.

Using a private kernel with Alpine's initrd is viable, but only if the private
kernel has all boot-critical drivers built in or the initrd contains matching
modules for that exact kernel version. Alpine's initrd modules are built for
Alpine's `vmlinuz-virt`; they will not match a private kernel's `uname -r`.
Therefore the recommended bring-up combination is:

- use the private SNP/enlightened `vmlinux`;
- reuse Alpine's initrd/root disk/cloud-init image for userspace convenience;
- build boot-critical functionality into the private kernel, at minimum ext4,
  initrd/devtmpfs basics, the chosen console path, and the storage bus used for
  the root disk (`virtio_pci`/`virtio_blk` for the current Alpine script, or
  VMBus storage if the test switches to VMBus);
- add SNP attestation tools to the initrd later only after basic boot works.

As a small improvement, `scripts/setup-alpine.sh` should also extract
`/boot/config-virt` from the Alpine image so we can quickly check whether the
stock Alpine kernel has the SNP and enlightenment options needed for experiments
that use Alpine's kernel directly.

## Is the work mainly around the loading path?

No. The loading path is necessary but not sufficient.

The launch loader must understand SNP page types and submit pages via
`KVM_SEV_SNP_LAUNCH_UPDATE`, but upstream KVM/QEMU make clear that working SNP
also depends on guest_memfd private memory, KVM memory attributes, vCPU/VMSA
protected-state handling, runtime conversion exits, CPUID page generation,
attestation-certificate handling, and platform/device restrictions.

A practical implementation should start by building the backend/memory
foundation first, then wire the loader onto it. Trying to only modify the
loader would leave OpenVMM with nowhere correct to put private pages and no way
to service guest runtime conversions.

## Suggested implementation order

1. Complete SNP VM initialization after the existing `KVM_X86_SNP_VM` creation:
   add `/dev/sev`, `KVM_SEV_INIT2`, launch-start state, and remaining capability
   checks.
2. Add guest_memfd-backed RAM/memslot support and OpenVMM-owned page-state
   tracking.
3. Extend loader/importer abstractions to preserve SNP page types.
4. Implement SNP launch start/update/finish for IGVM or firmware loading.
5. Add CPUID page generation and firmware mismatch diagnostics.
6. Add runtime private/shared conversion exit handling.
7. Add attestation certificate support.
8. Gate/reject incompatible devices and lifecycle operations.
9. Add docs and tests for the supported subset.

## Readiness for concrete implementation breakdown

The suggested implementation order is enough to organize the work into epics and
to decide sequencing, but it is not yet enough to turn every item into
implementation-ready tasks. Some areas can be broken down directly, while others
need a short design/investigation pass first so the cross-layer contracts are
clear before code is written.

### Work that is concrete enough to start

These items have clear current-code seams and can be decomposed into concrete
tasks with limited additional investigation:

- add KVM SNP capability probing and clear rejection diagnostics for unsupported
  hosts or backends;
- add a minimal KVM SNP backend skeleton behind `IsolationType::Snp`;
- extend the loader-facing initial page model so SNP page acceptances are not
  flattened into only `PageVisibility::{Exclusive,Shared}`;
- replace the current `todo!()` handling for SNP-specific
  `BootPageAcceptance` variants with explicit page descriptors;
- add unit tests for page-acceptance to initial-page descriptor mapping,
  duplicate detection, and overlap handling;
- add documentation for the initially supported and unsupported SNP subset once
  that subset is selected.

### Areas that need more investigation or definition

These should be tackled one by one before detailed implementation tasks are
assigned:

1. **KVM binding and ioctl surface.** Confirm which SNP, `guest_memfd`, memory
   attribute, and certificate-exit constants and structs already exist in the
   `vm/kvm` bindings, which need to be added, and how `/dev/sev` firmware errors
   should be represented.
2. **Guest-private memory ownership.** Decide where `guest_memfd` file
   descriptors live, how guest-private backing fits with `GuestMemory` and KVM
   memslot registration, how shared userspace mappings coexist with private
   backing, and how discard/punch-hole behavior is coordinated.
3. **OpenVMM page-state tracking.** Define the owner and data structure for
   private/shared page state, because KVM's memory-attributes API has no get
   operation. This state must be updated by launch, runtime conversion exits, and
   any future memory hotplug or discard paths.
4. **Loader-to-backend launch contract.** Decide whether to extend
   `PageVisibility` or introduce a new initial page/launch descriptor carrying
   visibility, SNP page type, measurement state, and special page purpose
   (`SECRETS`, `CPUID`, VMSA/VP context, etc.).
5. **Launch lifecycle sequencing.** Identify the exact OpenVMM lifecycle points
   for `KVM_SEV_INIT2`, `LAUNCH_START`, launch updates, vCPU protected-state/VMSA
   setup, and `LAUNCH_FINISH`, and ensure VPs cannot run before finish
   succeeds.
6. **CPUID source and validation.** Determine where the final KVM vCPU CPUID
   policy is materialized, how to construct the SNP CPUID page from it, and how
   firmware mismatch information should be surfaced without panicking.
7. **Runtime private/shared conversion handling.** Confirm the KVM exit shape
   OpenVMM must handle, define GPA/size validation rules, update ordering between
   OpenVMM state and `KVM_SET_MEMORY_ATTRIBUTES`, and decide whether to pre-fault
   converted ranges.
8. **Device and DMA policy.** Audit which device models, MMIO paths, DMA paths,
   remote mappers, debug/diagnostic features, and memory inspection paths assume
   host-readable guest RAM, then define the initial rejection or shared-page-only
   policy.
9. **Supported MVP boot path.** Choose the first supported target before coding
   the whole stack: IGVM plus OpenHCL-contained VTL0 Linux, direct VTL0 Linux
   IGVM, OVMF-style firmware loading, or a smaller bring-up harness. This choice
   affects loader metadata, CPUID/secrets pages, AP startup, and tests.
10. **Lifecycle restrictions.** Explicitly decide which operations are disabled
    for the first SNP implementation, including migration, save/restore, reset,
    debug decrypt/encrypt, memory snapshots, ballooning, hotplug, assigned
    devices, and SMM-like flows.
11. **Attestation certificate scope.** Decide whether
    `KVM_EXIT_SNP_REQ_CERTS` support is part of the MVP or a follow-up. If it is
    included, define the config surface for certificate blobs and the locking
    semantics expected by the kernel documentation.

The highest-risk design dependencies are the memory backing/page-state model,
the loader-to-backend launch descriptor API, and the launch lifecycle sequence.
Those should be resolved before implementing the later CPUID, runtime
conversion, attestation, and device-policy work.
