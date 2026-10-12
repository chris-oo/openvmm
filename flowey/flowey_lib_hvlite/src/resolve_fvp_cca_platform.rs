// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve prebuilt FVP firmware and payloads independently of the licensed model.

use flowey::node::prelude::*;

flowey_config! {
    pub struct Config {
        /// Release containing the FVP firmware and payload archives.
        pub version: Option<String>,
        /// Expected archive digest, including for local development packages.
        pub archive_sha256: Option<String>,
        /// Local release-shaped archive instead of a GitHub download.
        pub local_archive: Option<ConfigVar<PathBuf>>,
        /// Expected digest of the separate host and reference-guest payload archive.
        pub payload_archive_sha256: Option<String>,
        /// Local payload archive instead of a GitHub download.
        pub local_payload_archive: Option<ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        Get(WriteVar<PathBuf>),
        GetPayload(WriteVar<PathBuf>),
    }
}

new_flow_node_with_config!(struct Node);

fn validate_package_tree(root: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        fs_err::symlink_metadata(root)?.is_dir(),
        "FVP dependency package root must be a regular directory"
    );
    let mut directories = vec![(root.to_owned(), 0)];
    let mut entries = 0;
    while let Some((directory, depth)) = directories.pop() {
        for entry in fs_err::read_dir(directory)? {
            let entry = entry?;
            entries += 1;
            anyhow::ensure!(
                entries <= 4096,
                "FVP dependency package has too many entries"
            );
            let kind = entry.file_type()?;
            anyhow::ensure!(
                kind.is_file() || kind.is_dir(),
                "FVP dependency package contains a link or special file: {}",
                entry.path().display()
            );
            if kind.is_dir() {
                anyhow::ensure!(depth < 32, "FVP dependency package is nested too deeply");
                directories.push((entry.path(), depth + 1));
            }
        }
    }
    Ok(())
}

fn validate_source(version: Option<&str>, digest: Option<&str>) -> anyhow::Result<()> {
    let version =
        version.context("FVP dependency release is not configured; specify --fvp-deps-version")?;
    anyhow::ensure!(
        !version.is_empty()
            && version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte)),
        "invalid FVP dependency release name"
    );
    let digest = digest.context(
        "FVP archive digest is not configured; specify the corresponding package SHA-256",
    )?;
    anyhow::ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "FVP archive digest must be a lowercase SHA-256 value"
    );
    Ok(())
}

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
        let mut firmware_outputs = Vec::new();
        let mut payload_outputs = Vec::new();
        for request in requests {
            match request {
                Request::Get(output) => firmware_outputs.push(output),
                Request::GetPayload(output) => payload_outputs.push(output),
            }
        }
        if firmware_outputs.is_empty() && payload_outputs.is_empty() {
            return Ok(());
        }
        for (kind, outputs, digest, local_archive, required) in [
            (
                "fvp-cca",
                firmware_outputs,
                config.archive_sha256,
                config.local_archive,
                &[
                    "manifest.json",
                    "SHA256SUMS",
                    "cca-3world.yaml",
                    "runtime-overlay.yaml",
                    "cca-3world/bl1.bin",
                    "cca-3world/fip.bin",
                    "cca-3world/dt_bootargs.dtb",
                ][..],
            ),
            (
                "fvp-cca-payload",
                payload_outputs,
                config.payload_archive_sha256,
                config.local_payload_archive,
                &[
                    "manifest.json",
                    "SHA256SUMS",
                    "host/Image",
                    "host/initrd",
                    "guest/Image",
                    "guest/initrd",
                ][..],
            ),
        ] {
            if outputs.is_empty() {
                continue;
            }
            validate_source(config.version.as_deref(), digest.as_deref())
                .with_context(|| format!("invalid {kind} package identity"))?;
            let version = config
                .version
                .clone()
                .context("missing FVP dependency release")?;
            let digest = digest.context("missing FVP archive digest")?;
            let filename = format!("openvmm-test-{kind}.aarch64.{version}.tar.gz");
            let archive = if let Some(archive) = local_archive {
                archive.0
            } else {
                ctx.reqv(|path| flowey_lib_common::download_gh_release::Request {
                    repo_owner: crate::common::OPENVMM_GITHUB_OWNER.into(),
                    repo_name: crate::common::OPENVMM_DEPS_GITHUB_REPO.into(),
                    needs_auth: false,
                    tag: version,
                    file_name: filename.clone(),
                    path,
                })
            };
            let persistent_dir = ctx.persistent_dir();
            ctx.emit_rust_step(format!("resolve prebuilt {kind} package"), |ctx| {
                let archive = archive.claim(ctx);
                let persistent_dir = persistent_dir.claim(ctx);
                let outputs = outputs.claim(ctx);
                move |rt| {
                    let archive = rt.read(archive).absolute()?;
                    let persistent_dir = persistent_dir.map(|directory| rt.read(directory));
                    let directory = crate::cca_artifacts::resolve_archive(
                        rt,
                        persistent_dir.as_deref(),
                        &archive,
                        &filename,
                        &digest,
                        kind,
                    )?;
                    validate_package_tree(&directory)?;
                    for relative in required {
                        anyhow::ensure!(
                            fs_err::symlink_metadata(directory.join(relative))?.is_file(),
                            "FVP dependency package requires a regular file at {relative}"
                        );
                    }
                    rt.write_all(outputs, &directory);
                    Ok(())
                }
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn rejects_missing_or_invalid_package_identity() {
        let digest = "0".repeat(64);
        assert!(validate_source(Some("0.3.0-local-fvp"), Some(&digest)).is_ok());
        assert!(validate_source(None, Some(&digest)).is_err());
        assert!(validate_source(Some("0.3.0-local-fvp"), None).is_err());
        for version in ["", "../release", "release/name", "release name"] {
            assert!(validate_source(Some(version), Some(&digest)).is_err());
        }
        for digest in ["", "abcd", &"A".repeat(64), &"g".repeat(64)] {
            assert!(validate_source(Some("0.3.0-local-fvp"), Some(digest)).is_err());
        }
    }

    #[test]
    fn package_tree_accepts_regular_nested_files() {
        let directory = tempfile::tempdir().unwrap();
        fs_err::create_dir(directory.path().join("cca-3world")).unwrap();
        fs_err::write(directory.path().join("cca-3world/bl1.bin"), b"firmware").unwrap();
        validate_package_tree(directory.path()).unwrap();
    }

    #[test]
    fn package_tree_enforces_entry_limit() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..4096 {
            fs_err::write(directory.path().join(index.to_string()), b"").unwrap();
        }
        validate_package_tree(directory.path()).unwrap();
        fs_err::write(directory.path().join("extra"), b"").unwrap();
        let error = validate_package_tree(directory.path()).unwrap_err();
        assert!(error.to_string().contains("too many entries"), "{error:#}");
    }

    #[test]
    fn package_tree_enforces_depth_limit() {
        let directory = tempfile::tempdir().unwrap();
        let mut path = directory.path().to_owned();
        for _ in 0..32 {
            path.push("nested");
            fs_err::create_dir(&path).unwrap();
        }
        validate_package_tree(directory.path()).unwrap();
        path.push("extra");
        fs_err::create_dir(&path).unwrap();
        let error = validate_package_tree(directory.path()).unwrap_err();
        assert!(error.to_string().contains("too deeply"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn package_tree_rejects_links_before_copying() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("linked")).unwrap();
        assert!(validate_package_tree(directory.path()).is_err());
    }
}
