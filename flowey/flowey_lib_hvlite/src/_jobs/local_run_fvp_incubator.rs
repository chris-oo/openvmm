// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provision standalone FVP without the MSHV boot chain or test discovery.

use super::local_run_incubator::SourceBackend;
use super::local_run_incubator::path_str;
use super::local_run_incubator::shell_word;
use super::local_run_incubator::source_backend;
use super::local_run_incubator::stage_runtime;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub profile: CommonProfile,
        pub source_profile: String,
        pub working_dir: PathBuf,
        pub kernel: Option<PathBuf>,
        pub initrd: Option<PathBuf>,
        pub platform_root: PathBuf,
        pub package_root: Option<PathBuf>,
        pub inputs: Vec<PathBuf>,
        pub share: PathBuf,
        pub dir: PathBuf,
        pub prepare_only: bool,
        pub incubator_args: Vec<String>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_incubator::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::resolve_fvp_cca_platform::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            profile,
            source_profile,
            working_dir,
            kernel,
            initrd,
            platform_root,
            package_root,
            inputs,
            share,
            dir,
            prepare_only,
            incubator_args,
            done,
        } = request;
        anyhow::ensure!(
            source_backend(&source_profile)? == SourceBackend::Fvp,
            "FVP provisioning requires a type=fvp-cca profile"
        );
        let incubator = ctx.reqv(|incubator| crate::build_incubator::Request {
            target: CommonTriple::X86_64_LINUX_GNU,
            profile: if profile.to_release() {
                crate::run_cargo_build::BuildProfile::Release
            } else {
                crate::run_cargo_build::BuildProfile::Light
            },
            incubator,
        });
        let pipette = ctx.reqv(|pipette| crate::build_pipette::Request {
            target: CommonTriple::AARCH64_LINUX_MUSL,
            profile,
            pipette,
        });
        let package = package_root
            .map(ReadVar::from_static)
            .unwrap_or_else(|| ctx.reqv(crate::resolve_fvp_cca_platform::Request::Get));
        let payload = (kernel.is_none() || initrd.is_none())
            .then(|| ctx.reqv(crate::resolve_fvp_cca_platform::Request::GetPayload));

        ctx.emit_rust_step(
            "prepare standalone FVP Incubator profile and command",
            |ctx| {
                let incubator = incubator.claim(ctx);
                let pipette = pipette.claim(ctx);
                let package = package.claim(ctx);
                let payload = payload.claim(ctx);
                done.claim(ctx);
                move |rt| {
                    let runner = rt.read(incubator).bin;
                    let crate::build_pipette::PipetteOutput::LinuxBin { bin: pipette, .. } =
                        rt.read(pipette)
                    else {
                        anyhow::bail!("FVP requires a Linux Pipette binary");
                    };
                    let package = rt.read(package);
                    let payload = payload.map(|payload| rt.read(payload));
                    let kernel = payload_file(kernel, payload.as_deref(), "host/Image")?;
                    let initrd = payload_file(initrd, payload.as_deref(), "host/initrd")?;
                    fs_err::create_dir_all(&dir)?;
                    let runtime = stage_runtime(&dir.join("runtime"), &[("incubator", &runner)])?;
                    let runner = runtime.join("incubator");
                    let input_share =
                        prepare_input_share(&dir, &share, &pipette, &kernel, &initrd, &inputs)?;
                    let profile_path = dir.join("profile.toml");
                    let profile_file = tempfile::NamedTempFile::new_in(&dir)?;
                    fs_err::write(profile_file.path(), &source_profile)?;
                    profile_file.persist(&profile_path)?;
                    let args = command_args(
                        &profile_path,
                        &kernel,
                        &initrd,
                        &share,
                        &input_share,
                        &dir.join("results"),
                        &platform_root,
                        &package,
                        "/share/pipette",
                    )?;
                    log::info!("Prepared FVP profile: {}", profile_path.display());
                    if prepare_only {
                        let command = prepared_command(&runner, &args)?;
                        log::info!("Run directly: {command}");
                    } else {
                        rt.sh.change_dir(&working_dir);
                        flowey::shell_cmd!(rt, "{runner}")
                            .args(build_run_args(args, incubator_args))
                            .run_interactive()?;
                    }
                    Ok(())
                }
            },
        );
        Ok(())
    }
}

fn prepared_command(runner: &Path, args: &[String]) -> anyhow::Result<String> {
    Ok(std::iter::once(path_str(runner)?)
        .chain(args.iter().map(String::as_str))
        .chain(["--", "/bin/sh"])
        .map(shell_word)
        .collect::<Vec<_>>()
        .join(" "))
}

fn build_run_args(mut args: Vec<String>, forwarded: Vec<String>) -> Vec<String> {
    let mut guest_command = false;
    for arg in forwarded {
        if arg == "--" {
            guest_command = true;
        }
        if guest_command || arg != "--no-pty" {
            args.push(arg);
        }
    }
    args
}

fn prepare_input_share(
    dir: &Path,
    source_share: &Path,
    pipette: &Path,
    kernel: &Path,
    initrd: &Path,
    inputs: &[PathBuf],
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(inputs.len() <= 1024, "too many explicit FVP input files");
    let prepared = tempfile::Builder::new().prefix("inputs-").tempdir_in(dir)?;
    let mut files = std::collections::BTreeMap::from([
        (PathBuf::from("pipette"), pipette.to_owned()),
        (PathBuf::from("aarch64/Image"), kernel.to_owned()),
        (PathBuf::from("aarch64/initrd"), initrd.to_owned()),
    ]);
    for relative in inputs {
        anyhow::ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
            "FVP input must be a relative file path without traversal: {}",
            relative.display()
        );
        anyhow::ensure!(
            relative != Path::new(".openvmm-fvp-share.json")
                && !relative.starts_with("test_results"),
            "FVP input cannot name the inventory or guest output directory"
        );
        let mut source = source_share.to_owned();
        for component in relative.components() {
            source.push(component.as_os_str());
            anyhow::ensure!(
                !fs_err::symlink_metadata(&source)?.file_type().is_symlink(),
                "FVP input contains a symlink: {}",
                relative.display()
            );
        }
        anyhow::ensure!(
            !source.starts_with(dir),
            "FVP inputs must not include provisioning output"
        );
        anyhow::ensure!(
            files.insert(relative.clone(), source).is_none(),
            "duplicate or reserved FVP input: {}",
            relative.display()
        );
    }
    let inventory: Vec<_> = files.keys().cloned().collect();
    let manifest = serde_json::to_vec(&inventory)?;
    anyhow::ensure!(
        manifest.len() <= 64 * 1024,
        "FVP input inventory is too large"
    );
    for (relative, source) in files {
        anyhow::ensure!(
            fs_err::symlink_metadata(&source)?.is_file(),
            "FVP input is not a regular file: {}",
            source.display()
        );
        let destination = prepared.path().join(relative);
        fs_err::create_dir_all(destination.parent().context("FVP input has no parent")?)?;
        fs_err::copy(source, destination)?;
    }
    fs_err::write(prepared.path().join(".openvmm-fvp-share.json"), manifest)?;
    Ok(prepared.keep())
}

fn payload_file(
    explicit: Option<PathBuf>,
    payload: Option<&Path>,
    relative: &str,
) -> anyhow::Result<PathBuf> {
    let path = match explicit {
        Some(path) => path,
        None => payload
            .context("FVP payload package is required")?
            .join(relative),
    };
    anyhow::ensure!(path.is_file(), "missing FVP input file: {}", path.display());
    Ok(path)
}

#[expect(clippy::too_many_arguments)]
fn command_args(
    profile: &Path,
    kernel: &Path,
    initrd: &Path,
    share: &Path,
    input_share: &Path,
    output: &Path,
    platform: &Path,
    package: &Path,
    pipette: &str,
) -> anyhow::Result<Vec<String>> {
    let mut args = Vec::new();
    for (flag, path) in [
        ("--profile", profile),
        ("--kernel", kernel),
        ("--initrd", initrd),
        ("--share", share),
        ("--fvp-input-share", input_share),
        ("--output-dir", output),
        ("--fvp-platform-root", platform),
        ("--shrinkwrap-package-root", package),
    ] {
        args.extend([flag.into(), path_str(path)?.into()]);
    }
    args.extend(["--guest-pipette".into(), pipette.into(), "--no-pty".into()]);
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn fresh_preparation_inventories_only_pipette_payload_and_authorized_inputs()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir_in(std::env::current_dir()?)?;
        let share = root.path().join("checkout");
        let output = share.join("output");
        fs_err::create_dir_all(&output)?;
        fs_err::create_dir(share.join("bin"))?;
        fs_err::create_dir(share.join("data"))?;
        let pipette = root.path().join("built-pipette");
        let kernel = root.path().join("host-Image");
        let initrd = root.path().join("host-initrd");
        fs_err::write(&pipette, b"pipette")?;
        fs_err::write(&kernel, b"kernel")?;
        fs_err::write(&initrd, b"initrd")?;
        fs_err::write(share.join("bin/program"), b"command")?;
        fs_err::write(share.join("data/input with spaces"), b"input")?;
        fs_err::write(share.join("unlisted-secret"), b"not authorized")?;
        let prepared = prepare_input_share(
            &output,
            &share,
            &pipette,
            &kernel,
            &initrd,
            &["bin/program".into(), "data/input with spaces".into()],
        )?;
        let inventory: Vec<PathBuf> =
            serde_json::from_slice(&fs_err::read(prepared.join(".openvmm-fvp-share.json"))?)?;
        assert_eq!(
            inventory,
            [
                PathBuf::from("aarch64/Image"),
                PathBuf::from("aarch64/initrd"),
                PathBuf::from("bin/program"),
                PathBuf::from("data/input with spaces"),
                PathBuf::from("pipette"),
            ]
        );
        for path in &inventory {
            assert!(fs_err::symlink_metadata(prepared.join(path))?.is_file());
        }
        assert_eq!(fs_err::read(prepared.join("pipette"))?, b"pipette");
        assert_eq!(fs_err::read(prepared.join("bin/program"))?, b"command");
        assert!(!prepared.join("unlisted-secret").exists());
        assert!(!share.join(".openvmm-fvp-share.json").exists());
        Ok(())
    }

    #[test]
    fn preparation_rejects_recursive_escaping_and_reserved_inputs() -> anyhow::Result<()> {
        let root = tempfile::tempdir_in(std::env::current_dir()?)?;
        let share = root.path().join("checkout");
        let output = share.join("output");
        fs_err::create_dir_all(&output)?;
        fs_err::write(output.join("previous.log"), b"output")?;
        fs_err::create_dir(share.join("directory"))?;
        let source = root.path().join("trusted-build");
        fs_err::write(&source, b"built input")?;
        for input in [
            "../escape",
            "/absolute",
            "directory",
            "pipette",
            "test_results/old.log",
            ".openvmm-fvp-share.json",
            "output/previous.log",
        ] {
            assert!(
                prepare_input_share(&output, &share, &source, &source, &source, &[input.into()],)
                    .is_err(),
                "unexpectedly accepted {input}"
            );
        }
        assert_eq!(fs_err::read_dir(&output)?.count(), 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn preparation_rejects_input_links_and_directory_links() -> anyhow::Result<()> {
        let root = tempfile::tempdir_in(std::env::current_dir()?)?;
        let share = root.path().join("checkout");
        let output = share.join("output");
        fs_err::create_dir_all(&output)?;
        let source = root.path().join("source");
        fs_err::write(&source, b"source")?;
        std::os::unix::fs::symlink(&source, share.join("linked-file"))?;
        std::os::unix::fs::symlink(root.path(), share.join("linked-directory"))?;
        for input in ["linked-file", "linked-directory/source"] {
            assert!(
                prepare_input_share(&output, &share, &source, &source, &source, &[input.into()],)
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn payload_overrides_do_not_need_a_payload_package() -> anyhow::Result<()> {
        let root = tempfile::tempdir_in(std::env::current_dir()?)?;
        let kernel = root.path().join("custom kernel");
        fs_err::write(&kernel, b"kernel")?;
        assert_eq!(
            payload_file(Some(kernel.clone()), None, "host/Image")?,
            kernel
        );
        assert!(payload_file(None, None, "host/Image").is_err());
        assert!(payload_file(None, Some(root.path()), "host/Image").is_err());
        fs_err::create_dir(root.path().join("host"))?;
        fs_err::write(root.path().join("host/Image"), b"package kernel")?;
        assert_eq!(
            payload_file(None, Some(root.path()), "host/Image")?,
            root.path().join("host/Image")
        );
        Ok(())
    }

    #[test]
    fn fvp_command_keeps_paths_atomic_without_mshv_arguments() -> anyhow::Result<()> {
        let args = command_args(
            Path::new("/profile"),
            Path::new("/host kernel"),
            Path::new("/initrd"),
            Path::new("/share"),
            Path::new("/prepared inputs"),
            Path::new("/logs"),
            Path::new("/live shrinkwrap"),
            Path::new("/package"),
            "/share/runtime/pipette",
        )?;
        assert_eq!(
            args,
            [
                "--profile",
                "/profile",
                "--kernel",
                "/host kernel",
                "--initrd",
                "/initrd",
                "--share",
                "/share",
                "--fvp-input-share",
                "/prepared inputs",
                "--output-dir",
                "/logs",
                "--fvp-platform-root",
                "/live shrinkwrap",
                "--shrinkwrap-package-root",
                "/package",
                "--guest-pipette",
                "/share/runtime/pipette",
                "--no-pty",
            ]
        );
        let direct = prepared_command(Path::new("/runner with spaces/incubator"), &args)?;
        assert!(direct.starts_with("'/runner with spaces/incubator' '--profile' '/profile'"));
        assert!(direct.contains("'--fvp-input-share' '/prepared inputs'"));
        assert!(direct.ends_with("'--' '/bin/sh'"));
        assert!(!direct.contains("--map-command-path"));
        let command = ["--", "/bin/sh", "-c", "test -c /dev/kvm"].map(String::from);
        let expected = args
            .iter()
            .cloned()
            .chain(command.clone())
            .collect::<Vec<_>>();
        for redundant in [vec![], vec!["--no-pty"], vec!["--no-pty", "--no-pty"]] {
            let forwarded = redundant
                .into_iter()
                .map(String::from)
                .chain(command.clone())
                .collect();
            assert_eq!(build_run_args(args.clone(), forwarded), expected);
        }
        let forwarded = [
            "--timeout",
            "600",
            "--no-pty",
            "--guest-env",
            "MODE=--no-pty",
            "--",
            "/share/program",
            "--no-pty",
            "hello world",
        ]
        .map(String::from);
        let expected_forwarded = [
            "--timeout",
            "600",
            "--guest-env",
            "MODE=--no-pty",
            "--",
            "/share/program",
            "--no-pty",
            "hello world",
        ]
        .map(String::from);
        assert_eq!(
            build_run_args(args.clone(), forwarded.into()),
            args.into_iter()
                .chain(expected_forwarded)
                .collect::<Vec<_>>()
        );
        Ok(())
    }
}
