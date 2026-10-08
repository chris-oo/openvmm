# MSHV SNP guest_memfd_lite: kernel implementation plan

**Status:** Revised on 2026-10-08 and reviewed. Review reached **Minor revisions**; those corrections are incorporated. Earlier token proposals are superseded where marked in Review. Use existing modify-host-access calls for temporary copies; no new Hyper-V ABI or BEGIN/END copy-window ioctl. Lite backing/MM enforcement and ownership/reclaim gates remain.

**Scope:** Kernel backing and enforcement, with only the OpenVMM coordination contract. This is not the separate OpenVMM implementation plan. No source changes, builds, commits, or remote writes are part of this task.

Companion: [OpenVMM copy-only I/O plan](plan-openvmm-snp-bounce-io.md).

**Delivery order:** OpenVMM phase 1a can ship bounce-first on current backing with acquisition-only access and fail-closed intercepts; it leaves controls unchanged. Phase 1b adds scoped acquire/copy/release and conflict denial using existing host-access calls, independent of lite-MM capability. It preserves PSP/AP/doorbell interfaces but coordinates their conflicting page use with copies before side effects. The kernel work here gates phase-two backing/MM isolation, not either userspace stage. Ordinary bounce I/O does not redesign control pages.

Sources were read from these local working trees on 2026-10-07:

- **K:** `/home/coo/ai/leafeon/LSG-linux-rolling`
- **O:** `/home/coo/ai/leafeon/openvmm`

All `K/path:line` and `O/path:line` references below use these roots. Line numbers describe the inspected working trees, not an upstream release. Proposed functions and ioctl names are explicitly marked as proposed.

Existing user changes in the Guide CLI page and `virt_mshv`'s `mod.rs`/`snp.rs` were read as current working-tree evidence and left unchanged. The suggested root artifacts `research-mshv-snp-teardown.md` and `mshv-snp-unmap-first.patch` were not present in O when checked. Prior-session shutdown counts supplied by the parent are context, not allocator-return or recovery proof; this plan relies on the current code for teardown conclusions.

## 1. Goal and hard limits

Provide one kernel-owned backing object for SNP guest RAM. The same pages serve a GPA range across supported ownership states. Linux mappings are usable only under successful, authorized host access; no usable mapping may survive private commit or host-access revocation. Do not assume every temporary grant requires guest SHARED state or exposes private plaintext. Do not register/pin ordinary userspace pages as RAM. Ordinary device I/O uses owned bounce memory; control-page registration has a separate typed lifecycle.

“Lite” means a small supported operation set, not weaker ownership or transition rules:

- No `pin_user_pages*`, `get_user_pages*`, `VM_LOCKED`, or long-lived folio lock on guest backing.
- Retain backing ownership and ordinary allocation/file references until the hypervisor releases the pages. These are unavoidable lifetime references, not GUP pins.
- Temporary mutexes, page-table locks, allocation locks, and TLB synchronization are unavoidable. If “no kernel locking” also forbids these, the requested interface is impossible.
- Active private backing cannot be ordinary reclaimable anonymous memory. Initially the object is non-swappable, non-migratable, and unevictable. This retains resident RAM without `mlock`; it is still a resident-memory commitment and must be charged and limited. If the requirement forbids that commitment, private-memory eviction needs a separate hypervisor export/reclaim protocol and is outside this lite API.

Non-goals for the first implementation:

- Importing arbitrary anonymous/shmem/hugetlb user allocations or converting existing pinned regions.
- DMA passthrough, zero-copy kernel I/O, live migration, swap, KSM, transparent host huge mappings, arbitrary truncation, or guest-controlled hole punching.
- Protecting against a malicious host kernel. Trusted kernel code with a PFN can deliberately create a new mapping. Removing normal mappings and blocking GUP prevents supported access paths; it does not make arbitrary kernel remapping impossible.
- Promising a specific host exception or crash if existing private pages are accessed. The inspected MSHV sources do not establish such a mechanism.

The first implementation uses order-0 backing, 4 KiB guest mappings, and 4 KiB host-access requests. Large folios, guest mappings/grants, mixed 2 MiB state, and demotion are deferred to section 7. Ordinary direct-map removal and necessary page-table splitting remain MM safety requirements.

## 2. Direct answer: what MSHV enforces today

**Current MSHV does not provide a Linux-MM guarantee that SNP-private guest pages cannot be pinned or accessed through Linux mappings. It deliberately pins SNP RAM first and then asks Hyper-V to revoke root access.** Linux backing lifetime and hypervisor access control are different mechanisms.

Evidence:

1. `mshv_do_pt_regions_pinned()` selects the pinned path for encrypted partitions, and `mshv_partition_create_region()` uses that decision: `K/drivers/hv/mshv_root_main.c:1685-1724`. The SNP check itself is in `K/drivers/hv/mshv_root.h:270-273`.
2. `mshv_prepare_pinned_region()` calls `mshv_region_pin()` before `mshv_region_unshare()` and `mshv_region_map()`: `K/drivers/hv/mshv_root_main.c:1746-1803`. The independent implementation calls `pin_user_pages_fast(FOLL_WRITE | FOLL_LONGTERM)`: `K/drivers/hv/mshv_regions.c:292-326`. Existing pins therefore remain while root access is released.
3. `mshv_region_unshare()` sends `MAKE_EXCLUSIVE` with zero host permissions through the release hypercall; `mshv_region_share()` sends `MAKE_SHARED` with read/write permissions through acquire: `K/drivers/hv/mshv_regions.c:202-245`. The wrapper selects `HVCALL_{ACQUIRE,RELEASE}_SPARSE_SPA_PAGE_HOST_ACCESS` and includes the partition ID for exclusive requests: `K/drivers/hv/mshv_root_hv_call.c:1151-1215`. The corresponding ABI definitions are `K/include/hyperv/hvhdk.h:1021-1033`.
4. The prepare call site describes this as releasing host SLAT access: `K/drivers/hv/mshv_root_main.c:1758-1768`. None of these inspected functions zaps userspace PTEs, changes Linux direct-map PTEs, marks a no-GUP memory type, or installs a private-aware userspace fault handler.
5. Generic slow GUP rejects `VM_IO | VM_PFNMAP` and secretmem VMAs, not MSHV SNP regions: `K/mm/gup.c:1200-1220`. Fast GUP checks PTE permissions and special PTEs before grabbing a folio: `K/mm/gup.c:2840-2878`. Its file-folio policy has a secretmem check, with an order-0 assumption, rather than an MSHV check: `K/mm/gup.c:2721-2807`. These are independent MM implementations, not enforcement supplied by MSHV.

Thus:

| Question | Supported conclusion from these sources |
| --- | --- |
| Does MSHV retain pins on SNP guest RAM? | Yes, by design in the current registration path. |
| Does MSHV block new GUP pins because a page is SNP-private? | No MSHV-specific MM rejection is installed by the inspected path. Hardware root-access restrictions are not GUP-policy enforcement. This is not a claim that every attempted GUP operation always succeeds. |
| Does MSHV remove userspace VA mappings of private pages? | Not in the inspected SNP registration or host-access paths. Hyper-V can revoke the physical/root access behind those mappings. |
| Does MSHV remove Linux direct-map aliases? | Not in these paths. |
| Can the host read guest-private plaintext? | This cannot be inferred from a present Linux PTE or retained pin. Root SLAT restrictions and SNP ownership/validation are separate enforcement layers. |
| Will a particular host access panic, fault, or terminate the root? | Not established here. The driver contains warnings about crash risk, not a demonstrated exception-handling path or test proving the outcome. |

Do not equate `MAKE_EXCLUSIVE`, a root host-access grant, the guest C-bit view, and validated guest ownership in the RMP. The Hyper-V implementation is not in this tree. Its precise RMP and cache/TLB effects cannot be proved from a thin Linux hypercall wrapper.

There is relevant **different** prior art: native SNP host code explicitly splits overlapping large direct mappings before RMP updates, with a comment describing RMP faults in that environment: `K/arch/x86/virt/svm/sev.c:884-953`, called by its RMP-update implementation at `:980-994`. This is not evidence that MSHV root accesses fail by the same mechanism.

### Other current paths that matter

- `MSHV_MODIFY_GPA_HOST_ACCESS` translates GPAs to stored `struct page *` values and issues the access hypercall. It has no per-page Linux visibility state or PTE revocation: `K/drivers/hv/mshv_root_main.c:2415-2465`; lookup is `:688-718`.
- Partition ioctls use `pt_mutex`, but VP ioctls use individual `vp_mutex` locks: `K/drivers/hv/mshv_root_main.c:2615-2622`, `:1322-1374`. This does not serialize all vCPUs with a partition memory transition.
- The kernel handles movable GPA faults and MMIO faults, not GPA-attribute transitions in its intercept switch: `K/drivers/hv/mshv_root_main.c:859-886`. Its movable HMM/notifier path remaps guest pages `NO_ACCESS` when Linux invalidates backing: `K/drivers/hv/mshv_regions.c:414-437`, `:448-495`, `:558-627`. SNP does not use that movable path.
- VP PFN state transfer pins user buffers separately from RAM registration: `K/drivers/hv/mshv_root_main.c:903-957`, called at `:1046-1053`. This needs a bounce conversion too.
- MSHV also resolves raw PFNs from special mappings for MMIO with `follow_pfnmap_start()`: `K/drivers/hv/mshv_root_main.c:728-757`. A lite mapping must never be accepted here as device MMIO.
- PSP guest requests release host access to request/response GPA pages and reacquire it on some failures: `K/drivers/hv/mshv_root_main.c:2536-2577`. This is another page-access transition, not a generic userspace-buffer pin, and must join the new state machine or be unsupported in lite mode.
- GPA read/write ioctls pass inline data through a hypercall: `K/drivers/hv/mshv_root_main.c:1264-1317`. They must not become an alternate way to bypass the lite private-access policy.
- The VFIO bridge retains VFIO file references: `K/drivers/hv/mshv_vfio.c:46-86`. Creating the bridge forces pinned regions: `K/drivers/hv/mshv_root_main.c:2020-2021`. The bridge is not proof that the whole VFIO/IOMMU pin stack is safe for lite RAM.

## 3. Verified MM prior art and design choice

### Current guest_memfd is useful, but is not already the required integrated SNP API

The local `K/virt/kvm/guest_memfd.c` supplies these useful building blocks:

- A dedicated inode/page-cache backing, immutable size, controlled binding, and invalidation: `:185-205`, `:479-555`, `:579-647`.
- An inaccessible, unevictable address space: `:543-548`. `mapping_set_inaccessible()` sets both `AS_INACCESSIBLE` and `AS_UNEVICTABLE`: `K/include/linux/pagemap.h:325-338`.
- MM avoids migrating an inaccessible mapping and avoids partial-truncation zeroing of it: `K/mm/migrate.c:1081-1096`, `K/mm/truncate.c:239-246`. **These flags are not a blanket no-GUP or no-mapping policy.**
- Guest mapping invalidation and remote TLB flush before truncation: `K/virt/kvm/guest_memfd.c:117-149`, `:185-203`.
- Architecture preparation and free-time ownership invalidation: `:28-42`, `:458-477`. Native SNP implementations inspect/update RMP entries, handle partial large-entry reclaim, and flush caches: `K/arch/x86/kvm/svm/sev.c:4960-5069`; dispatch is `K/arch/x86/kvm/x86.c:14043-14054`.

Limits in this exact tree:

- Allocation still uses order-0 `filemap_grab_folio()` with a huge-page TODO: `K/virt/kvm/guest_memfd.c:100-110`. The user fault handler rejects large folios: `:374-377`.
- Mmap faults depend on a file-wide `INIT_SHARED` flag, not a per-page shared/private transition state: `:349-392`.
- The x86 capability explicitly rejects initial shared backing for VMs with private memory: `K/arch/x86/kvm/x86.c:14032-14040`.
- Its independent selftest checks read/write rejection, MAP_PRIVATE rejection, private faults, and shared faults/hole punching: `K/tools/testing/selftests/kvm/guest_memfd_test.c:27-113`, `:272-295`. This is not a test proving the desired integrated SNP transitions.

Do not copy KVM-internal bindings into MSHV or call native `rmp_make_private()` from a Hyper-V root. Hyper-V owns the MSHV isolation protocol.

### Proposed narrow lite backing

Use a dedicated MSHV backing inode with owned folios and **special base-page PFNMAP user PTEs**, not ordinary anonymous memory or ordinary page-returning file faults.

This deliberately trades ordinary MM services for a small provable API:

1. Accept only `MAP_SHARED`, non-executable mappings of this object.
2. Set `VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP | VM_DONTCOPY` plus the proposed, mandatory `VM_NO_PFN_EXPORT` MM restriction described below; do not set `VM_LOCKED`.
3. The fault callback checks object identity, committed ownership, confirmed authorized host access, permissions, and revocation phase under a shared state/invalidation gate held through PTE insertion. Only a page admitted by the existing host-access path may receive `vmf_insert_pfn_prot()`. Revocation closes faults, zaps all aliases, and finishes TLB barriers. Ungranted, ineligible, quarantined, detached, and out-of-bounds faults return `SIGBUS`. Do not infer permission from a SHARED flag alone or allow unsupported private access. Old admitted faults remain serviceable during ownership drain, before revocation; no fault waits for userspace acknowledgement under this gate.
4. Use only order-0 folios and special 4 KiB PTEs. Do not switch to a normal `vmf->page` fault.
5. Keep the backing folio owned by the inode independently of user PTE references. Special PTEs do not provide ordinary page lifetime/rmap accounting; explicit allocation charges and object lifetime are mandatory.
6. Remove the direct-map alias for **the whole lifetime of each exposed backing folio**, shared as well as private. Allocate and zero before removal. Restore only after confirmed reclaim, then scrub and free. The driver never maps private contents with `kmap`/`vmap`.
7. Supply no `vm_ops->access`, no file read/write/splice operations, no user-supplied PFN import, and no normal-MM alias path.

Why this route rather than just dropping `FOLL_PIN`:

- Slow GUP rejects PFNMAP VMAs irrespective of read/write, `FOLL_FORCE`, or long-term status: `K/mm/gup.c:1200-1208`.
- `vmf_insert_pfn_prot()` checks the PFNMAP/non-COW contract, and `insert_pfn()` creates a special PTE: `K/mm/memory.c:2669-2692`, `:2585-2628`. Fast PTE GUP rejects that special entry before `try_grab_folio_fast()`: `K/mm/gup.c:2868-2875`. This covers both `FOLL_GET` and `FOLL_PIN`, not just long-term pins.
- PMD/PUD fast GUP checks special entries too, but these are not permission to implement huge host PTEs without further review: `K/mm/gup.c:2935-3008`. No host huge-PTE implementation is proposed initially.
- Remote access falls back to a special VMA's `.access` callback when GUP fails; leave it absent: `K/mm/memory.c:6865-6909`.
- Standard mapping invalidation clears non-normal PTEs and records TLB invalidation: `K/mm/memory.c:1685-1711`; file invalidation walks all attached mappings: `:4233-4285`; zap completion uses `tlb_finish_mmu()`: `:2170-2188`. Use these paths, not a hand-written local PTE update.

The design still needs MM review for normal owned RAM exposed with this special-mapping contract, cache attributes, lifetime accounting, and exact fault/invalidation lock order. `vmf_insert_pfn_prot()` is driver-mapping prior art, not an existing guest_memfd implementation. If MM maintainers reject this use, choose the integrated guest_memfd route with an explicit no-GUP/no-PFN-export memory type and full slow/fast/huge-path enforcement. Do not silently use normal page faults as a fallback.

### Mandatory exclusion of non-GUP PFN consumers

PFNMAP alone is insufficient. KVM falls back from GUP failure to `hva_to_pfn_remapped()`, which calls `follow_pfnmap_start()` and can fault the mapping in before retrying: `K/virt/kvm/kvm_main.c:2946-2989`, `:2992-3023`. VFIO type1 independently uses the same fault/lookup sequence: `K/drivers/vfio/vfio_iommu_type1.c:540-575`. These facts establish a PFN-export path, not proof of a private-access exploit: notifier synchronization and lifetime rules still matter.

**Baseline choice: require a core-MM no-PFN-export restriction, not a documented unsupported-consumer list.** Proposed kernel changes, subject to MM review:

1. Add a permanent, kernel-only VMA restriction named provisionally `VM_NO_PFN_EXPORT`, with an MM predicate such as `vma_allows_pfn_export()`. The name and representation need MM approval; do not assign a flag bit in this plan. Lite mmap sets the restriction before exposure. VMA splitting, relocation, protection changes, and aliases preserve it; userspace cannot clear it.
2. In `K/mm/memory.c:follow_pfnmap_start()`, reject a restricted VMA with `-EOPNOTSUPP` **before** looking up or returning any PFN, for shared and private states alike. The present entry point checks only address bounds and IO/PFNMAP type before walking page tables (`:6685-6704`). Internal lite faults already know their owned PFNs and must not receive an export bypass.
3. Add an early predicate check in KVM's `hva_to_pfn_remapped()` and VFIO's `follow_fault_pfn()` before `fixup_user_fault()`. This makes denial terminal, avoids needless refaults, and never calls KVM's PFN resolver for lite backing. The core check remains the enforcement point for every caller, regardless of these convenience checks.
4. Verify every exported/raw-PFN lookup entry point and direct page-table consumer enabled in the target kernel. Route user-memory PFN lookup through this restriction or add equivalent backing-type rejection. The inspected additional callers include `generic_access_phys()` (`K/mm/memory.c:6814-6828`), legacy MSHV MMIO, and ACRN (`K/drivers/virt/acrn/mm.c:185-200`); the core check covers these lookup calls. Absence of `.access` remains a second remote-access barrier.
5. Require rejection tests for KVM and VFIO independently of MSHV partition mode. A process must not export a lite shared alias into another VM or an IOMMU mapping through a separate fd. Test any tunable that relaxes KVM's ordinary unsafe-remapped-memory policy; it must not override this restriction.

Feature gates: even the publicly usable shared-only prototype depends on this MM restriction. Without it, the driver must return unsupported for lite-fd creation/mmap and advertise no lite capability. Private mode also requires the transition/async gates. An internal unexposed allocation test is permitted, not a release that asks users to avoid KVM. If the complete enabled-consumer audit finds a path that cannot honor the restriction, disable that consumer in an enforced supported build/deployment configuration or withhold the capability; merely labelling it unsupported is not acceptable. Arbitrary deliberately malicious kernel mappings remain outside the trusted-kernel threat model.

Secretmem supplies independent evidence for direct-map removal and no-GUP design:

- It removes direct mappings, flushes kernel TLBs, and rolls back insertion failure: `K/mm/secretmem.c:50-111`.
- It blocks migration and restores the direct map before scrubbing at free: `:147-163`.
- Its tests check prefaulted `vmsplice`, remote process access, and ptrace: `K/tools/testing/selftests/mm/memfd_secret.c:88-233`.

**Do not simply reuse secretmem.** It sets `VM_LOCKED` and applies memlock limits (`K/mm/secretmem.c:121-133`), and the fast-GUP secretmem predicate assumes order-0 folios (`K/mm/gup.c:2755-2757`). Those choices do not satisfy this request.

### What this does not prohibit

Accessible shared user PTEs can be used by ordinary same-process kernel `copy_from_user()`/`copy_to_user()`. GUP denial does not prohibit every synchronous uaccess. The kernel and userspace also share the physical memory bus. An absolute “the kernel can never access even shared RAM” guarantee is incompatible with ordinary shared VA access without much broader architecture/uaccess changes.

The supported contract is: no kernel/device retention or pin of guest backing, and all supported VMM I/O uses separate buffers. Private commit drains and revokes all preceding grants and usable Linux aliases. Ungranted or ineligible backing has no usable user mapping. Any subsequently supported mapping requires fresh confirmed authorization under the verified ownership/access policy; a SHARED label, present PTE, or assumed private-plaintext capability is insufficient. Direct-map aliases remain absent throughout the exposed backing lifetime. Enforce this through GUP denial, mandatory MM PFN-export restriction, MSHV controls, and OpenVMM coordination. PFNMAP alone does not block every kernel API. Intentional privileged physical remapping remains outside this contract.

## 4. Proposed API and invariants

Add a capability-gated, opt-in memory mode. Keep legacy registration unchanged for non-lite partitions; do not mix legacy RAM and lite RAM in one lite SNP partition.

Proposed UAPI in `K/include/uapi/linux/mshv.h`:

- `MSHV_CREATE_GUEST_MEMFD_LITE { size, flags, resident_limit, reserved } -> fd`. Fixed aligned size; initial pages shared and zeroed. Bind it to exactly one lite partition and one mapping-owner `mm`. Creation/reservation charges are bounded. Allow unbound shared initialization, but no guest use before binding.
- `MSHV_BIND_GUEST_MEMFD_LITE { fd, offset, guest_pfn, size, permissions, reserved }`. Registration refers to an object offset, never a userspace VA. Reject GPA/object overlap, overflow, invalid flags, and other-partition binding.
- Retain existing `MSHV_MODIFY_GPA_HOST_ACCESS` for temporary acquisition/release; integrate it with lite object/map state as specified below. No BEGIN/END copy-window ioctls or copy-window capability.
- `MSHV_TRANSITION_GUEST_MEMORY { offset, size, expected_generation, target, transaction_id, reserved }`. Target is shared or private. Check expected state/generation; do not treat a request to acquire root access as proof the guest page is shared. Begin returns a durable transaction ID and drain epoch without waiting under the partition/object locks. Only the controller for the bound owner `mm` may acknowledge its drain.
- Proposed `MSHV_ACK_GUEST_MEMORY_DRAIN { transaction_id, drain_epoch, expected_generation, reserved }`. This acknowledges that the VMM closed lease/device-copy admission and drained all existing range leases. The epoch prevents a delayed acknowledgement from completing a different drain. It does not authorize freeing pages or override kernel revocation barriers.
- `MSHV_QUERY_GUEST_MEMORY_STATE { bounded range, ... }`. Return committed state, drain/revocation phase, generation, in-progress/failed transaction, completed subranges, and async status. A separate bounded/interruptible wait or poll operation may wait for progress outside driver locks. Query permits recovery after interrupted calls or output-copy failure.
- Query may report active/pending host-access ranges and uncertain cleanup against object identity/generation. Do not add per-copy tokens or idempotent BEGIN/END request APIs; retained kernel state must prevent lost/uncertain results from authorizing a copy or premature backing release.
- Use the existing guest intercept failure/completion mechanism once established. The exact GPA-attribute rejection/resume path is an implementation question, not a requirement for a new Hyper-V operation or kernel-issued copy token. Keep unresolved requests stopped.
- A separate unbind/destroy operation. No generic truncation, resize, hole punching, or backing-fd read/write.

The exact ioctl numbers and binary layout are not assigned in this research draft. Specify fixed-width fields, MBZ validation, checked arithmetic, compatibility rules, bounded lists, and failure semantics before implementing.

States: `SHARED`, `TO_PRIVATE`, `PRIVATE`, `TO_SHARED`, `QUARANTINED`, and `DETACHED`. Track at least:

- Object allocation identity and backing offset; GPA binding.
- Committed guest ownership/visibility versus separately observed root-access state.
- Independent host-access state (`NONE`, `ACQUIRING`, `GRANTED`, `RELEASING`, `UNCERTAIN`), normalized 4 KiB ranges, permissions, object/RAM identity, and retained cleanup. `SHARED` alone is not a grant.
- Generation, transition ID, and completed hypercall subranges; independent copy-admission phase (`OPEN`, `DRAINING`, `REVOKING`) and drain epoch. `DRAINING` preserves current committed ownership and fault service for already confirmed eligible grants while refusing new acquisitions. Neither SHARED nor PRIVATE alone proves authorization or absence of pending/active grants.
- Order-0 backing identity and 4 KiB mapping granularity.
- A partition-wide async operation owner, submission/completion phase, final payload, bounded retry/progress ledger, and retained references, as specified in section 6.
- No content pointers in persistent kernel bookkeeping.

Required invariants:

1. A user PTE is installed only after object eligibility, successful authorized host access, and range/permission coverage are confirmed. Do not silently change guest ownership to make it eligible.
2. No user PTE or direct-map alias can survive the commit to private.
3. No GUP-acquired guest page reference, kernel I/O request, or DMA mapping can survive or be created across either transition. Only the backing object's own lifetime references exist.
4. Kernel code does not zero/copy private contents, migrate/free backing, or return it to the allocator until confirmed reclaim to host-safe ownership. Temporary userspace access is a separate authorized grant, not reclamation or proof of private plaintext availability.
5. One object offset has one guest ownership context; no legacy RAM/MMIO registration or unchecked host-access bypass. The existing ioctl is the enforced temporary-access path, not an excluded alternative.
6. Hypercall success is not published as committed state until all relevant Linux and guest translation/cache barriers complete.
7. Uncertain ownership is quarantined. Never unpin/free optimistically.
8. Reallocation creates zeroed shared memory with a new generation. A stale bounce completion cannot write into a newer ownership generation.

OpenVMM tracks pending acquisition through completed release under one range coordinator; these records are **not kernel page pins**. Normal guest changes conflicting with these ranges are denied, not queued behind backend I/O. Explicit kernel ownership/teardown drain may close new admission, let old authorized copies/faults finish, and require a range/epoch acknowledgement. No copy may reacquire a draining generation. Kernel alias/TLB revocation, MM export denial, and ownership-change VP gating remain mandatory regardless of userspace claims.

### Existing modify-host-access enforcement

Temporary root access is not guest ownership transfer. The existing ioctl maps GPAs to pages, derives acquisition from `ACQUIRE`, initializes hypercall flags to zero, and adds only `LARGE_PAGE` when requested (`K/drivers/hv/mshv_root_main.c:2415-2468`). It calls the existing SPA host-access wrapper. The PSP path independently releases with flags zero and reacquires on an error (`:2557-2577`); ownership-changing `MAKE_*` flags belong to explicit share/unshare (`K/drivers/hv/mshv_regions.c:198-243`; flag definitions `K/include/hyperv/hvhdk.h:1021-1033`). No new Hyper-V operation or kernel window-token API is needed.

Integrate the **existing** path with lite object/map state:

1. Resolve each GPA to its bound owned object and checked 4 KiB range. Validate owner mm, binding identity/generation, permissions, page type, and no conflicting ownership/control operation. Reserve acquisition state before the call. Reject unsupported large-page requests in lite mode; preserve legacy behavior separately.
2. Record confirmed acquisition only after existing Hyper-V authorization succeeds. Failed or pending acquisition grants no userspace copy permission. Do not force SHARED ownership first or assume that arbitrary private plaintext can be granted. Ineligible pages fail closed; supported mapping eligibility must reflect actual authorization and object policy.
3. Allow faults only within confirmed accessible page ranges and permissions, holding the state gate through special-PTE insertion. All aliases follow the same admission; no accessible PTE outside authorized access.
4. OpenVMM serializes synchronous leaf copies from acquisition through completed release. Backend read/RX finishes into owned memory **before** acquisition/copyback; write/TX copies out and releases **before** submission. Queue/status/ring/input copies obey the same rule. No grant spans backend/RPC waits or task yields.
5. On release (clear `ACQUIRE`), close fault admission, drain fault insertion, zap covered user PTEs, and finish CPU TLB shootdowns before completing revocation through the existing host-access call. Retire state only after definite success. Guest ownership/generation does not change per copy. A private commit likewise cannot retain any accessible Linux alias. Test the ordering on the existing platform rather than requiring a new feature.
6. Failed or uncertain release leaves aliases absent and the affected range blocked; stop the partition and retain backing/cleanup ownership until verified recovery. Do not report success, reacquire blindly, free backing, or allow a conflicting ownership operation. Retained state/query may diagnose uncertainty; it is not a per-copy token capability.
7. Unbound initialization uses kernel-owned host-safe backing under creation-time owner-mm control, not a Hyper-V copy grant. Bind closes initialization admission, drains access, invalidates aliases, and transfers lifetime/admission to the partition. Account for or retire launch/initialization host grants before scoped runtime admission; no untracked persistent grant may bypass the lifecycle. Reopen only after wholly host-owned rollback; quarantine uncertainty. No special BEGIN/END initialization UAPI is required.
8. Explicit ownership/teardown drain lets already admitted authorized faults/releases finish before revocation. Do not hold the ownership operation's completion slot or locks while waiting for them. Whole-VM stops and async transaction owners below apply to those operations, not ordinary copy grants.

OpenVMM's simple coordinator reserves normalized GPA ranges and RAM identity during **pending acquisition, copy, release, and uncertainty**. All VPs use the same admission lock. Reject an intercepted ownership/visibility/access request atomically if any requested range conflicts; do not partially apply it or wait on backend I/O. An admitted guest change reserves its ranges before a racing copy can acquire. Unsupported nonconflicting transitions remain rejected.

**Denial completion question:** the message contains VP index, flags, and 29 ranges, not a completion/result field (`O/vm/hv1/hvdef/src/lib.rs:4335-4358`), and SNP dispatch calls the halt handler (`O/vmm_core/virt_mshv/src/x86_64/snp.rs:1133-1134`). Current `TripleFault` is fail-closed stopping, not a clean guest failure. The inspected register-intercept-result ABI contains CPUID/MSR results (`K/include/hyperv/hvhdk.h:1093-1125`; caller `K/drivers/hv/mshv_root_main.c:1217-1229`), not a demonstrated GPA-attribute reply. `CompletePartitionIntercept` appears only as a property definition in the scoped Rust trace (`O/vm/hv1/hvdef/src/lib.rs:515`). GHCB error writing (`O/vmm_core/virt_mshv/src/x86_64/snp.rs:368-372`) belongs to the separate VMGEXIT page handler (`:1404-1418`); it is not evidence for this message's return.

Establish the exact **existing** failure-completion/resume mechanism before resuming denied exits. Until then retain fail-closed halt/stop; never fabricate success, replay, or an error status. Guest retry is permitted only by the verified return protocol. This is a narrow implementation question, not a new Hyper-V-interface requirement.

AP VMSA and doorbell registration must use typed, long-lived **hypervisor control references**, not ordinary copy windows or blanket payload exemptions. Current AP creation passes a VMSA GPA into SEV control (`K/drivers/hv/mshv_root_main.c:2361-2401`; caller `O/vmm_core/virt_mshv/src/x86_64/snp.rs:1813-1818`). Current doorbell setup validates a GPA and writes `SevDoorbellGpa` (`O/vmm_core/virt_mshv/src/x86_64/snp.rs:1615-1635`). Neither call site proves that userspace should acquire a persistent root grant. Require verified control-page type/ownership, bounds, alignment, no overlapping ordinary I/O window or transition, VP/control unregister barriers, and ownership-aware teardown. Gate unsupported control lifecycles separately; userspace metadata copies, if needed, still use brief windows only on pages eligible for host access.

## 5. Concrete implementation stages

### Stage 0 — resolve ownership semantics and freeze a restricted contract

- Record Hyper-V requirements for `MAKE_EXCLUSIVE`, `MAKE_SHARED`, import, guest host-visibility changes, validation, partial completion, SLAT invalidation, and cache maintenance.
- Prove that guest transitions can be intercepted **before** root access/ownership is changed, and that the requesting VP cannot resume before completion.
- Define kernel-enforced stop/run gates for all VPs. Do not assume `pt_mutex` stops them.
- Specify whether launch import consumes bytes from the same backing and when Linux must revoke those bytes. Confirm treatment of CPUID/VMSA/PSP pages.
- Agree that ownership references and resident backing are allowed, while GUP pins and `mlock` are not.
- Obtain MM review of the special-PTE design. If the contract cannot be met, stop before defining a misleading capability.
- Approve and implement the mandatory no-PFN-export MM restriction before exposing any lite mapping; identify every enabled non-GUP consumer and its enforced rejection.
- Specify drain acknowledgement, deadlines, owner-mm death notification, and admitted-grant fault service in `DRAINING`, independently of ownership. Specify async completion-count/sub-status semantics and terminal-message routing before pending ownership operations.
- Test the existing acquire/release calls, error ordering, and lite fault/revocation integration. Do not add copy-window capability negotiation. Define separate AP VMSA/doorbell control lifecycles and reject lite runtime PSP forwarding until compatible owned buffers are verified.

Deliverable: documented kernel capability and ordering specification; no feature advertisement based only on the existence of a host-access hypercall.

### Stage 1 — owned shared-only backing

Add proposed `K/drivers/hv/mshv_guest_memfd.c`, wired through `K/drivers/hv/Makefile` and `K/drivers/hv/Kconfig`, with internal structures in `K/drivers/hv/mshv_root.h`.

- First add the proposed restriction/predicate in MM headers and `follow_pfnmap_start()`, with KVM/VFIO terminal denial checks and enabled-consumer audit. No public lite fd or mmap capability exists before this passes its tests.
- Implement inode creation, immutable size, bounded eager allocation of order-0 folios, charges, strong backing references, mmap validation, special-PTE faults, and teardown.
- Start with shared-only objects and no vCPU exposure. Remove direct-map aliases before userspace exposure, with explicit kernel TLB flush.
- Track owner-mm initialization access locally. Bind drains it and invalidates aliases; bound-runtime access uses the enforced existing modify-host-access path.
- Use the checked x86 direct-map helpers (`K/arch/x86/mm/pat/set_memory.c:2603-2658`). Verify `can_set_direct_map()`/platform capability on the actual Hyper-V root.
- Mark the address space inaccessible/unevictable. Disable migration/writeback. Keep destructive operations private to the driver's ownership-aware cleanup.
- Every allocation/direct-map failure unwinds only pages known still host-owned. Restore every modified direct-map entry and finish the corresponding TLB flush before freeing.
- Keep mapping tracking through the inode's `i_mmap` tree. Use object offsets rather than storing only the initial VA.

Acceptance at this stage: shared user loads/stores work; GUP and non-GUP PFN-export interfaces fail; no mlock flags or backing pins; aliases disappear on invalidation; object charges remain correct after fd close and VMA close. Lack of the MM restriction makes capability discovery and creation fail, not merely tests skip.

### Stage 2 — bind without the legacy pinned/MMIO path

Change `K/drivers/hv/mshv_root_main.c` region creation/registration and `K/drivers/hv/mshv_regions.c` dispatch:

- Add an owned-object region type. Do not call `mshv_prepare_pinned_region()` or the HMM movable path for it.
- Resolve folios from object offsets. The binding owns an object reference, not a GUP reference or a VA lifetime.
- Add object identity checks to reject lite VMAs in `mshv_map_user_memory()` and `mshv_chk_get_mmio_start_pfn()`. PFNMAP must not misclassify this RAM as MMIO.
- Reject legacy SNP RAM registration in lite mode. Reject lite backing aliases through another partition's legacy registration.
- Disable VFIO/passthrough in lite mode at `mshv_partition_ioctl_create_device()` and attribute attachment. Generic IOMMU/RDMA pin denial is also tested; do not rely on MSHV's bridge alone.
- Review partition flags, especially adjustable GPA permissions. They must not allow an unmediated guest ownership change.

### Stage 3 — transitions, vCPU gates, and no-bypass controls

Extend the proposed object manager and these existing kernel paths:

- `mshv_vp_ioctl_run_vp()` / `mshv_vp_handle_intercept()` in `mshv_root_main.c`: run admission, pending attribute transition registration, fail-closed replay/completion.
- The object manager and owner-mm lifetime notification: durable begin/drain-ack transactions, epoch checks, bounded drain deadline, and `DRAINING` fault admission for confirmed eligible grants.
- VP and partition ioctl dispatch: prevent host-access changes, GPA content access, import, and ownership-changing property calls outside the object state machine in lite mode.
- `mshv_partition_ioctl_modify_gpa_host_access()`: retain the existing ioctl, resolve bound objects, and enforce acquisition/release, permission, fault, alias/TLB, and retained-cleanup state. Never treat it as a guest ownership change.
- Integrate the userspace conflict coordinator with supported ownership operations; kernel ownership/teardown admission must not block releases needed to drain existing access. No BEGIN/END or root-copy-token UAPI.
- `mshv_partition_ioctl_import_isolated_pages()` and passthrough import: validate all ranges against bound objects and launch state; serialize ownership preparation. The existing import passes guest page numbers to the hypervisor, not a new Linux GUP buffer (`mshv_root_main.c:2474-2504`, `mshv_root_hv_call.c:1501-1558`).
- `mshv_partition_ioctl_issue_psp_guest_request()`: **reject guest-GPA payload forwarding in baseline lite mode**, including its raw passthrough equivalent. The current request/response GPA path conflicts with bounce-all-ordinary-kernel-I/O. Enable only after a verified Hyper-V/PSP-compatible interface uses separately owned request/response buffers, with bounded sizes, guest request authentication/address semantics preserved, generation checks, and async-owned lifetime. Do not reinterpret the PSP payload as an exempt control page or pretend arbitrary bounce-GPA substitution already works.
- `hv_call_modify_spa_host_access()`, `hv_call_map_gpa_pages()`, and unmap wrappers in `mshv_root_hv_call.c`: return exact completion information, bound retry/deposit work, and reject zero-progress loops.
- `mshv_init_async_handler()` / `mshv_async_hvcall_handler()`, `mshv_async_call_completion_isr()` in `mshv_synic.c`, async fields in `mshv_root.h`, and import/property/PSP/passthrough wrappers: replace the reusable bare completion with the persistent, partition-wide operation-owner protocol in section 6. Keep legacy non-lite behavior separate, but no legacy async command can share a lite partition's slot outside that protocol.

The existing SPA wrapper loses completed-prefix detail when an error occurs (`mshv_root_hv_call.c:1203-1212`). New callers need a ledger of every committed batch. Registration rollback currently may invalidate after unshare failure (`mshv_root_main.c:1770-1776`, `:1808-1811`); do not reuse that sequence for the new owned backing.

### Stage 4 — bounce all supported kernel I/O

- Replace `mshv_vp_ioctl_get_set_state_pfn()` user-page pinning with bounded kernel-owned bounce pages, then copy state to/from an ordinary userspace buffer. Preserve the existing size/overflow checks and buffer conventions.
- Ensure launch metadata, passthrough hypercall inputs/outputs, intercept messages, and state buffers are separate ordinary allocations. The passthrough path already allocates input/output bounce pages and copies input (`mshv_root_main.c:195-233`), but each operation still needs ownership validation.
- Reject lite-page GPA read/write bypasses or provide explicitly shared-only, state-gated copies. Hypervisor access to private data is not automatically authorized by an ioctl.
- Audit every supported file/block/network/io_uring operation in the VMM contract: registration, direct I/O, registered buffers, fixed buffers, splice/vmsplice, asynchronous work, futexes, RDMA, VFIO, and IOMMU. None may retain guest PFNs.
- Bounce memory may itself be pinned for I/O. It must not alias the guest object. This satisfies “no guest-memory pinning,” not “no pins anywhere in the machine.”
- Document that issuing a syscall with a shared guest VA directly is outside the supported VMM contract even if synchronous uaccess happens to work.
- Keep AP VMSA/doorbell control registration explicitly typed and capability-gated; these are hypervisor control references, not ordinary kernel-I/O payload buffers. The runtime PSP request/response path remains rejected until verified owned buffers are supported.

### Stage 5 — teardown and diagnostics

Implement teardown before enabling real private guests. Add kernel API documentation and tests. Large pages are a separate follow-up, not part of this stage. Update the Guide through the implementation effort, not this planning task.

## 6. Transition, race, and failure model

Separate three mechanisms: short metadata/transaction admission locks; a shared/exclusive fault-invalidation gate; and a userspace copy-drain handshake. A fault holds the shared gate through special-PTE insertion. A transition takes the exclusive gate only after draining copies, then closes fault admission and zaps aliases. Never hold that gate, `pt_mutex`, a VP mutex, `mmap_lock`, or an `i_mmap`/page-table lock while waiting for userspace, VP exit, or async hypercall completion. Document and lockdep-test the full order for the short locked sections. A partition operation owner serializes long operations without a thread holding those locks across waits.

### Kernel ownership/teardown drain and liveness

This handshake and whole-VM gating apply to explicit ownership changes and teardown. Ordinary per-copy access uses existing acquisition/release with range conflict rejection, not this transaction protocol. Unsupported guest changes remain rejected rather than silently becoming queued ownership work.

1. Reserve a durable logical transaction and drain epoch under short metadata locks, **not the wire hypercall owner**. Gate new VP runs and kick/drain running VPs without holding the fault gate; initial implementation stops the whole VM. No ownership-changing operation or transition-driven user-PTE zap has occurred yet.
2. Publish `DRAINING` and notify the VMM. Committed ownership remains unchanged; admitted authorized access keeps fault service until release. No new acquisition is admitted. An old copy may still fault an unpopulated alias, independently of the ownership label.
3. The VMM coordinator closes range lease acquisition and device-copy admission under its local lease lock. It waits for previously admitted range leases to reach zero, without holding a lock those copies need. A bounce completion without an existing lease cannot begin a new copy; queue/discard it according to the device protocol.
4. The VMM submits the ownership/teardown drain acknowledgement. Kernel validates controller/mm, transaction/epoch/generation, and zero active/pending or uncertain host-access grants. Unresolved acquisition/release returns busy. Stale, duplicate, or wrong-range acknowledgements cannot advance another transaction. A local copy count is not hardware-revocation proof.
5. Only after a valid acknowledgement and retirement of grant operations does the kernel atomically reserve the transition hypercall owner, then take exclusive invalidation authority, close fault admission (`REVOKING`/`TO_PRIVATE`), wait for existing fault handlers to leave the shared gate, and zap all aliases. There is no copy-drain wait inside this exclusive section. A new fault after closure gets the documented terminal fault and cannot install a PTE.

An acknowledgement may race raw user accesses not covered by VMM leases. Kernel PTE/TLB revocation still prevents those accesses from surviving private commit. The kernel need not discover arbitrary user copy counters to protect private ownership. Correct VMM copy semantics, however, require the handshake; a lying controller can cause its own accesses to fault, not extend physical ownership lifetime or regain a private mapping.

Use bounded ownership/teardown waits, with a proposed 5-second drain deadline and finite administrator maximum. Transition submission does not wait for acknowledgement under driver locks. Interruptible wait/poll cannot extend the deadline.

- **Drain timeout or interrupted waiter before revocation:** report durable failure; do not commit private ownership, close an admitted copy's faults prematurely, or free backing. Releases still make progress; the VM remains stopped. Host-initiated abort/reopen requires unchanged ownership/generation and no uncertain grant. An unresolved guest exit stays stopped unless its existing failure/abort protocol permits resume.
- **Controller fd closes while its mm still lives:** request shutdown and apply the same bounded drain rule. Closing a fd does not prove copies are gone, and a remaining VMA is not freed. If there is no acknowledgement, retain charged shared backing and a stopped/recoverable transaction; do not wait forever in `.release`.
- **Owner-mm death/exec:** install an owner-mm release notification whose callback only marks the event and schedules cleanup, without blocking under MM teardown locks. Hold `mmgrab`/metadata lifetime as needed, not `mm_users`, so the operation does not prevent release detection. After release establishes that the owner mm has no remaining user execution, no user copies can resume in that mm. The cleanup owner can skip the userspace acknowledgement, close faults, and complete alias revocation. Async bounce completions remain fenced by the dead owner/generation and must not copy into guest RAM. New mappings from another mm are rejected.
- **Death after revocation or an async submission:** ownership cleanup follows the persistent async owner below; process death is not hypercall cancellation. Never reopen shared access just because the caller vanished.

These rules are independent of the Hyper-V post-revocation ownership order. They remove the copy-fault deadlock before any private-capable hypercall is allowed.

### Shared → private

1. Validate the entire bounded range, ownership, expected generation, launch state, and page granularity. Allocate transaction metadata before changing access.
2. Gate new VP runs, kick/drain running VPs, and publish drain state. Reserve logical metadata only; existing releases/recovery retain call admission and fault service. Do not yet commit ownership change.
3. Close new host lease/device-copy admission and obtain the bounded epoch acknowledgement using the handshake above. Fence new asynchronous bounce copies; existing admitted copies may finish and fault. Timeout/death uses the explicit rules above, not a lock-held wait.
4. After acknowledgement confirms zero active/pending access and resolved grants, reserve ownership-operation admission, close faults under exclusive invalidation authority, and zap **every** alias. Complete CPU TLB shootdowns; release locks before waiting for hypervisor work. Direct mappings were removed. `mprotect(PROT_NONE)` is not ownership enforcement.
5. Remove or restrict guest mappings as required by Hyper-V and complete its guest-TLB/access barrier. Issue the verified exclusive/ownership transition in bounded batches, recording partial completion.
6. Complete guest validation/import/protocol handoff as required. Publish `PRIVATE` and increment generation only after both Linux and hypervisor barriers succeed.
7. Permit VP resume only after the pending transition token is resolved.

The exact order of steps 5–6 is a **blocking Hyper-V protocol question**, not something Linux source alone determines. In particular, root access revocation is not a substitute for SNP ownership/validation.

### Private → shared

1. Enter the common ownership drain regardless of committed ownership. Close new admission, resolve pending acquisition, and let confirmed eligible copies and their faults/releases finish before fault closure or ownership changes. Only verified absence of active, pending, and uncertain grants permits skipping the copy-drain wait. Then mark `TO_SHARED` and stop affected guest use.
2. Follow the verified guest/SNP release and Hyper-V reclaim protocol. Confirm shared ownership and root-access acquisition; complete required guest/root translation and cache barriers.
3. Keep the direct map absent, publish committed ownership/generation only after barriers, and reopen acquisition admission with no persistent user grant. Only successful authorized existing host-access acquisition permits PTE faults.
4. Do not expose uninitialized recycled pages. Content-preserving transitions follow the specified guest protocol; Linux does not zero a private page or promise its ciphertext becomes useful plaintext. Zero only a new host-owned allocation or an explicitly destructive, fully reclaimed reset.

### Races that must be closed

- **Fault versus revocation:** no fault may install a PTE after the zap. The gate must cover state checking and insertion, not just lookup.
- **Mmap/mremap/munmap versus transition:** all aliases remain attached to the backing inode; immutable object offsets, not remembered VAs, select pages. New mappings cannot make a private page faultable.
- **Multiple vCPUs:** individual `vp_mutex` locks are insufficient. Enforce kernel run gates and drain in-flight guest access before ownership changes.
- **Guest transition versus host acquisition:** reserve checked GPA ranges and RAM identity under one coordinator before either operation proceeds. Reject any guest request with a conflicting range as a whole, including during acquisition/release and on other VPs. Unsupported nonconflicting transitions stay rejected.
- **Device copy versus transition:** `DRAINING` permits confirmed eligible copies to fault while refusing new acquisition, independently of ownership. Revocation starts only after epoch acknowledgement or established owner-mm death. Kernel PTE/TLB revocation remains mandatory. Bounce I/O may finish afterward but must not copy into a stale generation.
- **Existing pins:** do not accept imported user pages, so there is no inherited GUP pin population. New special PTEs reject GUP before grabbing pages. An existing anonymous region cannot be “upgraded” safely just by changing PTE permissions or checking a pin counter.
- **Raw PFN consumers:** the permanent MM no-PFN-export restriction rejects `follow_pfnmap()` for all lite aliases before PFN lookup; KVM/VFIO denial tests are baseline capability gates. Also reject legacy MMIO and passthrough DMA paths. Deliberate trusted-kernel physical remapping is not an ordinary supported consumer.
- **Concurrent close/teardown:** transition and binding references keep backing alive; fd/VMA closure cannot free pages still owned or mapped by the guest.

### Failures and interruption

Before any successful ownership hypercall, normal local rollback can restore shared access after barriers. After partial completion, roll back only the exactly known completed pages and only through a verified reclaim protocol. If rollback fails or ownership is uncertain, keep user PTEs absent, stop the VM, and quarantine the pages.

Never report whole-range success after a partial hypercall. Initial ABI may return a failed transaction with queryable committed subranges rather than claim atomic range rollback. Signal delivery and failed `copy_to_user()` after a commit do not undo ownership. The transaction ID/query operation must recover the actual state.

Bound allocation, retry counts, hypervisor deposits, and pending transaction count. A zero completion on nominal success must not spin forever. Rate-limit repeated guest-induced errors. Invalid requests return typed errors, not assertions or host panics.

### Partition-wide asynchronous operation owner

Current evidence requires a separate design, not just adding a timeout to the old wait:

- MSHV defines one async completion/status slot per partition (`K/drivers/hv/mshv_root.h:118-124`); the existing callback waits without a deadline and returns only the stored status (`K/drivers/hv/mshv_root_main.c:1674-1681`).
- SynIC looks up the partition by ID, stores `status`, and completes that slot (`K/drivers/hv/mshv_synic.c:149-173`). The wire payload also has `completion_count` and `sub_status`, but no transaction cookie (`K/include/hyperv/hvhdk.h:700-705`).
- Import samples `hv_repcomp(status)` before handling `CALL_PENDING`, then advances by that sampled value after completion (`K/drivers/hv/mshv_root_hv_call.c:1539-1555`). Passthrough similarly captures REP progress before its wait (`K/drivers/hv/mshv_root_main.c:241-259`). A new protocol must not infer final progress from that pending snapshot.

Proposed owner protocol in `mshv_root.h`, the object manager, SynIC, and hypercall wrappers for potentially pending **kernel ownership/control/teardown** work. This is not a new per-copy ABI or a phase-1b dependency; uncertain existing host-access calls still retain cleanup and stop the partition:

1. **One admitted pending wire operation per partition.** Relevant launch/import/property/supported-PSP/ownership/teardown work owns the existing completion slot through terminal results and barriers. Logical drain reserves no slot; existing host-access releases/recovery must progress before ownership submission claims admission. Other conflicting commands return busy or wait with bounds outside locks. Teardown cannot steal/reinitialize the slot; legacy async passthrough cannot bypass its owner. No durable copy token is introduced.
2. **References precede submission.** The record holds strong partition, backing-object/binding, module, input/output/bounce-buffer, and pending-intercept references. A routing registry holds the record until terminal retirement or quarantined recovery. The live partition ID remains routable through `mshv_partition_find()`; do not remove/delete the partition before pending completion is retired. Define the registry as the recovery owner so retention is not an unbreakable partition/object cycle. Close, mm death, and waiter interruption request action, not release of these references.
3. **Persist input lifetime.** Use operation-owned hypercall input/output pages for any input/output that Hyper-V may retain while pending. Do not reuse per-CPU argument pages on an unverified assumption that pending consumes all data synchronously. If the verified contract copies a particular input before return, record that exception explicitly; backing references still remain.
4. **Publish before calling.** Set a submission phase and sequence under the admission/ISR-safe metadata lock before entering Hyper-V, so completion can arrive before the submitter observes `CALL_PENDING`. The ISR copies the full payload into the active record, changes its completion phase, and wakes/schedules a worker; it performs no reclaim, blocking wait, or ownership hypercall. Lookup and record-reference acquisition occur under RCU plus a safe refcount/slot-lock discipline.
5. **Route by partition, never by waiter.** Because the payload lacks a transaction cookie, an internal epoch alone cannot distinguish an old wire message from a new operation. Never admit/reinitialize a second operation before the first terminal message is consumed and the verified delivery barrier is satisfied. Require a verified exactly-once terminal-completion/delivery-order contract, or a Hyper-V-supported drain/sequence mechanism, as a capability prerequisite. Unexpected idle-slot messages poison the partition's async state. No new operation or partition-ID reuse is allowed after timeout merely because a local epoch changed. If the needed wire guarantee cannot be verified, pending ownership operations and private-lite capability remain unavailable.
6. **Separate submitted, pending, and completed progress.** Store the submitted batch bounds, immediate raw status/REP count, final `status`, final `completion_count`, and `sub_status`. An operation-specific adapter must define the authoritative completed prefix from the verified Hyper-V specification, including whether final counts are absolute or relative to prior progress. Do not add counts or replay pages by guessing. Immediate terminal responses use their verified REP count; `CALL_PENDING` does not advance the committed ledger. Only a validated terminal result can publish an exact completed prefix.
7. **Conservative baseline for async REP calls.** Initially submit one element per potentially asynchronous REP operation. Capability still requires verification of final success/count/sub-status semantics; one element does not excuse ignoring completion data. After verified final success, that element can be committed once. Partial/contradictory/unknown completion quarantines the submitted element and its retained extent; query reports it as uncertain, not completed or undone. Multi-element async batching is a later capability with adapter and partial-progress tests.
8. **Bound work, not unsafe lifetime.** Give each admitted operation a finite watchdog (proposed default 30 seconds, finite administrator maximum), bounded batches/deposit/retry budgets, and a zero-progress failure rule. A watchdog marks `TIMED_OUT_PENDING`, stops the VM, notifies query/poll, and returns waiters without cancelling the hypercall, admitting another async call, or freeing buffers/backing. Recovery keeps the same active slot and routing references. Existing resident/operation/quarantine charges remain; exhaustion refuses new admissions, never frees uncertain private pages.
9. **Late completion and teardown.** A late terminal payload reaches the same record. Its worker validates progress, then performs verified rollback/reclaim or queued teardown; it cannot automatically resume a timed-out/dead-owner VM. If cancellation is available, it must produce a verified terminal drain before release. Without such a protocol, retain quarantined backing/routing until completion or verified hypervisor reset/recovery. Ordinary `.release` returns without a forever-blocked worker; the registry owns recovery. Module unload and partition deletion fail/defer while that owner remains.
10. **Terminal retirement.** After validated final status, ownership barriers, and successful reclaim where requested, atomically detach the slot/routing references under the ISR-safe lock, synchronize readers/delivery as required by the verified protocol, and release retained buffers/objects. Retain a bounded metadata result for transaction query. Never use ioctl return, failed output copy, a zero user refcount, or a timed-out wait as terminal retirement.

These are mandatory baseline gates for private-lite capability. Without authoritative pending/final progress semantics or safe completion routing, the shared-only prototype may remain usable only after its independent MM-export gate passes; async ownership-changing support stays disabled.

## 7. Deferred follow-up: large pages

Use order-0 backing and 4 KiB guest mappings, grants, and state accounting now. Defer owned huge folios, large guest maps/grants, mixed 2 MiB state, split/demotion, and THP/hugetlb promises. A later plan must cover allocation, ownership/grant granules, demotion, and control-page exclusions.

Do not disable unrelated legacy guest superpage support. Removing Linux direct-map aliases may still require ordinary page-table splitting and TLB flushes; that mandatory MM safety work is not deferred guest large-page support.

## 8. Lifetime, donation, reclaim, and unsupported MM services

### Guest RAM is not hypervisor bookkeeping donation

`hv_alloc_dep_pages()` allocates kernel pages, splits them, and deposits PFNs through `HVCALL_DEPOSIT_MEMORY`: `K/drivers/hv/hv_proc.c:47-102`, `:109-142`. `hv_call_withdraw_memory()` frees returned donated pages: `K/drivers/hv/mshv_root_hv_call.c:44-84`. Partition teardown separately releases memory regions and then withdraws donations: `K/drivers/hv/mshv_root_main.c:2957-2965`.

The lite object owns guest backing. Hypervisor bookkeeping donation is a separate budget and lifetime. Do not put guest-object pages on the deposit/withdraw path. Cap deposits/retries associated with a guest; the inspected helpers do not establish a per-guest exhaustion bound.

### Teardown and host crash

Current region destruction unmaps, reacquires root access for SNP, then invalidates/unpins. On share failure it logs and returns without freeing: `K/drivers/hv/mshv_regions.c:350-389`. Partition destruction unmaps SNP regions, suspends VPs/clears SEV state, and sets isolation state before finalization: `K/drivers/hv/mshv_root_main.c:2797-2856`, `:2883-2897`, `:2949-2965`.

The current unmap-first guard is present: region destruction does not restore host access or invalidate pages after its guest-unmap failure, and skips the guest unmap only when `pt_initialized` is false (`K/drivers/hv/mshv_regions.c:362-371`). That improves operation ordering, but it is not a complete release/recovery contract:

- `mshv_unmap_user_memory()` removes the region from the partition list, calls the void `mshv_region_put()`, and returns success (`K/drivers/hv/mshv_root_main.c:1884-1915`). Thus a destructor failure can leave backing/pins allocated without this list entry or a reported ioctl failure.
- `mshv_region_destroy()` runs after the last kref drop and simply returns on unmap/share failure (`K/drivers/hv/mshv_regions.c:350-387`). The inspected failure path supplies no retry owner; the new backing cannot use that zero-ref failure pattern.
- SNP partition teardown ignores each preliminary region-unmap return (`K/drivers/hv/mshv_root_main.c:2885-2889`). It also ignores `hv_call_finalize_partition()`'s result and then clears `pt_initialized` (`:2949-2953`). Subsequent region destruction treats that boolean as permission to skip its unmap. Lite mode must instead retain explicit verified-unmap/finalize/reclaim state.

For lite, remove a binding from its tracked/recoverable registry only after successful cleanup, or atomically transfer it to a charged quarantine registry with a live retry owner. Separate externally visible unbind completion from the internal final reference release. An error-capable teardown operation must report failure; a last-reference destructor must never be the first place a recoverable hypervisor ownership change is attempted. Keep failed partition finalization and every failed range visible for diagnosis and retry.

The new path must:

1. Mark the object/partition dying and enter the common ownership/teardown drain regardless of committed ownership. Stop new run/transition/lease admission, resolve pending acquisition, and let confirmed eligible copies and their faults/releases finish before fault closure. Skip the copy-drain wait only on verified absence of active, pending, and uncertain grants.
2. Drain VPs and device users through the bounded handshake or established owner-mm death. Only then close faults and revoke all user aliases. A live-mm drain timeout retains admitted-grant fault service and stopped, charged backing; it is not permission to free or enter a lock-held drain wait.
3. Unmap guest GPAs and complete verified isolation teardown/reclaim.
4. Reacquire host-safe ownership of every page, checking results.
5. Restore direct mappings, flush kernel TLBs, scrub host-owned pages, and release allocation charges/references.
6. Retain/quarantine pages and enough recovery metadata if any step is uncertain. Do not allow the inode's generic eviction to free private pages. The implementation must define the recovery owner and avoid a partition/object reference cycle.

Process death and fd close must reach this same state machine. VMA references and binding references have different lifetimes; closing one fd is not permission to reclaim guest-owned RAM. Pending asynchronous work retains the partition-ID routing entry and operation owner until verified terminal retirement; finalization cannot bypass it. After completed unbind, stale VMAs should fault with `SIGBUS` until a documented safe detached state is reached.

Current panic handling unmaps SNP regions and attempts to share them for crash dump, logging failures: `K/drivers/hv/mshv_root_main.c:3684-3739`. That is an existing policy, not proof of guaranteed reclamation in panic context.

For lite: specify vmcore exclusion plus hypervisor/kexec ownership recovery before claiming crash support. Panic cannot safely depend on normal mutexes, allocation, or cooperative VMM leases. Preserve fail-closed ownership if reclaim is unconfirmed; never publish private pages for dumping merely because userspace died. A machine reset/hypervisor recovery contract remains necessary.

### MM operation policy

| Operation | Initial lite policy |
| --- | --- |
| Swap/reclaim/writeback | No backing swap or writeback. Unevictable inaccessible object; cap resident bytes. |
| NUMA migration/compaction/memory hot-remove | Reject moving owned backing. Report blocked offlining; no private-content copy. |
| Live migration/snapshot | Unsupported without SNP-aware export/import. Shared-byte copying alone is not VM migration. |
| THP/KSM | No automatic collapse/merge. Owned large folios are deferred follow-up work. |
| Fork | `VM_DONTCOPY`; inherited fds cannot create mappings in a different owner `mm`. Define the same-mm thread case. |
| MAP_PRIVATE/COW | Reject at mmap. No accidental anonymous COW alias. |
| mprotect | May reduce shared access; cannot make private pages present. Reject executable permissions and preserve special entries/state checks. Do not use it as the transition mechanism. |
| mremap | No expansion or DONTUNMAP. Same-size relocation, if retained, must preserve object-offset mapping tracking and pass revocation tests. |
| munmap/remap/aliases | Unmapping releases only VA, not guest backing. Any allowed same-mm alias joins inode invalidation. |
| mlock/mlockall | No guest backing `VM_LOCKED` or pinning. The inspected `vma_supports_mlock()` excludes `VM_SPECIAL` (`K/mm/internal.h:1133-1139`), and `mlock_fixup()` skips unsupported VMAs (`K/mm/mlock.c:475-481`). Verify new PFNMAP mappings retain that exclusion, including process-wide locked-default flags. Do not promise reclaimability. |
| madvise/truncate/fallocate | No destructive backing change outside the object state machine. VMA-only discard may zap PTEs, not free guest-owned folios. Reject ownership-changing advice, userfaultfd, and huge collapse unless explicitly supported. |
| Core dump/ptrace/process_vm/proc mem | `VM_DONTDUMP` plus no `.access`; GUP denial. Test forced/remote access too. |
| Memory poison | Stop guest use and quarantine; no generic copy/reclaim of private contents. Surface a bounded error to userspace. |

Relevant current MM evidence: secretmem migration rejection `K/mm/secretmem.c:147-151`; inaccessible mapping migration rejection `K/mm/migrate.c:1090-1093`; PFNMAP mremap restrictions `K/mm/mremap.c:1696-1698`, `:1736-1737`; core exclusion `K/fs/coredump.c:1602-1603`, `:1623-1625`. These support the policy, but are not a substitute for tests of the new memory type.

## 9. Focused tests and acceptance criteria

Add backing/MM selftests and MSHV-specific SNP integration tests; use a fake hypercall backend or fault-injection hooks for safe exhaustive failure testing. Hardware SNP tests require a suitable Hyper-V host and must skip when unsupported.

1. **API validation:** unknown flags, MBZ fields, zero/unaligned ranges, arithmetic overflow, overlap, cross-partition fd, different owner `mm`, duplicate binding, mixed legacy/lite RAM, unsupported MMIO/VFIO attachment.
2. **Authorized access:** user read/write only under successful existing host-access acquisition, denial after release, multiple aliases, remap/munmap, initialization closure before bind, and contents after supported ownership cycles. No inferred private plaintext authorization.
3. **Private mapping denial:** prefault all aliases, transition, verify absent special PTEs and completed shootdowns; user accesses get the documented fault. Test racing faults, mprotect, and new mappings. Do not deliberately probe a private PFN through unsafe kernel loads to “prove a crash.”
4. **GUP matrix:** instrument fast/slow/remote/unlocked/fast-only GUP, `FOLL_GET`, `FOLL_PIN`, read/write, `FOLL_FORCE`, and short/long-term variants. Prefault authorized mappings to hit fast GUP. Confirm zero acquired guest pins/references and no retained pages; no normal PMD/PUD user mapping can be introduced.
5. **Consumers:** vmsplice/splice, direct I/O, io_uring registered/fixed buffers, process_vm read/write, ptrace, proc mem, futex, RDMA/IOMMU/VFIO registration where available. Expect no zero-copy backing access; test bounced supported operations separately.
6. **Drain liveness and races:** hold an eligible grant on an unpopulated alias, start `DRAINING`, then force that copy to fault before releasing it. Test independently of the ownership label; its fault and copy must finish before zero-grant acknowledgement permits revocation. Racing new grants are refused; post-`REVOKING` faults install no PTE. Cover stalled acknowledgement, timeout, interrupted waits, controller close, owner-mm death/exec, stale epochs, multi-VP changes, queued bounce completions, and mremap. No drain waits under locks needed by the draining actor; stale completions never copy into a repurposed generation.
7. **Failure injection:** allocation, direct-map removal/restoration, PTE allocation, every hypercall batch, partial progress, zero progress, rollback failure, teardown failure, output-copy failure after commit. Verify ledger/query accuracy and quarantine.
8. **Lifetime/accounting:** last fd versus last VMA versus last binding; no leaks on successful reclaim, no early free on failed reclaim, no reference cycles. Resident and donated-memory limits hold under repeated malicious guest transition requests.
9. **MM services:** memory pressure, swap enablement, NUMA migration, hot-remove, KSM/THP advice, fork, mlockall, core dump, resize/hole punch, userfaultfd, and poison injection. No content touch of private backing or unsupported lifetime change.
10. **Base-page scope:** verify order-0 allocation and 4 KiB guest map/grant accounting. Reject unsupported large-page requests in lite mode; preserve legacy superpage behavior. Positive large-page/demotion tests belong to the deferred follow-up.
11. **Crash policy:** isolated platform tests for killed VMM, teardown failures, panic/kexec, and reset/recovery. Establish ownership recovery before claiming crashdump support.
12. **Regression:** legacy non-lite MSHV memory and VP state APIs retain existing behavior; shared-only lite tests run without SNP where the platform contract permits.
13. **Non-GUP exclusion gate:** prefault shared aliases and attempt KVM remapped memslot resolution, VFIO type1 DMA mapping, generic physical access, another MSHV partition's MMIO registration, and other enabled raw-PFN consumers. Confirm terminal denial before PFN resolution/export, even when KVM's unsafe-remapped policy is relaxed. Repeat after mprotect, split, same-size mremap, and additional aliases. An instrumented `follow_pfnmap_start()` must reject the immutable restriction for all page states. A build without the reviewed MM restriction must advertise no public lite capability and reject creation/mmap; userspace avoidance is not a passing result.
14. **Async ownership/progress gate:** force `CALL_PENDING`, completion before submit returns, delayed completion past watchdog, waiter interruption, failed result copy, mm death, final-fd close, teardown while pending, and a second partition/VP async ioctl while the first owner is active. Verify one admitted owner per partition and retained routing/backing/input/output/module references. Inject zero progress, final partial progress, contradictory counts, missing terminal messages, idle-slot unexpected messages, and late completion after timeout; no double-count, replay, slot reuse, premature deletion/free, or automatic VP resume is allowed. Validate operation-specific pending/final count adapters against the verified contract. If exactly-once/order or count semantics cannot be established, private capability must remain unavailable.
15. **Existing scoped host access and conflicts:** instrument acquire/release flags and PTE permissions; no per-copy `MAKE_SHARED`/`MAKE_EXCLUSIVE` or ownership-generation change. Failed acquire never copies. Successful release leaves no authorized aliases; failed/uncertain release blocks ranges, stops the partition, and retains cleanup. Race multi-VP attribute changes against pending acquisition, copy, and release, including repeated/malformed/overflowing ranges and mixed conflicting/nonconflicting batches. Reject the whole conflicting request without updates. Test admission in both directions, serialization of overlapping copies, metadata/payload page sharing, and releases during ownership drain. Denial/resume uses only a proved existing failure protocol; unresolved completion stays stopped.
16. **Control pages and PSP separation:** runtime PSP guest-GPA forwarding and raw passthrough fail in lite mode until an owned-buffer capability is verified. A future owned-buffer test must prove payload/authentication semantics and no guest-PFN kernel I/O. AP VMSA/doorbell tests enforce typed control references, registration/unregister barriers, and no conflict with ordinary copy windows or transitions; they must not create persistent userspace root grants.
17. **Cleanup and initialization:** interrupt/fail existing acquisition/release calls and result delivery; confirm that no uncertain outcome authorizes copying, blind retry, backing reuse, or conflicting updates. Race initialization access with bind; verify drain, admission transfer, and alias invalidation. Failed bind reopens initialization only after proven unbound host ownership; otherwise quarantine.

Acceptance requires section 4's invariants, no guest GUP acquisition or ordinary PFN export, no `VM_LOCKED`, one backing identity, no unauthorized access or accessible alias surviving private commit/revocation, bounded resources, exact failure reporting, and verified reclaim. Shared lite capability requires MM PFN-export denial; private ownership operations additionally require drain, async lifetime/progress/routing, and barriers. Missing gates withhold that lite capability, not the independent userspace scoped-copy stage. Boot success alone is insufficient.

## 10. Risks, unresolved facts, and next decisions

**Blocking before a private-capable implementation:**

1. Hyper-V ownership semantics: which operation transfers/validates/reclaims SNP ownership, and which only changes root visibility? What happens to contents and cache state at each stage?
2. Guest intercept ordering: can all guest-triggered visibility changes be intercepted before root access is withdrawn? How is completion acknowledged? Can another VP change/access the same extent in parallel? If this cannot be enforced, the requested concurrent integrated API is unsupported.
3. Hypervisor barriers: do SPA access changes and GPA unmap/map synchronously drain access and invalidate the relevant root/guest translations? What additional fences are required?
4. MM approval: is dedicated owned RAM with special PFNMAP PTEs and an inaccessible inode acceptable, with explicit charging and alias tracking? Confirm encryption/cache attributes on the Hyper-V root.
5. Clarify “no locking”: resident ownership and synchronization cannot be eliminated. If unevictable owned backing is prohibited, the lite proposal does not satisfy the request.
6. MM non-GUP exclusion: implement and review the immutable no-PFN-export restriction, with KVM/VFIO and enabled-consumer tests, before public shared mappings. If any enabled ordinary consumer cannot enforce it, withhold the capability or use an enforced build/deployment exclusion.
7. Drain and async liveness: validate the ownership-independent `DRAINING` epoch handshake and bounded timeout/death rules. Verify Hyper-V's exactly-once/ordered terminal-message or supported drain mechanism, input lifetime, and authoritative completion-count/sub-status semantics. Without these, no pending ownership-changing operation or private-lite capability is allowed.
8. Existing host-access integration: test acquisition/release and platform ordering/failure behavior, plus lite fault and alias/TLB revocation. No new Hyper-V feature or per-copy token capability is required. Exact GPA-attribute failure completion remains a narrow question; unresolved exits stop, never resume as success. Lite PSP payload forwarding remains rejected without owned buffers; AP VMSA/doorbell need verified control lifecycles.

**Resolve before advertising optional capabilities:**

- Large pages and demotion belong to the deferred follow-up; special import/control-page requirements for 4 KiB operation remain in scope.
- A PSP-compatible separately owned-buffer interface, payload/authentication semantics, bounded buffers, and overlap handling. Until verified, reject runtime PSP forwarding rather than exempt guest payload pages from bounce-all-I/O.
- Device and kernel-I/O contract coverage, including non-GUP PFN consumers.
- Crash/reset reclaim, vmcore exclusion, and recovery of quarantined objects.
- Allocation/memcg accounting for special mappings and backing folios; cap both guest RAM and separate hypervisor donation.

### Comparison and recommendation

| Approach | Result |
| --- | --- |
| Keep anonymous memory; remove MSHV `FOLL_PIN` | Incorrect. Linux can migrate/reclaim/COW it, existing and new pins remain possible, aliases are not controlled, and stable private ownership is not established. |
| Keep anonymous memory; add mprotect/notifiers | Insufficient. VA permissions do not revoke existing pins or every alias, and MM operations can touch private contents. |
| Reuse local KVM guest_memfd unchanged | Insufficient. It is KVM-bound, allocates order-0 backing, and does not support initial shared backing for x86 private-memory VMs. |
| Build a fully integrated shared/private guest_memfd core | Broadest MM integration, but still needs per-page ownership, mapping revocation, no-GUP policy, and architecture callbacks. Not automatically pin-free. |
| Dedicated MSHV lite object with special user PTEs | Recommended narrow starting point only with the mandatory reviewed MM no-PFN-export restriction. One owned backing; shared user VA; no normal GUP or ordinary PFN export; no swap/migration/DMA; drain handshake and persistent async owner; explicit reclaim and resource limits. |

OpenVMM phase 1b adds brief existing acquire/copy/release calls at the synchronous leaf, with an internal coordinator/guard. Current acquisition issues the existing ioctl (`O/vmm_core/virt_mshv/src/x86_64/snp.rs:422-457`); attribute exits currently halt (`:1200-1231`). No grant crosses translation/RPC/backend waits. Deny conflicts across the full grant lifecycle; failed/uncertain release stops the partition and retains cleanup. Do not turn halt into success/replay, force guest sharing per copy, or add a copy token ABI. Kernel ownership/teardown coordination remains separate.

Current working-tree integration also constrains acceptance:

- Partition creation enables GPA superpages (`O/vmm_core/virt_mshv/src/x86_64/mod.rs:239-241`), with an independent SNP flag test at `:1687-1705`. This is permission to use supported mappings, not proof of huge backing or mixed-state transitions.
- The SNP synthetic-feature builder intentionally does not advertise the reference-TSC page, and its support predicate excludes SNP (`O/vmm_core/virt_mshv/src/x86_64/mod.rs:275-300`, `:315-324`). Keep that current behavior until the shared reference-page lifecycle is separately validated. The comments report bring-up observations; they do not prove the cause of a guest sharing failure.
- The Guide calls SNP a direct-boot bring-up feature and excludes hugetlb backing (`O/Guide/src/reference/openvmm/management/cli.md:95-105`). Document lite mode as 4 KiB-only; infer no THP/hugetlb or large-page support.

**Next decision:** approve owned backing plus enforced no-GUP/no-PFN-export MM behavior, obtain Hyper-V ordering/completion answers, and select restricted PFNMAP-lite versus an integrated MM memory type. The drain handshake, async owner, and capability gates are baseline requirements, not deferred concurrency improvements. Only then freeze the private-transition ABI and begin implementation.

## Review

**History notice — superseded on 2026-10-08:** Earlier root-copy token/BEGIN/END capability requirements, initialization-token recovery, phase-two-only scoped access, and large-page launch prerequisites no longer apply. Historical findings on MM export denial, fault-before-drain liveness, persistent ownership/teardown lifetime, and safe reclaim remain relevant. Earlier verdicts do not approve this revision.

### Initial review

**Verdict: Needs rework.** Source validation supports the main rationale. The required changes concern copy-drain ordering, non-GUP consumers, and asynchronous completion ownership.

1. **Drain copies before blocking shared faults.** An existing copy can fault on an unpopulated shared alias. Blocking faults before draining its lease can deadlock the transition. Gate and drain VPs; stop new leases while keeping existing committed-shared faults serviceable; await a bounded drain acknowledgement without fault/invalidation locks; then block faults and revoke aliases. Specify the acknowledgement, timeout, and process-death rules. Add a deterministic test where an existing lease faults during revocation.
2. **Exclude non-GUP PFN consumers.** PFNMAP rejects slow GUP and special PTEs reject fast GUP (`K/mm/gup.c:1200-1208,2868-2878`). However, KVM deliberately falls back to `follow_pfnmap_start()` after GUP fails (`K/virt/kvm/kvm_main.c:2946-2989,3014-3023`). This does not alone prove a private-access bypass, because notifier synchronization matters. Before advertising an unconditional no-consumer guarantee, require MM-reviewed backing-type rejection or enforceable deployment restrictions. A documented unsupported consumer is not sufficient.
3. **Own asynchronous transitions through completion.** The driver has one completion slot per partition (`K/drivers/hv/mshv_root.h:118-124`), waits without a timeout (`K/drivers/hv/mshv_root_main.c:1674-1681`), and routes completion by partition ID (`K/drivers/hv/mshv_synic.c:152-173`). Import samples progress before handling pending completion (`K/drivers/hv/mshv_root_hv_call.c:1539-1555`). Add partition-wide async admission, authoritative pending/final progress rules, and persistent backing/partition/completion references. Timeout or interruption cannot mean cancellation or permission to reclaim. Test delayed completion, process death, partial progress, and teardown races.

**Validated:** current SNP registration pins before unsharing; standard special-PTE insertion and alias invalidation support the proposed prototype; teardown needs explicit quarantine ownership; large ownership demotion remains an external prerequisite. The shared-only prototype and legacy separation are appropriate.

This review was performed by the `review-plan` agent. The author must address these items and obtain a second verdict before treating the plan as reviewed.

### Author response to initial review

Revised the main plan for all three must-fix items:

1. Added a two-phase epoch drain handshake. Existing shared faults remain serviceable during copy drain; only acknowledgement or established owner-mm death permits revocation. Specified bounded deadlines, interrupted waiters, live-mm fd closure, guest-intercept abort limits, and deterministic fault-during-drain tests.
2. Made an immutable core-MM no-PFN-export restriction mandatory before public shared mappings. Added terminal KVM/VFIO checks, enabled-consumer enforcement/audit, alias-preservation tests, and capability denial when the restriction is absent.
3. Added one persistent async owner per partition, full completion-payload routing, retained lifetime references, operation-specific final-progress rules, single-element baseline async REP calls, and late-completion/timeout/death/teardown tests. Unverified wire ordering or progress semantics disables private capability; timeout does not cancel or reclaim.

**Readiness:** revised draft saved, pending re-review. The initial verdict is preserved above; no claim of reviewer approval is made.

### Author response to cross-plan coordination gap (superseded token proposal)

Added a separate capability-gated begin/end-copy-window ABI and independent root-grant state. End zaps aliases and must verify root release without SNP ownership churn; unknown release semantics blocks bound-runtime window support, and uncertain release fails the partition. Logical ownership drain does not hold the wire slot needed by old window end/recovery. Added leaf-only/no-await scope, grant/drain failure tests, baseline rejection of runtime PSP guest-GPA payload forwarding pending verified owned buffers, and typed AP VMSA/doorbell control-page lifecycles. Draft remains pending re-review.

### Focused re-review (historical; token ABI approval superseded)

**Verdict: Minor revisions.** All three original must-fix findings are resolved at plan level. Drain ordering preserves admitted faults and the wire slot needed by end/recovery. Non-GUP exclusion is now a mandatory MM capability gate. Async completion retains authoritative operation ownership; the wire payload has status/count/sub-status but no transaction cookie (`K/include/hyperv/hvhdk.h:700-705`), so delivery/progress semantics remain an external gate.

The reviewer requested two localized ABI clarifications: recover copy-window tokens after failed result delivery or interrupted waits, and define unbound initialization windows separately from partition-bound windows. Both are incorporated in section 4.

**Additional acceptance cases:** fail `BEGIN` result delivery after a confirmed grant; interrupt `END`; recover token/results during a pending drain; retry the same request without a second grant; race initialization begin with bind; inject failed bind rollback and verify that initialization reopens only after proven host-owned rollback.

Scoped copy windows, PSP rejection, and typed VMSA/doorbell lifecycles are coherent. This verdict approves the plan for presentation, not the unresolved Hyper-V/MM behavior or capability advertisement.

### Author response — existing host access and base-page scope, 2026-10-08

Replaced copy-window UAPI/capability proposals with enforcement of the existing modify-host-access path. OpenVMM phase 1b can acquire, copy, and release without lite-MM capability; phase 1a can still ship bounce-first unchanged. Added atomic range conflict rejection across VPs and the full pending/acquire/copy/release lifecycle. Exact existing attribute failure completion is unresolved; retain halt/stop rather than call TripleFault a clean denial. Kernel mapping revocation, no-GUP/no-PFN-export enforcement, control lifetimes, and safe reclaim remain mandatory. Whole-VM drain and async ownership machinery now apply to kernel ownership/teardown, not each copy. Large pages moved to a short deferred follow-up; 4 KiB operation and direct-map safety remain in scope.

**Review pending.** No implementation, build, or hardware result is claimed.

### Existing-interface revision review

**Verdict: Minor revisions; corrections incorporated.** Existing host-access acquire/release is the temporary-copy interface; no new Hyper-V operation, copy-token ioctl, or large-page support is required. Mandatory no-GUP/no-PFN-export enforcement, alias/direct-map revocation, owned backing, and verified reclaim remain.

The reviewer required phase-1b coordination of PSP/AP/doorbell page use with ordinary copies and removal of residual SHARED-only drain assumptions. The delivery note and companion plan now cover those runtime conflicts. Main-plan admission uses `DRAINING` independently of ownership: confirmed eligible copies/faults/releases finish before fault closure, and ownership labels cannot justify skipping the drain. Mapping eligibility requires fresh confirmed authorization; private commit revokes preceding grants and aliases.

Added ownership/teardown state-machine coverage independent of the ownership label. Exact clean GPA-attribute failure completion remains a narrow existing-protocol question, with halt/stop until resolved. This is plan review with targeted source evidence, not hardware execution.
