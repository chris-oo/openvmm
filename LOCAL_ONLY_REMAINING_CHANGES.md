# Local-Only Remaining Changes Audit

This note tracks non-doc, non-flowey changes still present in
`snp-cca-local-bringup-artifacts` relative to `snp-cca-upstream-stack`.
These changes came from restoring the old bring-up branch for local validation,
but some now look like stale bring-up residue after the upstream stack was
refactored.

The goal is to decide which items are still required for local CCA/SNP
validation, which should move into an upstream PR, and which should be dropped.

## Expected local-only changes

These are plausibly local validation infrastructure and should stay local-only
unless we decide to upstream the tooling:

| Area | Purpose |
| --- | --- |
| `.gitignore` | Allows checked-in local SNP helper scripts and ignores local kernel images. |
| `copy-snp-artifacts.sh`, `run-snp-openvmm.sh`, `run-snp-openvmm-repro.sh` | Local SNP copy/run/repro helpers. |
| `vm/kvm/kvm_cca_preflight/*` | Local CCA KVM host capability probe. |
| `Cargo.toml`, `Cargo.lock` | Workspace/package entries needed only because `kvm_cca_preflight` is local-only. |
| `flowey/**` CCA/FVP/KVM staging additions | Local FVP/CCA test staging infrastructure. |
| `vmm_tests/vmm_tests/test_data/kvm_cca_planes.yaml` | Local KVM CCA test plane data. |
| root `*.md` notes/plans | Bring-up tracking documents. |

## Suspicious source changes

These are non-doc, non-flowey source changes still present in the local-only
endpoint. They need review before we rely on them or keep them in the final
local-only commit.

| File | Suspicious behavior | Question |
| --- | --- | --- |
| `openvmm/openvmm_core/src/worker/dispatch.rs` | Changes guestmemfd/isolation validation and CCA device/shared-memory handling relative to the upstream stack. | Is this still needed for local CCA/FVP validation, or is it stale bring-up behavior that should be dropped or moved upstream? |
| `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs` | Appears to preserve/reintroduce local CCA shared-address handling, including older hard-coded shared IPA behavior in some hunks. | Should all CCA shared address handling now use the upstream platform-provided shared GPA bit instead? |
| `vmm_core/virt_kvm/src/cca.rs` | Local behavior skips shared CCA initial pages instead of rejecting them. Upstream PR9/PR10 direction was to reject unsupported shared initial imports until explicit conversion/alias handling exists. | Is skipping shared pages required for local FVP bring-up, or is it stale and unsafe? |
| `vmm_core/virt_kvm/src/memory.rs` | Local CCA memory-fault/private-attribute handling overlaps with upstream CCA shared-alias work and may duplicate or reorder code. | Is this extra behavior still required after PR10, or should it be moved/dropped? |
| `vmm_core/virt_kvm/src/lib.rs` | Local error enum/import changes include bring-up-only errors and ordering differences from upstream. | Are any remaining local errors actually used only by local tooling, or are they stale after the stack split? |
| `vmm_core/virt_kvm/src/arch/x86_64/mod.rs` | Local changes include additional SNP/SEV termination/hypercall handling and logging differences. | Are these still needed for SNP local validation, or should they already be upstreamed/dropped? |
| `vmm_core/virt_kvm/src/snp.rs` | Local SNP launch policy/comment differences remain. | Is the debug-capable SNP launch policy still intentionally local-only? |
| `vm/loader/src/linux.rs` | Local comments/API fallout around SNP CPUID page ownership and removed format behavior may duplicate upstream PR6 decisions. | Are any of these still local-only, or should the final local commit match upstream here? |
| `vm/loader/src/paravisor.rs` | Contains compatibility fallout from loader struct/API changes. | Should this be folded into the upstream loader/API commit instead of local-only? |
| `openhcl/underhill_core/src/loader/mod.rs` | Contains compatibility fallout from the Linux loader signature/API shape. | Should this be folded into the upstream loader/API commit instead of local-only? |
| `vmm_core/vm_manifest_builder/src/lib.rs` | Local manifest builder differences remain in the endpoint. | Determine whether these are CCA/FVP local hacks or upstreamable manifest-builder support. |
| `vmm_core/virt_kvm/Cargo.toml` | Local dependency/dev-dependency ordering/content differs. | Determine whether this is only due to local tests/tooling or should be folded into upstream commits. |

## Current concern

The local-only endpoint should ideally contain only:

1. Local validation scripts.
2. Local preflight/FVP/flowey staging.
3. Local test data and notes.

Any change to runtime behavior in core crates should be treated with suspicion.
It either belongs in one of the upstream PR bookmarks or should be removed from
the local-only endpoint unless it is explicitly required for local CCA
validation.

## Validation results

The suspicious runtime changes were tested by first reverting them to the
`snp-cca-upstream-stack` contents, then restoring only the groups required for
the repros.

| Commit | Files | Result |
| --- | --- | --- |
| `local: drop stale runtime bring-up changes` | Reverts the suspicious runtime files to the upstream stack as a baseline. | Builds with the Underhill API compatibility fix retained, but SNP hangs during early TSC calibration. |
| `local: keep SNP loader direct-boot fix` | `vm/loader/src/linux.rs` | Split from the SNP runtime group; part of the required local SNP validation set. |
| `local: keep SNP launch policy fix` | `vmm_core/virt_kvm/src/snp.rs` | Split from the SNP runtime group; part of the required local SNP validation set. |
| `local: keep SEV termination handling` | `vmm_core/virt_kvm/src/arch/x86_64/mod.rs`, `vmm_core/virt_kvm/src/lib.rs` | Split from the SNP runtime group; keeps only SEV termination system-event handling and the matching error. |
| `local: keep CCA Linux direct-boot address fallback` | `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs` | Split from the OpenVMM runtime group; part of the remaining local CCA/SNP validation set. |
| `local: keep OpenVMM guestmemfd validation and device wiring` | `openvmm/openvmm_core/src/worker/dispatch.rs` | Split from the OpenVMM runtime group; part of the remaining local CCA/SNP validation set. |
| `local: keep manifest builder direct-boot shape` | `vmm_core/vm_manifest_builder/src/lib.rs` | Required for the current SNP repro. Removing this file's local delta causes the same early TSC calibration hang. |

The following suspicious files were tested and found not to be required for the
current SNP/CCA repros, so their local-only deltas were dropped:

- `vmm_core/virt_kvm/src/memory.rs`
- `vm/loader/src/paravisor.rs`
- `vmm_core/virt_kvm/Cargo.toml`
- `vmm_core/virt_kvm/src/cca.rs`

Both `run-snp-openvmm-repro.sh` and `run-cca-openvmm-repro.sh` pass with the
remaining split local-runtime commits.
