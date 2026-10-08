// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for virtio-blk devices.

use crate::VirtioBlkDevice;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::blk::VirtioBlkBounceDiskHandle;
use virtio_resources::blk::VirtioBlkHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::IntoResource;
use vm_resource::ResourceId;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-blk devices.
pub struct VirtioBlkResolver;

fn validate_bounce_resource_id(id: &str) -> anyhow::Result<()> {
    if id != VirtioBlkBounceDiskHandle::ID {
        anyhow::bail!("bounce I/O requires a reviewed regular-file or RAM disk resource");
    }
    Ok(())
}

fn validate_bounce_file(file: &std::fs::File) -> anyhow::Result<()> {
    if !file.metadata()?.is_file() {
        anyhow::bail!("bounce I/O requires a regular file, not a host device");
    }
    #[cfg(target_os = "linux")]
    {
        use nix::fcntl::FcntlArg;
        use nix::fcntl::OFlag;
        use nix::fcntl::fcntl;
        let flags = OFlag::from_bits_retain(fcntl(file, FcntlArg::F_GETFL)?);
        if flags.contains(OFlag::O_DIRECT) {
            anyhow::bail!("bounce I/O requires a buffered file, not O_DIRECT");
        }
        if flags.contains(OFlag::O_APPEND) {
            anyhow::bail!("bounce I/O does not support O_APPEND file descriptors");
        }
    }
    Ok(())
}

declare_static_async_resolver! {
    VirtioBlkResolver,
    (VirtioDeviceHandle, VirtioBlkHandle),
    (DiskHandleKind, VirtioBlkBounceDiskHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VirtioBlkHandle> for VirtioBlkResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioBlkHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        if resource.bounce_io {
            validate_bounce_resource_id(resource.disk.id())?;
        }
        let disk = resolver
            .resolve(
                resource.disk,
                ResolveDiskParameters {
                    read_only: resource.read_only,
                    driver_source: input.driver_source,
                },
            )
            .await?;
        if resource.bounce_io && disk.0.sector_size() as usize > scsi_buffers::MAX_BOUNCE_IO_SIZE {
            anyhow::bail!("bounce I/O does not support sectors larger than the request limit");
        }

        let device = VirtioBlkDevice::new(
            input.driver_source,
            disk.0,
            resource.read_only,
            resource.serial,
        )?;
        let device = if resource.bounce_io {
            device.with_bounce_io()
        } else {
            device
        };
        Ok(device.into())
    }
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, VirtioBlkBounceDiskHandle> for VirtioBlkResolver {
    type Output = ResolvedDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioBlkBounceDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk = match resource {
            VirtioBlkBounceDiskHandle::File(file) => {
                validate_bounce_file(&file)?;
                disk_backend_resources::FileDiskHandle(file).into_resource()
            }
            VirtioBlkBounceDiskHandle::Ram { len } => {
                if len == 0 || len % 512 != 0 {
                    anyhow::bail!("bounce RAM disk length must be a positive multiple of 512");
                }
                LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                    len: Some(len),
                    sector_size: None,
                })
                .into_resource()
            }
        };
        Ok(resolver.resolve(disk, input).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_serial;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;
    use vm_resource::Resource;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

    async fn resolve_bounced_disk(
        disk: Resource<DiskHandleKind>,
        driver: DefaultDriver,
    ) -> anyhow::Result<ResolvedVirtioDevice> {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let mut resolver = ResourceResolver::new();
        resolver.add_async_resolver::<VirtioDeviceHandle, _, VirtioBlkHandle, _>(VirtioBlkResolver);
        resolver.add_async_resolver::<DiskHandleKind, _, VirtioBlkBounceDiskHandle, _>(
            VirtioBlkResolver,
        );
        Ok(resolver
            .resolve(
                VirtioBlkHandle {
                    disk,
                    read_only: false,
                    serial: None,
                    bounce_io: true,
                }
                .into_resource(),
                VirtioResolveInput {
                    driver_source: &driver_source,
                },
            )
            .await?)
    }

    #[async_test]
    async fn bounce_resolver_rejects_generic_file_before_backend_resolution(driver: DefaultDriver) {
        let disk =
            disk_backend_resources::FileDiskHandle(std::fs::File::open("Cargo.toml").unwrap())
                .into_resource();
        let error = resolve_bounced_disk(disk, driver).await.err().unwrap();
        assert!(
            format!("{error:#}")
                .contains("bounce I/O requires a reviewed regular-file or RAM disk resource"),
            "{error:#}"
        );
    }

    #[cfg(target_os = "linux")]
    #[async_test]
    async fn bounce_resolver_rejects_typed_append_file(driver: DefaultDriver) {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open("Cargo.toml")
            .unwrap();
        assert_eq!(
            validate_bounce_file(&file).unwrap_err().to_string(),
            "bounce I/O does not support O_APPEND file descriptors"
        );
        let error = resolve_bounced_disk(
            VirtioBlkBounceDiskHandle::File(file).into_resource(),
            driver,
        )
        .await
        .err()
        .unwrap();
        assert!(
            format!("{error:#}").contains("bounce I/O does not support O_APPEND file descriptors"),
            "{error:#}"
        );
    }

    #[test]
    fn bounce_rejects_generic_backing_resources() {
        validate_bounce_resource_id(VirtioBlkBounceDiskHandle::ID).unwrap();
        for id in ["file", "block", "layered", "nvme", "unknown"] {
            assert!(validate_bounce_resource_id(id).is_err(), "{id}");
        }
    }

    #[test]
    fn bounce_validates_opened_descriptor() {
        let file = std::fs::File::open("Cargo.toml").unwrap();
        validate_bounce_file(&file).unwrap();
        #[cfg(unix)]
        {
            let directory = std::fs::File::open(".").unwrap();
            assert!(validate_bounce_file(&directory).is_err());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounce_rejects_direct_io_descriptor() {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_DIRECT.bits())
            .open("Cargo.toml")
            .unwrap();
        assert!(validate_bounce_file(&file).is_err());
    }

    #[test]
    fn serial_is_nul_padded() {
        assert_eq!(
            parse_serial("DATA-DISK".into()).unwrap(),
            *b"DATA-DISK\0\0\0\0\0\0\0\0\0\0\0"
        );
        assert!(parse_serial("\u{1f},[]".into()).is_ok());
    }

    #[test]
    fn invalid_serial_is_rejected() {
        assert!(parse_serial(String::new()).is_err());
        assert!(parse_serial("123456789012345678901".into()).is_err());
        assert!(parse_serial("non-ascii-\u{e9}".into()).is_err());
    }
}
