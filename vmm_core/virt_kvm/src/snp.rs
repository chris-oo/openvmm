// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
use crate::memory::private_memory_range_from_slots;
use std::mem::size_of;
use std::os::fd::AsFd;
use virt::InitialPageImportType;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum SnpLaunchState {
    NotStarted,
    Started,
    Finished,
    Failed,
}

impl virt::AcceptInitialPages for KvmPartition {
    type Error = KvmError;

    fn accept_initial_pages(&self, pages: &[virt::InitialPageImport]) -> Result<(), Self::Error> {
        self.inner.snp_launch_initial_pages(pages)
    }
}

impl KvmPartitionInner {
    fn snp_launch_initial_pages(&self, pages: &[virt::InitialPageImport]) -> Result<(), KvmError> {
        {
            let mut state = self.snp_launch_state.lock();
            match *state {
                SnpLaunchState::NotStarted => *state = SnpLaunchState::Started,
                SnpLaunchState::Started => return Err(KvmError::SnpLaunchInProgress),
                SnpLaunchState::Finished => return Ok(()),
                SnpLaunchState::Failed => return Err(KvmError::SnpLaunchFailed),
            }
        }

        tracing::info!(page_ranges = pages.len(), "starting SNP launch");
        match self.snp_launch_initial_pages_inner(pages) {
            Ok(()) => {
                *self.snp_launch_state.lock() = SnpLaunchState::Finished;
                tracing::info!("finished SNP launch");
                Ok(())
            }
            Err(err) => {
                *self.snp_launch_state.lock() = SnpLaunchState::Failed;
                tracing::error!(error = &err as &dyn std::error::Error, "failed SNP launch");
                Err(err)
            }
        }
    }

    fn snp_launch_initial_pages_inner(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), KvmError> {
        let sev = self.sev.as_ref().ok_or(KvmError::IsolationNotSupported)?;
        self.kvm.check_sev_snp_launch_extensions()?;
        let mut launch_start = kvm::kvm_sev_snp_launch_start {
            // TODO: This debug-capable policy is for bring-up.
            policy: (1 << 19) | (1 << 17) | (1 << 16),
            ..Default::default()
        };
        tracing::debug!(policy = launch_start.policy, "KVM_SEV_SNP_LAUNCH_START");
        self.kvm
            .sev_snp_launch_start(sev.as_fd(), &mut launch_start)?;

        let memory = self.memory.lock();
        for page in pages {
            let launch_page_type = crate::arch::snp::snp_launch_page_type(page.import_type)?;
            let Some(kvm_page_type) = launch_page_type.kvm_page_type() else {
                return Err(KvmError::UnsupportedSnpPageImportType(page.import_type));
            };

            let private_range = private_memory_range_from_slots(page.range, &memory.ranges)
                .map_err(map_snp_private_range_error)?;
            if page.import_type == InitialPageImportType::Cpuid {
                tracing::debug!(
                    gpa = page.range.start(),
                    len = page.range.len(),
                    cpuid_entries = self.bsp_cpuid.len(),
                    "writing SNP CPUID page"
                );
                write_snp_cpuid_page(private_range.hva, page.range.len(), &self.bsp_cpuid)?;
                Self::trace_snp_cpuid_page(
                    "SNP CPUID page before launch update",
                    private_range.hva,
                    page.range.len(),
                );
            }
            if page.tag == "linux-pagetables" {
                let c_bit = snp_c_bit_from_cpuid(&self.bsp_cpuid)?;
                tracing::debug!(
                    gpa = page.range.start(),
                    len = page.range.len(),
                    c_bit,
                    "setting SNP C-bit in Linux direct-boot page tables"
                );
                set_snp_c_bit_in_page_tables(private_range.hva, page.range.len(), c_bit)?;
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    private_range.hva.cast_const(),
                    page.range.len() as usize,
                )
            };
            for run in split_zero_page_runs(bytes)? {
                let page_type = if run.kind == SnpPageRunKind::Zero
                    && kvm_page_type == kvm::SevSnpPageType::Normal
                {
                    kvm::SevSnpPageType::Zero
                } else {
                    kvm_page_type
                };
                let gpa = page.range.start() + run.byte_offset as u64;
                let uaddr = if page_type == kvm::SevSnpPageType::Zero {
                    0
                } else {
                    private_range.hva.wrapping_add(run.byte_offset) as u64
                };
                tracing::trace!(
                    gpa,
                    len = run.byte_len,
                    ?page_type,
                    import_type = ?page.import_type,
                    tag = page.tag,
                    "KVM_SEV_SNP_LAUNCH_UPDATE"
                );
                let cpuid_page_before =
                    (page.import_type == InitialPageImportType::Cpuid).then(|| unsafe {
                        std::slice::from_raw_parts(private_range.hva, page.range.len() as usize)
                            .to_vec()
                    });
                if let Err(err) = self.kvm.sev_snp_launch_update(
                    sev.as_fd(),
                    gpa / hvdef::HV_PAGE_SIZE,
                    uaddr,
                    run.byte_len as u64,
                    page_type,
                ) {
                    if let Some(cpuid_page_before) = cpuid_page_before {
                        Self::trace_snp_cpuid_page_diff(
                            &cpuid_page_before,
                            private_range.hva,
                            page.range.len(),
                        );
                    }
                    return Err(err.into());
                }
                if page.import_type == InitialPageImportType::Cpuid {
                    Self::trace_snp_cpuid_page(
                        "SNP CPUID page after launch update",
                        private_range.hva,
                        page.range.len(),
                    );
                }
            }
        }
        self.prepare_snp_vmsa_register_state()?;
        tracing::debug!("KVM_SEV_SNP_LAUNCH_FINISH");
        self.kvm
            .sev_snp_launch_finish(sev.as_fd(), &mut Default::default())?;
        Ok(())
    }

    fn prepare_snp_vmsa_register_state(&self) -> Result<(), KvmError> {
        for vp in &self.vps {
            let vp_info = vp.vp_info();
            let kvm_vp = self.kvm.vp(vp_info.apic_id);
            let sregs = kvm_vp.get_sregs()?;

            let xcr0 = kvm_vp.get_xcr0()?;
            if xcr0 & x86defs::xsave::XFEATURE_X87 == 0 {
                kvm_vp.set_xcr0(xcr0 | x86defs::xsave::XFEATURE_X87)?;
            }

            if vp_info.base.vp_index.is_bsp() {
                validate_snp_bsp_register_state(&kvm_vp.get_regs()?, &sregs)?;
            }
        }

        Ok(())
    }

    fn trace_snp_cpuid_page(message: &'static str, page: *const u8, page_len: u64) {
        const SNP_CPUID_COUNT_MAX: usize = 64;
        const SNP_CPUID_TABLE_HEADER_SIZE: usize = 16;
        const SNP_CPUID_FN_SIZE: usize = 48;

        if page_len < (SNP_CPUID_TABLE_HEADER_SIZE + SNP_CPUID_COUNT_MAX * SNP_CPUID_FN_SIZE) as u64
        {
            tracing::warn!(page_len, message);
            return;
        }

        let page = unsafe { std::slice::from_raw_parts(page, page_len as usize) };
        let count = u32::from_le_bytes(page[0..4].try_into().unwrap()) as usize;
        let mut standard_range = None;
        let mut hypervisor_range = None;
        let mut extended_range = None;
        let mut snp_leaf = None;
        for index in 0..count.min(SNP_CPUID_COUNT_MAX) {
            let offset = SNP_CPUID_TABLE_HEADER_SIZE + index * SNP_CPUID_FN_SIZE;
            let entry = Self::read_snp_cpuid_fn(&page[offset..][..SNP_CPUID_FN_SIZE]);
            match (entry.eax_in, entry.ecx_in) {
                (0, 0) => standard_range = Some(entry.eax),
                (0x4000_0000, 0) => hypervisor_range = Some(entry.eax),
                (0x8000_0000, 0) => extended_range = Some(entry.eax),
                (0x8000_001f, 0) => snp_leaf = Some((entry.eax, entry.ebx, entry.ecx, entry.edx)),
                _ => {}
            }
        }

        tracing::debug!(
            count,
            ?standard_range,
            ?hypervisor_range,
            ?extended_range,
            ?snp_leaf,
            message
        );
    }

    fn trace_snp_cpuid_page_diff(before: &[u8], after: *const u8, page_len: u64) {
        const SNP_CPUID_COUNT_MAX: usize = 64;
        const SNP_CPUID_TABLE_HEADER_SIZE: usize = 16;
        const SNP_CPUID_FN_SIZE: usize = 48;

        if page_len < (SNP_CPUID_TABLE_HEADER_SIZE + SNP_CPUID_COUNT_MAX * SNP_CPUID_FN_SIZE) as u64
        {
            tracing::warn!(page_len, "SNP CPUID debug page is too small");
            return;
        }

        let after = unsafe { std::slice::from_raw_parts(after, page_len as usize) };
        let count = u32::from_le_bytes(after[0..4].try_into().unwrap()) as usize;
        tracing::warn!(count, "SNP CPUID page after firmware rejection");
        for index in 0..count.min(SNP_CPUID_COUNT_MAX) {
            let offset = SNP_CPUID_TABLE_HEADER_SIZE + index * SNP_CPUID_FN_SIZE;
            let before_entry = &before[offset..][..SNP_CPUID_FN_SIZE];
            let after_entry = &after[offset..][..SNP_CPUID_FN_SIZE];
            if before_entry == after_entry {
                continue;
            }
            let before = Self::read_snp_cpuid_fn(before_entry);
            let after = Self::read_snp_cpuid_fn(after_entry);
            tracing::warn!(
                index,
                before.eax_in,
                before.ecx_in,
                before.xcr0_in,
                before.xss_in,
                before.eax,
                before.ebx,
                before.ecx,
                before.edx,
                after.eax_in,
                after.ecx_in,
                after.xcr0_in,
                after.xss_in,
                after.eax,
                after.ebx,
                after.ecx,
                after.edx,
                "SNP CPUID entry changed by firmware"
            );
        }
    }

    fn read_snp_cpuid_fn(entry: &[u8]) -> SnpCpuidFn {
        SnpCpuidFn {
            eax_in: u32::from_le_bytes(entry[0..4].try_into().unwrap()),
            ecx_in: u32::from_le_bytes(entry[4..8].try_into().unwrap()),
            xcr0_in: u64::from_le_bytes(entry[8..16].try_into().unwrap()),
            xss_in: u64::from_le_bytes(entry[16..24].try_into().unwrap()),
            eax: u32::from_le_bytes(entry[24..28].try_into().unwrap()),
            ebx: u32::from_le_bytes(entry[28..32].try_into().unwrap()),
            ecx: u32::from_le_bytes(entry[32..36].try_into().unwrap()),
            edx: u32::from_le_bytes(entry[36..40].try_into().unwrap()),
        }
    }
}

struct SnpCpuidFn {
    eax_in: u32,
    ecx_in: u32,
    xcr0_in: u64,
    xss_in: u64,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
}

fn validate_snp_bsp_register_state(
    regs: &kvm::kvm_regs,
    sregs: &kvm::kvm_sregs,
) -> Result<(), KvmError> {
    const REQUIRED_CR0: u64 = x86defs::X64_CR0_PE | x86defs::X64_CR0_PG;
    const REQUIRED_CR4: u64 = x86defs::X64_CR4_PAE;
    const REQUIRED_EFER: u64 =
        x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA | x86defs::X64_EFER_NXE;

    if sregs.cr0 & REQUIRED_CR0 != REQUIRED_CR0 {
        return Err(KvmError::InvalidState("invalid SNP BSP CR0"));
    }
    if sregs.cr3 == 0 {
        return Err(KvmError::InvalidState("invalid SNP BSP CR3"));
    }
    if sregs.cr4 & REQUIRED_CR4 != REQUIRED_CR4 {
        return Err(KvmError::InvalidState("invalid SNP BSP CR4"));
    }
    if sregs.efer & REQUIRED_EFER != REQUIRED_EFER {
        return Err(KvmError::InvalidState("invalid SNP BSP EFER"));
    }
    if sregs.cs.present == 0 || sregs.cs.l == 0 {
        return Err(KvmError::InvalidState("invalid SNP BSP CS"));
    }
    if regs.rip == 0 {
        return Err(KvmError::InvalidState("invalid SNP BSP RIP"));
    }

    tracing::debug!(
        rip = regs.rip,
        rsi = regs.rsi,
        cr0 = sregs.cr0,
        cr3 = sregs.cr3,
        cr4 = sregs.cr4,
        efer = sregs.efer,
        vmsa_efer = sregs.efer | x86defs::X64_EFER_SVME,
        cs_selector = sregs.cs.selector,
        cs_base = sregs.cs.base,
        cs_limit = sregs.cs.limit,
        cs_type = sregs.cs.type_,
        ds_selector = sregs.ds.selector,
        es_selector = sregs.es.selector,
        ss_selector = sregs.ss.selector,
        "validated SNP BSP register state"
    );

    Ok(())
}

fn snp_c_bit_from_cpuid(cpuid: &[kvm::kvm_cpuid_entry2]) -> Result<u8, KvmError> {
    cpuid
        .iter()
        .find(|entry| entry.function == 0x8000_001f && entry.index == 0)
        .map(|entry| (entry.ebx & 0x3f) as u8)
        .ok_or(KvmError::MissingSnpCBit)
}

fn set_snp_c_bit_in_page_tables(
    page_table: *mut u8,
    page_table_len: u64,
    c_bit: u8,
) -> Result<(), KvmError> {
    if !page_table_len.is_multiple_of(size_of::<u64>() as u64) {
        return Err(KvmError::InvalidSnpPageTableLength(page_table_len));
    }

    let c_bit_mask = 1u64 << c_bit;
    // SAFETY: The caller provides a valid page-table backing region, and the
    // length is validated to be a whole number of u64 entries.
    let entries = unsafe {
        std::slice::from_raw_parts_mut(page_table.cast::<u64>(), page_table_len as usize / 8)
    };
    for entry in entries {
        if *entry & 1 != 0 {
            *entry |= c_bit_mask;
        }
    }

    Ok(())
}

fn write_snp_cpuid_page(
    page: *mut u8,
    page_len: u64,
    cpuid: &[kvm::kvm_cpuid_entry2],
) -> Result<(), KvmError> {
    const SNP_CPUID_COUNT_MAX: usize = 64;
    const SNP_CPUID_TABLE_HEADER_SIZE: usize = 16;
    const SNP_CPUID_FN_SIZE: usize = 48;

    if cpuid.len() > SNP_CPUID_COUNT_MAX {
        return Err(KvmError::TooManySnpCpuidEntries(cpuid.len()));
    }
    if page_len < (SNP_CPUID_TABLE_HEADER_SIZE + SNP_CPUID_COUNT_MAX * SNP_CPUID_FN_SIZE) as u64 {
        return Err(KvmError::InvalidSnpLaunchRange);
    }

    let page = unsafe { std::slice::from_raw_parts_mut(page, page_len as usize) };
    page.fill(0);
    page[..4].copy_from_slice(&(cpuid.len() as u32).to_le_bytes());

    for (index, cpuid) in cpuid.iter().copied().enumerate() {
        let mut cpuid = cpuid;
        sanitize_snp_cpuid_entry(&mut cpuid);
        let entry = &mut page[SNP_CPUID_TABLE_HEADER_SIZE + index * SNP_CPUID_FN_SIZE..]
            [..SNP_CPUID_FN_SIZE];
        entry[0..4].copy_from_slice(&cpuid.function.to_le_bytes());
        entry[4..8].copy_from_slice(&cpuid.index.to_le_bytes());
        let initial_xsave_leaf = cpuid.function
            == x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0
            && (cpuid.index == 0 || cpuid.index == 1);
        let (xcr0, xss) = if initial_xsave_leaf {
            (1_u64, 0_u64)
        } else {
            (0_u64, 0_u64)
        };
        entry[8..16].copy_from_slice(&xcr0.to_le_bytes());
        entry[16..24].copy_from_slice(&xss.to_le_bytes());
        entry[24..28].copy_from_slice(&cpuid.eax.to_le_bytes());
        let ebx = if initial_xsave_leaf { 0x240 } else { cpuid.ebx };
        entry[28..32].copy_from_slice(&ebx.to_le_bytes());
        entry[32..36].copy_from_slice(&cpuid.ecx.to_le_bytes());
        entry[36..40].copy_from_slice(&cpuid.edx.to_le_bytes());
    }

    Ok(())
}

fn sanitize_snp_cpuid_entry(entry: &mut kvm::kvm_cpuid_entry2) {
    match (entry.function, entry.index) {
        // SNP firmware validates the CPUID page against hardware-supported
        // CPUID values, not KVM's synthetic guest CPUID additions.
        (0x1, _) => entry.ecx &= !0x01000000,
        (0x7, 0) => {
            entry.ebx &= !0x2;
            entry.edx = 0;
        }
        (0x80000008, _) => entry.ebx &= !0x02000000,
        (0x80000021, _) => {
            entry.eax &= !0x200;
            entry.ecx = 0;
        }
        _ => {}
    }
}

fn map_snp_private_range_error(err: KvmError) -> KvmError {
    match err {
        KvmError::InvalidPrivateMemoryRange => KvmError::InvalidSnpLaunchRange,
        err => err,
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum SnpPageRunKind {
    Zero,
    NonZero,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct SnpPageRun {
    byte_offset: usize,
    byte_len: usize,
    kind: SnpPageRunKind,
}

pub(crate) fn split_zero_page_runs(bytes: &[u8]) -> Result<Vec<SnpPageRun>, KvmError> {
    const PAGE_SIZE: usize = hvdef::HV_PAGE_SIZE as usize;
    if !bytes.len().is_multiple_of(PAGE_SIZE) {
        return Err(KvmError::UnalignedSnpLaunchRange);
    }

    let mut runs: Vec<SnpPageRun> = Vec::new();
    for (page_index, page) in bytes.chunks_exact(PAGE_SIZE).enumerate() {
        let kind = if page.iter().all(|&byte| byte == 0) {
            SnpPageRunKind::Zero
        } else {
            SnpPageRunKind::NonZero
        };
        if let Some(run) = runs.last_mut()
            && run.kind == kind
        {
            run.byte_len += PAGE_SIZE;
            continue;
        }
        runs.push(SnpPageRun {
            byte_offset: page_index * PAGE_SIZE,
            byte_len: PAGE_SIZE,
            kind,
        });
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of_val;

    #[test]
    fn split_zero_page_runs_coalesces_adjacent_pages_by_zero_state() {
        let mut bytes = vec![0u8; 5 * hvdef::HV_PAGE_SIZE as usize];
        bytes[hvdef::HV_PAGE_SIZE as usize] = 1;
        bytes[2 * hvdef::HV_PAGE_SIZE as usize] = 2;
        bytes[4 * hvdef::HV_PAGE_SIZE as usize] = 3;

        let runs = split_zero_page_runs(&bytes).unwrap();

        assert_eq!(
            runs,
            vec![
                SnpPageRun {
                    byte_offset: 0,
                    byte_len: hvdef::HV_PAGE_SIZE as usize,
                    kind: SnpPageRunKind::Zero,
                },
                SnpPageRun {
                    byte_offset: hvdef::HV_PAGE_SIZE as usize,
                    byte_len: 2 * hvdef::HV_PAGE_SIZE as usize,
                    kind: SnpPageRunKind::NonZero,
                },
                SnpPageRun {
                    byte_offset: 3 * hvdef::HV_PAGE_SIZE as usize,
                    byte_len: hvdef::HV_PAGE_SIZE as usize,
                    kind: SnpPageRunKind::Zero,
                },
                SnpPageRun {
                    byte_offset: 4 * hvdef::HV_PAGE_SIZE as usize,
                    byte_len: hvdef::HV_PAGE_SIZE as usize,
                    kind: SnpPageRunKind::NonZero,
                },
            ]
        );
    }

    #[test]
    fn split_zero_page_runs_rejects_partial_pages() {
        assert!(matches!(
            split_zero_page_runs(&[0; 17]),
            Err(KvmError::UnalignedSnpLaunchRange)
        ));
    }

    #[test]
    fn snp_c_bit_from_cpuid_reads_memory_encryption_bit() {
        let cpuid = [kvm::kvm_cpuid_entry2 {
            function: 0x8000_001f,
            index: 0,
            ebx: 51,
            ..Default::default()
        }];

        assert_eq!(snp_c_bit_from_cpuid(&cpuid).unwrap(), 51);
    }

    #[test]
    fn set_snp_c_bit_in_page_tables_updates_present_entries() {
        let mut entries = [0x1000u64 | 1, 0, 0x2000u64 | 1 << 1, 0x3000u64 | 1];

        set_snp_c_bit_in_page_tables(
            entries.as_mut_ptr().cast::<u8>(),
            size_of_val(&entries) as u64,
            51,
        )
        .unwrap();

        assert_eq!(entries[0], (1u64 << 51) | 0x1001);
        assert_eq!(entries[1], 0);
        assert_eq!(entries[2], 0x2002);
        assert_eq!(entries[3], (1u64 << 51) | 0x3001);
    }

    #[test]
    fn write_snp_cpuid_page_writes_linux_table_and_xsave_inputs() {
        let mut page = vec![0xff; hvdef::HV_PAGE_SIZE as usize];
        let cpuid = [
            kvm::kvm_cpuid_entry2 {
                function: 1,
                index: 0,
                eax: 0x11,
                ebx: 0x12,
                ecx: 0x13,
                edx: 0x14,
                ..Default::default()
            },
            kvm::kvm_cpuid_entry2 {
                function: x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0,
                index: 0,
                eax: 0x21,
                ebx: 0x22,
                ecx: 0x23,
                edx: 0x24,
                ..Default::default()
            },
        ];

        write_snp_cpuid_page(page.as_mut_ptr(), page.len() as u64, &cpuid).unwrap();

        assert_eq!(u32::from_le_bytes(page[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(page[16..20].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(page[40..44].try_into().unwrap()), 0x11);
        assert_eq!(u32::from_le_bytes(page[44..48].try_into().unwrap()), 0x12);
        let xsave = 16 + 48;
        assert_eq!(
            u32::from_le_bytes(page[xsave..xsave + 4].try_into().unwrap()),
            x86defs::cpuid::CpuidFunction::ExtendedStateEnumeration.0
        );
        assert_eq!(
            u64::from_le_bytes(page[xsave + 8..xsave + 16].try_into().unwrap()),
            1
        );
        assert_eq!(
            u32::from_le_bytes(page[xsave + 28..xsave + 32].try_into().unwrap()),
            0x240
        );
    }
}
