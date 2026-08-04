// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use std::path::PathBuf;

/// Build and stage the typed QEMU CCA platform artifacts.
#[derive(clap::Args)]
pub struct QemuCcaArtifactsCli {
    /// Root directory for the staged QEMU CCA platform.
    #[clap(long, default_value = "target/cca-qemu-packaged")]
    pub output_root: PathBuf,

    /// Root directory for intermediate native KVM CCA artifacts.
    #[clap(long, default_value = "target/cca-test")]
    pub test_root: PathBuf,

    /// Pinned linux-cca source tree used to build both host and guest kernels.
    #[clap(long)]
    pub cca_kernel_src: Option<PathBuf>,

    /// Exact linux-cca revision used to build both host and guest kernels.
    #[clap(long)]
    pub cca_kernel_rev: Option<String>,

    /// Realm guest initrd override. If omitted, use the aarch64 openvmm-deps initrd.
    #[clap(long)]
    pub guest_initrd: Option<PathBuf>,

    /// Extra OpenVMM command-line arguments for local debugging.
    #[clap(long)]
    pub openvmm_extra_args: Option<String>,

    /// Guest memory size passed to OpenVMM by the nested smoke launcher.
    #[clap(long, default_value = "256M")]
    pub openvmm_memory: String,

    /// Persistent cache for QEMU CCA firmware source snapshots.
    #[clap(long)]
    pub firmware_cache_root: Option<PathBuf>,

    /// Build QEMU CCA firmware without network access.
    #[clap(long)]
    pub firmware_offline: bool,
}

impl IntoPipeline for QemuCcaArtifactsCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let output_root = absolute(self.output_root);
        let test_root = absolute(self.test_root);
        let kernel_source = self
            .cca_kernel_src
            .map(absolute)
            .unwrap_or(default_cca_kernel_source()?);
        let kernel_revision = self
            .cca_kernel_rev
            .unwrap_or_else(|| flowey_lib_hvlite::cca_pins::LINUX_CCA_V15_REVISION.into());

        let mut pipeline = Pipeline::new();
        let job = pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "qemu-cca-artifacts: stage typed QEMU CCA platform",
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
                    auto_install: true,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(false),
                locked: false,
                deny_warnings: false,
                no_incremental: false,
            })
            .dep_on(
                move |ctx| flowey_lib_hvlite::_jobs::local_stage_qemu_cca::Params {
                    openvmm_root: crate::repo_root(),
                    test_root: test_root.clone(),
                    output_root: output_root.clone(),
                    kernels: flowey_lib_hvlite::_jobs::local_stage_kvm_cca::CcaKernelSource::Build(
                        flowey_lib_hvlite::build_cca_linux_kernels::CcaLinuxKernelBuildParams {
                            openvmm_root: crate::repo_root(),
                            source: kernel_source,
                            revision: kernel_revision,
                            output_root: test_root.join("cca-kernels-v15"),
                        },
                    ),
                    guest_initrd: self.guest_initrd.map(absolute),
                    openvmm_memory: self.openvmm_memory,
                    openvmm_extra_args: self.openvmm_extra_args,
                    firmware_cache_root: self.firmware_cache_root.map(absolute),
                    firmware_offline: self.firmware_offline,
                    done: ctx.new_done_handle(),
                },
            )
            .finish();
        let _ = job;
        Ok(pipeline)
    }
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        crate::repo_root().join(path)
    }
}

fn default_cca_kernel_source() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    Ok(home.join("ai/jolteon/linux-cca"))
}
