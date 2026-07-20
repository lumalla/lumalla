use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::path::Path;

use anyhow::Context;
use log::debug;
use lumalla_shared::Comms;

use crate::libseat::LibSeat;

mod libseat;

pub use libseat::SeatDevice;

pub struct SeatState {
    main_seat: LibSeat,
}

impl SeatState {
    pub fn new(comms: Comms) -> anyhow::Result<Self> {
        let seat = LibSeat::new(comms).context("Failed to create seat")?;
        Ok(Self { main_seat: seat })
    }

    pub fn fd(&self) -> RawFd {
        self.main_seat.fd()
    }

    pub fn dispatch(&mut self) -> anyhow::Result<()> {
        self.main_seat
            .dispatch()
            .context("Failed to dispatch libseat events")
    }

    pub fn seat_name(&self) -> anyhow::Result<String> {
        self.main_seat.seat_name()
    }

    pub fn enable_main_seat(&mut self) {
        self.main_seat.enable();
    }

    pub fn disable_main_seat(&mut self) {
        self.main_seat.disable();
    }

    pub fn is_enabled(&self) -> bool {
        self.main_seat.is_enabled()
    }

    /// Open the device from the given path via libseat.
    pub fn open_device(&self, path: &Path) -> anyhow::Result<SeatDevice> {
        debug!("Opening device in main seat: {}", path.display());
        let path_str = path.to_str().context("Device path is not valid UTF-8")?;
        let c_path = CString::new(path_str).context("Device path contains null byte")?;
        self.main_seat.open_device(&c_path)
    }

    /// Close a device previously opened with [`Self::open_device`].
    pub fn close_device(&self, device: SeatDevice) -> anyhow::Result<()> {
        debug!(
            "Closing device in main seat: device_id={}",
            device.device_id()
        );
        self.main_seat.close_device(device)
    }
}

impl mio::event::Source for SeatState {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        self.main_seat.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        self.main_seat.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        self.main_seat.deregister(registry)
    }
}
