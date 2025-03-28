//! TODO

#![allow(unsafe_code)]

mod raw {
    /// TODO: Lifted from uefi_raw because it's not in a crates.io release yet.
    ///
    /// ABI-compatible UEFI boolean.
    ///
    /// This is similar to a `bool`, but allows values other than 0 or 1 to be
    /// stored without it being undefined behavior.
    ///
    /// Any non-zero value is treated as logically `true`.
    #[derive(Copy, Clone, Debug, Default, PartialEq, Ord, PartialOrd, Eq, Hash)]
    #[repr(transparent)]
    pub struct Boolean(pub u8);

    impl Boolean {
        /// [`Boolean`] representing `true`.
        pub const TRUE: Self = Self(1);

        /// [`Boolean`] representing `false`.
        pub const FALSE: Self = Self(0);
    }

    impl From<bool> for Boolean {
        fn from(value: bool) -> Self {
            match value {
                true => Self(1),
                false => Self(0),
            }
        }
    }

    impl From<Boolean> for bool {
        #[allow(clippy::match_like_matches_macro)]
        fn from(value: Boolean) -> Self {
            // We handle it as in C: Any bit-pattern != 0 equals true
            match value.0 {
                0 => false,
                _ => true,
            }
        }
    }
}

mod ivm_protocol {
    use crate::raw::Boolean;
    use core::ffi::c_void;
    use uefi::Guid;
    use uefi::Status;
    use uefi::guid;
    use uefi::proto::unsafe_protocol;

    // typedef struct _EFI_HV_PROTECTION_OBJECT *EFI_HV_PROTECTION_HANDLE;
    pub type EfiHvProtectionHandle = *mut c_void;

    #[derive(Debug)]
    #[repr(C)]
    struct IvmProtocol {
        pub make_address_range_host_visible: unsafe extern "efiapi" fn(
            *mut Self,
            hv_map_gpa_flags: u32,
            base_address: usize, // is void*
            byte_count: u32,
            zero_pages: Boolean,
            protection_handle: *mut EfiHvProtectionHandle, // is EFI_HV_PROTECTION_HANDLE out*
        ) -> Status,
        pub make_address_range_not_host_visible: unsafe extern "efiapi" fn(
            *mut Self,
            protection_handle: EfiHvProtectionHandle,
        ) -> Status,
    }

    impl IvmProtocol {
        pub const GUID: Guid = guid!("c40a31b5-3899-4f76-bf7e-3295833feee7");
    }

    #[derive(Debug)]
    #[repr(transparent)]
    #[unsafe_protocol(IvmProtocol::GUID)]
    struct Ivm(IvmProtocol);
}

fn main() {
    let initial_cr0: u64;
    unsafe {
        std::arch::asm! {
            "mov {r}, cr0",
            r = out(reg) initial_cr0,
        }
    }
    println!("\rInitial CR0: {:#x}", initial_cr0);

    let new_cr0 = initial_cr0 & !0x10u64 | 0x40u64;
    println!("\rSetting CR0 to: {:#x}", new_cr0);

    let read_cr0: u64;
    unsafe {
        std::arch::asm! {
            "mov cr0, {r}",
            "mov {r}, cr0",
            r = inout(reg) new_cr0 => read_cr0,
        }
    }
    println!("\rRead CR0: {:#x}", read_cr0);
}
