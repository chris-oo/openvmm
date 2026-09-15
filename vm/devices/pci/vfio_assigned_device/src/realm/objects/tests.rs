// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use parking_lot::Mutex;
use std::sync::Arc;
use test_with_tracing::test;

const DEVICE: u32 = 10;
const IOAS: u32 = 20;
const PARENT: u32 = 30;
const VIOMMU: u32 = 40;
const CHILD: u32 = 50;
const VDEVICE: u32 = 60;
const RID: u32 = 0x20118;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Associate,
    Bind,
    Ioas,
    DisableHugePages(u32),
    Parent(u32, u32),
    Viommu(u32, u32),
    Child(u32, u32),
    Vdevice(u32, u32, u32),
    Attach(u32),
    Detach,
    Destroy(u32),
    Disassociate,
    CloseFile,
}

struct Model {
    calls: Vec<Call>,
    failures: Vec<Call>,
    drops: Vec<&'static str>,
    attach_reply: u32,
    hardware_attached: bool,
}

struct Token(&'static str, Arc<Mutex<Model>>);

impl Drop for Token {
    fn drop(&mut self) {
        self.1.lock().drops.push(self.0);
    }
}

struct Fake {
    file: Option<Token>,
    _context: Token,
    _partition: Token,
    model: Arc<Mutex<Model>>,
}

impl Fake {
    fn new(failures: Vec<Call>) -> (Self, Arc<Mutex<Model>>) {
        let model = Arc::new(Mutex::new(Model {
            calls: Vec::new(),
            failures,
            drops: Vec::new(),
            attach_reply: CHILD,
            hardware_attached: false,
        }));
        (
            Self {
                file: Some(Token("file", model.clone())),
                _context: Token("context", model.clone()),
                _partition: Token("partition", model.clone()),
                model: model.clone(),
            },
            model,
        )
    }

    fn record(&mut self, call: Call) -> anyhow::Result<()> {
        let mut model = self.model.lock();
        model.calls.push(call.clone());
        anyhow::ensure!(!model.failures.contains(&call), "injected {call:?}");
        Ok(())
    }
}

impl Operations for Fake {
    fn associate(&mut self) -> anyhow::Result<()> {
        self.record(Call::Associate)
    }
    fn bind(&mut self) -> anyhow::Result<u32> {
        self.record(Call::Bind)?;
        Ok(DEVICE)
    }
    fn allocate_ioas(&mut self) -> anyhow::Result<u32> {
        self.record(Call::Ioas)?;
        Ok(IOAS)
    }
    fn disable_huge_pages(&mut self, ioas: u32) -> anyhow::Result<()> {
        self.record(Call::DisableHugePages(ioas))
    }
    fn allocate_parent(&mut self, dev: u32, ioas: u32) -> anyhow::Result<u32> {
        self.record(Call::Parent(dev, ioas))?;
        Ok(PARENT)
    }
    fn allocate_viommu(&mut self, dev: u32, parent: u32) -> anyhow::Result<u32> {
        self.record(Call::Viommu(dev, parent))?;
        Ok(VIOMMU)
    }
    fn allocate_child(&mut self, dev: u32, viommu: u32) -> anyhow::Result<u32> {
        self.record(Call::Child(dev, viommu))?;
        Ok(CHILD)
    }
    fn allocate_vdevice(&mut self, dev: u32, viommu: u32, rid: u32) -> anyhow::Result<u32> {
        self.record(Call::Vdevice(dev, viommu, rid))?;
        Ok(VDEVICE)
    }
    fn attach(&mut self, child: u32) -> anyhow::Result<u32> {
        self.model.lock().hardware_attached = true;
        self.record(Call::Attach(child))?;
        Ok(self.model.lock().attach_reply)
    }
    fn detach(&mut self) -> anyhow::Result<()> {
        self.record(Call::Detach)?;
        self.model.lock().hardware_attached = false;
        Ok(())
    }
    fn destroy(&mut self, id: u32) -> anyhow::Result<()> {
        self.record(Call::Destroy(id))
    }
    fn disassociate(&mut self) -> anyhow::Result<()> {
        self.record(Call::Disassociate)
    }
    fn close_file(&mut self) {
        self.model.lock().calls.push(Call::CloseFile);
        self.file = None;
    }
}

fn preparation() -> Vec<Call> {
    vec![
        Call::Associate,
        Call::Bind,
        Call::Ioas,
        Call::DisableHugePages(IOAS),
        Call::Parent(DEVICE, IOAS),
        Call::Viommu(DEVICE, PARENT),
        Call::Child(DEVICE, VIOMMU),
    ]
}

fn cleanup() -> Vec<Call> {
    vec![
        Call::Detach,
        Call::Destroy(VDEVICE),
        Call::Destroy(CHILD),
        Call::Destroy(VIOMMU),
        Call::Destroy(PARENT),
        Call::Destroy(IOAS),
        Call::Disassociate,
        Call::CloseFile,
    ]
}

fn prepared(fake: Fake) -> ObjectOwner<Fake> {
    match ObjectOwner::prepare(fake) {
        Ok(owner) => owner,
        Err(_) => panic!("unexpected preparation failure"),
    }
}

fn failed(fake: Fake) -> PrepareFailure<Fake> {
    match ObjectOwner::prepare(fake) {
        Err(error) => error,
        Ok(_) => panic!("expected preparation failure"),
    }
}

#[test]
fn creates_in_dependency_order_and_attaches_only_after_rid() {
    let (fake, model) = Fake::new(vec![]);
    let mut owner = prepared(fake);
    assert_eq!(model.lock().calls, preparation());
    assert_eq!(owner.state().phase, RealmPhase::Prepared);
    assert_eq!(owner.state().device, Some(DEVICE));
    assert!(owner.state().vdevice.is_none());
    assert!(!model.lock().hardware_attached);
    owner.attach(RID).unwrap();
    assert_eq!(owner.state().requester_id, Some(RID));
    assert_eq!(owner.state().phase, RealmPhase::Attached);
    let mut expected = preparation();
    expected.extend([Call::Vdevice(DEVICE, VIOMMU, RID), Call::Attach(CHILD)]);
    assert_eq!(model.lock().calls, expected);
    owner.close().unwrap();
    expected.extend(cleanup());
    assert_eq!(model.lock().calls, expected);
    assert_eq!(model.lock().drops, ["file", "context", "partition"]);
    assert!(!model.lock().calls.contains(&Call::Destroy(DEVICE)));
    assert_eq!(owner.state().phase, RealmPhase::Closed);
    owner.close().unwrap();
    drop(owner);
    assert_eq!(model.lock().calls, expected);
}

#[test]
fn every_prepare_failure_rolls_back_only_completed_allocations() {
    let setup = preparation();
    let operations = [
        RealmOperation::Associate,
        RealmOperation::Bind,
        RealmOperation::AllocateIoas,
        RealmOperation::DisableHugePages,
        RealmOperation::AllocateParent,
        RealmOperation::AllocateViommu,
        RealmOperation::AllocateChild,
    ];
    for (index, call) in setup.iter().enumerate() {
        let (fake, model) = Fake::new(vec![call.clone()]);
        let error = failed(fake);
        assert_eq!(error.error.primary.operation, operations[index]);
        assert!(error.error.primary.source.to_string().contains("injected"));
        assert!(error.error.rollback.is_none());
        assert!(error.recovery.is_none());
        let mut expected = setup[..=index].to_vec();
        for (allocated_at, id) in [(5, VIOMMU), (4, PARENT), (2, IOAS)] {
            if index > allocated_at {
                expected.push(Call::Destroy(id));
            }
        }
        if index > 0 {
            expected.push(Call::Disassociate);
        }
        expected.push(Call::CloseFile);
        assert_eq!(model.lock().calls, expected, "{call:?}");
        assert_eq!(model.lock().drops, ["file", "context", "partition"]);
    }
}

#[test]
fn attach_failures_detach_even_if_kernel_changed_state_before_error() {
    for call in [Call::Vdevice(DEVICE, VIOMMU, RID), Call::Attach(CHILD)] {
        let (fake, model) = Fake::new(vec![call.clone()]);
        let mut owner = prepared(fake);
        let error = owner.attach(RID).unwrap_err();
        assert!(error.rollback.is_none());
        assert_eq!(owner.state().phase, RealmPhase::Closed);
        let mut expected = preparation();
        expected.push(Call::Vdevice(DEVICE, VIOMMU, RID));
        if call == Call::Attach(CHILD) {
            expected.push(Call::Attach(CHILD));
            expected.extend(cleanup());
            assert_eq!(error.primary.operation, RealmOperation::Attach);
        } else {
            expected.extend(cleanup()[2..].iter().cloned());
            assert_eq!(error.primary.operation, RealmOperation::AllocateVdevice);
        }
        assert_eq!(model.lock().calls, expected);
        assert!(!model.lock().hardware_attached);
        assert_eq!(model.lock().drops, ["file", "context", "partition"]);
    }
}

#[test]
fn unexpected_attached_id_is_detached_but_not_destroyed() {
    let (fake, model) = Fake::new(vec![]);
    model.lock().attach_reply = 999;
    let mut owner = prepared(fake);
    let error = owner.attach(RID).unwrap_err();
    assert_eq!(error.primary.operation, RealmOperation::Attach);
    assert!(error.primary.source.to_string().contains("999"));
    assert!(error.rollback.is_none());
    assert!(!model.lock().hardware_attached);
    assert!(model.lock().calls.contains(&Call::Detach));
    assert!(!model.lock().calls.contains(&Call::Destroy(999)));
}

#[test]
fn failed_attachment_and_failed_rollback_keep_both_errors_and_the_owner() {
    let (fake, model) = Fake::new(vec![Call::Attach(CHILD), Call::Detach]);
    let mut owner = prepared(fake);
    let error = owner.attach(RID).unwrap_err();
    assert_eq!(error.primary.operation, RealmOperation::Attach);
    assert_eq!(
        error.rollback.as_ref().unwrap().operation,
        RealmOperation::Detach
    );
    drop(error);
    assert_eq!(owner.state().phase, RealmPhase::Cleaning);
    assert!(owner.state().attach_attempted);
    assert_eq!(owner.state().vdevice, Some(VDEVICE));
    assert!(model.lock().hardware_attached);
    assert!(model.lock().drops.is_empty());
    model.lock().failures.clear();
    owner.close().unwrap();
    assert!(!model.lock().hardware_attached);
    assert_eq!(model.lock().drops, ["file", "context", "partition"]);
}

#[test]
fn every_cleanup_failure_stops_and_retains_remaining_dependencies_for_retry() {
    let teardown = cleanup();
    let operations = [
        RealmOperation::Detach,
        RealmOperation::DestroyVdevice,
        RealmOperation::DestroyChild,
        RealmOperation::DestroyViommu,
        RealmOperation::DestroyParent,
        RealmOperation::DestroyIoas,
        RealmOperation::Disassociate,
    ];
    for index in 0..operations.len() {
        let (fake, model) = Fake::new(vec![]);
        let mut owner = prepared(fake);
        owner.attach(RID).unwrap();
        model.lock().calls.clear();
        model.lock().failures = vec![teardown[index].clone()];
        let error = owner.close().unwrap_err();
        assert_eq!(error.operation, operations[index]);
        assert_eq!(model.lock().calls, teardown[..=index]);
        assert!(model.lock().drops.is_empty());
        let state = owner.state();
        assert_eq!(state.phase, RealmPhase::Cleaning);
        assert_eq!(state.device, Some(DEVICE));
        assert_eq!(state.attach_attempted, index == 0);
        assert_eq!(state.vdevice.is_some(), index <= 1);
        assert_eq!(state.child.is_some(), index <= 2);
        assert_eq!(state.viommu.is_some(), index <= 3);
        assert_eq!(state.parent.is_some(), index <= 4);
        assert_eq!(state.ioas.is_some(), index <= 5);
        assert!(state.associated);
        assert!(owner.attach(RID + 1).is_err());
        assert_eq!(owner.state(), state);
        assert_eq!(model.lock().calls, teardown[..=index]);
        model.lock().failures.clear();
        owner.close().unwrap();
        let mut expected = teardown[..=index].to_vec();
        expected.extend(teardown[index..].iter().cloned());
        assert_eq!(model.lock().calls, expected);
        assert_eq!(model.lock().drops, ["file", "context", "partition"]);
    }
}

#[test]
fn repeated_or_closed_attachment_does_not_modify_existing_state() {
    let (fake, model) = Fake::new(vec![]);
    let mut owner = prepared(fake);
    owner.attach(RID).unwrap();
    let state = owner.state();
    let calls = model.lock().calls.clone();
    assert!(owner.attach(RID + 1).is_err());
    assert_eq!(owner.state(), state);
    assert_eq!(model.lock().calls, calls);
    owner.close().unwrap();
    let calls = model.lock().calls.clone();
    assert!(owner.attach(RID).is_err());
    assert_eq!(owner.state().phase, RealmPhase::Closed);
    assert_eq!(model.lock().calls, calls);
}

#[test]
fn preparation_failure_returns_both_errors_and_a_recoverable_owner() {
    let (fake, model) = Fake::new(vec![Call::Child(DEVICE, VIOMMU), Call::Destroy(VIOMMU)]);
    let mut failure = failed(fake);
    assert_eq!(
        failure.error.primary.operation,
        RealmOperation::AllocateChild
    );
    assert_eq!(
        failure.error.rollback.as_ref().unwrap().operation,
        RealmOperation::DestroyViommu
    );
    let mut owner = failure.recovery.take().unwrap();
    drop(failure);
    assert!(model.lock().drops.is_empty());
    assert_eq!(owner.state().viommu, Some(VIOMMU));
    assert_eq!(owner.state().child, None);
    model.lock().failures.clear();
    owner.close().unwrap();
    assert_eq!(model.lock().drops, ["file", "context", "partition"]);
}

#[test]
fn abandoning_failed_preparation_retains_the_entire_bundle() {
    let (fake, model) = Fake::new(vec![Call::Child(DEVICE, VIOMMU), Call::Destroy(VIOMMU)]);
    let failure = failed(fake);
    drop(failure);
    assert!(model.lock().drops.is_empty());
    assert!(!model.lock().calls.contains(&Call::Disassociate));
    assert!(!model.lock().calls.contains(&Call::CloseFile));
}

#[test]
fn abandoning_failed_detach_retains_the_entire_bundle() {
    let (fake, model) = Fake::new(vec![]);
    let mut owner = prepared(fake);
    owner.attach(RID).unwrap();
    model.lock().failures = vec![Call::Detach];
    drop(owner);
    assert!(model.lock().hardware_attached);
    assert!(model.lock().drops.is_empty());
    assert!(!model.lock().calls.contains(&Call::Destroy(VDEVICE)));
    assert!(!model.lock().calls.contains(&Call::CloseFile));
}

#[test]
fn drop_cleans_up_a_prepared_owner_without_creating_an_attachment() {
    let (fake, model) = Fake::new(vec![]);
    drop(prepared(fake));
    let mut expected = preparation();
    expected.extend(cleanup()[2..].iter().cloned());
    assert_eq!(model.lock().calls, expected);
    assert_eq!(model.lock().drops, ["file", "context", "partition"]);
}
