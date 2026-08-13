# CCA v15 Stack Layering Plan

## Goal

Reorder and consolidate the current stack so that all upstreamable KVM CCA v15
implementation precedes local build, Flowey, incubator, test, and documentation
changes.

The upstreamable SNP/CCA portion should be independently reviewable and
movable into the earlier KVM SNP/CCA pull request stack without carrying local
platform automation.

## Target Layers

1. Existing upstreamable SNP support.
2. Upstreamable generic membacking support needed by CCA.
3. Upstreamable KVM CCA v15 implementation.
4. Local CCA v15 build and FVP validation.
5. QEMU CCA proof and typed artifacts.
6. QEMU CCA incubator and normal VMM test.
7. FVP CCA probe, incubator, and Flowey integration.
8. Developer documentation and remaining cleanup.

## Layer 1: Upstreamable SNP and CCA

Keep the existing SNP stack unchanged through its final private-memory support
commit.

The CCA portion should then use the following order.

### 1. Propagate partition unmap failures

Split the generic membacking changes out of `vuplxutp` into a standalone commit
immediately before the initial CCA Realm support:

```text
membacking: propagate partition unmap failures
```

The current `vuplxutp` diff changes these files:

- `openvmm/membacking/src/memory_manager/device_memory.rs`
- `openvmm/membacking/src/memory_manager/mod.rs`
- `openvmm/membacking/src/partition_mapper.rs`
- `openvmm/membacking/src/region_manager.rs`

Before splitting, inventory the complete trait and call graph for
`PartitionMemoryMap::unmap_range`, region unmap/teardown RPCs, and every
backend implementation. Include any additional trait definitions, backend
implementations, callers, and tests required for this commit to compile on all
supported hosts. Do not assume the four currently changed files are a complete
cross-platform boundary.

This commit should:

- make region and partition unmap operations fallible;
- propagate failures through RAM visibility control and RPCs;
- surface explicit teardown failures;
- log failures in infallible `Drop` paths; and
- preserve the VA reservation when partition unmap fails.

Do not include CCA-specific fatal-state or RIPAS behavior in this commit.

### 2. Introduce KVM CCA Realm support using the v15 ABI

Rebuild `uqkuzspt` directly against v15, selectively folding the core
implementation from `motsnxpl`. Do not mechanically squash the whole change.

The resulting initial CCA implementation should directly target v15 and
include:

- v15 KVM capability values:
  - `KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES = 250`;
  - `KVM_CAP_ARM_RMI = 251`;
- Realm VM creation with a probed supported IPA size;
- the v15 Realm VM type encoding;
- guestmemfd-default private state for CCA;
- VM memory-attribute private state retained for SNP;
- the `KvmGuestMemfdPrivateState` distinction;
- v15 initial Realm population and no-progress detection;
- v15 capability validation; and
- the correct CCA shared GPA bit derived from the probed Realm IPA size.

This is also where the x86 SNP call sites should be updated to use
`KvmGuestMemfdPrivateState::VmAttributes`. Those changes preserve existing SNP
behavior and should not appear as a later CCA fix.

Fold the applicable comments from `wrwsoyzo` into this commit rather than
retaining a separate documentation-only fixup.

Validate this rewritten commit before proceeding. The focused validation must
cover Realm IPA probing, VM type encoding, VM-fd capability results, initial
population progress, and unchanged SNP `VmAttributes` behavior.

### 3. Add CCA shared device memory aliasing

Keep `nllyypuw` as the next standalone commit:

```text
openvmm: add CCA shared device memory aliasing
```

It should consume the shared GPA bit established by the v15 Realm-support
commit.

### 4. Validate CCA configuration limits

Squash `kkmyyqvk` into `yyqznlzw`.

The resulting configuration-validation commit should include:

- architecture and firmware restrictions;
- memory-layout restrictions;
- unsupported configuration rejection; and
- global and per-NUMA hugetlb rejection for CCA.

Before landing RIPAS hole punching, audit and reject configurations whose
memory can be pinned or accessed without coordinated visibility transitions.
This includes assigned devices, vhost-user or other uncoordinated DMA
consumers, and any backing that can race `MADV_REMOVE` or guestmemfd hole
punching. Put cleanup-critical restrictions in this commit or in the RIPAS
commit itself; do not leave an unsafe intermediate revision.

### 5. Clean CCA backing on RIPAS and slot changes

Keep the KVM-specific portion of `vuplxutp` as a focused commit after shared
aliasing and the required safety validation:

```text
virt_kvm: clean CCA backing on RIPAS changes
```

Include:

- `MADV_REMOVE` cleanup of stale shared file-backed memory, with the existing
  anonymous-mapping fallback;
- guestmemfd hole punching when memory becomes shared;
- cleanup during CCA memory-slot replacement and unmap;
- partial RIPAS range handling that permits unbacked gaps;
- overlap validation;
- the CCA fatal-memory-cleanup latch;
- forced VP exits after a fatal cleanup failure; and
- rejection of later VP re-entry once the partition is fatal.

Move the RIPAS and cleanup-specific comments from `wrwsoyzo` into this commit.

## Layer 2: Local CCA v15 Enablement

These changes remain local developer and validation infrastructure. They must
follow all upstreamable SNP/CCA commits.

### Kernel builds

Squash:

- `lrstkruy` — reproducible CCA kernel build scripts;
- `kmonmntn` — Flowey host/guest kernel artifact.

Suggested result:

```text
local: build reproducible CCA v15 host and guest kernels
```

Keep both distinct host and guest configurations and the pinned v15 Linux
revision.

### Preflight

Keep `zmqwsqsz` in the local validation layer after every upstreamable core and
backend-neutral Petri commit:

```text
local: update CCA preflight for KVM v15
```

If the preflight tool is eventually included in the upstream PR, fold this
change into the commit that introduces the preflight probe. Otherwise, keep it
in the local validation layer.

### Pinned FVP platform

Squash `slytryms` and `zwonrqvp` so the history never introduces a v14 platform
only to replace it later.

Suggested result:

```text
local: pin KVM CCA v15 FVP environment
```

This commit should contain the final compatible Linux, TF-A, and TF-RMM pins.
QEMU and EDK2-specific pins should remain in the later QEMU incubator layer.

### FVP validation automation

Squash:

- `xqqwyoqu` — noninteractive preflight;
- `pqxvyyvr` — hardened FVP automation.

Suggested result:

```text
local: validate KVM CCA v15 on FVP
```

This commit should provide the reproducible preflight and nested OpenVMM smoke
path used to prove the core v15 implementation before incubator work begins.

### Planning documents

Collapse the incremental migration planning and findings changes:

- `kkkmmsly`;
- `nlzmwtqt`;
- `kpnztmku`.

Either retain one final local migration/results document or omit these planning
changes from the eventual upstream PR.

## Layer 3: QEMU CCA Proof and Typed Artifacts

Begin QEMU-specific work only after the FVP v15 validation layer passes.

### QEMU firmware builder

Keep `kmwruwln` standalone:

```text
local: build QEMU CCA firmware
```

### QEMU preflight proof

Squash:

- `rnrmmwsw`;
- `zyuloswl`.

Suggested result:

```text
local: test QEMU CCA preflight
```

### QEMU nested smoke proof

Squash:

- `wutluxts`;
- `mxqspmuo`.

The compatible CPU shape, including disabled LPA2, is part of the working smoke
test and should not appear as a later fixup.

Suggested result:

```text
local: test nested OpenVMM CCA under QEMU
```

Fold `oylpwmmn` into the final QEMU proof/results change or into a later
documentation commit.

### Typed QEMU artifacts

Keep these as separate reviewable commits:

1. `zqwqvppv` — typed QEMU and firmware artifacts.
2. `oxwstuyx` — packaged CCA host rootfs.
3. `vrmonnnz` — staged QEMU CCA platform.

## Layer 4: QEMU CCA Incubator and VMM Test

Keep these major implementation boundaries:

1. `wuozulrv` — QEMU CCA profile.
2. `kkyvzyzn` — QEMU CCA runtime.
3. `qrnmmwuz` — QEMU CCA Flowey wiring.
4. `ysrvpxov` — normal nested CCA Realm VMM test.

`lvztnuvn` is backend-neutral test infrastructure and must already have landed
after the upstreamable core layer and before any QEMU or FVP profile/runtime.

Review `qrnmmwuz` for runtime hunks that belong in `kkyvzyzn`; keep its final
form focused on Flowey artifact selection and target-runner wiring.

Split `xwtsnlzn`:

- move `.without_serial_output()` and its explanatory comment into
  `ysrvpxov`, so the CCA Realm test is introduced with the correct performant
  behavior;
- move the Guide timing and diagnostics text into the CCA documentation
  commit.

While rewriting `ysrvpxov`, rename
`boot_linux_direct_qemu_cca` to `boot_linux_direct_cca` and replace its
QEMU-specific capability assertion text with backend-neutral CCA wording.

## Layer 5: FVP CCA Probe and Incubator

### FVP probe host

Consider squashing `uumsztty` and `pnvlllpq`:

```text
local: build FVP CCA probe host
```

This combines the required built-in NIC support with the rootfs/init contract
that consumes it.

### Durable FVP probe

Use three reviewable commits rather than one probe mega-commit.

First, squash `vmztwszu` and `rtqokotw` into the functional probe:

```text
local: test FVP CCA incubator contract
```

It should contain:

- the TCP pipette probe utility;
- multiple loopback port mappings;
- readiness detection;
- basic success and failure cleanup; and
- the durable local Flowey probe entry point.

Second, keep `qrpsqkmt` and `mqlllkyq` together as focused lifecycle
hardening:

```text
local: harden FVP CCA probe lifecycle
```

It should contain:

- durable lock and lease state;
- signal handling;
- stale-container cleanup;
- failure injection; and
- port-collision retry.

Third, keep `pwusootm` as the result/documentation update after the hardened
probe passes.

### Shared host rootfs

Keep `mvumxtkr` standalone:

```text
local: build shared CCA incubator host rootfs
```

Move the executable-bit fix for
`build_support/cca/build-incubator-host-rootfs.sh` into this commit.

### FVP profile, runtime, and Flowey

Keep these boundaries:

1. `oplxzyku` — FVP CCA profile.
2. `unpyvvzs` — FVP CCA runtime.
3. `kkvvurlu` — FVP CCA Flowey wiring.

Move from `kkvvurlu`:

- the FVP parent lock/readiness timeout adjustment into `unpyvvzs`;
- the probe lock-timeout adjustment into the durable FVP probe commit;
- Guide updates into the documentation layer;
- planning results into the FVP plan/documentation change.

The remaining `kkvvurlu` change should focus on:

- FVP platform classification;
- typed local platform resolution;
- artifact staging;
- nextest serialization;
- isolated per-invocation guest shares; and
- generated Flowey pipeline updates.

## Layer 6: Documentation

Keep documentation after the behavior it describes.

Potential consolidation:

- keep the initial QEMU/FVP plans only if they remain useful historical design
  records;
- fold implementation-result updates into the final documentation change;
- keep one backend-neutral CCA incubator Guide page;
- include QEMU and FVP commands, prerequisites, timings, logs, cleanup, and the
  no-serial rationale; and
- omit local planning documents from the eventual upstream KVM PR unless they
  are specifically requested.

## Proposed Final Order

```text
existing SNP commits
membacking: propagate partition unmap failures
virt_kvm: add KVM CCA v15 Realm support
openvmm: add CCA shared device memory aliasing
openvmm: validate CCA configuration limits
virt_kvm: clean CCA backing on RIPAS changes
petri: add CCA isolation support

local: build reproducible CCA v15 host and guest kernels
local: update CCA preflight for KVM v15
local: pin KVM CCA v15 FVP environment
local: validate KVM CCA v15 on FVP

local: build QEMU CCA firmware
local: test QEMU CCA preflight
local: test nested OpenVMM CCA under QEMU
local: add typed QEMU CCA artifacts
local: package QEMU CCA host rootfs
local: stage QEMU CCA platform

incubator: add QEMU CCA profile
incubator: add QEMU CCA runtime
local: flowey: wire QEMU CCA incubator
vmm_tests: boot CCA Realm across incubators

local: build FVP CCA probe host
local: test FVP CCA incubator contract
local: harden FVP CCA probe lifecycle
local: record FVP CCA probe results
local: build shared CCA incubator host rootfs
incubator: add FVP CCA profile
incubator: add FVP CCA runtime
local: flowey: wire FVP CCA incubator

local: document CCA incubator workflows
```

## Current Change Disposition

| Change ID | Disposition |
|---|---|
| `uqkuzspt` | Rebuild directly as the initial v15 CCA Realm implementation and absorb selected core hunks from `motsnxpl`. |
| `nllyypuw` | Keep as shared-device aliasing immediately after initial v15 Realm support. |
| `wrwsoyzo` | Split its comments into the v15 Realm and RIPAS cleanup commits, then abandon the empty change. |
| `yyqznlzw` | Keep as CCA safety/configuration validation and absorb `kkmyyqvk`. |
| `kkmyyqvk` | Squash into `yyqznlzw` and place before RIPAS cleanup. |
| `kkkmmsly` | Squash into `kpnztmku` as one final local migration/results document. |
| `nlzmwtqt` | Squash into `kpnztmku` as one final local migration/results document. |
| `kpnztmku` | Absorb `kkkmmsly` and `nlzmwtqt`; keep locally and omit from the upstream PR. |
| `slytryms` | Rewrite as the final v15 FVP environment pin and absorb `zwonrqvp`. |
| `lrstkruy` | Squash with `kmonmntn`. |
| `kmonmntn` | Squash with `lrstkruy` after the upstreamable CCA layer. |
| `motsnxpl` | Selectively rebuild into `uqkuzspt`; do not retain as a late fixup. |
| `zmqwsqsz` | Keep as local v15 preflight, or fold into the original preflight addition if that tool is submitted. |
| `vuplxutp` | Split into generic unmap fallibility and KVM CCA RIPAS/slot cleanup. |
| `xqqwyoqu` | Squash with `pqxvyyvr`. |
| `pqxvyyvr` | Squash with `xqqwyoqu`. |
| `zwonrqvp` | Fold into `slytryms` to produce one final v15 FVP environment pin. |
| `ylypllms` | Keep as a local QEMU design plan and omit it from the upstream KVM PR. |
| `kmwruwln` | Keep as the QEMU CCA firmware builder. |
| `rnrmmwsw` | Squash with `zyuloswl`. |
| `zyuloswl` | Squash with `rnrmmwsw`. |
| `wutluxts` | Squash with `mxqspmuo`. |
| `mxqspmuo` | Squash with `wutluxts` so the correct CPU shape is present initially. |
| `oylpwmmn` | Fold into the final QEMU documentation/results change. |
| `zqwqvppv` | Keep as typed QEMU/firmware artifacts. |
| `oxwstuyx` | Keep as packaged QEMU CCA host rootfs. |
| `vrmonnnz` | Keep as QEMU CCA platform staging. |
| `wuozulrv` | Keep as QEMU CCA profile. |
| `kkyvzyzn` | Keep as QEMU CCA runtime; absorb any runtime hunks from `qrnmmwuz`. |
| `qrnmmwuz` | Keep only Flowey selection, staging, and target-runner wiring. |
| `lvztnuvn` | Keep as backend-neutral Petri CCA isolation infrastructure. |
| `ysrvpxov` | Keep, rename backend-neutrally, and absorb the test hunk from `xwtsnlzn`. |
| `xrzvywmq` | Move to the final documentation layer and absorb later CCA Guide updates from `kkvvurlu` and `xwtsnlzn`. |
| `wkrkxmmx` | Keep as the local FVP design/results plan, absorb later plan-result updates, and omit it from the upstream KVM PR. |
| `uumsztty` | Squash with `pnvlllpq` as the FVP probe host build. |
| `pnvlllpq` | Squash with `uumsztty` as the FVP probe host build. |
| `vmztwszu` | Squash into `rtqokotw`. |
| `rtqokotw` | Functional FVP probe; absorb `vmztwszu`. |
| `qrpsqkmt` | Squash with `mqlllkyq` as lifecycle hardening. |
| `pwusootm` | Keep after the hardened probe as results/documentation. |
| `mqlllkyq` | Squash with `qrpsqkmt`. |
| `mvumxtkr` | Keep; absorb the rootfs builder executable-bit fix. |
| `oplxzyku` | Keep as FVP CCA profile. |
| `unpyvvzs` | Keep as FVP CCA runtime; absorb the parent lock/readiness timeout fix. |
| `kkvvurlu` | Keep only typed FVP Flowey wiring and generated pipeline updates. |
| `xwtsnlzn` | Split test behavior into `ysrvpxov` and Guide text into documentation. |
| `plan-cca-v15-stack-layering.md` | Keep as local rewrite guidance; do not include in the upstream KVM PR. |

## Per-Commit Verification Matrix

| Rewritten commit | Required verification |
|---|---|
| Generic unmap fallibility | `cargo check`, clippy, docs, and nextest for `membacking`; cross-compile every affected partition backend; test map/unmap RPC failure propagation and teardown failure. |
| Initial v15 CCA Realm support | Check/clippy/docs/nextest for `virt_kvm`, `kvm`, and affected OpenVMM crates; test Realm IPA probing, capability 250/251 handling, VM type encoding, population progress, and an SNP boot/regression using `VmAttributes`. |
| CCA shared aliasing | Check/clippy/docs/nextest for `membacking`, `openvmm_core`, and `virt_kvm`; run a CCA block/network smoke that exercises shared MMIO aliases. |
| CCA configuration safety | Check/clippy/docs/nextest for `openvmm_entry` and `openvmm_core`; negative tests for global and per-NUMA hugetlb, assigned devices or pinned DMA, vhost-user/`GuestMemorySharing`, unsupported firmware, and other uncoordinated memory consumers. |
| CCA RIPAS/slot cleanup | Check/clippy/docs/nextest for `virt_kvm` and `membacking`; focused tests for RIPAS gaps/overlaps, `MADV_REMOVE` fallback, cleanup failure, fatal VP re-entry, slot replacement, and slot unmap. |
| Petri CCA isolation | Check/clippy/docs/nextest for `petri`; confirm generic non-CCA Petri configurations remain unchanged. |
| Reproducible v15 kernels | Build both host and guest kernels twice from the pinned revision and compare manifests/configuration hashes. |
| v15 preflight | Run the probe on the pinned FVP host and confirm capability, Realm VM, and guestmemfd checks. |
| Pinned v15 FVP environment | Run FVP preflight and nested OpenVMM block/network smoke; verify the recorded TF-A, TF-RMM, and Linux revisions. |
| QEMU firmware and proofs | Build firmware online and cache-only; run QEMU preflight and nested smoke with the final CPU shape. |
| Each typed QEMU artifact commit | Run package checks for modified Flowey crates and materialize the typed artifact independently. |
| Each incubator profile/runtime commit | Run incubator unit tests plus a direct `/bin/true` command; verify bounded cleanup on failure. |
| QEMU CCA Flowey and Realm test | Run the QEMU CCA Realm test and a generic AArch64 TCG regression. |
| FVP probe host and functional probe | Boot the host, execute `/bin/true`, and retain host/secure/RMM logs. |
| FVP lifecycle hardening | Exercise readiness timeout, client failure, SIGINT, SIGTERM, port collision, stale-container cleanup, and lock contention. |
| Shared host rootfs | Run packaged preflight under both QEMU and FVP. |
| FVP profile/runtime/Flowey | Run direct runtime smoke, full FVP Realm test, QEMU Realm regression, and generic AArch64 TCG regression. |
| Documentation | Confirm commands, paths, filter names, timings, and log locations match the final generated runner environment. |

## Execution Notes

- Use stable jj change IDs when describing or moving changes.
- Create a rollback bookmark at the current stack tip before rewriting.
- Split `vuplxutp` before performing broad squashes.
- Preserve SNP behavior while rewriting the CCA Realm commit to v15.
- Rebase and validate after each rewritten commit rather than rewriting the
  entire stack in one operation.
- Verify that the upstreamable range contains no Flowey CCA pins, local
  kernel/FVP/QEMU paths, incubator profiles, or platform scripts.
- Give the generic unmap-fallibility commit cross-platform compile coverage.
- Add focused coverage for RIPAS gaps and overlaps, cleanup failures, fatal VP
  re-entry, slot replacement/unmap, and global/per-NUMA hugetlb rejection.
- After rewriting the upstreamable layer, run SNP and CCA package validation
  before moving local or incubator commits.
- After rewriting each incubator layer, rerun the corresponding QEMU or FVP
  Realm test.

## Review

**Verdict: Minor revisions — addressed.**

The review required:

- inventorying the complete cross-platform unmap API before splitting
  `vuplxutp`;
- rebuilding the initial CCA change directly against v15 rather than blindly
  squashing a late migration;
- landing shared aliasing and cleanup-critical configuration restrictions
  before RIPAS hole punching;
- separating backend-neutral Petri CCA support from emulator-specific layers;
- retaining reviewable FVP probe and lifecycle-hardening commits;
- assigning every current change an explicit disposition;
- renaming the shared Realm test and its assertion text backend-neutrally;
- creating a rollback bookmark before rewriting; and
- defining per-commit validation, including SNP regression and negative DMA,
  pinning, vhost-user, and hugetlb tests.

The plan now incorporates these requirements.
