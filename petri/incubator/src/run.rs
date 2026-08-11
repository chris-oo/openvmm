// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Top-level API to run a command inside an incubator.

use crate::profile::IncubatorBackend;
use crate::profile::IncubatorProfile;
use crate::qemu;
use anyhow::Context;
use futures::AsyncReadExt;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::task::Spawn;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

/// Configuration for an incubator run.
pub struct IncubatorConfig {
    /// The parsed profile.
    pub profile: IncubatorProfile,
    /// Path to the guest kernel image.
    pub kernel: PathBuf,
    /// Path to the base initrd (gzip-compressed CPIO).
    pub initrd: PathBuf,
    /// Directory to share into the VM at [`crate::GUEST_SHARE_ROOT`].
    pub share_dir: PathBuf,
    /// Host directory where command output and logs should be written.
    pub output_dir: PathBuf,
    /// Path to the pipette binary inside the guest.
    pub guest_pipette_path: String,
    /// The command to run inside the VM: program followed by arguments.
    pub guest_command: Vec<String>,
    /// Environment variables to set for the guest command.
    pub guest_env: BTreeMap<String, String>,
    /// Working directory for the guest command. If unset, the command inherits
    /// pipette's working directory.
    pub guest_current_dir: Option<String>,
    /// Timeout for the VM to boot and pipette to become ready. Once pipette
    /// is connected, the guest command itself runs without a timeout.
    pub timeout: Duration,
    /// If set, override the QEMU binary path specified in the profile.
    pub qemu_binary_override: Option<PathBuf>,
    /// Whether to allocate a PTY for the guest command and put the host
    /// terminal into raw mode. Disabled when running non-interactively (e.g.
    /// as a cargo-nextest target runner), where raw mode would interfere with
    /// nextest's Ctrl-C handling.
    pub allocate_pty: bool,
}

/// Configuration for an Arm CCA incubator run.
pub struct QemuCcaIncubatorConfig {
    /// The parsed QEMU CCA profile.
    pub profile: IncubatorProfile,
    /// Path to the host kernel image.
    pub kernel: PathBuf,
    /// Path to the platform firmware image.
    pub firmware: PathBuf,
    /// Path to the base writable host rootfs image.
    pub rootfs: PathBuf,
    /// Directory to share into the VM at [`crate::GUEST_SHARE_ROOT`].
    pub share_dir: PathBuf,
    /// Host directory where command output and logs should be written.
    pub output_dir: PathBuf,
    /// Path to the pipette binary inside the guest.
    pub guest_pipette_path: String,
    /// The command to run inside the VM: program followed by arguments.
    pub guest_command: Vec<String>,
    /// Environment variables to set for the guest command.
    pub guest_env: BTreeMap<String, String>,
    /// Working directory for the guest command. If unset, the command inherits
    /// pipette's working directory.
    pub guest_current_dir: Option<String>,
    /// Timeout for the VM to boot and pipette to become ready. Once pipette
    /// is connected, the guest command itself runs without a timeout.
    pub timeout: Duration,
    /// If set, override the QEMU binary path specified in the profile.
    pub qemu_binary_override: Option<PathBuf>,
    /// Whether to allocate a PTY for the guest command and put the host
    /// terminal into raw mode. Disabled when running non-interactively (e.g.
    /// as a cargo-nextest target runner), where raw mode would interfere with
    /// nextest's Ctrl-C handling.
    pub allocate_pty: bool,
}

struct RuntimeConfig {
    profile: IncubatorProfile,
    kernel: Option<PathBuf>,
    initrd: Option<PathBuf>,
    firmware: Option<PathBuf>,
    rootfs: Option<PathBuf>,
    share_dir: PathBuf,
    output_dir: PathBuf,
    guest_pipette_path: String,
    guest_command: Vec<String>,
    guest_env: BTreeMap<String, String>,
    guest_current_dir: Option<String>,
    timeout: Duration,
    qemu_binary_override: Option<PathBuf>,
    allocate_pty: bool,
}

/// Result of an incubator run.
pub struct IncubatorOutput {
    /// The guest command's exit code, if it was captured.
    pub exit_code: Option<i32>,
    /// Total wall time for the run.
    pub elapsed: Duration,
}

/// Run a command inside an incubator.
///
/// Boots an emulated VM according to the profile, mounts `share_dir` at
/// [`crate::GUEST_SHARE_ROOT`] inside the guest, connects to pipette over TCP, executes the
/// command, and returns the exit code. Stdout/stderr are relayed to the
/// host process in real time.
pub fn run_in_incubator(config: IncubatorConfig) -> anyhow::Result<IncubatorOutput> {
    anyhow::ensure!(
        matches!(config.profile.incubator, IncubatorBackend::QemuTcg(_)),
        "run_in_incubator requires a QEMU TCG profile"
    );
    run(RuntimeConfig {
        profile: config.profile,
        kernel: Some(config.kernel),
        initrd: Some(config.initrd),
        firmware: None,
        rootfs: None,
        share_dir: config.share_dir,
        output_dir: config.output_dir,
        guest_pipette_path: config.guest_pipette_path,
        guest_command: config.guest_command,
        guest_env: config.guest_env,
        guest_current_dir: config.guest_current_dir,
        timeout: config.timeout,
        qemu_binary_override: config.qemu_binary_override,
        allocate_pty: config.allocate_pty,
    })
}

/// Run a command inside an Arm CCA incubator.
pub fn run_in_qemu_cca_incubator(
    config: QemuCcaIncubatorConfig,
) -> anyhow::Result<IncubatorOutput> {
    anyhow::ensure!(
        matches!(config.profile.incubator, IncubatorBackend::QemuCca(_)),
        "run_in_qemu_cca_incubator requires a QEMU CCA profile"
    );
    run(RuntimeConfig {
        profile: config.profile,
        kernel: Some(config.kernel),
        initrd: None,
        firmware: Some(config.firmware),
        rootfs: Some(config.rootfs),
        share_dir: config.share_dir,
        output_dir: config.output_dir,
        guest_pipette_path: config.guest_pipette_path,
        guest_command: config.guest_command,
        guest_env: config.guest_env,
        guest_current_dir: config.guest_current_dir,
        timeout: config.timeout,
        qemu_binary_override: config.qemu_binary_override,
        allocate_pty: config.allocate_pty,
    })
}

fn run(config: RuntimeConfig) -> anyhow::Result<IncubatorOutput> {
    let start = Instant::now();
    for attempt in 1..=3 {
        let result = run_once(&config)?;
        if result.port_conflict && attempt < 3 {
            tracing::warn!(attempt, "retrying after QEMU host-forward port collision");
            continue;
        }
        return Ok(IncubatorOutput {
            exit_code: result.exit_code,
            elapsed: start.elapsed(),
        });
    }
    unreachable!()
}

struct AttemptResult {
    exit_code: Option<i32>,
    port_conflict: bool,
}

fn run_once(config: &RuntimeConfig) -> anyhow::Result<AttemptResult> {
    // --- pick a host port for pipette TCP forwarding ---

    let port_reservation = PortReservation::new().context("failed to find a free port")?;
    let host_port = port_reservation.port();
    let output_dir = config.output_dir.clone();
    std::fs::create_dir_all(&output_dir).context("failed to create test results dir")?;
    let instance_id = next_instance_id();

    // Keep the per-run writable boot artifact alive until QEMU exits.
    let (mut cmd, serial_log, backend_capabilities, _prepared_boot_artifact) =
        match &config.profile.incubator {
            IncubatorBackend::QemuTcg(qemu_config) => {
                let kernel = config
                    .kernel
                    .as_deref()
                    .context("QEMU TCG kernel is missing")?;
                let initrd = config
                    .initrd
                    .as_deref()
                    .context("QEMU TCG initrd is missing")?;
                let patched_initrd =
                    qemu::prepare_initrd(initrd, &output_dir, &config.guest_pipette_path)?;

                let qemu_config_override;
                let qemu_config = if let Some(ref qemu_binary) = config.qemu_binary_override {
                    qemu_config_override = crate::profile::QemuTcgConfig {
                        binary: qemu_binary.display().to_string(),
                        ..qemu_config.clone()
                    };
                    &qemu_config_override
                } else {
                    qemu_config
                };
                let cmd = qemu::build_qemu_command(
                    qemu_config,
                    &config.profile.devices,
                    kernel,
                    &patched_initrd,
                    &config.share_dir,
                    host_port,
                )?;
                let serial_log = output_dir.join(format!("incubator-serial.{instance_id}.log"));
                (cmd, serial_log, Vec::new(), patched_initrd)
            }
            IncubatorBackend::QemuCca(qemu_config) => {
                let firmware = config
                    .firmware
                    .as_deref()
                    .context("QEMU CCA firmware is missing")?;
                let kernel = config
                    .kernel
                    .as_deref()
                    .context("QEMU CCA host kernel is missing")?;
                let rootfs = config
                    .rootfs
                    .as_deref()
                    .context("QEMU CCA host rootfs is missing")?;
                let writable_rootfs = qemu::prepare_rootfs(rootfs, &output_dir)?;

                let qemu_config_override;
                let qemu_config = if let Some(ref qemu_binary) = config.qemu_binary_override {
                    qemu_config_override = crate::profile::QemuCcaConfig {
                        binary: qemu_binary.display().to_string(),
                        ..qemu_config.clone()
                    };
                    &qemu_config_override
                } else {
                    qemu_config
                };
                let built = qemu::build_qemu_cca_command(
                    qemu_config,
                    firmware,
                    kernel,
                    &writable_rootfs,
                    &config.share_dir,
                    &config.guest_pipette_path,
                    host_port,
                    &output_dir,
                    instance_id,
                )?;
                for (name, path) in &built.console_logs {
                    tracing::info!(%name, path = %path.display(), "serial log");
                }
                let serial_log = built.console_logs[&qemu_config.primary_console].clone();
                (
                    built.command,
                    serial_log,
                    qemu_config.capabilities.clone(),
                    writable_rootfs,
                )
            }
            IncubatorBackend::FvpCca(_) => {
                anyhow::bail!("FVP CCA incubator runtime support is not implemented")
            }
        };

    if matches!(config.profile.incubator, IncubatorBackend::QemuTcg(_)) {
        tracing::info!(path = %serial_log.display(), "serial log");
    }

    // QEMU runs in the background. The primary serial console goes to a pipe;
    // an async task copies output to its log and signals pipette readiness.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    qemu::configure_process_group(&mut cmd);

    drop(port_reservation);
    let mut qemu_child = cmd.spawn().context("failed to launch QEMU")?;
    let qemu_pid = qemu_child.id();
    let mut process_guard = QemuProcessGuard::new(qemu_pid);
    let qemu_stdout = qemu_child.stdout.take().expect("stdout should be piped");
    let qemu_stderr = qemu_child.stderr.take().expect("stderr should be piped");

    // --- run everything inside the async executor ---

    let result: anyhow::Result<_> = pal_async::DefaultPool::run_with(async |driver| {
        let mut qemu_child = PolledChild::<std::process::Child>::new(&driver, qemu_child)
            .context("failed to create PolledChild")?;

        // Relay serial output to the log file in a spawned task.
        // Sends a signal when pipette's "PIPETTE READY" marker appears.
        let (ready_tx, ready_rx) = mesh::oneshot::<()>();
        let serial_pipe = PolledPipe::new(&driver, qemu::child_pipe_to_file(qemu_stdout))
            .context("failed to create polled pipe for serial output")?;
        let serial_log_path = serial_log.clone();
        let relay_task = driver.spawn("serial-relay", async move {
            qemu::relay_serial_output(serial_pipe, &serial_log_path, ready_tx).await;
        });

        // Capture QEMU stderr for diagnostics.
        let stderr_pipe = PolledPipe::new(&driver, qemu::child_pipe_to_file(qemu_stderr))
            .context("failed to create polled pipe for stderr")?;
        let stderr_task = driver.spawn("qemu-stderr", async move {
            let mut buf = Vec::new();
            let mut pipe = stderr_pipe;
            let _ = pipe.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).to_string()
        });

        let session_result = run_via_pipette(
            &driver,
            host_port,
            config,
            &backend_capabilities,
            &mut qemu_child,
            ready_rx,
        )
        .await;

        let exit_code = match session_result {
            Ok(code) => Some(code),
            Err(e) => {
                tracing::error!("pipette session failed: {e:#}");
                None
            }
        };

        let cleanup_result = if exit_code.is_some() {
            if let Err(err) =
                qemu::wait_for_qemu_exit(&driver, Duration::from_secs(30), &mut qemu_child).await
            {
                tracing::warn!(error = %err, "QEMU did not shut down cleanly");
                terminate_and_wait(&driver, qemu_pid, &mut qemu_child).await
            } else {
                Ok(())
            }
        } else {
            terminate_and_wait(&driver, qemu_pid, &mut qemu_child).await
        };
        if let Err(err) = cleanup_result {
            relay_task.cancel().await;
            stderr_task.cancel().await;
            return Err(err);
        }

        // Wait for the serial relay to finish flushing.
        relay_task.await;

        // Log any QEMU stderr output.
        let stderr_output = stderr_task.await;
        let port_conflict = stderr_output.contains("Could not set up host forwarding rule");
        if !stderr_output.is_empty() {
            tracing::warn!(stderr = %stderr_output, "QEMU stderr output");
        }

        Ok((exit_code, port_conflict))
    });
    if result.is_ok() {
        process_guard.disarm();
    }

    let (exit_code, port_conflict) = result?;
    Ok(AttemptResult {
        exit_code,
        port_conflict,
    })
}

/// Connect to pipette inside the VM over TCP and execute the command.
async fn run_via_pipette(
    driver: &pal_async::DefaultDriver,
    host_port: u16,
    config: &RuntimeConfig,
    backend_capabilities: &[String],
    qemu_child: &mut PolledChild<std::process::Child>,
    ready_rx: mesh::OneshotReceiver<()>,
) -> anyhow::Result<i32> {
    // Wait for pipette to print its readiness marker on the serial
    // console, or for QEMU to exit (indicating a boot failure).
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], host_port));
    tracing::info!(%addr, "waiting for pipette ready signal");
    qemu::wait_for_pipette_ready(driver, config.timeout, qemu_child, ready_rx).await?;

    tracing::info!("pipette ready, connecting");
    let conn = pal_async::socket::PolledSocket::connect_tcp(driver, addr)
        .await
        .context("failed to connect to pipette")?;

    let output_dir = config.output_dir.clone();
    std::fs::create_dir_all(&output_dir).context("failed to create test results dir")?;

    let client = pipette_client::PipetteClient::new(&driver, conn, &output_dir)
        .await
        .context("failed to connect to pipette")?;

    tracing::info!("connected to pipette");

    // Set up VFIO devices before running the guest command.
    let runtime_env = qemu::setup_vfio_devices(&client, &config.profile.devices).await?;
    let mut command_env = config.guest_env.clone();
    qemu::publish_capabilities(&mut command_env, backend_capabilities);
    for (key, value) in runtime_env {
        if key == "PETRI_CAPABILITIES" {
            let capabilities = value.split(',').map(str::to_owned).collect::<Vec<_>>();
            qemu::publish_capabilities(&mut command_env, &capabilities);
        } else {
            command_env.insert(key, value);
        }
    }

    tracing::info!("executing command");

    let (program, args) = config
        .guest_command
        .split_first()
        .context("empty guest command")?;

    let use_pty = config.allocate_pty;

    let mut cmd = client.command(program);
    cmd.args(args);
    for (key, value) in &command_env {
        cmd.env(key, value);
    }

    if let Some(current_dir) = &config.guest_current_dir {
        cmd.current_dir(current_dir);
    }

    if use_pty {
        cmd.pty(true);
    }

    // Put the host terminal into raw mode so that Ctrl-C, etc.
    // flow through to the guest PTY instead of being handled locally.
    let raw_guard = if use_pty {
        Some(RawModeGuard::enter().context("failed to enter raw mode")?)
    } else {
        None
    };

    let result = async {
        let mut child = cmd
            .spawn()
            .await
            .context("failed to spawn command in guest")?;
        child.wait().await.context("failed to wait for command")
    }
    .await;

    // Restore terminal before printing anything.
    drop(raw_guard);

    let status = result?;
    tracing::info!(%status, "command exited");

    let exit_code = if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        tracing::warn!("command killed by signal {signal}");
        128 + signal
    } else {
        tracing::warn!("command exited with unknown status");
        1
    };

    client
        .power_off()
        .await
        .context("failed to power off incubator VM")?;

    Ok(exit_code)
}

struct PortReservation {
    listener: std::net::TcpListener,
}

impl PortReservation {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            listener: std::net::TcpListener::bind("127.0.0.1:0")
                .context("failed to bind ephemeral port")?,
        })
    }

    fn port(&self) -> u16 {
        self.listener.local_addr().unwrap().port()
    }
}

fn next_instance_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    (u64::from(std::process::id()) << 32) | NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

struct QemuProcessGuard {
    pid: u32,
    armed: bool,
}

impl QemuProcessGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for QemuProcessGuard {
    fn drop(&mut self) {
        if self.armed {
            qemu::terminate_process_tree(self.pid, true);
        }
    }
}

async fn terminate_and_wait(
    driver: &impl pal_async::driver::Driver,
    pid: u32,
    child: &mut PolledChild<std::process::Child>,
) -> anyhow::Result<()> {
    qemu::terminate_process_tree(pid, false);
    if qemu::wait_for_qemu_exit(driver, Duration::from_secs(5), child)
        .await
        .is_ok()
    {
        return Ok(());
    }
    qemu::terminate_process_tree(pid, true);
    qemu::wait_for_qemu_exit(driver, Duration::from_secs(5), child)
        .await
        .context("QEMU did not exit after forced termination")?;
    Ok(())
}

/// RAII guard that puts the terminal into raw mode and restores it on drop,
/// so that Ctrl-C and similar control sequences flow through to the guest PTY
/// instead of being interpreted by the host terminal.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode().context("failed to enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Err(e) = crossterm::terminal::disable_raw_mode() {
            tracing::warn!(error = %e, "failed to restore terminal mode");
        }
    }
}
