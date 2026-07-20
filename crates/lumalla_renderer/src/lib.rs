use std::{io, path::PathBuf};

use lumalla_seat::SeatState;
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

    /// Open missing DRM devices via the seat and acquire DRM master.
    pub fn activate_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        self.drm_devices.activate(seat)
    }

    /// Drop DRM master without closing seat-opened devices.
    pub fn deactivate_drm(&mut self) {
        self.drm_devices.deactivate();
    }

    /// Close removed / open newly discovered DRM devices while the seat is active.
    pub fn reconcile_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        self.drm_devices.reconcile(seat)
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
