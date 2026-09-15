// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Owned, unmapped Realm assignment objects. This is not a PCI device resolver
//! or a TDISP state machine, and does not register DMA or guest BAR mappings.

mod objects;

pub use objects::OperationError;
pub use objects::RealmOperation;
pub use objects::RealmPhase;
pub use objects::RealmSetupError;
pub use objects::RealmState;

use objects::ObjectOwner;
use objects::Operations;
use pci_core::vfio::VfioVm;
use pci_core::vfio::VfioVmProvider;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use vfio_sys::cdev::CdevDevice;
use vfio_sys::iommufd::IOMMU_HWPT_ALLOC_NEST_PARENT;
use vfio_sys::iommufd::IOMMU_HWPT_DATA_ARM_SMMUV3;
use vfio_sys::iommufd::IOMMU_HWPT_DATA_NONE;
use vfio_sys::iommufd::IommuHwptArmSmmuv3;
use vfio_sys::iommufd::IommufdCtx;

/// Preparation failure, including the owner needed to retry failed rollback.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct RealmPrepareError {
    /// Original setup failure and any rollback failure.
    #[source]
    pub error: RealmSetupError,
    /// Present when rollback failed. Do not discard this owner as ordinary
    /// error text; explicitly retry its cleanup or fail the VM lifecycle.
    pub recovery: Option<RealmDevice>,
}

/// A single device's Realm IOMMUFD graph and owning VM association.
///
/// Requires exclusive lifecycle control of a fresh, unbound, unassociated VFIO open file
/// description. Duplicate descriptors can delay final unbinding.
/// Call [`Self::close`] explicitly. Failed cleanup retains the remaining state;
/// abandoning it while cleanup still fails quarantines the bundle until process
/// exit, with an error log. This is not clean teardown.
#[derive(Debug)]
#[must_use = "retain Realm objects until explicit close or recovery"]
pub struct RealmDevice {
    owner: ObjectOwner<LinuxOperations>,
}

impl RealmDevice {
    /// Associate, bind, and allocate through the S1-bypass child HWPT.
    ///
    /// The provider is invoked before any file association or binding. No
    /// vdevice or attachment is created until the final guest RID is supplied
    /// to [`Self::attach`]. The IOAS remains empty.
    pub fn prepare(
        provider: Option<&dyn VfioVmProvider>,
        cdev: File,
        iommufd: File,
    ) -> Result<Self, RealmPrepareError> {
        let association = provider
            .ok_or_else(|| anyhow::anyhow!("Realm assignment requires a VM association provider"))
            .and_then(|provider| provider.create().map_err(anyhow::Error::from))
            .map_err(|source| RealmPrepareError {
                error: RealmSetupError {
                    primary: OperationError {
                        operation: RealmOperation::Provider,
                        source,
                    },
                    rollback: None,
                },
                recovery: None,
            })?;
        let operations = LinuxOperations {
            cdev: Some(CdevDevice::from_file(cdev)),
            context: IommufdCtx::from_file(iommufd),
            association,
        };
        ObjectOwner::prepare(operations)
            .map(|owner| Self { owner })
            .map_err(|failure| RealmPrepareError {
                error: failure.error,
                recovery: failure.recovery.map(|owner| Self { owner }),
            })
    }

    /// Create the vdevice for a final `(segment << 16) | PCI_DEVID` identity,
    /// then attach its child HWPT. Only a prepared owner may attach, once.
    ///
    /// This does not perform TDISP LOCK/RUN or map RAM into the IOAS.
    pub fn attach(&mut self, requester_id: u32) -> Result<(), RealmSetupError> {
        self.owner.attach(requester_id)
    }

    /// Snapshot of owned IDs and lifecycle progress, including failed cleanup.
    pub fn state(&self) -> RealmState {
        self.owner.state()
    }

    /// Detach, destroy owned objects in dependency order, remove the KVM file
    /// association, and close the VFIO file before releasing other handles.
    ///
    /// Stops on the first error and preserves remaining state for retry. No
    /// preparation or attachment may resume once cleanup has started. The
    /// caller must stop any future DMA/access before calling this method.
    pub fn close(&mut self) -> Result<(), OperationError> {
        self.owner.close()
    }
}

// Final successful release order: cdev (which unbinds), IOMMUFD, then VM.
struct LinuxOperations {
    cdev: Option<CdevDevice>,
    context: IommufdCtx,
    association: Arc<dyn VfioVm>,
}

impl LinuxOperations {
    fn cdev(&self) -> &CdevDevice {
        self.cdev
            .as_ref()
            .expect("live Realm owner requires its VFIO file")
    }
}

impl Operations for LinuxOperations {
    fn associate(&mut self) -> anyhow::Result<()> {
        self.association.add_file(self.cdev().as_fd())?;
        Ok(())
    }

    fn bind(&mut self) -> anyhow::Result<u32> {
        self.cdev().bind_iommufd(self.context.as_raw_fd())
    }

    fn allocate_ioas(&mut self) -> anyhow::Result<u32> {
        self.context.ioas_alloc()
    }

    fn disable_huge_pages(&mut self, ioas: u32) -> anyhow::Result<()> {
        self.context.ioas_set_huge_pages(ioas, false)?;
        Ok(())
    }

    fn allocate_parent(&mut self, device: u32, ioas: u32) -> anyhow::Result<u32> {
        self.context.hwpt_alloc(
            IOMMU_HWPT_ALLOC_NEST_PARENT,
            device,
            ioas,
            IOMMU_HWPT_DATA_NONE,
            None,
        )
    }

    fn allocate_viommu(&mut self, device: u32, parent: u32) -> anyhow::Result<u32> {
        Ok(self.context.viommu_alloc_realm(device, parent)?)
    }

    fn allocate_child(&mut self, device: u32, viommu: u32) -> anyhow::Result<u32> {
        self.context.hwpt_alloc(
            0,
            device,
            viommu,
            IOMMU_HWPT_DATA_ARM_SMMUV3,
            Some(&IommuHwptArmSmmuv3::s1_bypass()),
        )
    }

    fn allocate_vdevice(&mut self, device: u32, viommu: u32, rid: u32) -> anyhow::Result<u32> {
        self.context.vdevice_alloc(viommu, device, u64::from(rid))
    }

    fn attach(&mut self, child: u32) -> anyhow::Result<u32> {
        self.cdev().attach_ioas(child)
    }

    fn detach(&mut self) -> anyhow::Result<()> {
        self.cdev().detach_ioas()
    }

    fn destroy(&mut self, id: u32) -> anyhow::Result<()> {
        self.context.destroy(id)
    }

    fn disassociate(&mut self) -> anyhow::Result<()> {
        self.association.remove_file(self.cdev().as_fd())?;
        Ok(())
    }

    fn close_file(&mut self) {
        self.cdev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pci_core::vfio::VfioVmError;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    #[test]
    fn unavailable_provider_fails_before_file_operations() {
        let error = RealmDevice::prepare(
            None,
            File::open("/dev/null").unwrap(),
            File::open("/dev/null").unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.error.primary.operation, RealmOperation::Provider);
        assert!(error.recovery.is_none());
        assert!(error.error.rollback.is_none());
    }

    #[test]
    fn provider_error_is_preserved_before_file_operations() {
        let calls = AtomicUsize::new(0);
        let provider = || -> Result<Arc<dyn VfioVm>, VfioVmError> {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(VfioVmError::new(std::io::Error::other(
                "provider unavailable",
            )))
        };
        let error = RealmDevice::prepare(
            Some(&provider),
            File::open("/dev/null").unwrap(),
            File::open("/dev/null").unwrap(),
        )
        .unwrap_err();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(error.error.primary.operation, RealmOperation::Provider);
        assert!(
            error
                .error
                .primary
                .source
                .downcast_ref::<VfioVmError>()
                .is_some()
        );
        assert!(error.recovery.is_none());
    }
}
