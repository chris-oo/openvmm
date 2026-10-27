// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux EFI handoff for the measured firmware metadata.

use arrayvec::ArrayVec;
use loader_defs::linux;
use uefi_specs::uefi::boot::EFI_MEMORY_DESCRIPTOR_VERSION;
use uefi_specs::uefi::boot::EFI_MEMORY_WB;
use uefi_specs::uefi::boot::EfiMemoryDescriptor;
use uefi_specs::uefi::boot::EfiMemoryType;

const PAGE_SIZE: u64 = 4096;
const MAX_ENTRIES: usize = 128;
type E820Map = ArrayVec<linux::e820entry, MAX_ENTRIES>;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    InvalidMap,
    InvalidRegion,
    TooManyEntries,
    UncoveredRegion,
    MapTooLarge,
}

fn entry(start: u64, end: u64, kind: u32) -> linux::e820entry {
    linux::e820entry {
        addr: start.into(),
        size: (end - start).into(),
        typ: kind.into(),
    }
}

fn set_type(
    map: &mut E820Map,
    start: u64,
    end: u64,
    kind: u32,
    require_ram: bool,
) -> Result<(), Error> {
    let mut output = E820Map::new();
    let mut covered = start;
    for original in map.iter() {
        let base = original.addr.get();
        let limit = base + original.size.get();
        if limit <= start || base >= end {
            output
                .try_push(*original)
                .map_err(|_| Error::TooManyEntries)?;
            continue;
        }
        let overlap_start = base.max(start);
        let overlap_end = limit.min(end);
        if overlap_start != covered || (require_ram && original.typ.get() != linux::E820_RAM) {
            return Err(Error::UncoveredRegion);
        }
        if base < overlap_start {
            output
                .try_push(entry(base, overlap_start, original.typ.get()))
                .map_err(|_| Error::TooManyEntries)?;
        }
        output
            .try_push(entry(overlap_start, overlap_end, kind))
            .map_err(|_| Error::TooManyEntries)?;
        if overlap_end < limit {
            output
                .try_push(entry(overlap_end, limit, original.typ.get()))
                .map_err(|_| Error::TooManyEntries)?;
        }
        covered = overlap_end;
    }
    if covered != end {
        return Err(Error::UncoveredRegion);
    }
    *map = output;
    Ok(())
}

/// Reserves the persistent tables and describes the complete final E820 map.
pub(crate) fn prepare(
    zero_page: &mut linux::boot_params,
    platform: &linux::SnpBootShimPlatformParams,
    ram_end: u64,
) -> Result<ArrayVec<EfiMemoryDescriptor, MAX_ENTRIES>, Error> {
    let efi_end = platform
        .efi_output_gpa
        .checked_add(platform.efi_output_size)
        .filter(|end| *end <= ram_end)
        .ok_or(Error::InvalidRegion)?;
    let rsdp_end = platform
        .rsdp_gpa
        .checked_add(PAGE_SIZE)
        .filter(|end| *end <= ram_end)
        .ok_or(Error::InvalidRegion)?;
    if platform.efi_output_size != linux::SNP_BOOT_SHIM_EFI_SIZE
        || !platform.efi_output_gpa.is_multiple_of(PAGE_SIZE)
        || !platform.rsdp_gpa.is_multiple_of(PAGE_SIZE)
        || platform.efi_output_gpa < rsdp_end && platform.rsdp_gpa < efi_end
    {
        return Err(Error::InvalidRegion);
    }
    let entries = zero_page
        .e820_map
        .get(..usize::from(zero_page.e820_entries))
        .ok_or(Error::InvalidMap)?;
    let mut map = E820Map::new();
    let mut previous_end = 0;
    for &entry in entries {
        let start = entry.addr.get();
        let size = entry.size.get();
        let end = start.checked_add(size).ok_or(Error::InvalidMap)?;
        if size == 0
            || !start.is_multiple_of(PAGE_SIZE)
            || !size.is_multiple_of(PAGE_SIZE)
            || start < previous_end
        {
            return Err(Error::InvalidMap);
        }
        map.try_push(entry).map_err(|_| Error::TooManyEntries)?;
        previous_end = end;
    }
    set_type(
        &mut map,
        platform.efi_output_gpa,
        efi_end,
        linux::E820_RESERVED,
        true,
    )?;
    set_type(
        &mut map,
        platform.rsdp_gpa,
        rsdp_end,
        linux::E820_ACPI,
        false,
    )?;
    let mut descriptors = ArrayVec::new();
    for entry in &map {
        let typ = if entry.addr.get() >= platform.efi_output_gpa
            && entry.addr.get() + entry.size.get() <= efi_end
        {
            EfiMemoryType::EFI_BOOT_SERVICES_DATA
        } else {
            match entry.typ.get() {
                linux::E820_RAM => EfiMemoryType::EFI_CONVENTIONAL_MEMORY,
                linux::E820_ACPI => EfiMemoryType::EFI_ACPI_RECLAIM_MEMORY,
                linux::E820_NVS => EfiMemoryType::EFI_ACPI_MEMORY_NVS,
                linux::E820_UNUSABLE => EfiMemoryType::EFI_UNUSABLE_MEMORY,
                _ => EfiMemoryType::EFI_RESERVED_MEMORY_TYPE,
            }
        };
        descriptors
            .try_push(EfiMemoryDescriptor {
                typ,
                _pad: 0,
                physical_start: entry.addr.get(),
                virtual_start: 0,
                number_of_pages: entry.size.get() / PAGE_SIZE,
                attribute: EFI_MEMORY_WB,
            })
            .map_err(|_| Error::TooManyEntries)?;
    }
    let map_size = descriptors.len() * size_of::<EfiMemoryDescriptor>();
    if map_size as u64 > platform.efi_output_size - PAGE_SIZE {
        return Err(Error::MapTooLarge);
    }
    zero_page.e820_map[..map.len()].copy_from_slice(&map);
    zero_page.e820_entries = map.len().try_into().map_err(|_| Error::TooManyEntries)?;
    let map_gpa = platform.efi_output_gpa + PAGE_SIZE;
    zero_page.efi_info = linux::efi_info {
        efi_loader_signature: u32::from_le_bytes(*b"EL64"),
        efi_systab: platform.efi_output_gpa as u32,
        efi_systab_hi: (platform.efi_output_gpa >> 32) as u32,
        efi_memmap: map_gpa as u32,
        efi_memmap_hi: (map_gpa >> 32) as u32,
        efi_memmap_size: map_size.try_into().map_err(|_| Error::MapTooLarge)?,
        efi_memdesc_size: size_of::<EfiMemoryDescriptor>() as u32,
        efi_memdesc_version: EFI_MEMORY_DESCRIPTOR_VERSION,
    };
    Ok(descriptors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loader_defs::linux::SnpBootShimPlatformParams;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    fn input() -> (linux::boot_params, SnpBootShimPlatformParams) {
        let mut zero_page = linux::boot_params::new_zeroed();
        zero_page.e820_entries = 4;
        zero_page.e820_map[..4].copy_from_slice(&[
            entry(0, 0xe0000, linux::E820_RAM),
            entry(0xe0000, 0x100000, linux::E820_RESERVED),
            entry(0x100000, 0x10000000, linux::E820_RAM),
            entry(0x1_0000_0000, 0x1_1000_0000, linux::E820_RAM),
        ]);
        (
            zero_page,
            SnpBootShimPlatformParams {
                efi_output_gpa: 0x4000000,
                efi_output_size: linux::SNP_BOOT_SHIM_EFI_SIZE,
                rsdp_gpa: 0xe0000,
                ..FromZeros::new_zeroed()
            },
        )
    }

    #[test]
    fn reserves_tables_and_preserves_high_ram() {
        let (mut zero_page, platform) = input();
        let map = prepare(&mut zero_page, &platform, 0x10000000).unwrap();
        assert_eq!(
            zero_page.efi_info.efi_loader_signature.to_le_bytes(),
            *b"EL64"
        );
        assert_eq!(
            zero_page.efi_info.efi_systab,
            platform.efi_output_gpa as u32
        );
        assert_eq!(
            zero_page.efi_info.efi_memmap_size as usize,
            map.len() * size_of::<EfiMemoryDescriptor>()
        );
        let metadata = map
            .iter()
            .find(|entry| entry.physical_start == platform.efi_output_gpa)
            .unwrap();
        assert_eq!(metadata.typ, EfiMemoryType::EFI_BOOT_SERVICES_DATA);
        assert_eq!(metadata.number_of_pages, 3);
        let rsdp = map
            .iter()
            .find(|entry| entry.physical_start == 0xe0000)
            .unwrap();
        assert_eq!(rsdp.typ, EfiMemoryType::EFI_ACPI_RECLAIM_MEMORY);
        assert!(map.iter().any(|entry| {
            entry.physical_start == 0x1_0000_0000
                && entry.typ == EfiMemoryType::EFI_CONVENTIONAL_MEMORY
        }));
        for pair in map.windows(2) {
            assert!(
                pair[0].physical_start + pair[0].number_of_pages * PAGE_SIZE
                    <= pair[1].physical_start
            );
        }
    }

    #[test]
    fn rejects_invalid_maps_and_regions() {
        let (mut zero_page, mut platform) = input();
        platform.efi_output_gpa = !(PAGE_SIZE - 1);
        assert!(matches!(
            prepare(&mut zero_page, &platform, u64::MAX),
            Err(Error::InvalidRegion)
        ));
        let (mut zero_page, platform) = input();
        zero_page.e820_map[2].addr = 0xd0000.into();
        assert!(matches!(
            prepare(&mut zero_page, &platform, 0x10000000),
            Err(Error::InvalidMap)
        ));
        let (mut zero_page, mut platform) = input();
        platform.efi_output_gpa = 0xf0000;
        assert!(matches!(
            prepare(&mut zero_page, &platform, 0x10000000),
            Err(Error::UncoveredRegion)
        ));
    }

    #[test]
    fn rejects_reservations_that_exceed_e820_capacity() {
        let (mut zero_page, mut platform) = input();
        zero_page.e820_entries = MAX_ENTRIES as u8;
        for (index, slot) in zero_page.e820_map.iter_mut().enumerate() {
            let base = index as u64 * 0x100000;
            *slot = entry(base, base + 0x100000, linux::E820_RAM);
        }
        platform.efi_output_gpa = 0x40000;
        assert!(matches!(
            prepare(&mut zero_page, &platform, 0x10000000),
            Err(Error::TooManyEntries)
        ));
        assert_eq!(zero_page.efi_info.efi_loader_signature, 0);
    }
}
