// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stage native OpenVMM KVM CCA artifacts into an isolated rootfs copy.

use crate::common::CommonArch;
use crate::common::CommonPlatform;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::Command;

#[derive(Serialize, Deserialize, Copy, Clone, Debug)]
pub enum StageMode {
    StageOnly,
    Preflight,
}

flowey_request! {
    pub struct Params {
        pub test_root: PathBuf,
        pub mode: StageMode,
        pub host_kernel: PathBuf,
        pub guest_kernel: Option<PathBuf>,
        pub guest_initrd: Option<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::build_kvm_cca_preflight::Node>();
        ctx.import::<crate::resolve_openvmm_test_initrd::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            test_root,
            mode,
            host_kernel,
            guest_kernel,
            guest_initrd,
            done,
        } = request;

        let target = CommonTriple::Common {
            arch: CommonArch::Aarch64,
            platform: CommonPlatform::LinuxGnu,
        };
        let openvmm = matches!(mode, StageMode::StageOnly).then(|| {
            ctx.reqv(|v| crate::build_openvmm::Request {
                params: crate::build_openvmm::OpenvmmBuildParams {
                    profile: CommonProfile::Debug,
                    target: target.clone(),
                    features: [].into(),
                },
                version: None,
                openvmm: v,
            })
        });
        let preflight = ctx.reqv(|v| crate::build_kvm_cca_preflight::Request {
            params: crate::build_kvm_cca_preflight::KvmCcaPreflightBuildParams {
                profile: CommonProfile::Debug,
                target,
            },
            preflight: v,
        });
        let guest_initrd = matches!(mode, StageMode::StageOnly).then(|| {
            guest_initrd.map_or_else(
                || {
                    ctx.reqv(|v| {
                        crate::resolve_openvmm_test_initrd::Request::Get(CommonArch::Aarch64, v)
                    })
                },
                ReadVar::from_static,
            )
        });

        ctx.emit_rust_step("stage native KVM CCA artifacts", |ctx| {
            done.claim(ctx);
            let openvmm = openvmm.map(|openvmm| openvmm.claim(ctx));
            let preflight = preflight.claim(ctx);
            let guest_initrd = guest_initrd.map(|guest_initrd| guest_initrd.claim(ctx));
            move |rt| {
                let openvmm = match openvmm {
                    Some(openvmm) => {
                        let openvmm = rt.read(openvmm);
                        Some(match openvmm {
                            crate::build_openvmm::OpenvmmOutput::LinuxBin { bin, .. } => bin,
                            crate::build_openvmm::OpenvmmOutput::WindowsBin { .. } => {
                                anyhow::bail!("expected a Linux OpenVMM binary")
                            }
                        })
                    }
                    None => None,
                };
                let preflight = rt.read(preflight).bin;
                let guest_initrd = guest_initrd.map(|guest_initrd| rt.read(guest_initrd));

                if let Some(openvmm) = &openvmm {
                    validate_regular_file(openvmm, "OpenVMM binary")?;
                }
                validate_regular_file(&preflight, "KVM CCA preflight binary")?;
                validate_regular_file(&host_kernel, "Plane0 host kernel")?;
                if let Some(guest_kernel) = &guest_kernel {
                    validate_regular_file(guest_kernel, "Realm guest kernel")?;
                }
                if let Some(guest_initrd) = &guest_initrd {
                    validate_regular_file(guest_initrd, "Realm guest initrd")?;
                }

                let home_dir = env::var("HOME").map(PathBuf::from).expect("HOME not set");
                let firmware_dir = home_dir.join(".shrinkwrap/package/cca-3world");
                let source_rootfs = firmware_dir.join("rootfs.ext2");
                validate_regular_file(&source_rootfs, "CCA shrinkwrap rootfs")?;

                let e2fsck_bin =
                    home_dir.join(".shrinkwrap/build/build/cca-3world/buildroot/host/sbin/e2fsck");
                let resize2fs_bin = home_dir
                    .join(".shrinkwrap/build/build/cca-3world/buildroot/host/sbin/resize2fs");
                let debugfs_bin = find_debugfs()?;
                validate_regular_file(&e2fsck_bin, "host e2fsck")?;
                validate_regular_file(&resize2fs_bin, "host resize2fs")?;

                let stage_dir = test_root.join("kvm-cca");
                fs::create_dir_all(&stage_dir)?;
                let _lock = LockFile::new(stage_dir.join(".stage.lock"))?;

                let rootfs_file = stage_dir.join("rootfs.ext2");
                fs::copy(&source_rootfs, &rootfs_file).with_context(|| {
                    format!(
                        "failed to copy {} to {}",
                        source_rootfs.display(),
                        rootfs_file.display()
                    )
                })?;

                run_fsck(&e2fsck_bin, &rootfs_file)?;
                run_command(
                    "resize staged rootfs",
                    Command::new(&resize2fs_bin).arg(&rootfs_file).arg("1024M"),
                )?;

                let generated_dir = stage_dir.join("generated");
                fs::create_dir_all(&generated_dir)?;
                let debugfs_input_dir = generated_dir.join("debugfs-inputs");
                fs::create_dir_all(&debugfs_input_dir)?;
                let run_script = generated_dir.join("run-openvmm-kvm-cca.sh");
                if matches!(mode, StageMode::StageOnly) {
                    fs::write(
                        &run_script,
                        format!(
                            r#"#!/bin/sh
set -eu

mkdir -p /cca/logs
echo "host: $(uname -a)" | tee /cca/logs/kvm-cca-host.log
echo "guest_kernel=/cca/guest-Image" | tee /cca/logs/kvm-cca-inputs.log
echo "guest_initrd=/cca/initrd" | tee -a /cca/logs/kvm-cca-inputs.log

/cca/kvm_cca_preflight 2>&1 | tee /cca/logs/kvm-cca-preflight.log

exec /cca/openvmm \
    --isolation cca \
    --kernel /cca/guest-Image \
    --initrd /cca/initrd \
    --device-tree \
    {extra_args} \
    2>&1 | tee /cca/logs/openvmm.log
"#,
                            extra_args = ""
                        ),
                    )?;
                }
                if matches!(mode, StageMode::StageOnly) {
                    set_executable(&run_script)?;
                }
                let init_hook = generated_dir.join(match mode {
                    StageMode::StageOnly => "S99run-openvmm-kvm-cca",
                    StageMode::Preflight => "S99kvm-cca-preflight",
                });
                let init_hook_contents = match mode {
                    StageMode::StageOnly => {
                        r#"#!/bin/sh

case "$1" in
    start|"")
        if [ -x /cca/run-openvmm-kvm-cca.sh ]; then
            exec </dev/console >/dev/console 2>&1 /cca/run-openvmm-kvm-cca.sh
        fi
        ;;
esac

exit 0
"#
                    }
                    StageMode::Preflight => {
                        r#"#!/bin/sh

case "$1" in
    start|"")
        mkdir -p /cca/logs
        if [ -x /cca/kvm_cca_preflight ]; then
            /cca/kvm_cca_preflight >/cca/logs/kvm-cca-preflight.log 2>&1
            rc=$?
            echo "$rc" >/cca/logs/kvm-cca-preflight.status
            cat /cca/logs/kvm-cca-preflight.log >/dev/console 2>&1 || true
            sync
            poweroff -f || poweroff || halt -f || halt || exit "$rc"
        fi
        ;;
esac

exit 0
"#
                    }
                };
                fs::write(&init_hook, init_hook_contents)?;
                set_executable(&init_hook)?;

                let inject_result = (|| -> anyhow::Result<()> {
                    debugfs_run(&debugfs_bin, &rootfs_file, "mkdir /cca", None)?;
                    debugfs_run(&debugfs_bin, &rootfs_file, "mkdir /cca/logs", None)?;
                    debugfs_run_allow_failure(
                        &debugfs_bin,
                        &rootfs_file,
                        "rm /etc/init.d/S99realm-launch",
                    );

                    let mut files_to_copy = vec![(&preflight, "kvm_cca_preflight")];
                    if let Some(openvmm) = &openvmm {
                        files_to_copy.push((openvmm, "openvmm"));
                    }
                    files_to_copy.push((&host_kernel, "host-Image"));
                    if let Some(guest_kernel) = &guest_kernel {
                        files_to_copy.push((guest_kernel, "guest-Image"));
                    }
                    if let Some(guest_initrd) = &guest_initrd {
                        files_to_copy.push((guest_initrd, "initrd"));
                    }
                    if matches!(mode, StageMode::StageOnly) {
                        files_to_copy.push((&run_script, "run-openvmm-kvm-cca.sh"));
                    }
                    for (src, dest_name) in files_to_copy {
                        let safe_src = debugfs_input_dir.join(dest_name);
                        fs::copy(src, &safe_src).with_context(|| {
                            format!(
                                "failed to copy {} to safe debugfs input {}",
                                src.display(),
                                safe_src.display()
                            )
                        })?;
                        debugfs_run_allow_failure(
                            &debugfs_bin,
                            &rootfs_file,
                            &format!("rm /cca/{dest_name}"),
                        );
                        debugfs_run(
                            &debugfs_bin,
                            &rootfs_file,
                            &format!("write {dest_name} /cca/{dest_name}"),
                            Some(&debugfs_input_dir),
                        )?;
                    }

                    let hook_name = init_hook.file_name().unwrap().to_string_lossy();
                    let safe_hook = debugfs_input_dir.join(hook_name.as_ref());
                    fs::copy(&init_hook, &safe_hook).with_context(|| {
                        format!(
                            "failed to copy {} to safe debugfs input {}",
                            init_hook.display(),
                            safe_hook.display()
                        )
                    })?;
                    debugfs_run_allow_failure(
                        &debugfs_bin,
                        &rootfs_file,
                        &format!("rm /etc/init.d/{hook_name}"),
                    );
                    debugfs_run(
                        &debugfs_bin,
                        &rootfs_file,
                        &format!("write {hook_name} /etc/init.d/{hook_name}"),
                        Some(&debugfs_input_dir),
                    )?;
                    Ok(())
                })();

                inject_result?;

                log::info!("staged native KVM CCA rootfs at {}", rootfs_file.display());
                if matches!(mode, StageMode::Preflight) {
                    let shrinkwrap_dir = test_root.join("shrinkwrap");
                    let venv_dir = shrinkwrap_dir.join("venv");
                    let shrinkwrap_py = shrinkwrap_dir.join("shrinkwrap/shrinkwrap.py");
                    let venv_python = venv_dir.join("bin/python3");
                    validate_regular_file(&shrinkwrap_py, "shrinkwrap.py")?;
                    validate_regular_file(&venv_python, "shrinkwrap venv python")?;
                    anyhow::ensure!(
                        venv_dir.is_dir(),
                        "shrinkwrap venv is missing at {}",
                        venv_dir.display()
                    );
                    let venv_bin_path = format!(
                        "{}:{}",
                        venv_dir.join("bin").display(),
                        env::var("PATH").unwrap_or_default()
                    );
                    flowey::shell_cmd!(
                        rt,
                        "timeout --foreground 20m {venv_python} {shrinkwrap_py} run cca-3world.yaml --rtvar ROOTFS={rootfs_file} --rtvar KERNEL={host_kernel}"
                    )
                        .env("VIRTUAL_ENV", &venv_dir)
                        .env("PATH", &venv_bin_path)
                        .run()
                        .with_context(|| "failed to launch FVP for KVM CCA preflight")?;
                    check_preflight_status(&debugfs_bin, &rootfs_file)?;
                }
                Ok(())
            }
        });

        Ok(())
    }
}

struct LockFile {
    path: PathBuf,
}

impl LockFile {
    fn new(path: PathBuf) -> anyhow::Result<Self> {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => Ok(Self { path }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                anyhow::bail!(
                    "another kvm-cca-tests staging operation is already using {}; remove it if the previous run is gone",
                    path.display()
                )
            }
            Err(err) => Err(err).with_context(|| format!("failed to create {}", path.display())),
        }
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_file(&self.path) {
            log::warn!("failed to remove lock file {}: {err}", self.path.display());
        }
    }
}

fn validate_regular_file(path: &Path, label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.is_file(),
        "{label} is missing or is not a regular file: {}",
        path.display()
    );
    Ok(())
}

fn run_command(description: &str, command: &mut Command) -> anyhow::Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to execute command to {description}"))?;
    anyhow::ensure!(
        status.success(),
        "failed to {description}: exit status {status}"
    );
    Ok(())
}

fn run_fsck(e2fsck: &Path, rootfs: &Path) -> anyhow::Result<()> {
    let status = Command::new(e2fsck)
        .arg("-fp")
        .arg(rootfs)
        .status()
        .with_context(|| format!("failed to execute {}", e2fsck.display()))?;
    let code = status.code().unwrap_or(i32::MAX);
    anyhow::ensure!(
        code <= 1,
        "failed to fsck staged rootfs {}: exit status {status}",
        rootfs.display()
    );
    Ok(())
}

fn debugfs_run(
    debugfs: &Path,
    rootfs: &Path,
    command: &str,
    current_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let mut debugfs = Command::new(debugfs);
    debugfs.arg("-w").arg("-R").arg(command).arg(rootfs);
    if let Some(current_dir) = current_dir {
        debugfs.current_dir(current_dir);
    }
    let output = debugfs
        .output()
        .with_context(|| format!("failed to execute debugfs command `{command}`"))?;
    anyhow::ensure!(
        output.status.success(),
        "debugfs command `{command}` failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn debugfs_run_allow_failure(debugfs: &Path, rootfs: &Path, command: &str) {
    if let Err(err) = debugfs_run(debugfs, rootfs, command, None) {
        log::debug!("{err:#}");
    }
}

fn debugfs_read(debugfs: &Path, rootfs: &Path, path: &str) -> anyhow::Result<Vec<u8>> {
    let output = Command::new(debugfs)
        .arg("-R")
        .arg(format!("cat {path}"))
        .arg(rootfs)
        .output()
        .with_context(|| format!("failed to execute debugfs cat for {path}"))?;
    anyhow::ensure!(
        output.status.success(),
        "failed to read {path} from {}: {}{}",
        rootfs.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn find_debugfs() -> anyhow::Result<PathBuf> {
    for path in [
        "/usr/sbin/debugfs",
        "/sbin/debugfs",
        "/usr/bin/debugfs",
        "/bin/debugfs",
    ] {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    anyhow::bail!("debugfs not found; install e2fsprogs to stage CCA rootfs without sudo")
}

fn set_executable(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn check_preflight_status(debugfs: &Path, rootfs: &Path) -> anyhow::Result<()> {
    let status_path = "/cca/logs/kvm-cca-preflight.status";
    let status = debugfs_read(debugfs, rootfs, status_path)
        .with_context(|| {
            format!("KVM CCA preflight did not complete; missing {status_path} (possible timeout or Plane0 crash)")
        })
        .and_then(|status| {
            String::from_utf8(status)
                .context("preflight status was not UTF-8")?
                .trim()
                .parse::<i32>()
                .context("preflight status was not an integer")
        })?;

    anyhow::ensure!(status == 0, "KVM CCA preflight failed with status {status}");
    Ok(())
}
