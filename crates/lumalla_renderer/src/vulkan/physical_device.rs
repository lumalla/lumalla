//! Physical device (GPU) selection and management

use std::ffi::CStr;

use anyhow::Context;
use ash::vk;
use log::{debug, info};

/// Represents a selected physical device (GPU) and its properties.
pub struct PhysicalDevice {
    /// The raw Vulkan physical device handle
    handle: vk::PhysicalDevice,
    /// Cached device properties
    properties: vk::PhysicalDeviceProperties,
    /// The queue family index that supports graphics operations
    graphics_queue_family: u32,
}

impl PhysicalDevice {
    /// Selects the best available physical device for rendering.
    ///
    /// Selection criteria:
    /// 1. Must have a queue family that supports graphics operations
    /// 2. Prefers discrete GPUs over integrated
    /// 3. Falls back to any suitable device if no discrete GPU is found
    pub fn select(instance: &ash::Instance) -> anyhow::Result<Self> {
        // SAFETY: Instance is valid and was created successfully
        let physical_devices = unsafe { instance.enumerate_physical_devices() }
            .context("Failed to enumerate physical devices")?;

        if physical_devices.is_empty() {
            anyhow::bail!("No Vulkan-capable GPUs found");
        }

        info!("Found {} Vulkan-capable device(s)", physical_devices.len());

        // Evaluate each device and collect suitable candidates
        let mut candidates: Vec<(vk::PhysicalDevice, vk::PhysicalDeviceProperties, u32, i32)> =
            Vec::new();

        for &physical_device in &physical_devices {
            // SAFETY: Physical device handle is valid from enumeration
            let properties = unsafe { instance.get_physical_device_properties(physical_device) };
            let device_name = unsafe {
                CStr::from_ptr(properties.device_name.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            };

            debug!(
                "Evaluating device: {} (type: {:?}, API version: {}.{}.{})",
                device_name,
                properties.device_type,
                vk::api_version_major(properties.api_version),
                vk::api_version_minor(properties.api_version),
                vk::api_version_patch(properties.api_version),
            );

            // Find a suitable queue family
            let queue_family = match Self::find_graphics_queue_family(instance, physical_device) {
                Some(index) => index,
                None => {
                    debug!("  Skipping: no graphics queue family found");
                    continue;
                }
            };

            // Score the device (higher is better)
            let score = Self::score_device(&properties);
            debug!(
                "  Score: {}, Graphics queue family: {}",
                score, queue_family
            );

            candidates.push((physical_device, properties, queue_family, score));
        }

        if candidates.is_empty() {
            anyhow::bail!("No suitable GPU found (need graphics queue support)");
        }

        // Select the device with the highest score
        candidates.sort_by(|a, b| b.3.cmp(&a.3));
        let (handle, properties, graphics_queue_family, _score) = candidates.remove(0);

        let device_name = unsafe {
            CStr::from_ptr(properties.device_name.as_ptr())
                .to_string_lossy()
                .into_owned()
        };

        info!(
            "Selected GPU: {} (type: {:?})",
            device_name, properties.device_type
        );

        Ok(Self {
            handle,
            properties,
            graphics_queue_family,
        })
    }

    /// Finds a queue family index that supports graphics operations.
    fn find_graphics_queue_family(
        instance: &ash::Instance,
        physical_device: vk::PhysicalDevice,
    ) -> Option<u32> {
        // SAFETY: Physical device handle is valid
        let queue_families =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };

        for (index, queue_family) in queue_families.iter().enumerate() {
            // Check for graphics support
            if queue_family.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                debug!(
                    "  Queue family {}: {:?} ({} queues)",
                    index, queue_family.queue_flags, queue_family.queue_count
                );
                return Some(index as u32);
            }
        }

        None
    }

    /// Scores a physical device based on its properties.
    /// Higher scores are better.
    fn score_device(properties: &vk::PhysicalDeviceProperties) -> i32 {
        let mut score = 0;

        // Strongly prefer discrete GPUs
        match properties.device_type {
            vk::PhysicalDeviceType::DISCRETE_GPU => score += 1000,
            vk::PhysicalDeviceType::INTEGRATED_GPU => score += 100,
            vk::PhysicalDeviceType::VIRTUAL_GPU => score += 50,
            vk::PhysicalDeviceType::CPU => score += 10,
            _ => score += 1,
        }

        // Bonus for higher API version support
        let api_version = properties.api_version;
        score += (vk::api_version_major(api_version) * 10) as i32;
        score += vk::api_version_minor(api_version) as i32;

        score
    }

    /// Returns the raw Vulkan physical device handle.
    pub fn handle(&self) -> vk::PhysicalDevice {
        self.handle
    }

    /// Returns the device properties.
    pub fn properties(&self) -> &vk::PhysicalDeviceProperties {
        &self.properties
    }

    /// Returns the device name as a string.
    pub fn name(&self) -> String {
        unsafe {
            CStr::from_ptr(self.properties.device_name.as_ptr())
                .to_string_lossy()
                .into_owned()
        }
    }

    /// Returns the device type.
    pub fn device_type(&self) -> vk::PhysicalDeviceType {
        self.properties.device_type
    }

    /// Returns the graphics queue family index.
    pub fn graphics_queue_family(&self) -> u32 {
        self.graphics_queue_family
    }

    /// Checks if the device supports a specific extension.
    pub fn supports_extension(
        &self,
        instance: &ash::Instance,
        extension_name: &CStr,
    ) -> anyhow::Result<bool> {
        // SAFETY: Physical device and instance are valid
        let extensions = unsafe { instance.enumerate_device_extension_properties(self.handle) }
            .context("Failed to enumerate device extensions")?;

        Ok(extensions.iter().any(|ext| {
            ext.extension_name_as_c_str()
                .map(|name| name == extension_name)
                .unwrap_or(false)
        }))
    }

    /// Lists all available device extensions (useful for debugging).
    pub fn list_extensions(&self, instance: &ash::Instance) -> anyhow::Result<Vec<String>> {
        // SAFETY: Physical device and instance are valid
        let extensions = unsafe { instance.enumerate_device_extension_properties(self.handle) }
            .context("Failed to enumerate device extensions")?;

        Ok(extensions
            .iter()
            .filter_map(|ext| {
                ext.extension_name_as_c_str()
                    .ok()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .collect())
    }
}
