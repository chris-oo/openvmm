# Outstanding OpenVMM SNP host-access issues

## Status and scope

MSHV SNP now always uses copy-only guest memory and owned bounce buffers for
supported device I/O. The host-access opt-in and legacy unconditional RW
acquisition branch are removed. CLI virtio block, software networking, and
in-process vsock select bouncing automatically. Non-isolated MSHV and KVM
retain their existing behavior.

The CLI rejects unsupported devices and backing configurations. VM-service
MSHV SNP admits only the minimal chipset and serial, selects base pages by
default, and rejects explicit NUMA huge pages or devices without bounce
admission. Use the CLI for bounced virtio devices.

Host grants remain fault-driven and retained until a guest ownership conflict
or control operation releases them. Ordinary-copy cancellation and a new
completion ABI are not prerequisites. The userspace control-page reservation
ledger and `snp_host_access` module have now been removed after checking the
backend contracts. Required control-operation permission releases, checked
RAM/range validation, backing retention, and CopyOnly export/pin restrictions
remain. The kernel and hypervisor retain their own control/lifetime enforcement.

A real two-CPU SNP lifecycle passed with plain `--hypervisor mshv` and no
per-device bounce options: block, TCP, vsock, all three driver cycles, 13
conflict releases and subsequent RUN returns, normal shutdown, and unchanged
host kernel logs. Evidence is in `target/mandatory-snp-bounce-hardware/`.
This uses legacy RAM backing, not the lite fd path.

The device-admission and branch-scope investigations are recorded in
`report-mshv-bounce-scope-and-io-enforcement.md`. They remain options-only;
no dedicated bounce-copy capability is implemented.
Identified-page ownership and failure-injection coverage remain separate
follow-ups.

## Historical scoped-model status (all remaining sections)

The sections below describe the superseded opt-in scoped-copy design.
Their cancellation and completion-ABI blockers do not apply to the current
retained-grant implementation. Preserve these sections as historical evidence.
Source citations below refer to the earlier implementation, including the
now-deleted `snp_host_access.rs`; they are not links to the current source.
That earlier source is retained in change ID `xzrmxqqp`.

Phase 1b remains a default-off experimental feature. Bounced block, software
networking, and in-process vsock work on the published SNP guest. Both guest
CPUs were online in the last acceptance run. All three drivers passed
unbind/rebind and subsequent I/O.

The backing-ownership teardown fix is complete: RAM region and VA owners
survive until the last kernel/copy owner closes. This does not establish
race-free VP cancellation or successful page-ownership transitions.

The committed implementation and hardware evidence are recorded in
[the scoped-access review report](review-openvmm-scoped-host-access.md).
This document tracks the remaining work, not completed isolation support.

| Issue | Status | Main dependency |
|-------|--------|-----------------|
| Persistent VP cancellation | Blocking | Kernel/hypervisor run contract |
| GPA-attribute completion and denial | Blocking | Existing Hyper-V response contract |
| Control-page replacement and unregister | Incomplete | Kernel/hypervisor reference lifetime |
| Identified-page transition acceptance | Not run | Full acceptance needs the first three contracts |
| Restricted-injection guest boot | Separate failure | GHCB boot-path diagnosis |

## 1. Persistent cancellation after uncertain host access (historical)

An unsuccessful acquire or release may leave a range's permissions uncertain.
The coordinator retains its exclusion, refuses new copies, and kicks VPs.
The userspace check immediately before `runner.run()` is not atomic with that
kick:

1. A VP passes the final poison check.
2. Another thread poisons the partition and signals the VP.
3. The VP handles the signal before entering the run ioctl.
4. The ioctl sees no pending signal and can dispatch the guest.

The post-run check cannot undo that execution. Another userspace check or
repeated signaling is not a race-free solution.

**Required contract:** persistent cancellation must prevent later guest
entries, interrupt existing execution, and acknowledge that the affected VPs
are quiescent. Entry, cancellation, rearm, and failure must have defined
ordering. A small kernel change may be needed; its ABI is not established.
Direct suspend-register writes are not a supported workaround in the
inspected MSHV kernel.

**Acceptance:** force cancellation immediately before dispatch and while
multiple VPs run. No VP may enter after acknowledged cancellation. Test
repeated requests, interruption, rearm, and failures without releasing
uncertain backing.

Evidence: `vmm_core/virt_mshv/src/lib.rs:470-486,766-768`;
`vmm_core/virt_mshv/src/x86_64/snp_host_access.rs:307-329`.

## 2. GPA-attribute completion and whole-request exclusion (historical)

The existing message definition and dispatch are already present. The handler
validates ranges and checks overlap against copy/control reservations.
However, conflicting and nonconflicting requests both currently halt the
guest with `TripleFault`.

Passing the overlap check means only that the instantaneous admission check
passed. It does not mean a transition was permitted, applied, or completed.
The existing message has no result field that establishes the reply format.
No `CompletePartitionIntercept` value or replacement GHCB response has been
guessed.

**Required contract:** establish the existing completion/denial protocol.
Then check and reserve the entire request atomically against other VPs'
copies and control operations. Keep that exclusion until authoritative
completion. A conflict must reject the whole request without partial
application or replay as success. Uncertainty must retain the exclusion and
stop execution.

**Acceptance:** cover permitted transitions, whole-request conflicts,
malformed/overflowing ranges, mixed conflicting/nonconflicting batches, and
new copy admission racing completion. Confirm the actual guest-visible
result, not just absence of a host error.

Evidence: `vmm_core/virt_mshv/src/x86_64/snp.rs:310-329,1361-1373,1439-1455`;
`vm/hv1/hvdef/src/lib.rs:4351-4355`.

## 3. Control-page lifetime, replacement, and failure semantics (historical)

PSP request/response reservations are transient and retire after success.
AP VMSA and SNP doorbell reservations remain until partition teardown.
Overlapping re-registration is rejected. Distinct retained registrations
consume the bounded reservation budget, including capacity needed by
transient PSP requests.

The coordinator permits at most 4,096 root-operation reservations and uses
fallible allocation. Hitting the limit does not itself poison the partition.
An issued operation with an uncertain outcome does poison it.

The requesting VP can still halt when admission fails, even without poisoning.
An AP-start ioctl error can first return a guest-visible error, then stop the
partition when its issued guard drops. That guest response does not prove
that the kernel/hypervisor released all references. Doorbell disable/rebind,
AP recreation, and reuse of old control pages are not supported lifecycles.

**Required contract:** identify each binding and verify terminal completion,
replacement, and unregister barriers before releasing its old reservation.
Keep these typed control references separate from ordinary host-copy grants.
Cancel an error locally only when the operation is proved not to have applied.

**Acceptance:** repeat AP/doorbell registration, disable and rebind, reuse
retired pages for I/O, exhaust and recover reservation capacity, and inject
control-operation failures. Confirm no stale references or ordinary-copy
exemptions remain.

Evidence: `vmm_core/virt_mshv/src/x86_64/snp.rs:238-259,1463-1495,1958-1985,2087-2111`;
`vmm_core/virt_mshv/src/x86_64/snp_host_access.rs:340-399`.

## 4. Explicit private/shared/private acceptance

Driver recreation plus subsequent I/O is useful lifecycle coverage, but it
does not identify a GPA and prove its ownership sequence. The published
guest's virtio drivers are built in; use unbind/rebind, not `rmmod`.

**Required evidence:** a guest helper must report one identified page and
exercise private, shared, and private states. Repeat with a deliberate
cross-VP copy conflict, control-page overlap, and uncertain-operation failure.
Record the request, authoritative result, grant retirement, and cleanup.
Do not deliberately load a private PFN through the host to demonstrate a crash.

A preliminary nonconflicting test can be built sooner. Full acceptance of
conflict and uncertain-failure cases remains gated on the contracts above.
The published fixture lacks kprobes/ftrace; no explicit per-page helper has
been built or run.

Evidence: `review-openvmm-scoped-host-access.md`, hardware-acceptance and
teardown follow-up sections.

## 5. Restricted-injection boot failure

The two-VP restricted-injection attempt failed before guest boot with an
invalid GHCB message. Normal two-VP SNP boot and the complete bounced-device
workload passed. Keep this diagnosis separate from backing teardown and from
the explicit transition acceptance above.

**Acceptance:** capture the failing GHCB protocol state, fix or explain the
rejection, and rerun boot plus the device lifecycle workload with restricted
injection enabled. Do not report that configuration as supported based on the
normal boot results.

## Relationship to kernel guest_memfd_lite work

Kernel owned-backing/MM work can proceed independently. The first kernel
prerequisite, change ID `polnznxl` in `LSG-linux-rolling`, blocks opt-in raw
PFNMAP lookups in core MM and the KVM/VFIO/MSHV fallback paths. It does not
expose a lite fd, revoke private-page aliases, or provide a complete
PFN-exposure policy.

Kernel change ID `unukynkz` adds an internal host-owned allocation step:
bounded lazy order-0 pages, direct-map removal, and retryable restore/scrub.
It has no fd, userspace mapping, guest binding, or private-page reclaim API.
It requires callers to maintain their admitted budget and owning memory-cgroup
context.

All seven backing tests, including the native alias test without a skip,
passed in a disposable two-CPU, non-SNP OpenVMM guest on the MSHV test host.
The running host kernel was not changed and no new tools were installed.
The guest selected the Hyper-V remote-TLB flush path. The native test checks
PTE removal/restoration and zero-after-restore; it does not prove stale-TLB
exclusion across CPUs or SNP ownership behavior. The bare-metal IPI flush
path was not exercised.

The vendor tree required x86 defconfig with `CONFIG_MSHV_ROOT=y`; stripped
configurations hit existing build dependencies. The VM was an ordinary
partition, not a root partition, so this is not root-mode coverage. The guest
also reported AMD chipset MSR `0xc001001f` access diagnostics from
`amd_bus_cpu_online` before the tests; those are
recorded as a separate platform/fixture limitation, not silently reported as
a clean boot. No unexpected diagnostic occurred during the seven tests, and
the host kernel log and partition set were unchanged.

The kernel work must still establish an owned backing fd, controlled userspace
aliases, complete non-GUP exclusion, public resource-admission policy, ownership
transitions, and confirmed guest-page reclaim. Public capability remains
withheld pending those gates and the full consumer audit, including privileged
`/proc/pagemap` PFN exposure.

MM enforcement alone does not cancel VPs, complete an intercept, or retire
control references. Conversely, finishing phase 1b on legacy pinned RAM does
not provide Linux-MM isolation.

See [the kernel implementation plan](plan-mshv-guest-memfd-lite.md) and
[the OpenVMM bounce plan](plan-openvmm-snp-bounce-io.md).
