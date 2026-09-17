# Arm CCA TDISP: implementation summary

Updated: 2026-09-17.

**OpenVMM now runs the unchanged kvmtool reference guest through TDISP
LOCK/RUN and a verified 64 MiB AHCI read on FVP.** The overall VMM test still
fails at UNLOCK and shutdown. This is a protocol/I/O milestone, not a claim
of clean teardown, reusable assignment or measured private-buffer DMA.

The detailed design, artifact pins and run history are in
[plan-arm-cca-tdisp.md](plan-arm-cca-tdisp.md).

## What worked

The guest found the modeled `0abc:aced` AHCI endpoint at `0000:01:00.0`.
Its original initrd script unbound the driver, wrote the kernel's TSM lock
and accept attributes, checked their values, and reprobed AHCI. OpenVMM
separately recorded successful native transitions to `Locked` and `Run`.

After acceptance, the guest kernel attached a 131072-sector disk. The script
read all 64 MiB with direct I/O and matched the reference hash:

```text
281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6
```

The guest recorded 132 AHCI MSI-X interrupts. These results are not based
only on PCI enumeration or a successful shell `accept` write: the host state
transitions, driver reprobe, disk read and matching data provide separate
evidence.

The kernel, initrd and patterned disk are byte-identical to reference run
`run-20260914-3`. OpenVMM uses PL011 console arguments and `quiet` to reduce
FVP console overhead. That run needed no guest helper, guest patch or firmware
rebuild. The L1 host payload remains separate from these guest artifacts.

The exact script and disk oracle are now stored in
[`vmm_tests/vmm_tests/test_data/cca_tdisp/`](vmm_tests/vmm_tests/test_data/cca_tdisp/),
alongside provenance and a deterministic disk generator. Their source no
longer depends on `target/` surviving `cargo clean`. This does not alter the
validated guest initrd.

## Changes by component

| Component | Main change | Role in the working path |
|---|---|---|
| `kvm` | guest_memfd flags/attributes, RIPAS and bounded prefault helpers; KVM/VFIO association; Arm SMCCC filters, registers and TIO exits | Exposes the pinned host-kernel interfaces without treating Arm hypercalls as x86 hypercalls. |
| `virt_kvm` | In-place memory ledger; full RHI request routing; pre-entry private prefault; serialized shared/private DMA conversion; TIO dispatch | Connects guest requests and memory transitions to one assignment service. Checks original x0, shared buffers, addresses and completion results. |
| `membacking` and `guestmem` | Partition-provided backing, explicit file offsets, revocable access policy and retained RAM-region ownership | Keeps CPU and device access on the intended guest_memfd backing rather than a second shared RAM copy. |
| `tdisp` | Additive native coordinator, bounded evidence snapshots, mutation handling and asynchronous service admission | Shares device state across evidence, LOCK/RUN, mapping and RAM work. Existing OpenHCL/VPCI protobuf behavior remains separate. |
| `vfio_sys` | Typed CCA TSM requests and strict Realm IOMMUFD primitives; shared ownership of the VFIO open file | Preserves syscall errors, residue and TSM status independently and keeps assignment handles alive. |
| `vfio_assigned_device` | Realm object owner, distinct resolver, access gate, fixed BAR/RID checks, shared-DMA ledger and native TDISP backend | Uses trapped shared BAR access before LOCK and blocks protected fallback access afterward. Maps shared RAM through retained VMAs with `IOMMU_IOAS_MAP`, matching kvmtool. |
| PCI core and KVM IRQ routing | Native assignment hooks, MSI-X route release, checked error latches and retained failed-route ownership | Supports the fixture's nonsecure MSI-X pages without relying on unchecked IRQ-route failures. |
| OpenVMM worker and topology | Selects `vfio-realm`, retains RAM ownership with the association, and gives its dedicated root selector-clear BAR addresses | Keeps ordinary CCA VFIO paths blocked. ECAM and other shared-device views remain separate from protected BAR resources. |
| Petri | Correct agent-free Linux direct boot setup | Does not add an empty agent disk or request the unsupported PL011 serial-agent RTS handshake. |
| Incubator and Flowey | Pinned DA fixture, separate unchanged guest inputs, native nextest inside one FVP boot and durable outcome reporting | Runs the real test before model shutdown and preserves guest results separately from fixture/model failures. |
| VMM test | `tdisp_ahci::boot_linux_direct_cca_tdisp_ahci` | Uses the original guest's autonomous sysfs/read/hash sequence, not an evidence-only helper or dormant preflight. |

## How the pieces fit

```text
Unchanged Linux guest: TSM sysfs + AHCI driver
  -> RSI_HOST_CALL / RHI
  -> Arm KVM exit and virt_kvm dispatch
  -> bounded native tdisp service
  -> Realm VFIO owner and access gate
  -> Linux TSM / IOMMUFD / RMM

Guest RAM visibility changes
  -> same admitted device transaction
  -> withdraw shared DMA mappings
  -> guest_memfd attributes and private prefault
  -> map newly shared backing before guest re-entry
```

The service owns the device coordinator. KVM holds weak service registrations;
the assignment retains its VM association and memory owner. This avoids a
registration ownership cycle. The checked access and conversion paths are
distinct from the still-incomplete process/shutdown recovery policy.

## Runtime evidence and result

The demonstrated run is:

```text
FVP: eb6b287c635f6e51633424680a74b0d1c68f6a850860ec34f24f57e976daf895
nextest: c44737e2-2933-4a58-94d3-15e7193217c3
```

Evidence root:

```text
vmm_test_results/cca-tdisp-exact-guest/fvp-single-boot-runs/
  single-boot-2630902-1789600632166144336/
```

`observed-io/linux.log` contains guest READY/LOCK/ACCEPT/I/O markers, kernel
enumeration and reprobe messages, and interrupt counters.
`observed-io/openvmm.log` records confirmed Locked and Run states, followed
by the UNLOCK guard failure. `observed-io/petri.log` records the VP halt and
test failure. The finalized `outputs/session-result.json` and native JUnit
preserve the failed overall verdict.

| Milestone | Result |
|---|---|
| Unchanged guest boots and finds AHCI | Demonstrated |
| Native TDISP LOCK and RUN | Demonstrated |
| 64 MiB direct read and reference hash | Demonstrated |
| AHCI MSI-X delivery | Observed |
| UNLOCK and guest poweroff | Not completed; OpenVMM stopped at its protected-mapping guard |
| Fixture teardown / FVP shutdown | Failed separately |
| Overall VMM test | Failed; do not relabel it as passed |

## What remains

OpenVMM currently refuses UNLOCK while its protected-mapping ledger contains
attempted mappings. kvmtool does implement UNLOCK: it sends the native state
request and restores nonsecure BAR access after success. Our guard is not
evidence that the host kernel cannot unlock.

Deferred work includes acknowledged protected-map release, checked UNLOCK,
forced-stop and parent-disconnect behavior, constructor-failure custody,
model shutdown and same-host reuse. The provisional worker hold does not
solve all those paths. The pinned kernel also has error-reporting gaps during
mapping rollback and destruction.

This guest run does not contain the later private-buffer instrumentation used
with kvmtool. It therefore does not independently prove that every OpenVMM
transfer used Realm-private buffers without a shared bounce path. Negative
isolation tests, production trust policy and physical-hardware qualification
also remain open. In-place QEMU debugging remains deferred.

## Commit and review state

Several foundations are committed, including the native host core and
evidence path (`zuvorsyr`, `smsrnmou`, `nxzsulql`, `xoqwunsu`), FVP fixture and
runner (`znkopxsz`, `owtsypxt`, `qmzlokou`), low-level helpers (`qwmpmtky`),
agent-free boot fix (`vqkonpku`), Realm resource (`lmlpotnx`) and PCI control
interfaces (`xxuzxupu`). These are jj change IDs, not an exhaustive history.

The remaining execution changes have now been split and committed:

| Change ID | Scope |
|---|---|
| `mktoyytw` | Native TDISP operations and RAM-worker contract, with existing-consumer compatibility edits |
| `lqzlyxul` | Checked KVM IRQ routing and retained failed-route ownership |
| `nsvzyuut` | Realm VFIO access, shared-DMA backend and PCI frontend |
| `tusnzrsq` | Full KVM RHI/TIO and memory-conversion runtime |
| `yzoxvwlz` | OpenVMM setup, RAM-region owner wrapper and private PCI resource view |
| `ozppvpsq` | Optional dormant-object diagnostic, separate from the guest test |
| `ykvolqkt` | Unchanged-guest VMM test, exact recovered script, disk oracle and generator |
| `tquwpomz`, `vosxqvox` | Stabilized the queued-cancellation test without changing production behavior |

Each candidate was reconstructed, reviewed and validated before commit,
including its relevant Arm consumers. The backend and frontend remained
together because their shared private gate otherwise had no production
consumer. The plan records per-chunk results.

The final integrated check passed all 525 tests. It first exposed a test-only
waiter-ordering race; the corrected cancellation test passed 20 stress
iterations before the full run passed.

The provisional shutdown hold is retained separately as change `kwtwmymp`
on `cca-assignment-deferred-shutdown`, unapproved and outside the execution
commits. Parent-disconnect and constructor-failure review findings remain
open. The recorded FVP run used the original combined tree, including that
hold; source-level checks of the split chain are not a new runtime result.

## Firmware recovery and committed-stack rerun

After `cargo clean` removed the original FVP binaries, TF-A, RMM and EDK2
were rebuilt from the same pinned source revisions. The verified artifacts,
manifest and checksums now live in
`.packages/cca-tdisp-fvp/artifacts/rebuild-20260917-verified/`, outside
`target/`. The build recipe and source pins are tracked under
`petri/incubator/platforms/rebuild-realm-firmware.{py,json}`.

The rebuilt BL1/FIP have new hashes. The host kernel, original guest kernel,
initrd and patterned disk were recovered without rebuilding them.
`.packages/cca-tdisp-runtime/` holds the restored runtime inputs and toolchain.
The plan records the recovery commands and all firmware output hashes.

The full single-boot rerun on the committed execution stack, without the
deferred shutdown hold, reproduced LOCK/RUN and the verified 64 MiB read.
OpenVMM confirmed Locked at 104.591285510 s and Run at 106.941596290 s.
The guest reported the expected hash and 132 AHCI MSI-X interrupts.

The overall test still failed at the protected-mapping UNLOCK guard:
`SingleStep` replaced the expected poweroff. Native nextest failed after
541.041 s, fixture teardown failed at `realm-binding`, and the launcher
returned 1. Result preservation succeeded. This is new protocol/I/O evidence
with rebuilt firmware, not a clean-lifecycle pass or byte-identical replay.

The new invocation is `single-boot-2987526-1789612384327479418` under
`vmm_test_results/cca-tdisp-committed-stack/fvp-single-boot-runs/`.
Its `outputs/session-result.json` records the separate outcomes. The plan
records the exact FVP/nextest identities and evidence locations.
