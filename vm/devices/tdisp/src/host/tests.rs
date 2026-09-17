// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use test_with_tracing::test;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    State,
    SetState(ConfirmedState),
    Size(Object),
    Read(Object),
    Regenerate,
    Reset,
    Teardown,
}

#[derive(Debug, thiserror::Error)]
#[error("injected backend failure")]
struct Failure;

struct FakeState {
    state: ConfirmedState,
    sizes: [u64; 4],
    actual: Option<usize>,
    calls: Vec<Call>,
    fail: Option<Call>,
    byte: u8,
    nonce: Option<[u8; 32]>,
    budget: SnapshotBudget,
}

struct Fake(Arc<Mutex<FakeState>>);

impl Fake {
    fn call(&mut self, call: Call) -> Result<(), Failure> {
        let mut state = self.0.lock();
        state.calls.push(call);
        if state.fail == Some(call) {
            Err(Failure)
        } else {
            Ok(())
        }
    }

    fn mutation(&mut self, call: Call) -> Result<(), Failure> {
        let mut state = self.0.lock();
        // These mutation tests use one device: invalidation must precede issue.
        assert_eq!(state.budget.used(), 0);
        state.byte = state.byte.wrapping_add(1);
        drop(state);
        self.call(call)
    }
}

impl Backend for Fake {
    type Error = Failure;

    fn state(&mut self) -> Result<ConfirmedState, Failure> {
        self.call(Call::State)?;
        Ok(self.0.lock().state)
    }

    fn set_state(&mut self, state: ConfirmedState) -> Result<(), Failure> {
        // Simulate a device committing before a later transport failure.
        self.0.lock().state = state;
        self.mutation(Call::SetState(state))
    }

    fn object_size(&mut self, object: Object) -> Result<u64, Failure> {
        self.call(Call::Size(object))?;
        Ok(self.0.lock().sizes[object.index()])
    }

    fn read_object(&mut self, object: Object, bytes: &mut [u8]) -> Result<usize, Failure> {
        self.call(Call::Read(object))?;
        let state = self.0.lock();
        assert_eq!(bytes.len() as u64, state.sizes[object.index()]);
        assert!(bytes.iter().all(|&byte| byte == 0));
        bytes.fill(state.byte + object.index() as u8);
        Ok(state.actual.unwrap_or(bytes.len()))
    }

    fn regenerate(&mut self, request: &Regenerate) -> Result<(), Failure> {
        if let Regenerate::Measurements(request) = request {
            self.0.lock().nonce = Some(request.nonce);
        }
        self.mutation(Call::Regenerate)
    }

    fn reset(&mut self) -> Result<(), Failure> {
        self.0.lock().state = ConfirmedState::Unlocked;
        self.mutation(Call::Reset)
    }

    fn teardown(&mut self) -> Result<(), Failure> {
        self.mutation(Call::Teardown)
    }
}

fn device(
    initial: ConfirmedState,
    budget: &SnapshotBudget,
    size: u64,
) -> (Coordinator<Fake>, Arc<Mutex<FakeState>>) {
    let state = Arc::new(Mutex::new(FakeState {
        state: initial,
        sizes: [size; 4],
        actual: None,
        calls: Vec::new(),
        fail: None,
        byte: 0x40,
        nonce: None,
        budget: budget.clone(),
    }));
    let coordinator = Coordinator::new(Fake(state.clone()), budget.clone()).unwrap();
    (coordinator, state)
}

#[test]
fn object_boundaries() {
    for size in [1, MAX_OBJECT_SIZE as u64 - 1, MAX_OBJECT_SIZE as u64] {
        let budget = SnapshotBudget::new(MAX_OBJECT_SIZE);
        let (mut device, backend) = device(ConfirmedState::Unlocked, &budget, size);
        assert_eq!(device.object_size(Object::Vca).unwrap() as u64, size);
        assert_eq!(
            device.read_object(Object::Vca, 0, size).unwrap(),
            vec![0x43; size as usize]
        );
        assert_eq!(budget.used(), size as usize);
        assert_eq!(
            backend.lock().calls,
            [
                Call::State,
                Call::Size(Object::Vca),
                Call::Read(Object::Vca)
            ]
        );
        assert!(device.read_object(Object::Vca, size, 0).unwrap().is_empty());
        drop(device);
        assert_eq!(budget.used(), 0);
    }
    for size in [MAX_OBJECT_SIZE as u64 + 1, u32::MAX as u64, u64::MAX] {
        let budget = SnapshotBudget::new(MAX_OBJECT_SIZE);
        let (mut device, backend) = device(ConfirmedState::Unlocked, &budget, size);
        assert!(matches!(
            device.object_size(Object::Vca),
            Err(Error::Snapshot(SnapshotError::TooLarge(n))) if n == size
        ));
        assert_eq!(backend.lock().calls, [Call::State, Call::Size(Object::Vca)]);
        assert_eq!(budget.used(), 0);
    }
}

#[test]
fn incoherent_reads_release_all_snapshots() {
    for actual in [0, 3, 5, usize::MAX] {
        let budget = SnapshotBudget::new(8);
        let (mut device, backend) = device(ConfirmedState::Locked, &budget, 4);
        device.object_size(Object::Certificate).unwrap();
        backend.lock().actual = Some(actual);
        assert!(matches!(
            device.object_size(Object::InterfaceReport),
            Err(Error::Snapshot(SnapshotError::IncoherentRead { expected: 4, actual: n }))
                if n == actual
        ));
        assert_eq!(budget.used(), 0);
        backend.lock().actual = None;
        device.object_size(Object::Certificate).unwrap();
        assert_eq!(
            backend
                .lock()
                .calls
                .iter()
                .filter(|&&call| call == Call::Read(Object::Certificate))
                .count(),
            2
        );
    }
}

#[test]
fn shared_budget_exhaustion_and_release() {
    let budget = SnapshotBudget::new(7);
    let (mut first, _) = device(ConfirmedState::Unlocked, &budget, 4);
    let (mut second, second_backend) = device(ConfirmedState::Unlocked, &budget, 4);
    first.object_size(Object::Measurements).unwrap();
    assert!(matches!(
        second.object_size(Object::Measurements),
        Err(Error::Snapshot(SnapshotError::BudgetExceeded {
            requested: 4,
            available: 3
        }))
    ));
    assert_eq!(budget.used(), 4);
    assert_eq!(
        second_backend.lock().calls,
        [Call::State, Call::Size(Object::Measurements)]
    );
    drop(first);
    assert_eq!(budget.used(), 0);
    second.object_size(Object::Measurements).unwrap();
    assert_eq!(budget.used(), 4);
    second.reset().unwrap();
    assert_eq!(budget.used(), 0);

    let zero_budget = SnapshotBudget::new(0);
    let (mut blocked, _) = device(ConfirmedState::Unlocked, &zero_budget, 1);
    assert!(matches!(
        blocked.object_size(Object::Measurements),
        Err(Error::Snapshot(SnapshotError::BudgetExceeded {
            requested: 1,
            available: 0
        }))
    ));
}

#[test]
fn empty_object_is_not_a_valid_snapshot() {
    let budget = SnapshotBudget::new(4);
    let (mut device, backend) = device(ConfirmedState::Locked, &budget, 4);
    device.object_size(Object::Certificate).unwrap();
    backend.lock().sizes[Object::Vca.index()] = 0;
    for _ in 0..2 {
        assert!(matches!(
            device.object_size(Object::Vca),
            Err(Error::Snapshot(SnapshotError::EmptyObject))
        ));
        assert_eq!(budget.used(), 0);
        assert_eq!(
            device.state(),
            DeviceState::Confirmed(ConfirmedState::Locked)
        );
    }
    assert_eq!(
        backend.lock().calls,
        [
            Call::State,
            Call::Size(Object::Certificate),
            Call::Read(Object::Certificate),
            Call::Size(Object::Vca),
            Call::Size(Object::Vca),
        ]
    );
}

#[test]
fn allocation_failure_releases_reservation() {
    let budget = SnapshotBudget::new(usize::MAX);
    // Exercise the allocator's capacity-overflow path without exhausting the host.
    // Public object acquisition rejects this size before reaching allocation.
    assert!(matches!(
        Snapshot::allocate(usize::MAX, &budget),
        Err(SnapshotError::Allocation(_))
    ));
    assert_eq!(budget.used(), 0);
}

#[test]
fn checked_slices_preserve_one_snapshot() {
    let budget = SnapshotBudget::new(16);
    let (mut device, backend) = device(ConfirmedState::Locked, &budget, 4);
    // A read without a prior size request acquires a full snapshot.
    assert_eq!(device.read_object(Object::Vca, 1, 2).unwrap(), [0x43; 2]);
    backend.lock().byte = 0x50;
    for (offset, length) in [(u64::MAX, 1), (1, u64::MAX), (5, 0), (4, 1), (0, 5)] {
        assert!(matches!(
            device.read_object(Object::Vca, offset, length),
            Err(Error::Snapshot(SnapshotError::InvalidRange { .. }))
        ));
    }
    assert_eq!(device.object_size(Object::Vca).unwrap(), 4);
    assert_eq!(device.read_object(Object::Vca, 0, 4).unwrap(), [0x43; 4]);
    assert_eq!(
        device.read_object(Object::Certificate, 0, 4).unwrap(),
        [0x52; 4]
    );
    assert_eq!(backend.lock().calls.len(), 5);
    assert_eq!(budget.used(), 8);
    assert_eq!(
        device.state(),
        DeviceState::Confirmed(ConfirmedState::Locked)
    );
}

#[test]
fn all_mutations_invalidate_all_objects() {
    let budget = SnapshotBudget::new(16);
    let (mut device, backend) = device(ConfirmedState::Unlocked, &budget, 4);
    for operation in [
        Mutation::SetState(ConfirmedState::Locked),
        Mutation::InterfaceReport,
        Mutation::Measurements,
        Mutation::SetState(ConfirmedState::Running),
        Mutation::SetState(ConfirmedState::Unlocked),
        Mutation::Reset,
        Mutation::Teardown,
    ] {
        for object in [
            Object::InterfaceReport,
            Object::Measurements,
            Object::Certificate,
            Object::Vca,
        ] {
            device.object_size(object).unwrap();
        }
        assert_eq!(budget.used(), 16);
        match operation {
            Mutation::Assignment => unreachable!(),
            Mutation::SetState(state) => device.set_state(state),
            Mutation::InterfaceReport => device.regenerate(Regenerate::InterfaceReport),
            Mutation::Measurements => {
                device.regenerate(Regenerate::Measurements(MeasurementRequest {
                    nonce: [0xa5; 32],
                    raw: false,
                }))
            }
            Mutation::Reset => device.reset(),
            Mutation::Teardown => device.teardown(),
        }
        .unwrap();
        assert_eq!(budget.used(), 0);
        assert_eq!(device.last_transition().unwrap().operation, operation);
        assert_eq!(device.last_transition().unwrap().after, device.state());
    }
    assert_eq!(backend.lock().nonce, Some([0xa5; 32]));
    assert_eq!(device.state(), DeviceState::TornDown);
    let calls = backend.lock().calls.len();
    assert!(device.object_size(Object::Vca).is_err());
    assert!(device.reset().is_err());
    assert!(device.teardown().is_err());
    assert!(device.set_state(ConfirmedState::Locked).is_err());
    assert!(device.regenerate(Regenerate::InterfaceReport).is_err());
    assert_eq!(backend.lock().calls.len(), calls);
}

#[test]
fn regeneration_changes_cached_bytes() {
    let budget = SnapshotBudget::new(8);
    let (mut device, backend) = device(ConfirmedState::Locked, &budget, 4);
    assert_eq!(device.read_object(Object::Vca, 0, 4).unwrap(), [0x43; 4]);
    device.regenerate(Regenerate::InterfaceReport).unwrap();
    backend.lock().sizes[Object::Vca.index()] = 3;
    assert_eq!(device.object_size(Object::Vca).unwrap(), 3);
    assert_eq!(device.read_object(Object::Vca, 0, 3).unwrap(), [0x44; 3]);
    assert!(device.read_object(Object::Vca, 0, 4).is_err());
}

#[test]
fn every_mutation_error_quarantines_even_after_commit() {
    for (initial, call) in [
        (
            ConfirmedState::Unlocked,
            Call::SetState(ConfirmedState::Locked),
        ),
        (
            ConfirmedState::Locked,
            Call::SetState(ConfirmedState::Running),
        ),
        (
            ConfirmedState::Running,
            Call::SetState(ConfirmedState::Unlocked),
        ),
        (ConfirmedState::Locked, Call::Regenerate),
        (ConfirmedState::Running, Call::Reset),
        (ConfirmedState::Unlocked, Call::Teardown),
    ] {
        let budget = SnapshotBudget::new(4);
        let (mut device, backend) = device(initial, &budget, 4);
        device.object_size(Object::InterfaceReport).unwrap();
        backend.lock().fail = Some(call);
        let result = match call {
            Call::SetState(state) => device.set_state(state),
            Call::Regenerate => device.regenerate(Regenerate::InterfaceReport),
            Call::Reset => device.reset(),
            Call::Teardown => device.teardown(),
            _ => unreachable!(),
        };
        assert!(matches!(result, Err(Error::Backend(Failure))));
        let quarantined = DeviceState::Quarantined {
            last_confirmed: initial,
        };
        assert_eq!(device.state(), quarantined);
        assert_eq!(device.last_transition().unwrap().after, quarantined);
        assert_eq!(budget.used(), 0);
        let calls = backend.lock().calls.len();
        assert!(device.set_state(ConfirmedState::Unlocked).is_err());
        assert!(device.reset().is_err());
        assert!(device.regenerate(Regenerate::InterfaceReport).is_err());
        assert!(device.object_size(Object::InterfaceReport).is_err());
        assert!(device.read_object(Object::InterfaceReport, 0, 0).is_err());
        assert_eq!(backend.lock().calls.len(), calls);
        // Failed cleanup cannot turn an unknown state into Unlocked.
        backend.lock().fail = Some(Call::Teardown);
        assert!(device.teardown().is_err());
        assert_eq!(device.state(), quarantined);
        backend.lock().fail = None;
        device.teardown().unwrap();
        assert_eq!(device.state(), DeviceState::TornDown);
    }
}

#[test]
fn explicit_transition_table_and_no_implicit_unlock() {
    use ConfirmedState::*;
    for from in [Unlocked, Locked, Running] {
        for to in [Unlocked, Locked, Running] {
            let budget = SnapshotBudget::new(4);
            let (mut device, backend) = device(from, &budget, 4);
            device.object_size(Object::Vca).unwrap();
            let valid = matches!(
                (from, to),
                (Unlocked, Locked) | (Locked, Running) | (Locked, Unlocked) | (Running, Unlocked)
            );
            let calls = backend.lock().calls.len();
            if valid {
                device.set_state(to).unwrap();
                assert_eq!(device.state(), DeviceState::Confirmed(to));
                assert_eq!(backend.lock().calls.len(), calls + 1);
            } else {
                assert!(matches!(
                    device.set_state(to),
                    Err(Error::InvalidTransition { .. })
                ));
                assert_eq!(device.state(), DeviceState::Confirmed(from));
                assert_eq!(backend.lock().calls.len(), calls);
                assert!(device.last_transition().is_none());
                assert_eq!(budget.used(), 4);
            }
        }
    }
    let budget = SnapshotBudget::new(4);
    let (mut device, backend) = device(Unlocked, &budget, 4);
    assert!(device.regenerate(Regenerate::InterfaceReport).is_err());
    assert!(
        device
            .regenerate(Regenerate::Measurements(MeasurementRequest {
                nonce: [0; 32],
                raw: false,
            }))
            .is_err()
    );
    assert_eq!(backend.lock().calls, [Call::State]);
}

#[test]
fn read_errors_invalidate_without_state_mutation() {
    for call in [Call::Size(Object::Vca), Call::Read(Object::Vca)] {
        let budget = SnapshotBudget::new(8);
        let (mut device, backend) = device(ConfirmedState::Locked, &budget, 4);
        device.object_size(Object::Certificate).unwrap();
        backend.lock().fail = Some(call);
        assert!(matches!(
            device.object_size(Object::Vca),
            Err(Error::Backend(Failure))
        ));
        assert_eq!(budget.used(), 0);
        assert_eq!(
            device.state(),
            DeviceState::Confirmed(ConfirmedState::Locked)
        );
        backend.lock().fail = None;
        assert_eq!(device.object_size(Object::Vca).unwrap(), 4);
    }
}

#[test]
fn dropping_does_not_unlock_or_teardown() {
    let budget = SnapshotBudget::new(4);
    let (mut device, backend) = device(ConfirmedState::Running, &budget, 4);
    device.object_size(Object::Vca).unwrap();
    let calls = backend.lock().calls.clone();
    drop(device);
    assert_eq!(budget.used(), 0);
    assert_eq!(backend.lock().calls, calls);
    assert_eq!(backend.lock().state, ConfirmedState::Running);
}

#[test]
fn initial_state_failure_is_not_success() {
    let budget = SnapshotBudget::new(0);
    let (device, backend) = device(ConfirmedState::Unlocked, &budget, 0);
    drop(device);
    backend.lock().fail = Some(Call::State);
    assert!(matches!(
        Coordinator::new(Fake(backend.clone()), budget),
        Err(Error::Backend(Failure))
    ));
    assert_eq!(backend.lock().calls, [Call::State, Call::State]);
}
