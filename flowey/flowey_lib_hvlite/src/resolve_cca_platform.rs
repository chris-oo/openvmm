// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compatibility entry point for the QEMU CCA platform resolver.
//!
//! New payload-only callers should use [`crate::resolve_cca_payload`].

use crate::resolve_cca_payload::CcaPayloadOutput;
use crate::resolve_cca_qemu_platform::CcaQemuPlatformOutput;
use flowey::node::prelude::*;

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

impl Config {
    fn split(
        self,
    ) -> anyhow::Result<(
        crate::resolve_cca_payload::Config,
        crate::resolve_cca_qemu_platform::Config,
    )> {
        self.version
            .as_ref()
            .context("CCA openvmm-deps release is not configured")?;
        self.kernel_archive_sha256
            .as_ref()
            .context("CCA kernel archive SHA-256 is not configured")?;
        self.rmm_archive_sha256
            .as_ref()
            .context("CCA TF-RMM archive SHA-256 is not configured")?;
        self.tfa_archive_sha256
            .as_ref()
            .context("CCA TF-A archive SHA-256 is not configured")?;
        self.initrd_archive_sha256
            .as_ref()
            .context("CCA initrd archive SHA-256 is not configured")?;
        crate::cca_artifacts::validate_local_archives(
            &[
                self.local_kernel_archive.is_some(),
                self.local_rmm_archive.is_some(),
                self.local_tfa_archive.is_some(),
                self.local_initrd_archive.is_some(),
            ],
            "local CCA configuration requires kernel, TF-RMM, TF-A, and initrd archives",
        )?;
        Ok((
            crate::resolve_cca_payload::Config {
                version: self.version.clone(),
                kernel_archive_sha256: self.kernel_archive_sha256,
                initrd_archive_sha256: self.initrd_archive_sha256,
                local_kernel_archive: self.local_kernel_archive,
                local_initrd_archive: self.local_initrd_archive,
            },
            crate::resolve_cca_qemu_platform::Config {
                version: self.version,
                rmm_archive_sha256: self.rmm_archive_sha256,
                tfa_archive_sha256: self.tfa_archive_sha256,
                local_rmm_archive: self.local_rmm_archive,
                local_tfa_archive: self.local_tfa_archive,
            },
        ))
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
        ctx.import::<crate::resolve_cca_payload::Node>();
        ctx.import::<crate::resolve_cca_qemu_platform::Node>();
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
        let (payload_config, qemu_config) = config.split()?;
        ctx.config(payload_config);
        ctx.config(qemu_config);
        let platform = ctx.reqv(crate::resolve_cca_qemu_platform::Request::Get);
        ctx.emit_rust_step("forward QEMU CCA platform", |ctx| {
            let platform = platform.claim(ctx);
            let outputs = outputs.claim(ctx);
            move |rt| {
                let output = CcaPlatformOutput::from(rt.read(platform));
                rt.write_all(outputs, &output);
                Ok(())
            }
        });
        Ok(())
    }
}

impl From<CcaQemuPlatformOutput> for CcaPlatformOutput {
    fn from(platform: CcaQemuPlatformOutput) -> Self {
        Self {
            host_kernel: platform.payload.host_kernel,
            realm_kernel: platform.payload.realm_kernel,
            kernel_config: platform.payload.kernel_config,
            kernel_manifest: platform.payload.kernel_manifest,
            host_initrd: platform.payload.initrd,
            firmware: platform.firmware,
            firmware_manifest: platform.firmware_manifest,
            rmm_image: platform.rmm_image,
            rmm_manifest: platform.rmm_manifest,
        }
    }
}

impl CcaPlatformOutput {
    pub fn validate(&self) -> anyhow::Result<()> {
        CcaQemuPlatformOutput {
            payload: CcaPayloadOutput {
                host_kernel: self.host_kernel.clone(),
                realm_kernel: self.realm_kernel.clone(),
                kernel_config: self.kernel_config.clone(),
                kernel_manifest: self.kernel_manifest.clone(),
                initrd: self.host_initrd.clone(),
            },
            firmware: self.firmware.clone(),
            firmware_manifest: self.firmware_manifest.clone(),
            rmm_image: self.rmm_image.clone(),
            rmm_manifest: self.rmm_manifest.clone(),
        }
        .validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_config() -> Config {
        Config {
            version: Some("local-release".into()),
            kernel_archive_sha256: Some("kernel-hash".into()),
            rmm_archive_sha256: Some("rmm-hash".into()),
            tfa_archive_sha256: Some("tfa-hash".into()),
            initrd_archive_sha256: Some("initrd-hash".into()),
            ..Default::default()
        }
    }

    #[test]
    fn forwards_release_overrides() {
        let (payload, qemu) = release_config().split().unwrap();
        assert_eq!(payload.version.as_deref(), Some("local-release"));
        assert_eq!(qemu.version, payload.version);
        assert_eq!(
            payload.kernel_archive_sha256.as_deref(),
            Some("kernel-hash")
        );
        assert_eq!(
            payload.initrd_archive_sha256.as_deref(),
            Some("initrd-hash")
        );
        assert_eq!(qemu.rmm_archive_sha256.as_deref(), Some("rmm-hash"));
        assert_eq!(qemu.tfa_archive_sha256.as_deref(), Some("tfa-hash"));
        assert!(payload.local_kernel_archive.is_none());
        assert!(payload.local_initrd_archive.is_none());
        assert!(qemu.local_rmm_archive.is_none());
        assert!(qemu.local_tfa_archive.is_none());
    }

    #[test]
    fn preserves_local_archive_coherence() {
        for mask in 0..16 {
            let local = |bit, name: &str| {
                (mask & (1 << bit) != 0)
                    .then(|| ConfigVar(ReadVar::from_static(PathBuf::from(name))))
            };
            let config = Config {
                local_kernel_archive: local(0, "kernel"),
                local_rmm_archive: local(1, "rmm"),
                local_tfa_archive: local(2, "tfa"),
                local_initrd_archive: local(3, "initrd"),
                ..release_config()
            };
            let result = config.split();
            assert_eq!(result.is_ok(), mask == 0 || mask == 15, "{mask}");
            if mask == 15 {
                let (payload, qemu) = result.unwrap();
                assert!(payload.local_kernel_archive.is_some());
                assert!(payload.local_initrd_archive.is_some());
                assert!(qemu.local_rmm_archive.is_some());
                assert!(qemu.local_tfa_archive.is_some());
            }
        }
    }

    #[test]
    fn preserves_required_configuration() {
        assert!(Config::default().split().is_err());
        for field in 0..5 {
            let mut config = release_config();
            match field {
                0 => config.version = None,
                1 => config.kernel_archive_sha256 = None,
                2 => config.rmm_archive_sha256 = None,
                3 => config.tfa_archive_sha256 = None,
                4 => config.initrd_archive_sha256 = None,
                _ => unreachable!(),
            }
            assert!(config.split().is_err(), "{field}");
        }
    }
}
