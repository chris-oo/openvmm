// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;

/// Lifecycle states of an unmapped Realm assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealmPhase {
    /// No association or objects have been created.
    New,
    /// The S1-bypass child exists; no vdevice or attachment exists.
    Prepared,
    /// A vdevice exists and the requested child HWPT is attached.
    Attached,
    /// Only dependency-ordered cleanup retry is permitted.
    Cleaning,
    /// All objects, associations, and owned handles have been released.
    Closed,
}

/// Operation at which setup or cleanup failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealmOperation {
    /// Confirm all frontend and protected-memory access is revoked.
    RevokeAccess,
    /// Obtain partition-owned association access.
    Provider,
    /// Associate the VFIO file with KVM.
    Associate,
    /// Bind the VFIO cdev to IOMMUFD.
    Bind,
    /// Allocate the empty IOAS.
    AllocateIoas,
    /// Disable IOAS huge-page combining.
    DisableHugePages,
    /// Allocate the nesting-parent HWPT.
    AllocateParent,
    /// Allocate the Realm SMMUv3 vIOMMU.
    AllocateViommu,
    /// Allocate the S1-bypass child HWPT.
    AllocateChild,
    /// Allocate the vdevice for the final guest requester identity.
    AllocateVdevice,
    /// Attach the cdev to the child HWPT.
    Attach,
    /// Detach a possibly installed child HWPT.
    Detach,
    /// Destroy the owned vdevice.
    DestroyVdevice,
    /// Destroy the owned child HWPT.
    DestroyChild,
    /// Destroy the owned Realm vIOMMU.
    DestroyViommu,
    /// Destroy the owned nesting parent.
    DestroyParent,
    /// Destroy the owned IOAS.
    DestroyIoas,
    /// Remove the KVM file association.
    Disassociate,
}

/// Owned IDs and progress. The bound device ID is released by closing the VFIO
/// file, not through `IOMMU_DESTROY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealmState {
    /// Permitted lifecycle operations.
    pub phase: RealmPhase,
    /// The owner must still remove its KVM file association.
    pub associated: bool,
    /// Bound device ID, released through VFIO file close.
    pub device: Option<u32>,
    /// Owned IOAS ID.
    pub ioas: Option<u32>,
    /// Owned nesting-parent HWPT ID.
    pub parent: Option<u32>,
    /// Owned Realm vIOMMU ID.
    pub viommu: Option<u32>,
    /// Owned S1-bypass child HWPT ID.
    pub child: Option<u32>,
    /// Owned vdevice ID.
    pub vdevice: Option<u32>,
    /// Identity supplied at attachment, including a failed attempt.
    pub requester_id: Option<u32>,
    /// Detachment is required, even if attachment completion failed.
    pub attach_attempted: bool,
}

impl RealmState {
    const fn new() -> Self {
        Self {
            phase: RealmPhase::New,
            associated: false,
            device: None,
            ioas: None,
            parent: None,
            viommu: None,
            child: None,
            vdevice: None,
            requester_id: None,
            attach_attempted: false,
        }
    }
}

/// An operation error, without discarding the backend's error chain.
#[derive(Debug, thiserror::Error)]
#[error("Realm operation {operation:?} failed")]
pub struct OperationError {
    /// Failed operation.
    pub operation: RealmOperation,
    /// Backend error and its original source chain.
    #[source]
    pub source: anyhow::Error,
}

/// Both the original setup error and any failure of its rollback.
///
/// Invalid phase requests do not mutate or roll back an existing assignment.
#[derive(Debug, thiserror::Error)]
#[error("{primary}; rollback error: {rollback:?}")]
pub struct RealmSetupError {
    /// Original setup or phase-validation failure.
    #[source]
    pub primary: OperationError,
    /// Failure of automatic rollback, if attempted.
    pub rollback: Option<OperationError>,
}

pub(super) trait Operations: Send + Sync {
    fn associate(&mut self) -> anyhow::Result<()>;
    fn bind(&mut self) -> anyhow::Result<u32>;
    fn allocate_ioas(&mut self) -> anyhow::Result<u32>;
    fn disable_huge_pages(&mut self, ioas: u32) -> anyhow::Result<()>;
    fn allocate_parent(&mut self, device: u32, ioas: u32) -> anyhow::Result<u32>;
    fn allocate_viommu(&mut self, device: u32, parent: u32) -> anyhow::Result<u32>;
    fn allocate_child(&mut self, device: u32, viommu: u32) -> anyhow::Result<u32>;
    fn allocate_vdevice(&mut self, device: u32, viommu: u32, rid: u32) -> anyhow::Result<u32>;
    fn attach(&mut self, child: u32) -> anyhow::Result<u32>;
    fn detach(&mut self) -> anyhow::Result<()>;
    fn destroy(&mut self, id: u32) -> anyhow::Result<()>;
    fn disassociate(&mut self) -> anyhow::Result<()>;
    fn close_file(&mut self);
}

fn step<T>(operation: RealmOperation, result: anyhow::Result<T>) -> Result<T, OperationError> {
    result.map_err(|source| OperationError { operation, source })
}

struct Bundle<B> {
    operations: B,
    state: RealmState,
}

pub(super) struct ObjectOwner<B: Operations> {
    bundle: Option<Box<Bundle<B>>>,
}

impl<B: Operations> fmt::Debug for ObjectOwner<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealmObjectOwner")
            .field("state", &self.state())
            .finish()
    }
}

#[derive(Debug)]
pub(super) struct PrepareFailure<B: Operations> {
    pub error: RealmSetupError,
    pub recovery: Option<ObjectOwner<B>>,
}

impl<B: Operations> ObjectOwner<B> {
    #[cfg(test)]
    pub(super) fn closed_for_test() -> Self {
        Self { bundle: None }
    }

    pub(super) fn retain(&mut self) {
        if let Some(bundle) = self.bundle.take() {
            std::mem::forget(bundle);
        }
    }
    pub fn prepare(operations: B) -> Result<Self, PrepareFailure<B>> {
        let mut owner = Self {
            bundle: Some(Box::new(Bundle {
                operations,
                state: RealmState::new(),
            })),
        };
        match owner.prepare_inner() {
            Ok(()) => Ok(owner),
            Err(primary) => {
                let rollback = owner.close().err();
                let recovery = rollback.is_some().then_some(owner);
                Err(PrepareFailure {
                    error: RealmSetupError { primary, rollback },
                    recovery,
                })
            }
        }
    }

    fn prepare_inner(&mut self) -> Result<(), OperationError> {
        use RealmOperation::*;
        let bundle = self.bundle.as_mut().expect("new owner");
        let ops = &mut bundle.operations;
        let state = &mut bundle.state;
        step(Associate, ops.associate())?;
        state.associated = true;
        let device = step(Bind, ops.bind())?;
        state.device = Some(device);
        let ioas = step(AllocateIoas, ops.allocate_ioas())?;
        state.ioas = Some(ioas);
        step(DisableHugePages, ops.disable_huge_pages(ioas))?;
        let parent = step(AllocateParent, ops.allocate_parent(device, ioas))?;
        state.parent = Some(parent);
        let viommu = step(AllocateViommu, ops.allocate_viommu(device, parent))?;
        state.viommu = Some(viommu);
        let child = step(AllocateChild, ops.allocate_child(device, viommu))?;
        state.child = Some(child);
        state.phase = RealmPhase::Prepared;
        Ok(())
    }

    pub fn state(&self) -> RealmState {
        match &self.bundle {
            Some(bundle) => bundle.state,
            None => RealmState {
                phase: RealmPhase::Closed,
                ..RealmState::new()
            },
        }
    }

    pub fn with_attached<T>(
        &mut self,
        call: impl FnOnce(&mut B, u32) -> T,
    ) -> Result<T, RealmPhase> {
        let phase = self.state().phase;
        if phase != RealmPhase::Attached {
            return Err(phase);
        }
        let bundle = self.bundle.as_mut().expect("attached owner");
        let vdevice = bundle.state.vdevice.expect("attached vdevice");
        Ok(call(&mut bundle.operations, vdevice))
    }

    pub fn attach(&mut self, rid: u32) -> Result<(), RealmSetupError> {
        if self.state().phase != RealmPhase::Prepared {
            return Err(RealmSetupError {
                primary: OperationError {
                    operation: RealmOperation::Attach,
                    source: anyhow::anyhow!(
                        "attachment requires Prepared state, got {:?}",
                        self.state().phase
                    ),
                },
                rollback: None,
            });
        }
        self.attach_inner(rid).map_err(|primary| {
            let rollback = self.close().err();
            RealmSetupError { primary, rollback }
        })
    }

    fn attach_inner(&mut self, rid: u32) -> Result<(), OperationError> {
        let bundle = self.bundle.as_mut().expect("prepared owner");
        let state = &mut bundle.state;
        let device = state.device.expect("prepared device");
        let viommu = state.viommu.expect("prepared vIOMMU");
        let child = state.child.expect("prepared child");
        state.requester_id = Some(rid);
        state.vdevice = Some(step(
            RealmOperation::AllocateVdevice,
            bundle.operations.allocate_vdevice(device, viommu, rid),
        )?);
        state.attach_attempted = true;
        let actual = step(RealmOperation::Attach, bundle.operations.attach(child))?;
        if actual != child {
            return Err(OperationError {
                operation: RealmOperation::Attach,
                source: anyhow::anyhow!(
                    "requested HWPT {child}, kernel attached unexpected HWPT {actual}"
                ),
            });
        }
        state.phase = RealmPhase::Attached;
        Ok(())
    }

    pub fn close(&mut self) -> Result<(), OperationError> {
        use RealmOperation::*;
        let Some(bundle) = self.bundle.as_mut() else {
            return Ok(());
        };
        let state = &mut bundle.state;
        let ops = &mut bundle.operations;
        state.phase = RealmPhase::Cleaning;
        if state.attach_attempted {
            step(Detach, ops.detach())?;
            state.attach_attempted = false;
        }
        for (operation, owned) in [
            (DestroyVdevice, &mut state.vdevice),
            (DestroyChild, &mut state.child),
            (DestroyViommu, &mut state.viommu),
            (DestroyParent, &mut state.parent),
            (DestroyIoas, &mut state.ioas),
        ] {
            if let Some(id) = *owned {
                step(operation, ops.destroy(id))?;
                *owned = None;
            }
        }
        if state.associated {
            step(Disassociate, ops.disassociate())?;
            state.associated = false;
        }
        ops.close_file();
        self.bundle = None;
        Ok(())
    }
}

impl<B: Operations> Drop for ObjectOwner<B> {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            let bundle = self
                .bundle
                .take()
                .expect("failed cleanup retains its bundle");
            tracelimit::error_ratelimited!(
                error = ?error, state = ?bundle.state,
                "Realm cleanup failed; retaining resources; keep DMA-reachable RAM and this process alive until the device/model domain stops"
            );
            // Dropping individual handles now could release dependencies out of
            // order. Explicit close/recovery is the normal, retryable path.
            std::mem::forget(bundle);
        }
    }
}

#[cfg(test)]
mod tests;
