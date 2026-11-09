// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `petri` test artifact declarations used by all petri-based tests, no matter
//! what VMM backend is being used.

#![deny(unsafe_code)]

/// Payload identities for the supported CCA host and guest kernel.
pub mod cca_payload {
    /// Matched Realm guest used by the TDISP AHCI test.
    pub mod tdisp_guest {
        /// Same kernel as the CCA host.
        pub const LINUX_IMAGE_SHA256: &str = super::LINUX_IMAGE_SHA256;
        /// Guest initializer with the direct-I/O/hash/unlock oracle.
        pub const INITRD_SHA256: &str =
            "b9dc5046ac739a90cf23c1c52073cf6cdedcbabcfa18481b73faa0cdec9f5e92";
    }
    /// Frozen kernel source.
    pub const LINUX_REVISION: &str = "cb45796cb3a9483202dce8c24735fc135bb9b675";
    /// Kernel release label is provenance, not an ABI selector.
    pub const LINUX_RELEASE: &str = "7.4.0-rc1-cca74-cb45796";
    /// Resolved kernel configuration.
    pub const LINUX_CONFIG_SHA256: &str =
        "bbd5e7ac13314c3b9779cf27a62f7b085c3ff0f3f4fb92ffc454e798fb0cb5b5";
    /// Release supplying the immutable standard userspace.
    pub const OPENVMM_DEPS_RELEASE: &str = "0.3.0-139";
    /// SHA-256 of the published AArch64 test-initrd archive.
    pub const INITRD_ARCHIVE_SHA256: &str =
        "132b4b4e66b032ee108cd06fc17aee5660b930aade9a3dace4525709a5fbf344";
    /// SHA-256 of the supported unified host and guest Image.
    pub const LINUX_IMAGE_SHA256: &str =
        "af5da17c7f5642865d18da0baa6b5bc36685fd7acf82b19d3064b3ce2452cb1b";
    /// SHA-256 of the extracted base initrd, before per-run init injection.
    pub const BASE_INITRD_SHA256: &str =
        "74ecad46de9da08aff7520a93a09fa473276360bcb018e106ae41862a6f2feb2";
}

/// Runtime capabilities that VMM tests can require.
pub mod capabilities {
    /// Software VPCI device emulation support.
    pub const VPCI: &str = "vpci";
    /// Arm Confidential Compute Architecture support.
    pub const CCA: &str = "cca";
    /// CCA host supports the qualified in-place guest_memfd memory mode.
    pub const GUEST_MEMFD_IN_PLACE: &str = "guest_memfd_in_place";
    /// Pinned AHCI cdev connected to the host TSM for Realm object tests.
    /// This does not qualify guest DMA or TDISP LOCK/RUN.
    pub const CCA_REALM_VFIO: &str = "cca_realm_vfio";

    /// Support for resetting a partition running Windows.
    pub const WINDOWS_PARTITION_RESET: &str = "windows_partition_reset";

    /// Support for nested virtualization
    pub const NESTED_VIRT: &str = "nested_virt";

    /// All capability names known to petri, including those defined by
    /// tests.
    pub const KNOWN_CAPABILITIES: &[&str] = &[
        VPCI,
        CCA,
        GUEST_MEMFD_IN_PLACE,
        CCA_REALM_VFIO,
        WINDOWS_PARTITION_RESET,
        NESTED_VIRT,
        "test_disk",
        "edu_initiator",
        "ivshmem_target",
    ];

    /// Returns `name` if it is a known capability name.
    pub fn known(name: &str) -> Option<&'static str> {
        KNOWN_CAPABILITIES
            .iter()
            .copied()
            .find(|capability| *capability == name)
    }

    /// Returns whether `name` is a known capability name.
    pub fn is_known_name(name: &str) -> bool {
        known(name).is_some()
    }
}

/// Artifact declarations
pub mod artifacts {
    use petri_artifacts_core::declare_artifacts;

    declare_artifacts! {
        /// Pipette windows x86_64 executable
        PIPETTE_WINDOWS_X64("pipette.exe", WINDOWS_X64),
        /// Pipette linux x86_64 musl executable
        PIPETTE_LINUX_X64_MUSL("pipette", LINUX_X64_MUSL),
        /// Pipette windows aarch64 executable
        PIPETTE_WINDOWS_AARCH64("pipette.exe", WINDOWS_AARCH64),
        /// Pipette linux aarch64 musl executable
        PIPETTE_LINUX_AARCH64_MUSL("pipette", LINUX_AARCH64_MUSL),
        /// Pinned FVP CCA firmware package directory.
        FVP_CCA_PLATFORM_AARCH64("fvp-cca", AARCH64),
        /// Qualified FVP host and unchanged TDISP guest payload directory.
        FVP_CCA_PAYLOAD_AARCH64("fvp-cca-payload", AARCH64),
        /// Directory to put petri test logs in
        TEST_LOG_DIRECTORY("test_results", ANY),
    }
}

/// Artifact tag trait declarations
pub mod tags {
    use petri_artifacts_core::ArtifactId;

    /// A coarse-grained label used to differentiate between different OS
    /// environments.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[expect(missing_docs)] // Self-describing names.
    pub enum OsFlavor {
        Windows,
        Linux,
        FreeBsd,
        Uefi,
    }

    /// The machine architecture supported by the artifact or VM.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    #[expect(missing_docs)] // Self describing names
    pub enum MachineArch {
        X86_64,
        Aarch64,
    }

    impl MachineArch {
        /// Returns the host's architecture.
        pub fn host() -> Self {
            // xtask-fmt allow-target-arch oneoff-petri-host-arch
            if cfg!(target_arch = "x86_64") {
                Self::X86_64
            }
            // xtask-fmt allow-target-arch oneoff-petri-host-arch
            else if cfg!(target_arch = "aarch64") {
                Self::Aarch64
            } else {
                panic!("unsupported host architecture")
            }
        }
    }

    /// Quirks needed to boot a guest.
    #[derive(Default, Clone, Debug)]
    pub struct GuestQuirksInner {
        /// How long to wait after the shutdown IC reports ready before sending
        /// the shutdown command.
        ///
        /// This is necessary because some guests will ignore shutdown requests
        /// that arrive too early in the boot process.
        pub hyperv_shutdown_ic_sleep: Option<std::time::Duration>,
        /// Some guests reboot automatically soon after first boot.
        pub initial_reboot: Option<InitialRebootCondition>,
    }

    /// Some guests may automatically reboot only in certain configurations
    #[derive(Clone, Copy, Debug)]
    pub enum InitialRebootCondition {
        /// This guest always reboots on this VMM.
        Always,
        /// This guest only reboots when the TPM is enabled.
        WithTpm,
    }

    /// Quirks needed to boot a guest, allowing for differences based on backend
    #[derive(Default, Clone, Debug)]
    pub struct GuestQuirks {
        /// Quirks when running in OpenVMM
        pub openvmm: GuestQuirksInner,
        /// Quirks when running in Hyper-V
        pub hyperv: GuestQuirksInner,
    }

    impl GuestQuirks {
        /// Use the same quirks for all backends
        pub fn for_all_backends(quirks: GuestQuirksInner) -> GuestQuirks {
            GuestQuirks {
                openvmm: quirks.clone(),
                hyperv: quirks,
            }
        }
    }

    /// Artifact is a OpenHCL IGVM file
    pub trait IsOpenhclIgvm: IsLoadable + ArtifactId {}

    /// Artifact is a bootable test VHD file
    pub trait IsTestVhd: ArtifactId {
        /// What [`OsFlavor`] this image boots into.
        const OS_FLAVOR: OsFlavor;

        /// What [`MachineArch`] this image supports.
        const ARCH: MachineArch;

        /// Declare any "quirks" needed to boot the image.
        fn quirks() -> GuestQuirks {
            GuestQuirks::default()
        }
    }

    /// Artifact is a bootable test ISO file
    pub trait IsTestIso: ArtifactId {
        /// What [`OsFlavor`] this image boots into.
        const OS_FLAVOR: OsFlavor;

        /// What [`MachineArch`] this image supports.
        const ARCH: MachineArch;

        /// Declare any "quirks" needed to boot the image.
        fn quirks() -> GuestQuirks {
            GuestQuirks::default()
        }
    }

    /// Artifact is a binary that can be loaded into a VM
    pub trait IsLoadable: ArtifactId {
        /// What [`MachineArch`] this artifact supports.
        const ARCH: MachineArch;
    }

    /// Artifact is a test VMGS file
    pub trait IsTestVmgs: ArtifactId {}

    /// Artifact is a VmgsTool binary
    pub trait IsVmgsTool: ArtifactId {}
}
