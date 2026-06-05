use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, mpsc};

use anyhow::Context;
use log::{debug, error, info};
use lumalla_shared::{
    Comms, DisplayMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner, RendererMessage,
    SeatMessage,
};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};

use crate::libseat::LibSeat;

mod libseat;

pub const LIBSEAT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);

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

    fn open_device(&mut self, path: std::path::PathBuf) -> anyhow::Result<()> {
        debug!(
            "handle_open_device called for {:?}, seat_enabled={}",
            path, self.seat_enabled
        );

        let path_str = path.to_str().context("Device path is not valid UTF-8")?;
        let c_path = CString::new(path_str).context("Device path contains null byte")?;

        debug!("Calling seat.open_device()");
        let (device_id, raw_fd) = self.seat.open_device(&c_path)?;
        debug!(
            "seat.open_device() returned device_id={}, fd={}",
            device_id, raw_fd
        );

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
        let seat = LibSeat::new(comms.clone()).context("Failed to create seat")?;
        let seat_fd = seat.fd().context("Failed to get seat fd")?;
        event_loop
            .registry()
            .register(&mut SourceFd(&seat_fd), LIBSEAT_TOKEN, Interest::READABLE)
            .context("Unable to listen on seat fd")?;

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
            if let Err(err) = self.event_loop.poll(&mut events, None) {
                error!("Unable to poll event loop: {err}");
            }

            for event in events.iter() {
                match event.token() {
                    MESSAGE_CHANNEL_TOKEN => {
                        while let Ok(msg) = self.channel.try_recv() {
                            debug!("Processing message: {:?}", msg);
                            if let Err(err) = self.handle_message(msg) {
                                error!("Unable to handle message: {err}");
                            }
                        }
                    }
                    LIBSEAT_TOKEN => {
                        self.seat
                            .dispatch()
                            .context("Failed to dispatch seat events")?;
                    }
                    _ => {
                        error!("Received message for unknown token");
                    }
                }
            }

            if self.shutting_down {
                break;
            }
        }

        Ok(())
    }
}
