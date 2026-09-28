//! One-shot CPU upload into a Vulkan scanout image.

use anyhow::Context;
use ash::vk;

use super::{
    CommandBufferRecorder, DmaBufImage, Fence, StagingBuffer, VulkanContext,
};

pub fn upload_bgra_to_image(
    vulkan: &mut VulkanContext,
    image: &DmaBufImage,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    upload_bgra_regions_from_backing(
        vulkan,
        image,
        pixels,
        width,
        &[UploadRegion {
            x: 0,
            y: 0,
            width,
            height,
        }],
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Uploads one or more sub-rectangles from a full output backing store.
pub fn upload_bgra_regions_from_backing(
    vulkan: &mut VulkanContext,
    image: &DmaBufImage,
    backing: &[u8],
    backing_width: u32,
    regions: &[UploadRegion],
) -> anyhow::Result<()> {
    anyhow::ensure!(!regions.is_empty(), "Upload region list must be non-empty");
    anyhow::ensure!(backing_width > 0, "Backing width must be non-zero");

    let backing_row_bytes = usize::try_from(backing_width)
        .context("Backing width overflows")?
        .checked_mul(4)
        .context("Backing row size overflows")?;

    let mut staging_bytes = Vec::new();
    let mut copies = Vec::with_capacity(regions.len());
    for region in regions {
        anyhow::ensure!(
            region.width > 0 && region.height > 0,
            "Upload region dimensions must be non-zero"
        );
        let end_x = region
            .x
            .checked_add(region.width)
            .context("Upload region x overflow")?;
        let end_y = region
            .y
            .checked_add(region.height)
            .context("Upload region y overflow")?;
        anyhow::ensure!(
            end_x <= backing_width && end_y <= image.extent().height,
            "Upload region exceeds backing or destination image"
        );

        let region_row_bytes = usize::try_from(region.width)
            .context("Region width overflows")?
            .checked_mul(4)
            .context("Region row bytes overflow")?;
        let region_size = region_row_bytes
            .checked_mul(region.height as usize)
            .context("Region size overflows")?;
        let buffer_offset = staging_bytes.len() as u64;
        staging_bytes.reserve(region_size);

        for row in 0..region.height {
            let src_y = region.y + row;
            let src_start = src_y as usize * backing_row_bytes + region.x as usize * 4;
            let src_end = src_start + region_row_bytes;
            anyhow::ensure!(
                src_end <= backing.len(),
                "Upload region exceeds backing pixel data"
            );
            staging_bytes.extend_from_slice(&backing[src_start..src_end]);
        }

        copies.push(
            vk::BufferImageCopy::default()
                .buffer_offset(buffer_offset)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D {
                    x: region.x as i32,
                    y: region.y as i32,
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width: region.width,
                    height: region.height,
                    depth: 1,
                }),
        );
    }

    let staging = vulkan.acquire_staging(staging_bytes.len() as vk::DeviceSize)?;
    if let Err(error) = staging.write_slice(vulkan.device(), &staging_bytes) {
        vulkan.release_staging_many([staging]);
        return Err(error);
    }

    let command_buffer = {
        let device = vulkan.device();
        vulkan
            .graphics_command_pool()
            .allocate_command_buffer(device)
            .context("Failed to allocate upload command buffer")?
    };

    let record_result = record_upload(vulkan, image, &staging, &copies, command_buffer);
    if let Err(error) = record_result {
        vulkan.free_command_buffers(&[command_buffer]);
        vulkan.release_staging_many([staging]);
        return Err(error);
    }

    let fence = match Fence::new(vulkan.device(), false) {
        Ok(fence) => fence,
        Err(error) => {
            vulkan.free_command_buffers(&[command_buffer]);
            vulkan.release_staging_many([staging]);
            return Err(error);
        }
    };
    if let Err(error) =
        vulkan
            .device()
            .submit_graphics(&[command_buffer], &[], &[], &[], fence.handle())
    {
        vulkan.free_command_buffers(&[command_buffer]);
        vulkan.release_staging_many([staging]);
        return Err(error);
    }
    if let Err(error) = fence
        .wait_default()
        .context("Timed out waiting for SHM upload to complete")
    {
        // Do not release staging memory while submitted work may still reference it.
        let _ = vulkan.device().wait_idle();
        vulkan.free_command_buffers(&[command_buffer]);
        vulkan.release_staging_many([staging]);
        return Err(error);
    }
    vulkan.free_command_buffers(&[command_buffer]);
    vulkan.release_staging_many([staging]);
    Ok(())
}

fn record_upload(
    vulkan: &VulkanContext,
    image: &DmaBufImage,
    staging: &StagingBuffer,
    copies: &[vk::BufferImageCopy],
    command_buffer: vk::CommandBuffer,
) -> anyhow::Result<()> {
    let device = vulkan.device();
    let recorder = CommandBufferRecorder::begin_one_time(device, command_buffer)?;
    let to_transfer = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image.image())
        .subresource_range(color_subresource_range());
    unsafe {
        device.handle().cmd_pipeline_barrier(
            recorder.command_buffer(),
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_transfer],
        );
    }

    unsafe {
        device.handle().cmd_copy_buffer_to_image(
            recorder.command_buffer(),
            staging.buffer(),
            image.image(),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            copies,
        );
    }

    let to_general = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ)
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image.image())
        .subresource_range(color_subresource_range());
    unsafe {
        device.handle().cmd_pipeline_barrier(
            recorder.command_buffer(),
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_general],
        );
    }
    recorder.end()?;
    Ok(())
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vulkan::{Framebuffer, RenderPass, clear_framebuffer_to_color};

    #[test]
    #[ignore = "requires a Vulkan GPU with DMA-BUF export support"]
    fn hardware_uploads_bgra_to_exportable_image() {
        let mut vulkan = VulkanContext::new(None).unwrap();
        let image = DmaBufImage::allocate(
            vulkan.device(),
            vulkan.physical_device(),
            16,
            16,
            vk::Format::B8G8R8A8_UNORM,
        )
        .unwrap();
        let render_pass =
            RenderPass::new_for_scanout(vulkan.device(), vk::Format::B8G8R8A8_UNORM).unwrap();
        let framebuffer =
            Framebuffer::from_view(vulkan.device(), &render_pass, image.view(), image.extent())
                .unwrap();
        clear_framebuffer_to_color(
            vulkan.device(),
            vulkan.graphics_command_pool(),
            &render_pass,
            &framebuffer,
            image.image(),
            vk::ImageLayout::UNDEFINED,
            [0.0, 0.0, 0.0, 1.0],
        )
        .unwrap();

        let pixels = vec![0x7f; 16 * 16 * 4];
        upload_bgra_to_image(&mut vulkan, &image, &pixels, 16, 16).unwrap();
        image.export_dma_buf().unwrap();
    }
}
