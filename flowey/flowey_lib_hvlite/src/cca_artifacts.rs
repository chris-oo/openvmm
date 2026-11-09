// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared CCA dependency artifact validation.

use anyhow::Context as _;
use flowey::node::prelude::RustRuntimeServices;
use sha2::Digest as _;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn validate_local_archives(present: &[bool], message: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        present.iter().all(|present| *present) || present.iter().all(|present| !present),
        "{message}"
    );
    Ok(())
}

pub(crate) fn resolve_archive(
    rt: &mut RustRuntimeServices<'_>,
    persistent_dir: Option<&Path>,
    archive: &Path,
    expected_name: &str,
    expected_sha256: &str,
    label: &str,
) -> anyhow::Result<PathBuf> {
    let cache_root = persistent_dir
        .map(Path::to_owned)
        .unwrap_or_else(|| rt.sh.current_dir());
    validate_archive_source(&cache_root, archive)?;
    anyhow::ensure!(
        archive.file_name() == Some(expected_name.as_ref()),
        "{label} name does not match expected {expected_name}: {}",
        archive.display()
    );
    verify_sha256(archive, expected_sha256, label)?;
    flowey_lib_common::_util::extract::extract_tar_gz_if_new(
        rt,
        persistent_dir,
        archive,
        expected_sha256,
    )
}

#[expect(
    clippy::disallowed_methods,
    reason = "reject archive aliases into cache trees before extraction invalidates them"
)]
fn validate_archive_source(cache_root: &Path, archive: &Path) -> anyhow::Result<()> {
    let cache_root = match fs::canonicalize(cache_root) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("cannot resolve archive cache root"),
    };
    let archive_source =
        fs::canonicalize(archive).context("cannot resolve local CCA archive input")?;
    // Keep the final path component unresolved too: invalidation can remove a
    // symlink in the cache even when its target is an external archive.
    let archive_location = fs::canonicalize(
        archive
            .parent()
            .context("CCA archive input has no parent")?,
    )?
    .join(
        archive
            .file_name()
            .context("CCA archive input has no filename")?,
    );
    for owned in [".flowey_info", "extracted"] {
        let owned = match fs::canonicalize(cache_root.join(owned)) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("cannot resolve archive cache tree"),
        };
        anyhow::ensure!(
            !archive_source.starts_with(&owned) && !archive_location.starts_with(&owned),
            "CCA archive input must be outside Flowey cache metadata and extraction trees: {}",
            archive.display()
        );
    }
    Ok(())
}

pub fn parse_manifest(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read manifest {}", path.display()))?;
    let mut manifest = BTreeMap::new();
    for line in contents.lines() {
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("invalid manifest line: {line}"))?;
        anyhow::ensure!(!key.is_empty() && !value.is_empty(), "empty manifest field");
        anyhow::ensure!(
            manifest.insert(key.into(), value.into()).is_none(),
            "duplicate manifest key: {key}"
        );
    }
    Ok(manifest)
}

pub fn require_manifest_value(
    manifest: &BTreeMap<String, String>,
    key: &str,
    expected: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        manifest.get(key).map(String::as_str) == Some(expected),
        "manifest {key} does not match expected value {expected}"
    );
    Ok(())
}

pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let mut output = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(output, "{byte:02x}")?;
    }
    Ok(output)
}

pub fn verify_sha256(path: &Path, expected: &str, label: &str) -> anyhow::Result<()> {
    let actual = sha256_file(path)?;
    anyhow::ensure!(
        actual == expected,
        "{label} SHA-256 mismatch: expected {expected}, got {actual}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use test_with_tracing::test;

    #[test]
    fn archive_sources_in_cache_trees_are_rejected_without_mutation() -> anyhow::Result<()> {
        let root = tempfile::tempdir_in(std::env::current_dir()?)?;
        let cache = root.path().join("cache");
        fs::create_dir(&cache)?;
        for tree in [".flowey_info", "extracted"] {
            let directory = cache.join(tree);
            fs::create_dir(&directory)?;
            let input = directory.join("payload.tar.gz");
            fs::write(&input, b"preserved input")?;
            assert!(validate_archive_source(&cache, &input).is_err());
            assert_eq!(fs::read(input)?, b"preserved input");
        }
        let valid = root.path().join("payload.tar.gz");
        fs::write(&valid, b"external archive")?;
        validate_archive_source(&cache, &valid)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn archive_source_cache_aliases_are_rejected_in_both_directions() -> anyhow::Result<()> {
        let root = tempfile::tempdir_in(std::env::current_dir()?)?;
        let cache = root.path().join("cache");
        let metadata = cache.join(".flowey_info");
        fs::create_dir_all(&metadata)?;
        let external = root.path().join("external.tar.gz");
        fs::write(&external, b"external")?;
        let metadata_alias = metadata.join("alias.tar.gz");
        std::os::unix::fs::symlink(&external, &metadata_alias)?;
        assert!(validate_archive_source(&cache, &metadata_alias).is_err());
        assert!(
            fs::symlink_metadata(&metadata_alias)?
                .file_type()
                .is_symlink()
        );
        let internal = metadata.join("source.tar.gz");
        fs::write(&internal, b"internal")?;
        let external_alias = root.path().join("alias.tar.gz");
        std::os::unix::fs::symlink(&internal, &external_alias)?;
        assert!(validate_archive_source(&cache, &external_alias).is_err());
        assert_eq!(fs::read(&internal)?, b"internal");
        Ok(())
    }

    #[test]
    fn local_archives_are_all_or_none() {
        for count in [2, 4] {
            for mask in 0..1 << count {
                let present: Vec<_> = (0..count).map(|bit| mask & (1 << bit) != 0).collect();
                assert_eq!(
                    validate_local_archives(&present, "incomplete local archives").is_ok(),
                    mask == 0 || mask == (1 << count) - 1,
                    "{present:?}"
                );
            }
        }
    }

    #[test]
    fn rejects_duplicate_manifest_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manifest.txt");
        fs::write(&path, "revision=one\nrevision=two\n").unwrap();
        assert!(parse_manifest(&path).is_err());
    }

    #[test]
    fn verifies_file_sha256() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"openvmm").unwrap();
        verify_sha256(
            file.path(),
            "2ebaf76b44d8459a0d848c3ad5f38fa9ec8936942be3cbe3d7e91469b1d32b1d",
            "test file",
        )
        .unwrap();
    }

    #[test]
    fn fvp_payload_rejects_altered_local_archive_bytes() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let archive = directory.path().join("initrd.tar.gz");
        fs::write(&archive, b"altered archive bytes").unwrap();
        let error = verify_sha256(
            &archive,
            crate::cca_pins::INITRD_ARCHIVE_SHA256,
            "initrd.tar.gz",
        )
        .unwrap_err();
        assert!(error.to_string().contains("SHA-256 mismatch"), "{error:#}");
    }
}
