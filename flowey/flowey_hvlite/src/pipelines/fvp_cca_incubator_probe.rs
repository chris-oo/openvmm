// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::FloweyPathExt;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use std::path::PathBuf;

/// Build and run the local FVP CCA incubator platform probe.
#[derive(clap::Args)]
pub struct FvpCcaIncubatorProbeCli {
    /// Root populated by `cargo xflowey kvm-cca-tests --install-emu`.
    #[clap(long, env = "OPENVMM_FVP_CCA_PLATFORM_ROOT")]
    pub fvp_platform_root: PathBuf,

    /// Output directory for probe artifacts and logs.
    #[clap(long, default_value = "target/cca-fvp-incubator-probe")]
    pub output_root: PathBuf,

    /// Pinned linux-cca source tree.
    #[clap(long, env = "OPENVMM_CCA_KERNEL_SRC")]
    pub cca_kernel_src: Option<PathBuf>,
}

impl IntoPipeline for FvpCcaIncubatorProbeCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let openvmm_root = crate::repo_root();
        let platform_root = absolute(self.fvp_platform_root)?;
        let output_root = absolute(self.output_root)?;
        let kernel_source = self
            .cca_kernel_src
            .map(absolute)
            .transpose()?
            .unwrap_or_else(|| openvmm_root.parent().unwrap().join("linux-cca"));
        anyhow::ensure!(
            platform_root.is_dir(),
            "FVP platform root not found at {}",
            platform_root.display()
        );
        anyhow::ensure!(
            kernel_source.is_dir(),
            "linux-cca source not found at {}",
            kernel_source.display()
        );

        let mut pipeline = Pipeline::new();
        let job = pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "fvp-cca-incubator-probe: run platform probe",
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
            .dep_on(move |ctx| {
                flowey_lib_hvlite::_jobs::local_run_fvp_cca_incubator_probe::Params {
                    openvmm_root,
                    platform_root,
                    output_root,
                    kernel_source,
                    done: ctx.new_done_handle(),
                }
            })
            .finish();
        let _ = job;
        Ok(pipeline)
    }
}

fn absolute(path: PathBuf) -> anyhow::Result<PathBuf> {
    Ok(if path.is_absolute() {
        path
    } else {
        crate::repo_root().join(path)
    }
    .absolute()?)
}
