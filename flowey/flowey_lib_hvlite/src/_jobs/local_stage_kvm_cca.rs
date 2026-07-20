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
    InteractiveHost,
    RunOpenvmm,
}

flowey_request! {
    pub struct Params {
        pub test_root: PathBuf,
        pub mode: StageMode,
        pub host_kernel: PathBuf,
        pub guest_kernel: Option<PathBuf>,
        pub guest_initrd: Option<PathBuf>,
        pub logs_dir: PathBuf,
        pub share_dir: PathBuf,
        pub openvmm_memory: String,
        pub openvmm_extra_args: Option<String>,
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
            logs_dir,
            share_dir,
            openvmm_memory,
            openvmm_extra_args,
            done,
        } = request;

        let target = CommonTriple::Common {
            arch: CommonArch::Aarch64,
            platform: CommonPlatform::LinuxGnu,
        };
        let openvmm = matches!(
            mode,
            StageMode::StageOnly | StageMode::InteractiveHost | StageMode::RunOpenvmm
        )
        .then(|| {
            ctx.reqv(|v| crate::build_openvmm::Request {
                params: crate::build_openvmm::OpenvmmBuildParams {
                    profile: CommonProfile::Debug,
                    target: target.clone(),
                    features: [crate::build_openvmm::OpenvmmFeature::VendoredCrypto].into(),
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
        let guest_initrd = matches!(
            mode,
            StageMode::StageOnly | StageMode::InteractiveHost | StageMode::RunOpenvmm
        )
        .then(|| {
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
                validate_shell_word(&openvmm_memory, "OpenVMM memory size")?;
                if let Some(openvmm_extra_args) = &openvmm_extra_args {
                    anyhow::ensure!(
                        !openvmm_extra_args.contains('\n'),
                        "OpenVMM extra args must not contain newlines"
                    );
                }
                fs::create_dir_all(&share_dir).with_context(|| {
                    format!("failed to create 9p share directory {}", share_dir.display())
                })?;

                let home_dir = env::var("HOME").map(PathBuf::from).expect("HOME not set");
                let firmware_dir = home_dir.join(".shrinkwrap/package/cca-3world");
                let packaged_rootfs = firmware_dir.join("rootfs.ext2");
                let build_rootfs =
                    home_dir.join(".shrinkwrap/build/build/cca-3world/buildroot/images/rootfs.ext2");
                let source_rootfs = if packaged_rootfs.is_file() {
                    packaged_rootfs
                } else {
                    build_rootfs
                };
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
                let mount_share_script = generated_dir.join("mount-kvm-cca-share.sh");
                fs::write(
                    &mount_share_script,
                    r#"#!/bin/sh
set -eu
mkdir -p /cca-share
if ! mountpoint -q /cca-share; then
    mount -t 9p -o trans=virtio,version=9p2000.L FM /cca-share
fi
"#,
                )?;
                set_executable(&mount_share_script)?;
                if matches!(
                    mode,
                    StageMode::StageOnly | StageMode::InteractiveHost | StageMode::RunOpenvmm
                ) {
                    fs::write(
                        &run_script,
                        format!(
                            r#"#!/bin/sh
set -eu

VIRTIO_BLK_SIZE="${{SNP_VIRTIO_BLK_SIZE:-64M}}"

mkdir -p /cca/logs
if [ -x /cca/mount-kvm-cca-share.sh ]; then
    /cca/mount-kvm-cca-share.sh 2>&1 | tee /cca/logs/kvm-cca-share-mount.log || true
fi
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ARTIFACT_DIR=/cca
if [ "$SCRIPT_DIR" = "/cca-share" ]; then
    ARTIFACT_DIR=/cca-share
fi
KERNEL_DIR="$ARTIFACT_DIR"
if [ "$ARTIFACT_DIR" = "/cca-share" ]; then
    KERNEL_DIR=/cca
fi
echo "host: $(uname -a)" | tee /cca/logs/kvm-cca-host.log
echo "artifact_dir=$ARTIFACT_DIR" | tee /cca/logs/kvm-cca-inputs.log
echo "kernel_dir=$KERNEL_DIR" | tee -a /cca/logs/kvm-cca-inputs.log
echo "guest_kernel=$KERNEL_DIR/guest-Image" | tee -a /cca/logs/kvm-cca-inputs.log
echo "guest_initrd=$KERNEL_DIR/initrd" | tee -a /cca/logs/kvm-cca-inputs.log
echo "openvmm_memory={openvmm_memory}" | tee -a /cca/logs/kvm-cca-inputs.log
echo "openvmm_extra_args={extra_args}" | tee -a /cca/logs/kvm-cca-inputs.log

"$ARTIFACT_DIR/kvm_cca_preflight" 2>&1 | tee /cca/logs/kvm-cca-preflight.log
preflight_rc=$?
echo "$preflight_rc" >/cca/logs/kvm-cca-preflight.status
if [ "$preflight_rc" -ne 0 ]; then
    sync
    poweroff -f || poweroff || halt -f || halt || exit "$preflight_rc"
fi

dmesg >/cca/logs/host-dmesg-before-openvmm.log 2>&1 || true
if [ ! -e /etc/resolv.conf ]; then
    printf 'nameserver 10.0.0.1\n' >/etc/resolv.conf
fi
set +e
RUST_BACKTRACE=1 "$ARTIFACT_DIR/openvmm" \
    --isolation cca \
    --kernel "$KERNEL_DIR/guest-Image" \
    --initrd "$KERNEL_DIR/initrd" \
    --device-tree \
    --memory {openvmm_memory} \
    --pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255,low_mmio=4M,high_mmio=1G \
    --pcie-root-port rc0:console \
    --com1 stderr \
    --virtio-console console \
    --virtio-console-pcie-port console \
    --pcie-root-port rc0:blk \
    --virtio-blk "mem:$VIRTIO_BLK_SIZE,pcie_port=blk" \
    --pcie-root-port rc0:net \
    --virtio-net pcie_port=net:consomme \
    --cmdline "console=hvc0" \
    {extra_args} \
    2>&1 | tee /cca/logs/openvmm.log
openvmm_rc=$?
set -e
echo "$openvmm_rc" >/cca/logs/openvmm.status
dmesg >/cca/logs/host-dmesg-after-openvmm.log 2>&1 || true
sync
if [ "$ARTIFACT_DIR" = "/cca-share" ]; then
    exit "$openvmm_rc"
fi
poweroff -f || poweroff || halt -f || halt || exit "$openvmm_rc"
"#,
                            extra_args = openvmm_extra_args.as_deref().unwrap_or(""),
                            openvmm_memory = openvmm_memory,
                        ),
                    )?;
                }
                if matches!(
                    mode,
                    StageMode::StageOnly | StageMode::InteractiveHost | StageMode::RunOpenvmm
                ) {
                    set_executable(&run_script)?;
                }
                let init_hook = generated_dir.join(match mode {
                    StageMode::StageOnly => "S99run-openvmm-kvm-cca",
                    StageMode::Preflight => "S99kvm-cca-preflight",
                    StageMode::InteractiveHost => "S99kvm-cca-interactive-host",
                    StageMode::RunOpenvmm => "S99run-openvmm-kvm-cca",
                });
                let init_hook_contents = match mode {
                    StageMode::StageOnly | StageMode::RunOpenvmm => {
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
                    StageMode::InteractiveHost => {
                        r#"#!/bin/sh

case "$1" in
    start|"")
        mkdir -p /cca/logs
        if [ -x /cca/mount-kvm-cca-share.sh ]; then
            /cca/mount-kvm-cca-share.sh 2>&1 | tee /cca/logs/kvm-cca-share-mount.log || true
        fi
        {
            echo "KVM CCA interactive host is ready."
            echo "Artifacts are staged under /cca:"
            echo "  /cca/kvm_cca_preflight"
            echo "  /cca/openvmm"
            echo "  /cca/guest-Image"
            echo "  /cca/initrd"
            echo "  /cca/run-openvmm-kvm-cca.sh"
            echo "Host 9p share is mounted at /cca-share when available."
            echo "Manual commands:"
            echo "  /cca/kvm_cca_preflight"
            echo "  /cca/run-openvmm-kvm-cca.sh"
            echo "  /cca-share/run-openvmm-kvm-cca.sh"
        } | tee /cca/logs/kvm-cca-interactive-host.log
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
            {
                /cca/kvm_cca_preflight 2>&1
                echo "$?" >/cca/logs/kvm-cca-preflight.status
            } | tee /cca/logs/kvm-cca-preflight.log
            rc=$(cat /cca/logs/kvm-cca-preflight.status)
            echo "$rc" >/cca/logs/kvm-cca-preflight.status
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

                    let mut files_to_copy = vec![(&mount_share_script, "mount-kvm-cca-share.sh")];
                    if !matches!(mode, StageMode::InteractiveHost) {
                        files_to_copy.push((&preflight, "kvm_cca_preflight"));
                    }
                    if !matches!(mode, StageMode::InteractiveHost)
                        && let Some(openvmm) = &openvmm
                    {
                        files_to_copy.push((openvmm, "openvmm"));
                    }
                    files_to_copy.push((&host_kernel, "host-Image"));
                    if let Some(guest_kernel) = &guest_kernel {
                        files_to_copy.push((guest_kernel, "guest-Image"));
                    }
                    if let Some(guest_initrd) = &guest_initrd {
                        files_to_copy.push((guest_initrd, "initrd"));
                    }
                    if matches!(mode, StageMode::StageOnly | StageMode::RunOpenvmm) {
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

                stage_share_dir(
                    &share_dir,
                    openvmm.as_deref(),
                    &preflight,
                    guest_kernel.as_deref(),
                    guest_initrd.as_deref(),
                    &run_script,
                )?;

                log::info!("staged native KVM CCA rootfs at {}", rootfs_file.display());
                log::info!("staged native KVM CCA 9p share at {}", share_dir.display());
                if matches!(
                    mode,
                    StageMode::Preflight | StageMode::InteractiveHost | StageMode::RunOpenvmm
                ) {
                    let shrinkwrap_dir = test_root.join("shrinkwrap");
                    let venv_dir = shrinkwrap_dir.join("venv");
                    let shrinkwrap_bin = venv_dir.join("bin/shrinkwrap");
                    validate_regular_file(&shrinkwrap_bin, "shrinkwrap executable")?;
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
                    if matches!(mode, StageMode::InteractiveHost) {
                        print_interactive_host_instructions(&rootfs_file, &logs_dir);
                    }
                    let fvp_command = if matches!(mode, StageMode::InteractiveHost) {
                        flowey::shell_cmd!(
                            rt,
                            "{shrinkwrap_bin} --runtime=docker --image=shrinkwraptool/base-slim:2026.3.0.dev0 run cca-3world.yaml --rtvar ROOTFS={rootfs_file} --rtvar KERNEL={host_kernel} --rtvar SHARE={share_dir}"
                        )
                    } else {
                        flowey::shell_cmd!(
                            rt,
                            "timeout --foreground 20m {shrinkwrap_bin} --runtime=docker --image=shrinkwraptool/base-slim:2026.3.0.dev0 run cca-3world.yaml --rtvar ROOTFS={rootfs_file} --rtvar KERNEL={host_kernel} --rtvar SHARE={share_dir}"
                        )
                    };
                    let fvp_result = fvp_command
                        .env("VIRTUAL_ENV", &venv_dir)
                        .env("PATH", &venv_bin_path)
                        .run();
                    extract_logs(&debugfs_bin, &rootfs_file, &logs_dir)?;
                    fvp_result.with_context(|| {
                        format!(
                            "failed to launch FVP for KVM CCA; logs extracted to {}",
                            logs_dir.display()
                        )
                    })?;
                    if matches!(mode, StageMode::InteractiveHost) {
                        return Ok(());
                    }
                    check_preflight_status(&debugfs_bin, &rootfs_file)?;
                    if matches!(mode, StageMode::RunOpenvmm) {
                        check_status_file(
                            &debugfs_bin,
                            &rootfs_file,
                            "/cca/logs/openvmm.status",
                            "OpenVMM",
                        )?;
                    }
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

fn validate_shell_word(value: &str, label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!value.is_empty(), "{label} must not be empty");
    anyhow::ensure!(
        !value.chars().any(char::is_whitespace),
        "{label} must not contain whitespace"
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

fn debugfs_dump_optional(
    debugfs: &Path,
    rootfs: &Path,
    guest_path: &str,
    host_path: &Path,
) -> anyhow::Result<bool> {
    let _ = fs::remove_file(host_path);
    let output = Command::new(debugfs)
        .arg("-R")
        .arg(format!("dump -p {guest_path} {}", host_path.display()))
        .arg(rootfs)
        .output()
        .with_context(|| format!("failed to execute debugfs dump for {guest_path}"))?;

    if output.status.success() {
        return Ok(true);
    }

    log::debug!(
        "did not extract {} from {}: {}{}",
        guest_path,
        rootfs.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(false)
}

fn extract_logs(debugfs: &Path, rootfs: &Path, logs_dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(logs_dir)
        .with_context(|| format!("failed to create log directory {}", logs_dir.display()))?;

    let logs = [
        ("kvm-cca-host.log", "/cca/logs/kvm-cca-host.log"),
        ("kvm-cca-inputs.log", "/cca/logs/kvm-cca-inputs.log"),
        (
            "kvm-cca-interactive-host.log",
            "/cca/logs/kvm-cca-interactive-host.log",
        ),
        (
            "kvm-cca-share-mount.log",
            "/cca/logs/kvm-cca-share-mount.log",
        ),
        ("kvm-cca-preflight.log", "/cca/logs/kvm-cca-preflight.log"),
        (
            "kvm-cca-preflight.status",
            "/cca/logs/kvm-cca-preflight.status",
        ),
        (
            "host-dmesg-before-openvmm.log",
            "/cca/logs/host-dmesg-before-openvmm.log",
        ),
        (
            "host-dmesg-after-openvmm.log",
            "/cca/logs/host-dmesg-after-openvmm.log",
        ),
        ("openvmm.log", "/cca/logs/openvmm.log"),
        ("openvmm.status", "/cca/logs/openvmm.status"),
    ];

    let mut extracted = Vec::new();
    for (host_name, guest_path) in logs {
        let host_path = logs_dir.join(host_name);
        if debugfs_dump_optional(debugfs, rootfs, guest_path, &host_path)? {
            extracted.push(host_name);
        }
    }

    if extracted.is_empty() {
        log::warn!("no KVM CCA logs were extracted to {}", logs_dir.display());
    } else {
        log::info!(
            "extracted KVM CCA logs to {}: {}",
            logs_dir.display(),
            extracted.join(", ")
        );
    }

    Ok(())
}

fn stage_share_dir(
    share_dir: &Path,
    openvmm: Option<&Path>,
    preflight: &Path,
    guest_kernel: Option<&Path>,
    guest_initrd: Option<&Path>,
    run_script: &Path,
) -> anyhow::Result<()> {
    fs::create_dir_all(share_dir).with_context(|| {
        format!(
            "failed to create 9p share directory {}",
            share_dir.display()
        )
    })?;

    let mut files_to_copy = vec![(preflight, "kvm_cca_preflight")];
    if let Some(openvmm) = openvmm {
        files_to_copy.push((openvmm, "openvmm"));
    }
    if let Some(guest_kernel) = guest_kernel {
        files_to_copy.push((guest_kernel, "guest-Image"));
    }
    if let Some(guest_initrd) = guest_initrd {
        files_to_copy.push((guest_initrd, "initrd"));
    }
    files_to_copy.push((run_script, "run-openvmm-kvm-cca.sh"));

    for (src, dest_name) in files_to_copy {
        let dest = share_dir.join(dest_name);
        let same_file = src.canonicalize().ok() == dest.canonicalize().ok();
        if !same_file {
            fs::copy(src, &dest).with_context(|| {
                format!("failed to copy {} to {}", src.display(), dest.display())
            })?;
        }
        set_executable(&dest)?;
    }

    Ok(())
}

fn print_interactive_host_instructions(rootfs: &Path, logs_dir: &Path) {
    println!("KVM CCA interactive host is starting under FVP.");
    println!("Staged rootfs: {}", rootfs.display());
    println!("Logs will be extracted to: {}", logs_dir.display());
    println!("After Plane0 boots, use the FVP/Plane0 console or SSH to run:");
    println!("  /cca/kvm_cca_preflight");
    println!("  /cca/run-openvmm-kvm-cca.sh");
    println!("  /cca-share/run-openvmm-kvm-cca.sh");
    println!("If SSH is available, try: ssh -p 8022 root@localhost");
    println!("Stop FVP when finished; xflowey will then extract any /cca/logs files.");
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
    check_status_file(
        debugfs,
        rootfs,
        "/cca/logs/kvm-cca-preflight.status",
        "KVM CCA preflight",
    )
}

fn check_status_file(
    debugfs: &Path,
    rootfs: &Path,
    status_path: &str,
    label: &'static str,
) -> anyhow::Result<()> {
    let status = debugfs_read(debugfs, rootfs, status_path)
        .with_context(|| {
            format!(
                "{label} did not complete; missing {status_path} (possible timeout or Plane0 crash)"
            )
        })
        .and_then(|status| {
            String::from_utf8(status)
                .context("preflight status was not UTF-8")?
                .trim()
                .parse::<i32>()
                .context("preflight status was not an integer")
        })?;

    anyhow::ensure!(status == 0, "{label} failed with status {status}");
    Ok(())
}
