# CCA Incubator VMM Tests

```admonish warning title="TODO: cleanup this page"
This page documents the current development workflow for QEMU and FVP CCA
tests. The workflow still has local prerequisites and duplicated setup that
should be cleaned up before this support is ready for a pull request.
```

The CCA VMM test boots the following nested stack:

1. QEMU TCG or the Arm FVP provides an Arm CCA-capable Linux/KVM host.
2. The incubator runs the AArch64 VMM test binary in that host.
3. OpenVMM launches a CCA Realm as the nested guest.
4. The test connects to the Realm agent over virtio-vsock.

The test requires Linux or WSL2. Docker builds the pinned TF-A, TF-RMM, and
EDK2 firmware stack.

## Prerequisites

Restore the AArch64 build packages:

```bash
cargo xflowey restore-packages aarch64 --no-compat-igvm
```

Install cargo-nextest if it is not already available:

```bash
cargo install --locked cargo-nextest
```

On an x86-64 host, install QEMU user-mode emulation so Flowey can inspect the
cross-compiled AArch64 test binary:

```bash
sudo apt install qemu-user
```

Docker must be installed, running, and usable by the current user.

FVP runs additionally require a user-provisioned, licensed Arm FVP model.
OpenVMM does not download or redistribute the model. The FVP platform root must
contain the Shrinkwrap virtual environment, configuration, CCA overlay, and
Buildroot image created by the existing KVM CCA setup.

The `linux-cca` repository must be checked out next to the OpenVMM repository:

```text
<PARENT_DIRECTORY>/
├── linux-cca/
└── openvmm/
```

The kernel build verifies the pinned revision. A different location can be
provided with `--cca-kernel-src` or `OPENVMM_CCA_KERNEL_SRC`.

## One-time emulation setup

Create the Shrinkwrap CCA environment and Buildroot host filesystem:

```bash
cargo xflowey kvm-cca-tests --install-emu
```

This is currently needed because the CCA incubator host-rootfs builder uses the
same Buildroot source image as the FVP KVM CCA workflow.

## Run the CCA Realm test

From the OpenVMM repository root, run:

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-qemu-cca.toml \
  --filter 'test(boot_linux_direct_cca) & !binary(cca)' \
  --skip-vhd-prompt
```

The filter selects the normal QEMU CCA VMM test while excluding the separate
FVP/TMK `cca` test binary.

To select the Realm boot test by its full descriptive suffix:

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-qemu-cca.toml \
  --filter 'test(boot_linux_direct_cca)' \
  --skip-vhd-prompt
```

To use a kernel checkout in another location:

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-qemu-cca.toml \
  --cca-kernel-src path/to/linux-cca \
  --filter 'test(boot_linux_direct_cca) & !binary(cca)' \
  --skip-vhd-prompt
```

The first run builds the firmware, kernels, OpenVMM, incubator, host rootfs,
and test binaries. Later runs reuse the build and firmware caches.

## Run with FVP

Pass the root populated by the one-time emulation setup:

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-fvp-cca.toml \
  --fvp-platform-root target/cca-test \
  --filter 'test(boot_linux_direct_cca) & !binary(cca)' \
  --skip-vhd-prompt
```

`OPENVMM_FVP_CCA_PLATFORM_ROOT` can be used instead of
`--fvp-platform-root`. FVP execution is serialized because concurrent use of
the licensed model has not been established as safe.

Only the per-run test artifact share is visible to the FVP host. The repository
and FVP platform root are not shared with the guest.

## Expected result

`boot_linux_direct_cca` creates a one-VP, 256 MiB Realm using:

- device-tree Linux direct boot;
- CCA isolation;
- no Hyper-V enlightenments or VMBus devices;
- a static PCIe root port; and
- virtio-vsock for the Realm agent.

The Realm serial console is disabled. FVP traps emulated UART accesses, and the
console increased the measured FVP test time substantially. The Realm agent
does not depend on serial because it communicates over virtio-vsock.

The test requires an agent ping, guest power-off, and clean OpenVMM teardown.
On the development platform used when this page was written, the test itself
took approximately 33 seconds under QEMU and 188 seconds under FVP.

## Logs

Host-side test orchestration files remain under:

```text
target/vmm_tests/
```

Each incubator invocation creates a unique `incubator-share-*` directory below
that root. Its `test_results/` directory contains Petri, OpenVMM, and incubator
console logs. Realm serial output is intentionally unavailable for this test;
use the OpenVMM and Petri logs to diagnose failures before the agent connects.

## Troubleshooting

### `qemu-aarch64` is missing

Install the `qemu-user` package or set Cargo's target runner explicitly:

```bash
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=path/to/qemu-aarch64
```

### The CCA kernel source is missing

Clone `linux-cca` next to OpenVMM or pass `--cca-kernel-src`. The source must
contain the pinned CCA v15 revision.

### The host rootfs source is missing

Run the one-time emulation setup:

```bash
cargo xflowey kvm-cca-tests --install-emu
```

### The FVP platform root is rejected

Pass the root created by `kvm-cca-tests --install-emu`. The path is
canonicalized and must contain:

```text
kvm-cca/rootfs.ext2
shrinkwrap/venv/bin/shrinkwrap
shrinkwrap/config/kvm_cca_planes.yaml
```

The FVP profile intentionally contains no machine-local model paths.

### Docker or firmware builds fail

Confirm that `docker run` works without `sudo`. The firmware source cache is
stored under `target/cca-qemu/firmware.cache`.

### Verify the generic incubator still works

Run the existing AArch64 QEMU TCG tests:

```bash
cargo xflowey vmm-tests-run \
  --target linux-aarch64-musl \
  --incubator petri/incubator/profiles/aarch64-tcg-pcie.toml \
  --filter 'test(aarch64_tcg)' \
  --skip-vhd-prompt
```
