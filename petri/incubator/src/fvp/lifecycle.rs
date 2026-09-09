// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Persistent ownership and bounded supervision for FVP runs.
//!
//! The model lock is a permanent inode. Failed cleanup keeps state for the next
//! owner. Recovery never signals a PID read from disk: without a retained child
//! or pidfd, even a matching `/proc` identity cannot close the PID-reuse race.
//! Instead, recovery removes exactly labelled containers and waits for their
//! launcher to exit. An unverifiable or surviving orphan requires intervention.

use super::process::Deadline;
use super::process::ManagedChild;
use super::process::run_command_cancellable;
use super::process::run_command_with_cleanup;
use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

const SCHEMA_VERSION: u32 = 1;
const STATE_KIND: &str = "openvmm-fvp";
const MODEL_ROLE: &str = "model";
const STATE_LIMIT: u64 = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const RUN_LABEL: &str = "io.openvmm.fvp.run-id";
const SCHEMA_LABEL: &str = "io.openvmm.fvp.schema-version";
const IMAGE_LABEL: &str = "io.openvmm.fvp.expected-image";
const ROLE_LABEL: &str = "io.openvmm.fvp.role";
pub(super) const WORKSPACE_MARKER: &str = ".openvmm-fvp-workspace.json";
const OUTPUT_MARKER: &str = ".openvmm-fvp-outputs.json";
const TREE_ENTRY_LIMIT: usize = 4096;
const TREE_BYTE_LIMIT: u64 = 1024 * 1024 * 1024;

/// A cryptographically random identity, not a PID or timestamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Generate a fresh 256-bit identity.
    pub fn new() -> anyhow::Result<Self> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow::anyhow!("failed to generate FVP run identity: {error}"))?;
        Ok(Self(hex::encode(bytes)))
    }

    /// The controlled, lowercase hexadecimal representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(is_hex_id(&self.0), "invalid FVP run identity");
        Ok(())
    }
}

fn is_hex_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Cancellation can be driven by signals or explicitly by the caller.
#[derive(Clone, Default)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
}

impl Cancellation {
    /// Request cancellation. Supervised polling observes this without a reset.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Fail when the owning invocation was cancelled.
    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cancelled.load(Ordering::Relaxed), "FVP run cancelled");
        Ok(())
    }

    /// Keep this guard alive for the complete lock, run, and cleanup interval.
    ///
    /// The first call takes process-lifetime ownership of SIGINT and SIGTERM.
    /// Active guards receive cancellation. After the last guard drops, permanent
    /// conditional handlers emulate each signal's default termination action.
    /// Do not combine this owner with another application termination dispatcher.
    pub fn install_signal_handlers(&self) -> anyhow::Result<SignalGuard> {
        let ownership = signal_ownership()?;
        let mut active = ownership.active.lock();
        let next = active
            .checked_add(1)
            .context("too many FVP signal guards")?;
        let mut guard = SignalGuard {
            ownership: ownership.clone(),
            registrations: Vec::new(),
            active: false,
        };
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            guard.registrations.push(
                signal_hook::flag::register(signal, self.cancelled.clone())
                    .context("failed to install FVP cancellation handler")?,
            );
        }
        *active = next;
        ownership.inactive.store(false, Ordering::SeqCst);
        guard.active = true;
        Ok(guard)
    }
}

struct SignalOwnership {
    inactive: Arc<AtomicBool>,
    active: parking_lot::Mutex<usize>,
}

fn signal_ownership() -> anyhow::Result<Arc<SignalOwnership>> {
    static OWNERSHIP: OnceLock<Result<Arc<SignalOwnership>, String>> = OnceLock::new();
    OWNERSHIP
        .get_or_init(|| {
            let ownership = Arc::new(SignalOwnership {
                inactive: Arc::new(AtomicBool::new(true)),
                active: Default::default(),
            });
            for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
                // These registrations intentionally last for the process.
                // This safe helper invokes emulate_default_handler when no
                // guards are active, including after partial setup failure.
                signal_hook::flag::register_conditional_default(signal, ownership.inactive.clone())
                    .map_err(|error| error.to_string())?;
            }
            Ok(ownership)
        })
        .clone()
        .map_err(|error| anyhow::anyhow!("failed to establish FVP signal ownership: {error}"))
}

/// Owns this invocation's cancellation registrations, not the OS dispositions.
/// The process-lifetime owner resumes default termination after the last guard.
pub struct SignalGuard {
    ownership: Arc<SignalOwnership>,
    registrations: Vec<signal_hook::SigId>,
    active: bool,
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        let mut active = self.active.then(|| self.ownership.active.lock());
        if let Some(active) = &mut active {
            assert!(**active > 0, "FVP signal guard count underflow");
            **active -= 1;
            // Restore default behavior before removing the last cancellation
            // action. Removing signal-hook actions alone leaves signals ignored.
            self.ownership
                .inactive
                .store(**active == 0, Ordering::SeqCst);
        }
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn pause(deadline: &Deadline, cancellation: &Cancellation) -> anyhow::Result<()> {
    cancellation.check()?;
    std::thread::sleep(deadline.remaining()?.min(POLL_INTERVAL));
    cancellation.check()
}

/// Stable private storage, independent of either input root or staging.
#[derive(Debug)]
pub struct RuntimeDirectory {
    path: PathBuf,
}

impl RuntimeDirectory {
    /// Select persistent per-user storage that survives logout and host reboot.
    /// Volatile `XDG_RUNTIME_DIR` storage must not hold recovery state.
    pub fn default_base() -> anyhow::Result<PathBuf> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
        anyhow::ensure!(home.is_absolute(), "HOME must be absolute");
        Ok(home.join(".local/state/openvmm"))
    }

    /// Open `base/fvp-v1`. `excluded` must include both canonical input roots
    /// and the per-run workspace root, including one that does not yet exist.
    pub fn open(base: &Path, excluded: &[&Path]) -> anyhow::Result<Self> {
        anyhow::ensure!(base.is_absolute(), "FVP runtime base must be absolute");
        let path = resolve_existing_ancestor(&base.join("fvp-v1"))?;
        for excluded in excluded {
            let excluded = resolve_existing_ancestor(excluded)?;
            anyhow::ensure!(
                !path.starts_with(&excluded) && !excluded.starts_with(&path),
                "FVP runtime directory overlaps an input or per-run root"
            );
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .context("failed to create stable FVP runtime directory")?;
        let metadata = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(metadata.is_dir(), "FVP runtime root is not a directory");
        validate_private_metadata(&metadata)?;
        anyhow::ensure!(
            canonical(&path)? == path,
            "FVP runtime directory changed while opening"
        );
        Ok(Self { path })
    }

    /// Stable directory containing the lock, state, and retained logs.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Acquire the permanent lock inode. Zero means one non-blocking attempt.
    pub fn lock(
        self,
        timeout: Duration,
        cancellation: &Cancellation,
    ) -> anyhow::Result<RuntimeLock> {
        let path = self.path.join("model.lock");
        let file = acquire_lock_file(&path, false, timeout, cancellation)
            .context("FVP model lock acquisition failed")?;
        Ok(RuntimeLock {
            directory: self,
            file,
        })
    }

    /// Hold this shared guard from before inventory validation until after
    /// execution, cleanup, and toolchain revalidation. Acquire it before the
    /// model lock. Cooperating toolchain writers must take an exclusive lock on
    /// the same persistent inode; this guard does not own either input root.
    pub fn lock_toolchain(
        &self,
        timeout: Duration,
        cancellation: &Cancellation,
    ) -> anyhow::Result<ToolchainUseLock> {
        let path = self.path.join("toolchain.lock");
        let file = acquire_lock_file(&path, true, timeout, cancellation)
            .context("FVP toolchain-use lock acquisition failed")?;
        Ok(ToolchainUseLock { path, file })
    }
}

fn acquire_lock_file(
    path: &Path,
    shared: bool,
    timeout: Duration,
    cancellation: &Cancellation,
) -> anyhow::Result<File> {
    anyhow::ensure!(
        timeout <= Duration::from_secs(2 * 60 * 60),
        "FVP lock deadline exceeds two hours"
    );
    let deadline = Deadline::new(timeout)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .context("failed to open persistent FVP lock")?;
    validate_private_file(&file)?;
    loop {
        cancellation.check()?;
        if !timeout.is_zero() {
            deadline
                .remaining()
                .context("FVP lock acquisition timed out")?;
        }
        let result = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match result {
            Ok(()) => {
                check_lock_inode(path, &file)?;
                cancellation.check()?;
                if !timeout.is_zero() {
                    deadline
                        .remaining()
                        .context("FVP lock acquisition timed out")?;
                }
                return Ok(file);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                if timeout.is_zero() || deadline.expired() {
                    anyhow::bail!("FVP lock acquisition timed out");
                }
                pause(&deadline, cancellation)
                    .context("FVP lock acquisition timed out or cancelled")?;
            }
            Err(error) => return Err(error).context("failed to lock persistent FVP inode"),
        }
    }
}

fn check_lock_inode(path: &Path, file: &File) -> anyhow::Result<()> {
    let actual = std::fs::symlink_metadata(path)?;
    let held = file.metadata()?;
    anyhow::ensure!(
        actual.dev() == held.dev() && actual.ino() == held.ino(),
        "persistent FVP lock inode was replaced: {}",
        path.display()
    );
    Ok(())
}

/// Shared toolchain-use lock, independent of model and session lifetimes.
/// Dropping it releases the lock without unlinking its persistent file.
pub struct ToolchainUseLock {
    path: PathBuf,
    file: File,
}

impl ToolchainUseLock {
    /// Check that another process has not replaced the lock inode.
    pub fn check(&self) -> anyhow::Result<()> {
        check_lock_inode(&self.path, &self.file)
    }

    /// Persistent path for cooperating exclusive toolchain writers.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "Ownership and containment checks must resolve symlinks, not only make paths absolute."
)]
fn canonical(path: &Path) -> anyhow::Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to resolve FVP path {}", path.display()))
}

pub(crate) fn resolve_existing_ancestor(path: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(path.is_absolute(), "FVP root must be absolute");
    anyhow::ensure!(
        !path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir)),
        "FVP runtime roots cannot contain parent traversal"
    );
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(existing.file_name().context("invalid FVP runtime path")?);
                existing = existing.parent().context("invalid FVP runtime parent")?;
            }
            Err(error) => return Err(error).context("failed to inspect FVP runtime root"),
        }
    }
    let mut resolved = canonical(existing)?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn validate_private_metadata(metadata: &std::fs::Metadata) -> anyhow::Result<()> {
    anyhow::ensure!(
        metadata.uid() == std::fs::metadata("/proc/self")?.uid() && metadata.mode() & 0o077 == 0,
        "FVP runtime resource must be private and owned by the current user"
    );
    Ok(())
}

fn validate_private_file(file: &File) -> anyhow::Result<()> {
    let metadata = file.metadata()?;
    validate_private_metadata(&metadata)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.nlink() == 1,
        "FVP runtime resource must be a regular file with one link"
    );
    Ok(())
}

/// The lock remains held until this value is dropped. Never unlink its file.
pub struct RuntimeLock {
    directory: RuntimeDirectory,
    file: File,
}

impl RuntimeLock {
    fn check_inode(&self) -> anyhow::Result<()> {
        check_lock_inode(&self.directory.path.join("model.lock"), &self.file)
    }

    fn state_path(&self) -> PathBuf {
        self.directory.path.join("state.json")
    }

    /// Parse state without repairing, deleting, or overwriting invalid records.
    pub fn read_state(&self) -> anyhow::Result<Option<RunState>> {
        self.check_inode()?;
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(self.state_path())
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("failed to open FVP state; preserving it"),
        };
        validate_private_file(&file)?;
        anyhow::ensure!(
            file.metadata()?.len() <= STATE_LIMIT,
            "FVP state is too large; preserving it"
        );
        let mut bytes = Vec::new();
        (&mut file).take(STATE_LIMIT + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() as u64 <= STATE_LIMIT, "FVP state is too large");
        let state = RunState::parse(&bytes).context("invalid FVP state; preserving it")?;
        Ok(Some(state))
    }

    fn write_state(&self, old: Option<&RunState>, state: &RunState) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.read_state()?.as_ref() == old,
            "FVP state ownership changed; preserving it"
        );
        state.validate()?;
        let bytes = serde_json::to_vec(state)?;
        anyhow::ensure!(
            (bytes.len() as u64) < STATE_LIMIT,
            "FVP state exceeds the bounded state format"
        );
        let path = self
            .directory
            .path
            .join(format!(".state-{}.new", RunId::new()?.as_str()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let result = (|| {
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&path, self.state_path())?;
            File::open(&self.directory.path)?.sync_all()?;
            anyhow::Ok(())
        })();
        if result.is_err() && path.try_exists()? {
            std::fs::remove_file(&path).context("failed to remove owned incomplete FVP state")?;
        }
        result.context("failed to atomically persist FVP state")
    }

    fn remove_state(&self, expected: &RunState) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.read_state()?.as_ref() == Some(expected),
            "FVP state ownership changed; preserving it"
        );
        std::fs::remove_file(self.state_path())?;
        File::open(&self.directory.path)?.sync_all()?;
        Ok(())
    }

    /// Recover after owner death. No host PID from disk is ever signalled.
    ///
    /// The trusted expected image and executables come from this invocation's
    /// validated tuple, not the state file. Foreign, newer, corrupt, reused-PID,
    /// or still-live-owner state remains untouched.
    /// Earlier-boot identities never authorize current-boot PID or group probes.
    /// Process cleanup, output preservation, and filesystem retirement each use
    /// one non-resetting `timeout` budget, as in normal finalization.
    pub fn recover(
        &self,
        docker: &Docker,
        expected_image: &str,
        expected_model: &Path,
        timeout: Duration,
        cancellation: &Cancellation,
    ) -> anyhow::Result<()> {
        let deadline = Deadline::new(timeout)?;
        cancellation.check()?;
        let Some(mut state) = self.read_state()? else {
            return Ok(());
        };
        anyhow::ensure!(
            state.expected_image == expected_image
                && state.expected_model == canonical(expected_model)?
                && state.owner.executable == canonical(&std::env::current_exe()?)?,
            "foreign FVP state does not match this launcher and platform; preserving it"
        );
        anyhow::ensure!(
            state.owner.observe()? == ProcessObservation::Gone,
            "FVP state owner is live or its PID was reused; preserving it"
        );
        anyhow::ensure!(
            state.post_verification != PostVerification::InFlight,
            "FVP owner died during post-verification; preserving recovery state"
        );
        anyhow::ensure!(
            !state.launch_pending,
            "FVP owner died during process registration; an unrecorded launcher may exist; \
             preserving state; manual intervention is required"
        );
        anyhow::ensure!(
            state.callback_phase == CallbackPhase::Idle,
            "FVP owner died during {:?} callback; unregistered helpers may exist; \
             preserving state; manual intervention is required: runtime={}, run={}, \
             owner_pid={}, owner_start_ticks={}",
            state.callback_phase,
            self.directory.path.display(),
            state.run_id.as_str(),
            state.owner.pid,
            state.owner.start_ticks,
        );
        if let Some(process) = &state.model {
            anyhow::ensure!(
                process.observe()? != ProcessObservation::Reused,
                "FVP model PID was reused; preserving state and resources"
            );
        }
        let containers = docker.cleanup(&state, &deadline)?;
        if let Some(process) = &state.model
            && process.boot_id == current_boot_id()?
        {
            loop {
                match process.observe()? {
                    ProcessObservation::Gone if !process_group_exists(process.pid)? => break,
                    ProcessObservation::Gone => {
                        pause(&deadline, cancellation).with_context(|| {
                            format!(
                                "FVP orphan process group survived; preserving state; \
                                 manual intervention is required: runtime={}, run={}, \
                                 pid={}, start_ticks={}, containers={containers:?}",
                                self.directory.path.display(),
                                state.run_id.as_str(),
                                process.pid,
                                process.start_ticks,
                            )
                        })?
                    }
                    ProcessObservation::Reused => {
                        anyhow::bail!("FVP orphan PID was reused; preserving state");
                    }
                    ProcessObservation::Matching => {
                        pause(&deadline, cancellation).with_context(|| {
                            format!(
                                "FVP orphan launcher survived container cleanup; preserving state; \
                                 manual intervention is required: runtime={}, run={}, \
                                 pid={}, start_ticks={}, containers={containers:?}",
                                self.directory.path.display(),
                                state.run_id.as_str(),
                                process.pid,
                                process.start_ticks,
                            )
                        })?
                    }
                }
            }
        }
        cancellation.check()?;
        deadline
            .remaining()
            .context("FVP recovery deadline exceeded; preserving state")?;
        let output_deadline = Deadline::new(timeout)?;
        let mut recovered = state.clone();
        recovered.resources_stopped = true;
        if let Some(workspace) = &mut recovered.workspace
            && workspace.outputs.is_none()
        {
            workspace.outputs = Some(workspace.seal_outputs(&output_deadline, true)?);
        }
        if recovered.post_verification == PostVerification::Pending {
            recovered.post_verification = PostVerification::Failed;
            tracing::warn!(
                run_id = state.run_id.as_str(),
                "recovering interrupted FVP resources; the previous run was not qualified"
            );
        }
        if recovered != state {
            self.write_state(Some(&state), &recovered)?;
            state = recovered;
        }
        output_deadline.remaining()?;
        let deadline = Deadline::new(timeout)?;
        if let Some(workspace) = state.workspace.clone() {
            workspace.preflight(&deadline)?;
            if !workspace.removal_started {
                let mut next = state.clone();
                next.resources_stopped = true;
                next.workspace
                    .as_mut()
                    .context("missing FVP workspace")?
                    .removal_started = true;
                self.write_state(Some(&state), &next)?;
                state = next;
            }
            workspace.remove(&deadline)?;
            let mut next = state.clone();
            next.workspace = None;
            self.write_state(Some(&state), &next)?;
            state = next;
        }
        self.remove_state(&state)
    }
}

fn process_group_exists(pid: u32) -> anyhow::Result<bool> {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(pid).context("invalid FVP process group")?);
    // Signal zero probes existence. It never signals a potentially reused PID.
    match nix::sys::signal::killpg(pid, None) {
        Ok(()) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(error).context("cannot prove that the FVP orphan process group is gone"),
    }
}

fn valid_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

fn current_boot_id() -> anyhow::Result<String> {
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("cannot read the Linux boot identity")?
        .trim()
        .to_owned();
    anyhow::ensure!(valid_boot_id(&boot_id), "invalid Linux boot identity");
    Ok(boot_id)
}

/// Persistent Linux identity. Start time is scoped to the recorded boot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pid: u32,
    start_ticks: u64,
    boot_id: String,
    executable: PathBuf,
    run_id: Option<RunId>,
}

#[derive(Debug, PartialEq, Eq)]
enum ProcessObservation {
    Gone,
    Matching,
    Reused,
}

impl ProcessIdentity {
    /// Capture identity twice around the executable lookup to detect reuse.
    pub fn capture(pid: u32, expected_executable: &Path) -> anyhow::Result<Self> {
        let identity = Self::read(pid)?.context("FVP process exited before identity capture")?;
        anyhow::ensure!(
            identity.executable == canonical(expected_executable)?,
            "FVP process executable does not match the expected executable"
        );
        Ok(identity)
    }

    fn read(pid: u32) -> anyhow::Result<Option<Self>> {
        anyhow::ensure!(pid > 1 && pid <= i32::MAX as u32, "invalid FVP process PID");
        let path = PathBuf::from(format!("/proc/{pid}"));
        let boot_id = current_boot_id()?;
        let read = || -> anyhow::Result<Self> {
            let before = std::fs::read_to_string(path.join("stat"))?;
            let start_ticks = process_start_ticks(&before, pid)?;
            let executable = std::fs::read_link(path.join("exe"))?;
            let mut environment = Vec::new();
            File::open(path.join("environ"))?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut environment)?;
            anyhow::ensure!(
                environment.len() <= 1024 * 1024,
                "FVP process environment is too large"
            );
            let mut run_id = None;
            for entry in environment.split(|byte| *byte == 0) {
                if let Some(value) = entry.strip_prefix(b"OPENVMM_FVP_RUN_ID=") {
                    anyhow::ensure!(run_id.is_none(), "duplicate FVP process run identity");
                    let id = RunId(std::str::from_utf8(value)?.to_owned());
                    id.validate()?;
                    run_id = Some(id);
                }
            }
            let after = std::fs::read_to_string(path.join("stat"))?;
            anyhow::ensure!(
                process_start_ticks(&after, pid)? == start_ticks,
                "FVP process PID changed during identity capture"
            );
            Ok(Self {
                pid,
                start_ticks,
                boot_id,
                executable,
                run_id,
            })
        };
        match read() {
            Ok(identity) => Ok(Some(identity)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                match std::fs::read_to_string(path.join("stat")) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Ok(stat)
                        if stat
                            .rsplit_once(") ")
                            .and_then(|(_, fields)| fields.split_whitespace().next())
                            .is_some_and(|state| state == "Z" || state == "X") =>
                    {
                        // A zombie has no executable link and cannot execute or fork.
                        Ok(None)
                    }
                    _ => Err(error).context("FVP process exists but its identity is unavailable"),
                }
            }
            Err(error) => Err(error).context("cannot prove FVP process identity"),
        }
    }

    fn observe(&self) -> anyhow::Result<ProcessObservation> {
        anyhow::ensure!(
            valid_boot_id(&self.boot_id),
            "invalid recorded Linux boot identity"
        );
        if self.boot_id != current_boot_id()? {
            // PIDs and process groups from an earlier boot cannot survive.
            return Ok(ProcessObservation::Gone);
        }
        let Some(actual) = Self::read(self.pid)? else {
            return Ok(ProcessObservation::Gone);
        };
        Ok(if actual == *self {
            ProcessObservation::Matching
        } else {
            ProcessObservation::Reused
        })
    }
}

fn process_start_ticks(stat: &str, expected_pid: u32) -> anyhow::Result<u64> {
    let (pid, _) = stat.split_once(" (").context("invalid /proc PID header")?;
    anyhow::ensure!(pid.parse::<u32>()? == expected_pid, "unexpected /proc PID");
    // comm can contain whitespace and parentheses, including a closing one.
    let (_, fields) = stat.rsplit_once(") ").context("invalid /proc comm field")?;
    let ticks = fields
        .split_whitespace()
        .nth(19)
        .context("missing /proc process start time")?
        .parse::<u64>()?;
    anyhow::ensure!(ticks != 0, "invalid zero process start time");
    Ok(ticks)
}

fn capture_owned_process(
    child: &mut ManagedChild,
    expected_executable: &Path,
    run_id: &RunId,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<ProcessIdentity> {
    // Process creation and /proc identity publication are not one atomic
    // observation. Registration must stay inside the launch's original budget.
    loop {
        cancellation.check()?;
        deadline
            .remaining()
            .context("FVP process registration failed: deadline exceeded")?;
        anyhow::ensure!(
            child.try_wait()?.is_none(),
            "FVP launcher exited before process registration"
        );
        let error = match ProcessIdentity::capture(child.id(), expected_executable) {
            Ok(identity) if identity.run_id.as_ref() == Some(run_id) => {
                deadline
                    .remaining()
                    .context("FVP process registration failed: deadline exceeded")?;
                return Ok(identity);
            }
            Ok(_) => anyhow::anyhow!("FVP process run identity does not match its owner"),
            Err(error) => error,
        };
        pause(deadline, cancellation)
            .with_context(|| format!("FVP process registration failed: {error:#}"))?;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CallbackPhase {
    Idle,
    Preparation,
    Confirmation,
    ReadinessProbe,
    TestDispatch,
    ShutdownRequest,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PostVerification {
    #[default]
    NotRequired,
    Pending,
    InFlight,
    Complete,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryStamp {
    device: u64,
    inode: u64,
    uid: u32,
    mode: u32,
}

impl DirectoryStamp {
    fn read(file: &File) -> anyhow::Result<Self> {
        let metadata = file.metadata()?;
        anyhow::ensure!(metadata.is_dir(), "FVP ownership path is not a directory");
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
            mode: metadata.mode() & 0o7777,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedDirectory {
    path: PathBuf,
    ancestors: Vec<DirectoryStamp>,
}

struct DirectoryHandle {
    file: File,
    parent: File,
    name: OsString,
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn directory_components(path: &Path) -> anyhow::Result<Vec<OsString>> {
    anyhow::ensure!(path.is_absolute(), "FVP ownership path must be absolute");
    let components = path
        .components()
        .skip(1)
        .map(|component| match component {
            std::path::Component::Normal(name) => Ok(name.to_owned()),
            _ => anyhow::bail!("FVP ownership path contains traversal"),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        !components.is_empty() && components.len() < 64,
        "FVP ownership path is a root or is too deep"
    );
    Ok(components)
}

fn open_directory(path: &Path) -> anyhow::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .with_context(|| {
            format!(
                "cannot open FVP directory without following links: {}",
                path.display()
            )
        })
}

impl OwnedDirectory {
    fn capture(path: &Path) -> anyhow::Result<Self> {
        let components = directory_components(path)?;
        anyhow::ensure!(
            canonical(path)? == path,
            "FVP ownership path has symlink parents"
        );
        path.to_str().context("FVP ownership path is not UTF-8")?;
        let mut file = open_directory(Path::new("/"))?;
        let mut ancestors = vec![DirectoryStamp::read(&file)?];
        for name in components {
            file = open_directory(&fd_path(&file).join(name))?;
            ancestors.push(DirectoryStamp::read(&file)?);
        }
        validate_private_metadata(&file.metadata()?)?;
        Ok(Self {
            path: path.to_owned(),
            ancestors,
        })
    }

    fn validate(&self) -> anyhow::Result<()> {
        let components = directory_components(&self.path)?;
        anyhow::ensure!(
            self.ancestors.len() == components.len() + 1,
            "invalid FVP directory ancestry record"
        );
        let leaf = self
            .ancestors
            .last()
            .context("missing FVP directory identity")?;
        anyhow::ensure!(leaf.mode & 0o077 == 0, "FVP owned directory is not private");
        Ok(())
    }

    fn open(&self, allow_missing_leaf: bool) -> anyhow::Result<Option<DirectoryHandle>> {
        self.validate()?;
        let components = directory_components(&self.path)?;
        let mut file = open_directory(Path::new("/"))?;
        anyhow::ensure!(
            self.ancestors.first() == Some(&DirectoryStamp::read(&file)?),
            "FVP directory root identity changed; preserving resources"
        );
        let mut parent = file.try_clone()?;
        let mut leaf_name = OsString::new();
        for (index, name) in components.iter().enumerate() {
            let next = match open_directory(&fd_path(&file).join(name)) {
                Ok(next) => next,
                Err(error)
                    if allow_missing_leaf
                        && index + 1 == components.len()
                        && error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            anyhow::ensure!(
                self.ancestors.get(index + 1) == Some(&DirectoryStamp::read(&next)?),
                "FVP directory identity or parent changed; preserving resources: {}",
                self.path.display()
            );
            parent = file;
            file = next;
            leaf_name = name.clone();
        }
        validate_private_metadata(&file.metadata()?)?;
        Ok(Some(DirectoryHandle {
            file,
            parent,
            name: leaf_name,
        }))
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryMarker {
    schema_version: u32,
    kind: String,
    run_id: RunId,
    directory: OwnedDirectory,
}

fn directory_marker(directory: &OwnedDirectory, run_id: &RunId, kind: &str) -> DirectoryMarker {
    DirectoryMarker {
        schema_version: SCHEMA_VERSION,
        kind: kind.to_owned(),
        run_id: run_id.clone(),
        directory: directory.clone(),
    }
}

fn write_directory_marker(
    handle: &DirectoryHandle,
    name: &str,
    marker: &DirectoryMarker,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(marker)?;
    anyhow::ensure!(
        bytes.len() as u64 <= STATE_LIMIT,
        "FVP directory marker is too large"
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(fd_path(&handle.file).join(name))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    handle.file.sync_all()?;
    Ok(())
}

fn verify_directory_marker(
    handle: &DirectoryHandle,
    name: &str,
    expected: &DirectoryMarker,
) -> anyhow::Result<()> {
    let identity = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_PATH | nix::libc::O_NOFOLLOW)
        .open(fd_path(&handle.file).join(name))
        .context("FVP owner marker is missing or inaccessible; preserving directory")?;
    validate_private_file(&identity)?;
    let file = File::open(fd_path(&identity))?;
    let mut bytes = Vec::new();
    file.take(STATE_LIMIT + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= STATE_LIMIT,
        "FVP owner marker is too large"
    );
    let marker: DirectoryMarker = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        marker == *expected,
        "foreign, changed, or newer FVP directory marker; preserving directory"
    );
    Ok(())
}

fn mount_id(file: &File) -> anyhow::Result<u64> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .context("missing Linux mount identity")?
        .trim()
        .parse()
        .context("invalid Linux mount identity")
}

fn ensure_persistent_output(
    directory: &OwnedDirectory,
    handle: &DirectoryHandle,
) -> anyhow::Result<()> {
    ensure_persistent_filesystem(&directory.path, &handle.file)
}

/// Check an output location before creating session state or directories.
pub(crate) fn validate_output_location(path: &Path) -> anyhow::Result<()> {
    let mut ancestor = path;
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => return ensure_persistent_filesystem(path, &open_directory(ancestor)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .context("FVP output has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("cannot inspect FVP output location"),
        }
    }
}

fn ensure_persistent_filesystem(path: &Path, file: &File) -> anyhow::Result<()> {
    anyhow::ensure!(
        !path.starts_with("/tmp") && !path.starts_with("/run"),
        "FVP outputs require a persistent destination outside /tmp and /run"
    );
    let prefix = format!("{} ", mount_id(file)?);
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
    let filesystem = mounts
        .lines()
        .find(|line| line.starts_with(&prefix))
        .and_then(|line| line.split_once(" - "))
        .and_then(|(_, fields)| fields.split_whitespace().next())
        .context("cannot prove FVP output filesystem identity")?;
    anyhow::ensure!(
        !matches!(
            filesystem,
            "tmpfs" | "ramfs" | "devtmpfs" | "proc" | "sysfs"
        ),
        "FVP output destination is on a volatile filesystem"
    );
    Ok(())
}

fn validate_workspace_name(path: &Path) -> anyhow::Result<()> {
    let suffix = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("openvmm-fvp-"))
        .context("FVP workspace must have an allocated openvmm-fvp- name")?;
    anyhow::ensure!(
        (6..=128).contains(&suffix.len())
            && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()),
        "invalid allocated FVP workspace name"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntry {
    relative: PathBuf,
    parents: Vec<(OsString, DirectoryStamp)>,
    device: u64,
    inode: u64,
    directory: bool,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

#[derive(Debug)]
struct TreeSnapshot {
    entries: Vec<TreeEntry>,
    digest: String,
    regular_files: usize,
}

/// Original identity and contents of an allocation not yet handed to a session.
#[derive(Debug)]
pub(super) struct WorkspaceAllocation {
    directory: OwnedDirectory,
    scaffold: TreeSnapshot,
}

impl WorkspaceAllocation {
    pub(super) fn capture(path: &Path, deadline: &Deadline) -> anyhow::Result<Self> {
        let directory = OwnedDirectory::capture(path)?;
        let handle = directory
            .open(false)?
            .context("missing new FVP allocation")?;
        let scaffold = snapshot_tree(&handle.file, deadline, false)?;
        anyhow::ensure!(
            scaffold.regular_files == 0,
            "new FVP allocation is not empty"
        );
        Ok(Self {
            directory,
            scaffold,
        })
    }

    fn verify_empty(&self, deadline: &Deadline) -> anyhow::Result<DirectoryHandle> {
        let handle = self
            .directory
            .open(false)?
            .context("FVP allocation disappeared")?;
        let current = snapshot_tree(&handle.file, deadline, false)?;
        anyhow::ensure!(
            current.regular_files == 0
                && current.digest == self.scaffold.digest
                && current.entries == self.scaffold.entries,
            "FVP allocation contents changed; preserving it"
        );
        Ok(handle)
    }

    pub(super) fn verify(&self, deadline: &Deadline) -> anyhow::Result<()> {
        self.verify_empty(deadline).map(|_| ())
    }

    pub(super) fn discard(&self, deadline: &Deadline) -> anyhow::Result<()> {
        let handle = self.verify_empty(deadline)?;
        remove_tree_entries(&self.directory, &handle, &self.scaffold, None, deadline)?;
        self.directory
            .open(false)?
            .context("FVP allocation changed before removal")?;
        // No recursive deletion: new or unexpected contents stop retirement.
        std::fs::remove_dir(fd_path(&handle.parent).join(&handle.name))?;
        handle.parent.sync_all()?;
        Ok(())
    }
}

fn snapshot_tree(root: &File, deadline: &Deadline, sync: bool) -> anyhow::Result<TreeSnapshot> {
    struct Scan<'a> {
        deadline: &'a Deadline,
        mount: u64,
        sync: bool,
        entries: Vec<TreeEntry>,
        hasher: sha2::Sha256,
        bytes: u64,
        regular_files: usize,
    }
    impl Scan<'_> {
        fn walk(
            &mut self,
            directory: &File,
            relative: &Path,
            parents: &mut Vec<(OsString, DirectoryStamp)>,
        ) -> anyhow::Result<()> {
            anyhow::ensure!(parents.len() < 32, "FVP directory tree is too deep");
            let mut names = Vec::new();
            for entry in std::fs::read_dir(fd_path(directory))? {
                self.deadline.remaining()?;
                anyhow::ensure!(
                    names.len() + self.entries.len() < TREE_ENTRY_LIMIT,
                    "FVP directory tree exceeds the entry limit"
                );
                names.push(entry?.file_name());
            }
            names.sort();
            for name in names {
                self.deadline.remaining()?;
                let identity = OpenOptions::new()
                    .read(true)
                    .custom_flags(nix::libc::O_PATH | nix::libc::O_NOFOLLOW)
                    .open(fd_path(directory).join(&name))
                    .context("FVP tree contains an inaccessible entry or symlink; preserving it")?;
                let metadata = identity.metadata()?;
                anyhow::ensure!(
                    mount_id(&identity)? == self.mount,
                    "FVP tree contains a nested mount; preserving it"
                );
                anyhow::ensure!(
                    metadata.is_dir() || (metadata.is_file() && metadata.nlink() == 1),
                    "FVP tree contains a special file or hard link; preserving it"
                );
                let file = File::open(fd_path(&identity))?;
                let child_relative = relative.join(&name);
                let path_bytes = child_relative.as_os_str().as_bytes();
                self.hasher.update((path_bytes.len() as u64).to_le_bytes());
                self.hasher.update(path_bytes);
                self.hasher.update(metadata.mode().to_le_bytes());
                if metadata.is_dir() {
                    parents.push((name.clone(), DirectoryStamp::read(&file)?));
                    self.walk(&file, &child_relative, parents)?;
                    parents.pop();
                } else {
                    self.regular_files += 1;
                    self.bytes = self
                        .bytes
                        .checked_add(metadata.len())
                        .context("FVP tree size overflow")?;
                    anyhow::ensure!(self.bytes <= TREE_BYTE_LIMIT, "FVP tree exceeds one GiB");
                    self.hasher.update(metadata.len().to_le_bytes());
                    let mut reader = &file;
                    let mut count = 0u64;
                    let mut buffer = [0; 65536];
                    loop {
                        self.deadline.remaining()?;
                        let read = reader.read(&mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        count += read as u64;
                        anyhow::ensure!(
                            count <= metadata.len(),
                            "FVP file grew during verification"
                        );
                        self.hasher.update(&buffer[..read]);
                    }
                    anyhow::ensure!(
                        count == metadata.len(),
                        "FVP file changed during verification"
                    );
                }
                if self.sync {
                    file.sync_all()?;
                }
                let current = std::fs::symlink_metadata(fd_path(directory).join(&name))?;
                anyhow::ensure!(
                    current.dev() == metadata.dev() && current.ino() == metadata.ino(),
                    "FVP tree entry was replaced during verification"
                );
                anyhow::ensure!(
                    self.entries.len() < TREE_ENTRY_LIMIT,
                    "FVP tree exceeds the entry limit"
                );
                self.entries.push(TreeEntry {
                    relative: child_relative,
                    parents: parents.clone(),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    directory: metadata.is_dir(),
                    length: metadata.len(),
                    modified_seconds: metadata.mtime(),
                    modified_nanoseconds: metadata.mtime_nsec(),
                });
            }
            Ok(())
        }
    }
    let mut scan = Scan {
        deadline,
        mount: mount_id(root)?,
        sync,
        entries: Vec::new(),
        hasher: sha2::Sha256::new(),
        bytes: 0,
        regular_files: 0,
    };
    scan.walk(root, Path::new(""), &mut Vec::new())?;
    if sync {
        root.sync_all()?;
    }
    deadline.remaining()?;
    Ok(TreeSnapshot {
        entries: scan.entries,
        digest: hex::encode(scan.hasher.finalize()),
        regular_files: scan.regular_files,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreservedOutputs {
    directory: OwnedDirectory,
    digest: String,
    workspace_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceRecord {
    directory: OwnedDirectory,
    run_id: RunId,
    destination: Option<OwnedDirectory>,
    outputs: Option<PreservedOutputs>,
    removal_started: bool,
}

impl WorkspaceRecord {
    fn validate(&self, run_id: &RunId) -> anyhow::Result<()> {
        self.directory.validate()?;
        validate_workspace_name(&self.directory.path)?;
        anyhow::ensure!(
            self.run_id == *run_id,
            "FVP workspace run identity mismatch"
        );
        if let Some(destination) = &self.destination {
            destination.validate()?;
            anyhow::ensure!(
                !destination.path.starts_with(&self.directory.path)
                    && !self.directory.path.starts_with(&destination.path),
                "FVP output destination overlaps its workspace"
            );
        }
        if let Some(outputs) = &self.outputs {
            outputs.directory.validate()?;
            anyhow::ensure!(
                is_hex_id(&outputs.digest)
                    && is_hex_id(&outputs.workspace_digest)
                    && !outputs.directory.path.starts_with(&self.directory.path)
                    && !self.directory.path.starts_with(&outputs.directory.path),
                "invalid FVP output preservation record"
            );
            anyhow::ensure!(
                self.destination
                    .as_ref()
                    .is_none_or(|destination| *destination == outputs.directory),
                "preserved FVP outputs do not match the registered destination"
            );
        }
        anyhow::ensure!(
            !self.removal_started || self.outputs.is_some(),
            "FVP workspace removal has no output preservation proof"
        );
        Ok(())
    }

    fn seal_outputs(
        &self,
        deadline: &Deadline,
        copy_outputs: bool,
    ) -> anyhow::Result<PreservedOutputs> {
        let source = self
            .directory
            .open(false)?
            .context("missing FVP workspace")?;
        verify_directory_marker(
            &source,
            WORKSPACE_MARKER,
            &directory_marker(&self.directory, &self.run_id, "openvmm-fvp-workspace"),
        )?;
        let directory = self.destination.as_ref().context(
            "FVP workspace outputs were not durably preserved or registered; retaining workspace",
        )?;
        let output = directory
            .open(false)?
            .context("missing FVP output destination")?;
        ensure_persistent_output(directory, &output)?;
        verify_directory_marker(
            &output,
            OUTPUT_MARKER,
            &directory_marker(directory, &self.run_id, "openvmm-fvp-outputs"),
        )?;
        if copy_outputs {
            super::staging::persist_outputs(
                &fd_path(&source.file),
                &fd_path(&output.file),
                deadline,
            )?;
        }
        let source_snapshot = snapshot_tree(&source.file, deadline, false)?;
        let before = snapshot_tree(&output.file, deadline, false)?;
        anyhow::ensure!(
            before.regular_files > 1 || source_snapshot.regular_files == 1,
            "FVP output destination is empty while workspace data remains"
        );
        let output_snapshot = snapshot_tree(&output.file, deadline, true)?;
        anyhow::ensure!(
            snapshot_tree(&source.file, deadline, false)?.digest == source_snapshot.digest,
            "FVP workspace changed while sealing preserved outputs"
        );
        Ok(PreservedOutputs {
            directory: directory.clone(),
            digest: output_snapshot.digest,
            workspace_digest: source_snapshot.digest,
        })
    }

    fn verify_outputs(&self, deadline: &Deadline) -> anyhow::Result<()> {
        let outputs = self.outputs.as_ref().with_context(|| {
            format!(
                "FVP workspace outputs were not durably preserved; retaining {} for manual recovery",
                self.directory.path.display()
            )
        })?;
        let handle = outputs
            .directory
            .open(false)?
            .context("missing FVP output directory")?;
        ensure_persistent_output(&outputs.directory, &handle)?;
        verify_directory_marker(
            &handle,
            OUTPUT_MARKER,
            &directory_marker(&outputs.directory, &self.run_id, "openvmm-fvp-outputs"),
        )?;
        anyhow::ensure!(
            snapshot_tree(&handle.file, deadline, false)?.digest == outputs.digest,
            "preserved FVP outputs changed; retaining workspace"
        );
        Ok(())
    }

    fn preflight(&self, deadline: &Deadline) -> anyhow::Result<()> {
        self.verify_outputs(deadline)?;
        let Some(handle) = self.directory.open(self.removal_started)? else {
            return Ok(());
        };
        verify_directory_marker(
            &handle,
            WORKSPACE_MARKER,
            &directory_marker(&self.directory, &self.run_id, "openvmm-fvp-workspace"),
        )?;
        anyhow::ensure!(
            Some(
                snapshot_tree(&handle.file, deadline, false)?
                    .digest
                    .as_str()
            ) == self
                .outputs
                .as_ref()
                .map(|outputs| outputs.workspace_digest.as_str()),
            "FVP workspace changed or removal was interrupted; retaining it"
        );
        Ok(())
    }

    fn remove(&self, deadline: &Deadline) -> anyhow::Result<()> {
        self.verify_outputs(deadline)?;
        let Some(handle) = self.directory.open(self.removal_started)? else {
            return Ok(());
        };
        verify_directory_marker(
            &handle,
            WORKSPACE_MARKER,
            &directory_marker(&self.directory, &self.run_id, "openvmm-fvp-workspace"),
        )?;
        let snapshot = snapshot_tree(&handle.file, deadline, false)?;
        anyhow::ensure!(
            Some(snapshot.digest.as_str())
                == self
                    .outputs
                    .as_ref()
                    .map(|outputs| outputs.workspace_digest.as_str()),
            "FVP workspace changed or removal was interrupted; retaining it"
        );
        remove_tree_entries(
            &self.directory,
            &handle,
            &snapshot,
            Some(Path::new(WORKSPACE_MARKER)),
            deadline,
        )?;
        deadline.remaining()?;
        self.directory
            .open(false)?
            .context("FVP workspace disappeared before removal")?;
        verify_directory_marker(
            &handle,
            WORKSPACE_MARKER,
            &directory_marker(&self.directory, &self.run_id, "openvmm-fvp-workspace"),
        )?;
        std::fs::remove_file(fd_path(&handle.file).join(WORKSPACE_MARKER))?;
        handle.file.sync_all()?;
        std::fs::remove_dir(fd_path(&handle.parent).join(&handle.name))?;
        handle.parent.sync_all()?;
        Ok(())
    }
}

fn remove_tree_entries(
    directory: &OwnedDirectory,
    handle: &DirectoryHandle,
    snapshot: &TreeSnapshot,
    skip: Option<&Path>,
    deadline: &Deadline,
) -> anyhow::Result<()> {
    let root_mount = mount_id(&handle.file)?;
    for entry in snapshot
        .entries
        .iter()
        .filter(|entry| Some(entry.relative.as_path()) != skip)
    {
        deadline.remaining()?;
        directory
            .open(false)?
            .context("FVP workspace disappeared during removal")?;
        let mut parent = handle.file.try_clone()?;
        for (name, expected) in &entry.parents {
            parent = open_directory(&fd_path(&parent).join(name))?;
            anyhow::ensure!(
                DirectoryStamp::read(&parent)? == *expected && mount_id(&parent)? == root_mount,
                "FVP workspace descendant changed; preserving remaining data"
            );
        }
        let name = entry
            .relative
            .file_name()
            .context("invalid FVP tree entry")?;
        let path = fd_path(&parent).join(name);
        let metadata = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            metadata.dev() == entry.device
                && metadata.ino() == entry.inode
                && (entry.directory
                    || (metadata.len() == entry.length
                        && metadata.mtime() == entry.modified_seconds
                        && metadata.mtime_nsec() == entry.modified_nanoseconds)),
            "FVP workspace entry changed; preserving remaining data"
        );
        if entry.directory {
            std::fs::remove_dir(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    deadline.remaining()?;
    Ok(())
}

/// One atomic, versioned record. Its fields are not cleanup authority by
/// themselves: recovery also checks the trusted tuple and live identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunState {
    schema_version: u32,
    kind: String,
    run_id: RunId,
    owner: ProcessIdentity,
    docker: DockerBinding,
    expected_image: String,
    expected_model: PathBuf,
    model: Option<ProcessIdentity>,
    launch_pending: bool,
    callback_phase: CallbackPhase,
    #[serde(default)]
    resources_stopped: bool,
    #[serde(default)]
    post_verification: PostVerification,
    workspace: Option<WorkspaceRecord>,
}

impl RunState {
    fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        anyhow::ensure!(
            value.get("schema_version").and_then(|v| v.as_u64()) == Some(u64::from(SCHEMA_VERSION)),
            "unsupported FVP state schema version"
        );
        anyhow::ensure!(
            value.get("workspace").is_some(),
            "FVP state has no workspace ownership declaration; preserving older state"
        );
        // Parse the original bytes again so duplicate fields remain errors.
        let state: Self = serde_json::from_slice(bytes)?;
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema_version == SCHEMA_VERSION && self.kind == STATE_KIND,
            "foreign or unsupported FVP state"
        );
        self.run_id.validate()?;
        self.docker.validate()?;
        if let Some(workspace) = &self.workspace {
            workspace.validate(&self.run_id)?;
            anyhow::ensure!(
                self.post_verification != PostVerification::NotRequired,
                "registered FVP workspace has no post-verification requirement"
            );
        }
        validate_image(&self.expected_image)?;
        anyhow::ensure!(
            self.owner.pid > 1
                && self.owner.pid <= i32::MAX as u32
                && self.owner.start_ticks > 0
                && valid_boot_id(&self.owner.boot_id)
                && self.owner.executable.is_absolute()
                && self.expected_model.is_absolute(),
            "invalid FVP process ownership record"
        );
        if let Some(model) = &self.model {
            anyhow::ensure!(
                !self.launch_pending
                    && model.pid > 1
                    && model.pid <= i32::MAX as u32
                    && model.start_ticks > 0
                    && model.boot_id == self.owner.boot_id
                    && model.executable == self.expected_model
                    && model.run_id.as_ref() == Some(&self.run_id),
                "invalid FVP model ownership record"
            );
        }
        Ok(())
    }

    /// The identity that must label containers and endpoint records.
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Registered writable workspace, if this run has one.
    pub fn workspace_path(&self) -> Option<&Path> {
        self.workspace
            .as_ref()
            .map(|workspace| workspace.directory.path.as_path())
    }

    /// Persistent output root registered before workspace use.
    pub fn output_path(&self) -> Option<&Path> {
        self.workspace
            .as_ref()
            .and_then(|workspace| workspace.destination.as_ref())
            .map(|destination| destination.path.as_path())
    }

    /// Pin every Docker command issued by preparation to this recorded daemon.
    pub fn docker_binding(&self) -> &DockerBinding {
        &self.docker
    }

    /// These labels must be applied at container creation, not afterward.
    pub fn container_labels(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (RUN_LABEL.to_owned(), self.run_id.as_str().to_owned()),
            (SCHEMA_LABEL.to_owned(), SCHEMA_VERSION.to_string()),
            (IMAGE_LABEL.to_owned(), self.expected_image.clone()),
            (ROLE_LABEL.to_owned(), MODEL_ROLE.to_owned()),
        ])
    }
}

fn validate_image(image: &str) -> anyhow::Result<()> {
    let (repository, digest) = image
        .split_once("@sha256:")
        .context("FVP image must use an immutable sha256 digest")?;
    anyhow::ensure!(
        !repository.is_empty()
            && !repository.starts_with('-')
            && repository
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"./:_-".contains(&b))
            && is_hex_id(digest),
        "invalid FVP image digest"
    );
    Ok(())
}

/// Recorded daemon selection. This is data, not proof of a current connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerBinding {
    endpoint: String,
    daemon_id: String,
}

impl DockerBinding {
    fn validate(&self) -> anyhow::Result<()> {
        Self::validate_endpoint(&self.endpoint)?;
        anyhow::ensure!(
            !self.daemon_id.is_empty()
                && self.daemon_id.len() <= 128
                && self
                    .daemon_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-:".contains(&byte)),
            "invalid Docker daemon identity"
        );
        Ok(())
    }

    fn validate_endpoint(endpoint: &str) -> anyhow::Result<&Path> {
        let socket = endpoint
            .strip_prefix("unix://")
            .context("FVP requires an explicit local unix:// Docker endpoint")?;
        let path = Path::new(socket);
        anyhow::ensure!(
            path.is_absolute()
                && socket.len() <= 4096
                && socket
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
                && !path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir)),
            "invalid Docker Unix socket endpoint"
        );
        Ok(path)
    }

    /// Explicit endpoint selected at connection time, not the current context.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Identity returned by the connected daemon's `info` response.
    pub fn daemon_id(&self) -> &str {
        &self.daemon_id
    }

    /// Export this selection to a launcher that invokes Docker internally.
    ///
    /// The launcher must not override it with Docker `--host`/`--context` flags
    /// or another environment. Ambient contexts and TLS settings are not used.
    pub fn apply_to_command(&self, command: &mut Command) {
        command.env("DOCKER_HOST", &self.endpoint);
        for variable in [
            "DOCKER_CONTEXT",
            "DOCKER_TLS",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
            "DOCKER_API_VERSION",
        ] {
            command.env_remove(variable);
        }
    }
}

/// A Docker executable bound to one explicit endpoint and verified daemon ID.
pub struct Docker {
    executable: PathBuf,
    binding: DockerBinding,
    #[cfg(test)]
    responder: Option<TestDockerResponder>,
}

#[cfg(test)]
type TestDockerResponder = Box<dyn Fn(&Command, &[&str]) -> anyhow::Result<Vec<u8>>>;

impl Docker {
    /// Resolve an explicit local Unix socket and verify its daemon identity.
    ///
    /// PR2 deliberately does not infer selection from `DOCKER_HOST`, the current
    /// Docker context, or TLS/SSH configuration. The caller selects the endpoint
    /// once; every later command and recovery must match that recorded binding.
    pub fn connect(
        executable: PathBuf,
        endpoint: &str,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Self> {
        cancellation.check()?;
        deadline.remaining()?;
        Self::at_endpoint(executable, endpoint)?.finish_connect(deadline, cancellation)
    }

    fn at_endpoint(executable: PathBuf, endpoint: &str) -> anyhow::Result<Self> {
        let socket = canonical(DockerBinding::validate_endpoint(endpoint)?)?;
        anyhow::ensure!(
            socket.metadata()?.file_type().is_socket(),
            "Docker endpoint is not a Unix socket"
        );
        let endpoint = format!(
            "unix://{}",
            socket.to_str().context("Docker socket path is not UTF-8")?
        );
        DockerBinding::validate_endpoint(&endpoint)?;
        Ok(Self {
            executable: canonical(&executable)?,
            binding: DockerBinding {
                endpoint,
                daemon_id: String::new(),
            },
            #[cfg(test)]
            responder: None,
        })
    }

    fn finish_connect(
        mut self,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<Self> {
        self.binding.daemon_id = self.read_daemon_id(deadline, Some(cancellation))?;
        self.binding.validate()?;
        Ok(self)
    }

    #[cfg(test)]
    fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            binding: tests::docker_binding(),
            responder: None,
        }
    }

    /// Verified selection to persist with the run and export to nested launchers.
    pub fn binding(&self) -> &DockerBinding {
        &self.binding
    }

    /// Build a Docker command with explicit host selection. Execute it only
    /// through a bounded process helper; do not add alternate selection flags.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        self.binding.apply_to_command(&mut command);
        command.arg("--host").arg(&self.binding.endpoint);
        command
    }

    fn invoke(&self, args: &[&str], deadline: &Deadline) -> anyhow::Result<Vec<u8>> {
        self.invoke_inner(args, deadline, None)
    }

    fn invoke_inner(
        &self,
        args: &[&str],
        deadline: &Deadline,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<Vec<u8>> {
        deadline.remaining()?;
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        let mut command = self.command();
        command.args(args);
        #[cfg(test)]
        if let Some(responder) = &self.responder {
            let output = responder(&command, args)?;
            deadline.remaining()?;
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            return Ok(output);
        }
        let output = match cancellation {
            Some(cancellation) => run_command_cancellable(&mut command, deadline, cancellation),
            None => run_command_with_cleanup(&mut command, deadline, deadline),
        }
        .context("FVP Docker command could not complete")?;
        anyhow::ensure!(
            output.status.success(),
            "FVP Docker command {:?} failed ({})",
            args.first(),
            output.status,
        );
        deadline.remaining()?;
        Ok(output.stdout)
    }

    fn read_daemon_id(
        &self,
        deadline: &Deadline,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<String> {
        let output = self.invoke_inner(
            &["info", "--format", "{{json .ID}}"],
            deadline,
            cancellation,
        )?;
        serde_json::from_slice(&output).context("Docker returned an invalid daemon identity")
    }

    fn verify_binding(
        &self,
        state: &RunState,
        deadline: &Deadline,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.binding == state.docker,
            "Docker daemon selection differs from the recorded FVP run; preserving state"
        );
        self.verify_identity_inner(deadline, cancellation)
    }

    /// Recheck the selected daemon around platform inventory and model launch.
    pub fn verify_identity(
        &self,
        deadline: &Deadline,
        cancellation: &Cancellation,
    ) -> anyhow::Result<()> {
        self.verify_identity_inner(deadline, Some(cancellation))
    }

    fn verify_identity_inner(
        &self,
        deadline: &Deadline,
        cancellation: Option<&Cancellation>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.read_daemon_id(deadline, cancellation)? == self.binding.daemon_id,
            "Docker daemon identity changed at {}; preserving FVP state",
            self.binding.endpoint
        );
        Ok(())
    }

    fn containers(&self, state: &RunState, deadline: &Deadline) -> anyhow::Result<Vec<String>> {
        let filter = format!("label={RUN_LABEL}={}", state.run_id.as_str());
        let output = self.invoke(
            &["ps", "--all", "--quiet", "--no-trunc", "--filter", &filter],
            deadline,
        )?;
        let mut ids = Vec::new();
        for id in std::str::from_utf8(&output)?.lines() {
            anyhow::ensure!(is_hex_id(id), "Docker returned an invalid container ID");
            anyhow::ensure!(ids.len() < 16, "too many containers for one FVP run");
            anyhow::ensure!(!ids.iter().any(|old| old == id), "duplicate Docker ID");
            ids.push(id.to_owned());
        }
        Ok(ids)
    }

    fn cleanup(&self, state: &RunState, deadline: &Deadline) -> anyhow::Result<Vec<String>> {
        self.verify_binding(state, deadline, None)?;
        let containers = self.containers(state, deadline)?;
        // Validate the entire inventory first. A foreign/newer record prevents
        // any removal, even when an earlier entry happens to be ours.
        for id in &containers {
            if let Some(output) = self.invoke_or_confirm_removed(
                &["inspect", "--type", "container", "--", id],
                id,
                state,
                deadline,
            )? {
                validate_container(&output, id, state)?;
            }
        }
        for id in &containers {
            self.invoke_or_confirm_removed(&["rm", "--force", "--", id], id, state, deadline)?;
        }
        anyhow::ensure!(
            self.containers(state, deadline)?.is_empty(),
            "owned FVP containers survived cleanup; preserving state"
        );
        self.verify_binding(state, deadline, None)?;
        Ok(containers)
    }

    fn invoke_or_confirm_removed(
        &self,
        args: &[&str],
        id: &str,
        state: &RunState,
        deadline: &Deadline,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        match self.invoke(args, deadline) {
            Ok(output) => Ok(Some(output)),
            Err(error) => {
                if error.is::<super::process::UnresolvedProcess>()
                    || error.is::<super::process::InterruptedCommand>()
                {
                    return Err(error);
                }
                // Shrinkwrap can remove its --rm container after our inventory.
                // Query the full immutable ID, without relying on ownership labels.
                self.verify_binding(state, deadline, None)?;
                let filter = format!("id={id}");
                let output = self.invoke(
                    &["ps", "--all", "--quiet", "--no-trunc", "--filter", &filter],
                    deadline,
                )?;
                self.verify_binding(state, deadline, None)?;
                if output.is_empty() {
                    Ok(None)
                } else {
                    Err(error.context("FVP container absence could not be confirmed"))
                }
            }
        }
    }
}

fn validate_container(bytes: &[u8], expected_id: &str, state: &RunState) -> anyhow::Result<()> {
    let records: Vec<serde_json::Value> = serde_json::from_slice(bytes)?;
    anyhow::ensure!(records.len() == 1, "ambiguous Docker container inventory");
    let record = records
        .first()
        .context("missing Docker container inventory")?;
    anyhow::ensure!(
        record.get("Id").and_then(|v| v.as_str()) == Some(expected_id),
        "Docker container ID mismatch; preserving container"
    );
    let config = record
        .get("Config")
        .context("missing Docker container config")?;
    anyhow::ensure!(
        config.get("Image").and_then(|v| v.as_str()) == Some(state.expected_image.as_str()),
        "foreign Docker image; preserving container"
    );
    let labels = config
        .get("Labels")
        .context("missing Docker ownership labels")?;
    for (name, expected) in state.container_labels() {
        anyhow::ensure!(
            labels.get(&name).and_then(|v| v.as_str()) == Some(expected.as_str()),
            "foreign or newer Docker container label {name}; preserving container"
        );
    }
    Ok(())
}

/// A run-bound, loopback-only pipette endpoint.
///
/// Deserialization must use [`Endpoint::parse`] with the expected run and port.
///
/// ```
/// use incubator::fvp::lifecycle::{Endpoint, RunId};
///
/// let run = RunId::new()?;
/// let endpoint = Endpoint::new(run.clone(), 12345)?;
/// let json = serde_json::to_vec(&endpoint)?;
/// let checked = Endpoint::parse(&json, &run, 12345)?;
/// assert_eq!(checked.address(), "127.0.0.1:12345".parse()?);
/// # Ok::<(), anyhow::Error>(())
/// ```
///
/// ```compile_fail
/// let _: incubator::fvp::lifecycle::Endpoint = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Serialize)]
pub struct Endpoint {
    schema_version: u32,
    run_id: RunId,
    address: SocketAddr,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointWire {
    schema_version: u32,
    run_id: RunId,
    address: SocketAddr,
}

impl Endpoint {
    /// Create a record only for the port selected by the owned launch attempt.
    pub fn new(run_id: RunId, port: u16) -> anyhow::Result<Self> {
        anyhow::ensure!(port != 0, "FVP endpoint cannot use port zero");
        run_id.validate()?;
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            run_id,
            address: (Ipv4Addr::LOCALHOST, port).into(),
        })
    }

    /// Reject stale IDs, changed ports, non-loopback addresses, and new schemas.
    pub fn parse(bytes: &[u8], run_id: &RunId, expected_port: u16) -> anyhow::Result<Self> {
        anyhow::ensure!(bytes.len() <= 4096, "FVP endpoint record is too large");
        let endpoint: EndpointWire =
            serde_json::from_slice(bytes).context("invalid FVP endpoint JSON")?;
        anyhow::ensure!(
            endpoint.schema_version == SCHEMA_VERSION,
            "unsupported FVP endpoint schema"
        );
        endpoint.run_id.validate()?;
        anyhow::ensure!(endpoint.run_id == *run_id, "FVP endpoint run ID mismatch");
        anyhow::ensure!(
            endpoint.address.ip() == std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
                && endpoint.address.port() != 0
                && endpoint.address.port() == expected_port,
            "FVP endpoint is not the allocated loopback port"
        );
        Ok(Self {
            schema_version: endpoint.schema_version,
            run_id: endpoint.run_id,
            address: endpoint.address,
        })
    }

    /// Validated endpoint address.
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

/// An allocation and retry budget, started before the first bind.
pub struct PortBudget {
    deadline: Deadline,
    maximum_attempts: u32,
    attempts: u32,
}

impl PortBudget {
    /// Keep this same value across allocation errors and failed launch commands.
    pub fn new(timeout: Duration, maximum_attempts: u32) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (Duration::from_secs(10)..=Duration::from_secs(5 * 60)).contains(&timeout),
            "FVP port budget must be between ten seconds and five minutes"
        );
        anyhow::ensure!(
            (1..=50).contains(&maximum_attempts),
            "FVP port attempts must be between one and fifty"
        );
        Ok(Self {
            deadline: Deadline::new(timeout)?,
            maximum_attempts,
            attempts: 0,
        })
    }

    /// Count attempts before binding. A failed bind also consumes an attempt.
    pub fn allocate(&mut self, cancellation: &Cancellation) -> anyhow::Result<PortReservation> {
        cancellation.check()?;
        self.deadline
            .remaining()
            .context("FVP port budget expired")?;
        anyhow::ensure!(
            self.attempts < self.maximum_attempts,
            "FVP port retry limit exhausted"
        );
        self.attempts += 1;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("failed to allocate FVP loopback port")?;
        let port = listener.local_addr()?.port();
        Ok(PortReservation {
            listener,
            port,
            deadline: self.deadline,
        })
    }

    /// Number consumed, including failed commands.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

/// Bind-then-release is racy; the launch confirmation must detect collisions.
pub struct PortReservation {
    listener: TcpListener,
    port: u16,
    deadline: Deadline,
}

impl PortReservation {
    /// Port to pass to the command builder.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Call immediately before spawn, after preparing the command. The returned
    /// model-start deadline cannot extend the original port budget.
    pub fn release(self, model_start: Duration) -> anyhow::Result<Deadline> {
        let deadline = self.deadline.limited_to(model_start)?;
        deadline.remaining()?;
        drop(self.listener);
        Ok(deadline)
    }
}

/// Monotonic lifecycle phases. Re-entering a phase cannot reset its deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Starting the prepared Shrinkwrap command.
    ModelStart,
    /// The model runs; wait for a run-bound pipette connection.
    PipetteReadiness,
    /// A test or listing request has been dispatched.
    TestExecution,
    /// Guest shutdown was requested or observed.
    GuestShutdown,
    /// Graceful shutdown failed or cancellation was requested.
    ForcedCleanup,
}

/// Explicit phase ordering, separate from configurable profile field names.
pub struct PhaseClock {
    phase: Phase,
    deadline: Deadline,
}

impl PhaseClock {
    /// Start the model phase immediately before spawning the command.
    pub fn model_start(deadline: Deadline) -> Self {
        Self {
            phase: Phase::ModelStart,
            deadline,
        }
    }

    /// Change phase once; normal transitions cannot turn a timeout into success.
    pub fn advance(&mut self, phase: Phase, timeout: Duration) -> anyhow::Result<&Deadline> {
        let valid = matches!(
            (self.phase, phase),
            (Phase::ModelStart, Phase::PipetteReadiness)
                | (Phase::PipetteReadiness, Phase::TestExecution)
                | (Phase::PipetteReadiness, Phase::GuestShutdown)
                | (Phase::TestExecution, Phase::GuestShutdown)
        ) || (phase == Phase::ForcedCleanup && self.phase != Phase::ForcedCleanup);
        anyhow::ensure!(valid, "invalid or repeated FVP phase transition");
        if phase != Phase::ForcedCleanup {
            self.deadline.remaining()?;
        }
        self.deadline = Deadline::new(timeout)?;
        self.phase = phase;
        Ok(&self.deadline)
    }

    /// The original, non-resetting phase deadline.
    pub fn deadline(&self) -> &Deadline {
        &self.deadline
    }
}

/// Model confirmation distinguishes a retryable port collision from other
/// launch failures. A running wrapper alone is not model confirmation.
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchConfirmation {
    /// The caller confirmed that the model itself runs.
    Running,
    /// A specific bind collision diagnostic was observed.
    PortCollision,
}

/// Owned resources. Explicit cleanup reports errors; drop is the cancellation
/// and unwinding fallback and reports failures through tracing.
///
/// The session owns a signal guard for its cancellation token until its final
/// field is dropped. External guards may safely have shorter lifetimes.
///
/// Every callback must supervise its external work within the supplied deadline
/// and reap its helper processes before returning `Ok`. Helpers must not detach
/// or outlive the callback scope. Session-owned model resources are separate.
/// Callback errors and panics leave durable in-flight intent. A callback that
/// returns `Ok` proves its helpers are gone, so its intent is cleared before
/// reporting subsequent cancellation or timeout. Cleanup still stops proven
/// model resources but preserves genuinely ambiguous state.
pub struct Session {
    lock: RuntimeLock,
    docker: Docker,
    state: RunState,
    child: Option<ManagedChild>,
    log: Option<File>,
    cancellation: Cancellation,
    forced_cleanup: Duration,
    cleanup_deadline: Option<Deadline>,
    phases: Option<PhaseClock>,
    model_confirmed: bool,
    ready: bool,
    cleaned: bool,
    defer_retirement: bool,
    retirement_deadline: Option<Deadline>,
    input_writer: Option<std::os::unix::net::UnixStream>,
    // Keep signal ownership alive through cleanup and every other field's drop.
    _signal_guard: SignalGuard,
}

impl Session {
    /// Persist intent before launch. Recover any prior state explicitly first.
    ///
    /// `expected_model` is the host executable observed through `/proc/PID/exe`,
    /// such as Shrinkwrap's validated Python interpreter. It is not the licensed
    /// FVP executable inside the container.
    pub fn new(
        lock: RuntimeLock,
        docker: Docker,
        expected_image: String,
        expected_model: &Path,
        forced_cleanup: Duration,
        cancellation: Cancellation,
    ) -> anyhow::Result<Self> {
        cancellation.check()?;
        anyhow::ensure!(
            (Duration::from_secs(10)..=Duration::from_secs(120)).contains(&forced_cleanup),
            "FVP forced cleanup must be between ten seconds and two minutes"
        );
        let signal_guard = cancellation.install_signal_handlers()?;
        let state = RunState {
            schema_version: SCHEMA_VERSION,
            kind: STATE_KIND.to_owned(),
            run_id: RunId::new()?,
            owner: ProcessIdentity::capture(std::process::id(), &std::env::current_exe()?)?,
            docker: docker.binding.clone(),
            expected_image,
            expected_model: canonical(expected_model)?,
            model: None,
            launch_pending: false,
            callback_phase: CallbackPhase::Idle,
            resources_stopped: false,
            post_verification: PostVerification::NotRequired,
            workspace: None,
        };
        lock.write_state(None, &state)?;
        Ok(Self {
            lock,
            docker,
            state,
            child: None,
            log: None,
            cancellation,
            forced_cleanup,
            cleanup_deadline: None,
            phases: None,
            model_confirmed: false,
            ready: false,
            cleaned: false,
            defer_retirement: false,
            retirement_deadline: None,
            input_writer: None,
            _signal_guard: signal_guard,
        })
    }

    /// Container labels and endpoint identity for the later runtime adapter.
    pub fn state(&self) -> &RunState {
        &self.state
    }

    /// Build a command using the session's pinned Docker executable and daemon.
    /// Execute it inside a supervised callback and finish its helpers in scope.
    pub fn docker_command(&self) -> Command {
        self.docker.command()
    }

    /// Register a newly allocated private workspace before copying inputs or
    /// launching anything that can use it. Only empty directory scaffolding is
    /// accepted; the allocation must have an `openvmm-fvp-` generated name.
    ///
    /// The journal and an exclusive owner marker bind the canonical path, every
    /// parent directory identity, and the workspace inode to this run. Registering
    /// a workspace requires explicit post-verification and retirement afterward.
    /// Callers must not remove or rename the registered directory themselves.
    pub fn register_workspace(&mut self, path: &Path, deadline: &Deadline) -> anyhow::Result<()> {
        self.register_workspace_inner(path, None, deadline)
    }

    /// Validate both roots and publish their ownership in one journal update.
    /// Cancellation cannot leave a durable workspace without its destination.
    pub fn register_workspace_with_output(
        &mut self,
        path: &Path,
        destination: &Path,
        deadline: &Deadline,
    ) -> anyhow::Result<()> {
        self.register_workspace_inner(path, Some(destination), deadline)
    }

    fn register_workspace_inner(
        &mut self,
        path: &Path,
        destination: Option<&Path>,
        deadline: &Deadline,
    ) -> anyhow::Result<()> {
        self.cancellation.check()?;
        deadline.remaining()?;
        anyhow::ensure!(
            self.lock.read_state()?.as_ref() == Some(&self.state),
            "FVP state changed before workspace registration"
        );
        anyhow::ensure!(
            !self.cleaned
                && !self.state.resources_stopped
                && self.child.is_none()
                && self.phases.is_none()
                && self.state.callback_phase == CallbackPhase::Idle
                && self.state.workspace.is_none(),
            "FVP workspace registration must precede input preparation and launch"
        );
        validate_workspace_name(path)?;
        let directory = OwnedDirectory::capture(path)?;
        anyhow::ensure!(
            !directory.path.starts_with(&self.lock.directory.path)
                && !self.lock.directory.path.starts_with(&directory.path),
            "FVP workspace overlaps persistent runtime storage"
        );
        let handle = directory
            .open(false)?
            .context("missing allocated FVP workspace")?;
        anyhow::ensure!(
            snapshot_tree(&handle.file, deadline, false)?.regular_files == 0,
            "FVP workspace must be registered before writing files"
        );
        let destination = destination
            .map(|path| self.capture_output_destination(&directory, path, deadline))
            .transpose()?;
        self.cancellation.check()?;
        deadline.remaining()?;
        write_directory_marker(
            &handle,
            WORKSPACE_MARKER,
            &directory_marker(&directory, &self.state.run_id, "openvmm-fvp-workspace"),
        )?;
        if let Some((directory, handle)) = &destination {
            write_directory_marker(
                handle,
                OUTPUT_MARKER,
                &directory_marker(directory, &self.state.run_id, "openvmm-fvp-outputs"),
            )?;
        }
        let mut state = self.state.clone();
        state.workspace = Some(WorkspaceRecord {
            directory,
            run_id: self.state.run_id.clone(),
            destination: destination.map(|(directory, _)| directory),
            outputs: None,
            removal_started: false,
        });
        state.post_verification = PostVerification::Pending;
        self.lock.write_state(Some(&self.state), &state)?;
        self.state = state;
        self.defer_retirement = true;
        self.cancellation.check()?;
        deadline.remaining()?;
        Ok(())
    }

    /// Register the private persistent output root before using the workspace.
    /// Runtime must first exclude the platform/package input roots. This method
    /// binds the root's inode, ancestry, and run-ID marker; it does not seal the
    /// contents, which can continue receiving snapshots and live diagnostics.
    pub fn register_output_destination(
        &mut self,
        path: &Path,
        deadline: &Deadline,
    ) -> anyhow::Result<()> {
        self.cancellation.check()?;
        deadline.remaining()?;
        anyhow::ensure!(
            !self.cleaned
                && !self.state.resources_stopped
                && self.child.is_none()
                && self.phases.is_none()
                && self.state.callback_phase == CallbackPhase::Idle,
            "FVP output destination must be registered before workspace use"
        );
        anyhow::ensure!(
            self.lock.read_state()?.as_ref() == Some(&self.state),
            "FVP state changed before output registration"
        );
        let workspace = self
            .state
            .workspace
            .as_ref()
            .context("register the FVP workspace first")?;
        anyhow::ensure!(
            workspace.destination.is_none() && workspace.outputs.is_none(),
            "FVP output destination is already registered"
        );
        let (destination, handle) =
            self.capture_output_destination(&workspace.directory, path, deadline)?;
        write_directory_marker(
            &handle,
            OUTPUT_MARKER,
            &directory_marker(&destination, &self.state.run_id, "openvmm-fvp-outputs"),
        )?;
        let mut state = self.state.clone();
        state
            .workspace
            .as_mut()
            .context("missing FVP workspace")?
            .destination = Some(destination);
        self.lock.write_state(Some(&self.state), &state)?;
        self.state = state;
        deadline.remaining()?;
        Ok(())
    }

    fn capture_output_destination(
        &self,
        workspace: &OwnedDirectory,
        path: &Path,
        deadline: &Deadline,
    ) -> anyhow::Result<(OwnedDirectory, DirectoryHandle)> {
        let destination = OwnedDirectory::capture(path)?;
        anyhow::ensure!(
            !destination.path.starts_with(&workspace.path)
                && !workspace.path.starts_with(&destination.path)
                && !self.lock.directory.path.starts_with(&destination.path),
            "FVP output destination overlaps workspace or runtime state"
        );
        let handle = destination
            .open(false)?
            .context("missing FVP output destination")?;
        ensure_persistent_output(&destination, &handle)?;
        snapshot_tree(&handle.file, deadline, false)?;
        match std::fs::symlink_metadata(fd_path(&handle.file).join(OUTPUT_MARKER)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect FVP output ownership marker"),
            Ok(_) => anyhow::bail!("FVP output ownership marker already exists"),
        }
        Ok((destination, handle))
    }

    /// Preserve outputs through verified directory handles, then seal their receipt.
    ///
    /// Both registered roots and their markers are checked before any copying.
    /// This also applies after cancellation or a completed failed verification.
    pub fn preserve_outputs(
        &mut self,
        destination: &Path,
        deadline: &Deadline,
    ) -> anyhow::Result<()> {
        self.seal_preserved_outputs(destination, deadline, true)
    }

    /// Seal a completed output copy after model cleanup, including on cancellation.
    ///
    /// The caller asserts that *all required logs and guest results* were copied
    /// successfully (for example, by staging's output-preservation helper).
    /// This method fsyncs the private persistent per-run destination and records content
    /// digests for both trees. Any later output or workspace mutation prevents
    /// automatic deletion. Recovery can create the receipt only when the output
    /// destination was registered before workspace use; otherwise it preserves
    /// the workspace for manual log recovery.
    ///
    /// Trees are limited to 4096 entries, 32 levels, and one GiB each. Symlinks,
    /// hard links, special files, and nested mounts require manual preservation.
    pub fn record_preserved_outputs(
        &mut self,
        destination: &Path,
        deadline: &Deadline,
    ) -> anyhow::Result<()> {
        self.seal_preserved_outputs(destination, deadline, false)
    }

    fn seal_preserved_outputs(
        &mut self,
        destination: &Path,
        deadline: &Deadline,
        copy_outputs: bool,
    ) -> anyhow::Result<()> {
        deadline.remaining()?;
        anyhow::ensure!(
            self.lock.read_state()?.as_ref() == Some(&self.state),
            "FVP state changed before preserving outputs"
        );
        anyhow::ensure!(
            !self.cleaned && self.state.resources_stopped,
            "FVP outputs can only be sealed after model cleanup"
        );
        let workspace = self
            .state
            .workspace
            .as_ref()
            .context("no registered FVP workspace")?;
        anyhow::ensure!(
            workspace.outputs.is_none() && !workspace.removal_started,
            "FVP output preservation was already recorded"
        );
        let directory = OwnedDirectory::capture(destination)?;
        anyhow::ensure!(
            workspace.destination.as_ref() == Some(&directory),
            "FVP output destination does not match its registered identity"
        );
        anyhow::ensure!(
            !directory.path.starts_with(&workspace.directory.path)
                && !workspace.directory.path.starts_with(&directory.path)
                && !self.lock.directory.path.starts_with(&directory.path),
            "FVP output destination overlaps workspace or runtime state"
        );
        let receipt = workspace.seal_outputs(deadline, copy_outputs)?;
        let mut state = self.state.clone();
        state
            .workspace
            .as_mut()
            .context("missing registered FVP workspace")?
            .outputs = Some(receipt);
        self.lock.write_state(Some(&self.state), &state)?;
        self.state = state;
        deadline.remaining()?;
        Ok(())
    }

    /// Prepare verified inputs before starting the port-allocation budget.
    /// This callback follows the same owned-helper lifetime contract as launch
    /// preparation and records durable intent before any external work.
    pub fn prepare_inputs<T>(
        &mut self,
        deadline: &Deadline,
        prepare: impl FnOnce(&Docker, &RunState) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        anyhow::ensure!(
            self.child.is_none() && self.phases.is_none(),
            "FVP input preparation must precede launch"
        );
        self.run_callback(CallbackPhase::Preparation, deadline, |session| {
            prepare(&session.docker, &session.state)
        })
    }

    /// Durable model/launcher output. Each retry appends to the same run log.
    pub fn log_path(&self) -> PathBuf {
        self.lock
            .directory
            .path
            .join(format!("model-{}.log", self.state.run_id.as_str()))
    }

    fn run_callback<T>(
        &mut self,
        phase: CallbackPhase,
        deadline: &Deadline,
        action: impl FnOnce(&mut Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        anyhow::ensure!(
            !self.cleaned
                && !self.state.resources_stopped
                && self.state.callback_phase == CallbackPhase::Idle,
            "FVP callback ownership is unresolved; preserving state"
        );
        self.cancellation.check()?;
        deadline.remaining()?;
        let mut state = self.state.clone();
        state.callback_phase = phase;
        self.lock.write_state(Some(&self.state), &state)?;
        self.state = state;
        self.cancellation.check()?;
        deadline.remaining()?;
        let result = action(self)?;
        anyhow::ensure!(
            self.state.owner.pid == std::process::id()
                && self.state.owner.observe()? == ProcessObservation::Matching,
            "FVP callback owner identity changed; preserving in-flight state"
        );
        let mut state = self.state.clone();
        state.callback_phase = CallbackPhase::Idle;
        self.lock.write_state(Some(&self.state), &state)?;
        self.state = state;
        self.cancellation.check()?;
        deadline.remaining()?;
        Ok(result)
    }

    /// Launch prepared commands without implementing Shrinkwrap staging here.
    ///
    /// `prepare` and `confirm` must bound their own external work by the supplied
    /// deadline. Every failed preparation, spawn, or collision consumes the same
    /// attempt/time budgets. Only an explicit collision is retried.
    /// Preparation and confirmation follow the callback ownership contract on
    /// [`Session`]. Durable in-flight intent protects both callback scopes.
    /// Apply `state.docker_binding()` to any preparation command that can invoke
    /// Docker. The returned launcher receives that same binding automatically.
    /// The command must not daemonize host children out of its process group.
    /// Containers must receive `state.container_labels()` at creation.
    pub fn launch_with_ports(
        &mut self,
        budget: &mut PortBudget,
        model_start: Duration,
        mut prepare: impl FnMut(u16, &RunState, &Deadline) -> anyhow::Result<Command>,
        mut confirm: impl FnMut(&mut ManagedChild, &Deadline) -> anyhow::Result<LaunchConfirmation>,
    ) -> anyhow::Result<u16> {
        self.launch_with_ports_scoped(
            budget,
            model_start,
            |port, state, deadline| prepare(port, state, deadline).map(Ok),
            |child, deadline| confirm(child, deadline).map(Ok),
        )
    }

    /// Launch with explicit helper-scope completion. Inner errors are safe to
    /// report after clearing callback intent; outer errors retain ambiguity.
    pub fn launch_with_ports_scoped(
        &mut self,
        budget: &mut PortBudget,
        model_start: Duration,
        mut prepare: impl FnMut(u16, &RunState, &Deadline) -> anyhow::Result<anyhow::Result<Command>>,
        mut confirm: impl FnMut(
            &mut ManagedChild,
            &Deadline,
        ) -> anyhow::Result<anyhow::Result<LaunchConfirmation>>,
    ) -> anyhow::Result<u16> {
        anyhow::ensure!(
            self.child.is_none() && self.phases.is_none() && !self.cleaned,
            "FVP session already launched or cleaned"
        );
        anyhow::ensure!(
            self.state.callback_phase == CallbackPhase::Idle,
            "FVP callback ownership is unresolved; preserving state"
        );
        loop {
            let reservation = budget.allocate(&self.cancellation)?;
            let port = reservation.port();
            let mut state = self.state.clone();
            state.launch_pending = true;
            self.lock.write_state(Some(&self.state), &state)?;
            self.state = state;
            self.docker
                .verify_binding(&self.state, &budget.deadline, Some(&self.cancellation))?;
            let mut command =
                self.run_callback(CallbackPhase::Preparation, &budget.deadline, |session| {
                    prepare(port, &session.state, &budget.deadline)
                })??;
            self.cancellation.check()?;
            self.docker
                .verify_binding(&self.state, &budget.deadline, Some(&self.cancellation))?;
            self.state.docker.apply_to_command(&mut command);
            let log = if let Some(log) = &self.log {
                log.try_clone()?
            } else {
                let log = OpenOptions::new()
                    .create_new(true)
                    .append(true)
                    .mode(0o600)
                    .custom_flags(nix::libc::O_NOFOLLOW)
                    .open(self.log_path())?;
                validate_private_file(&log)?;
                self.log = Some(log.try_clone()?);
                log
            };
            command
                .env("OPENVMM_FVP_RUN_ID", self.state.run_id.as_str())
                .stdout(log.try_clone()?)
                .stderr(log);
            let deadline = reservation.release(model_start)?;
            self.phases = Some(PhaseClock::model_start(deadline));
            let (input, writer) = std::os::unix::net::UnixStream::pair()
                .context("failed to create noninteractive FVP launcher input")?;
            self.input_writer = Some(writer);
            self.child = Some(ManagedChild::spawn_with_stdin(
                &mut command,
                std::process::Stdio::from(std::os::fd::OwnedFd::from(input)),
            )?);
            let child = self.child.as_mut().context("missing owned FVP child")?;
            let identity = capture_owned_process(
                child,
                &self.state.expected_model,
                &self.state.run_id,
                &deadline,
                &self.cancellation,
            )?;
            let mut state = self.state.clone();
            state.model = Some(identity);
            state.launch_pending = false;
            self.lock.write_state(Some(&self.state), &state)?;
            self.state = state;
            let confirmation =
                self.run_callback(CallbackPhase::Confirmation, &deadline, |session| {
                    confirm(
                        session.child.as_mut().context("missing owned FVP child")?,
                        &deadline,
                    )
                })??;
            deadline.remaining()?;
            self.cancellation.check()?;
            if confirmation == LaunchConfirmation::Running {
                anyhow::ensure!(
                    self.child
                        .as_mut()
                        .context("missing owned FVP child")?
                        .try_wait()?
                        .is_none(),
                    "FVP model exited during launch"
                );
                self.model_confirmed = true;
                return Ok(port);
            }
            self.child
                .as_mut()
                .context("missing owned FVP child")?
                .terminate(&budget.deadline)?;
            self.child = None;
            self.docker.cleanup(&self.state, &budget.deadline)?;
            let mut state = self.state.clone();
            state.model = None;
            self.lock.write_state(Some(&self.state), &state)?;
            self.state = state;
            self.phases = None;
        }
    }

    /// Begin readiness only after the launch callback confirmed the model.
    /// Each probe (including RPC connection attempts) must use this deadline and
    /// finish its helper processes before returning. One durable intent covers
    /// the complete readiness loop, without rewriting state on every poll.
    pub fn wait_ready(
        &mut self,
        timeout: Duration,
        mut probe: impl FnMut(&Deadline) -> anyhow::Result<bool>,
    ) -> anyhow::Result<()> {
        self.wait_ready_scoped(timeout, |deadline| probe(deadline).map(Ok))
    }

    /// Readiness with explicit helper-scope completion. An inner error reports
    /// a failed probe whose host helpers are all finished; an outer error keeps
    /// ambiguous helper ownership durable for recovery.
    pub fn wait_ready_scoped(
        &mut self,
        timeout: Duration,
        mut probe: impl FnMut(&Deadline) -> anyhow::Result<anyhow::Result<bool>>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.model_confirmed, "FVP model start was not confirmed");
        let deadline = *self
            .phases
            .as_mut()
            .context("FVP model has not started")?
            .advance(Phase::PipetteReadiness, timeout)?;
        let outcome = self.run_callback(CallbackPhase::ReadinessProbe, &deadline, |session| {
            loop {
                // Supervisor errors occur outside a probe's helper scope.
                // Preserve probe errors as ambiguous, but record completed
                // scopes before reporting supervisor cancellation/timeout.
                if let Err(error) = session.check_running(&deadline) {
                    return Ok(Err(error));
                }
                match probe(&deadline)? {
                    Ok(true) => return Ok(Ok(())),
                    Ok(false) => {}
                    Err(error) => return Ok(Err(error)),
                }
                if let Err(error) = pause(&deadline, &session.cancellation) {
                    return Ok(Err(error));
                }
            }
        })?;
        outcome?;
        self.check_running(&deadline)?;
        self.ready = true;
        Ok(())
    }

    fn check_running(&mut self, deadline: &Deadline) -> anyhow::Result<()> {
        self.cancellation.check()?;
        deadline.remaining()?;
        anyhow::ensure!(
            self.child
                .as_mut()
                .context("missing owned FVP model")?
                .try_wait()?
                .is_none(),
            "FVP model exited before guest shutdown"
        );
        Ok(())
    }

    /// Start test execution at dispatch, not at boot or connection setup.
    pub fn execute_test<T>(
        &mut self,
        timeout: Duration,
        dispatch: impl FnOnce(&Deadline, &Cancellation) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        anyhow::ensure!(self.ready, "FVP pipette readiness was not confirmed");
        let deadline = *self
            .phases
            .as_mut()
            .context("FVP model has not started")?
            .advance(Phase::TestExecution, timeout)?;
        self.check_running(&deadline)?;
        let result = self.run_callback(CallbackPhase::TestDispatch, &deadline, |session| {
            dispatch(&deadline, &session.cancellation)
        })?;
        self.cancellation.check()?;
        deadline.remaining()?;
        Ok(result)
    }

    /// Request or observe guest shutdown, then wait within the same budget.
    /// Confirmed readiness can lead directly here when no tests were dispatched.
    pub fn shutdown(
        &mut self,
        timeout: Duration,
        request: impl FnOnce(&Deadline) -> anyhow::Result<()>,
    ) -> anyhow::Result<ExitStatus> {
        self.shutdown_scoped(timeout, |deadline| request(deadline).map(Ok))
    }

    /// Shutdown with explicit helper-scope completion, as in
    /// [`Self::wait_ready_scoped`].
    pub fn shutdown_scoped(
        &mut self,
        timeout: Duration,
        request: impl FnOnce(&Deadline) -> anyhow::Result<anyhow::Result<()>>,
    ) -> anyhow::Result<ExitStatus> {
        anyhow::ensure!(self.ready, "FVP pipette readiness was not confirmed");
        let deadline = *self
            .phases
            .as_mut()
            .context("FVP model has not started")?
            .advance(Phase::GuestShutdown, timeout)?;
        self.cancellation.check()?;
        self.run_callback(CallbackPhase::ShutdownRequest, &deadline, |_| {
            request(&deadline)
        })??;
        loop {
            self.cancellation.check()?;
            deadline.remaining()?;
            if let Some(status) = self
                .child
                .as_mut()
                .context("missing owned FVP model")?
                .try_wait()?
            {
                return Ok(status);
            }
            pause(&deadline, &self.cancellation)?;
        }
    }

    /// Stop the registered model and containers without retiring their state.
    ///
    /// This selects explicit finalization: drop will not retire state afterward.
    /// Run [`Self::post_verify`] and preserve workspace outputs before calling
    /// [`Self::retire_state`]. Cancellation does not interrupt resource cleanup.
    pub fn stop_resources(&mut self) -> anyhow::Result<()> {
        self.defer_retirement = true;
        if self.cleanup_deadline.is_none() {
            self.cleanup_deadline = Some(Deadline::new(self.forced_cleanup)?);
        }
        if !self.cleaned && self.state.post_verification == PostVerification::NotRequired {
            let mut state = self.state.clone();
            state.post_verification = PostVerification::Pending;
            self.lock.write_state(Some(&self.state), &state)?;
            self.state = state;
        }
        self.stop_resources_inner()
    }

    fn stop_resources_inner(&mut self) -> anyhow::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        let deadline = match self.cleanup_deadline {
            Some(deadline) => deadline,
            None => {
                let deadline = Deadline::new(self.forced_cleanup)?;
                self.cleanup_deadline = Some(deadline);
                deadline
            }
        };
        anyhow::ensure!(
            self.lock.read_state()?.as_ref() == Some(&self.state),
            "FVP state changed before cleanup; preserving state and containers"
        );
        if self.state.resources_stopped {
            return Ok(());
        }
        self.input_writer.take();
        if let Some(child) = &mut self.child {
            child.terminate(&deadline)?;
        }
        self.docker.cleanup(&self.state, &deadline)?;
        let mut state = self.state.clone();
        state.resources_stopped = true;
        self.lock.write_state(Some(&self.state), &state)?;
        self.state = state;
        Ok(())
    }

    /// Verify toolchain identity after stopping resources, even after cancellation.
    ///
    /// The callback receives a fresh, non-cancelled token and an explicit
    /// verification deadline, independent of the completed cleanup budget.
    /// Return an inner error for a completed verification that found drift;
    /// an outer error means helper ownership remains unresolved. Both retain
    /// the original run's callback intent. SIGKILL during verification leaves
    /// durable in-flight state and cannot erase the recovery record.
    pub fn post_verify<T>(
        &mut self,
        deadline: &Deadline,
        verify: impl FnOnce(&Deadline, &Cancellation) -> anyhow::Result<anyhow::Result<T>>,
    ) -> anyhow::Result<anyhow::Result<T>> {
        anyhow::ensure!(
            !self.cleaned && self.state.resources_stopped,
            "FVP post-verification requires stopped model resources"
        );
        anyhow::ensure!(
            self.state.post_verification == PostVerification::Pending,
            "FVP post-verification was already attempted or not requested"
        );
        if let Err(error) = deadline.remaining() {
            self.settle_post_verification(false)?;
            return Err(error);
        }
        let mut state = self.state.clone();
        state.post_verification = PostVerification::InFlight;
        self.lock.write_state(Some(&self.state), &state)?;
        self.state = state;
        let verification_token = Cancellation::default();
        if let Err(error) = deadline.remaining() {
            self.settle_post_verification(false)?;
            return Err(error);
        }
        let outcome = verify(deadline, &verification_token)?;
        anyhow::ensure!(
            self.state.owner.pid == std::process::id()
                && self.state.owner.observe()? == ProcessObservation::Matching,
            "FVP post-verification owner changed; preserving state"
        );
        self.settle_post_verification(outcome.is_ok() && deadline.remaining().is_ok())?;
        let outcome = match (outcome, deadline.remaining()) {
            (Ok(value), Ok(_)) => Ok(value),
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(timeout)) => {
                Err(error.context(format!("FVP verification also timed out: {timeout:#}")))
            }
        };
        if outcome.is_err() && self.state.post_verification != PostVerification::Failed {
            self.settle_post_verification(false)?;
        }
        Ok(outcome)
    }

    fn settle_post_verification(&mut self, succeeded: bool) -> anyhow::Result<()> {
        let mut state = self.state.clone();
        state.post_verification = if succeeded {
            PostVerification::Complete
        } else {
            PostVerification::Failed
        };
        self.lock.write_state(Some(&self.state), &state)?;
        self.state = state;
        Ok(())
    }

    /// Retire state only after explicit post-verification has finished.
    ///
    /// Filesystem retirement has its own non-resetting budget, since toolchain
    /// verification can outlast the process cleanup phase. Failed or incomplete
    /// helper scopes remain recorded, even if the known model was stopped.
    pub fn retire_state(&mut self) -> anyhow::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        anyhow::ensure!(
            self.state.resources_stopped,
            "FVP resources must be stopped before state retirement"
        );
        anyhow::ensure!(
            matches!(
                self.state.post_verification,
                PostVerification::NotRequired
                    | PostVerification::Complete
                    | PostVerification::Failed
            ),
            "FVP post-verification is unfinished; preserving state"
        );
        let deadline = match self.retirement_deadline {
            Some(deadline) => deadline,
            None => {
                let deadline = Deadline::new(self.forced_cleanup)?;
                self.retirement_deadline = Some(deadline);
                deadline
            }
        };
        self.retire_state_inner(&deadline)
    }

    fn retire_state_inner(&mut self, deadline: &Deadline) -> anyhow::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        deadline.remaining()?;
        anyhow::ensure!(
            self.lock.read_state()?.as_ref() == Some(&self.state),
            "FVP state changed before retirement; preserving workspace and state"
        );
        anyhow::ensure!(
            self.state.resources_stopped,
            "FVP resources must be stopped before state retirement"
        );
        anyhow::ensure!(
            matches!(
                self.state.post_verification,
                PostVerification::NotRequired
                    | PostVerification::Complete
                    | PostVerification::Failed
            ),
            "FVP post-verification is unfinished; preserving state"
        );
        anyhow::ensure!(
            self.state.callback_phase == CallbackPhase::Idle,
            "FVP {:?} callback ownership is unresolved; preserving state; \
             manual intervention is required: runtime={}, run={}, \
             owner_pid={}, owner_start_ticks={}",
            self.state.callback_phase,
            self.lock.directory.path.display(),
            self.state.run_id.as_str(),
            self.state.owner.pid,
            self.state.owner.start_ticks,
        );
        if let Some(workspace) = self.state.workspace.clone() {
            workspace.preflight(deadline)?;
            if !workspace.removal_started {
                let mut state = self.state.clone();
                state
                    .workspace
                    .as_mut()
                    .context("missing FVP workspace")?
                    .removal_started = true;
                self.lock.write_state(Some(&self.state), &state)?;
                self.state = state;
            }
            workspace.remove(deadline)?;
            let mut state = self.state.clone();
            state.workspace = None;
            self.lock.write_state(Some(&self.state), &state)?;
            self.state = state;
        }
        self.lock.remove_state(&self.state)?;
        self.cleaned = true;
        Ok(())
    }

    /// Compatibility cleanup for callers without explicit post-verification.
    /// Registered finalization requirements are never bypassed.
    pub fn cleanup(&mut self) -> anyhow::Result<()> {
        self.stop_resources_inner()?;
        if self.cleaned {
            return Ok(());
        }
        let deadline = self
            .cleanup_deadline
            .context("missing FVP cleanup deadline")?;
        self.retire_state_inner(&deadline)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let result = if self.defer_retirement && !self.cleaned {
            self.stop_resources_inner().map(|()| {
                tracing::warn!(
                    run_id = self.state.run_id.as_str(),
                    "FVP finalization unfinished; retaining recovery state"
                );
            })
        } else {
            self.cleanup()
        };
        if let Err(error) = result {
            tracing::error!(
                run_id = self.state.run_id.as_str(),
                error = %format!("{error:#}"),
                "FVP cleanup failed; retained state requires recovery"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;
    use std::rc::Rc;
    use test_with_tracing::test;

    const IMAGE: &str = "example.invalid/model@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn directory() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fvp-lifecycle-tests");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(canonical(&base).unwrap()).unwrap()
    }

    fn runtime(path: &Path) -> RuntimeDirectory {
        RuntimeDirectory::open(path, &[]).unwrap()
    }

    fn lock(path: &Path) -> RuntimeLock {
        runtime(path)
            .lock(Duration::ZERO, &Cancellation::default())
            .unwrap()
    }

    fn state() -> RunState {
        RunState {
            schema_version: SCHEMA_VERSION,
            kind: STATE_KIND.to_owned(),
            run_id: RunId::new().unwrap(),
            owner: ProcessIdentity::capture(std::process::id(), &std::env::current_exe().unwrap())
                .unwrap(),
            docker: docker_binding(),
            expected_image: IMAGE.to_owned(),
            expected_model: canonical(&std::env::current_exe().unwrap()).unwrap(),
            model: None,
            launch_pending: false,
            callback_phase: CallbackPhase::Idle,
            resources_stopped: false,
            post_verification: PostVerification::NotRequired,
            workspace: None,
        }
    }

    pub(super) fn docker_binding() -> DockerBinding {
        DockerBinding {
            endpoint: "unix:///fake/daemon-a.sock".to_owned(),
            daemon_id: "daemon-a".to_owned(),
        }
    }

    #[derive(Default)]
    struct FakeDocker {
        records: Vec<serde_json::Value>,
        calls: Vec<Vec<String>>,
        daemon_id: String,
        switch_to_empty_daemon_on_ps: Option<String>,
        fail_once: Option<(&'static str, bool)>,
    }

    fn fake_docker(records: Vec<serde_json::Value>) -> (Docker, Rc<RefCell<FakeDocker>>) {
        fake_docker_bound(docker_binding(), records)
    }

    fn fake_docker_bound(
        binding: DockerBinding,
        records: Vec<serde_json::Value>,
    ) -> (Docker, Rc<RefCell<FakeDocker>>) {
        let fixture = Rc::new(RefCell::new(FakeDocker {
            records,
            calls: Vec::new(),
            daemon_id: binding.daemon_id.clone(),
            switch_to_empty_daemon_on_ps: None,
            fail_once: None,
        }));
        let shared = fixture.clone();
        let expected_endpoint = binding.endpoint.clone();
        let initial_daemon = binding.daemon_id.clone();
        (
            Docker {
                executable: PathBuf::from("never-executed"),
                binding,
                responder: Some(Box::new(move |command, args| {
                    assert_eq!(command.get_args().next().unwrap(), "--host");
                    assert_eq!(
                        command.get_args().nth(1).unwrap(),
                        expected_endpoint.as_str()
                    );
                    assert_eq!(
                        command
                            .get_envs()
                            .find(|(name, _)| *name == "DOCKER_HOST")
                            .unwrap()
                            .1
                            .unwrap(),
                        expected_endpoint.as_str()
                    );
                    let mut fake = shared.borrow_mut();
                    fake.calls
                        .push(args.iter().map(|s| (*s).to_owned()).collect());
                    if fake
                        .fail_once
                        .is_some_and(|(operation, _)| operation == args[0])
                    {
                        let (_, disappear) = fake.fail_once.take().unwrap();
                        if disappear {
                            let id = args.last().unwrap();
                            fake.records.retain(|record| record["Id"] != *id);
                        }
                        anyhow::bail!("injected Docker command failure");
                    }
                    match args[0] {
                        "info" => Ok(serde_json::to_vec(&fake.daemon_id).unwrap()),
                        "ps" => {
                            if let Some(daemon) = fake.switch_to_empty_daemon_on_ps.take() {
                                fake.daemon_id = daemon;
                            }
                            if fake.daemon_id != initial_daemon {
                                return Ok(Vec::new());
                            }
                            Ok(fake
                                .records
                                .iter()
                                .filter(|record| {
                                    args.last()
                                        .and_then(|filter| filter.strip_prefix("id="))
                                        .is_none_or(|id| record["Id"] == id)
                                })
                                .map(|record| format!("{}\n", record["Id"].as_str().unwrap()))
                                .collect::<String>()
                                .into_bytes())
                        }
                        "inspect" => {
                            let id = args.last().unwrap();
                            let record = fake.records.iter().find(|r| r["Id"] == *id).unwrap();
                            Ok(serde_json::to_vec(&[record]).unwrap())
                        }
                        "rm" => {
                            assert_eq!(&args[..3], &["rm", "--force", "--"]);
                            let id = args.last().unwrap();
                            fake.records.retain(|r| r["Id"] != *id);
                            Ok(Vec::new())
                        }
                        command => panic!("unexpected fake Docker command: {command}"),
                    }
                })),
            },
            fixture,
        )
    }

    fn container(record: &RunState, id: char) -> serde_json::Value {
        serde_json::json!({
            "Id": id.to_string().repeat(64),
            "Config": {
                "Image": record.expected_image,
                "Labels": record.container_labels(),
            },
        })
    }

    fn dead_owner(record: &mut RunState) {
        // Linux PID_MAX_LIMIT is below i32::MAX. No process is signalled.
        record.owner.pid = i32::MAX as u32;
    }

    #[test]
    fn container_auto_removal_requires_exact_absence_and_preserves_foreign_records() {
        for (operation, disappear, newer) in [
            ("inspect", true, false),
            ("rm", true, false),
            ("inspect", false, false),
            ("inspect", true, true),
        ] {
            let state = state();
            let first = container(&state, 'a');
            let mut second = container(&state, 'b');
            if newer {
                second["Config"]["Labels"][SCHEMA_LABEL] = serde_json::json!("999");
            }
            let (docker, fake) = fake_docker(vec![first, second]);
            fake.borrow_mut().fail_once = Some((operation, disappear));
            let result = docker.cleanup(&state, &Deadline::new(Duration::from_secs(5)).unwrap());
            assert_eq!(
                result.is_ok(),
                disappear && !newer,
                "{operation}: {result:?}"
            );
            let fake = fake.borrow();
            if result.is_ok() {
                assert!(fake.records.is_empty());
            } else {
                assert!(!fake.calls.iter().any(|call| call[0] == "rm"));
                assert!(
                    fake.records
                        .iter()
                        .any(|record| record["Id"] == "b".repeat(64))
                );
            }
            assert!(
                fake.calls
                    .iter()
                    .any(|call| call.last() == Some(&format!("id={}", "a".repeat(64))))
            );
        }
    }

    fn registered_workspace(root: &Path) -> (Session, PathBuf) {
        let parent = root.join("workspace-parent");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&parent)
            .unwrap();
        let workspace = tempfile::Builder::new()
            .prefix("openvmm-fvp-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(&parent)
            .unwrap()
            .keep();
        std::fs::create_dir(workspace.join("logs")).unwrap();
        let (docker, _) = fake_docker(Vec::new());
        let mut session = Session::new(
            lock(root),
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        session
            .register_workspace(&workspace, &Deadline::new(Duration::from_secs(5)).unwrap())
            .unwrap();
        std::fs::write(workspace.join("logs/model.log"), b"retained log").unwrap();
        (session, workspace)
    }

    fn sealed_workspace(root: &Path) -> (Session, PathBuf, PathBuf) {
        let (mut session, workspace) = registered_workspace(root);
        let output = root.join("outputs");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&output)
            .unwrap();
        session
            .register_output_destination(&output, &Deadline::new(Duration::from_secs(5)).unwrap())
            .unwrap();
        session.stop_resources().unwrap();
        session
            .post_verify(
                &Deadline::new(Duration::from_secs(5)).unwrap(),
                |_, token| {
                    token.check()?;
                    Ok(Ok(()))
                },
            )
            .unwrap()
            .unwrap();
        std::fs::copy(workspace.join("logs/model.log"), output.join("model.log")).unwrap();
        session
            .record_preserved_outputs(&output, &Deadline::new(Duration::from_secs(5)).unwrap())
            .unwrap();
        (session, workspace, output)
    }

    #[test]
    fn run_id_is_random_and_controlled() {
        let first = RunId::new().unwrap();
        let second = RunId::new().unwrap();
        assert_ne!(first, second);
        first.validate().unwrap();
        assert!(RunId("../foreign".to_owned()).validate().is_err());
    }

    #[test]
    fn lock_contention_and_persistent_inode() {
        let dir = directory();
        let held = lock(dir.path());
        let inode = held.file.metadata().unwrap().ino();
        let start = std::time::Instant::now();
        assert!(
            runtime(dir.path())
                .lock(Duration::ZERO, &Cancellation::default())
                .is_err()
        );
        assert!(
            runtime(dir.path())
                .lock(Duration::from_millis(40), &Cancellation::default())
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(held);
        assert_eq!(lock(dir.path()).file.metadata().unwrap().ino(), inode);
        assert!(
            runtime(dir.path())
                .lock(Duration::from_secs(7201), &Cancellation::default())
                .is_err()
        );
    }

    #[test]
    fn runtime_rejects_input_and_staging_overlap() {
        let dir = directory();
        assert!(RuntimeDirectory::open(dir.path(), &[dir.path()]).is_err());
        let staging = dir.path().join("fvp-v1/run");
        assert!(RuntimeDirectory::open(dir.path(), &[&staging]).is_err());
        assert!(!dir.path().join("fvp-v1").exists());
    }

    #[test]
    fn invalid_and_foreign_state_is_preserved() {
        let dir = directory();
        let lock = lock(dir.path());
        let valid = serde_json::to_value(state()).unwrap();
        let mut newer = valid.clone();
        newer["schema_version"] = 2.into();
        let mut foreign = valid.clone();
        foreign["kind"] = "another-launcher".into();
        let mut invalid_run = valid;
        invalid_run["run_id"] = "../foreign".into();
        for bytes in [
            b"{malformed".to_vec(),
            serde_json::to_vec(&newer).unwrap(),
            serde_json::to_vec(&foreign).unwrap(),
            serde_json::to_vec(&invalid_run).unwrap(),
        ] {
            let path = lock.state_path();
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            file.write_all(&bytes).unwrap();
            assert!(lock.read_state().is_err());
            assert!(lock.write_state(None, &state()).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn atomic_state_updates_and_removal_require_exact_ownership() {
        let dir = directory();
        let lock = lock(dir.path());
        let first = state();
        lock.write_state(None, &first).unwrap();
        assert_eq!(lock.read_state().unwrap(), Some(first.clone()));
        let foreign = state();
        assert!(lock.write_state(Some(&foreign), &first).is_err());
        assert!(lock.remove_state(&foreign).is_err());
        lock.remove_state(&first).unwrap();
        assert!(lock.read_state().unwrap().is_none());
        assert!(lock.directory.path.join("model.lock").is_file());
    }

    #[test]
    fn state_symlinks_and_hardlinks_are_not_followed() {
        let dir = directory();
        let lock = lock(dir.path());
        let foreign = dir.path().join("foreign");
        std::fs::write(&foreign, b"leave me").unwrap();
        std::os::unix::fs::symlink(&foreign, lock.state_path()).unwrap();
        assert!(lock.read_state().is_err());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"leave me");
        std::fs::remove_file(lock.state_path()).unwrap();
        std::fs::hard_link(&foreign, lock.state_path()).unwrap();
        assert!(lock.read_state().is_err());
    }

    #[test]
    fn stat_parser_handles_spaces_and_parentheses() {
        let fields = std::iter::once("S")
            .chain(std::iter::repeat_n("0", 18))
            .chain(std::iter::once("12345"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            process_start_ticks(&format!("42 (a ) strange ( name)) {fields}"), 42).unwrap(),
            12345
        );
        assert!(process_start_ticks("42 (short) S", 42).is_err());
        assert!(process_start_ticks(&format!("41 (wrong) {fields}"), 42).is_err());
    }

    #[test]
    fn live_and_reused_owner_are_never_recovered() {
        let dir = directory();
        let lock = lock(dir.path());
        let mut record = state();
        assert_eq!(
            record.owner.observe().unwrap(),
            ProcessObservation::Matching
        );
        record.owner.start_ticks += 1;
        assert_eq!(record.owner.observe().unwrap(), ProcessObservation::Reused);
        lock.write_state(None, &record).unwrap();
        // A nonexistent Docker executable proves recovery stopped before Docker.
        let error = lock
            .recover(
                &Docker::new(dir.path().join("must-not-run")),
                IMAGE,
                &record.expected_model,
                Duration::from_secs(1),
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("PID was reused"));
        assert_eq!(lock.read_state().unwrap(), Some(record));
    }

    #[test]
    fn docker_requires_every_ownership_label_and_exact_image_and_id() {
        let state = state();
        let id = "a".repeat(64);
        let record = serde_json::json!([{
            "Id": id,
            "Config": {
                "Image": IMAGE,
                "Labels": state.container_labels(),
            },
        }]);
        validate_container(&serde_json::to_vec(&record).unwrap(), &id, &state).unwrap();
        for label in [RUN_LABEL, SCHEMA_LABEL, IMAGE_LABEL, ROLE_LABEL] {
            let mut changed = record.clone();
            changed[0]["Config"]["Labels"][label] = "foreign-or-newer".into();
            assert!(
                validate_container(&serde_json::to_vec(&changed).unwrap(), &id, &state).is_err()
            );
        }
        let mut image = record.clone();
        image[0]["Config"]["Image"] = "foreign".into();
        assert!(validate_container(&serde_json::to_vec(&image).unwrap(), &id, &state).is_err());
        assert!(
            validate_container(
                &serde_json::to_vec(&record).unwrap(),
                &"b".repeat(64),
                &state
            )
            .is_err()
        );
    }

    #[test]
    fn docker_failure_is_explicit_and_keeps_state() {
        let dir = directory();
        let mut session = Session::new(
            lock(dir.path()),
            Docker::new(PathBuf::from("/usr/bin/false")),
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        let error = session.cleanup().unwrap_err();
        assert!(format!("{error:#}").contains("Docker command"));
        assert_eq!(
            session.lock.read_state().unwrap().as_ref(),
            Some(session.state())
        );
    }

    #[test]
    fn endpoint_rejects_stale_id_changed_port_and_non_loopback() {
        let run_id = RunId::new().unwrap();
        let endpoint = Endpoint::new(run_id.clone(), 12345).unwrap();
        let bytes = serde_json::to_vec(&endpoint).unwrap();
        assert_eq!(
            Endpoint::parse(&bytes, &run_id, 12345).unwrap().address(),
            endpoint.address()
        );
        assert!(Endpoint::parse(&bytes, &RunId::new().unwrap(), 12345).is_err());
        assert!(Endpoint::parse(&bytes, &run_id, 23456).is_err());
        let mut record = serde_json::to_value(endpoint).unwrap();
        record["address"] = "0.0.0.0:12345".into();
        assert!(Endpoint::parse(&serde_json::to_vec(&record).unwrap(), &run_id, 12345).is_err());
    }

    #[test]
    fn allocation_attempt_and_time_budgets_never_reset() {
        let cancel = Cancellation::default();
        let mut budget = PortBudget::new(Duration::from_secs(10), 2).unwrap();
        let first = budget.allocate(&cancel).unwrap();
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, first.port())).is_err());
        first.release(Duration::from_secs(30)).unwrap();
        drop(budget.allocate(&cancel).unwrap());
        assert!(budget.allocate(&cancel).is_err());
        assert_eq!(budget.attempts(), 2);
        budget.deadline = Deadline::new(Duration::ZERO).unwrap();
        assert!(budget.allocate(&cancel).is_err());
        assert_eq!(budget.attempts(), 2);
    }

    #[test]
    fn failed_launch_command_consumes_attempt_and_elapsed_budget() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        let mut budget = PortBudget {
            deadline: Deadline::new(Duration::from_millis(30)).unwrap(),
            maximum_attempts: 2,
            attempts: 0,
        };
        let result = session.launch_with_ports(
            &mut budget,
            Duration::from_secs(1),
            |_, _, deadline| {
                std::thread::sleep(Duration::from_millis(40));
                deadline.remaining()?;
                anyhow::bail!("injected preparation failure")
            },
            |_, _| anyhow::bail!("must not launch"),
        );
        assert!(result.is_err());
        assert_eq!(budget.attempts(), 1);
        assert!(budget.allocate(&Cancellation::default()).is_err());
    }

    #[test]
    fn phases_reject_resets_and_late_success_but_allow_cleanup() {
        let mut phases = PhaseClock::model_start(Deadline::new(Duration::from_secs(1)).unwrap());
        assert!(
            phases
                .advance(Phase::TestExecution, Duration::from_secs(1))
                .is_err()
        );
        phases
            .advance(Phase::PipetteReadiness, Duration::ZERO)
            .unwrap();
        assert!(
            phases
                .advance(Phase::PipetteReadiness, Duration::from_secs(1))
                .is_err()
        );
        assert!(
            phases
                .advance(Phase::TestExecution, Duration::from_secs(1))
                .is_err()
        );
        phases
            .advance(Phase::ForcedCleanup, Duration::from_secs(1))
            .unwrap();
        assert!(
            phases
                .advance(Phase::ForcedCleanup, Duration::from_secs(1))
                .is_err()
        );
    }

    #[test]
    fn cancellation_prevents_lock_and_allocation() {
        let dir = directory();
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(runtime(dir.path()).lock(Duration::ZERO, &cancel).is_err());
        let mut ports = PortBudget::new(Duration::from_secs(10), 1).unwrap();
        assert!(ports.allocate(&cancel).is_err());
        assert_eq!(ports.attempts(), 0);
    }

    #[test]
    fn sigkill_recovery_removes_only_proven_containers_and_state() {
        let dir = directory();
        let lock = lock(dir.path());
        let mut record = state();
        dead_owner(&mut record);
        lock.write_state(None, &record).unwrap();
        let (docker, fake) = fake_docker(vec![container(&record, 'a')]);
        let inode = lock.file.metadata().unwrap().ino();
        lock.recover(
            &docker,
            IMAGE,
            &record.expected_model,
            Duration::from_secs(1),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(lock.read_state().unwrap().is_none());
        assert!(fake.borrow().records.is_empty());
        assert_eq!(lock.file.metadata().unwrap().ino(), inode);
    }

    #[test]
    fn foreign_or_newer_container_preserves_entire_recovery_inventory() {
        for label in [RUN_LABEL, SCHEMA_LABEL, IMAGE_LABEL, ROLE_LABEL] {
            let dir = directory();
            let lock = lock(dir.path());
            let mut record = state();
            dead_owner(&mut record);
            lock.write_state(None, &record).unwrap();
            let owned = container(&record, 'a');
            let mut foreign = container(&record, 'b');
            foreign["Config"]["Labels"][label] = "foreign-or-newer".into();
            let records = vec![owned, foreign];
            let (docker, fake) = fake_docker(records.clone());
            assert!(
                lock.recover(
                    &docker,
                    IMAGE,
                    &record.expected_model,
                    Duration::from_secs(1),
                    &Cancellation::default(),
                )
                .is_err()
            );
            assert_eq!(lock.read_state().unwrap(), Some(record));
            assert_eq!(fake.borrow().records, records);
            assert!(fake.borrow().calls.iter().all(|call| call[0] != "rm"));
        }
    }

    #[test]
    fn surviving_orphan_is_not_signalled_or_forgotten() {
        let dir = directory();
        let lock = lock(dir.path());
        let mut record = state();
        dead_owner(&mut record);
        record.expected_model = canonical(Path::new("/usr/bin/sleep")).unwrap();
        let mut child = ManagedChild::spawn(
            Command::new(&record.expected_model)
                .arg("30")
                .env("OPENVMM_FVP_RUN_ID", record.run_id.as_str()),
        )
        .unwrap();
        record.model = Some(
            capture_owned_process(
                &mut child,
                &record.expected_model,
                &record.run_id,
                &Deadline::new(Duration::from_secs(1)).unwrap(),
                &Cancellation::default(),
            )
            .unwrap(),
        );
        lock.write_state(None, &record).unwrap();
        let (docker, _) = fake_docker(Vec::new());
        let error = lock
            .recover(
                &docker,
                IMAGE,
                &record.expected_model,
                Duration::from_millis(40),
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("manual intervention"));
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains(&format!("pid={}", child.id())));
        assert!(diagnostic.contains("start_ticks="));
        assert!(diagnostic.contains(&lock.directory.path.display().to_string()));
        assert!(diagnostic.contains("containers=[]"));
        assert!(child.try_wait().unwrap().is_none());
        assert_eq!(lock.read_state().unwrap(), Some(record));
        child
            .terminate(&Deadline::new(Duration::from_secs(1)).unwrap())
            .unwrap();
    }

    #[test]
    fn collision_retry_readiness_test_and_shutdown() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &model,
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        let mut budget = PortBudget::new(Duration::from_secs(10), 2).unwrap();
        let mut confirmations = 0;
        let port = session
            .launch_with_ports(
                &mut budget,
                Duration::from_secs(1),
                |_, _, _| {
                    let mut command = Command::new(&model);
                    command.arg("0.2");
                    Ok(command)
                },
                |_, _| {
                    confirmations += 1;
                    Ok(if confirmations == 1 {
                        LaunchConfirmation::PortCollision
                    } else {
                        LaunchConfirmation::Running
                    })
                },
            )
            .unwrap();
        assert_eq!(budget.attempts(), 2);
        assert!(
            session
                .execute_test::<()>(Duration::from_secs(1), |_, _| {
                    anyhow::bail!("must not dispatch before readiness")
                })
                .unwrap_err()
                .to_string()
                .contains("readiness was not confirmed")
        );
        let bytes = serde_json::to_vec(&Endpoint::new(session.state.run_id.clone(), port).unwrap())
            .unwrap();
        let run_id = session.state.run_id.clone();
        session
            .wait_ready(Duration::from_secs(1), |_| {
                Endpoint::parse(&bytes, &run_id, port)?;
                Ok(true)
            })
            .unwrap();
        assert_eq!(
            session
                .execute_test(Duration::from_secs(1), |_, _| Ok(42))
                .unwrap(),
            42
        );
        assert!(
            session
                .shutdown(Duration::from_secs(1), |_| Ok(()))
                .unwrap()
                .success()
        );
        let log = session.log_path();
        session.cleanup().unwrap();
        assert!(session.lock.read_state().unwrap().is_none());
        assert!(log.is_file());
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok());
    }

    #[test]
    fn cancellation_cleans_owned_child_and_retains_lock_inode() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
        let cancellation = Cancellation::default();
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &model,
            Duration::from_secs(10),
            cancellation.clone(),
        )
        .unwrap();
        let inode = session.lock.file.metadata().unwrap().ino();
        let mut budget = PortBudget::new(Duration::from_secs(10), 1).unwrap();
        session
            .launch_with_ports(
                &mut budget,
                Duration::from_secs(1),
                |_, _, _| {
                    let mut command = Command::new(&model);
                    command.arg("30");
                    Ok(command)
                },
                |_, _| Ok(LaunchConfirmation::Running),
            )
            .unwrap();
        let identity = session.state.model.clone().unwrap();
        cancellation.cancel();
        assert!(
            session
                .wait_ready(Duration::from_secs(1), |_| Ok(true))
                .is_err()
        );
        drop(session);
        assert_eq!(identity.observe().unwrap(), ProcessObservation::Gone);
        let lock = lock(dir.path());
        assert!(lock.read_state().unwrap().is_none());
        assert_eq!(lock.file.metadata().unwrap().ino(), inode);
    }

    #[test]
    fn crash_during_registration_preserves_ambiguous_ownership() {
        let dir = directory();
        let lock = lock(dir.path());
        let mut record = state();
        dead_owner(&mut record);
        record.launch_pending = true;
        lock.write_state(None, &record).unwrap();
        let (docker, fake) = fake_docker(vec![container(&record, 'a')]);
        let error = lock
            .recover(
                &docker,
                IMAGE,
                &record.expected_model,
                Duration::from_secs(1),
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("unrecorded launcher"));
        assert!(fake.borrow().calls.is_empty());
        assert_eq!(lock.read_state().unwrap(), Some(record));
    }

    #[test]
    fn failed_external_preparation_and_spawn_consume_the_same_attempt_budget() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        let mut budget = PortBudget::new(Duration::from_secs(10), 2).unwrap();
        assert!(
            session
                .launch_with_ports(
                    &mut budget,
                    Duration::from_secs(1),
                    |_, _, deadline| {
                        let output = run_command_with_cleanup(
                            &mut Command::new("/usr/bin/false"),
                            deadline,
                            deadline,
                        )?;
                        anyhow::ensure!(output.status.success(), "injected preparation failure");
                        anyhow::bail!("false unexpectedly succeeded")
                    },
                    |_, _| anyhow::bail!("must not confirm"),
                )
                .is_err()
        );
        assert_eq!(budget.attempts(), 1);
        assert!(
            session
                .launch_with_ports(
                    &mut budget,
                    Duration::from_secs(1),
                    |_, _, _| Ok(Command::new(dir.path().join("missing-executable"))),
                    |_, _| anyhow::bail!("must not confirm"),
                )
                .is_err()
        );
        assert_eq!(budget.attempts(), 1);
        assert!(session.cleanup().is_err());
        assert_eq!(
            session.lock.read_state().unwrap().unwrap().callback_phase,
            CallbackPhase::Preparation
        );
        drop(session);
        let second_dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let mut session = Session::new(
            lock(second_dir.path()),
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        assert!(
            session
                .launch_with_ports(
                    &mut budget,
                    Duration::from_secs(1),
                    |_, _, _| Ok(Command::new(second_dir.path().join("missing-executable"))),
                    |_, _| anyhow::bail!("must not confirm"),
                )
                .is_err()
        );
        assert_eq!(budget.attempts(), 2);
        assert!(budget.allocate(&Cancellation::default()).is_err());
        session.cleanup().unwrap();
        assert!(session.lock.read_state().unwrap().is_none());
    }

    #[test]
    fn sigint_and_sigterm_request_cancellation() {
        const CHILD: &str = "OPENVMM_FVP_SIGNAL_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            for signal in [
                nix::sys::signal::Signal::SIGINT,
                nix::sys::signal::Signal::SIGTERM,
            ] {
                let cancellation = Cancellation::default();
                let _guard = cancellation.install_signal_handlers().unwrap();
                nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(i32::try_from(std::process::id()).unwrap()),
                    signal,
                )
                .unwrap();
                let deadline = Deadline::new(Duration::from_secs(1)).unwrap();
                while cancellation.check().is_ok() {
                    deadline.remaining().unwrap();
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
            return;
        }
        // Keep process-global signal registrations out of the parallel suite.
        let deadline = Deadline::new(Duration::from_secs(5)).unwrap();
        let output = run_command_with_cleanup(
            Command::new(std::env::current_exe().unwrap())
                .arg("sigint_and_sigterm_request_cancellation")
                .env(CHILD, "1"),
            &deadline,
            &deadline,
        )
        .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}; stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[test]
    fn dropped_signal_guards_restore_default_termination() {
        const CHILD: &str = "OPENVMM_FVP_SIGNAL_DEFAULT_TEST_CHILD";
        if let Some(signal) = std::env::var_os(CHILD) {
            let signal: i32 = signal.to_str().unwrap().parse().unwrap();
            let cancellation = Cancellation::default();
            let first = cancellation.install_signal_handlers().unwrap();
            let second = cancellation.install_signal_handlers().unwrap();
            drop(first);
            signal_hook::low_level::raise(signal).unwrap();
            let deadline = Deadline::new(Duration::from_secs(1)).unwrap();
            while cancellation.check().is_ok() {
                deadline.remaining().unwrap();
                std::thread::sleep(POLL_INTERVAL);
            }
            drop(second);
            signal_hook::low_level::raise(signal).unwrap();
            panic!("default termination did not occur after dropping the last signal guard");
        }
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            let deadline = Deadline::new(Duration::from_secs(5)).unwrap();
            let output = run_command_with_cleanup(
                Command::new(std::env::current_exe().unwrap())
                    .arg("dropped_signal_guards_restore_default_termination")
                    .env(CHILD, signal.to_string()),
                &deadline,
                &deadline,
            )
            .unwrap();
            assert_eq!(
                std::os::unix::process::ExitStatusExt::signal(&output.status),
                Some(signal),
                "stdout: {}; stderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    #[test]
    fn signal_guards_cancel_only_active_owners_and_can_be_reinstalled() {
        const CHILD: &str = "OPENVMM_FVP_SIGNAL_OWNERS_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let first = Cancellation::default();
            let second = Cancellation::default();
            let first_guard = first.install_signal_handlers().unwrap();
            let second_guard = second.install_signal_handlers().unwrap();
            drop(first_guard);
            signal_hook::low_level::raise(signal_hook::consts::SIGTERM).unwrap();
            let deadline = Deadline::new(Duration::from_secs(1)).unwrap();
            while second.check().is_ok() {
                deadline.remaining().unwrap();
                std::thread::sleep(POLL_INTERVAL);
            }
            first.check().unwrap();
            drop(second_guard);
            let _reinstalled = first.install_signal_handlers().unwrap();
            signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
            let deadline = Deadline::new(Duration::from_secs(1)).unwrap();
            while first.check().is_ok() {
                deadline.remaining().unwrap();
                std::thread::sleep(POLL_INTERVAL);
            }
            return;
        }
        let deadline = Deadline::new(Duration::from_secs(5)).unwrap();
        let output = run_command_with_cleanup(
            Command::new(std::env::current_exe().unwrap())
                .arg("signal_guards_cancel_only_active_owners_and_can_be_reinstalled")
                .env(CHILD, "1"),
            &deadline,
            &deadline,
        )
        .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}; stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[test]
    fn phase_timeout_matrix_cleans_owned_resources() {
        for phase in [
            Phase::PipetteReadiness,
            Phase::TestExecution,
            Phase::GuestShutdown,
        ] {
            let dir = directory();
            let (docker, _) = fake_docker(Vec::new());
            let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
            let mut session = Session::new(
                lock(dir.path()),
                docker,
                IMAGE.to_owned(),
                &model,
                Duration::from_secs(10),
                Cancellation::default(),
            )
            .unwrap();
            let mut budget = PortBudget::new(Duration::from_secs(10), 1).unwrap();
            let port = session
                .launch_with_ports(
                    &mut budget,
                    Duration::from_secs(1),
                    |_, _, _| {
                        let mut command = Command::new(&model);
                        command.arg("30");
                        Ok(command)
                    },
                    |_, _| Ok(LaunchConfirmation::Running),
                )
                .unwrap();
            let identity = session.state.model.clone().unwrap();
            let started = std::time::Instant::now();
            let timeout = Duration::from_millis(20);
            let error = match phase {
                Phase::PipetteReadiness => session.wait_ready(timeout, |_| Ok(false)).unwrap_err(),
                Phase::TestExecution => {
                    session
                        .wait_ready(Duration::from_secs(1), |_| Ok(true))
                        .unwrap();
                    session
                        .execute_test(timeout, |_, _| {
                            std::thread::sleep(Duration::from_millis(40));
                            Ok(())
                        })
                        .unwrap_err()
                }
                Phase::GuestShutdown => {
                    session
                        .wait_ready(Duration::from_secs(1), |_| Ok(true))
                        .unwrap();
                    session
                        .execute_test(Duration::from_secs(1), |_, _| Ok(()))
                        .unwrap();
                    session.shutdown(timeout, |_| Ok(())).unwrap_err()
                }
                _ => unreachable!(),
            };
            assert!(
                format!("{error:#}").contains("deadline"),
                "{phase:?}: {error:#}"
            );
            session.cleanup().unwrap();
            assert!(session.lock.read_state().unwrap().is_none());
            assert!(started.elapsed() < Duration::from_secs(2), "{phase:?}");
            assert_eq!(identity.observe().unwrap(), ProcessObservation::Gone);
            assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok());
            drop(session);
            drop(lock(dir.path()));
        }
    }

    #[test]
    fn cleanup_timeout_is_not_reset_by_retry_or_drop() {
        let dir = directory();
        let calls = Rc::new(std::cell::Cell::new(0));
        let observed = calls.clone();
        let docker = Docker {
            executable: PathBuf::from("never-executed"),
            binding: docker_binding(),
            responder: Some(Box::new(move |_, _| {
                observed.set(observed.get() + 1);
                std::thread::sleep(Duration::from_millis(40));
                Ok(Vec::new())
            })),
        };
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        session.forced_cleanup = Duration::from_millis(20);
        assert!(session.cleanup().is_err());
        assert_eq!(calls.get(), 1);
        assert!(session.cleanup().is_err());
        assert!(session.lock.read_state().unwrap().is_some());
        drop(session);
        assert_eq!(calls.get(), 1);
        assert!(lock(dir.path()).read_state().unwrap().is_some());
    }

    #[test]
    fn process_registration_requires_run_identity_within_original_deadline() {
        let executable = canonical(Path::new("/usr/bin/sleep")).unwrap();
        let actual_run = RunId::new().unwrap();
        let mut child = ManagedChild::spawn(
            Command::new(&executable)
                .arg("30")
                .env("OPENVMM_FVP_RUN_ID", actual_run.as_str()),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let deadline = Deadline::new(Duration::from_millis(40)).unwrap();
        let error = capture_owned_process(
            &mut child,
            &executable,
            &RunId::new().unwrap(),
            &deadline,
            &Cancellation::default(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("registration failed"));
        assert!(deadline.expired());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(child.try_wait().unwrap().is_none());
        child
            .terminate(&Deadline::new(Duration::from_secs(1)).unwrap())
            .unwrap();
    }

    #[test]
    fn toolchain_readers_remain_locked_through_session_cleanup() {
        let dir = directory();
        let runtime = runtime(dir.path());
        let cancellation = Cancellation::default();
        let first = runtime
            .lock_toolchain(Duration::ZERO, &cancellation)
            .unwrap();
        let second = runtime
            .lock_toolchain(Duration::ZERO, &cancellation)
            .unwrap();
        let inode = first.file.metadata().unwrap().ino();
        let path = first.path().to_owned();
        assert!(acquire_lock_file(&path, false, Duration::ZERO, &cancellation).is_err());
        drop(second);
        let model_lock = runtime.lock(Duration::ZERO, &cancellation).unwrap();
        let (docker, _) = fake_docker(Vec::new());
        let mut session = Session::new(
            model_lock,
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            cancellation.clone(),
        )
        .unwrap();
        session.cleanup().unwrap();
        drop(session);
        first.check().unwrap();
        assert!(acquire_lock_file(&path, false, Duration::ZERO, &cancellation).is_err());
        // Inventory revalidation belongs here, before releasing the shared guard.
        drop(first);
        let writer = acquire_lock_file(&path, false, Duration::ZERO, &cancellation).unwrap();
        assert_eq!(writer.metadata().unwrap().ino(), inode);
    }

    #[test]
    fn toolchain_lock_has_a_bounded_wait_and_checks_its_inode() {
        let dir = directory();
        let runtime = runtime(dir.path());
        let cancellation = Cancellation::default();
        let path = runtime.path.join("toolchain.lock");
        let writer = acquire_lock_file(&path, false, Duration::ZERO, &cancellation).unwrap();
        let started = std::time::Instant::now();
        assert!(
            runtime
                .lock_toolchain(Duration::from_millis(40), &cancellation)
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(writer);
        let guard = runtime
            .lock_toolchain(Duration::ZERO, &cancellation)
            .unwrap();
        std::fs::rename(&path, runtime.path.join("replaced-toolchain.lock")).unwrap();
        let _replacement = acquire_lock_file(&path, false, Duration::ZERO, &cancellation).unwrap();
        assert!(guard.check().is_err());
    }

    #[test]
    fn state_fifo_is_rejected_without_waiting_and_preserved() {
        const CHILD: &str = "OPENVMM_FVP_FIFO_TEST_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let lock = lock(Path::new(&root));
            let started = std::time::Instant::now();
            let error = lock.read_state().unwrap_err();
            assert!(format!("{error:#}").contains("regular file"));
            assert!(started.elapsed() < Duration::from_secs(1));
            return;
        }
        let dir = directory();
        let runtime = runtime(dir.path());
        let path = runtime.path.join("state.json");
        let deadline = Deadline::new(Duration::from_secs(2)).unwrap();
        let output = run_command_with_cleanup(
            Command::new("/usr/bin/mkfifo")
                .args(["--mode=600", "--"])
                .arg(&path),
            &deadline,
            &deadline,
        )
        .unwrap();
        assert!(output.status.success());
        let before = std::fs::symlink_metadata(&path).unwrap();
        let deadline = Deadline::new(Duration::from_secs(2)).unwrap();
        let output = run_command_with_cleanup(
            Command::new(std::env::current_exe().unwrap())
                .arg("state_fifo_is_rejected_without_waiting_and_preserved")
                .env(CHILD, dir.path()),
            &deadline,
            &deadline,
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let after = std::fs::symlink_metadata(path).unwrap();
        assert!(after.file_type().is_fifo());
        assert_eq!(before.ino(), after.ino());
    }

    #[test]
    fn sigkill_inside_preparation_preserves_registration_intent() {
        const CHILD: &str = "OPENVMM_FVP_PREPARATION_CRASH_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = Path::new(&root);
            let (docker, _) = fake_docker(Vec::new());
            let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
            let mut session = Session::new(
                lock(root),
                docker,
                IMAGE.to_owned(),
                &model,
                Duration::from_secs(10),
                Cancellation::default(),
            )
            .unwrap();
            let state_path = session.lock.state_path();
            let mut budget = PortBudget::new(Duration::from_secs(10), 1).unwrap();
            let result = session.launch_with_ports(
                &mut budget,
                Duration::from_secs(1),
                |_, state, deadline| {
                    let persisted = RunState::parse(&std::fs::read(&state_path)?)?;
                    anyhow::ensure!(
                        persisted.launch_pending && persisted == *state,
                        "registration intent was not persisted before preparation"
                    );
                    let mut child = ManagedChild::spawn(
                        Command::new(&model)
                            .arg("0.25")
                            .env("OPENVMM_FVP_RUN_ID", state.run_id.as_str()),
                    )?;
                    let identity = capture_owned_process(
                        &mut child,
                        &model,
                        &state.run_id,
                        deadline,
                        &Cancellation::default(),
                    )?;
                    std::fs::write(root.join("orphan.json"), serde_json::to_vec(&identity)?)?;
                    signal_hook::low_level::raise(signal_hook::consts::SIGKILL)?;
                    anyhow::bail!("SIGKILL did not terminate the preparation owner")
                },
                |_, _| anyhow::bail!("must not reach model confirmation"),
            );
            panic!("preparation owner survived: {result:?}");
        }
        let dir = directory();
        let deadline = Deadline::new(Duration::from_secs(3)).unwrap();
        let output = run_command_with_cleanup(
            Command::new(std::env::current_exe().unwrap())
                .arg("sigkill_inside_preparation_preserves_registration_intent")
                .env(CHILD, dir.path()),
            &deadline,
            &deadline,
        )
        .unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&output.status),
            Some(signal_hook::consts::SIGKILL),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let lock = lock(dir.path());
        let record = lock.read_state().unwrap().unwrap();
        assert!(record.launch_pending);
        assert!(record.model.is_none());
        let (docker, fake) = fake_docker(Vec::new());
        let error = lock
            .recover(
                &docker,
                IMAGE,
                &record.expected_model,
                Duration::from_secs(1),
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("unrecorded launcher"));
        assert_eq!(lock.read_state().unwrap(), Some(record));
        assert!(fake.borrow().calls.is_empty());
        let identity: ProcessIdentity =
            serde_json::from_slice(&std::fs::read(dir.path().join("orphan.json")).unwrap())
                .unwrap();
        let deadline = Deadline::new(Duration::from_secs(1)).unwrap();
        loop {
            match identity.observe() {
                Ok(ProcessObservation::Gone | ProcessObservation::Reused) => break,
                Ok(ProcessObservation::Matching) => {
                    pause(&deadline, &Cancellation::default()).unwrap();
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    // /proc/exe can disappear before stat reports a zombie.
                    pause(&deadline, &Cancellation::default())
                        .with_context(|| format!("orphan identity stayed unavailable: {error:#}"))
                        .unwrap();
                }
                Err(error) => panic!("cannot observe fixture process: {error:#}"),
            }
        }
    }

    #[test]
    fn docker_connect_requires_explicit_socket_and_nonempty_daemon_identity() {
        let dir = directory();
        let path = dir.path().join("d");
        let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let endpoint = format!("unix://{}", path.display());
        let mut candidate =
            Docker::at_endpoint(std::env::current_exe().unwrap(), &endpoint).unwrap();
        let expected = DockerBinding {
            endpoint: candidate.binding.endpoint.clone(),
            daemon_id: "daemon-a".to_owned(),
        };
        let (mut fake, _) = fake_docker_bound(expected.clone(), Vec::new());
        candidate.responder = fake.responder.take();
        let connected = candidate
            .finish_connect(
                &Deadline::new(Duration::from_secs(1)).unwrap(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(connected.binding(), &expected);
        assert!(
            Docker::connect(
                PathBuf::from("/usr/bin/false"),
                &endpoint,
                &Deadline::new(Duration::from_secs(1)).unwrap(),
                &Cancellation::default(),
            )
            .is_err()
        );
        assert!(
            Docker::at_endpoint(std::env::current_exe().unwrap(), "tcp://127.0.0.1:2375").is_err()
        );
        let (fake, state) = fake_docker(Vec::new());
        state.borrow_mut().daemon_id.clear();
        assert!(
            fake.finish_connect(
                &Deadline::new(Duration::from_secs(1)).unwrap(),
                &Cancellation::default()
            )
            .is_err()
        );
    }

    #[test]
    fn another_selected_docker_daemon_cannot_clear_existing_state() {
        let dir = directory();
        let lock = lock(dir.path());
        let mut record = state();
        dead_owner(&mut record);
        lock.write_state(None, &record).unwrap();
        let (other, fake) = fake_docker_bound(
            DockerBinding {
                endpoint: "unix:///fake/daemon-b.sock".to_owned(),
                daemon_id: "daemon-b".to_owned(),
            },
            Vec::new(),
        );
        let error = lock
            .recover(
                &other,
                IMAGE,
                &record.expected_model,
                Duration::from_secs(1),
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("daemon selection differs"));
        assert_eq!(lock.read_state().unwrap(), Some(record));
        assert!(fake.borrow().calls.is_empty());
    }

    #[test]
    fn empty_inventory_from_a_switched_daemon_preserves_state_and_owned_containers() {
        for switch_during_inventory in [false, true] {
            let dir = directory();
            let lock = lock(dir.path());
            let mut record = state();
            dead_owner(&mut record);
            lock.write_state(None, &record).unwrap();
            let records = vec![container(&record, 'a')];
            let (docker, fake) = fake_docker(records.clone());
            if switch_during_inventory {
                fake.borrow_mut().switch_to_empty_daemon_on_ps = Some("daemon-b".to_owned());
            } else {
                fake.borrow_mut().daemon_id = "daemon-b".to_owned();
            }
            let error = lock
                .recover(
                    &docker,
                    IMAGE,
                    &record.expected_model,
                    Duration::from_secs(1),
                    &Cancellation::default(),
                )
                .unwrap_err();
            assert!(format!("{error:#}").contains("daemon identity changed"));
            assert_eq!(lock.read_state().unwrap(), Some(record));
            assert_eq!(fake.borrow().records, records);
            assert!(fake.borrow().calls.iter().all(|call| call[0] != "rm"));
        }
    }

    #[test]
    fn launch_exports_the_recorded_docker_binding() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &model,
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        let mut budget = PortBudget::new(Duration::from_secs(10), 1).unwrap();
        session
            .launch_with_ports(
                &mut budget,
                Duration::from_secs(1),
                |_, _, _| {
                    let mut command = Command::new(&model);
                    command
                        .arg("30")
                        .env("DOCKER_HOST", "unix:///fake/daemon-b.sock")
                        .env("DOCKER_CONTEXT", "daemon-b")
                        .env("DOCKER_TLS_VERIFY", "1");
                    Ok(command)
                },
                |child, _| {
                    let environment = std::fs::read(format!("/proc/{}/environ", child.id()))?;
                    let entries: Vec<_> = environment.split(|byte| *byte == 0).collect();
                    assert!(
                        entries.contains(&b"DOCKER_HOST=unix:///fake/daemon-a.sock".as_slice())
                    );
                    assert!(!entries.iter().any(|entry| {
                        entry.starts_with(b"DOCKER_CONTEXT=")
                            || entry.starts_with(b"DOCKER_TLS_VERIFY=")
                    }));
                    Ok(LaunchConfirmation::Running)
                },
            )
            .unwrap();
        session.cleanup().unwrap();
    }

    #[test]
    fn endpoint_wire_rejects_unknown_schema_and_invalid_run_id() {
        let run = RunId::new().unwrap();
        let endpoint = Endpoint::new(run.clone(), 12345).unwrap();
        let original = serde_json::to_value(endpoint).unwrap();
        for (field, value) in [
            ("schema_version", serde_json::json!(2)),
            ("run_id", serde_json::json!("invalid")),
            ("address", serde_json::json!("192.0.2.1:12345")),
        ] {
            let mut wire = original.clone();
            wire[field] = value;
            assert!(Endpoint::parse(&serde_json::to_vec(&wire).unwrap(), &run, 12345).is_err());
        }
    }

    #[test]
    fn every_callback_persists_intent_and_only_success_clears_it() {
        for phase in [
            CallbackPhase::Preparation,
            CallbackPhase::Confirmation,
            CallbackPhase::ReadinessProbe,
            CallbackPhase::TestDispatch,
            CallbackPhase::ShutdownRequest,
        ] {
            for fail in [false, true] {
                let dir = directory();
                let (docker, _) = fake_docker(Vec::new());
                let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
                let mut session = Session::new(
                    lock(dir.path()),
                    docker,
                    IMAGE.to_owned(),
                    &model,
                    Duration::from_secs(10),
                    Cancellation::default(),
                )
                .unwrap();
                let state_path = session.lock.state_path();
                let observed = std::cell::Cell::new(false);
                let check_intent = || -> anyhow::Result<()> {
                    let record = RunState::parse(&std::fs::read(&state_path)?)?;
                    assert_eq!(record.callback_phase, phase);
                    observed.set(true);
                    anyhow::ensure!(!fail, "injected callback failure");
                    Ok(())
                };
                let mut budget = PortBudget::new(Duration::from_secs(10), 1).unwrap();
                let outcome = (|| -> anyhow::Result<()> {
                    session.launch_with_ports(
                        &mut budget,
                        Duration::from_secs(1),
                        |_, _, _| {
                            if phase == CallbackPhase::Preparation {
                                check_intent()?;
                            }
                            let mut command = Command::new(&model);
                            command.arg(if phase == CallbackPhase::ShutdownRequest && !fail {
                                "0.5"
                            } else {
                                "30"
                            });
                            Ok(command)
                        },
                        |_, _| {
                            if phase == CallbackPhase::Confirmation {
                                check_intent()?;
                            }
                            Ok(LaunchConfirmation::Running)
                        },
                    )?;
                    if matches!(
                        phase,
                        CallbackPhase::ReadinessProbe
                            | CallbackPhase::TestDispatch
                            | CallbackPhase::ShutdownRequest
                    ) {
                        session.wait_ready(Duration::from_secs(1), |_| {
                            if phase == CallbackPhase::ReadinessProbe {
                                check_intent()?;
                            }
                            Ok(true)
                        })?;
                    }
                    if matches!(
                        phase,
                        CallbackPhase::TestDispatch | CallbackPhase::ShutdownRequest
                    ) {
                        session.execute_test(Duration::from_secs(1), |_, _| {
                            if phase == CallbackPhase::TestDispatch {
                                check_intent()?;
                            }
                            Ok(())
                        })?;
                    }
                    if phase == CallbackPhase::ShutdownRequest {
                        let status =
                            session.shutdown(Duration::from_secs(1), |_| check_intent())?;
                        anyhow::ensure!(status.success(), "model fixture did not exit cleanly");
                    }
                    Ok(())
                })();
                assert!(observed.get(), "{phase:?}");
                let identity = session.state.model.clone();
                if fail {
                    assert!(outcome.is_err(), "{phase:?}");
                    assert_eq!(session.state.callback_phase, phase);
                    assert!(session.cleanup().is_err(), "{phase:?}");
                    assert_eq!(
                        session.lock.read_state().unwrap().unwrap().callback_phase,
                        phase
                    );
                } else {
                    outcome.unwrap();
                    assert_eq!(session.state.callback_phase, CallbackPhase::Idle);
                    session.cleanup().unwrap();
                    assert!(session.lock.read_state().unwrap().is_none());
                }
                if let Some(identity) = identity {
                    assert_eq!(identity.observe().unwrap(), ProcessObservation::Gone);
                }
            }
        }
    }

    #[test]
    fn callback_cancellation_and_failed_cleanup_preserve_intent() {
        let dir = directory();
        let (docker, daemon) = fake_docker(Vec::new());
        let cancellation = Cancellation::default();
        let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &model,
            Duration::from_secs(10),
            cancellation.clone(),
        )
        .unwrap();
        let mut budget = PortBudget::new(Duration::from_secs(10), 1).unwrap();
        session
            .launch_with_ports(
                &mut budget,
                Duration::from_secs(1),
                |_, _, _| {
                    let mut command = Command::new(&model);
                    command.arg("30");
                    Ok(command)
                },
                |_, _| Ok(LaunchConfirmation::Running),
            )
            .unwrap();
        let identity = session.state.model.clone().unwrap();
        assert!(
            session
                .wait_ready(Duration::from_secs(1), |_| {
                    cancellation.cancel();
                    anyhow::bail!("callback cancelled before helper cleanup completed")
                })
                .is_err()
        );
        let unresolved = session.state.clone();
        assert_eq!(unresolved.callback_phase, CallbackPhase::ReadinessProbe);
        daemon.borrow_mut().daemon_id = "daemon-b".to_owned();
        assert!(session.cleanup().is_err());
        assert_eq!(session.lock.read_state().unwrap(), Some(unresolved.clone()));
        assert_eq!(identity.observe().unwrap(), ProcessObservation::Gone);
        drop(session);
        assert_eq!(lock(dir.path()).read_state().unwrap(), Some(unresolved));
    }

    #[test]
    fn callback_panic_leaves_durable_intent_after_drop() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session.run_callback::<()>(
                CallbackPhase::Preparation,
                &Deadline::new(Duration::from_secs(1)).unwrap(),
                |_| panic!("injected callback panic"),
            )
        }));
        assert!(result.is_err());
        drop(session);
        assert_eq!(
            lock(dir.path())
                .read_state()
                .unwrap()
                .unwrap()
                .callback_phase,
            CallbackPhase::Preparation
        );
    }

    #[test]
    fn sigkill_inside_confirmation_preserves_intent_after_model_exit() {
        const CHILD: &str = "OPENVMM_FVP_CONFIRMATION_CRASH_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = Path::new(&root);
            let (docker, _) = fake_docker(Vec::new());
            let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
            let mut session = Session::new(
                lock(root),
                docker,
                IMAGE.to_owned(),
                &model,
                Duration::from_secs(10),
                Cancellation::default(),
            )
            .unwrap();
            let state_path = session.lock.state_path();
            let run_id = session.state.run_id.clone();
            let mut budget = PortBudget::new(Duration::from_secs(10), 1).unwrap();
            let result = session.launch_with_ports(
                &mut budget,
                Duration::from_secs(1),
                |_, _, _| {
                    let mut command = Command::new(&model);
                    command.arg("0.25");
                    Ok(command)
                },
                |_, deadline| {
                    let persisted = RunState::parse(&std::fs::read(&state_path)?)?;
                    anyhow::ensure!(
                        persisted.callback_phase == CallbackPhase::Confirmation
                            && !persisted.launch_pending
                            && persisted.model.is_some(),
                        "confirmation intent is missing after model registration"
                    );
                    let mut helper = ManagedChild::spawn(
                        Command::new(&model)
                            .arg("1")
                            .env("OPENVMM_FVP_RUN_ID", run_id.as_str()),
                    )?;
                    let identity = capture_owned_process(
                        &mut helper,
                        &model,
                        &run_id,
                        deadline,
                        &Cancellation::default(),
                    )?;
                    std::fs::write(root.join("helper.json"), serde_json::to_vec(&identity)?)?;
                    signal_hook::low_level::raise(signal_hook::consts::SIGKILL)?;
                    anyhow::bail!("SIGKILL did not terminate the confirmation owner")
                },
            );
            panic!("confirmation owner survived: {result:?}");
        }
        let dir = directory();
        let deadline = Deadline::new(Duration::from_secs(3)).unwrap();
        let output = run_command_with_cleanup(
            Command::new(std::env::current_exe().unwrap())
                .arg("sigkill_inside_confirmation_preserves_intent_after_model_exit")
                .env(CHILD, dir.path()),
            &deadline,
            &deadline,
        )
        .unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&output.status),
            Some(signal_hook::consts::SIGKILL),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let lock = lock(dir.path());
        let record = lock.read_state().unwrap().unwrap();
        assert_eq!(record.callback_phase, CallbackPhase::Confirmation);
        let model = record.model.as_ref().unwrap();
        let deadline = Deadline::new(Duration::from_secs(2)).unwrap();
        while model.observe().unwrap() == ProcessObservation::Matching {
            pause(&deadline, &Cancellation::default()).unwrap();
        }
        let helper: ProcessIdentity =
            serde_json::from_slice(&std::fs::read(dir.path().join("helper.json")).unwrap())
                .unwrap();
        assert_eq!(helper.observe().unwrap(), ProcessObservation::Matching);
        let containers = vec![container(&record, 'a')];
        let (docker, fake) = fake_docker(containers.clone());
        let original_bytes = std::fs::read(lock.state_path()).unwrap();
        let error = lock
            .recover(
                &docker,
                IMAGE,
                &record.expected_model,
                Duration::from_secs(1),
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("Confirmation callback"));
        assert_eq!(std::fs::read(lock.state_path()).unwrap(), original_bytes);
        assert_eq!(lock.read_state().unwrap(), Some(record));
        assert_eq!(fake.borrow().records, containers);
        assert!(fake.borrow().calls.is_empty());
        let deadline = Deadline::new(Duration::from_secs(2)).unwrap();
        while helper.observe().unwrap() == ProcessObservation::Matching {
            pause(&deadline, &Cancellation::default()).unwrap();
        }
    }

    #[test]
    fn session_owns_signals_through_cleanup_and_drop() {
        const CHILD: &str = "OPENVMM_FVP_SESSION_SIGNAL_OWNER_CHILD";
        if let Some(mode) = std::env::var_os(CHILD) {
            let dir = directory();
            let cancellation = Cancellation::default();
            let (mut docker, _) = fake_docker(Vec::new());
            let responder = docker.responder.take().unwrap();
            let observed = Rc::new(std::cell::Cell::new(false));
            let signal_observed = observed.clone();
            let token = cancellation.clone();
            docker.responder = Some(Box::new(move |command, args| {
                if !signal_observed.replace(true) {
                    signal_hook::low_level::raise(signal_hook::consts::SIGTERM)?;
                    assert!(token.check().is_err());
                }
                responder(command, args)
            }));
            let mut session = Session::new(
                lock(dir.path()),
                docker,
                IMAGE.to_owned(),
                &std::env::current_exe().unwrap(),
                Duration::from_secs(10),
                cancellation.clone(),
            )
            .unwrap();
            // This guard is declared later and therefore can drop first.
            let external = cancellation.install_signal_handlers().unwrap();
            drop(external);
            if mode == "explicit" {
                session.cleanup().unwrap();
            }
            drop(session);
            assert!(observed.get());
            assert!(cancellation.check().is_err());
            assert!(lock(dir.path()).read_state().unwrap().is_none());
            return;
        }
        for mode in ["explicit", "drop"] {
            let deadline = Deadline::new(Duration::from_secs(3)).unwrap();
            let output = run_command_with_cleanup(
                Command::new(std::env::current_exe().unwrap())
                    .arg("session_owns_signals_through_cleanup_and_drop")
                    .env(CHILD, mode),
                &deadline,
                &deadline,
            )
            .unwrap();
            assert!(
                output.status.success(),
                "{mode}: {}; {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
    }

    #[test]
    fn confirmed_readiness_can_shutdown_without_dispatching_a_test() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &model,
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        let mut budget = PortBudget::new(Duration::from_secs(10), 1).unwrap();
        session
            .launch_with_ports(
                &mut budget,
                Duration::from_secs(1),
                |_, _, _| {
                    let mut command = Command::new(&model);
                    command.arg("0.25");
                    Ok(command)
                },
                |_, _| Ok(LaunchConfirmation::Running),
            )
            .unwrap();
        session
            .wait_ready(Duration::from_secs(1), |_| Ok(true))
            .unwrap();
        assert!(
            session
                .shutdown(Duration::from_secs(1), |_| Ok(()))
                .unwrap()
                .success()
        );
        session.cleanup().unwrap();
        assert!(session.lock.read_state().unwrap().is_none());
    }

    #[test]
    fn readiness_timeout_keeps_one_durable_intent_for_the_whole_loop() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &model,
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        let mut budget = PortBudget::new(Duration::from_secs(10), 1).unwrap();
        session
            .launch_with_ports(
                &mut budget,
                Duration::from_secs(1),
                |_, _, _| {
                    let mut command = Command::new(&model);
                    command.arg("30");
                    Ok(command)
                },
                |_, _| Ok(LaunchConfirmation::Running),
            )
            .unwrap();
        let state_path = session.lock.state_path();
        let mut held: Option<File> = None;
        let mut probes = 0;
        let error = session
            .wait_ready(Duration::from_millis(150), |_| {
                let current = File::open(&state_path)?;
                if let Some(first) = &held {
                    assert_eq!(first.metadata()?.ino(), current.metadata()?.ino());
                } else {
                    // Retain the inode so repeated replacements cannot reuse it.
                    held = Some(current);
                }
                let record = RunState::parse(&std::fs::read(&state_path)?)?;
                assert_eq!(record.callback_phase, CallbackPhase::ReadinessProbe);
                probes += 1;
                Ok(false)
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("deadline"));
        assert!(probes >= 2);
        assert_eq!(session.state.callback_phase, CallbackPhase::Idle);
        assert_ne!(
            held.as_ref().unwrap().metadata().unwrap().ino(),
            std::fs::metadata(&state_path).unwrap().ino()
        );
        session.cleanup().unwrap();
        assert!(session.lock.read_state().unwrap().is_none());
    }

    #[test]
    fn completed_callbacks_clear_intent_before_reporting_cancel_or_timeout() {
        for readiness in [false, true] {
            for cancel in [false, true] {
                let dir = directory();
                let (docker, _) = fake_docker(Vec::new());
                let cancellation = Cancellation::default();
                let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
                let mut session = Session::new(
                    lock(dir.path()),
                    docker,
                    IMAGE.to_owned(),
                    &model,
                    Duration::from_secs(10),
                    cancellation.clone(),
                )
                .unwrap();
                let mut budget = PortBudget::new(Duration::from_secs(10), 1).unwrap();
                session
                    .launch_with_ports(
                        &mut budget,
                        Duration::from_secs(1),
                        |_, _, _| {
                            let mut command = Command::new(&model);
                            command.arg("30");
                            Ok(command)
                        },
                        |_, _| Ok(LaunchConfirmation::Running),
                    )
                    .unwrap();
                let finish = || {
                    if cancel {
                        cancellation.cancel();
                    } else {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                };
                let timeout = if cancel {
                    Duration::from_secs(1)
                } else {
                    Duration::from_millis(30)
                };
                let error = if readiness {
                    session
                        .wait_ready(timeout, |_| {
                            finish();
                            Ok(true)
                        })
                        .unwrap_err()
                } else {
                    session
                        .wait_ready(Duration::from_secs(1), |_| Ok(true))
                        .unwrap();
                    session
                        .execute_test(timeout, |_, _| {
                            finish();
                            Ok(())
                        })
                        .unwrap_err()
                };
                assert!(format!("{error:#}").contains(if cancel {
                    "cancelled"
                } else {
                    "deadline"
                }));
                assert_eq!(session.state.callback_phase, CallbackPhase::Idle);
                assert_eq!(
                    session.lock.read_state().unwrap().unwrap().callback_phase,
                    CallbackPhase::Idle
                );
                session.cleanup().unwrap();
                assert!(session.lock.read_state().unwrap().is_none());
            }
        }
    }

    #[test]
    fn runtime_default_uses_persistent_home_not_volatile_xdg_storage() {
        const CHILD: &str = "OPENVMM_FVP_PERSISTENT_RUNTIME_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            let volatile = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
            let base = RuntimeDirectory::default_base().unwrap();
            assert_eq!(base, home.join(".local/state/openvmm"));
            assert!(!base.starts_with(volatile));
            return;
        }
        let dir = directory();
        let deadline = Deadline::new(Duration::from_secs(3)).unwrap();
        let output = run_command_with_cleanup(
            Command::new(std::env::current_exe().unwrap())
                .arg("runtime_default_uses_persistent_home_not_volatile_xdg_storage")
                .env(CHILD, "1")
                .env("HOME", dir.path().join("persistent"))
                .env("XDG_RUNTIME_DIR", dir.path().join("volatile")),
            &deadline,
            &deadline,
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[test]
    fn reboot_recovery_does_not_probe_or_stop_reused_new_boot_process_groups() {
        let dir = directory();
        let lock = lock(dir.path());
        let model = canonical(Path::new("/usr/bin/sleep")).unwrap();
        let foreign_run = RunId::new().unwrap();
        let mut foreign = ManagedChild::spawn(
            Command::new(&model)
                .arg("30")
                .env("OPENVMM_FVP_RUN_ID", foreign_run.as_str()),
        )
        .unwrap();
        let identity = capture_owned_process(
            &mut foreign,
            &model,
            &foreign_run,
            &Deadline::new(Duration::from_secs(1)).unwrap(),
            &Cancellation::default(),
        )
        .unwrap();
        let mut record = state();
        let mut old_boot = current_boot_id().unwrap();
        let replacement = if old_boot.starts_with('0') { "1" } else { "0" };
        old_boot.replace_range(..1, replacement);
        record.owner.boot_id = old_boot.clone();
        record.expected_model = model;
        record.model = Some(ProcessIdentity {
            boot_id: old_boot,
            run_id: Some(record.run_id.clone()),
            ..identity.clone()
        });
        assert_eq!(record.owner.observe().unwrap(), ProcessObservation::Gone);
        assert_eq!(
            record.model.as_ref().unwrap().observe().unwrap(),
            ProcessObservation::Gone
        );
        lock.write_state(None, &record).unwrap();
        let (docker, fake) = fake_docker(Vec::new());
        lock.recover(
            &docker,
            IMAGE,
            &record.expected_model,
            Duration::from_secs(1),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(lock.read_state().unwrap().is_none());
        assert!(fake.borrow().calls.iter().all(|call| call[0] != "rm"));
        assert!(foreign.try_wait().unwrap().is_none());
        assert_eq!(identity.observe().unwrap(), ProcessObservation::Matching);
        let mut malformed = identity;
        malformed.boot_id = "not-a-boot-id".to_owned();
        assert!(malformed.observe().is_err());
        foreign
            .terminate(&Deadline::new(Duration::from_secs(1)).unwrap())
            .unwrap();
    }

    #[test]
    fn session_docker_command_uses_the_pinned_executable_and_endpoint() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        let command = session.docker_command();
        assert_eq!(command.get_program(), "never-executed");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--host", "unix:///fake/daemon-a.sock"]);
        session.cleanup().unwrap();
    }

    #[test]
    fn split_cleanup_verifies_even_after_cancellation_and_retains_until_retirement() {
        let dir = directory();
        let cancellation = Cancellation::default();
        let (docker, _) = fake_docker(Vec::new());
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            cancellation.clone(),
        )
        .unwrap();
        cancellation.cancel();
        session.stop_resources().unwrap();
        assert!(session.lock.read_state().unwrap().is_some());
        assert_eq!(session.state.post_verification, PostVerification::Pending);
        assert!(session.retire_state().is_err());
        assert!(session.retirement_deadline.is_none());
        let state_path = session.lock.state_path();
        let result: anyhow::Result<()> = session
            .post_verify(
                &Deadline::new(Duration::from_secs(5)).unwrap(),
                |_, token| {
                    token.check()?;
                    let record = RunState::parse(&std::fs::read(&state_path)?)?;
                    assert_eq!(record.post_verification, PostVerification::InFlight);
                    Ok(Err(anyhow::anyhow!("injected toolchain drift")))
                },
            )
            .unwrap();
        assert!(result.is_err());
        assert_eq!(session.state.post_verification, PostVerification::Failed);
        assert!(session.lock.read_state().unwrap().is_some());
        session.retire_state().unwrap();
        assert!(session.lock.read_state().unwrap().is_none());
    }

    #[test]
    fn post_verification_does_not_erase_an_earlier_unresolved_callback() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let cancellation = Cancellation::default();
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            cancellation.clone(),
        )
        .unwrap();
        assert!(
            session
                .prepare_inputs::<()>(&Deadline::new(Duration::from_secs(1)).unwrap(), |_, _| {
                    anyhow::bail!("injected unresolved helper failure")
                })
                .is_err()
        );
        cancellation.cancel();
        session.stop_resources().unwrap();
        session
            .post_verify(
                &Deadline::new(Duration::from_secs(5)).unwrap(),
                |_, token| {
                    token.check()?;
                    Ok(Ok(()))
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(session.state.callback_phase, CallbackPhase::Preparation);
        assert!(session.retire_state().is_err());
        drop(session);
        let retained = lock(dir.path()).read_state().unwrap().unwrap();
        assert_eq!(retained.callback_phase, CallbackPhase::Preparation);
        assert_eq!(retained.post_verification, PostVerification::Complete);
    }

    #[test]
    fn runtime_finalization_matrix_uses_registered_staging() {
        use super::super::runtime::finish_session;
        use super::super::staging::PreparedFvpRun;

        for (case, diagnostic) in [
            ("ok", None),
            ("drift", Some("toolchain drift")),
            ("late", Some("deadline")),
            ("expired", Some("deadline")),
            ("cancelled", None),
            ("output-symlink", Some("symlink")),
            ("output-root-replaced", Some("symlink")),
            ("output-ancestor-replaced", Some("symlink")),
            ("unresolved", Some("unresolved verification helper")),
        ] {
            let dir = directory();
            let (docker, fake) = fake_docker(Vec::new());
            let cancellation = Cancellation::default();
            let mut session = Session::new(
                lock(dir.path()),
                docker,
                IMAGE.to_owned(),
                &std::env::current_exe().unwrap(),
                Duration::from_secs(10),
                cancellation.clone(),
            )
            .unwrap();
            let prepared = PreparedFvpRun::allocate(
                dir.path(),
                &Deadline::new(Duration::from_secs(5)).unwrap(),
            )
            .unwrap();
            let workspace = prepared.root().to_owned();
            let output_parent = dir.path().join("output-parent");
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&output_parent)
                .unwrap();
            let output = output_parent.join("run");
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&output)
                .unwrap();
            let deadline = Deadline::new(Duration::from_secs(5)).unwrap();
            session
                .register_workspace_with_output(&workspace, &output, &deadline)
                .unwrap();
            prepared
                .set_run_identity(session.state().run_id().as_str())
                .unwrap();
            std::fs::write(prepared.logs().join("host.log"), b"retained console").unwrap();
            std::fs::create_dir(prepared.share().join("test_results")).unwrap();
            std::fs::write(
                prepared.share().join("test_results/result"),
                b"guest result",
            )
            .unwrap();
            let foreign = dir.path().join("foreign");
            std::fs::write(&foreign, b"untouched").unwrap();
            let foreign_directory = dir.path().join("foreign-output");
            std::fs::create_dir(&foreign_directory).unwrap();
            std::fs::write(foreign_directory.join("sentinel"), b"untouched").unwrap();
            if case == "output-symlink" {
                std::os::unix::fs::symlink(&foreign, prepared.share().join("test_results/alias"))
                    .unwrap();
            }
            if case == "cancelled" {
                cancellation.cancel();
            }
            if case == "output-root-replaced" {
                std::fs::rename(&output, dir.path().join("preserved-output")).unwrap();
                std::os::unix::fs::symlink(&foreign_directory, &output).unwrap();
            } else if case == "output-ancestor-replaced" {
                std::fs::create_dir(foreign_directory.join("run")).unwrap();
                std::fs::rename(&output_parent, dir.path().join("preserved-output-parent"))
                    .unwrap();
                std::os::unix::fs::symlink(&foreign_directory, &output_parent).unwrap();
            }
            let budget = match case {
                "expired" => Duration::ZERO,
                "late" => Duration::from_millis(20),
                _ => Duration::from_secs(5),
            };
            let lock_inode = session.lock.file.metadata().unwrap().ino();
            let started = std::time::Instant::now();
            let result = finish_session(
                &mut session,
                Some(prepared),
                &output,
                budget,
                Duration::from_secs(5),
                |_, token| {
                    assert_ne!(case, "expired");
                    token.check().unwrap();
                    if case == "cancelled" {
                        cancellation.cancel();
                        token.check().unwrap();
                    }
                    match case {
                        "drift" => Ok(Err(anyhow::anyhow!("toolchain drift"))),
                        "unresolved" => Err(anyhow::anyhow!("unresolved verification helper")),
                        "late" => {
                            std::thread::sleep(Duration::from_millis(30));
                            Ok(Ok(()))
                        }
                        _ => Ok(Ok(())),
                    }
                },
            );
            assert_eq!(
                result.is_ok(),
                matches!(case, "ok" | "cancelled"),
                "{case}: {result:?}"
            );
            assert!(started.elapsed() < Duration::from_secs(31), "{case}");
            if let Some(diagnostic) = diagnostic {
                assert!(
                    format!("{:#}", result.as_ref().unwrap_err()).contains(diagnostic),
                    "{case}: {result:?}"
                );
            }
            let replaced = matches!(case, "output-root-replaced" | "output-ancestor-replaced");
            let retained = replaced || matches!(case, "output-symlink" | "unresolved");
            assert_eq!(workspace.exists(), retained, "{case}");
            assert_eq!(
                session.lock.read_state().unwrap().is_some(),
                retained,
                "{case}"
            );
            assert_eq!(std::fs::read(&foreign).unwrap(), b"untouched", "{case}");
            if !replaced {
                assert_eq!(
                    std::fs::read(output.join("consoles/host.log")).unwrap(),
                    b"retained console"
                );
            }
            assert_eq!(
                std::fs::read(foreign_directory.join("sentinel")).unwrap(),
                b"untouched"
            );
            assert_eq!(
                std::fs::read_dir(&foreign_directory).unwrap().count(),
                if case == "output-ancestor-replaced" {
                    2
                } else {
                    1
                },
                "{case}",
            );
            if case == "output-ancestor-replaced" {
                assert_eq!(
                    std::fs::read_dir(foreign_directory.join("run"))
                        .unwrap()
                        .count(),
                    0
                );
            }
            if !retained {
                assert_eq!(
                    std::fs::read(output.join("test_results/result")).unwrap(),
                    b"guest result"
                );
            }
            assert!(fake.borrow().records.is_empty(), "{case}");
            drop(session);
            let released = lock(dir.path());
            assert_eq!(
                released.file.metadata().unwrap().ino(),
                lock_inode,
                "{case}"
            );
        }
    }

    #[test]
    fn rejected_or_cancelled_registration_allows_a_subsequent_valid_session() {
        use super::super::runtime::finish_session;
        use super::super::staging::PreparedFvpRun;

        for cancelled in [false, true] {
            let dir = directory();
            let (docker, _) = fake_docker(Vec::new());
            let cancellation = Cancellation::default();
            let mut session = Session::new(
                lock(dir.path()),
                docker,
                IMAGE.to_owned(),
                &std::env::current_exe().unwrap(),
                Duration::from_secs(10),
                cancellation.clone(),
            )
            .unwrap();
            let staging = PreparedFvpRun::allocate(
                dir.path(),
                &Deadline::new(Duration::from_secs(5)).unwrap(),
            )
            .unwrap();
            let path = staging.root().to_owned();
            let volatile = tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in("/tmp")
                .unwrap();
            if cancelled {
                cancellation.cancel();
            }
            let error = session
                .register_workspace_with_output(
                    staging.root(),
                    volatile.path(),
                    &Deadline::new(Duration::from_secs(5)).unwrap(),
                )
                .unwrap_err();
            assert!(format!("{error:#}").contains(if cancelled {
                "cancelled"
            } else {
                "persistent"
            }));
            assert!(session.state.workspace.is_none());
            assert!(
                session
                    .lock
                    .read_state()
                    .unwrap()
                    .unwrap()
                    .workspace
                    .is_none()
            );
            staging
                .cleanup_unregistered(&Deadline::new(Duration::from_secs(5)).unwrap())
                .unwrap();
            assert!(!path.exists());
            finish_session(
                &mut session,
                None,
                volatile.path(),
                Duration::from_secs(5),
                Duration::from_secs(5),
                |_, token| {
                    token.check()?;
                    Ok(Ok(()))
                },
            )
            .unwrap();
            drop(session);

            let (docker, _) = fake_docker(Vec::new());
            let mut next = Session::new(
                lock(dir.path()),
                docker,
                IMAGE.to_owned(),
                &std::env::current_exe().unwrap(),
                Duration::from_secs(10),
                Cancellation::default(),
            )
            .unwrap();
            let staging = PreparedFvpRun::allocate(
                dir.path(),
                &Deadline::new(Duration::from_secs(5)).unwrap(),
            )
            .unwrap();
            let path = staging.root().to_owned();
            let output = dir.path().join("valid-output");
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&output)
                .unwrap();
            next.register_workspace_with_output(
                &path,
                &output,
                &Deadline::new(Duration::from_secs(5)).unwrap(),
            )
            .unwrap();
            std::fs::write(staging.logs().join("proof"), b"valid session").unwrap();
            finish_session(
                &mut next,
                Some(staging),
                &output,
                Duration::from_secs(5),
                Duration::from_secs(5),
                |_, _| Ok(Ok(())),
            )
            .unwrap();
            assert!(!path.exists());
            assert!(next.lock.read_state().unwrap().is_none());
            assert_eq!(
                std::fs::read(output.join("consoles/proof")).unwrap(),
                b"valid session"
            );
        }
    }

    #[test]
    fn rejected_registration_never_discards_replacement_or_unexpected_data() {
        use super::super::staging::PreparedFvpRun;

        for replacement in [false, true] {
            let dir = directory();
            let (docker, _) = fake_docker(Vec::new());
            let mut session = Session::new(
                lock(dir.path()),
                docker,
                IMAGE.to_owned(),
                &std::env::current_exe().unwrap(),
                Duration::from_secs(10),
                Cancellation::default(),
            )
            .unwrap();
            let deadline = Deadline::new(Duration::from_secs(5)).unwrap();
            let staging = PreparedFvpRun::allocate(dir.path(), &deadline).unwrap();
            let path = staging.root().to_owned();
            let aside = dir.path().join("original-allocation");
            if replacement {
                std::fs::rename(&path, &aside).unwrap();
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&path)
                    .unwrap();
            }
            let sentinel = path.join("sentinel");
            std::fs::write(&sentinel, b"foreign data").unwrap();
            let output = dir.path().join("output");
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&output)
                .unwrap();
            assert!(
                session
                    .register_workspace_with_output(&path, &output, &deadline)
                    .is_err()
            );
            assert!(session.state.workspace.is_none());
            assert!(staging.cleanup_unregistered(&deadline).is_err());
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"foreign data");
            assert_eq!(
                std::fs::read_dir(&path).unwrap().count(),
                if replacement { 1 } else { 7 }
            );
            if replacement {
                assert_eq!(std::fs::read_dir(&aside).unwrap().count(), 6);
            }
            session.cleanup().unwrap();
        }
    }

    #[test]
    fn owned_workspace_retirement_preserves_durable_outputs_and_lock_inode() {
        let dir = directory();
        let (mut session, workspace, output) = sealed_workspace(dir.path());
        let inode = session.lock.file.metadata().unwrap().ino();
        assert_eq!(session.state.workspace_path(), Some(workspace.as_path()));
        session.retire_state().unwrap();
        assert!(!workspace.exists());
        assert_eq!(
            std::fs::read(output.join("model.log")).unwrap(),
            b"retained log"
        );
        assert!(session.lock.read_state().unwrap().is_none());
        assert_eq!(session.lock.file.metadata().unwrap().ino(), inode);
    }

    #[test]
    fn recovery_preserves_registered_outputs_before_retiring_an_unverified_run() {
        let dir = directory();
        let (mut session, workspace) = registered_workspace(dir.path());
        let output = dir.path().join("output");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&output)
            .unwrap();
        session
            .register_output_destination(&output, &Deadline::new(Duration::from_secs(5)).unwrap())
            .unwrap();
        session.stop_resources().unwrap();
        drop(session);
        let lock = lock(dir.path());
        let original = lock.read_state().unwrap().unwrap();
        assert_eq!(original.post_verification, PostVerification::Pending);
        let mut stale = original.clone();
        dead_owner(&mut stale);
        lock.write_state(Some(&original), &stale).unwrap();
        let (docker, _) = fake_docker(Vec::new());
        lock.recover(
            &docker,
            IMAGE,
            &stale.expected_model,
            Duration::from_secs(5),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(!workspace.exists());
        assert!(lock.read_state().unwrap().is_none());
        assert_eq!(
            std::fs::read(output.join("consoles/model.log")).unwrap(),
            b"retained log"
        );
    }

    #[test]
    fn interrupted_workspace_removal_preserves_the_remaining_tree() {
        let dir = directory();
        let (mut session, workspace, output) = sealed_workspace(dir.path());
        let mut interrupted = session.state.clone();
        interrupted.workspace.as_mut().unwrap().removal_started = true;
        session
            .lock
            .write_state(Some(&session.state), &interrupted)
            .unwrap();
        session.state = interrupted;
        std::fs::remove_file(workspace.join("logs/model.log")).unwrap();
        drop(session);
        let lock = lock(dir.path());
        let original = lock.read_state().unwrap().unwrap();
        let mut stale = original.clone();
        dead_owner(&mut stale);
        lock.write_state(Some(&original), &stale).unwrap();
        let (docker, _) = fake_docker(Vec::new());
        let error = lock
            .recover(
                &docker,
                IMAGE,
                &stale.expected_model,
                Duration::from_secs(5),
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"));
        assert_eq!(lock.read_state().unwrap(), Some(stale));
        assert_eq!(
            std::fs::read(output.join("model.log")).unwrap(),
            b"retained log"
        );
        assert!(workspace.join("logs").is_dir());
    }

    #[test]
    fn retirement_can_resume_when_the_complete_sealed_tree_is_unchanged() {
        let dir = directory();
        let (mut session, workspace, _) = sealed_workspace(dir.path());
        let mut interrupted = session.state.clone();
        interrupted.workspace.as_mut().unwrap().removal_started = true;
        session
            .lock
            .write_state(Some(&session.state), &interrupted)
            .unwrap();
        session.state = interrupted;
        drop(session);
        let lock = lock(dir.path());
        let original = lock.read_state().unwrap().unwrap();
        let mut stale = original.clone();
        dead_owner(&mut stale);
        lock.write_state(Some(&original), &stale).unwrap();
        let (docker, _) = fake_docker(Vec::new());
        lock.recover(
            &docker,
            IMAGE,
            &stale.expected_model,
            Duration::from_secs(5),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(!workspace.exists());
        assert!(lock.read_state().unwrap().is_none());
    }

    #[test]
    fn recovery_removes_only_a_verified_workspace_with_preserved_outputs() {
        let dir = directory();
        let (session, workspace, output) = sealed_workspace(dir.path());
        drop(session);
        let lock = lock(dir.path());
        let original = lock.read_state().unwrap().unwrap();
        let mut stale = original.clone();
        dead_owner(&mut stale);
        lock.write_state(Some(&original), &stale).unwrap();
        let (docker, _) = fake_docker(Vec::new());
        lock.recover(
            &docker,
            IMAGE,
            &stale.expected_model,
            Duration::from_secs(5),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(!workspace.exists());
        assert_eq!(
            std::fs::read(output.join("model.log")).unwrap(),
            b"retained log"
        );
        assert!(lock.read_state().unwrap().is_none());
    }

    #[test]
    fn workspace_without_preserved_outputs_is_retained_with_state() {
        let dir = directory();
        let (mut session, workspace) = registered_workspace(dir.path());
        session.stop_resources().unwrap();
        session
            .post_verify(&Deadline::new(Duration::from_secs(5)).unwrap(), |_, _| {
                Ok(Ok(()))
            })
            .unwrap()
            .unwrap();
        let error = session.retire_state().unwrap_err();
        assert!(format!("{error:#}").contains("not durably preserved"));
        assert!(format!("{error:#}").contains(&workspace.display().to_string()));
        drop(session);
        let lock = lock(dir.path());
        let original = lock.read_state().unwrap().unwrap();
        let mut stale = original.clone();
        dead_owner(&mut stale);
        lock.write_state(Some(&original), &stale).unwrap();
        let (docker, _) = fake_docker(Vec::new());
        assert!(
            lock.recover(
                &docker,
                IMAGE,
                &stale.expected_model,
                Duration::from_secs(5),
                &Cancellation::default()
            )
            .is_err()
        );
        assert_eq!(lock.read_state().unwrap(), Some(stale));
        assert_eq!(
            std::fs::read(workspace.join("logs/model.log")).unwrap(),
            b"retained log"
        );
    }

    #[test]
    fn workspace_mutation_matrix_preserves_foreign_or_unarchived_data() {
        for mutation in [
            "newer-marker",
            "foreign-marker",
            "source",
            "output",
            "symlink",
            "replacement",
            "parent",
        ] {
            let dir = directory();
            let (mut session, workspace, output) = sealed_workspace(dir.path());
            let original_state = std::fs::read(session.lock.state_path()).unwrap();
            let foreign = dir.path().join("foreign");
            std::fs::create_dir(&foreign).unwrap();
            std::fs::write(foreign.join("keep"), b"foreign").unwrap();
            let mut original_workspace = workspace.clone();
            match mutation {
                "newer-marker" | "foreign-marker" => {
                    let path = workspace.join(WORKSPACE_MARKER);
                    let mut marker: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    if mutation == "newer-marker" {
                        marker["schema_version"] = serde_json::json!(2);
                    } else {
                        marker["run_id"] = serde_json::json!(RunId::new().unwrap().as_str());
                    }
                    std::fs::write(path, serde_json::to_vec(&marker).unwrap()).unwrap();
                }
                "source" => {
                    std::fs::write(workspace.join("logs/model.log"), b"unarchived log").unwrap();
                }
                "output" => {
                    std::fs::write(output.join("model.log"), b"changed output").unwrap();
                }
                "symlink" => {
                    std::os::unix::fs::symlink(&foreign, workspace.join("foreign-link")).unwrap();
                }
                "replacement" => {
                    original_workspace = workspace.parent().unwrap().join("original");
                    std::fs::rename(&workspace, &original_workspace).unwrap();
                    std::fs::DirBuilder::new()
                        .mode(0o700)
                        .create(&workspace)
                        .unwrap();
                    std::fs::write(workspace.join("keep"), b"replacement").unwrap();
                }
                "parent" => {
                    let parent = workspace.parent().unwrap();
                    let moved = dir.path().join("moved-parent");
                    std::fs::rename(parent, &moved).unwrap();
                    std::os::unix::fs::symlink(&moved, parent).unwrap();
                    original_workspace = moved.join(workspace.file_name().unwrap());
                }
                _ => unreachable!(),
            }
            assert!(session.retire_state().is_err(), "{mutation}");
            assert_eq!(
                std::fs::read(session.lock.state_path()).unwrap(),
                original_state,
                "{mutation}"
            );
            assert!(
                original_workspace.join("logs/model.log").is_file(),
                "{mutation}"
            );
            assert_eq!(std::fs::read(foreign.join("keep")).unwrap(), b"foreign");
            if mutation == "source" {
                assert_eq!(
                    std::fs::read(workspace.join("logs/model.log")).unwrap(),
                    b"unarchived log"
                );
            }
            if mutation == "replacement" {
                assert_eq!(
                    std::fs::read(workspace.join("keep")).unwrap(),
                    b"replacement"
                );
            }
        }
    }

    #[test]
    fn workspace_registration_rejects_existing_files_and_unsafe_roots() {
        let dir = directory();
        let (docker, _) = fake_docker(Vec::new());
        let mut session = Session::new(
            lock(dir.path()),
            docker,
            IMAGE.to_owned(),
            &std::env::current_exe().unwrap(),
            Duration::from_secs(10),
            Cancellation::default(),
        )
        .unwrap();
        let workspace = tempfile::Builder::new()
            .prefix("openvmm-fvp-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(dir.path())
            .unwrap();
        std::fs::write(workspace.path().join("existing"), b"preserve").unwrap();
        assert!(
            session
                .register_workspace(
                    workspace.path(),
                    &Deadline::new(Duration::from_secs(1)).unwrap()
                )
                .is_err()
        );
        assert!(
            session
                .register_workspace(dir.path(), &Deadline::new(Duration::from_secs(1)).unwrap())
                .is_err()
        );
        assert!(!workspace.path().join(WORKSPACE_MARKER).exists());
        assert_eq!(
            std::fs::read(workspace.path().join("existing")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn foreign_or_newer_state_prevents_workspace_retirement() {
        for newer in [false, true] {
            let dir = directory();
            let (mut session, workspace, _) = sealed_workspace(dir.path());
            let bytes = if newer {
                let mut state = serde_json::to_value(&session.state).unwrap();
                state["schema_version"] = serde_json::json!(2);
                serde_json::to_vec(&state).unwrap()
            } else {
                b"{foreign or corrupt state".to_vec()
            };
            std::fs::write(session.lock.state_path(), &bytes).unwrap();
            assert!(session.retire_state().is_err());
            assert_eq!(std::fs::read(session.lock.state_path()).unwrap(), bytes);
            assert_eq!(
                std::fs::read(workspace.join("logs/model.log")).unwrap(),
                b"retained log"
            );
        }
    }

    #[test]
    fn legacy_state_without_workspace_declaration_is_preserved() {
        let dir = directory();
        let lock = lock(dir.path());
        let record = state();
        lock.write_state(None, &record).unwrap();
        let mut legacy = serde_json::to_value(record).unwrap();
        legacy.as_object_mut().unwrap().remove("workspace");
        let bytes = serde_json::to_vec(&legacy).unwrap();
        std::fs::write(lock.state_path(), &bytes).unwrap();
        assert!(lock.read_state().is_err());
        assert_eq!(std::fs::read(lock.state_path()).unwrap(), bytes);
    }

    #[test]
    fn recovery_preserves_a_newer_workspace_marker() {
        let dir = directory();
        let (session, workspace, _) = sealed_workspace(dir.path());
        let marker_path = workspace.join(WORKSPACE_MARKER);
        let mut marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
        marker["schema_version"] = serde_json::json!(2);
        let marker_bytes = serde_json::to_vec(&marker).unwrap();
        std::fs::write(&marker_path, &marker_bytes).unwrap();
        drop(session);
        let lock = lock(dir.path());
        let original = lock.read_state().unwrap().unwrap();
        let mut stale = original.clone();
        dead_owner(&mut stale);
        lock.write_state(Some(&original), &stale).unwrap();
        let (docker, _) = fake_docker(Vec::new());
        assert!(
            lock.recover(
                &docker,
                IMAGE,
                &stale.expected_model,
                Duration::from_secs(5),
                &Cancellation::default()
            )
            .is_err()
        );
        assert_eq!(lock.read_state().unwrap(), Some(stale));
        assert_eq!(std::fs::read(marker_path).unwrap(), marker_bytes);
        assert_eq!(
            std::fs::read(workspace.join("logs/model.log")).unwrap(),
            b"retained log"
        );
    }

    #[test]
    fn recovery_finishes_retirement_when_the_verified_workspace_is_already_removed() {
        let dir = directory();
        let (mut session, workspace, output) = sealed_workspace(dir.path());
        let registered = session.state.workspace.clone().unwrap();
        let mut retiring = session.state.clone();
        retiring.workspace.as_mut().unwrap().removal_started = true;
        session
            .lock
            .write_state(Some(&session.state), &retiring)
            .unwrap();
        session.state = retiring;
        registered
            .remove(&Deadline::new(Duration::from_secs(5)).unwrap())
            .unwrap();
        assert!(!workspace.exists());
        drop(session);
        let lock = lock(dir.path());
        let original = lock.read_state().unwrap().unwrap();
        let mut stale = original.clone();
        dead_owner(&mut stale);
        lock.write_state(Some(&original), &stale).unwrap();
        let (docker, _) = fake_docker(Vec::new());
        lock.recover(
            &docker,
            IMAGE,
            &stale.expected_model,
            Duration::from_secs(5),
            &Cancellation::default(),
        )
        .unwrap();
        assert!(lock.read_state().unwrap().is_none());
        assert_eq!(
            std::fs::read(output.join("model.log")).unwrap(),
            b"retained log"
        );
    }

    #[test]
    fn sigkill_during_post_verification_preserves_workspace_and_state() {
        const CHILD: &str = "OPENVMM_FVP_POST_VERIFY_CRASH_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let (mut session, workspace) = registered_workspace(Path::new(&root));
            session.cancellation.cancel();
            session.stop_resources().unwrap();
            let state_path = session.lock.state_path();
            let result = session.post_verify::<()>(
                &Deadline::new(Duration::from_secs(2)).unwrap(),
                |_, token| {
                    token.check()?;
                    let state = RunState::parse(&std::fs::read(&state_path)?)?;
                    anyhow::ensure!(
                        state.post_verification == PostVerification::InFlight,
                        "post-verification intent was not persisted"
                    );
                    std::fs::write(workspace.join("logs/post-started"), b"verification began")?;
                    signal_hook::low_level::raise(signal_hook::consts::SIGKILL)?;
                    anyhow::bail!("SIGKILL did not terminate the verification owner")
                },
            );
            panic!("verification owner survived: {result:?}");
        }
        let dir = directory();
        let deadline = Deadline::new(Duration::from_secs(3)).unwrap();
        let output = run_command_with_cleanup(
            Command::new(std::env::current_exe().unwrap())
                .arg("sigkill_during_post_verification_preserves_workspace_and_state")
                .env(CHILD, dir.path()),
            &deadline,
            &deadline,
        )
        .unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&output.status),
            Some(signal_hook::consts::SIGKILL),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let lock = lock(dir.path());
        let record = lock.read_state().unwrap().unwrap();
        assert!(record.resources_stopped);
        assert_eq!(record.post_verification, PostVerification::InFlight);
        let workspace = record.workspace_path().unwrap().to_owned();
        let original = std::fs::read(lock.state_path()).unwrap();
        let (docker, fake) = fake_docker(Vec::new());
        let error = lock
            .recover(
                &docker,
                IMAGE,
                &record.expected_model,
                Duration::from_secs(1),
                &Cancellation::default(),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("post-verification"));
        assert_eq!(std::fs::read(lock.state_path()).unwrap(), original);
        assert!(fake.borrow().calls.is_empty());
        assert_eq!(
            std::fs::read(workspace.join("logs/post-started")).unwrap(),
            b"verification began"
        );
        assert_eq!(
            std::fs::read(workspace.join("logs/model.log")).unwrap(),
            b"retained log"
        );
    }
}
