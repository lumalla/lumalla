use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use ash::vk;
use log::{debug, error, info, warn};
use lumalla_seat::SeatState;
use lumalla_shared::{
    BufferTransform, CapturedImage, DrmDeviceState, Guide, Output, OutputConfig, View,
    view_at_source,
};
use stumpalo::Arena;

pub mod drm;
pub mod vulkan;

mod bitmap_font;
mod default_cursor;
mod output;
mod present_control;
mod scanout_pool;
mod scene_backing;
pub mod scheduler;

use crate::drm::{
    CompletedPageFlip, ConnectedOutput, DrmDevices, DrmDispatchResult, FlipEventQueue, ModeBlob,
    atomic_disable_output, atomic_modeset, atomic_page_flip, atomic_set_cursor_plane,
    atomic_set_plane_fb, dispatch_drm_events, probe_device_topology, resolve_connected_output,
};
pub use crate::present_control::{
    CompletedFlipEffect, FlipSideEffects, PRESENT_WAKE_TOKEN_BASE, PRESENT_WAKE_TOKEN_COUNT,
    PresentTickResult, is_present_wake_token,
};
use crate::present_control::NamedFlipDispatchOutcome;
use crate::output::{
    DrmDeviceTopology, DrmPlaneInfo, HW_CURSOR_MAX_SIZE, OutputId, OutputPresentControl,
    OutputState, PhysicalOutput, PlaneGeometry,
};
use crate::scanout_pool::{ScanoutBuffer, ScanoutBufferPool};
use crate::scene_backing::DamageRect;
pub use crate::scene_backing::{
    CompositeMode, DamageRect as OutputDamageRect, UploadRect, buffer_damage_to_upload_rect,
    clip_buffer_damage_list, clip_damage_list, cursor_damage_rects, cursor_damage_rects_default,
    expand_damage_for_buffer_age, prepare_gpu_composite, rect_union, union_damage_rects,
};
pub use crate::scheduler::{FrameTimings, RenderScheduler};
use crate::vulkan::{
    DRM_FORMAT_ARGB8888, DmaBufImage, Framebuffer, GpuCompositor, GpuWorkBatch,
    SurfaceTextureCache, VulkanContext, blit_image_region, clear_dma_image_color,
    composite_layers_to_image, composite_to_scanout, copy_scanout_frame, download_bgra_region,
    map_rect_through_view, overlay_cursor_on_image, upload_bgra_to_image, vulkan_to_drm_fourcc,
};

struct GpuRenderResources {
    compositor: Option<GpuCompositor>,
    surface_textures: SurfaceTextureCache,
    guide_labels: crate::bitmap_font::GuideLabelCache,
}

impl GpuRenderResources {
    fn new() -> Self {
        Self {
            compositor: None,
            surface_textures: SurfaceTextureCache::new(),
            guide_labels: crate::bitmap_font::GuideLabelCache::new(),
        }
    }

    fn clear(&mut self) {
        self.compositor = None;
        self.surface_textures.clear();
        self.guide_labels.clear();
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

/// Whether a present/page-flip error means further presents will only spam logs.
fn is_unrecoverable_present_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(vk_err) = cause.downcast_ref::<vk::Result>()
            && matches!(
                *vk_err,
                vk::Result::ERROR_DEVICE_LOST | vk::Result::ERROR_SURFACE_LOST_KHR
            )
        {
            return true;
        }
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>()
            && io_err.kind() == std::io::ErrorKind::PermissionDenied
        {
            return true;
        }
    }
    let msg = format!("{err:#}");
    msg.contains("ERROR_DEVICE_LOST")
        || msg.contains("device has been lost")
        || msg.contains("Permission denied")
}

fn is_drm_permission_denied(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::PermissionDenied)
            || cause.to_string().contains("Permission denied")
    })
}

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

/// What to composite for the pointer image.
#[derive(Debug)]
pub enum CursorState {
    /// Compositor theme / built-in default cursor.
    Default,
    /// Client explicitly hid the cursor; draw nothing.
    Hidden,
    /// Client-provided cursor buffer.
    Client(CursorFrame),
}

impl CursorState {
    pub fn as_client(&self) -> Option<&CursorFrame> {
        match self {
            Self::Client(frame) => Some(frame),
            Self::Default | Self::Hidden => None,
        }
    }

    pub fn as_client_mut(&mut self) -> Option<&mut CursorFrame> {
        match self {
            Self::Client(frame) => Some(frame),
            Self::Default | Self::Hidden => None,
        }
    }

    pub fn surface_key(&self) -> Option<(u32, u32)> {
        self.as_client()
            .map(|cursor| (cursor.owner_id, cursor.surface_id))
    }

    pub fn draw_ref(&self) -> CursorDraw<'_> {
        match self {
            Self::Default => CursorDraw::Default,
            Self::Hidden => CursorDraw::Hidden,
            Self::Client(frame) => CursorDraw::Client(frame),
        }
    }
}

/// Borrowed cursor draw intent passed into compositors.
#[derive(Debug, Clone, Copy)]
pub enum CursorDraw<'a> {
    Default,
    Hidden,
    Client(&'a CursorFrame),
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

/// Config-created output that presents without KMS export/modeset.
#[derive(Debug, Clone)]
struct VirtualOutput {
    name: String,
    width: u32,
    height: u32,
    refresh_mhz: i32,
}

pub struct RendererState {
    // Drop order: outputs → scanout_pool → vulkan → drm_devices.
    drm_devices: DrmDevices,
    /// CRTC/plane inventory per open DRM card.
    topologies: HashMap<PathBuf, DrmDeviceTopology>,
    vulkan: Option<VulkanContext>,
    scanout_pool: ScanoutBufferPool,
    /// Configured render device (`None` = auto).
    render_device: Option<PathBuf>,
    /// Per-connector overrides; missing names use defaults (enabled if connected).
    output_configs: HashMap<String, OutputConfig>,
    /// Virtual outputs registered from config (`add_output` with `virtual = true`).
    virtual_outputs: HashMap<String, VirtualOutput>,
    /// Views keyed by output name; empty means the output presents clear color only.
    output_views: HashMap<String, Vec<View>>,
    /// Compositor-drawn guides (scene space), back-to-front within each layer.
    guides: Vec<Guide>,
    /// Per-output plane pipelines + present pacing.
    outputs: HashMap<String, OutputState>,
    next_present_wake_token: u64,
    free_present_wake_tokens: Vec<u64>,
    /// Cached modeset-resolved present targets; invalidated on hotplug/config.
    cached_present_targets: Option<Vec<PresentTarget>>,
    /// Heap-stable queue pointer passed to DRM as page-flip `user_data`.
    flip_events: Box<FlipEventQueue>,
    /// Mapped surfaces in paint order (back to front).
    surface_frames: HashMap<(u32, u32), SurfaceFrame>,
    surface_order: Vec<(u32, u32)>,
    cursor_state: CursorState,
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
    /// When set, presents are skipped until DRM is reactivated (e.g. after VT return).
    present_halted: Option<String>,
    /// PipeWire DMA-BUF capture buffers keyed by stream id.
    screencast_buffers: HashMap<u32, Vec<ScreencastDmaSlot>>,
    /// Bumped when captured scene content may have changed (windows, cursor, guides…).
    screencast_content_serial: u64,
}

/// DMA-BUF handle handed to the PipeWire thread for one capture slot.
#[derive(Debug)]
pub struct ScreencastDmaExport {
    /// Index into the stream's buffer pool.
    pub index: usize,
    /// Duplicated DMA-BUF fd (PipeWire owns this clone).
    pub fd: OwnedFd,
    /// Buffer width in pixels.
    pub width: u32,
    /// Buffer height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub stride: u32,
    /// Byte offset into the DMA-BUF.
    pub offset: u32,
    /// Plane 0 byte size (may exceed stride×height for tiled modifiers).
    pub size: u32,
    /// DRM format modifier.
    pub modifier: u64,
}

struct ScreencastDmaSlot {
    image: DmaBufImage,
    framebuffer: Framebuffer,
    /// Original export fd kept alive for the image memory.
    _export_fd: OwnedFd,
    in_use: bool,
    /// True until the first GPU fill (layout still UNDEFINED).
    fresh: bool,
    /// In-flight GPU fill; must complete before PipeWire queues the buffer.
    gpu_pending: Option<crate::vulkan::PendingGpuSubmit>,
    /// Content serial last written into this slot (for unchanged-frame skip).
    content_serial: Option<u64>,
}

fn dup_owned_fd(fd: RawFd) -> anyhow::Result<OwnedFd> {
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    anyhow::ensure!(
        dup >= 0,
        "F_DUPFD_CLOEXEC failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

impl RendererState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            drm_devices: DrmDevices::new()?,
            topologies: HashMap::new(),
            vulkan: None,
            scanout_pool: ScanoutBufferPool::new(),
            render_device: None,
            output_configs: HashMap::new(),
            virtual_outputs: HashMap::new(),
            output_views: HashMap::new(),
            guides: Vec::new(),
            outputs: HashMap::new(),
            next_present_wake_token: 0,
            free_present_wake_tokens: Vec::new(),
            cached_present_targets: None,
            flip_events: Box::new(FlipEventQueue::new()),
            surface_frames: HashMap::new(),
            surface_order: Vec::new(),
            cursor_state: CursorState::Default,
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
            present_halted: None,
            screencast_buffers: HashMap::new(),
            screencast_content_serial: 1,
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
        if self.any_hw_cursor_capable() {
            for output in self.outputs.values_mut() {
                if output.cursor().is_some_and(|c| c.plane_id != 0) {
                    output.dirty.cursor_pos = true;
                }
            }
            return;
        }
        let old = (self.pointer_x, self.pointer_y);
        let damage = match self.cursor_state.draw_ref() {
            CursorDraw::Client(cursor) => cursor_damage_rects(cursor, old, (new_x, new_y)),
            CursorDraw::Default => cursor_damage_rects_default(old, (new_x, new_y)),
            CursorDraw::Hidden => Vec::new(),
        };
        self.pending_damage.extend(damage);
        self.pending_pointer_damage = true;
    }

    fn note_cursor_redraw(&mut self) {
        if self.any_hw_cursor_capable() {
            for output in self.outputs.values_mut() {
                let mark = if let Some(cursor) = output.cursor_mut() {
                    if cursor.plane_id != 0 {
                        // Retry HW after image change.
                        cursor.software = false;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if mark {
                    output.dirty.cursor_image = true;
                    output.dirty.cursor_pos = true;
                }
            }
            self.cursor_buffer_dirty = true;
            return;
        }
        let pointer = (self.pointer_x, self.pointer_y);
        let rect = match self.cursor_state.draw_ref() {
            CursorDraw::Client(cursor) => cursor_damage_rects(cursor, pointer, pointer),
            CursorDraw::Default => cursor_damage_rects_default(pointer, pointer),
            CursorDraw::Hidden => Vec::new(),
        };
        self.pending_damage.extend(rect);
        self.pending_pointer_damage = true;
    }

    fn any_hw_cursor_capable(&self) -> bool {
        self.outputs
            .values()
            .any(|o| o.cursor().is_some_and(|c| c.plane_id != 0))
    }

    pub fn scene_dirty(&self) -> bool {
        self.scene_dirty
    }

    pub fn mark_scene_dirty(&mut self) {
        self.scene_dirty = true;
        self.bump_screencast_content();
    }

    fn mark_dirty_if_active(&mut self) {
        if self.has_presentable_outputs() {
            self.scene_dirty = true;
        }
        // Screencast may capture even when presents are idle (PipeWire DRIVER wake).
        self.bump_screencast_content();
    }

    fn bump_screencast_content(&mut self) {
        self.screencast_content_serial = self.screencast_content_serial.wrapping_add(1).max(1);
    }

    /// Monotonic serial of compositor content relevant to screencast capture.
    pub fn screencast_content_serial(&self) -> u64 {
        self.screencast_content_serial
    }

    /// Content serial last written into screencast slot `(stream_id, index)`, if any.
    pub fn screencast_slot_content_serial(
        &self,
        stream_id: u32,
        index: usize,
    ) -> Option<u64> {
        self.screencast_buffers
            .get(&stream_id)?
            .get(index)?
            .content_serial
    }

    /// Forget cached content on all slots for `stream_id` (geometry / size change).
    pub fn invalidate_screencast_content(&mut self, stream_id: u32) {
        if let Some(slots) = self.screencast_buffers.get_mut(&stream_id) {
            for slot in slots {
                slot.content_serial = None;
            }
        }
    }

    fn has_presentable_outputs(&self) -> bool {
        !self.drm_devices.opened().is_empty() || !self.virtual_outputs.is_empty()
    }

    /// Whether any virtual (non-KMS) outputs are registered.
    pub fn has_virtual_outputs(&self) -> bool {
        !self.virtual_outputs.is_empty()
    }

    /// Register or update a virtual output for headless / non-KMS presentation.
    pub fn add_virtual_output(
        &mut self,
        name: String,
        width: u32,
        height: u32,
        refresh_mhz: i32,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            width > 0 && height > 0,
            "virtual output size must be positive"
        );
        let refresh_mhz = refresh_mhz.max(1);
        info!("Virtual output: {name} {width}x{height} @{refresh_mhz} mHz");
        self.virtual_outputs.insert(
            name.clone(),
            VirtualOutput {
                name,
                width,
                height,
                refresh_mhz,
            },
        );
        self.invalidate_present_targets();
        self.mark_dirty_if_active();
        Ok(())
    }

    /// Remove a virtual output and release its scanout resources.
    pub fn remove_virtual_output(&mut self, name: &str) {
        if self.virtual_outputs.remove(name).is_some() {
            info!("Removed virtual output {name}");
            self.output_views.remove(name);
            self.invalidate_present_targets();
            if let Some(output) = self.outputs.remove(name) {
                self.release_output_state(output);
            }
        }
    }

    /// Replace the view list used when compositing `output`.
    pub fn set_output_views(&mut self, output: &str, views: Vec<View>) {
        if views.is_empty() {
            self.output_views.remove(output);
        } else {
            self.output_views.insert(output.to_owned(), views);
        }
        self.pending_full_redraw = true;
        self.mark_dirty_if_active();
    }

    /// Add or replace a guide by name.
    pub fn add_guide(&mut self, guide: Guide) {
        if let Some(slot) = self.guides.iter_mut().find(|g| g.name == guide.name) {
            *slot = guide;
        } else {
            self.guides.push(guide);
        }
        self.pending_full_redraw = true;
        self.mark_dirty_if_active();
    }

    /// Remove a guide by name. Returns whether it existed.
    pub fn remove_guide(&mut self, name: &str) -> bool {
        let before = self.guides.len();
        self.guides.retain(|g| g.name != name);
        let removed = self.guides.len() != before;
        if removed {
            self.pending_full_redraw = true;
            self.mark_dirty_if_active();
        }
        removed
    }

    /// Remove all guides.
    pub fn clear_guides(&mut self) {
        if !self.guides.is_empty() {
            self.guides.clear();
            self.pending_full_redraw = true;
            self.mark_dirty_if_active();
        }
    }

    /// Drop views for an output (blank presents until new views are set).
    pub fn clear_output_views(&mut self, output: &str) {
        if self.output_views.remove(output).is_some() {
            self.pending_full_redraw = true;
            self.mark_dirty_if_active();
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
        let result = self.drm_devices.dispatch()?;
        if result.changed() {
            self.invalidate_present_targets();
        }
        Ok(result)
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
    pub fn dispatch_page_flips(&mut self, arena: &Arena) -> anyhow::Result<FlipDispatchOutcome> {
        let named = self.dispatch_page_flips_named(arena)?;
        Ok(FlipDispatchOutcome {
            status: named.status,
            completed: named.completed.into_iter().map(|(_, flip)| flip).collect(),
        })
    }

    /// Drain DRM page-flip events and attribute each completion to an output name.
    pub(crate) fn dispatch_page_flips_named<'a>(
        &mut self,
        arena: &'a Arena,
    ) -> anyhow::Result<NamedFlipDispatchOutcome<'a>> {
        let mut fds = allocator_api2::vec::Vec::new_in(arena);
        fds.extend(
            self.drm_devices
                .opened()
                .values()
                .map(|device| device.fd().as_raw_fd()),
        );
        for fd in fds {
            dispatch_drm_events(fd)?;
        }
        let completed = self.flip_events.drain();
        let mut named = allocator_api2::vec::Vec::with_capacity_in(completed.len(), arena);
        for flip in completed {
            let crtc_id = flip.crtc_id;
            if let Some(name) = self.retire_page_flip(crtc_id)? {
                named.push((name, flip));
            }
        }
        Ok(NamedFlipDispatchOutcome {
            status: self.present_status(),
            completed: named,
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
    pub fn sync_surface_scene(&mut self, scene: &[(u32, u32, i32, i32)], arena: &Arena) {
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
        let mut visible = allocator_api2::vec::Vec::new_in(arena);
        visible.extend(new_order.iter().copied());
        let mut removed = allocator_api2::vec::Vec::new_in(arena);
        for key in self.surface_frames.keys().copied() {
            if !visible.contains(&key) {
                removed.push(key);
            }
        }
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

    /// Drop a cached DMA-BUF import when the corresponding `wl_buffer` is destroyed.
    pub fn remove_dmabuf_buffer(&mut self, owner_id: u32, buffer_id: u32) -> anyhow::Result<()> {
        let (Some(vulkan), Some(compositor)) = (self.vulkan.as_ref(), self.gpu.compositor.as_ref())
        else {
            self.gpu
                .surface_textures
                .forget_dmabuf_buffer(owner_id, buffer_id);
            return Ok(());
        };
        self.gpu.surface_textures.remove_dmabuf_buffer(
            vulkan.device(),
            &compositor.descriptor_pool,
            owner_id,
            buffer_id,
        )
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
            .cursor_state
            .as_client()
            .is_some_and(|cursor| cursor.owner_id == owner_id);
        if cursor_removed {
            self.cursor_state = CursorState::Default;
        }
        if self.surface_frames.len() != before || cursor_removed {
            self.pending_full_redraw = true;
            self.pending_damage.clear();
            self.mark_dirty_if_active();
        }
        Ok(())
    }

    pub fn cursor_surface_key(&self) -> Option<(u32, u32)> {
        self.cursor_state.surface_key()
    }

    pub fn set_cursor_frame(&mut self, frame: CursorFrame) -> anyhow::Result<()> {
        frame.validate()?;
        self.cursor_state = CursorState::Client(frame);
        self.cursor_buffer_dirty = true;
        self.note_cursor_redraw();
        if self.any_hw_cursor_capable() {
            self.flush_hw_cursors()?;
            if self
                .outputs
                .values()
                .any(|o| o.cursor().is_some_and(|c| c.software && c.plane_id != 0))
            {
                self.pending_pointer_damage = true;
                self.mark_dirty_if_active();
            }
            return Ok(());
        }
        self.mark_dirty_if_active();
        Ok(())
    }

    /// Restore the compositor default cursor (not an explicit client hide).
    pub fn clear_cursor_frame(&mut self) -> anyhow::Result<()> {
        if matches!(self.cursor_state, CursorState::Default) {
            return Ok(());
        }
        self.note_cursor_redraw();
        self.cursor_state = CursorState::Default;
        self.cursor_buffer_dirty = true;
        self.note_cursor_redraw();
        if self.any_hw_cursor_capable() {
            self.flush_hw_cursors()?;
            return Ok(());
        }
        self.mark_dirty_if_active();
        Ok(())
    }

    /// Hide the pointer image entirely (client called set_cursor with null).
    pub fn hide_cursor(&mut self) -> anyhow::Result<()> {
        if matches!(self.cursor_state, CursorState::Hidden) {
            return Ok(());
        }
        self.note_cursor_redraw();
        self.cursor_state = CursorState::Hidden;
        if self.any_hw_cursor_capable() {
            self.flush_hw_cursors()?;
            return Ok(());
        }
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
        if self.any_hw_cursor_capable() {
            self.flush_hw_cursors()?;
            return Ok(());
        }
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn update_cursor_hotspot(&mut self, hotspot_x: i32, hotspot_y: i32) -> anyhow::Result<()> {
        let Some(cursor) = self.cursor_state.as_client() else {
            return Ok(());
        };
        if cursor.hotspot_x == hotspot_x && cursor.hotspot_y == hotspot_y {
            return Ok(());
        }
        let hw = self.any_hw_cursor_capable();
        if !hw {
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
        }
        if let Some(cursor) = self.cursor_state.as_client_mut() {
            cursor.hotspot_x = hotspot_x;
            cursor.hotspot_y = hotspot_y;
        }
        if hw {
            for output in self.outputs.values_mut() {
                if output.cursor().is_some_and(|c| c.plane_id != 0) {
                    output.dirty.cursor_pos = true;
                }
            }
            self.flush_hw_cursors()?;
            return Ok(());
        }
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn flip_idle(&self) -> bool {
        self.present_status().idle
    }

    /// Geometry of the first enabled present target (physical or virtual).
    pub fn primary_output_geometry(&self) -> Option<(String, i32, i32, i32)> {
        let target = self.collect_present_targets().into_iter().next()?;
        Some((
            target.name,
            target.width as i32,
            target.height as i32,
            target.refresh_mhz.max(60_000),
        ))
    }

    /// Capture a rectangular region of the displayed scanouts as RGBA8 pixels.
    ///
    /// `x`/`y`/`width`/`height` are in global compositor (logical) space. `outputs`
    /// supplies views for mapping that space onto scanout buffers.
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
            .flat_map(|output| CaptureRegion::from_output_views(output, x, y, width, height))
            .collect();

        for region in regions {
            if !self.outputs.contains_key(&region.name) {
                continue;
            }

            if let Some(pending) = self
                .outputs
                .get_mut(&region.name)
                .and_then(|o| o.primary_mut())
                .and_then(|p| p.current.as_mut())
                .and_then(|b| b.gpu_pending.take())
            {
                let vulkan = self
                    .vulkan
                    .as_mut()
                    .context("Vulkan is not initialized for screenshot capture")?;
                pending.wait(vulkan)?;
            }

            let vulkan = self
                .vulkan
                .as_ref()
                .context("Vulkan is not initialized for screenshot capture")?;
            let (format, image_ptr) = {
                let current = self
                    .outputs
                    .get(&region.name)
                    .and_then(|o| o.primary())
                    .and_then(|p| p.current.as_ref())
                    .context("primary buffer missing during capture")?;
                (
                    current.dma_image.format(),
                    &current.dma_image as *const DmaBufImage,
                )
            };
            let bgra = download_bgra_region(
                vulkan,
                unsafe { &*image_ptr },
                region.fb_x,
                region.fb_y,
                region.fb_w,
                region.fb_h,
            )?;

            blit_bgra_to_rgba(
                &bgra,
                region.fb_w,
                region.fb_h,
                format,
                &mut rgba,
                dest_w,
                dest_h,
                region.dest_x,
                region.dest_y,
                region.logical_w,
                region.logical_h,
            )?;
            covered = true;
        }

        anyhow::ensure!(
            covered,
            "screenshot region does not intersect any presented scanout"
        );

        Ok(CapturedImage {
            width: dest_w,
            height: dest_h,
            rgba,
        })
    }

    /// Allocate exportable buffers for a PipeWire DMA-BUF stream.
    ///
    /// Prefers a single-plane tiled DRM modifier when the GPU supports render+blit
    /// usage; falls back to LINEAR. Returns one [`ScreencastDmaExport`] per buffer
    /// (duplicated fds for the PW thread). Images stay owned by [`RendererState`]
    /// under `stream_id` until [`Self::free_screencast_buffers`].
    pub fn alloc_screencast_buffers(
        &mut self,
        stream_id: u32,
        width: u32,
        height: u32,
        count: usize,
    ) -> anyhow::Result<Vec<ScreencastDmaExport>> {
        anyhow::ensure!(
            width > 0 && height > 0,
            "screencast buffer size must be positive"
        );
        anyhow::ensure!(count > 0, "screencast buffer count must be positive");
        self.free_screencast_buffers(stream_id);

        if let Some(path) = self.resolved_render_device_path() {
            self.ensure_vulkan(Some(&path))?;
        } else {
            self.ensure_vulkan(None)?;
        }

        let format = vk::Format::B8G8R8A8_UNORM;
        let mut slots = Vec::with_capacity(count);
        let mut exports = Vec::with_capacity(count);
        {
            let vulkan = self
                .vulkan
                .as_mut()
                .context("Vulkan is not initialized for screencast buffers")?;
            vulkan.ensure_scanout_render_pass()?;
            let render_pass = vulkan.scanout_render_pass()?;

            // First allocation picks tiled-or-LINEAR; remaining slots match that modifier.
            let mut chosen_modifier: Option<u64> = None;
            for index in 0..count {
                let image = if let Some(modifier) = chosen_modifier {
                    DmaBufImage::allocate_with_modifiers(
                        vulkan.device(),
                        vulkan.physical_device(),
                        width,
                        height,
                        format,
                        &[modifier],
                    )
                } else {
                    DmaBufImage::allocate_for_screencast(
                        vulkan.device(),
                        vulkan.physical_device(),
                        vulkan.instance(),
                        width,
                        height,
                        format,
                    )
                }
                .with_context(|| format!("allocate screencast buffer {index}"))?;
                if chosen_modifier.is_none() {
                    chosen_modifier = Some(image.modifier());
                }
                let framebuffer = Framebuffer::from_view(
                    vulkan.device(),
                    render_pass,
                    image.view(),
                    image.extent(),
                )
                .with_context(|| format!("create screencast framebuffer {index}"))?;
                let export_fd = image
                    .export_dma_buf()
                    .context("export screencast DMA-BUF")?;
                let pw_fd = dup_owned_fd(export_fd.as_raw_fd())
                    .context("dup screencast DMA-BUF for PipeWire")?;
                let info = ScreencastDmaExport {
                    index,
                    fd: pw_fd,
                    width,
                    height,
                    stride: image.stride(),
                    offset: image.offset(),
                    size: image.size(),
                    modifier: image.modifier(),
                };
                exports.push(info);
                slots.push(ScreencastDmaSlot {
                    image,
                    framebuffer,
                    _export_fd: export_fd,
                    in_use: false,
                    fresh: true,
                    gpu_pending: None,
                    content_serial: None,
                });
            }
        }
        if let Some(first) = exports.first() {
            info!(
                "Allocated {count} screencast DMA-BUF(s) {}x{} modifier={:#x} stride={}",
                width, height, first.modifier, first.stride
            );
        }
        self.screencast_buffers.insert(stream_id, slots);
        Ok(exports)
    }

    /// Drop capture buffers for `stream_id`, waiting for any in-flight GPU fills.
    pub fn free_screencast_buffers(&mut self, stream_id: u32) {
        if let Err(err) = self.wait_screencast_stream_gpu(stream_id) {
            warn!("Error waiting for screencast GPU work before free: {err:#}");
        }
        self.screencast_buffers.remove(&stream_id);
    }

    /// GPU-blit the compositor region into screencast buffer `index` (does not wait).
    ///
    /// Completion is tracked on the slot; call [`Self::poll_screencast_gpu`] or
    /// [`Self::wait_screencast_buffer_gpu`] before exporting/reading the buffer.
    ///
    /// When `embed_cursor` is true and hardware cursor planes omit the cursor from
    /// scanout, the software cursor is composited into the capture buffer.
    ///
    /// `width`/`height` are the capture rectangle in compositor space.
    /// `dest_width`/`dest_height` are the blit destination size (must fit in the buffer).
    pub fn blit_region_to_screencast_buffer(
        &mut self,
        stream_id: u32,
        index: usize,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        dest_width: u32,
        dest_height: u32,
        outputs: &[Output],
        embed_cursor: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(width > 0 && height > 0, "capture region must be positive");
        anyhow::ensure!(
            dest_width > 0 && dest_height > 0,
            "destination size must be positive"
        );
        let regions: Vec<CaptureRegion> = outputs
            .iter()
            .flat_map(|output| CaptureRegion::from_output_views(output, x, y, width, height))
            .collect();
        anyhow::ensure!(
            !regions.is_empty(),
            "screencast region does not intersect any presented scanout"
        );

        let (buf_w, buf_h, fresh) = {
            let slots = self
                .screencast_buffers
                .get(&stream_id)
                .context("screencast buffers missing")?;
            let slot = slots
                .get(index)
                .context("screencast buffer index out of range")?;
            anyhow::ensure!(
                slot.gpu_pending.is_none(),
                "screencast buffer {index} still has pending GPU work"
            );
            let extent = slot.image.extent();
            (extent.width, extent.height, slot.fresh)
        };
        anyhow::ensure!(
            dest_width <= buf_w && dest_height <= buf_h,
            "destination {dest_width}x{dest_height} exceeds buffer {buf_w}x{buf_h}"
        );
        let out_w = dest_width;
        let out_h = dest_height;

        // Wait for source scanout GPU work first.
        let mut source_names = HashSet::new();
        for region in &regions {
            source_names.insert(region.name.clone());
        }
        for name in source_names {
            if let Some(pending) = self
                .outputs
                .get_mut(&name)
                .and_then(|o| o.primary_mut())
                .and_then(|p| p.current.as_mut())
                .and_then(|b| b.gpu_pending.take())
            {
                let vulkan = self
                    .vulkan
                    .as_mut()
                    .context("Vulkan missing while waiting for scanout")?;
                pending.wait(vulkan)?;
            }
        }

        let mut batch = GpuWorkBatch::new();
        // Clear so multi-region / partial coverage never leaves framebuffer garbage.
        {
            let dst_ptr = {
                let slots = self
                    .screencast_buffers
                    .get(&stream_id)
                    .context("screencast buffers missing")?;
                let slot = slots
                    .get(index)
                    .context("screencast buffer index out of range")?;
                &slot.image as *const DmaBufImage
            };
            let vulkan = self
                .vulkan
                .as_mut()
                .context("Vulkan missing for screencast clear")?;
            unsafe {
                clear_dma_image_color(vulkan, &mut batch, &*dst_ptr, fresh, [0.0, 0.0, 0.0, 1.0])?;
            }
        }

        let mut first = false; // dest already defined after clear
        let src_w = width as u32;
        let src_h = height as u32;
        for region in &regions {
            let src_ptr = {
                let current = self
                    .outputs
                    .get(&region.name)
                    .and_then(|o| o.primary())
                    .and_then(|p| p.current.as_ref())
                    .with_context(|| format!("missing primary buffer {}", region.name))?;
                &current.dma_image as *const DmaBufImage
            };
            let dst_ptr = {
                let slots = self
                    .screencast_buffers
                    .get(&stream_id)
                    .context("screencast buffers missing")?;
                let slot = slots
                    .get(index)
                    .context("screencast buffer index out of range")?;
                &slot.image as *const DmaBufImage
            };
            let dest_x =
                (u64::from(region.dest_x) * u64::from(out_w) / u64::from(src_w.max(1))) as u32;
            let dest_y =
                (u64::from(region.dest_y) * u64::from(out_h) / u64::from(src_h.max(1))) as u32;
            let dest_w = (u64::from(region.logical_w) * u64::from(out_w) / u64::from(src_w.max(1)))
                .max(1) as u32;
            let dest_h = (u64::from(region.logical_h) * u64::from(out_h) / u64::from(src_h.max(1)))
                .max(1) as u32;
            let vulkan = self
                .vulkan
                .as_mut()
                .context("Vulkan missing for screencast blit")?;
            // Safety: both images are owned by self and live for this call.
            unsafe {
                blit_image_region(
                    vulkan,
                    &mut batch,
                    &*src_ptr,
                    &*dst_ptr,
                    region.fb_x,
                    region.fb_y,
                    region.fb_w,
                    region.fb_h,
                    dest_x,
                    dest_y,
                    dest_w,
                    dest_h,
                    first,
                )?;
            }
            first = false;
        }

        // HW cursor is not in the scanout FB — overlay a software cursor when requested.
        let needs_cursor_overlay = embed_cursor
            && regions.iter().any(|region| {
                self.outputs
                    .get(&region.name)
                    .is_some_and(|o| o.hw_cursor_active())
            })
            && !matches!(self.cursor_state, CursorState::Hidden);

        if needs_cursor_overlay {
            let pointer_x = self.pointer_x;
            let pointer_y = self.pointer_y;
            let dest_px = ((i64::from(pointer_x) - i64::from(x)) * i64::from(out_w)
                / i64::from(width.max(1))) as i32;
            let dest_py = ((i64::from(pointer_y) - i64::from(y)) * i64::from(out_h)
                / i64::from(height.max(1))) as i32;

            let vulkan = self
                .vulkan
                .as_mut()
                .context("Vulkan missing for screencast cursor overlay")?;
            self.gpu.ensure_compositor(vulkan)?;
            vulkan.ensure_scanout_render_pass_load()?;
            let compositor = self
                .gpu
                .compositor
                .take()
                .context("GPU compositor missing")?;
            let result = (|| -> anyhow::Result<()> {
                self.gpu.surface_textures.sync_scene(
                    vulkan,
                    &compositor,
                    &mut batch,
                    &[],
                    self.cursor_state.draw_ref(),
                    &CompositeMode::Full,
                    &HashSet::new(),
                    &HashMap::new(),
                    &[],
                    false,
                )?;
                batch.ensure_recording(vulkan)?;
                let render_pass = vulkan.scanout_render_pass_load()?;
                let (image_ptr, fb_ptr) = {
                    let slots = self
                        .screencast_buffers
                        .get(&stream_id)
                        .context("screencast buffers missing")?;
                    let slot = slots
                        .get(index)
                        .context("screencast buffer index out of range")?;
                    (
                        &slot.image as *const DmaBufImage,
                        &slot.framebuffer as *const Framebuffer,
                    )
                };
                // Safety: owned by self for this call.
                unsafe {
                    overlay_cursor_on_image(
                        vulkan,
                        &mut batch,
                        &compositor,
                        &self.gpu.surface_textures,
                        render_pass,
                        &*image_ptr,
                        &*fb_ptr,
                        out_w,
                        out_h,
                        self.cursor_state.draw_ref(),
                        dest_px,
                        dest_py,
                    )?;
                }
                Ok(())
            })();
            self.gpu.compositor = Some(compositor);
            if let Err(error) = result {
                if let Some(vulkan) = self.vulkan.as_mut() {
                    batch.abandon(vulkan);
                }
                return Err(error);
            }
        }

        let vulkan = self
            .vulkan
            .as_mut()
            .context("Vulkan missing for screencast submit")?;
        let pending = batch.submit(vulkan)?;
        let serial = self.screencast_content_serial;

        if let Some(slot) = self
            .screencast_buffers
            .get_mut(&stream_id)
            .and_then(|slots| slots.get_mut(index))
        {
            slot.gpu_pending = Some(pending);
            slot.in_use = true;
            slot.fresh = false;
            slot.content_serial = Some(serial);
        }
        Ok(())
    }

    /// Composite only the given surface layers into screencast buffer `index` (does not wait).
    ///
    /// Completion is tracked on the slot; call [`Self::poll_screencast_gpu`] or
    /// [`Self::wait_screencast_buffer_gpu`] before exporting/reading the buffer.
    ///
    /// `origin`/`width`/`height` are the window AABB in compositor space. Layers are
    /// drawn with a view that maps that AABB onto `dest_width`×`dest_height`.
    /// Clear is transparent. When `embed_cursor` is true the pointer is drawn if it
    /// intersects the window.
    pub fn composite_window_to_screencast_buffer(
        &mut self,
        stream_id: u32,
        index: usize,
        layers: &[(u32, u32)],
        origin_x: i32,
        origin_y: i32,
        width: i32,
        height: i32,
        dest_width: u32,
        dest_height: u32,
        embed_cursor: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            width > 0 && height > 0,
            "window capture size must be positive"
        );
        anyhow::ensure!(
            dest_width > 0 && dest_height > 0,
            "destination size must be positive"
        );

        let (buf_w, buf_h, fresh) = {
            let slots = self
                .screencast_buffers
                .get(&stream_id)
                .context("screencast buffers missing")?;
            let slot = slots
                .get(index)
                .context("screencast buffer index out of range")?;
            anyhow::ensure!(
                slot.gpu_pending.is_none(),
                "screencast buffer {index} still has pending GPU work"
            );
            let extent = slot.image.extent();
            (extent.width, extent.height, slot.fresh)
        };
        anyhow::ensure!(
            dest_width <= buf_w && dest_height <= buf_h,
            "destination {dest_width}x{dest_height} exceeds buffer {buf_w}x{buf_h}"
        );

        let frame_layers: Vec<&SurfaceFrame> = layers
            .iter()
            .filter_map(|key| self.surface_frames.get(key))
            .collect();

        let view = View {
            name: String::from("screencast-window"),
            source: (origin_x, origin_y, width, height),
            dest: (0, 0, dest_width as i32, dest_height as i32),
        };

        let draw_cursor = embed_cursor && !matches!(self.cursor_state, CursorState::Hidden);
        let (dest_px, dest_py) = if draw_cursor {
            let (x, y) = lumalla_shared::map_source_to_dest(
                std::slice::from_ref(&view),
                self.pointer_x as f64,
                self.pointer_y as f64,
            );
            (x.round() as i32, y.round() as i32)
        } else {
            (0, 0)
        };

        let mut batch = GpuWorkBatch::new();
        {
            let vulkan = self
                .vulkan
                .as_mut()
                .context("Vulkan missing for window screencast")?;
            self.gpu.ensure_compositor(vulkan)?;
            let compositor = self
                .gpu
                .compositor
                .take()
                .context("GPU compositor missing")?;

            let result = (|| -> anyhow::Result<()> {
                let cursor = if draw_cursor {
                    self.cursor_state.draw_ref()
                } else {
                    CursorDraw::Hidden
                };
                self.gpu.surface_textures.sync_scene(
                    vulkan,
                    &compositor,
                    &mut batch,
                    &frame_layers,
                    cursor,
                    &CompositeMode::Full,
                    &HashSet::new(),
                    &HashMap::new(),
                    &[],
                    false,
                )?;

                vulkan.ensure_scanout_render_pass()?;
                batch.ensure_recording(vulkan)?;
                let render_pass = vulkan.scanout_render_pass()?;
                let image_old_layout = if fresh {
                    vk::ImageLayout::UNDEFINED
                } else {
                    vk::ImageLayout::GENERAL
                };

                let (image_ptr, fb_ptr) = {
                    let slots = self
                        .screencast_buffers
                        .get(&stream_id)
                        .context("screencast buffers missing")?;
                    let slot = slots
                        .get(index)
                        .context("screencast buffer index out of range")?;
                    (
                        &slot.image as *const DmaBufImage,
                        &slot.framebuffer as *const Framebuffer,
                    )
                };

                let cursor = if draw_cursor {
                    self.cursor_state.draw_ref()
                } else {
                    CursorDraw::Hidden
                };
                // Safety: both are owned by self for this call.
                unsafe {
                    composite_layers_to_image(
                        vulkan,
                        &mut batch,
                        &compositor,
                        &self.gpu.surface_textures,
                        render_pass,
                        &*image_ptr,
                        &*fb_ptr,
                        image_old_layout,
                        dest_width,
                        dest_height,
                        [0.0, 0.0, 0.0, 0.0],
                        &view,
                        &frame_layers,
                        cursor,
                        dest_px,
                        dest_py,
                    )?;
                }
                Ok(())
            })();
            self.gpu.compositor = Some(compositor);
            if let Err(error) = result {
                if let Some(vulkan) = self.vulkan.as_mut() {
                    batch.abandon(vulkan);
                }
                return Err(error);
            }
        }

        let vulkan = self
            .vulkan
            .as_mut()
            .context("Vulkan missing for window screencast submit")?;
        let pending = batch.submit(vulkan)?;
        let serial = self.screencast_content_serial;

        if let Some(slot) = self
            .screencast_buffers
            .get_mut(&stream_id)
            .and_then(|slots| slots.get_mut(index))
        {
            slot.gpu_pending = Some(pending);
            slot.in_use = true;
            slot.fresh = false;
            slot.content_serial = Some(serial);
        }
        Ok(())
    }

    /// Capture a window tree into a downscaled RGBA frame via GPU composite + readback.
    pub fn capture_window_for_screencast(
        &mut self,
        stream_id: u32,
        layers: &[(u32, u32)],
        origin_x: i32,
        origin_y: i32,
        width: i32,
        height: i32,
        dest_width: u32,
        dest_height: u32,
        embed_cursor: bool,
    ) -> anyhow::Result<CapturedImage> {
        let index = self
            .next_free_screencast_buffer(stream_id)
            .context("no free screencast buffer for MemFd window capture")?;
        self.composite_window_to_screencast_buffer(
            stream_id,
            index,
            layers,
            origin_x,
            origin_y,
            width,
            height,
            dest_width,
            dest_height,
            embed_cursor,
        )?;
        self.wait_screencast_buffer_gpu(stream_id, index)?;

        let format = {
            let slots = self
                .screencast_buffers
                .get(&stream_id)
                .context("screencast buffers missing after composite")?;
            let slot = slots
                .get(index)
                .context("screencast buffer index out of range")?;
            slot.image.format()
        };

        let vulkan = self
            .vulkan
            .as_ref()
            .context("Vulkan missing for window MemFd readback")?;
        let image_ptr = {
            let slots = self
                .screencast_buffers
                .get(&stream_id)
                .context("screencast buffers missing for readback")?;
            let slot = slots
                .get(index)
                .context("screencast buffer index out of range")?;
            &slot.image as *const DmaBufImage
        };
        let bgra =
            unsafe { download_bgra_region(vulkan, &*image_ptr, 0, 0, dest_width, dest_height)? };

        self.release_screencast_buffer(stream_id, index);

        let mut rgba = vec![0u8; (dest_width as usize) * (dest_height as usize) * 4];
        let _ = blit_bgra_to_rgba(
            &bgra,
            dest_width,
            dest_height,
            format,
            &mut rgba,
            dest_width,
            dest_height,
            0,
            0,
            dest_width,
            dest_height,
        );
        Ok(CapturedImage {
            width: dest_width,
            height: dest_height,
            rgba,
        })
    }

    /// Capture a region into a downscaled RGBA frame via GPU blit + small readback.
    ///
    /// Uses a free screencast DMA buffer for `stream_id` as staging. `dest_width`/
    /// `dest_height` should be the MemFd size (≤ buffer) so CPU readback stays cheap.
    pub fn capture_region_for_screencast(
        &mut self,
        stream_id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        dest_width: u32,
        dest_height: u32,
        outputs: &[Output],
        embed_cursor: bool,
    ) -> anyhow::Result<CapturedImage> {
        let index = self
            .next_free_screencast_buffer(stream_id)
            .context("no free screencast buffer for MemFd capture")?;
        self.blit_region_to_screencast_buffer(
            stream_id,
            index,
            x,
            y,
            width,
            height,
            dest_width,
            dest_height,
            outputs,
            embed_cursor,
        )?;
        self.wait_screencast_buffer_gpu(stream_id, index)?;

        let format = {
            let slots = self
                .screencast_buffers
                .get(&stream_id)
                .context("screencast buffers missing after blit")?;
            let slot = slots
                .get(index)
                .context("screencast buffer index out of range")?;
            slot.image.format()
        };

        let vulkan = self
            .vulkan
            .as_ref()
            .context("Vulkan missing for screencast MemFd readback")?;
        let image_ptr = {
            let slots = self
                .screencast_buffers
                .get(&stream_id)
                .context("screencast buffers missing for readback")?;
            let slot = slots
                .get(index)
                .context("screencast buffer index out of range")?;
            &slot.image as *const DmaBufImage
        };
        // Safety: image is owned by self and lives for this call.
        let bgra =
            unsafe { download_bgra_region(vulkan, &*image_ptr, 0, 0, dest_width, dest_height)? };

        self.release_screencast_buffer(stream_id, index);

        let mut rgba = vec![0u8; (dest_width as usize) * (dest_height as usize) * 4];
        blit_bgra_to_rgba(
            &bgra,
            dest_width,
            dest_height,
            format,
            &mut rgba,
            dest_width,
            dest_height,
            0,
            0,
            dest_width,
            dest_height,
        )?;

        Ok(CapturedImage {
            width: dest_width,
            height: dest_height,
            rgba,
        })
    }

    /// Mark a screencast buffer free for reuse after PipeWire has finished with it.
    pub fn release_screencast_buffer(&mut self, stream_id: u32, index: usize) {
        if let Some(slot) = self
            .screencast_buffers
            .get_mut(&stream_id)
            .and_then(|slots| slots.get_mut(index))
        {
            slot.in_use = false;
        }
    }

    /// Block until GPU fill for one screencast slot completes (MemFd readback path).
    pub fn wait_screencast_buffer_gpu(
        &mut self,
        stream_id: u32,
        index: usize,
    ) -> anyhow::Result<()> {
        let pending = self
            .screencast_buffers
            .get_mut(&stream_id)
            .and_then(|slots| slots.get_mut(index))
            .and_then(|slot| slot.gpu_pending.take());
        let Some(pending) = pending else {
            return Ok(());
        };
        let vulkan = self
            .vulkan
            .as_mut()
            .context("Vulkan missing while waiting for screencast GPU work")?;
        pending.wait(vulkan)
    }

    /// Block until all in-flight screencast GPU fills for `stream_id` complete.
    pub fn wait_screencast_stream_gpu(&mut self, stream_id: u32) -> anyhow::Result<()> {
        let indices: Vec<usize> = self
            .screencast_buffers
            .get(&stream_id)
            .map(|slots| {
                slots
                    .iter()
                    .enumerate()
                    .filter_map(|(i, slot)| slot.gpu_pending.is_some().then_some(i))
                    .collect()
            })
            .unwrap_or_default();
        for index in indices {
            self.wait_screencast_buffer_gpu(stream_id, index)?;
        }
        Ok(())
    }

    /// Whether any screencast slot still has in-flight GPU work.
    pub fn has_pending_screencast_gpu(&self) -> bool {
        self.screencast_buffers
            .values()
            .any(|slots| slots.iter().any(|slot| slot.gpu_pending.is_some()))
    }

    /// Non-blocking: recycle finished screencast GPU fills.
    ///
    /// Returns `(stream_id, buffer_index)` pairs that just became ready to queue
    /// to PipeWire (or read back).
    pub fn poll_screencast_gpu(&mut self) -> anyhow::Result<Vec<(u32, usize)>> {
        let mut pending_keys: Vec<(u32, usize)> = Vec::new();
        for (stream_id, slots) in &self.screencast_buffers {
            for (index, slot) in slots.iter().enumerate() {
                if slot.gpu_pending.is_some() {
                    pending_keys.push((*stream_id, index));
                }
            }
        }

        let mut ready = Vec::new();
        for (stream_id, index) in pending_keys {
            let pending = self
                .screencast_buffers
                .get_mut(&stream_id)
                .and_then(|slots| slots.get_mut(index))
                .and_then(|slot| slot.gpu_pending.take());
            let Some(pending) = pending else {
                continue;
            };
            let vulkan = self
                .vulkan
                .as_mut()
                .context("Vulkan missing while polling screencast GPU work")?;
            match pending.try_complete(vulkan)? {
                None => ready.push((stream_id, index)),
                Some(still) => {
                    if let Some(slot) = self
                        .screencast_buffers
                        .get_mut(&stream_id)
                        .and_then(|slots| slots.get_mut(index))
                    {
                        slot.gpu_pending = Some(still);
                    }
                }
            }
        }
        Ok(ready)
    }

    /// Next free screencast buffer index, if any.
    pub fn next_free_screencast_buffer(&self, stream_id: u32) -> Option<usize> {
        self.screencast_buffers
            .get(&stream_id)?
            .iter()
            .position(|slot| !slot.in_use && slot.gpu_pending.is_none())
    }

    /// DRM format/modifier pairs clients may use with linux-dmabuf.
    ///
    /// Ensures Vulkan is initialized against the preferred render device so the
    /// advertised set matches what import will accept. Falls back to Vulkan
    /// without a preferred path when no seat-opened DRM device exists yet
    /// (headless / before virtual outputs or DRM activate).
    pub fn supported_dmabuf_formats(&mut self) -> anyhow::Result<Vec<(u32, u64)>> {
        if let Some(path) = self.resolved_render_device_path() {
            self.ensure_vulkan(Some(&path))?;
        } else {
            self.ensure_vulkan(None)?;
        }
        let vulkan = self
            .vulkan
            .as_ref()
            .context("VulkanContext missing after ensure")?;
        Ok(vulkan.supported_dmabuf_formats())
    }

    /// DRM device path used for linux-dmabuf feedback `main_device` / tranche target.
    ///
    /// Prefers a seat-opened primary node. When none is open (headless / before DRM
    /// activate), falls back to the DRM path of the Vulkan GPU so clients can still
    /// resolve `main_device` via `drmGetDeviceFromDevId`.
    pub fn dmabuf_feedback_device_path(&mut self) -> Option<PathBuf> {
        if let Some(path) = self.resolved_render_device_path() {
            return Some(path);
        }
        if self.vulkan.is_none()
            && let Err(err) = self.ensure_vulkan(None)
        {
            warn!("Unable to init Vulkan for dmabuf feedback device: {err:#}");
            return None;
        }
        self.vulkan
            .as_ref()
            .and_then(|v| v.drm_device_path().cloned())
    }

    /// Whether `name` is a virtual (non-KMS) present target.
    pub fn output_is_virtual(&self, name: &str) -> bool {
        self.virtual_outputs.contains_key(name)
            || self
                .outputs
                .get(name)
                .is_some_and(|output| output.physical.is_none())
    }

    /// Nominal refresh period for an output present control, in nanoseconds.
    pub fn output_refresh_ns(&self, name: &str) -> Option<u32> {
        self.outputs.get(name).map(|output| {
            output
                .present
                .scheduler
                .frame_period()
                .as_nanos()
                .min(u128::from(u32::MAX)) as u32
        })
    }

    /// Open missing DRM devices via the seat (fresh open after VT resume).
    ///
    /// No-op when the seat backend cannot open devices (headless / no libseat).
    pub fn activate_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        if !seat.can_open_devices() {
            return Ok(());
        }
        self.drm_devices.activate(seat)?;
        self.refresh_topologies()?;
        self.invalidate_present_targets();
        if self.present_halted.take().is_some() {
            info!("Resuming presents after DRM activate");
        }
        Ok(())
    }

    /// Close seat-opened DRM devices after session disable was acknowledged.
    pub fn deactivate_drm(&mut self, seat: &SeatState) {
        self.drain_scanouts();
        let _ = self.flip_events.drain();
        self.invalidate_present_targets();
        if seat.can_open_devices() {
            self.drm_devices.deactivate(seat);
        }
    }

    /// Whether presents are halted after an unrecoverable GPU/DRM failure.
    pub fn presents_halted(&self) -> bool {
        self.present_halted.is_some()
    }

    fn halt_presents(&mut self, err: &anyhow::Error) {
        if self.present_halted.is_some() {
            return;
        }
        let reason = format!("{err:#}");
        error!("Halting presents after unrecoverable failure: {reason}");
        self.present_halted = Some(reason);
    }

    /// Close removed / open newly discovered DRM devices while the seat is active.
    pub fn reconcile_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        if !seat.can_open_devices() {
            return Ok(());
        }
        self.drain_scanouts();
        let _ = self.flip_events.drain();
        self.invalidate_present_targets();
        self.drm_devices.reconcile(seat)?;
        self.refresh_topologies()
    }

    fn refresh_topologies(&mut self) -> anyhow::Result<()> {
        self.topologies.clear();
        for (path, device) in self.drm_devices.opened() {
            match probe_device_topology(device.fd().as_raw_fd(), path.clone()) {
                Ok(topo) => {
                    info!(
                        "DRM topology on {}: {} CRTCs, {} planes ({} overlays free)",
                        path.display(),
                        topo.crtcs.len(),
                        topo.planes.len(),
                        topo.free_overlays.len()
                    );
                    self.topologies.insert(path.clone(), topo);
                }
                Err(err) => warn!(
                    "Failed to probe DRM topology on {}: {err:#}",
                    path.display()
                ),
            }
        }
        Ok(())
    }

    fn drain_scanouts(&mut self) {
        let outputs: Vec<OutputState> = self.outputs.drain().map(|(_, s)| s).collect();
        for output in outputs {
            self.disable_and_release_output_state(output);
        }
        self.scanout_pool.clear();
        self.topologies.clear();
        self.invalidate_surface_textures();
    }

    /// Free plane buffers without touching KMS state (e.g. remode of the same CRTC).
    fn release_output_state(&mut self, output: OutputState) {
        if let Some(planes) = output.planes {
            for buffer in planes.drain_buffers() {
                self.release_scanout_buffer(buffer);
            }
        }
    }

    /// Blank/power-down a physical output, then free its plane buffers.
    fn disable_and_release_output_state(&mut self, output: OutputState) {
        if let Some(physical) = output.physical.as_ref() {
            self.disable_hw_cursor_plane(&output);
            self.disable_physical_output(physical);
        }
        self.release_output_state(output);
    }

    fn disable_hw_cursor_plane(&self, output: &OutputState) {
        let Some(physical) = output.physical.as_ref() else {
            return;
        };
        let Some(cursor) = output.cursor() else {
            return;
        };
        if cursor.plane_id == 0 {
            return;
        }
        let Some(props) = cursor.props.as_ref() else {
            return;
        };
        let Some(device) = self.drm_devices.opened().get(&physical.drm_path) else {
            return;
        };
        if let Err(err) = atomic_set_cursor_plane(
            device.fd(),
            cursor.plane_id,
            props,
            0,
            0,
            0,
            0,
            0,
            0,
            false,
        ) {
            warn!(
                "Failed to disable cursor plane {} on {}: {err:#}",
                cursor.plane_id,
                physical.output.connector_name
            );
        }
    }

    fn disable_physical_output(&self, physical: &PhysicalOutput) {
        let Some(device) = self.drm_devices.opened().get(&physical.drm_path) else {
            warn!(
                "Cannot disable {}: DRM device {} is not open",
                physical.output.connector_name,
                physical.drm_path.display()
            );
            return;
        };
        match atomic_disable_output(device.fd(), &physical.output) {
            Ok(()) => info!(
                "Disabled output {} (CRTC {})",
                physical.output.connector_name, physical.output.crtc_id
            ),
            Err(err) => warn!(
                "Failed to disable output {}: {err:#}",
                physical.output.connector_name
            ),
        }
    }

    /// Wait for any in-flight flip, then disable CRTC and drop the output planes.
    fn disable_and_release_named_scanout(&mut self, name: &str) {
        if self
            .outputs
            .get(name)
            .is_some_and(|output| output.primary_flip_busy())
        {
            if let Err(err) = self.wait_for_connector_flip(name) {
                warn!("Waiting for flip before disabling {name}: {err:#}");
            }
        }
        if let Some(output) = self.outputs.remove(name) {
            self.disable_and_release_output_state(output);
        }
    }

    /// Immediately modeset-off connectors that config marks disabled.
    ///
    /// Covers both scanouts we already own and connected connectors that were
    /// never presented (e.g. left active by firmware / a previous compositor).
    fn apply_output_config_disables(&mut self) {
        let scanout_disable: Vec<String> = self
            .outputs
            .keys()
            .filter(|name| {
                self.output_configs
                    .get(*name)
                    .is_some_and(|config| !config.enabled)
            })
            .cloned()
            .collect();
        for name in scanout_disable {
            self.disable_and_release_named_scanout(&name);
        }

        let mut unresolved: Vec<(PathBuf, ConnectedOutput)> = Vec::new();
        for (path, device) in self.drm_devices.opened() {
            let mut used_crtcs = HashSet::new();
            for scanout in self.outputs.values() {
                if let Some(physical) = scanout.physical.as_ref() {
                    if &physical.drm_path == path {
                        used_crtcs.insert(physical.output.crtc_id);
                    }
                }
            }

            for connector in device.connectors() {
                let disabled = self
                    .output_configs
                    .get(&connector.name)
                    .is_some_and(|config| !config.enabled);
                if !disabled || !connector.connected || self.outputs.contains_key(&connector.name)
                {
                    continue;
                }
                match resolve_connected_output(
                    device.fd().as_raw_fd(),
                    connector.connector_id,
                    None,
                    &mut used_crtcs,
                ) {
                    Ok(Some(output)) => unresolved.push((path.clone(), output)),
                    Ok(None) => {}
                    Err(err) => warn!("Cannot resolve {} for disable: {err:#}", connector.name),
                }
            }
        }

        for (path, output) in unresolved {
            let Some(device) = self.drm_devices.opened().get(&path) else {
                continue;
            };
            match atomic_disable_output(device.fd(), &output) {
                Ok(()) => info!(
                    "Disabled output {} (CRTC {})",
                    output.connector_name, output.crtc_id
                ),
                Err(err) => warn!(
                    "Failed to disable output {}: {err:#}",
                    output.connector_name
                ),
            }
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
        self.invalidate_present_targets();
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
        self.invalidate_present_targets();
        self.apply_output_config_disables();
        self.mark_dirty_if_active();
        Ok(())
    }

    /// Run a present pass when `force` is set or the scene is dirty.
    ///
    /// Presents every presentable output (legacy bulk path). Prefer
    /// [`Self::present_named`] / [`Self::arm_presents`] for per-output pacing.
    pub fn present(&mut self, color: [f32; 4], force: bool) -> anyhow::Result<PresentOutcome> {
        if self.present_halted.is_some() {
            return Ok(PresentOutcome {
                presented: false,
                status: self.present_status(),
                timings: None,
            });
        }
        if !force && !self.scene_dirty {
            return Ok(PresentOutcome {
                presented: false,
                status: self.present_status(),
                timings: None,
            });
        }
        self.scene_dirty = false;
        for output in self.outputs.values_mut() {
            output.present.content_dirty = false;
        }
        let started = Instant::now();
        let status = self.present_enabled_outputs(color)?;
        Ok(PresentOutcome {
            presented: self.present_halted.is_none(),
            status,
            timings: Some(FrameTimings {
                render_duration: started.elapsed(),
            }),
        })
    }

    /// Present a single named output when `force` is set or that output is content-dirty.
    pub fn present_named(
        &mut self,
        name: &str,
        color: [f32; 4],
        force: bool,
    ) -> anyhow::Result<PresentOutcome> {
        if self.present_halted.is_some() {
            return Ok(PresentOutcome {
                presented: false,
                status: self.present_status(),
                timings: None,
            });
        }
        let content_dirty = self
            .outputs
            .get(name)
            .is_some_and(|o| o.present.content_dirty);
        if !force && !content_dirty && !self.scene_dirty {
            return Ok(PresentOutcome {
                presented: false,
                status: self.present_status(),
                timings: None,
            });
        }
        let started = Instant::now();
        let presented = self.present_one_named(name, color)?;
        Ok(PresentOutcome {
            presented,
            status: self.present_status(),
            timings: presented.then_some(FrameTimings {
                render_duration: started.elapsed(),
            }),
        })
    }

    /// Returns whether GPU/DRM work was submitted for `name`.
    fn present_one_named(&mut self, name: &str, color: [f32; 4]) -> anyhow::Result<bool> {
        if self.present_halted.is_some() {
            return Ok(false);
        }
        if !self.ensure_vulkan_ready()? {
            return Ok(false);
        }
        let _ = self.flush_hw_cursors();

        self.ensure_present_targets_cached();
        let Some(target) = self
            .cached_present_targets
            .as_ref()
            .and_then(|targets| targets.iter().find(|t| t.name == name).cloned())
        else {
            warn!("No present target named {name}");
            return Ok(false);
        };

        // Preserve global damage until every content-dirty output has presented.
        let other_dirty = self
            .outputs
            .iter()
            .any(|(n, o)| n != name && o.present.content_dirty);
        let damage_snapshot = if other_dirty {
            Some((
                self.pending_damage.clone(),
                self.pending_surface_buffer_damage.clone(),
                self.pending_full_redraw,
                self.pending_pointer_damage,
                self.cursor_buffer_dirty,
                self.dirty_surface_keys.clone(),
            ))
        } else {
            None
        };
        if other_dirty {
            // Each selectively presented output needs a full redraw of its scanout.
            self.pending_full_redraw = true;
        }

        let mut presented = false;
        match self.present_one_output(&target, color) {
            Ok(()) => {
                presented = true;
                if let Some(scanout) = self.outputs.get(&target.name) {
                    if let Some(physical) = scanout.physical.as_ref() {
                        debug!(
                            "Presented {} on {} (CRTC {}, {}x{}@{}Hz)",
                            physical.output.connector_name,
                            physical.drm_path.display(),
                            physical.output.crtc_id,
                            physical.output.mode.width(),
                            physical.output.mode.height(),
                            physical.output.mode.refresh_hz()
                        );
                    } else {
                        debug!(
                            "Presented virtual output {} ({}x{})",
                            target.name, target.width, target.height
                        );
                    }
                }
            }
            Err(err) => {
                if let Some(physical) = target.physical.as_ref() {
                    error!(
                        "Failed to present {} on {}: {err:#}",
                        target.name,
                        physical.drm_path.display()
                    );
                } else {
                    error!("Failed to present virtual output {}: {err:#}", target.name);
                }
                if is_unrecoverable_present_error(&err) {
                    self.halt_presents(&err);
                }
            }
        }

        if let Some((
            pending_damage,
            pending_surface_buffer_damage,
            pending_full_redraw,
            pending_pointer_damage,
            cursor_buffer_dirty,
            dirty_surface_keys,
        )) = damage_snapshot
        {
            self.pending_damage = pending_damage;
            self.pending_surface_buffer_damage = pending_surface_buffer_damage;
            self.pending_full_redraw = pending_full_redraw || self.pending_full_redraw;
            self.pending_pointer_damage = pending_pointer_damage || self.pending_pointer_damage;
            self.cursor_buffer_dirty = cursor_buffer_dirty || self.cursor_buffer_dirty;
            self.dirty_surface_keys.extend(dirty_surface_keys);
        }

        // Drop scanouts for outputs that are no longer presentable (cached names).
        let keep: HashSet<String> = self
            .cached_present_targets
            .as_ref()
            .map(|targets| targets.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default();
        let stale: Vec<String> = self
            .outputs
            .keys()
            .filter(|n| !keep.contains(*n))
            .cloned()
            .collect();
        for stale_name in stale {
            self.disable_and_release_named_scanout(&stale_name);
        }

        Ok(presented)
    }

    /// Present a solid clear on every enabled connected or virtual output.
    ///
    /// Physical targets allocate buffers on the selected render GPU and import them
    /// on each output's DRM card. Virtual targets composite without KMS export.
    /// Failures are logged per output.
    ///
    /// Unchanged physical modes schedule a non-blocking page-flip; the previous FB
    /// stays alive until [`Self::dispatch_page_flips`] reports completion.
    /// Virtual presents complete after GPU work with no flip wait.
    pub fn present_enabled_outputs(&mut self, color: [f32; 4]) -> anyhow::Result<PresentStatus> {
        if self.present_halted.is_some() {
            return Ok(self.present_status());
        }
        if !self.ensure_vulkan_ready()? {
            return Ok(self.present_status());
        }
        let _ = self.flush_hw_cursors();

        self.ensure_present_targets_cached();
        let targets = self.collect_present_targets();
        if targets.is_empty() {
            warn!("No enabled connected or virtual outputs to present");
            let stale: Vec<String> = self.outputs.keys().cloned().collect();
            for name in stale {
                self.disable_and_release_named_scanout(&name);
            }
            return Ok(self.present_status());
        }

        let keep: HashSet<String> = targets.iter().map(|t| t.name.clone()).collect();
        let mut presented = 0usize;
        for target in targets {
            match self.present_one_output(&target, color) {
                Ok(()) => {
                    if let Some(scanout) = self.outputs.get(&target.name) {
                        if let Some(physical) = scanout.physical.as_ref() {
                            debug!(
                                "Presented {} on {} (CRTC {}, {}x{}@{}Hz)",
                                physical.output.connector_name,
                                physical.drm_path.display(),
                                physical.output.crtc_id,
                                physical.output.mode.width(),
                                physical.output.mode.height(),
                                physical.output.mode.refresh_hz()
                            );
                        } else {
                            debug!(
                                "Presented virtual output {} ({}x{})",
                                target.name, target.width, target.height
                            );
                        }
                    }
                    presented += 1;
                }
                Err(err) => {
                    if let Some(physical) = target.physical.as_ref() {
                        error!(
                            "Failed to present {} on {}: {err:#}",
                            target.name,
                            physical.drm_path.display()
                        );
                    } else {
                        error!("Failed to present virtual output {}: {err:#}", target.name);
                    }
                    if is_unrecoverable_present_error(&err) {
                        self.halt_presents(&err);
                        break;
                    }
                }
            }
        }

        let stale: Vec<String> = self
            .outputs
            .keys()
            .filter(|name| !keep.contains(*name))
            .cloned()
            .collect();
        for name in stale {
            self.disable_and_release_named_scanout(&name);
        }
        debug!("Presented {presented} output(s)");
        Ok(self.present_status())
    }

    fn invalidate_present_targets(&mut self) {
        self.cached_present_targets = None;
    }

    fn ensure_present_targets_cached(&mut self) {
        if self.cached_present_targets.is_some() {
            return;
        }
        self.cached_present_targets = Some(self.resolve_present_targets());
    }

    /// Returns cached present targets when available, otherwise resolves once.
    fn collect_present_targets(&self) -> Vec<PresentTarget> {
        if let Some(cached) = &self.cached_present_targets {
            return cached.clone();
        }
        self.resolve_present_targets()
    }

    fn resolve_present_targets(&self) -> Vec<PresentTarget> {
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
                        let refresh_mhz = (output.mode.refresh_hz() as i32)
                            .saturating_mul(1000)
                            .max(1);
                        targets.push(PresentTarget {
                            name: connector.name.clone(),
                            width: output.mode.width(),
                            height: output.mode.height(),
                            refresh_mhz,
                            physical: Some(PhysicalPresent {
                                drm_path: drm_path.clone(),
                                output,
                            }),
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

        for virtual_output in self.virtual_outputs.values() {
            // Prefer the DRM connector when names collide.
            if targets.iter().any(|t| t.name == virtual_output.name) {
                continue;
            }
            targets.push(PresentTarget {
                name: virtual_output.name.clone(),
                width: virtual_output.width,
                height: virtual_output.height,
                refresh_mhz: virtual_output.refresh_mhz,
                physical: None,
            });
        }

        targets.sort_by(|a, b| a.name.cmp(&b.name));
        targets
    }

    fn present_status(&self) -> PresentStatus {
        PresentStatus {
            idle: !self.has_pending_flips(),
        }
    }

    fn has_pending_flips(&self) -> bool {
        self.outputs.values().any(|output| output.primary_flip_busy())
    }

    fn present_one_output(
        &mut self,
        target: &PresentTarget,
        color: [f32; 4],
    ) -> anyhow::Result<()> {
        let mut buffer = self.render_scanout_buffer(target, color)?;

        let Some(physical) = target.physical.as_ref() else {
            return self.complete_virtual_present(&target.name, buffer);
        };

        let reuse_mode = self.outputs.get(&target.name).is_some_and(|prev| {
            prev.physical
                .as_ref()
                .is_some_and(|p| p.matches_target(&physical.drm_path, &physical.output))
        });

        if reuse_mode {
            return self.schedule_or_queue_flip(&target.name, buffer);
        }

        if self
            .outputs
            .get(&target.name)
            .is_some_and(|output| output.primary_flip_busy())
        {
            self.wait_for_connector_flip(&target.name)?;
        }

        self.wait_scanout_gpu(&mut buffer)?;

        let drm_device = self
            .drm_devices
            .opened()
            .get(&physical.drm_path)
            .with_context(|| {
                format!(
                    "DRM device {} is no longer open",
                    physical.drm_path.display()
                )
            })?;

        let mode_blob = ModeBlob::create(drm_device.fd(), &physical.output.mode)
            .context("Failed to create MODE_ID property blob")?;

        let fb_id = buffer
            .drm_fb_id()
            .context("KMS present requires a DRM framebuffer")?;
        atomic_modeset(drm_device.fd(), &physical.output, mode_blob.id(), fb_id)
            .context("Failed atomic modeset")?;

        let zpos = self
            .topologies
            .get(&physical.drm_path)
            .and_then(|topo| topo.plane(physical.output.plane_id))
            .map(|p| p.zpos)
            .unwrap_or(0);

        let cursor_plane: Option<DrmPlaneInfo> = self
            .topologies
            .get(&physical.drm_path)
            .and_then(|topo| topo.cursor_for_crtc(physical.output.crtc_index))
            .cloned();

        let physical_output = PhysicalOutput {
            drm_path: physical.drm_path.clone(),
            output: physical.output.clone(),
            mode_blob,
        };

        let old_buffers = {
            let disable = self.outputs.get(&target.name).and_then(|output| {
                let physical = output.physical.as_ref()?;
                let cursor = output.cursor()?;
                if cursor.plane_id == 0 {
                    return None;
                }
                let props = cursor.props.clone()?;
                Some((
                    physical.drm_path.clone(),
                    cursor.plane_id,
                    props,
                    physical.output.connector_name.clone(),
                ))
            });
            if let Some((drm_path, plane_id, props, name)) = disable {
                if let Some(device) = self.drm_devices.opened().get(&drm_path) {
                    if let Err(err) = atomic_set_cursor_plane(
                        device.fd(),
                        plane_id,
                        &props,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        false,
                    ) {
                        warn!("Failed to disable cursor plane {plane_id} on {name}: {err:#}");
                    }
                }
            }
            self.outputs
                .get_mut(&target.name)
                .and_then(|output| output.planes.take())
                .map(|planes| planes.drain_buffers())
                .unwrap_or_default()
        };
        for old in old_buffers {
            self.release_scanout_buffer(old);
        }

        if let Some(output) = self.outputs.get_mut(&target.name) {
            output.id = OutputId::physical(
                physical.drm_path.clone(),
                physical.output.connector_id,
                target.name.clone(),
            );
            output.physical = Some(physical_output);
            let planes = output.ensure_planes(
                physical.output.plane_id,
                zpos,
                target.width,
                target.height,
                cursor_plane.as_ref(),
            );
            planes.primary.current = Some(buffer);
            planes.primary.pending = None;
            planes.primary.queued = None;
            output.dirty.cursor_image = true;
            output.dirty.cursor_pos = true;
        } else {
            let token = match self.alloc_present_wake_token() {
                Ok(token) => token,
                Err(_) => PRESENT_WAKE_TOKEN_BASE,
            };
            let mut output = OutputState::new(
                OutputId::physical(
                    physical.drm_path.clone(),
                    physical.output.connector_id,
                    target.name.clone(),
                ),
                OutputPresentControl::new(token, target.refresh_mhz),
            );
            output.physical = Some(physical_output);
            let planes = output.ensure_planes(
                physical.output.plane_id,
                zpos,
                target.width,
                target.height,
                cursor_plane.as_ref(),
            );
            planes.primary.current = Some(buffer);
            output.dirty.cursor_image = true;
            output.dirty.cursor_pos = true;
            self.outputs.insert(target.name.clone(), output);
        }
        let _ = self.flush_hw_cursors();
        Ok(())
    }

    /// Finish a virtual present immediately after GPU work (no KMS flip).
    fn complete_virtual_present(
        &mut self,
        name: &str,
        mut buffer: ScanoutBuffer,
    ) -> anyhow::Result<()> {
        self.wait_scanout_gpu(&mut buffer)?;

        let width = buffer.key.width;
        let height = buffer.key.height;

        let old_buffers = if let Some(output) = self.outputs.get_mut(name) {
            output.physical = None;
            output.id = OutputId::virtual_output(name.to_string());
            let planes = output.ensure_primary_planes(0, 0, width, height);
            planes.primary.software = false;
            let old_current = planes.primary.current.replace(buffer);
            let old_pending = planes.primary.pending.take();
            let old_queued = planes.primary.queued.take();
            [old_current, old_pending, old_queued]
        } else {
            let token = match self.alloc_present_wake_token() {
                Ok(token) => token,
                Err(_) => PRESENT_WAKE_TOKEN_BASE,
            };
            let mut output = OutputState::new(
                OutputId::virtual_output(name.to_string()),
                OutputPresentControl::new(token, 60_000),
            );
            let planes = output.ensure_primary_planes(0, 0, width, height);
            planes.primary.current = Some(buffer);
            self.outputs.insert(name.to_string(), output);
            [None, None, None]
        };

        for old in old_buffers.into_iter().flatten() {
            self.release_scanout_buffer(old);
        }
        Ok(())
    }

    fn render_scanout_buffer(
        &mut self,
        target: &PresentTarget,
        color: [f32; 4],
    ) -> anyhow::Result<ScanoutBuffer> {
        let width = target.width;
        let height = target.height;
        let format = vk::Format::B8G8R8A8_UNORM;
        let fourcc = vulkan_to_drm_fourcc(format)
            .with_context(|| format!("Vulkan format {format:?} has no DRM fourcc mapping"))?;

        let mut buffer = {
            let vulkan = self
                .vulkan
                .as_mut()
                .context("VulkanContext missing during present")?;
            if let Some(physical) = target.physical.as_ref() {
                let drm_device = self
                    .drm_devices
                    .opened()
                    .get(&physical.drm_path)
                    .with_context(|| {
                        format!(
                            "DRM device {} is no longer open",
                            physical.drm_path.display()
                        )
                    })?;
                self.scanout_pool.acquire(
                    vulkan,
                    &physical.drm_path,
                    drm_device.fd(),
                    width,
                    height,
                    format,
                    fourcc,
                )?
            } else {
                self.scanout_pool
                    .acquire_virtual(vulkan, width, height, format, fourcc)?
            }
        };

        let _pending_damage = std::mem::take(&mut self.pending_damage);
        let pending_surface_buffer_damage = std::mem::take(&mut self.pending_surface_buffer_damage);
        let force_full = self.pending_full_redraw;
        let hw_cursor = self
            .outputs
            .get(&target.name)
            .is_some_and(|o| o.hw_cursor_active());
        let pointer_damage = self.pending_pointer_damage && !hw_cursor;
        let cursor_buffer_dirty = self.cursor_buffer_dirty && !hw_cursor;
        let dirty_surfaces = std::mem::take(&mut self.dirty_surface_keys);
        self.pending_full_redraw = false;
        if !hw_cursor {
            self.pending_pointer_damage = false;
            self.cursor_buffer_dirty = false;
        } else {
            // Keep flags only if some software-cursor output still needs them.
            if !self
                .outputs
                .values()
                .any(|o| o.cursor().is_some_and(|c| c.software))
            {
                self.pending_pointer_damage = false;
                self.cursor_buffer_dirty = false;
            }
        }

        let views = self
            .output_views
            .get(&target.name)
            .cloned()
            .unwrap_or_default();
        // Non-identity / offset views need full redraws (global damage ≠ output-local,
        // and cursor damage is already output-native). Identity views at the global
        // origin can use partial damage directly.
        let (views_force_full, output_local_damage) =
            match identity_fullscreen_view_origin(&views, width, height) {
                Some((0, 0)) => (false, _pending_damage),
                Some((origin_x, origin_y)) if !(pointer_damage || cursor_buffer_dirty) => (
                    false,
                    translate_damage_list(&_pending_damage, -origin_x, -origin_y),
                ),
                _ => (true, Vec::new()),
            };
        let mut composite_mode = prepare_gpu_composite(
            width,
            height,
            &output_local_damage,
            force_full || views_force_full,
            buffer.fresh,
        );

        let layers: Vec<&SurfaceFrame> = self
            .surface_order
            .iter()
            .filter_map(|key| self.surface_frames.get(key))
            .collect();
        let cursor = if hw_cursor {
            CursorDraw::Hidden
        } else {
            self.cursor_state.draw_ref()
        };
        let pointer_x = self.pointer_x;
        let pointer_y = self.pointer_y;

        let mut batch = GpuWorkBatch::new();

        // Buffer-age path: when the back buffer still holds a recent frame, expand
        // damage across the missed presents and skip the full FB seed copy.
        let next_serial = self
            .outputs
            .get(&target.name)
            .map(|o| o.present_serial.wrapping_add(1))
            .unwrap_or(1);
        let mut skip_scanout_copy = false;
        if let CompositeMode::Partial(regions) = &composite_mode {
            let age = if buffer.fresh || buffer.content_serial == 0 {
                None
            } else {
                Some(next_serial.wrapping_sub(buffer.content_serial))
            };
            if let (Some(age), Some(current)) = (age, regions.first().copied()) {
                let history = self
                    .outputs
                    .get(&target.name)
                    .map(|o| o.damage_history.entries())
                    .unwrap_or_default();
                if let Some(expanded) = expand_damage_for_buffer_age(current, &history, age) {
                    composite_mode = CompositeMode::Partial(vec![expanded]);
                    skip_scanout_copy = true;
                }
            }
        }

        if matches!(composite_mode, CompositeMode::Partial(_)) && !skip_scanout_copy {
            let src_ptr = self.outputs.get(&target.name).and_then(|output| {
                let image = output.primary()?.newest_buffer()?;
                Some(&image.dma_image as *const DmaBufImage)
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

        let history_damage = match &composite_mode {
            CompositeMode::Full => None,
            CompositeMode::Partial(regions) => regions.first().copied(),
        };

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
            // Drop CPU rasters no longer referenced by current guides.
            {
                let live: std::collections::HashSet<_> = self
                    .guides
                    .iter()
                    .filter(|guide| !guide.label.is_empty())
                    .map(|guide| {
                        crate::bitmap_font::GuideLabelKey::new(
                            guide.label.clone(),
                            guide.effective_label_color(),
                        )
                    })
                    .collect();
                self.gpu.guide_labels.retain(|key| live.contains(key));
            }
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
                    &output_local_damage,
                    cursor_buffer_dirty,
                )?;

                self.gpu.surface_textures.prune_guide_labels(
                    vulkan.device(),
                    &compositor.descriptor_pool,
                    self.guides.len() as u32,
                )?;

                {
                    let GpuRenderResources {
                        guide_labels,
                        surface_textures,
                        ..
                    } = &mut self.gpu;
                    for (index, guide) in self.guides.iter().enumerate() {
                        if guide.label.is_empty() {
                            continue;
                        }
                        let color = guide.effective_label_color();
                        let key =
                            crate::bitmap_font::GuideLabelKey::new(guide.label.clone(), color);
                        let raster = guide_labels.get_or_rasterize(&guide.label, color);
                        surface_textures.sync_guide_label(
                            vulkan,
                            &compositor,
                            &mut batch,
                            index as u32,
                            &key,
                            &raster.pixels,
                            raster.width,
                            raster.height,
                        )?;
                    }
                }

                vulkan.ensure_scanout_render_pass()?;
                batch.ensure_recording(vulkan)?;
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
                    &views,
                    &layers,
                    &self.guides,
                    cursor,
                    pointer_x,
                    pointer_y,
                )
                .context("Failed to GPU-composite scene to scanout buffer")?;
                Ok(())
            })();
            if let Err(error) = gpu_result {
                batch.abandon(vulkan);
                self.gpu.surface_textures.clear();
                self.gpu.compositor = Some(compositor);
                return Err(error);
            }
            self.gpu.compositor = Some(compositor);

            buffer.gpu_pending = Some(batch.submit(vulkan)?);
            buffer.fresh = false;
            buffer.content_serial = next_serial;
        }
        if let Some(output) = self.outputs.get_mut(&target.name) {
            output.present_serial = next_serial;
            output.damage_history.push(history_damage);
        }

        Ok(buffer)
    }

    fn schedule_or_queue_flip(
        &mut self,
        connector_name: &str,
        mut buffer: ScanoutBuffer,
    ) -> anyhow::Result<()> {
        let (drm_path, output, flip_busy) = {
            let state = self
                .outputs
                .get(connector_name)
                .context("Missing output for page-flip")?;
            let physical = state
                .physical
                .as_ref()
                .context("Page-flip requires a physical output")?;
            (
                physical.drm_path.clone(),
                physical.output.clone(),
                state.primary_flip_busy(),
            )
        };

        if flip_busy {
            let primary = self
                .outputs
                .get_mut(connector_name)
                .and_then(|o| o.primary_mut())
                .context("Missing primary plane while queueing flip")?;
            if let Some(old) = primary.queued.replace(buffer) {
                self.release_scanout_buffer(old);
            }
            return Ok(());
        }

        // Overlap CPU flip prep with GPU: wait only when the buffer must be scanout-ready.
        self.wait_scanout_gpu(&mut buffer)?;

        let fb_id = buffer
            .drm_fb_id()
            .context("Page-flip requires a DRM framebuffer")?;
        let flip_result = {
            let device =
                self.drm_devices.opened().get(&drm_path).with_context(|| {
                    format!("DRM device {} is no longer open", drm_path.display())
                })?;
            atomic_page_flip(device.fd(), &output, fb_id, self.flip_events.as_ref())
        };

        match flip_result {
            Ok(()) => {
                let primary = self
                    .outputs
                    .get_mut(connector_name)
                    .and_then(|o| o.primary_mut())
                    .context("Missing primary plane after scheduling flip")?;
                primary.pending = Some(buffer);
                Ok(())
            }
            Err(err) => {
                if is_drm_permission_denied(&err) {
                    self.release_scanout_buffer(buffer);
                    return Err(err).context(
                        "DRM page-flip permission denied; not retrying with blocking commit",
                    );
                }
                warn!("Async page-flip failed on {connector_name}: {err:#}; using blocking update");
                {
                    let device = self.drm_devices.opened().get(&drm_path).with_context(|| {
                        format!("DRM device {} is no longer open", drm_path.display())
                    })?;
                    if let Err(blocking_err) = atomic_set_plane_fb(device.fd(), &output, fb_id) {
                        self.release_scanout_buffer(buffer);
                        if is_drm_permission_denied(&blocking_err) {
                            return Err(blocking_err)
                                .context("DRM blocking plane update permission denied");
                        }
                        return Err(blocking_err)
                            .context("Failed blocking plane FB update after page-flip error");
                    }
                }
                let old = {
                    let primary = self
                        .outputs
                        .get_mut(connector_name)
                        .and_then(|o| o.primary_mut())
                        .context("Missing primary plane after blocking flip fallback")?;
                    let old = primary.current.replace(buffer);
                    primary.pending = None;
                    primary.queued = None;
                    old
                };
                if let Some(old) = old {
                    self.release_scanout_buffer(old);
                }
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
        pending.wait(vulkan)
    }

    fn retire_page_flip(&mut self, crtc_id: u32) -> anyhow::Result<Option<String>> {
        let Some(connector_name) = self.outputs.iter().find_map(|(name, output)| {
            output
                .physical
                .as_ref()
                .is_some_and(|p| p.output.crtc_id == crtc_id)
                .then(|| name.clone())
        }) else {
            warn!("Ignoring page-flip completion for unknown CRTC {crtc_id}");
            return Ok(None);
        };

        let (old, queued) = {
            let primary = self
                .outputs
                .get_mut(&connector_name)
                .and_then(|o| o.primary_mut())
                .context("Missing primary plane during flip retirement")?;
            let Some(new_current) = primary.pending.take() else {
                warn!("Page-flip completion without pending buffer on {connector_name}");
                return Ok(Some(connector_name));
            };
            let old = primary.current.replace(new_current);
            let queued = primary.queued.take();
            (old, queued)
        };
        if let Some(old) = old {
            self.release_scanout_buffer(old);
        }

        if let Some(queued) = queued {
            self.schedule_or_queue_flip(&connector_name, queued)?;
        }
        Ok(Some(connector_name))
    }

    /// Update HW cursor planes for all outputs (pos/image). Software fallbacks stay on primary.
    pub(crate) fn flush_hw_cursors(&mut self) -> anyhow::Result<()> {
        if !self.any_hw_cursor_capable() {
            return Ok(());
        }
        if self.vulkan.is_none() && !matches!(self.cursor_state, CursorState::Hidden) {
            // Defer until Vulkan is ready; dirty flags remain set.
            return Ok(());
        }

        let pointer_x = self.pointer_x;
        let pointer_y = self.pointer_y;
        let hidden = matches!(self.cursor_state, CursorState::Hidden);
        let names: Vec<String> = self.outputs.keys().cloned().collect();
        let active_name = self.output_name_for_pointer(pointer_x, pointer_y);

        for name in names {
            let needs = self.outputs.get(&name).is_some_and(|o| {
                o.cursor().is_some_and(|c| c.plane_id != 0)
                    && (o.dirty.cursor_pos || o.dirty.cursor_image || hidden)
            });
            if !needs {
                continue;
            }

            let show_here = active_name.as_ref() == Some(&name) && !hidden;
            if let Err(err) = self.commit_hw_cursor_on_output(&name, show_here, pointer_x, pointer_y)
            {
                warn!("HW cursor update failed on {name}: {err:#}; falling back to software");
                if let Some(cursor) = self
                    .outputs
                    .get_mut(&name)
                    .and_then(|o| o.cursor_mut())
                {
                    cursor.software = true;
                }
                self.pending_pointer_damage = true;
                self.cursor_buffer_dirty = true;
                self.mark_dirty_if_active();
            } else if let Some(output) = self.outputs.get_mut(&name) {
                output.dirty.cursor_pos = false;
                output.dirty.cursor_image = false;
            }
        }
        Ok(())
    }

    fn output_name_for_pointer(&self, pointer_x: i32, pointer_y: i32) -> Option<String> {
        let mut fallback = None;
        for (name, output) in &self.outputs {
            if output.physical.is_none() {
                continue;
            }
            let views = self.output_views.get(name).cloned().unwrap_or_default();
            if views.is_empty() {
                // No views yet: treat as covering global origin-sized mode if pointer in range.
                if let Some(primary) = output.primary() {
                    let w = primary.geometry.crtc_w as i32;
                    let h = primary.geometry.crtc_h as i32;
                    if pointer_x >= 0 && pointer_y >= 0 && pointer_x < w && pointer_y < h {
                        return Some(name.clone());
                    }
                }
                fallback = fallback.or(Some(name.clone()));
                continue;
            }
            if view_at_source(&views, pointer_x as f64, pointer_y as f64).is_some() {
                return Some(name.clone());
            }
            fallback = fallback.or(Some(name.clone()));
        }
        fallback
    }

    fn commit_hw_cursor_on_output(
        &mut self,
        name: &str,
        show: bool,
        pointer_x: i32,
        pointer_y: i32,
    ) -> anyhow::Result<()> {
        let (drm_path, crtc_id, plane_id, props, need_image, mode_w, mode_h) = {
            let output = self.outputs.get(name).context("missing output")?;
            let physical = output
                .physical
                .as_ref()
                .context("HW cursor requires physical output")?;
            let cursor = output.cursor().context("missing cursor pipeline")?;
            if cursor.software || cursor.plane_id == 0 {
                anyhow::bail!("cursor plane not in HW mode");
            }
            let props = cursor
                .props
                .clone()
                .context("cursor plane missing atomic props")?;
            if let Some(info) = self
                .topologies
                .get(&physical.drm_path)
                .and_then(|t| t.plane(cursor.plane_id))
            {
                if !info.formats.is_empty() && !info.formats.contains(&DRM_FORMAT_ARGB8888) {
                    anyhow::bail!("cursor plane does not advertise AR24");
                }
            }
            (
                physical.drm_path.clone(),
                physical.output.crtc_id,
                cursor.plane_id,
                props,
                output.dirty.cursor_image || cursor.current.is_none(),
                physical.output.mode.width(),
                physical.output.mode.height(),
            )
        };

        if !show {
            let device = self
                .drm_devices
                .opened()
                .get(&drm_path)
                .with_context(|| format!("DRM device {} is not open", drm_path.display()))?;
            atomic_set_cursor_plane(
                device.fd(),
                plane_id,
                &props,
                0,
                0,
                0,
                0,
                0,
                0,
                false,
            )?;
            if let Some(cursor) = self.outputs.get_mut(name).and_then(|o| o.cursor_mut()) {
                cursor.geometry = PlaneGeometry::cursor(0, 0, 0, 0);
            }
            return Ok(());
        }

        let (hotspot_x, hotspot_y, pixels, width, height) =
            self.hw_cursor_image_pixels().context("cursor image not HW-compatible")?;

        let views = self.output_views.get(name).cloned().unwrap_or_default();
        let (local_x, local_y) = pointer_to_output_local(&views, pointer_x, pointer_y, mode_w, mode_h);
        let crtc_x = local_x - hotspot_x;
        let crtc_y = local_y - hotspot_y;

        if need_image {
            self.upload_hw_cursor_fb(name, &drm_path, &pixels, width, height)?;
        }

        let fb_id = self
            .outputs
            .get(name)
            .and_then(|o| o.cursor())
            .and_then(|c| c.current.as_ref())
            .and_then(|b| b.drm_fb_id())
            .context("cursor plane has no FB")?;

        // Probe once when uploading a new image.
        if need_image {
            let device = self
                .drm_devices
                .opened()
                .get(&drm_path)
                .with_context(|| format!("DRM device {} is not open", drm_path.display()))?;
            atomic_set_cursor_plane(
                device.fd(),
                plane_id,
                &props,
                crtc_id,
                fb_id,
                crtc_x,
                crtc_y,
                width,
                height,
                true,
            )
            .context("cursor plane TEST_ONLY rejected")?;
        }

        let device = self
            .drm_devices
            .opened()
            .get(&drm_path)
            .with_context(|| format!("DRM device {} is not open", drm_path.display()))?;
        atomic_set_cursor_plane(
            device.fd(),
            plane_id,
            &props,
            crtc_id,
            fb_id,
            crtc_x,
            crtc_y,
            width,
            height,
            false,
        )?;

        if let Some(cursor) = self.outputs.get_mut(name).and_then(|o| o.cursor_mut()) {
            cursor.geometry = PlaneGeometry::cursor(crtc_x, crtc_y, width, height);
        }
        Ok(())
    }

    fn hw_cursor_image_pixels(&self) -> Option<(i32, i32, Vec<u8>, u32, u32)> {
        let frame = match self.cursor_state.draw_ref() {
            CursorDraw::Hidden => return None,
            CursorDraw::Default => crate::default_cursor::default_cursor_frame(),
            CursorDraw::Client(frame) => frame,
        };
        if frame.dmabuf.is_some() {
            return None;
        }
        if frame.buffer_scale.max(1) != 1 {
            return None;
        }
        if BufferTransform::from_raw(frame.buffer_transform)
            != Some(BufferTransform::Normal)
        {
            return None;
        }
        let width = frame.width as u32;
        let height = frame.height as u32;
        if width == 0 || height == 0 || width > HW_CURSOR_MAX_SIZE || height > HW_CURSOR_MAX_SIZE {
            return None;
        }
        let pixels = prepare_cursor_argb_pixels(frame)?;
        Some((frame.hotspot_x, frame.hotspot_y, pixels, width, height))
    }

    fn upload_hw_cursor_fb(
        &mut self,
        output_name: &str,
        drm_path: &Path,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        let fourcc = DRM_FORMAT_ARGB8888;
        let format = vk::Format::B8G8R8A8_UNORM;
        let drm_device = self
            .drm_devices
            .opened()
            .get(drm_path)
            .with_context(|| format!("DRM device {} is not open", drm_path.display()))?;
        let vulkan = self
            .vulkan
            .as_mut()
            .context("Vulkan missing for cursor FB upload")?;

        let mut buffer = self.scanout_pool.acquire(
            vulkan,
            &drm_path.to_path_buf(),
            drm_device.fd(),
            width,
            height,
            format,
            fourcc,
        )?;
        upload_bgra_to_image(vulkan, &buffer.dma_image, pixels, width, height)?;
        buffer.fresh = false;

        let old = {
            let cursor = self
                .outputs
                .get_mut(output_name)
                .and_then(|o| o.cursor_mut())
                .context("missing cursor pipeline for upload")?;
            cursor.current.replace(buffer)
        };
        if let Some(old) = old {
            self.release_scanout_buffer(old);
        }
        Ok(())
    }

    fn wait_for_connector_flip(&mut self, connector_name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.outputs.contains_key(connector_name),
            "Missing output while waiting for flip"
        );

        for _ in 0..1_000 {
            if !self
                .outputs
                .get(connector_name)
                .is_some_and(|output| output.primary_flip_busy())
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
                let _ = self.retire_page_flip(flip.crtc_id)?;
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

    /// Ensure Vulkan is ready for presentation.
    ///
    /// Prefers a seat-opened DRM primary node when available; otherwise initializes
    /// Vulkan without a preferred path for virtual-only presentation.
    fn ensure_vulkan_ready(&mut self) -> anyhow::Result<bool> {
        if let Some(render_path) = self.resolved_render_device_path() {
            debug!("Using render device {}", render_path.display());
            self.ensure_vulkan(Some(&render_path))?;
            return Ok(true);
        }
        if !self.virtual_outputs.is_empty() {
            self.ensure_vulkan(None)?;
            return Ok(true);
        }
        warn!("No render device or virtual outputs available; skipping presentation");
        Ok(false)
    }

    fn ensure_vulkan(&mut self, preferred_drm_path: Option<&Path>) -> anyhow::Result<()> {
        let needs_recreate = match &self.vulkan {
            None => true,
            Some(vk) => match (preferred_drm_path, vk.drm_device_path()) {
                (Some(preferred), Some(selected)) => selected != preferred,
                // Keep an existing context when switching to virtual-only (no preferred path).
                (None, _) => false,
                // Prefer recreating when a preferred DRM path becomes available.
                (Some(_), None) => true,
            },
        };

        if needs_recreate {
            self.drain_scanouts();
            self.invalidate_surface_textures();
            match preferred_drm_path {
                Some(path) => info!("Initializing Vulkan for DRM device {}", path.display()),
                None => info!("Initializing Vulkan for virtual outputs (no DRM master)"),
            }
            self.vulkan = Some(VulkanContext::new(preferred_drm_path)?);
        }

        Ok(())
    }
}

/// Origin of a single identity fullscreen view, or `(0, 0)` when there are no views.
///
/// Returns `None` when views are non-identity (scaled, multi-view, or partial dest),
/// which requires a full scanout redraw because global damage is not output-local.
fn identity_fullscreen_view_origin(
    views: &[View],
    output_width: u32,
    output_height: u32,
) -> Option<(i32, i32)> {
    if views.is_empty() {
        return Some((0, 0));
    }
    if views.len() != 1 {
        return None;
    }
    let view = &views[0];
    let (sx, sy, sw, sh) = view.source;
    let (dx, dy, dw, dh) = view.dest;
    if dx == 0
        && dy == 0
        && dw == output_width as i32
        && dh == output_height as i32
        && sw == dw
        && sh == dh
    {
        Some((sx, sy))
    } else {
        None
    }
}

fn pointer_to_output_local(
    views: &[View],
    pointer_x: i32,
    pointer_y: i32,
    mode_w: u32,
    mode_h: u32,
) -> (i32, i32) {
    if let Some((sx, sy)) = identity_fullscreen_view_origin(views, mode_w, mode_h) {
        return (pointer_x - sx, pointer_y - sy);
    }
    if views.is_empty() {
        return (pointer_x, pointer_y);
    }
    let (x, y) = lumalla_shared::map_source_to_dest(views, pointer_x as f64, pointer_y as f64);
    (x.round() as i32, y.round() as i32)
}

/// Convert cursor SHM pixels to opaque-capable BGRA for DRM AR24 / Vulkan B8G8R8A8.
fn prepare_cursor_argb_pixels(frame: &CursorFrame) -> Option<Vec<u8>> {
    let row_bytes = frame.width.checked_mul(4)?;
    if frame.stride < row_bytes {
        return None;
    }
    let needed = frame.stride.checked_mul(frame.height)?;
    if frame.pixels.len() < needed {
        return None;
    }
    let force_opaque = frame.format == WL_SHM_FORMAT_XRGB8888;
    let mut out = Vec::with_capacity(frame.width * frame.height * 4);
    for y in 0..frame.height {
        let row = y * frame.stride;
        for x in 0..frame.width {
            let i = row + x * 4;
            let mut px = [
                frame.pixels[i],
                frame.pixels[i + 1],
                frame.pixels[i + 2],
                frame.pixels[i + 3],
            ];
            if force_opaque {
                px[3] = 255;
            }
            out.extend_from_slice(&px);
        }
    }
    Some(out)
}

fn translate_damage_list(damage: &[DamageRect], dx: i32, dy: i32) -> Vec<DamageRect> {
    damage
        .iter()
        .copied()
        .map(|rect| DamageRect {
            x: rect.x + dx,
            y: rect.y + dy,
            width: rect.width,
            height: rect.height,
        })
        .collect()
}

struct CaptureRegion {
    name: String,
    fb_x: u32,
    fb_y: u32,
    fb_w: u32,
    fb_h: u32,
    dest_x: u32,
    dest_y: u32,
    logical_w: u32,
    logical_h: u32,
}

impl CaptureRegion {
    fn from_output_views(output: &Output, x: i32, y: i32, width: i32, height: i32) -> Vec<Self> {
        let scale = output.scale.max(1);
        let mut regions = Vec::new();
        for view in &output.views {
            let (sx, sy, sw, sh) = view.source;
            if sw <= 0 || sh <= 0 {
                continue;
            }
            let ix0 = x.max(sx);
            let iy0 = y.max(sy);
            let ix1 = (x + width).min(sx + sw);
            let iy1 = (y + height).min(sy + sh);
            if ix0 >= ix1 || iy0 >= iy1 {
                continue;
            }
            let mapped = map_rect_through_view(
                view,
                [
                    ix0 as f32,
                    iy0 as f32,
                    (ix1 - ix0) as f32,
                    (iy1 - iy0) as f32,
                ],
            );
            if mapped[2] <= 0.0 || mapped[3] <= 0.0 {
                continue;
            }
            let fb_x = (mapped[0] * scale as f32).round().max(0.0) as u32;
            let fb_y = (mapped[1] * scale as f32).round().max(0.0) as u32;
            let fb_w = (mapped[2] * scale as f32).round().max(1.0) as u32;
            let fb_h = (mapped[3] * scale as f32).round().max(1.0) as u32;
            regions.push(Self {
                name: output.name.clone(),
                fb_x,
                fb_y,
                fb_w,
                fb_h,
                dest_x: (ix0 - x) as u32,
                dest_y: (iy0 - y) as u32,
                logical_w: (ix1 - ix0) as u32,
                logical_h: (iy1 - iy0) as u32,
            });
        }
        regions
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

#[derive(Clone)]
struct PhysicalPresent {
    drm_path: PathBuf,
    output: ConnectedOutput,
}

#[derive(Clone)]
struct PresentTarget {
    name: String,
    width: u32,
    height: u32,
    refresh_mhz: i32,
    /// `None` for virtual outputs (no KMS).
    physical: Option<PhysicalPresent>,
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

        let arena = Arena::new();
        state.sync_surface_scene(&[(1, 3, 40, 50), (1, 2, 10, 20)], &arena);

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
        let arena = Arena::new();
        state.sync_surface_scene(&[], &arena);
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

        let upload = composite_surface_full(&[&frame], 3, 1, [0.0, 0.0, 0.0, 1.0]).unwrap();
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

    #[test]
    fn identity_fullscreen_view_at_origin() {
        let views = [View {
            name: "main".into(),
            source: (0, 0, 1920, 1080),
            dest: (0, 0, 1920, 1080),
        }];
        assert_eq!(
            identity_fullscreen_view_origin(&views, 1920, 1080),
            Some((0, 0))
        );
    }

    #[test]
    fn identity_fullscreen_view_rejects_scale() {
        let views = [View {
            name: "main".into(),
            source: (0, 0, 3840, 2160),
            dest: (0, 0, 1920, 1080),
        }];
        assert_eq!(identity_fullscreen_view_origin(&views, 1920, 1080), None);
    }

    #[test]
    fn translate_damage_shifts_rects() {
        let damage = [DamageRect {
            x: 100,
            y: 50,
            width: 10,
            height: 20,
        }];
        let shifted = translate_damage_list(&damage, -100, -50);
        assert_eq!(shifted[0].x, 0);
        assert_eq!(shifted[0].y, 0);
        assert_eq!(shifted[0].width, 10);
        assert_eq!(shifted[0].height, 20);
    }

    #[test]
    fn prepare_cursor_argb_forces_xrgb_opaque() {
        let frame = CursorFrame {
            owner_id: 1,
            surface_id: 1,
            buffer_id: 1,
            pixels: vec![10, 20, 30, 0, 40, 50, 60, 128],
            width: 2,
            height: 1,
            stride: 8,
            format: WL_SHM_FORMAT_XRGB8888,
            hotspot_x: 0,
            hotspot_y: 0,
            buffer_scale: 1,
            buffer_transform: 0,
            dmabuf: None,
        };
        let pixels = prepare_cursor_argb_pixels(&frame).unwrap();
        assert_eq!(pixels, vec![10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn prepare_cursor_argb_rejects_truncated_pixels() {
        let frame = CursorFrame {
            owner_id: 1,
            surface_id: 1,
            buffer_id: 1,
            pixels: vec![0; 4],
            width: 2,
            height: 2,
            stride: 8,
            format: WL_SHM_FORMAT_ARGB8888,
            hotspot_x: 0,
            hotspot_y: 0,
            buffer_scale: 1,
            buffer_transform: 0,
            dmabuf: None,
        };
        assert!(prepare_cursor_argb_pixels(&frame).is_none());
    }

    #[test]
    fn pointer_to_output_local_subtracts_view_origin() {
        let views = [View {
            name: "main".into(),
            source: (100, 200, 1920, 1080),
            dest: (0, 0, 1920, 1080),
        }];
        assert_eq!(
            pointer_to_output_local(&views, 150, 250, 1920, 1080),
            (50, 50)
        );
    }

    #[test]
    fn hw_cursor_pipeline_from_plane_info() {
        use crate::output::{DrmPlaneInfo, PlaneAtomicProps, PlaneKind, PlanePipeline};
        let info = DrmPlaneInfo {
            plane_id: 77,
            kind: PlaneKind::Cursor,
            possible_crtcs: 1,
            zpos: 255,
            zpos_mutable: false,
            props: PlaneAtomicProps {
                fb_id: 1,
                crtc_id: 2,
                src_x: 3,
                src_y: 4,
                src_w: 5,
                src_h: 6,
                crtc_x: 7,
                crtc_y: 8,
                crtc_w: 9,
                crtc_h: 10,
                zpos: Some(11),
            },
            formats: vec![DRM_FORMAT_ARGB8888],
        };
        let pipe = PlanePipeline::hw_cursor(&info);
        assert!(pipe.is_hw_cursor());
        assert_eq!(pipe.plane_id, 77);
        assert!(!PlanePipeline::software_cursor().is_hw_cursor());
    }

    #[test]
    fn cursor_crtc_position_applies_hotspot() {
        let hotspot_x = 4;
        let hotspot_y = 6;
        let local = (100, 200);
        let crtc_x = local.0 - hotspot_x;
        let crtc_y = local.1 - hotspot_y;
        assert_eq!((crtc_x, crtc_y), (96, 194));
    }
}
