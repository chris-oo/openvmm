// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stage the unchanged run-20260914-3 guest independently of the FVP L1 payload.

use crate::write_incubator_target_runner::FvpPlatformRoots;
use crate::write_incubator_target_runner::IncubatorPlatform;
use anyhow::Context as _;
use flowey::node::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

pub const SHARE_INPUTS: &[&str] = &["cca-tdisp-guest/Image", "cca-tdisp-guest/initrd"];
/// The unchanged guest runs its own DA script instead of a guest agent.
pub const TEST_NAME: &str =
    "aarch64_exclusive::tdisp_ahci::openvmm_linux_aarch64_boot_linux_direct_cca_tdisp_ahci";
const PROVENANCE: &str = "cca-tdisp-guest/provenance.json";
const REFERENCE_RUN: &str = "run-20260914-3";
const PINS: &[(&str, &str, &str)] = &[
    (
        "Image",
        SHARE_INPUTS[0],
        "6bef4c54ac93d8513ad7f125737c9ff0a8b0b7e34720e77253e2b63460002437",
    ),
    (
        "guest-initrd.cpio",
        SHARE_INPUTS[1],
        "d3ba987d83bd46a60cf2199d7989a7b940499065e1011125775034b5710dad92",
    ),
];

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Provenance {
    schema_version: u32,
    reference_run: String,
    source_root: PathBuf,
    files: Vec<ProvenanceFile>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ProvenanceFile {
    source: String,
    staged: String,
    sha256: String,
}

/// Reject incompatible graph inputs before discovery, output creation, or cleanup.
pub fn validate_options(
    present: bool,
    platform: Option<IncubatorPlatform>,
    custom_kernel: bool,
    custom_kernel_modules: bool,
) -> anyhow::Result<()> {
    if present {
        anyhow::ensure!(
            platform == Some(IncubatorPlatform::FvpCcaRealmVfio),
            "--cca-tdisp-guest-root requires the FVP CCA Realm VFIO profile"
        );
        anyhow::ensure!(
            !custom_kernel && !custom_kernel_modules,
            "--cca-tdisp-guest-root conflicts with custom kernel options"
        );
    }
    Ok(())
}

/// Resolve exactly the supplied root, protect it from output cleanup, and check
/// both reference files. No guest artifact is built or changed.
pub fn resolve(root: &Path, output: &Path) -> anyhow::Result<PathBuf> {
    let root = FvpPlatformRoots::resolve_fvp_payload_root(root, output)?;
    verify_files(&root, PINS, false)?;
    Ok(root)
}

fn verify_files(root: &Path, pins: &[(&str, &str, &str)], staged: bool) -> anyhow::Result<()> {
    for (source, destination, hash) in pins {
        let name = if staged { destination } else { source };
        let path = root.join(name);
        anyhow::ensure!(
            fs_err::symlink_metadata(&path)?.is_file(),
            "CCA TDISP guest input must be a regular file: {}",
            path.display()
        );
        crate::cca_artifacts::verify_sha256(&path, hash, name)?;
    }
    Ok(())
}

fn provenance(source: &Path, pins: &[(&str, &str, &str)]) -> Provenance {
    Provenance {
        schema_version: 1,
        reference_run: REFERENCE_RUN.into(),
        source_root: source.to_owned(),
        files: pins
            .iter()
            .map(|(source, staged, sha256)| ProvenanceFile {
                source: (*source).into(),
                staged: (*staged).into(),
                sha256: (*sha256).into(),
            })
            .collect(),
    }
}

/// Copy checked bytes to a separate guest directory and record their provenance.
pub fn stage(source: &Path, content: &Path) -> anyhow::Result<()> {
    stage_pinned(source, content, PINS)
}

/// Stage only after the shared test content directory has been initialized.
pub(crate) fn stage_for_run(
    ctx: &mut NodeCtx<'_>,
    source: Option<PathBuf>,
    content: ReadVar<PathBuf>,
) -> Option<ReadVar<SideEffect>> {
    source.map(|source| {
        ctx.emit_rust_step("stage unchanged pinned CCA TDISP guest", |ctx| {
            let content = content.claim(ctx);
            move |rt| stage(&source, &rt.read(content))
        })
    })
}

fn stage_pinned(source: &Path, content: &Path, pins: &[(&str, &str, &str)]) -> anyhow::Result<()> {
    let source = FvpPlatformRoots::resolve_fvp_payload_root(source, content)?;
    verify_files(&source, pins, false)?;
    let directory = content.join("cca-tdisp-guest");
    if directory.try_exists()? {
        anyhow::ensure!(
            fs_err::symlink_metadata(&directory)?.is_dir(),
            "CCA TDISP guest output must be a regular directory"
        );
    } else {
        fs_err::create_dir_all(&directory)?;
    }
    for (_, name, _) in pins {
        reject_non_file(&content.join(name))?;
    }
    reject_non_file(&content.join(PROVENANCE))?;
    for (name, destination, _) in pins {
        let destination = content.join(destination);
        // Remove an old regular file before copying, so a hard link cannot
        // turn an output update into a write to an input.
        if destination.try_exists()? {
            fs_err::remove_file(&destination)?;
        }
        fs_err::copy(source.join(name), destination)?;
    }
    verify_files(content, pins, true)?;
    verify_files(&source, pins, false)?;
    let metadata = content.join(PROVENANCE);
    if metadata.try_exists()? {
        fs_err::remove_file(&metadata)?;
    }
    fs_err::write(
        metadata,
        serde_json::to_vec_pretty(&provenance(&source, pins))?,
    )?;
    Ok(())
}

fn reject_non_file(path: &Path) -> anyhow::Result<()> {
    match fs_err::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(metadata.is_file(), "not a regular file: {}", path.display())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Reject stale optional inputs when disabled; otherwise recheck the exact pins
/// and provenance before writing an inventory or making a private input copy.
pub(crate) fn validate_staged(content: &Path, enabled: bool) -> anyhow::Result<()> {
    validate_staged_pinned(content, enabled, PINS)
}

/// The private L1 share contains the two guest files, not host provenance.
pub(crate) fn validate_private_copy(content: &Path) -> anyhow::Result<()> {
    verify_files(content, PINS, true)
}

fn validate_staged_pinned(
    content: &Path,
    enabled: bool,
    pins: &[(&str, &str, &str)],
) -> anyhow::Result<()> {
    let directory = content.join("cca-tdisp-guest");
    if !enabled {
        match fs_err::symlink_metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => anyhow::bail!("stale cca-tdisp-guest content without --cca-tdisp-guest-root"),
        }
    }
    anyhow::ensure!(
        fs_err::symlink_metadata(&directory)?.is_dir(),
        "CCA TDISP guest output must be a regular directory"
    );
    verify_files(content, pins, true)?;
    let path = content.join(PROVENANCE);
    let metadata = fs_err::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= 64 * 1024,
        "invalid CCA TDISP guest provenance file"
    );
    let actual: Provenance = serde_json::from_slice(&fs_err::read(path)?)?;
    anyhow::ensure!(
        actual.source_root.is_absolute() && actual == provenance(&actual.source_root, pins),
        "CCA TDISP guest provenance mismatch"
    );
    let names = fs_err::read_dir(directory)?
        .map(|entry| Ok(entry?.file_name()))
        .collect::<anyhow::Result<Vec<_>>>()
        .context("cannot read CCA TDISP guest inventory")?;
    anyhow::ensure!(
        names.len() == 3
            && names
                .iter()
                .all(|name| name == "Image" || name == "initrd" || name == "provenance.json"),
        "unexpected CCA TDISP guest inventory"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const HASH: &str = "2ebaf76b44d8459a0d848c3ad5f38fa9ec8936942be3cbe3d7e91469b1d32b1d";
    const TEST_PINS: &[(&str, &str, &str)] = &[
        ("Image", SHARE_INPUTS[0], HASH),
        ("guest-initrd.cpio", SHARE_INPUTS[1], HASH),
    ];

    #[test]
    fn profile_and_custom_kernel_guards() {
        for platform in [
            None,
            Some(IncubatorPlatform::QemuTcg),
            Some(IncubatorPlatform::QemuCca),
            Some(IncubatorPlatform::QemuCcaGuestMemfdInPlace),
            Some(IncubatorPlatform::FvpCca),
            Some(IncubatorPlatform::FvpCcaGuestMemfdInPlace),
            Some(IncubatorPlatform::FvpCcaRealmVfio),
        ] {
            assert_eq!(
                validate_options(true, platform, false, false).is_ok(),
                platform == Some(IncubatorPlatform::FvpCcaRealmVfio)
            );
            assert!(validate_options(false, platform, false, false).is_ok());
        }
        for (kernel, modules) in [(true, false), (false, true), (true, true)] {
            assert!(
                validate_options(
                    true,
                    Some(IncubatorPlatform::FvpCcaRealmVfio),
                    kernel,
                    modules
                )
                .is_err()
            );
        }
    }

    #[test]
    fn checked_copy_preserves_host_payload_and_records_pins() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let source = root.path().join("source");
        let content = root.path().join("content");
        fs_err::create_dir(&source).unwrap();
        fs_err::create_dir_all(content.join("aarch64")).unwrap();
        for name in ["Image", "guest-initrd.cpio"] {
            fs_err::write(source.join(name), b"openvmm").unwrap();
        }
        for name in ["Image", "initrd"] {
            fs_err::write(content.join("aarch64").join(name), b"unchanged host").unwrap();
        }
        assert!(
            resolve(&source, &content)
                .unwrap_err()
                .to_string()
                .contains("SHA-256 mismatch")
        );
        stage_pinned(&source, &content, TEST_PINS).unwrap();
        validate_staged_pinned(&content, true, TEST_PINS).unwrap();
        assert!(validate_staged_pinned(&content, false, TEST_PINS).is_err());
        for (name, staged, _) in TEST_PINS {
            assert_eq!(fs_err::read(source.join(name)).unwrap(), b"openvmm");
            assert_eq!(fs_err::read(content.join(staged)).unwrap(), b"openvmm");
        }
        for name in ["Image", "initrd"] {
            assert_eq!(
                fs_err::read(content.join("aarch64").join(name)).unwrap(),
                b"unchanged host"
            );
        }
        fs_err::write(content.join(SHARE_INPUTS[0]), b"stale").unwrap();
        assert!(validate_staged_pinned(&content, true, TEST_PINS).is_err());
        stage_pinned(&source, &content, TEST_PINS).unwrap();
        fs_err::remove_file(content.join(SHARE_INPUTS[1])).unwrap();
        assert!(validate_staged_pinned(&content, true, TEST_PINS).is_err());
        fs_err::write(source.join("guest-initrd.cpio"), b"changed").unwrap();
        assert!(stage_pinned(&source, &content, TEST_PINS).is_err());
    }

    #[test]
    fn missing_stale_and_changed_provenance_are_rejected() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        assert!(validate_staged(root.path(), false).is_ok());
        assert!(validate_staged(root.path(), true).is_err());
        let source = root.path().join("source");
        let content = root.path().join("content");
        fs_err::create_dir(&source).unwrap();
        for name in ["Image", "guest-initrd.cpio"] {
            fs_err::write(source.join(name), b"openvmm").unwrap();
        }
        stage_pinned(&source, &content, TEST_PINS).unwrap();
        fs_err::write(content.join(PROVENANCE), b"{}").unwrap();
        assert!(validate_staged_pinned(&content, true, TEST_PINS).is_err());
        stage_pinned(&source, &content, TEST_PINS).unwrap();
        fs_err::write(content.join("cca-tdisp-guest/stale"), b"old").unwrap();
        assert!(validate_staged_pinned(&content, true, TEST_PINS).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_input_and_output_symlinks_without_changing_their_targets() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let source = root.path().join("source");
        let content = root.path().join("content");
        fs_err::create_dir(&source).unwrap();
        let other = root.path().join("other");
        fs_err::write(&other, b"openvmm").unwrap();
        symlink(&other, source.join("Image")).unwrap();
        fs_err::write(source.join("guest-initrd.cpio"), b"openvmm").unwrap();
        assert!(stage_pinned(&source, &content, TEST_PINS).is_err());
        assert!(!content.exists());
        fs_err::remove_file(source.join("Image")).unwrap();
        fs_err::write(source.join("Image"), b"openvmm").unwrap();
        fs_err::create_dir_all(content.join("cca-tdisp-guest")).unwrap();
        symlink(&other, content.join(SHARE_INPUTS[0])).unwrap();
        assert!(stage_pinned(&source, &content, TEST_PINS).is_err());
        assert_eq!(fs_err::read(&other).unwrap(), b"openvmm");
        fs_err::remove_file(content.join(SHARE_INPUTS[0])).unwrap();
        fs_err::hard_link(&other, content.join(SHARE_INPUTS[0])).unwrap();
        stage_pinned(&source, &content, TEST_PINS).unwrap();
        fs_err::write(content.join(SHARE_INPUTS[0]), b"output only").unwrap();
        assert_eq!(fs_err::read(&other).unwrap(), b"openvmm");
    }
}
