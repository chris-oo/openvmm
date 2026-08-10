// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the QEMU CCA L1 host rootfs with pipette startup.

use flowey::node::prelude::*;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaQemuHostRootfsBuildParams {
    pub openvmm_root: PathBuf,
    pub source_rootfs: ReadVar<PathBuf>,
    pub output_root: PathBuf,
    pub init_script: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaQemuHostRootfsOutput {
    pub image: PathBuf,
    pub init_script: PathBuf,
    pub manifest: PathBuf,
}

impl Artifact for CcaQemuHostRootfsOutput {}

flowey_request! {
    pub struct Request {
        pub params: CcaQemuHostRootfsBuildParams,
        pub output: WriteVar<CcaQemuHostRootfsOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let e2fsprogs = match ctx.platform() {
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Nix) => None,
            FlowPlatform::Linux(_) => {
                Some(
                    ctx.reqv(|v| flowey_lib_common::install_dist_pkg::Request::Install {
                        package_names: vec!["e2fsprogs".into()],
                        done: v,
                    }),
                )
            }
            _ => anyhow::bail!("CCA host rootfs builds require a Linux host"),
        };

        for Request { params, output } in requests {
            ctx.emit_rust_step("build QEMU CCA host rootfs", |ctx| {
                if let Some(e2fsprogs) = &e2fsprogs {
                    e2fsprogs.clone().claim(ctx);
                }
                let source_rootfs = params.source_rootfs.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let source_rootfs = rt.read(source_rootfs);
                    let script = params
                        .openvmm_root
                        .join("build_support/cca/build-qemu-host-rootfs.sh");
                    anyhow::ensure!(
                        script.is_file(),
                        "CCA host rootfs build script not found at {}",
                        script.display()
                    );
                    let output_root = &params.output_root;
                    let mut command = flowey::shell_cmd!(
                        rt,
                        "{script} --source-rootfs {source_rootfs} --output-root {output_root}"
                    );
                    if let Some(init_script) = &params.init_script {
                        command = command.arg("--init-script").arg(init_script);
                    }
                    command.run()?;

                    let artifact = CcaQemuHostRootfsOutput {
                        image: params.output_root.join("host-rootfs.ext4"),
                        init_script: params.output_root.join("qemu-host-init.sh"),
                        manifest: params.output_root.join("manifest.txt"),
                    };
                    for (name, path) in [
                        ("QEMU CCA host rootfs", &artifact.image),
                        ("QEMU CCA host init script", &artifact.init_script),
                        ("QEMU CCA host rootfs manifest", &artifact.manifest),
                    ] {
                        anyhow::ensure!(path.is_file(), "{name} not found at {}", path.display());
                    }
                    rt.write(output, &artifact);
                    Ok(())
                }
            });
        }
        Ok(())
    }
}
