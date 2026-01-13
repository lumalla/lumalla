//! Swapchain and display management

use anyhow::Context;
use ash::vk;
use log::{debug, info, warn};

use super::{Device, PhysicalDevice, Semaphore};

/// Information about a physical display.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    /// The display handle
    pub display: vk::DisplayKHR,
    /// Display name
    pub name: String,
    /// Physical width in millimeters
    pub physical_width_mm: u32,
    /// Physical height in millimeters
    pub physical_height_mm: u32,
    /// Physical resolution
    pub physical_resolution: vk::Extent2D,
}

/// A display mode (resolution and refresh rate).
#[derive(Debug, Clone)]
pub struct DisplayModeInfo {
    /// The display mode handle
    pub mode: vk::DisplayModeKHR,
    /// Visible region dimensions
    pub visible_region: vk::Extent2D,
    /// Refresh rate in millihertz (e.g., 60000 = 60Hz)
    pub refresh_rate: u32,
}

/// Manages a Vulkan swapchain for presenting to a display.
pub struct Swapchain {
    /// The swapchain handle
    handle: vk::SwapchainKHR,
    /// The surface we're presenting to
    surface: vk::SurfaceKHR,
    /// Swapchain images
    images: Vec<vk::Image>,
    /// Image views for the swapchain images
    image_views: Vec<vk::ImageView>,
    /// The swapchain image format
    format: vk::Format,
    /// The swapchain extent
    extent: vk::Extent2D,
    /// Swapchain extension loader
    swapchain_loader: ash::khr::swapchain::Device,
    /// Surface extension loader
    surface_loader: ash::khr::surface::Instance,
    /// Display extension loader
    display_loader: ash::khr::display::Instance,
    /// The device that owns this swapchain
    device: ash::Device,
    /// The instance (needed for surface destruction)
    instance: ash::Instance,
}

impl Swapchain {
    /// Enumerates available displays.
    pub fn enumerate_displays(
        entry: &ash::Entry,
        instance: &ash::Instance,
        physical_device: &PhysicalDevice,
    ) -> anyhow::Result<Vec<DisplayInfo>> {
        let display_loader = ash::khr::display::Instance::new(entry, instance);

        let displays = unsafe {
            display_loader.get_physical_device_display_properties(physical_device.handle())
        }
        .context("Failed to enumerate displays")?;

        let mut result = Vec::new();
        for display_props in displays {
            let name = unsafe {
                std::ffi::CStr::from_ptr(display_props.display_name)
                    .to_string_lossy()
                    .into_owned()
            };

            result.push(DisplayInfo {
                display: display_props.display,
                name,
                physical_width_mm: display_props.physical_dimensions.width,
                physical_height_mm: display_props.physical_dimensions.height,
                physical_resolution: display_props.physical_resolution,
            });
        }

        info!("Found {} display(s)", result.len());
        for (i, display) in result.iter().enumerate() {
            info!(
                "  Display {}: {} ({}x{})",
                i, display.name, display.physical_resolution.width, display.physical_resolution.height
            );
        }

        Ok(result)
    }

    /// Gets display modes for a display.
    pub fn get_display_modes(
        entry: &ash::Entry,
        instance: &ash::Instance,
        physical_device: &PhysicalDevice,
        display: vk::DisplayKHR,
    ) -> anyhow::Result<Vec<DisplayModeInfo>> {
        let display_loader = ash::khr::display::Instance::new(entry, instance);

        let modes = unsafe {
            display_loader.get_display_mode_properties(physical_device.handle(), display)
        }
        .context("Failed to get display modes")?;

        let result: Vec<DisplayModeInfo> = modes
            .into_iter()
            .map(|mode| DisplayModeInfo {
                mode: mode.display_mode,
                visible_region: mode.parameters.visible_region,
                refresh_rate: mode.parameters.refresh_rate,
            })
            .collect();

        debug!("Found {} display mode(s)", result.len());

        Ok(result)
    }

    /// Creates a new swapchain for the first available display.
    ///
    /// This will:
    /// 1. Find the first available display
    /// 2. Select a suitable display mode
    /// 3. Create a display surface
    /// 4. Create a swapchain
    pub fn new_for_display(
        entry: &ash::Entry,
        instance: &ash::Instance,
        device: &Device,
        physical_device: &PhysicalDevice,
    ) -> anyhow::Result<Self> {
        let display_loader = ash::khr::display::Instance::new(entry, instance);
        let surface_loader = ash::khr::surface::Instance::new(entry, instance);
        let swapchain_loader = ash::khr::swapchain::Device::new(instance, device.handle());

        // Find displays
        let displays = Self::enumerate_displays(entry, instance, physical_device)?;
        if displays.is_empty() {
            anyhow::bail!("No displays found");
        }

        let display_info = &displays[0];
        info!("Using display: {}", display_info.name);

        // Get display modes
        let modes = Self::get_display_modes(entry, instance, physical_device, display_info.display)?;
        if modes.is_empty() {
            anyhow::bail!("No display modes available");
        }

        // Select the best mode (prefer highest resolution, then highest refresh rate)
        let mode = modes
            .iter()
            .max_by_key(|m| {
                (
                    m.visible_region.width * m.visible_region.height,
                    m.refresh_rate,
                )
            })
            .unwrap();

        info!(
            "Using display mode: {}x{} @ {:.2}Hz",
            mode.visible_region.width,
            mode.visible_region.height,
            mode.refresh_rate as f32 / 1000.0
        );

        // Find a suitable plane
        let planes = unsafe {
            display_loader.get_physical_device_display_plane_properties(physical_device.handle())
        }
        .context("Failed to get display planes")?;

        let mut selected_plane_index = None;
        for (i, plane) in planes.iter().enumerate() {
            // Check if this plane supports our display
            let supported_displays = unsafe {
                display_loader.get_display_plane_supported_displays(
                    physical_device.handle(),
                    i as u32,
                )
            }
            .context("Failed to get supported displays for plane")?;

            if supported_displays.contains(&display_info.display)
                || plane.current_display == vk::DisplayKHR::null()
            {
                selected_plane_index = Some(i as u32);
                break;
            }
        }

        let plane_index = selected_plane_index.context("No suitable display plane found")?;
        debug!("Using display plane index: {}", plane_index);

        // Get plane capabilities
        let plane_caps = unsafe {
            display_loader.get_display_plane_capabilities(
                physical_device.handle(),
                mode.mode,
                plane_index,
            )
        }
        .context("Failed to get display plane capabilities")?;

        // Create display surface
        let surface_create_info = vk::DisplaySurfaceCreateInfoKHR::default()
            .display_mode(mode.mode)
            .plane_index(plane_index)
            .plane_stack_index(0)
            .transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
            .global_alpha(1.0)
            .alpha_mode(if plane_caps
                .supported_alpha
                .contains(vk::DisplayPlaneAlphaFlagsKHR::OPAQUE)
            {
                vk::DisplayPlaneAlphaFlagsKHR::OPAQUE
            } else {
                vk::DisplayPlaneAlphaFlagsKHR::GLOBAL
            })
            .image_extent(mode.visible_region);

        let surface = unsafe { display_loader.create_display_plane_surface(&surface_create_info, None) }
            .context("Failed to create display surface")?;

        info!("Created display surface");

        // Check surface support
        let supported = unsafe {
            surface_loader.get_physical_device_surface_support(
                physical_device.handle(),
                device.graphics_queue_family(),
                surface,
            )
        }
        .context("Failed to check surface support")?;

        if !supported {
            anyhow::bail!("Surface not supported by graphics queue family");
        }

        // Get surface capabilities
        let surface_caps = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device.handle(), surface)
        }
        .context("Failed to get surface capabilities")?;

        // Get surface formats
        let surface_formats = unsafe {
            surface_loader.get_physical_device_surface_formats(physical_device.handle(), surface)
        }
        .context("Failed to get surface formats")?;

        // Select format (prefer BGRA8 SRGB)
        let format = surface_formats
            .iter()
            .find(|f| {
                f.format == vk::Format::B8G8R8A8_SRGB
                    && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
            })
            .or_else(|| {
                surface_formats.iter().find(|f| {
                    f.format == vk::Format::B8G8R8A8_UNORM
                        && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
                })
            })
            .unwrap_or(&surface_formats[0]);

        info!("Using surface format: {:?}", format.format);

        // Determine extent
        let extent = if surface_caps.current_extent.width != u32::MAX {
            surface_caps.current_extent
        } else {
            mode.visible_region
        };

        // Determine image count (prefer triple buffering)
        let image_count = (surface_caps.min_image_count + 1).min(
            if surface_caps.max_image_count > 0 {
                surface_caps.max_image_count
            } else {
                3
            },
        );

        debug!("Swapchain image count: {}", image_count);

        // Create swapchain
        let swapchain_create_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(image_count)
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(surface_caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(vk::PresentModeKHR::FIFO) // VSync
            .clipped(true);

        let handle = unsafe { swapchain_loader.create_swapchain(&swapchain_create_info, None) }
            .context("Failed to create swapchain")?;

        info!("Created swapchain: {}x{}", extent.width, extent.height);

        // Get swapchain images
        let images = unsafe { swapchain_loader.get_swapchain_images(handle) }
            .context("Failed to get swapchain images")?;

        debug!("Got {} swapchain images", images.len());

        // Create image views
        let image_views: Result<Vec<_>, _> = images
            .iter()
            .map(|&image| {
                let view_info = vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format.format)
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

                unsafe { device.handle().create_image_view(&view_info, None) }
            })
            .collect();

        let image_views = image_views.context("Failed to create swapchain image views")?;

        Ok(Self {
            handle,
            surface,
            images,
            image_views,
            format: format.format,
            extent,
            swapchain_loader,
            surface_loader,
            display_loader,
            device: device.handle().clone(),
            instance: instance.clone(),
        })
    }

    /// Acquires the next image from the swapchain.
    ///
    /// Returns the index of the acquired image.
    pub fn acquire_next_image(&self, semaphore: &Semaphore) -> anyhow::Result<u32> {
        let (index, _suboptimal) = unsafe {
            self.swapchain_loader.acquire_next_image(
                self.handle,
                u64::MAX, // timeout
                semaphore.handle(),
                vk::Fence::null(),
            )
        }
        .context("Failed to acquire next swapchain image")?;

        Ok(index)
    }

    /// Presents an image to the display.
    pub fn present(&self, image_index: u32, wait_semaphore: &Semaphore, queue: vk::Queue) -> anyhow::Result<()> {
        let swapchains = [self.handle];
        let image_indices = [image_index];
        let wait_semaphores = [wait_semaphore.handle()];

        let present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(&wait_semaphores)
            .swapchains(&swapchains)
            .image_indices(&image_indices);

        unsafe { self.swapchain_loader.queue_present(queue, &present_info) }
            .context("Failed to present")?;

        Ok(())
    }

    /// Returns the swapchain format.
    pub fn format(&self) -> vk::Format {
        self.format
    }

    /// Returns the swapchain extent.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// Returns the number of swapchain images.
    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    /// Returns an image view by index.
    pub fn image_view(&self, index: usize) -> vk::ImageView {
        self.image_views[index]
    }

    /// Returns all image views.
    pub fn image_views(&self) -> &[vk::ImageView] {
        &self.image_views
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        unsafe {
            // Destroy image views
            for &view in &self.image_views {
                self.device.destroy_image_view(view, None);
            }

            // Destroy swapchain
            self.swapchain_loader.destroy_swapchain(self.handle, None);

            // Destroy surface
            self.surface_loader.destroy_surface(self.surface, None);
        }
        info!("Destroyed swapchain");
    }
}
