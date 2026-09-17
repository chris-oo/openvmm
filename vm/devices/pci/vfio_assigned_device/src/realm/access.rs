// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The frontend and native coordinator share this synchronous access boundary.
//! Realm BARs use intercepted access before LOCK, never an unacknowledged
//! shared direct map. After LOCK only the separate MSI-X pages remain accessible.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::ops::Range;
use tdisp::host::SharedMapping;

pub(crate) const GRANULE: u64 = 4096;

/// Retain the VMA used by IOMMU_IOAS_MAP until acknowledged IOAS unmap.
#[derive(Debug)]
pub(crate) struct SharedIoasMapping {
    pub mapping: SharedMapping,
    pub memory: std::sync::Arc<sparse_mmap::SparseMapping>,
    pub offset: usize,
}

impl std::ops::Deref for SharedIoasMapping {
    type Target = SharedMapping;

    fn deref(&self) -> &Self::Target {
        &self.mapping
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BarRange {
    pub guest: Range<u64>,
    pub host: u64,
}

pub(crate) struct AccessGate {
    pub state: Mutex<AccessState>,
    pub requester_id: u32,
    interrupts: std::sync::Arc<dyn pci_core::vfio::VfioVm>,
    service: std::sync::OnceLock<std::sync::Weak<dyn tdisp::host::EvidenceService>>,
}

#[derive(Debug)]
pub(crate) struct AccessState {
    pub deny_all: bool,
    pub stopped: bool,
    pub irq_error: Option<std::sync::Arc<dyn std::error::Error + Send + Sync>>,
    pub io_error: Option<std::sync::Arc<dyn std::error::Error + Send + Sync>>,
    pub protected_blocked: bool,
    pub frontend_live: bool,
    pub private_ready: bool,
    pub bars: Vec<BarRange>,
    pub nonsecure: Vec<Range<u64>>,
    /// Attempted mappings, including uncertain outcomes, not guest acceptance.
    pub protected: Vec<Range<u64>>,
    pub shared: BTreeMap<u64, SharedIoasMapping>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AccessError {
    #[error("Realm device access failed")]
    DeviceIo(#[source] std::sync::Arc<dyn std::error::Error + Send + Sync>),
    #[error("Realm service is already bound")]
    ServiceAlreadyBound,
    #[error("VFIO interrupt access is not acknowledged")]
    Interrupt(#[source] std::sync::Arc<dyn std::error::Error + Send + Sync>),
    #[error("device access is quarantined")]
    Quarantined,
    #[error("MMIO interval is not a fixed protected BAR interval")]
    InvalidMmio,
    #[error("protected device mappings remain; the kernel has no forced unmap API")]
    ProtectedMappingsRemain,
    #[error("checked TDISP UNLOCKED completion is required before kernel object destruction")]
    DeviceNotUnlocked,
    #[error("the PCI frontend has not released its accesses and IRQ routes")]
    FrontendLive,
    #[error("initial private RAM import is not acknowledged")]
    PrivateNotReady,
    #[error("invalid or overlapping shared IOAS interval")]
    InvalidShared,
}

impl AccessGate {
    pub fn with_interrupts(
        requester_id: u32,
        interrupts: std::sync::Arc<dyn pci_core::vfio::VfioVm>,
    ) -> Self {
        Self {
            requester_id,
            interrupts,
            service: std::sync::OnceLock::new(),
            state: Mutex::new(AccessState {
                deny_all: false,
                stopped: false,
                irq_error: None,
                io_error: None,
                protected_blocked: false,
                frontend_live: false,
                private_ready: false,
                bars: Vec::new(),
                nonsecure: Vec::new(),
                protected: Vec::new(),
                shared: BTreeMap::new(),
            }),
        }
    }

    pub fn bind_service(
        &self,
        service: std::sync::Weak<dyn tdisp::host::EvidenceService>,
    ) -> Result<(), AccessError> {
        self.service
            .set(service)
            .map_err(|_| AccessError::ServiceAlreadyBound)
    }

    pub fn close_admission(&self) {
        if let Some(service) = self.service.get().and_then(std::sync::Weak::upgrade) {
            service.close_admission();
        }
    }

    pub fn fail_io(&self, state: &mut AccessState, error: anyhow::Error) -> AccessError {
        let source: std::sync::Arc<dyn std::error::Error + Send + Sync> =
            error.into_boxed_dyn_error().into();
        let source = state.io_error.get_or_insert(source).clone();
        state.deny_all = true;
        self.close_admission();
        AccessError::DeviceIo(source)
    }

    pub fn check_interrupts(&self, state: &mut AccessState) -> Result<(), AccessError> {
        if let Err(error) = self.interrupt_status() {
            let error = state.record_irq_error(error.into());
            self.close_admission();
            return Err(error);
        }
        if let Some(error) = &state.irq_error {
            self.close_admission();
            return Err(AccessError::Interrupt(error.clone()));
        }
        Ok(())
    }

    pub fn interrupt_status(&self) -> Result<(), pci_core::vfio::VfioVmError> {
        self.interrupts.check_interrupt_routes()
    }

    #[cfg(test)]
    pub fn new(requester_id: u32) -> Self {
        struct CheckedTestVm;
        impl pci_core::vfio::VfioVm for CheckedTestVm {
            fn add_file(
                &self,
                _: std::os::fd::BorrowedFd<'_>,
            ) -> Result<(), pci_core::vfio::VfioVmError> {
                unreachable!("test access gate never associates files")
            }
            fn remove_file(
                &self,
                _: std::os::fd::BorrowedFd<'_>,
            ) -> Result<(), pci_core::vfio::VfioVmError> {
                unreachable!("test access gate never disassociates files")
            }
            fn check_interrupt_routes(&self) -> Result<(), pci_core::vfio::VfioVmError> {
                Ok(())
            }
        }
        Self::with_interrupts(requester_id, std::sync::Arc::new(CheckedTestVm))
    }
}

impl std::fmt::Debug for AccessGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessGate")
            .field("requester_id", &self.requester_id)
            .field("state", &self.state.lock())
            .finish_non_exhaustive()
    }
}

pub(crate) fn interval(base: u64, length: u64) -> Option<Range<u64>> {
    let end = base.checked_add(length)?;
    (length != 0 && base.is_multiple_of(GRANULE) && length.is_multiple_of(GRANULE))
        .then_some(base..end)
}

pub(crate) fn overlaps(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

impl AccessState {
    pub fn record_irq_error(&mut self, error: anyhow::Error) -> AccessError {
        let source: std::sync::Arc<dyn std::error::Error + Send + Sync> =
            error.into_boxed_dyn_error().into();
        self.irq_error = Some(source.clone());
        self.deny_all = true;
        AccessError::Interrupt(source)
    }

    pub fn quiescent_for_cleanup(&self) -> Result<(), AccessError> {
        if let Some(error) = &self.irq_error {
            return Err(AccessError::Interrupt(error.clone()));
        }
        if self.frontend_live {
            return Err(AccessError::FrontendLive);
        }
        if !self.protected.is_empty() {
            return Err(AccessError::ProtectedMappingsRemain);
        }
        if self.protected_blocked {
            return Err(AccessError::DeviceNotUnlocked);
        }
        Ok(())
    }

    pub fn invalidate_mmio(&mut self, range: Range<u64>) -> Result<(), AccessError> {
        if interval(range.start, range.end.saturating_sub(range.start)).is_none() {
            return Err(AccessError::InvalidMmio);
        }
        self.protected.sort_by_key(|r| r.start);
        let mut next = range.start;
        for mapped in &self.protected {
            if mapped.start <= next && next < mapped.end {
                next = mapped.end.min(range.end);
            }
        }
        if next != range.end {
            return Err(AccessError::InvalidMmio);
        }
        self.protected = self
            .protected
            .iter()
            .flat_map(|mapped| {
                if !overlaps(mapped, &range) {
                    return [Some(mapped.clone()), None];
                }
                [
                    (mapped.start < range.start).then_some(mapped.start..range.start),
                    (range.end < mapped.end).then_some(range.end..mapped.end),
                ]
            })
            .flatten()
            .collect();
        Ok(())
    }

    pub fn shared_unmap_keys(&self, iova: u64, length: u64) -> Result<Vec<u64>, AccessError> {
        let range = interval(iova, length).ok_or(AccessError::InvalidShared)?;
        let mut keys = Vec::new();
        let mut next = range.start;
        for (&base, mapping) in self.shared.range(range.clone()) {
            if base != next || mapping.length != GRANULE {
                return Err(AccessError::InvalidShared);
            }
            keys.push(base);
            next += GRANULE;
        }
        if next != range.end {
            return Err(AccessError::InvalidShared);
        }
        Ok(keys)
    }

    pub fn mmio_host(&self, range: &Range<u64>) -> Result<u64, AccessError> {
        if !self.protected_blocked
            || interval(range.start, range.end.saturating_sub(range.start)).is_none()
            || self.nonsecure.iter().any(|r| overlaps(r, range))
            || self.protected.iter().any(|r| overlaps(r, range))
        {
            return Err(AccessError::InvalidMmio);
        }
        self.bars
            .iter()
            .find(|bar| bar.guest.start <= range.start && range.end <= bar.guest.end)
            .and_then(|bar| bar.host.checked_add(range.start - bar.guest.start))
            .ok_or(AccessError::InvalidMmio)
    }

    pub fn permits_mmio(&self, address: u64, length: usize) -> bool {
        let Some(end) = address.checked_add(length as u64) else {
            return false;
        };
        !self.deny_all
            && !self.stopped
            && (!self.protected_blocked
                || self
                    .nonsecure
                    .iter()
                    .any(|r| r.start <= address && end <= r.end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use test_with_tracing::test;

    #[test]
    fn protected_ranges_reject_msix_pages_overflow_and_untracked_addresses() {
        let gate = AccessGate::new(8);
        let mut state = gate.state.lock();
        state.bars.push(BarRange {
            guest: 0x1000..0x5000,
            host: 0x8000,
        });
        state.nonsecure.push(0x3000..0x4000);
        assert!(state.mmio_host(&(0x1000..0x2000)).is_err());
        state.protected_blocked = true;
        assert_eq!(state.mmio_host(&(0x1000..0x2000)).unwrap(), 0x8000);
        assert!(state.mmio_host(&(0x2000..0x4000)).is_err());
        assert!(state.mmio_host(&(0x4000..0x6000)).is_err());
        assert!(state.mmio_host(&(0x1001..0x2001)).is_err());
        assert!(!state.permits_mmio(u64::MAX, 8));
        assert!(!state.permits_mmio(0x2000, 8));
        assert!(state.permits_mmio(0x3000, 8));
        state.deny_all = true;
        assert!(!state.permits_mmio(0x3000, 8));
    }

    #[test]
    fn completed_invalidation_splits_ranges_but_never_accepts_holes() {
        let gate = AccessGate::new(8);
        let mut state = gate.state.lock();
        state.protected = vec![0x1000..0x3000, 0x3000..0x6000];
        state.invalidate_mmio(0x2000..0x5000).unwrap();
        assert_eq!(state.protected, [0x1000..0x2000, 0x5000..0x6000]);
        assert!(state.invalidate_mmio(0x1000..0x6000).is_err());
        assert!(state.invalidate_mmio(0x1001..0x2001).is_err());
        state.invalidate_mmio(0x1000..0x2000).unwrap();
        state.invalidate_mmio(0x5000..0x6000).unwrap();
        assert!(state.protected.is_empty());
    }

    #[test]
    fn shared_unmap_requires_complete_granule_coverage() {
        let gate = AccessGate::new(8);
        let mut state = gate.state.lock();
        let file = Arc::new(std::fs::File::from(
            sparse_mmap::alloc_shared_memory(GRANULE as usize, "realm-shared-map-test").unwrap(),
        ));
        let memory = Arc::new(sparse_mmap::SparseMapping::new(GRANULE as usize).unwrap());
        memory
            .map_file(0, GRANULE as usize, file.as_ref(), 0, true)
            .unwrap();
        let weak_memory = Arc::downgrade(&memory);
        for iova in [0x1000, 0x2000, 0x4000] {
            state.shared.insert(
                iova,
                SharedIoasMapping {
                    mapping: SharedMapping {
                        file: file.clone(),
                        file_offset: 0,
                        iova,
                        length: GRANULE,
                    },
                    memory: memory.clone(),
                    offset: 0,
                },
            );
        }
        drop(memory);
        drop(file);
        assert_eq!(
            state.shared_unmap_keys(0x1000, 0x2000).unwrap(),
            [0x1000, 0x2000]
        );
        assert!(state.shared_unmap_keys(0x1000, 0x4000).is_err());
        assert!(state.shared_unmap_keys(0x1001, GRANULE).is_err());
        state.shared.remove(&0x1000);
        state.shared.remove(&0x2000);
        assert!(weak_memory.upgrade().is_some());
        state.shared.remove(&0x4000);
        assert!(weak_memory.upgrade().is_none());
    }

    #[test]
    fn teardown_cannot_depend_on_the_kernels_unchecked_implicit_unlock() {
        let gate = AccessGate::new(8);
        let mut state = gate.state.lock();
        state.quiescent_for_cleanup().unwrap();
        state.protected_blocked = true;
        assert!(matches!(
            state.quiescent_for_cleanup(),
            Err(AccessError::DeviceNotUnlocked)
        ));
        state.protected.push(0x1000..0x2000);
        assert!(matches!(
            state.quiescent_for_cleanup(),
            Err(AccessError::ProtectedMappingsRemain)
        ));
        state.frontend_live = true;
        assert!(matches!(
            state.quiescent_for_cleanup(),
            Err(AccessError::FrontendLive)
        ));
    }
}
