//! DRM modesetting: CRTC selection, framebuffer import, and SetCrtc.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::ptr;

use log::debug;

use super::sys;

/// Full kernel mode info required by `drmModeSetCrtc`.
#[derive(Clone)]
pub struct ModeInfo {
    raw: sys::drmModeModeInfo,
}

impl std::fmt::Debug for ModeInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModeInfo")
            .field("name", &self.name())
            .field("width", &self.width())
            .field("height", &self.height())
            .field("refresh_hz", &self.refresh_hz())
            .field("preferred", &self.preferred())
            .finish()
    }
}

impl ModeInfo {
    pub fn width(&self) -> u32 {
        u32::from(self.raw.hdisplay)
    }

    pub fn height(&self) -> u32 {
        u32::from(self.raw.vdisplay)
    }

    pub fn refresh_hz(&self) -> u32 {
        self.raw.vrefresh
    }

    pub fn name(&self) -> String {
        unsafe { CStr::from_ptr(self.raw.name.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn preferred(&self) -> bool {
        self.raw.type_ & sys::DRM_MODE_TYPE_PREFERRED != 0
    }
}

/// A connected connector with a chosen CRTC and mode, ready for modeset.
#[derive(Debug, Clone)]
pub struct ConnectedOutput {
    pub connector_id: u32,
    pub connector_name: String,
    pub crtc_id: u32,
    pub mode: ModeInfo,
}

/// A DRM framebuffer imported from a DMA-BUF.
pub struct DrmFramebuffer {
    fd: RawFd,
    fb_id: u32,
    gem_handle: u32,
}

impl DrmFramebuffer {
    /// Import a DMA-BUF as a KMS framebuffer via `drmModeAddFB2WithModifiers`.
    pub fn from_dma_buf(
        drm_fd: BorrowedFd<'_>,
        dma_buf: BorrowedFd<'_>,
        width: u32,
        height: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
        fourcc: u32,
    ) -> anyhow::Result<Self> {
        let fd = drm_fd.as_raw_fd();
        let mut gem_handle = 0u32;
        let result =
            unsafe { sys::drmPrimeFDToHandle(fd, dma_buf.as_raw_fd(), &mut gem_handle) };
        if result != 0 {
            anyhow::bail!(
                "drmPrimeFDToHandle failed: {}",
                io::Error::last_os_error()
            );
        }

        let handles = [gem_handle, 0, 0, 0];
        let pitches = [stride, 0, 0, 0];
        let offsets = [offset, 0, 0, 0];
        let modifiers = [modifier, modifier, modifier, modifier];
        let mut fb_id = 0u32;

        let result = unsafe {
            sys::drmModeAddFB2WithModifiers(
                fd,
                width,
                height,
                fourcc,
                handles.as_ptr(),
                pitches.as_ptr(),
                offsets.as_ptr(),
                modifiers.as_ptr(),
                &mut fb_id,
                sys::DRM_MODE_FB_MODIFIERS,
            )
        };
        if result != 0 {
            let err = io::Error::last_os_error();
            let _ = unsafe { sys::drmCloseBufferHandle(fd, gem_handle) };
            anyhow::bail!("drmModeAddFB2WithModifiers failed: {err}");
        }

        debug!(
            "Created DRM FB {fb_id} ({}x{}, fourcc={fourcc:#x}, modifier={modifier:#x}, stride={stride})",
            width, height
        );

        Ok(Self {
            fd,
            fb_id,
            gem_handle,
        })
    }

    pub fn id(&self) -> u32 {
        self.fb_id
    }
}

impl Drop for DrmFramebuffer {
    fn drop(&mut self) {
        if self.fb_id != 0 {
            let result = unsafe { sys::drmModeRmFB(self.fd, self.fb_id) };
            if result != 0 {
                log::warn!(
                    "drmModeRmFB({}) failed: {}",
                    self.fb_id,
                    io::Error::last_os_error()
                );
            }
            self.fb_id = 0;
        }
        if self.gem_handle != 0 {
            let result = unsafe { sys::drmCloseBufferHandle(self.fd, self.gem_handle) };
            if result != 0 {
                log::warn!(
                    "drmCloseBufferHandle({}) failed: {}",
                    self.gem_handle,
                    io::Error::last_os_error()
                );
            }
            self.gem_handle = 0;
        }
    }
}

/// Find the first connected connector with a usable CRTC and preferred (or first) mode.
pub fn find_first_connected_output(fd: RawFd) -> anyhow::Result<Option<ConnectedOutput>> {
    let resources = get_resources(fd)?;
    let connector_ids = resources.connector_ids();

    for &connector_id in connector_ids {
        match probe_connected_output(fd, &resources, connector_id) {
            Ok(Some(output)) => return Ok(Some(output)),
            Ok(None) => {}
            Err(err) => {
                log::warn!("Failed to probe connector {connector_id} for modeset: {err:#}");
            }
        }
    }

    Ok(None)
}

/// Program a CRTC with the given framebuffer and mode on one connector.
pub fn set_crtc(
    drm_fd: BorrowedFd<'_>,
    crtc_id: u32,
    fb_id: u32,
    connector_id: u32,
    mode: &ModeInfo,
) -> anyhow::Result<()> {
    let fd = drm_fd.as_raw_fd();
    let mut connectors = [connector_id];
    let mut mode_raw = mode.raw;

    let result = unsafe {
        sys::drmModeSetCrtc(
            fd,
            crtc_id,
            fb_id,
            0,
            0,
            connectors.as_mut_ptr(),
            1,
            &mut mode_raw,
        )
    };
    if result != 0 {
        anyhow::bail!(
            "drmModeSetCrtc(crtc={crtc_id}, fb={fb_id}, connector={connector_id}) failed: {}",
            io::Error::last_os_error()
        );
    }

    debug!(
        "Set CRTC {crtc_id} to FB {fb_id} on connector {connector_id} ({})",
        mode.name()
    );
    Ok(())
}

fn probe_connected_output(
    fd: RawFd,
    resources: &DrmModeResources,
    connector_id: u32,
) -> anyhow::Result<Option<ConnectedOutput>> {
    let connector = get_connector(fd, connector_id)?;
    let raw = connector.get();

    if raw.connection != sys::DRM_MODE_CONNECTED {
        return Ok(None);
    }

    let Some(mode) = connector.preferred_or_first_mode() else {
        return Ok(None);
    };

    let Some(crtc_id) = find_crtc_for_connector(fd, resources, &connector)? else {
        anyhow::bail!(
            "No usable CRTC for connector {connector_id} ({})",
            connector_name(raw)
        );
    };

    Ok(Some(ConnectedOutput {
        connector_id: raw.connector_id,
        connector_name: connector_name(raw),
        crtc_id,
        mode,
    }))
}

fn find_crtc_for_connector(
    fd: RawFd,
    resources: &DrmModeResources,
    connector: &DrmModeConnector,
) -> anyhow::Result<Option<u32>> {
    let raw = connector.get();

    if raw.encoder_id != 0 {
        if let Some(crtc_id) = crtc_from_encoder(fd, resources, raw.encoder_id)? {
            return Ok(Some(crtc_id));
        }
    }

    for &encoder_id in connector.encoder_ids() {
        if let Some(crtc_id) = crtc_from_encoder(fd, resources, encoder_id)? {
            return Ok(Some(crtc_id));
        }
    }

    Ok(None)
}

fn crtc_from_encoder(
    fd: RawFd,
    resources: &DrmModeResources,
    encoder_id: u32,
) -> anyhow::Result<Option<u32>> {
    let encoder = get_encoder(fd, encoder_id)?;
    let enc = encoder.get();

    if enc.crtc_id != 0 {
        return Ok(Some(enc.crtc_id));
    }

    Ok(crtc_from_possible_mask(resources, enc.possible_crtcs))
}

fn crtc_from_possible_mask(resources: &DrmModeResources, possible_crtcs: u32) -> Option<u32> {
    let crtcs = resources.crtc_ids();
    for (index, &crtc_id) in crtcs.iter().enumerate() {
        if possible_crtcs & (1 << index) != 0 {
            return Some(crtc_id);
        }
    }
    None
}

fn connector_name(raw: &sys::drmModeConnector) -> String {
    let type_name = connector_type_name(raw.connector_type)
        .unwrap_or_else(|| format!("Unknown-{}", raw.connector_type));
    format!("{type_name}-{}", raw.connector_type_id)
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

fn get_resources(fd: RawFd) -> anyhow::Result<DrmModeResources> {
    let ptr = unsafe { sys::drmModeGetResources(fd) };
    if ptr.is_null() {
        anyhow::bail!(
            "drmModeGetResources failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(DrmModeResources { ptr })
}

fn get_connector(fd: RawFd, connector_id: u32) -> anyhow::Result<DrmModeConnector> {
    let ptr = unsafe { sys::drmModeGetConnector(fd, connector_id) };
    if ptr.is_null() {
        anyhow::bail!(
            "drmModeGetConnector({connector_id}) failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(DrmModeConnector { ptr })
}

fn get_encoder(fd: RawFd, encoder_id: u32) -> anyhow::Result<DrmModeEncoder> {
    let ptr = unsafe { sys::drmModeGetEncoder(fd, encoder_id) };
    if ptr.is_null() {
        anyhow::bail!(
            "drmModeGetEncoder({encoder_id}) failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(DrmModeEncoder { ptr })
}

struct DrmModeResources {
    ptr: sys::drmModeResPtr,
}

impl DrmModeResources {
    fn connector_ids(&self) -> &[u32] {
        let count = unsafe { (*self.ptr).count_connectors.max(0) as usize };
        let ptr = unsafe { (*self.ptr).connectors };
        if ptr.is_null() || count == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(ptr, count) }
    }

    fn crtc_ids(&self) -> &[u32] {
        let count = unsafe { (*self.ptr).count_crtcs.max(0) as usize };
        let ptr = unsafe { (*self.ptr).crtcs };
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

    fn encoder_ids(&self) -> &[u32] {
        let raw = self.get();
        let count = raw.count_encoders.max(0) as usize;
        if raw.encoders.is_null() || count == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(raw.encoders, count) }
    }

    fn preferred_or_first_mode(&self) -> Option<ModeInfo> {
        let raw = self.get();
        let count = raw.count_modes.max(0) as usize;
        if raw.modes.is_null() || count == 0 {
            return None;
        }
        let modes = unsafe { std::slice::from_raw_parts(raw.modes, count) };
        let preferred = modes
            .iter()
            .find(|m| m.type_ & sys::DRM_MODE_TYPE_PREFERRED != 0)
            .or_else(|| modes.first())?;
        Some(ModeInfo { raw: *preferred })
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

struct DrmModeEncoder {
    ptr: sys::drmModeEncoderPtr,
}

impl DrmModeEncoder {
    fn get(&self) -> &sys::drmModeEncoder {
        unsafe { &*self.ptr }
    }
}

impl Drop for DrmModeEncoder {
    fn drop(&mut self) {
        unsafe {
            sys::drmModeFreeEncoder(self.ptr);
            self.ptr = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::drm::sys;

    #[test]
    fn drm_mode_encoder_layout() {
        assert_eq!(std::mem::size_of::<sys::drmModeEncoder>(), 20);
        assert_eq!(
            std::mem::offset_of!(sys::drmModeEncoder, possible_crtcs),
            12
        );
    }
}
