# MSHV SNP Guest-Memory Host Access

## Summary

MSHV SNP needs one coordinator for guest visibility, host access, software
I/O, and device DMA. The coordinator must block new users before it makes a
page private. It must then drain current users, remove DMA mappings, and
release MSHV host access.

Start with bounded bounce buffers for software virtio devices. Add page leases
only after each backend has a proven completion and cancellation lifetime.
Keep direct device DMA unsupported until the coordinator can synchronously
remove every DMA mapping.

This document uses these terms:

- **Guest visibility** is the guest-controlled private or shared state of a
  page.
- **Host access** is the MSHV permission that lets OpenVMM access the existing
  backing of a shared page.
- **Lease** is an RAII object that keeps host access active for a software
  operation.
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

### Why `lock_gpns` Alone Is Insufficient

OpenVMM must implement `lock_gpns` and reject a shared-to-private transition
when a requested page is locked. This change alone does not close every
host-access race.

First, locks protect only callers that use the locking API. Ordinary
`GuestMemory` reads and writes, fault-driven host-access acquisition, and
device DMA mappings do not automatically create a GPN lock. A transition
can observe no locks in these cases:

- A CPU thread is executing a short guest-memory copy.
- A host-access fault is about to reacquire the page.
- A network or storage backend retained a GPA without using
  `LockedIoBuffers`.
- VFIO or another direct-DMA backend still has the page mapped in an IOMMU.

Second, a check followed by release has a time-of-check/time-of-use race:

1. The transition checks that the pages are unlocked.
2. A new I/O operation locks or faults one of the pages.
3. The transition releases MSHV host access.
4. The new operation accesses a page that is becoming private.

The transition must first mark the pages `Revoking`. Both `lock_gpns` and the
host-access fault path must reject pages in that state. Only after new users
are excluded is it meaningful to check the existing lock count.

Third, the coordinator must drain short accesses that do not retain a lock.
Clearing the guest-memory access bitmap prevents new accesses. An access that
passed its bitmap check can still be active. The transition must wait for the
guest-memory RCU read-side domain before it releases host access. Underhill
combines visibility bitmaps and RCU synchronization with locked-GPN tracking:

- `vm/vmcore/guestmem/src/lib.rs:1715-1789`
- `openhcl/underhill_mem/src/lib.rs:535-655`

Finally, locks do not represent DMA mappings. VFIO and other direct-DMA users
need separate DMA reference tracking. The coordinator must remove their IOMMU
mappings before it makes a page private. An indefinite lock can represent an
IOMMU mapping in a common policy interface. The mapper still needs explicit
synchronous unmap and failure handling.

A minimal safe software-virtio policy can use `lock_gpns`:

1. Require every asynchronous guest-buffer user to retain an RAII GPN lock
   until completion or cancellation.
2. Mark a transition range `Revoking` before checking its locks.
3. Block new locks and host-access faults for revoking pages.
4. Clear access bitmap entries and drain existing RCU readers.
5. Reject the transition as busy if any page remains locked.
6. Release MSHV host access only after all of those checks succeed.

The minimal policy supports only audited software devices. Keep VFIO, direct
hardware DMA, DAX, and backends that retain unlocked GPAs unsupported. Enable
them only after they report their lifetimes to the same coordinator.

## Underhill/OpenHCL Model

Underhill already implements most of the required synchronization model:

- Visibility bitmaps gate guest-memory access.
- Lock and unlock callbacks record and remove locked GPNs.
- Visibility transitions reject locked pages.
- The transition clears access bitmaps before it changes visibility.
- The transition drains existing accesses through the guest-memory RCU domain.

Relevant implementation:

- `openhcl/underhill_mem/src/mapping.rs:100-260`
- `openhcl/underhill_mem/src/lib.rs:535-655`
- `openhcl/underhill_mem/src/lib.rs:1108-1143`

Adapting this model to regular OpenVMM is the most direct route to safe MSHV
SNP host-access transitions.

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

## Design Options

### 1. Bounce-buffer-only software virtio

Copy request data into bounded host buffers before asynchronous I/O. For
reads, put host I/O data in a host buffer. Acquire guest access only while
copying the result to guest memory.

Advantages:

- Smallest trust boundary.
- Simple cancellation and teardown.
- No guest page remains referenced while host I/O is pending.
- Existing block and network implementations provide useful examples.

Disadvantages:

- Additional copies.
- Buffer allocations must be strictly capped against malicious descriptor
  sizes.
- Does not support direct device DMA, DAX, or other zero-copy paths.

This is the safest first supported mode for virtio-blk and copy-based
networking.

### 2. Page-level host-access leases

Implement real `lock_gpns` and `unlock_gpns` callbacks in membacking. Every
asynchronous operation retains an RAII lease for its guest pages until
completion or cancellation.

A possible page state is:

```rust
enum PageState {
    Private,
    Acquiring,
    Shared {
        host_refs: u32,
        dma_refs: u32,
    },
    Revoking,
}
```

An operation must acquire and retain a lease:

```rust
let lease = guest_memory.lock_range(range, access)?;
backend.submit(request).await?;
drop(lease);
```

The lock path must:

1. Validate, page-align, and deduplicate the requested GPNs.
2. Reject pages in `Revoking`.
3. Acquire MSHV host access for private pages.
4. Enable the appropriate guest-memory access bitmap.
5. Increment each page's host reference count.
6. Return an RAII object that decrements the count on drop.

Existing `LockedIoBuffers` users can gain these semantics without changing
their outer I/O lifetime. Other zero-copy backends must retain an equivalent
lease until their actual completion event.

Advantages:

- Builds on existing `GuestMemory` APIs.
- Closely follows the proven Underhill design.
- Allows selected zero-copy paths.

Disadvantages:

- Requires auditing every backend that retains GPAs or guest buffers.
- Cancellation, reset, save, unmap, and teardown must all drain leases.
- Needs a serialized per-page state machine shared with memory faults and
  guest visibility requests.

### 3. Permanently shared I/O aperture

Reserve a guest region that remains shared and use it for queue data, bounce
buffers, and device DMA.

Advantages:

- Simple visibility and lifetime rules.
- Can provide a controlled DMA region for assigned devices.

Disadvantages:

- Requires guest cooperation.
- Reduces flexibility and private memory.
- Usually requires copies between private guest memory and the aperture.

### 4. Visibility-aware VFIO mappings

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

1. Validate the complete range without changing state.
2. Mark every page `Revoking`, blocking new leases and fault-driven
   acquisition.
3. Clear guest-memory access bitmap entries.
4. Drain the guest-memory RCU domain so accesses that began before the bitmap
   change have completed.
5. Check host and DMA reference counts.
6. If references remain, cancel revocation and report a retryable busy status.
7. Remove all DMA mappings.
8. Release MSHV host access.
9. Mark the pages `Private`.

Initially, return a retryable busy result instead of blocking a VP while a
device completes an operation. Underhill uses a similar policy when a
transition includes locked pages.

Host-access faults must use the same coordinator. A fault must not invoke MSHV
while a page is `Revoking`. Otherwise, the fault can reacquire access during
the release sequence.

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
removal. It must not assume that stopping a transport drains every memory
user.

## Recommended Staging

1. Add a shared page-state coordinator to membacking.
2. Port Underhill-style visibility bitmaps, GPN lock tracking, and RCU
   draining to regular OpenVMM.
3. Route MSHV acquisition faults and guest visibility requests through that
   coordinator.
4. Enable bounded bounce-buffer virtio-blk and copy-based networking first.
5. Add leases to selected zero-copy software backends after their completion
   and cancellation lifetimes are audited.
6. Add visibility-aware DMA mapping only after transitions can synchronously
   drain and unmap every DMA target.

Initially keep these configurations unsupported:

- Generic VFIO/iommufd assigned devices.
- Direct MANA or NVMe guest DMA.
- vhost-user and virtio-fs DAX.
- Hot memory unmap/remap during active device I/O.
- Save, migration, or reset while page leases or DMA mappings are active.

## Open Questions

- What retryable status should the MSHV GPA visibility protocol return for a
  page that is still in use?
- Can read-only host access be made reliable, or must every lease request
  read-write access?
- Should leases be page-granular or coalesced into ranges internally?
- How should reference counts interact with large-page visibility requests?
- Which existing virtio backends can be proven copy-only without additional
  leases?
- Can a permanently shared aperture provide an acceptable first VFIO model,
  or should assigned devices remain unsupported until dynamic DMA mapping is
  complete?
