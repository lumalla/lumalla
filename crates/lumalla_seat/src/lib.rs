use std::sync::{Arc, mpsc};

use anyhow::Context;
use log::error;
use lumalla_shared::{
    Comms, DisplayMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner, SeatMessage,
};
use mio::{Events, Interest, Poll, Token, unix::SourceFd};

use crate::libseat::LibSeat;

mod libseat;

const SEAT_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);

pub struct SeatState {
    _comms: Comms,
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
        }

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
        let mut seat_source = SourceFd(&seat_fd);
        event_loop
            .registry()
            .register(&mut seat_source, SEAT_TOKEN, Interest::READABLE)?;
        comms.display(DisplayMessage::ActivateSeat(
            seat.seat_name().context("Failed to get seat name")?,
        ));

        Ok(Self {
            _comms: comms,
            event_loop,
            channel,
            shutting_down: false,
            seat,
            seat_enabled: true,
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
                            if let Err(err) = self.handle_message(msg) {
                                error!("Unable to handle message: {err}");
                            }
                        }
                    }
                    SEAT_TOKEN => {
                        // Seat events are ready to be dispatched
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
