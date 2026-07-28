// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the FVP host and Realm guest Linux kernels for KVM CCA tests.

use flowey::node::prelude::*;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaLinuxKernelBuildParams {
    pub openvmm_root: PathBuf,
    pub source: PathBuf,
    pub revision: String,
    pub output_root: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaLinuxKernelOutput {
    pub host_image: PathBuf,
    pub guest_image: PathBuf,
    pub manifest: PathBuf,
}

impl Artifact for CcaLinuxKernelOutput {}

flowey_request! {
    pub struct Request {
        pub params: CcaLinuxKernelBuildParams,
        pub output: WriteVar<CcaLinuxKernelOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let package_names = match ctx.platform() {
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu) => Some(vec![
                "bc",
                "bison",
                "flex",
                "gcc-aarch64-linux-gnu",
                "libssl-dev",
            ]),
            FlowPlatform::Linux(
                FlowPlatformLinuxDistro::Fedora | FlowPlatformLinuxDistro::AzureLinux,
            ) => Some(vec![
                "bc",
                "bison",
                "flex",
                "gcc-aarch64-linux-gnu",
                "openssl-devel",
            ]),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Arch) => Some(vec![
                "aarch64-linux-gnu-gcc",
                "bc",
                "bison",
                "flex",
                "openssl",
            ]),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Nix) => None,
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Unknown) => {
                anyhow::bail!("unsupported Linux distribution for CCA kernel builds")
            }
            _ => anyhow::bail!("CCA kernel builds require a Linux host"),
        };
        let packages = package_names.map(|package_names| {
            ctx.reqv(|v| flowey_lib_common::install_dist_pkg::Request::Install {
                package_names: package_names.into_iter().map(Into::into).collect(),
                done: v,
            })
        });
        for Request { params, output } in requests {
            ctx.emit_rust_step("build CCA Linux kernels", |ctx| {
                if let Some(packages) = &packages {
                    packages.clone().claim(ctx);
                }
                let output_var = output.claim(ctx);
                move |rt| {
                    let CcaLinuxKernelBuildParams {
                        openvmm_root,
                        source,
                        revision,
                        output_root,
                    } = params;
                    let script = openvmm_root.join("build_support/cca/build-kernels.sh");
                    anyhow::ensure!(
                        script.is_file(),
                        "CCA kernel build script not found at {}",
                        script.display()
                    );

                    flowey::shell_cmd!(
                        rt,
                        "{script} --source {source} --revision {revision} --output-root {output_root}"
                    )
                    .run()?;

                    let artifact = CcaLinuxKernelOutput {
                        host_image: output_root.join("host-Image"),
                        guest_image: output_root.join("guest-Image"),
                        manifest: output_root.join("manifest.txt"),
                    };
                    for (name, path) in [
                        ("FVP host kernel", &artifact.host_image),
                        ("Realm guest kernel", &artifact.guest_image),
                        ("CCA kernel manifest", &artifact.manifest),
                    ] {
                        anyhow::ensure!(path.is_file(), "{name} not found at {}", path.display());
                    }
                    rt.write(output_var, &artifact);
                    Ok(())
                }
            });
        }

        Ok(())
    }
}
