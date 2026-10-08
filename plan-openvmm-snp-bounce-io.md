# Plan: copy-only guest I/O for OpenVMM MSHV SNP

Status: implementation plan, not an implemented or validated isolation boundary.
Research date: 2026-10-07. Sources below refer to the current working tree, including the user's SNP changes. Line numbers will move as those changes develop.
Revision: 2026-10-08 user correction incorporated and reviewed. Review reached **Minor revisions**; those corrections are incorporated. Scoped acquire/copy/release uses existing host-access calls, not a new hypervisor interface. Lite MM enforcement follows separately. The first implementation uses only 4 KiB pages; large pages are deferred.

Companion: [MSHV kernel backing and isolation plan](plan-mshv-guest-memfd-lite.md).

## 1. Goal and direct answer

The end state uses **one guest backing**, `guest_memfd_lite`, for `virt_mshv` SNP. OpenVMM touches it only through short, synchronous host-access windows when the verified guest-ownership/root-grant contract permits access. All supported device payload I/O uses OpenVMM-owned memory. No guest backing address, backing file descriptor, or guest page IOVA reaches a payload backend, kernel payload I/O request, or physical device. AP VMSA and doorbell registration are separately gated control-page exceptions.

**Implement this in two phases, with separate phase-1a and phase-1b gates:**

| Phase | Deliverable | Kernel dependency |
|---|---|---|
| **1a. Bounce I/O first** | Userspace copy-only I/O policy, bounded owned payloads, backend lifetime/error handling, and a tested device/backend allowlist. | **No kernel changes or new capability required.** Keep current RAM registration, acquisition-only host access, launch/control paths, and fail-closed attribute intercepts. |
| **1b. Scoped guest copies** | Acquire host access, copy synchronously, release; deny conflicting guest attribute exits through the existing completion protocol when established. | Uses the **existing** modify-host-access ioctl and Hyper-V acquire/release calls. Independent of lite-MM capability. |
| **2. Enforced private-memory isolation** | New lite backing, Linux mapping revocation, MM no-pin/no-PFN-export enforcement, ownership/control lifecycles, and verified reclaim. Reuse phases 1a/1b. | Requires companion kernel backing/MM work; no new Hyper-V operation or copy-window token ioctl. |

Phase 1a removes actual guest buffers from supported **device payload submissions**, not MSHV's RAM registration pins or all hypervisor control operations. It does not shorten current host-access grants, revoke Linux mappings, enable guest transitions, or establish a security boundary. Phase 1b adds balanced host access without lite MM enforcement. Neither is `guest_memfd_lite`; phase-two gates must not block phase 1a.

**OpenVMM already has useful bounce paths, but no universal bounce-only device mode and no scoped MSHV host-access release.**

* `virtio_blk::do_io` already coalesces scattered descriptors into an owned, page-aligned `GuestMemory` allocation. It does so only when the scatter list cannot form one `PagedRange`; compatible guest layouts still reach the disk directly. Both directions have an integration round-trip test. Evidence: `vm/devices/virtio/virtio_blk/src/lib.rs:657-754`; `vm/devices/virtio/virtio_blk/src/integration_tests.rs:1181-1210,1359-1363`.
* Linux `disk_blockdevice` has `always_bounce`, sector-alignment fallback, and `!supports_locking()` fallback. It uses `scsi_buffers::BounceBuffer`, with an optional per-thread page budget. This is a backend-specific control, not a VM-wide guarantee. The static resolver explicitly supplies no tracker and `always_bounce=false`. Evidence: `vm/devices/storage/disk_blockdevice/src/lib.rs:203-210,565-674`; `vm/devices/storage/disk_blockdevice/src/resolver.rs:76-107`; `vm/devices/storage/scsi_buffers/src/lib.rs:169-223,502-557`.
* `virtio_vsock` already falls back to temporary buffers when locking is unavailable or the payload cannot form a `PagedRange`. Its fast paths still pass guest slices to socket I/O. Evidence: `vm/devices/virtio/virtio_vsock/src/lib.rs:335-396,598-639`; `vm/devices/virtio/virtio_vsock/src/connections.rs:453-490`.
* Network backends have several copying paths. TAP linearizes TX, DIO copies TX, and Consomme copies TX into scratch memory. MANA has an explicit `GuestDmaMode::BounceBuffer`, with a test. However, the common API still exposes guest memory and guest RX addresses, and Consomme can retain guest TX segment addresses for later processing. Evidence: `vm/devices/net/net_backend/src/lib.rs:186-303`; `vm/devices/net/net_tap/src/lib.rs:306-318`; `vm/devices/net/net_dio/src/lib.rs:173-195`; `vm/devices/net/net_consomme/src/lib.rs:682-725`; `vm/devices/net/net_mana/src/lib.rs:118-142,708-741,1252-1278`; `vm/devices/net/net_mana/src/test.rs:109-123`.
* Console, 9p, RNG, and file-backed disk I/O already use host buffers for their principal data paths. They still need the scoped guest-copy boundary, allocation limits, and failure handling. In-process virtio-fs uses streaming guest readers/writers during filesystem dispatch; it is not a whole-request bounce boundary. Evidence: `vm/devices/virtio/virtio_console/src/lib.rs:298-375`; `vm/devices/virtio/virtio_p9/src/lib.rs:197-228`; `vm/devices/virtio/virtio_rng/src/lib.rs:168-187`; `vm/devices/storage/disk_file/src/lib.rs:102-136`; `vm/devices/virtio/virtiofs/src/virtio.rs:307-380`.

**Simply setting `always_bounce`, or overriding `VaMapper::supports_locking`, is insufficient.** The current `Arc<T>` guest-memory adapter does not forward `supports_locking`, and default subrange adapters recover a mapping and inherit the default mapping-based answer. A copy-only policy must propagate through all adapters and all export paths. Evidence: `vm/vmcore/guestmem/src/lib.rs:484-498,649-727,762-890,891-906,1228-1253`; real `Arc<VaMapper>` construction: `openvmm/membacking/src/memory_manager/mod.rs:718-723`.

### Non-goals

* No dual-memfd private/shared guest RAM design. Bounce allocations are I/O scratch memory, not a second guest RAM image.
* No new Hyper-V operation or BEGIN/END copy-window UAPI. Existing host-access flags support acquisition and release. New lite backing registration/query and MM enforcement may be needed in phase two. Guest ownership and temporary root grants are distinct: never force a SHARED/PRIVATE transition for each copy, or assume a caller must first make the page shared. Copy only after Hyper-V authorizes the existing access request; this is not a claim of arbitrary private plaintext access.
* Base 4 KiB pages only in the first implementation. Owned huge folios, large guest mappings/grants, mixed 2 MiB state, and demotion are follow-up work. Do not change unrelated legacy guests' superpage flags.
* Do not rewrite OpenHCL memory protection or non-SNP zero-copy paths.
* Do not promise transparent support for VFIO passthrough, kernel vhost, vhost-user, or shared-memory/DAX devices. Reject incompatible configurations until a separate implementation proves them safe.
* This task changes only this plan. Do not edit or revert the existing user changes in the Guide or SNP source files. No commit, push, or remote operation is needed.

## 2. Evidence-backed current architecture

### 2.1 MSHV access is fault-driven and acquisition-only

1. `worker/dispatch.rs` passes `partition.host_access()` when attaching VTL0 memory; VTL2 attachment passes `None`. `MshvPartition::host_access` returns the partition inner object only for x86_64 SNP. Sources: `openvmm/openvmm_core/src/worker/dispatch.rs:1551-1571`; `vmm_core/virt_mshv/src/lib.rs:910-922`.
2. `GuestMemoryManager::attach_partition` installs that callback on the primary `VaMapper`. The primary is eager and is the process-local cached mapper used by later `GuestMemoryClient::guest_memory` calls. There is not a separate device-copy permission lifetime today. Sources: `openvmm/membacking/src/memory_manager/mod.rs:799-825,718-723`; `openvmm/membacking/src/mapping_manager/manager.rs:45-80,164-193`.
3. `VaMapper::mapping` returns its reserved VA. On an eager-mapper fault, `page_fault` rounds the failing byte range to hypervisor pages, calls `acquire_host_access`, then returns `Retry`. There is no matching release in this implementation. Sources: `openvmm/membacking/src/mapping_manager/va_mapper.rs:862-959`; trait contract: `vmm_core/virt/src/generic/partition_memory_map.rs:65-77`.
4. `MshvPartitionInner::acquire_host_access` ignores the requested read/write direction. `acquire_snp_host_access` sends the existing modify-host-access ioctl with acquire, readable, and writable bits, plus a vector of page-aligned GPAs. It checks nonempty page alignment, but has no scoped token, release, complete mapped-range validation, or coordination state. Sources: `vmm_core/virt_mshv/src/lib.rs:925-940`; `vmm_core/virt_mshv/src/x86_64/snp.rs:422-457`.
5. GPA attribute intercepts currently fail closed by halting the VP with `TripleFault`; they do not revoke host access or drain copies. The current test checks rejection across adjust, memory type, visibility, and range-count combinations. Sources: `vmm_core/virt_mshv/src/x86_64/snp.rs:1200-1231,2457-2481`.
6. Runtime SNP guest requests pass request/response **guest GPAs** directly to `psp_issue_guest_request`; parsing and RAM validation do not bounce those payloads. AP creation passes a VMSA GPA to `sev_snp_ap_create`, and runtime doorbell registration writes `SevDoorbellGpa`. These handoffs bypass a generic guestmem payload-copy hook. Sources: `vmm_core/virt_mshv/src/x86_64/snp.rs:1663-1724,1727-1838,1605-1637`. In contrast, the `SnpVpState` GHCB mapping comes from the VP fd's kernel GHCB-state mmap offset, not from guest RAM; its wrapper unmaps it on drop (`vmm_core/virt_mshv/src/x86_64/snp.rs:195-241`).

Thus, a copy through today's `read_at`/`write_at` may acquire access, but does **not** release it when the copy ends. Short Rust borrows do not imply short hypervisor access.

### 2.2 GuestMemory, locking, and exports

* Normal read/write/fill/plain/compare-exchange operations use `run_on_mapping`, mapping-range validation, fault-contained `trycopy`, and fallback dispatch. Fault resolution can retry indefinitely; bitmap-enabled builds wrap the operation in RCU. This prevents some invalid VA faults from escaping but is not a host-access lifecycle. Sources: `vm/vmcore/guestmem/src/lib.rs:1505-1627,1631-1655,1718-1738,1747-1870`.
* `GuestMemoryAccess` distinguishes a stable VA reservation from currently accessible pages. Optional access bitmaps and RCU synchronization can restrict mapped access. The trait also exposes `expose_va`, `base_iova`, `lock_gpns`/`unlock_gpns`, and file sharing. Default `lock_gpns` returns `false`, meaning there is no unlock callback. Sources: `vm/vmcore/guestmem/src/lib.rs:303-338,436-498`.
* `lock_gpns` and `lock_range` probe mapped pages, optionally prepare kernel access, construct guest pointers/slices, and call backing locking. `LockedPages` and `LockedRangeImpl` call backing `unlock_gpns` only if the backing requested it. They are **not** MSHV host-access guards. `VaMapper` uses the default locking callbacks. Sources: `vm/vmcore/guestmem/src/lib.rs:1872-1910,1956-1980,2142-2170,2222-2236,2280-2301`; `openvmm/membacking/src/mapping_manager/va_mapper.rs:862-971`.
* `RequestBuffers::lock` creates guest `IoBuffer` vectors for asynchronous I/O. `reader`/`writer` copy via paged-memory helpers; the direction flag forbids backend writes to read-only request buffers, but readers remain available. `PagedRange` is one logical sequence of pages with only first/last partial pages, not arbitrary byte scatter-gather. Sources: `vm/devices/storage/scsi_buffers/src/lib.rs:310-412`; `vm/vmcore/guestmem/src/ranges.rs:113-140,193-224`.
* Raw/export surfaces are distinct: `full_mapping`, `iova`, `sharing`, locked slices, and the region manager's direct `DmaTarget` mapping. `supports_locking=false` alone does not disable these. Sources: `vm/vmcore/guestmem/src/lib.rs:1463-1499`; `openvmm/membacking/src/region_manager.rs:82-92,134-159,410-445`.
* `VaMapper::sharing` currently blocks sharing when mappings use anonymous/private backing. That is not an SNP-private/shared page-state check: file-backed guest RAM can still return a sharing provider. Sources: `openvmm/membacking/src/mapping_manager/va_mapper.rs:961-971`; `openvmm/membacking/src/mapping_manager/manager.rs:827-842`.
* File-backed versus anonymous/private allocation is separate from SNP page visibility. The builder currently allocates regular shared memory, hugepage shared memory, or anonymous/private mappings. Dispatch passes the memory configuration's `private_memory` flag. Do not label `--memory shared=off` as an implementation of `guest_memfd_lite`. Sources: `openvmm/membacking/src/memory_manager/mod.rs:485-535`; `openvmm/openvmm_core/src/worker/dispatch.rs:1475-1477`; `Guide/src/reference/openvmm/management/cli.md:31-59`.

### 2.3 Queue metadata is not payload

* Queue construction keeps `GuestMemory` subranges for the fixed descriptor ring, split avail/used rings, or packed event regions. These use `subrange(..., true)`, which permits backing-specific preemptive locking. The default subrange implementation currently ignores this hint, but another backing may honor it. Sources: `vm/devices/virtio/virtio/src/queue.rs:241-273`; `vm/devices/virtio/virtio/src/queue/split.rs:33-63`; `vm/devices/virtio/virtio/src/queue/packed.rs:62-95`; `vm/vmcore/guestmem/src/lib.rs:1425-1458,891-906`.
* Descriptor fetch uses `read_plain`. Indirect tables are guest subranges created dynamically when walking a chain, also with the preemptive-locking hint. Work items contain owned descriptor values and a completion token, not automatically bounced payload bytes. Chain length is bounded by queue size and nested indirect descriptors are rejected. Sources: `vm/devices/virtio/virtio/src/queue.rs:29-39,440-476,655-731`.
* Work payload read/write APIs traverse readable/writable descriptors and call guest `read_at`/`write_at`. The common offset helpers use saturating address addition, whereas `regions::data_regions` currently has unchecked `addr += skip`. Shape conversion is not sufficient GPA accessibility validation. Sources: `vm/devices/virtio/virtio/src/common.rs:34-95,144-210`; `vm/devices/virtio/virtio/src/regions.rs:47-65,86-148`.
* Completion publishes split used elements and then the used index with fences, or packed used descriptors with a release fence. The current public completion API consumes the token and releases in-flight capacity before publishing; publishing failures are logged, not returned to the device. Sources: `vm/devices/virtio/virtio/src/queue/split.rs:230-295`; `vm/devices/virtio/virtio/src/queue/packed.rs:219-258`; `vm/devices/virtio/virtio/src/common.rs:390-415`.
* Virtio-net buffers completion tokens in `InOrderCompletion`, and RX/TX call that helper. Preserve its publication order when adding scratch ownership. Do not assume all devices have the same completion discipline. Sources: `vm/devices/virtio/virtio/src/in_order.rs:29-52,101-148`; `vm/devices/virtio/virtio_net/src/lib.rs:1338-1347,1415-1425`.

**Decision:** Fixed rings and indirect tables use bounded synchronous metadata copies; payload submissions use owned backend buffers. No guest slice or copy guard crosses backend work or queue waits. Phase 1a retains acquisition-only access. Phase 1b adds acquire/copy/release and one coordinator for metadata and payload, including shared pages. Phase two adds kernel backing/MM enforcement.

### 2.4 Existing backend lifetimes and limits

* `virtio_blk` caps in-flight requests at 64. Pending disk futures live in persistent worker state, survive task stop, and are drained before removing the queue. Preserve this behavior. Sources: `vm/devices/virtio/virtio_blk/src/lib.rs:58-87,152-166,186-209,426-443`.
* Its bounce request size is only capped below `u32::MAX`; the implementation explicitly leaves a practical allocation cap as a TODO. Region/GPN expansion also allocates. Sources: `vm/devices/virtio/virtio_blk/src/lib.rs:690-725`; `vm/devices/virtio/virtio/src/regions.rs:94-143`.
* `BounceBuffer` and `GuestMemory::allocate` use 4096-byte-aligned page types. They are useful existing primitives, but their constructors are infallible allocation APIs. Alignment to 4096 is not proof of every possible backend's stronger address/length/offset constraint. Sources: `vm/devices/storage/scsi_buffers/src/lib.rs:167-202`; `vm/vmcore/guestmem/src/lib.rs:105-123,1354-1360`.
* The existing tracker decrements a per-thread page counter, waits if capacity is insufficient, and restores pages on drop. A request larger than the entire budget cannot succeed by waiting. It is not a reusable buffer pool, and thread indices are unchecked `unwrap` inputs. Sources: `vm/devices/storage/scsi_buffers/src/lib.rs:486-500,515-557`.
* Disk API reads/writes return `Result<(), DiskError>`, not partial counts. `disk_blockdevice` rejects short read/write results; its bounce read copies only after a full backend result. `FileDisk` currently does not check `read_at`/`write_at` byte counts. These are existing semantics, not guarantees that every backend is exact-length. Sources: `vm/devices/storage/disk_backend/src/lib.rs:241-261,499-529`; `vm/devices/storage/disk_blockdevice/src/lib.rs:608-615,670-674`; `vm/devices/storage/disk_file/src/lib.rs:108-136`.
* Dropping an in-flight io_uring future is **not safe asynchronous cancellation**. The driver contract says it aborts the process, and `IoFuture::drop` implements that behavior. Owned bounce memory prevents guest-pointer exposure; it does not solve cancellation by itself. Sources: `support/pal/pal_async/src/driver.rs:90-139`; `support/pal/pal_async/src/unix/epoll_uring.rs:476-500`.

## 3. Device and backend coverage matrix

“Adapt” means planned support, not current isolation support. This matrix describes the full phase-two disposition. Phase 1a can admit converted payload paths without scoped access; phase 1b adds scoped payload and metadata copies on existing interfaces. Configurations that export actual guest backing remain rejected in bounce mode. PSP/AP/doorbell no-pin requirements apply to phase two; preserve existing supported control behavior in phase one and state that exception.

| Surface | Current behavior and evidence | Copy-only SNP disposition |
|---|---|---|
| Virtio-blk, including scattered payload | Layout-dependent frontend bounce; otherwise passes guest `RequestBuffers` to arbitrary `Disk`; header/status use guest copy APIs. `virtio_blk/src/lib.rs:454-530,622-754` | Adapt first. Force owned staging even for page-compatible/aligned chains. Bound size before region/GPN expansion. |
| Linux block file/device, O_DIRECT/io_uring | `always_bounce`/unaligned/non-lockable fallback; direct branch keeps guest locks through await. `disk_blockdevice/src/lib.rs:565-674` | Safe backend buffer after frontend/disk staging. Retain backend alignment bounce if needed; do not pass actual guest memory to its direct branch. |
| Portable `FileDisk` | Allocates host `Vec`, copies before/after blocking file I/O. `disk_file/src/lib.rs:102-136`; trait callers `:175-191` | Adapt/retain. Fix exact-transfer handling as part of the touched I/O semantics, and budget allocations. |
| Layered/encrypted/VHD/VHDX and other `DiskIo` implementations | Backend API accepts `RequestBuffers`, exposes its memory/range, and permits implementation-specific access. `disk_backend/src/lib.rs:241-261`; `scsi_buffers/src/lib.rs:358-412` | Contain with staging at the public `Disk` boundary when buffers refer to copy-only guest memory. Do not claim every implementation already bounces. Ensure wrappers forward owned buffers, not original guest references. |
| Host NVMe disk | `NvmeDisk` passes memory/range to namespace read/write; `issue_external` chooses direct IOVA or device-owned double buffer. `disk_nvme/src/lib.rs:77-137`; `disk_nvme/nvme_driver/src/queue_pair.rs:788-865` | Conditional adaptation only after confirmed command completion/reset/drain and persistent submitted-operation ownership. For actual guest backing, no IOVA and no direct DMA. Device DMA buffers may be pinned. Validate transfer limits and retained buffers on timeout/death. |
| Virtio-net TX/RX | Frontend passes guest `TxSegment` addresses and a pool exposing guest memory/RX addresses; RX writes happen through pool callbacks. `virtio_net/src/lib.rs:1023-1087,1330-1425`; `virtio_net/src/buffers.rs:128-210` | Adapt with a bounded frontend-owned scratch arena and remapped TX/RX segments. Backend sees scratch memory only, including on later polls. Copy RX to guest before used completion. |
| TAP | TX linearizes; kernel writes host packet slices. `net_tap/src/lib.rs:306-353`; helper `net_backend/src/lib.rs:288-303` | Adapt via scratch pool. Preserve negotiated checksum/GSO/VLAN behavior and short-submit semantics. Avoid needless second linearization later. |
| DIO | TX copy occurs inside NIC `write_with` callback. `net_dio/src/lib.rs:173-195` | Adapt via scratch pool. Do not perform actual guest access inside a kernel/backend buffer callback. |
| Consomme | Later `poll_ready` drains queued guest segments and copies into scratch before protocol processing. `net_consomme/src/lib.rs:682-729` | Adapt via scratch pool. Retain scratch until backend TX completion; synchronous `tx_avail` method shape is not proof of synchronous payload lifetime. |
| MANA | Explicit bounce mode; direct RX uses guest IOVAs; TX bounce copies guest into driver DMA pool, direct paths use guest IOVAs. `net_mana/src/lib.rs:118-142,708-741,1252-1278,1335-1445`; tests `net_mana/src/test.rs:83-123` | Conditional on confirmed hardware completion/reset/drain and persistent buffer ownership. Force `BounceBuffer`. Existing bounce mode does not support TCP segmentation in its TX branch (`:1253-1255`); reconcile feature advertisement/software segmentation before allowing this configuration. Driver-owned DMA memory is allowed. |
| In-process virtio-vsock | Direct locked socket TX/RX with fallback; TX cap, but fallback RX allocation derives from posted writable capacity. `virtio_vsock/src/lib.rs:343-392,603-639`; `virtio_vsock/src/connections.rs:453-490` | Adapt. Disable guest locks in both directions. Cap RX allocation by capacity, negotiated credit, and device limit. Handle WouldBlock/partial data using owned buffers. |
| Kernel vhost-vsock | Makes separate eager guest file mappings for kernel use. `virtio_vsock/src/vhost.rs:333-404`; mapped-ring read `:314-328` | Reject. Existing fallback is in-process vsock, not automatic kernel-vhost bounce emulation. |
| vhost-user generic/fs/blk/network | Exports guest backing FDs using `SET_MEM_TABLE`, then queue addresses. Mapping persists across reset. `vhost_user_frontend/src/lib.rs:331-378,414-425`; resources `virtio_resources/src/lib.rs:185-230` | Reject. A future shadow-ring/payload protocol is separate work; do not expose the one guest memfd to an external process. |
| Virtio-console | Fixed-size host RX/TX scratch, partial TX progress, backend awaits after guest copy. `virtio_console/src/lib.rs:298-375` | Adapt common copy guard; preserve cancel-safe partial progress and reconnect behavior. No bulk rewrite needed. |
| Virtio-9p | Copies entire request and response into host vectors, lengths derive from guest payload. `virtio_p9/src/lib.rs:197-228` | Adapt with protocol/request limits and fallible budgeted buffers; keep guest access outside filesystem operations. |
| Virtio RNG | Host vector capped by `MAX_REQUEST_BYTES`, then guest write. `virtio_rng/src/lib.rs:168-187` | Adapt common copy guard; count only copied bytes and retire on inaccessible completion metadata. |
| In-process virtio-fs without DAX | Filesystem dispatch receives a streaming guest reader and reply writer, not owned request/reply snapshots. `virtiofs/src/virtio.rs:307-380`; `virtiofs/src/virtio_util.rs:72-89,164-184` | Defer initially, then adapt request/reply staging. Filesystem backend must not receive an actual guest reader/writer. Validate every pointer addition and total reply length. |
| Virtio-fs DAX; virtio-pmem | File mappings into a shared device-memory region bypass payload I/O. `virtiofs/src/virtio.rs:382-411`; `virtio_pmem/src/lib.rs:68-104` | Reject initially. Not evidence of kernel pins on guest RAM, but not covered by queue bounce. Separate device-memory visibility and backing contract required. |
| Emulated NVMe/SCSI/IDE, storvsp | NVMe and SCSI hand guest `RequestBuffers` to `Disk`; IDE has guest copy paths. `nvme/src/namespace.rs:145-175`; `scsidisk/src/lib.rs:1166-1211`; `ide/src/lib.rs:648-715` | Disk boundary can cover disk payload, not controller metadata, PRPs, completion queues, or transport rings. Gate complete device support until all such accesses use scoped copies. |
| VMBus ring/event pages, netvsp | VMBus rings hold `LockedPages`; netvsp RX buffer pool also locks guest pages. `vmbus_channel/src/gpadl_ring.rs:82-113`; `netvsp/src/buffers.rs:108-113`; `vmbus_server/src/lib.rs:2197-2202` | Reject initial SNP copy-only configurations that need these long-lived locks. A copied ring implementation is separate work; not fixed by bouncing disk payload. |
| Windows VMBus proxy | Passes a full guest VA mapping to kernel proxy. `vmbus_proxy/src/lib.rs:260-276` | Reject for this policy; host/platform-specific and not a scoped-copy interface. |
| VFIO type1 assignment | Maps guest backing VA into IOMMU. `vfio_assigned_device/src/manager.rs:29-45,439-445` | Reject before registration; arbitrary assigned device DMA cannot be transparently payload-bounced. |
| iommufd assignment, including file mapping/nested IOMMU | RAM path prefers `ioas_map_file`; fallback uses host VA. File path pins folios directly, rather than eliminating pins. `vfio_assigned_device/src/manager.rs:565-625`; `vfio_sys/src/iommufd.rs:416-483` | Reject both fd and VA routes for guest backing, including nested acceleration. No “map by file is safe because no VA pins” exception. |
| Driver-owned DMA allocations | `DmaClient` allocates device buffers; lockmem uses mmap/mlock and supplies DMA blocks. `user_driver/src/lib.rs:85-93`; `user_driver/src/lockmem.rs:54-58,94-100,133-135` | Allowed when backing is OpenVMM-owned scratch, not guest RAM. The requirement is **no guest backing pins**, not “no pinned memory anywhere.” |
| Emulated IOMMU translation | `TranslatingMemory` performs page-split translated reads/writes through inner guest memory while holding a translation lock. `iommu_common/src/lib.rs:198-224,238-281`; tests `:589-606` | Adapt policy propagation and lock ordering. Stage data without exporting translated guest VAs. Translation-table reads are metadata copies, not backend payload. |
| Remote chipset worker memory proxy | Read/write/fill RPCs forward to local `GuestMemory`; remote fallback copies response bytes. `workers/chipset_device_worker/src/guestmem.rs:85-101,142-204` | Potentially compatible for copy operations after local guard integration; bound RPC sizes. Do not infer all remote memory mappings are safe. |
| Hypervisor/boot/control access | SNP launch reads VMSA/CPUID through guest memory, separately tracks VMSA backing, registers user VA regions, and imports pages. `virt_mshv/src/x86_64/snp.rs:604-634,668-711,741-798`; `virt_mshv/src/lib.rs:273-294,980-999` | Separate prelaunch/control access policy. Bounce I/O does not remove MSHV backing-registration/control-page kernel references. Kernel contract and dedicated teardown tests are required. |
| Runtime PSP SNP guest request | `handle_snp_guest_request` builds `mshv_issue_psp_guest_request` with guest request/response GPAs and submits it. `virt_mshv/src/x86_64/snp.rs:1663-1724` | Reject before submission until a verified kernel/Hyper-V owned-buffer API preserves the encrypted protocol and copies through scoped windows. Do not place bytes at an arbitrary “bounce GPA” or reuse the current GPA API as proof of staging. Pending PSP operations require persistent completion/input/output owners. |
| Runtime AP VMSA and doorbell | AP creation registers a guest VMSA GPA; doorbell registration passes a guest GPA to a VP register. `virt_mshv/src/x86_64/snp.rs:1727-1838,1605-1637` | Explicit control-page exception or reject. Track owner VP, backing identity, page type, generation, root eligibility, replacement, stop/unregister/drain, reclaim, and teardown. No ordinary device/backend access or guest backing pin is permitted by the exception. Capability remains unavailable if required control-page retention violates the no-pin contract. |
| Kernel GHCB state | `SnpVpState::new` maps the VP fd GHCB-state offset; `MshvGhcbPage::drop` unmaps it. `virt_mshv/src/x86_64/snp.rs:195-241` | Keep separate kernel VP-state lifecycle. Do not label this mapping guest RAM or apply a guest copy lease to it. Validate kernel protocol data and keep its owner alive through VP use. |

The table uses crate-relative citations to keep cells readable. The exact source paths and principal evidence ranges are:

* Block frontend: `vm/devices/virtio/virtio_blk/src/lib.rs:454-530,622-754`.
* Linux block backend: `vm/devices/storage/disk_blockdevice/src/lib.rs:565-674`.
* File backend: `vm/devices/storage/disk_file/src/lib.rs:102-136,175-191`.
* Generic disk and request buffers: `vm/devices/storage/disk_backend/src/lib.rs:241-261`; `vm/devices/storage/scsi_buffers/src/lib.rs:358-412`.
* Host NVMe disk and driver: `vm/devices/storage/disk_nvme/src/lib.rs:77-137`; `vm/devices/storage/disk_nvme/nvme_driver/src/queue_pair.rs:788-865`.
* Network frontend and buffers: `vm/devices/virtio/virtio_net/src/lib.rs:1023-1087,1330-1425`; `vm/devices/virtio/virtio_net/src/buffers.rs:128-210`.
* Network API: `vm/devices/net/net_backend/src/lib.rs:186-303`.
* TAP: `vm/devices/net/net_tap/src/lib.rs:306-353`.
* DIO: `vm/devices/net/net_dio/src/lib.rs:173-195`.
* Consomme: `vm/devices/net/net_consomme/src/lib.rs:682-729`.
* MANA: `vm/devices/net/net_mana/src/lib.rs:118-142,708-741,1252-1278,1335-1445`; `vm/devices/net/net_mana/src/test.rs:83-123`.
* In-process vsock: `vm/devices/virtio/virtio_vsock/src/lib.rs:343-392,603-639`; `vm/devices/virtio/virtio_vsock/src/connections.rs:453-490`.
* Kernel vhost-vsock: `vm/devices/virtio/virtio_vsock/src/vhost.rs:314-328,333-404`.
* vhost-user: `vm/devices/virtio/vhost_user_frontend/src/lib.rs:331-378,414-425`; `vm/devices/virtio/virtio_resources/src/lib.rs:185-230`.
* Console: `vm/devices/virtio/virtio_console/src/lib.rs:298-375`.
* 9p: `vm/devices/virtio/virtio_p9/src/lib.rs:197-228`.
* RNG: `vm/devices/virtio/virtio_rng/src/lib.rs:168-187`.
* Filesystem request/reply and DAX: `vm/devices/virtio/virtiofs/src/virtio.rs:307-411`; `vm/devices/virtio/virtiofs/src/virtio_util.rs:72-89,164-184`.
* Persistent memory region: `vm/devices/virtio/virtio_pmem/src/lib.rs:68-104`.
* Emulated storage: `vm/devices/storage/nvme/src/namespace.rs:145-175`; `vm/devices/storage/scsidisk/src/lib.rs:1166-1211`; `vm/devices/storage/ide/src/lib.rs:648-715`.
* Persistent VMBus/netvsp buffers: `vm/devices/vmbus/vmbus_channel/src/gpadl_ring.rs:82-113`; `vm/devices/net/netvsp/src/buffers.rs:108-113`; `vm/devices/vmbus/vmbus_server/src/lib.rs:2197-2202`.
* Proxy kernel mapping: `vm/devices/vmbus/vmbus_proxy/src/lib.rs:260-276`.
* Assigned-device mappings: `vm/devices/pci/vfio_assigned_device/src/manager.rs:29-45,439-445,565-625`; `vm/devices/user_driver/vfio_sys/src/iommufd.rs:416-483`.
* Owned DMA buffers: `vm/devices/user_driver/src/lib.rs:85-93`; `vm/devices/user_driver/src/lockmem.rs:54-58,94-100,133-135`.
* IOMMU translation: `vm/devices/iommu/iommu_common/src/lib.rs:198-224,238-281,589-606`.
* Proxy copies: `workers/chipset_device_worker/src/guestmem.rs:85-101,142-204`.
* MSHV control/launch: `vmm_core/virt_mshv/src/x86_64/snp.rs:604-634,668-711,741-798`; `vmm_core/virt_mshv/src/lib.rs:273-294,980-999`.
* Runtime PSP, AP, doorbell, and kernel GHCB state: `vmm_core/virt_mshv/src/x86_64/snp.rs:1663-1724,1727-1838,1605-1637,195-241`.

## 4. Proposed architecture

### 4.1 Explicit capability policy, not heuristic selection

Add an immutable guest-memory **I/O** policy selected during VM construction, before `GuestMemory::new` caches backing capabilities. It is a userspace routing/export restriction, independently usable on current backing in phase one; it is not kernel enforcement:

* Existing normal memory: retains current supported access/export capabilities.
* Copy-only I/O guest view: permits bounded byte/plain/atomic copies; denies device-facing persistent slices, page locks, raw guest VA exports, IOVA exports, backing FD sharing, and guest DMA registration. Phase one retains the separate existing MSHV RAM-registration and control interfaces. Phase two additionally requires lite backing and MM enforcement.
* Host-owned I/O memory: can use normal locks, raw pointers, backend registration, and device DMA as needed.

A conceptual name is `GuestMemoryIoPolicy::CopyOnly`; choose final names during implementation. This is stronger than `supports_locking=false`. Forward policy through `Arc`, default nested subranges, multi-region wrappers, translated memory, and remote proxies. Preserve correct region offsets and return typed errors for denied operations. Do not accidentally make a copy-only subrange lockable because it still has an internal mapping.

Distinguish **internal mapping for fault-contained copies** from **exportable mapping**. `GuestMemory::full_mapping()` must return `None` for copy-only guest memory even if the internal mapped-copy fast path exists. `sharing()` and `iova()` must be denied. The region manager must reject guest-memory DMA registration independently, because it supplies VAs/FDs without going through these GuestMemory methods.

These userspace restrictions can ship in phase one without a new kernel capability, but cannot claim kernel no-pin/no-PFN-export enforcement. In **phase two**, capability discovery must confirm the companion's immutable MM restriction and terminal rejection of enabled GUP and non-GUP consumers, including KVM, VFIO, legacy MSHV MMIO, and another partition. Without that proof, withhold lite capability or enforce a supported deployment exclusion. See `plan-mshv-guest-memfd-lite.md`, sections 3 and 4.

Do not use `supports_locking` as the sole signal that a buffer is guest memory: emulated-IOMMU memory is already non-lockable. Carry policy/origin explicitly so device-owned staging is not needlessly staged again.

### 4.2 Scoped synchronous access boundary

**Phase 1b, independent of lite backing.** “Per-copy” means the short interval needed to transfer bytes between guest RAM and an owned buffer. It does not mean an I/O request lifetime, a guest ownership transition, or a hypervisor token. For completed read/RX, acquire only when the backend has returned valid bytes, copy them into the guest, then release. Write/TX acquires and copies out before submission. Descriptor, queue, status, and ring copies follow the same rule.

Extend the internal Rust host-access abstraction with fallible release and a coordinator/guard. Keep the existing `modify_gpa_host_access` interface: acquisition sets `ACQUIRE` and the supported permissions; release clears `ACQUIRE`. The current kernel initializes hypercall flags to zero (adding only `LARGE_PAGE` if requested), selects acquire from `ACQUIRE`, and invokes `hv_call_modify_spa_host_access` (`K/drivers/hv/mshv_root_main.c:2415-2468`, K root is the companion's kernel tree). The PSP path independently uses flags-zero release (`K/drivers/hv/mshv_root_main.c:2557-2577`). These calls do not add `MAKE_SHARED` or `MAKE_EXCLUSIVE`. No new Hyper-V operation, window capability, or BEGIN/END ioctl is required.

The bounded synchronous leaf operation is:

1. Translate addresses, select RAM backing, validate identity and checked byte ranges, then normalize to 4 KiB GPA ranges with checked rounding. Validate every covered page, holes, permissions, and page type; zero-length copies acquire nothing.
2. Under one partition coordinator lock, reserve all ranges **before acquisition**. Track RAM identity and acquiring, copying, releasing, and uncertain state. Pending guest changes and copies use the same admission decision, so neither can slip between the other's conflict check and reservation.
3. Issue the existing host-access acquisition. Copy only after definite success and only through fault-contained leaf helpers. Failed acquisition never permits a copy; retain any partially or uncertainly granted extent for release/recovery.
4. Copy only the bounded requested bytes, with no backend call, application await, RPC wait, or task yield while access is live.
5. Release through the existing call before retiring the reservation or returning success. Guest ownership remains unchanged. A grant must be authorized by Hyper-V; do not force guest SHARED state, infer eligibility from a present PTE, or claim arbitrary private plaintext access.

Initially serialize all leaf copies per partition, including overlap. Reserve pending ranges before the grant call; never let one copy release another copy's grant. No nested acquisition or permission upgrades. Internal reference counts are an optional later optimization, not a kernel token API.

The coordinator mutex protects admission and state changes only; release it before acquire/release ioctls and byte copies. Reservations remain visible throughout acquisition, copy, release, and uncertainty, so another VP can reject a conflicting exit without waiting for the active operation.

Phase 1b also admits existing runtime operations that change host access or establish control-page use through this coordinator, including PSP request/response pages and AP VMSA/doorbell registration or replacement. Preserve their ABI and supported behavior, but reserve affected pages before submission and exclude ordinary copies for the required operation/control lifetime. Reject conflicts before side effects through an established failure protocol, or retain fail-closed stopping where unresolved. Uncertain outcomes retain the exclusion and cleanup owner. This is userspace conflict coordination, not phase-two no-pin/control-lifecycle enforcement. The current PSP path itself releases host access before submission (`K/drivers/hv/mshv_root_main.c:2536-2577`), so excluding only attribute exits is insufficient.

Select scoped mode before runtime guest copies. Retire or account for any preexisting launch/initialization grants before opening scoped admission; a successful mapped access must not inherit an untracked persistent grant. Do not switch a running phase-1a VM to scoped mode without a complete coordinated drain.

Acquire exactly once at the ultimate backing operation, including mapped accesses that never fault. `Arc` forwards, subranges translate, multi-region views select backing, and IOMMU translation precedes leaf admission. RPC forwarding holds no grant while waiting. Keep current IOMMU/manager lock ordering explicit and use bounded admission; never wait while holding a lock required by a conflicting operation. Evidence: `vm/devices/iommu/iommu_common/src/lib.rs:198-224,238-281`; `workers/chipset_device_worker/src/guestmem.rs:85-101,125-134,158-195`.

In scoped mode, fault handling services only an already confirmed leaf grant. It must not independently acquire again, revive a released grant, or retry an ineligible fault forever. Phase 1a alone retains the current acquisition-on-fault callback.

### Conflicting guest exits: reject the request, not the copy

All VPs consult the same coordinator for intercepted ownership, visibility, or access changes. Validate range count against the 29-entry payload, encodings, checked GPA arithmetic, RAM identity, and 4 KiB extents before admission. Large-range requests are unsupported in this first implementation. If **any** requested range overlaps an acquiring, copying, releasing, or unresolved grant, reject the **whole** request atomically; no partial ownership update. Do not wait for backend I/O: no grant is held across backend work. An already admitted guest change reserves its range until resolved, preventing a racing copy acquisition. Unsupported nonconflicting transitions remain rejected; this work does not invent transition support.

“Deny” must mean a verified failure completion without applying the change or replaying it as success. **The exact existing GPA-attribute rejection/resume protocol remains a narrow implementation question.** `HvX64GpaAttributeInterceptMessage` contains VP index, attribute flags, and 29 ranges, but no result/completion field (`vm/hv1/hvdef/src/lib.rs:4335-4358`). Its SNP dispatch calls the current halt handler (`vmm_core/virt_mshv/src/x86_64/snp.rs:1133-1134`); the supplied baseline handler returns `TripleFault`, not a guest-visible failure. The inspected legacy register-intercept-result definition supports CPUID/MSR results, not a proven GPA-attribute reply (`K/include/hyperv/hvhdk.h:1093-1125`; forwarding call `K/drivers/hv/mshv_root_main.c:1217-1229`). `CompletePartitionIntercept` is only an enumerated property in the inspected Rust definition (`vm/hv1/hvdef/src/lib.rs:515`), not a demonstrated completion call site.

GHCB error returns are a different protocol: `set_ghcb_error` writes exit-info error fields (`snp.rs:368-372`), while GHCB page handling receives a VMGEXIT snapshot (`:1404-1418`); this does not establish a reply for the separate GPA-attribute message. No legacy non-SNP attribute handler or concrete denial call was found in the scoped trace. Do not invent an error status, write unrelated VP/GHCB registers, or treat `TripleFault` as clean denial. Until the existing completion/rejection path is established, retain fail-closed halt/stop on these exits and do not resume the unresolved request. Guest retry is the guest's responsibility **only if** the verified failure-return protocol permits it. This question does not require a new hypervisor interface.

Prefer explicit fallible `finish/release` plus a non-panicking RAII cleanup backstop. Failed or uncertain release keeps the conflicting range blocked in quarantine, stops/poisons the partition, closes new copy/control admission, notifies management, and retains backing and cleanup ownership. A device cannot suppress this by logging its local error. Do not blindly reacquire, replay, free, or resume. Confirm platform ordering and failure behavior through existing-call regression tests.

Phase two integrates this same lifecycle with lite kernel object/map state. Faults install accessible PTEs only under confirmed authorized access. Release/revocation closes faults, zaps aliases, and finishes CPU TLB barriers before access is considered revoked; private commit cannot leave a usable Linux mapping. Kernel ownership/teardown may need a drain handshake, but ordinary copy admission and conflict denial do not stop all VPs or reserve ownership transactions per copy. Large pages are deferred; ordinary direct-map removal/splitting needed for MM safety is not.

### 4.3 Owned I/O dataflow

This dataflow applies throughout. Phase 1a uses ordinary synchronous guest-copy calls; current hypervisor grants may persist afterward. Phase 1b implements the shown acquire/copy/release with existing calls, and phase two adds MM enforcement. Waiting for scratch or backend work must never retain a guest slice or copy guard.

Guest-to-backend write/TX:

```text
descriptor/header snapshot and validation
  -> await scratch-budget reservation (no guest access)
  -> acquire -> copy guest bytes into owned scratch -> release
  -> submit/await/poll backend using scratch only
  -> write status and publish used completion through fresh metadata windows
```

Backend-to-guest read/RX:

```text
descriptor/header snapshot and validation
  -> await scratch-budget reservation (no guest access)
  -> submit/await/poll backend into owned scratch only
  -> validate actual backend result and byte count
  -> acquire -> copy initialized valid bytes to guest -> release
  -> status/header -> release fence -> used ring publication -> notification
```

Descriptors, GPAs, completion tokens, and scratch handles may live across async work. Actual guest-access guards, guest slices, and guest backing export handles may not.

The distinction is important for RX: posting a guest RX capacity is not a reason to acquire that guest page. Access starts only when a completed owned packet is ready to copy.

### 4.4 Storage boundary

* Force frontend coalescing in `virtio_blk::do_io` when the source memory is copy-only, even when `try_build_gpn_list` succeeds. Reuse its existing owned-memory mechanism and scatter helpers after adding validation/bounds.
* Add a common staging helper at `Disk::read_vectored`/`write_vectored` for copy-only request buffers. It converts the original paged guest range to an owned buffer, passes only that owned `RequestBuffers` to `DiskIo`, and copies read results back afterward. This covers non-virtio frontends and backend decorators. When virtio-blk has already staged, the helper recognizes host-owned memory and does not stage again.
* Keep the current public `DiskIo` shape initially. This limits churn across many backend implementations. An eventual typed host-I/O-only buffer API can strengthen compile-time enforcement, but is not a prerequisite for the first conversion.
* Validate direction and whole-sector length before calling existing panic-on-invalid-internal-input helpers. Retain LBA bounds/overflow checks at the backend. Preserve FUA, flush, discard, reservations, and backend error mapping.
* Do not re-run a write/flush/discard after a failed guest status/completion copy. The backend operation may already have committed.
* Host NVMe should see host-owned staging and select driver double buffering when it lacks IOVA. Do not register the guest RAM merely to enable its direct branch. Keep host NVMe disabled until command completion/reset/drain and persistent submitted-buffer ownership are confirmed on the selected driver/hardware path; a double-buffer branch by itself does not satisfy this gate.

### 4.5 Network boundary

Keep the common backend API initially, but give copy-only virtio-net a **different pool memory object backed entirely by owned scratch**:

* TX: validate/copy the virtio header and packet into an arena slot, then derive checksum/VLAN/segmentation metadata and lengths **from that same owned snapshot**. Do not combine a pre-copy guest header/Ethernet peek with a later payload copy. Rewrite `TxSegment.gpa` to scratch offsets, and hold that slot through async TX completion. Retained backend segment lists then refer to scratch, not guest addresses.
* RX: reserve bounded scratch slots, expose scratch capacities/addresses to the backend, and record received data/metadata there. On `rx_poll` completion, copy the valid packet into the saved guest descriptor snapshot, write the virtio header, then publish in order.
* Separate the frontend's actual guest memory from `BufferAccess::guest_memory`; never use the backend-facing scratch accessor to fetch queue metadata.
* Callbacks with `()` returns cannot report a failed guest write. In copy-only mode they should only write scratch. The frontend's final copyback is fallible and controls the completion byte count.
* Keep slot identities stable while posted; include generation/state checks to reject stale or duplicate backend IDs before indexing. Respect partial segment submission: keep unsent slots and descriptor tokens, and do not free completed/sent slots early.
* Apply endpoint feature restrictions before negotiation. MANA's bounce branch currently asserts no TCP segmentation; do not allow guest-controlled offloads to reach that assertion. Force its existing driver bounce mode, or add a separately tested software segmentation path.

An ordinary heap arena is not a direct-DMA arena. MANA/host drivers must still allocate their own DMA-capable bounce buffers or use an explicitly owned DMA pool. Do not invent IOVAs for normal heap scratch.

Physical-network support remains disabled until the selected endpoint demonstrates terminal completion/reset/drain and persistent buffer ownership under stop, timeout, and device failure. Force bounce and negotiate safe offloads only after that gate; the existing bounce test is not hardware-drain proof.

### 4.6 Metadata, errors, retries, and ordering

Validation, snapshots, partial-result handling, and exactly-once publication apply throughout. Scoped host access, conflict denial, release failure, and partition poison enter in phase 1b. Kernel ownership transitions and enforced Linux revocation enter in phase two. Unsupported private/shared transitions remain rejected.

* Use short accesses for fixed rings, event suppression, status/header bytes, indirect descriptors, controller command/response metadata, and emulated-IOMMU page tables.
* Keep the existing acquire/release fences and ordering helpers. The host-access release must finish before used publication makes payload visible. Confirm kernel release visibility semantics; a userspace fence alone cannot establish a missing kernel contract.
* Snapshot descriptors once for each accepted request. Do not re-read guest descriptor chains after a backend await to decide where to write. Treat guest modifications during a valid copy as untrusted concurrent data, not stable protocol state.
* Store queue/reset and backing-mapping generations with accepted work. Before copyback, verify token ownership and RAM identity, then acquire fresh authorized access to the destination. A pre-I/O probe is not a lease. If future supported transitions change generation, define fresh validation or request failure; never write into repurposed backing because its GPA matches. Eligibility is not an assumed SHARED requirement or a new window capability.
* Revocation cannot erase bytes already copied while access was permitted. Backend work may continue on that authorized snapshot after the guest makes the source private. Define and document this semantics, scrub recycled scratch, and never acquire new private bytes to finish a partially captured write.
* Validate indirect lengths, entry counts, indices, alignment requirements, cycle/chain bounds, and all address additions before creating a subrange or allocation. Preserve split/packed format semantics.
* No whole-request atomic copy promise: a fault can leave an earlier scatter range or part of a guest write changed. For guest-to-host failure, do not submit the backend at all. For host-to-guest failure, do not report complete success; scrub/recycle scratch and use device-specific error/drop semantics.
* For disk operations, keep exact-length success semantics. For stream/network operations, copy only valid initialized bytes actually received, and retain progress for partial TX. Never copy capacity padding or stale pooled bytes.
* Prefer bounded transient retries tied to observed page-state/generation changes, not arbitrary busy loops. Do not return `Retry` forever for a page that remains ineligible or unmapped. Partition-window contention returns a defined busy outcome or bounded protocol wait, never nested acquisition or a guard carried through an application await.
* If payload copy fails, block can return IOERR if status is writable; network can complete/drop with zero valid bytes where the protocol permits. If status or used-ring publication is inaccessible, retire/fail the queue and report failure to management. Do not free a token and silently continue as if publication succeeded.
* Change the queue completion API to return a result or add a fallible path. Distinguish prepublication failure from failure after the used marker is visible (for example, notification suppression read failure). Never retry a visible completion as a new completion.
* The used-index/packed-used write is itself a scoped copy: the marker can become visible **before that window's release fails**. Record whether publication occurred; a visible marker remains exactly-once completed and cannot be retracted, rewritten as IOERR, or replayed. Poison/gate the partition and retain the uncertain window/cleanup owner regardless of the visible success marker. Report publication state separately from release/notification errors. Do not hold a payload window until completion; payload release still precedes used publication.
* For split rings, stage/check used-element and used-index updates, including cursor behavior when the element succeeds but index fails. For packed rings, used flags are the publication point. Preserve in-order net behavior and do not advance the packed cursor twice.

### 4.7 Allocation, alignment, cancellation, and dirty state

Bounded buffers and retained submitted-operation ownership are phase-one requirements. During a live VMM, stop/reset must drain operations rather than drop unsafe futures or recycle DMA buffers early. After process death, phase one relies on current kernel/driver close and I/O ownership contracts; it does not claim new guest-reclaim guarantees. Phase 1b retains uncertain host-access cleanup; phase two adds kernel ownership/control/teardown recovery.

* Start with a configurable host-side maximum request size and total per-VM/queue scratch budget. A possible initial block maximum is 1 MiB, but choose the final limit with workload data and advertise compatible `size_max`/`seg_max` limits. Do not silently use `seg_max * PAGE_SIZE`: one descriptor can legitimately span many pages.
* Bound descriptor expansion and metadata before payload allocation. Use checked sums/conversions and fallible reservation. Return typed errors for oversize/OOM pressure rather than panicking or waiting forever.
* Improve/reuse `BounceBufferTracker` accounting where practical. Reject a request larger than the budget. Make reservation cancellation-safe; restore permits exactly once. Avoid a per-thread budget deadlock when an owned frontend buffer requires an additional backend DMA/alignment buffer.
* Pool a small set of aligned size classes. Keep active bytes and retained pool bytes separately bounded. Scrub buffers on reuse according to isolation policy; copyback must use an initialized-length field, never allocation capacity.
* Query or preserve backend address/length/offset alignment requirements. Page alignment and sector alignment solve the current normal block case, but requests needing larger alignment must use a suitable owned allocation or be rejected. Do not change the guest GPA to fake direct-I/O alignment.
* Stop acceptance first, drain backend work, then retire/reset queue state and release scratch. Preserve virtio-blk's existing drain path. For non-cancel-safe io_uring, transfer outstanding owned buffers into a persistent worker owner and finish them rather than dropping the future.
* Install persistent submitted-operation ownership **before** submission, not after timeout. Keep backend scratch, request/completion tokens, required partition/backing-generation references, and terminal-result state alive through timeout, interrupted callers, worker stop, and process death. For pending kernel ownership/control calls, the kernel's partition-wide operation owner must retain completion routing and all input/output/backing references after userspace disappears; userspace cannot provide that death guarantee. Do not reuse a completion slot, partition ID, or buffer until authoritative terminal completion/drain. Late results reach the same owner and do not automatically resume poisoned VPs.
* Device DMA cancellation requires confirmed hardware completion/reset/quiescence before recycling scratch. A Rust future being dropped is not proof that the device stopped DMA.
* Initial lite backing and scoped grants use 4 KiB pages. Large guest mappings/grants, huge folios, THP/hugetlb support, mixed 2 MiB states, and demotion are deferred to the companion's section 7; they are not launch requirements.
* Dirty/access bitmaps are not interchangeable. The guestmem bitmaps cited above control accessibility, not proven migration dirty tracking. Guest payload/status/ring writes must participate in any selected dirty-tracking/snapshot scheme. If MSHV SNP has no supported scheme, explicitly reject that migration mode rather than claiming copyback is tracked. Test payload, partial copy, status, used ring, and packed descriptors once a concrete dirty tracker is selected.

## 5. Concrete implementation stages

Each stage is a separate reviewable change. **Implement phase 1a first, then phase 1b on existing calls; neither depends on phase-two APIs or gates.** Select opt-in bounce/scoped policy before device/memory construction, independent of lite capability. Choose final setting names during implementation. Keep ordinary guests' fast paths unchanged.

| Stage | Prerequisites | Independently deliverable result |
|---|---|---|
| 1A | None | Userspace I/O policy, propagation, and denied device exports. |
| 1B | None | Fallible queue completion and error propagation. |
| 1C | 1A and 1B | Bounded owned storage; first block-only bounce-I/O configuration can ship after its gate passes. |
| 1D | 1A and 1B | Converted simple devices and in-process vsock. Can proceed alongside 1C. |
| 1E | 1A and 1B | Owned network staging. Can proceed alongside 1C/1D. |
| 1F | Relevant device conversion | Per-device enablement, documentation, broader coverage. No requirement to finish all devices before shipping a tested subset. |
| 1G (phase 1b) | Phase-1a policy and admitted device conversions | Scoped host access and conflict rejection on existing interfaces; independent of lite kernel capability. |
| 2A | Companion backing/MM capabilities and phases 1a/1b | Enforced lite backing, ownership/control lifecycles, and reclaim. |

### Stage 1A — userspace I/O policy and deny device exports

Files/functions:

* `vm/vmcore/guestmem/src/lib.rs`: `GuestMemoryAccess`, `DynGuestMemoryAccess`, `GuestMemoryInner`, `GuestMemory::new/new_multi_region`, `Arc<T>`, `GuestMemoryAccessRange`, `MultiRegionGuestMemoryAccess`, `full_mapping`, `sharing`, `iova`, `lock_gpns`, `lock_range`.
* `openvmm/membacking/src/mapping_manager/va_mapper.rs`: immutable policy construction; deny guest exports and locking for copy-only backing.
* `openvmm/membacking/src/mapping_manager/manager.rs` and `memory_manager/mod.rs`: create the primary with policy before clients can construct/copy `GuestMemory`; preserve single-backing mapping and process-local cache semantics.
* `openvmm/membacking/src/region_manager.rs`: reject device-facing guest RAM DMA registration under policy, including map-by-file and late mappings. Leave existing MSHV RAM registration intact. Kernel no-GUP/no-PFN-export capability is required only when phase two creates lite backing.
* `vm/devices/iommu/iommu_common/src/lib.rs` and remote guest proxy: propagate restriction/origin through translations/RPC adapters.
* `openvmm/openvmm_core/src/worker/dispatch.rs` and relevant device resolvers: initial allowlist and early errors for passthrough, vhost, DAX/pmem, and persistent-lock VMBus paths.
* `virtio/src/queue.rs`, `queue/split.rs`, `queue/packed.rs`: existing synchronous metadata copies, no preemptive locking under the I/O policy, and checked indirect metadata. Phase 1b scopes metadata access.

Tests: device-facing direct, `Arc`, nested subrange, multi-region, translated/proxy views cannot acquire guest locks or export VA/FD/IOVA. Host-owned staging remains lockable. Reject device DMA registration before its first mapping ioctl; this does not prohibit the legacy MSHV RAM-registration ioctl.

Test split/packed queue construction and indirect-table access under the copy-only policy, including malformed metadata and denied preemptive locking. These shared queue checks are prerequisites for the first block-only release, not deferred simple-device work.

### Stage 1B — fallible queue publication prerequisite

Land this **before Stage 1C storage error integration and Stage 1E network staging**, independently of scoped host access:

* `vm/devices/virtio/virtio/src/common.rs`, `queue.rs`, `queue/split.rs`, `queue/packed.rs`, and `in_order.rs`: return publication phase plus copy/notification outcome; retain exactly-once tokens/cursors. Distinguish not-published, published-with-error, and uncertain publication. Phase 1b adds release outcomes and partition poison.
* Wire device worker stop/failure propagation and management notification before storage begins to depend on fallible copyback/status completion.
* Test used-element/index partial publication, packed marker visibility, and failure after a visible marker. A visible completion never replays backend work or completion. Root-release failure injection belongs to phase 1b.

### Stage 1C — bounded owned storage

Files/functions:

* `vm/devices/storage/scsi_buffers/src/lib.rs`: bounded/fallible owned staging helper, permit/buffer lifetime, tracker oversize handling, origin-aware request buffer access.
* `vm/devices/storage/disk_backend/src/lib.rs`: copy-only staging in `Disk::read_vectored/write_vectored`, exact transfer/error contract; no guest reference enters `DiskIo`.
* `vm/devices/virtio/virtio_blk/src/lib.rs`: force staging in `do_io`, validate/cap before allocation and GPN expansion, checked `copy_regions`, practical request limits, correct status/copyback completion accounting.
* `vm/devices/virtio/virtio/src/regions.rs`: checked skipped-address and total-length arithmetic; malformed input returns an error.
* `disk_blockdevice`, `disk_file`, `disk_nvme/nvme_driver`: preserve direct-I/O alignment, exact-count checks, no guest DMA, and persistent ownership during stop/cancellation.

Do not solve every storage backend by adding its own `always_bounce` flag. The common boundary provides coverage; backend-specific buffers remain for alignment/DMA requirements only.

### Stage 1D — simple devices and in-process vsock

Files/functions:

* `virtio/src/common.rs` and `in_order.rs`: use the already-landed Stage 1B fallible completion API; retain ordering and notify semantics.
* `virtio_console`, `virtio_rng`, `virtio_p9`: use existing synchronous copy APIs, bound allocations, and correct failure lengths; do not pass guest slices to backend waits. Existing hypervisor grants may persist in phase one.
* `virtio_vsock/src/lib.rs`: `lock_payload_data`, `handle_guest_tx_inner`, `write_packet`.
* `virtio_vsock/src/connections.rs`: owned RX/TX paths, bounded credit-aware fallback buffer, partial/socket progress and lifetime handling.

### Stage 1E — network staging

Files/functions:

* `virtio_net/src/buffers.rs`: separate actual guest descriptor ownership from backend scratch memory; fallible final copyback.
* `virtio_net/src/lib.rs`: packet preparation with header/offload parsing from the same owned snapshot, TX segment rewrite, pending slot lifetimes, `transmit_pending_segments`, `process_endpoint_rx`, `complete_tx_packet`, stop/reset drains.
* `net_backend/src/lib.rs`: document scratch-address semantics and actual lifetime requirements. Add capabilities only if needed; do not redefine a GPA secretly without documenting which memory namespace applies.
* `net_mana` construction/resolution: force driver bounce and gate unsupported offloads.
* TAP/DIO/Consomme: test with scratch memory and asynchronous/partial completion. Optimization to remove extra copies follows correctness.

### Stage 1F — per-device enablement, broader coverage, and documentation

* Implement virtio-fs request/reply staging without DAX before allowing it.
* Audit non-virtio controller metadata and memory-transfer helpers for synchronous copies and owned payloads. Add each device/backend combination to the bounce-I/O allowlist only after its phase-one tests pass; no phase-two kernel proof is required to ship a converted subset.
* Keep incompatible backends rejected. Shadow vhost queues, assigned-device emulation, and copied VMBus rings require separate plans.
* Update the Guide memory-backing architecture page, SNP support notes, device/backend compatibility pages, and CLI reference if configuration changes. Explain the independent opt-in bounce-I/O mode, tested combinations, buffer limits, costs, and retained kernel pins/host-access behavior. Do not describe it as MM isolation or anonymous `shared=off` RAM as SNP-private state.
* Follow `.github/instructions/doc-code-sync.instructions.md` for implementation changes. This planning task itself does not change the Guide.

### Stage 1G — phase 1b scoped copies on existing interfaces

* Extend internal `PartitionHostAccess` with fallible release/cleanup; route the leaf hook through existing `modify_gpa_host_access`, without a new Hyper-V ABI or BEGIN/END ioctl.
* Add the partition coordinator in `virt_mshv`: checked 4 KiB RAM ranges, pending acquisition through completed release, serialization, and whole-request conflict denial across VPs. Resolve the existing attribute failure-completion path before permitting guest-visible denial/resume; otherwise keep the halt/stop fallback.
* Route existing PSP and AP VMSA/doorbell page use through the same admission domain without changing their ABI. Reserve before submission or replacement; exclude payload/metadata copies through the required control lifetime. Resolve failure-return behavior before resuming conflicts, retain uncertain exclusion, and never hold the coordinator mutex across an ioctl or copy.
* Integrate `guestmem`/`VaMapper` leaf copies, including metadata and mapped-success operations. Adapters acquire no outer guard; backend/RPC waits carry no grant.
* Add failed acquisition/release and visible-publication tests, plus platform regression/ordering tests of existing calls. Release failure blocks the range, stops the partition, and retains cleanup ownership. No lite-MM capability is required.

### Stage 2A — kernel backing and MM integration

Start after companion backing/MM gates pass. Reuse stage 1G; do not replace it with a token API.

* `vmm_core/virt_mshv/src/lib.rs`: kernel backing capability checks and ownership/teardown drain, timeout/death, and pending-operation coordination where needed.
* `openvmm/membacking/src/memory_manager/mod.rs`, `mapping_manager/manager.rs`, and `region_manager.rs`: allocate/map one lite object with 4 KiB backing, preserving GPA/file-offset layout, holes, permissions, and lifetime. Require MM no-GUP/no-PFN-export enforcement; reject unsupported hotplug/restart combinations.
* `vmm_core/virt_mshv/src/x86_64/snp.rs`: integrate existing scoped host-access calls with kernel object state. Supported ownership transitions must exclude active/pending grants; unimplemented transitions remain rejected.
* `MshvIsolationState::map_user_memory`, `PartitionMemoryMap::map_range/unmap_range`, `snp_launch_initial_pages_inner`, and `add_snp_vmsa_mapping`: adopt backing-registration/import and kernel-owned initialization lifetimes. Bind closes initialization access and invalidates aliases. Do not reuse legacy user-VA registration as proof of no pins.
* Runtime `handle_snp_guest_request`: reject before PSP submission unless a verified owned-buffer protocol is negotiated; supported operations need persistent completion owners. Preserve authentication/protocol semantics.
* `handle_snp_ap_create` and doorbell handling: implement or reject typed control-page lifecycles, with page identity/type, replacement/unregistration, VP quiescence, terminal reclaim, and no ordinary payload exemption. Kernel GHCB state remains separate.
* `vm/vmcore/guestmem/src/lib.rs` and `va_mapper.rs`: preserve stage 1G leaf-only access while kernel faults/revocation enforce authorized map state. Reuse owned payload paths and release-error publication behavior unchanged.
* Add phase-two backing/MM, ownership/control, and reclaim tests and its enablement gate below.

Use existing typed guestmem errors, `thiserror` protocol errors, `anyhow::Context` for configuration, and rate-limited guest-triggered diagnostics. No new unsafe device interface. Rust 2024 and `guest_arch` rules apply to both phases.

## 6. Kernel dependencies and teardown boundary

**This section applies to phase two.** Phase 1a retains current backing, registration, acquisition-only access, launch/control behavior, and teardown; phase 1b adds balanced existing host-access calls. Both drain owned backend work before freeing scratch. Neither claims to repair kernel teardown or guarantee allocator return. No new window/drain/PSP capability gates phase 1a or scoped copies.

The companion specifies backing registration, ownership/teardown drain, retained async ownership, and MM export denial. Temporary copies use the existing host-access interface in both plans; guest ownership changes are separate. Verify existing-call ordering/failure behavior on the platform, not a proposed Hyper-V feature. Historical shutdown reports are not allocator-return proof.

The OpenVMM side currently stores `mshv_user_mem_region` with a userspace VA, defers SNP registration until launch, and calls `map_user_memory` after isolation configuration. It also keeps a separate VMSA mapping. This is a real kernel handoff independent of payload I/O. Sources: `vmm_core/virt_mshv/src/lib.rs:273-294,980-999`; `vmm_core/virt_mshv/src/x86_64/snp.rs:604-634,682-711`.

Required kernel answers before enabling `guest_memfd_lite`:

1. What actual feature negotiation/backing registration binds this one fd and offsets to guest GPAs without pinning/locking guest folios? Current userspace registration is not proof of that property.
2. How does the existing modify-host-access path enforce lite object acquisition/release state, fault admission, alias zap, and TLB completion? How are ownership changes excluded from all active/pending grants?
3. What does read-only acquisition mean? Current MSHV code requests both read and write.
4. How are mixed-state/partial range acquisition failures reported and rolled back?
5. What is the behavior when release, unmap, or partition destruction fails? Which component retains the cleanup/retry owner?
6. Are VMAs/folios/hypervisor references returned only after mappings and active users are drained? What proves allocator return rather than just an ioctl success or apparent unmap count?
7. Which control pages, VMSAs, register pages, and partition deposits remain referenced? These are not automatically removed by a bounced payload design.
8. What verified owned-buffer PSP interface preserves the guest request protocol? Until supplied, reject runtime guest requests before the current GPA-based ioctl. What AP VMSA/doorbell exception protocol provides registration/replacement/unregistration, VP quiescence, no pins, terminal reclaim, and death handling?
9. Does capability discovery include immutable MM denial of every enabled non-GUP PFN consumer? GUP rejection, `supports_locking=false`, and userspace device restrictions alone do not satisfy the gate.

### Kernel ownership/teardown drain and asynchronous lifetime

This protocol applies to explicit kernel ownership changes and teardown, **not each copy**. Normal conflicting guest exits are rejected under section 4.2; backend latency never holds a guest grant. Nonconflicting changes remain rejected until separately supported.

Use the companion kernel plan's transaction ID, drain epoch, and generation fields, not an unscoped “drained” boolean:

1. Kernel reserves the durable transition, gates new VP runs, and kicks/drains running VPs. Before ownership or alias changes, it publishes the drain phase and notifies the controller.
2. OpenVMM atomically closes new range windows and device copyback admission. Already admitted eligible copies can still fault their aliases and use their existing root grants; no blanket fault closure or access withdrawal may precede their drain.
3. Drain admitted leaf copies without fault/invalidation locks, mapping/RPC waits, or locks needed by those copies. Queue completed scratch for later or fail/discard it by protocol; it cannot acquire a new window for the draining generation.
4. At zero admitted copies and resolved acquisition/release state, acknowledge transaction ID, epoch, generation, and controller identity. Stale or mismatched acknowledgements fail. Uncertain release stops the partition and prevents success acknowledgement. No kernel copy token is required.
5. Only then may the kernel close faults, drain existing fault handlers, revoke every alias and complete translation/cache barriers, and execute/commit the verified ownership operation. The kernel must still enforce revocation against raw user accesses; the local lease count is coordination, not hardware authority.

Adopt finite controller drain and pending-operation deadlines consistent with the companion's proposed defaults (5 seconds for drain acknowledgement, 30 seconds for pending-operation watchdog; finite administrative limits). These are watchdog/error bounds, not permission to force free or cancel:

* Timeout or interrupted waiter before revocation retains the documented committed state and serviceable old faults until safe abort/recovery; no private commit proceeds without acknowledgement. Keep VPs gated and publish durable status.
* Closing a controller fd while its owner mm/VMA remains alive does not prove copies drained. Only kernel-established owner-mm death/exec and its alias/VP/fault barriers permit the companion's death path; never manufacture a userspace acknowledgement.
* For kernel ownership/control/teardown operations that can remain pending, a kernel partition-wide owner persists after timeout, interruption, or process death. It retains backing, binding, input/output, and completion routing until authoritative terminal results. Existing host-access calls must retain uncertain cleanup too; this is not a new per-copy async ABI.
* Serialize long pending operations per partition. Do not reuse the wire completion slot, partition identity, or buffers on timeout. Unexpected/uncertain completion poisons the partition; late completion reaches the original owner and triggers verified recovery, never automatic VM resume.
* Missing ownership/control completion routing, final-progress semantics, drain liveness, or owner-death behavior disables that lite operation/capability. No legacy ioctl bypass is allowed. Phase 1b uses existing calls and stops on uncertain completion, without requiring a new scoped-window capability.

OpenVMM shutdown order must be:

```text
stop VPs/transitions and new requests
  -> persistent backend/control owners drain or retain submitted operations
  -> stop new queue copies/publication; let admitted leaf copies and faults finish
  -> release windows definitively and acknowledge the exact drain epoch
  -> kernel closes faults, revokes aliases, and performs verified ownership barriers
  -> unregister/drain/reclaim AP VMSA and doorbell exceptions in the required order
  -> unmap guest regions and retire pending completion owners when terminal
  -> close partition/backing resources only after confirmed cleanup
```

The exact hypervisor unmap/access-restoration order is a kernel contract, not a guessed userspace recipe. Keep cleanup state on any failure and do not reuse/free backing while a reference can remain.

Current `MshvPartitionInner::unmap_range` retains its tracked entry when `unmap_user_memory` returns an error, but clears it when that ioctl succeeds (`vmm_core/virt_mshv/src/lib.rs:1024-1046`). If the kernel reports success before cleanup is complete, this userspace tracking alone cannot detect it. The historical report's “unmapped 40,960 pages / VMSA / withdrew deposits” is not allocator-return proof and must not appear as an acceptance result for this feature.

## 7. Tests and measurable acceptance criteria

### Phase-1a acceptance on existing backing and kernel

These tests require no new kernel ABI, no lite object, and no root-window mock to enable the bounce-I/O mode:

1. Exercise direct, `Arc`, nested subrange, multi-region, translated, and remote device views. Denied lock/VA/FD/IOVA exports must remain denied; host-owned scratch stays usable. Prove the legacy MSHV RAM-registration/control path still works.
2. Use storage and network fakes that reject actual guest backing identity, pointers, descriptors, and DMA addresses at payload handoff and on delayed consumption. Observe guest-copy begin/end events for synchronous byte copies, not hypervisor grants: no device request retains a guest slice/copy operation during backend work. Existing root grants and registration pins may remain.
3. Test aligned/page-compatible and scattered block requests in both directions; preserve FUA/flush/discard, exact counts, backend errors, and direct-I/O alignment. Exercise file and block backends on the current kernel.
4. Test delayed/partial network submission, RX copyback, stale/duplicate slot IDs, and TX offload metadata from the same owned snapshot. Include console/RNG/9p/in-process vsock only as their conversions pass.
5. Inject payload/status/ring-copy errors with an ordinary failing guest-memory test backing. Never replay a committed backend write or visible completion; fail the queue and surface publication errors.
6. Test request limits, allocation failure, scratch budget saturation, oversize requests, cancelled budget waits, backend stalls, stop/reset drains, and initialized-length copyback. Verify hard active/retained memory bounds.
7. Reject guest-backing export configurations in the opt-in bounce mode before device mapping/submission. Ordinary non-opt-in configurations keep their existing behavior. Do not require or fake kernel no-GUP/no-PFN-export tests.
8. Run admitted combinations on existing backing, including current `virt_mshv` SNP bring-up where supported. Confirm unchanged registration/control calls and continued rejection of guest attribute transitions. No new guest_memfd or scoped-window ioctl is sent, and missing new capabilities do not prevent bounce-I/O startup.
9. Measure buffer bytes, copy count, latency, throughput, and CPU. Compare ordinary-guest baseline and document that host grants and kernel registration pins are unchanged.

A block-only configuration can pass this gate and ship before network/simple devices. Expand the allowlist per tested combination; do not claim “all I/O” for unconverted devices or existing hypervisor control operations.

### Phase-1b scoped-copy tests and phase-two extensions

Tests 1–12 cover scoped-copy userspace behavior in phase 1b, using existing calls/fakes and phase-1a buffers/queues. Add platform regression tests for the existing calls and startup/initialization grant accounting. Tests 13–17 add kernel ownership/control/MM enforcement and base-page scope in phase two. No new copy-window UAPI is required.

1. Add a fake revocable backing/coordinator with active read/write counters, acquisition/release events, state generations, explicit private/unmapped states, and injectable acquire/copy/release failures.
2. Assert byte/plain/fill/atomic operations balance access, including mapped-success, page-crossing, overlapping, zero-length, and nested-subrange copies. Record exact RAM identity, translated/rounded 4 KiB GPA ranges, mode, generation, and acquire/release counts. Adapters acquire no outer grant; leaf admission happens once. Failed acquisition never copies; failed/uncertain release keeps ranges blocked and stops the partition. No guard spans RPC waits.
3. Test malformed, unmapped, overflowed, ineligible, and transitioning ranges. Race guest attribute admission on other VPs with pending acquisition, copy, and release. Reject a request atomically if any range conflicts; no partial update. Test reverse admission, denied unsupported nonconflicting changes, serialized overlap, and metadata sharing payload pages. Existing calls must not issue per-copy SHARED/PRIVATE transitions or require forced sharing. Successful grants need Hyper-V authorization. Verify actual failure-return/resume if established; otherwise unresolved completion retains halt/stop with no success/replay or fabricated retry status.
   Also race PSP request/response use against acquisition, copy, and release in both admission orders on different VPs; inject failed PSP restoration and control-page registration overlapping payload or metadata. Confirm visible reservations permit conflict handling without waiting for the active ioctl or copy.
4. Test `Arc` and subrange policy propagation. Existing `test_supports_locking` covers direct and multi-region cases but not this whole export contract (`vm/vmcore/guestmem/src/lib.rs:2674-2709`).
5. Test adversarial descriptors: split/packed, indirect cycles/nesting, length truncation, huge descriptors, address overflow during header skips, short header/status capacity, repeated pages, unaligned scatter boundaries, and metadata sharing a page with payload.
6. Extend virtio-blk's existing scatter bounce round trip with **aligned/page-compatible** copy-only requests. Use a backend that asserts its memory is host-owned and observes zero active guest grants before entering and at every backend poll/await.
7. Test read copyback failure after a successful backend read and write status/used-ring failure after a committed backend write. Verify no backend replay, no success completion for failed data copy, and defined queue retirement.
8. Test O_DIRECT alignment, 512/4096-byte sectors, FUA/flush/discard, short backend I/O, pool bounds, requests larger than budget, cancellation while waiting for budget, and stop with outstanding I/O. Existing unaligned tests provide a base (`disk_blockdevice/src/lib.rs:1091-1133`).
9. Test network delayed completion, delayed payload consumption, partial TX segment acceptance, duplicate/stale RxId/TxId, stop/reset, RX copy failure, valid-byte counts, VLAN/checksum/segmentation behavior, and no guest addresses in backend-visible segments. Mutate the guest header/Ethernet bytes between the old peek and payload-copy points; backend metadata must derive solely from the final owned snapshot. Extend existing virtio-net tests; its partial-submit test is currently ignored (`virtio_net/src/tests.rs:1034-1036`).
10. Test in-process vsock socket WouldBlock/partial transfers and owned RX credit bounds. Test console reconnect/cancel progress and 9p/FUSE allocation/reply limits.
11. Test used-ring publication failure at each write/read boundary, including **release failure after a visible split used index or packed used marker**. Distinguish failure before and after publication; verify exactly-once visible completion, no replay/retraction, cursor accounting, interrupt behavior, mandatory partition poison/VP gating, and retained cleanup even when a device suppresses its local error.
12. Negative configuration tests for VFIO/iommufd, kernel vhost, every vhost-user resource kind, DAX/pmem, and persistent VMBus locking. Failure must precede any guest backing export or DMA registration.
13. Exercise an admitted copy faulting an unpopulated eligible alias while drain admission is closed: its fault completes, its release finishes, acknowledgement advances, then post-revocation faults fail. Test wrong/stale acknowledgements, timeout, live-mm fd close, interrupted waiters, owner death, and acquired-but-uncertain grants.
14. Test PSP rejection before submission without the owned-buffer capability; supported PSP paths must show owned input/output and persistent pending ownership. Test AP VMSA/doorbell exception registration, overlap/page type, replacement, VP stop/drain, failed unregister/reclaim, owner death, and no ordinary backend access. Kernel GHCB-state accesses must create no guest backing window.
15. Test capability rejection without no-PFN-export enforcement and independently attempt KVM remapped resolution, VFIO, legacy MSHV MMIO, and each enabled raw-PFN consumer through prefaulted shared aliases. Verify terminal denial after split/remap/alias/protection changes, not only GUP failure.
16. Test pending acquire/release/control operation completion-before-return, delayed/late terminal result, timeout/death, duplicate/unexpected routing, and teardown. Confirm persistent owner references and no slot/partition-ID/buffer reuse. Hardware NVMe/network remain disabled until equivalent terminal completion/reset/drain ownership tests pass.
17. Verify base-4-KiB lite allocation/maps/grants and rejection of unsupported large-page operation without changing unrelated legacy guests. Positive huge-folio, large-grant, and demotion tests are deferred.

Use `test_with_tracing::test` in new unit modules and rate-limited diagnostics for guest-triggered errors. Assertions are for internal invariants, never malformed guest inputs.

### Phase-two kernel-backed/VMM acceptance

These are required future tests, not tests run for this document:

* Boot an SNP guest using one `guest_memfd_lite` backing with allowed block/network/simple virtio devices, and perform sustained bidirectional I/O with split/packed and indirect descriptors.
* Instrument the userspace copy coordinator: active guest-access scopes must be **zero at each backend handoff, await/poll boundary, queued DMA submission, and idle queue wait**. This is per request/thread; another independent copy may be active elsewhere.
* Instrument actual kernel guest backing references/pins: **zero guest backing kernel pins/locks**, including map-by-file, GUP, io_uring, VFIO/iommufd, vhost, backing registration, runtime PSP/AP/doorbell controls, and teardown paths. Physical-device scratch pins and kernel GHCB state are excluded and separately attributed. Ordinary backing lifetime references remain accountable until verified reclaim; do not mislabel them as GUP pins or allocator return.
* Prove enforced non-GUP PFN denial in the supported kernel configuration. Missing enforcement means no public lite-MM capability, not loss of the independent phase-1b existing-call path. User avoidance or skipped tests are not acceptance.
* Attempt attribute changes during queued RX, backend reads, copyback, and ring publication. No grant exists during backend waits. Conflicting guest requests are rejected across pending acquisition through completed release, not queued behind I/O. Nonconflicting unsupported changes stay rejected. Separately supported kernel ownership/teardown drain waits only for bounded live copies.
* Trace ordinary copies: each has a balanced root-grant acquire/release and unchanged committed guest ownership. Transition traces must show serviceable admitted-copy faults before exact epoch acknowledgement, then fault closure/alias revocation and ownership commit.
* Inject acquisition/release/unmap/teardown failures. The system must retain cleanup ownership, fail closed, and provide observable outstanding state. An ioctl returning zero is not sufficient.
* On normal shutdown and faulted shutdown, confirm no active scopes, no guest pins, no remaining mapping/reference owners, and eventual backing allocator return with kernel evidence. Check RAM and VMSA/control pages separately.
* Sample peak active scratch bytes, retained pool bytes, permit wait times, copy/window durations, acquire/release calls per I/O, latency percentiles, throughput, and CPU consumption. Verify hard configured memory bounds under malicious allocation requests and backend stalls.
* Compare non-SNP baseline behavior and performance. The policy must not disable existing zero-copy paths for ordinary guests.

Implementation validation: scoped `cargo check`, `cargo clippy --all-targets`, `cargo doc --no-deps`, and `cargo nextest run --profile agent` for each changed package; `cargo xtask fmt --fix` last. Use the VMM test skill and `cargo xflowey vmm-tests-run` for integration work. No build/test invocation is required for this Markdown-only task.

## 8. Risks, open questions, and launch gate

### Main risks

* More copies and host-access ioctls can dominate small I/O. Batch metadata and contiguous payload copies only within bounded synchronous windows; optimize after proving the lifetime boundary.
* Cross-layer staging can add extra copies, pin scratch unnecessarily, or deadlock budgets. Mark owned buffers and separate CPU scratch and DMA-buffer accounting.
* One policy set too late leaves cached capabilities or exported FDs alive. Select it before construction and disallow runtime switching without a complete drain/rebuild.
* New fallible completion behavior changes queue recovery semantics. Existing logging is not a reliable completion protocol for revocable metadata.
* Existing controllers/backend variants are broader than virtio. An allowlist is safer than a claim that disabling page locking converts all devices.
* Blocking synchronous copy acquisition can harm executor latency. Keep batches bounded and reject inaccessible pages rather than waiting for a guest that requires this same executor to run.
* Snapshot/restart, hotplug, NUMA, existing-backing files, and remote mapping need compatibility decisions for lite backing. Large pages are deferred. Current shared-file restart APIs do not prove new-fd restart support.

### Unresolved claims — do not present as facts

* Lite backing registration, MM capability discovery, no-pin enforcement, and reclaim are not implemented. Temporary acquisition/release uses the existing ioctl; platform tests must validate ordering and failures.
* Whether current kernel MSHV registration still pins particular SNP backing/control pages is not proved here.
* No full guest RAM dirty-tracking/migration scheme for this SNP backing was proved.
* No automatic bounce proxy exists for kernel vhost, vhost-user, or arbitrary assigned devices in the paths read.
* No universal safe cancellation contract was proved for host NVMe DMA or every `DiskIo` backend; the io_uring abort contract is confirmed.
* The exact existing GPA-attribute rejection/completion path is not established by the scoped trace. Until resolved, stop on unsupported/unresolved exits rather than resume or claim clean denial. Existing-call platform ordering/failure tests remain; no new Hyper-V feature or BEGIN/END contract is proposed.
* For phase two, a compatible owned-buffer PSP API and no-pin AP VMSA/doorbell exception lifecycles were not proved. In that phase, those operations must be rejected or separately capability-gated until verified. Phase one preserves existing supported PSP/AP/doorbell behavior and explicitly excludes it from the bounced device-payload guarantee. The kernel GHCB-state mapping is not guest RAM.
* Enforced no-PFN-export coverage, dead-owner/pending-completion recovery, and physical-device terminal drain/reset are capability prerequisites, not properties inferred from userspace bounce branches.
* Large-page support is deferred, not a blocker for the 4 KiB implementation.
* The suggested request cap and pool sizes need measurements and a feature-advertisement decision.
* In-process virtio-fs may be made copy-only without DAX, but its current streaming dispatch is not already an owned-request boundary.

### Phase-1a enablement gate

Enable the explicit bounce-I/O mode for a tested device/backend subset after Stage 1A policy, Stage 1B completion handling, that subset's conversions, bounded scratch/lifetime tests, current-kernel I/O tests, and documentation pass. Keep current registration, acquisition-only host access, launch/control support, and fail-closed SNP transition behavior. Reject configurations that would export actual guest backing for payload I/O.

No lite backing, BEGIN/END token, MM no-pin/no-PFN-export proof, transition handshake, no-registration-pin claim, or allocator-return proof is required for this gate. Those guarantees are absent, not simulated. Hardware payload backends still require demonstrated completion/reset/drain safety for their owned buffers; defer them without blocking software/file/block/network subsets.

### Phase-1b scoped-copy gate

After stage 1G, enable scoped copies on existing backing only after leaf balance, range admission, metadata/publication, release-failure cleanup, and existing-call platform tests pass. Establish the existing failure completion before permitting guest-visible denial/resume; otherwise preserve fail-closed stop. No lite-MM capability, new Hyper-V operation, or kernel token API is required. This gate does not claim Linux mapping revocation or pin-free backing.

### Phase-two isolation enablement gate

Enable lite backing and scoped private-memory isolation only after:

1. Existing host-access calls integrate with lite object/fault/map state and alias/TLB revocation. The kernel supplies one-backing/no-pin/no-PFN-export enforcement, ownership/teardown drain, timeout/death behavior, retained pending-completion ownership, and reclaim/recovery. No copy-window token ABI.
2. Capability propagation/export denial, scoped copies, fallible completion, owned storage/network staging, and allocation limits pass component tests.
3. Every enabled device/backend/control combination has a reviewed coverage row and kernel-backed test evidence. PSP has an owned-buffer protocol or is rejected; AP VMSA/doorbell exceptions have verified no-pin control lifecycles. Host NVMe/physical network remain disabled without confirmed drain/reset ownership.
4. No guest access spans backend work, no guest backing is exported or pinned, and transitions/shutdown pass adversarial tests.
5. The implementation is 4 KiB-only. The Guide states this scope, compatibility restrictions, and measured costs; large pages are a separate follow-up.

Unsupported guest transitions remain rejected in every phase; unresolved denial completion stays stopped. Phases 1a and 1b can ship independently of lite-MM gates, but are not MM enforcement or complete private-memory isolation.

## Review

**History notice — superseded on 2026-10-08:** Earlier BEGIN/END copy-window capability/UAPI, token recovery, phase-two-only scoped copies, and large-page launch requirements no longer apply. The staging review's phase-two-only metadata grant rule is also superseded by phase 1b. Keep the historical findings on owned payloads, leaf-only copies, publication ordering, MM enforcement, and safe cleanup. Earlier verdicts do not approve this revision.

### Initial review

**Verdict: Minor revisions.** The architecture is sound. Enforce copy-only policy at the memory boundary, stage payloads before backend handoff, scope metadata separately, and reject persistent guest mappings. The following localized changes are required before implementation.

1. **Cover runtime SNP control paths.** Add `handle_snp_guest_request`, `handle_snp_ap_create`, and runtime doorbell registration to the matrix and Stage B. PSP payloads need a verified owned-buffer interface, or rejection before submission; the existing GPA-based route is not bounced (`vmm_core/virt_mshv/src/x86_64/snp.rs:1663-1724`; K `drivers/hv/mshv_root_main.c:2536-2577`). AP VMSA and doorbell GPAs need explicit control-page ownership and teardown, not generic payload access (`snp.rs:1605-1637,1777-1818`). The mapped GHCB is kernel VP state, not guest RAM (`snp.rs:195-219`).
2. **Acquire exactly once at the leaf copy.** Propagate policy/origin through adapters, but acquire only after translation and validation at the ultimate guest-backing copy. `Arc` forwards; subranges translate offsets; multi-region views select backing; IOMMU fallbacks translate before the inner copy; RPC callers must not hold a lease while awaiting the remote copy. Current translations call inner guest memory under a translation lock (`vm/devices/iommu/iommu_common/src/lib.rs:198-224,238-281`), and remote fallbacks wait for another component (`workers/chipset_device_worker/src/guestmem.rs:85-101,121-134,158-195`). Test exact leaf ranges/modes/counts, not only final balance.
3. **Align with the revised kernel contract.** Keep committed-shared faults serviceable while blocking new leases and draining copies; revoke only after acknowledgement. Define timeout/process-death handling. Pending operations retain backing/partition/completion ownership; interruption is not cancellation. Failed or uncertain release must poison coordination and fail the partition independently of a device's error handling. Require enforceable rejection of non-GUP PFN consumers, not just GUP tests: KVM has a `follow_pfnmap_start()` fallback (K `virt/kvm/kvm_main.c:2946-2989,3005-3023`). Gate hugepage benefits on verified allocation, ownership granules, and demotion.

**Validated:** storage staging belongs at the common `Disk` boundary as well as virtio-blk; Consomme retains segments and needs owned scratch; io_uring and physical DMA need persistent submitted-operation ownership; completion publication must become fallible without replay.

**Additional refinements:** land fallible queue completion before storage error integration; derive TX offload metadata from the owned snapshot; inject release failure after a visible used marker; keep hardware NVMe/network conditional on confirmed drain/reset ownership.

This source-based review was performed by the `review-plan` agent. Future ABI and hardware results remain enablement prerequisites, not approved assumptions.

### Author response — main-plan revision (historical; token and large-page requirements superseded)

Addressed all three required items and the localized refinements above:

1. Added evidence and coverage rows for runtime PSP guest GPA submission, AP VMSA, doorbell, and kernel GHCB state. Stage B rejects PSP before submission without a verified owned-buffer protocol, and separately gates explicit no-pin control-page lifecycles. GHCB remains kernel VP state, not guest RAM.
2. Replaced blanket fallback acquisition with exact leaf-only admission after translation/backing selection. All adapters propagate policy/origin; IOMMU and RPC forwarding acquire no outer grant. Added exact range/mode/token/count tests and lease-free RPC waits.
3. Aligned section 4.2 with the companion's proposed `MSHV_CAP_LITE_COPY_WINDOWS`, BEGIN/END calls, one-window-per-partition baseline, alias/root revoke behavior, initialization, and distinct guest ownership/root grants. Copies never perform SHARED/PRIVATE ownership churn. Real ABI and Hyper-V semantics remain unresolved enablement gates.
4. Specified serviceable admitted-copy faults during drain, exact transaction/epoch acknowledgement, finite timeout/death handling, and END/recovery admission before transition wire-slot ownership. Added persistent pending owners, mandatory partition poison on failed/uncertain release, enforced non-GUP PFN denial, and separate hugepage gates.
5. Added Stage B.1 fallible queue publication before storage/network error integration. TX metadata now derives from the same owned snapshot. Tests include failed release after a visible used marker with exactly-once publication and partition poison. Host NVMe/physical network remain conditional on confirmed terminal drain/reset ownership.

The main plan now contains these changes; the initial review text is preserved. No implementation, kernel capability, or hardware result is claimed. Only this OpenVMM plan was edited; focused re-review remains for the parent to request.

### Final confirmation (superseded approval)

**Verdict: Ready at plan-review level.** The reviewer confirmed that all original must-fix items and localized refinements are resolved. No residual must-fix plan changes remain.

The added section 4.2 rules provide owner-authorized recovery of copy-window tokens and terminal results after failed delivery, with idempotent request correlation and explicit interrupted-end tests. Unbound initialization windows serialize per object; bind closes admission, drains windows, invalidates aliases, establishes the runtime root baseline, and transfers admission to the partition. Failed binding reopens initialization only after confirmed wholly unbound host-owned rollback; otherwise it quarantines the object.

This approves the plan's structure and safeguards, not the future kernel ABI, MM enforcement, Hyper-V semantics, or hardware behavior. Those remain explicit proof and enablement gates. Initial review and author responses above are retained as history.

### Staging revision — 2026-10-08

At the user's request, separated kernel-independent bounce I/O from enforced lite isolation. Stages 1A/1B precede independently deliverable storage, simple-device, and network conversions; Stage 2A now contains backing/scoped-access/transition/control integration. Each phase has a separate acceptance and enablement gate. Phase one preserves legacy registration, host grants, control behavior, and SNP transition rejection, and explicitly makes no kernel no-pin or private-memory claim. This revision awaits plan review; the earlier approval is retained as history.

### Staging review (historical; scoped-access staging superseded)

**Verdict: Minor revisions.** Kernel-independent bounce I/O is correctly separated from lite isolation, and the companion delivery-order note is consistent. The reviewer requested three localized corrections: qualify the metadata grant/coordinator rule as phase-two-only, move shared virtio queue policy/validation into Stage 1A rather than optional Stage 1D, and qualify the PSP/AP/doorbell rejection rule as phase-two-only. All three are incorporated in the main plan.

The reviewed order preserves current registration, acquisition-only host access, supported control behavior, and fail-closed SNP transitions while allowing a tested block-only subset to ship first. Phase-two MM/window/control/reclaim gates and applicable owned-buffer completion/drain requirements remain intact. This review assessed the staging text, not source execution or hardware results.

### Author response — existing access, conflict denial, and 4 KiB scope, 2026-10-08

Replaced the proposed BEGIN/END/token dependency with existing host-access acquire/copy/release. Phase 1a still ships bounce-first; phase 1b scopes leaf payload and metadata copies independently of lite backing. Completed backend read/RX copies back only after acquisition; write/TX copies out and releases before submission. One coordinator covers pending acquisition through completed release with 4 KiB GPA ranges and RAM identity. Any conflicting guest attribute request is rejected as a whole across VPs; uncertain release blocks ranges and stops the partition with retained cleanup.

The narrow trace does not establish the existing GPA-attribute failure-completion/resume mechanism. TripleFault is stopping, not clean denial; preserve halt/stop until the actual protocol is established. No new hypervisor interface is requested. Kernel MM/private revocation and safe reclaim stay in phase two. Large pages are deferred, without deferring direct-map safety or changing legacy superpage support.

**Review pending.** No implementation, build, or hardware result is claimed.

### Existing-interface revision review

**Verdict: Minor revisions; corrections incorporated.** The existing acquire/release interface is sufficient for the planned scoped-copy implementation; no new Hyper-V operation or copy-token ioctl is required. Both I/O directions, metadata, failed-release quarantine, bounce-first staging, and deferred large pages are consistent.

The reviewer required admission coordination for existing PSP/AP/doorbell page use in phase 1b, not only copies and attribute exits. The main plan now reserves those pages before side effects and keeps exclusions through the required control lifetime. The coordinator mutex is held only for admission/state updates, not ioctls or copying. Added multi-VP PSP/copy races, restoration failures, and control-page overlap tests.

The companion kernel plan now drains confirmed eligible grants independently of guest ownership labels. This preserves fault liveness and MM revocation without assuming SHARED is required for every authorized grant. Exact clean GPA-attribute denial/resume remains an existing-protocol integration question; unresolved exits remain stopped. This review assessed the plans and targeted source paths, not hardware execution.
