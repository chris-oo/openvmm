# FVP CCA Incubator Plan

## Status

Planning document for adding an FVP-backed CCA incubator alongside the existing
QEMU CCA incubator.

This plan is intentionally separate from `todo-qemu-cca-support.md`. The QEMU
path is working end to end; this work should reuse its incubator contract
without coupling FVP lifecycle and networking details into the QEMU backend.

## Goal

Allow a normal AArch64 `vmm_test` to run inside an FVP-hosted Linux/KVM CCA
environment:

1. Shrinkwrap launches the Arm FVP and boots an L1 Linux/KVM CCA host.
2. The L1 host runs `kvm_cca_preflight`.
3. The L1 host starts pipette and publishes the `cca` capability.
4. The incubator runs an AArch64 VMM-test binary through pipette.
5. OpenVMM launches an L2 CCA Realm.
6. The test verifies the Realm agent and clean teardown.

The result should coexist with:

- the QEMU CCA incubator;
- the existing FVP/TMK `cca_runtime` test; and
- ordinary AArch64 QEMU TCG incubator tests.

## Non-goals

- Replacing FVP with QEMU for architecture behavior that QEMU does not model.
- Making FVP a required PR gate in the initial implementation.
- Publishing or redistributing proprietary FVP binaries.
- Changing the CCA guestmemfd backing model.
- Adding FVP support to non-CCA incubator profiles.
- Replacing the existing FVP/TMK `cca_runtime` test before equivalent coverage
  is demonstrated.
- Inventing a serial pipette transport unless the TCP approaches are proven
  infeasible.

## Proven Current State

### Incubator

- `petri/incubator/src/profile.rs` defines `QemuTcg` and `QemuCca` backends.
- `petri/incubator/src/run.rs` provides separate public runtime configurations
  for generic QEMU TCG and QEMU CCA.
- `petri/incubator/src/path_mapping.rs` maps host paths into the shared guest
  tree independently of the QEMU command builder.
- The QEMU CCA backend waits for preflight-backed `PIPETTE READY`, retains
  named console logs, publishes `cca`, runs the target command, and enforces
  bounded process-tree teardown.
- `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs` selects an incubator
  platform by parsing the profile backend type before the Flowey graph is
  emitted.
- `flowey/flowey_lib_hvlite/src/write_incubator_target_runner.rs` supplies the
  incubator through Cargo's target-runner environment.

### Existing FVP CCA path

- `flowey/flowey_lib_hvlite/src/_jobs/local_stage_kvm_cca.rs` stages a CCA host
  rootfs, host/guest kernels, preflight, OpenVMM, launch scripts, and a 9p
  share.
- The same job launches:

  ```text
  shrinkwrap run --overlay kvm_cca_planes.yaml cca-3world.yaml
  ```

  with `ROOTFS`, `KERNEL`, and `SHARE` runtime variables.
- `vmm_tests/vmm_tests/tests/cca.rs` already launches Shrinkwrap as a child,
  captures stdin/stdout/stderr, waits for console markers, writes logs, and
  enforces a 20-minute overall timeout.
- Shrinkwrap does not reliably wait for descendant FVP processes. The current
  test uses `pgrep -f <rootfs>` and explicit signals to remove leftovers.
- The FVP base configuration enables user networking and maps a host port to
  guest SSH through `bp.hostbridge.userNetPorts`.
- The current FVP path is console-driven and does not implement the incubator
  pipette contract.

### Normal CCA VMM test

- `vmm_tests/vmm_tests/tests/tests/aarch64_exclusive.rs` contains the working
  QEMU-backed CCA Realm test.
- The test uses `requires(cca)`, VM-level `IsolationType::Cca`, device-tree
  Linux direct boot, one VP, 256 MiB, no VMBus, and virtio-vsock.
- The L2 test behavior is not inherently QEMU-specific. The backend-specific
  part is the incubator that provides the L1 CCA host.

## Recommended Architecture

Add a separate `FvpCca` incubator backend that implements the same high-level
contract as `QemuCca`, while keeping launch, networking, and cleanup
FVP-specific.

Shared incubator code should own:

- host/guest path mapping;
- command and environment forwarding;
- pipette connection and command execution;
- capability merging;
- output directory conventions;
- readiness timeout policy; and
- common result reporting.

The FVP backend should own:

- Shrinkwrap command construction;
- Shrinkwrap environment and configuration paths;
- FVP runtime variables and overlays;
- primary console marker detection;
- FVP network forwarding;
- descendant process discovery and termination; and
- FVP-specific logs.

Do not make `QemuCca` accept FVP-specific fields or special cases.

## Trust and Safety Constraints

- Do not expose pipette on non-loopback host interfaces.
- Do not interpolate unvalidated profile strings into a shell command.
- Construct Shrinkwrap commands with `std::process::Command` arguments.
- Treat all paths as untrusted runtime inputs and validate required file types.
- Use unique per-run rootfs copies, logs, ports, and temporary directories.
- Never kill processes by a broad name match.
- Process discovery must be scoped to a unique per-run token or artifact path.
- Bound boot, readiness, command, poweroff, and forced-teardown waits.
- Ensure cleanup runs on every error path and after interrupted pipette
  sessions.
- Preserve console output and rootfs logs on failure.
- Profiles must not contain machine-local absolute paths.

## Phase 0: Focused FVP Incubator Probe

The probe must be completed before adding a production `FvpCca` backend.

### Verified probe findings

- [x] FVP accepts multiple comma-separated user-network mappings.
- [x] Address-qualified mappings bind only to loopback:

  ```text
  127.0.0.1:<SSH_PORT>=22,127.0.0.1:<PIPETTE_PORT>=4919
  ```

- [x] The FVP user network uses `172.20.51.0/24`; the L1 received
      `172.20.51.1` with gateway `172.20.51.254` through DHCP.
- [x] Shrinkwrap supports terminal-specific host log files.
- [x] The shared host kernel required built-in `CONFIG_SMC91X`; the packaged
      rootfs contains no loadable kernel modules.
- [x] The FVP-specific probe init mounted the `FM` 9p share, retained DHCP,
      passed preflight, and emitted `PIPETTE READY`.
- [x] The host probe client executed `/bin/true` through pipette and requested
      L1 poweroff.
- [x] FVP and the forwarded listeners exited after poweroff.
- [x] Shrinkwrap left an idle Docker container after successful FVP exit; the
      durable probe now labels, identifies, and removes that exact container.
- [x] A forced client failure returned nonzero, wrote `status=failed`, and
      removed the labeled container and FVP process.
- [x] `cargo xflowey fvp-cca-incubator-probe --fvp-platform-root
      target/cca-test` passes and emits the planned manifest, generated overlay,
      FVP argv, status, terminal logs, and pipette command output.

Remaining Phase 0 hardening:

- [x] Add explicit port-bind collision retry.
- [x] Add readiness-timeout and SIGTERM failure-injection runs.
- [x] Add a SIGINT failure-injection run.
- [x] Add durable `reserved`, `container-started`, and `cleanup` lease-state
      manifest transitions in addition to the stable platform lock and Docker
      labels.

### Purpose

Prove the missing runtime contracts independently of the normal VMM-test flow:

- packaged host-rootfs compatibility;
- 9p share compatibility;
- FVP L1 network configuration;
- dynamic host-to-guest pipette TCP forwarding;
- preflight-backed readiness;
- host-to-pipette command execution;
- log collection; and
- deterministic cleanup.

### Probe implementation

- [ ] Add a durable `fvp-cca-incubator-probe` local Flowey pipeline. Keep it
      after backend implementation as a platform diagnostic rather than
      creating a temporary script that must later be removed.
- [ ] Require an explicit FVP platform root:

  ```text
  --fvp-platform-root <PATH>
  OPENVMM_FVP_CCA_PLATFORM_ROOT=<PATH>
  ```

  The root is the `test_root` previously populated by
  `local_install_cca_emu` and must contain the Shrinkwrap virtual environment,
  configuration, overlay, Buildroot image, and user-provisioned licensed FVP
  model state.
- [ ] Do not silently search `$HOME`, sibling directories, or unrelated
      `target/` trees for an FVP installation.
- [ ] Reuse the typed v15 host kernel from
      `build_cca_linux_kernels`.
- [ ] Reuse the packaged CCA host rootfs builder, but do not assume the
      QEMU-specific network settings work under FVP.
- [ ] For the earliest network probe, permit a probe-specific init script or
      rootfs injection that configures FVP networking without changing the
      shared QEMU init.
- [ ] Treat that probe image as disposable and record its script hash.
- [ ] Before Phase 0 is declared complete, move the proven network contract
      into the typed/shared init from Phase 1 and rerun both QEMU and FVP
      acceptance.
- [ ] Stage static AArch64-musl `pipette` and `kvm_cca_preflight` in the FVP
      share.
- [ ] Launch Shrinkwrap with a unique per-run rootfs and share.
- [ ] Record the complete successful launch contract in a probe manifest:
  - working directory;
  - Shrinkwrap executable and version;
  - virtual environment and `PATH`;
  - `SHRINKWRAP_CONFIG`;
  - `--runtime=docker`;
  - pinned `SHRINKWRAP_IMAGE`;
  - overlay paths;
  - runtime variables;
  - effective merged Shrinkwrap configuration;
  - generated FVP argv; and
  - Docker image and container identity.
- [ ] Reserve a loopback host TCP port for pipette.
- [ ] Generate a unique per-run overlay that preserves SSH forwarding and adds
      a pipette forwarding rule without mutating shared configuration.
- [ ] Configure FVP user networking to forward that port to
      `pipette_client::PIPETTE_PORT` and prove the host listener is loopback
      only.
- [ ] Release the port reservation only immediately before FVP/container bind.
- [ ] Detect the specific bind-collision diagnostic and retry with a new port
      and generated overlay.
- [ ] Configure terminal-specific file or pipe sinks in the generated overlay.
- [ ] Map Shrinkwrap/FVP terminal identifiers to stable profile console names.
- [ ] Capture Shrinkwrap stdout/stderr separately from every FVP console.
- [ ] Wait for `PIPETTE READY` only from the proven L1 primary-console sink,
      not multiplexed Shrinkwrap stdout.
- [ ] Connect with `pipette_client` and execute `/bin/true`.
- [ ] Request L1 poweroff through pipette if supported.
- [ ] Stop Shrinkwrap and all descendant FVP processes.
- [ ] Track the Docker container ID or unique runtime label and stop/remove that
      container explicitly.
- [ ] Acquire an exclusive cross-process lease for the licensed FVP model
      before launching Shrinkwrap and hold it until all containers/processes
      are gone.
- [ ] Store the lock under the explicit FVP platform root, for example:

  ```text
  <FVP_PLATFORM_ROOT>/.openvmm-fvp-cca.lock
  ```

- [ ] Keep the advisory-lock file and lease-state manifest separate:

  ```text
  <FVP_PLATFORM_ROOT>/.openvmm-fvp-cca.lock
  <FVP_PLATFORM_ROOT>/.openvmm-fvp-cca-lease.json
  ```

  The lock file must retain a stable inode for the lifetime of all users. Never
  atomically replace or rename the locked file. Apply durable atomic
  publication only to the separate lease-state manifest.

- [ ] Default to serializing FVP runs. Do not assume that unique ports,
      rootfses, and logs make the licensed model safe for concurrent use.
- [ ] Before accepting the lease as sufficient, reconcile stale runtime state:
  - list containers carrying the OpenVMM FVP CCA label for this platform root;
  - identify whether their recorded owner process still exists;
  - stop/remove only containers whose label, platform-root hash, and run
    manifest prove they belong to this platform;
  - check for remaining FVP processes carrying the proven unique run token;
  - fail closed if ownership cannot be established.
- [ ] Perform stale-state reconciliation before every launch, including after
      acquiring an OS-released lock following a crash.
- [ ] Store owner PID, process start identity, container ID/label, platform-root
      hash, and run ID in an atomic lease manifest.
- [ ] Consider a small independent lease supervisor if Docker/container
      lifecycle cannot be made crash-safe from the target-runner process.
- [ ] Verify no process associated with the unique run remains.
- [ ] Preserve logs and the failed run's identifying paths.
- [ ] Exercise cleanup after success, boot failure, readiness timeout, pipette
      disconnect, SIGINT, and SIGTERM.
- [ ] Install signal handling so SIGINT/SIGTERM initiates the same bounded
      cleanup path; do not rely only on Rust destructors.

### Network probe matrix

Test these approaches in order.

#### Option A: FVP user-mode TCP forwarding

Preferred approach.

- Add a second entry to `bp.hostbridge.userNetPorts`, preserving the SSH
  mapping.
- Forward a dynamically reserved loopback host port to the guest pipette TCP
  port.
- Determine whether the FVP user network uses the same `10.0.2.x` guest
  addressing as QEMU or requires DHCP/different static values.
- Capture the effective merged Shrinkwrap configuration and generated FVP argv
  to prove the exact multi-port syntax.
- Prove whether FVP or the Docker runtime can restrict the published host
  endpoint to `127.0.0.1`.
- Generalize the host init to accept network settings through kernel command
  line fields or backend-specific environment/configuration.

Advantages:

- Matches the existing incubator pipette TCP model.
- Requires no new pipette protocol.
- Keeps host command execution independent of the FVP console.

Risks:

- Exact multi-port `bp.hostbridge.userNetPorts` syntax is not yet proven.
- Shrinkwrap may need an overlay or new runtime variable to express the
  dynamic mapping.

#### Option B: SSH bootstrap, then pipette TCP

Use only if direct pipette forwarding cannot be expressed cleanly.

- Use the existing forwarded SSH port to reach the L1 host.
- Start or verify pipette through SSH.
- Establish a TCP forwarding path for pipette.

Advantages:

- Reuses the existing proven SSH mapping.
- Useful for diagnosing L1 network and init failures.

Disadvantages:

- Adds SSH keys, login state, and another control protocol.
- Duplicates work already handled by the init service.
- Is less suitable for concurrent automated test runs.

#### Option C: Serial pipette transport

Do not pursue unless both TCP approaches are infeasible.

This would require a new pipette transport and framing protocol. It would be a
cross-cutting change to `pipette`, `pipette_client`, and incubator runtime code.

### Phase 0 success criteria

All of the following must pass in one noninteractive command:

- [ ] FVP boots the pinned v15 L1 host.
- [ ] The host mounts the requested 9p share.
- [ ] L1 networking reaches the forwarded pipette path.
- [ ] `kvm_cca_preflight` exits zero.
- [ ] The primary host console emits `PIPETTE READY`.
- [ ] The host connects to pipette through a loopback-only port.
- [ ] `/bin/true` exits zero through pipette.
- [ ] The L1 host and Shrinkwrap/FVP processes terminate within bounded time.
- [ ] No descendant process remains.
- [ ] Console and preflight logs are retained.
- [ ] The same probe succeeds after replacing any probe-specific init with the
      shared typed init contract.
- [ ] A second concurrent probe blocks on or fails cleanly against the FVP
      lease without launching another model instance.

### Stop conditions

Stop and update this plan before backend implementation if:

- FVP cannot forward an arbitrary pipette TCP port;
- FVP networking requires privileged host setup;
- the packaged host rootfs cannot support both QEMU and FVP cleanly;
- deterministic descendant cleanup cannot be implemented without broad process
  matching; or
- the only viable transport requires a new pipette protocol.
- Shrinkwrap cannot route the L1 primary console to a distinct sink;
- the Docker container and all FVP descendants cannot be identified by a
  unique per-run container ID, label, process group, or run token; or
- the pipette endpoint cannot be restricted to loopback.

### Failure-injection validation

The durable probe must expose noninteractive failure modes for:

- invalid host kernel;
- missing rootfs;
- readiness marker never emitted;
- pipette connection dropped;
- guest command timeout;
- poweroff timeout;
- Shrinkwrap ignoring TERM; and
- a child process surviving Shrinkwrap exit.

Each mode must produce logs, return nonzero, and leave no associated container
or process.

### Exact probe command

The first commit must provide this command:

```bash
cargo xflowey fvp-cca-incubator-probe \
  --fvp-platform-root target/cca-test \
  --output-root target/cca-fvp-incubator-probe
```

Expected outputs:

```text
target/cca-fvp-incubator-probe/
├── manifest.txt
├── effective-shrinkwrap.yaml
├── fvp-argv.txt
├── logs/
└── status.txt
```

## Phase 1: Generalize the CCA Host Rootfs Contract

Phase 0 may use a disposable probe-specific init to discover the FVP network
contract. Phase 1 is required before final Phase 0 acceptance and before the
production incubator backend.

### Phase 1 result

- [x] QEMU and FVP use one `incubator-host-init.sh`.
- [x] QEMU selects `mount_tag=host` and `network=qemu-static`.
- [x] FVP selects `mount_tag=FM` and `network=dhcp` through Shrinkwrap
      `CMDLINE`.
- [x] Pipette and preflight paths remain typed `/share/...` arguments.
- [x] The shared rootfs manifest records `init_contract=1`.
- [x] Flowey exposes `build_cca_incubator_host_rootfs`.
- [x] The durable FVP probe passes with the shared image.
- [x] The normal nested QEMU CCA Realm test passes with the shared image.

### Init configuration

- [ ] Rename QEMU-specific host init/build concepts where they become shared.
- [ ] Keep compatibility wrappers if existing scripts depend on current names.
- [ ] Pass these values through kernel command line or another typed backend
      contract:
  - 9p mount tag;
  - pipette path;
  - preflight path;
  - network configuration mode;
  - static guest address, gateway, and DNS when applicable.
- [ ] Support `dhcp` and `static` network modes if the Phase 0 probe requires
      both.
- [ ] Inventory the packaged rootfs before choosing DHCP:
  - available DHCP client;
  - network interface names;
  - `ip`/route tools;
  - DNS configuration support; and
  - service ordering.
- [ ] Use a restrictive encoding for command-line values. Reject whitespace,
      separators, control characters, duplicate fields, and unknown modes.
- [ ] Reject missing or invalid configuration with console diagnostics.
- [ ] Emit `PIPETTE READY` only after preflight succeeds.
- [ ] Keep pipette and preflight outside the rootfs image so each run uses
      freshly-built binaries.
- [ ] Extract `/cca/logs` after every FVP run using the proven `debugfs`
      extraction pattern in
      `flowey/flowey_lib_hvlite/src/_jobs/local_stage_kvm_cca.rs`.

### Typed artifact

- [x] Generalize `build_cca_qemu_host_rootfs` into the shared
      `build_cca_incubator_host_rootfs` artifact.
- [ ] Preserve deterministic injection, manifest hashes, per-output locking,
      managed-output checks, and atomic publication.
- [ ] Record the init contract/version in the manifest.
- [ ] Validate the resulting image independently under QEMU and FVP.
- [ ] Re-run QEMU static networking before and after every shared-init change.

### Commit boundary

Commit the shared host-rootfs/init generalization separately after:

- QEMU packaged preflight passes;
- QEMU CCA `vmm_test` passes; and
- the FVP Phase 0 probe passes.

## Phase 2: Add the FVP CCA Incubator Backend

### Phase 2 result

- [x] Added the strict portable `FvpCca` profile.
- [x] Added backend-specific launcher/platform/kernel/rootfs runtime inputs.
- [x] Reused Rust incubator path mapping, guest environment, current directory,
      capability publishing, command execution, and poweroff.
- [x] Reused the durable Python FVP lifecycle helper in launch-only mode.
- [x] Added unique run directories and endpoint run-ID validation.
- [x] Passed profile console names, primary console, and guest pipette/preflight
      paths into the generated FVP contract.
- [x] Added Rust-owned host-port collision retry.
- [x] `/bin/true`, `PETRI_CAPABILITIES=cca`, custom pipette paths, and nonzero
      command cleanup pass.
- [x] Generic QEMU TCG and QEMU CCA incubator smoke commands remain passing.

### Profile

- [ ] Add `IncubatorBackend::FvpCca(FvpCcaConfig)` in
      `petri/incubator/src/profile.rs`.
- [ ] Fix the architecture to AArch64.
- [ ] Add `petri/incubator/profiles/aarch64-fvp-cca.toml`.
- [ ] Keep machine-local paths out of the profile.
- [ ] Model only portable behavior:
  - primary console name;
  - console names;
  - capabilities;
  - network mode/guest port policy if portable; and
  - narrowly typed FVP overrides proven necessary by Phase 0.
- [ ] Reject extra devices until their FVP/CCA behavior is explicitly
      supported.
- [ ] Validate capability names through the canonical Petri capability set.

### Runtime inputs

Add a dedicated `FvpCcaIncubatorConfig` containing typed paths for:

- Shrinkwrap executable;
- Shrinkwrap virtual environment;
- Shrinkwrap configuration directory;
- CCA overlay;
- host kernel;
- host rootfs;
- share directory;
- output directory; and
- optional packaged FVP configuration/artifact roots if not implied by
  Shrinkwrap.
- Docker runtime name;
- pinned Shrinkwrap image;
- runtime/container label prefix; and
- per-run generated overlay directory.

Do not overload `QemuCcaIncubatorConfig`.

### Runtime behavior

- [ ] Build the Shrinkwrap command without a shell.
- [ ] Reproduce the complete Phase 0 argv, cwd, and environment contract,
      including `--runtime=docker`, `--image`, `VIRTUAL_ENV`, `PATH`, and
      `SHRINKWRAP_CONFIG`.
- [ ] Allocate a unique run identifier and writable rootfs copy.
- [ ] Allocate required loopback ports.
- [ ] Pass unique `ROOTFS`, `KERNEL`, `SHARE`, and network runtime variables.
- [ ] Capture stdin/stdout/stderr.
- [ ] Retain every configured console log.
- [ ] Monitor only the primary host console for readiness.
- [ ] Race readiness against process exit and a bounded timeout.
- [ ] Connect pipette using the Phase 0-proven transport.
- [ ] Publish `cca` only after preflight-backed readiness.
- [ ] Run the Cargo target-runner command through pipette.
- [ ] Power off the L1 host where supported.
- [ ] Stop Shrinkwrap and all descendants.
- [ ] Verify cleanup before returning success.

### Timeout contract

Define separate configurable/bounded timeouts for:

- FVP boot;
- preflight/readiness;
- pipette TCP connection;
- target command execution;
- pipette poweroff request;
- graceful L1/FVP exit;
- TERM cleanup; and
- KILL cleanup.

The default target-command timeout may be longer than the boot timeout, but it
must not be infinite. Every timeout must identify its phase in the returned
error and preserved manifest.

### Process lifecycle

The current `pgrep -f <rootfs>` logic in
`vmm_tests/vmm_tests/tests/cca.rs` is useful evidence but should not be copied
unchanged into the incubator.

Preferred cleanup order:

1. Launch Docker/Shrinkwrap with a unique container label and record the
   container ID as soon as it is available.
2. Place local Shrinkwrap helpers in a dedicated process group.
3. Ask pipette to power off the host.
4. Wait for Shrinkwrap/FVP exit for a bounded interval.
5. Stop the identified container through the selected Docker-compatible
   runtime.
6. Signal the local process group.
7. Discover only descendants or processes carrying the proven unique run
   token.
8. Escalate container/process cleanup from TERM to KILL after bounded waits.
9. Cancel console relays if the process/container cannot be reaped.
10. Return an error if any associated process or container remains.

Linux and WSL behavior must both be tested.

A process group alone is not sufficient because FVP executes in a Docker
container and descendants may be reparented.

### Tests

- [ ] Parse the FVP profile.
- [ ] Reject machine-local profile paths.
- [ ] Test Shrinkwrap argument construction.
- [ ] Test console selection and readiness marker scanning.
- [ ] Test network runtime-variable generation.
- [ ] Test capability merging.
- [ ] Test timeout and forced cleanup using controllable child processes.
- [ ] Test SIGINT and SIGTERM cleanup.
- [ ] Test Docker container identification and removal through a fake or
      controllable runtime before relying on the licensed model.
- [ ] Test unique run paths and concurrent launch isolation.

### Commit boundary

Commit the profile/runtime backend after unit tests and the `/bin/true` probe
pass through the actual incubator API.

## Phase 3: Typed Flowey Artifact Wiring

### Implemented results

- [x] Added `FvpCca` platform classification and an explicit canonical
      `--fvp-platform-root` / `OPENVMM_FVP_CCA_PLATFORM_ROOT` input.
- [x] Added a typed local platform resolver for the selected Shrinkwrap
      executable, CCA overlay, and Buildroot source image. Hashing and broader
      platform metadata remain follow-up work.
- [x] Reused the typed v15 host kernel, shared host rootfs, preflight, pipette,
      OpenVMM, incubator, and nextest archive builds.
- [x] Added FVP-specific target-runner environment generation.
- [x] Added nextest thread-count plumbing and serialized FVP execution with
      `--test-threads 1`.
- [x] Retained the backend-owned platform lock, durable lease state, and exact
      platform-label container cleanup for independent xflowey invocations.
      Full ambiguous-owner and leftover-process reconciliation remains
      incomplete.
- [x] Use a unique per-xflowey guest share containing only staged runtime
      artifacts, extracted test binaries, and test output. The repository,
      host-side nextest archive/configuration, and licensed FVP platform root
      are not shared with the guest.
- [x] Extended the parent readiness deadline to include bounded lock waiting.
- [x] The existing capability-gated, QEMU-named CCA Realm test passes through
      FVP Flowey and QEMU CCA, and the generic AArch64 TCG regression remains
      passing. Backend-neutral test naming remains Phase 4 work.

### Platform selection

- [x] Extend
      `flowey_lib_hvlite::write_incubator_target_runner::IncubatorPlatform`
      with `FvpCca`.
- [x] Classify `type = "fvp-cca"` by parsing the profile at graph construction
      time.
- [x] Keep QEMU TCG and QEMU CCA resolution unchanged.
- [x] Update `--cca-kernel-src` validation in
      `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs` so it is accepted
      by both `QemuCca` and `FvpCca`.
- [x] Update CLI error text and target checks for an AArch64-only FVP backend.
- [x] Add `--fvp-platform-root` with
      `OPENVMM_FVP_CCA_PLATFORM_ROOT` fallback.
- [x] Require this input only for `FvpCca`; reject it for QEMU profiles.
- [x] Resolve relative values against the OpenVMM repository root.
- [x] Validate and canonicalize the directory before emitting the Flowey graph.

### Typed FVP platform artifacts

Add a typed resolver/staging node that reports:

- Shrinkwrap executable;
- virtual environment;
- configuration directory;
- KVM CCA overlay;
- required FVP package/configuration state; and
- a manifest describing the installed Shrinkwrap/FVP environment.
- Docker-compatible runtime;
- pinned Shrinkwrap image; and
- whether the required licensed model is available.

The node wraps the installation selected by the explicit FVP platform root and
must:

- validate every reported path;
- fail with actionable setup instructions;
- avoid embedding a developer home-directory layout in profiles;
- serialize installation/update operations; and
- remain local-only while model acquisition is not CI-ready.
- require a user-provisioned licensed FVP model;
- never download, copy into an archive, publish, or redistribute the FVP
  model; and
- keep model paths out of test-result artifacts and public manifests.
- consume the explicit FVP platform root rather than inventing its own
  installation location.

Flowey may install or build open-source Shrinkwrap tooling. It must not automate
licensed FVP acquisition without an approved licensing/distribution design.

### Rootfs source artifact

Do not continue the `$HOME/.shrinkwrap/...` lookup used by current local
tooling.

- [x] Add a typed FVP CCA host-rootfs source output to the local Shrinkwrap/FVP
      platform resolver.
- [ ] Report the exact Buildroot image path and its hash.
- [x] Feed that `ReadVar<PathBuf>` into the generalized host-rootfs builder.
- [x] Validate that the source belongs to the selected installed platform.

### FVP execution lease

- [x] Add a typed lease/lock node or backend-owned cross-process lock rooted at
      `<FVP_PLATFORM_ROOT>/.openvmm-fvp-cca.lock`.
- [x] Hold the lock across Shrinkwrap launch, pipette execution, poweroff,
      container removal, process cleanup, and log extraction.
- [x] Use a bounded lock-acquisition timeout.
- [ ] Add a test proving two target-runner invocations cannot launch FVP
      concurrently.
- [ ] Do not enable parallel FVP nextest execution unless multiple concurrent
      licensed-model runs are separately proven safe.
- [x] Configure the FVP CCA `vmm-tests-run` invocation itself with one nextest
      test thread (for example, the equivalent of `--test-threads 1`) so
      target-runner processes are not created concurrently and left waiting on
      the lease.
- [x] Add a thread-count option through the existing execution chain:
  - `test_nextest_vmm_tests_archive::Request`;
  - both `run_cargo_nextest_run` request layers; and
  - `gen_cargo_nextest_run_cmd`.
- [x] Set that option only for `IncubatorPlatform::FvpCca`; preserve current
      concurrency for QEMU and non-incubator test runs.
- [x] Keep the lease as defense in depth for separate xflowey invocations.
- [ ] Test both same-nextest-run serialization and two independent xflowey
      processes.

### Crash-safe stale-state reconciliation

An advisory lock is not sufficient after a crash because the OS releases the
lock while a Docker container or reparented FVP process may remain.

Before each launch:

1. Acquire the platform lock.
2. Read the atomic lease manifest, if present.
3. Query the selected Docker-compatible runtime by the exact OpenVMM label.
4. Compare container label, platform-root hash, run ID, and recorded owner
   identity.
5. Stop/remove stale containers that are proven to belong to this platform.
6. Reap proven leftover local processes.
7. Refuse to launch if any process/container cannot be attributed safely.
8. Atomically publish the new lease manifest.

After cleanup, remove the lease manifest only after container/process absence
has been verified.

Publish every lease-manifest transition durably:

1. Write a same-filesystem temporary file.
2. Flush the file and call `fsync`.
3. Atomically rename it over the manifest.
4. Call `fsync` on the parent directory.

Use explicit states:

- `reserved`: lock acquired and run identity allocated, before container
  launch;
- `container-started`: container ID and runtime label recorded;
- `cleanup`: shutdown/termination has begun; and
- manifest removed only after verified cleanup.

This closes the crash window between starting a container and recording its
identity.

### Other artifacts

Reuse:

- `build_cca_linux_kernels` for the L1 host kernel;
- the generalized CCA incubator host rootfs;
- `build_kvm_cca_preflight`;
- `build_pipette`;
- `build_incubator`; and
- the existing test archive and shared path mapping.

### Runner environment

Extend `write_incubator_target_runner::Request` with FVP-specific typed inputs.
Suggested environment variables:

- `INCUBATOR_SHRINKWRAP`;
- `INCUBATOR_SHRINKWRAP_VENV`;
- `INCUBATOR_SHRINKWRAP_CONFIG`;
- `INCUBATOR_FVP_OVERLAY`;
- `INCUBATOR_FVP_PLATFORM_ROOT`;
- existing `INCUBATOR_KERNEL`;
- existing `INCUBATOR_ROOTFS`; and
- existing `INCUBATOR_SHARE`.

Only set these variables for `FvpCca`.

### End-to-end platform-root plumbing

Implement the platform root through this exact chain:

1. `VmmTestsRunCli` in
   `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs` accepts
   `--fvp-platform-root`; if omitted, xflowey reads
   `OPENVMM_FVP_CCA_PLATFORM_ROOT`.
2. The CLI resolves the path to an absolute canonical directory before any
   hashing, locking, manifest identity, or child-path resolution. Symlink and
   relative aliases must produce the same platform identity.
3. The CLI validates the canonical path, then passes it in
   `local_build_and_run_nextest_vmm_tests::Params`.
4. The local job passes the path to the typed FVP platform resolver.
5. The resolver derives Shrinkwrap, venv, config, overlay, Buildroot source,
   runtime/image metadata, and licensed-model validation exclusively from that
   root.
6. The local job passes the resolved root/artifacts to
   `write_incubator_target_runner::Request`.
7. The target-runner environment exports
   `INCUBATOR_FVP_PLATFORM_ROOT` plus the resolved FVP artifact variables.
8. `petri/incubator/src/main.rs` consumes those variables only for
   `IncubatorBackend::FvpCca`.

`OPENVMM_FVP_CCA_PLATFORM_ROOT` is an xflowey configuration fallback;
`INCUBATOR_FVP_PLATFORM_ROOT` is the generated target-runner runtime input.

### Exact code surfaces

The Flowey/runtime implementation must enumerate and update:

- `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs`
  - profile classification;
  - `--cca-kernel-src`;
  - AArch64 target validation.
- `flowey/flowey_lib_hvlite/src/write_incubator_target_runner.rs`
  - `IncubatorPlatform::FvpCca`;
  - FVP runner environment;
  - unit tests.
- `flowey/flowey_lib_hvlite/src/_jobs/local_build_and_run_nextest_vmm_tests.rs`
  - node imports;
  - platform match;
  - kernel/rootfs/preflight/pipette staging;
  - FVP artifact requests.
- `petri/incubator/src/profile.rs`
  - backend/profile schema and validation.
- `petri/incubator/src/main.rs`
  - FVP-specific CLI/environment inputs and dispatch.
- `petri/incubator/src/lib.rs`
  - public FVP runtime config/export.
- `petri/incubator/src/run.rs`
  - shared command execution and FVP dispatch boundaries.
- a new FVP-specific runtime module rather than adding FVP command construction
  to `qemu.rs`.

### Local command

The intended command should be:

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca.toml \
  --fvp-platform-root target/cca-test \
  --filter 'test(qemu_cca) & !binary(cca)' \
  --skip-vhd-prompt
```

The final filter depends on the test naming decision below.

### Commit boundary

Commit Flowey wiring after:

- the durable standalone probe executes `/bin/true`;
- the existing capability-gated CCA Realm test runs through
  `cargo xflowey vmm-tests-run` with the FVP profile;
- the same test remains passing with the QEMU CCA profile; and
- an existing generic AArch64 TCG test remains passing.

## Phase 4: Share the Normal CCA Realm Test

### Naming decision

Preferred: rename `boot_linux_direct_qemu_cca` to
`boot_linux_direct_cca`.

Rationale:

- the L2 test body validates OpenVMM CCA, not the L1 emulator;
- backend selection already comes from the incubator profile;
- one shared test avoids duplicate Realm boot coverage; and
- `requires(cca)` is the correct runtime gate.

If backend-specific scheduling requires distinct test names, extract a shared
implementation function and use explicit backend-specific capabilities such as
`qemu_cca` and `fvp_cca`. Do not add two wrappers that both require only `cca`,
because both would run under either backend.

If backend-specific capabilities are selected, add them to
`petri_artifacts_common::capabilities::KNOWN_CAPABILITIES`, profile validation,
and the documented filter matrix before any profile advertises them.

### Test behavior

The shared test must continue to require:

- VM-level `IsolationType::Cca`;
- one VP;
- explicit small memory;
- device-tree Linux direct boot;
- no Hyper-V enlightenments;
- no VMBus;
- a static non-hotplug PCIe root port;
- virtio-vsock Realm agent transport;
- agent ping;
- guest poweroff; and
- clean OpenVMM teardown.

### Validation matrix

- [ ] Rename/generalize the QEMU-named CCA test.
- [x] Existing capability-gated CCA test passes under QEMU CCA.
- [x] Existing capability-gated CCA test passes under FVP CCA.
- [ ] Shared test skips under generic QEMU TCG.
- [x] Existing `test(aarch64_tcg)` tests pass.
- [ ] Existing FVP/TMK `cca_runtime` test passes.
- [ ] Existing SNP compile/regression gate remains unchanged.

### Commit boundary

Commit test renaming/generalization separately from the FVP backend so test
selection changes are easy to review.

## Phase 5: Documentation and Cleanup

- [ ] Update `Guide/src/dev_guide/tests/vmm/qemu_cca.md` or rename it to a
      backend-neutral CCA incubator page.
- [ ] Document the FVP model/Shrinkwrap prerequisites without implying that
      proprietary artifacts are redistributed.
- [ ] Document the FVP command, expected runtime, logs, and cleanup guidance.
- [ ] Update `Guide/src/SUMMARY.md`.
- [ ] Update the guide-maintenance code-to-doc mapping.
- [ ] Update `todo-qemu-cca-support.md` to reference this plan.
- [ ] Remove duplicated FVP rootfs staging and process-control helpers where
      the new typed backend supersedes them.
- [ ] Keep the temporary Guide cleanup warning until the local prerequisites,
      naming, and artifact setup are stable.

## Performance and CI Position

### Initial position

FVP incubator support should be local-only.

Reasons:

- FVP model acquisition/licensing is environment-dependent.
- Shrinkwrap/FVP is materially slower than QEMU TCG.
- FVP descendant lifecycle is not yet robust enough for shared CI workers.
- The current CI environment does not expose a typed FVP platform bundle.

### Future CI gate

Consider a non-blocking FVP CCA job only after:

- model acquisition is automated and legally distributable to the runner;
- the complete platform is represented by typed artifacts;
- no process remains after repeated failure/timeout tests;
- runtime is measured and bounded;
- logs are always published; and
- QEMU and FVP CCA jobs have clearly distinct coverage goals.

FVP should focus on behavior QEMU does not model, including RME integration
with interrupt/IOMMU architecture and other FVP-only coverage.

## Validation Commands

### Phase 0 probe

The exact command will be added after the probe job exists. It must be
noninteractive and return failure on missing readiness, failed preflight,
failed `/bin/true`, or leaked processes.

### Incubator smoke

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca.toml \
  --fvp-platform-root target/cca-test \
  --filter 'test(fvp_cca_incubator_smoke)' \
  --skip-vhd-prompt
```

This command is valid only after the plan explicitly adds the
`fvp_cca_incubator_smoke` test. Until then, use the standalone Phase 0 probe.

### Shared Realm test

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca.toml \
  --fvp-platform-root target/cca-test \
  --filter 'test(boot_linux_direct_cca)' \
  --skip-vhd-prompt
```

### QEMU regression

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-qemu-cca.toml \
  --filter 'test(boot_linux_direct_cca)' \
  --skip-vhd-prompt
```

### Generic incubator regression

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-tcg-pcie.toml \
  --filter 'test(aarch64_tcg)' \
  --skip-vhd-prompt
```

### Existing FVP/TMK regression

```bash
cargo xflowey vmm-tests-run --filter 'test(cca_runtime)'
```

## Proposed Commit Sequence

1. `local: test: probe FVP CCA incubator transport`
   - durable `fvp-cca-incubator-probe` pipeline;
   - focused network/readiness/cleanup probe;
   - failure-injection modes;
   - no incubator backend.
2. `local: build: generalize CCA incubator host rootfs`
   - backend-neutral init contract;
   - QEMU and FVP validation.
3. `incubator: add FVP CCA backend`
   - profile, runtime, console handling, teardown, unit tests.
4. `local: flowey: wire FVP CCA incubator`
   - typed local artifacts and target-runner environment.
5. `vmm_tests: share CCA Realm test across incubators`
   - backend-neutral test name/body and regression matrix.
6. `local: docs: document FVP CCA tests`
   - temporary developer documentation and mapping updates.

Each commit must be reviewed before proceeding to the next phase.
The durable probe remains after implementation as a lower-level diagnostic.

## Risks and Open Questions

### Open questions requiring Phase 0 evidence

- What exact syntax allows multiple FVP user-network port mappings?
- Can Shrinkwrap express a dynamic pipette port through an rtvar, or is a
  generated overlay required?
- Does the FVP user network use QEMU-compatible static addressing?
- Should the L1 host use DHCP under FVP?
- Does FVP power off cleanly after pipette's poweroff request?
- Which console carries the stable host readiness marker?
- Can every FVP descendant be associated with a unique run token?
- Does the shared host rootfs require any FVP-specific drivers or init timing?

### Known risks

- FVP processes can outlive Shrinkwrap.
- The licensed FVP model may not support concurrent instances; execution is
  serialized until proven otherwise.
- Broad process matching could terminate another concurrent run.
- FVP user networking may not support the required forwarding shape.
- Concurrent FVP instances may collide on model resources or fixed ports.
- FVP setup may require proprietary model downloads unavailable in CI.
- Runtime may approach or exceed existing test watchdogs.
- Sharing the host init may regress the faster QEMU path.
- Renaming the existing QEMU-suffixed test changes filters and documentation.

## Definition of Done

- [ ] A portable `aarch64-fvp-cca.toml` profile exists.
- [ ] FVP CCA is represented by a distinct typed incubator backend.
- [ ] Flowey resolves all FVP inputs without machine-local profile paths.
- [ ] The L1 host passes preflight before advertising `cca`.
- [ ] Pipette runs through a loopback-only host connection.
- [ ] A target-runner smoke command exits zero.
- [ ] The normal CCA Realm test passes under FVP.
- [ ] The same Realm test passes under QEMU.
- [ ] The Realm test skips without `cca`.
- [ ] Existing generic TCG and FVP/TMK tests remain passing.
- [ ] All FVP/Shrinkwrap descendants are gone after success, failure, and
      timeout.
- [ ] All host, secure, preflight, incubator, OpenVMM, and Realm logs are
      retained.
- [ ] Local developer documentation describes setup and execution.
- [ ] FVP remains local-only until model acquisition and CI policy are
      explicitly resolved.

## Review

**Verdict: Minor revisions — addressed.**

The review required the plan to cover:

- the complete Docker/Shrinkwrap launch environment;
- crash-safe container and descendant cleanup;
- terminal-specific primary-console readiness;
- exact Flowey/runtime parameter plumbing;
- dynamic FVP networking and collision behavior;
- bounded phase-specific timeouts;
- rootfs network tooling and log extraction;
- a concrete durable probe and smoke-test contract;
- an explicit user-provisioned licensed FVP platform root;
- serialization of nextest and independent xflowey runs;
- durable lease-state transitions; and
- canonical platform identity.

The final review noted that the advisory lock and atomically renamed lease
manifest must use separate paths so renaming state cannot invalidate
cross-process exclusion. The plan now requires a stable
`.openvmm-fvp-cca.lock` inode and a separate atomically published
`.openvmm-fvp-cca-lease.json` manifest.

**Phase 3 implementation review verdict: Minor revisions — addressed.**

The implementation review corrected the CCA kernel-source error text and
narrowed the completed claims around the typed resolver, stale-state
reconciliation, and the still-QEMU-named Realm test. Buildroot hashing,
ambiguous-owner and leftover-process reconciliation, backend-neutral test
naming, and a two-independent-xflowey concurrency test remain explicit
follow-up work.
