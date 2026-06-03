// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
use crate::memory::private_memory_range_from_slots;
use loader::importer::BootPageAcceptance;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum CcaLaunchState {
    NotStarted,
    Populating,
    Populated,
    Failed,
}

impl virt::FinalizeInitialPageImports for KvmPartition {
    type Error = KvmError;

    fn finalize_initial_page_imports(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), Self::Error> {
        self.inner.cca_populate_initial_pages(pages)
    }
}

impl KvmPartitionInner {
    fn cca_populate_initial_pages(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), KvmError> {
        {
            let mut state = self.cca_launch_state.lock();
            match *state {
                CcaLaunchState::NotStarted => *state = CcaLaunchState::Populating,
                CcaLaunchState::Populating => return Err(KvmError::CcaPopulateInProgress),
                CcaLaunchState::Populated => return Ok(()),
                CcaLaunchState::Failed => return Err(KvmError::CcaPopulateFailed),
            }
        }

        tracing::info!(page_ranges = pages.len(), "starting CCA initial population");
        match self.cca_populate_initial_pages_inner(pages) {
            Ok(()) => {
                *self.cca_launch_state.lock() = CcaLaunchState::Populated;
                tracing::info!("finished CCA initial population");
                Ok(())
            }
            Err(err) => {
                *self.cca_launch_state.lock() = CcaLaunchState::Failed;
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "failed CCA initial population"
                );
                Err(err)
            }
        }
    }

    fn cca_populate_initial_pages_inner(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), KvmError> {
        self.kvm
            .check_private_memory_extensions()
            .map_err(map_cca_capability_error)?;

        let pages = pages.to_vec();

        let memory = self.memory.lock();
        for page in &pages {
            if page.visibility == virt::PageVisibility::Shared {
                tracing::trace!(
                    gpa = page.range.start(),
                    len = page.range.len(),
                    acceptance = ?page.acceptance,
                    tag = page.tag.as_str(),
                    "skipping shared CCA initial page"
                );
                continue;
            }

            let flags = cca_populate_flags(page.acceptance)?;
            let private_range = private_memory_range_from_slots(page.range, &memory.ranges)
                .map_err(map_cca_private_range_error)?;
            let mut populate = kvm::KvmArmRmiPopulate {
                base: private_range.gpa.start(),
                size: private_range.gpa.len(),
                source_uaddr: private_range.hva as u64,
                flags,
                reserved: 0,
            };

            while populate.size != 0 {
                tracing::trace!(
                    gpa = populate.base,
                    len = populate.size,
                    source_uaddr = populate.source_uaddr,
                    flags = populate.flags,
                    acceptance = ?page.acceptance,
                    tag = page.tag.as_str(),
                    "KVM_ARM_RMI_POPULATE"
                );
                self.kvm.arm_rmi_populate(&mut populate)?;
            }
        }

        Ok(())
    }
}

fn cca_populate_flags(acceptance: BootPageAcceptance) -> Result<u32, KvmError> {
    match acceptance {
        BootPageAcceptance::Exclusive => Ok(kvm::KVM_ARM_RMI_POPULATE_FLAGS_MEASURE_UAPI),
        BootPageAcceptance::ExclusiveUnmeasured => Ok(0),
        BootPageAcceptance::Shared => Ok(0),
        BootPageAcceptance::VpContext
        | BootPageAcceptance::SecretsPage
        | BootPageAcceptance::CpuidPage
        | BootPageAcceptance::CpuidExtendedStatePage
        | BootPageAcceptance::ErrorPage => Err(KvmError::UnsupportedCcaPageAcceptance(acceptance)),
    }
}

fn map_cca_private_range_error(err: KvmError) -> KvmError {
    match err {
        KvmError::UnalignedPrivateMemoryRange => KvmError::UnalignedCcaPopulateRange,
        KvmError::InvalidPrivateMemoryRange => KvmError::InvalidCcaPopulateRange,
        err => err,
    }
}

pub(crate) fn map_cca_conversion_error(err: KvmError) -> KvmError {
    match err {
        KvmError::InvalidMapGpaRange => KvmError::InvalidCcaMemoryFault,
        err => err,
    }
}

fn map_cca_capability_error(err: kvm::Error) -> KvmError {
    match err {
        kvm::Error::MissingCapability(capability) => KvmError::MissingCcaCapability(capability),
        err => KvmError::Kvm(err),
    }
}
