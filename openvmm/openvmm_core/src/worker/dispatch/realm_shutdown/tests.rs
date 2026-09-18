// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use futures::executor::block_on;
use futures::task::ArcWake;
use futures::task::waker;
use mesh::OneshotReceiver;
use mesh::rpc::RpcSend;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use test_with_tracing::test;

type Events = Arc<Mutex<Vec<&'static str>>>;

struct Resource(&'static str, Events);

impl Drop for Resource {
    fn drop(&mut self) {
        self.1.lock().push(self.0);
    }
}

struct Owner {
    events: Events,
    dynamic: Option<Resource>,
    static_device: Option<Resource>,
    stop_pause: Option<OneshotReceiver<()>>,
    dynamic_pause: Option<OneshotReceiver<()>>,
    static_pause: Option<OneshotReceiver<()>>,
    teardown_pause: Option<OneshotReceiver<()>>,
    failures: VecDeque<&'static str>,
    _partition: Resource,
    _ram: Resource,
    _executor: Resource,
    _services: Resource,
}

impl Owner {
    fn new() -> Self {
        let events = Events::default();
        Self {
            events: events.clone(),
            dynamic: Some(Resource("dynamic_drop", events.clone())),
            static_device: Some(Resource("static_drop", events.clone())),
            stop_pause: None,
            dynamic_pause: None,
            static_pause: None,
            teardown_pause: None,
            failures: VecDeque::new(),
            _partition: Resource("partition_drop", events.clone()),
            _ram: Resource("ram_drop", events.clone()),
            _executor: Resource("executor_drop", events.clone()),
            _services: Resource("services_drop", events),
        }
    }
}

impl ShutdownOwner for Owner {
    fn close_admission(&self) {
        self.events.lock().push("close");
    }

    async fn stop(&mut self) {
        self.events.lock().push("stop");
        if let Some(pause) = self.stop_pause.take() {
            pause.await.unwrap();
        }
    }

    fn drain_dynamic(&mut self) -> BoxFuture<'static, ()> {
        self.events.lock().push("take_dynamic");
        let resource = self.dynamic.take().unwrap();
        let pause = self.dynamic_pause.take();
        Box::pin(async move {
            if let Some(pause) = pause {
                pause.await.unwrap();
            }
            drop(resource);
        })
    }

    fn drain_static(&mut self) -> BoxFuture<'static, ()> {
        self.events.lock().push("take_static");
        let resource = self.static_device.take().unwrap();
        let pause = self.static_pause.take();
        Box::pin(async move {
            if let Some(pause) = pause {
                pause.await.unwrap();
            }
            drop(resource);
        })
    }

    async fn teardown(&mut self) -> anyhow::Result<()> {
        assert_eq!(
            self.events
                .lock()
                .iter()
                .filter(|&&e| e == "dynamic_drop")
                .count(),
            1
        );
        assert_eq!(
            self.events
                .lock()
                .iter()
                .filter(|&&e| e == "static_drop")
                .count(),
            1
        );
        assert!(!self.events.lock().contains(&"ram_drop"));
        assert!(!self.events.lock().contains(&"executor_drop"));
        self.events.lock().push("teardown");
        if let Some(pause) = self.teardown_pause.take() {
            pause.await.unwrap();
        }
        if let Some(failure) = self.failures.pop_front() {
            return Err(anyhow::anyhow!(failure).context("checked Realm teardown"));
        }
        self.events.lock().push("checked");
        Ok(())
    }
}

fn pending<T>(future: Pin<&mut impl Future<Output = T>>) {
    assert!(matches!(
        future.poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
}

fn assert_roots_retained(events: &Events) {
    for released in [
        "partition_drop",
        "ram_drop",
        "executor_drop",
        "services_drop",
    ] {
        assert!(!events.lock().contains(&released), "unexpected {released}");
    }
}

#[test]
fn unpolled_shutdown_retains_vm_and_frontends() {
    let owner = Owner::new();
    let events = owner.events.clone();
    let (_send, recv) = mesh::channel::<WorkerRpc<()>>();
    let (rpc_send, rpc_recv) = mesh::channel();
    let future = begin_shutdown(owner, rpc_recv, recv);
    assert!(block_on(rpc_send.call(VmRpc::ClearHalt, ())).is_err());
    assert!(block_on(rpc_send.call_failable(VmRpc::Reset, ())).is_err());
    assert_eq!(*events.lock(), ["close"]);
    drop(future);
    assert_eq!(*events.lock(), ["close"]);
    assert_roots_retained(&events);
}

#[test]
fn cancellation_retains_popped_static_dynamic_and_shared_cleanup_owners() {
    for phase in [
        Phase::Stop,
        Phase::DynamicDevices,
        Phase::StaticDevices,
        Phase::RealmServices,
    ] {
        let mut owner = Owner::new();
        let events = owner.events.clone();
        let (_release, pause) = mesh::oneshot();
        match phase {
            Phase::Stop => owner.stop_pause = Some(pause),
            Phase::DynamicDevices => owner.dynamic_pause = Some(pause),
            Phase::StaticDevices => owner.static_pause = Some(pause),
            Phase::RealmServices => owner.teardown_pause = Some(pause),
            Phase::Complete => unreachable!(),
        }
        let (_send, recv) = mesh::channel::<WorkerRpc<()>>();
        let mut future = Box::pin(shutdown_retained(Custody::new(owner), recv));
        pending(future.as_mut());
        if phase == Phase::DynamicDevices {
            assert!(events.lock().contains(&"take_dynamic"));
            assert!(!events.lock().contains(&"dynamic_drop"));
        }
        if phase == Phase::StaticDevices {
            assert!(events.lock().contains(&"take_static"));
            assert!(!events.lock().contains(&"static_drop"));
        }
        let before = events.lock().clone();
        drop(future);
        assert_eq!(*events.lock(), before);
        assert!(!events.lock().contains(&"checked"));
        assert_roots_retained(&events);
    }
}

#[test]
fn suspended_removals_resume_in_place_without_repeating_drains() {
    for phase in [Phase::DynamicDevices, Phase::StaticDevices] {
        let mut owner = Owner::new();
        let events = owner.events.clone();
        let (release, pause) = mesh::oneshot();
        match phase {
            Phase::DynamicDevices => owner.dynamic_pause = Some(pause),
            _ => owner.static_pause = Some(pause),
        }
        let mut custody = Custody::new(owner);
        {
            let mut attempt = Box::pin(custody.attempt());
            pending(attempt.as_mut());
        }
        assert_eq!(custody.0.as_ref().unwrap().phase, phase);
        assert!(custody.0.as_ref().unwrap().removal.is_some());
        assert_roots_retained(&events);
        release.send(());
        block_on(custody.attempt()).unwrap();
        let owner = custody.release();
        assert_eq!(
            *events.lock(),
            [
                "close",
                "stop",
                "take_dynamic",
                "dynamic_drop",
                "take_static",
                "static_drop",
                "teardown",
                "checked",
            ]
        );
        assert_roots_retained(&events);
        drop(owner);
        assert!(events.lock().contains(&"ram_drop"));
        assert!(events.lock().contains(&"executor_drop"));
    }
}

#[test]
fn checked_completion_is_required_before_shutdown_can_return() {
    let mut owner = Owner::new();
    let events = owner.events.clone();
    let (release, pause) = mesh::oneshot();
    owner.teardown_pause = Some(pause);
    let (_send, recv) = mesh::channel::<WorkerRpc<()>>();
    let mut future = Box::pin(shutdown_retained(Custody::new(owner), recv));
    pending(future.as_mut());
    assert!(!events.lock().contains(&"checked"));
    assert_roots_retained(&events);
    release.send(());
    let owner = block_on(future);
    assert_eq!(events.lock().last(), Some(&"checked"));
    assert_roots_retained(&events);
    // The caller can run normal partition/VMBus shutdown only after this return.
    drop(owner);
    assert!(events.lock().contains(&"services_drop"));
}

#[test]
fn failed_cleanup_rejects_restart_and_retries_only_services() {
    let mut owner = Owner::new();
    let events = owner.events.clone();
    owner.failures = [
        "frontend IRQ release failed",
        "short shared IOAS unmap",
        "object destroy failed",
    ]
    .into();
    let (send, recv) = mesh::channel::<WorkerRpc<()>>();
    let mut future = Box::pin(shutdown_retained(Custody::new(owner), recv));
    for index in 0..3 {
        pending(future.as_mut());
        assert_roots_retained(&events);
        assert!(!events.lock().contains(&"checked"));
        let restart = send.call_failable(WorkerRpc::Restart, ());
        pending(future.as_mut());
        assert!(
            block_on(restart)
                .unwrap_err()
                .to_string()
                .contains("irreversible")
        );
        assert_eq!(events.lock().iter().filter(|&&e| e == "stop").count(), 1);
        assert_eq!(
            events.lock().iter().filter(|&&e| e == "teardown").count(),
            index + 1
        );
        send.send(WorkerRpc::Stop);
    }
    let owner = block_on(future);
    assert_eq!(
        events
            .lock()
            .iter()
            .filter(|&&e| e == "take_dynamic")
            .count(),
        1
    );
    assert_eq!(
        events
            .lock()
            .iter()
            .filter(|&&e| e == "take_static")
            .count(),
        1
    );
    assert_eq!(
        events.lock().iter().filter(|&&e| e == "teardown").count(),
        4
    );
    assert_eq!(events.lock().last(), Some(&"checked"));
    drop(owner);
}

#[test]
fn inspection_status_preserves_phase_and_full_cleanup_error() {
    let mut owner = Owner::new();
    owner.failures.push_back("short shared IOAS unmap");
    let mut custody = Custody::new(owner);
    block_on(custody.attempt()).unwrap_err();
    let retained = custody.0.as_ref().unwrap();
    assert_eq!(retained.phase, Phase::RealmServices);
    assert_eq!(
        retained.last_error.as_deref(),
        Some("checked Realm teardown: short shared IOAS unmap")
    );
    assert!(retained.removal.is_none());
    block_on(custody.attempt()).unwrap();
    assert!(custody.0.as_ref().unwrap().last_error.is_none());
    drop(custody.release());
}

struct WakeCount(AtomicUsize);

impl ArcWake for WakeCount {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn closed_parent_channel_waits_without_releasing_or_spinning() {
    let mut owner = Owner::new();
    let events = owner.events.clone();
    owner.failures.push_back("cleanup failure");
    let (send, recv) = mesh::channel::<WorkerRpc<()>>();
    drop(send);
    let mut future = Box::pin(shutdown_retained(Custody::new(owner), recv));
    let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = waker(wakes.clone());
    for _ in 0..3 {
        assert!(matches!(
            future.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        ));
    }
    assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
    assert_eq!(
        events.lock().iter().filter(|&&e| e == "teardown").count(),
        1
    );
    assert_roots_retained(&events);
    drop(future);
    assert_roots_retained(&events);
}
