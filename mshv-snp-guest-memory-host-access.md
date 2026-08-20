# MSHV SNP Guest-Memory Host Access

## Summary

MSHV SNP needs one coordinator for guest visibility, host access, and
zero-copy software I/O. The coordinator must prevent a guest visibility
transition while a host operation holds a guest page.

Use the existing `lock_gpns` lifetime as the ownership contract. Store active
locks in a bounded sparse registry instead of allocating state for every 4 KiB
page in the VM. Keep direct device DMA unsupported until DMA mappings
participate in the same ownership model.

This design makes the following working assumption:

> The hypervisor revokes host access synchronously. A later userspace access
> to the revoked page causes a recoverable fault that OpenVMM's `trycopy`
> handling can observe and return as a memory-access failure.

This assumption lets ordinary short `GuestMemory` accesses fail without a
separate reference count or RCU drain. Operations that retain a raw pointer
across an asynchronous boundary must hold a `lock_gpns` lease. The MSHV and
hypervisor follow-up questions at the end of this document must confirm the
assumption.

This document uses these terms:

- **Guest visibility** is the guest-controlled private or shared state of a
  page.
- **Host access** is the MSHV permission that lets OpenVMM access the existing
  backing of a shared page.
- **Lease** is an RAII object that keeps host access active for a software
  operation.
- **Lease registry** is the bounded list of active leases and their GPNs.
- **DMA reference** records that a device or IOMMU can still access a page.

## Problem

MSHV SNP requires OpenVMM to coordinate guest visibility with every host and
device user of guest memory. OpenVMM must not make a page private while
userspace, an asynchronous virtio operation, or an assigned device can access
it.

The current MSHV SNP implementation supports bring-up, but it does not
coordinate these lifetimes. The fault handler can acquire host access.
The release path assumes that no virtstack component uses the affected pages:

- `vmm_core/virt/src/generic/partition_memory_map.rs:39-52`
- `vmm_core/virt_mshv/src/lib.rs:875-889`
- `vmm_core/virt_mshv/src/x86_64/snp.rs:286-335`
- `vmm_core/virt_mshv/src/x86_64/snp.rs:923-952`

The missing coordination creates races between access faults, guest visibility
transitions, asynchronous I/O completion, memory unmapping, reset, and device
teardown.

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
Regular OpenVMM membacking relies on stable mappings. It does not override
these callbacks with page-level tracking:

- `vm/vmcore/guestmem/src/lib.rs:616-685`
- `openvmm/membacking/src/mapping_manager/va_mapper.rs:853-963`

An object such as `LockedIoBuffers` provides an RAII lifetime at the device
layer. Regular membacking does not use that lifetime to prevent SNP
host-access revocation.

### What `lock_gpns` Must Guarantee

OpenVMM must reject a shared-to-private transition when a requested page has
an active lease. It does not need to preserve a guest that changes visibility
while a device owns the buffer. OpenVMM can deny the transition or stop the
guest. It must only ensure that the bad request cannot crash the host.

The current generic lock order has a race:

1. `GuestMemory::lock_gpns` or `GuestMemory::lock_range` probes each page and
   obtains its host pointer.
2. The backing records the lock through its `lock_gpns` callback.
3. The method returns the RAII object.

A visibility transition can occur between steps 1 and 2. The relevant code is
near `GuestMemory::lock_gpns`, `GuestMemory::lock_range`, and
`probe_page_for_lock` in `vm/vmcore/guestmem/src/lib.rs`.

The lock path must reserve the lease before it exposes the pointer:

1. Lock the host-access coordinator.
2. Validate and deduplicate the requested GPNs.
3. Reject a page that is not shared or is blocked for a guest transition.
4. Insert a lease record.
5. Acquire MSHV host access if the page does not already have it.
6. Probe the mapping and expose the pointer.
7. Remove the lease record if acquisition or probing fails.

This order requires a new pre-lock callback or another change to the
`GuestMemoryAccess` locking interface. The current callback runs too late.

All asynchronous zero-copy users must retain the returned RAII object until
completion or cancellation. Backends that retain a GPA or pointer without a
lease remain unsupported.

Under the working hypervisor assumption, ordinary `GuestMemory` reads and
writes do not need leases. A revoked access faults through `trycopy` and
returns an error. Fault-driven acquisition must use the same coordinator as
visibility transitions. It must fail while the coordinator has blocked the
page for a pending guest transition.

Locks do not represent DMA mappings. Keep VFIO, direct hardware DMA, DAX, and
similar paths unsupported until they use the same coordinator and can remove
their mappings synchronously.

## Underhill/OpenHCL Model

Underhill does not use one lock bit or one integer reference count per page.
It stores one boxed GPN slice for each successful lock operation in
`HardwareIsolatedMemoryProtectorInner::locked_pages`:

- `lock_gpns` appends the complete slice. It allows overlapping locks.
- `unlock_gpns` removes one matching slice.
- A visibility transition rejects a GPN if any stored slice contains it.
- `LockedPages` and `LockedRangeImpl` call `unlock_gpns` when their RAII
  lifetime ends.

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

A regular OpenVMM implementation can use a bounded vector of lease records:

```rust
struct LeaseRecord {
    id: LeaseId,
    gpns: Box<[u64]>,
}

struct LeaseRegistry {
    leases: Vec<LeaseRecord>,
    total_gpn_references: usize,
}
```

Each overlapping lease remains a separate record. Removing one lease does not
remove another lease on the same page. A visibility transition scans the
bounded records and rejects any overlap.

The registry must cap both the number of lease records and the total number
of stored GPN references. Exceeding either limit fails the lock request. This
prevents an untrusted guest from using large or fragmented descriptors to
force unbounded host allocation. The limits should be policy values based on
the maximum supported in-flight device work, not on total VM memory.

Contiguous GPNs can later use range records to reduce storage. A bounded
`Vec<Box<[u64]>>` is the simplest first implementation when the number of
shared, actively locked pages is expected to stay small.

Underhill still has the generic probe-before-lock gap described above.
OpenVMM can reuse its sparse ownership model, but it must not copy that lock
ordering.

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

## Proposed Lease Design

Implement pre-probe `lock_gpns` and `unlock_gpns` callbacks in membacking.
Every asynchronous zero-copy operation retains an RAII lease until completion
or cancellation:

```rust
let lease = guest_memory.lock_range(range, access)?;
backend.submit(request).await?;
drop(lease);
```

The coordinator contains:

- The bounded sparse lease registry.
- A sparse set or range map for pages where OpenVMM acquired host access, if
  the hypervisor contract requires OpenVMM to cache that state.
- A sparse set of pages blocked for a pending guest private transition.
- One mutex that orders lease reservation, fault-driven acquisition, and host
  access release.

A dedicated `Revoking` state is not required while the coordinator holds the
mutex across the synchronous release ioctl. The coordinator must still keep a
page blocked after release because the intercepted guest visibility hypercall
has not completed yet. Otherwise, another thread could reacquire host access
before the guest re-executes that hypercall.

Existing `LockedIoBuffers` users can retain their current outer I/O lifetime
after the generic lock order is fixed. Other zero-copy backends must retain an
equivalent lease until their actual completion event.

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
3. Scan the bounded lease registry for an overlap.
4. If a lease overlaps, deny the transition. Do not wait for device
   completion.
5. Mark the pages as blocked for the pending guest transition.
6. Release MSHV host access while holding the coordinator mutex.
7. If release fails, restore the previous coordinator state and fail the
   guest operation.
8. If release succeeds, keep the pages blocked while the VP resumes and
   re-executes its visibility hypercall.

This protocol does not drain ordinary short `GuestMemory` accesses. Under the
working assumption, an access that loses host permission faults through
`trycopy` and returns an error. Only a zero-copy operation with an exposed
pointer needs a lease.

Fault-driven acquisition uses the same coordinator mutex. It fails without
calling MSHV when a page is blocked for a pending private transition. For
other pages, an MSHV acquisition failure becomes a normal guest-memory access
failure.

The design must define when to clear the pending-transition marker. Possible
signals include the next shared-visibility intercept or an authoritative
hypervisor visibility query. The hypervisor contract must define this point.

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
removal. These operations must reject or drain active lease records before
they remove memory mappings. They must not assume that stopping a transport
drains every memory user.

## Recommended Staging

1. Add a host-access coordinator with a bounded sparse lease registry.
2. Change `GuestMemory` locking so the backing reserves a lease before pointer
   probing and exposure.
3. Route MSHV acquisition faults and guest visibility requests through the
   coordinator.
4. Audit each supported software backend. Require it to retain a lease across
   every zero-copy asynchronous operation.
5. Add tests for overlapping leases, failed lock rollback, visibility
   rejection, concurrent faults, cancellation, reset, and teardown.
6. Confirm the working trycopy-fault assumption in the hypervisor.
7. Add visibility-aware DMA mapping only after transitions can synchronously
   drain and unmap every DMA target.

Initially keep these configurations unsupported:

- Generic VFIO/iommufd assigned devices.
- Direct MANA or NVMe guest DMA.
- vhost-user and virtio-fs DAX.
- Hot memory unmap/remap during active device I/O.
- Save, migration, or reset while leases or DMA mappings are active.

## Confirmed MSHV Kernel Behavior

The MSHV partition fd serializes all partition ioctls with
`mshv_partition::pt_mutex`. Two `MSHV_MODIFY_GPA_HOST_ACCESS` ioctls for one
partition do not execute concurrently:

- `drivers/hv/mshv_root.h`, near `struct mshv_partition`
- `drivers/hv/mshv_root_main.c`, near `mshv_partition_ioctl`
  in `~/ai/leafeon/LSG-linux-rolling`

The host-access ioctl does not track OpenVMM users or leases. It converts the
guest GPA list to host PFNs and calls
`hv_call_modify_spa_host_access`:

- `drivers/hv/mshv_root_main.c`, near
  `mshv_partition_ioctl_modify_gpa_host_access`
- `drivers/hv/mshv_root_hv_call.c`, near
  `hv_call_modify_spa_host_access`

The partition mutex orders the ioctls, but it cannot know that
`LockedIoBuffers`, io_uring, or another OpenVMM component still owns a host
pointer. OpenVMM must provide that ownership policy.

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
4. Acquire fails when the guest has not made the page shared.
5. Acquire and release are state-setting operations, not reference-counted
   operations that require balanced calls.
6. The hypervisor defines ordering for overlapping acquire and release calls.
   The MSHV partition mutex serializes ioctls, but other root or hypervisor
   paths may still act on the same SPA.
7. After OpenVMM releases access for a GPA attribute intercept, a fault cannot
   reacquire the page before the guest re-executes and completes its pending
   visibility hypercall. If it can, OpenVMM must retain the sparse blocked-page
   state described above.
8. The hypervisor defines the final state after partial completion of a
   repeated acquire or release hypercall.
9. Kernel users such as io_uring receive safe failure behavior if they touch a
   page after release. If not, every such operation must remain covered by a
   lease until kernel completion.

Until this review is complete, treat the recoverable-trycopy-fault behavior as
an explicit design assumption, not a verified contract.

## Remaining Design Questions

- What error should OpenVMM return when a guest requests a private transition
  for a leased page?
- What limits should apply to lease records and total stored GPN references?
- Should the first implementation store GPNs or coalesced ranges?
- Can read-only host access work correctly, or must every lease request
  read-write access?
- What event authoritatively clears a pending guest-transition marker?
