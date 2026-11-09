// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Portable configuration for the supported FVP platform tuples.

use serde::Deserialize;
use std::collections::BTreeSet;

/// A named UART in the pinned FVP platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FvpConsole {
    /// Normal-world Linux and EFI shell.
    Host,
    /// EDK2 debug output.
    Edk2,
    /// Secure-world output.
    Secure,
    /// Realm management monitor output.
    Rmm,
}

impl FvpConsole {
    /// Stable console name used in logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Edk2 => "edk2",
            Self::Secure => "secure",
            Self::Rmm => "rmm",
        }
    }

    /// Packaged FVP terminal identifier.
    pub fn terminal(self) -> &'static str {
        match self {
            Self::Host => "bp.terminal_0",
            Self::Edk2 => "bp.terminal_1",
            Self::Secure => "bp.terminal_2",
            Self::Rmm => "bp.terminal_3",
        }
    }
}

/// Per-phase host deadlines, in seconds, for the FVP lifecycle.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct FvpDeadlines {
    /// Allowance for each input or post-run validation phase.
    pub validation: u64,
    /// Time from launching Shrinkwrap to observing the model process.
    pub model_start: u64,
    /// Time from observing the model to usable Pipette.
    pub pipette_ready: u64,
    /// Maximum duration of a dispatched test.
    pub test_execution: u64,
    /// Graceful guest shutdown allowance.
    pub guest_shutdown: u64,
    /// Forced process/container cleanup allowance.
    pub forced_cleanup: u64,
    /// Model lock acquisition allowance; zero means do not wait.
    pub model_lock: u64,
    /// Total port-allocation and launch-collision retry budget.
    pub port_allocation: u64,
    /// Host elapsed-time bound on guest DHCP.
    pub dhcp: u64,
}

impl Default for FvpDeadlines {
    fn default() -> Self {
        Self {
            validation: 300,
            model_start: 120,
            pipette_ready: 900,
            test_execution: 1800,
            guest_shutdown: 120,
            forced_cleanup: 30,
            model_lock: 1800,
            port_allocation: 60,
            dhcp: 30,
        }
    }
}

impl FvpDeadlines {
    /// Reject unreasonable budgets before starting external commands.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, value, minimum, maximum) in [
            ("validation", self.validation, 30, 1800),
            ("model-start", self.model_start, 30, 300),
            ("pipette-ready", self.pipette_ready, 120, 1800),
            ("test-execution", self.test_execution, 300, 7200),
            ("guest-shutdown", self.guest_shutdown, 30, 300),
            ("forced-cleanup", self.forced_cleanup, 10, 120),
            ("model-lock", self.model_lock, 0, 7200),
            ("port-allocation", self.port_allocation, 10, 300),
            ("dhcp", self.dhcp, 10, 120),
        ] {
            anyhow::ensure!(
                (minimum..=maximum).contains(&value),
                "FVP {name} deadline must be in {minimum}..={maximum} seconds"
            );
        }
        Ok(())
    }
}

/// Configuration independent of any local toolchain or firmware package root.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct FvpCcaConfig {
    /// Immutable platform and payload tuple to validate.
    #[serde(default)]
    pub platform: FvpPlatform,
    /// Console outputs to retain.
    pub consoles: Vec<FvpConsole>,
    /// Console carrying Linux startup and readiness.
    pub primary_console: FvpConsole,
    /// Requested capabilities, published only after runtime qualification.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Per-phase deadlines in seconds.
    #[serde(default)]
    pub deadlines: FvpDeadlines,
    /// Maximum port collision attempts within the fixed port budget.
    #[serde(default = "default_fvp_port_retries")]
    pub port_retries: u32,
}

/// Explicitly supported FVP firmware and kernel tuples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FvpPlatform {
    /// Current host kernel and firmware, without an assigned-device fixture.
    #[default]
    GuestMemfdInPlace,
    /// Current host/guest pair with the AHCI fixture.
    RealmVfio,
}

impl FvpPlatform {
    pub(crate) fn has_realm_vfio(self) -> bool {
        self == Self::RealmVfio
    }

    pub(crate) fn payload_hashes(self) -> (&'static str, &'static str) {
        use petri_artifacts_common::cca_payload;
        (
            cca_payload::LINUX_IMAGE_SHA256,
            cca_payload::BASE_INITRD_SHA256,
        )
    }
}

fn default_fvp_port_retries() -> u32 {
    20
}

impl FvpCcaConfig {
    /// Load a standalone FVP profile without depending on the Incubator CLI.
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Profile {
            incubator: Backend,
        }

        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "kebab-case")]
        enum Backend {
            FvpCca(FvpCcaConfig),
        }
        let Profile {
            incubator: Backend::FvpCca(config),
        } = toml_edit::de::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate configuration before inspecting host platform inputs.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.deadlines.validate()?;
        anyhow::ensure!(
            (1..=50).contains(&self.port_retries),
            "FVP port retries must be in 1..=50"
        );
        anyhow::ensure!(
            !self.consoles.is_empty(),
            "FVP requires at least one console"
        );
        let mut consoles = BTreeSet::new();
        for console in &self.consoles {
            anyhow::ensure!(
                consoles.insert(console),
                "duplicate FVP console: {}",
                console.name()
            );
        }
        anyhow::ensure!(
            consoles.contains(&self.primary_console),
            "FVP primary console must be in the console list"
        );
        anyhow::ensure!(
            self.primary_console == FvpConsole::Host,
            "FVP primary console must be host for the pinned platform"
        );
        let mut capabilities = BTreeSet::new();
        for capability in &self.capabilities {
            anyhow::ensure!(
                capability == "cca"
                    || (matches!(
                        self.platform,
                        FvpPlatform::GuestMemfdInPlace | FvpPlatform::RealmVfio
                    ) && capability == "guest_memfd_in_place")
                    || (self.platform.has_realm_vfio() && capability == "cca_realm_vfio"),
                "unsupported FVP capability: {capability}"
            );
            anyhow::ensure!(
                capabilities.insert(capability),
                "duplicate FVP capability: {capability}"
            );
        }

        Ok(())
    }
}

pub(crate) fn reject_retired_payload_selector(
    value: Option<&std::ffi::OsStr>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.is_none(),
        "PETRI_FVP_PAYLOAD is retired; use the current FVP profiles and matching artifacts"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn current_profiles_keep_fixture_capabilities_separate() {
        let mut host = FvpCcaConfig::from_toml(include_str!(
            "../../../incubator/profiles/aarch64-fvp-cca.toml"
        ))
        .unwrap();
        host.capabilities.push("cca_realm_vfio".into());
        assert!(host.validate().is_err());
        FvpCcaConfig::from_toml(include_str!(
            "../../../incubator/profiles/aarch64-fvp-cca-realm-vfio.toml"
        ))
        .unwrap();
    }

    #[test]
    fn retired_payload_selectors_and_platforms_are_rejected() {
        reject_retired_payload_selector(None).unwrap();
        for value in ["reference", "source-built", "cca74"] {
            assert!(reject_retired_payload_selector(Some(std::ffi::OsStr::new(value))).is_err());
        }
        let current = include_str!("../../../incubator/profiles/aarch64-fvp-cca-realm-vfio.toml");
        for value in ["cca-v15", "realm-vfio-source-built", "cca74-realm-vfio"] {
            assert!(FvpCcaConfig::from_toml(&current.replace("realm-vfio", value)).is_err());
        }
    }
}
