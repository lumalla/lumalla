//! Vulkan rendering backend for lumalla
//!
//! This module provides Vulkan-based rendering using the `ash` crate.

mod command;
mod descriptor;
mod device;
mod framebuffer;
mod image;
mod instance;
mod memory;
mod physical_device;
mod pipeline;
mod render_pass;
pub mod shaders;
mod swapchain;
mod sync;

pub use command::{CommandBufferRecorder, CommandPool};
pub use descriptor::DescriptorSetLayout;
pub use device::Device;
pub use framebuffer::Framebuffer;
pub use image::Image;
pub use instance::VulkanContext;
pub use memory::MemoryAllocator;
pub use physical_device::PhysicalDevice;
pub use pipeline::{GraphicsPipeline, GraphicsPipelineBuilder, ShaderModule};
pub use render_pass::RenderPass;
pub use swapchain::{DisplayInfo, DisplayModeInfo, Swapchain};
pub use sync::{Fence, Semaphore};
