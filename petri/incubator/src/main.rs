// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runner for executing test commands in a disposable Linux guest.
//!
//! Incubator reads a TOML hardware profile, boots a VM with a shared
//! host directory, connects to the in-guest Pipette agent, and returns the
//! requested command's exit status. It is used for configurations such as
//! AArch64 PCIe, IOMMU, and device-assignment tests that need hardware models
//! outside OpenVMM's device surface.
//!
//! Configuration is available through flags and `INCUBATOR_*` environment
//! variables. Launch directly with
//! `incubator --profile <profile> --share <directory> <command> [args...]` for
//! focused debugging.

#![forbid(unsafe_code)]

use anyhow::Context;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;

mod fvp_inputs;

/// Standalone CLI for launching the incubator.
///
/// Options can also be supplied via their `INCUBATOR_*` environment variables.
/// Registered nested VMM tests use Petri directly, not this CLI.
#[derive(Parser)]
#[clap(
    after_long_help = "Checked native FVP recovery without launching a model:\n  incubator recover-fvp-native --help"
)]
struct Args {
    /// Path to a TOML profile file.
    #[clap(long, env = "INCUBATOR_PROFILE")]
    profile: String,
    /// Path to the kernel image (auto-detected if omitted).
    #[clap(long, env = "INCUBATOR_KERNEL")]
    kernel: Option<PathBuf>,
    /// Path to the initrd (auto-detected if omitted).
    #[clap(long, env = "INCUBATOR_INITRD")]
    initrd: Option<PathBuf>,
    /// Path to the platform firmware image.
    #[clap(long, env = "INCUBATOR_FIRMWARE")]
    firmware: Option<PathBuf>,
    /// Root containing the provisioned Shrinkwrap checkout for FVP CCA.
    #[clap(long, env = "INCUBATOR_FVP_PLATFORM_ROOT")]
    fvp_platform_root: Option<PathBuf>,
    /// Prepared FVP input inventory, separate from the host path-mapping share.
    #[clap(long, env = "INCUBATOR_FVP_INPUT_SHARE")]
    fvp_input_share: Option<PathBuf>,
    /// Root containing the pinned Shrinkwrap FVP firmware package.
    #[clap(long, env = "INCUBATOR_SHRINKWRAP_PACKAGE_ROOT")]
    shrinkwrap_package_root: Option<PathBuf>,
    /// Directory to share with the guest.
    #[clap(long, env = "INCUBATOR_SHARE")]
    share: String,
    /// Host directory for logs and captured output.
    #[clap(long, env = "INCUBATOR_OUTPUT_DIR")]
    output_dir: Option<PathBuf>,
    /// Fresh result file directly under the canonical FVP output directory.
    /// Relative paths are resolved from the host working directory.
    #[clap(long, env = "INCUBATOR_FVP_RESULT_FILE")]
    fvp_result_file: Option<PathBuf>,
    /// Guest path to the pipette binary.
    #[clap(long, env = "INCUBATOR_GUEST_PIPETTE")]
    guest_pipette: Option<String>,
    /// Environment variable to set for the guest command, as KEY=VALUE.
    #[clap(long = "guest-env", value_name = "KEY=VALUE")]
    guest_env: Vec<GuestEnv>,
    /// Map the command path from the host share to the guest share.
    #[clap(long, env = "INCUBATOR_MAP_COMMAND_PATH")]
    map_command_path: bool,
    /// Working directory for the guest command.
    #[clap(long, env = "INCUBATOR_GUEST_CURRENT_DIR")]
    guest_current_dir: Option<String>,
    /// Override the QEMU binary path from the profile.
    #[clap(long, env = "INCUBATOR_QEMU_BINARY")]
    qemu_binary: Option<PathBuf>,
    /// Do not allocate a PTY for the guest command or put the host terminal
    /// into raw mode. Required for FVP.
    #[clap(long, env = "INCUBATOR_NO_PTY")]
    no_pty: bool,
    /// Timeout in seconds: boot for Linux direct, boot and command for mshv.
    #[clap(long, env = "INCUBATOR_TIMEOUT", default_value_t = 300)]
    timeout: u64,
    /// Command to run in the guest: the program followed by its arguments.
    /// Use --map-command-path for a host program under the share. FVP also
    /// accepts absolute guest-resident commands.
    ///
    /// May be preceded by `--`, but it is not required.
    #[clap(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    command: Vec<String>,
}

/// Retire only the explicitly authorized native run after its original owner exits.
#[derive(Parser)]
#[clap(
    name = "incubator recover-fvp-native",
    bin_name = "incubator recover-fvp-native"
)]
struct FvpRecoveryArgs {
    /// Pinned Shrinkwrap toolchain root.
    #[clap(long)]
    platform_root: PathBuf,
    /// Extracted qualified portable FVP firmware package.
    #[clap(long)]
    package_root: PathBuf,
    /// JSON with run_id, full recorded owner, and independently captured owner_sha256.
    /// The original executable must still exist at its recorded canonical path.
    #[clap(long)]
    evidence: PathBuf,
}

fn recover_fvp_native() -> anyhow::Result<()> {
    let args = FvpRecoveryArgs::parse_from(
        std::iter::once(std::env::args_os().next().unwrap_or_default())
            .chain(std::env::args_os().skip(2)),
    );
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        let mut data = Vec::new();
        std::fs::File::open(&args.evidence)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut data)?;
        anyhow::ensure!(
            data.len() <= 64 * 1024,
            "FVP recovery evidence is too large"
        );
        let evidence: incubator::fvp::lifecycle::RecoveryEvidence =
            serde_json::from_slice(&data).context("invalid FVP recovery evidence")?;
        incubator::fvp::runtime::recover_native(&args.platform_root, &args.package_root, &evidence)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        anyhow::bail!("native FVP recovery requires Linux")
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("recover-fvp-native")) {
        return recover_fvp_native();
    }

    let matches = Args::command().get_matches();
    let args = Args::from_arg_matches(&matches)?;
    let share_dir = PathBuf::from(&args.share);
    let path_mapper = incubator::HostPathMapper::new(&share_dir, incubator::GUEST_SHARE_ROOT)?;
    let mut profile = incubator::IncubatorProfile::from_file(std::path::Path::new(&args.profile))?;
    configure_profile(
        &mut profile,
        &args,
        matches.value_source("timeout") != Some(clap::parser::ValueSource::DefaultValue),
    )?;
    let guest_pipette = args
        .guest_pipette
        .unwrap_or_else(|| format!("{}/pipette", incubator::GUEST_SHARE_ROOT));
    let (kernel, initrd, firmware) = match &profile.incubator {
        incubator::IncubatorBackend::FvpCca(_) => (
            Some(
                args.kernel
                    .context("FVP requires --kernel or INCUBATOR_KERNEL")?,
            ),
            Some(
                args.initrd
                    .context("FVP requires --initrd or INCUBATOR_INITRD")?,
            ),
            None,
        ),
        incubator::IncubatorBackend::QemuTcg(_) | incubator::IncubatorBackend::Openvmm(_) => {
            let arch = profile.incubator.arch();
            let kernel = match args.kernel {
                Some(kernel) => kernel,
                None if matches!(profile.boot, incubator::IncubatorBoot::Mshv(_)) => {
                    anyhow::bail!(
                        "mshv boot requires an explicit --kernel / INCUBATOR_KERNEL with mshv EFI launch support"
                    )
                }
                None => kernel_or_initrd_from_env(arch, "OPENVMM_LINUX_DIRECT_KERNEL")?,
            };
            let initrd = match args.initrd {
                Some(initrd) => initrd,
                None => kernel_or_initrd_from_env(arch, "OPENVMM_LINUX_DIRECT_INITRD")?,
            };
            (Some(kernel), Some(initrd), None)
        }
        incubator::IncubatorBackend::QemuCca(_) => (
            Some(
                args.kernel
                    .context("--kernel or INCUBATOR_KERNEL is required for QEMU CCA")?,
            ),
            Some(
                args.initrd
                    .context("--initrd or INCUBATOR_INITRD is required for QEMU CCA")?,
            ),
            Some(
                args.firmware
                    .context("--firmware or INCUBATOR_FIRMWARE is required for QEMU CCA")?,
            ),
        ),
    };

    tracing::info!(profile = %args.profile, "profile");
    if let Some(kernel) = &kernel {
        tracing::info!(kernel = %kernel.display(), "kernel");
    }
    if let Some(initrd) = &initrd {
        tracing::info!(initrd = %initrd.display(), "initrd");
    }
    if let Some(firmware) = &firmware {
        tracing::info!(firmware = %firmware.display(), "firmware");
    }
    let mut command = args.command;
    if args.map_command_path {
        let host_command = command.first().context("empty guest command")?.clone();
        command[0] = path_mapper.map_path(&host_command).with_context(|| {
            format!("failed to map command path '{host_command}' into the guest share")
        })?;
    }

    tracing::info!(share = %share_dir.display(), "share");
    tracing::info!(command = ?command, "command");

    let mut guest_env = guest_env_from_incubator_env(&path_mapper)?;
    for env in args.guest_env {
        guest_env.insert(env.key, env.value);
    }

    let output_dir = args
        .output_dir
        .unwrap_or_else(|| share_dir.join("test_results"));
    let timeout = std::time::Duration::from_secs(args.timeout);
    let allocate_pty = !args.no_pty && std::io::stdin().is_terminal();
    let output = match profile.incubator {
        incubator::IncubatorBackend::FvpCca(ref config) => {
            let inputs = fvp_inputs::prepare(
                &share_dir,
                args.fvp_input_share.as_deref(),
                if args.map_command_path {
                    Some(command.first().context("empty guest command")?.as_str())
                } else {
                    None
                },
                std::time::Duration::from_secs(config.deadlines.validation),
            )?;
            incubator::run_in_fvp_cca_incubator(incubator::FvpCcaIncubatorConfig {
                profile,
                kernel: kernel.context("missing FVP kernel")?,
                initrd: initrd.context("missing FVP initrd")?,
                share_dir: inputs.path.clone(),
                output_dir,
                result_file: args.fvp_result_file,
                guest_pipette_path: guest_pipette,
                guest_command: command,
                guest_env,
                guest_current_dir: args.guest_current_dir,
                platform_root: args
                    .fvp_platform_root
                    .context("FVP requires --fvp-platform-root or INCUBATOR_FVP_PLATFORM_ROOT")?,
                shrinkwrap_package_root: args.shrinkwrap_package_root.context(
                    "FVP requires --shrinkwrap-package-root or INCUBATOR_SHRINKWRAP_PACKAGE_ROOT",
                )?,
            })?
        }
        incubator::IncubatorBackend::QemuTcg(_) | incubator::IncubatorBackend::Openvmm(_) => {
            incubator::run_in_incubator(incubator::IncubatorConfig {
                profile,
                kernel: kernel.context("missing guest kernel")?,
                initrd: initrd.context("missing guest initrd")?,
                share_dir,
                output_dir,
                guest_pipette_path: guest_pipette,
                guest_command: command,
                guest_env,
                guest_current_dir: args.guest_current_dir,
                timeout,
                qemu_binary_override: args.qemu_binary,
                allocate_pty,
            })?
        }
        incubator::IncubatorBackend::QemuCca(_) => {
            incubator::run_in_qemu_cca_incubator(incubator::QemuCcaIncubatorConfig {
                profile,
                kernel: kernel.context("missing QEMU CCA kernel")?,
                firmware: firmware.context("missing QEMU CCA firmware")?,
                initrd: initrd.context("missing QEMU CCA initrd")?,
                share_dir,
                output_dir,
                guest_pipette_path: guest_pipette,
                guest_command: command,
                guest_env,
                guest_current_dir: args.guest_current_dir,
                timeout,
                qemu_binary_override: args.qemu_binary,
                allocate_pty,
            })?
        }
    };

    tracing::info!(
        elapsed_secs = output.elapsed.as_secs_f64(),
        exit_code = ?output.exit_code,
        "completed"
    );

    std::process::exit(output.exit_code.unwrap_or(1));
}

fn configure_profile(
    profile: &mut incubator::IncubatorProfile,
    args: &Args,
    timeout_explicit: bool,
) -> anyhow::Result<()> {
    if let incubator::IncubatorBackend::FvpCca(config) = &mut profile.incubator {
        if timeout_explicit {
            config.deadlines.test_execution = args.timeout;
        }
        anyhow::ensure!(
            args.firmware.is_none() && args.qemu_binary.is_none() && args.no_pty,
            "FVP requires --no-pty and does not accept QEMU firmware or binary overrides"
        );
    } else {
        anyhow::ensure!(
            args.fvp_platform_root.is_none()
                && args.fvp_input_share.is_none()
                && args.shrinkwrap_package_root.is_none()
                && args.fvp_result_file.is_none(),
            "FVP platform roots and result file require an FVP CCA profile"
        );
        anyhow::ensure!(
            args.firmware.is_none()
                || matches!(profile.incubator, incubator::IncubatorBackend::QemuCca(_)),
            "--firmware requires a QEMU CCA profile; mshv firmware belongs in [boot]"
        );
        if matches!(profile.incubator, incubator::IncubatorBackend::Openvmm(_)) {
            anyhow::ensure!(
                args.qemu_binary.is_none(),
                "--qemu-binary is not valid for the openvmm launcher"
            );
        }
    }
    profile.validate()
}

fn guest_env_from_incubator_env(
    path_mapper: &incubator::HostPathMapper,
) -> anyhow::Result<BTreeMap<String, String>> {
    let policy = match std::env::var("INCUBATOR_ENV") {
        Ok(policy) => policy,
        Err(std::env::VarError::NotPresent) => return Ok(BTreeMap::new()),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("INCUBATOR_ENV is not valid UTF-8")
        }
    };

    let host_env = std::env::vars().collect();
    incubator::guest_env_from_incubator_env(&policy, &host_env, path_mapper)
        .context("failed to apply INCUBATOR_ENV")
}

#[derive(Clone)]
struct GuestEnv {
    key: String,
    value: String,
}

impl std::str::FromStr for GuestEnv {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (key, value) = s
            .split_once('=')
            .ok_or_else(|| "expected KEY=VALUE".to_string())?;
        if key.is_empty() {
            return Err("environment variable name must not be empty".to_string());
        }
        Ok(Self {
            key: key.to_string(),
            value: value.to_string(),
        })
    }
}

/// Resolve a kernel or initrd path from an environment variable.
///
/// Mimics openvmm's lookup: given a base name like `OPENVMM_LINUX_DIRECT_KERNEL`,
/// it checks the unprefixed variable first, then the arch-specific variant
/// (e.g. `AARCH64_OPENVMM_LINUX_DIRECT_KERNEL`). The arch comes from the
/// profile. These variables are set by the repo's `.cargo/config.toml` so that
/// `cargo run` picks up the sample kernel/initrd packaged alongside
/// openvmm-deps. If neither is set, fail with a hint to pass the path
/// explicitly.
fn kernel_or_initrd_from_env(arch: incubator::Arch, base_name: &str) -> anyhow::Result<PathBuf> {
    let prefixed = format!("{}_{base_name}", arch.env_prefix());
    let value = non_empty_env(base_name).or_else(|| non_empty_env(&prefixed));
    match value {
        Some(value) => Ok(PathBuf::from(value)),
        None => anyhow::bail!(
            "neither {base_name} nor {prefixed} is set (normally provided by \
             .cargo/config.toml); pass --kernel/--initrd explicitly or run via \
             cargo from the repo"
        ),
    }
}

/// Read an environment variable, treating an empty value as unset.
fn non_empty_env(var: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(var).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn backend_specific_flags_and_fvp_timeout_are_checked_before_launch() {
        let mut args = Args::try_parse_from([
            "incubator",
            "--profile",
            "profile.toml",
            "--share",
            ".",
            "--no-pty",
            "--",
            "/share/runner",
        ])
        .unwrap();
        let mut fvp = incubator::IncubatorProfile::from_toml(include_str!(
            "../profiles/aarch64-fvp-cca.toml"
        ))
        .unwrap();
        configure_profile(&mut fvp, &args, false).unwrap();
        let incubator::IncubatorBackend::FvpCca(config) = &fvp.incubator else {
            panic!("expected FVP profile");
        };
        assert_eq!(config.deadlines.test_execution, 1800);
        args.timeout = 600;
        configure_profile(&mut fvp, &args, true).unwrap();
        let incubator::IncubatorBackend::FvpCca(config) = &fvp.incubator else {
            panic!("expected FVP profile");
        };
        assert_eq!(config.deadlines.test_execution, 600);
        args.timeout = 1;
        assert!(configure_profile(&mut fvp, &args, true).is_err());
        args.timeout = 600;
        args.no_pty = false;
        assert!(configure_profile(&mut fvp, &args, true).is_err());
        args.no_pty = true;
        args.qemu_binary = Some("qemu-system-aarch64".into());
        assert!(configure_profile(&mut fvp, &args, true).is_err());
        args.qemu_binary = None;
        args.fvp_platform_root = Some("platform".into());
        for text in [
            include_str!("../profiles/aarch64-tcg-pcie.toml"),
            include_str!("../profiles/x86_64-openvmm.toml"),
            include_str!("../profiles/aarch64-qemu-cca.toml"),
        ] {
            let mut profile = incubator::IncubatorProfile::from_toml(text).unwrap();
            assert!(configure_profile(&mut profile, &args, false).is_err());
        }
        args.fvp_platform_root = None;
        args.fvp_input_share = Some("prepared-inputs".into());
        let mut qemu = incubator::IncubatorProfile::from_toml(include_str!(
            "../profiles/aarch64-tcg-pcie.toml"
        ))
        .unwrap();
        assert!(configure_profile(&mut qemu, &args, false).is_err());
    }

    #[test]
    fn fvp_result_file_cli_and_environment_contract() {
        let command = Args::command();
        let option = command
            .get_arguments()
            .find(|argument| argument.get_id() == "fvp_result_file")
            .unwrap();
        assert_eq!(option.get_long(), Some("fvp-result-file"));
        assert_eq!(
            option.get_env(),
            Some(std::ffi::OsStr::new("INCUBATOR_FVP_RESULT_FILE"))
        );
        assert!(!option.is_required_set());
        let args = Args::try_parse_from([
            "incubator",
            "--profile",
            "profile.toml",
            "--share",
            ".",
            "--fvp-result-file",
            "/output/session-result.json",
            "--",
            "/share/runner",
        ])
        .unwrap();
        assert_eq!(
            args.fvp_result_file,
            Some(PathBuf::from("/output/session-result.json"))
        );
    }
}
