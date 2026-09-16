// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arm SMCCC/RHI ABI values, also available to cross-host protocol tests.

use open_enum::open_enum;

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
