//! DRM modesetting via the atomic API: planes, property blobs, and page-flips.

use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use log::debug;

use super::sys;

/// Full kernel mode info required for atomic MODE_ID blobs.
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

/// Cached property IDs for an atomic connector/CRTC/plane trio.
#[derive(Debug, Clone)]
pub struct AtomicProps {
    pub connector_crtc_id: u32,
    pub crtc_active: u32,
    pub crtc_mode_id: u32,
    pub plane_fb_id: u32,
    pub plane_crtc_id: u32,
    pub plane_src_x: u32,
    pub plane_src_y: u32,
    pub plane_src_w: u32,
    pub plane_src_h: u32,
    pub plane_crtc_x: u32,
    pub plane_crtc_y: u32,
    pub plane_crtc_w: u32,
    pub plane_crtc_h: u32,
}

/// A connected connector with CRTC, primary plane, and mode — ready for atomic modeset.
#[derive(Debug, Clone)]
pub struct ConnectedOutput {
    pub connector_id: u32,
    pub connector_name: String,
    pub crtc_id: u32,
    pub crtc_index: u32,
    pub plane_id: u32,
    pub mode: ModeInfo,
    pub props: AtomicProps,
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

/// Owns a MODE_ID property blob for the lifetime of an active modeset.
pub struct ModeBlob {
    fd: RawFd,
    id: u32,
}

impl ModeBlob {
    pub fn create(drm_fd: BorrowedFd<'_>, mode: &ModeInfo) -> anyhow::Result<Self> {
        let fd = drm_fd.as_raw_fd();
        let mut id = 0u32;
        let result = unsafe {
            sys::drmModeCreatePropertyBlob(
                fd,
                (&raw const mode.raw).cast(),
                std::mem::size_of::<sys::drmModeModeInfo>(),
                &mut id,
            )
        };
        if result != 0 {
            anyhow::bail!(
                "drmModeCreatePropertyBlob failed: {}",
                io::Error::last_os_error()
            );
        }
        Ok(Self { fd, id })
    }

    pub fn id(&self) -> u32 {
        self.id
    }
}

impl Drop for ModeBlob {
    fn drop(&mut self) {
        if self.id != 0 {
            let result = unsafe { sys::drmModeDestroyPropertyBlob(self.fd, self.id) };
            if result != 0 {
                log::warn!(
                    "drmModeDestroyPropertyBlob({}) failed: {}",
                    self.id,
                    io::Error::last_os_error()
                );
            }
            self.id = 0;
        }
    }
}

/// Enable atomic + universal planes on a freshly opened DRM primary node.
pub fn enable_atomic_client_caps(fd: RawFd) -> anyhow::Result<()> {
    let result =
        unsafe { sys::drmSetClientCap(fd, sys::DRM_CLIENT_CAP_ATOMIC, 1) };
    if result != 0 {
        anyhow::bail!(
            "drmSetClientCap(ATOMIC) failed: {}",
            io::Error::last_os_error()
        );
    }
    // ATOMIC implies UNIVERSAL_PLANES on modern kernels; set explicitly for older ones.
    let _ = unsafe { sys::drmSetClientCap(fd, sys::DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1) };
    Ok(())
}

/// Find the first connected connector with a usable CRTC, primary plane, and preferred mode.
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

/// Initial modeset: enable CRTC, attach connector, set mode, and assign primary plane FB.
pub fn atomic_modeset(
    drm_fd: BorrowedFd<'_>,
    output: &ConnectedOutput,
    mode_blob_id: u32,
    fb_id: u32,
) -> anyhow::Result<()> {
    let fd = drm_fd.as_raw_fd();
    let width = output.mode.width();
    let height = output.mode.height();
    let props = &output.props;

    let req = AtomicRequest::new()?;
    req.add(output.connector_id, props.connector_crtc_id, u64::from(output.crtc_id))?;
    req.add(output.crtc_id, props.crtc_active, 1u64)?;
    req.add(output.crtc_id, props.crtc_mode_id, u64::from(mode_blob_id))?;
    add_plane_fb_props(&req, output, props, fb_id, width, height)?;

    req.commit(
        fd,
        sys::DRM_MODE_ATOMIC_ALLOW_MODESET,
        ptr::null_mut(),
    )?;

    debug!(
        "Atomic modeset: CRTC {} plane {} FB {} on {} ({})",
        output.crtc_id,
        output.plane_id,
        fb_id,
        output.connector_name,
        output.mode.name()
    );
    Ok(())
}

/// Non-blocking page-flip of the primary plane FB, requesting a flip event.
///
/// `flip_done` is set to `true` by the DRM page-flip handler when the flip completes.
pub fn atomic_page_flip(
    drm_fd: BorrowedFd<'_>,
    output: &ConnectedOutput,
    fb_id: u32,
    flip_done: &AtomicBool,
) -> anyhow::Result<()> {
    let fd = drm_fd.as_raw_fd();
    let width = output.mode.width();
    let height = output.mode.height();
    let props = &output.props;

    flip_done.store(false, Ordering::SeqCst);

    let req = AtomicRequest::new()?;
    add_plane_fb_props(&req, output, props, fb_id, width, height)?;

    req.commit(
        fd,
        sys::DRM_MODE_PAGE_FLIP_EVENT | sys::DRM_MODE_ATOMIC_NONBLOCK,
        flip_done as *const AtomicBool as *mut c_void,
    )?;

    debug!(
        "Atomic page-flip scheduled: plane {} -> FB {}",
        output.plane_id, fb_id
    );
    Ok(())
}

/// Drain pending DRM events on `fd` (page-flip completions, etc.).
pub fn dispatch_drm_events(fd: RawFd) -> anyhow::Result<()> {
    let mut ctx = sys::drmEventContext {
        version: sys::DRM_EVENT_CONTEXT_VERSION,
        vblank_handler: ptr::null_mut(),
        page_flip_handler: Some(page_flip_handler),
        page_flip_handler2: None,
        sequence_handler: ptr::null_mut(),
    };

    let result = unsafe { sys::drmHandleEvent(fd, &mut ctx) };
    if result != 0 {
        anyhow::bail!(
            "drmHandleEvent failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(())
}

unsafe extern "C" fn page_flip_handler(
    _fd: std::ffi::c_int,
    _sequence: u32,
    _tv_sec: u32,
    _tv_usec: u32,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
    let flag = unsafe { &*(user_data as *const AtomicBool) };
    flag.store(true, Ordering::SeqCst);
}

fn add_plane_fb_props(
    req: &AtomicRequest,
    output: &ConnectedOutput,
    props: &AtomicProps,
    fb_id: u32,
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    req.add(output.plane_id, props.plane_fb_id, u64::from(fb_id))?;
    req.add(output.plane_id, props.plane_crtc_id, u64::from(output.crtc_id))?;
    req.add(output.plane_id, props.plane_src_x, 0u64)?;
    req.add(output.plane_id, props.plane_src_y, 0u64)?;
    req.add(output.plane_id, props.plane_src_w, u64::from(width) << 16)?;
    req.add(output.plane_id, props.plane_src_h, u64::from(height) << 16)?;
    req.add(output.plane_id, props.plane_crtc_x, 0u64)?;
    req.add(output.plane_id, props.plane_crtc_y, 0u64)?;
    req.add(output.plane_id, props.plane_crtc_w, u64::from(width))?;
    req.add(output.plane_id, props.plane_crtc_h, u64::from(height))?;
    Ok(())
}

struct AtomicRequest {
    ptr: sys::drmModeAtomicReqPtr,
}

impl AtomicRequest {
    fn new() -> anyhow::Result<Self> {
        let ptr = unsafe { sys::drmModeAtomicAlloc() };
        if ptr.is_null() {
            anyhow::bail!("drmModeAtomicAlloc failed");
        }
        Ok(Self { ptr })
    }

    fn add(&self, object_id: u32, prop_id: u32, value: impl Into<u64>) -> anyhow::Result<()> {
        let result =
            unsafe { sys::drmModeAtomicAddProperty(self.ptr, object_id, prop_id, value.into()) };
        if result < 0 {
            anyhow::bail!(
                "drmModeAtomicAddProperty(obj={object_id}, prop={prop_id}) failed: {}",
                io::Error::last_os_error()
            );
        }
        Ok(())
    }

    fn commit(&self, fd: RawFd, flags: u32, user_data: *mut c_void) -> anyhow::Result<()> {
        let result = unsafe { sys::drmModeAtomicCommit(fd, self.ptr, flags, user_data) };
        if result != 0 {
            anyhow::bail!(
                "drmModeAtomicCommit(flags={flags:#x}) failed: {}",
                io::Error::last_os_error()
            );
        }
        Ok(())
    }
}

impl Drop for AtomicRequest {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { sys::drmModeAtomicFree(self.ptr) };
            self.ptr = ptr::null_mut();
        }
    }
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

    let Some((crtc_id, crtc_index)) = find_crtc_for_connector(fd, resources, &connector)? else {
        anyhow::bail!(
            "No usable CRTC for connector {connector_id} ({})",
            connector_name(raw)
        );
    };

    let Some(plane_id) = find_primary_plane(fd, crtc_index)? else {
        anyhow::bail!(
            "No primary plane for CRTC {crtc_id} (index {crtc_index}) on {}",
            connector_name(raw)
        );
    };

    let props = AtomicProps {
        connector_crtc_id: find_prop_id(fd, connector_id, sys::DRM_MODE_OBJECT_CONNECTOR, "CRTC_ID")?,
        crtc_active: find_prop_id(fd, crtc_id, sys::DRM_MODE_OBJECT_CRTC, "ACTIVE")?,
        crtc_mode_id: find_prop_id(fd, crtc_id, sys::DRM_MODE_OBJECT_CRTC, "MODE_ID")?,
        plane_fb_id: find_prop_id(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE, "FB_ID")?,
        plane_crtc_id: find_prop_id(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE, "CRTC_ID")?,
        plane_src_x: find_prop_id(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE, "SRC_X")?,
        plane_src_y: find_prop_id(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE, "SRC_Y")?,
        plane_src_w: find_prop_id(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE, "SRC_W")?,
        plane_src_h: find_prop_id(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE, "SRC_H")?,
        plane_crtc_x: find_prop_id(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE, "CRTC_X")?,
        plane_crtc_y: find_prop_id(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE, "CRTC_Y")?,
        plane_crtc_w: find_prop_id(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE, "CRTC_W")?,
        plane_crtc_h: find_prop_id(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE, "CRTC_H")?,
    };

    Ok(Some(ConnectedOutput {
        connector_id: raw.connector_id,
        connector_name: connector_name(raw),
        crtc_id,
        crtc_index,
        plane_id,
        mode,
        props,
    }))
}

fn find_crtc_for_connector(
    fd: RawFd,
    resources: &DrmModeResources,
    connector: &DrmModeConnector,
) -> anyhow::Result<Option<(u32, u32)>> {
    let raw = connector.get();

    if raw.encoder_id != 0 {
        if let Some(found) = crtc_from_encoder(fd, resources, raw.encoder_id)? {
            return Ok(Some(found));
        }
    }

    for &encoder_id in connector.encoder_ids() {
        if let Some(found) = crtc_from_encoder(fd, resources, encoder_id)? {
            return Ok(Some(found));
        }
    }

    Ok(None)
}

fn crtc_from_encoder(
    fd: RawFd,
    resources: &DrmModeResources,
    encoder_id: u32,
) -> anyhow::Result<Option<(u32, u32)>> {
    let encoder = get_encoder(fd, encoder_id)?;
    let enc = encoder.get();

    if enc.crtc_id != 0 {
        if let Some(index) = resources.crtc_index(enc.crtc_id) {
            return Ok(Some((enc.crtc_id, index)));
        }
    }

    Ok(crtc_from_possible_mask(resources, enc.possible_crtcs))
}

fn crtc_from_possible_mask(
    resources: &DrmModeResources,
    possible_crtcs: u32,
) -> Option<(u32, u32)> {
    let crtcs = resources.crtc_ids();
    for (index, &crtc_id) in crtcs.iter().enumerate() {
        if possible_crtcs & (1 << index) != 0 {
            return Some((crtc_id, index as u32));
        }
    }
    None
}

fn find_primary_plane(fd: RawFd, crtc_index: u32) -> anyhow::Result<Option<u32>> {
    let planes = get_plane_resources(fd)?;
    for &plane_id in planes.plane_ids() {
        let plane = get_plane(fd, plane_id)?;
        if plane.get().possible_crtcs & (1 << crtc_index) == 0 {
            continue;
        }
        let props = object_props(fd, plane_id, sys::DRM_MODE_OBJECT_PLANE)?;
        let Some(type_prop) = props.get("type").copied() else {
            continue;
        };
        if type_prop == sys::DRM_PLANE_TYPE_PRIMARY {
            return Ok(Some(plane_id));
        }
    }
    Ok(None)
}

fn find_prop_id(fd: RawFd, object_id: u32, object_type: u32, name: &str) -> anyhow::Result<u32> {
    let props = object_prop_ids(fd, object_id, object_type)?;
    for &prop_id in &props {
        let prop = get_property(fd, prop_id)?;
        if prop.name() == name {
            return Ok(prop_id);
        }
    }
    anyhow::bail!("Property '{name}' not found on object {object_id:#x}");
}

fn object_props(
    fd: RawFd,
    object_id: u32,
    object_type: u32,
) -> anyhow::Result<HashMap<String, u64>> {
    let raw = get_object_properties(fd, object_id, object_type)?;
    let count = raw.count() as usize;
    let mut map = HashMap::with_capacity(count);
    for i in 0..count {
        let prop_id = raw.prop_id(i);
        let value = raw.prop_value(i);
        let prop = get_property(fd, prop_id)?;
        map.insert(prop.name(), value);
    }
    Ok(map)
}

fn object_prop_ids(fd: RawFd, object_id: u32, object_type: u32) -> anyhow::Result<Vec<u32>> {
    let raw = get_object_properties(fd, object_id, object_type)?;
    let count = raw.count() as usize;
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        ids.push(raw.prop_id(i));
    }
    Ok(ids)
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

fn get_plane_resources(fd: RawFd) -> anyhow::Result<DrmModePlaneResources> {
    let ptr = unsafe { sys::drmModeGetPlaneResources(fd) };
    if ptr.is_null() {
        anyhow::bail!(
            "drmModeGetPlaneResources failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(DrmModePlaneResources { ptr })
}

fn get_plane(fd: RawFd, plane_id: u32) -> anyhow::Result<DrmModePlane> {
    let ptr = unsafe { sys::drmModeGetPlane(fd, plane_id) };
    if ptr.is_null() {
        anyhow::bail!(
            "drmModeGetPlane({plane_id}) failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(DrmModePlane { ptr })
}

fn get_object_properties(
    fd: RawFd,
    object_id: u32,
    object_type: u32,
) -> anyhow::Result<DrmObjectProperties> {
    let ptr = unsafe { sys::drmModeObjectGetProperties(fd, object_id, object_type) };
    if ptr.is_null() {
        anyhow::bail!(
            "drmModeObjectGetProperties({object_id}) failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(DrmObjectProperties { ptr })
}

fn get_property(fd: RawFd, prop_id: u32) -> anyhow::Result<DrmProperty> {
    let ptr = unsafe { sys::drmModeGetProperty(fd, prop_id) };
    if ptr.is_null() {
        anyhow::bail!(
            "drmModeGetProperty({prop_id}) failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(DrmProperty { ptr })
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

    fn crtc_index(&self, crtc_id: u32) -> Option<u32> {
        self.crtc_ids()
            .iter()
            .position(|&id| id == crtc_id)
            .map(|i| i as u32)
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

struct DrmModePlaneResources {
    ptr: sys::drmModePlaneResPtr,
}

impl DrmModePlaneResources {
    fn plane_ids(&self) -> &[u32] {
        let count = unsafe { (*self.ptr).count_planes as usize };
        let ptr = unsafe { (*self.ptr).planes };
        if ptr.is_null() || count == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(ptr, count) }
    }
}

impl Drop for DrmModePlaneResources {
    fn drop(&mut self) {
        unsafe {
            sys::drmModeFreePlaneResources(self.ptr);
        }
    }
}

struct DrmModePlane {
    ptr: sys::drmModePlanePtr,
}

impl DrmModePlane {
    fn get(&self) -> &sys::drmModePlane {
        unsafe { &*self.ptr }
    }
}

impl Drop for DrmModePlane {
    fn drop(&mut self) {
        unsafe {
            sys::drmModeFreePlane(self.ptr);
            self.ptr = ptr::null_mut();
        }
    }
}

struct DrmObjectProperties {
    ptr: sys::drmModeObjectPropertiesPtr,
}

impl DrmObjectProperties {
    fn count(&self) -> u32 {
        unsafe { (*self.ptr).count_props }
    }

    fn prop_id(&self, index: usize) -> u32 {
        unsafe { *(*self.ptr).props.add(index) }
    }

    fn prop_value(&self, index: usize) -> u64 {
        unsafe { *(*self.ptr).prop_values.add(index) }
    }
}

impl Drop for DrmObjectProperties {
    fn drop(&mut self) {
        unsafe {
            sys::drmModeFreeObjectProperties(self.ptr);
        }
    }
}

struct DrmProperty {
    ptr: sys::drmModePropertyPtr,
}

impl DrmProperty {
    fn name(&self) -> String {
        unsafe { CStr::from_ptr((*self.ptr).name.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for DrmProperty {
    fn drop(&mut self) {
        unsafe {
            sys::drmModeFreeProperty(self.ptr);
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

    #[test]
    fn drm_event_context_layout() {
        // version (4) + pad (4) + 4 function pointers on 64-bit
        assert_eq!(
            std::mem::size_of::<sys::drmEventContext>(),
            8 + 4 * std::mem::size_of::<*mut ()>()
        );
        assert_eq!(
            std::mem::offset_of!(sys::drmEventContext, page_flip_handler),
            16
        );
    }
}
