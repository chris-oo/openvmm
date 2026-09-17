// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe topology construction and validation helpers.
//!
//! These turn the manifest's [`PcieRootComplexConfig`]/[`PciePortConfig`]
//! entries into runtime root-port definitions and validate that the configured
//! root complexes form a consistent bus-number topology before the VM is built.

use cxl_spec::pci_registers::spec::flex_bus_port_dvsec::CxlFlexBusPortDvsecCapability;
use openvmm_defs::config::PciePortConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use pci_core::spec::caps::acs::DEFAULT_ACS_CAP_MASK;
use pci_core::spec::caps::pci_express::MaxEndEndTlpPrefixes;
use pcie::GenericPciePortDefinition;
use pcie::PciePortSettings;

/// The initial trusted-assignment path isolates one static endpoint's BAR
/// resource view from the ordinary shared Virtio roots.
pub(super) fn realm_root_index(
    roots: &[PcieRootComplexConfig],
    devices: &[(&str, &str)],
    in_place_cca: bool,
) -> anyhow::Result<Option<u32>> {
    let realm_devices: Vec<_> = devices
        .iter()
        .filter(|(_, resource)| *resource == "vfio-realm")
        .collect();
    if realm_devices.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(in_place_cca, "Realm PCI assignment requires in-place CCA");
    anyhow::ensure!(
        realm_devices.len() == 1,
        "only one Realm PCI device is supported"
    );
    let (port_name, _) = *realm_devices[0];
    let mut matching_roots = roots
        .iter()
        .filter(|root| root.ports.iter().any(|port| port.name == port_name));
    let root = matching_roots
        .next()
        .ok_or_else(|| anyhow::anyhow!("Realm device port '{port_name}' has no root"))?;
    anyhow::ensure!(
        matching_roots.next().is_none(),
        "Realm device port is ambiguous"
    );
    anyhow::ensure!(
        roots
            .iter()
            .filter(|candidate| candidate.index == root.index)
            .count()
            == 1,
        "Realm root index must not be shared by another root"
    );
    anyhow::ensure!(
        root.ports.len() == 1
            && root.preserve_bars
            && root.cxl.is_none()
            && root.iommu.is_none()
            && root.end_bus
                == root
                    .start_bus
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Realm root has no endpoint bus"))?,
        "Realm device requires a dedicated two-bus root with preserved BARs and no CXL or guest IOMMU"
    );
    let port = &root.ports[0];
    anyhow::ensure!(
        !port.hotplug && !port.cxl && !port.pasid,
        "Realm device port must be static without CXL or PASID"
    );
    anyhow::ensure!(
        devices
            .iter()
            .filter(|(name, _)| *name == port_name)
            .count()
            == 1,
        "Realm device port must contain only its assigned endpoint"
    );
    Ok(Some(root.index))
}

/// Builds port PCIe settings from manifest flags.
///
/// When CXL is enabled, emit a default Flex Bus capability advertising both
/// cache and memory support.
///
/// When PASID is enabled, advertise support for up to four TLP prefixes to
/// work for both switch and root ports.
fn build_port_settings(port_cfg: &PciePortConfig) -> PciePortSettings {
    PciePortSettings {
        acs_capabilities_supported: port_cfg
            .acs_capabilities_supported
            .unwrap_or(DEFAULT_ACS_CAP_MASK),
        cxl_flex_bus_port_capability: port_cfg.cxl.then_some(
            CxlFlexBusPortDvsecCapability::new()
                .with_cache_capable(true)
                .with_mem_capable(true),
        ),
        tlp_prefixing_supported: port_cfg.pasid.then_some(MaxEndEndTlpPrefixes::Four),
    }
}

/// Converts a manifest port entry into the runtime port definition.
pub(super) fn build_port_definition(port_cfg: &PciePortConfig) -> GenericPciePortDefinition {
    let settings = build_port_settings(port_cfg);

    GenericPciePortDefinition {
        name: port_cfg.name.as_str().into(),
        devfn: port_cfg.devfn,
        hotplug: port_cfg.hotplug,
        settings,
    }
}

/// Validates that the configured PCIe root complexes form a consistent
/// topology: each bus range is well-formed (`start_bus <= end_bus`) and no two
/// root complexes on the same PCI segment have overlapping bus ranges.
pub(super) fn validate_pcie_root_complexes(
    root_complexes: &[PcieRootComplexConfig],
) -> anyhow::Result<()> {
    for (index, root_complex) in root_complexes.iter().enumerate() {
        if root_complex.start_bus > root_complex.end_bus {
            anyhow::bail!(
                "invalid PCIe root complex '{}': start_bus ({}) must be less than or equal to end_bus ({})",
                root_complex.name,
                root_complex.start_bus,
                root_complex.end_bus,
            );
        }

        for previous in &root_complexes[..index] {
            if root_complex.segment == previous.segment
                && root_complex.start_bus <= previous.end_bus
                && previous.start_bus <= root_complex.end_bus
            {
                anyhow::bail!(
                    "invalid PCIe root complex '{}': bus range {}..={} overlaps with '{}' bus range {}..={} on PCI segment {}",
                    root_complex.name,
                    root_complex.start_bus,
                    root_complex.end_bus,
                    previous.name,
                    previous.start_bus,
                    previous.end_bus,
                    root_complex.segment,
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openvmm_defs::config::PcieMmioRangeConfig;

    fn rc(name: &str, segment: u16, start_bus: u8, end_bus: u8) -> PcieRootComplexConfig {
        PcieRootComplexConfig {
            index: 0,
            name: name.to_string(),
            segment,
            start_bus,
            end_bus,
            low_mmio: PcieMmioRangeConfig::Dynamic { size: 0 },
            high_mmio: PcieMmioRangeConfig::Dynamic { size: 0 },
            ports: Vec::new(),
            cxl: None,
            iommu: None,
            vnode: None,
            preserve_bars: false,
        }
    }

    #[test]
    fn accepts_disjoint_ranges() {
        let rcs = [
            rc("rc0", 0, 0, 4),
            rc("rc1", 0, 5, 9),
            // Same bus range but a different segment is fine.
            rc("rc2", 1, 0, 4),
        ];
        validate_pcie_root_complexes(&rcs).unwrap();
    }

    #[test]
    fn rejects_inverted_bus_range() {
        let rcs = [rc("rc0", 0, 4, 0)];
        assert!(validate_pcie_root_complexes(&rcs).is_err());
    }

    #[test]
    fn rejects_overlapping_ranges_on_same_segment() {
        let rcs = [rc("rc0", 0, 0, 4), rc("rc1", 0, 4, 8)];
        assert!(validate_pcie_root_complexes(&rcs).is_err());
    }

    fn realm_root() -> PcieRootComplexConfig {
        let mut root = rc("realm", 0, 0, 1);
        root.preserve_bars = true;
        root.ports.push(PciePortConfig {
            name: "realm-port".into(),
            devfn: Some(0),
            hotplug: false,
            acs_capabilities_supported: None,
            cxl: false,
            pasid: false,
        });
        root
    }

    #[test]
    fn realm_resource_view_requires_an_isolated_static_root() {
        let devices = [("realm-port", "vfio-realm"), ("control-port", "virtio")];
        let root = realm_root();
        assert_eq!(realm_root_index(&[root], &devices, true).unwrap(), Some(0));
        assert!(realm_root_index(&[realm_root()], &devices, false).is_err());
        assert!(realm_root_index(&[realm_root(), rc("other", 1, 0, 1)], &devices, true).is_err());
        assert!(realm_root_index(&[], &devices, true).is_err());
        assert_eq!(
            realm_root_index(&[], &[("control", "virtio")], false).unwrap(),
            None
        );
        assert!(
            realm_root_index(
                &[realm_root()],
                &[("realm-port", "vfio-realm"), ("realm-port", "virtio"),],
                true
            )
            .is_err()
        );
        assert!(
            realm_root_index(
                &[realm_root()],
                &[("realm-port", "vfio-realm"), ("other", "vfio-realm"),],
                true
            )
            .is_err()
        );
        for mutation in 0..5 {
            let mut root = realm_root();
            match mutation {
                0 => root.preserve_bars = false,
                1 => root.ports[0].hotplug = true,
                2 => root.ports[0].pasid = true,
                3 => root.ports[0].cxl = true,
                4 => root.end_bus = 2,
                _ => unreachable!(),
            }
            assert!(realm_root_index(&[root], &devices, true).is_err());
        }
    }
}
