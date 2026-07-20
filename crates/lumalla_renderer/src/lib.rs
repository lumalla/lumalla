use std::io;

use lumalla_seat::SeatState;
use lumalla_shared::DrmDeviceState;
use mio::{Interest, Registry, Token, event::Source};

pub mod drm;
pub mod vulkan;

use crate::drm::{DrmDevices, DrmDispatchResult};

pub struct RendererState {
    drm_devices: DrmDevices,
}

impl RendererState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            drm_devices: DrmDevices::new()?,
        })
    }

    /// Snapshot of discovered DRM devices and probed connectors.
    pub fn drm_device_states(&self) -> Vec<DrmDeviceState> {
        self.drm_devices.device_states()
    }

    /// Drain pending udev DRM events; update device paths and/or connectors.
    pub fn dispatch(&mut self) -> anyhow::Result<DrmDispatchResult> {
        self.drm_devices.dispatch()
    }

    /// Open missing DRM devices via the seat (fresh open after VT resume).
    pub fn activate_drm(&mut self, seat: &SeatState) -> anyhow::Result<()> {
        self.drm_devices.activate(seat)
    }

    /// Close seat-opened DRM devices after session disable was acknowledged.
    pub fn deactivate_drm(&mut self, seat: &SeatState) {
        self.drm_devices.deactivate(seat);
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
