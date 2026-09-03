// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve the QEMU CCA platform from one openvmm-deps release.

use flowey::node::prelude::*;
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaPlatformOutput {
    pub host_kernel: PathBuf,
    pub realm_kernel: PathBuf,
    pub kernel_config: PathBuf,
    pub kernel_manifest: PathBuf,
    pub firmware: PathBuf,
    pub firmware_manifest: PathBuf,
    pub rmm_image: PathBuf,
    pub rmm_manifest: PathBuf,
    pub host_initrd: PathBuf,
}

impl Artifact for CcaPlatformOutput {}

flowey_config! {
    pub struct Config {
        /// openvmm-deps release containing all CCA platform archives.
        pub version: Option<String>,
        pub kernel_archive_sha256: Option<String>,
        pub rmm_archive_sha256: Option<String>,
        pub tfa_archive_sha256: Option<String>,
        pub initrd_archive_sha256: Option<String>,
        /// Local release-shaped archives for pre-publication validation.
        pub local_kernel_archive: Option<ConfigVar<PathBuf>>,
        pub local_rmm_archive: Option<ConfigVar<PathBuf>>,
        pub local_tfa_archive: Option<ConfigVar<PathBuf>>,
        pub local_initrd_archive: Option<ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        Get(WriteVar<CcaPlatformOutput>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
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
            kernel_archive_sha256,
            rmm_archive_sha256,
            tfa_archive_sha256,
            initrd_archive_sha256,
            local_kernel_archive,
            local_rmm_archive,
            local_tfa_archive,
            local_initrd_archive,
        } = config;
        let version = version.context("CCA openvmm-deps release is not configured")?;
        let hashes = ArchiveHashes {
            kernel: kernel_archive_sha256
                .context("CCA kernel archive SHA-256 is not configured")?,
            rmm: rmm_archive_sha256.context("CCA TF-RMM archive SHA-256 is not configured")?,
            tfa: tfa_archive_sha256.context("CCA TF-A archive SHA-256 is not configured")?,
            initrd: initrd_archive_sha256
                .context("CCA initrd archive SHA-256 is not configured")?,
        };
        let local_count = [
            local_kernel_archive.is_some(),
            local_rmm_archive.is_some(),
            local_tfa_archive.is_some(),
            local_initrd_archive.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        anyhow::ensure!(
            local_count == 0 || local_count == 4,
            "local CCA configuration requires kernel, TF-RMM, TF-A, and initrd archives"
        );

        let names = ArchiveNames::new(&version);
        let (kernel_archive, rmm_archive, tfa_archive, initrd_archive) = if local_count == 0 {
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
            (
                download(names.kernel.clone(), ctx),
                download(names.rmm.clone(), ctx),
                download(names.tfa.clone(), ctx),
                download(names.initrd.clone(), ctx),
            )
        } else {
            (
                local_kernel_archive.unwrap().0,
                local_rmm_archive.unwrap().0,
                local_tfa_archive.unwrap().0,
                local_initrd_archive.unwrap().0,
            )
        };

        let persistent_dir = ctx.persistent_dir();
        ctx.emit_rust_step("resolve CCA platform archives", |ctx| {
            let kernel_archive = kernel_archive.claim(ctx);
            let rmm_archive = rmm_archive.claim(ctx);
            let tfa_archive = tfa_archive.claim(ctx);
            let initrd_archive = initrd_archive.claim(ctx);
            let persistent_dir = persistent_dir.claim(ctx);
            let outputs = outputs.claim(ctx);
            move |rt| {
                let kernel_archive = rt.read(kernel_archive).absolute()?;
                let rmm_archive = rt.read(rmm_archive).absolute()?;
                let tfa_archive = rt.read(tfa_archive).absolute()?;
                let initrd_archive = rt.read(initrd_archive).absolute()?;
                for (archive, name, expected, label) in [
                    (
                        &kernel_archive,
                        &names.kernel,
                        &hashes.kernel,
                        "CCA kernel archive",
                    ),
                    (&rmm_archive, &names.rmm, &hashes.rmm, "CCA TF-RMM archive"),
                    (&tfa_archive, &names.tfa, &hashes.tfa, "CCA TF-A archive"),
                    (
                        &initrd_archive,
                        &names.initrd,
                        &hashes.initrd,
                        "CCA initrd archive",
                    ),
                ] {
                    anyhow::ensure!(
                        archive.file_name() == Some(name.as_ref()),
                        "{label} name does not match release {version}: {}",
                        archive.display()
                    );
                    crate::cca_artifacts::verify_sha256(archive, expected, label)?;
                }

                let persistent_dir = persistent_dir.map(|dir| rt.read(dir));
                let kernel_dir = extract(
                    rt,
                    persistent_dir.as_deref(),
                    &kernel_archive,
                    &hashes.kernel,
                )?;
                let rmm_dir = extract(rt, persistent_dir.as_deref(), &rmm_archive, &hashes.rmm)?;
                let tfa_dir = extract(rt, persistent_dir.as_deref(), &tfa_archive, &hashes.tfa)?;
                let initrd_dir = extract(
                    rt,
                    persistent_dir.as_deref(),
                    &initrd_archive,
                    &hashes.initrd,
                )?;
                let output = CcaPlatformOutput {
                    host_kernel: kernel_dir.join("Image"),
                    realm_kernel: kernel_dir.join("Image"),
                    kernel_config: kernel_dir.join("config"),
                    kernel_manifest: kernel_dir.join("manifest.txt"),
                    firmware: tfa_dir.join("flash.bin"),
                    firmware_manifest: tfa_dir.join("manifest.txt"),
                    rmm_image: rmm_dir.join("rmm.img"),
                    rmm_manifest: rmm_dir.join("manifest.txt"),
                    host_initrd: initrd_dir.join("initrd"),
                };
                output.validate()?;
                rt.write_all(outputs, &output);
                Ok(())
            }
        });

        Ok(())
    }
}

fn extract(
    rt: &mut RustRuntimeServices<'_>,
    persistent_dir: Option<&Path>,
    archive: &Path,
    archive_sha256: &str,
) -> anyhow::Result<PathBuf> {
    flowey_lib_common::_util::extract::extract_tar_gz_if_new(
        rt,
        persistent_dir,
        archive,
        archive_sha256,
    )
}

impl CcaPlatformOutput {
    pub fn validate(&self) -> anyhow::Result<()> {
        let kernel = crate::cca_artifacts::parse_manifest(&self.kernel_manifest)?;
        for (key, expected) in [
            ("architecture", "aarch64"),
            ("revision", crate::cca_pins::LINUX_REVISION),
            ("kernel_release", crate::cca_pins::LINUX_RELEASE),
            ("config_sha256", crate::cca_pins::LINUX_CONFIG_SHA256),
            ("Image_sha256", crate::cca_pins::LINUX_IMAGE_SHA256),
        ] {
            crate::cca_artifacts::require_manifest_value(&kernel, key, expected)?;
        }
        crate::cca_artifacts::verify_sha256(
            &self.host_kernel,
            crate::cca_pins::LINUX_IMAGE_SHA256,
            "CCA v15 Image",
        )?;
        crate::cca_artifacts::verify_sha256(
            &self.kernel_config,
            crate::cca_pins::LINUX_CONFIG_SHA256,
            "CCA v15 config",
        )?;

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
        anyhow::ensure!(
            self.host_initrd.is_file(),
            "CCA host initrd not found at {}",
            self.host_initrd.display()
        );
        anyhow::ensure!(
            std::fs::metadata(&self.host_initrd)?.len() <= crate::cca_pins::QEMU_INITRD_MAX_SIZE,
            "CCA host initrd is too large for its reserved physical range"
        );
        Ok(())
    }
}

struct ArchiveHashes {
    kernel: String,
    rmm: String,
    tfa: String,
    initrd: String,
}

struct ArchiveNames {
    kernel: String,
    rmm: String,
    tfa: String,
    initrd: String,
}

impl ArchiveNames {
    fn new(version: &str) -> Self {
        Self {
            kernel: format!("openvmm-test-linux-cca-v15.aarch64.{version}.tar.gz"),
            rmm: format!("openvmm-test-rmm-cca.aarch64.{version}.tar.gz"),
            tfa: format!("openvmm-test-tfa-cca.aarch64.{version}.tar.gz"),
            initrd: format!("openvmm-test-initrd.aarch64.{version}.tar.gz"),
        }
    }
}
