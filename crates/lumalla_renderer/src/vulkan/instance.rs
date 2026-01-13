//! Vulkan instance creation and management

use std::ffi::{CStr, CString};

use anyhow::Context;
use ash::vk;
use log::{debug, info, warn};

use super::{CommandPool, Device, MemoryAllocator, PhysicalDevice};

/// Holds the core Vulkan objects needed for rendering.
///
/// This struct manages the Vulkan entry point (loader) and instance,
/// along with optional debug utilities for development.
pub struct VulkanContext {
    /// The Vulkan function loader
    entry: ash::Entry,
    /// The Vulkan instance
    instance: ash::Instance,
    /// The selected physical device (GPU)
    physical_device: PhysicalDevice,
    /// The logical device (must be dropped before instance)
    device: Option<Device>,
    /// Command pool for graphics operations (must be destroyed before device)
    graphics_command_pool: Option<CommandPool>,
    /// Memory allocator (must be destroyed before device)
    memory_allocator: Option<MemoryAllocator>,
    /// Debug messenger (only present in debug builds with validation layers)
    #[cfg(debug_assertions)]
    debug_utils: Option<DebugUtils>,
}

#[cfg(debug_assertions)]
struct DebugUtils {
    loader: ash::ext::debug_utils::Instance,
    messenger: vk::DebugUtilsMessengerEXT,
}

impl VulkanContext {
    /// Creates a new Vulkan context with an instance configured for a Wayland compositor.
    ///
    /// This sets up:
    /// - Vulkan instance with appropriate extensions
    /// - Debug validation layers (in debug builds)
    pub fn new() -> anyhow::Result<Self> {
        // Load Vulkan dynamically
        let entry =
            unsafe { ash::Entry::load() }.context("Failed to load Vulkan. Is a Vulkan driver installed?")?;

        // Log Vulkan version info
        // SAFETY: The entry point was successfully loaded above
        match unsafe { entry.try_enumerate_instance_version() }? {
            Some(version) => {
                info!(
                    "Vulkan instance version: {}.{}.{}",
                    vk::api_version_major(version),
                    vk::api_version_minor(version),
                    vk::api_version_patch(version)
                );
            }
            None => {
                info!("Vulkan instance version: 1.0");
            }
        }

        // Query available extensions
        // SAFETY: The entry point was successfully loaded above
        let available_extensions =
            unsafe { entry.enumerate_instance_extension_properties(None) }?;
        let available_extension_names: Vec<&CStr> = available_extensions
            .iter()
            .map(|ext| ext.extension_name_as_c_str().unwrap_or(c""))
            .collect();

        debug!(
            "Available Vulkan instance extensions: {:?}",
            available_extension_names
        );

        // Determine which extensions to enable
        let mut extensions_to_enable: Vec<&CStr> = Vec::new();

        // Surface extensions for display output
        let desired_extensions: &[&CStr] = &[
            ash::khr::surface::NAME,
            ash::khr::display::NAME,
            #[cfg(debug_assertions)]
            ash::ext::debug_utils::NAME,
        ];

        for &ext in desired_extensions {
            if available_extension_names.contains(&ext) {
                extensions_to_enable.push(ext);
                debug!("Enabling Vulkan extension: {:?}", ext);
            } else {
                warn!("Vulkan extension not available: {:?}", ext);
            }
        }

        let extensions_ptrs: Vec<*const i8> = extensions_to_enable
            .iter()
            .map(|ext| ext.as_ptr())
            .collect();

        // Query available layers
        // SAFETY: The entry point was successfully loaded above
        let available_layers = unsafe { entry.enumerate_instance_layer_properties() }?;
        let available_layer_names: Vec<&CStr> = available_layers
            .iter()
            .map(|layer| layer.layer_name_as_c_str().unwrap_or(c""))
            .collect();

        debug!("Available Vulkan layers: {:?}", available_layer_names);

        // Enable validation layers in debug builds
        let mut layers_to_enable: Vec<&CStr> = Vec::new();

        #[cfg(debug_assertions)]
        {
            let validation_layer = c"VK_LAYER_KHRONOS_validation";
            if available_layer_names.contains(&validation_layer) {
                layers_to_enable.push(validation_layer);
                info!("Enabling Vulkan validation layers");
            } else {
                warn!("Vulkan validation layers not available");
            }
        }

        let layers_ptrs: Vec<*const i8> =
            layers_to_enable.iter().map(|layer| layer.as_ptr()).collect();

        // Application info
        let app_name = CString::new("lumalla").unwrap();
        let engine_name = CString::new("lumalla").unwrap();

        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(&engine_name)
            .engine_version(vk::make_api_version(0, 0, 1, 0))
            .api_version(vk::API_VERSION_1_2);

        // Create instance
        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&extensions_ptrs)
            .enabled_layer_names(&layers_ptrs);

        let instance = unsafe { entry.create_instance(&create_info, None) }
            .context("Failed to create Vulkan instance")?;

        info!("Vulkan instance created successfully");

        // Set up debug messenger in debug builds
        #[cfg(debug_assertions)]
        let debug_utils = Self::setup_debug_messenger(&entry, &instance);

        // Select a physical device
        let physical_device = PhysicalDevice::select(&instance)?;

        // Create the logical device
        let device = Device::new(&instance, &physical_device)?;

        // Create command pool for graphics operations
        let graphics_command_pool = CommandPool::new_graphics(&device)?;

        // Create memory allocator
        let memory_allocator = MemoryAllocator::new(&instance, &device, physical_device.handle())?;

        Ok(Self {
            entry,
            instance,
            physical_device,
            device: Some(device),
            graphics_command_pool: Some(graphics_command_pool),
            memory_allocator: Some(memory_allocator),
            #[cfg(debug_assertions)]
            debug_utils,
        })
    }

    /// Sets up the Vulkan debug messenger for validation layer output.
    #[cfg(debug_assertions)]
    fn setup_debug_messenger(entry: &ash::Entry, instance: &ash::Instance) -> Option<DebugUtils> {
        let debug_utils_loader = ash::ext::debug_utils::Instance::new(entry, instance);

        let messenger_create_info = vk::DebugUtilsMessengerCreateInfoEXT::default()
            .message_severity(
                vk::DebugUtilsMessageSeverityFlagsEXT::ERROR
                    | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                    | vk::DebugUtilsMessageSeverityFlagsEXT::INFO,
            )
            .message_type(
                vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                    | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                    | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
            )
            .pfn_user_callback(Some(vulkan_debug_callback));

        match unsafe { debug_utils_loader.create_debug_utils_messenger(&messenger_create_info, None) }
        {
            Ok(messenger) => {
                debug!("Vulkan debug messenger created");
                Some(DebugUtils {
                    loader: debug_utils_loader,
                    messenger,
                })
            }
            Err(e) => {
                warn!("Failed to create Vulkan debug messenger: {:?}", e);
                None
            }
        }
    }

    /// Returns a reference to the Vulkan instance.
    pub fn instance(&self) -> &ash::Instance {
        &self.instance
    }

    /// Returns a reference to the selected physical device.
    pub fn physical_device(&self) -> &PhysicalDevice {
        &self.physical_device
    }

    /// Returns a reference to the logical device.
    pub fn device(&self) -> &Device {
        self.device
            .as_ref()
            .expect("Device should always be present while VulkanContext is alive")
    }

    /// Returns a reference to the graphics command pool.
    pub fn graphics_command_pool(&self) -> &CommandPool {
        self.graphics_command_pool
            .as_ref()
            .expect("Command pool should always be present while VulkanContext is alive")
    }

    /// Returns a mutable reference to the memory allocator.
    pub fn memory_allocator_mut(&mut self) -> &mut MemoryAllocator {
        self.memory_allocator
            .as_mut()
            .expect("Memory allocator should always be present while VulkanContext is alive")
    }

    /// Returns a reference to the Vulkan entry (function loader).
    pub fn entry(&self) -> &ash::Entry {
        &self.entry
    }
}

impl Drop for VulkanContext {
    fn drop(&mut self) {
        // Command pool must be destroyed before device
        if let (Some(command_pool), Some(device)) =
            (&mut self.graphics_command_pool, &self.device)
        {
            command_pool.destroy(device);
        }
        self.graphics_command_pool = None;

        // Memory allocator must be destroyed before device
        // (gpu-allocator handles cleanup internally, but we drop it explicitly)
        drop(self.memory_allocator.take());

        // Device must be destroyed before instance
        drop(self.device.take());

        unsafe {
            #[cfg(debug_assertions)]
            if let Some(ref debug_utils) = self.debug_utils {
                debug_utils
                    .loader
                    .destroy_debug_utils_messenger(debug_utils.messenger, None);
            }

            self.instance.destroy_instance(None);
        }
        info!("Vulkan instance destroyed");
    }
}

/// Debug callback for Vulkan validation layers.
#[cfg(debug_assertions)]
unsafe extern "system" fn vulkan_debug_callback(
    message_severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    message_type: vk::DebugUtilsMessageTypeFlagsEXT,
    p_callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT,
    _user_data: *mut std::ffi::c_void,
) -> vk::Bool32 {
    let callback_data = unsafe { &*p_callback_data };

    let message = if callback_data.p_message.is_null() {
        std::borrow::Cow::Borrowed("(no message)")
    } else {
        unsafe { CStr::from_ptr(callback_data.p_message) }.to_string_lossy()
    };

    let type_str = match message_type {
        vk::DebugUtilsMessageTypeFlagsEXT::GENERAL => "General",
        vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION => "Validation",
        vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE => "Performance",
        _ => "Unknown",
    };

    match message_severity {
        vk::DebugUtilsMessageSeverityFlagsEXT::ERROR => {
            log::error!("[Vulkan {}] {}", type_str, message);
        }
        vk::DebugUtilsMessageSeverityFlagsEXT::WARNING => {
            log::warn!("[Vulkan {}] {}", type_str, message);
        }
        vk::DebugUtilsMessageSeverityFlagsEXT::INFO => {
            log::info!("[Vulkan {}] {}", type_str, message);
        }
        _ => {
            log::debug!("[Vulkan {}] {}", type_str, message);
        }
    }

    vk::FALSE
}
