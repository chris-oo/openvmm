// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve the QEMU binary proven by the CCA Phase 0 smoke path.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaQemuOutput {
    pub binary: PathBuf,
}

impl Artifact for CcaQemuOutput {}

flowey_request! {
    pub struct Request {
        pub host_arch: CommonArch,
        pub output: WriteVar<CcaQemuOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::resolve_openvmm_qemu::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        ctx.config(crate::resolve_openvmm_qemu::Config {
            version: Some(crate::cca_pins::QEMU_OPENVMM_DEPS_VERSION.into()),
            local_paths: BTreeMap::new(),
        });
        for Request { host_arch, output } in requests {
            let qemu = ctx.reqv(|v| {
                crate::resolve_openvmm_qemu::Request::Get(
                    crate::resolve_openvmm_qemu::QemuFile::SystemAarch64,
                    host_arch,
                    v,
                )
            });

            ctx.emit_rust_step("report CCA QEMU", |ctx| {
                let qemu = qemu.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let binary = rt.read(qemu);
                    anyhow::ensure!(
                        binary.is_file(),
                        "CCA QEMU binary not found at {}",
                        binary.display()
                    );
                    rt.write(output, &CcaQemuOutput { binary });
                    Ok(())
                }
            });
        }
        Ok(())
    }
}
