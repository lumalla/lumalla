//! Vulkan rendering backend for lumalla
//!
//! This module provides Vulkan-based rendering using the `ash` crate.

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

pub use command::{CommandBufferRecorder, CommandPool};
pub use descriptor::DescriptorSetLayout;
pub use device::Device;
pub use dma_buf::{drm_to_vulkan_format, ImportedDmaBuf, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR};
pub use framebuffer::Framebuffer;
pub use image::Image;
pub use instance::VulkanContext;
pub use memory::MemoryAllocator;
pub use physical_device::PhysicalDevice;
pub use pipeline::{GraphicsPipeline, GraphicsPipelineBuilder, ShaderModule};
pub use render_pass::RenderPass;
pub use sync::{Fence, Semaphore};
