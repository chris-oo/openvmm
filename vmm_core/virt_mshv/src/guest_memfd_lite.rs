// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Experimental local MSHV RAM ABI. This is not KVM guest_memfd.

use memory_range::MemoryRange;
use mshv_ioctls::VmFd;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
#[cfg(guest_arch = "x86_64")]
use std::os::fd::FromRawFd;
use std::sync::Arc;
use thiserror::Error;

// Local kernel policy bit, consumed before the Hyper-V partition-create call.
#[cfg(guest_arch = "x86_64")]
pub(crate) const MSHV_PT_LITE_SNP: u64 = 1 << 8;

#[derive(Debug, Error)]
pub(crate) enum GuestMemfdError {
    #[cfg(guest_arch = "x86_64")]
    #[error("guest_memfd_lite requires nonempty, ordered, disjoint 4 KiB RAM ranges")]
    InvalidRanges,
    #[error("guest_memfd_lite RAM size overflows or exceeds the kernel's 1 GiB file limit")]
    InvalidSize,
    #[error("guest_memfd_lite mapping must be contained in exactly one RAM range")]
    PartialRam,
    #[error("guest_memfd_lite refuses non-RAM mappings (no legacy fallback)")]
    NonRam,
    #[error("guest_memfd_lite requires nonempty aligned mappings without address overflow")]
    InvalidMapping,
    #[error("guest_memfd_lite requires one stable primary VA/GPA mapping base")]
    WrongMappingBase,
    #[error("guest_memfd_lite mapping overlaps a tracked GPA or file-offset region")]
    MappingOverlap,
    #[error("guest_memfd_lite unmap partially overlaps a tracked region")]
    PartialUnmap,
    #[cfg(guest_arch = "x86_64")]
    #[error(
        "MSHV_CREATE_GUEST_MEMFD failed; guest_memfd_lite requires an opted-in supporting kernel (no legacy fallback)"
    )]
    Create(#[source] io::Error),
    #[cfg(guest_arch = "x86_64")]
    #[error("failed to duplicate guest_memfd_lite backing")]
    Duplicate(#[source] io::Error),
}

#[cfg(guest_arch = "x86_64")]
#[repr(C)]
struct CreateGuestMemfd {
    size: u64,
    flags: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct GuestMemfdRegion {
    pub size: u64,
    pub guest_pfn: u64,
    pub offset: u64,
    pub fd: i32,
    pub flags: u8,
    pub rsvd: [u8; 3],
}

impl GuestMemfdRegion {
    pub fn unmap(mut self) -> Self {
        self.flags |= 1 << mshv_bindings::MSHV_SET_MEM_BIT_UNMAP;
        self
    }
}

// The provisional, default-off UAPI uses _IOW with the published MSHV type.
// Derive it from the bindings, not a second magic.
fn ioctl_request(number: u32, size: usize) -> libc::c_ulong {
    let magic = (mshv_ioctls::MSHV_SET_GUEST_MEMORY() as u32 >> 8) & 0xff;
    ((1u32 << 30) | ((size as u32) << 16) | (magic << 8) | number) as _
}

#[derive(Debug)]
pub(crate) struct GuestMemfdRange {
    pub range: MemoryRange,
    pub offset: u64,
}

#[derive(Debug)]
pub(crate) struct GuestMemfdBacking {
    pub file: Arc<File>,
    pub ranges: Vec<GuestMemfdRange>,
}

pub(crate) struct MapRequest {
    pub userspace_addr: u64,
    pub size: u64,
    pub gpa: u64,
    pub writable: bool,
    pub executable: bool,
}

pub(crate) struct MapDecision {
    pub region: GuestMemfdRegion,
    pub primary_base: u64,
}

#[cfg(guest_arch = "x86_64")]
fn backing_ranges(ranges: &[MemoryRange]) -> Result<(Vec<GuestMemfdRange>, u64), GuestMemfdError> {
    if ranges.is_empty() {
        return Err(GuestMemfdError::InvalidRanges);
    }
    let mut offset = 0u64;
    let mut end = 0;
    let mut result = Vec::with_capacity(ranges.len());
    for &range in ranges {
        if range.is_empty()
            || !range.start().is_multiple_of(4096)
            || !range.len().is_multiple_of(4096)
            || range.start() < end
        {
            return Err(GuestMemfdError::InvalidRanges);
        }
        result.push(GuestMemfdRange { range, offset });
        offset = offset
            .checked_add(range.len())
            .ok_or(GuestMemfdError::InvalidSize)?;
        end = range.end();
    }
    if offset > 1024 * 1024 * 1024 {
        return Err(GuestMemfdError::InvalidSize);
    }
    Ok((result, offset))
}

impl GuestMemfdBacking {
    #[cfg(guest_arch = "x86_64")]
    pub fn create(vmfd: &VmFd, ranges: &[MemoryRange]) -> Result<Self, GuestMemfdError> {
        let (ranges, size) = backing_ranges(ranges)?;
        let args = CreateGuestMemfd { size, flags: 0 };
        let file = create_file(vmfd.as_raw_fd(), &args)?;
        Ok(Self {
            file: Arc::new(file),
            ranges,
        })
    }

    /// A RAM mapping cannot span holes, adjacent allocation ranges, or MMIO.
    /// This follows the KVM guest_memfd classifier's containment rule.
    pub fn decide_mapping(
        &self,
        request: MapRequest,
        primary_base: Option<u64>,
        tracked: &[GuestMemfdRegion],
    ) -> Result<MapDecision, GuestMemfdError> {
        decide_mapping(
            &self.ranges,
            self.file.as_raw_fd(),
            request,
            primary_base,
            tracked,
        )
    }
}

fn checked_range(gpa: u64, size: u64) -> Result<MemoryRange, GuestMemfdError> {
    if size == 0 || !gpa.is_multiple_of(4096) || !size.is_multiple_of(4096) {
        return Err(GuestMemfdError::InvalidMapping);
    }
    let end = gpa
        .checked_add(size)
        .ok_or(GuestMemfdError::InvalidMapping)?;
    Ok(MemoryRange::new(gpa..end))
}

fn region_range(region: &GuestMemfdRegion) -> Result<MemoryRange, GuestMemfdError> {
    checked_range(
        region
            .guest_pfn
            .checked_mul(4096)
            .ok_or(GuestMemfdError::InvalidMapping)?,
        region.size,
    )
}

fn decide_mapping(
    ranges: &[GuestMemfdRange],
    fd: i32,
    request: MapRequest,
    primary_base: Option<u64>,
    tracked: &[GuestMemfdRegion],
) -> Result<MapDecision, GuestMemfdError> {
    let range = checked_range(request.gpa, request.size)?;
    let offset = classify(ranges, range)?.ok_or(GuestMemfdError::NonRam)?;
    let base = request
        .userspace_addr
        .checked_sub(request.gpa)
        .ok_or(GuestMemfdError::WrongMappingBase)?;
    if base == 0
        || !base.is_multiple_of(4096)
        || primary_base.is_some_and(|expected| expected != base)
        || request.userspace_addr.checked_add(request.size).is_none()
    {
        return Err(GuestMemfdError::WrongMappingBase);
    }
    let file_range = checked_range(offset, request.size)?;
    for existing in tracked {
        if existing.fd != fd
            || region_range(existing)?.overlaps(&range)
            || checked_range(existing.offset, existing.size)?.overlaps(&file_range)
        {
            return Err(GuestMemfdError::MappingOverlap);
        }
    }
    let flags = u8::from(request.writable) << mshv_bindings::MSHV_SET_MEM_BIT_WRITABLE
        | u8::from(request.executable) << mshv_bindings::MSHV_SET_MEM_BIT_EXECUTABLE;
    Ok(MapDecision {
        region: GuestMemfdRegion {
            size: request.size,
            guest_pfn: request.gpa / 4096,
            offset,
            fd,
            flags,
            rsvd: [0; 3],
        },
        primary_base: base,
    })
}

pub(crate) fn decide_unmapping(
    tracked: &[GuestMemfdRegion],
    gpa: u64,
    size: u64,
) -> Result<Vec<GuestMemfdRegion>, GuestMemfdError> {
    let range = checked_range(gpa, size)?;
    let mut unmaps = Vec::new();
    for existing in tracked {
        let existing_range = region_range(existing)?;
        if range.contains(&existing_range) {
            unmaps.push(existing.unmap());
        } else if range.overlaps(&existing_range) {
            return Err(GuestMemfdError::PartialUnmap);
        }
    }
    Ok(unmaps)
}

fn classify(
    ranges: &[GuestMemfdRange],
    range: MemoryRange,
) -> Result<Option<u64>, GuestMemfdError> {
    let mut contained = ranges.iter().filter(|ram| ram.range.contains(&range));
    if let Some(ram) = contained.next() {
        if contained.next().is_some() {
            return Err(GuestMemfdError::PartialRam);
        }
        return ram
            .offset
            .checked_add(range.start() - ram.range.start())
            .map(Some)
            .ok_or(GuestMemfdError::InvalidSize);
    }
    if ranges.iter().any(|ram| ram.range.overlaps(&range)) {
        return Err(GuestMemfdError::PartialRam);
    }
    Ok(None)
}

#[cfg(guest_arch = "x86_64")]
fn create_file(vmfd: i32, args: &CreateGuestMemfd) -> Result<File, GuestMemfdError> {
    // SAFETY: args has the agreed C layout and lives through this synchronous
    // ioctl. On success the kernel returns a new owned CLOEXEC descriptor.
    let fd = unsafe {
        libc::ioctl(
            vmfd,
            ioctl_request(0x0b, size_of::<CreateGuestMemfd>()) as _,
            args,
        )
    };
    if fd < 0 {
        return Err(GuestMemfdError::Create(io::Error::last_os_error()));
    }
    // SAFETY: the successful CREATE ioctl transfers this fresh descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) fn set_region(vmfd: &VmFd, region: &GuestMemfdRegion) -> io::Result<()> {
    // SAFETY: the retained backing owns region.fd; region has the agreed C
    // layout, initialized reserved bytes, and lives through this ioctl.
    let result = unsafe {
        libc::ioctl(
            vmfd.as_raw_fd(),
            ioctl_request(0x0c, size_of::<GuestMemfdRegion>()) as _,
            region,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(test, guest_arch = "x86_64"))]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const BASE: u64 = 0x1000_0000;

    fn test_ranges() -> Vec<GuestMemfdRange> {
        backing_ranges(&[
            MemoryRange::new(0..0x2000),
            MemoryRange::new(0x4000..0x8000),
        ])
        .unwrap()
        .0
    }

    fn request(gpa: u64, size: u64) -> MapRequest {
        MapRequest {
            userspace_addr: BASE + gpa,
            gpa,
            size,
            writable: true,
            executable: true,
        }
    }

    #[test]
    fn map_decision_selects_fd_offsets_and_all_permission_combinations() {
        let ranges = test_ranges();
        let first = decide_mapping(&ranges, 42, request(0, 0x2000), None, &[]).unwrap();
        assert_eq!(first.primary_base, BASE);
        for writable in [false, true] {
            for executable in [false, true] {
                let mut req = request(0x5000, 0x2000);
                req.writable = writable;
                req.executable = executable;
                let next =
                    decide_mapping(&ranges, 42, req, Some(first.primary_base), &[first.region])
                        .unwrap();
                assert_eq!(
                    (
                        next.region.fd,
                        next.region.guest_pfn,
                        next.region.offset,
                        next.region.size
                    ),
                    (42, 5, 0x3000, 0x2000)
                );
                assert_eq!(
                    next.region.flags,
                    (u8::from(writable) << mshv_bindings::MSHV_SET_MEM_BIT_WRITABLE)
                        | (u8::from(executable) << mshv_bindings::MSHV_SET_MEM_BIT_EXECUTABLE)
                );
                assert_eq!(next.region.rsvd, [0; 3]);
            }
        }
    }

    #[test]
    fn map_decision_denies_non_ram_partial_ranges_and_address_overflow() {
        let ranges = test_ranges();
        assert!(matches!(
            decide_mapping(&ranges, 42, request(0x2000, 0x2000), None, &[]),
            Err(GuestMemfdError::NonRam)
        ));
        assert!(matches!(
            decide_mapping(&ranges, 42, request(0x1000, 0x4000), None, &[]),
            Err(GuestMemfdError::PartialRam)
        ));
        assert!(matches!(
            decide_mapping(&ranges, 42, request(1, 4096), None, &[]),
            Err(GuestMemfdError::InvalidMapping)
        ));
        let req = MapRequest {
            gpa: u64::MAX - 4095,
            size: 4096,
            userspace_addr: BASE,
            writable: false,
            executable: false,
        };
        assert!(matches!(
            decide_mapping(&ranges, 42, req, None, &[]),
            Err(GuestMemfdError::InvalidMapping)
        ));
    }

    #[test]
    fn map_decision_denies_other_virtual_addresses_and_overlapping_identities() {
        let ranges = test_ranges();
        let first = decide_mapping(&ranges, 42, request(0, 0x2000), None, &[]).unwrap();
        for userspace_addr in [BASE + 0x6000, BASE + 0x5001, 0x1000] {
            let mut req = request(0x5000, 4096);
            req.userspace_addr = userspace_addr;
            assert!(matches!(
                decide_mapping(&ranges, 42, req, Some(BASE), &[first.region]),
                Err(GuestMemfdError::WrongMappingBase)
            ));
        }
        assert!(matches!(
            decide_mapping(
                &ranges,
                42,
                request(0x1000, 4096),
                Some(BASE),
                &[first.region]
            ),
            Err(GuestMemfdError::MappingOverlap)
        ));
        let alias_ranges = [
            GuestMemfdRange {
                range: MemoryRange::new(0..0x2000),
                offset: 0,
            },
            GuestMemfdRange {
                range: MemoryRange::new(0x4000..0x8000),
                offset: 0,
            },
        ];
        assert!(matches!(
            decide_mapping(
                &alias_ranges,
                42,
                request(0x4000, 4096),
                Some(BASE),
                &[first.region]
            ),
            Err(GuestMemfdError::MappingOverlap)
        ));
    }

    #[test]
    fn unmap_decision_uses_stored_fd_and_identity_and_rejects_partial_unmaps() {
        let ranges = test_ranges();
        let first = decide_mapping(&ranges, 42, request(0, 0x2000), None, &[])
            .unwrap()
            .region;
        let second = decide_mapping(&ranges, 42, request(0x5000, 0x2000), Some(BASE), &[first])
            .unwrap()
            .region;
        let tracked = [first, second];
        assert_eq!(
            decide_unmapping(&tracked, 0x5000, 0x2000).unwrap(),
            [second.unmap()]
        );
        assert_eq!(
            decide_unmapping(&tracked, 0, 0x8000).unwrap(),
            [first.unmap(), second.unmap()]
        );
        assert!(matches!(
            decide_unmapping(&tracked, 0x6000, 4096),
            Err(GuestMemfdError::PartialUnmap)
        ));
        assert!(matches!(
            decide_unmapping(&tracked, 0x5001, 4096),
            Err(GuestMemfdError::InvalidMapping)
        ));
        assert!(
            decide_unmapping(&tracked, 0x2000, 0x2000)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn provisional_abi_layout_and_numbers() {
        assert_eq!(size_of::<CreateGuestMemfd>(), 16);
        assert_eq!(size_of::<GuestMemfdRegion>(), 32);
        assert_eq!(std::mem::offset_of!(GuestMemfdRegion, fd), 24);
        assert_eq!(std::mem::offset_of!(GuestMemfdRegion, flags), 28);
        assert_eq!(ioctl_request(0x0b, 16), 0x4010b80b);
        assert_eq!(ioctl_request(0x0c, 32), 0x4020b80c);
    }

    #[test]
    fn packs_ranges_and_classifies_ram_and_holes() {
        let (ranges, size) = backing_ranges(&[
            MemoryRange::new(0..0x2000),
            MemoryRange::new(0x4000..0x8000),
        ])
        .unwrap();
        assert_eq!(size, 0x6000);
        assert_eq!(
            classify(&ranges, MemoryRange::new(0x5000..0x7000)).unwrap(),
            Some(0x3000)
        );
        assert_eq!(
            classify(&ranges, MemoryRange::new(0x2000..0x4000)).unwrap(),
            None
        );
        assert!(classify(&ranges, MemoryRange::new(0x1000..0x5000)).is_err());
        assert!(classify(&ranges, MemoryRange::new(0x7000..0x9000)).is_err());
    }

    #[test]
    fn rejects_bad_ram_requests() {
        for ranges in [
            vec![],
            vec![MemoryRange::EMPTY],
            vec![MemoryRange::new(0..8192), MemoryRange::new(4096..12288)],
            vec![MemoryRange::new(0..2 * 1024 * 1024 * 1024)],
        ] {
            assert!(backing_ranges(&ranges).is_err());
        }
        let (ranges, _) =
            backing_ranges(&[MemoryRange::new(0..4096), MemoryRange::new(4096..8192)]).unwrap();
        assert!(classify(&ranges, MemoryRange::new(0..8192)).is_err());
        let ambiguous = vec![
            GuestMemfdRange {
                range: MemoryRange::new(0..8192),
                offset: 0,
            },
            GuestMemfdRange {
                range: MemoryRange::new(0..8192),
                offset: 8192,
            },
        ];
        assert!(classify(&ambiguous, MemoryRange::new(0..4096)).is_err());
    }

    #[test]
    fn unmap_preserves_source_fd_offset_and_registration_identity() {
        let region = GuestMemfdRegion {
            size: 8192,
            guest_pfn: 7,
            offset: 4096,
            fd: 42,
            flags: (1 << mshv_bindings::MSHV_SET_MEM_BIT_WRITABLE)
                | (1 << mshv_bindings::MSHV_SET_MEM_BIT_EXECUTABLE),
            rsvd: [0; 3],
        };
        let unmap = region.unmap();
        assert_eq!(
            (unmap.size, unmap.guest_pfn, unmap.offset, unmap.fd),
            (region.size, region.guest_pfn, region.offset, region.fd)
        );
        assert_eq!(
            unmap.flags,
            region.flags | (1 << mshv_bindings::MSHV_SET_MEM_BIT_UNMAP)
        );
        assert_eq!(unmap.rsvd, [0; 3]);
    }

    #[test]
    fn unsupported_ioctl_is_an_error_not_a_fallback() {
        let args = CreateGuestMemfd {
            size: 4096,
            flags: 0,
        };
        let file = File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        let err = create_file(file.as_raw_fd(), &args).unwrap_err();
        assert!(matches!(&err,
            GuestMemfdError::Create(source) if source.raw_os_error() == Some(libc::ENOTTY)));
        assert!(err.to_string().contains("no legacy fallback"));
    }
}
