//! DRM device management

use std::ffi::{CStr, OsStr, c_int};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

use log::info;

#[allow(non_camel_case_types, dead_code)]
mod bindings {
    use std::ffi::{c_char, c_int};

    pub const DRM_NODE_PRIMARY: usize = 0;

    /// Partial `drmDevice` layout — only the fields we read.
    #[repr(C)]
    pub struct drmDevice {
        pub nodes: *mut *mut c_char,
        pub available_nodes: c_int,
    }

    pub type drmDevicePtr = *mut drmDevice;

    unsafe extern "C" {
        pub fn drmGetDevices2(
            flags: u32,
            devices: *mut drmDevicePtr,
            max_devices: c_int,
        ) -> c_int;
        pub fn drmFreeDevices(devices: *mut drmDevicePtr, count: c_int);
    }
}

/// A DRM primary-node file descriptor (typically from libseat).
///
/// Modesetting will be layered on top of this later via raw DRM ioctls.
pub struct DrmDevice {
    fd: OwnedFd,
}

impl AsFd for DrmDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl DrmDevice {
    /// Wraps an owned DRM device file descriptor.
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Returns a reference to the underlying file descriptor.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Finds available DRM primary nodes via `drmGetDevices2`.
///
/// Returns paths to primary nodes (e.g. `/dev/dri/card0`).
pub fn find_drm_devices() -> anyhow::Result<Vec<PathBuf>> {
    let count = unsafe { bindings::drmGetDevices2(0, ptr::null_mut(), 0) };
    if count < 0 {
        anyhow::bail!(
            "drmGetDevices2 failed: {}",
            std::io::Error::from_raw_os_error(-count)
        );
    }
    if count == 0 {
        info!("Found 0 DRM device(s)");
        return Ok(Vec::new());
    }

    let mut device_ptrs = vec![ptr::null_mut(); count as usize];
    let count = unsafe { bindings::drmGetDevices2(0, device_ptrs.as_mut_ptr(), count) };
    if count < 0 {
        anyhow::bail!(
            "drmGetDevices2 failed: {}",
            std::io::Error::from_raw_os_error(-count)
        );
    }

    let devices = DrmDeviceList {
        devices: device_ptrs,
        count,
    };

    let mut paths = Vec::new();
    for device_ptr in devices.iter() {
        let device = unsafe { &*device_ptr };
        if device.available_nodes & (1 << bindings::DRM_NODE_PRIMARY) == 0 {
            continue;
        }

        let node = unsafe { *device.nodes.add(bindings::DRM_NODE_PRIMARY) };
        if node.is_null() {
            continue;
        }

        let path = unsafe { CStr::from_ptr(node) };
        paths.push(PathBuf::from(OsStr::from_bytes(path.to_bytes())));
    }

    paths.sort();
    info!("Found {} DRM device(s): {:?}", paths.len(), paths);
    Ok(paths)
}

/// Owns the device list returned by `drmGetDevices2`.
struct DrmDeviceList {
    devices: Vec<bindings::drmDevicePtr>,
    count: c_int,
}

impl DrmDeviceList {
    fn iter(&self) -> impl Iterator<Item = bindings::drmDevicePtr> + '_ {
        self.devices.iter().take(self.count as usize).copied()
    }
}

impl Drop for DrmDeviceList {
    fn drop(&mut self) {
        unsafe {
            bindings::drmFreeDevices(self.devices.as_mut_ptr(), self.count);
        }
    }
}
