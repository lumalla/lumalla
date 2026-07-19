//! DRM/KMS display backend
//!
//! Device discovery uses libdrm. Scanout buffers are allocated in Vulkan and
//! exported as DMA-BUFs for KMS later.

mod device;

pub use device::{DrmDevice, find_drm_devices};
