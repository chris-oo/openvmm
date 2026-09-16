// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native TDISP evidence access through an exclusively owned Realm assignment.
//!
//! This connects the whole-object snapshot core to Linux TSM ioctls, not to
//! guest RHI exits. No state-changing ioctl, MMIO validation, reset, or evidence
//! regeneration is enabled. The assignment has no RAM or BAR mappings.
//! Explicit teardown uses the Realm owner's ordered, retryable cleanup.

use super::OperationError;
use super::RealmDevice;
use super::RealmPhase;
use super::RealmState;
use std::sync::Arc;
use tdisp::host::Backend;
use tdisp::host::ConfirmedState;
use tdisp::host::Coordinator;
use tdisp::host::DeviceState;
use tdisp::host::EvidenceService;
use tdisp::host::Object;
use tdisp::host::Regenerate;
use tdisp::host::SnapshotBudget;
use vfio_sys::iommufd::tsm::CcaObject;
use vfio_sys::iommufd::tsm::CcaTsmRequest;
use vfio_sys::iommufd::tsm::TsmCompletion;
use vfio_sys::iommufd::tsm::TsmRequestError;

/// Native evidence operation, for error reporting without protocol casts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceOperation {
    /// Query the whole object's size.
    ObjectSize(Object),
    /// Read the whole object at offset zero.
    ReadObject(Object),
}

/// Failure from the Linux evidence backend, with all result channels retained.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// No ioctl was issued because the object owner was not attached.
    #[error("Realm TSM requests require an attached owner, got {0:?}")]
    InvalidPhase(RealmPhase),
    /// Kernel failure or invalid ioctl completion.
    #[error("{operation:?} failed")]
    Request {
        /// Requested evidence operation.
        operation: EvidenceOperation,
        /// Original typed ioctl failure, including errno and raw TSM code.
        #[source]
        source: TsmRequestError,
    },
    /// The ioctl returned a nonzero TSM code or an invalid/short completion.
    #[error("{operation:?} returned {completion:?} for capacity {capacity}")]
    Completion {
        /// Requested evidence operation.
        operation: EvidenceOperation,
        /// Both nonnegative residue and raw TSM code.
        completion: TsmCompletion,
        /// Offered response bytes.
        capacity: usize,
    },
    /// The size reply is absent, empty, or not a positive Linux `int`.
    #[error("invalid {object:?} size {size}")]
    ObjectSize {
        /// Requested object.
        object: Object,
        /// Raw signed size field.
        size: i32,
    },
    /// No mutation was issued. Access/DMA coordination is not implemented yet.
    #[error("native Realm TDISP mutations are not enabled")]
    MutationsDisabled,
    /// Cleanup stopped at its first failure. The coordinator retains the owner.
    #[error("Realm evidence teardown failed; retained state: {state:?}")]
    Cleanup {
        /// State retained for explicit cleanup retry.
        state: RealmState,
        /// Original cleanup failure.
        #[source]
        source: OperationError,
    },
}

/// Native coordinator error, including bounded snapshot and range failures.
pub type Error = tdisp::host::Error<BackendError>;

/// A rejected ownership transfer. No cleanup or mutation was attempted.
/// A read-only request may have been issued to verify the CCA TDI binding.
#[derive(Debug, thiserror::Error)]
#[error("Realm evidence preparation failed")]
pub struct PrepareError {
    /// Phase validation or read-only binding verification failure.
    #[source]
    pub error: BackendError,
    /// Original owner, retained for attachment or explicit cleanup.
    pub device: RealmDevice,
}

/// Owns an attached Realm device and its native evidence snapshots.
///
/// Create through [`RealmDevice::into_tdisp`]. The supplied snapshot budget must
/// be shared by all assigned devices in the VM. This owner exposes no file
/// descriptor, raw backend, or state-changing request.
///
/// Direct calls are synchronous. Run them away from critical async executor
/// paths, or consume this owner with [`Self::into_evidence_service`] for bounded
/// blocking-pool execution.
/// Call [`Self::teardown`] explicitly, including after failures. Abandoning an
/// owner invokes the underlying Realm object's cleanup fallback; a failed
/// fallback retains its resource bundle until process exit, not clean reuse.
#[must_use = "retain the Realm evidence owner until explicit teardown"]
pub struct RealmTdispDevice {
    coordinator: Coordinator<EvidenceBackend<RealmDevice>>,
}

impl std::fmt::Debug for RealmTdispDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealmTdispDevice")
            .field("state", &self.coordinator.state())
            .finish_non_exhaustive()
    }
}

impl RealmDevice {
    /// Transfer a fresh attached assignment to native evidence coordination.
    ///
    /// A complete CCA certificate-size reply proves that the pinned kernel has
    /// a configured CCA TSM and a bound TDI; attachment alone does not. Successful
    /// CCA binding confirms UNLOCKED, and this exclusive owner has never issued
    /// a TSM mutation. This is binding provenance, not evidence authentication.
    /// Failure returns the entire original owner without cleanup.
    pub fn into_tdisp(self, budget: SnapshotBudget) -> Result<RealmTdispDevice, PrepareError> {
        prepare_backend(self, budget)
            .map(|coordinator| RealmTdispDevice { coordinator })
            .map_err(|(error, device)| PrepareError { error, device })
    }
}

impl RealmTdispDevice {
    /// Consume the verified exclusive owner into bounded asynchronous evidence.
    ///
    /// This preserves the binding checked by [`RealmDevice::into_tdisp`].
    /// The caller retains the strong reference and registers only a weak
    /// reference with its VM. Registration is not automatic.
    ///
    /// Cancellation after admission leaves the worker and sink alive until
    /// completion. An abandoned guest operation must not resume. Explicit
    /// teardown closes admission permanently and permits cleanup retry.
    pub fn into_evidence_service(self) -> Arc<dyn EvidenceService> {
        self.coordinator.into_evidence_service()
    }

    /// Local evidence lifecycle state; not permission to access device BARs or DMA.
    pub fn state(&self) -> DeviceState {
        self.coordinator.state()
    }

    /// Acquire a bounded whole-object snapshot and return its verified size.
    pub fn object_size(&mut self, object: Object) -> Result<usize, Error> {
        self.coordinator.object_size(object)
    }

    /// Serve a checked slice of the same snapshot used by [`Self::object_size`].
    /// Guest offsets never reach the kernel's whole-object read operation.
    pub fn read_object(
        &mut self,
        object: Object,
        offset: u64,
        length: u64,
    ) -> Result<&[u8], Error> {
        self.coordinator.read_object(object, offset, length)
    }

    /// Discard snapshots and close the assignment in dependency order.
    /// Failed cleanup keeps the owner quarantined and permits teardown retry.
    pub fn teardown(&mut self) -> Result<(), Error> {
        self.coordinator.teardown()
    }
}

trait EvidenceDevice {
    fn phase(&self) -> RealmPhase;
    fn request(
        &mut self,
        operation: EvidenceOperation,
        request: CcaTsmRequest<'_>,
        response: &mut [u8],
    ) -> Result<TsmCompletion, BackendError>;
    fn close(&mut self) -> Result<(), BackendError>;
}

impl EvidenceDevice for RealmDevice {
    fn phase(&self) -> RealmPhase {
        self.state().phase
    }

    fn request(
        &mut self,
        operation: EvidenceOperation,
        request: CcaTsmRequest<'_>,
        response: &mut [u8],
    ) -> Result<TsmCompletion, BackendError> {
        self.owner
            .with_attached(|ops, vdevice| ops.context.cca_tsm_request(vdevice, request, response))
            .map_err(BackendError::InvalidPhase)?
            .map_err(|source| BackendError::Request { operation, source })
    }

    fn close(&mut self) -> Result<(), BackendError> {
        RealmDevice::close(self).map_err(|source| BackendError::Cleanup {
            state: self.state(),
            source,
        })
    }
}

struct EvidenceBackend<D> {
    device: D,
}

fn prepare_backend<D: EvidenceDevice>(
    mut device: D,
    budget: SnapshotBudget,
) -> Result<Coordinator<EvidenceBackend<D>>, (BackendError, D)> {
    let phase = device.phase();
    if phase != RealmPhase::Attached {
        return Err((BackendError::InvalidPhase(phase), device));
    }
    if let Err(error) = object_size(&mut device, Object::Certificate) {
        return Err((error, device));
    }
    Ok(Coordinator::new(EvidenceBackend { device }, budget)
        .expect("verified fresh evidence backend reports its known unlocked state without I/O"))
}

fn kernel_object(object: Object) -> CcaObject {
    match object {
        Object::InterfaceReport => CcaObject::InterfaceReport,
        Object::Measurements => CcaObject::Measurement,
        Object::Certificate => CcaObject::Certificate,
        Object::Vca => CcaObject::Vca,
    }
}

fn completed_bytes(
    operation: EvidenceOperation,
    completion: TsmCompletion,
    capacity: usize,
) -> Result<usize, BackendError> {
    if completion.tsm_code == 0 {
        if let Some(bytes) = capacity.checked_sub(completion.residue as usize) {
            return Ok(bytes);
        }
    }
    Err(BackendError::Completion {
        operation,
        completion,
        capacity,
    })
}

impl<D: EvidenceDevice> Backend for EvidenceBackend<D> {
    type Error = BackendError;

    fn state(&mut self) -> Result<ConfirmedState, Self::Error> {
        // Construction verifies the CCA TDI binding of a fresh attached owner;
        // no mutation or alias can change its state before Coordinator::new.
        Ok(ConfirmedState::Unlocked)
    }

    fn set_state(&mut self, _state: ConfirmedState) -> Result<(), Self::Error> {
        Err(BackendError::MutationsDisabled)
    }

    fn object_size(&mut self, object: Object) -> Result<u64, Self::Error> {
        object_size(&mut self.device, object)
    }

    fn read_object(&mut self, object: Object, buffer: &mut [u8]) -> Result<usize, Self::Error> {
        let operation = EvidenceOperation::ReadObject(object);
        let completion = self.device.request(
            operation,
            CcaTsmRequest::ReadObject(kernel_object(object)),
            buffer,
        )?;
        completed_bytes(operation, completion, buffer.len())
    }

    fn regenerate(&mut self, _request: &Regenerate) -> Result<(), Self::Error> {
        Err(BackendError::MutationsDisabled)
    }

    fn reset(&mut self) -> Result<(), Self::Error> {
        Err(BackendError::MutationsDisabled)
    }

    fn teardown(&mut self) -> Result<(), Self::Error> {
        self.device.close()
    }
}

fn object_size(device: &mut impl EvidenceDevice, object: Object) -> Result<u64, BackendError> {
    let operation = EvidenceOperation::ObjectSize(object);
    // A zero-length object can return zero without writing the size field
    // in the pinned kernel. Never accept an untouched response as evidence.
    let mut response = (-1i32).to_ne_bytes();
    let completion = device.request(
        operation,
        CcaTsmRequest::ObjectSize(kernel_object(object)),
        &mut response,
    )?;
    if completed_bytes(operation, completion, response.len())? != response.len() {
        return Err(BackendError::Completion {
            operation,
            completion,
            capacity: response.len(),
        });
    }
    let size = i32::from_ne_bytes(response);
    if size <= 0 {
        return Err(BackendError::ObjectSize { object, size });
    }
    Ok(size as u64)
}

#[cfg(test)]
mod tests;
