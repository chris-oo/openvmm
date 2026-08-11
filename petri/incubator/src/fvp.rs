// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arm FVP CCA process management.

use crate::qemu;
use crate::run::RuntimeConfig;
use crate::run::connect_and_run_via_pipette;
use anyhow::Context;
use pal_async::DefaultPool;
use serde::Deserialize;
use std::fs::File;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(0);
const PORT_COLLISION_EXIT: i32 = 75;

#[derive(Debug)]
struct PortCollision;

impl std::fmt::Display for PortCollision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FVP port forwarding collision")
    }
}

impl std::error::Error for PortCollision {}

#[derive(Deserialize)]
struct Endpoint {
    port: u16,
    run_id: String,
}

pub(crate) struct FvpRunConfig<'a> {
    pub launcher: &'a Path,
    pub platform_root: &'a Path,
    pub kernel: &'a Path,
    pub rootfs: &'a Path,
    pub guest_pipette_path: &'a str,
    pub consoles: &'a [String],
    pub primary_console: &'a str,
}

pub(crate) fn run(
    runtime: &RuntimeConfig,
    config: FvpRunConfig<'_>,
    capabilities: &[String],
) -> anyhow::Result<Option<i32>> {
    for attempt in 1..=3 {
        match run_attempt(runtime, &config, capabilities, attempt) {
            Err(error) if error.is::<PortCollision>() && attempt < 3 => {
                tracing::warn!(attempt, "retrying after FVP host-forward port collision");
            }
            result => return result,
        }
    }
    unreachable!()
}

fn run_attempt(
    runtime: &RuntimeConfig,
    config: &FvpRunConfig<'_>,
    capabilities: &[String],
    attempt: u32,
) -> anyhow::Result<Option<i32>> {
    const LOCK_TIMEOUT_MULTIPLIER: u32 = 8;
    const ENDPOINT_STARTUP_ALLOWANCE: Duration = Duration::from_secs(120);

    let start = Instant::now();
    std::fs::create_dir_all(&runtime.output_dir)?;
    let run_id = allocate_run_id();
    let run_output = runtime.output_dir.join(format!("fvp-{run_id}"));
    std::fs::create_dir(&run_output)?;
    let writable_rootfs = qemu::prepare_rootfs(config.rootfs, &run_output)?;
    let endpoint = run_output.join("endpoint.json");
    endpoint.unlink_if_exists()?;
    let stdout = File::create(run_output.join("launcher.stdout.log"))?;
    let stderr = File::create(run_output.join("launcher.stderr.log"))?;

    let mut command = Command::new("python3");
    command
        .arg(config.launcher)
        .arg("--platform-root")
        .arg(config.platform_root)
        .arg("--output-root")
        .arg(run_output.join("platform"))
        .arg("--host-kernel")
        .arg(config.kernel)
        .arg("--host-rootfs")
        .arg(&writable_rootfs)
        .arg("--share-dir")
        .arg(&runtime.share_dir)
        .arg("--launch-only")
        .arg("--endpoint-file")
        .arg(&endpoint)
        .arg("--session-timeout")
        .arg(runtime.timeout.as_secs().to_string())
        .arg("--lock-timeout")
        .arg(
            runtime
                .timeout
                .as_secs()
                .saturating_mul(LOCK_TIMEOUT_MULTIPLIER.into())
                .to_string(),
        )
        .arg("--readiness-timeout")
        .arg(runtime.timeout.as_secs().to_string())
        .arg("--guest-pipette-path")
        .arg(config.guest_pipette_path)
        .arg("--consoles")
        .arg(config.consoles.join(","))
        .arg("--primary-console")
        .arg(config.primary_console)
        .arg("--run-id")
        .arg(&run_id)
        .arg("--internal-attempt")
        .stdout(stdout)
        .stderr(stderr);
    qemu::configure_process_group(&mut command);
    if attempt > 1 {
        command.env_remove("OPENVMM_FVP_PROBE_PIPETTE_PORT");
    }
    let mut child = command.spawn().context("failed to launch FVP CCA helper")?;
    let pid = child.id();
    let mut guard = ProcessGuard { pid, armed: true };

    let result = (|| {
        let endpoint_timeout = runtime
            .timeout
            .saturating_mul(LOCK_TIMEOUT_MULTIPLIER + 1)
            .saturating_add(ENDPOINT_STARTUP_ALLOWANCE);
        let deadline = Instant::now() + endpoint_timeout;
        let endpoint = loop {
            if endpoint.is_file() {
                let contents = std::fs::read_to_string(&endpoint)?;
                let endpoint = serde_json::from_str::<Endpoint>(&contents)
                    .context("failed to parse FVP endpoint")?;
                anyhow::ensure!(
                    endpoint.run_id == run_id,
                    "FVP endpoint belongs to a different run"
                );
                break endpoint;
            }
            if let Some(status) = child.try_wait()? {
                if status.code() == Some(PORT_COLLISION_EXIT) {
                    return Err(PortCollision.into());
                }
                anyhow::bail!("FVP launcher exited before readiness: {status}")
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for FVP pipette endpoint")
            }
            std::thread::sleep(Duration::from_millis(200));
        };

        let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, endpoint.port));
        let exit_code = DefaultPool::run_with(async |driver| {
            connect_and_run_via_pipette(&driver, address, runtime, capabilities, &run_output).await
        })?;

        let shutdown_deadline = Instant::now() + runtime.timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::ensure!(status.success(), "FVP launcher failed: {status}");
                break;
            }

            if Instant::now() >= shutdown_deadline {
                anyhow::bail!("timed out waiting for FVP launcher shutdown")
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(exit_code)
    })();

    if result.is_err() {
        if child.try_wait().ok().flatten().is_some() {
            guard.armed = false;
            return result.map(Some);
        }
        qemu::terminate_process_tree(pid, false);
        let cleanup_deadline = Instant::now() + Duration::from_secs(45);
        while Instant::now() < cleanup_deadline {
            if child.try_wait().ok().flatten().is_some() {
                guard.armed = false;
                return result.map(Some);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        qemu::terminate_process_tree(pid, true);
        let _ = child.wait();
        guard.armed = false;
    } else {
        guard.armed = false;
    }
    tracing::info!(elapsed = ?start.elapsed(), "FVP CCA incubator completed");
    result.map(Some)
}

fn allocate_run_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "{}-{timestamp}-{}",
        std::process::id(),
        NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
    )
}

struct ProcessGuard {
    pid: u32,
    armed: bool,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if self.armed {
            qemu::terminate_process_tree(self.pid, true);
        }
    }
}

trait PathExt {
    fn unlink_if_exists(&self) -> std::io::Result<()>;
}

impl PathExt for std::path::PathBuf {
    fn unlink_if_exists(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}
