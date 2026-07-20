//! DRM/KMS display backend
//!
//! Device discovery uses libdrm, with udev for hotplug. Scanout buffers are
//! allocated in Vulkan and exported as DMA-BUFs for KMS later.

mod device;

pub use device::{DrmDevice, DrmDevices, find_drm_devices};
