//! DRM device management

use std::ffi::{CStr, OsStr, c_int};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

use log::info;
use lumalla_shared::{Udev, UdevMonitor};
use mio::{Interest, Registry, Token, event::Source, unix::SourceFd};

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
        pub fn drmGetDevices2(flags: u32, devices: *mut drmDevicePtr, max_devices: c_int) -> c_int;
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

/// Discovered DRM primary nodes, refreshed via udev when GPUs appear or go away.
pub struct DrmDevices {
    paths: Vec<PathBuf>,
    /// Kept alive so the monitor's udev context remains valid.
    _udev: Udev,
    monitor: UdevMonitor,
}

impl DrmDevices {
    /// Enumerate primary nodes and start watching for add/remove via udev.
    pub fn new() -> anyhow::Result<Self> {
        let paths = find_drm_devices()?;

        let udev = Udev::new()?;
        let mut monitor = udev.monitor()?;
        monitor.match_subsystem("drm")?;
        monitor.enable_receiving()?;

        Ok(Self {
            paths,
            _udev: udev,
            monitor,
        })
    }

    /// Currently known primary-node paths (e.g. `/dev/dri/card0`).
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Drain pending udev DRM events and rescan primary nodes.
    ///
    /// Returns `true` if the discovered device list changed.
    pub fn dispatch(&mut self) -> anyhow::Result<bool> {
        let mut saw_card_event = false;
        while let Some(device) = self.monitor.receive_device() {
            let sysname = device.sysname().unwrap_or("");
            if sysname.starts_with("card") {
                saw_card_event = true;
            }
        }

        if !saw_card_event {
            return Ok(false);
        }

        let paths = find_drm_devices()?;
        if paths == self.paths {
            return Ok(false);
        }

        info!("DRM device list changed: {:?} -> {:?}", self.paths, paths);
        self.paths = paths;
        Ok(true)
    }
}

impl Source for DrmDevices {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.monitor.fd()).register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.monitor.fd()).reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        SourceFd(&self.monitor.fd()).deregister(registry)
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
