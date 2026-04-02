// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A crate containing the list of images stored in Azure Blob Storage for
//! in-tree VMM tests.
//!
//! NOTE: with the introduction of
//! [`petri_artifacts_vmm_test::artifacts::test_vhd`], this crate no longer
//! contains any interesting metadata about any VHDs, and only serves as a
//! bridge between the new petri artifact types in `test_vhd`, and existing code
//! that uses these types in flowey / xtask.
//!
//! FUTURE: this crate should be removed entirely, and flowey / xtask should be
//! updated to use the underlying artifact types themselves.

#![forbid(unsafe_code)]

use petri_artifacts_vmm_test::tags::IsHostedOnHvliteAzureBlobStore;

// re-export for convenience
pub use petri_artifacts_core::HasBuildMapping;

/// Build targets that correspond to fields on `BuildSelections` in flowey.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BuildTarget {
    /// Build openvmm binary
    Openvmm,
    /// Build openvmm_vhost binary
    OpenvmmVhost,
    /// Build OpenHCL IGVM files
    Openhcl,
    /// Build guest_test_uefi
    GuestTestUefi,
    /// Build TMK test kernels
    Tmks,
    /// Build TMK VMM for Windows
    TmkVmmWindows,
    /// Build TMK VMM for Linux
    TmkVmmLinux,
    /// Build vmgstool
    Vmgstool,
    /// Build pipette for Windows
    PipetteWindows,
    /// Build pipette for Linux
    PipetteLinux,
    /// Build TPM guest tests for Windows
    TpmGuestTestsWindows,
    /// Build TPM guest tests for Linux
    TpmGuestTestsLinux,
    /// Build test IGVM agent RPC server
    TestIgvmAgentRpcServer,
    /// Build prep_steps tool
    PrepSteps,
}

/// What the build system needs to do to provide this artifact.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactBuildCategory {
    /// Must be cargo-built.
    Build(BuildTarget),
    /// Downloaded from an external source (VHDs, ISOs, VMGS files).
    Download {
        /// Which known test artifact to download.
        artifact: KnownTestArtifacts,
        /// Additional build targets required when this artifact is used.
        also_build: &'static [BuildTarget],
    },
    /// Downloaded release IGVM from GitHub.
    ReleaseDownload,
    /// Always available from deps/environment (firmware, log dir, etc.).
    AlwaysAvailable,
}

/// Extension trait that provides the concrete build category for an artifact.
pub trait ArtifactBuildInfo: HasBuildMapping {
    /// The build category for this artifact.
    const BUILD_CATEGORY: ArtifactBuildCategory;
}

/// Helper macro to implement `ArtifactBuildInfo` (the `HasBuildMapping` marker
/// impl must live in the crate that defines the artifact type).
macro_rules! impl_build_info {
    ($artifact:ty, $category:expr) => {
        impl ArtifactBuildInfo for $artifact {
            const BUILD_CATEGORY: ArtifactBuildCategory = $category;
        }
    };
}

/// Helper macro to add an entry to the lookup table, referencing trait consts.
macro_rules! build_table_entry {
    ($artifact:ty) => {
        (
            <$artifact as petri_artifacts_core::ArtifactId>::GLOBAL_UNIQUE_ID,
            <$artifact as ArtifactBuildInfo>::BUILD_CATEGORY,
        )
    };
}

// --- Build mapping implementations ---

use petri_artifacts_vmm_test::artifacts;

// OpenVMM binaries
impl_build_info!(
    artifacts::OPENVMM_WIN_X64,
    ArtifactBuildCategory::Build(BuildTarget::Openvmm)
);
impl_build_info!(
    artifacts::OPENVMM_LINUX_X64,
    ArtifactBuildCategory::Build(BuildTarget::Openvmm)
);
impl_build_info!(
    artifacts::OPENVMM_WIN_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::Openvmm)
);
impl_build_info!(
    artifacts::OPENVMM_LINUX_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::Openvmm)
);
impl_build_info!(
    artifacts::OPENVMM_MACOS_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::Openvmm)
);

// OpenVMM vhost binaries
impl_build_info!(
    artifacts::OPENVMM_VHOST_LINUX_X64,
    ArtifactBuildCategory::Build(BuildTarget::OpenvmmVhost)
);
impl_build_info!(
    artifacts::OPENVMM_VHOST_LINUX_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::OpenvmmVhost)
);

// OpenHCL IGVM (built)
impl_build_info!(
    artifacts::openhcl_igvm::LATEST_STANDARD_X64,
    ArtifactBuildCategory::Build(BuildTarget::Openhcl)
);
impl_build_info!(
    artifacts::openhcl_igvm::LATEST_STANDARD_DEV_KERNEL_X64,
    ArtifactBuildCategory::Build(BuildTarget::Openhcl)
);
impl_build_info!(
    artifacts::openhcl_igvm::LATEST_CVM_X64,
    ArtifactBuildCategory::Build(BuildTarget::Openhcl)
);
impl_build_info!(
    artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64,
    ArtifactBuildCategory::Build(BuildTarget::Openhcl)
);
impl_build_info!(
    artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::Openhcl)
);
impl_build_info!(
    artifacts::openhcl_igvm::LATEST_STANDARD_DEV_KERNEL_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::Openhcl)
);

// OpenHCL usermode binaries (built as part of IGVM)
impl_build_info!(
    artifacts::openhcl_igvm::um_bin::LATEST_LINUX_DIRECT_TEST_X64,
    ArtifactBuildCategory::Build(BuildTarget::Openhcl)
);
impl_build_info!(
    artifacts::openhcl_igvm::um_dbg::LATEST_LINUX_DIRECT_TEST_X64,
    ArtifactBuildCategory::Build(BuildTarget::Openhcl)
);

// Release IGVM files (downloaded from GitHub releases)
impl_build_info!(
    artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_X64,
    ArtifactBuildCategory::ReleaseDownload
);
impl_build_info!(
    artifacts::openhcl_igvm::LATEST_RELEASE_LINUX_DIRECT_X64,
    ArtifactBuildCategory::ReleaseDownload
);
impl_build_info!(
    artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_AARCH64,
    ArtifactBuildCategory::ReleaseDownload
);

// Guest test UEFI
impl_build_info!(
    artifacts::test_vhd::GUEST_TEST_UEFI_X64,
    ArtifactBuildCategory::Build(BuildTarget::GuestTestUefi)
);
impl_build_info!(
    artifacts::test_vhd::GUEST_TEST_UEFI_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::GuestTestUefi)
);

// TMKs
impl_build_info!(
    artifacts::tmks::SIMPLE_TMK_X64,
    ArtifactBuildCategory::Build(BuildTarget::Tmks)
);
impl_build_info!(
    artifacts::tmks::SIMPLE_TMK_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::Tmks)
);

// TMK VMM Windows
impl_build_info!(
    artifacts::tmks::TMK_VMM_WIN_X64,
    ArtifactBuildCategory::Build(BuildTarget::TmkVmmWindows)
);
impl_build_info!(
    artifacts::tmks::TMK_VMM_WIN_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::TmkVmmWindows)
);

// TMK VMM Linux
impl_build_info!(
    artifacts::tmks::TMK_VMM_LINUX_X64,
    ArtifactBuildCategory::Build(BuildTarget::TmkVmmLinux)
);
impl_build_info!(
    artifacts::tmks::TMK_VMM_LINUX_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::TmkVmmLinux)
);
impl_build_info!(
    artifacts::tmks::TMK_VMM_LINUX_X64_MUSL,
    ArtifactBuildCategory::Build(BuildTarget::TmkVmmLinux)
);
impl_build_info!(
    artifacts::tmks::TMK_VMM_LINUX_AARCH64_MUSL,
    ArtifactBuildCategory::Build(BuildTarget::TmkVmmLinux)
);
impl_build_info!(
    artifacts::tmks::TMK_VMM_MACOS_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::TmkVmmLinux)
);

// VMGSTool
impl_build_info!(
    artifacts::VMGSTOOL_WIN_X64,
    ArtifactBuildCategory::Build(BuildTarget::Vmgstool)
);
impl_build_info!(
    artifacts::VMGSTOOL_WIN_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::Vmgstool)
);
impl_build_info!(
    artifacts::VMGSTOOL_LINUX_X64,
    ArtifactBuildCategory::Build(BuildTarget::Vmgstool)
);
impl_build_info!(
    artifacts::VMGSTOOL_LINUX_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::Vmgstool)
);
impl_build_info!(
    artifacts::VMGSTOOL_MACOS_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::Vmgstool)
);

// TPM guest tests
impl_build_info!(
    artifacts::guest_tools::TPM_GUEST_TESTS_WINDOWS_X64,
    ArtifactBuildCategory::Build(BuildTarget::TpmGuestTestsWindows)
);
impl_build_info!(
    artifacts::guest_tools::TPM_GUEST_TESTS_LINUX_X64,
    ArtifactBuildCategory::Build(BuildTarget::TpmGuestTestsLinux)
);

// Host tools
impl_build_info!(
    artifacts::host_tools::TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64,
    ArtifactBuildCategory::Build(BuildTarget::TestIgvmAgentRpcServer)
);

// Loadable firmware (always available from deps)
impl_build_info!(
    artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_X64,
    ArtifactBuildCategory::AlwaysAvailable
);
impl_build_info!(
    artifacts::loadable::LINUX_DIRECT_TEST_INITRD_X64,
    ArtifactBuildCategory::AlwaysAvailable
);
impl_build_info!(
    artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_AARCH64,
    ArtifactBuildCategory::AlwaysAvailable
);
impl_build_info!(
    artifacts::loadable::LINUX_DIRECT_TEST_INITRD_AARCH64,
    ArtifactBuildCategory::AlwaysAvailable
);
impl_build_info!(
    artifacts::loadable::PCAT_FIRMWARE_X64,
    ArtifactBuildCategory::AlwaysAvailable
);
impl_build_info!(
    artifacts::loadable::SVGA_FIRMWARE_X64,
    ArtifactBuildCategory::AlwaysAvailable
);
impl_build_info!(
    artifacts::loadable::UEFI_FIRMWARE_X64,
    ArtifactBuildCategory::AlwaysAvailable
);
impl_build_info!(
    artifacts::loadable::UEFI_FIRMWARE_AARCH64,
    ArtifactBuildCategory::AlwaysAvailable
);

// Petritools (always available)
impl_build_info!(
    artifacts::petritools::PETRITOOLS_EROFS_X64,
    ArtifactBuildCategory::AlwaysAvailable
);
impl_build_info!(
    artifacts::petritools::PETRITOOLS_EROFS_AARCH64,
    ArtifactBuildCategory::AlwaysAvailable
);

// Test VHDs (downloaded)
impl_build_info!(
    artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::Gen1WindowsDataCenterCore2022X64Vhd,
        also_build: &[BuildTarget::PipetteWindows],
    }
);
impl_build_info!(
    artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd,
        also_build: &[BuildTarget::PipetteWindows],
    }
);
impl_build_info!(
    artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::Gen2WindowsDataCenterCore2025X64Vhd,
        also_build: &[BuildTarget::PipetteWindows, BuildTarget::PrepSteps],
    }
);
impl_build_info!(
    artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64_PREPPED,
    ArtifactBuildCategory::Build(BuildTarget::PrepSteps)
);
impl_build_info!(
    artifacts::test_vhd::FREE_BSD_13_2_X64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::FreeBsd13_2X64Vhd,
        also_build: &[],
    }
);
impl_build_info!(
    artifacts::test_vhd::ALPINE_3_23_X64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::Alpine323X64Vhd,
        also_build: &[BuildTarget::PipetteLinux],
    }
);
impl_build_info!(
    artifacts::test_vhd::ALPINE_3_23_AARCH64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::Alpine323Aarch64Vhd,
        also_build: &[BuildTarget::PipetteLinux],
    }
);
impl_build_info!(
    artifacts::test_vhd::UBUNTU_2404_SERVER_X64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::Ubuntu2404ServerX64Vhd,
        also_build: &[BuildTarget::PipetteLinux],
    }
);
impl_build_info!(
    artifacts::test_vhd::UBUNTU_2504_SERVER_X64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::Ubuntu2504ServerX64Vhd,
        also_build: &[BuildTarget::PipetteLinux],
    }
);
impl_build_info!(
    artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::Ubuntu2404ServerAarch64Vhd,
        also_build: &[BuildTarget::PipetteLinux],
    }
);
impl_build_info!(
    artifacts::test_vhd::WINDOWS_11_ENTERPRISE_AARCH64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::Windows11EnterpriseAarch64Vhdx,
        also_build: &[BuildTarget::PipetteWindows],
    }
);

// Test ISOs (downloaded)
impl_build_info!(
    artifacts::test_iso::FREE_BSD_13_2_X64,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::FreeBsd13_2X64Iso,
        also_build: &[],
    }
);

// Test VMGS (downloaded)
impl_build_info!(
    artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::VmgsWithBootEntry,
        also_build: &[],
    }
);
impl_build_info!(
    artifacts::test_vmgs::VMGS_WITH_16K_TPM,
    ArtifactBuildCategory::Download {
        artifact: KnownTestArtifacts::VmgsWith16kTpm,
        also_build: &[],
    }
);

// Common artifacts
impl_build_info!(
    petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY,
    ArtifactBuildCategory::AlwaysAvailable
);
impl_build_info!(
    petri_artifacts_common::artifacts::PIPETTE_LINUX_X64,
    ArtifactBuildCategory::Build(BuildTarget::PipetteLinux)
);
impl_build_info!(
    petri_artifacts_common::artifacts::PIPETTE_LINUX_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::PipetteLinux)
);
impl_build_info!(
    petri_artifacts_common::artifacts::PIPETTE_WINDOWS_X64,
    ArtifactBuildCategory::Build(BuildTarget::PipetteWindows)
);
impl_build_info!(
    petri_artifacts_common::artifacts::PIPETTE_WINDOWS_AARCH64,
    ArtifactBuildCategory::Build(BuildTarget::PipetteWindows)
);

// --- Lookup table ---

/// Look up the build category for an artifact by its ID string.
///
/// The `artifact_id` should be in the format produced by
/// `ErasedArtifactHandle`'s `Debug` impl (with `__ty` suffix stripped).
/// The table stores raw `GLOBAL_UNIQUE_ID` values (with `__ty`), so the
/// comparison strips the suffix at lookup time.
///
/// The table is built from `ArtifactBuildInfo` trait consts — it is
/// compiler-verified and cannot get out of sync with the impls above.
pub fn lookup_build_category(artifact_id: &str) -> Option<ArtifactBuildCategory> {
    ARTIFACT_BUILD_TABLE
        .iter()
        .find(|(id, _)| {
            let stripped = id.strip_suffix("__ty").unwrap_or(id);
            stripped == artifact_id
        })
        .map(|(_, cat)| *cat)
}

const ARTIFACT_BUILD_TABLE: &[(&str, ArtifactBuildCategory)] = &[
    // OpenVMM
    build_table_entry!(artifacts::OPENVMM_WIN_X64),
    build_table_entry!(artifacts::OPENVMM_LINUX_X64),
    build_table_entry!(artifacts::OPENVMM_WIN_AARCH64),
    build_table_entry!(artifacts::OPENVMM_LINUX_AARCH64),
    build_table_entry!(artifacts::OPENVMM_MACOS_AARCH64),
    // OpenVMM vhost
    build_table_entry!(artifacts::OPENVMM_VHOST_LINUX_X64),
    build_table_entry!(artifacts::OPENVMM_VHOST_LINUX_AARCH64),
    // OpenHCL IGVM (built)
    build_table_entry!(artifacts::openhcl_igvm::LATEST_STANDARD_X64),
    build_table_entry!(artifacts::openhcl_igvm::LATEST_STANDARD_DEV_KERNEL_X64),
    build_table_entry!(artifacts::openhcl_igvm::LATEST_CVM_X64),
    build_table_entry!(artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64),
    build_table_entry!(artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64),
    build_table_entry!(artifacts::openhcl_igvm::LATEST_STANDARD_DEV_KERNEL_AARCH64),
    // OpenHCL usermode
    build_table_entry!(artifacts::openhcl_igvm::um_bin::LATEST_LINUX_DIRECT_TEST_X64),
    build_table_entry!(artifacts::openhcl_igvm::um_dbg::LATEST_LINUX_DIRECT_TEST_X64),
    // Release IGVM
    build_table_entry!(artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_X64),
    build_table_entry!(artifacts::openhcl_igvm::LATEST_RELEASE_LINUX_DIRECT_X64),
    build_table_entry!(artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_AARCH64),
    // Guest test UEFI
    build_table_entry!(artifacts::test_vhd::GUEST_TEST_UEFI_X64),
    build_table_entry!(artifacts::test_vhd::GUEST_TEST_UEFI_AARCH64),
    // TMKs
    build_table_entry!(artifacts::tmks::SIMPLE_TMK_X64),
    build_table_entry!(artifacts::tmks::SIMPLE_TMK_AARCH64),
    build_table_entry!(artifacts::tmks::TMK_VMM_WIN_X64),
    build_table_entry!(artifacts::tmks::TMK_VMM_WIN_AARCH64),
    build_table_entry!(artifacts::tmks::TMK_VMM_LINUX_X64),
    build_table_entry!(artifacts::tmks::TMK_VMM_LINUX_AARCH64),
    build_table_entry!(artifacts::tmks::TMK_VMM_LINUX_X64_MUSL),
    build_table_entry!(artifacts::tmks::TMK_VMM_LINUX_AARCH64_MUSL),
    build_table_entry!(artifacts::tmks::TMK_VMM_MACOS_AARCH64),
    // VMGSTool
    build_table_entry!(artifacts::VMGSTOOL_WIN_X64),
    build_table_entry!(artifacts::VMGSTOOL_WIN_AARCH64),
    build_table_entry!(artifacts::VMGSTOOL_LINUX_X64),
    build_table_entry!(artifacts::VMGSTOOL_LINUX_AARCH64),
    build_table_entry!(artifacts::VMGSTOOL_MACOS_AARCH64),
    // Guest tools
    build_table_entry!(artifacts::guest_tools::TPM_GUEST_TESTS_WINDOWS_X64),
    build_table_entry!(artifacts::guest_tools::TPM_GUEST_TESTS_LINUX_X64),
    // Host tools
    build_table_entry!(artifacts::host_tools::TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64),
    // Loadable firmware
    build_table_entry!(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_X64),
    build_table_entry!(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_X64),
    build_table_entry!(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_AARCH64),
    build_table_entry!(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_AARCH64),
    build_table_entry!(artifacts::loadable::PCAT_FIRMWARE_X64),
    build_table_entry!(artifacts::loadable::SVGA_FIRMWARE_X64),
    build_table_entry!(artifacts::loadable::UEFI_FIRMWARE_X64),
    build_table_entry!(artifacts::loadable::UEFI_FIRMWARE_AARCH64),
    // Petritools
    build_table_entry!(artifacts::petritools::PETRITOOLS_EROFS_X64),
    build_table_entry!(artifacts::petritools::PETRITOOLS_EROFS_AARCH64),
    // Test VHDs
    build_table_entry!(artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64),
    build_table_entry!(artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64),
    build_table_entry!(artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64),
    build_table_entry!(artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64_PREPPED),
    build_table_entry!(artifacts::test_vhd::FREE_BSD_13_2_X64),
    build_table_entry!(artifacts::test_vhd::ALPINE_3_23_X64),
    build_table_entry!(artifacts::test_vhd::ALPINE_3_23_AARCH64),
    build_table_entry!(artifacts::test_vhd::UBUNTU_2404_SERVER_X64),
    build_table_entry!(artifacts::test_vhd::UBUNTU_2504_SERVER_X64),
    build_table_entry!(artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64),
    build_table_entry!(artifacts::test_vhd::WINDOWS_11_ENTERPRISE_AARCH64),
    // Test ISOs
    build_table_entry!(artifacts::test_iso::FREE_BSD_13_2_X64),
    // Test VMGS
    build_table_entry!(artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY),
    build_table_entry!(artifacts::test_vmgs::VMGS_WITH_16K_TPM),
    // Common artifacts
    build_table_entry!(petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY),
    build_table_entry!(petri_artifacts_common::artifacts::PIPETTE_LINUX_X64),
    build_table_entry!(petri_artifacts_common::artifacts::PIPETTE_LINUX_AARCH64),
    build_table_entry!(petri_artifacts_common::artifacts::PIPETTE_WINDOWS_X64),
    build_table_entry!(petri_artifacts_common::artifacts::PIPETTE_WINDOWS_AARCH64),
];

/// The VHDs currently stored in Azure Blob Storage.
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[cfg_attr(feature = "clap", clap(rename_all = "verbatim"))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[expect(missing_docs)] // Self-describing names
pub enum KnownTestArtifacts {
    Alpine323X64Vhd,
    Alpine323Aarch64Vhd,
    Gen1WindowsDataCenterCore2022X64Vhd,
    Gen2WindowsDataCenterCore2022X64Vhd,
    Gen2WindowsDataCenterCore2025X64Vhd,
    FreeBsd13_2X64Vhd,
    FreeBsd13_2X64Iso,
    Ubuntu2404ServerX64Vhd,
    Ubuntu2504ServerX64Vhd,
    Ubuntu2404ServerAarch64Vhd,
    Windows11EnterpriseAarch64Vhdx,
    VmgsWithBootEntry,
    VmgsWith16kTpm,
}

struct KnownTestArtifactMeta {
    variant: KnownTestArtifacts,
    filename: &'static str,
    size: u64,
}

impl KnownTestArtifactMeta {
    const fn new(variant: KnownTestArtifacts, filename: &'static str, size: u64) -> Self {
        Self {
            variant,
            filename,
            size,
        }
    }
}

// linear scan to find entries is OK, given how few entries there are
const KNOWN_TEST_ARTIFACT_METADATA: &[KnownTestArtifactMeta] = &[
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Alpine323X64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Alpine323Aarch64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_AARCH64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_AARCH64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Gen1WindowsDataCenterCore2022X64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Gen2WindowsDataCenterCore2025X64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::FreeBsd13_2X64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::FREE_BSD_13_2_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::FREE_BSD_13_2_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::FreeBsd13_2X64Iso,
        petri_artifacts_vmm_test::artifacts::test_iso::FREE_BSD_13_2_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_iso::FREE_BSD_13_2_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Ubuntu2404ServerX64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_X64::SIZE,
    ),
        KnownTestArtifactMeta::new(
        KnownTestArtifacts::Ubuntu2504ServerX64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2504_SERVER_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2504_SERVER_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Ubuntu2404ServerAarch64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Windows11EnterpriseAarch64Vhdx,
        petri_artifacts_vmm_test::artifacts::test_vhd::WINDOWS_11_ENTERPRISE_AARCH64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::WINDOWS_11_ENTERPRISE_AARCH64::SIZE
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::VmgsWithBootEntry,
        petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::VmgsWith16kTpm,
        petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_16K_TPM::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_16K_TPM::SIZE,
    ),
];

impl KnownTestArtifacts {
    /// Get the name of the image.
    pub fn name(self) -> String {
        format!("{:?}", self)
    }

    /// Get the filename of the image.
    pub fn filename(self) -> &'static str {
        KNOWN_TEST_ARTIFACT_METADATA
            .iter()
            .find(|KnownTestArtifactMeta { variant, .. }| *variant == self)
            .unwrap()
            .filename
    }

    /// Get the image from its filename.
    pub fn from_filename(filename: &str) -> Option<Self> {
        Some(
            KNOWN_TEST_ARTIFACT_METADATA
                .iter()
                .find(|KnownTestArtifactMeta { filename: s, .. }| *s == filename)?
                .variant,
        )
    }

    /// Get the expected file size of the image.
    pub fn file_size(self) -> u64 {
        KNOWN_TEST_ARTIFACT_METADATA
            .iter()
            .find(|KnownTestArtifactMeta { variant, .. }| *variant == self)
            .unwrap()
            .size
    }
}
