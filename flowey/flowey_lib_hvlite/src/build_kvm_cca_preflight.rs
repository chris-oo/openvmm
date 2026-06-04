// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the `kvm_cca_preflight` binary.

use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct KvmCcaPreflightBuildParams {
    pub profile: CommonProfile,
    pub target: CommonTriple,
}

#[derive(Serialize, Deserialize)]
pub struct KvmCcaPreflightOutput {
    pub bin: PathBuf,
    pub dbg: PathBuf,
}

impl Artifact for KvmCcaPreflightOutput {}

flowey_request! {
    pub struct Request {
        pub params: KvmCcaPreflightBuildParams,
        pub preflight: WriteVar<KvmCcaPreflightOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        for Request {
            params: KvmCcaPreflightBuildParams { profile, target },
            preflight,
        } in requests
        {
            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "kvm_cca_preflight".into(),
                out_name: "kvm_cca_preflight".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile: profile.into(),
                features: flowey_lib_common::run_cargo_build::CargoFeatureSet::default(),
                target: target.as_triple(),
                no_split_dbg_info: false,
                extra_env: None,
                pre_build_deps: Vec::new(),
                output: v,
            });

            ctx.emit_minor_rust_step("report built kvm_cca_preflight", |ctx| {
                let preflight = preflight.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            KvmCcaPreflightOutput {
                                bin,
                                dbg: dbg.unwrap(),
                            }
                        }
                        _ => unreachable!("kvm_cca_preflight is Linux-only"),
                    };

                    rt.write(preflight, &output);
                }
            });
        }

        Ok(())
    }
}
