// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backend-neutral access to a VM's Linux VFIO association service.

use std::os::fd::BorrowedFd;
use std::sync::Arc;

/// A backend failure while creating or using the VM association service.
#[derive(Debug, thiserror::Error)]
#[error("VFIO VM association failed")]
pub struct VfioVmError {
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl VfioVmError {
    /// Preserve the backend's typed error and source chain.
    pub fn new(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(error),
        }
    }
}

/// An owning VM reference used throughout VFIO setup and teardown.
///
/// The allocation owner must retain this service and the VFIO open file
/// description until DMA is stopped and dependent IOMMUFD objects are gone.
pub trait VfioVm: Send + Sync {
    /// Associate a VFIO cdev before binding it to IOMMUFD.
    fn add_file(&self, file: BorrowedFd<'_>) -> Result<(), VfioVmError>;
    /// Remove the same open file description after dependency teardown.
    fn remove_file(&self, file: BorrowedFd<'_>) -> Result<(), VfioVmError>;
}

/// Creates association access only when a device actually needs it.
///
/// Obtaining or storing a provider must not create a kernel bridge.
pub trait VfioVmProvider: Send + Sync {
    /// Get owning association access, propagating unsupported-host errors.
    fn create(&self) -> Result<Arc<dyn VfioVm>, VfioVmError>;
}

impl<F> VfioVmProvider for F
where
    F: Fn() -> Result<Arc<dyn VfioVm>, VfioVmError> + Send + Sync,
{
    fn create(&self) -> Result<Arc<dyn VfioVm>, VfioVmError> {
        self()
    }
}
