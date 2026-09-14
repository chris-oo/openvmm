// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Linux contract for partition-supplied userspace RAM.

use memory_range::MemoryRange;
use std::fs::File;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;

/// A clone of a backend-owned file that supports writable, shared userspace
/// mappings of all RAM, including VTL2 RAM.
///
/// Ranges are packed in `MemoryLayout::ram` order, followed by VTL2 RAM, with
/// no file space reserved for GPA holes. The constructor derives the complete
/// offset map from the validated layout; callers cannot supply partial or
/// overlapping maps.
///
/// The provider must keep the file at least this size while mappings exist and
/// retain its own handle for the partition's lifetime. A private-only
/// guestmemfd does **not** satisfy this contract. The caller must not replace
/// this file with ordinary RAM or restored backing.
#[derive(Debug)]
pub struct MappableRamBacking {
    file: File,
    ranges: Vec<(MemoryRange, u64)>,
}

/// An invalid partition-supplied RAM backing.
#[derive(Debug, Error)]
pub enum RamBackingError {
    #[error("failed to query RAM backing file size")]
    FileSize(#[source] std::io::Error),
    #[error("RAM backing offsets overflow")]
    OffsetOverflow,
    #[error("RAM backing file is too small: need {required} bytes, have {actual}")]
    FileTooSmall { required: u64, actual: u64 },
    #[error("RAM layout differs from the prepared backing")]
    LayoutChanged,
    #[error("range {0:?} is not contained in one prepared RAM range")]
    InvalidRange(MemoryRange),
}

impl MappableRamBacking {
    /// Describes a mappable file using the layout's exact packed RAM offsets.
    ///
    /// This checks file size, not whether the file supports shared writable
    /// mmap. The provider must ensure that capability; mapping errors are
    /// reported when the memory builder imports the file.
    pub fn new(file: File, layout: &MemoryLayout) -> Result<Self, RamBackingError> {
        let mut size = 0u64;
        let mut ranges = Vec::new();
        for range in ram_ranges(layout) {
            ranges.push((range, size));
            size = size
                .checked_add(range.len())
                .ok_or(RamBackingError::OffsetOverflow)?;
        }
        let actual = file.metadata().map_err(RamBackingError::FileSize)?.len();
        if actual < size {
            return Err(RamBackingError::FileTooSmall {
                required: size,
                actual,
            });
        }
        Ok(Self { file, ranges })
    }

    /// Checks that a consumer is using the layout for which RAM was prepared.
    pub fn validate_layout(&self, layout: &MemoryLayout) -> Result<(), RamBackingError> {
        if !self
            .ranges
            .iter()
            .map(|&(range, _)| range)
            .eq(ram_ranges(layout))
        {
            return Err(RamBackingError::LayoutChanged);
        }
        Ok(())
    }

    /// The file to clone when importing a RAM slice into the memory builder.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Gets the file offset of a nonempty slice contained in one RAM range.
    ///
    /// A slice must not cross a GPA hole or a range boundary, even if the file
    /// offsets on the two sides are consecutive.
    pub fn file_offset(&self, slice: MemoryRange) -> Result<u64, RamBackingError> {
        if !slice.is_empty() {
            for &(range, offset) in &self.ranges {
                if range.contains(&slice) {
                    return Ok(offset + (slice.start() - range.start()));
                }
            }
        }
        Err(RamBackingError::InvalidRange(slice))
    }
}

fn ram_ranges(layout: &MemoryLayout) -> impl Iterator<Item = MemoryRange> + '_ {
    layout
        .ram()
        .iter()
        .map(|r| r.range)
        .chain(layout.vtl2_range())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use vm_topology::memory::MemoryRangeWithNode;

    fn layout() -> MemoryLayout {
        MemoryLayout::new_from_resolved_ranges(
            vec![
                MemoryRangeWithNode {
                    range: MemoryRange::new(0x1000..0x3000),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(0x5000..0x7000),
                    vnode: 1,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(0x7000..0x9000),
                    vnode: 0,
                },
            ],
            vec![],
            vec![],
            vec![],
            Some(MemoryRange::new(0x10000..0x12000)),
        )
        .unwrap()
    }

    fn file(size: usize) -> File {
        sparse_mmap::alloc_shared_memory(size, "partition-ram-test")
            .unwrap()
            .into()
    }

    #[test]
    fn packed_offsets_include_holes_numa_and_vtl2() {
        let layout = layout();
        let backing = MappableRamBacking::new(file(0x8000), &layout).unwrap();
        backing.validate_layout(&layout).unwrap();
        for (range, offset) in [
            (0x1000..0x3000, 0),
            (0x5000..0x7000, 0x2000),
            (0x8000..0x9000, 0x5000),
            (0x10000..0x12000, 0x6000),
        ] {
            assert_eq!(
                backing.file_offset(MemoryRange::new(range)).unwrap(),
                offset
            );
        }
    }

    #[test]
    fn rejects_holes_boundaries_and_empty_slices() {
        let backing = MappableRamBacking::new(file(0x8000), &layout()).unwrap();
        for range in [
            0..0x1000,
            0x3000..0x5000,
            0x2000..0x6000,
            0x6000..0x8000,
            0x11000..0x13000,
            0x1000..0x1000,
        ] {
            assert!(matches!(
                backing.file_offset(MemoryRange::new(range)),
                Err(RamBackingError::InvalidRange(_))
            ));
        }
    }

    #[test]
    fn rejects_short_file_and_changed_layout() {
        assert!(matches!(
            MappableRamBacking::new(file(0x7000), &layout()),
            Err(RamBackingError::FileTooSmall {
                required: 0x8000,
                actual: 0x7000
            })
        ));
        let backing = MappableRamBacking::new(file(0x8000), &layout()).unwrap();
        let changed = MemoryLayout::new_from_ranges(
            &[MemoryRangeWithNode {
                range: MemoryRange::new(0x1000..0x9000),
                vnode: 0,
            }],
            &[],
        )
        .unwrap();
        assert!(matches!(
            backing.validate_layout(&changed),
            Err(RamBackingError::LayoutChanged)
        ));
    }
}
