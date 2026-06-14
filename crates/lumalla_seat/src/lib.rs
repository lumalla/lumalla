use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use anyhow::Context;
use log::{debug, error, info};
use lumalla_shared::{
    Comms, DisplayMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner, RendererMessage,
    SeatMessage,
};
use mio::event::Source;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Registry, Token};

use crate::libseat::{LibSeat, SeatDevice};

mod libseat;

pub struct SeatState {
    shutting_down: bool,
    seat: LibSeat,
    seat_enabled: bool,
}

impl SeatState {
    pub fn new(comms: Comms) -> anyhow::Result<Self> {
        let seat = LibSeat::new(comms.clone()).context("Failed to create seat")?;
        Ok(Self {
            shutting_down: false,
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

    fn handle_message(&mut self, message: SeatMessage) -> anyhow::Result<()> {
        match message {
            SeatMessage::Shutdown => {
                self.shutting_down = true;
            }
            SeatMessage::SeatEnabled => {
                info!("Seat enabled");
                self.seat_enabled = true;

                let seat_name = self.seat.seat_name().context("Failed to get seat name")?;
                self.comms
                    .display(DisplayMessage::ActivateSeat(seat_name.clone()));
                self.comms
                    .renderer(RendererMessage::SeatSessionCreated { seat_name });
            }
            SeatMessage::SeatDisabled => {
                info!("Seat disabled");
                self.seat_enabled = false;
            }
            SeatMessage::OpenDevice { path } => {
                self.open_device(path)?;
            }
        }

        Ok(())
    }

    /// Open the device from the given path
    pub fn open_device(&mut self, path: &Path) -> anyhow::Result<SeatDevice> {
        let path_str = path.to_str().context("Device path is not valid UTF-8")?;
        let c_path = CString::new(path_str).context("Device path contains null byte")?;
        self.seat.open_device(&c_path)
    }
}

impl Source for SeatState {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.seat.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.seat.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        self.seat.deregister(registry)
    }
}
