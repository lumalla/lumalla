use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, mpsc};

use anyhow::Context;
use log::{error, info};
use lumalla_shared::{
    Comms, DisplayMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner, RendererMessage,
    SeatMessage,
};
use mio::{Events, Interest, Poll, Token, unix::SourceFd};

// Use the libseat crate wrapper when the feature is enabled,
// otherwise use the custom FFI bindings
#[cfg(feature = "use-libseat-crate")]
mod libseat_crate;
#[cfg(feature = "use-libseat-crate")]
use crate::libseat_crate::LibSeat;

#[cfg(not(feature = "use-libseat-crate"))]
mod libseat;
#[cfg(not(feature = "use-libseat-crate"))]
use crate::libseat::LibSeat;

const SEAT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);

pub struct SeatState {
    comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<SeatMessage>,
    shutting_down: bool,
    seat: LibSeat,
    seat_enabled: bool,
}

impl SeatState {
    fn handle_message(&mut self, message: SeatMessage) -> anyhow::Result<()> {
        match message {
            SeatMessage::Shutdown => {
                self.shutting_down = true;
            }
            SeatMessage::SeatEnabled => {
                self.seat_enabled = true;
            }
            SeatMessage::SeatDisabled => {
                self.seat_enabled = false;
            }
            SeatMessage::OpenDevice { path } => {
                self.handle_open_device(path)?;
            }
        }

        Ok(())
    }

    fn handle_open_device(&mut self, path: std::path::PathBuf) -> anyhow::Result<()> {
        let path_str = path.to_str().context("Device path is not valid UTF-8")?;
        let c_path = CString::new(path_str).context("Device path contains null byte")?;

        let raw_fd = self.seat.open_device(&c_path)?;

        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        self.comms
            .renderer(RendererMessage::FileOpenedInSession { path, fd });

        Ok(())
    }
}

impl MessageRunner for SeatState {
    type Message = SeatMessage;

    fn new(
        comms: Comms,
        event_loop: Poll,
        channel: mpsc::Receiver<Self::Message>,
        _args: Arc<GlobalArgs>,
    ) -> anyhow::Result<Self> {
        // mut needed for libseat crate feature, not for custom FFI
        #[allow(unused_mut)]
        let mut seat = LibSeat::new(comms.clone()).context("Failed to create seat")?;

        let seat_fd = seat.fd().context("Failed to get seat fd")?;
        let mut seat_source = SourceFd(&seat_fd);
        event_loop
            .registry()
            .register(&mut seat_source, SEAT_TOKEN, Interest::READABLE)?;
        let seat_name = seat.seat_name().context("Failed to get seat name")?;
        comms.display(DisplayMessage::ActivateSeat(seat_name.clone()));
        comms.renderer(RendererMessage::SeatSessionCreated { seat_name });

        Ok(Self {
            comms,
            event_loop,
            channel,
            shutting_down: false,
            seat,
            seat_enabled: false,
        })
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let mut events = Events::with_capacity(128);
        loop {
            let poll_timeout = Some(std::time::Duration::from_millis(100));
            if let Err(err) = self.event_loop.poll(&mut events, poll_timeout) {
                error!("Unable to poll event loop: {err}");
            }

            for event in events.iter() {
                match event.token() {
                    MESSAGE_CHANNEL_TOKEN => {
                        while let Ok(msg) = self.channel.try_recv() {
                            if let Err(err) = self.handle_message(msg) {
                                error!("Unable to handle message: {err}");
                            }
                        }
                    }
                    SEAT_TOKEN => {
                        if let Err(err) = self.seat.dispatch() {
                            error!("Failed to dispatch seat events: {err}");
                        }
                    }
                    _ => {}
                }
            }

            if self.shutting_down {
                break;
            }
        }

        Ok(())
    }
}
