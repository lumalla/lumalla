//! Command pool and command buffer management

use anyhow::Context;
use ash::vk;
use log::{debug, info};

use super::Device;

/// Manages a Vulkan command pool and provides command buffer allocation.
///
/// Command pools are used to allocate command buffers, which record
/// GPU commands for later submission to a queue.
pub struct CommandPool {
    /// The Vulkan command pool handle
    handle: vk::CommandPool,
    /// The queue family this pool allocates for
    queue_family: u32,
}

impl CommandPool {
    /// Creates a new command pool for the given queue family.
    ///
    /// The pool is created with the `RESET_COMMAND_BUFFER` flag, allowing
    /// individual command buffers to be reset and re-recorded.
    pub fn new(device: &Device, queue_family: u32) -> anyhow::Result<Self> {
        let create_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);

        let handle = unsafe { device.handle().create_command_pool(&create_info, None) }
            .context("Failed to create command pool")?;

        info!("Created command pool for queue family {}", queue_family);

        Ok(Self {
            handle,
            queue_family,
        })
    }

    /// Creates a new command pool for graphics operations.
    pub fn new_graphics(device: &Device) -> anyhow::Result<Self> {
        Self::new(device, device.graphics_queue_family())
    }

    /// Allocates a single primary command buffer.
    pub fn allocate_command_buffer(&self, device: &Device) -> anyhow::Result<vk::CommandBuffer> {
        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.handle)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);

        let command_buffers = unsafe { device.handle().allocate_command_buffers(&allocate_info) }
            .context("Failed to allocate command buffer")?;

        debug!("Allocated primary command buffer");

        Ok(command_buffers[0])
    }

    /// Allocates multiple primary command buffers.
    pub fn allocate_command_buffers(
        &self,
        device: &Device,
        count: u32,
    ) -> anyhow::Result<Vec<vk::CommandBuffer>> {
        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.handle)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(count);

        let command_buffers = unsafe { device.handle().allocate_command_buffers(&allocate_info) }
            .context("Failed to allocate command buffers")?;

        debug!("Allocated {} primary command buffers", count);

        Ok(command_buffers)
    }

    /// Frees command buffers back to the pool.
    pub fn free_command_buffers(&self, device: &Device, buffers: &[vk::CommandBuffer]) {
        unsafe {
            device.handle().free_command_buffers(self.handle, buffers);
        }
        debug!("Freed {} command buffers", buffers.len());
    }

    /// Resets the entire command pool, recycling all allocated command buffers.
    ///
    /// This is more efficient than resetting individual command buffers
    /// if you need to reset all of them.
    pub fn reset(&self, device: &Device) -> anyhow::Result<()> {
        unsafe {
            device
                .handle()
                .reset_command_pool(self.handle, vk::CommandPoolResetFlags::empty())
        }
        .context("Failed to reset command pool")?;

        debug!("Reset command pool");
        Ok(())
    }

    /// Returns the command pool handle.
    pub fn handle(&self) -> vk::CommandPool {
        self.handle
    }

    /// Returns the queue family this pool allocates for.
    pub fn queue_family(&self) -> u32 {
        self.queue_family
    }

    /// Destroys the command pool.
    ///
    /// This must be called before the device is destroyed.
    /// All command buffers allocated from this pool become invalid.
    pub fn destroy(&mut self, device: &Device) {
        if self.handle != vk::CommandPool::null() {
            info!("Destroying command pool");
            unsafe {
                device.handle().destroy_command_pool(self.handle, None);
            }
            self.handle = vk::CommandPool::null();
        }
    }
}

/// Helper for recording commands into a command buffer.
pub struct CommandBufferRecorder<'a> {
    device: &'a Device,
    command_buffer: vk::CommandBuffer,
}

impl<'a> CommandBufferRecorder<'a> {
    /// Begins recording commands into a command buffer.
    ///
    /// The command buffer is reset before recording begins.
    pub fn begin(
        device: &'a Device,
        command_buffer: vk::CommandBuffer,
        usage: vk::CommandBufferUsageFlags,
    ) -> anyhow::Result<Self> {
        let begin_info = vk::CommandBufferBeginInfo::default().flags(usage);

        unsafe { device.handle().begin_command_buffer(command_buffer, &begin_info) }
            .context("Failed to begin command buffer")?;

        Ok(Self {
            device,
            command_buffer,
        })
    }

    /// Begins recording with one-time submit usage.
    ///
    /// Use this for command buffers that will only be submitted once.
    pub fn begin_one_time(
        device: &'a Device,
        command_buffer: vk::CommandBuffer,
    ) -> anyhow::Result<Self> {
        Self::begin(
            device,
            command_buffer,
            vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
        )
    }

    /// Returns the command buffer being recorded.
    pub fn command_buffer(&self) -> vk::CommandBuffer {
        self.command_buffer
    }

    /// Returns a reference to the device.
    pub fn device(&self) -> &Device {
        self.device
    }

    /// Ends recording and returns the command buffer.
    pub fn end(self) -> anyhow::Result<vk::CommandBuffer> {
        unsafe { self.device.handle().end_command_buffer(self.command_buffer) }
            .context("Failed to end command buffer")?;

        Ok(self.command_buffer)
    }
}
