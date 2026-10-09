// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtio-net devices.

use crate::Device;
use async_trait::async_trait;
use net_backend::resolve::ResolveEndpointParams;
use net_backend_resources::consomme::ConsommeHandle;
use net_backend_resources::null::NullHandle;
#[cfg(target_os = "linux")]
use net_backend_resources::tap::TapHandle;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::net::VirtioNetHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceId;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-pmem devices.
pub struct VirtioNetResolver;

fn validate_owned_endpoint(id: &str) -> anyhow::Result<()> {
    if id == ConsommeHandle::ID || id == NullHandle::ID {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if id == TapHandle::ID {
        return Ok(());
    }
    anyhow::bail!("owned virtio-net staging does not support endpoint resource '{id}'")
}

declare_static_async_resolver! {
    VirtioNetResolver,
    (VirtioDeviceHandle, VirtioNetHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VirtioNetHandle> for VirtioNetResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioNetHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let mut builder = Device::builder();
        if resource.bounce_io {
            validate_owned_endpoint(resource.endpoint.id())?;
            builder = builder.bounce_io(true);
        }
        if let Some(max_queues) = resource.max_queues {
            builder = builder.max_queues(max_queues);
        }

        let endpoint = resolver
            .resolve(
                resource.endpoint,
                ResolveEndpointParams {
                    mac_address: resource.mac_address,
                },
            )
            .await?;

        let device = builder.build(input.driver_source, endpoint.0, resource.mac_address)?;

        Ok(device.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn owned_endpoint_admission() {
        validate_owned_endpoint(ConsommeHandle::ID).unwrap();
        validate_owned_endpoint(NullHandle::ID).unwrap();
        #[cfg(target_os = "linux")]
        validate_owned_endpoint(TapHandle::ID).unwrap();
        for id in ["mana", "dio", "vfio", "vhost", "external", "unknown"] {
            assert!(validate_owned_endpoint(id).is_err(), "{id}");
        }
    }
}
