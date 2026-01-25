//! Synchronization primitives (fences and semaphores)

use anyhow::Context;
use ash::vk;
use log::debug;

use super::Device;

/// A Vulkan fence for CPU-GPU synchronization.
///
/// Fences are used to synchronize the CPU with GPU operations.
/// The CPU can wait for a fence to be signaled by the GPU.
pub struct Fence {
    /// The Vulkan fence handle
    handle: vk::Fence,
    /// The device that owns this fence
    device: ash::Device,
}

impl Fence {
    /// Creates a new fence.
    ///
    /// If `signaled` is true, the fence starts in the signaled state,
    /// which is useful for the first frame where there's no prior work to wait on.
    pub fn new(device: &Device, signaled: bool) -> anyhow::Result<Self> {
        let flags = if signaled {
            vk::FenceCreateFlags::SIGNALED
        } else {
            vk::FenceCreateFlags::empty()
        };

        let create_info = vk::FenceCreateInfo::default().flags(flags);

        let handle = unsafe { device.handle().create_fence(&create_info, None) }
            .context("Failed to create fence")?;

        debug!("Created fence (signaled: {})", signaled);

        Ok(Self {
            handle,
            device: device.handle().clone(),
        })
    }

    /// Waits for the fence to be signaled.
    ///
    /// This blocks the CPU until the GPU signals the fence.
    pub fn wait(&self, timeout_ns: u64) -> anyhow::Result<()> {
        unsafe {
            self.device
                .wait_for_fences(&[self.handle], true, timeout_ns)
        }
        .context("Failed to wait for fence")?;
        Ok(())
    }

    /// Waits for the fence with a default timeout of 1 second.
    pub fn wait_default(&self) -> anyhow::Result<()> {
        self.wait(1_000_000_000) // 1 second in nanoseconds
    }

    /// Resets the fence to the unsignaled state.
    pub fn reset(&self) -> anyhow::Result<()> {
        unsafe { self.device.reset_fences(&[self.handle]) }.context("Failed to reset fence")?;
        Ok(())
    }

    /// Returns the fence handle.
    pub fn handle(&self) -> vk::Fence {
        self.handle
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_fence(self.handle, None);
        }
        debug!("Destroyed fence");
    }
}

/// A Vulkan semaphore for GPU-GPU synchronization.
///
/// Semaphores are used to synchronize operations on the GPU,
/// such as ensuring rendering completes before presentation.
pub struct Semaphore {
    /// The Vulkan semaphore handle
    handle: vk::Semaphore,
    /// The device that owns this semaphore
    device: ash::Device,
}

impl Semaphore {
    /// Creates a new semaphore.
    pub fn new(device: &Device) -> anyhow::Result<Self> {
        let create_info = vk::SemaphoreCreateInfo::default();

        let handle = unsafe { device.handle().create_semaphore(&create_info, None) }
            .context("Failed to create semaphore")?;

        debug!("Created semaphore");

        Ok(Self {
            handle,
            device: device.handle().clone(),
        })
    }

    /// Returns the semaphore handle.
    pub fn handle(&self) -> vk::Semaphore {
        self.handle
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_semaphore(self.handle, None);
        }
        debug!("Destroyed semaphore");
    }
}
