// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve pinned CCA kernels and base initrds without platform firmware.

use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CcaPayloadOutput {
    pub kind: CcaPayloadKind,
    pub host_kernel: PathBuf,
    pub realm_kernel: PathBuf,
    pub kernel_config: PathBuf,
    pub kernel_manifest: PathBuf,
    pub initrd: PathBuf,
}

/// Complete payload identities supported by the FVP runtime.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CcaPayloadKind {
    #[default]
    CcaV15,
    GuestMemfdInPlace,
}

impl Artifact for CcaPayloadOutput {}

flowey_config! {
    pub struct Config {
        /// Explicit local in-place tuple: Image, config, manifest.txt, base initrd.
        pub local_in_place_payload: Option<ConfigVar<PathBuf>>,
        /// Defaults to the pinned CCA openvmm-deps release.
        pub version: Option<String>,
        /// Archive hashes default to the checked-in release identities.
        pub kernel_archive_sha256: Option<String>,
        pub initrd_archive_sha256: Option<String>,
        /// Supply both local release-shaped archives, or neither.
        pub local_kernel_archive: Option<ConfigVar<PathBuf>>,
        pub local_initrd_archive: Option<ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        Get(WriteVar<CcaPayloadOutput>),
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

        if let Some(root) = config.local_in_place_payload {
            anyhow::ensure!(
                config.version.is_none()
                    && config.kernel_archive_sha256.is_none()
                    && config.initrd_archive_sha256.is_none()
                    && config.local_kernel_archive.is_none()
                    && config.local_initrd_archive.is_none(),
                "in-place payload directories cannot be combined with archive overrides"
            );
            ctx.emit_rust_step("resolve pinned local in-place CCA payload", |ctx| {
                let root = root.0.claim(ctx);
                let outputs = outputs.claim(ctx);
                move |rt| {
                    let root = rt.read(root).absolute()?;
                    let output = CcaPayloadOutput {
                        kind: CcaPayloadKind::GuestMemfdInPlace,
                        host_kernel: root.join("Image"),
                        realm_kernel: root.join("Image"),
                        kernel_config: root.join("config"),
                        kernel_manifest: root.join("manifest.txt"),
                        initrd: root.join("initrd"),
                    };
                    output.validate_fvp()?;
                    rt.write_all(outputs, &output);
                    Ok(())
                }
            });
            return Ok(());
        }

        let Config {
            local_in_place_payload: _,
            version,
            kernel_archive_sha256,
            initrd_archive_sha256,
            local_kernel_archive,
            local_initrd_archive,
        } = config;
        let version = version.unwrap_or_else(|| crate::cca_pins::OPENVMM_DEPS_RELEASE.into());
        let kernel_hash =
            kernel_archive_sha256.unwrap_or_else(|| crate::cca_pins::KERNEL_ARCHIVE_SHA256.into());
        let initrd_hash =
            initrd_archive_sha256.unwrap_or_else(|| crate::cca_pins::INITRD_ARCHIVE_SHA256.into());
        let kernel_name = format!("openvmm-test-linux-cca-v15.aarch64.{version}.tar.gz");
        let initrd_name = format!("openvmm-test-initrd.aarch64.{version}.tar.gz");
        crate::cca_artifacts::validate_local_archives(
            &[
                local_kernel_archive.is_some(),
                local_initrd_archive.is_some(),
            ],
            "local CCA payload configuration requires kernel and initrd archives",
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
        let kernel_archive = local_kernel_archive
            .map(|archive| archive.0)
            .unwrap_or_else(|| download(kernel_name.clone(), ctx));
        let initrd_archive = local_initrd_archive
            .map(|archive| archive.0)
            .unwrap_or_else(|| download(initrd_name.clone(), ctx));
        let persistent_dir = ctx.persistent_dir();
        ctx.emit_rust_step("resolve CCA payload archives", |ctx| {
            let kernel_archive = kernel_archive.claim(ctx);
            let initrd_archive = initrd_archive.claim(ctx);
            let persistent_dir = persistent_dir.claim(ctx);
            let outputs = outputs.claim(ctx);
            move |rt| {
                let kernel_archive = rt.read(kernel_archive).absolute()?;
                let initrd_archive = rt.read(initrd_archive).absolute()?;
                let persistent_dir = persistent_dir.map(|dir| rt.read(dir));
                let kernel_dir = crate::cca_artifacts::resolve_archive(
                    rt,
                    persistent_dir.as_deref(),
                    &kernel_archive,
                    &kernel_name,
                    &kernel_hash,
                    "CCA kernel archive",
                )?;
                let initrd_dir = crate::cca_artifacts::resolve_archive(
                    rt,
                    persistent_dir.as_deref(),
                    &initrd_archive,
                    &initrd_name,
                    &initrd_hash,
                    "CCA initrd archive",
                )?;
                let output = CcaPayloadOutput {
                    kind: CcaPayloadKind::CcaV15,
                    host_kernel: kernel_dir.join("Image"),
                    realm_kernel: kernel_dir.join("Image"),
                    kernel_config: kernel_dir.join("config"),
                    kernel_manifest: kernel_dir.join("manifest.txt"),
                    initrd: initrd_dir.join("initrd"),
                };
                output.validate()?;
                rt.write_all(outputs, &output);
                Ok(())
            }
        });
        Ok(())
    }
}

impl CcaPayloadOutput {
    /// Validate the qualified FVP payload, including cached extracted bytes.
    /// QEMU may use a different local base initrd through [`Self::validate`].
    pub fn validate_fvp(&self) -> anyhow::Result<()> {
        self.validate()?;
        validate_fvp_kernel_config(&fs_err::read_to_string(&self.kernel_config)?)?;
        validate_fvp_initrd(&self.initrd)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let manifest = crate::cca_artifacts::parse_manifest(&self.kernel_manifest)?;
        let (image_hash, config_hash) = match self.kind {
            CcaPayloadKind::CcaV15 => {
                validate_kernel_manifest(&manifest)?;
                (
                    crate::cca_pins::LINUX_IMAGE_SHA256,
                    crate::cca_pins::LINUX_CONFIG_SHA256,
                )
            }
            CcaPayloadKind::GuestMemfdInPlace => {
                use vmm_test_images::cca_payload::guest_memfd_in_place as pins;
                for (key, expected) in [
                    ("architecture", "aarch64"),
                    ("revision", pins::LINUX_REVISION),
                    ("kernel_release", pins::LINUX_RELEASE),
                    ("config_sha256", pins::LINUX_CONFIG_SHA256),
                    ("Image_sha256", pins::LINUX_IMAGE_SHA256),
                ] {
                    crate::cca_artifacts::require_manifest_value(&manifest, key, expected)?;
                }
                (pins::LINUX_IMAGE_SHA256, pins::LINUX_CONFIG_SHA256)
            }
        };
        anyhow::ensure!(
            self.host_kernel == self.realm_kernel,
            "CCA host and Realm must use the same unified kernel"
        );
        crate::cca_artifacts::verify_sha256(&self.host_kernel, image_hash, "CCA Image")?;
        crate::cca_artifacts::verify_sha256(&self.kernel_config, config_hash, "CCA config")?;
        validate_initrd(&self.initrd)
    }
}

fn validate_fvp_kernel_config(config: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.lines().any(|line| line == "CONFIG_SMC91X=y"),
        "FVP CCA kernel requires CONFIG_SMC91X=y; the base initrd does not load the host NIC module"
    );
    Ok(())
}

fn validate_fvp_initrd(initrd: &Path) -> anyhow::Result<()> {
    validate_initrd(initrd)?;
    crate::cca_artifacts::verify_sha256(
        initrd,
        crate::cca_pins::BASE_INITRD_SHA256,
        "FVP CCA base initrd",
    )
}

fn validate_initrd(initrd: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        initrd.is_file(),
        "CCA host initrd not found at {}",
        initrd.display()
    );
    Ok(())
}

fn validate_kernel_manifest(manifest: &BTreeMap<String, String>) -> anyhow::Result<()> {
    for (key, expected) in [
        ("architecture", "aarch64"),
        ("revision", crate::cca_pins::LINUX_REVISION),
        ("kernel_release", crate::cca_pins::LINUX_RELEASE),
        ("config_sha256", crate::cca_pins::LINUX_CONFIG_SHA256),
        ("Image_sha256", crate::cca_pins::LINUX_IMAGE_SHA256),
    ] {
        crate::cca_artifacts::require_manifest_value(manifest, key, expected)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn fvp_kernel_requires_builtin_host_nic() {
        validate_fvp_kernel_config("CONFIG_OTHER=y\nCONFIG_SMC91X=y\n").unwrap();
        for config in [
            "",
            "CONFIG_SMC91X=m\n",
            "# CONFIG_SMC91X is not set\n",
            "CONFIG_SMSC911X=y\n",
            "# CONFIG_SMC91X=y\n",
        ] {
            assert!(validate_fvp_kernel_config(config).is_err(), "{config}");
        }
    }

    #[test]
    #[ignore = "requires the pinned local in-place test payload"]
    fn local_in_place_payload_matches_pins() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/cca-tdisp-stage-a/test-platform/payload");
        CcaPayloadOutput {
            kind: CcaPayloadKind::GuestMemfdInPlace,
            host_kernel: root.join("Image"),
            realm_kernel: root.join("Image"),
            kernel_config: root.join("config"),
            kernel_manifest: root.join("manifest.txt"),
            initrd: root.join("initrd"),
        }
        .validate_fvp()
        .unwrap();
    }

    #[test]
    fn in_place_payload_rejects_changed_files_and_v15_manifest() {
        use vmm_test_images::cca_payload::guest_memfd_in_place as pins;
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = directory.path();
        fs_err::write(root.join("manifest.txt"), format!(
            "architecture=aarch64\nrevision={}\nkernel_release={}\nconfig_sha256={}\nImage_sha256={}\n",
            pins::LINUX_REVISION, pins::LINUX_RELEASE,
            pins::LINUX_CONFIG_SHA256, pins::LINUX_IMAGE_SHA256,
        )).unwrap();
        fs_err::write(root.join("Image"), b"changed in-place kernel").unwrap();
        let mut payload = CcaPayloadOutput {
            kind: CcaPayloadKind::GuestMemfdInPlace,
            host_kernel: root.join("Image"),
            realm_kernel: root.join("Image"),
            kernel_config: root.join("config"),
            kernel_manifest: root.join("manifest.txt"),
            initrd: root.join("initrd"),
        };
        assert!(
            payload
                .validate_fvp()
                .unwrap_err()
                .to_string()
                .contains("SHA-256 mismatch")
        );
        payload.kind = CcaPayloadKind::CcaV15;
        assert!(
            payload
                .validate_fvp()
                .unwrap_err()
                .to_string()
                .contains("manifest revision")
        );
    }

    #[test]
    fn fvp_payload_rejects_tampered_cached_kernel() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = directory.path();
        fs_err::write(
            root.join("manifest.txt"),
            format!(
                "architecture=aarch64\nrevision={}\nkernel_release={}\nconfig_sha256={}\nImage_sha256={}\n",
                crate::cca_pins::LINUX_REVISION,
                crate::cca_pins::LINUX_RELEASE,
                crate::cca_pins::LINUX_CONFIG_SHA256,
                crate::cca_pins::LINUX_IMAGE_SHA256,
            ),
        ).unwrap();
        fs_err::write(root.join("Image"), b"modified cached Image").unwrap();
        let payload = CcaPayloadOutput {
            kind: CcaPayloadKind::CcaV15,
            host_kernel: root.join("Image"),
            realm_kernel: root.join("Image"),
            kernel_config: root.join("config"),
            kernel_manifest: root.join("manifest.txt"),
            initrd: root.join("initrd"),
        };
        let error = payload.validate_fvp().unwrap_err();
        assert!(
            error.to_string().contains("CCA Image SHA-256 mismatch"),
            "{error:#}"
        );
    }

    #[test]
    fn fvp_payload_rejects_tampered_cached_initrd_without_restricting_qemu() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let initrd = directory.path().join("initrd");
        for bytes in [
            b"".as_slice(),
            b"truncated initrd",
            b"modified cached initrd",
        ] {
            fs_err::write(&initrd, bytes).unwrap();
            validate_initrd(&initrd).unwrap();
            let error = validate_fvp_initrd(&initrd).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("FVP CCA base initrd SHA-256 mismatch"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn initrd_must_be_a_regular_file() {
        let file = std::env::current_exe().unwrap();
        validate_initrd(&file).unwrap();
        assert!(validate_initrd(file.parent().unwrap()).is_err());
        assert!(validate_initrd(&file.join("missing-initrd")).is_err());
    }

    #[test]
    fn validates_kernel_manifest_without_platform_firmware() {
        let manifest: BTreeMap<String, String> = [
            ("architecture", "aarch64"),
            ("revision", crate::cca_pins::LINUX_REVISION),
            ("kernel_release", crate::cca_pins::LINUX_RELEASE),
            ("config_sha256", crate::cca_pins::LINUX_CONFIG_SHA256),
            ("Image_sha256", crate::cca_pins::LINUX_IMAGE_SHA256),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
        validate_kernel_manifest(&manifest).unwrap();
        for key in manifest.keys() {
            let mut missing = manifest.clone();
            missing.remove(key);
            assert!(validate_kernel_manifest(&missing).is_err(), "{key}");
            let mut changed = manifest.clone();
            changed.insert(key.clone(), "wrong".into());
            assert!(validate_kernel_manifest(&changed).is_err(), "{key}");
        }
    }
}
