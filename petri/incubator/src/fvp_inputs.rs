// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-invocation inputs for the standalone FVP command adapter.

#[cfg(target_os = "linux")]
use anyhow::Context;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) struct InputShare {
    pub(crate) path: PathBuf,
    _owner: Option<tempfile::TempDir>,
}

pub(crate) fn prepare(
    original: &Path,
    prepared: Option<&Path>,
    mapped_program: Option<&str>,
    validation_timeout: Duration,
) -> anyhow::Result<InputShare> {
    let Some(prepared) = prepared else {
        return Ok(InputShare {
            path: original.to_owned(),
            _owner: None,
        });
    };
    #[cfg(target_os = "linux")]
    {
        use petri::fvp::lifecycle::Cancellation;
        use petri::fvp::process::Deadline;
        use petri::fvp::staging::snapshot_additional_share_input;

        let deadline = Deadline::new(validation_timeout)?;
        let cancellation = Cancellation::default();
        let _signals = cancellation.install_signal_handlers()?;
        let (entries, manifest) = read_inventory(prepared)?;
        let owner = tempfile::Builder::new()
            .prefix(".incubator-fvp-inputs-")
            .tempdir_in(
                prepared
                    .parent()
                    .context("FVP prepared inputs need a parent")?,
            )?;
        for relative in &entries {
            snapshot_additional_share_input(
                prepared,
                owner.path(),
                relative,
                &deadline,
                &cancellation,
            )?;
        }
        if let Some(program) = mapped_program {
            let relative = Path::new(program)
                .strip_prefix(incubator::GUEST_SHARE_ROOT)
                .context("mapped FVP command must be under the guest share")?;
            if !entries.iter().any(|entry| entry == relative) {
                snapshot_additional_share_input(
                    original,
                    owner.path(),
                    relative,
                    &deadline,
                    &cancellation,
                )?;
            }
        }
        deadline.remaining()?;
        cancellation.check()?;
        fs_err::write(owner.path().join(INVENTORY), manifest)?;
        Ok(InputShare {
            path: owner.path().to_owned(),
            _owner: Some(owner),
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (prepared, mapped_program, validation_timeout);
        anyhow::bail!("FVP prepared input shares require a Linux host")
    }
}

#[cfg(target_os = "linux")]
const INVENTORY: &str = ".openvmm-fvp-share.json";

#[cfg(target_os = "linux")]
fn read_inventory(prepared: &Path) -> anyhow::Result<(Vec<PathBuf>, Vec<u8>)> {
    use std::collections::BTreeSet;
    use std::io::Read;

    const MAX_SIZE: u64 = 64 * 1024;
    let path = prepared.join(INVENTORY);
    let metadata = fs_err::symlink_metadata(&path).context("reading FVP input inventory")?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= MAX_SIZE,
        "FVP input inventory must be a regular file of at most 64 KiB"
    );
    let mut contents = Vec::new();
    fs_err::File::open(&path)?
        .take(MAX_SIZE + 1)
        .read_to_end(&mut contents)?;
    anyhow::ensure!(
        contents.len() as u64 <= MAX_SIZE,
        "FVP input inventory is larger than 64 KiB"
    );
    let entries: Vec<PathBuf> =
        serde_json::from_slice(&contents).context("invalid FVP input inventory")?;
    anyhow::ensure!(
        !entries.is_empty() && entries.len() <= 1024,
        "FVP input inventory must list 1..=1024 regular files"
    );
    let mut seen = BTreeSet::new();
    for entry in &entries {
        anyhow::ensure!(
            !entry.as_os_str().is_empty()
                && entry
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
                && entry != Path::new(INVENTORY)
                && !entry.starts_with("test_results"),
            "FVP input inventory must list relative input files without traversal"
        );
        anyhow::ensure!(seen.insert(entry), "duplicate FVP input inventory entry");
    }
    Ok((entries, contents))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn setup() -> anyhow::Result<tempfile::TempDir> {
        let root = tempfile::tempdir_in(".")?;
        fs_err::create_dir(root.path().join("prepared"))?;
        fs_err::create_dir(root.path().join("original"))?;
        fs_err::write(root.path().join("prepared/pipette"), "agent")?;
        fs_err::write(
            root.path().join("prepared").join(INVENTORY),
            br#"["pipette"]"#,
        )?;
        Ok(root)
    }

    #[test]
    fn mapped_command_uses_fresh_explicit_inputs_on_every_invocation() -> anyhow::Result<()> {
        let root = setup()?;
        let original = root.path().join("original");
        let prepared = root.path().join("prepared");
        fs_err::create_dir(original.join("commands"))?;
        fs_err::write(original.join("commands/run"), "runner")?;
        fs_err::write(original.join("commands/unrelated"), "not selected")?;
        let mut paths = Vec::new();
        for _ in 0..2 {
            let inputs = prepare(
                &original,
                Some(&prepared),
                Some("/share/commands/run"),
                Duration::from_secs(30),
            )?;
            assert_eq!(fs_err::read(inputs.path.join("pipette"))?, b"agent");
            assert_eq!(fs_err::read(inputs.path.join("commands/run"))?, b"runner");
            assert!(!inputs.path.join("commands/unrelated").exists());
            assert!(!prepared.join("commands").exists());
            paths.push(inputs.path.clone());
            drop(inputs);
            assert!(!paths.last().unwrap().exists());
        }
        assert_ne!(paths[0], paths[1]);
        Ok(())
    }

    #[test]
    fn guest_commands_add_no_host_files_and_listed_commands_use_prepared_copy() -> anyhow::Result<()>
    {
        let root = setup()?;
        let original = root.path().join("original");
        let prepared = root.path().join("prepared");
        let guest = prepare(&original, Some(&prepared), None, Duration::from_secs(30))?;
        assert_eq!(fs_err::read_dir(&guest.path)?.count(), 2);
        fs_err::write(prepared.join("run"), "authorized")?;
        fs_err::write(prepared.join(INVENTORY), br#"["pipette", "run"]"#)?;
        fs_err::write(original.join("run"), "not authorized")?;
        let mapped = prepare(
            &original,
            Some(&prepared),
            Some("/share/run"),
            Duration::from_secs(30),
        )?;
        assert_eq!(fs_err::read(mapped.path.join("run"))?, b"authorized");
        Ok(())
    }

    #[test]
    fn rejects_recursive_and_escaping_inputs() -> anyhow::Result<()> {
        let root = setup()?;
        let original = root.path().join("original");
        let prepared = root.path().join("prepared");
        fs_err::create_dir(prepared.join("directory"))?;
        for manifest in [
            br#"["../outside"]"#.as_slice(),
            br#"["directory"]"#.as_slice(),
            br#"["pipette", "pipette"]"#.as_slice(),
            br#"["test_results/old"]"#.as_slice(),
        ] {
            fs_err::write(prepared.join(INVENTORY), manifest)?;
            assert!(prepare(&original, Some(&prepared), None, Duration::from_secs(30)).is_err());
        }
        fs_err::write(prepared.join(INVENTORY), br#"["pipette"]"#)?;
        assert!(
            prepare(
                &original,
                Some(&prepared),
                Some("/share/missing"),
                Duration::from_secs(30),
            )
            .is_err()
        );
        Ok(())
    }
}
