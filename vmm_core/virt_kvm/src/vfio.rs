// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition-owned KVM/VFIO association access.

use crate::KvmPartition;
use crate::KvmPartitionInner;
use parking_lot::Mutex;
use pci_core::vfio::VfioVm;
use pci_core::vfio::VfioVmError;
use pci_core::vfio::VfioVmProvider;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

fn initialize_once<T, E>(
    slot: &Mutex<Option<T>>,
    create: impl FnOnce() -> Result<T, E>,
) -> Result<(), E> {
    let mut slot = slot.lock();
    if slot.is_none() {
        *slot = Some(create()?);
    }
    Ok(())
}

impl KvmPartition {
    /// Get an owning handle to this partition's KVM/VFIO bridge.
    ///
    /// The bridge is created on the first request and cached for the partition
    /// lifetime. Creation failure leaves the cache empty and returns the kernel
    /// error. This method does not change device-assignment policy or associate
    /// any files.
    pub fn vfio_assignment(&self) -> Result<KvmVfioAssignment, kvm::Error> {
        create_assignment(self.inner.clone())
    }
}

fn create_assignment(partition: Arc<KvmPartitionInner>) -> Result<KvmVfioAssignment, kvm::Error> {
    initialize_once(&partition.vfio_device, || {
        partition.kvm.create_vfio_device()
    })?;
    Ok(KvmVfioAssignment { partition })
}

pub(super) fn provider(partition: Arc<KvmPartitionInner>) -> Arc<dyn VfioVmProvider> {
    Arc::new(move || -> Result<Arc<dyn VfioVm>, VfioVmError> {
        Ok(Arc::new(
            create_assignment(partition.clone()).map_err(VfioVmError::new)?,
        ))
    })
}

/// Keeps the KVM partition and its bridge alive for an assignment owner.
///
/// No raw KVM VM or bridge fd is exposed. The owner must retain this handle and
/// the original VFIO file through IOMMUFD setup, rollback, and teardown.
/// Dropping a handle does not remove individual file associations: remove them
/// explicitly after detaching DMA and destroying dependent IOMMUFD objects.
/// The bridge closes only when the last partition owner is released.
#[derive(Clone)]
pub struct KvmVfioAssignment {
    partition: Arc<KvmPartitionInner>,
}

impl KvmVfioAssignment {
    /// Associate a VFIO file before binding its cdev to IOMMUFD.
    pub fn add_file(&self, file: BorrowedFd<'_>) -> Result<(), kvm::Error> {
        self.partition
            .vfio_device
            .lock()
            .as_ref()
            .expect("assignment handle requires an initialized VFIO bridge")
            .add_file(file)
    }

    /// Remove a VFIO association after its dependent objects have been released.
    ///
    /// On failure, retain the handle and file for explicit error recovery.
    pub fn remove_file(&self, file: BorrowedFd<'_>) -> Result<(), kvm::Error> {
        self.partition
            .vfio_device
            .lock()
            .as_ref()
            .expect("assignment handle requires an initialized VFIO bridge")
            .remove_file(file)
    }
}

impl VfioVm for KvmVfioAssignment {
    #[cfg(guest_arch = "aarch64")]
    fn check_interrupt_routes(&self) -> Result<(), VfioVmError> {
        self.partition
            .check_assignment_interrupt_routes()
            .map_err(VfioVmError::new)
    }

    fn add_file(&self, file: BorrowedFd<'_>) -> Result<(), VfioVmError> {
        KvmVfioAssignment::add_file(self, file).map_err(VfioVmError::new)
    }

    fn remove_file(&self, file: BorrowedFd<'_>) -> Result<(), VfioVmError> {
        KvmVfioAssignment::remove_file(self, file).map_err(VfioVmError::new)
    }

    #[cfg(guest_arch = "aarch64")]
    fn register_evidence(
        &self,
        requester_id: u32,
        service: std::sync::Weak<dyn tdisp::host::EvidenceService>,
    ) -> Result<(), VfioVmError> {
        self.partition
            .register_rhi_evidence(requester_id, service)
            .map_err(VfioVmError::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    #[test]
    fn provider_defers_creation_and_preserves_backend_error() {
        use std::error::Error as _;
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let provider: Arc<dyn VfioVmProvider> =
            Arc::new(move || -> Result<Arc<dyn VfioVm>, VfioVmError> {
                counter.fetch_add(1, Ordering::Relaxed);
                Err(VfioVmError::new(std::io::Error::from_raw_os_error(
                    libc::ENOTTY,
                )))
            });
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let error = provider.create().err().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::ENOTTY)
        );
    }

    #[test]
    fn failed_creation_can_be_retried_without_caching_a_handle() {
        let slot = Mutex::new(None::<u32>);
        assert_eq!(
            initialize_once(&slot, || Err("create failed")),
            Err("create failed")
        );
        assert!(slot.lock().is_none());
        initialize_once(&slot, || Ok::<_, &str>(41)).unwrap();
        initialize_once(&slot, || Err("must not recreate")).unwrap();
        assert_eq!(*slot.lock(), Some(41));
    }

    #[test]
    fn concurrent_requests_create_one_bridge() {
        let slot = Mutex::new(None);
        let calls = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let mut threads = Vec::new();
            for _ in 0..8 {
                threads.push(scope.spawn(|| {
                    initialize_once(&slot, || {
                        calls.fetch_add(1, Ordering::Relaxed);
                        Ok::<_, ()>(73)
                    })
                    .unwrap();
                }));
            }
            for thread in threads {
                thread.join().unwrap();
            }
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(*slot.lock(), Some(73));
    }

    #[test]
    fn cached_device_lives_until_the_owner_is_dropped() {
        struct Device(Arc<AtomicUsize>);
        impl Drop for Device {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let slot = Mutex::new(None);
        initialize_once(&slot, || Ok::<_, ()>(Device(drops.clone()))).unwrap();
        initialize_once(&slot, || Err("must not recreate")).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(slot);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}
