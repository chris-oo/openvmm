// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! QEMU device provisioning and diagnostic CCA command construction.

use crate::profile::DeviceConfig;
use crate::profile::QemuCcaConfig;
use crate::profile::QemuCcaExtraArg;
use crate::profile::parse_size;
use anyhow::Context;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const EXTRA_DEVICE_ADDR_BASE: usize = petri::qemu::EXTRA_DEVICE_ADDR_BASE;

pub(crate) fn petri_devices(
    devices: &[DeviceConfig],
) -> anyhow::Result<Vec<petri::qemu::devices::DeviceConfig>> {
    use petri::qemu::devices as p;
    devices
        .iter()
        .map(|device| {
            Ok(match device {
                DeviceConfig::VirtioBlk(c) => {
                    p::DeviceConfig::VirtioBlk(p::VirtioBlkDeviceConfig {
                        name: c.name.clone(),
                        size: parse_size(&c.size)?,
                        vfio: c.vfio,
                    })
                }
                DeviceConfig::Edu(c) => p::DeviceConfig::Edu(p::EduDeviceConfig {
                    name: c.name.clone(),
                    dma_mask: c
                        .dma_mask
                        .as_deref()
                        .map(|value| {
                            if let Some(hex) = value.strip_prefix("0x") {
                                u64::from_str_radix(hex, 16)
                            } else {
                                value.parse()
                            }
                            .context("invalid DMA mask")
                        })
                        .transpose()?,
                    vfio: c.vfio,
                }),
                DeviceConfig::IvshmemPlain(c) => {
                    p::DeviceConfig::IvshmemPlain(p::IvshmemPlainDeviceConfig {
                        name: c.name.clone(),
                        size: parse_size(&c.size)?,
                        vfio: c.vfio,
                    })
                }
            })
        })
        .collect()
}

/// Set up extra devices for VFIO before publishing their BDFs and capabilities.
pub async fn setup_vfio_devices(
    client: &pipette_client::PipetteClient,
    devices: &[DeviceConfig],
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    let mut capabilities = Vec::new();
    for (device_index, device) in devices.iter().enumerate().filter(|(_, d)| d.vfio()) {
        let name = device.name();
        let addr = EXTRA_DEVICE_ADDR_BASE + device_index;
        let rp_bdf = format!("0000:00:{addr:02x}.0");
        let secondary_bus_path = format!("/sys/bus/pci/devices/{rp_bdf}/secondary_bus_number");
        let secondary_bus_raw = client
            .read_file(&secondary_bus_path)
            .await
            .with_context(|| {
                format!("failed to read secondary bus number for device '{name}' ({rp_bdf})")
            })?;
        let secondary_bus_str = String::from_utf8_lossy(&secondary_bus_raw);
        let secondary_bus: u8 = secondary_bus_str.trim().parse().with_context(|| {
            format!("unexpected secondary bus number {secondary_bus_str:?} for device '{name}'")
        })?;
        let bdf = format!("0000:{secondary_bus:02x}:00.0");
        client
            .read_file(format!("/sys/bus/pci/devices/{bdf}/vendor"))
            .await
            .with_context(|| {
                format!("no device found behind {rp_bdf} (expected {bdf}) for '{name}'")
            })?;
        tracing::info!(%name, %bdf, %addr, "binding device to vfio-pci");
        let _ = client
            .write_file(
                format!("/sys/bus/pci/devices/{bdf}/driver/unbind"),
                bdf.as_bytes(),
            )
            .await;
        client
            .write_file(
                format!("/sys/bus/pci/devices/{bdf}/driver_override"),
                b"vfio-pci".as_slice(),
            )
            .await
            .context("failed to set driver_override")?;
        client
            .write_file("/sys/bus/pci/drivers/vfio-pci/bind", bdf.as_bytes())
            .await
            .context("failed to bind to vfio-pci")?;
        let env_name = format!(
            "INCUBATOR_VFIO_BDF_{}",
            name.to_uppercase().replace('-', "_")
        );
        tracing::info!(%env_name, %bdf, "VFIO device ready");
        env.insert(env_name, bdf);
        capabilities.push(device.capability());
    }
    if !capabilities.is_empty() {
        publish_capabilities(&mut env, &capabilities);
    }
    Ok(env)
}

/// Merge capabilities only after provisioning and agent readiness succeed.
pub(crate) fn publish_capabilities(env: &mut BTreeMap<String, String>, capabilities: &[String]) {
    let mut values = std::env::var("PETRI_CAPABILITIES")
        .ok()
        .into_iter()
        .chain(env.get("PETRI_CAPABILITIES").cloned())
        .flat_map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<std::collections::BTreeSet<_>>();
    values.extend(capabilities.iter().cloned());
    if !values.is_empty() {
        env.insert(
            "PETRI_CAPABILITIES".to_string(),
            values.into_iter().collect::<Vec<_>>().join(","),
        );
    }
}

/// A QEMU CCA command and the log path for each named serial console.
pub(crate) struct QemuCcaCommand {
    pub command: Command,
    pub console_logs: BTreeMap<String, PathBuf>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "the arguments are the complete QEMU CCA boot artifact contract"
)]
pub(crate) fn build_qemu_cca_command(
    config: &QemuCcaConfig,
    firmware: &Path,
    kernel: &Path,
    initrd: &Path,
    share_dir: &Path,
    host_pipette_port: u16,
    output_dir: &Path,
    instance_id: u64,
) -> anyhow::Result<QemuCcaCommand> {
    let share_dir = qemu_option_path(share_dir, "QEMU CCA share directory")?;
    let mut cmd = Command::new(&config.binary);
    cmd.arg("-nodefaults");
    cmd.arg("-accel").arg("tcg");
    cmd.arg("-machine").arg(&config.machine);
    cmd.arg("-cpu").arg(&config.cpu);
    cmd.arg("-m").arg(&config.memory);
    cmd.arg("-smp").arg(&config.smp);
    cmd.arg("-bios").arg(firmware);
    cmd.arg("-kernel").arg(kernel);
    if let Some(address) = config.kernel_load_address {
        let kernel = qemu_option_path(kernel, "QEMU CCA kernel")?;
        cmd.arg("-device").arg(format!(
            "loader,file={kernel},addr={address:#x},force-raw=on"
        ));
    }
    let initrd_address = config
        .initrd_load_address
        .context("QEMU CCA initrd load address is missing")?;
    let initrd = qemu_option_path(initrd, "QEMU CCA initrd")?;
    let initrd_size = fs_err::metadata(initrd)
        .context("failed to read QEMU CCA initrd metadata")?
        .len();
    let initrd_end = initrd_address
        .checked_add(initrd_size)
        .context("QEMU CCA initrd address overflow")?;
    let kernel_address = config
        .kernel_load_address
        .context("QEMU CCA kernel load address is missing")?;
    anyhow::ensure!(
        initrd_end <= kernel_address,
        "QEMU CCA initrd range {initrd_address:#x}..{initrd_end:#x} overlaps \
         the raw kernel at {kernel_address:#x}"
    );
    cmd.arg("-device").arg(format!(
        "loader,file={initrd},addr={initrd_address:#x},force-raw=on"
    ));
    cmd.arg("-virtfs").arg(format!(
        "local,path={share_dir},mount_tag=host,security_model=none,readonly=off"
    ));
    cmd.arg("-netdev").arg(format!(
        "user,id=net0,hostfwd=tcp:127.0.0.1:{host_pipette_port}-:{guest_port}",
        guest_port = pipette_client::PIPETTE_PORT,
    ));
    cmd.arg("-device")
        .arg("virtio-net-pci,netdev=net0,romfile=");
    cmd.arg("-append").arg(format!(
        "nokaslr console=ttyAMA0 rdinit=/cca-init.sh \
         initrd={initrd_address:#x},{initrd_size:#x}"
    ));
    cmd.arg("-display").arg("none");
    cmd.arg("-monitor").arg("none");
    cmd.arg("-no-reboot");
    for extra_arg in &config.extra_args {
        match extra_arg {
            QemuCcaExtraArg::Trace { value } => {
                cmd.arg("-d").arg(value);
            }
            QemuCcaExtraArg::Global { value } => {
                cmd.arg("-global").arg(value);
            }
        }
    }
    let mut console_logs = BTreeMap::new();
    for console in &config.consoles {
        let log_path = output_dir.join(format!("incubator-{console}.{instance_id}.log"));
        if console == &config.primary_console {
            cmd.arg("-serial").arg("stdio");
        } else {
            let log_path_arg = qemu_option_path(&log_path, "QEMU CCA console log")?;
            cmd.arg("-serial").arg(format!("file:{log_path_arg}"));
        }
        console_logs.insert(console.clone(), log_path);
    }
    Ok(QemuCcaCommand {
        command: cmd,
        console_logs,
    })
}

fn qemu_option_path<'a>(path: &'a Path, label: &str) -> anyhow::Result<&'a str> {
    let path = path
        .to_str()
        .with_context(|| format!("{label} is not valid UTF-8"))?;
    anyhow::ensure!(
        !path.contains([',', '\n', '\r']),
        "{label} contains a character that cannot be represented in a QEMU option"
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn qemu_cca_config() -> QemuCcaConfig {
        let profile =
            crate::IncubatorProfile::from_toml(include_str!("../profiles/aarch64-qemu-cca.toml"))
                .unwrap();
        let crate::IncubatorBackend::QemuCca(config) = profile.incubator else {
            panic!("expected QEMU CCA profile");
        };
        config
    }

    #[test]
    fn builds_qemu_cca_command_with_named_consoles() {
        let initrd = tempfile::NamedTempFile::new_in(".").unwrap();
        let built = build_qemu_cca_command(
            &qemu_cca_config(),
            Path::new("/artifacts/flash.bin"),
            Path::new("/artifacts/host-Image"),
            initrd.path(),
            Path::new("/share"),
            50000,
            Path::new("/logs"),
            42,
        )
        .unwrap();
        let args: Vec<_> = built
            .command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|args| args == ["-bios", "/artifacts/flash.bin"])
        );
        assert!(
            args.windows(2)
                .any(|args| args == ["-kernel", "/artifacts/host-Image"])
        );
        assert!(args.iter().any(|arg| arg.contains("rdinit=/cca-init.sh")));
        assert!(
            args.iter()
                .any(|arg| arg.contains("hostfwd=tcp:127.0.0.1:50000-:"))
        );
        assert!(args.windows(2).any(|args| args == ["-serial", "stdio"]));
        assert!(
            args.windows(2)
                .any(|args| args == ["-serial", "file:/logs/incubator-secure.42.log"])
        );
        assert_eq!(
            built.console_logs["host"],
            PathBuf::from("/logs/incubator-host.42.log")
        );
        assert!(!args.iter().any(|arg| arg.contains("id=rootfs")));
    }

    #[test]
    fn rejects_unrepresentable_qemu_cca_option_path() {
        let error = build_qemu_cca_command(
            &qemu_cca_config(),
            Path::new("/artifacts/flash.bin"),
            Path::new("/artifacts/host-Image"),
            Path::new("/artifacts/initrd"),
            Path::new("/share,unsafe"),
            50000,
            Path::new("/logs"),
            42,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("cannot be represented"));
    }

    #[test]
    fn rejects_qemu_cca_initrd_overlapping_kernel() {
        let initrd = tempfile::NamedTempFile::new_in(".").unwrap();
        initrd.as_file().set_len(0x08080001).unwrap();
        let error = build_qemu_cca_command(
            &qemu_cca_config(),
            Path::new("/artifacts/flash.bin"),
            Path::new("/artifacts/host-Image"),
            initrd.path(),
            Path::new("/share"),
            50000,
            Path::new("/logs"),
            42,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("overlaps the raw kernel"));
    }

    #[test]
    fn preserves_explicit_capabilities_without_runtime_capabilities() {
        let mut env = BTreeMap::from([("PETRI_CAPABILITIES".to_string(), "explicit".to_string())]);
        publish_capabilities(&mut env, &[]);
        assert!(
            env["PETRI_CAPABILITIES"]
                .split(',')
                .any(|value| value == "explicit")
        );
    }
}
