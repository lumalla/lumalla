use std::path::PathBuf;

pub mod drm;
pub mod vulkan;

use crate::drm::find_drm_devices;

pub struct RendererState {
    drm_devices: Vec<PathBuf>,
}

impl RendererState {
    pub fn new() -> anyhow::Result<Self> {
        let drm_devices = find_drm_devices()?;
        Ok(Self { drm_devices })
    }

    pub fn drm_devices(&self) -> &[PathBuf] {
        &self.drm_devices
    }
}
