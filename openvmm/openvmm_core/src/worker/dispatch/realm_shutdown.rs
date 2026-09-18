// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checked native owner shutdown. This is not constructor-failure recovery or
//! protection against process kill. Cancellation retains custody until the
//! external device/model domain ends; it does not acknowledge cleanup.

use super::LoadedVm;
use super::RestartState;
use futures::StreamExt;
use futures::future::BoxFuture;
use mesh::error::RemoteError;
use mesh_worker::WorkerRpc;
use openvmm_defs::rpc::VmRpc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Stop,
    DynamicDevices,
    StaticDevices,
    RealmServices,
    Complete,
}

trait ShutdownOwner {
    fn close_admission(&self);
    async fn stop(&mut self);
    fn drain_dynamic(&mut self) -> BoxFuture<'static, ()>;
    fn drain_static(&mut self) -> BoxFuture<'static, ()>;
    async fn teardown(&mut self) -> anyhow::Result<()>;
}

struct Retained<T> {
    owner: T,
    phase: Phase,
    removal: Option<BoxFuture<'static, ()>>,
    last_error: Option<String>,
}

/// The separate allocation also retains the owned future being polled. Merely
/// retaining the VM would lose any unit already moved into that future.
struct Custody<T>(Option<Box<Retained<T>>>);

impl<T: ShutdownOwner> Custody<T> {
    fn new(owner: T) -> Self {
        let custody = Self(Some(Box::new(Retained {
            owner,
            phase: Phase::Stop,
            removal: None,
            last_error: None,
        })));
        custody.0.as_ref().unwrap().owner.close_admission();
        custody
    }

    async fn attempt(&mut self) -> anyhow::Result<()> {
        let retained = self.0.as_mut().unwrap();
        loop {
            match retained.phase {
                Phase::Stop => {
                    retained.owner.stop().await;
                    retained.phase = Phase::DynamicDevices;
                }
                Phase::DynamicDevices | Phase::StaticDevices => {
                    if retained.removal.is_none() {
                        retained.removal = Some(match retained.phase {
                            Phase::DynamicDevices => retained.owner.drain_dynamic(),
                            _ => retained.owner.drain_static(),
                        });
                    }
                    retained.removal.as_mut().unwrap().await;
                    retained.removal = None;
                    retained.phase = match retained.phase {
                        Phase::DynamicDevices => Phase::StaticDevices,
                        _ => Phase::RealmServices,
                    };
                }
                Phase::RealmServices => {
                    if let Err(error) = retained.owner.teardown().await {
                        retained.last_error = Some(format!("{error:#}"));
                        return Err(error);
                    }
                    retained.last_error = None;
                    retained.phase = Phase::Complete;
                }
                Phase::Complete => return Ok(()),
            }
        }
    }

    fn release(mut self) -> T {
        assert_eq!(self.0.as_ref().unwrap().phase, Phase::Complete);
        self.0.take().unwrap().owner
    }
}

impl<T> Drop for Custody<T> {
    fn drop(&mut self) {
        if let Some(retained) = self.0.take() {
            // No locks, task cancellation, or blocking cleanup in this path.
            std::mem::forget(retained);
        }
    }
}

impl ShutdownOwner for LoadedVm {
    fn close_admission(&self) {
        self.inner._realm_teardown.close_admission();
    }

    async fn stop(&mut self) {
        self.pause().await;
    }

    fn drain_dynamic(&mut self) -> BoxFuture<'static, ()> {
        let vpci = std::mem::take(&mut self.inner.dynamic_vpci_devices);
        let pcie = std::mem::take(&mut self.inner.pcie_hotplug_devices);
        Box::pin(async move {
            for entry in vpci {
                entry.device.remove().await;
            }
            for (_, unit, device) in pcie {
                unit.remove().await;
                drop(device);
            }
        })
    }

    fn drain_static(&mut self) -> BoxFuture<'static, ()> {
        Box::pin(self.inner.chipset_devices.drain_device_units())
    }

    async fn teardown(&mut self) -> anyhow::Result<()> {
        self.inner._realm_teardown.teardown().await?;
        Ok(())
    }
}

/// Construct custody synchronously, before returning an unpolled future.
pub(super) fn shutdown(
    vm: LoadedVm,
    rpc: mesh::Receiver<VmRpc>,
    worker_rpc: mesh::Receiver<WorkerRpc<RestartState>>,
) -> impl Future<Output = LoadedVm> {
    begin_shutdown(vm, rpc, worker_rpc)
}

fn begin_shutdown<T: ShutdownOwner, R: 'static + Send>(
    vm: T,
    rpc: mesh::Receiver<VmRpc>,
    worker_rpc: mesh::Receiver<WorkerRpc<R>>,
) -> impl Future<Output = T> {
    let custody = Custody::new(vm);
    // Dropping the receiver rejects ClearHalt, Resume, Reset, device changes,
    // and every other ordinary VM RPC. Normal dispatch never resumes.
    drop(rpc);
    shutdown_retained(custody, worker_rpc)
}

async fn shutdown_retained<T: ShutdownOwner, R: 'static + Send>(
    mut custody: Custody<T>,
    mut worker_rpc: mesh::Receiver<WorkerRpc<R>>,
) -> T {
    loop {
        match custody.attempt().await {
            Ok(()) => {
                tracing::info!("native Realm owner teardown completed");
                return custody.release();
            }
            Err(error) => {
                tracing::error!(
                    phase = ?custody.0.as_ref().unwrap().phase,
                    error = %format_args!("{error:#}"),
                    "native Realm shutdown failed; retaining VM and backing"
                );
            }
        }
        loop {
            match worker_rpc.next().await {
                Some(WorkerRpc::Stop) => break,
                Some(WorkerRpc::Inspect(req)) => {
                    let retained = custody.0.as_ref().unwrap();
                    req.respond(|resp| {
                        resp.field("realm_shutdown_phase", format!("{:?}", retained.phase))
                            .field("realm_shutdown_error", &retained.last_error)
                            .field("realm_shutdown_retained", true);
                    });
                }
                Some(WorkerRpc::Restart(rpc)) => {
                    rpc.complete(Err(RemoteError::new(anyhow::anyhow!(
                        "native Realm shutdown is irreversible; only Inspect and Stop/retry are allowed"
                    ))));
                }
                None => {
                    // A closed parent channel cannot release custody or spin.
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
