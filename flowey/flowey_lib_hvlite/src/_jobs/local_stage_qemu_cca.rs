// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stage the typed QEMU CCA platform artifacts for local validation.

use crate::common::CommonArch;
use crate::common::CommonPlatform;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

flowey_request! {
    pub struct Params {
        pub openvmm_root: PathBuf,
        pub test_root: PathBuf,
        pub output_root: PathBuf,
        pub kernels: crate::_jobs::local_stage_kvm_cca::CcaKernelSource,
        pub guest_initrd: Option<PathBuf>,
        pub openvmm_memory: String,
        pub openvmm_extra_args: Option<String>,
        pub firmware_cache_root: Option<PathBuf>,
        pub firmware_offline: bool,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::_jobs::local_stage_kvm_cca::Node>();
        ctx.import::<crate::build_cca_qemu_firmware::Node>();
        ctx.import::<crate::build_cca_qemu_host_rootfs::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::resolve_cca_qemu::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            openvmm_root,
            test_root,
            output_root,
            kernels,
            guest_initrd,
            openvmm_memory,
            openvmm_extra_args,
            firmware_cache_root,
            firmware_offline,
            done,
        } = request;

        let share_dir = output_root.join("share");
        let host_arch = match ctx.arch() {
            FlowArch::X86_64 => CommonArch::X86_64,
            FlowArch::Aarch64 => CommonArch::Aarch64,
            other => anyhow::bail!("unsupported QEMU CCA host architecture: {other:?}"),
        };
        let stage_done = ctx.reqv(|v| crate::_jobs::local_stage_kvm_cca::Params {
            test_root: test_root.clone(),
            mode: crate::_jobs::local_stage_kvm_cca::StageMode::StageInteractiveHost,
            kernels,
            guest_initrd,
            logs_dir: output_root.join("logs/fvp"),
            share_dir: share_dir.clone(),
            openvmm_memory,
            openvmm_extra_args,
            done: v,
        });

        let qemu = ctx.reqv(|v| crate::resolve_cca_qemu::Request {
            host_arch,
            output: v,
        });
        let firmware = ctx.reqv(|v| crate::build_cca_qemu_firmware::Request {
            params: crate::build_cca_qemu_firmware::CcaQemuFirmwareBuildParams {
                openvmm_root: openvmm_root.clone(),
                output_root: output_root.join("firmware"),
                cache_root: firmware_cache_root,
                offline: firmware_offline,
            },
            output: v,
        });
        let pipette = ctx.reqv(|v| crate::build_pipette::Request {
            target: CommonTriple::Common {
                arch: CommonArch::Aarch64,
                platform: CommonPlatform::LinuxMusl,
            },
            profile: CommonProfile::Debug,
            pipette: v,
        });

        let source_rootfs_path = test_root.join("kvm-cca/rootfs.ext2");
        let (source_rootfs, write_source_rootfs) = ctx.new_var();
        ctx.emit_rust_step("report staged QEMU CCA source rootfs", |ctx| {
            stage_done.clone().claim(ctx);
            let write_source_rootfs = write_source_rootfs.claim(ctx);
            move |rt| {
                anyhow::ensure!(
                    source_rootfs_path.is_file(),
                    "staged QEMU CCA source rootfs not found at {}",
                    source_rootfs_path.display()
                );
                rt.write(write_source_rootfs, &source_rootfs_path);
                Ok(())
            }
        });

        let host_rootfs = ctx.reqv(|v| crate::build_cca_qemu_host_rootfs::Request {
            params: crate::build_cca_qemu_host_rootfs::CcaQemuHostRootfsBuildParams {
                openvmm_root: openvmm_root.clone(),
                source_rootfs,
                output_root: output_root.join("host-rootfs"),
            },
            output: v,
        });

        ctx.emit_rust_step("stage QEMU CCA platform artifacts", |ctx| {
            done.claim(ctx);
            stage_done.claim(ctx);
            let qemu = qemu.claim(ctx);
            let firmware = firmware.claim(ctx);
            let pipette = pipette.claim(ctx);
            let host_rootfs = host_rootfs.claim(ctx);
            move |rt| {
                let qemu = rt.read(qemu);
                let firmware = rt.read(firmware);
                let pipette = rt.read(pipette);
                let host_rootfs = rt.read(host_rootfs);

                let qemu_dir = output_root.join("qemu");
                let kernel_dir = output_root.join("cca-kernels-v15");
                fs::create_dir_all(&qemu_dir)?;
                fs::create_dir_all(&kernel_dir)?;
                fs::create_dir_all(&share_dir)?;
                let staged_qemu = qemu_dir.join("qemu-system-aarch64");
                copy_executable(&qemu.binary, &staged_qemu)?;
                let staged_host_kernel = kernel_dir.join("host-Image");
                copy_file(
                    &test_root.join("cca-kernels-v15/host-Image"),
                    &staged_host_kernel,
                )?;
                copy_file(
                    &test_root.join("cca-kernels-v15/guest-Image"),
                    &kernel_dir.join("guest-Image"),
                )?;
                copy_file(
                    &test_root.join("cca-kernels-v15/manifest.txt"),
                    &kernel_dir.join("manifest.txt"),
                )?;

                let pipette = match pipette {
                    crate::build_pipette::PipetteOutput::LinuxBin { bin, .. } => bin,
                    crate::build_pipette::PipetteOutput::WindowsBin { .. } => {
                        anyhow::bail!("expected a Linux pipette binary")
                    }
                };
                copy_executable(&pipette, &share_dir.join("pipette"))?;

                let packaged_preflight = output_root.join("run-packaged-preflight.sh");
                fs::write(
                    &packaged_preflight,
                    launch_script(
                        &openvmm_root,
                        &output_root,
                        &staged_qemu,
                        &firmware.flash,
                        &staged_host_kernel,
                        &host_rootfs.image,
                        &share_dir,
                        "host",
                        &output_root.join("logs"),
                        None,
                        "run-qemu-cca-preflight.sh",
                    ),
                )?;
                set_executable(&packaged_preflight)?;

                let smoke = output_root.join("run-smoke.sh");
                fs::write(
                    &smoke,
                    launch_script(
                        &openvmm_root,
                        &output_root,
                        &staged_qemu,
                        &firmware.flash,
                        &staged_host_kernel,
                        &test_root.join("kvm-cca/rootfs.ext2"),
                        &share_dir,
                        "FM",
                        &output_root.join("logs/smoke"),
                        Some(&output_root.join("phase1-manifest.txt")),
                        "run-qemu-cca-smoke.sh",
                    ),
                )?;
                set_executable(&smoke)?;

                let manifest = format!(
                    "qemu={}\nfirmware={}\nhost_kernel={}\npackaged_host_rootfs={}\ninteractive_host_rootfs={}\nshare={}\n",
                    staged_qemu.display(),
                    firmware.flash.display(),
                    staged_host_kernel.display(),
                    host_rootfs.image.display(),
                    test_root.join("kvm-cca/rootfs.ext2").display(),
                    share_dir.display(),
                );
                fs::write(output_root.join("artifacts.txt"), manifest)?;
                Ok(())
            }
        });

        Ok(())
    }
}

fn copy_executable(source: &Path, destination: &Path) -> anyhow::Result<()> {
    copy_file(source, destination)?;
    set_executable(destination)
}

fn copy_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
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

#[expect(
    clippy::too_many_arguments,
    reason = "the arguments are the environment contract for the generated launcher"
)]
fn launch_script(
    openvmm_root: &Path,
    platform_root: &Path,
    qemu: &Path,
    firmware: &Path,
    host_kernel: &Path,
    host_rootfs: &Path,
    share_dir: &Path,
    mount_tag: &str,
    log_dir: &Path,
    manifest: Option<&Path>,
    runner: &str,
) -> String {
    let mut script = String::from(
        "#!/bin/bash\n# Copyright (c) Microsoft Corporation.\n# Licensed under the MIT License.\n\nset -euo pipefail\n",
    );
    for (name, value) in [
        ("QEMU_CCA_ROOT", platform_root),
        ("CCA_TEST_ROOT", platform_root),
        ("QEMU_BIN", qemu),
        ("QEMU_CCA_FIRMWARE", firmware),
        ("QEMU_CCA_HOST_KERNEL", host_kernel),
        ("QEMU_CCA_HOST_ROOTFS", host_rootfs),
        ("QEMU_CCA_SHARE_DIR", share_dir),
        ("QEMU_CCA_LOG_DIR", log_dir),
    ] {
        writeln!(script, "export {name}={}", shell_quote(value)).unwrap();
    }
    writeln!(script, "export QEMU_CCA_MOUNT_TAG={mount_tag}").unwrap();
    if runner == "run-qemu-cca-preflight.sh" {
        script.push_str("export QEMU_CCA_EXPECT_PIPETTE_READY=1\n");
    }
    if let Some(manifest) = manifest {
        writeln!(
            script,
            "export QEMU_CCA_PHASE0_MANIFEST={}",
            shell_quote(manifest)
        )
        .unwrap();
    }
    writeln!(script, "exec {}/{}", shell_quote(openvmm_root), runner).unwrap();
    script
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}
