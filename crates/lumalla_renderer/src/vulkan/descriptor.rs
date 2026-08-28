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

    /// Creates a descriptor set layout for a sampled image + separate sampler.
    pub fn new_texture_sampler(device: &Device) -> anyhow::Result<Self> {
        let image_binding = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);
        let sampler_binding = vk::DescriptorSetLayoutBinding::default()
            .binding(1)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);

        Self::new(device, &[image_binding, sampler_binding])
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

/// Pool for allocating descriptor sets.
pub struct DescriptorPool {
    handle: vk::DescriptorPool,
    device: ash::Device,
}

impl DescriptorPool {
    pub fn new_combined_image_sampler(device: &Device, max_sets: u32) -> anyhow::Result<Self> {
        let image_pool = vk::DescriptorPoolSize {
            ty: vk::DescriptorType::SAMPLED_IMAGE,
            descriptor_count: max_sets,
        };
        let sampler_pool = vk::DescriptorPoolSize {
            ty: vk::DescriptorType::SAMPLER,
            descriptor_count: max_sets,
        };
        let pool_sizes = [image_pool, sampler_pool];
        let create_info = vk::DescriptorPoolCreateInfo::default()
            .pool_sizes(&pool_sizes)
            .max_sets(max_sets)
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET);

        let handle = unsafe { device.handle().create_descriptor_pool(&create_info, None) }
            .context("Failed to create descriptor pool")?;

        Ok(Self {
            handle,
            device: device.handle().clone(),
        })
    }

    pub fn allocate_sampler_set(
        &self,
        device: &Device,
        layout: &DescriptorSetLayout,
    ) -> anyhow::Result<vk::DescriptorSet> {
        let layouts = [layout.handle()];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.handle)
            .set_layouts(&layouts);

        let sets = unsafe { device.handle().allocate_descriptor_sets(&allocate_info) }
            .context("Failed to allocate descriptor set")?;
        Ok(sets[0])
    }

    pub fn free_set(&self, device: &Device, set: vk::DescriptorSet) -> anyhow::Result<()> {
        unsafe { device.handle().free_descriptor_sets(self.handle, &[set]) }
            .context("Failed to free descriptor set")?;
        Ok(())
    }

    pub fn handle(&self) -> vk::DescriptorPool {
        self.handle
    }
}

impl Drop for DescriptorPool {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_descriptor_pool(self.handle, None);
        }
        debug!("Destroyed descriptor pool");
    }
}
