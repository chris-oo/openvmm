// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use futures::executor::block_on;
use nix::errno::Errno;
use parking_lot::Mutex;
use std::sync::Arc;
use tdisp::host::EvidenceError;
use tdisp::host::MeasurementRequest;
use tdisp::host::SnapshotError;
use test_with_tracing::test;

#[derive(Debug, PartialEq, Eq)]
enum Call {
    Size(CcaObject),
    Read(CcaObject, usize),
    Close,
    Drop,
}

struct Model {
    calls: Vec<Call>,
    phase: RealmPhase,
    size: i32,
    write_size: bool,
    contents: Vec<u8>,
    size_result: TsmCompletion,
    read_result: TsmCompletion,
    request_error: Option<(Errno, u64)>,
    close_failures: usize,
    released: bool,
}

struct Fake(Arc<Mutex<Model>>);

impl Fake {
    fn new() -> (Self, Arc<Mutex<Model>>) {
        let model = Arc::new(Mutex::new(Model {
            calls: Vec::new(),
            phase: RealmPhase::Attached,
            size: 8,
            write_size: true,
            contents: (0..8).collect(),
            size_result: TsmCompletion {
                residue: 0,
                tsm_code: 0,
            },
            read_result: TsmCompletion {
                residue: 0,
                tsm_code: 0,
            },
            request_error: None,
            close_failures: 0,
            released: false,
        }));
        (Self(model.clone()), model)
    }
}

impl EvidenceDevice for Fake {
    fn phase(&self) -> RealmPhase {
        self.0.lock().phase
    }

    fn request(
        &mut self,
        operation: EvidenceOperation,
        request: CcaTsmRequest<'_>,
        response: &mut [u8],
    ) -> Result<TsmCompletion, BackendError> {
        let mut model = self.0.lock();
        match &request {
            CcaTsmRequest::ObjectSize(object) => model.calls.push(Call::Size(*object)),
            CcaTsmRequest::ReadObject(object) => {
                model.calls.push(Call::Read(*object, response.len()))
            }
            _ => panic!("evidence backend issued a mutation"),
        }
        if let Some((errno, tsm_code)) = model.request_error {
            return Err(BackendError::Request {
                operation,
                source: TsmRequestError::Ioctl { errno, tsm_code },
            });
        }
        match request {
            CcaTsmRequest::ObjectSize(_) => {
                assert_eq!(response, (-1i32).to_ne_bytes());
                if model.write_size {
                    response.copy_from_slice(&model.size.to_ne_bytes());
                }
                Ok(model.size_result)
            }
            CcaTsmRequest::ReadObject(_) => {
                assert!(response.iter().all(|&byte| byte == 0));
                let len = response.len().min(model.contents.len());
                response[..len].copy_from_slice(&model.contents[..len]);
                Ok(model.read_result)
            }
            _ => panic!("evidence backend issued a mutation"),
        }
    }

    fn close(&mut self) -> Result<(), BackendError> {
        let mut model = self.0.lock();
        model.calls.push(Call::Close);
        model.phase = RealmPhase::Cleaning;
        if model.close_failures != 0 {
            model.close_failures -= 1;
            Err(BackendError::Cleanup {
                state: RealmState {
                    phase: RealmPhase::Cleaning,
                    associated: true,
                    device: Some(10),
                    ioas: Some(20),
                    parent: Some(30),
                    viommu: Some(40),
                    child: Some(50),
                    vdevice: Some(60),
                    requester_id: Some(0x100),
                    attach_attempted: true,
                },
                source: OperationError {
                    operation: super::super::RealmOperation::Detach,
                    source: anyhow::anyhow!("injected detach failure"),
                },
            })
        } else {
            model.phase = RealmPhase::Closed;
            model.released = true;
            Ok(())
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.0.lock().calls.push(Call::Drop);
    }
}

#[test]
fn verified_owner_service_retains_native_cleanup_error_and_retries() {
    use std::error::Error as _;

    let _: fn(RealmTdispDevice) -> Arc<dyn EvidenceService> =
        RealmTdispDevice::into_evidence_service;
    let (device, model) = Fake::new();
    let budget = SnapshotBudget::new(8);
    let coordinator = prepare_backend(device, budget.clone())
        .unwrap_or_else(|(error, _)| panic!("preparation failed: {error}"));
    let service = coordinator.into_evidence_service();
    assert_eq!(
        block_on(service.object_size(Object::Certificate)).unwrap(),
        8
    );
    assert_eq!(
        model.lock().calls,
        [
            Call::Size(CcaObject::Certificate),
            Call::Size(CcaObject::Certificate),
            Call::Read(CcaObject::Certificate, 8),
        ]
    );
    model.lock().close_failures = 1;
    let error = block_on(service.teardown()).unwrap_err();
    assert!(matches!(error, EvidenceError::Device(_)));
    let error = error.source().unwrap().downcast_ref::<Error>().unwrap();
    let backend = error
        .source()
        .unwrap()
        .downcast_ref::<BackendError>()
        .unwrap();
    assert!(matches!(backend, BackendError::Cleanup { .. }));
    assert!(backend.source().unwrap().is::<OperationError>());
    assert_eq!(budget.used(), 0);
    assert!(!model.lock().released);
    assert!(!model.lock().calls.contains(&Call::Drop));
    assert!(matches!(
        block_on(service.object_size(Object::Certificate)),
        Err(EvidenceError::Closed)
    ));
    block_on(service.teardown()).unwrap();
    block_on(service.teardown()).unwrap();
    assert!(model.lock().released);
    assert_eq!(
        model
            .lock()
            .calls
            .iter()
            .filter(|call| **call == Call::Close)
            .count(),
        2
    );
    drop(service);
    assert_eq!(model.lock().calls.last(), Some(&Call::Drop));
}

#[test]
fn all_native_objects_translate_explicitly_and_use_whole_snapshots() {
    for (object, kernel) in [
        (Object::Vca, CcaObject::Vca),
        (Object::Certificate, CcaObject::Certificate),
        (Object::Measurements, CcaObject::Measurement),
        (Object::InterfaceReport, CcaObject::InterfaceReport),
    ] {
        let (device, model) = Fake::new();
        let budget = SnapshotBudget::new(32);
        let mut coordinator = Coordinator::new(EvidenceBackend { device }, budget.clone()).unwrap();
        assert_eq!(
            coordinator.state(),
            DeviceState::Confirmed(ConfirmedState::Unlocked)
        );
        assert_eq!(coordinator.object_size(object).unwrap(), 8);
        model.lock().contents.fill(0xff);
        assert_eq!(coordinator.read_object(object, 2, 3).unwrap(), &[2, 3, 4]);
        assert_eq!(coordinator.read_object(object, 8, 0).unwrap(), &[]);
        assert_eq!(
            model.lock().calls,
            [Call::Size(kernel), Call::Read(kernel, 8)]
        );
        assert_eq!(budget.used(), 8);
        assert!(coordinator.read_object(object, u64::MAX, 1).is_err());
        coordinator.teardown().unwrap();
        assert_eq!(budget.used(), 0);
        assert!(model.lock().released);
        assert!(coordinator.object_size(object).is_err());
    }
}

#[test]
fn absent_empty_negative_and_oversized_objects_never_read() {
    for (write_size, size) in [
        (false, 8),
        (true, 0),
        (true, -1),
        (true, i32::MIN),
        (true, tdisp::host::MAX_OBJECT_SIZE as i32 + 1),
    ] {
        let (device, model) = Fake::new();
        model.lock().write_size = write_size;
        model.lock().size = size;
        let budget = SnapshotBudget::new(tdisp::host::MAX_OBJECT_SIZE);
        let mut coordinator = Coordinator::new(EvidenceBackend { device }, budget.clone()).unwrap();
        let error = coordinator.object_size(Object::Certificate).unwrap_err();
        if size > tdisp::host::MAX_OBJECT_SIZE as i32 {
            assert!(matches!(error, Error::Snapshot(SnapshotError::TooLarge(_))));
        } else {
            assert!(matches!(
                error,
                Error::Backend(BackendError::ObjectSize { .. })
            ));
        }
        assert_eq!(model.lock().calls, [Call::Size(CcaObject::Certificate)]);
        assert_eq!(budget.used(), 0);
    }
}

#[test]
fn size_requires_complete_reply_and_zero_tsm_status() {
    for completion in [
        TsmCompletion {
            residue: 1,
            tsm_code: 0,
        },
        TsmCompletion {
            residue: 4,
            tsm_code: 0,
        },
        TsmCompletion {
            residue: 5,
            tsm_code: 0,
        },
        TsmCompletion {
            residue: 0,
            tsm_code: 9,
        },
    ] {
        let (device, model) = Fake::new();
        model.lock().size_result = completion;
        let mut backend = EvidenceBackend { device };
        let error = backend.object_size(Object::Vca).unwrap_err();
        let BackendError::Completion {
            operation,
            completion: actual,
            capacity,
        } = error
        else {
            panic!("wrong error")
        };
        assert_eq!(operation, EvidenceOperation::ObjectSize(Object::Vca));
        assert_eq!(actual, completion);
        assert_eq!(capacity, 4);
    }
}

#[test]
fn short_or_failed_reads_never_produce_a_snapshot() {
    for completion in [
        TsmCompletion {
            residue: 3,
            tsm_code: 0,
        },
        TsmCompletion {
            residue: 8,
            tsm_code: 0,
        },
        TsmCompletion {
            residue: 9,
            tsm_code: 0,
        },
        TsmCompletion {
            residue: 0,
            tsm_code: 5,
        },
    ] {
        let (device, model) = Fake::new();
        model.lock().read_result = completion;
        let budget = SnapshotBudget::new(32);
        let mut coordinator = Coordinator::new(EvidenceBackend { device }, budget.clone()).unwrap();
        let error = coordinator.object_size(Object::Vca).unwrap_err();
        if completion.tsm_code == 0 && completion.residue <= 8 {
            assert!(matches!(
                error,
                Error::Snapshot(SnapshotError::IncoherentRead { .. })
            ));
        } else {
            assert!(matches!(
                error,
                Error::Backend(BackendError::Completion { .. })
            ));
        }
        assert_eq!(budget.used(), 0);
    }
}

#[test]
fn failed_acquisition_invalidates_previous_objects() {
    let (device, model) = Fake::new();
    let budget = SnapshotBudget::new(32);
    let mut coordinator = Coordinator::new(EvidenceBackend { device }, budget.clone()).unwrap();
    coordinator.object_size(Object::Vca).unwrap();
    model.lock().size_result.residue = 1;
    assert!(coordinator.object_size(Object::Certificate).is_err());
    assert_eq!(budget.used(), 0);
    model.lock().size_result.residue = 0;
    model.lock().contents.fill(0xa5);
    assert_eq!(
        coordinator.read_object(Object::Vca, 0, 2).unwrap(),
        &[0xa5; 2]
    );
    assert_eq!(budget.used(), 8);
}

#[test]
fn ioctl_errno_and_tsm_code_survive_the_native_coordinator() {
    let (device, model) = Fake::new();
    model.lock().request_error = Some((Errno::EFAULT, 17));
    let budget = SnapshotBudget::new(32);
    let mut coordinator = Coordinator::new(EvidenceBackend { device }, budget.clone()).unwrap();
    let error = coordinator.object_size(Object::Vca).unwrap_err();
    let Error::Backend(BackendError::Request {
        operation,
        source: TsmRequestError::Ioctl { errno, tsm_code },
    }) = error
    else {
        panic!("lost typed syscall failure")
    };
    assert_eq!(operation, EvidenceOperation::ObjectSize(Object::Vca));
    assert_eq!(errno, Errno::EFAULT);
    assert_eq!(tsm_code, 17);
    assert_eq!(budget.used(), 0);
}

#[test]
fn attached_without_cca_binding_retains_owner_instead_of_claiming_unlocked() {
    let (device, model) = Fake::new();
    model.lock().request_error = Some((Errno::ENXIO, 0));
    let budget = SnapshotBudget::new(8);
    let (error, device) = match prepare_backend(device, budget.clone()) {
        Err(failure) => failure,
        Ok(_) => panic!("attachment without TSM binding must not create a coordinator"),
    };
    assert!(matches!(
        error,
        BackendError::Request {
            operation: EvidenceOperation::ObjectSize(Object::Certificate),
            source: TsmRequestError::Ioctl {
                errno: Errno::ENXIO,
                tsm_code: 0
            },
        }
    ));
    assert_eq!(device.phase(), RealmPhase::Attached);
    assert_eq!(model.lock().calls, [Call::Size(CcaObject::Certificate)]);
    assert!(!model.lock().released);
    assert_eq!(budget.used(), 0);

    model.lock().request_error = None;
    let mut coordinator = match prepare_backend(device, budget.clone()) {
        Ok(coordinator) => coordinator,
        Err(_) => panic!("verified binding should create a coordinator"),
    };
    assert_eq!(
        coordinator.state(),
        DeviceState::Confirmed(ConfirmedState::Unlocked)
    );
    assert_eq!(
        model.lock().calls,
        [
            Call::Size(CcaObject::Certificate),
            Call::Size(CcaObject::Certificate)
        ]
    );
    assert_eq!(budget.used(), 0);
    coordinator.teardown().unwrap();
    assert!(model.lock().released);
}

#[test]
fn ownership_transfer_rejects_other_phases_without_io() {
    for phase in [
        RealmPhase::New,
        RealmPhase::Prepared,
        RealmPhase::Cleaning,
        RealmPhase::Closed,
    ] {
        let (device, model) = Fake::new();
        model.lock().phase = phase;
        let (error, device) = match prepare_backend(device, SnapshotBudget::new(8)) {
            Err(failure) => failure,
            Ok(_) => panic!("nonattached owner must not create a coordinator"),
        };
        assert!(matches!(error, BackendError::InvalidPhase(actual) if actual == phase));
        assert_eq!(device.phase(), phase);
        assert!(model.lock().calls.is_empty());
    }
}

#[test]
fn binding_verification_requires_a_complete_present_object_size() {
    for (write_size, size, completion) in [
        (
            false,
            8,
            TsmCompletion {
                residue: 0,
                tsm_code: 0,
            },
        ),
        (
            true,
            0,
            TsmCompletion {
                residue: 0,
                tsm_code: 0,
            },
        ),
        (
            true,
            -1,
            TsmCompletion {
                residue: 0,
                tsm_code: 0,
            },
        ),
        (
            true,
            8,
            TsmCompletion {
                residue: 1,
                tsm_code: 0,
            },
        ),
        (
            true,
            8,
            TsmCompletion {
                residue: 0,
                tsm_code: 5,
            },
        ),
    ] {
        let (device, model) = Fake::new();
        model.lock().write_size = write_size;
        model.lock().size = size;
        model.lock().size_result = completion;
        let (_error, device) = match prepare_backend(device, SnapshotBudget::new(8)) {
            Err(failure) => failure,
            Ok(_) => panic!("unverified binding must not create a coordinator"),
        };
        assert_eq!(device.phase(), RealmPhase::Attached);
        assert_eq!(model.lock().calls, [Call::Size(CcaObject::Certificate)]);
        assert!(!model.lock().released);
    }
}

#[test]
fn shared_budget_is_released_by_ordered_teardown() {
    let (device, first_model) = Fake::new();
    let budget = SnapshotBudget::new(8);
    let mut first = Coordinator::new(EvidenceBackend { device }, budget.clone()).unwrap();
    let (device, second_model) = Fake::new();
    let mut second = Coordinator::new(EvidenceBackend { device }, budget.clone()).unwrap();
    first.object_size(Object::Vca).unwrap();
    assert!(matches!(
        second.object_size(Object::Vca),
        Err(Error::Snapshot(SnapshotError::BudgetExceeded { .. }))
    ));
    assert_eq!(second_model.lock().calls, [Call::Size(CcaObject::Vca)]);
    first.teardown().unwrap();
    assert!(first_model.lock().released);
    assert_eq!(second.object_size(Object::Vca).unwrap(), 8);
    second.teardown().unwrap();
    assert_eq!(budget.used(), 0);
}

#[test]
fn mutation_methods_do_not_issue_ioctls() {
    let (device, model) = Fake::new();
    let mut backend = EvidenceBackend { device };
    for state in [
        ConfirmedState::Unlocked,
        ConfirmedState::Locked,
        ConfirmedState::Running,
    ] {
        assert!(matches!(
            backend.set_state(state),
            Err(BackendError::MutationsDisabled)
        ));
    }
    for request in [
        Regenerate::InterfaceReport,
        Regenerate::Measurements(MeasurementRequest { nonce: [0x41; 32] }),
    ] {
        assert!(matches!(
            backend.regenerate(&request),
            Err(BackendError::MutationsDisabled)
        ));
    }
    assert!(matches!(
        backend.reset(),
        Err(BackendError::MutationsDisabled)
    ));
    assert!(model.lock().calls.is_empty());
}

#[test]
fn failed_cleanup_retains_owner_and_can_retry() {
    let (device, model) = Fake::new();
    model.lock().close_failures = 1;
    let budget = SnapshotBudget::new(8);
    let mut coordinator = Coordinator::new(EvidenceBackend { device }, budget.clone()).unwrap();
    coordinator.object_size(Object::Vca).unwrap();
    let error = coordinator.teardown().unwrap_err();
    assert!(
        matches!(error, Error::Backend(BackendError::Cleanup { state, .. }) if state.phase == RealmPhase::Cleaning)
    );
    assert_eq!(
        coordinator.state(),
        DeviceState::Quarantined {
            last_confirmed: ConfirmedState::Unlocked
        }
    );
    assert!(!model.lock().released);
    assert!(!model.lock().calls.contains(&Call::Drop));
    assert_eq!(budget.used(), 0);
    assert!(coordinator.object_size(Object::Vca).is_err());
    coordinator.teardown().unwrap();
    assert_eq!(coordinator.state(), DeviceState::TornDown);
    assert!(model.lock().released);
    assert!(!model.lock().calls.contains(&Call::Drop));
    drop(coordinator);
    assert_eq!(model.lock().calls.last(), Some(&Call::Drop));
}
