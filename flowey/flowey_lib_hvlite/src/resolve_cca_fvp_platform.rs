// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve a user-provisioned local FVP CCA platform.

use flowey::node::prelude::*;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaFvpPlatformOutput {
    pub platform_root: PathBuf,
    pub source_rootfs: PathBuf,
    pub shrinkwrap: PathBuf,
    pub overlay: PathBuf,
}

impl Artifact for CcaFvpPlatformOutput {}

flowey_request! {
    pub struct Request {
        pub platform_root: PathBuf,
        pub output: WriteVar<CcaFvpPlatformOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            platform_root,
            output,
        } = request;
        ctx.emit_rust_step("resolve local FVP CCA platform", |ctx| {
            let output = output.claim(ctx);
            move |rt| {
                let platform_root = fs_err::canonicalize(&platform_root).with_context(|| {
                    format!(
                        "failed to resolve FVP platform root {}",
                        platform_root.display()
                    )
                })?;
                let artifact = CcaFvpPlatformOutput {
                    source_rootfs: platform_root.join("kvm-cca/rootfs.ext2"),
                    shrinkwrap: platform_root.join("shrinkwrap/venv/bin/shrinkwrap"),
                    overlay: platform_root.join("shrinkwrap/config/kvm_cca_planes.yaml"),
                    platform_root,
                };
                for (name, path) in [
                    ("FVP CCA source rootfs", &artifact.source_rootfs),
                    ("Shrinkwrap executable", &artifact.shrinkwrap),
                    ("FVP CCA overlay", &artifact.overlay),
                ] {
                    anyhow::ensure!(path.is_file(), "{name} not found at {}", path.display());
                }
                rt.write(output, &artifact);
                Ok(())
            }
        });
        Ok(())
    }
}
