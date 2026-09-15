// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Typed access to the KVM/VFIO bridge.

use crate::Device;
use crate::Error;
use crate::KVM_DEV_VFIO_FILE;
use crate::KVM_DEV_VFIO_FILE_ADD;
use crate::KVM_DEV_VFIO_FILE_DEL;
use crate::Partition;
use crate::Result;
use crate::kvm_device_type_KVM_DEV_TYPE_VFIO;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;

impl Partition {
    /// Create the single KVM/VFIO bridge for this VM.
    ///
    /// The returned device fd holds a kernel reference to the VM. A second
    /// bridge cannot be created while this one exists. This is not a capability
    /// probe and does not associate any VFIO files.
    pub fn create_vfio_device(&self) -> Result<VfioDevice> {
        self.create_device(kvm_device_type_KVM_DEV_TYPE_VFIO, 0)
            .map(VfioDevice)
            .map_err(Error::CreateVfioDevice)
    }
}

/// An owned KVM/VFIO bridge, distinct from arbitrary KVM devices.
///
/// Closing its fd releases its kernel-held VFIO file associations. This is
/// not a substitute for stopping DMA and tearing down IOMMUFD objects first.
/// The higher-level owner must retain the VM's userspace resources until
/// assigned devices can no longer access them.
pub struct VfioDevice(Device);

impl VfioDevice {
    /// Associate a VFIO group or cdev file with this VM.
    ///
    /// For a Realm cdev, call this before `VFIO_DEVICE_BIND_IOMMUFD` so that
    /// IOMMUFD receives the KVM association. KVM retains its own reference to
    /// the file; this method neither consumes nor closes the caller's fd.
    ///
    /// Keep a descriptor for the same open file description until
    /// [`Self::remove_file`]. Duplicate registration errors are not ignored.
    pub fn add_file(&self, file: BorrowedFd<'_>) -> Result<()> {
        let fd = file.as_raw_fd();
        // SAFETY: the bridge has the VFIO device type. This attribute reads
        // exactly an int32_t fd, borrowed and live for the duration of the ioctl.
        unsafe {
            self.0
                .set_device_attr(KVM_DEV_VFIO_FILE, KVM_DEV_VFIO_FILE_ADD, &fd, 0)
        }
        .map_err(Error::AddVfioFile)
    }

    /// Remove the association for the same VFIO open file description.
    ///
    /// Stop DMA and release dependent IOMMUFD objects before removing a Realm
    /// association. All errors, including a missing association, are returned;
    /// failure does not establish that the association has been removed.
    pub fn remove_file(&self, file: BorrowedFd<'_>) -> Result<()> {
        let fd = file.as_raw_fd();
        // SAFETY: as for add_file, with the documented FILE_DEL attribute.
        unsafe {
            self.0
                .set_device_attr(KVM_DEV_VFIO_FILE, KVM_DEV_VFIO_FILE_DEL, &fd, 0)
        }
        .map_err(Error::RemoveVfioFile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvm_create_device;
    use crate::kvm_device_attr;
    use nix::errno::Errno;
    use std::fs::File;
    use std::mem::offset_of;
    use std::os::fd::AsFd;
    use std::os::fd::RawFd;
    use test_with_tracing::test;

    #[test]
    fn vfio_bridge_abi_matches_linux() {
        assert_eq!(kvm_device_type_KVM_DEV_TYPE_VFIO, 4);
        assert_eq!(KVM_DEV_VFIO_FILE, 1);
        assert_eq!(KVM_DEV_VFIO_FILE_ADD, 1);
        assert_eq!(KVM_DEV_VFIO_FILE_DEL, 2);
        assert_eq!(size_of::<RawFd>(), 4);
        assert_eq!(size_of::<kvm_create_device>(), 12);
        assert_eq!(offset_of!(kvm_create_device, type_), 0);
        assert_eq!(offset_of!(kvm_create_device, fd), 4);
        assert_eq!(offset_of!(kvm_create_device, flags), 8);
        assert_eq!(size_of::<kvm_device_attr>(), 24);
        assert_eq!(offset_of!(kvm_device_attr, flags), 0);
        assert_eq!(offset_of!(kvm_device_attr, group), 4);
        assert_eq!(offset_of!(kvm_device_attr, attr), 8);
        assert_eq!(offset_of!(kvm_device_attr, addr), 16);
    }

    #[test]
    fn creation_failure_is_not_a_device_handle() {
        let partition = Partition {
            vm: File::open("/dev/null").unwrap(),
            vps: Vec::new(),
            mmap_size: 0,
        };
        assert!(matches!(
            partition.create_vfio_device(),
            Err(Error::CreateVfioDevice(Errno::ENOTTY))
        ));
    }

    #[test]
    fn file_operations_preserve_errno_and_borrow_the_fd() {
        let bridge = VfioDevice(Device(File::open("/dev/null").unwrap()));
        let file = File::open("/dev/null").unwrap();
        assert!(matches!(
            bridge.add_file(file.as_fd()),
            Err(Error::AddVfioFile(Errno::ENOTTY))
        ));
        assert!(matches!(
            bridge.remove_file(file.as_fd()),
            Err(Error::RemoveVfioFile(Errno::ENOTTY))
        ));
        drop(bridge);
        file.metadata().unwrap();
    }
}
