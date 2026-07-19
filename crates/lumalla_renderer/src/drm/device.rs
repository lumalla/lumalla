//! DRM device management

use std::ffi::{CStr, OsStr, c_int};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

use anyhow::Context;
use drm::Device;
use drm::control::Device as ControlDevice;
use log::{debug, info};

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

/// A DRM device wrapper that implements the drm-rs traits.
///
/// This wraps an `OwnedFd` received from libseat (which has DRM master privileges).
pub struct DrmDevice {
    fd: OwnedFd,
}

// Implement the drm-rs Device trait
impl Device for DrmDevice {}

impl AsFd for DrmDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

// Implement ControlDevice for modesetting operations
impl ControlDevice for DrmDevice {}

impl DrmDevice {
    /// Creates a new DRM device from an owned file descriptor.
    ///
    /// The fd should be obtained from libseat via `open_device()`,
    /// which grants DRM master privileges.
    pub fn from_fd(fd: OwnedFd) -> anyhow::Result<Self> {
        let device = Self { fd };

        // Verify we can access DRM resources
        let _resources = device
            .resource_handles()
            .context("Failed to get DRM resources - is this a valid DRM device?")?;

        // Check for atomic modesetting support
        device
            .set_client_capability(drm::ClientCapability::Atomic, true)
            .context("Failed to enable atomic modesetting - driver may not support it")?;

        // Enable universal planes (needed for atomic)
        device
            .set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .context("Failed to enable universal planes")?;

        info!("DRM device initialized with atomic modesetting support");

        Ok(device)
    }

    /// Returns the DRM device capabilities.
    pub fn get_capabilities(&self) -> anyhow::Result<DrmCapabilities> {
        let driver = self.get_driver().context("Failed to get DRM driver info")?;

        let name = driver.name().to_string_lossy().into_owned();
        let description = driver.description().to_string_lossy().into_owned();

        debug!("DRM driver: {} - {}", name, description);

        // Check various capabilities
        let has_dumb_buffer = self
            .get_driver_capability(drm::DriverCapability::DumbBuffer)
            .unwrap_or(0)
            != 0;

        let has_prime = self
            .get_driver_capability(drm::DriverCapability::Prime)
            .unwrap_or(0)
            != 0;

        let cursor_width = self
            .get_driver_capability(drm::DriverCapability::CursorWidth)
            .unwrap_or(64);

        let cursor_height = self
            .get_driver_capability(drm::DriverCapability::CursorHeight)
            .unwrap_or(64);

        Ok(DrmCapabilities {
            driver_name: name,
            driver_description: description,
            has_dumb_buffer,
            has_prime,
            cursor_width,
            cursor_height,
        })
    }

    /// Returns a reference to the underlying file descriptor.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Capabilities of a DRM device.
#[derive(Debug, Clone)]
pub struct DrmCapabilities {
    /// The driver name (e.g., "amdgpu", "i915", "nouveau")
    pub driver_name: String,
    /// The driver description
    pub driver_description: String,
    /// Whether the driver supports dumb buffers
    pub has_dumb_buffer: bool,
    /// Whether the driver supports PRIME (DMA-BUF sharing)
    pub has_prime: bool,
    /// Cursor width
    pub cursor_width: u64,
    /// Cursor height
    pub cursor_height: u64,
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
