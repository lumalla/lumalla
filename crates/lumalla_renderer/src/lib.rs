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
    resolve_connected_output,
};
use crate::vulkan::{
    DmaBufImage, Framebuffer, RenderPass, VulkanContext, clear_framebuffer_to_color,
};

/// Default clear color for enabled outputs (teal).
pub const SOLID_CLEAR_COLOR: [f32; 4] = [0.0, 0.55, 0.65, 1.0];

#[derive(Debug)]
pub struct SurfaceFrame {
    pub owner_id: u32,
    pub surface_id: u32,
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
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
        Ok(())
    }
}

struct OutputScanout {
    drm_path: PathBuf,
    output: ConnectedOutput,
    _mode_blob: ModeBlob,
    _drm_fb: DrmFramebuffer,
    _dma_image: DmaBufImage,
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
    active_surface_frame: Option<SurfaceFrame>,
}

impl RendererState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            drm_devices: DrmDevices::new()?,
            vulkan: None,
            render_device: None,
            output_configs: HashMap::new(),
            scanouts: HashMap::new(),
            active_surface_frame: None,
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

    /// Replace the current single-surface scene. Rendering is added by the
    /// SHM upload path; retaining the latest frame here makes commit handling
    /// independent from DRM lifecycle events.
    pub fn set_surface_frame(&mut self, frame: SurfaceFrame) -> anyhow::Result<()> {
        frame.validate()?;
        self.active_surface_frame = Some(frame);
        Ok(())
    }

    pub fn remove_surface_frame(&mut self, owner_id: u32, surface_id: u32) {
        if self
            .active_surface_frame
            .as_ref()
            .is_some_and(|frame| frame.owner_id == owner_id && frame.surface_id == surface_id)
        {
            self.active_surface_frame = None;
        }
    }

    pub fn remove_client_frames(&mut self, owner_id: u32) {
        if self
            .active_surface_frame
            .as_ref()
            .is_some_and(|frame| frame.owner_id == owner_id)
        {
            self.active_surface_frame = None;
        }
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
    pub fn present_enabled_outputs(&mut self, color: [f32; 4]) -> anyhow::Result<()> {
        self.scanouts.clear();

        let Some(render_path) = self.resolved_render_device_path() else {
            warn!("No render device available; skipping presentation");
            return Ok(());
        };

        info!("Using render device {}", render_path.display());
        self.ensure_vulkan(&render_path)?;

        let targets = self.collect_present_targets();
        if targets.is_empty() {
            warn!("No enabled connected outputs to present");
            return Ok(());
        }

        let mut presented = 0usize;
        for target in targets {
            match self.present_one_output(&target, color) {
                Ok(scanout) => {
                    info!(
                        "Presented {} on {} (CRTC {}, {}x{}@{}Hz)",
                        scanout.output.connector_name,
                        scanout.drm_path.display(),
                        scanout.output.crtc_id,
                        scanout.output.mode.width(),
                        scanout.output.mode.height(),
                        scanout.output.mode.refresh_hz()
                    );
                    self.scanouts
                        .insert(scanout.output.connector_name.clone(), scanout);
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
    ) -> anyhow::Result<OutputScanout> {
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

        let mode_blob = ModeBlob::create(drm_device.fd(), &target.output.mode)
            .context("Failed to create MODE_ID property blob")?;

        atomic_modeset(drm_device.fd(), &target.output, mode_blob.id(), drm_fb.id())
            .context("Failed atomic modeset")?;

        Ok(OutputScanout {
            drm_path: target.drm_path.clone(),
            output: target.output.clone(),
            _mode_blob: mode_blob,
            _drm_fb: drm_fb,
            _dma_image: dma_image,
        })
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
    }
}
