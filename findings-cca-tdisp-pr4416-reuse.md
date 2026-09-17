# Sharing TDISP machinery with microsoft/openvmm#4416

Created: 2026-09-17 UTC. Updated: 2026-09-18 UTC.

Status: **Background analysis; the main plan is authoritative.**
The current implementation design, commit sequence, acceptance gates and
review are maintained in [section 4 of plan-arm-cca-tdisp.md](plan-arm-cca-tdisp.md#4-integrating-with-vmdevicestdisp).
The draft sketches below are retained as research context, not a second
implementation plan.

## Finding

**We can converge on the same host-side TDISP machinery, but rebasing alone
will not do that.** The useful change is to separate the existing host
state machine from its VPCI protocol policy, then make both the VPCI host
target and native CCA adapters use one common host core.

Do not replace our native path with `VpciClientTdispState`, or implement
`TdispResourceValidationInterface` in OpenVMM and assume that completes CCA.
Those are parts of OpenHCL's **guest-side consumer** of a host TDISP device.
Our OpenVMM instance is the **host** that services Linux Realm requests and
owns the VFIO/IOMMUFD assignment. The roles are different.

This revises the earlier additive approach in `plan-arm-cca-tdisp.md`:
instead of keeping the legacy emulator and native coordinator separate,
migrate their matching host behavior into one implementation. Keep transport
and platform differences explicit. This is a proposed refactor, not an API
already provided by the PR. The selected approach below shares the complete
lifecycle/transaction engine while retaining thin VPCI and native facades.
It does not force their different evidence APIs into one artificial protocol.

### Scope and source versions

- Upstream: [microsoft/openvmm#4416], open when inspected, titled
  `tdisp_openhcl: Implement necessary infrastructure for TDISP under VPCI Relay`.
- Inspected PR head: `f626c6d7e9798803d411daa7cc0dfbb543a7caf6`.
- Compared PR base: `711723c4cef8601cb94838f38a230daa05fd4cc1`.
- Current baseline: rebased jj change ID `tkorwyzw`, on `cca-kvm-tdisp`,
  above merge `tmqyvsty` and MPIDR fix `vvutrqnt`.
- This document was saved in change `nxuyxowl` before the rebase and is being
  updated there, now above the rebased baseline. No implementation or history
  changes are part of this planning update.

Upstream links below pin the original PR investigation. References C1-C7
identify current code read again for this update.

### Completed prerequisites and measured baseline

The history merge and conflict repairs are done. Repairs were squashed into
their owning changes; they did not consolidate the two TDISP engines.
The original `TdispHostStateMachine` and native `host::Coordinator` still
own separate transition implementations. [C1][L3]

The rebased stack passed 609 host unit tests and 71 Arm KVM unit tests.
Its full FVP TDISP rerun reached Locked at 104.361491030 s, Run at
106.697004300 s, and the original guest completed the matching 64 MiB read
with 132 AHCI MSI-X interrupts. The full test still failed at the existing
protected-mapping UNLOCK guard, followed by fixture/launcher cleanup failure.
This is the behavior-preservation baseline, not a passing lifecycle test.

```text
Source change: tkorwyzw
Invocation: single-boot-3788265-1789703740648468672
FVP: 5cc6553999afe3350aba6fd08580b9a1fa9b0ffb879157b87bdabc4db4412891
nextest: 1f3af87c-fee9-4359-86cb-b0e78714760d
Evidence: vmm_test_results/cca-tdisp-rebased-stack/fvp-single-boot-runs/
          single-boot-3788265-1789703740648468672/
```

The merge's separate-backing QEMU and v15 FVP CCA boot tests also passed
after `vvutrqnt`. Keep that fix; it is not part of the TDISP refactor.

## 1. What the PR actually supplies

The existing `tdisp` crate already had `TdispHostDeviceInterface`,
`TdispHostDeviceTarget`, `TdispHostDeviceTargetEmulator`,
`TdispHostStateMachine`, and `TdispGuestRequestInterface`. The PR extends
that machinery; it does not introduce all of it. [P0][P1]

| Piece | Role and relevance |
|---|---|
| Existing host state machine and backend callbacks | Negotiate, bind/lock, start, get reports, and unbind. This is the right layer to converge with our host coordinator, after separating policy and fixing uncertain-outcome handling. |
| New `tdisp_modify_mmio_range` / `request_modify_mmio_range` | Host-side Block/Unblock notification with range ID, GPA and byte length. A useful common operation boundary, but not a complete CCA mapping transaction. |
| `tdisp::devicereport` | Existing report parser and descriptors, plus new serialization support. Useful shared representation and fixture generation. |
| New `VpciClientTdispState` | OpenHCL-side state, report cache, accepted-BAR records, and DMA-unblock tracking. Not a native-host assignment owner. |
| New `TdispResourceValidationInterface` | Guest-side firmware hooks around bind/start and MMIO/DMA acceptance. Its context includes VTL, device ID and platform isolation. |
| New `TdispRelayedDeviceTarget` and isolation report types | Report resource classification to the VPCI guest. They are not another interface for issuing host TDISP commands. |
| VPCI relay changes | Drive attestation/resource acceptance from PCI command-register writes, queue conflicting writes, and implement isolation queries. |

The host versus relay distinction is explicit in the PR:
`ChipsetDevice::supports_tdisp()` becomes `supports_tdisp_host()`, and
`supports_tdisp_relay()` is separate. The VPCI server sends TDISP commands
to the former; `RelayedVpciDevice` implements the latter. [P1][P2][P3]

The new client selects SEV-TIO or TDX Connect and rejects CCA. Its validator
factory currently selects the no-op implementation, which records resource
operations rather than performing real firmware acceptance. Its method name
`attest` must not be read as proof of cryptographic device attestation. [P4][P5]

## 2. The two flows are on opposite sides of the trust boundary

### The PR's OpenHCL/VPCI flow

```text
VTL0 guest: PCI configuration / isolation query
  -> OpenHCL VPCI relay
  -> VPCI client attestation and resource-acceptance state
  -> protobuf TDISP commands over VPCI to the external host
  -> host TdispHostDeviceTarget
  -> host state machine and device backend

OpenHCL also calls its own trusted-platform acceptance hooks.
```

Here `VpciClientTdispState` is the consumer that requests host actions.
Its implementation has concrete VPCI worker, slot, isolation, VTOM and VTL
fields; its constructor is `pub(super)`, not a generic public engine
constructor. [P6]

The actual orchestration is:

1. Negotiate, obtain the device ID, call the pre-bind hook, and request Bind.
2. Require Locked, call the pre-start hook, and request Start.
3. Require Run, call the post-start hook, then retrieve/cache the interface
   report used by the common classification logic.
4. On MMIO enable, program/flush the BARs and accept their ranges.
5. For a Private BAR, ask the host to Unblock first, then invoke the local
   platform MMIO-unblock hook. Record success and unblock DMA as required.

Run alone does not make BARs accessible. The host's emulated NVMe tests check
that distinction. The relay serializes configuration writes around long
operations, while configuration reads continue. [P7][P8][P9][P10]

### Our native Linux-host CCA flow

```text
Linux Realm guest: TSM sysfs and guest CCA/RSI code
  -> RHI host request exits to virt_kvm
  -> EvidenceService admission and blocking worker
  -> tdisp::host::Coordinator
  -> EvidenceBackend<RealmDevice>
  -> IOMMU_VDEVICE_TSM_REQ / Linux TSM and RMM

KVM TIO exits and guest RAM conversion
  -> the same per-device service and coordinator
  -> protected-MMIO validation or one complete RAM/DMA transaction
```

The resolver constructs one owner, attaches its PCI access gate, retains
the strong service reference, and registers a weak reference with KVM.
RHI state changes are explicit guest requests; OpenVMM does not substitute
an automatic Bind-then-Start acceptance cycle. [L1][L2]

The coordinator owns state and bounded evidence snapshots. The backend checks
the ioctl result, residue and TSM status, and retains the physical assignment.
MMIO validation also checks the requested host address against fixed BAR
translation. Shared/private RAM changes run under the same admission as
state changes, not a separate uncoordinated DMA path. [L3][L4][L5][L11]

**The Linux guest/RMM acceptance steps must remain where they are.** Moving
OpenHCL's consumer-side acceptance routine into the untrusted host is not an
equivalent implementation of that trust boundary.

## 3. What should be common, and what should remain CCA-specific

| Area | Recommended common part | CCA-specific part retained |
|---|---|---|
| Lifecycle | One host transition implementation, successful-completion state updates, health/quarantine and history | RHI state decoding and checked Linux TSM state requests |
| Host operations | Bind/LOCK, Start/RUN, Unbind/UNLOCK, evidence and resource-operation interfaces | CCA object kinds, regeneration requests, and the pinned kernel ABI |
| Evidence | Common healthy-state checks and operation serialization; retain bounded snapshot utilities | Native size/read/regenerate/VCA and nonce handling; preserve VPCI's whole-report callback instead of fabricating size/read calls |
| Reports | Report structures, parser/serializer, layout tests where the formats match | Verified interpretation of the CCA report's addresses and IDs; return original evidence bytes unchanged |
| Resource access | Common operation ordering and explicit resource-status vocabulary | Fixed BAR/HPA translation, nonsecure MSI-X separation, attempted protected-map ledger and IOAS ownership |
| Execution | One serialized operation owner per device | Blocking ioctl worker, cancellation retention and whole `RamWork` transactions |
| Tests | Shared lifecycle/backend contract tests plus existing emulated acceptance tests | RHI/TIO ABI tests, guest_memfd conversion tests and unchanged-guest FVP test |

This shares implementation, not necessarily the same runtime instance across
two transports. Each device needs one authoritative owner. The existing VPCI
emulated device and a native Realm-assigned device are separate consumers of
the same core. Exposing both frontends to one physical assignment would need
an additional authorization/session design; it is not proposed here.

### Operation correspondence is not numeric or one-to-one

| PR operation | Native CCA counterpart | Required care |
|---|---|---|
| Bind | RHI LOCK -> TSM SetState Locked | Preserve the explicit guest-controlled Locked interval. |
| Start | RHI RUN -> TSM SetState Run | Do not auto-start in response to LOCK or a PCI enable write. |
| Unbind | RHI UNLOCK and later assignment cleanup | Only report completion after the relevant backend operation succeeds. UNLOCK is not object destruction. |
| GetReport | Native evidence objects | Existing GetReport does not express all size/slice/regenerate/VCA operations. |
| ModifyMmioRange Unblock | Part of protected-resource preparation | CCA TIO also supplies an HPA to verify, and KVM/RMM independently validate on re-entry. |
| ModifyMmioRange Block | Conceptually withdraw access | A notification is not proof of CCA protected-map invalidation. Do not delete retained mappings on that basis. |

RHI uses 0/1/2 for Unlocked/Locked/Run. The protobuf enum uses 1/2/3,
with 0 meaning Uninitialized. Translate explicitly. A PCI requester ID, a
report range ID and a firmware device ID are also distinct identities. [P11][L6]

The PR's Bind/Start messages carry no nonce, and GetReport takes a report
type rather than a regeneration challenge. Our nonce-bearing measurement
regeneration is a separate operation, not a reason to invent a host-driven
CCA lock/start nonce exchange. Preserve the current Linux/RMM ownership of
the underlying protocol. [P12][L6][L7]

## 4. Why directly wrapping our backend is insufficient

Putting `EvidenceBackend<RealmDevice>` behind `TdispHostDeviceInterface`
would reuse callback names, but leave important behavior wrong.

### Host state and protocol policy are currently mixed

At the PR head, `TdispHostStateMachine` still:

- Requires protocol negotiation, including before unbind.
- Calls backend unbind on invalid Bind/Start requests.
- Sets its software state to Unlocked before backend unbind succeeds.
- Has no separate quarantined/unknown-outcome condition.

These are actual implementation behaviors, not just comments. Existing tests
expect invalid Bind/Start to unbind. The newly added MMIO command differs:
it rejects an invalid state without unbinding. [P13][P14][P15]

Our coordinator instead rejects invalid transitions without backend effects,
and records quarantine before a mutation. A failed mutation retains that
condition. Neither a reset nor a state query clears it. [L3][L8]

The common core should adopt completion-aware state and health semantics.
VPCI negotiation and its deliberate invalid-command cleanup policy belong
in the protocol adapter. Preserve successful flows and intentional protocol
policy, but **do not promise zero behavior change for failed physical
cleanup**: early-Unlocked reporting must be addressed explicitly. Error
responses must not serve as proof that the device is Unlocked, and quarantine
must not be fabricated as a healthy wire state. Section 6.3 specifies the
existing indeterminate-state encoding and its required compatibility tests.

### The supplied emulator is not an injectable dispatcher

`TdispHostDeviceTarget` is a useful existing frontend boundary. However,
`TdispHostDeviceTargetEmulator` hardcodes `TdispHostStateMachine` and reads
its private state to form responses. Merely implementing
`TdispGuestRequestInterface` on our coordinator does not replace that owner.
The request dispatcher and response formatting need to be separated from
the concrete state machine. [P1][P16]

### Async execution cannot be hidden behind a synchronous callback

The host target/backend callbacks are synchronous. Our native service retains
its owner, admission permit and evidence sink until a blocking ioctl finishes,
even if its caller is cancelled. [P1][L9]

Share a synchronous, typed state/operation core, then preserve appropriate
execution wrappers: direct calls for the existing short emulated callbacks,
and our admitted blocking worker for native Linux operations. Do not block on
an async service while holding the VPCI server's synchronous device lock.
Actually exposing a slow physical backend through VPCI would require a
separate deferred host-dispatch design; the relay's guest-side write queue
does not automatically provide that. [P3][P10]

### Resource classification is not a physical mapping ledger

The PR's isolation snapshot classifies BARs from the cached report and
interception flags. A Ready snapshot does not prove resource acceptance;
DMA is classified Private when any BAR is Private, independently of the
DMA-unblock flag. Tests explicitly exercise report-only readiness. [P17]

The client records a BAR only after host and platform unblock succeed. That
does not cover our need to retain uncertain or partially installed mappings.
Nor does matching a report's range ID prove that a supplied GPA/HPA interval
is valid. Keep the native access gate and mapping ledger; share checked
representations or helpers only where their semantics match. [P8][L4][L10]

The PR's cleanup path can panic on failed block/unbind or trusted-state
disagreement. Do not transplant that error policy into guest-triggered
native-host operations. This is an integration constraint, not a separate
security audit of the PR. [P18]

## 5. Background shared-flow sketch

The names in this diagram describe proposed boundaries, not existing new APIs:

```text
Existing OpenHCL consumer                     Unchanged Linux Realm guest
  | protobuf/VPCI                              | RHI requests
  v                                            v
VPCI host protocol adapter                 virt_kvm native adapter
  | negotiate, decode, compatibility           | decode, validate buffers
  | policy, encode responses                   | retain guest operation
  v                                            v
TdispHostStateMachine facade               EvidenceService worker
  |                                        Coordinator<B> facade
  |                                            |
  +------ same common tdisp host core ----------+
          confirmed state + health
          transition legality and commit
          mutation failure/quarantine
          bounded transition history
                  |
          backend implementation
          /                    \
Legacy/emulated callbacks       RealmDevice + access gate
and PR MMIO gate                Linux TSM/IOMMUFD
                               ^
                               |
                      KVM TIO and complete RAM work
                      enter through the same worker/core
```

The CCA execution sequence remains:

1. Prepare and attach the Realm device. Create the access gate and common
   owner; retain the service and register its weak KVM route.
2. Complete initial private-memory preparation before advertising full native
   assignment support.
3. Translate the guest's LOCK request to the common transition. The native
   backend closes conflicting access, calls TSM, checks completion, and only
   then confirms Locked.
4. Serve the guest's evidence generation and size/read requests through the
   same owner. Keep its explicit intermediate state and original evidence.
5. Service KVM TIO validation through the same owner, check fixed BAR/HPA
   translation, and retain attempted mappings. A successful host return only
   permits the independent KVM/RMM validation step.
6. Translate the guest's RUN request to the common transition. Keep subsequent
   RAM/DMA changes serialized with device operations.
7. At UNLOCK, require the backend's access-release/completion contract. On
   failure retain the error/ownership state; do not report a successful unbind.

The PR's consumer path continues to drive its own OpenHCL acceptance sequence
above its VPCI host target. There is no need to add VMBus, a VPCI device, an
OpenHCL image or a synthetic protocol negotiation to the existing Linux guest.

## 6. Superseded draft design

Use the main plan's section 4 for implementation decisions and later revisions.

### 6.1 Extract one engine, keep two thin facades

Add `vm/devices/tdisp/src/host/lifecycle.rs` with a shared `Lifecycle`
engine. These are proposed names. Move the existing native confirmed-state,
health and transition types there and re-export them from `tdisp::host` to
avoid a broad rename in KVM/VFIO. The engine owns the authoritative state,
mutation outcome and bounded transition history. It knows neither protobuf
nor Linux handles, guest addresses, VTLs or report formats.

Move **both** transition tables and mutation commit/error handling into this
engine. A facade may choose a protocol operation and perform wire validation;
it may not decide independently that hardware has entered a new state.
Operations execute synchronously under exclusive ownership. Use a scoped
transaction that poisons state before calling a backend and commits the
target state only after success. Error or unwind leaves quarantine.
Do not expose a public unchecked state setter or a backend accessor.

Keep `host::Coordinator<B>` as the native backend/evidence facade. Replace its
`state`, `last_transition`, `confirmed` state updates and `mutate` machinery
with the shared engine. Keep its snapshot cache, VM-wide budget and native
request types. A rejected operation must not call the backend or invalidate
snapshots; an admitted mutation invalidates them before backend work. Preserve
the current post-read access-health check and original error source. [L3]

Move the legacy protocol implementation to `tdisp/src/vpci.rs`, retaining
root re-exports and the existing public host-target names.
`TdispHostStateMachine` becomes a compatibility facade containing the same
`Lifecycle`, its callback handle and negotiation/reason metadata. Remove its
independent `current_state`, `is_valid_state_transition`, and
`transition_state_to` implementations. Its `state()` becomes a projection
of the shared engine. `TdispHostDeviceTargetEmulator` continues to dispatch
commands and construct responses, not to own another state machine. [C1]

This is not just a shared enum or legality helper: failure handling, state
commit, quarantine admission, terminal teardown and transition recording must
also execute in the shared engine. Keep bounded protocol reason history as
diagnostic metadata, not another source of lifecycle truth.

### 6.2 Make the operation distinctions explicit

The engine needs distinct operations rather than a single permissive
`set_state(Unlocked)`:

| Operation | Allowed healthy state | Successful result |
|---|---|---|
| Native `SetState(Locked)` / VPCI Bind | Unlocked | Locked |
| Native `SetState(Running)` / VPCI Start | Locked | Running |
| Native `SetState(Unlocked)` | Locked or Running | Unlocked |
| VPCI Unbind, including compatibility cleanup | Unlocked, Locked or Running | Unlocked; backend called even when already Unlocked |
| Native evidence regeneration | Locked or Running | Same confirmed state; snapshots invalidated |
| VPCI MMIO notification | Locked or Running | Same confirmed state |
| Native assignment/RAM transaction | Confirmed state, with existing backend preconditions | Same confirmed state |
| Native reset | Confirmed state, with existing backend support | Unlocked only after success |
| Owner teardown | Confirmed or quarantined | TornDown only after acknowledged cleanup |

Repeated native state requests remain invalid. VPCI's explicit idempotent
Unbind is needed by existing tests and must not be implemented as native
assignment destruction. Normal Unbind retains the device for a later Bind;
teardown is terminal. Preserve the emulator's currently no-op `reset()` API
rather than silently turning it into recovery. [C4][L3]

Keep negotiation outside this table: RHI does not negotiate a VPCI protocol.
Capabilities and local input validation must reject unsupported requests
before mutation admission. Once backend mutation begins, any error is
potentially committed; an errno is not proof of no effect.

### 6.3 Select the wire error contract now

The protobuf already defines `Uninitialized = 0` as **not initialized or
indeterminate**. Use that existing value for an unconfirmed host state; no
new protocol enum or CCA protocol number is needed. [C2]

| Situation | VPCI result and state fields | Native result |
|---|---|---|
| Successful transition | Success; before/after are confirmed states | Existing RHI success |
| Wire validation or unsupported request rejected before mutation | Existing error; healthy state unchanged | Existing input/unsupported mapping; no backend work |
| Invalid VPCI Bind/Start or state-gated report while healthy | Run explicit Unbind, then existing invalid-state/report error; after=Unlocked only if cleanup succeeded | Invalid native transition stays side-effect-free |
| Wrong-state/invalid VPCI MMIO request | Existing validation error; no implicit Unbind | Native address/operation checks remain separate |
| Backend mutation or compatibility cleanup fails | HostFailedToProcessCommand; after=Uninitialized; quarantine retained | Existing device error/fatal-guest handling |
| Non-mutating report read fails | HostFailedToProcessCommand; preserve healthy state unless containment failed | Preserve snapshot/access error behavior |
| Another command arrives while quarantined | Error and before/after=Uninitialized; no guest-triggered recovery | Existing closed/quarantined behavior |

For a failure that begins from a confirmed state, `tdi_state_before` may
report that confirmed state. Never report last-confirmed state as the
current state after an uncertain operation. Do not encode TornDown as a
successfully Unlocked device either.

Pin existing validation order in wire tests: GuestDeviceId is allowed while
Unlocked; report enum validation versus state checks affects implicit cleanup;
a recognized `Unknown` unbind reason still invokes cleanup, whereas an
unrecognized numeric enum is rejected by the dispatcher. Preserve response
body shapes where they are compatible with the explicit failure contract.
Cleanup failure takes precedence over the original invalid-command error.

`VpciClientTdispState::send_tdisp_command` updates its cached state **before**
checking the response error. It already recognizes Uninitialized. Test that
the new error response produces that cache value and an error, never an
accepted device. Its existing fatal cleanup policy remains explicit; do not
hide failure to make the client continue. This is an intentional change to
failed-mutation behavior, not a claim of byte-identical error responses.
Require host/client fault-injection coverage and review before landing the
VPCI migration. No broad client orchestration rewrite is proposed. [C3][P18]

### 6.4 Quarantine must also revoke access

A poisoned state variable is not a BAR or DMA access gate. Preserve native
`AccessGate` denial, checked IRQ failures, mapping ownership and retention.
Do not replace these with the PR's report-classification snapshot.

For emulated host backends, add an explicit local quarantine/revocation
contract to the callback adapter. The NVMe emulator must deny access through
its shared `TdispMmioRanges` gate after an uncertain mutation; today that gate
only tracks explicitly blocked/unblocked ranges. Test a callback that changes
the gate and then fails. Revoke access on error/unwind without invoking an
implicit physical Unbind or doing blocking cleanup from Drop. Mocks with no
resources must implement that contract explicitly, not inherit a silent
success default. Audit callback aliases so they cannot issue lifecycle changes
outside their owner. [C5][C7][L10]

If local revocation cannot prove release of physical mappings, remain
quarantined and retain ownership. This refactor does not make the currently
missing CCA protected-map release acknowledgement available.

### 6.5 Preserve evidence and execution boundaries

Do **not** implement native `object_size` by fetching a legacy report and
then fetch it again for `read_object`. The native API deliberately snapshots
one whole object under a VM-wide budget; the legacy callback already returns
a `Vec<u8>`. A naive adapter would change coherence, allocation and error
semantics. Keep native snapshots and VPCI whole-report retrieval as separate
facade operations, both under the same shared health/transaction rules.
No new legacy caching or size/offset protocol is required. Metadata reports
such as GuestDeviceId/IsRegistered are not CCA VCA evidence. [C6][L3]

Keep `EvidenceService` as the native execution wrapper, without renaming it
in the first refactor. Its mutex continues to own the entire coordinator,
including evidence storage. One admitted worker spans the complete
`RamWork::run`: DMA withdrawal, KVM memory work, prefault/mapping and failure
handling. Keep device-before-memory lock order, weak KVM registrations,
strong assignment ownership, and no re-entry into the service from admitted
work. Cancellation retains the worker/owner/sink; ambiguous completion still
prevents guest continuation. Teardown closes admission before draining it.
The synchronous VPCI facade uses exclusive access and its existing callback
mutex; it must not block on this async service. [L5][L9]

## 7. Superseded draft sequencing

The active sequence and acceptance gates are in the main plan's section 4.

Implement as new reviewable changes above the measured baseline. Do not
rewrite the now-repaired rebase stack again.

| Step | Files and change | Exit gate |
|---|---|---|
| 1. Contract tests | `tdisp/src/tests/{statemachine,endtoend,serialize}_tests.rs`, `host/tests.rs`, and VPCI client fault-injection tests | Pin successful responses, validation order, strict native transitions and legacy idempotent Unbind. Add controllable before/after-effect failures. New failed-mutation expectations land with their implementation, not as a red intermediate commit. |
| 2. Extract the engine and migrate native coordination | Add `tdisp/src/host/lifecycle.rs`; update `host.rs` with re-exports and delegation; retain `host/evidence.rs` admission | Native state, snapshot, failure, cancellation and RAM-transaction tests pass unchanged. No new platform dependencies. |
| 3. Migrate VPCI host coordination | Move protocol facade/dispatch to `tdisp/src/vpci.rs`, re-export public names, replace legacy state machinery, wire existing Uninitialized/error fields, add local revocation contract | Both facades use the same engine; state and callback-count matrix passes, including post-effect failures. No second authoritative state field remains. |
| 4. Adapt emulator consumers and verify the actual client | `tdisp/src/test_helpers.rs`, `tdisp/src/tests/mocks.rs`, `nvme_test/src/tdisp.rs`, `vpci_relay` mocks/tests, and focused `vpci_client` tests | BAR access closes after fault/Unbind; healthy rebind still works; client rejects uncertain replies; serialization and deferred configuration ordering remain intact. Keep necessary caller/gate edits in step 3 if required for a compiling, fail-closed commit. |
| 5. Native integration and removal audit | Audit VFIO resolver/backend, KVM RHI/TIO/memory and worker ownership; change callsites only if required | All retain the existing coordinator/service boundary and one native owner. Remove any now-dead transition implementation, not safety checks. |
| 6. Runtime qualification | Existing Flowey commands and preserved inputs, no guest or firmware changes | QEMU/v15 CCA boot smoke remains good; full FVP TDISP rerun preserves LOCK/RUN and read/hash and reports lifecycle failures separately. |

The native backend does not need a new `TdispHostDeviceInterface`
implementation merely to use the engine. The first native migration should
leave RHI, TSM request encoding and VFIO object ownership untouched. Changes
to those areas need a specific integration reason, not mechanical API churn.

The existing report parser/serializer and resource types remain shared
library utilities. Generalizing report classification across CCA and VPCI is
not on the critical path; it needs real format/address fixtures first.
Do not change `openhcl_tdisp::TdispResourceValidationInterface` into a host
backend, add firmware stubs, or treat no-op validation as attestation.

### Test and acceptance matrix

The exact test selectors can be refined during implementation; use existing
package runners, not new tools:

```bash
cargo nextest run --profile agent \
  -p tdisp -p nvme_test -p vpci -p vpci_client -p vpci_relay -p openhcl_tdisp
cargo nextest run --profile agent \
  -p vfio_sys -p vfio_assigned_device -p virt_kvm -p openvmm_core -p petri
```

Run the Arm KVM unit tests through the existing user-mode runner as well.
Check/clippy the affected native and Arm consumers, run rustdoc and finish
with the full formatter before each implementation commit.

Required assertions include:

- Both facades call the same transition/commit/quarantine implementation;
  allowed matching transitions invoke a backend exactly once.
- Unsupported/preflight rejection has no backend effects. Native duplicate
  state requests remain invalid; healthy VPCI Unbind from Unlocked still
  invokes its callback and permits a later Bind.
- Failures before and after a backend effect never invent success. Test
  Uninitialized state encoding and the real client's cache/error ordering.
- Quarantine denies operations and access; reset/query/re-negotiation cannot
  clear it. Owner cleanup failure retains custody; acknowledged teardown is
  terminal. Do not turn guest Unbind into an undocumented recovery path.
- Native snapshot bounds, accounting and byte identity remain unchanged.
  Legacy reports keep their existing fresh-read behavior and metadata rules.
- Queued/running cancellation retains resources. A full RAM transaction
  cannot interleave with another request or teardown, including its error
  path. IRQ/BAR failures still propagate to native containment.

### Final FVP command and pass criteria

Use the same restored DA tuple as the rebased baseline, not the separate
v15 FVP package. Run this after other nextest commands have exited: Flowey's
nextest installation can otherwise fail with `ETXTBSY`.

```bash
PYTHONDONTWRITEBYTECODE=1 INCUBATOR_TIMEOUT=1800 \
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca-realm-vfio.toml \
  --fvp-platform-root "$PWD/.packages/cca-tdisp-runtime" \
  --shrinkwrap-package-root "$PWD/.packages/cca-tdisp-runtime/package" \
  --cca-in-place-payload-root "$PWD/.packages/cca-tdisp-runtime/host-payload" \
  --cca-tdisp-guest-root "$PWD/.packages/cca-tdisp-runtime/guest-payload" \
  --dir "$PWD/vmm_test_results/cca-tdisp-shared-core" \
  --fvp-single-test \
  aarch64_exclusive::tdisp_ahci::openvmm_linux_aarch64_boot_linux_direct_cca_tdisp_ahci
```

Require fresh host-confirmed Locked/Run transitions, ordered guest markers,
and the full 64 MiB hash
`281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6`.
Record interrupts, native JUnit, UNLOCK, fixture cleanup, launcher exit and
preservation separately. Preserve the source identity and artifact hashes.
Do not weaken the test to make the known lifecycle failure pass.

The implementation acceptance gate is preservation of the measured protocol/I/O
milestone plus the shared-core contract tests. A clean full VMM test remains
a separate, unmet lifecycle gate. New failures before the I/O marker are
regressions; changed post-I/O failures need investigation, not automatic
classification as the old guard.

### Non-goals and rollback

No CCA UNLOCK/kernel-reclamation redesign, deferred shutdown hold, same-host
reuse claim, private-buffer DMA measurement, physical VPCI host backend,
OpenHCL CCA guest support or new transport is included. Do not expand the
refactor to resolve the deferred in-place QEMU diagnostic.

Rollback means return to the recorded source/input tuple and start a fresh
test instance. It is not live rollback of an uncertain physical assignment.
Keep the older baseline evidence rather than overwriting its result directory.

## Review

The original pre-rebase review reached **Minor revisions**, incorporated at
that time. The post-rebase review is recorded under `## Review` in the main
plan; this findings document is not the implementation authority.

## Source references

Upstream references are pinned to the PR head, except P0 (the PR base).
Local references use repository paths and line ranges at change ID `tkorwyzw`.

| Current ref | Evidence rechecked after the rebase |
|---|---|
| [C1] | Legacy authoritative state, transition table and early-Unlocked unbind |
| [C2] | Existing indeterminate wire state and operation errors |
| [C3] | Client adopts response state before testing result status |
| [C4] | Idempotent Unbind and invalid-command cleanup tests |
| [C5] | Emulated host callbacks and shared MMIO gate |
| [C6] | Legacy whole-report callback versus native evidence contract |
| [C7] | NVMe BAR access actually consults the shared gate |

| Ref | Evidence |
|---|---|
| [P0] | Pre-existing host interfaces at the PR base |
| [P1] | Host/backend interfaces, MMIO hook, host/relay targets and concrete emulator |
| [P2] | Relay implements the relay target |
| [P3] | VPCI host command dispatch and synchronous device lock |
| [P4] | Client isolation/protocol selection rejects CCA |
| [P5] | Resource-validator interface and no-op factory |
| [P6] | Concrete VPCI client state and constructor |
| [P7] | Consumer bind/start/report ordering |
| [P8] | Host/platform unblock ordering and success ledger |
| [P9] | NVMe Run-versus-MMIO-access tests |
| [P10] | Relay deferred configuration writes |
| [P11] | Protobuf state values |
| [P12] | Bind/Start/report request schema |
| [P13] | Early software-Unlocked transition |
| [P14] | Negotiation and invalid-transition policies |
| [P15] | Tests for invalid Bind/Start cleanup |
| [P16] | State owner, private state query and negotiation state |
| [P17] | Resource classification and isolation snapshot |
| [P18] | Client cleanup and panic policy |
| [L1] | Resolver creates and retains the native service |
| [L2] | Native RHI and TIO service calls |
| [L3] | Native coordinator transitions, quarantine and snapshots |
| [L4] | Native protected-MMIO validation and attempted-map retention |
| [L5] | Serialized KVM RAM preparation/conversion |
| [L6] | Native state and measurement-input decoding |
| [L7] | Typed Linux CCA object/state/request ABI |
| [L8] | Native invalid-transition and mutation-error tests |
| [L9] | Native service admission and cancellation-safe worker |
| [L10] | Native access gate and mapping ownership |
| [L11] | Native state request and checked completion |

[microsoft/openvmm#4416]: https://github.com/microsoft/openvmm/pull/4416
[P0]: https://github.com/microsoft/openvmm/blob/711723c4cef8601cb94838f38a230daa05fd4cc1/vm/devices/tdisp/src/lib.rs#L74-L120
[P1]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/tdisp/src/lib.rs#L80-L222
[P2]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/pci/vpci_relay/src/lib.rs#L673-L719
[P3]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/pci/vpci/src/device.rs#L1083-L1143
[P4]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/pci/vpci_client/src/tdisp.rs#L727-L745
[P5]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/openhcl/openhcl_tdisp/src/lib.rs#L128-L299
[P6]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/pci/vpci_client/src/tdisp.rs#L39-L175
[P7]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/pci/vpci_client/src/tdisp.rs#L862-L976
[P8]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/pci/vpci_client/src/tdisp.rs#L1187-L1230
[P9]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/storage/nvme_test/src/tests/tdisp_tests.rs#L131-L162
[P10]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/pci/vpci_relay/src/lib.rs#L723-L768
[P11]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/tdisp_proto/src/tdisp.proto#L12-L35
[P12]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/tdisp_proto/src/tdisp.proto#L207-L225
[P13]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/tdisp/src/lib.rs#L533-L565
[P14]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/tdisp/src/lib.rs#L650-L874
[P15]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/tdisp/src/tests/endtoend_tests.rs#L307-L348
[P16]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/tdisp/src/lib.rs#L415-L464
[P17]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/pci/vpci_client/src/tdisp.rs#L1006-L1075
[P18]: https://github.com/microsoft/openvmm/blob/f626c6d7e9798803d411daa7cc0dfbb543a7caf6/vm/devices/pci/vpci_client/src/tdisp.rs#L609-L714
[L1]: vm/devices/pci/vfio_assigned_device/src/resolver.rs#L164-L231
[L2]: vmm_core/virt_kvm/src/rhi.rs#L346-L490
[L3]: vm/devices/tdisp/src/host.rs#L431-L665
[L4]: vm/devices/pci/vfio_assigned_device/src/realm/tdisp.rs#L226-L270
[L5]: vmm_core/virt_kvm/src/memory.rs#L428-L505
[L6]: vmm_core/virt_kvm/src/rhi.rs#L75-L123
[L7]: vm/devices/user_driver/vfio_sys/src/iommufd/tsm.rs#L75-L220
[L8]: vm/devices/tdisp/src/host/tests.rs#L350-L468
[L9]: vm/devices/tdisp/src/host/evidence.rs#L59-L211
[L10]: vm/devices/pci/vfio_assigned_device/src/realm/access.rs#L13-L78
[L11]: vm/devices/pci/vfio_assigned_device/src/realm/tdisp.rs#L559-L694
[C1]: vm/devices/tdisp/src/lib.rs#L420-L572
[C2]: vm/devices/tdisp_proto/src/tdisp.proto#L12-L74
[C3]: vm/devices/pci/vpci_client/src/tdisp.rs#L256-L319
[C4]: vm/devices/tdisp/src/tests/statemachine_tests.rs#L51-L160
[C5]: vm/devices/storage/nvme_test/src/tdisp.rs#L37-L194
[C6]: vm/devices/tdisp/src/lib.rs#L80-L120
[C7]: vm/devices/storage/nvme_test/src/pci.rs#L548-L590
