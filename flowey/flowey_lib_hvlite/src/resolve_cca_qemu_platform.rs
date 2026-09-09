// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Add QEMU TF-A and TF-RMM to the shared CCA payload.

use crate::resolve_cca_payload::CcaPayloadOutput;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaQemuPlatformOutput {
    pub payload: CcaPayloadOutput,
    pub firmware: PathBuf,
    pub firmware_manifest: PathBuf,
    pub rmm_image: PathBuf,
    pub rmm_manifest: PathBuf,
}

impl Artifact for CcaQemuPlatformOutput {}

flowey_config! {
    pub struct Config {
        /// Firmware release and archive hashes default to the checked-in pins.
        /// Configure resolve_cca_payload separately to override the shared payload.
        pub version: Option<String>,
        pub rmm_archive_sha256: Option<String>,
        pub tfa_archive_sha256: Option<String>,
        /// Supply both local release-shaped firmware archives, or neither.
        pub local_rmm_archive: Option<ConfigVar<PathBuf>>,
        pub local_tfa_archive: Option<ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        Get(WriteVar<CcaQemuPlatformOutput>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::resolve_cca_payload::Node>();
        ctx.import::<flowey_lib_common::download_gh_release::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let outputs: Vec<_> = requests
            .into_iter()
            .map(|Request::Get(output)| output)
            .collect();
        if outputs.is_empty() {
            return Ok(());
        }

        let Config {
            version,
            rmm_archive_sha256,
            tfa_archive_sha256,
            local_rmm_archive,
            local_tfa_archive,
        } = config;
        let version = version.unwrap_or_else(|| crate::cca_pins::OPENVMM_DEPS_RELEASE.into());
        let rmm_hash =
            rmm_archive_sha256.unwrap_or_else(|| crate::cca_pins::RMM_ARCHIVE_SHA256.into());
        let tfa_hash =
            tfa_archive_sha256.unwrap_or_else(|| crate::cca_pins::TFA_ARCHIVE_SHA256.into());
        let rmm_name = format!("openvmm-test-rmm-cca.aarch64.{version}.tar.gz");
        let tfa_name = format!("openvmm-test-tfa-cca.aarch64.{version}.tar.gz");
        crate::cca_artifacts::validate_local_archives(
            &[local_rmm_archive.is_some(), local_tfa_archive.is_some()],
            "local QEMU CCA configuration requires TF-RMM and TF-A archives",
        )?;
        let download = |file_name: String, ctx: &mut NodeCtx<'_>| {
            ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                repo_owner: "microsoft".into(),
                repo_name: "openvmm-deps".into(),
                needs_auth: false,
                tag: version.clone(),
                file_name,
                path: v,
            })
        };
        let rmm_archive = local_rmm_archive
            .map(|archive| archive.0)
            .unwrap_or_else(|| download(rmm_name.clone(), ctx));
        let tfa_archive = local_tfa_archive
            .map(|archive| archive.0)
            .unwrap_or_else(|| download(tfa_name.clone(), ctx));
        let payload = ctx.reqv(crate::resolve_cca_payload::Request::Get);
        let persistent_dir = ctx.persistent_dir();
        ctx.emit_rust_step("resolve QEMU CCA firmware archives", |ctx| {
            let rmm_archive = rmm_archive.claim(ctx);
            let tfa_archive = tfa_archive.claim(ctx);
            let payload = payload.claim(ctx);
            let persistent_dir = persistent_dir.claim(ctx);
            let outputs = outputs.claim(ctx);
            move |rt| {
                let rmm_archive = rt.read(rmm_archive).absolute()?;
                let tfa_archive = rt.read(tfa_archive).absolute()?;
                let payload = rt.read(payload);
                let persistent_dir = persistent_dir.map(|dir| rt.read(dir));
                let rmm_dir = crate::cca_artifacts::resolve_archive(
                    rt,
                    persistent_dir.as_deref(),
                    &rmm_archive,
                    &rmm_name,
                    &rmm_hash,
                    "CCA TF-RMM archive",
                )?;
                let tfa_dir = crate::cca_artifacts::resolve_archive(
                    rt,
                    persistent_dir.as_deref(),
                    &tfa_archive,
                    &tfa_name,
                    &tfa_hash,
                    "CCA TF-A archive",
                )?;
                let output = CcaQemuPlatformOutput {
                    payload,
                    firmware: tfa_dir.join("flash.bin"),
                    firmware_manifest: tfa_dir.join("manifest.txt"),
                    rmm_image: rmm_dir.join("rmm.img"),
                    rmm_manifest: rmm_dir.join("manifest.txt"),
                };
                // The payload node has already validated the kernel and initrd.
                output.validate_firmware()?;
                rt.write_all(outputs, &output);
                Ok(())
            }
        });
        Ok(())
    }
}

impl CcaQemuPlatformOutput {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.payload.validate()?;
        self.validate_firmware()
    }

    fn validate_firmware(&self) -> anyhow::Result<()> {
        let rmm = crate::cca_artifacts::parse_manifest(&self.rmm_manifest)?;
        for (key, expected) in [
            ("architecture", "aarch64"),
            ("source_revision", crate::cca_pins::TF_RMM_REVISION),
            ("config", "qemu_virt_defcfg"),
            ("rmm_img_sha256", crate::cca_pins::TF_RMM_IMAGE_SHA256),
        ] {
            crate::cca_artifacts::require_manifest_value(&rmm, key, expected)?;
        }
        crate::cca_artifacts::verify_sha256(
            &self.rmm_image,
            crate::cca_pins::TF_RMM_IMAGE_SHA256,
            "CCA TF-RMM image",
        )?;

        let firmware = crate::cca_artifacts::parse_manifest(&self.firmware_manifest)?;
        for (key, expected) in [
            ("architecture", "aarch64"),
            ("source_revision", crate::cca_pins::TF_A_REVISION),
            ("rmm_source_revision", crate::cca_pins::TF_RMM_REVISION),
            ("rmm_img_sha256", crate::cca_pins::TF_RMM_IMAGE_SHA256),
            ("platform", "qemu"),
            ("linux_as_bl33", "true"),
            ("flash_sha256", crate::cca_pins::TF_A_FLASH_SHA256),
            ("flash_size", "67108864"),
            ("flash_fip_offset", "0x40000"),
            ("preloaded_bl33_base", "0x50080000"),
            ("dtb_base", "0x40000000"),
        ] {
            crate::cca_artifacts::require_manifest_value(&firmware, key, expected)?;
        }
        crate::cca_artifacts::verify_sha256(
            &self.firmware,
            crate::cca_pins::TF_A_FLASH_SHA256,
            "CCA TF-A flash",
        )?;
        validate_initrd_size(std::fs::metadata(&self.payload.initrd)?.len())
    }
}

fn validate_initrd_size(size: u64) -> anyhow::Result<()> {
    anyhow::ensure!(
        size <= crate::cca_pins::QEMU_INITRD_MAX_SIZE,
        "CCA host initrd is too large for its reserved physical range"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initrd_fits_qemu_physical_range() {
        validate_initrd_size(crate::cca_pins::QEMU_INITRD_MAX_SIZE).unwrap();
        assert!(validate_initrd_size(crate::cca_pins::QEMU_INITRD_MAX_SIZE + 1).is_err());
    }
}
