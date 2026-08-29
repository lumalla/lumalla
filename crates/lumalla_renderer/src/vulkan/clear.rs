//! One-shot clear of a color attachment (no graphics pipeline).

use anyhow::Context;
use ash::vk;

use super::{CommandBufferRecorder, CommandPool, Device, Fence, Framebuffer, RenderPass};

/// Clear `framebuffer` to `color` (RGBA float) using a render pass with CLEAR load op.
///
/// `image` must match the framebuffer's color attachment. Pass the layout the image
/// is currently in (`UNDEFINED` for freshly allocated images, `GENERAL` for reused
/// scanout buffers).
pub fn clear_framebuffer_to_color(
    device: &Device,
    command_pool: &CommandPool,
    render_pass: &RenderPass,
    framebuffer: &Framebuffer,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    color: [f32; 4],
) -> anyhow::Result<()> {
    let command_buffer = command_pool
        .allocate_command_buffer(device)
        .context("Failed to allocate clear command buffer")?;

    let clear_value = vk::ClearValue {
        color: vk::ClearColorValue { float32: color },
    };

    {
        let mut recorder = CommandBufferRecorder::begin_one_time(device, command_buffer)?;
        transition_for_render(device, recorder.command_buffer(), image, old_layout);
        recorder.begin_render_pass(render_pass, framebuffer, &[clear_value])?;
        recorder.end_render_pass();
        recorder.end()?;
    }

    let fence = Fence::new(device, false)?;
    device.submit_graphics(&[command_buffer], &[], &[], &[], fence.handle())?;
    fence
        .wait_default()
        .context("Timed out waiting for clear to complete")?;

    command_pool.free_command_buffers(device, &[command_buffer]);
    Ok(())
}

fn transition_for_render(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
    old_layout: vk::ImageLayout,
) {
    if old_layout == vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL {
        return;
    }
    let barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });
    unsafe {
        device.handle().cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
    }
}
