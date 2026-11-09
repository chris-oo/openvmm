// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM hypervisor backend.

#![cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]

use anyhow::Context as _;
use hypervisor_resources::HypervisorKind;
use hypervisor_resources::KvmHandle;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// KVM probe for auto-detection.
pub struct KvmProbe;

impl hypervisor_resources::HypervisorProbe for KvmProbe {
    fn name(&self) -> &str {
        "kvm"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        let kvm = match open_kvm() {
            Ok(kvm) => kvm,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(Some(KvmHandle { kvm: kvm.into() }.into_resource()))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        validate_parameters(params)?;
        let kvm = open_kvm().context("KVM is not available")?;
        Ok(KvmHandle { kvm: kvm.into() }.into_resource())
    }
}

fn open_kvm() -> std::io::Result<fs_err::File> {
    fs_err::File::options()
        .read(true)
        .write(true)
        .open("/dev/kvm")
}

fn validate_parameters(params: &[(&str, &str)]) -> anyhow::Result<()> {
    if let Some(&(key, value)) = params.first() {
        if key == "cca-abi" {
            anyhow::bail!(
                "KVM parameter cca-abi is retired; only the current CCA ABI is supported"
            );
        }
        anyhow::bail!("unsupported KVM parameter {key}={value}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn retired_abi_selectors_fail_before_opening_kvm() {
        validate_parameters(&[]).unwrap();
        for value in ["linux-cca74", "legacy", "latest"] {
            assert!(
                validate_parameters(&[("cca-abi", value)])
                    .unwrap_err()
                    .to_string()
                    .contains("retired")
            );
        }
        assert!(validate_parameters(&[("unknown", "true")]).is_err());
    }
}
