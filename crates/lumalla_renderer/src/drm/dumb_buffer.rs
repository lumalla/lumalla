//! DRM dumb buffers for simple CPU rendering
//!
//! Dumb buffers are simple CPU-writable framebuffers, useful for testing
//! the display pipeline without GPU rendering.

use anyhow::Context;
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{Device as ControlDevice, dumbbuffer, framebuffer};
use log::{debug, info};

use super::DrmDevice;

/// A simple CPU-writable framebuffer.
pub struct DumbBuffer {
    /// Buffer handle
    handle: dumbbuffer::DumbBuffer,
    /// Framebuffer handle
    fb: framebuffer::Handle,
    /// Width in pixels
    width: u32,
    /// Height in pixels
    height: u32,
    /// Stride (bytes per row)
    stride: u32,
}

impl DumbBuffer {
    /// Creates a new dumb buffer with the given dimensions.
    pub fn new(device: &DrmDevice, width: u32, height: u32) -> anyhow::Result<Self> {
        // Create the dumb buffer (XRGB8888 format, 32 bits per pixel)
        let handle = device
            .create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)
            .context("Failed to create dumb buffer")?;

        let stride = handle.pitch();

        // Create a framebuffer from the dumb buffer
        let fb = device
            .add_framebuffer(&handle, 24, 32)
            .context("Failed to create framebuffer from dumb buffer")?;

        debug!(
            "Created dumb buffer: {}x{}, stride={}",
            width, height, stride
        );

        Ok(Self {
            handle,
            fb,
            width,
            height,
            stride,
        })
    }

    /// Returns the framebuffer handle for scanout.
    pub fn framebuffer(&self) -> framebuffer::Handle {
        self.fb
    }

    /// Returns the buffer dimensions.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Fills the entire buffer with a solid color (XRGB format).
    pub fn fill(&mut self, device: &DrmDevice, r: u8, g: u8, b: u8) -> anyhow::Result<()> {
        let pixel = u32::from_ne_bytes([b, g, r, 0xFF]);
        self.fill_raw(device, pixel)
    }

    /// Fills the entire buffer with a raw 32-bit pixel value.
    pub fn fill_raw(&mut self, device: &DrmDevice, pixel: u32) -> anyhow::Result<()> {
        let mut map = device
            .map_dumb_buffer(&mut self.handle)
            .context("Failed to map dumb buffer")?;

        let ptr = map.as_mut_ptr() as *mut u32;
        let count = (self.stride / 4) * self.height;

        // SAFETY: We have exclusive access and the buffer is large enough
        unsafe {
            for i in 0..count as usize {
                ptr.add(i).write(pixel);
            }
        }

        Ok(())
    }

    /// Draws a simple gradient pattern for testing.
    pub fn draw_gradient(&mut self, device: &DrmDevice) -> anyhow::Result<()> {
        let mut map = device
            .map_dumb_buffer(&mut self.handle)
            .context("Failed to map dumb buffer")?;

        let ptr = map.as_mut_ptr();

        for y in 0..self.height {
            for x in 0..self.width {
                let offset = (y * self.stride + x * 4) as usize;

                // Simple gradient: red increases left-to-right, blue increases top-to-bottom
                let r = ((x * 255) / self.width.max(1)) as u8;
                let g = 0u8;
                let b = ((y * 255) / self.height.max(1)) as u8;

                // XRGB8888 format: [B, G, R, X]
                unsafe {
                    *ptr.add(offset) = b;
                    *ptr.add(offset + 1) = g;
                    *ptr.add(offset + 2) = r;
                    *ptr.add(offset + 3) = 0xFF;
                }
            }
        }

        Ok(())
    }

    /// Draws a checkerboard pattern for testing.
    pub fn draw_checkerboard(
        &mut self,
        device: &DrmDevice,
        tile_size: u32,
        color1: (u8, u8, u8),
        color2: (u8, u8, u8),
    ) -> anyhow::Result<()> {
        let mut map = device
            .map_dumb_buffer(&mut self.handle)
            .context("Failed to map dumb buffer")?;

        let ptr = map.as_mut_ptr();

        for y in 0..self.height {
            for x in 0..self.width {
                let offset = (y * self.stride + x * 4) as usize;

                let tile_x = x / tile_size.max(1);
                let tile_y = y / tile_size.max(1);
                let is_odd = (tile_x + tile_y) % 2 == 1;

                let (r, g, b) = if is_odd { color1 } else { color2 };

                // XRGB8888 format: [B, G, R, X]
                unsafe {
                    *ptr.add(offset) = b;
                    *ptr.add(offset + 1) = g;
                    *ptr.add(offset + 2) = r;
                    *ptr.add(offset + 3) = 0xFF;
                }
            }
        }

        Ok(())
    }

    /// Draws a simple test pattern with colored bars.
    pub fn draw_color_bars(&mut self, device: &DrmDevice) -> anyhow::Result<()> {
        let mut map = device
            .map_dumb_buffer(&mut self.handle)
            .context("Failed to map dumb buffer")?;

        let ptr = map.as_mut_ptr();
        let bar_width = self.width / 8;

        // Standard color bar pattern
        let colors: [(u8, u8, u8); 8] = [
            (255, 255, 255), // White
            (255, 255, 0),   // Yellow
            (0, 255, 255),   // Cyan
            (0, 255, 0),     // Green
            (255, 0, 255),   // Magenta
            (255, 0, 0),     // Red
            (0, 0, 255),     // Blue
            (0, 0, 0),       // Black
        ];

        for y in 0..self.height {
            for x in 0..self.width {
                let offset = (y * self.stride + x * 4) as usize;
                let bar_index = (x / bar_width.max(1)).min(7) as usize;
                let (r, g, b) = colors[bar_index];

                // XRGB8888 format: [B, G, R, X]
                unsafe {
                    *ptr.add(offset) = b;
                    *ptr.add(offset + 1) = g;
                    *ptr.add(offset + 2) = r;
                    *ptr.add(offset + 3) = 0xFF;
                }
            }
        }

        Ok(())
    }
}

impl Drop for DumbBuffer {
    fn drop(&mut self) {
        debug!("Dropped dumb buffer");
    }
}

/// Creates a set of dumb buffers for double buffering.
pub fn create_double_buffer(
    device: &DrmDevice,
    width: u32,
    height: u32,
) -> anyhow::Result<[DumbBuffer; 2]> {
    let buf1 = DumbBuffer::new(device, width, height)?;
    let buf2 = DumbBuffer::new(device, width, height)?;

    info!("Created double buffer: {}x{}", width, height);

    Ok([buf1, buf2])
}
