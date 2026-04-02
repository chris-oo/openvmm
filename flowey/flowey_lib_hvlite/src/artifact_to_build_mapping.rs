// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mapping from petri artifact IDs to flowey build/download selections.
//!
//! This module uses the compile-time-verified `ArtifactBuildInfo` trait impls
//! (in `vmm_test_images`) to map artifact ID strings to build selections,
//! replacing the previous hand-maintained string match.

use crate::_jobs::local_build_and_run_nextest_vmm_tests::BuildSelections;
use std::collections::BTreeSet;
use vmm_test_images::ArtifactBuildCategory;
use vmm_test_images::BuildTarget;
use vmm_test_images::KnownTestArtifacts;

/// Result of resolving artifact requirements to build/download selections.
#[derive(Debug)]
pub struct ResolvedArtifactSelections {
    /// What to build
    pub build: BuildSelections,
    /// What to download
    pub downloads: BTreeSet<KnownTestArtifacts>,
    /// Any unknown artifacts that couldn't be mapped
    pub unknown: Vec<String>,
    /// Target triple from the artifacts file (if present)
    pub target_from_file: Option<String>,
    /// Whether any tests need release IGVM files from GitHub
    pub needs_release_igvm: bool,
}

impl Default for ResolvedArtifactSelections {
    fn default() -> Self {
        Self {
            build: BuildSelections::none(),
            downloads: BTreeSet::new(),
            unknown: Vec::new(),
            target_from_file: None,
            needs_release_igvm: false,
        }
    }
}

impl ResolvedArtifactSelections {
    /// Parse the JSON output from `--list-required-artifacts` and resolve to
    /// build/download selections.
    ///
    /// The `target_arch` and `target_os` parameters specify the target to
    /// validate against. If the JSON contains a `target` field, it will be
    /// checked to ensure it matches.
    pub fn from_artifact_list_json(
        json: &str,
        target_arch: target_lexicon::Architecture,
        target_os: target_lexicon::OperatingSystem,
    ) -> anyhow::Result<Self> {
        let parsed: ArtifactListOutput = serde_json::from_str(json)?;

        // Validate target if present in the JSON
        if let Some(ref file_target) = parsed.target {
            let expected_target = format!(
                "{}-{}",
                match target_arch {
                    target_lexicon::Architecture::X86_64 => "x86_64",
                    target_lexicon::Architecture::Aarch64(_) => "aarch64",
                    _ => "unknown",
                },
                match target_os {
                    target_lexicon::OperatingSystem::Windows => "pc-windows-msvc",
                    target_lexicon::OperatingSystem::Linux => "unknown-linux-gnu",
                    _ => "unknown",
                }
            );

            // Check if the target in the file is compatible with what we're building for
            if !file_target.contains(expected_target.split('-').next().unwrap_or(""))
                || (target_os == target_lexicon::OperatingSystem::Windows
                    && !file_target.contains("windows"))
                || (target_os == target_lexicon::OperatingSystem::Linux
                    && !file_target.contains("linux"))
            {
                anyhow::bail!(
                    "Target mismatch: artifacts file was generated for '{}', but building for '{}'",
                    file_target,
                    expected_target
                );
            }
        }

        let mut result = Self {
            target_from_file: parsed.target,
            ..Default::default()
        };

        // Process both required and optional artifacts
        for artifact in parsed.required.iter().chain(parsed.optional.iter()) {
            if !result.resolve_artifact(artifact, target_arch, target_os) {
                result.unknown.push(artifact.clone());
            }
        }

        Ok(result)
    }

    /// Resolve a single artifact ID and update selections. Returns true if the
    /// artifact was recognized.
    fn resolve_artifact(
        &mut self,
        artifact_id: &str,
        _target_arch: target_lexicon::Architecture,
        _target_os: target_lexicon::OperatingSystem,
    ) -> bool {
        let Some(category) = vmm_test_images::lookup_build_category(artifact_id) else {
            log::warn!("unknown artifact ID with no build mapping: {artifact_id}");
            return false;
        };

        match category {
            ArtifactBuildCategory::Build(target) => {
                self.apply_build_target(target);
            }
            ArtifactBuildCategory::Download {
                artifact,
                also_build,
            } => {
                self.downloads.insert(artifact);
                for &target in also_build {
                    self.apply_build_target(target);
                }
            }
            ArtifactBuildCategory::ReleaseDownload => {
                self.needs_release_igvm = true;
            }
            ArtifactBuildCategory::AlwaysAvailable => {}
        }

        true
    }

    fn apply_build_target(&mut self, target: BuildTarget) {
        match target {
            BuildTarget::Openvmm => self.build.openvmm = true,
            BuildTarget::OpenvmmVhost => self.build.openvmm_vhost = true,
            BuildTarget::Openhcl => self.build.openhcl = true,
            BuildTarget::GuestTestUefi => self.build.guest_test_uefi = true,
            BuildTarget::Tmks => self.build.tmks = true,
            BuildTarget::TmkVmmWindows => self.build.tmk_vmm_windows = true,
            BuildTarget::TmkVmmLinux => self.build.tmk_vmm_linux = true,
            BuildTarget::Vmgstool => self.build.vmgstool = true,
            BuildTarget::PipetteWindows => self.build.pipette_windows = true,
            BuildTarget::PipetteLinux => self.build.pipette_linux = true,
            BuildTarget::TpmGuestTestsWindows => self.build.tpm_guest_tests_windows = true,
            BuildTarget::TpmGuestTestsLinux => self.build.tpm_guest_tests_linux = true,
            BuildTarget::TestIgvmAgentRpcServer => self.build.test_igvm_agent_rpc_server = true,
            BuildTarget::PrepSteps => self.build.prep_steps = true,
        }
    }
}

/// JSON structure matching the output of `--list-required-artifacts`
#[derive(serde::Deserialize)]
struct ArtifactListOutput {
    /// Target triple the artifacts were discovered for (if present)
    #[serde(default)]
    target: Option<String>,
    required: Vec<String>,
    optional: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_openvmm() {
        let json = r#"{"required":["petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64"],"optional":[]}"#;
        let result = ResolvedArtifactSelections::from_artifact_list_json(
            json,
            target_lexicon::Architecture::X86_64,
            target_lexicon::OperatingSystem::Windows,
        )
        .unwrap();

        assert!(result.build.openvmm);
        assert!(!result.build.openhcl);
        assert!(result.downloads.is_empty());
        assert!(result.unknown.is_empty());
    }

    #[test]
    fn test_resolve_with_downloads() {
        let json = r#"{"required":["petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_X64"],"optional":[]}"#;
        let result = ResolvedArtifactSelections::from_artifact_list_json(
            json,
            target_lexicon::Architecture::X86_64,
            target_lexicon::OperatingSystem::Linux,
        )
        .unwrap();

        assert!(result.build.pipette_linux);
        assert!(
            result
                .downloads
                .contains(&KnownTestArtifacts::Ubuntu2404ServerX64Vhd)
        );
    }

    #[test]
    fn test_unknown_artifact() {
        let json = r#"{"required":["some::unknown::artifact"],"optional":[]}"#;
        let result = ResolvedArtifactSelections::from_artifact_list_json(
            json,
            target_lexicon::Architecture::X86_64,
            target_lexicon::OperatingSystem::Linux,
        )
        .unwrap();

        assert_eq!(result.unknown, vec!["some::unknown::artifact"]);
    }
}
