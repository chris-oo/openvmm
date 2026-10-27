// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Synthetic EFI metadata for firmware-less Linux boot.
//!
//! This contains no boot or runtime service implementations. Callers provide
//! the memory map and ACPI tables separately and reserve space for the blob.

use crate::smbios;
use thiserror::Error;
use uefi_specs::uefi::boot::ACPI_20_TABLE_GUID;
use uefi_specs::uefi::boot::EFI_2_70_SYSTEM_TABLE_REVISION;
use uefi_specs::uefi::boot::EFI_RT_PROPERTIES_TABLE_GUID;
use uefi_specs::uefi::boot::EFI_SYSTEM_TABLE_SIGNATURE;
use uefi_specs::uefi::boot::EfiRtPropertiesTable;
use uefi_specs::uefi::boot::EfiSystemTable;
use uefi_specs::uefi::boot::LINUX_EFI_MEMRESERVE_TABLE_GUID;
use uefi_specs::uefi::boot::LinuxEfiMemreserve;
use uefi_specs::uefi::boot::SMBIOS3_TABLE_GUID;
use zerocopy::IntoBytes;

/// Serialized metadata and its system table address.
#[derive(Debug)]
pub struct EfiMetadata {
    /// Metadata starting with the EFI system table, with no memory map prefix.
    pub data: Vec<u8>,
    /// Guest physical address of the EFI system table.
    pub systab_addr: u64,
}

/// Errors constructing synthetic EFI metadata.
#[derive(Debug, Error)]
pub enum Error {
    /// A guest address or serialized size cannot be represented.
    #[error("EFI metadata address or size overflow")]
    AddressOverflow,
    /// The SMBIOS identity cannot fit in the entry point's size field.
    #[error("SMBIOS identity is too large")]
    SmbiosTooLarge,
}

struct Builder {
    base: u64,
    data: Vec<u8>,
}

impl Builder {
    fn address(&self) -> Result<u64, Error> {
        self.base
            .checked_add(u64::try_from(self.data.len()).map_err(|_| Error::AddressOverflow)?)
            .ok_or(Error::AddressOverflow)
    }

    fn append(&mut self, bytes: &[u8]) -> Result<u64, Error> {
        let address = self.address()?;
        address
            .checked_add(u64::try_from(bytes.len()).map_err(|_| Error::AddressOverflow)?)
            .ok_or(Error::AddressOverflow)?;
        self.data.extend_from_slice(bytes);
        Ok(address)
    }

    fn align(&mut self, alignment: u64) -> Result<(), Error> {
        let address = self.address()?;
        let padding = address.wrapping_neg() & (alignment - 1);
        self.append(&[0; 16][..padding as usize])?;
        Ok(())
    }
}

/// Builds EFI metadata at `base_gpa`, with an external ACPI RSDP pointer.
///
/// All other nonzero pointers refer to data in the returned blob. The caller
/// must check that the variable-length blob fits its reserved memory region.
pub fn build_metadata(
    base_gpa: u64,
    rsdp_gpa: u64,
    smbios: &smbios::SmbiosTables<'_>,
) -> Result<EfiMetadata, Error> {
    // Bound the strings before calling the SMBIOS builder, whose entry point
    // has a u32 size field. 256 bytes cover all formatted areas and terminators.
    let mut smbios_bound = 256u32;
    for value in [
        smbios.bios.vendor,
        smbios.bios.version,
        smbios.bios.release_date,
        smbios.system.manufacturer,
        smbios.system.product_name,
        smbios.system.version,
        smbios.system.serial_number,
        smbios.system.sku_number,
        smbios.system.family,
    ] {
        let length = value.split('\0').next().unwrap_or("").len();
        smbios_bound = smbios_bound
            .checked_add(u32::try_from(length).map_err(|_| Error::SmbiosTooLarge)?)
            .ok_or(Error::SmbiosTooLarge)?;
    }

    let mut builder = Builder {
        base: base_gpa,
        data: Vec::new(),
    };
    builder.append(&[0; size_of::<EfiSystemTable>()])?;
    const CONFIG_ENTRY_SIZE: usize = 24;
    const NUM_CONFIG_ENTRIES: usize = 4;
    let config_offset = builder.data.len();
    let config_table_addr = builder.append(&[0; CONFIG_ENTRY_SIZE * NUM_CONFIG_ENTRIES])?;
    let vendor: Vec<u8> = "OpenVMM\0"
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    let fw_vendor_addr = builder.append(&vendor)?;

    builder.align(8)?;
    let rt_props_addr = builder.append(EfiRtPropertiesTable::NONE_SUPPORTED.as_bytes())?;
    // Linux drivers append persistent reservations here, including ITS tables.
    builder.align(8)?;
    let memreserve_addr = builder.append(LinuxEfiMemreserve::default().as_bytes())?;
    builder.align(16)?;
    let smbios_ep_offset = builder.data.len();
    let smbios_ep_addr = builder.append(&[0; smbios::ENTRY_POINT_SIZE])?;
    builder.align(16)?;
    let smbios_table_addr = builder.address()?;
    let built_smbios = smbios::build(smbios, smbios_table_addr);
    builder.append(&built_smbios.structure_table)?;
    builder.data[smbios_ep_offset..smbios_ep_offset + smbios::ENTRY_POINT_SIZE]
        .copy_from_slice(&built_smbios.entry_point);

    for (entry, (guid, address)) in builder.data
        [config_offset..config_offset + CONFIG_ENTRY_SIZE * NUM_CONFIG_ENTRIES]
        .chunks_exact_mut(CONFIG_ENTRY_SIZE)
        .zip([
            (ACPI_20_TABLE_GUID, rsdp_gpa),
            (EFI_RT_PROPERTIES_TABLE_GUID, rt_props_addr),
            (SMBIOS3_TABLE_GUID, smbios_ep_addr),
            (LINUX_EFI_MEMRESERVE_TABLE_GUID, memreserve_addr),
        ])
    {
        entry[..16].copy_from_slice(guid.as_bytes());
        entry[16..].copy_from_slice(&address.to_le_bytes());
    }

    let mut systab = EfiSystemTable {
        signature: EFI_SYSTEM_TABLE_SIGNATURE,
        revision: EFI_2_70_SYSTEM_TABLE_REVISION,
        header_size: u32::try_from(size_of::<EfiSystemTable>())
            .map_err(|_| Error::AddressOverflow)?,
        firmware_vendor: fw_vendor_addr,
        firmware_revision: 1,
        number_of_table_entries: NUM_CONFIG_ENTRIES as u64,
        configuration_table: config_table_addr,
        ..Default::default()
    };
    // UEFI 4.2: hash header_size bytes with the CRC field zeroed.
    systab.crc32 = crc32fast::hash(systab.as_bytes());
    builder.data[..size_of::<EfiSystemTable>()].copy_from_slice(systab.as_bytes());
    Ok(EfiMetadata {
        data: builder.data,
        systab_addr: base_gpa,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn read_u64(data: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
    }

    fn offset(data: &[u8], field: usize, base: u64) -> usize {
        usize::try_from(read_u64(data, field) - base).unwrap()
    }

    #[test]
    fn system_table_crc_and_services() {
        let metadata = build_metadata(0x1_0000_0000, 0x2000, &Default::default()).unwrap();
        let mut header = metadata.data[..size_of::<EfiSystemTable>()].to_vec();
        assert_eq!(read_u64(&header, 0), EFI_SYSTEM_TABLE_SIGNATURE);
        assert_eq!(
            u32::from_le_bytes(header[8..12].try_into().unwrap()),
            EFI_2_70_SYSTEM_TABLE_REVISION
        );
        assert_eq!(
            u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize,
            header.len()
        );
        let crc = u32::from_le_bytes(header[16..20].try_into().unwrap());
        header[16..20].fill(0);
        assert_eq!(crc32fast::hash(&header), crc);
        assert!(header[40..104].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn configuration_layout_and_smbios() {
        // Include an unaligned base to check physical, not offset, alignment.
        for base in [0x1000, 0x1_0000_0003] {
            let metadata = build_metadata(base, 0x1234_5000, &Default::default()).unwrap();
            assert_eq!(metadata.systab_addr, base);
            let data = &metadata.data;
            assert_eq!(read_u64(data, 104), 4);
            let config = offset(data, 112, base);
            assert_eq!(config, size_of::<EfiSystemTable>());
            let vendor = offset(data, 24, base);
            let expected: Vec<u8> = "OpenVMM\0"
                .encode_utf16()
                .flat_map(|c| c.to_le_bytes())
                .collect();
            assert_eq!(&data[vendor..vendor + expected.len()], expected);
            let mut pointers = Vec::new();
            for (index, guid) in [
                ACPI_20_TABLE_GUID,
                EFI_RT_PROPERTIES_TABLE_GUID,
                SMBIOS3_TABLE_GUID,
                LINUX_EFI_MEMRESERVE_TABLE_GUID,
            ]
            .iter()
            .enumerate()
            {
                let entry = config + index * CONFIG_ENTRY_SIZE;
                assert_eq!(&data[entry..entry + 16], guid.as_bytes());
                pointers.push(read_u64(data, entry + 16));
            }
            assert_eq!(pointers[0], 0x1234_5000);
            for &pointer in &pointers[1..] {
                assert!(pointer >= base && pointer < base + data.len() as u64);
                assert_eq!(pointer % 8, 0);
            }
            let rt = (pointers[1] - base) as usize;
            assert_eq!(
                &data[rt..rt + size_of::<EfiRtPropertiesTable>()],
                EfiRtPropertiesTable::NONE_SUPPORTED.as_bytes()
            );
            let memreserve = (pointers[3] - base) as usize;
            assert_eq!(
                &data[memreserve..memreserve + size_of::<LinuxEfiMemreserve>()],
                LinuxEfiMemreserve::default().as_bytes()
            );
            assert_eq!(pointers[2] % 16, 0);
            let ep = (pointers[2] - base) as usize;
            let ep = &data[ep..ep + smbios::ENTRY_POINT_SIZE];
            assert_eq!(&ep[..5], b"_SM3_");
            assert_eq!(ep[6], smbios::ENTRY_POINT_SIZE as u8);
            assert_eq!(&ep[7..9], &[3, 1]);
            assert_eq!(ep.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)), 0);
            let table = offset(ep, 16, base);
            assert_eq!(read_u64(ep, 16) % 16, 0);
            let table_size = u32::from_le_bytes(ep[12..16].try_into().unwrap()) as usize;
            assert_eq!(table + table_size, data.len());
            let expected = smbios::build(&Default::default(), base + table as u64);
            assert_eq!(&data[table..], expected.structure_table);
            let date = b"06/19/2026\0";
            assert!(data[table..].windows(date.len()).any(|bytes| bytes == date));
        }
    }

    const CONFIG_ENTRY_SIZE: usize = 24;

    #[test]
    fn address_overflow_is_an_error() {
        let tables = smbios::SmbiosTables::default();
        let size = build_metadata(0, 0, &tables).unwrap().data.len() as u64;
        for base in [u64::MAX, u64::MAX - 127, u64::MAX - size + 1] {
            assert!(matches!(
                build_metadata(base, 0, &tables),
                Err(Error::AddressOverflow)
            ));
        }
    }

    #[test]
    fn metadata_can_span_pages() {
        let vendor = "v".repeat(8192);
        let mut tables = smbios::SmbiosTables::default();
        tables.bios.vendor = &vendor;
        let metadata = build_metadata(0x1000, 0, &tables).unwrap();
        assert!(metadata.data.len() > 8192);
    }
}
