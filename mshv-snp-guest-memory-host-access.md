# MSHV SNP Guest-Memory Host Access

## Summary

MSHV SNP needs one coordinator for guest visibility, host access, and
zero-copy software I/O. The coordinator must prevent a guest visibility
transition while a host operation holds a guest page.

Use the existing `lock_gpns` lifetime as the ownership contract. Store active
host-access locks in a bounded sparse registry instead of allocating state for
every 4 KiB page in the VM. Keep direct device DMA unsupported until DMA
mappings participate in the same ownership model.

This design makes the following working assumption:

> The hypervisor revokes host access synchronously. A later userspace access
> to the revoked page causes a recoverable fault that OpenVMM's `trycopy`
> handling can observe and return as a memory-access failure.

This assumption lets ordinary short `GuestMemory` accesses fail without a
separate reference count or RCU drain. Operations that retain a raw pointer
across an asynchronous boundary must hold a `lock_gpns` backing lock. The MSHV
and hypervisor follow-up questions at the end of this document must confirm
the assumption.

This document uses these terms:

- **Guest visibility** is the guest-controlled private or shared state of a
  page.
- **Host access** is the MSHV permission that lets OpenVMM access the existing
  backing of a shared page.
- **Host-access lock** is an RAII object that keeps host access active for a
  software operation.
- **Host-access lock registry** is the bounded map of active locks and their
  GPNs.
- **DMA reference** records that a device or IOMMU can still access a page.

## Problem

MSHV SNP requires OpenVMM to coordinate guest visibility with every host and
device user of guest memory. OpenVMM must not make a page private while
userspace, an asynchronous virtio operation, or an assigned device can access
it.

Without lifetime coordination, host-access faults can race guest visibility
transitions, asynchronous I/O completion, memory unmapping, reset, and device
teardown.

## Current Implementation Status

The MSHV SNP host-access coordinator tracks zero-copy software users,
serializes fault-driven acquisition with guest visibility transitions, and
rejects a private transition while a host-access lock overlaps the range:

- `vmm_core/virt/src/generic/partition_memory_map.rs`, near
  `PartitionHostAccess`
- `vmm_core/virt_mshv/src/lib.rs`, in the `PartitionHostAccess`
  implementation for `MshvPartitionInner`
- `vmm_core/virt_mshv/src/x86_64/snp.rs`, near `SnpHostAccessState`,
  `lock_snp_host_access`, and `handle_snp_gpa_attribute_intercept`

Direct DMA users do not participate in this coordinator. They remain
unsupported until their mapping lifetimes can be tracked and drained.

## Current Virtio Buffer Lifetimes

Virtio queues do not pin payload pages for the lifetime of a request. They
copy descriptor GPA and length metadata into owned work items:

- `vm/devices/virtio/virtio/src/queue.rs:417-473`
- `vm/devices/virtio/virtio/src/common.rs:95-150`

The actual guest-memory lifetime is backend-specific:

- `disk_file` copies write data into host-owned memory before awaiting file
  I/O, and copies read data back after the host operation completes:
  `vm/devices/storage/disk_file/src/lib.rs:94-127`.
- The block-device io_uring path retains `LockedIoBuffers` across the
  asynchronous operation:
  `vm/devices/storage/disk_blockdevice/src/lib.rs:559-645`.
- Virtio-net RX can retain the work item until the backend returns its RX ID:
  `vm/devices/virtio/virtio_net/src/buffers.rs:84-124`.
- Direct hardware networking can retain programmed guest IOVAs until hardware
  completion:
  `vm/devices/net/net_mana/src/lib.rs:1210-1390`.

Virtio queue accounting tracks descriptor heads and completion IDs, not the
guest pages referenced by each operation:

- `vm/devices/virtio/virtio/src/queue.rs:129-156`
- `vm/devices/virtio/virtio/src/queue.rs:341-367`

No central component can identify all guest pages that virtio currently uses.

## Existing Guest-Memory Locking

`GuestMemory` already exposes range and GPN locking with RAII unlock:

- `vm/vmcore/guestmem/src/lib.rs:2078-2131`
- `vm/vmcore/guestmem/src/lib.rs:2303-2485`

The default `lock_gpns` implementation does not record page references.
Regular OpenVMM membacking now forwards the callback to the partition
host-access coordinator when one is installed:

- `vm/vmcore/guestmem/src/lib.rs:616-685`
- `openvmm/membacking/src/mapping_manager/va_mapper.rs:853-963`

An object such as `LockedIoBuffers` owns both its device-layer buffers and the
backing lock returned by the coordinator. Dropping it releases the MSHV SNP
host-access lock.

### What `lock_gpns` Must Guarantee

OpenVMM must reject a shared-to-private transition when a requested page has
an active host-access lock. It does not need to preserve a guest that changes
visibility while a device owns the buffer. OpenVMM can deny the transition or
stop the guest. It must only ensure that the bad request cannot crash the host.

The original generic lock order had a race:

1. `GuestMemory::lock_gpns` or `GuestMemory::lock_range` probes each page and
   obtains its host pointer.
2. The backing records the lock through its `lock_gpns` callback.
3. The method returns the RAII object.

A visibility transition can occur between steps 1 and 2. The relevant code is
near `GuestMemory::lock_gpns`, `GuestMemory::lock_range`, and
`probe_page_for_lock` in `vm/vmcore/guestmem/src/lib.rs`.

The lock path must first reserve the host-access lock under the coordinator
mutex:

1. Lock the host-access coordinator.
2. Let `GuestMemory` validate the requested GPNs before it calls the backing.
3. Insert a host-access lock record that preserves duplicate GPNs.
4. Deduplicate the GPNs used for the MSHV acquire ioctl.
5. Acquire MSHV host access for GPNs that are not in the bounded access cache.
6. Release the coordinator mutex.
7. Probe the mapping and expose the pointer.
8. Remove the host-access lock record if acquisition or probing fails.

`GuestMemoryAccess::lock_gpns` now returns an owned backing lock. `GuestMemory`
stores that lock in `LockedPages` or `LockedRangeImpl`. A failed probe drops
the lock automatically before an address is exposed.

All asynchronous zero-copy users must retain the returned RAII object until
completion or cancellation. Backends that retain a GPA or pointer without a
host-access lock remain unsupported.

Under the working hypervisor assumption, ordinary `GuestMemory` reads and
writes do not need host-access locks. A revoked access faults through `trycopy`
and returns an error. Fault-driven acquisition must use the same coordinator
as visibility transitions. MSHV must fail acquisition when the guest has not
made the page shared.

Locks do not represent DMA mappings. Keep VFIO, direct hardware DMA, DAX, and
similar paths unsupported until they use the same coordinator and can remove
their mappings synchronously.

## Underhill/OpenHCL Model

Underhill does not use one lock bit or one integer reference count per page.
It stores one boxed GPN slice for each successful lock operation in
`HardwareIsolatedMemoryProtectorInner::locked_pages`:

- `lock_gpns` appends the complete slice. It allows overlapping locks.
- Dropping the backing lock removes one matching slice.
- A visibility transition rejects a GPN if any stored slice contains it.
- `LockedPages` and `LockedRangeImpl` own the backing lock for their RAII
  lifetime.

Relevant implementation:

- `openhcl/underhill_mem/src/lib.rs`, near
  `HardwareIsolatedMemoryProtectorInner`, `check_gpn_not_locked`,
  `lock_gpns`, and `unlock_gpns`
- `openhcl/underhill_mem/src/mapping.rs`, in the `GuestMemoryAccess`
  implementation for `GuestMemoryView`
- `vm/vmcore/guestmem/src/lib.rs`, near `LockedPages` and `LockedRangeImpl`

The `valid_shared` and `valid_encrypted` bitmaps have a separate purpose. They
gate ordinary mapped accesses. They do not record lock ownership.

Underhill's sparse representation fits the expected OpenVMM workload better
than a dense `u32` array. A dense counter costs 1 MiB for each GiB of guest
RAM, even when no page is locked. A 1 TiB VM would require 1 GiB of counters.

A regular OpenVMM implementation can use bounded sparse maps:

```rust
struct HostAccessLockRegistry {
    distinct_lock_sets: BTreeMap<Box<[u64]>, usize>,
    locked_gpn_count: BTreeMap<u64, usize>,
    total_gpn_references: usize,
}
```

Identical lock records share a counted map entry. A separate sparse per-GPN
reference map records overlap across different locks. Removing one lock does
not remove another lock on the same page.

The registry does not cap the number of host-access lock objects directly. It
caps the GPNs in one lock and the total number of stored GPN references. The
total reference limit also bounds the lock count because each lock contains at
least one GPN. Exceeding a GPN limit fails the lock request. These limits
prevent an untrusted guest from using large or fragmented descriptors to force
unbounded host allocation. The total GPN-reference limit is the sole tuning
control for the indirect lock-count ceiling and should reflect the maximum
supported in-flight device work.

The implementation uses bounded sparse maps because the number of shared,
actively locked pages is expected to stay small.

The shared `GuestMemory` implementation now reserves the backing lock before
it probes the mapping, so Underhill uses the same corrected ordering.

## VFIO and DMA

OpenVMM already tracks the lifetime and ordering of structural DMA mappings:

- `openvmm/membacking/src/region_manager.rs:45-177`
- `openvmm/membacking/src/region_manager.rs:408-484`
- `openvmm/membacking/src/region_manager.rs:853-889`

Current VFIO mappings do not support arbitrary SNP private/shared transitions:

- VFIO type1 maps ranges by host VA, allowing the kernel to pin pages for the
  lifetime of the IOMMU mapping:
  `vm/devices/pci/vfio_assigned_device/src/manager.rs:31-69`.
- iommufd maps RAM by backing file where possible and otherwise by host VA:
  `vm/devices/pci/vfio_assigned_device/src/manager.rs:557-653`.
- Current mappings cover active RAM ranges rather than following individual
  SNP shared-page transitions.

Structural teardown removes DMA mappings before memory mappings. No visibility
event synchronously removes a page from every DMA target before MSHV makes it
private.

## Host-Access Lock Design

The pre-probe `lock_gpns` callback returns an owned backing lock.
Every asynchronous zero-copy operation retains that RAII lock until completion
or cancellation:

```rust
let lock = guest_memory.lock_range(range, access)?;
backend.submit(request).await?;
drop(lock);
```

The coordinator contains:

- The bounded sparse host-access lock registry.
- A sparse per-GPN lock reference index for fast overlap checks.
- A bounded sparse cache of GPNs where OpenVMM acquired host access.
- One mutex that orders lock reservation, fault-driven acquisition, and host
  access release.

The first cache implementation does not evict entries. After it reaches its
limit, uncached GPNs continue to work but require an MSHV acquire ioctl for
each zero-copy lock.

A persistent `Revoking` or blocked-page state is not required. The coordinator
holds its mutex across the synchronous release ioctl and invalidates the
acquired-GPN cache before release. The guest's visibility hypercall then
re-executes when the VP resumes.

Another thread can reacquire access before that re-execution. The hypervisor
then intercepts the guest hypercall again instead of completing the private
transition. A zero-copy operation has a host-access lock, so OpenVMM denies
the second transition. A short access relies on the accepted `trycopy` fault
assumption if a later release wins the race.

Existing `LockedIoBuffers` users can retain their current outer I/O lifetime
after the generic lock order is fixed. Other zero-copy backends must retain an
equivalent host-access lock until their actual completion event.

### Deferred visibility-aware VFIO mappings

Map only shared pages into each VFIO IOAS and synchronously remove those
mappings before completing a private transition.

Visibility-aware VFIO mappings require:

- Per-page or coalesced-range DMA reference tracking.
- Device quiescing before unmap.
- Synchronous IOMMU invalidation.
- Rollback if any target fails to unmap.
- Correct handling of reset, hot-unplug, and process teardown.
- A policy for ATS-capable devices and device-side translation caches.

This option provides the best direct-DMA performance. It also has the highest
correctness and security risk.

## Proposed Transition Protocol

When the guest requests that pages become private:

1. Validate the complete range.
2. Lock the host-access coordinator.
3. Scan the bounded host-access lock registry for an overlap.
4. If a host-access lock overlaps, deny the transition. Do not wait for device
   completion.
5. Invalidate the acquired-GPN cache for the full transition range.
6. Release MSHV host access while holding the coordinator mutex.
7. If release fails, fail the guest operation. MSHV does not report which
   prefix a repeated hypercall processed, so keep the cache invalidated.
8. Resume the VP. The guest re-executes its visibility hypercall.

This protocol does not drain ordinary short `GuestMemory` accesses. Under the
working assumption, an access that loses host permission faults through
`trycopy` and returns an error. Only a zero-copy operation with an exposed
pointer needs a host-access lock.

Fault-driven acquisition uses the same coordinator mutex and always calls
MSHV. An MSHV acquisition failure becomes a normal guest-memory access
failure. Zero-copy lock acquisition can use the acquired-GPN cache because
`GuestMemory` probes every locked page before it exposes the page's address.

MSHV can complete only part of a repeated host-access hypercall before it
returns an error. The current kernel reports the failure but does not return
the completed count to userspace. Until MSHV provides an atomic operation,
completed-count output, or an authoritative query, OpenVMM must use a batch
size and failure policy that cannot silently treat a mixed result as success.

## Device Teardown

Some device paths already provide useful draining behavior:

- Virtio transport stop disables and stops each queue:
  `vm/devices/virtio/virtio/src/transport/task.rs:220-282`.
- Virtio-blk drains pending disk futures before dropping its queue:
  `vm/devices/virtio/virtio_blk/src/lib.rs:389-421`.

This is not universal:

- Virtio-net queue save/restore remains incomplete:
  `vm/devices/virtio/virtio_net/src/lib.rs:411-439`.
- VFIO save/restore is unsupported:
  `vm/devices/pci/vfio_assigned_device/src/lib.rs:1571-1582`.

The host-access coordinator must participate in reset, unmap, save, and device
removal. These operations must reject or drain active host-access locks before
they remove memory mappings. They must not assume that stopping a transport
drains every memory user.

## Remaining Staging

The current implementation has a bounded sparse lock registry, reserves
backing locks before pointer exposure, and routes MSHV faults and guest
visibility requests through one coordinator.

1. Audit each supported software backend. Require it to retain a host-access
   lock across every zero-copy asynchronous operation.
2. Add tests for overlapping locks, failed lock rollback, visibility
   rejection, concurrent faults, cancellation, reset, and teardown.
3. Confirm the working trycopy-fault assumption in the hypervisor.
4. Add visibility-aware DMA mapping only after transitions can synchronously
   drain and unmap every DMA target.

Initially keep these configurations unsupported:

- Generic VFIO/iommufd assigned devices.
- Direct MANA or NVMe guest DMA.
- vhost-user and virtio-fs DAX.
- Hot memory unmap/remap during active device I/O.
- Save, migration, or reset while host-access locks or DMA mappings are active.

## Confirmed MSHV Kernel Behavior

The MSHV partition fd serializes all partition ioctls with
`mshv_partition::pt_mutex`. Two `MSHV_MODIFY_GPA_HOST_ACCESS` ioctls for one
partition do not execute concurrently:

- `drivers/hv/mshv_root.h`, near `struct mshv_partition`
- `drivers/hv/mshv_root_main.c`, near `mshv_partition_ioctl`

The host-access ioctl does not track OpenVMM users or host-access locks. It
converts the guest GPA list to host PFNs and calls
`hv_call_modify_spa_host_access`:

- `drivers/hv/mshv_root_main.c`, near
  `mshv_partition_ioctl_modify_gpa_host_access`
- `drivers/hv/mshv_root_hv_call.c`, near
  `hv_call_modify_spa_host_access`

The partition mutex orders the ioctls, but it cannot know that
`LockedIoBuffers`, io_uring, or another OpenVMM component still owns a host
pointer. OpenVMM must provide that ownership policy.

## Confirmed Hypervisor Behavior

The Microsoft Hypervisor implementation confirms these protocol details:

- `HvModifySparseGpaPageHostVisibility` sets `Adjust=TRUE` on the GPA
  attribute intercept.
- Private-to-shared guest transitions normally complete without an intercept.
  OpenVMM learns whether a page is shared by attempting
  `HvCallAcquireSparseSpaPageHostAccess`.
- Acquire verifies that requested current host access does not exceed the
  page's maximum host access.
- A private transition that conflicts with acquired host access suspends the
  VP and sends the GPA attribute intercept. After OpenVMM releases access and
  resumes the VP, the guest hypercall re-executes.
- If another host user reacquires access before re-execution, the hypervisor
  intercepts the private transition again.
- Repeated host-access hypercalls stop at the first failure after processing a
  prefix.

These findings were confirmed against the Microsoft Hypervisor
implementation.

## Hypervisor Follow-up

Review the Microsoft Hypervisor implementation of
`HVCALL_ACQUIRE_SPARSE_SPA_PAGE_HOST_ACCESS` and
`HVCALL_RELEASE_SPARSE_SPA_PAGE_HOST_ACCESS`. Confirm these points:

1. Release completes synchronously and invalidates root mappings and TLB
   entries before it returns.
2. A userspace load or store after release causes a recoverable fault that
   `trycopy` reports as an access failure. It must not cause a host bugcheck or
   expose memory after ownership changes.
3. An access already executing during release either completes safely or
   faults safely.
4. Acquire and release are state-setting operations, not reference-counted
   operations that require balanced calls.
5. The hypervisor defines ordering for overlapping acquire and release calls.
   The MSHV partition mutex serializes ioctls, but other root or hypervisor
   paths may still act on the same SPA.
6. The hypervisor defines the final state after partial completion of a
   repeated acquire or release hypercall.
7. Kernel users such as io_uring receive safe failure behavior if they touch a
   page after release. If not, every such operation must remain covered by a
   host-access lock until kernel completion.

Until this review is complete, treat the recoverable-trycopy-fault behavior as
an explicit design assumption, not a verified contract.

## Remaining Design Questions

- What error should OpenVMM return when a guest requests a private transition
  for a locked page?
- Can read-only host access work correctly, or must every lock request
  read-write access?
