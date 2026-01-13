//! Framebuffer management

use anyhow::Context;
use ash::vk;
use log::debug;

use super::{Device, Image, RenderPass};

/// Represents a Vulkan framebuffer.
///
/// A framebuffer wraps one or more image views and defines the render target
/// for a render pass. It specifies which images will be written to during rendering.
pub struct Framebuffer {
    /// The Vulkan framebuffer handle
    handle: vk::Framebuffer,
    /// The extent (width, height) of the framebuffer
    extent: vk::Extent2D,
    /// The device that owns this framebuffer
    device: ash::Device,
}

impl Framebuffer {
    /// Creates a new framebuffer from a render pass and color attachment image.
    ///
    /// The framebuffer will use the provided image as the color attachment.
    /// The image's extent must match the render pass requirements.
    pub fn new(
        device: &Device,
        render_pass: &RenderPass,
        color_attachment: &Image,
    ) -> anyhow::Result<Self> {
        let extent = color_attachment.extent();
        let image_view = color_attachment.view();

        // Store array in variable to ensure it lives long enough
        let attachments = [image_view];

        // Create framebuffer
        let create_info = vk::FramebufferCreateInfo::default()
            .render_pass(render_pass.handle())
            .attachments(&attachments)
            .width(extent.width)
            .height(extent.height)
            .layers(1);

        let handle = unsafe { device.handle().create_framebuffer(&create_info, None) }
            .context("Failed to create framebuffer")?;

        debug!(
            "Created framebuffer: {}x{}",
            extent.width, extent.height
        );

        Ok(Self {
            handle,
            extent,
            device: device.handle().clone(),
        })
    }

    /// Returns the framebuffer handle.
    pub fn handle(&self) -> vk::Framebuffer {
        self.handle
    }

    /// Returns the framebuffer extent (width, height).
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_framebuffer(self.handle, None);
        }
        debug!("Destroyed framebuffer");
    }
}
