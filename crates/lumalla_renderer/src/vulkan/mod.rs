//! Vulkan rendering backend for lumalla
//!
//! This module provides Vulkan-based rendering using the `ash` crate.

mod clear;
mod command;
mod descriptor;
mod device;
mod dma_buf;
mod framebuffer;
mod image;
mod instance;
mod memory;
mod physical_device;
mod pipeline;
mod render_pass;
pub mod shaders;
mod sync;
mod upload;

pub use clear::clear_framebuffer_to_color;
pub use command::{CommandBufferRecorder, CommandPool};
pub use descriptor::DescriptorSetLayout;
pub use device::Device;
pub use dma_buf::{
    DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR,
    DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB8888, DmaBufImage, vulkan_to_drm_fourcc,
};
pub use framebuffer::Framebuffer;
pub use image::Image;
pub use instance::VulkanContext;
pub use memory::MemoryAllocator;
pub use physical_device::PhysicalDevice;
pub use pipeline::{GraphicsPipeline, GraphicsPipelineBuilder, ShaderModule};
pub use render_pass::RenderPass;
pub use sync::{Fence, Semaphore};
pub use upload::upload_bgra_to_image;
