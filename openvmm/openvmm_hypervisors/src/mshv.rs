// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSHV hypervisor backend.

#![cfg(all(target_os = "linux", feature = "virt_mshv", guest_is_native))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::MshvConfig;
use hypervisor_resources::MshvHandle;
use vm_resource::IntoResource;
use vm_resource::Resource;

fn parse_mshv_params(params: &[(&str, &str)]) -> anyhow::Result<MshvConfig> {
    MshvConfig::parse(params)
}

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
                scoped_host_access: false,
            }
            .into_resource(),
        ))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        let config = parse_mshv_params(params)?;
        anyhow::ensure!(virt_mshv::is_available()?, "MSHV is not available");
        Ok(Resource::new(MshvHandle {
            mshv: fs_err::File::open("/dev/mshv")?.into(),
            snp_disable_cpuid_offload: config.snp_disable_cpuid_offload,
            scoped_host_access: config.scoped_host_access,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn parses_snp_cpuid_offload_parameter() {
        assert!(!parse_mshv_params(&[]).unwrap().snp_disable_cpuid_offload);
        assert!(
            parse_mshv_params(&[("snp_disable_cpuid_offload", "true")])
                .unwrap()
                .snp_disable_cpuid_offload
        );
        assert!(
            !parse_mshv_params(&[("snp_disable_cpuid_offload", "false")])
                .unwrap()
                .snp_disable_cpuid_offload
        );
        assert!(parse_mshv_params(&[("unknown", "true")]).is_err());
    }

    #[test]
    fn parses_scoped_host_access_parameter() {
        assert!(!parse_mshv_params(&[]).unwrap().scoped_host_access);
        let config = parse_mshv_params(&[
            ("snp_scoped_host_access", "true"),
            ("snp_disable_cpuid_offload", "false"),
        ])
        .unwrap();
        assert!(config.scoped_host_access);
        assert!(!config.snp_disable_cpuid_offload);
        assert!(
            !parse_mshv_params(&[("snp_scoped_host_access", "false")])
                .unwrap()
                .scoped_host_access
        );
        assert!(parse_mshv_params(&[("snp_scoped_host_access", "invalid")]).is_err());
        assert!(parse_mshv_params(&[("scoped_host_access", "true")]).is_err());
    }
}
