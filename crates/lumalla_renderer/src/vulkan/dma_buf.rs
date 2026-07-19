//! Vulkan-allocated images exported as DMA-BUFs for KMS scanout.

use std::os::fd::{FromRawFd, OwnedFd};

use anyhow::Context;
use ash::vk;
use log::debug;

use super::{Device, PhysicalDevice};

/// The DRM_FORMAT_MOD_LINEAR modifier value.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// The DRM_FORMAT_MOD_INVALID modifier value.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ffffffffffffff;

/// DRM fourcc: XRGB8888 ('XR24').
pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

/// DRM fourcc: ARGB8888 ('AR24').
pub const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");

/// DRM fourcc: XBGR8888 ('XB24').
pub const DRM_FORMAT_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");

/// DRM fourcc: ABGR8888 ('AB24').
pub const DRM_FORMAT_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");

/// A Vulkan image allocated for DMA-BUF export (and later KMS scanout).
pub struct DmaBufImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    format: vk::Format,
    extent: vk::Extent2D,
    modifier: u64,
    stride: u32,
    offset: u32,
    device: ash::Device,
    external_memory_fd: ash::khr::external_memory_fd::Device,
}

impl DmaBufImage {
    /// Allocates a new exportable image suitable for rendering and DMA-BUF export.
    ///
    /// Uses `DRM_FORMAT_MOD_LINEAR` so the buffer can be imported by KMS without
    /// negotiating a device-specific modifier yet.
    pub fn allocate(
        device: &Device,
        physical_device: &PhysicalDevice,
        width: u32,
        height: u32,
        format: vk::Format,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(width > 0 && height > 0, "Image dimensions must be non-zero");

        let extent = vk::Extent2D { width, height };
        let modifiers = [DRM_FORMAT_MOD_LINEAR];

        let mut external_memory_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
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
            .context("Failed to create exportable Vulkan image")?;

        let mut dedicated_requirements = vk::MemoryDedicatedRequirements::default();
        let mut memory_requirements2 =
            vk::MemoryRequirements2::default().push_next(&mut dedicated_requirements);
        let image_requirements_info = vk::ImageMemoryRequirementsInfo2::default().image(image);
        unsafe {
            device.handle().get_image_memory_requirements2(
                &image_requirements_info,
                &mut memory_requirements2,
            );
        }
        let mem_requirements = memory_requirements2.memory_requirements;

        let memory_type_index = find_memory_type_index(
            physical_device.memory_properties(),
            mem_requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .context("No DEVICE_LOCAL memory type for exportable image")?;

        let mut export_alloc_info = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated_alloc_info = vk::MemoryDedicatedAllocateInfo::default().image(image);

        let mut alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut export_alloc_info);

        if dedicated_requirements.requires_dedicated_allocation == vk::TRUE
            || dedicated_requirements.prefers_dedicated_allocation == vk::TRUE
        {
            alloc_info = alloc_info.push_next(&mut dedicated_alloc_info);
        }

        let memory = unsafe { device.handle().allocate_memory(&alloc_info, None) }
            .context("Failed to allocate exportable Vulkan memory")?;

        unsafe { device.handle().bind_image_memory(image, memory, 0) }
            .context("Failed to bind exportable image memory")?;

        let mut modifier_props = vk::ImageDrmFormatModifierPropertiesEXT::default();
        unsafe {
            device
                .image_drm_format_modifier()
                .get_image_drm_format_modifier_properties(image, &mut modifier_props)
        }
        .context("Failed to query DRM format modifier for image")?;

        let layout = unsafe {
            device.handle().get_image_subresource_layout(
                image,
                vk::ImageSubresource {
                    aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                    mip_level: 0,
                    array_layer: 0,
                },
            )
        };

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(vk::ComponentMapping::default())
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });

        let view = unsafe { device.handle().create_image_view(&view_info, None) }
            .context("Failed to create image view for exportable image")?;

        debug!(
            "Allocated exportable Vulkan image: {}x{} format={:?} modifier={:#x} stride={}",
            width, height, format, modifier_props.drm_format_modifier, layout.row_pitch
        );

        Ok(Self {
            image,
            memory,
            view,
            format,
            extent,
            modifier: modifier_props.drm_format_modifier,
            stride: layout.row_pitch as u32,
            offset: layout.offset as u32,
            device: device.handle().clone(),
            external_memory_fd: device.external_memory_fd().clone(),
        })
    }

    /// Exports the image memory as a DMA-BUF file descriptor.
    pub fn export_dma_buf(&self) -> anyhow::Result<OwnedFd> {
        let get_fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(self.memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let fd = unsafe { self.external_memory_fd.get_memory_fd(&get_fd_info) }
            .context("Failed to export DMA-BUF from Vulkan memory")?;

        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn image(&self) -> vk::Image {
        self.image
    }

    pub fn view(&self) -> vk::ImageView {
        self.view
    }

    pub fn format(&self) -> vk::Format {
        self.format
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    pub fn modifier(&self) -> u64 {
        self.modifier
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    pub fn offset(&self) -> u32 {
        self.offset
    }

    /// DRM fourcc matching this Vulkan format, if known.
    pub fn drm_fourcc(&self) -> Option<u32> {
        vulkan_to_drm_fourcc(self.format)
    }
}

impl Drop for DmaBufImage {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
        debug!("Destroyed exportable Vulkan DMA-BUF image");
    }
}

/// Maps a Vulkan format to a DRM fourcc.
pub fn vulkan_to_drm_fourcc(format: vk::Format) -> Option<u32> {
    match format {
        vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB => Some(DRM_FORMAT_ARGB8888),
        vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB => Some(DRM_FORMAT_ABGR8888),
        _ => None,
    }
}

fn find_memory_type_index(
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required_properties: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..memory_properties.memory_type_count).find(|&index| {
        let suitable = type_bits & (1 << index) != 0;
        let flags = memory_properties.memory_types[index as usize].property_flags;
        suitable && flags.contains(required_properties)
    })
}
