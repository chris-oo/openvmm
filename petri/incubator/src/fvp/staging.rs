// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Verified, writable FVP snapshots. Neither input root is a runtime mount.
//!
//! The caller validates the official payload before passing the kernel and
//! patched initrd here. This module detects changes while making their snapshots;
//! it does not establish release provenance. The caller must stop all owned
//! processes and containers before calling [`PreparedFvpRun::cleanup`]. Dropping
//! a prepared run deliberately preserves its workspace, including live mounts.

use super::lifecycle::Cancellation;
use super::platform::PlatformManifest;
use super::platform::PlatformSources;
use super::process::Deadline;
use crate::profile::FvpCcaConfig;
use crate::profile::FvpConsole;
use anyhow::Context;
use anyhow::ensure;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Checked-in append-only runtime overlay, applied after the platform overlay.
pub const INITRD_OVERLAY: &str = include_str!("../../platforms/fvp-initrd-overlay.yaml");
/// Required JSON array declaring the source-matched share input list.
/// Unlisted content, including previous outputs and downloaded archives, is not
/// copied. The manifest is not exposed to the guest.
pub const SHARE_MANIFEST: &str = ".openvmm-fvp-share.json";

const MAX_MANIFEST_SIZE: u64 = 4 * 1024 * 1024;
const RUN_ID_FILE: &str = ".incubator-run-id";

/// Preserve completed model/guest output before removing an owned workspace.
/// The caller must first stop all writers and validate ownership of both roots.
/// Existing identical files permit retry; conflicting files are never replaced.
pub fn persist_outputs(workspace: &Path, output: &Path, deadline: &Deadline) -> anyhow::Result<()> {
    fn copy(
        source: &Path,
        target: &Path,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<()> {
        check(deadline, cancellation)?;
        let metadata = fs::symlink_metadata(source)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "FVP output contains a symlink"
        );
        if metadata.is_dir() {
            fs::create_dir_all(target)?;
            ensure!(
                fs::symlink_metadata(target)?.is_dir(),
                "FVP output destination is not a directory"
            );
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                copy(
                    &entry.path(),
                    &target.join(entry.file_name()),
                    deadline,
                    cancellation,
                )?;
            }
        } else {
            ensure!(metadata.is_file(), "FVP output must be a regular file");
            if target.try_exists()? {
                ensure!(
                    digest(&mut open_regular(source)?, deadline, cancellation)?
                        == digest(&mut open_regular(target)?, deadline, cancellation)?,
                    "FVP output conflicts with an existing file"
                );
            } else {
                copy_verified(source, target, None, deadline, cancellation)?;
            }
        }
        check(deadline, cancellation)
    }
    let cancellation = Cancellation::default();
    for (source, target) in [
        (workspace.join("logs"), output.join("consoles")),
        (workspace.join("config"), output.join("configuration")),
        (workspace.join("package"), output.join("package")),
        (
            workspace.join("share/test_results"),
            output.join("test_results"),
        ),
        (workspace.join("share/cca-logs"), output.join("cca-logs")),
    ] {
        match fs::symlink_metadata(&source) {
            Ok(_) => copy(&source, &target, deadline, &cancellation)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect FVP output"),
        }
    }
    check(deadline, &cancellation)
}

/// An owned snapshot whose lifetime is independent of process supervision.
#[derive(Debug)]
pub struct PreparedFvpRun {
    root: PathBuf,
    cleaned: bool,
    allocation: super::lifecycle::WorkspaceAllocation,
}

impl PreparedFvpRun {
    /// Snapshot validated sources before any process uses the workspace.
    ///
    /// `share` is a prepared input root, not a repository checkout.
    /// [`SHARE_MANIFEST`] must provide an explicit list of files to copy.
    /// Paths may contain shell metacharacters; Rust copies them
    /// without evaluating shell text.
    pub fn new(
        platform: &PlatformSources,
        kernel: &Path,
        patched_initrd: &Path,
        share: &Path,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Self> {
        Self::new_with_inputs(
            platform,
            kernel,
            patched_initrd,
            share,
            &[],
            deadline,
            cancellation,
        )
    }

    /// Snapshot the share inventory plus explicit runtime-selected guest files.
    ///
    /// `additional_inputs` contains relative regular-file paths under `share`, such as
    /// the source-matched nextest runner. The same containment checks apply to
    /// manifest entries and additional entries. No directory is copied wholesale.
    /// A fresh writable `share/test_results` directory holds guest output;
    /// inventory entries cannot import previous results into that directory.
    /// The workspace uses a generated shell-safe path outside the persistent
    /// lock/state directory and does not inherit a user-controlled TMPDIR.
    pub fn new_with_inputs(
        platform: &PlatformSources,
        kernel: &Path,
        patched_initrd: &Path,
        share: &Path,
        additional_inputs: &[PathBuf],
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Self> {
        Self::prepare_in(
            Path::new("/tmp"),
            platform,
            (kernel, patched_initrd),
            share,
            additional_inputs,
            deadline,
            cancellation,
        )
    }

    /// Snapshot inputs under an explicit existing, canonical shell-safe parent.
    ///
    /// `payload` contains the kernel and patched initrd paths, respectively.
    /// The parent must be outside the input roots and the persistent lock/state
    /// directory. The caller retains the same explicit cleanup responsibility.
    fn prepare_in(
        parent: &Path,
        platform: &PlatformSources,
        payload: (&Path, &Path),
        share: &Path,
        additional: &[PathBuf],
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Self> {
        let prepared = Self::allocate_checked(parent, platform, share, deadline, cancellation)?;
        match prepared.populate_inputs(platform, payload, share, additional, deadline, cancellation)
        {
            Ok(()) => Ok(prepared),
            Err(error) => {
                prepared
                    .cleanup()
                    .context("failed to remove unused FVP staging")?;
                Err(error)
            }
        }
    }

    /// Allocate empty scaffolding for registration with the lifecycle journal.
    pub(crate) fn allocate_for_session(
        platform: &PlatformSources,
        share: &Path,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Self> {
        Self::allocate_checked(Path::new("/tmp"), platform, share, deadline, cancellation)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "resolve staging parent and share before checking input-root containment"
    )]
    fn allocate_checked(
        parent: &Path,
        platform: &PlatformSources,
        share: &Path,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Self> {
        check(deadline, cancellation)?;
        let parent = fs::canonicalize(parent).context("cannot resolve FVP staging parent")?;
        let share_root = fs::canonicalize(share).context("cannot resolve prepared FVP share")?;
        for input_root in [&platform.platform_root, &platform.package_root, &share_root] {
            ensure!(
                !parent.starts_with(input_root),
                "FVP staging parent overlaps an input root"
            );
        }
        check(deadline, cancellation)?;
        Self::allocate(&parent, deadline)
    }

    /// Fill a registered workspace. Errors leave cleanup to its session.
    pub(crate) fn populate_inputs(
        &self,
        platform: &PlatformSources,
        payload: (&Path, &Path),
        share: &Path,
        additional: &[PathBuf],
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<()> {
        check(deadline, cancellation)?;
        platform.revalidate_sources(deadline)?;
        for (source, relative) in platform.sources().into_iter().zip([
            "package/cca-3world.yaml",
            "config/kvm_cca_planes.yaml",
            "inputs/bl1.bin",
            "inputs/fip.bin",
            "inputs/dt_bootargs.dtb",
        ]) {
            check(deadline, cancellation)?;
            source.revalidate(deadline)?;
            copy_verified(
                source.path(),
                &self.root.join(relative),
                Some(source.sha256()),
                deadline,
                cancellation,
            )?;
            source.verify_snapshot(&self.root, Path::new(relative), deadline)?;
        }
        for (source, relative) in [(payload.0, "inputs/Image"), (payload.1, "inputs/initrd")] {
            copy_verified(
                source,
                &self.root.join(relative),
                None,
                deadline,
                cancellation,
            )?;
        }
        snapshot_share_with_inputs(share, &self.share(), additional, deadline, cancellation)?;
        fs::write(self.root.join("config/openvmm-initrd.yaml"), INITRD_OVERLAY)?;
        check(deadline, cancellation)?;
        Ok(())
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the workspace must have a canonical shell-safe parent"
    )]
    pub(super) fn allocate(parent: &Path, deadline: &Deadline) -> anyhow::Result<Self> {
        let parent = fs::canonicalize(parent).context("cannot resolve FVP staging parent")?;
        ensure!(safe_path(&parent), "FVP staging parent is not shell-safe");
        let directory = tempfile::Builder::new()
            .prefix("openvmm-fvp-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)
            .context("cannot create owned FVP workspace")?;
        ensure!(
            safe_path(directory.path()),
            "FVP workspace is not shell-safe"
        );
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        for relative in ["build", "package", "config", "inputs", "share", "logs"] {
            fs::create_dir(directory.path().join(relative))?;
        }
        let allocation =
            super::lifecycle::WorkspaceAllocation::capture(directory.path(), deadline)?;
        // From here onward, no automatic TempDir removal can race live mounts.
        Ok(Self {
            root: directory.keep(),
            cleaned: false,
            allocation,
        })
    }

    /// Canonical safe path to the owned per-run workspace.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical safe workspace path for owned-process state and diagnostics.
    pub fn workspace_path(&self) -> &Path {
        self.root()
    }

    /// Owned 9P share. Guest output remains here until the caller saves it.
    pub fn share(&self) -> PathBuf {
        self.root.join("share")
    }

    /// Owned 9P share directory, including the per-session endpoint identity.
    pub fn share_dir(&self) -> PathBuf {
        self.share()
    }

    /// Durable per-run logs. Save these before explicit workspace cleanup.
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Log directory to save after process cleanup and before workspace removal.
    pub fn logs_dir(&self) -> PathBuf {
        self.logs()
    }

    /// Bind the guest share to the session before starting any owned process.
    ///
    /// The runtime reads `/share/.incubator-run-id` over pipette and compares the
    /// exact bytes with its session ID before accepting readiness. Existing files
    /// fail closed, including identities supplied by the share manifest.
    pub fn set_run_identity(&self, run_id: &str) -> anyhow::Result<()> {
        ensure!(
            run_id.len() == 64
                && run_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid FVP run identity"
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(self.share().join(RUN_ID_FILE))
            .context("cannot create owned FVP run identity")?;
        file.write_all(run_id.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// Named UART log for one launch attempt, never reused after a collision.
    pub fn console_log(&self, console: FvpConsole, attempt: u32) -> PathBuf {
        self.logs()
            .join(format!("attempt-{attempt}"))
            .join(format!("{}.log", console.name()))
    }

    /// Add only controlled paths to an isolated Shrinkwrap command.
    ///
    /// The caller creates `command` with the validated platform's isolated
    /// interpreter builder and applies its pinned Docker binding and ownership
    /// labels. This method neither starts a process nor depends on a session.
    /// Call again with a fresh command for a port-collision retry.
    pub fn populate_command(
        &self,
        command: &mut Command,
        port: u16,
        config: &FvpCcaConfig,
        attempt: u32,
    ) -> anyhow::Result<()> {
        config.validate()?;
        ensure!(port != 0, "FVP pipette forwarding port must be nonzero");
        ensure!(attempt != 0, "FVP launch attempt must be nonzero");
        fs::create_dir(self.logs().join(format!("attempt-{attempt}")))
            .context("cannot create fresh FVP attempt logs")?;
        let generated = self
            .root
            .join(format!("config/generated-run-{attempt}.yaml"));
        let params = BTreeMap::from([(
            "-C bp.hostbridge.userNetPorts".to_owned(),
            format!("127.0.0.1:{port}={}", pipette_client::PIPETTE_PORT),
        )]);
        let mut terminals = BTreeMap::new();
        for console in [
            FvpConsole::Host,
            FvpConsole::Edk2,
            FvpConsole::Secure,
            FvpConsole::Rmm,
        ] {
            terminals.insert(
                console.terminal(),
                serde_json::json!({
                    "type": if config.consoles.contains(&console) { "stdout" } else { "null" },
                    "friendly": console.name(),
                    "logfile": self.console_log(console, attempt),
                }),
            );
        }
        fs::write(
            &generated,
            serde_yaml::to_string(&serde_json::json!({
                "run": { "params": params, "terminals": terminals }
            }))?,
        )
        .context("failed to write generated FVP overlay")?;
        command
            .current_dir(&self.root)
            .env("SHRINKWRAP_BUILD", self.root.join("build"))
            .env("SHRINKWRAP_PACKAGE", self.root.join("package"))
            .env("SHRINKWRAP_CONFIG", self.root.join("config"))
            .env("TMPDIR", self.root.join("build"))
            .args([
                "--runtime=docker",
                &format!(
                    "--image={}",
                    PlatformManifest::pinned()?.shrinkwrap.container.digest
                ),
                "run",
                "--no-color",
            ]);
        for overlay in [
            self.root.join("config/kvm_cca_planes.yaml"),
            self.root.join("config/openvmm-initrd.yaml"),
            generated,
        ] {
            command.arg("--overlay").arg(overlay);
        }
        command.arg(self.root.join("package/cca-3world.yaml"));
        for (name, relative) in [
            ("BL1", "inputs/bl1.bin"),
            ("FIP", "inputs/fip.bin"),
            ("DTB", "inputs/dt_bootargs.dtb"),
            ("KERNEL", "inputs/Image"),
            ("INITRD", "inputs/initrd"),
            ("SHARE", "share"),
        ] {
            command
                .arg("--rtvar")
                .arg(format!("{name}={}", self.root.join(relative).display()));
        }
        command.args(["--rtvar", "CMDLINE="]);
        Ok(())
    }

    /// Remove snapshots only after the caller has proved owned-process cleanup.
    ///
    /// Save diagnostics and guest output first. On failure this object preserves
    /// the remaining workspace and warns; the caller must retain its state.
    pub fn cleanup(mut self) -> anyhow::Result<()> {
        fs::remove_dir_all(&self.root).context("failed to remove owned FVP workspace")?;
        self.cleaned = true;
        Ok(())
    }

    /// Check the original allocation before handing its path to registration.
    pub(crate) fn verify_unregistered(&self, deadline: &Deadline) -> anyhow::Result<()> {
        self.allocation.verify(deadline)
    }

    /// Discard only the original empty scaffold, never replacement data.
    pub(crate) fn cleanup_unregistered(mut self, deadline: &Deadline) -> anyhow::Result<()> {
        self.allocation.discard(deadline)?;
        self.cleaned = true;
        Ok(())
    }

    /// Acknowledge lifecycle retirement without performing a second deletion.
    pub(crate) fn confirm_retired(mut self) -> anyhow::Result<()> {
        match fs::symlink_metadata(&self.root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.cleaned = true;
                Ok(())
            }
            Err(error) => Err(error).context("cannot confirm FVP workspace retirement"),
            Ok(_) => anyhow::bail!("FVP session did not retire its workspace"),
        }
    }
}

impl Drop for PreparedFvpRun {
    fn drop(&mut self) {
        if !self.cleaned {
            tracing::warn!(
                workspace = %self.root.display(),
                "preserving FVP workspace: owned-process cleanup was not confirmed"
            );
        }
    }
}

fn safe_path(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some_and(|text| {
            text.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
        })
}

fn check(deadline: &Deadline, cancellation: &Cancellation) -> anyhow::Result<()> {
    cancellation.check()?;
    deadline.remaining()?;
    Ok(())
}

fn open_regular(path: &Path) -> anyhow::Result<File> {
    // Nonblocking prevents a file replaced by a FIFO from hanging the caller.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .context("cannot open regular FVP snapshot input")?;
    ensure!(
        file.metadata()?.is_file(),
        "FVP snapshot input is not a regular file"
    );
    Ok(file)
}

fn digest(
    file: &mut File,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<String> {
    check(deadline, cancellation)?;
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        check(deadline, cancellation)?;
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    check(deadline, cancellation)?;
    Ok(hex::encode(digest.finalize()))
}

fn copy_verified(
    source: &Path,
    destination: &Path,
    expected: Option<&str>,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<()> {
    copy_verified_with_progress(source, destination, expected, deadline, cancellation, || {})
}

fn copy_verified_with_progress(
    source: &Path,
    destination: &Path,
    expected: Option<&str>,
    deadline: &Deadline,
    cancellation: &Cancellation,
    mut progress: impl FnMut(),
) -> anyhow::Result<()> {
    check(deadline, cancellation)?;
    let mut input = open_regular(source)?;
    let before = digest(&mut input, deadline, cancellation)?;
    if let Some(expected) = expected {
        ensure!(before == expected, "FVP source changed before snapshot");
    }
    input.seek(SeekFrom::Start(0))?;
    let mut output = tempfile::Builder::new().prefix(".fvp-copy-").tempfile_in(
        destination
            .parent()
            .context("FVP snapshot destination has no parent")?,
    )?;
    let mut buffer = [0; 64 * 1024];
    loop {
        check(deadline, cancellation)?;
        let length = input.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        output.write_all(&buffer[..length])?;
        progress();
    }
    output
        .as_file()
        .set_permissions(fs::Permissions::from_mode(
            input.metadata()?.permissions().mode() & 0o777,
        ))?;
    output.as_file().sync_all()?;
    ensure!(
        digest(&mut open_regular(output.path())?, deadline, cancellation)? == before,
        "FVP snapshot changed while copying"
    );
    ensure!(
        digest(&mut open_regular(source)?, deadline, cancellation)? == before,
        "FVP source changed while copying"
    );
    check(deadline, cancellation)?;
    output
        .persist_noclobber(destination)
        .map_err(|error| error.error)
        .context("cannot publish verified FVP snapshot without replacing existing data")?;
    check(deadline, cancellation)?;
    Ok(())
}

fn share_file(root: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "FVP share inventory must contain relative paths without traversal"
    );
    let mut path = root.to_owned();
    for part in relative.components() {
        path.push(part);
        ensure!(
            !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "FVP share inventory contains a symlink"
        );
    }
    ensure!(
        path.is_file(),
        "FVP share inventory entry is not a regular file"
    );
    Ok(path)
}

#[cfg(test)]
fn snapshot_share(
    source: &Path,
    destination: &Path,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<()> {
    snapshot_share_with_inputs(source, destination, &[], deadline, cancellation)
}

#[expect(
    clippy::disallowed_methods,
    reason = "resolve the input root before checking share path containment"
)]
fn snapshot_share_with_inputs(
    source: &Path,
    destination: &Path,
    additional: &[PathBuf],
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<()> {
    let root = fs::canonicalize(source).context("cannot resolve prepared FVP share")?;
    ensure!(root.is_dir(), "prepared FVP share must be a directory");
    check(deadline, cancellation)?;
    let manifest_path = root.join(SHARE_MANIFEST);
    let mut text = Vec::new();
    open_regular(&manifest_path)
        .context("required FVP share inventory is missing or invalid")?
        .take(MAX_MANIFEST_SIZE + 1)
        .read_to_end(&mut text)?;
    ensure!(
        text.len() as u64 <= MAX_MANIFEST_SIZE,
        "FVP share inventory is too large"
    );
    let mut entries: Vec<PathBuf> =
        serde_json::from_slice(&text).context("invalid FVP share inventory")?;
    let manifest_digest = hex::encode(Sha256::digest(&text));
    ensure!(!entries.is_empty(), "FVP share inventory is empty");
    let mut seen = BTreeSet::new();
    for relative in &entries {
        ensure!(
            seen.insert(relative.clone()),
            "duplicate FVP share inventory entry"
        );
    }
    for relative in additional {
        if seen.insert(relative.clone()) {
            entries.push(relative.clone());
        }
    }
    for relative in entries {
        check(deadline, cancellation)?;
        ensure!(
            relative != Path::new(SHARE_MANIFEST),
            "FVP share inventory cannot include itself"
        );
        ensure!(
            !relative.starts_with("test_results"),
            "FVP share inputs cannot include the guest output directory"
        );
        let input = share_file(&root, &relative)?;
        let output = destination.join(&relative);
        fs::create_dir_all(output.parent().context("invalid FVP share path")?)?;
        copy_verified(&input, &output, None, deadline, cancellation)?;
        ensure!(
            share_file(&root, &relative)? == input,
            "FVP share path changed while copying"
        );
    }
    ensure!(
        digest(&mut open_regular(&manifest_path)?, deadline, cancellation)? == manifest_digest,
        "FVP share inventory changed while copying"
    );
    let results = destination.join("test_results");
    fs::create_dir(&results).context("cannot create fresh FVP guest output directory")?;
    fs::set_permissions(&results, fs::Permissions::from_mode(0o700))
        .context("cannot make FVP guest output directory writable")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::process::run_command_cancellable;
    use super::*;
    use serde_yaml::Value;
    use std::time::Duration;
    use test_with_tracing::test;

    #[test]
    fn interrupted_copy_leaves_no_final_file_and_can_retry() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        fs::write(&source, vec![7u8; 128 * 1024]).unwrap();
        let cancellation = Cancellation::default();
        assert!(
            copy_verified_with_progress(&source, &target, None, &deadline(), &cancellation, || {
                cancellation.cancel()
            },)
            .is_err()
        );
        assert!(!target.exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        copy_verified(
            &source,
            &target,
            None,
            &deadline(),
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(fs::read(&source).unwrap(), fs::read(&target).unwrap());
    }

    #[test]
    fn output_collection_obeys_deadlines_and_rejects_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let output = directory.path().join("output");
        fs::create_dir_all(workspace.join("logs")).unwrap();
        fs::write(workspace.join("logs/host.log"), b"complete").unwrap();
        assert!(
            persist_outputs(&workspace, &output, &Deadline::new(Duration::ZERO).unwrap()).is_err()
        );
        assert!(!output.exists());
        persist_outputs(&workspace, &output, &deadline()).unwrap();
        persist_outputs(&workspace, &output, &deadline()).unwrap();
        assert_eq!(
            fs::read(output.join("consoles/host.log")).unwrap(),
            b"complete"
        );
        // A dangling output root must not be silently treated as absent.
        fs::create_dir(workspace.join("share")).unwrap();
        std::os::unix::fs::symlink(
            directory.path().join("absent"),
            workspace.join("share/test_results"),
        )
        .unwrap();
        assert!(persist_outputs(&workspace, &output, &deadline()).is_err());
    }

    const PACKAGED: &str = include_str!("../../platforms/fvp-packaged-run.fixture.yaml");
    const EFFECTIVE: &str = include_str!("../../platforms/fvp-effective-run.fixture.yaml");

    fn directory() -> tempfile::TempDir {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fvp-staging-tests");
        fs::create_dir_all(&parent).unwrap();
        tempfile::Builder::new()
            .prefix("test-")
            .tempdir_in(parent)
            .unwrap()
    }

    fn deadline() -> Deadline {
        Deadline::new(Duration::from_secs(15)).unwrap()
    }

    fn merge(base: &mut Value, overlay: Value) {
        match (base, overlay) {
            (Value::Mapping(base), Value::Mapping(overlay)) => {
                for (key, value) in overlay {
                    match base.get_mut(&key) {
                        Some(existing) => merge(existing, value),
                        None => {
                            base.insert(key, value);
                        }
                    }
                }
            }
            (Value::Sequence(base), Value::Sequence(overlay)) => base.extend(overlay),
            (base, overlay) => *base = overlay,
        }
    }

    fn effective() -> Value {
        let mut base = serde_yaml::from_str(PACKAGED).unwrap();
        merge(&mut base, serde_yaml::from_str(INITRD_OVERLAY).unwrap());
        assert_eq!(base, serde_yaml::from_str::<Value>(EFFECTIVE).unwrap());
        base
    }

    fn render(value: &str, variables: &BTreeMap<&str, String>) -> String {
        let mut output = value.to_owned();
        for (key, value) in variables {
            output = output.replace(&format!("${{rtvar:{key}}}"), value);
        }
        assert!(!output.contains("${rtvar:"));
        output.replace("$$", "$")
    }

    #[test]
    fn rendered_prerun_preserves_setup_order_and_cleanup() {
        let directory = directory();
        let prepared = PreparedFvpRun::allocate(directory.path(), &deadline()).unwrap();
        let input = directory
            .path()
            .join("input ' \" $HOME $(touch EVALUATED) ; `false`");
        fs::create_dir(&input).unwrap();
        let mut variables = BTreeMap::from([
            ("CMDLINE", String::new()),
            ("ROOTFS", "legacy-image-must-not-be-used".to_owned()),
        ]);
        for (name, filename, contents) in [
            ("KERNEL", "Image", "kernel"),
            ("DTB", "fdt.dtb", "device tree"),
            ("INITRD", "initrd", "patched archive"),
        ] {
            let source = input.join(format!("{filename} ' $()"));
            fs::write(&source, contents).unwrap();
            let target = prepared.root.join("inputs").join(filename);
            copy_verified(
                &source,
                &target,
                None,
                &deadline(),
                &Cancellation::default(),
            )
            .unwrap();
            assert!(safe_path(&target));
            variables.insert(name, target.display().to_string());
        }
        let effective = effective();
        assert_eq!(
            render(
                effective["run"]["params"]["-C bp.virtioblockdevice.image_path"]
                    .as_str()
                    .unwrap(),
                &variables,
            ),
            ""
        );
        assert_eq!(effective["run"]["rtvars"]["ROOTFS"]["type"], "string");
        let prerun = effective["run"]["prerun"].as_sequence().unwrap();
        let rendered = prerun
            .iter()
            .map(|value| render(value.as_str().unwrap(), &variables))
            .collect::<Vec<_>>()
            .join("\n");
        let semihost = prepared.root.join("fake-semihost");
        let capture = prepared.root.join("startup-captured");
        let trace = prepared.root.join("copies");
        // Fake only directory allocation. The recorded package's setup, copies,
        // and EXIT trap execute unchanged, without using the host temp directory.
        let script = format!(
            r#"set -euo pipefail
function mktemp {{ mkdir "$FAKE_SEMIHOST"; printf '%s\n' "$FAKE_SEMIHOST"; }}
function cp {{ printf '%s\n' "$2" >> "$TRACE"; command cp "$@"; }}
{rendered}
test "$(cat "$SEMIHOSTDIR/Image")" = kernel
test "$(cat "$SEMIHOSTDIR/fdt.dtb")" = "device tree"
test "$(cat "$SEMIHOSTDIR/initrd")" = "patched archive"
command cp "$SEMIHOSTDIR/startup.nsh" "$CAPTURE"
"#
        );
        let output = run_command_cancellable(
            Command::new("bash")
                .args(["-c", &script])
                .current_dir(&prepared.root)
                .env("FAKE_SEMIHOST", &semihost)
                .env("CAPTURE", &capture)
                .env("TRACE", &trace),
            &deadline(),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !semihost.exists(),
            "inherited EXIT trap must remove semihost files"
        );
        assert!(!prepared.root.join("EVALUATED").exists());
        let copies = fs::read_to_string(&trace).unwrap();
        assert_eq!(
            copies
                .lines()
                .map(|path| Path::new(path).file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["Image", "fdt.dtb", "initrd"],
        );
        let startup = fs::read_to_string(&capture).unwrap();
        assert_eq!(
            startup,
            "Image dtb=fdt.dtb initrd=initrd rdinit=/cca-init.sh console=ttyAMA0 earlycon=pl011,0x1c090000 ip=off\n"
        );
        for argument in ["dtb=", "initrd=", "rdinit=", "console=", "earlycon=", "ip="] {
            assert_eq!(startup.matches(argument).count(), 1, "{argument}");
        }
        assert!(!startup.contains("root=/dev/"));

        fs::remove_file(&variables["INITRD"]).unwrap();
        let output = run_command_cancellable(
            Command::new("bash")
                .args(["-c", &script])
                .current_dir(&prepared.root)
                .env("FAKE_SEMIHOST", &semihost)
                .env("CAPTURE", &capture)
                .env("TRACE", &trace),
            &deadline(),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(!output.status.success());
        assert!(
            !semihost.exists(),
            "failed initrd lookup must run inherited cleanup"
        );
        prepared.cleanup().unwrap();
    }

    #[test]
    fn share_inventory_copies_only_explicit_regular_files() {
        let directory = directory();
        let source = directory.path().join("source $() '");
        let target = directory.path().join("snapshot");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::create_dir(source.join("bin")).unwrap();
        fs::write(source.join("bin/pipette"), "source matched").unwrap();
        for unwanted in ["output.log", "download.tar.gz", "nextest-cache"] {
            fs::write(source.join(unwanted), "not an input").unwrap();
        }
        fs::write(source.join(SHARE_MANIFEST), r#"["bin/pipette"]"#).unwrap();
        snapshot_share(&source, &target, &deadline(), &Cancellation::default()).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("bin/pipette")).unwrap(),
            "source matched"
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 2);
        fs::write(source.join("bin/pipette"), "changed live source").unwrap();
        assert_eq!(
            fs::read_to_string(target.join("bin/pipette")).unwrap(),
            "source matched"
        );
    }

    #[test]
    fn share_rejects_traversal_symlinks_missing_and_nonregular_files() {
        let directory = directory();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(directory.path().join("foreign"), "foreign").unwrap();
        std::os::unix::fs::symlink(directory.path().join("foreign"), source.join("escape"))
            .unwrap();
        fs::create_dir(source.join("folder")).unwrap();
        for entry in ["../foreign", "/foreign", "escape", "missing", "folder"] {
            let output = tempfile::Builder::new()
                .tempdir_in(directory.path())
                .unwrap();
            fs::write(
                source.join(SHARE_MANIFEST),
                serde_json::to_vec(&[entry]).unwrap(),
            )
            .unwrap();
            assert!(
                snapshot_share(
                    &source,
                    output.path(),
                    &deadline(),
                    &Cancellation::default()
                )
                .is_err(),
                "{entry}"
            );
        }
        assert_eq!(
            fs::read_to_string(directory.path().join("foreign")).unwrap(),
            "foreign"
        );
    }

    #[test]
    fn additional_share_inputs_are_explicit_checked_and_deduplicated() {
        let directory = directory();
        let source = directory.path().join("source");
        let target = directory.path().join("snapshot");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        for name in ["pipette", "runner", "unlisted"] {
            fs::write(source.join(name), name).unwrap();
        }
        fs::write(source.join(SHARE_MANIFEST), r#"["pipette"]"#).unwrap();
        snapshot_share_with_inputs(
            &source,
            &target,
            &["pipette".into(), "runner".into(), "runner".into()],
            &deadline(),
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(target.join("runner")).unwrap(), "runner");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 3);
        assert!(!target.join("unlisted").exists());
        let invalid = directory.path().join("invalid");
        fs::create_dir(&invalid).unwrap();
        assert!(
            snapshot_share_with_inputs(
                &source,
                &invalid,
                &["../foreign".into()],
                &deadline(),
                &Cancellation::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn flowey_share_requires_manifest_and_adds_only_invoked_binary() {
        let directory = directory();
        let source = directory.path().join("source");
        let target = directory.path().join("snapshot");
        fs::create_dir(&target).unwrap();
        let runner = PathBuf::from("nextest-archive-tmp/target/tests");
        let files = ["pipette", "openvmm", "aarch64/Image", "aarch64/initrd"];
        for relative in files.iter().map(PathBuf::from).chain([runner.clone()]) {
            let path = source.join(&relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, relative.as_os_str().as_encoded_bytes()).unwrap();
        }
        for relative in [
            "download.tar.gz",
            "nextest-archive-tmp/cache",
            "test_results/previous.log",
        ] {
            let path = source.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "unlisted").unwrap();
        }
        assert!(
            snapshot_share_with_inputs(
                &source,
                &target,
                std::slice::from_ref(&runner),
                &deadline(),
                &Cancellation::default(),
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        fs::write(
            source.join(SHARE_MANIFEST),
            serde_json::to_vec(&files).unwrap(),
        )
        .unwrap();
        snapshot_share_with_inputs(
            &source,
            &target,
            std::slice::from_ref(&runner),
            &deadline(),
            &Cancellation::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(target.join(&runner)).unwrap(),
            runner.to_str().unwrap()
        );
        for relative in files {
            assert!(target.join(relative).is_file());
        }
        assert!(!target.join("download.tar.gz").exists());
        assert!(!target.join("nextest-archive-tmp/cache").exists());
        let results = target.join("test_results");
        assert!(results.is_dir());
        assert_eq!(
            results.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::read_dir(&results).unwrap().count(), 0);
        fs::write(results.join("new.log"), "guest output").unwrap();
        assert!(!source.join("test_results/new.log").exists());
        assert_eq!(
            fs::read_to_string(source.join("test_results/previous.log")).unwrap(),
            "unlisted"
        );
        assert!(!target.join(SHARE_MANIFEST).exists());
        let invalid = directory.path().join("invalid");
        fs::create_dir(&invalid).unwrap();
        assert!(
            snapshot_share_with_inputs(
                &source,
                &invalid,
                &["test_results/previous.log".into()],
                &deadline(),
                &Cancellation::default(),
            )
            .is_err()
        );
        assert!(!invalid.join("test_results").exists());
    }

    #[test]
    fn snapshot_rejects_changed_identity_and_preserves_executable_mode() {
        let directory = directory();
        let input = directory.path().join("input");
        fs::write(&input, "source").unwrap();
        fs::set_permissions(&input, fs::Permissions::from_mode(0o755)).unwrap();
        let target = directory.path().join("snapshot");
        assert!(
            copy_verified(
                &input,
                &target,
                Some(&"0".repeat(64)),
                &deadline(),
                &Cancellation::default()
            )
            .is_err()
        );
        assert!(!target.exists());
        copy_verified(&input, &target, None, &deadline(), &Cancellation::default()).unwrap();
        assert_eq!(
            target.metadata().unwrap().permissions().mode() & 0o777,
            0o755
        );
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&input, &link).unwrap();
        assert!(
            copy_verified(
                &link,
                &directory.path().join("bad"),
                None,
                &deadline(),
                &Cancellation::default()
            )
            .is_err()
        );
    }

    #[test]
    fn snapshot_failure_checks_are_bounded_and_do_not_create_outputs() {
        let directory = directory();
        let input = directory.path().join("input");
        fs::write(&input, "source").unwrap();
        let output = directory.path().join("output");
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert!(copy_verified(&input, &output, None, &deadline(), &cancellation).is_err());
        assert!(!output.exists());
        for invalid in [
            directory.path().join("missing"),
            directory.path().to_path_buf(),
        ] {
            assert!(
                copy_verified(
                    &invalid,
                    &output,
                    None,
                    &deadline(),
                    &Cancellation::default()
                )
                .is_err()
            );
            assert!(!output.exists());
        }
        let fifo = directory.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(
            copy_verified(&fifo, &output, None, &deadline(), &Cancellation::default()).is_err()
        );
        assert!(!output.exists());
        assert_eq!(fs::read_to_string(input).unwrap(), "source");
    }

    #[test]
    fn workspace_drop_preserves_until_explicit_cleanup() {
        let directory = directory();
        let prepared = PreparedFvpRun::allocate(directory.path(), &deadline()).unwrap();
        let preserved = prepared.root.clone();
        fs::write(prepared.share().join("guest-output"), "keep").unwrap();
        drop(prepared);
        assert_eq!(
            fs::read_to_string(preserved.join("share/guest-output")).unwrap(),
            "keep"
        );
        fs::remove_dir_all(&preserved).unwrap();
        assert!(!preserved.exists());
    }

    #[test]
    fn run_identity_is_exact_owned_and_write_once() {
        let directory = directory();
        let prepared = PreparedFvpRun::allocate(directory.path(), &deadline()).unwrap();
        assert_eq!(prepared.workspace_path(), prepared.root());
        assert!(safe_path(prepared.root()));
        assert_eq!(
            prepared.root().metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(prepared.share_dir(), prepared.share());
        assert_eq!(prepared.logs_dir(), prepared.logs());
        let path = prepared.share_dir().join(RUN_ID_FILE);
        for invalid in ["", "short", &"A".repeat(64), &"../".repeat(22)] {
            assert!(prepared.set_run_identity(invalid).is_err());
            assert!(!path.exists());
        }
        let run_id = super::super::lifecycle::RunId::new().unwrap();
        prepared.set_run_identity(run_id.as_str()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), run_id.as_str());
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o444);
        assert!(prepared.set_run_identity(run_id.as_str()).is_err());
        assert!(prepared.set_run_identity(&"f".repeat(64)).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), run_id.as_str());
        prepared.cleanup().unwrap();
    }

    #[test]
    fn command_uses_only_owned_paths_and_generated_overlay_last() {
        let directory = directory();
        let prepared = PreparedFvpRun::allocate(directory.path(), &deadline()).unwrap();
        let config = FvpCcaConfig {
            consoles: vec![FvpConsole::Host, FvpConsole::Rmm],
            primary_console: FvpConsole::Host,
            capabilities: vec!["cca".into()],
            deadlines: Default::default(),
            port_retries: 20,
        };
        let mut command = Command::new("isolated-python");
        command.args(["-I", "-B", "-u", "/trusted/shrinkwrap"]);
        // Poisoned ambient/default workspace values must be replaced, not used.
        command.env("SHRINKWRAP_PACKAGE", "/not-the-approved-package");
        prepared
            .populate_command(&mut command, 12345, &config, 1)
            .unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(command.get_program(), "isolated-python");
        assert_eq!(&args[..4], ["-I", "-B", "-u", "/trusted/shrinkwrap"]);
        let overlays = args
            .windows(2)
            .filter(|pair| pair[0] == "--overlay")
            .map(|pair| pair[1])
            .collect::<Vec<_>>();
        assert_eq!(overlays.len(), 3);
        assert!(overlays[2].ends_with("/config/generated-run-1.yaml"));
        assert!(
            args.contains(
                &prepared
                    .root
                    .join("package/cca-3world.yaml")
                    .to_str()
                    .unwrap()
            )
        );
        assert_eq!(
            args.windows(2)
                .filter(|pair| pair[0] == "--rtvar")
                .map(|pair| pair[1].split_once('=').unwrap().0)
                .collect::<Vec<_>>(),
            ["BL1", "FIP", "DTB", "KERNEL", "INITRD", "SHARE", "CMDLINE"]
        );
        for pair in args.windows(2).filter(|pair| pair[0] == "--rtvar") {
            let (name, path) = pair[1].split_once('=').unwrap();
            assert_ne!(name, "ROOTFS");
            if name != "CMDLINE" {
                assert!(safe_path(Path::new(path)));
                assert!(Path::new(path).starts_with(&prepared.root));
            } else {
                assert!(path.is_empty());
            }
        }
        for (name, value) in command.get_envs() {
            if name.to_string_lossy().starts_with("SHRINKWRAP_") {
                assert!(Path::new(value.unwrap()).starts_with(&prepared.root));
            }
        }
        let generated: Value = serde_yaml::from_str(
            &fs::read_to_string(prepared.root.join("config/generated-run-1.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            generated["run"]["params"]["-C bp.hostbridge.userNetPorts"],
            format!("127.0.0.1:12345={}", pipette_client::PIPETTE_PORT)
        );
        assert_eq!(
            generated["run"]["terminals"]["bp.terminal_0"]["type"],
            "stdout"
        );
        assert_eq!(
            generated["run"]["terminals"]["bp.terminal_3"]["friendly"],
            "rmm"
        );
        prepared.cleanup().unwrap();
    }

    #[test]
    fn retries_keep_prior_logs_out_of_current_readiness() {
        let directory = directory();
        let prepared = PreparedFvpRun::allocate(directory.path(), &deadline()).unwrap();
        let config = FvpCcaConfig {
            consoles: vec![FvpConsole::Host],
            primary_console: FvpConsole::Host,
            capabilities: vec!["cca".into()],
            deadlines: Default::default(),
            port_retries: 20,
        };
        prepared
            .populate_command(&mut Command::new("python"), 12345, &config, 1)
            .unwrap();
        let first = prepared.console_log(FvpConsole::Host, 1);
        let stale = "Failed to load initrd\nINCUBATOR DHCP START\nPIPETTE READY\n";
        fs::write(&first, stale).unwrap();
        prepared
            .populate_command(&mut Command::new("python"), 12346, &config, 2)
            .unwrap();
        let second = prepared.console_log(FvpConsole::Host, 2);
        assert_ne!(first, second);
        assert!(!second.exists());
        assert_eq!(fs::read_to_string(&first).unwrap(), stale);
        for attempt in [1, 2] {
            let generated: Value = serde_yaml::from_str(
                &fs::read_to_string(
                    prepared
                        .root
                        .join(format!("config/generated-run-{attempt}.yaml")),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                generated["run"]["terminals"]["bp.terminal_0"]["logfile"]
                    .as_str()
                    .unwrap(),
                prepared
                    .console_log(FvpConsole::Host, attempt)
                    .to_str()
                    .unwrap(),
            );
        }
        assert!(
            prepared
                .populate_command(&mut Command::new("python"), 12347, &config, 2)
                .is_err()
        );
        prepared.cleanup().unwrap();
    }
}
