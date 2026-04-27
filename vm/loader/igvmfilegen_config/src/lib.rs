// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration for generating IGVM files. These are deserialized from a JSON
//! manifest file used by the file builder.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::CString;
use std::path::PathBuf;

/// The UEFI config type to pass to the UEFI loader.
#[derive(Serialize, Deserialize, Debug, Copy, Clone)]
#[serde(rename_all = "snake_case")]
pub enum UefiConfigType {
    /// No UEFI config set at load time.
    None,
    /// UEFI config is specified via IGVM parameters.
    Igvm,
}

/// The interrupt injection type that should be used for VMPL0 on SNP.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SnpInjectionType {
    /// Normal injection.
    Normal,
    /// Restricted injection.
    Restricted,
}

/// Secure AVIC type.
#[derive(Serialize, Default, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SecureAvicType {
    /// Offload AVIC to the hardware.
    Enabled,
    /// The paravisor emulates APIC.
    #[default]
    Disabled,
}

/// The isolation type that should be used for the loader.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ConfigIsolationType {
    /// No isolation is present.
    None,
    /// Hypervisor based isolation (VBS) is present.
    Vbs {
        /// Boolean representing if the guest allows debugging
        enable_debug: bool,
    },
    /// AMD SEV-SNP.
    Snp {
        /// The optional shared GPA boundary to configure for the guest. A
        /// `None` value represents a guest that no shared GPA boundary is to be
        /// configured.
        shared_gpa_boundary_bits: Option<u8>,
        /// The SEV-SNP policy for the guest.
        policy: u64,
        /// Boolean representing if the guest allows debugging
        enable_debug: bool,
        /// The interrupt injection type to use for the highest vmpl.
        injection_type: SnpInjectionType,
        /// Secure AVIC
        #[serde(default)]
        secure_avic: SecureAvicType,
    },
    /// Intel TDX.
    Tdx {
        /// Boolean representing if the guest allows debugging
        enable_debug: bool,
        /// Boolean representing if the guest is disallowed from handling
        /// virtualization exceptions
        sept_ve_disable: bool,
    },
}

impl ConfigIsolationType {
    /// The generated IGVM platform type for this isolation configuration.
    pub fn platform_type_str(&self) -> &'static str {
        match self {
            ConfigIsolationType::None | ConfigIsolationType::Vbs { .. } => "vsm_isolation",
            ConfigIsolationType::Snp { .. } => "snp",
            ConfigIsolationType::Tdx { .. } => "tdx",
        }
    }

    fn enable_debug(&self) -> Option<bool> {
        match *self {
            ConfigIsolationType::None => None,
            ConfigIsolationType::Vbs { enable_debug }
            | ConfigIsolationType::Snp { enable_debug, .. }
            | ConfigIsolationType::Tdx { enable_debug, .. } => Some(enable_debug),
        }
    }
}

/// Which platform header schema version to emit.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlatformHeadersVersion {
    /// Emit original supported-platform headers only.
    V1,
    /// Emit v2 supported-platform headers.
    V2,
}

impl PlatformHeadersVersion {
    /// The default platform header version.
    pub fn v1_default() -> Self {
        PlatformHeadersVersion::V1
    }
}

/// Requirement for confidential debugging.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DebugRequirement {
    /// The context requires debugging to be enabled.
    Enabled,
    /// The context requires debugging to be disabled.
    Disabled,
    /// The context accepts either debugging state.
    Any,
}

impl DebugRequirement {
    fn as_str(self) -> &'static str {
        match self {
            DebugRequirement::Enabled => "enabled",
            DebugRequirement::Disabled => "disabled",
            DebugRequirement::Any => "any",
        }
    }
}

/// Requirement for migration support.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MigrationRequirement {
    /// The context requires migration to be enabled.
    Enabled,
    /// The context requires migration to be disabled.
    Disabled,
    /// The context accepts either migration state.
    Any,
}

impl MigrationRequirement {
    /// The default migration requirement.
    pub fn any_default() -> Self {
        MigrationRequirement::Any
    }

    fn as_str(self) -> &'static str {
        match self {
            MigrationRequirement::Enabled => "enabled",
            MigrationRequirement::Disabled => "disabled",
            MigrationRequirement::Any => "any",
        }
    }
}

/// Requirements used to disambiguate a generated context.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ContextRequirements {
    /// Requirement for confidential debugging.
    pub debug: DebugRequirement,
    /// Requirement for migration support.
    #[serde(default = "MigrationRequirement::any_default")]
    pub migration: MigrationRequirement,
}

/// Platform key used to select a v1 fallback context.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PlatformFallbackKey {
    /// VBS/VSM isolation fallback context.
    Vbs,
    /// SNP fallback context.
    Snp,
    /// TDX fallback context.
    Tdx,
}

impl PlatformFallbackKey {
    /// The generated platform type this fallback key requires.
    pub fn expected_platform_type_str(&self) -> &'static str {
        match self {
            PlatformFallbackKey::Vbs => "vsm_isolation",
            PlatformFallbackKey::Snp => "snp",
            PlatformFallbackKey::Tdx => "tdx",
        }
    }

    /// The serialized key string.
    pub fn as_str(&self) -> &'static str {
        match self {
            PlatformFallbackKey::Vbs => "vbs",
            PlatformFallbackKey::Snp => "snp",
            PlatformFallbackKey::Tdx => "tdx",
        }
    }
}

/// Configuration for generated platform headers.
#[derive(Serialize, Deserialize, Debug)]
pub struct PlatformHeaders {
    /// Platform header schema version.
    #[serde(default = "PlatformHeadersVersion::v1_default")]
    pub version: PlatformHeadersVersion,
    /// Contexts to also emit as v1 fallback headers/files in v2 mode.
    #[serde(default)]
    pub v1_fallback_contexts: HashMap<PlatformFallbackKey, String>,
}

impl Default for PlatformHeaders {
    fn default() -> Self {
        PlatformHeaders {
            version: PlatformHeadersVersion::V1,
            v1_fallback_contexts: HashMap::new(),
        }
    }
}

/// Configuration on what to load.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Image {
    /// Load nothing.
    None,
    /// Load UEFI.
    Uefi { config_type: UefiConfigType },
    /// Load the OpenHCL paravisor.
    Openhcl {
        /// The paravisor kernel command line.
        #[serde(default)]
        command_line: String,
        /// If false, the host may provide additional kernel command line
        /// parameters at runtime.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        static_command_line: bool,
        /// The base page number for paravisor memory. None means relocation is used.
        #[serde(skip_serializing_if = "Option::is_none")]
        memory_page_base: Option<u64>,
        /// The number of pages for paravisor memory.
        memory_page_count: u64,
        /// Include the UEFI firmware for loading into the guest.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        uefi: bool,
        /// Include the Linux kernel for loading into the guest.
        #[serde(skip_serializing_if = "Option::is_none")]
        linux: Option<LinuxImage>,
    },
    /// Load the Linux kernel.
    /// TODO: Currently, this only works with underhill.
    Linux(LinuxImage),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct LinuxImage {
    /// Load with an initrd.
    pub use_initrd: bool,
    /// The command line to boot the kernel with.
    pub command_line: CString,
}

impl Image {
    /// Get the required resources for this image config.
    pub fn required_resources(&self) -> Vec<ResourceType> {
        match *self {
            Image::None => vec![],
            Image::Uefi { .. } => vec![ResourceType::Uefi],
            Image::Openhcl {
                uefi, ref linux, ..
            } => [
                ResourceType::UnderhillKernel,
                ResourceType::OpenhclBoot,
                ResourceType::UnderhillInitrd,
            ]
            .into_iter()
            .chain(if uefi { Some(ResourceType::Uefi) } else { None })
            .chain(linux.iter().flat_map(|linux| linux.required_resources()))
            .collect(),
            Image::Linux(ref linux) => linux.required_resources(),
        }
    }
}

impl LinuxImage {
    fn required_resources(&self) -> Vec<ResourceType> {
        [ResourceType::LinuxKernel]
            .into_iter()
            .chain(if self.use_initrd {
                Some(ResourceType::LinuxInitrd)
            } else {
                None
            })
            .collect()
    }
}

/// The config used to describe an initial guest context to be generated by the
/// tool.
#[derive(Serialize, Deserialize, Debug)]
pub struct GuestConfig {
    /// The unique context name for v2 multi-context files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_name: Option<String>,
    /// Requirements used to select this context in v2 multi-context files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requirements: Option<ContextRequirements>,
    /// The SVN of this guest.
    pub guest_svn: u32,
    /// The maximum VTL to be enabled for the guest.
    pub max_vtl: u8,
    /// The isolation type to be used for the guest.
    pub isolation_type: ConfigIsolationType,
    /// The image to load into the guest.
    pub image: Image,
}

/// The architecture of the igvm file.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum GuestArch {
    /// x64
    X64,
    /// AArch64 aka ARM64
    Aarch64,
}

/// The config used to describe a multi-architecture IGVM file containing
/// multiple guests.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct Config {
    /// The architecture of the igvm file.
    pub guest_arch: GuestArch,
    /// Platform header generation options.
    #[serde(default)]
    pub platform_headers: PlatformHeaders,
    /// The array of guest configs to be used to generate a single IGVM file.
    pub guest_configs: Vec<GuestConfig>,
}

/// Error returned when an IGVM file generation config is invalid.
#[derive(thiserror::Error, Debug)]
pub enum ConfigValidationError {
    /// The same context name appears more than once.
    #[error("duplicate context name {0:?}")]
    DuplicateContextName(String),
    /// A v2 config contains a guest config without a context name.
    #[error("missing context name in v2 platform header mode")]
    MissingContextName,
    /// A v2 context is missing requirements.
    #[error("missing requirements for context {0:?}")]
    MissingRequirements(String),
    /// Two v2 contexts generate the same platform type and requirements.
    #[error("duplicate requirement set: {0}")]
    DuplicateRequirementSet(String),
    /// V2 platform header mode requires at least one v1 fallback context.
    #[error("missing v1 fallback contexts in v2 platform header mode")]
    MissingV1FallbackContexts,
    /// A v1 fallback context name was not found.
    #[error("fallback {key:?} references unknown context {context_name:?}")]
    UnknownFallbackContext {
        /// The fallback key.
        key: String,
        /// The context name referenced by the fallback key.
        context_name: String,
    },
    /// A v1 fallback context has the wrong generated platform type.
    #[error(
        "fallback {key:?} references context {context_name:?} with platform type {actual_platform:?}; expected {expected_platform:?}"
    )]
    FallbackPlatformMismatch {
        /// The fallback key.
        key: String,
        /// The context name referenced by the fallback key.
        context_name: String,
        /// The platform type required by the fallback key.
        expected_platform: String,
        /// The actual generated platform type of the context.
        actual_platform: String,
    },
    /// A context's requirements contradict its isolation debug flag.
    #[error(
        "context {context_name:?} has debug requirement {requirements_debug:?} but enable_debug is {enable_debug}"
    )]
    ContradictoryDebugRequirement {
        /// The context name.
        context_name: String,
        /// The requested debug requirement.
        requirements_debug: String,
        /// The debug flag from the isolation type.
        enable_debug: bool,
    },
    /// A release-only context enables OpenHCL confidential debugging.
    #[error("context {context_name:?} disables debug but enables OpenHCL confidential debug")]
    ConfidentialDebugInReleaseContext {
        /// The context name.
        context_name: String,
    },
}

impl Config {
    /// Get a vec representing the required resources for this config.
    pub fn required_resources(&self) -> Vec<ResourceType> {
        let mut resources = vec![];
        for guest_config in &self.guest_configs {
            resources.extend(guest_config.image.required_resources());
        }
        resources
    }

    /// Validate this config for IGVM file generation.
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        let is_v2 = self.platform_headers.version == PlatformHeadersVersion::V2;
        let mut context_names = HashSet::new();
        let mut context_platforms = HashMap::new();
        let mut requirement_sets = HashMap::new();

        for (index, guest_config) in self.guest_configs.iter().enumerate() {
            let context_name = match guest_config.context_name.as_deref() {
                Some(context_name) => context_name,
                None if is_v2 => return Err(ConfigValidationError::MissingContextName),
                None => "",
            };

            if !context_name.is_empty() {
                if !context_names.insert(context_name) {
                    return Err(ConfigValidationError::DuplicateContextName(
                        context_name.to_string(),
                    ));
                }
                context_platforms.insert(
                    context_name.to_string(),
                    guest_config.isolation_type.platform_type_str(),
                );
            }

            let requirements = match guest_config.requirements {
                Some(requirements) => Some(requirements),
                None if is_v2 => {
                    return Err(ConfigValidationError::MissingRequirements(
                        context_name.to_string(),
                    ));
                }
                None => None,
            };

            let context_name_for_error = || {
                guest_config
                    .context_name
                    .clone()
                    .unwrap_or_else(|| format!("<guest_config {index}>"))
            };

            if let Some(requirements) = requirements {
                if let Some(enable_debug) = guest_config.isolation_type.enable_debug() {
                    let requirements_debug = match requirements.debug {
                        DebugRequirement::Enabled if !enable_debug => Some("enabled"),
                        DebugRequirement::Disabled if enable_debug => Some("disabled"),
                        DebugRequirement::Enabled
                        | DebugRequirement::Disabled
                        | DebugRequirement::Any => None,
                    };

                    if let Some(requirements_debug) = requirements_debug {
                        return Err(ConfigValidationError::ContradictoryDebugRequirement {
                            context_name: context_name_for_error(),
                            requirements_debug: requirements_debug.to_string(),
                            enable_debug,
                        });
                    }
                }

                if requirements.debug == DebugRequirement::Disabled
                    && matches!(
                        &guest_config.image,
                        Image::Openhcl { command_line, .. }
                            if command_line.contains("OPENHCL_CONFIDENTIAL_DEBUG=1")
                    )
                {
                    return Err(ConfigValidationError::ConfidentialDebugInReleaseContext {
                        context_name: context_name_for_error(),
                    });
                }

                if is_v2 {
                    let requirement_set = (
                        guest_config.isolation_type.platform_type_str(),
                        requirements.debug,
                        requirements.migration,
                    );
                    if let Some(previous_context_name) =
                        requirement_sets.insert(requirement_set, context_name.to_string())
                    {
                        return Err(ConfigValidationError::DuplicateRequirementSet(format!(
                            "platform={} debug={} migration={} contexts={:?},{:?}",
                            requirement_set.0,
                            requirement_set.1.as_str(),
                            requirement_set.2.as_str(),
                            previous_context_name,
                            context_name,
                        )));
                    }
                }
            }
        }

        if is_v2 {
            if self.platform_headers.v1_fallback_contexts.is_empty() {
                return Err(ConfigValidationError::MissingV1FallbackContexts);
            }

            for (key, context_name) in &self.platform_headers.v1_fallback_contexts {
                let Some(actual_platform) = context_platforms.get(context_name) else {
                    return Err(ConfigValidationError::UnknownFallbackContext {
                        key: key.as_str().to_string(),
                        context_name: context_name.clone(),
                    });
                };
                let expected_platform = key.expected_platform_type_str();
                if *actual_platform != expected_platform {
                    return Err(ConfigValidationError::FallbackPlatformMismatch {
                        key: key.as_str().to_string(),
                        context_name: context_name.clone(),
                        expected_platform: expected_platform.to_string(),
                        actual_platform: (*actual_platform).to_string(),
                    });
                }
            }
        }

        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    Uefi,
    UnderhillKernel,
    OpenhclBoot,
    UnderhillInitrd,
    UnderhillSidecar,
    LinuxKernel,
    LinuxInitrd,
}

/// Resources used by igvmfilegen to generate IGVM files. These are generated by
/// build tooling and not checked into the repo.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct Resources {
    /// The set of resources to use to generate IGVM files. These paths must be
    /// absolute.
    #[serde(deserialize_with = "parse::resources")]
    resources: HashMap<ResourceType, PathBuf>,
}

mod parse {
    use super::*;
    use serde::Deserialize;
    use serde::Deserializer;
    use std::collections::HashMap;

    pub fn resources<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<HashMap<ResourceType, PathBuf>, D::Error> {
        let resources: HashMap<ResourceType, PathBuf> = Deserialize::deserialize(d)?;

        for (resource, path) in &resources {
            if !path.is_absolute() {
                return Err(serde::de::Error::custom(AbsolutePathError(
                    *resource,
                    path.clone(),
                )));
            }
        }

        Ok(resources)
    }
}

/// Error returned when required resources are missing.
#[derive(Debug)]
pub struct MissingResourcesError(pub Vec<ResourceType>);

impl std::fmt::Display for MissingResourcesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing resources: {:?}", self.0)
    }
}

impl std::error::Error for MissingResourcesError {}

/// Error returned when a resource is not an absolute path.
#[derive(Debug)]
pub struct AbsolutePathError(ResourceType, PathBuf);

impl std::fmt::Display for AbsolutePathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "resource {:?} path is not absolute: {:?}",
            self.0, self.1
        )
    }
}

impl std::error::Error for AbsolutePathError {}

impl Resources {
    /// Create a new set of resources. Returns an error if any of the paths are
    /// not absolute.
    pub fn new(resources: HashMap<ResourceType, PathBuf>) -> Result<Self, AbsolutePathError> {
        for (resource, path) in &resources {
            if !path.is_absolute() {
                return Err(AbsolutePathError(*resource, path.clone()));
            }
        }

        Ok(Resources { resources })
    }

    /// Get the resources for this set.
    pub fn resources(&self) -> &HashMap<ResourceType, PathBuf> {
        &self.resources
    }

    /// Get the resource path for a given resource type.
    pub fn get(&self, resource: ResourceType) -> Option<&PathBuf> {
        self.resources.get(&resource)
    }

    /// Check that the required resources are present. On error, returns which
    /// resources are missing.
    pub fn check_required(&self, required: &[ResourceType]) -> Result<(), MissingResourcesError> {
        let mut missing = vec![];
        for resource in required {
            if !self.resources.contains_key(resource) {
                missing.push(*resource);
            }
        }

        if missing.is_empty() {
            Ok(())
        } else {
            Err(MissingResourcesError(missing))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::Path;

    fn validate_json(json: &str) -> Result<(), ConfigValidationError> {
        let config: Config = serde_json::from_str(json).expect("config should parse");
        config.validate()
    }

    fn v2_config(guest_configs: &str, v1_fallback_contexts: &str) -> String {
        format!(
            r#"{{
                "guest_arch": "x64",
                "platform_headers": {{
                    "version": "v2",
                    "v1_fallback_contexts": {v1_fallback_contexts}
                }},
                "guest_configs": {guest_configs}
            }}"#
        )
    }

    fn v2_guest(context_name: &str, debug: &str, isolation_type: &str, image: &str) -> String {
        format!(
            r#"{{
                "context_name": "{context_name}",
                "requirements": {{
                    "debug": "{debug}",
                    "migration": "any"
                }},
                "guest_svn": 1,
                "max_vtl": 2,
                "isolation_type": {isolation_type},
                "image": {image}
            }}"#
        )
    }

    fn valid_v2_json() -> String {
        let release = v2_guest("release", "disabled", r#""none""#, r#""none""#);
        let debug = v2_guest("debug", "enabled", r#""none""#, r#""none""#);
        v2_config(&format!("[{release}, {debug}]"), r#"{"vbs": "release"}"#)
    }

    #[test]
    fn non_absolute_path_new() {
        let mut resources = HashMap::new();
        resources.insert(ResourceType::Uefi, PathBuf::from("./uefi"));
        let result = Resources::new(resources);
        assert!(result.is_err());
    }

    #[test]
    fn parse_non_absolute_path() {
        let resources = r#"{"uefi":"./uefi"}"#;
        let result: Result<Resources, _> = serde_json::from_str(resources);
        assert!(result.is_err());
    }

    #[test]
    fn missing_resources() {
        let resources = Resources {
            resources: HashMap::new(),
        };
        let required = vec![ResourceType::Uefi];
        let result = resources.check_required(&required);
        assert!(result.is_err());
    }

    #[test]
    fn parse_existing_manifests() {
        let manifests = [
            "openhcl-aarch64-dev.json",
            "openhcl-aarch64-release.json",
            "openhcl-x64-cvm-dev.json",
            "openhcl-x64-cvm-release.json",
            "openhcl-x64-dev.json",
            "openhcl-x64-direct-dev.json",
            "openhcl-x64-direct-release.json",
            "openhcl-x64-multicontext.json",
            "openhcl-x64-release.json",
            "uefi-aarch64.json",
            "uefi-x64.json",
        ];

        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("igvmfilegen_config has a parent directory")
            .join("manifests");

        for manifest in manifests {
            let path = manifest_dir.join(manifest);
            let config: Config = serde_json::from_str(
                &std::fs::read_to_string(&path).expect("manifest should be readable"),
            )
            .expect("manifest should parse");
            config.validate().expect("manifest should validate");
        }
    }

    #[test]
    fn valid_v2_config_validates() {
        validate_json(&valid_v2_json()).expect("valid v2 config should validate");
    }

    #[test]
    fn duplicate_context_names_are_rejected() {
        let first = v2_guest("release", "disabled", r#""none""#, r#""none""#);
        let second = v2_guest("release", "enabled", r#""none""#, r#""none""#);
        let err = validate_json(&v2_config(
            &format!("[{first}, {second}]"),
            r#"{"vbs": "release"}"#,
        ))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigValidationError::DuplicateContextName(name) if name == "release"
        ));
    }

    #[test]
    fn duplicate_requirement_sets_are_rejected() {
        let first = v2_guest("release-a", "disabled", r#""none""#, r#""none""#);
        let second = v2_guest("release-b", "disabled", r#""none""#, r#""none""#);
        let err = validate_json(&v2_config(
            &format!("[{first}, {second}]"),
            r#"{"vbs": "release-a"}"#,
        ))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigValidationError::DuplicateRequirementSet(_)
        ));
    }

    #[test]
    fn missing_requirements_in_v2_are_rejected() {
        let json = v2_config(
            r#"[{
                "context_name": "release",
                "guest_svn": 1,
                "max_vtl": 2,
                "isolation_type": "none",
                "image": "none"
            }]"#,
            r#"{"vbs": "release"}"#,
        );
        let err = validate_json(&json).unwrap_err();
        assert!(matches!(
            err,
            ConfigValidationError::MissingRequirements(name) if name == "release"
        ));
    }

    #[test]
    fn unknown_fallback_context_is_rejected() {
        let err = validate_json(&v2_config(
            &format!(
                "[{}]",
                v2_guest("release", "disabled", r#""none""#, r#""none""#)
            ),
            r#"{"vbs": "missing"}"#,
        ))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigValidationError::UnknownFallbackContext { key, context_name }
                if key == "vbs" && context_name == "missing"
        ));
    }

    #[test]
    fn fallback_platform_mismatch_is_rejected() {
        let err = validate_json(&v2_config(
            &format!(
                "[{}]",
                v2_guest("release", "disabled", r#""none""#, r#""none""#)
            ),
            r#"{"snp": "release"}"#,
        ))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigValidationError::FallbackPlatformMismatch {
                key,
                context_name,
                expected_platform,
                actual_platform
            } if key == "snp"
                && context_name == "release"
                && expected_platform == "snp"
                && actual_platform == "vsm_isolation"
        ));
    }

    #[test]
    fn contradictory_debug_settings_are_rejected() {
        let err = validate_json(&v2_config(
            &format!(
                "[{}]",
                v2_guest(
                    "debug",
                    "enabled",
                    r#"{"vbs": {"enable_debug": false}}"#,
                    r#""none""#
                )
            ),
            r#"{"vbs": "debug"}"#,
        ))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigValidationError::ContradictoryDebugRequirement {
                context_name,
                requirements_debug,
                enable_debug: false
            } if context_name == "debug" && requirements_debug == "enabled"
        ));
    }

    #[test]
    fn confidential_debug_in_release_context_is_rejected() {
        let err = validate_json(&v2_config(
            &format!(
                "[{}]",
                v2_guest(
                    "release",
                    "disabled",
                    r#""none""#,
                    r#"{"openhcl": {
                        "command_line": "OPENHCL_CONFIDENTIAL_DEBUG=1",
                        "memory_page_count": 1
                    }}"#
                )
            ),
            r#"{"vbs": "release"}"#,
        ))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigValidationError::ConfidentialDebugInReleaseContext { context_name }
                if context_name == "release"
        ));
    }
}
