//! Render pass management

use anyhow::Context;
use ash::vk;
use log::debug;

use super::Device;

/// Represents a Vulkan render pass.
///
/// A render pass describes the structure of framebuffer attachments and
/// how they are used during rendering operations.
pub struct RenderPass {
    /// The Vulkan render pass handle
    handle: vk::RenderPass,
    /// The device that owns this render pass
    device: ash::Device,
}

impl RenderPass {
    /// Creates a simple render pass for a single color attachment.
    ///
    /// This is suitable for basic compositing operations where we render
    /// to a single color buffer (the final composited output).
    pub fn new_simple_color(device: &Device, format: vk::Format) -> anyhow::Result<Self> {
        // Color attachment description
        let color_attachment = vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR) // Clear the attachment at the start
            .store_op(vk::AttachmentStoreOp::STORE) // Store the attachment after rendering
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED) // Layout before render pass
            .final_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL); // Layout after render pass

        // Attachment reference (which attachment to use in the subpass)
        let color_attachment_ref = vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        };

        // Subpass dependency (synchronization between subpasses or external operations)
        // This ensures the render pass waits for any previous operations to complete
        let dependency = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);

        // Store arrays in variables to ensure they live long enough
        let attachments = [color_attachment];
        let color_attachment_refs = [color_attachment_ref];
        let dependencies = [dependency];

        // Subpass description
        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_attachment_refs);

        // Create render pass
        let subpasses = [subpass];
        let create_info = vk::RenderPassCreateInfo::default()
            .attachments(&attachments)
            .subpasses(&subpasses)
            .dependencies(&dependencies);

        let handle = unsafe { device.handle().create_render_pass(&create_info, None) }
            .context("Failed to create render pass")?;

        debug!("Created render pass for format {:?}", format);

        Ok(Self {
            handle,
            device: device.handle().clone(),
        })
    }

    /// Creates a render pass suitable for rendering to a display output.
    ///
    /// This uses PRESENT_SRC_KHR as the final layout, which is appropriate
    /// for images that will be presented directly to a display.
    pub fn new_for_display(device: &Device, format: vk::Format) -> anyhow::Result<Self> {
        // Color attachment description
        let color_attachment = vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::PRESENT_SRC_KHR); // For display presentation

        // Attachment reference
        let color_attachment_ref = vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        };

        // Subpass dependency
        let dependency = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);

        // Store arrays in variables to ensure they live long enough
        let attachments = [color_attachment];
        let color_attachment_refs = [color_attachment_ref];
        let dependencies = [dependency];

        // Subpass description
        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_attachment_refs);

        // Create render pass
        let subpasses = [subpass];
        let create_info = vk::RenderPassCreateInfo::default()
            .attachments(&attachments)
            .subpasses(&subpasses)
            .dependencies(&dependencies);

        let handle = unsafe { device.handle().create_render_pass(&create_info, None) }
            .context("Failed to create render pass for display")?;

        debug!("Created render pass for display with format {:?}", format);

        Ok(Self {
            handle,
            device: device.handle().clone(),
        })
    }

    /// Returns the render pass handle.
    pub fn handle(&self) -> vk::RenderPass {
        self.handle
    }
}

impl Drop for RenderPass {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_render_pass(self.handle, None);
        }
        debug!("Destroyed render pass");
    }
}
