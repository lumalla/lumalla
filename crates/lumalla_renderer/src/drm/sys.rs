//! Shared libdrm FFI bindings used by connector probing and modesetting.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::{c_char, c_int, c_void};

pub const DRM_MODE_CONNECTED: u32 = 1;
pub const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
pub const DRM_MODE_FB_MODIFIERS: u32 = 1 << 1;
pub const DRM_DISPLAY_MODE_LEN: usize = 32;
pub const DRM_PROP_NAME_LEN: usize = 32;

pub const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub const DRM_CLIENT_CAP_ATOMIC: u64 = 3;

pub const DRM_MODE_OBJECT_CRTC: u32 = 0xcccccccc;
pub const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0c0c0;
pub const DRM_MODE_OBJECT_PLANE: u32 = 0xeeeeeeee;

pub const DRM_PLANE_TYPE_OVERLAY: u64 = 0;
pub const DRM_PLANE_TYPE_PRIMARY: u64 = 1;
pub const DRM_PLANE_TYPE_CURSOR: u64 = 2;

pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
pub const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
pub const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;

pub const DRM_EVENT_CONTEXT_VERSION: c_int = 4;

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

#[repr(C)]
pub struct drmModeObjectProperties {
    pub count_props: u32,
    pub props: *mut u32,
    pub prop_values: *mut u64,
}

pub type drmModeObjectPropertiesPtr = *mut drmModeObjectProperties;

#[repr(C)]
pub struct drmModePropertyRes {
    pub prop_id: u32,
    pub flags: u32,
    pub name: [c_char; DRM_PROP_NAME_LEN],
    pub count_values: c_int,
    pub values: *mut u64,
    pub count_enums: c_int,
    pub enums: *mut c_void,
    pub count_blobs: c_int,
    pub blob_ids: *mut u32,
}

pub type drmModePropertyPtr = *mut drmModePropertyRes;

#[repr(C)]
pub struct drmModePlane {
    pub count_formats: u32,
    pub formats: *mut u32,
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub crtc_x: u32,
    pub crtc_y: u32,
    pub x: u32,
    pub y: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
}

pub type drmModePlanePtr = *mut drmModePlane;

#[repr(C)]
pub struct drmModePlaneRes {
    pub count_planes: u32,
    pub planes: *mut u32,
}

pub type drmModePlaneResPtr = *mut drmModePlaneRes;

pub type drmModeAtomicReqPtr = *mut c_void;

pub type PageFlipHandler = Option<
    unsafe extern "C" fn(
        fd: c_int,
        sequence: u32,
        tv_sec: u32,
        tv_usec: u32,
        user_data: *mut c_void,
    ),
>;

pub type PageFlipHandler2 = Option<
    unsafe extern "C" fn(
        fd: c_int,
        sequence: u32,
        tv_sec: u32,
        tv_usec: u32,
        crtc_id: u32,
        user_data: *mut c_void,
    ),
>;

#[repr(C)]
pub struct drmEventContext {
    pub version: c_int,
    pub vblank_handler: *mut c_void,
    pub page_flip_handler: PageFlipHandler,
    pub page_flip_handler2: PageFlipHandler2,
    pub sequence_handler: *mut c_void,
}

unsafe extern "C" {
    pub fn drmSetClientCap(fd: c_int, capability: u64, value: u64) -> c_int;

    pub fn drmModeGetResources(fd: c_int) -> drmModeResPtr;
    pub fn drmModeFreeResources(ptr: drmModeResPtr);
    pub fn drmModeGetConnector(fd: c_int, connector_id: u32) -> drmModeConnectorPtr;
    pub fn drmModeFreeConnector(ptr: drmModeConnectorPtr);
    pub fn drmModeGetConnectorTypeName(connector_type: u32) -> *const c_char;
    pub fn drmModeGetEncoder(fd: c_int, encoder_id: u32) -> drmModeEncoderPtr;
    pub fn drmModeFreeEncoder(ptr: drmModeEncoderPtr);

    pub fn drmModeGetPlaneResources(fd: c_int) -> drmModePlaneResPtr;
    pub fn drmModeFreePlaneResources(ptr: drmModePlaneResPtr);
    pub fn drmModeGetPlane(fd: c_int, plane_id: u32) -> drmModePlanePtr;
    pub fn drmModeFreePlane(ptr: drmModePlanePtr);

    pub fn drmModeObjectGetProperties(
        fd: c_int,
        object_id: u32,
        object_type: u32,
    ) -> drmModeObjectPropertiesPtr;
    pub fn drmModeFreeObjectProperties(ptr: drmModeObjectPropertiesPtr);
    pub fn drmModeGetProperty(fd: c_int, property_id: u32) -> drmModePropertyPtr;
    pub fn drmModeFreeProperty(ptr: drmModePropertyPtr);

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

    pub fn drmModeAtomicAlloc() -> drmModeAtomicReqPtr;
    pub fn drmModeAtomicFree(req: drmModeAtomicReqPtr);
    pub fn drmModeAtomicAddProperty(
        req: drmModeAtomicReqPtr,
        object_id: u32,
        property_id: u32,
        value: u64,
    ) -> c_int;
    pub fn drmModeAtomicCommit(
        fd: c_int,
        req: drmModeAtomicReqPtr,
        flags: u32,
        user_data: *mut c_void,
    ) -> c_int;

    pub fn drmModeCreatePropertyBlob(
        fd: c_int,
        data: *const c_void,
        size: usize,
        id: *mut u32,
    ) -> c_int;
    pub fn drmModeDestroyPropertyBlob(fd: c_int, id: u32) -> c_int;

    pub fn drmPrimeFDToHandle(fd: c_int, prime_fd: c_int, handle: *mut u32) -> c_int;
    pub fn drmCloseBufferHandle(fd: c_int, handle: u32) -> c_int;

    pub fn drmHandleEvent(fd: c_int, evctx: *mut drmEventContext) -> c_int;
}
