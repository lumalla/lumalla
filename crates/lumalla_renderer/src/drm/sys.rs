//! Shared libdrm FFI bindings used by connector probing and modesetting.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::{c_char, c_int};

pub const DRM_MODE_CONNECTED: u32 = 1;
pub const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
pub const DRM_MODE_FB_MODIFIERS: u32 = 1 << 1;
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
#[derive(Clone, Copy)]
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

/// Partial `drmModeConnector` — fields through `encoders`.
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

#[repr(C)]
pub struct drmModeEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

pub type drmModeEncoderPtr = *mut drmModeEncoder;

unsafe extern "C" {
    pub fn drmModeGetResources(fd: c_int) -> drmModeResPtr;
    pub fn drmModeFreeResources(ptr: drmModeResPtr);
    pub fn drmModeGetConnector(fd: c_int, connector_id: u32) -> drmModeConnectorPtr;
    pub fn drmModeFreeConnector(ptr: drmModeConnectorPtr);
    pub fn drmModeGetConnectorTypeName(connector_type: u32) -> *const c_char;
    pub fn drmModeGetEncoder(fd: c_int, encoder_id: u32) -> drmModeEncoderPtr;
    pub fn drmModeFreeEncoder(ptr: drmModeEncoderPtr);
    pub fn drmModeAddFB2WithModifiers(
        fd: c_int,
        width: u32,
        height: u32,
        pixel_format: u32,
        bo_handles: *const u32,
        pitches: *const u32,
        offsets: *const u32,
        modifier: *const u64,
        buf_id: *mut u32,
        flags: u32,
    ) -> c_int;
    pub fn drmModeRmFB(fd: c_int, buffer_id: u32) -> c_int;
    pub fn drmModeSetCrtc(
        fd: c_int,
        crtc_id: u32,
        buffer_id: u32,
        x: u32,
        y: u32,
        connectors: *mut u32,
        count: c_int,
        mode: *mut drmModeModeInfo,
    ) -> c_int;
    pub fn drmPrimeFDToHandle(fd: c_int, prime_fd: c_int, handle: *mut u32) -> c_int;
    pub fn drmCloseBufferHandle(fd: c_int, handle: u32) -> c_int;
}
