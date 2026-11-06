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
    fn record_protected_mapping(
        &self,
        requester_id: u32,
        range: std::ops::Range<u64>,
        host_base: u64,
    ) -> Result<(), VfioVmError> {
        self.association
            .record_protected_mapping(requester_id, range, host_base)
    }

    fn complete_protected_unlock(&self, requester_id: u32) -> Result<(), VfioVmError> {
        self.association.complete_protected_unlock(requester_id)
    }

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

    struct LedgerVm {
        fail: bool,
        calls: Arc<parking_lot::Mutex<Vec<&'static str>>>,
    }

    impl VfioVm for LedgerVm {
        fn add_file(&self, _: BorrowedFd<'_>) -> Result<(), VfioVmError> {
            unreachable!("ledger forwarding test")
        }
        fn remove_file(&self, _: BorrowedFd<'_>) -> Result<(), VfioVmError> {
            unreachable!("ledger forwarding test")
        }
        fn record_protected_mapping(
            &self,
            rid: u32,
            range: std::ops::Range<u64>,
            host: u64,
        ) -> Result<(), VfioVmError> {
            assert_eq!((rid, range, host), (0x1_0100, 0x4000..0x6000, 0x9000));
            self.calls.lock().push("record");
            if self.fail {
                return Err(VfioVmError::new(std::io::Error::other("record failure")));
            }
            Ok(())
        }
        fn complete_protected_unlock(&self, rid: u32) -> Result<(), VfioVmError> {
            assert_eq!(rid, 0x1_0100);
            self.calls.lock().push("unlock");
            if self.fail {
                return Err(VfioVmError::new(std::io::Error::other("unlock failure")));
            }
            Ok(())
        }
    }

    #[test]
    fn protected_hook_forwarding_preserves_results_and_ram_custody() {
        use std::error::Error as _;
        for fail in [false, true] {
            let drops = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let vm = RetainedVm {
                association: Arc::new(LedgerVm {
                    fail,
                    calls: calls.clone(),
                }),
                _memory: Arc::new(Memory(drops.clone())),
            };
            for (result, source) in [
                (
                    vm.record_protected_mapping(0x1_0100, 0x4000..0x6000, 0x9000),
                    "record failure",
                ),
                (vm.complete_protected_unlock(0x1_0100), "unlock failure"),
            ] {
                if fail {
                    assert_eq!(result.unwrap_err().source().unwrap().to_string(), source);
                } else {
                    result.unwrap();
                }
                assert_eq!(drops.load(Ordering::Relaxed), 0);
            }
            assert_eq!(*calls.lock(), ["record", "unlock"]);
            drop(vm);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
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
