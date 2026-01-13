//! GPU memory allocation using gpu-allocator

use anyhow::Context;
use ash::vk;
use gpu_allocator::vulkan::{Allocator, AllocatorCreateDesc};
use gpu_allocator::{AllocationSizes, AllocatorDebugSettings};
use log::info;

use super::Device;

/// Wrapper around gpu-allocator for Vulkan memory management.
///
/// This provides efficient sub-allocation of GPU memory for images and buffers.
pub struct MemoryAllocator {
    allocator: Allocator,
}

impl MemoryAllocator {
    /// Creates a new memory allocator for the given device.
    pub fn new(
        instance: &ash::Instance,
        device: &Device,
        physical_device: vk::PhysicalDevice,
    ) -> anyhow::Result<Self> {
        let allocator = Allocator::new(&AllocatorCreateDesc {
            instance: instance.clone(),
            device: device.handle().clone(),
            physical_device,
            buffer_device_address: false, // We don't need this for basic compositing
            allocation_sizes: AllocationSizes::default(),
            debug_settings: AllocatorDebugSettings::default(),
        })
        .context("Failed to create GPU memory allocator")?;

        info!("GPU memory allocator created");

        Ok(Self { allocator })
    }

    /// Returns a reference to the underlying allocator.
    pub fn inner(&self) -> &Allocator {
        &self.allocator
    }

    /// Returns a mutable reference to the underlying allocator.
    pub fn inner_mut(&mut self) -> &mut Allocator {
        &mut self.allocator
    }
}

impl Drop for MemoryAllocator {
    fn drop(&mut self) {
        info!("Destroying GPU memory allocator");
        // gpu-allocator handles cleanup internally
    }
}
