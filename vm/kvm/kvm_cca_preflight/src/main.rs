// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM Arm CCA host capability preflight probe.

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn main() -> anyhow::Result<()> {
    let kvm = kvm::Kvm::new()?;
    println!("opened /dev/kvm");

    let arm_rmi = query_kvm_cap(&kvm, kvm::KVM_CAP_ARM_RMI_UAPI, "KVM_CAP_ARM_RMI")?;
    if arm_rmi == 0 {
        let v14 = query_kvm_cap(&kvm, kvm::KVM_CAP_ARM_RMI_V14_UAPI, "KVM_CAP_ARM_RMI_V14")?;
        anyhow::ensure!(
            v14 == 0,
            "host exposes the KVM CCA v14 capability; a v15 kernel is required"
        );
        anyhow::bail!("missing required KVM capability KVM_CAP_ARM_RMI");
    }
    check_kvm_cap(&kvm, kvm::KVM_CAP_GUEST_MEMFD, "KVM_CAP_GUEST_MEMFD")?;

    let host_ipa_bits = match kvm
        .check_extension(kvm::KVM_CAP_ARM_VM_IPA_SIZE)
        .map_err(kvm::Error::CheckExtension)?
    {
        bits if bits > 0 => bits as u8,
        _ => 40,
    };
    println!("KVM_CAP_ARM_VM_IPA_SIZE(host)={host_ipa_bits}");

    let (realm, ipa_bits) = kvm.new_realm_vm_with_max_ipa_size(host_ipa_bits)?;
    println!("created Realm VM with IPA size {ipa_bits}");

    check_partition_cap(&realm, kvm::KVM_CAP_USER_MEMORY2, "KVM_CAP_USER_MEMORY2")?;
    check_partition_cap(&realm, kvm::KVM_CAP_GUEST_MEMFD, "KVM_CAP_GUEST_MEMFD")?;
    let memory_attributes = check_partition_cap(
        &realm,
        kvm::KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES_UAPI,
        "KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES",
    )?;
    anyhow::ensure!(
        memory_attributes as u64 & kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64 != 0,
        "KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES does not include KVM_MEMORY_ATTRIBUTE_PRIVATE"
    );
    println!("Realm VM private memory extensions are available");

    realm.test_create_device(kvm::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3)?;
    println!("VGICv3 device creation is available");

    println!("KVM CCA preflight passed");
    Ok(())
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn check_kvm_cap(kvm: &kvm::Kvm, cap: u32, name: &'static str) -> anyhow::Result<()> {
    let value = query_kvm_cap(kvm, cap, name)?;
    anyhow::ensure!(value != 0, "missing required KVM capability {name}");
    Ok(())
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn query_kvm_cap(kvm: &kvm::Kvm, cap: u32, name: &'static str) -> anyhow::Result<i32> {
    let value = kvm
        .check_extension(cap)
        .map_err(kvm::Error::CheckExtension)?;
    println!("{name}={value:#x}");
    Ok(value)
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn check_partition_cap(kvm: &kvm::Partition, cap: u32, name: &'static str) -> anyhow::Result<i32> {
    let value = kvm
        .check_extension(cap)
        .map_err(kvm::Error::CheckExtension)?;
    println!("{name}={value:#x}");
    anyhow::ensure!(value != 0, "missing required KVM capability {name}");
    Ok(value)
}

#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
fn main() {
    eprintln!("kvm_cca_preflight must run on aarch64 Linux");
    std::process::exit(2);
}
