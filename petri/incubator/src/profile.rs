// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Incubator profile definitions.

use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

/// An incubator profile describing the backend platform and how to run it.
#[derive(Debug, Deserialize)]
pub struct IncubatorProfile {
    /// Incubator backend configuration.
    pub incubator: IncubatorBackend,
    /// Extra devices to add to the platform.
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
}

/// Backend-specific configuration, tagged by `type`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum IncubatorBackend {
    /// QEMU TCG emulation.
    QemuTcg(QemuTcgConfig),
    /// QEMU Arm CCA emulation.
    QemuCca(QemuCcaConfig),
}

impl IncubatorBackend {
    /// The guest architecture this backend emulates.
    pub fn arch(&self) -> Arch {
        match self {
            IncubatorBackend::QemuTcg(config) => config.arch,
            IncubatorBackend::QemuCca(_) => Arch::Aarch64,
        }
    }
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
    /// `rdinit=/tcg-init.sh` (the injected init script); everything else,
    /// including the arch-specific serial console (e.g., "console=ttyAMA0"
    /// for aarch64 PL011, "console=ttyS0" for x86 16550), comes from here.
    pub cmdline: String,
}

/// QEMU Arm CCA configuration parsed from the profile.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct QemuCcaConfig {
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
    /// Load a profile from a TOML file.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path).context("failed to read incubator profile")?;
        Self::from_toml(&contents)
    }

    /// Parse a profile from a TOML string.
    pub fn from_toml(toml: &str) -> anyhow::Result<Self> {
        let profile: Self =
            toml_edit::de::from_str(toml).context("failed to parse incubator profile")?;
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if let IncubatorBackend::QemuCca(config) = &self.incubator {
            validate_qemu_cca(config)?;
        }
        Ok(())
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
            !console.is_empty(),
            "QEMU CCA console names must not be empty"
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

    let mut capabilities = BTreeSet::new();
    for capability in &config.capabilities {
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
}
