// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM Arm CCA host capability preflight probe.

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn main() -> anyhow::Result<()> {
    let kvm = kvm::Kvm::new()?;
    println!("opened /dev/kvm");

    check_kvm_cap(&kvm, kvm::KVM_CAP_ARM_RMI_UAPI, "KVM_CAP_ARM_RMI")?;
    check_kvm_cap(&kvm, kvm::KVM_CAP_GUEST_MEMFD, "KVM_CAP_GUEST_MEMFD")?;

    let memory_attributes = kvm
        .check_extension(kvm::KVM_CAP_MEMORY_ATTRIBUTES)
        .map_err(kvm::Error::CheckExtension)?;
    println!("KVM_CAP_MEMORY_ATTRIBUTES={memory_attributes:#x}");
    anyhow::ensure!(
        memory_attributes & kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as i32 != 0,
        "KVM_CAP_MEMORY_ATTRIBUTES does not include KVM_MEMORY_ATTRIBUTE_PRIVATE"
    );

    let ipa_bits = match kvm
        .check_extension(kvm::KVM_CAP_ARM_VM_IPA_SIZE)
        .map_err(kvm::Error::CheckExtension)?
    {
        bits if bits > 0 => bits as u8,
        _ => 40,
    };
    println!("KVM_CAP_ARM_VM_IPA_SIZE={ipa_bits}");

    let realm = kvm.new_aarch64_vm(kvm::Aarch64VmType::Realm { ipa_bits })?;
    println!("created Realm VM with IPA size {ipa_bits}");

    realm.check_private_memory_extensions()?;
    println!("Realm VM private memory extensions are available");

    realm.test_create_device(kvm::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3)?;
    println!("VGICv3 device creation is available");

    println!("KVM CCA preflight passed");
    Ok(())
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn check_kvm_cap(kvm: &kvm::Kvm, cap: u32, name: &'static str) -> anyhow::Result<()> {
    let value = kvm
        .check_extension(cap)
        .map_err(kvm::Error::CheckExtension)?;
    println!("{name}={value:#x}");
    anyhow::ensure!(value != 0, "missing required KVM capability {name}");
    Ok(())
}

#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
fn main() {
    eprintln!("kvm_cca_preflight must run on aarch64 Linux");
    std::process::exit(2);
}
