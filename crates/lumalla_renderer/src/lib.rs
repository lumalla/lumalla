use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use ash::vk;
use log::{debug, error, info, warn};
use lumalla_seat::SeatState;
use lumalla_shared::{DrmDeviceState, OutputConfig};
use mio::{Interest, Registry, Token, event::Source};

pub mod drm;
pub mod vulkan;

mod default_cursor;
pub mod scheduler;
mod scanout_pool;

use crate::default_cursor::default_cursor_frame;
use crate::scanout_pool::{ScanoutBuffer, ScanoutBufferPool};
use crate::drm::{
    CompletedPageFlip, ConnectedOutput, DrmDevices, DrmDispatchResult, FlipEventQueue, ModeBlob,
    atomic_modeset, atomic_page_flip, atomic_set_plane_fb, dispatch_drm_events,
    resolve_connected_output,
};
pub use crate::scheduler::{FrameTimings, RenderScheduler};
use crate::vulkan::{VulkanContext, clear_framebuffer_to_color, upload_bgra_to_image, vulkan_to_drm_fourcc};

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

#[derive(Debug)]
pub struct SurfaceFrame {
    pub owner_id: u32,
    pub surface_id: u32,
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
    pub x: i32,
    pub y: i32,
    pub buffer_scale: i32,
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
        let required = self
            .stride
            .checked_mul(self.height)
            .context("Surface frame size overflows")?;
        anyhow::ensure!(
            self.pixels.len() >= required,
            "Surface frame pixel data is truncated"
        );
        anyhow::ensure!(
            matches!(self.format, WL_SHM_FORMAT_ARGB8888 | WL_SHM_FORMAT_XRGB8888),
            "Unsupported Wayland SHM format {:#x}",
            self.format
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CursorFrame {
    pub owner_id: u32,
    pub surface_id: u32,
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub buffer_scale: i32,
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
        let required = self
            .stride
            .checked_mul(self.height)
            .context("Cursor frame size overflows")?;
        anyhow::ensure!(
            self.pixels.len() >= required,
            "Cursor frame pixel data is truncated"
        );
        anyhow::ensure!(
            matches!(self.format, WL_SHM_FORMAT_ARGB8888 | WL_SHM_FORMAT_XRGB8888),
            "Unsupported cursor SHM format {:#x}",
            self.format
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
        })
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
        self.surface_frames.insert(key, frame);
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn remove_surface_frame(&mut self, owner_id: u32, surface_id: u32) -> anyhow::Result<()> {
        let key = (owner_id, surface_id);
        if self.surface_frames.remove(&key).is_some() {
            self.surface_order.retain(|k| *k != key);
            self.mark_dirty_if_active();
        }
        Ok(())
    }

    pub fn remove_client_frames(&mut self, owner_id: u32) -> anyhow::Result<()> {
        let before = self.surface_frames.len();
        self.surface_frames.retain(|(owner, _), _| *owner != owner_id);
        self.surface_order.retain(|(owner, _)| *owner != owner_id);
        let cursor_removed = self
            .cursor_frame
            .as_ref()
            .is_some_and(|cursor| cursor.owner_id == owner_id);
        if cursor_removed {
            self.cursor_frame = None;
        }
        if self.surface_frames.len() != before || cursor_removed {
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
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn clear_cursor_frame(&mut self) -> anyhow::Result<()> {
        if self.cursor_frame.is_none() {
            return Ok(());
        }
        self.cursor_frame = None;
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn update_pointer_position(&mut self, x: i32, y: i32) -> anyhow::Result<()> {
        if self.pointer_x == x && self.pointer_y == y {
            return Ok(());
        }
        self.pointer_x = x;
        self.pointer_y = y;
        self.mark_dirty_if_active();
        Ok(())
    }

    pub fn update_cursor_hotspot(&mut self, hotspot_x: i32, hotspot_y: i32) -> anyhow::Result<()> {
        let Some(cursor) = self.cursor_frame.as_mut() else {
            return Ok(());
        };
        if cursor.hotspot_x == hotspot_x && cursor.hotspot_y == hotspot_y {
            return Ok(());
        }
        cursor.hotspot_x = hotspot_x;
        cursor.hotspot_y = hotspot_y;
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

    /// Open missing DRM devices via the seat (fresh open after VT resume).
    pub fn activate_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        self.drm_devices.activate(seat)
    }

    /// Close seat-opened DRM devices after session disable was acknowledged.
    pub fn deactivate_drm(&mut self, seat: &SeatState) {
        self.drain_scanouts();
        let _ = self.flip_events.drain();
        self.drm_devices.deactivate(seat);
    }

    /// Close removed / open newly discovered DRM devices while the seat is active.
    pub fn reconcile_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
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
    }

    fn release_output_scanout(&mut self, scanout: OutputScanout) {
        self.scanout_pool.release(scanout.current);
        if let Some(pending) = scanout.pending {
            self.scanout_pool.release(pending);
        }
        if let Some(queued) = scanout.queued {
            self.scanout_pool.release(queued);
        }
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
        self.scanouts.values().any(|scanout| scanout.pending.is_some())
    }

    fn present_one_output(
        &mut self,
        target: &PresentTarget,
        color: [f32; 4],
    ) -> anyhow::Result<()> {
        let buffer = self.render_scanout_buffer(target, color)?;

        let reuse_mode = self.scanouts.get(&target.connector_name).is_some_and(|prev| {
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

        let buffer = {
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

        {
            let vulkan = self
                .vulkan
                .as_mut()
                .context("VulkanContext missing during present")?;
            vulkan.ensure_scanout_render_pass()?;
        }

        {
            let vulkan = self
                .vulkan
                .as_ref()
                .context("VulkanContext missing during present")?;
            let render_pass = vulkan.scanout_render_pass()?;

            clear_framebuffer_to_color(
                vulkan.device(),
                vulkan.graphics_command_pool(),
                render_pass,
                &buffer.framebuffer,
                color,
            )
            .context("Failed to clear scanout image")?;

            let frames: Vec<&SurfaceFrame> = self
                .surface_order
                .iter()
                .filter_map(|key| self.surface_frames.get(key))
                .collect();
            let upload = composite_scene_upload(
                &frames,
                self.cursor_frame.as_ref(),
                self.pointer_x,
                self.pointer_y,
                width,
                height,
                color,
            )?;
            upload_bgra_to_image(
                vulkan.device(),
                vulkan.physical_device(),
                vulkan.graphics_command_pool(),
                &buffer.dma_image,
                &upload.pixels,
                upload.width,
                upload.height,
            )
            .context("Failed to upload Wayland SHM scene")?;
        }

        Ok(buffer)
    }

    fn schedule_or_queue_flip(
        &mut self,
        connector_name: &str,
        buffer: ScanoutBuffer,
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
                self.scanout_pool.release(old);
            }
            return Ok(());
        }

        let fb_id = buffer.drm_fb_id();
        let flip_result = {
            let device = self
                .drm_devices
                .opened()
                .get(&drm_path)
                .with_context(|| format!("DRM device {} is no longer open", drm_path.display()))?;
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
                warn!(
                    "Async page-flip failed on {connector_name}: {err:#}; using blocking update"
                );
                {
                    let device = self.drm_devices.opened().get(&drm_path).with_context(|| {
                        format!("DRM device {} is no longer open", drm_path.display())
                    })?;
                    atomic_set_plane_fb(device.fd(), &output, fb_id)
                        .context("Failed blocking plane FB update after page-flip error")?;
                }
                let scanout = self
                    .scanouts
                    .get_mut(connector_name)
                    .context("Missing scanout after blocking flip fallback")?;
                let old = std::mem::replace(&mut scanout.current, buffer);
                self.scanout_pool.release(old);
                scanout.pending = None;
                scanout.queued = None;
                Ok(())
            }
        }
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
            self.scanout_pool.release(old);
            scanout.queued.take()
        };

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

struct PresentTarget {
    drm_path: PathBuf,
    connector_name: String,
    output: ConnectedOutput,
}

struct PreparedSurfaceUpload {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

fn composite_scene_upload(
    frames: &[&SurfaceFrame],
    cursor: Option<&CursorFrame>,
    pointer_x: i32,
    pointer_y: i32,
    output_width: u32,
    output_height: u32,
    clear: [f32; 4],
) -> anyhow::Result<PreparedSurfaceUpload> {
    let mut upload = composite_surface_upload(frames, output_width, output_height, clear)?;
    match cursor {
        Some(client) => composite_cursor(
            &mut upload.pixels,
            output_width as usize,
            output_height as usize,
            client,
            pointer_x,
            pointer_y,
        )?,
        None => composite_cursor(
            &mut upload.pixels,
            output_width as usize,
            output_height as usize,
            default_cursor_frame(),
            pointer_x,
            pointer_y,
        )?,
    }
    Ok(upload)
}

fn composite_cursor(
    pixels: &mut [u8],
    output_width: usize,
    output_height: usize,
    cursor: &CursorFrame,
    pointer_x: i32,
    pointer_y: i32,
) -> anyhow::Result<()> {
    cursor.validate()?;
    let row_bytes = output_width
        .checked_mul(4)
        .context("Output row size overflows")?;
    let scale = cursor.buffer_scale.max(1) as usize;
    let dest_w = cursor.width / scale;
    let dest_h = cursor.height / scale;
    if dest_w == 0 || dest_h == 0 {
        return Ok(());
    }
    let dest_x = pointer_x - cursor.hotspot_x;
    let dest_y = pointer_y - cursor.hotspot_y;
    for dy in 0..dest_h {
        let out_y = dest_y + dy as i32;
        if out_y < 0 || out_y as usize >= output_height {
            continue;
        }
        let source_y = ((dy as u128 * cursor.height as u128) / dest_h as u128) as usize;
        for dx in 0..dest_w {
            let out_x = dest_x + dx as i32;
            if out_x < 0 || out_x as usize >= output_width {
                continue;
            }
            let source_x = ((dx as u128 * cursor.width as u128) / dest_w as u128) as usize;
            let source = source_y * cursor.stride + source_x * 4;
            let destination = out_y as usize * row_bytes + out_x as usize * 4;
            let src = &cursor.pixels[source..source + 4];
            let alpha = if cursor.format == WL_SHM_FORMAT_ARGB8888 {
                src[3] as u16
            } else {
                255
            };
            if alpha == 0 {
                continue;
            }
            if alpha == 255 {
                pixels[destination..destination + 4].copy_from_slice(src);
                if cursor.format == WL_SHM_FORMAT_XRGB8888 {
                    pixels[destination + 3] = u8::MAX;
                }
                continue;
            }
            let inv = 255 - alpha;
            for channel in 0..3 {
                let dst = pixels[destination + channel] as u16;
                let src_channel = src[channel] as u16;
                pixels[destination + channel] =
                    ((src_channel * alpha + dst * inv) / 255) as u8;
            }
            pixels[destination + 3] = u8::MAX;
        }
    }
    Ok(())
}

fn composite_surface_upload(
    frames: &[&SurfaceFrame],
    output_width: u32,
    output_height: u32,
    clear: [f32; 4],
) -> anyhow::Result<PreparedSurfaceUpload> {
    anyhow::ensure!(
        output_width > 0 && output_height > 0,
        "Output dimensions must be non-zero"
    );
    let width = output_width as usize;
    let height = output_height as usize;
    let row_bytes = width
        .checked_mul(4)
        .context("Scaled surface row size overflows")?;
    let capacity = row_bytes
        .checked_mul(height)
        .context("Scaled surface size overflows")?;
    let clear_b = (clear[2].clamp(0.0, 1.0) * 255.0) as u8;
    let clear_g = (clear[1].clamp(0.0, 1.0) * 255.0) as u8;
    let clear_r = (clear[0].clamp(0.0, 1.0) * 255.0) as u8;
    let mut pixels = vec![0u8; capacity];
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.copy_from_slice(&[clear_b, clear_g, clear_r, 0xff]);
    }

    for frame in frames {
        frame.validate()?;
        let scale = frame.buffer_scale.max(1) as usize;
        let dest_w = frame.width / scale;
        let dest_h = frame.height / scale;
        if dest_w == 0 || dest_h == 0 {
            continue;
        }
        for dy in 0..dest_h {
            let out_y = frame.y + dy as i32;
            if out_y < 0 || out_y as usize >= height {
                continue;
            }
            let source_y = ((dy as u128 * frame.height as u128) / dest_h as u128) as usize;
            for dx in 0..dest_w {
                let out_x = frame.x + dx as i32;
                if out_x < 0 || out_x as usize >= width {
                    continue;
                }
                let source_x = ((dx as u128 * frame.width as u128) / dest_w as u128) as usize;
                let source = source_y * frame.stride + source_x * 4;
                let destination = out_y as usize * row_bytes + out_x as usize * 4;
                pixels[destination..destination + 4]
                    .copy_from_slice(&frame.pixels[source..source + 4]);
                if frame.format == WL_SHM_FORMAT_XRGB8888 {
                    pixels[destination + 3] = u8::MAX;
                }
            }
        }
    }

    Ok(PreparedSurfaceUpload {
        pixels,
        width: output_width,
        height: output_height,
    })
}

impl Source for RendererState {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.drm_devices.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.drm_devices.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        self.drm_devices.deregister(registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> SurfaceFrame {
        SurfaceFrame {
            owner_id: 1,
            surface_id: 2,
            pixels: vec![0; 16],
            width: 2,
            height: 2,
            stride: 8,
            format: 0,
            x: 0,
            y: 0,
            buffer_scale: 1,
        }
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
            pixels: vec![1, 2, 3, 0, 4, 5, 6, 0],
            width: 2,
            height: 1,
            stride: 8,
            format: WL_SHM_FORMAT_XRGB8888,
            x: 0,
            y: 0,
            buffer_scale: 1,
        };

        let upload =
            composite_surface_upload(&[&frame], 3, 1, [0.0, 0.0, 0.0, 1.0]).unwrap();
        assert_eq!((upload.width, upload.height), (3, 1));
        assert_eq!(
            upload.pixels,
            vec![1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 255]
        );
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
            ..frame()
        };
        let upload =
            composite_surface_upload(&[&frame], 2, 1, [0.0, 0.0, 0.0, 1.0]).unwrap();
        assert_eq!(upload.pixels, vec![0, 0, 0, 255, 9, 8, 7, 6]);
    }

    #[test]
    fn composites_cursor_with_hotspot() {
        let cursor = CursorFrame {
            owner_id: 1,
            surface_id: 3,
            pixels: vec![10, 20, 30, 255],
            width: 1,
            height: 1,
            stride: 4,
            format: WL_SHM_FORMAT_ARGB8888,
            hotspot_x: 0,
            hotspot_y: 0,
            buffer_scale: 1,
        };
        let upload = composite_scene_upload(&[], Some(&cursor), 1, 0, 2, 1, [0.0, 0.0, 0.0, 1.0])
            .unwrap();
        assert_eq!(upload.pixels, vec![0, 0, 0, 255, 10, 20, 30, 255]);
    }
}
