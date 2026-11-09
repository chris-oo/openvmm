// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Incubator profile definitions.

use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

pub use petri::fvp::FvpCcaConfig;
pub use petri::fvp::FvpConsole;
pub use petri::fvp::FvpDeadlines;
pub use petri::fvp::FvpPlatform;

/// An incubator profile describing the backend platform and how to run it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncubatorProfile {
    /// Incubator backend configuration.
    pub incubator: IncubatorBackend,
    /// Guest boot environment, independent of the VM launcher.
    #[serde(default)]
    pub boot: IncubatorBoot,
    /// Extra devices to add to the platform.
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
}

/// Backend-specific configuration, tagged by `type`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum IncubatorBackend {
    /// QEMU TCG emulation.
    QemuTcg(QemuTcgConfig),
    /// Native OpenVMM launcher.
    Openvmm(OpenvmmConfig),
    /// QEMU Arm CCA emulation.
    QemuCca(QemuCcaConfig),
    /// Licensed Arm FVP CCA platform.
    FvpCca(FvpCcaConfig),
}

/// Guest boot environment.
#[derive(Debug, Default, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum IncubatorBoot {
    /// Boot a Linux kernel and prepared initrd directly.
    #[default]
    LinuxDirect,
    /// Launch the Microsoft hypervisor and its Linux root partition via UEFI.
    Mshv(MshvBootConfig),
}

/// Artifacts for mshv boot, independent of the launcher executable.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct MshvBootConfig {
    /// Development-only x64 UEFI host boot application executable.
    pub host_boot: std::path::PathBuf,
    /// Compatible x64 UEFI firmware.
    pub firmware: std::path::PathBuf,
    /// Extracted artifact directory containing `boot/`.
    pub boot_artifacts: std::path::PathBuf,
    /// Linux options, before Incubator's required initrd entry point.
    pub kernel_cmdline: String,
    /// Hypervisor options, excluding the loader DLL path.
    pub hypervisor_cmdline: String,
}

impl IncubatorBackend {
    /// The guest architecture this backend emulates.
    pub fn arch(&self) -> Arch {
        match self {
            IncubatorBackend::QemuTcg(config) => config.arch,
            IncubatorBackend::Openvmm(config) => config.arch,
            IncubatorBackend::QemuCca(_) | IncubatorBackend::FvpCca(_) => Arch::Aarch64,
        }
    }
}

/// Native OpenVMM launcher and machine configuration.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OpenvmmConfig {
    /// Guest architecture (must match the native host).
    pub arch: Arch,
    /// Native host OpenVMM executable, independent of the guest test build.
    pub binary: std::path::PathBuf,
    /// Outer VM memory size.
    pub memory: String,
    /// Outer VM processor count.
    pub processors: u32,
    /// Override SMT exposure in the outer VM's processor topology.
    #[serde(default)]
    pub enable_smt: Option<bool>,
    /// Expose nested virtualization. Required for mshv boot.
    #[serde(default)]
    pub nested_virt: bool,
}

/// Guest architecture emulated by an incubator backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Arch {
    /// x86-64.
    X86_64,
    /// AArch64.
    Aarch64,
}

impl Arch {
    /// The prefix used for arch-specific environment variables, matching
    /// openvmm's convention (e.g., `X86_64_OPENVMM_LINUX_DIRECT_KERNEL`).
    pub fn env_prefix(self) -> &'static str {
        match self {
            Arch::X86_64 => "X86_64",
            Arch::Aarch64 => "AARCH64",
        }
    }
}

/// A device to add to the platform.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DeviceConfig {
    /// A virtio-blk disk device.
    VirtioBlk(VirtioBlkDeviceConfig),
    /// A QEMU `edu` device — a simple PCI device with a register-programmed
    /// DMA engine. Used as a P2P DMA *initiator* in device-assignment tests.
    Edu(EduDeviceConfig),
    /// A QEMU `ivshmem-plain` device — a PCI device whose BAR2 is a
    /// prefetchable, RAM-backed memory window. Used as a P2P DMA *target*
    /// (peer BAR) in device-assignment tests.
    IvshmemPlain(IvshmemPlainDeviceConfig),
}

impl DeviceConfig {
    /// The device's name, used in env var names (e.g. `test-disk` →
    /// `INCUBATOR_VFIO_BDF_TEST_DISK`).
    pub fn name(&self) -> &str {
        match self {
            DeviceConfig::VirtioBlk(cfg) => &cfg.name,
            DeviceConfig::Edu(cfg) => &cfg.name,
            DeviceConfig::IvshmemPlain(cfg) => &cfg.name,
        }
    }

    /// Whether the device should be bound to vfio-pci after boot so it can be
    /// assigned into the L2 guest.
    pub fn vfio(&self) -> bool {
        match self {
            DeviceConfig::VirtioBlk(cfg) => cfg.vfio,
            DeviceConfig::Edu(cfg) => cfg.vfio,
            DeviceConfig::IvshmemPlain(cfg) => cfg.vfio,
        }
    }

    /// The capability this device advertises once provisioned, derived from
    /// its name with `-` replaced by `_` so it is a valid `requires(...)`
    /// identifier (e.g. `edu-initiator` → `edu_initiator`). Tests gate on this
    /// via `requires(...)`.
    pub fn capability(&self) -> String {
        self.name().replace('-', "_")
    }
}

/// Configuration for a virtio-blk device added to the incubator.
#[derive(Debug, Deserialize)]
pub struct VirtioBlkDeviceConfig {
    /// Name for this device (used in env var names, e.g., "test-disk" →
    /// `INCUBATOR_VFIO_BDF_TEST_DISK`).
    pub name: String,
    /// Size of the RAM-backed disk (e.g., "64M").
    pub size: String,
    /// If true, bind the device to vfio-pci after boot, making it available
    /// for passthrough into the L2 guest.
    #[serde(default)]
    pub vfio: bool,
}

/// Configuration for a QEMU `edu` device added to the incubator.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct EduDeviceConfig {
    /// Name for this device (used in env var names, e.g., "edu-initiator" →
    /// `INCUBATOR_VFIO_BDF_EDU_INITIATOR`).
    pub name: String,
    /// Optional `dma_mask` for the edu DMA engine (e.g. "0xffffffffffff").
    /// The edu default is 28 bits, which clamps DMA addresses to the low
    /// 256 MiB — too small for aarch64 guest physical addresses, so P2P tests
    /// must widen it. Accepts decimal or `0x`-prefixed hex.
    #[serde(default)]
    pub dma_mask: Option<String>,
    /// If true, bind the device to vfio-pci after boot, making it available
    /// for passthrough into the L2 guest.
    #[serde(default)]
    pub vfio: bool,
}

/// Configuration for a QEMU `ivshmem-plain` device added to the incubator.
#[derive(Debug, Deserialize)]
pub struct IvshmemPlainDeviceConfig {
    /// Name for this device (used in env var names, e.g., "ivshmem-target" →
    /// `INCUBATOR_VFIO_BDF_IVSHMEM_TARGET`).
    pub name: String,
    /// Size of the RAM-backed shared-memory BAR2 (e.g., "4M").
    pub size: String,
    /// If true, bind the device to vfio-pci after boot, making it available
    /// for passthrough into the L2 guest.
    #[serde(default)]
    pub vfio: bool,
}

/// QEMU TCG configuration parsed from the profile.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QemuTcgConfig {
    /// Guest architecture (e.g., "aarch64", "x86-64"). Selects the
    /// arch-specific kernel/initrd when those are auto-detected.
    pub arch: Arch,
    /// Path or name of the QEMU binary (e.g., "qemu-system-aarch64").
    pub binary: String,
    /// Machine type (e.g., "virt,virtualization=on,iommu=smmuv3").
    pub machine: String,
    /// CPU model (e.g., "max").
    pub cpu: String,
    /// Memory size (e.g., "4G").
    pub memory: String,
    /// Number of CPUs (e.g., "2").
    pub smp: String,
    /// Extra kernel command line arguments. The incubator always appends
    /// `rdinit=/incubator-init.sh` (the injected init script); everything else,
    /// including the arch-specific serial console (e.g., "console=ttyAMA0"
    /// for aarch64 PL011, "console=ttyS0" for x86 16550), comes from here.
    pub cmdline: String,
}

/// QEMU Arm CCA configuration parsed from the profile.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct QemuCcaConfig {
    /// Select the pinned in-place guest_memfd kernel without changing firmware.
    #[serde(default)]
    pub guest_memfd_in_place: bool,
    /// Path or name of the QEMU binary.
    pub binary: String,
    /// Machine configuration.
    pub machine: String,
    /// CPU model and features.
    pub cpu: String,
    /// Memory size.
    pub memory: String,
    /// Number of CPUs.
    pub smp: String,
    /// Optional physical address where QEMU also loads the raw kernel image.
    ///
    /// Linux-direct TF-A uses this as its fixed BL33 entry point while
    /// `-kernel` remains present so QEMU generates the DTB and command line.
    pub kernel_load_address: Option<u64>,
    /// Physical address where QEMU loads the raw initrd.
    pub initrd_load_address: Option<u64>,
    /// Ordered serial console names. Each entry maps to one QEMU `-serial`
    /// device in the same order.
    pub consoles: Vec<String>,
    /// Console monitored for pipette readiness.
    pub primary_console: String,
    /// Capabilities published after the CCA host reaches pipette readiness.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Additional QEMU arguments for narrowly-scoped platform overrides.
    #[serde(default)]
    pub extra_args: Vec<QemuCcaExtraArg>,
}

/// A typed additional QEMU CCA argument.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum QemuCcaExtraArg {
    /// Enable a QEMU trace category through `-d`.
    Trace {
        /// QEMU trace category expression.
        value: String,
    },
    /// Set a QEMU global device property through `-global`.
    Global {
        /// QEMU global property expression.
        value: String,
    },
}

impl IncubatorProfile {
    /// Reject unsupported launcher/boot combinations before reading artifacts.
    pub fn validate(&self) -> anyhow::Result<()> {
        match (&self.incubator, &self.boot) {
            (IncubatorBackend::QemuTcg(_), IncubatorBoot::Mshv(_)) => {
                anyhow::bail!("mshv boot is not yet supported by the qemu-tcg launcher");
            }
            (IncubatorBackend::Openvmm(config), IncubatorBoot::Mshv(_)) => {
                anyhow::ensure!(config.arch == Arch::X86_64, "mshv boot requires x86-64");
                anyhow::ensure!(config.nested_virt, "mshv boot requires nested-virt = true");
            }
            (
                IncubatorBackend::QemuCca(_) | IncubatorBackend::FvpCca(_),
                IncubatorBoot::Mshv(_),
            ) => {
                anyhow::bail!("CCA launchers do not support mshv boot");
            }
            (_, IncubatorBoot::LinuxDirect) => {}
        }
        match &self.incubator {
            IncubatorBackend::QemuTcg(config) => {
                anyhow::ensure!(
                    config.arch == Arch::Aarch64,
                    "Petri QEMU currently supports only AArch64 guests"
                );
            }
            IncubatorBackend::Openvmm(config) => {
                anyhow::ensure!(
                    config.arch == Arch::X86_64,
                    "OpenVMM Incubator currently supports only x86-64 guests"
                );
                anyhow::ensure!(
                    config.processors > 0,
                    "OpenVMM requires at least one processor"
                );
                anyhow::ensure!(
                    parse_size(&config.memory)? > 0,
                    "OpenVMM memory must be nonzero"
                );
                anyhow::ensure!(
                    self.devices.is_empty(),
                    "extra profile devices are currently supported only by qemu-tcg"
                );
            }
            IncubatorBackend::FvpCca(config) => {
                config.validate()?;
                anyhow::ensure!(
                    self.devices.is_empty(),
                    "FVP CCA profiles do not support extra devices"
                );
            }
            IncubatorBackend::QemuCca(config) => {
                validate_qemu_cca(config)?;
                anyhow::ensure!(
                    self.devices.is_empty(),
                    "QEMU CCA profiles do not yet support extra devices"
                );
            }
        }
        Ok(())
    }

    /// Load a profile from a TOML file.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let contents = fs_err::read_to_string(path).context("failed to read incubator profile")?;
        Self::from_toml(&contents)
    }

    /// Parse a profile from a TOML string.
    pub fn from_toml(toml: &str) -> anyhow::Result<Self> {
        let profile: Self =
            toml_edit::de::from_str(toml).context("failed to parse incubator profile")?;
        profile.validate()?;
        Ok(profile)
    }
}

fn validate_qemu_cca(config: &QemuCcaConfig) -> anyhow::Result<()> {
    for (name, value) in [
        ("binary", &config.binary),
        ("machine", &config.machine),
        ("cpu", &config.cpu),
        ("memory", &config.memory),
        ("smp", &config.smp),
        ("primary-console", &config.primary_console),
    ] {
        anyhow::ensure!(!value.is_empty(), "QEMU CCA {name} must not be empty");
    }
    if let Some(address) = config.kernel_load_address {
        anyhow::ensure!(
            address & 0xfff == 0,
            "QEMU CCA kernel load address must be page aligned"
        );
    }
    if let Some(address) = config.initrd_load_address {
        anyhow::ensure!(
            address & 0xfff == 0,
            "QEMU CCA initrd load address must be page aligned"
        );
    }
    anyhow::ensure!(
        config.binary != "."
            && config.binary != ".."
            && config.binary.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+')
            }),
        "QEMU CCA binary must be a bare executable name"
    );
    anyhow::ensure!(
        !config.consoles.is_empty(),
        "QEMU CCA must configure at least one console"
    );

    let mut consoles = BTreeSet::new();
    for console in &config.consoles {
        anyhow::ensure!(
            !console.is_empty()
                && console
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "invalid QEMU CCA console name: {console}"
        );
        anyhow::ensure!(
            consoles.insert(console),
            "duplicate QEMU CCA console name: {console}"
        );
    }
    anyhow::ensure!(
        consoles.contains(&config.primary_console),
        "QEMU CCA primary console {} is not present in the console list",
        config.primary_console
    );
    anyhow::ensure!(
        config.consoles.first() == Some(&config.primary_console),
        "QEMU CCA primary console must be the first serial console"
    );

    let mut capabilities = BTreeSet::new();
    for capability in &config.capabilities {
        anyhow::ensure!(
            capability != "guest_memfd_in_place" || config.guest_memfd_in_place,
            "QEMU in-place capability requires guest-memfd-in-place = true"
        );
        anyhow::ensure!(
            petri_artifacts_common::capabilities::is_known_name(capability),
            "unknown QEMU CCA capability: {capability}"
        );
        anyhow::ensure!(
            capabilities.insert(capability),
            "duplicate QEMU CCA capability: {capability}"
        );
    }

    for extra_arg in &config.extra_args {
        let value = match extra_arg {
            QemuCcaExtraArg::Trace { value } | QemuCcaExtraArg::Global { value } => value,
        };
        anyhow::ensure!(
            !value.is_empty(),
            "QEMU CCA extra argument must not be empty"
        );
        anyhow::ensure!(
            !value.contains('\0') && !value.contains('\n'),
            "QEMU CCA extra argument contains an invalid character"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn parses_fvp_profile_with_declared_capabilities() {
        let profile =
            IncubatorProfile::from_toml(include_str!("../profiles/aarch64-fvp-cca.toml")).unwrap();
        assert_eq!(profile.incubator.arch(), Arch::Aarch64);
        let IncubatorBackend::FvpCca(config) = profile.incubator else {
            panic!("expected FVP profile");
        };
        assert_eq!(config.capabilities, ["cca", "guest_memfd_in_place"]);
        assert_eq!(config.platform, FvpPlatform::GuestMemfdInPlace);
        assert_eq!(config.primary_console, FvpConsole::Host);
        assert_eq!(config.deadlines.model_start, 120);
        assert_eq!(config.deadlines.validation, 300);
        assert_eq!(config.deadlines.dhcp, 30);
        assert_eq!(config.port_retries, 20);
    }

    #[test]
    fn in_place_fvp_capability_requires_its_platform() {
        let text = include_str!("../profiles/aarch64-fvp-cca-guest-memfd-in-place.toml");
        let profile = IncubatorProfile::from_toml(text).unwrap();
        let IncubatorBackend::FvpCca(config) = profile.incubator else {
            panic!("expected FVP profile");
        };
        assert_eq!(config.platform, FvpPlatform::GuestMemfdInPlace);
        assert_eq!(config.capabilities, ["cca", "guest_memfd_in_place"]);
        assert!(
            IncubatorProfile::from_toml(&text.replace(
                "platform = \"guest-memfd-in-place\"",
                "platform = \"cca-v15\""
            ))
            .is_err()
        );
        assert!(
            IncubatorProfile::from_toml(&text.replace(
                "platform = \"guest-memfd-in-place\"",
                "platform = \"arbitrary\""
            ))
            .is_err()
        );
    }

    #[test]
    fn rejects_invalid_fvp_profiles() {
        for fields in [
            "consoles = []\nprimary-console = 'host'",
            "consoles = ['host', 'host']\nprimary-console = 'host'",
            "consoles = ['host', 'unknown']\nprimary-console = 'host'",
            "consoles = ['edk2']\nprimary-console = 'host'",
            "consoles = ['host', 'edk2']\nprimary-console = 'edk2'",
            "consoles = ['host']\nprimary-console = 'host'\ncapabilities = ['vpci']",
            "consoles = ['host']\nprimary-console = 'host'\ncapabilities = ['cca', 'cca']",
            "consoles = ['host']\nprimary-console = 'host'\nrootfs = 'disk.img'",
            "consoles = ['host']\nprimary-console = 'host'\nport-retries = 0",
            "consoles = ['host']\nprimary-console = 'host'\nport-retries = 51",
        ] {
            assert!(
                IncubatorProfile::from_toml(&format!("[incubator]\ntype='fvp-cca'\n{fields}"))
                    .is_err(),
                "{fields}"
            );
        }
    }

    #[test]
    fn rejects_fvp_top_level_rootfs() {
        let profile = "rootfs = 'disk.img'\n\
                       [incubator]\ntype = 'fvp-cca'\n\
                       consoles = ['host']\nprimary-console = 'host'\n";
        assert!(
            IncubatorProfile::from_toml(profile)
                .unwrap_err()
                .chain()
                .any(|error| error.to_string().contains("unknown field `rootfs`"))
        );
    }

    #[test]
    fn fvp_deadline_boundaries() {
        for (name, minimum, maximum) in [
            ("validation", 30u64, 1800u64),
            ("model-start", 30u64, 300u64),
            ("pipette-ready", 120, 1800),
            ("test-execution", 300, 7200),
            ("guest-shutdown", 30, 300),
            ("forced-cleanup", 10, 120),
            ("model-lock", 0, 7200),
            ("port-allocation", 10, 300),
            ("dhcp", 10, 120),
        ] {
            for value in [minimum, maximum] {
                let deadlines: FvpDeadlines =
                    toml_edit::de::from_str(&format!("{name}={value}")).unwrap();
                deadlines.validate().unwrap();
            }
            for value in [minimum.checked_sub(1), Some(maximum + 1)]
                .into_iter()
                .flatten()
            {
                let deadlines: FvpDeadlines =
                    toml_edit::de::from_str(&format!("{name}={value}")).unwrap();
                assert!(deadlines.validate().is_err(), "{name}={value}");
            }
        }
    }

    #[test]
    fn parses_qemu_tcg_profile() {
        let profile =
            IncubatorProfile::from_toml(include_str!("../profiles/aarch64-tcg-pcie.toml")).unwrap();

        assert_eq!(profile.incubator.arch(), Arch::Aarch64);
        assert!(matches!(profile.incubator, IncubatorBackend::QemuTcg(_)));
    }

    #[test]
    fn parses_qemu_cca_profile() {
        let profile =
            IncubatorProfile::from_toml(include_str!("../profiles/aarch64-qemu-cca.toml")).unwrap();

        let IncubatorBackend::QemuCca(config) = profile.incubator else {
            panic!("expected QEMU CCA profile");
        };
        assert_eq!(config.primary_console, "host");
        assert_eq!(config.consoles, ["host", "secure"]);
        assert_eq!(config.capabilities, ["cca"]);
    }

    #[test]
    fn qemu_in_place_profile_requires_explicit_payload_selection() {
        let text = include_str!("../profiles/aarch64-qemu-cca-guest-memfd-in-place.toml");
        let profile = IncubatorProfile::from_toml(text).unwrap();
        let IncubatorBackend::QemuCca(config) = profile.incubator else {
            panic!("expected QEMU CCA profile");
        };
        assert!(config.guest_memfd_in_place);
        assert_eq!(config.capabilities, ["cca", "guest_memfd_in_place"]);
        assert!(
            IncubatorProfile::from_toml(&text.replace(
                "guest-memfd-in-place = true",
                "guest-memfd-in-place = false"
            ))
            .unwrap_err()
            .to_string()
            .contains("in-place capability")
        );
        assert!(
            IncubatorProfile::from_toml(&text.replace(
                "guest-memfd-in-place = true",
                "guest-memfd-in-place = 'true'"
            ))
            .is_err()
        );
    }

    #[test]
    fn rejects_missing_qemu_cca_primary_console() {
        let error = IncubatorProfile::from_toml(
            r#"
[incubator]
type = "qemu-cca"
binary = "qemu-system-aarch64"
machine = "virt"
cpu = "max,x-rme=on"
memory = "2G"
smp = "1"
consoles = ["host"]
primary-console = "missing"
"#,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("primary console missing is not present")
        );
    }

    #[test]
    fn rejects_machine_local_qemu_cca_binary() {
        for binary in ["/tmp/qemu-system-aarch64", r"C:\qemu\qemu.exe"] {
            let error = IncubatorProfile::from_toml(&format!(
                r#"
[incubator]
type = "qemu-cca"
binary = {binary:?}
machine = "virt"
cpu = "max,x-rme=on"
memory = "2G"
smp = "1"
consoles = ["host"]
primary-console = "host"
"#
            ))
            .unwrap_err();

            assert!(error.to_string().contains("bare executable name"));
        }
    }

    #[test]
    fn rejects_unknown_qemu_cca_capability() {
        let error = IncubatorProfile::from_toml(
            r#"
[incubator]
type = "qemu-cca"
binary = "qemu-system-aarch64"
machine = "virt"
cpu = "max,x-rme=on"
memory = "2G"
smp = "1"
consoles = ["host"]
primary-console = "host"
capabilities = ["unknown"]
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown QEMU CCA capability"));
    }
    #[test]
    fn realm_vfio_capability_requires_its_fixture() {
        let text = include_str!("../profiles/aarch64-fvp-cca-realm-vfio.toml");
        let profile = IncubatorProfile::from_toml(text).unwrap();
        let IncubatorBackend::FvpCca(config) = profile.incubator else {
            panic!("not FVP")
        };
        assert_eq!(config.platform, FvpPlatform::RealmVfio);
        assert_eq!(
            config.capabilities,
            ["cca", "guest_memfd_in_place", "cca_realm_vfio"]
        );
        for platform in ["cca-v15", "guest-memfd-in-place"] {
            assert!(
                IncubatorProfile::from_toml(&text.replace(
                    "platform = \"realm-vfio\"",
                    &format!("platform = \"{platform}\"")
                ))
                .is_err()
            );
        }
        for capability in ["vpci", "tdisp", "dma"] {
            assert!(
                IncubatorProfile::from_toml(&text.replace("cca_realm_vfio", capability)).is_err()
            );
        }
    }

    #[test]
    fn cca_profiles_accept_linux_direct_but_reject_mshv_and_extra_devices() {
        for text in [
            include_str!("../profiles/aarch64-fvp-cca.toml"),
            include_str!("../profiles/aarch64-qemu-cca.toml"),
        ] {
            let direct = format!("{text}\n[boot]\ntype = 'linux-direct'\n");
            IncubatorProfile::from_toml(&direct).unwrap();
            let mshv = format!(
                "{text}\n[boot]\ntype = 'mshv'\nhost-boot = 'app.efi'\n\
                 firmware = 'fw'\nboot-artifacts = 'boot'\nkernel-cmdline = ''\n\
                 hypervisor-cmdline = ''\n"
            );
            assert!(
                IncubatorProfile::from_toml(&mshv)
                    .unwrap_err()
                    .to_string()
                    .contains("do not support mshv boot")
            );
            let devices =
                format!("{text}\n[[devices]]\ntype = 'virtio-blk'\nname = 'disk'\nsize = '64M'\n");
            assert!(
                IncubatorProfile::from_toml(&devices)
                    .unwrap_err()
                    .to_string()
                    .contains("extra devices")
            );
        }
    }
}

/// Parse memory/device sizes using the OpenVMM suffix convention.
pub(crate) fn parse_size(s: &str) -> anyhow::Result<u64> {
    || -> Option<u64> {
        let mut b = s.as_bytes();
        if s.ends_with('B') {
            b = &b[..b.len() - 1]
        }
        if b.is_empty() {
            return None;
        }
        let multi = match b[b.len() - 1] as char {
            'T' => Some(1024 * 1024 * 1024 * 1024),
            'G' => Some(1024 * 1024 * 1024),
            'M' => Some(1024 * 1024),
            'K' => Some(1024),
            _ => None,
        };
        if multi.is_some() {
            b = &b[..b.len() - 1]
        }
        let n: u64 = std::str::from_utf8(b).ok()?.parse().ok()?;
        n.checked_mul(multi.unwrap_or(1))
    }()
    .with_context(|| format!("invalid size '{s}'"))
}

#[cfg(test)]
mod launcher_tests {
    use super::*;
    use test_with_tracing::test;

    const OPENVMM: &str = include_str!("../profiles/x86_64-openvmm.toml");
    const MSHV: &str = include_str!("../profiles/x86_64-mshv.toml");
    const QEMU: &str = include_str!("../profiles/aarch64-tcg-pcie.toml");

    #[test]
    fn launcher_and_boot_are_separate() -> anyhow::Result<()> {
        for profile in [OPENVMM, MSHV, QEMU] {
            IncubatorProfile::from_toml(profile)?.validate()?;
        }
        let direct = IncubatorProfile::from_toml(OPENVMM)?;
        assert!(matches!(direct.incubator, IncubatorBackend::Openvmm(_)));
        assert!(matches!(direct.boot, IncubatorBoot::LinuxDirect));
        let mshv = IncubatorProfile::from_toml(MSHV)?;
        assert!(matches!(mshv.incubator, IncubatorBackend::Openvmm(_)));
        assert!(matches!(mshv.boot, IncubatorBoot::Mshv(_)));
        let qemu = IncubatorProfile::from_toml(QEMU)?;
        assert!(matches!(qemu.boot, IncubatorBoot::LinuxDirect));
        Ok(())
    }

    #[test]
    fn mshv_command_lines_are_required() {
        for field in ["kernel-cmdline", "hypervisor-cmdline"] {
            let profile = MSHV
                .lines()
                .filter(|line| !line.starts_with(field))
                .collect::<Vec<_>>()
                .join("\n");
            let error = IncubatorProfile::from_toml(&profile).unwrap_err();
            assert!(
                format!("{error:#}").contains(&format!("missing field `{field}`")),
                "{error:#}"
            );
        }
    }

    #[test]
    fn unsupported_combinations_and_misplaced_fields_fail() -> anyhow::Result<()> {
        let qemu_mshv = format!(
            "{QEMU}\n[boot]\ntype = 'mshv'\nhost-boot = 'app.efi'\nfirmware = 'fw'\nboot-artifacts = 'boot'\nkernel-cmdline = 'test-kernel'\nhypervisor-cmdline = 'test-hypervisor'\n"
        );
        let error = IncubatorProfile::from_toml(&qemu_mshv).unwrap_err();
        assert!(error.to_string().contains("not yet supported"), "{error:#}");
        for invalid in [
            MSHV.replace("nested-virt = true", "nested-virt = false"),
            MSHV.replace("x86-64", "aarch64"),
            OPENVMM.replace("processors = 2", "processors = 0"),
        ] {
            assert!(IncubatorProfile::from_toml(&invalid).is_err());
        }
        assert!(
            IncubatorProfile::from_toml(
                &OPENVMM.replace("[boot]", "boot-artifacts = 'wrong-section'\n[boot]")
            )
            .is_err()
        );
        assert!(IncubatorProfile::from_toml(&OPENVMM.replace("linux-direct", "typo")).is_err());
        Ok(())
    }
}
