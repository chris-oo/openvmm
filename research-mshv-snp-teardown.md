# MSHV SNP teardown failure on chris-mshv

Investigated and tested with a patched host kernel on 2026-10-02.

## Summary

**This is a host-kernel teardown failure, not a guest boot failure.**

An SNP guest boots, reaches userspace, receives timer interrupts, and powers
off. OpenVMM exits with status 0. During memory teardown, MSHV tries to
reclaim host access to the guest RAM before removing its guest mapping.
Hyper-V rejects that request with `HV_STATUS_OPERATION_DENIED`.

The driver then loses track of that RAM region without unmapping or
unpinning it. Partition teardown cannot find the missing region, so its
unmap pass leaves the RAM mapped. The subsequent transition to
`INSECURE_DIRTY` also returns `HV_STATUS_OPERATION_DENIED`. The driver
abandons partition destruction.

**The ordering fix now passes on the host.** The revised kernel unmaps
guest GPAs before reclaiming host SPA access, and skips redundant unmaps
after partition finalization. Four IGVM boots and one direct boot passed
with no teardown warnings or retained guest partitions. Tracing confirmed
RAM/VMSA unpinning, memory withdrawal, and successful partition deletion.

The precise Hyper-V rule behind the original rejection is not exposed by
the status code. However, the patched trace directly confirms that
unmap-before-reclaim succeeds without moving VP/SEV-control/runnable
cleanup ahead of RAM reclamation. Failure-path ownership hardening remains
separate work; this patch does not implement it.

## Environment and artifacts

| Item | Value |
|---|---|
| SSH target | `chris-mshv` |
| Host name | `openvmm-mshv-1` |
| Original kernel | `6.18.34.mshv3`, `#1 SMP @1790013136` |
| Current kernel | `6.18.34.mshv3-mshv-snp-injection-unmap-first-v2`, build 3 |
| Kernel source | `/home/coo/ai/leafeon/LSG-linux-rolling` |
| Kernel source parent change ID | `rrnxqykx` |
| Kernel patch change ID | `uosolxky` (committed locally; not pushed) |
| Kernel source bookmark | `user/cho/mshv-snp-normal-injection` |
| OpenVMM source parent change ID | `myrmvork` |
| OpenVMM binary used | `/home/chris/snp-injection-test/fail-closed-7b5e9dac/openvmm-snp-main` |
| Guest image | `/home/chris/snp-injection-test/normal-injection-x2apic-1cpu.igvm` |
| VM configuration | SNP, MSHV, 160 MiB RAM, one VP, no VMBus or virtio devices |

Artifact SHA-256 values:

```text
OpenVMM:
556a5e41f0b6c182955efac9b289c58c685d95d88be1cf4d37254d38d0f37857

Guest IGVM:
bd2a98777d8fe32a9bc58b6d15352479e0150190adecbf7d922ed518b433bae4

Installed /boot/vmlinuz-6.18.34.mshv3:
90a5d5908dfa560625773cb485415dba33b477488976c572c856f469ef896047
```

The older kernel image in `snp-injection-test/host/` has a different hash
from the installed kernel. It is not evidence of the exact running build.
The checked-out kernel source explains the observed control flow, and
probes confirmed the critical ordering in the original running kernel.
The later build/deployment experiment is described below.

## Reproduction

Run from a terminal with a TTY:

```bash
ssh -tt chris-mshv '
  ulimit -c 0
  cd /home/chris/snp-injection-test/fail-closed-7b5e9dac
  sudo -n env OPENVMM_LOG=info \
    timeout --foreground --kill-after=5s 120s \
    ./openvmm-snp-main \
    --hypervisor mshv --isolation snp --hv --no-vmbus \
    --memory 160MB --processors 1 --com1 console \
    --guest-shutdown-action exit \
    --guest-reset-action exit:2 \
    --guest-crash-action exit:3 \
    --igvm ../normal-injection-x2apic-1cpu.igvm \
    --igvm-personality linux-direct
  status=$?
  printf "\nOPENVMM_EXIT_STATUS=%s\n" "$status"
  sleep 1
  sudo -n dmesg --color=never --since "2 minutes ago"
  exit "$status"
'
```

**On the original kernel, each reproduction leaks a partition and guest
memory.** Avoid an unbounded boot loop when testing that version. The
patched kernel's results are recorded below.

Two runs reproduced the problem, creating partitions 14 and 15. Both
guests printed:

```text
SNP_ENCRYPTION_CONFIRMED
SNP_NORMAL_INJECTION_PASS
reboot: Power down
guest halted reason=PowerOff
OPENVMM_EXIT_STATUS=0
```

In the traced run, Hyper-V timer interrupt counts increased from 449 to
486. No driver-unbind test or shared-to-private guest request was needed.

The first run produced:

```text
[682186.003974] misc mshv: p14: Failed to regain access to memory, unpinning user pages will fail and crash the host error: -5
[682186.029682] hv_call_set_partition_property: HV_STATUS_OPERATION_DENIED
[682186.031397] misc mshv: p14: Failed to set isolation state to INSECURE_DIRTY
[682186.033131] misc mshv: p14: Failed to destroy SNP state: -5
```

The pre-existing log contained the same four-message sequence for
partitions 4, 5, 7, 8, 9, 11, 12, and 13.

## Runtime trace: the first error and its consequences

Temporary kprobes and existing MSHV tracepoints ran in a separate tracefs
instance. They recorded the SPA host-access wrapper, failed hypercall
statuses, region unmaps, partition release, and partition-property calls.
The instance and all diagnostic probes were removed afterward.

The following is the relevant trace, with kernel pointers omitted:

| Order | Thread | Operation | Result |
|---|---|---|---|
| 1 | `basic_device_th` | Release SPA host access, partition 15, one page, flags `MAKE_EXCLUSIVE` | 0 |
| 2 | `basic_device_th` | Release SPA host access, partition 15, 40,960 pages, flags `MAKE_EXCLUSIVE` | 0 |
| 3 | `memory_manager` | Acquire SPA host access, partition argument 15, 40,960 pages, read/write access, flags `MAKE_SHARED` | Raw status `0x8`; Linux return `-5` |
| 4 | `memory_manager` | Partition release and partition destruction, partition 15 | Entered |
| 5 | `memory_manager` | `mshv_region_unmap()` for a remaining region | 0 |
| 6 | `memory_manager` | Set isolation control, property `0x5000d`, value 0 | 0 |
| 7 | `memory_manager` | Set isolation state, property `0x5000c`, value 2 (`INSECURE_DIRTY`) | Raw status `0x8`; Linux return `-5` |

There was **no `mshv_region_unmap()` before the failed RAM host-access
acquisition**. The later unmap did not remove the RAM mapping:

```text
/sys/kernel/debug/mshv/partition/15/stats
PtVirtualProcessors : 1
PtDepositedPages    : 1024
PtGpaPages4K        : 40960
PtPartitionId      : 15
```

Partition 14 had the same relevant counters. In each case, 40,960 4 KiB
GPA pages equal the VM's full 160 MiB RAM allocation. The retained
partition also reports 1,024 deposited pages and one VP.

`0x8` is `HV_STATUS_OPERATION_DENIED`
(`include/hyperv/hvgdk_mini.h:26`). Linux maps it to `-EIO`, or `-5`
(`drivers/hv/hv_common.c:775,819-834`).

## Source-level failure path before the patch

Kernel paths below are relative to
`/home/coo/ai/leafeon/LSG-linux-rolling`. OpenVMM paths are relative to
this report's repository. Kernel line references in this baseline section
refer to parent change ID `rrnxqykx`, before the destructor was changed.

### 1. OpenVMM removes memory mappings during teardown

`PartitionMapper::drop()` calls `unmap_region()` before its underlying
virtual-address mapping goes away:

- `openvmm/membacking/src/partition_mapper.rs:134-150`

The MSHV mapper sends `unmap_user_memory()` for each mapped region and
then removes it from its own tracking:

- `vmm_core/virt_mshv/src/lib.rs:1024-1045`

The observed failing host-access request runs on the `memory_manager`
thread, before partition release.

### 2. The kernel removes the region from its list before cleanup succeeds

`mshv_unmap_user_memory()` calls `hlist_del()`, drops the region reference
with `mshv_region_put()`, and returns 0:

- `drivers/hv/mshv_root_main.c:1882-1915`

The region destructor tries `mshv_region_share()` **before**
`mshv_region_unmap()`. If sharing fails, it logs the warning and returns
without unmapping, invalidating/unpinning, or freeing the region:

- `drivers/hv/mshv_regions.c:348-375`

The callback returns `void`, so this error does not reach the unmap ioctl.
OpenVMM sees success and clears its tracking entry. The kernel region
has already been removed from the partition list and its final reference
has been consumed.

Keeping the pages pinned avoids the crash described by the warning.
There was no host panic in either run. The observed outcome is lost
cleanup ownership and leaked resources, not a demonstrated host crash.

### 3. Sharing requests host ownership of the RAM's SPA pages

`mshv_region_share()` requests read/write host access with `MAKE_SHARED`:

- `drivers/hv/mshv_regions.c:200-221`

`hv_call_modify_spa_host_access()` selects
`HVCALL_ACQUIRE_SPARSE_SPA_PAGE_HOST_ACCESS`. For `MAKE_SHARED`, its
zeroed input leaves the hypercall's `partition_id` field at zero; it
only fills that field for `MAKE_EXCLUSIVE`:

- `drivers/hv/mshv_root_hv_call.c:1152-1213`

Thus the trace's partition argument identifies the wrapper's caller; it
does not mean the acquire hypercall carries partition ID 15. Do not
"fix" this by changing the partition ID without confirming the ABI.

The failure is from this SPA ownership operation, not OpenVMM's separate
runtime GPA host-access acquisition for shared I/O pages.

### 4. Partition teardown cannot recover the missing RAM region

`destroy_partition()` unmaps regions still in `pt_mem_regions`, then
calls `destroy_snp_partition_state()`:

- `drivers/hv/mshv_root_main.c:2882-2897`

The failed RAM region is no longer in that list. The runtime counters
confirm that its 40,960 GPA mappings remain.

`destroy_snp_partition_state()` explicitly suspends VPs, clears their
SEV control registers, clears the runnable bit after completed import,
and then requests `INSECURE_DIRTY`:

- `drivers/hv/mshv_root_main.c:2797-2857`

Clearing isolation control succeeds in the trace. The state transition
fails with `OPERATION_DENIED`. Remaining RAM mappings are the leading
explanation, but Hyper-V's exact rejection condition was not inspected.

That failure returns from `destroy_partition()` before VP removal,
partition finalization, list removal, region release, memory withdrawal,
and partition deletion:

- `drivers/hv/mshv_root_main.c:2890-2895,2909-2968`

This explains the retained debugfs partitions, VPs, and memory counters.

## Kernel patch and deployed results

### Final change

The complete, apply-ready patch is saved alongside this report:
[mshv-snp-unmap-first.patch](mshv-snp-unmap-first.patch), including the
committed message, author, and trailers. It applies from
the kernel repository root to the baseline in parent change ID `rrnxqykx`
and matches the source used for the deployed v2 kernel.

The only kernel source change is in
`drivers/hv/mshv_regions.c:350-384`, in change ID `uosolxky`:

```diff
@@
 	if (region->mreg_type == MSHV_REGION_TYPE_MEM_MOVABLE)
 		mshv_region_movable_fini(region);

+	/* Finalizing the partition already removes its GPA mappings. */
+	if (partition->pt_initialized) {
+		ret = mshv_region_unmap(region);
+		if (ret) {
+			pt_err(partition,
+			       "Failed to unmap memory region (guest_pfn: %llu): %d\n",
+			       region->start_gfn, ret);
+			return;
+		}
+	}
+
 	if (mshv_partition_encrypted(partition)) {
 		ret = mshv_region_share(region);
@@
-	mshv_region_unmap(region);
-
 	mshv_region_invalidate(region);
```

For a still-initialized partition, the destructor now unmaps the region
before reclaiming its SPA pages, and checks the unmap result. It still
does not unpin pages if either operation fails.

For a finalized partition, the destructor skips the redundant unmap and
continues through host-access reclamation and unpinning. The existing
partition teardown calls `hv_call_finalize_partition()` and sets
`pt_initialized = false` before dropping the remaining regions
(`drivers/hv/mshv_root_main.c:2949-2961`).

No OpenVMM code was changed.

### First iteration: why the finalization guard is needed

The first experimental kernel, release
`6.18.34.mshv3-mshv-snp-injection-unmap-first`, moved unmap before share
and checked its return value, but had no finalization guard.

The guest powered off and the original four-error sequence disappeared.
Its partition was deleted. However, the host logged:

```text
[191.870476] misc mshv: p2: Failed to unmap memory region (guest_pfn: 68719476735): -5
```

That GFN is the one-page VMSA region at GPA `0xfffffffff000`. Partition
teardown had already unmapped it and finalized the partition. Checking
the redundant unmap in the destructor caused an early return before its
share/unpin steps. Partition disappearance alone was therefore not enough
to declare the experiment successful.

The second iteration added the `pt_initialized` guard. Its trace confirms
that the final VMSA page is reclaimed and unpinned, without a second
post-finalization unmap.

### Build and deployment

The existing `.copilot-snp-build` configuration and GCC build cache were
reused. The final commands were:

```bash
cd /home/coo/ai/leafeon/LSG-linux-rolling
jj diff --git drivers/hv/mshv_regions.c | scripts/checkpatch.pl --no-tree -
make -s -j16 O=.copilot-snp-build LOCALVERSION=-unmap-first-v2 bzImage modules
make -s O=.copilot-snp-build LOCALVERSION=-unmap-first-v2 \
  INSTALL_MOD_PATH=<staging-directory> INSTALL_MOD_STRIP=1 modules_install
```

Checkpatch reported no errors or warnings. The build completed.
The image, modules, `System.map`, and configuration were installed under a
new release name. `depmod` and `dracut` completed on the host.

Final installed image SHA-256:

```text
/boot/vmlinuz-6.18.34.mshv3-mshv-snp-injection-unmap-first-v2
0fff0a4fbb7b155e3ddd777b07257f91658bb7693aee8fe7f7da03b69d59718f
```

The host was rebooted once for each iteration. It currently runs:

```text
6.18.34.mshv3-mshv-snp-injection-unmap-first-v2
#3 SMP Fri Oct 2 14:11:00 PDT 2026
```

Both experimental entries were added to `/boot/grub2/custom.cfg`.
The final entry is `snp-unmap-first-v2-uosolxky`. Existing kernels and boot
entries were kept. `grub2-reboot` selected each test for one boot only.
The saved default remains `snp-two-bit-xtulsovu`; `next_entry` is now empty.
**A later ordinary reboot will not automatically select the patched kernel.**

Installation artifacts and boot-configuration backups are in
`/home/chris/snp-teardown-03b4b8d5/`.

### Final validation

The unchanged `openvmm-snp-main` binary ran five guests:

| Test | Partition | Timer interrupts | Exit | Result |
|---|---|---|---|---|
| IGVM 1 | 2 | 447 -> 479 | 0 | Guest powered off; partition deleted |
| IGVM 2 | 3 | 452 -> 482 | 0 | Guest powered off; partition deleted |
| IGVM 3 | 4 | 448 -> 480 | 0 | Guest powered off; partition deleted |
| Direct boot | 5 | 383 -> 415 | 0 | Guest powered off; partition deleted |
| IGVM hypercall proof | 6 | 449 -> 481 | 0 | Full unmap/reclaim sequence confirmed |

Every guest confirmed SNP encryption and reached userspace. After every
run, the debugfs partition set returned to its baseline: only partition 1.
The host emitted no teardown warnings or failed-hypercall probe events
during these runs.

For partition 6, the final runtime trace establishes this sequence:

| Operation | Pages/value | Result |
|---|---|---|
| Unmap guest RAM GPAs at GFN 0 | 40,960 pages | 0 |
| Acquire read/write SPA host access with `MAKE_SHARED` | 40,960 pages | 0 |
| Unpin guest RAM | 40,960 pages | Called |
| Unmap VMSA GFN `0xfffffffff` | One page | 0 |
| Clear isolation control | 0 | 0 |
| Set isolation state to `INSECURE_DIRTY` | 2 | 0 |
| Finalize partition | Partition 6 | Hyper-V status 0 |
| Acquire VMSA SPA host access with `MAKE_SHARED` | One page | 0 |
| Unpin VMSA | One page | Called |
| Withdraw deposited memory | 1,024 pages | Hyper-V status 0 |
| Delete partition | Partition 6 | Hyper-V status 0 |

The first three IGVM runs had the same reclaim/unpin/finalize/delete
results. Direct boot reclaimed and unpinned all 40,960 RAM pages and
withdrew 1,024 deposited pages before deleting its partition; it did not
use a separate tracked VMSA region.

This is stronger evidence than guest exit status or debugfs disappearance:
the trace confirms that both RAM and the separate IGVM VMSA page reach
their unpin calls, and that Hyper-V accepts memory withdrawal and deletion.

This is real resource release, not suppression of an error: unpinning
releases the driver's hold on the backing pages. Their final return to
the Linux allocator also depends on the remaining userspace mappings and
references being released. We traced the unpin calls, not each page's final
allocator release. The remaining ownership concern is the failure path,
not deliberate retention of memory on the tested successful path.

## Fix status and remaining proposals

### A. Fix the kernel's region teardown ordering

In `mshv_region_destroy()` or a new fallible cleanup stage, remove the
guest GPA mapping **before** asking to make its SPA pages host-shared.
Check the unmap result. Unpin pages only after host access is restored.

This order also appears in the kernel's SNP panic cleanup:

- `drivers/hv/mshv_root_main.c:3685-3695`

This is now implemented and runtime-tested, with the finalization guard
described above. For these guests, RAM reclaim succeeds before the later
VP/SEV-control/runnable cleanup. Moving that cleanup earlier was not needed.

### B. Keep cleanup ownership until the operation succeeds

Do not remove a region from `pt_mem_regions` and exhaust its last
reference before fallible unmap/reclaim operations complete.

Move those operations out of the final `kref` callback where an error
must reach the caller. On failure, retain a tracked, pinned region with
enough state to retry or finish cleanup during partition destruction.
Serialize this with region users and partition teardown; handle partial
unmap or reclaim progress explicitly.

For a live unmap ioctl, report failure instead of returning success.
This is not a standalone user-space fix: OpenVMM's memory mapper assumes
valid unmaps cannot fail
(`vmm_core/virt/src/generic/partition_memory_map.rs:9-17`), and its
`PartitionMapper` uses `expect("unmap cannot fail")`. Kernel error
reporting and the user-space lifetime contract must be addressed together.
Do not free the backing mapping merely because cleanup returned an error.

### C. Make partition shutdown an ordered, recoverable operation

Audit partition-wide teardown as a separate path from live region removal:
quiesce VPs and device access; satisfy the Hyper-V SEV-control and runnable
requirements; unmap every tracked region; perform the required isolation
transition; restore host access; then unpin and release resources.

The exact order of the isolation transition and SPA reclaim must be
validated against Hyper-V's contract. Split the existing helper if needed
so quiescing can occur before memory reclamation without losing the
required transition ordering.

Never ignore unmap errors or force-unpin inaccessible pages. If cleanup
cannot finish, keep an explicit cleanup owner and surface the failure,
rather than abandoning an untracked region or a zero-reference partition.

### D. Improve diagnostics

Log the raw hypercall status and completed repetition count at the failed
SPA operation, plus the affected GPA range and cleanup phase. The current
`-EIO` message hides the first `OPERATION_DENIED`; the named status appears
only at the later isolation transition.

Proposal A fixes the reproduced shutdown failure. B remains necessary
hardening for genuine unmap/reclaim failures. C and D remain audit and
diagnostic proposals; this experiment did not require a new partition
shutdown API or a different isolation-state transition sequence.

## Validation required for a fix

The deployed kernel satisfies these criteria for the tested boots:

1. The guest reaches userspace and powers off normally.
2. Host dmesg contains no reclaim or isolation-state teardown errors.
3. The new partition disappears from debugfs after the VM exits.
4. Guest RAM pins, GPA mappings, VPs, and deposited pages are released.
5. Repeated boot/shutdown cycles do not accumulate resources.

Direct boot also passed. Still cover multiple VPs, failed/incomplete
launch, VMM termination, non-SNP guests, and injected unmap/reclaim failures.
Failure tests must show that pinned pages remain tracked and are never returned
to the allocator before host ownership is restored.

## Scope and remaining uncertainty

The kernel ordering patch was built, deployed, and tested. It resolves
the reproduced normal SNP shutdown failure without any OpenVMM changes.
The kernel fix is committed locally as change ID `uosolxky`, with the
message `mshv: unmap SNP regions before reclaiming host access`. It has not
been pushed.

The patch is intentionally narrow. It does not solve the existing loss
of region ownership or success returned by the unmap ioctl if a different
failure reaches the final-reference destructor. Returning early still
protects inaccessible pages from unsafe unpinning, but a complete
failure-path fix must preserve a cleanup owner and handle partial progress.
Partition teardown also still ignores some hypercall results, including
finalization; the new guard relies on the existing `pt_initialized`
lifecycle. The tested finalization calls all succeeded.

The first Hyper-V rejection is now known to be `OPERATION_DENIED`, not
merely an unspecified `-EIO`. Its detailed internal precondition remains
unknown. The effectiveness of unmap-before-reclaim is now confirmed for
the tested guest shutdown paths, not for arbitrary live region removal
or concurrent guest/device access.

Diagnostic probes and the tracing instance were removed, and no
reproduction process remains. The host reboots cleared the original leaked
partitions, including 14 and 15. No guest partitions accumulated on v2.

Captured host and guest logs are saved in:

```text
/home/coo/.copilot/session-state/03b4b8d5-26d8-45bf-b042-8c27d0f91e80/files/
  host-dmesg-before.txt
  snp-boot-reproduction.txt
  snp-traced-boot.txt
  unmap-first-boot.txt
  unmap-first-v2-validation.txt
  unmap-first-v2-trace-and-host.txt
  unmap-first-v2-hypercall-proof.txt
  v2-kernel-trace.txt
  v2-host-dmesg-full.txt
  v2-igvm-1.log
  v2-igvm-2.log
  v2-igvm-3.log
  v2-direct-1.log
```

The original failing trace's relevant events are preserved in this report.
The original traced boot transcript does not include that trace buffer:
it was read separately before removing the diagnostic instance. The
patched-kernel trace buffers and guest logs were saved in full.

The first four-run validator completed its guest and tracing checks but
then rejected an ISO timestamp while reading dmesg. The host log was read
separately, and the timestamp format was corrected. The final
`igvm-proof` run completed all checks and printed `VALIDATION_PASSED`.
