// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pipeline to discover artifacts and run VMM tests in a single command.
//!
//! This pipeline:
//! 1. Discovers required artifacts for the specified test filter (at pipeline
//!    construction time)
//! 2. Builds the necessary dependencies
//! 3. Runs the tests

use anyhow::Context as _;
use flowey::node::prelude::ReadVar;
use flowey::node::prelude::fs_err;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::VmmTestSelections;
use flowey_lib_hvlite::common::CommonPlatform;
use flowey_lib_hvlite::common::CommonTriple;
use flowey_lib_hvlite::init_vmm_tests_content_dir::VmmTestsBuiltArtifactsSelections;
use flowey_lib_hvlite::init_vmm_tests_content_dir::VmmTestsPreBuiltArtifactsSelections;
use flowey_lib_hvlite::init_vmm_tests_env::PetriParams;
use flowey_lib_hvlite::install_vmm_tests_external_deps::VmmTestsExternalDeps;
use flowey_lib_hvlite::install_vmm_tests_external_deps::VmmTestsExternalDepsLinux;
use flowey_lib_hvlite::install_vmm_tests_external_deps::VmmTestsExternalDepsWindows;
use petri_artifacts_core::ArtifactId;
use petri_artifacts_core::ArtifactListOutput;
use petri_artifacts_core::ArtifactTarget;
use petri_artifacts_core::artifact_from_id;
use petri_artifacts_vmm_test::ErasedVmmTestImage;
use petri_artifacts_vmm_test::vmm_test_image_from_id;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

/// Build and run VMM tests with automatic artifact discovery
#[derive(clap::Args)]
#[clap(group(
    clap::ArgGroup::new("fvp_archive_hashes")
        .args(["fvp_package_sha256", "fvp_payload_sha256"])
        .multiple(true)
))]
pub struct VmmTestsRunCli {
    /// Specify what target to build the VMM tests for
    ///
    /// If not specified, defaults to the current host target.
    #[clap(long)]
    target: Option<VmmTestTargetCli>,

    /// Directory for the output artifacts.
    ///
    /// If not specified, defaults to `target/vmm_tests`.
    /// WSL-to-Windows runs must override this to a Windows-accessible output
    /// directory (a DrvFs mount like `/mnt/c/...`) only when the selected tests
    /// use disk images that require a Windows filesystem (Hyper-V disks, or
    /// VHDX / dynamic VHD1 images); otherwise the default works.
    #[clap(long)]
    dir: Option<PathBuf>,

    /// Test filter (nextest filter expression)
    ///
    /// Examples:
    ///   - `test(alpine)` - run tests with "alpine" in the name
    ///   - `test(/^boot_/)` - run tests starting with "boot_"
    ///   - `all()` - run all tests
    #[clap(long, default_value = "all()")]
    filter: String,

    /// pass `--verbose` to cargo
    #[clap(long)]
    verbose: bool,
    /// Automatically install any missing required dependencies.
    #[clap(long)]
    install_missing_deps: bool,

    /// Release build instead of debug build
    #[clap(long)]
    release: bool,

    /// Build only, do not run
    #[clap(long)]
    build_only: bool,
    /// Copy extras to output dir (symbols, etc)
    #[clap(long)]
    copy_extras: bool,

    /// Skip the interactive VHD download prompt
    #[clap(long)]
    skip_vhd_prompt: bool,

    /// Download all disk images upfront instead of streaming on demand.
    ///
    /// By default, VHD/ISO disk images are streamed on demand via HTTP
    /// and cached locally, avoiding large upfront downloads. Use this
    /// flag to force all images to be downloaded before tests run.
    #[clap(long)]
    no_lazy_fetch: bool,

    /// Optional: custom kernel modules
    #[clap(long)]
    custom_kernel_modules: Option<PathBuf>,
    /// Optional: custom kernel image
    #[clap(long)]
    custom_kernel: Option<PathBuf>,
    /// Optional: custom UEFI firmware (MSVM.fd) to use instead of the
    /// downloaded release. Path to a locally-built MSVM.fd file.
    #[clap(long)]
    custom_uefi_firmware: Option<PathBuf>,

    /// use the nextest CI profile rather than the default one
    #[clap(long)]
    ci_profile: bool,

    /// Don't reuse prepped vhds, even if they already exist.
    /// Use when making changes to prep_steps
    #[clap(long)]
    no_reuse_prepped_vhds: bool,

    /// Disable secure AVIC support for SNP. This adds the
    /// `disable_secure_avic` cargo feature and sets `secure_avic` to
    /// `disabled` in the IGVM manifest.
    #[clap(long)]
    pub disable_secure_avic: bool,

    /// How many times to run the tests
    #[clap(long)]
    repetitions: Option<u64>,

    /// Release containing the selected prebuilt FVP packages.
    #[clap(long, requires = "fvp_archive_hashes")]
    fvp_deps_version: Option<String>,

    /// Local FVP firmware archive instead of downloading the release package.
    #[clap(long, requires_all = ["fvp_deps_version", "fvp_package_sha256"])]
    fvp_package_archive: Option<PathBuf>,

    /// Expected SHA-256 of the FVP firmware archive.
    #[clap(long, requires = "fvp_deps_version")]
    fvp_package_sha256: Option<String>,

    /// Local FVP host and reference-guest payload archive.
    #[clap(long, requires_all = ["fvp_deps_version", "fvp_payload_sha256"])]
    fvp_payload_archive: Option<PathBuf>,

    /// Expected SHA-256 of the FVP host and reference-guest payload archive.
    #[clap(long, requires = "fvp_deps_version")]
    fvp_payload_sha256: Option<String>,
}

struct CargoNextestListRequest<'a> {
    repo_root: &'a Path,
    target: &'a str,
    filter: &'a str,
    release: bool,
    include_ignored: bool,
}

struct RustSuite {
    binary_path: PathBuf,
    testcases: Vec<String>,
}

/// Result of resolving artifact requirements to build/download selections
#[derive(Debug)]
struct ResolvedArtifactSelections {
    target: target_lexicon::Triple,

    /// What to build
    build: VmmTestsBuiltArtifactsSelections,
    /// Prebuilt artifacts to download
    prebuilt_artifacts: VmmTestsPreBuiltArtifactsSelections,
    /// Prep steps variants
    prep_steps_variants: BTreeSet<String>,
    /// What to download
    downloads: BTreeSet<ErasedVmmTestImage>,
    /// Downloads that must happen even when lazy fetch is enabled (e.g.
    /// VHDs needed by prep_steps, which copies them to create prepped images).
    force_downloads: BTreeSet<ErasedVmmTestImage>,
    /// Whether any of the tests require Hyper-V
    needs_hyperv: bool,
    /// Whether any of the tests require hardware isolation
    needs_hardware_isolation: bool,

    // Relative paths to artifacts used by the pipeline
    flowey_hvlite_path: Option<PathBuf>,

    // TODO: refactor these last two to use one artifact per arch so that
    // they can be part of `VmmTestsPreBuiltArtifactsSelections`.
    needs_virtio_win_drivers: bool,
    needs_release_igvm: bool,
}

impl ResolvedArtifactSelections {
    fn new(target: target_lexicon::Triple, build_only: bool) -> anyhow::Result<Self> {
        let mut build = VmmTestsBuiltArtifactsSelections::default();
        build.require_nextest_vmm_tests_archive_for(ArtifactTarget::Triple(target.clone()))?;
        let flowey_hvlite_path = build_only
            .then(|| build.require_flowey_hvlite_for(ArtifactTarget::Triple(target.clone())))
            .transpose()?;

        Ok(Self {
            target: target.clone(),

            build,
            prebuilt_artifacts: Default::default(),
            prep_steps_variants: Default::default(),
            downloads: Default::default(),
            force_downloads: Default::default(),
            needs_hyperv: Default::default(),
            needs_hardware_isolation: Default::default(),

            flowey_hvlite_path,

            needs_virtio_win_drivers: Default::default(),
            needs_release_igvm: Default::default(),
        })
    }
}

impl IntoPipeline for VmmTestsRunCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("vmm-tests-run is for local use only")
        }

        let Self {
            target,
            dir,
            mut filter,
            verbose,
            install_missing_deps,
            release,
            build_only,
            copy_extras,
            skip_vhd_prompt,
            no_lazy_fetch,
            custom_kernel_modules,
            custom_kernel,
            custom_uefi_firmware,
            ci_profile,
            no_reuse_prepped_vhds,
            disable_secure_avic,
            repetitions,
            fvp_deps_version,
            fvp_package_archive,
            fvp_package_sha256,
            fvp_payload_archive,
            fvp_payload_sha256,
        } = self;

        let repetitions =
            NonZeroU64::new(repetitions.unwrap_or(1)).context("repetitions must not be zero")?;

        let target = resolve_target(target, backend_hint)?;
        let target_os = target.as_triple().operating_system;
        let target_architecture = target.common_arch()?;
        let target_str = target.as_triple().to_string();

        // Windows *guest* payloads (e.g. pipette) must be PE binaries even when
        // the VMM host target is Linux. On a non-WSL Linux build host the MSVC
        // toolchain / Windows SDK is unavailable, so cross-compile those guest
        // binaries with the GNU (mingw-w64) toolchain instead.
        let windows_guest_platform =
            if matches!(FlowPlatform::host(backend_hint), FlowPlatform::Linux(_))
                && !flowey_cli::running_in_wsl()
            {
                CommonPlatform::WindowsGnu
            } else {
                CommonPlatform::WindowsMsvc
            };

        let repo_root = crate::repo_root();

        let include_ignored = build_only;

        // Run artifact discovery inline at pipeline construction time since
        // flowey doesn't support conditional requests yet
        log::info!(
            "Discovering artifacts for filter: {} (target: {})",
            filter,
            target
        );

        // Determine which tests match the filter
        let mut suites = run_cargo_nextest_list(CargoNextestListRequest {
            repo_root: &repo_root,
            target: &target_str,
            filter: &filter,
            release,
            // When using build-only mode, we need to enumerate tests that could be
            // run on any system so that we build all necessary dependencies. By default
            // petri marks incompatible tests as ignored.
            //
            include_ignored,
        })?;

        if suites.is_empty() {
            anyhow::bail!("No tests found for the given filter");
        }

        // Query for the required artifacts
        let mut artifacts = Vec::new();
        let mut nested = Vec::new();
        let mut excluded = Vec::new();
        for suite in suites.values_mut() {
            let mut output = query_configured_suite_artifacts(
                suite,
                fvp_deps_version.is_some(),
                &mut query_test_binary_artifacts,
                &mut excluded,
            )?;
            nested.append(&mut output.nested);
            artifacts.append(&mut output.into_artifacts_list());
        }
        suites.retain(|_, suite| !suite.testcases.is_empty());
        if !excluded.is_empty() {
            log::info!(
                "Skipping {} FVP package-dependent tests; configure \
                 --fvp-deps-version and package digests to include them",
                excluded.len()
            );
            let exclusions = excluded
                .into_iter()
                .map(|name| format!("test(={name})"))
                .collect::<Vec<_>>()
                .join(" + ");
            filter = format!("({filter}) & !({exclusions})");
            anyhow::ensure!(
                !suites.is_empty(),
                "selected tests require FVP packages; specify --fvp-deps-version \
                 and the required firmware or payload SHA-256"
            );
        }

        // Query artifacts for nested tests
        // Note that this requires qemu-user-static to be installed to run
        // binaries for other architectures.
        for (id, tests) in nested {
            let nested_target_str = artifact_from_id(&id)
                .with_context(|| format!("unknown artifact handle: {id}"))?
                .target_triple()
                .context("archive artifact should have associated target triple")?
                .to_string();
            log::info!(
                "{} nested tests require artifacts for {}",
                tests.len(),
                nested_target_str
            );
            let nested_filter = tests
                .into_iter()
                .map(|t| format!("test(={t})"))
                .collect::<Vec<_>>()
                .join(" + ");

            // Artifact discovery only needs to execute the test binary far enough
            // to dump its static artifact metadata (`--list-required-artifacts`),
            // which never boots a VM. So we run it directly rather than through the
            // full emulator. The binary is built for the test target, so on a foreign
            // host this relies on the binary being executable (natively, or via
            // binfmt/user-mode emulation).
            //
            // Running outside the real guest means the per-test host capability
            // checks (the source of nextest's `#[ignore]` flag) would wrongly drop
            // emulated tests, so for the emulated path we enumerate ignored tests
            // too — their artifacts still need to be built.
            let nested_suites = run_cargo_nextest_list(CargoNextestListRequest {
                repo_root: &repo_root,
                target: &nested_target_str,
                filter: &nested_filter,
                release,
                include_ignored: true,
            })?;
            for suite in nested_suites.values() {
                let output = query_test_binary_artifacts(suite)?;
                if !output.nested.is_empty() {
                    anyhow::bail!("recursive nesting not supported");
                }
                anyhow::ensure!(
                    fvp_deps_version.is_some() || !uses_fvp_packages(&output),
                    "nested tests require FVP packages; specify --fvp-deps-version \
                     and the required firmware or payload SHA-256"
                );
                artifacts.append(&mut output.into_artifacts_list());
            }
        }

        // Resolve to build selections
        let mut resolved = ResolvedArtifactSelections::new(target.as_triple(), build_only)?;
        for artifact in artifacts {
            resolved.resolve_artifact(&artifact)?;
        }
        anyhow::ensure!(
            fvp_deps_version.is_none()
                || resolved.prebuilt_artifacts.fvp_cca_platform_aarch64
                || resolved.prebuilt_artifacts.fvp_cca_payload_aarch64,
            "FVP package options require a test that uses FVP firmware or payloads"
        );
        anyhow::ensure!(
            fvp_package_sha256.is_none() || resolved.prebuilt_artifacts.fvp_cca_platform_aarch64,
            "FVP firmware options require a test that uses the FVP backend"
        );
        anyhow::ensure!(
            fvp_payload_sha256.is_none() || resolved.prebuilt_artifacts.fvp_cca_payload_aarch64,
            "FVP payload options require a test that uses the FVP payload"
        );

        // Determine whether we need hyper-v and/or hardware isolation
        resolved.needs_hyperv = suites
            .values()
            .any(|s| s.testcases.iter().any(|name| name.contains("hyperv")));
        resolved.needs_hardware_isolation = suites.values().any(|s| {
            s.testcases
                .iter()
                .any(|name| name.contains("snp") || name.contains("tdx"))
        });

        // Determine lazy fetch mode.
        //
        // By default, VHD/ISO downloads are skipped and disk images are
        // streamed on demand via HTTP (with local SQLite caching). This
        // avoids multi-GB upfront downloads for dev-inner-loop scenarios.
        //
        // Lazy fetch is disabled for all downloads when the user passes
        // --no-lazy-fetch and for any downloads that are used by a selected
        // Hyper-V test.
        //
        // When both Hyper-V and non-Hyper-V tests are selected, only the
        // artifacts required by Hyper-V tests are downloaded upfront; the
        // rest are lazy-fetched.
        if no_lazy_fetch {
            log::info!("Lazy fetch disabled");
        } else {
            let mut hyperv_tests: usize = 0;
            let mut hyperv_artifacts = Vec::new();
            for suite in suites.values() {
                let hyperv_testcases: Vec<_> = suite
                    .testcases
                    .iter()
                    .filter(|name| name.contains("hyperv"))
                    .cloned()
                    .collect();

                if !hyperv_testcases.is_empty() {
                    hyperv_tests += hyperv_testcases.len();
                    hyperv_artifacts.append(
                        &mut query_test_binary_artifacts(&RustSuite {
                            binary_path: suite.binary_path.clone(),
                            testcases: hyperv_testcases,
                        })?
                        .into_artifacts_list(),
                    );
                }
            }

            resolved.downloads.retain(|a| !a.supports_blob_disk());

            // Re-add force_downloads (prep_steps dependencies) that were removed.
            resolved
                .downloads
                .extend(resolved.force_downloads.iter().cloned());

            if hyperv_tests == 0 {
                log::info!("Lazy fetch enabled: disk images will be streamed on demand via HTTP");
            } else {
                log::info!(
                    "Downloading disk images required by {} Hyper-V tests",
                    hyperv_tests
                );
            }

            // Re-add only the downloads needed for hyper-v. Other selections should
            // remain the same since resolve_artifact can only add selections
            for artifact in hyperv_artifacts {
                resolved.resolve_artifact(&artifact)?;
            }
        }

        log::info!("Resolved selections: {:?}", resolved);

        // Validate the output directory now that we know which disk images the
        // selected tests need. When targeting Windows from WSL, the only hard
        // filesystem constraint is that certain disk images must live on a
        // Windows filesystem rather than a `\\wsl$` 9p path:
        //
        // - Hyper-V tests attach their VHDs to a real Hyper-V VM, whose worker
        //   process can't open disks over 9p, so any Hyper-V disk needs a
        //   Windows path regardless of format.
        // - OpenVMM opens fixed VHD1, VMGS, and raw/ISO images as plain files
        //   (fine over 9p), but routes VHDX (and dynamic/differencing VHD1)
        //   disks through the Windows virtual-disk mount API, which requires a
        //   real local volume. The only such artifact today is the `.vhdx`.
        //
        // All other cases (streamed disks, or fixed-VHD1 files even with
        // `--no-lazy-fetch`) work fine from the default WSL-side directory.
        let needs_windows_disk = !build_only
            && (resolved.needs_hyperv
                || resolved
                    .downloads
                    .iter()
                    .any(|a| a.filename().ends_with(".vhdx")));
        validate_output_dir(dir.as_deref(), target_os, needs_windows_disk)?;
        let test_content_dir = dir.unwrap_or_else(|| repo_root.join("target").join("vmm_tests"));
        std::fs::create_dir_all(&test_content_dir).context("failed to create output directory")?;
        for archive in [&fvp_package_archive, &fvp_payload_archive]
            .into_iter()
            .flatten()
        {
            let archive = fs_err::canonicalize(archive)
                .context("cannot resolve the local FVP dependency archive")?;
            let output = fs_err::canonicalize(&test_content_dir)
                .context("cannot resolve the test output directory")?;
            anyhow::ensure!(
                !archive.starts_with(output),
                "local FVP dependency archives must be outside the test output directory"
            );
        }

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(repo_root),
        );

        let mut pipeline = Pipeline::new();

        let mut job = pipeline.new_job(
            FlowPlatform::host(backend_hint),
            FlowArch::host(backend_hint),
            "build all dependencies and run vmm tests",
        );

        job = job.dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init);

        if let Some(version) = fvp_deps_version {
            let local_archive = fvp_package_archive.map(std::path::absolute).transpose()?;
            let local_payload_archive = fvp_payload_archive.map(std::path::absolute).transpose()?;
            job = job.dep_on(move |_| {
                flowey_lib_hvlite::_jobs::cfg_versions::Request::FvpCcaPlatform {
                    version,
                    archive_sha256: fvp_package_sha256,
                    local_archive: local_archive.map(ReadVar::from_static),
                    payload_archive_sha256: fvp_payload_sha256,
                    local_payload_archive: local_payload_archive.map(ReadVar::from_static),
                }
            });
        }

        // Override kernel with local paths if both kernel and modules are specified
        if let (Some(kernel_path), Some(modules_path)) =
            (custom_kernel.clone(), custom_kernel_modules.clone())
        {
            job =
                job.dep_on(
                    move |_| flowey_lib_hvlite::_jobs::cfg_versions::Request::LocalKernel {
                        arch: target_architecture,
                        kernel: ReadVar::from_static(kernel_path),
                        modules: ReadVar::from_static(modules_path),
                    },
                );
        }

        // Override UEFI firmware with a local MSVM.fd path
        if let Some(fw_path) = custom_uefi_firmware {
            job = job.dep_on(move |_| {
                flowey_lib_hvlite::_jobs::cfg_versions::Request::LocalUefi(
                    target_architecture,
                    ReadVar::from_static(fw_path),
                )
            });
        }

        job = job
            .dep_on(
                |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                    hvlite_repo_source: openvmm_repo.clone(),
                },
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: install_missing_deps,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked: false,
                deny_warnings: false,
                no_incremental: false,
            })
            .dep_on(|ctx| {
                flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::Params {
                    target,
                    windows_guest_platform,
                    test_content_dir,
                    selections: selections_from_resolved(filter, resolved, target_os),
                    release,
                    build_only,
                    copy_extras,
                    custom_kernel_modules,
                    custom_kernel,
                    skip_vhd_prompt,
                    nextest_profile: if ci_profile {
                        flowey_lib_hvlite::run_cargo_nextest_run::NextestProfile::Ci
                    } else {
                        flowey_lib_hvlite::run_cargo_nextest_run::NextestProfile::Default
                    },
                    petri_params: PetriParams {
                        disable_remote_artifacts: false,
                        reuse_prepped_vhds: !no_reuse_prepped_vhds,
                        require_2mb_hugetlb: false, // TODO
                    },
                    disable_secure_avic,
                    repetitions,
                    done: ctx.new_done_handle(),
                }
            });

        job.finish();

        Ok(pipeline)
    }
}

/// Get test binaries and associated matching tests for a given nextest filter.
// TODO: this function should really be a flowey node without automatic
// dependency installation, but that would require conditional requests.
fn run_cargo_nextest_list<'a>(
    req: CargoNextestListRequest<'a>,
) -> anyhow::Result<BTreeMap<String, RustSuite>> {
    let CargoNextestListRequest {
        repo_root,
        target,
        filter,
        release,
        include_ignored,
    } = req;

    // Check that cargo-nextest is available
    let nextest_check = Command::new("cargo")
        .args(["nextest", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match nextest_check {
        Ok(status) if status.success() => {}
        _ => anyhow::bail!(
            "cargo-nextest not found. Run 'cargo install --locked cargo-nextest' first."
        ),
    }

    // Step 1: Use nextest to resolve the filter expression to test names and
    // get the binary path
    let mut cmd = Command::new("cargo");
    cmd.stderr(Stdio::inherit());
    cmd.current_dir(repo_root).args([
        "nextest",
        "list",
        "-p",
        "vmm_tests",
        "--target",
        target,
        "--filter-expr",
        filter,
        "--message-format",
        "json",
    ]);
    if release {
        cmd.arg("--release");
    }
    if include_ignored {
        cmd.args(["--run-ignored", "all"]);
    }
    let nextest_output = cmd.output().context("failed to run cargo nextest list")?;
    anyhow::ensure!(nextest_output.status.success(), "cargo nextest list failed",);
    let nextest_stdout = String::from_utf8(nextest_output.stdout)
        .map_err(|e| anyhow::anyhow!("nextest output is not valid UTF-8: {}", e))?;

    parse_nextest_output(&nextest_stdout)
}

/// Parse `cargo nextest list --message-format json` output to extract test
/// names and binary path.
fn parse_nextest_output(stdout: &str) -> anyhow::Result<BTreeMap<String, RustSuite>> {
    let json: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| anyhow::anyhow!("failed to parse nextest JSON output: {}", e))?;

    let mut suites = BTreeMap::new();

    for (name, suite) in json
        .get("rust-suites")
        .and_then(|s| s.as_object())
        .context("no rust-suites object")?
    {
        let binary_path = PathBuf::from(
            suite
                .get("binary-path")
                .and_then(|v| v.as_str())
                .context("no binary-path str")?,
        );

        let testcases: Vec<_> = suite
            .get("testcases")
            .and_then(|t| t.as_object())
            .context("no testcases object")?
            .iter()
            .filter(|(_, test_info)| {
                test_info
                    .get("filter-match")
                    .and_then(|fm| fm.get("status"))
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s == "matches")
            })
            .map(|(test_name, _)| test_name.to_owned())
            .collect();

        if !testcases.is_empty() {
            suites.insert(
                name.to_owned(),
                RustSuite {
                    binary_path,
                    testcases,
                },
            );
        }
    }

    Ok(suites)
}

/// Runs the test binary with `--list-required-artifacts --tests-from-stdin`
/// and returns all the required and optional artifacts for all test defined
/// in the RustSuite.
fn query_test_binary_artifacts(suite: &RustSuite) -> anyhow::Result<ArtifactListOutput> {
    log::info!("Using test binary: {}", suite.binary_path.display());
    log::info!("Querying artifacts for {} tests", suite.testcases.len());

    petri_artifacts_core::query_test_binary_artifacts(&suite.binary_path, &suite.testcases)
}

fn uses_fvp_packages(output: &ArtifactListOutput) -> bool {
    use petri_artifacts_common::artifacts::FVP_CCA_PAYLOAD_AARCH64;
    use petri_artifacts_common::artifacts::FVP_CCA_PLATFORM_AARCH64;

    output.required.iter().chain(&output.optional).any(|id| {
        matches!(
            id.as_str(),
            FVP_CCA_PLATFORM_AARCH64::GLOBAL_UNIQUE_ID | FVP_CCA_PAYLOAD_AARCH64::GLOBAL_UNIQUE_ID
        )
    })
}

fn query_configured_suite_artifacts(
    suite: &mut RustSuite,
    fvp_configured: bool,
    query: &mut impl FnMut(&RustSuite) -> anyhow::Result<ArtifactListOutput>,
    excluded: &mut Vec<String>,
) -> anyhow::Result<ArtifactListOutput> {
    let output = query(suite)?;
    if fvp_configured || !uses_fvp_packages(&output) {
        return Ok(output);
    }

    let mut combined = ArtifactListOutput {
        required: Vec::new(),
        optional: Vec::new(),
        nested: Vec::new(),
    };
    let mut retained = Vec::new();
    for name in &suite.testcases {
        let mut output = query(&RustSuite {
            binary_path: suite.binary_path.clone(),
            testcases: vec![name.clone()],
        })?;
        if uses_fvp_packages(&output) {
            excluded.push(name.clone());
            continue;
        }
        combined.required.append(&mut output.required);
        combined.optional.append(&mut output.optional);
        combined.nested.append(&mut output.nested);
        retained.push(name.clone());
    }
    suite.testcases = retained;
    Ok(combined)
}

#[derive(clap::ValueEnum, Copy, Clone)]
pub(crate) enum VmmTestTargetCli {
    /// Windows Aarch64
    WindowsAarch64,
    /// Windows X64
    WindowsX64,
    /// Linux X64
    LinuxX64,
}

/// Resolve a CLI target option to a CommonTriple, defaulting to the host.
pub(crate) fn resolve_target(
    target: Option<VmmTestTargetCli>,
    backend_hint: PipelineBackendHint,
) -> anyhow::Result<CommonTriple> {
    let target = if let Some(t) = target {
        t
    } else {
        match (
            FlowArch::host(backend_hint),
            FlowPlatform::host(backend_hint),
        ) {
            (FlowArch::Aarch64, FlowPlatform::Windows) => VmmTestTargetCli::WindowsAarch64,
            (FlowArch::X86_64, FlowPlatform::Windows) => VmmTestTargetCli::WindowsX64,
            (FlowArch::X86_64, FlowPlatform::Linux(_)) => VmmTestTargetCli::LinuxX64,
            _ => anyhow::bail!("unsupported host"),
        }
    };

    Ok(match target {
        VmmTestTargetCli::WindowsAarch64 => CommonTriple::AARCH64_WINDOWS_MSVC,
        VmmTestTargetCli::WindowsX64 => CommonTriple::X86_64_WINDOWS_MSVC,
        VmmTestTargetCli::LinuxX64 => CommonTriple::X86_64_LINUX_GNU,
    })
}

/// Validate the output directory path based on the current platform.
///
/// When running under WSL and targeting Windows, some disk images must live on
/// a Windows-accessible path (a DrvFs mount like `/mnt/c/...`) rather than a
/// `\\wsl$` 9p path: Hyper-V disks (attached to a real Hyper-V VM) and VHDX /
/// dynamic VHD1 images (opened via the Windows virtual-disk mount API). This
/// constraint only applies when the selected tests actually use such a disk
/// (`needs_windows_disk`); fixed-VHD1, VMGS, ISO, and streamed disks work fine
/// from the WSL side. On native Windows or Linux this check is a no-op.
fn validate_output_dir(
    dir: Option<&Path>,
    target_os: target_lexicon::OperatingSystem,
    needs_windows_disk: bool,
) -> anyhow::Result<()> {
    if needs_windows_disk
        && flowey_cli::running_in_wsl()
        && matches!(target_os, target_lexicon::OperatingSystem::Windows)
    {
        if let Some(dir) = dir {
            if !flowey_cli::is_wsl_windows_path(dir) {
                anyhow::bail!(
                    "When targeting Windows from WSL, --dir must be a path on Windows \
                        (i.e., on a DrvFs mount like /mnt/c/vmm_tests) because the selected \
                        tests use disk images that require a Windows filesystem (Hyper-V \
                        disks, or VHDX / dynamic VHD1 images). \
                        Got: {}",
                    dir.display()
                );
            }
        } else {
            anyhow::bail!(
                "The selected tests use disk images that require a Windows filesystem \
                    (Hyper-V disks, or VHDX / dynamic VHD1 images) when targeting Windows \
                    from WSL. Specify an output directory on a DrvFs mount with --dir \
                    (e.g., --dir /mnt/c/vmm_tests)."
            )
        }
    }
    Ok(())
}

/// Resolve `ResolvedArtifactSelections` to `VmmTestSelections`.
fn selections_from_resolved(
    filter: String,
    resolved: ResolvedArtifactSelections,
    target_os: target_lexicon::OperatingSystem,
) -> VmmTestSelections {
    let ResolvedArtifactSelections {
        target,
        build,
        prebuilt_artifacts,
        prep_steps_variants,
        downloads,
        force_downloads: _,
        needs_hyperv,
        needs_hardware_isolation,
        flowey_hvlite_path,
        needs_virtio_win_drivers,
        needs_release_igvm,
    } = resolved;
    let needs_whp = build
        .openvmm_for(ArtifactTarget::Triple(target))
        .expect("no native openvmm");

    VmmTestSelections {
        filter,
        downloaded_artifacts: downloads.into_iter().collect(),
        build,
        prebuilt_artifacts,
        prep_steps_variants: prep_steps_variants.into_iter().collect(),
        external_deps: match target_os {
            target_lexicon::OperatingSystem::Windows => {
                VmmTestsExternalDeps::Windows(VmmTestsExternalDepsWindows {
                    hyperv: needs_hyperv,
                    whp: needs_whp,
                    hardware_isolation: needs_hardware_isolation,
                })
            }
            target_lexicon::OperatingSystem::Linux => {
                VmmTestsExternalDeps::Linux(VmmTestsExternalDepsLinux {
                    hugetlb_2mb_overcommit_pages: None, // TODO
                    prepare_vhost_vsock: false,         // TODO
                })
            }
            _ => unreachable!(),
        },
        flowey_hvlite_path,
        needs_virtio_win_drivers,
        needs_release_igvm,
    }
}

impl ResolvedArtifactSelections {
    /// Resolve a single artifact ID and update selections.
    fn resolve_artifact(&mut self, id: &str) -> anyhow::Result<()> {
        use petri_artifacts_common::artifacts::*;
        use petri_artifacts_vmm_test::artifacts::*;

        match id {
            vmfw_dll::LATEST_CVM_X64::GLOBAL_UNIQUE_ID => {
                self.build.vmfirmwareigvm_cvm_x64 = true;
                self.build.openhcl_cvm_x64 = true;
            }
            _ if self.build.resolve_artifact(id) => {}
            _ if self.prebuilt_artifacts.resolve_artifact(id) => {}
            _ if let Some(image) = vmm_test_image_from_id(id) => {
                self.downloads.insert(image);
            }

            // Release IGVM files (downloaded, not built)
            openhcl_igvm::LATEST_RELEASE_STANDARD_X64::GLOBAL_UNIQUE_ID
            | openhcl_igvm::LATEST_RELEASE_LINUX_DIRECT_X64::GLOBAL_UNIQUE_ID
            | openhcl_igvm::LATEST_RELEASE_STANDARD_AARCH64::GLOBAL_UNIQUE_ID => {
                // These are downloaded from GitHub releases, not built
                self.needs_release_igvm = true;
            }

            loadable::PCAT_FIRMWARE_X64::GLOBAL_UNIQUE_ID
            | loadable::SVGA_FIRMWARE_X64::GLOBAL_UNIQUE_ID => {
                // We can't legally distribute these, so hope they are already
                // on the host (which should always be the case for windows).
            }

            test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64_PREPPED::GLOBAL_UNIQUE_ID => {
                self.build
                    .require_openvmm_for(ArtifactTarget::Triple(self.target.clone()))?;
                self.build
                    .require_prep_steps_for(ArtifactTarget::Triple(self.target.clone()))?;
                self.prep_steps_variants.insert("standard".into());
                // prep_steps needs actual VHD files on disk to copy them.
                // Force download even when lazy fetch is enabled.
                self.force_downloads
                    .insert(test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64.into());
                self.force_downloads
                    .insert(test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64.into());
            }
            test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64_NO_VMBUS_PREPPED::GLOBAL_UNIQUE_ID => {
                self.build
                    .require_openvmm_for(ArtifactTarget::Triple(self.target.clone()))?;
                self.build
                    .require_prep_steps_for(ArtifactTarget::Triple(self.target.clone()))?;
                self.needs_virtio_win_drivers = true;
                self.prep_steps_variants.insert("no-vmbus".into());
                self.force_downloads
                    .insert(test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64.into());
            }

            // OpenHCL usermode binaries (built as part of IGVM)
            openhcl_igvm::um_bin::LATEST_LINUX_DIRECT_TEST_X64::GLOBAL_UNIQUE_ID
            | openhcl_igvm::um_dbg::LATEST_LINUX_DIRECT_TEST_X64::GLOBAL_UNIQUE_ID => {
                self.build.openhcl_linux_direct_x64 = true;
            }

            // Common artifacts (always available, no build needed)
            TEST_LOG_DIRECTORY::GLOBAL_UNIQUE_ID => {}

            // Virtio-win drivers (downloaded from openvmm-deps)
            virtio_win::VIRTIO_WINDOWS_DRIVERS::GLOBAL_UNIQUE_ID => {
                self.needs_virtio_win_drivers = true;
            }

            _ => anyhow::bail!("unknown artifact: {id}"),
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use petri_artifacts_vmm_test::artifacts::openhcl_igvm;
    use petri_artifacts_vmm_test::artifacts::vmfw_dll;
    use test_with_tracing::test;

    #[derive(clap::Parser)]
    struct TestCli {
        #[clap(flatten)]
        run: VmmTestsRunCli,
    }

    #[test]
    fn fvp_package_options_require_complete_identity() {
        use clap::Parser;

        for args in [
            vec!["test", "--fvp-deps-version", "0.3.0-local-fvp"],
            vec!["test", "--fvp-package-sha256", "digest"],
            vec!["test", "--fvp-package-archive", "package.tar.gz"],
            vec!["test", "--fvp-payload-archive", "payload.tar.gz"],
        ] {
            assert!(TestCli::try_parse_from(args).is_err());
        }
        assert!(
            TestCli::try_parse_from([
                "test",
                "--fvp-deps-version",
                "0.3.0-local-fvp",
                "--fvp-package-sha256",
                &"0".repeat(64),
                "--fvp-package-archive",
                "openvmm-test-fvp-cca.aarch64.0.3.0-local-fvp.tar.gz",
            ])
            .is_ok()
        );
        assert!(
            TestCli::try_parse_from([
                "test",
                "--fvp-deps-version",
                "0.3.0-local-fvp",
                "--fvp-payload-sha256",
                &"0".repeat(64),
                "--fvp-payload-archive",
                "openvmm-test-fvp-cca-payload.aarch64.0.3.0-local-fvp.tar.gz",
            ])
            .is_ok()
        );
    }

    #[test]
    fn fvp_artifact_selects_firmware_without_qemu() {
        use petri_artifacts_common::artifacts::FVP_CCA_PLATFORM_AARCH64;

        let mut selections =
            ResolvedArtifactSelections::new(CommonTriple::X86_64_LINUX_GNU.as_triple(), false)
                .unwrap();
        selections
            .resolve_artifact(FVP_CCA_PLATFORM_AARCH64::GLOBAL_UNIQUE_ID)
            .unwrap();

        assert!(selections.prebuilt_artifacts.fvp_cca_platform_aarch64);
        assert!(!selections.prebuilt_artifacts.qemu_system_aarch64_linux_x64);
    }

    #[test]
    fn fvp_payload_artifact_selects_the_qualified_payload_directory() {
        use petri_artifacts_common::artifacts::FVP_CCA_PAYLOAD_AARCH64;

        let mut selections =
            ResolvedArtifactSelections::new(CommonTriple::X86_64_LINUX_GNU.as_triple(), false)
                .unwrap();
        selections
            .resolve_artifact(FVP_CCA_PAYLOAD_AARCH64::GLOBAL_UNIQUE_ID)
            .unwrap();
        assert!(selections.prebuilt_artifacts.fvp_cca_payload_aarch64);
        assert!(!selections.prebuilt_artifacts.test_linux_kernel_aarch64);
        assert!(!selections.prebuilt_artifacts.test_linux_kernel_cca_aarch64);
    }

    fn query_mock_suite(suite: &RustSuite) -> anyhow::Result<ArtifactListOutput> {
        use petri_artifacts_common::artifacts::FVP_CCA_PAYLOAD_AARCH64;
        use petri_artifacts_common::artifacts::FVP_CCA_PLATFORM_AARCH64;

        let mut output = ArtifactListOutput {
            required: Vec::new(),
            optional: Vec::new(),
            nested: Vec::new(),
        };
        for name in &suite.testcases {
            match name.as_str() {
                "ordinary_with_fvp_in_name" => output.required.push("ordinary".into()),
                "firmware_consumer" => output
                    .required
                    .push(FVP_CCA_PLATFORM_AARCH64::GLOBAL_UNIQUE_ID.into()),
                "payload_consumer" => output
                    .optional
                    .push(FVP_CCA_PAYLOAD_AARCH64::GLOBAL_UNIQUE_ID.into()),
                "ordinary_nested" => output.nested.push(("archive".into(), vec!["inner".into()])),
                _ => anyhow::bail!("unexpected mock test"),
            }
        }
        Ok(output)
    }

    fn mock_fvp_suite() -> RustSuite {
        RustSuite {
            binary_path: "test-binary".into(),
            testcases: [
                "ordinary_with_fvp_in_name",
                "firmware_consumer",
                "payload_consumer",
                "ordinary_nested",
            ]
            .map(String::from)
            .to_vec(),
        }
    }

    #[test]
    fn unconfigured_fvp_discovery_preserves_ordinary_and_nested_cases() {
        let mut suite = mock_fvp_suite();
        let mut excluded = Vec::new();
        let output = query_configured_suite_artifacts(
            &mut suite,
            false,
            &mut query_mock_suite,
            &mut excluded,
        )
        .unwrap();
        assert_eq!(
            suite.testcases,
            ["ordinary_with_fvp_in_name", "ordinary_nested"]
        );
        assert_eq!(excluded, ["firmware_consumer", "payload_consumer"]);
        assert_eq!(output.required, ["ordinary"]);
        assert!(output.optional.is_empty());
        assert_eq!(output.nested, [("archive".into(), vec!["inner".into()])]);
    }

    #[test]
    fn configured_fvp_discovery_keeps_every_case_and_queries_once() {
        let mut suite = mock_fvp_suite();
        let mut excluded = Vec::new();
        let mut queries = 0;
        let output = query_configured_suite_artifacts(
            &mut suite,
            true,
            &mut |suite| {
                queries += 1;
                query_mock_suite(suite)
            },
            &mut excluded,
        )
        .unwrap();
        assert_eq!(queries, 1);
        assert_eq!(suite.testcases.len(), 4);
        assert!(excluded.is_empty());
        assert!(uses_fvp_packages(&output));
        assert_eq!(output.nested, [("archive".into(), vec!["inner".into()])]);
    }

    #[test]
    fn cvm_dll_selects_cvm_igvm_and_dll() {
        let mut selections =
            ResolvedArtifactSelections::new(CommonTriple::X86_64_WINDOWS_MSVC.as_triple(), false)
                .unwrap();
        selections
            .resolve_artifact(vmfw_dll::LATEST_CVM_X64::GLOBAL_UNIQUE_ID)
            .unwrap();

        assert!(selections.build.vmfirmwareigvm_cvm_x64);
        assert!(selections.build.openhcl_cvm_x64);
    }

    #[test]
    fn cvm_igvm_does_not_select_dll() {
        let mut selections =
            ResolvedArtifactSelections::new(CommonTriple::X86_64_WINDOWS_MSVC.as_triple(), false)
                .unwrap();
        selections
            .resolve_artifact(openhcl_igvm::LATEST_CVM_X64::GLOBAL_UNIQUE_ID)
            .unwrap();

        assert!(selections.build.openhcl_cvm_x64);
        assert!(!selections.build.vmfirmwareigvm_cvm_x64);
    }
}
