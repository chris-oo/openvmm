// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use parking_lot::Mutex;
use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use tdisp::host::SnapshotError;
use test_with_tracing::test;

fn ready<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test service must complete synchronously"),
    }
}

async fn dispatch(
    nr: u64,
    flags: u64,
    args: [u64; 7],
    lookup: impl FnOnce(u32) -> Option<Arc<dyn EvidenceService>>,
    sink: impl FnOnce(u64, u64) -> Arc<dyn EvidenceSink>,
) -> [u64; 4] {
    super::dispatch(nr, nr, flags, args, lookup, sink).await
}

#[derive(Clone, Copy)]
enum Failure {
    Device,
    Access,
    Range,
    Closed,
}

struct Service {
    closed: AtomicBool,
    assignment: bool,
    states: Mutex<Vec<ConfirmedState>>,
    regenerations: Mutex<Vec<Regenerate>>,
    size: usize,
    count: usize,
    failure: Option<Failure>,
    calls: Mutex<Vec<Object>>,
    drops: Arc<AtomicUsize>,
}

impl Service {
    fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            assignment: false,
            states: Mutex::new(Vec::new()),
            regenerations: Mutex::new(Vec::new()),
            size: 8,
            count: 3,
            failure: None,
            calls: Mutex::new(Vec::new()),
            drops: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn check(&self) -> Result<(), EvidenceError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(EvidenceError::Closed);
        }
        match self.failure {
            None => Ok(()),
            Some(Failure::Device) => Err(EvidenceError::Device(
                std::io::Error::other("device").into(),
            )),
            Some(Failure::Access) => {
                Err(EvidenceError::Access(std::io::Error::other("copy").into()))
            }
            Some(Failure::Range) => Err(EvidenceError::InvalidRange(SnapshotError::InvalidRange {
                offset: 9,
                length: 3,
                size: 8,
            })),
            Some(Failure::Closed) => Err(EvidenceError::Closed),
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl EvidenceService for Service {
    fn close_admission(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn supports_assignment(&self) -> bool {
        self.assignment
    }

    async fn set_state(&self, state: ConfirmedState) -> Result<(), EvidenceError> {
        self.check()?;
        self.states.lock().push(state);
        Ok(())
    }

    async fn regenerate(&self, request: Regenerate) -> Result<(), EvidenceError> {
        self.check()?;
        self.regenerations.lock().push(request);
        Ok(())
    }

    async fn object_size(&self, object: Object) -> Result<usize, EvidenceError> {
        self.check()?;
        self.calls.lock().push(object);
        Ok(self.size)
    }

    async fn read_object(
        &self,
        object: Object,
        offset: u64,
        length: u64,
        sink: Arc<dyn EvidenceSink>,
    ) -> Result<usize, EvidenceError> {
        self.check()?;
        self.calls.lock().push(object);
        assert_eq!(offset, 2);
        assert_eq!(length, 3);
        sink.write(&[2, 3, 4]).map_err(EvidenceError::Access)?;
        Ok(self.count)
    }

    async fn teardown(&self) -> Result<(), EvidenceError> {
        self.close_admission();
        Ok(())
    }
}

struct Sink(Mutex<Vec<u8>>, bool);

impl EvidenceSink for Sink {
    fn write(&self, data: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.lock().extend_from_slice(data);
        if self.1 {
            Err(std::io::Error::other("failed after copy").into())
        } else {
            Ok(())
        }
    }
}

fn service() -> Arc<dyn EvidenceService> {
    Arc::new(Service::new())
}

#[test]
fn fatal_request_closes_admission_before_pending_mutations_can_start() {
    let service = Arc::new(Service::new());
    let pending_state = service.set_state(ConfirmedState::Running);
    let pending_regeneration = service.regenerate(Regenerate::InterfaceReport);
    let pending_size = service.object_size(Object::Certificate);
    let sink = Arc::new(Sink(Mutex::new(Vec::new()), false));
    let pending_read = service.read_object(Object::Certificate, 2, 3, sink.clone());
    drop(RequestGuard::new(|| service.close_admission()));
    assert!(service.closed.load(Ordering::SeqCst));
    assert!(matches!(ready(pending_state), Err(EvidenceError::Closed)));
    assert!(matches!(
        ready(pending_regeneration),
        Err(EvidenceError::Closed)
    ));
    assert!(matches!(ready(pending_size), Err(EvidenceError::Closed)));
    assert!(matches!(ready(pending_read), Err(EvidenceError::Closed)));
    assert!(service.states.lock().is_empty());
    assert!(service.regenerations.lock().is_empty());
    assert!(service.calls.lock().is_empty());
    assert!(sink.0.lock().is_empty());
    ready(service.teardown()).unwrap();
    assert!(matches!(
        ready(service.set_state(ConfirmedState::Unlocked)),
        Err(EvidenceError::Closed)
    ));
}

#[test]
fn tio_decode_checks_full_range_reason_flags_id_and_address_views() {
    let request = decode_mapping(8, 0, 0x100, 0x4000, 0x8000, 0x9000, 1 << 40).unwrap();
    assert_eq!(request.rid, 0x100);
    assert_eq!(
        request.range,
        memory_range::MemoryRange::new(0x4000..0x8000)
    );
    assert_eq!(request.pa, 0x9000);
    for (nr, flags, rid, base, top, pa, bit) in [
        (9, 0, 0, 0x4000, 0x8000, 0x9000, 1 << 40),
        (8, 1, 0, 0x4000, 0x8000, 0x9000, 1 << 40),
        (8, 0, 1 << 32, 0x4000, 0x8000, 0x9000, 1 << 40),
        (8, 0, 0, 0x8000, 0x4000, 0x9000, 1 << 40),
        (8, 0, 0, 0x4000, 0x4000, 0x9000, 1 << 40),
        (8, 0, 0, 0x4001, 0x8000, 0x9000, 1 << 40),
        (8, 0, 0, 1 << 40, (1 << 40) + 4096, 0x9000, 1 << 40),
        (8, 0, 0, 0x4000, 0x8000, u64::MAX - 4095, 1 << 40),
        (8, 0, 0, 0x4000, 0x8000, 0x9001, 1 << 40),
    ] {
        assert!(decode_mapping(nr, flags, rid, base, top, pa, bit).is_err());
    }
}

#[test]
fn assignment_registration_does_not_advertise_before_ram_ready() {
    let mut registry = Registry::default();
    let evidence = service();
    assert!(matches!(
        registry.register_assignment(1, Arc::downgrade(&evidence), || panic!("evidence filter")),
        Err(RegistrationError::EvidenceOnly)
    ));
    let mut service = Service::new();
    service.assignment = true;
    let service: Arc<dyn EvidenceService> = Arc::new(service);
    registry
        .register_assignment(1, Arc::downgrade(&service), || Ok(()))
        .unwrap();
    assert!(!registry.full_features());
    assert!(registry.assignment_requested());
    assert!(matches!(
        registry.register_assignment(2, Arc::downgrade(&service), || panic!("second assignment")),
        Err(RegistrationError::MultipleAssignments)
    ));
    registry.assignment_prepared();
    assert!(registry.full_features());
    assert_eq!(ASSIGNMENT_FEATURES, 0x3b);
    drop(service);
    assert!(!registry.full_features());
}

#[test]
fn assignment_decoder_keeps_full_width_and_known_states() {
    let function = u64::from(RhiDaFunction::VDEV_SET_TDI_STATE.0);
    for (value, state) in [
        (0, ConfirmedState::Unlocked),
        (1, ConfirmedState::Locked),
        (2, ConfirmedState::Running),
    ] {
        assert_eq!(
            decode_assignment(function, 0, [0x100, value, 0, 0, 0, 0, 0]),
            Ok(AssignmentRequest::SetState { rid: 0x100, state })
        );
    }
    for value in [3, 1 << 32, u64::MAX] {
        assert_eq!(
            decode_assignment(function, 0, [0x100, value, 0, 0, 0, 0, 0]),
            Err(INPUT)
        );
    }
    assert_eq!(
        decode_assignment(function | (1 << 32), 0, [0; 7]),
        Err(NOT_SUPPORTED)
    );
    assert_eq!(
        decode_assignment(function, 0, [1 << 32, 0, 0, 0, 0, 0, 0]),
        Err(INVALID_VDEV_ID)
    );
    assert_eq!(decode_assignment(function, 4, [0; 7]), Err(INPUT));
}

#[test]
fn measurement_parameters_use_nonce_at_0x100_and_only_hash_or_raw_flags() {
    let mut bytes = [0xa5; MEASUREMENT_PARAMETER_SIZE];
    bytes[0x100..].fill(0x73);
    for (flags, raw) in [(0u64, false), (1, true)] {
        bytes[..8].copy_from_slice(&flags.to_le_bytes());
        let parameters = measurement_parameters(&bytes).unwrap();
        assert_eq!(parameters.nonce, [0x73; 32]);
        assert_eq!(parameters.raw, raw);
    }
    for flags in [2u64, 1 << 32, u64::MAX] {
        bytes[..8].copy_from_slice(&flags.to_le_bytes());
        assert_eq!(measurement_parameters(&bytes), Err(INPUT));
    }
}

#[test]
fn native_mutations_share_one_service_and_own_measurement_input() {
    let mut native = Service::new();
    native.assignment = true;
    let native = Arc::new(native);
    for request in [
        AssignmentRequest::SetState {
            rid: 7,
            state: ConfirmedState::Locked,
        },
        AssignmentRequest::InterfaceReport { rid: 7 },
        AssignmentRequest::Measurements {
            rid: 7,
            gpa: 0x2000,
        },
    ] {
        assert_eq!(
            ready(dispatch_assignment(
                request,
                |_| Some(native.clone()),
                |gpa, bytes| {
                    assert_eq!(gpa, 0x2000);
                    assert_eq!(bytes.len(), 0x120);
                    bytes[..8].copy_from_slice(&1u64.to_le_bytes());
                    bytes[0x100..].fill(0x42);
                    Ok(())
                }
            )),
            (response(SUCCESS, 0), false)
        );
    }
    assert_eq!(*native.states.lock(), [ConfirmedState::Locked]);
    assert_eq!(
        *native.regenerations.lock(),
        [
            Regenerate::InterfaceReport,
            Regenerate::Measurements(MeasurementRequest {
                nonce: [0x42; 32],
                raw: true
            }),
        ]
    );
}

#[test]
fn native_mutation_failure_requires_vm_quarantine() {
    let mut native = Service::new();
    native.failure = Some(Failure::Device);
    assert_eq!(
        ready(dispatch_assignment(
            AssignmentRequest::SetState {
                rid: 7,
                state: ConfirmedState::Running
            },
            |_| Some(Arc::new(native)),
            |_, _| panic!("state change must not read guest memory"),
        )),
        (response(DEVICE, 0), true)
    );
}

#[test]
fn registration_is_weak_and_filters_install_once() {
    let mut registry = Registry::default();
    let first = Service::new();
    let drops = first.drops.clone();
    let first: Arc<dyn EvidenceService> = Arc::new(first);
    registry
        .register(0x100, Arc::downgrade(&first), || Ok(()))
        .unwrap();
    assert!(registry.enabled());
    assert!(Arc::ptr_eq(&registry.lookup(0x100).unwrap(), &first));
    assert!(matches!(
        registry.register(0x100, Arc::downgrade(&first), || panic!("duplicate filter")),
        Err(RegistrationError::Duplicate(0x100))
    ));
    let second = service();
    registry
        .register(0x200, Arc::downgrade(&second), || {
            panic!("filters installed twice")
        })
        .unwrap();
    drop(first);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(registry.lookup(0x100).is_none());
    registry.freeze().unwrap();
    assert!(matches!(
        registry.register(0x100, Arc::downgrade(&second), || panic!("frozen")),
        Err(RegistrationError::Frozen)
    ));
}

#[test]
fn empty_freeze_and_partial_filter_failure_never_reopen() {
    let service = service();
    let mut empty = Registry::default();
    empty.freeze().unwrap();
    assert!(!empty.enabled());
    assert!(matches!(
        empty.register(0x100, Arc::downgrade(&service), || panic!("late install")),
        Err(RegistrationError::Frozen)
    ));

    let mut registry = Registry::default();
    let mut filters = Vec::new();
    let error = registry
        .register(0x100, Arc::downgrade(&service), || {
            filters.push("first installed");
            filters.push("second failed");
            Err(kvm::Error::MissingCapability("injected second filter"))
        })
        .unwrap_err();
    assert!(matches!(error, RegistrationError::Filters(_)));
    assert_eq!(filters.len(), 2);
    assert!(!registry.enabled());
    assert!(registry.lookup(0x100).is_none());
    assert!(matches!(registry.freeze(), Err(RegistrationError::Failed)));
    assert!(matches!(
        registry.register(0x100, Arc::downgrade(&service), || panic!("retry")),
        Err(RegistrationError::Failed)
    ));
}

#[test]
fn registration_and_first_run_share_one_freeze_lock() {
    let registry = Mutex::new(Registry::default());
    let service = service();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (checked_tx, checked_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let registry = &registry;
        scope.spawn(move || {
            started_rx.recv().unwrap();
            assert!(registry.try_lock().is_none());
            checked_tx.send(()).unwrap();
            registry.lock().freeze().unwrap();
        });
        registry
            .lock()
            .register(0x100, Arc::downgrade(&service), || {
                started_tx.send(()).unwrap();
                checked_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
    });
    assert!(registry.lock().started);
    assert!(registry.lock().enabled());
}

#[test]
fn expired_service_does_not_enable_filters() {
    let service = service();
    let weak = Arc::downgrade(&service);
    drop(service);
    let mut registry = Registry::default();
    assert!(matches!(
        registry.register(0x100, weak, || panic!("expired registration")),
        Err(RegistrationError::Expired)
    ));
    assert!(!registry.enabled());
}

#[test]
fn full_function_and_argument_values_are_not_truncated() {
    let size = RhiDaFunction::OBJECT_SIZE.0 as u64;
    for nr in [
        size | (1 << 32),
        0xc400004c,
        0x8500004c,
        0xc500004e,
        0xc5000050,
        0xc5000051,
        0xc5000052,
        0xc5000053,
        0xc5000054,
        u64::MAX,
    ] {
        assert_eq!(decode(nr, 0, [0; 7]), Err(NOT_SUPPORTED));
    }
    for flags in 0..4 {
        assert_eq!(
            decode(size, flags, [0x100, 0, 0, 0, 0, 0, 0]),
            Ok(Request::Size {
                rid: 0x100,
                object: Object::Vca
            })
        );
    }
    assert_eq!(decode(size, 4, [0; 7]), Err(INPUT));
    assert_eq!(decode(size, 1 << 63, [0; 7]), Err(INPUT));
    assert_eq!(
        decode(size, 0, [1 << 32, 0, 0, 0, 0, 0, 0]),
        Err(INVALID_VDEV_ID)
    );
    for object in [4, 1 << 32, u64::MAX] {
        assert_eq!(
            decode(size, 0, [0, object, 0, 0, 0, 0, 0]),
            Err(INVALID_OBJECT)
        );
    }
    for (id, object) in [
        (0, Object::Vca),
        (1, Object::Certificate),
        (2, Object::Measurements),
        (3, Object::InterfaceReport),
    ] {
        assert_eq!(
            decode(size, 0, [u32::MAX as u64, id, 0, 0, 0, 0, 0]),
            Ok(Request::Size {
                rid: u32::MAX,
                object
            })
        );
    }
}

#[test]
fn narrowed_exit_numbers_do_not_hide_full_x0_bits() {
    for function in [
        RhiDaFunction::FEATURES,
        RhiDaFunction::OBJECT_SIZE,
        RhiDaFunction::OBJECT_READ,
    ] {
        let nr = u64::from(function.0);
        assert_eq!(
            ready(super::dispatch(
                nr,
                nr | (1 << 32),
                2,
                [0; 7],
                |_| panic!("narrowed function reached service"),
                |_, _| panic!("narrowed function reached memory")
            )),
            [NOT_SUPPORTED, 0, 0, 0]
        );
    }
    assert_eq!(
        ready(super::dispatch(
            u64::from(RhiDaFunction::OBJECT_SIZE.0),
            u64::from(RhiDaFunction::OBJECT_READ.0),
            0,
            [0; 7],
            |_| panic!("mismatched function reached service"),
            |_, _| panic!("mismatched function reached memory")
        )),
        [NOT_SUPPORTED, 0, 0, 0]
    );
}

#[test]
fn features_return_only_evidence_bits_directly_in_x0() {
    let result = ready(dispatch(
        RhiDaFunction::FEATURES.0 as u64,
        2,
        [0; 7],
        |_| panic!("features must not access devices"),
        |_, _| panic!("features must not access memory"),
    ));
    assert_eq!(result, [3, 0, 0, 0]);
    assert_eq!(result[0] & ((1 << 2) | (1 << 3) | (1 << 4) | (1 << 5)), 0);
}

#[test]
fn read_argument_order_bounds_and_result_counts() {
    let service: Arc<dyn EvidenceService> = Arc::new(Service::new());
    let sink = Arc::new(Sink(Mutex::new(Vec::new()), false));
    let nr = RhiDaFunction::OBJECT_READ.0 as u64;
    let result = ready(dispatch(
        nr,
        0,
        [0x100, 1, 0x2fff, 3, 2, 0, 0],
        |rid| {
            assert_eq!(rid, 0x100);
            Some(service)
        },
        |gpa, length| {
            assert_eq!((gpa, length), (0x2fff, 3));
            sink.clone()
        },
    ));
    assert_eq!(result, [SUCCESS, 3, 0, 0]);
    assert_eq!(*sink.0.lock(), [2, 3, 4]);
    for (gpa, length, offset, status) in [
        (0, 0, 0, INPUT),
        (0, MAX_OBJECT_SIZE as u64 + 1, 0, INPUT),
        (u64::MAX, 2, 0, INPUT),
        (0, 1, u64::MAX, INVALID_OFFSET),
    ] {
        assert_eq!(
            decode(nr, 0, [0x100, 0, gpa, length, offset, 0, 0]),
            Err(status)
        );
    }
}

#[test]
fn missing_devices_and_service_errors_zero_other_results() {
    let nr = RhiDaFunction::OBJECT_SIZE.0 as u64;
    assert_eq!(
        ready(dispatch(
            nr,
            0,
            [0x100, 0, 0, 0, 0, 0, 0],
            |_| None,
            |_, _| panic!("no device")
        )),
        [INVALID_VDEV_ID, 0, 0, 0]
    );
    for (failure, status) in [
        (Failure::Device, DEVICE),
        (Failure::Access, ACCESS_FAILED),
        (Failure::Range, INVALID_OFFSET),
        (Failure::Closed, DEVICE),
    ] {
        let mut service = Service::new();
        service.failure = Some(failure);
        assert_eq!(
            ready(dispatch(
                nr,
                0,
                [0x100, 0, 0, 0, 0, 0, 0],
                |_| Some(Arc::new(service)),
                |_, _| panic!("size memory")
            )),
            [status, 0, 0, 0]
        );
    }
}

#[test]
fn bad_service_lengths_and_partial_copy_do_not_report_success() {
    for size in [0, MAX_OBJECT_SIZE + 1] {
        let mut service = Service::new();
        service.size = size;
        assert_eq!(
            ready(dispatch(
                RhiDaFunction::OBJECT_SIZE.0 as u64,
                0,
                [0; 7],
                |_| Some(Arc::new(service)),
                |_, _| panic!("size memory")
            )),
            [DEVICE, 0, 0, 0]
        );
    }
    for (count, fault, expected) in [
        (2, false, DEVICE),
        (4, false, DEVICE),
        (3, true, ACCESS_FAILED),
    ] {
        let mut service = Service::new();
        service.count = count;
        let sink = Arc::new(Sink(Mutex::new(Vec::new()), fault));
        assert_eq!(
            ready(dispatch(
                RhiDaFunction::OBJECT_READ.0 as u64,
                0,
                [0x100, 0, 0x1000, 3, 2, 0, 0],
                |_| Some(Arc::new(service)),
                |_, _| sink.clone()
            )),
            [expected, 0, 0, 0]
        );
        assert_eq!(*sink.0.lock(), [2, 3, 4]);
    }
}

#[test]
fn abandoned_requests_and_partial_register_writes_are_fatal() {
    let fatal = AtomicBool::new(false);
    let kicks = AtomicUsize::new(0);
    drop(RequestGuard::new(|| {
        fatal.store(true, Ordering::Release);
        kicks.fetch_add(1, Ordering::Relaxed);
    }));
    assert!(fatal.load(Ordering::Acquire));
    assert_eq!(kicks.load(Ordering::Relaxed), 1);
    for failed_at in 0..4 {
        let fatal = AtomicBool::new(false);
        let kicks = AtomicUsize::new(0);
        let guard = RequestGuard::new(|| {
            fatal.store(true, Ordering::Release);
            kicks.fetch_add(1, Ordering::Relaxed);
        });
        let mut written = Vec::new();
        let result = (|| {
            for register in 0..4 {
                if register == failed_at {
                    return Err(kvm::Error::MissingCapability("injected register write"));
                }
                written.push(register);
            }
            Ok(())
        })();
        assert!(guard.complete(result).is_err());
        assert!(fatal.load(Ordering::Acquire));
        assert_eq!(kicks.load(Ordering::Relaxed), 1);
        assert_eq!(written.len(), failed_at);
    }
    let fatal = AtomicBool::new(false);
    RequestGuard::new(|| {
        fatal.store(true, Ordering::Release);
        panic!("completed request must not poison or kick peers");
    })
    .complete(Ok(()))
    .unwrap();
    assert!(!fatal.load(Ordering::Acquire));
}
