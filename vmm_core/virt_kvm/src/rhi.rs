// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Evidence-only native RHI routing. Registration is explicit and pre-run.

use kvm::arm_smccc::KVM_HYPERCALL_EXIT_16BIT_UAPI;
use kvm::arm_smccc::KVM_HYPERCALL_EXIT_SMC_UAPI;
use kvm::arm_smccc::RhiDaFunction;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::Ordering;
use tdisp::host::EvidenceError;
use tdisp::host::EvidenceService;
use tdisp::host::EvidenceSink;
use tdisp::host::MAX_OBJECT_SIZE;
use tdisp::host::Object;

const NOT_SUPPORTED: u64 = u64::MAX;
const SUCCESS: u64 = 0;
const INVALID_VDEV_ID: u64 = 3;
const INVALID_OBJECT: u64 = 4;
const INPUT: u64 = 5;
const DEVICE: u64 = 6;
const INVALID_OFFSET: u64 = 7;
const ACCESS_FAILED: u64 = 8;
const EVIDENCE_FEATURES: u64 = (1 << 0) | (1 << 1);

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
}

#[derive(Default)]
pub(crate) struct Registry {
    started: bool,
    enabled: bool,
    failed: bool,
    devices: BTreeMap<u32, Weak<dyn EvidenceService>>,
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
