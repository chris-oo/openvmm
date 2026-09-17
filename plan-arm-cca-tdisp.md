# Linux host TDISP for Arm CCA guests in OpenVMM

Date: 2026-09-11

Updated: 2026-09-18

Status: **native OpenVMM TDISP LOCK/RUN and a 64 MiB AHCI read are demonstrated
on FVP with the unchanged kvmtool reference guest.** The read matched the
reference disk hash. The complete VMM test still fails at UNLOCK and shutdown;
this is a successful protocol/I/O milestone, not clean-lifecycle qualification.
The execution implementation and tests have now been split into reviewed,
validated commits. The provisional shutdown hold is retained separately as
an unapproved deferred change.

**Current implementation priority: share the host-side TDISP infrastructure
with microsoft/openvmm#4416.** The rebase is complete; the refactor is not.
Section 4 is the authoritative design and commit sequence for that work.
The original bring-up stages elsewhere in this document remain context and
qualification history, not instructions to repeat the rebase or implementation.

The guest_memfd in-place Virtio tests also pass on FVP. In-place QEMU debugging,
OpenVMM private-buffer DMA measurement, negative isolation controls, teardown
and same-host reuse remain separate work.

See [the high-level component summary](summary-arm-cca-tdisp.md) for the
implementation overview, runtime evidence, committed layers and pending splits.

## 1. Recommendation

Extend OpenVMM's existing VFIO cdev/IOMMUFD assignment path with a **CCA-specific
assignment mode**, a native RHI guest-request adapter, and trusted-I/O exit
handling. Use `vm/devices/tdisp` for transport-independent device lifecycle and
report support, but **do not route Linux CCA requests through the existing
OpenHCL VPCI/protobuf protocol**.

The first end-to-end target is one static, modeled AHCI controller assigned to
a Linux-direct Realm. Run a new `vmm_test` inside the existing FVP incubator,
with a separately qualified **CCA DA v7 platform and payload**. Keep the current
v15 FVP/QEMU boot paths intact.

This is not just a TDISP callback implementation. The main prerequisites are:

1. Guest_memfd in-place memory, including explicit initial RIPAS, the guest_memfd
   attribute ABI, and DMA-ready private mappings.
2. KVM-associated Realm IOMMUFD objects, distinct from ordinary SMMUv3 nesting.
3. Guest RHI transport, TIO completion, and one serialized device/access state.
4. Protected BAR access, usable interrupts, and conversion-aware shared DMA.
5. A DA-capable FVP package, host provisioning, guest payload, and test.

The existing Linux VFIO and FVP infrastructure is reused. Ordinary `vfio` and
`vfio-cdev` resources remain rejected for CCA. The distinct `vfio-realm`
resource selects the native assignment, access and memory-coordination path;
removing the ordinary resource guard is not the implementation.

### Current milestone and remaining review

The rebased `cca-kvm-tdisp` baseline is change ID `tkorwyzw`, above merge
`tmqyvsty` and MPIDR fix `vvutrqnt`. Conflict repairs were squashed into their
owning changes. The final tree passed 609 host unit tests and 71 Arm KVM tests.
The full FVP rerun reproduced LOCK/RUN, the original 64 MiB read/hash and
132 AHCI MSI-X interrupts, then failed at the existing UNLOCK guard and
cleanup. The shared-core refactor must preserve this milestone without
claiming a full lifecycle pass.

```text
Invocation: single-boot-3788265-1789703740648468672
FVP: 5cc6553999afe3350aba6fd08580b9a1fa9b0ffb879157b87bdabc4db4412891
nextest: 1f3af87c-fee9-4359-86cb-b0e78714760d
Evidence: vmm_test_results/cca-tdisp-rebased-stack/fvp-single-boot-runs/
          single-boot-3788265-1789703740648468672/
```

The following split records describe the completed original implementation.

The fifth unchanged-guest trial reached native Locked and Run states, reprobed
AHCI at `0000:01:00.0`, and completed the original initrd's 64 MiB direct-read
and hash check. Guest interrupt records show AHCI MSI-X delivery. UNLOCK then
hit OpenVMM's conservative protected-mapping guard; the VP halted and the
overall test failed. kvmtool implements UNLOCK through the native state ioctl.
Our guard failure does not prove that the kernel cannot perform UNLOCK.

The execution changes were assembled into dependency-ordered, buildable chunks:

| Chunk | Main files | Review/commit status |
|---|---|---|
| Native TDISP operations and serialized RAM work | `vm/devices/tdisp/src/host*` | Committed as `mktoyytw`, including required caller-compatibility edits. |
| KVM interrupt-route error handling | `vmm_core/virt_kvm/src/gsi.rs` and route-check forwarding | Committed as `lqzlyxul`. |
| Realm VFIO backend and PCI frontend | `vm/devices/pci/vfio_assigned_device/` | Committed together as `nsvzyuut`; the private gate API and its frontend consumers form one warning-free boundary. |
| Full RHI/TIO and RAM-conversion runtime | `vmm_core/virt_kvm/src/{rhi,memory,cca_in_place}.rs` and Arm dispatch | Committed as `tusnzrsq`. |
| OpenVMM setup and private PCI resource view | `openvmm_core` worker/loader, `vm_topology`, ACPI test initializers | Committed as `yzoxvwlz`, without the provisional shutdown hold. |
| Same-guest VMM test and preserved commands | `vmm_tests/.../aarch64_exclusive/tdisp_ahci.rs` and `test_data/cca_tdisp/` | Committed as `ykvolqkt`; the known failed overall runtime verdict remains explicit. |
| Optional dormant-object test | `aarch64_exclusive.rs` and Arm-only dev-dependencies | Committed separately as `ozppvpsq`; not needed for the achieved guest milestone. |
| Status and component documentation | This plan and `summary-arm-cca-tdisp.md` | Updated after the split and observed milestone. |

The worker lifetime hold is provisional. Review found gaps on parent mesh
disconnection and constructor failure; do not describe it as complete recovery
or process-lifetime protection. Those shutdown/reuse changes are deferred, not
covered by the successful guest I/O result. Apply the normal review and
pre-commit checks to each final split; a review of the combined tree does not
prove that every proposed intermediate commit builds independently. Each
execution candidate above was therefore reconstructed and checked on its own.

### Final split and commit procedure

Preserve the complete reviewed working tree under a temporary local jj
bookmark before changing the checkout. Build the commit chain from its parent
in the existing workspace, restoring only the next chunk from that snapshot.
This keeps all unsplit code and the captured runtime evidence available.
Do not rewrite pushed changes or push the new chain.

Record the source change and parent before leaving that checkpoint, and check
that every intended source file is included. Ignored artifacts are not saved
by jj: `cargo clean` removed `target/cca-tdisp-stage-a` on September 16, while
the saved run evidence under `vmm_test_results/` remains. Check preserved
input copies before any new FVP run; missing tools or artifacts block that
run, not source-level validation.

| Order | Proposed commit | Dependency and boundary |
|---|---|---|
| 1 | Native TDISP assignment operations and worker contract | `tdisp` API/coordinator/service plus the minimum existing-consumer compatibility edits for `raw`, `Unsupported` and admission closure. No live backend enablement. |
| 2 | Checked KVM IRQ route lifetime | GSI route/error handling and the VM route-check implementation. Keep full RHI/DMA dispatch for a later commit. |
| 3 | Realm access gate and native backend | Realm owner, TSM operations, shared-DMA mapping and backend tests. Keep PCI frontend tests out until their implementation lands. |
| 4 | Realm PCI frontend and resolver | Trapped BAR/config access, MSI-X handling, fixed identity, service ownership and resolver wiring. Combine with order 3 when separating private APIs from their consumers prevents a clean, warning-free boundary. |
| 5 | Full KVM RHI/TIO and RAM conversion | Remaining Arm dispatch, initial private preparation, serialized conversions and runtime tests. |
| 6 | OpenVMM assembly and private PCI view | Resolver admission, retained RAM-region owner, dedicated-root validation, DT metadata and required ACPI test initializers. |
| 7 | Optional dormant Realm-object diagnostic | Its function and Arm-only test dependencies, separate from the actual guest test and never an execution gate. |
| 8 | Unchanged-guest TDISP/AHCI VMM test | Exact guest/disk checks, PCI setup and guest/kernel result interpretation. Keep the observed failed overall verdict. |
| 9 | Milestone and component documentation | This plan and the standalone summary, updated with the resulting change IDs and validation state. |

Keep provisional shutdown-hold changes isolated from these execution chunks.
Review their bounded behavior separately. If blocking findings remain, retain
them as a named deferred change rather than mark them approved or silently
include them in an execution commit.

Specifically, keep the RAM-region ownership wrapper in the assembly commit,
but preserve the provisional `LoadedVm::run` shutdown hold as a separate,
unapproved change atop the chain. The prior FVP run used the combined tree;
excluding that hold is not behavior-neutral and must not inherit a fresh
runtime-qualification claim. Do not lose it when restoring the worker file.

Use hunk-level compatibility edits where files span commits. The IRQ chunk
uses the already-committed route-check trait. The backend chunk includes its
module declarations and dependencies but excludes the frontend-test module.
The frontend chunk includes its manager binding and resolver. Full KVM
registration and retention-query implementation remain in the runtime chunk.

For each candidate, inspect its actual diff and obtain a focused review.
Resolve findings, then run the modified packages' unit tests, clippy and
rustdoc; check the Arm consumer when interfaces cross architectures.
Run the full `cargo xtask fmt --fix` last and commit only after it succeeds.
Use normal Cargo lockfile regeneration for the selected manifests so unrelated
pending dependencies do not enter earlier commits.

Inspect the final formatted diff before committing. If formatting makes
semantic or out-of-scope changes, resolve them and rerun affected checks.
Record each chunk's review and pass/fail/blocked status here. A failed candidate
stays uncommitted for correction; never restore the next chunk over that work.
Compile the test targets for both VMM-test commits, without mislabeling the
known overall FVP failure as a passing unit/build check.

At the end, compare the assembled tree with the preserved source. Account for
every difference: compatibility edits later replaced by their final versions,
review fixes, documentation updates and any explicitly deferred shutdown
change. Remove the temporary bookmark only after all source changes are
committed or otherwise explicitly retained.

### Executed split validation

The preserved source was change `sxturssr`, based on `xxuzxupu`. Before the
integrated test correction below, the only source-code differences were the
provisional shutdown hold and its tracking fields in `dispatch.rs` and
`dispatch/realm_retention.rs`. That code is preserved in unapproved change
`kwtwmymp`, on `cca-assignment-deferred-shutdown`, outside the execution
commits. The RAM-region owner wrapper remains in `yzoxvwlz`.

| Change | Candidate review | Validation before commit |
|---|---|---|
| `mktoyytw` | Focused review: no significant issues | 180 native tests across the API and existing consumers; native/Arm clippy, rustdoc, full formatter |
| `lqzlyxul` | Focused review: no significant issues | 59 native tests; native/Arm clippy, rustdoc, full formatter |
| `nsvzyuut` | Focused review: no significant issues | 88 native tests; native/Arm clippy, rustdoc, full formatter |
| `tusnzrsq` | Focused review: no significant issues | 72 native tests; native/Arm clippy, rustdoc, full formatter |
| `yzoxvwlz` | Execution-only review: no significant issues | 125 native tests across worker/topology/VMM core; native/Arm clippy, rustdoc, full formatter |
| `ozppvpsq` | Direct review of the complete diagnostic and gating | Native/Arm test-target compilation through clippy; rustdoc, full formatter; no live diagnostic rerun |
| `ykvolqkt` | Focused review: no significant issues | Native/Arm test-target compilation through clippy; rustdoc, full formatter; shell syntax and recovered-source hashes |
| `tquwpomz`, `vosxqvox` | Direct review of the cancellation-test scheduling and assertion order | Final correction passed 20 stress iterations, then all 525 integrated tests; clippy, rustdoc, full formatter |

The backend-only candidate initially passed 73 tests but left private APIs
without production consumers. It was not committed with warning suppressions;
the backend and frontend were combined and rechecked. Standalone
`openvmm_core` checks retain the existing crypto-backend configuration warning.

These are source-level checks of the split chain, not a new FVP execution.
The successful LOCK/RUN/I/O evidence remains the recorded combined-tree run,
whose full test failed at UNLOCK/shutdown. Missing `target/` artifacts after
`cargo clean` prevented no source checks, but a new live run requires restored
tool and input staging.

The integrated run initially timed out in the queued-worker cancellation
test. Its pre-polled retry used a no-op waker and could consume the mutex's
only wake while the test waited on a second waiter. The final test drains and
asserts the cancelled task's cleanup before creating its retry. This fixes the
test race without letting the retry satisfy the cancelled-task assertion.
Production code is unchanged. The final integrated result was 525 passing
tests across ten packages; this correction is the additional intentional
test-file difference from the preserved source.

## 2. Inputs and evidence

The starting point is
[`~/lkml/findings-cca-tdisp-v7-openvmm.md`](../../../lkml/findings-cca-tdisp-v7-openvmm.md).
This plan checks that report against the current OpenVMM and local Linux sources,
and the pinned kvmtool reference. Paths beginning with `../linux-cca`,
`../kvmtool-cca`, or `../tf-rmm` refer to the sibling checkouts.

| Input | Reference inspected | Qualification |
|---|---|---|
| OpenVMM | Plan baseline change ID `rukkunwr`, based on change ID `llntpwqp`, bookmark `cca-v15-fvp-upstream` | Current working tree demonstrates FVP LOCK/RUN and AHCI read/hash; lifecycle and private-buffer qualification remain open |
| Host and Realm Linux | `../linux-cca`, `scratch/cca-tdisp-integration-v7`, `2b68f486fdbc8d2818309f91199dde46b2b7cdd6` | Matches the findings document |
| kvmtool | `scratch/cca-tdisp-integration-v7`, `2e0928d1f945d68af388575e7bd4d6bfa7200120` | Pinned revision inspected initially; matching working tree independently reviewed in pass two |
| TF-RMM | `../tf-rmm`, `33bbaf7814fee335027bf4d2417d97b551838b70` | Includes the DA overlay selecting both v7 branches |

The initial kvmtool checkout was on `master`; the primary investigation read
the pinned v7 revision without changing that checkout. The user then switched
it to the integration branch. Review pass two read its working-tree files and
checked sibling branch references against the pins above. That review did not
establish working-tree cleanliness or byte-for-byte equivalence to committed
files. Build from recorded clean inputs when creating the reference baseline.

These sources are a **candidate integration set**. TF-A, model compatibility,
the effective firmware configuration, and artifact hashes were recorded for
the local run below. Protocol/read success does not establish a clean lifecycle
or production trust policy. Do not label the old v15 FVP tuple as a tested v7
tuple.

### Implementation status

| Change ID | Completed scope | Not enabled by that change |
|---|---|---|
| `wrwxunkx` | Explicit guest_memfd flags, attributes2, INIT_RIPAS and prefault wrappers with ABI tests | In-place backing integration or device assignment |
| `orsxqwvz` | Successful memory-fault exit decoding, original errno preservation, and rejection of non-`EFAULT` errors by the legacy conversion path | Handling v7 completion through in-place conversion; successful memory-fault exits still stop explicitly |

Both code chunks received code review and scoped validation before commit.
The baseline-code descriptions elsewhere in this plan identify why the full
integration is needed; the two completed wrapper/exit tasks above supersede
statements that those low-level interfaces are entirely absent.

### Guest_memfd in-place implementation and parity

The memory mode is **guest_memfd in-place**, selected by
`--guest-memfd-in-place` with `--isolation cca`. The configuration field is
`guest_memfd_in_place`; the backend recognition hook is
`recognizes_guest_memfd_in_place`. The helper module is
`vmm_core/virt_kvm/src/cca_in_place.rs`. The old `--cca-v7` spelling is
rejected. References to v7 elsewhere identify the pinned Linux integration
branch and its ABI, not an OpenVMM memory-mode name.

The later foundation changes are `nkxtrrzo` (backing imports at file offsets),
`pyrmwurs` (GNU/musl ioctl request types), `svvtpwvn` (partition-owned RAM
preparation and imports), and `vxmksnus` (in-place launch and conversion).
They supersede the corresponding baseline gaps described in section 6.
At that foundation checkpoint, assigned-device private prefaulting and
coordinated IOAS mapping were future work. They are now implemented in the
full assignment path; measured private-buffer DMA qualification remains open.

The follow-up fixes are change ID `wmumznmy` (guest-memory policy forwarding
and vsock copy paths) and change ID `vwkkpkwp` (in-place naming, revocable
access policy, and the paired VMM test).

In-place memory now uses the ordinary CCA device allowlist, not a separate
no-PCI restriction. Revocable mappings disable raw page locking and file-based
sharing. Arc and subrange guest-memory wrappers preserve that policy. Vsock
uses its copy path when locking is unavailable; that path now respects peer
credit and handles an RX descriptor chain that changes across an await.

The paired tests in
`vmm_tests/vmm_tests/tests/tests/aarch64_exclusive.rs` share one body. Each
boots a one-VP, 256-MiB Realm with the same PCIe/Virtio-vsock setup, connects
pipette, pings, requests poweroff, and requires clean OpenVMM teardown.
Within the shared test body, only the in-place selection differs. The
recorded runs use the default QEMU/FVP tuples for separate backing and the
separately pinned FVP tuple for in-place backing; they do not establish
same-tuple backing-mode parity.

| Parity check | Result |
|---|---|
| Existing separate-backing CCA VMM test, default QEMU tuple | 1 passed, 36.663 s |
| Existing separate-backing CCA VMM test, default FVP tuple | 1 passed, 219.714 s |
| Paired guest_memfd in-place CCA VMM test, pinned in-place FVP tuple | 1 passed, 246.103 s |
| `guestmem`, `membacking`, `virtio`, block, net, rng, console and vsock suites | 275 passed, 1 skipped |

The in-place nextest run ID is `efcaa82e-e82b-4c51-a2ad-f935bdb7bc95`.
Its local output is under `vmm_test_results/in-place/test_results/`.
Each of the three VMM results above represents one executed, passing test,
not enumeration-only success or a capability skip. The in-place FVP run also
completed with outer exit status 0. The separate crate suites reported 275
passed and 1 skipped. At that checkpoint, the matched guest-VM coverage was
Virtio-vsock; the other listed Virtio devices had crate regression coverage.
The later block and network results are recorded below.

The separate FVP profile and command are documented in
`Guide/src/dev_guide/tests/vmm/qemu_cca.md`. Its kernel differs from the local
reference config only by `CONFIG_SMC91X=m` becoming `y`: the official initrd
does not load that host NIC module. The effective package omits
`pci.hierarchy_file_name` to select the model default. An empty filename is
not the default and caused a model error during an earlier enumeration run.
The corrected profile completed both the Realm test and outer host shutdown.
The default v15 inputs remain unchanged. This does not qualify in-place
memory on the old QEMU tuple, TDISP, host DMA, or assigned-device teardown.

The reference's later
[`run-20260914-private2/result.json`](target/cca-tdisp-stage-a/runs/run-20260914-private2/result.json)
records one 4096-byte AHCI read into checked RIPAS_RAM payload and command
buffers. Its data and completion checks passed. Four IOMMUFD `EBUSY` cleanup
errors and FVP exit 134 remained. That bounded kvmtool result is separate
from the clean OpenVMM Virtio test and does not complete the A3 controls.

### Upstream firmware and payload publication

As checked on 2026-09-15, the latest `microsoft/openvmm-deps` release is
`0.3.0-139`. Its CCA firmware archives target QEMU: the TF-A manifest specifies
`platform=qemu` and `linux_as_bl33=true`, and TF-RMM specifies
`config=qemu_virt_defcfg`. The release does not contain an FVP firmware
archive. QEMU binaries are not a substitute for FVP-targeted firmware.

For reproducible upstream FVP testing, add an upstream build and release
package for the qualified FVP configuration. Include BL1, the FIP containing
the required TF-A/RMM/EDK2 components, the device tree, and relocatable
package metadata. Record source revisions, effective configurations,
toolchain identity and artifact hashes. Update the FVP resolver to download
and validate these assets rather than require a developer's locally built
Shrinkwrap package. The licensed simulator remains separately provisioned.
Qualify the resulting published bytes with the VMM tests before changing
the profile pins; the current local results do not qualify an unpublished
replacement package.

For QEMU, reuse the existing upstream QEMU/TF-A/RMM assets first. A newer
in-place guest_memfd kernel alone does not justify rebuilding firmware.
Publish replacement QEMU firmware only if a demonstrated compatibility
failure requires it, or a later feature such as device assignment requires
different firmware support. Keep that decision separate from publishing
the in-place-capable host/Realm Linux kernel, config and manifest, which
are currently local test inputs. Reuse the existing published base initrd.
Retain the default v15 release/profile as the separate-backing control.

### In-place block and network testing: 2026-09-15

The new `virtio_blk_cca_in_place` and `virtio_net_cca_in_place` VMM tests
explicitly select CCA and in-place guest_memfd. Both use Virtio-vsock as an
independent control channel and require clean OpenVMM teardown.

| Test | FVP | QEMU with published `0.3.0-139` firmware |
|---|---|---|
| Virtio-blk | Passed, 258.168 s | Blocked during Realm boot; no I/O qualification |
| Virtio-net | Passed, 261.217 s | Blocked before pipette; bounded run timed out |

Block coverage checks distinct 64-KiB patterns at the start and end of an
8-MiB scratch disk, guest direct writes and direct readback, and the host
backing file after teardown. Network coverage verifies all 64 KiB in each
direction over TCP through the Virtio NIC. An initial network test error
polled the IPv4 socket table while BusyBox listened on IPv6; explicit IPv4
binding and early server-exit detection fixed that test error.

FVP evidence: nextest run `44ad82a7-df38-4e44-ab1e-5ce899ef02a3` contains the
block pass and initial network failure; run
`1d92bc67-4790-4537-ab02-993662472978` contains the corrected network pass.
Outputs remain under `vmm_test_results/in-place-io/` and
`vmm_test_results/in-place-net/`.

The QEMU diagnostic profile reuses the published emulator and TF-A/RMM,
with the same local kernel/initrd payload used on FVP. No locally rebuilt
QEMU firmware was used. The host boots and the Realm reaches early kernel
initialization;
the block diagnostic console stops after printing the ITS resource.
Temporary tracing recorded completed guest_memfd attribute conversions for
the SWIOTLB and initial ITS allocations, but does not establish correct
subsequent access or the cause of the stall. The separate network attempt
also failed to reach pipette. Bounded diagnostic runs exited with timeout
status 124, not test success.

QEMU evidence is under `vmm_test_results/qemu-in-place-io/`,
`qemu-in-place-diagnostic/`, `qemu-in-place-trace/`, and
`qemu-in-place-net/`. This combination remains unqualified. Do not infer
that firmware replacement alone fixes the stall; isolate the kernel,
RMM and VMM interaction before changing upstream firmware pins. The
temporary console, monitor and conversion-trace changes were removed.

**QEMU is broken for this in-place test combination and needs further debug.**
Per the implementation priority, defer that investigation and continue native
TDISP bring-up on the tested FVP path. QEMU is not a gate for the next FVP
implementation chunks. Keep its diagnostic profile and failure evidence;
do not present it as qualified or replace firmware without an identified
cause. This deferral does not waive the FVP device-assignment, private-DMA,
interrupt, or clean-lifecycle gates.

### Local Stage A results: 2026-09-14

The local reference uses the Linux/kvmtool/RMM pins above, TF-A
`38269bb73d34b4a31100adf208a27e4c28e7d611`, FVP 11.31.28, and Shrinkwrap
`1c6b7a5278b47be11cad3bcd3a20416fc43fd388`. The container digest, effective
kernel settings, build deviations and artifact hashes are saved under
`target/cca-tdisp-stage-a/`. These are local artifacts, not a published
OpenVMM platform. The existing v15 package was not changed.

| Observation in run 3 | Result and limit |
|---|---|
| Host and no-device Realm boot | Passed; the no-device Realm powered off |
| Host AHCI connection | `0000:02:00.0` bound to VFIO and connected to the host TSM |
| Guest TDISP | Guest `0000:00:00.0` completed LOCK and RUN; RMM logged `RSI_VDEV_DMA_ENABLE > RSI_SUCCESS` |
| Protected MMIO | ABAR `[0x50006000,0x50008000)` observed with RIPAS_DEV |
| Data transfer | Read the complete 64 MiB disk using `dd bs=1048576 count=64 iflag=direct`; hash matched the nonzero fixture |
| Interrupts | MSI-X through the GIC ITS; AHCI cumulative interrupt count was 132 after I/O, not a read-only interrupt delta |
| RMM device teardown | STE disable, VDEV unlock/destroy, stream-table destruction and pSMMU deactivation succeeded |
| Remaining cleanup | Four `IOMMU_DESTROY` failures with errno 16 (`EBUSY`) |
| Simulator shutdown | FVP aborted with exit 134 and `corrupted size vs. prev_size`; launcher failed despite guest/host script exit 0 |
| Confidential DMA | Expected normal path, but not yet demonstrated for the actual command/data buffers |

The complete-image SHA-256 was
`281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6`.
This proves matching read data, not writes, absence of shared bounce buffers,
or physical-link encryption.

Evidence is in
[`runs/run-20260914-3/result.json`](target/cca-tdisp-stage-a/runs/run-20260914-3/result.json),
the run's `input-hashes.json`, `share/realm-ahci.log:327-359,661-673`, and
`console.log:82363,82379-82405,84016-84017,84058-84059`. The invocation is
preserved in `guest-overlay/da-guest-init.sh`; the guest log prints transfer
record counts and the hash, not the full command line.

Run 1 stopped on a local BusyBox `lspci` option mismatch. Run 2 reached
acceptance but the local probe exited before asynchronous disk discovery
finished. Run 3 added a bounded discovery wait and passed the read test.
Run 2's VDEV/stream/pSMMU teardown failures did **not** recur in run 3.
The remaining IOMMUFD errors and model heap abort are separate observations;
their causal relationship is unknown. Do not suppress either or treat the
model's shutdown failure as a passing end-to-end test.

### Minimal DA model shutdown reproduction

The host-object fixture remains blocked before test execution. Its first
enumeration boot reached host TSM connection, VFIO binding and pipette, but
FVP aborted at poweroff. A retry explicitly unbound VFIO, cleared the driver
override and disconnected the host TSM. All teardown steps completed,
including `RMI_PDEV_STREAM_DISCONNECT -> RMI_SUCCESS`, yet the same heap
diagnostic and model exit 134 remained. No Realm allocation owner ran.

Three bounded diagnostic cases and two follow-up controls reduced the
reproduction:

| Case | Linux / TSM activity | Observed result |
|---|---|---|
| Single-AHCI model construction, no firmware or payload, one simulated instruction | Neither Linux nor TSM | Natural model exit 0, 1.41 s |
| Minimal host using the pinned firmware/kernel and a tiny diagnostic init; no networking, 9P, pipette or test listing | Linux boots; AHCI stays bound; no TSM connect | Guest exit 0 and natural model exit 0, 11.58 s |
| Same minimal host, adding AHCI-driver unbind and host TSM connect/disconnect, under GDB | Both TSM operations complete; guest powers down | Guest exit 0, then model SIGABRT with `corrupted size vs. prev_size`, 13.69 s |
| Minimal host with AHCI unbind only, under the same GDB launcher | Linux completes; TSM connection stays empty | Guest, model and debugger exit 0, 11.63 s |
| The third case's exact model arguments and boot inputs, without GDB | TSM connect/disconnect, RMI disconnect and SPDM session termination complete | Guest exit 0; heap-corruption diagnostic and natural model status 139, 19.80 s |

GDB caught the third case's abort, killed the stopped inferior, and exited
134. This is not a recorded natural model exit. The earlier attempt at that
case exited 1 before Linux because the debugger launcher passed escaped
brackets literally; correcting the launcher produced the result above.

The captured stack detects corruption in `free()` during model destruction,
below `scx::scx_evs_base::~scx_evs_base()`. Stripped model callers include
`0x175920c`, `0x1545f0d`, and `0x1301081`; the AHCI worker was waiting in
`pthread_cond_wait`. The stack identifies detection, not the original
corrupting operation.

This reproduction does not require VFIO, a Realm VM, the allocation owner,
the test infrastructure, or GDB. AHCI unbind alone did not reproduce the
failure; the host TSM connect/disconnect sequence did. This narrows the
trigger for this fixture but does not identify the corrupting write or
separate connection from disconnection as the triggering operation. The
natural status 139 in the fifth case must not be relabeled as the earlier
134 or as a debugger-controlled exit.
Do not infer that a firmware rebuild or an allocation-owner change fixes it.
The object-lifecycle runtime gate remains open.

Commands, configuration differences, results and logs are retained under
`target/cca-tdisp-stage-a/realm-vfio-debug/`, with the initial matrix in
`summary.json` and follow-up controls in `controls-summary.json`.
The successful minimal controls are diagnostic results, not TDISP or DMA
qualification. No diagnostic process or container remains active.

Per the implementation priority, defer further investigation of this model
shutdown failure for now. Keep the model failure visible and separate from
the test-command outcome; do not relabel it as clean shutdown. This deferral
does not waive the Realm owner's allocation/cleanup checks, private-DMA
evidence, or interrupt requirements. The full OpenVMM guest trial now exercises
Realm-object creation and attachment. Clean object release remains unqualified;
an enumeration-only boot or optional dormant preflight cannot establish it.

### Single-boot diagnostic execution

The standalone Realm-object preflight is optional diagnostic coverage, not
a prerequisite for further implementation. The primary target is a VMM test
that launches OpenVMM, boots the CCA guest, activates TDISP, and verifies
device I/O. Actual object creation and cleanup can be checked as part of that
test. Keep the independent preflight for isolating failures when useful.

Add an explicit `--fvp-single-test <exact-name>` mode to
`cargo xflowey vmm-tests-run`, mutually exclusive with a user-supplied filter.
Keep the existing build/artifact discovery and pinned FVP validation, but
run the native AArch64 cargo-nextest executable inside one L1 session.
Enumeration and the one selected `tests`-binary test then occur before any
L1 poweroff.
The default per-test target-runner path remains unchanged.

After artifact copies complete, create private per-invocation inputs and
snapshot the native runner, test archive, and a derived nextest configuration
alongside the existing FVP inputs. Use guest-local temporary storage, serial
execution with no retries, an exact filter, and `--no-tests fail` so an empty
or ignored-only selection cannot pass. Preserve native JUnit output inside
the registered guest results directory, without modifying the source config.
Validate complete JUnit and the exact non-skipped test identity before
reporting its result. Build-only mode must never boot the model.

Record confirmed command results as soon as wait observes exit, before output
draining can time out. Use atomic, synced result updates and retain separate
teardown, launcher-shutdown and finalization outcomes. A failed FVP shutdown
must still fail the overall invocation, even if native tests pass. Use a fresh
output directory per invocation to avoid accepting stale result files. Resolve
JUnit through the invocation's registered output location, not a latest-run
search. Publish available evidence before reporting overall failure; unsafe
or incomplete result copying remains unavailable, not a pass. The execution
deadline covers native enumeration and the selected test together. This mode
is diagnostic progress, not clean-platform or trusted-DMA qualification.

The single-boot path and runtime reports are implemented but have not yet
been exercised together on FVP. Private run directories live under
`fvp-single-boot-runs`, outside the shared `test_results` and `temp` trees
that normal setup clears. The caller-known runtime report uses schema version
2 with `run_id` and `run_output_dir`; a failed overall report must not erase a
separately validated guest test result.

### Important refinements to the earlier findings

- OpenVMM already has PCI passthrough, VFIO cdev, IOMMUFD, nested SMMUv3 support,
  and an FVP test runner. They are not missing projects.
- The current CCA memory backend uses separate userspace and guest_memfd
  backing. Its RIPAS handler discards backing; it does not call
  `KVM_SET_MEMORY_ATTRIBUTES2`. The v7 reference uses mmap-able guest_memfd and
  explicit attributes on the **guest_memfd fd**. This is a memory-backend
  integration task, not just adding INIT_RIPAS. [O3, K1, V1]
- The existing TDISP state machine assumes OpenHCL protocol negotiation and has
  automatic-unbind semantics. It cannot be used unchanged as the native RHI
  state machine. [O1]
- A successful guest `tsm/accept` write is not enough to prove DMA enablement.
  This Linux revision logs an RSI DMA-enable failure but still returns success
  from `cca_device_accept`. The test must detect this condition and require
  private DMA I/O. [K5]
- Guest evidence checks compare received object digests with RMM-provided
  digests. This is not, by itself, a production certificate/measurement
  authorization policy. The FVP uses public test credentials. [K5, F1]

## 3. How the components fit together

```text
Development machine / L0
  cargo xflowey vmm-tests-run + nextest
    |
    +-- FVP incubator: pinned model, TF-A, RMM, PCI hierarchy, AHCI image
          |
          +-- Normal-world Linux / L1
          |     pipette control agent
          |       -> AArch64 vmm_tests executable
          |            -> OpenVMM worker + KVM Realm partition
          |                 |
          |                 +-- PCI root/port + VFIO AHCI presentation
          |                 +-- CCA assignment coordinator
          |                 |     tdisp lifecycle/report support
          |                 |     VFIO cdev + IOMMUFD Realm objects
          |                 +-- RHI adapter / ARM64_TIO completion
          |                 +-- guest_memfd conversion + DMA coordination
          |                       |
          |                  host Linux PCI TSM / CCA / SMMUv3 drivers
          |                       |
          +-- RMM: Realm/device binding, trusted mappings, device protocol
          |                       |
          +-- Model AHCI endpoint: DOE / IDE / TEE-I/O
          |
          +-- Realm Linux / L2
                PCI discovery + CCA guest TSM
                  RHI over RSI_HOST_CALL -> KVM_EXIT_HYPERCALL -> OpenVMM
                  RSI validation -> RMM -> KVM_EXIT_ARM64_TIO -> OpenVMM
                  RSI DMA enable -> RMM
                AHCI driver -> protected MMIO and private DMA
                guest pipette -> test control through virtio-vsock
```

The two pipette connections serve different purposes. L0 uses the incubator's
L1 connection to run the test executable. The test uses Petri's L2 connection
to control the Realm. Host VFIO binding and `tsm/connect` run in L1, never in
the Realm and never against the development machine's PCI devices.

OpenVMM mediates requests and manages assignment. It does not implement a new
SPDM stack, manage production endpoint keys, or replace RMM/guest acceptance.
The secure MMIO mapping is installed and validated by the kernel/RMM path;
it is not implemented as ordinary OpenVMM MMIO emulation. [K3-K5]

### Existing infrastructure and required extensions

| Surface | Existing implementation | Required change |
|---|---|---|
| TDISP | `tdisp`, `tdisp_proto`, OpenHCL client, VPCI dispatch, synthetic NVMe tests | Add native-host lifecycle support without changing the existing wire protocol |
| PCI assignment | `vfio_assigned_device` config filtering, BAR maps, MSI-X, cdev manager and resources | Add typed CCA mode, lifecycle/access coordinator, and AHCI interrupt support if required |
| Host IOMMU | `vfio_sys::iommufd`, IOAS/HWPT/vIOMMU/vdevice wrappers; `iommufd_nesting` | Add Realm vIOMMU type, TSM request ABI, KVM association, and S1-bypass Realm path |
| KVM | Realm creation, population, memory-fault handling, GICv3, in-place guest_memfd | Add Arm SMCCC exits/register completion, TIO exits and assignment prefault |
| VM assembly | CCA validation, PCI roots, device-tree generation, resource resolution | Admit only the new CCA-aware resource; wire partition/device services and address views |
| Petri | Realm boot and ordinary AArch64 VFIO tests | Combine their patterns with a DA-specific fixture and real lock/accept/I/O assertions |
| FVP | Validated v15 tuple, initrd boot, checked staging, logs, deadlines, cleanup | Add a separate DA tuple, payload, PCI assets, and L1 provisioning |

## 4. Integrating with `vm/devices/tdisp`

### Current rebased infrastructure

`TdispHostDeviceInterface` supplies negotiate, bind, start, unbind, and report
callbacks, plus the PR's MMIO Block/Unblock hook.
`TdispHostDeviceTargetEmulator` accepts `GuestToHostCommand` and
uses `TdispHostStateMachine` to call those callbacks. VPCI deserializes a
protobuf message, obtains `ChipsetDevice::supports_tdisp_host()`, and dispatches
it. Synthetic NVMe can provide that interface. OpenHCL has a matching client.
[O1-O2]

The protocol enum currently contains AMD SEV-TIO and Intel TDX Connect, not
native CCA RHI. Its report enum does not model all RHI object operations:
VCA, offset reads, object size, and regeneration with a nonce need separate
representation. The state numbers also differ: RHI uses 0/1/2 for
UNLOCKED/LOCKED/RUN, while the protobuf enum uses 1/2/3. Never cast between
them. [O1, K2]

The current state machine also:

- Requires protocol negotiation before transitions.
- Tries to unbind after some invalid requests.
- Changes its local state to Unlocked before the unbind callback completes.
- Has no physical-device quarantine state for an operation whose outcome is
  unknown. [O1]

The successful flows and intentional invalid-command cleanup policy must
remain compatible. Failed mutations need explicit uncertainty handling.
Do not make the Linux guest pretend to negotiate an OpenHCL protocol.

### Implemented additive baseline

The original implementation added a separate native host module under
`vm/devices/tdisp`. These pieces now exist:

| Existing piece | Responsibility |
|---|---|
| `tdisp::host::DeviceState` / transition helper | Confirmed device state, transition-in-progress, quarantined outcome, transition history |
| Native object/request types | Object identity, read offset/length, regeneration flags/nonce; no raw user pointers |
| `vfio_assigned_device::realm::tdisp` backend and assignment owner | Own the access gate, stable guest identity, IOMMUFD binding and backend operations |
| `virt_kvm` RHI adapter | Decode Arm registers and shared buffers; translate native results to RHI |
| Existing protobuf/VPCI adapter | Remains the OpenHCL/synthetic-device frontend |

Use one coordinator instance per assigned device for both RHI requests and TIO
validation. Do not create a separate TDISP state machine inside each exit
handler. Keep Linux ioctl code in `vfio_sys`/the VFIO backend, not in `tdisp`.
Keep Arm calling-convention code out of the generic device crate.

The additive approach deliberately left the legacy emulator unchanged.
That was sufficient for CCA bring-up, but is **not the next implementation
direction**: the plan below replaces both independent lifecycle engines with
one shared implementation and retains thin transport/backend facades.

`tdisp::devicereport` is useful for reading interface-report ranges for
diagnostics/access policy. Before using it for host access decisions, add
fixtures from the pinned Linux/FVP format and confirm byte order, range IDs,
count bounds, vendor-tail treatment, and page alignment. Keep the original
evidence bytes unchanged when returning them to the guest. Parsing an object
does not authenticate it. [O1, K5]

No OpenHCL image, VMBus, VPCI device, protobuf CCA protocol tag, or emulated
DOE device is required for the native Linux path.

The additive `tdisp::host` core is now implemented, with native state/object
types, explicit lifecycle transitions, quarantine on mutation errors, and
bounded transition history. Whole-object acquisition rejects empty or
oversized objects and inconsistent returned lengths. Snapshots use a shared
RAII budget across devices, fallible allocation, and checked slices. Mutations
and failed acquisition invalidate cached evidence. Legacy protobuf behavior
remains unchanged.

Typed `vfio_sys::iommufd::tsm` bindings now encode the pinned CCA request
layouts, retain host buffers for synchronous calls, and preserve syscall
errno, nonnegative residue and TSM code as separate results. Reads use only
backend offset zero.

The initial `RealmDevice::into_tdisp` path connected these components for
evidence reads.
It consumes only an attached, exclusively owned assignment, retaining the
original owner on a rejected transfer. Attachment can succeed without a TSM,
so a read-only certificate-size request first verifies a configured CCA TSM
and bound TDI. Successful CCA binding confirms UNLOCKED in the pinned kernel,
and this owner has never issued a TSM mutation. This verifies binding
provenance, not evidence authenticity. Complete positive size replies,
nonzero TSM codes, and read residue are checked before the snapshot core can
return guest slices.
Requests cannot use a prepared, cleaning or closed object owner.

Explicit teardown invalidates snapshots and uses dependency-ordered object
cleanup where release is allowed. A failed teardown retains the assignment.
The later full-assignment service adds native state, regeneration, MMIO and
serialized RAM operations behind the frontend access gate. That path has now
demonstrated LOCK/RUN and I/O; its UNLOCK/release restrictions remain open.

### Shared host refactor: current implementation plan

This section supersedes the earlier proposal to leave the two engines
separate. The background analysis is in
[findings-cca-tdisp-pr4416-reuse.md](findings-cca-tdisp-pr4416-reuse.md);
implementation decisions, sequencing and acceptance gates live here.

The PR's `VpciClientTdispState` and OpenHCL resource-validator hooks are
**guest-consumer** machinery. Our CCA OpenVMM instance is the **host**.
Do not move guest/RMM acceptance into the untrusted host or replace native
RHI with a VPCI client. Share the host engine beneath the two host facades.

```text
OpenHCL consumer                         Unchanged Linux Realm guest
  | VPCI/protobuf                         | RHI
  v                                       v
VPCI protocol facade                    virt_kvm adapter
TdispHostStateMachine                   EvidenceService worker
  |                                     Coordinator<B> facade
  +----------- shared Lifecycle engine --------+
               confirmed state + health
               transition legality/commit
               mutation failure/quarantine
               bounded transition history
                        |
              facade-owned backend
              /                  \
Emulator callbacks + MMIO gate    RealmDevice + AccessGate
                                 Linux TSM/IOMMUFD
                                 ^
                                 KVM TIO and whole RAM work enter
                                 through the same native service
```

Each device has one authoritative owner. This shares an implementation, not
one runtime instance between unrelated VPCI and CCA devices. Simultaneously
exposing both protocols for a physical assignment is not proposed.

#### State and ownership extraction

Add `tdisp/src/host/lifecycle.rs` with a shared `Lifecycle` engine (proposed
name). Move the native confirmed-state, health and transition types there,
with re-exports from `tdisp::host` to avoid widespread caller renames.
The engine owns authoritative state, mutation outcomes and bounded history.
It must not depend on protobuf, Linux handles, guest buffers, VTLs or reports.

Move the actual transition tables and mutation commit/error handling from
**both** implementations into it. Use a scoped synchronous transaction:
reject invalid requests before invoking work, mark uncertain state before
backend mutation, and commit confirmed state only after successful completion.
Error or unwind retains quarantine. No public unchecked state setter or
backend accessor may bypass this path.

`host::Coordinator<B>` remains the native backend/evidence facade, but loses
its independent state updates, transition table and `mutate` implementation.
It retains snapshots, the shared VM budget and native request types.
`TdispHostStateMachine` becomes the VPCI compatibility facade with the same
engine, callback handle, negotiation and bounded reason metadata. Move that
protocol code to `tdisp/src/vpci.rs`, keeping root re-exports and public target
names. Remove its independent `current_state`, `is_valid_state_transition`
and `transition_state_to`; `state()` becomes an engine projection.
`TdispHostDeviceTargetEmulator` only dispatches and formats replies. [S1-S2]

Sharing only an enum or legality helper is insufficient. Both facades must
use the same commit, quarantine admission, failure and history implementation.
Protocol reason history remains diagnostic, not a second lifecycle authority.

#### Operation contract

Do not collapse strict native state requests and idempotent VPCI Unbind into
one permissive state setter:

| Operation | Allowed healthy state | Successful outcome |
|---|---|---|
| Native LOCK / VPCI Bind | Unlocked | Locked |
| Native RUN / VPCI Start | Locked | Running |
| Native UNLOCK | Locked or Running | Unlocked |
| VPCI Unbind, including compatibility cleanup | Unlocked, Locked or Running | Unlocked; call backend even when already Unlocked |
| Native evidence regeneration | Locked or Running | Same state; invalidate snapshots |
| VPCI MMIO notification | Locked or Running | Same state |
| Native assignment/RAM work | Confirmed state and existing backend preconditions | Same state |
| Native reset | Confirmed state; preserve the existing backend invocation | Unlocked on success; backend error quarantines |
| Owner teardown | Confirmed or quarantined | TornDown only after acknowledged cleanup |

VPCI Unbind must leave a healthy device available for another Bind; it is not
terminal native assignment destruction. Repeated native state requests remain
invalid. Do not repurpose the emulator's currently no-op `reset()` as recovery.
Negotiation is VPCI policy, not a prerequisite for RHI.
Keep capability/input rejection before mutation wherever support is known.
Once a backend mutation is invoked, an error remains potentially committed,
including a backend validation error; do not infer safety from errno. [S1-S3]

**Reset decision:** preserve the existing invoked-reset failure behavior, not
add a reset-capability API in this refactor. `Backend` has no
`supports_reset()` query, and the Realm backend returns `MutationsDisabled`
from its reset callback. Thus reset on a healthy Realm coordinator still
invokes that callback once, invalidates snapshots, records the attempted
transition and quarantines on failure. This is not a preflight Unsupported
rejection. Test callback count, cache/budget release, last-confirmed state and
transition history explicitly. Do not expose a new RHI reset operation or
clear uncertainty through reset. [S2, S8-S9]

#### Concrete wire and error policy

The existing protobuf `Uninitialized = 0` already means uninitialized **or
indeterminate**. Use it for unconfirmed host state, with the existing failure
code; do not add a CCA protocol tag or invent a new healthy wire state. [S4]

| Situation | VPCI response | Native behavior |
|---|---|---|
| Successful transition | Success and confirmed before/after states | Existing RHI success |
| Decode/capability rejection before mutation | Existing error; healthy state unchanged | Existing input/unsupported status; no work |
| Invalid healthy Bind/Start or state-gated report | Explicit Unbind, then existing invalid-state/report error; after=Unlocked only if cleanup succeeds | Reject invalid transitions without effects |
| Invalid/wrong-state MMIO request | Existing error; no implicit Unbind | Keep native checks separate |
| Backend mutation or implicit cleanup fails | HostFailedToProcessCommand; after=Uninitialized; retain quarantine | Existing device error/fatal-guest handling |
| Pure report read fails | HostFailedToProcessCommand; retain healthy state unless containment failed | Preserve snapshot/access error semantics |
| Guest request while quarantined | Error, before/after=Uninitialized; no guest recovery | Existing quarantined/closed behavior |

On the first uncertain failure, `tdi_state_before` can contain its prior
confirmed value. Never project last-confirmed state as the current state
after failure. TornDown is not a successfully Unlocked live device.
Cleanup failure takes precedence over an earlier invalid-command error.

Golden-wire tests must pin validation order and response body shape.
GuestDeviceId is readable while Unlocked; report enum validation versus
state checks affects implicit cleanup. A recognized `Unknown` Unbind reason
still invokes cleanup, unlike an unrecognized numeric enum.

The actual VPCI client caches `tdi_state_after` **before** checking the result.
Test that Uninitialized plus failure yields that cache value and an error,
never acceptance. Preserve the existing explicit fatal cleanup policy rather
than hiding failure to continue. This intentionally changes failed-mutation
semantics; host/client fault tests and review gate the migration. Successful
wire behavior stays compatible. [S3-S5]

#### Access revocation is part of the contract

Quarantine bookkeeping does not itself revoke BAR or DMA access. Retain native
`AccessGate` denial, checked IRQ errors, attempted-map ledgers and physical
ownership. Do not replace them with report-based isolation classification.

For emulated backends, use an independently shared denial latch alongside
the `TdispMmioRanges` range set, not a callback that must reacquire the device
mutex during unwinding. An admitted mutation owns an access permit: entry
temporarily denies access, success restores eligibility under the ordinary
range rules, and error/unwind latches quarantine. Implement the permit with
local atomic state (healthy/in-flight/quarantined); Drop performs no locks,
Unbind or I/O. A successful completion must not overwrite a separately
latched quarantine. Only a new device owner can reset a quarantined latch.
The latch projects access denial, not a second authoritative TDI state.

**Coverage decision:** quarantine denies every emulated controller MMIO read
and write, including BAR4 MSI-X, not just BAR0. Healthy shared MSI-X access
still bypasses TDI acceptance as before. Today BAR4 bypasses the range-set
check, so wire the denial latch into both MMIO dispatch paths before they
reach registers or MSI-X state. Do not turn healthy Unbind into sticky
quarantine: clearing the normal range set must still allow a later Bind.
This latch does not itself prove DMA withdrawal; native DMA and physical
mapping containment remain backend responsibilities. [S6-S7]

Land the latch, transaction permit and all gate consumers together in step 3.
Test callbacks that change access and then either return an error or panic,
followed by actual BAR0 and BAR4 reads/writes. Re-negotiation, reset and later
Unblock must not reopen quarantine; healthy Unbind/rebind must work.
Resource-free mocks implement the containment contract explicitly, not through
a silent success default. Audit aliases so no callback can issue lifecycle
mutations outside its owner.

If revocation cannot prove physical release, keep quarantine and custody.
The refactor does not supply the missing CCA protected-map acknowledgement.

#### Evidence and execution remain role-specific

Keep native size/read/regenerate/VCA handling, snapshots and VM-wide budget.
Legacy report callbacks already return `Vec<u8>`; do not manufacture an
object-size API by fetching once for size and again for data. That would
change coherence, allocation and failure behavior. Both facades use common
healthy-state/transaction checks, but VPCI keeps fresh whole-report retrieval
and metadata rules. GuestDeviceId/IsRegistered are not native VCA evidence.
No new legacy cache or wire fragmentation is needed. [S1-S2]

An invalid local request must not invalidate native snapshots. An admitted
mutation invalidates them before backend work. Preserve post-read
access-health checks and the original diagnostic source on failure.

Keep `EvidenceService` and its mutex owning the whole native coordinator.
One admission spans the entire `RamWork::run`: DMA withdrawal, KVM memory
changes, prefault/mapping and failure handling. Keep device-before-memory
lock order, weak KVM routes, strong assignment ownership, and no service
re-entry from admitted work. Cancellation retains worker/owner/sink;
ambiguous completion prevents guest continuation. Teardown closes admission
before draining it. The synchronous VPCI facade must not wait on this async
service while holding its device lock. [S8-S9]

#### Reviewable implementation sequence

Create new changes above the qualified rebased baseline. Do not rewrite the
repaired stack again or replay its completed bring-up work.

| Step | Change and primary files | Exit gate |
|---|---|---|
| 1. Characterize contracts | `tdisp/src/tests/{statemachine,endtoend,serialize}_tests.rs`, `host/tests.rs`, VPCI client mocks | Pin success, validation order, callback counts and strict-native versus idempotent-VPCI behavior. Add before/after-effect fault controls. New failure expectations land with implementation, not as a failing intermediate commit. |
| 2. Extract engine and migrate native facade | Add `host/lifecycle.rs`; change `host.rs`; retain `host/evidence.rs` ownership | Native transition, snapshot, cancellation and RAM tests remain valid; no platform dependencies added. |
| 3. Migrate VPCI host facade and gates atomically | `tdisp/src/vpci.rs`, root re-exports, common-engine delegation, error projection, emulator denial latch/permit and all MMIO consumers | Both facades share one engine; remove superseded state logic; callback/error matrix and BAR0/BAR4 error/unwind containment tests pass. Include required mock and constructor edits here. |
| 4. Prove consumer/client behavior | `tdisp` helpers/mocks, NVMe integration tests, `vpci_relay` mocks/tests, focused `vpci_client` tests | Healthy rebind works, client rejects uncertainty, deferred config ordering remains. No intermediate commit may leave a gate consumer unwired. |
| 5. Audit native integration and remove dead code | VFIO resolver/backend, KVM RHI/TIO/memory, worker ownership | Keep one native owner and the complete transaction boundary; change callsites only when required. |
| 6. Runtime qualification | Existing Flowey runners and original inputs | Preserve v15 QEMU/FVP boot and the native TDISP LOCK/RUN/read milestone; report cleanup separately. |

The native backend need not implement the legacy `TdispHostDeviceInterface`
merely to share the engine. Keep TSM encoding, RHI calling conventions and
VFIO allocation lifetimes unchanged unless a specific integration need is
identified. Do not generalize the OpenHCL guest resource-validator trait into
a host backend. Report-classification sharing needs real CCA fixtures and is
not a prerequisite for this refactor.

#### Required validation

Run existing package tests, including the actual VPCI client/relay and NVMe
gate, not just the extracted engine:

```bash
cargo nextest run --profile agent \
  -p tdisp -p nvme_test -p vpci -p vpci_client -p vpci_relay -p openhcl_tdisp
cargo nextest run --profile agent \
  -p vfio_sys -p vfio_assigned_device -p virt_kvm -p openvmm_core -p petri
```

Run Arm KVM unit tests through the existing user-mode runner. Check/clippy
native and Arm consumers, run rustdoc, then full formatting before each
implementation commit.

Require assertions that both facades use the same engine, matching legal
requests invoke one backend operation, and no adapter keeps an independent
authoritative lifecycle. Test idempotent VPCI Unbind, strict native repeats,
wire validation order, post-effect failure, Uninitialized/error replies,
access denial, original error propagation and terminal owner teardown.
Query, reset and re-negotiation must not clear quarantine.

Retain snapshot bounds/byte identity and the native cancellation tests.
Attempt concurrent teardown during queued and running whole-RAM work;
neither individual DMA callbacks nor error handling may release admission
early. Unsupported requests known before mutation must have no effects.

Run the final FVP command after other nextest processes exit, because
Flowey's installer can otherwise hit `ETXTBSY`:

```bash
PYTHONDONTWRITEBYTECODE=1 INCUBATOR_TIMEOUT=1800 \
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca-realm-vfio.toml \
  --fvp-platform-root "$PWD/.packages/cca-tdisp-runtime" \
  --shrinkwrap-package-root "$PWD/.packages/cca-tdisp-runtime/package" \
  --cca-in-place-payload-root "$PWD/.packages/cca-tdisp-runtime/host-payload" \
  --cca-tdisp-guest-root "$PWD/.packages/cca-tdisp-runtime/guest-payload" \
  --dir "$PWD/vmm_test_results/cca-tdisp-shared-core" \
  --fvp-single-test \
  aarch64_exclusive::tdisp_ahci::openvmm_linux_aarch64_boot_linux_direct_cca_tdisp_ahci
```

Require fresh host-confirmed Locked/Run, ordered guest markers and the
full 64 MiB hash
`281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6`.
Record interrupts, native JUnit, UNLOCK, fixture/launcher exits and preservation
separately. Do not weaken the test to make known cleanup failures pass.
Failures before the I/O marker are regressions; a changed post-I/O failure
needs investigation, not automatic acceptance as the old guard.

Acceptance is shared-core contract coverage and preservation of the measured
protocol/I/O milestone. A clean full VMM test remains an unmet lifecycle gate.
No UNLOCK/kernel-reclamation redesign, deferred shutdown hold, same-host reuse,
private-buffer DMA measurement, physical VPCI backend, OpenHCL CCA guest, new
transport or in-place QEMU debugging is included.
Rollback uses the recorded source/inputs on a fresh test instance; it is not
live rollback of an uncertain physical assignment.

## 5. Configuration and ownership

Introduce explicit, experimental CCA-DA configuration. Recommended shape:

- A VM-wide v7 CCA memory/DA ABI selection, distinct from ordinary CCA v15.
- A typed CCA variant of the cdev assignment resource, carrying pre-opened
  cdev/IOMMUFD files and assignment policy. A distinct resource ID makes the
  CCA allowlist precise.
- CLI plumbing that creates that typed resource; use the existing `--vfio`
  and `--iommu` parser patterns. Final flag spelling is an implementation
  decision, not an existing command in this document.
- Equivalent direct resource construction for Petri, without going through
  shell command-line parsing.

Reject legacy VFIO containers, plain cdev assignment into a Realm, mismatched
VM/resource modes, guest-visible accelerated SMMU, and unsupported hosts.
Keep the current v15 allowlist unless the complete new mode is selected.
Validate both CLI input and worker configuration: tests/services can bypass
the CLI. Preserve CCA's no-VMBus, no-hotplug, no-snapshot, and no-migration
restrictions. [O3, O5]

The VMM should consume a device already bound to VFIO and connected to its
host TSM. A trusted launcher/fixture owns those host sysfs operations. Verify
the expected cdev, IOMMU group, TSM connection and host identity; never select
an arbitrary first PCI device.

### KVM and IOMMUFD object order

Implement this explicit dependency graph, with rollback for each step:

1. Create/configure the KVM Realm VM and its in-kernel GIC.
2. Create the KVM VFIO device; associate the VFIO cdev using
   `KVM_DEV_VFIO_FILE_ADD` **before** IOMMUFD binding.
3. Bind the cdev to IOMMUFD and retain its returned device ID.
4. Allocate the IOAS and nesting-parent HWPT. For the initial reference path,
   disable IOAS huge-page mappings as kvmtool does; do not confuse this with
   guest RAM hugetlb policy.
5. Allocate `IOMMU_VIOMMU_TYPE_ARM_REALM_SMMUV3` with the device/parent HWPT.
   Linux obtains the KVM association through the bound device and ensures the
   Realm exists.
6. Allocate a vIOMMU-backed S1-bypass HWPT and a vdevice whose `virt_id` is the
   guest requester identity. Allocate the vdevice before attaching that HWPT.
7. Attach the VFIO cdev to the resulting HWPT. Publish the assignment only when
   this and its memory/access prerequisites have succeeded. [K3, V2]

In v7, vdevice initialization calls `tsm_bind`, which creates the RMM VDEV.
That is **not** TDISP LOCK. Do not add the removed v6 standalone bind ioctl.
The normal OpenVMM nested SMMU path uses a different vIOMMU type and waits for
guest STE configuration. Reuse low-level object wrappers and ownership
patterns, not that guest-SMMU control flow. [O4, K3]

Add a narrow partition-owned KVM assignment service to the worker/resolver
wiring. Do not pass unowned raw KVM fds through unrelated device APIs.
Similarly, supply a typed native DA request service to the Arm VP path through
partition/device assembly. `CpuIo` currently has no TDISP operation. Either a
new explicit service parameter or an architecture-scoped device callback is
needed; `supports_tdisp_host()` alone does not connect KVM exits to the device.
[O2-O5]

### Owned host-object construction

Use a Linux-only, backend-neutral VFIO association provider at the PCI
boundary. The KVM provider creates the existing partition-owned bridge only
when requested. Worker/resolver wiring may carry this provider for CCA, but
must not change the current rejection of live CCA VFIO devices or create a
bridge for ordinary boots.

The Realm allocation owner retains the VFIO file, IOMMUFD context and KVM
association together. Preparation associates before binding, then allocates
an IOAS with huge-page combining disabled, a nesting parent, a Realm vIOMMU,
and an S1-bypass child. A separate attachment step takes the final guest
requester identity, creates the vdevice, and only then attaches the child
HWPT. No DMA mapper or guest BAR access is registered by these operations.
The caller must supply an unbound VFIO file under exclusive lifecycle
control; duplicate descriptors can delay final unbinding.

On a preparation or attachment error, attempt dependency-ordered rollback.
Stop at the first cleanup error, preserve both the original operation error
and cleanup error, and retain the remaining handles and object IDs in a
recoverable owner. Clear each ownership marker only after successful cleanup.
An attempted attachment must be detached even if its completion is ambiguous;
do not destroy an unexpected returned HWPT ID that this owner did not allocate.
Attachment is allowed once from the prepared state. Once cleanup starts,
only cleanup retry is allowed, not renewed setup or attachment.

Cleanup order is detach, vdevice, child HWPT, vIOMMU, nesting parent, IOAS,
KVM file association, VFIO file close, then IOMMUFD and partition handles.
Skip absent objects. The bound device ID is bookkeeping, not an independently
allocated object to destroy; final VFIO file release ends the binding.

Normal teardown is explicit and retryable. If a caller abandons an owner whose
cleanup still fails, log the failure and retain the entire remaining resource
bundle until process exit rather than release memory or associations out of
order. This is a fail-closed fallback, not clean teardown or a reuse result.
The later live-DMA path must stop access before calling this cleanup path.

Mock tests must cover every allocation and cleanup failure boundary, retained
state and retry, association-before-bind, vdevice-before-attach, and the exact
cleanup order. Drop-counter tests must prove failed recovery retains the
entire resource bundle. Wiring tests must preserve lazy provider creation,
ordinary boot behavior, and the existing CCA VFIO rejection cases.
Actual FVP object-lifecycle testing still requires the DA
fixture; mock results do not close that runtime gate.

The provider and unmapped object owner are now implemented. Mock coverage
checks allocation/cleanup boundaries, ambiguous attachment, final-RID ordering,
and retention of all resource handles on failed cleanup. The existing
in-place FVP Realm boot regression also passes (nextest run
`d19f865a-4932-4d92-bd20-7c4f1ec62b42`, 243.217 s), confirming ordinary
boot/teardown with provider wiring present. This run does not allocate a
Realm vdevice. Actual DA object-lifecycle qualification remains outstanding.

### Guest identity and topology

Store this mapping per VM:

```text
guest segment:bus:device.function / 32-bit requester identity
  -> assigned device + host BDF
  -> IOMMUFD device ID, IOAS, parent HWPT, vIOMMU, child HWPT, vdevice ID
  -> guest BAR intervals, host report offsets, current access/lifecycle state
```

The Linux guest computes `(segment << 16) | PCI_DEVID(bus, devfn)`.
This is not the IOMMUFD vdevice object ID. TIO's `vdev_id` and RHI requests
must resolve through the same VM-local map. [K2]

For the first fixture, use one segment, one static endpoint, and fixed
boot-assigned bus numbers/BARs. Reuse PCI routing notifications and
`preserve_boot_config` support. Establish the final RID before creating the
Realm vdevice; retain the ordinary guest-SMMU lazy-routing path unchanged.
Before first lock, confirm that enumeration has not changed the identity.
Reject bus renumbering, BAR relocation, function reset, or assignment-changing
config writes while locked/running. Unlocked reconfiguration must either
rebuild the binding safely or be explicitly unsupported. [O4-O5]

## 6. Memory: the first implementation gate

### Why the original separate-backing backend needed work

This section records the baseline requirements. The implementation status
above identifies the completed memory work; assignment-specific coordination
below is still required.

The current `GuestMemfdDefault` mode creates guest_memfd with flags zero,
registers it alongside a separate userspace mapping, populates imported pages,
and discards old backing on conversion. The INIT_RIPAS, attributes2 and
prefault wrappers now exist, but this mode does not use them. [O3; implementation
status above]

The pinned v7 reference creates RAM with
`GUEST_MEMFD_FLAG_MMAP | GUEST_MEMFD_FLAG_INIT_SHARED`, maps that fd, and issues
`KVM_SET_MEMORY_ATTRIBUTES2` on it. Linux completion checks the backing
attributes before completing a RAM RIPAS change. Keeping the old handler
unchanged can repeatedly exit without completing the requested conversion.
[K1, V1]

**Recommended baseline:** implement an opt-in guest_memfd in-place mode that
matches this kernel/reference design. Do not transplant its operations onto
the old separate-backing path and retain destructive discard calls.

### Required work

1. Add capability checks for guest_memfd flags and memory attributes, and
   bindings for mmap-enabled creation and the 128-byte
   `kvm_memory_attributes2`. It uses file offsets, not GPAs, on guest_memfd.
   Handle `error_offset` and partial conversion failure. A copyback failure
   does not prove that no conversion occurred. [K1]
2. Integrate the partition-owned guest_memfd backing with `membacking` so
   shared CPU/device accesses use that backing. Avoid a second unrelated
   shared RAM copy. Use explicit packed RAM-range/file-offset translation.
3. Preserve a separate loader source buffer for measured population. Convert
   destination ranges to private, populate and measure the imported image,
   and initialize all configured guest RAM to RIPAS_RAM before activation.
   Split INIT_RIPAS calls at memslot boundaries. Do not initialize holes,
   PCI windows, or device memory as RAM. INIT_RIPAS itself sets backing
   attributes to private; update the host-access and DMA ledgers for this
   side effect, not just for explicit attributes2 calls. Keep initial shared
   IOAS mappings empty through population/initialization, or withdraw any
   existing pins before either operation. Record successfully initialized
   RAM as private before publishing the assignment. [K1, V1]
4. Prefault all initial private RAM through a vCPU before any vCPU runs when
   a DA device is present. Check alignment, full progress, partial progress,
   interruptions, retryable errors, and zero-progress failure. [V1]
5. Decode both forms of `KVM_EXIT_MEMORY_FAULT`: supported negative `KVM_RUN`
   returns and successful returns carrying that exit reason. The initial
   RIPAS exit returns `-EFAULT`, but completion can re-exit with return zero
   when backing attributes still disagree. The low-level decoder now exposes
   both forms, but the successful form is explicitly rejected by the existing
   backends. Preserve the originating errno and pending-exit
   state across re-entry and `complete_exit()`, including stop/cancellation.
   [O10, K6]
6. Classify memory-fault causes before changing visibility. Backing acquisition
   failures can also produce memory-fault exits with the private flag clear;
   that flag alone is not a request to share RAM. Handle resource failures
   and poisoned backing separately, with an explicit error policy. If the
   available ABI/state cannot distinguish a backing failure from a conversion,
   stop with a diagnostic; resolve any required kernel-interface gap rather
   than guessing a visibility change. Test classification before admitting
   the coordinated conversion path. [K6]
7. Extend the in-place RIPAS handler with coordinated assignment conversion:
   withdraw shared IOAS mappings before shared-to-private conversion; change
   guest_memfd attributes; prefault private ranges; map newly shared RAM into
   IOAS after conversion. Complete the KVM exit only after required work has
   succeeded. Use the negotiated shared-IPA selector for shared IOVAs.
8. Distinguish RAM conversion from device RIPAS invalidation. Linux v7
   supports DEV-to-EMPTY without guest_memfd attributes. Do not reject every
   non-RAM conversion or prefault a BAR. Validate non-RAM intervals against
   this VM's tracked protected mappings. [K1, K4]
9. Serialize conversion and mapping changes against access requests and
   teardown. Block a VM/device on ambiguous or partial failure rather than
   resuming with stale mappings. Keep host-accessible range tracking and
   guest-buffer validation consistent.

`membacking::DmaTarget` receives region mapping changes, not CCA visibility
changes. Its use of "private RAM" means anonymous host backing, not Realm
private memory. The ordinary IOMMUFD target maps all active ranges with
unmodified GPAs and may use map-by-file. Neither behavior is sufficient for
CCA. Add a CCA-aware DMA registry/target and explicit conversion notifications;
do not eagerly pin/map the entire private guest_memfd into the shared IOAS.
[O4, O6]

Keep a range ledger for shared IOAS mappings and validate unmap coverage.

The pinned kernel's `IOMMU_IOAS_MAP_FILE` path accepts shmem and hugetlb files,
not guest_memfd. Match kvmtool by mapping newly shared RAM through a host
virtual address with `IOMMU_IOAS_MAP`. Retain an owned guest_memfd mmap view
and its file in the IOAS ledger before each request, including uncertain
failures; release the view only after all corresponding IOAS unmaps succeed.
This maps the existing shared backing, not a second RAM copy.
No P2P BAR exports are needed in milestone one. Generic best-effort P2P mapping
failure behavior must not become success for required CCA DMA mappings.

The exact `membacking` backing-import API and scheduling/lock order need a
focused design pass during the memory milestone. The required outcome is
fixed: one coherent backing, correct access restrictions, no stale DMA alias,
and no old discard operation that destroys in-place data. An in-place Realm boot and
both conversion directions must pass before enabling physical assignment.

## 7. Guest RHI and trusted-I/O transport

### KVM bindings

Add these to the low-level wrappers, with size/offset tests against the pinned
headers rather than the build machine's installed headers:

| Interface | Contract |
|---|---|
| `KVM_ARM_RMI_INIT_RIPAS` | 64-byte request; zero flags/reserved fields; one guest_memfd memslot |
| `KVM_PRE_FAULT_MEMORY` | 64-byte mutable request on a vCPU; consume all progress |
| `KVM_SET_MEMORY_ATTRIBUTES2` | 128-byte request on guest_memfd, offset-based; preserve failure location |
| `KVM_EXIT_MEMORY_FAULT` | Handle successful and supported error-return exits; retain errno and classify the cause before conversion |
| Arm `KVM_EXIT_HYPERCALL` | Decode function/flags, read arguments and write results using Arm registers |
| `KVM_EXIT_ARM64_TIO` | Exit 44 in this tree; seven 64-bit fields in `cca_exit` |
| `IOMMU_VDEVICE_TSM_REQ` | 48-byte outer request; operation, architecture, lengths, two pointers, `tsm_code` |
| Arm TSM payloads | Validate-MMIO 24 bytes; state/object-info 4; object-read 16; regenerate 24 |

The pinned kvmtool headers call the TSM ioctl
`IOMMU_VDEVICE_TSM_GUEST_REQUEST`; Linux calls it `IOMMU_VDEVICE_TSM_REQ`.
Their command number is 0x96 and their outer layout is 48 bytes. Use Linux's
names for new bindings and assert encoding/layout compatibility rather than
copying a name from the reference VMM. [K3, V4]

The x86 `Exit::Hypercall` remains separate from the new `Exit::ArmHypercall`,
which carries Arm's `nr`/`flags`, not the x86 argument/result contract.
`Processor::read_arm_smccc_function`, `read_arm_smccc_arguments`, and
`write_arm_smccc_results` use `KVM_GET_ONE_REG` and `KVM_SET_ONE_REG`, including
results in x0-x3. The evidence adapter checks original x0 against the exit
number: the pinned kernel narrows `hypercall.nr` to 32 bits, so that field
alone cannot enforce full-value validation. Kernel PSCI handling remains
unchanged. [O3, K2, V3]

Install forwarding filters before first run for the two exact DA ranges:
`0xc500004b..=0xc500004d` and `0xc5000052..=0xc5000054`.
RHI uses SMCCC owner 5 (`STANDARD_HYP`), not owner 4. Derive these constants
from the pinned definitions and test their full encoded values.
Leave host-configuration calls at `0xc500004e..=0xc5000050` to KVM. Validate each function;
do not forward a broad range of unrelated SMCCC calls. [K2, V3]

`Partition::set_arm_rhi_da_filters` now provides those exact, opt-in filters.
It is not called by normal partition setup. Explicit pre-run evidence service
registration installs it. `Exit::ArmTio` preserves the
seven-field trusted-I/O packet and starts with a rejecting response.
`ArmTioExit::accept` refuses unknown reasons or flags; known mapping requests
still require device/address-policy validation before acceptance. The Arm
`virt_kvm` loop rejects/stops on unregistered hypercalls and trusted-I/O exits.
Evidence-only registration still enables only the two evidence feature bits
and size/read handlers. Full assignment registration additionally prepares
private RAM and enables the base RHI operations and TIO mapping requests.
The latter path is used by the demonstrated guest trial.

### Request translation

#### Evidence routing ownership

Connect the evidence-only service through the existing VM association handle.
The VFIO caller retains a strong service reference; the partition registry
holds only weak references keyed by the final guest requester ID. This avoids
a cycle through the service's partition-owned VFIO association. Registration
is explicit and allowed only before the first VP run. The first registration
installs the exact RHI filters; a partial filter failure poisons the partition
and cannot be retried. Ordinary VMs install no filters.

Use the existing blocking worker pool for synchronous evidence operations.
An asynchronous admission gate permits only one scheduled or executing
worker per device; do not submit workers that wait for the device lock.
Each admitted worker owns its permit, coordinator and sink until completion,
even if the waiting future is cancelled. Cancellation while awaiting admission
submits no work. Keep the device lock through snapshot selection and delivery
to a host-provided output sink; do not allocate an unbudgeted copy of the
object. Only host management exposes explicit teardown. Teardown closes
admission, drains accepted work, and rejects queued reads before issuing
cleanup. Failed cleanup retains the closed owner for explicit retry.

Freeze registration under the registry mutex at entry to the first `run_vp`,
before its initial await, even if no device was registered. Reject duplicate
live requester IDs; expired weak references resolve as missing devices.
Never reopen or replace registrations after freeze. Failed filter setup blocks
both ordinary entry and `complete_exit`, which also invokes `KVM_RUN`.

The KVM output sink accepts only selector-clear, nonempty RAM byte ranges.
Under the partition memory lock, check current slot coverage, the in-place
visibility ledger, and the fatal flag, then copy with `GuestMemory::write_at`.
Keep that lock through the copy so conversion and slot removal cannot race
validation. Do not hold the memory lock across an evidence ioctl. Lock order
is device coordinator, then partition memory; conversion must not acquire the
device coordinator while holding the memory lock.

Initially route FEATURES, OBJECT_SIZE and OBJECT_READ only. FEATURES returns
the two implemented evidence bits directly in x0. Reject unsupported calls,
unknown function/flag bits, invalid requester/object IDs, overflowing offsets,
and invalid shared buffers with explicit protocol results. Backend and copy
failures retain typed diagnostics and never report a successful byte count.
The full Linux DA base feature set remains unadvertised; regeneration,
LOCK/RUN and TIO acceptance remain disabled.

That paragraph describes the initial evidence-only stage. The full assignment
path now advertises the base feature set after private-memory preparation and
routes state, regeneration and TIO operations through the same admitted owner.

GET/SET_ONE_REG failures poison the partition; partial result writes must
never reach guest re-entry. Preserve the existing stop contract, which waits
for the current VP operation to complete. An abandoned RHI future also poisons
the partition and forces peer VPs out of KVM, preventing re-entry while an
admitted worker can still access guest memory. A failed guest copy may have
written a prefix: report
ACCESS_FAILED with other result registers zero, not atomic rollback.

Verify registration/freeze races, partial filter failure, weak-reference/drop
ordering, bounded admission, cancellation, teardown/queued-read ordering,
conversion/unmap versus copies, and full-width register/ID decoding. A later
live diagnostic must issue evidence calls directly: the pinned Linux driver
rejects the partial FEATURES mask and will not exercise this path on boot.

| RHI request | Host action |
|---|---|
| FEATURES | Return only implemented feature bits |
| OBJECT_SIZE | Query and acquire a bounded whole-object snapshot; return its verified size |
| OBJECT_READ | Serve a bounded slice of the current object snapshot, then copy to validated shared guest memory |
| GET_INTERFACE_REPORT | Regenerate the interface-report object |
| GET_MEASUREMENTS | Read the guest parameter block, copy its 32-byte nonce, regenerate measurements |
| SET_TDI_STATE | Serialized LOCK/UNLOCK/RUN operation with access coordination |

#### Object snapshots for the pinned backend

Do not forward arbitrary guest read offsets/chunk sizes to the pinned Linux
backend. `cca_vdev_read_cached_object` requires capacity for the whole object
and copies that whole length starting at `buf + offset`; it does not implement
a bounded slice. kvmtool forwards those parameters unchanged. The normal guest
avoids this mismatch by reading the whole object at offset zero. [K7, V3]

For the initial implementation, fetch the whole object at **backend offset
zero** into owned host storage and serve guest slices from that snapshot.
Query its size with `TSM_REQ_OBJECT_INFO`, require the full 32-bit size result,
enforce the 16 MiB object bound and a bounded per-VM snapshot budget, then
check the returned length against both the allocation and queried size.
Return an explicit error on inconsistent size/read results; do not expose
partially initialized storage or retry without a bound.

Serialize snapshot acquisition with regeneration and device transitions.
Key snapshots by device, object and coordinator generation. Invalidate them
on regeneration, lock/unlock, reset, teardown, or uncertain backend outcome.
Serve size/read from a coherent current snapshot; do not combine chunks from
different generations. A request without a usable snapshot must acquire one
or fail explicitly. Guest/RMM evidence checks remain authoritative for changes
outside the coordinator. This is a VMM compatibility measure, not a fix to the
kernel's offset-read implementation. Track a Linux fix separately. [K7]

#### Guest buffer encoding

The guest measurement structure places the nonce at byte 0x100, not immediately
after its 64-bit flags. The ioctl payload's nonce is a host pointer to owned
bytes. Do not pass the guest IPA as that pointer.

The pinned guest converts buffer backing to shared and then passes
`virt_to_phys()` addresses, which can have the shared selector **clear**.
Validate sharing from the current backing/access ledger, not from the address
bit alone. Accept that guest encoding when the entire RAM interval is
host-accessible/shared; reject the same address when its backing is private.
For this initial ABI, require the pinned guest's selector-clear buffer encoding
and explicitly reject selector-set aliases rather than normalizing them
silently. This buffer rule is distinct from shared DMA IOVA construction.
Keep validation and copying synchronized against conversion, and check full
range coverage, offsets, lengths and arithmetic. [K2, K8, V1]

Use synchronous backend requests for the initial implementation. Do not
advertise CONTINUE or return INCOMPLETE unless continuation/cancellation has
been implemented. The guest supports those operations but the base feature
set does not require them. Keep slow ioctl work off the async executor's
critical path; cancellation must not release buffers/fds while a syscall is
still using them.

Preserve three result channels: syscall error, nonnegative residue, and
`tsm_code`. Validate residue against the offered request/response length.
The CCA backend does not populate meaningful `tsm_code` on every path;
zero is not independent proof of success. Early validation can fail before
copyback, while a copyback `EFAULT` can occur after a state-changing operation.
Classify the latter as uncertain completion and quarantine access. Ordinary
backend errors can also follow a completed RMI state change: this kernel
records LOCKED/STARTED before waiting for device communication. Treat any
state-changing error as potentially committed unless there is positive
evidence to the contrary. Do not blindly retry LOCK or restore shared BAR
access. [K3-K4]

### TIO completion

For `RMI_EXIT_VDEV_VALIDATE_MAPPING`, resolve the guest ID, check the interval
and device policy, and send `gpa_base`, exclusive `gpa_top`, and `pa_base` to
`TSM_REQ_VALIDATE_MMIO`. The host backend calls `realm_dev_mem_map`.
Write `cca_exit.response` before re-entry: zero permits kernel/RMM validation;
nonzero rejects. Unknown subreasons and invalid requests must never get a
default zero response. [K3-K4]

Do not convert this to an ordinary `MmioRead`/`MmioWrite`. Keep raw private,
shared-alias, PCI bus, and host report addresses distinct throughout lookup.
The device tree currently applies a shared address transform to PCI CPU
windows. Linux builds protected MMIO ranges from those resource addresses;
its setup operation labels them encrypted but does not translate them to
private IPAs. RMM requires protected IPAs for validation. A DA-specific
firmware/address-view change is therefore required, not optional. [O5, K5, F2]

Design a private resource view for the assigned endpoint while preserving the
shared ECAM, virtio, and nonsecure interrupt views. The guest's ioremap hook
selects mapping protection from RIPAS, so a private-address resource can use
shared access before acceptance and protected access after validation and
driver reprobe. Qualify the smallest address-view change first; a separate DA
aperture or root was initially a candidate, not a general requirement. The
current single-device implementation uses a dedicated root and passed the
unchanged guest's LOCK/RUN and I/O sequence. [K8]
Do not clear the shared bit only in the VMM's
TIO handler: RMM checks the guest's RSI arguments before that exit. Test that
the guest sends private IPAs for protected AHCI ranges, while shared device
traffic still works. Do not mark all PCI accesses private.

## 8. BAR access, interrupts, and teardown

### Transactional device access

Maintain an access gate separate from guest-visible PCI BAR register values:

```text
Prepared/Unlocked -> Withdrawing shared access -> Locked
Locked -> Validating protected ranges -> Run requested
Run requested -> guest RSI DMA enable (observed by tests, not asserted by VMM)
Locked/Run -> Unmapping protected access -> Unlocked
Any uncertain transition -> Quarantined; no normal access or reassignment
```

On LOCK, withdraw direct shared mappings **and** fallback MMIO reads/writes
to protected register ranges. The existing BAR updater returns no error and
can log mapping failures; a trusted transition needs a checked, acknowledged
operation. Quiesce accesses before invoking the backend. On failed LOCK,
restore access only when the backend outcome is known to be unlocked.
Inject failure after RMI LOCK/START but before device communication completes;
the same quarantine rule applies even when the syscall returns an ordinary
device error rather than `EFAULT`.

Track PCI configuration changes and resets under the same gate. Otherwise a
BAR write can recreate a shared alias after LOCK. Do not hold a PCI mutex
while awaiting work that must reacquire it. Define worker/coordinator lock
order and use request completion to serialize changes.

MSI-X table/PBA pages need separate handling. OpenVMM already subtracts these
pages from direct mappings, but still emulates accesses to them. Reuse that
range splitting; do not copy kvmtool's whole-BAR exemption. Reject layouts
where protected registers and required nonsecure interrupt structures cannot
be isolated at the mapping granule. Secure MSI-X is out of scope. [O4, V2]

### AHCI interrupts are a prerequisite, not an assumption

The existing assigned-device frontend implements MSI-X. The device-tree PCI
path explicitly has no legacy INTx interrupt map. The TF-RMM recipe proves
an AHCI baseline exists for kvmtool, not that OpenVMM's MSI-X-only frontend can
drive that endpoint. [O4-O5, F1]

Run 3 established that the modeled AHCI endpoint uses MSI-X with kvmtool.
Thus ordinary MSI or INTx support is not a demonstrated prerequisite for this
fixture. Still validate OpenVMM's MSI-X routing and protected/nonsecure range
handling on the same endpoint. Capture PCI configuration, VFIO IRQ capability
queries, and the mode used by the AHCI driver. If another supported fixture
needs MSI, implement
ordinary MSI capability virtualization, VFIO eventfd setup, and GIC routing.
If it only works with INTx, implement level/mask/resample handling and the
guest DT interrupt map. Gate the test on the supported observed mode.
Do not substitute polling or silently change to another device and claim
the planned AHCI test passed.

### Lifecycle

Provide an explicit async shutdown path, not just best-effort destructors:

1. Stop new requests; quiesce DMA and interrupts and drain in-flight work.
2. Prefer guest driver unbind and guest TSM unlock while the Realm is live.
   Confirm protected mappings were invalidated before restoring shared access.
3. Withdraw any remaining mappings. Detach the VFIO device from its HWPT,
   destroy the vdevice (which triggers TSM unbind), then release dependent
   child HWPT/vIOMMU/parent/IOAS resources in valid reference order.
4. Remove the KVM VFIO association and close files only after their users
   have stopped. Retain the KVM partition until its assignment dependencies
   are gone. Unmap shared IOAS ranges before releasing backing.
5. Let the fixture disconnect/restore the host device only after the VMM
   reports safe release.

The host kernel comments require protected device memory to be unmapped before
TSM unbind. They mention teardown paths that must be checked against actual
available APIs; do not invent an `unmap_private_range` ioctl from a comment.
The inspected KVM UAPI does not expose that named operation. Forced teardown
with live protected mappings therefore needs explicit kernel/VMM qualification,
not a guessed object-close order. [K3-K4]

Guest sysfs success also does not prove safe unlock. The guest unlock callback
cannot return an error, can fail invalidation, and ignores an unlock result;
the PCI sysfs write can still succeed. Require coordinator/kernel mapping
evidence before acknowledging safe release. The failed-accept path needs
separate qualification: RUN-failure cleanup uses an unmap helper that reads
`dsc->pci.mmio`, but acceptance publishes that field only after RUN succeeds.
Test partial protected mapping and post-mapping RUN failures. Keep cleanup
uncertainty visible and record any required kernel fixes separately. [K5]

If a clean release cannot be established, quarantine the device, retain
diagnostics and end that FVP instance. A fresh FVP run is a development
recovery boundary, not proof of same-host reuse. Repeat assignment within one
L1 is a separate required lifecycle test before enabling reuse.

## 9. FVP platform and artifact work

### Preserve the working v15 platform

The current runner accepts one exact manifest and fixed CCA payload hashes.
The profile rejects extra devices and accepts only the `cca` capability.
Runtime and staging recheck those identities. Supplying a new archive hash
or `--custom-kernel` does not make a v7 kernel an accepted FVP payload.
[O7-O8]

Add a separate, checked-in DA platform selection and payload identity, for
example `CcaPlatformVariant::DaV7` and
`petri/incubator/profiles/aarch64-fvp-cca-tdisp.toml`. These names are proposed.
Thread the selection through Flowey resolution, runner environment/config,
profile parsing, platform validation, staging, runtime verification, and
artifact discovery. Do not disable hash checks or overwrite the existing
v15 manifest.

### DA package

Use the pinned TF-RMM `cca_da.yaml` and `model-enable-da.yaml` as the recipe
reference. Resolve moving TF-A/toolchain/Shrinkwrap inputs before qualification.
The current v15 manifest records model 11.31.28, but that is not proof it
supports this DA configuration. [F1, O7]

The new package inventory must include:

- TF-A/RMM firmware, host DTB, effective model/build overlays, and exact hashes.
- Linux v7 Image, effective config, modules if used, and source/build manifest.
  Prefer one unified host/guest kernel initially, with separately prepared
  initrds.
- PCI hierarchy, measurements, public model test credential assets and their
  provenance. Use them only for this model fixture.
- A dedicated 64 MiB AHCI disk with a known nonzero data pattern/hash.
- The pinned kvmtool binary for baseline comparison, and OpenVMM/test/pipette
  artifacts for the Petri run.

The DA overlay enables PCI TSM/DOE, host and guest CCA/TSM support, IOMMUFD,
SMMUv3 IOMMUFD, and VFIO cdev. Check the **effective** kernel configuration:
also require PCI enumeration, AHCI/libata/SCSI disk, virtio-vsock, the L1
network/9p path and test utilities. Include built-in drivers or matching
modules in the correct initrd. Verify that the guest init process permits
sysfs operations and bounded driver probing. [F1, O8]

Adapt the DA recipe to the existing initrd-based incubator. Do not reintroduce
the reference's public block-rootfs input just to carry test artifacts.
Stage all DA files into the owned per-run workspace, rewrite model asset
paths to those copies, and validate the effective command. Give each run its
own AHCI image copy. Preserve existing lifecycle locks, deadlines, cancellation,
source revalidation, output collection and ownership-safe cleanup.

The upstream model overlay uses one-entry TLB/GPT-TLB workarounds. Record
them in the result manifest; passing this setup does not prove hardware
invalidation correctness or physical-link encryption.

### L1 provisioning and capability publication

Extend the FVP backend with a typed modeled-AHCI fixture, not arbitrary QEMU
`devices` settings. Discover the expected host endpoint by topology and PCI
identity, ensure it is not hosting a mounted filesystem, bind it to VFIO,
and write the discovered TSM name to its `tsm/connect`.

Publish a new typed capability such as `cca_tdisp_ahci` and the fixture BDF
only after the selected platform, L1 kernel, TSM connection, cdev and IOMMUFD
prerequisites pass. Reuse the ordinary incubator VFIO device-name/BDF convention
where possible. Register the capability in Petri's shared capability table.
Do not let caller-supplied environment variables fabricate it.

Build the reference baseline before switching to OpenVMM: same firmware,
kernel, AHCI image and model configuration; first Realm boot without DA, then
kvmtool lock/accept/I/O. Record reference failures separately. Reference
error-handling shortcuts are not acceptable OpenVMM behavior.

## 10. The `vmm_test`

### Current execution priority: unchanged reference guest

The immediate milestone is LOCK/RUN and disk I/O in the isolated FVP instance.
Defer shutdown, teardown and same-host reuse redesign until that execution
path is understood. For this controlled trial, keep the test controller,
OpenVMM and its RAM alive while observing the guest, then end the entire FVP
instance. Do not make a new cleanup interface or a guest helper a prerequisite.
This does not qualify parent-disconnect, forced process termination, physical
hardware recovery or clean resource reclamation.

Do not add a guest helper or require a separate evidence-only/preflight test.
Complete the OpenVMM host path, then run the unmodified guest used in the
successful Stage A run `run-20260914-3`. Its own initrd already runs the
TSM lock/accept, 64 MiB direct read, hash check, unlock and poweroff sequence.
The guest discovers AHCI by PCI class rather than a fixed guest BDF.

The exact guest inputs are:

| Input | SHA-256 |
|---|---|
| `runs/run-20260914-3/share/Image` | `6bef4c54ac93d8513ad7f125737c9ff0a8b0b7e34720e77253e2b63460002437` |
| `runs/run-20260914-3/share/guest-initrd.cpio` | `d3ba987d83bd46a60cf2199d7989a7b940499065e1011125775034b5710dad92` |

Paths are relative to `target/cca-tdisp-stage-a`. These are not the later
instrumented `private2` guest or the ordinary Petri in-place payload.
`--cca-tdisp-guest-root` stages them separately under `cca-tdisp-guest/`;
it must not replace the L1 host kernel/initrd or rebuild firmware.
Only platform console arguments change for OpenVMM's PL011 device.

Provision a new `realm-vfio-reference-platform` package with the saved run-3
AHCI disk, SHA-256
`281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6`.
The firmware-build package's zero-filled disk is not a valid substitute for
the guest's unchanged oracle. Preserve old inputs and reuse firmware without
rebuilding. The newer L1 in-place kernel and ordinary Petri initrd remain
host inputs, separate from the guest pair above.

The initial Realm frontend uses intercepted shared BAR accesses, rather than
the ordinary VFIO direct-map path. A single access gate can therefore drain
and withdraw those accesses before LOCK without relying on the existing
infallible BAR-unmap API. MSI-X table/PBA pages stay separately emulated.
A dedicated static root supplies selector-clear BAR resource addresses while
other roots and ECAM retain their shared views.

Missing protected-unmap acknowledgement blocks a clean-release claim, not
necessarily the bounded LOCK/RUN/I/O attempt. Do not remove tracked attempted
protected mappings based on an ordinary guest exit or guest sysfs success.
RMM checks actual mappings during acceptance, but Linux's mapping rollback
can hide the original error. On uncertain release, stop guest execution and
new operations, contain DMA, and retain all reachable backing and assignment
dependencies. If that containment cannot be established, stop before enabling
the device. An I/O result followed by failed unlock/teardown remains an overall
failure, not a clean lifecycle or private-DMA qualification.

The retention boundary applies to guest exit, timeout, cancellation, partial
acceptance and OpenVMM teardown failure. Ordinary test cleanup must not
destroy containment resources. If model termination is the recovery boundary,
keep those resources until termination is confirmed. Establish this before
device enable; do not infer containment from process exit.

For this first trial, observe the original initrd's ordered READY, LOCK,
ACCEPT, complete-image hash and UNLOCK markers through PL011. Do not inject
pipette or drive its sysfs sequence externally. Record protocol/I/O completion,
OpenVMM release and FVP shutdown separately; the latter two remain required
for an overall pass.

Add a test next to `boot_linux_direct_cca` in
`vmm_tests/vmm_tests/tests/tests/aarch64_exclusive.rs`. Proposed name:
`boot_linux_direct_cca_tdisp_ahci`. Reuse the base CCA test's small RAM/vCPU
configuration, non-hotplug PCI topology, and virtio-vsock agent. Reuse the
ordinary VFIO test's pre-opened resource pattern, but construct the new
CCA-aware resource. Do not use its plain `VfioCdevDeviceHandle` unchanged.
[O9]

### Follow-on instrumented qualification

The expanded procedure below is for later private-DMA qualification. It is
not a prerequisite to implementing the host path and attempting the unchanged
reference guest above.

### Summary after LOCK/RUN and I/O

Once LOCK/RUN and device I/O work, add a high-level summary to this document
of all changes by component: low-level KVM, `virt_kvm`, `tdisp`, VFIO/PCI and
MSI-X, memory backing/conversion, OpenVMM setup and device tree, Petri/Flowey,
and the FVP artifacts/tests. Explain how requests and ownership flow between
them. Separate observed working behavior from the deferred kernel, shutdown,
teardown, reuse and private-DMA qualification work, and link the relevant
commits and runtime evidence. Do not write this as a completed milestone
before the guest demonstrates LOCK/RUN and I/O.

### Later measured test procedure

1. Require the new DA fixture capability and read its validated L1 BDF.
   Capture source/artifact manifest, host kernel identity and VFIO IRQ data.
2. Construct a v7-mode Realm with one dedicated non-hotplug port for AHCI
   and separate virtio-vsock transport. Use the qualified v7 guest Image and
   initrd. Keep the AHCI disk separate from every boot/control artifact.
3. Boot and connect guest pipette. Find the guest endpoint from its PCI
   topology/identity, not a fixed assumption about host and guest BDF equality.
4. Prevent early AHCI binding if the payload supports it; otherwise unbind
   the driver and confirm it is detached before lock/accept.
5. In the Realm, discover its TSM and execute the real sequence:
   write the TSM name to `<device>/tsm/lock`, then `1` to
   `<device>/tsm/accept`, then the guest BDF to `drivers_probe`.
   Check each status. Capture evidence generation and protected-map events.
6. Require a block device descended from that AHCI controller, exactly
   131072 512-byte sectors. Read all 64 MiB with bounded direct I/O and compare
   against the known data hash. For a writable test variant, use only the
   per-run scratch image, write a known pattern and read it back.
7. Require proof of private DMA, not just disk enumeration: successful RSI
   DMA enable, protected MMIO validation, and a DMA operation whose buffer
   range is private. Add test-kernel tracing/helper support if existing logs
   cannot identify the DMA range. Do not infer private DMA solely from
   "guest is a Realm". Fail on the current kernel's DMA-enable-error log.
8. Unbind the guest driver, unlock through the supported guest sysfs interface,
   and collect state/mapping results. Require protected-map release evidence;
   the unlock write and `tsm/lock` contents alone are not sufficient. Power off via pipette and require
   `wait_for_clean_teardown()`. Preserve host/RMM/VMM/guest logs on failure.

Step 7 is an explicit acceptance gate. Until instrumentation can establish
it, report an AHCI protocol/I/O smoke test, not a private-DMA qualification.
A shell `accept` return code cannot cover the kernel issue described above.
Fixing that upstream/local guest error propagation is recommended; record any
kernel patch separately from OpenVMM changes.

### Follow-on qualification: prove the actual DMA path

**Recommendation:** add a small test-only Linux instrumentation/helper patch,
then rerun kvmtool on the same FVP configuration. Do not change the pinned
kernel silently: retain the baseline, record the patch and new Image/config
hashes, and keep this qualification work separate from the OpenVMM stack.

There are three different claims:

| Claim | Required evidence |
|---|---|
| The device can transfer data after acceptance | Already shown by run 3 |
| The device actually transfers to/from Realm-private buffers | Final device DMA addresses, private RAM state, transfer completion, and no shared staging for those bytes |
| An untrusted host/unauthorized device cannot access those buffers | Additional negative access/isolation tests; neither a hash nor a clear address selector alone establishes this |

FVP can validate the modeled CCA assignment/isolation flow. It cannot establish
production device trust, real link encryption, side-channel resistance or
hardware confidentiality.

#### Why private DMA is expected, but still needs measurement

In this guest kernel, `force_dma_unencrypted()` returns false for an accepted
device. The ordinary direct DMA path then selects the encrypted/private address
form unless attributes or a bounce path require another form. On Arm CCA,
shared DMA uses the `PROT_NS_SHARED` selector; the normal encrypted form is the
canonical address. The selector must be obtained from this guest's negotiated
address configuration, not a hardcoded bit. [K9]

AHCI allocates coherent memory for command headers, received FIS data and
command tables. Payloads go through `dma_map_sg`, then `ahci_fill_sg` writes
the **mapped** addresses and lengths into PRDT entries. A userspace pointer,
`sg_phys()` of the original buffer, the absence of `DMA_ATTR_CC_SHARED`, or
`O_DIRECT` alone is not proof of what the device actually addresses. [K10]

#### Phase A1: collect the DMA API evidence

Enable guest event tracing in a derived test kernel if necessary. The current
effective config has `TRACING_SUPPORT`, but that alone does not mean tracefs
DMA events are enabled. Verify the effective tracing configuration and actual
`events/dma` availability.

Capture `dma_alloc`, `dma_map_sg`, corresponding unmap/free events, and
SWIOTLB bounce events for the AHCI BDF. Start before **post-accept driver
reprobe** so coherent command/FIS allocations are included. Add explicit
workload begin/end markers and record device acceptance and DMA direction.
Use per-event filters only after checking each event's `format`; do not assume
all events expose the same fields. Size the trace buffer for the bounded test
and fail evidence collection on overruns/dropped records. DMA SG events also
cap their arrays at `DMA_TRACE_MAX_ENTRIES` (128 in this tree), exposing
`truncated`, `full_nents` and `full_ents`. A larger ring does not remove that
per-event cap. Bound the request below the limit or supply independently
correlated complete hook records; a truncated event never satisfies full
buffer coverage. Record truncation separately from ring-buffer loss. [K11]

DMA API traces are useful first evidence, but not the final oracle. They do
not establish every buffer's RIPAS or all actual descriptor contents.
SWIOTLB can use private as well as shared pools in this tree; "a bounce
occurred" is not identical to "confidentiality failed". For the initial
controlled test require **no bounce at all**, and report private-bounce support
as separate future coverage rather than guessing from an event. [K9, K11]

#### Phase A2: correlate a real AHCI command with private buffers

Use a small, deterministic, single-request test before repeating the large
`dd` workload. Recommended helper shape:

1. After guest acceptance and AHCI reprobe, allocate owned, page-aligned guest
   kernel pages for a bounded test read. Submit them through the normal block
   layer to the confirmed AHCI scratch disk; do not emulate DMA with a CPU
   copy or replace the AHCI driver. Keep the pages alive until completion and
   verification. Serialize the test with driver reset/unbind and prohibit
   conversion/reuse of its buffers while in flight.
2. In a test hook after DMA mapping and before command issue, record the guest
   BDF, acceptance state, request ID, ATA hardware tag, direction, LBA/length,
   final mapped SG entries, and actual PRDT address/length fields. Check the
   entries submitted to hardware, not just the input list to `dma_map_sg`.
   Correlate request completion/error with the same ID, including all split,
   retried and reissued commands. Reject truncated SG/PRDT coverage rather
   than counting a partially recorded request as complete. [K10]
3. Check the coherent command-header/table and received-FIS ranges as well.
   These can contain addresses and data; proving private payload pages alone
   does not qualify the entire AHCI command path. Capture their allocation
   after acceptance and their programmed addresses before engine startup.
4. For every device-visible test range, require a clear shared selector and
   convert the DMA address back to a Realm IPA using the device's actual DMA
   translation. For this fixture require the direct-DMA/no-guest-IOMMU path.
   Do not treat an arbitrary IOVA as an IPA. Match payload ranges to the owned
   test pages; reject unexpected remapping/staging or uncovered bytes.
5. Query `rsi_ipa_state_get()` across **every covered granule**, requiring
   `RSI_SUCCESS`, bounded forward progress and exactly `RIPAS_RAM`. Do not use
   `arm64_rsi_is_protected()` as a RAM proof: it accepts non-EMPTY states,
   including device memory. Record checks before submission and after
   completion, plus buffer ownership/conversion exclusion for the interval
   between them. [K12]
6. Confirm that none of the submitted addresses resolves to a SWIOTLB buffer
   or other staging allocation, and that the helper's original pages hold the
   expected disk bytes after normal DMA synchronization/completion. Reconcile
   per-request SG/PRDT byte totals with the requested transfer size.

Review hook execution context before implementation. `ata_sg_setup` runs with
the host lock held; do not insert sleeping allocation, unbounded logging, or
an unchecked long RSI walk there. Perform preparation/state walks in a safe
context, keep fast-path records bounded, and use owned-buffer lifetime plus
conversion exclusion to bridge the checks to the actual command. If those
constraints cannot be established, the result stays inconclusive rather than
claiming atomic proof from two snapshots. [K10]

For a read, device DMA writes guest memory (`DMA_FROM_DEVICE`). Follow it with
a write/read-back variant on the **per-run disposable disk** to cover device
DMA reads (`DMA_TO_DEVICE`). Generate a fresh nonsecret test pattern inside the
Realm, use a page-aligned transfer/LBA range, flush the scratch device before
read-back, and compare data using a separate verified read request. Never run
this write test on a host or guest boot disk.

Then repeat the 64 MiB read with equivalent range coverage. Report the initial
small-request result separately until instrumentation covers the full workload.

Suggested machine-readable result fields:

```text
run_id, device_bdf, request_id, ata_tag, direction, lba, transfer_bytes
accepted, dma_enable_result, shared_selector
command_fis_ranges[], mapped_sg[], submitted_prdt[]
each_range: dma_address, realm_ipa, length, ripas_before, ripas_after
bounce_count, conversion_count, completed_bytes, completion_status
expected_hash, actual_hash, trace_dropped_records, sg_event_truncated
```

Pass only if the test records cover the actual submitted buffers, all required
checks succeed, completion/data match, and no evidence is missing. Never
convert a trace failure into a skipped-success result.

#### Phase A3: prove the oracle rejects a non-confidential path

Run controls in **fresh** FVP/Realm instances because same-host cleanup is not
qualified.

| Control | Required result |
|---|---|
| Ordinary unaccepted AHCI, where supported | Shared/bounce DMA may work, but the confidential-DMA checker must report **not confidential** |
| Synthetic helper record with a shared selector, non-RAM state, missing segment, truncated SG array or dropped record | Checker fails deterministically; this tests the checker, not hardware isolation |
| Test-only guest mode that withholds RSI DMA enable while retaining private test pages | No successful private transfer; require a mapped-request trace and a bounded completion failure with unchanged destination sentinel where applicable |

For the DMA-disabled control, implement an explicit test-only guest mode.
Do not merely omit `tsm/accept`: an unaccepted driver may fall back to shared
buffers and legitimately work. Establish the control's bootstrap path first:
withholding DMA enable can prevent AHCI discovery because even IDENTIFY uses
DMA with this driver. There may be no block device for the normal helper to
submit against. Select an actual observable probe command if necessary and
capture its private buffers, submission and attributable failure; do not
promise a post-discovery block request. Missing disk discovery alone is
inconclusive. [K13]

Never revoke DMA during an active request or
declare a timeout alone proof of enforcement. Correlate the attempted private
command with device/RMM/SMMU status; if attribution is unavailable, mark the
negative isolation result inconclusive. Keep instrumentation that reports an
invalid state from being mistaken for a production acceptance path.

An optional later host-access control can attempt a bounded read of the exact
private guest_memfd offset in an isolated helper process and verify expected
denial, after checking this kernel's fault contract. This is supplementary:
host denial by itself does not prove the device used that range. Do not expose
real secrets or convert the tested pages to shared to inspect their contents.

After A2 and meaningful controls pass, the justified claim is: **this pinned
model stack completed AHCI DMA using checked Realm-private command/data
buffers for the tested requests**. Clean teardown, cross-VM/device isolation
and physical hardware assurance remain separately qualified results.

### Invocation and selection

After full host integration is ready and the new roots are provisioned, use
the exact reference guest and single-boot test:

```bash
PYTHONDONTWRITEBYTECODE=1 cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca-realm-vfio.toml \
  --fvp-platform-root "$PWD/target" \
  --shrinkwrap-package-root "$PWD/target/cca-tdisp-stage-a/realm-vfio-reference-platform/package" \
  --cca-in-place-payload-root "$PWD/target/cca-tdisp-stage-a/test-platform/payload" \
  --cca-tdisp-guest-root "$PWD/target/cca-tdisp-stage-a/runs/run-20260914-3/share" \
  --dir "$PWD/vmm_test_results/cca-tdisp-exact-guest" \
  --fvp-single-test aarch64_exclusive::tdisp_ahci::openvmm_linux_aarch64_boot_linux_direct_cca_tdisp_ahci
```

Artifact staging and the test are implemented, but full host integration and
runtime qualification are still pending. `--build-only` must not launch FVP.

Require exactly one executed passing test. Zero-test, ignored-only and
listing-only results are not success. Keep this test out of QEMU's current
`aarch64_tcg` name-based CI selection.

The root execution-target plan is explicitly deferred. This work does not
resume it or assume `targets(fvp_cca)` exists. Use the exact manual filter,
a DA capability, and runtime fixture checks. Preserve existing in-incubator
discovery behavior; the explicit single-boot path avoids the separate
enumeration shutdown before the selected test. It does not suppress model
shutdown failures or create a reusable persistent FVP session.

### Additional validation

| Layer | Required cases |
|---|---|
| ABI/unit | Struct sizes/offsets; full SMCCC function IDs; RHI register ABI; explicit state/object translation; nonce placement; residue and short-response handling |
| Request handling | Unknown function/device/object/state; oversized or wrapping buffer; selector-clear shared buffer succeeds and private backing fails; selector-set buffer rejection; stale copyback; cancellation |
| State/access | Invalid transition does not open BAR access; failure after RMI LOCK/START; uncertain backend completion; BAR/config/reset race; mixed MSI-X page rejection |
| Memory | INIT_RIPAS boundaries/holes and ledger side effects; attributes2 failure location; prefault partial/no progress; successful memory-fault re-exit and ordinary `-EFAULT`; backing/resource/poison failure classification; stop during completion; both DMA conversion directions |
| TIO | Guest supplies private protected-resource IPAs; shared ECAM/virtio still work; unknown subreason; wrong requester ID; rejected range; nonzero completion |
| Evidence | Whole-object backend reads at offset zero; guest nonzero offset/short chunk/end/beyond-end cases; regeneration between size/read; corrupted bytes and stale sequence rejected; no RUN after failed validation |
| Lifecycle | Partial object allocation rollback; verified guest unlock; partial protected mapping and RUN failure; interrupted lock/accept; same-L1 repeat assignment; forced teardown with live mappings |
| Regressions | Existing CCA v15 FVP/QEMU boot, ordinary cdev passthrough/nested SMMU, and existing TDISP/VPCI/synthetic tests |

Negative guest-protocol tests may need a small test-kernel helper; sysfs alone
does not expose arbitrary malformed RHI or TIO requests. Unit fakes cover
transport/error plumbing, but do not replace a real FVP negative result.

## 11. Staged implementation

**Active sequence:** implement and review the shared-host refactor in section 4.
The rebase, native LOCK/RUN and initial I/O integration are complete.
Do not repeat the original stages below as new refactor prerequisites.
Buffer-level DMA and clean-lifecycle qualification remain separate follow-ons.

### Original bring-up stages and remaining qualification

| Stage | Main files/crates | Exit criterion |
|---|---|---|
| A. Reference and interrupt qualification | Pinned Linux/kvmtool/TF-RMM build manifest; DA model assets; qualification helper | Recorded reference/IRQ baseline plus section 10 buffer-level qualification and clean lifecycle; current read smoke success alone does not close A |
| B. In-place guest_memfd ABI and backing | `vm/kvm`; `vmm_core/virt_kvm/{cca,memory}`; `openvmm/membacking`; worker assembly | In-place Realm boots on FVP; INIT_RIPAS ledgers, both memory-fault return forms/cause classification, pending completion and both conversion directions pass; v15 unchanged; QEMU debug deferred |
| C. DA FVP/payload mode | `petri/incubator/{profile,fvp,cca_init}`; platform/profile files; `resolve_cca_payload`; Flowey runner/pipeline; Petri artifacts | Validated DA L1 and guest artifacts; readiness-gated fixture; negative identity tests |
| D. Host object path | `vfio_sys/{cdev,iommufd}`; VFIO resources/resolver/manager; KVM association service | Realm vIOMMU/vdevice/S1-bypass attach and partial-allocation cleanup; no live TDISP state requests yet |
| E. Native TDISP/RHI | `tdisp` host module; VFIO CCA coordinator; Arm KVM exit/register adapter | Whole-object snapshot adapter, guest buffer encoding, mocked requests and evidence transport pass; real LOCK/RUN remain disabled |
| F. Access and DMA completion | VFIO BAR/config/IRQ paths; TIO handler; memory/DMA coordinator; DT/address integration | Real transitions enabled only now; protected MMIO, private/shared DMA and observed interrupt mode work |
| G. End-to-end and lifecycle | Petri fixture; new test and helper/instrumentation; fault tests; Guide | Exact FVP test and required evidence pass with clean teardown; record any failures separately, never as overall success |

Stage A has a protocol/read baseline and known MSI-X mode, but remains
incomplete for confidential DMA and clean lifecycle. The original integration
and unchanged reference-guest trial have now reached LOCK/RUN and verified I/O.
Section 10's buffer-level instrumentation is a later qualification task, not
a gate on the shared-core refactor. Shutdown investigations remain deferred; do not use
repeated forced shutdown as a clean-reuse result.

Stage C scaffolding and B can proceed using the recorded local candidate;
publishing a qualified DA platform/test still requires the open Stage A gates.
D depends on B;
E can develop against fakes and typed ioctls alongside D. F requires B, D and
E. G's completion requires qualified C/F outputs, but developing and attempting
the end-to-end diagnostic can use validated candidate artifacts before clean
reference-platform qualification. This does not relax F's implementation and
access-control prerequisites for real LOCK/RUN. Interrupt work discovered in A
is a prerequisite for F, not deferred cleanup.

Use focused crate checks, clippy, rustdoc and unit tests while implementing.
Use `cargo nextest run --profile agent -p <package>` for unit tests and
`cargo xflowey vmm-tests-run` for VMM tests. Follow the repository's mandatory
pre-commit checks. Regenerate Flowey YAML rather than editing generated CI.

Update the existing Guide pages for CCA VMM tests, VFIO assignment, CLI
configuration and memory backing. Explain that Realm host vIOMMU does not
mean guest-visible vSMMU. If adding a TDISP reference page or new crate,
load the Guide maintenance skill and update the page index/code-sync mapping.
Do not copy the ordinary VFIO guide's unsafe-interrupt workaround into the
trusted-assignment prerequisites.

## 12. Scope and open gates

Milestone one excludes hotplug, migration/snapshots, multiple assigned
devices, P2P, ATS/PASID, secure MSI-X, a guest-visible SMMU, production trust
policy, and hardware qualification.

Resolve these before claiming end-to-end support:

- Clean lifecycle qualification of the recorded DA tuple, including separate
  IOMMUFD `EBUSY` and FVP heap-abort investigations.
- OpenVMM MSI-X delivery and BAR-range handling for the now-observed AHCI mode.
- v7 backing integration, memory-fault cause/completion handling and
  guest-buffer access tracking.
- Private/shared PCI address translation with the actual Linux TSM resource
  allocator.
- Protected-map cleanup on forced exit; same-host reuse cannot be assumed.
- Reliable observation/error propagation for guest RSI DMA enable.
- Actual command/data-buffer private-state and no-shared-bounce evidence for
  completed device transfers, as specified below.

These are bounded engineering gates, not reasons to implement a second VMM
or replace the existing TDISP infrastructure.

## 13. Source index

S1-S9 refer to the rebased code inspected on 2026-09-18, at change ID
`tkorwyzw`, for the current section 4 refactor.

| ID | Current shared-core evidence |
|---|---|
| S1 | `vm/devices/tdisp/src/lib.rs:80-120,187-368,420-572,653-944`: callbacks, dispatch, state and protocol policy |
| S2 | `vm/devices/tdisp/src/host.rs:234-278,431-665`: native backend contract, state, transactions and snapshots |
| S3 | `vm/devices/tdisp/src/tests/statemachine_tests.rs:51-160,200-252`: idempotent Unbind and validation behavior |
| S4 | `vm/devices/tdisp_proto/src/tdisp.proto:12-74`: indeterminate state and existing error codes |
| S5 | `vm/devices/pci/vpci_client/src/tdisp.rs:256-319,609-714`: cache/error ordering and cleanup policy |
| S6 | `vm/devices/storage/nvme_test/src/tdisp.rs:37-194`: callback and shared gate implementation |
| S7 | `vm/devices/storage/nvme_test/src/pci.rs:548-610`: BAR0 gate and separate BAR4 MSI-X access paths |
| S8 | `vm/devices/tdisp/src/host/evidence.rs:59-236`: native admission, worker retention and teardown |
| S9 | `vmm_core/virt_kvm/src/memory.rs:181-199,428-505`; `vm/devices/pci/vfio_assigned_device/src/realm/tdisp.rs:226-270,528-694`: RAM transaction and backend containment |

Line ranges refer to the inspected sources, before implementation. Kvmtool's
working tree is now on the pinned v7 branch and was read independently during
review pass two. The revision links below remain the reproducible references.

| ID | Evidence |
|---|---|
| O1 | `vm/devices/tdisp/src/lib.rs:73-126,307-468,578-712`; `vm/devices/tdisp_proto/src/tdisp.proto:12-145`; `vm/devices/tdisp/src/devicereport.rs:12-153` |
| O2 | `vm/devices/pci/vpci/src/device.rs:924-956`; `vm/chipset_device/src/lib.rs:68`; `vm/devices/storage/nvme_test/src/pci.rs:513-523`; `openhcl/openhcl_tdisp/src/lib.rs:40-94` |
| O3 | `vm/kvm/src/lib.rs:943-1018,1990-2000,2092-2100`; `vmm_core/virt_kvm/src/cca.rs:34-125`; `vmm_core/virt_kvm/src/memory.rs:81-173,251-301,418-535`; `vmm_core/virt_kvm/src/arch/aarch64/mod.rs:525-622`; `vmm_core/virt/src/io.rs:10-50` |
| O4 | `vm/devices/pci/vfio_assigned_device/src/resolver.rs:164-288`; `src/manager.rs:553-653,750-925`; `src/iommufd_nesting.rs:154-199`; `src/lib.rs:694-837,843-873,1406-1453,1673-1768` (the latter paths share the same crate directory); `vm/devices/user_driver/vfio_sys/src/{cdev,iommufd}.rs` |
| O5 | `openvmm/openvmm_core/src/worker/dispatch.rs:989-995,1338-1416,2603-2635`; `openvmm/openvmm_core/src/worker/vm_loaders/linux.rs:515-595`; `vm/devices/pci/vfio_assigned_device_resources/src/lib.rs:45-71` |
| O6 | `openvmm/membacking/src/region_manager.rs:38-103,408-467`; `vm/devices/pci/vfio_assigned_device/src/manager.rs:565-652,759-787` |
| O7 | `petri/incubator/platforms/fvp-cca-v15.yaml`; `petri/incubator/src/fvp/platform.rs:40-181`; `petri/incubator/src/profile.rs:355-437`; `petri/incubator/src/fvp/staging.rs:238-279`; `petri/incubator/src/fvp/runtime.rs:59-94,170-230,398-401` |
| O8 | `flowey/flowey_lib_hvlite/src/resolve_cca_payload.rs:14-30,126-201`; `flowey/flowey_lib_hvlite/src/write_incubator_target_runner.rs:38-51,121-133`; `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs:302-370`; `petri/src/vm/openvmm/construct.rs:817-820,918-948` |
| O9 | `vmm_tests/vmm_tests/tests/tests/aarch64_exclusive.rs:20-55,121-210`; `vmm_tests/vmm_tests/tests/tests/x86_64.rs:283-311`; `plan-incubator-test-targets.md:3-24`; `note-fvp-manual-test-selection.md` |
| O10 | `vm/kvm/src/lib.rs:1834-1875,2030-2045`; `vmm_core/virt_kvm/src/arch/aarch64/mod.rs:545-565`, existing error-only memory-fault path |
| K1 | `../linux-cca/include/uapi/linux/kvm.h:1668-1730`; `virt/kvm/guest_memfd.c:810-864`; `arch/arm64/kvm/rmi.c:1142-1198,1257-1398,1799-1838`; `arch/arm64/kvm/mmu.c:2964-3065` (remaining paths relative to Linux) |
| K2 | `../linux-cca/include/linux/arm-smccc-rhi.h:11-89`; `arch/arm64/kvm/hypercalls.c:143-217,258-295`; `drivers/virt/coco/arm-cca-guest/rsi-da.h:14-47`; `drivers/virt/coco/arm-cca-guest/rhi-da.c:42-166` |
| K3 | `../linux-cca/include/uapi/linux/iommufd.h:1358-1427`; `arch/arm64/include/uapi/asm/rmi-da.h`; `drivers/iommu/iommufd/tsm.c:43-98`; `drivers/iommu/iommufd/viommu.c:83-85,120-149`; `drivers/iommu/arm/arm-smmu-v3/arm-smmu-v3-realm.c:201-274`; `drivers/virt/coco/arm-cca-host/arm-cca.c:427-606` |
| K4 | `../linux-cca/arch/arm64/kvm/rmi-exit.c:88-162`; `arch/arm64/kvm/rmi.c:1618-1683`; `drivers/virt/coco/arm-cca-host/rmi-da.c:1184-1256` |
| K5 | `../linux-cca/drivers/virt/coco/arm-cca-guest/arm-cca.c:285-478,502-524`; `drivers/virt/coco/arm-cca-guest/rsi-da.c:85-234,241-305`; `drivers/pci/tsm/core.c:606-646,694-799,823-885,977-1003` |
| K6 | `../linux-cca/arch/arm64/kvm/rmi-exit.c:88-116`; `arch/arm64/kvm/rmi.c:1312-1370,1651-1683`; `arch/arm64/kvm/arm.c:1344-1359`; `arch/arm64/kvm/mmu.c:1723-1730`; `virt/kvm/guest_memfd.c:1179-1254` |
| K7 | `../linux-cca/drivers/virt/coco/arm-cca-host/rmi-da.c:1296-1351`; `drivers/virt/coco/arm-cca-host/arm-cca.c:552-578`; `drivers/virt/coco/arm-cca-guest/rhi-da.c:290-370`, actual whole-object read behavior |
| K8 | `../linux-cca/drivers/virt/coco/arm-cca-guest/rsi-da.c:39-59`; `drivers/virt/coco/arm-cca-guest/rhi-da.c:249-272,327-343`; `arch/arm64/include/asm/memory.h:340-371`; `drivers/firmware/arm_rmm/rsi.c:88-139,239-243` |
| K9 | `../linux-cca/arch/arm64/mm/mem_encrypt.c:77-83`; `arch/arm64/include/asm/mem_encrypt.h:26-35`; `include/linux/dma-direct.h:92-123,149-156`; `kernel/dma/direct.c:688-742`; `kernel/dma/swiotlb.c:1767-1776` |
| K10 | `../linux-cca/drivers/ata/libahci.c:749-756,1652-1672,1684-1723,2044-2073,2525-2553`; `drivers/ata/libata-core.c:4870-4915` |
| K11 | `../linux-cca/include/trace/events/dma.h`, DMA allocation/mapping event formats; `kernel/dma/mapping.c:240-269`; `kernel/dma/swiotlb.c:1767-1776` |
| K12 | `../linux-cca/include/linux/arm-rsi-cmds.h:39-43,71-85`; `drivers/firmware/arm_rmm/rsi.c:106-130`, non-EMPTY versus exact RAM checks |
| K13 | `../linux-cca/drivers/ata/libata-core.c:1763-1767,5228-5229`; `drivers/ata/ahci.h:253`, IDENTIFY and AHCI PIO-protocol DMA |
| V1 | [kvmtool v7 `kvm.c:431-474`](https://gitlab.arm.com/linux-arm/kvmtool-cca/-/blob/2e0928d1f945d68af388575e7bd4d6bfa7200120/kvm.c); `util/util.c:175-225`; `arm64/realm.c:84-141`; `arm64/kvm-cpu.c:547-596` at the same revision |
| V2 | [kvmtool v7 `vfio/iommufd.c`](https://gitlab.arm.com/linux-arm/kvmtool-cca/-/blob/2e0928d1f945d68af388575e7bd4d6bfa7200120/vfio/iommufd.c), allocation/BAR/IOAS helpers; `arm64/kvm.c:530-554,628-646` |
| V3 | [kvmtool v7 `arm64/smccc.c`](https://gitlab.arm.com/linux-arm/kvmtool-cca/-/blob/2e0928d1f945d68af388575e7bd4d6bfa7200120/arm64/smccc.c); `arm64/include/asm/smccc.h:20-64`; `arm64/tsm.c`; `arm64/kvm-cpu.c:600-630` |
| V4 | `../kvmtool-cca/include/linux/iommufd.h`, `IOMMU_VDEVICE_TSM_GUEST_REQUEST` encoding and outer request layout; compare K3 |
| F1 | `../tf-rmm/tools/shrinkwrap/configs/cca_da.yaml:24-65`; `tools/shrinkwrap/configs/model-enable-da.yaml:12-73`; `tools/shrinkwrap/pci.json`; `docs/getting_started/building-with-shrinkwrap.rst:122-218` |
| F2 | `../tf-rmm/runtime/rsi/vdev.c:342-348`, protected-IPA validation, independently checked during plan review |

## Review

### Shared host infrastructure refactor: 2026-09-18

Review verdict: **Minor revisions**, incorporated.

The review checked the current S1-S9 implementations and accepted the shared
engine, thin facades, operation/wire contracts and staged validation.
It identified two details that needed explicit decisions:

- Emulator revocation must not rely on clearing the BAR0 range set or taking
  the callback mutex in Drop. Section 4 now specifies a sticky local atomic
  denial latch/permit, coverage of BAR0 and BAR4, error and panic tests, and
  an atomic landing of the facade and all gate consumers.
- Reset has no existing capability query. Section 4 now preserves the Realm
  backend's invoked-reset failure behavior and requires tests for callback
  count, snapshot/budget release, quarantine and transition history. It does
  not relabel a backend error as a preflight Unsupported rejection.

The main plan is the implementation authority; the findings document is
background only. No implementation, build or runtime validation was performed
for this planning update. Earlier reviews below apply to the original
bring-up and its later milestone updates.

### Pass one

The `review-plan` agent reviewed the plan and pivotal OpenVMM/Linux/TF-RMM
sources on 2026-09-11. Verdict: **Minor revisions**, with the qualification
that it could not independently inspect kvmtool through `git show` in its tool
environment. The primary investigation did inspect the pinned kvmtool
revision, including its guest_memfd conversion, S1-bypass setup and Arm
register-completion helpers.

The review confirmed the main architecture and implementation gates. Its
four requested changes are incorporated:

1. Correct the RHI filter encodings to SMCCC owner 5 and require full-value
   ABI assertions.
2. Make the private PCI resource address view a required change. RMM rejects
   the current shared-alias addresses before a VMM TIO fixup could help.
3. Treat errors after RMI LOCK/START as potentially committed operations,
   not just copyback failures.
4. Require verified unlock/partial-accept cleanup, including the guest
   kernel's false-success and unpublished-MMIO-state failure paths.

The review does not qualify the candidate stack at runtime. Firmware/model
selection, AHCI interrupts, memory integration, private-DMA evidence and
forced teardown remain explicit implementation gates.

### Pass two: independent kvmtool working-tree review

After the user switched kvmtool to the v7 branch, the `review-plan` agent
read its implementation directly with the Linux, TF-RMM and OpenVMM sources.
This closes pass one's independent-kvmtool inspection gap. Branch-reference
reads matched the recorded sibling pins; working-tree cleanliness was not
established.

Initial verdict: **Needs rework**, requiring targeted corrections rather than
an architecture rewrite. The following changes are incorporated:

1. Fetch whole backend objects at offset zero and serve guest chunks from
   bounded, generation-scoped snapshots. The pinned kernel does not implement
   safe bounded offset reads as the earlier plan assumed.
2. Handle both successful and error-return memory-fault exits, preserve errno,
   distinguish backing failures from conversion, and track pending completion
   through re-entry and cancellation.
3. Accept the actual selector-clear RHI buffer encoding based on shared
   backing state; do not mistake a clear selector for private backing.
4. Include INIT_RIPAS's private-attribute side effect in host-access/DMA
   ledgers and keep initial shared IOAS mappings out of population/init.

The pass also confirmed Realm object order, Arm register completion, and the
need not to copy reference error handling. The guest ioremap path supports a
private resource address view in principle; a second PCI root is not proven
necessary. Runtime and forced-teardown qualifications remain outstanding.

The targeted revision review returned **Ready**. It confirmed that all four
corrections are reflected in the design, validation cases and implementation
gates, with no further amendments required. That check used the established
second-pass source evidence and revised document sections; it was not another
full source audit or a runtime qualification.

### Runtime evidence and DMA qualification update: 2026-09-14

The `verify` agent independently checked run 3's matching-image read, MSI-X,
RSI DMA-enable success, successful RMM device teardown, remaining IOMMUFD
errors and FVP abort. It did not infer private-buffer DMA from those results
and did not attribute the model abort to the cleanup errors.

The plan now separates completed foundation changes and observed local results
from the proposed buffer-level qualification. The new test design requires
actual AHCI descriptor/SG correlation, exact RIPAS_RAM checks and no staging
for owned test buffers, followed by explicit controls.

Targeted `review-plan` review: **Minor revisions**. The review cross-checked
run 3 artifacts and K9-K12 and confirmed the scope of the claimed result.
Its three corrections are incorporated: establish an observable DMA-disabled
probe command before promising a block-request control; reject truncated SG
trace records separately from dropped events; and require buffer-level
qualification/clean lifecycle consistently in the stage table. The review
does not establish confidential DMA or resolve the shutdown failures.

Final targeted review before commit: **Ready**. All three corrections were
confirmed, and the DMA/SWIOTLB tracepoints needed to start A1 were checked in
the pinned Linux sources. A1 remains trace evidence collection, not a substitute
for A2's descriptor, RAM-state and lifetime checks.

### Guest_memfd in-place parity update: 2026-09-15

Code review found no significant issues in the naming, revocable-memory,
vsock and FVP runner changes after the earlier scoped corrections.
The effective package was then corrected to omit the PCI hierarchy filename;
the model's parameter listing identifies `<default>`, not an empty string,
as its default. The corrected FVP VMM run passed.

Targeted `review-plan` review: **Minor revisions**, incorporated above.
The review checked the shared test body and bounded private-read record,
using the recorded test outcomes as supplied execution evidence. It required
the exact backend-hook name, an explicit distinction between shared test
configuration and different platform tuples, and separate wording for
executed VMM tests versus the skipped crate test. It did not qualify
additional Virtio guest devices, same-tuple parity, or TDISP assignment.

### Upstream publication note review: 2026-09-15

Targeted `review-plan` review: **Ready**. The review accepted the checked
release/manifest observations and confirmed the distinction between missing
FVP package publication, local Linux payload publication, and conditional
QEMU firmware replacement. It did not qualify the QEMU runtime combination.
Code reviews of the device tests and QEMU input plumbing completed; the
duplicate builder modification and IPv4 listener setup findings were fixed.

The targeted FVP-priority revision review also returned **Ready**. It
confirmed that broken QEMU qualification is explicitly deferred while the
FVP assignment, private-DMA, interrupt and clean-lifecycle gates remain
mandatory.

### Owned Realm object design and implementation review

Design review: **Minor revisions**, incorporated. The review required final
file-release ordering, exclusive control of the VFIO open file description,
one-time attachment and cleanup-only retry, structurally retained recovery
ownership, and drop-counter/wiring regression tests. Separate code reviews
of the provider wiring and allocation owner found no significant issues.
The FVP boot regression checks provider lifetime integration only, not
hardware object creation or trusted DMA.

### Direct OpenVMM execution priority: 2026-09-16

Targeted plan review: **Minor revisions**, incorporated. Removed the remaining
mandatory standalone-preflight wording and distinguished permission to develop
and attempt end-to-end diagnostics from completed support qualification.
Stage E proceeds with fakes and typed native TSM bindings. Actual lifecycle,
interrupt and private-DMA evidence remain required; no live assignment or
state transition is enabled by the new bindings alone.

Independent code reviews of the native host core and typed CCA ioctl bindings
found no significant issues. Scoped unit coverage includes legacy TDISP
behavior, snapshot bounds/budget release, quarantine, ABI layouts, residue
handling and errno propagation. The ioctl layout was also checked against
the pinned C headers. These are implementation checks, not runtime TDISP or
private-DMA qualification.

### Owned Linux evidence adapter review: 2026-09-16

The initial review found that a successful vdevice allocation can bypass CCA
TDI creation when no TSM is configured. The adapter now requires a complete,
positive certificate-size response through the CCA request path before
reporting the fresh binding's Unlocked state. Failed verification returns
the original owner without cleanup or mutation. Constructor-boundary tests
cover missing TSM/TDI, invalid phases, and incomplete or absent size replies.
The scoped corrective review found no significant issues.

### Arm KVM native transport interface review: 2026-09-16

Review verified the pinned ABI interfaces and found an incomplete exhaustive
exit match in the Arm `virt_kvm` consumer. This is fixed in the same change:
unexpected native hypercalls stop the VM, and trusted-I/O exits remain rejected
before stopping. The real Arm backend now compiles with the new variants.
Packet/filter/register unit coverage does not establish live filter forwarding,
register round-trips, or RMM completion behavior on FVP; those remain runtime
qualification work for the guest request integration.

### Native evidence routing design review: 2026-09-16

Design review: **Minor revisions**, incorporated. Added bounded admission
before blocking-pool submission, explicit cancellation and teardown draining,
a partition-wide freeze before any VP await/entry, and fatal handling for
partial register writes or abandoned requests. Guest copy failures explicitly
permit a written prefix, but never a success result. The runtime diagnostic
must call the partial evidence interface directly rather than infer routing
coverage from an ordinary Linux boot.

The routing implementation review found that the kernel's exit number loses
the original x0 high bits, and that marking a fatal flag alone does not stop
peer VPs already in KVM. The adapter now reads and checks original x0, and
its request guard invokes partition-wide poisoning with VP interruption.
The scoped corrective review found no significant issues. Live host-call
completion and multi-VP cancellation/copy behavior remain unqualified.

The asynchronous service review found that dropping a caller could cancel an
admitted blocking task before the pool started it. Admitted tasks now detach
before the caller awaits a separate completion channel. The worker retains
its admission permit, coordinator, and sink. Deterministic tests cover queued
size, read, and teardown cancellation; the corrective review found no
significant issues.

The ordinary in-place CCA boot regression passed on FVP in 261.233 seconds,
nextest run `010f1be1-4c50-4733-9692-da96971659c5`. Results are retained under
`vmm_test_results/rhi-default-boot`. This run used no evidence registration:
it confirms default boot behavior, not native RHI dispatch or TDISP operation.

### Unchanged-guest trial review

Design review: **Minor revisions**, incorporated. The original initrd's
autonomous sequence is authoritative for the first trial; pipette-driven and
instrumented procedures are follow-on qualification. The guest pair and
patterned reference disk are pinned separately from the L1 payload. Retention
must cover all failure and cancellation paths through the model recovery
boundary, and protocol/I/O evidence remains separate from the overall verdict.

Test review corrected the initial marker to `DA_STAGE_A_GUEST_READY` because
`BOOT_PASS` belongs only to the separate boot-only phase. It also corrected
the disk oracle to the original run-3 hash rather than the zero-filled firmware
build artifact. A new package copies the saved patterned disk without changing
the original packages or rebuilding firmware. The exact-guest Flowey staging
review found no significant issues. Review alone does not establish a live
LOCK/RUN or I/O result.

The first live attempt executed the selected test, but failed in Petri
configuration before guest boot: `wait for RTS not supported with this serial
type`. Native nextest run `d28cc26d-2b44-46bd-90b2-8e2380b1415e` failed after
307.384 seconds in FVP session
`8ea70a314a443c46c36c7f3aa91ffe6176c45c993cee343932b1950a017278a2`.
The wrapper separately recorded native command exit 100, successful fixture
teardown, and failed launcher shutdown. This is not a TDISP execution result.
The fix skips the unused serial-agent handshake and empty agent disk for
agent-free Linux direct boot, leaving the original guest unchanged.

The second attempt passed that setup stage and reached OpenVMM, but the test
requested an invalid zero-sized high-MMIO allocation for the Realm root.
Session `33bb16dc05b0b106533d9482809466c151e998a5ccb0e9d8613fcd588cf0d146`
failed before guest execution. The test now retains the normal high-MMIO
window instead of overriding its size to zero.

The third attempt booted the unchanged guest kernel under OpenVMM. It stopped
during early shared-memory setup after the 64 MiB SWIOTLB allocation, before
the initrd's READY/LOCK sequence. Session
`220a5b2cbaf32d1391bcb573b98a72532045c92c61e550dd73437685868696da`
reported a shared-DMA mapping failure. The implementation used the incompatible
file-pin ioctl described above; it now retains a mmap view and uses the same
virtual-address mapping ioctl as kvmtool. The scoped correction review found
no significant issues. At that point, LOCK/RUN and disk I/O had not yet been
demonstrated.

The fourth attempt passed early shared-memory setup and enumerated AHCI at
`0000:01:00.0` with the expected six BARs. It reached the separate Virtio
root's probe before the execution deadline, without reporting the prior DMA
mapping error. Session
`a1d137edf2c103493f298dc5cf7ce2aacbe7706de5196fa9bc58068630dab069`
has no completed native JUnit result. The next trial adds `quiet` to the
platform command line to reduce console overhead while retaining kernel
errors and uses a longer execution window. Kernel, initrd and disk bytes
remain unchanged.

### Demonstrated LOCK/RUN and I/O: 2026-09-16

The fifth trial completed the execution milestone with the unchanged run-3
guest and patterned disk. FVP session
`eb6b287c635f6e51633424680a74b0d1c68f6a850860ec34f24f57e976daf895`
ran nextest invocation `c44737e2-2933-4a58-94d3-15e7193217c3`.

The host log recorded `Locked` at 103.884 seconds and `Run` at 106.232 seconds
relative to OpenVMM startup. The guest checked its kernel TSM attributes,
reprobed AHCI, read 64 MiB using direct I/O and matched
`281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6`.
Guest interrupt output recorded 132 AHCI MSI-X interrupts.

UNLOCK then reached OpenVMM's protected-mapping guard and halted the VP.
The native test failed after 530.835 seconds; native command exit was 100,
fixture teardown exited 1, and the launcher also exited 1. Result preservation
succeeded. Do not convert this failed overall verdict to a passing lifecycle
test. kvmtool does issue native UNLOCK; our conservative guard is a separate
implementation restriction, not proof that the kernel cannot unlock.

Persistent evidence is under
`vmm_test_results/cca-tdisp-exact-guest/fvp-single-boot-runs/`
`single-boot-2630902-1789600632166144336/`. The `observed-io/` directory holds
guest, OpenVMM and Petri logs; `outputs/session-result.json` and the finalized
native JUnit keep the failure channels separate. The requested component
summary is now in [summary-arm-cca-tdisp.md](summary-arm-cca-tdisp.md).

The earlier single-boot design review returned **Minor revisions**. It required
immediate exit recording before output draining, caller-known result locations,
exact non-skipped test evidence, private input snapshots, and publication before
overall failure reporting. A later code review found shared setup could delete
an active invocation's files; the implementation now keeps its inputs, outputs,
host temporary storage and publication directory outside shared cleanup trees.
Live native execution and result preservation are now demonstrated. Clean
fixture/model shutdown and lifecycle qualification remain outstanding.

### Milestone summary review

Review verdict: **Minor revisions**, incorporated. The review checked the
guest and host milestones, the failed native JUnit verdict, separate session
failure channels, and the distinction between implemented memory coordination
and measured private-buffer DMA. Earlier checkpoint wording was corrected so
it does not contradict the current result. Proposed commit boundaries still
require compatibility edits and independent build validation.

### Final commit-sequence review

Review verdict: **Minor revisions**, incorporated. Preserve the full source
checkpoint and ignored evidence separately; use hunk-level compatibility
edits rather than overwriting earlier fixes with later files. Keep the RAM
owner wrapper distinct from the unapproved shutdown hold, and retain that
hold as a separate change. Inspect the post-format candidate and validate
actual Arm consumers and test targets before committing.

After `cargo clean`, the preserved guest Image, initrd and patterned disk
under the successful run directory still match all three recorded hashes.
The removed `target/` inputs and FVP tool staging must be restored before
any new live run.

The exact guest script and disk oracle have also been recovered from that
initrd into `vmm_tests/vmm_tests/test_data/cca_tdisp/`, with their hashes in
`provenance.json`. `generate-reference-disk.py` reproduces the original
64 MiB pattern and verifies its hash. No source script or guest command should
exist only in `target/`; generated binaries remain separate from tracked source.

## Durable FVP firmware recovery (2026-09-17)

The deleted BL1/FIP could not be recovered from the surviving release or global
caches. Those caches contain different firmware. With explicit approval,
TF-A, RMM and EDK2 were rebuilt from the pinned source revisions, using the
pinned container, GCC 15.2.1 and CMake 3.31.6. The exact DTB was reused.
Neither the host nor guest Linux payload was rebuilt.

The durable publication is
`.packages/cca-tdisp-fvp/artifacts/rebuild-20260917-verified/`.
It contains the firmware, `manifest.json` and `SHA256SUMS`. Sources, build
logs and recipe inputs also remain under `.packages/cca-tdisp-fvp/`.
These are ignored local artifacts, not large binaries added to version control.
They survive `cargo clean`; keep a separate backup if the checkout is removed.

| Artifact | Rebuilt SHA-256 |
|---|---|
| `bl1.bin` | `735c5a84430fb748db544c2d9c243a54959a160b2223c1588005c0a84ca1c0aa` |
| `fip.bin` | `8877fc00cf6d69d35a16550417170b391d0b84e666a0ae116f66fb8eeeb8ccd9` |
| `rmm.img` | `b88b2c90470e1beaf1dfec31f6258d56d7aeadfc9bb5f59fa6b51f9f8be4a61f` |
| `FVP_AARCH64_EFI.fd` | `12ea09b6e2254b011a25e3bcbecd4e3221d42ca5e231502357e416a9ce6fa178` |

The current Realm-VFIO profile pins the rebuilt BL1/FIP. They are not
byte-identical to the lost original firmware. Historical results above still
refer to the original firmware and must not be silently reattributed.

### Repeatable recovery

From the repository root, restore the unchanged payloads and DA assets from
the preserved successful-I/O single-boot run. The helper checks every copied
input and reconstructs the host configuration from its verified IKCONFIG:

```bash
python3 petri/incubator/platforms/restore-realm-test-inputs.py \
  path/to/saved-single-boot-run .packages/cca-tdisp-runtime
```

It refuses existing destination payloads. It does not supply firmware or
Shrinkwrap. When a firmware rebuild is needed, use a fresh run name:

```bash
python3 petri/incubator/platforms/rebuild-realm-firmware.py \
  --source-package .packages/cca-tdisp-runtime/package/cca-3world.yaml \
  --dtb path/to/verified/dt_bootargs.dtb \
  --source-cache "$HOME/.shrinkwrap/build/source/cca-3world" \
  --name NEW_BUILD_NAME --jobs 8
```

The recipe requires the pinned Docker image to be present. `--source-cache`
is optional. It clones independent pinned sources, rebuilds EDK2 as well as
RMM/TF-A, checks packaged contents, and publishes hashes with provenance.
Do not replace the runtime profile's hashes without reviewing a new build.

For the existing verified publication, stage its runtime firmware:

```bash
FW="$PWD/.packages/cca-tdisp-fvp/artifacts/rebuild-20260917-verified"
RUNTIME="$PWD/.packages/cca-tdisp-runtime"
(cd "$FW" && sha256sum --check SHA256SUMS)
cp "$FW/bl1.bin" "$FW/fip.bin" "$FW/dt_bootargs.dtb" \
  "$RUNTIME/package/cca-3world/"
```

Restore the exact editable tool environment in a fresh checkout:

```bash
TOOL="$PWD/.packages/cca-tdisp-runtime/cca-test/shrinkwrap"
git clone https://git.gitlab.arm.com/tooling/shrinkwrap.git "$TOOL"
git -C "$TOOL" checkout --detach 1c6b7a5278b47be11cad3bcd3a20416fc43fd388
python3 -m venv "$TOOL/venv"
"$TOOL/venv/bin/python" -m pip install --editable "$TOOL" \
  'PyYAML==6.0.3' 'termcolor==3.3.0' 'tuxmake==1.43.0'
rm -r -- "$TOOL/src/shrinkwraptool.egg-info"
"$TOOL/venv/bin/python" -I -B -m pip freeze --disable-pip-version-check
```

Remove only that generated source metadata directory, not the installed
`venv/` distribution metadata. The editable import continues to use the pinned
source; the installed `direct_url.json` records its identity. The strict runner
accepts `venv/` but rejects `.venv/`, generated source metadata, a non-editable
wheel, or dependency versions outside `fvp-cca-v15.pip-freeze`.
No validation guard was relaxed during recovery.

### Committed-stack rerun

Three restoration attempts stopped before model execution because of the
virtualenv path, checkout cleanliness, and editable-package identity checks.
They are tooling failures, not guest or TDISP results.

The fourth attempt passed those checks and started native nextest:

```text
Execution stack: kwovwzpq (jj change ID)
Invocation: single-boot-2987526-1789612384327479418
FVP: 284acc4b7ad79c204c42fbf06ac2d7d734d71bbbe6a570dcd71f18750e8af464
nextest: 2ad152d7-6f6b-4684-8fe3-2bd37f33d0fa
```

Results are under `vmm_test_results/cca-tdisp-committed-stack/`.
The deferred hold is not included. The firmware profile and recovery files
are the only additional runtime-input changes.

The rerun completed and reproduced LOCK/RUN and the full I/O milestone:

| Outcome | Result |
|---|---|
| Native LOCK | Confirmed at OpenVMM timestamp 104.591285510 s |
| Native RUN | Confirmed at 106.941596290 s |
| Guest acceptance | Original LOCK and ACCEPT markers present |
| AHCI read | All 64 MiB read; hash `281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6` matched |
| Interrupts | 132 AHCI MSI-X interrupts |
| UNLOCK | Rejected at the existing protected-mapping guard; no UNLOCK-returned marker |
| Native nextest | One test failed after 541.041 s; suite command exit 100 |
| Fixture teardown | Exit 1, `step=realm-binding` |
| FVP launcher shutdown | Exit 1 |
| Output preservation | Succeeded; no report-write errors |
| Outer Flowey command | Exit 255; overall failure |

Under the invocation's `outputs/fvp-284acc4b.../test_results/`, the selected
test directory contains `openvmm.log`, `linux.log` and `petri.log`.
`openvmm.log:131` records the protected-mapping guard error;
`petri.log:22-27` records `SingleStep` instead of poweroff and the missing
UNLOCK marker. `nextest-single-boot.xml:2-5` records the failed native test.
The invocation's `outputs/session-result.json` records the separate command,
fixture, launcher and preservation outcomes; the FVP output directory also
contains `fixture-teardown.stderr.log` with the failing cleanup step.

This is new runtime evidence for the committed execution stack without the
provisional hold and with rebuilt firmware. It confirms that the split stack
still reaches the intended LOCK/RUN and read/hash milestone. It does not
qualify teardown, same-host reuse or instrumented private-buffer DMA.
