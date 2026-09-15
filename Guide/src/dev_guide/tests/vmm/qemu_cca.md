# CCA Incubator VMM Tests

The shared CCA boot test runs an OpenVMM Realm inside a QEMU or licensed FVP
Linux/KVM host.

The default profiles use the unified AArch64 CCA kernel and base test initrd from
`openvmm-deps` release `0.3.0-139`. The incubator injects `/cca-init.sh` and
host CA certificates into a temporary initrd copy. The published kernel and
base initrd remain unchanged. No block root filesystem is required.

## Run with QEMU

From the repository root:

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-qemu-cca.toml \
  --filter 'binary(=tests) & test(boot_linux_direct_cca)'
```

Flowey resolves the official kernel, initrd, QEMU, TF-A, and TF-RMM assets,
then builds OpenVMM, pipette, incubator, and the VMM test archive from the
current source.

The QEMU CCA host mounts the `host` 9P tag and uses static QEMU user
networking. QEMU uses Linux-direct TF-A rather than EDK2.

## Run with FVP

```admonish warning
FVP is opt-in and requires a user-provisioned licensed model and the exact
supported local firmware/toolchain tuple. Hosted CI does not provide the
licensed model. A mismatch is an error, not a skipped test or a request to
rebuild local firmware automatically.
```

Use Linux or WSL2 with a local Docker daemon accessible through a Unix
socket. The model is installed inside the pinned Docker image, not at a
host executable path.

Provide two read-only roots:

| Input | Contents |
|---|---|
| FVP platform root | `shrinkwrap/` checkout, its `venv/`, and the provisioned platform overlay |
| Shrinkwrap package root | `cca-3world.yaml` and its `cca-3world/` firmware files |

The exact supported model, container digest, Shrinkwrap revision, package,
overlay, BL1, FIP, and DTB identities are checked in under
`petri/incubator/platforms/fvp-cca-v15.yaml`. The normalized Python package
identity is in the adjacent `fvp-cca-v15.pip-freeze` file. The runtime rejects
tracked source changes and unapproved untracked files in the toolchain.
Do not change its checkout or virtualenv during a run.

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca.toml \
  --fvp-platform-root /path/to/fvp-platform \
  --shrinkwrap-package-root /path/to/shrinkwrap-package \
  --filter 'binary(=tests) & test(boot_linux_direct_cca)'
```

FVP resolves the common kernel/initrd payload without downloading QEMU
platform firmware. Flowey prepares a small share containing the required
source-matched binaries and Realm payload. The runtime copies verified
inputs into an owned writable Shrinkwrap workspace and uses explicit
firmware paths. It does not load artifacts from Shrinkwrap's default
workspace.

The initial FVP tuple requires the qualified release and payload identities.
Local kernel/initrd archive paths may select copies of those official
archives; incompatible version or hash overrides fail before launch.
QEMU's development overrides are unchanged.

FVP commands must name a snapshotted binary under the guest share. Flowey
maps the selected nextest executable there and includes it in the explicit
share inventory. Arbitrary commands outside that share are not supported.

The profile's `[incubator.deadlines]` table accepts a `validation` override
in seconds (default 300, allowed range 30..1800). It separately bounds initial
input validation, locked inventory/snapshot preparation, and post-run
toolchain verification. Retries share their phase's deadline. Lock waiting
does not consume the subsequent inventory budget.

EDK2 loads the initrd through SemihostFs. `startup.nsh` is the only source
of the EFI kernel command line, including `initrd=initrd` and
`rdinit=/cca-init.sh`. The host mounts the `FM` 9P tag and runs bounded
userspace DHCP on `eth0`, with kernel DHCP disabled by `ip=off`.

There is no FVP rootfs option. The runtime overlay fixes the virtio-blk
backing-image path to empty. An unbacked, zero-capacity device may still
appear in the host; it is not used as the root filesystem.

### Local in-place guest_memfd profile

`aarch64-fvp-cca-guest-memfd-in-place.toml` selects a separate, pinned local
firmware and kernel tuple. It does not change the default v15 profile.
The tuple is recorded in
`petri/incubator/platforms/fvp-cca-guest-memfd-in-place.yaml`.
The kernel comes from linux-cca integration-v7, but the profile names the
memory mode, not that source branch.

This profile requires `--cca-in-place-payload-root`. Its directory must
contain `Image`, `config`, `manifest.txt`, and `initrd`. The manifest uses
the same `key=value` fields as the published kernel manifest. The Image,
configuration, revision, and release must match the constants in
`petri_artifacts_common::cca_payload::guest_memfd_in_place`. Both host and
Realm use that Image. The initrd must be the unmodified official base
initrd from release `0.3.0-139`, not a local probe initrd. Archive, release,
and hash overrides are rejected.

The FVP host NIC must be built in (`CONFIG_SMC91X=y`); the base initrd does
not load its module. Payload validation checks this before launching FVP.

The local layout places the existing clean toolchain at
`<PLATFORM_ROOT>/cca-test/shrinkwrap` and the isolated runtime overlay at
`<PLATFORM_ROOT>/cca-tdisp-stage-a/test-platform/overlay.yaml`. Copy the
checked-in `fvp-guest-memfd-in-place-overlay.yaml` to that overlay path.
Provide the separately pinned package through `--shrinkwrap-package-root`.
Keep output outside all input roots. The canonical payload root must neither
contain the output directory nor be inside it, including through symlinks.
Both CLI and direct Flowey job requests reject overlap before output creation
or cleanup:

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca-guest-memfd-in-place.toml \
  --fvp-platform-root path/to/platform-root \
  --shrinkwrap-package-root path/to/in-place-package \
  --cca-in-place-payload-root path/to/in-place-payload \
  --dir path/to/separate-output \
  --filter 'binary(=tests) & test(boot_linux_direct_cca_in_place)'
```

Tests must require both `cca` and `guest_memfd_in_place` and select the
OpenVMM memory mode themselves. The profile only selects host inputs and
publishes capabilities after the normal readiness handshake. Use an explicit
filter; this profile does not qualify the legacy memory mode on the new host.

The default QEMU and FVP profiles qualify the existing CCA Virtio-vsock
pipette boot case. They do not advertise `guest_memfd_in_place`. The official
v15 payload has not been qualified for the full in-place ABI sequence,
including mmap, INIT_SHARED, ATTRIBUTES2, and INIT_RIPAS. Do not infer support
from one ioctl or add an ENOTTY fallback. Use the separate pinned candidate
to run the same CCA test body with the in-place memory mode selected.
Generic TCG VFIO/P2P tests are separate regressions, not Realm memory-mode
parity evidence.

For QEMU, `aarch64-qemu-cca-guest-memfd-in-place.toml` selects the same pinned
local kernel and official initrd while reusing the published QEMU, TF-A and
TF-RMM archives from `openvmm-deps` release `0.3.0-139`. It does not rebuild
firmware or change the default QEMU profile. Supply the payload directory;
firmware, release and hash overrides are rejected for this profile.

```admonish warning
The in-place QEMU profile is a diagnostic candidate, not a qualified tuple.
The current local kernel and published firmware boot the host, but the
Realm stalls before pipette. A block-test console run stopped during ITS
initialization. This does not yet identify the component that needs a fix.
Use `INCUBATOR_TIMEOUT` to bound each incubator invocation while debugging.
```

The effective package uses the model's default PCI hierarchy. It removes the
DA recipe's PCI JSON fixup command and its unused PCI hierarchy and sample-key
runtime variables. Its remaining CPU/SMMU model parameters and firmware
bytes are unchanged. These differences are recorded beside the package hash.
The package omits `pci.hierarchy_file_name`; an empty filename is not the
model default and causes a PCI hierarchy parser error.
There are no assigned devices, DA certificates, or DA disk images in the
staged runtime. This is an in-process Virtio test input, not evidence of
TDISP or host VFIO support. A prior no-device boot of the source package
does not qualify this effective package; run the Petri tests to qualify it.

The paired CCA Virtio-vsock test has passed on the pinned in-place FVP tuple,
including pipette ping, Realm poweroff, OpenVMM teardown, and outer host
shutdown. The original separate-backing test also passes on the default
QEMU and FVP tuples. The dedicated block and network tests below extend
guest-VM coverage. Rng and console crate tests remain separate regression
coverage, not CCA guest-VM coverage.

### Virtio block and network I/O

`virtio_blk_cca_in_place` and `virtio_net_cca_in_place` require both `cca` and
`guest_memfd_in_place`. Each explicitly enables in-place backing in a
Linux-direct Realm and keeps Virtio-vsock as the independent pipette control
channel.

The block test attaches a temporary 8-MiB scratch file as a PCIe Virtio disk.
It checks distinct 64-KiB patterns near each end, performs guest direct writes
and direct readback, then checks the backing file from the host after clean
teardown. Direct I/O prevents guest page-cache hits from replacing device
reads.

The network test attaches a PCIe Virtio-net NIC backed by Consomme. It uses
a dynamically allocated loopback forwarding port and a guest TCP listener,
with distinct 64-KiB payloads in each direction. Both receivers check all
bytes. The exchange has a deadline and needs no external network service.
Both tests require Realm poweroff and clean OpenVMM teardown.

Use the in-place FVP invocation above with this filter. Run one test at a
time so model instances do not compete for the FVP lock:

```bash
NEXTEST_TEST_THREADS=1 cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca-guest-memfd-in-place.toml \
  --fvp-platform-root path/to/platform-root \
  --shrinkwrap-package-root path/to/in-place-package \
  --cca-in-place-payload-root path/to/in-place-payload \
  --dir path/to/separate-output \
  --filter 'binary(=tests) & test(virtio_) & test(cca_in_place)'
```

Attempt the same tests on QEMU with upstream firmware and bounded execution:

```bash
NEXTEST_TEST_THREADS=1 INCUBATOR_TIMEOUT=180 cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-qemu-cca-guest-memfd-in-place.toml \
  --cca-in-place-payload-root path/to/in-place-payload \
  --dir path/to/separate-qemu-output \
  --filter 'binary(=tests) & test(virtio_) & test(cca_in_place)'
```

These are in-process Virtio tests, not assigned-device or TDISP DMA tests.
Both have passed on the pinned in-place FVP tuple. Neither has completed on
the current in-place QEMU candidate.

## Build now, run later

Add `--build-only` to a command above to prepare the artifacts without
running the tests. Run the generated `run.sh` on the same build host.
It retains the resolved payload, firmware and FVP input paths, so keep those
inputs at their original locations. This script is not a portable target-side
test package.

`vmm-tests-run-target --incubator` rejects CCA profiles before constructing
the execution pipeline. Use `vmm-tests-run` or its generated host-side script;
the portable target-side path does not carry the required CCA overrides.

## Avoid unnecessary enumeration boots

The `tests` executable contains `boot_linux_direct_cca`. The explicit
`binary(=tests)` filter lets nextest exclude the separate `cca` and `tmks`
executables before running them to enumerate their tests.

```admonish tip
Use the binary filter for this focused CCA test. A test-name filter alone
can require booting the incubator to enumerate unrelated test binaries.
Do not restrict the binary when you intend to run a multi-binary suite.
```

This optimization avoids unrelated boots; it does not accelerate an
individual boot or bypass host-property checks. Normal and ignored-test
enumeration for the selected executable still runs inside the chosen
backend. The exact number of enumeration invocations depends on nextest
and the selected suite.

Initial artifact discovery is different: Flowey may use native execution
or QEMU user-mode emulation to collect required artifacts, including ignored
candidates. That does not qualify the execution backend or advertise its
runtime capabilities.

See [nextest binary exclusions](https://nexte.st/docs/filtersets/reference/#basic-predicates).

## Results and cleanup

The selected test creates a one-VP, 256-MiB CCA Realm, pings its pipette agent
over virtio-vsock, requests Realm poweroff, and waits for OpenVMM teardown.
The outer incubator then powers off. Enumeration-only or ignored-test
success is not evidence that a Realm booted.

FVP logs include the launcher, named consoles, model version inventory,
DHCP/network diagnostics, and nested VMM test output. The runtime reports
the per-run output directory. Persistent ownership state and model locks
are separate from the user-provided input roots and temporary workspace.
Use a persistent output filesystem outside `/tmp` and `/run`. Unsupported
output locations are rejected before session resources are registered.
Each launch attempt has separate console logs, a generated overlay, and a
command record. A port-collision retry cannot reuse stale readiness markers.

Cancellation and phase deadlines trigger owned-resource cleanup. Foreign,
newer-version, corrupt, or ambiguous state is preserved with an explicit
diagnostic. If a prior owner died while an unrecorded helper might have
been running, recovery can require manual intervention. Do not delete the
persistent lock file or remove containers solely by a name prefix.

When ownership and helper termination are proven, recovery can preserve
logs in the registered output directory before removing an interrupted
workspace. It treats the previous run as failed, not qualified. Process
cleanup, output preservation, and retirement have separate non-resetting
cleanup budgets. Partial or changed workspace trees remain preserved.

Automatic retirement supports trees of at most 4096 entries, 32 directory
levels, and one GiB each. Unsupported trees, nested mounts, symlinks, and
hard links require manual preservation; the runtime reports an error
rather than deleting unverified data.

Inventory errors identify the failing operation and status without blindly
printing output that might contain credentials. Reproduce the named check
locally with the same isolated interpreter or selected Docker endpoint.
Redact credentials and private environment values before sharing logs.

## Current limits

The pinned CCA kernel does not permit MPIDR register writes for Realm VPs.
OpenVMM checks that the requested topology matches KVM's reset MPIDR values
and rejects mismatches instead of changing the Realm's CPU identities.
Ordinary KVM guests still use the configurable MPIDR path.

The initial FVP mode targets basic Realm boot, not FVP device assignment.
The known kernel Realm-teardown warning and RMM SMMUv3 initialization
diagnostic remain visible follow-ups. They must not be interpreted as
evidence of working FVP SMMU/VFIO assignment.

Persistent incubator sessions, host-side runnable-test enumeration, and
model checkpoint/restore are not part of this mode. Do not assume that a
passing basic Realm test validates those features.
