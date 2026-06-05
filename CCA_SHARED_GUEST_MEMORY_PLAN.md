# CCA Shared Guest Memory Plan

## Problem

CCA guests put the shared IPA bit on DMA addresses for shared memory. OpenVMM
already strips this bit for KVM MMIO exits, but emulated devices still receive
the raw guest-provided DMA address. Virtio then tries to access addresses such
as `0x800000f0a000` through the normal `GuestMemory` view and fails because RAM
is mapped at the lower GPA (`0xf0a000`).

OpenHCL handles this by giving emulated devices a shared guest-memory view that
maps shared pages both below and above VTOM. OpenVMM should use the same layering:
devices stay generic, and CCA-specific address aliasing is handled by the memory
view passed to devices.

## Proposed implementation

1. Update the generated CCA FVP OpenVMM run script first so the test path
   exercises the behavior this change is meant to fix:
   - default `--openvmm-memory` to `256M`;
   - keep PL011 only as early console with
     `earlycon=pl011,mmio32,0x8000effec000`;
   - make virtio-console the main guest console with `console=hvc0`;
   - expose virtio-console over PCIe root-port plumbing, for example
     `--pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255,low_mmio=4M,high_mmio=1G`,
     `--pcie-root-port rc0:console`, and
     `--virtio-console-pcie-port console`;
   - use `--virtio-console console` so guest virtio-console output also appears
     on the OpenVMM console.
   - before committing this script/test-path change, run the FVP flow and confirm
     it reproduces the current expected failure: virtio queue setup should fail
     on a DMA address with the CCA shared IPA bit set. Only then commit this as
     the baseline repro chunk and move on to the shared-memory fix.
2. Add a `GuestMemoryClient` helper in `openvmm/membacking` that creates an
   aliased shared view:
   - `GuestMemory::new_multi_region("shared-ram", shared_bit, vec![Some(mapper),
     Some(mapper)])`;
   - both regions use `mapping_manager.new_mapper().await?`, so lower GPA and
     `gpa | shared_bit` resolve to the same backing offsets.
   - handle both `VaMapperError` and `guestmem::MultiRegionError` with typed
     errors or explicit context at the OpenVMM call site.
3. Expose the CCA shared IPA bit to `openvmm_core`:
   - KVM already computes it from Realm IPA size as `1u64 << (ipa_size - 1)`;
   - plumb it through an aarch64 platform/capability field such as
     `virt::PlatformInfo::shared_gpa_bit: Option<u64>` rather than
     duplicating the KVM IPA-size query in OpenVMM core.
   - add an in-code TODO at the new shared-bit plumbing to revisit whether this
     should be unified with SNP's existing `vtom`/shared-GPA-boundary model
     instead of leaving CCA and SNP on separate abstractions.
4. In `InitializedVm::new_with_hypervisor`, build two memory views for CCA:
   - `gm`: the existing lower-GPA view used for loading, initial page
     population, partition setup, and vCPU/partition access;
   - `device_gm`: the aliased shared view used for untrusted emulated devices.
   - store or plumb `device_gm` alongside `gm` so it remains alive until device
     construction; `InitializedVm` should retain any memory view used by devices.
   - add an in-code TODO near this CCA device-memory alias creation to revisit
     the allowed DMA aliases after confirming Arm CCA pSMMU behavior, and to
     update the implementation to match that behavior.
5. Pass `device_gm` to PCIe/virtio device construction paths, especially the
   `build_pcie_device(..., guest_memory, ...)` call used by CCA-supported virtio
   devices on PCIe root ports.
6. Keep virtio and PCIe device code unaware of CCA. Devices should continue to
   use `GuestMemory` normally; only the VM construction layer chooses the correct
   memory view.

## Invariants and scope

- Only create the aliased device view for `IsolationType::Cca`.
- Validate that the shared bit is nonzero and power-of-two, and that the guest
  RAM layout fits below it (`max_addr <= shared_bit`) before calling
  `GuestMemory::new_multi_region`.
- Keep loader, partition setup, initial page population, and `PartitionUnit` on
  the original lower-GPA `gm`. The aliased view is for emulated device DMA only.
- Scope the first fix to in-process emulated virtio devices. Passing a
  `new_multi_region` view to devices that require `GuestMemorySharing` may break
  vhost-user/shared-memory setup because the aliased view may not expose sharing
  metadata.
- The TODO below assumes the CCA configuration used by this path has emulated
  DMA backed by the shared userspace VA view, not `membacking` private anonymous
  memory. Revisit before enabling this path with different CCA memory-backing
  modes.
- Before making the aliasing contract permanent, check which DMA aliases devices
  should be allowed to use: lower IPA, shared/high IPA, or both. Match the pSMMU
  behavior required by Arm CCA rather than inventing an OpenVMM-specific policy.

## Validation

- Re-run the CCA FVP flow with virtio-console on a PCIe root port:
  - `--com1 none`;
  - `--virtio-console stderr --virtio-console-pcie-port console`;
  - `--cmdline "console=hvc0"`;
  - no `earlycon`.
- Confirm the virtio queue setup no longer fails on addresses with the shared
  IPA bit set.
- Confirm the guest reaches the virtio console or fails later for an unrelated
  reason.
- Add a small aliasing test or smoke check that access through `gpa` and
  `gpa | shared_bit` hits the same backing, while unbacked alias addresses fail
  cleanly.
- Run scoped validation for modified packages:
  - `cargo check -p membacking`;
  - `cargo clippy --all-targets -p membacking`;
  - `cargo doc --no-deps -p membacking`;
  - plus the equivalent checks for any OpenVMM core package touched;
  - `cargo xtask fmt --fix` last.

## TODO

- Add an explicit check that emulated devices cannot access private pages through
  the device memory view. This can be follow-up work for now: unlike OpenHCL,
  OpenVMM is outside the guest TCB, and emulated devices cannot actually DMA into
  private memory because the mapped VA is always for the shared address.
- Investigate Arm pSMMU requirements for Realm DMA address interpretation and
  document whether emulated devices should accept only high/shared IPA DMA, only
  low IPA DMA after translation, or both aliases. Update the implementation to
  match pSMMU semantics.

## Review

Verdict: minor revisions.

The review agreed that the shared `GuestMemory` alias is the right layering and
that fixing the `build_pcie_device(..., gm, ...)` path should address the virtio
queue failure. It called out implementation details to make explicit: keep
`device_gm` alive through device construction, handle both mapper and
multi-region errors, validate shared-bit/layout invariants, expose the shared bit
via a concrete `PlatformInfo` field, scope the first fix away from vhost-user
devices that need sharing metadata, and add a small aliasing test. Those points
are incorporated above.
