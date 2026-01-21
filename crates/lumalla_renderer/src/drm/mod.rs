//! DRM/KMS display backend
//!
//! This module provides direct display output using Linux DRM (Direct Rendering Manager)
//! with atomic modesetting and GBM (Generic Buffer Management) for buffer allocation.

mod device;
mod dumb_buffer;
mod gbm;
mod output;

pub use device::{find_drm_devices, DrmDevice};
pub use dumb_buffer::DumbBuffer;
pub use gbm::{GbmAllocator, GbmBuffer};
pub use output::{Connector, Crtc, Output, OutputManager, Plane};
