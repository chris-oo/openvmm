# Linux CCA 7.4 FVP migration report

## Follow-up: OpenVMM-owned guest test fixture

The user requested new commits in both repositories. Test policy belongs
in OpenVMM, not in the binary dependency producer. No previous commit will
be amended or pushed.

The follow-up keeps the full payload's `guest/Image` and `guest/initrd`
layout, but makes its guest initrd the same pinned generic binary base as
the host initrd. Remove the custom guest-initrd producer and its script,
oracle and recipe pins from `openvmm-deps`; retain binary provenance and
the existing host-only/full package roles.

OpenVMM owns the AHCI guest script and injects it plus the existing expected
disk hash with `PetriInitrd::prepare`. Keep the prepared initrd alive through
VM teardown. Preserve the existing LOCK/accept/probe/direct-read/hash/
unbind/UNLOCK order, checked host/RMM teardown and immutable binary hashes.
Record the composed initrd identity separately from its pinned base.

Capture a pre-teardown kernel-log baseline and complete snapshots after
unbind and UNLOCK, before poweroff. Mark each diagnostic section so the
host test can reject newly reported teardown errors or warnings without
rejecting unrelated earlier boot diagnostics. Require successful commands,
an absent bound driver, complete ordered diagnostic sections and guest exit
zero. Do not mask failures or weaken the existing DMA/hash checks.

Use focused synthetic console cases registered with Petri's native
`test_sync!` mechanism; this test binary has no standard Rust test harness.
Run them through `cargo xflowey vmm-tests-run`. Validate initrd injection
with the existing Petri tests, rebuild the changed binary packages and run
the real full TDISP/AHCI VMM test on the unchanged firmware/kernel. Run all
modified-package check, clippy and rustdoc gates, then full fixing and
non-fixing formatter checks. Commit the dependency change and matching
OpenVMM change only after validation; include report results in the
OpenVMM commit and save its own receipt externally.

After both commits, investigate the DOE diagnostic in the exact pinned
RMM source. Start with the TDISP-only cache filter accepting all vendor
responses and distinguish legitimate IDE-KM protocol zero from malformed
headers. Do not change firmware or launch recovery fault injection.

### Follow-up review

**Minor revisions**, incorporated: compose from a hash-verified private
copy of the pinned base; retain the prepared owner through teardown;
compare complete ordered raw-priority kernel-log records with prefix
continuity and fail on loss or newly repeated diagnostics. Require unique,
ordered diagnostic sections and one zero guest exit. Synthetic cases must
cover old warnings, new/repeated warnings and errors in both windows,
missing/reordered/duplicate sections, lost baseline, bad exit and DMA/hash
failure. Replace the deleted producer's contract tests with strict generic
base/role/substitution tests. Update current byte-identical-script claims
and check the relevant VMM Guide pages.

### Follow-up implementation and checks

The fixture now lives beside `tdisp_ahci.rs` in OpenVMM. It records full
raw-priority logs in the three ordered phases and verifies driver removal.
The host checker preserves old boot warnings but rejects newly appended
warning/error records, including identical repetitions, missing sections,
lost baseline continuity, DMA/hash failure and missing/nonzero guest exit.
The pinned BusyBox binary was checked through the registered ARM runner;
its `dmesg -r` option preserves the required raw priorities.

All three native Petri diagnostic cases were discovered and passed through
Flowey. The existing initrd preparation test passed, followed by all 291
Petri/Flowey unit tests. Both modified packages passed native and ARM
check, clippy and rustdoc; fixing and non-fixing full formatter checks
passed. The first real composed-guest run completed I/O, UNLOCK and clean
native teardown, but its test failed because Petri's serial logger consumed
the raw syslog priority prefixes before the checker read `linux.log`.
The guest now escapes each raw record with `K:`; synthetic tests include
the actual serial-log normalization step. No severity or continuity check
was relaxed. Structured logs from the failed run confirm one new INFO
record after unbind and no new records after UNLOCK.

The corrected real FVP run passed: **1 test in 1680.202 seconds**,
run ID `1c069f597cf66a88c4cd2ef97f3c2c9fe2662081062e254f19a0153d3a34ff73`,
under `target/tdisp-fixture-owned-initrd-serial`. LOCK/RUN, the direct
64 MiB expected hash, driver unbind, UNLOCK and all checked cleanup passed.
The preserved logs contained 499 baseline records, one new notice after
unbind and no new records after UNLOCK. No new teardown warning or error
was accepted. All guest, fixture, launcher and finalization outcomes were
zero; both journals and owned containers were absent.

The composed initrd hash was
`099f1fa6ff19125e40e18f177434eaf156eaf1379ea2c9440c4692fb4b81059b`,
with OpenVMM script hash
`3050802e072609b67b45ce02af8e6441f4778754ecd7f6740521faf11e0e4e2b`.
The code inventory matched the frozen, model-tested source. This closes
the late-log collection/checking gap described in the older sections below;
it does not establish general failed-operation recovery.

The new full binary-base payload is
`0.3.0-fvp-base-tdisp-cb45796-local`, archive SHA-256
`4d52713cf6dd715d8b67a64fb9432083c6fbdba64d9ec9a4e49d2cd74858ce86`.
Two package builds produced identical archives. Its host and guest initrds
both have the generic `74ecad46...` identity; Image and configuration pins
are unchanged. The host-only package remains byte-identical (`084474d8...`).
The dependency producer passed 26 remaining tests, provenance checks and
binary source/license inventories. No test script or oracle remains in its
guest initrd or producer recipe.

The dependency correction is new change ID `qwzwplrl`, above `rylvukpl`.
The matching OpenVMM change includes this report; its own finalized ID and
clean-tree receipt are stored externally. These are new commits, not
amendments, and nothing was pushed.

## Current direction: one supported CCA ABI

The 2026-11-09 decision is to remove the old CCA/TDISP kernel ABI path,
not maintain it behind a compatibility switch. This supersedes the
dual-path commit split and old-ABI acceptance gates below. Those sections
are historical planning evidence and must not be executed unchanged.
The cleanup is finalized in Root change ID `mzuzyppt`, directly above
`xkuwvnxw`, and dependency change ID `rylvukpl`, directly above `ksmtlzss`.
The validated drafts `tuvpopsk` and `vxklnskt`, and temporary checkpoints
`wyzkwmoz` and `ltyxrwtv`, remain historical source snapshots rather than
part of the final selected migration series.
The operation log now records a concurrent move and origin push of
`fvp-kvm-cca-tdisp` to `wyzkwmoz`, outside this workflow's commands.
The user subsequently authorized replacing this temporary published
checkpoint: "you can overwrite published history for this temp commit,
it was only pushed for temp purposes". This permission applies to the
temporary checkpoint, not older changes. No remote push is authorized.

The Root cleanup removes the version marker, old wire API, legacy completion
adapter, old RAM completion probe, Realm parent HWPT, userspace HOSTCONF
handler and parallel CCA profiles. Current KVM capability and TIO values are
canonical. The retained-VM device-memory factory, split MSI-X/PBA mappings,
real BME/provider ordering and failure-retention guards remain.

The cleanup passed native and AArch64 check, clippy and rustdoc for all
16 affected packages. The complete selected native unit run passed all
558 tests; the ARM RAM/provider run passed all 48 selected tests.
Shared-consumer checks passed for Windows MSVC and ARM macOS. The full
formatter ran last and completed rustfmt, lints, fuzzer verification and
Flowey verification. Both independent Root code reviews are complete.
New-only normal-path validation is complete. Earlier measured passes in
the historical sections used the checkpointed dual-path source; the fresh
results below validate this cleanup. Stronger late-error detection and
general error-recovery qualification remain separate.
The actual final R1 prefix, without this report, then passed all native
and ARM check/clippy/rustdoc gates for its 12 modified packages, all
558 selected native and 81 ARM unit tests, both foreign shared-consumer
checks and full fixing/non-fixing formatter checks. Its complete
mode-aware inventory matched the reviewed source immediately before commit.

The formal review found a cleanup regression: the forwarded RHI-call
admission guard required a registered device, so a no-device CCA guest's
DA feature probe would halt the VM. The guard now also admits CCA based on
its isolation type, without restoring an ABI selector. The existing
empty-registry handler returns evidence-only feature bits and rejects
absent devices safely. The affected crate now has fresh check, clippy and
rustdoc passes on native and ARM targets.
The complete `virt_kvm` unit suites passed: 88 native and 81 ARM tests.
All four formatter passes ran last and passed. The real no-device CCA
smoke passed on this corrected source: 1 test in 803.958 seconds.
It consumed the current host-only payload and Image, completed with zero
command and launcher exits, finalized successfully, and left no ownership
journals or owned containers. The reviewed runtime-source digest was
unchanged. The full new-only TDISP/AHCI case also passed in 863.045 seconds:
LOCK, RUN, private DMA, the 64 MiB direct-read hash and UNLOCK all completed.
RMM traces show successful vDEVICE destruction and Realm-destroy
continuation. Command, native owner, fixture, model and finalization
completed successfully, with no retained ownership state.

The second review withdrew its initial multi-device UNLOCK finding after
checking the single-assignment invariant and the quarantine-before-close
path. Keep the VM-wide sharing check with its VM-wide protected-attempt
ledger; do not make only one of them requester-specific. Rejected kernel
approval and retry recovery remain outside the qualified scope. No
re-approval shortcut or mapping-guard relaxation was added.

The new-only ordinary ARM control passed in 335.333 seconds, with the
current host Image and host-only package. Command, launcher and finalization
all succeeded, and no ownership journal or owned container remained.
This run preceded the CCA-only admission correction above. A fresh ordinary
ARM run on the exact corrected source then passed in 322.870 seconds,
again with verified inputs and clean teardown.

The consolidated kernel recipe reproducibly produces Image SHA-256
`af5da17c7f5642865d18da0baa6b5bc36685fd7acf82b19d3064b3ce2452cb1b`.
The producer verified two independent builds. Compared with tested Image
`1b30b41684dc1a5ed98e18d102d75f6fab93ec000e3602201839995f511fee9c`,
only 40 GNU build-ID bytes differ; executable and other bytes match.
Root now pins the consolidated image. Source, configuration and guest
initrd pins remain unchanged. Canonical archive identities are:

| Role | Version | SHA-256 |
| --- | --- | --- |
| Host-only | `0.3.0-fvp-current-cb45796-local` | `084474d84afe64b42def12ee69274b279ba9d7f10643ce05e26a84fda79f8e73` |
| Full TDISP | `0.3.0-fvp-current-tdisp-cb45796-local` | `964fda7fc6736eadd32a933da229a097d1fb04641098c986609a83a7a4eed121` |

The full package was reproduced after rerunning guest construction and
packaging. Both role contracts were verified. Producer reviews found no
blocking issue; Python build debris and stale example paths were corrected.
The reviewed-context full and host-only rebuilds reproduced both archive
digests above exactly. All 33 recipe tests and the 53-component provenance
check passed again. No-device CCA smoke and full TDISP passed.
No older digest is accepted as a fallback.

The October CTLV series on `cca/topics/cca-da-attestation` is separate from
this frozen `cca/topics/7.4/tdisp-host` migration. It would need a separate
host and guest tooling migration.

## Single-ABI measured results

| Test | Result | Duration | Output directory |
| --- | --- | --- | --- |
| Current no-device CCA smoke | 1 passed | 803.958 s | `target/cca-single-abi-final-smoke` |
| Current full TDISP/AHCI | 1 passed | 863.045 s | `target/cca-single-abi-final-tdisp` |
| Native x64 ordinary, shared and file-backed memory boots | 3 passed | 3.740 s | `target/cca-single-abi-native-controls` |
| Standalone Incubator host command | passed | 31.954 s | `target/cca-single-abi-final-standalone/results` |
| Ordinary ARM, before the CCA-only guard fix | 1 passed | 335.333 s | `target/cca-single-abi-final-basic` |
| Ordinary ARM, exact post-review source | 1 passed | 322.870 s | `target/cca-single-abi-final-basic-reviewed` |

The standalone command checked `/dev/kvm` and the exact host kernel release.
Its checked command, launcher and finalization outcomes were zero. Pipette
logged a node disconnect after the command passed during shutdown; the
receipt retains this diagnostic.

These are normal-path results, not warning-free or general recovery
qualification. The current guest fixture takes its `dmesg` snapshot before
final unbind/UNLOCK. Stronger late-error detection remains separate work.
Guest RSI-protected warnings, PCI IRQ-parse messages, unsupported root-port
config writes and firmware DOE VDM diagnostics remain visible. Successful
vDEVICE state readback and destruction, Realm-destroy continuation, native
owner completion and fixture context checks provide teardown evidence for
this run; do not treat a guest echo marker alone as proof. Native x64
controls also logged unsupported MSR probes while passing.

All model runs are finished. Both ownership journals are absent, and no
FVP-owned containers remain. The runtime-source digest stayed unchanged
through final validation and R1 commit construction. D1 and R1 are committed
locally. This report is the final documentation cut; its own commit receipt
and final bookmark/clean-tree checks are recorded externally. No push was
performed.

The new implementation should be the only supported CCA path:

1. Remove `linux_cca74`, `cca-abi=linux-cca74`, and their configuration,
   resource and trait-marker plumbing. Retired CLI parameters should fail
   clearly, not be silently ignored.
2. Replace the old IOMMUFD requests and residue/completion adapters with
   the tested new API. Collapse CCA capability, TIO, filter and RAM-fault
   forks to the new semantics. Keep required-capability errors and all
   input, lease and no-progress checks.
3. Use parentless allocation for Realm vIOMMUs and keep their separate
   shared-DMA IOAS/HWPT. Do not remove generic SMMUv3 nesting or parent
   HWPT support used outside Realm assignment.
4. Keep device guest_memfd backing, real BME ordering, shared MSI-X/PBA
   interception, safe byte access, failure retention and RAM ownership.
   The retention wrapper still needs device-memory factory forwarding;
   only its ABI-marker forwarding becomes unnecessary.
5. Use the supported tuple through the ordinary FVP CCA profiles rather
   than parallel legacy/candidate variants. Artifact provenance is a
   separate choice: source-built versus imported packages can remain if
   they both supply the supported ABI. Inspect existing host-only/full
   payload contracts before deciding how to fold the parallel producer.
6. Preserve ordinary KVM, non-CCA guests, other hypervisors and shared
   memory code. In particular, do not apply CCA capability numbers
   globally. Leave `--guest-memfd-in-place` and shared backing enums
   unchanged in the first cleanup unless their other users are audited.

This removes compatibility branches and duplicate wrappers, but not the
substantial new backend work required by the kernel's device-MMIO contract.
There is no replacement runtime fallback for old CCA kernels.

The commit plan must be regenerated from the resulting diff. The temporary
`linux_cca74: false` constructor edit is no longer needed. Keep replacement
API declarations with their dependent callers; the former additive ABI-only
cut may need to merge with native consumers. Basic ARM onboarding remains
independent, and FVP integration, tests and documentation remain useful
review boundaries.

Keep old archives and receipts as immutable history, not obsolete runtime
support. Old-ABI compatibility controls are no longer an acceptance gate.
Instead, test clear rejection of unsupported CCA kernels and regressions
for ordinary x64/ARM KVM, non-CCA FVP and shared-config consumers. Re-run
the new-only final-stack smoke, full TDISP, ordinary ARM and standalone
checks. Teardown diagnostics and independent code reviews remain required.
The successful dual-path binaries below do not validate the cleanup.

Direction review: **Minor revisions**, incorporated above. The reviewer
specifically required CCA-only changes in shared memory code, preservation
of generic SMMU support and ownership guards, and inspection before producer
consolidation. This is design review, not final code or prefix validation.

## Historical dual-ABI status

The first normal end-to-end run works on Aneesh's Linux
`cca/topics/7.4/tdisp-host` branch, frozen at
`cb45796cb3a9483202dce8c24735fc135bb9b675`.

Both the packaged CCA smoke and full TDISP/AHCI VMM test passed.
The full test completed LOCK, RUN, private DMA enable, a 64 MiB direct
disk read with the expected hash, UNLOCK, and checked native, fixture,
and model teardown. No ownership journals or owned containers remained.

This is a first normal-path pass, not final promotion or a general
failure-recovery guarantee. Code review, final regression controls,
stronger guest teardown diagnostics, and review of recorded warnings remain.
The validated source is preserved in the temporary checkpoint named above.
Final review and commit splitting remain pending.

At the original preparation time, the Root working change ID was `wyzkwmoz`, directly
above `xkuwvnxw`. At that time the `fvp-kvm-cca-tdisp` bookmark named `xkuwvnxw`;
its later published checkpoint is recorded above.
The dependency working change ID is `ltyxrwtv`, above `ksmtlzss`.
These are change IDs, not Git commit hashes. Partitioning the working
changes will create new working change IDs; update the ledger below with
each finalized change ID.

## Historical dual-ABI measured VMM results

| Test | Result | Duration | Output directory |
| --- | --- | --- | --- |
| Existing ordinary ARM `boot_no_hv` on the first candidate host | 1 passed | 295.162 s | `target/cca74-candidate-basic` |
| Packaged candidate CCA in-place smoke | 1 passed | 722.969 s | `target/cca74-candidate-smoke-swiotlb` |
| Full candidate TDISP/AHCI | 1 passed | 785.781 s | `target/cca74-candidate-tdisp-msix` |

The ordinary ARM result predates the later RMM boot-mode fix. Repeat it
on the final stack; do not count it as final-stack regression evidence.
The passing smoke also predates the final MSI-X mapping correction.

The full test is:

```text
aarch64_exclusive::tdisp_ahci::openvmm_linux_aarch64_boot_linux_direct_cca_tdisp_ahci_on_fvp_linux_aarch64_vm_host_fvp_realm_vfio
```

It uses the native x64 outer runner, an AArch64 Linux host under FVP,
and an AArch64 OpenVMM/KVM Realm guest. It is a real `vmm_tests` case,
not a standalone shell probe or a unit-test substitute.

Its guest script is our pinned qualification fixture, not an author's
published November reference script. The script and initrd were unchanged
for this passing run. It performs a read, not a disk write test.

The full run ID is
`ed4f9eee3fb965c6857e3a1bb14e5f175eb665d5ee6f10b2b45839bf190c2c10`.
The recorded nested command, fixture, launcher, and overall exits were zero.
Both inner and outer passed markers were present. The native owner logged
successful teardown, and the RMM completed vDEVICE and Realm destruction.

## Tested inputs

| Input | Identity |
| --- | --- |
| Linux source | `cb45796cb3a9483202dce8c24735fc135bb9b675` |
| Host and guest Image SHA-256 | `1b30b41684dc1a5ed98e18d102d75f6fab93ec000e3602201839995f511fee9c` |
| Candidate payload archive SHA-256 | `0b0706ef788fa7a061f2811d759b2d2c3b29cdb61f0ef90707f2ed433077c0d7` |
| Firmware archive SHA-256 | `101a636410b7048ad6d789b7597ef2ddf3a5c38c4ffe073deb1a43134289ec49` |
| Guest initrd SHA-256 | `b9dc5046ac739a90cf23c1c52073cf6cdedcbabcfa18481b73faa0cdec9f5e92` |
| Expected 64 MiB disk SHA-256 | `281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6` |
| RMM source | `33bbaf7814fee335027bf4d2417d97b551838b70` |
| Model | FVP 11.32.19 |
| Payload version | `0.3.0-fvp-cca74-cb45796-local` |
| Firmware version | `0.3.0-fvp-source-local` |

Firmware, model, and pinned standard userspace were preserved. No local
Linux patch was applied. Shrinkwrap only runs the model; `openvmm-deps`
builds and packages the kernel and payload. Package metadata remains
conservative: external test results do not rewrite immutable archives
or make `runtime_qualified` true.

## Changes required

| Area | Change and reason | Main sources |
| --- | --- | --- |
| Dependency production | Added the independent, opt-in `fvp-cca74` producer. It builds one pinned Image for host and guest, records the resolved config and sources, and keeps old producers unchanged. | `openvmm-deps/pkg/fvp-cca74/`, its `Dockerfile` target and provenance registration |
| KVM/IOMMUFD ABI | Added the new 40-byte vDEVICE-operation envelope, zero-success completion checks, 64-bit object sizes, inline nonce, resource-backed guest_memfd, capability numbers, and candidate TIO responses. No ioctl fallback. | `vm/kvm/src/`, `vm/devices/user_driver/vfio_sys/src/iommufd/tsm/cca74.rs` |
| Native integration | Added device guest_memfd leases, checked private mapping approval, per-file RAM visibility conversion, parentless CCA vIOMMU allocation, separate shared-DMA HWPT handling, and explicit ABI selection. | `vmm_core/virt_kvm/src/`, `vm/devices/pci/vfio_assigned_device/src/realm/`, hypervisor configuration and resolvers |
| Host boot | The kernel enables Realm support only in `KVM_MODE_RMM`. Added candidate-only `kvm-arm.mode=rmm` and fixed the initrd overlay to carry it into `startup.nsh`. Passing an unused CLI variable was insufficient. | `petri/src/vm/fvp/staging.rs`, `petri/incubator/platforms/fvp-initrd-overlay.yaml` |
| RHI filters | Removed a broad userspace filter that overlapped the kernel-reserved HOSTCONF range and caused `EEXIST`. Reused the six synchronous DA-call filters; KVM services HOSTCONF. | `vm/kvm/src/arm.rs`, `vmm_core/virt_kvm/src/arch/aarch64/mod.rs` |
| RAM faults | The new kernel returns `EFAULT` until userspace changes guest_memfd attributes. Added an explicit candidate observer that converts immediately instead of using the old completion-only probe. Legacy probe and repeat-fault guards remain. | `vmm_core/virt_kvm/src/cca_in_place.rs`, `memory.rs` |
| Smoke transport | The kernel shrank the 256 MiB guest's SWIOTLB pool to 256 KiB, preventing the virtio-vsock queues from filling. The candidate smoke now requests `swiotlb=8192`, a 16 MiB pool. This does not change the assigned-device fixture. | `vmm_tests/vmm_tests/tests/tests/aarch64_exclusive.rs` |
| RAM-retention wrapper | Forwarded the new ABI and device-memory methods through `RetainedVm`. Their defaults had incorrectly selected legacy allocation and a parent HWPT despite candidate VM configuration. | `openvmm/openvmm_core/src/worker/dispatch/realm_retention.rs` |
| BME lifecycle | Prepared and read back real Memory Enable/BME before LOCK. On final BME clear, shared protected MMIO, retired slots and VMAs, released guest_memfd, performed the physical write/readback, and reinstated shared backing for later UNLOCK. No fake disable. | `vfio_assigned_device/src/lib.rs`, `virt_kvm/src/device_memory.rs`, `cca_in_place.rs` |
| MSI-X and byte access | Full-BAR memslots let shared table/PBA accesses bypass software MSI-X emulation. Split BARs at validated page exclusions and kept those pages host-mapped but absent from guest memslots. Also fixed page-aligned `MemoryRange` construction for byte transfers. | `vmm_core/virt_kvm/src/device_memory.rs`, existing MSI-X dispatch in `vfio_assigned_device/src/lib.rs` |
| FVP and test selection | Added explicit candidate profiles and exact hash selection. Kept the existing ARM boot registration independent and used the existing packaged smoke and full AHCI test. | `petri/src/vm/fvp/`, `petri/incubator/profiles/`, common artifacts, VMM tests |

The successful IRQ/mapping correction did not require unwinding the
TDISP state machine, moving guest UNLOCK before unbind, or changing the
64 MiB oracle. The normal BME policy remains an explicit experimental
lifecycle policy, not a claim of generic protected-device stop/restart support.

## Full test reproduction

Run from this repository with the approved licensed runtime already installed.
The paths below describe the tested local artifacts; replace them with
matching files, not arbitrary rebuilds.

```bash
export PETRI_FVP_PLATFORM_ROOT="$PWD/.packages/cca-tdisp-runtime"
export PETRI_FVP_PAYLOAD=cca74
export NEXTEST_TEST_THREADS=1
export PYTHONDONTWRITEBYTECODE=1

cargo xflowey vmm-tests-run \
  --include-emulated \
  --filter 'test(cca_tdisp_ahci_on_fvp)' \
  --fvp-deps-version 0.3.0-fvp-source-local \
  --fvp-payload-version 0.3.0-fvp-cca74-cb45796-local \
  --fvp-package-archive ../openvmm-deps/out/fvp-cca-source-packages/openvmm-test-fvp-cca.aarch64.0.3.0-fvp-source-local.tar.gz \
  --fvp-package-sha256 101a636410b7048ad6d789b7597ef2ddf3a5c38c4ffe073deb1a43134289ec49 \
  --fvp-payload-archive ../openvmm-deps/out/cca74-cb45796-packages-a/openvmm-test-fvp-cca-payload.aarch64.0.3.0-fvp-cca74-cb45796-local.tar.gz \
  --fvp-payload-sha256 0b0706ef788fa7a061f2811d759b2d2c3b29cdb61f0ef90707f2ed433077c0d7 \
  --dir target/cca74-final-tdisp
```

Use the same options with `test(cca_in_place_smoke_on_fvp)` for smoke.
Use the existing `boot_no_hv_on_fvp` case for ordinary ARM boot.
Run model tests sequentially because they share the ownership domain.
Do not edit active run inputs or bypass journals after failure.

## Final single-ABI commit split

Plan review: **Minor revisions**, incorporated below. This replaces the
dual-ABI split below.
Use three commits across the two repositories:

| Cut | Parent | Commit scope |
| --- | --- | --- |
| D1 | Deps `ksmtlzss` | Consolidated current-kernel producer, host/full payload pins, provenance, tests and producer documentation: all 16 combined changed paths |
| R1 | Root `xkuwvnxw` | Atomic sole-ABI runtime migration, matching FVP artifacts/profiles, existing basic onboarding, tests and Guide updates: all 49 combined changed paths except this report |
| R2 | R1 | This report, including finalized D1/R1 IDs and validation receipts |

### Final commit ledger

| Cut | Finalized change ID | Prefix validation | Status |
| --- | --- | --- | --- |
| D1 | `rylvukpl` | All 33 recipe tests; provenance 53; both default packages reproduced exact archive hashes; final mode-aware inventory matched | Committed locally |
| R1 | `mzuzyppt` | Exact 49-path mode-aware inventory; native/ARM check/clippy/doc; 558 native and 81 ARM tests; Windows/macOS shared checks; fixing/non-fixing full formatter | Committed locally |
| R2 | External receipt | Report-only scope and fixing/non-fixing formatter; own receipt stays outside the report | Final documentation cut |

R1 is deliberately atomic. The CCA implementation already runs in the
parent: changing its wire API or capability numbers without its native
callers and matched artifact/profile/test consumers would create a
half-ported prefix. Do not retain temporary compatibility symbols or
introduce an ABI selector to make smaller cuts compile. Review R1 by its
ABI, mapping/lifetime, artifact/backend and test areas instead.
The independent basic onboarding `nvmlowny` above `llpyuxxt` remains
untouched.

### Exact manifests

D1 contains these paths, including the deletion:

```text
.dockerignore
.gitignore
Dockerfile
README.md
cgmanifest.json
pkg/Tools/gen-cgmanifest.py
pkg/fvp-cca-host-kernel/README.md
pkg/fvp-cca-host-kernel/build.py (delete)
pkg/fvp-cca-host-kernel/kernel.py
pkg/fvp-cca-host-kernel/pins.json
pkg/fvp-cca-host-kernel/test_recipe.py
pkg/fvp-cca-payload/README.md
pkg/fvp-cca-payload/build.py
pkg/fvp-cca-payload/pins.json
pkg/fvp-cca-payload/tdisp-pins.json
pkg/fvp-cca-payload/test_recipe.py
```

R1 contains these paths, including the three deletions:

```text
Guide/src/dev_guide/tests/vmm/qemu_cca.md
Guide/src/reference/openvmm/management/cli.md
flowey/flowey_lib_hvlite/src/cca_artifacts.rs
flowey/flowey_lib_hvlite/src/cca_pins.rs
flowey/flowey_lib_hvlite/src/resolve_cca_payload.rs
openvmm/openvmm_core/src/worker/dispatch/realm_retention.rs
openvmm/openvmm_hypervisors/src/kvm.rs
petri/incubator/platforms/fvp-effective-run.fixture.yaml
petri/incubator/platforms/fvp-initrd-overlay.yaml
petri/incubator/profiles/aarch64-fvp-cca-realm-vfio-source-built.toml (delete)
petri/incubator/profiles/aarch64-fvp-cca-source-built.toml (delete)
petri/incubator/profiles/aarch64-fvp-cca.toml
petri/incubator/src/profile.rs
petri/incubator/src/run.rs
petri/petri_artifacts_common/src/lib.rs
petri/src/vm/fvp/backend.rs
petri/src/vm/fvp/cca_init.rs
petri/src/vm/fvp/cca_realm_vfio_teardown.sh
petri/src/vm/fvp/platform.rs
petri/src/vm/fvp/profile.rs
petri/src/vm/fvp/runtime.rs
petri/src/vm/fvp/staging.rs
vm/devices/pci/pci_core/src/vfio.rs
vm/devices/pci/vfio_assigned_device/src/lib.rs
vm/devices/pci/vfio_assigned_device/src/realm/access.rs
vm/devices/pci/vfio_assigned_device/src/realm/objects/tests.rs
vm/devices/pci/vfio_assigned_device/src/realm/objects.rs
vm/devices/pci/vfio_assigned_device/src/realm/tdisp/tests.rs
vm/devices/pci/vfio_assigned_device/src/realm/tdisp.rs
vm/devices/pci/vfio_assigned_device/src/realm.rs
vm/devices/pci/vfio_assigned_device/src/resolver.rs
vm/devices/user_driver/vfio_sys/src/iommufd/tsm/tests.rs (delete)
vm/devices/user_driver/vfio_sys/src/iommufd/tsm.rs
vm/devices/user_driver/vfio_sys/src/iommufd.rs
vm/kvm/src/arm.rs
vm/kvm/src/arm_smccc.rs
vm/kvm/src/lib.rs
vmm_core/virt_kvm/src/arch/aarch64/mod.rs
vmm_core/virt_kvm/src/cca_in_place.rs
vmm_core/virt_kvm/src/device_memory.rs
vmm_core/virt_kvm/src/lib.rs
vmm_core/virt_kvm/src/memory.rs
vmm_core/virt_kvm/src/rhi/tests.rs
vmm_core/virt_kvm/src/rhi.rs
vmm_core/virt_kvm/src/vfio.rs
vmm_tests/vmm_tests/tests/tests/aarch64_exclusive/tdisp_ahci.rs
vmm_tests/vmm_tests/tests/tests/aarch64_exclusive.rs
vmm_tests/vmm_tests/tests/tests/multiarch.rs
vmm_tests/vmm_tests/tests/tests/nested.rs
```

The captured reviewed inventories record file status, mode and SHA-256:
D1 has 16 paths and inventory digest
`a0d3b780dfb372fa58f8ef89b1600f8cf0a9bf9c3c387bc0108638dfde3f5acf`.
R1 has 49 paths and inventory digest
`9153f937f86e86a598fa350ae5e8031fad81d6e0713a8612603528b8e92ab9a2`.
Its runtime-only diff digest remains
`0c7e11cf34af261d25a17954976784710514212f45141733cbb1f58e1bea5e8b`.
The report is excluded from R1's inventory and all runtime digests.

### Construction and prefix gates

Use the existing workspaces sequentially; no model is active and there are
no unrelated working changes. Keep the validated source change IDs
`tuvpopsk` and `vxklnskt` available in operation history. Do not abandon
them or create recovery bookmarks.

For D1, use `jj new ksmtlzss`, then restore exactly the D1 manifest from
`vxklnskt`. Compare the actual prefix inventory with the reviewed D1
inventory before validation. Run all 33 producer recipe tests, the
53-component provenance check, and both default host-only/full Docker
package targets. Require exact reproduction of archives `084474d8...`
and `964fda7f...`; do not update a pin merely because a prefix differs.
Repeat the complete mode-aware inventory comparison after tests and builds,
immediately before committing. Commit with `jj commit` only after these
checks pass. Record its stable
change ID in this report before snapshotting Root for reconstruction.
Pin that report-bearing source snapshot by its full revision ID in the
external receipt before leaving Root; use that immutable source for restore.

For R1, use `jj new xkuwvnxw`, then restore exactly the R1 manifest from
the validated Root source. The report must be absent while R1 is checked:
validate that actual selected prefix, not a full tree with later code.
Compare all 49 statuses and hashes with the reviewed R1 inventory.
No intermediate code edits are planned.

The 12 modified Root packages are `flowey_lib_hvlite`, `openvmm_core`,
`openvmm_hypervisors`, `incubator`, `petri_artifacts_common`, `petri`,
`pci_core`, `vfio_assigned_device`, `vfio_sys`, `kvm`, `virt_kvm` and
`vmm_tests`. Run all-target check, all-target clippy and no-dependency
rustdoc for them on native Linux and AArch64 musl, with
`openvmm_hypervisors/virt_kvm` enabled. Run the existing eight-package
native unit selection and the complete 81-test ARM `virt_kvm` suite.
Repeat the applicable Windows MSVC and ARM macOS shared-consumer checks.
Run full `cargo xtask fmt --fix` last and require all four passes, then
run full non-fixing `cargo xtask fmt` and require success.
If it changes source, repeat the affected gates and formatter until
stable; compare the inventory again before committing R1.

R1 and the final R2 tree must reproduce the reviewed runtime-source digest.
The fresh model and native-control receipts above validate that source.
Changing only commit metadata or adding the report does not require
another model campaign. Any functional source or artifact change does.
Never use raw nextest on `vmm_tests`.

Restore the report from its saved Root snapshot after R1, add D1/R1
stable change IDs and prefix receipts, and finalize it as R2. Run the full
formatter fix and non-fixing check last before this documentation commit.
Require the complete R2 diff against R1, including modes and unexpected
paths, to contain only this report. R2's own finalized
change ID and final clean-tree verification belong in a persistent
external receipt, not another report-only commit.
After each `jj commit`, capture the finalized revision from `@-`, not the
new empty `@`. Keep full revision IDs in machine-readable receipts and
use stable change IDs in this report and user-facing references.

Use the standard Copilot co-author and session trailers. After all gates,
retarget only the local `fvp-kvm-cca-tdisp` bookmark to R2 under the user's
temporary-history rewrite permission. Preserve `xkuwvnxw` and all older
changes, the independent basic-onboarding line, and unrelated bookmarks.
Do not push either repository or mutate remote references.

This split finalizes the tested normal-path migration. It does not mark
candidate archives as fully qualified or silently include the separately
planned stronger guest teardown fixture. That work and firmware warning
adjudication remain explicit promotion limits.

## Review

Final single-ABI plan review: **Minor revisions**. The reviewer confirmed
that the three cuts are coherent and that no further functional split is
warranted. Captured diffs, source inventories, modes, ancestry and actual
package ownership resolved the read-only reviewer's initial tool-access
blocker. The plan now requires a non-fixing full formatter check, final
post-build inventories, report-only R2 scope verification and explicit
finalized-revision capture. Those revisions are incorporated above.
Actual prefix construction and pre-commit validation remain execution
gates, not claims made by this plan review.

## Previous commit split (superseded)

These are logical boundaries, not yet validated prefix commits.
The runtime cut is deliberately larger: ABI selection, checked mappings,
VFIO consumers, and RAM-retention forwarding must remain coherent.

| Cut | Repository | Scope | Dependency |
| --- | --- | --- | --- |
| D1 | `openvmm-deps` | Opt-in producer: all nine current changed files, including recipe, target, documentation and provenance registration | Existing deps tip `ksmtlzss` |
| D2 | `openvmm-deps` | Candidate-only guest teardown diagnostics and new immutable package identity; preserve old guest recipes and archives | D1; required before final qualification |
| R1 | OpenVMM | Existing basic ARM FVP onboarding: `multiarch.rs` and `nested.rs` only | Existing Root tip `xkuwvnxw` |
| R2 | OpenVMM | ABI foundations: `vm/kvm/src/{lib,arm,arm_smccc}.rs` and the `vfio_sys` IOMMUFD changes | R1 for linear history; no new candidate runtime enabled |
| R3 | OpenVMM | Atomic native runtime/selector integration, plus Petri's new field initialized to `false` for compatibility | R2 |
| R4 | OpenVMM | Petri/Incubator candidate profiles, pins, boot overlay, lifecycle, worker propagation, constructor activation and FVP Guide page | R3; final D2 artifact identity |
| R5 | OpenVMM | Existing packaged CCA smoke and full TDISP test selection/configuration; smoke bounce-pool setting and diagnostics | R4 |
| R6 | OpenVMM | This migration report and final local change-ID/validation ledger | R5 and final test/review evidence |

The independently prepared basic test change ID `nvmlowny` on the
infrastructure change ID `llpyuxxt` remains preserved for separate submission.
R1 records the same onboarding on the requested CCA migration lineage;
do not rewrite the independent change or the existing CCA bookmark.

### Explicit paths and intermediate constructor

R1 selects `vmm_tests/vmm_tests/tests/tests/multiarch.rs` and
`vmm_tests/vmm_tests/tests/tests/nested.rs`.

R2 selects `vm/kvm/src/lib.rs`, `vm/kvm/src/arm.rs`,
`vm/kvm/src/arm_smccc.rs`, and the three changed `vfio_sys` files:
`vm/devices/user_driver/vfio_sys/src/iommufd.rs`,
`vm/devices/user_driver/vfio_sys/src/iommufd/tsm.rs`, and
`vm/devices/user_driver/vfio_sys/src/iommufd/tsm/cca74.rs`.

R3 selects all current changes under `vmm_core/virt_kvm/src/` and
`vm/devices/pci/`, the changed files under `openvmm/`, and
`Guide/src/reference/openvmm/management/cli.md`. It also selects
`petri/src/vm/openvmm/construct.rs`, but with the newly required
`HypervisorConfig` field initialized as `linux_cca74: false`.
This is an intentional intermediate edit, not a partition of the current
final constructor alone.

R4 selects the remaining current `petri/` changes and
`Guide/src/dev_guide/tests/vmm/qemu_cca.md`. It replaces that compatibility
initializer with the final Linux/AArch64/payload-mode expression currently
in `construct.rs:739-744`. Keep the candidate enum, artifact constants,
profile consumers, and this activation together.

R5 selects `vmm_tests/vmm_tests/tests/tests/aarch64_exclusive.rs` and
`vmm_tests/vmm_tests/tests/tests/aarch64_exclusive/tdisp_ahci.rs`.
R6 selects this report only.

Before R3 is finalized, apply the same compatibility initializer in the
primary workspace and the prefix-validation workspace. After R3, restore
the final expression as the R4 remainder. Run no model during this transition.
Require the final tracked-source inventory, excluding report metadata, to
match the reviewed final candidate source. If these intermediate edits are
not desired, combine R3 and R4 instead; do not commit a broken boundary.

If prefix validation finds a constructor or consumer needed earlier, move
the cohesive declaration and callers together or fold neighboring cuts.
Do not ship a non-compiling prefix or select files solely by directory.
This includes enum match arms and forwarding methods with compatibility
defaults that can otherwise hide an incomplete port.

## Historical dual-ABI reviews and validation (superseded)

Run independent formal code review and rubber-duck review in parallel.
Use GPT-6.1 Sol for formal code review and Claude Opus 5.5 for rubber-duck
review. Review the final candidate changes against `xkuwvnxw`, and review
the deps producer against `ksmtlzss`.

Review priorities are:

1. Native ABI layouts, reserved fields, completion codes, filter ownership,
   private TIO approval, and no capability-based fallback.
2. BAR segmentation, shared MSI-X emulation, byte-access safety, mapping
   permissions, slot retirement, and host VMA lifetime.
3. BME release/rebind semantics, lock ordering, pending DMA/RAM ownership,
   cancellation, and failure retention. The first happy-path pass does not
   establish generic stop/restart or error recovery.
4. Parentless vIOMMU/shared-DMA allocation and transparent wrapper forwarding.
5. Exact profile/package hashes, candidate-only behavior and legacy controls.
6. Test oracle, real I/O and native UNLOCK evidence, post-teardown diagnostics,
   and Guide claims that distinguish a local pass from final qualification.

Recorded guest shared-MMIO RIPAS warnings and PCI IRQ-parse errors still
need source review. The RMM log also contains an invalid DOE VDM header
message during physical link teardown. Classify that message from its
surrounding operation flow; neither declare a failure nor dismiss it only
because the launcher exited zero.

Before final promotion, create a separately pinned candidate diagnostic
fixture that checks post-unbind/post-UNLOCK failures. Do not modify the
qualified old producer or remove the original oracle.

D2 must preserve LOCK/accept/probe/read/hash/unbind/UNLOCK ordering and the
64 MiB expected hash. Capture kernel diagnostics before final unbind and
again after UNLOCK. Fail on new warning/error diagnostics in that teardown
window, including AHCI engine-stop and device-MMIO unmap failures; do not
whitelist them merely to retain a passing test. Require successful sysfs
writes, absence of the bound driver, and externally confirmed native
UNLOCK/owner teardown. A successful void guest callback alone is insufficient.
Test the diagnostic checker with synthetic failing logs, not deliberately
failed kernel teardown. Resolve pre-existing warning classifications before
using them as a final qualification baseline.

Record D2's new guest script, initrd, manifest, package version and hashes.
Keep the tested archive above unchanged as historical evidence. Re-run both
independent code reviews over D2 and its matching Root pins. Run final-stack
ordinary ARM boot, smoke, full TDISP/AHCI and standalone host command.
Then run the frozen old-consumer/old-tuple controls, not new ABI consumers
against an old kernel. Each run must retire its ownership domain first.

For every Root code commit, identify all affected packages and validate the
actual selected prefix, not the complete unsplit working tree:

```bash
cargo check --offline --all-targets -p <PACKAGE>
cargo clippy --offline --all-targets -p <PACKAGE>
cargo doc --offline --no-deps -p <PACKAGE>
cargo nextest run --offline --profile agent -p <UNIT_TEST_PACKAGE>
cargo xtask fmt --fix
```

Combine package selectors where appropriate. VMM tests must instead use
`cargo xflowey vmm-tests-run`; never raw nextest on `vmm_tests`.
Check native and AArch64 consumers, plus applicable foreign shared-config
consumers. Run the full formatter last and inspect all four passes.
The deps repository needs its existing recipe tests, provenance generator
check, and reproducible package checks.

Use one reusable scratch jj workspace when needed to validate prefixes
without disturbing the remaining working changes. Keep caches controlled.
If a shared target embeds another checkout's xtask root, select the intended
checkout with `--custom-root`. No new recovery bookmark is needed.

For each cut, construct only the specified prefix on its intended parent
in that workspace. Record the parent change ID, tracked-file SHA-256
inventory, selected paths, intermediate edits, target triples, commands
and exit codes. Do not validate another full-tree checkout as a prefix.
If formatter fixes change code, repeat the applicable checks/tests and
then the full formatter last until the prefix is stable. Compare the
selected primary-workspace source with that validated inventory before commit.

After a prefix passes, finalize only its explicit paths with `jj commit`.
The installed CLI supports this: selected paths become the closed change
and remaining changes stay in a new working change on top. Do not use
`jj describe`, amend old changes, move existing bookmarks, or push.
Record the finalized change ID using:

```bash
jj log -r @- --no-graph -T 'change_id.shortest(8) ++ "\n"'
```

Keep the standard Copilot co-author and session trailers in the descriptions.
After each code or dependency commit, update this report's ledger and verify
that the working changes are the expected remainder. Update the opening status
and working change IDs as execution progresses. Re-run the full tests on the
final stack and exact final artifact tuple.

Commit R6 with receipts through R5. Record R6's own finalized change ID and
post-commit verification in a separate persistent session receipt; do not
create an endless sequence of commits that only record the preceding report
commit. Leave no undisclosed report edit in the worktree after R6.

## Superseded commit ledger

Update this table as changes are finalized. All rows are currently planned.

| Cut | Finalized change ID | Prefix validation / review / runtime receipt | Status |
| --- | --- | --- | --- |
| D1 | Pending | Producer tests/build receipts exist; final review pending | Planned |
| D2 | Pending | Candidate-only diagnostic fixture not implemented | Planned |
| R1 | Pending | Independent `nvmlowny` evidence exists; CCA-lineage prefix gate pending | Planned |
| R2 | Pending | Scoped unit/static checks exist; prefix gate pending | Planned |
| R3 | Pending | Normal runtime passed; full review and prefix gate pending | Planned |
| R4 | Pending | Current candidate profiles passed; final artifact/prefix gate pending | Planned |
| R5 | Pending | Smoke and full TDISP receipts above; final-stack gate pending | Planned |
| R6 | External receipt | Its own finalized ID is recorded outside this committed report | Planned |

## Historical plan review (superseded)

Initial review: **Needs rework**. It found the R3 declaration/R4 constructor
dependency, insufficient prefix manifests, incomplete D2 gates, and a final
report-receipt loop. The revised plan specifies the compatibility initializer,
explicit cut paths and inventories, diagnostic gates, and an external R6
receipt.

Re-review: **Ready**. The reviewer checked the constructor dependency and
accepted all four corrections. No further concrete cut-blocking dependency
was identified in that bounded review. This approves the plan, not prefix
compilation, the full implementation, or final qualification.
No prefix builds or commits have been performed.
