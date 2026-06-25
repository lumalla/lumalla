use std::ffi::CString;
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::path::Path;

use anyhow::Context;
use lumalla_shared::Comms;

use crate::libseat::LibSeat;

mod libseat;

pub struct SeatState {
    seat: LibSeat,
    seat_enabled: bool,
}

impl SeatState {
    pub fn new(comms: Comms) -> anyhow::Result<Self> {
        let seat = LibSeat::new(comms).context("Failed to create seat")?;
        Ok(Self {
            seat,
            seat_enabled: false,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.seat.fd()
    }

    pub fn dispatch(&mut self) -> anyhow::Result<()> {
        self.seat
            .dispatch()
            .context("Failed to dispatch libseat events")
    }

    pub fn seat_name(&self) -> anyhow::Result<String> {
        self.seat.seat_name()
    }

    pub fn is_enabled(&self) -> bool {
        self.seat_enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.seat_enabled = enabled;
    }

    /// Open the device from the given path
    pub fn open_device(&mut self, path: &Path) -> anyhow::Result<OwnedFd> {
        let path_str = path.to_str().context("Device path is not valid UTF-8")?;
        let c_path = CString::new(path_str).context("Device path contains null byte")?;
        Ok(self.seat.open_device(&c_path)?.into_fd())
    }
}

impl mio::event::Source for SeatState {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        self.seat.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        self.seat.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        self.seat.deregister(registry)
    }
}
