//! One-shot CPU upload into a Vulkan scanout image.

use std::ptr;

use anyhow::Context;
use ash::vk;

use super::{CommandBufferRecorder, CommandPool, Device, DmaBufImage, Fence, PhysicalDevice};

pub fn upload_bgra_to_image(
    device: &Device,
    physical_device: &PhysicalDevice,
    command_pool: &CommandPool,
    image: &DmaBufImage,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        width > 0 && height > 0,
        "Upload dimensions must be non-zero"
    );
    anyhow::ensure!(
        width <= image.extent().width && height <= image.extent().height,
        "Upload exceeds destination image"
    );
    let required_size = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|size| size.checked_mul(4))
        .context("Upload size overflows")?;
    anyhow::ensure!(
        pixels.len() as u64 >= required_size,
        "Upload pixel data is truncated"
    );

    let staging = StagingBuffer::new(device, physical_device, &pixels[..required_size as usize])?;
    let command_buffer = command_pool
        .allocate_command_buffer(device)
        .context("Failed to allocate upload command buffer")?;

    let record_result = (|| -> anyhow::Result<()> {
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

        let copy = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D::default())
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        unsafe {
            device.handle().cmd_copy_buffer_to_image(
                recorder.command_buffer(),
                staging.buffer,
                image.image(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy],
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
    })();
    if let Err(error) = record_result {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }

    let fence = match Fence::new(device, false) {
        Ok(fence) => fence,
        Err(error) => {
            command_pool.free_command_buffers(device, &[command_buffer]);
            return Err(error);
        }
    };
    if let Err(error) = device.submit_graphics(&[command_buffer], &[], &[], &[], fence.handle()) {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    if let Err(error) = fence
        .wait_default()
        .context("Timed out waiting for SHM upload to complete")
    {
        // Do not release staging memory while submitted work may still reference it.
        let _ = device.wait_idle();
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    command_pool.free_command_buffers(device, &[command_buffer]);
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

struct StagingBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    device: ash::Device,
}

impl StagingBuffer {
    fn new(
        device: &Device,
        physical_device: &PhysicalDevice,
        bytes: &[u8],
    ) -> anyhow::Result<Self> {
        let size = bytes.len() as vk::DeviceSize;
        let buffer_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.handle().create_buffer(&buffer_info, None) }
            .context("Failed to create SHM staging buffer")?;
        let requirements = unsafe { device.handle().get_buffer_memory_requirements(buffer) };
        let Some((memory_type_index, coherent)) = find_host_memory_type(
            physical_device.memory_properties(),
            requirements.memory_type_bits,
        ) else {
            unsafe {
                device.handle().destroy_buffer(buffer, None);
            }
            anyhow::bail!("No host-visible Vulkan memory for SHM staging");
        };
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        let memory = match unsafe { device.handle().allocate_memory(&allocate_info, None) } {
            Ok(memory) => memory,
            Err(error) => {
                unsafe {
                    device.handle().destroy_buffer(buffer, None);
                }
                return Err(error).context("Failed to allocate SHM staging memory");
            }
        };
        if let Err(error) = unsafe { device.handle().bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                device.handle().free_memory(memory, None);
                device.handle().destroy_buffer(buffer, None);
            }
            return Err(error).context("Failed to bind SHM staging memory");
        }

        let mapped = match unsafe {
            device
                .handle()
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        } {
            Ok(mapped) => mapped,
            Err(error) => {
                unsafe {
                    device.handle().free_memory(memory, None);
                    device.handle().destroy_buffer(buffer, None);
                }
                return Err(error).context("Failed to map SHM staging memory");
            }
        };
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast(), bytes.len());
        }
        let flush_result = if !coherent {
            let range = vk::MappedMemoryRange::default()
                .memory(memory)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            unsafe { device.handle().flush_mapped_memory_ranges(&[range]) }
                .context("Failed to flush SHM staging memory")
        } else {
            Ok(())
        };
        unsafe {
            device.handle().unmap_memory(memory);
        }
        if let Err(error) = flush_result {
            unsafe {
                device.handle().free_memory(memory, None);
                device.handle().destroy_buffer(buffer, None);
            }
            return Err(error);
        }

        Ok(Self {
            buffer,
            memory,
            device: device.handle().clone(),
        })
    }
}

impl Drop for StagingBuffer {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

fn find_host_memory_type(
    properties: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
) -> Option<(u32, bool)> {
    let mut host_visible = None;
    for index in 0..properties.memory_type_count {
        if type_bits & (1 << index) == 0 {
            continue;
        }
        let flags = properties.memory_types[index as usize].property_flags;
        if !flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
            continue;
        }
        let coherent = flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT);
        if coherent {
            return Some((index, true));
        }
        host_visible = Some((index, false));
    }
    host_visible
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vulkan::{Framebuffer, RenderPass, VulkanContext, clear_framebuffer_to_color};

    #[test]
    #[ignore = "requires a Vulkan GPU with DMA-BUF export support"]
    fn hardware_uploads_bgra_to_exportable_image() {
        let vulkan = VulkanContext::new(None).unwrap();
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
            [0.0, 0.0, 0.0, 1.0],
        )
        .unwrap();

        let pixels = vec![0x7f; 16 * 16 * 4];
        upload_bgra_to_image(
            vulkan.device(),
            vulkan.physical_device(),
            vulkan.graphics_command_pool(),
            &image,
            &pixels,
            16,
            16,
        )
        .unwrap();
        image.export_dma_buf().unwrap();
    }
}
