// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run the unchanged Linux guest and sysfs/I/O script qualified with kvmtool.

use super::incubator_vfio_bdf;
use super::open_vfio_cdev;
use anyhow::Context;
use openvmm_defs::config::LinuxDirectBootMode;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::PcieMmioRangeConfig;
use petri::IsolationType;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use sha2::Digest;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::path::Path;
use vfio_assigned_device_resources::BarAddressConfig;
use vfio_assigned_device_resources::VfioRealmDeviceHandle;
use vm_resource::IntoResource;
use vmm_test_macros::vmm_test_with;

const GUEST_IMAGE_SHA256: &str = "6bef4c54ac93d8513ad7f125737c9ff0a8b0b7e34720e77253e2b63460002437";
const GUEST_INITRD_SHA256: &str =
    "d3ba987d83bd46a60cf2199d7989a7b940499065e1011125775034b5710dad92";
const DISK_SHA256: &str = "281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6";

fn open_pinned(path: &Path, expected: &str) -> anyhow::Result<File> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let actual: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    anyhow::ensure!(
        actual == expected,
        "reference guest hash mismatch for {}: {actual}",
        path.display()
    );
    file.rewind()?;
    Ok(file)
}

/// The unchanged initrd owns the guest driver, TSM lock/accept, direct disk
/// read, hash verification, unlock and poweroff sequence. No pipette is added.
#[vmm_test_with(
    openvmm,
    noagent,
    requires(cca, guest_memfd_in_place, cca_realm_vfio),
    configs(linux_direct_aarch64)
)]
async fn boot_linux_direct_cca_tdisp_ahci(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let output_dir = config.log_source().output_dir().to_path_buf();
    let content = std::env::var_os("VMM_TESTS_CONTENT_DIR")
        .context("VMM_TESTS_CONTENT_DIR is required for pinned reference guest inputs")?;
    let guest = Path::new(&content).join("cca-tdisp-guest");
    let kernel = open_pinned(&guest.join("Image"), GUEST_IMAGE_SHA256)?;
    let initrd = open_pinned(&guest.join("initrd"), GUEST_INITRD_SHA256)?;
    let bdf = incubator_vfio_bdf("cca-realm-vfio")?;
    let sysfs = Path::new("/sys/bus/pci/devices").join(&bdf);
    for (field, expected) in [
        ("vendor", "0x0abc"),
        ("device", "0xaced"),
        ("class", "0x010601"),
    ] {
        let actual = std::fs::read_to_string(sysfs.join(field))?;
        anyhow::ensure!(
            actual.trim() == expected,
            "unexpected fixture {field}: {actual}"
        );
    }
    let cdev = open_vfio_cdev(&bdf)?;
    let iommufd = File::options().read(true).write(true).open("/dev/iommu")?;
    let resource = VfioRealmDeviceHandle {
        pci_id: bdf,
        cdev,
        iommufd,
        requester_id: 0x100,
        bar_addresses: [
            0x5000_0000,
            0x5000_2000,
            0x5000_8000,
            0x5000_4000,
            0x5000_9000,
            0x5000_6000,
        ]
        .map(BarAddressConfig::Fixed),
    }
    .into_resource();

    let vm = config
        .with_isolation(IsolationType::Cca)
        .with_memory(petri::MemoryConfig {
            startup_bytes: 256 * 1024 * 1024,
            transparent_hugepages: false,
            ..Default::default()
        })
        .with_processor_topology(petri::ProcessorTopology {
            vp_count: 1,
            ..Default::default()
        })
        .modify_backend(move |backend| {
            backend
                .with_pcie_root_topology(2, 1, 1)
                .with_custom_config(move |config| {
                    config.hypervisor.guest_memfd_in_place = true;
                    for root in &mut config.pcie_root_complexes {
                        root.end_bus = 1;
                        for port in &mut root.ports {
                            port.hotplug = false;
                            port.devfn = Some(0);
                        }
                        if root.segment == 0 {
                            root.preserve_bars = true;
                            root.low_mmio = PcieMmioRangeConfig::Fixed(
                                memory_range::MemoryRange::new(0x5000_0000..0x5010_0000),
                            );
                        }
                    }
                    for device in &mut config.pcie_devices {
                        if device.resource.id() == "virtio" {
                            device.port_name = "s1rc0rp0".into();
                        }
                    }
                    config.pcie_devices.push(PcieDeviceConfig {
                        port_name: "s0rc0rp0".into(),
                        resource,
                    });
                    // Only console/platform arguments differ from kvmtool. Guest
                    // Image, initrd, driver and test script remain byte-identical.
                    config.load_mode = LoadMode::Linux {
                        kernel,
                        initrd: Some(initrd),
                        cmdline:
                            "rdinit=/da-guest-init.sh console=ttyAMA0 earlycon quiet da_phase=ahci"
                                .into(),
                        enable_serial: true,
                        isolation: openvmm_defs::config::LinuxIsolationConfig::None,
                        boot_mode: LinuxDirectBootMode::DeviceTree,
                        smbios: Default::default(),
                    };
                })
        })
        .run_without_agent()
        .await?;
    let teardown = vm.wait_for_clean_teardown().await;
    let console = std::fs::read_to_string(output_dir.join("linux.log"))
        .context("read unchanged guest console")?;
    let io_marker = format!("DA_STAGE_A_GUEST_AHCI_IO_PASS hash={DISK_SHA256}");
    let mut cursor = 0;
    for marker in [
        "DA_STAGE_A_GUEST_READY",
        "DA_STAGE_A_GUEST_LOCK_PASS",
        "DA_STAGE_A_GUEST_ACCEPT_RETURNED",
        io_marker.as_str(),
        "DA_STAGE_A_GUEST_UNLOCK_RETURNED",
    ] {
        let offset = console[cursor..].find(marker).with_context(|| {
            format!("unchanged guest did not report {marker}; teardown: {teardown:?}")
        })?;
        cursor += offset + marker.len();
    }
    anyhow::ensure!(
        !console.contains("failed to enable DMA from the device"),
        "guest reported failed RSI DMA enable"
    );
    tracing::info!("unchanged reference guest completed TDISP and AHCI I/O sequence");
    teardown.context("guest sequence completed but OpenVMM teardown failed")?;
    Ok(())
}
