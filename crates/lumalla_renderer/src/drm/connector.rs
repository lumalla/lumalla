//! DRM connector enumeration via libdrm.

use std::ffi::CStr;
use std::os::fd::RawFd;
use std::ptr;

use log::warn;
use lumalla_shared::{DrmConnector, DrmMode};

use super::sys;

/// Probe all connectors on an open DRM primary-node fd.
pub fn probe_connectors(fd: RawFd) -> anyhow::Result<Vec<DrmConnector>> {
    let resources = unsafe { sys::drmModeGetResources(fd) };
    if resources.is_null() {
        anyhow::bail!(
            "drmModeGetResources failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let resources = DrmModeResources { ptr: resources };

    let count = resources.count_connectors();
    let connector_ids = resources.connector_ids();
    let mut connectors = Vec::with_capacity(count);

    for &connector_id in connector_ids {
        match probe_one(fd, connector_id) {
            Ok(connector) => connectors.push(connector),
            Err(err) => {
                warn!("Failed to probe DRM connector {connector_id}: {err:#}");
            }
        }
    }

    connectors.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(connectors)
}

fn probe_one(fd: RawFd, connector_id: u32) -> anyhow::Result<DrmConnector> {
    let connector = unsafe { sys::drmModeGetConnector(fd, connector_id) };
    if connector.is_null() {
        anyhow::bail!(
            "drmModeGetConnector({connector_id}) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let connector = DrmModeConnector { ptr: connector };
    let raw = connector.get();

    let type_name = connector_type_name(raw.connector_type)
        .unwrap_or_else(|| format!("Unknown-{}", raw.connector_type));
    let name = format!("{type_name}-{}", raw.connector_type_id);
    let modes = connector.modes();

    Ok(DrmConnector {
        name,
        connector_id: raw.connector_id,
        connector_type: type_name,
        connected: raw.connection == sys::DRM_MODE_CONNECTED,
        mm_width: raw.mmWidth,
        mm_height: raw.mmHeight,
        modes,
    })
}

fn connector_type_name(connector_type: u32) -> Option<String> {
    let ptr = unsafe { sys::drmModeGetConnectorTypeName(connector_type) };
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

fn mode_from_raw(mode: &sys::drmModeModeInfo) -> DrmMode {
    let name = unsafe { CStr::from_ptr(mode.name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    DrmMode {
        width: u32::from(mode.hdisplay),
        height: u32::from(mode.vdisplay),
        refresh_hz: mode.vrefresh,
        name,
        preferred: mode.type_ & sys::DRM_MODE_TYPE_PREFERRED != 0,
    }
}

struct DrmModeResources {
    ptr: sys::drmModeResPtr,
}

impl DrmModeResources {
    fn count_connectors(&self) -> usize {
        unsafe { (*self.ptr).count_connectors.max(0) as usize }
    }

    fn connector_ids(&self) -> &[u32] {
        let count = self.count_connectors();
        let ptr = unsafe { (*self.ptr).connectors };
        if ptr.is_null() || count == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(ptr, count) }
    }
}

impl Drop for DrmModeResources {
    fn drop(&mut self) {
        unsafe {
            sys::drmModeFreeResources(self.ptr);
        }
    }
}

struct DrmModeConnector {
    ptr: sys::drmModeConnectorPtr,
}

impl DrmModeConnector {
    fn get(&self) -> &sys::drmModeConnector {
        unsafe { &*self.ptr }
    }

    fn modes(&self) -> Vec<DrmMode> {
        let raw = self.get();
        let count = raw.count_modes.max(0) as usize;
        if raw.modes.is_null() || count == 0 {
            return Vec::new();
        }
        let modes = unsafe { std::slice::from_raw_parts(raw.modes, count) };
        modes.iter().map(mode_from_raw).collect()
    }
}

impl Drop for DrmModeConnector {
    fn drop(&mut self) {
        unsafe {
            sys::drmModeFreeConnector(self.ptr);
            self.ptr = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::drm::sys;

    #[test]
    fn drm_mode_mode_info_layout() {
        assert_eq!(std::mem::size_of::<sys::drmModeModeInfo>(), 68);
        assert_eq!(std::mem::offset_of!(sys::drmModeModeInfo, hdisplay), 4);
        assert_eq!(std::mem::offset_of!(sys::drmModeModeInfo, vdisplay), 14);
        assert_eq!(std::mem::offset_of!(sys::drmModeModeInfo, vrefresh), 24);
        assert_eq!(std::mem::offset_of!(sys::drmModeModeInfo, name), 36);
    }
}
