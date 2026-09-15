// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bindings for the Linux iommufd subsystem (`/dev/iommu`).
//!
//! Provides safe wrappers around iommufd ioctls for:
//! - IOAS allocation and DMA mapping (`IOMMU_IOAS_ALLOC`, `IOMMU_IOAS_MAP`,
//!   `IOMMU_IOAS_MAP_FILE`, `IOMMU_IOAS_UNMAP`)
//! - Hardware page table management (`IOMMU_HWPT_ALLOC`, `IOMMU_HWPT_INVALIDATE`)
//! - Hardware info query (`IOMMU_GET_HW_INFO`)
//! - Virtual IOMMU objects (`IOMMU_VIOMMU_ALLOC`, `IOMMU_VDEVICE_ALLOC`,
//!   `IOMMU_VEVENTQ_ALLOC`)
//! - Per-IOAS huge-page policy (`IOMMU_OPTION`)
//!
//! The IOAS path supports identity DMA mapping (Phase 4). The HWPT/vIOMMU
//! path supports nested stage 1 translation for VFIO passthrough (Phase 5).

use anyhow::Context as _;
use std::fs;
use std::os::unix::prelude::*;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// iommufd ioctl type character (';' = 0x3B).
const IOMMUFD_TYPE: u8 = b';';

/// Base command number for iommufd ioctls.
const IOMMUFD_CMD_BASE: u8 = 0x80;

// Command numbers (IOMMUFD_CMD_BASE + offset).
const IOMMUFD_CMD_DESTROY: u8 = IOMMUFD_CMD_BASE;
const IOMMUFD_CMD_IOAS_ALLOC: u8 = IOMMUFD_CMD_BASE + 1;
const IOMMUFD_CMD_IOAS_MAP: u8 = IOMMUFD_CMD_BASE + 5;
const IOMMUFD_CMD_IOAS_UNMAP: u8 = IOMMUFD_CMD_BASE + 6;
const IOMMUFD_CMD_OPTION: u8 = IOMMUFD_CMD_BASE + 7;
const IOMMUFD_CMD_HWPT_ALLOC: u8 = IOMMUFD_CMD_BASE + 9;
const IOMMUFD_CMD_IOAS_MAP_FILE: u8 = IOMMUFD_CMD_BASE + 15;
const IOMMUFD_CMD_GET_HW_INFO: u8 = IOMMUFD_CMD_BASE + 0x0a;
const IOMMUFD_CMD_HWPT_INVALIDATE: u8 = IOMMUFD_CMD_BASE + 0x0d;
const IOMMUFD_CMD_VIOMMU_ALLOC: u8 = IOMMUFD_CMD_BASE + 0x10;
const IOMMUFD_CMD_VDEVICE_ALLOC: u8 = IOMMUFD_CMD_BASE + 0x11;
const IOMMUFD_CMD_VEVENTQ_ALLOC: u8 = IOMMUFD_CMD_BASE + 0x13;

/// Flags for `IOMMU_IOAS_MAP`.
pub const IOMMU_IOAS_MAP_FIXED_IOVA: u32 = 1 << 0;
pub const IOMMU_IOAS_MAP_WRITEABLE: u32 = 1 << 1;
pub const IOMMU_IOAS_MAP_READABLE: u32 = 1 << 2;

const IOMMU_OPTION_HUGE_PAGES: u32 = 1;
const IOMMU_OPTION_OP_SET: u16 = 0;

mod ioctl {
    use nix::request_code_none;

    // IOMMUFD ioctls use _IO (no direction, just type + nr).
    // The kernel defines them as _IO(IOMMUFD_TYPE, cmd_nr).
    nix::ioctl_readwrite_bad!(
        iommu_destroy,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_DESTROY as u32
        ),
        super::IommuDestroy
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_alloc,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_IOAS_ALLOC as u32
        ),
        super::IommuIoasAlloc
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_map,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_IOAS_MAP as u32
        ),
        super::IommuIoasMap
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_map_file,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_IOAS_MAP_FILE as u32
        ),
        super::IommuIoasMapFile
    );
    nix::ioctl_readwrite_bad!(
        iommu_ioas_unmap,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_IOAS_UNMAP as u32
        ),
        super::IommuIoasUnmap
    );
    nix::ioctl_readwrite_bad!(
        iommu_option,
        request_code_none!(super::IOMMUFD_TYPE as u32, super::IOMMUFD_CMD_OPTION as u32),
        super::IommuOption
    );
    nix::ioctl_readwrite_bad!(
        iommu_hwpt_alloc,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_HWPT_ALLOC as u32
        ),
        super::IommuHwptAlloc
    );
    nix::ioctl_readwrite_bad!(
        iommu_get_hw_info,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_GET_HW_INFO as u32
        ),
        super::IommuGetHwInfo
    );
    nix::ioctl_readwrite_bad!(
        iommu_hwpt_invalidate,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_HWPT_INVALIDATE as u32
        ),
        super::IommuHwptInvalidate
    );
    nix::ioctl_readwrite_bad!(
        iommu_viommu_alloc,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_VIOMMU_ALLOC as u32
        ),
        super::IommuViommuAlloc
    );
    nix::ioctl_readwrite_bad!(
        iommu_vdevice_alloc,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_VDEVICE_ALLOC as u32
        ),
        super::IommuVdeviceAlloc
    );
    nix::ioctl_readwrite_bad!(
        iommu_veventq_alloc,
        request_code_none!(
            super::IOMMUFD_TYPE as u32,
            super::IOMMUFD_CMD_VEVENTQ_ALLOC as u32
        ),
        super::IommuVeventqAlloc
    );
}

// Kernel ABI structs — must match `include/uapi/linux/iommufd.h` exactly.

#[repr(C)]
struct IommuDestroy {
    size: u32,
    id: u32,
}

#[repr(C)]
struct IommuIoasAlloc {
    size: u32,
    flags: u32,
    out_ioas_id: u32,
}

#[repr(C)]
struct IommuIoasMap {
    size: u32,
    flags: u32,
    ioas_id: u32,
    __reserved: u32,
    user_va: u64,
    length: u64,
    iova: u64,
}

#[repr(C)]
struct IommuIoasMapFile {
    size: u32,
    flags: u32,
    ioas_id: u32,
    fd: i32,
    start: u64,
    length: u64,
    iova: u64,
}

#[repr(C)]
struct IommuIoasUnmap {
    size: u32,
    ioas_id: u32,
    iova: u64,
    length: u64,
}

#[repr(C)]
struct IommuOption {
    size: u32,
    option_id: u32,
    op: u16,
    __reserved: u16,
    object_id: u32,
    val64: u64,
}

impl IommuOption {
    fn huge_pages(ioas_id: u32, enabled: bool) -> Self {
        Self {
            size: size_of::<Self>() as u32,
            option_id: IOMMU_OPTION_HUGE_PAGES,
            op: IOMMU_OPTION_OP_SET,
            __reserved: 0,
            object_id: ioas_id,
            val64: u64::from(enabled),
        }
    }
}

/// Failure to change an IOAS's huge-page policy.
#[derive(Debug, thiserror::Error)]
#[error("IOMMU_OPTION_HUGE_PAGES failed for IOAS {ioas_id} (enabled={enabled})")]
pub struct IoasHugePagesError {
    pub ioas_id: u32,
    pub enabled: bool,
    #[source]
    pub errno: nix::errno::Errno,
}

// --- HWPT allocation ---

/// Flags for `IOMMU_HWPT_ALLOC`.
pub const IOMMU_HWPT_ALLOC_NEST_PARENT: u32 = 1 << 0;

/// HWPT data type: no type-specific data.
pub const IOMMU_HWPT_DATA_NONE: u32 = 0;
/// HWPT data type: ARM SMMUv3 (nested STE DW0-1).
pub const IOMMU_HWPT_DATA_ARM_SMMUV3: u32 = 2;

#[repr(C)]
struct IommuHwptAlloc {
    size: u32,
    flags: u32,
    dev_id: u32,
    pt_id: u32,
    out_hwpt_id: u32,
    __reserved: u32,
    data_type: u32,
    data_len: u32,
    data_uptr: u64,
    fault_id: u32,
    __reserved2: u32,
}

/// ARM SMMUv3 nested HWPT data: the first two double words of the STE.
///
/// Passed via `data_uptr` when `data_type == IOMMU_HWPT_DATA_ARM_SMMUV3`.
/// The kernel validates the STE fields and programs the host IOMMU.
#[repr(C)]
#[derive(IntoBytes, Immutable)]
pub struct IommuHwptArmSmmuv3 {
    pub ste: [u64; 2],
}

impl IommuHwptArmSmmuv3 {
    /// A valid S1-bypass STE in the little-endian format required by Linux.
    ///
    /// Sets `V=1` and `Cfg=0b100`; all other fields are zero.
    /// This bypasses only stage 1; the nesting parent's stage 2 still applies.
    /// Use with [`IOMMU_HWPT_DATA_ARM_SMMUV3`] and a vIOMMU as `pt_id`.
    pub const fn s1_bypass() -> Self {
        Self {
            ste: [(1_u64 | (4 << 1)).to_le(), 0],
        }
    }
}

// --- Hardware info query ---

/// HW info type: ARM SMMUv3.
pub const IOMMU_HW_INFO_TYPE_ARM_SMMUV3: u32 = 2;

#[repr(C)]
struct IommuGetHwInfo {
    size: u32,
    flags: u32,
    dev_id: u32,
    data_len: u32,
    data_uptr: u64,
    out_data_type: u32,
    out_max_pasid_log2: u8,
    __reserved: [u8; 3],
    out_capabilities: u64,
}

/// ARM SMMUv3 hardware information returned by `IOMMU_GET_HW_INFO`.
///
/// Contains the physical IOMMU's IDR register values. The VMM uses
/// these to cap the virtual SMMU's advertised capabilities.
#[repr(C)]
pub struct IommuHwInfoArmSmmuv3 {
    pub flags: u32,
    pub __reserved: u32,
    pub idr: [u32; 6],
    pub iidr: u32,
    pub aidr: u32,
}

// --- HWPT invalidation ---

/// Invalidation data type for ARM SMMUv3 (via vIOMMU).
pub const IOMMU_VIOMMU_INVALIDATE_DATA_ARM_SMMUV3: u32 = 1;

#[repr(C)]
struct IommuHwptInvalidate {
    size: u32,
    hwpt_id: u32,
    data_uptr: u64,
    data_type: u32,
    entry_len: u32,
    entry_num: u32,
    __reserved: u32,
}

/// Error from [`IommufdCtx::hwpt_invalidate`], pairing the underlying ioctl
/// errno with the kernel's reported handled-entry count.
#[derive(Debug, thiserror::Error)]
#[error("IOMMU_HWPT_INVALIDATE failed (kernel handled {handled} entries)")]
pub struct HwptInvalidateError {
    /// The underlying `IOMMU_HWPT_INVALIDATE` ioctl errno.
    #[source]
    pub errno: nix::errno::Errno,
    /// The kernel's in/out `entry_num` after the failed call: the number of
    /// leading entries it reports as handled before the failure. See the
    /// caveat on [`IommufdCtx::hwpt_invalidate`] — this is unreliable for early
    /// failures and may equal the input count.
    pub handled: u32,
}

// --- Virtual IOMMU ---

/// vIOMMU type: ARM SMMUv3.
pub const IOMMU_VIOMMU_TYPE_ARM_SMMUV3: u32 = 1;
/// vIOMMU type: KVM-associated Arm Realm SMMUv3 in the CCA integration ABI.
pub const IOMMU_VIOMMU_TYPE_ARM_REALM_SMMUV3: u32 = 3;

/// Failure to allocate a KVM-associated Realm vIOMMU.
///
/// In particular, `EINVAL` is not a reliable physical-IOMMU mismatch signal
/// for this type: it can also report a missing KVM association or invalid IRQ
/// configuration. Callers must not treat it as permission to try another parent.
#[derive(Debug, thiserror::Error)]
#[error("IOMMU_VIOMMU_ALLOC failed for Realm device {dev_id}, parent HWPT {hwpt_id}")]
pub struct RealmViommuAllocError {
    pub dev_id: u32,
    pub hwpt_id: u32,
    #[source]
    pub errno: nix::errno::Errno,
}

/// Outcome of [`IommufdCtx::viommu_alloc`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ViommuAlloc {
    /// The kernel-assigned vIOMMU object ID.
    Allocated(u32),
    /// The nesting parent belongs to a different physical IOMMU than the
    /// device. Callers may probe another candidate parent.
    Incompatible,
}

#[repr(C)]
struct IommuViommuAlloc {
    size: u32,
    flags: u32,
    r#type: u32,
    dev_id: u32,
    hwpt_id: u32,
    out_viommu_id: u32,
    data_len: u32,
    __reserved: u32,
    data_uptr: u64,
}

impl IommuViommuAlloc {
    fn new(viommu_type: u32, dev_id: u32, hwpt_id: u32) -> Self {
        Self {
            size: size_of::<Self>() as u32,
            flags: 0,
            r#type: viommu_type,
            dev_id,
            hwpt_id,
            out_viommu_id: 0,
            data_len: 0,
            __reserved: 0,
            data_uptr: 0,
        }
    }
}

fn classify_viommu_alloc(
    viommu_type: u32,
    result: Result<u32, nix::errno::Errno>,
) -> Result<ViommuAlloc, nix::errno::Errno> {
    match result {
        Ok(id) => Ok(ViommuAlloc::Allocated(id)),
        Err(nix::errno::Errno::EINVAL) if viommu_type == IOMMU_VIOMMU_TYPE_ARM_SMMUV3 => {
            Ok(ViommuAlloc::Incompatible)
        }
        Err(err) => Err(err),
    }
}

// --- Virtual device ---

#[repr(C)]
struct IommuVdeviceAlloc {
    size: u32,
    viommu_id: u32,
    dev_id: u32,
    out_vdevice_id: u32,
    virt_id: u64,
}

// --- Virtual event queue ---

/// vEVENTQ type: ARM SMMUv3.
pub const IOMMU_VEVENTQ_TYPE_ARM_SMMUV3: u32 = 1;

/// `IommufdVeventHeader::flags`: the queue lost one or more events before this
/// one. No event data follows a header carrying this flag.
pub const IOMMU_VEVENTQ_FLAG_LOST_EVENTS: u32 = 1 << 0;

#[repr(C)]
struct IommuVeventqAlloc {
    size: u32,
    flags: u32,
    viommu_id: u32,
    r#type: u32,
    veventq_depth: u32,
    out_veventq_id: u32,
    out_veventq_fd: u32,
    __reserved: u32,
}

/// Header for each event in a vEVENTQ fd read.
///
/// `sequence` is monotonic over `[0, i32::MAX]`, wrapping back to 0. A gap of
/// more than 1 between adjacent headers means the intervening events were lost.
#[repr(C)]
#[derive(Copy, Clone, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct IommufdVeventHeader {
    pub flags: u32,
    pub sequence: u32,
}

/// ARM SMMUv3 virtual event record (256-bit, little-endian).
///
/// Follows an `IommufdVeventHeader` in the vEVENTQ fd read stream. The four
/// quadwords are a verbatim SMMUv3 Event queue record, except that the kernel
/// has rewritten the StreamID field to the virtual StreamID the vDevice was
/// allocated with.
#[repr(C)]
#[derive(Copy, Clone, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct IommuVeventArmSmmuv3 {
    pub evt: [u64; 4],
}

/// An open iommufd file descriptor (`/dev/iommu`).
///
/// Wraps the fd and provides safe methods for the iommufd ioctls needed
/// to allocate an IOAS and map/unmap host memory into it.
pub struct IommufdCtx {
    file: fs::File,
}

impl IommufdCtx {
    /// Open `/dev/iommu` and return a new iommufd context.
    pub fn new() -> anyhow::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/iommu")
            .context("failed to open /dev/iommu")?;
        Ok(Self { file })
    }

    /// Wrap an existing iommufd file descriptor.
    pub fn from_file(file: fs::File) -> Self {
        Self { file }
    }

    /// Allocate a new IO Address Space (IOAS).
    ///
    /// Returns the kernel-assigned IOAS object ID.
    pub fn ioas_alloc(&self) -> anyhow::Result<u32> {
        let mut cmd = IommuIoasAlloc {
            size: size_of::<IommuIoasAlloc>() as u32,
            flags: 0,
            out_ioas_id: 0,
        };
        // SAFETY: fd is valid, struct is correctly sized and zeroed.
        unsafe {
            ioctl::iommu_ioas_alloc(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_IOAS_ALLOC failed")?;
        }
        Ok(cmd.out_ioas_id)
    }

    /// Enable or disable coalescing contiguous pages in this IOAS's DMA mappings.
    ///
    /// Linux enables coalescing by default. Passing `false` limits mappings to
    /// the host page size, as required by the initial Realm assignment path.
    /// This is not the VM's RAM hugetlb policy. Configure it before DMA mapping.
    pub fn ioas_set_huge_pages(
        &self,
        ioas_id: u32,
        enabled: bool,
    ) -> Result<(), IoasHugePagesError> {
        let mut cmd = IommuOption::huge_pages(ioas_id, enabled);
        // SAFETY: the fd is borrowed from self and cmd is a fully initialized,
        // correctly sized scalar-only ioctl buffer.
        unsafe { ioctl::iommu_option(self.file.as_raw_fd(), &mut cmd) }
            .map(|_| ())
            .map_err(|errno| IoasHugePagesError {
                ioas_id,
                enabled,
                errno,
            })
    }

    /// Map a user VA range into an IOAS at a fixed IOVA.
    ///
    /// `ioas_id` is the IOAS to map into. `iova` is the fixed IO virtual
    /// address. `user_va` is the host virtual address of the backing memory.
    /// `length` is the size in bytes (must be page-aligned).
    ///
    /// # Safety
    /// `user_va` must point to valid, backed memory for `length` bytes.
    /// The memory must remain mapped for the lifetime of this IOAS mapping.
    pub unsafe fn ioas_map(
        &self,
        ioas_id: u32,
        iova: u64,
        user_va: u64,
        length: u64,
        writable: bool,
    ) -> anyhow::Result<()> {
        let mut flags = IOMMU_IOAS_MAP_FIXED_IOVA | IOMMU_IOAS_MAP_READABLE;
        if writable {
            flags |= IOMMU_IOAS_MAP_WRITEABLE;
        }
        let mut cmd = IommuIoasMap {
            size: size_of::<IommuIoasMap>() as u32,
            flags,
            ioas_id,
            __reserved: 0,
            user_va,
            length,
            iova,
        };
        // SAFETY: fd is valid, struct correctly constructed. Caller
        // guarantees user_va is backed and stable.
        unsafe {
            ioctl::iommu_ioas_map(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_IOAS_MAP failed")?;
        }
        Ok(())
    }

    /// Map a file/memfd range into an IOAS at a fixed IOVA via
    /// `IOMMU_IOAS_MAP_FILE`.
    ///
    /// Unlike [`Self::ioas_map`], the kernel pins the backing folios directly
    /// from `fd`, so no host VA is required. `start` is the byte offset within
    /// the file; like [`Self::ioas_map`], both `start` and `length` must be
    /// page-aligned. Requires a kernel with `IOMMU_IOAS_MAP_FILE` (Linux
    /// 6.13+).
    pub fn ioas_map_file(
        &self,
        ioas_id: u32,
        iova: u64,
        fd: RawFd,
        start: u64,
        length: u64,
        writable: bool,
    ) -> anyhow::Result<()> {
        let mut flags = IOMMU_IOAS_MAP_FIXED_IOVA | IOMMU_IOAS_MAP_READABLE;
        if writable {
            flags |= IOMMU_IOAS_MAP_WRITEABLE;
        }
        let mut cmd = IommuIoasMapFile {
            size: size_of::<IommuIoasMapFile>() as u32,
            flags,
            ioas_id,
            fd,
            start,
            length,
            iova,
        };
        // SAFETY: the iommufd fd is valid and the struct is correctly sized and
        // constructed. `fd` is only read during the ioctl.
        unsafe {
            ioctl::iommu_ioas_map_file(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_IOAS_MAP_FILE failed")?;
        }
        Ok(())
    }

    /// Unmap an IOVA range from an IOAS.
    ///
    /// Returns the number of bytes actually unmapped.
    pub fn ioas_unmap(&self, ioas_id: u32, iova: u64, length: u64) -> anyhow::Result<u64> {
        let mut cmd = IommuIoasUnmap {
            size: size_of::<IommuIoasUnmap>() as u32,
            ioas_id,
            iova,
            length,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::iommu_ioas_unmap(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_IOAS_UNMAP failed")?;
        }
        Ok(cmd.length)
    }

    /// Destroy an iommufd object by its ID.
    pub fn destroy(&self, id: u32) -> anyhow::Result<()> {
        let mut cmd = IommuDestroy {
            size: size_of::<IommuDestroy>() as u32,
            id,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::iommu_destroy(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_DESTROY failed")?;
        }
        Ok(())
    }

    /// Allocate a hardware page table (HWPT).
    ///
    /// For a **nesting parent** (S2): set `flags = IOMMU_HWPT_ALLOC_NEST_PARENT`,
    /// `pt_id` = IOAS ID, `data_type = IOMMU_HWPT_DATA_NONE`.
    ///
    /// For a **nested child** (S1): set `flags = 0`, `pt_id` = parent HWPT ID
    /// or vIOMMU ID, `data_type = IOMMU_HWPT_DATA_ARM_SMMUV3`, and pass the
    /// STE data via `data`.
    ///
    /// Returns the kernel-assigned HWPT object ID.
    pub fn hwpt_alloc(
        &self,
        flags: u32,
        dev_id: u32,
        pt_id: u32,
        data_type: u32,
        data: Option<&IommuHwptArmSmmuv3>,
    ) -> anyhow::Result<u32> {
        let (data_uptr, data_len) = match data {
            Some(data) => (
                std::ptr::from_ref(data) as u64,
                size_of::<IommuHwptArmSmmuv3>() as u32,
            ),
            None => (0, 0),
        };
        let mut cmd = IommuHwptAlloc {
            size: size_of::<IommuHwptAlloc>() as u32,
            flags,
            dev_id,
            pt_id,
            out_hwpt_id: 0,
            __reserved: 0,
            data_type,
            data_len,
            data_uptr,
            fault_id: 0,
            __reserved2: 0,
        };
        // SAFETY: the fd is valid and `cmd` is correctly constructed.
        // `data_uptr`/`data_len` are derived from the optional live `data`
        // borrow (or null/zero when absent), so the kernel reads only within a
        // valid, fully-initialized `#[repr(C)]` buffer.
        unsafe {
            ioctl::iommu_hwpt_alloc(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_HWPT_ALLOC failed")?;
        }
        Ok(cmd.out_hwpt_id)
    }

    /// Query hardware information for a device's IOMMU.
    ///
    /// Returns `(out_data_type, out_capabilities)`. The type-specific data is
    /// written into `out_info`.
    pub fn get_hw_info(
        &self,
        dev_id: u32,
        out_info: &mut IommuHwInfoArmSmmuv3,
    ) -> anyhow::Result<(u32, u64)> {
        let mut cmd = IommuGetHwInfo {
            size: size_of::<IommuGetHwInfo>() as u32,
            flags: 0,
            dev_id,
            data_len: size_of::<IommuHwInfoArmSmmuv3>() as u32,
            data_uptr: std::ptr::from_mut(out_info) as u64,
            out_data_type: 0,
            out_max_pasid_log2: 0,
            __reserved: [0; 3],
            out_capabilities: 0,
        };
        // SAFETY: the fd is valid and `cmd` is correctly constructed.
        // `data_uptr`/`data_len` are derived from the live, exclusively
        // borrowed `out_info`, so the kernel writes at most
        // `size_of::<IommuHwInfoArmSmmuv3>()` bytes into a valid buffer. Every
        // field of that `#[repr(C)]` struct is an integer, so any bytes the
        // kernel writes form a valid value.
        unsafe {
            ioctl::iommu_get_hw_info(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_GET_HW_INFO failed")?;
        }
        Ok((cmd.out_data_type, cmd.out_capabilities))
    }

    /// Invalidate IOMMU caches via a nested HWPT or vIOMMU.
    ///
    /// `hwpt_id` is a nested HWPT ID or vIOMMU ID. Each entry in `entries` is a
    /// raw 128-bit invalidation command as a `[qw0, qw1]` quadword pair; the
    /// kernel parses the opcode and operands per `data_type`.
    ///
    /// On full success returns the number of entries handled (always
    /// `entries.len()`). On failure returns a [`HwptInvalidateError`] carrying
    /// the kernel's in/out `entry_num` — the count of leading entries it
    /// reports as handled before the failure — so the caller can locate the
    /// offending entry. The kernel writes `entry_num` back even on error.
    ///
    /// Caveat: `entry_num` is only meaningful when the kernel reached its
    /// per-entry processing loop. For an early failure (notably `-ENOMEM`
    /// allocating the kernel-side scratch array) the field is left at the
    /// input count, so [`HwptInvalidateError::handled`] can equal
    /// `entries.len()` despite nothing being handled. Callers must treat
    /// `handled >= entries.len()` on the error path as "position unknown".
    pub fn hwpt_invalidate(
        &self,
        hwpt_id: u32,
        data_type: u32,
        entries: &[[u64; 2]],
    ) -> Result<u32, HwptInvalidateError> {
        let entry_num = u32::try_from(entries.len()).map_err(|_| HwptInvalidateError {
            errno: nix::errno::Errno::EINVAL,
            handled: 0,
        })?;
        let mut cmd = IommuHwptInvalidate {
            size: size_of::<IommuHwptInvalidate>() as u32,
            hwpt_id,
            data_uptr: entries.as_ptr() as u64,
            data_type,
            entry_len: size_of::<[u64; 2]>() as u32,
            entry_num,
            __reserved: 0,
        };
        // SAFETY: the fd is valid and `cmd` is correctly constructed.
        // `data_uptr`/`entry_len`/`entry_num` are derived from the live
        // `entries` slice, so the kernel reads only within a valid,
        // fully-initialized array.
        let res = unsafe { ioctl::iommu_hwpt_invalidate(self.file.as_raw_fd(), &mut cmd) };
        // The kernel writes `entry_num` (the number of entries it handled) back
        // even on failure, so read it regardless of the ioctl result.
        match res {
            Ok(_) => Ok(cmd.entry_num),
            Err(errno) => Err(HwptInvalidateError {
                errno,
                handled: cmd.entry_num,
            }),
        }
    }

    /// Allocate a virtual IOMMU (vIOMMU).
    ///
    /// `viommu_type` should be `IOMMU_VIOMMU_TYPE_ARM_SMMUV3` for SMMUv3.
    /// `dev_id` is a device bound to the physical IOMMU backing this vIOMMU.
    /// `hwpt_id` is the nesting parent HWPT to associate with.
    ///
    /// For `IOMMU_VIOMMU_TYPE_ARM_SMMUV3`, returns [`ViommuAlloc::Incompatible`]
    /// when the nesting parent and device belong to different physical SMMUs.
    /// For this type all generic fields
    /// are fixed to valid Arm SMMUv3 values and `hwpt_id` is supplied by
    /// [`IommufdCtx::hwpt_alloc`] with `IOMMU_HWPT_ALLOC_NEST_PARENT`; after
    /// those core checks, the Arm driver returns `EINVAL` only when the parent
    /// domain's SMMU differs from the device's SMMU. Other types propagate
    /// `EINVAL` as an error. Use [`Self::viommu_alloc_realm`] for Realm allocation.
    pub fn viommu_alloc(
        &self,
        viommu_type: u32,
        dev_id: u32,
        hwpt_id: u32,
    ) -> anyhow::Result<ViommuAlloc> {
        classify_viommu_alloc(
            viommu_type,
            self.viommu_alloc_raw(viommu_type, dev_id, hwpt_id),
        )
        .context("IOMMU_VIOMMU_ALLOC failed")
    }

    /// Allocate an Arm Realm vIOMMU, preserving every kernel error.
    ///
    /// Before binding the VFIO cdev to this IOMMUFD, associate it with the KVM
    /// VM using `KVM_DEV_VFIO_FILE_ADD`. The kernel obtains that KVM association
    /// from `dev_id` and creates the Realm if necessary. `hwpt_id` must be a
    /// nesting parent for the device's physical SMMU.
    ///
    /// The caller owns the returned object ID and must destroy dependent
    /// objects before destroying it. This does not lock or accept a TDISP
    /// device, attach an HWPT, or enable DMA.
    pub fn viommu_alloc_realm(
        &self,
        dev_id: u32,
        hwpt_id: u32,
    ) -> Result<u32, RealmViommuAllocError> {
        self.viommu_alloc_raw(IOMMU_VIOMMU_TYPE_ARM_REALM_SMMUV3, dev_id, hwpt_id)
            .map_err(|errno| RealmViommuAllocError {
                dev_id,
                hwpt_id,
                errno,
            })
    }

    fn viommu_alloc_raw(
        &self,
        viommu_type: u32,
        dev_id: u32,
        hwpt_id: u32,
    ) -> Result<u32, nix::errno::Errno> {
        let mut cmd = IommuViommuAlloc::new(viommu_type, dev_id, hwpt_id);
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe { ioctl::iommu_viommu_alloc(self.file.as_raw_fd(), &mut cmd) }?;
        Ok(cmd.out_viommu_id)
    }

    /// Allocate a virtual device (vDevice) on a vIOMMU.
    ///
    /// `virt_id` is the virtual stream ID (e.g., guest BDF for SMMUv3).
    ///
    /// Returns the kernel-assigned vDevice object ID.
    pub fn vdevice_alloc(&self, viommu_id: u32, dev_id: u32, virt_id: u64) -> anyhow::Result<u32> {
        let mut cmd = IommuVdeviceAlloc {
            size: size_of::<IommuVdeviceAlloc>() as u32,
            viommu_id,
            dev_id,
            out_vdevice_id: 0,
            virt_id,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::iommu_vdevice_alloc(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_VDEVICE_ALLOC failed")?;
        }
        Ok(cmd.out_vdevice_id)
    }

    /// Allocate a virtual event queue (vEVENTQ) on a vIOMMU.
    ///
    /// `veventq_type` should be `IOMMU_VEVENTQ_TYPE_ARM_SMMUV3` for SMMUv3.
    /// `depth` is the maximum number of events in the queue.
    ///
    /// Returns `(veventq_id, veventq_fd)`. The fd is an eventfd-style file
    /// descriptor that can be polled for fault events.
    pub fn veventq_alloc(
        &self,
        viommu_id: u32,
        veventq_type: u32,
        depth: u32,
    ) -> anyhow::Result<(u32, fs::File)> {
        let mut cmd = IommuVeventqAlloc {
            size: size_of::<IommuVeventqAlloc>() as u32,
            flags: 0,
            viommu_id,
            r#type: veventq_type,
            veventq_depth: depth,
            out_veventq_id: 0,
            out_veventq_fd: 0,
            __reserved: 0,
        };
        // SAFETY: fd is valid, struct correctly constructed.
        unsafe {
            ioctl::iommu_veventq_alloc(self.file.as_raw_fd(), &mut cmd)
                .context("IOMMU_VEVENTQ_ALLOC failed")?;
        }
        // SAFETY: kernel returned a valid fd in out_veventq_fd.
        let veventq_file = unsafe { fs::File::from_raw_fd(cmd.out_veventq_fd as RawFd) };
        Ok((cmd.out_veventq_id, veventq_file))
    }
}

impl AsFd for IommufdCtx {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl AsRawFd for IommufdCtx {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::errno::Errno;
    use std::mem::align_of;
    use std::mem::offset_of;
    use test_with_tracing::test;

    #[test]
    fn realm_object_abi_matches_linux() {
        assert_eq!(IOMMU_VIOMMU_TYPE_ARM_REALM_SMMUV3, 3);
        assert_eq!(
            nix::request_code_none!(IOMMUFD_TYPE, IOMMUFD_CMD_OPTION),
            0x3b87
        );
        assert_eq!(
            nix::request_code_none!(IOMMUFD_TYPE, IOMMUFD_CMD_VIOMMU_ALLOC),
            0x3b90
        );
        assert_eq!(size_of::<IommuOption>(), 24);
        assert_eq!(align_of::<IommuOption>(), 8);
        assert_eq!(offset_of!(IommuOption, size), 0);
        assert_eq!(offset_of!(IommuOption, option_id), 4);
        assert_eq!(offset_of!(IommuOption, op), 8);
        assert_eq!(offset_of!(IommuOption, __reserved), 10);
        assert_eq!(offset_of!(IommuOption, object_id), 12);
        assert_eq!(offset_of!(IommuOption, val64), 16);
        assert_eq!(size_of::<IommuViommuAlloc>(), 40);
        assert_eq!(align_of::<IommuViommuAlloc>(), 8);
        assert_eq!(offset_of!(IommuViommuAlloc, size), 0);
        assert_eq!(offset_of!(IommuViommuAlloc, flags), 4);
        assert_eq!(offset_of!(IommuViommuAlloc, r#type), 8);
        assert_eq!(offset_of!(IommuViommuAlloc, dev_id), 12);
        assert_eq!(offset_of!(IommuViommuAlloc, hwpt_id), 16);
        assert_eq!(offset_of!(IommuViommuAlloc, out_viommu_id), 20);
        assert_eq!(offset_of!(IommuViommuAlloc, data_len), 24);
        assert_eq!(offset_of!(IommuViommuAlloc, __reserved), 28);
        assert_eq!(offset_of!(IommuViommuAlloc, data_uptr), 32);
    }

    #[test]
    fn huge_page_option_initializes_all_fields() {
        for enabled in [false, true] {
            let cmd = IommuOption::huge_pages(71, enabled);
            assert_eq!(cmd.size, 24);
            assert_eq!(cmd.option_id, 1);
            assert_eq!(cmd.op, 0);
            assert_eq!(cmd.__reserved, 0);
            assert_eq!(cmd.object_id, 71);
            assert_eq!(cmd.val64, u64::from(enabled));
        }
    }

    #[test]
    fn realm_viommu_request_has_no_extra_data() {
        let cmd = IommuViommuAlloc::new(IOMMU_VIOMMU_TYPE_ARM_REALM_SMMUV3, 17, 23);
        assert_eq!(cmd.size, 40);
        assert_eq!(cmd.flags, 0);
        assert_eq!(cmd.r#type, 3);
        assert_eq!(cmd.dev_id, 17);
        assert_eq!(cmd.hwpt_id, 23);
        assert_eq!(cmd.out_viommu_id, 0);
        assert_eq!(cmd.data_len, 0);
        assert_eq!(cmd.__reserved, 0);
        assert_eq!(cmd.data_uptr, 0);
    }

    #[test]
    fn realm_allocation_errors_are_not_parent_probe_results() {
        for kind in [
            0,
            IOMMU_VIOMMU_TYPE_ARM_SMMUV3,
            2,
            IOMMU_VIOMMU_TYPE_ARM_REALM_SMMUV3,
            u32::MAX,
        ] {
            assert_eq!(
                classify_viommu_alloc(kind, Ok(59)),
                Ok(ViommuAlloc::Allocated(59))
            );
            for errno in [
                Errno::EINVAL,
                Errno::EOPNOTSUPP,
                Errno::EIO,
                Errno::ENOMEM,
                Errno::ENOTTY,
            ] {
                let expected = if kind == IOMMU_VIOMMU_TYPE_ARM_SMMUV3 && errno == Errno::EINVAL {
                    Ok(ViommuAlloc::Incompatible)
                } else {
                    Err(errno)
                };
                assert_eq!(classify_viommu_alloc(kind, Err(errno)), expected);
            }
        }
    }

    #[test]
    fn s1_bypass_ste_is_little_endian() {
        let data = IommuHwptArmSmmuv3::s1_bypass();
        assert_eq!(size_of::<IommuHwptArmSmmuv3>(), 16);
        assert_eq!(
            data.as_bytes(),
            &[9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn realm_helpers_preserve_ioctl_errno() {
        let ctx = IommufdCtx::from_file(fs::File::open("/dev/null").unwrap());
        let error = ctx.viommu_alloc_realm(17, 23).unwrap_err();
        assert_eq!(error.errno, Errno::ENOTTY);
        assert_eq!(error.dev_id, 17);
        assert_eq!(error.hwpt_id, 23);
        let error = ctx.ioas_set_huge_pages(71, false).unwrap_err();
        assert_eq!(error.errno, Errno::ENOTTY);
        assert_eq!(error.ioas_id, 71);
        assert!(!error.enabled);
    }
}
