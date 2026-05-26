// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use std::path::PathBuf;

/// Native OpenVMM KVM CCA debug and test flows.
#[derive(clap::Args)]
pub struct KvmCcaTestsCli {
    /// Root directory for holding all native KVM CCA test artifacts.
    #[clap(long, default_value = "target/cca-test")]
    pub test_root: PathBuf,

    /// Install CCA emulation environment, including downloading emulator and building needed firmware.
    #[clap(long)]
    pub install_emu: bool,

    /// Update CCA emulation environment components, then exit.
    #[clap(long)]
    pub update_emu: bool,

    /// Rebuild the Plane0 Linux image from the existing source tree.
    #[clap(long)]
    pub rebuild_plane0_linux: bool,

    /// Rebuild the shrinkwrap-generated rootfs image.
    #[clap(long)]
    pub rebuild_rootfs: bool,

    /// Host kernel source tree for future Plane0 rebuild support.
    #[clap(long)]
    pub host_kernel_src: Option<PathBuf>,

    /// Host kernel revision for future Plane0 rebuild support.
    #[clap(long)]
    pub host_kernel_rev: Option<String>,

    /// Plane0 host kernel Image to boot under FVP.
    #[clap(long)]
    pub host_kernel: Option<PathBuf>,

    /// Realm guest kernel Image for OpenVMM direct Linux boot.
    #[clap(long)]
    pub guest_kernel: Option<PathBuf>,

    /// Realm guest initrd override. If omitted, use the aarch64 openvmm-deps initrd.
    #[clap(long)]
    pub guest_initrd: Option<PathBuf>,

    /// Extra OpenVMM command-line arguments for local debugging.
    #[clap(long)]
    pub openvmm_extra_args: Option<String>,

    /// Boot FVP/Plane0 and run only the KVM CCA preflight probe.
    #[clap(long)]
    pub preflight: bool,

    /// Stage native OpenVMM KVM CCA artifacts into an isolated rootfs, then exit.
    #[clap(long)]
    pub stage_only: bool,

    /// Boot FVP/Plane0 with artifacts staged for manual debugging.
    #[clap(long)]
    pub interactive_host: bool,

    /// Boot FVP/Plane0 and run OpenVMM via the staged init hook.
    #[clap(long)]
    pub run_openvmm: bool,
}

impl IntoPipeline for KvmCcaTestsCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let Self {
            test_root,
            install_emu,
            update_emu,
            rebuild_plane0_linux,
            rebuild_rootfs,
            host_kernel_src,
            host_kernel_rev,
            host_kernel,
            guest_kernel,
            guest_initrd,
            openvmm_extra_args,
            preflight,
            stage_only,
            interactive_host,
            run_openvmm,
        } = self;

        let test_root = if test_root.is_absolute() {
            test_root
        } else {
            crate::repo_root().join(test_root)
        };

        let run_mode_count = [preflight, stage_only, interactive_host, run_openvmm]
            .into_iter()
            .filter(|mode| *mode)
            .count();
        let maintenance_mode_count = [install_emu, update_emu]
            .into_iter()
            .filter(|mode| *mode)
            .count();

        if maintenance_mode_count > 1 {
            anyhow::bail!("--install-emu and --update-emu are mutually exclusive");
        }
        if maintenance_mode_count != 0 && run_mode_count != 0 {
            anyhow::bail!("maintenance modes cannot be combined with run modes");
        }
        if maintenance_mode_count == 0 && run_mode_count != 1 {
            anyhow::bail!(
                "select exactly one run mode: --preflight, --stage-only, --interactive-host, or --run-openvmm"
            );
        }
        if host_kernel_src.is_some() || host_kernel_rev.is_some() {
            anyhow::bail!("--host-kernel-src/--host-kernel-rev support is not implemented yet");
        }

        let mut pipeline = Pipeline::new();
        if install_emu {
            let check_job = pipeline
                .new_job(
                    FlowPlatform::host(backend_hint),
                    FlowArch::host(backend_hint),
                    "kvm-cca-tests: check existence of emulation environment needed tools",
                )
                .config(flowey_lib_common::install_dist_pkg::Config {
                    interactive: Some(true),
                    skip_update: Some(false),
                })
                .dep_on(
                    |ctx| flowey_lib_hvlite::_jobs::local_check_cca_emu_prereq::Params {
                        done: ctx.new_done_handle(),
                    },
                )
                .finish();

            let install_job = pipeline
                .new_job(
                    FlowPlatform::host(backend_hint),
                    FlowArch::host(backend_hint),
                    "kvm-cca-tests: install emulation environment",
                )
                .config(flowey_lib_common::git_checkout::Config {
                    require_local_clones: Some(false),
                })
                .config(flowey_lib_common::install_git::Config {
                    auto_install: Some(true),
                })
                .config(flowey_lib_common::install_dist_pkg::Config {
                    interactive: Some(true),
                    skip_update: Some(false),
                })
                .dep_on(
                    |ctx| flowey_lib_hvlite::_jobs::local_install_cca_emu::Params {
                        test_root: test_root.clone(),
                        openvmm_root: crate::repo_root(),
                        skip_plane0_linux: true,
                        use_kvm_cca_overlay: true,
                        done: ctx.new_done_handle(),
                    },
                )
                .finish();

            pipeline.non_artifact_dep(&install_job, &check_job);
            return Ok(pipeline);
        }

        if stage_only || preflight {
            let host_kernel = host_kernel.unwrap_or(default_cca_kernel_path()?);
            let guest_kernel = guest_kernel.unwrap_or_else(|| host_kernel.clone());
            let test_job = pipeline
                .new_job(
                    FlowPlatform::host(backend_hint),
                    FlowArch::host(backend_hint),
                    if preflight {
                        "kvm-cca-tests: run KVM CCA preflight in FVP"
                    } else {
                        "kvm-cca-tests: stage native OpenVMM KVM CCA artifacts"
                    },
                )
                .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
                .dep_on(
                    |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                        hvlite_repo_source:
                            flowey_lib_common::git_checkout::RepoSource::ExistingClone(
                                ReadVar::from_static(crate::repo_root()),
                            ),
                    },
                )
                .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                    local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                        interactive: true,
                        auto_install: true,
                        ignore_rust_version: true,
                    }),
                    verbose: ReadVar::from_static(false),
                    locked: false,
                    deny_warnings: false,
                    no_incremental: false,
                })
                .dep_on(
                    move |ctx| flowey_lib_hvlite::_jobs::local_stage_kvm_cca::Params {
                        test_root: test_root.clone(),
                        mode: if preflight {
                            flowey_lib_hvlite::_jobs::local_stage_kvm_cca::StageMode::Preflight
                        } else {
                            flowey_lib_hvlite::_jobs::local_stage_kvm_cca::StageMode::StageOnly
                        },
                        host_kernel,
                        guest_kernel: (!preflight).then_some(guest_kernel),
                        guest_initrd,
                        done: ctx.new_done_handle(),
                    },
                )
                .finish();
            let _ = test_job;
            return Ok(pipeline);
        }

        if interactive_host || run_openvmm {
            let _ = (host_kernel, guest_kernel, guest_initrd, openvmm_extra_args);
            anyhow::bail!("this kvm-cca-tests run mode is not implemented yet");
        }

        let update_job = pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "kvm-cca-tests: update emulation environment",
            )
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::local_update_cca_emu::Params {
                    test_root,
                    sub_cmds: flowey_lib_hvlite::_jobs::local_update_cca_emu::SubCmds {
                        rebuild_plane0_linux,
                        rebuild_rootfs,
                        tfa_rev: None,
                        tfrmm_rev: None,
                        plane0_linux_rev: None,
                    },
                    done: ctx.new_done_handle(),
                },
            )
            .finish();
        let _ = update_job;

        Ok(pipeline)
    }
}

fn default_cca_kernel_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    Ok(home.join("ai/eevee/linux/out/cca-fvp/kernel/arch/arm64/boot/Image"))
}
