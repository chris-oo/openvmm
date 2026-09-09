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
}
