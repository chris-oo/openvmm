// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source and runtime identities for the qualified CCA v15 platform.

pub const LINUX_REVISION: &str = "4ddbc65b5b408c37605110166a8da19f4dd0e180";
pub const LINUX_RELEASE: &str = "7.2.0-rc1";
pub const LINUX_CONFIG_SHA256: &str =
    "51fe0612694c51f19b548cc4efe5ed8bc1d486e3f7299f158d01bdd3e43afdea";
pub const LINUX_IMAGE_SHA256: &str =
    "2f4dde0a43269ede897b2382b77130e63427b32cb23e968cfc494e82320d5482";
pub const TF_RMM_REVISION: &str = "f00eac344b6f7c18abc6dad1948b07e9a82ff9f0";
pub const TF_RMM_IMAGE_SHA256: &str =
    "d8e1b42ec5b995400c7d37bdfcd3d1249dc397fe55642bda088064cec98a09d4";
pub const TF_A_REVISION: &str = "da738d5eae93af342fdc4995dd3c05acb4c9d757";
pub const TF_A_FLASH_SHA256: &str =
    "a9a1d7b0c7d331062a93e0f152ffd856f7e72ad51efcd29b3065c9a8e0f3c4dd";
pub const OPENVMM_DEPS_RELEASE: &str = "0.3.0-139";
pub const KERNEL_ARCHIVE_SHA256: &str =
    "852e5b6edf09b1e54ac39ba50f34cbe502b24f03b4598a5bc26db29d138217eb";
pub const RMM_ARCHIVE_SHA256: &str =
    "f63d24ec820f5ff80fcf9f4191380f7980baedafd4cb14ea3dd0abc045c64502";
pub const TFA_ARCHIVE_SHA256: &str =
    "480e79a32c9bc46f75120b840b8aeb454696bf5b64d8c3968d2e67a3e6eb900c";
pub const INITRD_ARCHIVE_SHA256: &str =
    "132b4b4e66b032ee108cd06fc17aee5660b930aade9a3dace4525709a5fbf344";
pub const QEMU_MACHINE: &str = "virt,secure=on,virtualization=on,gic-version=3,acpi=off";
pub const QEMU_CPU: &str = "max,x-rme=on,lpa2=off,sme=off,pauth-impdef=on";
pub const QEMU_KERNEL_LOAD_ADDRESS: i64 = 0x50080000;
pub const QEMU_INITRD_LOAD_ADDRESS: i64 = 0x48000000;
pub const QEMU_INITRD_MAX_SIZE: u64 = 0x08080000;
