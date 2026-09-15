# Linux host TDISP for Arm CCA guests in OpenVMM

Date: 2026-09-11

Updated: 2026-09-15

Status: the OpenVMM guest_memfd in-place memory foundation is implemented.
The paired CCA Virtio-vsock VMM test passes on FVP. The existing separate-backing
test still passes on QEMU and FVP. Native OpenVMM TDISP assignment is not
implemented. The local kvmtool reference has demonstrated one bounded
private-buffer DMA read, but clean device teardown and negative isolation
controls remain unqualified. See the runtime results below.

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

The existing Linux VFIO and FVP infrastructure is substantial and reusable.
However, the current CCA configuration explicitly rejects VFIO devices. Merely
removing that rejection would enable an unsupported memory and access path.

## 2. Inputs and evidence

The starting point is
[`~/lkml/findings-cca-tdisp-v7-openvmm.md`](../../../lkml/findings-cca-tdisp-v7-openvmm.md).
This plan checks that report against the current OpenVMM and local Linux sources,
and the pinned kvmtool reference. Paths beginning with `../linux-cca`,
`../kvmtool-cca`, or `../tf-rmm` refer to the sibling checkouts.

| Input | Reference inspected | Qualification |
|---|---|---|
| OpenVMM | Plan baseline change ID `rukkunwr`, based on change ID `llntpwqp`, bookmark `cca-v15-fvp-upstream` | Later foundation changes are listed below; no OpenVMM DA runtime qualification |
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
Private prefaulting for assigned devices and coordinated IOAS mapping remain
future work.

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
passed and 1 skipped. The matched guest-VM coverage is Virtio-vsock; the other
listed Virtio devices have crate
regression coverage, not new CCA guest-VM tests.

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
| KVM | Realm creation, population, memory-fault handling, GICv3 | Add v7 memory mode, Arm SMCCC exits/register completion, TIO exits and prefault |
| VM assembly | CCA validation, PCI roots, device-tree generation, resource resolution | Admit only the new CCA-aware resource; wire partition/device services and address views |
| Petri | Realm boot and ordinary AArch64 VFIO tests | Combine their patterns with a DA-specific fixture and real lock/accept/I/O assertions |
| FVP | Validated v15 tuple, initrd boot, checked staging, logs, deadlines, cleanup | Add a separate DA tuple, payload, PCI assets, and L1 provisioning |

## 4. Integrating with `vm/devices/tdisp`

### What it does today

`TdispHostDeviceInterface` supplies negotiate, bind, start, unbind, and report
callbacks. `TdispHostDeviceTargetEmulator` accepts `GuestToHostCommand` and
uses `TdispHostStateMachine` to call those callbacks. VPCI deserializes a
protobuf message, obtains `ChipsetDevice::supports_tdisp()`, and dispatches
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

These are reasons to preserve the existing emulator's behavior, not to make
the Linux guest pretend to negotiate an OpenHCL protocol.

### Proposed split

Add a transport-independent host module under `vm/devices/tdisp`, with native
operation types and lifecycle bookkeeping. The exact Rust names below are
proposals:

| Proposed piece | Responsibility |
|---|---|
| `tdisp::host::DeviceState` / transition helper | Confirmed device state, transition-in-progress, quarantined outcome, transition history |
| Native object/request types | Object identity, read offset/length, regeneration flags/nonce; no raw user pointers |
| `vfio_assigned_device::cca` coordinator | Own the per-device state, access gate, stable guest identity, IOMMUFD binding and backend operations |
| `virt_kvm` RHI adapter | Decode Arm registers and shared buffers; translate native results to RHI |
| Existing protobuf/VPCI adapter | Remains the OpenHCL/synthetic-device frontend |

Use one coordinator instance per assigned device for both RHI requests and TIO
validation. Do not create a separate TDISP state machine inside each exit
handler. Keep Linux ioctl code in `vfio_sys`/the VFIO backend, not in `tdisp`.
Keep Arm calling-convention code out of the generic device crate.

Initially leave `TdispHostDeviceTargetEmulator` and its tests intact. Reuse its
state vocabulary and report support through explicit conversions. Extract
small shared transition predicates where their semantics truly match. A later
migration of the legacy emulator to the new core must preserve its existing
wire results and invalid-request behavior; it is not a prerequisite for CCA.
This additive approach avoids changing OpenHCL behavior to fit a different
transport.

`tdisp::devicereport` is useful for reading interface-report ranges for
diagnostics/access policy. Before using it for host access decisions, add
fixtures from the pinned Linux/FVP format and confirm byte order, range IDs,
count bounds, vendor-tail treatment, and page alignment. Keep the original
evidence bytes unchanged when returning them to the guest. Parsing an object
does not authenticate it. [O1, K5]

No OpenHCL image, VMBus, VPCI device, protobuf CCA protocol tag, or emulated
DOE device is required for the native Linux path.

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
needed; `supports_tdisp()` alone does not connect KVM exits to the device.
[O2-O5]

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

OpenVMM's existing `Exit::Hypercall` is x86-only. Arm KVM fills `nr`/`flags`,
not the x86 argument/result contract. Add Arm-specific register handling via
`KVM_GET_ONE_REG` and `KVM_SET_ONE_REG`, including results in x0-x3. Preserve
the existing kernel PSCI handling. [O3, K2, V3]

Install forwarding filters before first run for the two exact DA ranges:
`0xc500004b..=0xc500004d` and `0xc5000052..=0xc5000054`.
RHI uses SMCCC owner 5 (`STANDARD_HYP`), not owner 4. Derive these constants
from the pinned definitions and test their full encoded values.
Leave host-configuration calls at `0xc500004e..=0xc5000050` to KVM. Validate each function;
do not forward a broad range of unrelated SMCCC calls. [K2, V3]

### Request translation

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
aperture or root is a candidate, not a demonstrated requirement. [K8]
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

Add a test next to `boot_linux_direct_cca` in
`vmm_tests/vmm_tests/tests/tests/aarch64_exclusive.rs`. Proposed name:
`boot_linux_direct_cca_tdisp_ahci`. Reuse the base CCA test's small RAM/vCPU
configuration, non-hotplug PCI topology, and virtio-vsock agent. Reuse the
ordinary VFIO test's pre-opened resource pattern, but construct the new
CCA-aware resource. Do not use its plain `VfioCdevDeviceHandle` unchanged.
[O9]

### Test procedure

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

### Next qualification step: prove the actual DMA path

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

After the proposed profile/capability/test are implemented and the new roots
are provisioned, the intended command shape is:

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca-tdisp.toml \
  --fvp-platform-root /absolute/path/to/qualified-da-platform \
  --shrinkwrap-package-root /absolute/path/to/qualified-da-package \
  --filter 'binary(=tests) & test(=aarch64_exclusive::openvmm_linux_aarch64_boot_linux_direct_cca_tdisp_ahci)'
```

This is **not runnable with the current profile/test implementation**. If
payload selection needs an additional option, add it to this example during
implementation rather than silently selecting the old v15 payload.

Require exactly one executed passing test. Zero-test, ignored-only and
listing-only results are not success. Keep this test out of QEMU's current
`aarch64_tcg` name-based CI selection.

The root execution-target plan is explicitly deferred. This work does not
resume it or assume `targets(fvp_cca)` exists. Use the exact manual filter,
a DA capability, and runtime fixture checks. Preserve existing in-incubator
discovery behavior; no persistent FVP session or enumeration optimization is
required.

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

| Stage | Main files/crates | Exit criterion |
|---|---|---|
| A. Reference and interrupt qualification | Pinned Linux/kvmtool/TF-RMM build manifest; DA model assets; qualification helper | Recorded reference/IRQ baseline plus section 10 buffer-level qualification and clean lifecycle; current read smoke success alone does not close A |
| B. v7 memory ABI and backing | `vm/kvm`; `vmm_core/virt_kvm/{cca,memory}`; `openvmm/membacking`; worker assembly | v7 Realm boots; INIT_RIPAS ledgers, both memory-fault return forms/cause classification, pending completion and both conversion directions pass; v15 unchanged |
| C. DA FVP/payload mode | `petri/incubator/{profile,fvp,cca_init}`; platform/profile files; `resolve_cca_payload`; Flowey runner/pipeline; Petri artifacts | Validated DA L1 and guest artifacts; readiness-gated fixture; negative identity tests |
| D. Host object path | `vfio_sys/{cdev,iommufd}`; VFIO resources/resolver/manager; KVM association service | Realm vIOMMU/vdevice/S1-bypass attach and partial-allocation cleanup; no live TDISP state requests yet |
| E. Native TDISP/RHI | `tdisp` host module; VFIO CCA coordinator; Arm KVM exit/register adapter | Whole-object snapshot adapter, guest buffer encoding, mocked requests and evidence transport pass; real LOCK/RUN remain disabled |
| F. Access and DMA completion | VFIO BAR/config/IRQ paths; TIO handler; memory/DMA coordinator; DT/address integration | Real transitions enabled only now; protected MMIO, private/shared DMA and observed interrupt mode work |
| G. End-to-end and lifecycle | Petri fixture; new test and helper/instrumentation; fault tests; Guide | Exact FVP test and required evidence pass with clean teardown; record any failures separately, never as overall success |

Stage A has a protocol/read baseline and known MSI-X mode, but remains
incomplete for confidential DMA and clean lifecycle. The next qualification
task is the buffer-level test in section 10; investigate the two shutdown failures
separately. Do not use repeated forced shutdown as a clean-reuse result.

Stage C scaffolding and B can proceed using the recorded local candidate;
publishing a qualified DA platform/test still requires the open Stage A gates.
D depends on B;
E can develop against fakes alongside D. F requires B, D and E. G requires
the qualified C/F outputs. Interrupt work discovered in A is a prerequisite
for F, not deferred cleanup.

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
