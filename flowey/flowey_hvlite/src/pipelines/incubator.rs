// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local standalone backend provisioning, independent of VMM test discovery.

use anyhow::Context as _;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::common::CommonProfile;
use std::path::Path;
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct IncubatorCli {
    /// Source Incubator profile (x64 OpenVMM with mshv boot, or FVP CCA).
    #[arg(long)]
    profile: PathBuf,

    /// Override the MSHV root bzImage or the FVP host Image.
    #[arg(long)]
    kernel: Option<PathBuf>,

    /// Override the MSHV test initrd or the FVP host initrd.
    #[arg(long)]
    initrd: Option<PathBuf>,

    /// Override the pinned x64 MSVM.fd firmware.
    #[arg(long)]
    firmware: Option<PathBuf>,

    /// Use authorized local payloads containing boot/ instead of pinned RPMs.
    #[arg(long)]
    boot_artifacts: Option<PathBuf>,

    /// Live Shrinkwrap checkout/venv with the pinned licensed FVP image cached.
    /// Defaults to PETRI_FVP_PLATFORM_ROOT for FVP profiles only.
    #[arg(long)]
    fvp_platform_root: Option<PathBuf>,

    /// Qualified extracted FVP firmware package instead of a release archive.
    #[arg(long, conflicts_with_all = ["fvp_package_archive", "fvp_package_sha256"])]
    shrinkwrap_package_root: Option<PathBuf>,

    /// Release containing qualified FVP firmware and payload packages.
    #[arg(long)]
    fvp_deps_version: Option<String>,

    /// Payload release if different from --fvp-deps-version.
    #[arg(long, requires_all = ["fvp_deps_version", "fvp_payload_sha256"])]
    fvp_payload_version: Option<String>,

    /// Local release-shaped FVP firmware archive.
    #[arg(long, requires_all = ["fvp_deps_version", "fvp_package_sha256"])]
    fvp_package_archive: Option<PathBuf>,

    /// Expected lowercase SHA-256 of the FVP firmware archive.
    #[arg(long, requires = "fvp_deps_version")]
    fvp_package_sha256: Option<String>,

    /// Local release-shaped FVP host and reference-guest payload archive.
    #[arg(long, requires_all = ["fvp_deps_version", "fvp_payload_sha256"])]
    fvp_payload_archive: Option<PathBuf>,

    /// Expected lowercase SHA-256 of the FVP payload archive.
    #[arg(long, requires = "fvp_deps_version")]
    fvp_payload_sha256: Option<String>,

    /// Explicit regular input file beneath --share to include in the FVP snapshot.
    ///
    /// Paths are relative to --share. Directories and symlinks are not accepted.
    /// The command binary is included separately by Incubator.
    #[arg(long = "fvp-input")]
    fvp_inputs: Vec<PathBuf>,

    /// Writable host share (defaults to the OpenVMM checkout).
    #[arg(long)]
    share: Option<PathBuf>,

    /// Output beneath the share (default: target/incubator or target/incubator-fvp).
    #[arg(long)]
    dir: Option<PathBuf>,

    /// Override Linux options, excluding the required initrd entry point.
    #[arg(long)]
    kernel_cmdline: Option<String>,

    /// Override hypervisor options, excluding the loader DLL path.
    #[arg(long)]
    hypervisor_cmdline: Option<String>,

    /// Prepare dependencies/profile and print a direct command without booting.
    #[arg(long, conflicts_with = "incubator_args")]
    prepare_only: bool,

    /// Release VMM, agent, and EFI builds (Incubator otherwise uses light).
    #[arg(long)]
    release: bool,

    /// Automatically install missing build dependencies.
    #[arg(long)]
    install_missing_deps: bool,

    /// Enable verbose Flowey output.
    #[arg(long)]
    verbose: bool,

    /// Incubator options and guest command, after `--`.
    #[arg(last = true, required_unless_present = "prepare_only")]
    incubator_args: Vec<String>,
}

impl IntoPipeline for IncubatorCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let working_dir = std::env::current_dir().context("getting Incubator working directory")?;
        let source_profile = read_source_profile(&self.profile)?;
        let source_backend =
            flowey_lib_hvlite::_jobs::local_run_incubator::source_backend(&source_profile)?;
        if source_backend == flowey_lib_hvlite::_jobs::local_run_incubator::SourceBackend::Fvp {
            return self.into_fvp_pipeline(backend_hint, source_profile, working_dir);
        }
        anyhow::ensure!(
            self.fvp_platform_root.is_none()
                && self.shrinkwrap_package_root.is_none()
                && self.fvp_deps_version.is_none()
                && self.fvp_payload_version.is_none()
                && self.fvp_package_archive.is_none()
                && self.fvp_package_sha256.is_none()
                && self.fvp_payload_archive.is_none()
                && self.fvp_payload_sha256.is_none()
                && self.fvp_inputs.is_empty(),
            "FVP provisioning options require a type=fvp-cca profile"
        );
        anyhow::ensure!(
            matches!(backend_hint, PipelineBackendHint::Local)
                && matches!(FlowPlatform::host(backend_hint), FlowPlatform::Linux(_))
                && matches!(FlowArch::host(backend_hint), FlowArch::X86_64),
            "incubator provisioning currently requires a local x64 Linux host with KVM"
        );
        let kernel = self.kernel.as_deref().map(existing_file).transpose()?;
        let initrd = self.initrd.as_deref().map(existing_file).transpose()?;
        let firmware = self.firmware.as_deref().map(existing_file).transpose()?;
        let boot_artifacts = self
            .boot_artifacts
            .map(|path| {
                anyhow::ensure!(path.is_dir(), "not a payload directory: {}", path.display());
                fs_canonicalize(&path)
            })
            .transpose()?;
        let share = fs_canonicalize(&self.share.unwrap_or_else(crate::repo_root))?;
        anyhow::ensure!(
            share.is_dir(),
            "share is not a directory: {}",
            share.display()
        );
        let dir = self.dir.unwrap_or_else(|| share.join("target/incubator"));
        std::fs::create_dir_all(&dir)?;
        let dir = fs_canonicalize(&dir)?;
        anyhow::ensure!(
            dir.starts_with(&share),
            "--dir must be beneath --share so the guest Pipette source is accessible"
        );

        let mut pipeline = Pipeline::new();
        pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "provision and run standalone mshv Incubator",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
            .dep_on(
                |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                    hvlite_repo_source: flowey_lib_common::git_checkout::RepoSource::ExistingClone(
                        ReadVar::from_static(crate::repo_root()),
                    ),
                },
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: self.install_missing_deps,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(self.verbose),
                locked: false,
                deny_warnings: false,
                no_incremental: false,
            })
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::local_run_incubator::Params {
                    profile: CommonProfile::from_release(self.release),
                    source_profile,
                    working_dir,
                    kernel,
                    initrd,
                    firmware,
                    boot_artifacts,
                    share,
                    dir,
                    kernel_cmdline: self.kernel_cmdline,
                    hypervisor_cmdline: self.hypervisor_cmdline,
                    prepare_only: self.prepare_only,
                    incubator_args: self.incubator_args,
                    done: ctx.new_done_handle(),
                },
            )
            .finish();
        Ok(pipeline)
    }
}

impl IncubatorCli {
    fn into_fvp_pipeline(
        self,
        backend_hint: PipelineBackendHint,
        source_profile: String,
        working_dir: PathBuf,
    ) -> anyhow::Result<Pipeline> {
        anyhow::ensure!(
            matches!(backend_hint, PipelineBackendHint::Local)
                && matches!(FlowPlatform::host(backend_hint), FlowPlatform::Linux(_))
                && matches!(FlowArch::host(backend_hint), FlowArch::X86_64),
            "FVP provisioning requires a local x64 Linux model host, not KVM; other model-host architectures are not qualified"
        );
        anyhow::ensure!(
            self.firmware.is_none()
                && self.boot_artifacts.is_none()
                && self.kernel_cmdline.is_none()
                && self.hypervisor_cmdline.is_none(),
            "MSHV firmware, boot artifacts and command-line overrides do not apply to FVP"
        );
        let platform_root = self
            .fvp_platform_root
            .or_else(|| std::env::var_os("PETRI_FVP_PLATFORM_ROOT").map(PathBuf::from))
            .context("FVP requires --fvp-platform-root or PETRI_FVP_PLATFORM_ROOT")?;
        let platform_root = existing_directory(&platform_root)?;
        let package_root = self
            .shrinkwrap_package_root
            .as_deref()
            .map(existing_directory)
            .transpose()?;
        let kernel = self.kernel.as_deref().map(existing_file).transpose()?;
        let initrd = self.initrd.as_deref().map(existing_file).transpose()?;
        anyhow::ensure!(
            package_root.is_some()
                || (self.fvp_deps_version.is_some() && self.fvp_package_sha256.is_some()),
            "FVP requires --shrinkwrap-package-root or an explicit release and firmware SHA-256"
        );
        anyhow::ensure!(
            (kernel.is_some() && initrd.is_some())
                || (self.fvp_deps_version.is_some() && self.fvp_payload_sha256.is_some()),
            "FVP requires both --kernel and --initrd, or an explicit release and payload SHA-256"
        );
        let share = existing_directory(&self.share.unwrap_or_else(crate::repo_root))?;
        let dir = self
            .dir
            .unwrap_or_else(|| share.join("target/incubator-fvp"));
        std::fs::create_dir_all(&dir)?;
        let dir = fs_canonicalize(&dir)?;
        anyhow::ensure!(
            dir.starts_with(&share),
            "--dir must be beneath --share so the FVP Pipette snapshot is accessible"
        );
        for root in [
            &platform_root,
            package_root.as_ref().unwrap_or(&platform_root),
        ] {
            anyhow::ensure!(
                !dir.starts_with(root) && !root.starts_with(&dir),
                "FVP output and platform/package roots must not overlap"
            );
        }
        for archive in [&self.fvp_package_archive, &self.fvp_payload_archive]
            .into_iter()
            .flatten()
        {
            anyhow::ensure!(
                !existing_file(archive)?.starts_with(&dir),
                "FVP input archives must be outside the output directory"
            );
        }
        let mut pipeline = Pipeline::new();
        let mut job = pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "provision and run standalone FVP Incubator",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
            .dep_on(
                |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                    hvlite_repo_source: flowey_lib_common::git_checkout::RepoSource::ExistingClone(
                        ReadVar::from_static(crate::repo_root()),
                    ),
                },
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: self.install_missing_deps,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(self.verbose),
                locked: false,
                deny_warnings: false,
                no_incremental: false,
            });
        if let Some(version) = self.fvp_deps_version {
            let local_archive = self
                .fvp_package_archive
                .map(existing_owned_file)
                .transpose()?;
            let local_payload_archive = self
                .fvp_payload_archive
                .map(existing_owned_file)
                .transpose()?;
            job = job.dep_on(move |_| {
                flowey_lib_hvlite::_jobs::cfg_versions::Request::FvpCcaPlatform {
                    version,
                    payload_version: self.fvp_payload_version,
                    archive_sha256: self.fvp_package_sha256,
                    local_archive: local_archive.map(ReadVar::from_static),
                    payload_archive_sha256: self.fvp_payload_sha256,
                    local_payload_archive: local_payload_archive.map(ReadVar::from_static),
                }
            });
        }
        job.dep_on(
            |ctx| flowey_lib_hvlite::_jobs::local_run_fvp_incubator::Params {
                profile: CommonProfile::from_release(self.release),
                source_profile,
                working_dir,
                kernel,
                initrd,
                platform_root,
                package_root,
                inputs: self.fvp_inputs,
                share,
                dir,
                prepare_only: self.prepare_only,
                incubator_args: self.incubator_args,
                done: ctx.new_done_handle(),
            },
        )
        .finish();
        Ok(pipeline)
    }
}

fn existing_owned_file(path: PathBuf) -> anyhow::Result<PathBuf> {
    existing_file(&path)
}

fn existing_directory(path: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(path.is_dir(), "not a directory: {}", path.display());
    fs_canonicalize(path)
}

fn read_source_profile(path: &Path) -> anyhow::Result<String> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading Incubator profile {}", path.display()))?;
    flowey_lib_hvlite::_jobs::local_run_incubator::parse_source_profile(&contents)
        .with_context(|| format!("validating Incubator profile {}", path.display()))?;
    Ok(contents)
}

#[expect(
    clippy::disallowed_methods,
    reason = "resolve symlinks before enforcing containment within the host share"
)]
fn fs_canonicalize(path: &Path) -> anyhow::Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))
}

fn existing_file(path: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(path.is_file(), "not an input file: {}", path.display());
    fs_canonicalize(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use test_with_tracing::test;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        incubator: IncubatorCli,
    }

    #[test]
    fn preparation_needs_neither_a_kernel_override_nor_a_command() -> anyhow::Result<()> {
        let cli = Cli::try_parse_from(["incubator", "--profile", "source.toml", "--prepare-only"])?;
        assert!(cli.incubator.prepare_only);
        assert!(cli.incubator.kernel.is_none());
        assert!(cli.incubator.incubator_args.is_empty());
        Ok(())
    }

    #[test]
    fn fvp_preparation_accepts_local_roots_and_explicit_payloads() -> anyhow::Result<()> {
        let cli = Cli::try_parse_from([
            "incubator",
            "--profile",
            "fvp-cca.toml",
            "--fvp-platform-root",
            "live shrinkwrap",
            "--shrinkwrap-package-root",
            "qualified package",
            "--kernel",
            "host/Image",
            "--initrd",
            "host/initrd",
            "--prepare-only",
        ])?;
        assert_eq!(
            cli.incubator.fvp_platform_root.as_deref(),
            Some(Path::new("live shrinkwrap"))
        );
        assert!(cli.incubator.fvp_deps_version.is_none());
        assert!(cli.incubator.prepare_only);
        Ok(())
    }

    #[test]
    fn fvp_archives_require_identity_and_cannot_override_a_package_root() {
        let common = ["incubator", "--profile", "fvp.toml", "--prepare-only"];
        for extra in [
            vec!["--fvp-package-archive", "firmware.tar.gz"],
            vec!["--fvp-payload-archive", "payload.tar.gz"],
            vec!["--fvp-package-sha256", "digest"],
            vec![
                "--shrinkwrap-package-root",
                "package",
                "--fvp-deps-version",
                "qualified",
                "--fvp-package-sha256",
                "digest",
            ],
        ] {
            assert!(Cli::try_parse_from(common.into_iter().chain(extra)).is_err());
        }
    }

    #[test]
    fn fvp_payload_version_and_explicit_inputs_preserve_command_arguments() -> anyhow::Result<()> {
        let digest = "0".repeat(64);
        let args = [
            "incubator",
            "--profile",
            "fvp.toml",
            "--fvp-deps-version",
            "source-firmware",
            "--fvp-payload-sha256",
            &digest,
            "--prepare-only",
        ];
        assert!(
            Cli::try_parse_from(args)?
                .incubator
                .fvp_payload_version
                .is_none()
        );
        let cli = Cli::try_parse_from([
            "incubator",
            "--profile",
            "fvp.toml",
            "--fvp-deps-version",
            "source-firmware",
            "--fvp-payload-sha256",
            &digest,
            "--fvp-payload-version",
            "reference-payload",
            "--fvp-input",
            "data/input with spaces",
            "--",
            "--map-command-path",
            "--",
            "/checkout/program",
            "--arbitrary",
            "hello world",
        ])?;
        assert_eq!(
            cli.incubator.fvp_payload_version.as_deref(),
            Some("reference-payload")
        );
        assert_eq!(
            cli.incubator.fvp_inputs,
            [PathBuf::from("data/input with spaces")]
        );
        assert_eq!(
            cli.incubator.incubator_args,
            [
                "--map-command-path",
                "--",
                "/checkout/program",
                "--arbitrary",
                "hello world"
            ]
        );
        assert!(
            Cli::try_parse_from([
                "incubator",
                "--profile",
                "fvp.toml",
                "--prepare-only",
                "--fvp-payload-version",
                "reference-payload",
            ])
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn commands_preserve_shell_arguments_and_conflicting_modes_are_rejected() -> anyhow::Result<()>
    {
        let cli = Cli::try_parse_from([
            "incubator",
            "--profile",
            "source.toml",
            "--",
            "--timeout",
            "600",
            "--guest-env",
            "TEST=hello world",
            "--no-pty",
            "--",
            "/bin/sh",
            "-c",
            "exit 17",
        ])?;
        assert_eq!(
            cli.incubator.incubator_args,
            [
                "--timeout",
                "600",
                "--guest-env",
                "TEST=hello world",
                "--no-pty",
                "--",
                "/bin/sh",
                "-c",
                "exit 17"
            ]
        );
        for args in [
            vec!["incubator"],
            vec!["incubator", "--prepare-only"],
            vec!["incubator", "--profile", "source.toml", "--timeout", "600"],
            vec![
                "incubator",
                "--profile",
                "source.toml",
                "--prepare-only",
                "--",
                "/bin/sh",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        Ok(())
    }
}
