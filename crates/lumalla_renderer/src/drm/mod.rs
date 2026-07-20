//! DRM/KMS display backend
//!
//! Device discovery uses libdrm, with udev for hotplug. Scanout buffers are
//! allocated in Vulkan and exported as DMA-BUFs for KMS later.

mod connector;
mod device;

pub use device::{DrmDevice, DrmDevices, DrmDispatchResult, find_drm_devices};
