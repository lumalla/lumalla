use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::path::PathBuf;

use anyhow::Context;
use ash::vk;
use log::{info, warn};
use lumalla_seat::SeatState;
use lumalla_shared::DrmDeviceState;
use mio::{Interest, Registry, Token, event::Source};

pub mod drm;
pub mod vulkan;

use crate::drm::{
    DrmDevices, DrmDispatchResult, DrmFramebuffer, find_first_connected_output, set_crtc,
};
use crate::vulkan::{
    DmaBufImage, Framebuffer, RenderPass, VulkanContext, clear_framebuffer_to_color,
};

/// Default clear color for the first on-screen frame (teal).
pub const SOLID_CLEAR_COLOR: [f32; 4] = [0.0, 0.55, 0.65, 1.0];

/// Scanout resources for the currently presented frame.
///
/// Drop order: DRM FB first (uses the card fd), then the Vulkan image.
struct ActiveScanout {
    _drm_fb: DrmFramebuffer,
    _dma_image: DmaBufImage,
}

pub struct RendererState {
    // Drop order is reverse of declaration: scanout → vulkan → drm_devices.
    drm_devices: DrmDevices,
    vulkan: Option<VulkanContext>,
    scanout: Option<ActiveScanout>,
}

impl RendererState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            drm_devices: DrmDevices::new()?,
            vulkan: None,
            scanout: None,
        })
    }

    /// Snapshot of discovered DRM devices and probed connectors.
    pub fn drm_device_states(&self) -> Vec<DrmDeviceState> {
        self.drm_devices.device_states()
    }

    /// Drain pending udev DRM events; update device paths and/or connectors.
    pub fn dispatch(&mut self) -> anyhow::Result<DrmDispatchResult> {
        self.drm_devices.dispatch()
    }

    /// Open missing DRM devices via the seat (fresh open after VT resume).
    pub fn activate_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        self.drm_devices.activate(seat)
    }

    /// Close seat-opened DRM devices after session disable was acknowledged.
    pub fn deactivate_drm(&mut self, seat: &SeatState) {
        // Tear down KMS FB before closing the DRM fd.
        self.scanout = None;
        self.drm_devices.deactivate(seat);
    }

    /// Close removed / open newly discovered DRM devices while the seat is active.
    pub fn reconcile_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        // Drop FBs that may reference devices about to be closed.
        self.scanout = None;
        self.drm_devices.reconcile(seat)
    }

    /// Clear the first connected output to `color` and modeset it on screen.
    ///
    /// Allocates a Vulkan DMA-BUF image, clears it, imports it as a DRM FB, and
    /// calls `drmModeSetCrtc`. Intended for the first on-screen frame after seat enable.
    pub fn present_solid_clear(&mut self, color: [f32; 4]) -> anyhow::Result<()> {
        let (drm_path, output) = self
            .find_present_target()
            .context("No connected DRM output available for presentation")?;

        let width = output.mode.width();
        let height = output.mode.height();

        info!(
            "Presenting solid clear on {} / {} (CRTC {}, {}x{}@{}Hz)",
            drm_path.display(),
            output.connector_name,
            output.crtc_id,
            width,
            height,
            output.mode.refresh_hz()
        );

        self.ensure_vulkan(&drm_path)?;

        let (dma_image, fourcc) = {
            let vulkan = self
                .vulkan
                .as_ref()
                .expect("VulkanContext must exist after ensure_vulkan");

            let format = vk::Format::B8G8R8A8_UNORM;
            let dma_image =
                DmaBufImage::allocate(vulkan.device(), vulkan.physical_device(), width, height, format)
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

            // Ensure GPU work is done before KMS imports the DMA-BUF.
            vulkan.device().wait_idle()?;

            (dma_image, fourcc)
        };

        let dma_buf = dma_image
            .export_dma_buf()
            .context("Failed to export DMA-BUF for scanout")?;

        let drm_device = self
            .drm_devices
            .opened()
            .get(&drm_path)
            .with_context(|| format!("DRM device {} is no longer open", drm_path.display()))?;

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

        set_crtc(
            drm_device.fd(),
            output.crtc_id,
            drm_fb.id(),
            output.connector_id,
            &output.mode,
        )
        .context("Failed to set CRTC mode")?;

        info!(
            "Scanout active: FB {} on {} ({})",
            drm_fb.id(),
            output.connector_name,
            output.mode.name()
        );

        self.scanout = Some(ActiveScanout {
            _drm_fb: drm_fb,
            _dma_image: dma_image,
        });

        Ok(())
    }

    fn find_present_target(&self) -> anyhow::Result<(PathBuf, crate::drm::ConnectedOutput)> {
        for (path, device) in self.drm_devices.opened() {
            match find_first_connected_output(device.fd().as_raw_fd()) {
                Ok(Some(output)) => return Ok((path.clone(), output)),
                Ok(None) => {}
                Err(err) => {
                    warn!(
                        "Failed to find connected output on {}: {err:#}",
                        path.display()
                    );
                }
            }
        }
        anyhow::bail!("No opened DRM device has a connected connector with a usable CRTC");
    }

    fn ensure_vulkan(&mut self, preferred_drm_path: &std::path::Path) -> anyhow::Result<()> {
        let needs_recreate = match &self.vulkan {
            None => true,
            Some(vk) => match vk.drm_device_path() {
                Some(selected) => selected != preferred_drm_path,
                None => false,
            },
        };

        if needs_recreate {
            self.scanout = None;
            info!(
                "Initializing Vulkan for DRM device {}",
                preferred_drm_path.display()
            );
            self.vulkan = Some(VulkanContext::new(Some(preferred_drm_path))?);
        }

        Ok(())
    }
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
