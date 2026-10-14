// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SNP control-page references, independent of ordinary host permissions.

use parking_lot::Mutex;
use std::sync::Arc;
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
    #[error("SNP root-operation reservation limit reached")]
    ReservationLimit,
    #[error("failed to allocate an SNP root-operation reservation")]
    Allocation(#[source] std::collections::TryReserveError),
    #[error("SNP control binding cannot be replaced: {0}")]
    ControlBinding(&'static str),
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

#[derive(Debug)]
struct RootReservation {
    id: u64,
    ranges: Vec<PageRange>,
    operation: &'static str,
    binding: Option<ControlBinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlBinding {
    Ap(virt::VpIndex),
    Doorbell(virt::VpIndex),
}

#[derive(Default)]
struct ControlReferences {
    root: Vec<RootReservation>,
    next_root_id: u64,
}

#[derive(Default)]
pub(super) struct ControlPageRegistry {
    references: Mutex<ControlReferences>,
}

impl ControlPageRegistry {
    fn check(references: &ControlReferences, ranges: &[PageRange]) -> Result<(), HostAccessError> {
        if references.root.iter().any(|root| {
            root.ranges
                .iter()
                .any(|control| ranges.iter().any(|range| range.overlaps(*control)))
        }) {
            return Err(HostAccessError::ControlPage);
        }
        Ok(())
    }

    pub fn check_change(&self, ranges: &[PageRange]) -> Result<(), HostAccessError> {
        Self::check(&self.references.lock(), ranges)
    }

    /// Serialize permission changes with control-reference admission, not copies.
    pub fn with_access<T>(
        &self,
        ranges: &[PageRange],
        modify: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let references = self.references.lock();
        Self::check(&references, ranges)?;
        modify()
    }

    pub fn reserve_root(
        self: &Arc<Self>,
        ranges: Vec<PageRange>,
        operation: &'static str,
        binding: Option<ControlBinding>,
    ) -> Result<RootAccessGuard, HostAccessError> {
        let mut references = self.references.lock();
        if binding.is_some() && references.root.iter().any(|root| root.binding == binding) {
            return Err(HostAccessError::ControlBinding(operation));
        }
        Self::check(&references, &ranges)?;
        if references.root.len() >= MAX_ROOT_RESERVATIONS {
            return Err(HostAccessError::ReservationLimit);
        }
        let id = references
            .next_root_id
            .checked_add(1)
            .ok_or(HostAccessError::InvalidRange)?;
        references
            .root
            .try_reserve(1)
            .map_err(HostAccessError::Allocation)?;
        references.next_root_id = id;
        references.root.push(RootReservation {
            id,
            ranges,
            operation,
            binding,
        });
        Ok(RootAccessGuard {
            registry: self.clone(),
            id,
            live: true,
            issued: false,
        })
    }
}

/// Root operations retain references, not ordinary host-copy grants.
pub(crate) struct RootAccessGuard {
    registry: Arc<ControlPageRegistry>,
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
        // No unregister contract exists for AP VMSAs or SevDoorbellGpa.
        self.issued = true;
        self.live = false;
    }

    pub fn complete(mut self) {
        self.registry
            .references
            .lock()
            .root
            .retain(|root| root.id != self.id);
        self.live = false;
    }
}

impl Drop for RootAccessGuard {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let mut references = self.registry.references.lock();
        if self.issued {
            if let Some(root) = references.root.iter().find(|root| root.id == self.id) {
                tracelimit::error_ratelimited!(
                    operation = root.operation,
                    "retaining uncertain SNP root-operation pages"
                );
            }
        } else {
            references.root.retain(|root| root.id != self.id);
        }
    }
}

/// Validate the complete normalized extent, not just its first byte.
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
    if message.flags.into_bits() >> 14 != 0 {
        return Err(HostAccessError::InvalidRange);
    }
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

pub(super) fn revoke_attribute_access(
    message: &hvdef::HvX64GpaAttributeInterceptMessage,
    ram: &[PageRange],
    mappings: &[(PageRange, u64, bool)],
    controls: &[PageRange],
    registry: &ControlPageRegistry,
    mut release: impl FnMut(PageRange) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<PageRange>> {
    let ranges = attribute_ranges(message)?;
    for range in &ranges {
        validate_ram(*range, false, ram, mappings, controls)?;
    }
    registry.with_access(&ranges, || {
        for (index, range) in ranges.iter().enumerate() {
            release(*range).map_err(|error| {
                error.context(format!(
                    "GPA attribute host release failed after {index} completed ranges"
                ))
            })?;
        }
        Ok(())
    })?;
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    fn page(addr: u64) -> PageRange {
        PageRange::new(addr, PAGE_SIZE).unwrap()
    }

    #[test]
    fn control_references_survive_uncertain_operations_without_poisoning_copies() {
        let registry = Arc::new(ControlPageRegistry::default());
        let mut control = registry
            .reserve_root(
                vec![page(0)],
                "AP VMSA",
                Some(ControlBinding::Ap(virt::VpIndex::new(1))),
            )
            .unwrap();
        control.start();
        drop(control);
        assert!(
            registry
                .reserve_root(
                    vec![page(16384)],
                    "AP VMSA",
                    Some(ControlBinding::Ap(virt::VpIndex::new(1)))
                )
                .is_err()
        );
        assert!(matches!(
            registry.check_change(&[page(0)]),
            Err(HostAccessError::ControlPage)
        ));
        registry
            .with_access::<()>(&[page(4096)], || anyhow::bail!("denied grant"))
            .unwrap_err();
        registry.with_access(&[page(4096)], || Ok(())).unwrap();
        let control = registry
            .reserve_root(vec![page(8192)], "PSP", None)
            .unwrap();
        control.complete();
        registry.check_change(&[page(8192)]).unwrap();
        let control = registry
            .reserve_root(
                vec![page(12288)],
                "doorbell",
                Some(ControlBinding::Doorbell(virt::VpIndex::new(1))),
            )
            .unwrap();
        control.retain();
        assert!(registry.check_change(&[page(12288)]).is_err());
        assert!(
            registry
                .reserve_root(
                    vec![page(16384)],
                    "doorbell",
                    Some(ControlBinding::Doorbell(virt::VpIndex::new(1)))
                )
                .is_err()
        );
        let mut ghcb = registry
            .reserve_root(vec![page(16384)], "GHCB", None)
            .unwrap();
        ghcb.mark_granted();
        ghcb.complete();
        registry.check_change(&[page(16384)]).unwrap();
    }

    #[test]
    fn control_references_are_bounded_and_unused_admission_is_removed() {
        let registry = Arc::new(ControlPageRegistry::default());
        let mut guards: Vec<_> = (0..MAX_ROOT_RESERVATIONS)
            .map(|index| {
                registry
                    .reserve_root(vec![page(index as u64 * PAGE_SIZE)], "control", None)
                    .unwrap()
            })
            .collect();
        let next = page(MAX_ROOT_RESERVATIONS as u64 * PAGE_SIZE);
        assert!(matches!(
            registry.reserve_root(vec![next], "control", None),
            Err(HostAccessError::ReservationLimit)
        ));
        guards.pop().unwrap().complete();
        drop(registry.reserve_root(vec![next], "control", None).unwrap());
        drop(guards);
        assert!(registry.references.lock().root.is_empty());
    }

    #[test]
    fn validates_complete_ram_ranges_and_controls() {
        for (addr, size) in [(0, 0), (u64::MAX, 1), (u64::MAX - 1, 1), (1, u64::MAX)] {
            assert!(PageRange::new(addr, size).is_err());
        }
        let range = PageRange::new(4095, 2).unwrap();
        let ram = [page(0), page(4096)];
        let mappings = [(range, 42, false)];
        assert_eq!(
            validate_ram(range, false, &ram, &mappings, &[]).unwrap(),
            42
        );
        assert!(validate_ram(range, true, &ram, &mappings, &[]).is_err());
        assert!(validate_ram(range, false, &[page(0)], &mappings, &[]).is_err());
        assert!(validate_ram(range, false, &ram, &[(page(0), 42, true)], &[]).is_err());
        assert!(validate_ram(range, false, &ram, &mappings, &[page(4096)]).is_err());
    }

    #[test]
    fn expands_attribute_ranges_with_checked_granularity() {
        use hvdef::hypercall::HvGpaRange;
        use hvdef::hypercall::HvGpaRangeExtended;
        use hvdef::hypercall::HvGpaRangeExtendedLargePage;

        let mut message = hvdef::HvX64GpaAttributeInterceptMessage::new_zeroed();
        for count in [0, 30, 31] {
            message.flags.set_range_count(count);
            assert!(attribute_ranges(&message).is_err());
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
            attribute_ranges(&message).unwrap(),
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
        assert_eq!(attribute_ranges(&message).unwrap().len(), 29);
    }

    #[test]
    fn rejects_whole_attribute_batch_before_effects_and_reports_release_prefix() {
        use hvdef::hypercall::HvGpaRange;
        use hvdef::hypercall::HvGpaRangeExtended;

        let mut message = hvdef::HvX64GpaAttributeInterceptMessage::new_zeroed();
        message.flags.set_range_count(2);
        message.ranges[0] = HvGpaRange(0);
        message.ranges[1] = HvGpaRange(
            HvGpaRangeExtended::new()
                .with_gpa_page_number(1)
                .into_bits(),
        );
        let range = PageRange::new(0, 8192).unwrap();
        let registry = Arc::new(ControlPageRegistry::default());
        let mut released = Vec::new();
        for (ram, controls) in [(&[page(0)][..], &[][..]), (&[range][..], &[page(4096)][..])] {
            assert!(
                revoke_attribute_access(
                    &message,
                    ram,
                    &[(range, 0, true)],
                    controls,
                    &registry,
                    |range| {
                        released.push(range);
                        Ok(())
                    }
                )
                .is_err()
            );
            assert!(released.is_empty());
        }
        let control = registry
            .reserve_root(vec![page(4096)], "PSP", None)
            .unwrap();
        assert!(
            revoke_attribute_access(
                &message,
                &[range],
                &[(range, 0, true)],
                &[],
                &registry,
                |range| {
                    released.push(range);
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(released.is_empty());
        control.complete();
        let error = revoke_attribute_access(
            &message,
            &[range],
            &[(range, 0, true)],
            &[],
            &registry,
            |range| {
                if range.start != 0 {
                    anyhow::bail!("release denied");
                }
                released.push(range);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(released, [page(0)]);
        assert!(error.to_string().contains("after 1 completed ranges"));
        released.clear();
        assert_eq!(
            revoke_attribute_access(
                &message,
                &[range],
                &[(range, 0, true)],
                &[],
                &registry,
                |range| {
                    released.push(range);
                    Ok(())
                }
            )
            .unwrap(),
            [page(0), page(4096)]
        );
        assert_eq!(released, [page(0), page(4096)]);
    }

    #[test]
    fn malformed_whole_batches_have_no_effects() {
        use hvdef::hypercall::HvGpaRange;
        use hvdef::hypercall::HvGpaRangeExtended;

        let range = PageRange::new(0, 8192).unwrap();
        let registry = ControlPageRegistry::default();
        let mut message = hvdef::HvX64GpaAttributeInterceptMessage::new_zeroed();
        message.flags.set_range_count(1);
        let mut releases = 0;
        for count in [0, 30, 31] {
            message.flags.set_range_count(count);
            assert!(
                revoke_attribute_access(
                    &message,
                    &[range],
                    &[(range, 0, true)],
                    &[],
                    &registry,
                    |_| {
                        releases += 1;
                        Ok(())
                    }
                )
                .is_err()
            );
            assert_eq!(releases, 0);
        }
        message.flags.set_range_count(2);
        for malformed in [
            HvGpaRange(
                HvGpaRangeExtended::new()
                    .with_gpa_page_number((1 << 52) - 1)
                    .into_bits(),
            ),
            HvGpaRange((1 << 11) | (1 << 13)),
        ] {
            message.ranges[1] = malformed;
            assert!(
                revoke_attribute_access(
                    &message,
                    &[range],
                    &[(range, 0, true)],
                    &[],
                    &registry,
                    |_| {
                        releases += 1;
                        Ok(())
                    }
                )
                .is_err()
            );
            assert_eq!(releases, 0);
        }
        message.flags = hvdef::HvX64GpaAttributeInterceptMessageFlags::from_bits(1 | (1 << 14));
        assert!(
            revoke_attribute_access(
                &message,
                &[range],
                &[(range, 0, true)],
                &[],
                &registry,
                |_| {
                    releases += 1;
                    Ok(())
                }
            )
            .is_err()
        );
        assert_eq!(releases, 0);
    }

    #[test]
    fn valid_identical_attribute_requests_can_repeat() {
        let mut message = hvdef::HvX64GpaAttributeInterceptMessage::new_zeroed();
        message.flags.set_range_count(1);
        let range = page(0);
        let registry = ControlPageRegistry::default();
        let mut releases = 0;
        for _ in 0..16 {
            revoke_attribute_access(
                &message,
                &[range],
                &[(range, 0, true)],
                &[],
                &registry,
                |_| {
                    releases += 1;
                    Ok(())
                },
            )
            .unwrap();
        }
        assert_eq!(releases, 16);
    }
}
