// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native TDISP access through an exclusively owned Realm assignment.
//!
//! Evidence-only preparation exposes no mutations. The production Realm
//! resolver supplies the shared frontend gate, enabling typed state changes,
//! regeneration and tracked memory operations on the same coordinator.
//! Protected mappings require kernel-completed DEV-to-EMPTY invalidation;
//! forced unmap and function reset are unsupported. Failed teardown retains
//! the owner and its backing. Destruction requires checked UNLOCKED completion;
//! the pinned kernel's implicit unlock during destruction discards errors.
//! Full-assignment evidence transport failures revoke access and admission.
//! Snapshot budget and slice validation failures alone do not quarantine.

use super::OperationError;
use super::RealmDevice;
use super::RealmPhase;
use super::RealmState;
use super::access::AccessError;
use super::access::AccessGate;
use super::access::AccessState;
use super::access::interval;
use super::access::overlaps;
use std::sync::Arc;
use tdisp::host::AssignmentOperation;
use tdisp::host::Backend;
use tdisp::host::ConfirmedState;
use tdisp::host::Coordinator;
use tdisp::host::DeviceState;
use tdisp::host::EvidenceService;
use tdisp::host::Object;
use tdisp::host::Regenerate;
use tdisp::host::SnapshotBudget;
use vfio_sys::iommufd::tsm::CcaObject;
use vfio_sys::iommufd::tsm::CcaTdiState;
use vfio_sys::iommufd::tsm::CcaTsmRequest;
use vfio_sys::iommufd::tsm::TsmCompletion;
use vfio_sys::iommufd::tsm::TsmRequestError;

/// Native evidence operation, for error reporting without protocol casts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceOperation {
    /// Change native device state.
    SetState(ConfirmedState),
    /// Regenerate an evidence object.
    Regenerate,
    /// Install protected device memory.
    ValidateMmio,
    /// Query the whole object's size.
    ObjectSize(Object),
    /// Read the whole object at offset zero.
    ReadObject(Object),
}

/// Failure from the Linux evidence backend, with all result channels retained.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// Checked assignment access or IOAS operation failed.
    #[error("Realm assignment access failed")]
    Assignment(#[source] anyhow::Error),
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
    /// No mutation was issued because the owner has no live frontend gate.
    #[error("native Realm TDISP mutations require a coordinated frontend")]
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
/// descriptor or raw backend. Live services are enabled only by the Realm PCI
/// resolver, which supplies the checked frontend gate.
///
/// Direct calls are synchronous. Run them away from critical async executor
/// paths, or consume this owner with [`Self::into_evidence_service`] for bounded
/// blocking-pool execution.
/// Call [`Self::teardown`] explicitly, including after failures. Abandoning an
/// owner invokes the underlying Realm object's cleanup fallback; a failed
/// fallback retains its resource bundle for the process lifetime, not clean
/// reuse. The process and DMA-reachable RAM must remain alive until the external
/// device/model domain ends. Process exit does not prove that DMA stopped.
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
    /// Consume the verified owner into bounded asynchronous device operations.
    ///
    /// This preserves the binding checked by [`RealmDevice::into_tdisp`].
    /// Assignment support is available only when the resolver supplied a gate.
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
    fn access(&self) -> Option<Arc<AccessGate>> {
        None
    }

    fn assignment(&mut self, _operation: AssignmentOperation) -> Result<(), BackendError> {
        Err(BackendError::MutationsDisabled)
    }
    fn phase(&self) -> RealmPhase;
    fn request(
        &mut self,
        operation: EvidenceOperation,
        request: CcaTsmRequest<'_>,
        response: &mut [u8],
    ) -> Result<TsmCompletion, BackendError>;
    fn close(&mut self) -> Result<(), BackendError>;
}

impl RealmDevice {
    fn assignment_locked(
        &mut self,
        state: &mut AccessState,
        operation: AssignmentOperation,
    ) -> anyhow::Result<()> {
        if state.deny_all {
            return Err(AccessError::Quarantined.into());
        }
        match operation {
            AssignmentOperation::ConvertRam(work) => {
                let mut dma = RealmSharedDma {
                    device: self,
                    state,
                    failed: false,
                };
                work.run(&mut dma).map_err(anyhow::Error::from_boxed)?;
                if dma.failed {
                    return Err(AccessError::Quarantined.into());
                }
            }
            AssignmentOperation::Quarantine => {
                return Err(AccessError::Quarantined.into());
            }
            AssignmentOperation::ValidateMmio { base, top, pa_base } => {
                let range = base..top;
                let host = state.mmio_host(&range)?;
                if host != pa_base {
                    return Err(AccessError::InvalidMmio.into());
                }
                // Retain even an uncertain or partially installed interval.
                state.protected.push(range);
                let completion = self.request(
                    EvidenceOperation::ValidateMmio,
                    CcaTsmRequest::ValidateMmio {
                        gpa_base: base,
                        gpa_top: top,
                        pa_base: host,
                    },
                    &mut [],
                )?;
                mutation_complete(EvidenceOperation::ValidateMmio, completion)?;
            }
            AssignmentOperation::InvalidateMmio { base, top } => {
                // The caller only submits this after kernel DEV->EMPTY
                // completion. This is bookkeeping, not an invented ioctl.
                state.invalidate_mmio(base..top)?;
            }
            AssignmentOperation::PreparePrivateMemory => {
                if state.private_ready || !state.shared.is_empty() {
                    return Err(AccessError::InvalidShared.into());
                }
                state.private_ready = true;
            }
            AssignmentOperation::MapShared(mapping) => {
                if !state.private_ready {
                    return Err(AccessError::PrivateNotReady.into());
                }
                let range =
                    interval(mapping.iova, mapping.length).ok_or(AccessError::InvalidShared)?;
                if interval(mapping.file_offset, mapping.length).is_none()
                    || state
                        .shared
                        .values()
                        .any(|m| overlaps(&range, &(m.iova..m.iova + m.length)))
                {
                    return Err(AccessError::InvalidShared.into());
                }
                let ioas = self.state().ioas.ok_or(AccessError::InvalidShared)?;
                let length =
                    usize::try_from(mapping.length).map_err(|_| AccessError::InvalidShared)?;
                let memory = Arc::new(sparse_mmap::SparseMapping::new(length)?);
                memory.map_file(0, length, mapping.file.as_ref(), mapping.file_offset, true)?;
                // Map at the conversion granule so later subrange unmaps
                // never split a kernel IOAS mapping.
                for offset in (0..mapping.length).step_by(super::access::GRANULE as usize) {
                    let part = tdisp::host::SharedMapping {
                        file: mapping.file.clone(),
                        file_offset: mapping.file_offset + offset,
                        iova: mapping.iova + offset,
                        length: super::access::GRANULE,
                    };
                    // The ioctl can commit before a copyback error.
                    let retained = super::access::SharedIoasMapping {
                        mapping: part.clone(),
                        memory: memory.clone(),
                        offset: usize::try_from(offset).map_err(|_| AccessError::InvalidShared)?,
                    };
                    let address = retained.memory.as_ptr().wrapping_add(retained.offset) as u64;
                    state.shared.insert(part.iova, retained);
                    let result = self
                        .owner
                        .with_attached(|ops, _| {
                            // UNSAFETY: The ledger retains the backed VMA through acknowledged unmap.
                            #[expect(unsafe_code)]
                            // SAFETY: This shared file range is mapped above, and the
                            // ledger owns both its VMA and file before the ioctl. Failed
                            // or partial requests retain them with the assignment.
                            unsafe {
                                ops.context
                                    .ioas_map(ioas, part.iova, address, part.length, true)
                            }
                        })
                        .map_err(|phase| anyhow::anyhow!("invalid assignment phase {phase:?}"))?;
                    if let Err(error) = result {
                        tracelimit::error_ratelimited!(
                            ioas, iova = part.iova, file_offset = part.file_offset,
                            length = part.length, error = %error.root_cause(),
                            "Realm shared DMA map failed"
                        );
                        return Err(error);
                    }
                }
            }
            AssignmentOperation::UnmapShared { iova, length } => {
                let keys = state.shared_unmap_keys(iova, length)?;
                let ioas = self.state().ioas.ok_or(AccessError::InvalidShared)?;
                let actual = self
                    .owner
                    .with_attached(|ops, _| ops.context.ioas_unmap(ioas, iova, length))
                    .map_err(|phase| anyhow::anyhow!("invalid assignment phase {phase:?}"))??;
                anyhow::ensure!(actual == length, "short IOAS unmap: {actual} of {length}");
                for key in keys {
                    state.shared.remove(&key);
                }
            }
        }
        Ok(())
    }
}

struct AssignmentAccess<'a> {
    state: &'a mut AccessState,
    gate: &'a AccessGate,
    completed: bool,
}

impl Drop for AssignmentAccess<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.state.deny_all = true;
            self.gate.close_admission();
        }
    }
}

struct RealmSharedDma<'a> {
    device: &'a mut RealmDevice,
    state: &'a mut AccessState,
    failed: bool,
}

impl RealmSharedDma<'_> {
    fn perform(
        &mut self,
        operation: AssignmentOperation,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let result = self.device.assignment_locked(self.state, operation);
        if result.is_err() {
            self.failed = true;
            self.state.deny_all = true;
        }
        result.map_err(anyhow::Error::into_boxed_dyn_error)
    }
}

impl tdisp::host::SharedDma for RealmSharedDma<'_> {
    fn map(
        &mut self,
        mapping: tdisp::host::SharedMapping,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.perform(AssignmentOperation::MapShared(mapping))
    }

    fn unmap(
        &mut self,
        iova: u64,
        length: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.perform(AssignmentOperation::UnmapShared { iova, length })
    }

    fn prepare_private_memory(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.perform(AssignmentOperation::PreparePrivateMemory)
    }
}

impl EvidenceDevice for RealmDevice {
    fn access(&self) -> Option<Arc<AccessGate>> {
        self.access.clone()
    }

    fn assignment(&mut self, operation: AssignmentOperation) -> Result<(), BackendError> {
        let gate = self.access().ok_or(BackendError::MutationsDisabled)?;
        let mut state = gate.state.lock();
        let mut access = AssignmentAccess {
            state: &mut state,
            gate: &gate,
            completed: false,
        };
        let result = self.assignment_locked(access.state, operation);
        access.completed = result.is_ok();
        result.map_err(BackendError::Assignment)
    }

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
        if let Some(gate) = &self.access {
            let mut state = gate.state.lock();
            state.deny_all = true;
            gate.check_interrupts(&mut state)
                .map_err(|error| BackendError::Assignment(error.into()))?;
            state
                .quiescent_for_cleanup()
                .map_err(|error| BackendError::Assignment(error.into()))?;
            let ioas = self.state().ioas;
            while let Some((&iova, mapping)) = state.shared.first_key_value() {
                let length = mapping.length;
                let ioas = ioas
                    .ok_or_else(|| BackendError::Assignment(AccessError::InvalidShared.into()))?;
                let actual = self
                    .owner
                    .with_attached(|ops, _| ops.context.ioas_unmap(ioas, iova, length))
                    .map_err(BackendError::InvalidPhase)?
                    .map_err(BackendError::Assignment)?;
                if actual != length {
                    return Err(BackendError::Assignment(anyhow::anyhow!(
                        "short IOAS cleanup: {actual} of {length}"
                    )));
                }
                state.shared.remove(&iova);
            }
        }
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

    fn check_access(&self) -> Result<(), Self::Error> {
        if let Some(gate) = self.device.access() {
            let mut state = gate.state.lock();
            gate.check_interrupts(&mut state)
                .map_err(|error| BackendError::Assignment(error.into()))?;
            if let Some(error) = &state.io_error {
                return Err(BackendError::Assignment(
                    AccessError::DeviceIo(error.clone()).into(),
                ));
            }
            if state.deny_all {
                return Err(BackendError::Assignment(AccessError::Quarantined.into()));
            }
        }
        Ok(())
    }

    fn supports_assignment(&self) -> bool {
        self.device.access().is_some()
    }

    fn assignment(&mut self, operation: AssignmentOperation) -> Option<Result<(), Self::Error>> {
        Some(self.device.assignment(operation))
    }

    fn state(&mut self) -> Result<ConfirmedState, Self::Error> {
        // Construction verifies the CCA TDI binding of a fresh attached owner;
        // no mutation or alias can change its state before Coordinator::new.
        Ok(ConfirmedState::Unlocked)
    }

    fn set_state(&mut self, target: ConfirmedState) -> Result<(), Self::Error> {
        let gate = self
            .device
            .access()
            .ok_or(BackendError::MutationsDisabled)?;
        let mut state = gate.state.lock();
        if state.deny_all {
            return Err(BackendError::Assignment(AccessError::Quarantined.into()));
        }
        state.deny_all = true;
        state.protected_blocked = true;
        if !state.private_ready {
            return Err(BackendError::Assignment(
                AccessError::PrivateNotReady.into(),
            ));
        }
        if target == ConfirmedState::Unlocked && !state.protected.is_empty() {
            return Err(BackendError::Assignment(
                AccessError::ProtectedMappingsRemain.into(),
            ));
        }
        let operation = EvidenceOperation::SetState(target);
        let target = match target {
            ConfirmedState::Unlocked => CcaTdiState::Unlocked,
            ConfirmedState::Locked => CcaTdiState::Locked,
            ConfirmedState::Running => CcaTdiState::Run,
        };
        let completion =
            self.device
                .request(operation, CcaTsmRequest::SetState(target), &mut [])?;
        mutation_complete(operation, completion)?;
        state.protected_blocked = target != CcaTdiState::Unlocked;
        state.deny_all = false;
        tracelimit::info_ratelimited!(
            requester_id = gate.requester_id,
            ?target,
            "Realm TDISP state confirmed"
        );
        Ok(())
    }

    fn object_size(&mut self, object: Object) -> Result<u64, Self::Error> {
        checked_evidence_read(&mut self.device, |device| object_size(device, object))
    }

    fn read_object(&mut self, object: Object, buffer: &mut [u8]) -> Result<usize, Self::Error> {
        let full_assignment = self.device.access().is_some();
        let operation = EvidenceOperation::ReadObject(object);
        checked_evidence_read(&mut self.device, |device| {
            let completion = device.request(
                operation,
                CcaTsmRequest::ReadObject(kernel_object(object)),
                buffer,
            )?;
            let actual = completed_bytes(operation, completion, buffer.len())?;
            if full_assignment && actual != buffer.len() {
                return Err(BackendError::Completion {
                    operation,
                    completion,
                    capacity: buffer.len(),
                });
            }
            Ok(actual)
        })
    }

    fn regenerate(&mut self, request: &Regenerate) -> Result<(), Self::Error> {
        let gate = self
            .device
            .access()
            .ok_or(BackendError::MutationsDisabled)?;
        let mut state = gate.state.lock();
        if state.deny_all {
            return Err(BackendError::Assignment(AccessError::Quarantined.into()));
        }
        state.deny_all = true;
        let request = match request {
            Regenerate::InterfaceReport => CcaTsmRequest::RegenerateInterfaceReport,
            Regenerate::Measurements(request) => CcaTsmRequest::RegenerateMeasurements {
                flags: u64::from(request.raw),
                nonce: &request.nonce,
            },
        };
        let completion = self
            .device
            .request(EvidenceOperation::Regenerate, request, &mut [])?;
        mutation_complete(EvidenceOperation::Regenerate, completion)?;
        state.deny_all = false;
        Ok(())
    }

    fn reset(&mut self) -> Result<(), Self::Error> {
        Err(BackendError::MutationsDisabled)
    }

    fn teardown(&mut self) -> Result<(), Self::Error> {
        self.device.close()
    }
}

fn checked_evidence_read<D: EvidenceDevice, R>(
    device: &mut D,
    call: impl FnOnce(&mut D) -> Result<R, BackendError>,
) -> Result<R, BackendError> {
    let Some(gate) = device.access() else {
        return call(device);
    };
    let mut state = gate.state.lock();
    gate.check_interrupts(&mut state)
        .map_err(|error| BackendError::Assignment(error.into()))?;
    let mut access = AssignmentAccess {
        state: &mut state,
        gate: &gate,
        completed: false,
    };
    if access.state.deny_all {
        return Err(BackendError::Assignment(AccessError::Quarantined.into()));
    }
    let result = call(device);
    access.completed = result.is_ok();
    result
}

fn mutation_complete(
    operation: EvidenceOperation,
    completion: TsmCompletion,
) -> Result<(), BackendError> {
    if completion.residue != 0 || completion.tsm_code != 0 {
        return Err(BackendError::Completion {
            operation,
            completion,
            capacity: 0,
        });
    }
    Ok(())
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
