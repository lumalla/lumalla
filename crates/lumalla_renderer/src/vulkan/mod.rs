//! Vulkan rendering backend for lumalla
//!
//! This module provides Vulkan-based rendering using the `ash` crate.

mod clear;
mod command;
mod descriptor;
mod device;
mod dma_buf;
mod framebuffer;
mod gpu_compositor;
mod image;
mod instance;
mod memory;
mod physical_device;
mod pipeline;
mod render_pass;
pub mod shaders;
mod sampler;
mod sync;
mod upload;

pub use clear::clear_framebuffer_to_color;
pub use command::{CommandBufferRecorder, CommandPool};
pub use descriptor::{DescriptorPool, DescriptorSetLayout};
pub use device::Device;
pub use dma_buf::{
    DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR,
    DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB8888, DmaBufImage, drm_fourcc_to_vulkan,
    query_samplable_dmabuf_formats, vulkan_to_drm_fourcc,
};
pub use framebuffer::Framebuffer;
pub use gpu_compositor::{
    GpuCompositor, GpuWorkBatch, PendingGpuSubmit, SurfaceTextureCache, composite_to_scanout,
    copy_scanout_frame,
};
pub use image::Image;
pub use instance::VulkanContext;
pub use memory::MemoryAllocator;
pub use physical_device::PhysicalDevice;
pub use pipeline::{GraphicsPipeline, GraphicsPipelineBuilder, ShaderModule};
pub use render_pass::RenderPass;
pub use sampler::Sampler;
pub use sync::{Fence, Semaphore};
pub use upload::{UploadRegion, upload_bgra_regions_from_backing, upload_bgra_to_image};
