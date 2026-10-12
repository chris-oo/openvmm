// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Admission and lifetime tracking for temporary SNP host-access grants.

use parking_lot::Condvar;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use thiserror::Error;

const PAGE_SIZE: u64 = hvdef::HV_PAGE_SIZE;
const MAX_ROOT_RESERVATIONS: usize = 4096;

#[derive(Debug, Error)]
pub(super) enum HostAccessError {
    #[error("invalid or overflowing SNP host-access range")]
    InvalidRange,
    #[error("SNP host access requires mapped RAM with suitable permissions")]
    NotRam,
    #[error("SNP host-access range overlaps a control page")]
    ControlPage,
    #[error("SNP host-access coordinator is busy; retry the operation")]
    Busy,
    #[error("SNP host-access admission timed out")]
    AdmissionTimeout,
    #[error("SNP root-operation reservation limit reached")]
    ReservationLimit,
    #[error("failed to allocate an SNP root-operation reservation")]
    Allocation(#[source] std::collections::TryReserveError),
    #[error("SNP host-access state is uncertain; partition execution is stopped")]
    Poisoned,
    #[error("legacy host-access acquisition is disabled in scoped mode")]
    LegacyAcquisition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PageRange {
    pub start: u64,
    pub end: u64,
}

impl PageRange {
    pub fn new(addr: u64, size: u64) -> Result<Self, HostAccessError> {
        let end = addr
            .checked_add(size)
            .filter(|_| size != 0)
            .and_then(|end| end.checked_add(PAGE_SIZE - 1))
            .ok_or(HostAccessError::InvalidRange)?
            & !(PAGE_SIZE - 1);
        Ok(Self {
            start: addr & !(PAGE_SIZE - 1),
            end,
        })
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Acquiring,
    Granted,
    Releasing,
    Uncertain,
}

#[derive(Debug, Clone, Copy)]
struct Reservation {
    range: PageRange,
    /// The registered userspace backing must not move during a grant.
    backing: u64,
    phase: Phase,
}

#[derive(Debug)]
struct RootReservation {
    id: u64,
    ranges: Vec<PageRange>,
    operation: &'static str,
    phase: Phase,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CopyCounts {
    read_acquire: u64,
    read_release: u64,
    write_acquire: u64,
    write_release: u64,
}

#[derive(Default)]
struct Ledger {
    active: Option<Reservation>,
    root: Vec<RootReservation>,
    next_root_id: u64,
    counts: CopyCounts,
    poisoned: bool,
}

pub(super) trait HostAccessBackend: Send + Sync {
    fn modify(&self, range: PageRange, acquire: bool, write: bool) -> anyhow::Result<()>;
}

type Kick = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub(super) struct HostAccessCoordinator {
    ledger: Mutex<Ledger>,
    changed: Condvar,
    kick: Mutex<Option<Kick>>,
}

impl Drop for HostAccessCoordinator {
    fn drop(&mut self) {
        let ledger = self.ledger.get_mut();
        let counts = ledger.counts;
        tracing::info!(
            read_acquire = counts.read_acquire,
            read_release = counts.read_release,
            write_acquire = counts.write_acquire,
            write_release = counts.write_release,
            active = ledger.active.is_some(),
            poisoned = ledger.poisoned,
            root_reservations = ledger.root.len(),
            "final scoped SNP host-access counts"
        );
    }
}

impl HostAccessCoordinator {
    #[cfg(test)]
    pub fn ledger_for_test_is_empty(&self) -> bool {
        self.ledger.lock().active.is_none()
    }

    pub fn set_kick(&self, kick: Kick) {
        *self.kick.lock() = Some(kick);
    }

    pub fn check_run(&self) -> Result<(), HostAccessError> {
        if self.ledger.lock().poisoned {
            Err(HostAccessError::Poisoned)
        } else {
            Ok(())
        }
    }

    pub fn check_change(&self, ranges: &[PageRange]) -> Result<(), HostAccessError> {
        let ledger = self.ledger.lock();
        if ledger.poisoned {
            return Err(HostAccessError::Poisoned);
        }
        if ledger
            .active
            .is_some_and(|active| ranges.iter().any(|range| active.range.overlaps(*range)))
        {
            return Err(HostAccessError::Busy);
        }
        if ledger.root.iter().any(|root| {
            root.ranges
                .iter()
                .any(|control| ranges.iter().any(|range| range.overlaps(*control)))
        }) {
            return Err(HostAccessError::ControlPage);
        }
        Ok(())
    }

    /// Reserve before taking the memory mapping lock. Waiting releases the
    /// ledger lock; neither validation nor ioctls nor copies run under it.
    pub fn reserve(
        self: &Arc<Self>,
        range: PageRange,
        backing: u64,
        write: bool,
        backend: Arc<dyn HostAccessBackend>,
    ) -> Result<HostAccessGuard, HostAccessError> {
        self.reserve_with_timeout(range, backing, write, backend, Duration::from_secs(5))
    }

    fn reserve_with_timeout(
        self: &Arc<Self>,
        range: PageRange,
        backing: u64,
        write: bool,
        backend: Arc<dyn HostAccessBackend>,
        timeout: Duration,
    ) -> Result<HostAccessGuard, HostAccessError> {
        let start = Instant::now();
        let mut ledger = self.ledger.lock();
        loop {
            if ledger.poisoned {
                return Err(HostAccessError::Poisoned);
            }
            if ledger
                .root
                .iter()
                .any(|root| root.ranges.iter().any(|control| range.overlaps(*control)))
            {
                return Err(HostAccessError::ControlPage);
            }
            if ledger.active.is_none() {
                break;
            }
            let remaining = timeout
                .checked_sub(start.elapsed())
                .ok_or(HostAccessError::AdmissionTimeout)?;
            self.changed.wait_for(&mut ledger, remaining);
        }
        ledger.active = Some(Reservation {
            range,
            backing,
            phase: Phase::Acquiring,
        });
        Ok(HostAccessGuard {
            coordinator: self.clone(),
            backend,
            range,
            write,
            live: true,
            acquired: false,
        })
    }

    pub fn reserve_root(
        self: &Arc<Self>,
        ranges: Vec<PageRange>,
        operation: &'static str,
    ) -> Result<RootAccessGuard, HostAccessError> {
        let start = Instant::now();
        let mut ledger = self.ledger.lock();
        loop {
            if ledger.poisoned {
                return Err(HostAccessError::Poisoned);
            }
            if ledger.root.len() >= MAX_ROOT_RESERVATIONS {
                return Err(HostAccessError::ReservationLimit);
            }
            if ledger.root.iter().any(|root| {
                root.ranges
                    .iter()
                    .any(|control| ranges.iter().any(|range| range.overlaps(*control)))
            }) {
                return Err(HostAccessError::ControlPage);
            }
            if !ledger
                .active
                .is_some_and(|active| ranges.iter().any(|range| range.overlaps(active.range)))
            {
                break;
            }
            let remaining = Duration::from_secs(5)
                .checked_sub(start.elapsed())
                .ok_or(HostAccessError::AdmissionTimeout)?;
            self.changed.wait_for(&mut ledger, remaining);
        }
        let id = ledger
            .next_root_id
            .checked_add(1)
            .ok_or(HostAccessError::InvalidRange)?;
        ledger
            .root
            .try_reserve(1)
            .map_err(HostAccessError::Allocation)?;
        ledger.next_root_id = id;
        ledger.root.push(RootReservation {
            id,
            ranges,
            operation,
            phase: Phase::Acquiring,
        });
        Ok(RootAccessGuard {
            coordinator: self.clone(),
            id,
            live: true,
            issued: false,
        })
    }

    fn record_copy(&self, write: bool, acquire: bool) {
        let counts = {
            let mut ledger = self.ledger.lock();
            let count = match (write, acquire) {
                (false, true) => &mut ledger.counts.read_acquire,
                (false, false) => &mut ledger.counts.read_release,
                (true, true) => &mut ledger.counts.write_acquire,
                (true, false) => &mut ledger.counts.write_release,
            };
            *count = count.saturating_add(1);
            ledger.counts
        };
        tracelimit::info_ratelimited!(
            write,
            acquire,
            read_acquire = counts.read_acquire,
            read_release = counts.read_release,
            write_acquire = counts.write_acquire,
            write_release = counts.write_release,
            "scoped SNP host-access proof counts"
        );
    }

    fn poison(&self) {
        let reservation = {
            let mut ledger = self.ledger.lock();
            ledger.poisoned = true;
            if let Some(active) = &mut ledger.active {
                active.phase = Phase::Uncertain;
            }
            ledger.active
        };
        self.changed.notify_all();
        if let Some(active) = reservation {
            tracelimit::error_ratelimited!(
                start = active.range.start,
                end = active.range.end,
                backing = active.backing,
                "retaining uncertain SNP host-access extent"
            );
        }
        let kick = self.kick.lock().clone();
        if let Some(kick) = kick {
            kick();
        }
    }
}

/// Root operations reserve pages but never request ordinary host-copy grants.
pub(crate) struct RootAccessGuard {
    coordinator: Arc<HostAccessCoordinator>,
    id: u64,
    live: bool,
    issued: bool,
}

impl RootAccessGuard {
    pub fn start(&mut self) {
        self.issued = true;
    }

    pub fn retain(mut self) {
        self.mark_granted();
        self.live = false;
    }

    pub fn mark_granted(&mut self) {
        if let Some(root) = self
            .coordinator
            .ledger
            .lock()
            .root
            .iter_mut()
            .find(|root| root.id == self.id)
        {
            root.phase = Phase::Granted;
        }
        // No local unregister contract exists for SNP AP VMSAs or the
        // SevDoorbellGpa binding. Keep them reserved until partition teardown.
    }

    pub fn complete(mut self) {
        self.cancel();
    }

    fn cancel(&mut self) {
        self.coordinator
            .ledger
            .lock()
            .root
            .retain(|root| root.id != self.id);
        self.live = false;
        self.coordinator.changed.notify_all();
    }
}

impl Drop for RootAccessGuard {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        if self.issued {
            {
                let mut ledger = self.coordinator.ledger.lock();
                if let Some(root) = ledger.root.iter_mut().find(|root| root.id == self.id) {
                    root.phase = Phase::Uncertain;
                    tracelimit::error_ratelimited!(
                        operation = root.operation,
                        "retaining uncertain SNP root-operation pages"
                    );
                }
            }
            self.coordinator.poison();
        } else {
            self.cancel();
        }
    }
}

pub(super) struct HostAccessGuard {
    coordinator: Arc<HostAccessCoordinator>,
    backend: Arc<dyn HostAccessBackend>,
    range: PageRange,
    write: bool,
    live: bool,
    acquired: bool,
}

impl HostAccessGuard {
    pub fn set_backing(&mut self, backing: u64) {
        if let Some(active) = &mut self.coordinator.ledger.lock().active {
            active.backing = backing;
        }
    }

    pub fn acquire(mut self) -> anyhow::Result<Self> {
        if let Err(error) = self.backend.modify(self.range, true, self.write) {
            // MSHV does not report a reliable completed prefix. Retain the
            // entire reservation, and do not attempt to reuse or release it.
            self.live = false;
            self.coordinator.poison();
            return Err(error.context("SNP host-access acquisition is uncertain"));
        }
        if let Some(active) = &mut self.coordinator.ledger.lock().active {
            active.phase = Phase::Granted;
        }
        self.acquired = true;
        self.coordinator.record_copy(self.write, true);
        Ok(self)
    }

    fn release(&mut self) -> anyhow::Result<()> {
        if !self.live {
            return Ok(());
        }
        self.live = false;
        if !self.acquired {
            // No ioctl was issued, so a dropped admission can be cancelled.
            self.coordinator.ledger.lock().active = None;
            self.coordinator.changed.notify_all();
            return Ok(());
        }
        if let Some(active) = &mut self.coordinator.ledger.lock().active {
            active.phase = Phase::Releasing;
        }
        if let Err(error) = self.backend.modify(self.range, false, false) {
            self.coordinator.poison();
            return Err(error.context("SNP host-access release is uncertain"));
        }
        self.coordinator.ledger.lock().active = None;
        self.coordinator.record_copy(self.write, false);
        self.coordinator.changed.notify_all();
        Ok(())
    }
}

impl virt::PartitionHostAccessGuard for HostAccessGuard {
    fn finish(mut self: Box<Self>) -> anyhow::Result<()> {
        self.release()
    }
}

impl Drop for HostAccessGuard {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            tracelimit::error_ratelimited!(
                error = &*error,
                "failed to release dropped SNP host-access guard"
            );
        }
    }
}

/// Validate the complete normalized extent, not just the caller's first byte.
pub(super) fn validate_ram(
    range: PageRange,
    write: bool,
    ram: &[PageRange],
    mappings: &[(PageRange, u64, bool)],
    controls: &[PageRange],
) -> Result<u64, HostAccessError> {
    if controls.iter().any(|control| range.overlaps(*control)) {
        return Err(HostAccessError::ControlPage);
    }
    let mut cursor = range.start;
    while cursor < range.end {
        let end = ram
            .iter()
            .filter(|ram| ram.start <= cursor && cursor < ram.end)
            .map(|ram| ram.end)
            .max()
            .ok_or(HostAccessError::NotRam)?;
        cursor = end.min(range.end);
    }
    // A scope stays within one registered backing. This also excludes holes
    // and gives the ledger a stable RAM identity.
    mappings
        .iter()
        .find_map(|(mapping, backing, writable)| {
            (mapping.start <= range.start && range.end <= mapping.end && (!write || *writable))
                .then_some(*backing)
        })
        .ok_or(HostAccessError::NotRam)
}

pub(super) fn attribute_ranges(
    message: &hvdef::HvX64GpaAttributeInterceptMessage,
) -> Result<Vec<PageRange>, HostAccessError> {
    let count = message.flags.range_count() as usize;
    let ranges = message
        .ranges
        .get(..count)
        .filter(|ranges| !ranges.is_empty())
        .ok_or(HostAccessError::InvalidRange)?;
    ranges
        .iter()
        .map(|range| {
            let extended = range.as_extended();
            let (page, address_shift, size_shift) = if extended.large_page() {
                let large = range.as_extended_large_page();
                // Bits 13..20 are reserved for both large-page sizes.
                if range.0 & (0xff << 13) != 0 {
                    return Err(HostAccessError::InvalidRange);
                }
                (
                    large.gpa_large_page_number(),
                    21,
                    if large.page_size() { 30 } else { 21 },
                )
            } else {
                (extended.gpa_page_number(), 12, 12)
            };
            let granularity = 1u64 << size_shift;
            let start = page
                .checked_mul(1u64 << address_shift)
                .ok_or(HostAccessError::InvalidRange)?;
            if !start.is_multiple_of(granularity) {
                return Err(HostAccessError::InvalidRange);
            }
            let size = (extended.additional_pages() + 1)
                .checked_mul(granularity)
                .ok_or(HostAccessError::InvalidRange)?;
            PageRange::new(start, size)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    #[test]
    fn root_reservations_are_bounded_without_poisoning() {
        let coordinator = Arc::new(HostAccessCoordinator::default());
        let mut guards: Vec<_> = (0..MAX_ROOT_RESERVATIONS)
            .map(|index| {
                coordinator
                    .reserve_root(
                        vec![PageRange::new(index as u64 * PAGE_SIZE, PAGE_SIZE).unwrap()],
                        "test control",
                    )
                    .unwrap()
            })
            .collect();
        let next = PageRange::new(MAX_ROOT_RESERVATIONS as u64 * PAGE_SIZE, PAGE_SIZE).unwrap();
        assert!(matches!(
            coordinator.reserve_root(vec![next], "test control"),
            Err(HostAccessError::ReservationLimit)
        ));
        coordinator.check_run().unwrap();
        guards.pop().unwrap().complete();
        let guard = coordinator
            .reserve_root(vec![next], "test control")
            .unwrap();
        guard.complete();
        drop(guards);
        assert!(coordinator.ledger.lock().root.is_empty());
    }
    use virt::PartitionHostAccessGuard;
    use zerocopy::FromZeros;

    #[derive(Default)]
    struct FakeHostAccessBackend {
        events: Mutex<Vec<&'static str>>,
        requests: Mutex<Vec<(PageRange, bool, bool)>>,
        fail_acquire: AtomicBool,
        fail_release: AtomicBool,
        callback: Mutex<Option<Arc<dyn Fn(bool) + Send + Sync>>>,
    }

    impl HostAccessBackend for FakeHostAccessBackend {
        fn modify(&self, range: PageRange, acquire: bool, write: bool) -> anyhow::Result<()> {
            self.events
                .lock()
                .push(if acquire { "acquire" } else { "release" });
            self.requests.lock().push((range, acquire, write));
            let callback = self.callback.lock().clone();
            if let Some(callback) = callback {
                callback(acquire);
            }
            if (acquire && self.fail_acquire.load(Ordering::Relaxed))
                || (!acquire && self.fail_release.load(Ordering::Relaxed))
            {
                anyhow::bail!("injected host-access failure");
            }
            Ok(())
        }
    }

    fn page(addr: u64) -> PageRange {
        PageRange::new(addr, PAGE_SIZE).expect("test range")
    }

    fn acquire(
        coordinator: &Arc<HostAccessCoordinator>,
        backend: &Arc<FakeHostAccessBackend>,
        range: PageRange,
        write: bool,
    ) -> anyhow::Result<HostAccessGuard> {
        coordinator
            .reserve(range, 0x100000, write, backend.clone())?
            .acquire()
    }

    #[test]
    fn balances_acquire_copy_release_and_permissions() {
        for write in [false, true] {
            let coordinator = Arc::new(HostAccessCoordinator::default());
            let backend = Arc::new(FakeHostAccessBackend::default());
            let range = PageRange::new(4095, 2).expect("test range");
            let guard = acquire(&coordinator, &backend, range, write).expect("acquire");
            assert_eq!(
                coordinator.ledger.lock().active.expect("reservation").phase,
                Phase::Granted
            );
            backend.events.lock().push("copy");
            Box::new(guard).finish().expect("release");
            assert_eq!(*backend.events.lock(), ["acquire", "copy", "release"]);
            assert_eq!(
                *backend.requests.lock(),
                [(range, true, write), (range, false, false)]
            );
            assert!(coordinator.ledger.lock().active.is_none());
            coordinator.check_run().expect("unpoisoned");
        }
    }

    #[test]
    fn validates_normalized_ram_permissions_holes_and_controls() {
        assert_eq!(
            PageRange::new(4095, 2).expect("range"),
            PageRange {
                start: 0,
                end: 8192
            }
        );
        for (addr, size) in [(0, 0), (u64::MAX, 1), (u64::MAX - 1, 1), (1, u64::MAX)] {
            assert!(matches!(
                PageRange::new(addr, size),
                Err(HostAccessError::InvalidRange)
            ));
        }
        let range = PageRange::new(4095, 2).expect("range");
        let ram = [range];
        let mappings = [(range, 42, false)];
        assert_eq!(
            validate_ram(range, false, &ram, &mappings, &[]).expect("readable"),
            42
        );
        assert!(matches!(
            validate_ram(range, true, &ram, &mappings, &[]),
            Err(HostAccessError::NotRam)
        ));
        assert!(matches!(
            validate_ram(range, false, &[page(0)], &mappings, &[]),
            Err(HostAccessError::NotRam)
        ));
        assert!(matches!(
            validate_ram(range, false, &ram, &[(page(0), 42, true)], &[]),
            Err(HostAccessError::NotRam)
        ));
        assert!(matches!(
            validate_ram(range, false, &ram, &mappings, &[page(4096)]),
            Err(HostAccessError::ControlPage)
        ));
        assert!(validate_ram(range, false, &[page(0), page(4096)], &mappings, &[]).is_ok());
        assert!(validate_ram(range, false, &[page(0), page(8192)], &mappings, &[]).is_err());
    }

    #[test]
    fn checks_whole_attribute_request_across_vps_and_release() {
        let coordinator = Arc::new(HostAccessCoordinator::default());
        let other_vp = coordinator.clone();
        let backend = Arc::new(FakeHostAccessBackend::default());
        let weak = Arc::downgrade(&coordinator);
        *backend.callback.lock() = Some(Arc::new(move |acquire| {
            let coordinator = weak.upgrade().expect("coordinator");
            // Both ACQUIRING and RELEASING deny a concurrent VP change.
            assert!(matches!(
                coordinator.check_change(&[page(4096)]),
                Err(HostAccessError::Busy)
            ));
            let state = coordinator.ledger.try_lock().expect("no ioctl lock");
            assert_eq!(
                state.active.expect("reservation").phase,
                if acquire {
                    Phase::Acquiring
                } else {
                    Phase::Releasing
                }
            );
        }));
        let guard = acquire(&coordinator, &backend, page(4096), false).expect("acquire");
        assert!(other_vp.check_change(&[page(0), page(8192)]).is_ok());
        assert!(matches!(
            other_vp.check_change(&[page(0), page(4096)]),
            Err(HostAccessError::Busy)
        ));
        assert!(matches!(
            coordinator.reserve_with_timeout(
                page(8192),
                99,
                false,
                backend.clone(),
                Duration::ZERO
            ),
            Err(HostAccessError::AdmissionTimeout)
        ));
        assert!(coordinator.ledger.try_lock().is_some(), "no copy lock");
        Box::new(guard).finish().expect("release");
        assert!(other_vp.check_change(&[page(0), page(4096)]).is_ok());
    }

    fn install_kick(coordinator: &Arc<HostAccessCoordinator>) -> Arc<AtomicUsize> {
        let kicks = Arc::new(AtomicUsize::new(0));
        let count = kicks.clone();
        let weak = Arc::downgrade(coordinator);
        coordinator.set_kick(Arc::new(move || {
            let coordinator = weak.upgrade().expect("coordinator");
            assert!(coordinator.ledger.try_lock().is_some(), "no kick lock");
            assert!(matches!(
                coordinator.check_run(),
                Err(HostAccessError::Poisoned)
            ));
            count.fetch_add(1, Ordering::Relaxed);
        }));
        kicks
    }

    #[test]
    fn acquire_failure_retains_entire_extent_and_stops_admission() {
        let coordinator = Arc::new(HostAccessCoordinator::default());
        let kicks = install_kick(&coordinator);
        let backend = Arc::new(FakeHostAccessBackend::default());
        backend.fail_acquire.store(true, Ordering::Relaxed);
        let range = PageRange::new(1, PAGE_SIZE * 2).expect("range");
        assert!(acquire(&coordinator, &backend, range, true).is_err());
        assert_eq!(*backend.events.lock(), ["acquire"]);
        let reservation = coordinator.ledger.lock().active.expect("retained");
        assert_eq!(reservation.range, range);
        assert_eq!(reservation.phase, Phase::Uncertain);
        assert!(matches!(
            coordinator.check_run(),
            Err(HostAccessError::Poisoned)
        ));
        assert!(matches!(
            coordinator.check_change(&[page(65536)]),
            Err(HostAccessError::Poisoned)
        ));
        assert!(matches!(
            coordinator.reserve(page(65536), 12, false, backend),
            Err(HostAccessError::Poisoned)
        ));
        assert_eq!(kicks.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn release_failure_and_drop_failure_retain_uncertain_grants() {
        for explicit_finish in [false, true] {
            let coordinator = Arc::new(HostAccessCoordinator::default());
            let kicks = install_kick(&coordinator);
            let backend = Arc::new(FakeHostAccessBackend::default());
            let guard = acquire(&coordinator, &backend, page(0), false).expect("acquire");
            backend.fail_release.store(true, Ordering::Relaxed);
            if explicit_finish {
                assert!(Box::new(guard).finish().is_err());
            } else {
                drop(guard);
            }
            assert_eq!(*backend.events.lock(), ["acquire", "release"]);
            assert_eq!(
                coordinator.ledger.lock().active.expect("retained").phase,
                Phase::Uncertain
            );
            assert!(matches!(
                coordinator.check_run(),
                Err(HostAccessError::Poisoned)
            ));
            assert_eq!(kicks.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn drop_releases_and_unused_admission_cancels() {
        let coordinator = Arc::new(HostAccessCoordinator::default());
        let backend = Arc::new(FakeHostAccessBackend::default());
        drop(acquire(&coordinator, &backend, page(0), false).expect("acquire"));
        assert_eq!(*backend.events.lock(), ["acquire", "release"]);
        drop(
            coordinator
                .reserve(page(0), 42, false, backend.clone())
                .expect("reserve"),
        );
        assert_eq!(*backend.events.lock(), ["acquire", "release"]);
        assert!(coordinator.ledger.lock().active.is_none());
        coordinator.check_run().expect("healthy");
    }

    #[test]
    fn concurrent_healthy_copies_wait_without_holding_mapping_or_ledger_locks() {
        let coordinator = Arc::new(HostAccessCoordinator::default());
        let backend = Arc::new(FakeHostAccessBackend::default());
        let mappings = Arc::new(Mutex::new(()));
        let weak = Arc::downgrade(&coordinator);
        let callback_mappings = mappings.clone();
        *backend.callback.lock() = Some(Arc::new(move |_| {
            let coordinator = weak.upgrade().expect("coordinator");
            assert!(
                coordinator
                    .ledger
                    .try_lock_for(Duration::from_secs(1))
                    .is_some(),
                "ioctl has no ledger lock"
            );
            assert!(
                callback_mappings.try_lock().is_some(),
                "waiter does not hold mapping lock"
            );
        }));
        let first = acquire(&coordinator, &backend, page(0), false).expect("first");
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut threads = Vec::new();
        for worker in 0..3 {
            let coordinator = coordinator.clone();
            let backend = backend.clone();
            let mappings = mappings.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for iteration in 0..12 {
                    let write = (worker + iteration) % 2 == 0;
                    let mut guard = coordinator
                        .reserve(page(0), 0, write, backend.clone())
                        .expect("healthy admission waits, not Busy");
                    {
                        let _validation = mappings.lock();
                        guard.set_backing(0x100000);
                    }
                    let guard = guard.acquire().expect("acquire");
                    assert!(
                        coordinator
                            .ledger
                            .try_lock_for(Duration::from_secs(1))
                            .is_some(),
                        "copy has no ledger lock"
                    );
                    Box::new(guard).finish().expect("release");
                }
            }));
        }
        barrier.wait();
        std::thread::sleep(Duration::from_millis(20));
        Box::new(first)
            .finish()
            .expect("first release wakes waiters");
        for thread in threads {
            thread.join().expect("worker");
        }
        assert_eq!(
            coordinator.ledger.lock().counts,
            CopyCounts {
                read_acquire: 19,
                read_release: 19,
                write_acquire: 18,
                write_release: 18,
            }
        );
        assert!(coordinator.ledger.lock().active.is_none());
    }

    #[test]
    fn root_operations_are_not_copy_grants_and_controls_stay_reserved() {
        let coordinator = Arc::new(HostAccessCoordinator::default());
        let backend = Arc::new(FakeHostAccessBackend::default());
        let mut psp = coordinator
            .reserve_root(vec![page(0), page(4096)], "PSP")
            .expect("PSP admission");
        psp.start();
        assert!(matches!(
            coordinator.reserve(page(0), 0, true, backend.clone()),
            Err(HostAccessError::ControlPage)
        ));
        assert!(matches!(
            coordinator.check_change(&[page(8192), page(4096)]),
            Err(HostAccessError::ControlPage)
        ));
        drop(acquire(&coordinator, &backend, page(8192), false).expect("disjoint copy"));
        psp.complete();
        drop(acquire(&coordinator, &backend, page(0), false).expect("PSP pages released"));
        assert_eq!(
            *backend.events.lock(),
            ["acquire", "release", "acquire", "release"]
        );

        let mut ap = coordinator
            .reserve_root(vec![page(4096)], "AP VMSA")
            .expect("AP admission");
        ap.start();
        ap.retain();
        assert!(matches!(
            coordinator.reserve(page(4096), 0, true, backend.clone()),
            Err(HostAccessError::ControlPage)
        ));
        assert!(matches!(
            coordinator.check_change(&[page(4096)]),
            Err(HostAccessError::ControlPage)
        ));
        assert_eq!(coordinator.ledger.lock().root[0].phase, Phase::Granted);
        coordinator
            .check_run()
            .expect("control binding does not poison");
    }

    #[test]
    fn poison_wakes_waiters_and_retains_uncertain_root_operation() {
        let coordinator = Arc::new(HostAccessCoordinator::default());
        let kicks = install_kick(&coordinator);
        let backend = Arc::new(FakeHostAccessBackend::default());
        let first = acquire(&coordinator, &backend, page(0), false).expect("first");
        let mut root = coordinator
            .reserve_root(vec![page(8192), page(12288)], "PSP")
            .expect("root");
        root.start();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let waiter_coordinator = coordinator.clone();
        let waiter_backend = backend.clone();
        let waiter = std::thread::spawn(move || {
            ready_tx.send(()).expect("ready");
            let result = waiter_coordinator.reserve(page(4096), 0, false, waiter_backend);
            result_tx
                .send(matches!(result, Err(HostAccessError::Poisoned)))
                .expect("result");
        });
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiting");
        std::thread::sleep(Duration::from_millis(20));
        drop(root);
        assert!(
            result_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("poison wakes waiter")
        );
        waiter.join().expect("waiter");
        assert_eq!(kicks.load(Ordering::Relaxed), 1);
        let ledger = coordinator.ledger.lock();
        assert_eq!(ledger.root[0].phase, Phase::Uncertain);
        assert_eq!(ledger.root[0].ranges, [page(8192), page(12288)]);
        drop(ledger);
        drop(first);
        assert!(matches!(
            coordinator.check_change(&[page(8192)]),
            Err(HostAccessError::Poisoned)
        ));
    }

    #[test]
    fn expands_attribute_ranges_with_checked_granularity() {
        use hvdef::hypercall::HvGpaRange;
        use hvdef::hypercall::HvGpaRangeExtended;
        use hvdef::hypercall::HvGpaRangeExtendedLargePage;

        let mut message = hvdef::HvX64GpaAttributeInterceptMessage::new_zeroed();
        for count in [0, 30, 31] {
            message.flags.set_range_count(count);
            assert!(matches!(
                attribute_ranges(&message),
                Err(HostAccessError::InvalidRange)
            ));
        }
        message.flags.set_range_count(3);
        message.ranges[0] = HvGpaRange(
            HvGpaRangeExtended::new()
                .with_gpa_page_number(1)
                .with_additional_pages(1)
                .into_bits(),
        );
        message.ranges[1] = HvGpaRange(
            HvGpaRangeExtendedLargePage::new()
                .with_large_page(true)
                .with_gpa_large_page_number(2)
                .with_additional_pages(1)
                .into_bits(),
        );
        message.ranges[2] = HvGpaRange(
            HvGpaRangeExtendedLargePage::new()
                .with_large_page(true)
                .with_page_size(true)
                .with_gpa_large_page_number(512)
                .into_bits(),
        );
        assert_eq!(
            attribute_ranges(&message).expect("ranges"),
            [
                PageRange {
                    start: 4096,
                    end: 12288
                },
                PageRange {
                    start: 4 << 20,
                    end: 8 << 20
                },
                PageRange {
                    start: 1 << 30,
                    end: 2 << 30
                },
            ]
        );
        message.flags.set_range_count(1);
        message.ranges[0] = HvGpaRange(
            HvGpaRangeExtended::new()
                .with_gpa_page_number((1 << 52) - 1)
                .into_bits(),
        );
        assert!(attribute_ranges(&message).is_err());
        message.ranges[0] = HvGpaRange(
            HvGpaRangeExtendedLargePage::new()
                .with_large_page(true)
                .with_page_size(true)
                .with_gpa_large_page_number(1)
                .into_bits(),
        );
        assert!(attribute_ranges(&message).is_err());
        message.ranges[0] = HvGpaRange((1 << 11) | (1 << 13));
        assert!(attribute_ranges(&message).is_err());
        message.flags.set_range_count(29);
        message.ranges.fill(HvGpaRange(0));
        assert_eq!(attribute_ranges(&message).expect("all ranges").len(), 29);
    }
}
