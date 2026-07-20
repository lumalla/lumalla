use std::{io, path::PathBuf};

use mio::{Interest, Registry, Token, event::Source};

pub mod drm;
pub mod vulkan;

use crate::drm::DrmDevices;

pub struct RendererState {
    drm_devices: DrmDevices,
}

impl RendererState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            drm_devices: DrmDevices::new()?,
        })
    }

    pub fn drm_devices(&self) -> &[PathBuf] {
        self.drm_devices.paths()
    }

    /// Drain pending udev DRM events and rescan primary nodes.
    ///
    /// Returns `true` if the discovered device list changed.
    pub fn dispatch(&mut self) -> anyhow::Result<bool> {
        self.drm_devices.dispatch()
    }
}

impl Source for RendererState {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.drm_devices.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.drm_devices.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        self.drm_devices.deregister(registry)
    }
}
