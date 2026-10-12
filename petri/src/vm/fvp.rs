// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native FVP L1 backend. The incubator library owns model supervision, checked
//! staging, fixture teardown and durable diagnostics; Petri owns the live VM.

use super::*;
use crate::NoPetriVmFramebufferAccess;
use crate::NoPetriVmInspector;
use incubator::fvp::lifecycle::Cancellation;
use incubator::fvp::runtime::run_live;
use incubator::fvp::staging::SHARE_MANIFEST;

/// Pinned licensed Arm FVP CCA backend.
#[derive(Debug)]
pub struct FvpPetriBackend {
    package: ResolvedArtifact,
    payload: ResolvedArtifact,
    pipette: ResolvedArtifact,
}

/// FVP configuration. The firmware, model and host payload remain pinned.
#[derive(Default)]
pub struct FvpPetriConfig {}

/// A live L1, supervised on its own event-loop thread.
pub struct FvpPetriRuntime {
    ready: Option<mesh::OneshotReceiver<PipetteClient>>,
    finish: Option<mesh::OneshotSender<i32>>,
    nested_test_exit: Option<parking_lot::Mutex<Option<i32>>>,
    completion: Option<mesh::OneshotReceiver<anyhow::Result<()>>>,
    completed: Option<Result<(), String>>,
    cancellation: Cancellation,
    supervisor: Arc<SupervisorCompletion>,
    watchdog_timeout: Duration,
    cleanup_timeout: Duration,
}

impl Drop for FvpPetriRuntime {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Err(error) = self.supervisor.wait(self.cleanup_timeout) {
            tracing::error!(error = %error, "FVP supervisor cleanup did not complete successfully");
        }
    }
}

#[derive(Default)]
struct SupervisorCompletion {
    result: parking_lot::Mutex<Option<Result<(), String>>>,
    changed: parking_lot::Condvar,
}

impl SupervisorCompletion {
    fn publish(&self, result: &anyhow::Result<()>) {
        *self.result.lock() = Some(
            result
                .as_ref()
                .map(|_| ())
                .map_err(|error| format!("{error:#}")),
        );
        self.changed.notify_all();
    }

    fn wait(&self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .context("FVP cleanup wait deadline overflow")?;
        let mut result = self.result.lock();
        loop {
            if let Some(result) = result.as_ref() {
                return result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| anyhow::anyhow!("{error}"));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "FVP supervisor cleanup timed out after {timeout:?}; owned resources may remain"
            );
            self.changed.wait_for(&mut result, remaining);
        }
    }
}

fn lifecycle_budgets(deadlines: &incubator::FvpDeadlines) -> anyhow::Result<(Duration, Duration)> {
    deadlines.validate()?;
    // Post-verification, output preservation, retirement and outcome publication
    // each have a validation budget. Stop and pipette close use cleanup budgets.
    let finalization = deadlines.validation * 4 + deadlines.forced_cleanup * 2 + 30;
    // Admission and locked inventory have separate validation budgets.
    // Startup recovery uses cleanup, output preservation and retirement budgets.
    let lifecycle = deadlines.validation * 2
        + finalization
        + deadlines.forced_cleanup
        + deadlines.validation * 2
        + deadlines.model_lock
        + deadlines.port_allocation
        + deadlines.model_start
        + deadlines.pipette_ready
        + deadlines.dhcp
        + deadlines.test_execution
        + deadlines.guest_shutdown;
    Ok((
        Duration::from_secs(lifecycle),
        Duration::from_secs(finalization),
    ))
}

#[async_trait]
impl PetriVmmBackend for FvpPetriBackend {
    type VmmConfig = FvpPetriConfig;
    type VmRuntime = FvpPetriRuntime;
    const SUPPORTS_VMBUS: bool = false;
    const SUPPORTS_CPU_EMULATION: bool = true;
    const SUPPORTS_FILE_SHARING: bool = true;

    fn check_compat(firmware: &Firmware, arch: MachineArch) -> bool {
        arch == MachineArch::Aarch64 && matches!(firmware, Firmware::LinuxDirect { .. })
    }

    fn quirks(_: &Firmware) -> (GuestQuirksInner, VmmQuirks) {
        (GuestQuirksInner::default(), VmmQuirks::default())
    }

    fn default_servicing_flags() -> OpenHclServicingFlags {
        OpenHclServicingFlags {
            enable_nvme_keepalive: false,
            enable_mana_keepalive: false,
            override_version_checks: true,
            stop_timeout_hint_secs: None,
        }
    }

    fn create_guest_dump_disk() -> anyhow::Result<
        Option<(
            Arc<TempPath>,
            Box<dyn FnOnce() -> anyhow::Result<Box<dyn fatfs::ReadWriteSeek>>>,
        )>,
    > {
        Ok(None)
    }

    fn build_custom_init_script(_: &str, _: Vec<(String, String)>) -> Option<String> {
        // The checked CCA init script mounts the model's sole FM share. The live
        // session maps its subdirectories to Petri's mount-share contract.
        None
    }

    fn new(resolver: &ArtifactResolver<'_>, _: MachineArch) -> Self {
        use petri_artifacts_common::artifacts::*;
        Self {
            package: resolver.require(FVP_CCA_PLATFORM_AARCH64).erase(),
            payload: resolver.require(FVP_CCA_PAYLOAD_AARCH64).erase(),
            pipette: resolver.require(PIPETTE_LINUX_AARCH64_MUSL).erase(),
        }
    }

    async fn run(
        self,
        config: PetriVmConfig,
        modify: Option<ModifyFn<Self::VmmConfig>>,
        resources: &PetriVmResources,
        properties: &PetriVmProperties,
    ) -> anyhow::Result<(Self::VmRuntime, PetriVmRuntimeConfig)> {
        if let Some(modify) = modify {
            modify.0(FvpPetriConfig::default());
        }
        anyhow::ensure!(
            Self::check_compat(&config.firmware, config.arch),
            "FVP requires an AArch64 Linux L1"
        );
        let profile = incubator::IncubatorProfile::from_toml(include_str!(
            "../../incubator/profiles/aarch64-fvp-cca-realm-vfio.toml"
        ))?;
        let incubator::IncubatorBackend::FvpCca(fvp) = &profile.incubator else {
            anyhow::bail!("native FVP requires an FVP profile");
        };
        let (watchdog_timeout, cleanup_timeout) = lifecycle_budgets(&fvp.deadlines)?;
        let snapshot_deadline =
            incubator::fvp::process::Deadline::new(Duration::from_secs(fvp.deadlines.validation))?;
        let platform_root = std::env::var_os("PETRI_FVP_PLATFORM_ROOT")
            .context("PETRI_FVP_PLATFORM_ROOT must identify the pinned licensed toolchain")?;
        let output_dir = resources.log_source.output_dir().to_owned();
        incubator::fvp::runtime::validate_native_payload(self.payload.get())?;
        let share = tempfile::Builder::new()
            .prefix("fvp-inputs-")
            .tempdir_in(&output_dir)?;
        let mut inventory = Vec::new();
        copy_input(
            self.pipette.get(),
            &share.path().join("pipette"),
            share.path(),
            &output_dir,
            &mut inventory,
        )?;
        copy_input(
            &self.payload.get().join("host/Image"),
            &share.path().join("aarch64/Image"),
            share.path(),
            &output_dir,
            &mut inventory,
        )?;
        copy_input(
            &self.payload.get().join("host/initrd"),
            &share.path().join("aarch64/initrd"),
            share.path(),
            &output_dir,
            &mut inventory,
        )?;
        for (tag, path) in &config.guest_shares {
            anyhow::ensure!(
                matches!(tag.as_str(), NESTED_CONTENT_TAG | NESTED_IMAGES_TAG),
                "unsupported FVP guest share {tag}"
            );
            copy_input(
                path,
                &share.path().join(tag),
                share.path(),
                &output_dir,
                &mut inventory,
            )?;
        }
        if let Some(runner) = &properties.nested_test_binary {
            snapshot_nested_runner(
                runner,
                config
                    .guest_shares
                    .get(NESTED_CONTENT_TAG)
                    .context("nested FVP runner requires the content share")?,
                share.path(),
                &output_dir,
                &mut inventory,
                &snapshot_deadline,
            )?;
        }
        std::fs::write(
            share.path().join(SHARE_MANIFEST),
            serde_json::to_vec(&inventory)?,
        )?;
        let runtime_config = incubator::FvpCcaIncubatorConfig {
            profile,
            kernel: self.payload.get().join("host/Image"),
            initrd: self.payload.get().join("host/initrd"),
            share_dir: share.path().to_owned(),
            output_dir,
            result_file: None,
            guest_pipette_path: "/share/pipette".into(),
            guest_command: Vec::new(),
            guest_env: BTreeMap::new(),
            guest_current_dir: None,
            platform_root: platform_root.into(),
            shrinkwrap_package_root: self.package.get().to_owned(),
        };
        let (ready_send, ready) = mesh::oneshot();
        let (finish, finish_recv) = mesh::oneshot();
        let (complete_send, completion) = mesh::oneshot();
        let cancellation = Cancellation::default();
        let token = cancellation.clone();
        let supervisor = Arc::new(SupervisorCompletion::default());
        let worker_completion = supervisor.clone();
        let has_content = config.guest_shares.contains_key(NESTED_CONTENT_TAG);
        let has_images = config.guest_shares.contains_key(NESTED_IMAGES_TAG);
        std::thread::Builder::new().name("petri-fvp".into()).spawn(move || {
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let operation = Box::new(move |client: PipetteClient| -> futures::future::BoxFuture<'static, anyhow::Result<i32>> {
                Box::pin(async move {
                    let mut script = String::from("set -eu\nmkdir -p /cidata\n");
                    if has_content {
                        script.push_str("mkdir -p /cidata/vmm_tests /share/testcontent/test_results /share/testcontent/temp\nmount --bind /share/testcontent /cidata/vmm_tests\nmount --bind /share/test_results /cidata/vmm_tests/test_results\n");
                    }
                    if has_images {
                        script.push_str("mkdir -p /cidata/images\nmount --bind /share/testimages /cidata/images\n");
                    }
                    let setup = client.command("/bin/sh").arg("-c").arg(script).output().await?;
                    anyhow::ensure!(setup.status.success(), "FVP share setup failed: {}", String::from_utf8_lossy(&setup.stderr));
                    let fixture = client.command("/bin/cat").arg("/run/incubator-cca-realm-vfio").output().await?;
                    anyhow::ensure!(fixture.status.success() && fixture.stdout == b"0000:01:00.0\n", "FVP Realm VFIO fixture did not complete provisioning");
                    anyhow::ensure!(!ready_send.is_closed(), "Petri abandoned FVP readiness");
                    ready_send.send(client);
                    finish_recv.await.context("Petri abandoned the live FVP session")
                })
            });
            let outcome = run_live(runtime_config, operation, token).and_then(|output| {
                // The caller retains nested-test failure. This result describes
                // checked VM cleanup, not whether the nested test passed.
                anyhow::ensure!(output.exit_code.is_some(), "FVP session has no confirmed operation status");
                Ok(())
            });
            let cleanup = share.close().context("failed to remove FVP input snapshot");
            match (outcome, cleanup) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => Err(error.context(format!(
                    "FVP input snapshot cleanup also failed: {cleanup:#}"
                ))),
            }
            })).unwrap_or_else(|panic| {
                let message = panic.downcast_ref::<String>().map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic payload");
                Err(anyhow::anyhow!("FVP supervisor panicked: {message}; inspect durable lifecycle state for cleanup failures"))
            });
            worker_completion.publish(&outcome);
            complete_send.send(outcome);
        }).context("failed to start FVP supervisor")?;
        Ok((
            FvpPetriRuntime {
                ready: Some(ready),
                finish: Some(finish),
                nested_test_exit: properties
                    .nested_test_binary
                    .is_some()
                    .then(|| parking_lot::Mutex::new(None)),
                completion: Some(completion),
                completed: None,
                cancellation,
                supervisor,
                watchdog_timeout,
                cleanup_timeout,
            },
            config
                .firmware
                .into_runtime_config(config.vmbus_storage_controllers),
        ))
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "exclude actual output and snapshot subtrees even through path aliases"
)]
fn snapshot_nested_runner(
    source: &Path,
    content: &Path,
    root: &Path,
    output_root: &Path,
    inventory: &mut Vec<PathBuf>,
    deadline: &incubator::fvp::process::Deadline,
) -> anyhow::Result<()> {
    let source = absolute(source)?;
    let content = absolute(content)?;
    let relative = source
        .strip_prefix(&content)
        .context("nested FVP runner must be inside the content share")?;
    anyhow::ensure!(
        std::fs::symlink_metadata(&source)?.is_file(),
        "nested FVP runner must be a regular file, not a symlink"
    );
    anyhow::ensure!(
        !std::fs::canonicalize(&source)?.starts_with(std::fs::canonicalize(output_root)?),
        "nested FVP runner must not be inside the output subtree"
    );
    let entry = Path::new(NESTED_CONTENT_TAG).join(relative);
    if !inventory.contains(&entry) {
        incubator::fvp::staging::snapshot_additional_share_input(
            &content,
            &root.join(NESTED_CONTENT_TAG),
            relative,
            deadline,
            &Cancellation::default(),
        )?;
        inventory.push(entry);
    }
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "canonical input and output roots prevent recursive self-copy through path aliases"
)]
fn copy_input(
    source: &Path,
    target: &Path,
    root: &Path,
    output_root: &Path,
    inventory: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !std::fs::symlink_metadata(source)?.file_type().is_symlink(),
        "FVP input must not be a symlink: {}",
        source.display()
    );
    std::fs::create_dir_all(root)?;
    let relative = target
        .strip_prefix(root)
        .context("FVP copy target is outside the share")?;
    let source = std::fs::canonicalize(source)?;
    let root = std::fs::canonicalize(root)?;
    let output_root = std::fs::canonicalize(output_root)?;
    copy_input_tree(
        &source,
        &root.join(relative),
        &root,
        &output_root,
        inventory,
    )
}

fn copy_input_tree(
    source: &Path,
    target: &Path,
    root: &Path,
    output_root: &Path,
    inventory: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    if source.starts_with(root) || source.starts_with(output_root) {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        std::fs::create_dir_all(target)?;
        let relative = target.strip_prefix(root)?;
        if !relative.as_os_str().is_empty() {
            inventory.push(relative.to_owned());
        }
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            if matches!(entry.file_name().to_str(), Some("test_results" | "temp")) {
                continue;
            }
            copy_input_tree(
                &entry.path(),
                &target.join(entry.file_name()),
                root,
                output_root,
                inventory,
            )?;
        }
    } else {
        anyhow::ensure!(
            metadata.is_file(),
            "FVP input must be a regular file: {}",
            source.display()
        );
        std::fs::create_dir_all(target.parent().context("missing FVP input parent")?)?;
        std::fs::copy(source, target)?;
        inventory.push(target.strip_prefix(root)?.to_owned());
    }
    Ok(())
}

impl FvpPetriRuntime {
    async fn complete(&mut self) -> anyhow::Result<()> {
        if let Some(receiver) = self.completion.take() {
            let result = receiver
                .await
                .context("FVP supervisor did not report its cleanup outcome")
                .and_then(|result| result);
            self.completed = Some(result.map_err(|error| format!("{error:#}")));
        }
        match self.completed.as_ref() {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(anyhow::anyhow!("{error}")),
            None => anyhow::bail!("FVP supervisor completion is unavailable"),
        }
    }
}

#[async_trait]
impl PetriVmRuntime for FvpPetriRuntime {
    type VmInspector = NoPetriVmInspector;
    type VmFramebufferAccess = NoPetriVmFramebufferAccess;

    fn watchdog_timeout(&self) -> Duration {
        self.watchdog_timeout
    }

    fn record_nested_test_exit(&self, exit_code: i32) -> anyhow::Result<()> {
        if let Some(exit) = &self.nested_test_exit {
            let mut recorded = exit.lock();
            anyhow::ensure!(
                recorded.is_none(),
                "FVP nested test completion already recorded"
            );
            *recorded = Some(exit_code);
        }
        Ok(())
    }

    async fn teardown(mut self) -> anyhow::Result<()> {
        if self.completion.is_some() {
            self.cancellation.cancel();
        }

        self.complete().await
    }

    async fn wait_for_halt(&mut self, allow_reset: bool) -> anyhow::Result<PetriHaltReasonDetail> {
        anyhow::ensure!(!allow_reset, "FVP does not support guest reset");
        if let Some(finish) = self.finish.take() {
            let exit_code = match &self.nested_test_exit {
                Some(exit) => exit
                    .lock()
                    .take()
                    .context("FVP nested test has no confirmed exit status")?,
                None => 0,
            };
            finish.send(exit_code);
        }
        self.complete().await?;
        Ok(PetriHaltReasonDetail {
            reason: PetriHaltReason::PowerOff,
            detail: "Checked FVP fixture teardown and L1 shutdown".into(),
        })
    }

    async fn wait_for_agent(&mut self, high_vtl: bool) -> anyhow::Result<PipetteClient> {
        anyhow::ensure!(!high_vtl, "FVP has no OpenHCL agent");
        let ready = self.ready.take().context("FVP agent already taken")?;
        match ready.await {
            Ok(client) => Ok(client),
            Err(_) => {
                self.complete().await?;
                anyhow::bail!("FVP exited before pipette readiness")
            }
        }
    }

    fn openhcl_diag(&self) -> Option<OpenHclDiagHandler> {
        None
    }
    async fn wait_for_boot_event(
        &mut self,
        _: Option<Duration>,
    ) -> anyhow::Result<Option<FirmwareEvent>> {
        Ok(None)
    }
    async fn wait_for_enlightened_shutdown_ready(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("FVP has no shutdown IC")
    }
    async fn send_enlightened_shutdown(&mut self, _: ShutdownKind) -> anyhow::Result<()> {
        anyhow::bail!("FVP has no shutdown IC")
    }
    async fn restart_openhcl(
        &mut self,
        _: &ResolvedArtifact,
        _: OpenHclServicingFlags,
    ) -> anyhow::Result<()> {
        anyhow::bail!("FVP has no OpenHCL")
    }
    async fn save_openhcl(
        &mut self,
        _: &ResolvedArtifact,
        _: OpenHclServicingFlags,
    ) -> anyhow::Result<()> {
        anyhow::bail!("FVP has no OpenHCL")
    }
    async fn restore_openhcl(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("FVP has no OpenHCL")
    }
    async fn update_command_line(&mut self, _: &str) -> anyhow::Result<()> {
        anyhow::bail!("FVP uses a pinned host command line")
    }
    async fn reset(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("FVP does not support reset")
    }
    async fn set_vtl2_settings(&mut self, _: &Vtl2Settings) -> anyhow::Result<()> {
        anyhow::bail!("FVP has no OpenHCL")
    }
    async fn set_vmbus_drive(&mut self, _: &Drive, _: &Guid, _: u32) -> anyhow::Result<()> {
        anyhow::bail!("FVP has no VMBus")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn dropping_live_vm_requests_supervisor_cleanup() {
        let (_, completion) = mesh::oneshot();
        let token = Cancellation::default();
        let supervisor = Arc::new(SupervisorCompletion::default());
        let worker_completion = supervisor.clone();
        let worker_token = token.clone();
        let worker = std::thread::spawn(move || {
            while worker_token.check().is_ok() {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(10));
            worker_completion.publish(&Ok(()));
        });
        let runtime = FvpPetriRuntime {
            ready: None,
            finish: None,
            nested_test_exit: None,
            completion: Some(completion),
            completed: None,
            cancellation: token.clone(),
            supervisor: supervisor.clone(),
            watchdog_timeout: Duration::from_mins(10),
            cleanup_timeout: Duration::from_secs(2),
        };
        drop(runtime);
        assert!(token.check().is_err());
        supervisor.wait(Duration::ZERO).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn failed_fixture_or_model_shutdown_is_not_clean_halt() {
        let (sender, completion) = mesh::oneshot();
        sender.send(Err(anyhow::anyhow!("fixture teardown failed")));
        let supervisor = Arc::new(SupervisorCompletion::default());
        supervisor.publish(&Err(anyhow::anyhow!("fixture teardown failed")));
        let mut runtime = FvpPetriRuntime {
            ready: None,
            finish: None,
            nested_test_exit: None,
            completion: Some(completion),
            completed: None,
            cancellation: Cancellation::default(),
            supervisor,
            watchdog_timeout: Duration::from_mins(10),
            cleanup_timeout: Duration::from_secs(2),
        };
        let error = futures::executor::block_on(runtime.wait_for_halt(false)).unwrap_err();
        assert!(format!("{error:#}").contains("fixture teardown failed"));
        assert!(futures::executor::block_on(runtime.wait_for_halt(false)).is_err());
    }

    #[test]
    fn nested_failure_reports_actual_exit_before_checked_cleanup() {
        let (finish, finished) = mesh::oneshot();
        let (sender, completion) = mesh::oneshot();
        sender.send(Ok(()));
        let supervisor = Arc::new(SupervisorCompletion::default());
        supervisor.publish(&Ok(()));
        let mut runtime = FvpPetriRuntime {
            ready: None,
            finish: Some(finish),
            nested_test_exit: Some(parking_lot::Mutex::new(None)),
            completion: Some(completion),
            completed: None,
            cancellation: Cancellation::default(),
            supervisor,
            watchdog_timeout: Duration::from_mins(10),
            cleanup_timeout: Duration::from_secs(2),
        };
        runtime.record_nested_test_exit(101).unwrap();
        assert!(runtime.record_nested_test_exit(0).is_err());
        assert_eq!(
            futures::executor::block_on(runtime.wait_for_halt(false))
                .unwrap()
                .reason,
            PetriHaltReason::PowerOff
        );
        assert_eq!(futures::executor::block_on(finished).unwrap(), 101);
    }

    #[test]
    fn input_inventory_omits_previous_results_and_rejects_links() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        let output = directory.path().join("output");
        std::fs::create_dir_all(&output).unwrap();
        std::fs::create_dir_all(source.join("test_results")).unwrap();
        std::fs::write(source.join("test_results/previous"), "old").unwrap();
        std::fs::write(source.join("binary"), "input").unwrap();
        let mut inventory = Vec::new();
        copy_input(&source, &target, &target, &output, &mut inventory).unwrap();
        assert_eq!(inventory, [PathBuf::from("binary")]);
        assert!(!target.join("test_results").exists());
        std::os::unix::fs::symlink(source.join("binary"), source.join("link")).unwrap();
        assert!(copy_input(&source, &target, &target, &output, &mut Vec::new()).is_err());
    }

    #[test]
    fn supervisor_wait_preserves_errors_and_has_a_deadline() {
        let supervisor = SupervisorCompletion::default();
        assert!(
            supervisor
                .wait(Duration::from_millis(1))
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        supervisor.publish(&Err(anyhow::anyhow!("model cleanup failed")));
        assert!(
            supervisor
                .wait(Duration::ZERO)
                .unwrap_err()
                .to_string()
                .contains("model cleanup failed")
        );
    }

    #[test]
    fn fvp_watchdog_covers_qualified_phase_budgets() {
        let profile = incubator::IncubatorProfile::from_toml(include_str!(
            "../../incubator/profiles/aarch64-fvp-cca-realm-vfio.toml"
        ))
        .unwrap();
        let incubator::IncubatorBackend::FvpCca(profile) = profile.incubator else {
            panic!("wrong FVP profile");
        };
        let deadlines = profile.deadlines;
        let (watchdog, cleanup) = lifecycle_budgets(&deadlines).unwrap();
        assert_eq!(watchdog, Duration::from_secs(7350));
        assert_eq!(cleanup, Duration::from_secs(1290));
        let mut invalid = deadlines;
        invalid.validation = 0;
        assert!(lifecycle_budgets(&invalid).is_err());
    }

    #[test]
    fn input_inventory_excludes_nonstandard_output_inside_content() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("content");
        let output = source.join("custom-results");
        let root = output.join("fvp-inputs");
        let target = root.join("testcontent");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(source.join("ordinary")).unwrap();
        std::fs::write(source.join("ordinary/binary"), "input").unwrap();
        std::fs::write(output.join("previous.log"), "old").unwrap();
        let mut inventory = Vec::new();
        copy_input(&source, &target, &root, &output, &mut inventory).unwrap();
        assert_eq!(
            inventory,
            [
                PathBuf::from("testcontent"),
                PathBuf::from("testcontent/ordinary"),
                PathBuf::from("testcontent/ordinary/binary"),
            ]
        );
        assert_eq!(
            std::fs::read(target.join("ordinary/binary")).unwrap(),
            b"input"
        );
        assert!(!target.join("custom-results").exists());
    }

    #[test]
    fn input_inventory_preserves_empty_share_tag_roots_and_nested_directories() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let output = directory.path().join("output");
        let root = output.join("fvp-inputs");
        std::fs::create_dir_all(source.join("testimages")).unwrap();
        std::fs::create_dir_all(source.join("testcontent/empty/nested")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let mut inventory = Vec::new();
        for tag in ["testimages", "testcontent"] {
            copy_input(
                &source.join(tag),
                &root.join(tag),
                &root,
                &output,
                &mut inventory,
            )
            .unwrap();
        }
        assert_eq!(
            inventory,
            [
                PathBuf::from("testimages"),
                PathBuf::from("testcontent"),
                PathBuf::from("testcontent/empty"),
                PathBuf::from("testcontent/empty/nested"),
            ]
        );
        assert!(root.join("testimages").is_dir());
        assert_eq!(
            std::fs::read_dir(root.join("testimages")).unwrap().count(),
            0
        );
    }

    #[test]
    fn nested_runner_snapshot_preserves_guest_path_without_unrelated_temporary_inputs() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let content = directory.path().join("content");
        let output = content.join("custom-results");
        let root = output.join("fvp-inputs");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(content.join("temp/cache")).unwrap();
        std::fs::create_dir_all(content.join("test_results")).unwrap();
        let runner = content.join("temp/.tmp-selected-runner");
        std::fs::write(&runner, "selected").unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        for relative in [
            "temp/cache/unrelated",
            "temp/other-runner",
            "test_results/old.log",
            "custom-results/old.log",
        ] {
            std::fs::write(content.join(relative), "excluded").unwrap();
        }
        std::fs::write(content.join("ordinary"), "ordinary").unwrap();
        let mut inventory = Vec::new();
        copy_input(
            &content,
            &root.join(NESTED_CONTENT_TAG),
            &root,
            &output,
            &mut inventory,
        )
        .unwrap();
        assert!(!root.join("testcontent/temp").exists());
        let deadline = incubator::fvp::process::Deadline::new(Duration::from_secs(5)).unwrap();
        snapshot_nested_runner(&runner, &content, &root, &output, &mut inventory, &deadline)
            .unwrap();
        // Repeated inclusion must neither replace the snapshot nor duplicate its inventory.
        snapshot_nested_runner(&runner, &content, &root, &output, &mut inventory, &deadline)
            .unwrap();
        let relative = runner.strip_prefix(&content).unwrap();
        assert_eq!(
            Path::new("/cidata/vmm_tests").join(relative),
            Path::new("/cidata/vmm_tests/temp/.tmp-selected-runner")
        );
        let entry = Path::new(NESTED_CONTENT_TAG).join(relative);
        assert_eq!(inventory.iter().filter(|path| **path == entry).count(), 1);
        assert_eq!(std::fs::read(root.join(&entry)).unwrap(), b"selected");
        assert_eq!(
            root.join(&entry).metadata().unwrap().permissions().mode() & 0o777,
            0o755
        );
        for relative in [
            "temp/cache",
            "temp/other-runner",
            "test_results",
            "custom-results",
        ] {
            assert!(!root.join(NESTED_CONTENT_TAG).join(relative).exists());
        }
        assert_eq!(
            std::fs::read(root.join("testcontent/ordinary")).unwrap(),
            b"ordinary"
        );
    }

    #[test]
    fn nested_runner_snapshot_rejects_escape_output_directory_and_links() {
        let directory = tempfile::tempdir().unwrap();
        let content = directory.path().join("content");
        let output = content.join("custom-results");
        let root = output.join("fvp-inputs");
        std::fs::create_dir_all(root.join(NESTED_CONTENT_TAG)).unwrap();
        std::fs::create_dir_all(content.join("temp")).unwrap();
        let runner = content.join("temp/runner");
        std::fs::write(&runner, "selected").unwrap();
        std::fs::write(output.join("bad"), "output").unwrap();
        let outside = directory.path().join("outside");
        std::fs::write(&outside, "outside").unwrap();
        std::os::unix::fs::symlink(&runner, content.join("temp/link")).unwrap();
        std::os::unix::fs::symlink(content.join("temp"), content.join("alias")).unwrap();
        let deadline = incubator::fvp::process::Deadline::new(Duration::from_secs(5)).unwrap();
        let mut inventory = Vec::new();
        for input in [
            outside,
            output.join("bad"),
            content.join("temp"),
            content.join("temp/link"),
            content.join("alias/runner"),
        ] {
            assert!(
                snapshot_nested_runner(&input, &content, &root, &output, &mut inventory, &deadline)
                    .is_err(),
                "{}",
                input.display()
            );
        }
        assert!(inventory.is_empty());
        assert_eq!(
            std::fs::read_dir(root.join(NESTED_CONTENT_TAG))
                .unwrap()
                .count(),
            0
        );
    }
}
