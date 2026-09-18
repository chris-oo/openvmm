// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backend-neutral access to a VM's Linux VFIO association service.

use std::os::fd::BorrowedFd;
use std::sync::Arc;
use std::sync::Weak;
use tdisp::host::EvidenceService;

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
    /// Record a protected mapping attempt for a registered full assignment.
    ///
    /// Called synchronously under device admission and its access gate, before
    /// the mapping ioctl. Retain conservative history on error. Implementations
    /// must validate the requester ID and must not re-enter the device service.
    fn record_protected_mapping(
        &self,
        _requester_id: u32,
        _range: std::ops::Range<u64>,
        _host_base: u64,
    ) -> Result<(), VfioVmError> {
        Err(VfioVmError::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "protected mapping accounting is not supported by this VM",
        )))
    }

    /// Retire protected mapping history after fully checked native UNLOCK.
    ///
    /// The pinned platform's successful UNLOCK must prove release of every
    /// protected device range. The caller holds device admission and its gate,
    /// with access denied. Validate all prerequisites before retiring history;
    /// errors must leave it unchanged. This does not release RAM or shared DMA.
    /// Do not re-enter the device service. Only infallible bookkeeping may follow
    /// successful completion in the caller before access is reopened.
    fn complete_protected_unlock(&self, _requester_id: u32) -> Result<(), VfioVmError> {
        Err(VfioVmError::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "checked protected UNLOCK completion is not supported by this VM",
        )))
    }

    /// Check acknowledged hypervisor interrupt-route operations.
    ///
    /// Native assignment frontends must check after MSI updates, IRQ stop, and
    /// destruction of every route owner. A failure is permanent: retain the
    /// frontend/assignment ownership and do not report checked release.
    fn check_interrupt_routes(&self) -> Result<(), VfioVmError> {
        Err(VfioVmError::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "checked interrupt routes are not supported by this VM",
        )))
    }

    /// Whether an assignment has made ordinary VM/RAM destruction unsafe.
    ///
    /// This is conservative and need not become false after guest shutdown.
    /// Retain the whole VM memory owner until the containing model has ended
    /// or the backend has positively established safe resource release.
    fn requires_assignment_retention(&self) -> bool {
        false
    }

    /// Associate a VFIO cdev before binding it to IOMMUFD.
    fn add_file(&self, file: BorrowedFd<'_>) -> Result<(), VfioVmError>;
    /// Remove the same open file description after dependency teardown.
    fn remove_file(&self, file: BorrowedFd<'_>) -> Result<(), VfioVmError>;
    /// Register evidence for a final guest requester ID before the first VP run.
    ///
    /// The caller must retain the strong service reference for the assigned
    /// device's lifetime and use this device's own VM association. The VM keeps
    /// only a weak reference, avoiding a cycle through the assignment owner.
    /// This is an explicit evidence-only opt-in, not permission for DMA or BAR
    /// access. Backends without native evidence routing reject the request.
    fn register_evidence(
        &self,
        _requester_id: u32,
        _service: Weak<dyn EvidenceService>,
    ) -> Result<(), VfioVmError> {
        Err(VfioVmError::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "native device evidence routing is not supported by this VM",
        )))
    }

    /// Register a full native assignment after the private PCI address view
    /// and checked device-access gate are configured.
    ///
    /// The service must own the CCA-aware shared DMA mappings. It must not use
    /// an ordinary full-RAM DMA mapper. The VM prepares private RAM before any
    /// VP enters KVM and advertises base RHI features only after that succeeds.
    /// Protected mapping attempts remain retained until checked native UNLOCK.
    /// The caller must retain the entire VM and its RAM
    /// owners on failed shutdown; this API does not promise clean teardown.
    /// The caller retains the strong service reference through checked teardown.
    fn register_assignment(
        &self,
        _requester_id: u32,
        _service: Weak<dyn EvidenceService>,
    ) -> Result<(), VfioVmError> {
        Err(VfioVmError::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "native device assignment is not supported by this VM",
        )))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use test_with_tracing::test;

    struct UnsupportedVm;

    impl VfioVm for UnsupportedVm {
        fn add_file(&self, _: BorrowedFd<'_>) -> Result<(), VfioVmError> {
            unreachable!("no VFIO file in this test");
        }
        fn remove_file(&self, _: BorrowedFd<'_>) -> Result<(), VfioVmError> {
            unreachable!("no VFIO file in this test");
        }
    }

    #[test]
    fn protected_platform_hooks_never_default_to_success() {
        for result in [
            UnsupportedVm.record_protected_mapping(0x100, 0x4000..0x6000, 0x9000),
            UnsupportedVm.complete_protected_unlock(0x100),
        ] {
            let error = result.unwrap_err();
            assert_eq!(
                error
                    .source()
                    .unwrap()
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .kind(),
                std::io::ErrorKind::Unsupported,
            );
        }
    }
}
