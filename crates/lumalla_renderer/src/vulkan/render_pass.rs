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
    /// for images that will be presented directly via WSI.
    pub fn new_for_display(device: &Device, format: vk::Format) -> anyhow::Result<Self> {
        Self::new_with_final_layout(device, format, vk::ImageLayout::PRESENT_SRC_KHR)
    }

    /// Creates a render pass for clearing a DMA-BUF image destined for KMS scanout.
    ///
    /// Final layout is `GENERAL` so the image can be exported and scanned out.
    pub fn new_for_scanout(device: &Device, format: vk::Format) -> anyhow::Result<Self> {
        Self::new_with_final_layout(device, format, vk::ImageLayout::GENERAL)
    }

    fn new_with_final_layout(
        device: &Device,
        format: vk::Format,
        final_layout: vk::ImageLayout,
    ) -> anyhow::Result<Self> {
        let color_attachment = vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(final_layout);

        let color_attachment_ref = vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        };

        // External dependency after the pass so KMS can sample the stored image.
        let dependency_begin = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);

        let dependency_end = vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags::BOTTOM_OF_PIPE)
            .dst_access_mask(vk::AccessFlags::empty());

        let attachments = [color_attachment];
        let color_attachment_refs = [color_attachment_ref];
        let dependencies = [dependency_begin, dependency_end];

        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_attachment_refs);

        let subpasses = [subpass];
        let create_info = vk::RenderPassCreateInfo::default()
            .attachments(&attachments)
            .subpasses(&subpasses)
            .dependencies(&dependencies);

        let handle = unsafe { device.handle().create_render_pass(&create_info, None) }
            .context("Failed to create render pass")?;

        debug!(
            "Created render pass with format {:?} final_layout={:?}",
            format, final_layout
        );

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
