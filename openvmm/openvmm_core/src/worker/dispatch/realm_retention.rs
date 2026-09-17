// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Keep RAM region ownership alongside every Realm device association.

use pci_core::vfio::VfioVm;
use pci_core::vfio::VfioVmError;
use pci_core::vfio::VfioVmProvider;
use std::os::fd::BorrowedFd;
use std::sync::Arc;
use std::sync::Weak;
use tdisp::host::EvidenceService;

pub(super) fn retain_provider<M: Send + Sync + 'static>(
    provider: Arc<dyn VfioVmProvider>,
    memory: Arc<M>,
) -> Arc<dyn VfioVmProvider> {
    Arc::new(move || -> Result<Arc<dyn VfioVm>, VfioVmError> {
        Ok(Arc::new(RetainedVm {
            association: provider.create()?,
            _memory: memory.clone(),
        }))
    })
}

struct RetainedVm<M> {
    association: Arc<dyn VfioVm>,
    // Drop the association before releasing the RAM-region owner.
    _memory: Arc<M>,
}

impl<M: Send + Sync> VfioVm for RetainedVm<M> {
    fn requires_assignment_retention(&self) -> bool {
        self.association.requires_assignment_retention()
    }

    fn check_interrupt_routes(&self) -> Result<(), VfioVmError> {
        self.association.check_interrupt_routes()
    }

    fn add_file(&self, file: BorrowedFd<'_>) -> Result<(), VfioVmError> {
        self.association.add_file(file)
    }

    fn remove_file(&self, file: BorrowedFd<'_>) -> Result<(), VfioVmError> {
        self.association.remove_file(file)
    }

    fn register_evidence(
        &self,
        rid: u32,
        service: Weak<dyn EvidenceService>,
    ) -> Result<(), VfioVmError> {
        self.association.register_evidence(rid, service)
    }

    fn register_assignment(
        &self,
        rid: u32,
        service: Weak<dyn EvidenceService>,
    ) -> Result<(), VfioVmError> {
        self.association.register_assignment(rid, service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    struct Memory(Arc<AtomicUsize>);

    impl Drop for Memory {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct Vm;

    impl VfioVm for Vm {
        fn requires_assignment_retention(&self) -> bool {
            true
        }
        fn add_file(&self, _: BorrowedFd<'_>) -> Result<(), VfioVmError> {
            Err(VfioVmError::new(std::io::Error::other("add failure")))
        }
        fn remove_file(&self, _: BorrowedFd<'_>) -> Result<(), VfioVmError> {
            Ok(())
        }
        fn check_interrupt_routes(&self) -> Result<(), VfioVmError> {
            Err(VfioVmError::new(std::io::Error::other("IRQ failure")))
        }
    }

    #[test]
    fn association_owns_ram_regions_after_provider_and_worker_release() {
        let drops = Arc::new(AtomicUsize::new(0));
        let memory = Arc::new(Memory(drops.clone()));
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let provider = retain_provider(
            Arc::new(move || -> Result<Arc<dyn VfioVm>, VfioVmError> {
                counter.fetch_add(1, Ordering::Relaxed);
                Ok(Arc::new(Vm))
            }),
            memory.clone(),
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let association = provider.create().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        drop(memory);
        drop(provider);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert!(association.requires_assignment_retention());
        assert!(association.check_interrupt_routes().is_err());
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(association.add_file(file.as_fd()).is_err());
        association.remove_file(file.as_fd()).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(association);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}
