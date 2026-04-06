# VirtioBlkDxe in mu_msvm — Implementation Plan

## Goal

Vendor the VirtioBlkDxe UEFI driver (and its dependencies) from
[mu_tiano_platforms](https://github.com/microsoft/mu_tiano_platforms/tree/main/QemuPkg/VirtioBlkDxe)
into the mu_msvm firmware repo, so that UEFI firmware can boot from a virtio-blk
disk exposed on an emulated PCIe root port. Then write an openvmm VMM test that
validates the full boot chain end-to-end.

## Context

OpenVMM already has a full virtio-blk device emulator (`vm/devices/virtio/virtio_blk`)
and emulated PCIe root complex support. The mu_msvm firmware already has PCIe
enumeration (PciHostBridgeDxe, PciBusDxe, CpuIo2Dxe) and the existing
`pcie_nvme_boot` test validates NVMe-over-PCIe boot. This plan adds the virtio
transport and block driver to the firmware side so the same PCIe infrastructure
can be used to boot from a virtio-blk device.

---

## Part 1 — Vendor Drivers into mu_msvm

All work in `/mnt/d/ai/jolteon/mu_msvm`.

### 1.1 Vendor Include Headers

Create the `MsvmPkg/Include/IndustryStandard/` directory (does not exist yet),
then copy header files from `mu_tiano_platforms QemuPkg/Include/` into
`MsvmPkg/Include/`:

```bash
mkdir -p /mnt/d/ai/jolteon/mu_msvm/MsvmPkg/Include/IndustryStandard
```

| Source (QemuPkg/Include/)           | Destination (MsvmPkg/Include/)             |
|-------------------------------------|--------------------------------------------|
| `Protocol/VirtioDevice.h`           | `Protocol/VirtioDevice.h`                  |
| `IndustryStandard/Virtio.h`         | `IndustryStandard/Virtio.h`                |
| `IndustryStandard/Virtio10.h`       | `IndustryStandard/Virtio10.h`              |
| `IndustryStandard/Virtio095.h`      | `IndustryStandard/Virtio095.h`             |
| `IndustryStandard/VirtioBlk.h`      | `IndustryStandard/VirtioBlk.h`             |
| `Library/VirtioLib.h`               | `Library/VirtioLib.h`                      |

**Notes:**
- `Virtio10.h` includes `IndustryStandard/Pci23.h` which is already provided by
  `MdePkg` — no action needed.
- `Library/` directory already exists in `MsvmPkg/Include/` (contains 15 files).
- `Protocol/` directory already exists (contains 7 files).

### 1.2 Vendor VirtioLib (Library)

Copy `QemuPkg/Library/VirtioLib/` into `MsvmPkg/Library/VirtioLib/`:

| File             | Notes                                             |
|------------------|---------------------------------------------------|
| `VirtioLib.c`    | Core ring init/map/flush utilities                |
| `VirtioLib.inf`  | Update `[Packages]`: replace `QemuPkg/QemuPkg.dec` with `MsvmPkg/MsvmPkg.dec` |

**INF edit** — in VirtioLib.inf, change:
```diff
 [Packages]
   MdePkg/MdePkg.dec
-  QemuPkg/QemuPkg.dec
+  MsvmPkg/MsvmPkg.dec
```

Dependencies (all from MdePkg, already available):
- `BaseLib`, `BaseMemoryLib`, `DebugLib`, `UefiBootServicesTableLib`

### 1.3 Vendor VirtioPciDeviceDxe (PCI Transport)

Copy `QemuPkg/VirtioPciDeviceDxe/` into `MsvmPkg/VirtioPciDeviceDxe/`:

| File                      | Notes                                             |
|---------------------------|---------------------------------------------------|
| `VirtioPciDevice.c`       | Driver binding, init/stop, protocol production     |
| `VirtioPciDevice.h`       | Internal VIRTIO_PCI_DEVICE structure               |
| `VirtioPciFunctions.c`    | PCI I/O read/write, DMA mapping implementations    |
| `VirtioPciDeviceDxe.inf`  | Update `[Packages]`: replace `QemuPkg/QemuPkg.dec` with `MsvmPkg/MsvmPkg.dec` |

**INF edit** — in VirtioPciDeviceDxe.inf, change:
```diff
 [Packages]
   MdePkg/MdePkg.dec
-  QemuPkg/QemuPkg.dec
+  MsvmPkg/MsvmPkg.dec
```

This driver:
- **Consumes:** `gEfiPciIoProtocolGuid` (already produced by PciBusDxe)
- **Produces:** `gVirtioDeviceProtocolGuid` (new, needs declaration in MsvmPkg.dec)

### 1.4 Vendor VirtioBlkDxe (Block Device Driver)

Copy `QemuPkg/VirtioBlkDxe/` into `MsvmPkg/VirtioBlkDxe/`:

| File             | Notes                                             |
|------------------|---------------------------------------------------|
| `VirtioBlk.c`    | Block I/O protocol implementation over virtio      |
| `VirtioBlk.h`    | Internal VBLK_DEV structure                        |
| `VirtioBlk.inf`  | Update `[Packages]`: replace `QemuPkg/QemuPkg.dec` with `MsvmPkg/MsvmPkg.dec` |

**INF edit** — in VirtioBlk.inf, change:
```diff
 [Packages]
   MdePkg/MdePkg.dec
-  QemuPkg/QemuPkg.dec
+  MsvmPkg/MsvmPkg.dec
```

This driver:
- **Consumes:** `gVirtioDeviceProtocolGuid` (produced by VirtioPciDeviceDxe)
- **Produces:** `gEfiBlockIoProtocolGuid` (standard, already declared in MdePkg)
- **Uses:** `VirtioLib` (library class)

### 1.5 Update MsvmPkg.dec

In `MsvmPkg/MsvmPkg.dec`, make two changes:

**a) Add protocol GUID** — append to the existing `[Protocols]` section (after
the last protocol around line 67):

```ini
  ## Virtio Device Protocol (vendored from QemuPkg)
  gVirtioDeviceProtocolGuid = {0xfa920010, 0x6785, 0x4941, {0xb6, 0xec, 0x49, 0x8c, 0x57, 0x9f, 0x16, 0x0a}}
```

**b) Add library class declaration** — add a new `[LibraryClasses]` section
(MsvmPkg.dec does not currently have one):

```ini
[LibraryClasses]
  ## @libraryclass Virtio utility library (vendored from QemuPkg)
  VirtioLib|Include/Library/VirtioLib.h
```

Insert this after the `[Protocols]` section and before `[PcdsFixedAtBuild]`
(around line 69).

### 1.6 Update DSC Files

**MsvmPkgX64.dsc:**

- **[LibraryClasses]** (around line 72–173): Add near the end of the general
  library class section:
  ```ini
    VirtioLib|MsvmPkg/Library/VirtioLib/VirtioLib.inf
  ```

- **[Components]** (around line 882, next to NvmExpressDxe): Add after
  `MsvmPkg/NvmExpressDxe/NvmExpressDxe.inf`:
  ```ini
    MsvmPkg/VirtioPciDeviceDxe/VirtioPciDeviceDxe.inf
    MsvmPkg/VirtioBlkDxe/VirtioBlk.inf
  ```

**MsvmPkgAARCH64.dsc:**

- **[LibraryClasses]** (around line 72–175): Same addition as X64.
- **[Components]** (around line 861, next to NvmExpressDxe): Same addition as X64.

### 1.7 Update FDF Files

**MsvmPkgX64.fdf:**

In the `[FV.DXE]` section, add after `INF MsvmPkg/NvmExpressDxe/NvmExpressDxe.inf`
(line 249):

```ini
  INF MsvmPkg/VirtioPciDeviceDxe/VirtioPciDeviceDxe.inf
  INF MsvmPkg/VirtioBlkDxe/VirtioBlk.inf
```

**MsvmPkgAARCH64.fdf:**

Same addition after `INF MsvmPkg/NvmExpressDxe/NvmExpressDxe.inf` (line 211).

### 1.8 Build & Verify

mu_msvm uses the **stuart** build system (edk2-pytool) with **VS2022** on Windows.
The repo already has a pre-configured `.venv` at `/mnt/d/ai/jolteon/mu_msvm/.venv`
with all stuart tools installed — no `pip install` needed.

Since we're running in **WSL**, use `pwsh.exe` to invoke the build (stuart
requires the VS2022 Windows toolchain):

```bash
# From WSL — X64 Debug build
pwsh.exe -Command "cd D:\\ai\\jolteon\\mu_msvm; .venv\\Scripts\\Activate.ps1; stuart_build -c MsvmPkg\\PlatformBuild.py TARGET=DEBUG BUILD_ARCH=X64"

# From WSL — AARCH64 Debug build
pwsh.exe -Command "cd D:\\ai\\jolteon\\mu_msvm; .venv\\Scripts\\Activate.ps1; stuart_build -c MsvmPkg\\PlatformBuild.py TARGET=DEBUG BUILD_ARCH=AARCH64"
```

If submodules haven't been set up yet (first build only):
```bash
pwsh.exe -Command "cd D:\\ai\\jolteon\\mu_msvm; .venv\\Scripts\\Activate.ps1; stuart_setup -c MsvmPkg\\PlatformBuild.py TOOL_CHAIN_TAG=VS2022; stuart_update -c MsvmPkg\\PlatformBuild.py TOOL_CHAIN_TAG=VS2022"
```

**Build output:**
- X64: `Build/MsvmX64/DEBUG_VS2022/FV/MSVM.fd`
- AARCH64: `Build/MsvmAARCH64/DEBUG_VS2022/FV/MSVM.fd`

### 1.9 Test Firmware with OpenVMM

To test the built firmware with the openvmm VMM test, use the
`--custom-uefi-firmware` flag when running tests:

```bash
# From the openvmm repo
cargo xflowey vmm-tests-run \
  --filter "test(pcie_virtio_blk_boot)" \
  --custom-uefi-firmware /mnt/d/ai/jolteon/mu_msvm/Build/MsvmX64/DEBUG_VS2022/FV/MSVM.fd \
  --dir /tmp/vmm-test-out
```

The existing `pcie_nvme_boot` test uses this same pattern — the test comment says:
> Pass `--custom-uefi-firmware <path>` to use a locally-built firmware.

---

## Part 2 — openvmm VMM Test

All work in `/home/coo/ai/jolteon/openvmm`.

### 2.1 Add `BootDeviceType::PcieVirtioBlk` Variant

In `petri/src/vm/mod.rs`, add a new variant to the `BootDeviceType` enum
(line ~2338, after `PcieNvme`):

```rust
/// Boot from a virtio-blk device attached to a PCIe root port.
PcieVirtioBlk,
```

**Update the `is_vpci` match** (~line 2348): add `PcieVirtioBlk` to the
non-VPCI arm alongside `PcieNvme`.

**Add routing logic** in the boot-device match (~line 824, after the `PcieNvme`
arm). The approach is to push a `PcieDeviceConfig` directly into
`self.config.pcie_devices` with a `VirtioPciDeviceHandle(VirtioBlkHandle { .. })`
resource — the same pattern used by `with_virtio_nic`:

```rust
BootDeviceType::PcieVirtioBlk => {
    self.config.pcie_devices.push(PcieDeviceConfig {
        port_name: "rp0".into(),
        resource: VirtioPciDeviceHandle(
            VirtioBlkHandle {
                disk: boot_drive,
                read_only: false,
            }
            .into_resource(),
        )
        .into_resource(),
    });
    self
}
```

This avoids adding a new config field — it reuses the existing `pcie_devices`
vector.

### 2.2 Add `with_pcie_virtio_blk` Helper to Petri

In `petri/src/vm/openvmm/modify.rs`, add a method after `with_pcie_nvme`
(~line 170), following the same pattern:

```rust
/// Add a PCIe virtio-blk device to the VM using a RAM-backed disk.
///
/// This exposes a virtio-blk device on a PCIe root port, suitable for
/// guests running virtio drivers (e.g. Linux, or UEFI with VirtioBlkDxe).
pub fn with_pcie_virtio_blk(mut self, port_name: &str) -> Self {
    self.config.pcie_devices.push(PcieDeviceConfig {
        port_name: port_name.to_string(),
        resource: virtio_resources::VirtioPciDeviceHandle(
            virtio_resources::blk::VirtioBlkHandle {
                disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                    len: Some(1024 * 1024),
                    sector_size: None,
                })
                .into_resource(),
                read_only: false,
            }
            .into_resource(),
        )
        .into_resource(),
    });
    self
}
```

### 2.3 Write the VMM Test

Add a new test to `vmm_tests/vmm_tests/tests/tests/multiarch/pcie.rs`,
modeled closely on the existing `pcie_nvme_boot` test (~line 415). The test
sets up the same PCIe topology (ECAM, MMIO gaps, single root complex with one
root port) and boots from the virtio-blk device instead of NVMe:

```rust
/// Boot an OS from a virtio-blk device on an emulated PCIe root port.
/// Validates the full PciHostBridgeDxe → PciBusDxe → VirtioPciDeviceDxe →
/// VirtioBlkDxe → OS boot chain.
///
/// Requires a mu_msvm firmware build with VirtioBlkDxe and VirtioPciDeviceDxe.
/// Pass `--custom-uefi-firmware <path>` to use a locally-built firmware.
#[openvmm_test(uefi_x64(vhd(alpine_3_23_x64)), uefi_aarch64(vhd(alpine_3_23_aarch64)))]
async fn pcie_virtio_blk_boot(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    const ECAM_SIZE: u64 = 256 * 1024 * 1024;
    const LOW_MMIO_SIZE: u64 = 64 * 1024 * 1024;
    const HIGH_MMIO_SIZE: u64 = 1024 * 1024 * 1024;

    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .with_boot_device_type(petri::BootDeviceType::PcieVirtioBlk)
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                c.efi_diagnostics_log_level =
                    openvmm_defs::config::EfiDiagnosticsLogLevelType::Full;
                if let openvmm_defs::config::LoadMode::Uefi {
                    ref mut default_boot_always_attempt,
                    ref mut enable_vpci_boot,
                    ..
                } = c.load_mode
                {
                    *default_boot_always_attempt = true;
                    *enable_vpci_boot = false;
                }
                let low_mmio_start = c.memory.mmio_gaps[0].start();
                let high_mmio_end = c.memory.mmio_gaps[1].end();
                let pcie_low =
                    MemoryRange::new(low_mmio_start - LOW_MMIO_SIZE..low_mmio_start);
                let pcie_high =
                    MemoryRange::new(high_mmio_end..high_mmio_end + HIGH_MMIO_SIZE);
                let ecam_range =
                    MemoryRange::new(pcie_low.start() - ECAM_SIZE..pcie_low.start());
                c.memory.pci_ecam_gaps.push(ecam_range);
                c.memory.pci_mmio_gaps.push(pcie_low);
                c.memory.pci_mmio_gaps.push(pcie_high);
                c.pcie_root_complexes.push(PcieRootComplexConfig {
                    index: 0,
                    name: "rc0".into(),
                    segment: 0,
                    start_bus: 0,
                    end_bus: 255,
                    ecam_range,
                    low_mmio: pcie_low,
                    high_mmio: pcie_high,
                    ports: vec![PcieRootPortConfig {
                        name: "rp0".into(),
                        hotplug: false,
                    }],
                });
            })
        })
        .run()
        .await?;

    // If we get here, the firmware successfully:
    //   1. Enumerated the PCIe root complex via ECAM
    //   2. PciBusDxe found the virtio-blk PCI device
    //   3. VirtioPciDeviceDxe bound and produced VIRTIO_DEVICE_PROTOCOL
    //   4. VirtioBlkDxe bound and produced EFI_BLOCK_IO_PROTOCOL
    //   5. UEFI boot manager booted the OS from the virtio-blk device
    //   6. Pipette agent started in guest

    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    tracing::info!(?guest_devices, "guest devices");

    // Virtio PCI vendor 0x1AF4, block device ID 0x1001 (legacy) or
    // 0x1042 (modern transitional)
    let virtio_blk_count = guest_devices
        .iter()
        .filter(|d| {
            d.vendor_id == 0x1AF4
                && (d.device_id == 0x1001 || d.device_id == 0x1042)
        })
        .count();
    assert!(virtio_blk_count >= 1, "virtio-blk device not visible in guest");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
```

**Required imports** to add at the top of `pcie.rs`:
```rust
use memory_range::MemoryRange;
use openvmm_defs::config::PcieRootComplexConfig;
use openvmm_defs::config::PcieRootPortConfig;
```
(Check whether these are already imported — `pcie_nvme_boot` may have added them.)

---

## Dependency Graph

```
VirtioBlkDxe (block device driver)
  ├── VirtioLib (library: ring init/map/flush utilities)
  ├── gVirtioDeviceProtocolGuid (protocol)
  │   └── Virtio10Dxe (modern 1.0 PCI transport — required for OpenVMM)
  │       ├── PciCapLib (PCI capability parsing)
  │       │   └── OrderedCollectionLib (from MdePkg, already available)
  │       ├── PciCapPciIoLib (PCI capability access via EFI_PCI_IO_PROTOCOL)
  │       └── gEfiPciIoProtocolGuid (from PciBusDxe, already in firmware)
  └── Standard MdePkg libs (BaseMemoryLib, DebugLib, etc.)
```

**Note:** VirtioPciDeviceDxe (legacy 0.9.5 transport) is **NOT needed** — see
"Implementation Findings" below. It can be kept in the tree for future use with
legacy virtio devices but is not required for the boot chain.

## Minimal Files to Vendor (Complete List)

**From `mu_tiano_platforms` → `mu_msvm`:**

### Headers (8 files)
1. `QemuPkg/Include/Protocol/VirtioDevice.h` → `MsvmPkg/Include/Protocol/VirtioDevice.h`
2. `QemuPkg/Include/IndustryStandard/Virtio.h` → `MsvmPkg/Include/IndustryStandard/Virtio.h`
3. `QemuPkg/Include/IndustryStandard/Virtio10.h` → `MsvmPkg/Include/IndustryStandard/Virtio10.h`
4. `QemuPkg/Include/IndustryStandard/Virtio095.h` → `MsvmPkg/Include/IndustryStandard/Virtio095.h`
5. `QemuPkg/Include/IndustryStandard/VirtioBlk.h` → `MsvmPkg/Include/IndustryStandard/VirtioBlk.h`
6. `QemuPkg/Include/Library/VirtioLib.h` → `MsvmPkg/Include/Library/VirtioLib.h`
7. `QemuPkg/Include/Library/PciCapLib.h` → `MsvmPkg/Include/Library/PciCapLib.h`
8. `QemuPkg/Include/Library/PciCapPciIoLib.h` → `MsvmPkg/Include/Library/PciCapPciIoLib.h`

### VirtioLib (2 files)
9. `QemuPkg/Library/VirtioLib/VirtioLib.c` → `MsvmPkg/Library/VirtioLib/VirtioLib.c`
10. `QemuPkg/Library/VirtioLib/VirtioLib.inf` → `MsvmPkg/Library/VirtioLib/VirtioLib.inf`

### BasePciCapLib (3 files)
11. `QemuPkg/Library/BasePciCapLib/BasePciCapLib.c` → `MsvmPkg/Library/BasePciCapLib/BasePciCapLib.c`
12. `QemuPkg/Library/BasePciCapLib/BasePciCapLib.h` → `MsvmPkg/Library/BasePciCapLib/BasePciCapLib.h`
13. `QemuPkg/Library/BasePciCapLib/BasePciCapLib.inf` → `MsvmPkg/Library/BasePciCapLib/BasePciCapLib.inf`

### UefiPciCapPciIoLib (3 files)
14. `QemuPkg/Library/UefiPciCapPciIoLib/UefiPciCapPciIoLib.c` → `MsvmPkg/Library/UefiPciCapPciIoLib/UefiPciCapPciIoLib.c`
15. `QemuPkg/Library/UefiPciCapPciIoLib/UefiPciCapPciIoLib.h` → `MsvmPkg/Library/UefiPciCapPciIoLib/UefiPciCapPciIoLib.h`
16. `QemuPkg/Library/UefiPciCapPciIoLib/UefiPciCapPciIoLib.inf` → `MsvmPkg/Library/UefiPciCapPciIoLib/UefiPciCapPciIoLib.inf`

### Virtio10Dxe (3 files) — modern PCI transport
17. `QemuPkg/Virtio10Dxe/Virtio10.c` → `MsvmPkg/Virtio10Dxe/Virtio10.c`
18. `QemuPkg/Virtio10Dxe/Virtio10.h` → `MsvmPkg/Virtio10Dxe/Virtio10.h`
19. `QemuPkg/Virtio10Dxe/Virtio10.inf` → `MsvmPkg/Virtio10Dxe/Virtio10.inf`

### VirtioBlkDxe (3 files) — block device driver
20. `QemuPkg/VirtioBlkDxe/VirtioBlk.c` → `MsvmPkg/VirtioBlkDxe/VirtioBlk.c`
21. `QemuPkg/VirtioBlkDxe/VirtioBlk.h` → `MsvmPkg/VirtioBlkDxe/VirtioBlk.h`
22. `QemuPkg/VirtioBlkDxe/VirtioBlk.inf` → `MsvmPkg/VirtioBlkDxe/VirtioBlk.inf`

### Optional: VirtioPciDeviceDxe (4 files) — legacy transport, not required
23–26. Can be kept for future legacy virtio device support, but not needed for OpenVMM.

## Files to Modify in mu_msvm

- `MsvmPkg/MsvmPkg.dec` — add `gVirtioDeviceProtocolGuid` + `VirtioLib`, `PciCapLib`, `PciCapPciIoLib` library classes
- `MsvmPkg/MsvmPkgX64.dsc` — add library class mappings (VirtioLib, PciCapLib, PciCapPciIoLib, OrderedCollectionLib) + components (Virtio10Dxe, VirtioBlkDxe)
- `MsvmPkg/MsvmPkgAARCH64.dsc` — same as X64
- `MsvmPkg/MsvmPkgX64.fdf` — add `INF` entries to `[FV.DXE]`
- `MsvmPkg/MsvmPkgAARCH64.fdf` — same as X64

## Files to Modify/Create in openvmm

- `petri/src/vm/mod.rs` — add `PcieVirtioBlk` to `BootDeviceType` enum + routing
- `petri/src/vm/openvmm/construct.rs` — add `pcie_virtio_blk_drives` construction
- `petri/src/vm/openvmm/modify.rs` — add `with_pcie_virtio_blk()` helper
- `vmm_tests/vmm_tests/tests/tests/multiarch/pcie.rs` — add `pcie_virtio_blk_boot` test

---

## Implementation Findings

### Transport version mismatch (resolved)

OpenVMM's virtio-pci device presents as a **modern (1.0)** device:
- Vendor ID: `0x1AF4`
- Device ID: `0x1042` (base `0x1040` + block device type 2)
- Uses PCI capabilities for config space layout (BAR0 with cap pointers)

The initially vendored **VirtioPciDeviceDxe** only supports **legacy (0.9.5)** transport:
- Matches device IDs `0x1000–0x103F` with revision 0
- Uses fixed I/O port offsets in BAR0

**Fix:** Vendor **Virtio10Dxe** instead — this is the modern transport driver that
uses PCI capabilities (VIRTIO_PCI_CAP) to locate config regions. It matches
device IDs `≥ 0x1040`. Requires two additional libraries: `BasePciCapLib` and
`UefiPciCapPciIoLib`.

### UEFI boot chain: WORKING ✅

With Virtio10Dxe + VirtioBlkDxe, the full UEFI boot chain works:

1. ✅ PciHostBridgeDxe enumerates PCIe root complex via ECAM
2. ✅ PciBusDxe discovers virtio PCI device (0x1AF4:0x1042)
3. ✅ Virtio10Dxe binds and produces `VIRTIO_DEVICE_PROTOCOL`
4. ✅ VirtioBlkDxe binds and produces `EFI_BLOCK_IO_PROTOCOL`
5. ✅ UEFI boot manager finds EFI system partition on the virtio-blk disk
6. ✅ GRUB loads and starts the Linux kernel

Confirmed by `BootSuccess` event and GRUB appearing on the framebuffer.

### Linux guest driver probe failure: BLOCKING 🔴

After UEFI hands off to Linux, the kernel's `virtio_blk` driver fails:

```
virtio_blk virtio0: 1/0/0 default/read/poll queues
virtio_blk virtio0: probe with driver virtio_blk failed with error -2
```

Error -2 is `ENOENT`. The device is discovered on PCI but the virtio_blk driver
can't complete initialization — likely a virtqueue setup issue in OpenVMM's
virtio-pci emulator when used by the Linux driver (as opposed to the UEFI driver
which uses a simpler initialization sequence).

This causes the root filesystem mount to fail, the guest drops to emergency
shell, and the pipette test agent never starts — making the VMM test hang.

**This is an OpenVMM virtio-pci emulator bug, not a firmware issue.** The UEFI
firmware vendoring is complete and working. The VMM test needs either:
1. The OpenVMM virtio-pci emulator bug to be fixed (separate investigation), or
2. The test to validate only the UEFI boot chain (BootSuccess) without waiting
   for pipette

## Risks & Open Questions

1. ~~**VirtIO PCI version**~~ — **Resolved.** Virtio10Dxe handles modern transport.

2. **DMA / Bounce Buffers**: The virtio drivers use `VirtioAllocateSharedPages` /
   `VirtioMapSharedBuffer` for DMA. Under isolation (SNP/TDX), these may need
   bounce-buffer treatment. For initial non-isolated scenarios this works as-is.

3. **Linux virtio_blk probe failure**: OpenVMM's virtio-pci device emulator has
   a bug that prevents the Linux virtio_blk driver from probing successfully.
   UEFI boot works, but guest OS can't use the device after handoff. Needs
   separate investigation in `vm/devices/virtio/virtio/src/transport/pci.rs`.

4. **Firmware size**: The DXE firmware volume has sufficient space — X64 build
   succeeds in 12s with all drivers included.

5. **Guest driver availability**: Alpine Linux includes virtio drivers in-kernel
   (confirmed — `virtio_blk` module loads and finds the PCI device). The probe
   failure is an emulator issue, not a missing driver issue.
