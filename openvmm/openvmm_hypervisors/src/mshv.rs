// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSHV hypervisor backend.

#![cfg(all(target_os = "linux", feature = "virt_mshv", guest_is_native))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::MshvConfig;
use hypervisor_resources::MshvHandle;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// MSHV probe for auto-detection.
pub struct MshvProbe;

impl hypervisor_resources::HypervisorProbe for MshvProbe {
    fn name(&self) -> &str {
        "mshv"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        let mshv = match fs_err::File::open("/dev/mshv") {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(Some(
            MshvHandle {
                mshv: mshv.into(),
                snp_disable_cpuid_offload: false,
                guest_memfd_lite: false,
            }
            .into_resource(),
        ))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        let config = MshvConfig::parse(params)?;
        anyhow::ensure!(virt_mshv::is_available()?, "MSHV is not available");
        Ok(Resource::new(MshvHandle {
            mshv: fs_err::File::open("/dev/mshv")?.into(),
            snp_disable_cpuid_offload: config.snp_disable_cpuid_offload,
            guest_memfd_lite: config.guest_memfd_lite,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn parses_snp_cpuid_offload_parameter() {
        assert!(!MshvConfig::parse(&[]).unwrap().snp_disable_cpuid_offload);
        assert!(
            MshvConfig::parse(&[("snp_disable_cpuid_offload", "true")])
                .unwrap()
                .snp_disable_cpuid_offload
        );
        assert!(MshvConfig::parse(&[("snp_disable_cpuid_offload", "invalid")]).is_err());
        assert!(MshvConfig::parse(&[("unknown", "true")]).is_err());
    }

    #[test]
    fn parses_guest_memfd_lite_parameter() {
        assert!(!MshvConfig::parse(&[]).unwrap().guest_memfd_lite);
        assert!(
            MshvConfig::parse(&[("guest_memfd_lite", "yes")])
                .unwrap()
                .guest_memfd_lite
        );
        assert!(
            !MshvConfig::parse(&[("guest_memfd_lite", "false")])
                .unwrap()
                .guest_memfd_lite
        );
        assert!(MshvConfig::parse(&[("guest_memfd_lite", "invalid")]).is_err());
    }
}
