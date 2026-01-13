//! GBM buffer allocation

use std::os::fd::OwnedFd;

use anyhow::Context;
use drm::buffer::DrmFourcc;
use drm::control::{framebuffer, Device as ControlDevice};
use gbm::{BufferObject, BufferObjectFlags, Device as GbmDevice};
use log::{debug, info};

use super::DrmDevice;

/// GBM allocator for creating scanout-capable buffers.
pub struct GbmAllocator {
    device: GbmDevice<DrmDevice>,
}

impl GbmAllocator {
    /// Creates a new GBM allocator from a DRM device.
    pub fn new(drm_device: DrmDevice) -> anyhow::Result<Self> {
        let device = GbmDevice::new(drm_device).context("Failed to create GBM device")?;

        info!("GBM allocator created");

        Ok(Self { device })
    }

    /// Creates a scanout buffer with the given dimensions and format.
    ///
    /// The buffer will be suitable for direct display scanout.
    pub fn create_buffer(
        &self,
        width: u32,
        height: u32,
        format: DrmFourcc,
    ) -> anyhow::Result<GbmBuffer> {
        let bo = self
            .device
            .create_buffer_object::<()>(
                width,
                height,
                format,
                BufferObjectFlags::SCANOUT | BufferObjectFlags::RENDERING,
            )
            .context("Failed to create GBM buffer object")?;

        debug!("Created GBM buffer: {}x{} {:?}", width, height, format);

        Ok(GbmBuffer { bo })
    }

    /// Creates multiple buffers for double/triple buffering.
    pub fn create_buffers(
        &self,
        count: usize,
        width: u32,
        height: u32,
        format: DrmFourcc,
    ) -> anyhow::Result<Vec<GbmBuffer>> {
        let mut buffers = Vec::with_capacity(count);

        for i in 0..count {
            let buffer = self.create_buffer(width, height, format)?;
            debug!("Created buffer {} of {}", i + 1, count);
            buffers.push(buffer);
        }

        info!("Created {} GBM buffers: {}x{}", count, width, height);

        Ok(buffers)
    }

    /// Returns a reference to the underlying GBM device.
    pub fn device(&self) -> &GbmDevice<DrmDevice> {
        &self.device
    }
}

/// A GBM buffer object that can be used for rendering and scanout.
pub struct GbmBuffer {
    bo: BufferObject<()>,
}

impl GbmBuffer {
    /// Returns the buffer width.
    pub fn width(&self) -> anyhow::Result<u32> {
        self.bo.width().context("GBM device was destroyed")
    }

    /// Returns the buffer height.
    pub fn height(&self) -> anyhow::Result<u32> {
        self.bo.height().context("GBM device was destroyed")
    }

    /// Returns the buffer format.
    pub fn format(&self) -> anyhow::Result<DrmFourcc> {
        self.bo.format().context("GBM device was destroyed")
    }

    /// Returns the buffer stride (bytes per row).
    pub fn stride(&self) -> anyhow::Result<u32> {
        self.bo.stride().context("GBM device was destroyed")
    }

    /// Returns the number of planes in this buffer.
    pub fn plane_count(&self) -> anyhow::Result<u32> {
        self.bo.plane_count().context("GBM device was destroyed")
    }

    /// Returns the stride for a specific plane.
    pub fn stride_for_plane(&self, plane: i32) -> anyhow::Result<u32> {
        self.bo
            .stride_for_plane(plane)
            .context("GBM device was destroyed")
    }

    /// Returns the offset for a specific plane.
    pub fn offset(&self, plane: i32) -> anyhow::Result<u32> {
        self.bo.offset(plane).context("GBM device was destroyed")
    }

    /// Returns the DRM modifier for this buffer.
    pub fn modifier(&self) -> anyhow::Result<drm::buffer::DrmModifier> {
        self.bo.modifier().context("GBM device was destroyed")
    }

    /// Returns the buffer handle.
    pub fn handle(&self) -> anyhow::Result<gbm::BufferObjectHandle> {
        self.bo.handle().context("GBM device was destroyed")
    }

    /// Exports this buffer as a DMA-BUF file descriptor.
    ///
    /// The returned fd can be imported into Vulkan.
    pub fn export_dma_buf(&self) -> anyhow::Result<OwnedFd> {
        self.bo.fd().context("Failed to export GBM buffer as DMA-BUF")
    }

    /// Exports the DMA-BUF fd for a specific plane.
    pub fn export_dma_buf_for_plane(&self, plane: i32) -> anyhow::Result<OwnedFd> {
        self.bo
            .fd_for_plane(plane)
            .context("Failed to export GBM buffer plane as DMA-BUF")
    }

    /// Creates a DRM framebuffer from this buffer.
    ///
    /// This allows the buffer to be scanned out to a display.
    pub fn create_framebuffer(&self, device: &DrmDevice) -> anyhow::Result<framebuffer::Handle> {
        let modifier = self.modifier()?;

        // Check if modifier is valid (not INVALID)
        let _use_modifiers = modifier != drm::buffer::DrmModifier::Invalid
            && modifier != drm::buffer::DrmModifier::Linear;

        // Use the planar framebuffer API
        // The drm-rs crate handles modifiers internally
        device
            .add_planar_framebuffer(&self.bo, drm::control::FbCmd2Flags::MODIFIERS)
            .context("Failed to create framebuffer")
    }

    /// Returns a reference to the underlying buffer object.
    pub fn bo(&self) -> &BufferObject<()> {
        &self.bo
    }
}

/// Common buffer formats for compositing.
pub mod formats {
    use drm::buffer::DrmFourcc;

    /// XRGB8888 - 32-bit RGB, no alpha (X is padding)
    pub const XRGB8888: DrmFourcc = DrmFourcc::Xrgb8888;

    /// ARGB8888 - 32-bit RGBA with alpha
    pub const ARGB8888: DrmFourcc = DrmFourcc::Argb8888;

    /// XBGR8888 - 32-bit BGR, no alpha
    pub const XBGR8888: DrmFourcc = DrmFourcc::Xbgr8888;

    /// ABGR8888 - 32-bit BGRA with alpha
    pub const ABGR8888: DrmFourcc = DrmFourcc::Abgr8888;
}
