# SNP follow-up inventory

Snapshot:

- Upstream `main`: `b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33`
- `chris-oo:mshv-snp-core`: `da4628d98ede680fd46fc6a560da529847160aa6`
- Latest #4260 head checked: 2026-08-20
- Primary PRs reviewed: [#3939](https://github.com/microsoft/openvmm/pull/3939), [#3970](https://github.com/microsoft/openvmm/pull/3970), [#4233](https://github.com/microsoft/openvmm/pull/4233), [#4237](https://github.com/microsoft/openvmm/pull/4237), [#4257](https://github.com/microsoft/openvmm/pull/4257), and [#4260](https://github.com/microsoft/openvmm/pull/4260)

PR #4260 will be squash-merged. Its head commit IDs and line numbers are
temporary. Until merge, treat each #4260 reference as a file path plus a nearby
symbol or comment. After merge, replace those links with permalinks to the
squash commit and refresh the line ranges.

The `mshv-snp-core` diff contains 15 added `TODO`, `HACK`, or explicit
workaround markers. The tables below combine closely related markers into
logical follow-up items and add the untagged temporary behavior called out in
the PR descriptions and review discussions.

## 1. Guest-memory ownership and host access

| Priority | Follow-up | Evidence | Notes |
|---|---|---|---|
| Critical | Design a complete MSHV SNP host-access lifecycle: serialize lazy acquisition with guest shared/private transitions, prevent new faults while revoking, drain active `GuestMemory`, locked-range, device, and DMA users, and only report success after the release ioctl succeeds. | [`PartitionMemoryMap` abstraction TODO](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt/src/generic/partition_memory_map.rs#L46-L54), [`virt_mshv` acquisition-only implementation](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/lib.rs#L875-L885), [acquire/revoke race](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/snp.rs#L297-L307), [revocation/drain requirements](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/snp.rs#L922-L930) | This is the largest correctness gap in #4260. The PR description explicitly calls the current shared-memory host access insufficient. |
| High | Decide the long-term KVM guestmemfd transition model and failure atomicity. The current non-in-place implementation changes KVM attributes, then discards the stale shared/private backing; discard failure can leave an effective state change despite returning an error. | [attribute update followed by discard](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/vmm_core/virt_kvm/src/memory.rs#L316-L383), [dual-backing discard implementation](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/vmm_core/virt_kvm/src/memory.rs#L386-L449), [PR review discussion](https://github.com/microsoft/openvmm/pull/3970#discussion_r3617934070), [in-place follow-up response](https://github.com/microsoft/openvmm/pull/3970#discussion_r3617991584) | The review expected a later move toward in-place guestmemfd. Before removing the current path, confirm the kernel versions OpenVMM must continue to support. |

## 2. Launch, measurement, and loader architecture

| Priority | Follow-up | Evidence | Notes |
|---|---|---|---|
| Medium | Stop pre-importing every RAM page for KVM SNP direct boot. Use IGVM/firmware/a bootshim, or let the guest accept remaining memory after launch. | [bring-up hack](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/openvmm/openvmm_core/src/worker/vm_loaders/linux.rs#L61-L65), [PR #3970 description](https://github.com/microsoft/openvmm/pull/3970) | Primarily a severe launch-performance problem. |
| High | Replace the hard-coded debug-capable KVM SNP launch policy with policy supplied by the image/IGVM or explicit configuration. | [policy TODO](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/vmm_core/virt_kvm/src/snp.rs#L110-L119), [review discussion](https://github.com/microsoft/openvmm/pull/3970#discussion_r3615748014) | Security-sensitive bring-up default. |
| High | Plumb the MSHV SNP launch/attestation policy instead of always using the default policy with ID-block and author-key verification disabled; also document or validate the point at which the partition is moved to the secure isolation state. | [secure-state transition](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/snp.rs#L549-L570), [default policy and disabled verification](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/snp.rs#L712-L720) | Significant untagged bring-up behavior in #4260. |
| High | Import loader-provided VMSAs directly once KVM supports it, instead of translating the VMSA back into register-setting calls. | [translation TODO](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/vmm_core/virt_kvm/src/snp.rs#L164-L166), [PR #4233 description](https://github.com/microsoft/openvmm/pull/4233) | KVM currently constructs its own measured VMSA state, so it cannot match an image-supplied expected VMSA measurement. |
| Medium | Revisit initial-register/page-import ordering when KVM can import VMSA pages; `SNP_LAUNCH_FINISH` currently forces registers to be loaded first. | [ordering TODO](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/openvmm/openvmm_core/src/worker/dispatch.rs#L3403-L3410) | Also needs a backend-neutral model for other isolation types. |
| Medium | Remove or relocate the SNP-specific VMSA finalization path from the generic loader if direct boot converges on IGVM-only loading. | [generic-loader TODO](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/vmm_core/vm_loader/src/lib.rs#L344-L348), [review discussion](https://github.com/microsoft/openvmm/pull/4233#discussion_r3778156031) | Architectural cleanup rather than a boot blocker. |

## 3. Kernel and platform compatibility hacks

| Priority | Follow-up | Evidence | Notes |
|---|---|---|---|
| High | Reduce the SNP direct-boot chipset from full PIC/PIT/power-management devices to the minimum timer/port surface required for TSC calibration, or fix the guest kernel. | [main TODO and candidate minimal surface](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/vmm_core/vm_manifest_builder/src/lib.rs#L546-L564) | Shared by KVM and MSHV direct boot. |
| Medium | Remove or gate the CMOS RTC added for the current SNP direct-boot repro kernel. | [RTC HACK](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/vm_manifest_builder/src/lib.rs#L541-L545) | Added by #4260, but it is on the shared enlightened-direct chipset path used by both backends. |
| Medium | Re-enable the Hyper-V reference-TSC feature after fixing/tracing MSHV SNP PSC/host-access handling for the shared reference-TSC page. | [reference-TSC HACK](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/mod.rs#L205-L219) | Currently forces ACI onto the reference-counter MSR so virtio I/O works. |
| Medium | Remove `--pcie-ecam-below-4gb` from SNP configurations when direct-boot kernels can discover high ECAM through firmware tables or a kernel fix. | [compatibility option](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/Guide/src/reference/openvmm/management/cli.md#L321-L326), [placement code](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/openvmm/openvmm_core/src/worker/memory_layout.rs#L238-L245), [PR #4237 description](https://github.com/microsoft/openvmm/pull/4237) | Explicit compatibility switch; high ECAM remains the default. |
| Medium | Implement MSHV LINT injection, or ensure isolated guests never depend on PIC ExtINT/LINT0 after the temporary calibration devices are removed. | [LINT TODO and limitation](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/mod.rs#L537-L544) | The TODO says non-isolated partitions, while the adjacent comment documents the isolated-SNP consequence. |

## 4. CPUID and Hyper-V guest-contract cleanup

| Priority | Follow-up | Evidence | Notes |
|---|---|---|---|
| High | Define the correct CPUID leaf list for enlightened direct-boot SNP instead of reusing the paravisor list. | [loader TODO](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vm/loader/src/linux.rs#L369-L383) | The reused list currently supplies Linux's extended-state subleaves. |
| High | Import enough extended-state CPUID data to preserve supported XSS components instead of clearing them. | [XSS TODO](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/snp.rs#L378-L387) | Current behavior avoids Linux consuming missing component entries as zero-offset user state. |
| Medium | Determine why MSHV does not derive the Hyper-V SNP isolation CPUID contract from `MSHV_PT_ISOLATION_SNP`, then remove the userspace overrides if the kernel/hypervisor should supply them. | [isolation-leaf TODO](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/snp.rs#L448-L464) | Needed for ACI to use direct VMMCALL and retain restricted-injection behavior. |
| Medium | Choose the durable synthetic Hyper-V CPUID strategy: measured-page entries, GHCB fallback, or guest-contract-dependent support for both. | [strategy TODO](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/snp.rs#L633-L660) | Current augmentation is retained for Linux versions that treat missing leaves as successful all-zero results. |
| Medium | Confirm and formalize the ACI-specific `HvCallStartVP` convention that encodes `vmsa_gpa \| 1` in the nominal initial context. | [contract TODO](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/snp.rs#L492-L508) | Could become a documented Microsoft Hypervisor SNP ABI or be replaced. |

## 5. VP lifecycle, interrupts, and reset

| Priority | Follow-up | Evidence | Notes |
|---|---|---|---|
| Medium | Add SNP AP `CREATE_ON_INIT` and `DESTROY` when the MSHV kernel ABI exposes target-VP lifecycle operations. | [AP lifecycle TODO](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/snp.rs#L235-L265) | Only `SNP_AP_CREATE` is accepted today. |
| Medium | Support firmware-reload reset for isolated partitions, or define a different reset contract. | [reset TODO](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/openvmm/openvmm_core/src/worker/dispatch.rs#L4015-L4019) | Currently rejected for every isolated VM. |
| Keep unless disproved | Retain the 2 MiB VMSA alignment rejection unless MSHV documents that it is unnecessary; it mirrors KVM's workaround for an SNP RMP/VMSA erratum. | [erratum workaround](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/vmm_core/virt_mshv/src/x86_64/snp.rs#L259-L265) | This is a workaround marker, but likely hardware correctness rather than bring-up debt. |

## 6. Support gaps that are not tagged TODOs

| Backend | Current constrained surface | Evidence |
|---|---|---|
| KVM SNP | Linux direct boot only; no Hyper-V enlightenments, VTL2, VMBus, framebuffer/VGA/debugger; only the small approved chipset set and virtio-over-PCIe devices; hugetlb is also rejected by CLI validation. | [central config checks](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/openvmm/openvmm_entry/src/lib.rs#L2048-L2091), [CLI hugetlb/direct-boot checks](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/openvmm/openvmm_entry/src/cli_args.rs#L1387-L1398) |
| MSHV SNP | Linux direct boot only; Hyper-V enlightenments are allowed only without VMBus; no UEFI, VTL2, hugetlb, or non-virtio optional devices. Restricted injection is still an explicit bring-up switch. | [branch CLI documentation](https://github.com/microsoft/openvmm/blob/da4628d98ede680fd46fc6a560da529847160aa6/Guide/src/reference/openvmm/management/cli.md#L83-L94) |

## 7. Review-only architectural debt

| Follow-up | Evidence | Status |
|---|---|---|
| Make isolation capability/config validation canonical and type-driven instead of maintaining overlapping checks in CLI/entry and worker dispatch. | [scalability review](https://github.com/microsoft/openvmm/pull/3970#discussion_r3624476128), [canonical-validation review](https://github.com/microsoft/openvmm/pull/3970#discussion_r3624164192), [entry validation](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/openvmm/openvmm_entry/src/lib.rs#L2048-L2091), [worker validation](https://github.com/microsoft/openvmm/blob/b6d6e0c15fef2f7219eaf48c2a3f65963ab2ac33/openvmm/openvmm_core/src/worker/dispatch.rs#L1177-L1222) | Still present, but not tagged TODO. |
| Split the large `virt_mshv::ErrorInner` into smaller typed errors at subsystem boundaries. | PR #4260, `vmm_core/virt_mshv/src/lib.rs`, near `ErrorInner` and its `TODO: Chunk this up into smaller types`; [review comment](https://github.com/microsoft/openvmm/pull/4260#discussion_r3798761101) | The TODO was present in the reviewed diff but was missing from this inventory. Keep this separate from the KVM error-enum cleanup completed by #4257. |

## 8. Proposed GitHub issue filing plan

File one umbrella issue for each open top-level section, not one issue for each
table row. Use each row as a checklist item in its umbrella issue. This keeps
related design choices together without losing independently closable work.

| Section | Proposed title | Labels | Body scope |
|---|---|---|---|
| 1 | `MSHV SNP: complete guest-memory ownership and host-access lifecycle` | `snp`, `bug` | Include both host-access lifecycle rows. State the concurrency and release-success invariants explicitly. Separate MSHV implementation work from the related KVM guestmemfd design decision in the checklist. |
| 2 | `SNP: finish launch policy, measurement, and loader architecture` | `snp`, `enhancement` | Include all launch, policy, VMSA, ordering, and generic-loader rows. Call out which work is MSHV-only, KVM-only, or backend-neutral. |
| 3 | `MSHV SNP: remove direct-boot kernel and platform compatibility hacks` | `snp`, `enhancement` | Include chipset, RTC, reference-TSC, ECAM, and LINT rows. Record the guest-kernel version or behavior that permits removal of each workaround. |
| 4 | `MSHV SNP: define the CPUID and Hyper-V guest contract` | `snp`, `enhancement` | Include all five contract rows. Require a documented source of truth for measured CPUID, runtime CPUID, XSS, isolation leaves, and StartVP behavior. |
| 5 | `MSHV SNP: complete VP lifecycle and reset semantics` | `snp`, `enhancement` | Include AP lifecycle and reset work. Keep the VMSA alignment check as a verification item, not removal work, unless MSHV or AMD documentation disproves the need. |
| 6 | `SNP: track unsupported OpenVMM configuration surfaces` | `snp`, `enhancement` | Convert the KVM and MSHV constrained-surface rows into separate checklists. Add support only with a test and remove the matching validation rejection in the same change. |
| 7 | `SNP: consolidate isolation validation and backend error architecture` | `snp`, `enhancement` | Include canonical type-driven validation and the `virt_mshv::ErrorInner` split. Link the relevant review discussions and avoid making this a general unrelated refactor. |

For each issue body:

1. Link PR #4260 and this inventory.
2. Copy the section rows as task-list items, preserving priority.
3. Cite the file path and nearby symbol or comment instead of a temporary PR
   head commit.
4. Add acceptance criteria and tests for each checked item.
5. Note dependencies on MSHV kernel ABI, guest-kernel fixes, or KVM support.
6. After #4260 merges, replace relative locations with squash-commit
   permalinks.

## Already resolved or superseded follow-ups

- The two correctness findings from the first #4260 review are fixed in the
  current PR head.
  `hv1_reference_tsc_page` now follows the SNP feature policy near
  `hv1_reference_tsc_page_supported` in
  `vmm_core/virt_mshv/src/x86_64/mod.rs`, and tracked-range end calculations
  use checked addition in `MshvPartitionInner::unmap_range` in
  `vmm_core/virt_mshv/src/lib.rs`. See
  [reference-TSC review](https://github.com/microsoft/openvmm/pull/4260#discussion_r3787086262)
  and
  [range-overflow review](https://github.com/microsoft/openvmm/pull/4260#discussion_r3787086324).
- The latest #4260 updates also moved the CPUID-offload diagnostic into
  MSHV-specific configuration, introduced `MshvIsolationState`, and moved the
  imported-initial-register decision to the partition boundary. These resolve
  the associated placement review comments and do not need separate issues.
- The request to split the large KVM SNP/memory error enum was handled by
  [#4257](https://github.com/microsoft/openvmm/pull/4257).
- The earlier closed foundation PRs
  [#3669](https://github.com/microsoft/openvmm/pull/3669) and
  [#3710](https://github.com/microsoft/openvmm/pull/3710) were superseded by the
  merged loader/KVM series above; their review comments should not be counted as
  outstanding unless the same code or concern survived.
- The unrelated custom-DSDT cleanup raised during #3939 was handled separately
  by [#3954](https://github.com/microsoft/openvmm/pull/3954).
