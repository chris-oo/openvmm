// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run a command using Petri's VM lifecycle and Pipette connection.

use crate::profile::IncubatorBackend;
use crate::profile::IncubatorBoot;
use crate::profile::IncubatorProfile;
use crate::profile::parse_size;
use crate::qemu;
use anyhow::Context;
use futures_concurrency::future::Race;
use petri::Firmware;
use petri::PetriLogSource;
use petri::PetriVmArtifacts;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri_artifacts_common::tags::MachineArch;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

/// Configuration for one disposable VM and guest command.
pub struct IncubatorConfig {
    /// Hardware and boot profile.
    pub profile: IncubatorProfile,
    /// Root VM kernel, not the kernel of any nested test.
    pub kernel: PathBuf,
    /// Root VM initrd.
    pub initrd: PathBuf,
    /// Host directory mounted at `/share`.
    pub share_dir: PathBuf,
    /// Host directory for persistent logs.
    pub output_dir: PathBuf,
    /// Guest Pipette path under `/share`.
    pub guest_pipette_path: String,
    /// Program and arguments to execute.
    pub guest_command: Vec<String>,
    /// Guest environment overrides.
    pub guest_env: BTreeMap<String, String>,
    /// Guest working directory.
    pub guest_current_dir: Option<String>,
    /// Boot timeout for Linux direct; boot and command timeout for mshv.
    pub timeout: Duration,
    /// Override the QEMU executable.
    pub qemu_binary_override: Option<PathBuf>,
    /// Allocate a guest PTY and forward terminal control characters.
    pub allocate_pty: bool,
}

/// Configuration for the diagnostic QEMU CCA launcher.
pub struct QemuCcaIncubatorConfig {
    /// QEMU CCA hardware profile with Linux-direct boot.
    pub profile: IncubatorProfile,
    /// CCA host kernel.
    pub kernel: PathBuf,
    /// CCA platform firmware.
    pub firmware: PathBuf,
    /// Base host initrd.
    pub initrd: PathBuf,
    /// Host directory mounted at `/share`.
    pub share_dir: PathBuf,
    /// Host directory for persistent logs.
    pub output_dir: PathBuf,
    /// Guest Pipette path under `/share`.
    pub guest_pipette_path: String,
    /// Program and arguments to execute.
    pub guest_command: Vec<String>,
    /// Guest environment overrides.
    pub guest_env: BTreeMap<String, String>,
    /// Guest working directory.
    pub guest_current_dir: Option<String>,
    /// Boot and agent connection timeout.
    pub timeout: Duration,
    /// Override the QEMU executable.
    pub qemu_binary_override: Option<PathBuf>,
    /// Allocate a guest PTY.
    pub allocate_pty: bool,
}

/// Runtime inputs for an opt-in licensed FVP CCA incubator.
pub struct FvpCcaIncubatorConfig {
    /// Validated typed FVP profile.
    pub profile: IncubatorProfile,
    /// Unified official CCA kernel used by the L1 host.
    pub kernel: PathBuf,
    /// Immutable official base initrd.
    pub initrd: PathBuf,
    /// Prepared guest input tree; snapshotted before launch.
    pub share_dir: PathBuf,
    /// Durable host-side results directory, not a Shrinkwrap workspace.
    pub output_dir: PathBuf,
    /// Optional fresh direct child of the canonical output directory.
    pub result_file: Option<PathBuf>,
    /// Pipette location under the guest share.
    pub guest_pipette_path: String,
    /// Guest command and arguments; host programs are mapped under `/share`.
    pub guest_command: Vec<String>,
    /// Mapped per-command guest environment.
    pub guest_env: BTreeMap<String, String>,
    /// Guest working directory.
    pub guest_current_dir: Option<String>,
    /// User-provisioned Shrinkwrap checkout and overlay root.
    pub platform_root: PathBuf,
    /// User-provisioned pinned FVP firmware package root.
    pub shrinkwrap_package_root: PathBuf,
}

impl TryFrom<FvpCcaIncubatorConfig> for petri::fvp::FvpRunConfig {
    type Error = anyhow::Error;

    fn try_from(config: FvpCcaIncubatorConfig) -> anyhow::Result<Self> {
        config.profile.validate()?;
        let IncubatorBackend::FvpCca(profile) = config.profile.incubator else {
            anyhow::bail!("run_in_fvp_cca_incubator requires an FVP CCA profile");
        };
        Ok(Self {
            profile,
            kernel: config.kernel,
            initrd: config.initrd,
            share_dir: config.share_dir,
            output_dir: config.output_dir,
            result_file: config.result_file,
            guest_pipette_path: config.guest_pipette_path,
            guest_command: config.guest_command,
            guest_env: config.guest_env,
            guest_current_dir: config.guest_current_dir,
            platform_root: config.platform_root,
            shrinkwrap_package_root: config.shrinkwrap_package_root,
            nested_test_parent: None,
        })
    }
}

/// Run a command through Petri's checked FVP lifecycle.
pub fn run_in_fvp_cca_incubator(config: FvpCcaIncubatorConfig) -> anyhow::Result<IncubatorOutput> {
    let config = petri::fvp::FvpRunConfig::try_from(config)?;
    #[cfg(target_os = "linux")]
    {
        let output = petri::fvp::runtime::run(config)?;
        Ok(IncubatorOutput {
            exit_code: output.exit_code,
            elapsed: output.elapsed,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        anyhow::bail!("FVP CCA incubators require a Linux host")
    }
}

/// Run the diagnostic QEMU CCA platform without changing the general launcher.
pub fn run_in_qemu_cca_incubator(
    config: QemuCcaIncubatorConfig,
) -> anyhow::Result<IncubatorOutput> {
    config.profile.validate()?;
    anyhow::ensure!(
        matches!(config.profile.incubator, IncubatorBackend::QemuCca(_)),
        "run_in_qemu_cca_incubator requires a QEMU CCA profile"
    );
    anyhow::ensure!(!config.guest_command.is_empty(), "empty guest command");
    crate::qemu_cca::run(config)
}

/// Guest command result.
pub struct IncubatorOutput {
    /// Guest exit code, including `128 + signal` for signal termination.
    pub exit_code: Option<i32>,
    /// Total wall time.
    pub elapsed: Duration,
}

/// Cold-boot a VM, execute the command through Pipette, and tear down the VM.
pub fn run_in_incubator(config: IncubatorConfig) -> anyhow::Result<IncubatorOutput> {
    let start = Instant::now();
    config.profile.validate()?;
    anyhow::ensure!(
        matches!(
            config.profile.incubator,
            IncubatorBackend::QemuTcg(_) | IncubatorBackend::Openvmm(_)
        ),
        "CCA profiles require their dedicated launcher"
    );
    let signals = TerminationSignals::new()?;
    anyhow::ensure!(!config.guest_command.is_empty(), "empty guest command");
    if matches!(config.profile.incubator, IncubatorBackend::Openvmm(_)) {
        anyhow::ensure!(
            config.qemu_binary_override.is_none(),
            "--qemu-binary is not valid for the openvmm launcher"
        );
        anyhow::ensure!(
            cfg!(target_os = "linux") && MachineArch::host() == MachineArch::X86_64,
            "OpenVMM Incubator requires an x86-64 Linux host"
        );
    }
    fs_err::create_dir_all(&config.output_dir)?;
    let log_dir = tempfile::Builder::new()
        .prefix("incubator-")
        .tempdir_in(&config.output_dir)?
        .keep();
    let logs = PetriLogSource::file_only(&log_dir)?;
    tracing::info!(path = %log_dir.display(), "incubator logs");
    let initrd = crate::bootstrap::prepare_initrd(&config)?;
    let firmware = match &config.profile.boot {
        IncubatorBoot::LinuxDirect => Firmware::LinuxDirect {
            kernel: ResolvedArtifact::from_local_path(config.kernel.clone()),
            initrd: ResolvedArtifact::from_local_path(config.initrd.clone()),
        },
        IncubatorBoot::Mshv(boot) => Firmware::Uefi {
            uefi_firmware: ResolvedArtifact::from_local_path(boot.firmware.clone()),
            guest: petri::UefiGuest::Mshv(petri::mshv_boot::MshvBoot {
                host_boot: ResolvedArtifact::from_local_path(boot.host_boot.clone()),
                kernel: ResolvedArtifact::from_local_path(config.kernel.clone()),
                initrd: initrd.clone(),
                boot_artifacts: ResolvedArtifact::from_local_path(boot.boot_artifacts.clone()),
                kernel_cmdline: boot.kernel_cmdline.clone(),
                hypervisor_cmdline: boot.hypervisor_cmdline.clone(),
            }),
            uefi_config: petri::UefiConfig {
                default_boot_always_attempt: true,
                ..Default::default()
            },
        },
    };
    let bound_command = matches!(config.profile.boot, IncubatorBoot::Mshv(_));
    let code = pal_async::DefaultPool::run_with(async |driver| match &config.profile.incubator {
        IncubatorBackend::Openvmm(profile) => {
            let artifacts = PetriVmArtifacts {
                backend: petri::openvmm::OpenVmmPetriBackend::from_binary(profile.binary.clone()),
                firmware,
                arch: MachineArch::X86_64,
                agent_image: None,
                openhcl_agent_image: None,
                pipette_binary: None,
                nested_test: None,
            };
            let mut builder = PetriVmBuilder::minimal("incubator", &logs, artifacts, &driver)?
                .with_memory(petri::MemoryConfig {
                    startup_bytes: parse_size(&profile.memory)?,
                    ..Default::default()
                })
                .with_processor_topology(petri::ProcessorTopology {
                    vp_count: profile.processors,
                    enable_smt: profile.enable_smt,
                    ..Default::default()
                })
                .with_external_pipette()
                .with_shared_directory("hostshare", &config.share_dir, crate::GUEST_SHARE_ROOT);
            if profile.nested_virt {
                builder = builder.with_nested_virt();
            }
            builder = match config.profile.boot {
                IncubatorBoot::Mshv(_) => builder.with_mshv_platform(),
                IncubatorBoot::LinuxDirect => builder
                    .with_prebuilt_initrd(initrd)
                    .with_no_vmbus()
                    .with_virtio_vsock(true)
                    .with_virtio_console()
                    .with_serial_output()
                    .modify_backend(|backend| {
                        backend
                            .with_pcie_root_topology_at(0, 1, 1, 1)
                            .with_virtio_nic("s0rc0rp0", petri::openvmm::NIC_MAC_ADDRESS)
                    }),
            };
            execute(builder, &driver, &config, &signals, bound_command).await
        }
        IncubatorBackend::QemuTcg(profile) => {
            let artifacts = PetriVmArtifacts {
                backend: petri::qemu::QemuPetriBackend::from_binary(
                    config
                        .qemu_binary_override
                        .clone()
                        .unwrap_or_else(|| profile.binary.clone().into()),
                ),
                firmware,
                arch: MachineArch::Aarch64,
                agent_image: None,
                openhcl_agent_image: None,
                pipette_binary: None,
                nested_test: None,
            };
            let devices = qemu::petri_devices(&config.profile.devices)?;
            let profile = profile.clone();
            let share = config.share_dir.clone();
            let builder = PetriVmBuilder::minimal("incubator", &logs, artifacts, &driver)?
                .with_external_pipette()
                .with_prebuilt_initrd(initrd)
                .modify_backend(move |backend| {
                    backend
                        .with_share_9p(share)
                        .with_devices(devices)
                        .with_platform(
                            profile.machine,
                            profile.cpu,
                            profile.smp,
                            profile.memory,
                            profile.cmdline,
                        )
                });
            execute(builder, &driver, &config, &signals, bound_command).await
        }
        IncubatorBackend::QemuCca(_) | IncubatorBackend::FvpCca(_) => {
            anyhow::bail!("CCA profiles require their dedicated launcher")
        }
    })?;
    Ok(IncubatorOutput {
        exit_code: Some(code),
        elapsed: start.elapsed(),
    })
}

async fn execute<T: PetriVmmBackend>(
    builder: PetriVmBuilder<T>,
    driver: &pal_async::DefaultDriver,
    config: &IncubatorConfig,
    signals: &TerminationSignals,
    bound_command: bool,
) -> anyhow::Result<i32> {
    let start = Instant::now();
    signals.check()?;
    let boot = builder
        .with_watchdog_timeout(None)
        .without_screenshots()
        .run();
    let (mut vm, client) = (
        boot,
        timeout(driver, config.timeout, "boot"),
        signals.cancelled(driver),
    )
        .race()
        .await?;
    let session = async {
        let vfio_env = qemu::setup_vfio_devices(&client, &config.profile.devices).await?;
        let mut command = client.command(&config.guest_command[0]);
        command.args(&config.guest_command[1..]);
        for (key, value) in config.guest_env.iter().chain(vfio_env.iter()) {
            command.env(key, value);
        }
        command.current_dir(
            config
                .guest_current_dir
                .as_deref()
                .unwrap_or(crate::GUEST_SHARE_ROOT),
        );
        command.pty(config.allocate_pty);
        let guard = config.allocate_pty.then(RawModeGuard::enter).transpose()?;
        let status = command.spawn().await?.wait().await?;
        drop(guard);
        status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .context("guest command returned no exit code or signal")
    };
    let result = (
        session,
        async {
            let halt = vm.wait_for_halt().await?;
            anyhow::bail!("incubator halted before command completed: {halt:?}")
        },
        async {
            if bound_command {
                timeout(
                    driver,
                    config.timeout.saturating_sub(start.elapsed()),
                    "command",
                )
                .await
            } else {
                std::future::pending().await
            }
        },
        signals.cancelled(driver),
    )
        .race()
        .await;
    let result = match result {
        Ok(code) => {
            let shutdown = async {
                client.power_off().await?;
                vm.wait_for_clean_shutdown().await?;
                Ok(code)
            };
            (
                shutdown,
                timeout(driver, Duration::from_secs(10), "shutdown"),
            )
                .race()
                .await
        }
        Err(error) => Err(error),
    };
    let teardown = (
        vm.teardown(),
        timeout(driver, Duration::from_secs(10), "teardown"),
    )
        .race()
        .await;
    if let Err(error) = &teardown {
        tracing::error!(%error, "incubator teardown failed");
    }
    let code = result?;
    teardown?;
    Ok(code)
}

pub(crate) struct TerminationSignals {
    #[cfg(unix)]
    signal: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(unix)]
    registrations: Vec<signal_hook::SigId>,
}

impl TerminationSignals {
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            let signal = self.signal.load(std::sync::atomic::Ordering::Relaxed);
            anyhow::ensure!(signal == 0, "incubator interrupted by signal {signal}");
        }
        Ok(())
    }

    pub(crate) fn new() -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            let mut guard = Self {
                signal: Default::default(),
                registrations: Vec::new(),
            };
            for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
                guard.registrations.push(signal_hook::flag::register_usize(
                    signal,
                    guard.signal.clone(),
                    signal as usize,
                )?);
            }
            Ok(guard)
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    pub(crate) async fn cancelled<T>(
        &self,
        driver: &pal_async::DefaultDriver,
    ) -> anyhow::Result<T> {
        #[cfg(unix)]
        loop {
            self.check()?;
            pal_async::timer::PolledTimer::new(driver)
                .sleep(Duration::from_millis(100))
                .await;
        }
        #[cfg(not(unix))]
        {
            let _ = driver;
            std::future::pending().await
        }
    }
}

impl Drop for TerminationSignals {
    fn drop(&mut self) {
        #[cfg(unix)]
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

pub(crate) async fn timeout<T>(
    driver: &pal_async::DefaultDriver,
    duration: Duration,
    phase: &str,
) -> anyhow::Result<T> {
    pal_async::timer::PolledTimer::new(driver)
        .sleep(duration)
        .await;
    anyhow::bail!("incubator {phase} timed out after {duration:?}")
}

pub(crate) struct RawModeGuard;

impl RawModeGuard {
    pub(crate) fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode().context("failed to enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Err(error) = crossterm::terminal::disable_raw_mode() {
            tracing::warn!(%error, "failed to restore terminal mode");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn fvp_config(text: &str) -> FvpCcaIncubatorConfig {
        FvpCcaIncubatorConfig {
            profile: IncubatorProfile::from_toml(text).unwrap(),
            kernel: "kernel".into(),
            initrd: "initrd".into(),
            share_dir: "share".into(),
            output_dir: "output".into(),
            result_file: Some("output/result.json".into()),
            guest_pipette_path: "/share/pipette".into(),
            guest_command: vec!["/share/command".into()],
            guest_env: BTreeMap::from([("TEST".into(), "value".into())]),
            guest_current_dir: Some("/share/work".into()),
            platform_root: "platform".into(),
            shrinkwrap_package_root: "package".into(),
        }
    }

    #[test]
    fn fvp_adapter_preserves_runtime_inputs_and_rejects_other_backends() {
        let input = fvp_config(include_str!("../profiles/aarch64-fvp-cca.toml"));
        let output = petri::fvp::FvpRunConfig::try_from(input).unwrap();
        assert_eq!(output.profile.capabilities, ["cca"]);
        assert_eq!(output.result_file, Some("output/result.json".into()));
        assert_eq!(output.guest_command, ["/share/command"]);
        assert_eq!(output.guest_env["TEST"], "value");
        assert_eq!(output.guest_current_dir.as_deref(), Some("/share/work"));
        assert_eq!(output.platform_root, PathBuf::from("platform"));
        assert_eq!(output.shrinkwrap_package_root, PathBuf::from("package"));
        let input = fvp_config(include_str!("../profiles/aarch64-qemu-cca.toml"));
        assert!(petri::fvp::FvpRunConfig::try_from(input).is_err());
    }

    #[test]
    fn general_bootstrap_rejects_cca_before_reading_artifacts() {
        for text in [
            include_str!("../profiles/aarch64-fvp-cca.toml"),
            include_str!("../profiles/aarch64-qemu-cca.toml"),
        ] {
            let fvp = fvp_config(text);
            let config = IncubatorConfig {
                profile: fvp.profile,
                kernel: fvp.kernel,
                initrd: fvp.initrd,
                share_dir: fvp.share_dir,
                output_dir: fvp.output_dir,
                guest_pipette_path: fvp.guest_pipette_path,
                guest_command: fvp.guest_command,
                guest_env: fvp.guest_env,
                guest_current_dir: fvp.guest_current_dir,
                timeout: Duration::from_secs(300),
                qemu_binary_override: None,
                allocate_pty: false,
            };
            let error = match crate::bootstrap::prepare_initrd(&config) {
                Ok(_) => panic!("CCA used the general bootstrap"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("CCA host bootstrap"));
            let error = match run_in_incubator(config) {
                Ok(_) => panic!("CCA used the general runner"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("dedicated launcher"));
        }
    }
}
