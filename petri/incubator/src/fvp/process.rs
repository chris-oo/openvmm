// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded local commands used to supervise the licensed FVP.

use anyhow::Context;
use nix::errno::Errno;
use nix::sys::signal::Signal;
use nix::sys::signal::killpg;
use nix::sys::wait::Id;
use nix::sys::wait::WaitPidFlag;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitid;
use nix::unistd::Pid;
use parking_lot::Mutex;
use std::io::Read;
use std::io::Seek;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Output;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// A non-resetting host-monotonic deadline.
#[derive(Clone, Copy, Debug)]
pub struct Deadline(Instant);

impl Deadline {
    /// Start a phase budget.
    pub fn new(duration: Duration) -> anyhow::Result<Self> {
        Ok(Self(
            Instant::now()
                .checked_add(duration)
                .context("FVP deadline overflow")?,
        ))
    }

    /// Time remaining in the original phase, or an explicit timeout error.
    pub fn remaining(&self) -> anyhow::Result<Duration> {
        let remaining = self.0.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "FVP phase deadline exceeded");
        Ok(remaining)
    }

    /// Whether the phase budget has expired.
    pub fn expired(&self) -> bool {
        Instant::now() >= self.0
    }

    /// A subcommand budget that cannot extend this phase.
    pub fn limited_to(&self, maximum: Duration) -> anyhow::Result<Self> {
        Ok(Self(self.0.min(Self::new(maximum)?.0)))
    }
}

/// An owned child and process group. PID ownership is retained until cleanup.
pub struct ManagedChild {
    child: Option<Child>,
    id: u32,
    pid: Pid,
    status: Option<ExitStatus>,
    cleanup_deadline: Option<Deadline>,
    reaper: Arc<ChildReaper>,
}

struct ChildReaper {
    pending: Mutex<Vec<Child>>,
}

fn child_reaper() -> anyhow::Result<Arc<ChildReaper>> {
    static REAPER: OnceLock<Result<Arc<ChildReaper>, String>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            let reaper = Arc::new(ChildReaper {
                pending: Mutex::new(Vec::new()),
            });
            let worker = reaper.clone();
            std::thread::Builder::new()
                .name("fvp-child-reaper".into())
                .spawn(move || loop {
                    {
                        let mut pending = worker.pending.lock();
                        let mut index = 0;
                        while index < pending.len() {
                            match pending[index].try_wait() {
                                Ok(Some(_)) => {
                                    let mut reaped = pending.swap_remove(index);
                                    if let Err(error) = reaped.wait() {
                                        tracing::error!(pid = reaped.id(), %error, "FVP reaper lost cached exit status");
                                    }
                                }
                                Err(error) if error.raw_os_error() == Some(Errno::ECHILD as i32) => {
                                    tracing::error!(pid = pending[index].id(), %error, "FVP child was reaped externally");
                                    drop(pending.swap_remove(index));
                                }
                                Err(error) => {
                                    tracing::error!(pid = pending[index].id(), %error, "FVP deferred reap failed");
                                    index += 1;
                                }
                                Ok(None) => index += 1,
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(20));
                })
                .map_err(|error| error.to_string())?;
            Ok(reaper)
        })
        .as_ref()
        .cloned()
        .map_err(|error| anyhow::anyhow!("failed to start FVP child reaper: {error}"))
}

impl ManagedChild {
    /// Launch into a fresh process group without inheriting terminal input.
    pub fn spawn(command: &mut Command) -> anyhow::Result<Self> {
        let reaper = child_reaper()?;
        command.process_group(0).stdin(Stdio::null());
        let mut child = command.spawn().context("failed to start FVP subprocess")?;
        let pid = match i32::try_from(child.id()) {
            Ok(pid) => Pid::from_raw(pid),
            Err(error) => {
                child
                    .kill()
                    .context("failed to stop child with invalid PID")?;
                child
                    .wait()
                    .context("failed to reap child with invalid PID")?;
                return Err(error).context("FVP subprocess PID is out of range");
            }
        };
        Ok(Self {
            id: child.id(),
            child: Some(child),
            pid,
            status: None,
            cleanup_deadline: None,
            reaper,
        })
    }

    /// Owned leader PID.
    pub fn id(&self) -> u32 {
        self.id
    }

    fn signal_group(&self, signal: Signal) -> anyhow::Result<()> {
        match killpg(self.pid, signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(error).context("failed to signal owned FVP process group"),
        }
    }

    /// Observe exit, terminate residual group members, and reap the leader.
    pub fn try_wait(&mut self) -> anyhow::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let observed = waitid(
            Id::Pid(self.pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
        )
        .context("failed to inspect owned FVP subprocess")?;
        if observed == WaitStatus::StillAlive {
            return Ok(None);
        }
        // WNOWAIT keeps the leader PID reserved while we signal the group.
        self.signal_group(Signal::SIGKILL)?;
        let status = self
            .child
            .as_mut()
            .context("FVP child handle is absent")?
            .wait()
            .context("failed to reap FVP subprocess")?;
        self.status = Some(status);
        Ok(Some(status))
    }

    /// Wait without resetting the caller's deadline.
    pub fn wait(&mut self, deadline: &Deadline) -> anyhow::Result<ExitStatus> {
        loop {
            deadline.remaining()?;
            if let Some(status) = self.try_wait()? {
                deadline.remaining()?;
                return Ok(status);
            }
            std::thread::sleep(deadline.remaining()?.min(Duration::from_millis(20)));
        }
    }

    /// Force cleanup within the caller's remaining cleanup budget.
    pub fn terminate(&mut self, deadline: &Deadline) -> anyhow::Result<()> {
        let deadline = self
            .cleanup_deadline
            .map_or(*deadline, |original| Deadline(original.0.min(deadline.0)));
        self.cleanup_deadline = Some(deadline);
        if self.status.is_some() {
            return Ok(());
        }
        self.signal_group(Signal::SIGKILL)?;
        self.wait(&deadline)?;
        Ok(())
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.status.is_none() {
            let result = self
                .cleanup_deadline
                .map_or_else(|| Deadline::new(Duration::from_secs(10)), Ok)
                .and_then(|deadline| self.terminate(&deadline));
            if let Err(error) = result {
                tracing::error!(pid = self.id(), %error, "owned FVP subprocess cleanup failed");
                if let Some(child) = self.child.take() {
                    self.reaper.pending.lock().push(child);
                }
            }
        }
    }
}

/// Capture a bounded command without pipe deadlocks. Nonzero status is returned
/// to the caller, which supplies command-specific error context.
pub fn run_command(command: &mut Command, deadline: &Deadline) -> anyhow::Result<Output> {
    run_command_inner(command, deadline, None, None)
}

/// Capture ordinary inventory/startup commands while observing cancellation.
/// Cleanup deliberately remains non-cancellable.
pub fn run_command_cancellable(
    command: &mut Command,
    deadline: &Deadline,
    cancellation: &super::lifecycle::Cancellation,
) -> anyhow::Result<Output> {
    run_command_inner(command, deadline, None, Some(cancellation))
}

/// Capture a command while sharing an existing non-resetting cleanup budget.
/// Use this for commands executed during session cleanup.
pub fn run_command_with_cleanup(
    command: &mut Command,
    deadline: &Deadline,
    cleanup_deadline: &Deadline,
) -> anyhow::Result<Output> {
    run_command_inner(command, deadline, Some(cleanup_deadline), None)
}

fn run_command_inner(
    command: &mut Command,
    deadline: &Deadline,
    cleanup_deadline: Option<&Deadline>,
    cancellation: Option<&super::lifecycle::Cancellation>,
) -> anyhow::Result<Output> {
    deadline.remaining()?;
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
    // Leave part of an existing cleanup phase for termination and reaping.
    let operation_deadline = if let Some(cleanup) = cleanup_deadline {
        let remaining = cleanup.remaining()?;
        let reserve = (remaining / 4).min(Duration::from_secs(1));
        deadline.limited_to(remaining.saturating_sub(reserve))?
    } else {
        *deadline
    };
    let mut stdout = tempfile::tempfile().context("failed to create command stdout spool")?;
    let mut stderr = tempfile::tempfile().context("failed to create command stderr spool")?;
    command
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let mut child = ManagedChild::spawn(command)?;
    const OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;
    let mut wait = || -> anyhow::Result<ExitStatus> {
        loop {
            operation_deadline.remaining()?;
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            anyhow::ensure!(
                stdout.metadata()?.len() <= OUTPUT_LIMIT
                    && stderr.metadata()?.len() <= OUTPUT_LIMIT,
                "FVP command output exceeds 16 MiB"
            );
            if let Some(status) = child.try_wait()? {
                operation_deadline.remaining()?;
                return Ok(status);
            }
            std::thread::sleep(
                operation_deadline
                    .remaining()?
                    .min(Duration::from_millis(20)),
            );
        }
    };
    let status = match wait() {
        Ok(status) => status,
        Err(error) => {
            // Termination is necessary even when the operation deadline expired.
            let cleanup = match cleanup_deadline {
                Some(deadline) => *deadline,
                None => Deadline::new(Duration::from_secs(10))?,
            };
            child
                .terminate(&cleanup)
                .context("FVP command failed and owned process cleanup failed")?;
            return Err(error).context("FVP command failed under phase supervision");
        }
    };
    let read = |file: &mut std::fs::File| -> anyhow::Result<Vec<u8>> {
        const LIMIT: u64 = 16 * 1024 * 1024;
        anyhow::ensure!(
            file.metadata()?.len() <= LIMIT,
            "FVP command output exceeds 16 MiB"
        );
        file.rewind()?;
        let mut output = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut output)?;
        anyhow::ensure!(
            output.len() as u64 <= LIMIT,
            "FVP command output exceeds 16 MiB"
        );
        Ok(output)
    };
    let output = Output {
        status,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
    };
    deadline.remaining()?;
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn cancellation_interrupts_an_in_flight_command() {
        let cancellation = super::super::lifecycle::Cancellation::default();
        let trigger = cancellation.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            trigger.cancel();
        });
        let start = Instant::now();
        let result = run_command_cancellable(
            Command::new("sleep").arg("30"),
            &Deadline::new(Duration::from_secs(30)).unwrap(),
            &cancellation,
        );
        thread.join().unwrap();
        assert!(format!("{:#}", result.unwrap_err()).contains("cancelled"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn expired_cleanup_budget_does_not_abandon_child_reaping() {
        for _ in 0..8 {
            let mut child = ManagedChild::spawn(Command::new("sleep").arg("30")).unwrap();
            let pid = child.id();
            let _ = child.terminate(&Deadline::new(Duration::ZERO).unwrap());
            drop(child);
            let deadline = Instant::now() + Duration::from_secs(2);
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                assert!(Instant::now() < deadline, "child {pid} was not reaped");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn an_expired_wait_cannot_accept_an_already_exited_child() {
        let mut child = ManagedChild::spawn(Command::new("sleep").arg("0.05")).unwrap();
        let deadline = Deadline::new(Duration::from_millis(10)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(child.wait(&deadline).is_err());
        child
            .terminate(&Deadline::new(Duration::from_secs(1)).unwrap())
            .unwrap();
    }

    #[test]
    fn subprocess_output_and_error_status_are_preserved() {
        let output = run_command(
            Command::new("sh").args(["-c", "echo stdout; echo stderr >&2; exit 7"]),
            &Deadline::new(Duration::from_secs(5)).unwrap(),
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"stdout\n");
        assert_eq!(output.stderr, b"stderr\n");
    }

    #[test]
    fn subprocess_timeout_terminates_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let start = Instant::now();
        let result = run_command(
            Command::new("sh")
                .args(["-c", "sleep 30 & echo $! > \"$PIDFILE\"; wait"])
                .env("PIDFILE", &pidfile),
            &Deadline::new(Duration::from_millis(200)).unwrap(),
        );
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
        let pid = std::fs::read_to_string(pidfile).unwrap();
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
            assert_eq!(
                stat.rsplit_once(") ").unwrap().1.split_whitespace().next(),
                Some("Z")
            );
        }
    }

    #[test]
    fn successful_leader_cannot_leave_a_running_descendant() {
        let output = run_command(
            Command::new("sh").args(["-c", "sleep 30 & echo $!"]),
            &Deadline::new(Duration::from_secs(5)).unwrap(),
        )
        .unwrap();
        assert!(output.status.success());
        let pid: u32 = std::str::from_utf8(&output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Ok(stat) if stat.rsplit_once(") ").unwrap().1.starts_with("Z ") => break,
                result => {
                    assert!(Instant::now() < deadline, "descendant survived: {result:?}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    #[test]
    fn rejects_excessive_command_output() {
        let result = run_command(
            Command::new("dd").args(["if=/dev/zero", "bs=1048576", "count=20"]),
            &Deadline::new(Duration::from_secs(5)).unwrap(),
        );
        assert!(result.unwrap_err().to_string().contains("FVP command"));
    }

    #[test]
    fn subcommand_cannot_extend_deadline() {
        let deadline = Deadline::new(Duration::from_millis(100)).unwrap();
        let subcommand = deadline.limited_to(Duration::from_secs(30)).unwrap();
        assert!(subcommand.0 <= deadline.0);
        let expired = Deadline::new(Duration::ZERO).unwrap();
        assert!(expired.expired());
        assert!(expired.remaining().is_err());
    }
}
