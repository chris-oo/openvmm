# SNP/CCA upstream split findings

This branch is useful bring-up proof, but it is not ready to send upstream as a
single series. The current `openvmm-cca` line is clean and differs from `main`
by 56 files and about 8.8k added lines, including reusable abstractions, KVM
UAPI work, guest_memfd mechanics, SNP launch, CCA realm population, flowey
automation, root-level notes, and local repro scripts. The separate
`openvmm-snp` bookmark appears to contain the SNP subset; the current branch
contains that plus additional CCA work.

## High-level assessment

Some abstractions are directionally right:

- `virt::IsolationType` now includes `Snp`, `Tdx`, and `Cca`, with
  `is_hardware_isolated()` for generic code paths
  (`vmm_core/virt/src/generic.rs:91-116`).
- The loader-to-partition handoff now records imported page ranges, acceptance
  type, and debug tag (`vmm_core/vm_loader/src/lib.rs:37-80`,
  `vmm_core/vm_loader/src/lib.rs:112-157`).
- A hypervisor extension point for initial page work exists
  (`vmm_core/virt/src/generic.rs:369-379`), and both WHP and KVM can implement
  it with different back-end semantics (`vmm_core/virt_whp/src/lib.rs:504-520`,
  `vmm_core/virt_kvm/src/lib.rs:248-263`).

But the bring-up shape needs refactoring before upstream:

- The names `InitialAcceptedPage` and `AcceptInitialPages` are misleading for
  SNP/CCA. SNP launch update and CCA RMI populate are not guest page acceptance.
  Rename before upstreaming this API, e.g. `InitialPageImport` plus
  `CompleteInitialPageImports` or `PopulateInitialPages`.
- `vmm_core/virt_kvm/src/lib.rs` has become a mixed implementation file for
  memory slots, guest_memfd, SNP launch, CCA population, conversion/discard, and
  debug helpers. It even has a local TODO to split it up
  (`vmm_core/virt_kvm/src/lib.rs:266-267`).
- The SNP RAM completion workaround should not be upstreamed as normal
  behavior. It fabricates private launch pages for all remaining RAM until the
  loader provides complete metadata (`vmm_core/virt_kvm/src/lib.rs:686-690`,
  `vmm_core/virt_kvm/src/lib.rs:1007-1053`).
- CCA shared-device-memory aliasing is still an open design point. The dispatch
  path constructs an aliased `GuestMemory` but has a TODO to confirm Arm CCA
  pSMMU semantics (`openvmm/openvmm_core/src/worker/dispatch.rs:1174-1193`).
- CCA device-tree generation currently hardcodes shared IPA handling for serial
  MMIO instead of flowing the probed KVM shared alias value through the loader
  (`openvmm/openvmm_core/src/worker/vm_loaders/linux.rs:499-511`,
  `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs:561-573`).
- KVM CCA code currently maps SNP-named helper errors into CCA errors
  (`vmm_core/virt_kvm/src/lib.rs:1211-1261`). Shared private-memory helpers
  should use neutral names and typed errors.

## Recommended upstream sequence

### 1. Drop or quarantine bring-up artifacts

Do not include local root-level plans/scripts in feature PRs:

- `copy-snp-artifacts.sh`
- `run-snp-openvmm*.sh`
- `snp-debug-notes.md`
- exploratory `plan-*.md` and tracking docs

If any notes are useful long-term, convert them later into stable `Guide/`
documentation. Keep this first so reviewers see a small code series, not a
bring-up workspace dump.

### 2. KVM UAPI wrappers and dependency bump

Split the low-level `vm/kvm` bindings first, before changing `virt_kvm`
behavior. This should contain only new wrappers/constants/types for:

- guest_memfd and memory attributes;
- SNP VM type and launch ioctls;
- Arm RMI/CCA capability and populate/memory-fault ioctls.

This is also where the temporary `kvm-bindings` git dependency belongs if the
needed UAPI is unreleased (`Cargo.toml:608-610`). Keep the PR explicit about
why crates.io is insufficient and how the dependency will move back to a
release.

### 3. Generic isolation and platform shape

Land the cross-hypervisor enum/config shape with no functional SNP/CCA launch:

- `virt::IsolationType::{Snp,Tdx,Cca}` and conversion helpers;
- no-op/unsupported handling in WHP/HVF/MSHV/KVM backends;
- minimal `PlatformInfo` fields needed by later AArch64 topology work.

Avoid exposing new stable CLI behavior until a backend can run it. If a hidden
CLI option is necessary for development, call it experimental and keep
validation strict.

### 4. Loader metadata handoff

Refactor the loader metadata API before it becomes an upstream surface:

- rename `InitialAcceptedPage` / `AcceptInitialPages`;
- keep range, visibility, loader `BootPageAcceptance`, and tag;
- keep the `PartitionUnit` handoff before first run
  (`vmm_core/src/partition_unit.rs:288-297`,
  `vmm_core/src/partition_unit.rs:489-498`);
- add focused tests for duplicate range detection, metadata preservation, and
  non-isolated no-op behavior.

This PR should not require KVM SNP/CCA launch.

### 5. Memory backing improvements independent of confidential VMs

Keep host memory backing work separate from guest-private confidential memory:

- structured `--memory` parsing;
- private anonymous host backing;
- THP and hugetlb support;
- NUMA backing if it is independently useful.

Be precise in naming. `RamBackingRequest::private_memory()` means anonymous host
memory (`openvmm/membacking/src/memory_manager/mod.rs:221-247`,
`openvmm/membacking/src/memory_manager/mod.rs:430-445`), while KVM guest_memfd
private memory is a confidential-computing attribute.

### 6. KVM guest_memfd foundation

Add the shared KVM memory-slot machinery without SNP/CCA launch:

- `set_user_memory_region2` slot handling;
- per-slot guest_memfd ownership;
- memory-attribute setup/clear;
- classification that maps RAM through guest_memfd and non-RAM through normal
  userspace mappings (`vmm_core/virt_kvm/src/lib.rs:415-493`,
  `vmm_core/virt_kvm/src/lib.rs:1328-1354`);
- conversion discard helpers (`vmm_core/virt_kvm/src/lib.rs:582-638`).

Require tests for range classification, partial overlap rejection, slot cleanup,
and failure semantics. Today, discard failure happens after attributes are
changed and has no rollback; that must be an explicit behavior or fixed before
upstream (`vmm_core/virt_kvm/src/lib.rs:542-543`,
`vmm_core/virt_kvm/src/lib.rs:576-578`).

### 7. SNP Linux direct-boot loader groundwork

Split loader changes from KVM launch:

- x86 Linux direct boot additions for bzImage if needed;
- SNP secrets, CPUID, CC blob, setup-data pages
  (`vm/loader/src/linux.rs:323-390`);
- page-table import tagging for later C-bit patching
  (`vm/loader/src/linux.rs:618-626`);
- zero-page `setup_data` linkage (`vm/loader/src/linux.rs:651-667`).

This should be reviewable as Linux boot protocol support plus metadata, without
`/dev/sev` launch ioctls.

### 8. Minimal KVM SNP launch

Then add the KVM SNP implementation:

- SNP VM creation and `/dev/sev` handling
  (`vmm_core/virt_kvm/src/arch/x86_64/mod.rs:280-309`);
- hypercall exit support for `KVM_HC_MAP_GPA_RANGE`
  (`vmm_core/virt_kvm/src/lib.rs:496-545`);
- launch start/update/finish (`vmm_core/virt_kvm/src/lib.rs:641-785`);
- CPUID-page construction/sanitization and C-bit patching
  (`vmm_core/virt_kvm/src/lib.rs:1110-1209`);
- direct-Linux-only restrictions
  (`openvmm/openvmm_core/src/worker/dispatch.rs:1015-1107`).

Remove the RAM hack before upstream, or make it impossible to enable outside an
ignored/manual test.

### 9. AArch64 KVM platform cleanup independent of CCA

Land AArch64 KVM improvements that benefit non-CCA guests first:

- GICv3/GICv2/ITS probing and topology wiring;
- IPA-size probing;
- DT/ACPI cleanups that are not realm-specific.

Do not include CCA shared alias semantics here. Also audit upstream-hostile
placeholders in AArch64 KVM: current code still has `unimplemented!()` paths in
state access (`vmm_core/virt_kvm/src/arch/aarch64/mod.rs:448-458`) and should
return typed errors or be proven unreachable before broad upstreaming.

### 10. Minimal KVM CCA realm population

Add CCA-specific KVM behavior after the platform cleanup:

- CCA VM type/capability checks;
- initial private page population (`vmm_core/virt_kvm/src/lib.rs:788-871`);
- memory-fault/RIPAS conversion (`vmm_core/virt_kvm/src/lib.rs:547-580`);
- run gating until population completes
  (`vmm_core/virt_kvm/src/arch/aarch64/mod.rs:500-513`);
- direct Linux DT-only restrictions.

Keep the device model narrow and explain why. Rename `cca_shared_gpa_bit` to
something semantically clearer, such as `cca_shared_gpa_mask` or
`cca_shared_alias_base`, because the value is used as an alias offset/mask, not
just a bit.

### 11. CCA shared MMIO/device aliasing

Treat shared MMIO/device-memory aliasing as a follow-up design PR, not part of
minimal realm launch:

- flow the probed shared alias value into DT generation;
- remove hardcoded serial shared IPA handling;
- document expected pSMMU/device DMA semantics;
- add targeted tests for shared vs private device access.

This is likely the most design-sensitive abstraction in the branch.

### 12. Manifest/config validation cleanup

Move the large isolation-specific restrictions out of the central dispatch path
into smaller validation helpers or manifest/capability checks. The current block
is understandable for bring-up but will keep growing
(`openvmm/openvmm_core/src/worker/dispatch.rs:1015-1107`). Keep negative tests
for unsupported combinations such as disks, VMBus, VTL2, Hyper-V
enlightenments, CCA ACPI boot, and CCA unsupported devices.

### 13. OpenHCL/Underhill changes

The diff includes OpenHCL/Underhill wrapper changes. Either:

- drop them from this upstream series if they are collateral; or
- split them into their own PR with a clear dependency on the renamed initial
  page metadata trait.

Do not bury OpenHCL behavior changes inside a KVM SNP/CCA PR.

### 14. Flowey, preflight, and tests last, but not all validation last

Automation should follow the code it exercises:

- unit tests should land with each core PR;
- capability-gated/manual KVM tests can land with the relevant SNP/CCA backend
  PR;
- FVP/KVM CCA flowey staging, `kvm_cca_preflight`, and vmm-test YAML should
  come after the minimal code path is reviewable.

## Specific refactors to do before sending

1. Rename initial page APIs away from "accepted" terminology.
2. Split `virt_kvm/src/lib.rs` into modules, likely:
   - `memory.rs` for slot/guest_memfd/attributes/conversion;
   - `snp.rs` for launch page mapping and SNP-specific CPUID/VMSA handling;
   - `cca.rs` for RMI populate and RIPAS conversion;
   - keep generic partition plumbing in `lib.rs`.
3. Replace SNP-named private-range helper errors used by CCA with neutral
   errors.
4. Remove or isolate the SNP RAM hack.
5. Flow the probed CCA shared alias value into DT/device-memory construction.
6. Define rollback/failure semantics for KVM memory attribute conversion and
   guest_memfd slot setup.
7. Keep CLI exposure synchronized with actual backend support.

## Suggested review-sized PR stack

| Order | PR theme | Main files | Should be easy to review because |
| --- | --- | --- | --- |
| 1 | KVM UAPI wrappers | `vm/kvm/*`, `Cargo.toml` | mechanical bindings only |
| 2 | Isolation enum/platform shape | `vmm_core/virt/*`, backend stubs | no launch behavior |
| 3 | Initial page metadata rename/handoff | `vmm_core/vm_loader`, `vmm_core/partition_unit`, `openvmm_core/partition` | loader metadata only |
| 4 | Memory backing CLI/builder | `openvmm_entry`, `membacking` | host backing behavior only |
| 5 | KVM guest_memfd foundation | `virt_kvm` memory module | no SNP/CCA launch |
| 6 | SNP Linux loader support | `vm/loader`, `openvmm_core/vm_loaders/linux.rs` | Linux boot protocol only |
| 7 | KVM SNP launch | `virt_kvm` x86/SNP | one confidential backend |
| 8 | AArch64 KVM cleanup | `virt_kvm` aarch64, topology/DT | non-CCA platform work |
| 9 | KVM CCA minimal realm | `virt_kvm` aarch64/CCA | realm launch only |
| 10 | CCA shared MMIO/device aliasing | dispatch, DT, device memory | isolated design topic |
| 11 | Manifest/config validation cleanup | manifest builder, dispatch validation | mechanical policy extraction |
| 12 | Flowey/preflight/vmm tests | `flowey/*`, `vm/kvm/kvm_cca_preflight`, `vmm_tests/*` | automation for landed code |

## Practical jj splitting workflow

Build the upstream stack additively from `main` instead of squashing all bring-up
work and subtracting pieces out of it. A squashed branch is useful only as a
safety snapshot; using it as the working base makes it easy to accidentally
leave unrelated code in a review commit.

Recommended workflow:

1. Preserve the current bring-up state with a bookmark:

   ```bash
   jj bookmark create snp-cca-bringup-snapshot -r openvmm-cca
   ```

2. Start a fresh stack from `main`:

   ```bash
   jj new main
   jj bookmark create snp-cca-upstream-stack
   ```

3. For each logical PR/commit, restore only that slice from the snapshot:

   ```bash
   jj restore --from snp-cca-bringup-snapshot -- path/or/file
   jj diff
   # edit/split hunks until this commit is reviewable on its own
   cargo check -p <package>
   cargo clippy --all-targets -p <package>
   cargo doc --no-deps -p <package>
   cargo nextest run --profile agent -p <package>
   cargo xtask fmt --fix
   jj commit -m "area: describe logical change"
   ```

4. Repeat for the next stack entry, always making each commit buildable and
   reviewable before moving on.

Use the PR stack table above as the default order, but split further whenever a
commit starts mixing unrelated review concerns. Prefer fixups directly in the
commit being built over preserving bring-up history. Keep the snapshot bookmark
until the stacked commits fully reproduce the needed behavior.

## Review notes incorporated

An independent plan review rated this as "minor revisions" and called out these
changes, all incorporated above:

- split KVM UAPI wrappers before guest_memfd/SNP/CCA behavior;
- explicitly account for OpenHCL/Underhill and manifest/config changes;
- rename the initial page API before exposing it upstream;
- avoid early CLI exposure as a stable contract;
- clarify that the CCA shared GPA value is used as an alias mask/base;
- call out rollback risks in memory-attribute conversion and guest_memfd slot
  setup;
- remove the SNP RAM hack for upstream rather than merely renaming it;
- land tests with each PR, not only at the end.
