//! Logical device creation and management

use std::ffi::CStr;

use anyhow::Context;
use ash::vk;
use log::{debug, info};

use super::PhysicalDevice;

/// Represents a Vulkan logical device and its queues.
///
/// The logical device is the primary interface for interacting with the GPU.
/// It holds the device handle and queue handles for submitting commands.
pub struct Device {
    /// The Vulkan logical device handle
    handle: ash::Device,
    /// The graphics queue
    graphics_queue: vk::Queue,
    /// The graphics queue family index
    graphics_queue_family: u32,
}

impl Device {
    /// Creates a new logical device from the selected physical device.
    ///
    /// This sets up:
    /// - A logical device with required features
    /// - A graphics queue for rendering commands
    pub fn new(instance: &ash::Instance, physical_device: &PhysicalDevice) -> anyhow::Result<Self> {
        let graphics_queue_family = physical_device.graphics_queue_family();

        // Queue creation info - we only need one graphics queue for now
        let queue_priorities = [1.0_f32];
        let queue_create_infos = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(graphics_queue_family)
            .queue_priorities(&queue_priorities)];

        // Query available device extensions
        let available_extensions =
            unsafe { instance.enumerate_device_extension_properties(physical_device.handle()) }
                .context("Failed to enumerate device extensions")?;

        let available_extension_names: Vec<&CStr> = available_extensions
            .iter()
            .filter_map(|ext| ext.extension_name_as_c_str().ok())
            .collect();

        debug!(
            "Available device extensions: {} total",
            available_extension_names.len()
        );

        // Determine which extensions to enable
        let mut extensions_to_enable: Vec<&CStr> = Vec::new();

        // Extensions needed for a Wayland compositor
        let desired_extensions: &[&CStr] = &[
            // For DRM/KMS rendering (swapchain still useful for some cases)
            ash::khr::swapchain::NAME,
            // For timeline semaphores (useful for synchronization)
            ash::khr::timeline_semaphore::NAME,
            // For DMA-BUF import (needed for GBM buffer import)
            ash::khr::external_memory::NAME,
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            // For DRM format modifiers
            ash::ext::image_drm_format_modifier::NAME,
            // Required dependency for external memory
            ash::khr::bind_memory2::NAME,
            ash::khr::get_memory_requirements2::NAME,
            // For synchronization with DRM
            ash::khr::external_semaphore::NAME,
            ash::khr::external_semaphore_fd::NAME,
        ];

        for &ext in desired_extensions {
            if available_extension_names.contains(&ext) {
                extensions_to_enable.push(ext);
                debug!("Enabling device extension: {:?}", ext);
            } else {
                debug!("Device extension not available: {:?}", ext);
            }
        }

        let extension_ptrs: Vec<*const i8> = extensions_to_enable
            .iter()
            .map(|ext| ext.as_ptr())
            .collect();

        // Enable required features
        // Vulkan 1.2 features via the pNext chain
        let mut vulkan_12_features = vk::PhysicalDeviceVulkan12Features::default()
            // Timeline semaphores for better synchronization
            .timeline_semaphore(true);

        let device_features = vk::PhysicalDeviceFeatures::default();

        let mut features2 = vk::PhysicalDeviceFeatures2::default()
            .features(device_features)
            .push_next(&mut vulkan_12_features);

        // Create the logical device
        let device_create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_create_infos)
            .enabled_extension_names(&extension_ptrs)
            .push_next(&mut features2);

        let device = unsafe { instance.create_device(physical_device.handle(), &device_create_info, None) }
            .context("Failed to create logical device")?;

        info!("Vulkan logical device created");

        // Get the graphics queue
        let graphics_queue = unsafe { device.get_device_queue(graphics_queue_family, 0) };
        debug!(
            "Got graphics queue from family {}",
            graphics_queue_family
        );

        Ok(Self {
            handle: device,
            graphics_queue,
            graphics_queue_family,
        })
    }

    /// Returns the raw Vulkan device handle.
    pub fn handle(&self) -> &ash::Device {
        &self.handle
    }

    /// Returns the graphics queue.
    pub fn graphics_queue(&self) -> vk::Queue {
        self.graphics_queue
    }

    /// Returns the graphics queue family index.
    pub fn graphics_queue_family(&self) -> u32 {
        self.graphics_queue_family
    }

    /// Waits for the device to become idle.
    ///
    /// This is useful for cleanup and synchronization.
    pub fn wait_idle(&self) -> anyhow::Result<()> {
        unsafe { self.handle.device_wait_idle() }.context("Failed to wait for device idle")?;
        Ok(())
    }

    /// Submits command buffers to the graphics queue.
    ///
    /// This submits the given command buffers with synchronization:
    /// - Waits for `wait_semaphores` before execution
    /// - Signals `signal_semaphores` after execution
    /// - Signals `fence` when all commands complete
    pub fn submit_graphics(
        &self,
        command_buffers: &[vk::CommandBuffer],
        wait_semaphores: &[vk::Semaphore],
        wait_stages: &[vk::PipelineStageFlags],
        signal_semaphores: &[vk::Semaphore],
        fence: vk::Fence,
    ) -> anyhow::Result<()> {
        let submit_info = vk::SubmitInfo::default()
            .wait_semaphores(wait_semaphores)
            .wait_dst_stage_mask(wait_stages)
            .command_buffers(command_buffers)
            .signal_semaphores(signal_semaphores);

        unsafe { self.handle.queue_submit(self.graphics_queue, &[submit_info], fence) }
            .context("Failed to submit to graphics queue")?;

        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        info!("Destroying Vulkan logical device");
        unsafe {
            // Wait for all operations to complete before destroying
            let _ = self.handle.device_wait_idle();
            self.handle.destroy_device(None);
        }
    }
}
