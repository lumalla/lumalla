//! Image and image view management

use anyhow::Context;
use ash::vk;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc};
use gpu_allocator::MemoryLocation;
use log::debug;

use super::{Device, MemoryAllocator};

/// Represents a Vulkan image with its memory allocation and view.
///
/// This struct manages the full lifecycle of an image:
/// - VkImage creation
/// - Memory allocation via gpu-allocator
/// - Image view creation for sampling/rendering
pub struct Image {
    /// The Vulkan image handle
    image: vk::Image,
    /// The memory allocation (managed by gpu-allocator)
    allocation: Option<Allocation>,
    /// The image view for sampling/rendering
    view: vk::ImageView,
    /// Image format
    format: vk::Format,
    /// Image extent (width, height)
    extent: vk::Extent2D,
    /// The device that owns this image
    device: ash::Device,
}

impl Image {
    /// Creates a new 2D image with the specified properties.
    ///
    /// The image is allocated in device-local memory, suitable for rendering targets
    /// and textures that will be sampled by the GPU.
    pub fn new_2d(
        device: &Device,
        allocator: &mut MemoryAllocator,
        format: vk::Format,
        extent: vk::Extent2D,
        usage: vk::ImageUsageFlags,
        samples: vk::SampleCountFlags,
    ) -> anyhow::Result<Self> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(samples)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let image = unsafe { device.handle().create_image(&image_info, None) }
            .context("Failed to create Vulkan image")?;

        // Get memory requirements
        let requirements = unsafe { device.handle().get_image_memory_requirements(image) };

        // Allocate memory using gpu-allocator
        let allocation = allocator
            .inner_mut()
            .allocate(&AllocationCreateDesc {
                name: "image",
                requirements,
                location: MemoryLocation::GpuOnly,
                linear: false, // Optimal tiling is not linear
                allocation_scheme: gpu_allocator::vulkan::AllocationScheme::GpuAllocatorManaged,
            })
            .context("Failed to allocate memory for image")?;

        // Bind image to memory
        unsafe {
            device
                .handle()
                .bind_image_memory(image, allocation.memory(), allocation.offset())
        }
        .context("Failed to bind image memory")?;

        debug!(
            "Created 2D image: {}x{} format={:?}",
            extent.width, extent.height, format
        );

        // Create image view
        let view = Self::create_view(device.handle(), image, format, vk::ImageAspectFlags::COLOR)?;

        Ok(Self {
            image,
            allocation: Some(allocation),
            view,
            format,
            extent,
            device: device.handle().clone(),
        })
    }

    /// Creates a new 2D image suitable for use as a render target (color attachment).
    pub fn new_render_target(
        device: &Device,
        allocator: &mut MemoryAllocator,
        format: vk::Format,
        extent: vk::Extent2D,
    ) -> anyhow::Result<Self> {
        Self::new_2d(
            device,
            allocator,
            format,
            extent,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            vk::SampleCountFlags::TYPE_1,
        )
    }

    /// Creates an image view for the given image.
    fn create_view(
        device: &ash::Device,
        image: vk::Image,
        format: vk::Format,
        aspect_mask: vk::ImageAspectFlags,
    ) -> anyhow::Result<vk::ImageView> {
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
                aspect_mask,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });

        let view = unsafe { device.create_image_view(&view_info, None) }
            .context("Failed to create image view")?;

        Ok(view)
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

    /// Returns the image extent (width, height).
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            // Destroy image view first
            self.device.destroy_image_view(self.view, None);

            // Destroy image (must happen before freeing memory)
            self.device.destroy_image(self.image, None);

            // Free memory allocation
            // The Allocation type from gpu-allocator handles cleanup automatically when dropped.
            // It internally holds a reference to the allocator, so dropping it will free the memory.
            if let Some(allocation) = self.allocation.take() {
                drop(allocation);
            }
        }
        debug!("Destroyed image");
    }
}
