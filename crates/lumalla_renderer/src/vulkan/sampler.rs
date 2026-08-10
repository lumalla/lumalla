//! Vulkan sampler for texture compositing.

use anyhow::Context;
use ash::vk;
use log::debug;

use super::Device;

pub struct Sampler {
    handle: vk::Sampler,
    device: ash::Device,
}

impl Sampler {
    /// Nearest-neighbor sampler matching CPU nearest scaling for buffer_scale > 1.
    pub fn new_nearest(device: &Device) -> anyhow::Result<Self> {
        let create_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);

        let handle = unsafe { device.handle().create_sampler(&create_info, None) }
            .context("Failed to create texture sampler")?;

        debug!("Created nearest-neighbor sampler");

        Ok(Self {
            handle,
            device: device.handle().clone(),
        })
    }

    pub fn handle(&self) -> vk::Sampler {
        self.handle
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_sampler(self.handle, None);
        }
        debug!("Destroyed sampler");
    }
}
