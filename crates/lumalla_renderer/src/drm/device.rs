//! DRM device management

use std::collections::HashMap;
use std::ffi::{CStr, OsStr, c_int};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::Context;
use log::{info, warn};
use lumalla_seat::{SeatDevice, SeatState};
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
        pub fn drmSetMaster(fd: c_int) -> c_int;
        pub fn drmDropMaster(fd: c_int) -> c_int;
    }
}

/// A DRM primary-node file descriptor opened through libseat.
///
/// Modesetting will be layered on top of this later via raw DRM ioctls.
pub struct DrmDevice {
    path: PathBuf,
    device_id: i32,
    fd: OwnedFd,
}

impl AsFd for DrmDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl DrmDevice {
    /// Wraps a seat-opened DRM primary node.
    pub fn from_seat_device(path: PathBuf, device: SeatDevice) -> Self {
        let device_id = device.device_id();
        Self {
            path,
            device_id,
            fd: device.into_fd(),
        }
    }

    /// Primary node path (e.g. `/dev/dri/card0`).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a reference to the underlying file descriptor.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Become DRM master on this device.
    pub fn set_master(&self) -> anyhow::Result<()> {
        let result = unsafe { bindings::drmSetMaster(self.fd.as_raw_fd()) };
        if result != 0 {
            anyhow::bail!(
                "drmSetMaster failed for {}: {}",
                self.path.display(),
                io::Error::last_os_error()
            );
        }
        Ok(())
    }

    /// Drop DRM master on this device.
    pub fn drop_master(&self) -> anyhow::Result<()> {
        let result = unsafe { bindings::drmDropMaster(self.fd.as_raw_fd()) };
        if result != 0 {
            anyhow::bail!(
                "drmDropMaster failed for {}: {}",
                self.path.display(),
                io::Error::last_os_error()
            );
        }
        Ok(())
    }

    /// Convert back into a [`SeatDevice`] for `libseat_close_device`.
    fn into_seat_device(self) -> SeatDevice {
        SeatDevice::from_raw_parts(self.device_id, self.fd)
    }
}

/// Discovered DRM primary nodes, refreshed via udev when GPUs appear or go away.
pub struct DrmDevices {
    paths: Vec<PathBuf>,
    opened: HashMap<PathBuf, DrmDevice>,
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
            opened: HashMap::new(),
            _udev: udev,
            monitor,
        })
    }

    /// Currently known primary-node paths (e.g. `/dev/dri/card0`).
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Currently opened DRM devices (session-active fds).
    pub fn opened(&self) -> &HashMap<PathBuf, DrmDevice> {
        &self.opened
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

    /// Open any missing primary nodes via the seat.
    ///
    /// Fresh opens through libseat already have DRM master. `drmSetMaster` is only
    /// called for devices that were already open (re-acquiring master after a VT switch).
    pub fn activate(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        let already_open: Vec<PathBuf> = self.opened.keys().cloned().collect();
        self.open_missing(seat)?;
        for path in already_open {
            let Some(device) = self.opened.get(&path) else {
                continue;
            };
            if let Err(err) = device.set_master() {
                warn!(
                    "Failed to set DRM master for {}: {err:#}",
                    device.path().display()
                );
            }
        }
        Ok(())
    }

    /// Drop DRM master on all opened devices without closing them.
    pub fn deactivate(&mut self) {
        for device in self.opened.values() {
            if let Err(err) = device.drop_master() {
                warn!(
                    "Failed to drop DRM master for {}: {err:#}",
                    device.path().display()
                );
            }
        }
    }

    /// Close removed devices and open newly discovered ones while the seat is active.
    ///
    /// New devices opened via libseat already have DRM master; no `drmSetMaster` needed.
    pub fn reconcile(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        self.close_removed(seat)?;
        self.open_missing(seat)?;
        Ok(())
    }

    fn open_missing(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        for path in &self.paths {
            if self.opened.contains_key(path) {
                continue;
            }
            let seat_device = seat
                .open_device(path)
                .with_context(|| format!("Failed to open DRM device {}", path.display()))?;
            info!(
                "Opened DRM device {} (device_id={})",
                path.display(),
                seat_device.device_id()
            );
            let device = DrmDevice::from_seat_device(path.clone(), seat_device);
            self.opened.insert(path.clone(), device);
        }
        Ok(())
    }

    fn close_removed(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        let to_close: Vec<PathBuf> = self
            .opened
            .keys()
            .filter(|path| !self.paths.contains(path))
            .cloned()
            .collect();

        for path in to_close {
            if let Some(device) = self.opened.remove(&path) {
                info!("Closing removed DRM device {}", path.display());
                let seat_device = device.into_seat_device();
                if let Err(err) = seat.close_device(seat_device) {
                    warn!("Failed to close DRM device {}: {err:#}", path.display());
                }
            }
        }
        Ok(())
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
