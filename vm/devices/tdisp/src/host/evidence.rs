// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Backend;
use super::Coordinator;
use super::DeviceState;
use super::Error;
use super::Object;
use super::SnapshotError;
use futures::lock::Mutex;
use futures::lock::OwnedMutexGuard;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Host-owned destination for a borrowed evidence snapshot.
///
/// Called on a blocking worker while the device remains exclusively owned.
/// Implementations validate and synchronize destination access themselves; the
/// service knows nothing about guest addresses. Failure may leave a partial
/// write. Do not reenter this device's service from the sink.
pub trait EvidenceSink: Send + Sync {
    /// Write the selected bytes, or report failure without a success byte count.
    fn write(&self, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Evidence failure with the original diagnostic source retained.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    /// Only native snapshot slicing failures map to this variant.
    #[error("invalid evidence range")]
    InvalidRange(#[source] SnapshotError),
    /// Backend, lifecycle, or snapshot acquisition failed.
    #[error("evidence device operation failed")]
    Device(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Destination access failed and may have copied a prefix.
    #[error("evidence destination access failed")]
    Access(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Teardown has closed evidence admission, even if cleanup failed.
    #[error("evidence service is closed")]
    Closed,
}

impl<E: std::error::Error + Send + Sync + 'static> From<Error<E>> for EvidenceError {
    fn from(error: Error<E>) -> Self {
        match error {
            Error::Snapshot(error @ SnapshotError::InvalidRange { .. }) => {
                Self::InvalidRange(error)
            }
            error => Self::Device(Box::new(error)),
        }
    }
}

/// Exclusively owned evidence access with one admitted worker per device.
///
/// Waiting for admission creates no worker or object copy. After admission,
/// cancellation does not stop the worker: it retains the owner, permit and sink
/// through completion, including while queued in the blocking pool. A caller
/// must not resume a guest operation abandoned while awaiting this service;
/// its sink may still be writing guest memory.
/// Callers must bound their pending operations (for example, one per VP).
///
/// Teardown closes admission on its first poll, before waiting for accepted
/// work. Cancellation never reopens it. Retry teardown to drain prior workers
/// and retry failed cleanup. Retain a strong service reference until cleanup
/// succeeds; routing registries should hold only weak references.
#[async_trait::async_trait]
pub trait EvidenceService: Send + Sync {
    /// Acquire a bounded whole snapshot and return its verified size.
    async fn object_size(&self, object: Object) -> Result<usize, EvidenceError>;

    /// Deliver a borrowed snapshot slice; return its exact length only on success.
    async fn read_object(
        &self,
        object: Object,
        offset: u64,
        length: u64,
        sink: Arc<dyn EvidenceSink>,
    ) -> Result<usize, EvidenceError>;

    /// Close admission permanently, drain workers, and perform retryable cleanup.
    async fn teardown(&self) -> Result<(), EvidenceError>;
}

struct Service<B: Backend> {
    owner: Arc<Mutex<Coordinator<B>>>,
    closed: AtomicBool,
}

impl<B: Backend + Send + 'static> Coordinator<B> {
    /// Consume the exclusive coordinator into an evidence-only worker service.
    ///
    /// No backend aliases or mutations are exposed. The coordinator's existing
    /// snapshot budget remains shared; sinks borrow snapshots without copying.
    pub fn into_evidence_service(self) -> Arc<dyn EvidenceService> {
        Arc::new(Service {
            owner: Arc::new(Mutex::new(self)),
            closed: AtomicBool::new(false),
        })
    }
}

impl<B: Backend> Service<B> {
    async fn admit(&self) -> Result<OwnedMutexGuard<Coordinator<B>>, EvidenceError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(EvidenceError::Closed);
        }
        let owner = self.owner.clone().lock_owned().await;
        if self.closed.load(Ordering::SeqCst) {
            return Err(EvidenceError::Closed);
        }
        Ok(owner)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("evidence worker ended without reporting completion")]
struct WorkerCompletionError(#[source] mesh::RecvError);

async fn receive_completion<R>(
    receiver: mesh::OneshotReceiver<Result<R, EvidenceError>>,
) -> Result<R, EvidenceError> {
    receiver
        .await
        .map_err(|source| EvidenceError::Device(Box::new(WorkerCompletionError(source))))?
}

async fn run<R: Send + 'static>(
    operation: impl FnOnce() -> Result<R, EvidenceError> + Send + 'static,
) -> Result<R, EvidenceError> {
    let (sender, receiver) = mesh::oneshot();
    // Detach before awaiting: dropping a queued Task would cancel its work.
    blocking::unblock(move || {
        let result = operation();
        // A cancelled caller no longer needs the result, but work must finish.
        sender.send(result);
    })
    .detach();
    receive_completion(receiver).await
}

#[async_trait::async_trait]
impl<B: Backend + Send + 'static> EvidenceService for Service<B> {
    async fn object_size(&self, object: Object) -> Result<usize, EvidenceError> {
        let mut owner = self.admit().await?;
        run(move || owner.object_size(object).map_err(EvidenceError::from)).await
    }

    async fn read_object(
        &self,
        object: Object,
        offset: u64,
        length: u64,
        sink: Arc<dyn EvidenceSink>,
    ) -> Result<usize, EvidenceError> {
        let mut owner = self.admit().await?;
        run(move || {
            let bytes = owner.read_object(object, offset, length)?;
            sink.write(bytes).map_err(EvidenceError::Access)?;
            Ok(bytes.len())
        })
        .await
    }

    async fn teardown(&self) -> Result<(), EvidenceError> {
        self.closed.store(true, Ordering::SeqCst);
        let mut owner = self.owner.clone().lock_owned().await;
        run(move || {
            if owner.state() == DeviceState::TornDown {
                return Ok(());
            }
            owner.teardown().map_err(EvidenceError::from)
        })
        .await
    }
}

#[cfg(test)]
mod tests;
