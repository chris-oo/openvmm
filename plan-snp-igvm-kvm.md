# Plan: KVM SEV-SNP IGVM generation and launch

## Goal

Add an initial, KVM-focused SEV-SNP IGVM path that can generate and boot the same
minimal Linux guest OpenVMM currently boots directly from command-line inputs:
a bzImage kernel, optional initrd, and command line. The generated IGVM should
carry complete initial acceptance metadata for guest RAM so KVM does not need to
synthesize missing RAM during SNP launch. OpenVMM must never use the existing KVM
RAM hack when loading from IGVM.

This is intentionally scoped as a temporary x86_64/KVM/SNP direct-Linux mode,
not a fully general SNP IGVM firmware/paravisor model.

## Current state

- OpenVMM direct Linux boot already accepts kernel/initrd command-line inputs and
  supports `LinuxKernelFormat::BzImage`.
- The x86 Linux loader imports the direct-boot GDT, page tables, command line,
  ACPI tables, zero page, kernel, initrd, and SNP boot protocol pages at fixed
  addresses in `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs`.
- `loader::linux::load_x86` already handles both ELF and bzImage kernels and
  imports SNP secrets/CPUID/CC blob pages when given an `SnpBootConfig`.
- `igvmfilegen` already has SNP platform support: it can emit a SEV-SNP platform
  header, guest policy, SNP page-data types, `SnpVpContext`, and measurement
  material.
- OpenVMM's runtime IGVM loader is currently VBS-oriented:
  - `read_igvm_file` parses with `igvm::IsolationType::Vbs`.
  - platform selection only accepts `IgvmPlatformType::VSM_ISOLATION`.
  - `IgvmPageDataType` handling only supports `NORMAL`.
  - `SnpVpContext`, `SnpIdBlock`, and `X64NativeVpContext` are unimplemented.
- KVM SNP launch currently fills any RAM not supplied by the loader using
  `snp_launch_pages_with_ram_hack`, producing `"kvm-snp-ram-hack"` page ranges.
  This must remain only as a direct-Linux compatibility fallback.

## Design decisions to settle before broad plumbing

1. **VP context representation**

   The generated SNP IGVM should use the IGVM `SnpVpContext` directive and
   describe the initial VMSA state. OpenVMM should not switch the SNP IGVM to a
   native x64 VP context just to match the current direct-Linux implementation.

   KVM's SNP UAPI does not expose `KVM_SEV_SNP_PAGE_TYPE_VMSA` through
   `struct kvm_sev_snp_launch_update`: the public page types are normal, zero,
   unmeasured, secrets, and CPUID. In the local Linux tree, VMSAs are measured
   during `KVM_SEV_SNP_LAUNCH_FINISH`: `snp_launch_finish()` calls
   `snp_launch_update_vmsa()`, which syncs each KVM vCPU's internal VMSA with
   `sev_es_sync_vmsa()` and issues firmware `SEV_CMD_SNP_LAUNCH_UPDATE` with
   `SNP_PAGE_TYPE_VMSA`.

   Therefore the OpenVMM side of "import the VMSA page described by IGVM" should
   mean: parse `SnpVpContext`, translate the VMSA fields into KVM vCPU register
   state before launch finish, and verify that KVM's internally generated VMSA
   will match the IGVM-provided VMSA for the fields KVM owns. If there are VMSA
   fields that cannot be represented through KVM vCPU state, identify the needed
   KVM UAPI extension or reject the IGVM with a typed error. Do not silently
   replace `SnpVpContext` with `X64NativeVpContext`.

   QEMU follows this model for IGVM SNP: `backends/igvm.c` passes IGVM VP
   context data to confidential guest support as `CGS_PAGE_TYPE_VMSA`;
   `target/i386/sev.c` validates the VMSA, stores it as launch CPU context, and
   copies supported VMSA fields into CPU state in `sev_apply_cpu_context()`.
   QEMU's comment notes that directly providing the VMSA to KVM would be ideal,
   but current KVM does not expose that, so userspace must synchronize supported
   VMSA fields into vCPU state and let KVM measure its internal VMSA.

2. **Linux page-table C-bit handling**

   KVM direct Linux SNP launch currently detects pages tagged
   `"linux-pagetables"` and sets the SNP C-bit in those page tables immediately
   before launch. IGVM page-data directives do not preserve loader debug tags in
   a way KVM can rely on.

   For this temporary mode, make the generated IGVM host-specific: `igvmfilegen`
   should take the SNP C-bit position as configuration and emit final Linux
   direct-boot page tables with the C-bit already set. OpenVMM should not apply
   runtime page-table C-bit fixups when loading from IGVM.

   This means the generated IGVM and its measurement are only valid for hosts
   with the same C-bit position. OpenVMM should validate the runtime SNP C-bit
   against the C-bit recorded in the IGVM/manifest metadata and fail clearly if
   they differ. The existing direct-Linux path can keep its runtime C-bit patch
   until that path is removed or converted.

3. **Complete initial acceptance semantics**

   "Complete" means the loader/generator has supplied all pages KVM must include
   in SNP launch updates for initial private guest RAM. For IGVM this should be a
   hard contract: if coverage is incomplete, fail before launch rather than
   silently applying the KVM RAM hack.

   Complete acceptance must exclude MMIO holes and shared pages, account for
   already imported pages, and validate 4K alignment and overlap. The generator
   should take an explicit memory layout description: RAM ranges plus MMIO holes
   needed to build the Linux zero-page/e820 data. OpenVMM must validate at load
   time that the IGVM-declared RAM layout matches the runtime `MemoryLayout`
   before treating acceptance as complete.

4. **Shared SNP pages**

   The temporary supported subset should reject shared SNP IGVM pages unless and
   until KVM launch has an explicit path for them. Do not pass shared pages into
   SNP `LAUNCH_UPDATE`, since KVM's current page-type mapping has no launch
   update type for `BootPageAcceptance::Shared`.

## Implementation steps

### 1. Define the temporary generated-IGVM contract

Document the expected generated file shape in code comments and tests before
threading it through all layers:

- SEV-SNP platform header.
- x86_64 VTL0-only guest (`max_vtl = 0`) for the initial mode.
- bzImage kernel, optional initrd, command line.
- Same direct-boot addresses as OpenVMM's Linux loader for GDT, CR3/page
  tables, zero page, command line, ACPI, SNP secrets, SNP CPUID, SNP CC blob,
  SNP CC setup data, and kernel start.
- SNP page-data types for secrets and CPUID.
- `SnpVpContext` VMSA directive for the BSP/initial VP state.
- Complete initial private RAM acceptance directives generated from the explicit
  memory layout.
- Host-specific Linux page tables with the configured SNP C-bit already set.
- Metadata recording the C-bit position used to generate the IGVM, so OpenVMM
  can validate it against the runtime host.

### 2. Add igvmfilegen config for KVM SNP direct Linux

Modify `vm/loader/igvmfilegen_config/src/lib.rs`:

- Add a temporary image variant such as `kvm_snp_linux_direct`.
- Include:
  - `use_initrd`
  - `command_line`
  - kernel format, initially restricted to `bz_image`
  - SNP C-bit position for host-specific page-table generation
  - explicit memory layout: RAM ranges and MMIO holes sufficient to build the
    Linux zero-page/e820 data and to define "accept the rest of guest RAM"
- Prefer explicit layout over memory-size-only configuration so the IGVM
  manifest must match the runtime memory layout.

Update resource requirements so this image requests the Linux kernel and,
optionally, initrd resources.

### 3. Generate the KVM SNP direct-Linux IGVM

Modify `vm/loader/igvmfilegen/src/main.rs` and likely add a small helper module:

- Reuse `loader::linux::load_x86` rather than the existing `Image::Linux` path,
  because the existing path only loads kernel/initrd and ignores command line.
- Use the same constants as `openvmm_core`'s x86 Linux loader for direct boot.
- Choose one ACPI strategy before implementation:
  - preferred for matching OpenVMM runtime behavior: add IGVM parameter areas for
    RSDP/ACPI table insertion and have OpenVMM populate them during IGVM load; or
  - strictly temporary alternative: generate static ACPI for a fixed topology and
    validate at runtime that CPU topology, memory layout, and MMIO layout match.
  Do not silently omit ACPI if the direct boot path relies on it.
- Enable `SnpBootConfig` so the generator emits secrets, CPUID, CC blob, and CC
  setup-data pages.
- Ensure the generated VP context follows the decision from step 1.
- Add a host-specific page-table generation mode that sets the configured SNP
  C-bit in the direct-boot page tables before they are emitted into the IGVM.
  The resulting IGVM measurement is tied to that C-bit position.

### 4. Add explicit RAM-gap acceptance generation

Modify `vm/loader/igvmfilegen/src/file_loader.rs`:

- Add a method that accepts the configured memory layout and emits private
  acceptance metadata for every not-yet-imported RAM page.
- Subtract already imported/accepted ranges from each RAM range.
- Reject unaligned, overlapping, zero-length, or MMIO/shared ranges.
- Prefer a compact range-style IGVM representation if the IGVM format/library
  supports one. If not, emit empty `PageData NORMAL` with private/measured
  acceptance for the gaps and explicitly scope the first implementation to tiny
  test guests because directive count scales one-per-page.
- Add unit tests for gap computation, overlap rejection, alignment rejection, and
  already-imported pages.

### 5. Teach OpenVMM to parse SNP IGVMs

Modify `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs`:

- Make `read_igvm_file` isolation-aware, or parse without hard-coding VBS when
  the VM isolation mode is SNP.
- Add SNP platform-header selection for `IgvmPlatformType::SEV_SNP`.
- Preserve existing VBS/OpenHCL behavior.
- Map SNP page-data types:
  - `NORMAL` + measured/unmeasured/shared flags to existing acceptance types.
  - `SECRETS` to `BootPageAcceptance::SecretsPage`.
  - `CPUID_DATA` to `BootPageAcceptance::CpuidPage`.
  - `CPUID_XF` either to `CpuidExtendedStatePage` if KVM support is added, or
    reject with a typed unsupported-feature error.
  - shared pages should be rejected for the temporary KVM SNP subset unless an
    explicit non-launch-update handling path is implemented.
- Implement `SnpVpContext` handling:
  - parse the IGVM-provided VMSA for each VP.
  - translate representable VMSA fields into `X86Register`/KVM vCPU state before
    `KVM_SEV_SNP_LAUNCH_FINISH`.
  - compare or validate fields that KVM will synthesize internally.
  - reject VMSA fields that cannot be represented through current KVM UAPI rather
    than dropping them.
- Treat `SnpIdBlock` explicitly. If launch-finish metadata is not supported in
  this initial mode, reject signed/ID-block IGVMs with a clear error rather than
  `todo!()` or ignoring them.
- Convert SNP-path `todo!()`, `panic!()`, `assert!()`, and `expect()` cases that
  can be reached from malformed IGVM input into typed errors. IGVM files are
  untrusted input.

### 6. Add complete-acceptance and launch-metadata plumbing

Modify:

- `vmm_core/virt/src/generic.rs`
- `vmm_core/src/partition_unit.rs`
- `openvmm/openvmm_core/src/partition.rs`
- `openvmm/openvmm_core/src/worker/dispatch.rs`
- `vmm_core/virt_kvm/src/lib.rs`

Add a carrier around initial accepted pages, for example:

```rust
pub struct InitialAcceptedPages {
    pub pages: Vec<InitialAcceptedPage>,
    pub is_complete: bool,
}
```

Use it as follows:

- Direct Linux load: `is_complete = false`, preserving the existing direct-Linux
  runtime page-table C-bit patching behavior for now.
- IGVM load: `is_complete = true`, with no runtime page-table C-bit patching.
- KVM SNP launch:
  - apply `snp_launch_pages_with_ram_hack` only when `is_complete == false`.
  - never apply the hack for IGVM.
  - do not apply SNP page-table C-bit fixups for IGVM; the page tables are
    already final in the IGVM.
  - when `is_complete == true`, validate coverage against KVM RAM ranges and
    fail with a clear error if pages are missing.
- OpenVMM runtime must compare the IGVM-declared memory layout against the actual
  VM `MemoryLayout` before setting `is_complete = true`.
- OpenVMM runtime must compare the IGVM-declared/generated C-bit position against
  runtime KVM CPUID before launch.

### 7. Relax SNP load-mode gates only for the supported IGVM subset

Modify:

- `openvmm/openvmm_entry/src/lib.rs`
- `openvmm/openvmm_core/src/worker/dispatch.rs`

Allow `LoadMode::Igvm` when SNP isolation is enabled, but keep existing
rejections for Hyper-V enlightenments, VTL2, unsupported devices, and disks
unless separately implemented. Add clear errors if the IGVM does not match the
temporary supported subset.

Update user-visible docs or add an example manifest for the new `igvmfilegen`
mode. If documentation is intentionally deferred, state that in the PR and keep
the config marked experimental.

### 8. Tests

Add or update tests for:

- `igvmfilegen_config` parsing of the new image and RAM acceptance fields.
- RAM-gap acceptance generation:
  - fills gaps
  - skips imported pages
  - rejects overlap and unaligned ranges
  - does not include MMIO/shared ranges.
- Generated SNP IGVM shape:
  - SEV-SNP platform header
  - expected SNP page-data types
  - page tables include the configured SNP C-bit
  - metadata records the configured SNP C-bit
  - complete RAM acceptance directives.
  - compact acceptance behavior or explicit tiny-guest directive-count limits.
- OpenVMM IGVM loader:
  - parses SEV-SNP IGVM
  - maps `SECRETS` and `CPUID_DATA`
  - rejects unsupported `CPUID_XF`/`SnpIdBlock` if not implemented
  - preserves VBS IGVM behavior.
  - rejects shared SNP pages for this temporary subset.
- KVM SNP launch preparation:
  - direct Linux still applies RAM hack
  - IGVM complete acceptance never applies `"kvm-snp-ram-hack"`
  - IGVM incomplete coverage fails
  - IGVM launch fails if runtime SNP C-bit differs from the generated IGVM C-bit.
  - runtime memory layout mismatches fail before SNP launch.
- End-to-end manual validation on KVM SNP:
  - generate bzImage+initrd IGVM
  - boot it via `LoadMode::Igvm`
  - confirm guest reaches the same point as direct Linux boot
  - confirm logs show no KVM RAM hack for IGVM.

### 9. Validation commands

For modified packages, run package-scoped checks:

```bash
cargo check -p igvmfilegen_config
cargo clippy --all-targets -p igvmfilegen_config
cargo doc --no-deps -p igvmfilegen_config
cargo nextest run --profile agent -p igvmfilegen_config

cargo check -p igvmfilegen
cargo clippy --all-targets -p igvmfilegen
cargo doc --no-deps -p igvmfilegen
cargo nextest run --profile agent -p igvmfilegen

cargo check -p openvmm_core
cargo clippy --all-targets -p openvmm_core
cargo doc --no-deps -p openvmm_core
cargo nextest run --profile agent -p openvmm_core

cargo check -p virt
cargo clippy --all-targets -p virt
cargo doc --no-deps -p virt
cargo nextest run --profile agent -p virt

cargo check -p virt_kvm
cargo clippy --all-targets -p virt_kvm
cargo doc --no-deps -p virt_kvm
cargo nextest run --profile agent -p virt_kvm

cargo xtask fmt --fix
```

Run formatting last.

Include `openvmm_entry` in the same check/clippy/doc/test cycle if CLI/config
validation changes land there.

## Open questions for feedback

1. Should the temporary IGVM mode use static ACPI page data, or should it define
   IGVM parameter areas so OpenVMM can insert runtime-generated ACPI?
2. Which IGVM `SnpVpContext` VMSA fields must be bit-exactly reflected in KVM's
   internal VMSA, and which are expected to be synthesized or normalized by KVM
   during `KVM_SEV_SNP_LAUNCH_FINISH`?
3. Should signed SNP ID blocks be out of scope initially, with `SnpIdBlock`
   rejected clearly, or should launch-finish metadata be part of the first
   implementation?
4. Should RAM acceptance ranges be specified explicitly in the manifest only, or
   should igvmfilegen also support an OpenVMM-memory-layout helper for local
   convenience?
5. Is one-page-per-4K empty `PageData` acceptable for the first tiny test
   guests, or should compact range acceptance be mandatory before implementation?
6. How should the generated C-bit position be recorded for runtime validation:
   IGVM metadata, map/sidecar plus command-line assertion, or a temporary
   OpenVMM-private parameter?

## Review

Initial plan review verdict: Needs rework.

The review identified VP context handling, Linux page-table C-bit handling,
SNP-specific page-data mapping, runtime data needed by `load_x86`, and complete
RAM acceptance semantics as blockers rather than minor risks. This revised plan
promotes those issues to explicit design decisions and implementation steps,
chooses host-specific generated page tables with a configured SNP C-bit,
requires explicit SNP page-data handling in the OpenVMM IGVM loader, and makes
incomplete IGVM RAM acceptance a launch-time error instead of falling back to the
KVM RAM hack.

Second plan review verdict: Minor revisions.

The re-review called out remaining specifics around where page-table C-bit
metadata lives, possible IGVM directive-count explosion for accepting RAM,
ACPI/runtime topology matching, full memory-layout input, shared SNP page
handling, and untrusted IGVM error handling. This version tightens those points:
page tables are generated with a configured host-specific SNP C-bit and validated
against runtime CPUID, memory layout must be declared and validated, shared pages
are rejected for the temporary subset, one-page-per-4K acceptance is explicitly
limited to tiny guests unless a compact encoding exists, and SNP IGVM parsing
must return typed errors for malformed or unsupported input.
