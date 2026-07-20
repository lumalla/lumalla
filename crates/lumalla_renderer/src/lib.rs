use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use ash::vk;
use log::{info, warn};
use lumalla_seat::SeatState;
use lumalla_shared::DrmDeviceState;
use mio::{Interest, Registry, Token, event::Source, unix::SourceFd};

pub mod drm;
pub mod vulkan;

use crate::drm::{
    ConnectedOutput, DrmDevices, DrmDispatchResult, DrmFramebuffer, ModeBlob, atomic_modeset,
    atomic_page_flip, dispatch_drm_events, find_first_connected_output,
};
use crate::vulkan::{
    DmaBufImage, Framebuffer, RenderPass, VulkanContext, clear_framebuffer_to_color,
};

/// Primary clear color (teal).
pub const SOLID_CLEAR_COLOR: [f32; 4] = [0.0, 0.55, 0.65, 1.0];

/// Alternate clear color (coral), swapped in every second.
pub const ALTERNATE_CLEAR_COLOR: [f32; 4] = [0.85, 0.25, 0.35, 1.0];

const COLOR_CYCLE_PERIOD: Duration = Duration::from_secs(1);

struct ScanoutBuffer {
    drm_fb: DrmFramebuffer,
    dma_image: DmaBufImage,
}

/// Double-buffered atomic scanout with a one-second color cycle.
struct ActiveScanout {
    drm_path: PathBuf,
    output: ConnectedOutput,
    _mode_blob: ModeBlob,
    buffers: [ScanoutBuffer; 2],
    /// Index of the buffer currently on screen.
    front: usize,
    flip_pending: bool,
    /// Heap-stable flag set by the DRM page-flip handler (must outlive the commit).
    flip_done: Box<AtomicBool>,
    color_index: usize,
    next_color_at: Instant,
    /// Raw fd of the DRM card, registered for page-flip events.
    card_fd: std::os::fd::RawFd,
    page_flip_registered: bool,
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
    pub fn deactivate_drm(&mut self, seat: &SeatState, registry: Option<&Registry>) {
        if let Some(registry) = registry {
            let _ = self.deregister_page_flip_source(registry);
        }
        self.scanout = None;
        self.drm_devices.deactivate(seat);
    }

    /// Close removed / open newly discovered DRM devices while the seat is active.
    pub fn reconcile_drm(
        &mut self,
        seat: &SeatState,
        registry: Option<&Registry>,
    ) -> anyhow::Result<()> {
        if let Some(registry) = registry {
            let _ = self.deregister_page_flip_source(registry);
        }
        self.scanout = None;
        self.drm_devices.reconcile(seat)
    }

    /// Time until the next color-cycle flip should be attempted, if scanout is active.
    pub fn color_cycle_timeout(&self) -> Option<Duration> {
        let scanout = self.scanout.as_ref()?;
        if scanout.flip_pending {
            return None;
        }
        Some(
            scanout
                .next_color_at
                .saturating_duration_since(Instant::now()),
        )
    }

    /// Register the active DRM card fd for page-flip event notifications.
    pub fn register_page_flip_source(
        &mut self,
        registry: &Registry,
        token: Token,
    ) -> io::Result<()> {
        let Some(scanout) = self.scanout.as_mut() else {
            return Ok(());
        };
        if scanout.page_flip_registered {
            return Ok(());
        }
        let fd = scanout.card_fd;
        let mut source = SourceFd(&fd);
        source.register(registry, token, Interest::READABLE)?;
        scanout.page_flip_registered = true;
        Ok(())
    }

    /// Deregister the DRM card fd from the event loop.
    pub fn deregister_page_flip_source(&mut self, registry: &Registry) -> io::Result<()> {
        let Some(scanout) = self.scanout.as_mut() else {
            return Ok(());
        };
        if !scanout.page_flip_registered {
            return Ok(());
        }
        let fd = scanout.card_fd;
        let mut source = SourceFd(&fd);
        source.deregister(registry)?;
        scanout.page_flip_registered = false;
        Ok(())
    }

    /// Drain DRM page-flip events; when a flip completes, schedule the next color change.
    pub fn dispatch_page_flips(&mut self) -> anyhow::Result<()> {
        let Some(scanout) = self.scanout.as_ref() else {
            return Ok(());
        };
        let card_fd = scanout.card_fd;
        dispatch_drm_events(card_fd)?;

        let Some(scanout) = self.scanout.as_mut() else {
            return Ok(());
        };
        if !scanout.flip_done.swap(false, Ordering::SeqCst) {
            return Ok(());
        }

        scanout.front ^= 1;
        scanout.flip_pending = false;
        scanout.next_color_at = Instant::now() + COLOR_CYCLE_PERIOD;
        info!(
            "Page-flip complete: now showing buffer {} ({})",
            scanout.front,
            if scanout.color_index == 0 {
                "teal"
            } else {
                "coral"
            }
        );
        Ok(())
    }

    /// If a color change is due and no flip is pending, clear the back buffer and flip.
    pub fn tick_color_cycle(&mut self) -> anyhow::Result<()> {
        let Some(scanout) = &self.scanout else {
            return Ok(());
        };
        if scanout.flip_pending || Instant::now() < scanout.next_color_at {
            return Ok(());
        }

        let back = scanout.front ^ 1;
        let next_color_index = scanout.color_index ^ 1;
        let color = if next_color_index == 0 {
            SOLID_CLEAR_COLOR
        } else {
            ALTERNATE_CLEAR_COLOR
        };

        self.clear_buffer(back, color)?;

        let (drm_path, output, fb_id) = {
            let scanout = self.scanout.as_ref().unwrap();
            (
                scanout.drm_path.clone(),
                scanout.output.clone(),
                scanout.buffers[back].drm_fb.id(),
            )
        };

        let drm_device = self
            .drm_devices
            .opened()
            .get(&drm_path)
            .with_context(|| format!("DRM device {} is no longer open", drm_path.display()))?;

        let flip_done = self.scanout.as_ref().unwrap().flip_done.as_ref();
        atomic_page_flip(drm_device.fd(), &output, fb_id, flip_done)
            .context("Failed to schedule atomic page-flip")?;

        let scanout = self.scanout.as_mut().unwrap();
        scanout.flip_pending = true;
        scanout.color_index = next_color_index;
        info!(
            "Scheduled color flip to {} (FB {fb_id})",
            if next_color_index == 0 {
                "teal"
            } else {
                "coral"
            }
        );
        Ok(())
    }

    /// Clear both buffers, atomic-modeset the first, and start the one-second color cycle.
    pub fn present_solid_clear(
        &mut self,
        color: [f32; 4],
        registry: Option<&Registry>,
    ) -> anyhow::Result<()> {
        if let Some(registry) = registry {
            let _ = self.deregister_page_flip_source(registry);
        }
        if let Some(old) = self.scanout.take() {
            if old.flip_pending {
                let _ = dispatch_drm_events(old.card_fd);
            }
        }

        let (drm_path, output) = self
            .find_present_target()
            .context("No connected DRM output available for presentation")?;

        let width = output.mode.width();
        let height = output.mode.height();

        info!(
            "Presenting solid clear on {} / {} (CRTC {}, plane {}, {}x{}@{}Hz)",
            drm_path.display(),
            output.connector_name,
            output.crtc_id,
            output.plane_id,
            width,
            height,
            output.mode.refresh_hz()
        );

        self.ensure_vulkan(&drm_path)?;

        let format = vk::Format::B8G8R8A8_UNORM;
        let mut buffers = Vec::with_capacity(2);
        for i in 0..2 {
            let buffer_color = if i == 0 { color } else { ALTERNATE_CLEAR_COLOR };
            let buffer = self
                .allocate_scanout_buffer(&drm_path, width, height, format, buffer_color)
                .with_context(|| format!("Failed to allocate scanout buffer {i}"))?;
            buffers.push(buffer);
        }
        let buffers: [ScanoutBuffer; 2] = buffers
            .try_into()
            .unwrap_or_else(|_| unreachable!("exactly two buffers"));

        let drm_device = self
            .drm_devices
            .opened()
            .get(&drm_path)
            .with_context(|| format!("DRM device {} is no longer open", drm_path.display()))?;

        let mode_blob = ModeBlob::create(drm_device.fd(), &output.mode)
            .context("Failed to create MODE_ID property blob")?;

        atomic_modeset(
            drm_device.fd(),
            &output,
            mode_blob.id(),
            buffers[0].drm_fb.id(),
        )
        .context("Failed atomic modeset")?;

        let card_fd = drm_device.fd().as_raw_fd();

        info!(
            "Atomic scanout active: FB {} on {} plane {} ({}); color cycle every {:?}",
            buffers[0].drm_fb.id(),
            output.connector_name,
            output.plane_id,
            output.mode.name(),
            COLOR_CYCLE_PERIOD
        );

        self.scanout = Some(ActiveScanout {
            drm_path,
            output,
            _mode_blob: mode_blob,
            buffers,
            front: 0,
            flip_pending: false,
            flip_done: Box::new(AtomicBool::new(false)),
            color_index: 0,
            next_color_at: Instant::now() + COLOR_CYCLE_PERIOD,
            card_fd,
            page_flip_registered: false,
        });

        Ok(())
    }

    fn allocate_scanout_buffer(
        &mut self,
        drm_path: &std::path::Path,
        width: u32,
        height: u32,
        format: vk::Format,
        color: [f32; 4],
    ) -> anyhow::Result<ScanoutBuffer> {
        let (dma_image, fourcc) = {
            let vulkan = self
                .vulkan
                .as_ref()
                .expect("VulkanContext must exist after ensure_vulkan");

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
            .get(drm_path)
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

        Ok(ScanoutBuffer { drm_fb, dma_image })
    }

    fn clear_buffer(&mut self, index: usize, color: [f32; 4]) -> anyhow::Result<()> {
        let vulkan = self
            .vulkan
            .as_ref()
            .context("VulkanContext missing during buffer clear")?;
        let scanout = self
            .scanout
            .as_ref()
            .context("No active scanout during buffer clear")?;
        let dma_image = &scanout.buffers[index].dma_image;
        let format = dma_image.format();

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
        )?;
        vulkan.device().wait_idle()?;
        Ok(())
    }

    fn find_present_target(&self) -> anyhow::Result<(PathBuf, ConnectedOutput)> {
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
