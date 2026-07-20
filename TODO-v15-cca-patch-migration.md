# KVM CCA v15 Patch Migration

Do not start this migration until the current KVM CCA v14 environment remains
available as a known-good baseline.

## Linux patch stack

- [ ] Preserve the current `/home/coo/ai/eevee/linux` working copy before
  changing revisions. It is a jj repository and has an uncommitted modification
  to `build-cca-fvp-kernels.sh`; do not discard, overwrite, or amend that work.
- [ ] Create new jj commits from the required base with `jj new`; use jj only
  and do not run git commands in the repository.
- [ ] Build the v15 kernel stack in this order:
  1. Linux `v7.2-rc1`.
  2. Ackerley Tng's guest_memfd in-place conversion v8 series,
     `<20260618-gmem-inplace-conversion-v8-0-9d2959357853@google.com>`.
  3. Suzuki K Poulose's required guest_memfd fix,
     `<114e2488-97ed-4740-a8e8-1edd991f26c5@arm.com>`.
  4. The generic RMM firmware support series,
     `<20260715142739.80398-1-steven.price@arm.com>`.
  5. KVM CCA v15,
     `<20260715142841.80544-1-steven.price@arm.com>`.
- [ ] Compare the resulting tree with `cca-host/v15` and record immutable commit
  IDs for the base and every prerequisite series. Also record the source message
  IDs and patch hashes, including how any conflicts were resolved.
- [ ] Use a new v15 kernel output directory and force configuration regeneration
  for the first build. Do not reuse the v14 `.config`, output directory, staged
  kernel, or build manifest.
- [ ] Build the complete patch stack and run an aarch64 kernel build/preflight
  checkpoint before changing OpenVMM.
- [ ] Keep Linux-tree changes separate from OpenVMM changes. If the local kernel
  helper needs updating, preserve the user's existing
  `build-cca-fvp-kernels.sh` edits and commit any additional helper changes
  separately.

## guest_memfd in-place conversion

The v15 kernel series is based on guest_memfd in-place v8, but OpenVMM does not
currently use its in-place conversion interface. OpenVMM supplies separate
userspace/shared and guest_memfd/private backing, changes private state through
the VM memory-attributes ioctl, and discards the stale backing after each
conversion.

- [ ] Treat guest_memfd in-place v8 as a required kernel prerequisite even if
  OpenVMM initially continues using its existing two-backing model.
- [ ] Separately evaluate adopting the in-place interface in OpenVMM:
  - guest_memfd-fd `KVM_SET_MEMORY_ATTRIBUTES2` capability and ioctl handling;
  - guest_memfd creation flags and `INIT_SHARED` versus initial-private
    behavior;
  - mapping the guest_memfd as the shared userspace view and handling `SIGBUS`
    for pages currently marked private;
  - ordering and failure semantics for shared/private conversion;
  - whether `discard_stale_private_memory_backing`, `MADV_DONTNEED`, and
    guest_memfd hole punching can be removed;
  - memory-slot setup, unmap, restore, and inspection behavior;
  - SNP `KVM_SEV_SNP_LAUNCH_UPDATE` behavior now that its source userspace
    address may be optional;
  - CCA initial population, runtime RIPAS changes, and whether Realm memory
    contents must still be copied from a separate unprotected source;
  - page pinning, concurrent mappings, conversion safety, and partial-range
    failure behavior.
- [ ] Decide explicitly whether v15 bring-up will:
  1. retain the current two-backing OpenVMM implementation, or
  2. migrate SNP and CCA to guest_memfd in-place conversion.
- [ ] Default the initial v15 bring-up to retaining the two-backing
  implementation. Treat adoption of in-place conversion as a separately
  approved follow-up so it cannot block basic v15 CCA validation.
- [ ] If adopted, implement the in-place conversion work as a dedicated
  `virt_kvm` commit. Do not mix it with kernel, Flowey, overlay, or firmware
  revision updates.
- [ ] Add focused unit tests for capability detection, initial attributes,
  conversion requests, slot lifecycle, and errors. Retain regression coverage
  for the existing SNP zero-page measurement and CCA population semantics.

## Firmware and companion tools

- [ ] Update the environment for RMM v2.0-bet2.
- [ ] Pin TF-RMM to the tested `topics/rmm-v2.0-poc_3` revision by immutable
  commit ID.
- [ ] Determine and pin the compatible TF-A revision; do not assume the v14
  TF-A pin remains correct.
- [ ] Update any RMM/TF-A build options needed for v2.0-bet2, Stateful RMI
  Operations, explicit SHA-256 selection, and the new Realm parameter layout.
- [ ] Update kvm-unit-tests from the v15-recommended `cca/v4` branch only if it
  is used by validation, and pin it by immutable commit ID.
- [ ] Do not build kvmtool merely because the cover letter references
  `cca/v13`. OpenVMM does not require kvmtool for its CCA smoke test; remove or
  disable that Shrinkwrap component unless a concrete validation step needs it.

## OpenVMM `virt_kvm` audit

- [ ] Update the `kvm-bindings` source/version as needed and audit the
  hand-written ARM RMI constants, `KvmArmRmiPopulate`, and ioctl definition in
  `vm/kvm/src/lib.rs` against the final v15 UAPI. Verify the C layout, ioctl
  number, capability value, and input/output semantics rather than copying
  constants speculatively.
- [ ] Update `vm/kvm/kvm_cca_preflight` for all new or changed v15 capabilities
  and ABI expectations, cross-build it for aarch64, and run it on the FVP host
  before attempting an OpenVMM boot.
- [ ] Audit the CCA UAPI and call flow, including:
  - `KVM_CAP_ARM_RMI` and Realm VM creation;
  - `KVM_ARM_RMI_POPULATE` input/output progress semantics, flags, range
    overflow handling, and locking expectations;
  - removal of the old PSCI-completion ioctl flow;
  - initial Realm population and first-vCPU-run activation;
  - memory-fault exits and private/shared attribute transitions;
  - VGIC, timer, SVE, register-list, abort-injection, and host-call behavior.
- [ ] Make any required `virt_kvm` changes in a separate commit so that commit
  can be folded into the KVM CCA bookmark independently of local tooling.
- [ ] Keep trust-boundary handling strict: no panic, unwrap, or unchecked
  arithmetic on kernel- or guest-provided ranges and progress values.
- [ ] Test zero-length, unaligned, overflowing, partially completed, and
  kernel-rejected `KVM_ARM_RMI_POPULATE` requests, including observable state
  after failure.
- [ ] Test failed private/shared attribute transitions and stale-backing cleanup
  even when retaining the current two-backing implementation.

## Flowey and local CCA environment

- [ ] Update the OpenVMM-owned Shrinkwrap overlay to the final immutable v15
  Linux, TF-A, TF-RMM, and optional test-tool revisions.
- [ ] Keep the Flowey/overlay update in a separate local-only commit from all
  `virt_kvm` changes.
- [ ] Continue pinning the Shrinkwrap source revision and matching container
  image together.
- [ ] Ensure a clean machine can rebuild the environment without relying on
  previously built firmware or source checkouts.
- [ ] Keep the pinned v14 recipe and artifact path available as a rollback and
  comparison baseline.
- [ ] Advance `snp-cca-upstream-and-local` only after the v15 environment and
  runtime validation pass, in this order: isolated kernel/firmware artifacts,
  kernel and preflight validation, separate `virt_kvm` commit if needed,
  separate Flowey/overlay commit, then runtime smoke and regressions.

## Validation

- [ ] Re-run the known-good v14 CCA smoke repro before replacing staged
  artifacts.
- [ ] Build the complete v15 kernel and firmware environment from clean source
  directories.
- [ ] Cross-build every affected CCA package and `kvm_cca_preflight` for
  aarch64; normal host builds do not compile all aarch64-only code.
- [ ] Boot OpenVMM CCA on FVP and pass the block and network smoke checks.
- [ ] Exercise initial population, runtime private/shared transitions, repeated
  transitions, and ranges at RAM-slot boundaries.
- [ ] If guest_memfd in-place conversion is adopted, test shared mappings,
  private-page `SIGBUS`, conversion of allocated and unallocated pages,
  conversion with elevated page references, partial failures, and cleanup.
- [ ] Re-run KVM SNP direct-boot block/network smoke tests using an x86_64 build
  from the same kernel source patch stack on SNP-capable hardware. The arm64 FVP
  kernel cannot provide this regression coverage.
- [ ] Run the modified OpenVMM packages through check, clippy, rustdoc, unit
  tests, and workspace formatting before each commit.

## Review

Plan review verdict: **Minor revisions**. The review confirmed the prerequisite
order, current OpenVMM backing model, commit separation, Linux working-copy
preservation, and removal of unnecessary kvmtool builds. Its requested
revisions are incorporated above: isolated v15 artifacts, reproducible patch
provenance, two-backing as the default bring-up path, explicit binding and
preflight work, architecture-specific validation, stronger failure-path tests,
and an ordered release gate with a v14 rollback.
