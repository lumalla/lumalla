//! GPU → host image readback for screenshots.

use std::ptr;

use anyhow::Context;
use ash::vk;

use super::{
    CommandBufferRecorder, Device, DmaBufImage, Fence, PhysicalDevice, VulkanContext,
};

/// Downloads a rectangular region from a scanout image as tightly packed BGRA8 bytes.
pub fn download_bgra_region(
    vulkan: &VulkanContext,
    image: &DmaBufImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(width > 0 && height > 0, "download region must be non-empty");
    let extent = image.extent();
    anyhow::ensure!(
        x.saturating_add(width) <= extent.width && y.saturating_add(height) <= extent.height,
        "download region ({x},{y},{width}x{height}) exceeds image {}x{}",
        extent.width,
        extent.height
    );

    let device = vulkan.device();
    let physical_device = vulkan.physical_device();
    let command_pool = vulkan.graphics_command_pool();
    let byte_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .context("download size overflow")?;

    let staging = DownloadStagingBuffer::new(device, physical_device, byte_len as vk::DeviceSize)?;
    let command_buffer = command_pool.allocate_command_buffer(device)?;

    let record_result = (|| -> anyhow::Result<()> {
        let recorder = CommandBufferRecorder::begin_one_time(device, command_buffer)?;
        let cb = recorder.command_buffer();

        let to_src = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image.image())
            .subresource_range(color_subresource_range());
        unsafe {
            device.handle().cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );
        }

        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D {
                x: x as i32,
                y: y as i32,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        unsafe {
            device.handle().cmd_copy_image_to_buffer(
                cb,
                image.image(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                staging.buffer,
                &[region],
            );
        }

        let back = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image.image())
            .subresource_range(color_subresource_range());
        unsafe {
            device.handle().cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[back],
            );
        }

        recorder.end()?;
        Ok(())
    })();

    if let Err(error) = record_result {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }

    let fence = Fence::new(device, false).context("Failed to create download fence")?;
    if let Err(error) = device.submit_graphics(&[command_buffer], &[], &[], &[], fence.handle()) {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error).context("Failed to submit image download");
    }
    if let Err(error) = fence
        .wait_default()
        .context("Timed out waiting for image download")
    {
        let _ = device.wait_idle();
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    command_pool.free_command_buffers(device, &[command_buffer]);

    staging.read_bytes(byte_len)
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

struct DownloadStagingBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: vk::DeviceSize,
    coherent: bool,
    device: ash::Device,
}

impl DownloadStagingBuffer {
    fn new(
        device: &Device,
        physical_device: &PhysicalDevice,
        size: vk::DeviceSize,
    ) -> anyhow::Result<Self> {
        let buffer_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.handle().create_buffer(&buffer_info, None) }
            .context("Failed to create download staging buffer")?;
        let requirements = unsafe { device.handle().get_buffer_memory_requirements(buffer) };
        let Some((memory_type_index, coherent)) = find_host_memory_type(
            physical_device.memory_properties(),
            requirements.memory_type_bits,
        ) else {
            unsafe {
                device.handle().destroy_buffer(buffer, None);
            }
            anyhow::bail!("No host-visible Vulkan memory for download staging");
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
                return Err(error).context("Failed to allocate download staging memory");
            }
        };
        if let Err(error) = unsafe { device.handle().bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                device.handle().free_memory(memory, None);
                device.handle().destroy_buffer(buffer, None);
            }
            return Err(error).context("Failed to bind download staging memory");
        }

        Ok(Self {
            buffer,
            memory,
            size,
            coherent,
            device: device.handle().clone(),
        })
    }

    fn read_bytes(&self, len: usize) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(len as vk::DeviceSize <= self.size, "download read exceeds staging size");
        if !self.coherent {
            let range = vk::MappedMemoryRange::default()
                .memory(self.memory)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            unsafe { self.device.invalidate_mapped_memory_ranges(&[range]) }
                .context("Failed to invalidate download staging memory")?;
        }
        let mapped = unsafe {
            self.device
                .map_memory(self.memory, 0, self.size, vk::MemoryMapFlags::empty())
        }
        .context("Failed to map download staging memory")?;
        let mut bytes = vec![0u8; len];
        unsafe {
            ptr::copy_nonoverlapping(mapped.cast(), bytes.as_mut_ptr(), len);
            self.device.unmap_memory(self.memory);
        }
        Ok(bytes)
    }
}

impl Drop for DownloadStagingBuffer {
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
