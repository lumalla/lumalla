//! One-shot clear of a color attachment (no graphics pipeline).

use anyhow::Context;
use ash::vk;

use super::{
    CommandBufferRecorder, CommandPool, Device, Fence, Framebuffer, RenderPass,
};

/// Clear `framebuffer` to `color` (RGBA float) using a render pass with CLEAR load op.
///
/// Blocks until the GPU finishes. No pipeline is bound.
pub fn clear_framebuffer_to_color(
    device: &Device,
    command_pool: &CommandPool,
    render_pass: &RenderPass,
    framebuffer: &Framebuffer,
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
