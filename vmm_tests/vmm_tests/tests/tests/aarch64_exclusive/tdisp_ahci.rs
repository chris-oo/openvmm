// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compose and run the OpenVMM-owned Realm device-assignment guest fixture.

use super::super::nested::vm_host_fvp_realm_vfio;
use super::emulator_vfio_bdf;
use super::open_pinned_cca_payload as open_pinned;
use super::open_vfio_cdev;
use anyhow::Context;
use openvmm_defs::config::LinuxDirectBootMode;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::PcieMmioRangeConfig;
use petri::IsolationType;
use petri::PetriInitrd;
use petri::PetriVmBuilder;
use petri::ResolvedArtifact;
use petri::initrd::InitrdFile;
use petri::openvmm::OpenVmmPetriBackend;
use petri_artifacts_common::artifacts::FVP_CCA_PAYLOAD_AARCH64;
use petri_artifacts_vmm_test::artifacts::host_tools::NEXTEST_VMM_TESTS_ARCHIVE_LINUX_AARCH64_MUSL;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use vfio_assigned_device_resources::BarAddressConfig;
use vfio_assigned_device_resources::VfioRealmDeviceHandle;
use vm_resource::IntoResource;
use vmm_test_macros::vmm_test_with;

use petri_artifacts_common::cca_payload::tdisp_guest::INITRD_SHA256 as GUEST_INITRD_SHA256;
use petri_artifacts_common::cca_payload::tdisp_guest::LINUX_IMAGE_SHA256 as GUEST_IMAGE_SHA256;
const DISK_SHA256: &str = "281e519df3077b557c6b03f5da83c4e8d397219259615dd7c3308f89cae8f2a6";
const GUEST_INIT: &[u8] = include_bytes!("tdisp_ahci_guest_init.sh");

/// The test overlays its driver, TSM, I/O and teardown policy onto a pinned
/// generic binary initrd. No pipette is added to the Realm guest.
#[vmm_test_with(
    openvmm,
    noagent,
    requires(cca, guest_memfd_in_place, cca_realm_vfio),
    configs(
        linux_direct_aarch64[FVP_CCA_PAYLOAD_AARCH64],
        nested(
            (vm_host_fvp_realm_vfio, fvp_linux_direct_aarch64),
            (NEXTEST_VMM_TESTS_ARCHIVE_LINUX_AARCH64_MUSL, "tests", "aarch64_exclusive::tdisp_ahci", linux_direct_aarch64[FVP_CCA_PAYLOAD_AARCH64]),
        ),
    )
)]
async fn boot_linux_direct_cca_tdisp_ahci(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (payload,): (ResolvedArtifact<FVP_CCA_PAYLOAD_AARCH64>,),
) -> anyhow::Result<()> {
    let output_dir = config.log_source().output_dir().to_path_buf();
    let guest = payload.get().join("guest");
    let kernel = open_pinned(&guest.join("Image"), GUEST_IMAGE_SHA256)?;
    let mut base = open_pinned(&guest.join("initrd"), GUEST_INITRD_SHA256)?;
    let mut private_base = tempfile::NamedTempFile::new_in(&output_dir)?;
    std::io::copy(&mut base, &mut private_base)?;
    private_base.flush()?;
    let _verified_base = open_pinned(private_base.path(), GUEST_INITRD_SHA256)?;
    let expected_hash = format!("{DISK_SHA256}\n");
    let prepared = PetriInitrd::prepare(
        private_base.path(),
        &[
            InitrdFile {
                name: "da-guest-init.sh",
                contents: GUEST_INIT,
                permissions: 0o755,
            },
            InitrdFile {
                name: "da-ahci.sha256",
                contents: expected_hash.as_bytes(),
                permissions: 0o644,
            },
        ],
        "/da-guest-init.sh",
        Some(&output_dir),
    )?;
    let initrd = File::open(prepared.path.as_ref())?;
    {
        use sha2::Digest;
        let hex_digest = |bytes: &[u8]| -> String {
            sha2::Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        };
        std::fs::write(
            output_dir.join("tdisp-initrd-identity.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "base_sha256": GUEST_INITRD_SHA256,
                "script_sha256": hex_digest(GUEST_INIT),
                "composed_sha256": hex_digest(&std::fs::read(prepared.path.as_ref())?),
            }))?,
        )?;
    }
    let bdf = emulator_vfio_bdf("cca-realm-vfio")?;
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
        .context("read composed guest console")?;
    validate_guest_console(&console)
        .with_context(|| format!("guest fixture validation failed; teardown: {teardown:?}"))?;
    tracing::info!("OpenVMM guest completed TDISP, AHCI I/O and teardown diagnostics");
    teardown.context("guest sequence completed but OpenVMM teardown failed")?;
    drop(prepared);
    Ok(())
}

fn unique_line(lines: &[&str], marker: &str) -> anyhow::Result<usize> {
    let mut matches = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == marker).then_some(index));
    let index = matches
        .next()
        .with_context(|| format!("missing {marker}"))?;
    anyhow::ensure!(matches.next().is_none(), "duplicate {marker}");
    Ok(index)
}

fn validate_guest_console(console: &str) -> anyhow::Result<()> {
    let lines: Vec<_> = console.lines().collect();
    let io_marker = format!("DA_STAGE_A_GUEST_AHCI_IO_PASS hash={DISK_SHA256}");
    let mut positions = Vec::new();
    for marker in [
        "DA_STAGE_A_GUEST_READY",
        "DA_STAGE_A_GUEST_LOCK_PASS",
        "DA_STAGE_A_GUEST_ACCEPT_RETURNED",
        io_marker.as_str(),
        "DA_STAGE_A_GUEST_DRIVER_UNBOUND",
        "DA_STAGE_A_GUEST_UNLOCK_RETURNED",
        "DA_STAGE_A_GUEST_EXIT=0",
    ] {
        let position = unique_line(&lines, marker)?;
        anyhow::ensure!(
            positions.last().is_none_or(|previous| *previous < position),
            "out-of-order {marker}"
        );
        positions.push(position);
    }
    anyhow::ensure!(
        lines
            .iter()
            .filter(|line| line.starts_with("DA_STAGE_A_GUEST_EXIT="))
            .count()
            == 1,
        "expected one successful guest exit"
    );
    anyhow::ensure!(
        !console.contains("failed to enable DMA from the device"),
        "guest reported failed RSI DMA enable"
    );
    let mut previous: &[&str] = &[];
    for (phase, lower, upper) in [
        ("before_unbind", positions[3], positions[4]),
        ("after_unbind", positions[4], positions[5]),
        ("after_unlock", positions[5], positions[6]),
    ] {
        let begin = unique_line(&lines, &format!("DA_STAGE_A_DMESG_BEGIN {phase}"))?;
        let end = unique_line(&lines, &format!("DA_STAGE_A_DMESG_END {phase}"))?;
        anyhow::ensure!(
            lower < begin && begin < end && end < upper,
            "incomplete or out-of-order kernel log section: {phase}"
        );
        let records = &lines[begin + 1..end];
        anyhow::ensure!(!records.is_empty(), "empty kernel log section: {phase}");
        anyhow::ensure!(
            records.starts_with(previous),
            "kernel log lost continuity in {phase}"
        );
        for (index, record) in records.iter().enumerate() {
            let priority = record
                .strip_prefix("K:<")
                .and_then(|line| line.split_once('>'))
                .and_then(|(priority, _)| priority.parse::<u8>().ok())
                .with_context(|| format!("invalid raw kernel log record in {phase}: {record}"))?;
            anyhow::ensure!(
                phase == "before_unbind" || index < previous.len() || priority & 7 > 4,
                "new kernel warning/error in {phase}: {record}"
            );
        }
        previous = records;
    }
    Ok(())
}

petri::test_sync!(tdisp_diagnostics_preserve_boot_warnings, |_| Some(()));
fn tdisp_diagnostics_preserve_boot_warnings(
    _: petri::PetriTestParams<'_>,
    (): (),
) -> anyhow::Result<()> {
    validate_guest_console(&serial_log_roundtrip(&diagnostic_console(
        "<6>[3] driver detached\n",
        "<6>[4] device unlocked\n",
    )))
}

petri::test_sync!(tdisp_diagnostics_reject_new_errors, |_| Some(()));
fn tdisp_diagnostics_reject_new_errors(
    _: petri::PetriTestParams<'_>,
    (): (),
) -> anyhow::Result<()> {
    for diagnostic in [
        "<4>[3] warning while stopping AHCI\n",
        "<3>[3] failed to unmap device MMIO\n",
        "<4>[1] existing boot warning\n",
    ] {
        for (unbind, unlock) in [(diagnostic, ""), ("", diagnostic)] {
            let error =
                validate_guest_console(&serial_log_roundtrip(&diagnostic_console(unbind, unlock)))
                    .expect_err("new teardown diagnostic must fail");
            anyhow::ensure!(
                error.to_string().contains("new kernel warning/error"),
                "unexpected rejection: {error}"
            );
        }
    }
    Ok(())
}

petri::test_sync!(tdisp_diagnostics_reject_incomplete_evidence, |_| Some(()));
fn tdisp_diagnostics_reject_incomplete_evidence(
    _: petri::PetriTestParams<'_>,
    (): (),
) -> anyhow::Result<()> {
    let valid = diagnostic_console("", "");
    let mut invalid = vec![
        valid.replace("DA_STAGE_A_DMESG_END after_unlock\n", ""),
        valid.replace("DA_STAGE_A_GUEST_DRIVER_UNBOUND\n", ""),
        valid.replace("DA_STAGE_A_GUEST_EXIT=0\n", ""),
        valid.replace("DA_STAGE_A_GUEST_EXIT=0", "DA_STAGE_A_GUEST_EXIT=1"),
        valid.replace(DISK_SHA256, "incorrect hash"),
        valid.replace(
            "DA_STAGE_A_DMESG_BEGIN after_unbind\nK:<4>[1] existing boot warning\n",
            "DA_STAGE_A_DMESG_BEGIN after_unbind\n",
        ),
        valid.replace("K:<6>[2] disk read completed\n", "invalid raw record\n"),
        valid.replace(
            "K:<4>[1] existing boot warning\nK:<6>[2] disk read completed\n",
            "",
        ),
        format!("{valid}DA_STAGE_A_GUEST_EXIT=1\n"),
        format!("{valid}failed to enable DMA from the device\n"),
        format!(
            "{valid}DA_STAGE_A_DMESG_BEGIN before_unbind\n\
             <6>[9] duplicate section\nDA_STAGE_A_DMESG_END before_unbind\n"
        ),
    ];
    invalid.push(
        valid
            .replace("after_unbind", "temporary_phase")
            .replace("after_unlock", "after_unbind")
            .replace("temporary_phase", "after_unlock"),
    );
    invalid.push(valid.replace("K:", ""));
    for console in invalid {
        anyhow::ensure!(
            validate_guest_console(&console).is_err(),
            "incomplete or failed guest evidence was accepted"
        );
    }
    Ok(())
}

fn diagnostic_console(unbind: &str, unlock: &str) -> String {
    let baseline = "<4>[1] existing boot warning\n<6>[2] disk read completed\n";
    let encode = |records: &str| {
        records
            .lines()
            .map(|record| format!("K:{record}\n"))
            .collect::<String>()
    };
    let before = encode(baseline);
    let after_unbind = encode(&format!("{baseline}{unbind}"));
    let after_unlock = encode(&format!("{baseline}{unbind}{unlock}"));
    format!(
        "DA_STAGE_A_GUEST_READY\n\
         DA_STAGE_A_GUEST_LOCK_PASS\n\
         DA_STAGE_A_GUEST_ACCEPT_RETURNED\n\
         DA_STAGE_A_GUEST_AHCI_IO_PASS hash={DISK_SHA256}\n\
         DA_STAGE_A_DMESG_BEGIN before_unbind\n\
         {before}DA_STAGE_A_DMESG_END before_unbind\n\
         DA_STAGE_A_GUEST_DRIVER_UNBOUND\n\
         DA_STAGE_A_DMESG_BEGIN after_unbind\n\
         {after_unbind}DA_STAGE_A_DMESG_END after_unbind\n\
         DA_STAGE_A_GUEST_UNLOCK_RETURNED\n\
         DA_STAGE_A_DMESG_BEGIN after_unlock\n\
         {after_unlock}DA_STAGE_A_DMESG_END after_unlock\n\
         DA_STAGE_A_GUEST_EXIT=0\n"
    )
}

fn serial_log_roundtrip(console: &str) -> String {
    console
        .lines()
        .map(|line| match kmsg::SyslogParsedEntry::new(line) {
            Some(message) => message.display(false).to_string(),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
