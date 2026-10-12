// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The client for `pipette`.

#![forbid(unsafe_code)]

pub mod process;
mod send;
pub mod shell;

pub use pipette_protocol::PIPETTE_PORT;
pub use pipette_protocol::PIPETTE_READY_MARKER;

use crate::send::PipetteSender;
use anyhow::Context;
use futures::AsyncBufReadExt;
use futures::AsyncRead;
use futures::AsyncWrite;
use futures::AsyncWriteExt;
use futures::FutureExt as _;
use futures::StreamExt;
use futures::io::BufReader;
use futures_concurrency::future::TryJoin;
use mesh::CancelContext;
use mesh::error::RemoteError;
use mesh::payload::Timestamp;
use mesh::rpc::RpcError;
use mesh_remote::PointToPointMesh;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pipette_protocol::DiagnosticFile;
use pipette_protocol::ListDirRequest;
use pipette_protocol::ListDirResponse;
use pipette_protocol::PipetteBootstrap;
use pipette_protocol::PipetteRequest;
use pipette_protocol::ReadFileRequest;
use pipette_protocol::WriteFileRequest;
use shell::UnixShell;
use shell::WindowsShell;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

/// A client to a running `pipette` instance inside a VM.
#[derive(Clone)]
pub struct PipetteClient {
    send: PipetteSender,
    connection: std::sync::Arc<PipetteConnection>,
}

struct PipetteConnection {
    watch: futures::lock::Mutex<Option<mesh::OneshotReceiver<()>>>,
    tasks: futures::lock::Mutex<Option<PipetteConnectionTasks>>,
}

struct PipetteConnectionTasks {
    mesh: PointToPointMesh,
    log_task: Task<()>,
    diag_task: Task<()>,
}

/// Maximum time to wait for a single pipette connection attempt — the mesh
/// handshake plus the liveness ping — to complete.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

impl PipetteClient {
    /// Connects to a `pipette` instance inside a VM.
    ///
    /// `conn` must be an established connection over some byte stream (e.g., a
    /// socket).
    pub async fn new(
        spawner: impl Spawn,
        conn: impl 'static + AsyncRead + AsyncWrite + Send + Unpin,
        output_dir: &Path,
    ) -> anyhow::Result<Self> {
        let mut ctx = CancelContext::new().with_timeout(CONNECT_TIMEOUT);
        match ctx
            .until_cancelled(Self::connect(spawner, conn, output_dir))
            .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "timed out establishing pipette connection after {CONNECT_TIMEOUT:?}"
            )),
        }
    }

    async fn connect(
        spawner: impl Spawn,
        conn: impl 'static + AsyncRead + AsyncWrite + Send + Unpin,
        output_dir: &Path,
    ) -> anyhow::Result<Self> {
        let (bootstrap_send, bootstrap_recv) = mesh::oneshot::<PipetteBootstrap>();
        let mesh = PointToPointMesh::new(&spawner, conn, bootstrap_send.into());
        let bootstrap = bootstrap_recv
            .await
            .context("failed to receive pipette bootstrap")?;

        let PipetteBootstrap {
            requests,
            diag_file_recv,
            watch,
            log,
        } = bootstrap;

        let log_task = spawner.spawn("pipette-log", replay_logs(log));
        let diag_task = spawner.spawn(
            "diagnostics-recv",
            recv_diag_files(output_dir.to_owned(), diag_file_recv),
        );

        let client = Self {
            send: PipetteSender::new(requests),
            connection: std::sync::Arc::new(PipetteConnection {
                watch: futures::lock::Mutex::new(Some(watch)),
                tasks: futures::lock::Mutex::new(Some(PipetteConnectionTasks {
                    mesh,
                    log_task,
                    diag_task,
                })),
            }),
        };

        // A successful mesh handshake is not, on its own, proof of a usable
        // connection: a byte stream can deliver the guest's bootstrap to the host
        // and then be torn down moments later — for example by the reset in a
        // save/restore pulse, or by a TCP forward that silently drops the
        // guest's traffic — leaving a client whose requests would never be
        // answered. To guard against this, we confirm the agent is actually
        // reachable with a ping, bounded by the timeout in `new`.
        client
            .ping()
            .await
            .context("pipette liveness ping failed")?;

        Ok(client)
    }

    /// Close the shared connection for all clones and join its tasks.
    ///
    /// Call this on the connection's executor before stopping that executor.
    /// Clones may outlive it, but cannot issue further guest requests.
    pub async fn close(&self) {
        let mut tasks = self.connection.tasks.lock().await;
        if let Some(PipetteConnectionTasks {
            mesh,
            log_task,
            diag_task,
        }) = tasks.take()
        {
            mesh.shutdown().await;
            futures::join!(log_task.cancel(), diag_task.cancel());
        }
    }

    /// Pings the agent to check if it's alive.
    pub async fn ping(&self) -> Result<(), RpcError> {
        self.send.call(PipetteRequest::Ping, ()).await
    }

    /// Return a shell object to interact with a Windows guest.
    pub fn windows_shell(&self) -> WindowsShell<'_> {
        WindowsShell::new(self)
    }

    /// Return a shell object to interact with a Linux guest.
    pub fn unix_shell(&self) -> UnixShell<'_> {
        UnixShell::new(self)
    }

    /// Mounts a filesystem inside the guest (Linux only).
    pub async fn mount(
        &self,
        source: &str,
        target: &str,
        fstype: &str,
        flags: u64,
        mkdir_target: bool,
    ) -> anyhow::Result<()> {
        self.send
            .call_failable(
                PipetteRequest::Mount,
                pipette_protocol::MountRequest {
                    source: source.to_owned(),
                    target: target.to_owned(),
                    fstype: fstype.to_owned(),
                    flags,
                    mkdir_target,
                },
            )
            .await
            .context("failed to send mount request")?;
        Ok(())
    }

    /// Prepares a chroot by bind-mounting `/proc`, `/dev`, and `/sys` into it,
    /// and mounting a writable tmpfs at `/tmp`.
    pub async fn prepare_chroot(&self, target: &str) -> anyhow::Result<()> {
        // MS_BIND = 0x1000
        const MS_BIND: u64 = 0x1000;
        for dir in ["/proc", "/dev", "/sys"] {
            let mount_target = format!("{target}{dir}");
            self.mount(dir, &mount_target, "", MS_BIND, true).await?;
        }
        // Mount a writable tmpfs so tools like iperf3 can create temp files.
        let tmp_target = format!("{target}/tmp");
        self.mount("tmpfs", &tmp_target, "tmpfs", 0, true).await?;
        Ok(())
    }

    /// Returns an object used to launch a command inside the guest.
    ///
    /// TODO: this is a low-level interface. Make a high-level interface like
    /// `xshell::Shell` for manipulating the environment and launching
    /// processes.
    pub fn command(&self, program: impl AsRef<str>) -> process::Command<'_> {
        process::Command::new(self, program)
    }

    /// Sends a request to the guest to power off.
    pub async fn power_off(&self) -> anyhow::Result<()> {
        self.shutdown(pipette_protocol::ShutdownType::PowerOff)
            .await
    }

    /// Sends a request to the guest to reboot.
    pub async fn reboot(&self) -> anyhow::Result<()> {
        self.shutdown(pipette_protocol::ShutdownType::Reboot).await
    }

    async fn shutdown(&self, shutdown_type: pipette_protocol::ShutdownType) -> anyhow::Result<()> {
        tracing::debug!(?shutdown_type, "sending shutdown request to guest");
        let r = self.send.call(
            PipetteRequest::Shutdown,
            pipette_protocol::ShutdownRequest { shutdown_type },
        );
        match r.await {
            Ok(r) => r
                .map_err(anyhow::Error::from)
                .context("failed to shut down")?,
            Err(_) => {
                // Presumably this is an expected error due to the agent exiting
                // or the guest powering off.
            }
        }
        Ok(())
    }

    /// Reads the full contents of a file.
    pub async fn read_file(&self, path: impl AsRef<str>) -> anyhow::Result<Vec<u8>> {
        let (recv_pipe, send_pipe) = mesh::pipe::pipe();
        let req = ReadFileRequest {
            path: path.as_ref().to_string(),
            sender: send_pipe,
        };

        let request_future = self.send.call_failable(PipetteRequest::ReadFile, req);

        let mut contents = Vec::new();
        let transfer_future = async { futures::io::copy(recv_pipe, &mut contents).await };

        tracing::debug!(path = path.as_ref(), "beginning file read transfer");
        let (bytes_read, io_result) = (request_future, transfer_future.map(Ok))
            .try_join()
            .await
            .context("failed to read file")?;

        io_result.context("io failure")?;
        if bytes_read != contents.len() as u64 {
            anyhow::bail!("file truncated");
        }

        tracing::debug!("file read complete");
        Ok(contents)
    }

    /// Writes a file to the guest.
    /// Note: This may transfer the file in chunks. It is likely not suitable
    /// for writing to files that require all content to be written at once,
    /// e.g. files in /proc or /sys.
    pub async fn write_file(
        &self,
        path: impl AsRef<str>,
        contents: impl AsyncRead,
    ) -> anyhow::Result<()> {
        let (recv_pipe, mut send_pipe) = mesh::pipe::pipe();
        let req = WriteFileRequest {
            path: path.as_ref().to_string(),
            receiver: recv_pipe,
        };

        let request_future = self.send.call_failable(PipetteRequest::WriteFile, req);

        let transfer_future = async {
            let copy_result = futures::io::copy(contents, &mut send_pipe).await;
            send_pipe.close().await?;
            copy_result
        };

        tracing::debug!(path = path.as_ref(), "beginning file wurite transfer");
        let (bytes_written, io_result) = (request_future, transfer_future.map(Ok))
            .try_join()
            .await
            .context("failed to write file")?;
        if bytes_written != io_result.context("io failure")? {
            anyhow::bail!("file truncated");
        }

        tracing::debug!("file write complete");
        Ok(())
    }

    /// List the contents of a directory
    pub async fn list_dir(&self, path: impl AsRef<str>) -> anyhow::Result<Vec<String>> {
        let ListDirResponse { files } = self
            .send
            .call_failable(
                PipetteRequest::ListDir,
                ListDirRequest {
                    path: path.as_ref().to_string(),
                },
            )
            .await?;

        tracing::debug!("list dir complete");
        Ok(files)
    }

    /// Waits for the agent to exit.
    ///
    /// Only one client sharing this connection can consume the exit notification.
    pub async fn wait(self) -> Result<(), mesh::RecvError> {
        let watch = self.connection.watch.lock().await.take();
        match watch {
            Some(watch) => watch.await,
            None => Err(mesh::RecvError::Closed),
        }
    }

    /// Returns the current time in the guest.
    pub async fn get_time(&self) -> anyhow::Result<Timestamp> {
        self.send
            .call(PipetteRequest::GetTime, ())
            .await
            .context("failed to get time")
    }

    /// Tell the agent to crash itself.
    pub async fn crash(&self) -> Result<(), RemoteError> {
        Self::handle_crash_result(self.send.call_failable(PipetteRequest::Crash, ()).await)
    }

    /// Tell the agent to crash the kernel.
    pub async fn kernel_crash(&self) -> Result<(), RemoteError> {
        Self::handle_crash_result(
            self.send
                .call_failable(PipetteRequest::KernelCrash, ())
                .await,
        )
    }

    fn handle_crash_result(r: Result<(), RpcError<RemoteError>>) -> Result<(), RemoteError> {
        match r {
            Ok(()) => unreachable!(),
            Err(RpcError::Call(err)) => Err(err),
            Err(RpcError::Channel(_)) => {
                // Presumably this is an expected error due to the agent exiting
                // or the guest crashing.
                Ok(())
            }
        }
    }
}

async fn replay_logs(log: mesh::pipe::ReadPipe) {
    let mut lines = BufReader::new(log).lines();
    while let Some(line) = lines.next().await {
        match line {
            Ok(line) => tracing::debug!(target: "pipette", "{}", line),
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "pipette log failure"
                );
                break;
            }
        }
    }
}

async fn recv_diag_files(output_dir: PathBuf, mut diag_file_recv: mesh::Receiver<DiagnosticFile>) {
    while let Some(diag_file) = diag_file_recv.next().await {
        let DiagnosticFile { name, mut receiver } = diag_file;
        tracing::debug!(name, "receiving diagnostic file");
        let path = output_dir.join(&name);
        let file = fs_err::File::create(&path).expect("failed to create diagnostic file {name}");
        futures::io::copy(&mut receiver, &mut futures::io::AllowStdIo::new(file))
            .await
            .expect("failed to write diagnostic file");
        tracing::debug!(name, "diagnostic file transfer complete");

        #[expect(
            clippy::disallowed_methods,
            reason = "ATTACHMENT is most reliable when using true canonicalized paths"
        )]
        let canonical_path = path
            .canonicalize()
            .expect("failed to canonicalize attachment path");
        // Use the inline junit syntax to attach the file to the test result.
        println!("[[ATTACHMENT|{}]]", canonical_path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncRead;
    use futures::AsyncWrite;
    use pal_async::DefaultPool;
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;
    use test_with_tracing::test;

    struct Duplex(mesh::pipe::ReadPipe, mesh::pipe::WritePipe);

    impl AsyncRead for Duplex {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Duplex {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.1).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.1).poll_flush(cx)
        }
        fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.1).poll_close(cx)
        }
    }

    #[test]
    fn client_drop_after_supervisor_pool_exits_does_not_block() {
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut pool = DefaultPool::new();
            let driver = pool.driver();
            let (host_read, guest_write) = mesh::pipe::pipe();
            let (guest_read, host_write) = mesh::pipe::pipe();
            let (bootstrap_send, bootstrap_recv) = mesh::oneshot::<PipetteBootstrap>();
            let guest_mesh = PointToPointMesh::new(
                &driver,
                Duplex(guest_read, guest_write),
                bootstrap_recv.into(),
            );
            let (requests, mut request_recv) = mesh::channel();
            let (_diag, diag_file_recv) = mesh::channel();
            let (_watch, watch) = mesh::oneshot();
            let (log, _log_write) = mesh::pipe::pipe();
            bootstrap_send.send(PipetteBootstrap {
                requests,
                diag_file_recv,
                watch,
                log,
            });
            let guest = driver.spawn("mock-pipette", async move {
                while let Some(request) = request_recv.next().await {
                    if let PipetteRequest::Ping(rpc) = request {
                        rpc.complete(());
                    }
                }
            });
            let client = pool
                .run_until(PipetteClient::new(
                    &driver,
                    Duplex(host_read, host_write),
                    Path::new("."),
                ))
                .unwrap();
            let surviving_client = client.clone();
            pool.run_until(client.close());
            pool.run_until(surviving_client.close());
            drop(client);
            drop(guest_mesh);
            drop(guest);
            drop(driver);
            pool.run();
            drop(surviving_client);
            done.send(()).unwrap();
        });
        result
            .recv_timeout(Duration::from_secs(5))
            .expect("client destruction blocked after its supervisor pool exited");
    }
}
