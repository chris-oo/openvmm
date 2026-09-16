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
const OUTCOME_REPORT: &str = "run-outcome.json";

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CommandExecutionReport {
    #[default]
    NotAttempted,
    Unknown {
        error: Option<String>,
    },
    Exited {
        /// Exit code, or 128 plus signal number, as returned by the runner.
        normalized_exit_code: i32,
    },
}

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CaptureOutcomeReport {
    #[default]
    NotAttempted,
    Unknown {
        error: Option<String>,
    },
    Succeeded,
    Failed {
        error: String,
    },
}

#[derive(Clone, Debug, Default, serde::Serialize)]
struct CommandOutcomeReport {
    execution: CommandExecutionReport,
    capture: CaptureOutcomeReport,
}

impl CommandOutcomeReport {
    fn start(&mut self) {
        self.execution = CommandExecutionReport::Unknown { error: None };
        self.capture = CaptureOutcomeReport::Unknown { error: None };
    }

    fn exited(&mut self, normalized_exit_code: i32) {
        self.execution = CommandExecutionReport::Exited {
            normalized_exit_code,
        };
    }

    fn observe(&mut self, result: &anyhow::Result<CompletedCommand>) {
        match result {
            Ok(completion) => {
                self.exited(completion.exit_code);
                self.capture = match &completion.capture {
                    Ok(()) => CaptureOutcomeReport::Succeeded,
                    Err(error) => CaptureOutcomeReport::Failed {
                        error: report_error(error),
                    },
                };
            }
            Err(error) => {
                if matches!(self.execution, CommandExecutionReport::Unknown { .. }) {
                    self.execution = CommandExecutionReport::Unknown {
                        error: Some(report_error(error)),
                    };
                }
                if matches!(self.capture, CaptureOutcomeReport::Unknown { .. }) {
                    self.capture = CaptureOutcomeReport::Unknown {
                        error: Some(report_error(error)),
                    };
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum LauncherOutcomeReport {
    #[default]
    NotAttempted,
    Unknown {
        error: Option<String>,
    },
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
        success: bool,
    },
}

impl LauncherOutcomeReport {
    fn observe(&mut self, result: &anyhow::Result<std::process::ExitStatus>) {
        match result {
            Ok(status) => {
                *self = Self::Exited {
                    exit_code: status.code(),
                    signal: status.signal(),
                    success: status.success(),
                };
            }
            Err(error) if !matches!(self, Self::NotAttempted) => {
                *self = Self::Unknown {
                    error: Some(report_error(error)),
                };
            }
            Err(_) => {}
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum FinalizationOutcomeReport {
    #[default]
    NotAttempted,
    Unknown,
    Succeeded,
    Failed {
        error: String,
    },
}

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum OverallOutcomeReport {
    #[default]
    Pending,
    Succeeded {
        exit_code: i32,
    },
    Failed {
        exit_code: Option<i32>,
        error: Option<String>,
    },
}

impl OverallOutcomeReport {
    fn from_result(result: &anyhow::Result<i32>) -> Self {
        match result {
            Ok(0) => Self::Succeeded { exit_code: 0 },
            Ok(code) => Self::Failed {
                exit_code: Some(*code),
                error: None,
            },
            Err(error) => Self::Failed {
                exit_code: None,
                error: Some(report_error(error)),
            },
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
struct RunOutcomeReport {
    schema_version: u32,
    run_id: Option<String>,
    run_output_dir: Option<PathBuf>,
    guest_command: CommandOutcomeReport,
    fixture_teardown: CommandOutcomeReport,
    /// The supervised launcher is Shrinkwrap, not the FVP process itself.
    launcher_status_source: &'static str,
    launcher_shutdown: LauncherOutcomeReport,
    finalization: FinalizationOutcomeReport,
    overall: OverallOutcomeReport,
    report_write_errors: Vec<String>,
}

impl RunOutcomeReport {
    fn initial() -> Self {
        Self {
            schema_version: 2,
            run_id: None,
            run_output_dir: None,
            guest_command: CommandOutcomeReport::default(),
            fixture_teardown: CommandOutcomeReport::default(),
            launcher_status_source: "shrinkwrap",
            launcher_shutdown: LauncherOutcomeReport::default(),
            finalization: FinalizationOutcomeReport::default(),
            overall: OverallOutcomeReport::default(),
            report_write_errors: Vec::new(),
        }
    }
}

struct ReservedResultFile {
    directory: File,
    root: PathBuf,
    name: std::ffi::OsString,
    file: File,
}

impl ReservedResultFile {
    fn reserve(root: &Path, requested: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !requested
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
            "FVP result file must not contain parent traversal"
        );
        let root = super::lifecycle::resolve_existing_ancestor(&std::path::absolute(root)?)?;
        let path = std::path::absolute(requested)?;
        anyhow::ensure!(
            path.parent() == Some(root.as_path()),
            "FVP result file must be a direct child of the canonical output directory"
        );
        let name = path
            .file_name()
            .context("missing FVP result filename")?
            .to_owned();
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_DIRECTORY)
            .open(&root)?;
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(anchored.join(&name))
            .context("FVP result file must be fresh; existing files and symlinks are rejected")?;
        let mut reserved = Self {
            directory,
            root,
            name,
            file,
        };
        reserved.publish(&RunOutcomeReport::initial(), &Deadline::new(seconds(5))?)?;
        Ok(reserved)
    }

    fn publish(&mut self, report: &RunOutcomeReport, deadline: &Deadline) -> anyhow::Result<()> {
        deadline.remaining()?;
        let actual_root = std::fs::symlink_metadata(&self.root)?;
        let held_root = self.directory.metadata()?;
        anyhow::ensure!(
            actual_root.is_dir()
                && (actual_root.dev(), actual_root.ino()) == (held_root.dev(), held_root.ino()),
            "FVP result directory identity changed"
        );
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()));
        let path = anchored.join(&self.name);
        let current = std::fs::symlink_metadata(&path)?;
        let held = self.file.metadata()?;
        anyhow::ensure!(
            current.is_file()
                && current.nlink() == 1
                && (current.dev(), current.ino()) == (held.dev(), held.ino()),
            "FVP reserved result file identity changed"
        );
        self.file = write_outcome_snapshot(&anchored, &path, report, deadline)?;
        Ok(())
    }
}

struct OutcomeLedger {
    output: PathBuf,
    budget: Duration,
    report: RunOutcomeReport,
    write_errors: anyhow::Result<()>,
    result_file: Option<ReservedResultFile>,
}

impl OutcomeLedger {
    fn new(output: &Path, run_id: &str, budget: Duration) -> Self {
        Self {
            output: output.to_owned(),
            budget,
            report: RunOutcomeReport {
                run_id: Some(run_id.into()),
                ..RunOutcomeReport::initial()
            },
            write_errors: Ok(()),
            result_file: None,
        }
    }

    // Reporting errors must not short-circuit ordered process completion.
    fn checkpoint(&mut self, phase: &str) {
        let output = self.output.clone();
        let budget = self.budget;
        let mut result_file = self.result_file.take();
        self.checkpoint_with(phase, |report| {
            write_outcome_copies(
                &output,
                result_file.as_mut(),
                report,
                &Deadline::new(budget)?,
            )
        });
        self.result_file = result_file;
    }

    fn checkpoint_with(
        &mut self,
        phase: &str,
        write: impl FnOnce(&RunOutcomeReport) -> anyhow::Result<()>,
    ) -> bool {
        match write(&self.report) {
            Ok(()) => true,
            Err(error) => {
                let error = error.context(format!("FVP outcome report checkpoint {phase} failed"));
                self.report.report_write_errors.push(report_error(&error));
                self.write_errors = combine(
                    std::mem::replace(&mut self.write_errors, Ok(())),
                    Err(error),
                );
                false
            }
        }
    }

    fn finish(mut self, outcome: anyhow::Result<i32>) -> anyhow::Result<i32> {
        let output = self.output.clone();
        let budget = self.budget;
        let mut result_file = self.result_file.take();
        self.finish_with(outcome, |report| {
            write_outcome_copies(
                &output,
                result_file.as_mut(),
                report,
                &Deadline::new(budget)?,
            )
        })
    }

    fn defer_final_report(mut self, outcome: anyhow::Result<i32>) -> anyhow::Result<i32> {
        // Retained workspace state may seal the entire output tree for later
        // recovery. Updating this file would invalidate that receipt.
        let outcome = combine_guest_outcome(
            outcome,
            combine(
                std::mem::replace(&mut self.write_errors, Ok(())),
                Err(anyhow::anyhow!(
                    "FVP per-run outcome report remains incomplete: retained workspace may have sealed outputs"
                )),
            ),
        );
        // The caller's direct-child result file is outside the sealed per-run
        // tree, so it can still publish the final failure without changing it.
        if let Some(mut result_file) = self.result_file.take() {
            let budget = self.budget;
            self.finish_with(outcome, |report| {
                result_file.publish(report, &Deadline::new(budget)?)
            })
        } else {
            outcome
        }
    }

    fn finish_with(
        mut self,
        outcome: anyhow::Result<i32>,
        mut write: impl FnMut(&RunOutcomeReport) -> anyhow::Result<()>,
    ) -> anyhow::Result<i32> {
        let mut outcome =
            combine_guest_outcome(outcome, std::mem::replace(&mut self.write_errors, Ok(())));
        self.report.overall = OverallOutcomeReport::from_result(&outcome);
        if !self.checkpoint_with("final", &mut write) {
            outcome =
                combine_guest_outcome(outcome, std::mem::replace(&mut self.write_errors, Ok(())));
            self.report.overall = OverallOutcomeReport::from_result(&outcome);
            // One bounded retry may publish the reporting failure itself.
            // A successful retry does not turn the run back into success.
            self.checkpoint_with("final_failure", &mut write);
        }
        combine_guest_outcome(outcome, self.write_errors)
    }
}

fn report_error(error: &anyhow::Error) -> String {
    const LIMIT: usize = 4096;
    let mut text = format!("{error:#}");
    if text.len() <= LIMIT {
        text
    } else {
        let mut boundary = LIMIT;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
        text.push_str(" [truncated]");
        text
    }
}

fn write_outcome_report(
    output: &Path,
    report: &RunOutcomeReport,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    write_outcome_snapshot(output, &output.join(OUTCOME_REPORT), report, deadline).map(|_| ())
}

fn write_outcome_copies(
    output: &Path,
    result_file: Option<&mut ReservedResultFile>,
    report: &RunOutcomeReport,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    let local = write_outcome_report(output, report, deadline);
    let external = if let Some(result_file) = result_file {
        if let Err(error) = &local {
            let mut failed = report.clone();
            failed.report_write_errors.push(report_error(error));
            if !matches!(failed.overall, OverallOutcomeReport::Pending) {
                failed.overall = OverallOutcomeReport::Failed {
                    exit_code: None,
                    error: Some(report_error(error)),
                };
            }
            result_file.publish(&failed, deadline)
        } else {
            result_file.publish(report, deadline)
        }
    } else {
        Ok(())
    };
    combine(local, external)
}

fn write_outcome_snapshot(
    output: &Path,
    destination: &Path,
    report: &RunOutcomeReport,
    deadline: &Deadline,
) -> anyhow::Result<File> {
    deadline.remaining()?;
    let mut snapshot = tempfile::NamedTempFile::new_in(output)?;
    serde_json::to_writer_pretty(&mut snapshot, report)?;
    snapshot.write_all(b"\n")?;
    snapshot.flush()?;
    snapshot.as_file().sync_all()?;
    deadline.remaining()?;
    // Publish only a complete, flushed snapshot. There are no fallible steps
    // after publication that could leave a success report for a failed write.
    snapshot
        .persist(destination)
        .map_err(|error| error.error.into())
}

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
    let manifest = PlatformManifest::for_platform(profile.platform)?;
    let kernel_sha256 = match profile.platform {
        crate::profile::FvpPlatform::CcaV15 => {
            petri_artifacts_common::cca_payload::LINUX_IMAGE_SHA256
        }
        crate::profile::FvpPlatform::GuestMemfdInPlace | crate::profile::FvpPlatform::RealmVfio => {
            petri_artifacts_common::cca_payload::guest_memfd_in_place::LINUX_IMAGE_SHA256
        }
    };
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
    let result_file = if let Some(path) = &config.result_file {
        std::fs::create_dir_all(&output_base)?;
        Some(ReservedResultFile::reserve(&output_base, path)?)
    } else {
        None
    };
    verify_payload(&config.kernel, kernel_sha256, &initial, &cancellation)?;
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
    let mut ledger = OutcomeLedger::new(&output, &run_id, seconds(profile.deadlines.validation));
    ledger.result_file = result_file;
    ledger.checkpoint("created");
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
        ledger.report.run_output_dir = Some(output.clone());
        ledger.checkpoint("workspace_registered");
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
            realm_vfio: profile.platform == crate::profile::FvpPlatform::RealmVfio,
        };
        let kernel_copy = snapshot_payload(
            &config.kernel,
            kernel_sha256,
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
            kernel_sha256,
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
            kernel_sha256,
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
                "kernel_sha256": kernel_sha256,
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
        let completion = session.execute_test(seconds(profile.deadlines.test_execution), |deadline, cancellation| {
            Ok(pool.run_until(bounded(&driver, deadline, cancellation, async {
                let realm_vfio = profile.platform == crate::profile::FvpPlatform::RealmVfio;
                if realm_vfio {
                    let ready = connected.command("/bin/cat")
                        .arg("/run/incubator-cca-realm-vfio").output().await?;
                    anyhow::ensure!(
                        ready.status.code() == Some(0) && ready.stdout == b"0000:01:00.0\n",
                        "FVP Realm VFIO host fixture did not complete provisioning"
                    );
                }
                let mut command = connected.command(program);
                command.args(arguments);
                for (key, value) in &config.guest_env {
                    anyhow::ensure!(
                        key != "PETRI_CAPABILITIES" && !key.starts_with("INCUBATOR_VFIO_"),
                        "FVP runtime capabilities and VFIO identity cannot be supplied by the guest environment"
                    );
                    command.env(key, value);
                }
                command.env("PETRI_CAPABILITIES", profile.capabilities.join(","));
                if realm_vfio {
                    command.env("INCUBATOR_VFIO_BDF_CCA_REALM_VFIO", "0000:01:00.0");
                }
                if let Some(directory) = &config.guest_current_dir { command.current_dir(directory); }
                command
                    .stdin(pipette_client::process::Stdio::null())
                    .stdout(pipette_client::process::Stdio::piped())
                    .stderr(pipette_client::process::Stdio::piped());
                let stdout_target = host_output(&driver, &std::io::stdout())?;
                let stderr_target = host_output(&driver, &std::io::stderr())?;
                ledger.report.guest_command.start();
                ledger.checkpoint("guest_command_start");
                let mut child = command.spawn().await.context("failed to dispatch FVP guest command")?;
                let stdout = child.stdout.take().context("missing FVP command stdout pipe")?;
                let stderr = child.stderr.take().context("missing FVP command stderr pipe")?;
                wait_and_drain(
                    async {
                        let status = child.wait().await.context("failed to wait for FVP guest command")?;
                        let code = status.code().or_else(|| status.signal().map(|signal| 128 + signal))
                            .context("FVP guest command returned no exit status")?;
                        ledger.report.guest_command.exited(code);
                        ledger.checkpoint("guest_command_exit");
                        Ok(code)
                    },
                    drain_command_output(stdout, output.join("command.stdout.log"), stdout_target, deadline, cancellation),
                    drain_command_output(stderr, output.join("command.stderr.log"), stderr_target, deadline, cancellation),
                ).await
            })))
        }).and_then(|result| result);
        ledger.report.guest_command.observe(&completion);
        ledger.checkpoint("guest_command_result");
        let completion = completion?;
        // Only confirmed completion permits fixture teardown. Capture errors
        // remain failures, but do not erase the confirmed process exit.
        let exit_code = completion.outcome();
        let mut teardown = Ok(());
        let shutdown =
            session.shutdown_scoped(seconds(profile.deadlines.guest_shutdown), |deadline| {
                if profile.platform == crate::profile::FvpPlatform::RealmVfio {
                    let completion = pool.run_until(bounded(&driver, deadline, &cancellation, async {
                        ledger.report.fixture_teardown.start();
                        ledger.checkpoint("fixture_teardown_start");
                        let mut child = connected.command("/bin/sh")
                            .arg("/run/incubator-cca-realm-vfio-teardown.sh")
                            .stdin(pipette_client::process::Stdio::null())
                            .stdout(pipette_client::process::Stdio::piped())
                            .stderr(pipette_client::process::Stdio::piped())
                            .spawn().await?;
                        let stdout = child.stdout.take().context("missing fixture teardown stdout")?;
                        let stderr = child.stderr.take().context("missing fixture teardown stderr")?;
                        wait_and_drain(
                            async {
                                let status = child.wait().await.context("failed to wait for fixture teardown")?;
                                let code = status.code().or_else(|| status.signal().map(|signal| 128 + signal))
                                    .context("fixture teardown returned no exit status")?;
                                ledger.report.fixture_teardown.exited(code);
                                ledger.checkpoint("fixture_teardown_exit");
                                Ok(code)
                            },
                            drain_command_output(stdout, output.join("fixture-teardown.stdout.log"),
                                futures::io::sink(), deadline, &cancellation),
                            drain_command_output(stderr, output.join("fixture-teardown.stderr.log"),
                                futures::io::sink(), deadline, &cancellation),
                        ).await
                    }));
                    ledger.report.fixture_teardown.observe(&completion);
                    ledger.checkpoint("fixture_teardown_result");
                    let completion = match completion {
                        Ok(completion) => completion,
                        // Do not power off with a possibly live teardown
                        // process. The session's forced-cleanup path owns it.
                        Err(error) => return Ok(Err(error)),
                    };
                    teardown = completion.outcome()
                        .context("FVP fixture teardown output capture failed")
                        .and_then(|status| {
                        anyhow::ensure!(status == 0,
                            "FVP Realm VFIO fixture teardown failed: {:?}; see fixture-teardown logs",
                            status);
                        Ok(())
                    });
                }
                ledger.report.launcher_shutdown = LauncherOutcomeReport::Unknown { error: None };
                ledger.checkpoint("launcher_shutdown_start");
                Ok(pool.run_until(bounded(
                    &driver,
                    deadline,
                    &cancellation,
                    connected.power_off(),
                )))
            });
        ledger.report.launcher_shutdown.observe(&shutdown);
        ledger.checkpoint("launcher_shutdown_result");
        let shutdown = shutdown.and_then(|shutdown| {
            anyhow::ensure!(
                shutdown.success(),
                "FVP launcher failed during L1 shutdown: {shutdown}"
            );
            Ok(())
        });
        combine_guest_outcome(exit_code, combine(teardown, shutdown))
    })();
    drop(client);
    ledger.report.finalization = FinalizationOutcomeReport::Unknown;
    ledger.checkpoint("finalization_start");
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
    ledger.report.finalization = match &finalization {
        Ok(()) => FinalizationOutcomeReport::Succeeded,
        Err(error) => FinalizationOutcomeReport::Failed {
            error: report_error(error),
        },
    };
    let outcome = combine_guest_outcome(outcome, finalization);
    let exit_code = if session.state().workspace_path().is_none() {
        ledger.finish(outcome)
    } else {
        ledger.defer_final_report(outcome)
    }?;
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

#[derive(Debug)]
struct CompletedCommand {
    exit_code: i32,
    capture: anyhow::Result<()>,
}

impl CompletedCommand {
    fn outcome(self) -> anyhow::Result<i32> {
        combine_guest_outcome(Ok(self.exit_code), self.capture)
    }
}

/// Capture failure must neither cancel wait nor strand a writer on a full pipe.
/// An error result never authorizes the next normal shutdown step.
async fn wait_and_drain(
    wait: impl Future<Output = anyhow::Result<i32>>,
    stdout: impl Future<Output = anyhow::Result<()>>,
    stderr: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<CompletedCommand> {
    let (status, stdout, stderr) = futures::join!(wait, stdout, stderr);
    let capture = combine(stdout, stderr);
    match status {
        Ok(exit_code) => Ok(CompletedCommand { exit_code, capture }),
        Err(error) => combine(
            Err(error.context("FVP command completion is unknown")),
            capture,
        ),
    }
}

async fn drain_command_output(
    pipe: mesh::pipe::ReadPipe,
    path: PathBuf,
    terminal: impl AsyncWrite + Unpin,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<()> {
    let file = File::create(&path).with_context(|| format!("cannot create {}", path.display()));
    drain_command_output_to(pipe, file, terminal, deadline, cancellation).await
}

async fn drain_command_output_to(
    mut pipe: mesh::pipe::ReadPipe,
    file: anyhow::Result<impl Write>,
    terminal: impl AsyncWrite + Unpin,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<()> {
    let (mut file, mut capture) = match file {
        Ok(file) => (Some(file), Vec::new()),
        Err(error) => (None, vec![error]),
    };
    let mut terminal = Some(terminal);
    let mut buffer = [0u8; 8192];
    let mut total = 0u64;
    let mut limited = false;
    let drained = async {
        loop {
            cancellation.check()?;
            deadline.remaining()?;
            let length = pipe
                .read(&mut buffer)
                .await
                .context("cannot read FVP command output")?;
            if length == 0 {
                break;
            }
            total = total.saturating_add(length as u64);
            if !limited && total > 128 * 1024 * 1024 {
                limited = true;
                capture.push(anyhow::anyhow!("FVP command output exceeds 128 MiB"));
                file = None;
                terminal = None;
            }
            if let Some(target) = &mut file {
                if let Err(error) = target.write_all(&buffer[..length]) {
                    capture.push(anyhow::Error::new(error).context("cannot write FVP capture log"));
                    file = None;
                }
            }
            if let Some(target) = &mut terminal {
                if let Err(error) = target.write_all(&buffer[..length]).await {
                    capture.push(
                        anyhow::Error::new(error).context("cannot forward FVP command output"),
                    );
                    terminal = None;
                }
            }
        }
        if let Some(file) = &mut file {
            if let Err(error) = file.flush() {
                capture.push(anyhow::Error::new(error).context("cannot flush FVP capture log"));
            }
        }
        if let Some(terminal) = &mut terminal {
            if let Err(error) = terminal.flush().await {
                capture.push(anyhow::Error::new(error).context("cannot flush FVP command output"));
            }
        }
        deadline.remaining()?;
        anyhow::Ok(())
    }
    .await;
    let mut captured = Ok(());
    for error in capture {
        captured = combine(captured, Err(error));
    }
    combine(drained, captured)
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

    fn outcome_directory() -> tempfile::TempDir {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("target/fvp-outcome-tests");
        std::fs::create_dir_all(&parent).unwrap();
        tempfile::tempdir_in(parent).unwrap()
    }

    fn read_outcome(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(path.join(OUTCOME_REPORT)).unwrap()).unwrap()
    }

    #[test]
    fn outcome_report_retains_guest_result_before_model_shutdown_failure() {
        let directory = outcome_directory();
        let mut ledger = OutcomeLedger::new(directory.path(), "run-id", seconds(5));
        let guest = Ok(CompletedCommand {
            exit_code: 0,
            capture: Ok(()),
        });
        ledger.report.guest_command.observe(&guest);
        ledger.checkpoint("guest_command_result");
        let before = read_outcome(directory.path());
        assert_eq!(before["schema_version"], 2);
        assert_eq!(
            before["guest_command"]["execution"]["normalized_exit_code"],
            0
        );
        assert_eq!(before["guest_command"]["execution"]["state"], "exited");
        assert_eq!(
            before["fixture_teardown"]["execution"]["state"],
            "not_attempted"
        );
        assert_eq!(before["launcher_shutdown"]["state"], "not_attempted");
        assert_eq!(before["overall"]["state"], "pending");

        ledger
            .report
            .fixture_teardown
            .observe(&Ok(CompletedCommand {
                exit_code: 0,
                capture: Ok(()),
            }));
        ledger
            .report
            .launcher_shutdown
            .observe(&Ok(std::process::ExitStatus::from_raw(134 << 8)));
        ledger.report.finalization = FinalizationOutcomeReport::Succeeded;
        assert!(
            ledger
                .finish(Err(anyhow::anyhow!("launcher shutdown failed")))
                .is_err()
        );
        let final_report = read_outcome(directory.path());
        assert_eq!(final_report["guest_command"], before["guest_command"]);
        assert_eq!(
            final_report["fixture_teardown"]["execution"]["normalized_exit_code"],
            0
        );
        assert_eq!(final_report["launcher_status_source"], "shrinkwrap");
        assert_eq!(final_report["launcher_shutdown"]["exit_code"], 134);
        assert_eq!(final_report["launcher_shutdown"]["success"], false);
        assert_eq!(final_report["finalization"]["state"], "succeeded");
        assert_eq!(final_report["overall"]["state"], "failed");
    }

    #[test]
    fn outcome_report_observed_exit_survives_capture_timeout() {
        for fixture in [false, true] {
            let directory = outcome_directory();
            let result_path = directory.path().join("session-result.json");
            let reserved = ReservedResultFile::reserve(directory.path(), &result_path).unwrap();
            let output = directory.path().join("fvp-run");
            std::fs::create_dir(&output).unwrap();
            let mut ledger = OutcomeLedger::new(&output, "run-id", seconds(5));
            ledger.result_file = Some(reserved);
            if fixture {
                ledger.report.fixture_teardown.start();
            } else {
                ledger.report.guest_command.start();
            }
            let result = DefaultPool::run_with(async |driver| {
                let deadline = Deadline::new(Duration::from_millis(50)).unwrap();
                let cancellation = Cancellation::default();
                bounded(
                    &driver,
                    &deadline,
                    &cancellation,
                    wait_and_drain(
                        async {
                            if fixture {
                                ledger.report.fixture_teardown.exited(17);
                            } else {
                                ledger.report.guest_command.exited(17);
                            }
                            ledger.checkpoint("observed_exit");
                            Ok(17)
                        },
                        std::future::pending(),
                        async { Ok(()) },
                    ),
                )
                .await
            });
            assert!(result.is_err());
            let key = if fixture {
                "fixture_teardown"
            } else {
                "guest_command"
            };
            let observed = read_outcome(&output);
            let external: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&result_path).unwrap()).unwrap();
            assert_eq!(external[key], observed[key]);
            assert_eq!(observed[key]["execution"]["state"], "exited");
            assert_eq!(observed[key]["execution"]["normalized_exit_code"], 17);
            assert_eq!(observed[key]["capture"]["state"], "unknown");
            assert!(observed[key]["capture"]["error"].is_null());
            if fixture {
                ledger.report.fixture_teardown.observe(&result);
            } else {
                ledger.report.guest_command.observe(&result);
            }
            ledger.checkpoint("capture_timeout");
            let report = read_outcome(&output);
            assert_eq!(report[key]["execution"]["normalized_exit_code"], 17);
            assert!(
                report[key]["capture"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("deadline")
            );
        }
    }

    #[test]
    fn outcome_result_file_rejects_collisions_symlinks_and_nondirect_paths() {
        let directory = outcome_directory();
        let root = directory.path();
        let valid = root.join("session-result.json");
        let _reserved = ReservedResultFile::reserve(root, &valid).unwrap();
        let initial = std::fs::read(&valid).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&initial).unwrap();
        assert_eq!(report["schema_version"], 2);
        assert!(report["run_id"].is_null() && report["run_output_dir"].is_null());
        assert!(ReservedResultFile::reserve(root, &valid).is_err());
        assert_eq!(std::fs::read(&valid).unwrap(), initial);
        std::fs::create_dir(root.join("nested")).unwrap();
        let existing = root.join("existing");
        std::fs::write(&existing, b"untouched").unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&existing, &alias).unwrap();
        let dangling = root.join("dangling");
        std::os::unix::fs::symlink(root.join("missing"), &dangling).unwrap();
        for invalid in [
            root.to_owned(),
            root.join("nested/result.json"),
            existing.clone(),
            alias,
            dangling,
            root.join("nested/../traversal.json"),
            root.parent().unwrap().join("outside.json"),
        ] {
            assert!(
                ReservedResultFile::reserve(root, &invalid).is_err(),
                "{invalid:?}"
            );
        }
        assert_eq!(std::fs::read(existing).unwrap(), b"untouched");
        assert!(!root.join("traversal.json").exists());
        assert!(!root.join("nested/result.json").exists());
    }

    #[test]
    fn outcome_result_file_rejects_replacement_after_reservation() {
        let directory = outcome_directory();
        let root = directory.path().join("output");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("session-result.json");
        let mut reserved = ReservedResultFile::reserve(&root, &path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"foreign").unwrap();
        assert!(
            reserved
                .publish(
                    &RunOutcomeReport::initial(),
                    &Deadline::new(seconds(5)).unwrap()
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign");
        std::fs::rename(&root, directory.path().join("original")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(&path, b"replacement directory").unwrap();
        assert!(
            reserved
                .publish(
                    &RunOutcomeReport::initial(),
                    &Deadline::new(seconds(5)).unwrap()
                )
                .is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), b"replacement directory");
    }

    #[test]
    fn outcome_result_file_records_registered_path_and_retained_cleanup_failure() {
        let directory = outcome_directory();
        let path = directory.path().join("session-result.json");
        let reserved = ReservedResultFile::reserve(directory.path(), &path).unwrap();
        let output = directory.path().join("fvp-run");
        std::fs::create_dir(&output).unwrap();
        let mut ledger = OutcomeLedger::new(&output, "run-id", seconds(5));
        ledger.result_file = Some(reserved);
        ledger.report.run_output_dir = Some(output.clone());
        ledger.report.guest_command.observe(&Ok(CompletedCommand {
            exit_code: 0,
            capture: Ok(()),
        }));
        ledger.report.finalization = FinalizationOutcomeReport::Unknown;
        ledger.checkpoint("finalization_start");
        let sealed = std::fs::read(output.join(OUTCOME_REPORT)).unwrap();
        ledger.report.finalization = FinalizationOutcomeReport::Failed {
            error: "retirement failed".into(),
        };
        assert!(
            ledger
                .defer_final_report(Err(anyhow::anyhow!("retirement failed")))
                .is_err()
        );
        assert_eq!(std::fs::read(output.join(OUTCOME_REPORT)).unwrap(), sealed);
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(report["run_id"], "run-id");
        assert_eq!(report["run_output_dir"], output.to_str().unwrap());
        assert_eq!(
            report["guest_command"]["execution"]["normalized_exit_code"],
            0
        );
        assert_eq!(report["guest_command"]["capture"]["state"], "succeeded");
        assert_eq!(report["finalization"]["state"], "failed");
        assert_eq!(report["overall"]["state"], "failed");
    }

    #[test]
    fn outcome_result_file_failure_is_retained_without_overwriting_foreign_data() {
        let directory = outcome_directory();
        let path = directory.path().join("session-result.json");
        let reserved = ReservedResultFile::reserve(directory.path(), &path).unwrap();
        let output = directory.path().join("fvp-run");
        std::fs::create_dir(&output).unwrap();
        let mut ledger = OutcomeLedger::new(&output, "run-id", seconds(5));
        ledger.result_file = Some(reserved);
        ledger.report.run_output_dir = Some(output.clone());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"foreign").unwrap();
        ledger.report.guest_command.observe(&Ok(CompletedCommand {
            exit_code: 0,
            capture: Ok(()),
        }));
        ledger.checkpoint("guest_command_result");
        ledger
            .report
            .fixture_teardown
            .observe(&Ok(CompletedCommand {
                exit_code: 0,
                capture: Ok(()),
            }));
        ledger.report.finalization = FinalizationOutcomeReport::Succeeded;
        let error = ledger.finish(Ok(0)).unwrap_err();
        assert!(format!("{error:#}").contains("reserved result file identity changed"));
        assert_eq!(std::fs::read(path).unwrap(), b"foreign");
        let report = read_outcome(&output);
        assert_eq!(
            report["fixture_teardown"]["execution"]["normalized_exit_code"],
            0
        );
        assert_eq!(report["overall"]["state"], "failed");
        assert!(!report["report_write_errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn outcome_result_file_reports_a_failed_per_run_publication() {
        let directory = outcome_directory();
        let path = directory.path().join("session-result.json");
        let reserved = ReservedResultFile::reserve(directory.path(), &path).unwrap();
        let output = directory.path().join("fvp-run");
        std::fs::create_dir_all(output.join(OUTCOME_REPORT)).unwrap();
        let mut ledger = OutcomeLedger::new(&output, "run-id", seconds(5));
        ledger.result_file = Some(reserved);
        ledger.report.run_output_dir = Some(output.clone());
        ledger.report.guest_command.observe(&Ok(CompletedCommand {
            exit_code: 0,
            capture: Ok(()),
        }));
        ledger.report.finalization = FinalizationOutcomeReport::Succeeded;
        assert!(ledger.finish(Ok(0)).is_err());
        assert!(output.join(OUTCOME_REPORT).is_dir());
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            report["guest_command"]["execution"]["normalized_exit_code"],
            0
        );
        assert_eq!(report["overall"]["state"], "failed");
        assert!(!report["report_write_errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn outcome_report_distinguishes_unknown_from_not_attempted() {
        let directory = outcome_directory();
        let mut ledger = OutcomeLedger::new(directory.path(), "run-id", seconds(5));
        ledger.checkpoint("created");
        let created = read_outcome(directory.path());
        assert_eq!(
            created["guest_command"]["execution"]["state"],
            "not_attempted"
        );
        assert_eq!(created["finalization"]["state"], "not_attempted");
        ledger.report.guest_command.start();
        ledger
            .report
            .guest_command
            .observe(&Err(anyhow::anyhow!("wait transport lost")));
        ledger.report.finalization = FinalizationOutcomeReport::Unknown;
        ledger.checkpoint("unknown_completion");
        let pending = read_outcome(directory.path());
        assert_eq!(pending["guest_command"]["execution"]["state"], "unknown");
        assert_eq!(
            pending["guest_command"]["execution"]["error"],
            "wait transport lost"
        );
        assert!(
            pending["guest_command"]["execution"]
                .get("normalized_exit_code")
                .is_none()
        );
        assert_eq!(
            pending["fixture_teardown"]["execution"]["state"],
            "not_attempted"
        );
        assert_eq!(pending["launcher_shutdown"]["state"], "not_attempted");
        assert_eq!(pending["finalization"]["state"], "unknown");
        ledger.report.finalization = FinalizationOutcomeReport::Failed {
            error: "resources not stopped".into(),
        };
        assert!(
            ledger
                .finish(Err(anyhow::anyhow!("resources not stopped")))
                .is_err()
        );
        assert_eq!(
            read_outcome(directory.path())["finalization"]["state"],
            "failed"
        );
    }

    #[test]
    fn outcome_report_keeps_capture_errors_separate_from_exit_status() {
        let directory = outcome_directory();
        let mut ledger = OutcomeLedger::new(directory.path(), "run-id", seconds(5));
        let guest = Ok(CompletedCommand {
            exit_code: 23,
            capture: Err(anyhow::anyhow!("guest capture ENOSPC")),
        });
        let teardown = Ok(CompletedCommand {
            exit_code: 7,
            capture: Err(anyhow::anyhow!("teardown capture ENOSPC")),
        });
        ledger.report.guest_command.observe(&guest);
        ledger.report.fixture_teardown.observe(&teardown);
        ledger
            .report
            .launcher_shutdown
            .observe(&Ok(std::process::ExitStatus::from_raw(0)));
        ledger.report.finalization = FinalizationOutcomeReport::Succeeded;
        let error = ledger
            .finish(combine_guest_outcome(
                guest.and_then(CompletedCommand::outcome),
                teardown.and_then(CompletedCommand::outcome).map(|_| ()),
            ))
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("exit code 23") && text.contains("exit code 7"));
        let report = read_outcome(directory.path());
        assert_eq!(
            report["guest_command"]["execution"]["normalized_exit_code"],
            23
        );
        assert_eq!(
            report["guest_command"]["capture"]["error"],
            "guest capture ENOSPC"
        );
        assert_eq!(
            report["fixture_teardown"]["execution"]["normalized_exit_code"],
            7
        );
        assert_eq!(
            report["fixture_teardown"]["capture"]["error"],
            "teardown capture ENOSPC"
        );
        assert_eq!(report["overall"]["state"], "failed");
    }

    #[test]
    fn outcome_report_write_failure_does_not_skip_ordered_completion() {
        let directory = outcome_directory();
        let mut ledger = OutcomeLedger::new(directory.path(), "run-id", seconds(5));
        ledger.report.guest_command.start();
        assert!(!ledger.checkpoint_with("guest_command_start", |_| {
            Err(anyhow::anyhow!("injected report write failure"))
        }));
        let cleanup_started = std::cell::Cell::new(false);
        let (sender, receiver) = mesh::oneshot();
        futures::executor::block_on(async {
            let ordered = async {
                let completion = wait_and_drain(
                    async { receiver.await.context("wait failed") },
                    async { Ok(()) },
                    async { Ok(()) },
                )
                .await;
                ledger.report.guest_command.observe(&completion);
                ledger.checkpoint("guest_command_result");
                let completion = completion?;
                cleanup_started.set(true);
                ledger
                    .report
                    .fixture_teardown
                    .observe(&Ok(CompletedCommand {
                        exit_code: 0,
                        capture: Ok(()),
                    }));
                ledger.report.finalization = FinalizationOutcomeReport::Succeeded;
                ledger.finish(completion.outcome())
            };
            let mut ordered = std::pin::pin!(ordered);
            assert!(futures::poll!(&mut ordered).is_pending());
            assert!(!cleanup_started.get());
            sender.send(0);
            let error = ordered.await.unwrap_err();
            assert!(format!("{error:#}").contains("injected report write failure"));
            assert!(cleanup_started.get());
        });
        let report = read_outcome(directory.path());
        assert_eq!(report["guest_command"]["execution"]["state"], "exited");
        assert_eq!(report["fixture_teardown"]["execution"]["state"], "exited");
        assert_eq!(report["overall"]["state"], "failed");
        assert_eq!(report["report_write_errors"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn outcome_report_final_write_failure_cannot_turn_into_success() {
        for retry_succeeds in [false, true] {
            let directory = outcome_directory();
            let mut ledger = OutcomeLedger::new(directory.path(), "run-id", seconds(5));
            ledger.report.guest_command.observe(&Ok(CompletedCommand {
                exit_code: 0,
                capture: Ok(()),
            }));
            ledger.report.finalization = FinalizationOutcomeReport::Succeeded;
            ledger.checkpoint("before_final");
            let mut attempts = 0;
            let result = ledger.finish_with(Ok(0), |report| {
                attempts += 1;
                if attempts == 1 || !retry_succeeds {
                    anyhow::bail!("injected final report ENOSPC");
                }
                write_outcome_report(directory.path(), report, &Deadline::new(seconds(5))?)
            });
            assert!(format!("{:#}", result.unwrap_err()).contains("final report ENOSPC"));
            assert_eq!(attempts, 2);
            assert_eq!(
                read_outcome(directory.path())["overall"]["state"],
                if retry_succeeds { "failed" } else { "pending" }
            );
        }
    }

    #[test]
    fn outcome_report_publication_is_atomic_and_error_text_is_bounded() {
        let directory = outcome_directory();
        let mut ledger = OutcomeLedger::new(directory.path(), "run-id", seconds(5));
        ledger.checkpoint("created");
        let original = std::fs::read(directory.path().join(OUTCOME_REPORT)).unwrap();
        ledger.report.guest_command.start();
        assert!(
            write_outcome_report(
                directory.path(),
                &ledger.report,
                &Deadline::new(Duration::ZERO).unwrap()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(directory.path().join(OUTCOME_REPORT)).unwrap(),
            original
        );
        ledger.checkpoint("started");
        assert_eq!(
            read_outcome(directory.path())["guest_command"]["execution"]["state"],
            "unknown"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        let error = report_error(&anyhow::anyhow!("界".repeat(5000)));
        assert!(error.len() <= 4096 + " [truncated]".len());
        assert!(error.ends_with(" [truncated]"));
    }

    #[test]
    fn outcome_report_does_not_modify_a_retained_output_receipt() {
        let directory = outcome_directory();
        let mut ledger = OutcomeLedger::new(directory.path(), "run-id", seconds(5));
        ledger.report.finalization = FinalizationOutcomeReport::Unknown;
        ledger.checkpoint("finalization_start");
        let sealed = std::fs::read(directory.path().join(OUTCOME_REPORT)).unwrap();
        ledger.report.finalization = FinalizationOutcomeReport::Failed {
            error: "retirement failed".into(),
        };
        let error = ledger
            .defer_final_report(Err(anyhow::anyhow!("retirement failed")))
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("retirement failed") && text.contains("sealed outputs"));
        assert_eq!(
            std::fs::read(directory.path().join(OUTCOME_REPORT)).unwrap(),
            sealed
        );
    }

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
    fn realm_vfio_teardown_and_launcher_errors_cannot_pass_listing() {
        for (teardown_failed, launcher_failed) in [(true, false), (false, true), (true, true)] {
            let teardown = if teardown_failed {
                Err(anyhow::anyhow!("TSM disconnect failed"))
            } else {
                Ok(())
            };
            let shutdown = if launcher_failed {
                Err(anyhow::anyhow!("launcher exit 134"))
            } else {
                Ok(())
            };
            let error = combine_guest_outcome(Ok(0), combine(teardown, shutdown)).unwrap_err();
            let text = format!("{error:#}");
            assert_eq!(text.contains("TSM disconnect failed"), teardown_failed);
            assert_eq!(text.contains("launcher exit 134"), launcher_failed);
        }
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

    struct FailedCapture(std::rc::Rc<std::cell::Cell<usize>>);

    impl Write for FailedCapture {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            self.0.set(self.0.get() + 1);
            Err(std::io::Error::from_raw_os_error(nix::libc::ENOSPC))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn capture_write_failure_drains_and_does_not_cancel_pending_wait() {
        for failure_on_open in [false, true] {
            let calls = std::rc::Rc::new(std::cell::Cell::new(0));
            let file = if failure_on_open {
                Err(anyhow::anyhow!("injected capture open failure"))
            } else {
                Ok(FailedCapture(calls.clone()))
            };
            let written = std::cell::Cell::new(false);
            let deadline = Deadline::new(seconds(5)).unwrap();
            let cancellation = Cancellation::default();
            let (reader, mut writer) = mesh::pipe::pipe();
            let (complete, completion) = mesh::oneshot();
            futures::executor::block_on(async {
                let wait = async {
                    // More than one read buffer: capture failure must not
                    // leave the simulated child blocked on its output pipe.
                    writer.write_all(&vec![0u8; 256 * 1024]).await?;
                    drop(writer);
                    written.set(true);
                    completion.await.context("wait transport failed")
                };
                let monitored = wait_and_drain(
                    wait,
                    drain_command_output_to(
                        reader,
                        file,
                        futures::io::sink(),
                        &deadline,
                        &cancellation,
                    ),
                    async { Ok(()) },
                );
                let mut monitored = std::pin::pin!(monitored);
                for _ in 0..128 {
                    assert!(futures::poll!(&mut monitored).is_pending());
                    if written.get() && (failure_on_open || calls.get() != 0) {
                        break;
                    }
                }
                assert!(written.get(), "capture failure blocked the child writer");
                assert_eq!(calls.get(), usize::from(!failure_on_open));
                // A short-circuiting join would have dropped this receiver.
                complete.send(23);
                let completed = monitored.await.unwrap();
                assert_eq!(completed.exit_code, 23);
                let error = completed.outcome().unwrap_err();
                let text = format!("{error:#}");
                assert!(text.contains("exit code 23"));
                assert!(text.contains(if failure_on_open {
                    "open failure"
                } else {
                    "capture log"
                }));
            });
        }
    }

    #[test]
    fn terminal_write_failure_is_discarded_until_confirmed_exit() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let deadline = Deadline::new(seconds(5)).unwrap();
        let cancellation = Cancellation::default();
        let (reader, writer) = mesh::pipe::pipe();
        writer.write_nonblocking(&vec![0u8; 16384]).unwrap();
        drop(writer);
        let completed = futures::executor::block_on(wait_and_drain(
            async { Ok(0) },
            drain_command_output_to(
                reader,
                Ok(Vec::new()),
                futures::io::AllowStdIo::new(FailedCapture(calls.clone())),
                &deadline,
                &cancellation,
            ),
            async { Ok(()) },
        ))
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(format!("{:#}", completed.outcome().unwrap_err()).contains("cannot forward"));
    }

    #[test]
    fn unknown_wait_outcome_never_authorizes_normal_shutdown() {
        let next_step = std::cell::Cell::new(false);
        let result = futures::executor::block_on(async {
            let completed = wait_and_drain(
                async { Err(anyhow::anyhow!("wait transport lost")) },
                async { Err(anyhow::anyhow!("capture write failed")) },
                async { Ok(()) },
            )
            .await?;
            next_step.set(true);
            completed.outcome()
        });
        let text = format!("{:#}", result.unwrap_err());
        assert!(!next_step.get());
        assert!(text.contains("completion is unknown"));
        assert!(text.contains("wait transport lost") && text.contains("capture write failed"));
    }

    #[test]
    fn pending_wait_after_capture_failure_uses_deadline_not_normal_shutdown() {
        let next_step = std::cell::Cell::new(false);
        DefaultPool::run_with(async |driver| {
            let deadline = Deadline::new(Duration::from_millis(50)).unwrap();
            let cancellation = Cancellation::default();
            let result = bounded(&driver, &deadline, &cancellation, async {
                let completed = wait_and_drain(
                    std::future::pending(),
                    async { Err(anyhow::anyhow!("capture write failed")) },
                    async { Ok(()) },
                )
                .await?;
                next_step.set(true);
                completed.outcome()
            })
            .await;
            assert!(format!("{:#}", result.unwrap_err()).contains("deadline"));
        });
        assert!(!next_step.get());
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
            petri_artifacts_common::cca_payload::LINUX_IMAGE_SHA256,
            directory.path(),
            &deadline,
            &cancellation,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("deadline"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
