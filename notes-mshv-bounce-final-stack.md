# MSHV bounce stack findings

These findings record the squash discussion before the latest-main rebase.
They are not authorization to squash commits or remove more behavior.

## Tested source before the rebase

- Bounce tip: change ID `vnrmnomr`, bookmark `mshv-bounce-simplified`.
- Base: change ID `yxvslnsp`.
- Aggregate diff to that base: 74 files, 6,455 additions, 446 deletions.
- Local validation: 697 package tests passed, three skipped; all three
  forced-bounce VMM tests passed. Native checks, Clippy, docs, formatting,
  and ARM Windows/macOS/Linux Clippy passed.
- SNP hardware: RAM and buffered-file disk runs passed on `chris-mshv`.
  Each completed 30,720 verified block operations, two TCP/vsock rounds,
  all device unbind/rebind cycles, 13 kernel-proven release/resume pairs,
  and 65,536 balanced pins. Kernel logs stayed unchanged and teardown was
  clean.
- Evidence: `target/adjacent-cleanup-20261021/validation-summary.json`.
  The first hardware attempt was rejected because its trace included
  other guest partitions. It is preserved separately from the accepted retry.
- Those hardware runs used the CLI. They did not establish end-to-end
  ttrpc SNP or device-mapper coverage.

## Proposed final five-commit stack

| Commit | Pre-rebase owner | Intended content |
| --- | --- | --- |
| Guest-memory foundations | `lspwqqqu` | CopyOnly policy, user/kernel access distinctions, export restrictions, and required caller changes. Absorb the queue-policy rollback so the removed completion contract never appears in the final commit. |
| Storage support | `osyznnpq` | Lower-backend CopyOnly support, bounds checks, exact-transfer fixes, and tests. Absorb frontend-staging removal, FUA rollback, normal allocation, and comment cleanup. |
| Network/vsock support | `ltxryxrn` | Software-backend admission, RX safety, existing bounce fallbacks, and partial-write/credit fixes. Absorb arena/per-device-mode removal and boxed-ring/comment restoration. |
| MSHV SNP integration | `poyqlmtw` | Fault-driven host grants, automatic CopyOnly selection, VM-wide force option, DMA-export gate, CLI admission, and forced-bounce VMM tests. Absorb its validation and editorial cleanup. |
| VM-service SNP support | Combine `rwvusozw` and `zpowklwv` | Supported-device admission at create/hot-add, base-page defaults, disk-opening policy, and service tests/docs. No separate bounce mechanism or force option. |

The follow-up changes to distribute by hunk are:

- `urpzwrks`: restore the existing queue completion policy.
- `losxqlwv`: centralize CopyOnly policy and remove frontend staging/modes.
- `ystkqzwp`: undo the unrelated FileDisk FUA extension.
- `ymyoooky`: restore baseline comments and layout.
- `vnrmnomr`: restore fixed ring storage and normal FileDisk allocation;
  remove unused validation results and duplicate/history-only tests;
  retain the DMA gate without reindenting VFIO registration.

Do not squash a whole follow-up into one ancestor when it changes several
owners. Keep each final commit buildable. Rename the storage and network
commit descriptions: they no longer implement mandatory frontend staging.
A history-only squash must preserve the final tested source tree.

The separate usermode-lock and lite branches are not part of this proposed
bounce-stack squash.

## Surviving ttrpc rationale

The service does not enable bouncing itself. MSHV SNP reports CopyOnly,
and dispatch applies that policy regardless of the management frontend.
The service still selects memory defaults and performs early admission.

Retain the following behavior:

- Allow supported PCIe virtio-blk, software virtio-net, and in-process
  virtio-vsock instead of rejecting every PCIe topology.
- Apply the same admission to boot devices and hot-added devices.
- Retain the current VPCI and unsupported-device restrictions.
- Default SNP memory to base pages and reject explicit huge-page requests,
  subject to checking the latest upstream backend requirements.
- Retain VM-level policy state while hot-add admission needs it.
- Do not expose a ttrpc force-bounce option.

The normal Linux raw-disk path already returns `BlockDeviceDiskHandle`
for regular files and host block devices. The block backend already copies
through owned buffers when guest memory cannot be kernel-locked. A special
ttrpc block backend is therefore not a bounce prerequisite.

The current SNP-specific opener also enforces a narrower disk policy:
buffered/plain regular files, descriptor-based block-device classification,
block-device aliases with image-like suffixes, and nonblocking rejection
of unsupported descriptors. Those are opening/admission rules, not bounce
mechanisms.

Consolidating that opener with the shared disk-opening code remains a
separate decision. Replacing it with the generic builder without equivalent
validation would also remove existing restrictions. Do not hide that behavior
change inside the history cleanup.

Pre-rebase evidence locations:

- `vmm_core/virt_mshv/src/x86_64/mod.rs:443`: backend CopyOnly policy.
- `openvmm/openvmm_core/src/worker/dispatch.rs:1455`: central policy selection.
- `openvmm/openvmm_entry/src/ttrpc/mod.rs:654`: service admission.
- `openvmm/openvmm_entry/src/ttrpc/mod.rs:1193`: memory defaults.
- `openvmm/openvmm_entry/src/ttrpc/mod.rs:1605`: hot-add path.
- `openvmm/openvmm_entry/src/ttrpc/mod.rs:2489`: virtio-blk construction.
- `openvmm/openvmm_entry/src/ttrpc/mod.rs:2527`: SNP-specific opener.
- `openvmm/openvmm_helpers/src/disk.rs:152`: shared raw-disk opening.
- `vm/devices/storage/disk_blockdevice/src/lib.rs:576,638`: backend bouncing.

These line references describe the saved pre-rebase source, not future
upstream or rebased line numbers.

## Follow-up test requirements

After the rebase, assess upstream MSHV SNP VMM-test support and the
PCIe-capable IGVM builder before adding new harness code.

Coverage must include:

- SNP virtio-blk payload verification, not just successful boot.
- RAM-backed, regular-file-backed, and host-block-backed virtio-blk disks.
- Host block devices, including a disposable loop/device-mapper fixture,
  in both local backend tests and VMM tests.
- Buffered and direct host-block I/O, plus read-only behavior where applicable.
- PCIe device hot-add, I/O after add, removal, and re-add.
- ttrpc create/add/remove paths and verified I/O, not only resource-builder
  unit tests or CLI-created VMs.
- Equivalent host-block-backed coverage through ttrpc.
- Appropriate host capability and privilege gates. Tests must use disposable
  fixtures and must not write arbitrary host disks.

Keep the existing exact-transfer, vsock partial-write, guest-input safety,
and CopyOnly DMA protections. New coverage and later squash work are separate
from the rebase.

## Latest-main rebase

The current bounce stack was copied onto upstream `main`, change ID
`yywkuopm`, on 2026-10-21. The user approved new change IDs so the separate
usermode-lock, lite, integration, and historical branches could remain
unchanged. No feature commits were squashed.

| Pre-rebase change ID | Rebased change ID | Role |
| --- | --- | --- |
| `lspwqqqu` | `ntxotomx` | Foundations |
| `osyznnpq` | `tmtsmuvz` | Storage |
| `ltxryxrn` | `wyqukmov` | Network/vsock |
| `poyqlmtw` | `llulpvzx` | MSHV SNP |
| `rwvusozw` | `lmzzsxzq` | VM-service SNP |
| `zpowklwv` | `lskxuoru` | VM-service host block |
| `urpzwrks` | `zlkwzyts` | Queue-policy rollback |
| `losxqlwv` | `xlvkzsnt` | Central CopyOnly simplification |
| `ystkqzwp` | `mrprmxzq` | FUA rollback |
| `ymyoooky` | `ykykunst` | Comment/layout cleanup |
| `vnrmnomr` | `slmxvmpo` | Adjacent cleanup |
| `xtxmzryz` | `xrtkmmtr` | These saved findings |

The original tested source and notes remain reachable through
`pre-rebase-bounce-20261021` and `pre-rebase-bounce-notes-20261021`.
The source bookmark is `mshv-bounce-simplified`; the findings have their own
bookmark, `mshv-bounce-stack-notes`. The findings are a review artifact, not
one of the five proposed feature commits.

The only textual rebase conflict was in Petri's default virtio-vsock
selection. The resolution keeps upstream's `isolated_without_paravisor`
default while removing the obsolete per-device bounce flag in the later
policy-removal commit. The final `petri/src/vm/mod.rs` matches upstream.

### Upstream support now available

- `vmm_tests/vmm_tests/tests/tests/x86_64/snp_linux_direct.rs:13,18`
  registers one- and two-VP SNP boot cases through
  `configs(snp_linux_direct_x64)`.
- `vmm_tests/vmm_test_macros/src/lib.rs:1414` requires MSHV and an AMD host
  for that configuration. Passing an ordinary local KVM test does not
  establish SNP coverage.
- `flowey/flowey_lib_hvlite/src/build_snp_linux_direct_igvm.rs:54`
  builds the SNP test IGVM and embeds pipette in its initrd.
- `openvmm/openvmm_core/src/worker/vm_loaders/igvm.rs:917` passes the
  runtime PCIe host bridges into the device tree. The SNP bootshim builds
  PCIe SSDT and MCFG tables from those bridges in
  `vm/loader/snp_bootshim/src/acpi_tables.rs:36`.
- `vm/loader/igvmfilegen/src/snp_linux_direct.rs:140` reserves output
  space rather than embedding the old fixed ACPI topology. This addresses
  the earlier PCIe-capable IGVM fixture limitation.
- `vmm_tests/vmm_tests/tests/tests/ttrpc.rs:39` provides the existing
  local RPC regression and reusable launch/configuration helpers.

### Rebase validation

- Native check, all-target Clippy, and no-dependency docs passed for the
  selected memory, MSHV, storage, virtio, management, and Petri packages.
- 708 package tests passed, three skipped.
- All three existing forced-bounce VMM tests and the ordinary local ttrpc
  interface VMM test passed. These are not new SNP/ttrpc hardware tests.
- ARM Windows/macOS/Linux management, device, and Petri Clippy passed.
- At the rebase hand-off, no post-rebase SNP hardware campaign had run.
  Subsequent hardware results are recorded below.
- Rebase evidence is under `target/rebased-main-20261021/`.

## Implemented SNP, host-block, and RPC coverage

The post-rebase follow-up added:

- Registered block tests for ordinary forced-CopyOnly guests and SNP IGVM
  guests, with RAM, regular-file, buffered host-block, and direct host-block
  backends.
- Separate boot and hot-added disks, verified binary payloads and backing
  files, removal, and read-only re-add for persistent backends.
- SNP-over-ttrpc tests for regular files and buffered/direct host block
  devices, covering create, add, remove, I/O, and complete process teardown.
- Owned disposable loop fixtures behind `PETRI_CAPABILITIES=loop_device`.
  A keeper descriptor and kernel autoclear protect against loop-device
  leaks when a test is killed.
- Portable `--target linux-musl-x64` Flowey bundles. The target host needs
  neither a Rust toolchain nor the build host's glibc version.

Removal is quiesced: the guest virtio-pci driver is unbound while MMIO is
still present, then the host removes the function. Tests wait for complete
guest PCI teardown before re-add. The user confirmed that surprise removal
is a known issue and selected this scope; no production PCIe removal code
was changed.

The new RPC tests needed a below-4-GiB ECAM override in this test setup.
Without it, the SNP guest reported no PCI configuration access function
and could not discover its virtio devices.

**The user requires this override to remain a separate local-only top
commit. It will not be submitted.** Do not fold it into the proposed
VM-service commit or any other submitted change. Its coupled ECAM reference
documentation stays in that local commit too. Non-SNP ECAM placement is
unchanged.

`mshv-bounce-simplified` identifies the tests and runner changes without the
override. `mshv-snp-ecam-local` identifies the local top commit above it.
The SNP RPC hardware results below include this local override; they are
not evidence that the submission candidate works without it on this host.

Ordinary MSHV test guests retain Hyper-V clocks with VMBus disabled.
Ordinary KVM CopyOnly tests keep Hyper-V disabled because page overlays
need pointer access. Backend selection uses Petri's host context.

### Hardware and local results

- Final registered hardware run: 11 passed, no failures. It includes two
  ordinary host-block cases, four SNP block cases, three SNP RPC cases,
  and upstream one-/two-VP SNP boot tests.
- Six local KVM VMM regressions passed, including existing block/net/vsock
  bounce tests, the new RAM/file hotplug tests, and ordinary ttrpc.
- Final native Clippy/docs and 212 package tests passed, with one privileged
  test skipped locally. ARM Windows/macOS/Linux checks also passed.
- The previously ignored VM-service host-block opener unit test passed
  against an owned device-mapper linear target, including buffered/direct
  and read-only/read-write descriptor modes.
- Both established RAM/file SNP campaigns now boot the same PCIe-capable
  IGVM. Their approved guest kernel, initrd, payloads, and nonce are
  unchanged. Each passed 30,720 verified block operations, two TCP/vsock
  rounds, all driver unbind/rebind cycles, 13 raw-trace-proven release/resume
  pairs, and 65,537 balanced pins.
- The host was observed already rebooted after the user authorized reset.
  No extra reboot or forced partition deletion was performed. Hardware
  results use kernel `6.18.34.mshv3-mshv-lite-shared-5e10`, not the earlier
  cached-grants kernel.
- Evidence is under `target/snp-igvm-coverage-20261021/` and
  `target/snp-igvm-lifecycle-20261021/`. Earlier failed setup attempts remain
  separate from the accepted final run.

The normal local WSL account lacks loop-device privileges. Real host-block
and device-mapper cases ran on `chris-mshv`; they were not replaced by
regular-file proxies.

## Final five-commit reconstruction, 2026-10-27

The review candidate is now `mshv-bounce-review`. It has these five changes,
oldest first:

| Change ID | Content |
| --- | --- |
| `pwwwuytm` | Guest-memory CopyOnly policy, user/kernel access split, and required caller and resolver adapters. |
| `vpmnuqou` | Existing storage copy paths, exact transfers, input checks, and tests. No frontend staging or FUA extension. |
| `srnrnxor` | Software network/vsock admission, RX input checks, and partial-write progress. No arenas or per-device bounce switches. |
| `kyvvnuyk` | MSHV SNP integration, CLI policy, forced-bounce and SNP block tests, owned host-block fixtures, and portable musl runner. |
| `olnxxntw` | Combined VM-service SNP and host-block support, disk-opening rules, and SNP RPC tests. |

This is a new copy, not a rewrite of the earlier source bookmarks. The
candidate uses the already-tested October 21 base, `yywkuopm`; it has not
been rebased onto the newer October 27 main.

The candidate tree exactly matches `tynzlwyq`, except that this local review
notes file is excluded. Inverse cleanup hunks no longer appear as separate
commits. The upstream virtio queue completion contract is retained throughout.
Required API adapters are in the foundations change; policy activation waits
until the MSHV change. VM-service admission and its documentation wait until
the fifth change.

Each boundary passed compilation, scoped unit tests, all-target Clippy,
no-dependency docs, and full workspace formatting before its commit:

| Change | Tests passed | Tests skipped |
| --- | --- | --- |
| Foundations | 538 | 7 |
| Storage | 42 | 0 |
| Network/vsock | 117 | 1 |
| MSHV integration | 429 | 0 |
| VM service | 185 | 1 |

These are overlapping package selections, not a count of unique tests.
The completed candidate also passed all six local KVM VMM regressions and
ARM Windows, macOS, and Linux compilation checks.

Independent GPT-6.1 Sol and Opus 5.5 boundary reviews found no blocking
issues. One documentation sentence about RPC tests was moved from the fourth
change to the fifth, where those tests are introduced. This preserves the
final source tree.

### Local-only EFI/SMBIOS overlay

The original finalized EFI experiment is `ksswrkxp`, at
`mshv-snp-efi-local`. It publishes measured synthetic EFI/SMBIOS metadata
and a final E820-derived EFI memory map. It does not implement EFI runtime
services. The generator inserts the required `efi=noruntime` before init
arguments and bounds the complete command line to avoid Linux truncation.
The bootshim remains `no_std` and uses a 64-KiB stack.

Independent GPT-6.1 Sol and Opus 5.5 reviews found no blocking issues after
the command-line and stack fixes. The final tree passed 227 package tests,
the minimal-runtime build, ARM loader compilation, Clippy, docs, and
formatting.

The final hardware run passed all 11 registered cases without the low-ECAM
workaround. All nine SNP cases verified EFI SMBIOS discovery, the BIOS date,
and an MCFG ECAM above 4 GiB. They include all three SNP ttrpc disk cases:
regular files and buffered/direct host block devices. The host finished with
zero child partitions, zero loop devices, 545,554 balanced pins, and no new
kernel errors. Evidence is under `target/snp-efi-final-20261027/`.

A copy of this exact source tree is placed above the five review commits
for local ttrpc testing. Only this local notes artifact changes afterward.
That copy is `smonmxny`, at `mshv-bounce-review-local`.
Neither the EFI experiment nor the old `mshv-snp-ecam-local` workaround
belongs in the submission stack. Both original alternatives remain preserved.

The user confirmed that the separately observed boot-kill teardown leaks
are a known MSHV driver issue with a forthcoming fix that is not available
yet. That investigation is closed; no workaround or kernel change is part
of this reconstruction.

Reconstruction evidence is under `target/mshv-five-stack-20261027/`.

## Latest-main rebase and refreshed-kernel validation

The five feature changes and the separate local-only EFI overlay are now
rebased onto upstream main, change ID `uqnopqwq`. The five change IDs above
remain stable. `pre-main-rebase-20261027` preserves a copy of the old stack.
The separate usermode-lock, lite, integration, and report branches were not
changed.

The storage merge retains exact-transfer loops and the upstream short-read
regression. Virtio-blk tests now use upstream `disk_hook::HookDisk`, with a
thin buffer-probe decorator where a hook cannot inspect guest memory.
The RPC merge retains upstream UEFI configuration and its console list,
while preserving SNP admission for every configured console. New RTC
devices remain outside the SNP allowlist. Tests use the new
`linux_direct_x64[snp]` configuration and explicit vsock enable argument.

The two disk-opening simplification changes are already folded into their
owners: the CopyOnly regular-file regression is in `vpmnuqou`, and shared
normal RPC disk opening is in `olnxxntw`. There are still five submission
changes, with `smonmxny` as the local-only EFI/notes overlay.

Native validation passed 1,142 affected-package tests, with nine repository
ignored tests. All six local KVM VMM regressions passed. All affected-package
Clippy and docs, full formatting, ARM Windows/macOS/Linux checks, and the
minimal-runtime SNP bootshim build passed. Independent formal and
rubber-duck reviews found no remaining merge issues.

On `chris-mshv`, kernel `6.18.34.mshv4-snp-teardown2`, validation passed:

- 1,144 affected-package tests, including the two privileged disk cases.
  Seven repository-ignored tests remain excluded: six KVM interrupt tests
  and the partial-consume network-backend contract test.
- All 18 affected VMM cases, including all four SNP ttrpc block cases,
  ordinary forced-bounce block/net/vsock, and one-/two-VP SNP boot.
- Eighteen clean CLI shutdown cycles across one, two, and four VPs.
- Sixty early-startup SIGKILL cycles, alternating owned launcher and worker
  targets, across one, two, and four VPs.
- Six kills with verified active TCP echo traffic, and four concurrent
  batches with two or four VMs (12 VMs total).
- A fresh boot after each accepted early, I/O, and concurrent campaign.
  An additional SNP ttrpc direct-file I/O case passed after all stress.

These are kernel-cleanup stress checks, not a claim that deliberately killed
VMs emit no userspace errors. Expected mesh/Consomme disconnect diagnostics
are recorded. The active-I/O kill proof is TCP, not concurrent block I/O.
Earlier harness false stops and their raw evidence remain separate from
the accepted runs.

The former eight-partition teardown leak did not recur. Final checks found
zero child partitions, loop devices, and mapper devices, with 16,004,526
balanced pin acquisitions/releases during the full hardware campaign.
There were no new kernel WARNs, isolation-state denials, or SNP
finalization/deletion failures. Forced startup kills did log
`Failed to pin memory region: -12`; those messages are retained in the
kernel evidence rather than described as a completely error-free dmesg.

Evidence and accepted stress results are under
`target/latest-main-rebase-20261027/`.
