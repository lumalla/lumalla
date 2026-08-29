//! Reusable scanout buffers: Vulkan DMA-BUF images, framebuffers, and DRM FBs.

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;

use anyhow::Context;
use ash::vk;

use crate::drm::DrmFramebuffer;
use crate::vulkan::{DmaBufImage, Framebuffer, VulkanContext};

/// Pool capacity per `(drm device, width, height, fourcc)` tuple.
const POOL_MAX_PER_KEY: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ScanoutBufferKey {
    pub drm_path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
}

/// A KMS-ready scanout target with persistent Vulkan and DRM resources.
pub(crate) struct ScanoutBuffer {
    pub key: ScanoutBufferKey,
    pub drm_fb: DrmFramebuffer,
    pub dma_image: DmaBufImage,
    pub framebuffer: Framebuffer,
    /// True until the first GPU upload fills the buffer.
    pub fresh: bool,
    /// GPU work that must finish before flip or reuse.
    pub gpu_pending: Option<crate::vulkan::PendingGpuSubmit>,
}

impl ScanoutBuffer {
    pub fn drm_fb_id(&self) -> u32 {
        self.drm_fb.id()
    }
}

pub(crate) struct ScanoutBufferPool {
    free: HashMap<ScanoutBufferKey, Vec<ScanoutBuffer>>,
}

impl ScanoutBufferPool {
    pub fn new() -> Self {
        Self {
            free: HashMap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.free.clear();
    }

    pub fn acquire(
        &mut self,
        vulkan: &mut VulkanContext,
        drm_path: &PathBuf,
        drm_fd: BorrowedFd<'_>,
        width: u32,
        height: u32,
        format: vk::Format,
        fourcc: u32,
    ) -> anyhow::Result<ScanoutBuffer> {
        let key = ScanoutBufferKey {
            drm_path: drm_path.clone(),
            width,
            height,
            fourcc,
        };
        if let Some(mut buffer) = self.free.get_mut(&key).and_then(|free| free.pop()) {
            if let Some(pending) = buffer.gpu_pending.take() {
                pending.wait(vulkan.device(), vulkan.graphics_command_pool())?;
            }
            return Ok(buffer);
        }
        create_scanout_buffer(vulkan, drm_path, drm_fd, width, height, format, fourcc)
    }

    pub fn release(&mut self, buffer: ScanoutBuffer) {
        let key = buffer.key.clone();
        let entry = self.free.entry(key).or_default();
        if entry.len() < POOL_MAX_PER_KEY {
            entry.push(buffer);
        }
    }
}

fn create_scanout_buffer(
    vulkan: &mut VulkanContext,
    drm_path: &PathBuf,
    drm_fd: BorrowedFd<'_>,
    width: u32,
    height: u32,
    format: vk::Format,
    fourcc: u32,
) -> anyhow::Result<ScanoutBuffer> {
    let dma_image = DmaBufImage::allocate(
        vulkan.device(),
        vulkan.physical_device(),
        width,
        height,
        format,
    )
    .context("Failed to allocate exportable scanout image")?;

    vulkan.ensure_scanout_render_pass()?;
    let render_pass = vulkan.scanout_render_pass()?;
    let device = vulkan.device();
    let framebuffer =
        Framebuffer::from_view(device, render_pass, dma_image.view(), dma_image.extent())
            .context("Failed to create scanout framebuffer")?;

    let dma_buf = dma_image
        .export_dma_buf()
        .context("Failed to export DMA-BUF for scanout")?;

    let drm_fb = DrmFramebuffer::from_dma_buf(
        drm_fd,
        dma_buf.as_fd(),
        width,
        height,
        dma_image.stride(),
        dma_image.offset(),
        dma_image.modifier(),
        fourcc,
    )
    .context("Failed to import DMA-BUF as DRM framebuffer")?;

    Ok(ScanoutBuffer {
        key: ScanoutBufferKey {
            drm_path: drm_path.clone(),
            width,
            height,
            fourcc,
        },
        drm_fb,
        dma_image,
        framebuffer,
        fresh: true,
        gpu_pending: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_keys_distinguish_drm_devices() {
        let a = ScanoutBufferKey {
            drm_path: PathBuf::from("/dev/dri/card0"),
            width: 1920,
            height: 1080,
            fourcc: 0x34325258,
        };
        let b = ScanoutBufferKey {
            drm_path: PathBuf::from("/dev/dri/card1"),
            width: 1920,
            height: 1080,
            fourcc: 0x34325258,
        };
        assert_ne!(a, b);
    }
}
