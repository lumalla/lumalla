//! DRM connector enumeration via libdrm.

use std::ffi::CStr;
use std::os::fd::RawFd;
use std::ptr;

use log::warn;
use lumalla_shared::{DrmConnector, DrmMode};

#[allow(non_camel_case_types, non_snake_case, dead_code)]
mod bindings {
    use std::ffi::{c_char, c_int};

    pub const DRM_MODE_CONNECTED: u32 = 1;
    pub const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
    pub const DRM_DISPLAY_MODE_LEN: usize = 32;

    #[repr(C)]
    pub struct drmModeRes {
        pub count_fbs: c_int,
        pub fbs: *mut u32,
        pub count_crtcs: c_int,
        pub crtcs: *mut u32,
        pub count_connectors: c_int,
        pub connectors: *mut u32,
        pub count_encoders: c_int,
        pub encoders: *mut u32,
        pub min_width: u32,
        pub max_width: u32,
        pub min_height: u32,
        pub max_height: u32,
    }

    pub type drmModeResPtr = *mut drmModeRes;

    #[repr(C)]
    pub struct drmModeModeInfo {
        pub clock: u32,
        pub hdisplay: u16,
        pub hsync_start: u16,
        pub hsync_end: u16,
        pub htotal: u16,
        pub hskew: u16,
        pub vdisplay: u16,
        pub vsync_start: u16,
        pub vsync_end: u16,
        pub vtotal: u16,
        pub vscan: u16,
        pub vrefresh: u32,
        pub flags: u32,
        pub type_: u32,
        pub name: [c_char; DRM_DISPLAY_MODE_LEN],
    }

    /// Partial `drmModeConnector` — fields up through `modes`.
    #[repr(C)]
    pub struct drmModeConnector {
        pub connector_id: u32,
        pub encoder_id: u32,
        pub connector_type: u32,
        pub connector_type_id: u32,
        pub connection: u32,
        pub mmWidth: u32,
        pub mmHeight: u32,
        pub subpixel: u32,
        pub count_modes: c_int,
        pub modes: *mut drmModeModeInfo,
        pub count_props: c_int,
        pub props: *mut u32,
        pub prop_values: *mut u64,
        pub count_encoders: c_int,
        pub encoders: *mut u32,
    }

    pub type drmModeConnectorPtr = *mut drmModeConnector;

    unsafe extern "C" {
        pub fn drmModeGetResources(fd: c_int) -> drmModeResPtr;
        pub fn drmModeFreeResources(ptr: drmModeResPtr);
        pub fn drmModeGetConnector(fd: c_int, connector_id: u32) -> drmModeConnectorPtr;
        pub fn drmModeFreeConnector(ptr: drmModeConnectorPtr);
        pub fn drmModeGetConnectorTypeName(connector_type: u32) -> *const c_char;
    }
}

/// Probe all connectors on an open DRM primary-node fd.
pub fn probe_connectors(fd: RawFd) -> anyhow::Result<Vec<DrmConnector>> {
    let resources = unsafe { bindings::drmModeGetResources(fd) };
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
    let connector = unsafe { bindings::drmModeGetConnector(fd, connector_id) };
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
        connected: raw.connection == bindings::DRM_MODE_CONNECTED,
        mm_width: raw.mmWidth,
        mm_height: raw.mmHeight,
        modes,
    })
}

fn connector_type_name(connector_type: u32) -> Option<String> {
    let ptr = unsafe { bindings::drmModeGetConnectorTypeName(connector_type) };
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

fn mode_from_raw(mode: &bindings::drmModeModeInfo) -> DrmMode {
    let name = unsafe { CStr::from_ptr(mode.name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    DrmMode {
        width: u32::from(mode.hdisplay),
        height: u32::from(mode.vdisplay),
        refresh_hz: mode.vrefresh,
        name,
        preferred: mode.type_ & bindings::DRM_MODE_TYPE_PREFERRED != 0,
    }
}

struct DrmModeResources {
    ptr: bindings::drmModeResPtr,
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
            bindings::drmModeFreeResources(self.ptr);
        }
    }
}

struct DrmModeConnector {
    ptr: bindings::drmModeConnectorPtr,
}

impl DrmModeConnector {
    fn get(&self) -> &bindings::drmModeConnector {
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
            bindings::drmModeFreeConnector(self.ptr);
            self.ptr = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bindings;

    #[test]
    fn drm_mode_mode_info_layout() {
        assert_eq!(std::mem::size_of::<bindings::drmModeModeInfo>(), 68);
        assert_eq!(std::mem::offset_of!(bindings::drmModeModeInfo, hdisplay), 4);
        assert_eq!(
            std::mem::offset_of!(bindings::drmModeModeInfo, vdisplay),
            14
        );
        assert_eq!(
            std::mem::offset_of!(bindings::drmModeModeInfo, vrefresh),
            24
        );
        assert_eq!(std::mem::offset_of!(bindings::drmModeModeInfo, name), 36);
    }
}
