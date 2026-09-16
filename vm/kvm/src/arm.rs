// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host Arm KVM interfaces for SMCCC forwarding and experimental CCA trusted I/O.
//!
//! These layouts match the Linux CCA DA v7 headers. Nothing installs filters or
//! enables device assignment automatically.

use crate::Error;
use crate::Exit;
#[cfg(target_arch = "aarch64")]
use crate::Partition;
#[cfg(target_arch = "aarch64")]
use crate::Processor;
use crate::Result;
use crate::ioctl;
use crate::kvm_device_attr;
use crate::kvm_run;
use open_enum::open_enum;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;

pub const KVM_EXIT_ARM64_TIO_UAPI: u32 = 44;
pub const KVM_HYPERCALL_EXIT_SMC_UAPI: u64 = 1;
pub const KVM_HYPERCALL_EXIT_16BIT_UAPI: u64 = 2;

// arm-smccc-rhi.h: FAST_CALL, SMC_64, OWNER_STANDARD_HYP (5).
const fn rhi_call(function: u16) -> u32 {
    (1 << 31) | (1 << 30) | (5 << 24) | function as u32
}

open_enum! {
    pub enum RhiDaFunction: u32 {
        FEATURES = rhi_call(0x004b),
        OBJECT_SIZE = rhi_call(0x004c),
        OBJECT_READ = rhi_call(0x004d),
        VDEV_GET_MEASUREMENTS = rhi_call(0x0052),
        VDEV_GET_INTERFACE_REPORT = rhi_call(0x0053),
        VDEV_SET_TDI_STATE = rhi_call(0x0054),
    }
}

/// The VM-fd SMCCC filter packet, including its reserved bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KvmSmcccFilter {
    base: u32,
    nr_functions: u32,
    action: u8,
    pad: [u8; 15],
}

const RHI_DA_FILTERS: [KvmSmcccFilter; 2] = [
    KvmSmcccFilter {
        base: RhiDaFunction::FEATURES.0,
        nr_functions: 3,
        action: 2, // KVM_SMCCC_FILTER_FWD_TO_USER
        pad: [0; 15],
    },
    KvmSmcccFilter {
        base: RhiDaFunction::VDEV_GET_MEASUREMENTS.0,
        nr_functions: 3,
        action: 2,
        pad: [0; 15],
    },
];

fn smccc_filter_attr(filter: &KvmSmcccFilter) -> kvm_device_attr {
    kvm_device_attr {
        group: 0, // KVM_ARM_VM_SMCCC_CTRL
        attr: 0,  // KVM_ARM_VM_SMCCC_FILTER
        addr: std::ptr::from_ref(filter) as u64,
        flags: 0,
    }
}

fn set_rhi_da_filters(fd: BorrowedFd<'_>) -> Result<()> {
    for filter in &RHI_DA_FILTERS {
        // SAFETY: This VM attribute takes the complete filter packet. Both the
        // packet and the fd remain valid until the synchronous ioctl returns.
        unsafe { ioctl::kvm_set_device_attr(fd.as_raw_fd(), &smccc_filter_attr(filter)) }
            .map_err(Error::SetDeviceAttr)?;
    }
    Ok(())
}

#[cfg(target_arch = "aarch64")]
impl Partition {
    /// Forwards only the six synchronous RHI DA calls to userspace.
    ///
    /// Call before the first vCPU run, only for an explicitly configured DA VM.
    /// Host configuration, PSCI and all other default handling stay with KVM.
    /// If the second filter fails, the first remains installed; discard the VM
    /// rather than retrying or running it with a partial configuration.
    pub fn set_arm_rhi_da_filters(&self) -> Result<()> {
        use std::os::fd::AsFd;
        set_rhi_da_filters(self.vm.as_fd())
    }
}

// KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE, with the core
// register's byte offset divided by sizeof(__u32), not sizeof(__u64).
const fn core_reg_id(index: u64) -> u64 {
    0x6000_0000_0000_0000 | 0x0030_0000_0000_0000 | 0x0010_0000 | (index * 2)
}

fn read_smccc_arguments(mut get: impl FnMut(u64) -> Result<u64>) -> Result<[u64; 7]> {
    let mut args = [0; 7];
    for (index, value) in args.iter_mut().enumerate() {
        *value = get(core_reg_id(index as u64 + 1))?;
    }
    Ok(args)
}

fn write_smccc_results(
    results: [u64; 4],
    mut set: impl FnMut(u64, u64) -> Result<()>,
) -> Result<()> {
    for (index, value) in results.into_iter().enumerate() {
        set(core_reg_id(index as u64), value)?;
    }
    Ok(())
}

#[cfg(target_arch = "aarch64")]
impl Processor<'_> {
    /// Reads SMCCC arguments x1-x7 while the vCPU is stopped.
    ///
    /// The function ID comes from [`Exit::ArmHypercall`], not x0.
    pub fn read_arm_smccc_arguments(&self) -> Result<[u64; 7]> {
        read_smccc_arguments(|id| self.get_reg64(id))
    }

    /// Writes SMCCC results x0-x3 while the vCPU is stopped.
    ///
    /// On failure earlier registers may already have changed. Do not re-enter
    /// the vCPU with an incomplete response.
    pub fn write_arm_smccc_results(&self, results: [u64; 4]) -> Result<()> {
        write_smccc_results(results, |id, value| self.set_reg64(id, value))
    }
}

open_enum! {
    pub enum ArmTioExitReason: u64 {
        VDEV_VALIDATE_MAPPING = 0x08,
    }
}

/// The `kvm_run.cca_exit` packet in the pinned experimental kernel.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct KvmArm64Tio {
    flags: u64,
    nr: u64,
    vdev_id: u64,
    gpa_base: u64,
    gpa_top: u64,
    pa_base: u64,
    response: u64,
}

const _: () = {
    assert!(size_of::<KvmArm64Tio>() <= size_of::<crate::kvm_run__bindgen_ty_1>());
    assert!(align_of::<KvmArm64Tio>() <= align_of::<crate::kvm_run__bindgen_ty_1>());
};

/// A trusted-I/O exit. Dropping it without accepting leaves a rejection.
///
/// `gpa_top` is exclusive and is an input/output field in the kernel ABI.
/// The caller must validate the device and mapping before accepting.
#[derive(Debug)]
pub struct ArmTioExit<'a> {
    pub flags: u64,
    pub nr: ArmTioExitReason,
    pub vdev_id: u64,
    pub gpa_base: u64,
    pub gpa_top: &'a mut u64,
    pub pa_base: u64,
    response: &'a mut u64,
}

#[derive(Debug, thiserror::Error)]
#[error("unsupported Arm trusted-I/O exit: reason {nr:?}, flags {flags:#x}")]
pub struct UnsupportedArmTioExit {
    pub nr: ArmTioExitReason,
    pub flags: u64,
}

impl ArmTioExit<'_> {
    /// Permits kernel/RMM mapping validation on re-entry, not device DMA.
    ///
    /// Unknown reasons and flags remain rejected.
    pub fn accept(&mut self) -> std::result::Result<(), UnsupportedArmTioExit> {
        self.reject();
        if self.nr != ArmTioExitReason::VDEV_VALIDATE_MAPPING || self.flags != 0 {
            return Err(UnsupportedArmTioExit {
                nr: self.nr,
                flags: self.flags,
            });
        }
        *self.response = 0;
        Ok(())
    }

    /// Rejects on re-entry. The kernel treats any nonzero response as rejection.
    pub fn reject(&mut self) {
        *self.response = u64::MAX;
    }
}

pub(crate) fn hypercall_exit(run: &kvm_run) -> Exit<'_> {
    // SAFETY: The caller checked KVM_EXIT_HYPERCALL. Arm only fills nr/flags.
    let hypercall = unsafe { &run.__bindgen_anon_1.hypercall };
    Exit::ArmHypercall {
        nr: hypercall.nr,
        // SAFETY: Arm uses the 64-bit flags member, not x86's longmode member.
        flags: unsafe { hypercall.__bindgen_anon_1.flags },
    }
}

pub(crate) fn tio_exit(run: &mut kvm_run) -> Exit<'_> {
    // SAFETY: The caller checked exit 44. The pinned UAPI places seven u64s
    // at the start of the run union, whose size and alignment cover the packet.
    // This exclusive run reference also prevents re-entry while it is borrowed.
    let tio = unsafe { &mut *std::ptr::from_mut(&mut run.__bindgen_anon_1).cast::<KvmArm64Tio>() };
    // The kernel initializes this to success. Require explicit acceptance even
    // for known reasons; an unhandled exit must not authorize a mapping.
    tio.response = u64::MAX;
    Exit::ArmTio(ArmTioExit {
        flags: tio.flags,
        nr: ArmTioExitReason(tio.nr),
        vdev_id: tio.vdev_id,
        gpa_base: tio.gpa_base,
        gpa_top: &mut tio.gpa_top,
        pa_base: tio.pa_base,
        response: &mut tio.response,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::mem::align_of;
    use std::mem::offset_of;
    use std::mem::size_of;
    use std::os::fd::AsFd;
    use test_with_tracing::test;

    #[test]
    fn rhi_da_full_function_encodings_and_filter_ranges() {
        assert_eq!(RhiDaFunction::FEATURES.0, 0xc500_004b);
        assert_eq!(RhiDaFunction::OBJECT_SIZE.0, 0xc500_004c);
        assert_eq!(RhiDaFunction::OBJECT_READ.0, 0xc500_004d);
        assert_eq!(RhiDaFunction::VDEV_GET_MEASUREMENTS.0, 0xc500_0052);
        assert_eq!(RhiDaFunction::VDEV_GET_INTERFACE_REPORT.0, 0xc500_0053);
        assert_eq!(RhiDaFunction::VDEV_SET_TDI_STATE.0, 0xc500_0054);
        assert_eq!(RhiDaFunction(0xffff_ffff).0, 0xffff_ffff);

        let forwarded: Vec<_> = RHI_DA_FILTERS
            .iter()
            .flat_map(|filter| filter.base..filter.base + filter.nr_functions)
            .collect();
        assert_eq!(
            forwarded,
            [
                0xc500_004b,
                0xc500_004c,
                0xc500_004d,
                0xc500_0052,
                0xc500_0053,
                0xc500_0054,
            ]
        );
        // Exclude host configuration, CONTINUE, ABORT, owner 4, and PSCI.
        for id in [
            0xc500_004a,
            0xc500_004e,
            0xc500_004f,
            0xc500_0050,
            0xc500_0051,
            0xc500_0055,
            0xc500_0056,
            0xc400_004b,
            0x8400_0000,
            0xc400_0003,
        ] {
            assert!(!forwarded.contains(&id));
        }
    }

    #[test]
    fn smccc_filter_packet_and_vm_attribute_layout() {
        assert_eq!(size_of::<KvmSmcccFilter>(), 24);
        assert_eq!(align_of::<KvmSmcccFilter>(), 4);
        assert_eq!(offset_of!(KvmSmcccFilter, base), 0);
        assert_eq!(offset_of!(KvmSmcccFilter, nr_functions), 4);
        assert_eq!(offset_of!(KvmSmcccFilter, action), 8);
        assert_eq!(offset_of!(KvmSmcccFilter, pad), 9);
        assert_eq!(size_of::<kvm_device_attr>(), 24);
        assert_eq!(offset_of!(kvm_device_attr, flags), 0);
        assert_eq!(offset_of!(kvm_device_attr, group), 4);
        assert_eq!(offset_of!(kvm_device_attr, attr), 8);
        assert_eq!(offset_of!(kvm_device_attr, addr), 16);
        for filter in &RHI_DA_FILTERS {
            assert_eq!(filter.action, 2);
            assert_eq!(filter.pad, [0; 15]);
            let attr = smccc_filter_attr(filter);
            assert_eq!(attr.flags, 0);
            assert_eq!(attr.group, 0);
            assert_eq!(attr.attr, 0);
            assert_eq!(attr.addr, std::ptr::from_ref(filter) as u64);
        }
    }

    #[test]
    fn smccc_filter_failure_preserves_errno() {
        let file = File::open("/dev/null").unwrap();
        assert!(matches!(
            set_rhi_da_filters(file.as_fd()),
            Err(Error::SetDeviceAttr(nix::errno::Errno::ENOTTY))
        ));
    }

    #[test]
    fn smccc_one_reg_numbers_and_register_order() {
        let mut ids = Vec::new();
        let args = read_smccc_arguments(|id| {
            ids.push(id);
            Ok(0x100 + ids.len() as u64)
        })
        .unwrap();
        assert_eq!(args, [0x101, 0x102, 0x103, 0x104, 0x105, 0x106, 0x107]);
        assert_eq!(
            ids,
            [
                0x6030_0000_0010_0002,
                0x6030_0000_0010_0004,
                0x6030_0000_0010_0006,
                0x6030_0000_0010_0008,
                0x6030_0000_0010_000a,
                0x6030_0000_0010_000c,
                0x6030_0000_0010_000e,
            ]
        );
        let mut writes = Vec::new();
        write_smccc_results([u64::MAX, 11, 22, 33], |id, value| {
            writes.push((id, value));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            writes,
            [
                (0x6030_0000_0010_0000, u64::MAX),
                (0x6030_0000_0010_0002, 11),
                (0x6030_0000_0010_0004, 22),
                (0x6030_0000_0010_0006, 33),
            ]
        );
        assert_eq!(size_of::<crate::kvm_one_reg>(), 16);
        assert_eq!(offset_of!(crate::kvm_one_reg, id), 0);
        assert_eq!(offset_of!(crate::kvm_one_reg, addr), 8);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn core_register_ids_match_arm_bindings() {
        let base =
            crate::KVM_REG_ARM64 | crate::KVM_REG_SIZE_U64 | u64::from(crate::KVM_REG_ARM_CORE);
        for index in 0..8 {
            let offset = offset_of!(crate::kvm_regs, regs)
                + offset_of!(crate::user_pt_regs, regs)
                + index * size_of::<u64>();
            assert_eq!(core_reg_id(index as u64), base | (offset / 4) as u64);
        }
    }

    #[test]
    fn smccc_register_errors_stop_without_losing_errno() {
        for errno in [nix::errno::Errno::EIO, nix::errno::Errno::ENOTTY] {
            for fail_at in 0..7 {
                let mut reads = 0;
                let error = read_smccc_arguments(|_| {
                    reads += 1;
                    if reads == fail_at + 1 {
                        Err(Error::GetRegs(errno))
                    } else {
                        Ok(0)
                    }
                })
                .unwrap_err();
                assert!(matches!(error, Error::GetRegs(source) if source == errno));
                assert_eq!(reads, fail_at + 1);
            }
            for fail_at in 0..4 {
                let mut writes = 0;
                let error = write_smccc_results([0; 4], |_, _| {
                    writes += 1;
                    if writes == fail_at + 1 {
                        Err(Error::SetRegs(errno))
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
                assert!(matches!(error, Error::SetRegs(source) if source == errno));
                assert_eq!(writes, fail_at + 1);
            }
        }
    }

    #[test]
    fn arm_hypercall_packet_uses_only_nr_and_all_flags() {
        assert_eq!(KVM_HYPERCALL_EXIT_SMC_UAPI, 1);
        assert_eq!(KVM_HYPERCALL_EXIT_16BIT_UAPI, 2);
        let mut run = kvm_run {
            exit_reason: crate::KVM_EXIT_HYPERCALL,
            ..Default::default()
        };
        // SAFETY: This test selects the hypercall union member.
        let packet = unsafe { &mut run.__bindgen_anon_1.hypercall };
        let base = std::ptr::from_ref(packet) as usize;
        assert_eq!(size_of_val(packet), 72);
        assert_eq!(std::ptr::from_ref(&packet.nr) as usize - base, 0);
        assert_eq!(std::ptr::from_ref(&packet.args) as usize - base, 8);
        assert_eq!(std::ptr::from_ref(&packet.ret) as usize - base, 56);
        assert_eq!(
            std::ptr::from_ref(&packet.__bindgen_anon_1) as usize - base,
            64
        );
        packet.nr = 0xc500_004d;
        packet.args = [u64::MAX; 6];
        packet.ret = 0xfeed_face;
        let flags = (1 << 63) | KVM_HYPERCALL_EXIT_SMC_UAPI | KVM_HYPERCALL_EXIT_16BIT_UAPI;
        packet.__bindgen_anon_1.flags = flags;
        assert!(matches!(
            hypercall_exit(&run),
            Exit::ArmHypercall { nr: 0xc500_004d, flags: value } if value == flags
        ));
        // SAFETY: The hypercall member is still active; Arm decoding must not
        // use or change the x86 response field.
        assert_eq!(unsafe { run.__bindgen_anon_1.hypercall.ret }, 0xfeed_face);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_hypercall_contract_is_unchanged() {
        let mut run = kvm_run {
            exit_reason: crate::KVM_EXIT_HYPERCALL,
            ..Default::default()
        };
        run.__bindgen_anon_1.hypercall.nr = 12;
        run.__bindgen_anon_1.hypercall.args = [1, 2, 3, 4, 5, 6];
        run.__bindgen_anon_1.hypercall.__bindgen_anon_1.flags = 1 << 63;
        let Exit::Hypercall {
            nr,
            args,
            result,
            flags,
        } = crate::x86_hypercall_exit(&mut run)
        else {
            panic!("expected x86 hypercall");
        };
        assert_eq!(nr, 12);
        assert_eq!(args, [1, 2, 3, 4, 5, 6]);
        assert_eq!(flags, 1 << 63);
        *result = 0x1234;
        // SAFETY: The test selected the hypercall union member.
        assert_eq!(unsafe { run.__bindgen_anon_1.hypercall.ret }, 0x1234);
    }

    #[test]
    fn tio_packet_layout_matches_pinned_uapi() {
        assert_eq!(KVM_EXIT_ARM64_TIO_UAPI, 44);
        assert_eq!(ArmTioExitReason::VDEV_VALIDATE_MAPPING.0, 8);
        assert_eq!(size_of::<ArmTioExitReason>(), 8);
        assert_eq!(size_of::<KvmArm64Tio>(), 56);
        assert_eq!(align_of::<KvmArm64Tio>(), 8);
        assert_eq!(offset_of!(KvmArm64Tio, flags), 0);
        assert_eq!(offset_of!(KvmArm64Tio, nr), 8);
        assert_eq!(offset_of!(KvmArm64Tio, vdev_id), 16);
        assert_eq!(offset_of!(KvmArm64Tio, gpa_base), 24);
        assert_eq!(offset_of!(KvmArm64Tio, gpa_top), 32);
        assert_eq!(offset_of!(KvmArm64Tio, pa_base), 40);
        assert_eq!(offset_of!(KvmArm64Tio, response), 48);
        assert_eq!(offset_of!(kvm_run, __bindgen_anon_1), 32);
        assert_eq!(size_of::<crate::kvm_run__bindgen_ty_1>(), 256);
        assert!(align_of::<crate::kvm_run__bindgen_ty_1>() >= align_of::<KvmArm64Tio>());
    }

    fn tio_run(nr: u64, flags: u64) -> kvm_run {
        let mut run = kvm_run {
            exit_reason: KVM_EXIT_ARM64_TIO_UAPI,
            ..Default::default()
        };
        // Write the seven fields by their pinned byte offsets, independently
        // of the Rust overlay used by the decoder.
        let fields = [flags, nr, 0x1234, 0x4000, 0x6000, 0xa000, 0];
        // SAFETY: The default run packet initializes the complete union.
        let bytes = unsafe { &mut run.__bindgen_anon_1.padding };
        for (index, value) in fields.into_iter().enumerate() {
            for (byte_index, byte) in value.to_ne_bytes().into_iter().enumerate() {
                bytes[index * 8 + byte_index] = byte as _;
            }
        }
        run
    }

    fn tio_response(run: &kvm_run) -> u64 {
        // SAFETY: The test uses the TIO union member. Reading its raw bytes is
        // valid; padding covers the complete initialized packet.
        let bytes = unsafe { run.__bindgen_anon_1.padding };
        u64::from_ne_bytes(std::array::from_fn(|i| bytes[48 + i].to_ne_bytes()[0]))
    }

    #[test]
    fn tio_mapping_requires_explicit_acceptance_and_updates_in_place() {
        let mut run = tio_run(8, 0);
        {
            let Exit::ArmTio(exit) = tio_exit(&mut run) else {
                panic!("expected TIO exit");
            };
            assert_eq!(exit.flags, 0);
            assert_eq!(exit.nr, ArmTioExitReason::VDEV_VALIDATE_MAPPING);
            assert_eq!(exit.vdev_id, 0x1234);
            assert_eq!(exit.gpa_base, 0x4000);
            assert_eq!(*exit.gpa_top, 0x6000);
            assert_eq!(exit.pa_base, 0xa000);
        }
        assert_ne!(tio_response(&run), 0);
        {
            let Exit::ArmTio(mut exit) = tio_exit(&mut run) else {
                panic!("expected TIO exit");
            };
            *exit.gpa_top = 0x5000;
            exit.accept().unwrap();
        }
        assert_eq!(tio_response(&run), 0);
        // SAFETY: Read back the gpa_top input/output field from the raw packet.
        let bytes = unsafe { run.__bindgen_anon_1.padding };
        assert_eq!(
            u64::from_ne_bytes(std::array::from_fn(|i| bytes[32 + i].to_ne_bytes()[0])),
            0x5000
        );
        {
            let Exit::ArmTio(mut exit) = tio_exit(&mut run) else {
                panic!("expected TIO exit");
            };
            exit.accept().unwrap();
            exit.reject();
        }
        assert_ne!(tio_response(&run), 0);
    }

    #[test]
    fn unknown_tio_reason_and_flags_roundtrip_and_cannot_be_accepted() {
        for (nr, flags) in [(u64::MAX, 0), (8, 1 << 63), (0, u64::MAX)] {
            let mut run = tio_run(nr, flags);
            {
                let Exit::ArmTio(mut exit) = tio_exit(&mut run) else {
                    panic!("expected TIO exit");
                };
                assert_eq!(exit.nr.0, nr);
                assert_eq!(exit.flags, flags);
                let error = exit.accept().unwrap_err();
                assert_eq!(error.nr.0, nr);
                assert_eq!(error.flags, flags);
            }
            assert_ne!(tio_response(&run), 0);
        }
    }
}
