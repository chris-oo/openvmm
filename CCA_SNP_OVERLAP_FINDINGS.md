# Arm CCA and SEV-SNP support overlap findings

This note summarizes how much of the work to support AMD SEV-SNP in OpenVMM is
also useful for supporting Arm CCA/Realms, based on:

- `SNP_VM_SUPPORT_FINDINGS.md` in this repository;
- the Arm CCA v13 KVM patch series in `~/ai/eevee/kvm-mail`;
- the CCA-enabled kvmtool tree in `~/ai/eevee/kvmtool-cca`.

## Executive summary

There is substantial overlap in the infrastructure OpenVMM should build, but
the final launch and vCPU mechanisms are mostly technology-specific.

If SNP support is implemented with generic confidential-guest abstractions, it
should provide a large part of the foundation needed for CCA:

- confidential VM type/capability plumbing;
- guest-private memory backing;
- private/shared page-state tracking;
- initial measured/private page population abstractions;
- runtime private/shared conversion handling;
- confidential-VM device and lifecycle restrictions; and
- user-visible isolation configuration.

If SNP support is implemented as x86-only SEV plumbing inside the KVM backend,
much less will carry over to CCA.

Rough estimate: **40-60% of the CCA-enabling foundation overlaps with SNP**,
mostly in memory management, loader contracts, page-state tracking, device
policy, and KVM backend shape. The remaining work is Arm CCA/RMI/REC-specific.

## Overlap matrix

| Area | Overlap | Notes |
|---|---:|---|
| KVM confidential-VM abstraction | High | Both need a distinct KVM VM type/capability path, protected launch lifecycle, protected vCPU state, and backend capability probing. SNP uses `KVM_X86_SNP_VM`; CCA uses the Arm realm VM type and `KVM_CAP_ARM_RME`. |
| Private memory / `guest_memfd` / memory attributes | High | SNP requires `guest_memfd` and `KVM_SET_MEMORY_ATTRIBUTES`. CCA v13 also selects `KVM_GUEST_MEMFD` and `KVM_GENERIC_MEMORY_ATTRIBUTES` and exposes private memory for Realm VMs. This is the biggest reusable foundation. |
| Runtime private/shared conversion tracking | High | SNP handles guest private/shared conversion requests via KVM exits such as `KVM_HC_MAP_GPA_RANGE`. CCA handles `RMI_EXIT_RIPAS_CHANGE`, treating it as a protected/unprotected conversion that userspace must coordinate with guest_memfd and memslot mappings. |
| Initial image population | Medium-high | Both need the loader/backend boundary to describe measured, unmeasured, private, shared, and zeroed initial pages. The actual mechanisms differ: SNP launch-update page types versus CCA `KVM_ARM_RMI_POPULATE`/RMI data-map-init. |
| Measurement/finalization | Medium | Both measure and seal initial state, but SNP uses SEV-SNP launch start/update/finish and VMSA measurement, while CCA creates/configures a Realm Descriptor, populates measured data, finalizes RECs, and activates the Realm. |
| vCPU protected state | Medium | A generic protected-vCPU lifecycle can help, but the implementation differs. SNP uses VMSA/protected register state; CCA uses Realm Execution Contexts and `KVM_ARM_VCPU_FINALIZE(KVM_ARM_VCPU_REC)`. |
| Attestation/config knobs | Medium-low | Both need policy and measurement-related user configuration, but the knobs differ. SNP has policy, ID block, host data, and certificate handling. CCA has hash algorithm, Realm Personalization Value, SVE, PMU, and debug configuration. |
| Firmware/boot model | Low-medium | SNP is tied to x86 CPUID/VMSA/OVMF/IGVM metadata. CCA in kvmtool direct-boots arm64 kernel/initrd/DTB and populates those assets as Realm memory. |
| Device restrictions | Medium | Both need confidential-VM device and DMA policy. CCA explicitly prevents device mappings for Realms and kvmtool forces `VIRTIO_F_ACCESS_PLATFORM`; SNP needs equivalent care for DMA, SMM, debug, migration, and hotplug restrictions. |
| Architecture-specific CPU/platform work | Low | SNP CPUID, VMSA, GHCB, and SEV firmware details are x86/AMD-specific. CCA RMI/RSI, PSCI, VGIC, timers, SVE, PMU, REC, and RIPAS handling are arm64-specific. |

## What SNP work should make generic

The existing SNP findings already point to cross-cutting OpenVMM gaps that are
also relevant to CCA:

- `PageVisibility` only represents `Exclusive` and `Shared`, which is too small
  for SNP page types and also too small for CCA measured/unmeasured/private
  population semantics.
- The loader currently flattens or rejects important accepted-page types instead
  of preserving their purpose through to the hypervisor backend.
- The import path writes plaintext directly into `GuestMemory`, but both SNP and
  CCA need backend-mediated population of protected/private memory.
- The KVM backend needs guest-private memory and page-state tracking rather than
  assuming normal userspace-backed RAM is sufficient.

The best reusable abstraction is likely an initial page-state contract richer
than `PageVisibility`, carrying at least:

- visibility: private/shared;
- content source: data/zero;
- measurement: measured/unmeasured;
- purpose: normal data, firmware metadata, secrets/CPUID-like special page,
  vCPU context, DTB/initrd/kernel, or architecture-specific extension; and
- allowed runtime conversion behavior.

The backend can then map that generic state to SNP launch-update page types or
CCA Realm population calls.

## CCA-specific work that SNP will not provide

CCA support still needs substantial Arm-specific implementation:

- create a Realm VM type and probe `KVM_CAP_ARM_RME`;
- configure Realm parameters such as hash algorithm, Realm Personalization Value,
  SVE, PMU, and debug capabilities;
- create the Realm Descriptor;
- initialize and populate IPA ranges using CCA/RMI ioctls;
- finalize vCPUs as RECs with `KVM_ARM_VCPU_FINALIZE(KVM_ARM_VCPU_REC)`;
- activate the Realm after vCPU initial state is ready;
- handle Realm PSCI requests;
- handle VGIC and timer behavior for Realm execution;
- handle Realm MMIO exits;
- handle `RMI_EXIT_RIPAS_CHANGE` conversions;
- enforce Arm-specific register access restrictions;
- expose or hide SVE, PMU, debug, stolen time, and other capabilities correctly;
  and
- reject or constrain device mappings that cannot be safely used with Realms.

These are not reusable from SNP except through broad lifecycle hooks and policy
interfaces.

## Evidence from `kvmtool-cca`

The CCA-enabled kvmtool tree models Realm support as a distinct Arm path:

- `arm/aarch64/kvm.c` validates `--realm`, rejects AArch32 Realms, chooses the
  Realm measurement algorithm, validates the Realm Personalization Value, and
  disables SVE by default unless a vector length is explicitly requested.
- `arm/aarch64/kvm.c` selects the Realm VM type when `--realm` is requested and
  checks `KVM_CAP_ARM_RME`.
- `arm/aarch64/realm.c` configures hash, RPV, SVE, PMU, and debug parameters,
  creates the Realm Descriptor, initializes IPA ranges, populates kernel/initrd
  and DTB pages, and activates the Realm.
- `arm/aarch64/kvm-cpu.c` skips normal PSTATE setup for Realms and finalizes the
  vCPU with `KVM_ARM_VCPU_FINALIZE(KVM_ARM_VCPU_REC)`.
- `arm/fdt.c` uses PSCI `smc` rather than `hvc` for Realms and populates the DTB
  into Realm memory after FDT generation.
- `arm/aarch64/kvm.c` forces `VIRTIO_F_ACCESS_PLATFORM` for Realm guests.
- `arm/kvm.c` pins Realm memory and avoids mergeable pages until export/import
  support exists.

## Evidence from CCA v13 on LKML

The v13 CCA series in `~/ai/eevee/kvm-mail` is broad and includes:

- RMI SMC definitions and wrappers;
- KVM checks for RMI support;
- Realm user ABI;
- infrastructure for creating a Realm;
- Realm Descriptor creation;
- Realm enter/exit;
- REC allocation and vCPU lifecycle;
- VGIC and timer support in Realms;
- `RMI_EXIT_RIPAS_CHANGE` handling;
- Realm MMIO emulation;
- support for private memory;
- initial contents population;
- initial memslot RIPAS setup;
- runtime faulting of memory;
- Realm PSCI requests;
- register access validation;
- capability filtering for Realm guests;
- PMU/SVE/debug propagation and configuration; and
- prevention of device mappings for Realms.

Two particularly important overlaps with SNP are visible in the series:

1. **Private memory support.** The patch titled
   `[PATCH v13 23/48] KVM: arm64: Expose support for private memory` selects
   `KVM_GUEST_MEMFD` and `KVM_GENERIC_MEMORY_ATTRIBUTES`, and makes private
   memory depend on the VM being a Realm.
2. **Initial protected population.** The patch titled
   `[PATCH v13 24/48] arm64: RMI: Allow populating initial contents` adds an
   ioctl path for userspace to populate Realm memory from userspace buffers,
   optionally measuring the populated data.

The RIPAS conversion patch also shows a direct conceptual match with SNP runtime
conversion handling: `RMI_EXIT_RIPAS_CHANGE` represents a Realm request to move
memory between protected and unprotected states, and userspace must coordinate
the backing guest memory and mappings before KVM completes the change.

## Implication for OpenVMM

The SNP implementation should avoid baking SNP assumptions into the loader or
KVM backend APIs. A better target is:

1. a generic confidential-guest launch trait/state machine;
2. generic private-memory backing and page-state tracking;
3. a rich initial page-state model;
4. generic runtime private/shared conversion dispatch;
5. generic confidential-VM device/lifecycle restrictions; and
6. architecture-specific launch drivers for SNP, TDX, and CCA.

With that split, SNP and CCA share the hard infrastructure while keeping the
architecture-specific protocol code isolated.
