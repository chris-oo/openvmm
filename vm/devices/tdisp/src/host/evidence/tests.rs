// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::host::ConfirmedState;
use crate::host::Regenerate;
use crate::host::SnapshotBudget;
use futures::executor::block_on;
use parking_lot::Mutex as SyncMutex;
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use test_with_tracing::test;

#[derive(Debug, thiserror::Error)]
#[error("injected failure")]
struct Failure;

struct Pause {
    entered: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
}

impl Pause {
    fn wait(self) {
        self.entered.send(()).unwrap();
        self.resume.recv().unwrap();
    }
}

fn pause() -> (Pause, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, received) = mpsc::channel();
    let (resume, sent) = mpsc::channel();
    (
        Pause {
            entered,
            resume: sent,
        },
        received,
        resume,
    )
}

#[derive(Default)]
struct Model {
    sizes: usize,
    reads: usize,
    cleanups: usize,
    drops: usize,
    pointer: usize,
    size: u64,
    short_read: bool,
    fail_read: bool,
    fail_cleanup: bool,
    read_pause: Option<Pause>,
    cleanup_pause: Option<Pause>,
    dropped: Option<mpsc::Sender<()>>,
}

struct Fake(Arc<SyncMutex<Model>>);

impl Drop for Fake {
    fn drop(&mut self) {
        let mut model = self.0.lock();
        model.drops += 1;
        if let Some(dropped) = model.dropped.take() {
            dropped.send(()).unwrap();
        }
    }
}

impl Backend for Fake {
    type Error = Failure;

    fn state(&mut self) -> Result<ConfirmedState, Failure> {
        Ok(ConfirmedState::Unlocked)
    }

    fn object_size(&mut self, _: Object) -> Result<u64, Failure> {
        let mut model = self.0.lock();
        model.sizes += 1;
        Ok(model.size)
    }

    fn read_object(&mut self, _: Object, bytes: &mut [u8]) -> Result<usize, Failure> {
        let pause = {
            let mut model = self.0.lock();
            model.reads += 1;
            model.pointer = bytes.as_ptr() as usize;
            model.read_pause.take()
        };
        if let Some(pause) = pause {
            pause.wait();
        }
        let model = self.0.lock();
        if model.fail_read {
            return Err(Failure);
        }
        bytes.fill(0x5a);
        Ok(bytes.len() - usize::from(model.short_read))
    }

    fn teardown(&mut self) -> Result<(), Failure> {
        let pause = {
            let mut model = self.0.lock();
            model.cleanups += 1;
            model.cleanup_pause.take()
        };
        if let Some(pause) = pause {
            pause.wait();
        }
        if self.0.lock().fail_cleanup {
            Err(Failure)
        } else {
            Ok(())
        }
    }

    fn set_state(&mut self, _: ConfirmedState) -> Result<(), Failure> {
        panic!("evidence service must not change device state");
    }

    fn regenerate(&mut self, _: &Regenerate) -> Result<(), Failure> {
        panic!("evidence service must not regenerate objects");
    }

    fn reset(&mut self) -> Result<(), Failure> {
        panic!("evidence service must not reset the device");
    }
}

#[test]
fn evidence_only_service_rejects_every_assignment_entry() {
    let (service, model) = service(&SnapshotBudget::new(8));
    assert!(!service.supports_assignment());
    assert!(matches!(
        block_on(service.set_state(ConfirmedState::Locked)),
        Err(EvidenceError::Unsupported)
    ));
    assert!(matches!(
        block_on(service.regenerate(Regenerate::InterfaceReport)),
        Err(EvidenceError::Unsupported)
    ));
    assert!(matches!(
        block_on(service.assignment(AssignmentOperation::PreparePrivateMemory)),
        Err(EvidenceError::Unsupported)
    ));
    assert_eq!(model.lock().reads, 0);
}

fn service(budget: &SnapshotBudget) -> (Arc<Service<Fake>>, Arc<SyncMutex<Model>>) {
    let model = Arc::new(SyncMutex::new(Model {
        size: 8,
        ..Default::default()
    }));
    let coordinator = Coordinator::new(Fake(model.clone()), budget.clone()).unwrap();
    (
        Arc::new(Service {
            assignment: false,
            owner: Arc::new(Mutex::new(coordinator)),
            closed: AtomicBool::new(false),
        }),
        model,
    )
}

#[derive(Default)]
struct Sink {
    pause: SyncMutex<Option<Pause>>,
    selected: SyncMutex<Option<(usize, usize)>>,
    fail: bool,
}

impl EvidenceSink for Sink {
    fn write(&self, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        assert!(bytes.iter().all(|&byte| byte == 0x5a));
        *self.selected.lock() = Some((bytes.as_ptr() as usize, bytes.len()));
        if let Some(pause) = self.pause.lock().take() {
            pause.wait();
        }
        if self.fail {
            return Err(Box::new(Failure));
        }
        Ok(())
    }
}

fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn synchronous_close_does_not_lock_coordinator_and_rejects_queued_mutations() {
    let (mut service, model) = service(&SnapshotBudget::new(8));
    Arc::get_mut(&mut service).unwrap().assignment = true;
    let owner = block_on(service.owner.clone().lock_owned());
    let mut queued = service.set_state(ConfirmedState::Locked);
    assert!(poll(queued.as_mut()).is_pending());

    let (sent, received) = mpsc::channel();
    let closer = service.clone();
    let thread = std::thread::spawn(move || {
        closer.close_admission();
        sent.send(()).unwrap();
    });
    received
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("synchronous admission close must not wait for the coordinator");
    thread.join().unwrap();
    assert!(service.closed.load(Ordering::SeqCst));
    assert!(matches!(
        block_on(service.object_size(Object::Certificate)),
        Err(EvidenceError::Closed)
    ));

    drop(owner);
    assert!(matches!(block_on(queued), Err(EvidenceError::Closed)));
    let model = model.lock();
    assert_eq!(model.sizes, 0);
    assert_eq!(model.reads, 0);
    assert_eq!(model.cleanups, 0);
}

#[test]
fn unexpected_completion_loss_is_a_device_error_with_its_source() {
    use std::error::Error as _;

    let (sender, receiver) = mesh::oneshot::<Result<usize, EvidenceError>>();
    drop(sender);
    let error = block_on(receive_completion(receiver)).unwrap_err();
    assert!(matches!(error, EvidenceError::Device(_)));
    let worker = error
        .source()
        .unwrap()
        .downcast_ref::<WorkerCompletionError>()
        .unwrap();
    assert!(worker.source().unwrap().is::<mesh::RecvError>());
}

#[test]
fn cancelled_admitted_workers_execute_after_pool_queueing() {
    const CHILD: &str = "TDISP_QUEUED_WORKER_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::evidence::tests::cancelled_admitted_workers_execute_after_pool_queueing",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("BLOCKING_MAX_THREADS", "1")
            .status()
            .unwrap();
        assert!(status.success(), "queued-worker child failed: {status}");
        return;
    }
    assert_eq!(std::env::var("BLOCKING_MAX_THREADS").unwrap(), "1");

    for read in [false, true] {
        let budget = SnapshotBudget::new(8);
        let (service, model) = service(&budget);
        let (pause, entered, resume) = pause();
        let blocker = blocking::unblock(move || pause.wait());
        entered.recv().unwrap();

        let sink = Arc::new(Sink::default());
        let weak_sink = Arc::downgrade(&sink);
        let mut operation = if read {
            service.read_object(Object::Certificate, 2, 3, sink.clone())
        } else {
            service.object_size(Object::Certificate)
        };
        drop(sink);
        assert!(poll(operation.as_mut()).is_pending());
        assert!(service.owner.try_lock().is_none());
        assert_eq!(model.lock().sizes, 0);
        assert_eq!(model.lock().reads, 0);
        assert_eq!(budget.used(), 0);
        drop(operation);
        assert!(service.owner.try_lock().is_none());
        let retained_sink = read.then(|| weak_sink.upgrade().unwrap());

        resume.send(()).unwrap();
        block_on(blocker);
        // Acquiring the guard drains even a cancelled queued task. Checking
        // here detects skipped work without waiting for a callback it omitted.
        let drained = block_on(service.owner.clone().lock_owned());
        assert_eq!(model.lock().sizes, 1);
        assert_eq!(model.lock().reads, 1);
        assert_eq!(budget.used(), 8);
        if let Some(sink) = retained_sink {
            assert_eq!(*sink.selected.lock(), Some((model.lock().pointer + 2, 3)));
        }
        drop(drained);
        block_on(service.teardown()).unwrap();
        assert_eq!(budget.used(), 0);
    }

    let budget = SnapshotBudget::new(8);
    let (service, model) = service(&budget);
    block_on(service.object_size(Object::Certificate)).unwrap();
    let (pause, entered, resume) = pause();
    let blocker = blocking::unblock(move || pause.wait());
    entered.recv().unwrap();

    let mut teardown = service.teardown();
    assert!(poll(teardown.as_mut()).is_pending());
    assert_eq!(model.lock().cleanups, 0);
    assert_eq!(budget.used(), 8);
    drop(teardown);
    let mut retry = service.teardown();
    assert!(poll(retry.as_mut()).is_pending());
    assert!(matches!(
        block_on(service.object_size(Object::Certificate)),
        Err(EvidenceError::Closed)
    ));
    resume.send(()).unwrap();
    block_on(blocker);
    let drained = block_on(service.owner.clone().lock_owned());
    assert_eq!(model.lock().cleanups, 1);
    assert_eq!(drained.state(), DeviceState::TornDown);
    assert_eq!(budget.used(), 0);
    drop(drained);
    block_on(retry).unwrap();
    assert_eq!(model.lock().cleanups, 1);
}

#[test]
fn sink_borrows_the_budgeted_snapshot_and_returns_exact_selected_count() {
    let budget = SnapshotBudget::new(16);
    let (service, model) = service(&budget);
    let sink = Arc::new(Sink::default());
    assert_eq!(
        block_on(service.object_size(Object::Certificate)).unwrap(),
        8
    );
    assert_eq!(
        block_on(service.read_object(Object::Certificate, 2, 3, sink.clone())).unwrap(),
        3
    );
    assert_eq!(*sink.selected.lock(), Some((model.lock().pointer + 2, 3)));
    assert_eq!(budget.used(), 8);
    assert_eq!(model.lock().sizes, 1);
    assert_eq!(model.lock().reads, 1);
    assert_eq!(
        block_on(service.read_object(Object::Certificate, 8, 0, sink.clone())).unwrap(),
        0
    );
    block_on(service.teardown()).unwrap();
    block_on(service.teardown()).unwrap();
    assert_eq!(budget.used(), 0);
    assert_eq!(model.lock().cleanups, 1);
}

#[test]
fn admission_waiters_do_not_submit_workers_or_retain_cancelled_sinks() {
    let budget = SnapshotBudget::new(32);
    let (service, model) = service(&budget);
    let (pause, entered, resume) = pause();
    model.lock().read_pause = Some(pause);
    let mut active = service.object_size(Object::Certificate);
    assert!(poll(active.as_mut()).is_pending());
    entered.recv().unwrap();

    let sink = Arc::new(Sink::default());
    let weak_sink = Arc::downgrade(&sink);
    let mut cancelled = service.read_object(Object::Vca, 0, 1, sink);
    let mut queued = service.object_size(Object::Measurements);
    assert!(poll(cancelled.as_mut()).is_pending());
    assert!(poll(queued.as_mut()).is_pending());
    assert!(service.owner.try_lock().is_none());
    assert_eq!(model.lock().sizes, 1);
    assert_eq!(model.lock().reads, 1);
    drop(cancelled);
    assert!(weak_sink.upgrade().is_none());

    resume.send(()).unwrap();
    assert_eq!(block_on(active).unwrap(), 8);
    assert_eq!(block_on(queued).unwrap(), 8);
    assert_eq!(model.lock().sizes, 2);
    assert_eq!(model.lock().reads, 2);
    assert_eq!(budget.used(), 16);
    block_on(service.teardown()).unwrap();
}

#[test]
fn cancelled_active_waiter_retains_exclusive_owner_sink_and_budget() {
    let budget = SnapshotBudget::new(8);
    let (service, model) = service(&budget);
    let (dropped, received_drop) = mpsc::channel();
    model.lock().dropped = Some(dropped);
    let (pause, entered, resume) = pause();
    let sink = Arc::new(Sink {
        pause: SyncMutex::new(Some(pause)),
        ..Default::default()
    });
    let weak_sink = Arc::downgrade(&sink);
    let owner = Arc::downgrade(&service.owner);
    let mut active = service.read_object(Object::Certificate, 0, 8, sink);
    assert!(poll(active.as_mut()).is_pending());
    entered.recv().unwrap();
    drop(active);

    let mut queued = service.object_size(Object::Vca);
    assert!(poll(queued.as_mut()).is_pending());
    assert!(service.owner.try_lock().is_none());
    assert!(weak_sink.upgrade().is_some());
    assert_eq!(model.lock().sizes, 1);
    drop(queued);
    drop(service);
    assert_eq!(model.lock().drops, 0);
    assert_eq!(budget.used(), 8);

    let retained_owner = owner.upgrade().unwrap();
    resume.send(()).unwrap();
    block_on(async {
        let mut drained = retained_owner.clone().lock_owned().await;
        assert_eq!(model.lock().cleanups, 0);
        drained.teardown().unwrap();
    });
    drop(retained_owner);
    received_drop.recv().unwrap();
    assert_eq!(model.lock().drops, 1);
    assert_eq!(model.lock().cleanups, 1);
    assert_eq!(budget.used(), 0);
}

#[test]
fn cancelled_teardown_closes_admission_and_rejects_queued_reads_before_cleanup() {
    let budget = SnapshotBudget::new(16);
    let (service, model) = service(&budget);
    let (pause, entered, resume) = pause();
    let sink = Arc::new(Sink {
        pause: SyncMutex::new(Some(pause)),
        ..Default::default()
    });
    let mut active = service.read_object(Object::Certificate, 0, 8, sink);
    assert!(poll(active.as_mut()).is_pending());
    entered.recv().unwrap();
    let mut queued = service.object_size(Object::Vca);
    assert!(poll(queued.as_mut()).is_pending());
    let mut teardown = service.teardown();
    assert!(poll(teardown.as_mut()).is_pending());
    assert!(matches!(
        block_on(service.object_size(Object::Measurements)),
        Err(EvidenceError::Closed)
    ));
    assert!(matches!(
        block_on(service.read_object(Object::Vca, 0, 1, Arc::new(Sink::default()))),
        Err(EvidenceError::Closed)
    ));
    assert_eq!(model.lock().cleanups, 0);
    drop(teardown);
    resume.send(()).unwrap();
    assert_eq!(block_on(active).unwrap(), 8);
    assert!(matches!(block_on(queued), Err(EvidenceError::Closed)));
    assert_eq!(model.lock().sizes, 1);
    assert_eq!(model.lock().cleanups, 0);
    block_on(service.teardown()).unwrap();
    assert_eq!(model.lock().cleanups, 1);
    assert_eq!(budget.used(), 0);
}

#[test]
fn cancelled_cleanup_is_drained_before_retry_and_never_reopens_reads() {
    let budget = SnapshotBudget::new(8);
    let (service, model) = service(&budget);
    block_on(service.object_size(Object::Certificate)).unwrap();
    let (pause, entered, resume) = pause();
    model.lock().cleanup_pause = Some(pause);
    let mut teardown = service.teardown();
    assert!(poll(teardown.as_mut()).is_pending());
    entered.recv().unwrap();
    drop(teardown);
    assert_eq!(budget.used(), 0);
    let mut retry = service.teardown();
    assert!(poll(retry.as_mut()).is_pending());
    assert_eq!(model.lock().cleanups, 1);
    assert_eq!(model.lock().drops, 0);
    assert!(matches!(
        block_on(service.object_size(Object::Certificate)),
        Err(EvidenceError::Closed)
    ));
    resume.send(()).unwrap();
    block_on(retry).unwrap();
    assert_eq!(model.lock().cleanups, 1);
}

#[test]
fn cleanup_failure_retains_closed_owner_for_explicit_retry() {
    let budget = SnapshotBudget::new(8);
    let (service, model) = service(&budget);
    block_on(service.object_size(Object::Certificate)).unwrap();
    model.lock().fail_cleanup = true;
    let error = block_on(service.teardown()).unwrap_err();
    assert_device_source(&error);
    assert_eq!(budget.used(), 0);
    assert_eq!(model.lock().drops, 0);
    assert!(matches!(
        block_on(service.object_size(Object::Certificate)),
        Err(EvidenceError::Closed)
    ));
    assert_eq!(model.lock().sizes, 1);
    model.lock().fail_cleanup = false;
    block_on(service.teardown()).unwrap();
    assert_eq!(model.lock().cleanups, 2);
    assert_eq!(model.lock().drops, 0);
    drop(service);
    assert_eq!(model.lock().drops, 1);
}

fn assert_device_source(error: &EvidenceError) {
    use std::error::Error as _;
    assert!(matches!(error, EvidenceError::Device(_)));
    let core = error
        .source()
        .unwrap()
        .downcast_ref::<Error<Failure>>()
        .unwrap();
    assert!(core.source().unwrap().is::<Failure>());
}

#[test]
fn error_classification_preserves_range_device_and_access_sources() {
    use std::error::Error as _;

    let budget = SnapshotBudget::new(8);
    let (service, model) = service(&budget);
    let sink = Arc::new(Sink::default());
    for (offset, length) in [(8, 1), (u64::MAX, 2), (0, u64::MAX)] {
        let error =
            block_on(service.read_object(Object::Vca, offset, length, sink.clone())).unwrap_err();
        assert!(matches!(
            error,
            EvidenceError::InvalidRange(SnapshotError::InvalidRange { .. })
        ));
        assert!(error.source().unwrap().is::<SnapshotError>());
        assert!(sink.selected.lock().is_none());
    }
    let sink = Arc::new(Sink {
        fail: true,
        ..Default::default()
    });
    let error = block_on(service.read_object(Object::Vca, 0, 3, sink.clone())).unwrap_err();
    assert!(matches!(error, EvidenceError::Access(_)));
    assert!(error.source().unwrap().is::<Failure>());
    assert_eq!(sink.selected.lock().unwrap().1, 3);
    block_on(service.teardown()).unwrap();

    // The generic conversion must not classify lifecycle errors as bad ranges.
    let error = EvidenceError::from(Error::<Failure>::InvalidState {
        state: DeviceState::TornDown,
    });
    assert!(matches!(error, EvidenceError::Device(_)));
    assert!(error.source().unwrap().is::<Error<Failure>>());
    assert_eq!(model.lock().reads, 1);
}

#[test]
fn acquisition_errors_are_device_errors_and_shared_budget_is_released() {
    for size in [0, crate::host::MAX_OBJECT_SIZE as u64 + 1, 8] {
        let budget = SnapshotBudget::new(4);
        let (service, model) = service(&budget);
        model.lock().size = size;
        let error = block_on(service.object_size(Object::Vca)).unwrap_err();
        let EvidenceError::Device(error) = error else {
            panic!("acquisition error must be a device error");
        };
        assert!(matches!(
            error.downcast_ref::<Error<Failure>>().unwrap(),
            Error::Snapshot(_)
        ));
        assert_eq!(model.lock().reads, 0);
        assert_eq!(budget.used(), 0);
    }

    let budget = SnapshotBudget::new(8);
    let (first, first_model) = service(&budget);
    let (second, second_model) = service(&budget);
    block_on(first.object_size(Object::Vca)).unwrap();
    assert!(matches!(
        block_on(second.object_size(Object::Vca)),
        Err(EvidenceError::Device(_))
    ));
    assert_eq!(second_model.lock().reads, 0);
    block_on(first.teardown()).unwrap();
    assert_eq!(first_model.lock().drops, 0);
    second_model.lock().fail_read = true;
    let error = block_on(second.object_size(Object::Vca)).unwrap_err();
    assert_device_source(&error);
    assert_eq!(budget.used(), 0);
    second_model.lock().fail_read = false;
    second_model.lock().short_read = true;
    assert!(matches!(
        block_on(second.object_size(Object::Vca)),
        Err(EvidenceError::Device(_))
    ));
    assert_eq!(budget.used(), 0);
    second_model.lock().short_read = false;
    block_on(second.object_size(Object::Vca)).unwrap();
    assert_eq!(budget.used(), 8);
    block_on(second.teardown()).unwrap();
    assert_eq!(budget.used(), 0);
}
