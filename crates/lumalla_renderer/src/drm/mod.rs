//! DRM/KMS display backend
//!
//! Device discovery uses libdrm, with udev for hotplug. Scanout buffers are
//! allocated in Vulkan and exported as DMA-BUFs for KMS.

mod connector;
mod device;
mod modeset;
mod sys;

pub use device::{DrmDevice, DrmDevices, DrmDispatchResult, find_drm_devices};
pub use modeset::{
    ConnectedOutput, DrmFramebuffer, ModeBlob, ModeInfo, atomic_modeset, atomic_page_flip,
    dispatch_drm_events, enable_atomic_client_caps, find_first_connected_output,
};
