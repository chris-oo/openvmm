# CCA Incubator VMM Tests

The shared CCA boot test runs an OpenVMM Realm inside a QEMU or licensed FVP
Linux/KVM host.

Both backends use the unified AArch64 CCA kernel and base test initrd from
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

The initial FVP mode targets basic Realm boot, not FVP device assignment.
The known kernel Realm-teardown warning and RMM SMMUv3 initialization
diagnostic remain visible follow-ups. They must not be interpreted as
evidence of working FVP SMMU/VFIO assignment.

Persistent incubator sessions, host-side runnable-test enumeration, and
model checkpoint/restore are not part of this mode. Do not assume that a
passing basic Realm test validates those features.
