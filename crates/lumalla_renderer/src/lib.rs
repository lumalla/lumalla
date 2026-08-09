use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};

use anyhow::Context;
use ash::vk;
use log::{error, info, warn};
use lumalla_seat::SeatState;
use lumalla_shared::{DrmDeviceState, OutputConfig};
use mio::{Interest, Registry, Token, event::Source};

pub mod drm;
pub mod vulkan;

use crate::drm::{
    ConnectedOutput, DrmDevices, DrmDispatchResult, DrmFramebuffer, ModeBlob, atomic_modeset,
    atomic_set_plane_fb, resolve_connected_output,
};
use crate::vulkan::{
    DmaBufImage, Framebuffer, RenderPass, VulkanContext, clear_framebuffer_to_color,
    upload_bgra_to_image,
};

/// Default clear color for enabled outputs (teal).
pub const SOLID_CLEAR_COLOR: [f32; 4] = [0.0, 0.55, 0.65, 1.0];
const WL_SHM_FORMAT_ARGB8888: u32 = 0;
const WL_SHM_FORMAT_XRGB8888: u32 = 1;

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

struct OutputScanout {
    drm_path: PathBuf,
    output: ConnectedOutput,
    /// Owned so the CRTC's MODE_ID blob is not destroyed while still active.
    #[allow(dead_code)]
    mode_blob: ModeBlob,
    drm_fb: DrmFramebuffer,
    dma_image: DmaBufImage,
}

pub struct RendererState {
    // Drop order: scanouts → vulkan → drm_devices.
    drm_devices: DrmDevices,
    vulkan: Option<VulkanContext>,
    /// Configured render device (`None` = auto).
    render_device: Option<PathBuf>,
    /// Per-connector overrides; missing names use defaults (enabled if connected).
    output_configs: HashMap<String, OutputConfig>,
    scanouts: HashMap<String, OutputScanout>,
    /// Mapped surfaces in paint order (back to front).
    surface_frames: HashMap<(u32, u32), SurfaceFrame>,
    surface_order: Vec<(u32, u32)>,
}

impl RendererState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            drm_devices: DrmDevices::new()?,
            vulkan: None,
            render_device: None,
            output_configs: HashMap::new(),
            scanouts: HashMap::new(),
            surface_frames: HashMap::new(),
            surface_order: Vec::new(),
        })
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

    /// Replace or insert a surface frame and present the scene.
    pub fn set_surface_frame(&mut self, frame: SurfaceFrame) -> anyhow::Result<()> {
        frame.validate()?;
        let key = (frame.owner_id, frame.surface_id);
        if !self.surface_frames.contains_key(&key) {
            self.surface_order.push(key);
        }
        self.surface_frames.insert(key, frame);
        if self.drm_devices.opened().is_empty() {
            return Ok(());
        }
        self.present_enabled_outputs(SOLID_CLEAR_COLOR)
    }

    pub fn remove_surface_frame(&mut self, owner_id: u32, surface_id: u32) {
        let key = (owner_id, surface_id);
        if self.surface_frames.remove(&key).is_some() {
            self.surface_order.retain(|k| *k != key);
            if !self.drm_devices.opened().is_empty() {
                if let Err(error) = self.present_enabled_outputs(SOLID_CLEAR_COLOR) {
                    error!("Failed to clear removed Wayland surface: {error:#}");
                }
            }
        }
    }

    pub fn remove_client_frames(&mut self, owner_id: u32) {
        let before = self.surface_frames.len();
        self.surface_frames.retain(|(owner, _), _| *owner != owner_id);
        self.surface_order.retain(|(owner, _)| *owner != owner_id);
        if self.surface_frames.len() != before && !self.drm_devices.opened().is_empty() {
            if let Err(error) = self.present_enabled_outputs(SOLID_CLEAR_COLOR) {
                error!("Failed to clear disconnected Wayland surface: {error:#}");
            }
        }
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
        self.scanouts.clear();
        self.drm_devices.deactivate(seat);
    }

    /// Close removed / open newly discovered DRM devices while the seat is active.
    pub fn reconcile_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        self.scanouts.clear();
        self.drm_devices.reconcile(seat)
    }

    /// Select the Vulkan render device (`None` = auto). Re-presents if the seat is active.
    pub fn set_render_device(&mut self, path: Option<PathBuf>) -> anyhow::Result<()> {
        info!("Render device config: {path:?}");
        self.render_device = path;
        if !self.drm_devices.opened().is_empty() {
            self.present_enabled_outputs(SOLID_CLEAR_COLOR)?;
        }
        Ok(())
    }

    /// Merge per-connector output config. Re-presents if the seat is active.
    pub fn set_output_configs(&mut self, configs: Vec<OutputConfig>) -> anyhow::Result<()> {
        for config in configs {
            info!(
                "Output config: {} enabled={} mode={:?}",
                config.name, config.enabled, config.mode_name
            );
            self.output_configs.insert(config.name.clone(), config);
        }
        if !self.drm_devices.opened().is_empty() {
            self.present_enabled_outputs(SOLID_CLEAR_COLOR)?;
        }
        Ok(())
    }

    /// Present a solid clear on every enabled connected output (any card).
    ///
    /// Buffers are allocated on the selected render GPU and imported on each
    /// output's DRM card (same- or cross-device). Failures are logged per output.
    ///
    /// Active scanouts are retained across presents: the previous FB / mode blob
    /// stay alive until the new commit succeeds, and unchanged modes use a
    /// plane-only update instead of a full modeset.
    pub fn present_enabled_outputs(&mut self, color: [f32; 4]) -> anyhow::Result<()> {
        let Some(render_path) = self.resolved_render_device_path() else {
            warn!("No render device available; skipping presentation");
            return Ok(());
        };

        info!("Using render device {}", render_path.display());
        self.ensure_vulkan(&render_path)?;

        let targets = self.collect_present_targets();
        if targets.is_empty() {
            warn!("No enabled connected outputs to present");
            self.scanouts.clear();
            return Ok(());
        }

        let keep: HashSet<String> = targets.iter().map(|t| t.connector_name.clone()).collect();
        let mut presented = 0usize;
        for target in targets {
            match self.present_one_output(&target, color) {
                Ok(()) => {
                    if let Some(scanout) = self.scanouts.get(&target.connector_name) {
                        info!(
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

        self.scanouts.retain(|name, _| keep.contains(name));
        info!("Presented {presented} output(s)");
        Ok(())
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

    fn present_one_output(
        &mut self,
        target: &PresentTarget,
        color: [f32; 4],
    ) -> anyhow::Result<()> {
        let width = target.output.mode.width();
        let height = target.output.mode.height();
        let format = vk::Format::B8G8R8A8_UNORM;

        let (dma_image, fourcc) = {
            let vulkan = self
                .vulkan
                .as_ref()
                .context("VulkanContext missing during present")?;

            let dma_image = DmaBufImage::allocate(
                vulkan.device(),
                vulkan.physical_device(),
                width,
                height,
                format,
            )
            .context("Failed to allocate exportable scanout image")?;

            let fourcc = dma_image
                .drm_fourcc()
                .context("Vulkan format has no DRM fourcc mapping")?;

            let render_pass = RenderPass::new_for_scanout(vulkan.device(), format)?;
            let framebuffer = Framebuffer::from_view(
                vulkan.device(),
                &render_pass,
                dma_image.view(),
                dma_image.extent(),
            )?;

            clear_framebuffer_to_color(
                vulkan.device(),
                vulkan.graphics_command_pool(),
                &render_pass,
                &framebuffer,
                color,
            )
            .context("Failed to clear scanout image")?;

            let frames: Vec<&SurfaceFrame> = self
                .surface_order
                .iter()
                .filter_map(|key| self.surface_frames.get(key))
                .collect();
            if !frames.is_empty() {
                let upload = composite_surface_upload(&frames, width, height, color)?;
                upload_bgra_to_image(
                    vulkan.device(),
                    vulkan.physical_device(),
                    vulkan.graphics_command_pool(),
                    &dma_image,
                    &upload.pixels,
                    upload.width,
                    upload.height,
                )
                .context("Failed to upload Wayland SHM scene")?;
            }

            vulkan.device().wait_idle()?;
            (dma_image, fourcc)
        };

        let dma_buf = dma_image
            .export_dma_buf()
            .context("Failed to export DMA-BUF for scanout")?;

        let drm_device = self
            .drm_devices
            .opened()
            .get(&target.drm_path)
            .with_context(|| {
                format!("DRM device {} is no longer open", target.drm_path.display())
            })?;

        let drm_fb = DrmFramebuffer::from_dma_buf(
            drm_device.fd(),
            dma_buf.as_fd(),
            width,
            height,
            dma_image.stride(),
            dma_image.offset(),
            dma_image.modifier(),
            fourcc,
        )
        .context("Failed to import DMA-BUF as DRM framebuffer")?;

        let reuse_mode = self.scanouts.get(&target.connector_name).is_some_and(|prev| {
            prev.drm_path == target.drm_path
                && prev.output.connector_id == target.output.connector_id
                && prev.output.crtc_id == target.output.crtc_id
                && prev.output.plane_id == target.output.plane_id
                && prev.output.mode == target.output.mode
        });

        if reuse_mode {
            match atomic_set_plane_fb(drm_device.fd(), &target.output, drm_fb.id()) {
                Ok(()) => {
                    let mut prev = self
                        .scanouts
                        .remove(&target.connector_name)
                        .expect("reuse_mode requires an existing scanout");
                    prev.drm_path = target.drm_path.clone();
                    prev.output = target.output.clone();
                    // Replace FB/image only after the plane update succeeded so the
                    // CRTC never loses its active framebuffer.
                    prev.drm_fb = drm_fb;
                    prev.dma_image = dma_image;
                    self.scanouts
                        .insert(target.connector_name.clone(), prev);
                    return Ok(());
                }
                Err(err) => {
                    warn!(
                        "Plane FB update failed on {}: {err:#}; falling back to modeset",
                        target.connector_name
                    );
                }
            }
        }

        let mode_blob = ModeBlob::create(drm_device.fd(), &target.output.mode)
            .context("Failed to create MODE_ID property blob")?;

        atomic_modeset(drm_device.fd(), &target.output, mode_blob.id(), drm_fb.id())
            .context("Failed atomic modeset")?;

        // Only drop the previous scanout after the new modeset is active.
        let _previous = self.scanouts.insert(
            target.connector_name.clone(),
            OutputScanout {
                drm_path: target.drm_path.clone(),
                output: target.output.clone(),
                mode_blob,
                drm_fb,
                dma_image,
            },
        );
        Ok(())
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
            self.scanouts.clear();
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
}
