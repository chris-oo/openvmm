// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native RHI routing. Registration and full assignment are explicit and pre-run.

use kvm::arm_smccc::KVM_HYPERCALL_EXIT_16BIT_UAPI;
use kvm::arm_smccc::KVM_HYPERCALL_EXIT_SMC_UAPI;
use kvm::arm_smccc::RhiDaFunction;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::Ordering;
use tdisp::host::ConfirmedState;
use tdisp::host::EvidenceError;
use tdisp::host::EvidenceService;
use tdisp::host::EvidenceSink;
use tdisp::host::MAX_OBJECT_SIZE;
use tdisp::host::MeasurementRequest;
use tdisp::host::Object;
use tdisp::host::Regenerate;

const NOT_SUPPORTED: u64 = u64::MAX;
const SUCCESS: u64 = 0;
const INVALID_VDEV_ID: u64 = 3;
const INVALID_OBJECT: u64 = 4;
const INPUT: u64 = 5;
const DEVICE: u64 = 6;
const INVALID_OFFSET: u64 = 7;
const ACCESS_FAILED: u64 = 8;
const EVIDENCE_FEATURES: u64 = (1 << 0) | (1 << 1);
const ASSIGNMENT_FEATURES: u64 = EVIDENCE_FEATURES | (1 << 3) | (1 << 4) | (1 << 5);
const MEASUREMENT_PARAMETER_SIZE: usize = 0x120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MappingRequest {
    pub(crate) rid: u32,
    pub(crate) range: memory_range::MemoryRange,
    pub(crate) pa: u64,
}

fn decode_mapping(
    nr: u64,
    flags: u64,
    rid: u64,
    base: u64,
    top: u64,
    pa: u64,
    shared_bit: u64,
) -> Result<MappingRequest, crate::cca_in_place::CcaInPlaceError> {
    use crate::cca_in_place::CcaInPlaceError;
    let invalid = || CcaInPlaceError::InvalidProtectedMapping;
    if nr != 8 || flags != 0 {
        return Err(invalid());
    }
    let length = top.checked_sub(base).ok_or_else(invalid)?;
    let range = crate::cca_in_place::checked_range(base, length)?;
    crate::cca_in_place::checked_range(pa, length)?;
    if shared_bit < 4096 || !shared_bit.is_power_of_two() || top > shared_bit {
        return Err(invalid());
    }
    Ok(MappingRequest {
        rid: u32::try_from(rid).map_err(|_| invalid())?,
        range,
        pa,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssignmentRequest {
    SetState { rid: u32, state: ConfirmedState },
    InterfaceReport { rid: u32 },
    Measurements { rid: u32, gpa: u64 },
}

fn decode_assignment(nr: u64, flags: u64, args: [u64; 7]) -> Result<AssignmentRequest, u64> {
    if flags & !(KVM_HYPERCALL_EXIT_SMC_UAPI | KVM_HYPERCALL_EXIT_16BIT_UAPI) != 0 {
        return Err(INPUT);
    }
    let function = RhiDaFunction(u32::try_from(nr).map_err(|_| NOT_SUPPORTED)?);
    if !matches!(
        function,
        RhiDaFunction::VDEV_SET_TDI_STATE
            | RhiDaFunction::VDEV_GET_INTERFACE_REPORT
            | RhiDaFunction::VDEV_GET_MEASUREMENTS
    ) {
        return Err(NOT_SUPPORTED);
    }
    let rid = u32::try_from(args[0]).map_err(|_| INVALID_VDEV_ID)?;
    match function {
        RhiDaFunction::VDEV_SET_TDI_STATE => {
            let state = match args[1] {
                0 => ConfirmedState::Unlocked,
                1 => ConfirmedState::Locked,
                2 => ConfirmedState::Running,
                _ => return Err(INPUT),
            };
            Ok(AssignmentRequest::SetState { rid, state })
        }
        RhiDaFunction::VDEV_GET_INTERFACE_REPORT => Ok(AssignmentRequest::InterfaceReport { rid }),
        _ => {
            let gpa = args[1];
            gpa.checked_add(MEASUREMENT_PARAMETER_SIZE as u64)
                .ok_or(INPUT)?;
            Ok(AssignmentRequest::Measurements { rid, gpa })
        }
    }
}

fn measurement_parameters(
    bytes: &[u8; MEASUREMENT_PARAMETER_SIZE],
) -> Result<MeasurementRequest, u64> {
    let mut flags = [0; 8];
    flags.copy_from_slice(&bytes[..8]);
    let flags = u64::from_le_bytes(flags);
    if flags > 1 {
        return Err(INPUT);
    }
    let mut nonce = [0; 32];
    nonce.copy_from_slice(&bytes[0x100..]);
    Ok(MeasurementRequest {
        nonce,
        raw: flags == 1,
    })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RegistrationError {
    #[cfg(guest_arch = "aarch64")]
    #[error("RHI evidence routing requires in-place CCA")]
    Unsupported,
    #[error("RHI evidence registration is frozen")]
    Frozen,
    #[error("native RHI routing is in a failed state")]
    Failed,
    #[error("RHI requester ID {0:#x} is already registered")]
    Duplicate(u32),
    #[error("RHI evidence service has no owner")]
    Expired,
    #[error("RHI filter setup failed; this VM cannot run")]
    Filters(#[source] kvm::Error),
    #[error("only one full native assignment is supported per VM")]
    MultipleAssignments,
    #[error("the service does not implement native assignment")]
    EvidenceOnly,
}

#[derive(Default)]
pub(crate) struct Registry {
    started: bool,
    enabled: bool,
    failed: bool,
    devices: BTreeMap<u32, Weak<dyn EvidenceService>>,
    assignment: Option<u32>,
    assignment_ready: bool,
}

impl Registry {
    fn register(
        &mut self,
        rid: u32,
        service: Weak<dyn EvidenceService>,
        install_filters: impl FnOnce() -> Result<(), kvm::Error>,
    ) -> Result<(), RegistrationError> {
        if self.failed {
            return Err(RegistrationError::Failed);
        }
        if self.started {
            return Err(RegistrationError::Frozen);
        }
        let _owner = service.upgrade().ok_or(RegistrationError::Expired)?;
        if self.devices.get(&rid).and_then(Weak::upgrade).is_some() {
            return Err(RegistrationError::Duplicate(rid));
        }
        if !self.enabled {
            if let Err(error) = install_filters() {
                self.failed = true;
                return Err(RegistrationError::Filters(error));
            }
            self.enabled = true;
        }
        self.devices.insert(rid, service);
        Ok(())
    }

    pub(crate) fn freeze(&mut self) -> Result<(), RegistrationError> {
        self.started = true;
        if self.failed {
            return Err(RegistrationError::Failed);
        }
        Ok(())
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled && !self.failed
    }

    fn lookup(&self, rid: u32) -> Option<Arc<dyn EvidenceService>> {
        self.devices.get(&rid).and_then(Weak::upgrade)
    }

    pub(crate) fn assignment_service(&self) -> Option<Arc<dyn EvidenceService>> {
        self.assignment.and_then(|rid| self.lookup(rid))
    }

    pub(crate) fn assignment_requested(&self) -> bool {
        self.assignment.is_some()
    }

    pub(crate) fn assignment_prepared(&mut self) {
        self.assignment_ready = true;
    }

    fn full_features(&self) -> bool {
        self.assignment_ready && self.enabled() && self.assignment_service().is_some()
    }

    fn register_assignment(
        &mut self,
        rid: u32,
        service: Weak<dyn EvidenceService>,
        install_filters: impl FnOnce() -> Result<(), kvm::Error>,
    ) -> Result<(), RegistrationError> {
        if self.started {
            return Err(RegistrationError::Frozen);
        }
        if self.assignment.is_some() {
            return Err(RegistrationError::MultipleAssignments);
        }
        if !service
            .upgrade()
            .ok_or(RegistrationError::Expired)?
            .supports_assignment()
        {
            return Err(RegistrationError::EvidenceOnly);
        }
        self.register(rid, service, install_filters)?;
        self.assignment = Some(rid);
        Ok(())
    }
}

/// Dropping an unfinished request must prevent re-entry with stale registers
/// or while an admitted worker may still be writing guest memory.
pub(crate) struct RequestGuard<F: FnOnce()> {
    poison: Option<F>,
}

impl<F: FnOnce()> RequestGuard<F> {
    pub(crate) fn new(poison: F) -> Self {
        Self {
            poison: Some(poison),
        }
    }

    pub(crate) fn complete(mut self, result: Result<(), kvm::Error>) -> Result<(), kvm::Error> {
        result?;
        self.poison = None;
        Ok(())
    }
}

impl<F: FnOnce()> Drop for RequestGuard<F> {
    fn drop(&mut self) {
        if let Some(poison) = self.poison.take() {
            poison();
            tracelimit::error_ratelimited!("unfinished RHI request; CCA partition marked fatal");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Features,
    Size {
        rid: u32,
        object: Object,
    },
    Read {
        rid: u32,
        object: Object,
        gpa: u64,
        length: u64,
        offset: u64,
    },
}

fn response(status: u64, value: u64) -> [u64; 4] {
    [status, value, 0, 0]
}

fn decode(nr: u64, flags: u64, args: [u64; 7]) -> Result<Request, u64> {
    if flags & !(KVM_HYPERCALL_EXIT_SMC_UAPI | KVM_HYPERCALL_EXIT_16BIT_UAPI) != 0 {
        return Err(INPUT);
    }
    let function = RhiDaFunction(u32::try_from(nr).map_err(|_| NOT_SUPPORTED)?);
    if function == RhiDaFunction::FEATURES {
        return Ok(Request::Features);
    }
    if !matches!(
        function,
        RhiDaFunction::OBJECT_SIZE | RhiDaFunction::OBJECT_READ
    ) {
        return Err(NOT_SUPPORTED);
    }
    let rid = u32::try_from(args[0]).map_err(|_| INVALID_VDEV_ID)?;
    let object = match args[1] {
        0 => Object::Vca,
        1 => Object::Certificate,
        2 => Object::Measurements,
        3 => Object::InterfaceReport,
        _ => return Err(INVALID_OBJECT),
    };
    if function == RhiDaFunction::OBJECT_SIZE {
        return Ok(Request::Size { rid, object });
    }
    let [_, _, gpa, length, offset, _, _] = args;
    if length == 0 || length > MAX_OBJECT_SIZE as u64 || gpa.checked_add(length).is_none() {
        return Err(INPUT);
    }
    if offset.checked_add(length).is_none() {
        return Err(INVALID_OFFSET);
    }
    Ok(Request::Read {
        rid,
        object,
        gpa,
        length,
        offset,
    })
}

fn service_error(error: EvidenceError) -> [u64; 4] {
    let status = match &error {
        EvidenceError::Unsupported => NOT_SUPPORTED,
        EvidenceError::InvalidRange(_) => INVALID_OFFSET,
        EvidenceError::Access(_) => ACCESS_FAILED,
        EvidenceError::Device(_) | EvidenceError::Closed => DEVICE,
    };
    tracelimit::warn_ratelimited!(
        error = &error as &dyn std::error::Error,
        "RHI evidence request failed"
    );
    response(status, 0)
}

async fn dispatch_assignment(
    request: AssignmentRequest,
    lookup: impl FnOnce(u32) -> Option<Arc<dyn EvidenceService>>,
    read: impl FnOnce(u64, &mut [u8]) -> Result<(), crate::memory::SharedBufferError>,
) -> ([u64; 4], bool) {
    let rid = match request {
        AssignmentRequest::SetState { rid, .. }
        | AssignmentRequest::InterfaceReport { rid }
        | AssignmentRequest::Measurements { rid, .. } => rid,
    };
    let Some(service) = lookup(rid) else {
        return (response(INVALID_VDEV_ID, 0), false);
    };
    let result = match request {
        AssignmentRequest::SetState { state, .. } => service.set_state(state).await,
        AssignmentRequest::InterfaceReport { .. } => {
            service.regenerate(Regenerate::InterfaceReport).await
        }
        AssignmentRequest::Measurements { gpa, .. } => {
            let mut bytes = [0; MEASUREMENT_PARAMETER_SIZE];
            if let Err(error) = read(gpa, &mut bytes) {
                tracelimit::warn_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    "RHI measurement parameter read failed"
                );
                return (response(ACCESS_FAILED, 0), false);
            }
            let parameters = match measurement_parameters(&bytes) {
                Ok(parameters) => parameters,
                Err(status) => return (response(status, 0), false),
            };
            service
                .regenerate(Regenerate::Measurements(parameters))
                .await
        }
    };
    match result {
        Ok(()) => (response(SUCCESS, 0), false),
        Err(error) => {
            let fatal = matches!(error, EvidenceError::Device(_) | EvidenceError::Closed);
            (service_error(error), fatal)
        }
    }
}

async fn dispatch(
    nr: u64,
    full_function: u64,
    flags: u64,
    args: [u64; 7],
    lookup: impl FnOnce(u32) -> Option<Arc<dyn EvidenceService>>,
    sink: impl FnOnce(u64, u64) -> Arc<dyn EvidenceSink>,
) -> [u64; 4] {
    if nr != full_function {
        return response(NOT_SUPPORTED, 0);
    }
    let request = match decode(full_function, flags, args) {
        Ok(request) => request,
        Err(status) => return response(status, 0),
    };
    if request == Request::Features {
        return response(EVIDENCE_FEATURES, 0);
    }
    let rid = match request {
        Request::Size { rid, .. } | Request::Read { rid, .. } => rid,
        Request::Features => unreachable!("features handled above"),
    };
    let Some(service) = lookup(rid) else {
        return response(INVALID_VDEV_ID, 0);
    };
    match request {
        Request::Size { object, .. } => match service.object_size(object).await {
            Ok(size) if size != 0 && size <= MAX_OBJECT_SIZE => response(SUCCESS, size as u64),
            Ok(size) => {
                tracelimit::error_ratelimited!(size, "invalid RHI evidence service size");
                response(DEVICE, 0)
            }
            Err(error) => service_error(error),
        },
        Request::Read {
            object,
            gpa,
            length,
            offset,
            ..
        } => {
            match service
                .read_object(object, offset, length, sink(gpa, length))
                .await
            {
                Ok(count) if count as u64 == length => response(SUCCESS, length),
                Ok(count) => {
                    tracelimit::error_ratelimited!(
                        count,
                        length,
                        "incomplete RHI evidence service read"
                    );
                    response(DEVICE, 0)
                }
                Err(error) => service_error(error),
            }
        }
        Request::Features => unreachable!("features handled above"),
    }
}

#[cfg(guest_arch = "aarch64")]
impl crate::KvmPartitionInner {
    pub(crate) async fn handle_tio(
        self: &Arc<Self>,
        tio: &mut kvm::arm::ArmTioExit<'_>,
    ) -> Result<(), crate::KvmError> {
        tio.reject();
        let guard = RequestGuard::new(|| self.mark_cca_fatal());
        let request = decode_mapping(
            tio.nr.0,
            tio.flags,
            tio.vdev_id,
            tio.gpa_base,
            *tio.gpa_top,
            tio.pa_base,
            self.shared_gpa_bit
                .ok_or(crate::KvmError::InvalidCcaMemoryFault)?,
        )?;
        let service = {
            let registry = self.rhi.lock();
            if !registry.full_features() || registry.assignment != Some(request.rid) {
                return Err(EvidenceError::Unsupported.into());
            }
            registry.assignment_service().ok_or(EvidenceError::Closed)?
        };
        self.record_protected_attempt(request)?;
        service
            .assignment(tdisp::host::AssignmentOperation::ValidateMmio {
                base: request.range.start(),
                top: request.range.end(),
                pa_base: request.pa,
            })
            .await?;
        if self.cca_fatal.load(Ordering::Acquire) {
            return Err(crate::cca_in_place::CcaInPlaceError::AmbiguousFault.into());
        }
        // Zero only permits KVM's independent RMM validation on re-entry.
        // The attempted mapping remains retained, even after later exits.
        *tio.gpa_top = request.range.end();
        tio.accept()
            .map_err(|_| crate::KvmError::InvalidCcaMemoryFault)?;
        guard.complete(Ok(()))?;
        Ok(())
    }

    pub(crate) fn register_rhi_assignment(
        &self,
        rid: u32,
        service: Weak<dyn EvidenceService>,
    ) -> Result<(), RegistrationError> {
        if !self.memory_backing_mode.is_in_place()
            || self.caps.isolation != virt::IsolationType::Cca
        {
            return Err(RegistrationError::Unsupported);
        }
        if self.cca_fatal.load(Ordering::Acquire) {
            return Err(RegistrationError::Failed);
        }
        let mut registry = self.rhi.lock();
        let result = registry
            .register_assignment(rid, service.clone(), || self.kvm.set_arm_rhi_da_filters());
        if registry.failed {
            self.mark_cca_fatal();
        }
        result?;
        // The OnceLock lets fatal paths close admission without acquiring the
        // registry lock while holding the memory ledger or coordinator.
        self.cca_assignment_service
            .set(service)
            .map_err(|_| RegistrationError::MultipleAssignments)?;
        Ok(())
    }

    pub(crate) fn register_rhi_evidence(
        &self,
        rid: u32,
        service: Weak<dyn EvidenceService>,
    ) -> Result<(), RegistrationError> {
        if !self.memory_backing_mode.is_in_place()
            || self.caps.isolation != virt::IsolationType::Cca
        {
            return Err(RegistrationError::Unsupported);
        }
        if self.cca_fatal.load(Ordering::Acquire) {
            return Err(RegistrationError::Failed);
        }
        let mut registry = self.rhi.lock();
        let result = registry.register(rid, service, || self.kvm.set_arm_rhi_da_filters());
        if registry.failed {
            self.mark_cca_fatal();
        }
        if result.is_ok() && self.cca_fatal.load(Ordering::Acquire) {
            registry.failed = true;
            return Err(RegistrationError::Failed);
        }
        result
    }

    pub(crate) async fn handle_rhi(
        self: &Arc<Self>,
        nr: u64,
        full_function: u64,
        flags: u64,
        args: [u64; 7],
    ) -> [u64; 4] {
        if nr != full_function {
            return response(NOT_SUPPORTED, 0);
        }
        if self.rhi.lock().full_features() {
            if nr == u64::from(RhiDaFunction::FEATURES.0) {
                return match decode(nr, flags, args) {
                    Ok(Request::Features) => response(ASSIGNMENT_FEATURES, 0),
                    Err(status) => response(status, 0),
                    _ => response(NOT_SUPPORTED, 0),
                };
            }
            match decode_assignment(full_function, flags, args) {
                Ok(request) => {
                    let (result, fatal) = dispatch_assignment(
                        request,
                        |rid| {
                            let registry = self.rhi.lock();
                            (registry.assignment == Some(rid))
                                .then(|| registry.lookup(rid))
                                .flatten()
                        },
                        |gpa, data| self.read_rhi_shared(gpa, data),
                    )
                    .await;
                    if fatal {
                        self.mark_cca_fatal();
                    }
                    return result;
                }
                Err(NOT_SUPPORTED) => {}
                Err(status) => return response(status, 0),
            }
        }
        struct Sink {
            partition: Arc<crate::KvmPartitionInner>,
            gpa: u64,
            length: u64,
        }
        impl EvidenceSink for Sink {
            fn write(&self, data: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                if data.len() as u64 != self.length {
                    return Err(crate::memory::SharedBufferError::InvalidRange {
                        gpa: self.gpa,
                        length: data.len(),
                    }
                    .into());
                }
                self.partition.write_rhi_shared(self.gpa, data)?;
                Ok(())
            }
        }
        dispatch(
            nr,
            full_function,
            flags,
            args,
            |rid| self.rhi.lock().lookup(rid),
            |gpa, length| {
                Arc::new(Sink {
                    partition: self.clone(),
                    gpa,
                    length,
                })
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests;
