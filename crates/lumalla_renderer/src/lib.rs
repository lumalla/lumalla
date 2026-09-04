use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use ash::vk;
use log::{debug, error, info, warn};
use lumalla_seat::SeatState;
use lumalla_shared::{BufferTransform, CapturedImage, DrmDeviceState, Output, OutputConfig};

pub mod drm;
pub mod vulkan;

mod default_cursor;
mod scanout_pool;
mod scene_backing;
pub mod scheduler;

use crate::drm::{
    CompletedPageFlip, ConnectedOutput, DrmDevices, DrmDispatchResult, FlipEventQueue, ModeBlob,
    atomic_modeset, atomic_page_flip, atomic_set_plane_fb, dispatch_drm_events,
    resolve_connected_output,
};
use crate::scanout_pool::{ScanoutBuffer, ScanoutBufferPool};
use crate::scene_backing::DamageRect;
pub use crate::scene_backing::{
    CompositeMode, DamageRect as OutputDamageRect, UploadRect, buffer_damage_to_upload_rect,
    clip_buffer_damage_list, clip_damage_list, cursor_damage_rects, cursor_damage_rects_default,
    prepare_gpu_composite, rect_union, union_damage_rects,
};
pub use crate::scheduler::{FrameTimings, RenderScheduler};
use crate::vulkan::{
    DmaBufImage, GpuCompositor, GpuWorkBatch, SurfaceTextureCache, VulkanContext,
    composite_to_scanout, copy_scanout_frame, download_bgra_region, vulkan_to_drm_fourcc,
};

struct GpuRenderResources {
    compositor: Option<GpuCompositor>,
    surface_textures: SurfaceTextureCache,
}

impl GpuRenderResources {
    fn new() -> Self {
        Self {
            compositor: None,
            surface_textures: SurfaceTextureCache::new(),
        }
    }

    fn clear(&mut self) {
        self.compositor = None;
        self.surface_textures.clear();
    }

    fn ensure_compositor(&mut self, vulkan: &mut VulkanContext) -> anyhow::Result<()> {
        if self.compositor.is_some() {
            return Ok(());
        }
        vulkan.ensure_scanout_render_pass()?;
        let render_pass = vulkan.scanout_render_pass()?;
        self.compositor = Some(GpuCompositor::new(vulkan.device(), render_pass)?);
        Ok(())
    }
}

/// Default clear color for enabled outputs (teal).
pub const SOLID_CLEAR_COLOR: [f32; 4] = [0.0, 0.55, 0.65, 1.0];
const WL_SHM_FORMAT_ARGB8888: u32 = 0;
const WL_SHM_FORMAT_XRGB8888: u32 = 1;

/// Outcome of a present or page-flip dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentStatus {
    /// No page-flips are currently in flight.
    pub idle: bool,
}

/// Outcome of a scheduled present pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentOutcome {
    /// Whether GPU/DRM work was submitted this call.
    pub presented: bool,
    pub status: PresentStatus,
    pub timings: Option<FrameTimings>,
}

/// Page-flip events drained from DRM fds.
#[derive(Debug, Clone)]
pub struct FlipDispatchOutcome {
    pub status: PresentStatus,
    pub completed: Vec<CompletedPageFlip>,
}

/// Client DMA-BUF attachment for zero-copy GPU import.
#[derive(Debug)]
pub struct DmabufAttachment {
    pub buffer_id: u32,
    pub fd: OwnedFd,
    pub drm_fourcc: u32,
    pub offset: u32,
    pub modifier: u64,
}

#[derive(Debug)]
pub struct SurfaceFrame {
    pub owner_id: u32,
    pub surface_id: u32,
    pub buffer_id: u32,
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
    pub x: i32,
    pub y: i32,
    pub buffer_scale: i32,
    pub buffer_transform: u32,
    /// Destination size in surface-local coordinates (after viewport).
    pub surface_width: i32,
    pub surface_height: i32,
    /// Source crop in post-scale surface coordinates, if set.
    pub viewport_src: Option<(f32, f32, f32, f32)>,
    pub dmabuf: Option<DmabufAttachment>,
    /// Output-space regions updated by this commit.
    pub damage: Vec<DamageRect>,
    /// Buffer-space regions updated by this commit.
    pub buffer_damage: Vec<DamageRect>,
    /// When true, the entire output backing must be recomposited.
    pub full_surface: bool,
}

impl SurfaceFrame {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.width > 0 && self.height > 0,
            "Surface frame dimensions must be non-zero"
        );
        let row_bytes = self
            .width
            .checked_mul(4)
            .context("Surface frame width overflows")?;
        anyhow::ensure!(
            self.stride >= row_bytes,
            "Surface frame stride is smaller than one row"
        );
        if self.dmabuf.is_none() {
            let required = self
                .stride
                .checked_mul(self.height)
                .context("Surface frame size overflows")?;
            anyhow::ensure!(
                self.pixels.len() >= required,
                "Surface frame pixel data is truncated"
            );
        }
        anyhow::ensure!(
            matches!(self.format, WL_SHM_FORMAT_ARGB8888 | WL_SHM_FORMAT_XRGB8888),
            "Unsupported Wayland SHM format {:#x}",
            self.format
        );
        anyhow::ensure!(
            BufferTransform::from_raw(self.buffer_transform).is_some(),
            "Unsupported buffer transform {}",
            self.buffer_transform
        );
        Ok(())
    }
}

#[derive(Debug)]
pub struct CursorFrame {
    pub owner_id: u32,
    pub surface_id: u32,
    pub buffer_id: u32,
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub buffer_scale: i32,
    pub buffer_transform: u32,
    pub dmabuf: Option<DmabufAttachment>,
}

impl CursorFrame {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.width > 0 && self.height > 0,
            "Cursor frame dimensions must be non-zero"
        );
        let row_bytes = self
            .width
            .checked_mul(4)
            .context("Cursor frame width overflows")?;
        anyhow::ensure!(
            self.stride >= row_bytes,
            "Cursor frame stride is smaller than one row"
        );
        if self.dmabuf.is_none() {
            let required = self
                .stride
                .checked_mul(self.height)
                .context("Cursor frame size overflows")?;
            anyhow::ensure!(
                self.pixels.len() >= required,
                "Cursor frame pixel data is truncated"
            );
        }
        anyhow::ensure!(
            matches!(self.format, WL_SHM_FORMAT_ARGB8888 | WL_SHM_FORMAT_XRGB8888),
            "Unsupported cursor SHM format {:#x}",
            self.format
        );
        anyhow::ensure!(
            BufferTransform::from_raw(self.buffer_transform).is_some(),
            "Unsupported cursor buffer transform {}",
            self.buffer_transform
        );
        Ok(())
    }
}

struct OutputScanout {
    drm_path: PathBuf,
    output: ConnectedOutput,
    /// Owned so the CRTC's MODE_ID blob is not destroyed while still active.
    #[allow(dead_code)]
    mode_blob: ModeBlob,
    /// Buffer currently owned by the CRTC (or last committed).
    current: ScanoutBuffer,
    /// Buffer waiting for page-flip completion; must not be dropped yet.
    pending: Option<ScanoutBuffer>,
    /// Newest buffer to flip after `pending` completes.
    queued: Option<ScanoutBuffer>,
}

pub struct RendererState {
    // Drop order: scanouts → scanout_pool → vulkan → drm_devices.
    drm_devices: DrmDevices,
    vulkan: Option<VulkanContext>,
    scanout_pool: ScanoutBufferPool,
    /// Configured render device (`None` = auto).
    render_device: Option<PathBuf>,
    /// Per-connector overrides; missing names use defaults (enabled if connected).
    output_configs: HashMap<String, OutputConfig>,
    scanouts: HashMap<String, OutputScanout>,
    /// Heap-stable queue pointer passed to DRM as page-flip `user_data`.
    flip_events: Box<FlipEventQueue>,
    /// Mapped surfaces in paint order (back to front).
    surface_frames: HashMap<(u32, u32), SurfaceFrame>,
    surface_order: Vec<(u32, u32)>,
    cursor_frame: Option<CursorFrame>,
    pointer_x: i32,
    pointer_y: i32,
    scene_dirty: bool,
    gpu: GpuRenderResources,
    pending_damage: Vec<DamageRect>,
    pending_surface_buffer_damage: HashMap<(u32, u32), DamageRect>,
    pending_full_redraw: bool,
    pending_pointer_damage: bool,
    dirty_surface_keys: HashSet<(u32, u32)>,
    cursor_buffer_dirty: bool,
}

impl RendererState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            drm_devices: DrmDevices::new()?,
            vulkan: None,
            scanout_pool: ScanoutBufferPool::new(),
            render_device: None,
            output_configs: HashMap::new(),
            scanouts: HashMap::new(),
            flip_events: Box::new(FlipEventQueue::new()),
            surface_frames: HashMap::new(),
            surface_order: Vec::new(),
            cursor_frame: None,
            pointer_x: 0,
            pointer_y: 0,
            scene_dirty: false,
            gpu: GpuRenderResources::new(),
            pending_damage: Vec::new(),
            pending_surface_buffer_damage: HashMap::new(),
            pending_full_redraw: false,
            pending_pointer_damage: false,
            dirty_surface_keys: HashSet::new(),
            cursor_buffer_dirty: false,
        })
    }

    fn invalidate_surface_textures(&mut self) {
        self.gpu.clear();
        self.pending_damage.clear();
        self.pending_surface_buffer_damage.clear();
        self.pending_full_redraw = true;
        self.pending_pointer_damage = false;
        self.dirty_surface_keys.clear();
        self.cursor_buffer_dirty = false;
    }

    fn note_pointer_damage(&mut self, new_x: i32, new_y: i32) {
        let old = (self.pointer_x, self.pointer_y);
        let damage = match self.cursor_frame.as_ref() {
            Some(cursor) => cursor_damage_rects(cursor, old, (new_x, new_y)),
            None => cursor_damage_rects_default(old, (new_x, new_y)),
        };
        self.pending_damage.extend(damage);
        self.pending_pointer_damage = true;
    }

    fn note_cursor_redraw(&mut self) {
        let rect = match self.cursor_frame.as_ref() {
            Some(cursor) => cursor_damage_rects(
                cursor,
                (self.pointer_x, self.pointer_y),
                (self.pointer_x, self.pointer_y),
            ),
            None => cursor_damage_rects_default(
                (self.pointer_x, self.pointer_y),
                (self.pointer_x, self.pointer_y),
            ),
        };
        self.pending_damage.extend(rect);
        self.pending_pointer_damage = true;
    }

    pub fn scene_dirty(&self) -> bool {
        self.scene_dirty
    }

    pub fn mark_scene_dirty(&mut self) {
        self.scene_dirty = true;
    }

    fn mark_dirty_if_active(&mut self) {
        if !self.drm_devices.opened().is_empty() {
            self.scene_dirty = true;
        }
    }

    /// Snapshot of discovered DRM devices and connectors, with render-device selection marked.
    pub fn drm_device_states(&self) -> Vec<DrmDeviceState> {
        let selected = self.resolved_render_device_path();
        self.drm_devices
            .device_states()
            .into_iter()
            .map(|mut state| {
                state.selected_render_device =
                    selected.as_ref().is_some_and(|path| path == &state.path);
                state
            })
            .collect()
    }

    /// Drain pending udev DRM events; update device paths and/or connectors.
    pub fn dispatch(&mut self) -> anyhow::Result<DrmDispatchResult> {
        self.drm_devices.dispatch()
    }

    /// Opened DRM primary-node paths and fds for event-loop registration.
    pub fn opened_drm_fds(&self) -> Vec<(PathBuf, RawFd)> {
        self.drm_devices
            .opened()
            .iter()
            .map(|(path, device)| (path.clone(), device.fd().as_raw_fd()))
            .collect()
    }

    /// Drain DRM page-flip events, retire buffers, and schedule queued flips.
    pub fn dispatch_page_flips(&mut self) -> anyhow::Result<FlipDispatchOutcome> {
        let fds: Vec<RawFd> = self
            .drm_devices
            .opened()
            .values()
            .map(|device| device.fd().as_raw_fd())
            .collect();
        for fd in fds {
            dispatch_drm_events(fd)?;
        }
        let completed = self.flip_events.drain();
        for flip in &completed {
            self.retire_page_flip(flip.crtc_id)?;
        }
        Ok(FlipDispatchOutcome {
            status: self.present_status(),
            completed,
        })
    }

    /// Replace or insert a surface frame without presenting.
    pub fn set_surface_frame(&mut self, frame: SurfaceFrame) -> anyhow::Result<()> {
        frame.validate()?;
        let key = (frame.owner_id, frame.surface_id);
        if !self.surface_frames.contains_key(&key) {
            self.surface_order.push(key);
        }
        if frame.full_surface {
            self.pending_full_redraw = true;
            self.pending_damage.clear();
            self.pending_surface_buffer_damage.remove(&key);
        } else {
            self.pending_damage.extend(frame.damage.iter().copied());
            if let Some(commit_rect) = union_damage_rects(frame.buffer_damage.iter().copied()) {
                match self.pending_surface_buffer_damage.get_mut(&key) {
                    Some(existing) => {
                        if let Some(merged) = rect_union(*existing, commit_rect) {
                            *existing = merged;
                        }
                    }
                    None => {
                        self.pending_surface_buffer_damage.insert(key, commit_rect);
                    }
                }
            }
        }
        self.dirty_surface_keys.insert(key);
        self.surface_frames.insert(key, frame);
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn update_surface_frame_position(
        &mut self,
        owner_id: u32,
        surface_id: u32,
        x: i32,
        y: i32,
    ) -> anyhow::Result<()> {
        let key = (owner_id, surface_id);
        let Some(frame) = self.surface_frames.get_mut(&key) else {
            return Ok(());
        };
        if frame.x == x && frame.y == y {
            return Ok(());
        }
        frame.x = x;
        frame.y = y;
        self.pending_full_redraw = true;
        self.pending_damage.clear();
        self.mark_dirty_if_active();
        Ok(())
    }

    /// Replace cached placement and z-order from the display's authoritative
    /// back-to-front scene.
    pub fn sync_surface_scene(&mut self, scene: &[(u32, u32, i32, i32)]) {
        let new_order: Vec<(u32, u32)> = scene
            .iter()
            .map(|(owner, surface, _, _)| (*owner, *surface))
            .filter(|key| self.surface_frames.contains_key(key))
            .collect();
        let mut changed = new_order != self.surface_order;
        for &(owner, surface, x, y) in scene {
            if let Some(frame) = self.surface_frames.get_mut(&(owner, surface))
                && (frame.x != x || frame.y != y)
            {
                frame.x = x;
                frame.y = y;
                changed = true;
            }
        }
        let visible: HashSet<(u32, u32)> = new_order.iter().copied().collect();
        let removed: Vec<(u32, u32)> = self
            .surface_frames
            .keys()
            .copied()
            .filter(|key| !visible.contains(key))
            .collect();
        for key in removed {
            self.surface_frames.remove(&key);
            self.gpu.surface_textures.remove(key);
            self.dirty_surface_keys.remove(&key);
            self.pending_surface_buffer_damage.remove(&key);
            changed = true;
        }
        self.surface_order = new_order;
        if changed {
            self.pending_full_redraw = true;
            self.pending_damage.clear();
            self.mark_dirty_if_active();
        }
    }

    pub fn remove_surface_frame(&mut self, owner_id: u32, surface_id: u32) -> anyhow::Result<()> {
        let key = (owner_id, surface_id);
        if self.surface_frames.remove(&key).is_some() {
            self.surface_order.retain(|k| *k != key);
            self.gpu.surface_textures.remove(key);
            self.dirty_surface_keys.remove(&key);
            self.pending_surface_buffer_damage.remove(&key);
            self.pending_full_redraw = true;
            self.pending_damage.clear();
            self.mark_dirty_if_active();
        }
        Ok(())
    }

    pub fn remove_client_frames(&mut self, owner_id: u32) -> anyhow::Result<()> {
        let before = self.surface_frames.len();
        self.surface_frames
            .retain(|(owner, _), _| *owner != owner_id);
        self.surface_order.retain(|(owner, _)| *owner != owner_id);
        self.gpu.surface_textures.remove_client(owner_id);
        self.pending_surface_buffer_damage
            .retain(|(owner, _), _| *owner != owner_id);
        let cursor_removed = self
            .cursor_frame
            .as_ref()
            .is_some_and(|cursor| cursor.owner_id == owner_id);
        if cursor_removed {
            self.cursor_frame = None;
        }
        if self.surface_frames.len() != before || cursor_removed {
            self.pending_full_redraw = true;
            self.pending_damage.clear();
            self.mark_dirty_if_active();
        }
        Ok(())
    }

    pub fn cursor_surface_key(&self) -> Option<(u32, u32)> {
        self.cursor_frame
            .as_ref()
            .map(|cursor| (cursor.owner_id, cursor.surface_id))
    }

    pub fn set_cursor_frame(&mut self, frame: CursorFrame) -> anyhow::Result<()> {
        frame.validate()?;
        self.cursor_frame = Some(frame);
        self.cursor_buffer_dirty = true;
        self.note_cursor_redraw();
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn clear_cursor_frame(&mut self) -> anyhow::Result<()> {
        if self.cursor_frame.is_none() {
            return Ok(());
        }
        self.note_cursor_redraw();
        self.cursor_frame = None;
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn update_pointer_position(&mut self, x: i32, y: i32) -> anyhow::Result<()> {
        if self.pointer_x == x && self.pointer_y == y {
            return Ok(());
        }
        self.note_pointer_damage(x, y);
        self.pointer_x = x;
        self.pointer_y = y;
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn update_cursor_hotspot(&mut self, hotspot_x: i32, hotspot_y: i32) -> anyhow::Result<()> {
        let Some(cursor) = self.cursor_frame.as_ref() else {
            return Ok(());
        };
        if cursor.hotspot_x == hotspot_x && cursor.hotspot_y == hotspot_y {
            return Ok(());
        }
        let old_hotspot = (cursor.hotspot_x, cursor.hotspot_y);
        let pointer = (self.pointer_x, self.pointer_y);
        let damage_for = |hotspot_x: i32, hotspot_y: i32| {
            let scratch = CursorFrame {
                owner_id: cursor.owner_id,
                surface_id: cursor.surface_id,
                buffer_id: cursor.buffer_id,
                pixels: Vec::new(),
                width: cursor.width,
                height: cursor.height,
                stride: cursor.stride,
                format: cursor.format,
                hotspot_x,
                hotspot_y,
                buffer_scale: cursor.buffer_scale,
                buffer_transform: cursor.buffer_transform,
                dmabuf: None,
            };
            cursor_damage_rects(&scratch, pointer, pointer)
        };
        self.pending_damage
            .extend(damage_for(old_hotspot.0, old_hotspot.1));
        self.pending_damage.extend(damage_for(hotspot_x, hotspot_y));
        self.pending_pointer_damage = true;
        if let Some(cursor) = self.cursor_frame.as_mut() {
            cursor.hotspot_x = hotspot_x;
            cursor.hotspot_y = hotspot_y;
        }
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn flip_idle(&self) -> bool {
        self.present_status().idle
    }

    /// Geometry of the first enabled present target, if DRM outputs are resolvable.
    pub fn primary_output_geometry(&self) -> Option<(String, i32, i32, i32)> {
        let target = self.collect_present_targets().into_iter().next()?;
        let refresh_mhz = (target.output.mode.refresh_hz() as i32).saturating_mul(1000);
        Some((
            target.connector_name,
            target.output.mode.width() as i32,
            target.output.mode.height() as i32,
            refresh_mhz.max(60_000),
        ))
    }

    /// Capture a rectangular region of the displayed scanouts as RGBA8 pixels.
    ///
    /// `x`/`y`/`width`/`height` are in global compositor (logical) space. `outputs`
    /// supplies layout for mapping that space onto DRM scanout buffers.
    pub fn capture_region(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        outputs: &[Output],
    ) -> anyhow::Result<CapturedImage> {
        anyhow::ensure!(width > 0 && height > 0, "capture region must be positive");
        let dest_w = width as u32;
        let dest_h = height as u32;
        let mut rgba = vec![0u8; (dest_w as usize) * (dest_h as usize) * 4];
        let mut covered = false;

        let regions: Vec<CaptureRegion> = outputs
            .iter()
            .filter_map(|output| CaptureRegion::intersect(output, x, y, width, height))
            .collect();

        for region in regions {
            if !self.scanouts.contains_key(&region.name) {
                continue;
            }

            if let Some(pending) = self
                .scanouts
                .get_mut(&region.name)
                .and_then(|scanout| scanout.current.gpu_pending.take())
            {
                let vulkan = self
                    .vulkan
                    .as_mut()
                    .context("Vulkan is not initialized for screenshot capture")?;
                pending.wait(vulkan.device(), vulkan.graphics_command_pool())?;
            }

            let fb_x = ((region.ix0 - region.ox) * region.scale) as u32;
            let fb_y = ((region.iy0 - region.oy) * region.scale) as u32;
            let fb_w = ((region.ix1 - region.ix0) * region.scale) as u32;
            let fb_h = ((region.iy1 - region.iy0) * region.scale) as u32;
            let logical_w = (region.ix1 - region.ix0) as u32;
            let logical_h = (region.iy1 - region.iy0) as u32;
            let dest_x = (region.ix0 - x) as u32;
            let dest_y = (region.iy0 - y) as u32;

            let vulkan = self
                .vulkan
                .as_ref()
                .context("Vulkan is not initialized for screenshot capture")?;
            let scanout = self
                .scanouts
                .get(&region.name)
                .context("scanout disappeared during capture")?;
            let format = scanout.current.dma_image.format();
            let bgra =
                download_bgra_region(vulkan, &scanout.current.dma_image, fb_x, fb_y, fb_w, fb_h)?;

            blit_bgra_to_rgba(
                &bgra, fb_w, fb_h, format, &mut rgba, dest_w, dest_h, dest_x, dest_y, logical_w,
                logical_h,
            )?;
            covered = true;
        }

        anyhow::ensure!(
            covered,
            "screenshot region does not intersect any DRM scanout"
        );

        Ok(CapturedImage {
            width: dest_w,
            height: dest_h,
            rgba,
        })
    }

    /// DRM format/modifier pairs clients may use with linux-dmabuf.
    ///
    /// Ensures Vulkan is initialized against the preferred render device so the
    /// advertised set matches what import will accept.
    pub fn supported_dmabuf_formats(&mut self) -> anyhow::Result<Vec<(u32, u64)>> {
        let Some(path) = self.resolved_render_device_path() else {
            return Ok(vec![
                (
                    crate::vulkan::DRM_FORMAT_XRGB8888,
                    crate::vulkan::DRM_FORMAT_MOD_LINEAR,
                ),
                (
                    crate::vulkan::DRM_FORMAT_ARGB8888,
                    crate::vulkan::DRM_FORMAT_MOD_LINEAR,
                ),
            ]);
        };
        self.ensure_vulkan(&path)?;
        let vulkan = self
            .vulkan
            .as_ref()
            .context("VulkanContext missing after ensure")?;
        Ok(vulkan.supported_dmabuf_formats())
    }

    /// DRM device path used for linux-dmabuf feedback `main_device` / tranche target.
    pub fn dmabuf_feedback_device_path(&mut self) -> Option<PathBuf> {
        self.resolved_render_device_path()
    }

    /// Open missing DRM devices via the seat (fresh open after VT resume).
    ///
    /// No-op when the seat backend cannot open devices (headless / no libseat).
    pub fn activate_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        if !seat.can_open_devices() {
            return Ok(());
        }
        self.drm_devices.activate(seat)
    }

    /// Close seat-opened DRM devices after session disable was acknowledged.
    pub fn deactivate_drm(&mut self, seat: &SeatState) {
        self.drain_scanouts();
        let _ = self.flip_events.drain();
        if seat.can_open_devices() {
            self.drm_devices.deactivate(seat);
        }
    }

    /// Close removed / open newly discovered DRM devices while the seat is active.
    pub fn reconcile_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        if !seat.can_open_devices() {
            return Ok(());
        }
        self.drain_scanouts();
        let _ = self.flip_events.drain();
        self.drm_devices.reconcile(seat)
    }

    fn drain_scanouts(&mut self) {
        let scanouts: Vec<OutputScanout> = self.scanouts.drain().map(|(_, s)| s).collect();
        for scanout in scanouts {
            self.release_output_scanout(scanout);
        }
        self.scanout_pool.clear();
        self.invalidate_surface_textures();
    }

    fn release_output_scanout(&mut self, scanout: OutputScanout) {
        self.release_scanout_buffer(scanout.current);
        if let Some(pending) = scanout.pending {
            self.release_scanout_buffer(pending);
        }
        if let Some(queued) = scanout.queued {
            self.release_scanout_buffer(queued);
        }
    }

    fn release_scanout_buffer(&mut self, mut buffer: ScanoutBuffer) {
        if let Err(err) = self.wait_scanout_gpu(&mut buffer) {
            warn!("Failed waiting for GPU work before releasing scanout buffer: {err:#}");
        }
        self.scanout_pool.release(buffer);
    }

    /// Select the Vulkan render device (`None` = auto).
    pub fn set_render_device(&mut self, path: Option<PathBuf>) -> anyhow::Result<()> {
        info!("Render device config: {path:?}");
        self.render_device = path;
        self.mark_dirty_if_active();
        Ok(())
    }

    /// Merge per-connector output config.
    pub fn set_output_configs(&mut self, configs: Vec<OutputConfig>) -> anyhow::Result<()> {
        for config in configs {
            info!(
                "Output config: {} enabled={} mode={:?}",
                config.name, config.enabled, config.mode_name
            );
            self.output_configs.insert(config.name.clone(), config);
        }
        self.mark_dirty_if_active();
        Ok(())
    }

    /// Run a present pass when `force` is set or the scene is dirty.
    pub fn present(&mut self, color: [f32; 4], force: bool) -> anyhow::Result<PresentOutcome> {
        if !force && !self.scene_dirty {
            return Ok(PresentOutcome {
                presented: false,
                status: self.present_status(),
                timings: None,
            });
        }
        self.scene_dirty = false;
        let started = Instant::now();
        let status = self.present_enabled_outputs(color)?;
        Ok(PresentOutcome {
            presented: true,
            status,
            timings: Some(FrameTimings {
                render_duration: started.elapsed(),
            }),
        })
    }

    /// Present a solid clear on every enabled connected output (any card).
    ///
    /// Buffers are allocated on the selected render GPU and imported on each
    /// output's DRM card (same- or cross-device). Failures are logged per output.
    ///
    /// Unchanged modes schedule a non-blocking page-flip; the previous FB stays
    /// alive until [`Self::dispatch_page_flips`] reports completion.
    pub fn present_enabled_outputs(&mut self, color: [f32; 4]) -> anyhow::Result<PresentStatus> {
        let Some(render_path) = self.resolved_render_device_path() else {
            warn!("No render device available; skipping presentation");
            return Ok(self.present_status());
        };

        debug!("Using render device {}", render_path.display());
        self.ensure_vulkan(&render_path)?;

        let targets = self.collect_present_targets();
        if targets.is_empty() {
            warn!("No enabled connected outputs to present");
            self.scanouts.clear();
            return Ok(self.present_status());
        }

        let keep: HashSet<String> = targets.iter().map(|t| t.connector_name.clone()).collect();
        let mut presented = 0usize;
        for target in targets {
            match self.present_one_output(&target, color) {
                Ok(()) => {
                    if let Some(scanout) = self.scanouts.get(&target.connector_name) {
                        debug!(
                            "Presented {} on {} (CRTC {}, {}x{}@{}Hz)",
                            scanout.output.connector_name,
                            scanout.drm_path.display(),
                            scanout.output.crtc_id,
                            scanout.output.mode.width(),
                            scanout.output.mode.height(),
                            scanout.output.mode.refresh_hz()
                        );
                    }
                    presented += 1;
                }
                Err(err) => {
                    error!(
                        "Failed to present {} on {}: {err:#}",
                        target.connector_name,
                        target.drm_path.display()
                    );
                }
            }
        }

        let stale: Vec<String> = self
            .scanouts
            .keys()
            .filter(|name| !keep.contains(*name))
            .cloned()
            .collect();
        for name in stale {
            if let Some(scanout) = self.scanouts.remove(&name) {
                self.release_output_scanout(scanout);
            }
        }
        debug!("Presented {presented} output(s)");
        Ok(self.present_status())
    }

    fn collect_present_targets(&self) -> Vec<PresentTarget> {
        let mut targets = Vec::new();

        for (drm_path, device) in self.drm_devices.opened() {
            let mut used_crtcs = HashSet::new();
            for connector in device.connectors() {
                if !connector.connected {
                    continue;
                }

                let config = self.output_configs.get(&connector.name);
                let enabled = config.map(|c| c.enabled).unwrap_or(true);
                if !enabled {
                    info!("Skipping disabled output {}", connector.name);
                    continue;
                }

                let mode_name = config.and_then(|c| c.mode_name.as_deref());
                match resolve_connected_output(
                    device.fd().as_raw_fd(),
                    connector.connector_id,
                    mode_name,
                    &mut used_crtcs,
                ) {
                    Ok(Some(output)) => {
                        targets.push(PresentTarget {
                            drm_path: drm_path.clone(),
                            connector_name: connector.name.clone(),
                            output,
                        });
                    }
                    Ok(None) => {}
                    Err(err) => {
                        error!(
                            "Failed to resolve output {} on {}: {err:#}",
                            connector.name,
                            drm_path.display()
                        );
                    }
                }
            }
        }

        targets.sort_by(|a, b| a.connector_name.cmp(&b.connector_name));
        targets
    }

    fn present_status(&self) -> PresentStatus {
        PresentStatus {
            idle: !self.has_pending_flips(),
        }
    }

    fn has_pending_flips(&self) -> bool {
        self.scanouts
            .values()
            .any(|scanout| scanout.pending.is_some())
    }

    fn present_one_output(
        &mut self,
        target: &PresentTarget,
        color: [f32; 4],
    ) -> anyhow::Result<()> {
        let mut buffer = self.render_scanout_buffer(target, color)?;

        let reuse_mode = self
            .scanouts
            .get(&target.connector_name)
            .is_some_and(|prev| {
                prev.drm_path == target.drm_path
                    && prev.output.connector_id == target.output.connector_id
                    && prev.output.crtc_id == target.output.crtc_id
                    && prev.output.plane_id == target.output.plane_id
                    && prev.output.mode == target.output.mode
            });

        if reuse_mode {
            return self.schedule_or_queue_flip(&target.connector_name, buffer);
        }

        if self
            .scanouts
            .get(&target.connector_name)
            .is_some_and(|scanout| scanout.pending.is_some())
        {
            self.wait_for_connector_flip(&target.connector_name)?;
        }

        self.wait_scanout_gpu(&mut buffer)?;

        let drm_device = self
            .drm_devices
            .opened()
            .get(&target.drm_path)
            .with_context(|| {
                format!("DRM device {} is no longer open", target.drm_path.display())
            })?;

        let mode_blob = ModeBlob::create(drm_device.fd(), &target.output.mode)
            .context("Failed to create MODE_ID property blob")?;

        atomic_modeset(
            drm_device.fd(),
            &target.output,
            mode_blob.id(),
            buffer.drm_fb.id(),
        )
        .context("Failed atomic modeset")?;

        let _previous = if let Some(old) = self.scanouts.remove(&target.connector_name) {
            self.release_output_scanout(old);
        };
        self.scanouts.insert(
            target.connector_name.clone(),
            OutputScanout {
                drm_path: target.drm_path.clone(),
                output: target.output.clone(),
                mode_blob,
                current: buffer,
                pending: None,
                queued: None,
            },
        );
        Ok(())
    }

    fn render_scanout_buffer(
        &mut self,
        target: &PresentTarget,
        color: [f32; 4],
    ) -> anyhow::Result<ScanoutBuffer> {
        let width = target.output.mode.width();
        let height = target.output.mode.height();
        let format = vk::Format::B8G8R8A8_UNORM;
        let fourcc = vulkan_to_drm_fourcc(format)
            .with_context(|| format!("Vulkan format {format:?} has no DRM fourcc mapping"))?;

        let drm_device = self
            .drm_devices
            .opened()
            .get(&target.drm_path)
            .with_context(|| {
                format!("DRM device {} is no longer open", target.drm_path.display())
            })?;

        let mut buffer = {
            let vulkan = self
                .vulkan
                .as_mut()
                .context("VulkanContext missing during present")?;
            self.scanout_pool.acquire(
                vulkan,
                &target.drm_path,
                drm_device.fd(),
                width,
                height,
                format,
                fourcc,
            )?
        };

        let _pending_damage = std::mem::take(&mut self.pending_damage);
        let pending_surface_buffer_damage = std::mem::take(&mut self.pending_surface_buffer_damage);
        let force_full = self.pending_full_redraw;
        let pointer_damage = self.pending_pointer_damage;
        let cursor_buffer_dirty = self.cursor_buffer_dirty;
        let dirty_surfaces = std::mem::take(&mut self.dirty_surface_keys);
        self.pending_full_redraw = false;
        self.pending_pointer_damage = false;
        self.cursor_buffer_dirty = false;

        let mut composite_mode =
            prepare_gpu_composite(width, height, &_pending_damage, force_full, buffer.fresh);

        let layers: Vec<&SurfaceFrame> = self
            .surface_order
            .iter()
            .filter_map(|key| self.surface_frames.get(key))
            .collect();
        let cursor = self.cursor_frame.as_ref();
        let pointer_x = self.pointer_x;
        let pointer_y = self.pointer_y;

        let mut batch = GpuWorkBatch::new();

        if matches!(composite_mode, CompositeMode::Partial(_)) {
            let src_ptr = self.scanouts.get(&target.connector_name).map(|scanout| {
                let image = scanout
                    .queued
                    .as_ref()
                    .or(scanout.pending.as_ref())
                    .unwrap_or(&scanout.current);
                &image.dma_image as *const DmaBufImage
            });
            let dst_ptr = &buffer.dma_image as *const DmaBufImage;
            let dst_fresh = buffer.fresh;
            if let Some(src_ptr) = src_ptr {
                let vulkan = self
                    .vulkan
                    .as_mut()
                    .context("VulkanContext missing during scanout copy")?;
                copy_scanout_frame(
                    vulkan,
                    &mut batch,
                    unsafe { &*src_ptr },
                    unsafe { &*dst_ptr },
                    dst_fresh,
                )
                .context("Failed to seed back buffer from current scanout")?;
                buffer.fresh = false;
            } else {
                composite_mode = CompositeMode::Full;
            }
        }

        {
            let vulkan = self
                .vulkan
                .as_mut()
                .context("VulkanContext missing during present")?;

            self.gpu.ensure_compositor(vulkan)?;
            let compositor = self
                .gpu
                .compositor
                .take()
                .context("GPU compositor missing after init")?;
            let gpu_result = (|| -> anyhow::Result<()> {
                self.gpu.surface_textures.sync_scene(
                    vulkan,
                    &compositor,
                    &mut batch,
                    &layers,
                    cursor,
                    &composite_mode,
                    &dirty_surfaces,
                    &pending_surface_buffer_damage,
                    &_pending_damage,
                    pointer_damage || cursor_buffer_dirty,
                )?;

                vulkan.ensure_scanout_render_pass()?;
                let render_pass = match &composite_mode {
                    CompositeMode::Full => vulkan.scanout_render_pass()?,
                    CompositeMode::Partial(_) => {
                        vulkan.ensure_scanout_render_pass_load()?;
                        vulkan.scanout_render_pass_load()?
                    }
                };
                let scanout_old_layout = if buffer.fresh {
                    vk::ImageLayout::UNDEFINED
                } else {
                    vk::ImageLayout::GENERAL
                };
                composite_to_scanout(
                    vulkan,
                    &mut batch,
                    &compositor,
                    &self.gpu.surface_textures,
                    render_pass,
                    &buffer.dma_image,
                    &buffer.framebuffer,
                    scanout_old_layout,
                    width,
                    height,
                    color,
                    composite_mode,
                    &layers,
                    cursor,
                    pointer_x,
                    pointer_y,
                )
                .context("Failed to GPU-composite scene to scanout buffer")?;
                Ok(())
            })();
            if let Err(error) = gpu_result {
                self.gpu.surface_textures.clear();
                self.gpu.compositor = Some(compositor);
                return Err(error);
            }
            self.gpu.compositor = Some(compositor);

            buffer.gpu_pending = Some(batch.submit(vulkan.device())?);
            buffer.fresh = false;
        }

        Ok(buffer)
    }

    fn schedule_or_queue_flip(
        &mut self,
        connector_name: &str,
        mut buffer: ScanoutBuffer,
    ) -> anyhow::Result<()> {
        let (drm_path, output, flip_busy) = {
            let scanout = self
                .scanouts
                .get(connector_name)
                .context("Missing scanout for page-flip")?;
            (
                scanout.drm_path.clone(),
                scanout.output.clone(),
                scanout.pending.is_some(),
            )
        };

        if flip_busy {
            let scanout = self
                .scanouts
                .get_mut(connector_name)
                .context("Missing scanout while queueing flip")?;
            if let Some(old) = scanout.queued.replace(buffer) {
                self.release_scanout_buffer(old);
            }
            return Ok(());
        }

        // Overlap CPU flip prep with GPU: wait only when the buffer must be scanout-ready.
        self.wait_scanout_gpu(&mut buffer)?;

        let fb_id = buffer.drm_fb_id();
        let flip_result = {
            let device =
                self.drm_devices.opened().get(&drm_path).with_context(|| {
                    format!("DRM device {} is no longer open", drm_path.display())
                })?;
            atomic_page_flip(device.fd(), &output, fb_id, self.flip_events.as_ref())
        };

        match flip_result {
            Ok(()) => {
                let scanout = self
                    .scanouts
                    .get_mut(connector_name)
                    .context("Missing scanout after scheduling flip")?;
                scanout.pending = Some(buffer);
                Ok(())
            }
            Err(err) => {
                warn!("Async page-flip failed on {connector_name}: {err:#}; using blocking update");
                {
                    let device = self.drm_devices.opened().get(&drm_path).with_context(|| {
                        format!("DRM device {} is no longer open", drm_path.display())
                    })?;
                    atomic_set_plane_fb(device.fd(), &output, fb_id)
                        .context("Failed blocking plane FB update after page-flip error")?;
                }
                let (old, _) = {
                    let scanout = self
                        .scanouts
                        .get_mut(connector_name)
                        .context("Missing scanout after blocking flip fallback")?;
                    let old = std::mem::replace(&mut scanout.current, buffer);
                    scanout.pending = None;
                    scanout.queued = None;
                    (old, ())
                };
                self.release_scanout_buffer(old);
                Ok(())
            }
        }
    }

    fn wait_scanout_gpu(&mut self, buffer: &mut ScanoutBuffer) -> anyhow::Result<()> {
        let Some(pending) = buffer.gpu_pending.take() else {
            return Ok(());
        };
        let vulkan = self
            .vulkan
            .as_mut()
            .context("VulkanContext missing while waiting for scanout GPU work")?;
        pending.wait(vulkan.device(), vulkan.graphics_command_pool())
    }

    fn retire_page_flip(&mut self, crtc_id: u32) -> anyhow::Result<()> {
        let Some(connector_name) = self
            .scanouts
            .iter()
            .find_map(|(name, scanout)| (scanout.output.crtc_id == crtc_id).then(|| name.clone()))
        else {
            warn!("Ignoring page-flip completion for unknown CRTC {crtc_id}");
            return Ok(());
        };

        let queued = {
            let scanout = self
                .scanouts
                .get_mut(&connector_name)
                .context("Missing scanout during flip retirement")?;
            let Some(new_current) = scanout.pending.take() else {
                warn!("Page-flip completion without pending buffer on {connector_name}");
                return Ok(());
            };
            let old = std::mem::replace(&mut scanout.current, new_current);
            let queued = scanout.queued.take();
            (old, queued)
        };
        let (old, queued) = queued;
        self.release_scanout_buffer(old);

        if let Some(queued) = queued {
            self.schedule_or_queue_flip(&connector_name, queued)?;
        }
        Ok(())
    }

    fn wait_for_connector_flip(&mut self, connector_name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.scanouts.contains_key(connector_name),
            "Missing scanout while waiting for flip"
        );

        for _ in 0..1_000 {
            if !self
                .scanouts
                .get(connector_name)
                .is_some_and(|scanout| scanout.pending.is_some())
            {
                return Ok(());
            }

            let fds: Vec<RawFd> = self
                .drm_devices
                .opened()
                .values()
                .map(|device| device.fd().as_raw_fd())
                .collect();
            for fd in fds {
                dispatch_drm_events(fd)?;
            }
            let completed = self.flip_events.drain();
            if completed.is_empty() {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            for flip in completed {
                self.retire_page_flip(flip.crtc_id)?;
            }
        }

        anyhow::bail!("Timed out waiting for page-flip on {connector_name}")
    }

    fn resolved_render_device_path(&self) -> Option<PathBuf> {
        if let Some(path) = &self.render_device {
            if self.drm_devices.opened().contains_key(path) {
                return Some(path.clone());
            }
            warn!(
                "Configured render device {} is not open; falling back to auto",
                path.display()
            );
        }
        self.auto_render_device_path()
    }

    fn auto_render_device_path(&self) -> Option<PathBuf> {
        let mut best: Option<(PathBuf, i32)> = None;
        for (path, device) in self.drm_devices.opened() {
            let connected = device.connectors().iter().any(|c| c.connected);
            let mut score = if connected { 1000 } else { 0 };
            // Prefer lower card numbers slightly as a stable tie-break.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if let Some(num) = name
                    .strip_prefix("card")
                    .and_then(|s| s.parse::<i32>().ok())
                {
                    score -= num;
                }
            }
            if best
                .as_ref()
                .is_none_or(|(_, best_score)| score > *best_score)
            {
                best = Some((path.clone(), score));
            }
        }
        best.map(|(path, _)| path)
    }

    fn ensure_vulkan(&mut self, preferred_drm_path: &Path) -> anyhow::Result<()> {
        let needs_recreate = match &self.vulkan {
            None => true,
            Some(vk) => match vk.drm_device_path() {
                Some(selected) => selected != preferred_drm_path,
                None => false,
            },
        };

        if needs_recreate {
            self.drain_scanouts();
            info!(
                "Initializing Vulkan for DRM device {}",
                preferred_drm_path.display()
            );
            self.vulkan = Some(VulkanContext::new(Some(preferred_drm_path))?);
        }

        Ok(())
    }
}

struct CaptureRegion {
    name: String,
    ox: i32,
    oy: i32,
    scale: i32,
    ix0: i32,
    iy0: i32,
    ix1: i32,
    iy1: i32,
}

impl CaptureRegion {
    fn intersect(output: &Output, x: i32, y: i32, width: i32, height: i32) -> Option<Self> {
        let ox = output.location.0;
        let oy = output.location.1;
        let ow = output.size.0;
        let oh = output.size.1;
        if ow <= 0 || oh <= 0 {
            return None;
        }
        let ix0 = x.max(ox);
        let iy0 = y.max(oy);
        let ix1 = (x + width).min(ox + ow);
        let iy1 = (y + height).min(oy + oh);
        if ix0 >= ix1 || iy0 >= iy1 {
            return None;
        }
        Some(Self {
            name: output.name.clone(),
            ox,
            oy,
            scale: output.scale.max(1),
            ix0,
            iy0,
            ix1,
            iy1,
        })
    }
}

/// Nearest-neighbor blit from a BGRA/RGBA GPU download into an RGBA destination.
fn blit_bgra_to_rgba(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    format: vk::Format,
    dest: &mut [u8],
    dest_w: u32,
    dest_h: u32,
    dest_x: u32,
    dest_y: u32,
    dest_region_w: u32,
    dest_region_h: u32,
) -> anyhow::Result<()> {
    let src_stride = (src_w as usize)
        .checked_mul(4)
        .context("source stride overflow")?;
    anyhow::ensure!(
        src.len() >= src_stride.saturating_mul(src_h as usize),
        "source buffer too small for {}x{}",
        src_w,
        src_h
    );
    anyhow::ensure!(
        dest_x.saturating_add(dest_region_w) <= dest_w
            && dest_y.saturating_add(dest_region_h) <= dest_h,
        "destination region out of bounds"
    );

    let swap_rb = matches!(
        format,
        vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB
    );

    for dy in 0..dest_region_h {
        let sy = if dest_region_h == src_h {
            dy
        } else {
            dy * src_h / dest_region_h
        };
        for dx in 0..dest_region_w {
            let sx = if dest_region_w == src_w {
                dx
            } else {
                dx * src_w / dest_region_w
            };
            let si = (sy as usize) * src_stride + (sx as usize) * 4;
            let di = (((dest_y + dy) as usize) * (dest_w as usize) + ((dest_x + dx) as usize)) * 4;
            if swap_rb {
                dest[di] = src[si + 2];
                dest[di + 1] = src[si + 1];
                dest[di + 2] = src[si];
                dest[di + 3] = src[si + 3];
            } else {
                dest[di..di + 4].copy_from_slice(&src[si..si + 4]);
            }
        }
    }
    Ok(())
}

struct PresentTarget {
    drm_path: PathBuf,
    connector_name: String,
    output: ConnectedOutput,
}

impl RendererState {
    pub fn udev_monitor_fd(&self) -> RawFd {
        self.drm_devices.monitor_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_backing::{composite_cursor_into, composite_surface_full};

    fn frame() -> SurfaceFrame {
        SurfaceFrame {
            owner_id: 1,
            surface_id: 2,
            buffer_id: 3,
            pixels: vec![0; 16],
            width: 2,
            height: 2,
            stride: 8,
            format: 0,
            x: 0,
            y: 0,
            buffer_scale: 1,
            buffer_transform: 0,
            surface_width: 2,
            surface_height: 2,
            viewport_src: None,
            dmabuf: None,
            damage: Vec::new(),
            buffer_damage: Vec::new(),
            full_surface: true,
        }
    }

    #[test]
    fn accumulates_disjoint_buffer_damage_across_commits() {
        let mut state = RendererState::new().unwrap();
        let key = (1, 2);
        let mut first = frame();
        first.full_surface = false;
        first.buffer_damage = vec![DamageRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        }];
        state.set_surface_frame(first).unwrap();

        let mut second = frame();
        second.buffer_id = 4;
        second.full_surface = false;
        second.buffer_damage = vec![DamageRect {
            x: 8,
            y: 0,
            width: 4,
            height: 4,
        }];
        state.set_surface_frame(second).unwrap();

        let accumulated = state
            .pending_surface_buffer_damage
            .get(&key)
            .expect("buffer damage should accumulate per surface");
        assert_eq!(accumulated.x, 0);
        assert_eq!(accumulated.width, 12);
    }

    #[test]
    fn authoritative_scene_sync_reorders_and_moves_existing_frames() {
        let mut state = RendererState::new().unwrap();
        let first = frame();
        let mut second = frame();
        second.surface_id = 3;
        second.buffer_id = 4;
        state.set_surface_frame(first).unwrap();
        state.set_surface_frame(second).unwrap();

        state.sync_surface_scene(&[(1, 3, 40, 50), (1, 2, 10, 20)]);

        assert_eq!(state.surface_order, vec![(1, 3), (1, 2)]);
        assert_eq!(
            (
                state.surface_frames[&(1, 3)].x,
                state.surface_frames[&(1, 3)].y
            ),
            (40, 50)
        );
        assert_eq!(
            (
                state.surface_frames[&(1, 2)].x,
                state.surface_frames[&(1, 2)].y
            ),
            (10, 20)
        );
        assert!(state.pending_full_redraw);
    }

    #[test]
    fn authoritative_scene_sync_removes_invisible_frames() {
        let mut state = RendererState::new().unwrap();
        state.set_surface_frame(frame()).unwrap();
        state.sync_surface_scene(&[]);
        assert!(state.surface_frames.is_empty());
        assert!(state.surface_order.is_empty());
    }

    #[test]
    fn validates_surface_frame_layout() {
        assert!(frame().validate().is_ok());

        let mut truncated = frame();
        truncated.pixels.pop();
        assert!(truncated.validate().is_err());

        let mut short_stride = frame();
        short_stride.stride = 4;
        assert!(short_stride.validate().is_err());

        let mut unknown_format = frame();
        unknown_format.format = 0xdead_beef;
        assert!(unknown_format.validate().is_err());
    }

    #[test]
    fn composites_xrgb_at_origin_with_clear() {
        let frame = SurfaceFrame {
            owner_id: 1,
            surface_id: 2,
            buffer_id: 3,
            pixels: vec![1, 2, 3, 0, 4, 5, 6, 0],
            width: 2,
            height: 1,
            stride: 8,
            format: WL_SHM_FORMAT_XRGB8888,
            x: 0,
            y: 0,
            buffer_scale: 1,
            buffer_transform: 0,
            surface_width: 2,
            surface_height: 1,
            viewport_src: None,
            dmabuf: None,
            damage: Vec::new(),
            buffer_damage: Vec::new(),
            full_surface: true,
        };

        let mut upload = composite_surface_full(&[&frame], 3, 1, [0.0, 0.0, 0.0, 1.0]).unwrap();
        assert_eq!(upload.len(), 3 * 1 * 4);
        assert_eq!(upload, vec![1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn composites_frame_at_offset() {
        let frame = SurfaceFrame {
            pixels: vec![9, 8, 7, 6],
            width: 1,
            height: 1,
            stride: 4,
            format: WL_SHM_FORMAT_ARGB8888,
            x: 1,
            y: 0,
            buffer_scale: 1,
            buffer_transform: 0,
            surface_width: 1,
            surface_height: 1,
            damage: Vec::new(),
            full_surface: true,
            ..frame()
        };
        let upload = composite_surface_full(&[&frame], 2, 1, [0.0, 0.0, 0.0, 1.0]).unwrap();
        assert_eq!(upload, vec![0, 0, 0, 255, 9, 8, 7, 255]);
    }

    #[test]
    fn composites_cursor_with_hotspot() {
        let cursor = CursorFrame {
            owner_id: 1,
            surface_id: 3,
            buffer_id: 4,
            pixels: vec![10, 20, 30, 255],
            width: 1,
            height: 1,
            stride: 4,
            format: WL_SHM_FORMAT_ARGB8888,
            hotspot_x: 0,
            hotspot_y: 0,
            buffer_scale: 1,
            buffer_transform: 0,
            dmabuf: None,
        };
        let mut upload = composite_surface_full(&[], 2, 1, [0.0, 0.0, 0.0, 1.0]).unwrap();
        composite_cursor_into(&mut upload, 2, 1, &cursor, 1, 0).unwrap();
        assert_eq!(upload, vec![0, 0, 0, 255, 10, 20, 30, 255]);
    }
}
