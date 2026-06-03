// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use crate::KvmError;
use virt::InitialPageImportType;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SnpLaunchPageType {
    Normal,
    Zero,
    Unmeasured,
    Secrets,
    Cpuid,
    Vmsa,
    Shared,
}

impl SnpLaunchPageType {
    pub const fn kvm_page_type(self) -> Option<kvm::SevSnpPageType> {
        match self {
            SnpLaunchPageType::Normal => Some(kvm::SevSnpPageType::Normal),
            SnpLaunchPageType::Zero => Some(kvm::SevSnpPageType::Zero),
            SnpLaunchPageType::Unmeasured => Some(kvm::SevSnpPageType::Unmeasured),
            SnpLaunchPageType::Secrets => Some(kvm::SevSnpPageType::Secrets),
            SnpLaunchPageType::Cpuid => Some(kvm::SevSnpPageType::Cpuid),
            SnpLaunchPageType::Vmsa | SnpLaunchPageType::Shared => None,
        }
    }
}

pub fn snp_launch_page_type(
    import_type: InitialPageImportType,
) -> Result<SnpLaunchPageType, KvmError> {
    match import_type {
        InitialPageImportType::Normal => Ok(SnpLaunchPageType::Normal),
        InitialPageImportType::NormalUnmeasured => Ok(SnpLaunchPageType::Unmeasured),
        InitialPageImportType::VpContext => Ok(SnpLaunchPageType::Vmsa),
        InitialPageImportType::Secrets => Ok(SnpLaunchPageType::Secrets),
        InitialPageImportType::Cpuid => Ok(SnpLaunchPageType::Cpuid),
        InitialPageImportType::Shared => Ok(SnpLaunchPageType::Shared),
        InitialPageImportType::CpuidExtendedState => {
            Err(KvmError::UnsupportedSnpPageImportType(import_type))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn maps_supported_boot_acceptance_to_snp_launch_page_type() {
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::Normal).unwrap(),
            SnpLaunchPageType::Normal
        );
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::NormalUnmeasured).unwrap(),
            SnpLaunchPageType::Unmeasured
        );
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::VpContext).unwrap(),
            SnpLaunchPageType::Vmsa
        );
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::Secrets).unwrap(),
            SnpLaunchPageType::Secrets
        );
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::Cpuid).unwrap(),
            SnpLaunchPageType::Cpuid
        );
        assert_eq!(
            snp_launch_page_type(InitialPageImportType::Shared).unwrap(),
            SnpLaunchPageType::Shared
        );
    }

    #[test]
    fn maps_snp_launch_page_type_to_kvm_uapi_page_type() {
        assert_eq!(
            SnpLaunchPageType::Normal.kvm_page_type(),
            Some(kvm::SevSnpPageType::Normal)
        );
        assert_eq!(
            SnpLaunchPageType::Zero.kvm_page_type(),
            Some(kvm::SevSnpPageType::Zero)
        );
        assert_eq!(
            SnpLaunchPageType::Unmeasured.kvm_page_type(),
            Some(kvm::SevSnpPageType::Unmeasured)
        );
        assert_eq!(
            SnpLaunchPageType::Secrets.kvm_page_type(),
            Some(kvm::SevSnpPageType::Secrets)
        );
        assert_eq!(
            SnpLaunchPageType::Cpuid.kvm_page_type(),
            Some(kvm::SevSnpPageType::Cpuid)
        );
        assert_eq!(SnpLaunchPageType::Vmsa.kvm_page_type(), None);
        assert_eq!(SnpLaunchPageType::Shared.kvm_page_type(), None);
    }

    #[test]
    fn rejects_unsupported_boot_acceptance_for_initial_snp_launch() {
        assert!(matches!(
            snp_launch_page_type(InitialPageImportType::CpuidExtendedState),
            Err(KvmError::UnsupportedSnpPageImportType(
                InitialPageImportType::CpuidExtendedState
            ))
        ));
    }
}
