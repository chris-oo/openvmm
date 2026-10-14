# MSHV bounce I/O: branch scope, enforcement, and control-page bookkeeping

Date: 2026-10-14.

This records the investigation, not an implementation plan. The investigation
itself changed no product code. A later user-approved simplification is
recorded immediately below; the rest describes the original source snapshot.

## Follow-up: redundant userspace control bookkeeping removed

After checking the backend contracts, the user-approved follow-up removes
`snp_host_access.rs`, `RootAccessGuard`, the control reservation table, its
4,096-entry budget, duplicate-binding checks, and userspace control exclusions.
Checked range/attribute parsing now lives directly in `snp.rs`.

This corrects the earlier assumption that a programmed control GPA always
implies a persistent kernel reference to that guest RAM frame. In particular,
the kernel maps a distinct VP GHCB state page, and the PSP ioctl waits for
completion before returning. The absence of a legacy kernel-side ledger does
not by itself establish a need for a userspace ledger.

The change keeps pre-control host-permission releases, whole-batch RAM/range
validation, backend error propagation, GHCB clear-before-replacement and
confirmed unregister, CopyOnly I/O, and RAM/fd lifetime ownership. Kernel lite
bindings and fail-stop/reclaim handling are unchanged. Backend contracts, not
an extra VMM table, decide which control operations are accepted.

Representative kernel evidence:
`drivers/hv/mshv_root_main.c:1741-1752,2605-2650,2828-2877`;
`drivers/hv/mshv_root_hv_call.c:1608-1632`;
`drivers/hv/mshv_root_main.c:1858-1865`;
`drivers/hv/mshv_guest_memfd.c:975-1106,1250-1318`.
These paths refer to the separate kernel checkout used for contract research.
No kernel code was changed by this simplification.

The earlier control-registry discussion below is historical, not a claim that
the removed table is required. Source-contract review does not establish an
exact source-to-running-hypervisor binary match or complete isolation proof.

## Follow-up: separate locking capabilities and protected CPU loans

The next user-approved change separates `supports_kernel_lock()` from
`supports_user_lock()`. `supports_locking()` retains its kernel-access
meaning. Devices select owned staging from the actual memory's kernel-lock
capability, while explicit bounce options still force staging. Unsupported
kernel-dependent paths reject memory rather than export guest pages.
`requires_owned_io()` distinguishes enforced ownership from raw-lock
availability. Ordinary IOMMU-translated memory keeps its existing copying
fallbacks, but forwards any underlying ownership restriction.

`lock_user_gpns()` returns a CPU-only `UserLockedPages` handle. It reserves
pages before probes, rolls back failed setup, and keeps the backing owner
alive through unlock. These loans protect CPU addresses and permissions, not
OS residency or concurrent byte access. They do not authorize GUP or DMA.

MSHV SNP shares loan tracking with mapping and permission changes. A complete
guest request is checked before permissions are released. A loan conflict
halts the requesting VP and retains the locked permissions. Transient change
guards exclude new loans while control or mapping operations run. Ordinary
fault-contained copies remain revocable and do not register loans.

This is not the deleted control-binding registry: it tracks explicit CPU
pointer lifetimes, not persistent GHCB, doorbell, or VMSA identities. Kernel
control-page ownership remains authoritative.

Implementation and local validation are complete: 693 combined package
tests passed, with package clippy/rustdoc, repository formatting, and an ARM
MSHV check. Formal review accepted the mapping, teardown, and halt fixes;
rubber-duck review accepted the device and foundation fixes. The static
MSHV/Consomme candidate builds.

Fresh hardware validation is blocked by test-host connectivity. The first
SSH connection timed out before deployment or guest launch. A bounded retry
and a Windows-side TCP check also failed. Candidate artifacts are retained
in `target/protected-user-loans-hardware/`. Earlier hardware results do not
validate the new CPU-loan paths, and the planned normal lifecycle does not
exercise an explicit loan conflicting with a real guest transition.

Logical submission groups remain queued for discussion. History has not yet
been reorganized.

## Summary

- Most virtio changes implement owned staging, asynchronous buffer retention,
  or fallible queue completion. They are not unrelated to bounce I/O.
- There are broader correctness changes worth separate review, especially
  descriptor arithmetic and FileDisk semantics. Packed publication progress
  must stay coupled to the fallible-completion contract.
- `snp_host_access.rs` is now a control-page/range helper, not the old
  ordinary-copy coordinator. Its remaining guard names are misleading.
- A "root operation" submits guest control pages to SNP/MSHV. It is not a
  bounce-buffer operation. Its references cannot be discarded merely because
  byte copies no longer need leases.
- CopyOnly already blocks safe pointer/pin/DMA/shared-memory exports. But
  **the bounce bookmark lacks runtime PCIe hot-add admission checks** that
  exist on the lite/integration workstream.
- Owned backend-buffer provenance and shared device admission address the
  normal-I/O rule more directly than a second acquisition mechanism.
  A copy-purpose capability is optional additional restriction, and must
  also permit queue metadata and bootstrap access.

## Scope and source snapshot

The branch under review is `mshv-bounce`, change ID `xluknrmp`, against its
`main` ancestor, change ID `tokqorux`. The working checkout also contains the
separate lite workstream, change ID `nnwzvpmq`, and the source-neutral integration
merge, change ID `lqwwyzku`.

Unless stated otherwise, source line references below refer to `xluknrmp`, not
the integration checkout. For example:

```bash
jj file show -r xluknrmp vmm_core/virt_mshv/src/x86_64/snp.rs
jj diff --from tokqorux --to xluknrmp --stat
```

This distinction matters: the lite workstream adds functions, fields, and tests
that shift line numbers. Kernel-owned fd creation, Linux-MM enforcement, and
the `guest_memfd_lite.rs` implementation are **not** part of `mshv-bounce`.
The integration has a further 21-file delta over the bounce branch.

The bounce diff contains **68 files, 11,145 insertions, and 468 deletions**.
Those are physical diff counts, not production-code counts. They include six
new root-level plans/reports, Guide text, dependency manifests, standalone test
files, and many inline tests.

## What changed in this branch

The branch grew through four stages:

| Stage | Purpose | Representative change IDs |
|---|---|---|
| Copy-only memory and block I/O | Prevent guest-backed disk buffers reaching backends; provide owned staging | `xqkktvtr`, `pulyttxv`, `rpxxuosk` |
| Network and vsock I/O | Extend the same rule to software packets and stream buffers | `opxpvzmv`, `oymvrkom`, `otkzxzss` |
| Initial scoped SNP access | Add per-copy guards and a coordinator, plus backing/control lifetimes | `znmztzpx`, `slknztno`, `qxkpmovr` |
| Retained grants and mandatory SNP bouncing | Remove ordinary-copy coordination and the legacy SNP bypass | `ulsknpvw`, `xluknrmp` |

There is also an archived revocation experiment, change ID `rwzwywmo`. Its
instrumentation was removed from the production result. History therefore
shows more machinery than the current source contains.

### Change map

| Area | What the diff does | Relationship to the request |
|---|---|---|
| `guestmem` | Copy-only views, fallible owned allocation, safe-copy fault retry, export restrictions, tests | Core policy and copy mechanism |
| Common storage buffers and `disk_backend` | Stage restricted disk requests in owned memory | Core block I/O boundary |
| `disk_file` | Exact transfers, EOF handling, force-unit-access behavior | Coupled backend correctness, with effects outside SNP |
| `virtio_blk` | Bounded payload staging, approved backend resolution, fallible completion | Core block device support |
| `virtio_net` | Owned backend-facing packet pools, RX copyback, TX lifetime tracking | Core network support; larger than a single copy |
| `virtio_vsock` | Owned stream buffers and safe queue/connection retirement | Core in-process vsock support |
| Shared virtio queues | Fallible completion/publication and ordered retirement | Needed when guest-memory accesses can fail; some general fixes have broader scope |
| Device resources and CLI | Carry bounce settings, choose SNP defaults, reject unsupported combinations | Admission and wiring |
| VM-service | Reject unadmitted MSHV SNP devices; select supported memory backing | Closes a second entry point, not a new data path |
| MSHV permission handling | Read/RW fault acquisition and conflict-triggered release/resume | Core revocable permission model |
| MSHV control references | Protect GHCB, VMSA, doorbell, and PSP pages | Separate control-protocol lifetime requirement |
| Partition/RAM ownership | Keep RAM and mappings alive until kernel references close | Teardown correctness |
| Petri, VMM tests, and vhost constructors | Initialize new resource fields to `false` | Mechanical compatibility, not new device behavior |
| Plans, reviews, and hardware records | Record decisions and evidence | Task artifacts, not runtime requirements |

Most manifest changes add test dependencies already present in the workspace.
The `Cargo.lock` delta adds dependency edges, not new package versions.
`virtio_blk` also adds existing `disk_backend_resources` and Linux `nix` usage.

The small changes under `petri`, `vmm_tests`, and `openvmm_vhost` are literal
`bounce_io: false` field initializers. They preserve those callers' previous
behavior. For examples, see `petri/src/vm/openvmm/construct.rs:284-292`,
`petri/src/vm/openvmm/modify.rs:269-278`, and
`openvmm_vhost/openvmm_vhost/src/main.rs:105-113`.

### What the current SNP permission path actually does

The partition reports copy-only access whenever it is SNP:
`vmm_core/virt_mshv/src/lib.rs:1010-1037`.

An ordinary guest-memory fault goes through range/mapping checks and requests
the needed read or write permission. It does not register a copy lease:
`vmm_core/virt_mshv/src/x86_64/snp.rs:287-323`.

A GPA-attribute intercept validates the request, releases conflicting host
permissions, and returns to the normal RUN path. It does not invent a new
completion-register encoding:
`vmm_core/virt_mshv/src/x86_64/snp.rs:345-368,1391-1412`.

The important distinction is:

```text
Old scoped model: reserve copy -> acquire -> copy -> release -> drop lease
Current model:    contained fault -> acquire -> retry copy
                 guest conflict -> release -> resume guest
```

There is no ordinary-copy cancellation ledger or permission guard left in
`snp_host_access.rs`. That does **not** mean there are no locks anywhere.
Permission/mapping changes still use `partition.memory`, and control-reference
admission uses a registry mutex. Neither mutex spans the payload byte copy.
See `snp.rs:229-269,287-309,345-368` and
`snp_host_access.rs:95-104`.

## Are the virtio and storage changes unrelated?

**The bulk of the device work is related, but some general correctness changes
deserve their own explanation and review scope.** A change can be a prerequisite
for the feature without itself being SNP-specific.

| Change | Why it is here | Scope assessment |
|---|---|---|
| Common virtio fallible completion | A used-ring publication or notification access can fail when guest-memory access is revocable; callers need a result instead of hidden failure | Shared infrastructure needed by the new device paths |
| In-order completion retirement | Stop publishing/reusing a queue after an uncertain completion and preserve ordered ownership | Coupled lifetime/correctness work; generic invariant fixes can be reviewed separately |
| Packed completion cursor update | Publication may succeed before reading notification flags fails; the internal cursor must reflect the already-visible publication | Generic correctness prerequisite worth a separate commit; not safe to discard from a fallible-completion design |
| Descriptor-region address arithmetic | Avoid wrapping an address when skipping descriptor bytes | General hardening that can stand alone |
| `OwnedIoBuffers` and Disk staging | Keep guest buffers out of backend I/O and copy read data back only after success | Direct implementation of the block requirement |
| Exact FileDisk transfers | A partial backend transfer must not look like complete staging success | Coupled correctness prerequisite; can land separately before staging |
| FileDisk FUA handling | Honor requested durable-write semantics | General backend semantics, not host-access isolation |
| Network packet arenas and queue retirement | Backends may consume TX later or complete RX asynchronously; ownership must outlive the callback | Direct network requirement, not merely an extra memcpy |
| Vsock fallback buffers and retirement | Partial stream writes and old queue generations can keep buffers alive | Direct stream/lifetime requirement |

Evidence:

- `vm/devices/virtio/virtio/src/common.rs:394-443`.
- `vm/devices/virtio/virtio/src/in_order.rs:76-123,159-220`.
- `vm/devices/virtio/virtio/src/queue/packed.rs:235-298`.
- `vm/devices/virtio/virtio/src/regions.rs:55-58`.
- `vm/devices/storage/scsi_buffers/src/lib.rs:172-220`.
- `vm/devices/storage/disk_backend/src/lib.rs:441-607`.
- `vm/devices/storage/disk_file/src/lib.rs:102-192,238-341`.
- `vm/devices/storage/disk_file/src/readwriteat.rs:18-57`.

The shared completion changes concern failure while touching queue memory,
not a rule that every ordinary backend I/O error must retire the entire queue.

### What could be separated for review

Three useful review scopes are generic correctness fixes, owned-buffer device
support, and MSHV-specific permissions/control lifetime. Dependencies between
them still matter; separating review does not mean removing prerequisites.

The most obvious standalone items are descriptor arithmetic and generic
FileDisk semantics. Packed publication progress is also generic, but the
bounce paths rely on correct behavior when publication succeeds and a later
notification read fails.

The resource-field initializer changes should remain with the resource change;
otherwise their callers stop compiling. The large plans and historical review
files are task artifacts. They can be kept outside a source PR's runtime
narrative without pretending they were unrelated to the original task.

The historical scoped coordinator is **not** a remaining cleanup candidate:
it was already removed. The remaining cleanup target is the naming and shape
of the control-page machinery described below.

### Behavior changes to keep visible

This is not only a performance/data-copy change:

- MSHV SNP bouncing is mandatory; the former opt-out path no longer exists.
- SNP CLI admission now requires base pages and rejects unreviewed devices,
  existing memory backing files, debugger setup, dumps, and restore paths.
- VM-service SNP device support is narrower than CLI support.
- Owned queue paths need defined failure and retirement behavior; bounce
  save/restore support is not implied by an ordinary stop/start path.
- AP/doorbell replacement is conservative: retained control bindings are not
  silently replaced, and SNP doorbell disable is rejected.
- Common FileDisk transfer/FUA corrections apply beyond SNP.

The first three are shown by the mandatory-policy and startup validators
cited elsewhere in this report. The control restrictions are shown by
`snp_host_access.rs:106-118` and `snp.rs:1483-1487`. FileDisk changes are shown
by the storage citations above.

These initial lifecycle restrictions are implementation limits, not
fundamental properties of bounce buffering. They should stay explicit in
review and documentation.

## What "root operation" means

The name is misleading. It does not mean a bounce copy, a filesystem operation,
a new privilege level in OpenVMM, or a root-owned disk buffer.

Here it means **a host/root-partition operation that submits guest control-page
addresses to MSHV or the SNP control protocol**. Those pages are not ordinary
paravirtualized I/O payloads.

| Control use | Submitted pages | When the reservation ends | Bounce-branch evidence |
|---|---|---|---|
| PSP guest request | Request and response pages | Successful request completion; uncertain errors retain them | `snp.rs:2025-2058` |
| GHCB registration | Per-VP communication page | Confirmed old-register clear or unregister | `snp.rs:1622-1694` |
| GHCB AP creation | Target VP's VMSA | Kept until partition teardown in the current model | `snp.rs:2154-2187` |
| Hyper-V StartVirtualProcessor | Target VP's VMSA | Same retained AP binding as GHCB creation | `x86_64/mod.rs:1369-1408` |
| SNP doorbell registration | Doorbell page | Kept until partition teardown; disable is currently rejected | `snp.rs:1483-1522` |
| BSP launch VMSA | Initial processor-state page | Static exclusion for partition lifetime | `snp.rs:917-921` |

The table's `snp.rs` and `x86_64/mod.rs` are under
`vmm_core/virt_mshv/src/x86_64/`.

Both AP creation entry points use `ControlBinding::Ap(target_vp)`. A second
binding for that VP is rejected rather than silently replacing a retained
reference: `snp_host_access.rs:106-118`.

### Why this is separate from bounce-buffer lifetime

An ordinary copy can be interrupted by revocation and handled by annotated
fault recovery. That does not prove that a kernel/hypervisor control-page
reference has ended.

The guest cannot safely reuse a control page merely because OpenVMM finished
a payload copy or returned from a userspace function. The control operation
needs its own completion or unregister contract.

The current helper therefore does this:

1. Reserve the control range and reject overlap or duplicate binding.
2. Validate mapped RAM.
3. Close cached ordinary host grants for that range.
4. Submit the control operation.
5. Remove the reservation only on confirmed completion, or keep it for a
   persistent/uncertain reference.

Evidence: `snp.rs:229-269`; `snp_host_access.rs:106-194`; the call sites above.
Before launch finishes, `control_tracking_active()` returns false and the
helper returns no guard: `snp.rs:236-238,279-284`. The BSP launch page has a
separate static exclusion.

This is why deleting the control registry just because copy cancellation is
gone would remove a different safeguard.

### What the guard state means

`RootAccessGuard` is a reservation token, not a host-access grant.

| Action | Meaning |
|---|---|
| Drop before `start()` | No control ioctl was marked issued; remove the reservation |
| `start()` | The operation may now have side effects |
| `complete()` | Remove the reservation after confirmed completion |
| `retain()` | Leave the table entry after a persistent binding |
| `mark_granted()` | Keep the table entry while retaining a token for later explicit completion |
| Drop after issue without completion | Log uncertainty and keep the table entry |

The implementation is at `snp_host_access.rs:144-194`.

Despite its name, `mark_granted()` does not grant ordinary host access. It
changes reservation bookkeeping. GHCB uses the retained token because it has
a later unregister path; AP VMSA and doorbell use persistent entries.

An uncertain control reference blocks overlapping permission/mapping changes.
It does not poison every unrelated ordinary copy. The test
`control_references_survive_uncertain_operations_without_poisoning_copies`
checks this distinction: `snp_host_access.rs:307-367`.

### Is `snp_host_access.rs` overcomplicated?

It is smaller and narrower than its historical name suggests:

- **295 physical non-test lines**, including comments and blanks.
- **364 lines in the test section**.
- **659 total lines**.

The production part has three responsibilities: checked range/RAM validation,
control-reference bookkeeping, and GPA-attribute batch validation/release.
It is not a general copy coordinator anymore.

The remaining complexity has real reasons: guest-controlled addresses,
partial hypercall outcomes, persistent control bindings, and fallible/bounded
allocation. The 4,096-entry limit prevents unbounded control bookkeeping:
`snp_host_access.rs:10-11,106-140`.

However, the naming and representation can be simpler. These are options,
not changes made in this investigation:

| Candidate | Benefit | Requirement to preserve |
|---|---|---|
| Rename `RootAccessGuard` to a control-page reservation/token | Stops confusing it with copy permission acquisition | Persistent and uncertain references must still survive |
| Rename `reserve_root_operation` to a control-page preparation operation | Describes validation and closing grants | Keep reserve-before-submit ordering |
| Replace `live`/`issued` booleans with named lifecycle states | Makes Drop behavior and terminal transitions easier to review | Keep unissued, uncertain, persistent, and completed outcomes distinct |
| Rename `mark_granted()` to a reference-retention action | Removes the false implication that it grants copy access | GHCB still needs explicit later completion |
| Reduce duplicated static/GHCB/registry bookkeeping | Potentially fewer containers and special cases | Preserve launch-time exclusion and per-VP unregister behavior |
| Trim proof counters/rate-limited success logs for PR presentation | Less observability-only production code | Preserve useful failure diagnostics and required acceptance evidence |

The current structures are visible together at `snp.rs:170-199`. This is a
plausible cleanup area, not proof that the entire registry can be removed.

### RAM ownership is another separate requirement

`MshvMemoryOwner` closes its kernel fd before releasing retained RAM owners.
VP/binder field ordering also keeps the RAM owner after relevant fd/mapping
owners. The drop-order test covers the intended order:
`vmm_core/virt_mshv/src/lib.rs:299-363,365-375,542-549`.

`PartitionMapper` transfers teardown ownership when the partition retains
backing, rather than unmapping merely because a mapper object goes away:
`openvmm/membacking/src/partition_mapper.rs:37-61`.
The shared contract is documented in
`vmm_core/virt/src/generic/partition_memory_map.rs:7-17`.

This is not ordinary-copy cancellation either. Removing it would conflate
"a device request finished" with "all kernel references to RAM ended."

## Bounce-only I/O: the guarantee we should ask for

There are three different requirements:

| Requirement | What it protects | What it does not prove |
|---|---|---|
| Deny unapproved devices | Only reviewed device/backend combinations can start | Every later I/O call uses owned memory |
| Never export guest-backed memory for normal backend I/O | Kernel/device I/O cannot pin or retain guest payload pages | Only payload copies can acquire host permissions |
| Allow only approved guest-memory operations | Which VMM callers may touch guest memory | Cached hardware permissions disappear when a function returns |

For normal disk, network, and vsock I/O, the desired flow is:

```text
write/TX: guest memory -> contained copy -> owned buffer -> backend/kernel I/O
read/RX:  backend/kernel I/O -> owned buffer -> contained copy -> guest memory
```

The owned buffer must stay alive until the backend releases it. Merely copying
into temporary memory is insufficient if an asynchronous backend keeps a
pointer after that temporary memory is dropped.

### Existing memory and storage boundaries

`GuestMemoryIoPolicy::CopyOnly` centrally refuses the safe APIs that export
mapping pointers, lockable ranges, DMA addresses, shared-memory handles, and
raw allocated-buffer slices. It is stronger than an instruction telling each
device author to remember to bounce.

Evidence in `vm/vmcore/guestmem/src/lib.rs`:

- Policy and monotonic view restriction: `52-61,1480-1490`.
- Raw allocated-buffer extraction: `1500-1532`.
- Lockable subranges and full mappings: `1601-1618`.
- IOVA and sharing: `1631-1647`.

These are userspace API restrictions, not kernel MM enforcement. Existing
exported pointers or locks are not retroactively revoked by attaching a
stricter policy. Initialization must establish CopyOnly before such exports.
That is different from claiming that an ordinary `GuestMemory` clone keeps
Direct permission forever.

Clones share the `GuestMemoryInner` policy latch and dynamically observe
backing-policy tightening. The non-revocable artifacts are already-exported
pointers, DMA addresses, sharing objects, and locks, not ordinary clones:
`guestmem/src/lib.rs:1119-1122,1471-1490,2390-2401`.

The common `Disk` wrapper also intercepts CopyOnly request buffers. It creates
owned staging buffers, copies writes in before backend submission, and copies
reads out only after successful backend completion. Thus storage has a
shared runtime boundary rather than only per-device flag checks:

- `vm/devices/storage/disk_backend/src/lib.rs:520-607`.
- `vm/devices/storage/scsi_buffers/src/lib.rs:172-220`.

Network and vsock require equivalent backend-facing ownership and retention,
but have different asynchronous packet/stream interfaces. Their buffers must
not be treated as interchangeable with the storage wrapper.

### Device admission: an important branch-specific gap

There are startup checks, but they are not a complete runtime device-admission
boundary on `mshv-bounce`.

| Entry point | Bounce-branch behavior | Evidence |
|---|---|---|
| CLI configuration | Automatically bounce supported SNP devices; reject the unsupported startup combinations | `openvmm/openvmm_entry/src/cli_args.rs:1589-1595,1595-1680` |
| VM-service creation | MSHV SNP accepts only the minimal chipset and serial, without crash dumps; uses base pages | `openvmm/openvmm_entry/src/ttrpc/mod.rs:653-694,897-900` |
| `AddPcieDevice` worker RPC | No CopyOnly admission check against the startup allowlist | `openvmm/openvmm_core/src/worker/dispatch.rs:3952` and following handler |
| `AddVmbusDevice` worker RPC | No CopyOnly admission check; requires an existing VMBus server | `openvmm/openvmm_core/src/worker/dispatch.rs:3868-3887` |
| `ReadMemory` / `WriteMemory` worker RPCs | Use ordinary guest-memory copies, not an explicit CopyOnly denial | `openvmm/openvmm_core/src/worker/dispatch.rs:3929-3940` |
| Custom resource/config construction | Does not inherently pass through the CLI validator | Startup validators above versus worker handlers |

The normal CLI SNP allowlist omits VMBus, so a missing VMBus server rejects
that particular runtime operation. This is an incidental construction limit,
not a general CopyOnly device policy.

PCIe topology is present for the supported SNP devices. Consequently, runtime
PCIe addition cannot be described as categorically blocked by the bounce
branch. A resolver or device might still fail when it attempts an export that
CopyOnly denies. That late failure is not the same guarantee as rejecting
every non-bouncing device before activation.

Likewise, the memory RPCs can attempt ordinary contained reads/writes; whether
an individual request succeeds still depends on valid GPA and permitted
host access. They are not unconditional private-page access.

**Do not confuse this with the integration checkout.** The CopyOnly guards
around worker hot-add and memory RPCs were added by the lite workstream.
`dispatch.rs` has no delta from `tokqorux` to `xluknrmp`, but has a 364-line
delta from bounce to integration. The integration's guards do not make the
standalone bounce bookmark enforce them.

This is a concrete coverage gap to address if the promise is "all unsupported
devices are denied everywhere." It is not, by itself, proof that a normal
backend can successfully pin guest pages: the central CopyOnly export
restrictions still exist.

### Does this require a different copy path?

**Not to keep normal backend I/O off guest pages.** Existing annotated,
fault-contained copies can move data to and from owned staging allocations.
The crucial boundary is what memory reaches the backend, not a second memcpy
implementation.

**Possibly, if the additional goal is to restrict which callers can access
guest memory.** Today `PartitionHostAccess::acquire_host_access` has address,
length, and write direction, but no caller-purpose token:
`vmm_core/virt/src/generic/partition_memory_map.rs:80-97`.
The MSHV callback therefore cannot distinguish a payload bounce from a queue
metadata access merely from those arguments.

However, "only payload bounce copies may acquire access" is too narrow:

- Virtqueue descriptors, indices, used entries, and notification flags also
  need synchronous guest-memory access.
- Loader/bootstrap accesses are not device payload copies.
- GHCB, VMSA, PSP, and doorbell handling has separate control-protocol needs.
- A future supported debugger or memory dumper would need an explicit policy
  rather than inheriting payload authority.

A purpose-restricted interface could allow payload copies, queue metadata,
bootstrap access, and control operations as separate roles. It could still use
**one underlying fault-contained copier and one MSHV acquire operation**.
Duplicating the assembly copy machinery or creating a new kernel ABI is not
inherently required.

### A retained-grant limitation

Restricting the fault callback alone would not enforce every access. A later
copy can succeed without faulting because the page already has a cached grant.
If the goal is logical caller authorization, the API must check authority
**before every operation**, not only when handling a fault.

Even that does not make a granted VA inaccessible to arbitrary trusted/unsafe
code in the same process. Grants remain cached until an ownership conflict
or control operation releases them. A copy capability is a VMM API boundary,
not a hardware guarantee that a page is accessible only during one memcpy.

The absence of a per-copy grant ledger is intentional. Ordinary grants are
limited by the VM's RAM, not by the control registry's 4,096-entry limit.
Reintroducing per-copy release, cancellation, or cross-VP copy accounting is
not required for either admission checks or buffer-provenance checks.

### Options

| Option | Benefit | Cost or limitation |
|---|---|---|
| Existing allowlist plus central CopyOnly export restrictions | Small, already implemented foundation | Startup checks need consistent coverage across resource and runtime entry points |
| Shared device/backend admission at resource construction | Reject unsupported paths before activation, including non-CLI callers | Requires an explicit supported-capability model; does not itself prove buffer provenance |
| Owned/backend-safe buffer types at I/O boundaries | Makes payload provenance and asynchronous retention easier to enforce and review | Changes backend interfaces; must preserve legitimate Direct-mode paths for non-SNP guests |
| Purpose-restricted guest-memory copy capabilities | Limits which callers can perform payload, metadata, bootstrap, or control copies | Wider API change; cached grants mean fault-callback gating alone is insufficient |

For the owned-buffer option, **Direct policy is not ownership proof**.
Ordinary non-isolated guest RAM can be Direct too. A stronger type needs a
trusted owned-allocation constructor or explicit verified provenance. Checking
only `io_policy() == Direct` would not establish the intended invariant.

### Recommendation

Keep the existing single copy/fault mechanism. Pair reviewed device admission
with the central export/pin/DMA restrictions, and make normal backend buffers
provably owned wherever stronger enforcement is needed.

A shared admission boundary and narrow ownership-aware backend interfaces
address the requested I/O rule more directly than a second MSHV acquisition
path. A purpose-restricted copy API is an additional option if limiting
callers is also a requirement; it should cover queue metadata and bootstrap
operations, not only large payloads.

Keep SNP control protocols as explicit exceptions to the normal payload rule.
Guest RAM registration and protocol control-page submission necessarily
identify guest pages to the virtualization stack. "No guest page is ever
given to the kernel for any purpose" is a different, incompatible requirement
from "no guest payload page is used for ordinary backend I/O."

## Evidence and limits

The latest bounce-only hardware record is
`target/mandatory-snp-bounce-hardware-final/lifecycle.json`.
It records a real two-CPU SNP guest with plain `--hypervisor mshv` and no
per-device bounce flags:

- Block, TCP, vsock, and all three driver unbind/rebind cycles completed.
- 14,510 read and 16,397 write acquisitions occurred.
- 13 conflict releases had 13 subsequent successful RUN returns.
- Shutdown status was zero; the child partition disappeared.
- The host kernel log was unchanged.

That run used **legacy registered/pinned RAM**, not the lite fd backend. Its
65,536 pin-acquisition/release deltas were balanced. These are guest RAM
registration pins, not evidence that normal device I/O exported guest pages.
Bounce-only userspace policy is not Linux-MM private-page enforcement.

The existing lite acceptance record is separate:
`target/lite-cached-snp-hardware-final/lifecycle.json`. Do not attribute it to
the bounce-only bookmark or claim the entire combined feature is implemented
by `mshv-bounce`.

The latest source-validation results from the preceding implementation task
were 199 tests on the producer and 272 tests on the integrated selection.
Those were scoped component suites, not an exhaustive all-device proof.
No new tests were run for this report.

Successful RUN returns do not identify every page's private/shared/private
sequence. Control replacement, failure injection, and restricted-injection
boot remain distinct acceptance gaps.

## Full changed-file inventory

This inventory is the product/task delta from `tokqorux` to `xluknrmp`. It
excludes this newly requested report and the separate lite delta.

```text
Cargo.lock
Guide/src/reference/openvmm/management/cli.md
openvmm/hypervisor_resources/Cargo.toml
openvmm/hypervisor_resources/src/lib.rs
openvmm/membacking/Cargo.toml
openvmm/membacking/src/mapping_manager/va_mapper.rs
openvmm/membacking/src/memory_manager/mod.rs
openvmm/membacking/src/partition_mapper.rs
openvmm/openvmm_entry/src/cli_args.rs
openvmm/openvmm_entry/src/lib.rs
openvmm/openvmm_entry/src/storage_builder.rs
openvmm/openvmm_entry/src/ttrpc/mod.rs
openvmm/openvmm_helpers/src/hypervisor.rs
openvmm/openvmm_hypervisors/src/mshv.rs
openvmm/openvmm_resources/Cargo.toml
openvmm/openvmm_resources/src/lib.rs
openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto
openvmm_vhost/openvmm_vhost/src/main.rs
pending-openvmm-snp-host-access.md
petri/burette/src/tests/disk_io.rs
petri/burette/src/tests/network.rs
petri/burette/src/tests/virtio_fs.rs
petri/src/vm/openvmm/construct.rs
petri/src/vm/openvmm/modify.rs
plan-mshv-guest-memfd-lite.md
plan-openvmm-snp-bounce-io.md
review-openvmm-block-bounce.md
review-openvmm-network-vsock-bounce.md
review-openvmm-scoped-host-access.md
vm/devices/net/net_backend/src/lib.rs
vm/devices/storage/disk_backend/Cargo.toml
vm/devices/storage/disk_backend/src/lib.rs
vm/devices/storage/disk_backend/src/tests.rs
vm/devices/storage/disk_file/Cargo.toml
vm/devices/storage/disk_file/src/lib.rs
vm/devices/storage/disk_file/src/readwriteat.rs
vm/devices/storage/scsi_buffers/Cargo.toml
vm/devices/storage/scsi_buffers/src/lib.rs
vm/devices/virtio/virtio/src/common.rs
vm/devices/virtio/virtio/src/in_order.rs
vm/devices/virtio/virtio/src/queue/packed.rs
vm/devices/virtio/virtio/src/regions.rs
vm/devices/virtio/virtio_blk/Cargo.toml
vm/devices/virtio/virtio_blk/src/integration_tests.rs
vm/devices/virtio/virtio_blk/src/lib.rs
vm/devices/virtio/virtio_blk/src/resolver.rs
vm/devices/virtio/virtio_net/Cargo.toml
vm/devices/virtio/virtio_net/src/buffers.rs
vm/devices/virtio/virtio_net/src/lib.rs
vm/devices/virtio/virtio_net/src/resolver.rs
vm/devices/virtio/virtio_net/src/tests.rs
vm/devices/virtio/virtio_resources/Cargo.toml
vm/devices/virtio/virtio_resources/src/lib.rs
vm/devices/virtio/virtio_vsock/src/connections.rs
vm/devices/virtio/virtio_vsock/src/integration_tests.rs
vm/devices/virtio/virtio_vsock/src/lib.rs
vm/devices/virtio/virtio_vsock/src/resolver.rs
vm/devices/virtio/virtio_vsock/src/ring.rs
vm/devices/virtio/virtio_vsock/src/unix_relay.rs
vm/vmcore/guestmem/Cargo.toml
vm/vmcore/guestmem/src/lib.rs
vmm_core/virt/src/generic/partition_memory_map.rs
vmm_core/virt_mshv/src/aarch64/mod.rs
vmm_core/virt_mshv/src/lib.rs
vmm_core/virt_mshv/src/x86_64/mod.rs
vmm_core/virt_mshv/src/x86_64/snp.rs
vmm_core/virt_mshv/src/x86_64/snp_host_access.rs
vmm_tests/vmm_tests/tests/tests/x86_64.rs
```
