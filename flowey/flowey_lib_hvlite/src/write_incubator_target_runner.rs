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
    "CARGO_BIN_EXE_*/p",
    "NEXTEST_BIN_EXE_*/p",
];

const NEXTEST_ARCHIVE_TMP_DIR: &str = "nextest-archive-tmp";
const DEFAULT_INCUBATOR_RUST_LOG: &str = "info";

/// Incubator platform selected at Flowey graph construction time.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncubatorPlatform {
    /// Generic direct-boot QEMU TCG platform.
    QemuTcg,
    /// QEMU Arm CCA L1 host platform.
    QemuCca,
    /// Arm FVP CCA L1 host platform.
    FvpCca,
}

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
        /// Path to the writable host rootfs image.
        pub rootfs: Option<ReadVar<PathBuf>>,
        /// Root of the locally provisioned FVP CCA platform.
        pub fvp_platform_root: Option<ReadVar<PathBuf>>,
        /// Path to the FVP lifecycle launcher.
        pub fvp_launcher: Option<ReadVar<PathBuf>>,
        /// Directory containing VMM test runtime artifacts and test outputs.
        pub test_content_dir: ReadVar<PathBuf>,
        /// Staged files that must exist beneath the test content directory.
        pub required_share_files: Vec<ReadVar<PathBuf>>,
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
            rootfs,
            fvp_platform_root,
            fvp_launcher,
            test_content_dir,
            required_share_files,
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
            let rootfs = rootfs.claim(ctx);
            let fvp_platform_root = fvp_platform_root.claim(ctx);
            let fvp_launcher = fvp_launcher.claim(ctx);
            let test_content_dir = test_content_dir.claim(ctx);
            let required_share_files = required_share_files.claim(ctx);
            let extra_env = extra_env.claim(ctx);
            let qemu_binary = qemu_binary.claim(ctx);
            let nextest_env = nextest_env.claim(ctx);

            move |rt| {
                let incubator_bin = rt.read(incubator_bin).absolute()?;
                let profile_path = rt.read(profile_path).absolute()?;
                let kernel = kernel.map(|v| rt.read(v).absolute()).transpose()?;
                let initrd = initrd.map(|v| rt.read(v).absolute()).transpose()?;
                let firmware = firmware.map(|v| rt.read(v).absolute()).transpose()?;
                let rootfs = rootfs.map(|v| rt.read(v).absolute()).transpose()?;
                let fvp_platform_root = fvp_platform_root
                    .map(|v| rt.read(v).absolute())
                    .transpose()?;
                let fvp_launcher = fvp_launcher.map(|v| rt.read(v).absolute()).transpose()?;
                let test_content_dir = rt.read(test_content_dir).absolute()?;
                let required_share_files = rt
                    .read(required_share_files)
                    .into_iter()
                    .map(|path| path.absolute().map_err(Into::into))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let extra_env = extra_env.map(|v| rt.read(v)).unwrap_or_default();
                let qemu_binary = qemu_binary.map(|v| rt.read(v).absolute()).transpose()?;

                let images_dir = extra_env.get("VMM_TEST_IMAGES").map(PathBuf::from);
                if let Some(ref images_dir) = images_dir {
                    anyhow::ensure!(
                        images_dir.starts_with(&test_content_dir),
                        "VMM test images must be staged under the test content directory"
                    );
                }
                for path in required_share_files {
                    anyhow::ensure!(
                        path.is_file() && path.starts_with(&test_content_dir),
                        "required incubator share file is not staged under {}: {}",
                        test_content_dir.display(),
                        path.display()
                    );
                }
                let share_root = test_content_dir.clone();
                let guest_test_content_dir = "/share";
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
                    rootfs: rootfs.as_deref(),
                    fvp_platform_root: fvp_platform_root.as_deref(),
                    fvp_launcher: fvp_launcher.as_deref(),
                    share_root: &share_root,
                    output_dir: &output_dir,
                    guest_pipette: &format!("{guest_test_content_dir}/pipette"),
                    guest_current_dir: guest_test_content_dir,
                    qemu_binary: qemu_binary.as_deref(),
                    tmp_dir: &tmp_dir,
                }));
                add_incubator_target_runner_env(&mut nextest, &target, &incubator_bin);

                rt.write(nextest_env, &nextest);

                Ok(())
            }
        });

        Ok(())
    }
}

/// Inputs to [`incubator_runner_env`].
struct IncubatorRunnerConfig<'a> {
    pub profile_path: &'a Path,
    pub kernel: Option<&'a Path>,
    pub initrd: Option<&'a Path>,
    pub firmware: Option<&'a Path>,
    pub rootfs: Option<&'a Path>,
    pub fvp_platform_root: Option<&'a Path>,
    pub fvp_launcher: Option<&'a Path>,
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
        rootfs,
        fvp_platform_root,
        fvp_launcher,
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
    if let Some(rootfs) = rootfs {
        env.insert("INCUBATOR_ROOTFS".into(), rootfs.display().to_string());
    }
    if let Some(fvp_platform_root) = fvp_platform_root {
        env.insert(
            "INCUBATOR_FVP_PLATFORM_ROOT".into(),
            fvp_platform_root.display().to_string(),
        );
    }
    if let Some(fvp_launcher) = fvp_launcher {
        env.insert(
            "INCUBATOR_FVP_LAUNCHER".into(),
            fvp_launcher.display().to_string(),
        );
    }
    if let Some(qemu_binary) = qemu_binary {
        env.insert(
            "INCUBATOR_QEMU_BINARY".into(),
            qemu_binary.display().to_string(),
        );
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_incubator_runner_env() {
        let env = incubator_runner_env(IncubatorRunnerConfig {
            profile_path: Path::new("/tmp/profiles/aarch64-tcg.toml"),
            kernel: Some(Path::new("/tmp/kernel Image")),
            initrd: Some(Path::new("/tmp/initrd.gz")),
            firmware: Some(Path::new("/tmp/flash.bin")),
            rootfs: Some(Path::new("/tmp/rootfs.ext4")),
            fvp_platform_root: Some(Path::new("/tmp/fvp")),
            fvp_launcher: Some(Path::new("/tmp/run-fvp.py")),
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
        assert_eq!(env.get("INCUBATOR_ROOTFS").unwrap(), "/tmp/rootfs.ext4");
        assert_eq!(env.get("INCUBATOR_FVP_PLATFORM_ROOT").unwrap(), "/tmp/fvp");
        assert_eq!(
            env.get("INCUBATOR_FVP_LAUNCHER").unwrap(),
            "/tmp/run-fvp.py"
        );
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
            rootfs: None,
            fvp_platform_root: None,
            fvp_launcher: None,
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
        assert!(!env.contains_key("INCUBATOR_ROOTFS"));
        assert!(!env.contains_key("INCUBATOR_FVP_PLATFORM_ROOT"));
        assert!(!env.contains_key("INCUBATOR_FVP_LAUNCHER"));
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
