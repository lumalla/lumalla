//! Reusable host-visible staging buffers for CPU→GPU uploads.

use std::ptr;

use anyhow::Context;
use ash::vk;

use super::{Device, PhysicalDevice};
use crate::scene_backing::UploadRect;

/// Minimum capacity for a pooled staging allocation (avoids tiny buffer churn).
const MIN_STAGING_CAPACITY: vk::DeviceSize = 64 * 1024;
/// Cap free-list length so mode changes / large uploads cannot retain unbounded GPU memory.
const MAX_FREE_STAGING_BUFFERS: usize = 16;

/// Host-visible transfer-src buffer retained by a pool or an in-flight submit.
pub struct StagingBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: vk::DeviceSize,
    mapped: *mut std::ffi::c_void,
    coherent: bool,
    device: ash::Device,
}

// Staging is only touched on the render thread; the raw mapped pointer is not shared.
unsafe impl Send for StagingBuffer {}

impl StagingBuffer {
    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    pub fn capacity(&self) -> vk::DeviceSize {
        self.capacity
    }

    fn allocate(
        device: &Device,
        physical_device: &PhysicalDevice,
        capacity: vk::DeviceSize,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(capacity > 0, "Staging buffer size must be non-zero");
        let buffer_info = vk::BufferCreateInfo::default()
            .size(capacity)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.handle().create_buffer(&buffer_info, None) }
            .context("Failed to create texture staging buffer")?;
        let requirements = unsafe { device.handle().get_buffer_memory_requirements(buffer) };
        let Some((memory_type_index, coherent)) = find_host_memory_type(
            physical_device.memory_properties(),
            requirements.memory_type_bits,
        ) else {
            unsafe {
                device.handle().destroy_buffer(buffer, None);
            }
            anyhow::bail!("No host-visible Vulkan memory for texture staging");
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
                return Err(error).context("Failed to allocate texture staging memory");
            }
        };
        if let Err(error) = unsafe { device.handle().bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                device.handle().free_memory(memory, None);
                device.handle().destroy_buffer(buffer, None);
            }
            return Err(error).context("Failed to bind texture staging memory");
        }

        let mapped = match unsafe {
            device
                .handle()
                .map_memory(memory, 0, capacity, vk::MemoryMapFlags::empty())
        } {
            Ok(mapped) => mapped,
            Err(error) => {
                unsafe {
                    device.handle().free_memory(memory, None);
                    device.handle().destroy_buffer(buffer, None);
                }
                return Err(error).context("Failed to map texture staging memory");
            }
        };

        Ok(Self {
            buffer,
            memory,
            capacity,
            mapped,
            coherent,
            device: device.handle().clone(),
        })
    }

    /// Copy `bytes` into the start of the persistently mapped staging region.
    pub fn write_slice(&self, device: &Device, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            (bytes.len() as vk::DeviceSize) <= self.capacity,
            "Staging write exceeds buffer capacity"
        );
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), self.mapped.cast(), bytes.len());
        }
        self.flush_written(device, bytes.len() as vk::DeviceSize)
    }

    /// Pack a damage region into tightly packed BGRA rows at the start of staging.
    pub fn write_packed_region(
        &self,
        device: &Device,
        pixels: &[u8],
        stride: u32,
        region: UploadRect,
    ) -> anyhow::Result<()> {
        let row_bytes = usize::try_from(stride).context("Stride overflows")?;
        let region_row_bytes = usize::try_from(region.width)
            .context("Region width overflows")?
            .checked_mul(4)
            .context("Region row bytes overflow")?;
        let region_size = region_row_bytes
            .checked_mul(region.height as usize)
            .context("Region size overflows")?;
        anyhow::ensure!(
            (region_size as vk::DeviceSize) <= self.capacity,
            "Staging write exceeds buffer capacity"
        );
        unsafe {
            let dst_base = self.mapped.cast::<u8>();
            for row in 0..region.height {
                let src_row = (region.y + row) as usize;
                let src_start = src_row
                    .checked_mul(row_bytes)
                    .and_then(|offset| offset.checked_add(region.x as usize * 4))
                    .context("Region source offset overflows")?;
                anyhow::ensure!(
                    src_start + region_row_bytes <= pixels.len(),
                    "Texture pixel data is truncated for damage region"
                );
                let dst_start = row as usize * region_row_bytes;
                ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(src_start),
                    dst_base.add(dst_start),
                    region_row_bytes,
                );
            }
        }
        self.flush_written(device, region_size as vk::DeviceSize)
    }

    fn flush_written(&self, device: &Device, written: vk::DeviceSize) -> anyhow::Result<()> {
        if !self.coherent && written > 0 {
            // WHOLE_SIZE avoids nonCoherentAtomSize alignment requirements on the range.
            let range = vk::MappedMemoryRange::default()
                .memory(self.memory)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            unsafe {
                device
                    .handle()
                    .flush_mapped_memory_ranges(&[range])
                    .context("Failed to flush texture staging memory")?;
            }
        }
        Ok(())
    }
}

impl Drop for StagingBuffer {
    fn drop(&mut self) {
        unsafe {
            self.device.unmap_memory(self.memory);
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// Free-list of host-visible staging buffers reused across frames.
pub struct StagingBufferPool {
    free: Vec<StagingBuffer>,
}

impl StagingBufferPool {
    pub fn new() -> Self {
        Self { free: Vec::new() }
    }

    /// Acquire a buffer with at least `size` bytes of capacity.
    pub fn acquire(
        &mut self,
        device: &Device,
        physical_device: &PhysicalDevice,
        size: vk::DeviceSize,
    ) -> anyhow::Result<StagingBuffer> {
        let needed = round_capacity(size);
        if let Some(index) = self
            .free
            .iter()
            .enumerate()
            .filter(|(_, buffer)| buffer.capacity >= needed)
            .min_by_key(|(_, buffer)| buffer.capacity)
            .map(|(index, _)| index)
        {
            return Ok(self.free.swap_remove(index));
        }
        StagingBuffer::allocate(device, physical_device, needed)
    }

    /// Return a buffer to the free list after GPU work that referenced it has completed.
    pub fn release(&mut self, buffer: StagingBuffer) {
        if self.free.len() < MAX_FREE_STAGING_BUFFERS {
            self.free.push(buffer);
            return;
        }
        // Prefer retaining smaller reusable sizes when the free list is full.
        if let Some(largest) = self
            .free
            .iter()
            .enumerate()
            .max_by_key(|(_, candidate)| candidate.capacity)
            .map(|(index, _)| index)
            && buffer.capacity < self.free[largest].capacity
        {
            let _ = self.free.swap_remove(largest);
            self.free.push(buffer);
        }
        // else drop `buffer`
    }

    pub fn release_many(&mut self, buffers: impl IntoIterator<Item = StagingBuffer>) {
        for buffer in buffers {
            self.release(buffer);
        }
    }

    pub fn clear(&mut self) {
        self.free.clear();
    }
}

fn round_capacity(size: vk::DeviceSize) -> vk::DeviceSize {
    let size = size.max(MIN_STAGING_CAPACITY);
    size.checked_next_power_of_two().unwrap_or(size)
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

    #[test]
    fn round_capacity_uses_minimum_and_pow2() {
        assert_eq!(round_capacity(1), MIN_STAGING_CAPACITY);
        assert_eq!(round_capacity(MIN_STAGING_CAPACITY), MIN_STAGING_CAPACITY);
        assert_eq!(
            round_capacity(MIN_STAGING_CAPACITY + 1),
            MIN_STAGING_CAPACITY * 2
        );
    }
}
