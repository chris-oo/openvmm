// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fail-closed helpers for the explicitly selected guest_memfd in-place mode.

use memory_range::MemoryRange;
use sparse_mmap::SparseMapping;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CcaInPlaceError {
    #[error("guest_memfd in-place requires 4096-byte host pages (host page size is {0})")]
    UnsupportedHostPageSize(usize),
    #[error("invalid guest_memfd in-place range or population progress")]
    InvalidRange,
    #[error("overlapping guest_memfd in-place initial imports")]
    OverlappingImports,
    #[error("failed to allocate CCA population source")]
    Allocate(#[source] std::io::Error),
    #[error("failed to copy CCA population source")]
    Read(#[from] guestmem::GuestMemoryError),
    #[error("failed to write CCA population source")]
    Write(#[from] sparse_mmap::SparseMappingError),
    #[error("ambiguous or repeated guest_memfd in-place memory fault")]
    AmbiguousFault,
    #[error("guest buffer includes private CCA backing")]
    PrivateBuffer,
    #[error("invalid, overlapping, or unknown protected device mapping")]
    InvalidProtectedMapping,
}

/// Conservative history of attempted protected mappings. Neither ioctl zero,
/// a following exit, nor DEV -> EMPTY proves removal in the pinned kernel.
/// Never remove these entries without a new, checked kernel interface.
#[derive(Debug, Default)]
pub(crate) struct ProtectedAttempts {
    ranges: Vec<MemoryRange>,
}

impl ProtectedAttempts {
    pub(crate) fn record(
        &mut self,
        range: MemoryRange,
        pa: u64,
        shared_bit: u64,
    ) -> Result<(), CcaInPlaceError> {
        checked_range(range.start(), range.len())?;
        checked_range(pa, range.len())?;
        if shared_bit < 4096
            || !shared_bit.is_power_of_two()
            || range.end() > shared_bit
            || self.ranges.iter().any(|existing| existing.overlaps(&range))
        {
            return Err(CcaInPlaceError::InvalidProtectedMapping);
        }
        self.ranges.push(range);
        self.ranges.sort_by_key(MemoryRange::start);
        Ok(())
    }

    pub(crate) fn covers(&self, range: MemoryRange) -> bool {
        checked_range(range.start(), range.len()).is_ok()
            && memory_range::walk_ranges([(range, ())], self.ranges.iter().map(|r| (*r, ())))
                .all(|(_, state)| !matches!(state, memory_range::RangeWalkResult::Left(())))
    }
}
pub(crate) fn validate_host_page_size(page_size: usize) -> Result<(), CcaInPlaceError> {
    if page_size != 4096 {
        return Err(CcaInPlaceError::UnsupportedHostPageSize(page_size));
    }
    Ok(())
}

pub(crate) fn checked_range(gpa: u64, size: u64) -> Result<MemoryRange, CcaInPlaceError> {
    let end = gpa.checked_add(size).ok_or(CcaInPlaceError::InvalidRange)?;
    if size == 0 || !gpa.is_multiple_of(4096) || !size.is_multiple_of(4096) {
        return Err(CcaInPlaceError::InvalidRange);
    }
    Ok(MemoryRange::new(gpa..end))
}

pub(crate) fn validate_imports(
    ranges: impl IntoIterator<Item = MemoryRange>,
) -> Result<(), CcaInPlaceError> {
    let mut ranges: Vec<_> = ranges.into_iter().collect();
    for range in &ranges {
        checked_range(range.start(), range.len())?;
    }
    ranges.sort_by_key(|r| r.start());
    if ranges.windows(2).any(|r| r[0].overlaps(&r[1])) {
        return Err(CcaInPlaceError::OverlappingImports);
    }
    Ok(())
}

/// Owns an aligned source separate from guestmemfd. No references to guest
/// memory survive the copy; conversion may revoke every destination alias.
pub(crate) fn copy_source(
    gm: &guestmem::GuestMemory,
    range: MemoryRange,
) -> Result<SparseMapping, CcaInPlaceError> {
    validate_host_page_size(SparseMapping::page_size())?;
    checked_range(range.start(), range.len())?;
    let len = usize::try_from(range.len()).map_err(|_| CcaInPlaceError::InvalidRange)?;
    let source = SparseMapping::new(len).map_err(CcaInPlaceError::Allocate)?;
    source.alloc(0, len).map_err(CcaInPlaceError::Allocate)?;
    let mut buffer = [0; 4096];
    for offset in (0..len).step_by(buffer.len()) {
        gm.read_at(range.start() + offset as u64, &mut buffer)?;
        source.write_at(offset, &buffer)?;
    }
    Ok(source)
}

pub(crate) fn validate_progress(
    previous: (u64, u64, u64),
    next: (u64, u64, u64),
) -> Result<(), CcaInPlaceError> {
    let (base, size, source) = previous;
    let (next_base, next_size, next_source) = next;
    let done = size
        .checked_sub(next_size)
        .ok_or(CcaInPlaceError::InvalidRange)?;
    if done == 0
        || !done.is_multiple_of(4096)
        || base.checked_add(done) != Some(next_base)
        || source.checked_add(done) != Some(next_source)
    {
        return Err(CcaInPlaceError::InvalidRange);
    }
    Ok(())
}

pub(crate) trait LaunchOps {
    fn convert(&mut self, range: MemoryRange) -> Result<(), crate::KvmError>;
    fn populate(
        &mut self,
        range: (u64, u64, u64),
        measured: bool,
    ) -> Result<(u64, u64, u64), crate::KvmError>;
    fn init_ripas(&mut self, range: MemoryRange) -> Result<(), crate::KvmError>;
}

/// Runs the launch sequence only after the caller validates slot coverage.
/// Every source is copied before the first conversion; INIT_RIPAS runs last,
/// once per RAM slot, and no operation is retried after an error.
pub(crate) fn launch(
    gm: &guestmem::GuestMemory,
    pages: &[virt::InitialPageImport],
    ram_slots: &[MemoryRange],
    mut ops: impl LaunchOps,
) -> Result<(), crate::KvmError> {
    validate_imports(pages.iter().map(|page| page.range))?;
    validate_imports(ram_slots.iter().copied())?;
    let sources = pages
        .iter()
        .map(|page| {
            let measured = match page.import_type {
                virt::InitialPageImportType::Normal => true,
                virt::InitialPageImportType::NormalUnmeasured => false,
                _ => {
                    return Err(crate::KvmError::UnsupportedCcaPageImportType(
                        page.import_type,
                    ));
                }
            };
            Ok((page.range, measured, copy_source(gm, page.range)?))
        })
        .collect::<Result<Vec<_>, crate::KvmError>>()?;
    for (range, measured, source) in sources {
        ops.convert(range)?;
        let mut progress = (range.start(), range.len(), source.as_ptr() as u64);
        while progress.1 != 0 {
            let next = ops.populate(progress, measured)?;
            validate_progress(progress, next)?;
            progress = next;
        }
    }
    for &slot in ram_slots {
        ops.init_ripas(slot)?;
    }
    Ok(())
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct Fault {
    pub(crate) gpa: u64,
    pub(crate) size: u64,
    pub(crate) flags: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FaultAction {
    /// Re-enter only to identify a RIPAS completion. Do not change backing.
    ProbeCompletion,
    Convert,
    /// Backing already matches. KVM may finish RIPAS and resume the guest.
    NoChange,
}

/// Actual guestmemfd attributes, protected by the partition memory lock.
/// INIT_RIPAS establishes all-private RAM. Only successful ATTRIBUTES2 calls
/// change this ledger. A partial ioctl failure poisons the partition instead.
#[derive(Debug, Default)]
pub(crate) struct Visibility {
    ranges: Vec<(MemoryRange, bool)>,
}

impl Visibility {
    pub(crate) fn all_private(mut slots: Vec<MemoryRange>) -> Result<Self, CcaInPlaceError> {
        validate_imports(slots.iter().copied())?;
        slots.sort_by_key(MemoryRange::start);
        Ok(Self {
            ranges: memory_range::merge_adjacent_ranges(slots.into_iter().map(|r| (r, true)))
                .collect(),
        })
    }

    /// Validate full RAM coverage, including holes between slots, before
    /// returning only the subranges whose attributes need to change.
    pub(crate) fn changes(
        &self,
        range: MemoryRange,
        private: bool,
    ) -> Result<Vec<MemoryRange>, CcaInPlaceError> {
        checked_range(range.start(), range.len())?;
        let mut changes = Vec::new();
        for (part, state) in memory_range::walk_ranges([(range, ())], self.ranges.iter().copied()) {
            match state {
                memory_range::RangeWalkResult::Left(()) => {
                    return Err(CcaInPlaceError::InvalidRange);
                }
                memory_range::RangeWalkResult::Both((), current) if current != private => {
                    changes.push(part);
                }
                _ => {}
            }
        }
        Ok(changes)
    }

    pub(crate) fn record(&mut self, range: MemoryRange, private: bool) {
        self.ranges = memory_range::merge_adjacent_ranges(
            memory_range::walk_ranges(self.ranges.iter().copied(), [(range, private)]).filter_map(
                |(part, state)| {
                    let value = match state {
                        memory_range::RangeWalkResult::Left(current) => current,
                        memory_range::RangeWalkResult::Both(_, new) => new,
                        _ => return None,
                    };
                    Some((part, value))
                },
            ),
        )
        .collect();
    }

    pub(crate) fn require_shared(&self, range: MemoryRange) -> Result<(), CcaInPlaceError> {
        checked_range(range.start(), range.len())?;
        for (_, state) in memory_range::walk_ranges([(range, ())], self.ranges.iter().copied()) {
            match state {
                memory_range::RangeWalkResult::Left(()) => {
                    return Err(CcaInPlaceError::InvalidRange);
                }
                memory_range::RangeWalkResult::Both((), true) => {
                    return Err(CcaInPlaceError::PrivateBuffer);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// The pinned kernel's arch/arm64/kvm/rmi-exit.c rec_exit_ripas_change returns
/// EFAULT for each fresh request, even if its attributes already match.
/// rmi.c kvm_complete_ripas_change returns a successful memory fault only when
/// attributes differ; otherwise it can resume the guest within the same
/// KVM_RUN. There is no completion marker or requirement for an intervening
/// ordinary userspace exit.
///
/// mmu.c gmem_abort also returns EFAULT with PRIVATE clear for backing failures.
/// Probe only shared requests that still need conversion: a matching success
/// confirms RIPAS, whereas another EFAULT must not trigger conversion. For
/// already-shared requests, allow one no-op without arming a completion probe.
/// An identical shared EFAULT without intervening progress is indistinguishable
/// from a persistent backing failure (including poison), so fail closed.
///
/// PRIVATE EFAULT needs no repeat guard: gmem_abort sets PRIVATE only for an
/// attribute mismatch, never for a backing failure. If the ledger now matches,
/// this is either a fresh satisfied RIPAS request or an access fault resolved
/// by another VP's conversion. This relies on exclusive VMM attribute updates
/// under the memory lock, including all-private INIT_RIPAS before first entry.
#[derive(Debug, Default)]
pub(crate) struct FaultTracker {
    probe: Option<Fault>,
    unconfirmed_noop: Option<Fault>,
    device_probe: Option<Fault>,
}

impl FaultTracker {
    /// Permit one kernel completion attempt for a tracked non-RAM interval,
    /// without treating it as a visibility change or acknowledged unmapping.
    pub(crate) fn observe_device(
        &mut self,
        fault: Fault,
        successful_exit: bool,
    ) -> Result<(), CcaInPlaceError> {
        checked_range(fault.gpa, fault.size)?;
        if successful_exit || fault.flags != 0 || self.device_probe == Some(fault) {
            return Err(CcaInPlaceError::AmbiguousFault);
        }
        self.device_probe = Some(fault);
        Ok(())
    }

    /// An ordinary exit proves that KVM finished the previous completion.
    /// An interrupt does not: immediate_exit can run before REC pre-entry.
    pub(crate) fn completed(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn observe(
        &mut self,
        fault: Fault,
        successful_exit: bool,
        visibility: &Visibility,
    ) -> Result<FaultAction, CcaInPlaceError> {
        let range = checked_range(fault.gpa, fault.size)?;
        if fault.flags & !kvm::KVM_MEMORY_EXIT_FLAG_PRIVATE_UAPI != 0 {
            return Err(CcaInPlaceError::AmbiguousFault);
        }
        let private = fault.flags != 0;
        let needs_change = !visibility.changes(range, private)?.is_empty();
        if let Some(probe) = self.probe {
            // Another VP may have satisfied the pending request. In that
            // case KVM can already have resumed this VP, too.
            let still_pending = !visibility
                .changes(checked_range(probe.gpa, probe.size)?, false)?
                .is_empty();
            if still_pending && (!successful_exit || probe != fault) {
                return Err(CcaInPlaceError::AmbiguousFault);
            }
            self.probe = None;
        }
        if !successful_exit && !private && needs_change {
            self.unconfirmed_noop = None;
            self.probe = Some(fault);
            return Ok(FaultAction::ProbeCompletion);
        }
        let guard_noop = !needs_change && (!private || successful_exit);
        if guard_noop && self.unconfirmed_noop == Some(fault) {
            return Err(CcaInPlaceError::AmbiguousFault);
        }
        self.unconfirmed_noop = if guard_noop { Some(fault) } else { None };
        if needs_change {
            Ok(FaultAction::Convert)
        } else {
            // A successful exit can race another VP's conversion. Allow one
            // no-op, but do not infer that this VP entered the REC.
            Ok(FaultAction::NoChange)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn protected_attempts_cover_only_known_ranges_and_never_acknowledge_removal() {
        let mut ledger = ProtectedAttempts::default();
        let range = MemoryRange::new(0x4000..0x8000);
        ledger.record(range, 0x9000, 1 << 40).unwrap();
        ledger
            .record(MemoryRange::new(0x8000..0x9000), 0x11000, 1 << 40)
            .unwrap();
        assert!(ledger.covers(range));
        assert!(ledger.covers(MemoryRange::new(0x5000..0x9000)));
        assert!(!ledger.covers(MemoryRange::new(0x3000..0x9000)));
        assert!(!ledger.covers(MemoryRange::new(0x4000..0xa000)));
        assert!(ledger.record(range, 0x9000, 1 << 40).is_err());
        let mut tracker = FaultTracker::default();
        let fault = Fault {
            gpa: range.start(),
            size: range.len(),
            flags: 0,
        };
        tracker.observe_device(fault, false).unwrap();
        assert!(tracker.observe_device(fault, false).is_err());
        tracker.completed();
        assert!(ledger.covers(range));
        assert!(tracker.observe_device(fault, true).is_err());
        assert!(
            tracker
                .observe_device(
                    Fault {
                        flags: kvm::KVM_MEMORY_EXIT_FLAG_PRIVATE_UAPI,
                        ..fault
                    },
                    false
                )
                .is_err()
        );
    }

    #[test]
    fn requires_4k_host_pages() {
        validate_host_page_size(4096).unwrap();
        for size in [0, 16384, 65536] {
            assert!(matches!(
                validate_host_page_size(size),
                Err(CcaInPlaceError::UnsupportedHostPageSize(actual)) if actual == size
            ));
        }
    }

    #[test]
    fn copies_all_source_data_before_conversion() {
        let gm = guestmem::GuestMemory::allocate(0x3000);
        gm.fill_at(0x1000, 0x35, 0x2000).unwrap();
        let source = copy_source(&gm, MemoryRange::new(0x1000..0x3000)).unwrap();
        assert_eq!(source.as_ptr() as usize % 4096, 0);
        gm.fill_at(0x1000, 0xee, 0x2000).unwrap();
        let mut data = [0; 0x2000];
        source.read_at(0, &mut data).unwrap();
        assert_eq!(data, [0x35; 0x2000]);
    }

    #[test]
    fn validates_ranges_imports_and_population_progress() {
        for (gpa, size) in [(0, 0), (1, 4096), (0, 4095), (u64::MAX - 4095, 4096)] {
            assert!(checked_range(gpa, size).is_err());
        }
        assert!(
            validate_imports([MemoryRange::new(0..8192), MemoryRange::new(4096..8192)]).is_err()
        );
        validate_imports([MemoryRange::new(0..4096), MemoryRange::new(8192..12288)]).unwrap();
        validate_progress((4096, 8192, 4096), (8192, 4096, 8192)).unwrap();
        validate_progress((8192, 4096, 8192), (12288, 0, 12288)).unwrap();
        for next in [(4096, 8192, 4096), (8192, 12288, 8192), (8192, 4096, 4096)] {
            assert!(validate_progress((4096, 8192, 4096), next).is_err());
        }
        assert!(validate_progress((u64::MAX - 4095, 4096, 0), (0, 0, 4096)).is_err());
    }

    #[test]
    fn shared_efault_requires_matching_successful_completion() {
        let fault = Fault {
            gpa: 4096,
            size: 4096,
            flags: 0,
        };
        let mut visibility = Visibility::all_private(vec![MemoryRange::new(0..0x4000)]).unwrap();
        let mut tracker = FaultTracker::default();
        assert_eq!(
            tracker.observe(fault, false, &visibility).unwrap(),
            FaultAction::ProbeCompletion
        );
        assert_eq!(
            tracker.observe(fault, true, &visibility).unwrap(),
            FaultAction::Convert
        );
        visibility.record(MemoryRange::new(4096..8192), false);
        // A success racing a different VP's conversion needs no ioctl.
        assert_eq!(
            tracker.observe(fault, true, &visibility).unwrap(),
            FaultAction::NoChange
        );
        assert!(tracker.observe(fault, true, &visibility).is_err());
        visibility.record(MemoryRange::new(4096..8192), true);
        let mut tracker = FaultTracker::default();
        tracker.observe(fault, false, &visibility).unwrap();
        assert!(tracker.observe(fault, false, &visibility).is_err());
        let mut tracker = FaultTracker::default();
        tracker.observe(fault, false, &visibility).unwrap();
        assert!(
            tracker
                .observe(Fault { gpa: 8192, ..fault }, true, &visibility)
                .is_err()
        );
    }

    #[test]
    fn consecutive_identical_private_requests_need_no_ordinary_exit() {
        let fault = Fault {
            gpa: 4096,
            size: 4096,
            flags: kvm::KVM_MEMORY_EXIT_FLAG_PRIVATE_UAPI,
        };
        for success in [false, true] {
            let range = MemoryRange::new(4096..8192);
            let mut visibility = Visibility::all_private(vec![range]).unwrap();
            visibility.record(range, false);
            let mut tracker = FaultTracker::default();
            assert_eq!(
                tracker.observe(fault, success, &visibility).unwrap(),
                FaultAction::Convert
            );
            visibility.record(range, true);
            for _ in 0..3 {
                assert_eq!(
                    tracker.observe(fault, false, &visibility).unwrap(),
                    FaultAction::NoChange
                );
            }
            // Another VP can share the range before an identical request.
            visibility.record(range, false);
            assert_eq!(
                tracker.observe(fault, success, &visibility).unwrap(),
                FaultAction::Convert
            );
        }
    }

    #[test]
    fn already_shared_then_other_requests_without_ordinary_exits() {
        let fault = Fault {
            gpa: 4096,
            size: 4096,
            flags: 0,
        };
        let mut visibility = Visibility::all_private(vec![MemoryRange::new(0..0x4000)]).unwrap();
        visibility.record(MemoryRange::new(4096..8192), false);
        let mut tracker = FaultTracker::default();
        assert_eq!(
            tracker.observe(fault, false, &visibility).unwrap(),
            FaultAction::NoChange
        );
        assert_eq!(tracker.probe, None);
        let other = Fault { gpa: 8192, ..fault };
        assert_eq!(
            tracker.observe(other, false, &visibility).unwrap(),
            FaultAction::ProbeCompletion
        );
        assert_eq!(
            tracker.observe(other, true, &visibility).unwrap(),
            FaultAction::Convert
        );
        visibility.record(MemoryRange::new(8192..12288), false);
        let private = Fault {
            flags: kvm::KVM_MEMORY_EXIT_FLAG_PRIVATE_UAPI,
            ..fault
        };
        assert_eq!(
            tracker.observe(private, false, &visibility).unwrap(),
            FaultAction::Convert
        );
        visibility.record(MemoryRange::new(4096..8192), true);
        assert_eq!(
            tracker.observe(private, false, &visibility).unwrap(),
            FaultAction::NoChange
        );
        assert_eq!(
            tracker.observe(fault, false, &visibility).unwrap(),
            FaultAction::ProbeCompletion
        );
        assert_eq!(
            tracker.observe(fault, true, &visibility).unwrap(),
            FaultAction::Convert
        );
    }

    #[test]
    fn ambiguity_guards_survive_interrupt_stop_and_reentry() {
        let fault = Fault {
            gpa: 4096,
            size: 4096,
            flags: 0,
        };
        let range = MemoryRange::new(4096..8192);
        let mut visibility = Visibility::all_private(vec![range]).unwrap();
        let mut tracker = FaultTracker::default();
        tracker.observe(fault, false, &visibility).unwrap();
        // Interrupted (including immediate_exit on stop) must not call
        // completed. Retain the VP-owned tracker when the run future exits.
        let mut rebound = tracker;
        assert_eq!(rebound.probe, Some(fault));
        assert_eq!(
            rebound.observe(fault, true, &visibility).unwrap(),
            FaultAction::Convert
        );
        visibility.record(range, false);
        // After conversion, stop need not drive KVM_RUN to a completion exit.
        assert_eq!(rebound.probe, None);
        assert_eq!(
            rebound.observe(fault, false, &visibility).unwrap(),
            FaultAction::NoChange
        );
        assert_eq!(rebound.probe, None);
        assert!(rebound.observe(fault, false, &visibility).is_err());
        rebound.completed();
        assert_eq!(
            rebound.observe(fault, false, &visibility).unwrap(),
            FaultAction::NoChange
        );
    }

    #[test]
    fn another_vp_can_complete_probe_but_cannot_authorize_new_shared_efault() {
        let fault = Fault {
            gpa: 0,
            size: 4096,
            flags: 0,
        };
        let mut visibility = Visibility::all_private(vec![MemoryRange::new(0..8192)]).unwrap();
        let mut tracker = FaultTracker::default();
        tracker.observe(fault, false, &visibility).unwrap();
        visibility.record(MemoryRange::new(0..4096), false);
        let other = Fault { gpa: 4096, ..fault };
        assert_eq!(
            tracker.observe(other, false, &visibility).unwrap(),
            FaultAction::ProbeCompletion
        );
        assert!(tracker.observe(other, false, &visibility).is_err());
    }

    #[test]
    fn mixed_visibility_changes_only_differing_subranges_across_slots() {
        let full = MemoryRange::new(0..0x6000);
        let mut visibility = Visibility::all_private(vec![
            MemoryRange::new(0x3000..0x6000),
            MemoryRange::new(0..0x3000),
        ])
        .unwrap();
        assert!(visibility.changes(full, true).unwrap().is_empty());
        visibility.record(MemoryRange::new(0x1000..0x4000), false);
        assert_eq!(
            visibility.changes(full, false).unwrap(),
            [
                MemoryRange::new(0..0x1000),
                MemoryRange::new(0x4000..0x6000)
            ]
        );
        let request = MemoryRange::new(0x2000..0x5000);
        assert_eq!(
            visibility.changes(request, true).unwrap(),
            [MemoryRange::new(0x2000..0x4000)]
        );
        visibility.record(MemoryRange::new(0x2000..0x4000), true);
        assert_eq!(
            visibility.changes(full, true).unwrap(),
            [MemoryRange::new(0x1000..0x2000)]
        );
        let mut tracker = FaultTracker::default();
        let fault = Fault {
            gpa: 0,
            size: full.len(),
            flags: 0,
        };
        assert_eq!(
            tracker.observe(fault, false, &visibility).unwrap(),
            FaultAction::ProbeCompletion
        );
        assert_eq!(
            tracker.observe(fault, true, &visibility).unwrap(),
            FaultAction::Convert
        );
    }

    #[test]
    fn ledger_rejects_holes_unknown_ram_and_flags_even_for_noops() {
        let visibility = Visibility::all_private(vec![
            MemoryRange::new(0..4096),
            MemoryRange::new(8192..12288),
        ])
        .unwrap();
        assert!(
            visibility
                .changes(MemoryRange::new(0..12288), true)
                .is_err()
        );
        assert!(
            Visibility::default()
                .changes(MemoryRange::new(0..4096), true)
                .is_err()
        );
        let mut tracker = FaultTracker::default();
        for (gpa, size, flags) in [(0, 12288, 0), (1, 4096, 0), (0, 4096, 1)] {
            assert!(
                tracker
                    .observe(Fault { gpa, size, flags }, false, &visibility)
                    .is_err()
            );
        }
    }

    #[test]
    fn launch_orders_copies_conversion_population_and_per_slot_init() {
        struct Ops<'a> {
            gm: &'a guestmem::GuestMemory,
            events: &'a mut Vec<(char, u64, bool)>,
            fail_at: Option<char>,
        }
        impl LaunchOps for Ops<'_> {
            fn convert(&mut self, range: MemoryRange) -> Result<(), crate::KvmError> {
                self.events.push(('c', range.start(), false));
                // Revoke all loader sources, including later imports.
                self.gm.fill_at(0, 0xee, 0x6000).unwrap();
                if self.fail_at == Some('c') {
                    return Err(crate::KvmError::NotSupported);
                }
                Ok(())
            }
            fn populate(
                &mut self,
                (base, size, source): (u64, u64, u64),
                measured: bool,
            ) -> Result<(u64, u64, u64), crate::KvmError> {
                self.events.push(('p', base, measured));
                if self.fail_at == Some('p') {
                    return Err(crate::KvmError::NotSupported);
                }
                if self.fail_at == Some('n') {
                    return Ok((base, size, source));
                }
                // SAFETY: launch owns the aligned, allocated source throughout
                // this callback. Read one byte to verify its original contents.
                assert_eq!(unsafe { *(source as *const u8) }, 0x35);
                assert!(size >= 4096);
                Ok((base + 4096, size - 4096, source + 4096))
            }
            fn init_ripas(&mut self, range: MemoryRange) -> Result<(), crate::KvmError> {
                self.events.push(('i', range.start(), false));
                if self.fail_at == Some('i') {
                    return Err(crate::KvmError::NotSupported);
                }
                Ok(())
            }
        }
        let gm = guestmem::GuestMemory::allocate(0x6000);
        let pages = [
            virt::InitialPageImport {
                range: MemoryRange::new(0x1000..0x3000),
                import_type: virt::InitialPageImportType::Normal,
                tag: "measured",
            },
            virt::InitialPageImport {
                range: MemoryRange::new(0x5000..0x6000),
                import_type: virt::InitialPageImportType::NormalUnmeasured,
                tag: "unmeasured",
            },
        ];
        let slots = [
            MemoryRange::new(0..0x3000),
            MemoryRange::new(0x4000..0x6000),
        ];
        for fail_at in [None, Some('c'), Some('p'), Some('n'), Some('i')] {
            gm.fill_at(0, 0x35, 0x6000).unwrap();
            let mut events = Vec::new();
            let result = launch(
                &gm,
                &pages,
                &slots,
                Ops {
                    gm: &gm,
                    events: &mut events,
                    fail_at,
                },
            );
            let expected = [
                ('c', 0x1000, false),
                ('p', 0x1000, true),
                ('p', 0x2000, true),
                ('c', 0x5000, false),
                ('p', 0x5000, false),
                ('i', 0, false),
                ('i', 0x4000, false),
            ];
            if let Some(fail_at) = fail_at {
                assert!(result.is_err());
                let count = match fail_at {
                    'c' => 1,
                    'p' | 'n' => 2,
                    'i' => 6,
                    _ => unreachable!(),
                };
                assert_eq!(events, expected[..count]);
            } else {
                result.unwrap();
                assert_eq!(events, expected);
            }
        }
        let mut invalid = pages;
        invalid[1].import_type = virt::InitialPageImportType::Shared;
        let mut events = Vec::new();
        assert!(
            launch(
                &gm,
                &invalid,
                &slots,
                Ops {
                    gm: &gm,
                    events: &mut events,
                    fail_at: None
                }
            )
            .is_err()
        );
        assert!(events.is_empty());
    }
}
