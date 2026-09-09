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
use std::collections::BTreeMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::TcpListener;
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

fn resolve_existing_ancestor(path: &Path) -> anyhow::Result<PathBuf> {
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
            serde_json::to_writer(&mut file, state)?;
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
        let Some(state) = self.read_state()? else {
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
}

impl RunState {
    fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        anyhow::ensure!(
            value.get("schema_version").and_then(|v| v.as_u64()) == Some(u64::from(SCHEMA_VERSION)),
            "unsupported FVP state schema version"
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
            "FVP Docker command {:?} failed ({}): {}",
            args.first(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
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
            let output = self.invoke(&["inspect", "--type", "container", "--", id], deadline)?;
            validate_container(&output, id, state)?;
        }
        for id in &containers {
            self.invoke(&["rm", "--force", "--", id], deadline)?;
        }
        anyhow::ensure!(
            self.containers(state, deadline)?.is_empty(),
            "owned FVP containers survived cleanup; preserving state"
        );
        self.verify_binding(state, deadline, None)?;
        Ok(containers)
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
            !self.cleaned && self.state.callback_phase == CallbackPhase::Idle,
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
                })?;
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
            self.child = Some(ManagedChild::spawn(&mut command)?);
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
                })?;
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
                if probe(&deadline)? {
                    return Ok(Ok(()));
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
        anyhow::ensure!(self.ready, "FVP pipette readiness was not confirmed");
        let deadline = *self
            .phases
            .as_mut()
            .context("FVP model has not started")?
            .advance(Phase::GuestShutdown, timeout)?;
        self.cancellation.check()?;
        self.run_callback(CallbackPhase::ShutdownRequest, &deadline, |_| {
            request(&deadline)
        })?;
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

    /// Stop the owned child/group, prove and remove containers, then remove only
    /// this state. Cancellation does not interrupt cleanup. Errors keep state.
    pub fn cleanup(&mut self) -> anyhow::Result<()> {
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
        if let Some(child) = &mut self.child {
            child.terminate(&deadline)?;
        }
        self.docker.cleanup(&self.state, &deadline)?;
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
        self.lock.remove_state(&self.state)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
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
        while identity.observe().unwrap() == ProcessObservation::Matching {
            pause(&deadline, &Cancellation::default()).unwrap();
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
}
