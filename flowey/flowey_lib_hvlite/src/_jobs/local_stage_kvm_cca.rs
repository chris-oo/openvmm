// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stage native OpenVMM KVM CCA artifacts into an isolated rootfs copy.

use crate::common::CommonArch;
use crate::common::CommonPlatform;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

flowey_request! {
    pub struct Params {
        pub test_root: PathBuf,
        pub host_kernel: PathBuf,
        pub guest_kernel: PathBuf,
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
            host_kernel,
            guest_kernel,
            guest_initrd,
            done,
        } = request;

        let target = CommonTriple::Common {
            arch: CommonArch::Aarch64,
            platform: CommonPlatform::LinuxGnu,
        };
        let openvmm = ctx.reqv(|v| crate::build_openvmm::Request {
            params: crate::build_openvmm::OpenvmmBuildParams {
                profile: CommonProfile::Debug,
                target: target.clone(),
                features: [].into(),
            },
            version: None,
            openvmm: v,
        });
        let preflight = ctx.reqv(|v| crate::build_kvm_cca_preflight::Request {
            params: crate::build_kvm_cca_preflight::KvmCcaPreflightBuildParams {
                profile: CommonProfile::Debug,
                target,
            },
            preflight: v,
        });
        let guest_initrd = guest_initrd.map_or_else(
            || {
                ctx.reqv(|v| {
                    crate::resolve_openvmm_test_initrd::Request::Get(CommonArch::Aarch64, v)
                })
            },
            ReadVar::from_static,
        );

        ctx.emit_rust_step("stage native KVM CCA artifacts", |ctx| {
            done.claim(ctx);
            let openvmm = openvmm.claim(ctx);
            let preflight = preflight.claim(ctx);
            let guest_initrd = guest_initrd.claim(ctx);
            move |rt| {
                let openvmm = rt.read(openvmm);
                let openvmm = match openvmm {
                    crate::build_openvmm::OpenvmmOutput::LinuxBin { bin, .. } => bin,
                    crate::build_openvmm::OpenvmmOutput::WindowsBin { .. } => {
                        anyhow::bail!("expected a Linux OpenVMM binary")
                    }
                };
                let preflight = rt.read(preflight).bin;
                let guest_initrd = rt.read(guest_initrd);

                validate_regular_file(&openvmm, "OpenVMM binary")?;
                validate_regular_file(&preflight, "KVM CCA preflight binary")?;
                validate_regular_file(&host_kernel, "Plane0 host kernel")?;
                validate_regular_file(&guest_kernel, "Realm guest kernel")?;
                validate_regular_file(&guest_initrd, "Realm guest initrd")?;

                let home_dir = env::var("HOME").map(PathBuf::from).expect("HOME not set");
                let firmware_dir = home_dir.join(".shrinkwrap/package/cca-3world");
                let source_rootfs = firmware_dir.join("rootfs.ext2");
                validate_regular_file(&source_rootfs, "CCA shrinkwrap rootfs")?;

                let e2fsck_bin =
                    home_dir.join(".shrinkwrap/build/build/cca-3world/buildroot/host/sbin/e2fsck");
                let resize2fs_bin = home_dir
                    .join(".shrinkwrap/build/build/cca-3world/buildroot/host/sbin/resize2fs");
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
                let run_script = generated_dir.join("run-openvmm-kvm-cca.sh");
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
                let init_hook = generated_dir.join("S99run-openvmm-kvm-cca");
                fs::write(
                    &init_hook,
                    r#"#!/bin/sh

case "$1" in
    start|"")
        if [ -x /cca/run-openvmm-kvm-cca.sh ]; then
            exec </dev/console >/dev/console 2>&1 /cca/run-openvmm-kvm-cca.sh
        fi
        ;;
esac

exit 0
"#,
                )?;

                let mnt_dir = stage_dir.join("mnt");
                let mut mounted = false;
                let inject_result = (|| -> anyhow::Result<()> {
                    run_sudo(
                        "create staged rootfs mount directory",
                        &[OsStr::new("mkdir"), OsStr::new("-p"), mnt_dir.as_os_str()],
                    )?;
                    run_sudo(
                        "mount staged rootfs",
                        &[
                            OsStr::new("mount"),
                            rootfs_file.as_os_str(),
                            mnt_dir.as_os_str(),
                        ],
                    )?;
                    mounted = true;

                    let cca_dir = mnt_dir.join("cca");
                    run_sudo(
                        "create /cca in staged rootfs",
                        &[OsStr::new("mkdir"), OsStr::new("-p"), cca_dir.as_os_str()],
                    )?;
                    let init_dir = mnt_dir.join("etc/init.d");
                    run_sudo(
                        "create init.d in staged rootfs",
                        &[OsStr::new("mkdir"), OsStr::new("-p"), init_dir.as_os_str()],
                    )?;
                    let tmk_hook = init_dir.join("S99realm-launch");
                    if tmk_hook.exists() {
                        run_sudo(
                            "disable TMK CCA auto-launch hook",
                            &[
                                OsStr::new("mv"),
                                tmk_hook.as_os_str(),
                                init_dir.join("S99realm-launch.disabled").as_os_str(),
                            ],
                        )?;
                    }

                    for (src, dest_name) in [
                        (&openvmm, "openvmm"),
                        (&preflight, "kvm_cca_preflight"),
                        (&host_kernel, "host-Image"),
                        (&guest_kernel, "guest-Image"),
                        (&guest_initrd, "initrd"),
                        (&run_script, "run-openvmm-kvm-cca.sh"),
                    ] {
                        run_sudo(
                            &format!("copy {} into staged rootfs", src.display()),
                            &[
                                OsStr::new("cp"),
                                src.as_os_str(),
                                cca_dir.join(dest_name).as_os_str(),
                            ],
                        )?;
                    }

                    run_sudo(
                        "install native KVM CCA init hook",
                        &[
                            OsStr::new("cp"),
                            init_hook.as_os_str(),
                            init_dir.join("S99run-openvmm-kvm-cca").as_os_str(),
                        ],
                    )?;
                    run_sudo(
                        "make native KVM CCA staged files executable",
                        &[
                            OsStr::new("chmod"),
                            OsStr::new("0755"),
                            cca_dir.join("openvmm").as_os_str(),
                            cca_dir.join("kvm_cca_preflight").as_os_str(),
                            cca_dir.join("run-openvmm-kvm-cca.sh").as_os_str(),
                            init_dir.join("S99run-openvmm-kvm-cca").as_os_str(),
                        ],
                    )?;
                    run_sudo("sync staged rootfs writes", &[OsStr::new("sync")])?;
                    Ok(())
                })();

                if mounted {
                    run_sudo(
                        "unmount staged rootfs",
                        &[OsStr::new("umount"), mnt_dir.as_os_str()],
                    )
                    .or_else(|_| {
                        run_sudo(
                            "lazy unmount staged rootfs",
                            &[OsStr::new("umount"), OsStr::new("-l"), mnt_dir.as_os_str()],
                        )
                    })
                    .context("failed to unmount staged rootfs; manual cleanup may be required")?;
                }

                if let Err(err) = run_sudo("sync host writes", &[OsStr::new("sync")]) {
                    log::warn!("{err:#}");
                }

                thread::sleep(Duration::from_secs(1));
                if mnt_dir.is_dir() {
                    if let Err(err) = run_sudo(
                        "remove staged rootfs mount directory",
                        &[OsStr::new("rmdir"), mnt_dir.as_os_str()],
                    ) {
                        log::warn!("{err:#}");
                    }
                }

                inject_result?;

                log::info!("staged native KVM CCA rootfs at {}", rootfs_file.display());
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

fn run_sudo(description: &str, args: &[&OsStr]) -> anyhow::Result<()> {
    let status = Command::new("sudo")
        .args(args)
        .status()
        .with_context(|| format!("failed to execute sudo command to {description}"))?;
    anyhow::ensure!(
        status.success(),
        "failed to {description}: exit status {status}"
    );
    Ok(())
}
