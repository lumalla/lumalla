//! Phase-local collaboration between display (Wayland) and renderer (GPU/KMS).
//!
//! Display calls [`RenderSink`] while handling client requests / layout changes.
//! Renderer calls [`PresentationNotify`] after present / page-flip.

use std::os::fd::OwnedFd;
use std::rc::Rc;

use anyhow::Result;

/// Axis-aligned damage / clip rectangle in integer coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// DMA-BUF attachment carried with a surface submit.
#[derive(Debug)]
pub struct SurfaceDmabuf {
    pub buffer_id: u32,
    pub fd: OwnedFd,
    pub drm_fourcc: u32,
    pub offset: u32,
    pub modifier: u64,
}

/// How a submitted surface should be used by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceSubmitRole {
    /// Normal desktop / layer content.
    Content,
    /// Pointer image; hotspot is in buffer coordinates.
    Cursor { hotspot_x: i32, hotspot_y: i32 },
}

/// One surface buffer submission from display → renderer.
#[derive(Debug)]
pub struct SurfaceSubmit {
    pub owner_id: u32,
    pub surface_id: u32,
    pub buffer_id: u32,
    pub pixels: Rc<Vec<u8>>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
    pub x: i32,
    pub y: i32,
    pub buffer_scale: i32,
    pub buffer_transform: u32,
    pub surface_width: i32,
    pub surface_height: i32,
    pub viewport_src: Option<(f32, f32, f32, f32)>,
    pub dmabuf: Option<SurfaceDmabuf>,
    pub damage: Option<DamageRect>,
    pub buffer_damage: Option<DamageRect>,
    pub full_surface: bool,
    pub role: SurfaceSubmitRole,
}

/// Presentation timing for a completed DRM page-flip (or virtual present).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationFlipInfo {
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub refresh_ns: u32,
}

/// Display → renderer sink used during protocol / layout handling.
pub trait RenderSink {
    fn submit_surface(&mut self, submit: SurfaceSubmit) -> Result<()>;
    fn remove_surface(&mut self, owner_id: u32, surface_id: u32) -> Result<()>;
    fn remove_buffer(&mut self, owner_id: u32, buffer_id: u32) -> Result<()>;
    fn remove_client(&mut self, owner_id: u32) -> Result<()>;

    /// Authoritative back-to-front desktop scene: `(owner, surface, x, y)`.
    fn set_desktop_scene(&mut self, scene: &[(u32, u32, i32, i32)]);

    /// Authoritative per-output layer scenes: `(owner, surface, x, y, band)`.
    fn set_output_layer_scenes(&mut self, scenes: &[(String, Vec<(u32, u32, i32, i32, u8)>)]);

    fn update_surface_position(
        &mut self,
        owner_id: u32,
        surface_id: u32,
        x: i32,
        y: i32,
    ) -> Result<()>;

    fn update_cursor_hotspot(&mut self, hotspot_x: i32, hotspot_y: i32) -> Result<()>;
    fn hide_cursor(&mut self) -> Result<()>;
    fn clear_cursor(&mut self) -> Result<()>;

    /// Software / HW cursor position in global compositor space.
    fn update_pointer_position(&mut self, x: i32, y: i32) -> Result<()>;

    /// Mark that a present is needed (content change and/or pending frame callbacks).
    fn request_present(&mut self);
}

/// Renderer → display notifications after present / flip.
pub trait PresentationNotify {
    fn presentation_completed(&mut self, info: PresentationFlipInfo);
    fn frames_completed(&mut self, time_msec: u32);
    fn pending_presentation_feedback(&self) -> bool;
    fn pending_frame_callbacks(&self) -> bool;
    fn pending_present_work(&self) -> bool {
        self.pending_presentation_feedback() || self.pending_frame_callbacks()
    }
}
