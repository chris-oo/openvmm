// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that builds everything needed and runs the VMM tests

use crate::_jobs::consume_and_test_nextest_vmm_tests_archive::CcaTestArtifacts;
use crate::_jobs::consume_and_test_nextest_vmm_tests_archive::TestContentConfig;
use crate::build_incubator::IncubatorProfileNameOrPath;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmOutput;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeDetailsLocalOnly;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeType;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::build_tpm_guest_tests::TpmGuestTestsOutput;
use crate::common::CommonArch;
use crate::common::CommonPlatform;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifacts;
use crate::init_vmm_tests_env::PetriParams;
use crate::install_vmm_tests_external_deps::VmmTestsExternalDeps;
use flowey::node::prelude::*;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::num::NonZeroU64;
use vmm_test_images::KnownTestArtifacts;

#[derive(Serialize, Deserialize)]
pub struct VmmTestSelections {
    /// Test filter
    pub filter: String,
    /// List of artifacts to download
    pub downloaded_artifacts: Vec<KnownTestArtifacts>,
    /// List of artifacts to build
    pub build: BuildSelections,
    /// Dependencies to install
    pub external_deps: VmmTestsExternalDeps,
    /// Whether to download release IGVM files from GitHub
    pub needs_release_igvm: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BuildSelections {
    pub openhcl_standard: bool,
    pub openhcl_standard_dev: bool,
    pub openhcl_cvm: bool,
    pub openhcl_linux_direct: bool,
    pub openvmm: bool,
    pub openvmm_vhost: bool,
    pub pipette_windows: bool,
    pub pipette_linux: bool,
    pub prep_steps_standard: bool,
    pub prep_steps_no_vmbus: bool,
    pub guest_test_uefi: bool,
    pub tmks: bool,
    pub tmk_vmm_windows: bool,
    pub tmk_vmm_linux: bool,
    pub vmgstool: bool,
    pub vmgstool_dev: bool,
    pub tpm_guest_tests_windows: bool,
    pub tpm_guest_tests_linux: bool,
    pub test_igvm_agent_rpc_server: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum CcaPlatformSource {
    /// Explicit local directory containing the pinned in-place payload files.
    PayloadGuestMemfdInPlace { root: PathBuf },
    PayloadRelease {
        version: String,
        kernel_archive_sha256: String,
        initrd_archive_sha256: String,
    },
    PayloadLocal {
        version: String,
        kernel_archive: PathBuf,
        kernel_archive_sha256: String,
        initrd_archive: PathBuf,
        initrd_archive_sha256: String,
    },
    Release {
        version: String,
        kernel_archive_sha256: String,
        rmm_archive_sha256: String,
        tfa_archive_sha256: String,
        initrd_archive_sha256: String,
    },
    Local {
        version: String,
        kernel_archive: PathBuf,
        kernel_archive_sha256: String,
        rmm_archive: PathBuf,
        rmm_archive_sha256: String,
        tfa_archive: PathBuf,
        tfa_archive_sha256: String,
        initrd_archive: PathBuf,
        initrd_archive_sha256: String,
    },
}

impl CcaPlatformSource {
    /// Protect local payload inputs from output creation and cleanup. Also used
    /// for direct job requests, which do not pass through CLI validation.
    pub fn protect_payload_from_output(&mut self, output: &Path) -> anyhow::Result<()> {
        if let Self::PayloadGuestMemfdInPlace { root } = self {
            *root =
                crate::write_incubator_target_runner::FvpPlatformRoots::resolve_fvp_payload_root(
                    root, output,
                )?;
        }
        Ok(())
    }

    /// Select the qualified FVP payload, optionally from local copies of the
    /// official archives. The payload resolver verifies their bytes.
    pub fn fvp_payload(
        version: Option<String>,
        kernel_archive_sha256: Option<String>,
        initrd_archive_sha256: Option<String>,
        local_archives: Option<(PathBuf, PathBuf)>,
    ) -> anyhow::Result<Self> {
        let version = version.unwrap_or_else(|| crate::cca_pins::OPENVMM_DEPS_RELEASE.into());
        let kernel_archive_sha256 =
            kernel_archive_sha256.unwrap_or_else(|| crate::cca_pins::KERNEL_ARCHIVE_SHA256.into());
        let initrd_archive_sha256 =
            initrd_archive_sha256.unwrap_or_else(|| crate::cca_pins::INITRD_ARCHIVE_SHA256.into());
        let source = match local_archives {
            None => Self::PayloadRelease {
                version,
                kernel_archive_sha256,
                initrd_archive_sha256,
            },
            Some((kernel_archive, initrd_archive)) => Self::PayloadLocal {
                version,
                kernel_archive,
                kernel_archive_sha256,
                initrd_archive,
                initrd_archive_sha256,
            },
        };
        source.validate_fvp_payload()?;
        Ok(source)
    }

    fn validate_fvp_payload(&self) -> anyhow::Result<()> {
        if matches!(self, Self::PayloadGuestMemfdInPlace { .. }) {
            return Ok(());
        }
        let (version, kernel_hash, initrd_hash) = match self {
            Self::PayloadRelease {
                version,
                kernel_archive_sha256,
                initrd_archive_sha256,
            }
            | Self::PayloadLocal {
                version,
                kernel_archive_sha256,
                initrd_archive_sha256,
                ..
            } => (version, kernel_archive_sha256, initrd_archive_sha256),
            _ => anyhow::bail!("FVP CCA requires common payload artifacts without QEMU firmware"),
        };
        for (option, selected, expected) in [
            (
                "--cca-deps-version",
                version,
                crate::cca_pins::OPENVMM_DEPS_RELEASE,
            ),
            (
                "--cca-kernel-archive-sha256",
                kernel_hash,
                crate::cca_pins::KERNEL_ARCHIVE_SHA256,
            ),
            (
                "--cca-initrd-archive-sha256",
                initrd_hash,
                crate::cca_pins::INITRD_ARCHIVE_SHA256,
            ),
        ] {
            anyhow::ensure!(
                selected == expected,
                "FVP CCA requires the qualified {} payload: {option} must be {expected}; \
                 local archive paths are allowed only with these identities",
                crate::cca_pins::OPENVMM_DEPS_RELEASE
            );
        }
        Ok(())
    }
}

flowey_request! {
    pub struct Params {
        pub target: CommonTriple,

        /// Toolchain platform to use when cross-compiling Windows *guest*
        /// payloads (e.g. pipette). On a non-WSL Linux build host this is
        /// [`CommonPlatform::WindowsGnu`], since the MSVC toolchain is
        /// unavailable there; otherwise it is [`CommonPlatform::WindowsMsvc`].
        pub windows_guest_platform: CommonPlatform,

        pub test_content_dir: PathBuf,

        pub selections: VmmTestSelections,

        /// Release build instead of debug build
        pub release: bool,

        /// Whether to run the tests or just build and archive
        pub build_only: bool,
        /// Run this exact tests-binary test within one FVP invocation.
        pub fvp_single_test: Option<String>,
        /// Copy extras to output dir (symbols, etc)
        pub copy_extras: bool,

        /// Optional: provide a custom kernel modules cpio or directory for initrd layering
        pub custom_kernel_modules: Option<PathBuf>,
        /// Optional: provide a custom kernel image to embed in IGVM (forces UEFI)
        pub custom_kernel: Option<PathBuf>,

        /// Skip the interactive VHD download prompt
        pub skip_vhd_prompt: bool,

        pub nextest_profile: crate::run_cargo_nextest_run::NextestProfile,

        pub petri_params: PetriParams,

        pub disable_secure_avic: bool,

        pub repetitions: NonZeroU64,

        /// Optional: incubator profile path. When set, tests run inside
        /// an emulated VM instead of on the host.
        pub incubator_profile: Option<IncubatorProfileNameOrPath>,
        /// Incubator platform selected before the Flowey graph is emitted.
        pub incubator_platform:
            Option<crate::write_incubator_target_runner::IncubatorPlatform>,
        /// Coherent CCA platform artifact source.
        pub cca_platform_source: Option<CcaPlatformSource>,
        /// Explicit root of the unchanged, pinned CCA TDISP guest.
        pub cca_tdisp_guest_root: Option<PathBuf>,
        /// Licensed FVP toolchain and package input directories.
        pub fvp_roots: Option<crate::write_incubator_target_runner::FvpPlatformRoots>,

        pub done: WriteVar<SideEffect>,
    }
}

fn validate_fvp_selections(
    selections: &VmmTestSelections,
    cca_tdisp_guest: bool,
) -> anyhow::Result<()> {
    let allowed = BuildSelections {
        openvmm: true,
        pipette_linux: true,
        ..Default::default()
    };
    let no_agent = BuildSelections {
        pipette_linux: false,
        ..allowed.clone()
    };
    let valid_build = if cca_tdisp_guest {
        selections.filter
            == crate::run_fvp_single_boot::single_test_filter(crate::cca_tdisp_guest::TEST_NAME)?
            && selections.build == no_agent
    } else {
        selections.build == allowed
    };
    anyhow::ensure!(
        valid_build && selections.downloaded_artifacts.is_empty() && !selections.needs_release_igvm,
        "FVP CCA supports only direct-boot CCA artifacts; narrow --filter to the CCA tests"
    );
    Ok(())
}

fn cca_host_test_script(
    command: &flowey_lib_common::gen_cargo_nextest_run_cmd::Script,
    repetitions: NonZeroU64,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        matches!(
            command.shell,
            flowey_lib_common::gen_cargo_nextest_run_cmd::CommandShell::Bash
        ),
        "CCA host test scripts require a Linux build host"
    );
    Ok(format!(
        "#!/bin/sh\nset -e\nfor iteration in $(seq 1 {}); do\n{command}\ndone\n",
        repetitions.get()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cca_build_only_script_preserves_host_runner_overrides() {
        use flowey_lib_common::gen_cargo_nextest_run_cmd::CommandShell;
        use flowey_lib_common::gen_cargo_nextest_run_cmd::Script;
        use std::collections::BTreeMap;

        for (platform_key, platform_path) in [
            ("INCUBATOR_FIRMWARE", "/resolved/firmware.bin"),
            ("INCUBATOR_FVP_PLATFORM_ROOT", "/licensed/FVP root"),
        ] {
            let command = Script {
                env: BTreeMap::from([
                    (
                        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER".into(),
                        "/build/host/incubator".into(),
                    ),
                    ("INCUBATOR_KERNEL".into(), "/resolved/host-kernel".into()),
                    ("INCUBATOR_INITRD".into(), "/resolved/initrd".into()),
                    (platform_key.into(), platform_path.into()),
                ]),
                commands: vec![(
                    "/build/host/cargo-nextest".into(),
                    vec![
                        "nextest".into(),
                        "run".into(),
                        "--archive-file".into(),
                        "/built/test archive.tar.zst".into(),
                    ],
                )],
                shell: CommandShell::Bash,
            };
            let script = cca_host_test_script(&command, NonZeroU64::new(2).unwrap()).unwrap();
            assert!(script.contains("seq 1 2"));
            assert!(script.contains("export INCUBATOR_KERNEL='/resolved/host-kernel'"));
            assert!(script.contains(&format!("export {platform_key}='{platform_path}'")));
            assert!(script.contains("'/build/host/cargo-nextest'"));
            assert!(script.contains("'/built/test archive.tar.zst'"));
            assert!(!script.contains("vmm-tests-run-target"));
            #[cfg(unix)]
            {
                use std::io::Write as _;
                use std::process::Command;
                use std::process::Stdio;

                let mut child = Command::new("sh")
                    .arg("-n")
                    .stdin(Stdio::piped())
                    .spawn()
                    .unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(script.as_bytes())
                    .unwrap();
                assert!(child.wait().unwrap().success());
            }
        }
    }

    #[test]
    fn fvp_payload_output_rejects_cleanup_ancestors() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let output = directory.path().join("output");
        for relative in ["temp/payload", "test_results/payload", ""] {
            let payload = output.join(relative);
            fs_err::create_dir_all(&payload).unwrap();
            fs_err::write(payload.join("Image"), b"preserve payload").unwrap();
            let mut source = CcaPlatformSource::PayloadGuestMemfdInPlace {
                root: payload.clone(),
            };
            let error = source.protect_payload_from_output(&output).unwrap_err();
            assert!(error.to_string().contains("must not overlap"), "{error:#}");
            assert_eq!(
                fs_err::read(payload.join("Image")).unwrap(),
                b"preserve payload"
            );
        }
    }

    #[test]
    fn fvp_payload_output_rejects_descendants_before_creation() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let payload = directory.path().join("payload");
        fs_err::create_dir(&payload).unwrap();
        let output = payload.join("missing/output");
        let mut source = CcaPlatformSource::PayloadGuestMemfdInPlace {
            root: payload.clone(),
        };
        assert!(source.protect_payload_from_output(&output).is_err());
        assert!(!payload.join("missing").exists());
        let separate = directory.path().join("separate/output");
        source.protect_payload_from_output(&separate).unwrap();
        let CcaPlatformSource::PayloadGuestMemfdInPlace { root } = source else {
            panic!("expected in-place payload");
        };
        assert_eq!(root, fs_err::canonicalize(payload).unwrap());
        assert!(!separate.exists());
    }

    #[cfg(unix)]
    #[test]
    fn fvp_payload_output_rejects_symlink_aliases_in_both_directions() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let output = directory.path().join("output");
        let payload = output.join("temp/payload");
        fs_err::create_dir_all(&payload).unwrap();
        let payload_alias = directory.path().join("payload-alias");
        let output_alias = directory.path().join("output-alias");
        symlink(&payload, &payload_alias).unwrap();
        symlink(&output, &output_alias).unwrap();
        for (input, destination) in [
            (payload_alias.clone(), output.clone()),
            (payload.clone(), output_alias.clone()),
            (payload_alias.clone(), output_alias),
            (payload.clone(), payload_alias.join("missing/output")),
            (payload_alias.clone(), payload.clone()),
        ] {
            let mut source = CcaPlatformSource::PayloadGuestMemfdInPlace { root: input };
            assert!(source.protect_payload_from_output(&destination).is_err());
        }
        assert!(!payload.join("missing").exists());
        let mut source = CcaPlatformSource::PayloadGuestMemfdInPlace {
            root: payload_alias,
        };
        source
            .protect_payload_from_output(&directory.path().join("safe"))
            .unwrap();
        let CcaPlatformSource::PayloadGuestMemfdInPlace { root } = source else {
            panic!("expected in-place payload");
        };
        assert_eq!(root, fs_err::canonicalize(payload).unwrap());
    }

    #[test]
    fn fvp_payload_defaults_to_qualified_release() {
        let source = CcaPlatformSource::fvp_payload(None, None, None, None).unwrap();
        let CcaPlatformSource::PayloadRelease {
            version,
            kernel_archive_sha256,
            initrd_archive_sha256,
        } = source
        else {
            panic!("expected release payload");
        };
        assert_eq!(version, crate::cca_pins::OPENVMM_DEPS_RELEASE);
        assert_eq!(
            kernel_archive_sha256,
            crate::cca_pins::KERNEL_ARCHIVE_SHA256
        );
        assert_eq!(
            initrd_archive_sha256,
            crate::cca_pins::INITRD_ARCHIVE_SHA256
        );
    }

    #[test]
    fn fvp_payload_accepts_same_identity_local_archives() {
        let kernel = PathBuf::from("local/openvmm-test-linux-cca-v15.aarch64.0.3.0-139.tar.gz");
        let initrd = PathBuf::from("local/openvmm-test-initrd.aarch64.0.3.0-139.tar.gz");
        for explicit in [false, true] {
            let source = CcaPlatformSource::fvp_payload(
                explicit.then(|| crate::cca_pins::OPENVMM_DEPS_RELEASE.into()),
                explicit.then(|| crate::cca_pins::KERNEL_ARCHIVE_SHA256.into()),
                explicit.then(|| crate::cca_pins::INITRD_ARCHIVE_SHA256.into()),
                Some((kernel.clone(), initrd.clone())),
            )
            .unwrap();
            source.validate_fvp_payload().unwrap();
            let CcaPlatformSource::PayloadLocal {
                kernel_archive,
                initrd_archive,
                ..
            } = source
            else {
                panic!("expected local payload");
            };
            assert_eq!(kernel_archive, kernel);
            assert_eq!(initrd_archive, initrd);
        }
    }

    #[test]
    fn fvp_payload_rejects_incompatible_release_and_archive_identities() {
        for local in [None, Some(("local-kernel".into(), "local-initrd".into()))] {
            for (version, kernel, initrd, diagnostic) in [
                (Some("0.3.0-140".into()), None, None, "--cca-deps-version"),
                (
                    None,
                    Some("0".repeat(64)),
                    None,
                    "--cca-kernel-archive-sha256",
                ),
                (
                    None,
                    None,
                    Some("0".repeat(64)),
                    "--cca-initrd-archive-sha256",
                ),
            ] {
                let error = CcaPlatformSource::fvp_payload(version, kernel, initrd, local.clone())
                    .unwrap_err();
                assert!(error.to_string().contains(diagnostic), "{error:#}");
            }
        }
    }

    #[test]
    fn fvp_payload_revalidates_direct_graph_sources() {
        let invalid = CcaPlatformSource::PayloadRelease {
            version: "unqualified-release".into(),
            kernel_archive_sha256: crate::cca_pins::KERNEL_ARCHIVE_SHA256.into(),
            initrd_archive_sha256: crate::cca_pins::INITRD_ARCHIVE_SHA256.into(),
        };
        assert!(invalid.validate_fvp_payload().is_err());
        let invalid = CcaPlatformSource::PayloadLocal {
            version: crate::cca_pins::OPENVMM_DEPS_RELEASE.into(),
            kernel_archive: "kernel".into(),
            kernel_archive_sha256: crate::cca_pins::KERNEL_ARCHIVE_SHA256.into(),
            initrd_archive: "initrd".into(),
            initrd_archive_sha256: "0".repeat(64),
        };
        assert!(invalid.validate_fvp_payload().is_err());
    }

    fn fvp_selections() -> VmmTestSelections {
        VmmTestSelections {
            filter: "binary(=tests) & test(boot_linux_direct_cca)".into(),
            downloaded_artifacts: Vec::new(),
            build: BuildSelections {
                openvmm: true,
                pipette_linux: true,
                ..Default::default()
            },
            external_deps: VmmTestsExternalDeps::Linux(
                crate::install_vmm_tests_external_deps::VmmTestsExternalDepsLinux {
                    hugetlb_2mb_overcommit_pages: None,
                    prepare_vhost_vsock: false,
                },
            ),
            needs_release_igvm: false,
        }
    }

    #[test]
    fn fvp_selects_only_source_matched_cca_binaries() {
        validate_fvp_selections(&fvp_selections(), false).unwrap();
        let mut selections = fvp_selections();
        selections.build.openhcl_standard = true;
        assert!(validate_fvp_selections(&selections, false).is_err());
        selections = fvp_selections();
        selections.build.guest_test_uefi = true;
        assert!(validate_fvp_selections(&selections, false).is_err());
        selections = fvp_selections();
        selections.needs_release_igvm = true;
        assert!(validate_fvp_selections(&selections, false).is_err());
        selections = fvp_selections();
        selections
            .downloaded_artifacts
            .push(KnownTestArtifacts::Alpine323Aarch64Vhd);
        assert!(validate_fvp_selections(&selections, false).is_err());
    }

    #[test]
    fn fvp_no_agent_requires_the_pinned_guest_and_exact_da_selection() {
        let mut selections = fvp_selections();
        selections.build.pipette_linux = false;
        selections.filter =
            crate::run_fvp_single_boot::single_test_filter(crate::cca_tdisp_guest::TEST_NAME)
                .unwrap();
        validate_fvp_selections(&selections, true).unwrap();
        assert!(validate_fvp_selections(&selections, false).is_err());
        assert!(validate_fvp_selections(&fvp_selections(), true).is_err());
        let exact = selections.filter.clone();
        for filter in [
            "all()",
            "binary(=tests) & test(=some_other_no_agent_test)",
            &format!("{exact} | test(other)"),
        ] {
            selections.filter = filter.into();
            assert!(validate_fvp_selections(&selections, true).is_err());
            selections.build.pipette_linux = true;
            assert!(validate_fvp_selections(&selections, true).is_err());
            selections.build.pipette_linux = false;
        }
        selections.filter = exact;
        selections.build.guest_test_uefi = true;
        assert!(validate_fvp_selections(&selections, true).is_err());
        selections.build.guest_test_uefi = false;
        selections.needs_release_igvm = true;
        assert!(validate_fvp_selections(&selections, true).is_err());
        selections.needs_release_igvm = false;
        selections
            .downloaded_artifacts
            .push(KnownTestArtifacts::Alpine323Aarch64Vhd);
        assert!(validate_fvp_selections(&selections, true).is_err());
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_guest_test_uefi::Node>();
        ctx.import::<crate::build_incubator::Node>();
        ctx.import::<crate::build_nextest_vmm_tests::Node>();
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::build_openvmm_vhost::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::build_prep_steps::Node>();
        ctx.import::<crate::build_tmks::Node>();
        ctx.import::<crate::build_tmk_vmm::Node>();
        ctx.import::<crate::build_tpm_guest_tests::Node>();
        ctx.import::<crate::build_test_igvm_agent_rpc_server::Node>();
        ctx.import::<crate::download_openvmm_vmm_tests_artifacts::Node>();
        ctx.import::<crate::init_vmm_tests_content_dir::Node>();
        ctx.import::<crate::test_nextest_vmm_tests_archive::Node>();
        ctx.import::<crate::resolve_cca_platform::Node>();
        ctx.import::<crate::resolve_cca_payload::Node>();
        ctx.import::<crate::resolve_openvmm_qemu::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_vmm_tests_env::Node>();
        ctx.import::<crate::write_incubator_target_runner::Node>();
        ctx.import::<crate::resolve_vmm_tests_pipeline_artifacts::Node>();
        ctx.import::<flowey_lib_common::download_cargo_nextest::Node>();
        ctx.import::<flowey_lib_common::gen_cargo_nextest_run_cmd::Node>();
        ctx.import::<crate::resolve_cca_qemu_platform::Node>();
        ctx.import::<crate::build_vmgstool::Node>();
        ctx.import::<crate::_jobs::build_and_publish_openhcl_igvm_from_recipe::Node>();
        ctx.import::<crate::_jobs::consume_and_test_nextest_vmm_tests_archive::Node>();
        ctx.import::<crate::build_flowey_hvlite::Node>();
        ctx.import::<crate::run_fvp_single_boot::Node>();
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            windows_guest_platform,
            test_content_dir,
            selections,
            release,
            build_only,
            fvp_single_test,
            copy_extras,
            custom_kernel_modules,
            custom_kernel,
            skip_vhd_prompt,
            nextest_profile,
            mut petri_params,
            disable_secure_avic,
            repetitions,
            incubator_profile,
            incubator_platform,
            mut cca_platform_source,
            cca_tdisp_guest_root,
            fvp_roots,
            done,
        } = request;

        anyhow::ensure!(
            incubator_profile.is_some() == incubator_platform.is_some(),
            "incubator profile and platform classification must be provided together"
        );
        let is_fvp = incubator_platform.is_some_and(|platform| platform.is_fvp());
        anyhow::ensure!(
            cca_platform_source.is_some()
                == (is_fvp
                    || matches!(
                        incubator_platform,
                        Some(
                            crate::write_incubator_target_runner::IncubatorPlatform::QemuCca
                                | crate::write_incubator_target_runner::IncubatorPlatform::QemuCcaGuestMemfdInPlace
                        )
                    )),
            "CCA platform artifacts require a CCA incubator profile"
        );
        if let Some(source) = &mut cca_platform_source {
            source.protect_payload_from_output(&test_content_dir)?;
        }
        crate::cca_tdisp_guest::validate_options(
            cca_tdisp_guest_root.is_some(),
            incubator_platform,
            custom_kernel.is_some(),
            custom_kernel_modules.is_some(),
        )?;
        let cca_tdisp_guest_root = cca_tdisp_guest_root
            .map(|root| crate::cca_tdisp_guest::resolve(&root, &test_content_dir))
            .transpose()?;
        if let Some(name) = &fvp_single_test {
            anyhow::ensure!(is_fvp, "single-boot execution requires FVP");
            anyhow::ensure!(
                selections.filter == crate::run_fvp_single_boot::single_test_filter(name)?,
                "single-boot selection must match the exact requested test"
            );
        }
        let in_place = incubator_platform.is_some_and(|platform| platform.is_in_place());
        anyhow::ensure!(
            in_place
                == matches!(
                    cca_platform_source,
                    Some(CcaPlatformSource::PayloadGuestMemfdInPlace { .. })
                ),
            "in-place CCA profiles require the explicit pinned local in-place payload"
        );
        anyhow::ensure!(
            is_fvp == fvp_roots.is_some(),
            "FVP CCA requires both local platform roots; other backends do not accept them"
        );
        if is_fvp {
            petri_params.disable_remote_artifacts = true;
            anyhow::ensure!(
                matches!(ctx.platform(), FlowPlatform::Linux(_))
                    && target.common_arch()? == CommonArch::Aarch64
                    && target.as_triple().operating_system
                        == target_lexicon::OperatingSystem::Linux,
                "FVP CCA requires a Linux host and an AArch64 Linux target"
            );
            validate_fvp_selections(&selections, cca_tdisp_guest_root.is_some())?;
            cca_platform_source
                .as_ref()
                .context("FVP CCA requires configured payload artifacts")?
                .validate_fvp_payload()?;
            let config = match cca_platform_source.as_ref() {
                Some(CcaPlatformSource::PayloadGuestMemfdInPlace { root }) => {
                    crate::resolve_cca_payload::Config {
                        local_in_place_payload: Some(ConfigVar(ReadVar::from_static(root.clone()))),
                        ..Default::default()
                    }
                }
                Some(CcaPlatformSource::PayloadRelease {
                    version,
                    kernel_archive_sha256,
                    initrd_archive_sha256,
                }) => crate::resolve_cca_payload::Config {
                    version: Some(version.clone()),
                    kernel_archive_sha256: Some(kernel_archive_sha256.clone()),
                    initrd_archive_sha256: Some(initrd_archive_sha256.clone()),
                    ..Default::default()
                },
                Some(CcaPlatformSource::PayloadLocal {
                    version,
                    kernel_archive,
                    kernel_archive_sha256,
                    initrd_archive,
                    initrd_archive_sha256,
                }) => crate::resolve_cca_payload::Config {
                    version: Some(version.clone()),
                    kernel_archive_sha256: Some(kernel_archive_sha256.clone()),
                    initrd_archive_sha256: Some(initrd_archive_sha256.clone()),
                    local_kernel_archive: Some(ConfigVar(ReadVar::from_static(
                        kernel_archive.clone(),
                    ))),
                    local_initrd_archive: Some(ConfigVar(ReadVar::from_static(
                        initrd_archive.clone(),
                    ))),
                    ..Default::default()
                },
                _ => {
                    anyhow::bail!("FVP CCA requires common payload artifacts without QEMU firmware")
                }
            };
            ctx.config(config);
        } else if let Some(CcaPlatformSource::PayloadGuestMemfdInPlace { root }) =
            &cca_platform_source
        {
            ctx.config(crate::resolve_cca_payload::Config {
                local_in_place_payload: Some(ConfigVar(ReadVar::from_static(root.clone()))),
                ..Default::default()
            });
            ctx.config(crate::resolve_cca_qemu_platform::Config {
                version: Some(crate::cca_pins::OPENVMM_DEPS_RELEASE.into()),
                rmm_archive_sha256: Some(crate::cca_pins::RMM_ARCHIVE_SHA256.into()),
                tfa_archive_sha256: Some(crate::cca_pins::TFA_ARCHIVE_SHA256.into()),
                ..Default::default()
            });
        } else if let Some(source) = &cca_platform_source {
            let config = match source {
                CcaPlatformSource::Release {
                    version,
                    kernel_archive_sha256,
                    rmm_archive_sha256,
                    tfa_archive_sha256,
                    initrd_archive_sha256,
                } => crate::resolve_cca_platform::Config {
                    version: Some(version.clone()),
                    kernel_archive_sha256: Some(kernel_archive_sha256.clone()),
                    rmm_archive_sha256: Some(rmm_archive_sha256.clone()),
                    tfa_archive_sha256: Some(tfa_archive_sha256.clone()),
                    initrd_archive_sha256: Some(initrd_archive_sha256.clone()),
                    ..Default::default()
                },
                CcaPlatformSource::Local {
                    version,
                    kernel_archive,
                    kernel_archive_sha256,
                    rmm_archive,
                    rmm_archive_sha256,
                    tfa_archive,
                    tfa_archive_sha256,
                    initrd_archive,
                    initrd_archive_sha256,
                } => crate::resolve_cca_platform::Config {
                    version: Some(version.clone()),
                    kernel_archive_sha256: Some(kernel_archive_sha256.clone()),
                    rmm_archive_sha256: Some(rmm_archive_sha256.clone()),
                    tfa_archive_sha256: Some(tfa_archive_sha256.clone()),
                    initrd_archive_sha256: Some(initrd_archive_sha256.clone()),
                    local_kernel_archive: Some(ConfigVar(ReadVar::from_static(
                        kernel_archive.clone(),
                    ))),
                    local_rmm_archive: Some(ConfigVar(ReadVar::from_static(rmm_archive.clone()))),
                    local_tfa_archive: Some(ConfigVar(ReadVar::from_static(tfa_archive.clone()))),
                    local_initrd_archive: Some(ConfigVar(ReadVar::from_static(
                        initrd_archive.clone(),
                    ))),
                },
                CcaPlatformSource::PayloadRelease { .. }
                | CcaPlatformSource::PayloadLocal { .. }
                | CcaPlatformSource::PayloadGuestMemfdInPlace { .. } => {
                    anyhow::bail!("payload-only CCA sources require FVP CCA")
                }
            };
            ctx.config(config);
        }
        let cca_platform = match incubator_platform {
            Some(crate::write_incubator_target_runner::IncubatorPlatform::QemuCca) => {
                Some(ctx.reqv(crate::resolve_cca_platform::Request::Get))
            }
            Some(
                crate::write_incubator_target_runner::IncubatorPlatform::QemuCcaGuestMemfdInPlace,
            ) => Some(
                ctx.reqv(crate::resolve_cca_qemu_platform::Request::Get)
                    .map(ctx, crate::resolve_cca_platform::CcaPlatformOutput::from),
            ),
            _ => None,
        };
        let fvp_payload = is_fvp.then(|| {
            let payload = ctx.reqv(crate::resolve_cca_payload::Request::Get);
            ctx.emit_rust_stepv("verify qualified FVP payload", |ctx| {
                let payload = payload.claim(ctx);
                move |rt| {
                    let payload = rt.read(payload);
                    payload.validate_fvp()?;
                    Ok(payload)
                }
            })
        });

        let cca_artifacts = if let Some(platform) = cca_platform {
            anyhow::ensure!(
                target.common_arch()? == CommonArch::Aarch64,
                "QEMU CCA incubator requires an AArch64 target"
            );
            anyhow::ensure!(
                crate::_jobs::cfg_versions::OPENVMM_DEPS == crate::cca_pins::OPENVMM_DEPS_RELEASE,
                "configured QEMU release does not match the CCA platform contract"
            );
            let host_arch = ctx.arch().try_into()?;
            let qemu_binary = ctx.reqv(|v| {
                crate::resolve_openvmm_qemu::Request::Get(
                    crate::resolve_openvmm_qemu::QemuFile::SystemAarch64,
                    host_arch,
                    v,
                )
            });
            Some(CcaTestArtifacts {
                realm_kernel: platform.clone().map(ctx, |output| output.realm_kernel),
                host_kernel: platform.clone().map(ctx, |output| output.host_kernel),
                initrd: platform.clone().map(ctx, |output| output.host_initrd),
                firmware: Some(platform.map(ctx, |output| output.firmware)),
                qemu_binary: Some(qemu_binary),
                fvp_roots: None,
                cca_tdisp_guest_root: None,
            })
        } else {
            fvp_payload.map(|payload| CcaTestArtifacts {
                realm_kernel: payload.clone().map(ctx, |output| output.realm_kernel),
                host_kernel: payload.clone().map(ctx, |output| output.host_kernel),
                initrd: payload.map(ctx, |output| output.initrd),
                firmware: None,
                qemu_binary: None,
                fvp_roots: fvp_roots.clone(),
                cca_tdisp_guest_root,
            })
        };

        let test_content_dir = if is_fvp {
            let test_content_dir = fvp_roots
                .as_ref()
                .context("missing FVP roots")?
                .output_directory(&test_content_dir)?;
            fs_err::create_dir_all(&test_content_dir)?;
            fs_err::canonicalize(&test_content_dir)?
        } else {
            test_content_dir.absolute()?
        };
        let custom_kernel_modules_abs = custom_kernel_modules.map(|p| p.absolute()).transpose()?;
        let custom_kernel_abs = custom_kernel.map(|p| p.absolute()).transpose()?;

        let target_triple = target.as_triple();
        let arch = target.common_arch().unwrap();
        let test_label = build_test_label(&target_triple);

        let mut copy_to_dir = Vec::new();
        let extras_dir = Path::new("extras");

        let VmmTestSelections {
            filter: nextest_filter_expr,
            downloaded_artifacts,
            build,
            external_deps,
            needs_release_igvm,
        } = selections;

        // Some things can only be built on linux
        if !matches!(ctx.platform(), FlowPlatform::Linux(_))
            && (build.openhcl_standard
                || build.openhcl_standard_dev
                || build.openhcl_cvm
                || build.openhcl_linux_direct
                || build.pipette_linux
                || build.openvmm_vhost
                || build.tmk_vmm_linux
                || build.tpm_guest_tests_linux)
        {
            anyhow::bail!(
                "Selected tests require artifacts that can only be built on linux. Try building from WSL2."
            );
        }

        let openvmm_hcl_profile = if release {
            OpenvmmHclBuildProfile::OpenvmmHclShip
        } else {
            OpenvmmHclBuildProfile::Debug
        };
        let openhcl_extras_dir = extras_dir.join("openhcl");

        let mut build_openhcl = |recipe: OpenhclIgvmRecipe| -> ReadVar<OpenhclIgvmOutput> {
            let (igvm, openhcl_igvm) = ctx.new_var();
            let (extras, openhcl_igvm_extras) = ctx.new_var();

            let custom_recipe =
                if custom_kernel_modules_abs.is_some() || custom_kernel_abs.is_some() {
                    let mut details = recipe.recipe_details(release);
                    if custom_kernel_abs.is_some() {
                        details.with_uefi = true;
                    }
                    assert!(details.local_only.is_none());
                    details.local_only = Some(OpenhclIgvmRecipeDetailsLocalOnly {
                        openvmm_hcl_no_strip: false,
                        openhcl_initrd_extra_params: None,
                        custom_openvmm_hcl: None,
                        custom_openhcl_boot: None,
                        custom_kernel: custom_kernel_abs.clone(),
                        custom_sidecar: None,
                        custom_extra_rootfs: vec![],
                    });
                    OpenhclIgvmRecipeType::LocalOnlyCustom(details)
                } else {
                    OpenhclIgvmRecipeType::WellKnown(recipe.clone())
                };

            ctx.req(crate::build_openhcl_igvm_from_recipe::Request {
                build_profile: openvmm_hcl_profile,
                release_cfg: release,
                recipe: custom_recipe,
                custom_target: None,
                extra_features: BTreeSet::new(),
                disable_secure_avic,
                confidential_debug: true,
                openhcl_igvm,
                openhcl_igvm_extras,
            });

            if copy_extras {
                let dir = openhcl_extras_dir.join(recipe.non_production_tag());
                copy_to_dir.extend_from_slice(&[
                    (dir.clone(), extras.map(ctx, |x| Some(x.openvmm_hcl.bin))),
                    (dir.clone(), extras.map(ctx, |x| x.openvmm_hcl.dbg)),
                    (dir.clone(), extras.map(ctx, |x| Some(x.openhcl_boot.bin))),
                    (dir.clone(), extras.map(ctx, |x| Some(x.openhcl_boot.dbg))),
                    (dir.clone(), extras.map(ctx, |x| x.sidecar.map(|y| y.bin))),
                    (dir.clone(), extras.map(ctx, |x| x.sidecar.map(|y| y.dbg))),
                ]);
            } else {
                extras.claim_unused(ctx);
            }
            igvm
        };

        let register_openhcl_standard = build.openhcl_standard.then(|| {
            build_openhcl(match arch {
                CommonArch::X86_64 => OpenhclIgvmRecipe::X64,
                CommonArch::Aarch64 => OpenhclIgvmRecipe::Aarch64,
            })
        });
        let register_openhcl_standard_dev = build.openhcl_standard_dev.then(|| {
            build_openhcl(match arch {
                CommonArch::X86_64 => OpenhclIgvmRecipe::X64Devkern,
                CommonArch::Aarch64 => OpenhclIgvmRecipe::Aarch64Devkern,
            })
        });
        let register_openhcl_cvm = build.openhcl_cvm.then(|| {
            build_openhcl(match arch {
                CommonArch::X86_64 => OpenhclIgvmRecipe::X64Cvm,
                CommonArch::Aarch64 => unreachable!("openhcl_cvm not supported on aarch64"),
            })
        });
        let register_openhcl_linux_direct = build.openhcl_linux_direct.then(|| {
            build_openhcl(match arch {
                CommonArch::X86_64 => OpenhclIgvmRecipe::X64TestLinuxDirect,
                CommonArch::Aarch64 => {
                    unreachable!("openhcl_linux_direct not supported on aarch64")
                }
            })
        });

        let register_openvmm = build.openvmm.then(|| {
            let output = ctx.reqv(|v| crate::build_openvmm::Request {
                params: crate::build_openvmm::OpenvmmBuildParams {
                    target: target.clone(),
                    profile: CommonProfile::from_release(release),
                    // FIXME: this relies on openvmm default features
                    features: [].into(),
                },
                openvmm: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_openvmm::OpenvmmOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_openvmm::OpenvmmOutput::LinuxBin { bin: _, dbg } => Some(dbg),
                    }),
                ));
            }
            output
        });

        let register_openvmm_vhost = build.openvmm_vhost.then(|| {
            ctx.reqv(|v| crate::build_openvmm_vhost::Request {
                params: crate::build_openvmm_vhost::OpenvmmVhostBuildParams {
                    target: target.clone(),
                    profile: CommonProfile::from_release(release),
                },
                openvmm_vhost: v,
            })
        });

        let register_pipette_windows = build.pipette_windows.then(|| {
            let output = ctx.reqv(|v| crate::build_pipette::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: windows_guest_platform,
                },
                profile: CommonProfile::from_release(release),
                pipette: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_pipette::PipetteOutput::WindowsBin { exe: _, pdb } => pdb,
                        _ => unreachable!(),
                    }),
                ));
            }
            output
        });

        // The incubator's L1 runner needs pipette even when the L2 guest has no agent.
        let register_pipette_linux_musl = (build.pipette_linux || incubator_profile.is_some())
            .then(|| {
                let output = ctx.reqv(|v| crate::build_pipette::Request {
                    target: CommonTriple::Common {
                        arch,
                        platform: CommonPlatform::LinuxMusl,
                    },
                    profile: CommonProfile::from_release(release),
                    pipette: v,
                });
                if copy_extras {
                    copy_to_dir.push((
                        extras_dir.to_owned(),
                        output.map(ctx, |x| {
                            Some(match x {
                                crate::build_pipette::PipetteOutput::LinuxBin { bin: _, dbg } => {
                                    dbg
                                }
                                _ => unreachable!(),
                            })
                        }),
                    ));
                }
                output
            });

        let register_guest_test_uefi = build.guest_test_uefi.then(|| {
            let output = ctx.reqv(|v| crate::build_guest_test_uefi::Request {
                arch,
                profile: CommonProfile::from_release(release),
                guest_test_uefi: v,
            });
            if copy_extras {
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| Some(x.efi))));
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| Some(x.pdb))));
            }
            output
        });

        let register_tmks = build.tmks.then(|| {
            let output = ctx.reqv(|v| crate::build_tmks::Request {
                arch,
                profile: CommonProfile::from_release(release),
                tmks: v,
            });
            if copy_extras {
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| Some(x.dbg))));
            }
            output
        });

        let register_tpm_guest_tests_windows = build.tpm_guest_tests_windows.then(|| {
            let output = ctx.reqv(|v| crate::build_tpm_guest_tests::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: windows_guest_platform,
                },
                profile: CommonProfile::from_release(release),
                tpm_guest_tests: v,
            });

            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        TpmGuestTestsOutput::WindowsBin { pdb, .. } => pdb.clone(),
                        TpmGuestTestsOutput::LinuxBin { .. } => unreachable!(),
                    }),
                ));
            }
            output
        });

        let register_tpm_guest_tests_linux = build.tpm_guest_tests_linux.then(|| {
            let output = ctx.reqv(|v| crate::build_tpm_guest_tests::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::LinuxGnu,
                },
                profile: CommonProfile::from_release(release),
                tpm_guest_tests: v,
            });

            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            TpmGuestTestsOutput::LinuxBin { dbg, .. } => dbg.clone(),
                            TpmGuestTestsOutput::WindowsBin { .. } => unreachable!(),
                        })
                    }),
                ));
            }
            output
        });

        let register_test_igvm_agent_rpc_server = build.test_igvm_agent_rpc_server.then(|| {
            let output = ctx.reqv(|v| crate::build_test_igvm_agent_rpc_server::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::WindowsMsvc,
                },
                profile: CommonProfile::from_release(release),
                test_igvm_agent_rpc_server: v,
            });

            if copy_extras {
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| x.pdb.clone())));
            }
            output
        });

        let register_tmk_vmm = build.tmk_vmm_windows.then(|| {
            let output = ctx.reqv(|v| crate::build_tmk_vmm::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::WindowsMsvc,
                },
                profile: CommonProfile::from_release(release),
                tmk_vmm: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_tmk_vmm::TmkVmmOutput::WindowsBin { exe: _, pdb } => pdb,
                        _ => unreachable!(),
                    }),
                ));
            }
            output
        });

        let register_tmk_vmm_linux_musl = build.tmk_vmm_linux.then(|| {
            let output = ctx.reqv(|v| crate::build_tmk_vmm::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::LinuxMusl,
                },
                profile: CommonProfile::from_release(release),
                tmk_vmm: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            crate::build_tmk_vmm::TmkVmmOutput::LinuxBin { bin: _, dbg } => dbg,
                            _ => unreachable!(),
                        })
                    }),
                ));
            }
            output
        });

        let needs_prep_steps = build.prep_steps_standard || build.prep_steps_no_vmbus;
        let mut prep_steps_variants: Vec<String> = Vec::new();
        if build.prep_steps_standard {
            prep_steps_variants.push("standard".into());
        }
        if build.prep_steps_no_vmbus {
            prep_steps_variants.push("no-vmbus".into());
        }

        let register_prep_steps = needs_prep_steps.then(|| {
            let output = ctx.reqv(|v| crate::build_prep_steps::Request {
                target: target.clone(),
                profile: CommonProfile::from_release(release),
                prep_steps: v,
            });

            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_prep_steps::PrepStepsOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_prep_steps::PrepStepsOutput::LinuxBin { bin: _, dbg } => dbg,
                    }),
                ));
            }
            output
        });

        let mut build_vmgstool = |with_test_helpers| {
            let output = ctx.reqv(|v| crate::build_vmgstool::Request {
                target: target.clone(),
                profile: CommonProfile::from_release(release),
                with_crypto: true,
                with_test_helpers,
                vmgstool: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_vmgstool::VmgstoolOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_vmgstool::VmgstoolOutput::LinuxBin { bin: _, dbg } => {
                            Some(dbg)
                        }
                    }),
                ));
            }
            output
        };

        let register_vmgstool = build.vmgstool.then(|| build_vmgstool(false));

        let register_vmgstool_dev = build.vmgstool_dev.then(|| build_vmgstool(true));

        let register_incubator = incubator_profile.is_some().then(|| {
            let host_arch = match ctx.arch() {
                FlowArch::X86_64 => CommonArch::X86_64,
                FlowArch::Aarch64 => CommonArch::Aarch64,
                other => {
                    panic!("unsupported host architecture for incubator: {other:?}")
                }
            };
            let incubator_target = CommonTriple::Common {
                arch: host_arch,
                platform: CommonPlatform::LinuxGnu,
            };
            let output = ctx.reqv(|v| crate::build_incubator::Request {
                target: incubator_target,
                profile: if release {
                    CommonProfile::Release
                } else {
                    CommonProfile::Debug
                },
                incubator: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        let crate::build_incubator::IncubatorOutput { bin: _, dbg } = x;
                        dbg
                    }),
                ));
            }
            output
        });

        let register_vmm_tests_nextest_archive =
            ctx.reqv(|v| crate::build_nextest_vmm_tests::Request {
                target: target.as_triple(),
                profile: CommonProfile::from_release(release),
                build_mode: crate::build_nextest_vmm_tests::BuildNextestVmmTestsMode::Archive(v),
            });

        let register_flowey_hvlite = (build_only && cca_artifacts.is_none()).then(|| {
            let output = ctx.reqv(|v| crate::build_flowey_hvlite::Request {
                target: target.clone(),
                flowey_hvlite: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_flowey_hvlite::FloweyHvliteOutput::WindowsBin {
                            exe: _,
                            pdb,
                        } => pdb,
                        crate::build_flowey_hvlite::FloweyHvliteOutput::LinuxBin {
                            bin: _,
                            dbg,
                        } => dbg,
                    }),
                ));
            }
            output
        });

        let mut side_effects = Vec::new();

        if !copy_to_dir.is_empty() {
            side_effects.push(ctx.emit_rust_step(
                "copy additional files to test content dir",
                |ctx| {
                    let copy_to_dir = copy_to_dir
                        .into_iter()
                        .map(|(dst, src)| (dst, src.claim(ctx)))
                        .collect::<Vec<_>>();
                    let test_content_dir = test_content_dir.clone();

                    move |rt| {
                        for (dst, src) in copy_to_dir {
                            let src = rt.read(src);

                            if let Some(src) = src {
                                // TODO: specify files names for everything
                                let dst = if dst.starts_with("extras") {
                                    test_content_dir
                                        .join(dst)
                                        .join(src.file_name().context("no file name")?)
                                } else {
                                    test_content_dir.join(dst)
                                };

                                fs_err::create_dir_all(dst.parent().context("no parent")?)?;
                                fs_err::copy(src, dst)?;
                            }
                        }

                        Ok(())
                    }
                },
            ));
        }

        let built_artifacts = VmmTestsBuiltArtifacts {
            flowey_hvlite: register_flowey_hvlite,
            nextest_vmm_tests_archive: Some(register_vmm_tests_nextest_archive),
            incubator: register_incubator,
            prep_steps: register_prep_steps,
            test_igvm_agent_rpc_server: register_test_igvm_agent_rpc_server,
            openvmm: register_openvmm,
            openvmm_vhost: register_openvmm_vhost,
            pipette_windows: register_pipette_windows,
            pipette_linux_musl: register_pipette_linux_musl,
            guest_test_uefi: register_guest_test_uefi,
            openhcl_standard: register_openhcl_standard,
            openhcl_standard_dev: register_openhcl_standard_dev,
            openhcl_cvm: register_openhcl_cvm,
            openhcl_linux_direct: register_openhcl_linux_direct,
            tmks: register_tmks,
            tmk_vmm: register_tmk_vmm,
            tmk_vmm_linux_musl: register_tmk_vmm_linux_musl,
            vmgstool: register_vmgstool,
            vmgstool_dev: register_vmgstool_dev,
            tpm_guest_tests_windows: register_tpm_guest_tests_windows,
            tpm_guest_tests_linux: register_tpm_guest_tests_linux,
        };

        if build_only || fvp_single_test.is_some() {
            let initialized = ctx.reqv(|v| crate::init_vmm_tests_content_dir::Request {
                test_content_dir: ReadVar::from_static(test_content_dir.clone()),
                vmm_tests_target: target_triple.clone(),
                built_artifacts,
                is_repo_root: true,
                needs_release_igvm,
                needs_incubator_profiles: incubator_profile.is_some(),
                test_linux_kernel_override: cca_artifacts.as_ref().map(|a| a.realm_kernel.clone()),
                test_linux_initrd_override: cca_artifacts.as_ref().map(|a| a.initrd.clone()),
                cca_payload_only: is_fvp,
                done: v,
            });

            side_effects.push(initialized.clone());

            // CCA runs from the build host with resolved platform inputs. A
            // target-side Flowey script would lose those overrides.
            if let Some(artifacts) = cca_artifacts {
                let incubator_profile = incubator_profile.context("CCA requires a profile")?;
                init_artifacts_dir(ctx, &test_content_dir, skip_vhd_prompt)?;
                ctx.req(
                    crate::download_openvmm_vmm_tests_artifacts::Request::Download(
                        downloaded_artifacts,
                    ),
                );
                let disk_images_dir = ctx
                    .reqv(crate::download_openvmm_vmm_tests_artifacts::Request::GetDownloadFolder);
                let test_content_dir_var =
                    ReadVar::from_static(test_content_dir.clone()).depending_on(ctx, &initialized);
                let cca_tdisp_guest = crate::cca_tdisp_guest::stage_for_run(
                    ctx,
                    artifacts.cca_tdisp_guest_root,
                    test_content_dir_var.clone(),
                );
                let (archive, nextest_vmm_tests_archive) = ctx.new_var();
                let (incubator, incubator_write) = ctx.new_var();
                ctx.req(crate::resolve_vmm_tests_pipeline_artifacts::Request {
                    test_content_dir: test_content_dir_var.clone(),
                    vmm_tests_target: target_triple.clone(),
                    nextest_vmm_tests_archive,
                    incubator: Some(incubator_write),
                    prep_steps: None,
                    test_igvm_agent_rpc_server: None,
                });
                let repo_root = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
                let config_file = repo_root
                    .clone()
                    .map(ctx, |p| p.join(".config/nextest.toml"));
                let extra_env = ctx.reqv(|v| crate::init_vmm_tests_env::Request {
                    test_content_dir: test_content_dir_var.clone(),
                    vmm_tests_target: target_triple.clone(),
                    disk_images_dir: Some(disk_images_dir),
                    get_test_log_path: None,
                    petri_params,
                    get_env: v,
                });
                let archive_file = archive.map(ctx, |x| x.archive_file);
                let extra_env = ctx.reqv(|v| crate::write_incubator_target_runner::Request {
                    incubator: incubator.clone(),
                    incubator_profile,
                    kernel: Some(artifacts.host_kernel),
                    initrd: Some(artifacts.initrd),
                    firmware: artifacts.firmware,
                    fvp_roots: artifacts.fvp_roots,
                    cca_tdisp_guest: cca_tdisp_guest.clone(),
                    repo_root: repo_root.clone(),
                    test_content_dir: test_content_dir_var.clone(),
                    extra_share_paths: vec![archive_file.clone(), config_file.clone()],
                    extra_env: Some(extra_env),
                    qemu_binary: artifacts.qemu_binary,
                    target: target_triple.clone(),
                    nextest_env: v,
                });
                if let Some(test_name) = fvp_single_test {
                    let native_nextest = ctx.reqv(|v| {
                        flowey_lib_common::download_cargo_nextest::Request::Get(
                            target_triple.clone(),
                            v,
                        )
                    });
                    side_effects.push(ctx.emit_rust_step("stage native FVP nextest", |ctx| {
                        let native_nextest = native_nextest.claim(ctx);
                        let content = test_content_dir_var.claim(ctx);
                        move |rt| {
                            let content = rt.read(content);
                            let nextest = content.join("cargo-nextest");
                            fs_err::copy(rt.read(native_nextest), &nextest)?;
                            nextest.make_executable()?;
                            // Do not leave a prior multi-boot script beside a
                            // single-boot-only artifact set.
                            match fs_err::remove_file(content.join("run.sh")) {
                                Ok(()) => {}
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                                Err(error) => return Err(error.into()),
                            }
                            Ok(())
                        }
                    }));
                    if build_only {
                        side_effects.push(extra_env.into_side_effect());
                        ctx.emit_side_effect_step(side_effects, [done]);
                        return Ok(());
                    }
                    let platform =
                        incubator_platform.context("single-boot FVP requires a platform")?;
                    let incubator_bin = incubator.map(ctx, |output| output.bin);
                    let results = ctx.reqv(|results| crate::run_fvp_single_boot::Request {
                        platform,
                        cca_tdisp_guest,
                        incubator: incubator_bin,
                        env: extra_env,
                        content_dir: test_content_dir,
                        test_name,
                        profile: nextest_profile,
                        pre_run_deps: side_effects,
                        results,
                    });
                    let test_results = results.map(ctx, |result| result.test_results());
                    let publication_dir = results.map(ctx, |result| result.run_directory);
                    let published =
                        ctx.reqv(|done| flowey_lib_common::publish_test_results::Request {
                            test_results,
                            test_label,
                            attachments: Default::default(),
                            output_dir: Some(publication_dir),
                            upload_logs_on_success: true,
                            done,
                        });
                    ctx.emit_rust_step("report single-boot test and session outcomes", |ctx| {
                        published.claim(ctx);
                        done.claim(ctx);
                        let results = results.claim(ctx);
                        move |rt| {
                            let result = rt.read(results);
                            log::info!("native JUnit reports: {:?}", result.reported_test_outcome);
                            anyhow::ensure!(
                                result.errors.is_empty(),
                                "single-boot FVP session failed: {}; evidence: {}",
                                result.errors.join("; "),
                                result.run_directory.display()
                            );
                            log::info!("single-boot test and FVP session completed successfully");
                            Ok(())
                        }
                    });
                    return Ok(());
                }
                let nextest_bin = ctx.reqv(|v| {
                    flowey_lib_common::download_cargo_nextest::Request::Get(
                        target_lexicon::Triple::host(),
                        v,
                    )
                });
                let command = ctx.reqv(|v| flowey_lib_common::gen_cargo_nextest_run_cmd::Request {
                    run_kind_deps:
                        flowey_lib_common::gen_cargo_nextest_run_cmd::RunKindDeps::RunFromArchive {
                            archive_file,
                            nextest_bin,
                            target: target_lexicon::Triple::host(),
                        },
                    working_dir: repo_root,
                    config_file,
                    tool_config_files: Vec::new(),
                    nextest_profile: nextest_profile.as_str().into(),
                    nextest_filter_expr: Some(nextest_filter_expr),
                    run_ignored: false,
                    fail_fast: None,
                    extra_env: Some(extra_env),
                    extra_commands: needs_prep_steps.then(|| {
                        ReadVar::from_static(
                            prep_steps_variants
                                .iter()
                                .map(|variant| {
                                    (
                                        test_content_dir.join("prep_steps").into_os_string(),
                                        vec![OsString::from(variant)],
                                    )
                                })
                                .collect(),
                        )
                    }),
                    portable: false,
                    command: v,
                });
                side_effects.push(ctx.emit_rust_step("write CCA host test script", |ctx| {
                    let command = command.claim(ctx);
                    move |rt| {
                        let command = rt.read(command);
                        let script = test_content_dir.join("run.sh");
                        fs_err::write(&script, cca_host_test_script(&command, repetitions)?)?;
                        script.make_executable()?;
                        log::info!("Run the CCA tests on this host with: {}", script.display());
                        Ok(())
                    }
                }));
                ctx.emit_side_effect_step(side_effects, [done]);
                return Ok(());
            }

            side_effects.push(ctx.emit_rust_step("write script", |ctx| {
                // place this job at the end so the log is visible for convenience
                initialized.claim(ctx);
                move |rt| {
                    let (script_name, dir, flowey_hvlite_bin) = match target_triple.operating_system
                    {
                        target_lexicon::OperatingSystem::Windows => {
                            ("run.ps1", "$PSScriptRoot", ".\\flowey_hvlite.exe")
                        }
                        _ => (
                            "run.sh",
                            "\"$(dirname \"${BASH_SOURCE[0]}\")\"",
                            "./flowey_hvlite",
                        ),
                    };

                    let target_cli = match target {
                        CommonTriple::AARCH64_WINDOWS_MSVC => "windows-aarch64",
                        CommonTriple::X86_64_WINDOWS_MSVC => "windows-x64",
                        CommonTriple::X86_64_LINUX_GNU => "linux-x64",
                        CommonTriple::AARCH64_LINUX_MUSL => "linux-aarch64-musl",
                        _ => unreachable!(),
                    };

                    let mut run_target_args: Vec<OsString> = vec![
                        "cd".into(),
                        dir.into(),
                        ";".into(),
                        flowey_hvlite_bin.into(),
                        "pipeline".into(),
                        "run".into(),
                        "vmm-tests-run-target".into(),
                        "--target".into(),
                        target_cli.into(),
                        "--dir".into(),
                        ".".into(),
                        "--filter".into(),
                        format!("'{nextest_filter_expr}'").into(),
                        "--repetitions".into(),
                        repetitions.get().to_string().into(),
                    ];

                    if !downloaded_artifacts.is_empty() {
                        run_target_args.push("--artifacts".into());
                        run_target_args.push(
                            downloaded_artifacts
                                .iter()
                                .map(|a| a.name())
                                .collect::<Vec<_>>()
                                .join(",")
                                .into(),
                        );
                    }

                    if !prep_steps_variants.is_empty() {
                        run_target_args.push("--prep-steps".into());
                        run_target_args.push(prep_steps_variants.join(",").into());
                    }

                    if skip_vhd_prompt {
                        run_target_args.push("--skip-vhd-prompt".into());
                    }

                    if matches!(
                        nextest_profile,
                        crate::run_cargo_nextest_run::NextestProfile::Ci
                    ) {
                        run_target_args.push("--ci-profile".into());
                    }

                    if !petri_params.reuse_prepped_vhds {
                        run_target_args.push("--no-reuse-prepped-vhds".into());
                    }

                    if matches!(
                        external_deps,
                        VmmTestsExternalDeps::Windows(ref deps) if deps.hardware_isolation
                    ) {
                        run_target_args.push("--needs-hardware-isolation".into());
                    }

                    if build.test_igvm_agent_rpc_server {
                        run_target_args.push("--needs-igvm-agent".into());
                    }

                    if let Some(profile) = &incubator_profile {
                        run_target_args.push("--incubator".into());
                        run_target_args.push(profile.to_string().into());
                    }

                    let dst = test_content_dir.join(script_name);

                    fs_err::write(
                        &dst,
                        run_target_args.join(OsStr::new(" ")).as_encoded_bytes(),
                    )?;
                    dst.make_executable()?;

                    match target_triple.operating_system {
                        target_lexicon::OperatingSystem::Windows => {
                            let dst = if flowey_lib_common::_util::running_in_wsl(rt) {
                                flowey_lib_common::_util::wslpath::linux_to_win(rt, dst)
                                    .to_string_lossy()
                                    .replace("\\", "\\\\")
                            } else {
                                dst.to_string_lossy().to_string()
                            };
                            log::info!("Run the vmm tests with: powershell.exe {dst}");
                        }
                        _ => {
                            log::info!("Run the vmm tests with: {}", dst.display());
                        }
                    }

                    Ok(())
                }
            }));
        } else {
            init_artifacts_dir(ctx, &test_content_dir, skip_vhd_prompt)?;

            let test_content_config = TestContentConfig::Uninitialized {
                test_content_dir: Some(ReadVar::from_static(test_content_dir)),
                built_artifacts,
                needs_release_igvm,
            };

            side_effects.push(ctx.reqv(|v| {
                crate::_jobs::consume_and_test_nextest_vmm_tests_archive::Params {
                    junit_test_label: test_label,
                    target: target_triple,
                    nextest_profile,
                    nextest_filter_expr: Some(nextest_filter_expr),
                    test_content_config,
                    downloaded_artifacts,
                    prep_steps_variants,
                    external_deps,
                    incubator_profile,
                    cca_artifacts,
                    upload_logs_on_success: true,
                    fail_job_on_test_fail: true,
                    repetitions,
                    petri_params,
                    test_content_dir_as_repo_root: true,
                    done: v,
                }
            }));
        }

        ctx.emit_side_effect_step(side_effects, [done]);

        Ok(())
    }
}

pub(crate) fn build_test_label(target: &target_lexicon::Triple) -> String {
    let arch = CommonArch::from_triple(target).unwrap();
    let arch_tag = match arch {
        CommonArch::X86_64 => "x64",
        CommonArch::Aarch64 => "aarch64",
    };
    let platform_tag = match target.operating_system {
        target_lexicon::OperatingSystem::Windows => "windows",
        target_lexicon::OperatingSystem::Linux => "linux",
        _ => unreachable!(),
    };
    format!("{arch_tag}-{platform_tag}-vmm-tests")
}

pub(crate) fn init_artifacts_dir(
    ctx: &mut NodeCtx<'_>,
    test_content_dir: &Path,
    skip_vhd_prompt: bool,
) -> anyhow::Result<()> {
    let vmm_test_artifacts_dir = test_content_dir.join("images");
    ctx.config(crate::download_openvmm_vmm_tests_artifacts::Config {
        custom_cache_dir: Some(vmm_test_artifacts_dir.clone()),
        skip_prompt: Some(skip_vhd_prompt),
        ..Default::default()
    });
    Ok(())
}
