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
use lumalla_shared::{DrmConnector, DrmDeviceState, Udev, UdevMonitor};
use mio::{Interest, Registry, Token, event::Source, unix::SourceFd};

use super::connector::probe_connectors;

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

/// Result of draining udev DRM events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrmDispatchResult {
    /// Primary-node path list changed.
    pub devices_changed: bool,
    /// Connector state on an opened device changed.
    pub connectors_changed: bool,
}

impl DrmDispatchResult {
    /// True if either devices or connectors changed.
    pub fn changed(self) -> bool {
        self.devices_changed || self.connectors_changed
    }
}

/// A DRM primary-node file descriptor opened through libseat.
///
/// Modesetting will be layered on top of this later via raw DRM ioctls.
pub struct DrmDevice {
    path: PathBuf,
    device_id: i32,
    fd: OwnedFd,
    connectors: Vec<DrmConnector>,
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
            connectors: Vec::new(),
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

    /// Last probed connectors for this device.
    pub fn connectors(&self) -> &[DrmConnector] {
        &self.connectors
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

    /// Probe connectors via libdrm; returns `true` if the list changed.
    pub fn probe_connectors(&mut self) -> anyhow::Result<bool> {
        let connectors = probe_connectors(self.fd.as_raw_fd())
            .with_context(|| format!("Failed to probe connectors on {}", self.path.display()))?;
        if connectors == self.connectors {
            return Ok(false);
        }
        info!(
            "DRM connectors on {}: {:?}",
            self.path.display(),
            connectors
                .iter()
                .map(|c| {
                    let status = if c.connected {
                        "connected"
                    } else {
                        "disconnected"
                    };
                    let preferred = c
                        .modes
                        .iter()
                        .find(|m| m.preferred)
                        .map(|m| format!("{}@{}Hz", m.name, m.refresh_hz))
                        .unwrap_or_else(|| "-".into());
                    format!(
                        "{}({status}, {} modes, preferred={preferred})",
                        c.name,
                        c.modes.len()
                    )
                })
                .collect::<Vec<_>>()
        );
        self.connectors = connectors;
        Ok(true)
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

    /// Snapshot of discovered devices and probed connectors for IPC.
    pub fn device_states(&self) -> Vec<DrmDeviceState> {
        self.paths
            .iter()
            .map(|path| {
                let connectors = self
                    .opened
                    .get(path)
                    .map(|device| device.connectors().to_vec())
                    .unwrap_or_default();
                DrmDeviceState {
                    path: path.clone(),
                    connectors,
                }
            })
            .collect()
    }

    /// Drain pending udev DRM events; update device paths and/or connectors.
    pub fn dispatch(&mut self) -> anyhow::Result<DrmDispatchResult> {
        let mut saw_card_event = false;
        while let Some(device) = self.monitor.receive_device() {
            let sysname = device.sysname().unwrap_or("");
            if sysname.starts_with("card") {
                saw_card_event = true;
            }
        }

        if !saw_card_event {
            return Ok(DrmDispatchResult::default());
        }

        let mut result = DrmDispatchResult::default();

        let paths = find_drm_devices()?;
        if paths != self.paths {
            info!("DRM device list changed: {:?} -> {:?}", self.paths, paths);
            self.paths = paths;
            result.devices_changed = true;
        }

        if !self.opened.is_empty() {
            result.connectors_changed = self.probe_all_connectors()?;
        }

        Ok(result)
    }

    /// Open any missing primary nodes via the seat.
    ///
    /// Fresh opens through libseat already have DRM master. After a VT switch,
    /// devices must have been closed in [`Self::deactivate`] so they are reopened here.
    pub fn activate(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        self.open_missing(seat)?;
        self.probe_all_connectors()?;
        Ok(())
    }

    /// Close all seat-opened DRM devices in preparation for session disable.
    ///
    /// libseat requires acknowledging disable with `libseat_disable_seat` after
    /// devices are no longer used; existing fds may be revoked on VT switch.
    pub fn deactivate(&mut self, seat: &SeatState) {
        let opened = std::mem::take(&mut self.opened);
        for (path, device) in opened {
            info!("Closing DRM device {} for session disable", path.display());
            let seat_device = device.into_seat_device();
            if let Err(err) = seat.close_device(seat_device) {
                warn!("Failed to close DRM device {}: {err:#}", path.display());
            }
        }
    }

    /// Close removed devices and open newly discovered ones while the seat is active.
    ///
    /// New devices opened via libseat already have DRM master; no `drmSetMaster` needed.
    pub fn reconcile(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        self.close_removed(seat)?;
        self.open_missing(seat)?;
        self.probe_all_connectors()?;
        Ok(())
    }

    fn probe_all_connectors(&mut self) -> anyhow::Result<bool> {
        let mut changed = false;
        let paths: Vec<PathBuf> = self.opened.keys().cloned().collect();
        for path in paths {
            let Some(device) = self.opened.get_mut(&path) else {
                continue;
            };
            if device.probe_connectors()? {
                changed = true;
            }
        }
        Ok(changed)
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
            let mut device = DrmDevice::from_seat_device(path.clone(), seat_device);
            if let Err(err) = device.probe_connectors() {
                warn!(
                    "Failed to probe connectors on newly opened {}: {err:#}",
                    path.display()
                );
            }
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
