use hcl::GuestVtl;
use hcl::ioctl::ProcessorRunner;
use hcl::ioctl::tdx::Tdx;
use inspect::Inspect;
use thiserror::Error;
use x86defs::vmx::VmcsField;

/// Registers that can be virtual and shadowed.
#[derive(Debug, Inspect)]
pub(super) enum ShadowedRegister {
    Cr0,
    Cr4,
}

impl ShadowedRegister {
    const fn name(&self) -> &'static str {
        match self {
            Self::Cr0 => "cr0",
            Self::Cr4 => "cr4",
        }
    }

    const fn physical_vmcs_field(&self) -> VmcsField {
        match self {
            Self::Cr0 => VmcsField::VMX_VMCS_GUEST_CR0,
            Self::Cr4 => VmcsField::VMX_VMCS_GUEST_CR4,
        }
    }

    const fn shadow_vmcs_field(&self) -> VmcsField {
        match self {
            Self::Cr0 => VmcsField::VMX_VMCS_CR0_READ_SHADOW,
            Self::Cr4 => VmcsField::VMX_VMCS_CR4_READ_SHADOW,
        }
    }

    pub(super) const fn guest_owned_mask(&self) -> u64 {
        // Control register bits that are guest owned by default. A bit is guest
        // owned when the physical register bit is always set to the virtual
        // register bit (subject to validation of the virtual register).
        match self {
            Self::Cr0 => {
                x86defs::X64_CR0_ET
                    | x86defs::X64_CR0_MP
                    | x86defs::X64_CR0_EM
                    | x86defs::X64_CR0_TS
                    | x86defs::X64_CR0_WP
                    | x86defs::X64_CR0_AM
                    | x86defs::X64_CR0_PE
                    | x86defs::X64_CR0_PG
            }
            Self::Cr4 => {
                x86defs::X64_CR4_VME
                    | x86defs::X64_CR4_PVI
                    | x86defs::X64_CR4_TSD
                    | x86defs::X64_CR4_DE
                    | x86defs::X64_CR4_PSE
                    | x86defs::X64_CR4_PAE
                    | x86defs::X64_CR4_PGE
                    | x86defs::X64_CR4_PCE
                    | x86defs::X64_CR4_FXSR
                    | x86defs::X64_CR4_XMMEXCPT
                    | x86defs::X64_CR4_UMIP
                    | x86defs::X64_CR4_LA57
                    | x86defs::X64_CR4_RWFSGS
                    | x86defs::X64_CR4_PCIDE
                    | x86defs::X64_CR4_OSXSAVE
                    | x86defs::X64_CR4_SMEP
                    | x86defs::X64_CR4_SMAP
                    | x86defs::X64_CR4_CET
            }
        }
    }
}

trait VmcsAccess {
    fn write_vmcs64(&mut self, vtl: GuestVtl, field: VmcsField, mask: u64, value: u64);
    fn read_vmcs64(&self, vtl: GuestVtl, field: VmcsField) -> u64;
}

/// A virtual register that is shadowed by the virtstack.
///
/// Some bits are owned by the guest while others are owned by the virtstack,
/// due to TDX requirements.
#[derive(Inspect)]
pub(super) struct VirtualRegister {
    /// The register being shadowed.
    register: ShadowedRegister,
    /// The VTL this register is shadowed for.
    vtl: GuestVtl,
    /// The value the guest sees.
    shadow_value: u64,
    /// Additional constraints on bits.
    allowed_bits: Option<u64>,
}

#[derive(Debug, Error)]
pub(super) enum VirtualRegisterError {
    #[error("invalid value {0} for register {1}")]
    InvalidValue(u64, &'static str),
}

impl VirtualRegister {
    pub(super) fn new(
        reg: ShadowedRegister,
        vtl: GuestVtl,
        initial_value: u64,
        allowed_bits: Option<u64>,
    ) -> Self {
        Self {
            register: reg,
            vtl,
            shadow_value: initial_value,
            allowed_bits,
        }
    }

    /// Write a new value to the virtual register. This updates host owned bits
    /// in the shadowed value, and updates guest owned bits in the physical
    /// register in the vmcs.
    pub(super) fn write<'a>(
        &mut self,
        value: u64,
        runner: &mut ProcessorRunner<'a, Tdx<'a>>,
    ) -> Result<(), VirtualRegisterError> {
        tracing::trace!(?self.register, value, "write virtual register");

        if value & !self.allowed_bits.unwrap_or(u64::MAX) != 0 {
            return Err(VirtualRegisterError::InvalidValue(
                value,
                self.register.name(),
            ));
        }

        // If guest owned bits of the physical register have changed, then update
        // the guest owned bits of the physical field.
        let old_physical_reg = runner.read_vmcs64(self.vtl, self.register.physical_vmcs_field());

        tracing::trace!(old_physical_reg, "old_physical_reg");

        let guest_owned_mask = self.register.guest_owned_mask();
        if (old_physical_reg ^ value) & guest_owned_mask != 0 {
            let new_physical_reg =
                (old_physical_reg & !guest_owned_mask) | (value & guest_owned_mask);

            tracing::trace!(new_physical_reg, "new_physical_reg");

            runner.write_vmcs64(
                self.vtl,
                self.register.physical_vmcs_field(),
                !0,
                new_physical_reg,
            );
        }

        self.shadow_value = value;
        runner.write_vmcs64(self.vtl, self.register.shadow_vmcs_field(), !0, value);
        Ok(())
    }

    pub(super) fn read<'a>(&self, runner: &ProcessorRunner<'a, Tdx<'a>>) -> u64 {
        let physical_reg = runner.read_vmcs64(self.vtl, self.register.physical_vmcs_field());

        // Get the bits owned by the host from the shadow and the bits owned by the
        // guest from the physical value.
        let guest_owned_mask = self.register.guest_owned_mask();
        (self.shadow_value & !self.register.guest_owned_mask()) | (physical_reg & guest_owned_mask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x86defs::vmx::VmcsField;

    struct TestVmcsAccess {
        cr0: u64,
        cr4: u64,
    }

    impl VmcsAccess for TestVmcsAccess {
        fn write_vmcs64(&mut self, _vtl: GuestVtl, field: VmcsField, mask: u64, value: u64) {
            assert_eq!(mask, !0);
            match field {
                VmcsField::VMX_VMCS_GUEST_CR0 => self.cr0 = value,
                VmcsField::VMX_VMCS_GUEST_CR4 => self.cr4 = value,
                _ => panic!("unexpected vmcs field"),
            }
        }

        fn read_vmcs64(&self, _vtl: GuestVtl, field: VmcsField) -> u64 {
            match field {
                VmcsField::VMX_VMCS_GUEST_CR0 => self.cr0,
                VmcsField::VMX_VMCS_GUEST_CR4 => self.cr4,
                _ => panic!("unexpected vmcs field"),
            }
        }
    }

    // #[test]
    // fn test_virtual_register() {
    //     let mut vmcs = TestVmcsAccess { cr0: 0, cr4: 0 };
    //     let mut reg = VirtualRegister::new(
    //         ShadowedRegister::Cr0,
    //         GuestVtl::Vtl0,
    //         0,
    //         Some(x86defs::X64_CR0_PE | x86defs::X64_CR0_PG),
    //     );

    //     reg.write(0, &mut vmcs).unwrap();
    //     assert_eq!(reg.read(&vmcs), 0);

    //     reg.write(1, &mut runner).unwrap();
    //     assert_eq!(reg.read(&runner), 1);
    // }
}
