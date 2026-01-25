//! Descriptor set layout and management

use anyhow::Context;
use ash::vk;
use log::debug;

use super::Device;

/// Represents a descriptor set layout.
///
/// Descriptor set layouts define the structure of descriptor sets,
/// which are used to bind resources (textures, buffers, etc.) to shaders.
pub struct DescriptorSetLayout {
    /// The Vulkan descriptor set layout handle
    handle: vk::DescriptorSetLayout,
    /// The device that owns this layout
    device: ash::Device,
}

impl DescriptorSetLayout {
    /// Creates a new descriptor set layout from bindings.
    pub fn new(
        device: &Device,
        bindings: &[vk::DescriptorSetLayoutBinding],
    ) -> anyhow::Result<Self> {
        let create_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(bindings);

        let handle = unsafe {
            device
                .handle()
                .create_descriptor_set_layout(&create_info, None)
        }
        .context("Failed to create descriptor set layout")?;

        debug!(
            "Created descriptor set layout with {} bindings",
            bindings.len()
        );

        Ok(Self {
            handle,
            device: device.handle().clone(),
        })
    }

    /// Creates a simple descriptor set layout for a single combined image sampler.
    ///
    /// This is commonly used for texture sampling in fragment shaders.
    pub fn new_sampler(device: &Device, binding: u32) -> anyhow::Result<Self> {
        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(binding)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);

        Self::new(device, &[binding])
    }

    /// Returns the descriptor set layout handle.
    pub fn handle(&self) -> vk::DescriptorSetLayout {
        self.handle
    }
}

impl Drop for DescriptorSetLayout {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_descriptor_set_layout(self.handle, None);
        }
        debug!("Destroyed descriptor set layout");
    }
}
