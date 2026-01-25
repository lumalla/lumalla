//! Physical device (GPU) selection and management

use std::ffi::CStr;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use anyhow::Context;
use ash::vk;
use log::{debug, info, warn};

/// Represents a selected physical device (GPU) and its properties.
pub struct PhysicalDevice {
    /// The raw Vulkan physical device handle
    handle: vk::PhysicalDevice,
    /// Cached device properties
    properties: vk::PhysicalDeviceProperties,
    /// The queue family index that supports graphics operations
    graphics_queue_family: u32,
    /// The DRM primary device path (e.g., /dev/dri/card0) if available
    drm_primary_device_path: Option<PathBuf>,
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

        // Query DRM device properties if available
        let drm_primary_device_path = Self::query_drm_device_path(instance, handle);
        if let Some(ref path) = drm_primary_device_path {
            info!("DRM primary device for selected GPU: {}", path.display());
        } else {
            warn!("Could not determine DRM device path for selected GPU");
        }

        Ok(Self {
            handle,
            properties,
            graphics_queue_family,
            drm_primary_device_path,
        })
    }

    /// Queries the DRM device path for a physical device using VK_EXT_physical_device_drm.
    fn query_drm_device_path(
        instance: &ash::Instance,
        physical_device: vk::PhysicalDevice,
    ) -> Option<PathBuf> {
        // Query DRM properties using the pNext chain
        let mut drm_properties = vk::PhysicalDeviceDrmPropertiesEXT::default();
        let mut properties2 =
            vk::PhysicalDeviceProperties2::default().push_next(&mut drm_properties);

        // SAFETY: Physical device handle is valid
        unsafe { instance.get_physical_device_properties2(physical_device, &mut properties2) };

        // Check if the device has a primary node (needed for modesetting)
        // has_primary is a VkBool32 (u32), not a Rust bool
        if drm_properties.has_primary == vk::FALSE {
            debug!("Physical device does not have a DRM primary node");
            return None;
        }

        let primary_major = drm_properties.primary_major;
        let primary_minor = drm_properties.primary_minor;

        debug!(
            "DRM primary device: major={}, minor={}",
            primary_major, primary_minor
        );

        // Find the matching /dev/dri/card* device by comparing major/minor numbers
        Self::find_drm_device_by_dev_id(primary_major, primary_minor)
    }

    /// Finds a DRM device path by matching device major/minor numbers.
    fn find_drm_device_by_dev_id(major: i64, minor: i64) -> Option<PathBuf> {
        let dri_path = std::path::Path::new("/dev/dri");

        if !dri_path.exists() {
            return None;
        }

        let entries = match std::fs::read_dir(dri_path) {
            Ok(e) => e,
            Err(_) => return None,
        };

        for entry in entries.flatten() {
            let path = entry.path();

            // Only check card* devices (not renderD*)
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if !name.starts_with("card") {
                    continue;
                }
            } else {
                continue;
            }

            // Get the device's major/minor numbers via stat
            if let Ok(metadata) = std::fs::metadata(&path) {
                let rdev = metadata.rdev();
                // On Linux, major/minor can be extracted from rdev
                // major = (rdev >> 8) & 0xfff, minor = (rdev & 0xff) | ((rdev >> 12) & 0xfff00)
                // But we can use libc::major() and libc::minor() for correctness
                let dev_major = libc::major(rdev) as i64;
                let dev_minor = libc::minor(rdev) as i64;

                if dev_major == major && dev_minor == minor {
                    debug!(
                        "Found DRM device {} matching major={}, minor={}",
                        path.display(),
                        major,
                        minor
                    );
                    return Some(path);
                }
            }
        }

        None
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

    /// Returns the DRM primary device path (e.g., /dev/dri/card0) if available.
    ///
    /// This path can be used to open the DRM device for modesetting.
    pub fn drm_device_path(&self) -> Option<&PathBuf> {
        self.drm_primary_device_path.as_ref()
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
