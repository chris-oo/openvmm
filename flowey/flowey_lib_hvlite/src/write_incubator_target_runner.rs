// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compute the environment that runs cargo-nextest tests in an incubator.
//!
//! Rather than generating a wrapper script, the incubator binary is itself used
//! as the cargo-nextest target runner, and all per-run configuration is plumbed
//! in via `INCUBATOR_*` environment variables (see the `incubator` crate's CLI,
//! whose options each have a matching `env =` fallback).

use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::path::Path;

const INCUBATOR_ENV_POLICY: &[&str] = &[
    "RUST_LOG",
    "RUST_BACKTRACE",
    "OPENVMM_LOG",
    "OPENVMM_SHOW_SPANS",
    "OPENVMM_LOG_SPANS",
    "PETRI_REMOTE_ARTIFACTS",
    "PETRI_REUSE_PREPPED_VHDS",
    "PETRI_IGNORE_UNSTABLE_FAILURES",
    "OPENVMM_REQUIRE_2MB_HUGETLB",
    "VMM_TESTS_CONTENT_DIR/p",
    "TEST_OUTPUT_PATH/p",
    "VMM_TEST_IMAGES/p",
    "NEXTEST_WORKSPACE_ROOT/p",
    "CARGO_MANIFEST_DIR/p",
    "CARGO_BIN_EXE_*/p",
    "NEXTEST_BIN_EXE_*/p",
];

const NEXTEST_ARCHIVE_TMP_DIR: &str = "nextest-archive-tmp";
const DEFAULT_INCUBATOR_RUST_LOG: &str = "info";
// Incubator's FVP staging reads this inventory and adds the invoked nextest
// binary in memory. Archives, cached files, and earlier outputs are not inputs.
const FVP_SHARE_MANIFEST: &str = ".openvmm-fvp-share.json";
const FVP_SHARE_INPUTS: &[&str] = &["pipette", "openvmm", "aarch64/Image", "aarch64/initrd"];

/// Incubator platform selected at Flowey graph construction time.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncubatorPlatform {
    /// Generic direct-boot QEMU TCG platform.
    QemuTcg,
    /// QEMU Arm CCA L1 host platform.
    QemuCca,
    /// Licensed Arm FVP CCA L1 host platform.
    FvpCca,
}

/// Read-only local roots. Incubator validates their complete pinned inventory.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FvpPlatformRoots {
    pub platform: PathBuf,
    pub package: PathBuf,
}

impl FvpPlatformRoots {
    /// Resolve a writable output location without creating files in either
    /// read-only input root, including when an ancestor is a symlink.
    pub fn output_directory(&self, path: &Path) -> anyhow::Result<PathBuf> {
        let roots = Self::resolve(Some(self.platform.clone()), Some(self.package.clone()))?;
        let absolute = std::path::absolute(path)?;
        anyhow::ensure!(
            !absolute
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
            "FVP output directory must not contain parent traversal"
        );
        let mut existing = absolute.as_path();
        let mut suffix = Vec::new();
        loop {
            match fs_err::symlink_metadata(existing) {
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    suffix.push(existing.file_name().context("invalid FVP output path")?);
                    existing = existing.parent().context("invalid FVP output parent")?;
                }
                Err(error) => return Err(error).context("cannot inspect FVP output path"),
            }
        }
        let mut output = fs_err::canonicalize(existing)?;
        for part in suffix.into_iter().rev() {
            output.push(part);
        }
        anyhow::ensure!(
            !output.starts_with(&roots.platform) && !output.starts_with(&roots.package),
            "FVP output directory must be outside the read-only platform and package roots"
        );
        Ok(output)
    }

    pub fn resolve(platform: Option<PathBuf>, package: Option<PathBuf>) -> anyhow::Result<Self> {
        fn root(path: Option<PathBuf>, option: &str) -> anyhow::Result<PathBuf> {
            let path = path.with_context(|| format!("FVP CCA requires {option}"))?;
            anyhow::ensure!(!path.as_os_str().is_empty(), "{option} must not be empty");
            let path = fs_err::canonicalize(&path)
                .with_context(|| format!("cannot resolve {option}: {}", path.display()))?;
            anyhow::ensure!(path.is_dir(), "{option} must name a directory");
            Ok(path)
        }
        Ok(Self {
            platform: root(platform, "--fvp-platform-root")?,
            package: root(package, "--shrinkwrap-package-root")?,
        })
    }
}

// The verified FVP share contains guest artifacts, not a repository checkout.
// In particular, nextest's host manifest and workspace paths cannot be mapped.
const FVP_INCUBATOR_ENV_POLICY: &[&str] = &[
    "RUST_LOG",
    "RUST_BACKTRACE",
    "OPENVMM_LOG",
    "OPENVMM_SHOW_SPANS",
    "OPENVMM_LOG_SPANS",
    "PETRI_REMOTE_ARTIFACTS",
    "PETRI_IGNORE_UNSTABLE_FAILURES",
    "VMM_TESTS_CONTENT_DIR/p",
    "TEST_OUTPUT_PATH/p",
];

fn cargo_target_runner_env_var(target: &target_lexicon::Triple) -> String {
    format!(
        "CARGO_TARGET_{}_RUNNER",
        target.to_string().replace('-', "_").to_ascii_uppercase()
    )
}

/// Merge the policy/runtime environment that xflowey owns into `env`: the cargo
/// target-runner pointer (the incubator binary itself), a default `RUST_LOG`,
/// and the `INCUBATOR_ENV` forwarding policy.
fn add_incubator_target_runner_env(
    env: &mut BTreeMap<String, String>,
    target: &target_lexicon::Triple,
    runner_bin: &Path,
) {
    env.insert(
        cargo_target_runner_env_var(target),
        runner_bin.display().to_string(),
    );
    env.entry("RUST_LOG".into()).or_insert_with(|| {
        std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_INCUBATOR_RUST_LOG.into())
    });
    env.insert("INCUBATOR_ENV".into(), INCUBATOR_ENV_POLICY.join(":"));
}

flowey_request! {
    pub struct Request {
        /// Path to the incubator binary.
        pub incubator_bin: ReadVar<PathBuf>,
        /// Path to the incubator profile TOML file.
        pub profile_path: ReadVar<PathBuf>,
        /// Path to the guest kernel image. If omitted, incubator auto-detects it.
        pub kernel: Option<ReadVar<PathBuf>>,
        /// Path to the base initrd. If omitted, incubator auto-detects it.
        pub initrd: Option<ReadVar<PathBuf>>,
        /// Path to the platform firmware image.
        pub firmware: Option<ReadVar<PathBuf>>,
        /// FVP uses only the prepared content directory as its guest share.
        pub fvp_roots: Option<FvpPlatformRoots>,
        /// Path to the OpenVMM repo root. Must contain any repo-relative paths
        /// referenced by the runner's environment (e.g. `NEXTEST_WORKSPACE_ROOT`,
        /// `CARGO_MANIFEST_DIR`) so they fall under the computed incubator share
        /// root and translate correctly into the guest.
        pub repo_root: ReadVar<PathBuf>,
        /// Directory containing VMM test runtime artifacts and test outputs.
        pub test_content_dir: ReadVar<PathBuf>,
        /// Additional host paths that must be visible in the incubator share.
        pub extra_share_paths: Vec<ReadVar<PathBuf>>,
        /// Additional environment variables used to discover path roots that
        /// must be visible in the incubator share.
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
        /// Path to the QEMU binary (overrides the profile's binary setting).
        pub qemu_binary: Option<ReadVar<PathBuf>>,
        /// The test target triple, used to name the `CARGO_TARGET_*_RUNNER`
        /// environment variable.
        pub target: target_lexicon::Triple,
        /// The complete cargo-nextest environment: the input `extra_env` plus
        /// the `INCUBATOR_*` configuration, `TMPDIR`, the
        /// `CARGO_TARGET_*_RUNNER` pointer, and the `INCUBATOR_ENV` policy.
        pub nextest_env: WriteVar<BTreeMap<String, String>>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            incubator_bin,
            profile_path,
            kernel,
            initrd,
            firmware,
            fvp_roots,
            repo_root,
            test_content_dir,
            extra_share_paths,
            extra_env,
            qemu_binary,
            target,
            nextest_env,
        } = request;

        ctx.emit_rust_step("compute incubator target runner env", |ctx| {
            let incubator_bin = incubator_bin.claim(ctx);
            let profile_path = profile_path.claim(ctx);
            let kernel = kernel.claim(ctx);
            let initrd = initrd.claim(ctx);
            let firmware = firmware.claim(ctx);
            let repo_root = repo_root.claim(ctx);
            let test_content_dir = test_content_dir.claim(ctx);
            let extra_share_paths = extra_share_paths.claim(ctx);
            let extra_env = extra_env.claim(ctx);
            let qemu_binary = qemu_binary.claim(ctx);
            let nextest_env = nextest_env.claim(ctx);

            move |rt| {
                let incubator_bin = rt.read(incubator_bin).absolute()?;
                let profile_path = rt.read(profile_path).absolute()?;
                let kernel = kernel.map(|v| rt.read(v).absolute()).transpose()?;
                let initrd = initrd.map(|v| rt.read(v).absolute()).transpose()?;
                let firmware = firmware.map(|v| rt.read(v).absolute()).transpose()?;
                let repo_root = rt.read(repo_root).absolute()?;
                let test_content_dir = rt.read(test_content_dir).absolute()?;
                let extra_share_paths = rt
                    .read(extra_share_paths)
                    .into_iter()
                    .map(|p| p.absolute().map_err(Into::into))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let extra_env = extra_env.map(|v| rt.read(v)).unwrap_or_default();
                let qemu_binary = qemu_binary.map(|v| rt.read(v).absolute()).transpose()?;

                let mut share_paths = vec![repo_root.as_path(), test_content_dir.as_path()];
                share_paths.extend(extra_share_paths.iter().map(|p| p.as_path()));
                let images_dir = extra_env.get("VMM_TEST_IMAGES").map(PathBuf::from);
                if let Some(ref images_dir) = images_dir {
                    share_paths.push(images_dir.as_path());
                }
                let share_root =
                    incubator_share_root(fvp_roots.is_some(), &test_content_dir, &share_paths)?;

                let guest_test_content_dir = guest_path(&share_root, &test_content_dir)?;
                let output_dir = test_content_dir.join("test_results");
                let tmp_dir = test_content_dir.join(NEXTEST_ARCHIVE_TMP_DIR);
                fs_err::create_dir_all(&output_dir)?;
                fs_err::create_dir_all(&tmp_dir)?;

                incubator_bin.make_executable()?;
                if let Some(qemu_binary) = &qemu_binary {
                    qemu_binary.make_executable()?;
                }

                let mut nextest = extra_env;
                nextest.extend(incubator_runner_env(IncubatorRunnerConfig {
                    profile_path: &profile_path,
                    kernel: kernel.as_deref(),
                    initrd: initrd.as_deref(),
                    firmware: firmware.as_deref(),
                    share_root: &share_root,
                    output_dir: &output_dir,
                    guest_pipette: &format!("{guest_test_content_dir}/pipette"),
                    guest_current_dir: &guest_test_content_dir,
                    qemu_binary: qemu_binary.as_deref(),
                    tmp_dir: &tmp_dir,
                }));
                add_incubator_target_runner_env(&mut nextest, &target, &incubator_bin);
                if let Some(roots) = &fvp_roots {
                    add_fvp_runner_env(&mut nextest, roots)?;
                    for relative in FVP_SHARE_INPUTS {
                        anyhow::ensure!(
                            fs_err::symlink_metadata(test_content_dir.join(relative))?.is_file(),
                            "FVP guest input must be a regular file: {relative}"
                        );
                    }
                    fs_err::write(
                        test_content_dir.join(FVP_SHARE_MANIFEST),
                        serde_json::to_vec(FVP_SHARE_INPUTS)?,
                    )?;
                }

                rt.write(nextest_env, &nextest);

                Ok(())
            }
        });

        Ok(())
    }
}

fn incubator_share_root(fvp: bool, content: &Path, paths: &[&Path]) -> anyhow::Result<PathBuf> {
    if fvp {
        Ok(content.to_owned())
    } else {
        common_ancestor(paths)
    }
}

fn add_fvp_runner_env(
    env: &mut BTreeMap<String, String>,
    roots: &FvpPlatformRoots,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !env.contains_key("INCUBATOR_QEMU_BINARY") && !env.contains_key("INCUBATOR_FIRMWARE"),
        "FVP CCA does not accept QEMU binary or firmware overrides"
    );
    env.insert(
        "INCUBATOR_FVP_PLATFORM_ROOT".into(),
        roots.platform.display().to_string(),
    );
    env.insert(
        "INCUBATOR_SHRINKWRAP_PACKAGE_ROOT".into(),
        roots.package.display().to_string(),
    );
    env.insert("INCUBATOR_ENV".into(), FVP_INCUBATOR_ENV_POLICY.join(":"));
    env.insert("PETRI_REMOTE_ARTIFACTS".into(), "0".into());
    Ok(())
}

/// Inputs to [`incubator_runner_env`].
struct IncubatorRunnerConfig<'a> {
    pub profile_path: &'a Path,
    pub kernel: Option<&'a Path>,
    pub initrd: Option<&'a Path>,
    pub firmware: Option<&'a Path>,
    pub share_root: &'a Path,
    pub output_dir: &'a Path,
    pub guest_pipette: &'a str,
    pub guest_current_dir: &'a str,
    pub qemu_binary: Option<&'a Path>,
    pub tmp_dir: &'a Path,
}

/// Build the per-run `INCUBATOR_*` (and `TMPDIR`) environment that configures
/// the incubator when it runs as a cargo-nextest target runner. Each variable
/// mirrors an option on the `incubator` CLI.
fn incubator_runner_env(config: IncubatorRunnerConfig<'_>) -> BTreeMap<String, String> {
    let IncubatorRunnerConfig {
        profile_path,
        kernel,
        initrd,
        firmware,
        share_root,
        output_dir,
        guest_pipette,
        guest_current_dir,
        qemu_binary,
        tmp_dir,
    } = config;

    let mut env = BTreeMap::new();
    env.insert(
        "INCUBATOR_PROFILE".into(),
        profile_path.display().to_string(),
    );
    env.insert("INCUBATOR_SHARE".into(), share_root.display().to_string());
    env.insert(
        "INCUBATOR_OUTPUT_DIR".into(),
        output_dir.display().to_string(),
    );
    env.insert("INCUBATOR_GUEST_PIPETTE".into(), guest_pipette.to_string());
    env.insert(
        "INCUBATOR_GUEST_CURRENT_DIR".into(),
        guest_current_dir.to_string(),
    );
    // The runner always receives a host command path that must be translated
    // into the guest share.
    env.insert("INCUBATOR_MAP_COMMAND_PATH".into(), "true".into());
    // Never drive an interactive PTY / raw mode under cargo-nextest; it would
    // fight nextest's own Ctrl-C handling.
    env.insert("INCUBATOR_NO_PTY".into(), "true".into());
    env.insert("TMPDIR".into(), tmp_dir.display().to_string());
    if let Some(kernel) = kernel {
        env.insert("INCUBATOR_KERNEL".into(), kernel.display().to_string());
    }
    if let Some(initrd) = initrd {
        env.insert("INCUBATOR_INITRD".into(), initrd.display().to_string());
    }
    if let Some(firmware) = firmware {
        env.insert("INCUBATOR_FIRMWARE".into(), firmware.display().to_string());
    }
    if let Some(qemu_binary) = qemu_binary {
        env.insert(
            "INCUBATOR_QEMU_BINARY".into(),
            qemu_binary.display().to_string(),
        );
    }
    env
}

fn guest_path(share_root: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path.strip_prefix(share_root).with_context(|| {
        format!(
            "{} is not under share root {}",
            path.display(),
            share_root.display()
        )
    })?;

    if relative.as_os_str().is_empty() {
        Ok("/share".to_string())
    } else {
        Ok(format!("/share/{}", relative.display()))
    }
}

fn common_ancestor(paths: &[&Path]) -> anyhow::Result<PathBuf> {
    let mut candidate = paths
        .first()
        .context("no paths for share root")?
        .to_path_buf();

    loop {
        if paths.iter().all(|path| path.starts_with(&candidate)) {
            return Ok(candidate);
        }

        if !candidate.pop() {
            anyhow::bail!("paths do not share a common root")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_fvp_roots() {
        let file = std::env::current_exe().unwrap();
        let directory = file.parent().unwrap().to_path_buf();
        let roots =
            FvpPlatformRoots::resolve(Some(directory.clone()), Some(directory.clone())).unwrap();
        assert!(roots.platform.is_absolute());
        assert_eq!(roots.platform, roots.package);
        assert!(
            roots
                .output_directory(&directory.join("generated-output"))
                .is_err()
        );
        assert!(
            roots
                .output_directory(&directory.parent().unwrap().join("separate-output"))
                .is_ok()
        );
        assert!(FvpPlatformRoots::resolve(None, Some(directory.clone())).is_err());
        assert!(FvpPlatformRoots::resolve(Some(directory.clone()), None).is_err());
        assert!(FvpPlatformRoots::resolve(Some(file), Some(directory.clone())).is_err());
        assert!(
            FvpPlatformRoots::resolve(
                Some(directory.join("missing-root")),
                Some(directory.clone())
            )
            .is_err()
        );
        assert!(FvpPlatformRoots::resolve(Some(PathBuf::new()), Some(directory)).is_err());
    }

    #[test]
    fn fvp_share_excludes_repository_and_external_archives() {
        let content = Path::new("/repo/target/content");
        let paths = [Path::new("/repo"), content, Path::new("/external/archive")];
        let share = incubator_share_root(true, content, &paths).unwrap();
        assert_eq!(share, content);
        assert_eq!(
            guest_path(
                &share,
                &content.join("nextest-archive-tmp/unpacked/target/tests")
            )
            .unwrap(),
            "/share/nextest-archive-tmp/unpacked/target/tests"
        );
        assert_eq!(
            incubator_share_root(false, content, &paths).unwrap(),
            Path::new("/")
        );
        assert_eq!(
            serde_json::to_value(FVP_SHARE_INPUTS).unwrap(),
            serde_json::json!(["pipette", "openvmm", "aarch64/Image", "aarch64/initrd"])
        );
    }

    #[test]
    fn fvp_runner_keeps_actual_backend_and_limits_guest_paths() {
        let roots = FvpPlatformRoots {
            platform: "/platform".into(),
            package: "/package".into(),
        };
        let mut env = BTreeMap::new();
        let target = target_lexicon::triple!("aarch64-unknown-linux-musl");
        add_incubator_target_runner_env(&mut env, &target, Path::new("/incubator"));
        add_fvp_runner_env(&mut env, &roots).unwrap();
        assert_eq!(env[&cargo_target_runner_env_var(&target)], "/incubator");
        assert_eq!(env["INCUBATOR_FVP_PLATFORM_ROOT"], "/platform");
        assert_eq!(env["INCUBATOR_SHRINKWRAP_PACKAGE_ROOT"], "/package");
        assert_eq!(env["PETRI_REMOTE_ARTIFACTS"], "0");
        assert_eq!(env["INCUBATOR_ENV"], FVP_INCUBATOR_ENV_POLICY.join(":"));
        assert!(!env["INCUBATOR_ENV"].contains("NEXTEST_WORKSPACE_ROOT"));
        assert!(!env["INCUBATOR_ENV"].contains("CARGO_MANIFEST_DIR"));
        assert!(!env["INCUBATOR_ENV"].contains("BIN_EXE"));
        assert!(!env.contains_key("PETRI_CAPABILITIES"));
        for key in ["INCUBATOR_QEMU_BINARY", "INCUBATOR_FIRMWARE"] {
            let mut invalid = env.clone();
            invalid.insert(key.into(), "/override".into());
            assert!(add_fvp_runner_env(&mut invalid, &roots).is_err());
        }
    }

    #[test]
    fn maps_guest_share_paths() {
        assert_eq!(
            guest_path(Path::new("/tmp/share"), Path::new("/tmp/share/bin/test")).unwrap(),
            "/share/bin/test"
        );
        assert_eq!(
            guest_path(Path::new("/tmp/share"), Path::new("/tmp/share")).unwrap(),
            "/share"
        );
    }

    #[test]
    fn builds_incubator_runner_env() {
        let env = incubator_runner_env(IncubatorRunnerConfig {
            profile_path: Path::new("/tmp/profiles/aarch64-tcg.toml"),
            kernel: Some(Path::new("/tmp/kernel Image")),
            initrd: Some(Path::new("/tmp/initrd.gz")),
            firmware: Some(Path::new("/tmp/flash.bin")),
            share_root: Path::new("/tmp/test content"),
            output_dir: Path::new("/tmp/test content/test_results"),
            guest_pipette: "/share/pipette",
            guest_current_dir: "/share",
            qemu_binary: Some(Path::new("/tmp/qemu/system-aarch64")),
            tmp_dir: Path::new("/tmp/test content/nextest-archive-tmp"),
        });

        assert_eq!(
            env.get("INCUBATOR_PROFILE").unwrap(),
            "/tmp/profiles/aarch64-tcg.toml"
        );
        assert_eq!(env.get("INCUBATOR_KERNEL").unwrap(), "/tmp/kernel Image");
        assert_eq!(env.get("INCUBATOR_INITRD").unwrap(), "/tmp/initrd.gz");
        assert_eq!(env.get("INCUBATOR_FIRMWARE").unwrap(), "/tmp/flash.bin");
        assert_eq!(env.get("INCUBATOR_SHARE").unwrap(), "/tmp/test content");
        assert_eq!(
            env.get("INCUBATOR_OUTPUT_DIR").unwrap(),
            "/tmp/test content/test_results"
        );
        assert_eq!(
            env.get("INCUBATOR_GUEST_PIPETTE").unwrap(),
            "/share/pipette"
        );
        assert_eq!(env.get("INCUBATOR_GUEST_CURRENT_DIR").unwrap(), "/share");
        assert_eq!(env.get("INCUBATOR_MAP_COMMAND_PATH").unwrap(), "true");
        assert_eq!(
            env.get("INCUBATOR_QEMU_BINARY").unwrap(),
            "/tmp/qemu/system-aarch64"
        );
        assert_eq!(
            env.get("TMPDIR").unwrap(),
            "/tmp/test content/nextest-archive-tmp"
        );
    }

    #[test]
    fn omits_optional_incubator_env() {
        let env = incubator_runner_env(IncubatorRunnerConfig {
            profile_path: Path::new("/tmp/profile.toml"),
            kernel: None,
            initrd: None,
            firmware: None,
            share_root: Path::new("/tmp/share"),
            output_dir: Path::new("/tmp/share/test_results"),
            guest_pipette: "/share/pipette",
            guest_current_dir: "/share",
            qemu_binary: None,
            tmp_dir: Path::new("/tmp/share/nextest-archive-tmp"),
        });

        assert!(!env.contains_key("INCUBATOR_KERNEL"));
        assert!(!env.contains_key("INCUBATOR_INITRD"));
        assert!(!env.contains_key("INCUBATOR_FIRMWARE"));
        assert!(!env.contains_key("INCUBATOR_QEMU_BINARY"));
    }

    #[test]
    fn builds_cargo_target_runner_env_var() {
        assert_eq!(
            cargo_target_runner_env_var(&target_lexicon::triple!("aarch64-unknown-linux-musl")),
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER"
        );
    }

    #[test]
    fn adds_incubator_target_runner_env() {
        let mut env = BTreeMap::new();
        let runner = Path::new("tmp").join("incubator");
        add_incubator_target_runner_env(
            &mut env,
            &target_lexicon::triple!("aarch64-unknown-linux-musl"),
            &runner,
        );

        assert_eq!(
            env.get("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER")
                .unwrap(),
            &runner.display().to_string()
        );
        assert_eq!(
            env.get("RUST_LOG").unwrap(),
            &std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_INCUBATOR_RUST_LOG.into())
        );
        assert_eq!(
            env.get("INCUBATOR_ENV").unwrap(),
            &INCUBATOR_ENV_POLICY.join(":")
        );
        assert!(
            !env.get("INCUBATOR_ENV")
                .unwrap()
                .contains("LD_LIBRARY_PATH")
        );
    }

    #[test]
    fn keeps_explicit_incubator_rust_log() {
        let mut env = BTreeMap::from([("RUST_LOG".into(), "warn,mesh=off".into())]);
        add_incubator_target_runner_env(
            &mut env,
            &target_lexicon::triple!("aarch64-unknown-linux-musl"),
            Path::new("/tmp/incubator"),
        );

        assert_eq!(env.get("RUST_LOG").unwrap(), "warn,mesh=off");
    }
}
