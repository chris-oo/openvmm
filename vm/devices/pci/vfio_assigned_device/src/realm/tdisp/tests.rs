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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    SetState(CcaTdiState),
    Record(u32, std::ops::Range<u64>, u64),
    Map(u64, u64, u64),
    CompleteUnlock(u32),
    Report,
    Measurements(u64, [u8; 32]),
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
    kernel_state: CcaTdiState,
    commit_state_before_error: bool,
    vm_ledger: Vec<std::ops::Range<u64>>,
    gate: std::sync::Weak<AccessGate>,
    record_failure: Option<bool>,
    completion_failure: bool,
    pause: Option<(Call, Pause)>,
    completion_pause: Option<Pause>,
}

struct Pause {
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

impl Pause {
    fn wait(self) {
        self.entered.send(()).unwrap();
        self.resume
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
    }
}

fn pause() -> (
    Pause,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, observed) = std::sync::mpsc::channel();
    let (resume, released) = std::sync::mpsc::channel();
    (
        Pause {
            entered,
            resume: released,
        },
        observed,
        resume,
    )
}

struct Fake(Arc<Mutex<Model>>, Option<Arc<AccessGate>>);

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
            kernel_state: CcaTdiState::Unlocked,
            commit_state_before_error: true,
            vm_ledger: Vec::new(),
            gate: std::sync::Weak::new(),
            record_failure: None,
            completion_failure: false,
            pause: None,
            completion_pause: None,
        }));
        (Self(model.clone(), None), model)
    }
}

impl EvidenceDevice for Fake {
    fn access(&self) -> Option<Arc<AccessGate>> {
        self.1.clone()
    }
    fn phase(&self) -> RealmPhase {
        self.0.lock().phase
    }

    fn assignment(&mut self, operation: AssignmentOperation) -> Result<(), BackendError> {
        let gate = self.access().ok_or(BackendError::MutationsDisabled)?;
        let mut state = gate.state.lock();
        let mut access = AssignmentAccess {
            state: &mut state,
            gate: &gate,
            completed: false,
        };
        let result = if access.state.deny_all {
            Err(AccessError::Quarantined.into())
        } else {
            match operation {
                AssignmentOperation::ValidateMmio { base, top, pa_base } => {
                    validate_mmio(self, access.state, base, top, pa_base)
                }
                _ => Err(BackendError::MutationsDisabled.into()),
            }
        };
        access.completed = result.is_ok();
        result.map_err(BackendError::Assignment)
    }

    fn request(
        &mut self,
        operation: EvidenceOperation,
        request: CcaTsmRequest<'_>,
        response: &mut [u8],
    ) -> Result<TsmCompletion, BackendError> {
        if let Some(gate) = &self.1 {
            if matches!(
                operation,
                EvidenceOperation::SetState(_) | EvidenceOperation::ValidateMmio
            ) {
                assert!(gate.state.try_lock().is_none());
            }
        }
        let mut model = self.0.lock();
        match &request {
            CcaTsmRequest::ObjectSize(object) => model.calls.push(Call::Size(*object)),
            CcaTsmRequest::ReadObject(object) => {
                model.calls.push(Call::Read(*object, response.len()))
            }
            CcaTsmRequest::SetState(state) if self.1.is_some() => {
                model.calls.push(Call::SetState(*state));
                // The kernel can change its software state before returning an error.
                if model.request_error.is_none() || model.commit_state_before_error {
                    model.kernel_state = *state;
                }
            }
            CcaTsmRequest::ValidateMmio {
                gpa_base,
                gpa_top,
                pa_base,
            } if self.1.is_some() => {
                assert!(model.vm_ledger.contains(&(*gpa_base..*gpa_top)));
                model.calls.push(Call::Map(*gpa_base, *gpa_top, *pa_base));
            }
            CcaTsmRequest::RegenerateInterfaceReport if self.1.is_some() => {
                model.calls.push(Call::Report)
            }
            CcaTsmRequest::RegenerateMeasurements { flags, nonce } if self.1.is_some() => {
                model.calls.push(Call::Measurements(*flags, **nonce));
            }
            _ => panic!("evidence backend issued a mutation"),
        }
        let pause = if model
            .pause
            .as_ref()
            .is_some_and(|(call, _)| Some(call) == model.calls.last())
        {
            model.pause.take().map(|(_, pause)| pause)
        } else {
            None
        };
        drop(model);
        if let Some(pause) = pause {
            pause.wait();
        }
        let model = &mut *self.0.lock();
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
            _ if self.1.is_some() => {
                assert!(self.1.as_ref().unwrap().state.try_lock().is_none());
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
fn native_mutations_hold_access_gate_and_preserve_nonce_format() {
    let (mut fake, model) = Fake::new();
    let gate = Arc::new(AccessGate::new(8));
    gate.state.lock().private_ready = true;
    fake.1 = Some(gate.clone());
    let mut owner = prepare_backend(fake, SnapshotBudget::new(8))
        .unwrap_or_else(|(error, _)| panic!("{error}"));
    owner.object_size(Object::Certificate).unwrap();
    owner.set_state(ConfirmedState::Locked).unwrap();
    assert!(gate.state.lock().protected_blocked);
    for raw in [false, true] {
        owner
            .regenerate(Regenerate::Measurements(MeasurementRequest {
                nonce: [0x5a; 32],
                raw,
            }))
            .unwrap();
        assert!(
            model
                .lock()
                .calls
                .contains(&Call::Measurements(u64::from(raw), [0x5a; 32]))
        );
    }
    owner.regenerate(Regenerate::InterfaceReport).unwrap();
    owner.set_state(ConfirmedState::Running).unwrap();
    owner.set_state(ConfirmedState::Unlocked).unwrap();
    assert!(!gate.state.lock().protected_blocked);
}

#[test]
fn every_native_mutation_failure_quarantines_frontend_access() {
    for (residue, tsm_code, syscall) in
        [(1, 0, None), (0, 9, None), (0, 0, Some((Errno::EFAULT, 7)))]
    {
        let (mut fake, model) = Fake::new();
        let gate = Arc::new(AccessGate::new(8));
        gate.state.lock().private_ready = true;
        fake.1 = Some(gate.clone());
        let mut owner = prepare_backend(fake, SnapshotBudget::new(8))
            .unwrap_or_else(|(error, _)| panic!("{error}"));
        model.lock().read_result = TsmCompletion { residue, tsm_code };
        model.lock().request_error = syscall;
        assert!(owner.set_state(ConfirmedState::Locked).is_err());
        assert!(gate.state.lock().deny_all);
        assert!(matches!(owner.state(), DeviceState::Quarantined { .. }));
        let calls = model.lock().calls.len();
        assert!(owner.set_state(ConfirmedState::Unlocked).is_err());
        assert_eq!(calls, model.lock().calls.len());
    }
}

#[test]
fn native_lock_requires_private_import_and_unlock_issues_despite_protected_mappings() {
    let (mut fake, model) = Fake::new();
    let gate = Arc::new(AccessGate::new(8));
    fake.1 = Some(gate.clone());
    let mut backend = EvidenceBackend { device: fake };
    assert!(backend.set_state(ConfirmedState::Locked).is_err());
    assert!(model.lock().calls.is_empty());
    {
        let mut access = gate.state.lock();
        access.deny_all = false;
        access.private_ready = true;
        access.protected.push(0x1000..0x2000);
    }
    backend.set_state(ConfirmedState::Unlocked).unwrap();
    assert_eq!(model.lock().calls, [Call::SetState(CcaTdiState::Unlocked)]);
    assert!(gate.state.lock().protected.is_empty());
    assert!(!gate.state.lock().deny_all);
}

struct LedgerVm(Arc<Mutex<Model>>);

impl pci_core::vfio::VfioVm for LedgerVm {
    fn add_file(&self, _: std::os::fd::BorrowedFd<'_>) -> Result<(), pci_core::vfio::VfioVmError> {
        unreachable!("ledger fixture")
    }
    fn remove_file(
        &self,
        _: std::os::fd::BorrowedFd<'_>,
    ) -> Result<(), pci_core::vfio::VfioVmError> {
        unreachable!("ledger fixture")
    }
    fn check_interrupt_routes(&self) -> Result<(), pci_core::vfio::VfioVmError> {
        Ok(())
    }
    fn record_protected_mapping(
        &self,
        rid: u32,
        range: std::ops::Range<u64>,
        host: u64,
    ) -> Result<(), pci_core::vfio::VfioVmError> {
        let mut model = self.0.lock();
        assert_eq!(rid, 0x1_0100);
        assert!(model.gate.upgrade().unwrap().state.try_lock().is_none());
        model.calls.push(Call::Record(rid, range.clone(), host));
        if model.record_failure != Some(false) {
            assert!(!model.vm_ledger.iter().any(|old| overlaps(old, &range)));
            model.vm_ledger.push(range);
        }
        if model.record_failure.is_some() {
            return Err(pci_core::vfio::VfioVmError::new(std::io::Error::other(
                "record failure",
            )));
        }
        Ok(())
    }
    fn complete_protected_unlock(&self, rid: u32) -> Result<(), pci_core::vfio::VfioVmError> {
        let mut model = self.0.lock();
        assert_eq!(rid, 0x1_0100);
        assert!(model.gate.upgrade().unwrap().state.try_lock().is_none());
        assert_eq!(model.kernel_state, CcaTdiState::Unlocked);
        assert_eq!(
            model.calls.last(),
            Some(&Call::SetState(CcaTdiState::Unlocked))
        );
        model.calls.push(Call::CompleteUnlock(rid));
        let pause = model.completion_pause.take();
        drop(model);
        if let Some(pause) = pause {
            pause.wait();
        }
        let mut model = self.0.lock();
        if model.completion_failure {
            return Err(pci_core::vfio::VfioVmError::new(std::io::Error::other(
                "completion failure",
            )));
        }
        model.vm_ledger.clear();
        Ok(())
    }
}

fn mapping_owner() -> (
    Coordinator<EvidenceBackend<Fake>>,
    Arc<Mutex<Model>>,
    Arc<AccessGate>,
) {
    let (mut fake, model) = Fake::new();
    let gate = Arc::new(AccessGate::with_interrupts(
        0x1_0100,
        Arc::new(LedgerVm(model.clone())),
    ));
    model.lock().gate = Arc::downgrade(&gate);
    {
        let mut state = gate.state.lock();
        state.private_ready = true;
        state.bars.push(super::super::access::BarRange {
            guest: 0x4000..0x8000,
            host: 0x9000,
        });
    }
    fake.1 = Some(gate.clone());
    (
        Coordinator::new(EvidenceBackend { device: fake }, SnapshotBudget::new(8)).unwrap(),
        model,
        gate,
    )
}

fn map_request() -> AssignmentOperation {
    AssignmentOperation::ValidateMmio {
        base: 0x4000,
        top: 0x6000,
        pa_base: 0x9000,
    }
}

#[test]
fn checked_unlock_retires_both_ledgers_before_reopening_and_preserves_ram() {
    let (mut owner, model, gate) = mapping_owner();
    {
        let mut state = gate.state.lock();
        state.shared.insert(
            1 << 40,
            super::super::access::SharedIoasMapping {
                mapping: tdisp::host::SharedMapping {
                    file: Arc::new(std::fs::File::open("/dev/zero").unwrap()),
                    file_offset: 0,
                    iova: 1 << 40,
                    length: 4096,
                },
                memory: Arc::new(sparse_mmap::SparseMapping::new(4096).unwrap()),
                offset: 0,
            },
        );
    }
    for _ in 0..2 {
        owner.set_state(ConfirmedState::Locked).unwrap();
        owner.assignment(map_request()).unwrap();
        assert_eq!(gate.state.lock().protected, [0x4000..0x6000]);
        owner.set_state(ConfirmedState::Running).unwrap();
        owner.set_state(ConfirmedState::Unlocked).unwrap();
        assert!(model.lock().vm_ledger.is_empty());
        let state = gate.state.lock();
        assert!(state.protected.is_empty());
        assert!(!state.protected_blocked && !state.deny_all);
        assert!(state.permits_mmio(0x4000, 4));
        assert!(state.private_ready);
        assert_eq!(state.shared.len(), 1);
    }
    assert_eq!(
        model.lock().calls,
        [
            Call::SetState(CcaTdiState::Locked),
            Call::Record(0x1_0100, 0x4000..0x6000, 0x9000),
            Call::Map(0x4000, 0x6000, 0x9000),
            Call::SetState(CcaTdiState::Run),
            Call::SetState(CcaTdiState::Unlocked),
            Call::CompleteUnlock(0x1_0100),
            Call::SetState(CcaTdiState::Locked),
            Call::Record(0x1_0100, 0x4000..0x6000, 0x9000),
            Call::Map(0x4000, 0x6000, 0x9000),
            Call::SetState(CcaTdiState::Run),
            Call::SetState(CcaTdiState::Unlocked),
            Call::CompleteUnlock(0x1_0100),
        ]
    );
}

#[test]
fn failed_unlock_never_retires_ledgers_even_after_kernel_state_changed() {
    for failure in 0..5 {
        let (mut owner, model, gate) = mapping_owner();
        owner.set_state(ConfirmedState::Locked).unwrap();
        owner.assignment(map_request()).unwrap();
        owner.set_state(ConfirmedState::Running).unwrap();
        match failure {
            0 => {
                model.lock().request_error = Some((Errno::EIO, 0));
                model.lock().commit_state_before_error = false;
            }
            1 => model.lock().request_error = Some((Errno::EFAULT, 17)),
            2 => model.lock().read_result.residue = 1,
            3 => model.lock().read_result.tsm_code = 9,
            _ => model.lock().completion_failure = true,
        }
        let error = owner.set_state(ConfirmedState::Unlocked).unwrap_err();
        match failure {
            0 => assert!(matches!(
                error,
                Error::Backend(BackendError::Request {
                    source: TsmRequestError::Ioctl {
                        errno: Errno::EIO,
                        tsm_code: 0
                    },
                    ..
                })
            )),
            1 => assert!(matches!(
                error,
                Error::Backend(BackendError::Request {
                    source: TsmRequestError::Ioctl {
                        errno: Errno::EFAULT,
                        tsm_code: 17
                    },
                    ..
                })
            )),
            2 | 3 => assert!(matches!(
                error,
                Error::Backend(BackendError::Completion { .. })
            )),
            _ => assert!(matches!(error, Error::Backend(BackendError::Assignment(_)))),
        }
        assert_eq!(
            owner.state(),
            DeviceState::Quarantined {
                last_confirmed: ConfirmedState::Running
            }
        );
        assert_eq!(
            model.lock().kernel_state,
            if failure == 0 {
                CcaTdiState::Run
            } else {
                CcaTdiState::Unlocked
            }
        );
        assert_eq!(model.lock().vm_ledger, [0x4000..0x6000]);
        assert_eq!(
            model.lock().calls.contains(&Call::CompleteUnlock(0x1_0100)),
            failure == 4
        );
        let state = gate.state.lock();
        assert_eq!(state.protected, [0x4000..0x6000]);
        assert!(state.deny_all && state.protected_blocked && state.private_ready);
        assert!(!state.permits_mmio(0x4000, 4));
    }
}

#[test]
fn empty_unlock_still_requires_platform_acknowledgement() {
    let (mut owner, model, gate) = mapping_owner();
    owner.set_state(ConfirmedState::Locked).unwrap();
    model.lock().completion_failure = true;
    assert!(owner.set_state(ConfirmedState::Unlocked).is_err());
    assert_eq!(
        model.lock().calls.last(),
        Some(&Call::CompleteUnlock(0x1_0100))
    );
    assert!(gate.state.lock().deny_all);
}

#[test]
fn partial_recording_failure_retains_attempts_without_mapping_ioctl() {
    for after in [false, true] {
        let (mut owner, model, gate) = mapping_owner();
        owner.set_state(ConfirmedState::Locked).unwrap();
        model.lock().record_failure = Some(after);
        assert!(owner.assignment(map_request()).is_err());
        assert_eq!(gate.state.lock().protected, [0x4000..0x6000]);
        assert_eq!(model.lock().vm_ledger.len(), usize::from(after));
        assert!(
            !model
                .lock()
                .calls
                .iter()
                .any(|call| matches!(call, Call::Map(..)))
        );
        assert!(gate.state.lock().deny_all);
        assert!(matches!(owner.state(), DeviceState::Quarantined { .. }));
    }
}

#[test]
fn fixed_bar_and_hpa_validation_precedes_either_ledger() {
    for operation in [
        AssignmentOperation::ValidateMmio {
            base: 0x3000,
            top: 0x4000,
            pa_base: 0x8000,
        },
        AssignmentOperation::ValidateMmio {
            base: 0x4000,
            top: 0x6000,
            pa_base: 0xa000,
        },
    ] {
        let (mut owner, model, gate) = mapping_owner();
        owner.set_state(ConfirmedState::Locked).unwrap();
        assert!(owner.assignment(operation).is_err());
        assert!(gate.state.lock().protected.is_empty());
        assert!(model.lock().vm_ledger.is_empty());
        assert_eq!(model.lock().calls, [Call::SetState(CcaTdiState::Locked)]);
        assert!(gate.state.lock().deny_all);
    }
}

#[test]
fn mapping_record_and_unlock_retirement_share_whole_device_admission() {
    use std::future::Future;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;

    fn pending<T>(future: &mut std::pin::Pin<Box<dyn Future<Output = T> + Send + '_>>) {
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
    }
    fn entered(receiver: &std::sync::mpsc::Receiver<()>) {
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
    }

    let (owner, model, gate) = mapping_owner();
    let service = owner.into_evidence_service();
    let (hold_lock, lock_entered, release_lock) = pause();
    model.lock().pause = Some((Call::SetState(CcaTdiState::Locked), hold_lock));
    let mut lock = service.set_state(ConfirmedState::Locked);
    pending(&mut lock);
    entered(&lock_entered);
    let mut mapping = service.assignment(map_request());
    pending(&mut mapping);
    assert!(gate.state.try_lock().is_none());
    assert!(model.lock().vm_ledger.is_empty());
    assert_eq!(model.lock().calls, [Call::SetState(CcaTdiState::Locked)]);

    let (hold_map, map_entered, release_map) = pause();
    model.lock().pause = Some((Call::Map(0x4000, 0x6000, 0x9000), hold_map));
    release_lock.send(()).unwrap();
    block_on(lock).unwrap();
    pending(&mut mapping);
    entered(&map_entered);
    assert_eq!(model.lock().vm_ledger, [0x4000..0x6000]);
    assert_eq!(
        model.lock().calls,
        [
            Call::SetState(CcaTdiState::Locked),
            Call::Record(0x1_0100, 0x4000..0x6000, 0x9000),
            Call::Map(0x4000, 0x6000, 0x9000),
        ]
    );
    let mut unlock = service.set_state(ConfirmedState::Unlocked);
    pending(&mut unlock);
    assert_eq!(model.lock().calls.len(), 3);
    assert!(gate.state.try_lock().is_none());

    let (hold_completion, completion_entered, release_completion) = pause();
    model.lock().completion_pause = Some(hold_completion);
    release_map.send(()).unwrap();
    block_on(mapping).unwrap();
    pending(&mut unlock);
    entered(&completion_entered);
    assert_eq!(model.lock().vm_ledger, [0x4000..0x6000]);
    let mut next_lock = service.set_state(ConfirmedState::Locked);
    pending(&mut next_lock);
    assert_eq!(model.lock().calls.len(), 5);
    assert!(gate.state.try_lock().is_none());
    release_completion.send(()).unwrap();
    block_on(unlock).unwrap();
    assert!(model.lock().vm_ledger.is_empty());
    assert!(gate.state.lock().protected.is_empty());
    block_on(next_lock).unwrap();
    block_on(service.assignment(map_request())).unwrap();
    assert_eq!(model.lock().vm_ledger, [0x4000..0x6000]);
    assert_eq!(gate.state.lock().protected, [0x4000..0x6000]);
    assert_eq!(
        model.lock().calls,
        [
            Call::SetState(CcaTdiState::Locked),
            Call::Record(0x1_0100, 0x4000..0x6000, 0x9000),
            Call::Map(0x4000, 0x6000, 0x9000),
            Call::SetState(CcaTdiState::Unlocked),
            Call::CompleteUnlock(0x1_0100),
            Call::SetState(CcaTdiState::Locked),
            Call::Record(0x1_0100, 0x4000..0x6000, 0x9000),
            Call::Map(0x4000, 0x6000, 0x9000),
        ]
    );
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
fn live_service_routes_mutations_and_frontend_failure_invalidates_cached_evidence() {
    let (mut fake, model) = Fake::new();
    let gate = Arc::new(AccessGate::new(8));
    gate.state.lock().private_ready = true;
    fake.1 = Some(gate.clone());
    let budget = SnapshotBudget::new(8);
    let owner =
        prepare_backend(fake, budget.clone()).unwrap_or_else(|(error, _)| panic!("{error}"));
    let service = owner.into_evidence_service();
    assert!(service.supports_assignment());
    block_on(service.set_state(ConfirmedState::Locked)).unwrap();
    block_on(
        service.regenerate(Regenerate::Measurements(MeasurementRequest {
            nonce: [0x13; 32],
            raw: true,
        })),
    )
    .unwrap();
    block_on(service.object_size(Object::Certificate)).unwrap();
    assert_eq!(budget.used(), 8);
    gate.state.lock().deny_all = true;
    assert!(block_on(service.object_size(Object::Certificate)).is_err());
    assert_eq!(budget.used(), 0);
    assert!(
        model
            .lock()
            .calls
            .contains(&Call::Measurements(1, [0x13; 32]))
    );
}

#[test]
fn full_evidence_transport_errors_quarantine_before_returning() {
    for failure in 0..6 {
        let (mut fake, model) = Fake::new();
        let gate = Arc::new(AccessGate::new(8));
        gate.state.lock().private_ready = true;
        fake.1 = Some(gate.clone());
        let mut owner = prepare_backend(fake, SnapshotBudget::new(8))
            .unwrap_or_else(|(error, _)| panic!("{error}"));
        {
            let mut model = model.lock();
            match failure {
                0 => model.request_error = Some((Errno::EIO, 7)),
                1 => model.size_result.tsm_code = 9,
                2 => model.size_result.residue = 1,
                3 => model.read_result.tsm_code = 11,
                4 => model.read_result.residue = 1,
                _ => model.write_size = false,
            }
        }
        assert!(matches!(
            owner.object_size(Object::Certificate),
            Err(Error::Backend(_))
        ));
        assert!(matches!(owner.state(), DeviceState::Quarantined { .. }));
        assert!(gate.state.lock().deny_all);
        let calls = model.lock().calls.len();
        assert!(owner.set_state(ConfirmedState::Locked).is_err());
        assert!(
            owner
                .assignment(AssignmentOperation::PreparePrivateMemory)
                .is_err()
        );
        assert_eq!(model.lock().calls.len(), calls);
    }
}

#[test]
fn full_evidence_failure_permanently_closes_service_admission() {
    let (mut fake, model) = Fake::new();
    let gate = Arc::new(AccessGate::new(8));
    gate.state.lock().private_ready = true;
    fake.1 = Some(gate.clone());
    let owner = prepare_backend(fake, SnapshotBudget::new(8))
        .unwrap_or_else(|(error, _)| panic!("{error}"));
    let service = owner.into_evidence_service();
    gate.bind_service(Arc::downgrade(&service)).unwrap();
    model.lock().read_result.residue = 1;
    assert!(block_on(service.object_size(Object::Certificate)).is_err());
    assert!(matches!(
        block_on(service.set_state(ConfirmedState::Locked)),
        Err(EvidenceError::Closed)
    ));
    assert!(matches!(
        block_on(service.assignment(AssignmentOperation::PreparePrivateMemory)),
        Err(EvidenceError::Closed)
    ));
}

#[test]
fn local_evidence_budget_and_slice_failures_do_not_quarantine() {
    for budget in [4, 8] {
        let (mut fake, _) = Fake::new();
        let gate = Arc::new(AccessGate::new(8));
        gate.state.lock().private_ready = true;
        fake.1 = Some(gate.clone());
        let mut owner = prepare_backend(fake, SnapshotBudget::new(budget))
            .unwrap_or_else(|(error, _)| panic!("{error}"));
        assert!(matches!(
            owner.read_object(Object::Certificate, 9, 1),
            Err(Error::Snapshot(
                SnapshotError::BudgetExceeded { .. } | SnapshotError::InvalidRange { .. }
            ))
        ));
        assert!(!gate.state.lock().deny_all);
        owner.set_state(ConfirmedState::Locked).unwrap();
    }
}

struct PrepareRam {
    ignore_failure: bool,
    fail: bool,
    panic: bool,
}

#[test]
fn tio_host_address_must_match_the_fixed_bar_translation() {
    for pa_base in [0x9000, 0x8001, u64::MAX] {
        let gate = Arc::new(AccessGate::new(8));
        {
            let mut state = gate.state.lock();
            state.protected_blocked = true;
            state.bars.push(super::super::access::BarRange {
                guest: 0x1000..0x3000,
                host: 0x8000,
            });
        }
        let mut device = RealmDevice {
            owner: super::super::objects::ObjectOwner::closed_for_test(),
            access: Some(gate.clone()),
        };
        let error = EvidenceDevice::assignment(
            &mut device,
            AssignmentOperation::ValidateMmio {
                base: 0x1000,
                top: 0x2000,
                pa_base,
            },
        )
        .unwrap_err();
        let BackendError::Assignment(error) = error else {
            panic!("unexpected error")
        };
        assert!(matches!(
            error.downcast_ref::<AccessError>(),
            Some(AccessError::InvalidMmio)
        ));
        let mut state = gate.state.lock();
        assert!(state.protected.is_empty());
        assert!(state.deny_all);
        // This synthetic owner never issued a hardware LOCK.
        state.protected_blocked = false;
    }
}

impl tdisp::host::RamWork for PrepareRam {
    fn run(
        &self,
        dma: &mut dyn tdisp::host::SharedDma,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        dma.prepare_private_memory()?;
        assert!(!self.panic, "injected callback panic");
        if self.ignore_failure {
            assert!(dma.prepare_private_memory().is_err());
        }
        if self.fail {
            return Err(Box::new(std::io::Error::other("conversion failed")));
        }
        Ok(())
    }
}

#[test]
fn ram_work_uses_borrowed_admission_and_latches_ignored_dma_failure() {
    for (ignore_failure, fail) in [(false, false), (true, false), (false, true)] {
        let gate = Arc::new(AccessGate::new(8));
        let mut device = RealmDevice {
            owner: super::super::objects::ObjectOwner::closed_for_test(),
            access: Some(gate.clone()),
        };
        let result = EvidenceDevice::assignment(
            &mut device,
            AssignmentOperation::ConvertRam(Arc::new(PrepareRam {
                ignore_failure,
                fail,
                panic: false,
            })),
        );
        assert_eq!(result.is_err(), ignore_failure || fail);
        let state = gate.state.lock();
        assert!(state.private_ready);
        assert_eq!(state.deny_all, ignore_failure || fail);
    }
}

#[test]
fn ram_work_unwind_revokes_frontend_access() {
    let gate = Arc::new(AccessGate::new(8));
    let mut device = RealmDevice {
        owner: super::super::objects::ObjectOwner::closed_for_test(),
        access: Some(gate.clone()),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        EvidenceDevice::assignment(
            &mut device,
            AssignmentOperation::ConvertRam(Arc::new(PrepareRam {
                ignore_failure: false,
                fail: false,
                panic: true,
            })),
        )
    }));
    assert!(result.is_err());
    assert!(gate.state.lock().deny_all);
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
        Regenerate::Measurements(MeasurementRequest {
            nonce: [0x41; 32],
            raw: false,
        }),
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
