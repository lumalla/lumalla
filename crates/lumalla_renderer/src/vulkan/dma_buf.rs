//! DMA-BUF import for Vulkan
//!
//! This module provides functionality to import DMA-BUF file descriptors
//! (from GBM buffers) into Vulkan as VkImages.

use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::Context;
use ash::vk;
use log::debug;

use super::Device;

/// An imported DMA-BUF image.
///
/// This wraps a VkImage that was imported from a DMA-BUF file descriptor.
pub struct ImportedDmaBuf {
    /// The Vulkan image handle
    image: vk::Image,
    /// The imported memory
    memory: vk::DeviceMemory,
    /// The image view
    view: vk::ImageView,
    /// Image format
    format: vk::Format,
    /// Image extent
    extent: vk::Extent2D,
    /// The device
    device: ash::Device,
}

impl ImportedDmaBuf {
    /// Imports a DMA-BUF as a Vulkan image.
    ///
    /// # Arguments
    /// * `device` - The Vulkan device
    /// * `fd` - The DMA-BUF file descriptor (will be consumed)
    /// * `width` - Image width
    /// * `height` - Image height
    /// * `format` - Vulkan format (must be compatible with the DMA-BUF format)
    /// * `modifier` - DRM format modifier (or LINEAR if none)
    pub fn import(
        device: &Device,
        fd: OwnedFd,
        width: u32,
        height: u32,
        format: vk::Format,
        modifier: u64,
    ) -> anyhow::Result<Self> {
        let extent = vk::Extent2D { width, height };

        // Create the image with external memory info
        let mut external_memory_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        // Set up DRM format modifier info
        let modifiers = [modifier];
        let mut modifier_list_info =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_memory_info)
            .push_next(&mut modifier_list_info);

        let image = unsafe { device.handle().create_image(&image_info, None) }
            .context("Failed to create image for DMA-BUF import")?;

        // Get memory requirements
        let mem_requirements = unsafe { device.handle().get_image_memory_requirements(image) };

        // Import the DMA-BUF fd
        let raw_fd = fd.as_raw_fd();

        let mut import_memory_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw_fd);

        // Find a suitable memory type
        let memory_type_index = 0; // TODO: Properly select memory type

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut import_memory_info);

        let memory = unsafe { device.handle().allocate_memory(&alloc_info, None) }
            .context("Failed to allocate memory for DMA-BUF import")?;

        // Don't close the fd - Vulkan now owns it
        std::mem::forget(fd);

        // Bind image to memory
        unsafe { device.handle().bind_image_memory(image, memory, 0) }
            .context("Failed to bind DMA-BUF memory to image")?;

        // Create image view
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::IDENTITY,
            })
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });

        let view = unsafe { device.handle().create_image_view(&view_info, None) }
            .context("Failed to create image view for DMA-BUF")?;

        debug!(
            "Imported DMA-BUF as Vulkan image: {}x{} format={:?}",
            width, height, format
        );

        Ok(Self {
            image,
            memory,
            view,
            format,
            extent,
            device: device.handle().clone(),
        })
    }

    /// Returns the Vulkan image handle.
    pub fn image(&self) -> vk::Image {
        self.image
    }

    /// Returns the image view handle.
    pub fn view(&self) -> vk::ImageView {
        self.view
    }

    /// Returns the image format.
    pub fn format(&self) -> vk::Format {
        self.format
    }

    /// Returns the image extent.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }
}

impl Drop for ImportedDmaBuf {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
        debug!("Destroyed imported DMA-BUF image");
    }
}

/// Converts a DRM fourcc format to a Vulkan format.
pub fn drm_to_vulkan_format(fourcc: drm::buffer::DrmFourcc) -> Option<vk::Format> {
    use drm::buffer::DrmFourcc;

    match fourcc {
        DrmFourcc::Xrgb8888 => Some(vk::Format::B8G8R8A8_UNORM),
        DrmFourcc::Argb8888 => Some(vk::Format::B8G8R8A8_UNORM),
        DrmFourcc::Xbgr8888 => Some(vk::Format::R8G8B8A8_UNORM),
        DrmFourcc::Abgr8888 => Some(vk::Format::R8G8B8A8_UNORM),
        DrmFourcc::Rgb888 => Some(vk::Format::R8G8B8_UNORM),
        DrmFourcc::Bgr888 => Some(vk::Format::B8G8R8_UNORM),
        _ => None,
    }
}

/// The DRM_FORMAT_MOD_LINEAR modifier value.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// The DRM_FORMAT_MOD_INVALID modifier value.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ffffffffffffff;
