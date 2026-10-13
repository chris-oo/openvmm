# OpenVMM scoped SNP host access: phase 1b

## Status

The remaining issues and their acceptance gates are tracked in
[Outstanding OpenVMM SNP host-access issues](pending-openvmm-snp-host-access.md).

Phase 1b is not complete. The scoped-copy foundation is committed in jj change
ID `znmztzpx`. All pending code, configuration, and documentation changes have
been split into reviewed, default-off experimental commits. Committing these
snapshots does not resolve the VP-entry cancellation or GPA-intercept
completion blockers.

| Change ID | Logical chunk |
|-----------|---------------|
| `mlprwlwv` | Kernel descriptor and guest-backing ownership |
| `slknztno` | Bounded coordinator and standalone tests |
| `qxkpmovr` | Experimental MSHV scoped-copy integration |
| `mkprmznx` | Configuration, CLI admission, and Guide |
| `rkunsrtx` | Review findings and hardware acceptance report |

The teardown follow-up now retains RAM allocation and mapping owners with the
kernel partition fd. Runtime unmap checks are unchanged. The VP-entry race
and GPA-intercept completion remain unresolved.
The reviewed shared teardown interface is committed in change ID `pruuvzlm`.

This work uses existing MSHV acquire/release calls. It does not implement
Linux-MM enforcement or guest_memfd_lite backing.

## Committed foundation

Guest-memory backings can return owned guards around synchronous mapped
copies. Copies finish their guards before fallback. Release errors propagate
even if the copy also fails. Empty and fallback-only operations take no guard.

Copy-only policy becomes sticky after partition attachment. It prevents new
mapping exports and locks, but cannot revoke handles exported before attachment.
The callback contract now requires attachment before exports, locks, and
secondary mappers.

VaMapper checks mapping coverage and write permissions before admission. Its
scoped fault path cannot acquire another grant. Direct-mode copies do not call
the scoped hook.

GPT-6.1 Sol formal review and Claude Opus 5.5 rubber-duck review independently
approved this chunk. The latter identified extra callback work and an
unnecessary scoped callback in Direct mode. Both were fixed and re-reviewed.
All 73 foundation tests passed. Clippy, rustdoc, and workspace formatting passed
before the commit.

## Real SNP hardware acceptance

The candidate ran on `chris-mshv` with the published
`microsoft/openvmm-deps` release `0.3.0-155` SNP guest, Linux 6.18.53.
Its kernel SHA256 is
`1ac6e1ed682af6fd6cb464bdd135b742a7efaee71587abd4d9e152b992273512`.

The successful run used:

```bash
--hypervisor mshv:snp_scoped_host_access=true
--isolation snp --hv --no-vmbus --processors 1
--memory size=192MB,thp=off
--virtio-blk mem:256MB,pcie_port=blk,serial=BOUNCE-TEST,bounce
--virtio-net bounce:pcie_port=net:consomme:...
--virtio-vsock-path path/to/relay --virtio-vsock-bounce
```

The guest used an initramfs and a private disposable RAM disk. It ran block,
TCP, and in-process vsock verification, unbound and rebound all three virtio
drivers, then repeated the same verification.

| Check | Before rebind | After rebind |
|-------|---------------|--------------|
| Block operations | 7,680 writes and 7,680 verified reads | Same |
| Block request sizes | 512 B, 4 KiB, and 64 KiB | Same |
| Block workers | 1 and 8, sequential and scattered offsets | Same |
| TCP | 28 verified frames, 1,853,464 bytes each direction | Same |
| Vsock | 28 verified frames, 1,853,464 bytes each direction | Same |

The built-in drivers cannot be unloaded with `rmmod`. The harness discovers
each device by its virtio device ID and unbinds/rebinds its actual driver.
The vsock driver name is `vmw_vsock_virtio_transport`, not `virtio_vsock`.
All three unbind/rebind checks passed. The guest shut down normally, OpenVMM
returned zero, the partition set returned to its initial state, and the host
kernel log did not change.

Evidence is retained in the session's `files/scoped-hardware/` directory:
`lifecycle.json`, `lifecycle.serial.log`, `lifecycle.host.log`, and
`lifecycle.kernel-new.log`. The executable harness and guest init are retained
there too. Private relay sockets were removed.

### Problems caught by the hardware tests

The first updated candidate treated MMIO ioeventfd registration as a guest-RAM
operation. An unavailable ioeventfd optimization then poisoned the partition.
The patch now preserves the original ioeventfd registration and fallback
behavior. Actual SNP doorbell-page reservations remain separate.

With default THP eligibility, the workload completed the first two block
phases and then stopped making progress before completing the 64 KiB phase.
The harness timed out and confirmed cleanup. With `thp=off`, the complete
workload and driver cycles passed. This comparison does not establish the
precise THP failure mechanism. The experimental CLI now requires base-page
backing rather than enabling an unvalidated configuration.

These driver cycles prove successful teardown/recreation and subsequent I/O.
They do not prove an explicit private/shared/private operation on one
identified page. The published guest has neither kprobes nor ftrace enabled.
No explicit per-page transition helper has been built or run.

## Review

GPT-6.1 Sol formal review and Claude Opus 5.5 rubber-duck review found these
remaining problems in the MSHV chunk:

1. **Failure and retained-control teardown:** `unmap_range` applies runtime
   admission checks. Poisoned state and retained AP VMSA or SNP doorbell pages
   therefore reject cleanup unmaps. `PartitionMapper` expects those unmaps to
   succeed and can panic. The fix must stop execution, drain copies, and retain
   uncertain backing until kernel references are destroyed. Ignoring the
   error or bypassing the check while freeing backing is not safe.
2. **Poison versus VP entry:** the final poison check and VP-run ioctl are not
   atomic with the kick. A signal can be consumed after the check but before
   guest entry. A race-free entry cancellation contract is still needed.
3. **GPA-attribute completion:** the existing message and dispatch are present,
   but the completion/denial contract is not established. Both conflicting
   and nonconflicting intercepted requests still halt the guest. No
   `CompletePartitionIntercept` value encoding was guessed.

Persistent control-page replacement/unbinding also needs a defined lifetime.
The current reservations last until partition teardown.

### Teardown ownership follow-up

The memory manager transfers a token containing its RAM region handles and
primary VA mapper to scoped MSHV. Dropping `PartitionMapper` no longer requests
an early unmap after that transfer. Retained region handles also prevent the
memory manager from asynchronously removing RAM mappings on its own drop.
Other backends and ordinary MSHV retain their original drop-time unmaps.

MSHV stores the tokens with the partition fd in `MshvMemoryOwner`. The fd drops
before the tokens. Copy backends share that owner, so a copy that outlives the
partition still keeps both the fd and backing alive. Separately allocated
IGVM VMSA backing is retained too. BSP and AP VP descriptors, including the
GHCB mapping's kernel reference, drop before the partition owner.

This does not treat a failed release as successful or clear an uncertain
reservation. It keeps the backing until the kernel closes its references.
Explicit runtime unmaps still go through the original admission checks.

Three memory-manager tests cover ordinary drop-time unmaps, retained private
RAM, and actual file-backed `GuestMemoryManager::attach_partition` followed by
manager and partition destruction. Another test uses the actual MSHV owner
type to verify that the last copy owner closes the kernel reference before
releasing backing. All 109 targeted tests passed.
The file-backed test's reads are smoke checks; its retained-token and unmap
assertions establish the ownership contract. Both independent reviewers
approved the follow-up after these tests and the architecture guard were added.
All 253 integrated tests passed before the commit-split review fixes.

The updated candidate also passed the complete hardware workload with two
guest CPUs. `SCOPED_CPUS online=0-1` confirms both were online. All three
driver unbind/rebind cycles and both I/O rounds passed, followed by normal
shutdown. The partition set and host kernel log were unchanged. Evidence is
retained in the session's `files/scoped-teardown/` directory.

A separate two-VP restricted-injection attempt failed before guest boot with
an invalid GHCB message. Cleanup completed without a new host kernel error.
That configuration is not reported as working, and its boot failure was not
changed in this teardown patch.

### Remaining VP-entry cancellation contract

The inspected kernel's `MSHV_RUN_VP` argument contains only an output message
buffer (`include/uapi/linux/mshv.h:408-414`). It has no run-input cancellation
field or signal-mask argument. The root scheduler checks guest-entry work
inside the ioctl, but a userspace signal already handled before ioctl entry
does not provide a persistent cancellation request.

Writing suspend registers directly is not a workaround: the kernel rejects
`HV_REGISTER_EXPLICIT_SUSPEND` and `HV_REGISTER_INTERCEPT_SUSPEND` writes
(`drivers/hv/mshv_root_main.c:1198-1203`). The root-scheduler suspend path also
states that VP cancellation is not yet supported
(`drivers/hv/mshv_root_main.c:609-624`).

These findings explain why another userspace poison check or a direct
suspend-register write is not the fix. A supported persistent cancellation
contract still needs to be established. No kernel ABI was changed or guessed
in this follow-up.

The coordinator concurrency test initially used an immediate `try_lock`
assertion. Another waiting thread could briefly hold the ledger, giving a
false failure. Its bounded lock probe now still detects a lock held by the
callback's own thread, without requiring other threads to stop accessing the
ledger. The 48 MSHV tests then passed. At that stage, all 249 integrated tests,
including the base-page CLI test, passed. Clippy, rustdoc, and workspace
formatting also passed.

Rate-limited acquire/release counter samples show both directions operating.
They are not a complete grant-balance or pin-accounting proof.

Do not describe the MSHV chunk as finished until the blocking review findings
and explicit visibility-transition acceptance are resolved.

### Review fixes before splitting the pending changes

GPT-6.1 Sol and Claude Opus 5.5 independently reviewed the pending changes for
the commit split. The CLI now checks each NUMA node's THP setting and rejects
existing guest-memory backing files. Negative tests check the exact admission
error, and a positive test covers base-page NUMA backing.

Root reservations are bounded at 4,096 entries and use fallible allocation.
Tests verify capacity denial without poisoning and reuse of capacity after a
transient reservation completes. Permanent AP VMSA and SNP doorbell
reservations still do not support replacement or unregister. Exhaustion also
blocks transient PSP requests. Uncertain root-operation failures stop the
partition even when the guest first receives an AP-start error.

These prototype limits are recorded in the Guide. They are not silently
treated as completed lifecycle support.

Both reviewers approved the final fixes for local experimental commits.
Each code commit's exact intermediate tree was checked in an isolated jj
workspace using the existing package and build caches. Package checks,
clippy with warnings denied, rustdoc, and unit tests passed; workspace
formatting ran last before each commit. The ownership snapshot passed 36
MSHV tests, the standalone coordinator snapshot passed 47, and the integrated
MSHV snapshot passed 50. The configuration snapshot passed 151 tests with
the MSHV producer and resolver features enabled. An aarch64 MSHV compile
check also passed.

## Shared-page mid-copy revocation experiment

The `revocation-experiment` build feature adds an opt-in probe to the legacy
MSHV SNP bounce path. It does not change normal copy selection, production
fault policy, scoped grants, or guest ownership transitions. The runtime
selector is `OPENVMM_SHARED_REVOKE_EXPERIMENT=read|write:u8|u64|simd|rep`.

The probe runs while an actual virtio-blk request is pending. The cooperative
guest holds its SNP shared DMA mapping until device completion. The host
validates that the revoked 4 KiB page lies wholly inside that payload. A
single annotated copy stores a 64-byte prefix, publishes progress, and pauses
with its source, destination, and registers live. Another host thread
verifies the prefix, releases host permissions with zero ownership flags,
and resumes the copy only after the release returns successfully.

Eight hardware cases passed on `chris-mshv`: reads and writes with byte,
u64, SIMD, and `rep movsb` copies. In every case:

- The release returned success, then the next guest-memory access faulted.
- Linux delivered SIGSEGV (11), `SI_KERNEL` (128), CPU trap 13 (`#GP`).
- Independently measured progress was exactly 64 bytes. Guards and the
  uncopied tail were unchanged.
- Explicit RW reacquisition and a fresh full-primitive retry succeeded.
- The guest completed its I/O and powered off. Host dmesg, partition state,
  and lite allocation counters returned to their baselines.

The GP fault address and reported offset were zero. Even the write cases
reported `MemoryError.is_write=false`; the caller's original operation
direction is authoritative, not this diagnostic. The tests do not infer
partial progress from that offset.

These hardware cases used page-aligned 4 KiB payloads and same-page
revocation. Cross-page recovery was tested locally with `mprotect`, not
against Hyper-V. The hardware run did not use Linux `mprotect`, fd PTE
invalidation, guest-private handoff, or physical-PFN probes.

Two independent reviews approved the bounded experiment and its payload
shape fix. Feature-enabled tests passed 95 cases; default tests passed 91.
Clippy with warnings denied, rustdoc, formatting, and a static musl build
passed. The initial non-terminal launch aborted before the probe; a terminal
retry exposed the original 8 KiB eligibility mismatch with actual 4 KiB DMA
descriptors. Both failed attempts are preserved separately.

Successful evidence is retained in the session's
`files/shared-revocation-preflight-20261013-1512/hardware-matrix/` directory.
The tested VMM SHA256 is
`f2f016898cc7bbc81b79915ab200068d397d60e6ee903696ad95f6edef83b511`.

This establishes recoverable host-copy faults after shared-page permission
revocation. The retry is manual, not the production GuestMemory/VaMapper
automatic retry. It does not establish acquisition versus guest-private
handoff ordering, revocation starvation behavior, or the safety of removing
production reservations, locks, and per-copy release.
