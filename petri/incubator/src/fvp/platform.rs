// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Read-only validation of the one approved local FVP platform.
//!
//! The caller holds the shared toolchain-use lock from validation through
//! post-run verification, including failure and cancellation cleanup. Source
//! checks do not replace copying and hashing owned snapshots before launch.
//! Model output must come from the exact executable in a session-owned,
//! digest-pinned container. This module never starts a container or a model.
//! Launchers must use [`PlatformSources::shrinkwrap_command_with_identity`],
//! not execute the raw entry point, so validation and execution use the same
//! isolated imports and the supervisor receives the matching executable identity.

use super::lifecycle::Cancellation;
use super::process::Deadline;
use super::process::run_command_cancellable;
use anyhow::Context;
use anyhow::bail;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

/// The approved manifest, not an override mechanism.
pub const PINNED_MANIFEST: &str = include_str!("../../platforms/fvp-cca-v15.yaml");
/// Sorted, exact package identities, including the editable checkout revision.
pub const PINNED_PIP_FREEZE: &str = include_str!("../../platforms/fvp-cca-v15.pip-freeze");

/// Full approved platform identity. Unknown fields fail closed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlatformManifest {
    /// Manifest schema understood by this validator.
    pub schema_version: u32,
    /// Licensed model identity inside the pinned container.
    pub fvp: ModelManifest,
    /// Live checkout, Python package, and container identities.
    pub shrinkwrap: ShrinkwrapManifest,
    /// Packaged Shrinkwrap configuration identity.
    pub package: PackageManifest,
    /// Provisioned CCA overlay identity.
    pub overlay: OverlayManifest,
    /// Firmware and device tree identities consumed at runtime.
    pub runtime: RuntimeManifest,
    /// Approved provenance mapping for the firmware package.
    pub provenance: ProvenanceManifest,
}

/// Licensed model path and exact normalized version banner.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelManifest {
    /// Absolute model path inside the container, never a host path.
    pub executable_in_container: String,
    /// The single accepted version line, excluding known boilerplate.
    pub version_output: String,
}

/// Identity of the live Shrinkwrap toolchain and its runtime image.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShrinkwrapManifest {
    /// Checkout directory relative to the platform root.
    pub root_relative_path: PathBuf,
    /// Regular executable entry point relative to the platform root.
    pub executable_relative_path: PathBuf,
    /// Full commit ID required for the clean editable checkout.
    pub checkout_revision: String,
    /// Exact installed `shrinkwraptool` distribution version.
    pub package_version: String,
    /// Container tag to inspect and immutable digest to execute.
    pub container: ContainerManifest,
}

/// Approved container inventory tag and immutable execution identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContainerManifest {
    /// Local tag whose repository digests must match the approved image.
    pub tag: String,
    /// Full repository-qualified SHA-256 digest required for execution.
    pub digest: String,
}

/// Identity of the packaged Shrinkwrap YAML.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    /// Declared root name; must be `shrinkwrap_package_root`.
    pub root_input: String,
    /// YAML path relative to the package root.
    pub yaml_relative_path: PathBuf,
    /// Full SHA-256 digest of the package YAML bytes.
    pub sha256: String,
}

/// Identity of the provisioned CCA overlay, before the per-run initrd overlay.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OverlayManifest {
    /// Declared root name; must be `fvp_platform_root`.
    pub root_input: String,
    /// Overlay path relative to the platform root.
    pub relative_path: PathBuf,
    /// Full SHA-256 digest of the overlay bytes.
    pub sha256: String,
}

/// Runtime firmware and device tree paths under the package root.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeManifest {
    /// Declared root name; must be `shrinkwrap_package_root`.
    pub root_input: String,
    /// BL1 path relative to the package root.
    pub bl1_relative_path: PathBuf,
    /// Full SHA-256 digest of BL1.
    pub bl1_sha256: String,
    /// Firmware image package path relative to the package root.
    pub fip_relative_path: PathBuf,
    /// Full SHA-256 digest of the firmware image package.
    pub fip_sha256: String,
    /// Device tree path relative to the package root.
    pub dtb_relative_path: PathBuf,
    /// Full SHA-256 digest of the device tree.
    pub dtb_sha256: String,
}

/// Approved mapping to the FIP hash, not revisions extracted from the binary.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceManifest {
    /// Full approved TF-A commit ID.
    pub tf_a_revision: String,
    /// Full approved TF-RMM commit ID.
    pub tf_rmm_revision: String,
    /// Exact approved TF-RMM build configuration label.
    pub tf_rmm_config: String,
    /// Exact approved EDK2 revision label, not a commit ID.
    pub edk2_revision: String,
    /// Exact approved device tree revision label, not a commit ID.
    pub dt_revision: String,
}

impl PlatformManifest {
    /// Load the checked-in expected tuple without reading either user root.
    pub fn pinned() -> anyhow::Result<Self> {
        serde_yaml::from_str(PINNED_MANIFEST).context("invalid built-in FVP platform manifest")
    }

    /// Parse a supplied inventory declaration and reject every tuple deviation.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let manifest: Self =
            serde_yaml::from_str(text).context("invalid FVP platform manifest schema")?;
        manifest.require_pinned()?;
        Ok(manifest)
    }

    fn require_pinned(&self) -> anyhow::Result<()> {
        ensure!(
            *self == Self::pinned()?,
            "unsupported FVP platform tuple; the complete pinned manifest must match"
        );
        Ok(())
    }
}

/// A source identity to check immediately before copying and on the owned copy.
#[derive(Clone, Debug)]
pub struct VerifiedSource {
    root: PathBuf,
    relative: PathBuf,
    canonical: PathBuf,
    sha256: String,
}

impl VerifiedSource {
    /// Canonical absolute path validated under the declared input root.
    pub fn path(&self) -> &Path {
        &self.canonical
    }

    /// Expected full SHA-256 digest for both source and owned snapshot.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Re-resolve the declared path as well as hashing it, to detect retargeting.
    pub fn revalidate(&self, deadline: &Deadline) -> anyhow::Result<()> {
        self.revalidate_cancellable(deadline, &Cancellation::default())
    }

    fn revalidate_cancellable(
        &self,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<()> {
        cancellation.check()?;
        let current = resolve(&self.root, &self.relative, PathKind::File)?;
        ensure!(
            current == self.canonical,
            "FVP source path changed since validation"
        );
        ensure!(
            hash_file_cancellable(&current, deadline, cancellation)? == self.sha256,
            "FVP source changed since validation"
        );
        Ok(())
    }

    /// Check a regular snapshot within the caller's canonical owned workspace.
    /// This does not copy sources or establish ownership of the workspace.
    pub fn verify_snapshot(
        &self,
        owned_root: &Path,
        relative_path: &Path,
        deadline: &Deadline,
    ) -> anyhow::Result<PathBuf> {
        let root = canonical_root(owned_root)?;
        let path = resolve(&root, relative_path, PathKind::File)?;
        verify_hash(&path, &self.sha256, deadline).context("FVP snapshot identity mismatch")?;
        Ok(path)
    }
}

/// Canonical sources only; this is not proof of a usable model or toolchain.
#[derive(Clone, Debug)]
pub struct PlatformSources {
    /// Canonical directory containing the checkout and provisioned overlay.
    pub platform_root: PathBuf,
    /// Canonical directory containing package metadata and runtime firmware.
    pub package_root: PathBuf,
    /// Canonical live Shrinkwrap checkout directory.
    pub shrinkwrap_root: PathBuf,
    /// Canonical regular executable entry point in the live virtualenv.
    /// Execute it only through [`Self::shrinkwrap_command`] to isolate imports.
    pub shrinkwrap_executable: PathBuf,
    /// Verified package YAML source.
    pub package: VerifiedSource,
    /// Verified provisioned overlay source.
    pub overlay: VerifiedSource,
    /// Verified BL1 source.
    pub bl1: VerifiedSource,
    /// Verified firmware image package source.
    pub fip: VerifiedSource,
    /// Verified device tree source.
    pub dtb: VerifiedSource,
    manifest: PlatformManifest,
}

impl PlatformSources {
    /// Validate only host files. No command is run and neither root is written.
    pub fn validate(
        manifest: &PlatformManifest,
        platform_root: &Path,
        package_root: &Path,
        deadline: &Deadline,
    ) -> anyhow::Result<Self> {
        manifest.require_pinned()?;
        Self::resolve_sources(manifest, platform_root, package_root, deadline)
    }

    fn resolve_sources(
        manifest: &PlatformManifest,
        platform_root: &Path,
        package_root: &Path,
        deadline: &Deadline,
    ) -> anyhow::Result<Self> {
        deadline.remaining()?;
        let platform_root = canonical_root(platform_root)?;
        let package_root = canonical_root(package_root)?;
        let shrinkwrap_root = resolve(
            &platform_root,
            &manifest.shrinkwrap.root_relative_path,
            PathKind::Directory,
        )?;
        let shrinkwrap_executable = resolve(
            &platform_root,
            &manifest.shrinkwrap.executable_relative_path,
            PathKind::Executable,
        )?;
        let source =
            |root: &Path, relative: &Path, sha256: &str| -> anyhow::Result<VerifiedSource> {
                let canonical = resolve(root, relative, PathKind::File)?;
                verify_hash(&canonical, sha256, deadline)?;
                Ok(VerifiedSource {
                    root: root.to_owned(),
                    relative: relative.to_owned(),
                    canonical,
                    sha256: sha256.to_owned(),
                })
            };
        Ok(Self {
            package: source(
                &package_root,
                &manifest.package.yaml_relative_path,
                &manifest.package.sha256,
            )?,
            overlay: source(
                &platform_root,
                &manifest.overlay.relative_path,
                &manifest.overlay.sha256,
            )?,
            bl1: source(
                &package_root,
                &manifest.runtime.bl1_relative_path,
                &manifest.runtime.bl1_sha256,
            )?,
            fip: source(
                &package_root,
                &manifest.runtime.fip_relative_path,
                &manifest.runtime.fip_sha256,
            )?,
            dtb: source(
                &package_root,
                &manifest.runtime.dtb_relative_path,
                &manifest.runtime.dtb_sha256,
            )?,
            platform_root,
            package_root,
            shrinkwrap_root,
            shrinkwrap_executable,
            manifest: manifest.clone(),
        })
    }

    /// All consumed platform files, in package, overlay, BL1, FIP, DTB order.
    pub fn sources(&self) -> [&VerifiedSource; 5] {
        [
            &self.package,
            &self.overlay,
            &self.bl1,
            &self.fip,
            &self.dtb,
        ]
    }

    /// Call immediately before staging. PR 3 must hash each resulting snapshot.
    pub fn revalidate_sources(&self, deadline: &Deadline) -> anyhow::Result<()> {
        for source in self.sources() {
            source.revalidate(deadline)?;
        }
        Ok(())
    }

    /// Build a Shrinkwrap command with the same isolated Python import policy
    /// used by inventory validation: the entry point's virtualenv Python with
    /// `-I -B`, the canonical entry point, and normalized Python environment.
    ///
    /// The launcher must use this builder instead of executing the raw entry
    /// point. Add Shrinkwrap arguments after construction; do not replace the
    /// interpreter or isolation flags. Hold the toolchain-use lock and require
    /// successful inventory validation before executing the command.
    pub fn shrinkwrap_command(&self) -> anyhow::Result<Command> {
        self.shrinkwrap_command_with_identity()
            .map(|(command, _)| command)
    }

    /// Build the isolated command together with its canonical launcher identity.
    /// Pass the returned identity to the session supervisor for `/proc/exe`
    /// ownership checks. Both values come from the same interpreter observation.
    ///
    /// The command retains the virtualenv interpreter path to select the correct
    /// Python environment. Do not replace it with the canonical identity, which
    /// can point outside the virtualenv through a symlink. The toolchain lock and
    /// inventory requirements of [`Self::shrinkwrap_command`] also apply here.
    #[expect(
        clippy::disallowed_methods,
        reason = "the supervisor needs the resolved interpreter identity for /proc/exe"
    )]
    pub fn shrinkwrap_command_with_identity(&self) -> anyhow::Result<(Command, PathBuf)> {
        let python = self.python_interpreter()?;
        let expected_executable = fs::canonicalize(&python)
            .context("cannot resolve the Shrinkwrap launcher executable identity")?;
        let mut command = isolated_python_command(&python);
        command.arg(&self.shrinkwrap_executable);
        Ok((command, expected_executable))
    }

    /// Check toolchain and image inventory without starting any container.
    /// The caller must already hold its shared toolchain-use lock.
    pub fn validate_inventory(
        &self,
        docker: &super::lifecycle::Docker,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<ToolchainIdentity> {
        cancellation.check()?;
        self.manifest.require_pinned()?;
        for source in self.sources() {
            source.revalidate_cancellable(deadline, cancellation)?;
        }
        let before = self.capture_toolchain(deadline, cancellation)?;
        self.check_checkout(deadline, cancellation)?;
        self.check_python(deadline, cancellation)?;
        docker.verify_identity(deadline, cancellation)?;
        let output = checked_command(
            docker.command().args([
                "image",
                "inspect",
                "--format",
                "{{json .RepoDigests}}",
                &self.manifest.shrinkwrap.container.tag,
            ]),
            deadline,
            cancellation,
            "FVP container image inventory",
        )?;
        verify_repo_digests(&output, &self.manifest.shrinkwrap.container)?;
        docker.verify_identity(deadline, cancellation)?;
        let after = self.capture_toolchain(deadline, cancellation)?;
        ensure!(before == after, "FVP toolchain changed during validation");
        Ok(before)
    }

    /// Always call after execution and owned-resource cleanup, even on failure.
    /// An error invalidates an otherwise successful guest test.
    pub fn verify_toolchain_after(
        &self,
        before: &ToolchainIdentity,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<()> {
        ensure!(
            *before == self.capture_toolchain(deadline, cancellation)?,
            "FVP live toolchain changed; run is invalid"
        );
        self.check_checkout(deadline, cancellation)?;
        self.check_python(deadline, cancellation)?;
        ensure!(
            *before == self.capture_toolchain(deadline, cancellation)?,
            "FVP live toolchain changed during post-run verification; run is invalid"
        );
        Ok(())
    }

    fn capture_toolchain(
        &self,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<ToolchainIdentity> {
        cancellation.check()?;
        ensure!(
            resolve(
                &self.platform_root,
                &self.manifest.shrinkwrap.root_relative_path,
                PathKind::Directory
            )? == self.shrinkwrap_root,
            "FVP toolchain checkout path changed"
        );
        ensure!(
            resolve(
                &self.platform_root,
                &self.manifest.shrinkwrap.executable_relative_path,
                PathKind::Executable
            )? == self.shrinkwrap_executable,
            "FVP toolchain executable path changed"
        );
        ToolchainIdentity::capture_cancellable(&self.shrinkwrap_root, deadline, cancellation)
    }

    fn git(
        &self,
        args: &[&std::ffi::OsStr],
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Vec<u8>> {
        let mut command = Command::new("git");
        command
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(&self.shrinkwrap_root)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        checked_command(
            &mut command,
            deadline,
            cancellation,
            "FVP Shrinkwrap checkout inventory",
        )
    }

    fn check_checkout(
        &self,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<()> {
        let git = |args: &[&str]| {
            self.git(
                &args.iter().map(std::ffi::OsStr::new).collect::<Vec<_>>(),
                deadline,
                cancellation,
            )
        };
        let revision = git(&["rev-parse", "--verify", "HEAD"])?;
        ensure!(
            std::str::from_utf8(&revision)?.trim() == self.manifest.shrinkwrap.checkout_revision,
            "FVP Shrinkwrap checkout revision mismatch"
        );
        let status = git(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
        ])?;
        let overlay = self
            .overlay
            .canonical
            .strip_prefix(&self.shrinkwrap_root)
            .context("FVP overlay must be inside the pinned Shrinkwrap checkout")?;
        verify_checkout_status(&status, overlay)?;
        let tree = git(&["ls-tree", "-r", "-z", "--full-tree", "HEAD"])?;
        for entry in tree.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
            cancellation.check()?;
            deadline.remaining()?;
            let (header, path) =
                split_bytes(entry, b'\t').context("invalid Shrinkwrap tree entry")?;
            let fields: Vec<_> = header.split(|b| *b == b' ').collect();
            ensure!(
                fields.len() == 3 && fields[1] == b"blob",
                "unsupported Shrinkwrap tracked entry"
            );
            let relative = PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()));
            let file = resolve(&self.shrinkwrap_root, &relative, PathKind::File)?;
            ensure!(
                !fs::symlink_metadata(self.shrinkwrap_root.join(&relative))?
                    .file_type()
                    .is_symlink(),
                "FVP Shrinkwrap tracked symlink is not supported"
            );
            let executable = fs::metadata(&file)?.permissions().mode() & 0o111 != 0;
            ensure!(
                (fields[0] == b"100755" && executable) || (fields[0] == b"100644" && !executable),
                "FVP Shrinkwrap tracked file mode mismatch"
            );
            let object = std::ffi::OsStr::from_bytes(fields[2]);
            let expected = self.git(
                &["cat-file".as_ref(), "blob".as_ref(), object],
                deadline,
                cancellation,
            )?;
            ensure!(
                hash_file_cancellable(&file, deadline, cancellation)?
                    == hex::encode(Sha256::digest(&expected)),
                "FVP Shrinkwrap tracked bytes differ from pinned tree: {}",
                relative.display()
            );
        }
        Ok(())
    }

    fn python_interpreter(&self) -> anyhow::Result<PathBuf> {
        ensure!(
            resolve(
                &self.platform_root,
                &self.manifest.shrinkwrap.root_relative_path,
                PathKind::Directory
            )? == self.shrinkwrap_root
                && resolve(
                    &self.platform_root,
                    &self.manifest.shrinkwrap.executable_relative_path,
                    PathKind::Executable
                )? == self.shrinkwrap_executable,
            "FVP toolchain paths changed"
        );
        let entry = fs::read(&self.shrinkwrap_executable)?;
        let first = entry
            .split(|b| *b == b'\n')
            .next()
            .context("empty Shrinkwrap entry point")?;
        let interpreter = first
            .strip_prefix(b"#!")
            .context("FVP Shrinkwrap entry point has no interpreter")?;
        let python = Path::new(std::ffi::OsStr::from_bytes(interpreter));
        ensure!(
            python.is_absolute()
                && python.parent() == Some(self.shrinkwrap_root.join("venv/bin").as_path())
                && !interpreter.iter().any(u8::is_ascii_whitespace),
            "FVP Shrinkwrap entry point must use its absolute virtualenv interpreter"
        );
        ensure!(
            fs::metadata(python)?.is_file()
                && fs::metadata(python)?.permissions().mode() & 0o111 != 0,
            "FVP virtualenv interpreter is not a regular executable"
        );
        Ok(python.to_owned())
    }

    fn check_python(&self, deadline: &Deadline, cancellation: &Cancellation) -> anyhow::Result<()> {
        cancellation.check()?;
        let python = self.python_interpreter()?;
        let mut version = self.shrinkwrap_command()?;
        let version = checked_command(
            version.arg("--version"),
            deadline,
            cancellation,
            "FVP Shrinkwrap version",
        )?;
        verify_shrinkwrap_version(
            std::str::from_utf8(&version)?,
            &self.manifest.shrinkwrap.package_version,
        )?;
        let mut freeze = isolated_python_command(&python);
        let freeze = checked_command(
            freeze.args(["-m", "pip", "freeze", "--disable-pip-version-check"]),
            deadline,
            cancellation,
            "FVP Python package inventory",
        )?;
        verify_pip_freeze(std::str::from_utf8(&freeze)?)?;
        // Check the installed editable distribution, not only pip's VCS label.
        let mut metadata = isolated_python_command(&python);
        let output = checked_command(metadata.args(["-c", concat!(
            "import importlib.metadata,json,pathlib,shrinkwrap,urllib.parse;",
            "d=importlib.metadata.distribution('shrinkwraptool');",
            "u=json.loads(d.read_text('direct_url.json'));",
            "p=urllib.parse.urlparse(u['url']);",
            "print(json.dumps({'version':d.version,'editable':u.get('dir_info',{}).get('editable',False),",
            "'scheme':p.scheme,'host':p.netloc,'root':urllib.parse.unquote(p.path),",
            "'module':str(pathlib.Path(shrinkwrap.__file__).resolve())}))"
        )]), deadline, cancellation, "FVP editable Python checkout identity")?;
        #[derive(Deserialize)]
        struct PythonIdentity {
            version: String,
            editable: bool,
            scheme: String,
            host: String,
            root: PathBuf,
            module: PathBuf,
        }
        let identity: PythonIdentity = serde_json::from_slice(&output)
            .context("invalid FVP editable Python checkout identity")?;
        ensure!(
            identity.version == self.manifest.shrinkwrap.package_version
                && identity.editable
                && identity.scheme == "file"
                && identity.host.is_empty()
                && canonical_root(&identity.root)? == self.shrinkwrap_root
                && identity.module == self.shrinkwrap_root.join("src/shrinkwrap/__init__.py"),
            "FVP Python package version or editable checkout mismatch"
        );
        Ok(())
    }
}

fn isolated_python_command(python: &Path) -> Command {
    let mut command = Command::new(python);
    command.args(["-I", "-B"]);
    python_environment(&mut command);
    command
}

fn python_environment(command: &mut Command) {
    command
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONHOME")
        .env_remove("PYTHONUSERBASE")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONNOUSERSITE", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1");
}

fn checked_command(
    command: &mut Command,
    deadline: &Deadline,
    cancellation: &Cancellation,
    operation: &str,
) -> anyhow::Result<Vec<u8>> {
    let output = run_command_cancellable(command, deadline, cancellation)
        .with_context(|| operation.to_owned())?;
    // Do not print arbitrary subprocess output: it can contain user environment
    // or registry credentials. The operation and exit status are safe diagnostics.
    ensure!(
        output.status.success(),
        "{operation} failed: {}",
        output.status
    );
    ensure!(
        output.stderr.iter().all(u8::is_ascii_whitespace),
        "{operation} emitted unexpected diagnostics"
    );
    Ok(output.stdout)
}

/// Verify the command identity and output reported by the owned-container
/// supervisor. This must not be populated from unverified user declarations.
pub fn verify_model_identity(
    image_digest: &str,
    executable_in_container: &str,
    output: &Output,
) -> anyhow::Result<()> {
    let manifest = PlatformManifest::pinned()?;
    ensure!(
        image_digest == manifest.shrinkwrap.container.digest
            && executable_in_container == manifest.fvp.executable_in_container,
        "FVP model must use the exact executable inside the pinned image digest"
    );
    verify_model_version(output)
}

/// Verify only the banner. Use `verify_model_identity` at the container
/// boundary. The caller retains ownership labels and cleans up the container.
pub fn verify_model_version(output: &Output) -> anyhow::Result<()> {
    ensure!(
        output.status.success(),
        "FVP model version command failed: {}",
        output.status
    );
    let manifest = PlatformManifest::pinned()?;
    let mut versions = Vec::new();
    for stream in [&output.stdout, &output.stderr] {
        for line in std::str::from_utf8(stream)
            .context("FVP model banner is not UTF-8")?
            .lines()
        {
            let line = line.trim();
            match line {
                ""
                | "Copyright 2000-2026 ARM Limited."
                | "All Rights Reserved."
                | "Info: /OSCI/SystemC: Simulation stopped by user." => {}
                _ => versions.push(line),
            }
        }
    }
    ensure!(
        versions == [manifest.fvp.version_output.as_str()],
        "FVP model version mismatch: expected one exact pinned version line and only known boilerplate"
    );
    Ok(())
}

/// Reject ambiguous tags even when one entry happens to have the approved hash.
pub fn verify_repo_digests(bytes: &[u8], expected: &ContainerManifest) -> anyhow::Result<()> {
    ensure!(
        expected == &PlatformManifest::pinned()?.shrinkwrap.container,
        "unsupported FVP container tuple"
    );
    let digests: Vec<String> =
        serde_json::from_slice(bytes).context("invalid FVP container RepoDigests")?;
    let (repository, _) = expected
        .digest
        .split_once('@')
        .context("invalid pinned FVP digest")?;
    let matches: Vec<_> = digests
        .iter()
        .filter(|digest| {
            digest
                .split_once('@')
                .is_some_and(|(repo, _)| repo == repository)
        })
        .collect();
    ensure!(
        matches.len() == 1 && matches[0] == &expected.digest,
        "FVP container RepoDigests mismatch: require exactly one approved repository digest"
    );
    Ok(())
}

fn verify_shrinkwrap_version(output: &str, version: &str) -> anyhow::Result<()> {
    let lines: Vec<_> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    ensure!(
        lines == [format!("shrinkwrap version {version}")],
        "FVP Shrinkwrap package version mismatch"
    );
    Ok(())
}

/// Ignore package ordering and blank lines, but never package or revision drift.
pub fn normalize_pip_freeze(text: &str) -> anyhow::Result<String> {
    let mut lines = BTreeSet::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        ensure!(
            !line.starts_with('#'),
            "unsupported FVP pip freeze comment or local editable identity"
        );
        ensure!(lines.insert(line), "duplicate FVP Python package identity");
    }
    Ok(lines.into_iter().collect::<Vec<_>>().join("\n") + "\n")
}

/// Require the exact checked-in package set and editable checkout revision.
pub fn verify_pip_freeze(text: &str) -> anyhow::Result<()> {
    ensure!(
        normalize_pip_freeze(text)? == normalize_pip_freeze(PINNED_PIP_FREEZE)?,
        "FVP Python package set or editable revision mismatch"
    );
    Ok(())
}

fn verify_checkout_status(status: &[u8], overlay: &Path) -> anyhow::Result<()> {
    for entry in status.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        ensure!(
            entry.len() >= 4 && entry[2] == b' ',
            "malformed FVP Shrinkwrap status"
        );
        let path = Path::new(std::ffi::OsStr::from_bytes(&entry[3..]));
        let allowed_input = path == overlay || path.starts_with("venv");
        ensure!(
            (&entry[..2] == b"??" || &entry[..2] == b"!!") && allowed_input,
            "FVP Shrinkwrap checkout is dirty or has unapproved untracked inputs"
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum PathKind {
    Directory,
    File,
    Executable,
}

#[expect(
    clippy::disallowed_methods,
    reason = "platform roots require resolved symlink identities"
)]
fn canonical_root(path: &Path) -> anyhow::Result<PathBuf> {
    let root = fs::canonicalize(path)
        .with_context(|| format!("missing FVP input root: {}", path.display()))?;
    ensure!(root.is_dir(), "FVP input root must be a directory");
    Ok(root)
}

fn validate_relative(relative: &Path) -> anyhow::Result<()> {
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "FVP manifest paths must be nonempty root-relative paths without traversal"
    );
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "enforce containment after resolving symlinks"
)]
fn resolve(root: &Path, relative: &Path, kind: PathKind) -> anyhow::Result<PathBuf> {
    validate_relative(relative)?;
    let path = fs::canonicalize(root.join(relative))
        .with_context(|| format!("missing FVP input: {}", relative.display()))?;
    ensure!(
        path.starts_with(root),
        "FVP input symlink escapes its declared root"
    );
    let metadata = fs::metadata(&path)?;
    match kind {
        PathKind::Directory => ensure!(metadata.is_dir(), "FVP input must be a directory"),
        PathKind::File | PathKind::Executable => {
            ensure!(metadata.is_file(), "FVP input must be a regular file")
        }
    }
    if matches!(kind, PathKind::Executable) {
        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "FVP Shrinkwrap entry point is not executable"
        );
    }
    Ok(path)
}

fn hash_file(path: &Path, deadline: &Deadline) -> anyhow::Result<String> {
    hash_file_cancellable(path, deadline, &Cancellation::default())
}

fn hash_file_cancellable(
    path: &Path,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<String> {
    cancellation.check()?;
    deadline.remaining()?;
    let mut file = fs::File::open(path)
        .with_context(|| format!("cannot read FVP input: {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "FVP input must be a regular file"
    );
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        cancellation.check()?;
        deadline.remaining()?;
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    Ok(hex::encode(hash.finalize()))
}

fn verify_hash(path: &Path, expected: &str, deadline: &Deadline) -> anyhow::Result<()> {
    ensure!(
        hash_file(path, deadline)? == expected,
        "FVP SHA-256 mismatch: {}",
        path.display()
    );
    Ok(())
}

fn split_bytes(bytes: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    let position = bytes.iter().position(|b| *b == separator)?;
    Some((&bytes[..position], &bytes[position + 1..]))
}

/// Byte and mode inventory of the live checkout and virtualenv. `.git` is
/// checked separately; Python caches and all other files are included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolchainIdentity(BTreeMap<PathBuf, TreeEntry>);

#[derive(Clone, Debug, PartialEq, Eq)]
struct TreeEntry {
    mode: u32,
    link: Option<PathBuf>,
    canonical: PathBuf,
    sha256: Option<String>,
}

impl ToolchainIdentity {
    /// Hash checkout and virtualenv files, modes, and link targets without writes.
    /// External file link targets, such as the Python interpreter, are hashed too.
    pub fn capture(root: &Path, deadline: &Deadline) -> anyhow::Result<Self> {
        Self::capture_cancellable(root, deadline, &Cancellation::default())
    }

    /// Capture a live toolchain while polling cancellation between files and
    /// each bounded hash read. A cancelled capture cannot validate a run.
    pub fn capture_cancellable(
        root: &Path,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Self> {
        cancellation.check()?;
        let root = canonical_root(root)?;
        let mut identity = Self(BTreeMap::new());
        identity.visit(
            &root,
            Path::new(""),
            &mut BTreeSet::new(),
            deadline,
            cancellation,
        )?;
        Ok(identity)
    }

    /// Pure post-run byte identity check, also useful if command execution failed.
    pub fn verify_unchanged(&self, root: &Path, deadline: &Deadline) -> anyhow::Result<()> {
        ensure!(
            *self == Self::capture(root, deadline)?,
            "FVP live toolchain changed; run is invalid"
        );
        Ok(())
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "inventory must detect symlink target changes"
    )]
    fn visit(
        &mut self,
        root: &Path,
        relative: &Path,
        ancestors: &mut BTreeSet<PathBuf>,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<()> {
        cancellation.check()?;
        deadline.remaining()?;
        let path = root.join(relative);
        let raw = fs::symlink_metadata(&path)?;
        let canonical = fs::canonicalize(&path)?;
        let metadata = fs::metadata(&canonical)?;
        let link = if raw.file_type().is_symlink() {
            Some(fs::read_link(&path)?)
        } else {
            None
        };
        let sha256 = if metadata.is_file() {
            Some(hash_file_cancellable(&canonical, deadline, cancellation)?)
        } else if metadata.is_dir() {
            ensure!(
                canonical.starts_with(root),
                "FVP toolchain directory link escapes checkout"
            );
            None
        } else {
            bail!("FVP toolchain contains a non-regular input");
        };
        self.0.insert(
            relative.to_owned(),
            TreeEntry {
                mode: metadata.permissions().mode(),
                link,
                canonical: canonical.clone(),
                sha256,
            },
        );
        if metadata.is_dir() {
            ensure!(
                ancestors.insert(canonical.clone()),
                "FVP toolchain contains a directory link cycle"
            );
            for entry in fs::read_dir(&path)? {
                cancellation.check()?;
                deadline.remaining()?;
                let entry = entry?;
                if relative.as_os_str().is_empty() && entry.file_name() == ".git" {
                    continue;
                }
                self.visit(
                    root,
                    &relative.join(entry.file_name()),
                    ancestors,
                    deadline,
                    cancellation,
                )?;
            }
            ancestors.remove(&canonical);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::ExitStatusExt;
    use std::time::Duration;
    use test_with_tracing::test;

    fn deadline() -> Deadline {
        Deadline::new(Duration::from_secs(30)).unwrap()
    }

    fn fixture() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fvp-platform-unit");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(canonical_root(&root).unwrap()).unwrap()
    }

    #[test]
    fn every_manifest_leaf_is_pinned() {
        let original: serde_json::Value =
            serde_json::to_value(PlatformManifest::pinned().unwrap()).unwrap();
        fn mutate(value: &mut serde_json::Value, path: &[String]) {
            if let Some((key, rest)) = path.split_first() {
                mutate(&mut value[key], rest);
            } else if value.is_string() {
                *value = serde_json::Value::String(format!("{}-wrong", value.as_str().unwrap()));
            } else {
                *value = serde_json::json!(99);
            }
        }
        fn leaves(value: &serde_json::Value, path: Vec<String>, result: &mut Vec<Vec<String>>) {
            if let Some(object) = value.as_object() {
                for (key, child) in object {
                    let mut path = path.clone();
                    path.push(key.clone());
                    leaves(child, path, result);
                }
            } else {
                result.push(path);
            }
        }
        let mut paths = Vec::new();
        leaves(&original, Vec::new(), &mut paths);
        for path in paths {
            let mut changed = original.clone();
            mutate(&mut changed, &path);
            assert!(
                PlatformManifest::parse(&serde_yaml::to_string(&changed).unwrap()).is_err(),
                "{path:?}"
            );
        }
        assert!(PlatformManifest::parse(PINNED_MANIFEST).is_ok());
        assert!(PlatformManifest::parse(&format!("{PINNED_MANIFEST}\nunknown: true\n")).is_err());
        assert!(
            PlatformManifest::parse(&PINNED_MANIFEST.replace("  tf_rmm_config: fvp_defcfg\n", ""))
                .is_err()
        );
    }

    #[test]
    fn declared_roots_types_and_symlinks() {
        let dir = fixture();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), b"data").unwrap();
        fs::write(dir.path().join("outside"), b"data").unwrap();
        symlink("../outside", root.join("escape")).unwrap();
        symlink("file", root.join("inside")).unwrap();
        for invalid in ["../outside", "/outside", "", "./file", "escape", "missing"] {
            assert!(
                resolve(&root, Path::new(invalid), PathKind::File).is_err(),
                "{invalid}"
            );
        }
        assert_eq!(
            resolve(&root, Path::new("inside"), PathKind::File).unwrap(),
            root.join("file")
        );
        assert!(resolve(&root, Path::new("file"), PathKind::Directory).is_err());
        assert!(resolve(&root, Path::new("file"), PathKind::Executable).is_err());
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(resolve(&root, Path::new("file"), PathKind::Executable).is_ok());
        assert!(canonical_root(&root.join("file")).is_err());
    }

    #[test]
    fn snapshot_and_source_mutations_fail() {
        let dir = fixture();
        let root = canonical_root(dir.path()).unwrap();
        fs::write(root.join("source"), b"approved").unwrap();
        let source = VerifiedSource {
            canonical: root.join("source"),
            root: root.clone(),
            relative: "source".into(),
            sha256: hex::encode(Sha256::digest(b"approved")),
        };
        fs::write(root.join("snapshot"), b"approved").unwrap();
        source.revalidate(&deadline()).unwrap();
        source
            .verify_snapshot(&root, Path::new("snapshot"), &deadline())
            .unwrap();
        fs::write(root.join("snapshot"), b"wrong").unwrap();
        assert!(
            source
                .verify_snapshot(&root, Path::new("snapshot"), &deadline())
                .is_err()
        );
        fs::write(root.join("source"), b"changed").unwrap();
        assert!(source.revalidate(&deadline()).is_err());
    }

    fn platform_fixture(platform: &Path, package: &Path) -> PlatformSources {
        fs::create_dir_all(platform.join("shrinkwrap/venv/bin")).unwrap();
        fs::create_dir_all(platform.join("shrinkwrap/config")).unwrap();
        fs::create_dir_all(package.join("cca-3world")).unwrap();
        let executable = platform.join("shrinkwrap/venv/bin/shrinkwrap");
        fs::write(&executable, b"fixture").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut manifest = PlatformManifest::pinned().unwrap();
        let digest = hex::encode(Sha256::digest(b"fixture"));
        manifest.package.sha256 = digest.clone();
        manifest.overlay.sha256 = digest.clone();
        manifest.runtime.bl1_sha256 = digest.clone();
        manifest.runtime.fip_sha256 = digest.clone();
        manifest.runtime.dtb_sha256 = digest;
        let paths = [
            package.join(&manifest.package.yaml_relative_path),
            platform.join(&manifest.overlay.relative_path),
            package.join(&manifest.runtime.bl1_relative_path),
            package.join(&manifest.runtime.fip_relative_path),
            package.join(&manifest.runtime.dtb_relative_path),
        ];
        for path in &paths {
            fs::write(path, b"fixture").unwrap();
        }
        PlatformSources::resolve_sources(&manifest, platform, package, &deadline()).unwrap()
    }

    #[test]
    fn all_five_sources_use_declared_roots_and_exact_hashes() {
        let dir = fixture();
        let platform = dir.path().join("platform with spaces");
        let package = dir.path().join("alternate package root");
        let sources = platform_fixture(&platform, &package);
        let manifest = &sources.manifest;
        let paths: Vec<_> = sources
            .sources()
            .iter()
            .map(|source| source.path().to_owned())
            .collect();
        // Only the private resolver can accept fixture hashes. The production
        // entry point must still reject this otherwise well-formed tuple.
        assert!(PlatformSources::validate(manifest, &platform, &package, &deadline()).is_err());
        for (source, path) in sources.sources().iter().zip(&paths) {
            assert_eq!(source.path(), path);
            fs::write(path, b"modified").unwrap();
            assert!(sources.revalidate_sources(&deadline()).is_err());
            assert!(
                PlatformSources::resolve_sources(manifest, &platform, &package, &deadline())
                    .is_err()
            );
            fs::write(path, b"fixture").unwrap();
        }
        sources.revalidate_sources(&deadline()).unwrap();
    }

    #[test]
    fn shrinkwrap_version_requires_the_actual_pinned_banner() {
        let version = "2026.9.0.dev0";
        verify_shrinkwrap_version("shrinkwrap version 2026.9.0.dev0\n", version).unwrap();
        for rejected in [
            "2026.9.0.dev0\n",
            "shrinkwrap 2026.9.0.dev0\n",
            "shrinkwrap version 2026.9.0.dev1\n",
            "shrinkwrap version 2026.9.0.dev0\nunexpected line\n",
            "shrinkwrap version 2026.9.0.dev0\nshrinkwrap version 2026.9.0.dev0\n",
        ] {
            assert!(
                verify_shrinkwrap_version(rejected, version).is_err(),
                "{rejected}"
            );
        }
    }

    #[test]
    fn isolated_entry_point_ignores_script_directory_shadow_modules() {
        let dir = fixture();
        let sources = platform_fixture(&dir.path().join("platform"), &dir.path().join("package"));
        let budget = deadline();
        let cancellation = Cancellation::default();
        let venv = sources.shrinkwrap_root.join("venv");
        checked_command(
            Command::new("python3")
                .args(["-I", "-B", "-m", "venv", "--without-pip", "--symlinks"])
                .arg(&venv),
            &budget,
            &cancellation,
            "create owned Python fixture",
        )
        .unwrap();
        let python = venv.join("bin/python");
        let output = checked_command(
            isolated_python_command(&python).args([
                "-c",
                "import sysconfig; print(sysconfig.get_path('purelib'))",
            ]),
            &budget,
            &cancellation,
            "locate owned Python fixture modules",
        )
        .unwrap();
        let site_packages = PathBuf::from(std::str::from_utf8(&output).unwrap().trim());
        assert!(site_packages.starts_with(&venv));
        fs::write(
            site_packages.join("shrinkwrap.py"),
            "VERSION = 'shrinkwrap version 2026.9.0.dev0'\n",
        )
        .unwrap();
        fs::write(
            venv.join("bin/shrinkwrap.py"),
            "VERSION = 'shadow module was imported'\n",
        )
        .unwrap();
        fs::write(
            &sources.shrinkwrap_executable,
            format!(
                "#!{}\nfrom shrinkwrap import VERSION\nprint(VERSION)\n",
                python.display()
            ),
        )
        .unwrap();
        let before = ToolchainIdentity::capture(&sources.shrinkwrap_root, &budget).unwrap();

        let mut raw = Command::new(&sources.shrinkwrap_executable);
        python_environment(&mut raw);
        let raw_output = checked_command(
            raw.arg("--version"),
            &budget,
            &cancellation,
            "demonstrate script directory shadowing",
        )
        .unwrap();
        assert_eq!(raw_output, b"shadow module was imported\n");

        let (mut isolated, expected_executable) =
            sources.shrinkwrap_command_with_identity().unwrap();
        assert!(
            fs::symlink_metadata(&python)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(isolated.get_program(), python.as_os_str());
        #[expect(
            clippy::disallowed_methods,
            reason = "compare resolved command and /proc/exe identities through virtualenv symlinks"
        )]
        let resolved_program = fs::canonicalize(isolated.get_program()).unwrap();
        assert_eq!(resolved_program, expected_executable);
        assert_ne!(python, expected_executable);
        assert_eq!(
            isolated.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("-I"),
                std::ffi::OsStr::new("-B"),
                sources.shrinkwrap_executable.as_os_str(),
            ]
        );
        let output = checked_command(
            isolated.arg("--version"),
            &budget,
            &cancellation,
            "verify isolated entry point",
        )
        .unwrap();
        verify_shrinkwrap_version(
            std::str::from_utf8(&output).unwrap(),
            &sources.manifest.shrinkwrap.package_version,
        )
        .unwrap();
        before
            .verify_unchanged(&sources.shrinkwrap_root, &budget)
            .unwrap();
    }

    #[test]
    fn live_toolchain_mutation_invalidates_run() {
        let dir = fixture();
        fs::create_dir(dir.path().join("venv")).unwrap();
        fs::write(dir.path().join("venv/module.py"), b"approved").unwrap();
        let before = ToolchainIdentity::capture(dir.path(), &deadline()).unwrap();
        before.verify_unchanged(dir.path(), &deadline()).unwrap();
        fs::write(dir.path().join("venv/module.py"), b"modified").unwrap();
        assert!(before.verify_unchanged(dir.path(), &deadline()).is_err());
    }

    #[test]
    fn model_banner_is_exact_and_all_streams_are_checked() {
        fn output(stdout: &str, stderr: &str) -> Output {
            Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            }
        }
        let version = PlatformManifest::pinned().unwrap().fvp.version_output;
        assert!(verify_model_version(&output(&version, "")).is_ok());
        let manifest = PlatformManifest::pinned().unwrap();
        assert!(
            verify_model_identity(
                &manifest.shrinkwrap.container.digest,
                &manifest.fvp.executable_in_container,
                &output(&version, "")
            )
            .is_ok()
        );
        assert!(
            verify_model_identity(
                &manifest.shrinkwrap.container.tag,
                &manifest.fvp.executable_in_container,
                &output(&version, "")
            )
            .is_err()
        );
        assert!(
            verify_model_identity(
                &manifest.shrinkwrap.container.digest,
                "/host/model",
                &output(&version, "")
            )
            .is_err()
        );
        assert!(
            verify_model_version(&output(
                &format!("\n{version}\nCopyright 2000-2026 ARM Limited.\nAll Rights Reserved.\n"),
                "Info: /OSCI/SystemC: Simulation stopped by user.\n"
            ))
            .is_ok()
        );
        for bad in ["", "Fast Models [11.31.29 (Mar  1 2026)]", "noise"] {
            assert!(verify_model_version(&output(bad, "")).is_err());
        }
        assert!(verify_model_version(&output(&version, &version)).is_err());
        assert!(verify_model_version(&output(&version, "unexpected warning")).is_err());
        let mut failed = output(&version, "");
        failed.status = std::process::ExitStatus::from_raw(256);
        assert!(verify_model_version(&failed).is_err());
    }

    #[test]
    fn image_digest_rejects_absence_duplicates_and_conflicts() {
        let container = PlatformManifest::pinned().unwrap().shrinkwrap.container;
        assert!(
            verify_repo_digests(
                &serde_json::to_vec(&[&container.digest]).unwrap(),
                &container
            )
            .is_ok()
        );
        for value in [
            serde_json::json!([]),
            serde_json::json!(null),
            serde_json::json!([container.digest, container.digest]),
            serde_json::json!([container.digest, "shrinkwraptool/base-slim@sha256:wrong"]),
            serde_json::json!(["other@sha256:wrong"]),
        ] {
            assert!(verify_repo_digests(&serde_json::to_vec(&value).unwrap(), &container).is_err());
        }
    }

    #[test]
    fn freeze_rejects_dependency_and_editable_drift() {
        verify_pip_freeze(PINNED_PIP_FREEZE).unwrap();
        let reversed = PINNED_PIP_FREEZE
            .lines()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        verify_pip_freeze(&reversed).unwrap();
        for bad in [
            PINNED_PIP_FREEZE.replace("6.0.3", "6.0.4"),
            PINNED_PIP_FREEZE.replace("1c6b7a5278b47be11cad3bcd3a20416fc43fd388", "deadbeef"),
            format!("{PINNED_PIP_FREEZE}extra==1\n"),
            format!("{PINNED_PIP_FREEZE}PyYAML==6.0.3\n"),
            PINNED_PIP_FREEZE.replace("termcolor==3.3.0\n", ""),
        ] {
            assert!(verify_pip_freeze(&bad).is_err());
        }
    }

    #[test]
    fn checkout_status_has_no_line_ending_waiver() {
        let overlay = Path::new("config/kvm_cca_planes.yaml");
        verify_checkout_status(b"?? config/kvm_cca_planes.yaml\0!! venv/\0", overlay).unwrap();
        for bad in [
            b" M tracked.py\0".as_slice(),
            b"M  tracked.py\0",
            b"?? unknown.py\0",
            b"!! src/__pycache__/\0",
            b"?? venv-foreign/file\0",
            b"R  new\0old\0",
        ] {
            assert!(verify_checkout_status(bad, overlay).is_err());
        }
    }

    #[test]
    fn expired_budget_stops_filesystem_inventory() {
        let dir = fixture();
        assert!(
            ToolchainIdentity::capture(dir.path(), &Deadline::new(Duration::ZERO).unwrap())
                .is_err()
        );
    }

    #[test]
    fn cancellation_stops_inventory_before_command_launch() {
        let cancellation = Cancellation::default();
        cancellation.cancel();
        let error = checked_command(
            &mut Command::new("unused-inventory-command"),
            &deadline(),
            &cancellation,
            "fixture inventory",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("FVP run cancelled"));
    }

    #[test]
    fn cancellation_stops_toolchain_capture_and_source_hashing() {
        let dir = fixture();
        let source = dir.path().join("source");
        fs::write(&source, b"approved").unwrap();
        let cancellation = Cancellation::default();
        ToolchainIdentity::capture_cancellable(dir.path(), &deadline(), &cancellation).unwrap();
        cancellation.cancel();
        assert!(
            ToolchainIdentity::capture_cancellable(dir.path(), &deadline(), &cancellation)
                .unwrap_err()
                .to_string()
                .contains("FVP run cancelled")
        );
        assert!(
            hash_file_cancellable(&source, &deadline(), &cancellation)
                .unwrap_err()
                .to_string()
                .contains("FVP run cancelled")
        );
    }
}
