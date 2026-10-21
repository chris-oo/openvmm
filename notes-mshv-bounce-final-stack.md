# MSHV bounce stack findings

These findings record the squash discussion before the latest-main rebase.
They are not authorization to squash commits or remove more behavior.

## Tested source before the rebase

- Bounce tip: change ID `vnrmnomr`, bookmark `mshv-bounce-simplified`.
- Base: change ID `yxvslnsp`.
- Aggregate diff to that base: 74 files, 6,455 additions, 446 deletions.
- Local validation: 697 package tests passed, three skipped; all three
  forced-bounce VMM tests passed. Native checks, Clippy, docs, formatting,
  and ARM Windows/macOS/Linux Clippy passed.
- SNP hardware: RAM and buffered-file disk runs passed on `chris-mshv`.
  Each completed 30,720 verified block operations, two TCP/vsock rounds,
  all device unbind/rebind cycles, 13 kernel-proven release/resume pairs,
  and 65,536 balanced pins. Kernel logs stayed unchanged and teardown was
  clean.
- Evidence: `target/adjacent-cleanup-20261021/validation-summary.json`.
  The first hardware attempt was rejected because its trace included
  other guest partitions. It is preserved separately from the accepted retry.
- Those hardware runs used the CLI. They did not establish end-to-end
  ttrpc SNP or device-mapper coverage.

## Proposed final five-commit stack

| Commit | Pre-rebase owner | Intended content |
| --- | --- | --- |
| Guest-memory foundations | `lspwqqqu` | CopyOnly policy, user/kernel access distinctions, export restrictions, and required caller changes. Absorb the queue-policy rollback so the removed completion contract never appears in the final commit. |
| Storage support | `osyznnpq` | Lower-backend CopyOnly support, bounds checks, exact-transfer fixes, and tests. Absorb frontend-staging removal, FUA rollback, normal allocation, and comment cleanup. |
| Network/vsock support | `ltxryxrn` | Software-backend admission, RX safety, existing bounce fallbacks, and partial-write/credit fixes. Absorb arena/per-device-mode removal and boxed-ring/comment restoration. |
| MSHV SNP integration | `poyqlmtw` | Fault-driven host grants, automatic CopyOnly selection, VM-wide force option, DMA-export gate, CLI admission, and forced-bounce VMM tests. Absorb its validation and editorial cleanup. |
| VM-service SNP support | Combine `rwvusozw` and `zpowklwv` | Supported-device admission at create/hot-add, base-page defaults, disk-opening policy, and service tests/docs. No separate bounce mechanism or force option. |

The follow-up changes to distribute by hunk are:

- `urpzwrks`: restore the existing queue completion policy.
- `losxqlwv`: centralize CopyOnly policy and remove frontend staging/modes.
- `ystkqzwp`: undo the unrelated FileDisk FUA extension.
- `ymyoooky`: restore baseline comments and layout.
- `vnrmnomr`: restore fixed ring storage and normal FileDisk allocation;
  remove unused validation results and duplicate/history-only tests;
  retain the DMA gate without reindenting VFIO registration.

Do not squash a whole follow-up into one ancestor when it changes several
owners. Keep each final commit buildable. Rename the storage and network
commit descriptions: they no longer implement mandatory frontend staging.
A history-only squash must preserve the final tested source tree.

The separate usermode-lock and lite branches are not part of this proposed
bounce-stack squash.

## Surviving ttrpc rationale

The service does not enable bouncing itself. MSHV SNP reports CopyOnly,
and dispatch applies that policy regardless of the management frontend.
The service still selects memory defaults and performs early admission.

Retain the following behavior:

- Allow supported PCIe virtio-blk, software virtio-net, and in-process
  virtio-vsock instead of rejecting every PCIe topology.
- Apply the same admission to boot devices and hot-added devices.
- Retain the current VPCI and unsupported-device restrictions.
- Default SNP memory to base pages and reject explicit huge-page requests,
  subject to checking the latest upstream backend requirements.
- Retain VM-level policy state while hot-add admission needs it.
- Do not expose a ttrpc force-bounce option.

The normal Linux raw-disk path already returns `BlockDeviceDiskHandle`
for regular files and host block devices. The block backend already copies
through owned buffers when guest memory cannot be kernel-locked. A special
ttrpc block backend is therefore not a bounce prerequisite.

The current SNP-specific opener also enforces a narrower disk policy:
buffered/plain regular files, descriptor-based block-device classification,
block-device aliases with image-like suffixes, and nonblocking rejection
of unsupported descriptors. Those are opening/admission rules, not bounce
mechanisms.

Consolidating that opener with the shared disk-opening code remains a
separate decision. Replacing it with the generic builder without equivalent
validation would also remove existing restrictions. Do not hide that behavior
change inside the history cleanup.

Pre-rebase evidence locations:

- `vmm_core/virt_mshv/src/x86_64/mod.rs:443`: backend CopyOnly policy.
- `openvmm/openvmm_core/src/worker/dispatch.rs:1455`: central policy selection.
- `openvmm/openvmm_entry/src/ttrpc/mod.rs:654`: service admission.
- `openvmm/openvmm_entry/src/ttrpc/mod.rs:1193`: memory defaults.
- `openvmm/openvmm_entry/src/ttrpc/mod.rs:1605`: hot-add path.
- `openvmm/openvmm_entry/src/ttrpc/mod.rs:2489`: virtio-blk construction.
- `openvmm/openvmm_entry/src/ttrpc/mod.rs:2527`: SNP-specific opener.
- `openvmm/openvmm_helpers/src/disk.rs:152`: shared raw-disk opening.
- `vm/devices/storage/disk_blockdevice/src/lib.rs:576,638`: backend bouncing.

These line references describe the saved pre-rebase source, not future
upstream or rebased line numbers.

## Follow-up test requirements

After the rebase, assess upstream MSHV SNP VMM-test support and the
PCIe-capable IGVM builder before adding new harness code.

Coverage must include:

- SNP virtio-blk payload verification, not just successful boot.
- RAM-backed, regular-file-backed, and host-block-backed virtio-blk disks.
- Host block devices, including a disposable loop/device-mapper fixture,
  in both local backend tests and VMM tests.
- Buffered and direct host-block I/O, plus read-only behavior where applicable.
- PCIe device hot-add, I/O after add, removal, and re-add.
- ttrpc create/add/remove paths and verified I/O, not only resource-builder
  unit tests or CLI-created VMs.
- Equivalent host-block-backed coverage through ttrpc.
- Appropriate host capability and privilege gates. Tests must use disposable
  fixtures and must not write arbitrary host disks.

Keep the existing exact-transfer, vsock partial-write, guest-input safety,
and CopyOnly DMA protections. New coverage and later squash work are separate
from the rebase.

## Latest-main rebase

The current bounce stack was copied onto upstream `main`, change ID
`yywkuopm`, on 2026-10-21. The user approved new change IDs so the separate
usermode-lock, lite, integration, and historical branches could remain
unchanged. No feature commits were squashed.

| Pre-rebase change ID | Rebased change ID | Role |
| --- | --- | --- |
| `lspwqqqu` | `ntxotomx` | Foundations |
| `osyznnpq` | `tmtsmuvz` | Storage |
| `ltxryxrn` | `wyqukmov` | Network/vsock |
| `poyqlmtw` | `llulpvzx` | MSHV SNP |
| `rwvusozw` | `lmzzsxzq` | VM-service SNP |
| `zpowklwv` | `lskxuoru` | VM-service host block |
| `urpzwrks` | `zlkwzyts` | Queue-policy rollback |
| `losxqlwv` | `xlvkzsnt` | Central CopyOnly simplification |
| `ystkqzwp` | `mrprmxzq` | FUA rollback |
| `ymyoooky` | `ykykunst` | Comment/layout cleanup |
| `vnrmnomr` | `slmxvmpo` | Adjacent cleanup |
| `xtxmzryz` | `xrtkmmtr` | These saved findings |

The original tested source and notes remain reachable through
`pre-rebase-bounce-20261021` and `pre-rebase-bounce-notes-20261021`.
The source bookmark is `mshv-bounce-simplified`; the findings have their own
bookmark, `mshv-bounce-stack-notes`. The findings are a review artifact, not
one of the five proposed feature commits.

The only textual rebase conflict was in Petri's default virtio-vsock
selection. The resolution keeps upstream's `isolated_without_paravisor`
default while removing the obsolete per-device bounce flag in the later
policy-removal commit. The final `petri/src/vm/mod.rs` matches upstream.

### Upstream support now available

- `vmm_tests/vmm_tests/tests/tests/x86_64/snp_linux_direct.rs:13,18`
  registers one- and two-VP SNP boot cases through
  `configs(snp_linux_direct_x64)`.
- `vmm_tests/vmm_test_macros/src/lib.rs:1414` requires MSHV and an AMD host
  for that configuration. Passing an ordinary local KVM test does not
  establish SNP coverage.
- `flowey/flowey_lib_hvlite/src/build_snp_linux_direct_igvm.rs:54`
  builds the SNP test IGVM and embeds pipette in its initrd.
- `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:917` passes the
  runtime PCIe host bridges into the device tree. The SNP bootshim builds
  PCIe SSDT and MCFG tables from those bridges in
  `vm/loader/snp_bootshim/src/acpi_tables.rs:36`.
- `vm/loader/igvmfilegen/src/snp_linux_direct.rs:140` reserves output
  space rather than embedding the old fixed ACPI topology. This addresses
  the earlier PCIe-capable IGVM fixture limitation.
- `vmm_tests/vmm_tests/tests/tests/ttrpc.rs:39` provides the existing
  local RPC regression and reusable launch/configuration helpers.

### Rebase validation

- Native check, all-target Clippy, and no-dependency docs passed for the
  selected memory, MSHV, storage, virtio, management, and Petri packages.
- 708 package tests passed, three skipped.
- All three existing forced-bounce VMM tests and the ordinary local ttrpc
  interface VMM test passed. These are not new SNP/ttrpc hardware tests.
- ARM Windows/macOS/Linux management, device, and Petri Clippy passed.
- At the rebase hand-off, no post-rebase SNP hardware campaign had run.
  Subsequent hardware results are recorded below.
- Rebase evidence is under `target/rebased-main-20261021/`.

## Implemented SNP, host-block, and RPC coverage

The post-rebase follow-up added:

- Registered block tests for ordinary forced-CopyOnly guests and SNP IGVM
  guests, with RAM, regular-file, buffered host-block, and direct host-block
  backends.
- Separate boot and hot-added disks, verified binary payloads and backing
  files, removal, and read-only re-add for persistent backends.
- SNP-over-ttrpc tests for regular files and buffered/direct host block
  devices, covering create, add, remove, I/O, and complete process teardown.
- Owned disposable loop fixtures behind `PETRI_CAPABILITIES=loop_device`.
  A keeper descriptor and kernel autoclear protect against loop-device
  leaks when a test is killed.
- Portable `--target linux-musl-x64` Flowey bundles. The target host needs
  neither a Rust toolchain nor the build host's glibc version.

Removal is quiesced: the guest virtio-pci driver is unbound while MMIO is
still present, then the host removes the function. Tests wait for complete
guest PCI teardown before re-add. The user confirmed that surprise removal
is a known issue and selected this scope; no production PCIe removal code
was changed.

The new RPC tests needed a below-4-GiB ECAM override in this test setup.
Without it, the SNP guest reported no PCI configuration access function
and could not discover its virtio devices.

**The user requires this override to remain a separate local-only top
commit. It will not be submitted.** Do not fold it into the proposed
VM-service commit or any other submitted change. Its coupled ECAM reference
documentation stays in that local commit too. Non-SNP ECAM placement is
unchanged.

`mshv-bounce-simplified` identifies the tests and runner changes without the
override. `mshv-snp-ecam-local` identifies the local top commit above it.
The SNP RPC hardware results below include this local override; they are
not evidence that the submission candidate works without it on this host.

Ordinary MSHV test guests retain Hyper-V clocks with VMBus disabled.
Ordinary KVM CopyOnly tests keep Hyper-V disabled because page overlays
need pointer access. Backend selection uses Petri's host context.

### Hardware and local results

- Final registered hardware run: 11 passed, no failures. It includes two
  ordinary host-block cases, four SNP block cases, three SNP RPC cases,
  and upstream one-/two-VP SNP boot tests.
- Six local KVM VMM regressions passed, including existing block/net/vsock
  bounce tests, the new RAM/file hotplug tests, and ordinary ttrpc.
- Final native Clippy/docs and 212 package tests passed, with one privileged
  test skipped locally. ARM Windows/macOS/Linux checks also passed.
- The previously ignored VM-service host-block opener unit test passed
  against an owned device-mapper linear target, including buffered/direct
  and read-only/read-write descriptor modes.
- Both established RAM/file SNP campaigns now boot the same PCIe-capable
  IGVM. Their approved guest kernel, initrd, payloads, and nonce are
  unchanged. Each passed 30,720 verified block operations, two TCP/vsock
  rounds, all driver unbind/rebind cycles, 13 raw-trace-proven release/resume
  pairs, and 65,537 balanced pins.
- The host was observed already rebooted after the user authorized reset.
  No extra reboot or forced partition deletion was performed. Hardware
  results use kernel `6.18.34.mshv3-mshv-lite-shared-5e10`, not the earlier
  cached-grants kernel.
- Evidence is under `target/snp-igvm-coverage-20261021/` and
  `target/snp-igvm-lifecycle-20261021/`. Earlier failed setup attempts remain
  separate from the accepted final run.

The normal local WSL account lacks loop-device privileges. Real host-block
and device-mapper cases ran on `chris-mshv`; they were not replaced by
regular-file proxies.
