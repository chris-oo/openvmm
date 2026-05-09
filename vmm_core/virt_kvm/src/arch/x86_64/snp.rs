// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(dead_code)]

use crate::KvmError;
use loader::importer::BootPageAcceptance;

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

pub fn snp_launch_page_type(acceptance: BootPageAcceptance) -> Result<SnpLaunchPageType, KvmError> {
    match acceptance {
        BootPageAcceptance::Exclusive => Ok(SnpLaunchPageType::Normal),
        BootPageAcceptance::ExclusiveUnmeasured => Ok(SnpLaunchPageType::Unmeasured),
        BootPageAcceptance::VpContext => Ok(SnpLaunchPageType::Vmsa),
        BootPageAcceptance::SecretsPage => Ok(SnpLaunchPageType::Secrets),
        BootPageAcceptance::CpuidPage => Ok(SnpLaunchPageType::Cpuid),
        BootPageAcceptance::Shared => Ok(SnpLaunchPageType::Shared),
        BootPageAcceptance::CpuidExtendedStatePage | BootPageAcceptance::ErrorPage => {
            Err(KvmError::UnsupportedSnpPageAcceptance(acceptance))
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
            snp_launch_page_type(BootPageAcceptance::Exclusive).unwrap(),
            SnpLaunchPageType::Normal
        );
        assert_eq!(
            snp_launch_page_type(BootPageAcceptance::ExclusiveUnmeasured).unwrap(),
            SnpLaunchPageType::Unmeasured
        );
        assert_eq!(
            snp_launch_page_type(BootPageAcceptance::VpContext).unwrap(),
            SnpLaunchPageType::Vmsa
        );
        assert_eq!(
            snp_launch_page_type(BootPageAcceptance::SecretsPage).unwrap(),
            SnpLaunchPageType::Secrets
        );
        assert_eq!(
            snp_launch_page_type(BootPageAcceptance::CpuidPage).unwrap(),
            SnpLaunchPageType::Cpuid
        );
        assert_eq!(
            snp_launch_page_type(BootPageAcceptance::Shared).unwrap(),
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
            snp_launch_page_type(BootPageAcceptance::CpuidExtendedStatePage),
            Err(KvmError::UnsupportedSnpPageAcceptance(
                BootPageAcceptance::CpuidExtendedStatePage
            ))
        ));
        assert!(matches!(
            snp_launch_page_type(BootPageAcceptance::ErrorPage),
            Err(KvmError::UnsupportedSnpPageAcceptance(
                BootPageAcceptance::ErrorPage
            ))
        ));
    }
}
