// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::AssignmentOperation;
use super::Backend;
use super::ConfirmedState;
use super::Coordinator;
use super::DeviceState;
use super::Error;
use super::Object;
use super::Regenerate;
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
    /// The owner does not provide live assignment operations.
    #[error("native assignment operation is unsupported")]
    Unsupported,
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

/// Exclusively owned device access with one admitted worker per device.
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
    /// Permanently reject new work without waiting for a worker or taking its
    /// coordinator lock. Accepted work still owns its resources until it
    /// finishes. This must be safe to call from a fatal-error/drop path.
    fn close_admission(&self);

    /// Whether this service routes live native assignment operations.
    fn supports_assignment(&self) -> bool {
        false
    }

    /// Change state through the same exclusive owner used for evidence.
    async fn set_state(&self, _state: ConfirmedState) -> Result<(), EvidenceError> {
        Err(EvidenceError::Unsupported)
    }

    /// Regenerate an object with owned request parameters.
    async fn regenerate(&self, _request: Regenerate) -> Result<(), EvidenceError> {
        Err(EvidenceError::Unsupported)
    }

    /// Change assignment mappings without creating another state machine.
    async fn assignment(&self, _operation: AssignmentOperation) -> Result<(), EvidenceError> {
        Err(EvidenceError::Unsupported)
    }

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
    assignment: bool,
}

impl<B: Backend + Send + 'static> Coordinator<B> {
    /// Consume the exclusive coordinator into a bounded worker service.
    ///
    /// No backend aliases are exposed. Native mutations use the same admission
    /// and quarantine policy as evidence. The snapshot budget remains shared.
    pub fn into_evidence_service(self) -> Arc<dyn EvidenceService> {
        let assignment = self.supports_assignment();
        Arc::new(Service {
            owner: Arc::new(Mutex::new(self)),
            closed: AtomicBool::new(false),
            assignment,
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
    fn close_admission(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn supports_assignment(&self) -> bool {
        self.assignment
    }

    async fn set_state(&self, state: ConfirmedState) -> Result<(), EvidenceError> {
        if !self.assignment {
            return Err(EvidenceError::Unsupported);
        }
        let mut owner = self.admit().await?;
        run(move || owner.set_state(state).map_err(EvidenceError::from)).await
    }

    async fn regenerate(&self, request: Regenerate) -> Result<(), EvidenceError> {
        if !self.assignment {
            return Err(EvidenceError::Unsupported);
        }
        let mut owner = self.admit().await?;
        run(move || owner.regenerate(request).map_err(EvidenceError::from)).await
    }

    async fn assignment(&self, operation: AssignmentOperation) -> Result<(), EvidenceError> {
        if !self.assignment {
            return Err(EvidenceError::Unsupported);
        }
        let mut owner = self.admit().await?;
        run(move || owner.assignment(operation).map_err(EvidenceError::from)).await
    }

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
        self.close_admission();
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
