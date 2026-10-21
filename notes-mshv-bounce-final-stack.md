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
