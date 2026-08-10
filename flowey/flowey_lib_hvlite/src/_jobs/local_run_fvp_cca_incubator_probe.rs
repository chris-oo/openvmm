// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and run the local FVP CCA incubator platform probe.

use crate::common::CommonArch;
use crate::common::CommonPlatform;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use std::fs;
use std::path::Path;

flowey_request! {
    pub struct Params {
        pub openvmm_root: PathBuf,
        pub platform_root: PathBuf,
        pub output_root: PathBuf,
        pub kernel_source: PathBuf,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_cca_linux_kernels::Node>();
        ctx.import::<crate::build_cca_qemu_host_rootfs::Node>();
        ctx.import::<crate::build_kvm_cca_preflight::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            openvmm_root,
            platform_root,
            output_root,
            kernel_source,
            done,
        } = request;
        anyhow::ensure!(
            matches!(ctx.platform(), FlowPlatform::Linux(_)),
            "FVP CCA incubator probe requires Linux"
        );
        let host_arch = match ctx.arch() {
            FlowArch::X86_64 => CommonArch::X86_64,
            FlowArch::Aarch64 => CommonArch::Aarch64,
            other => anyhow::bail!("unsupported FVP CCA probe host architecture: {other:?}"),
        };

        let aarch64_musl = CommonTriple::Common {
            arch: CommonArch::Aarch64,
            platform: CommonPlatform::LinuxMusl,
        };
        let kernels = ctx.reqv(|v| crate::build_cca_linux_kernels::Request {
            params: crate::build_cca_linux_kernels::CcaLinuxKernelBuildParams {
                openvmm_root: openvmm_root.clone(),
                source: kernel_source,
                revision: crate::cca_pins::LINUX_CCA_V15_REVISION.into(),
                output_root: output_root.join("kernels"),
            },
            output: v,
        });
        let pipette = ctx.reqv(|v| crate::build_pipette::Request {
            target: aarch64_musl.clone(),
            profile: CommonProfile::Debug,
            pipette: v,
        });
        let preflight = ctx.reqv(|v| crate::build_kvm_cca_preflight::Request {
            params: crate::build_kvm_cca_preflight::KvmCcaPreflightBuildParams {
                profile: CommonProfile::Debug,
                target: aarch64_musl,
            },
            preflight: v,
        });
        let probe_client = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "pipette_tcp_probe".into(),
            out_name: "pipette_tcp_probe".into(),
            crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
            profile: CommonProfile::Debug.into(),
            features: Default::default(),
            target: CommonTriple::Common {
                arch: host_arch,
                platform: CommonPlatform::LinuxGnu,
            }
            .as_triple(),
            no_split_dbg_info: true,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output: v,
        });
        let rootfs = ctx.reqv(|v| crate::build_cca_qemu_host_rootfs::Request {
            params: crate::build_cca_qemu_host_rootfs::CcaQemuHostRootfsBuildParams {
                openvmm_root: openvmm_root.clone(),
                source_rootfs: ReadVar::from_static(platform_root.join("kvm-cca/rootfs.ext2")),
                output_root: output_root.join("host-rootfs"),
                init_script: Some(openvmm_root.join("build_support/cca/fvp-probe-host-init.sh")),
            },
            output: v,
        });

        ctx.emit_rust_step("run FVP CCA incubator probe", |ctx| {
            done.claim(ctx);
            let kernels = kernels.claim(ctx);
            let pipette = pipette.claim(ctx);
            let preflight = preflight.claim(ctx);
            let probe_client = probe_client.claim(ctx);
            let rootfs = rootfs.claim(ctx);
            move |rt| {
                let kernels = rt.read(kernels);
                let pipette = match rt.read(pipette) {
                    crate::build_pipette::PipetteOutput::LinuxBin { bin, .. } => bin,
                    crate::build_pipette::PipetteOutput::WindowsBin { .. } => {
                        anyhow::bail!("expected Linux pipette")
                    }
                };
                let preflight = rt.read(preflight).bin;
                let probe_client = match rt.read(probe_client) {
                    crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, .. } => bin,
                    _ => anyhow::bail!("expected Linux pipette probe client"),
                };
                let rootfs = rt.read(rootfs);

                let share = output_root.join("share");
                fs::create_dir_all(&share)?;
                copy_executable(&pipette, &share.join("pipette"))?;
                copy_executable(&preflight, &share.join("kvm_cca_preflight"))?;

                let script =
                    openvmm_root.join("build_support/cca/run-fvp-incubator-probe.py");
                let host_kernel = kernels.host_image;
                let host_rootfs = rootfs.image;
                flowey::shell_cmd!(
                    rt,
                    "{script} --platform-root {platform_root} --output-root {output_root} --host-kernel {host_kernel} --host-rootfs {host_rootfs} --share-dir {share} --pipette-probe {probe_client}"
                )
                .run()?;
                Ok(())
            }
        });

        Ok(())
    }
}

fn copy_executable(source: &Path, destination: &Path) -> anyhow::Result<()> {
    fs::copy(source, destination)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(destination)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(destination, permissions)?;
    }
    Ok(())
}
