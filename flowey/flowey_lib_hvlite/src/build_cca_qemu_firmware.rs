// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the pinned QEMU CCA firmware stack in Docker.

use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaQemuFirmwareBuildParams {
    pub openvmm_root: PathBuf,
    pub output_root: PathBuf,
    pub cache_root: Option<PathBuf>,
    pub offline: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaQemuFirmwareOutput {
    pub flash: PathBuf,
    pub rmm: PathBuf,
    pub edk2: PathBuf,
    pub bl1: PathBuf,
    pub fip: PathBuf,
    pub logs: PathBuf,
    pub manifest: PathBuf,
}

impl Artifact for CcaQemuFirmwareOutput {}

flowey_request! {
    pub struct Request {
        pub params: CcaQemuFirmwareBuildParams,
        pub output: WriteVar<CcaQemuFirmwareOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let docker_package = match ctx.platform() {
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu) => Some("docker.io"),
            FlowPlatform::Linux(
                FlowPlatformLinuxDistro::Fedora | FlowPlatformLinuxDistro::AzureLinux,
            ) => Some("moby-engine"),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Arch) => Some("docker"),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Nix) => None,
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Unknown) => {
                anyhow::bail!("unsupported Linux distribution for CCA firmware builds")
            }
            _ => anyhow::bail!("CCA firmware builds require a Linux host"),
        };
        let docker_package = docker_package.map(|package| {
            ctx.reqv(|v| flowey_lib_common::install_dist_pkg::Request::Install {
                package_names: vec![package.into()],
                done: v,
            })
        });

        for Request { params, output } in requests {
            ctx.emit_rust_step("build QEMU CCA firmware", |ctx| {
                if let Some(docker_package) = &docker_package {
                    docker_package.clone().claim(ctx);
                }
                let output = output.claim(ctx);
                move |rt| {
                    let CcaQemuFirmwareBuildParams {
                        openvmm_root,
                        output_root,
                        cache_root,
                        offline,
                    } = params;
                    let script = openvmm_root.join("build_support/cca/build-qemu-firmware.sh");
                    anyhow::ensure!(
                        script.is_file(),
                        "CCA firmware build script not found at {}",
                        script.display()
                    );

                    let mut command =
                        flowey::shell_cmd!(rt, "{script} --output-root {output_root}");
                    if let Some(cache_root) = cache_root {
                        command = command.arg("--cache-root").arg(cache_root);
                    }
                    if offline {
                        command = command.arg("--offline");
                    }
                    command.run()?;

                    let artifact = CcaQemuFirmwareOutput {
                        flash: output_root.join("flash.bin"),
                        rmm: output_root.join("rmm.img"),
                        edk2: output_root.join("QEMU_EFI.fd"),
                        bl1: output_root.join("bl1.bin"),
                        fip: output_root.join("fip.bin"),
                        logs: output_root.join("logs"),
                        manifest: output_root.join("manifest.txt"),
                    };
                    for (name, path) in [
                        ("QEMU CCA flash", &artifact.flash),
                        ("TF-RMM image", &artifact.rmm),
                        ("EDK2 firmware", &artifact.edk2),
                        ("TF-A BL1", &artifact.bl1),
                        ("TF-A FIP", &artifact.fip),
                        ("firmware manifest", &artifact.manifest),
                    ] {
                        anyhow::ensure!(path.is_file(), "{name} not found at {}", path.display());
                    }
                    anyhow::ensure!(
                        artifact.logs.is_dir(),
                        "firmware logs not found at {}",
                        artifact.logs.display()
                    );

                    let manifest = parse_manifest(&artifact.manifest)?;
                    verify_manifest_pin(
                        &manifest,
                        "tf_rmm_revision",
                        crate::cca_pins::TF_RMM_V2_REVISION,
                    )?;
                    verify_manifest_pin(
                        &manifest,
                        "tf_a_revision",
                        crate::cca_pins::TF_A_V2_15_REVISION,
                    )?;
                    verify_manifest_pin(
                        &manifest,
                        "edk2_revision",
                        crate::cca_pins::EDK2_STABLE_202505_REVISION,
                    )?;

                    rt.write(output, &artifact);
                    Ok(())
                }
            });
        }
        Ok(())
    }
}

fn parse_manifest(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let contents = fs_err::read_to_string(path)?;
    parse_manifest_contents(&contents)
}

fn parse_manifest_contents(contents: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut manifest = BTreeMap::new();
    for line in contents.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid manifest line: {line}"))?;
        anyhow::ensure!(
            manifest.insert(key.into(), value.into()).is_none(),
            "duplicate manifest key: {key}"
        );
    }
    Ok(manifest)
}

fn verify_manifest_pin(
    manifest: &BTreeMap<String, String>,
    key: &'static str,
    expected: &'static str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        manifest.get(key).map(String::as_str) == Some(expected),
        "firmware manifest {key} does not match pinned revision {expected}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_verifies_manifest() {
        let manifest = parse_manifest_contents(&format!(
            "tf_rmm_revision={}\ntf_a_revision={}\nedk2_revision={}\n",
            crate::cca_pins::TF_RMM_V2_REVISION,
            crate::cca_pins::TF_A_V2_15_REVISION,
            crate::cca_pins::EDK2_STABLE_202505_REVISION,
        ))
        .unwrap();
        verify_manifest_pin(
            &manifest,
            "tf_rmm_revision",
            crate::cca_pins::TF_RMM_V2_REVISION,
        )
        .unwrap();
    }

    #[test]
    fn rejects_duplicate_manifest_keys() {
        assert!(parse_manifest_contents("key=one\nkey=two\n").is_err());
    }
}
