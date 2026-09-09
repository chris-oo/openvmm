// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Launch the pinned FVP with the shared CCA payload and pipette transport.

use super::lifecycle::Cancellation;
use super::lifecycle::Docker;
use super::lifecycle::LaunchConfirmation;
use super::lifecycle::PortBudget;
use super::lifecycle::RunState;
use super::lifecycle::RuntimeDirectory;
use super::lifecycle::Session;
use super::platform::PlatformManifest;
use super::platform::PlatformSources;
use super::platform::verify_model_identity;
use super::process::Deadline;
use super::process::ManagedChild;
use super::process::run_command_cancellable;
use super::staging::PreparedFvpRun;
use anyhow::Context;
use futures::AsyncReadExt;
use futures::AsyncWrite;
use futures::AsyncWriteExt;
use futures_concurrency::future::Race;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::pipe::PolledPipe;
use pal_async::socket::PolledSocket;
use pal_async::timer::PolledTimer;
use petri_artifacts_common::cca_payload::BASE_INITRD_SHA256 as INITRD_SHA256;
use petri_artifacts_common::cca_payload::LINUX_IMAGE_SHA256 as KERNEL_SHA256;
use pipette_client::PipetteClient;
use sha2::Digest;
use std::fs::File;
use std::future::Future;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::time::Duration;
use std::time::Instant;

const LOG_LIMIT: u64 = 16 * 1024 * 1024;

/// Run one nextest listing or test command in a fresh, validated FVP L1.
pub fn run(
    config: crate::run::FvpCcaIncubatorConfig,
) -> anyhow::Result<crate::run::IncubatorOutput> {
    let started = Instant::now();
    let crate::profile::IncubatorBackend::FvpCca(profile) = &config.profile.incubator else {
        anyhow::bail!("FVP runtime requires an FVP CCA profile");
    };
    profile.validate()?;
    let (program, arguments) = config
        .guest_command
        .split_first()
        .context("empty FVP guest command")?;
    let additional = [shared_program_input(program)?];
    let cancellation = Cancellation::default();
    let _signals = cancellation.install_signal_handlers()?;
    let initial = Deadline::new(seconds(profile.deadlines.validation))?;
    let manifest = PlatformManifest::pinned()?;
    let sources = PlatformSources::validate(
        &manifest,
        &config.platform_root,
        &config.shrinkwrap_package_root,
        &initial,
    )?;
    let output_base = output_path(
        &config.output_dir,
        &[&sources.platform_root, &sources.package_root],
    )?;
    verify_payload(&config.kernel, KERNEL_SHA256, &initial, &cancellation)?;
    verify_payload(&config.initrd, INITRD_SHA256, &initial, &cancellation)?;
    let directory = RuntimeDirectory::open(
        &RuntimeDirectory::default_base()?,
        &[
            &sources.platform_root,
            &sources.package_root,
            &config.share_dir,
        ],
    )?;
    let runtime_path = directory.path().to_owned();
    let lock_deadline = Deadline::new(seconds(profile.deadlines.model_lock))?;
    let lock_time = || {
        if profile.deadlines.model_lock == 0 {
            Ok(Duration::ZERO)
        } else {
            lock_deadline.remaining()
        }
    };
    let _toolchain_lock = directory.lock_toolchain(lock_time()?, &cancellation)?;
    let lock = directory.lock(lock_time()?, &cancellation)?;
    let inventory_deadline = Deadline::new(seconds(profile.deadlines.validation))?;
    let docker = connect_docker(&inventory_deadline, &cancellation)?;
    let (_, expected_launcher) = sources.shrinkwrap_command_with_identity()?;
    lock.recover(
        &docker,
        &manifest.shrinkwrap.container.digest,
        &expected_launcher,
        seconds(profile.deadlines.forced_cleanup),
        &cancellation,
    )?;
    let mut session = Session::new(
        lock,
        docker,
        manifest.shrinkwrap.container.digest.clone(),
        &expected_launcher,
        seconds(profile.deadlines.forced_cleanup),
        cancellation.clone(),
    )?;
    let run_id = session.state().run_id().as_str().to_owned();
    let output = output_base.join(format!("fvp-{run_id}"));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&output)
        .context("cannot create durable FVP output directory")?;
    tracing::info!(path = %output.display(), %run_id, "FVP run output");
    let mut prepared: Option<PreparedFvpRun> = None;
    let mut identity = None;
    let mut pool = DefaultPool::new();
    let driver = pool.driver();
    let mut client: Option<PipetteClient> = None;
    let outcome: anyhow::Result<i32> = (|| {
        prepared = Some(PreparedFvpRun::allocate_for_session(
            &sources,
            &config.share_dir,
            &inventory_deadline,
            &cancellation,
        )?);
        let staging = prepared.as_ref().context("missing FVP staging")?;
        staging.verify_unregistered(&inventory_deadline)?;
        anyhow::ensure!(
            !staging.root().starts_with(&runtime_path) && !runtime_path.starts_with(staging.root()),
            "FVP staging overlaps the persistent runtime directory"
        );
        if let Err(error) =
            session.register_workspace_with_output(staging.root(), &output, &inventory_deadline)
        {
            if session.state().workspace_path().is_none() {
                let cleanup = prepared
                    .take()
                    .context("missing unused FVP allocation")?
                    .cleanup_unregistered(&Deadline::new(seconds(
                        profile.deadlines.forced_cleanup,
                    ))?);
                return combine(Err(error), cleanup);
            }
            return Err(error);
        }
        let staging = prepared
            .as_ref()
            .context("missing registered FVP staging")?;
        identity = Some(session.prepare_inputs(&inventory_deadline, |docker, _| {
            scope_result(sources.validate_inventory(docker, &inventory_deadline, &cancellation))
        })??);
        let init_config = crate::CcaInitConfig {
            mount_tag: "FM".into(),
            network: crate::CcaHostNetwork::Dhcp {
                timeout: seconds(profile.deadlines.dhcp),
            },
        };
        let kernel_copy = snapshot_payload(
            &config.kernel,
            KERNEL_SHA256,
            &output,
            &inventory_deadline,
            &cancellation,
        )?;
        let initrd_copy = snapshot_payload(
            &config.initrd,
            INITRD_SHA256,
            &output,
            &inventory_deadline,
            &cancellation,
        )?;
        let patched = crate::prepare_cca_initrd(
            &initrd_copy,
            &output,
            &config.guest_pipette_path,
            &init_config,
        )?;
        let patched_hash = payload_hash(&patched, &inventory_deadline, &cancellation)?;
        session.prepare_inputs(&inventory_deadline, |_, _| {
            scope_result(staging.populate_inputs(
                &sources,
                (&kernel_copy, &patched),
                &config.share_dir,
                &additional,
                &inventory_deadline,
                &cancellation,
            ))
        })??;
        verify_payload(
            &staging.root().join("inputs/Image"),
            KERNEL_SHA256,
            &inventory_deadline,
            &cancellation,
        )?;
        verify_payload(
            &staging.root().join("inputs/initrd"),
            &patched_hash,
            &inventory_deadline,
            &cancellation,
        )?;
        verify_payload(
            &staging.share().join("aarch64/Image"),
            KERNEL_SHA256,
            &inventory_deadline,
            &cancellation,
        )?;
        verify_payload(
            &staging.share().join("aarch64/initrd"),
            INITRD_SHA256,
            &inventory_deadline,
            &cancellation,
        )?;
        staging.set_run_identity(&run_id)?;
        std::fs::write(
            output.join("platform-identity.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "platform": manifest,
                "kernel_sha256": KERNEL_SHA256,
                "base_initrd_sha256": INITRD_SHA256,
                "patched_initrd_sha256": patched_hash,
                "toolchain_fingerprint": identity.as_ref().context("missing validated toolchain identity")?.fingerprint(),
                "run_id": run_id,
            }))?,
        )?;
        std::fs::create_dir_all(staging.share().join("test_results"))?;
        let template = session.docker_command();
        session.prepare_inputs(&inventory_deadline, |_, state| {
            scope_result(verify_model(
                &template,
                state,
                &manifest,
                &output,
                &inventory_deadline,
                &cancellation,
            ))
        })??;
        let model_pid = std::cell::Cell::new(None);
        let selected_port = std::cell::Cell::new(0);
        let selected_attempt = std::cell::Cell::new(0u32);
        let log_offset = std::cell::Cell::new(0);
        let launcher_log = session.log_path();
        let launch_state = session.state().clone();
        let mut ports = PortBudget::new(
            seconds(profile.deadlines.port_allocation),
            profile.port_retries,
        )?;
        let port = session.launch_with_ports_scoped(
            &mut ports,
            seconds(profile.deadlines.model_start),
            |port, state, deadline| {
                scope_result((|| {
                    cancellation.check()?;
                    deadline.remaining()?;
                    selected_port.set(port);
                    let attempt = selected_attempt
                        .get()
                        .checked_add(1)
                        .context("FVP launch attempt counter overflow")?;
                    selected_attempt.set(attempt);
                    log_offset.set(match std::fs::metadata(&launcher_log) {
                        Ok(metadata) => metadata.len(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                        Err(error) => return Err(error).context("cannot inspect FVP launcher log"),
                    });
                    let mut command = sources.shrinkwrap_command()?;
                    for (name, _) in std::env::vars_os() {
                        if name.to_str().is_some_and(|name| {
                            name.starts_with("SHRINKWRAP_") || name.starts_with("TUXMAKE_")
                        }) {
                            command.env_remove(name);
                        }
                    }
                    staging.populate_command(&mut command, port, profile, attempt)?;
                    let labels = state
                        .container_labels()
                        .into_iter()
                        .map(|(key, value)| format!("--label {key}={value}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    command.env("TUXMAKE_DOCKER_RUN", labels);
                    let description = serde_json::json!({
                        "program": command.get_program(),
                        "args": command.get_args().collect::<Vec<_>>(),
                        "environment_overrides": command.get_envs().collect::<Vec<_>>(),
                        "docker_binding": state.docker_binding(),
                        "run_id": run_id,
                        "port": port,
                        "attempt": attempt,
                    });
                    std::fs::write(
                        output.join(format!("launch-command-{attempt}.json")),
                        serde_json::to_vec_pretty(&description)?,
                    )?;
                    Ok(command)
                })())
            },
            |child, deadline| {
                scope_result(
                    confirm_model(
                        child,
                        &template,
                        &launch_state,
                        &manifest,
                        &launcher_log,
                        log_offset.get(),
                        selected_port.get(),
                        deadline,
                        &cancellation,
                    )
                    .map(|(confirmation, pid)| {
                        model_pid.set(pid);
                        confirmation
                    }),
                )
            },
        )?;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let primary_log = staging.console_log(profile.primary_console, selected_attempt.get());
        let primary_output = output.join("consoles").join(
            primary_log
                .strip_prefix(staging.logs())
                .context("FVP console is outside its log directory")?,
        );
        let mut dhcp = DhcpProgress::default();
        session.wait_ready_scoped(seconds(profile.deadlines.pipette_ready), |deadline| {
            // These probes create no host subprocesses; an error closes scope.
            Ok((|| {
                let text = read_log(&primary_log)?;
                anyhow::ensure!(
                    !text.contains("Failed to load initrd")
                        && !text.contains("Failed to open file: initrd"),
                    "FVP EFI initrd lookup failed; see {}",
                    primary_output.display()
                );
                anyhow::ensure!(
                    !text.contains("CCA host initialization failed:"),
                    "FVP host initialization failed; see {}",
                    primary_output.display()
                );
                dhcp.observe(&text, seconds(profile.deadlines.dhcp))?;
                if !text.contains("PIPETTE READY") {
                    return Ok(false);
                }
                anyhow::ensure!(
                    listener_state(
                        model_pid.get().context("missing verified FVP model PID")?,
                        port
                    )? == ListenerState::Owned,
                    "FVP pipette forwarding ownership changed"
                );
                let connected =
                    pool.run_until(bounded(&driver, deadline, &cancellation, async {
                        let socket = PolledSocket::connect_tcp(&driver, address).await?;
                        let client = PipetteClient::new(&driver, socket, &output).await?;
                        let identity = client
                            .command("/bin/cat")
                            .arg(format!("{}/.incubator-run-id", crate::GUEST_SHARE_ROOT))
                            .output()
                            .await?;
                        anyhow::ensure!(
                            identity.status.code() == Some(0)
                                && std::str::from_utf8(&identity.stdout)?.trim() == run_id,
                            "FVP pipette endpoint belongs to a different run"
                        );
                        Ok(client)
                    }))?;
                client = Some(connected);
                Ok(true)
            })())
        })?;
        let connected = client
            .as_ref()
            .context("FVP readiness did not establish pipette")?;
        let exit_code = session.execute_test(seconds(profile.deadlines.test_execution), |deadline, cancellation| {
            Ok(pool.run_until(bounded(&driver, deadline, cancellation, async {
                let mut command = connected.command(program);
                command.args(arguments);
                for (key, value) in &config.guest_env {
                    anyhow::ensure!(key != "PETRI_CAPABILITIES", "FVP runtime capabilities cannot be supplied by the guest environment");
                    command.env(key, value);
                }
                command.env("PETRI_CAPABILITIES", profile.capabilities.join(","));
                if let Some(directory) = &config.guest_current_dir { command.current_dir(directory); }
                command
                    .stdin(pipette_client::process::Stdio::null())
                    .stdout(pipette_client::process::Stdio::piped())
                    .stderr(pipette_client::process::Stdio::piped());
                let mut child = command.spawn().await.context("failed to dispatch FVP guest command")?;
                let stdout = child.stdout.take().context("missing FVP command stdout pipe")?;
                let stderr = child.stderr.take().context("missing FVP command stderr pipe")?;
                let stdout_target = host_output(&driver, &std::io::stdout())?;
                let stderr_target = host_output(&driver, &std::io::stderr())?;
                let (status, (), ()) = futures::try_join!(
                    async { child.wait().await.context("failed to wait for FVP guest command") },
                    drain_command_output(stdout, output.join("command.stdout.log"), stdout_target, deadline, cancellation),
                    drain_command_output(stderr, output.join("command.stderr.log"), stderr_target, deadline, cancellation),
                )?;
                status.code().or_else(|| status.signal().map(|signal| 128 + signal))
                    .context("FVP guest command returned no exit status")
            })))
        })??;
        let shutdown =
            session.shutdown_scoped(seconds(profile.deadlines.guest_shutdown), |deadline| {
                Ok(pool.run_until(bounded(
                    &driver,
                    deadline,
                    &cancellation,
                    connected.power_off(),
                )))
            })?;
        anyhow::ensure!(
            shutdown.success(),
            "FVP launcher failed during L1 shutdown: {shutdown}"
        );
        Ok(exit_code)
    })();
    drop(client);
    let finalization = finish_session(
        &mut session,
        prepared,
        &output,
        seconds(profile.deadlines.validation),
        seconds(profile.deadlines.forced_cleanup),
        |deadline, token| {
            scope_result(match &identity {
                Some(identity) => sources.verify_toolchain_after(identity, deadline, token),
                None => Ok(()),
            })
        },
    );
    let exit_code = combine_guest_outcome(outcome, finalization)?;
    Ok(crate::run::IncubatorOutput {
        exit_code: Some(exit_code),
        elapsed: started.elapsed(),
    })
}

pub(super) fn finish_session(
    session: &mut Session,
    mut prepared: Option<PreparedFvpRun>,
    output: &Path,
    validation_budget: Duration,
    output_budget: Duration,
    verify: impl FnOnce(&Deadline, &Cancellation) -> anyhow::Result<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let stopped = session.stop_resources();
    if stopped.is_ok() {
        let post = (|| {
            let deadline = Deadline::new(validation_budget)?;
            session.post_verify(&deadline, verify)?
        })();
        let saved = (|| {
            if let Some(staging) = &prepared {
                anyhow::ensure!(
                    session.state().workspace_path() == Some(staging.root())
                        && session.state().output_path() == Some(output),
                    "FVP workspace/output registration is incomplete; preserving staging"
                );
                let deadline = Deadline::new(output_budget)?;
                session.preserve_outputs(output, &deadline)?;
            }
            Ok(())
        })();
        let retired = if saved.is_ok() {
            session.retire_state().and_then(|()| match prepared.take() {
                Some(staging) => staging.confirm_retired(),
                None => Ok(()),
            })
        } else {
            Ok(())
        };
        combine(combine(post, saved), retired)
    } else {
        stopped
    }
}

fn combine<T>(outcome: anyhow::Result<T>, cleanup: anyhow::Result<()>) -> anyhow::Result<T> {
    match (outcome, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("FVP finalization also failed: {cleanup:#}")))
        }
    }
}

fn combine_guest_outcome(
    outcome: anyhow::Result<i32>,
    finalization: anyhow::Result<()>,
) -> anyhow::Result<i32> {
    match (outcome, finalization) {
        (Ok(code), Err(error)) if code != 0 => Err(error.context(format!(
            "FVP guest command failed with exit code {code}; finalization also failed"
        ))),
        (outcome, finalization) => combine(outcome, finalization),
    }
}

fn shared_program_input(program: &str) -> anyhow::Result<PathBuf> {
    let relative = Path::new(program)
        .strip_prefix(crate::GUEST_SHARE_ROOT)
        .context("FVP command must name a snapshotted binary under the guest share")?;
    anyhow::ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
        "FVP command must name a relative file within the guest share"
    );
    Ok(relative.to_owned())
}

#[derive(Default)]
struct DhcpProgress {
    deadline: Option<Deadline>,
    completed: bool,
}

impl DhcpProgress {
    fn observe(&mut self, text: &str, budget: Duration) -> anyhow::Result<()> {
        if self.deadline.is_none() && text.contains("INCUBATOR DHCP START") {
            self.deadline = Some(Deadline::new(budget)?);
        }
        if let Some(deadline) = &self.deadline {
            if !self.completed {
                deadline
                    .remaining()
                    .context("FVP DHCP exceeded its host wall-clock deadline")?;
                self.completed = text.contains("INCUBATOR DHCP COMPLETE");
            }
        }
        Ok(())
    }
}

async fn drain_command_output(
    mut pipe: mesh::pipe::ReadPipe,
    path: PathBuf,
    mut terminal: impl AsyncWrite + Unpin,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<()> {
    let mut file = File::create(path)?;
    let mut buffer = [0u8; 8192];
    let mut total = 0u64;
    loop {
        cancellation.check()?;
        deadline.remaining()?;
        let length = pipe.read(&mut buffer).await?;
        if length == 0 {
            break;
        }
        total += length as u64;
        anyhow::ensure!(
            total <= 128 * 1024 * 1024,
            "FVP command output exceeds 128 MiB"
        );
        file.write_all(&buffer[..length])?;
        terminal.write_all(&buffer[..length]).await?;
        deadline.remaining()?;
    }
    file.flush()?;
    terminal.flush().await?;
    deadline.remaining()?;
    Ok(())
}

fn host_output(
    driver: &DefaultDriver,
    descriptor: &impl AsFd,
) -> anyhow::Result<Box<dyn AsyncWrite + Unpin>> {
    let file = File::from(descriptor.as_fd().try_clone_to_owned()?);
    let metadata = file.metadata()?;
    if metadata.is_file() {
        // Preserve the existing file offset for ordinary redirected log files.
        return Ok(Box::new(futures::io::AllowStdIo::new(file)));
    }
    if metadata.rdev() != 0 && metadata.rdev() == std::fs::metadata("/dev/null")?.rdev() {
        return Ok(Box::new(futures::io::sink()));
    }
    // Reopen rather than dup so O_NONBLOCK does not change the caller's flags.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOCTTY)
        .open(format!("/proc/self/fd/{}", descriptor.as_fd().as_raw_fd()))
        .context("cannot reopen FVP output for nonblocking forwarding")?;
    Ok(Box::new(PolledPipe::new(driver, file).context(
        "FVP output does not support asynchronous forwarding",
    )?))
}

fn scope_result<T>(result: anyhow::Result<T>) -> anyhow::Result<anyhow::Result<T>> {
    match result {
        Err(error)
            if error.is::<super::process::UnresolvedProcess>()
                || error.is::<super::process::InterruptedCommand>() =>
        {
            Err(error)
        }
        result => Ok(result),
    }
}

fn output_path(path: &Path, roots: &[&Path]) -> anyhow::Result<PathBuf> {
    let path = super::lifecycle::resolve_existing_ancestor(&std::path::absolute(path)?)?;
    for root in roots {
        anyhow::ensure!(
            !path.starts_with(root),
            "FVP output must be outside the read-only platform and package roots"
        );
    }
    super::lifecycle::validate_output_location(&path)?;
    Ok(path)
}

fn seconds(value: u64) -> Duration {
    Duration::from_secs(value)
}

#[expect(
    clippy::disallowed_methods,
    reason = "pin the exact executable selected before launch"
)]
fn executable(name: &str) -> anyhow::Result<PathBuf> {
    for directory in std::env::split_paths(&std::env::var_os("PATH").context("PATH is not set")?) {
        let candidate = directory.join(name);
        if let Ok(metadata) = candidate.metadata() {
            if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                return std::fs::canonicalize(&candidate).context("failed to resolve runtime tool");
            }
        }
    }
    anyhow::bail!("required FVP runtime tool is missing: {name}")
}

fn check_output(output: Output, operation: &str) -> anyhow::Result<Vec<u8>> {
    if let Some(signal) = output.status.signal() {
        return Err(super::process::InterruptedCommand::new(anyhow::anyhow!(
            "{operation} was interrupted by signal {signal}"
        ))
        .into());
    }
    anyhow::ensure!(
        output.status.success(),
        "{operation} failed: {}",
        output.status
    );
    Ok(output.stdout)
}

fn check_mutating_output(output: Output, operation: &str) -> anyhow::Result<Vec<u8>> {
    if !output.status.success() {
        return Err(super::process::InterruptedCommand::new(anyhow::anyhow!(
            "{operation} has an uncertain remote outcome: {}",
            output.status
        ))
        .into());
    }
    Ok(output.stdout)
}

fn connect_docker(deadline: &Deadline, cancellation: &Cancellation) -> anyhow::Result<Docker> {
    let docker = executable("docker")?;
    let context = std::env::var_os("DOCKER_CONTEXT").filter(|value| !value.is_empty());
    let endpoint = if context.is_none() {
        match std::env::var("DOCKER_HOST") {
            Ok(value) => Some(value).filter(|value| !value.is_empty()),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(error).context("DOCKER_HOST must be valid UTF-8"),
        }
    } else {
        None
    };
    let endpoint = match endpoint {
        Some(endpoint) => endpoint,
        None => {
            let mut command = Command::new(&docker);
            command.args([
                "context",
                "inspect",
                "--format",
                "{{json .Endpoints.docker.Host}}",
            ]);
            if let Some(context) = context {
                command.arg(context);
            }
            let output = check_output(
                run_command_cancellable(&mut command, deadline, cancellation)?,
                "resolve Docker daemon endpoint",
            )?;
            serde_json::from_slice::<String>(&output).context("invalid Docker context endpoint")?
        }
    };
    Docker::connect(docker, &endpoint, deadline, cancellation)
}

fn docker_command(template: &Command) -> Command {
    let mut command = Command::new(template.get_program());
    command.args(template.get_args());
    for (name, value) in template.get_envs() {
        match value {
            Some(value) => {
                command.env(name, value);
            }
            None => {
                command.env_remove(name);
            }
        }
    }
    command
}

fn docker_output(
    template: &Command,
    args: &[&str],
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<Output> {
    run_command_cancellable(docker_command(template).args(args), deadline, cancellation)
}

fn container_id(text: &[u8]) -> anyhow::Result<String> {
    let id = std::str::from_utf8(text)?.trim();
    anyhow::ensure!(
        id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Docker returned an invalid FVP container ID"
    );
    Ok(id.to_owned())
}

fn verify_container(
    template: &Command,
    id: &str,
    state: &RunState,
    image: &str,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<()> {
    let output = check_output(
        docker_output(
            template,
            &["inspect", "--type", "container", "--", id],
            deadline,
            cancellation,
        )?,
        "inspect FVP container ownership",
    )?;
    let records: Vec<serde_json::Value> = serde_json::from_slice(&output)?;
    anyhow::ensure!(records.len() == 1, "ambiguous FVP container ownership");
    let record = &records[0];
    anyhow::ensure!(
        record["Id"].as_str() == Some(id) && record["Config"]["Image"].as_str() == Some(image),
        "FVP container identity mismatch"
    );
    for (key, value) in state.container_labels() {
        anyhow::ensure!(
            record["Config"]["Labels"][&key].as_str() == Some(&value),
            "FVP container ownership label mismatch: {key}"
        );
    }
    Ok(())
}

fn verify_model(
    template: &Command,
    state: &RunState,
    manifest: &PlatformManifest,
    logs: &Path,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<()> {
    let mut create = docker_command(template);
    create.args(["create", "--network", "none"]);
    for (key, value) in state.container_labels() {
        create.arg("--label").arg(format!("{key}={value}"));
    }
    create
        .arg("--entrypoint")
        .arg(&manifest.fvp.executable_in_container)
        .arg(&manifest.shrinkwrap.container.digest)
        .arg("--version");
    let id = container_id(&check_mutating_output(
        run_command_cancellable(&mut create, deadline, cancellation)?,
        "create FVP model inventory container",
    )?)?;
    verify_container(
        template,
        &id,
        state,
        &manifest.shrinkwrap.container.digest,
        deadline,
        cancellation,
    )?;
    let version = docker_output(
        template,
        &["start", "--attach", &id],
        deadline,
        cancellation,
    )?;
    if !version.status.success() {
        return Err(super::process::InterruptedCommand::new(anyhow::anyhow!(
            "FVP model inventory start has an uncertain remote outcome: {}",
            version.status
        ))
        .into());
    }
    std::fs::write(logs.join("model-version.stdout.log"), &version.stdout)?;
    std::fs::write(logs.join("model-version.stderr.log"), &version.stderr)?;
    let verification = verify_model_identity(
        &manifest.shrinkwrap.container.digest,
        &manifest.fvp.executable_in_container,
        &version,
    );
    let exit_status = check_output(
        docker_output(
            template,
            &["inspect", "--format", "{{.State.ExitCode}}", &id],
            deadline,
            cancellation,
        )?,
        "inspect FVP model inventory exit status",
    )?;
    verify_container(
        template,
        &id,
        state,
        &manifest.shrinkwrap.container.digest,
        deadline,
        cancellation,
    )?;
    check_mutating_output(
        docker_output(
            template,
            &["rm", "--force", "--", &id],
            deadline,
            cancellation,
        )?,
        "remove owned model inventory container",
    )?;
    anyhow::ensure!(
        std::str::from_utf8(&exit_status)?.trim() == "0",
        "FVP model inventory executable failed"
    );
    verification
}

fn read_log(path: &Path) -> anyhow::Result<String> {
    read_log_since(path, 0)
}

fn read_log_since(path: &Path, offset: u64) -> anyhow::Result<String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error).context("failed to read FVP console log"),
    };
    let length = file.metadata()?.len();
    anyhow::ensure!(
        length >= offset && length - offset <= LOG_LIMIT,
        "FVP readiness log changed or exceeds 16 MiB"
    );
    file.seek(SeekFrom::Start(offset))?;
    let mut data = Vec::new();
    file.take(LOG_LIMIT + 1).read_to_end(&mut data)?;
    anyhow::ensure!(
        data.len() as u64 <= LOG_LIMIT,
        "FVP readiness log exceeds 16 MiB"
    );
    Ok(String::from_utf8_lossy(&data).into_owned())
}

fn owned_containers(
    template: &Command,
    state: &RunState,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<Vec<String>> {
    let mut command = docker_command(template);
    command.args(["ps", "--quiet", "--no-trunc"]);
    for (key, value) in state.container_labels() {
        command.arg("--filter").arg(format!("label={key}={value}"));
    }
    let output = check_output(
        run_command_cancellable(&mut command, deadline, cancellation)?,
        "find running owned FVP container",
    )?;
    std::str::from_utf8(&output)?
        .lines()
        .map(|line| container_id(line.as_bytes()))
        .collect()
}

fn model_pid(top: &[u8], model: &str) -> anyhow::Result<Option<u32>> {
    let name = Path::new(model)
        .file_name()
        .context("model path has no filename")?;
    let mut found = None;
    for line in std::str::from_utf8(top)?.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 3
            && fields[1].starts_with("FVP_Base_RevC")
            && Path::new(fields[2]).file_name() == Some(name)
        {
            anyhow::ensure!(
                found.is_none(),
                "multiple FVP model processes in owned container"
            );
            found = Some(fields[0].parse().context("invalid FVP model PID")?);
        }
    }
    Ok(found)
}

fn confirm_model(
    child: &mut ManagedChild,
    template: &Command,
    state: &RunState,
    manifest: &PlatformManifest,
    launcher_log: &Path,
    launcher_offset: u64,
    port: u16,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<(LaunchConfirmation, Option<u32>)> {
    loop {
        cancellation.check()?;
        deadline.remaining()?;
        let log = read_log_since(launcher_log, launcher_offset)?;
        if log.contains("Address already in use") || log.contains("Failed to bind host port") {
            return Ok((LaunchConfirmation::PortCollision, None));
        }
        anyhow::ensure!(
            child.try_wait()?.is_none(),
            "FVP launcher exited before model startup; see {}",
            launcher_log.display()
        );
        let ids = owned_containers(template, state, deadline, cancellation)?;
        anyhow::ensure!(
            ids.len() <= 1,
            "multiple owned containers during FVP model startup"
        );
        if let Some(id) = ids.first() {
            verify_container(
                template,
                id,
                state,
                &manifest.shrinkwrap.container.digest,
                deadline,
                cancellation,
            )?;
            let top = check_output(
                docker_output(
                    template,
                    &["top", id, "-eo", "pid,comm,args"],
                    deadline,
                    cancellation,
                )?,
                "inspect running FVP model",
            )?;
            if let Some(pid) = model_pid(&top, &manifest.fvp.executable_in_container)? {
                match listener_state(pid, port)? {
                    ListenerState::Owned => return Ok((LaunchConfirmation::Running, Some(pid))),
                    ListenerState::Occupied => {
                        return Ok((LaunchConfirmation::PortCollision, None));
                    }
                    ListenerState::Pending => {}
                }
            }
        }
        std::thread::sleep(deadline.remaining()?.min(Duration::from_millis(100)));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ListenerState {
    Pending,
    Owned,
    Occupied,
}

fn listener_state(pid: u32, port: u16) -> anyhow::Result<ListenerState> {
    let table = std::fs::read_to_string(format!("/proc/{pid}/net/tcp"))
        .context("cannot inspect FVP model listeners")?;
    let address = format!("0100007F:{port:04X}");
    let mut sockets = Vec::new();
    for line in table.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() > 9 && fields[1] == address && fields[3] == "0A" {
            sockets.push(format!("socket:[{}]", fields[9]));
        }
    }
    if sockets.is_empty() {
        return Ok(ListenerState::Pending);
    }
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
        let entry = entry?;
        match std::fs::read_link(entry.path()) {
            Ok(target)
                if sockets
                    .iter()
                    .any(|socket| target.as_os_str() == socket.as_str()) =>
            {
                return Ok(ListenerState::Owned);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot verify FVP socket ownership"),
        }
    }
    Ok(ListenerState::Occupied)
}

async fn bounded<T>(
    driver: &DefaultDriver,
    deadline: &Deadline,
    cancellation: &Cancellation,
    work: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let timeout = async {
        let mut timer = PolledTimer::new(driver);
        loop {
            cancellation.check()?;
            timer
                .sleep(deadline.remaining()?.min(Duration::from_millis(20)))
                .await;
        }
    };
    let result = (work, timeout).race().await?;
    cancellation.check()?;
    deadline.remaining()?;
    Ok(result)
}

fn payload_hash(
    path: &Path,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<String> {
    cancellation.check()?;
    deadline.remaining()?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("cannot open CCA payload {}", path.display()))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "CCA payload must be a regular file"
    );
    let mut hash = sha2::Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        cancellation.check()?;
        deadline.remaining()?;
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
    }
    cancellation.check()?;
    deadline.remaining()?;
    Ok(hex::encode(hash.finalize()))
}

fn verify_payload(
    path: &Path,
    expected: &str,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        payload_hash(path, deadline, cancellation)? == expected,
        "CCA payload identity mismatch: {}",
        path.display()
    );
    Ok(())
}

fn snapshot_payload(
    path: &Path,
    expected: &str,
    output: &Path,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<tempfile::TempPath> {
    cancellation.check()?;
    deadline.remaining()?;
    let mut source = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW)
        .open(path)?;
    anyhow::ensure!(
        source.metadata()?.is_file(),
        "CCA payload must be a regular file"
    );
    let mut snapshot = tempfile::Builder::new()
        .prefix(".cca-payload-")
        .tempfile_in(output)?;
    let mut buffer = [0u8; 65536];
    loop {
        cancellation.check()?;
        deadline.remaining()?;
        let length = source.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        snapshot.write_all(&buffer[..length])?;
    }
    snapshot.flush()?;
    verify_payload(snapshot.path(), expected, deadline, cancellation)?;
    Ok(snapshot.into_temp_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn guest_failure_is_not_lost_when_finalization_also_fails() {
        assert_eq!(combine_guest_outcome(Ok(42), Ok(())).unwrap(), 42);
        let error =
            combine_guest_outcome(Ok(42), Err(anyhow::anyhow!("toolchain changed"))).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("exit code 42") && text.contains("toolchain changed"));
        let error = combine_guest_outcome(
            Err(anyhow::anyhow!("cancelled")),
            Err(anyhow::anyhow!("cleanup failed")),
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("cancelled") && text.contains("cleanup failed"));
    }

    #[test]
    fn rejects_invalid_container_ids() {
        for id in ["", "short", "../../outside", &"z".repeat(64)] {
            assert!(container_id(id.as_bytes()).is_err());
        }
        assert_eq!(
            container_id(format!("{}\n", "a".repeat(64)).as_bytes()).unwrap(),
            "a".repeat(64)
        );
    }

    #[test]
    fn guest_command_must_be_an_explicit_share_input() {
        assert_eq!(
            shared_program_input(&format!(
                "{}/nextest-archive-tmp/tests",
                crate::GUEST_SHARE_ROOT
            ))
            .unwrap(),
            Path::new("nextest-archive-tmp/tests")
        );
        for command in [
            "/bin/true",
            "tests",
            "/share-other/tests",
            "/share",
            "/share/../tests",
        ] {
            assert!(shared_program_input(command).is_err(), "{command}");
        }
    }

    #[test]
    fn late_dhcp_completion_cannot_bypass_host_deadline() {
        let mut progress = DhcpProgress {
            deadline: Some(Deadline::new(Duration::ZERO).unwrap()),
            completed: false,
        };
        assert!(
            progress
                .observe("INCUBATOR DHCP COMPLETE", seconds(30))
                .is_err()
        );
        assert!(!progress.completed);
    }

    #[test]
    fn interrupted_docker_mutation_keeps_scope_unresolved() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(9),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let error = scope_result(check_mutating_output(output, "create container")).unwrap_err();
        assert!(error.is::<super::super::process::InterruptedCommand>());
    }

    #[test]
    fn output_paths_cannot_enter_read_only_inputs() {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("target/fvp-runtime-tests");
        std::fs::create_dir_all(&parent).unwrap();
        let directory = tempfile::tempdir_in(parent).unwrap();
        let root = directory.path().join("platform");
        std::fs::create_dir(&root).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        assert!(output_path(&root.join("results"), &[&root]).is_err());
        assert!(output_path(&alias.join("results"), &[&root]).is_err());
        assert!(!root.join("results").exists());
        output_path(&directory.path().join("results"), &[&root]).unwrap();
        let volatile = tempfile::tempdir().unwrap();
        assert!(output_path(&volatile.path().join("results"), &[]).is_err());
        assert!(!volatile.path().join("results").exists());
    }

    #[test]
    fn command_output_is_drained_after_early_exit_status() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stdout.log");
        let deadline = Deadline::new(seconds(5)).unwrap();
        let cancellation = Cancellation::default();
        let (reader, writer) = mesh::pipe::pipe();
        DefaultPool::run_with(async |driver| {
            let producer = async {
                let mut timer = PolledTimer::new(&driver);
                timer.sleep(Duration::from_millis(20)).await;
                writer.write_nonblocking(b"last output frame\n")?;
                drop(writer);
                anyhow::Ok(())
            };
            let (status, (), ()) = futures::try_join!(
                async { anyhow::Ok(0) },
                drain_command_output(
                    reader,
                    path.clone(),
                    futures::io::Cursor::new(Vec::<u8>::new()),
                    &deadline,
                    &cancellation
                ),
                producer,
            )?;
            assert_eq!(status, 0);
            anyhow::Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"last output frame\n");
    }

    #[test]
    fn blocked_output_does_not_block_the_execution_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let (reader, mut writer) = mesh::pipe::pipe();
        let (unread, target) = PolledPipe::file_pair().unwrap();
        let start = Instant::now();
        DefaultPool::run_with(async |driver| {
            let sink = host_output(&driver, &target).unwrap();
            let deadline = Deadline::new(Duration::from_millis(100)).unwrap();
            let cancellation = Cancellation::default();
            let producer = async {
                writer.write_all(&vec![0u8; 1024 * 1024]).await?;
                drop(writer);
                anyhow::Ok(())
            };
            let result = bounded(&driver, &deadline, &cancellation, async {
                futures::try_join!(
                    drain_command_output(
                        reader,
                        directory.path().join("output"),
                        sink,
                        &deadline,
                        &cancellation
                    ),
                    producer,
                )?;
                anyhow::Ok(())
            })
            .await;
            assert!(format!("{:#}", result.unwrap_err()).contains("deadline"));
        });
        drop(unread);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn payload_snapshot_keeps_verified_bytes_after_source_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("payload");
        std::fs::write(&source, b"verified").unwrap();
        let expected = hex::encode(sha2::Sha256::digest(b"verified"));
        let deadline = Deadline::new(seconds(5)).unwrap();
        let cancellation = Cancellation::default();
        let snapshot = snapshot_payload(
            &source,
            &expected,
            directory.path(),
            &deadline,
            &cancellation,
        )
        .unwrap();
        std::fs::write(&source, b"mutated").unwrap();
        verify_payload(&snapshot, &expected, &deadline, &cancellation).unwrap();
        assert!(verify_payload(&source, &expected, &deadline, &cancellation).is_err());
        assert!(
            snapshot_payload(
                &source,
                &expected,
                directory.path(),
                &deadline,
                &cancellation,
            )
            .is_err()
        );
    }

    #[test]
    fn expired_payload_phase_cannot_start_more_file_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("absent");
        let deadline = Deadline::new(Duration::ZERO).unwrap();
        let cancellation = Cancellation::default();
        let error = payload_hash(&path, &deadline, &cancellation).unwrap_err();
        assert!(format!("{error:#}").contains("deadline"));
        let error = snapshot_payload(
            &path,
            KERNEL_SHA256,
            directory.path(),
            &deadline,
            &cancellation,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("deadline"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
