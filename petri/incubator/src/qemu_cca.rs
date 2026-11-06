// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Diagnostic CCA launch path, separate from Petri's general QEMU lifecycle.

use crate::IncubatorBackend;
use crate::IncubatorOutput;
use crate::QemuCcaIncubatorConfig;
use crate::qemu;
use crate::run::RawModeGuard;
use crate::run::TerminationSignals;
use crate::run::timeout;
use anyhow::Context;
use futures_concurrency::future::Race;
use std::io::Read;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

pub(crate) fn run(config: QemuCcaIncubatorConfig) -> anyhow::Result<IncubatorOutput> {
    let start = Instant::now();
    let signals = TerminationSignals::new()?;
    let IncubatorBackend::QemuCca(profile) = &config.profile.incubator else {
        anyhow::bail!("QEMU CCA launcher requires a QEMU CCA profile");
    };
    tracing::warn!("QEMU CCA is diagnostic only; the platform is not qualified");
    fs_err::create_dir_all(&config.output_dir)?;
    let logs = tempfile::Builder::new()
        .prefix("incubator-qemu-cca-")
        .tempdir_in(&config.output_dir)?
        .keep();
    tracing::info!(path = %logs.display(), "incubator logs");
    let initrd = petri::fvp::prepare_cca_initrd(
        &config.initrd,
        &logs,
        &config.guest_pipette_path,
        &petri::fvp::CcaInitConfig::qemu(),
    )?;
    for attempt in 1..=3 {
        signals.check()?;
        let reservation =
            std::net::TcpListener::bind("127.0.0.1:0").context("reserving QEMU CCA agent port")?;
        let port = reservation.local_addr()?.port();
        let mut built = qemu::build_qemu_cca_command(
            profile,
            &config.firmware,
            &config.kernel,
            initrd.path.as_ref(),
            &config.share_dir,
            port,
            &logs,
            attempt,
        )?;
        if let Some(binary) = &config.qemu_binary_override {
            let mut command = Command::new(binary);
            command.args(built.command.get_args());
            built.command = command;
        }
        let serial = built
            .console_logs
            .get(&profile.primary_console)
            .context("QEMU CCA primary console is missing")?;
        let stderr = logs.join(format!("qemu-stderr.{attempt}.log"));
        for (name, path) in &built.console_logs {
            tracing::info!(%name, path = %path.display(), "serial log");
        }
        built
            .command
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(serial).context("creating QEMU serial log")?)
            .stderr(std::fs::File::create(&stderr).context("creating QEMU stderr log")?);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            built.command.process_group(0);
        }
        drop(reservation);
        signals.check()?;
        let mut child = ChildGuard(built.command.spawn().context("launching QEMU CCA")?);
        let mut dispatched = false;
        let result = pal_async::DefaultPool::run_with(async |driver| {
            let boot = async {
                wait_for_ready(&driver, &mut child.0, serial).await?;
                let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                let socket = pal_async::socket::PolledSocket::connect_tcp(&driver, address)
                    .await
                    .context("connecting to QEMU CCA Pipette")?;
                pipette_client::PipetteClient::new(&driver, socket, &logs)
                    .await
                    .context("connecting to QEMU CCA Pipette")
            };
            let boot = (
                boot,
                timeout(&driver, config.timeout, "QEMU CCA boot"),
                signals.cancelled(&driver),
            )
                .race()
                .await;
            let result = match boot {
                Ok(client) => {
                    dispatched = true;
                    let result = (
                        execute(&config, &client),
                        async {
                            let status = wait_for_exit(&driver, &mut child.0).await?;
                            anyhow::bail!("QEMU CCA exited during the guest command: {status}");
                        },
                        signals.cancelled(&driver),
                    )
                        .race()
                        .await;
                    if result.is_ok() {
                        if let Err(error) = (
                            client.power_off(),
                            timeout(&driver, Duration::from_secs(10), "QEMU CCA poweroff"),
                        )
                            .race()
                            .await
                        {
                            tracing::warn!(%error, "failed to request QEMU CCA poweroff");
                        }
                    }
                    result
                }
                Err(error) => Err(error),
            };
            let cleanup = child.cleanup(&driver, result.is_ok()).await;
            if let Err(error) = &cleanup {
                tracing::error!(%error, "QEMU CCA teardown failed");
            }
            cleanup?;
            result
        });
        if let Err(error) = result {
            let diagnostic = read_diagnostic(&stderr)?;
            // Retry only before dispatch: a guest command must never run twice.
            if !dispatched
                && attempt < 3
                && diagnostic.contains("Could not set up host forwarding rule")
            {
                tracing::warn!(attempt, "retrying after QEMU host-forward port collision");
                continue;
            }
            return Err(error).with_context(|| {
                format!(
                    "QEMU CCA failed; logs: {}; stderr: {diagnostic}",
                    logs.display()
                )
            });
        }
        return Ok(IncubatorOutput {
            exit_code: Some(result?),
            elapsed: start.elapsed(),
        });
    }
    anyhow::bail!("QEMU CCA exhausted its launch attempts")
}

async fn execute(
    config: &QemuCcaIncubatorConfig,
    client: &pipette_client::PipetteClient,
) -> anyhow::Result<i32> {
    let IncubatorBackend::QemuCca(profile) = &config.profile.incubator else {
        anyhow::bail!("QEMU CCA launcher requires a QEMU CCA profile");
    };
    let (program, args) = config
        .guest_command
        .split_first()
        .context("empty guest command")?;
    let mut env = config.guest_env.clone();
    qemu::publish_capabilities(&mut env, &profile.capabilities);
    let mut command = client.command(program);
    command.args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    command.current_dir(
        config
            .guest_current_dir
            .as_deref()
            .unwrap_or(crate::GUEST_SHARE_ROOT),
    );
    command.pty(config.allocate_pty);
    let guard = config.allocate_pty.then(RawModeGuard::enter).transpose()?;
    let status = command.spawn().await?.wait().await?;
    drop(guard);
    let code = status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .context("guest command returned no exit code or signal")?;
    Ok(code)
}

async fn wait_for_ready(
    driver: &pal_async::DefaultDriver,
    child: &mut Child,
    serial: &Path,
) -> anyhow::Result<()> {
    let mut log = fs_err::File::open(serial)?;
    let marker = pipette_client::PIPETTE_READY_MARKER.as_bytes();
    let mut window = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = log.read(&mut buffer)?;
        window.extend_from_slice(&buffer[..count]);
        if window.windows(marker.len()).any(|part| part == marker) {
            return Ok(());
        }
        let remove = window.len().saturating_sub(marker.len().saturating_sub(1));
        window.drain(..remove);
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("QEMU exited before Pipette was ready: {status}");
        }
        pal_async::timer::PolledTimer::new(driver)
            .sleep(Duration::from_millis(if count == 0 { 100 } else { 1 }))
            .await;
    }
}

async fn wait_for_exit(
    driver: &pal_async::DefaultDriver,
    child: &mut Child,
) -> anyhow::Result<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        pal_async::timer::PolledTimer::new(driver)
            .sleep(Duration::from_millis(100))
            .await;
    }
}

fn read_diagnostic(path: &Path) -> anyhow::Result<String> {
    let mut data = Vec::new();
    fs_err::File::open(path)?
        .take(1024 * 1024)
        .read_to_end(&mut data)?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

struct ChildGuard(Child);

impl ChildGuard {
    fn terminate(&mut self, force: bool) {
        #[cfg(unix)]
        {
            let signal = if force { "-KILL" } else { "-TERM" };
            let _ = Command::new("kill")
                .args([signal, "--", &format!("-{}", self.0.id())])
                .status();
        }
        #[cfg(windows)]
        {
            let mut command = Command::new("taskkill");
            command.args(["/PID", &self.0.id().to_string(), "/T"]);
            if force {
                command.arg("/F");
            }
            let _ = command.status();
        }
        if force {
            let _ = self.0.kill();
        }
    }

    async fn cleanup(
        &mut self,
        driver: &pal_async::DefaultDriver,
        graceful: bool,
    ) -> anyhow::Result<()> {
        if self.0.try_wait()?.is_some() {
            return Ok(());
        }
        if graceful
            && (
                wait_for_exit(driver, &mut self.0),
                timeout(driver, Duration::from_secs(30), "QEMU CCA shutdown"),
            )
                .race()
                .await
                .is_ok()
        {
            return Ok(());
        }
        self.terminate(false);
        if (
            wait_for_exit(driver, &mut self.0),
            timeout(driver, Duration::from_secs(5), "QEMU CCA termination"),
        )
            .race()
            .await
            .is_ok()
        {
            return Ok(());
        }
        self.terminate(true);
        (
            wait_for_exit(driver, &mut self.0),
            timeout(
                driver,
                Duration::from_secs(5),
                "QEMU CCA forced termination",
            ),
        )
            .race()
            .await?;
        Ok(())
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            self.terminate(true);
            let _ = self.0.try_wait();
        }
    }
}
